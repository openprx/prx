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
use crate::config::Config;
use crate::runtime::tasks_cli::{TasksEndpoint, request_channel_send};
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

tokio::task_local! {
    /// The channel a turn *originates* on, for surfaces that run an agent turn
    /// without owning a messaging channel.
    static OUTBOUND_ORIGIN_CHANNEL: Arc<str>;
}

/// Run `future` with the turn's origin channel recorded for outbound scope
/// matching.
///
/// A channel-driven turn does not need this: the channel it arrived on is the
/// channel it replies through, so the reply handle already names the origin.
/// The gateway is different — it runs webhook turns against whichever channel
/// handle the daemon happened to register (a Signal handle, in practice), so
/// without this the origin of *every* gateway turn reads as that handle's name
/// and an operator's `channel = "whatsapp"` rule can never match one. Naming
/// the origin here keeps the reply routing exactly as it is and only changes
/// which scope rules are consulted.
pub async fn with_outbound_origin_channel<F>(channel: &str, future: F) -> F::Output
where
    F: std::future::Future,
{
    OUTBOUND_ORIGIN_CHANNEL.scope(Arc::from(channel), future).await
}

/// The origin channel of the turn in scope, if one was named.
///
/// Visible to the crate so a surface that installs an origin can assert it is
/// actually in scope for the turn it wraps.
pub(crate) fn current_outbound_origin_channel() -> Option<Arc<str>> {
    OUTBOUND_ORIGIN_CHANNEL.try_with(Arc::clone).ok()
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

/// The one tool name both `message_send` entry points answer to.
///
/// `prx chat` holds no channel object of its own, so its variant reaches the
/// daemon over HTTP. The public name remains stable across entrypoints, while
/// each implementation publishes only the operations it can execute.
pub(crate) const MESSAGE_SEND_TOOL_NAME: &str = "message_send";

fn message_send_operation(channel: &str, recipient: &str) -> String {
    let recipient_ref = op_id::ref_for_channel_recipient(channel, recipient);
    op_id::op_id(MESSAGE_SEND_TOOL_NAME, "send", &[channel, &recipient_ref])
}

/// Tool-list summary for the channel-owned `message_send`. Kept beside the
/// schema, and out of the trait method, so the contract test can hold it to the
/// same standard without standing up a channel.
const CHANNEL_MESSAGE_SEND_DESCRIPTION: &str = "Send a message on the active messaging channel (Signal, Telegram, …). Text, file/image/voice \
     attachments, emoji reactions and quote replies. Use action='send' to send and action='react' \
     to react; 'edit' rewrites and 'delete'/'unsend' withdraws an earlier message by `message_id`; \
     'thread' replies inside a thread; set 'channel' to deliver text on another configured channel.";

/// Tool-list summary for the daemon-brokered `message_send`.
const DAEMON_MESSAGE_SEND_DESCRIPTION: &str = "Send a message through one of the PRX daemon's messaging channels (Signal, Telegram, wacli, etc.). \
     This chat session holds no channel of its own, so the send is performed by the daemon: \
     'channel' is required, only action='send' is available, and the daemon's outbound scope \
     rules decide whether the recipient may be reached.";

/// Parameter schema for a channel-owned `message_send` implementation.
fn channel_message_send_parameters_schema() -> serde_json::Value {
    let mut schema = crate::tools::schema::with_default_action_requirements(
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["send", "react", "edit", "delete", "unsend", "thread"],
                    "description": "'send' text/files/voice (message); 'react' emoji reaction; 'edit' (message_id + message); \
                                    'delete'/'unsend' (message_id); 'thread' (thread_id + message)."
                },
                "target": {
                    "type": "string",
                    "description": "Recipient (phone number, group ID, Signal UUID, …). Defaults to the current sender."
                },
                "channel": {
                    "type": "string",
                    "description": "Destination channel (e.g. 'signal', 'telegram', 'wacli'); omit to stay on the current one. \
                                    Another channel must be allowed by the outbound scope rules and carries text only \
                                    (media markers and as_voice are refused). Not accepted for 'react'."
                },
                "message": {
                    "type": "string",
                    "description": "Message text, required by 'send', 'edit' and 'thread'. Embed media with markers \
                                    [IMAGE:path], [VOICE:path], [DOCUMENT:path]; text outside a marker becomes the caption."
                },
                "as_voice": {
                    "type": "boolean",
                    "description": "Send the first [VOICE:]/[AUDIO:] attachment as a voice note (default false)."
                },
                "quote_timestamp": {
                    "type": "integer",
                    "description": "Timestamp (ms) of the message to quote-reply to."
                },
                "quote_author": {
                    "type": "string",
                    "description": "Author of the quoted message (required with quote_timestamp)."
                },
                "emoji": {
                    "type": "string",
                    "description": "'react': the emoji, e.g. 👍."
                },
                "target_author": {
                    "type": "string",
                    "description": "'react': author of the target message."
                },
                "target_timestamp": {
                    "type": "integer",
                    "description": "'react': timestamp (ms) of the target message."
                },
                "message_id": {
                    "type": "string",
                    "description": "'edit'/'delete'/'unsend': platform message id (ms timestamp on Signal)."
                },
                "thread_id": {
                    "type": "string",
                    "description": "'thread': the thread/conversation id to reply into."
                }
            },
            "required": ["action"]
        }),
        "action",
        "send",
        &[
            crate::tools::schema::ActionRequirement {
                action: "send",
                required: &["message"],
            },
            crate::tools::schema::ActionRequirement {
                action: "react",
                required: &["emoji", "target_author", "target_timestamp"],
            },
            crate::tools::schema::ActionRequirement {
                action: "edit",
                required: &["message_id", "message"],
            },
            crate::tools::schema::ActionRequirement {
                action: "delete",
                required: &["message_id"],
            },
            crate::tools::schema::ActionRequirement {
                action: "unsend",
                required: &["message_id"],
            },
            crate::tools::schema::ActionRequirement {
                action: "thread",
                required: &["thread_id", "message"],
            },
        ],
    );
    if let Some(all_of) = schema.get_mut("allOf").and_then(serde_json::Value::as_array_mut) {
        all_of.push(json!({
            "if": {"required": ["quote_timestamp"]},
            "then": {"required": ["quote_author"]}
        }));
    }
    schema
}

