//! Message send tool — lets the LLM proactively send messages through channels.
//!
//! Aligns with OpenClaw's `message` tool:
//! - Send text messages to specific recipients
//! - Send files/images/voice as attachments via `[IMAGE:]`, `[VOICE:]`, `[DOCUMENT:]` markers
//! - Send emoji reactions (Signal-specific, falls back to error on unsupported channels)
//! - Quote reply to specific messages

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::channels::SignalChannel;
use crate::channels::traits::{Channel, SendMessage};
use crate::security::op_id;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// Auto-generate a voice file from text using edge-tts + ffmpeg.
///
/// Returns the path to an M4A (AAC) file that can be embedded as `[VOICE:/tmp/…m4a]`.
/// The caller is responsible for cleaning up the file after sending.
pub(crate) async fn auto_generate_voice(text: &str, voice: &str) -> anyhow::Result<String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mp3_path = format!("/tmp/openprx-tts-{id}.mp3");
    let m4a_path = format!("/tmp/openprx-tts-{id}.m4a");

    // Sanitise text for embedding inside a JS single-quoted string.
    let safe_text = text
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', " ")
        .replace('\r', "");

    // Sanitise voice the same way — it is user-controlled and must
    // not be able to break out of the JS single-quoted string literal.
    let safe_voice = voice
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', " ")
        .replace('\r', "");

    // 1. Generate MP3 with node-edge-tts
    let tts_script = format!(
        r#"const{{EdgeTTS}}=require('node-edge-tts');new EdgeTTS().ttsPromise('{safe_text}','{mp3_path}',{{voice:'{safe_voice}'}}).then(()=>console.log('ok')).catch(e=>{{console.error(String(e));process.exit(1)}})"#
    );

    // Resolve the global node_modules path so `require('node-edge-tts')` works.
    // npm/node/ffmpeg all fork helper processes of their own, so every step runs
    // in its own process group and is torn down as a group with this future.
    let mut npm_cmd = tokio::process::Command::new("npm");
    npm_cmd.args(["root", "-g"]);
    let npm_root_out = crate::runtime::shell_process::run_managed_output(npm_cmd)
        .await
        .map_err(|e| anyhow::anyhow!("npm not found: {e}"))?;
    let node_modules = String::from_utf8_lossy(&npm_root_out.stdout).trim().to_string();

    let mut node_cmd = tokio::process::Command::new("node");
    node_cmd.args(["-e", &tts_script]).env("NODE_PATH", &node_modules);
    let tts_out = crate::runtime::shell_process::run_managed_output(node_cmd)
        .await
        .map_err(|e| anyhow::anyhow!("node not found: {e}"))?;

    if !tts_out.status.success() {
        let stderr = String::from_utf8_lossy(&tts_out.stderr).to_string();
        anyhow::bail!("edge-tts failed: {stderr}");
    }

    // 2. Convert MP3 → M4A (AAC) — Signal displays M4A as a playable voice note.
    let mut ffmpeg_cmd = tokio::process::Command::new("ffmpeg");
    ffmpeg_cmd.args(["-y", "-i", &mp3_path, "-c:a", "aac", "-b:a", "64k", &m4a_path]);
    let ffmpeg_out = crate::runtime::shell_process::run_managed_output(ffmpeg_cmd)
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

/// Per-turn routing defaults for `message_send`.
///
/// Channel/gateway/chat turns may run concurrently in the same process. The
/// legacy `active_channel` / `default_recipient` slots below remain as a
/// non-turn fallback, but in-turn tool calls must read this task-local context
/// so one inbound message cannot overwrite another turn's implicit reply target.
#[derive(Clone)]
pub(crate) struct MessageSendExecutionContext {
    pub(crate) default_recipient: Option<String>,
    pub(crate) active_channel: Arc<dyn Channel>,
}

impl MessageSendExecutionContext {
    pub(crate) fn new(default_recipient: Option<String>, active_channel: Arc<dyn Channel>) -> Self {
        Self {
            default_recipient,
            active_channel,
        }
    }
}

impl fmt::Debug for MessageSendExecutionContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageSendExecutionContext")
            .field("default_recipient", &self.default_recipient)
            .field("active_channel", &self.active_channel.name())
            .finish()
    }
}

tokio::task_local! {
    pub(crate) static MESSAGE_SEND_EXECUTION_CONTEXT: MessageSendExecutionContext;
}

fn current_message_send_execution_context() -> Option<MessageSendExecutionContext> {
    MESSAGE_SEND_EXECUTION_CONTEXT.try_with(Clone::clone).ok()
}

