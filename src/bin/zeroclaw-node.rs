use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::Deserialize;
use zeroclaw::config::{NodeServerConfig, NodesConfig};

#[derive(Parser, Debug)]
#[command(name = "zeroclaw-node")]
#[command(about = "ZeroClaw remote node server")]
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = NodeServerConfig::default();

    if let Some(path) = &cli.config {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed reading config file {path}"))?;
        let parsed: NodeConfigFile =
            toml::from_str(&raw).with_context(|| format!("failed parsing config file {path}"))?;

        if let Some(server) = parsed.server {
            cfg = server;
        } else {
            cfg = parsed.nodes.server;
        }
    }

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

    zeroclaw::nodes::run_node_server(cfg).await
}
