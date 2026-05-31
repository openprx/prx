use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::memory::principal::MemoryWriteContext;

/// A single memory entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
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

/// A persisted conversation turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub id: i64,
    pub session_key: String,
    pub role: String,
    pub content: String,
    pub timestamp: String,
    pub message_id: Option<String>,
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
}

impl MemoryPrincipal {
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

/// Input used to append an event into the shared message fabric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEventInput {
    pub event_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub source: String,
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
    pub content: String,
    pub raw_payload_json: Option<String>,
    pub visibility: MemoryVisibility,
}

/// Persisted shared message event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEvent {
    pub id: i64,
    pub event_id: String,
    pub idempotency_key: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub source: String,
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
    pub content: String,
    pub content_hash: Option<String>,
    pub raw_payload_json: Option<String>,
    pub visibility: MemoryVisibility,
    pub created_at: String,
    pub updated_at: String,
}

/// Query for recent shared events visible to an agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedContextQuery {
    pub principal: MemoryPrincipal,
    pub since_event_id: Option<i64>,
    pub limit: usize,
    pub include_roles: Vec<String>,
}

/// Query for recent current-session events visible to an agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContextQuery {
    pub principal: MemoryPrincipal,
    pub since_event_id: Option<i64>,
    pub limit: usize,
    pub include_roles: Vec<String>,
}

/// Input used to append a lightweight outbox event for memory fabric changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEventInput {
    pub event_id: Option<String>,
    pub workspace_id: String,
    pub event_type: String,
    pub subject_table: String,
    pub subject_id: String,
    pub session_key: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub visibility: MemoryVisibility,
    pub payload_json: Option<String>,
}

/// Persisted memory fabric outbox event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvent {
    pub id: i64,
    pub event_id: String,
    pub workspace_id: String,
    pub event_type: String,
    pub subject_table: String,
    pub subject_id: String,
    pub session_key: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub visibility: MemoryVisibility,
    pub payload_json: Option<String>,
    pub created_at: String,
}

/// Optional metadata persisted with semantic memory entries.
///
/// This is intentionally backend-neutral so SQLite can persist it now and
/// Postgres can implement the same contract without changing entrypoint code.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryStoreMetadata {
    pub workspace_id: Option<String>,
    pub owner_id: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub source_event_id: Option<String>,
    pub source: Option<String>,
    /// Optional topic scope. When absent, backends may fall back to
    /// `source_event_id` so Project-visibility memories remain scope-resolvable
    /// (FIX-P0-23).
    pub topic_id: Option<String>,
    /// Optional originating channel (e.g. discord/slack/cli). Persisted so that
    /// anonymous principals can still resolve channel scope on later recall
    /// (FIX-P1-08).
    pub channel: Option<String>,
}