/// Sender / chat-type identity for the outbound authorization decision.
///
/// Read only from the runtime-injected trusted scope (`crate::tools::execution`
/// scrubs any model-supplied `_zc_scope*` before rewriting it), so a model
/// cannot claim a different sender to widen its own outbound reach. When no
/// trusted scope is present — direct tool invocation, tests, non-turn calls —
/// the identity is unknown and only wildcard scope rules can match.
fn trusted_outbound_identity(args: &serde_json::Value) -> (String, String) {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return ("unknown".to_string(), "unknown".to_string());
    }
    let field = |key: &str| {
        args.pointer(&format!("/_zc_scope/{key}"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown")
            .to_string()
    };
    (field("sender"), field("chat_type"))
}

/// The explicitly requested destination channel, if any.
///
/// Blank strings are treated as absent so an empty argument keeps the
/// pre-existing single-channel behaviour instead of failing the lookup.
fn requested_channel(args: &serde_json::Value) -> Option<&str> {
    args.get("channel")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

pub struct MessageSendTool {
    /// Active channel — updated per-message via `set_active_channel` so that
    /// replies are always routed back on the same channel the message arrived on
    /// (e.g., wacli instead of signal for WhatsApp messages).
    /// Uses a `RwLock` so the active channel can be swapped per-message.
    active_channel: Arc<tokio::sync::RwLock<Arc<dyn Channel>>>,
    /// Optional Signal channel reference for reaction support.
    signal: Option<Arc<SignalChannel>>,
    /// Default recipient used when the LLM omits `target`.
    /// Stored in an `RwLock` so the gateway can update it per-message.
    default_recipient: Arc<tokio::sync::RwLock<Option<String>>>,
    security: Arc<SecurityPolicy>,
    /// Every configured channel, keyed by [`Channel::name`] — the registry the
    /// optional `channel` argument resolves against. Shared with
    /// `sessions_spawn`'s announce/kill routing registry so both tools address
    /// exactly the same set of channels. Empty when no registry was injected,
    /// in which case only the turn's own channel is addressable.
    channels: Arc<HashMap<String, Arc<dyn Channel>>>,
}

impl MessageSendTool {
    /// Create a new `MessageSendTool` backed by a generic channel.
    pub fn new(channel: Arc<dyn Channel>, security: Arc<SecurityPolicy>) -> Self {
        Self {
            active_channel: Arc::new(tokio::sync::RwLock::new(channel)),
            signal: None,
            default_recipient: Arc::new(tokio::sync::RwLock::new(None)),
            security,
            channels: Arc::new(HashMap::new()),
        }
    }

    /// Create a new `MessageSendTool` backed by a Signal channel (enables reactions).
    pub fn new_signal(channel: Arc<SignalChannel>, security: Arc<SecurityPolicy>) -> Self {
        Self {
            active_channel: Arc::new(tokio::sync::RwLock::new(channel.clone() as Arc<dyn Channel>)),
            signal: Some(channel),
            default_recipient: Arc::new(tokio::sync::RwLock::new(None)),
            security,
            channels: Arc::new(HashMap::new()),
        }
    }

    /// Attach the full set of configured channels, keyed by [`Channel::name`].
    ///
    /// This is what makes the optional `channel` argument resolvable. Callers
    /// pass the very same `Arc` they hand to `sessions_spawn`, so a channel that
    /// can receive a sub-agent announcement is exactly a channel `message_send`
    /// can address, and vice versa. Without it the tool stays single-channel:
    /// only the turn's own channel is addressable.
    #[must_use]
    pub fn with_channels(mut self, channels: Arc<HashMap<String, Arc<dyn Channel>>>) -> Self {
        self.channels = channels;
        self
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

    /// Channel names this tool can address, for the "unknown channel" error.
    ///
    /// The turn's own channel is always addressable even when no registry was
    /// injected, so it is unioned in rather than assumed to be a registry key.
    fn addressable_channel_names(&self, current: &str) -> Vec<String> {
        let mut names: Vec<String> = self.channels.keys().cloned().collect();
        if !names.iter().any(|name| name == current) {
            names.push(current.to_string());
        }
        names.sort_unstable();
        names
    }

    /// Cross-channel delivery carries text only.
    ///
    /// [`SendMessage`] has no attachment field: media travels as a `[KIND:path]`
    /// marker that the *originating* channel resolves against paths it owns.
    /// Handing such a marker to a different channel would neither reliably
    /// deliver the file nor stay inside that channel's media ownership — it
    /// turns an outbound text send into "read this local path". So a request
    /// that both names another channel and embeds (or asks to synthesise) media
    /// is refused outright rather than silently downgraded to plain text.
    fn reject_cross_channel_media(args: &serde_json::Value, dst_channel: &str) -> Result<(), String> {
        let message = args.get("message").and_then(serde_json::Value::as_str).unwrap_or("");
        let (_, media) = crate::channels::traits::extract_outgoing_media(message);
        if let Some((marker, _)) = media.first() {
            return Err(format!(
                "Cross-channel delivery to '{dst_channel}' is text-only, but this message embeds a \
                 [{marker}:] media marker. Attachments are local paths owned by the originating \
                 channel and cannot be handed to another channel. Send the text across channels, \
                 or send the media on the current channel."
            ));
        }
        if args
            .get("as_voice")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Err(format!(
                "Cross-channel delivery to '{dst_channel}' is text-only, so as_voice=true cannot be \
                 honoured: the synthesised voice note is a local file the destination channel does \
                 not own. Drop as_voice, or send the voice note on the current channel."
            ));
        }
        Ok(())
    }

    /// Resolve which channel object actually carries this call.
    ///
    /// No `channel` argument — the overwhelmingly common case — returns the
    /// turn's own channel unchanged, so every pre-existing behaviour is
    /// bit-for-bit what it was. An explicit `channel` naming the turn's own
    /// channel is likewise a no-op. Anything else must be found in the injected
    /// registry; an unknown name is a hard error that lists what *is*
    /// addressable, never a silent fallback to the current channel.
    fn resolve_outbound_route(
        &self,
        args: &serde_json::Value,
        current: &Arc<dyn Channel>,
    ) -> Result<Arc<dyn Channel>, String> {
        let Some(requested) = requested_channel(args) else {
            return Ok(Arc::clone(current));
        };
        if requested == current.name() {
            return Ok(Arc::clone(current));
        }
        let Some(target) = self.channels.get(requested) else {
            return Err(format!(
                "Unknown channel '{requested}'. Available channels: {}",
                self.addressable_channel_names(current.name()).join(", ")
            ));
        };
        Ok(Arc::clone(target))
    }

    /// Outbound recipient authorization, applied to every action that reaches a
    /// recipient (`send`, `react`, `edit`, `delete`/`unsend`, `thread`).
    ///
    /// `src_channel` is the channel the turn is anchored to and `dst_channel`
    /// the one the message actually leaves on. They differ exactly when the
    /// caller passed an explicit `channel`; with no `channel` argument they are
    /// the same object, which is what keeps this a no-op unless an operator
    /// configures `send_allow` / `send_deny`.
    ///
    /// The rejection message carries only the stable audit fingerprint of the
    /// destination, never the plaintext recipient.
    fn authorize_outbound(
        &self,
        args: &serde_json::Value,
        src_channel: &str,
        dst_channel: &str,
        recipient: &str,
    ) -> Result<(), String> {
        let (sender, chat_type) = trusted_outbound_identity(args);
        if self
            .security
            .is_outbound_allowed(&sender, src_channel, &chat_type, dst_channel, recipient)
        {
            return Ok(());
        }
        let recipient_ref = op_id::ref_for_channel_recipient(dst_channel, recipient);
        Err(crate::security::audit::redact_secrets(&format!(
            "Security policy: outbound messaging to recipient {recipient_ref} on channel \
             '{dst_channel}' is not permitted by the configured scope rules"
        )))
    }

    /// Decide which channel object carries this call, and clear it to do so.
    ///
    /// Three gates, in this order, for every recipient-bearing action except
    /// `react` (whose delivery handle is fixed — see that arm):
    ///
    /// 1. resolve the requested channel, erroring on an unknown name;
    /// 2. authorize the destination — a refusal here is final, the call is
    ///    **never** retried on the turn's own channel, because falling back
    ///    would report a denied cross-channel send as a delivered one;
    /// 3. refuse cross-channel media, which cannot travel between channels.
    ///
    /// With no `channel` argument the destination is the turn's own channel and
    /// gates 1 and 3 are inert, leaving exactly the pre-existing behaviour.
    fn prepare_outbound(
        &self,
        args: &serde_json::Value,
        current: &Arc<dyn Channel>,
        recipient: &str,
    ) -> Result<Arc<dyn Channel>, String> {
        let destination = self.resolve_outbound_route(args, current)?;
        self.authorize_outbound(args, current.name(), destination.name(), recipient)?;
        if destination.name() != current.name() {
            Self::reject_cross_channel_media(args, destination.name())?;
        }
        Ok(destination)
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
         Use action='send' for messages and action='react' for emoji reactions. \
         Set 'channel' to deliver text on a different configured channel."
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
                "channel": {
                    "type": "string",
                    "description": "Destination channel name (e.g. 'signal', 'telegram', 'wacli'). \
                                    Omit to stay on the current conversation's channel. Naming another \
                                    channel requires it to be permitted by the outbound scope rules and \
                                    delivers text only — media markers and as_voice are refused. \
                                    Not accepted for action='react'."
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
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("send");

        let turn_context = current_message_send_execution_context();

        // Resolve the active channel. In-turn calls use task-local routing;
        // outside a turn, fall back to the construction/update-time slot.
        let channel = match turn_context.as_ref() {
            Some(context) => Arc::clone(&context.active_channel),
            None => self.active_channel.read().await.clone(),
        };

        // Resolve recipient: explicit arg takes priority, then the task-local
        // turn default. Only non-turn calls fall back to the legacy slot.
        let default = match turn_context {
            Some(context) => context.default_recipient,
            None => self.default_recipient.read().await.clone(),
        };
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
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
                let destination = match self.prepare_outbound(&args, &channel, &recipient) {
                    Ok(destination) => destination,
                    Err(error) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(error),
                        });
                    }
                };
                let recipient_ref = op_id::ref_for_channel_recipient(destination.name(), &recipient);
                let operation_name = op_id::op_id(self.name(), "send", &[destination.name(), &recipient_ref]);
                let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                if let Err(error) = SideEffectGate::new(&self.security).authorize_resource_operation(
                    self.name(),
                    &operation_name,
                    ResourceRiskLevel::Medium,
                    approval_grant.as_ref(),
                ) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(error),
                    });
                }

                let raw_content = args.get("message").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let as_voice = args.get("as_voice").and_then(|v| v.as_bool()).unwrap_or(false);

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

                if let Some(ts) = args.get("quote_timestamp").and_then(|v| v.as_u64()) {
                    msg.quote_timestamp = Some(ts);
                }
                if let Some(author) = args.get("quote_author").and_then(|v| v.as_str()) {
                    msg.quote_author = Some(author.to_owned());
                }

                match destination.send(&msg).await {
                    Ok(()) => {
                        // Delayed cleanup for auto-generated TTS files (Bug 1 fix):
                        // signal-cli may read the file asynchronously after the RPC response,
                        // so we wait 30 s before deleting to avoid "file not found" errors.
                        if let Some(tts_path) = auto_tts_path {
                            tokio::spawn(async move {
                                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                                if let Err(e) = tokio::fs::remove_file(&tts_path).await {
                                    tracing::debug!("auto-tts cleanup: could not remove {tts_path}: {e}");
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
                            error: Some("Missing 'target': provide a recipient for the reaction.".into()),
                        });
                    }
                };

                // Reactions are delivered through the Signal channel handle, which
                // may differ from the turn's active channel; authorize the channel
                // that actually carries the reaction.
                let react_channel_name = self
                    .signal
                    .as_ref()
                    .map_or_else(|| channel.name(), |signal| signal.name());
                // `channel` would be ambiguous here: a reaction is not carried by
                // the `Channel` object the argument resolves to, and no other
                // channel exposes `send_reaction`. Rather than let the argument
                // read as "route this reaction elsewhere" while it silently did
                // nothing, refuse it unless it names the handle that really sends.
                if let Some(requested) = requested_channel(&args)
                    && requested != react_channel_name
                {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Reactions cannot be redirected: action='react' is always delivered by the \
                             '{react_channel_name}' handle, so channel='{requested}' cannot be honoured. \
                             Drop 'channel', or use action='send' to reach '{requested}'."
                        )),
                    });
                }
                if let Err(error) = self.authorize_outbound(&args, react_channel_name, react_channel_name, &recipient) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(error),
                    });
                }

                let emoji = match args.get("emoji").and_then(|v| v.as_str()) {
                    Some(e) if !e.is_empty() => e.to_owned(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing required 'emoji' parameter for react action.".into()),
                        });
                    }
                };

                let target_author = match args.get("target_author").and_then(|v| v.as_str()) {
                    Some(a) if !a.is_empty() => a.to_owned(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing required 'target_author' parameter for react action.".into()),
                        });
                    }
                };

                let target_timestamp = match args.get("target_timestamp").and_then(|v| v.as_u64()) {
                    Some(ts) => ts,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing required 'target_timestamp' parameter for react action.".into()),
                        });
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
                        error: Some("Reactions are not supported on this channel (Signal required).".into()),
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
                let destination = match self.prepare_outbound(&args, &channel, &recipient) {
                    Ok(destination) => destination,
                    Err(error) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(error),
                        });
                    }
                };
                let message_id = match args.get("message_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => id.to_owned(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'message_id' for edit action.".into()),
                        });
                    }
                };
                let new_text = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
                match destination.edit_message(&recipient, &message_id, new_text).await {
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
                let destination = match self.prepare_outbound(&args, &channel, &recipient) {
                    Ok(destination) => destination,
                    Err(error) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(error),
                        });
                    }
                };
                let message_id = match args.get("message_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => id.to_owned(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'message_id' for delete/unsend action.".into()),
                        });
                    }
                };
                match destination.delete_message(&recipient, &message_id).await {
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
                let destination = match self.prepare_outbound(&args, &channel, &recipient) {
                    Ok(destination) => destination,
                    Err(error) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(error),
                        });
                    }
                };
                let thread_id = match args.get("thread_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => id.to_owned(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'thread_id' for thread action.".into()),
                        });
                    }
                };
                let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
                match destination.send_thread_reply(&recipient, &thread_id, message).await {
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

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Communication]
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use async_trait::async_trait;

    struct DummyChannel {
        pub name: &'static str,
        pub sent: Arc<tokio::sync::Mutex<Vec<String>>>,
        pub recipients: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    impl DummyChannel {
        fn new() -> (Arc<Self>, Arc<tokio::sync::Mutex<Vec<String>>>) {
            let (channel, sent, _recipients) = Self::new_named("dummy");
            (channel, sent)
        }

        fn new_named(
            name: &'static str,
        ) -> (
            Arc<Self>,
            Arc<tokio::sync::Mutex<Vec<String>>>,
            Arc<tokio::sync::Mutex<Vec<String>>>,
        ) {
            let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let recipients = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    name,
                    sent: sent.clone(),
                    recipients: recipients.clone(),
                }),
                sent,
                recipients,
            )
        }
    }

    #[async_trait]
    impl Channel for DummyChannel {
        fn name(&self) -> &str {
            self.name
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().await.push(message.content.clone());
            self.recipients.lock().await.push(message.recipient.clone());
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn test_security(level: AutonomyLevel) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
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
    async fn task_local_context_beats_mutated_fallback_defaults() {
        let (channel_a, sent_a, recipients_a) = DummyChannel::new_named("channel-a");
        let (channel_b, sent_b, recipients_b) = DummyChannel::new_named("channel-b");
        let tool = MessageSendTool::new(channel_a.clone(), test_security(AutonomyLevel::Full));

        let turn_a_context = MessageSendExecutionContext::new(
            Some("recipient-a".to_string()),
            channel_a as Arc<dyn crate::channels::traits::Channel>,
        );

        let result = MESSAGE_SEND_EXECUTION_CONTEXT
            .scope(turn_a_context, async {
                // Simulate another inbound turn updating the legacy fallback while
                // turn A is still in progress. The send below omits `target`; it
                // must still route to A's task-local recipient/channel.
                tool.set_default_recipient(Some("recipient-b".to_string())).await;
                tool.set_active_channel(channel_b as Arc<dyn crate::channels::traits::Channel>)
                    .await;
                tool.execute(json!({
                    "action": "send",
                    "message": "reply from A"
                }))
                .await
            })
            .await
            .unwrap();

        assert!(result.success, "Expected success, got: {:?}", result.error);
        assert_eq!(&*sent_a.lock().await, &["reply from A".to_string()]);
        assert_eq!(&*recipients_a.lock().await, &["recipient-a".to_string()]);
        assert!(
            sent_b.lock().await.is_empty(),
            "fallback channel must not receive turn A send"
        );
        assert!(
            recipients_b.lock().await.is_empty(),
            "fallback recipient must not receive turn A send"
        );
    }

    #[tokio::test]
    async fn fallback_default_still_works_without_turn_context() {
        let (ch, sent, recipients) = DummyChannel::new_named("fallback");
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));
        tool.set_default_recipient(Some("fallback-recipient".to_string())).await;

        let result = tool
            .execute(json!({
                "action": "send",
                "message": "fallback send"
            }))
            .await
            .unwrap();

        assert!(result.success, "Expected success, got: {:?}", result.error);
        assert_eq!(&*sent.lock().await, &["fallback send".to_string()]);
        assert_eq!(&*recipients.lock().await, &["fallback-recipient".to_string()]);
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

        let result = tool.execute(json!({ "action": "destroy" })).await.unwrap();

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
        assert_eq!(
            original_sent.lock().await.len(),
            1,
            "first send should go to original channel"
        );

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
        assert_eq!(
            original_sent.lock().await.len(),
            1,
            "original channel should still have only 1 message"
        );
        assert_eq!(
            new_sent.lock().await.len(),
            1,
            "new channel should have received the second message"
        );
    }

    // ── Outbound recipient authorization (plan a1) ──────────────────────────

    const DENIED_RECIPIENT: &str = "+15550001111";
    const POLICY_ERROR_MARKER: &str = "not permitted by the configured scope rules";

    fn scoped_security(send_allow: &[&str], send_deny: &[&str]) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            scope_rules: vec![crate::config::ScopeRule {
                user: None,
                channel: None,
                chat_type: None,
                tools_allow: vec![],
                tools_deny: vec![],
                send_allow: send_allow.iter().map(|s| (*s).to_string()).collect(),
                send_deny: send_deny.iter().map(|s| (*s).to_string()).collect(),
            }],
            ..SecurityPolicy::default()
        })
    }

    /// Every action that reaches a recipient, with a target that a `send_deny`
    /// entry can match.
    fn denied_recipient_actions() -> Vec<serde_json::Value> {
        vec![
            json!({"action": "send", "target": DENIED_RECIPIENT, "message": "hi"}),
            json!({
                "action": "react", "target": DENIED_RECIPIENT, "emoji": "\u{1F44D}",
                "target_author": "+19998887777", "target_timestamp": 1_700_000_000_000u64
            }),
            json!({"action": "edit", "target": DENIED_RECIPIENT, "message_id": "1", "message": "hi"}),
            json!({"action": "delete", "target": DENIED_RECIPIENT, "message_id": "1"}),
            json!({"action": "unsend", "target": DENIED_RECIPIENT, "message_id": "1"}),
            json!({"action": "thread", "target": DENIED_RECIPIENT, "thread_id": "t1", "message": "hi"}),
        ]
    }

    #[tokio::test]
    async fn send_deny_blocks_every_recipient_action_without_leaking_the_recipient() {
        let (ch, sent) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, scoped_security(&[], &["*:+15550001111"]));

        for args in denied_recipient_actions() {
            let action = args["action"].as_str().unwrap().to_string();
            let result = tool.execute(args).await.unwrap();
            assert!(!result.success, "action {action} must be denied");
            let error = result.error.unwrap_or_default();
            assert!(
                error.contains(POLICY_ERROR_MARKER),
                "action {action} must fail on the outbound policy, got: {error}"
            );
            assert!(
                !error.contains(DENIED_RECIPIENT),
                "action {action} error must not leak the plaintext recipient: {error}"
            );
        }
        assert!(sent.lock().await.is_empty(), "no message may reach the channel");
    }

    /// Zero-regression guard: with no scope rules configured, not one action
    /// changes behaviour — in particular none of them can fail on the new
    /// outbound decision point.
    #[tokio::test]
    async fn without_scope_rules_no_action_hits_the_outbound_policy() {
        let (ch, sent) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, test_security(AutonomyLevel::Full));

        for args in denied_recipient_actions() {
            let action = args["action"].as_str().unwrap().to_string();
            let result = tool.execute(args).await.unwrap();
            let error = result.error.clone().unwrap_or_default();
            assert!(
                !error.contains(POLICY_ERROR_MARKER),
                "action {action} must not be touched by the outbound policy, got: {error}"
            );
        }
        // `send` is the only action DummyChannel implements; it still went out.
        assert_eq!(sent.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn send_deny_outranks_send_allow() {
        let (ch, sent) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, scoped_security(&["*"], &["*:+15550001111"]));

        let denied = tool
            .execute(json!({"action": "send", "target": DENIED_RECIPIENT, "message": "hi"}))
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(denied.error.unwrap_or_default().contains(POLICY_ERROR_MARKER));

        // The wildcard allow still lets every other recipient through.
        let allowed = tool
            .execute(json!({"action": "send", "target": "+15550002222", "message": "hi"}))
            .await
            .unwrap();
        assert!(allowed.success, "got: {:?}", allowed.error);
        assert_eq!(sent.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn send_allow_whitelist_scopes_the_destination_channel() {
        let (ch, _sent, _recipients) = DummyChannel::new_named("dummy");
        let tool = MessageSendTool::new(ch, scoped_security(&["telegram:*"], &[]));

        // The turn's own channel is "dummy", which the whitelist excludes.
        let result = tool
            .execute(json!({"action": "send", "target": "+15550002222", "message": "hi"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains(POLICY_ERROR_MARKER));
    }

    // ── Explicit destination channel (plan a2) ─────────────────────────────

    /// Build the shared channel registry the way the runtime does: keyed by
    /// [`Channel::name`], the same `Arc` `sessions_spawn` receives.
    fn registry(channels: &[Arc<DummyChannel>]) -> Arc<HashMap<String, Arc<dyn Channel>>> {
        Arc::new(
            channels
                .iter()
                .map(|ch| (ch.name().to_string(), Arc::clone(ch) as Arc<dyn Channel>))
                .collect(),
        )
    }

    /// A turn channel plus a second configured channel, wired into one registry.
    #[allow(clippy::type_complexity)]
    fn two_channel_tool(
        security: Arc<SecurityPolicy>,
    ) -> (
        MessageSendTool,
        Arc<tokio::sync::Mutex<Vec<String>>>,
        Arc<tokio::sync::Mutex<Vec<String>>>,
        Arc<tokio::sync::Mutex<Vec<String>>>,
    ) {
        let (current, current_sent, _current_recipients) = DummyChannel::new_named("dummy");
        let (other, other_sent, other_recipients) = DummyChannel::new_named("telegram");
        let tool = MessageSendTool::new(Arc::clone(&current) as Arc<dyn Channel>, security)
            .with_channels(registry(&[current, other]));
        (tool, current_sent, other_sent, other_recipients)
    }

    /// Evidence 2: an authorized explicit `channel` really hands the message to
    /// that channel object — and to no other.
    #[tokio::test]
    async fn explicit_channel_delivers_to_the_named_channel() {
        let (tool, current_sent, other_sent, other_recipients) =
            two_channel_tool(scoped_security(&["telegram:*"], &[]));

        let result = tool
            .execute(json!({
                "action": "send", "channel": "telegram",
                "target": "12345", "message": "across the wire"
            }))
            .await
            .unwrap();

        assert!(result.success, "got: {:?}", result.error);
        assert_eq!(&*other_sent.lock().await, &["across the wire".to_string()]);
        assert_eq!(&*other_recipients.lock().await, &["12345".to_string()]);
        assert!(
            current_sent.lock().await.is_empty(),
            "the turn's own channel must not also receive a cross-channel send"
        );
    }

    /// Evidence 3 — the load-bearing one. An unauthorized cross-channel send is
    /// refused outright; nothing may leak onto the turn's own channel, because a
    /// fallback would dress a denied send up as a delivered one.
    #[tokio::test]
    async fn unauthorized_cross_channel_send_never_falls_back_to_the_current_channel() {
        // No scope rules at all: same-channel stays allowed, cross-channel denied.
        let (tool, current_sent, other_sent, other_recipients) = two_channel_tool(test_security(AutonomyLevel::Full));

        let result = tool
            .execute(json!({
                "action": "send", "channel": "telegram",
                "target": "12345", "message": "must not be delivered"
            }))
            .await
            .unwrap();

        assert!(!result.success, "cross-channel send must be denied by default");
        let error = result.error.unwrap_or_default();
        assert!(error.contains(POLICY_ERROR_MARKER), "got: {error}");
        assert!(
            current_sent.lock().await.is_empty(),
            "denied cross-channel send must not fall back to the current channel"
        );
        assert!(other_sent.lock().await.is_empty());
        assert!(other_recipients.lock().await.is_empty());
    }

    /// Same guarantee for every other recipient-bearing action that can be routed.
    #[tokio::test]
    async fn unauthorized_cross_channel_is_refused_for_every_routable_action() {
        let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(test_security(AutonomyLevel::Full));

        for args in [
            json!({"action": "send", "channel": "telegram", "target": "1", "message": "x"}),
            json!({"action": "edit", "channel": "telegram", "target": "1", "message_id": "9", "message": "x"}),
            json!({"action": "delete", "channel": "telegram", "target": "1", "message_id": "9"}),
            json!({"action": "unsend", "channel": "telegram", "target": "1", "message_id": "9"}),
            json!({"action": "thread", "channel": "telegram", "target": "1", "thread_id": "t", "message": "x"}),
        ] {
            let action = args["action"].as_str().unwrap_or_default().to_string();
            let result = tool.execute(args).await.unwrap();
            assert!(!result.success, "action {action} must be denied");
            let error = result.error.unwrap_or_default();
            assert!(
                error.contains(POLICY_ERROR_MARKER),
                "action {action} must fail on the outbound policy, got: {error}"
            );
        }
        assert!(current_sent.lock().await.is_empty());
        assert!(other_sent.lock().await.is_empty());
    }

    /// Evidence 4: an unknown channel name is an error that names what *is*
    /// addressable — never a silent send on the current channel.
    #[tokio::test]
    async fn unknown_channel_name_is_rejected_and_lists_the_available_channels() {
        let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(scoped_security(&["*"], &[]));

        let result = tool
            .execute(json!({
                "action": "send", "channel": "matrix", "target": "12345", "message": "hi"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("Unknown channel 'matrix'"), "got: {error}");
        assert!(error.contains("Available channels: dummy, telegram"), "got: {error}");
        assert!(
            current_sent.lock().await.is_empty(),
            "unknown channel must not fall back"
        );
        assert!(other_sent.lock().await.is_empty());
    }

    /// Without an injected registry only the turn's own channel is addressable,
    /// and the error says so instead of pretending the send went out.
    #[tokio::test]
    async fn without_a_registry_only_the_turn_channel_is_addressable() {
        let (ch, sent) = DummyChannel::new();
        let tool = MessageSendTool::new(ch, scoped_security(&["*"], &[]));

        let result = tool
            .execute(json!({
                "action": "send", "channel": "telegram", "target": "12345", "message": "hi"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("Unknown channel 'telegram'"), "got: {error}");
        assert!(error.contains("Available channels: dummy"), "got: {error}");
        assert!(sent.lock().await.is_empty());
    }

    /// Evidence 5: cross-channel carries text only. A media marker is refused
    /// with the reason, not quietly stripped or shipped as a bare local path.
    #[tokio::test]
    async fn cross_channel_media_marker_is_refused_with_a_reason() {
        let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(scoped_security(&["*"], &[]));

        let result = tool
            .execute(json!({
                "action": "send", "channel": "telegram", "target": "12345",
                "message": "look at this [IMAGE:/tmp/cat.png]"
            }))
            .await
            .unwrap();

        assert!(!result.success, "cross-channel media must be refused");
        let error = result.error.unwrap_or_default();
        assert!(error.contains("text-only"), "got: {error}");
        assert!(error.contains("[IMAGE:]"), "got: {error}");
        assert!(current_sent.lock().await.is_empty());
        assert!(other_sent.lock().await.is_empty());
    }

    /// `as_voice` synthesises a local file, so it is media too.
    #[tokio::test]
    async fn cross_channel_as_voice_is_refused() {
        let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(scoped_security(&["*"], &[]));

        let result = tool
            .execute(json!({
                "action": "send", "channel": "telegram", "target": "12345",
                "message": "read this out", "as_voice": true
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("as_voice"));
        assert!(current_sent.lock().await.is_empty());
        assert!(other_sent.lock().await.is_empty());
    }

    /// A media marker on the turn's *own* channel is untouched by the guard —
    /// the restriction is about crossing channels, not about media.
    #[tokio::test]
    async fn same_channel_media_marker_is_unaffected() {
        let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(test_security(AutonomyLevel::Full));

        for args in [
            json!({"action": "send", "target": "12345", "message": "pic [IMAGE:/tmp/cat.png]"}),
            json!({"action": "send", "channel": "dummy", "target": "12345", "message": "pic [IMAGE:/tmp/cat.png]"}),
        ] {
            let result = tool.execute(args).await.unwrap();
            assert!(result.success, "got: {:?}", result.error);
        }
        assert_eq!(current_sent.lock().await.len(), 2);
        assert!(other_sent.lock().await.is_empty());
    }

    /// Evidence 1: with a registry injected but no `channel` argument, routing is
    /// byte-for-byte what it was — and naming the turn's own channel is a no-op.
    #[tokio::test]
    async fn omitting_channel_keeps_the_existing_routing_verbatim() {
        let (tool, current_sent, other_sent, other_recipients) = two_channel_tool(test_security(AutonomyLevel::Full));

        for args in [
            json!({"action": "send", "target": "12345", "message": "implicit"}),
            json!({"action": "send", "channel": "dummy", "target": "12345", "message": "explicit-same"}),
            json!({"action": "send", "channel": "", "target": "12345", "message": "blank-is-absent"}),
        ] {
            let result = tool.execute(args).await.unwrap();
            assert!(result.success, "got: {:?}", result.error);
        }

        assert_eq!(
            &*current_sent.lock().await,
            &[
                "implicit".to_string(),
                "explicit-same".to_string(),
                "blank-is-absent".to_string()
            ]
        );
        assert!(other_sent.lock().await.is_empty());
        assert!(other_recipients.lock().await.is_empty());
    }

    /// The turn's task-local channel — not the construction-time slot — is what
    /// `channel` is measured against, so a concurrent turn cannot redefine what
    /// "the current channel" means for this call.
    #[tokio::test]
    async fn explicit_channel_is_measured_against_the_task_local_turn_channel() {
        let (current, current_sent, _r1) = DummyChannel::new_named("dummy");
        let (other, other_sent, _r2) = DummyChannel::new_named("telegram");
        let tool = MessageSendTool::new(
            Arc::clone(&other) as Arc<dyn Channel>,
            test_security(AutonomyLevel::Full),
        )
        .with_channels(registry(&[Arc::clone(&current), Arc::clone(&other)]));

        // The turn is anchored to "dummy" even though the tool was built on
        // "telegram"; asking for "dummy" must therefore stay same-channel.
        let context =
            MessageSendExecutionContext::new(Some("12345".to_string()), Arc::clone(&current) as Arc<dyn Channel>);
        let result = MESSAGE_SEND_EXECUTION_CONTEXT
            .scope(context, async {
                tool.execute(json!({"action": "send", "channel": "dummy", "message": "in-turn"}))
                    .await
            })
            .await
            .unwrap();

        assert!(result.success, "got: {:?}", result.error);
        assert_eq!(&*current_sent.lock().await, &["in-turn".to_string()]);
        assert!(other_sent.lock().await.is_empty());
    }

    /// `react` is delivered by the Signal handle, not by the resolved channel
    /// object, so an explicit `channel` naming anything else is refused rather
    /// than accepted and ignored.
    #[tokio::test]
    async fn react_refuses_an_explicit_channel_it_cannot_honour() {
        let (tool, current_sent, other_sent, _other_recipients) = two_channel_tool(test_security(AutonomyLevel::Full));

        let result = tool
            .execute(json!({
                "action": "react", "channel": "telegram", "target": "12345",
                "emoji": "\u{1F44D}", "target_author": "+19998887777",
                "target_timestamp": 1_700_000_000_000u64
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("Reactions cannot be redirected"), "got: {error}");
        assert!(error.contains("telegram"), "got: {error}");
        assert!(current_sent.lock().await.is_empty());
        assert!(other_sent.lock().await.is_empty());

        // Naming the handle that really delivers is accepted (and then fails on
        // the pre-existing "no Signal channel" path, unchanged).
        let same = tool
            .execute(json!({
                "action": "react", "channel": "dummy", "target": "12345",
                "emoji": "\u{1F44D}", "target_author": "+19998887777",
                "target_timestamp": 1_700_000_000_000u64
            }))
            .await
            .unwrap();
        assert!(!same.success);
        assert!(same.error.unwrap_or_default().contains("Signal"));
    }

    /// An explicit channel routes `edit`/`delete`/`thread` too, once authorized.
    #[tokio::test]
    async fn explicit_channel_routes_the_other_recipient_actions() {
        let (tool, _current_sent, _other_sent, _other_recipients) = two_channel_tool(scoped_security(&["*"], &[]));

        // DummyChannel does not implement these verbs, so success is not the
        // observable here; what matters is that the call reached the *named*
        // channel's default implementation rather than the policy or router.
        for args in [
            json!({"action": "edit", "channel": "telegram", "target": "1", "message_id": "9", "message": "x"}),
            json!({"action": "delete", "channel": "telegram", "target": "1", "message_id": "9"}),
            json!({"action": "thread", "channel": "telegram", "target": "1", "thread_id": "t", "message": "x"}),
        ] {
            let action = args["action"].as_str().unwrap_or_default().to_string();
            let result = tool.execute(args).await.unwrap();
            let error = result.error.unwrap_or_default();
            assert!(
                !error.contains(POLICY_ERROR_MARKER) && !error.contains("Unknown channel"),
                "action {action} must have been routed, got: {error}"
            );
        }
    }

    #[tokio::test]
    async fn outbound_identity_comes_from_the_trusted_scope_only() {
        let (ch, sent) = DummyChannel::new();
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            scope_rules: vec![crate::config::ScopeRule {
                user: Some("uuid:mallory".into()),
                channel: None,
                chat_type: None,
                tools_allow: vec![],
                tools_deny: vec![],
                send_allow: vec![],
                send_deny: vec!["*".into()],
            }],
            ..SecurityPolicy::default()
        });
        let tool = MessageSendTool::new(ch, security);

        // Runtime-injected trusted scope: the rule matches and denies.
        let denied = tool
            .execute(json!({
                "action": "send", "target": "+15550002222", "message": "hi",
                "_zc_scope_trusted": true,
                "_zc_scope": {"sender": "uuid:mallory", "channel": "dummy", "chat_type": "direct"}
            }))
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(denied.error.unwrap_or_default().contains(POLICY_ERROR_MARKER));

        // A model-supplied scope without the trusted marker must not be believed:
        // the identity stays unknown, so mallory's rule cannot be dodged *or*
        // impersonated. Here it simply does not match.
        let untrusted = tool
            .execute(json!({
                "action": "send", "target": "+15550002222", "message": "hi",
                "_zc_scope": {"sender": "uuid:mallory", "channel": "dummy", "chat_type": "direct"}
            }))
            .await
            .unwrap();
        assert!(untrusted.success, "got: {:?}", untrusted.error);
        assert_eq!(sent.lock().await.len(), 1);
    }
}
