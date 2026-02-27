use crate::channels::traits::{
    extract_outgoing_media, guess_audio_mime, guess_mime_from_path, Channel, ChannelMessage,
    SendMessage,
};
use async_trait::async_trait;
use base64::Engine as _;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Map a MIME type to a simple file extension for temp files.
fn mime_to_extension(mime: &str) -> &str {
    // Strip codec parameters (e.g. "audio/ogg; codecs=opus" → "audio/ogg")
    let base = mime.split(';').next().unwrap_or(mime).trim();
    match base {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/aac" => "m4a",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        // Documents
        "application/pdf" => "pdf",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/msword" => "doc",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/rtf" | "text/rtf" => "rtf",
        "application/epub+zip" => "epub",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "text/html" => "html",
        "text/xml" | "application/xml" => "xml",
        "application/json" => "json",
        "application/toml" => "toml",
        "text/markdown" => "md",
        _ => "bin",
    }
}

/// Run an external command and return its stdout as a string (if successful and non-empty).
async fn run_command(cmd: &str, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .ok()?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    } else {
        tracing::debug!(
            "run_command {cmd} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        None
    }
}

/// Return true if the file should be treated as plain text (read directly).
fn is_text_file(content_type: &str, ext: &str) -> bool {
    content_type.starts_with("text/")
        || matches!(
            ext,
            "txt"
                | "csv"
                | "json"
                | "xml"
                | "yaml"
                | "yml"
                | "toml"
                | "md"
                | "html"
                | "log"
                | "ini"
                | "conf"
                | "cfg"
                | "env"
                | "rs"
                | "py"
                | "js"
                | "ts"
                | "go"
                | "java"
                | "c"
                | "cpp"
                | "h"
                | "sh"
                | "sql"
                | "vue"
                | "svelte"
                | "jsx"
                | "tsx"
                | "rb"
                | "php"
                | "swift"
                | "kt"
                | "cs"
                | "r"
        )
}

/// Extract readable text from a document file.
/// Returns None if extraction failed or file type is not supported.
async fn extract_document_text(path: &str, content_type: &str, filename: &str) -> Option<String> {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Plain text files — read directly
    if is_text_file(content_type, &ext) {
        return std::fs::read_to_string(path).ok();
    }

    // PDF → pdftotext
    if ext == "pdf" || content_type == "application/pdf" {
        return run_command("pdftotext", &[path, "-"]).await;
    }

    // XLSX / XLS → openpyxl or xlsx2csv
    if ext == "xlsx"
        || ext == "xls"
        || content_type.contains("spreadsheet")
        || content_type.contains("ms-excel")
    {
        let script = format!(
            "import openpyxl; wb=openpyxl.load_workbook('{}', read_only=True, data_only=True); \
            [print('\\n=== ' + ws.title + ' ===\\n' + '\\n'.join('\\t'.join(str(c.value or '') for c in row) for row in ws.iter_rows())) for ws in wb.worksheets]",
            path.replace('\'', "\\'")
        );
        if let Some(t) = run_command("python3", &["-c", &script]).await {
            return Some(t);
        }
        return run_command("xlsx2csv", &[path]).await;
    }

    // DOCX → python-docx or pandoc
    if ext == "docx" || content_type.contains("wordprocessingml") {
        let script = format!(
            "from docx import Document; d=Document('{}'); \
            print('\\n'.join(p.text for p in d.paragraphs if p.text.strip()))",
            path.replace('\'', "\\'")
        );
        if let Some(t) = run_command("python3", &["-c", &script]).await {
            return Some(t);
        }
        return run_command("pandoc", &["-t", "plain", path]).await;
    }

    // PPTX → python-pptx or pandoc
    if ext == "pptx" || content_type.contains("presentationml") {
        let script = format!(
            "from pptx import Presentation; prs=Presentation('{}'); \
            [print(shape.text) for slide in prs.slides for shape in slide.shapes if hasattr(shape, 'text') and shape.text.strip()]",
            path.replace('\'', "\\'")
        );
        if let Some(t) = run_command("python3", &["-c", &script]).await {
            return Some(t);
        }
        return run_command("pandoc", &["-t", "plain", path]).await;
    }

    // RTF → unrtf or pandoc
    if ext == "rtf" || content_type.contains("rtf") {
        if let Some(t) = run_command("unrtf", &["--text", path]).await {
            return Some(t);
        }
        return run_command("pandoc", &["-t", "plain", path]).await;
    }

    // EPUB → pandoc
    if ext == "epub" || content_type == "application/epub+zip" {
        return run_command("pandoc", &["-t", "plain", path]).await;
    }

    // Fallback: try pandoc for anything else (DOC, ODT, etc.)
    run_command("pandoc", &["-t", "plain", path]).await
}

const GROUP_TARGET_PREFIX: &str = "group:";

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecipientTarget {
    Direct(String),
    Group(String),
}

/// Signal channel using signal-cli daemon's native JSON-RPC + SSE API.
///
/// Connects to a running `signal-cli daemon --http <host:port>`.
/// Listens via SSE at `/api/v1/events` and sends via JSON-RPC at
/// `/api/v1/rpc`.
#[derive(Clone)]
pub struct SignalChannel {
    http_url: String,
    account: String,
    group_id: Option<String>,
    allowed_from: Vec<String>,
    ignore_attachments: bool,
    ignore_stories: bool,
    /// Media understanding config for audio STT and video frame extraction.
    media_config: crate::config::MediaConfig,
    /// When true, use native signal-cli daemon JSON-RPC API (`/api/v1/rpc`)
    /// instead of the Docker signal-cli-rest-api REST endpoints.
    is_native: bool,
    /// signal-cli data directory (native mode). Used to resolve attachment paths.
    data_dir: Option<String>,
}

// ── signal-cli SSE event JSON shapes ────────────────────────────

