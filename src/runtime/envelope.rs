use crate::memory::principal::{MemoryWriteContext, OwnerPrincipal, Role};
use crate::memory::{MemoryPrincipal, MemoryVisibility, MessageEventScope};

/// Normalized ingress metadata for an agent-runtime turn.
///
/// Existing callers can keep their current session-key formats while deriving
/// memory-fabric, scope, owner, and task lineage inputs from one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEnvelope {
    pub source: RuntimeSource,
    pub workspace_id: String,
    pub session_key: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub channel: Option<String>,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub visibility: MemoryVisibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSource {
    Chat,
    Agent,
    Gateway,
    Channel,
    Console,
    SessionWorker,
    SessionsSpawn,
    Delegate,
}

impl RuntimeSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Agent => "agent",
            Self::Gateway => "gateway",
            Self::Channel => "channel",
            Self::Console => "console",
            Self::SessionWorker => "session_worker",
            Self::SessionsSpawn => "sessions_spawn",
            Self::Delegate => "delegate",
        }
    }
}

impl RuntimeEnvelope {
    /// Internal raw constructor shared by every purpose-specific builder.
    ///
    /// This bypasses the visibility/sender/channel defaults that the public
    /// constructors apply, so it stays private. The public `new` (slated for
    /// deprecation) delegates here.
    fn new_internal(
        source: RuntimeSource,
        workspace_id: impl Into<String>,
        session_key: impl Into<String>,
        visibility: MemoryVisibility,
    ) -> Self {
        Self {
            source,
            workspace_id: workspace_id.into(),
            session_key: session_key.into(),
            owner_id: None,
            topic_id: None,
            task_id: None,
            source_message_event_id: None,
            run_id: None,
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            channel: None,
            sender: None,
            recipient: None,
            visibility,
        }
    }

    /// DEPRECATED — do not use in new code.
    ///
    /// Prefer a purpose-specific constructor:
    /// [`channel`](Self::channel) / [`channel_with_session`](Self::channel_with_session) /
    /// [`agent`](Self::agent) / [`agent_process_message`](Self::agent_process_message) /
    /// [`gateway`](Self::gateway) / [`gateway_webhook`](Self::gateway_webhook) /
    /// [`chat`](Self::chat) / [`chat_terminal`](Self::chat_terminal) /
    /// [`console`](Self::console) / [`session_worker`](Self::session_worker) /
    /// [`sessions_spawn`](Self::sessions_spawn) / [`delegate`](Self::delegate).
    ///
    /// `new` bypasses the visibility/sender/channel defaults that those
    /// constructors apply, which causes default drift across ingress paths.
    ///
    /// All in-crate call sites have been migrated to the purpose-specific
    /// constructors, so the `#[deprecated]` attribute is now attached without
    /// tripping the crate's `-D warnings` build. `#[doc(hidden)]` keeps it out
    /// of the public API surface.
    #[doc(hidden)]
    #[deprecated(note = "use channel()/agent()/gateway_webhook()/chat() — new() bypasses visibility/sender defaults")]
    #[must_use]
    pub fn new(
        source: RuntimeSource,
        workspace_id: impl Into<String>,
        session_key: impl Into<String>,
        visibility: MemoryVisibility,
    ) -> Self {
        Self::new_internal(source, workspace_id, session_key, visibility)
    }

    #[must_use]
    pub fn chat(workspace_id: impl Into<String>, chat_session_id: impl std::fmt::Display) -> Self {
        Self::new_internal(
            RuntimeSource::Chat,
            workspace_id,
            format!("chat:{chat_session_id}"),
            MemoryVisibility::Session,
        )
        .with_channel("terminal")
        .with_sender("local-user")
    }

