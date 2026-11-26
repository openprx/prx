pub(crate) const MESSAGE_EVENT_BATCH_LIMIT: usize = 32;

use async_trait::async_trait;
use std::collections::HashMap;

use crate::memory::backend::MemoryBackendKind;
use crate::memory::principal::MemoryWriteContext;

mod types;

pub use types::{
    AgentPartition, PartitionKey, AgentScopeUnsupported, AgentScopeRefused,
    AgentPartitionUnreadable, MemoryEntryOrigin, MemoryEntry, DecisionMemoryHit,
    DecisionDocumentHit, DecisionSessionHit, MemoryReadMode, ChatProfile, VerificationStatus,
    LifecycleState, MemoryCategory, validate_memory_write_target, ConversationSessionSummary,
    CALLER_KEYED_SESSION_CHANNEL, SessionClaim, ConversationSessionOwner, ConversationTurn,
    MemoryVisibility, MemoryPrincipal, MessageEventSource, MessageEventSubject, MessageEventInput,
    MessageEvent, TranscriptHit
};

#[cfg(test)]
mod message_event_contract_tests;

mod records;

pub use records::{
    SharedContextQuery, SessionPageIncomplete, SessionContextQuery, MemoryEventInput, MemoryEvent,
    MemoryStoreMetadata, MemoryDraftInput, MemoryDraft, DocumentIngestInput, DocumentRecord,
    DocumentChunkRecord, DocumentSearchResult, RetrievedContextItem, MemoryLinkInput, MemoryLink,
    RetrievalTraceInput, RetrievalTrace, CompactionSourceEventRange, CompactionRunInput,
    CompactionRun
};

/// Whether a backend's *default*, unscoped read must answer nothing
/// (C03 / V02-R5, follow-up F2).
///
/// The context-carrying read defaults on [`Memory`] fall back to the unscoped
/// `recall` / `get`, which cannot express an owner boundary: those backends
/// store no `owner_id` and have no visibility predicate to attach one to. An
/// owner-isolated caller would therefore read every row in the store — the
/// same defect the SQL adapters were fixed for, reachable on every backend
/// that does not override those methods (`markdown`, `none`, `lucid`, the wasm
/// plugin wrappers, and an unknown backend name, which falls back to
/// `markdown`).
///
/// Fail closed instead: no attribution, no rows. `SqliteMemory` and
/// `PostgresMemory` override every one of those methods with a real
/// owner-scoped predicate, so they never reach this and keep serving an
/// isolated caller its own rows. A caller that is not owner-isolated —
/// including one with no context at all — is untouched.
///
/// The log line carries the backend name and the operation only: no key, no
/// query, no owner id, no content.
fn unscoped_read_is_withheld(
    backend: MemoryBackendKind,
    context: Option<&MemoryWriteContext>,
    operation: &'static str,
) -> bool {
    if !context.is_some_and(|context| context.require_owner_match) {
        return false;
    }
    tracing::warn!(
        backend_kind = ?backend,
        operation,
        "memory backend cannot scope a read to an owner; the owner-isolated caller is served nothing"
    );
    true
}

/// An owner-isolated context is enforced whatever mode was requested, so an
/// `Observe` request cannot route around the fail-closed defaults above. This
/// mirrors the override both SQL adapters already apply in their own
/// `*_with_context_mode`.
const fn enforced_read_mode(context: Option<&MemoryWriteContext>, requested: MemoryReadMode) -> MemoryReadMode {
    match context {
        Some(context) if context.require_owner_match => MemoryReadMode::Enforce,
        _ => requested,
    }
}

/// Core memory trait — implement for any persistence backend
#[async_trait]
pub trait Memory: Send + Sync {
    /// Available only with the transactional rf journal backend.
    fn sync_store(&self) -> Option<std::sync::Arc<dyn crate::memory::sync::SyncStore>> {
        None
    }

    /// Unavailable without a transactional namespace for child sessions.
    fn child_session_store(&self) -> Option<&dyn crate::memory::child_sessions::ChildSessionStore> {
        None
    }

    /// Unavailable for backends without repository-owned durable authority.
    fn recovery_repository(&self) -> Option<&dyn crate::memory::recovery_authority::RecoveryRepository> {
        None
    }

    /// Unavailable for backends without a transactional agent incarnation registry.
    fn agent_registry(&self) -> Option<&dyn crate::memory::agent_registry::AgentRegistry> {
        None
    }

    /// Whether this backend can serve a Named agent partition.
    fn supports_agent_scope(&self) -> bool {
        false
    }

    /// Whether execution requires this exact in-process memory capability.
    fn requires_scoped_execution(&self) -> bool {
        false
    }

