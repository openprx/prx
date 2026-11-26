use serde::{Deserialize, Serialize};

use crate::config::agent_name::AgentName;

/// Console transcript/event partition carried on [`MemoryPrincipal`].
///
/// Built only in gateway code from server-side workspace, owner and the
/// incarnation 01b1b-2 resolved or minted. `None` on the principal keeps
/// today's unpartitioned reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPartition {
    pub workspace_id: String,
    pub owner_id: String,
    pub key: PartitionKey,
}

/// Main has no incarnation term; Named always carries the pinned UUID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionKey {
    Main,
    Named { name: AgentName, incarnation: uuid::Uuid },
}

/// Named-partition reads on a backend that does not keep agent scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentScopeUnsupported;

impl std::fmt::Display for AgentScopeUnsupported {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("agent scope is unsupported by this memory backend")
    }
}

impl std::error::Error for AgentScopeUnsupported {}

/// A Named or Main write whose agent scope cannot be derived unambiguously.
///
/// The transaction rolls back and nothing is stored. NULL is never written as a
/// fallback (NULL is Main-visible).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentScopeRefused;

impl std::fmt::Display for AgentScopeRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("agent scope refused")
    }
}

impl std::error::Error for AgentScopeRefused {}

/// A runtime-authenticated scope whose agent partition cannot be read: a
/// partitioned read refuses instead of reading unpartitioned (MA-01h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentPartitionUnreadable;

impl std::fmt::Display for AgentPartitionUnreadable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the agent partition of this call cannot be determined")
    }
}

impl std::error::Error for AgentPartitionUnreadable {}

/// Where a recalled entry came from, as known to the backend that returned it.
///
/// Not serialized: it is a runtime fact about the read, never wire data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MemoryEntryOrigin {
    /// Returned by a backend that stores and scopes the row itself.
    #[default]
    Stored,
    /// Returned by a remote service that cannot prove owner or workspace scope.
    /// Such hits are never used for scoped prompt recall.
    RemoteUnscoped,
}

/// A single memory entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Stored source provenance for excluding a live input from prompt recall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<String>,
    /// Backend provenance of this read result; see [`MemoryEntryOrigin`].
    #[serde(skip)]
    pub origin: MemoryEntryOrigin,
    pub id: String,
    pub key: String,
    pub content: String,
    pub category: MemoryCategory,
    pub timestamp: String,
    pub session_id: Option<String>,
    pub score: Option<f64>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub access_count: Option<u32>,
    #[serde(default)]
    pub useful_count: Option<u32>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub source_confidence: Option<f64>,
    #[serde(default)]
    pub verification_status: Option<VerificationStatus>,
    #[serde(default)]
    pub lifecycle_state: Option<LifecycleState>,
    #[serde(default)]
    pub compressed_from: Option<Vec<String>>,
}

/// Bounded content returned by a selected causal Memory retrieval.
/// The runtime owns the query and principal; these fields are never a tool
/// argument or a substitute for the backend ACL check.
#[derive(Clone, PartialEq, Eq)]
pub struct DecisionMemoryHit {
    pub id: String,
    pub key: String,
    pub category: MemoryCategory,
    pub session_id: Option<String>,
    pub content: String,
    pub created_at: String,
    /// Stored row metadata, not a verified runtime event receipt.
    pub source_event_id: Option<String>,
}

impl std::fmt::Debug for DecisionMemoryHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionMemoryHit")
            .field("content_len", &self.content.len())
            .finish_non_exhaustive()
    }
}

/// Bounded document evidence from a selected, read-only decision query.
/// Row identities are metadata, not execution authority or event receipts.
#[derive(Clone, PartialEq, Eq)]
pub struct DecisionDocumentHit {
    pub chunk_id: String,
    pub document_id: String,
    pub content: String,
    pub created_at: String,
}

impl std::fmt::Debug for DecisionDocumentHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionDocumentHit")
            .field("content_len", &self.content.len())
            .finish_non_exhaustive()
    }
}

/// Bounded historical session text, never a runtime authority receipt.
#[derive(Clone, PartialEq, Eq)]
pub struct DecisionSessionHit {
    pub event_id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

impl std::fmt::Debug for DecisionSessionHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionSessionHit")
            .field("content_len", &self.content.len())
            .finish_non_exhaustive()
    }
}

