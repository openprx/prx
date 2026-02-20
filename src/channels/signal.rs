use super::traits::{Channel, ChannelMessage};
use crate::config::schema::SignalConfig;
use anyhow::Context;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngressMode {
    DirectSse,
    GatewayWebhook,
    Both,
}

impl IngressMode {
    fn from_config(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "gateway_webhook" => Self::GatewayWebhook,
            "both" => Self::Both,
            _ => Self::DirectSse,
        }
    }

    fn allows_sse(self) -> bool {
        matches!(self, Self::DirectSse | Self::Both)
    }

    fn allows_gateway(self) -> bool {
        matches!(self, Self::GatewayWebhook | Self::Both)
    }
}

#[derive(Debug, Clone)]
enum SignalSender {
    Phone { e164: String },
    Uuid { raw: String },
}

impl SignalSender {
    fn display(&self) -> String {
        match self {
            Self::Phone { e164 } => e164.clone(),
            Self::Uuid { raw } => format!("uuid:{raw}"),
        }
    }

    fn recipient(&self) -> String {
        match self {
            Self::Phone { e164 } => e164.clone(),
            Self::Uuid { raw } => raw.clone(),
        }
    }

    fn peer_id(&self) -> String {
        self.display()
    }
}

#[derive(Debug, Clone)]
enum SignalTarget {
    Recipient(String),
    Group(String),
    Username(String),
}