    /// Return a view that attributes every subsequent write to one input.
    fn with_erasure_origin(
        &self,
        _origin: crate::memory::erasure::ErasureOrigin,
    ) -> anyhow::Result<std::sync::Arc<dyn Memory>> {
        Err(crate::memory::erasure::Unsupported("transactional erasure").into())
    }

    fn erasure_store(&self) -> anyhow::Result<crate::memory::erasure::SharedErasureStore> {
        Err(crate::memory::erasure::Unsupported("transactional erasure").into())
    }

    /// Return a view that appends one content-free write-log row, in the
    /// write's own transaction, for every memory and document it writes.
    fn with_write_provenance(
        &self,
        _provenance: crate::memory::write_provenance::WriteProvenance,
    ) -> anyhow::Result<std::sync::Arc<dyn Memory>> {
        Err(crate::memory::erasure::Unsupported("write provenance").into())
    }

    /// Backend name
    fn name(&self) -> &str;

    /// The backend kind [`Self::name`] denotes; a name that is not one of
    /// the known backends (a plugin, a wrapper) is
    /// [`MemoryBackendKind::Unknown`]. Logs carry this kind, never the name.
    fn backend_kind(&self) -> MemoryBackendKind {
        crate::memory::backend::classify_memory_backend(self.name())
    }

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
        // V02-R9: this default stores no owner, so it cannot keep an
        // owner-isolated caller inside its own keys. Refuse the write rather
        // than let it overwrite whatever the key holds. `SqliteMemory` and
        // `PostgresMemory` override this with an owner-guarded upsert.
        if context.is_some_and(|context| context.require_owner_match) {
            return Err(super::owner_predicate::OwnerScopedStoreRefusal::UnscopedBackend {
                backend: self.name().to_string(),
            }
            .into());
        }
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

    /// Runtime statistics read; verifies a durable terminal receipt and source.
    /// One actual selected Router-history purpose, consumed without lexical recall fallback.
    async fn read_router_history(
        &self,
        evidence: super::RouterHistoryReadEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::RouterHistoryRead> {
        let _ = (evidence, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "read_router_history"))
    }

    /// Persist one actual terminal outcome and its retained local embedding.
    async fn commit_router_history(
        &self,
        evidence: &super::RouterHistoryWriteEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::RouterHistoryCommitReceipt> {
        let _ = (evidence, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "commit_router_history"))
    }

    async fn read_router_learning(
        &self,
        evidence: &RouterLearningEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<RouterLearningRead> {
        let _ = (evidence, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "read_router_learning"))
    }

