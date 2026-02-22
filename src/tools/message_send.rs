//! Message send tool — lets the LLM proactively send messages through channels.
//!
//! Aligns with OpenClaw's `message` tool:
//! - Send text messages to specific recipients
//! - Send files/images/voice as attachments via `[IMAGE:]`, `[VOICE:]`, `[DOCUMENT:]` markers
//! - Send emoji reactions (Signal-specific, falls back to error on unsupported channels)
//! - Quote reply to specific messages

use super::traits::{Tool, ToolResult};
use crate::channels::traits::{Channel, SendMessage};
use crate::channels::SignalChannel;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Auto-generate a voice file from text using edge-tts + ffmpeg.
///
/// Returns the path to an M4A (AAC) file that can be embedded as `[VOICE:/tmp/…m4a]`.
/// The caller is responsible for cleaning up the file after sending.
pub(crate) async fn auto_generate_voice(text: &str, voice: &str) -> anyhow::Result<String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mp3_path = format!("/tmp/zeroclaw-tts-{id}.mp3");
    let m4a_path = format!("/tmp/zeroclaw-tts-{id}.m4a");

    // Sanitise text for embedding inside a JS single-quoted string.
    let safe_text = text.replace('\\', "\\\\").replace('\'', "\\'").replace('\n', " ").replace('\r', "");

    // 1. Generate MP3 with node-edge-tts
    let tts_script = format!(
        r#"const{{EdgeTTS}}=require('node-edge-tts');new EdgeTTS().ttsPromise('{safe_text}','{mp3_path}',{{voice:'{voice}'}}).then(()=>console.log('ok')).catch(e=>{{console.error(String(e));process.exit(1)}})"#
    );

    // Resolve the global node_modules path so `require('node-edge-tts')` works.
    let npm_root_out = tokio::process::Command::new("npm")
        .args(["root", "-g"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("npm not found: {e}"))?;
    let node_modules = String::from_utf8_lossy(&npm_root_out.stdout).trim().to_string();

    let tts_out = tokio::process::Command::new("node")
        .args(["-e", &tts_script])
        .env("NODE_PATH", &node_modules)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("node not found: {e}"))?;

    if !tts_out.status.success() {
        let stderr = String::from_utf8_lossy(&tts_out.stderr).to_string();
        anyhow::bail!("edge-tts failed: {stderr}");
    }

    // 2. Convert MP3 → M4A (AAC) — Signal displays M4A as a playable voice note.
    let ffmpeg_out = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-i", &mp3_path, "-c:a", "aac", "-b:a", "64k", &m4a_path])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("ffmpeg not found: {e}"))?;

    if !ffmpeg_out.status.success() {
        let stderr = String::from_utf8_lossy(&ffmpeg_out.stderr).to_string();
        anyhow::bail!("ffmpeg conversion failed: {stderr}");
    }

    // 3. Clean up the intermediate MP3.
    let _ = tokio::fs::remove_file(&mp3_path).await;

    Ok(m4a_path)
}

pub struct MessageSendTool {
    /// Active channel — updated per-message via `set_active_channel` so that
    /// replies are always routed back on the same channel the message arrived on
    /// (e.g., wacli instead of signal for WhatsApp messages).
    /// Uses a `RwLock` identical to `TtsTool` for the same reason.
    active_channel: Arc<tokio::sync::RwLock<Arc<dyn Channel>>>,
    /// Optional Signal channel reference for reaction support.
    signal: Option<Arc<SignalChannel>>,
    /// Default recipient used when the LLM omits `target`.
    /// Stored in an `RwLock` so the gateway can update it per-message.
    default_recipient: Arc<tokio::sync::RwLock<Option<String>>>,
    security: Arc<SecurityPolicy>,
}

impl MessageSendTool {
    /// Create a new `MessageSendTool` backed by a generic channel.
    pub fn new(channel: Arc<dyn Channel>, security: Arc<SecurityPolicy>) -> Self {
        Self {
            active_channel: Arc::new(tokio::sync::RwLock::new(channel)),
            signal: None,
            default_recipient: Arc::new(tokio::sync::RwLock::new(None)),
            security,
        }
    }

    /// Create a new `MessageSendTool` backed by a Signal channel (enables reactions).
    pub fn new_signal(channel: Arc<SignalChannel>, security: Arc<SecurityPolicy>) -> Self {
        Self {
            active_channel: Arc::new(tokio::sync::RwLock::new(
                channel.clone() as Arc<dyn Channel>,
            )),
            signal: Some(channel),
            default_recipient: Arc::new(tokio::sync::RwLock::new(None)),
            security,
        }
    }

    /// Return a shareable handle to the default-recipient slot so callers can update
    /// it before each agent turn without replacing the tool registration.
    pub fn default_recipient_handle(&self) -> Arc<tokio::sync::RwLock<Option<String>>> {
        self.default_recipient.clone()
    }

