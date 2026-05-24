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
}

/// Input used to append an event into the shared message fabric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEventInput {
    pub event_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub workspace_id: String,
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
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    pub source_event_id: Option<String>,
    pub source: Option<String>,
}

/// A private worker memory draft waiting for parent merge/reject.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDraftInput {
    pub draft_id: Option<String>,
    pub workspace_id: String,
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
    ) -> anyhow::Result<()> {
        let _ = (session_key, channel, sender, role, content, timestamp, message_id);
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
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        let _ = (session_key, limit, offset);
        Ok(Vec::new())
    }

    /// Load recent turns per session for runtime history hydration.
    async fn load_recent_conversation_histories(
        &self,
        max_turns_per_session: usize,
        max_sessions: usize,
    ) -> anyhow::Result<HashMap<String, Vec<ConversationTurn>>> {
        let _ = (max_turns_per_session, max_sessions);
        Ok(HashMap::new())
    }

    /// Append a normalized message event into the shared memory fabric.
    ///
    /// Backends without event-log support can no-op and return a synthetic event
    /// so callers can be wired before every backend is upgraded.
    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        let now = chrono::Utc::now().to_rfc3339();
        Ok(MessageEvent {
            id: 0,
            event_id: input.event_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            idempotency_key: input.idempotency_key,
            workspace_id: input.workspace_id,
            source: input.source,
            channel: input.channel,
            session_key: input.session_key,
            parent_session_key: input.parent_session_key,
            run_id: input.run_id,
            parent_run_id: input.parent_run_id,
            agent_id: input.agent_id,
            persona_id: input.persona_id,
            sender: input.sender,
            recipient: input.recipient,
            role: input.role,
            content: input.content,
            content_hash: None,
            raw_payload_json: input.raw_payload_json,
            visibility: input.visibility,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// List message events visible to `principal` after a cursor.
    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (principal, after_id, limit);
        Ok(Vec::new())
    }

    /// Load recent shared context events visible to an agent turn.
    async fn load_recent_shared_context(&self, query: SharedContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = query;
        Ok(Vec::new())
    }

    /// Load recent current-session context events visible to an agent turn.
    async fn load_recent_session_context(&self, query: SessionContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = query;
        Ok(Vec::new())
    }

    /// Append a memory outbox event.
    async fn append_memory_event(&self, input: MemoryEventInput) -> anyhow::Result<MemoryEvent> {
        let now = chrono::Utc::now().to_rfc3339();
        Ok(MemoryEvent {
            id: 0,
            event_id: input.event_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            workspace_id: input.workspace_id,
            event_type: input.event_type,
            subject_table: input.subject_table,
            subject_id: input.subject_id,
            session_key: input.session_key,
            agent_id: input.agent_id,
            persona_id: input.persona_id,
            visibility: input.visibility,
            payload_json: input.payload_json,
            created_at: now,
        })
    }

    /// List memory outbox events visible to `principal` after a cursor.
    async fn list_memory_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        let _ = (principal, after_id, limit);
        Ok(Vec::new())
    }

    /// Create a private worker memory draft.
    async fn create_memory_draft(&self, input: MemoryDraftInput) -> anyhow::Result<MemoryDraft> {
        let now = chrono::Utc::now().to_rfc3339();
        Ok(MemoryDraft {
            id: 0,
            draft_id: input.draft_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            workspace_id: input.workspace_id,
            worker_run_id: input.worker_run_id,
            parent_run_id: input.parent_run_id,
            session_key: input.session_key,
            agent_id: input.agent_id,
            persona_id: input.persona_id,
            key: input.key,
            content: input.content,
            category: input.category,
            source_event_id: input.source_event_id,
            visibility: input.visibility,
            status: "pending".to_string(),
            payload_json: input.payload_json,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// List memory drafts produced by a worker run.
    async fn list_memory_drafts_for_run(&self, worker_run_id: &str) -> anyhow::Result<Vec<MemoryDraft>> {
        let _ = worker_run_id;
        Ok(Vec::new())
    }

    /// Mark a draft as merged into semantic memory.
    async fn merge_memory_draft(&self, draft_id: &str) -> anyhow::Result<Option<MemoryDraft>> {
        let _ = draft_id;
        Ok(None)
    }

    /// Reject a draft without merging it into semantic memory.
    async fn reject_memory_draft(&self, draft_id: &str, reason: Option<&str>) -> anyhow::Result<Option<MemoryDraft>> {
        let _ = (draft_id, reason);
        Ok(None)
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
