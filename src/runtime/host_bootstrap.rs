//! Headless host bootstrap using the existing configuration and credential owners.
use crate::config::{
    Config,
    manage::{ApplyOptions, ConfigActor, ConfigChange, ConfigTransactions},
};
use anyhow::{Context, Result};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("input_missing: {0}")]
    Missing(&'static str),
    #[error("input_invalid: {0}")]
    Invalid(&'static str),
    #[error("output_exists: token output already exists")]
    OutputExists,
}

pub fn runtime_path(config: &Config) -> Result<PathBuf> {
    Ok(config
        .config_path
        .parent()
        .context("state_directory_missing")?
        .join("runtime.json"))
}

fn private_temp(parent: &Path) -> Result<tempfile::NamedTempFile> {
    anyhow::ensure!(parent.is_dir(), "output_parent_missing");
    let file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Print no credential; the caller may show only this path and owner id.
pub async fn issue_token(config: Config, out: &Path, force: bool) -> Result<String> {
    anyhow::ensure!(
        config.gateway.require_pairing,
        "pairing_disabled: enable pairing before issuing an owner bearer"
    );
    if !force {
        match std::fs::symlink_metadata(out) {
            Ok(_) => return Err(BootstrapError::OutputExists.into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let parent = out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = private_temp(parent)?;
    let owner = crate::security::install_owner::load_or_create(&config.workspace_dir)?;
    let token = zeroize::Zeroizing::new(format!("zc_{}", hex::encode(rand::random::<[u8; 32]>())));
    file.write_all(token.as_bytes())?;
    file.as_file().sync_all()?;
    let hash = crate::security::pairing::hash_token(&token);
    let mut updated = config.clone();
    updated.gateway.paired_tokens.push(hash);
    let tx = ConfigTransactions::new(crate::config::new_shared(config)).deferring_activation();
    let plan = tx.plan(ConfigChange::Structured(Box::new(updated))).await?;
    let revision = plan.planned_revision().map(str::to_owned);
    tx.apply(
        plan,
        ConfigActor::Cli,
        ApplyOptions {
            expected_revision: revision.as_deref(),
            confirm_high_risk: true,
            confirmed_revision: revision.as_deref(),
        },
    )
    .await?;
    if force {
        file.persist(out).map_err(|error| error.error)?;
    } else {
        file.persist_noclobber(out).map_err(|error| {
            if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::Error::from(BootstrapError::OutputExists)
            } else {
                error.error.into()
            }
        })?;
    }
    Ok(owner.to_string())
}

/// Bounded provider-key input shared with `prx init --from-stdin`.
pub fn provider_key(stdin: &mut dyn Read) -> Result<zeroize::Zeroizing<String>> {
    const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;
    let mut secret = zeroize::Zeroizing::new(String::new());
    stdin.take(MAX_CREDENTIAL_BYTES + 1).read_to_string(&mut secret)?;
    if secret.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err(BootstrapError::Invalid("provider key too large").into());
    }
    let trimmed_len = secret.trim_end_matches(['\r', '\n']).len();
    secret.truncate(trimmed_len);
    if secret.trim().is_empty() {
        return Err(BootstrapError::Missing("provider key on stdin").into());
    }
    Ok(secret)
}

/// Whether `provider` names a bring-your-own endpoint (`custom:<url>` or
/// `anthropic-custom:<url>`), which has no catalog identity and so no
/// default-named `cred-<name>` reference.
#[must_use]
pub fn is_endpoint_provider(provider: &str) -> bool {
    use crate::providers::{ProviderId, ProviderNamespace};
    matches!(
        ProviderId::of(provider).namespace(),
        ProviderNamespace::Custom | ProviderNamespace::AnthropicCustom
    )
}

/// Persist only through an approved encrypted store: the provider-reference
/// store for a catalog provider, and the encrypted root connection `api_key`
/// for a bring-your-own endpoint (the connection the resolver releases to the
/// configured default provider; there is no catalog name to build a
/// reference from). Neither path writes the key in plaintext.
pub async fn store_provider_key(config: Config, secret: &str) -> Result<Config> {
    let provider = crate::providers::selection::default_provider(&config).to_owned();
    anyhow::ensure!(config.secrets.encrypt, "credential_encryption_required");
    if is_endpoint_provider(&provider) {
        let mut updated = config.clone();
        updated.api_key = Some(secret.to_owned());
        let tx = ConfigTransactions::new(crate::config::new_shared(config.clone())).deferring_activation();
        let plan = tx
            .plan(ConfigChange::Structured(Box::new(updated)))
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let revision = plan.planned_revision().map(str::to_owned);
        tx.apply(
            plan,
            ConfigActor::Cli,
            ApplyOptions {
                expected_revision: revision.as_deref(),
                confirm_high_risk: true,
                confirmed_revision: revision.as_deref(),
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        return Config::load_from_path(&config.config_path, config.workspace_dir);
    }
    let name = crate::providers::ProviderId::of(&provider)
        .catalog_name()
        .context(BootstrapError::Invalid("provider requires a catalog identity"))?
        .to_owned();
    crate::config::manage::service::ManagementService::deferred(crate::config::new_shared(config.clone()))
        .set_credential(provider, name, secret.to_owned(), ConfigActor::Cli)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Config::load_from_path(&config.config_path, config.workspace_dir)
}

/// The gateway binding owns this file, including when supervised by the daemon.
pub(crate) struct RuntimeFile {
    path: PathBuf,
    started_at: String,
}
impl RuntimeFile {
    pub(crate) fn write(config: &Config, port: u16, owner: &str) -> Result<Self> {
        let path = runtime_path(config)?;
        let parent = path.parent().context("state_directory_missing")?;
        let mut file = private_temp(parent)?;
        let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let api_base = format!("http://127.0.0.1:{port}");
        serde_json::to_writer(
            &mut file,
            &serde_json::json!({"prx_version":env!("CARGO_PKG_VERSION"),"api_base":api_base,"pid":std::process::id(),"owner_id":owner,"started_at":started_at,"capabilities_url":format!("{api_base}/api/capabilities")}),
        )?;
        file.as_file().sync_all()?;
        file.persist(&path).map_err(|error| error.error)?;
        Ok(Self { path, started_at })
    }
}
impl Drop for RuntimeFile {
    fn drop(&mut self) {
        let result = (|| -> Result<()> {
            let mut data = String::new();
            std::fs::File::open(&self.path)?.take(8193).read_to_string(&mut data)?;
            anyhow::ensure!(data.len() <= 8192, "runtime_file_invalid");
            let value: serde_json::Value = serde_json::from_str(&data)?;
            if value.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(std::process::id()))
                && value.get("started_at").and_then(serde_json::Value::as_str) == Some(&self.started_at)
            {
                std::fs::remove_file(&self.path)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error, "runtime discovery cleanup failed");
        }
    }
}