#[derive(Debug, Deserialize)]
struct SseEnvelope {
    #[serde(default)]
    envelope: Option<Envelope>,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    source: Option<String>,
    #[serde(rename = "sourceNumber", default)]
    source_number: Option<String>,
    #[serde(rename = "dataMessage", default)]
    data_message: Option<DataMessage>,
    #[serde(rename = "storyMessage", default)]
    story_message: Option<serde_json::Value>,
    #[serde(default)]
    timestamp: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DataMessage {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    timestamp: Option<u64>,
    #[serde(rename = "groupInfo", default)]
    group_info: Option<GroupInfo>,
    #[serde(default)]
    attachments: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct GroupInfo {
    #[serde(rename = "groupId", default)]
    group_id: Option<String>,
    #[serde(rename = "groupName", alias = "name", default)]
    group_name: Option<String>,
}

impl SignalChannel {
    pub fn new(
        http_url: String,
        account: String,
        group_id: Option<String>,
        allowed_from: Vec<String>,
        ignore_attachments: bool,
        ignore_stories: bool,
        media_config: crate::config::MediaConfig,
    ) -> Self {
        Self::new_with_mode(
            http_url,
            account,
            group_id,
            allowed_from,
            ignore_attachments,
            ignore_stories,
            media_config,
            false,
            None,
        )
    }

    /// Like [`new`] but allows specifying native daemon mode.
    pub fn new_with_mode(
        http_url: String,
        account: String,
        group_id: Option<String>,
        allowed_from: Vec<String>,
        ignore_attachments: bool,
        ignore_stories: bool,
        media_config: crate::config::MediaConfig,
        is_native: bool,
        data_dir: Option<String>,
    ) -> Self {
        let http_url = http_url.trim_end_matches('/').to_string();
        Self {
            http_url,
            account,
            group_id,
            allowed_from,
            ignore_attachments,
            ignore_stories,
            media_config,
            is_native,
            data_dir,
        }
    }

    fn http_client(&self) -> Client {
        let builder = Client::builder().connect_timeout(Duration::from_secs(10));
        let builder = crate::config::apply_runtime_proxy_to_builder(builder, "channel.signal");
        builder.build().expect("Signal HTTP client should build")
    }

    /// Effective sender: prefer `sourceNumber` (E.164), fall back to `source`.
    fn sender(envelope: &Envelope) -> Option<String> {
        envelope
            .source_number
            .as_deref()
            .or(envelope.source.as_deref())
            .map(String::from)
    }

    fn is_sender_allowed(&self, sender: &str) -> bool {
        if self.allowed_from.iter().any(|u| u == "*") {
            return true;
        }
        self.allowed_from.iter().any(|u| u == sender)
    }

    fn is_e164(recipient: &str) -> bool {
        let Some(number) = recipient.strip_prefix('+') else {
            return false;
        };
        (2..=15).contains(&number.len()) && number.chars().all(|c| c.is_ascii_digit())
    }

    /// Check whether a string is a valid UUID (signal-cli uses these for
    /// privacy-enabled users who have opted out of sharing their phone number).
    fn is_uuid(s: &str) -> bool {
        Uuid::parse_str(s).is_ok()
    }

    fn parse_recipient_target(recipient: &str) -> RecipientTarget {
        if let Some(group_id) = recipient.strip_prefix(GROUP_TARGET_PREFIX) {
            return RecipientTarget::Group(group_id.to_string());
        }

        if Self::is_e164(recipient) || Self::is_uuid(recipient) {
            RecipientTarget::Direct(recipient.to_string())
        } else {
            RecipientTarget::Group(recipient.to_string())
        }
    }

    /// Check whether the message targets the configured group.
    /// If no `group_id` is configured (None), all DMs and groups are accepted.
    /// Use "dm" to filter DMs only.
    fn matches_group(&self, data_msg: &DataMessage) -> bool {
        let Some(ref expected) = self.group_id else {
            return true;
        };
        match data_msg
            .group_info
            .as_ref()
            .and_then(|g| g.group_id.as_deref())
        {
            Some(gid) => gid == expected.as_str(),
            None => expected.eq_ignore_ascii_case("dm"),
        }
    }

    /// Determine the send target: group id or the sender's number.
    fn reply_target(&self, data_msg: &DataMessage, sender: &str) -> String {
        if let Some(group_id) = data_msg
            .group_info
            .as_ref()
            .and_then(|g| g.group_id.as_deref())
        {
            format!("{GROUP_TARGET_PREFIX}{group_id}")
        } else {
            sender.to_string()
        }
    }

    /// Send a JSON-RPC request to signal-cli daemon.
    async fn rpc_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let url = format!("{}/api/v1/rpc", self.http_url);
        let id = Uuid::new_v4().to_string();

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });

        let resp = self
            .http_client()
            .post(&url)
            .timeout(Duration::from_secs(30))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        // 201 = success with no body (e.g. typing indicators)
        if resp.status().as_u16() == 201 {
            return Ok(None);
        }

        let text = resp.text().await?;
        if text.is_empty() {
            return Ok(None);
        }

