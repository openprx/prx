use async_trait::async_trait;

/// A message received from or sent to a channel
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    pub id: String,
    pub sender: String,
    pub reply_target: String,
    pub content: String,
    pub channel: String,
    pub timestamp: u64,
    /// Platform thread identifier (e.g. Slack `ts`, Discord thread ID).
    /// When set, replies should be posted as threaded responses.
    pub thread_ts: Option<String>,
    /// UUIDs/identifiers of users mentioned in this message (e.g. Signal @mentions).
    /// Used by mention_only filter to detect if the bot was explicitly mentioned.
    pub mentioned_uuids: Vec<String>,
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
    pub fn with_subject(
        content: impl Into<String>,
        recipient: impl Into<String>,
        subject: impl Into<String>,
    ) -> Self {
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

    /// Send a message through this channel
    async fn send(&self, message: &SendMessage) -> anyhow::Result<()>;

    /// Start listening for incoming messages (long-running)
    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()>;

    /// Check if channel is healthy
    async fn health_check(&self) -> bool {
        true
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
    async fn update_draft(
        &self,
        _recipient: &str,
        _message_id: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Finalize a draft with the complete response (e.g. apply Markdown formatting).
    async fn finalize_draft(
        &self,
        _recipient: &str,
        _message_id: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
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
    async fn edit_message(
        &self,
        _channel_id: &str,
        _message_id: &str,
        _new_text: &str,
    ) -> anyhow::Result<()> {
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
    async fn send_thread_reply(
        &self,
        _channel_id: &str,
        _thread_id: &str,
        _message: &str,
    ) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "thread reply not supported on this channel"
        ))
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
    let re = regex::Regex::new(r"\[(IMAGE|DOCUMENT|AUDIO|VOICE|VIDEO):([^\]]+)\]")
        .expect("compile regex: outgoing media tag pattern");
    let mut media = Vec::new();
    let clean = re
        .replace_all(text, |caps: &regex::Captures| {
            media.push((caps[1].to_string(), caps[2].to_string()));
            String::new()
        })
        .trim()
        .to_string();
    (clean, media)
}

/// Guess an image MIME type from a file path extension.
pub fn guess_mime_from_path(path: &str) -> &'static str {
    if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else {
        "image/jpeg"
    }
}

/// Guess an audio MIME type from a file path extension.
pub fn guess_audio_mime(path: &str) -> &'static str {
    if path.ends_with(".m4a") {
        "audio/mp4"
    } else if path.ends_with(".mp3") {
        "audio/mpeg"
    } else if path.ends_with(".ogg") || path.ends_with(".oga") {
        "audio/ogg"
    } else if path.ends_with(".wav") {
        "audio/wav"
    } else {
        "audio/mpeg"
    }
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

        async fn listen(
            &self,
            tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            tx.send(ChannelMessage {
                id: "1".into(),
                sender: "tester".into(),
                reply_target: "tester".into(),
                content: "hello".into(),
                channel: "dummy".into(),
                timestamp: 123,
                thread_ts: None,
                mentioned_uuids: vec![],
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
            mentioned_uuids: vec![],
        };

        let cloned = message.clone();
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
        assert!(
            channel
                .send(&SendMessage::new("hello", "bob"))
                .await
                .is_ok()
        );
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
        assert!(
            channel
                .finalize_draft("bob", "msg_1", "final text")
                .await
                .is_ok()
        );
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
