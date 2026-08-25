use crate::channels::activity::LivenessModel;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[allow(clippy::expect_used)]
static RE_OUTGOING_MEDIA: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\[(IMAGE|DOCUMENT|AUDIO|VOICE|VIDEO):([^\]]+)\]")
        .expect("BUG: invalid hardcoded outgoing media tag regex")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChatKind {
    #[default]
    Dm,
    Group,
    Thread,
}

impl ChatKind {
    #[must_use]
    pub const fn scope_chat_type(self) -> &'static str {
        match self {
            Self::Dm => "direct",
            Self::Group => "group",
            Self::Thread => "thread",
        }
    }

    #[must_use]
    pub const fn is_group_like(self) -> bool {
        matches!(self, Self::Group | Self::Thread)
    }
}

/// A message received from or sent to a channel
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    pub id: String,
    pub sender: String,
    pub reply_target: String,
    pub content: String,
    pub channel: String,
    pub timestamp: u64,
    pub chat_kind: ChatKind,
    pub chat_title: Option<String>,
    pub sender_display: Option<String>,
    /// Platform thread identifier (e.g. Slack `ts`, Discord thread ID).
    /// When set, replies should be posted as threaded responses.
    pub thread_ts: Option<String>,
    /// UUIDs/identifiers of users mentioned in this message (e.g. Signal @mentions).
    /// Used by mention_only filter to detect if the bot was explicitly mentioned.
    pub mentioned_uuids: Vec<String>,
    /// Smart group-reply hint: whether the channel layer detected that this
    /// message explicitly mentions the bot (@-mention / reply-to-bot).
    ///
    /// Only meaningful for channels that participate in smart group-reply and
    /// whose `reply_target` does not encode group-ness centrally (Telegram,
    /// Discord). Defaults to `false`; the central pipeline still computes its own
    /// mention detection where applicable, so a `false` here never suppresses a
    /// reply on non-smart paths.
    pub mentioned: bool,
    /// Smart group-reply hint: whether the channel layer knows this message
    /// originated in a group/guild channel.
    ///
    /// Required for channels (Telegram, Discord) whose `reply_target` is a bare
    /// chat/channel id with no `group:` prefix, so the central pipeline cannot
    /// otherwise distinguish group from DM. Defaults to `false`. Used ONLY to
    /// gate smart group-reply behavior (stay_silent exposure + outbound
    /// suppression); it does not change `infer_chat_type_from_message` / scope.
    pub is_group_hint: bool,
    /// Authoritative platform flag: whether the sender is itself a bot account
    /// (Telegram `from.is_bot`, Discord `author.bot`). Anti bot-to-bot-loop
    /// signal for smart proactive replies — the central `is_bot_sender` heuristic
    /// (sender-name suffix) is kept only as a fallback for channels that do not
    /// supply this flag. Defaults to `false`.
    pub sender_is_bot: bool,
}

impl Default for ChannelMessage {
    fn default() -> Self {
        Self {
            id: String::new(),
            sender: String::new(),
            reply_target: String::new(),
            content: String::new(),
            channel: String::new(),
            timestamp: 0,
            chat_kind: ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            thread_ts: None,
            mentioned_uuids: Vec::new(),
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        }
    }
}

/// Message to send through a channel
#[derive(Debug, Clone)]
pub struct SendMessage {
    pub content: String,
    pub recipient: String,
    pub subject: Option<String>,
    /// Platform thread identifier for threaded replies (e.g. Slack `thread_ts`).
    pub thread_ts: Option<String>,
    /// For reply/quote: timestamp of the message being replied to
    pub quote_timestamp: Option<u64>,
    /// For reply/quote: author of the message being replied to
    pub quote_author: Option<String>,
}

impl SendMessage {
    /// Create a new message with content and recipient
    pub fn new(content: impl Into<String>, recipient: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            recipient: recipient.into(),
            subject: None,
            thread_ts: None,
            quote_timestamp: None,
            quote_author: None,
        }
    }

    /// Create a new message with content, recipient, and subject
    pub fn with_subject(content: impl Into<String>, recipient: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            recipient: recipient.into(),
            subject: Some(subject.into()),
            thread_ts: None,
            quote_timestamp: None,
            quote_author: None,
        }
    }

    /// Set the thread identifier for threaded replies.
    pub fn in_thread(mut self, thread_ts: Option<String>) -> Self {
        self.thread_ts = thread_ts;
        self
    }
}

/// Core channel trait — implement for any messaging platform
#[async_trait]
pub trait Channel: Send + Sync {
    /// Human-readable channel name
    fn name(&self) -> &str;