#[derive(Debug, Default)]
struct SseEvent {
    event: Option<String>,
    data: Option<String>,
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SignalReceivePayload {
    #[serde(default)]
    envelope: Option<SignalEnvelope>,
    #[serde(default)]
    exception: Option<SignalException>,
}

#[derive(Debug, Deserialize)]
struct SignalException {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SignalEnvelope {
    #[serde(rename = "sourceNumber")]
    #[serde(default)]
    source_number: Option<String>,
    #[serde(rename = "sourceUuid")]
    #[serde(default)]
    source_uuid: Option<String>,
    #[serde(rename = "sourceName")]
    #[serde(default)]
    source_name: Option<String>,
    #[serde(default)]
    timestamp: Option<u64>,
    #[serde(rename = "dataMessage")]
    #[serde(default)]
    data_message: Option<SignalDataMessage>,
    #[serde(rename = "editMessage")]
    #[serde(default)]
    edit_message: Option<SignalEditMessage>,
    #[serde(rename = "syncMessage")]
    #[serde(default)]
    sync_message: Option<Value>,
    #[serde(rename = "reactionMessage")]
    #[serde(default)]
    reaction_message: Option<SignalReactionMessage>,
}

#[derive(Debug, Deserialize)]
struct SignalEditMessage {
    #[serde(rename = "dataMessage")]
    #[serde(default)]
    data_message: Option<SignalDataMessage>,
}

#[derive(Debug, Deserialize)]
struct SignalDataMessage {
    #[serde(default)]
    timestamp: Option<u64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    attachments: Option<Vec<SignalAttachment>>,
    #[serde(default)]
    mentions: Option<Vec<SignalMention>>,
    #[serde(rename = "groupInfo")]
    #[serde(default)]
    group_info: Option<SignalGroupInfo>,
    #[serde(default)]
    quote: Option<SignalQuote>,
    #[serde(default)]
    reaction: Option<SignalReactionMessage>,
}

#[derive(Debug, Deserialize)]
struct SignalGroupInfo {
    #[serde(rename = "groupId")]
    #[serde(default)]
    group_id: Option<String>,
    #[serde(rename = "groupName")]
    #[serde(default)]
    group_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SignalQuote {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SignalMention {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    number: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    start: Option<usize>,
    #[serde(default)]
    length: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SignalAttachment {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "contentType")]
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SignalReactionMessage {
    #[serde(default)]
    emoji: Option<String>,
    #[serde(rename = "targetAuthor")]
    #[serde(default)]
    target_author: Option<String>,
    #[serde(rename = "targetAuthorUuid")]
    #[serde(default)]
    target_author_uuid: Option<String>,
    #[serde(rename = "targetSentTimestamp")]
    #[serde(default)]
    target_sent_timestamp: Option<u64>,
    #[serde(rename = "isRemove")]
    #[serde(default)]
    is_remove: Option<bool>,
    #[serde(rename = "groupInfo")]
    #[serde(default)]
    group_info: Option<SignalGroupInfo>,
}

#[derive(Debug, Deserialize)]
struct SignalRpcResponse {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<SignalRpcError>,
}

#[derive(Debug, Deserialize)]
struct SignalRpcError {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

/// Signal channel based on signal-cli JSON-RPC + SSE events API.
pub struct SignalChannel {
    config: SignalConfig,
    client: reqwest::Client,
    dedupe: Mutex<HashMap<String, Instant>>,
    daemon: Mutex<Option<Child>>,
}

impl SignalChannel {
    pub fn new(config: SignalConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            dedupe: Mutex::new(HashMap::new()),
            daemon: Mutex::new(None),
        }
    }

    pub fn accepts_gateway_ingress(&self) -> bool {
        IngressMode::from_config(&self.config.ingress_mode).allows_gateway()
    }

    pub fn webhook_secret(&self) -> Option<&str> {
        self.config.webhook_secret.as_deref()
    }

    pub fn parse_webhook_payload(&self, payload: &serde_json::Value) -> Vec<ChannelMessage> {
        let mut out = Vec::new();

        if let Some(events) = payload.get("events").and_then(Value::as_array) {
            for event in events {
                out.extend(self.parse_single_webhook_event(event));
            }
            return out;
        }

        out.extend(self.parse_single_webhook_event(payload));
        out
    }

    fn parse_single_webhook_event(&self, payload: &serde_json::Value) -> Vec<ChannelMessage> {
        let mut out = Vec::new();

        if payload.get("envelope").is_some() {
            if let Ok(receive) = serde_json::from_value::<SignalReceivePayload>(payload.clone()) {
                if let Some(msg) = self.parse_receive_payload(&receive) {
                    out.push(msg);
                }
            }
            return out;
        }

        let event_name = payload
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event_name != "receive" {
            return out;
        }

        if let Some(data_obj) = payload.get("data") {
            let receive = if data_obj.is_string() {
                data_obj
                    .as_str()
                    .and_then(|s| serde_json::from_str::<SignalReceivePayload>(s).ok())
            } else {
                serde_json::from_value::<SignalReceivePayload>(data_obj.clone()).ok()
            };
            if let Some(receive) = receive {
                if let Some(msg) = self.parse_receive_payload(&receive) {
                    out.push(msg);
                }
            }
        }

        out
    }

    fn parse_target(raw: &str) -> anyhow::Result<SignalTarget> {
        let mut value = raw.trim();
        if value.is_empty() {
            anyhow::bail!("Signal recipient is required");
        }

        if value.to_ascii_lowercase().starts_with("signal:") {
            value = value["signal:".len()..].trim();
        }

        let lower = value.to_ascii_lowercase();
        if lower.starts_with("group:") {
            let group_id = value["group:".len()..].trim();
            if group_id.is_empty() {
                anyhow::bail!("Signal group id is required");
            }
            return Ok(SignalTarget::Group(group_id.to_string()));
        }
        if lower.starts_with("username:") {
            let username = value["username:".len()..].trim();
            if username.is_empty() {
                anyhow::bail!("Signal username is required");
            }
            return Ok(SignalTarget::Username(username.to_string()));
        }
        if lower.starts_with("u:") {
            let username = value["u:".len()..].trim();
            if username.is_empty() {
                anyhow::bail!("Signal username is required");
            }
            return Ok(SignalTarget::Username(username.to_string()));
        }

        Ok(SignalTarget::Recipient(value.to_string()))
    }

    async fn rpc_request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let url = format!("{}/api/v1/rpc", self.base_url());
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": Uuid::new_v4().to_string(),
        });

        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Signal RPC request failed ({method})"))?;

        if response.status() == reqwest::StatusCode::CREATED {
            return Ok(Value::Null);
        }

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Signal RPC HTTP {}: {}", status, text);
        }

        if text.trim().is_empty() {
            return Ok(Value::Null);
        }

        let parsed: SignalRpcResponse = serde_json::from_str(&text)
            .with_context(|| format!("Signal RPC parse failed ({method})"))?;

        if let Some(err) = parsed.error {
            let code = err.code.unwrap_or_default();
            let message = err.message.unwrap_or_else(|| "Signal RPC error".into());
            anyhow::bail!("Signal RPC {}: {}", code, message);
        }