/// Parameter schema for chat and process workers that send through the daemon.
///
/// These callers have no inbound channel or recipient to inherit and expose
/// only the daemon HTTP send operation, so their public contract must be
/// narrower than the channel-owned implementation.
fn daemon_message_send_parameters_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {
                "type": "string",
                "enum": ["send"],
                "default": "send",
                "description": "Send text through a configured PRX daemon channel. Defaults to 'send' when omitted."
            },
            "channel": {
                "type": "string",
                "minLength": 1,
                "description": "Configured daemon channel name."
            },
            "target": {
                "type": "string",
                "minLength": 1,
                "description": "Recipient identifier for the selected daemon channel."
            },
            "message": {
                "type": "string",
                "minLength": 1,
                "description": "Text to send."
            },
            "as_voice": {
                "type": "boolean",
                "default": false,
                "description": "Ask the destination channel to deliver the text as voice when supported."
            }
        },
        "required": ["channel", "target", "message"]
    })
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

    /// Resolve routing without performing outbound authorization or I/O.
    async fn routing_defaults(&self, args: &serde_json::Value) -> (Arc<dyn Channel>, Option<String>) {
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

        (channel, target)
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
    pub(crate) fn reject_cross_channel_media(args: &serde_json::Value, dst_channel: &str) -> Result<(), String> {
        Self::reject_cross_channel_media_on(args, dst_channel, None)
    }

    /// The same refusal, with the destination channel available to answer for
    /// itself.
    ///
    /// Two readings are consulted and either one refuses: the shared
    /// conservative floor, and the destination's own parser via
    /// [`Channel::outbound_attachment`]. Asking the destination is what keeps
    /// the gate from being narrower than the parser it is guarding — Telegram
    /// uppercases marker kinds, knows the `PHOTO`/`FILE` aliases and uploads a
    /// bare path with no marker at all, none of which the shared marker regex
    /// can see. Keeping the floor as well means a channel that answers `None`
    /// too eagerly still cannot open the hole back up.
    ///
    /// Neither branch echoes the target: it is a local path, and a refusal is
    /// no place to disclose the layout of the machine.
    pub(crate) fn reject_cross_channel_media_on(
        args: &serde_json::Value,
        dst_channel: &str,
        destination: Option<&dyn Channel>,
    ) -> Result<(), String> {
        use crate::channels::traits::{OutboundAttachment, conservative_outbound_attachment};

        let message = args.get("message").and_then(serde_json::Value::as_str).unwrap_or("");
        let attachment = conservative_outbound_attachment(message)
            .or_else(|| destination.and_then(|channel| channel.outbound_attachment(message)));
        match attachment {
            Some(OutboundAttachment::Marker(marker)) => {
                return Err(format!(
                    "Cross-channel delivery to '{dst_channel}' is text-only, but this message embeds a \
                     [{marker}:] media marker. Attachments are local paths owned by the originating \
                     channel and cannot be handed to another channel. Send the text across channels, \
                     or send the media on the current channel."
                ));
            }
            Some(OutboundAttachment::BarePath) => {
                return Err(format!(
                    "Cross-channel delivery to '{dst_channel}' is text-only, but this message is a bare \
                     path or URL, which '{dst_channel}' uploads as an attachment instead of printing as \
                     text. Attachments are local paths owned by the originating channel and cannot be \
                     handed to another channel. Describe the file in words, or send it on the current \
                     channel."
                ));
            }
            None => {}
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
    /// `reply_channel` is the channel this turn replies through and `dst_channel`
    /// the one the message actually leaves on. They differ exactly when the
    /// caller passed an explicit `channel`; with no `channel` argument they are
    /// the same object, which is what keeps this a no-op unless an operator
    /// configures `send_allow` / `send_deny`.
    ///
    /// Scope rules are matched against the turn's *origin* instead, which is the
    /// reply channel for every channel-driven turn and the channel named by
    /// [`with_outbound_origin_channel`] on the gateway's webhook surface. The
    /// same-channel default keeps measuring against `reply_channel`, so naming
    /// an origin can only bring more rules into play — never mute a turn that
    /// no rule constrains.
    ///
    /// The rejection message carries only the stable audit fingerprint of the
    /// destination, never the plaintext recipient.
    fn authorize_outbound(
        &self,
        args: &serde_json::Value,
        reply_channel: &str,
        dst_channel: &str,
        recipient: &str,
    ) -> Result<(), String> {
        let (sender, chat_type) = trusted_outbound_identity(args);
        let origin = current_outbound_origin_channel();
        let src_channel = origin.as_deref().unwrap_or(reply_channel);
        if self.security.is_outbound_allowed_for_turn(
            crate::tools::effective_autonomy::call_policy(&self.security).autonomy,
            &sender,
            src_channel,
            reply_channel,
            &chat_type,
            dst_channel,
            recipient,
        ) {
            return Ok(());
        }
        let recipient_ref = op_id::ref_for_channel_recipient(dst_channel, recipient);
        Err(crate::security::redaction::redact_secrets(&format!(
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
            Self::reject_cross_channel_media_on(args, destination.name(), Some(destination.as_ref()))?;
        }
        Ok(destination)
    }
}

#[async_trait]
impl Tool for MessageSendTool {
    fn name(&self) -> &str {
        MESSAGE_SEND_TOOL_NAME
    }

    fn description(&self) -> &str {
        CHANNEL_MESSAGE_SEND_DESCRIPTION
    }

    fn parameters_schema(&self) -> serde_json::Value {
        channel_message_send_parameters_schema()
    }

    async fn resolve_approval_operation(
        &self,
        args: &serde_json::Value,
    ) -> anyhow::Result<Option<(String, crate::acl::approval_grant::RiskLevel)>> {
        if args.get("action").and_then(serde_json::Value::as_str).unwrap_or("send") != "send" {
            return Ok(None);
        }
        // An implicit recipient must come from this task's turn. Mutable
        // legacy defaults cannot establish another turn's approval scope.
        anyhow::ensure!(
            current_message_send_execution_context().is_some()
                || (requested_channel(args).is_some()
                    && args
                        .get("target")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|value| !value.is_empty())),
            "message approval requires turn-owned routing or explicit channel and target"
        );
        let (current, target) = self.routing_defaults(args).await;
        let recipient = target
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("message approval recipient missing"))?;
        let destination = self
            .resolve_outbound_route(args, &current)
            .map_err(anyhow::Error::msg)?;
        Ok(Some((
            message_send_operation(destination.name(), &recipient),
            crate::acl::approval_grant::RiskLevel::Medium,
        )))
    }

    async fn set_active_recipient(&self, recipient: &str) {
        *self.default_recipient.write().await = Some(recipient.to_string());
    }

    async fn set_active_channel(&self, channel: Arc<dyn crate::channels::traits::Channel>) {
        *self.active_channel.write().await = channel;
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Security guard: autonomy check
        if !crate::tools::effective_autonomy::call_policy(&self.security).can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("send");

        let (channel, target) = self.routing_defaults(&args).await;

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
                let operation_name = message_send_operation(destination.name(), &recipient);
                let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                if let Err(error) = SideEffectGate::new(&crate::tools::effective_autonomy::call_policy(&self.security))
                    .authorize_resource_operation(
                        self.name(),
                        &operation_name,
                        ResourceRiskLevel::Medium,
                        approval_grant.as_ref(),
                    )
                {
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

// ── Chat-side variant: outbound through the daemon ──────────────────────────

/// `message_send` for a process that holds no channel of its own.
///
/// `prx chat` deliberately opens no IM connection: doing so would put a second
/// listener on the same account and race the daemon for inbound messages. So
/// its outbound goes the other way — the daemon, which already owns the channel
/// objects, is asked to send. The tool name stays stable, while its schema is
/// narrowed to the operation and context actually available in this process.
///
/// The local side-effect gate checks the exact requested channel and recipient
/// before an HTTP request. Destination resolution, outbound authorization and
/// media restrictions remain owned by the daemon under its own configuration.
/// A local approval cannot replace or bypass those destination-side checks.
pub struct DaemonMessageSendTool {
    endpoint: TasksEndpoint,
    security: Arc<SecurityPolicy>,
}

impl DaemonMessageSendTool {
    /// Build from the chat process's configuration.
    ///
    /// The endpoint defaults to the configured gateway bind address — the same
    /// default `prx tasks` uses — so a local daemon needs no extra setup beyond
    /// a control-plane token.
    #[must_use]
    pub fn from_config(config: &Config, security: Arc<SecurityPolicy>) -> Self {
        let daemon = &config.chat.daemon;
        Self {
            endpoint: TasksEndpoint::resolve(
                config,
                Some(daemon.url.clone()).filter(|url| !url.trim().is_empty()),
                Some(daemon.token.clone()).filter(|token| !token.trim().is_empty()),
            ),
            security,
        }
    }

    /// Arguments this route cannot carry, and must therefore refuse.
    ///
    /// Dropping them silently would be worse than failing: the model would be
    /// told its quoted reply was sent when a plain message went out instead.
    fn reject_uncarried_arguments(args: &serde_json::Value) -> Result<(), String> {
        const UNCARRIED: [&str; 2] = ["quote_timestamp", "quote_author"];
        let named: Vec<&str> = UNCARRIED
            .into_iter()
            .filter(|key| args.get(*key).is_some_and(|value| !value.is_null()))
            .collect();
        if named.is_empty() {
            return Ok(());
        }
        Err(format!(
            "Outbound from chat is routed through the PRX daemon, which carries text only: {} \
             cannot be honoured. Drop it, or run the send from the channel itself.",
            named.join(", ")
        ))
    }
}

#[async_trait]
impl Tool for DaemonMessageSendTool {
    fn name(&self) -> &str {
        MESSAGE_SEND_TOOL_NAME
    }

    fn description(&self) -> &str {
        DAEMON_MESSAGE_SEND_DESCRIPTION
    }

    fn parameters_schema(&self) -> serde_json::Value {
        daemon_message_send_parameters_schema()
    }

    async fn resolve_approval_operation(
        &self,
        args: &serde_json::Value,
    ) -> anyhow::Result<Option<(String, crate::acl::approval_grant::RiskLevel)>> {
        if args.get("action").and_then(serde_json::Value::as_str).unwrap_or("send") != "send" {
            return Ok(None);
        }
        let channel = requested_channel(args).ok_or_else(|| anyhow::anyhow!("message approval channel missing"))?;
        let recipient = args
            .get("target")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("message approval recipient missing"))?;
        Self::reject_uncarried_arguments(args).map_err(anyhow::Error::msg)?;
        Ok(Some((
            message_send_operation(channel, recipient),
            crate::acl::approval_grant::RiskLevel::Medium,
        )))
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !crate::tools::effective_autonomy::call_policy(&self.security).can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        let refuse = |error: String| {
            Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            })
        };

        let action = args.get("action").and_then(serde_json::Value::as_str).unwrap_or("send");
        if action != "send" {
            return refuse(format!(
                "Action '{action}' is not available from chat: outbound here is routed through the PRX \
                 daemon, which exposes sending only. Use action='send'."
            ));
        }
        let Some(channel) = requested_channel(&args) else {
            return refuse(
                "Missing 'channel': this chat session has no conversation channel to inherit, so the \
                 destination channel must be named explicitly."
                    .to_string(),
            );
        };
        let Some(recipient) = args
            .get("target")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|target| !target.is_empty())
        else {
            return refuse(
                "Missing 'target': a send through the daemon has no conversation to reply to, so the \
                 recipient must be given."
                    .to_string(),
            );
        };
        let message = args
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if message.is_empty() {
            return refuse("Missing 'message': nothing to send.".to_string());
        }
        if let Err(error) = Self::reject_uncarried_arguments(&args) {
            return refuse(error);
        }
        let as_voice = args
            .get("as_voice")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let operation = message_send_operation(channel, recipient);
        let approval = ApprovalGrant::from_runtime_args(self.name(), &args);
        if let Err(error) = SideEffectGate::new(&crate::tools::effective_autonomy::call_policy(&self.security))
            .authorize_resource_operation(self.name(), &operation, ResourceRiskLevel::Medium, approval.as_ref())
        {
            return refuse(error);
        }

        match request_channel_send(&self.endpoint, channel, recipient, message, as_voice).await {
            Ok(report) => Ok(ToolResult {
                success: report.delivered,
                output: if report.delivered {
                    format!("{} (via the PRX daemon on channel '{}')", report.detail, report.channel)
                } else {
                    String::new()
                },
                error: (!report.delivered).then(|| report.detail.clone()),
            }),
            // A refusal, an unreachable daemon and a channel error all arrive
            // here as one error chain. It is reported, not retried and not
            // reinterpreted: the chat turn continues either way.
            Err(error) => refuse(crate::security::redaction::redact_provider_text(&format!("{error:#}"))),
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
mod tests;