/// A private worker memory draft waiting for parent merge/reject.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDraftInput {
    pub draft_id: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub worker_run_id: String,
    pub parent_run_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub key: String,
    pub content: String,
    pub category: MemoryCategory,
    pub source_event_id: Option<String>,
    pub visibility: MemoryVisibility,
    pub payload_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDraft {
    pub id: i64,
    pub draft_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub worker_run_id: String,
    pub parent_run_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub key: String,
    pub content: String,
    pub category: MemoryCategory,
    pub source_event_id: Option<String>,
    pub visibility: MemoryVisibility,
    pub status: String,
    pub payload_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentIngestInput {
    pub document_id: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub source_kind: String,
    pub source_uri: Option<String>,
    pub title: Option<String>,
    pub content: String,
    pub mime_type: Option<String>,
    pub visibility: MemoryVisibility,
    pub metadata_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentRecord {
    pub id: i64,
    pub document_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub source_kind: String,
    pub source_uri: Option<String>,
    pub title: Option<String>,
    pub content_sha256: String,
    pub mime_type: Option<String>,
    pub visibility: MemoryVisibility,
    pub metadata_json: Option<String>,
    pub chunk_count: usize,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChunkRecord {
    pub id: i64,
    pub chunk_id: String,
    pub document_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub task_id: Option<String>,
    pub chunk_index: usize,
    pub heading: Option<String>,
    pub content: String,
    pub content_sha256: String,
    pub source_anchor: String,
    pub token_estimate: usize,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSearchResult {
    pub chunk: DocumentChunkRecord,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievedContextItem {
    pub source: String,
    pub document_id: Option<String>,
    pub chunk_id: Option<String>,
    pub source_anchor: Option<String>,
    pub score: Option<f32>,
    pub token_estimate: Option<usize>,
    pub payload_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLinkInput {
    pub link_id: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub memory_key: Option<String>,
    pub memory_event_id: Option<String>,
    pub message_event_id: Option<String>,
    pub document_id: String,
    pub chunk_id: Option<String>,
    pub link_type: String,
    pub payload_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLink {
    pub id: i64,
    pub link_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub memory_key: Option<String>,
    pub memory_event_id: Option<String>,
    pub message_event_id: Option<String>,
    pub document_id: String,
    pub chunk_id: Option<String>,
    pub link_type: String,
    pub payload_json: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalTraceInput {
    pub trace_id: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub source: String,
    pub query: String,
    pub candidate_count: usize,
    pub selected_count: usize,
    pub dropped_count: usize,
    pub budget_tokens: Option<usize>,
    pub selected_json: Option<String>,
    pub dropped_json: Option<String>,
    pub payload_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalTrace {
    pub id: i64,
    pub trace_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub source: String,
    pub query: String,
    pub candidate_count: usize,
    pub selected_count: usize,
    pub dropped_count: usize,
    pub budget_tokens: Option<usize>,
    pub selected_json: Option<String>,
    pub dropped_json: Option<String>,
    pub payload_json: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRunInput {
    pub run_id: Option<String>,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub trigger: String,
    pub mode: String,
    pub source_message_count: usize,
    pub source_token_estimate: usize,
    pub summary: String,
    pub summary_memory_key: Option<String>,
    pub source_event_ids_json: Option<String>,
    pub source_document_refs_json: Option<String>,
    pub fidelity_status: String,
    pub payload_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRun {
    pub id: i64,
    pub run_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub trigger: String,
    pub mode: String,
    pub source_message_count: usize,
    pub source_token_estimate: usize,
    pub summary: String,
    pub summary_memory_key: Option<String>,
    pub source_event_ids_json: Option<String>,
    pub source_document_refs_json: Option<String>,
    pub fidelity_status: String,
    pub payload_json: Option<String>,
    pub created_at: String,
}

/// Core memory trait — implement for any persistence backend
#[async_trait]
pub trait Memory: Send + Sync {
    /// Backend name
    fn name(&self) -> &str;

    /// Store a memory entry, optionally scoped to a session
    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()>;

    /// Store a memory entry with optional channel/tool context.
    ///
    /// Backends that do not support context-aware metadata can ignore `context`
    /// by relying on this default implementation.
    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<()> {
        let _ = context;
        self.store(key, content, category, session_id).await
    }

    /// Store a memory entry with portable fabric metadata.
    ///
    /// Backends that have not implemented metadata persistence can safely fall
    /// back to `store`, preserving compatibility while SQLite/Postgres converge
    /// on the same schema.
    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        metadata: MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        let _ = metadata;
        self.store(key, content, category, session_id).await
    }

    /// Store a memory entry with both channel/tool context and fabric metadata.
    async fn store_with_context_and_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        metadata: MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        let _ = metadata;
        self.store_with_context(key, content, category, session_id, context)
            .await
    }

    /// Recall memories matching a query (keyword search), optionally scoped to a session
    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>>;

    /// Recall memories matching a query with optional runtime ACL/write context.
    ///
    /// Backends without ACL-aware recall support fall back to legacy recall.
    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let _ = context;
        self.recall(query, limit, session_id).await
    }

    /// Get a specific memory by key
    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>>;

    /// List all memory keys, optionally filtered by category and/or session
    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>>;

    /// Remove a memory by key
    async fn forget(&self, key: &str) -> anyhow::Result<bool>;

    /// Remove a memory by key after applying a runtime write/read scope.
    ///
    /// Backends that do not support ACL metadata fall back to legacy key
    /// deletion for compatibility; SQLite overrides this with scoped deletion.
    async fn forget_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> anyhow::Result<bool> {
        let _ = context;
        self.forget(key).await
    }

    /// Increment the usefulness feedback counter for a recalled memory entry.
    ///
    /// Backends that do not persist `useful_count` can safely no-op.
    async fn increment_useful_count(&self, id: &str) -> anyhow::Result<()> {
        let _ = id;
        Ok(())
    }

    /// Count total memories
    async fn count(&self) -> anyhow::Result<usize>;

    /// Health check
    async fn health_check(&self) -> bool;

    /// Rebuild backend search indexes and backfill stale vectors when supported.
    async fn reindex(&self) -> anyhow::Result<usize> {
        Ok(0)
    }

    /// Whether this backend can durably ingest source documents and chunks
    /// (FIX-P0-28). Backends with full document support (SQLite/Postgres) keep the
    /// default `true`. Backends without it (`NoneMemory`, `MarkdownMemory`) return
    /// `false` so callers can skip ingestion and gracefully degrade to plain
    /// history compaction instead of emitting a fail-fast error on every large
    /// tool output.
    fn supports_document_ingest(&self) -> bool {
        true
    }

    /// Append a persisted channel conversation turn.
    ///
    /// Backends without conversation persistence can no-op.
    #[allow(clippy::too_many_arguments)]
    async fn append_conversation_turn(
        &self,
        session_key: &str,
        channel: &str,
        sender: &str,
        role: &str,
        content: &str,
        timestamp: Option<&str>,
        message_id: Option<&str>,
        owner_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let _ = (
            session_key,
            channel,
            sender,
            role,
            content,
            timestamp,
            message_id,
            owner_id,
        );
        Ok(())
    }

    /// List persisted conversation sessions ordered by most recently updated.
    async fn list_conversation_sessions(
        &self,
        limit: usize,
        offset: usize,
        channel: Option<&str>,
    ) -> anyhow::Result<Vec<ConversationSessionSummary>> {
        let _ = (limit, offset, channel);
        Ok(Vec::new())
    }

    /// Get one persisted conversation session by key.
    async fn get_conversation_session(&self, session_key: &str) -> anyhow::Result<Option<ConversationSessionSummary>> {
        let _ = session_key;
        Ok(None)
    }

    /// List persisted turns for a conversation session (oldest first).
    async fn list_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        let _ = (principal, session_key, limit, offset);
        Ok(Vec::new())
    }

    /// Load recent turns per session for runtime history hydration.
    async fn load_recent_conversation_histories(
        &self,
        principal: &MemoryPrincipal,
        max_turns_per_session: usize,
        max_sessions: usize,
    ) -> anyhow::Result<HashMap<String, Vec<ConversationTurn>>> {
        let _ = (principal, max_turns_per_session, max_sessions);
        Ok(HashMap::new())
    }

    /// Append a normalized message event into the shared memory fabric.
    ///
    /// Backends without event-log support must fail fast instead of returning
    /// synthetic success, so runtime timeline loss is visible to callers.
    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        let _ = input;
        Err(crate::memory::fabric::fail_fast(self.name(), "append_message_event"))
    }

    /// List message events visible to `principal` after a cursor.
    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (principal, after_id, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_message_events_since",
        ))
    }

    /// List the most recent message events visible to `principal`.
    async fn list_message_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (principal, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_message_events_recent",
        ))
    }

    /// Load recent shared context events visible to an agent turn.
    async fn load_recent_shared_context(&self, query: SharedContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = query;
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "load_recent_shared_context",
        ))
    }

    /// Load recent current-session context events visible to an agent turn.
    async fn load_recent_session_context(&self, query: SessionContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = query;
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "load_recent_session_context",
        ))
    }

    /// Append a memory outbox event.
    async fn append_memory_event(&self, input: MemoryEventInput) -> anyhow::Result<MemoryEvent> {
        let _ = input;
        Err(crate::memory::fabric::fail_fast(self.name(), "append_memory_event"))
    }

    /// List memory outbox events visible to `principal` after a cursor.
    async fn list_memory_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        let _ = (principal, after_id, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_memory_events_since",
        ))
    }

    /// List the most recent memory outbox events visible to `principal`.
    async fn list_memory_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        let _ = (principal, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_memory_events_recent",
        ))
    }

    /// Create a private worker memory draft.
    async fn create_memory_draft(&self, input: MemoryDraftInput) -> anyhow::Result<MemoryDraft> {
        let _ = input;
        Err(crate::memory::fabric::fail_fast(self.name(), "create_memory_draft"))
    }

    /// List memory drafts produced by a worker run, scoped to `principal`.
    ///
    /// Only drafts owned by the principal (or with no owner) are returned, so a
    /// caller that merely knows a `worker_run_id` cannot read another owner's
    /// drafts.
    async fn list_memory_drafts_for_run(
        &self,
        principal: &MemoryPrincipal,
        worker_run_id: &str,
    ) -> anyhow::Result<Vec<MemoryDraft>> {
        let _ = (principal, worker_run_id);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_memory_drafts_for_run",
        ))
    }

    /// Mark a draft as merged into semantic memory, enforcing owner ACL.
    ///
    /// Returns `Err` if the draft is owned by a different principal.
    async fn merge_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        let _ = (principal, draft_id);
        Err(crate::memory::fabric::fail_fast(self.name(), "merge_memory_draft"))
    }

    /// Reject a draft without merging it into semantic memory, enforcing owner ACL.
    ///
    /// Returns `Err` if the draft is owned by a different principal.
    async fn reject_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        let _ = (principal, draft_id, reason);
        Err(crate::memory::fabric::fail_fast(self.name(), "reject_memory_draft"))
    }

    /// Persist a new evolution proposal draft (FIX-P0-03).
    ///
    /// Writes one `evolution_proposals` row plus a `proposal.drafted` event and
    /// returns the stored `draft_id`. Backends without evolution support keep the
    /// default `Err` so callers see backend gaps instead of silent loss.
    async fn create_evolution_proposal(
        &self,
        draft: crate::self_system::evolution::EvolutionProposalDraft,
    ) -> anyhow::Result<String> {
        let _ = draft;
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "create_evolution_proposal",
        ))
    }

    /// List evolution proposals visible to `principal`, filtered by `filter`.
    ///
    /// Non-system principals are constrained to their own `owner_id`; system
    /// principals (`self_system`/`router`/`internal`/`system`) may query globally.
    async fn list_evolution_proposals(
        &self,
        principal: &MemoryPrincipal,
        filter: crate::self_system::evolution::ProposalFilter,
    ) -> anyhow::Result<Vec<crate::self_system::evolution::EvolutionProposalDraft>> {
        let _ = (principal, filter);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_evolution_proposals",
        ))
    }

    /// Fetch one proposal by id, enforcing owner ACL.
    ///
    /// Returns `Ok(None)` for a missing OR cross-owner draft (NotFound rather
    /// than Forbidden) to avoid a cross-owner existence side channel.
    async fn get_evolution_proposal(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<crate::self_system::evolution::EvolutionProposalDraft>> {
        let _ = (principal, draft_id);
        Err(crate::memory::fabric::fail_fast(self.name(), "get_evolution_proposal"))
    }

    /// Apply a status transition (judge / apply / rollback) to a proposal.
    ///
    /// Each transition appends the matching `evolution_proposal_events` row and
    /// mutates the proposal columns. Re-judging an already-judged proposal is
    /// rejected to prevent verdict laundering.
    async fn update_evolution_proposal_status(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        update: crate::self_system::evolution::ProposalStatusUpdate,
    ) -> anyhow::Result<()> {
        let _ = (principal, draft_id, update);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "update_evolution_proposal_status",
        ))
    }

    /// Append a free-form lifecycle event to an existing proposal.
    ///
    /// Used for non-state-changing markers such as `proposal.evidence_mismatch`.
    async fn append_evolution_proposal_event(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        event_type: &str,
        actor: &str,
        payload_json: Option<&str>,
    ) -> anyhow::Result<()> {
        let _ = (principal, draft_id, event_type, actor, payload_json);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "append_evolution_proposal_event",
        ))
    }

    /// Count proposals visible to `principal` matching `filter` (cheap status probe).
    async fn count_evolution_proposals(
        &self,
        principal: &MemoryPrincipal,
        filter: crate::self_system::evolution::ProposalFilter,
    ) -> anyhow::Result<usize> {
        let proposals = self.list_evolution_proposals(principal, filter).await?;
        Ok(proposals.len())
    }

    /// Soft-delete a memory key into a trash holding area with a grace expiry
    /// (FIX-P1-11). Unlike `forget`, this MUST NOT physically remove the row;
    /// it snapshots the value and marks it recoverable until `grace_until`.
    ///
    /// Returns the trash entry id on success, or `Ok(None)` if the key is absent.
    /// Backends without trash support keep the default `Err` so callers do not
    /// silently fall back to a hard delete.
    async fn move_to_trash(&self, key: &str, reason: &str, grace_days: u32) -> anyhow::Result<Option<String>> {
        let _ = (key, reason, grace_days);
        Err(crate::memory::fabric::fail_fast(self.name(), "move_to_trash"))
    }

    /// Ingest a durable source document and create chunk records.
    async fn ingest_document(&self, input: DocumentIngestInput) -> anyhow::Result<DocumentRecord> {
        let _ = input;
        Err(crate::memory::document::fail_fast(self.name(), "ingest_document"))
    }

    /// Search document chunks visible to a principal.
    async fn search_document_chunks(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DocumentSearchResult>> {
        let _ = (principal, query, limit);
        Err(crate::memory::document::fail_fast(
            self.name(),
            "search_document_chunks",
        ))
    }

    /// Retrieve one document chunk by stable chunk id.
    async fn get_document_chunk(&self, chunk_id: &str) -> anyhow::Result<Option<DocumentChunkRecord>> {
        let _ = chunk_id;
        Err(crate::memory::document::fail_fast(self.name(), "get_document_chunk"))
    }

    /// Link a memory/event/summary to a source document chunk.
    async fn link_memory_source(&self, input: MemoryLinkInput) -> anyhow::Result<MemoryLink> {
        let _ = input;
        Err(crate::memory::document::fail_fast(self.name(), "link_memory_source"))
    }

    /// Append a retrieval trace for context packing and evidence audits.
    async fn append_retrieval_trace(&self, input: RetrievalTraceInput) -> anyhow::Result<RetrievalTrace> {
        let _ = input;
        Err(crate::memory::retrieval::fail_fast(
            self.name(),
            "append_retrieval_trace",
        ))
    }

    /// Append a compaction run audit record.
    async fn append_compaction_run(&self, input: CompactionRunInput) -> anyhow::Result<CompactionRun> {
        let _ = input;
        Err(crate::memory::compaction::fail_fast(
            self.name(),
            "append_compaction_run",
        ))
    }
}