    /// Convenience: update the default recipient from the current message's reply_target.
    pub async fn set_default_recipient(&self, recipient: Option<String>) {
        *self.default_recipient.write().await = recipient;
    }
}

#[async_trait]
impl Tool for MessageSendTool {
    fn name(&self) -> &str {
        "message_send"
    }

    fn description(&self) -> &str {
        "Send a message through the active messaging channel (Signal, Telegram, etc.). \
         Supports text, file/image/voice attachments, emoji reactions, and quote replies. \
         Use action='send' for messages and action='react' for emoji reactions."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["send", "react", "edit", "delete", "unsend", "thread"],
                    "description": "Action type: 'send' for text/files/voice, 'react' for emoji reactions, \
                                    'edit' to edit a sent message (message_id + message), \
                                    'delete'/'unsend' to delete a sent message (message_id), \
                                    'thread' to reply in a thread (thread_id + message)"
                },
                "target": {
                    "type": "string",
                    "description": "Recipient identifier (phone number, group ID, Signal UUID, etc.). \
                                    Defaults to the current conversation's sender when omitted."
                },
                "message": {
                    "type": "string",
                    "description": "Message text. Embed media by including markers: \
                                    [IMAGE:/path/to/file.png], [VOICE:/path/to/audio.m4a], \
                                    [DOCUMENT:/path/to/file.pdf]. Text outside markers is sent as caption."
                },
                "as_voice": {
                    "type": "boolean",
                    "description": "When true, the first [VOICE:] or [AUDIO:] attachment is sent as a voice note (default: false)."
                },
                "quote_timestamp": {
                    "type": "integer",
                    "description": "Timestamp (ms) of the message to quote-reply to."
                },
                "quote_author": {
                    "type": "string",
                    "description": "Author identifier of the message being replied to (required when quote_timestamp is set)."
                },
                "emoji": {
                    "type": "string",
                    "description": "For action='react': the emoji to react with, e.g. '👍', '❤️', '😂'."
                },
                "target_author": {
                    "type": "string",
                    "description": "For action='react': the author of the message to react to."
                },
                "target_timestamp": {
                    "type": "integer",
                    "description": "For action='react': the timestamp (ms) of the message to react to."
                },
                "message_id": {
                    "type": "string",
                    "description": "For action='edit'/'delete'/'unsend': the platform-specific message identifier (timestamp in ms for Signal)."
                },
                "thread_id": {
                    "type": "string",
                    "description": "For action='thread': the thread/conversation identifier to reply into."
                }
            },
            "required": ["action"]
        })
    }

    async fn set_active_recipient(&self, recipient: &str) {
        *self.default_recipient.write().await = Some(recipient.to_string());
    }

    async fn set_active_channel(&self, channel: Arc<dyn crate::channels::traits::Channel>) {
        *self.active_channel.write().await = channel;
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Security guard: autonomy check
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }
        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: rate limit exceeded".into()),
            });
        }

        let action = args["action"].as_str().unwrap_or("send");

        // Resolve the active channel (updated per-message via set_active_channel so that
        // replies are routed on the same channel the incoming message arrived on).
        let channel = self.active_channel.read().await.clone();

        // Resolve recipient: explicit arg takes priority, then default_recipient
        let default = self.default_recipient.read().await.clone();
        let target = args["target"]
            .as_str()
            .map(str::to_owned)
            .or(default);

        match action {
            "send" => {
                let recipient = match target {
                    Some(r) if !r.is_empty() => r,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Missing 'target': provide a recipient or ensure the conversation \
                                 context has a known sender."
                                    .into(),
                            ),
                        });
                    }
                };

                let raw_content = args["message"].as_str().unwrap_or("").to_owned();
                let as_voice = args["as_voice"].as_bool().unwrap_or(false);

                // Auto-TTS: when as_voice=true and the message is plain text (no [VOICE:] marker
                // already embedded), generate a voice file automatically so the LLM only needs
                // to set as_voice=true rather than orchestrating three separate steps.
                let mut auto_tts_path: Option<String> = None;
                let content = if as_voice
                    && !raw_content.is_empty()
                    && !raw_content.contains("[VOICE:")
                    && !raw_content.contains("<media:audio")
                {
                    let voice = "zh-CN-YunxiNeural";
                    match auto_generate_voice(&raw_content, voice).await {
                        Ok(voice_path) => {
                            tracing::info!("Auto-TTS: generated voice file at {voice_path}");
                            auto_tts_path = Some(voice_path.clone());
                            format!("[VOICE:{voice_path}]")
                        }
                        Err(e) => {
                            tracing::warn!("Auto-TTS failed ({e}), sending as plain text");
                            raw_content
                        }
                    }
                } else {
                    raw_content
                };

                let mut msg = SendMessage::new(content, &recipient);

                if let Some(ts) = args["quote_timestamp"].as_u64() {
                    msg.quote_timestamp = Some(ts);
                }
                if let Some(author) = args["quote_author"].as_str() {
                    msg.quote_author = Some(author.to_owned());
                }

                match channel.send(&msg).await {
                    Ok(()) => {
                        // Delayed cleanup for auto-generated TTS files (Bug 1 fix):
                        // signal-cli may read the file asynchronously after the RPC response,
                        // so we wait 30 s before deleting to avoid "file not found" errors.
                        if let Some(tts_path) = auto_tts_path {
                            tokio::spawn(async move {
                                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                                if let Err(e) = tokio::fs::remove_file(&tts_path).await {
                                    tracing::debug!(
                                        "auto-tts cleanup: could not remove {tts_path}: {e}"
                                    );
                                }
                            });
                        }
                        Ok(ToolResult {
                            success: true,
                            output: format!("Message sent to {recipient}"),
                            error: None,
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to send message: {e}")),
                    }),
                }
            }

            "react" => {
                let recipient = match target {
                    Some(r) if !r.is_empty() => r,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Missing 'target': provide a recipient for the reaction.".into(),
                            ),
                        });
                    }
                };

                let emoji = match args["emoji"].as_str() {
                    Some(e) if !e.is_empty() => e.to_owned(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing required 'emoji' parameter for react action.".into()),
                        })
                    }
                };

                let target_author = match args["target_author"].as_str() {
                    Some(a) if !a.is_empty() => a.to_owned(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Missing required 'target_author' parameter for react action.".into(),
                            ),
                        })
                    }
                };

                let target_timestamp = match args["target_timestamp"].as_u64() {
                    Some(ts) => ts,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Missing required 'target_timestamp' parameter for react action."
                                    .into(),
                            ),
                        })
                    }
                };

                match &self.signal {
                    Some(signal) => {
                        match signal
                            .send_reaction(&recipient, &emoji, &target_author, target_timestamp)
                            .await
                        {
                            Ok(()) => Ok(ToolResult {
                                success: true,
                                output: format!(
                                    "Reaction '{emoji}' sent to message from {target_author} \
                                     at {target_timestamp}"
                                ),
                                error: None,
                            }),
                            Err(e) => Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Failed to send reaction: {e}")),
                            }),
                        }
                    }
                    None => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(
                            "Reactions are not supported on this channel (Signal required).".into(),
                        ),
                    }),
                }
            }

            // ── edit ──────────────────────────────────────────────────────────
            "edit" => {
                let recipient = match target {
                    Some(r) if !r.is_empty() => r,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'target' for edit action.".into()),
                        });
                    }
                };
                let message_id = match args["message_id"].as_str() {
                    Some(id) if !id.is_empty() => id.to_owned(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'message_id' for edit action.".into()),
                        });
                    }
                };
                let new_text = args["message"].as_str().unwrap_or("");
                match channel.edit_message(&recipient, &message_id, new_text).await {
                    Ok(()) => Ok(ToolResult {
                        success: true,
                        output: format!("Message {message_id} edited."),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("edit_message failed: {e}")),
                    }),
                }
            }

            // ── delete / unsend ───────────────────────────────────────────────
            "delete" | "unsend" => {
                let recipient = match target {
                    Some(r) if !r.is_empty() => r,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'target' for delete/unsend action.".into()),
                        });
                    }
                };
                let message_id = match args["message_id"].as_str() {
                    Some(id) if !id.is_empty() => id.to_owned(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'message_id' for delete/unsend action.".into()),
                        });
                    }
                };
                match channel.delete_message(&recipient, &message_id).await {
                    Ok(()) => Ok(ToolResult {
                        success: true,
                        output: format!("Message {message_id} deleted."),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("delete_message failed: {e}")),
                    }),
                }
            }

            // ── thread ────────────────────────────────────────────────────────
            "thread" => {
                let recipient = match target {
                    Some(r) if !r.is_empty() => r,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'target' for thread action.".into()),
                        });
                    }
                };
                let thread_id = match args["thread_id"].as_str() {
                    Some(id) if !id.is_empty() => id.to_owned(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'thread_id' for thread action.".into()),
                        });
                    }
                };
                let message = args["message"].as_str().unwrap_or("");
                match channel.send_thread_reply(&recipient, &thread_id, message).await {
                    Ok(()) => Ok(ToolResult {
                        success: true,
                        output: format!("Thread reply sent to thread {thread_id}."),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("send_thread_reply failed: {e}")),
                    }),
                }
            }

            unknown => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{unknown}'. Use 'send', 'react', 'edit', 'delete', 'unsend', or 'thread'."
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use async_trait::async_trait;

    struct DummyChannel {
        pub sent: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    impl DummyChannel {
        fn new() -> (Arc<Self>, Arc<tokio::sync::Mutex<Vec<String>>>) {
            let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            (Arc::new(Self { sent: sent.clone() }), sent)
        }
    }

    #[async_trait]
    impl Channel for DummyChannel {
        fn name(&self) -> &str {
            "dummy"
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().await.push(message.content.clone());
            Ok(())
        }

        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn test_security(level: AutonomyLevel) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            max_actions_per_hour: 100,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn tool_name_and_description() {
        let (ch, _) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));
        assert_eq!(tool.name(), "message_send");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn parameters_schema_has_required_action() {
        let (ch, _) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("action")));
    }

    #[tokio::test]
    async fn send_action_delivers_message() {
        let (ch, sent) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

        let result = tool
            .execute(json!({
                "action": "send",
                "target": "+15551234567",
                "message": "Hello from ZeroClaw!"
            }))
            .await
            .unwrap();

        assert!(result.success, "Expected success, got: {:?}", result.error);
        let msgs = sent.lock().await;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "Hello from ZeroClaw!");
    }

    #[tokio::test]
    async fn send_uses_default_recipient_when_target_omitted() {
        let (ch, sent) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));
        tool.set_default_recipient(Some("+19998887777".to_string())).await;

        let result = tool
            .execute(json!({
                "action": "send",
                "message": "Using default recipient"
            }))
            .await
            .unwrap();

        assert!(result.success, "Expected success, got: {:?}", result.error);
        let msgs = sent.lock().await;
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn send_fails_without_target_and_no_default() {
        let (ch, _) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

        let result = tool
            .execute(json!({ "action": "send", "message": "no target" }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("target"));
    }

    #[tokio::test]
    async fn react_fails_without_signal_channel() {
        let (ch, _) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

        let result = tool
            .execute(json!({
                "action": "react",
                "target": "+15551234567",
                "emoji": "👍",
                "target_author": "+10001112222",
                "target_timestamp": 1_700_000_000_000u64
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Signal"));
    }

    #[tokio::test]
    async fn react_fails_missing_emoji() {
        let (ch, _) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

        let result = tool
            .execute(json!({
                "action": "react",
                "target": "+15551234567",
                "target_author": "+10001112222",
                "target_timestamp": 1_700_000_000_000u64
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("emoji"));
    }

    #[tokio::test]
    async fn unknown_action_returns_error() {
        let (ch, _) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

        let result = tool
            .execute(json!({ "action": "destroy" }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    #[tokio::test]
    async fn execute_blocks_readonly_mode() {
        let (ch, _) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::ReadOnly));

        let result = tool
            .execute(json!({
                "action": "send",
                "target": "+15551234567",
                "message": "test"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn execute_blocks_rate_limit() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            max_actions_per_hour: 0,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let (ch, _) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, security);

        let result = tool
            .execute(json!({
                "action": "send",
                "target": "+15551234567",
                "message": "rate limited"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn default_recipient_handle_allows_external_update() {
        let (ch, sent) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));
        let handle = tool.default_recipient_handle();

        // Update via handle (as the gateway would do per-message)
        *handle.write().await = Some("+19998887777".to_string());

        let result = tool
            .execute(json!({ "action": "send", "message": "via handle" }))
            .await
            .unwrap();

        assert!(result.success, "Expected success, got: {:?}", result.error);
        let msgs = sent.lock().await;
        assert_eq!(msgs.len(), 1);
    }

    /// Regression test: verify that set_active_channel routes messages through the new channel
    /// instead of the original channel. This covers the bug where WhatsApp messages were
    /// routed through Signal because MessageSendTool used a fixed channel field.
    #[tokio::test]
    async fn set_active_channel_routes_to_new_channel() {
        let (original_ch, original_sent) = DummyChannel::new();
        let tool = MessageSendTool::new(original_ch, test_security(AutonomyLevel::Full));
        tool.set_active_recipient("+15551234567").await;

        // First send goes to original channel
        let _ = tool
            .execute(json!({ "action": "send", "message": "first" }))
            .await
            .unwrap();
        assert_eq!(original_sent.lock().await.len(), 1, "first send should go to original channel");

        // Simulate wacli message arriving: gateway switches active channel
        let (new_ch, new_sent) = DummyChannel::new();
        let new_ch_arc: Arc<dyn crate::channels::traits::Channel> = new_ch;
        tool.set_active_channel(new_ch_arc).await;

        // Second send should now go to the new channel, not the original
        let result = tool
            .execute(json!({ "action": "send", "message": "second" }))
            .await
            .unwrap();
        assert!(result.success, "Expected success on second send: {:?}", result.error);
        assert_eq!(original_sent.lock().await.len(), 1, "original channel should still have only 1 message");
        assert_eq!(new_sent.lock().await.len(), 1, "new channel should have received the second message");
    }
}
