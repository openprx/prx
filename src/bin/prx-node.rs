use anyhow::{Context, Result, bail};
use clap::Parser;
use openprx::config::{NodeServerConfig, NodesConfig};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "prx-node")]
#[command(about = "OpenPRX remote node server")]
struct Cli {
    #[arg(long)]
    config: Option<String>,

    #[arg(long, alias = "listen")]
    bind: Option<String>,

    #[arg(long, alias = "bearer-token")]
    token: Option<String>,

    #[arg(long)]
    hmac_secret: Option<String>,

    #[arg(long)]
    sandbox_root: Option<String>,

    #[arg(long)]
    exec_timeout_ms: Option<u64>,

    #[arg(long, value_delimiter = ',')]
    allowed_commands: Vec<String>,

    #[arg(long, value_delimiter = ',', alias = "command_blacklist")]
    blocked_commands: Vec<String>,

    #[arg(long)]
    max_output_bytes: Option<usize>,

    #[arg(long)]
    max_concurrent_tasks: Option<usize>,

    #[arg(long)]
    task_result_ttl_ms: Option<u64>,

    #[arg(long)]
    tls_required: Option<bool>,

    #[arg(long)]
    tls_cert: Option<String>,

    #[arg(long)]
    tls_key: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct NodeConfigFile {
    #[serde(default)]
    nodes: NodesConfig,
    #[serde(default)]
    server: Option<NodeServerConfig>,
}

/// The node server settings of a config file: a standalone node file
/// (`[server]`) or the PRX configuration (`[nodes.server]`). Credentials the
/// PRX configuration stores encrypted (`enc2:`) are decrypted with the key
/// beside that file, as `prx` reads them; plaintext is read as it is.
fn load_config_file(path: &str) -> Result<NodeServerConfig> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("failed reading config file {path}"))?;
    let mut tree: toml::Value = toml::from_str(&raw).with_context(|| format!("failed parsing config file {path}"))?;
    // Only the table this server reads: other credentials of a PRX
    // configuration stay encrypted, and one of them that does not decrypt
    // cannot stop the node server.
    openprx::config::secret_paths::decrypt_file_table(&mut tree, &["nodes", "server"], std::path::Path::new(path))
        .with_context(|| format!("failed decrypting credentials in config file {path}"))?;
    let parsed: NodeConfigFile = tree
        .try_into()
        .with_context(|| format!("failed parsing config file {path}"))?;
    Ok(parsed.server.unwrap_or(parsed.nodes.server))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive(
                "info"
                    .parse()
                    .map_err(|e| anyhow::anyhow!("BUG: static directive parse failed: {e}"))?,
            ),
        )
        .init();

    let cli = Cli::parse();
    let mut cfg = match &cli.config {
        Some(path) => load_config_file(path)?,
        None => NodeServerConfig::default(),
    };

    if let Some(bind) = cli.bind {
        cfg.listen_addr = bind;
    }
    if let Some(token) = cli.token {
        cfg.bearer_token = token;
    }
    if let Some(secret) = cli.hmac_secret {
        cfg.hmac_secret = Some(secret);
    }
    if let Some(root) = cli.sandbox_root {
        cfg.sandbox_root = root;
    }
    if let Some(timeout_ms) = cli.exec_timeout_ms {
        cfg.exec_timeout_ms = timeout_ms;
    }
    if !cli.allowed_commands.is_empty() {
        cfg.allowed_commands = cli.allowed_commands;
    }
    if !cli.blocked_commands.is_empty() {
        cfg.blocked_commands = cli.blocked_commands;
    }
    if let Some(max_output_bytes) = cli.max_output_bytes {
        cfg.max_output_bytes = max_output_bytes;
    }
    if let Some(max_concurrent_tasks) = cli.max_concurrent_tasks {
        cfg.max_concurrent_tasks = max_concurrent_tasks.max(1);
    }
    if let Some(task_result_ttl_ms) = cli.task_result_ttl_ms {
        cfg.task_result_ttl_ms = task_result_ttl_ms.max(1);
    }
    if let Some(tls_required) = cli.tls_required {
        cfg.tls_required = tls_required;
    }
    if let Some(tls_cert) = cli.tls_cert {
        cfg.tls_cert = Some(tls_cert);
    }
    if let Some(tls_key) = cli.tls_key {
        cfg.tls_key = Some(tls_key);
    }

    if cfg.bearer_token.trim().is_empty() {
        bail!("bearer token is required (set --token or config nodes.server.bearer_token)");
    }

    openprx::nodes::server::run_node_server(cfg).await
}