#[cfg(test)]
pub(crate) mod conformance {
    use super::{Memory, MemoryCategory};
    use crate::memory::principal::MemoryWriteContext;

    pub(crate) async fn assert_scoped_memory_acl_conformance(mem: &dyn Memory, key_prefix: &str) {
        let alice_ctx = MemoryWriteContext {
            channel: Some("telegram".into()),
            chat_type: Some("private".into()),
            chat_id: Some(format!("{key_prefix}:dm-alice")),
            sender_id: None,
            raw_sender: Some("alice".into()),
        };
        let bob_ctx = MemoryWriteContext {
            channel: Some("telegram".into()),
            chat_type: Some("private".into()),
            chat_id: Some(format!("{key_prefix}:dm-bob")),
            sender_id: None,
            raw_sender: Some("bob".into()),
        };
        let alice_key = format!("{key_prefix}:alice-private");
        let bob_key = format!("{key_prefix}:bob-private");

        mem.store_with_context(
            &alice_key,
            "shared conformance keyword alice private",
            MemoryCategory::Conversation,
            None,
            Some(&alice_ctx),
        )
        .await
        .expect("store alice scoped memory");
        mem.store_with_context(
            &bob_key,
            "shared conformance keyword bob private",
            MemoryCategory::Conversation,
            None,
            Some(&bob_ctx),
        )
        .await
        .expect("store bob scoped memory");

        let alice_results = mem
            .recall_with_context("shared conformance keyword", 10, None, Some(&alice_ctx))
            .await
            .expect("recall alice scoped memory");
        let alice_keys = alice_results.iter().map(|entry| entry.key.as_str()).collect::<Vec<_>>();
        assert!(alice_keys.contains(&alice_key.as_str()), "{alice_keys:?}");
        assert!(!alice_keys.contains(&bob_key.as_str()), "{alice_keys:?}");

        assert!(
            !mem.forget_with_context(&bob_key, Some(&alice_ctx))
                .await
                .expect("alice cannot delete bob memory"),
            "alice context must not delete bob scoped memory"
        );
        assert!(
            mem.forget_with_context(&alice_key, Some(&alice_ctx))
                .await
                .expect("alice deletes alice memory"),
            "alice context should delete alice scoped memory"
        );
        assert!(
            mem.get(&bob_key).await.expect("bob memory remains").is_some(),
            "bob memory should remain after denied alice delete"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_category_display_outputs_expected_values() {
        assert_eq!(MemoryCategory::Core.to_string(), "core");
        assert_eq!(MemoryCategory::Daily.to_string(), "daily");
        assert_eq!(MemoryCategory::Conversation.to_string(), "conversation");
        assert_eq!(
            MemoryCategory::Custom("project_notes".into()).to_string(),
            "project_notes"
        );
    }

    #[test]
    fn memory_category_serde_uses_snake_case() {
        let core = serde_json::to_string(&MemoryCategory::Core).unwrap();
        let daily = serde_json::to_string(&MemoryCategory::Daily).unwrap();
        let conversation = serde_json::to_string(&MemoryCategory::Conversation).unwrap();

        assert_eq!(core, "\"core\"");
        assert_eq!(daily, "\"daily\"");
        assert_eq!(conversation, "\"conversation\"");
    }

    #[test]
    fn memory_entry_roundtrip_preserves_optional_fields() {
        let entry = MemoryEntry {
            id: "id-1".into(),
            key: "favorite_language".into(),
            content: "Rust".into(),
            category: MemoryCategory::Core,
            timestamp: "2026-02-16T00:00:00Z".into(),
            session_id: Some("session-abc".into()),
            score: Some(0.98),
            tags: Some(vec!["language".into(), "preference".into()]),
            access_count: Some(4),
            useful_count: Some(3),
            source: Some("task-2026-02-16".into()),
            source_confidence: Some(0.92),
            verification_status: Some(VerificationStatus::Verified),
            lifecycle_state: Some(LifecycleState::Active),
            compressed_from: Some(vec!["old-1".into(), "old-2".into()]),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: MemoryEntry = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "id-1");
        assert_eq!(parsed.key, "favorite_language");
        assert_eq!(parsed.content, "Rust");
        assert_eq!(parsed.category, MemoryCategory::Core);
        assert_eq!(parsed.session_id.as_deref(), Some("session-abc"));
        assert_eq!(parsed.score, Some(0.98));
        assert_eq!(parsed.tags.as_ref().map(Vec::len), Some(2));
        assert_eq!(parsed.access_count, Some(4));
        assert_eq!(parsed.useful_count, Some(3));
        assert_eq!(parsed.source.as_deref(), Some("task-2026-02-16"));
        assert_eq!(parsed.source_confidence, Some(0.92));
        assert_eq!(parsed.verification_status, Some(VerificationStatus::Verified));
        assert_eq!(parsed.lifecycle_state, Some(LifecycleState::Active));
        assert_eq!(parsed.compressed_from.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn memory_entry_deserialize_legacy_payload_defaults_new_fields() {
        let raw = r#"{
            "id":"id-legacy",
            "key":"legacy_key",
            "content":"legacy content",
            "category":"core",
            "timestamp":"2026-02-01T00:00:00Z",
            "session_id":null,
            "score":0.7
        }"#;
        let parsed: MemoryEntry = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.id, "id-legacy");
        assert!(parsed.tags.is_none());
        assert!(parsed.access_count.is_none());
        assert!(parsed.useful_count.is_none());
        assert!(parsed.source.is_none());
        assert!(parsed.source_confidence.is_none());
        assert!(parsed.verification_status.is_none());
        assert!(parsed.lifecycle_state.is_none());
        assert!(parsed.compressed_from.is_none());
    }
}
