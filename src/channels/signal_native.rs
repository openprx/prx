//! # SignalNativeChannel
//!
//! A Signal channel that spawns a local `signal-cli` daemon (HTTP mode) and
//! delegates all message I/O to the inner [`SignalChannel`].
//!
//! ## Why native mode?
//!
//! The classic "rest" mode connects to an externally-managed `signal-cli` daemon
//! (typically running inside a Docker container via `signal-cli-rest-api`).  Native
//! mode eliminates that dependency by letting ZeroClaw manage the daemon lifecycle
//! itself – no Docker, no REST-API wrapper.
//!
//! ## How it works
//!
//! 1. [`SignalNativeChannel::listen`] spawns
//!    `signal-cli -a <account> daemon --http 127.0.0.1:<port> --no-receive-stdout`.
//! 2. It waits for the daemon's JSON-RPC endpoint (`/api/v1/rpc`) to become
//!    reachable (up to 15 s, polling every 500 ms).
//! 3. It then delegates the long-running SSE listener to the inner
//!    [`SignalChannel`] which connects to the local HTTP daemon.
//! 4. When `listen` returns (error or graceful shutdown) the child `signal-cli`
//!    process is automatically killed because the [`tokio::process::Child`] guard
//!    is dropped.
//!
//! All send/reaction/health operations are forwarded to the inner [`SignalChannel`]
//! which communicates with the daemon's REST + JSON-RPC API.

use super::signal::SignalChannel;
use super::traits::{Channel, ChannelMessage, SendMessage};
use anyhow::Result;
use async_trait::async_trait;
use std::process::Stdio;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

/// Signal channel that spawns a local `signal-cli` daemon and communicates
/// with it over HTTP (SSE for receiving, JSON-RPC for sending).
pub struct SignalNativeChannel {
    /// Path to the `signal-cli` binary (e.g. `/usr/local/bin/signal-cli`).
    cli_path: String,
    /// E.164 account number (e.g. `+995551518602`).
    account: String,
    /// Optional `--config` path for signal-cli.  When `None`, signal-cli uses
    /// its default XDG data directory (`~/.local/share/signal-cli`).
    data_dir: Option<String>,
    /// Port on which the spawned daemon will listen for HTTP connections.
    http_port: u16,
    /// Inner channel that handles all HTTP communication with the daemon.
    inner: SignalChannel,
}

impl SignalNativeChannel {
    /// Create a new `SignalNativeChannel`.
    ///
    /// The daemon is **not** spawned here; it is started lazily in [`listen`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cli_path: String,
        account: String,
        data_dir: Option<String>,
        http_port: u16,
        group_id: Option<String>,
        allowed_from: Vec<String>,
        ignore_attachments: bool,
        ignore_stories: bool,
        media_config: crate::config::MediaConfig,
    ) -> Self {
        let http_url = format!("http://127.0.0.1:{http_port}");
        let inner = SignalChannel::new(
            http_url,
            account.clone(),
            group_id,
            allowed_from,
            ignore_attachments,
            ignore_stories,
            media_config,
        );
        Self {
            cli_path,
            account,
            data_dir,
            http_port,
            inner,
        }
    }

    /// Poll the daemon's JSON-RPC endpoint until it responds or we time out.
    async fn wait_for_ready(&self) -> Result<()> {
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{}/api/v1/rpc", self.http_port);
        let ping_body = r#"{"jsonrpc":"2.0","method":"version","id":"init"}"#;

        for attempt in 0u32..30 {
            sleep(Duration::from_millis(500)).await;
            let result = client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(ping_body)
                .timeout(Duration::from_secs(2))
                .send()
                .await;
            if result.is_ok() {
                tracing::info!(
                    "Signal native: daemon ready after {}ms",
                    (attempt + 1) * 500
                );
                return Ok(());
            }
        }
        anyhow::bail!(
            "Signal native: daemon on port {} did not become ready within 15 s",
            self.http_port
        )
    }

    /// Build the `signal-cli` command for daemon mode.
    fn build_command(&self) -> Command {
        let mut cmd = Command::new(&self.cli_path);
        // Account
        cmd.arg("-a").arg(&self.account);
        // Optional config/data dir
        if let Some(ref data_dir) = self.data_dir {
            cmd.arg("--config").arg(data_dir);
        }
        // Daemon: HTTP endpoint, no stdout receive (we use SSE)
        cmd.arg("daemon")
            .arg("--http")
            .arg(format!("127.0.0.1:{}", self.http_port))
            .arg("--no-receive-stdout")
            // Kill the child when our handle is dropped
            .kill_on_drop(true)
            // Suppress daemon stdout (we poll via HTTP)
            .stdout(Stdio::null())
            // Inherit stderr so daemon logs reach our log output
            .stderr(Stdio::inherit());
        cmd
    }
}

#[async_trait]
impl Channel for SignalNativeChannel {
    fn name(&self) -> &str {
        "signal"
    }

    /// Spawn the signal-cli daemon, wait for it to be ready, then delegate to
    /// the inner SSE listener.  The daemon process is automatically killed when
    /// this function returns (because the `Child` guard goes out of scope).
    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
        tracing::info!(
            "Signal native: spawning signal-cli daemon on port {}",
            self.http_port
        );

        let mut cmd = self.build_command();
        let _child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn signal-cli at '{}': {e}", self.cli_path))?;

        self.wait_for_ready().await?;
        tracing::info!(
            "Signal native: daemon ready on port {}, starting SSE listener",
            self.http_port
        );

        // Delegate the long-running SSE receive loop to the inner channel.
        // `_child` stays alive (kill_on_drop) until listen() returns.
        self.inner.listen(tx).await
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        self.inner.send(message).await
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }

    async fn start_typing(&self, recipient: &str) -> Result<()> {
        self.inner.start_typing(recipient).await
    }

    async fn stop_typing(&self, recipient: &str) -> Result<()> {
        self.inner.stop_typing(recipient).await
    }

    fn supports_draft_updates(&self) -> bool {
        self.inner.supports_draft_updates()
    }
}
