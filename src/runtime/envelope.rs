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
    /// Pre-cutover (legacy) durable `session_key` for D4 read-merge.
    ///
    /// When `session_key` holds a migrated canonical value, this carries the
    /// old legacy key so recall reads both as a union (read-merge, never move).
    /// `None` preserves single-key behaviour. Always read-only.
    pub legacy_session_key: Option<String>,
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
            legacy_session_key: None,
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

    /// Stable durable-canonical `session_key` for a chat session (D4 / D7).
    ///
    /// The recipient component is derived from the immutable `chat_session.id`
    /// (NOT `{provider}/{model}`, which would split one logical conversation
    /// across model switches), so the canonical key is stable for the lifetime
    /// of the session. The format matches [`Self::canonical_session_key`] for a
    /// `terminal` channel / `local-user` sender:
    /// `chat:terminal:local-user:{chat_session_id}`.
    #[must_use]
    pub fn chat_canonical_session_key(chat_session_id: &str) -> String {
        format!("chat:terminal:local-user:{chat_session_id}")
    }

    /// `Chat` envelope with a stable durable-canonical `session_key` plus the
    /// legacy `chat:{id}` key carried for read-merge (D4 C6).
    ///
    /// Both the write scope ([`Self::message_scope`]) and the read principal
    /// ([`Self::memory_principal`]) derive their durable `session_key` from this
    /// one envelope, so they are guaranteed identical (asserted in C2 tests).
    #[must_use]
    pub fn chat_canonical(
        workspace_id: impl Into<String>,
        chat_session_id: &str,
        visibility: MemoryVisibility,
    ) -> Self {
        let canonical = Self::chat_canonical_session_key(chat_session_id);
        let legacy = format!("chat:{chat_session_id}");
        Self::new_internal(RuntimeSource::Chat, workspace_id, canonical, visibility)
            .with_channel("terminal")
            .with_sender("local-user")
            .with_recipient(chat_session_id.to_string())
            .with_legacy_session_key(legacy)
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

    /// Carry the pre-cutover (legacy) durable `session_key` for D4 read-merge.
    ///
    /// Empty/whitespace values are ignored (no legacy key). The legacy key flows
    /// into [`Self::memory_principal`] so `session`-visibility recall reads both
    /// the canonical and legacy histories as a union.
    #[must_use]
    pub fn with_legacy_session_key(mut self, legacy_session_key: impl Into<String>) -> Self {
        let legacy = legacy_session_key.into();
        self.legacy_session_key = if legacy.trim().is_empty() { None } else { Some(legacy) };
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
    /// Durable-key migration status (D4 — durable canonical + legacy read-merge):
    /// the recipient-aware durable-key migration that was previously deferred is
    /// now done for the `chat` and `gateway` *fabric* paths. Each writes its
    /// durable `message_events` `session_key` as the recipient-aware canonical
    /// (chat: stable `chat:terminal:local-user:{session_id}` derived from the
    /// immutable session id — see [`Self::chat_canonical`]; gateway fabric:
    /// `gateway:{channel}:{sender}:{recipient}`), and every read takes the
    /// **union of the canonical and the pre-cutover legacy key**
    /// (`legacy_session_key`) so legacy history stays visible (read-merge, never
    /// move; the legacy row is never updated or deleted). Two exceptions are kept
    /// deliberately legacy: the `agent` per-turn `agent:{turn_run_id}` write key
    /// (run-boundary isolation — agent does read-merge only, no write-side
    /// canonical collapse), and the gateway **console** external `session_id`
    /// (a user-visible path/list contract value; canonicalizing it would break
    /// the frontend). The `chat` blob storage key `chat_session:{id}` is also
    /// kept on its session-id basis (whole-blob overwrite, no recipient
    /// semantics) — only the fine-grained `message_events` `session_key` is
    /// migrated.
    ///
    /// **No cross-mode exact recall claim.** Because the canonical key's first
    /// component is the `source` (`chat:` / `agent:` / `gateway:` / ...), the
    /// canonical keys of two different sources are structurally never equal. D4
    /// therefore does **not** make one user's chat / agent / gateway histories
    /// visible to each other via session-key union — that union only merges the
    /// legacy and canonical history **within a single source**. Cross-mode
    /// sharing is carried by the `owner_id` / `sender` / workspace visibility
    /// dimensions, not by the session key. The derivation-layer convergence tests
    /// (`canonical_session_key_*`) verify key-derivation determinism only and must
    /// not be read as a cross-mode recall guarantee.
    ///
    /// # Session contract across `chat` / `gateway` / `channels` (D7)
    ///
    /// `canonical_session_key` is the *one* shared layer between the three
    /// session subsystems. Everything else about how they store and reload a
    /// conversation differs, which is why D7 deliberately does **not** abstract
    /// a `trait SessionManager{load,save,list}` over them — the common contract
    /// surface measures ~8% (well under the 50% threshold the layer-D plan set
    /// for introducing a shared trait). Forcing a trait would only yield
    /// per-implementation `downcast`/empty/`unimplemented!()` methods (a dead
    /// abstraction that violates iron rules 2/3), so D7 stops at unifying the
    /// key-derivation layer (this method) plus documenting the contract here.
    /// This mirrors the prior P1-27 decision (single-implementer presentation
    /// stack → no `ModeRunner` trait).
    ///
    /// | dimension | `chat` | `gateway` | `channels` |
    /// |---|---|---|---|
    /// | storage | single `MemoryEntry` JSON blob (whole-session) + `message_events` rows | fabric `message_events` rows + console `conversation_turns` (append-only) + `conversation_sessions` meta | in-process `HashMap<String, Vec<ChatMessage>>` cache + `conversation_turns` rows |
    /// | key model | blob `chat_session:{id}` (session-id basis, **not** via `canonical_session_key`); message_events durable key = stable canonical `chat:terminal:local-user:{id}` (D4) + legacy `chat:{id}` read-merge | fabric durable key = canonical `gateway:{ch}:{sender}:{recipient}` (D4) + legacy `gateway:…` read-merge; console external `session_id` kept **legacy** (contract) | `ConversationKey{canonical, legacy}`: canonical via `canonical_session_key`, legacy `{channel}_{sender}` |
    /// | load | exact `get(chat_session:{id})` → deser blob; message_events recall reads **union** of canonical + legacy session keys | fabric recall reads **union** of canonical + legacy keys; console meta `get_conversation_session` then paged `list_conversation_turns` on the external id | `merged_history` = read-merge **union** of canonical + legacy keys from the cache |
    /// | save | blob whole-blob **overwrite** (Pure-mode single-writer via `Effect::SaveSession`, `dual_write_guard` suppresses legacy side-writes); message_events **append** under canonical key | fabric per-event **append** under canonical key; console per-turn **append** under external id | cache push + per-turn DB **append** |
    /// | invariant | Pure-mode single-source blob; message_events read-merge union, legacy key **read-only** | read-merge union, legacy key **read-only, never moved/deleted** | read-merge union; legacy key **read-only, never moved/deleted** (`fdfd8ec0`) |
    ///
    /// Three storage models, three key models, three load return shapes
    /// (`Option<ChatSession>` / paged `Vec<SessionMessage>` / `Vec<ChatMessage>`)
    /// — there is no shared `Session` type or `save`/`list` semantics to abstract,
    /// only this key derivation. The `chat` blob key is intentionally kept on its
    /// `chat_session:{id}` form (see the durable-key migration note above); see
    /// [`crate::chat::session::ChatSession::memory_key`] for that special case.
    ///
    /// Error semantics are unified separately by D10 (load paths distinguish
    /// `None` "no such session" from `Err` storage failure and fail fast), not by
    /// a trait — each subsystem's degradation strategy stays storage-specific.
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
            // D4: carry the pre-cutover legacy key so `session`-visibility recall
            // read-merges canonical + legacy. `None` preserves single-key recall.
            legacy_session_key: self.legacy_session_key.clone(),
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

    // ---- D4 C2: legacy-key derivation + stable chat canonical identity ----

    #[test]
    fn chat_canonical_session_key_derives_from_stable_session_id() {
        // Stable across model/provider switches: derived purely from session id.
        let key = RuntimeEnvelope::chat_canonical_session_key("sess-123");
        assert_eq!(key, "chat:terminal:local-user:sess-123");
    }

    #[test]
    fn chat_canonical_envelope_write_and_read_durable_key_are_strictly_equal() {
        // C2 core assertion: the write scope and read principal derive their
        // durable session_key from one envelope, so they are identical and equal
        // to the stable canonical (NOT the legacy `chat:{id}`).
        let envelope = RuntimeEnvelope::chat_canonical("workspace", "sess-abc", MemoryVisibility::Workspace);
        let canonical = "chat:terminal:local-user:sess-abc";

        let scope = envelope.message_scope();
        let principal = envelope.memory_principal();
        // Write durable key == read durable key == stable canonical.
        assert_eq!(scope.session_key.as_deref(), Some(canonical));
        assert_eq!(principal.session_key.as_deref(), Some(canonical));
        assert_eq!(scope.session_key, principal.session_key);
        // Legacy key carried for read-merge, distinct from canonical.
        assert_eq!(principal.legacy_session_key.as_deref(), Some("chat:sess-abc"));
        assert_eq!(envelope.canonical_session_key(), canonical);
        // The candidate set unions canonical + legacy.
        assert_eq!(
            principal.session_key_candidates(),
            vec![
                "chat:terminal:local-user:sess-abc".to_string(),
                "chat:sess-abc".to_string()
            ]
        );
    }

    #[test]
    fn with_legacy_session_key_ignores_blank_and_flows_into_principal() {
        let blank = RuntimeEnvelope::chat("workspace", "id-1").with_legacy_session_key("   ");
        assert_eq!(blank.legacy_session_key, None);
        assert_eq!(blank.memory_principal().legacy_session_key, None);

        let set = RuntimeEnvelope::chat("workspace", "id-1").with_legacy_session_key("chat:old");
        assert_eq!(set.legacy_session_key.as_deref(), Some("chat:old"));
        assert_eq!(set.memory_principal().legacy_session_key.as_deref(), Some("chat:old"));
    }

    #[test]
    fn non_chat_envelopes_carry_no_legacy_key_by_default() {
        // Single-key behaviour preserved for sources that have not migrated.
        let agent = RuntimeEnvelope::agent("ws", "run-7");
        assert_eq!(agent.memory_principal().legacy_session_key, None);
        let gateway = RuntimeEnvelope::gateway(
            "ws",
            "gw-1",
            "webchat",
            "user-9",
            "agent-bot",
            MemoryVisibility::Session,
        );
        assert_eq!(gateway.memory_principal().legacy_session_key, None);
    }
}
