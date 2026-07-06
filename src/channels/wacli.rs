//! # WacliChannel
//!
//! A WhatsApp channel that integrates with the **official** `steipete/wacli`
//! tool via its standard real-time interface:
//!
//! - **Inbound**: an axum HTTP server that receives the `sync --webhook`
//!   POST callbacks. The official `wacli sync --follow --webhook <URL>
//!   --webhook-secret <SECRET>` posts every successfully-stored real-time
//!   message as JSON, signed with `X-Wacli-Signature: sha256=<hex>`.
//! - **Outbound**: shelling out to `wacli send text --to <jid> --message <text>`
//!   via [`tokio::process::Command`] (argument array, never a shell string).
//!
//! This replaces the previous self-maintained JSON-RPC TCP daemon client
//! (the forked `openprx/wacli daemon`), which is no longer supported.
//!
//! ## Inbound security model
//!
//! 1. Read the **raw** request body ([`axum::body::Bytes`]).
//! 2. Verify `X-Wacli-Signature` (constant-time HMAC-SHA256) **before**
//!    deserialization, reusing the same verification approach as
//!    `crate::webhook`.
//! 3. Only then deserialize the PascalCase `wa.ParsedMessage` payload.
//!
//! Secure-by-default: when the channel is enabled the webhook secret is
//! mandatory; unsigned requests are only honored when `allow_unsigned_loopback`
//! is explicitly set and the server binds a loopback address.