/// ACL behavior requested by a backend-neutral memory reader.
///
/// `Enforce` returns only entries visible to the runtime principal. `Observe`
/// returns the backend's unrestricted compatibility result while still letting
/// ACL-aware backends evaluate and audit what enforcement would have removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryReadMode {
    Enforce,
    Observe,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatProfile {
    pub id: String,
    pub channel: String,
    pub chat_id: String,
    pub chat_kind: String,
    pub title: Option<String>,
    pub purpose: Option<String>,
    pub notes: Option<String>,
    pub tags: Vec<String>,
    pub updated_by: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Verification state of a memory entry's factual quality.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Unverified,
    Verified,
    Conflicted,
    Deprecated,
}

/// Lifecycle state used by evolution-aware memory retrieval and maintenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Active,
    Archived,
    Tombstoned,
}

/// Memory categories for organization
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    /// Long-term facts, preferences, decisions
    Core,
    /// Daily session logs
    Daily,
    /// Conversation context
    Conversation,
    /// User-defined custom category
    Custom(String),
}

pub fn validate_memory_write_target(key: &str, session_id: Option<&str>) -> anyhow::Result<()> {
    const RESERVED_PREFIXES: &[&str] = &["self/", "router/"];

    anyhow::ensure!(
        !key.starts_with("self/fitness/"),
        "refusing to write fitness telemetry into conversational memory; use FitnessStore"
    );

    if RESERVED_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
        && session_id != Some(crate::self_system::SELF_SYSTEM_SESSION_ID)
    {
        anyhow::bail!("refusing to write reserved memory namespace without self_system session");
    }

    Ok(())
}

impl std::fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core => write!(f, "core"),
            Self::Daily => write!(f, "daily"),
            Self::Conversation => write!(f, "conversation"),
            Self::Custom(name) => write!(f, "{name}"),
        }
    }
}

/// Session metadata for persisted channel conversations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSessionSummary {
    pub session_key: String,
    pub channel: String,
    pub sender: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u64,
    pub last_message_preview: String,
}

/// The `sessions.channel` of a session created under a caller-chosen key
/// (`POST /api/sessions {"session_key"}`): the durable mark of its origin.
pub const CALLER_KEYED_SESSION_CHANNEL: &str = "rf";

/// How claiming a caller-keyed session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionClaim {
    /// No row held the key; it is now a caller-keyed session of the claimant.
    Created,
    /// The key already names a caller-keyed session of the same owner.
    Existing,
    /// The key names a session of another owner, or one of another origin.
    OtherOwner,
}

/// The channel and owner of one stored session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSessionOwner {
    pub channel: String,
    pub owner_id: Option<String>,
}

/// A persisted conversation turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub id: i64,
    pub session_key: String,
    pub role: String,
    pub content: String,
    pub timestamp: String,
    pub message_id: Option<String>,
    /// The `message_events.event_id` this turn was projected from, when the
    /// turn was written through the event fabric.
    #[serde(default)]
    pub message_event_id: Option<String>,
}

/// Visibility scope for message and memory fabric events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryVisibility {
    Global,
    Workspace,
    Agent,
    Session,
    Private,
    System,
}

impl MemoryVisibility {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
            Self::Agent => "agent",
            Self::Session => "session",
            Self::Private => "private",
            Self::System => "system",
        }
    }
}

impl std::fmt::Display for MemoryVisibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MemoryVisibility {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "global" => Ok(Self::Global),
            "workspace" => Ok(Self::Workspace),
            "agent" => Ok(Self::Agent),
            "session" => Ok(Self::Session),
            "private" => Ok(Self::Private),
            "system" => Ok(Self::System),
            other => anyhow::bail!("unknown memory visibility '{other}'"),
        }
    }
}

/// Principal used to filter shared message events for a concrete agent turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryPrincipal {
    pub workspace_id: String,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub session_key: Option<String>,
    pub channel: Option<String>,
    pub sender: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    /// Require `owner_id` equality before admitting `session`- and
    /// `private`-visibility rows (C03 / V02-R4).
    ///
    /// `false` (the default) is the historical contract: `session` rows are
    /// admitted by `session_key` and `private` rows by `sender` / `agent_id` /
    /// `persona_id`. That is correct for channel, chat, cron, xin and sub-agent
    /// principals, whose `session_key` and `sender` already encode one trust
    /// domain.
    ///
    /// `true` is set ONLY by trusted internal context for an externally
    /// authenticated origin — today the MCP execution context, whose session key
    /// (`mcp:{subject}`) and sender are the *external subject only*, so two
    /// identities presenting the same subject under different issuers would
    /// otherwise share both. With this set, those two arms additionally require
    /// `owner_id = principal.owner_id`; a principal that requires the match but
    /// carries no `owner_id` admits no `session`/`private` row at all (fail
    /// closed), and a stored row with a NULL `owner_id` is never admitted.
    /// `workspace` and `global` visibility keep their documented shared
    /// contract.
    ///
    /// This is never read from caller-supplied tool arguments: the execution
    /// service re-authors `_zc_scope` from the authenticated envelope.
    #[serde(default)]
    pub require_owner_match: bool,
    /// Optional pre-cutover (legacy) `session_key` candidate for read-merge.
    ///
    /// When a durable `session_key` is migrated to a canonical format (D4),
    /// the legacy value is carried here so `session`-visibility recall can read
    /// both the new canonical history and the old legacy history as a union
    /// (read-merge, never move). `None` (the default) preserves the historical
    /// single-key behaviour exactly. The legacy key is always read-only.
    #[serde(default)]
    pub legacy_session_key: Option<String>,
    /// Agent history partition. `None` is today's unpartitioned read.
    #[serde(default)]
    pub agent_partition: Option<AgentPartition>,
}

