//! # WacliChannel
//!
//! A WhatsApp channel that communicates with the `wacli` daemon via JSON-RPC 2.0
//! over a TCP connection (line-delimited JSON, matching jrpc2 `channel.Line`).
//!
//! ## How it works
//!
//! 1. [`WacliChannel::listen`] connects to the wacli daemon at `host:port`.
//! 2. It sends a JSON-RPC `subscribe` request to register for event notifications.
//! 3. The daemon pushes `event` notifications (method="event") for incoming messages.
//! 4. Each `message.received` event is converted to a [`ChannelMessage`] and forwarded
//!    to the agent via the `mpsc::Sender`.
//!
//! ## Reconnection
//!
//! On connection failure or unexpected disconnection, the listen loop returns an `Err`
//! and the caller ([`spawn_supervised_listener`]) handles backoff and reconnection.
//!
//! ## Sending
//!
//! [`WacliChannel::send`] opens a fresh TCP connection for each send operation,
//! sends the JSON-RPC request, reads the response, and closes. This keeps the
//! implementation simple and avoids shared state for the send path.

use super::traits::{
    extract_outgoing_media, Channel, ChannelCapabilities, ChannelMessage, SendMessage,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

// ── JSON-RPC 2.0 types ──────────────────────────────────────────────────────

#[derive(Serialize)]
struct RpcRequest<'a, P: Serialize> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    params: P,
}

#[derive(Serialize)]
struct RpcRequestNoParams<'a> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[allow(dead_code)]
    id: Option<Value>,
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

/// A server-initiated notification pushed by the wacli daemon.
#[derive(Debug, Deserialize)]
struct RpcNotification {
    #[serde(default)]
    method: String,
    params: Option<Value>,
}

// ── Channel implementation ──────────────────────────────────────────────────

/// Counter for JSON-RPC request IDs.
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Configuration for the wacli channel.
#[derive(Debug, Clone)]
pub struct WacliChannelConfig {
    /// Daemon host (default "127.0.0.1").
    pub host: String,
    /// Daemon port (default 8686).
    pub port: u16,
    /// JID allowlist. `["*"]` means all senders are accepted.
    pub allowed_from: Vec<String>,
}

impl Default for WacliChannelConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8686,
            allowed_from: vec!["*".to_string()],
        }
    }
}

/// WhatsApp channel backed by the wacli JSON-RPC daemon.
pub struct WacliChannel {
    config: Arc<WacliChannelConfig>,
}

impl WacliChannel {
    /// Create a new `WacliChannel` with the given configuration.
    pub fn new(config: WacliChannelConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Create with explicit host/port/allowlist.
    pub fn with_params(host: String, port: u16, allowed_from: Vec<String>) -> Self {
        Self::new(WacliChannelConfig {
            host,
            port,
            allowed_from,
        })
    }

    fn addr(&self) -> String {
        format!("{}:{}", self.config.host, self.config.port)
    }

    /// Return true if `sender` is in the allowlist (or the allowlist is `["*"]`).
    fn is_allowed(&self, sender: &str) -> bool {
        self.config
            .allowed_from
            .iter()
            .any(|entry| entry == "*" || entry == sender)
    }

    /// Open a TCP connection to the wacli daemon.
    async fn connect(&self) -> Result<TcpStream> {
        let addr = self.addr();
        timeout(Duration::from_secs(5), TcpStream::connect(&addr))
            .await
            .with_context(|| format!("timeout connecting to wacli daemon at {addr}"))?
            .with_context(|| format!("failed to connect to wacli daemon at {addr}"))
    }

    /// Send a JSON-RPC request and read the single-line response.
    /// Opens a fresh TCP connection for each call.
    async fn rpc_call<P: Serialize>(&self, method: &str, params: P) -> Result<Value> {
        let stream = self.connect().await?;
        let (reader, mut writer) = stream.into_split();

        let id = next_id();
        let req = RpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let mut line = serde_json::to_string(&req)
            .with_context(|| format!("serializing JSON-RPC request for '{method}'"))?;
        line.push('\n');
        writer
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("writing JSON-RPC request for '{method}'"))?;
        writer.flush().await?;

        // Read response lines until we find one with a matching id.
        let mut buf_reader = BufReader::new(reader);
        let mut response_line = String::new();
        let read_timeout = Duration::from_secs(10);

        loop {
            response_line.clear();
            let n = timeout(read_timeout, buf_reader.read_line(&mut response_line))
                .await
                .with_context(|| format!("timeout waiting for response to '{method}'"))?
                .with_context(|| format!("reading JSON-RPC response for '{method}'"))?;

            if n == 0 {
                anyhow::bail!(
                    "wacli daemon closed connection before sending response to '{method}'"
                );
            }

            let trimmed = response_line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Try to parse as a response; skip notifications (no "id").
            if let Ok(resp) = serde_json::from_str::<RpcResponse>(trimmed) {
                if let Some(ref err) = resp.error {
                    anyhow::bail!(
                        "wacli RPC '{}' returned error {}: {}",
                        method,
                        err.code,
                        err.message
                    );
                }
                return resp
                    .result
                    .ok_or_else(|| anyhow::anyhow!("wacli RPC '{method}' returned null result"));
            }
            // Not a valid response line — skip and continue reading.
        }
    }