    /// Bot account/name on this platform when known.
    fn bot_identity(&self) -> Option<String> {
        None
    }

    /// Send a message through this channel
    async fn send(&self, message: &SendMessage) -> anyhow::Result<()>;

    /// Start listening for incoming messages (long-running)
    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()>;

    /// Check if channel is healthy
    async fn health_check(&self) -> bool {
        true
    }

    /// What *this* channel's outbound parser would turn into a file upload.
    ///
    /// Single source of truth for the cross-channel "text only" gate. The gate
    /// asks the destination channel rather than re-deriving the answer from a
    /// regex of its own, so it can never drift narrower than what the channel
    /// actually does with the same bytes — which is exactly how a marker the
    /// gate spelled one way and Telegram spelled another slipped through.
    ///
    /// The default is [`conservative_outbound_attachment`], the shared floor.
    /// A channel that accepts more — extra kind aliases, case-insensitive
    /// kinds, bare paths, a syntax of its own — must override this and answer
    /// with its real parser. Answering `None` when the channel would in fact
    /// upload something re-opens an arbitrary-file-read path, so when in doubt
    /// an implementation reports the risk.
    fn outbound_attachment(&self, text: &str) -> Option<OutboundAttachment> {
        conservative_outbound_attachment(text)
    }

    /// How this channel proves that its receive path is still alive.
    ///
    /// Read by the listener supervisor when `listen()` starts, so a wedged
    /// listener can be told apart from a merely quiet one. Channels that complete
    /// an upstream round-trip on a bounded cadence — a long poll the server is
    /// obliged to answer, a gateway heartbeat, a fixed poll interval — return
    /// [`LivenessModel::Bounded`] with the longest gap that is still normal, and
    /// call [`crate::channels::activity::record_upstream`] once per round-trip.
    /// Channels driven purely by push with no keepalive of their own return
    /// [`LivenessModel::Passive`]: silence is genuinely indistinguishable from a
    /// wedge there, so no stall verdict is claimed.
    ///
    /// Report-only. The returned duration describes the channel; it must never be
    /// used as a timeout, a restart trigger or a cancellation deadline.
    fn liveness_expectation(&self) -> LivenessModel {
        LivenessModel::Passive
    }

    /// Signal that the bot is processing a response (e.g. "typing" indicator).
    /// Implementations should repeat the indicator as needed for their platform.
    async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Stop any active typing indicator.
    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether this channel supports progressive message updates via draft edits.
    fn supports_draft_updates(&self) -> bool {
        false
    }

    /// Send an initial draft message. Returns a platform-specific message ID for later edits.
    async fn send_draft(&self, _message: &SendMessage) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// Update a previously sent draft message with new accumulated content.
    async fn update_draft(&self, _recipient: &str, _message_id: &str, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Finalize a draft with the complete response (e.g. apply Markdown formatting).
    async fn finalize_draft(&self, _recipient: &str, _message_id: &str, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Cancel and remove a previously sent draft message if the channel supports it.
    async fn cancel_draft(&self, _recipient: &str, _message_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    // ── P3-2: Extended channel actions ──────────────────────────────────────

    /// Report which extended actions this channel supports.
    /// Defaults to all-false so existing implementations need not change.
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities::default()
    }

    /// Edit a previously sent message.
    ///
    /// `channel_id` is the conversation/chat identifier.
    /// `message_id` is the platform-specific message identifier.
    /// `new_text` is the replacement text.
    ///
    /// Returns `Err` if the platform does not support editing or if the edit fails.
    async fn edit_message(&self, _channel_id: &str, _message_id: &str, _new_text: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("edit not supported on this channel"))
    }

    /// Delete (unsend) a previously sent message.
    ///
    /// `channel_id` is the conversation/chat identifier.
    /// `message_id` is the platform-specific message identifier (timestamp for Signal, etc.).
    ///
    /// Returns `Err` if the platform does not support deletion or if the delete fails.
    async fn delete_message(&self, _channel_id: &str, _message_id: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("delete not supported on this channel"))
    }

    /// Send a reply within a thread.
    ///
    /// `channel_id` is the conversation/chat identifier.
    /// `thread_id` is the platform-specific thread identifier.
    /// `message` is the reply text.
    ///
    /// Channels that do not have a native thread concept should degrade gracefully
    /// (e.g. Signal can fall back to a quote reply).
    async fn send_thread_reply(&self, _channel_id: &str, _thread_id: &str, _message: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("thread reply not supported on this channel"))
    }
}

// ── Channel capabilities ─────────────────────────────────────────────────────

