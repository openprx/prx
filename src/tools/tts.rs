//! TTS (Text-To-Speech) tool — converts text to a voice message and sends it.
//!
//! One-step alternative to the manual edge-tts → ffmpeg → message_send pipeline.
//! The LLM calls `tts(text="…")` and the tool handles MP3 generation, M4A
//! conversion, and delivery to the current conversation sender automatically.

use super::message_send::auto_generate_voice;
use super::traits::{Tool, ToolResult};
use crate::channels::traits::{Channel, SendMessage};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct TtsTool {
    /// Active channel — updated per-message via `set_active_channel` so that
    /// replies are always routed back on the same channel the message arrived on
    /// (e.g., wacli instead of signal for WhatsApp messages).
    active_channel: Arc<tokio::sync::RwLock<Arc<dyn Channel>>>,
    default_recipient: Arc<tokio::sync::RwLock<Option<String>>>,
    security: Arc<SecurityPolicy>,
}

impl TtsTool {
    pub fn new(
        channel: Arc<dyn Channel>,
        default_recipient: Arc<tokio::sync::RwLock<Option<String>>>,
        security: Arc<SecurityPolicy>,
    ) -> Self {
        Self {
            active_channel: Arc::new(tokio::sync::RwLock::new(channel)),
            default_recipient,
            security,
        }
    }
}

#[async_trait]
impl Tool for TtsTool {
    fn name(&self) -> &str {
        "tts"
    }