    /// `Chat` envelope for callers that already hold a fully-formed terminal
    /// session key and need to pick the memory visibility explicitly.
    ///
    /// The interactive [`chat`](Self::chat) builder derives a `chat:{id}` key
    /// and pins `Session` visibility; the terminal scope/runtime helpers
    /// instead pass an existing `chat_session_key` and record at
    /// `Workspace` visibility. This constructor applies the same
    /// `terminal` channel / `local-user` sender defaults so both terminal
    /// entry points share one channel/sender identity.
    #[must_use]
    pub fn chat_terminal(
        workspace_id: impl Into<String>,
        session_key: impl Into<String>,
        visibility: MemoryVisibility,
    ) -> Self {
        Self::new_internal(RuntimeSource::Chat, workspace_id, session_key, visibility)
            .with_channel("terminal")
            .with_sender("local-user")
    }

    #[must_use]
    pub fn agent(workspace_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        let run_id = run_id.into();
        Self::new_internal(
            RuntimeSource::Agent,
            workspace_id,
            format!("agent:{run_id}"),
            MemoryVisibility::Session,
        )
        .with_run_id(run_id)
        .with_channel("cli")
        .with_sender("local-user")
    }

    /// `Agent` envelope for the `process_message` entry path.
    ///
    /// Unlike [`agent`](Self::agent) (interactive CLI, `agent:{run_id}` key,
    /// `cli` channel), this path keeps its own caller-supplied `session_key`
    /// and routes through the `process_message` channel. It applies the same
    /// `local-user` sender / `Session` visibility defaults so the
    /// channel/sender/recipient triad stays consistent with the CLI agent.
    #[must_use]
    pub fn agent_process_message(workspace_id: impl Into<String>, session_key: impl Into<String>) -> Self {
        Self::new_internal(
            RuntimeSource::Agent,
            workspace_id,
            session_key,
            MemoryVisibility::Session,
        )
        .with_channel("process_message")
        .with_sender("local-user")
        .with_recipient("process_message:local-user".to_string())
    }

    #[must_use]
    pub fn gateway_webhook(workspace_id: impl Into<String>, reply_target: Option<&str>) -> Self {
        let target = reply_target
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("webhook-client");
        Self::new_internal(
            RuntimeSource::Gateway,
            workspace_id,
            format!("gateway:webhook:{target}"),
            MemoryVisibility::Session,
        )
        .with_channel("webhook")
        .with_sender("webhook")
        .with_recipient(target.to_string())
    }

    /// `Gateway` envelope for the multimodal chat path, where the channel,
    /// sender, recipient, session key and visibility are all supplied by the
    /// caller's `GatewayFabricContext`.
    ///
    /// [`gateway_webhook`](Self::gateway_webhook) covers the fixed
    /// webhook ingress (constant `webhook` channel/sender, derived session
    /// key); this constructor is for gateway traffic whose routing identity is
    /// resolved upstream and must be carried through verbatim.
    #[must_use]
    pub fn gateway(
        workspace_id: impl Into<String>,
        session_key: impl Into<String>,
        channel: impl Into<String>,
        sender: impl Into<String>,
        recipient: impl Into<String>,
        visibility: MemoryVisibility,
    ) -> Self {
        Self::new_internal(RuntimeSource::Gateway, workspace_id, session_key, visibility)
            .with_channel(channel)
            .with_sender(sender)
            .with_recipient(recipient.into())
    }

    #[must_use]
    pub fn channel(
        workspace_id: impl Into<String>,
        channel: impl Into<String>,
        sender: impl Into<String>,
        chat_id: Option<String>,
    ) -> Self {
        let channel = channel.into();
        let sender = sender.into();
        Self::new_internal(
            RuntimeSource::Channel,
            workspace_id,
            format!("{channel}_{sender}"),
            MemoryVisibility::Session,
        )
        .with_channel(channel)
        .with_sender(sender)
        .with_recipient(chat_id.unwrap_or_default())
    }