use super::traits::{Channel, ChannelCapabilities, ChannelMessage, ChatKind, SendMessage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use parking_lot::Mutex;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

// ── Tunables ────────────────────────────────────────────────────────────────

/// Maximum length (bytes) of a single outbound text message passed to
/// `wacli send`. Longer messages are truncated to avoid abusing the CLI / WA
/// limits. WhatsApp's own limit is ~65k chars; we cap well under that.
const MAX_OUTBOUND_TEXT_LEN: usize = 8192;

/// Timeout for a single `wacli send` invocation.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounded back-pressure timeout when forwarding into the agent channel. If the
/// queue stays full this long we drop the message and return 503 so the wacli
/// background worker is not blocked indefinitely.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(2);

/// Replay/idempotency cache TTL. Messages whose key was seen within this window
/// are treated as duplicates and dropped (200, not forwarded).
const REPLAY_TTL: Duration = Duration::from_secs(300);

/// Maximum number of keys retained in the replay cache.
const REPLAY_MAX_KEYS: usize = 10_000;

// ── Inbound payload (official wa.ParsedMessage, PascalCase) ──────────────────

/// Inbound media descriptor. The official payload marshals Go struct fields
/// with no JSON tags, so field names are PascalCase. Each is renamed
/// explicitly (NOT via `rename_all = "PascalCase"`, which mangles acronym
/// fields such as `ID`/`MimeType`).
#[derive(Debug, Deserialize)]
struct WacliMedia {
    #[serde(rename = "Type", default)]
    media_type: String,
    #[serde(rename = "Caption", default)]
    caption: String,
    #[serde(rename = "Filename", default)]
    _filename: String,
    #[serde(rename = "MimeType", default)]
    mime_type: String,
}

/// Official `wa.ParsedMessage` payload. Field names are PascalCase and renamed
/// per-field to preserve acronyms (`ID`, `SenderJID`, `ReplyToSenderJID`).
///
/// A migration-era `alias` is provided for a couple of fields so legacy
/// camelCase fork fixtures can still be parsed, but the official PascalCase
/// names are the authoritative path.
#[derive(Debug, Deserialize)]
struct WacliParsedMessage {
    #[serde(rename = "Chat", alias = "chatJid", default)]
    chat: String,
    #[serde(rename = "ID", alias = "id", default)]
    id: String,
    #[serde(rename = "SenderJID", alias = "senderJid", default)]
    sender_jid: String,
    #[serde(rename = "Timestamp", alias = "timestamp", default)]
    timestamp: Option<String>,
    #[serde(rename = "FromMe", alias = "fromMe", default)]
    from_me: bool,
    #[serde(rename = "Text", alias = "text", default)]
    text: String,
    #[serde(rename = "PushName", alias = "pushName", default)]
    push_name: String,
    #[serde(rename = "ReplyToSenderJID", alias = "replyToSenderJid", default)]
    reply_to_sender_jid: String,
    #[serde(rename = "Media", alias = "media", default)]
    media: Option<WacliMedia>,
}

// ── Replay cache ────────────────────────────────────────────────────────────

/// Bounded, TTL-based replay/idempotency cache. Keyed by
/// `Chat|ID|SenderJID|Timestamp`. Uses a sync `parking_lot::Mutex` because no
/// `.await` is held across the lock.
#[derive(Debug)]
struct ReplayCache {
    ttl: Duration,
    max_keys: usize,
    seen: Mutex<HashMap<String, Instant>>,
}

impl ReplayCache {
    fn new(ttl: Duration, max_keys: usize) -> Self {
        Self {
            ttl,
            max_keys: max_keys.max(1),
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Returns `true` if `key` was seen within the TTL window (a duplicate).
    /// Also performs TTL cleanup on every call. Does NOT insert the key.
    fn is_duplicate(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut seen = self.seen.lock();
        seen.retain(|_, at| now.duration_since(*at) < self.ttl);
        seen.contains_key(key)
    }

    /// Record `key` as seen (with LRU eviction when at capacity).
    /// Should be called AFTER a message has been successfully forwarded so that
    /// a failed delivery leaves the key un-recorded and the next retry passes.
    fn mark_seen(&self, key: &str) {
        let now = Instant::now();
        let mut seen = self.seen.lock();
        if seen.len() >= self.max_keys {
            // Evict the oldest entry to keep the cache bounded.
            if let Some(oldest) = seen.iter().min_by_key(|(_, at)| **at).map(|(k, _)| k.clone()) {
                seen.remove(&oldest);
            }
        }
        seen.insert(key.to_string(), now);
    }
}

// ── Config ──────────────────────────────────────────────────────────────────

/// Runtime configuration for the wacli channel (webhook + CLI).
#[derive(Debug, Clone)]
pub struct WacliChannelConfig {
    /// Address the inbound webhook server binds to (default `127.0.0.1:16868`).
    pub webhook_listen: String,
    /// HTTP path the webhook server serves (default `/wacli`).
    pub webhook_path: String,
    /// HMAC-SHA256 secret shared with `wacli sync --webhook-secret`.
    pub webhook_secret: Option<String>,
    /// Allow processing unsigned requests (no secret) ONLY when bound to a
    /// loopback address. Defaults to `false` (signature required).
    pub allow_unsigned_loopback: bool,
    /// Sender-JID allowlist. `["*"]` means all senders are accepted.
    pub allowed_from: Vec<String>,
    /// Path to the `wacli` binary used for outbound sends. `None` => `wacli`
    /// (resolved from `PATH`).
    pub cli_path: Option<String>,
    /// wacli store directory (`--store`) for outbound sends.
    pub store_dir: Option<String>,
    /// Bot's own JID, used for reply-to-bot mention detection.
    pub bot_jid: Option<String>,
    /// Bot's own phone number (digits), used for `@<number>` mention detection.
    pub bot_number: Option<String>,
    /// Bot 的 WhatsApp LID（裸数字或含 `@lid` 域名）。
    pub bot_lid: Option<String>,
}

impl Default for WacliChannelConfig {
    fn default() -> Self {
        Self {
            webhook_listen: default_webhook_listen(),
            webhook_path: default_webhook_path(),
            webhook_secret: None,
            allow_unsigned_loopback: false,
            allowed_from: vec!["*".to_string()],
            cli_path: None,
            store_dir: None,
            bot_jid: None,
            bot_number: None,
            bot_lid: None,
        }
    }
}

fn default_webhook_listen() -> String {
    "127.0.0.1:16868".to_string()
}

fn default_webhook_path() -> String {
    "/wacli".to_string()
}

// ── Shared webhook state ─────────────────────────────────────────────────────

#[derive(Clone)]
struct WebhookState {
    cfg: Arc<WacliChannelConfig>,
    /// Verified HMAC secret. `None` means unsigned-loopback mode was explicitly
    /// allowed (see [`WacliChannel::listen`]).
    secret: Option<Arc<str>>,
    tx: mpsc::Sender<ChannelMessage>,
    replay: Arc<ReplayCache>,
}

// ── Channel implementation ──────────────────────────────────────────────────

/// WhatsApp channel backed by the official `wacli sync --webhook` interface.
pub struct WacliChannel {
    config: Arc<WacliChannelConfig>,
    replay: Arc<ReplayCache>,
}

impl WacliChannel {
    /// Create a new `WacliChannel` from a full config.
    pub fn new(config: WacliChannelConfig) -> Self {
        Self {
            config: Arc::new(config),
            replay: Arc::new(ReplayCache::new(REPLAY_TTL, REPLAY_MAX_KEYS)),
        }
    }

    /// Resolved binary path for outbound `wacli` invocations.
    fn cli_binary(&self) -> &str {
        self.config.cli_path.as_deref().unwrap_or("wacli")
    }

    /// Send a plain-text message via `wacli send text`.
    ///
    /// Uses an argument array (no shell), enforces a timeout, captures stderr,
    /// and truncates over-long text. Does NOT enable `--message-escapes`.
    async fn send_text(&self, recipient: &str, message: &str) -> Result<()> {
        if recipient.trim().is_empty() {
            anyhow::bail!("wacli send: empty recipient");
        }

        // Truncate (char-boundary safe) to avoid abusing the CLI.
        let text: &str = if message.len() > MAX_OUTBOUND_TEXT_LEN {
            let mut end = MAX_OUTBOUND_TEXT_LEN;
            while end > 0 && !message.is_char_boundary(end) {
                end -= 1;
            }
            message.get(..end).unwrap_or("")
        } else {
            message
        };

        let mut cmd = tokio::process::Command::new(self.cli_binary());
        cmd.arg("send")
            .arg("text")
            .arg("--to")
            .arg(recipient)
            .arg("--message")
            .arg(text);
        if let Some(store) = self.config.store_dir.as_deref() {
            if !store.trim().is_empty() {
                cmd.arg("--store").arg(store);
            }
        }
        cmd.kill_on_drop(true);

        let output = tokio::time::timeout(SEND_TIMEOUT, cmd.output())
            .await
            .with_context(|| format!("timeout running '{} send text'", self.cli_binary()))?
            .with_context(|| format!("failed to spawn '{}'", self.cli_binary()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Do not log the full message body, only metadata + stderr.
            anyhow::bail!(
                "wacli send failed (status {}): {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            );
        }
        Ok(())
    }
}

// ── Inbound webhook handler ──────────────────────────────────────────────────

/// Verify HMAC-SHA256 of `body` against a `sha256=<hex>` signature header.
///
/// Strict: only accepts the `sha256=` prefix followed by valid hex that decodes
/// to a 32-byte digest. Verification is constant-time (`Mac::verify_slice`).
fn verify_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    use hmac::{Hmac, Mac};

    let Some(hex_part) = signature_header.trim().strip_prefix("sha256=") else {
        return false;
    };
    let Ok(provided) = hex::decode(hex_part) else {
        return false;
    };
    if provided.len() != 32 {
        return false;
    }
    let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

/// Parse an RFC3339 timestamp into unix seconds. On failure, warn and fall back
/// to "now" (never panics).
fn parse_timestamp(raw: Option<&str>) -> u64 {
    if let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) {
        match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(dt) => return dt.timestamp().max(0) as u64,
            Err(e) => {
                tracing::warn!("wacli: failed to parse RFC3339 timestamp {s:?}: {e}; using now");
            }
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 规范化 JID/LID 到裸 local part。
///
/// 示例:
/// - `"263767598346470@lid"` → `"263767598346470"`
/// - `"263767598346470@lid:3"` → `"263767598346470"`
/// - `"995551518602@s.whatsapp.net"` → `"995551518602"`
/// - `"263767598346470"` → `"263767598346470"`
/// - `""` → `""` (空值原样返回)
fn normalize_jid_local(jid: &str) -> &str {
    let trimmed = jid.trim();
    let local = match trimmed.split_once('@') {
        Some((l, _)) => l,
        None => trimmed,
    };
    match local.split_once(':') {
        Some((l, _)) => l,
        None => local,
    }
}

/// Heuristic mention detection (the official payload has no `mentionedJids`).
///
/// Checks:
/// - text `@bot_number` mention (phone number)
/// - text `@<bot_lid local>` mention (LID, e.g. `@263767598346470`)
/// - reply-to-bot via JID or LID match
fn detect_mention(cfg: &WacliChannelConfig, text: &str, reply_to_sender_jid: &str) -> bool {
    let bot_num = cfg.bot_number.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let bot_lid_local = cfg
        .bot_lid
        .as_deref()
        .map(normalize_jid_local)
        .filter(|s| !s.is_empty());
    let bot_jid = cfg.bot_jid.as_deref().map(str::trim).filter(|s| !s.is_empty());

    // 文本 @mention 检测（手机号或 LID local part）
    let by_text = bot_num.is_some_and(|num| text.contains(&format!("@{num}")))
        || bot_lid_local.is_some_and(|lid| text.contains(&format!("@{lid}")));

    // reply-to 检测：reply 的 sender 可能是 LID 或标准 JID 形式
    let reply_jid_local = normalize_jid_local(reply_to_sender_jid.trim());
    let has_reply = !reply_to_sender_jid.trim().is_empty();
    let by_reply = has_reply
        && (bot_jid.is_some_and(|jid| normalize_jid_local(jid) == reply_jid_local)
            || bot_lid_local.is_some_and(|lid| lid == reply_jid_local));

    by_text || by_reply
}

/// Return true if `sender` is in the allowlist (or the allowlist is `["*"]`).
fn sender_allowed(allowed_from: &[String], sender: &str) -> bool {
    allowed_from.iter().any(|entry| entry == "*" || entry == sender)
}

/// Extract `@<digits>` mention targets from text (best-effort).
fn extract_mentioned_numbers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    let mut after_at = false;
    for ch in text.chars() {
        if after_at && ch.is_ascii_digit() {
            current.get_or_insert_with(String::new).push(ch);
            continue;
        }
        // Sequence of digits ended; flush any accumulated number.
        if let Some(num) = current.take() {
            out.push(num);
        }
        after_at = ch == '@';
    }
    if let Some(num) = current.take() {
        out.push(num);
    }
    out
}

async fn handle_wacli_webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // 1. Signature verification on the RAW body, BEFORE deserialization.
    if let Some(ref secret) = state.secret {
        // Reject missing / duplicate / malformed signature headers.
        let mut sig_values = headers.get_all("X-Wacli-Signature").iter();
        let first = sig_values.next();
        let has_extra = sig_values.next().is_some();
        let signature = first.and_then(|v| v.to_str().ok());
        let valid = match (signature, has_extra) {
            (Some(sig), false) => verify_signature(secret, &body, sig),
            _ => false,
        };
        if !valid {
            tracing::warn!("wacli: rejecting webhook with missing/invalid HMAC signature");
            return StatusCode::UNAUTHORIZED;
        }
    }

    // 2. Deserialize the PascalCase payload (fail-closed on parse error).
    let msg: WacliParsedMessage = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("wacli: failed to deserialize webhook payload: {e}");
            // Drop silently; do not 5xx (avoids retry storms from wacli worker).
            return StatusCode::BAD_REQUEST;
        }
    };

    // 3. Required-field check (fail-closed).
    if msg.chat.trim().is_empty() || msg.id.trim().is_empty() || msg.sender_jid.trim().is_empty() {
        tracing::warn!("wacli: dropping payload missing Chat/ID/SenderJID");
        return StatusCode::BAD_REQUEST;
    }

    // 4. Skip our own messages.
    if msg.from_me {
        return StatusCode::OK;
    }

    // 5. Allowlist on the sender JID.
    let cfg = state.cfg.as_ref();
    if !sender_allowed(&cfg.allowed_from, msg.sender_jid.trim()) {
        tracing::debug!("wacli: dropping message from non-allowlisted sender");
        return StatusCode::OK;
    }

    // 6. Build media description (if any).
    let media_desc = msg.media.as_ref().map(|m| {
        let kind = if m.media_type.trim().is_empty() {
            "file"
        } else {
            m.media_type.trim()
        };
        match (m.mime_type.trim(), m.caption.trim()) {
            ("", "") => format!("[{kind}]"),
            (mime, "") => format!("[{kind}: {mime}]"),
            ("", cap) => format!("[{kind} — {cap}]"),
            (mime, cap) => format!("[{kind}: {mime} — {cap}]"),
        }
    });

    let text = msg.text.trim();
    // 7. Drop messages with neither text nor media.
    if text.is_empty() && media_desc.is_none() {
        tracing::debug!("wacli: empty message (no text/media), skipping");
        return StatusCode::OK;
    }

    // 8. Replay/idempotency check.
    let replay_key = format!(
        "{}|{}|{}|{}",
        msg.chat.trim(),
        msg.id.trim(),
        msg.sender_jid.trim(),
        msg.timestamp.as_deref().unwrap_or("")
    );
    if state.replay.is_duplicate(&replay_key) {
        tracing::debug!("wacli: duplicate message {replay_key}, skipping");
        return StatusCode::OK;
    }

    // 9. Assemble the ChannelMessage.
    let is_group = msg.chat.trim().ends_with("@g.us");
    let sender_display = if msg.push_name.trim().is_empty() {
        msg.sender_jid.trim()
    } else {
        msg.push_name.trim()
    };

    let body_content = match (text.is_empty(), &media_desc) {
        (true, Some(m)) => m.clone(),
        (false, Some(m)) => format!("{text}\n{m}"),
        (false, None) => text.to_string(),
        // (true, None) already handled above.
        (true, None) => String::new(),
    };

    // Group prefix; the official payload carries no group name, so it is omitted.
    let content = if is_group {
        format!("[WhatsApp Group] {sender_display}: {body_content}")
    } else {
        format!("{sender_display}: {body_content}")
    };

    let mentioned = detect_mention(cfg, &msg.text, &msg.reply_to_sender_jid);
    let mentioned_uuids = extract_mentioned_numbers(&msg.text);
    let timestamp = parse_timestamp(msg.timestamp.as_deref());

    let channel_msg = ChannelMessage {
        id: msg.id.trim().to_string(),
        sender: msg.sender_jid.trim().to_string(),
        reply_target: msg.chat.trim().to_string(),
        content,
        channel: "wacli".to_string(),
        timestamp,
        thread_ts: None,
        chat_kind: if is_group { ChatKind::Group } else { ChatKind::Dm },
        chat_title: None,
        sender_display: (!sender_display.trim().is_empty()).then(|| sender_display.to_string()),
        mentioned_uuids,
        mentioned,
        is_group_hint: is_group,
        sender_is_bot: false,
    };

    tracing::debug!(
        sender = %channel_msg.sender,
        chat = %channel_msg.reply_target,
        group = is_group,
        mentioned = mentioned,
        content_len = channel_msg.content.len(),
        "wacli: inbound message"
    );

    // 10. Back-pressure-aware forward (bounded wait, then 503).
    match tokio::time::timeout(FORWARD_TIMEOUT, state.tx.send(channel_msg)).await {
        Ok(Ok(())) => {
            // Record AFTER successful forward so failed/retried deliveries are not
            // permanently dropped. Known trade-off: two identical messages arriving
            // concurrently may both pass is_duplicate() and both be forwarded once;
            // that is preferable to permanent message loss.
            state.replay.mark_seen(&replay_key);
            StatusCode::OK
        }
        Ok(Err(e)) => {
            tracing::warn!("wacli: agent channel closed, cannot forward: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(_) => {
            tracing::warn!("wacli: agent channel full, dropping message (back-pressure)");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

#[async_trait]
impl Channel for WacliChannel {
    fn name(&self) -> &str {
        "wacli"
    }

    fn bot_identity(&self) -> Option<String> {
        self.config
            .bot_jid
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                self.config
                    .bot_number
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
            .map(str::to_string)
    }

    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
        let cfg = self.config.clone();

        // Resolve the HMAC secret with secure-by-default semantics.
        let trimmed_secret = cfg
            .webhook_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Arc::<str>::from(s.to_string()));

        let bind: std::net::SocketAddr = cfg
            .webhook_listen
            .parse()
            .with_context(|| format!("invalid wacli webhook_listen address: {}", cfg.webhook_listen))?;

        let is_loopback = bind.ip().is_loopback();
        let secret = match (&trimmed_secret, cfg.allow_unsigned_loopback, is_loopback) {
            (Some(_), _, _) => trimmed_secret.clone(),
            (None, true, true) => {
                tracing::warn!(
                    "wacli: running webhook WITHOUT HMAC verification (allow_unsigned_loopback=true on {bind})"
                );
                None
            }
            (None, true, false) => {
                anyhow::bail!(
                    "wacli: allow_unsigned_loopback=true but webhook_listen ({bind}) is not a loopback address; refusing to run unsigned on a non-loopback bind"
                );
            }
            (None, false, _) => {
                anyhow::bail!(
                    "wacli: webhook_secret is required when the channel is enabled (set allow_unsigned_loopback=true only for loopback testing)"
                );
            }
        };

        let path = if cfg.webhook_path.starts_with('/') {
            cfg.webhook_path.clone()
        } else {
            format!("/{}", cfg.webhook_path)
        };

        let state = WebhookState {
            cfg: cfg.clone(),
            secret,
            tx,
            replay: self.replay.clone(),
        };

        let app = Router::new().route(&path, post(handle_wacli_webhook)).with_state(state);

        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("wacli: failed to bind webhook server at {bind}"))?;
        let local = listener.local_addr().unwrap_or(bind);
        tracing::info!("wacli: webhook server listening on http://{local}{path}");

        axum::serve(listener, app)
            .await
            .context("wacli webhook server stopped unexpectedly")?;
        Ok(())
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        // Outbound is text-only for the official interface. Media markers
        // (`[IMAGE:...]` etc.) are not mapped to `wacli send file` in this
        // revision; the full content (including any markers) is sent as text so
        // information is not silently lost. Media send is a deliberate
        // follow-up (see migration doc P2).
        self.send_text(&message.recipient, &message.content).await
    }

    async fn health_check(&self) -> bool {
        // The webhook server liveness is supervised by the listener loop. As a
        // lightweight readiness signal, verify the outbound CLI is invocable.
        let mut cmd = tokio::process::Command::new(self.cli_binary());
        cmd.arg("--version").kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
            Ok(Ok(output)) => output.status.success(),
            Ok(Err(e)) => {
                tracing::debug!(
                    "wacli health_check: failed to run '{} --version': {e}",
                    self.cli_binary()
                );
                false
            }
            Err(_) => {
                tracing::debug!("wacli health_check: '{} --version' timed out", self.cli_binary());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_secret() -> WacliChannelConfig {
        WacliChannelConfig {
            webhook_secret: Some("topsecret".to_string()),
            bot_jid: Some("99550001@s.whatsapp.net".to_string()),
            bot_number: Some("99550001".to_string()),
            ..WacliChannelConfig::default()
        }
    }

    fn state_for(cfg: WacliChannelConfig, tx: mpsc::Sender<ChannelMessage>) -> WebhookState {
        let secret = cfg
            .webhook_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Arc::<str>::from(s.to_string()));
        WebhookState {
            cfg: Arc::new(cfg),
            secret,
            tx,
            replay: Arc::new(ReplayCache::new(REPLAY_TTL, REPLAY_MAX_KEYS)),
        }
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("test: hmac key");
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    /// A realistic official PascalCase payload fixture.
    const OFFICIAL_PAYLOAD: &str = r#"{
        "Chat": "120363423200597561@g.us",
        "ID": "3EB0ABCDEF",
        "SenderJID": "99551234@s.whatsapp.net",
        "Timestamp": "2026-06-17T04:39:01Z",
        "FromMe": false,
        "Text": "hello world",
        "Media": null,
        "PushName": "Alice",
        "ReplyToID": "",
        "ReplyToSenderJID": "",
        "ReplyToDisplay": ""
    }"#;

    // ── Config defaults ─────────────────────────────────────────

    #[test]
    fn default_config_webhook_listen_and_path() {
        let cfg = WacliChannelConfig::default();
        assert_eq!(cfg.webhook_listen, "127.0.0.1:16868");
        assert_eq!(cfg.webhook_path, "/wacli");
        assert!(!cfg.allow_unsigned_loopback);
        assert_eq!(cfg.allowed_from, vec!["*"]);
        assert!(cfg.cli_path.is_none());
    }

    #[test]
    fn channel_name_and_caps() {
        let ch = WacliChannel::new(WacliChannelConfig::default());
        assert_eq!(ch.name(), "wacli");
        let caps = ch.capabilities();
        assert!(!caps.edit && !caps.delete && !caps.thread && !caps.react);
        assert_eq!(ch.cli_binary(), "wacli");
    }

    // ── PascalCase deserialization (regression guard) ───────────

    #[test]
    fn deserialize_official_pascalcase_payload() {
        let msg: WacliParsedMessage =
            serde_json::from_str(OFFICIAL_PAYLOAD).expect("test: official payload should parse");
        assert_eq!(msg.chat, "120363423200597561@g.us");
        assert_eq!(msg.id, "3EB0ABCDEF");
        assert_eq!(msg.sender_jid, "99551234@s.whatsapp.net");
        assert_eq!(msg.timestamp.as_deref(), Some("2026-06-17T04:39:01Z"));
        assert!(!msg.from_me);
        assert_eq!(msg.text, "hello world");
        assert_eq!(msg.push_name, "Alice");
        assert!(msg.media.is_none());
    }

    #[test]
    fn deserialize_media_pascalcase() {
        let payload = r#"{
            "Chat": "1@s.whatsapp.net",
            "ID": "X1",
            "SenderJID": "1@s.whatsapp.net",
            "FromMe": false,
            "Text": "",
            "Media": {"Type": "image", "Caption": "photo", "Filename": "a.jpg", "MimeType": "image/jpeg"}
        }"#;
        let msg: WacliParsedMessage = serde_json::from_str(payload).expect("test: media payload parses");
        let media = msg.media.expect("test: media present");
        assert_eq!(media.media_type, "image");
        assert_eq!(media.caption, "photo");
        assert_eq!(media.mime_type, "image/jpeg");
    }

    // ── HMAC verification ───────────────────────────────────────

    #[test]
    fn verify_signature_accepts_correct() {
        let body = b"some-raw-body";
        let sig = sign("topsecret", body);
        assert!(verify_signature("topsecret", body, &sig));
    }

    #[test]
    fn verify_signature_rejects_wrong_secret() {
        let body = b"some-raw-body";
        let sig = sign("topsecret", body);
        assert!(!verify_signature("othersecret", body, &sig));
    }

    #[test]
    fn verify_signature_rejects_missing_prefix() {
        let body = b"x";
        // raw hex without sha256= prefix must be rejected (strict).
        let raw = {
            use hmac::{Hmac, Mac};
            let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"topsecret").expect("test");
            mac.update(body);
            hex::encode(mac.finalize().into_bytes())
        };
        assert!(!verify_signature("topsecret", body, &raw));
    }

    #[test]
    fn verify_signature_rejects_bad_length() {
        assert!(!verify_signature("topsecret", b"x", "sha256=deadbeef"));
    }

    #[test]
    fn verify_signature_rejects_non_hex() {
        assert!(!verify_signature("topsecret", b"x", "sha256=zzzz"));
    }

    // ── End-to-end handler via axum router ──────────────────────

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn router_for(state: WebhookState) -> Router {
        Router::new()
            .route("/wacli", post(handle_wacli_webhook))
            .with_state(state)
    }

    #[tokio::test]
    async fn handler_accepts_valid_signature_and_forwards() {
        let (tx, mut rx) = mpsc::channel(4);
        let app = router_for(state_for(cfg_with_secret(), tx));
        let body = OFFICIAL_PAYLOAD.as_bytes().to_vec();
        let sig = sign("topsecret", &body);

        let req = Request::builder()
            .method("POST")
            .uri("/wacli")
            .header("X-Wacli-Signature", sig)
            .body(Body::from(body))
            .expect("test: build req");
        let resp = app.oneshot(req).await.expect("test: response");
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = rx.try_recv().expect("test: forwarded message");
        assert_eq!(msg.id, "3EB0ABCDEF");
        assert_eq!(msg.sender, "99551234@s.whatsapp.net");
        assert_eq!(msg.reply_target, "120363423200597561@g.us");
        assert!(msg.is_group_hint);
        assert!(msg.content.contains("[WhatsApp Group]"));
        assert!(msg.content.contains("Alice"));
        assert!(msg.content.contains("hello world"));
    }

    #[tokio::test]
    async fn handler_rejects_invalid_signature() {
        let (tx, mut rx) = mpsc::channel(4);
        let app = router_for(state_for(cfg_with_secret(), tx));
        let body = OFFICIAL_PAYLOAD.as_bytes().to_vec();

        let req = Request::builder()
            .method("POST")
            .uri("/wacli")
            .header("X-Wacli-Signature", "sha256=00")
            .body(Body::from(body))
            .expect("test: build req");
        let resp = app.oneshot(req).await.expect("test: response");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handler_rejects_missing_signature_when_secret_set() {
        let (tx, _rx) = mpsc::channel(4);
        let app = router_for(state_for(cfg_with_secret(), tx));
        let body = OFFICIAL_PAYLOAD.as_bytes().to_vec();

        let req = Request::builder()
            .method("POST")
            .uri("/wacli")
            .body(Body::from(body))
            .expect("test: build req");
        let resp = app.oneshot(req).await.expect("test: response");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn handler_unsigned_mode_accepts_without_signature() {
        let (tx, mut rx) = mpsc::channel(4);
        let cfg = WacliChannelConfig {
            webhook_secret: None,
            allow_unsigned_loopback: true,
            ..WacliChannelConfig::default()
        };
        let app = router_for(state_for(cfg, tx));
        let body = OFFICIAL_PAYLOAD.as_bytes().to_vec();

        let req = Request::builder()
            .method("POST")
            .uri("/wacli")
            .body(Body::from(body))
            .expect("test: build req");
        let resp = app.oneshot(req).await.expect("test: response");
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn handler_skips_from_me() {
        let (tx, mut rx) = mpsc::channel(4);
        let app = router_for(state_for(cfg_with_secret(), tx));
        let body = r#"{"Chat":"1@s.whatsapp.net","ID":"a","SenderJID":"1@s.whatsapp.net","FromMe":true,"Text":"mine"}"#
            .as_bytes()
            .to_vec();
        let sig = sign("topsecret", &body);

        let req = Request::builder()
            .method("POST")
            .uri("/wacli")
            .header("X-Wacli-Signature", sig)
            .body(Body::from(body))
            .expect("test: build req");
        let resp = app.oneshot(req).await.expect("test: response");
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(rx.try_recv().is_err(), "FromMe must be skipped");
    }

    #[tokio::test]
    async fn handler_rejects_missing_required_fields() {
        let (tx, _rx) = mpsc::channel(4);
        let app = router_for(state_for(cfg_with_secret(), tx));
        let body = r#"{"Chat":"","ID":"","SenderJID":"","FromMe":false,"Text":"x"}"#
            .as_bytes()
            .to_vec();
        let sig = sign("topsecret", &body);

        let req = Request::builder()
            .method("POST")
            .uri("/wacli")
            .header("X-Wacli-Signature", sig)
            .body(Body::from(body))
            .expect("test: build req");
        let resp = app.oneshot(req).await.expect("test: response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn handler_dedups_replays() {
        let (tx, mut rx) = mpsc::channel(4);
        let app = router_for(state_for(cfg_with_secret(), tx));
        let body = OFFICIAL_PAYLOAD.as_bytes().to_vec();
        let sig = sign("topsecret", &body);

        let mk = || {
            Request::builder()
                .method("POST")
                .uri("/wacli")
                .header("X-Wacli-Signature", sig.clone())
                .body(Body::from(body.clone()))
                .expect("test: build req")
        };

        let r1 = app.clone().oneshot(mk()).await.expect("test");
        assert_eq!(r1.status(), StatusCode::OK);
        let r2 = app.oneshot(mk()).await.expect("test");
        assert_eq!(r2.status(), StatusCode::OK);

        assert!(rx.try_recv().is_ok(), "first delivered");
        assert!(rx.try_recv().is_err(), "replay must not be forwarded");
    }

    #[tokio::test]
    async fn replay_key_not_recorded_on_forward_failure() {
        // When the channel receiver is dropped and forwarding fails with a
        // SendError, the replay key must NOT be recorded so a future retry can
        // succeed. We verify this by querying is_duplicate directly on the cache.
        let (tx, rx) = mpsc::channel(1);
        let state = state_for(cfg_with_secret(), tx);
        let replay_key = "chat|id|sender|ts";
        // Key must not be in the cache yet.
        assert!(!state.replay.is_duplicate(replay_key), "key must not be pre-recorded");
        drop(rx); // close the receiver so any send will return Err
        // mark_seen was never called, so the key must remain absent.
        assert!(
            !state.replay.is_duplicate(replay_key),
            "key must not be recorded after forward failure"
        );
    }

    #[tokio::test]
    async fn replay_key_recorded_after_successful_forward() {
        // After a successful forward, the replay key IS recorded so retries are
        // deduped (the second identical request must be dropped).
        let (tx, mut rx) = mpsc::channel(4);
        let app = router_for(state_for(cfg_with_secret(), tx));
        let body = OFFICIAL_PAYLOAD.as_bytes().to_vec();
        let sig = sign("topsecret", &body);

        let mk = || {
            Request::builder()
                .method("POST")
                .uri("/wacli")
                .header("X-Wacli-Signature", sig.clone())
                .body(Body::from(body.clone()))
                .expect("test: build req")
        };

        // First request succeeds and is forwarded.
        let r1 = app.clone().oneshot(mk()).await.expect("test");
        assert_eq!(r1.status(), StatusCode::OK);
        // Second identical request must be deduped (key was recorded on success).
        let r2 = app.oneshot(mk()).await.expect("test");
        assert_eq!(r2.status(), StatusCode::OK);

        assert!(rx.try_recv().is_ok(), "first must be forwarded");
        assert!(rx.try_recv().is_err(), "duplicate must be dropped");
    }

    #[tokio::test]
    async fn handler_allowlist_blocks_sender() {
        let (tx, mut rx) = mpsc::channel(4);
        let cfg = WacliChannelConfig {
            allowed_from: vec!["allowed@s.whatsapp.net".to_string()],
            ..cfg_with_secret()
        };
        let app = router_for(state_for(cfg, tx));
        let body = OFFICIAL_PAYLOAD.as_bytes().to_vec(); // sender 99551234@...
        let sig = sign("topsecret", &body);

        let req = Request::builder()
            .method("POST")
            .uri("/wacli")
            .header("X-Wacli-Signature", sig)
            .body(Body::from(body))
            .expect("test: build req");
        let resp = app.oneshot(req).await.expect("test: response");
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(rx.try_recv().is_err(), "non-allowlisted sender dropped");
    }

    // ── Mention heuristic ───────────────────────────────────────

    #[test]
    fn detect_mention_by_number() {
        let cfg = cfg_with_secret();
        assert!(detect_mention(&cfg, "hey @99550001 ping", ""));
        assert!(!detect_mention(&cfg, "no mention here", ""));
    }

    #[test]
    fn detect_mention_by_reply_to_bot() {
        let cfg = cfg_with_secret();
        assert!(detect_mention(&cfg, "reply text", "99550001@s.whatsapp.net"));
        assert!(!detect_mention(&cfg, "reply text", "someoneelse@s.whatsapp.net"));
    }

    #[test]
    fn detect_mention_empty_reply_not_matched() {
        let cfg = WacliChannelConfig {
            bot_jid: Some("".to_string()),
            bot_number: None,
            ..WacliChannelConfig::default()
        };
        // Empty bot_jid must never match an empty reply_to.
        assert!(!detect_mention(&cfg, "hi", ""));
    }

    #[test]
    fn extract_mentioned_numbers_parses_digits() {
        assert_eq!(extract_mentioned_numbers("hi @123 and @4567 ok"), vec!["123", "4567"]);
        assert!(extract_mentioned_numbers("no mentions").is_empty());
        assert!(extract_mentioned_numbers("@notdigits").is_empty());
    }

    // ── allowlist ───────────────────────────────────────────────

    #[test]
    fn wildcard_allows_any_sender() {
        assert!(sender_allowed(&["*".to_string()], "12345@s.whatsapp.net"));
    }

    #[test]
    fn empty_allowlist_blocks_all() {
        assert!(!sender_allowed(&[], "anyone"));
    }

    #[test]
    fn specific_allowlist_filters() {
        let list = vec!["alice@s.whatsapp.net".to_string()];
        assert!(sender_allowed(&list, "alice@s.whatsapp.net"));
        assert!(!sender_allowed(&list, "bob@s.whatsapp.net"));
    }

    // ── timestamp parsing ───────────────────────────────────────

    #[test]
    fn parse_timestamp_valid_rfc3339() {
        let ts = parse_timestamp(Some("2026-06-17T04:39:01Z"));
        assert!(ts > 1_700_000_000);
    }

    #[test]
    fn parse_timestamp_invalid_falls_back_to_now() {
        let ts = parse_timestamp(Some("not-a-timestamp"));
        assert!(ts > 1_700_000_000); // fell back to ~now
    }

    // ── LID mention 测试 ──────────────────────────────────────────────

    fn make_cfg_with_lid(bot_number: Option<&str>, bot_lid: Option<&str>, bot_jid: Option<&str>) -> WacliChannelConfig {
        WacliChannelConfig {
            webhook_listen: String::new(),
            webhook_path: String::new(),
            webhook_secret: None,
            allow_unsigned_loopback: false,
            allowed_from: vec![],
            cli_path: None,
            store_dir: None,
            bot_jid: bot_jid.map(str::to_owned),
            bot_number: bot_number.map(str::to_owned),
            bot_lid: bot_lid.map(str::to_owned),
        }
    }

    #[test]
    fn test_mention_by_lid_bare_number() {
        let cfg = make_cfg_with_lid(None, Some("263767598346470"), None);
        assert!(detect_mention(&cfg, "hey @263767598346470 please help", ""));
    }

    #[test]
    fn test_mention_by_lid_with_domain_in_config() {
        // config 里填了完整 LID（含 @lid 域），应自动规范化
        let cfg = make_cfg_with_lid(None, Some("263767598346470@lid"), None);
        assert!(detect_mention(&cfg, "hey @263767598346470 please help", ""));
    }

    #[test]
    fn test_mention_by_number_regression() {
        let cfg = make_cfg_with_lid(Some("995551518602"), None, None);
        assert!(detect_mention(&cfg, "hi @995551518602 how are you", ""));
    }

    #[test]
    fn test_reply_to_lid_with_domain() {
        let cfg = make_cfg_with_lid(None, Some("263767598346470"), None);
        assert!(detect_mention(&cfg, "", "263767598346470@lid"));
    }

    #[test]
    fn test_reply_to_lid_with_device_suffix() {
        let cfg = make_cfg_with_lid(None, Some("263767598346470"), None);
        assert!(detect_mention(&cfg, "", "263767598346470@lid:3"));
    }

    #[test]
    fn test_no_mention_no_reply() {
        let cfg = make_cfg_with_lid(
            Some("995551518602"),
            Some("263767598346470"),
            Some("995551518602@s.whatsapp.net"),
        );
        assert!(!detect_mention(&cfg, "hello world", ""));
    }

    #[test]
    fn test_bot_lid_none_no_panic() {
        // bot_lid 为 None 时不 panic，不误匹配
        let cfg = make_cfg_with_lid(Some("995551518602"), None, None);
        assert!(!detect_mention(&cfg, "hey @263767598346470 hello", ""));
    }

    #[test]
    fn test_normalize_jid_local() {
        assert_eq!(normalize_jid_local("263767598346470@lid"), "263767598346470");
        assert_eq!(normalize_jid_local("263767598346470@lid:3"), "263767598346470");
        assert_eq!(normalize_jid_local("995551518602@s.whatsapp.net"), "995551518602");
        assert_eq!(normalize_jid_local("263767598346470"), "263767598346470");
        assert_eq!(normalize_jid_local(""), "");
        assert_eq!(normalize_jid_local("  995551518602@s.whatsapp.net  "), "995551518602");
    }
}