/// Describes which extended messaging actions a channel implementation supports.
#[derive(Debug, Clone, Default)]
pub struct ChannelCapabilities {
    /// Whether the channel supports editing previously sent messages.
    pub edit: bool,
    /// Whether the channel supports deleting (unsending) sent messages.
    pub delete: bool,
    /// Whether the channel natively supports threaded replies.
    pub thread: bool,
    /// Whether the channel supports emoji reactions.
    pub react: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// Shared outgoing-media helpers (used by all channel implementations)
// ──────────────────────────────────────────────────────────────────────────────

/// Extract media markers from outgoing message text.
///
/// The LLM may embed markers such as `[IMAGE:/tmp/foo.png]`, `[VOICE:/tmp/bar.m4a]`,
/// `[AUDIO:…]`, `[VIDEO:…]`, or `[DOCUMENT:…]` in its response. This function
/// strips them out and returns both the cleaned text and the list of
/// `(marker_type, file_path)` pairs so each channel can attach the files.
///
/// # Example
/// ```
/// use openprx::channels::traits::extract_outgoing_media;
/// let (text, media) = extract_outgoing_media("Here you go [IMAGE:/tmp/cat.png] enjoy!");
/// assert_eq!(text, "Here you go  enjoy!");
/// assert_eq!(media, vec![("IMAGE".to_string(), "/tmp/cat.png".to_string())]);
/// ```
pub fn extract_outgoing_media(text: &str) -> (String, Vec<(String, String)>) {
    let mut media = Vec::new();
    let clean = RE_OUTGOING_MEDIA
        .replace_all(text, |caps: &regex::Captures| {
            media.push((caps[1].to_string(), caps[2].to_string()));
            String::new()
        })
        .trim()
        .to_string();
    (clean, media)
}

/// Coarse media class an outgoing `[KIND:path]` marker claims.
///
/// The marker records what the model *intended* to send. The actual type is
/// always decided from the file's own bytes by [`crate::media::type_id`]; this
/// mapping exists so a channel can notice, and log, a disagreement between the
/// two instead of shipping a video labelled as a photo.
pub fn outgoing_marker_category(marker: &str) -> Option<crate::media::MediaCategory> {
    use crate::media::MediaCategory;
    Some(match marker.trim().to_ascii_uppercase().as_str() {
        "IMAGE" | "PHOTO" => MediaCategory::Image,
        "VIDEO" => MediaCategory::Video,
        "VOICE" | "AUDIO" => MediaCategory::Audio,
        _ => return None,
    })
}

/// Marker kinds that *some* channel's outbound parser turns into a file upload.
///
/// Deliberately a union, not the vocabulary of any single channel:
/// [`extract_outgoing_media`] knows five kinds, Telegram additionally accepts
/// `PHOTO`/`FILE` and matches case-insensitively, and a channel added tomorrow
/// may know more. Widening this list can only ever widen a *refusal*, never a
/// delivery, so erring long is the safe direction.
const ATTACHMENT_MARKER_KINDS: &[&str] = &[
    "IMAGE",
    "PHOTO",
    "PICTURE",
    "DOCUMENT",
    "FILE",
    "ATTACHMENT",
    "AUDIO",
    "VOICE",
    "VIDEO",
    "GIF",
    "STICKER",
];

/// The part of an outbound text that a channel would resolve into a file upload
/// instead of printing as words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundAttachment {
    /// A `[KIND:target]` marker. Carries the canonical upper-case kind only —
    /// never the target, which is a local path and must not be echoed back into
    /// an error message.
    Marker(&'static str),
    /// The whole message is a lone path or URL that a channel may upload as-is,
    /// with no marker syntax involved at all.
    BarePath,
}

/// The conservative, channel-agnostic reading of "would this become a file?".
///
/// This is the floor every channel is held to, and a strict superset of
/// [`extract_outgoing_media`]: kinds match case-insensitively, the alias list is
/// the union of every channel's vocabulary, and a lone path-shaped message
/// counts even without marker syntax. Channels whose own parser accepts more
/// override [`Channel::outbound_attachment`] and answer for themselves.
#[must_use]
pub fn conservative_outbound_attachment(text: &str) -> Option<OutboundAttachment> {
    if let Some(kind) = first_attachment_marker_kind(text) {
        return Some(OutboundAttachment::Marker(kind));
    }
    if looks_like_bare_path_message(text) {
        return Some(OutboundAttachment::BarePath);
    }
    None
}

/// First `[KIND:target]` marker whose kind is in [`ATTACHMENT_MARKER_KINDS`].
///
/// Every `[` is tried as an opener, not just the ones a previous match did not
/// consume, so a marker nested behind an unrelated bracket — `[note [IMAGE:/x]`
/// — is still seen. A bracket-scanning parser that resumes past the first `]`
/// misses exactly that case; the shared regex does not, and this must not be
/// narrower than either.
fn first_attachment_marker_kind(text: &str) -> Option<&'static str> {
    for (open, _) in text.match_indices('[') {
        let Some(rest) = text.get(open + 1..) else {
            continue;
        };
        let Some(close) = rest.find(']') else {
            continue;
        };
        let Some(body) = rest.get(..close) else {
            continue;
        };
        let Some((kind, target)) = body.split_once(':') else {
            continue;
        };
        if target.is_empty() {
            continue;
        }
        let kind = kind.trim();
        if let Some(known) = ATTACHMENT_MARKER_KINDS
            .iter()
            .find(|known| kind.eq_ignore_ascii_case(known))
        {
            return Some(known);
        }
    }
    None
}