    /// Send a plain-text message to `recipient` (a WhatsApp JID).
    async fn send_text(&self, recipient: &str, message: &str) -> Result<()> {
        #[derive(Serialize)]
        struct SendParams<'a> {
            recipient: &'a str,
            message: &'a str,
        }
        self.rpc_call("send", SendParams { recipient, message })
            .await
            .map(|_| ())
    }

    /// Send a file to `recipient`. `caption` may be empty.
    async fn send_file(&self, recipient: &str, file_path: &str, caption: &str) -> Result<()> {
        #[derive(Serialize)]
        struct SendFileParams<'a> {
            recipient: &'a str,
            #[serde(rename = "filePath")]
            file_path: &'a str,
            #[serde(skip_serializing_if = "str::is_empty")]
            caption: &'a str,
        }
        self.rpc_call(
            "sendFile",
            SendFileParams {
                recipient,
                file_path,
                caption,
            },
        )
        .await
        .map(|_| ())
    }

    /// Connect, send `subscribe`, then stream event notifications.
    async fn listen_loop(&self, tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
        let addr = self.addr();
        tracing::info!("wacli: connecting to daemon at {addr}");
        let stream = self.connect().await?;
        let (reader, mut writer) = stream.into_split();
        let mut buf_reader = BufReader::new(reader);

        // Send subscribe request.
        let subscribe_id = next_id();
        let sub_req = RpcRequestNoParams {
            jsonrpc: "2.0",
            id: subscribe_id,
            method: "subscribe",
        };
        let mut sub_line = serde_json::to_string(&sub_req)?;
        sub_line.push('\n');
        writer.write_all(sub_line.as_bytes()).await?;
        writer.flush().await?;
        tracing::info!("wacli: subscribe request sent (id={subscribe_id})");

        // Wait for the subscribe response, then stream notifications.
        let mut line = String::new();
        loop {
            line.clear();
            let n = buf_reader
                .read_line(&mut line)
                .await
                .context("reading from wacli daemon")?;

            if n == 0 {
                anyhow::bail!("wacli daemon closed connection unexpectedly");
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Try parsing as a response first (for the subscribe ack).
            if let Ok(resp) = serde_json::from_str::<serde_json::Value>(trimmed) {
                // If this has "method" == "event" it's a push notification.
                if resp.get("method").and_then(|m| m.as_str()) == Some("event") {
                    if let Ok(notif) = serde_json::from_value::<RpcNotification>(resp) {
                        self.handle_event(notif, &tx).await;
                    }
                    continue;
                }

                // Otherwise it's a response (subscribe ack or error).
                if let Ok(rpc_resp) = serde_json::from_str::<RpcResponse>(trimmed) {
                    if let Some(ref err) = rpc_resp.error {
                        anyhow::bail!("wacli subscribe failed: {} ({})", err.message, err.code);
                    }
                    tracing::info!("wacli: subscribed, waiting for events");
                }
                // Continue listening after subscribe ack.
                continue;
            }
        }
    }

    /// Convert a `message.received` event notification to a `ChannelMessage` and send it.
    async fn handle_event(&self, notif: RpcNotification, tx: &mpsc::Sender<ChannelMessage>) {
        let Some(params) = notif.params else { return };

        let event_type = params
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if event_type != "message.received" {
            return;
        }

        let payload = match params.get("payload") {
            Some(p) => p,
            None => {
                tracing::debug!("wacli: message.received event missing payload");
                return;
            }
        };

        // Skip messages sent by us.
        let from_me = payload
            .get("fromMe")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if from_me {
            return;
        }

        let sender = payload
            .get("senderJid")
            .or_else(|| payload.get("chatJid"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if sender.is_empty() {
            tracing::debug!("wacli: event missing sender JID, skipping");
            return;
        }

        // Apply allowlist filtering.
        if !self.is_allowed(sender) {
            tracing::debug!("wacli: dropping message from non-allowlisted sender: {sender}");
            return;
        }

        let chat_jid = payload
            .get("chatJid")
            .and_then(|v| v.as_str())
            .unwrap_or(sender);
        let group_name = payload.get("groupName").and_then(|v| v.as_str());
        let sender_name = payload
            .get("pushName")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let self_jid = payload
            .get("selfJid")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let is_group = chat_jid.contains("@g.us");
        let sender_display = if sender_name.is_empty() {
            sender
        } else {
            sender_name
        };

        let text = payload
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        // Build attachment description if media is present.
        let media_desc = payload.get("media").and_then(|m| {
            let media_type = m.get("type").and_then(|v| v.as_str()).unwrap_or("file");
            let caption = m.get("caption").and_then(|v| v.as_str()).unwrap_or("");
            let mime = m.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");
            if mime.is_empty() {
                Some(format!("[{media_type}]"))
            } else if caption.is_empty() {
                Some(format!("[{media_type}: {mime}]"))
            } else {
                Some(format!("[{media_type}: {mime} — {caption}]"))
            }
        });

        let content = match (text, &media_desc) {
            ("", Some(m)) => m.clone(),
            (t, Some(m)) => format!("{t}\n{m}"),
            (t, None) => t.to_string(),
        };

        if content.is_empty() {
            tracing::debug!("wacli: empty message from {sender}, skipping");
            return;
        }

        let self_note = if !self_jid.is_empty() {
            format!(" (you are {self_jid})")
        } else {
            String::new()
        };
        let prefix = if is_group {
            if let Some(name) = group_name {
                format!("[WhatsApp Group: {name}]{self_note} {sender_display}: ")
            } else {
                format!("[WhatsApp Group]{self_note} {sender_display}: ")
            }
        } else {
            format!("{sender_display}: ")
        };
        let content = format!("{prefix}{content}");

        let msg_id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let timestamp = payload
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|dt| dt.timestamp() as u64)
            })
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            });

        let channel_msg = ChannelMessage {
            id: msg_id,
            sender: sender.to_string(),
            reply_target: chat_jid.to_string(),
            content,
            channel: "wacli".to_string(),
            timestamp,
            thread_ts: None,
            mentioned_uuids: vec![],
        };

        tracing::debug!(
            "wacli: received message from sender_jid={} sender_name=\"{}\" group_name=\"{}\" in {}: {}",
            sender,
            sender_name,
            group_name.unwrap_or(""),
            chat_jid,
            &channel_msg.content.chars().take(80).collect::<String>()
        );

        if let Err(e) = tx.send(channel_msg).await {
            tracing::warn!("wacli: failed to forward message to agent: {e}");
        }
    }
}