        Ok(parsed.result.unwrap_or(Value::Null))
    }

    async fn send_single(&self, message: &str, recipient: &str) -> anyhow::Result<()> {
        let target = Self::parse_target(recipient)?;
        let mut params = serde_json::json!({
            "message": message,
        });

        if let Some(account) = self
            .config
            .account
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            params["account"] = Value::String(account.trim().to_string());
        }

        match target {
            SignalTarget::Recipient(recipient) => {
                params["recipient"] = serde_json::json!([recipient]);
            }
            SignalTarget::Group(group_id) => {
                params["groupId"] = Value::String(group_id);
            }
            SignalTarget::Username(username) => {
                params["username"] = serde_json::json!([username]);
            }
        }

        self.rpc_request("send", params).await?;
        Ok(())
    }

    async fn send_receipt(&self, recipient: &str, timestamp: u64) {
        if timestamp == 0 {
            return;
        }

        let Ok(target) = Self::parse_target(recipient) else {
            return;
        };
        if !matches!(target, SignalTarget::Recipient(_)) {
            return;
        }

        let mut params = serde_json::json!({
            "targetTimestamp": timestamp,
            "type": "read",
        });
        if let SignalTarget::Recipient(to) = target {
            params["recipient"] = serde_json::json!([to]);
        }
        if let Some(account) = self
            .config
            .account
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            params["account"] = Value::String(account.trim().to_string());
        }

        if let Err(err) = self.rpc_request("sendReceipt", params).await {
            tracing::debug!("Signal sendReceipt failed: {err}");
        }
    }

    fn base_url(&self) -> String {
        let trimmed = self.config.base_url.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            trimmed.trim_end_matches('/').to_string()
        } else {
            format!("http://{}", trimmed.trim_end_matches('/'))
        }
    }

    fn split_message(&self, message: &str) -> Vec<String> {
        let limit = self.config.text_chunk_limit.max(1);
        if message.len() <= limit {
            return vec![message.to_string()];
        }

        if self.config.chunk_mode.eq_ignore_ascii_case("newline") {
            let mut chunks = Vec::new();
            for line in message.lines() {
                if line.len() <= limit {
                    chunks.push(line.to_string());
                    continue;
                }
                let mut start = 0;
                while start < line.len() {
                    let mut end = (start + limit).min(line.len());
                    while !line.is_char_boundary(end) {
                        end -= 1;
                    }
                    chunks.push(line[start..end].to_string());
                    start = end;
                }
            }
            return chunks.into_iter().filter(|s| !s.is_empty()).collect();
        }

        let mut chunks = Vec::new();
        let mut start = 0;
        while start < message.len() {
            let mut end = (start + limit).min(message.len());
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            chunks.push(message[start..end].to_string());
            start = end;
        }
        chunks
    }

    fn normalize_phone(raw: &str) -> String {
        let trimmed = raw.trim();
        let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return trimmed.to_string();
        }
        format!("+{digits}")
    }

    fn resolve_sender(envelope: &SignalEnvelope) -> Option<SignalSender> {
        if let Some(number) = envelope.source_number.as_deref() {
            let normalized = Self::normalize_phone(number);
            if !normalized.is_empty() {
                return Some(SignalSender::Phone { e164: normalized });
            }
        }

        if let Some(uuid) = envelope.source_uuid.as_deref().map(str::trim) {
            if !uuid.is_empty() {
                return Some(SignalSender::Uuid {
                    raw: uuid.to_string(),
                });
            }
        }

        None
    }

    fn parse_allow_entry(value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed == "*" {
            return Some("*".into());
        }

        let stripped = trimmed
            .strip_prefix("signal:")
            .or_else(|| trimmed.strip_prefix("SIGNAL:"))
            .unwrap_or(trimmed)
            .trim();

        if stripped.to_ascii_lowercase().starts_with("uuid:") {
            return Some(stripped.to_ascii_lowercase());
        }

        if stripped.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
            && stripped.chars().any(|c| c.is_ascii_alphabetic())
        {
            return Some(format!("uuid:{}", stripped.to_ascii_lowercase()));
        }

        Some(Self::normalize_phone(stripped))
    }

    fn is_sender_allowed(sender: &SignalSender, allow: &[String]) -> bool {
        if allow.is_empty() {
            return false;
        }

        let parsed: Vec<String> = allow
            .iter()
            .filter_map(|entry| Self::parse_allow_entry(entry))
            .collect();

        if parsed.iter().any(|entry| entry == "*") {
            return true;
        }

        match sender {
            SignalSender::Phone { e164 } => parsed.iter().any(|entry| entry == e164),
            SignalSender::Uuid { raw } => {
                let normalized = format!("uuid:{}", raw.to_ascii_lowercase());
                parsed.iter().any(|entry| entry == &normalized)
            }
        }
    }

    fn render_mentions(message: &str, mentions: Option<&[SignalMention]>) -> String {
        let Some(mentions) = mentions else {
            return message.to_string();
        };

        let mut hydrated = message.to_string();
        for mention in mentions {
            let replacement = mention
                .number
                .as_deref()
                .map(Self::normalize_phone)
                .or_else(|| mention.uuid.as_deref().map(|u| format!("uuid:{u}")))
                .or_else(|| mention.name.clone())
                .map(|r| format!("@{r}"))
                .unwrap_or_else(|| "@mention".to_string());

            hydrated = hydrated.replacen('\u{FFFC}', &replacement, 1);
        }

        hydrated
    }

    fn media_placeholder(content_type: Option<&str>) -> &'static str {
        let Some(kind) = content_type.map(str::to_ascii_lowercase) else {
            return "<media:attachment>";
        };

        if kind.starts_with("image/") {
            "<media:image>"
        } else if kind.starts_with("video/") {
            "<media:video>"
        } else if kind.starts_with("audio/") {
            "<media:audio>"
        } else {
            "<media:attachment>"
        }
    }

    fn is_reaction_message(reaction: &SignalReactionMessage) -> bool {
        let emoji = reaction.emoji.as_deref().unwrap_or("").trim();
        let has_target = reaction
            .target_author
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
            || reaction
                .target_author_uuid
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| !s.is_empty());
        emoji.len() > 0 && has_target && reaction.target_sent_timestamp.unwrap_or(0) > 0
    }

    fn should_emit_reaction(
        &self,
        reaction: &SignalReactionMessage,
        sender: &SignalSender,
    ) -> bool {
        let mode = self.config.reaction_notifications.to_ascii_lowercase();
        if mode == "off" {
            return false;
        }

        if mode == "all" {
            return true;
        }

        if mode == "allowlist" {
            return Self::is_sender_allowed(sender, &self.config.reaction_allowlist);
        }

        // own
        let Some(account) = self.config.account.as_deref() else {
            return false;
        };

        if let Some(target_number) = reaction.target_author.as_deref() {
            return Self::normalize_phone(target_number) == Self::normalize_phone(account);
        }

        if let Some(target_uuid) = reaction.target_author_uuid.as_deref() {
            return account.eq_ignore_ascii_case(target_uuid)
                || account
                    .strip_prefix("uuid:")
                    .is_some_and(|a| a.eq_ignore_ascii_case(target_uuid));
        }

        false
    }

    fn should_skip_for_mention(&self, body: &str, is_group: bool) -> bool {
        if !is_group || !self.config.require_mention_in_groups {
            return false;
        }

        let lower = body.to_ascii_lowercase();
        if self.config.mention_patterns.is_empty() {
            return !lower.contains('@');
        }

        !self
            .config
            .mention_patterns
            .iter()
            .any(|p| !p.trim().is_empty() && lower.contains(&p.to_ascii_lowercase()))
    }

    fn allow_dm_sender(&self, sender: &SignalSender) -> bool {
        match self.config.dm_policy.to_ascii_lowercase().as_str() {
            "disabled" => false,
            "open" => true,
            "pairing" => Self::is_sender_allowed(sender, &self.config.allowed_senders),
            _ => Self::is_sender_allowed(sender, &self.config.allowed_senders),
        }
    }

    fn allow_group_sender(&self, sender: &SignalSender) -> bool {
        match self.config.group_policy.to_ascii_lowercase().as_str() {
            "disabled" => false,
            "open" => true,
            "allowlist" => {
                if !self.config.group_allowed_senders.is_empty() {
                    Self::is_sender_allowed(sender, &self.config.group_allowed_senders)
                } else {
                    Self::is_sender_allowed(sender, &self.config.allowed_senders)
                }
            }
            _ => {
                if !self.config.group_allowed_senders.is_empty() {
                    Self::is_sender_allowed(sender, &self.config.group_allowed_senders)
                } else {
                    Self::is_sender_allowed(sender, &self.config.allowed_senders)
                }
            }
        }
    }

    fn build_dedupe_key(
        &self,
        sender: &SignalSender,
        is_group: bool,
        group_id: Option<&str>,
        body: &str,
        timestamp: Option<u64>,
    ) -> String {
        let account = self
            .config
            .account
            .as_deref()
            .unwrap_or("default")
            .trim()
            .to_string();
        let conversation = if is_group {
            format!("group:{}", group_id.unwrap_or("unknown"))
        } else {
            sender.peer_id()
        };
        let content_hash = if let Some(ts) = timestamp {
            ts.to_string()
        } else {
            format!(
                "{}:{}",
                body.len(),
                body.chars().take(48).collect::<String>()
            )
        };

        format!(
            "signal:{account}:{conversation}:{}:{content_hash}",
            sender.peer_id()
        )
    }

    fn record_dedupe(&self, key: &str) -> bool {
        let mut guard = self
            .dedupe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let window = Duration::from_secs(self.config.dedupe_window_secs.max(1));

        guard.retain(|_, seen_at| now.duration_since(*seen_at) < window);

        if guard.contains_key(key) {
            return false;
        }

        guard.insert(key.to_string(), now);
        true
    }

    fn parse_receive_payload(&self, payload: &SignalReceivePayload) -> Option<ChannelMessage> {
        if let Some(exception) = payload
            .exception
            .as_ref()
            .and_then(|e| e.message.as_deref())
        {
            tracing::debug!("Signal receive exception: {exception}");
        }

        let envelope = payload.envelope.as_ref()?;
        if envelope.sync_message.is_some() {
            return None;
        }

        let sender = Self::resolve_sender(envelope)?;

        if let Some(account) = self.config.account.as_deref() {
            if let SignalSender::Phone { e164 } = &sender {
                if Self::normalize_phone(account) == *e164 {
                    return None;
                }
            }
        }

        let data_message = envelope.data_message.as_ref().or_else(|| {
            envelope
                .edit_message
                .as_ref()
                .and_then(|e| e.data_message.as_ref())
        });

        let reaction = envelope
            .reaction_message
            .as_ref()
            .or_else(|| data_message.and_then(|d| d.reaction.as_ref()));

        if let Some(reaction) = reaction {
            if Self::is_reaction_message(reaction) {
                if reaction.is_remove.unwrap_or(false) {
                    return None;
                }
                if !self.should_emit_reaction(reaction, &sender) {
                    return None;
                }
                let actor = envelope
                    .source_name
                    .as_deref()
                    .unwrap_or(&sender.display())
                    .to_string();
                let emoji = reaction.emoji.as_deref().unwrap_or("emoji").trim();
                let message_id = reaction
                    .target_sent_timestamp
                    .map(|ts| ts.to_string())
                    .unwrap_or_else(|| "unknown".into());
                let group_label = reaction
                    .group_info
                    .as_ref()
                    .and_then(|g| g.group_name.clone().or(g.group_id.clone()));
                let mut text =
                    format!("Signal reaction added: {emoji} by {actor} msg {message_id}");
                if let Some(group) = group_label {
                    text.push_str(&format!(" in {group}"));
                }

                let is_group = reaction
                    .group_info
                    .as_ref()
                    .and_then(|g| g.group_id.as_deref())
                    .is_some();

                let sender_id = if is_group {
                    format!(
                        "group:{}",
                        reaction
                            .group_info
                            .as_ref()
                            .and_then(|g| g.group_id.clone())
                            .unwrap_or_else(|| "unknown".into())
                    )
                } else {
                    format!("signal:{}", sender.recipient())
                };

                let dedupe_key = self.build_dedupe_key(&sender, is_group, None, &text, None);
                if !self.record_dedupe(&dedupe_key) {
                    return None;
                }

                return Some(ChannelMessage {
                    id: Uuid::new_v4().to_string(),
                    sender: sender_id,
                    content: text,
                    channel: "signal".to_string(),
                    timestamp: current_unix_secs(),
                });
            }
        }

        let data_message = data_message?;
        let group_id = data_message
            .group_info
            .as_ref()
            .and_then(|g| g.group_id.as_deref())
            .map(str::to_string);
        let is_group = group_id.is_some();

        if is_group {
            if !self.allow_group_sender(&sender) {
                return None;
            }
        } else if !self.allow_dm_sender(&sender) {
            return None;
        }

        let raw_message = data_message.message.as_deref().unwrap_or_default();
        let hydrated_message = Self::render_mentions(raw_message, data_message.mentions.as_deref());
        let text = hydrated_message.trim().to_string();

        let quote = data_message
            .quote
            .as_ref()
            .and_then(|q| q.text.as_ref())
            .map(|q| q.trim().to_string())
            .unwrap_or_default();

        let first_attachment = data_message
            .attachments
            .as_ref()
            .and_then(|list| list.first());

        let mut media_placeholder = String::new();
        if first_attachment.is_some() {
            if self.config.ignore_attachments {
                media_placeholder = "<media:attachment>".into();
            } else if let Some(att) = first_attachment {
                media_placeholder =
                    Self::media_placeholder(att.content_type.as_deref()).to_string();
                if let Some(size) = att.size {
                    let max_bytes = u64::from(self.config.media_max_mb) * 1024 * 1024;
                    if size > max_bytes {
                        media_placeholder = "<media:attachment_too_large>".into();
                    }
                }
                let _ = att.id.as_deref();
            }
        }

        let body = if !text.is_empty() {
            text
        } else if !media_placeholder.is_empty() {
            media_placeholder
        } else {
            quote
        };

        if body.is_empty() {
            return None;
        }

        if self.should_skip_for_mention(&body, is_group) {
            return None;
        }

        let timestamp_ms = envelope.timestamp.or(data_message.timestamp);
        let dedupe_key =
            self.build_dedupe_key(&sender, is_group, group_id.as_deref(), &body, timestamp_ms);
        if !self.record_dedupe(&dedupe_key) {
            return None;
        }

        let sender_label = if is_group {
            format!(
                "group:{}",
                group_id.clone().unwrap_or_else(|| "unknown".into())
            )
        } else {
            format!("signal:{}", sender.recipient())
        };

        let id = timestamp_ms
            .map(|ts| ts.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        Some(ChannelMessage {
            id,
            sender: sender_label,
            content: body,
            channel: "signal".to_string(),
            timestamp: timestamp_ms
                .map(|ts| ts / 1000)
                .unwrap_or_else(current_unix_secs),
        })
    }

    fn parse_sse_event_line(line: &str, current: &mut SseEvent) {
        if line.is_empty() || line.starts_with(':') {
            return;
        }

        let mut split = line.splitn(2, ':');
        let field = split.next().unwrap_or("").trim();
        let value = split
            .next()
            .map(|v| v.strip_prefix(' ').unwrap_or(v))
            .unwrap_or("");

        match field {
            "event" => current.event = Some(value.to_string()),
            "data" => {
                let next = if let Some(existing) = current.data.take() {
                    format!("{existing}\n{value}")
                } else {
                    value.to_string()
                };
                current.data = Some(next);
            }
            "id" => current.id = Some(value.to_string()),
            _ => {}
        }
    }

    fn flush_sse_event(
        &self,
        current: &mut SseEvent,
        tx: &tokio::sync::mpsc::Sender<ChannelMessage>,
    ) {
        let event_name = current.event.take();
        let data = current.data.take();
        let _ = current.id.take();

        if event_name.as_deref() != Some("receive") {
            return;
        }

        let Some(data) = data else {
            return;
        };

        match serde_json::from_str::<SignalReceivePayload>(&data) {
            Ok(payload) => {
                if let Some(msg) = self.parse_receive_payload(&payload) {
                    if tx.blocking_send(msg).is_err() {
                        tracing::debug!("Signal listener channel closed");
                    }
                }
            }
            Err(err) => {
                tracing::debug!("Signal receive payload parse failed: {err}");
            }
        }
    }

    async fn run_sse_once(
        &self,
        tx: &tokio::sync::mpsc::Sender<ChannelMessage>,
    ) -> anyhow::Result<()> {
        let url = if let Some(account) = self
            .config
            .account
            .as_deref()
            .filter(|a| !a.trim().is_empty())
        {
            format!(
                "{}/api/v1/events?account={}",
                self.base_url(),
                account.trim()
            )
        } else {
            format!("{}/api/v1/events", self.base_url())
        };

        let response = self
            .client
            .get(url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .context("Signal SSE connect failed")?;

        if !response.status().is_success() {
            anyhow::bail!("Signal SSE status {}", response.status());
        }

        let mut current = SseEvent::default();
        let mut carry = String::new();
        let mut resp = response;

        while let Some(chunk) = resp
            .chunk()
            .await
            .context("Signal SSE stream read failed")?
        {
            let text = String::from_utf8_lossy(&chunk);
            carry.push_str(&text);

            while let Some(newline) = carry.find('\n') {
                let mut line = carry[..newline].to_string();
                carry = carry[(newline + 1)..].to_string();
                if line.ends_with('\r') {
                    line.pop();
                }

                if line.is_empty() {
                    self.flush_sse_event(&mut current, tx);
                } else {
                    Self::parse_sse_event_line(&line, &mut current);
                }
            }
        }

        if !carry.is_empty() {
            Self::parse_sse_event_line(&carry, &mut current);
        }
        self.flush_sse_event(&mut current, tx);

        Ok(())
    }

    async fn ensure_daemon_started(&self) -> anyhow::Result<()> {
        if !self.config.auto_start {
            return Ok(());
        }

        {
            let guard = self
                .daemon
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_some() {
                return Ok(());
            }
        }

        let mut cmd = Command::new(self.config.cli_path.trim());
        cmd.arg("daemon")
            .arg("--http")
            .arg(self.config.http_host.trim())
            .arg("--port")
            .arg(self.config.http_port.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(account) = self
            .config
            .account
            .as_deref()
            .filter(|a| !a.trim().is_empty())
        {
            cmd.arg("--account").arg(account.trim());
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn {} daemon", self.config.cli_path))?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let lowered = line.to_ascii_lowercase();
                    if lowered.contains("error") || lowered.contains("failed") {
                        tracing::warn!("signal-cli: {line}");
                    } else {
                        tracing::debug!("signal-cli: {line}");
                    }
                }
            });
        }

        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!("signal-cli: {line}");
                }
            });
        }

        {
            let mut guard = self
                .daemon
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(child);
        }

        let timeout = Duration::from_millis(self.config.startup_timeout_ms.clamp(1_000, 120_000));
        let started = Instant::now();
        while started.elapsed() < timeout {
            if self.health_check().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        anyhow::bail!(
            "Signal daemon startup timed out after {}ms",
            timeout.as_millis()
        )
    }

    async fn stop_daemon(&self) {
        let child_opt = {
            let mut guard = self
                .daemon
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.take()
        };

        if let Some(mut child) = child_opt {
            if let Err(err) = child.kill().await {
                tracing::debug!("Signal daemon kill failed: {err}");
            }
        }
    }

    async fn sse_loop(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        let mut attempt = 0_u32;

        loop {
            match self.run_sse_once(&tx).await {
                Ok(()) => {
                    attempt = 0;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    let backoff = (2_u64.saturating_pow(attempt.min(5)) * 500).min(10_000);
                    tracing::warn!(
                        "Signal SSE disconnected: {err}. Reconnecting in {}ms",
                        backoff
                    );
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                }
            }
        }
    }
}