    fn description(&self) -> &str {
        "Convert text to a voice message and send it to the current conversation. \
         Uses edge-tts (zh-CN-YunxiNeural by default) + ffmpeg to produce an M4A \
         audio file and delivers it as a Signal voice note in one step."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The text to convert to speech and send as a voice message."
                },
                "voice": {
                    "type": "string",
                    "description": "Edge-TTS voice name (default: zh-CN-YunxiNeural). \
                                    Other options: zh-CN-XiaoxiaoNeural (female), \
                                    en-US-AriaNeural (English female), etc."
                },
                "target": {
                    "type": "string",
                    "description": "Recipient identifier. Defaults to the current conversation sender."
                }
            },
            "required": ["text"]
        })
    }

    async fn set_active_recipient(&self, recipient: &str) {
        *self.default_recipient.write().await = Some(recipient.to_string());
    }

    async fn set_active_channel(&self, channel: Arc<dyn crate::channels::traits::Channel>) {
        *self.active_channel.write().await = channel;
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Security guards
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

        let text = match args["text"].as_str() {
            Some(t) if !t.is_empty() => t.to_owned(),
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing required 'text' parameter.".into()),
                });
            }
        };

        let voice = args["voice"]
            .as_str()
            .unwrap_or("zh-CN-YunxiNeural")
            .to_owned();

        // Resolve recipient
        let default = self.default_recipient.read().await.clone();
        let recipient = match args["target"].as_str().map(str::to_owned).or(default) {
            Some(r) if !r.is_empty() => r,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "Missing 'target': no recipient specified and no default conversation sender.".into(),
                    ),
                });
            }
        };

        // Generate voice file
        let voice_path = match auto_generate_voice(&text, &voice).await {
            Ok(path) => path,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("TTS generation failed: {e}")),
                });
            }
        };

        let channel = self.active_channel.read().await.clone();
        tracing::info!(
            "tts: generated {voice_path} for recipient {recipient} via channel={}",
            channel.name()
        );

        // Send via channel using [VOICE:] marker
        let content = format!("[VOICE:{voice_path}]");
        let msg = SendMessage::new(content, &recipient);

        match channel.send(&msg).await {
            Ok(()) => {
                // Delay cleanup to ensure signal-cli has finished reading the file.
                // signal-cli may process the attachment asynchronously after the RPC returns,
                // so immediate deletion causes a "file not found" error on the daemon side.
                let path_for_cleanup = voice_path.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                    if let Err(e) = tokio::fs::remove_file(&path_for_cleanup).await {
                        tracing::debug!("tts cleanup: could not remove {path_for_cleanup}: {e}");
                    }
                });
                Ok(ToolResult {
                    success: true,
                    output: format!("Voice message sent to {recipient}"),
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to send voice message: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::ChannelMessage;
    use crate::security::AutonomyLevel;
    use parking_lot::Mutex as ParkingMutex;

    // ── Mock channel ────────────────────────────────────────────

    struct MockChannel {
        sent: ParkingMutex<Vec<String>>,
        fail_send: bool,
    }

    impl MockChannel {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                sent: ParkingMutex::new(Vec::new()),
                fail_send: false,
            })
        }
        fn failing() -> Arc<Self> {
            Arc::new(Self {
                sent: ParkingMutex::new(Vec::new()),
                fail_send: true,
            })
        }
    }

    #[async_trait]
    impl Channel for MockChannel {
        fn name(&self) -> &str {
            "mock"
        }
        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            if self.fail_send {
                anyhow::bail!("mock send failure");
            }
            self.sent.lock().push(message.content.clone());
            Ok(())
        }
        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // ── Helpers ─────────────────────────────────────────────────

    fn test_security(level: AutonomyLevel, max_actions: u32) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            max_actions_per_hour: max_actions,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn make_tts(
        channel: Arc<dyn Channel>,
        recipient: Option<&str>,
        level: AutonomyLevel,
    ) -> TtsTool {
        let default_recipient = Arc::new(tokio::sync::RwLock::new(recipient.map(String::from)));
        TtsTool::new(channel, default_recipient, test_security(level, 1000))
    }

    // ── Metadata ────────────────────────────────────────────────

    #[test]
    fn tool_name() {
        let tool = make_tts(MockChannel::ok(), None, AutonomyLevel::Full);
        assert_eq!(tool.name(), "tts");
    }

    #[test]
    fn tool_description_non_empty() {
        let tool = make_tts(MockChannel::ok(), None, AutonomyLevel::Full);
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn tool_schema_requires_text() {
        let tool = make_tts(MockChannel::ok(), None, AutonomyLevel::Full);
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().expect("test: required");
        assert!(required.iter().any(|v| v == "text"));
    }

    // ── Security: read-only ─────────────────────────────────────

    #[tokio::test]
    async fn readonly_blocks_execution() {
        let tool = make_tts(MockChannel::ok(), Some("bob"), AutonomyLevel::ReadOnly);
        let result = tool.execute(json!({"text": "hello"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only"));
    }

    // ── Security: rate limiting ─────────────────────────────────

    #[tokio::test]
    async fn rate_limit_blocks_after_exhaustion() {
        let default_recipient = Arc::new(tokio::sync::RwLock::new(Some("bob".into())));
        let tool = TtsTool::new(
            MockChannel::ok(),
            default_recipient,
            test_security(AutonomyLevel::Full, 1),
        );
        // First call uses the budget (will fail at auto_generate_voice but after rate limit check)
        let r1 = tool.execute(json!({"text": "first"})).await.unwrap();
        // r1 fails at TTS generation (no edge-tts binary) — that's fine, budget consumed
        let _ = r1;

        // Second call should be blocked by rate limit
        let r2 = tool.execute(json!({"text": "second"})).await.unwrap();
        assert!(!r2.success);
        assert!(r2.error.as_deref().unwrap_or("").contains("rate limit"));
    }

    // ── Arg validation: missing/empty text ──────────────────────

    #[tokio::test]
    async fn missing_text_fails() {
        let tool = make_tts(MockChannel::ok(), Some("bob"), AutonomyLevel::Full);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("text"));
    }

    #[tokio::test]
    async fn empty_text_fails() {
        let tool = make_tts(MockChannel::ok(), Some("bob"), AutonomyLevel::Full);
        let result = tool.execute(json!({"text": ""})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("text"));
    }

    #[tokio::test]
    async fn null_text_fails() {
        let tool = make_tts(MockChannel::ok(), Some("bob"), AutonomyLevel::Full);
        let result = tool.execute(json!({"text": null})).await.unwrap();
        assert!(!result.success);
    }

    // ── Recipient resolution ────────────────────────────────────

    #[tokio::test]
    async fn no_recipient_and_no_default_fails() {
        let tool = make_tts(MockChannel::ok(), None, AutonomyLevel::Full);
        let result = tool.execute(json!({"text": "hello"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("target"));
    }

    #[tokio::test]
    async fn empty_target_and_no_default_fails() {
        let tool = make_tts(MockChannel::ok(), None, AutonomyLevel::Full);
        let result = tool
            .execute(json!({"text": "hello", "target": ""}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("target"));
    }

    // ── Voice defaults ──────────────────────────────────────────
    // (auto_generate_voice will fail because edge-tts is not installed in CI,
    //  but we verify the TTS generation error path is handled gracefully)

    #[tokio::test]
    async fn tts_generation_failure_returns_error_not_panic() {
        let tool = make_tts(MockChannel::ok(), Some("bob"), AutonomyLevel::Full);
        let result = tool.execute(json!({"text": "test speech"})).await.unwrap();
        // auto_generate_voice fails → graceful error, not panic
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("TTS generation failed")
        );
    }

    #[tokio::test]
    async fn custom_voice_passed_through() {
        let tool = make_tts(MockChannel::ok(), Some("bob"), AutonomyLevel::Full);
        // This will fail at TTS generation (no edge-tts), but exercises the voice param path
        let result = tool
            .execute(json!({
                "text": "test",
                "voice": "en-US-AriaNeural"
            }))
            .await
            .unwrap();
        // Graceful failure expected — voice is parsed before reaching auto_generate_voice
        assert!(!result.success);
    }

    // ── set_active_recipient / set_active_channel ───────────────

    #[tokio::test]
    async fn set_active_recipient_updates_default() {
        let tool = make_tts(MockChannel::ok(), None, AutonomyLevel::Full);

        // Initially no default → fails
        let r1 = tool.execute(json!({"text": "hi"})).await.unwrap();
        assert!(!r1.success);
        assert!(r1.error.as_deref().unwrap_or("").contains("target"));

        // Set recipient
        tool.set_active_recipient("alice").await;

        // Now recipient is resolved → proceeds past validation (fails at TTS generation)
        let r2 = tool.execute(json!({"text": "hi"})).await.unwrap();
        assert!(!r2.success);
        assert!(r2.error.as_deref().unwrap_or("").contains("TTS generation"));
    }

    #[tokio::test]
    async fn set_active_channel_switches_channel() {
        let ch1 = MockChannel::ok();
        let ch2 = MockChannel::ok();
        let tool = make_tts(ch1.clone(), Some("bob"), AutonomyLevel::Full);

        // Switch to ch2
        tool.set_active_channel(ch2.clone()).await;

        let channel = tool.active_channel.read().await;
        assert_eq!(channel.name(), "mock"); // both are mock, but it's a different Arc
    }
}