#[async_trait]
impl Channel for WacliChannel {
    fn name(&self) -> &str {
        "wacli"
    }

    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
        self.listen_loop(tx).await
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        let recipient = &message.recipient;
        let content = &message.content;

        // Use extract_outgoing_media to handle [IMAGE:], [VOICE:], [DOCUMENT:] etc.
        let (caption, media_items) = extract_outgoing_media(content);

        if media_items.is_empty() {
            // Plain text message.
            self.send_text(recipient, content).await
        } else {
            // Send each media file; attach remaining text as caption on the first item.
            for (i, (media_type, path)) in media_items.iter().enumerate() {
                tracing::debug!("wacli: sending {media_type} from {path} to {recipient}");
                let cap = if i == 0 { caption.as_str() } else { "" };
                self.send_file(recipient, path, cap).await?;
            }
            Ok(())
        }
    }

    async fn health_check(&self) -> bool {
        // Try a lightweight RPC call to see if the daemon is alive.
        #[derive(Serialize)]
        struct ListChatsParams {
            limit: i32,
        }
        match self
            .rpc_call("listChats", ListChatsParams { limit: 1 })
            .await
        {
            Ok(_) => true,
            Err(e) => {
                tracing::debug!("wacli health_check failed: {e}");
                false
            }
        }
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            edit: false,
            delete: false,
            thread: false,
            react: false,
        }
    }
}