impl Default for MemoryPrincipal {
    fn default() -> Self {
        Self {
            workspace_id: String::new(),
            agent_id: None,
            persona_id: None,
            session_key: None,
            channel: None,
            sender: None,
            owner_id: None,
            require_owner_match: false,
            legacy_session_key: None,
            agent_partition: None,
        }
    }
}

impl MemoryPrincipal {
    /// Refuse a Named partition on backends that do not keep agent scopes.
    ///
    /// # Errors
    ///
    /// [`AgentScopeUnsupported`] when `agent_partition` is Named.
    pub fn reject_named_scope(&self) -> anyhow::Result<()> {
        match self.agent_partition.as_ref().map(|partition| &partition.key) {
            Some(PartitionKey::Named { .. }) => Err(AgentScopeUnsupported.into()),
            _ => Ok(()),
        }
    }

    /// Deduplicated `session_key` candidates for `session`-visibility recall.
    ///
    /// Returns the canonical `session_key` first, followed by the optional
    /// `legacy_session_key` when it is present, non-empty, and distinct from the
    /// canonical key. When no `session_key` is set the list is empty (the
    /// predicate must then match nothing, mirroring the historical
    /// `session_key IS NOT NULL AND session_key = ?` behaviour). When the legacy
    /// key is `None` (or equal to canonical) the list degrades to a single key,
    /// producing byte-identical SQL to the pre-D4 single-key predicate.
    #[must_use]
    pub fn session_key_candidates(&self) -> Vec<String> {
        let mut candidates: Vec<String> = Vec::with_capacity(2);
        if let Some(canonical) = self.session_key.as_deref().map(str::trim).filter(|key| !key.is_empty()) {
            candidates.push(canonical.to_string());
        }
        if let Some(legacy) = self
            .legacy_session_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            if !candidates.iter().any(|existing| existing == legacy) {
                candidates.push(legacy.to_string());
            }
        }
        candidates
    }

    #[must_use]
    pub fn effective_owner_id(&self) -> Option<String> {
        if self.owner_id.as_deref().is_some_and(|owner| !owner.trim().is_empty()) {
            return self.owner_id.clone();
        }

        let channel = self.channel.as_deref()?.trim();
        let sender = self.sender.as_deref()?.trim();
        if channel.is_empty() || sender.is_empty() {
            return None;
        }

        Some(
            crate::memory::principal::OwnerPrincipal::new(
                self.workspace_id.clone(),
                channel,
                sender,
                self.session_key.clone().unwrap_or_default(),
                vec![crate::memory::principal::Role::Anonymous],
            )
            .owner_id,
        )
    }
}

/// Typed origin for an event in the shared message fabric.
///
/// The string representation intentionally matches the legacy `source` column
/// so existing rows and external JSON remain compatible while Rust callers no
/// longer pass an unstructured source string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageEventSource {
    Chat,
    Agent,
    Gateway,
    Channel,
    Console,
    SessionWorker,
    SessionsSpawn,
    Delegate,
    Cron,
    Xin,
    Heartbeat,
    Webhook,
    Other(String),
}

impl MessageEventSource {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Chat => "chat",
            Self::Agent => "agent",
            Self::Gateway => "gateway",
            Self::Channel => "channel",
            Self::Console => "console",
            Self::SessionWorker => "session_worker",
            Self::SessionsSpawn => "sessions_spawn",
            Self::Delegate => "delegate",
            Self::Cron => "cron",
            Self::Xin => "xin",
            Self::Heartbeat => "heartbeat",
            Self::Webhook => "webhook",
            Self::Other(source) => source,
        }
    }
}

