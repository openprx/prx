//! Operator CLI for inspecting and authorizing exact plugin artifacts.
use super::trust::{PluginMaterial, PluginPolicy};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Reload one trusted plugin in an authenticated, already-running gateway.
    Reload {
        name: String,
        #[arg(long)]
        gateway_url: String,
        #[arg(long)]
        token_from_stdin: bool,
        #[arg(long)]
        yes: bool,
    },
    /// List installed artifact identities and their current trust records (no guest execution).
    List,
    /// Inspect a local bundle without executing it or granting permissions.
    Inspect { source: PathBuf },
    /// Trust exactly the two inspected hashes and explicitly selected permissions.
    Trust {
        source: PathBuf,
        #[arg(long)]
        sha256: String,
        #[arg(long)]
        manifest_sha256: String,
        #[arg(long = "permission")]
        permissions: Vec<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Revoke standing trust; reload or restart running owners to apply it.
    Revoke {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Ask a running gateway to enable one disabled plugin.
    ///
    /// Dry-run does not connect. `--token-from-stdin --yes` posts to that
    /// gateway. This command does not write trust and it does not change
    /// `[plugins] enabled`.
    Enable {
        name: String,
        #[arg(long)]
        gateway_url: String,
        #[arg(long)]
        token_from_stdin: bool,
        #[arg(long)]
        yes: bool,
    },
}

#[allow(clippy::print_stdout)] // This is the operator CLI output boundary.
pub async fn handle(config: &crate::config::Config, command: PluginCommand) -> anyhow::Result<()> {
    let result = match command {
        PluginCommand::Reload {
            name,
            gateway_url,
            token_from_stdin,
            yes,
        } => super::cli_reload::reload(config, &name, &gateway_url, token_from_stdin, yes).await?,
        PluginCommand::Enable {
            name,
            gateway_url,
            token_from_stdin,
            yes,
        } => super::cli_reload::enable(config, &name, &gateway_url, token_from_stdin, yes).await?,
        other => run(config, other)?,
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn run(config: &crate::config::Config, command: PluginCommand) -> anyhow::Result<serde_json::Value> {
    let policy = PluginPolicy::from_config(config)?;
    match command {
        PluginCommand::Reload { .. } => anyhow::bail!("reload requires the asynchronous gateway handler"),
        PluginCommand::Enable { .. } => anyhow::bail!("enable requires the asynchronous handler"),
        PluginCommand::Inspect { source } => inspect(&source, &policy),
        PluginCommand::List => {
            let root = config.workspace_dir.join("plugins");
            let mut paths = Vec::new();
            if root.exists() {
                for entry in std::fs::read_dir(root)? {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        paths.push(entry.path());
                    }
                }
            }
            paths.sort();
            let entries = paths
                .into_iter()
                .map(|path| inspect(&path, &policy))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(serde_json::json!({"enabled":policy.enabled(), "plugins":entries}))
        }
        PluginCommand::Trust {
            source,
            sha256,
            manifest_sha256,
            permissions,
            yes,
        } => {
            let material = PluginMaterial::read(&source)?;
            anyhow::ensure!(material.wasm_bytes.is_some(), "plugin component is missing");
            anyhow::ensure!(
                material.wasm_sha256 == sha256 && material.manifest_sha256 == manifest_sha256,
                "plugin hashes differ from the reviewed artifacts"
            );
            let record = material.record_for(&permissions)?;
            if yes {
                anyhow::ensure!(
                    config.autonomy.level != crate::security::AutonomyLevel::ReadOnly,
                    "read-only mode refuses plugin trust changes"
                );
                policy.record(record.clone())?;
            }
            Ok(serde_json::json!({"applied":yes, "trust":record, "activation":"reload or restart the owning runtime"}))
        }
        PluginCommand::Revoke { name, yes } => {
            let previous = policy.record_for_name(&name)?;
            anyhow::ensure!(previous.is_some(), "plugin has no trust record");
            if yes {
                anyhow::ensure!(
                    config.autonomy.level != crate::security::AutonomyLevel::ReadOnly,
                    "read-only mode refuses plugin trust changes"
                );
                policy.restore(&name, None)?;
            }
            Ok(serde_json::json!({"applied":yes,"name":name,"activation":"reload or restart the owning runtime"}))
        }
    }
}

fn inspect(source: &std::path::Path, policy: &PluginPolicy) -> anyhow::Result<serde_json::Value> {
    let material = PluginMaterial::read(source)?;
    let trust = policy.record_for_name(&material.manifest.plugin.name)?;
    let evaluated = super::trust::evaluate(&policy.records()?, &material);
    Ok(
        serde_json::json!({"name":material.manifest.plugin.name,"sha256":material.wasm_sha256,
        "manifest_sha256":material.manifest_sha256,"required_permissions":material.manifest.permissions.required,
        "optional_permissions":material.manifest.permissions.optional,"trust":trust,"status":format!("{evaluated:?}")}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trust_requires_exact_hashes_explicit_permissions_and_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let source = workspace.join("plugins/prx_fixture");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("plugin.toml"),
            "[plugin]\nname='prx_fixture'\nversion='1'\n[permissions]\nrequired=['log']\n",
        )
        .unwrap();
        std::fs::write(source.join("plugin.wasm"), b"test artifact, never executed").unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace;
        config.config_path = temp.path().join("config.toml");
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        let material = PluginMaterial::read(&source).unwrap();
        let command = |hash: &str, permissions: Vec<String>, yes| PluginCommand::Trust {
            source: source.clone(),
            sha256: hash.into(),
            manifest_sha256: material.manifest_sha256.clone(),
            permissions,
            yes,
        };
        assert!(run(&config, command("wrong", vec!["log".into()], true)).is_err());
        assert!(run(&config, command(&material.wasm_sha256, vec![], true)).is_err());
        let policy = PluginPolicy::from_config(&config).unwrap();
        run(&config, command(&material.wasm_sha256, vec!["log".into()], false)).unwrap();
        assert!(!policy.trust_file().exists());
        run(&config, command(&material.wasm_sha256, vec!["log".into()], true)).unwrap();
        assert!(policy.record_for_name("prx_fixture").unwrap().is_some());
        run(
            &config,
            PluginCommand::Revoke {
                name: "prx_fixture".into(),
                yes: true,
            },
        )
        .unwrap();
        assert!(policy.record_for_name("prx_fixture").unwrap().is_none());
    }

    fn disabled_fixture(root: &std::path::Path, name: &str, wasm: &[u8]) -> std::path::PathBuf {
        let source = root.join("workspace").join(".plugins-disabled").join(name);
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("plugin.toml"),
            format!("[plugin]\nname='{name}'\nversion='1'\nwasm='plugin.wasm'\n[permissions]\nrequired=['log']\n"),
        )
        .unwrap();
        std::fs::write(source.join("plugin.wasm"), wasm).unwrap();
        source
    }

    fn config_for(root: &std::path::Path, enabled: bool, read_only: bool) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.workspace_dir = root.join("workspace");
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config.config_path = root.join("config.toml");
        config.plugins.enabled = enabled;
        config.autonomy.level = if read_only {
            crate::security::AutonomyLevel::ReadOnly
        } else {
            crate::security::AutonomyLevel::Full
        };
        let flag = if enabled { "true" } else { "false" };
        std::fs::write(&config.config_path, format!("[plugins]\nenabled = {flag}\n")).unwrap();
        config
    }

    #[tokio::test]
    async fn enable_dry_run_does_not_move_or_write_trust_even_when_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let disabled = disabled_fixture(temp.path(), "prx_fixture", b"test artifact, never executed");
        let config = config_for(temp.path(), true, true);
        let before = std::fs::read_to_string(&config.config_path).unwrap();
        let value = super::super::cli_reload::enable(&config, "prx_fixture", "http://127.0.0.1:1", false, false)
            .await
            .unwrap();
        assert_eq!(value.get("applied").and_then(serde_json::Value::as_bool), Some(false));
        assert_eq!(value.get("enabled").and_then(serde_json::Value::as_bool), Some(false));
        let activation = value
            .get("activation")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        assert!(!activation.contains("refresh this process"), "{activation}");
        assert!(disabled.join("plugin.toml").exists());
        assert!(!config.workspace_dir.join("plugins/prx_fixture").exists());
        let policy = PluginPolicy::from_config(&config).unwrap();
        assert!(!policy.trust_file().exists());
        assert_eq!(std::fs::read_to_string(&config.config_path).unwrap(), before);
    }

    #[tokio::test]
    async fn enable_yes_refuses_read_only_without_moving() {
        let temp = tempfile::tempdir().unwrap();
        let disabled = disabled_fixture(temp.path(), "prx_fixture", b"test artifact, never executed");
        let config = config_for(temp.path(), true, true);
        let before = std::fs::read_to_string(&config.config_path).unwrap();
        let error = super::super::cli_reload::enable(&config, "prx_fixture", "http://127.0.0.1:1", true, true)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("read-only mode refuses plugin enable"),
            "{error}"
        );
        assert!(disabled.join("plugin.toml").exists());
        assert!(!PluginPolicy::from_config(&config).unwrap().trust_file().exists());
        assert_eq!(std::fs::read_to_string(&config.config_path).unwrap(), before);
    }

    #[tokio::test]
    async fn enable_yes_without_a_token_does_not_flip_plugins_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let disabled = disabled_fixture(temp.path(), "prx_fixture", b"test artifact, never executed");
        let config = config_for(temp.path(), false, false);
        let before = std::fs::read_to_string(&config.config_path).unwrap();
        let error = super::super::cli_reload::enable(&config, "prx_fixture", "http://127.0.0.1:1", false, true)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("token-from-stdin"), "{error}");
        // A plugins-only file is not a full stored config. The bytes are the
        // proof that enable did not rewrite `[plugins] enabled`.
        assert!(before.contains("enabled = false"), "{before}");
        assert_eq!(std::fs::read_to_string(&config.config_path).unwrap(), before);
        assert!(disabled.join("plugin.toml").exists());
        assert!(!config.workspace_dir.join("plugins/prx_fixture").exists());
        assert!(!PluginPolicy::from_config(&config).unwrap().trust_file().exists());
    }

    #[tokio::test]
    async fn enable_yes_posts_the_gateway_route_and_leaves_the_local_plugin_alone() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let temp = tempfile::tempdir().unwrap();
        let disabled = disabled_fixture(temp.path(), "prx_fixture", b"test artifact, never executed");
        let config = config_for(temp.path(), false, false);
        let before = std::fs::read_to_string(&config.config_path).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let byte = stream.read_u8().await.unwrap();
                request.push(byte);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 8192);
            }
            let header = String::from_utf8(request).unwrap();
            let lower = header.to_ascii_lowercase();
            assert!(
                lower.starts_with("post /api/plugins/prx_fixture/enable http/1.1\r\n"),
                "{header}"
            );
            assert!(lower.contains("authorization: bearer prx-test-token\r\n"), "{header}");
            assert!(lower.contains("content-type: application/json"), "{header}");
            let length = lower
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap();
            let mut body = vec![0_u8; length];
            stream.read_exact(&mut body).await.unwrap();
            assert_eq!(body, b"{}");
            let response_body = r#"{"plugin":"prx_fixture","enabled":true,"generation":11}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let url =
            super::super::cli_reload::plugin_endpoint(&format!("http://{address}"), "prx_fixture", "enable").unwrap();
        let value = super::super::cli_reload::submit_enable(
            url,
            crate::config::manage::credentials::SecretString::new("prx-test-token".into()),
        )
        .await
        .unwrap();
        assert_eq!(value.get("applied").and_then(serde_json::Value::as_bool), Some(true));
        assert_eq!(value.get("generation").and_then(serde_json::Value::as_u64), Some(11));
        assert_eq!(value.get("enabled").and_then(serde_json::Value::as_bool), Some(true));
        server.await.unwrap();
        assert!(disabled.join("plugin.toml").exists());
        assert!(!config.workspace_dir.join("plugins/prx_fixture").exists());
        assert!(!PluginPolicy::from_config(&config).unwrap().trust_file().exists());
        assert_eq!(std::fs::read_to_string(&config.config_path).unwrap(), before);
    }
}
