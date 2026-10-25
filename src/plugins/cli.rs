//! Operator CLI for inspecting and authorizing exact plugin artifacts.
use super::trust::{PluginMaterial, PluginPolicy};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
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
}

#[allow(clippy::print_stdout)] // This is the operator CLI output boundary.
pub fn handle(config: &crate::config::Config, command: PluginCommand) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(&run(config, command)?)?);
    Ok(())
}

fn run(config: &crate::config::Config, command: PluginCommand) -> anyhow::Result<serde_json::Value> {
    let policy = PluginPolicy::from_config(config)?;
    match command {
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
}
