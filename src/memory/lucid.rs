use super::sqlite::SqliteMemory;
use super::traits::{
    CompactionRun, CompactionRunInput, DecisionMemoryHit, DocumentChunkRecord, DocumentIngestInput, DocumentRecord,
    DocumentSearchResult, Memory, MemoryCategory, MemoryEntry, MemoryEvent, MemoryEventInput, MemoryLink,
    MemoryLinkInput, MemoryPrincipal, MessageEvent, MessageEventInput, RetrievalTrace, RetrievalTraceInput,
    SessionContextQuery, SharedContextQuery,
};
use async_trait::async_trait;
use chrono::Local;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::timeout;

pub struct LucidMemory {
    local: SqliteMemory,
    lucid_cmd: String,
    token_budget: usize,
    workspace_dir: PathBuf,
    recall_timeout: Duration,
    store_timeout: Duration,
    local_hit_threshold: usize,
    failure_cooldown: Duration,
    last_failure_at: Mutex<Option<Instant>>,
}

impl LucidMemory {
    const DEFAULT_LUCID_CMD: &'static str = "lucid";
    const DEFAULT_TOKEN_BUDGET: usize = 200;

    pub fn new(workspace_dir: &Path, local: SqliteMemory, config: &crate::config::MemoryConfig) -> Self {
        let lucid_cmd = config
            .lucid_cmd
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(Self::DEFAULT_LUCID_CMD)
            .to_string();

        let token_budget = config
            .lucid_budget
            .map(|v| v as usize)
            .filter(|v| *v > 0)
            .unwrap_or(Self::DEFAULT_TOKEN_BUDGET);

        let recall_timeout = Duration::from_millis(config.lucid_recall_timeout_ms.max(20));
        let store_timeout = Duration::from_millis(config.lucid_store_timeout_ms.max(50));
        let local_hit_threshold = config.lucid_local_hit_threshold.max(1);
        let failure_cooldown = Duration::from_millis(config.lucid_failure_cooldown_ms.max(100));

        Self {
            local,
            lucid_cmd,
            token_budget,
            workspace_dir: workspace_dir.to_path_buf(),
            recall_timeout,
            store_timeout,
            local_hit_threshold,
            failure_cooldown,
            last_failure_at: Mutex::new(None),
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn with_options(
        workspace_dir: &Path,
        local: SqliteMemory,
        lucid_cmd: String,
        token_budget: usize,
        local_hit_threshold: usize,
        recall_timeout: Duration,
        store_timeout: Duration,
        failure_cooldown: Duration,
    ) -> Self {
        Self {
            local,
            lucid_cmd,
            token_budget,
            workspace_dir: workspace_dir.to_path_buf(),
            recall_timeout,
            store_timeout,
            local_hit_threshold: local_hit_threshold.max(1),
            failure_cooldown,
            last_failure_at: Mutex::new(None),
        }
    }

    fn in_failure_cooldown(&self) -> bool {
        let guard = self.last_failure_at.lock();
        guard
            .as_ref()
            .is_some_and(|last| last.elapsed() < self.failure_cooldown)
    }

    fn mark_failure_now(&self) {
        let mut guard = self.last_failure_at.lock();
        *guard = Some(Instant::now());
    }

    fn clear_failure(&self) {
        let mut guard = self.last_failure_at.lock();
        *guard = None;
    }

    const fn to_lucid_type(category: &MemoryCategory) -> &'static str {
        match category {
            MemoryCategory::Core => "decision",
            MemoryCategory::Daily => "context",
            MemoryCategory::Conversation => "conversation",
            MemoryCategory::Custom(_) => "learning",
        }
    }

    fn to_memory_category(label: &str) -> MemoryCategory {
        let normalized = label.to_lowercase();
        if normalized.contains("visual") {
            return MemoryCategory::Custom("visual".to_string());
        }

        match normalized.as_str() {
            "decision" | "learning" | "solution" => MemoryCategory::Core,
            "context" | "conversation" => MemoryCategory::Conversation,
            "bug" => MemoryCategory::Daily,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    fn merge_results(
        primary_results: Vec<MemoryEntry>,
        secondary_results: Vec<MemoryEntry>,
        limit: usize,
    ) -> Vec<MemoryEntry> {
        if limit == 0 {
            return Vec::new();
        }

        let mut merged = Vec::new();
        let mut seen = HashSet::new();

        for entry in primary_results.into_iter().chain(secondary_results) {
            let signature = format!("{}\u{0}{}", entry.key.to_lowercase(), entry.content.to_lowercase());

            if seen.insert(signature) {
                merged.push(entry);
                if merged.len() >= limit {
                    break;
                }
            }
        }

        merged
    }

    fn parse_lucid_context(raw: &str) -> Vec<MemoryEntry> {
        let mut in_context_block = false;
        let mut entries = Vec::new();
        let now = Local::now().to_rfc3339();

        for line in raw.lines().map(str::trim) {
            if line == "<lucid-context>" {
                in_context_block = true;
                continue;
            }

            if line == "</lucid-context>" {
                break;
            }

            if !in_context_block || line.is_empty() {
                continue;
            }

            let Some(rest) = line.strip_prefix("- [") else {
                continue;
            };

            let Some((label, content_part)) = rest.split_once(']') else {
                continue;
            };

            let content = content_part.trim();
            if content.is_empty() {
                continue;
            }

            let rank = entries.len();
            entries.push(MemoryEntry {
                id: format!("lucid:{rank}"),
                key: format!("lucid_{rank}"),
                content: content.to_string(),
                category: Self::to_memory_category(label.trim()),
                timestamp: now.clone(),
                session_id: None,
                score: Some((rank as f64).mul_add(-0.05, 1.0).max(0.1)),
                tags: None,
                access_count: None,
                useful_count: None,
                source: None,
                source_confidence: None,
                verification_status: None,
                lifecycle_state: None,
                compressed_from: None,
            });
        }

        entries
    }

    async fn run_lucid_command_raw(
        lucid_cmd: &str,
        args: &[String],
        timeout_window: Duration,
    ) -> anyhow::Result<String> {
        let mut cmd = Command::new(lucid_cmd);
        cmd.args(args);
        cmd.kill_on_drop(true);

        let output = timeout(timeout_window, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("lucid command timed out after {}ms", timeout_window.as_millis()))??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("lucid command failed: {stderr}");
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn run_lucid_command(&self, args: &[String], timeout_window: Duration) -> anyhow::Result<String> {
        Self::run_lucid_command_raw(&self.lucid_cmd, args, timeout_window).await
    }

    fn build_store_args(&self, key: &str, content: &str, category: &MemoryCategory) -> Vec<String> {
        let payload = format!("{key}: {content}");
        vec![
            "store".to_string(),
            payload,
            format!("--type={}", Self::to_lucid_type(category)),
            format!("--project={}", self.workspace_dir.display()),
        ]
    }

    fn build_recall_args(&self, query: &str) -> Vec<String> {
        vec![
            "context".to_string(),
            query.to_string(),
            format!("--budget={}", self.token_budget),
            format!("--project={}", self.workspace_dir.display()),
        ]
    }

    async fn sync_to_lucid_async(&self, key: &str, content: &str, category: &MemoryCategory) {
        let args = self.build_store_args(key, content, category);
        if let Err(error) = self.run_lucid_command(&args, self.store_timeout).await {
            tracing::debug!(
                command = %self.lucid_cmd,
                error = %error,
                "Lucid store sync failed; sqlite remains authoritative"
            );
        }
    }

    async fn recall_from_lucid(&self, query: &str) -> anyhow::Result<Vec<MemoryEntry>> {
        let args = self.build_recall_args(query);
        let output = self.run_lucid_command(&args, self.recall_timeout).await?;
        Ok(Self::parse_lucid_context(&output))
    }
}

#[async_trait]
impl Memory for LucidMemory {
    fn name(&self) -> &str {
        "lucid"
    }

    async fn mirror_operator_write(&self, key: &str, content: &str, category: &MemoryCategory) -> anyhow::Result<()> {
        // The local SQLite store owns the workspace Markdown mirror.
        self.local.mirror_operator_write(key, content, category).await
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.local.store(key, content, category.clone(), session_id).await?;
        self.sync_to_lucid_async(key, content, &category).await;
        Ok(())
    }

    /// Context-bearing writes go to the local SQLite store with their context,
    /// exactly as reads do (`recall_with_context`). The trait default called
    /// `store`, which dropped the context: every network caller's row landed
    /// with no owner or channel and was exported to `MEMORY_SNAPSHOT.md` as an
    /// operator row (D-026 N-1, R-INT2A F2).
    ///
    /// An owner-isolated write is not copied to the Lucid service, which keeps
    /// no per-row owner and would hand the text to every other caller's
    /// recall; the local store remains authoritative for it.
    async fn store_with_context(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&crate::memory::principal::MemoryWriteContext>,
    ) -> anyhow::Result<()> {
        self.local
            .store_with_context(key, content, category.clone(), session_id, context)
            .await?;
        if !context.is_some_and(|context| context.require_owner_match) {
            self.sync_to_lucid_async(key, content, &category).await;
        }
        Ok(())
    }

    /// Metadata-bearing writes (a plugin's origin, for one) reach the local
    /// store with their metadata; see `store_with_context`.
    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        metadata: crate::memory::traits::MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        self.local
            .store_with_metadata(key, content, category.clone(), session_id, metadata)
            .await?;
        self.sync_to_lucid_async(key, content, &category).await;
        Ok(())
    }

    /// Both together; see `store_with_context`.
    async fn store_with_context_and_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        context: Option<&crate::memory::principal::MemoryWriteContext>,
        metadata: crate::memory::traits::MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        self.local
            .store_with_context_and_metadata(key, content, category.clone(), session_id, context, metadata)
            .await?;
        if !context.is_some_and(|context| context.require_owner_match) {
            self.sync_to_lucid_async(key, content, &category).await;
        }
        Ok(())
    }

    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        let local_results = self.local.recall(query, limit, session_id).await?;
        if limit == 0 || local_results.len() >= limit || local_results.len() >= self.local_hit_threshold {
            return Ok(local_results);
        }

        if self.in_failure_cooldown() {
            return Ok(local_results);
        }

        match self.recall_from_lucid(query).await {
            Ok(lucid_results) if !lucid_results.is_empty() => {
                self.clear_failure();
                Ok(Self::merge_results(local_results, lucid_results, limit))
            }
            Ok(_) => {
                self.clear_failure();
                Ok(local_results)
            }
            Err(error) => {
                self.mark_failure_now();
                tracing::debug!(
                    command = %self.lucid_cmd,
                    error = %error,
                    "Lucid context unavailable; using local sqlite results"
                );
                Ok(local_results)
            }
        }
    }

    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &crate::memory::principal::MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        self.local.count_rehearsal_hits(query, limit, context, deadline).await
    }

    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &super::principal::MemoryWriteContext,
        session_id: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
        self.local
            .retrieve_decision_memory(query, limit, context, session_id, deadline)
            .await
    }

    async fn retrieve_decision_documents(
        &self,
        query: &str,
        limit: u8,
        principal: &super::traits::MemoryPrincipal,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<super::traits::DecisionDocumentHit>> {
        self.local
            .retrieve_decision_documents(query, limit, principal, deadline)
            .await
    }

    async fn retrieve_decision_session(
        &self,
        principal: &super::traits::MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<super::traits::DecisionSessionHit>> {
        self.local
            .retrieve_decision_session(principal, before_event_id, limit, deadline)
            .await
    }

    /// Owner-isolated recall is answered by the local SQLite store alone.
    ///
    /// This backend is a wrapper, not a leaf: its rows live in a real
    /// `SqliteMemory` and do carry `owner_id`. Forwarding only the
    /// context-free methods left the trait's fail-closed default to answer for
    /// it, so an owner-isolated caller reached none of its **own** rows here.
    /// The local store can scope the read, so it gets the context.
    ///
    /// The Lucid service half is deliberately not consulted under isolation: it
    /// returns unattributed context with no per-row owner, which is exactly the
    /// case the fail-closed default exists for. A caller that is not
    /// owner-isolated takes the historical path below, byte for byte —
    /// local hit, threshold, cooldown and merge included.
    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&crate::memory::principal::MemoryWriteContext>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if context.is_some_and(|context| context.require_owner_match) {
            return self.local.recall_with_context(query, limit, session_id, context).await;
        }
        self.recall(query, limit, session_id).await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        self.local.get(key).await
    }

    /// The keyed twin of `recall_with_context`, for the same reason.
    ///
    /// The `*_with_context_mode` variants need no override: an owner-isolated
    /// context is forced to `Enforce` before the mode is consulted, so they
    /// route through this method and `recall_with_context` above.
    async fn get_with_context(
        &self,
        key: &str,
        context: Option<&crate::memory::principal::MemoryWriteContext>,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        if context.is_some_and(|context| context.require_owner_match) {
            return self.local.get_with_context(key, context).await;
        }
        self.get(key).await
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.local.list(category, session_id).await
    }

    async fn list_owner_ids(
        &self,
        category: &MemoryCategory,
    ) -> anyhow::Result<Option<std::collections::HashMap<String, Option<String>>>> {
        self.local.list_owner_ids(category).await
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.local.forget(key).await
    }

    /// An owner-isolated forget is answered by the local store, which deletes
    /// the caller's own row and no other owner's.
    ///
    /// Owner-isolated writes land only in the local store (see
    /// `store_with_context`), so that is where they are forgotten; the Lucid
    /// service never held them. Without this override the trait's
    /// fail-closed default answered `false` and the caller could store a row
    /// it could never remove. A caller that is not owner-isolated keeps the
    /// historical path, which is the trait default's: `forget`.
    async fn forget_with_context(
        &self,
        key: &str,
        context: Option<&crate::memory::principal::MemoryWriteContext>,
    ) -> anyhow::Result<bool> {
        if context.is_some_and(|context| context.require_owner_match) {
            return self.local.forget_with_context(key, context).await;
        }
        self.forget(key).await
    }

    async fn increment_useful_count(&self, id: &str) -> anyhow::Result<()> {
        self.local.increment_useful_count(id).await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        self.local.count().await
    }

    async fn health_check(&self) -> bool {
        self.local.health_check().await
    }

    async fn reindex(&self) -> anyhow::Result<usize> {
        self.local.reindex().await
    }

    // Scoped Router authority is verified by the authoritative local store.
    // These operations must never invoke the external Lucid recall/sync path.
    async fn read_router_history(
        &self,
        evidence: super::RouterHistoryReadEvidence,
        deadline: Instant,
    ) -> anyhow::Result<super::RouterHistoryRead> {
        self.local.read_router_history(evidence, deadline).await
    }

    async fn commit_router_history(
        &self,
        evidence: &super::RouterHistoryWriteEvidence,
        deadline: Instant,
    ) -> anyhow::Result<super::RouterHistoryCommitReceipt> {
        self.local.commit_router_history(evidence, deadline).await
    }

    async fn read_router_learning(
        &self,
        evidence: &super::traits::RouterLearningEvidence,
        deadline: Instant,
    ) -> anyhow::Result<super::traits::RouterLearningRead> {
        self.local.read_router_learning(evidence, deadline).await
    }

    async fn commit_router_learning(
        &self,
        evidence: &super::traits::RouterLearningEvidence,
        expected_version: i64,
        next: &super::traits::RouterLearningSnapshot,
        deadline: Instant,
    ) -> anyhow::Result<super::traits::RouterLearningCommit> {
        self.local
            .commit_router_learning(evidence, expected_version, next, deadline)
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
    async fn append_message_event_until(
        &self,
        input: MessageEventInput,
        deadline: std::time::Instant,
    ) -> anyhow::Result<MessageEvent> {
        self.local.append_message_event_until(input, deadline).await
    }
    async fn project_console_message_until(
        &self,
        event: &MessageEvent,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        self.local.project_console_message_until(event, deadline).await
    }

    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        self.local.append_message_event(input).await
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

    async fn list_console_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<crate::memory::ConversationTurn>> {
        self.local
            .list_console_conversation_turns(principal, session_key, limit, offset)
            .await
    }

    async fn list_console_terminal_events(
        &self,
        workspace_id: &str,
        session_key: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.local
            .list_console_terminal_events(workspace_id, session_key, after_id, limit)
            .await
    }

    async fn project_console_message(&self, event: &MessageEvent) -> anyhow::Result<()> {
        self.local.project_console_message(event).await
    }

    async fn list_worker_launch_events(
        &self,
        workspace_id: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.local
            .list_worker_launch_events(workspace_id, after_id, limit)
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
        self.local
            .find_owned_message_event(workspace_id, owner_id, event_id)
            .await
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

    async fn append_memory_event(&self, input: MemoryEventInput) -> anyhow::Result<MemoryEvent> {
        self.local.append_memory_event(input).await
    }

    async fn move_to_trash(&self, key: &str, reason: &str, grace_days: u32) -> anyhow::Result<Option<String>> {
        self.local.move_to_trash(key, reason, grace_days).await
    }

    async fn create_evolution_proposal(
        &self,
        draft: crate::self_system::evolution::EvolutionProposalDraft,
    ) -> anyhow::Result<String> {
        self.local.create_evolution_proposal(draft).await
    }

    async fn list_evolution_proposals(
        &self,
        principal: &MemoryPrincipal,
        filter: crate::self_system::evolution::ProposalFilter,
    ) -> anyhow::Result<Vec<crate::self_system::evolution::EvolutionProposalDraft>> {
        self.local.list_evolution_proposals(principal, filter).await
    }

    async fn get_evolution_proposal(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<crate::self_system::evolution::EvolutionProposalDraft>> {
        self.local.get_evolution_proposal(principal, draft_id).await
    }

    async fn update_evolution_proposal_status(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        update: crate::self_system::evolution::ProposalStatusUpdate,
    ) -> anyhow::Result<()> {
        self.local
            .update_evolution_proposal_status(principal, draft_id, update)
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
        self.local
            .append_evolution_proposal_event(principal, draft_id, event_type, actor, payload_json)
            .await
    }

    async fn list_memory_events_since(
        &self,
        principal: &MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        self.local.list_memory_events_since(principal, after_id, limit).await
    }

    async fn list_memory_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEvent>> {
        self.local.list_memory_events_recent(principal, limit).await
    }

    async fn ingest_document(&self, input: DocumentIngestInput) -> anyhow::Result<DocumentRecord> {
        self.local.ingest_document(input).await
    }

    async fn search_document_chunks(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DocumentSearchResult>> {
        self.local.search_document_chunks(principal, query, limit).await
    }

    async fn get_document_chunk(&self, chunk_id: &str) -> anyhow::Result<Option<DocumentChunkRecord>> {
        self.local.get_document_chunk(chunk_id).await
    }

    async fn link_memory_source(&self, input: MemoryLinkInput) -> anyhow::Result<MemoryLink> {
        self.local.link_memory_source(input).await
    }

    async fn append_retrieval_trace(&self, input: RetrievalTraceInput) -> anyhow::Result<RetrievalTrace> {
        self.local.append_retrieval_trace(input).await
    }

    async fn append_compaction_run(&self, input: CompactionRunInput) -> anyhow::Result<CompactionRun> {
        self.local.append_compaction_run(input).await
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal,
        clippy::literal_string_with_formatting_args
    )]
    use super::*;
    use crate::memory::traits::MemoryReadMode;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn write_fake_lucid_script(dir: &Path) -> String {
        let script_path = dir.join("fake-lucid.sh");
        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "store" ]]; then
  echo '{"success":true,"id":"mem_1"}'
  exit 0
fi

if [[ "${1:-}" == "context" ]]; then
  cat <<'EOF'
<lucid-context>
Auth context snapshot
- [decision] Use token refresh middleware
- [context] Working in src/auth.rs
</lucid-context>
EOF
  exit 0
fi

echo "unsupported command" >&2
exit 1
"#;

        fs::write(&script_path, script).unwrap();
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).unwrap();
        script_path.display().to_string()
    }

    fn write_delayed_lucid_script(dir: &Path) -> String {
        let script_path = dir.join("delayed-lucid.sh");
        let script = r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "store" ]]; then
  echo '{"success":true,"id":"mem_1"}'
  exit 0