    /// Atomically commit a bounded statistics snapshot and permanent terminal deduplication.
    async fn commit_router_learning(
        &self,
        evidence: &RouterLearningEvidence,
        expected_version: i64,
        next: &RouterLearningSnapshot,
        deadline: std::time::Instant,
    ) -> anyhow::Result<RouterLearningCommit> {
        let _ = (evidence, expected_version, next, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "commit_router_learning"))
    }

    /// Count at most `limit` keyword hits visible to a trusted source scope.
    /// Rehearsal must not compute embeddings, cache, log access, or return
    /// content. Backends without a strictly scoped read-only path fail closed.
    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        let _ = (query, limit, context, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "count_rehearsal_hits"))
    }

    /// Read selected Memory evidence with source-owned ACL scope. An optional
    /// session filter is applied in the backend before ranking/limit; callers
    /// must authorize its value from the source rather than model arguments.
    /// Returns stored key/category/session metadata, but no semantic score or
    /// ancillary metadata. Unsupported backends fail closed instead of using
    /// semantic recall with side effects.
    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
        let _ = (query, limit, context, session_id, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "retrieve_decision_memory",
        ))
    }

    /// Read selected document evidence locally without embedding generation,
    /// cache writes or unscoped fallback. The caller owns the principal and deadline.
    async fn retrieve_decision_documents(
        &self,
        query: &str,
        limit: u8,
        principal: &MemoryPrincipal,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionDocumentHit>> {
        let _ = (query, limit, principal, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "retrieve_decision_documents",
        ))
    }

    /// Read bounded session history strictly before the runtime-owned source
    /// event. No embeddings, unscoped fallback, or current-input echo is allowed.
    async fn retrieve_decision_session(
        &self,
        principal: &MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionSessionHit>> {
        let _ = (principal, before_event_id, limit, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "retrieve_decision_session",
        ))
    }

    /// Recall memories matching a query with optional runtime ACL/write context.
    ///
    /// Backends without ACL-aware recall support fall back to legacy recall —
    /// except for an owner-isolated caller, which this default cannot serve at
    /// all; see [`unscoped_read_is_withheld`].
    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if unscoped_read_is_withheld(self.backend_kind(), context, "recall") {
            return Ok(Vec::new());
        }
        self.recall(query, limit, session_id).await
    }

    /// Backend-neutral recall for user-facing readers with ACL
    /// enforcement/observation modes.
    async fn recall_with_context_mode(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        match enforced_read_mode(context, mode) {
            MemoryReadMode::Enforce => self.recall_with_context(query, limit, session_id, context).await,
            MemoryReadMode::Observe => self.recall(query, limit, session_id).await,
        }
    }

    /// Get a specific memory by key
    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>>;

    /// Get a specific memory by key under an optional runtime ACL context.
    async fn get_with_context(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        if unscoped_read_is_withheld(self.backend_kind(), context, "keyed read") {
            return Ok(None);
        }
        self.get(key).await
    }

    /// Backend-neutral keyed read for ACL enforcement/observation modes.
    async fn get_with_context_mode(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        match enforced_read_mode(context, mode) {
            MemoryReadMode::Enforce => self.get_with_context(key, context).await,
            MemoryReadMode::Observe => self.get(key).await,
        }
    }

    /// List all memory keys, optionally filtered by category and/or session
    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>>;

    /// Rows whose key starts with `prefix`, limited by the backend query.
    async fn list_key_prefix(&self, prefix: &str, limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = self.list(None, None).await?;
        entries.retain(|entry| entry.key.starts_with(prefix));
        entries.truncate(limit);
        Ok(entries)
    }

    /// Newest `core` memories of `principal`, newest first by write order.
    ///
    /// Only the principal's owner and agent partition are read; `sensitivity`
    /// other than `normal` is never returned. A principal with no
    /// `agent_partition` is read as the Main partition, never as all agents.
    /// A backend without this read returns none, so the index is simply absent.
    async fn newest_core_entries(&self, principal: &MemoryPrincipal, limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        let _ = (principal, limit);
        Ok(Vec::new())
    }

    /// Delete message-event rows whose idempotency key is `idempotency_key`.
    ///
    /// The default leaves the log untouched. Backends that store the causal
    /// log override this so a channel erase can remove one message and its
    /// terminal payload without listing the table.
    async fn forget_message_idempotency(&self, idempotency_key: &str) -> anyhow::Result<usize> {
        let _ = idempotency_key;
        Err(crate::memory::erasure::Unsupported("forget_message_idempotency").into())
    }

    /// Delete one session prefix row (`context.prefix.recorded`) by its row id.
    /// Used when a freshly recorded prefix no longer matches the memories it was
    /// rendered from. Backends without an ordered event log fail fast.
    async fn delete_session_prefix_row(&self, row_id: i64) -> anyhow::Result<usize> {
        let _ = row_id;
        Err(crate::memory::erasure::Unsupported("delete_session_prefix_row").into())
    }

    /// Key as stored in the backend. Encrypting scopes hash it.
    fn persisted_key(&self, key: &str) -> String {
        key.to_string()
    }

    /// Truncate the sqlite WAL so deleted pages are not left in `brain.db-wal`.
    ///
    /// Backends without a WAL ignore this.
    async fn checkpoint_wal(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Forget every row whose stored key starts with `prefix`.
    async fn forget_key_prefix(&self, prefix: &str) -> anyhow::Result<usize> {
        let entries = self.list_key_prefix(prefix, usize::MAX).await?;
        let mut removed = 0usize;
        for entry in entries {
            if self.forget(&entry.key).await? {
                removed = removed.saturating_add(1);
            }
        }
        Ok(removed)
    }

    /// The owner of every row in `category`, by row id, for maintenance that
    /// must never act across owners (deduplication, for one).
    ///
    /// `None` means this backend keeps no per-row owner, so every row belongs
    /// to the one workspace. A backend that stores owners (`SqliteMemory`,
    /// `PostgresMemory`) must override this: `None` from it would let such
    /// maintenance treat two owners' rows as one owner's.
    async fn list_owner_ids(
        &self,
        category: &MemoryCategory,
    ) -> anyhow::Result<Option<std::collections::HashMap<String, Option<String>>>> {
        let _ = category;
        Ok(None)
    }

    /// Remove a memory by key
    async fn forget(&self, key: &str) -> anyhow::Result<bool>;

    /// Mirror a write the **local operator** made into the backend's workspace
    /// Markdown files (`MEMORY.md` for `core`, `memory/<date>.md` otherwise),
    /// when the backend keeps such a mirror (D-026).
    ///
    /// `MEMORY.md` is inlined into the system prompt of every surface, so text
    /// placed there steers every user's turns. The caller — `memory_store` —
    /// invokes this only for a write that came from the terminal UI or the
    /// local-operator CLI tuple; every other origin (webhook, channel, gateway,
    /// Console, MCP, sub-agent) stays in the owner-scoped store. Backends with
    /// no separate mirror do nothing.
    async fn mirror_operator_write(&self, key: &str, content: &str, category: &MemoryCategory) -> anyhow::Result<()> {
        let _ = (key, content, category);
        Ok(())
    }

    /// Whether this backend's store *is* the shared prompt material: the
    /// `markdown` backend keeps memory in `MEMORY.md` and the daily logs
    /// themselves, so a store on it cannot be kept out of every surface's
    /// system prompt (D-026). `memory_store` refuses such a backend to every
    /// origin but the local operator.
    fn stores_into_shared_prompt_files(&self) -> bool {
        false
    }

    /// Remove a memory by key after applying a runtime write/read scope.
    ///
    /// Backends that do not support ACL metadata fall back to legacy key
    /// deletion for compatibility; SQLite and PostgreSQL override this with
    /// scoped deletion.
    ///
    /// The owner boundary applies to the delete as it does to the reads above.
    /// A backend that cannot prove who owns a row cannot prove the row belongs
    /// to an owner-isolated caller either, so it deletes nothing and reports
    /// that nothing was deleted. `memory_forget` is not on the default MCP
    /// surface, but an operator may expose it; without this, the caller that
    /// cannot *read* another owner's row could still delete it by key.
    /// A caller that is not owner-isolated — including one with no context at
    /// all — keeps the legacy unscoped delete unchanged.
    async fn forget_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> anyhow::Result<bool> {
        if unscoped_read_is_withheld(self.backend_kind(), context, "forget") {
            return Ok(false);
        }
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

    /// Upsert channel-provided metadata for the exact conversation without
    /// touching self-maintained purpose/notes/tags.
    async fn upsert_chat_profile_metadata(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        title: Option<&str>,
    ) -> anyhow::Result<()> {
        let _ = (channel, chat_id, chat_kind, title);
        Ok(())
    }

    /// Update the current conversation profile. The caller is responsible for
    /// deriving channel/chat_id from trusted runtime scope.
    async fn update_chat_profile(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        purpose: Option<&str>,
        notes: Option<&str>,
        tags: Option<&[String]>,
        updated_by: &str,
    ) -> anyhow::Result<ChatProfile> {
        let _ = (channel, chat_id, chat_kind, purpose, notes, tags, updated_by);
        Err(crate::memory::fabric::fail_fast(self.name(), "update_chat_profile"))
    }

    /// Load the exact conversation profile by `(channel, chat_id)`.
    async fn get_chat_profile(&self, channel: &str, chat_id: &str) -> anyhow::Result<Option<ChatProfile>> {
        let _ = (channel, chat_id);
        Ok(None)
    }

    /// Create an empty Console session without inventing a conversation turn.
    /// Keys are server-generated; an existing key must fail without mutation.
    async fn create_conversation_session(&self, session_key: &str, owner_id: &str) -> anyhow::Result<()> {
        let _ = (session_key, owner_id);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "create_conversation_session",
        ))
    }

    /// Get-or-create an empty caller-keyed session (channel
    /// [`CALLER_KEYED_SESSION_CHANNEL`]) in one transaction: a new row is owned
    /// by `owner_id`; an existing row is compared, never changed.
    async fn claim_conversation_session(&self, session_key: &str, owner_id: &str) -> anyhow::Result<SessionClaim> {
        let _ = (session_key, owner_id);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "claim_conversation_session",
        ))
    }

    /// The channel and owner of the session row `session_key`; `None` when
    /// no row holds it.
    async fn conversation_session_owner(&self, session_key: &str) -> anyhow::Result<Option<ConversationSessionOwner>> {
        let _ = session_key;
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "conversation_session_owner",
        ))
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
        agent_partition: Option<&AgentPartition>,
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
            agent_partition,
        );
        Ok(())
    }

    /// Project an exact durable Console message once, including the session
    /// counter update. Repeating the same event is a no-op; conflicts fail.
    /// Implementations must verify the stored event inside the transaction.
    async fn project_console_message(&self, event: &MessageEvent) -> anyhow::Result<()> {
        let _ = event;
        Err(crate::memory::fabric::fail_fast(self.name(), "project_console_message"))
    }

    /// Enumerate committed Console terminal markers for explicit recovery.
    /// Returned rows are candidates only; the caller must authenticate replay.
    async fn list_console_terminal_events(
        &self,
        workspace_id: &str,
        session_key: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (workspace_id, session_key, after_id, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_console_terminal_events",
        ))
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

    /// List the persisted conversation sessions of one owner, ordered by most
    /// recently updated. A backend that lists no sessions lists none here.
    async fn list_owner_conversation_sessions(
        &self,
        owner_id: &str,
        limit: usize,
        offset: usize,
        channel: Option<&str>,
    ) -> anyhow::Result<Vec<ConversationSessionSummary>> {
        let _ = (owner_id, limit, offset, channel);
        Ok(Vec::new())
    }

    /// The agent key of each stored conversation turn among `turn_ids`
    /// (MA-01h): `m` for a Main row (stamped Main or unstamped) and
    /// `n:<name>` for a named agent's row. A backend that keeps no agent
    /// scopes cannot label rows.
    async fn conversation_turn_agent_keys(&self, turn_ids: &[i64]) -> anyhow::Result<HashMap<i64, String>> {
        let _ = turn_ids;
        Err(AgentScopeUnsupported.into())
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

    /// Read the newest Console history window in event-time order, reversing
    /// the selected page for chronological presentation. Recovery insertion
    /// order must not move an old event into the newest page.
    async fn list_console_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        principal.reject_named_scope()?;
        let _ = (session_key, limit, offset);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_console_conversation_turns",
        ))
    }

    /// The user and assistant message rows of one session, oldest first, at
    /// most the newest `limit`: the log the transcript builds model history
    /// from. Filtered by owner, session key and agent partition only; the
    /// row-visibility tiers of the session-context reads do not apply.
    ///
    /// These are the rows the session routes already show through the
    /// `conversation_turns` projection, so the read exposes nothing new; it is
    /// not a cross-session or cross-owner read. Only `context::transcript` calls
    /// it. A backend without it reports `SessionTranscriptUnsupported`.
    #[doc(hidden)]
    async fn session_transcript_rows(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (principal, session_key, limit);
        Err(crate::memory::session_transcript::SessionTranscriptUnsupported.into())
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

    /// Append at most 32 independent events, returning one result per input in
    /// the original order. Unsupported backends retain explicit per-event errors.
    async fn append_message_events(&self, inputs: Vec<MessageEventInput>) -> Vec<anyhow::Result<MessageEvent>> {
        if inputs.len() > MESSAGE_EVENT_BATCH_LIMIT {
            return inputs
                .iter()
                .map(|_| Err(anyhow::anyhow!("message event batch exceeds capacity")))
                .collect();
        }
        let mut results = Vec::with_capacity(inputs.len());
        for input in inputs {
            results.push(self.append_message_event(input).await);
        }
        results
    }

    /// Store a chat session snapshot (the session-snapshot persistence path
    /// only; nothing else calls it). A snapshot is the owner's transcript, not
    /// recall memory, so no backend runs the memory content filter on this
    /// path. The default is an ordinary `Conversation` store.
    async fn store_session_snapshot(&self, key: &str, content: &str, session_id: Option<&str>) -> anyhow::Result<()> {
        self.store(key, content, MemoryCategory::Conversation, session_id).await
    }

    /// [`Memory::store_session_snapshot`] with fabric metadata. The default
    /// is the metadata-bearing `Conversation` store; a backend whose
    /// `store_with_metadata` reaches the content filter must override this.
    async fn store_session_snapshot_with_metadata(
        &self,
        key: &str,
        content: &str,
        session_id: Option<&str>,
        metadata: MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        self.store_with_metadata(key, content, MemoryCategory::Conversation, session_id, metadata)
            .await
    }

    /// Copy a session's transcript rows into a new branch in one transaction,
    /// rewriting every copied handoff note to reference the copies. Backends
    /// without a transactional event log return [`BranchCopyError::Unsupported`].
    ///
    /// [`BranchCopyError::Unsupported`]: crate::memory::branch_copy::BranchCopyError::Unsupported
    async fn copy_events_for_branch(
        &self,
        plan: crate::memory::branch_copy::BranchCopyPlan,
    ) -> anyhow::Result<crate::memory::branch_copy::BranchCopyResult> {
        let _ = plan;
        Err(crate::memory::branch_copy::BranchCopyError::Unsupported.into())
    }

    /// Create a backend lifetime owner. Unsupported backends fail explicitly.
    async fn acquire_ingress_lifetime(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<std::sync::Arc<dyn super::ingress_recovery::IngressLifetime>> {
        let _ = (workspace_id, token, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "acquire_ingress_lifetime",
        ))
    }

    /// Input, capsule and indexed projection must commit in one transaction.
    async fn append_ingress_input(
        &self,
        input: MessageEventInput,
        capsule: super::ingress_recovery::IngressCapsule,
        lifetime: std::sync::Arc<dyn super::ingress_recovery::IngressLifetime>,
        deadline: std::time::Instant,
        attestation: Option<crate::memory::ingress_recovery::IngressAttestation>,
    ) -> anyhow::Result<super::ingress_recovery::IngressAppendOutcome> {
        let _ = (input, capsule, lifetime, deadline);
        // An attestation is refused with its typed error, never silently dropped.
        super::ingress_recovery::refuse_attestation(attestation.as_ref())?;
        Err(crate::memory::fabric::fail_fast(self.name(), "append_ingress_input"))
    }

    /// [`Self::append_ingress_input`] that also writes the rf attempt row in
    /// the same transaction (AC-1b2a); a refused attempt rolls everything back
    /// with [`super::rf_messages::RfAdmissionRefused`].
    async fn append_ingress_input_admitting(
        &self,
        input: MessageEventInput,
        capsule: super::ingress_recovery::IngressCapsule,
        lifetime: std::sync::Arc<dyn super::ingress_recovery::IngressLifetime>,
        deadline: std::time::Instant,
        attestation: Option<crate::memory::ingress_recovery::IngressAttestation>,
        admission: super::rf_messages::RfAdmission,
    ) -> anyhow::Result<super::ingress_recovery::IngressAppendOutcome> {
        let _ = (input, capsule, lifetime, deadline, admission);
        super::ingress_recovery::refuse_attestation(attestation.as_ref())?;
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "append_ingress_input_admitting",
        ))
    }

    /// Append a new input event and its rf attempt row in one transaction.
    async fn append_message_event_admitting(
        &self,
        input: MessageEventInput,
        admission: super::rf_messages::RfAdmission,
    ) -> anyhow::Result<MessageEvent> {
        let _ = (input, admission);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "append_message_event_admitting",
        ))
    }

    /// One rf attempt with its run's durable terminal, read in one statement.
    async fn rf_message_attempt(
        &self,
        workspace_id: &str,
        lookup: super::rf_messages::RfAttemptLookup,
    ) -> anyhow::Result<Option<super::rf_messages::RfAttempt>> {
        let _ = (workspace_id, lookup);
        Err(crate::memory::fabric::fail_fast(self.name(), "rf_message_attempt"))
    }

    /// Give an rf run its terminal `interrupted` in one transaction, only
    /// while it has no terminal row; `false` when it has one.
    /// Startup uses `restartable` to preserve the key's restart rule; a
    /// cancel of an unfinished run or a new admission settles it permanently.
    async fn interrupt_rf_run(
        &self,
        workspace_id: &str,
        owner_id: &str,
        run_id: &str,
        restartable: bool,
    ) -> anyhow::Result<bool> {
        let _ = (workspace_id, owner_id, run_id, restartable);
        Err(crate::memory::fabric::fail_fast(self.name(), "interrupt_rf_run"))
    }

    /// Delete one rf message (AC-1b2b): mark every matching attempt deleted
    /// and tombstone its rows in one transaction; an empty list when no
    /// attempt matches.
    async fn rf_delete_message(
        &self,
        request: super::rf_messages::RfMessageDelete,
    ) -> anyhow::Result<Vec<super::rf_messages::RfDeletedAgent>> {
        let _ = request;
        Err(crate::memory::fabric::fail_fast(self.name(), "rf_delete_message"))
    }

    /// Delete the assistant side of one ended rf run in one transaction;
    /// with `forget_writes`, also forget what its run family wrote.
    async fn rf_delete_reply(
        &self,
        workspace_id: &str,
        owner_id: &str,
        session_key: &str,
        run_id: &str,
        forget_writes: bool,
    ) -> anyhow::Result<super::rf_messages::RfReplyDelete> {
        let _ = (workspace_id, owner_id, session_key, run_id, forget_writes);
        Err(crate::memory::fabric::fail_fast(self.name(), "rf_delete_reply"))
    }

    /// Tombstone the rows of the session's deleted runs whose jobs have
    /// ended (every pending run not in `live_runs`).
    async fn rf_settle_deleted(
        &self,
        workspace_id: &str,
        owner_id: &str,
        session_key: &str,
        live_runs: Vec<String>,
    ) -> anyhow::Result<()> {
        let _ = (workspace_id, owner_id, session_key, live_runs);
        Err(crate::memory::fabric::fail_fast(self.name(), "rf_settle_deleted"))
    }

    /// Empty the WAL of deleted rf content when a delete left it pending
    /// (AC-1b2b); `true` when nothing is pending afterwards. Backends
    /// without a WAL hold nothing pending.
    async fn scrub_deleted_wal(&self) -> anyhow::Result<bool> {
        Ok(true)
    }

    /// [`Self::list_console_conversation_turns`] with deleted tombstones.
    async fn list_rf_history_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<super::rf_messages::RfHistoryTurn>> {
        let _ = (principal, session_key, limit, offset);
        Err(crate::memory::fabric::fail_fast(self.name(), "list_rf_history_turns"))
    }

    async fn list_ingress_recovery_candidates(
        &self,
        scope: &super::ingress_recovery::IngressRecoveryScope,
        current: std::sync::Arc<dyn super::ingress_recovery::IngressLifetime>,
        phase: super::ingress_recovery::IngressRecoveryScan,
        after_id: i64,
        limit: usize,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<super::ingress_recovery::IngressRecoveryCandidate>> {
        let _ = (scope, current, phase, after_id, limit, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_ingress_recovery_candidates",
        ))
    }

    async fn read_ingress_event(
        &self,
        scope: &super::ingress_recovery::IngressRecoveryScope,
        lookup: &super::ingress_recovery::IngressEventLookup,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let _ = (scope, lookup, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "read_ingress_event"))
    }

    /// None means another lifetime still owns the lock; no death inference.
    /// [`IngressLockMissing`](super::ingress_recovery::IngressLockMissing) means
    /// the lock is missing, which proves nothing about the lifetime either.
    async fn claim_ingress_recovery(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<super::ingress_recovery::IngressRecoveryClaim>> {
        let _ = (workspace_id, token, deadline);
        Err(crate::memory::fabric::fail_fast(self.name(), "claim_ingress_recovery"))
    }

    /// Evidence event, outbox and projection transition must commit atomically.
    async fn commit_ingress_recovery(
        &self,
        commit: super::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<MessageEvent> {
        let _ = commit;
        Err(crate::memory::fabric::fail_fast(self.name(), "commit_ingress_recovery"))
    }

    /// Settle another lifetime's row whose lifetime lock is missing. `commit.claim`
    /// is the caller's own live lifetime; the backend proves the lock absent in
    /// the same transaction. Backends whose leases cannot go missing refuse.
    async fn commit_orphaned_ingress_recovery(
        &self,
        commit: super::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<MessageEvent> {
        let _ = commit;
        Err(super::ingress_recovery::IngressOrphanUnsupported.into())
    }

    /// Atomic terminal commit: claim, optional assistant/usage, final, attestation, outbox.
    async fn commit_recovery_terminal(
        &self,
        commit: super::recovery_authority::RecoveryTerminalCommit,
        deadline: std::time::Instant,
    ) -> anyhow::Result<super::recovery_authority::RecoveryTerminalOutcome> {
        let _ = (commit, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "commit_recovery_terminal",
        ))
    }

    /// The owner's pre-decision terminal record and the settlement of its own
    /// ingress row (the commit's evidence), in one transaction (I3).
    async fn commit_owned_admission_failure(
        &self,
        event: MessageEventInput,
        ingress: super::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<MessageEvent> {
        let _ = (event, ingress);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "commit_owned_admission_failure",
        ))
    }

    /// Bounded evidence repair; this does not grant provider or tool authority.
    async fn append_message_event_until(
        &self,
        input: MessageEventInput,
        deadline: std::time::Instant,
    ) -> anyhow::Result<MessageEvent> {
        let _ = (input, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "append_message_event_until",
        ))
    }
    async fn project_console_message_until(
        &self,
        event: &MessageEvent,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        let _ = (event, deadline);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "project_console_message_until",
        ))
    }

    /// Find one MessageEvent by its workspace-scoped idempotency key.
    ///
    /// The default returns no match for backends without event-ledger lookup
    /// support. Persistent event backends override this so execution recovery can
    /// distinguish a committed result from an unowned retry.
    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let _ = (workspace_id, idempotency_key);
        Ok(None)
    }

    /// Bounded runtime recovery discovery, ordered by durable event ID. This
    /// returns launch candidates only; callers must authenticate their signed
    /// provenance before using any owner, run, process or result fields.
    async fn list_worker_launch_events(
        &self,
        workspace_id: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (workspace_id, after_id, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_worker_launch_events",
        ))
    }

    /// Owner-scoped rows of one event type, ordered by durable id.
    ///
    /// The default fails closed. A backend that cannot filter by owner must
    /// not answer with an empty page, because an empty page means this owner
    /// has no rows.
    async fn list_owned_message_events_by_type(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_type: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (workspace_id, owner_id, event_type, after_id, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "list_owned_message_events_by_type",
        ))
    }

    /// Indexed recovery lookup with owner isolation. Unsupported backends must
    /// not turn an unavailable ledger into evidence that no segment existed.
    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let _ = (workspace_id, owner_id, idempotency_key);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "find_owned_message_event_by_idempotency_key",
        ))
    }

    /// Read one durable event for an exact workspace and owner. Runtime replay
    /// uses this for its original source receipt; it grants no caller authority.
    async fn find_owned_message_event(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let _ = (workspace_id, owner_id, event_id);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "find_owned_message_event",
        ))
    }

    /// List message events visible to `principal` after a cursor.
    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        principal.reject_named_scope()?;
        let _ = (after_id, limit);
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

    /// List message events in an RFC 3339 half-open time range.
    ///
    /// The default preserves compatibility for external backends by paging the
    /// existing cursor API. Indexed backends should override this to avoid a
    /// lifetime scan for daily analytics. Supplying
    /// `descendant_workspace_root` requests a privileged system-only query over
    /// every visibility in the principal workspace and that descendant tree;
    /// backends that cannot enforce that boundary must reject it.
    async fn list_message_events_time_range(
        &self,
        principal: &MemoryPrincipal,
        descendant_workspace_root: Option<&str>,
        start: &str,
        end: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        anyhow::ensure!(
            descendant_workspace_root.is_none(),
            "this memory backend does not support system-scoped descendant workspace event queries"
        );
        let start = chrono::DateTime::parse_from_rfc3339(start)?.with_timezone(&chrono::Utc);
        let end = chrono::DateTime::parse_from_rfc3339(end)?.with_timezone(&chrono::Utc);
        let mut cursor = 0_i64;
        let mut output = Vec::new();
        loop {
            let page = self.list_message_events_since(principal, cursor, 500).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |event| event.id);
            let page_len = page.len();
            output.extend(page.into_iter().filter(|event| {
                chrono::DateTime::parse_from_rfc3339(&event.created_at).is_ok_and(|time| time >= start && time < end)
            }));
            if output.len() >= limit || page_len < 500 {
                break;
            }
        }
        output.truncate(limit);
        Ok(output)
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

    /// Rows of `query` (hard-filtered by its session key, same visibility rules as
    /// `load_recent_session_context`) with `id < before_id`, newest first, at most
    /// `query.limit`. Backends without an ordered event log return the fail-fast error.
    /// A backend that would have to drop stored rows it cannot read returns
    /// [`SessionPageIncomplete`] instead, so a short page always means the store
    /// holds no older row.
    async fn load_session_context_before(
        &self,
        query: SessionContextQuery,
        before_id: i64,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (query, before_id);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "load_session_context_before",
        ))
    }

    /// The newest `limit` rows of `event_type` in the session of `session_key`,
    /// newest first by row id, read with the visibility and agent partition
    /// filters of `principal` as [`Memory::load_session_context_before`] reads
    /// them. Backends without an ordered event log return the fail-fast error.
    async fn newest_session_events_of_type(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        event_type: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let _ = (principal, session_key, event_type, limit);
        Err(crate::memory::fabric::fail_fast(
            self.name(),
            "newest_session_events_of_type",
        ))
    }

    /// Full-text search over the transcript rows (`message_events`) of the
    /// principal's owner and workspace, in the agent partition of the principal
    /// (no partition means Main). The best match comes first (bm25) when the
    /// query has a word of 3 or more characters; the newest first when every
    /// word is shorter. Only user and assistant messages are searched. `query`
    /// is free text: every word must occur, and each word is matched as a
    /// literal. Backends without a transcript index return no hits.
    async fn search_transcripts(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        session_key: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<TranscriptHit>> {
        let _ = (principal, query, session_key, limit);
        Ok(Vec::new())
    }

    /// Build the transcript index now, so the first search does not. Backends
    /// without a transcript index do nothing.
    async fn warm_transcript_index(&self) -> anyhow::Result<()> {
        Ok(())
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

    /// Retrieve one document chunk by id only when it lies in the principal's
    /// agent partition (MA-01h). A backend that keeps no agent scopes holds
    /// only Main rows: it refuses a Named partition and reads as
    /// [`Memory::get_document_chunk`] otherwise.
    async fn get_document_chunk_scoped(
        &self,
        chunk_id: &str,
        principal: &MemoryPrincipal,
    ) -> anyhow::Result<Option<DocumentChunkRecord>> {
        principal.reject_named_scope()?;
        self.get_document_chunk(chunk_id).await
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
pub(crate) mod conformance;

#[cfg(test)]
mod tests;

mod router_learning;

pub use router_learning::{RouterLearningCommit, RouterLearningEvidence, RouterLearningMetrics, RouterLearningRead, RouterLearningSnapshot};

#[cfg(test)]
mod router_served_auxiliary_tests;
