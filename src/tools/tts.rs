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
    channel: Arc<dyn Channel>,
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
            channel,
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

        tracing::info!("tts: generated {voice_path} for recipient {recipient}");

        // Send via channel using [VOICE:] marker
        let content = format!("[VOICE:{voice_path}]");
        let msg = SendMessage::new(content, &recipient);

        match self.channel.send(&msg).await {
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