#[cfg(test)]
mod tests {
    use super::load_config_file;

    /// RED before SECRETS-OTHER (once `prx` encrypts the node credentials):
    /// `prx-node --config ~/.openprx/config.toml` read `enc2:` ciphertext as
    /// the bearer token and HMAC secret. After: they are decrypted with the
    /// key beside that file, as `prx` reads them.
    #[test]
    fn a_prx_config_with_encrypted_node_credentials_is_decrypted() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let store = openprx::security::SecretStore::new(dir.path(), true);
        let bearer = store.encrypt("sentinel-node-bearer").expect("test: encrypt bearer");
        let hmac = store.encrypt("sentinel-node-hmac").expect("test: encrypt hmac");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!("[nodes.server]\nbearer_token = \"{bearer}\"\nhmac_secret = \"{hmac}\"\n"),
        )
        .expect("test: write config");

        let config = load_config_file(path.to_str().expect("test: utf-8 path")).expect("test: load");
        assert_eq!(config.bearer_token, "sentinel-node-bearer");
        assert_eq!(config.hmac_secret.as_deref(), Some("sentinel-node-hmac"));
    }

    /// Guard: plaintext (an older release, or `secrets.encrypt = false`) and
    /// a standalone node file still load as written; a ciphertext that does
    /// not decrypt fails closed instead of becoming the token.
    #[test]
    fn plaintext_and_standalone_node_files_load_and_bad_ciphertext_fails() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let path = dir.path().join("config.toml");
        let path_str = path.to_str().expect("test: utf-8 path");
        std::fs::write(&path, "[nodes.server]\nbearer_token = \"plain-bearer\"\n").expect("test: write");
        assert_eq!(
            load_config_file(path_str).expect("test: load").bearer_token,
            "plain-bearer"
        );

        std::fs::write(&path, "[server]\nbearer_token = \"standalone-bearer\"\n").expect("test: write");
        assert_eq!(
            load_config_file(path_str).expect("test: load").bearer_token,
            "standalone-bearer"
        );

        let other = tempfile::TempDir::new().expect("test: other key dir");
        let foreign = openprx::security::SecretStore::new(other.path(), true)
            .encrypt("sentinel-foreign")
            .expect("test: encrypt");
        openprx::security::SecretStore::new(dir.path(), true)
            .encrypt("prime-the-key")
            .expect("test: create key");
        std::fs::write(&path, format!("[nodes.server]\nbearer_token = \"{foreign}\"\n")).expect("test: write");
        let error = load_config_file(path_str).expect_err("test: a foreign ciphertext must not load");
        assert!(!format!("{error:#}").contains("sentinel-foreign"));
    }

    /// R-SECRETS F5, RED before SECRETS-f: without a key file beside the
    /// configuration, loading an encrypted node credential wrote a fresh,
    /// wrong `.secret_key` there; and an unrelated corrupt credential of the
    /// PRX configuration stopped the node server. After: no key is created
    /// (the error names the missing key file), and only `nodes.server` is
    /// decrypted.
    #[test]
    fn a_missing_key_is_never_created_and_only_the_node_table_is_decrypted() {
        let writer = tempfile::TempDir::new().expect("test: writer dir");
        let bearer = openprx::security::SecretStore::new(writer.path(), true)
            .encrypt("sentinel-node-bearer")
            .expect("test: encrypt bearer");

        let copied = tempfile::TempDir::new().expect("test: copied config dir");
        let path = copied.path().join("config.toml");
        let path_str = path.to_str().expect("test: utf-8 path");
        std::fs::write(&path, format!("[nodes.server]\nbearer_token = \"{bearer}\"\n")).expect("test: write");
        let error = load_config_file(path_str).expect_err("test: no key, no decryption");
        assert!(format!("{error:#}").contains(".secret_key"), "{error:#}");
        assert!(!copied.path().join(".secret_key").exists(), "a key file was created");

        // With the key beside it, an unrelated undecryptable credential does
        // not matter: only `nodes.server` is read.
        std::fs::copy(writer.path().join(".secret_key"), copied.path().join(".secret_key")).expect("test: key");
        std::fs::write(
            &path,
            format!("api_key = \"enc2:00ff\"\n\n[nodes.server]\nbearer_token = \"{bearer}\"\n"),
        )
        .expect("test: write");
        assert_eq!(
            load_config_file(path_str).expect("test: load").bearer_token,
            "sentinel-node-bearer"
        );
    }
}