fi

if [[ "${1:-}" == "context" ]]; then
  # Simulate a cold start: the answer arrives late, and recall still waits for it.
  sleep 0.2
  cat <<'EOF'
<lucid-context>
- [decision] Delayed token refresh guidance
</lucid-context>
EOF
  exit 0
fi

echo "unsupported command" >&2
exit 1
"#;

        fs::write(&script_path, script).unwrap();
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).unwrap();
        script_path.display().to_string()
    }

    fn write_probe_lucid_script(dir: &Path, marker_path: &Path) -> String {
        let script_path = dir.join("probe-lucid.sh");
        let marker = marker_path.display().to_string();
        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${{1:-}}" == "store" ]]; then
  echo '{{"success":true,"id":"mem_store"}}'
  exit 0
fi

if [[ "${{1:-}}" == "context" ]]; then
  printf 'context\n' >> "{marker}"
  cat <<'EOF'
<lucid-context>
- [decision] should not be used when local hits are enough
</lucid-context>
EOF
  exit 0
fi

echo "unsupported command" >&2
exit 1
"#
        );

        fs::write(&script_path, script).unwrap();
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).unwrap();
        script_path.display().to_string()
    }

    /// A recall budget no fake script can plausibly exhaust.
    ///
    /// The fakes are `bash` scripts, and starting one under a parallel test run
    /// has been measured at well over two seconds. A test that wants to see
    /// what Lucid answered must not race process start-up against the clock:
    /// when it loses, the recall times out, the answer is silently the local
    /// one, and the assertion blames the merge. None of these tests is about
    /// the timeout, so the budget is only a guard against a script that hangs.
    const HANG_GUARD: Duration = Duration::from_mins(1);

    fn test_memory(workspace: &Path, cmd: String) -> LucidMemory {
        let sqlite = SqliteMemory::new(workspace).unwrap();
        LucidMemory::with_options(
            workspace,
            sqlite,
            cmd,
            200,
            3,
            HANG_GUARD,
            Duration::from_millis(400),
            Duration::from_secs(2),
        )
    }

    #[tokio::test]
    async fn decision_memory_reads_use_local_sqlite_without_lucid_process() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("context_calls.log");
        let cmd = write_probe_lucid_script(tmp.path(), &marker);
        let memory = test_memory(tmp.path(), cmd);
        let context = crate::memory::principal::MemoryWriteContext {
            workspace_id: Some(tmp.path().to_string_lossy().into_owned()),
            channel: Some("cli".into()),
            chat_type: Some("private".into()),
            chat_id: Some("agent:prx_run".into()),
            raw_sender: Some("prx_user".into()),
            ..Default::default()
        };
        memory
            .local
            .store_with_context(
                "prx_rehearsal_note",
                "prx_rehearsal_keyword scoped note",
                MemoryCategory::Conversation,
                None,
                Some(&context),
            )
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        assert_eq!(
            memory
                .count_rehearsal_hits("prx_rehearsal_keyword", 5, &context, deadline)
                .await
                .unwrap(),
            1
        );
        let hits = memory
            .retrieve_decision_memory("prx_rehearsal_keyword", 5, &context, None, deadline)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "prx_rehearsal_keyword scoped note");
        assert!(
            !marker.exists(),
            "decision Memory reads must not call the Lucid service"
        );
    }

    fn snapshot_text(workspace: &Path) -> String {
        crate::memory::snapshot::export_snapshot(workspace).unwrap();
        fs::read_to_string(workspace.join(crate::memory::snapshot::SNAPSHOT_FILENAME)).unwrap_or_default()
    }

    /// R-INT2A F2: a network caller's write on Lucid keeps its origin and
    /// owner in the local store, so it stays out of the operator snapshot
    /// (D-026 N-1). Lucid used the trait default, which calls `store` and
    /// drops the context: the row landed ownerless and was exported.
    #[tokio::test]
    async fn a_network_callers_write_on_lucid_keeps_its_owner_and_stays_out_of_the_snapshot() {
        let tmp = TempDir::new().unwrap();
        let memory = test_memory(tmp.path(), "nonexistent-lucid-binary".to_string());
        let context = crate::memory::MemoryWriteContext {
            channel: Some("webhook".to_string()),
            sender_id: Some("webhook-owner".to_string()),
            ..crate::memory::MemoryWriteContext::default()
        };
        memory
            .store_with_context(
                "net-note",
                "webhook caller text",
                MemoryCategory::Core,
                None,
                Some(&context),
            )
            .await
            .unwrap();
        let owners = memory
            .list_owner_ids(&MemoryCategory::Core)
            .await
            .unwrap()
            .unwrap_or_default();
        assert_eq!(
            owners.values().cloned().collect::<Vec<_>>(),
            vec![Some("webhook-owner".to_string())],
            "the row keeps its owner"
        );
        drop(memory);
        let snapshot = snapshot_text(tmp.path());
        assert!(!snapshot.contains("webhook caller text"), "{snapshot}");
    }

    /// R-INT2A F2 for plugin rows: the metadata a plugin write carries
    /// reaches the local store, so the row stays out of the snapshot on
    /// Lucid as it does on SQLite.
    #[cfg(feature = "wasm-plugins")]
    #[tokio::test]
    async fn a_plugin_write_on_lucid_stays_out_of_the_snapshot() {
        let tmp = TempDir::new().unwrap();
        let memory = test_memory(tmp.path(), "nonexistent-lucid-binary".to_string());
        crate::plugins::capabilities::common::store_plugin_memory(&memory, "sample-plugin", "plugin text", "core")
            .await
            .unwrap();
        drop(memory);
        let snapshot = snapshot_text(tmp.path());
        assert!(!snapshot.contains("plugin text"), "{snapshot}");
    }

    #[tokio::test]
    async fn lucid_name() {
        let tmp = TempDir::new().unwrap();
        let memory = test_memory(tmp.path(), "nonexistent-lucid-binary".to_string());
        assert_eq!(memory.name(), "lucid");
    }

    #[tokio::test]
    async fn store_succeeds_when_lucid_missing() {
        let tmp = TempDir::new().unwrap();
        let memory = test_memory(tmp.path(), "nonexistent-lucid-binary".to_string());

        memory
            .store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();

        let entry = memory.get("lang").await.unwrap();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().content, "User prefers Rust");
    }

    #[tokio::test]
    async fn recall_merges_lucid_and_local_results() {
        let tmp = TempDir::new().unwrap();
        let fake_cmd = write_fake_lucid_script(tmp.path());
        let memory = test_memory(tmp.path(), fake_cmd);

        memory
            .store(
                "local_note",
                "Local sqlite auth fallback note",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();

        let entries = memory.recall("auth", 5, None).await.unwrap();

        assert!(
            entries
                .iter()
                .any(|e| e.content.contains("Local sqlite auth fallback note"))
        );
        assert!(entries.iter().any(|e| e.content.contains("token refresh")));
    }

    #[tokio::test]
    async fn recall_handles_lucid_cold_start_delay_within_timeout() {
        let tmp = TempDir::new().unwrap();
        let delayed_cmd = write_delayed_lucid_script(tmp.path());
        let memory = test_memory(tmp.path(), delayed_cmd);

        memory
            .store(
                "local_note",
                "Local sqlite auth fallback note",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();

        let entries = memory.recall("auth", 5, None).await.unwrap();

        assert!(
            entries
                .iter()
                .any(|e| e.content.contains("Local sqlite auth fallback note"))
        );
        assert!(
            entries
                .iter()
                .any(|e| e.content.contains("Delayed token refresh guidance"))
        );
    }

    #[tokio::test]
    async fn recall_skips_lucid_when_local_hits_are_enough() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("context_calls.log");
        let probe_cmd = write_probe_lucid_script(tmp.path(), &marker);

        let sqlite = SqliteMemory::new(tmp.path()).unwrap();
        let memory = LucidMemory::with_options(
            tmp.path(),
            sqlite,
            probe_cmd,
            200,
            1,
            Duration::from_millis(500),
            Duration::from_millis(400),
            Duration::from_secs(2),
        );

        memory
            .store("pref", "Rust should stay local-first", MemoryCategory::Core, None)
            .await
            .unwrap();

        let entries = memory.recall("rust", 5, None).await.unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.content.contains("Rust should stay local-first"))
        );

        let context_calls = tokio::fs::read_to_string(&marker).await.unwrap_or_default();
        assert!(
            context_calls.trim().is_empty(),
            "Expected local-hit short-circuit; got calls: {context_calls}"
        );
    }

    fn write_failing_lucid_script(dir: &Path, marker_path: &Path) -> String {
        let script_path = dir.join("failing-lucid.sh");
        let marker = marker_path.display().to_string();
        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${{1:-}}" == "store" ]]; then
  echo '{{"success":true,"id":"mem_store"}}'
  exit 0
fi

if [[ "${{1:-}}" == "context" ]]; then
  printf 'context\n' >> "{marker}"
  echo "simulated lucid failure" >&2
  exit 1
fi

echo "unsupported command" >&2
exit 1
"#
        );

        fs::write(&script_path, script).unwrap();
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).unwrap();
        script_path.display().to_string()
    }

    /// A failed Lucid call puts recall into cooldown, so the next recall does
    /// not start the command again.
    ///
    /// The failure has to be the script's own `exit 1`, reached after it has
    /// recorded the call. With a 500 ms recall budget a slow `bash` start under
    /// a parallel run timed out first and was killed before writing the
    /// marker, so the count read 0 although the cooldown worked. The budget is
    /// now [`HANG_GUARD`] and the cooldown an hour, so neither clock can decide
    /// the outcome: the two recalls run back to back.
    #[tokio::test]
    async fn failure_cooldown_avoids_repeated_lucid_calls() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("failing_context_calls.log");
        let failing_cmd = write_failing_lucid_script(tmp.path(), &marker);

        let sqlite = SqliteMemory::new(tmp.path()).unwrap();
        let memory = LucidMemory::with_options(
            tmp.path(),
            sqlite,
            failing_cmd,
            200,
            99,
            HANG_GUARD,
            Duration::from_millis(400),
            Duration::from_hours(1),
        );

        let first = memory.recall("auth", 5, None).await.unwrap();
        let second = memory.recall("auth", 5, None).await.unwrap();

        assert!(first.is_empty());
        assert!(second.is_empty());

        let calls = tokio::fs::read_to_string(&marker).await.unwrap_or_default();
        assert_eq!(calls.lines().count(), 1);
    }

    const LUCID_WORKSPACE: &str = "ws-lucid";
    const LUCID_OWNER: &str = "owner:ws-lucid:agent:issuer-a.example:shared-subject";
    const LUCID_OTHER_OWNER: &str = "owner:ws-lucid:agent:issuer-b.example:shared-subject";
    const LUCID_KEY: &str = "first-owner-note";
    const LUCID_CONTENT: &str = "alpha rollout note authored by the first owner";
    const LUCID_QUERY: &str = "rollout";

    fn lucid_owner_context(owner_id: &str, isolated: bool) -> crate::memory::principal::MemoryWriteContext {
        crate::memory::principal::MemoryWriteContext {
            workspace_id: Some(LUCID_WORKSPACE.to_string()),
            channel: Some("mcp".to_string()),
            chat_type: Some("gateway".to_string()),
            chat_id: Some("mcp:shared-subject".to_string()),
            sender_id: Some(owner_id.to_string()),
            raw_sender: Some("shared-subject".to_string()),
            owner_id: Some(owner_id.to_string()),
            require_owner_match: isolated,
        }
    }

    /// `lucid` wraps a real `SqliteMemory` whose rows do carry `owner_id`, but
    /// it forwarded only the context-free methods, so the trait's fail-closed
    /// default answered for it and an owner-isolated caller reached none of its
    /// **own** rows either. The two context reads are now forwarded to the
    /// local store, which can scope them.
    ///
    /// MUTATION GUARD: delete the `recall_with_context` / `get_with_context`
    /// overrides and the first two assertions go red; forward without passing
    /// the context and the last two do.
    #[tokio::test]
    async fn owner_isolated_reads_reach_the_callers_own_rows_in_the_local_store() {
        let tmp = TempDir::new().unwrap();
        let writer = lucid_owner_context(LUCID_OWNER, false);
        {
            let seeder = SqliteMemory::new(tmp.path()).unwrap();
            seeder
                .store_with_context_and_metadata(
                    LUCID_KEY,
                    LUCID_CONTENT,
                    MemoryCategory::Core,
                    None,
                    Some(&writer),
                    crate::memory::traits::MemoryStoreMetadata {
                        workspace_id: Some(LUCID_WORKSPACE.to_string()),
                        owner_id: Some(LUCID_OWNER.to_string()),
                        channel: Some("mcp".to_string()),
                        ..crate::memory::traits::MemoryStoreMetadata::default()
                    },
                )
                .await
                .unwrap();
        }

        let memory = LucidMemory::with_options(
            tmp.path(),
            SqliteMemory::new(tmp.path()).unwrap(),
            write_fake_lucid_script(tmp.path()),
            200,
            1,
            Duration::from_millis(500),
            Duration::from_millis(400),
            Duration::from_secs(2),
        );

        let owner = lucid_owner_context(LUCID_OWNER, true);
        let recalled = memory
            .recall_with_context(LUCID_QUERY, 10, None, Some(&owner))
            .await
            .unwrap();
        assert!(
            recalled.iter().any(|entry| entry.key == LUCID_KEY),
            "an owner-isolated caller must still read its own rows on the lucid backend: {:?}",
            recalled.iter().map(|entry| entry.key.as_str()).collect::<Vec<_>>()
        );
        assert!(
            memory
                .get_with_context(LUCID_KEY, Some(&owner))
                .await
                .unwrap()
                .is_some(),
            "and by exact key too"
        );

        let other = lucid_owner_context(LUCID_OTHER_OWNER, true);
        assert!(
            memory
                .recall_with_context(LUCID_QUERY, 10, None, Some(&other))
                .await
                .unwrap()
                .is_empty(),
            "forwarding must not reopen the boundary for a different owner"
        );
        assert!(
            memory
                .get_with_context(LUCID_KEY, Some(&other))
                .await
                .unwrap()
                .is_none(),
            "nor for its exact-key read"
        );

        // The two `_mode` entry points route through the overrides above,
        // because an owner-isolated context is forced to `Enforce` before the
        // mode is consulted. An `Observe` request therefore cannot reopen the
        // boundary here either.
        assert!(
            memory
                .recall_with_context_mode(LUCID_QUERY, 10, None, Some(&other), MemoryReadMode::Observe)
                .await
                .unwrap()
                .is_empty(),
            "Observe must not reopen the owner boundary on the lucid backend"
        );
        assert!(
            memory
                .get_with_context_mode(LUCID_KEY, Some(&other), MemoryReadMode::Observe)
                .await
                .unwrap()
                .is_none(),
            "nor for the keyed read"
        );
        assert!(
            memory
                .recall_with_context_mode(LUCID_QUERY, 10, None, Some(&owner), MemoryReadMode::Observe)
                .await
                .unwrap()
                .iter()
                .any(|entry| entry.key == LUCID_KEY),
            "and the owner still reads its own row through the mode variant"
        );
    }

    /// The non-isolated path is untouched: a context-carrying read with no
    /// isolation bit answers exactly what `LucidMemory::recall` /
    /// `LucidMemory::get` answer, which is the historical behaviour of the
    /// trait default it replaces.
    ///
    /// The equivalence is asserted rather than the Lucid process being
    /// observed: whether the service half is reached depends on the local hit
    /// threshold and a wall-clock timeout, and a test must not depend on
    /// either. The *isolated* path is a different answer by construction and is
    /// pinned by the test above.
    ///
    /// Both reads do reach the fake here, so the recall budget is
    /// [`HANG_GUARD`] and the cooldown an hour: with a 2 s budget a slow `bash`
    /// start could time out one read and not the other, and the two answers
    /// would differ for a reason that has nothing to do with the path.
    #[tokio::test]
    async fn a_non_isolated_context_read_keeps_the_historical_lucid_path() {
        let tmp = TempDir::new().unwrap();
        let memory = LucidMemory::with_options(
            tmp.path(),
            SqliteMemory::new(tmp.path()).unwrap(),
            write_fake_lucid_script(tmp.path()),
            200,
            99,
            HANG_GUARD,
            Duration::from_secs(2),
            Duration::from_hours(1),
        );
        memory
            .store(LUCID_KEY, LUCID_CONTENT, MemoryCategory::Core, None)
            .await
            .unwrap();

        let reader = lucid_owner_context(LUCID_OWNER, false);
        let with_context = memory
            .recall_with_context(LUCID_QUERY, 5, None, Some(&reader))
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.content)
            .collect::<Vec<_>>();
        let historical = memory
            .recall(LUCID_QUERY, 5, None)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.content)
            .collect::<Vec<_>>();
        assert!(
            historical.iter().any(|content| content.contains(LUCID_CONTENT)),
            "sanity: the historical answer is not empty, or the equality below is vacuous: {historical:?}"
        );
        assert_eq!(
            with_context, historical,
            "a non-isolated context read must answer exactly what the context-free recall answers"
        );

        assert_eq!(
            memory
                .get_with_context(LUCID_KEY, Some(&reader))
                .await
                .unwrap()
                .map(|entry| entry.content),
            memory.get(LUCID_KEY).await.unwrap().map(|entry| entry.content),
            "and the keyed read likewise"
        );
    }

    /// N1-LUCID (R-INT2Af N1): an owner-isolated caller could store on
    /// `lucid` (the local store keeps the owner) but never forget, because
    /// `forget_with_context` fell to the trait's fail-closed default and
    /// answered `false` with the row still there. It now reaches the local
    /// store with the context, which deletes the caller's own row and nobody
    /// else's.
    #[tokio::test]
    async fn an_owner_isolated_caller_forgets_its_own_row_and_only_that() {
        let tmp = TempDir::new().unwrap();
        let memory = test_memory(tmp.path(), write_fake_lucid_script(tmp.path()));
        let owner = lucid_owner_context(LUCID_OWNER, true);
        let other = lucid_owner_context(LUCID_OTHER_OWNER, true);
        memory
            .store_with_context(LUCID_KEY, LUCID_CONTENT, MemoryCategory::Core, None, Some(&owner))
            .await
            .unwrap();
        memory
            .store_with_context(
                "second-owner-note",
                "beta note",
                MemoryCategory::Core,
                None,
                Some(&other),
            )
            .await
            .unwrap();

        assert!(
            !memory
                .forget_with_context("second-owner-note", Some(&owner))
                .await
                .unwrap(),
            "an isolated caller must not delete another owner's row"
        );
        assert!(
            memory
                .get_with_context("second-owner-note", Some(&other))
                .await
                .unwrap()
                .is_some(),
            "the other owner's row survives"
        );

        assert!(
            memory.forget_with_context(LUCID_KEY, Some(&owner)).await.unwrap(),
            "an isolated caller forgets its own row on lucid"
        );
        assert!(
            memory
                .get_with_context(LUCID_KEY, Some(&owner))
                .await
                .unwrap()
                .is_none(),
            "and the row is gone"
        );
    }
}
