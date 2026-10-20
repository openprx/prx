#![cfg(test)]

//! Transparent test adapter: all source/lifetime/ACL and learning operations
//! used by this fixture reach one actual SQLite instance without evidence edits.
use crate::memory::principal::MemoryWriteContext;
use crate::memory::traits::*;
use crate::memory::{
    RouterHistoryCommitReceipt, RouterHistoryRead, RouterHistoryReadEvidence, RouterHistoryRow,
    RouterHistoryWriteEvidence, SqliteMemory,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::time::Instant;

#[derive(Debug, PartialEq)]
pub(super) struct ObservedRow {
    pub source_sequence: i64,
    pub terminal_sequence: i64,
    pub terminal: String,
    pub provider: String,
    pub model: String,
    pub success: bool,
    pub embedding: Vec<f32>,
}
impl ObservedRow {
    pub(super) fn from_row(row: &RouterHistoryRow) -> Self {
        Self {
            source_sequence: row.source_sequence,
            terminal_sequence: row.terminal_sequence,
            terminal: row.terminal_event_id.clone(),
            provider: row.provider.clone(),
            model: row.model.clone(),
            success: row.success,
            embedding: row.embedding.clone(),
        }
    }
}
#[derive(Debug)]
pub(super) struct ReadSummary {
    pub source_sequence: i64,
    pub count: usize,
    pub only_row: Option<ObservedRow>,
}
#[derive(Debug, Default)]
pub(super) struct Observations {
    pub query_reads: usize,
    pub last_query_hit_count: Option<usize>,
    pub read_calls: usize,
    pub last_read: Option<Result<ReadSummary, Vec<String>>>,
    pub commit_calls: usize,
    pub last_commit: Option<Result<(), Vec<String>>>,
    pub learning_commits: usize,
    pub last_learning_commit: Option<Result<(), Vec<String>>>,
}

// Never accumulate a call log or copy the 1024-row result. Only the most recent
// result is retained; single-row copies inherit the backend's field/vector bounds.
pub(super) struct ObservedSqlite {
    inner: SqliteMemory,
    pub observations: Mutex<Observations>,
    pub learning_contention: Mutex<Option<std::path::PathBuf>>,
}
impl ObservedSqlite {
    pub(super) fn new(inner: SqliteMemory) -> Self {
        Self {
            inner,
            observations: Mutex::new(Observations::default()),
            learning_contention: Mutex::new(None),
        }
    }
}
fn error_chain(error: &anyhow::Error) -> Vec<String> {
    struct Bounded(String);
    impl std::fmt::Write for Bounded {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            if self.0.len().saturating_add(value.len()) > 512 {
                return Err(std::fmt::Error);
            }
            self.0.push_str(value);
            Ok(())
        }
    }
    error
        .chain()
        .take(8)
        .map(|cause| {
            let mut output = Bounded(String::new());
            if std::fmt::write(&mut output, format_args!("{cause}")).is_err() {
                return "<error exceeds fixture observation bound>".to_owned();
            }
            output.0
        })
        .collect()
}
#[async_trait]
impl Memory for ObservedSqlite {
    async fn read_router_history(
        &self,
        evidence: RouterHistoryReadEvidence,
        deadline: Instant,
    ) -> anyhow::Result<RouterHistoryRead> {
        let result = self.inner.read_router_history(evidence, deadline).await;
        let observation = match &result {
            Ok(read) => Ok(ReadSummary {
                source_sequence: read.source_sequence,
                count: read.rows.len(),
                only_row: if read.rows.len() == 1 {
                    read.rows.first().map(ObservedRow::from_row)
                } else {
                    None
                },
            }),
            Err(error) => Err(error_chain(error)),
        };
        let mut observations = self.observations.lock();
        observations.read_calls += 1;
        observations.last_read = Some(observation);
        drop(observations);
        result
    }
    async fn commit_router_history(
        &self,
        evidence: &RouterHistoryWriteEvidence,
        deadline: Instant,
    ) -> anyhow::Result<RouterHistoryCommitReceipt> {
        let result = self.inner.commit_router_history(evidence, deadline).await;
        let observation = result.as_ref().map(|_| ()).map_err(error_chain);
        let mut observations = self.observations.lock();
        observations.commit_calls += 1;
        observations.last_commit = Some(observation);
        drop(observations);
        result
    }
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.inner.store(key, content, category, session_id).await
    }

    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<()> {
        self.inner
            .store_with_context(key, content, category, session_id, context)
            .await
    }

    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        metadata: MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        self.inner
            .store_with_metadata(key, content, category, session_id, metadata)
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
        self.inner
            .store_with_context_and_metadata(key, content, category, session_id, context, metadata)
            .await
    }

    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner.recall(query, limit, session_id).await
    }

    async fn read_router_learning(
        &self,
        evidence: &RouterLearningEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<RouterLearningRead> {
        self.inner.read_router_learning(evidence, deadline).await
    }

    async fn commit_router_learning(
        &self,
        evidence: &RouterLearningEvidence,
        expected_version: i64,
        next: &RouterLearningSnapshot,
        deadline: std::time::Instant,
    ) -> anyhow::Result<RouterLearningCommit> {
        let contention = self.learning_contention.lock().take();
        let holder = if let Some(path) = contention {
            let (ready, acquired) = tokio::sync::oneshot::channel();
            let job = crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
                let mut connection = rusqlite::Connection::open(path)?;
                let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                ready
                    .send(())
                    .map_err(|_| anyhow::anyhow!("contention fixture receiver closed"))?;
                // A real competing writer outlives the old connection's 25 ms
                // busy timeout, within the unchanged learning deadline.
                std::thread::sleep(std::time::Duration::from_millis(100));
                transaction.rollback()?;
                Ok(())
            });
            acquired.await?;
            Some(job)
        } else {
            None
        };
        let result = self
            .inner
            .commit_router_learning(evidence, expected_version, next, deadline)
            .await;
        if let Some(holder) = holder {
            holder.await??;
        }
        let mut observations = self.observations.lock();
        observations.learning_commits += 1;
        observations.last_learning_commit = Some(result.as_ref().map(|_| ()).map_err(error_chain));
        result
    }

    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        self.inner.count_rehearsal_hits(query, limit, context, deadline).await
    }

    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
        self.observations.lock().query_reads += 1;
        let result = self
            .inner
            .retrieve_decision_memory(query, limit, context, session_id, deadline)
            .await;
        self.observations.lock().last_query_hit_count = result.as_ref().ok().map(Vec::len);
        result
    }

    async fn retrieve_decision_documents(
        &self,
        query: &str,
        limit: u8,
        principal: &MemoryPrincipal,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionDocumentHit>> {
        self.inner
            .retrieve_decision_documents(query, limit, principal, deadline)
            .await
    }

    async fn retrieve_decision_session(
        &self,
        principal: &MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionSessionHit>> {
        self.inner
            .retrieve_decision_session(principal, before_event_id, limit, deadline)
            .await
    }

    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner.recall_with_context(query, limit, session_id, context).await
    }

    async fn recall_with_context_mode(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner
            .recall_with_context_mode(query, limit, session_id, context, mode)
            .await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        self.inner.get(key).await
    }

    async fn get_with_context(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        self.inner.get_with_context(key, context).await
    }

    async fn get_with_context_mode(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        self.inner.get_with_context_mode(key, context, mode).await
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner.list(category, session_id).await
    }

    async fn list_owner_ids(
        &self,
        category: &MemoryCategory,
    ) -> anyhow::Result<Option<std::collections::HashMap<String, Option<String>>>> {
        self.inner.list_owner_ids(category).await
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.inner.forget(key).await
    }

    fn stores_into_shared_prompt_files(&self) -> bool {
        self.inner.stores_into_shared_prompt_files()
    }

    async fn forget_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> anyhow::Result<bool> {
        self.inner.forget_with_context(key, context).await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        self.inner.count().await
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }

    fn supports_document_ingest(&self) -> bool {
        self.inner.supports_document_ingest()
    }

    async fn project_console_message(&self, event: &MessageEvent) -> anyhow::Result<()> {
        self.inner.project_console_message(event).await
    }

    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        self.inner.append_message_event(input).await
    }

    async fn append_message_events(&self, inputs: Vec<MessageEventInput>) -> Vec<anyhow::Result<MessageEvent>> {
        self.inner.append_message_events(inputs).await
    }

    async fn acquire_ingress_lifetime(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<std::sync::Arc<dyn crate::memory::ingress_recovery::IngressLifetime>> {
        self.inner.acquire_ingress_lifetime(workspace_id, token, deadline).await
    }

    async fn append_ingress_input(
        &self,
        input: MessageEventInput,
        capsule: crate::memory::ingress_recovery::IngressCapsule,
        lifetime: std::sync::Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::ingress_recovery::IngressAppendOutcome> {
        self.inner
            .append_ingress_input(input, capsule, lifetime, deadline)
            .await
    }

    async fn list_ingress_recovery_candidates(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        current: std::sync::Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        phase: crate::memory::ingress_recovery::IngressRecoveryScan,
        after_id: i64,
        limit: usize,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::ingress_recovery::IngressRecoveryCandidate>> {
        self.inner
            .list_ingress_recovery_candidates(scope, current, phase, after_id, limit, deadline)
            .await
    }

    async fn read_ingress_event(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        lookup: &crate::memory::ingress_recovery::IngressEventLookup,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.inner.read_ingress_event(scope, lookup, deadline).await
    }

    async fn claim_ingress_recovery(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<crate::memory::ingress_recovery::IngressRecoveryClaim>> {
        self.inner.claim_ingress_recovery(workspace_id, token, deadline).await
    }

    async fn commit_ingress_recovery(
        &self,
        commit: crate::memory::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<MessageEvent> {
        self.inner.commit_ingress_recovery(commit).await
    }

    async fn append_message_event_until(
        &self,
        input: MessageEventInput,
        deadline: std::time::Instant,
    ) -> anyhow::Result<MessageEvent> {
        self.inner.append_message_event_until(input, deadline).await
    }

    async fn project_console_message_until(
        &self,
        event: &MessageEvent,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        self.inner.project_console_message_until(event, deadline).await
    }

    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.inner
            .find_message_event_by_idempotency_key(workspace_id, idempotency_key)
            .await
    }

    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.inner
            .find_owned_message_event_by_idempotency_key(workspace_id, owner_id, idempotency_key)
            .await
    }

    async fn find_owned_message_event(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.inner
            .find_owned_message_event(workspace_id, owner_id, event_id)
            .await
    }

    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.inner.list_message_events_since(principal, after_id, limit).await
    }

    async fn list_message_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.inner.list_message_events_recent(principal, limit).await
    }

    async fn list_message_events_time_range(
        &self,
        principal: &MemoryPrincipal,
        descendant_workspace_root: Option<&str>,
        start: &str,
        end: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.inner
            .list_message_events_time_range(principal, descendant_workspace_root, start, end, limit)
            .await
    }

    async fn load_recent_shared_context(&self, query: SharedContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        self.inner.load_recent_shared_context(query).await
    }

    async fn load_recent_session_context(&self, query: SessionContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        self.inner.load_recent_session_context(query).await
    }

    async fn append_memory_event(&self, input: MemoryEventInput) -> anyhow::Result<MemoryEvent> {
        self.inner.append_memory_event(input).await
    }

    async fn append_compaction_run(&self, input: CompactionRunInput) -> anyhow::Result<CompactionRun> {
        self.inner.append_compaction_run(input).await
    }
}