/// Whether the message is nothing but a path or URL.
///
/// Mirrors the shape Telegram's path-only reading accepts (single token, quote
/// and `file://` stripped) but drops both its file-extension requirement and
/// its existence check: the gate must not depend on what happens to be on disk
/// at the moment it runs.
fn looks_like_bare_path_message(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return false;
    }
    let candidate = trimmed.trim_matches(|c| matches!(c, '`' | '"' | '\''));
    if candidate.is_empty() || candidate.chars().any(char::is_whitespace) {
        return false;
    }
    let candidate = candidate.strip_prefix("file://").unwrap_or(candidate);
    candidate.contains('/') || candidate.contains('\\') || has_file_extension(candidate)
}

/// A trailing `.ext` short enough and plain enough to be a real file suffix.
fn has_file_extension(candidate: &str) -> bool {
    let Some((stem, ext)) = candidate.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty() && !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyChannel;

    #[async_trait]
    impl Channel for DummyChannel {
        fn name(&self) -> &str {
            "dummy"
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }

        async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
            tx.send(ChannelMessage {
                id: "1".into(),
                sender: "tester".into(),
                reply_target: "tester".into(),
                content: "hello".into(),
                channel: "dummy".into(),
                timestamp: 123,
                thread_ts: None,
                chat_kind: crate::channels::traits::ChatKind::Dm,
                chat_title: None,
                sender_display: None,
                mentioned_uuids: vec![],
                mentioned: false,
                is_group_hint: false,
                sender_is_bot: false,
            })
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
    }

    #[test]
    fn channel_message_clone_preserves_fields() {
        let message = ChannelMessage {
            id: "42".into(),
            sender: "alice".into(),
            reply_target: "alice".into(),
            content: "ping".into(),
            channel: "dummy".into(),
            timestamp: 999,
            thread_ts: None,
            chat_kind: crate::channels::traits::ChatKind::Dm,
            chat_title: None,
            sender_display: None,
            mentioned_uuids: vec![],
            mentioned: false,
            is_group_hint: false,
            sender_is_bot: false,
        };

        let cloned = message;
        assert_eq!(cloned.id, "42");
        assert_eq!(cloned.sender, "alice");
        assert_eq!(cloned.reply_target, "alice");
        assert_eq!(cloned.content, "ping");
        assert_eq!(cloned.channel, "dummy");
        assert_eq!(cloned.timestamp, 999);
    }

    #[tokio::test]
    async fn default_trait_methods_return_success() {
        let channel = DummyChannel;

        assert!(channel.health_check().await);
        assert!(channel.start_typing("bob").await.is_ok());
        assert!(channel.stop_typing("bob").await.is_ok());
        assert!(channel.send(&SendMessage::new("hello", "bob")).await.is_ok());
    }

    #[tokio::test]
    async fn default_draft_methods_return_success() {
        let channel = DummyChannel;

        assert!(!channel.supports_draft_updates());
        assert!(
            channel
                .send_draft(&SendMessage::new("draft", "bob"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(channel.update_draft("bob", "msg_1", "text").await.is_ok());
        assert!(channel.finalize_draft("bob", "msg_1", "final text").await.is_ok());
        assert!(channel.cancel_draft("bob", "msg_1").await.is_ok());
    }

    #[tokio::test]
    async fn listen_sends_message_to_channel() {
        let channel = DummyChannel;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        channel.listen(tx).await.unwrap();

        let received = rx.recv().await.expect("message should be sent");
        assert_eq!(received.sender, "tester");
        assert_eq!(received.content, "hello");
        assert_eq!(received.channel, "dummy");
    }
}