impl std::fmt::Display for MessageEventSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq<str> for MessageEventSource {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for MessageEventSource {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<&str> for MessageEventSource {
    fn from(source: &str) -> Self {
        match source.trim() {
            "chat" => Self::Chat,
            "agent" => Self::Agent,
            "gateway" => Self::Gateway,
            "channel" => Self::Channel,
            "console" => Self::Console,
            "session_worker" => Self::SessionWorker,
            "sessions_spawn" => Self::SessionsSpawn,
            "delegate" => Self::Delegate,
            "cron" => Self::Cron,
            "xin" => Self::Xin,
            "heartbeat" => Self::Heartbeat,
            "webhook" => Self::Webhook,
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<String> for MessageEventSource {
    fn from(source: String) -> Self {
        Self::from(source.as_str())
    }
}

impl Serialize for MessageEventSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MessageEventSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

/// Typed subject associated with a message event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum MessageEventSubject {
    Project(String),
    Task(String),
    Incident(String),
    Customer(String),
    Document(String),
    Goal(String),
    Topic(String),
    Conversation(String),
    Other { subject_type: String, id: String },
}

fn legacy_message_event_type() -> String {
    "message.legacy".to_string()
}

/// Input used to append an event into the shared message fabric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEventInput {
    pub event_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub source: MessageEventSource,
    pub channel: Option<String>,
    pub session_key: Option<String>,
    pub parent_session_key: Option<String>,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub role: String,
    #[serde(default)]
    pub event_type: String,
    #[serde(default)]
    pub subject: Option<MessageEventSubject>,
    #[serde(default)]
    pub goal_id: Option<String>,
    #[serde(default)]
    pub causation_event_id: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub lease_epoch: Option<i64>,
    #[serde(default)]
    pub config_generation_id: Option<u64>,
    #[serde(default)]
    pub config_source_revision: Option<String>,
    pub content: String,
    pub raw_payload_json: Option<String>,
    pub visibility: MemoryVisibility,
    /// Agent partition the writing turn runs under (MA-01b3). In-process carrier only:
    /// not serialised, so ingress capsule digests stay byte-identical (R0-2).
    #[serde(skip)]
    pub agent_partition: Option<AgentPartition>,
}

impl MessageEventInput {
    /// Validate the explicit typed metadata required by every backend.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.source.as_str().trim().is_empty(),
            "message event source must not be empty"
        );
        anyhow::ensure!(
            !self.event_type.trim().is_empty(),
            "message event type must not be empty"
        );
        Ok(())
    }
}

/// Persisted shared message event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEvent {
    pub id: i64,
    pub event_id: String,
    pub idempotency_key: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub source: MessageEventSource,
    pub channel: Option<String>,
    pub session_key: Option<String>,
    pub parent_session_key: Option<String>,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub role: String,
    #[serde(default = "legacy_message_event_type")]
    pub event_type: String,
    #[serde(default)]
    pub subject: Option<MessageEventSubject>,
    #[serde(default)]
    pub goal_id: Option<String>,
    #[serde(default)]
    pub causation_event_id: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub lease_epoch: Option<i64>,
    #[serde(default)]
    pub config_generation_id: Option<u64>,
    #[serde(default)]
    pub config_source_revision: Option<String>,
    pub content: String,
    pub content_hash: Option<String>,
    pub raw_payload_json: Option<String>,
    pub visibility: MemoryVisibility,
    pub created_at: String,
    pub updated_at: String,
}

impl MessageEvent {
    pub(crate) fn validate_console_projection(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.id > 0
                && !self.event_id.is_empty()
                && !self.workspace_id.is_empty()
                && self.owner_id.as_deref().is_some_and(|value| !value.is_empty())
                && self.session_key.as_deref().is_some_and(|value| !value.is_empty())
                && self.channel.as_deref().is_some_and(|value| !value.is_empty())
                && self.sender.as_deref().is_some_and(|value| !value.is_empty())
                && self.source == MessageEventSource::Console
                && self.visibility == MemoryVisibility::Session
                && self.event_type == "message.created"
                && matches!(self.role.as_str(), "user" | "assistant"),
            "console projection requires a durable scoped message event"
        );
        Ok(())
    }
}

/// One transcript row matched by [`crate::memory::traits::Memory::search_transcripts`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptHit {
    pub session_key: Option<String>,
    pub event_id: String,
    pub role: String,
    pub created_at: String,
    /// The matched row's raw content, unredacted and without markers: the whole
    /// row up to 65,536 characters; for a longer row, the region of 4,096
    /// characters on each side of the query's first word's first match (ASCII
    /// case-insensitive), or from the row start when that match is not found.
    /// Readers redact secrets on this whole text before cutting or showing an
    /// excerpt of it.
    pub excerpt: String,
}