#[async_trait]
impl Channel for SignalChannel {
    fn name(&self) -> &str {
        "signal"
    }

    async fn send(&self, message: &str, recipient: &str) -> anyhow::Result<()> {
        let chunks = self.split_message(message);
        for chunk in chunks {
            self.send_single(&chunk, recipient).await?;
        }
        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        let mode = IngressMode::from_config(&self.config.ingress_mode);
        if !mode.allows_sse() {
            tracing::info!("Signal channel running in gateway_webhook mode");
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }

        self.ensure_daemon_started().await?;

        let result = self.sse_loop(tx).await;
        self.stop_daemon().await;
        result
    }

    async fn health_check(&self) -> bool {
        let url = format!("{}/api/v1/check", self.base_url());
        self.client
            .get(url)
            .send()
            .await
            .map(|res| res.status().is_success())
            .unwrap_or(false)
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        let target = Self::parse_target(recipient)?;
        let mut params = serde_json::json!({});

        match target {
            SignalTarget::Recipient(recipient) => {
                params["recipient"] = serde_json::json!([recipient]);
            }
            SignalTarget::Group(group_id) => {
                params["groupId"] = Value::String(group_id);
            }
            SignalTarget::Username(_) => {
                return Ok(());
            }
        }

        if let Some(account) = self
            .config
            .account
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            params["account"] = Value::String(account.trim().to_string());
        }

