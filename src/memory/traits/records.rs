use serde::{Deserialize, Serialize};

use super::{AgentPartition, MemoryCategory, MemoryPrincipal, MemoryVisibility};

/// Query for recent shared events visible to an agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedContextQuery {
    pub principal: MemoryPrincipal,
    pub since_event_id: Option<i64>,
    pub limit: usize,
    pub include_roles: Vec<String>,
}

/// [`Memory::load_session_context_before`] could not return a page that is
/// provably complete: the store holds rows in it that this backend cannot read
/// (counted, never shown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("session context page is not provably complete: {unreadable_rows} stored rows could not be read")]
pub struct SessionPageIncomplete {
    pub unreadable_rows: usize,
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
    /// Owning principal of this event, mirroring `message_events.owner_id`.
    ///
    /// `None` means unattributed: the row is preserved and stays readable by
    /// every non-owner-isolated principal, but an owner-isolated principal
    /// (see [`MemoryPrincipal::require_owner_match`]) never admits it.
    #[serde(default)]
    pub owner_id: Option<String>,
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
    /// Agent partition the writing turn runs under (MA-01b3). `None` for unpartitioned writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_partition: Option<AgentPartition>,
}

/// Persisted memory fabric outbox event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvent {
    pub id: i64,
    pub event_id: String,
    pub workspace_id: String,
    /// Owning principal of this event. `None` for rows written before the
    /// column existed, and for writers that have no owner in scope.
    #[serde(default)]
    pub owner_id: Option<String>,
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
    /// Agent partition the writing turn runs under (MA-01b3). `None` for unpartitioned writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_partition: Option<AgentPartition>,
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
    /// V02-R6: scoping keys copied onto the `document.ingested` memory event so
    /// it is readable through the same predicate arms as every other memory
    /// event. Without them the event row carries `NULL` in the columns the
    /// `session`/`agent`/`private` arms compare against, which no principal can
    /// ever match. They scope the event row only; the document's own visibility
    /// and owner columns are unchanged.
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub persona_id: Option<String>,
    /// Agent partition the writing turn runs under (MA-01b3). `None` for unpartitioned writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_partition: Option<AgentPartition>,
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
    /// `source_kind` of the owning document (e.g. `"tool_output"`,
    /// `"untrusted_external"`). FIX-P2-02: carried up from the `documents` row so
    /// the recall→context path can hard-filter untrusted external copies before
    /// they are injected as trusted agent context. `None` when the backend cannot
    /// resolve it (e.g. backends without a documents table).
    pub source_kind: Option<String>,
    /// Source event of the owning document (`documents.source_message_event_id`).
    pub source_message_event_id: Option<String>,
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
    /// Agent partition the writing turn runs under (MA-01b3). `None` for unpartitioned writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_partition: Option<AgentPartition>,
    /// Session of the writing turn, required to resolve a partitioned stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
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
    /// Agent partition the writing turn runs under (MA-01b3). `None` for unpartitioned writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_partition: Option<AgentPartition>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionSourceEventRange {
    pub first_event_id: String,
    pub last_event_id: String,
    pub first_row_id: i64,
    pub last_row_id: i64,
    pub source_event_count: usize,
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
    #[serde(default)]
    pub source_event_range_json: Option<String>,
    pub source_document_refs_json: Option<String>,
    pub fidelity_status: String,
    pub payload_json: Option<String>,
    /// Agent partition the writing turn runs under (MA-01b3). `None` for unpartitioned writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_partition: Option<AgentPartition>,
}

impl CompactionRunInput {
    pub fn validate_source_event_provenance(&self) -> anyhow::Result<()> {
        let (Some(ids_json), Some(range_json)) = (
            self.source_event_ids_json.as_deref(),
            self.source_event_range_json.as_deref(),
        ) else {
            anyhow::ensure!(
                self.source_event_ids_json.is_none() && self.source_event_range_json.is_none(),
                "compaction source event IDs and covered range must be supplied together"
            );
            return Ok(());
        };

        let ids: Vec<String> = serde_json::from_str(ids_json)
            .map_err(|error| anyhow::anyhow!("compaction source_event_ids_json must be a string array: {error}"))?;
        anyhow::ensure!(!ids.is_empty(), "compaction source event IDs must not be empty");
        anyhow::ensure!(
            ids.iter().all(|id| !id.trim().is_empty()),
            "compaction source event IDs must not contain blank values"
        );
        let unique = ids.iter().collect::<std::collections::HashSet<_>>();
        anyhow::ensure!(
            unique.len() == ids.len(),
            "compaction source event IDs must not contain duplicates"
        );

        let range: CompactionSourceEventRange = serde_json::from_str(range_json)
            .map_err(|error| anyhow::anyhow!("invalid compaction source event range: {error}"))?;
        anyhow::ensure!(
            range.first_row_id > 0,
            "compaction source event range must start after row zero"
        );
        anyhow::ensure!(
            range.last_row_id >= range.first_row_id,
            "compaction source event range rows are reversed"
        );
        anyhow::ensure!(
            ids.first() == Some(&range.first_event_id) && ids.last() == Some(&range.last_event_id),
            "compaction covered range endpoints must match the source event ID list"
        );
        anyhow::ensure!(
            range.source_event_count == ids.len(),
            "compaction covered range count must match the source event ID list"
        );
        Ok(())
    }
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
    #[serde(default)]
    pub source_event_range_json: Option<String>,
    pub source_document_refs_json: Option<String>,
    pub fidelity_status: String,
    pub payload_json: Option<String>,
    pub created_at: String,
}
