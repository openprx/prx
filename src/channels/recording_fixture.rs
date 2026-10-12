//! Recording prerequisites for exactly two original channel fixtures.
use crate::memory::SqliteMemory;
use crate::memory::traits::{
    Memory, MemoryCategory, MemoryEntry, MemoryPrincipal, MessageEvent, MessageEventInput, SessionContextQuery,
    SharedContextQuery,
};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct RecordedChannelFixtureMemory {
    original: Arc<dyn Memory>,
    local: SqliteMemory,
    owned_readbacks: AtomicUsize,
}

impl RecordedChannelFixtureMemory {
    pub(super) fn owned_readbacks(&self) -> usize {
        self.owned_readbacks.load(Ordering::SeqCst)
    }
}

pub(super) fn attach(ctx: &mut super::ChannelRuntimeContext, workspace: &Path) -> Arc<RecordedChannelFixtureMemory> {
    // These two fixtures use default policy except the explicit autonomy level.
    // Preserve their injected runtime values when moving to a same-owner pin.
    let mut config = ctx.config.pin().effective.as_ref().clone();
    config.workspace_dir = workspace.to_path_buf();
    config.config_path = workspace.join("config.toml");
    config.default_provider = Some(ctx.default_provider.as_str().to_owned());
    config.default_model = Some(ctx.model.as_str().to_owned());
    config.default_temperature = ctx.temperature;
    config.api_key = ctx.api_key.clone();
    config.api_url = ctx.api_url.clone();
    config.reliability = ctx.reliability.as_ref().clone();
    config.autonomy.level = ctx.security.load_full().security.autonomy;
    config.multimodal = ctx.multimodal.clone();
    config.agent.read_only_tool_concurrency_window = ctx.read_only_tool_concurrency_window;
    config.agent.priority_scheduling_enabled = ctx.priority_scheduling_enabled;
    config.agent.low_priority_tools = ctx.low_priority_tools.clone();
    config.tool_tiering = ctx.tool_tiering.clone();
    config.memory.auto_save = ctx.auto_save_memory;
    config.memory.events.record_user_messages = ctx.memory_event_recording.record_user_messages;
    config.memory.events.record_assistant_messages = ctx.memory_event_recording.record_assistant_messages;
    config.memory.events.record_tool_events = ctx.memory_event_recording.record_tool_events;
    assert!(ctx.memory_event_recording.enabled);
    assert!(config.causal_tree.enabled);
    let memory = Arc::new(RecordedChannelFixtureMemory {
        original: Arc::clone(&ctx.memory),
        local: SqliteMemory::new(workspace).expect("channel fixture event ledger"),
        owned_readbacks: AtomicUsize::new(0),
    });
    let shared = crate::config::new_shared(config);
    ctx.config_generation = shared.pin();
    ctx.config = shared;
    ctx.workspace_dir = Arc::new(workspace.to_path_buf());
    ctx.hooks = Arc::new(crate::hooks::HookManager::new(workspace.to_path_buf()));
    ctx.memory = memory.clone();
    memory
}

#[async_trait::async_trait]
impl Memory for RecordedChannelFixtureMemory {
    fn name(&self) -> &str {
        self.original.name()
    }
    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.original.store(key, content, category, session_id).await
    }
    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        self.original.recall(query, limit, session_id).await
    }
    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        self.original.get(key).await
    }
    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.original.list(category, session_id).await
    }
    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.original.forget(key).await
    }
    async fn count(&self) -> anyhow::Result<usize> {
        self.original.count().await
    }
    async fn health_check(&self) -> bool {
        self.original.health_check().await
    }
    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&crate::memory::MemoryWriteContext>,
    ) -> anyhow::Result<()> {
        self.original
            .store_with_context(key, content, category, session_id, context)
            .await
    }
    async fn store_with_context_and_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&crate::memory::MemoryWriteContext>,
        metadata: crate::memory::MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        self.original
            .store_with_context_and_metadata(key, content, category, session_id, context, metadata)
            .await
    }
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
        self.original
            .append_conversation_turn(
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
    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &crate::memory::MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        self.original
            .count_rehearsal_hits(query, limit, context, deadline)
            .await
    }
    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &crate::memory::MemoryWriteContext,
        session_id: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::traits::DecisionMemoryHit>> {
        self.original
            .retrieve_decision_memory(query, limit, context, session_id, deadline)
            .await
    }
    async fn acquire_ingress_lifetime(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Arc<dyn crate::memory::ingress_recovery::IngressLifetime>> {
        self.local.acquire_ingress_lifetime(workspace_id, token, deadline).await
    }
    async fn append_ingress_input(
        &self,
        input: MessageEventInput,
        capsule: crate::memory::ingress_recovery::IngressCapsule,
        lifetime: Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::ingress_recovery::IngressAppendOutcome> {
        self.local
            .append_ingress_input(input, capsule, lifetime, deadline)
            .await
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
        self.local
            .list_ingress_recovery_candidates(scope, current, phase, after_id, limit, deadline)
            .await
    }
    async fn read_ingress_event(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        lookup: &crate::memory::ingress_recovery::IngressEventLookup,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.local.read_ingress_event(scope, lookup, deadline).await
    }
    async fn claim_ingress_recovery(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<crate::memory::ingress_recovery::IngressRecoveryClaim>> {
        self.local.claim_ingress_recovery(workspace_id, token, deadline).await
    }
    async fn commit_ingress_recovery(
        &self,
        commit: crate::memory::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<MessageEvent> {
        self.local.commit_ingress_recovery(commit).await
    }
    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        self.local.append_message_event(input).await
    }
    async fn append_message_event_until(
        &self,
        input: MessageEventInput,
        deadline: std::time::Instant,
    ) -> anyhow::Result<MessageEvent> {
        self.local.append_message_event_until(input, deadline).await
    }
    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.local
            .find_message_event_by_idempotency_key(workspace_id, idempotency_key)
            .await
    }
    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.local
            .find_owned_message_event_by_idempotency_key(workspace_id, owner_id, idempotency_key)
            .await
    }
    async fn find_owned_message_event(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let event = self
            .local
            .find_owned_message_event(workspace_id, owner_id, event_id)
            .await?;
        if let Some(event) = event.as_ref() {
            anyhow::ensure!(
                event.id > 0
                    && event.workspace_id == workspace_id
                    && event.owner_id.as_deref() == Some(owner_id)
                    && event.event_id == event_id,
                "channel fixture owned ledger identity mismatch"
            );
            self.owned_readbacks.fetch_add(1, Ordering::SeqCst);
        }
        Ok(event)
    }
    async fn list_message_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.local.list_message_events_since(principal, after_id, limit).await
    }
    async fn list_message_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.local.list_message_events_recent(principal, limit).await
    }
    async fn load_recent_shared_context(&self, query: SharedContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        self.local.load_recent_shared_context(query).await
    }
    async fn load_recent_session_context(&self, query: SessionContextQuery) -> anyhow::Result<Vec<MessageEvent>> {
        self.local.load_recent_session_context(query).await
    }
    async fn project_console_message(&self, event: &MessageEvent) -> anyhow::Result<()> {
        self.local.project_console_message(event).await
    }
    async fn project_console_message_until(
        &self,
        event: &MessageEvent,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        self.local.project_console_message_until(event, deadline).await
    }
    async fn list_message_events_time_range(
        &self,
        principal: &MemoryPrincipal,
        descendant_workspace_root: Option<&str>,
        start: &str,
        end: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.local
            .list_message_events_time_range(principal, descendant_workspace_root, start, end, limit)
            .await
    }
}