        self.rpc_request("sendTyping", params).await?;
        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        let target = Self::parse_target(recipient)?;
        let mut params = serde_json::json!({ "stop": true });

        match target {
            SignalTarget::Recipient(recipient) => {
                params["recipient"] = serde_json::json!([recipient]);
            }
            SignalTarget::Group(group_id) => {
                params["groupId"] = Value::String(group_id);
            }
            SignalTarget::Username(_) => {
                return Ok(());
            }
        }

        if let Some(account) = self
            .config
            .account
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            params["account"] = Value::String(account.trim().to_string());
        }

        self.rpc_request("sendTyping", params).await?;
        Ok(())
    }
}

fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> SignalChannel {
        let mut cfg = SignalConfig::default();
        cfg.dm_policy = "open".into();
        cfg.group_policy = "open".into();
        SignalChannel::new(cfg)
    }

    #[test]
    fn parse_target_recipient() {
        let target = SignalChannel::parse_target("signal:+15550001111").unwrap();
        assert!(matches!(target, SignalTarget::Recipient(v) if v == "+15550001111"));
    }

    #[test]
    fn parse_target_group() {
        let target = SignalChannel::parse_target("group:abc123").unwrap();
        assert!(matches!(target, SignalTarget::Group(v) if v == "abc123"));
    }

    #[test]
    fn parse_target_username() {
        let target = SignalChannel::parse_target("username:alice").unwrap();
        assert!(matches!(target, SignalTarget::Username(v) if v == "alice"));
    }

    #[test]
    fn parse_target_short_username() {
        let target = SignalChannel::parse_target("u:alice").unwrap();
        assert!(matches!(target, SignalTarget::Username(v) if v == "alice"));
    }

    #[test]
    fn sender_allowlist_supports_wildcard_and_uuid() {
        let sender_phone = SignalSender::Phone {
            e164: "+15550001111".into(),
        };
        let sender_uuid = SignalSender::Uuid {
            raw: "3f67c89f-625b-4d84-a4c5-2cca359f79b5".into(),
        };

        assert!(SignalChannel::is_sender_allowed(
            &sender_phone,
            &["*".into()]
        ));
        assert!(SignalChannel::is_sender_allowed(
            &sender_uuid,
            &["uuid:3f67c89f-625b-4d84-a4c5-2cca359f79b5".into()]
        ));
        assert!(!SignalChannel::is_sender_allowed(
            &sender_phone,
            &["+15550002222".into()]
        ));
    }

    #[test]
    fn parse_webhook_payload_receive_object() {
        let ch = channel();
        let payload = serde_json::json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "timestamp": 1_700_000_000_000u64,
                "dataMessage": {
                    "message": "hello"
                }
            }
        });

        let out = ch.parse_webhook_payload(&payload);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].channel, "signal");
        assert_eq!(out[0].sender, "signal:+15550001111");
        assert_eq!(out[0].content, "hello");
    }

    #[test]
    fn parse_webhook_payload_skips_sync_message() {
        let ch = channel();
        let payload = serde_json::json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "syncMessage": {},
                "dataMessage": {
                    "message": "hello"
                }
            }
        });

        let out = ch.parse_webhook_payload(&payload);
        assert!(out.is_empty());
    }

    #[test]
    fn mention_gate_blocks_group_when_required() {
        let mut cfg = SignalConfig::default();
        cfg.require_mention_in_groups = true;
        cfg.group_policy = "open".into();
        let ch = SignalChannel::new(cfg);

        let payload = serde_json::json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "timestamp": 1_700_000_000_000u64,
                "dataMessage": {
                    "message": "hello everyone",
                    "groupInfo": {"groupId": "g1"}
                }
            }
        });

        let out = ch.parse_webhook_payload(&payload);
        assert!(out.is_empty());
    }
}