    /// `Channel` envelope for callers that already track their own
    /// `session_key` (e.g. the per-sender conversation history key) and need
    /// to choose the memory visibility explicitly.
    ///
    /// [`channel`](Self::channel) derives the `{channel}_{sender}` history key
    /// and pins `Session` visibility; this constructor keeps the caller's key
    /// and visibility while applying the same channel/sender/recipient triad,
    /// so both channel entry points produce identical owner/principal
    /// identities.
    #[must_use]
    pub fn channel_with_session(
        workspace_id: impl Into<String>,
        session_key: impl Into<String>,
        channel: impl Into<String>,
        sender: impl Into<String>,
        recipient: impl Into<String>,
        visibility: MemoryVisibility,
    ) -> Self {
        Self::new_internal(RuntimeSource::Channel, workspace_id, session_key, visibility)
            .with_channel(channel)
            .with_sender(sender)
            .with_recipient(recipient.into())
    }

    #[must_use]
    pub fn console(workspace_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self::new_internal(
            RuntimeSource::Console,
            workspace_id,
            session_id,
            MemoryVisibility::Session,
        )
    }

    #[must_use]
    pub fn session_worker(
        workspace_id: impl Into<String>,
        session_key: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Self {
        Self::new_internal(
            RuntimeSource::SessionWorker,
            workspace_id,
            session_key,
            MemoryVisibility::Workspace,
        )
        .with_run_id(run_id)
    }

    #[must_use]
    pub fn sessions_spawn(
        workspace_id: impl Into<String>,
        session_key: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Self {
        Self::new_internal(
            RuntimeSource::SessionsSpawn,
            workspace_id,
            session_key,
            MemoryVisibility::Workspace,
        )
        .with_run_id(run_id)
    }

    #[must_use]
    pub fn delegate(
        workspace_id: impl Into<String>,
        session_key: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Self {
        Self::new_internal(
            RuntimeSource::Delegate,
            workspace_id,
            session_key,
            MemoryVisibility::Workspace,
        )
        .with_run_id(run_id)
    }

    #[must_use]
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    #[must_use]
    pub fn with_parent_run_id(mut self, parent_run_id: impl Into<String>) -> Self {
        self.parent_run_id = Some(parent_run_id.into());
        self
    }

    #[must_use]
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    #[must_use]
    pub fn with_persona_id(mut self, persona_id: impl Into<String>) -> Self {
        self.persona_id = Some(persona_id.into());
        self
    }

    #[must_use]
    pub fn with_channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = Some(channel.into());
        self
    }

    #[must_use]
    pub fn with_sender(mut self, sender: impl Into<String>) -> Self {
        self.sender = Some(sender.into());
        self
    }

    #[must_use]
    pub fn with_recipient(mut self, recipient: impl Into<String>) -> Self {
        self.recipient = Some(recipient.into());
        self
    }

    #[must_use]
    pub fn with_owner_id(mut self, owner_id: impl Into<String>) -> Self {
        self.owner_id = Some(owner_id.into());
        self
    }

    #[must_use]
    pub fn with_topic_id(mut self, topic_id: impl Into<String>) -> Self {
        self.topic_id = Some(topic_id.into());
        self
    }

    #[must_use]
    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    #[must_use]
    pub fn with_source_message_event_id(mut self, source_message_event_id: impl Into<String>) -> Self {
        self.source_message_event_id = Some(source_message_event_id.into());
        self
    }

    #[must_use]
    pub fn resolved_owner_id(&self) -> String {
        self.owner_id.clone().unwrap_or_else(|| self.owner_principal().owner_id)
    }

    #[must_use]
    pub fn resolved_task_id(&self) -> Option<&str> {
        self.task_id.as_deref().or(self.run_id.as_deref())
    }

    /// Unified, source-agnostic session identity.
    ///
    /// Today every ingress path (chat/agent/gateway/channel/...) builds its own
    /// `session_key` format, so the same user arriving through two entry points
    /// lands in different sessions, breaking memory sharing and task lineage.
    /// This method derives one canonical key from the structured envelope fields
    /// (independent of the legacy `session_key` string), so two envelopes that
    /// describe the same logical conversation produce the same key.
    ///
    /// Format: `{source}:{channel}:{sender}:{recipient}` where any missing
    /// component is replaced with the literal placeholder `-`.
    ///
    /// Migration note (FIX-P1-25b / FIX-P1-02): callers that key on an ad-hoc
    /// `session_key` format should derive their key through this method so all
    /// entry points converge on one deterministic shape.
    ///
    /// The `channel` ingress is the first call site to adopt it: its in-memory
    /// route/history maps and conversation-turn persistence now write under the
    /// canonical key, while every read takes the **union of the canonical and
    /// legacy histories** (read-merge, never move) so turns stored under the old
    /// `{channel}_{sender}` key stay visible rather than orphaned. Because the
    /// legacy key has no recipient component, its history is shared (read-only)
    /// across every recipient of the same sender (see `channels::ConversationKey`
    /// and `channels::merged_history`).
    ///
    /// The `chat`/`agent`/`gateway` durable `session_key`s are intentionally
    /// left on their legacy formats for now: those keys are the primary
    /// `message_events` persistence identity, and swapping them outright would
    /// dark existing shared-event recall. They already derive a consistent
    /// canonical identity through this method (each builder sets
    /// source/channel/sender/recipient), so the cross-mode convergence is
    /// verified at the derivation layer; a recipient-aware durable-key migration
    /// is deferred to a dedicated wave.
    #[must_use]
    pub fn canonical_session_key(&self) -> String {
        fn component(value: Option<&str>) -> &str {
            const PLACEHOLDER: &str = "-";
            value.map(str::trim).filter(|v| !v.is_empty()).unwrap_or(PLACEHOLDER)
        }
        format!(
            "{}:{}:{}:{}",
            self.source.as_str(),
            component(self.channel.as_deref()),
            component(self.sender.as_deref()),
            component(self.recipient.as_deref()),
        )
    }

    #[must_use]
    pub fn memory_principal(&self) -> MemoryPrincipal {
        MemoryPrincipal {
            workspace_id: self.workspace_id.clone(),
            agent_id: self.agent_id.clone(),
            persona_id: self.persona_id.clone(),
            session_key: Some(self.session_key.clone()),
            channel: self.channel.clone(),
            sender: self.sender.clone(),
            owner_id: Some(self.resolved_owner_id()),
        }
    }

    #[must_use]
    pub fn message_scope(&self) -> MessageEventScope {
        let mut scope = MessageEventScope::new(self.source.as_str(), self.visibility.clone())
            .with_owner_id(self.resolved_owner_id())
            .with_session_key(self.session_key.clone());
        if let Some(run_id) = &self.run_id {
            scope = scope.with_run_id(run_id.clone());
        }
        if let Some(parent_run_id) = &self.parent_run_id {
            scope = scope.with_parent_run_id(parent_run_id.clone());
        }
        if let Some(agent_id) = &self.agent_id {
            scope = scope.with_agent_id(agent_id.clone());
        }
        if let Some(persona_id) = &self.persona_id {
            scope = scope.with_persona_id(persona_id.clone());
        }
        if let Some(channel) = &self.channel {
            scope = scope.with_channel(channel.clone());
        }
        if let Some(sender) = &self.sender {
            scope = scope.with_sender(sender.clone());
        }
        if let Some(recipient) = &self.recipient {
            if !recipient.is_empty() {
                scope = scope.with_recipient(recipient.clone());
            }
        }
        scope
    }

    #[must_use]
    pub fn memory_write_context(&self, chat_type: impl Into<String>) -> MemoryWriteContext {
        let recipient = self.recipient.as_deref().filter(|value| !value.is_empty());
        let owner_principal = self.owner_principal();
        MemoryWriteContext {
            channel: self.channel.clone(),
            chat_type: Some(chat_type.into()),
            chat_id: Some(recipient.unwrap_or(self.session_key.as_str()).to_string()),
            sender_id: Some(owner_principal.principal_id),
            raw_sender: self.sender.clone(),
        }
    }

    #[must_use]
    pub fn owner_principal(&self) -> OwnerPrincipal {
        let mut owner = OwnerPrincipal::new(
            self.workspace_id.clone(),
            self.channel.as_deref().unwrap_or(self.source.as_str()),
            self.sender
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| self.agent_id.as_deref().unwrap_or("local-user")),
            self.session_key.clone(),
            vec![Role::Anonymous],
        );
        if let Some(owner_id) = &self.owner_id {
            owner.owner_id = owner_id.clone();
        }
        owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn runtime_envelope_chat_maps_to_existing_scope() {
        let id = Uuid::now_v7();
        let envelope = RuntimeEnvelope::chat("workspace", id);
        assert_eq!(envelope.session_key, format!("chat:{id}"));

        let scope = envelope.message_scope();
        assert_eq!(scope.source, "chat");
        assert_eq!(scope.owner_id.as_deref(), Some("owner:workspace:terminal:local-user"));
        assert_eq!(scope.channel.as_deref(), Some("terminal"));
        assert_eq!(scope.sender.as_deref(), Some("local-user"));

        let principal = envelope.memory_principal();
        assert_eq!(principal.session_key.as_deref(), Some(envelope.session_key.as_str()));

        let write_context = envelope.memory_write_context("private");
        assert_eq!(write_context.channel.as_deref(), Some("terminal"));
        assert_eq!(write_context.chat_id.as_deref(), Some(envelope.session_key.as_str()));
        assert_eq!(write_context.sender_id.as_deref(), Some("terminal:local-user"));
        assert_eq!(write_context.raw_sender.as_deref(), Some("local-user"));
    }

    #[test]
    fn runtime_envelope_derives_owner_principal_from_channel_sender() {
        let envelope = RuntimeEnvelope::channel("workspace", "telegram", "alice", Some("chat-1".to_string()));
        let owner = envelope.owner_principal();

        assert_eq!(owner.workspace_id, "workspace");
        assert_eq!(owner.source_channel, "telegram");
        assert_eq!(owner.external_subject, "alice");
        assert_eq!(owner.principal_id, "telegram:alice");
        assert_eq!(owner.owner_id, "owner:workspace:telegram:alice");

        let write_context = envelope.memory_write_context("dm");
        assert_eq!(write_context.sender_id.as_deref(), Some("telegram:alice"));
        assert_eq!(write_context.raw_sender.as_deref(), Some("alice"));
    }

    #[test]
    fn runtime_envelope_preserves_explicit_owner_topic_task_lineage() {
        let envelope = RuntimeEnvelope::channel("workspace", "telegram", "alice", Some("chat-1".to_string()))
            .with_owner_id("owner:workspace:custom:alice")
            .with_topic_id("topic-1")
            .with_task_id("task-1")
            .with_source_message_event_id("msg-1");

        assert_eq!(envelope.resolved_owner_id(), "owner:workspace:custom:alice");
        assert_eq!(envelope.resolved_task_id(), Some("task-1"));
        assert_eq!(envelope.topic_id.as_deref(), Some("topic-1"));
        assert_eq!(envelope.source_message_event_id.as_deref(), Some("msg-1"));

        let owner = envelope.owner_principal();
        assert_eq!(owner.owner_id, "owner:workspace:custom:alice");
        assert_eq!(owner.principal_id, "telegram:alice");

        let scope = envelope.message_scope();
        assert_eq!(scope.owner_id.as_deref(), Some("owner:workspace:custom:alice"));
    }

    #[test]
    fn runtime_envelope_agent_preserves_current_session_key_format() {
        let run_id = Uuid::now_v7().to_string();
        let envelope = RuntimeEnvelope::agent("workspace", run_id.clone());
        assert_eq!(envelope.session_key, format!("agent:{run_id}"));
        assert_eq!(envelope.channel.as_deref(), Some("cli"));
        assert_eq!(envelope.sender.as_deref(), Some("local-user"));
    }

    #[test]
    fn runtime_envelope_gateway_webhook_preserves_reply_target_session() {
        let envelope = RuntimeEnvelope::gateway_webhook("workspace", Some("client-a"));
        assert_eq!(envelope.session_key, "gateway:webhook:client-a");
        assert_eq!(envelope.channel.as_deref(), Some("webhook"));
        assert_eq!(envelope.recipient.as_deref(), Some("client-a"));
    }

    #[test]
    fn runtime_envelope_channel_preserves_history_key() {
        let envelope = RuntimeEnvelope::channel("workspace", "telegram", "alice", Some("reply-1".to_string()));
        assert_eq!(envelope.session_key, "telegram_alice");
        assert_eq!(envelope.channel.as_deref(), Some("telegram"));
        assert_eq!(envelope.sender.as_deref(), Some("alice"));
        assert_eq!(envelope.recipient.as_deref(), Some("reply-1"));
    }

    #[test]
    fn runtime_envelope_console_keeps_path_session_key() {
        let envelope = RuntimeEnvelope::console("workspace", "session-123");
        assert_eq!(envelope.session_key, "session-123");
        assert_eq!(envelope.message_scope().source, "console");
    }

    #[test]
    fn runtime_envelope_session_worker_preserves_lineage() {
        let envelope = RuntimeEnvelope::session_worker("workspace", "telegram:chat-1:alice", "run-child")
            .with_parent_run_id("run-parent")
            .with_agent_id("agent-a")
            .with_persona_id("persona-a")
            .with_channel("telegram")
            .with_sender("alice")
            .with_recipient("chat-1");

        let scope = envelope.message_scope();
        assert_eq!(scope.source, "session_worker");
        assert_eq!(scope.session_key.as_deref(), Some("telegram:chat-1:alice"));
        assert_eq!(scope.run_id.as_deref(), Some("run-child"));
        assert_eq!(scope.parent_run_id.as_deref(), Some("run-parent"));
        assert_eq!(scope.agent_id.as_deref(), Some("agent-a"));
        assert_eq!(scope.persona_id.as_deref(), Some("persona-a"));

        let principal = envelope.memory_principal();
        assert_eq!(principal.agent_id.as_deref(), Some("agent-a"));
        assert_eq!(principal.persona_id.as_deref(), Some("persona-a"));
    }

    #[test]
    fn runtime_envelope_sessions_spawn_uses_task_pool_source() {
        let envelope = RuntimeEnvelope::sessions_spawn("workspace", "signal:group:test", "run-child")
            .with_parent_run_id("run-parent")
            .with_channel("signal")
            .with_sender("alice")
            .with_recipient("group");
        let scope = envelope.message_scope();
        assert_eq!(scope.source, "sessions_spawn");
        assert_eq!(scope.session_key.as_deref(), Some("signal:group:test"));
        assert_eq!(scope.parent_run_id.as_deref(), Some("run-parent"));
    }

    #[test]
    fn runtime_envelope_delegate_uses_delegate_source() {
        let envelope = RuntimeEnvelope::delegate("workspace", "delegate:telegram:chat-1:alice", "run-delegate")
            .with_agent_id("tester")
            .with_channel("telegram")
            .with_sender("alice")
            .with_recipient("chat-1");
        let scope = envelope.message_scope();
        assert_eq!(scope.source, "delegate");
        assert_eq!(scope.agent_id.as_deref(), Some("tester"));
        assert_eq!(scope.session_key.as_deref(), Some("delegate:telegram:chat-1:alice"));
    }

    #[test]
    fn canonical_session_key_uses_structured_components() {
        let envelope = RuntimeEnvelope::channel("workspace", "telegram", "alice", Some("chat-1".to_string()));
        assert_eq!(envelope.canonical_session_key(), "channel:telegram:alice:chat-1");
    }

    #[test]
    fn canonical_session_key_fills_missing_components_with_placeholder() {
        // console() sets no channel/sender/recipient.
        let envelope = RuntimeEnvelope::console("workspace", "session-123");
        assert_eq!(envelope.canonical_session_key(), "console:-:-:-");
    }

    #[test]
    fn canonical_session_key_treats_blank_components_as_missing() {
        // channel() with an empty chat_id stores an empty recipient string;
        // it must collapse to the placeholder, not an empty segment.
        let envelope = RuntimeEnvelope::channel("workspace", "telegram", "alice", None);
        assert_eq!(envelope.canonical_session_key(), "channel:telegram:alice:-");
    }

    #[test]
    fn canonical_session_key_converges_for_same_user_across_legacy_session_keys() {
        // Two envelopes that describe the same logical conversation but were
        // built with different legacy session_key formats must still produce
        // one identical canonical key.
        let from_channel = RuntimeEnvelope::channel("workspace", "telegram", "alice", Some("chat-1".to_string()));

        let from_worker = RuntimeEnvelope::session_worker(
            "workspace",
            "telegram:chat-1:alice", // legacy worker session_key format
            "run-1",
        )
        .with_channel("telegram")
        .with_sender("alice")
        .with_recipient("chat-1");

        // Legacy session_key strings differ...
        assert_ne!(from_channel.session_key, from_worker.session_key);
        // ...but the canonical key is source-scoped per entry point.
        assert_eq!(from_channel.canonical_session_key(), "channel:telegram:alice:chat-1");
        assert_eq!(
            from_worker.canonical_session_key(),
            "session_worker:telegram:alice:chat-1"
        );
        // The channel/sender/recipient tail (the user identity portion) matches,
        // which is what downstream session convergence keys on.
        let tail = |key: &str| key.split_once(':').map(|(_, rest)| rest.to_string());
        assert_eq!(
            tail(&from_channel.canonical_session_key()),
            tail(&from_worker.canonical_session_key())
        );
    }

    #[test]
    fn canonical_session_key_is_deterministic_across_the_four_runtime_modes() {
        // FIX-P1-25b / FIX-P1-02: chat/agent/gateway/channel each derive their
        // session identity through the one canonical method, so a given mode +
        // routing identity always yields the same canonical key (no per-call-site
        // drift). The source prefix differs by design (it scopes the entry
        // point); the channel/sender/recipient tail is the shared user identity.

        // chat (terminal): fixed terminal channel + local-user sender.
        let chat = RuntimeEnvelope::chat_terminal("ws", "chat:42", MemoryVisibility::Workspace)
            .with_recipient("openrouter/gpt");
        assert_eq!(chat.canonical_session_key(), "chat:terminal:local-user:openrouter/gpt");

        // agent (CLI): fixed cli channel + local-user sender.
        let agent = RuntimeEnvelope::agent("ws", "run-7");
        assert_eq!(agent.canonical_session_key(), "agent:cli:local-user:-");

        // gateway: routing identity supplied verbatim by the caller.
        let gateway = RuntimeEnvelope::gateway(
            "ws",
            "gw-session-1",
            "webchat",
            "user-9",
            "agent-bot",
            MemoryVisibility::Session,
        );
        assert_eq!(gateway.canonical_session_key(), "gateway:webchat:user-9:agent-bot");

        // channel: derives from channel/sender/chat_id, matching the key the
        // channels ingress builds for its ConversationKey.
        let channel = RuntimeEnvelope::channel("ws", "telegram", "alice", Some("chat-1".to_string()));
        assert_eq!(channel.canonical_session_key(), "channel:telegram:alice:chat-1");

        // Re-deriving from a fresh builder with the same inputs is stable.
        let channel_again = RuntimeEnvelope::channel("ws", "telegram", "alice", Some("chat-1".to_string()));
        assert_eq!(channel.canonical_session_key(), channel_again.canonical_session_key());
    }
}
