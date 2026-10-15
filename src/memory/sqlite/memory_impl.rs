//! The `Memory` trait implementation; each method delegates to its domain module.

use crate::memory::principal::MemoryWriteContext;
use crate::memory::traits::{
    ChatProfile, CompactionRun, CompactionRunInput, ConversationSessionSummary, ConversationTurn, DecisionMemoryHit,
    DocumentChunkRecord, DocumentIngestInput, DocumentRecord, DocumentSearchResult, Memory, MemoryCategory,
    MemoryDraft, MemoryDraftInput, MemoryEntry, MemoryEvent, MemoryEventInput, MemoryLink, MemoryLinkInput,
    MemoryPrincipal, MemoryReadMode, MemoryStoreMetadata, MessageEvent, MessageEventInput, RetrievalTrace,
    RetrievalTraceInput, SessionContextQuery, SharedContextQuery,
};
use anyhow::Context;
use async_trait::async_trait;
use rusqlite::params;
use std::sync::Arc;

use super::SqliteMemory;
use super::ingress_recovery;

#[async_trait]
impl Memory for SqliteMemory {
    fn name(&self) -> &str {
        "sqlite"
    }

    async fn upsert_chat_profile_metadata(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        title: Option<&str>,
    ) -> anyhow::Result<()> {
        self.upsert_chat_profile_metadata_impl(channel, chat_id, chat_kind, title)
            .await
    }

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
        self.update_chat_profile_impl(channel, chat_id, chat_kind, purpose, notes, tags, updated_by)
            .await
    }

    async fn get_chat_profile(&self, channel: &str, chat_id: &str) -> anyhow::Result<Option<ChatProfile>> {
        self.get_chat_profile_impl(channel, chat_id).await
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store_internal(key, content, category, session_id, None, None)
            .await
    }

    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<()> {
        self.store_internal(key, content, category, session_id, context, None)
            .await
    }

    async fn mirror_operator_write(&self, key: &str, content: &str, category: &MemoryCategory) -> anyhow::Result<()> {
        self.mirror_operator_write_impl(key, content, category).await
    }

    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        metadata: MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        self.store_internal(key, content, category, session_id, None, Some(metadata))
            .await
    }

    async fn store_with_context_and_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        metadata: MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        self.store_internal(key, content, category, session_id, context, Some(metadata))
            .await
    }

    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        self.recall_impl(query, limit, session_id).await
    }

    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.recall_with_context_impl(query, limit, session_id, context).await
    }

    async fn read_router_history(
        &self,
        evidence: crate::memory::router_history::RouterHistoryReadEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::router_history::RouterHistoryRead> {
        self.router_history_read(evidence, deadline).await
    }

    async fn commit_router_history(
        &self,
        evidence: &crate::memory::router_history::RouterHistoryWriteEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::router_history::RouterHistoryCommitReceipt> {
        self.router_history_commit(evidence, deadline).await
    }

    async fn read_router_learning(
        &self,
        evidence: &crate::memory::traits::RouterLearningEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::traits::RouterLearningRead> {
        self.router_learning_transaction(evidence, None, deadline)
            .await
            .map(|(read, _)| read)
    }

    async fn commit_router_learning(
        &self,
        evidence: &crate::memory::traits::RouterLearningEvidence,
        expected_version: i64,
        next: &crate::memory::traits::RouterLearningSnapshot,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::traits::RouterLearningCommit> {
        let (read, conflict) = self
            .router_learning_transaction(evidence, Some((expected_version, next.clone())), deadline)
            .await?;
        if conflict {
            Ok(crate::memory::traits::RouterLearningCommit::Conflict)
        } else {
            Ok(crate::memory::traits::RouterLearningCommit::Committed(
                read.snapshot.context("Router committed snapshot missing")?,
            ))
        }
    }

    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        self.count_rehearsal_hits_impl(query, limit, context, deadline).await
    }

    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
        self.retrieve_decision_memory_impl(query, limit, context, session_id, deadline)
            .await
    }

    async fn recall_with_context_mode(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.recall_with_context_mode_impl(query, limit, session_id, context, mode)
            .await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        self.get_impl(key).await
    }

    async fn get_with_context(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        self.get_with_context_impl(key, context).await
    }

    async fn get_with_context_mode(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        self.get_with_context_mode_impl(key, context, mode).await
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.list_impl(category, session_id).await
    }

    async fn list_owner_ids(
        &self,
        category: &MemoryCategory,
    ) -> anyhow::Result<Option<std::collections::HashMap<String, Option<String>>>> {
        self.list_owner_ids_impl(category).await
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let pool = self.pool.clone();
        let key = key.to_string();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = pool.write();
            let affected = conn.execute("DELETE FROM memories WHERE key = ?1", params![key])?;
            Ok(affected > 0)
        })
        .await?
    }

    async fn forget_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> anyhow::Result<bool> {
        self.forget_with_context_impl(key, context).await
    }

    async fn move_to_trash(&self, key: &str, reason: &str, grace_days: u32) -> anyhow::Result<Option<String>> {
        self.move_to_trash_impl(key, reason, grace_days).await
    }

    async fn create_evolution_proposal(
        &self,
        draft: crate::self_system::evolution::EvolutionProposalDraft,
    ) -> anyhow::Result<String> {
        self.create_evolution_proposal_impl(draft).await
    }

    async fn list_evolution_proposals(
        &self,
        principal: &MemoryPrincipal,
        filter: crate::self_system::evolution::ProposalFilter,
    ) -> anyhow::Result<Vec<crate::self_system::evolution::EvolutionProposalDraft>> {
        self.list_evolution_proposals_impl(principal, filter).await
    }

    async fn get_evolution_proposal(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<crate::self_system::evolution::EvolutionProposalDraft>> {
        self.get_evolution_proposal_impl(principal, draft_id).await
    }

    async fn update_evolution_proposal_status(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        update: crate::self_system::evolution::ProposalStatusUpdate,
    ) -> anyhow::Result<()> {
        self.update_evolution_proposal_status_impl(principal, draft_id, update)
            .await
    }

    async fn append_evolution_proposal_event(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        event_type: &str,
        actor: &str,
        payload_json: Option<&str>,
    ) -> anyhow::Result<()> {
        self.append_evolution_proposal_event_impl(principal, draft_id, event_type, actor, payload_json)
            .await
    }

    async fn increment_useful_count(&self, id: &str) -> anyhow::Result<()> {
        self.increment_useful_count_impl(id).await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<usize> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            Ok(count as usize)
        })
        .await?
    }

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
        self.append_conversation_turn_impl(
            session_key,
            channel,
            sender,
            role,
            content,
            timestamp,
            message_id,
            owner_id,
        )
        .await
    }

    async fn project_console_message(&self, event: &MessageEvent) -> anyhow::Result<()> {
        self.project_console_message_with_deadline(event, None).await
    }

    async fn list_console_terminal_events(
        &self,
        workspace_id: &str,
        session_key: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.list_console_terminal_events_impl(workspace_id, session_key, after_id, limit)
            .await
    }

    async fn list_conversation_sessions(
        &self,
        limit: usize,
        offset: usize,
        channel: Option<&str>,
    ) -> anyhow::Result<Vec<ConversationSessionSummary>> {
        self.list_conversation_sessions_impl(limit, offset, channel).await
    }

    async fn get_conversation_session(&self, session_key: &str) -> anyhow::Result<Option<ConversationSessionSummary>> {
        self.get_conversation_session_impl(session_key).await
    }

    async fn list_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        self.list_conversation_turns_ordered(principal, session_key, limit, offset, false)
            .await
    }

    async fn list_console_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        self.list_conversation_turns_ordered(principal, session_key, limit, offset, true)
            .await
    }

    async fn load_recent_conversation_histories(
        &self,
        principal: &MemoryPrincipal,
        max_turns_per_session: usize,
        max_sessions: usize,
    ) -> anyhow::Result<std::collections::HashMap<String, Vec<ConversationTurn>>> {
        self.load_recent_conversation_histories_impl(principal, max_turns_per_session, max_sessions)
            .await
    }

    async fn acquire_ingress_lifetime(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Arc<dyn crate::memory::ingress_recovery::IngressLifetime>> {
        self.acquire_ingress_lifetime_impl(workspace_id, token, deadline).await
    }

    async fn append_ingress_input(
        &self,
        input: MessageEventInput,
        capsule: crate::memory::ingress_recovery::IngressCapsule,
        lifetime: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::ingress_recovery::IngressAppendOutcome> {
        self.append_ingress_input_impl(input, capsule, lifetime, deadline).await
    }

    async fn list_ingress_recovery_candidates(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        current: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        phase: crate::memory::ingress_recovery::IngressRecoveryScan,
        after_id: i64,
        limit: usize,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::ingress_recovery::IngressRecoveryCandidate>> {
        ingress_recovery::list(
            self.pool.clone(),
            scope.clone(),
            current,
            phase,
            after_id,
            limit,
            deadline,
        )
        .await
    }

    async fn read_ingress_event(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        lookup: &crate::memory::ingress_recovery::IngressEventLookup,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<MessageEvent>> {
        ingress_recovery::read_event(self.pool.clone(), scope.clone(), lookup.clone(), deadline).await
    }

    async fn claim_ingress_recovery(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<crate::memory::ingress_recovery::IngressRecoveryClaim>> {
        Ok(ingress_recovery::acquire(
            self.pool.clone(),
            self.db_path.clone(),
            workspace_id.to_owned(),
            token.to_owned(),
            deadline,
            true,
        )
        .await?
        .map(|lifetime| crate::memory::ingress_recovery::IngressRecoveryClaim { lifetime }))
    }

    async fn commit_ingress_recovery(
        &self,
        commit: crate::memory::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<MessageEvent> {
        // Bound the caller-owned evidence before duplicating it for the shared
        // append path; the blocking transaction rechecks scope and authority.
        ingress_recovery::validate_input_bounds(&commit.evidence)?;
        self.append_event_with_recovery(
            commit.evidence.clone(),
            Some(crate::memory::ingress_recovery::AtomicIngressMutation::Recovery(commit)),
            None,
        )
        .await
        .map(|(event, _)| event)
    }

    async fn append_message_event_until(
        &self,
        input: MessageEventInput,
        deadline: std::time::Instant,
    ) -> anyhow::Result<MessageEvent> {
        ingress_recovery::validate_input_bounds(&input)?;
        self.append_event_with_recovery(input, None, Some(deadline))
            .await
            .map(|(event, _)| event)
    }

    async fn project_console_message_until(
        &self,
        event: &MessageEvent,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        self.project_console_message_with_deadline(event, Some(deadline)).await
    }

    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        self.append_event_with_recovery(input, None, None)
            .await
            .map(|(event, _)| event)
    }

    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.find_message_event_by_idempotency_key_impl(workspace_id, idempotency_key)
            .await
    }

    async fn list_worker_launch_events(
        &self,
        workspace_id: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.list_worker_launch_events_impl(workspace_id, after_id, limit).await
    }

    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        anyhow::ensure!(
            !workspace_id.is_empty() && !owner_id.is_empty() && !idempotency_key.is_empty(),
            "owned event lookup requires workspace, owner and key"
        );
        // The workspace/key index yields at most one row; filter owner before
        // returning any event content to the caller.
        Ok(self
            .find_message_event_by_idempotency_key(workspace_id, idempotency_key)
            .await?
            .filter(|event| event.owner_id.as_deref() == Some(owner_id)))
    }

    async fn find_owned_message_event(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.find_owned_message_event_impl(workspace_id, owner_id, event_id)
            .await
    }

    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.list_message_events_since_impl(principal, after_id, limit).await
    }

    async fn list_message_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.list_message_events_recent_impl(principal, limit).await
    }

    async fn list_message_events_time_range(
        &self,
        principal: &MemoryPrincipal,
        descendant_workspace_root: Option<&str>,
        start: &str,
        end: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.list_message_events_time_range_impl(principal, descendant_workspace_root, start, end, limit)
            .await
    }

    async fn load_recent_shared_context(&self, query: SharedContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        self.load_recent_shared_context_impl(query).await
    }

    async fn retrieve_decision_session(
        &self,
        principal: &MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::traits::DecisionSessionHit>> {
        self.retrieve_decision_session_impl(principal, before_event_id, limit, deadline)
            .await
    }

    async fn load_recent_session_context(&self, query: SessionContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        self.load_recent_session_context_impl(query).await
    }

    async fn append_memory_event(&self, input: MemoryEventInput) -> anyhow::Result<MemoryEvent> {
        self.append_memory_event_impl(input).await
    }

    async fn list_memory_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        self.list_memory_events_since_impl(principal, after_id, limit).await
    }

    async fn list_memory_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        self.list_memory_events_recent_impl(principal, limit).await
    }

    async fn create_memory_draft(&self, input: MemoryDraftInput) -> anyhow::Result<MemoryDraft> {
        self.create_memory_draft_impl(input).await
    }

    async fn list_memory_drafts_for_run(
        &self,
        principal: &MemoryPrincipal,
        worker_run_id: &str,
    ) -> anyhow::Result<Vec<MemoryDraft>> {
        self.list_memory_drafts_for_run_impl(principal, worker_run_id).await
    }

    async fn merge_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        self.merge_memory_draft_impl(principal, draft_id).await
    }

    async fn reject_memory_draft(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        self.reject_memory_draft_impl(principal, draft_id, reason).await
    }

    async fn ingest_document(&self, input: DocumentIngestInput) -> anyhow::Result<DocumentRecord> {
        self.ingest_document_impl(input).await
    }

    async fn retrieve_decision_documents(
        &self,
        query: &str,
        limit: u8,
        principal: &MemoryPrincipal,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::traits::DecisionDocumentHit>> {
        self.retrieve_decision_documents_impl(query, limit, principal, deadline)
            .await
    }

    async fn search_document_chunks(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DocumentSearchResult>> {
        self.search_document_chunks_impl(principal, query, limit).await
    }

    async fn get_document_chunk(&self, chunk_id: &str) -> anyhow::Result<Option<DocumentChunkRecord>> {
        self.get_document_chunk_impl(chunk_id).await
    }

    async fn link_memory_source(&self, input: MemoryLinkInput) -> anyhow::Result<MemoryLink> {
        self.link_memory_source_impl(input).await
    }

    async fn append_retrieval_trace(&self, input: RetrievalTraceInput) -> anyhow::Result<RetrievalTrace> {
        self.append_retrieval_trace_impl(input).await
    }

    async fn append_compaction_run(&self, input: CompactionRunInput) -> anyhow::Result<CompactionRun> {
        self.append_compaction_run_impl(input).await
    }

    async fn health_check(&self) -> bool {
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            // Probed on a reader: readers are opened lazily, so this is the
            // first place a database that read traffic can no longer open
            // becomes visible.
            pool.read()
                .and_then(|reader| reader.connection().map(|conn| conn.execute_batch("SELECT 1").is_ok()))
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }

    async fn reindex(&self) -> anyhow::Result<usize> {
        Self::reindex(self).await
    }
}
