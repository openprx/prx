#![cfg(test)]
use super::{CountingProbeMemory, TurnDecisionService};
use crate::memory::SqliteMemory;
use crate::memory::principal::MemoryWriteContext;
use crate::memory::traits::*;
use std::sync::Arc;
use std::time::Instant;

// Actual SQLite owns all persisted ledger/source facts. Only the bounded read
// fault/counter seam uses the original component probe; no execution is claimed.
pub(super) struct LedgerProbeMemory {
    pub(super) ledger: Arc<SqliteMemory>,
    pub(super) probe: Arc<CountingProbeMemory>,
}

#[async_trait::async_trait]
impl Memory for LedgerProbeMemory {
    fn name(&self) -> &str {
        self.ledger.name()
    }
    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.ledger.store(key, content, category, session_id).await
    }
    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<()> {
        self.ledger
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
        self.ledger
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
        self.ledger
            .store_with_context_and_metadata(key, content, category, session_id, context, metadata)
            .await
    }
    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        self.ledger.recall(query, limit, session_id).await
    }
    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.ledger.recall_with_context(query, limit, session_id, context).await
    }
    async fn recall_with_context_mode(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.ledger
            .recall_with_context_mode(query, limit, session_id, context, mode)
            .await
    }
    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        self.ledger.get(key).await
    }
    async fn get_with_context(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        self.ledger.get_with_context(key, context).await
    }
    async fn get_with_context_mode(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        self.ledger.get_with_context_mode(key, context, mode).await
    }
    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.ledger.list(category, session_id).await
    }
    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.ledger.forget(key).await
    }
    async fn forget_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> anyhow::Result<bool> {
        self.ledger.forget_with_context(key, context).await
    }
    async fn count(&self) -> anyhow::Result<usize> {
        self.ledger.count().await
    }
    async fn health_check(&self) -> bool {
        self.ledger.health_check().await
    }
    async fn read_router_history(
        &self,
        evidence: crate::memory::RouterHistoryReadEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::RouterHistoryRead> {
        self.ledger.read_router_history(evidence, deadline).await
    }
    async fn commit_router_history(
        &self,
        evidence: &crate::memory::RouterHistoryWriteEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::RouterHistoryCommitReceipt> {
        self.ledger.commit_router_history(evidence, deadline).await
    }
    async fn read_router_learning(
        &self,
        evidence: &RouterLearningEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<RouterLearningRead> {
        self.ledger.read_router_learning(evidence, deadline).await
    }
    async fn commit_router_learning(
        &self,
        evidence: &RouterLearningEvidence,
        expected_version: i64,
        next: &RouterLearningSnapshot,
        deadline: std::time::Instant,
    ) -> anyhow::Result<RouterLearningCommit> {
        self.ledger
            .commit_router_learning(evidence, expected_version, next, deadline)
            .await
    }
    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        self.ledger.append_message_event(input).await
    }
    async fn append_message_event_until(
        &self,
        input: MessageEventInput,
        deadline: std::time::Instant,
    ) -> anyhow::Result<MessageEvent> {
        self.ledger.append_message_event_until(input, deadline).await
    }
    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.ledger
            .find_message_event_by_idempotency_key(workspace_id, idempotency_key)
            .await
    }
    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.ledger
            .find_owned_message_event_by_idempotency_key(workspace_id, owner_id, idempotency_key)
            .await
    }
    async fn find_owned_message_event(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.ledger
            .find_owned_message_event(workspace_id, owner_id, event_id)
            .await
    }
    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.ledger.list_message_events_since(principal, after_id, limit).await
    }
    async fn list_message_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.ledger.list_message_events_recent(principal, limit).await
    }
    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        self.probe.count_rehearsal_hits(query, limit, context, deadline).await
    }
    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &MemoryWriteContext,
        session_id: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
        self.probe
            .retrieve_decision_memory(query, limit, context, session_id, deadline)
            .await
    }
}

// Bound both actual write queues on success and assertion panic before the
// caller's workspace/runtime may drop. No detached cleanup task is accepted.
pub(super) async fn finish_service_fixture<T>(
    service: &TurnDecisionService,
    work: impl std::future::Future<Output = T>,
) -> T {
    use futures::FutureExt;
    let outcome = std::panic::AssertUnwindSafe(work).catch_unwind().await;
    let drained = service
        .drain_writes(Instant::now() + std::time::Duration::from_secs(5))
        .await;
    match outcome {
        Ok(value) => {
            assert!(drained, "actual fixture decision/event queues drained");
            value
        }
        Err(panic) => {
            if !drained {
                let subscriber = tracing_subscriber::fmt()
                    .with_test_writer()
                    .with_ansi(false)
                    .without_time()
                    .with_max_level(tracing::Level::ERROR)
                    .finish();
                tracing::subscriber::with_default(subscriber, || {
                    tracing::error!("fixture assertion failed and actual decision/event queues did not drain");
                });
            }
            std::panic::resume_unwind(panic)
        }
    }
}