        let parsed: serde_json::Value = serde_json::from_str(&text)?;
        if let Some(err) = parsed.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            anyhow::bail!("Signal RPC error {code}: {msg}");
        }

        Ok(parsed.get("result").cloned())
    }

    /// Process a single SSE envelope, returning a ChannelMessage if valid.
    fn process_envelope(&self, envelope: &Envelope) -> Option<ChannelMessage> {
        // Skip story messages when configured
        if self.ignore_stories && envelope.story_message.is_some() {
            return None;
        }

        let data_msg = envelope.data_message.as_ref()?;

        // Skip attachment-only messages when configured
        if self.ignore_attachments {
            let has_attachments = data_msg.attachments.as_ref().is_some_and(|a| !a.is_empty());
            if has_attachments && data_msg.message.is_none() {
                return None;
            }
        }

        // Allow attachment-only messages (voice, image, video) even without text
        let has_attachments = data_msg.attachments.as_ref().is_some_and(|a| !a.is_empty());
        let text = data_msg.message.as_deref().unwrap_or("");
        if text.is_empty() && !has_attachments {
            return None;
        }
        let sender = Self::sender(envelope)?;
        let is_group_message = data_msg.group_info.is_some();

        if !is_group_message && !self.is_sender_allowed(&sender) {
            return None;
        }

        if !self.matches_group(data_msg) {
            return None;
        }

        let target = self.reply_target(data_msg, &sender);

        let timestamp = data_msg
            .timestamp
            .or(envelope.timestamp)
            .unwrap_or_else(|| {
                u64::try_from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                )
                .unwrap_or(u64::MAX)
            });

        let group_info = data_msg.group_info.as_ref();
        let group_label = group_info.and_then(|g| {
            g.group_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .or(g.group_id.as_deref())
        });
        let content_prefix = if is_group_message {
            if let Some(group_name) = group_info
                .and_then(|g| g.group_name.as_deref())
                .filter(|name| !name.trim().is_empty())
            {
                format!("[Signal Group: {group_name}] {sender}: ")
            } else {
                format!("[Signal Group] {sender}: ")
            }
        } else {
            String::new()
        };

        // Append Signal reaction metadata so the LLM has the information needed
        // to call message_send(action="react", target_author=..., target_timestamp=...).
        let signal_meta = if is_group_message {
            let group = group_label.unwrap_or("unknown");
            format!("[signal-meta sender={sender} ts={timestamp} group={group} chat_type=group]")
        } else {
            format!("[signal-meta sender={sender} ts={timestamp} chat_type=direct]")
        };

        let mut content_with_meta = String::new();
        content_with_meta.push_str(&content_prefix);
        if !text.is_empty() {
            content_with_meta.push_str(text);
            content_with_meta.push('\n');
        }
        content_with_meta.push_str(&signal_meta);

        Some(ChannelMessage {
            id: format!("sig_{timestamp}"),
            sender: sender.clone(),
            reply_target: target,
            content: content_with_meta,
            channel: "signal".to_string(),
            timestamp: timestamp / 1000, // millis → secs
            thread_ts: None,
        })
    }

    /// Download a single Signal attachment and return an inline marker string
    /// (`[IMAGE:path]`, `<media:audio ...>`, etc.) ready to append to message content.
    ///
    /// In native mode: reads the local file path from the `file` field in the
    /// attachment JSON (signal-cli downloads attachments to its data directory).
    /// In REST mode: downloads via `GET /v1/attachments/{id}`.
    async fn download_attachment_as_marker(&self, att: &serde_json::Value) -> Option<String> {
        let content_type = att
            .get("contentType")
            .and_then(|v| v.as_str())
            .unwrap_or("application/octet-stream");
        let filename = att
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("attachment");

        // ── Native mode: read local file directly ────────────────────────────
        if self.is_native {
            // signal-cli stores downloaded attachments in {data_dir}/attachments/{id}
            // The SSE event provides the `id` field which is the filename.
            let file_path = att
                .get("file")
                .or_else(|| att.get("storedFilename"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    // Construct path from data_dir + attachments/ + id
                    let id = att.get("id").and_then(|v| v.as_str())?;
                    let dir = self.data_dir.as_deref()?;
                    Some(format!("{}/attachments/{}", dir, id))
                })?;

            let path = std::path::PathBuf::from(&file_path);
            if !path.exists() {
                tracing::warn!("Signal native: attachment file not found: {file_path}");
                return None;
            }

            let bytes = std::fs::read(&path).ok()?;
            // Copy to a temp path with a proper extension so media pipelines
            // can identify the format by filename.
            let id = att.get("id").and_then(|v| v.as_str()).unwrap_or("0");
            // Avoid double extension: if id already has the right extension, use it as-is
            let ext = mime_to_extension(content_type);
            let temp_path = if id.ends_with(&format!(".{ext}")) {
                format!("/tmp/openprx-att-{id}")
            } else {
                format!("/tmp/openprx-att-{id}.{ext}")
            };
            std::fs::write(&temp_path, &bytes).ok()?;

            tracing::info!(
                "Signal native: attachment {file_path} ({} bytes) → {temp_path}",
                bytes.len()
            );

            return Self::make_attachment_marker(
                &temp_path,
                content_type,
                filename,
                &self.media_config,
            )
            .await;
        }

        // ── REST mode: download via signal-cli-rest-api ───────────────────────
        let id = att.get("id").and_then(|v| v.as_str())?;
        let url = format!("{}/v1/attachments/{}", self.http_url, id);
        let response = self
            .http_client()
            .get(&url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .ok()?;

        if !response.status().is_success() {
            tracing::warn!(
                "Signal: failed to download attachment {id}: {}",
                response.status()
            );
            return None;
        }

        let bytes = response.bytes().await.ok()?;
        let ext = mime_to_extension(content_type);
        let temp_path = format!("/tmp/openprx-att-{id}.{ext}");
        std::fs::write(&temp_path, &bytes).ok()?;

        tracing::info!(
            "Signal: downloaded attachment {id} ({} bytes) → {}",
            bytes.len(),
            temp_path
        );

        Self::make_attachment_marker(&temp_path, content_type, filename, &self.media_config).await
    }

    /// Convert a locally-stored attachment file into the appropriate content marker.
    async fn make_attachment_marker(
        temp_path: &str,
        content_type: &str,
        filename: &str,
        media_config: &crate::config::MediaConfig,
    ) -> Option<String> {
        // Images: keep raw [IMAGE:] marker for the existing multimodal pipeline
        if content_type.starts_with("image/") {
            return Some(format!("[IMAGE:{temp_path}]"));
        }

        // Audio: attempt STT transcription via media engine
        if content_type.starts_with("audio/") {
            if let Some(transcription) =
                crate::media::process_media_attachment(temp_path, content_type, media_config).await
            {
                return Some(format!(
                    "[Voice message transcription: \"{transcription}\"]"
                ));
            }
            return Some(format!(
                "<media:audio path=\"{temp_path}\" type=\"{content_type}\" name=\"{filename}\">"
            ));
        }

        // Video: attempt frame extraction via media engine
        if content_type.starts_with("video/") {
            if let Some(frames) =
                crate::media::process_media_attachment(temp_path, content_type, media_config).await
            {
                return Some(frames);
            }
            return Some(format!(
                "<media:video path=\"{temp_path}\" type=\"{content_type}\" name=\"{filename}\">"
            ));
        }

        // Documents: attempt text extraction
        if let Some(text) = extract_document_text(temp_path, content_type, filename).await {
            let truncated = if text.len() > 8000 {
                format!(
                    "{}...\n[truncated, {} total chars]",
                    &text[..8000],
                    text.len()
                )
            } else {
                text
            };
            return Some(format!("[Document: {filename}]\n{truncated}\n[/Document]"));
        }

        // Unrecognised file types: pass as media marker
        Some(format!(
            "<media:file path=\"{temp_path}\" type=\"{content_type}\" name=\"{filename}\">"
        ))
    }

    /// Enrich a `ChannelMessage` with `[IMAGE:...]` or `<media:...>` markers
    /// by downloading any attachments from the original envelope.
    /// Returns the message unchanged when `ignore_attachments` is set.
    async fn maybe_enrich_with_attachments(
        &self,
        mut msg: ChannelMessage,
        envelope: &Envelope,
    ) -> ChannelMessage {
        if self.ignore_attachments {
            return msg;
        }

        let Some(data_msg) = envelope.data_message.as_ref() else {
            return msg;
        };

        let Some(raw_attachments) = data_msg.attachments.as_ref() else {
            return msg;
        };

        if raw_attachments.is_empty() {
            return msg;
        }

        for att in raw_attachments {
            if let Some(marker) = self.download_attachment_as_marker(att).await {
                msg.content.push('\n');
                msg.content.push_str(&marker);
            }
        }

        msg
    }

    /// Send an emoji reaction to a specific message.
    pub async fn send_reaction(
        &self,
        recipient: &str,
        emoji: &str,
        target_author: &str,
        timestamp: u64,
    ) -> anyhow::Result<()> {
        // ── Native mode: JSON-RPC sendReaction ───────────────────────────────
        if self.is_native {
            let params = match Self::parse_recipient_target(recipient) {
                RecipientTarget::Direct(number) => serde_json::json!({
                    "recipient": [number],
                    "emoji": emoji,
                    "targetAuthor": target_author,
                    "targetTimestamp": timestamp,
                }),
                RecipientTarget::Group(group_id) => serde_json::json!({
                    "groupId": group_id,
                    "emoji": emoji,
                    "targetAuthor": target_author,
                    "targetTimestamp": timestamp,
                }),
            };
            self.rpc_request("sendReaction", params).await?;
            return Ok(());
        }

        // ── REST mode: PUT /v1/reactions/{account} ────────────────────────────
        let url = format!("{}/v1/reactions/{}", self.http_url, self.account);

        let body = match Self::parse_recipient_target(recipient) {
            RecipientTarget::Direct(number) => serde_json::json!({
                "recipient": number,
                "reaction": emoji,
                "target_author": target_author,
                "timestamp": timestamp
            }),
            RecipientTarget::Group(group_id) => serde_json::json!({
                "recipient": format!("{GROUP_TARGET_PREFIX}{group_id}"),
                "reaction": emoji,
                "target_author": target_author,
                "timestamp": timestamp
            }),
        };

        let resp = self
            .http_client()
            .put(&url)
            .timeout(Duration::from_secs(10))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!("Signal reaction failed: {status} - {body}");
        }

        Ok(())
    }

    /// Poll-based listener for signal-cli-rest-api `/v1/receive/{account}`.
    async fn listen_polling(
        &self,
        poll_url: &str,
        tx: mpsc::Sender<ChannelMessage>,
    ) -> anyhow::Result<()> {
        let poll_interval = Duration::from_secs(2);
        let mut retry_delay_secs = 2u64;
        let max_delay_secs = 60u64;

        loop {
            let resp = self
                .http_client()
                .get(poll_url)
                .timeout(Duration::from_secs(30))
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    retry_delay_secs = 2;
                    let text = r.text().await.unwrap_or_default();
                    if text.is_empty() || text == "[]" {
                        tokio::time::sleep(poll_interval).await;
                        continue;
                    }

                    // REST API returns an array of envelopes
                    if let Ok(envelopes) = serde_json::from_str::<Vec<SseEnvelope>>(&text) {
                        for sse in &envelopes {
                            if let Some(ref envelope) = sse.envelope {
                                if let Some(msg) = self.process_envelope(envelope) {
                                    let msg =
                                        self.maybe_enrich_with_attachments(msg, envelope).await;
                                    if tx.send(msg).await.is_err() {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    } else {
                        tracing::debug!("Signal poll parse skip: {text}");
                    }
                }
                Ok(r) => {
                    let status = r.status();
                    tracing::warn!("Signal poll returned {status}, retrying...");
                    tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                    retry_delay_secs = (retry_delay_secs * 2).min(max_delay_secs);
                }
                Err(e) => {
                    tracing::warn!("Signal poll error: {e}, retrying...");
                    tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                    retry_delay_secs = (retry_delay_secs * 2).min(max_delay_secs);
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// SSE-based listener for signal-cli daemon `/api/v1/events`.
    async fn listen_sse(
        &self,
        sse_url: &str,
        tx: mpsc::Sender<ChannelMessage>,
    ) -> anyhow::Result<()> {
        let url = reqwest::Url::parse(sse_url)?;

        let mut retry_delay_secs = 2u64;
        let max_delay_secs = 60u64;

        loop {
            let resp = self
                .http_client()
                .get(url.clone())
                .header("Accept", "text/event-stream")
                .send()
                .await;

            let resp = match resp {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    tracing::warn!("Signal SSE returned {status}: {body}");
                    tokio::time::sleep(tokio::time::Duration::from_secs(retry_delay_secs)).await;
                    retry_delay_secs = (retry_delay_secs * 2).min(max_delay_secs);
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Signal SSE connect error: {e}, retrying...");
                    tokio::time::sleep(tokio::time::Duration::from_secs(retry_delay_secs)).await;
                    retry_delay_secs = (retry_delay_secs * 2).min(max_delay_secs);
                    continue;
                }
            };

            retry_delay_secs = 2;

            let mut bytes_stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut current_data = String::new();

            while let Some(chunk) = bytes_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::debug!("Signal SSE chunk error, reconnecting: {e}");
                        break;
                    }
                };

                let text = match String::from_utf8(chunk.to_vec()) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!("Signal SSE invalid UTF-8, skipping chunk: {}", e);
                        continue;
                    }
                };

                buffer.push_str(&text);

                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    if line.starts_with(':') {
                        continue;
                    }

                    if line.is_empty() {
                        if !current_data.is_empty() {
                            match serde_json::from_str::<SseEnvelope>(&current_data) {
                                Ok(sse) => {
                                    if let Some(ref envelope) = sse.envelope {
                                        if let Some(msg) = self.process_envelope(envelope) {
                                            let msg = self
                                                .maybe_enrich_with_attachments(msg, envelope)
                                                .await;
                                            if tx.send(msg).await.is_err() {
                                                return Ok(());
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!("Signal SSE parse skip: {e}");
                                }
                            }
                            current_data.clear();
                        }
                    } else if let Some(data) = line.strip_prefix("data:") {
                        if !current_data.is_empty() {
                            current_data.push('\n');
                        }
                        current_data.push_str(data.trim_start());
                    }
                }
            }

            if !current_data.is_empty() {
                if let Ok(sse) = serde_json::from_str::<SseEnvelope>(&current_data) {
                    if let Some(ref envelope) = sse.envelope {
                        if let Some(msg) = self.process_envelope(envelope) {
                            let msg = self.maybe_enrich_with_attachments(msg, envelope).await;
                            let _ = tx.send(msg).await;
                        }
                    }
                }
            }

            tracing::debug!("Signal SSE stream ended, reconnecting...");
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
    }
}

#[async_trait]
impl Channel for SignalChannel {
    fn name(&self) -> &str {
        "signal"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        // Parse any media markers embedded in the message content
        let (clean_text, media_items) = extract_outgoing_media(&message.content);
        let text_content = &clean_text;

        // ── Native mode: JSON-RPC send ────────────────────────────────────────
        tracing::info!(
            "Signal send: is_native={} http_url={}",
            self.is_native,
            self.http_url
        );
        if self.is_native {
            // In native mode, attachments are referenced as absolute paths.
            // signal-cli JSON-RPC expects plain absolute paths, not file:// URIs.
            let file_attachments: Vec<String> = media_items
                .iter()
                .filter_map(|(_kind, path)| {
                    if std::path::Path::new(path).exists() {
                        Some(path.clone())
                    } else {
                        tracing::warn!("Signal native: attachment file missing: {path}");
                        None
                    }
                })
                .collect();

            let mut params = match Self::parse_recipient_target(&message.recipient) {
                RecipientTarget::Direct(number) => serde_json::json!({
                    "recipient": [number],
                    "message": text_content,
                }),
                RecipientTarget::Group(group_id) => serde_json::json!({
                    "groupId": group_id,
                    "message": text_content,
                }),
            };

            if !file_attachments.is_empty() {
                params["attachment"] = serde_json::json!(file_attachments);
            }

            // Quote/reply support
            if let (Some(ts), Some(author)) = (&message.quote_timestamp, &message.quote_author) {
                params["quoteTimestamp"] = serde_json::json!(ts);
                params["quoteAuthor"] = serde_json::json!(author);
            }

            self.rpc_request("send", params).await?;
            return Ok(());
        }

        // ── REST mode: POST /v2/send ──────────────────────────────────────────
        // Build base64 attachments from media markers
        let mut base64_attachments: Vec<String> = Vec::new();
        for (kind, path) in &media_items {
            match std::fs::read(path) {
                Ok(bytes) => {
                    let mime: &str = match kind.as_str() {
                        "IMAGE" => guess_mime_from_path(path),
                        "VOICE" | "AUDIO" => guess_audio_mime(path),
                        "VIDEO" => "video/mp4",
                        _ => "application/octet-stream",
                    };
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    base64_attachments.push(format!("data:{mime};base64,{b64}"));
                }
                Err(e) => {
                    tracing::warn!("Failed to read attachment {path}: {e}");
                }
            }
        }

        let rest_url = format!("{}/v2/send", self.http_url);
        let mut body = match Self::parse_recipient_target(&message.recipient) {
            RecipientTarget::Direct(number) => serde_json::json!({
                "number": &self.account,
                "recipients": [number],
            }),
            RecipientTarget::Group(group_id) => serde_json::json!({
                "number": &self.account,
                "recipients": [format!("{GROUP_TARGET_PREFIX}{group_id}")],
            }),
        };

        // Only include message field if there's text to send
        if !clean_text.is_empty() {
            body["message"] = serde_json::Value::String(clean_text.clone());
        } else if base64_attachments.is_empty() {
            body["message"] = serde_json::Value::String(message.content.clone());
        }

        if !base64_attachments.is_empty() {
            body["base64_attachments"] = serde_json::json!(base64_attachments);
        }

        // Add quote/reply fields if reply context is present
        if let (Some(ts), Some(author)) = (&message.quote_timestamp, &message.quote_author) {
            body["quote_timestamp"] = serde_json::json!(ts);
            body["quote_author"] = serde_json::json!(author);
        }

        let resp = self
            .http_client()
            .post(&rest_url)
            .timeout(Duration::from_secs(30))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 201 => return Ok(()),
            Ok(r) => {
                let status = r.status();
                let body_text = r.text().await.unwrap_or_default();
                tracing::warn!("Signal REST send failed: {status} - {body_text}");
                // Fallback to JSON-RPC for text-only messages
                if base64_attachments.is_empty() {
                    let params = match Self::parse_recipient_target(&message.recipient) {
                        RecipientTarget::Direct(number) => serde_json::json!({
                            "recipient": [number],
                            "message": text_content,
                            "account": &self.account,
                        }),
                        RecipientTarget::Group(group_id) => serde_json::json!({
                            "groupId": group_id,
                            "message": text_content,
                            "account": &self.account,
                        }),
                    };
                    self.rpc_request("send", params).await?;
                }
            }
            Err(e) => {
                tracing::warn!("Signal REST send error: {e}");
                if base64_attachments.is_empty() {
                    let params = match Self::parse_recipient_target(&message.recipient) {
                        RecipientTarget::Direct(number) => serde_json::json!({
                            "recipient": [number],
                            "message": text_content,
                            "account": &self.account,
                        }),
                        RecipientTarget::Group(group_id) => serde_json::json!({
                            "groupId": group_id,
                            "message": text_content,
                            "account": &self.account,
                        }),
                    };
                    self.rpc_request("send", params).await?;
                }
            }
        }
        Ok(())
    }

    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        // Native daemon mode: always use SSE — no REST polling endpoint available.
        if self.is_native {
            let sse_url = format!("{}/api/v1/events", self.http_url);
            tracing::info!("Signal native: listening via SSE on {sse_url}");
            return self.listen_sse(&sse_url, tx).await;
        }

        // REST mode: probe for signal-cli-rest-api, fall back to SSE.
        let poll_url = format!("{}/v1/receive/{}", self.http_url, self.account);
        let sse_url_str = format!("{}/api/v1/events?account={}", self.http_url, self.account);

        let use_polling = {
            let probe = self
                .http_client()
                .get(&format!("{}/v1/about", self.http_url))
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            probe.is_ok_and(|r| r.status().is_success())
        };

        if use_polling {
            tracing::info!("Signal channel using REST polling on {}...", self.http_url);
            self.listen_polling(&poll_url, tx).await
        } else {
            tracing::info!("Signal channel using SSE on {}...", self.http_url);
            self.listen_sse(&sse_url_str, tx).await
        }
    }
    async fn health_check(&self) -> bool {
        let url = format!("{}/api/v1/check", self.http_url);
        let Ok(resp) = self
            .http_client()
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        else {
            return false;
        };
        resp.status().is_success()
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        let params = match Self::parse_recipient_target(recipient) {
            RecipientTarget::Direct(number) => serde_json::json!({
                "recipient": [number],
                "account": &self.account,
            }),
            RecipientTarget::Group(group_id) => serde_json::json!({
                "groupId": group_id,
                "account": &self.account,
            }),
        };
        self.rpc_request("sendTyping", params).await?;
        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        // signal-cli doesn't have a stop-typing RPC; typing indicators
        // auto-expire after ~15s on the client side.
        Ok(())
    }

    // ── P3-2: Extended channel actions ──────────────────────────────────────

    fn capabilities(&self) -> crate::channels::traits::ChannelCapabilities {
        crate::channels::traits::ChannelCapabilities {
            edit: false,   // signal-cli does not support editing sent messages
            delete: true,  // supported via remoteDelete RPC
            thread: false, // Signal has no native thread concept; degrades to quote reply
            react: true,   // supported via sendReaction RPC
        }
    }

    /// Delete a sent message via Signal's `remoteDelete` RPC.
    ///
    /// `channel_id` is the recipient (E.164 phone, UUID, or `group:<id>`).
    /// `message_id` is the *timestamp* (in ms) of the message to delete, as a decimal string.
    async fn delete_message(&self, channel_id: &str, message_id: &str) -> anyhow::Result<()> {
        let ts: u64 = message_id
            .parse()
            .map_err(|_| anyhow::anyhow!("message_id must be a numeric timestamp (ms)"))?;

        // Native mode: JSON-RPC `remoteDelete`
        if self.is_native {
            let params = match Self::parse_recipient_target(channel_id) {
                RecipientTarget::Direct(number) => serde_json::json!({
                    "account": &self.account,
                    "recipient": [number],
                    "targetTimestamp": ts,
                }),
                RecipientTarget::Group(group_id) => serde_json::json!({
                    "account": &self.account,
                    "groupId": group_id,
                    "targetTimestamp": ts,
                }),
            };
            self.rpc_request("remoteDelete", params).await?;
            return Ok(());
        }

        // REST mode: DELETE /v1/messages/{account}
        let url = format!("{}/v1/messages/{}", self.http_url, self.account);
        let body = match Self::parse_recipient_target(channel_id) {
            RecipientTarget::Direct(number) => serde_json::json!({
                "recipient": number,
                "timestamp": ts,
            }),
            RecipientTarget::Group(group_id) => serde_json::json!({
                "recipient": format!("{GROUP_TARGET_PREFIX}{group_id}"),
                "timestamp": ts,
            }),
        };

        let resp = self
            .http_client()
            .delete(&url)
            .timeout(Duration::from_secs(10))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Signal remoteDelete failed [{status}]: {text}");
        }
        Ok(())
    }

    /// Send a reply within a "thread".
    ///
    /// Signal has no native thread concept. This implementation degrades gracefully
    /// by sending a quote reply to the `thread_id` timestamp.
    ///
    /// `channel_id` is the recipient.
    /// `thread_id` is the timestamp (ms) of the original message to quote-reply to.
    /// `message` is the reply text.
    async fn send_thread_reply(
        &self,
        channel_id: &str,
        thread_id: &str,
        message: &str,
    ) -> anyhow::Result<()> {
        let ts: u64 = thread_id
            .parse()
            .map_err(|_| anyhow::anyhow!("thread_id must be a numeric timestamp (ms)"))?;

        let mut msg = crate::channels::traits::SendMessage::new(message, channel_id);
        msg.quote_timestamp = Some(ts);
        msg.quote_author = Some(self.account.clone());
        self.send(&msg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel() -> SignalChannel {
        SignalChannel::new(
            "http://127.0.0.1:8686".to_string(),
            "+1234567890".to_string(),
            None,
            vec!["+1111111111".to_string()],
            false,
            false,
            crate::config::MediaConfig::default(),
        )
    }

    fn make_channel_with_group(group_id: &str) -> SignalChannel {
        SignalChannel::new(
            "http://127.0.0.1:8686".to_string(),
            "+1234567890".to_string(),
            Some(group_id.to_string()),
            vec!["*".to_string()],
            true,
            true,
            crate::config::MediaConfig::default(),
        )
    }

    fn make_envelope(source_number: Option<&str>, message: Option<&str>) -> Envelope {
        Envelope {
            source: source_number.map(String::from),
            source_number: source_number.map(String::from),
            data_message: message.map(|m| DataMessage {
                message: Some(m.to_string()),
                timestamp: Some(1_700_000_000_000),
                group_info: None,
                attachments: None,
            }),
            story_message: None,
            timestamp: Some(1_700_000_000_000),
        }
    }

    #[test]
    fn creates_with_correct_fields() {
        let ch = make_channel();
        assert_eq!(ch.http_url, "http://127.0.0.1:8686");
        assert_eq!(ch.account, "+1234567890");
        assert!(ch.group_id.is_none());
        assert_eq!(ch.allowed_from.len(), 1);
        assert!(!ch.ignore_attachments);
        assert!(!ch.ignore_stories);
    }

    #[test]
    fn strips_trailing_slash() {
        let ch = SignalChannel::new(
            "http://127.0.0.1:8686/".to_string(),
            "+1234567890".to_string(),
            None,
            vec![],
            false,
            false,
            crate::config::MediaConfig::default(),
        );
        assert_eq!(ch.http_url, "http://127.0.0.1:8686");
    }

    #[test]
    fn wildcard_allows_anyone() {
        let ch = make_channel_with_group("dm");
        assert!(ch.is_sender_allowed("+9999999999"));
    }

    #[test]
    fn specific_sender_allowed() {
        let ch = make_channel();
        assert!(ch.is_sender_allowed("+1111111111"));
    }

    #[test]
    fn unknown_sender_denied() {
        let ch = make_channel();
        assert!(!ch.is_sender_allowed("+9999999999"));
    }

    #[test]
    fn empty_allowlist_denies_all() {
        let ch = SignalChannel::new(
            "http://127.0.0.1:8686".to_string(),
            "+1234567890".to_string(),
            None,
            vec![],
            false,
            false,
            crate::config::MediaConfig::default(),
        );
        assert!(!ch.is_sender_allowed("+1111111111"));
    }

    #[test]
    fn name_returns_signal() {
        let ch = make_channel();
        assert_eq!(ch.name(), "signal");
    }

    #[test]
    fn matches_group_no_group_id_accepts_all() {
        let ch = make_channel();
        let dm = DataMessage {
            message: Some("hi".to_string()),
            timestamp: Some(1000),
            group_info: None,
            attachments: None,
        };
        assert!(ch.matches_group(&dm));

        let group = DataMessage {
            message: Some("hi".to_string()),
            timestamp: Some(1000),
            group_info: Some(GroupInfo {
                group_id: Some("group123".to_string()),
                group_name: None,
            }),
            attachments: None,
        };
        assert!(ch.matches_group(&group));
    }

    #[test]
    fn matches_group_filters_group() {
        let ch = make_channel_with_group("group123");
        let matching = DataMessage {
            message: Some("hi".to_string()),
            timestamp: Some(1000),
            group_info: Some(GroupInfo {
                group_id: Some("group123".to_string()),
                group_name: None,
            }),
            attachments: None,
        };
        assert!(ch.matches_group(&matching));

        let non_matching = DataMessage {
            message: Some("hi".to_string()),
            timestamp: Some(1000),
            group_info: Some(GroupInfo {
                group_id: Some("other_group".to_string()),
                group_name: None,
            }),
            attachments: None,
        };
        assert!(!ch.matches_group(&non_matching));
    }

    #[test]
    fn matches_group_dm_keyword() {
        let ch = make_channel_with_group("dm");
        let dm = DataMessage {
            message: Some("hi".to_string()),
            timestamp: Some(1000),
            group_info: None,
            attachments: None,
        };
        assert!(ch.matches_group(&dm));

        let group = DataMessage {
            message: Some("hi".to_string()),
            timestamp: Some(1000),
            group_info: Some(GroupInfo {
                group_id: Some("group123".to_string()),
                group_name: None,
            }),
            attachments: None,
        };
        assert!(!ch.matches_group(&group));
    }

    #[test]
    fn reply_target_dm() {
        let ch = make_channel();
        let dm = DataMessage {
            message: Some("hi".to_string()),
            timestamp: Some(1000),
            group_info: None,
            attachments: None,
        };
        assert_eq!(ch.reply_target(&dm, "+1111111111"), "+1111111111");
    }

    #[test]
    fn reply_target_group() {
        let ch = make_channel();
        let group = DataMessage {
            message: Some("hi".to_string()),
            timestamp: Some(1000),
            group_info: Some(GroupInfo {
                group_id: Some("group123".to_string()),
                group_name: None,
            }),
            attachments: None,
        };
        assert_eq!(ch.reply_target(&group, "+1111111111"), "group:group123");
    }

    #[test]
    fn parse_recipient_target_e164_is_direct() {
        assert_eq!(
            SignalChannel::parse_recipient_target("+1234567890"),
            RecipientTarget::Direct("+1234567890".to_string())
        );
    }

    #[test]
    fn parse_recipient_target_prefixed_group_is_group() {
        assert_eq!(
            SignalChannel::parse_recipient_target("group:abc123"),
            RecipientTarget::Group("abc123".to_string())
        );
    }

    #[test]
    fn parse_recipient_target_uuid_is_direct() {
        let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        assert_eq!(
            SignalChannel::parse_recipient_target(uuid),
            RecipientTarget::Direct(uuid.to_string())
        );
    }

    #[test]
    fn parse_recipient_target_non_e164_plus_is_group() {
        assert_eq!(
            SignalChannel::parse_recipient_target("+abc123"),
            RecipientTarget::Group("+abc123".to_string())
        );
    }

    #[test]
    fn is_uuid_valid() {
        assert!(SignalChannel::is_uuid(
            "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
        ));
        assert!(SignalChannel::is_uuid(
            "00000000-0000-0000-0000-000000000000"
        ));
    }

    #[test]
    fn is_uuid_invalid() {
        assert!(!SignalChannel::is_uuid("+1234567890"));
        assert!(!SignalChannel::is_uuid("not-a-uuid"));
        assert!(!SignalChannel::is_uuid("group:abc123"));
        assert!(!SignalChannel::is_uuid(""));
    }

    #[test]
    fn sender_prefers_source_number() {
        let env = Envelope {
            source: Some("uuid-123".to_string()),
            source_number: Some("+1111111111".to_string()),
            data_message: None,
            story_message: None,
            timestamp: Some(1000),
        };
        assert_eq!(SignalChannel::sender(&env), Some("+1111111111".to_string()));
    }

    #[test]
    fn sender_falls_back_to_source() {
        let env = Envelope {
            source: Some("uuid-123".to_string()),
            source_number: None,
            data_message: None,
            story_message: None,
            timestamp: Some(1000),
        };
        assert_eq!(SignalChannel::sender(&env), Some("uuid-123".to_string()));
    }

    #[test]
    fn process_envelope_uuid_sender_dm() {
        let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let ch = SignalChannel::new(
            "http://127.0.0.1:8686".to_string(),
            "+1234567890".to_string(),
            None,
            vec!["*".to_string()],
            false,
            false,
            crate::config::MediaConfig::default(),
        );
        let env = Envelope {
            source: Some(uuid.to_string()),
            source_number: None,
            data_message: Some(DataMessage {
                message: Some("Hello from privacy user".to_string()),
                timestamp: Some(1_700_000_000_000),
                group_info: None,
                attachments: None,
            }),
            story_message: None,
            timestamp: Some(1_700_000_000_000),
        };
        let msg = ch.process_envelope(&env).unwrap();
        assert_eq!(msg.sender, uuid);
        assert_eq!(msg.reply_target, uuid);
        assert!(
            msg.content.starts_with("Hello from privacy user"),
            "content: {}",
            msg.content
        );
        assert!(
            msg.content.contains(&format!("[signal-meta sender={uuid}")),
            "content: {}",
            msg.content
        );
        assert!(
            msg.content.contains("chat_type=direct"),
            "content: {}",
            msg.content
        );

        // Verify reply routing: UUID sender in DM should route as Direct
        let target = SignalChannel::parse_recipient_target(&msg.reply_target);
        assert_eq!(target, RecipientTarget::Direct(uuid.to_string()));
    }

    #[test]
    fn process_envelope_uuid_sender_in_group() {
        let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let ch = SignalChannel::new(
            "http://127.0.0.1:8686".to_string(),
            "+1234567890".to_string(),
            Some("testgroup".to_string()),
            vec!["*".to_string()],
            false,
            false,
            crate::config::MediaConfig::default(),
        );
        let env = Envelope {
            source: Some(uuid.to_string()),
            source_number: None,
            data_message: Some(DataMessage {
                message: Some("Group msg from privacy user".to_string()),
                timestamp: Some(1_700_000_000_000),
                group_info: Some(GroupInfo {
                    group_id: Some("testgroup".to_string()),
                    group_name: Some("Test Group".to_string()),
                }),
                attachments: None,
            }),
            story_message: None,
            timestamp: Some(1_700_000_000_000),
        };
        let msg = ch.process_envelope(&env).unwrap();
        assert_eq!(msg.sender, uuid);
        assert_eq!(msg.reply_target, "group:testgroup");
        assert!(
            msg.content
                .starts_with(&format!("[Signal Group: Test Group] {uuid}: Group msg from privacy user")),
            "content: {}",
            msg.content
        );
        assert!(
            msg.content.contains("group=Test Group chat_type=group"),
            "content: {}",
            msg.content
        );

        // Verify reply routing: group message should still route as Group
        let target = SignalChannel::parse_recipient_target(&msg.reply_target);
        assert_eq!(target, RecipientTarget::Group("testgroup".to_string()));
    }

    #[test]
    fn sender_none_when_both_missing() {
        let env = Envelope {
            source: None,
            source_number: None,
            data_message: None,
            story_message: None,
            timestamp: None,
        };
        assert_eq!(SignalChannel::sender(&env), None);
    }

    #[test]
    fn process_envelope_valid_dm() {
        let ch = make_channel();
        let env = make_envelope(Some("+1111111111"), Some("Hello!"));
        let msg = ch.process_envelope(&env).unwrap();
        assert!(
            msg.content.starts_with("Hello!"),
            "content: {}",
            msg.content
        );
        assert!(
            msg.content.contains("[signal-meta sender=+1111111111"),
            "content: {}",
            msg.content
        );
        assert!(
            msg.content.contains("chat_type=direct"),
            "content: {}",
            msg.content
        );
        assert_eq!(msg.sender, "+1111111111");
        assert_eq!(msg.channel, "signal");
    }

    #[test]
    fn process_envelope_denied_sender() {
        let ch = make_channel();
        let env = make_envelope(Some("+9999999999"), Some("Hello!"));
        assert!(ch.process_envelope(&env).is_none());
    }

    #[test]
    fn process_envelope_empty_message() {
        let ch = make_channel();
        let env = make_envelope(Some("+1111111111"), Some(""));
        assert!(ch.process_envelope(&env).is_none());
    }

    #[test]
    fn process_envelope_no_data_message() {
        let ch = make_channel();
        let env = make_envelope(Some("+1111111111"), None);
        assert!(ch.process_envelope(&env).is_none());
    }

    #[test]
    fn process_envelope_skips_stories() {
        let ch = make_channel_with_group("dm");
        let mut env = make_envelope(Some("+1111111111"), Some("story text"));
        env.story_message = Some(serde_json::json!({}));
        assert!(ch.process_envelope(&env).is_none());
    }

    #[test]
    fn process_envelope_skips_attachment_only() {
        let ch = make_channel_with_group("dm");
        let env = Envelope {
            source: Some("+1111111111".to_string()),
            source_number: Some("+1111111111".to_string()),
            data_message: Some(DataMessage {
                message: None,
                timestamp: Some(1_700_000_000_000),
                group_info: None,
                attachments: Some(vec![serde_json::json!({"contentType": "image/png"})]),
            }),
            story_message: None,
            timestamp: Some(1_700_000_000_000),
        };
        assert!(ch.process_envelope(&env).is_none());
    }

    #[test]
    fn sse_envelope_deserializes() {
        let json = r#"{
            "envelope": {
                "source": "+1111111111",
                "sourceNumber": "+1111111111",
                "timestamp": 1700000000000,
                "dataMessage": {
                    "message": "Hello Signal!",
                    "timestamp": 1700000000000
                }
            }
        }"#;
        let sse: SseEnvelope = serde_json::from_str(json).unwrap();
        let env = sse.envelope.unwrap();
        assert_eq!(env.source_number.as_deref(), Some("+1111111111"));
        let dm = env.data_message.unwrap();
        assert_eq!(dm.message.as_deref(), Some("Hello Signal!"));
    }

    #[test]
    fn sse_envelope_deserializes_group() {
        let json = r#"{
            "envelope": {
                "sourceNumber": "+2222222222",
                "dataMessage": {
                    "message": "Group msg",
                    "groupInfo": {
                        "groupId": "abc123",
                        "name": "Signal Crew"
                    }
                }
            }
        }"#;
        let sse: SseEnvelope = serde_json::from_str(json).unwrap();
        let env = sse.envelope.unwrap();
        let dm = env.data_message.unwrap();
        assert_eq!(
            dm.group_info.as_ref().unwrap().group_id.as_deref(),
            Some("abc123")
        );
        assert_eq!(
            dm.group_info.as_ref().unwrap().group_name.as_deref(),
            Some("Signal Crew")
        );
    }

    #[test]
    fn envelope_defaults() {
        let json = r#"{}"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        assert!(env.source.is_none());
        assert!(env.source_number.is_none());
        assert!(env.data_message.is_none());
        assert!(env.story_message.is_none());
        assert!(env.timestamp.is_none());
    }
}
