use crate::memory::principal::MemoryWriteContext;
use crate::memory::traits::{
    ChatProfile, ConversationSessionSummary, ConversationTurn, MemoryCategory, MemoryEntry, MemoryPrincipal,
    MemoryReadMode, MessageEvent,
};
use crate::memory::{self, Memory};
use async_trait::async_trait;
use std::collections::HashMap;

use std::fmt::Write;

#[async_trait]
pub trait MemoryLoader: Send + Sync {
    async fn load_context(&self, memory: &dyn Memory, user_message: &str) -> anyhow::Result<String>;
}

/// Formats memory summaries already selected and scoped by the turn decision.
/// Implementations receive no backend, source credentials, or mutable decision.
/// The returned text is prompt context, never retrieval or tool authority.
pub trait SelectedMemoryFormatter: Send + Sync {
    fn format_context(&self, summaries: &[&str]) -> anyhow::Result<String>;
}

pub(crate) fn format_selected_memory(
    formatter: Option<&dyn SelectedMemoryFormatter>,
    artifacts: &[crate::causal_tree::state::ObservedArtifact],
) -> anyhow::Result<String> {
    if artifacts.is_empty() {
        return Ok(String::new());
    }
    anyhow::ensure!(
        artifacts
            .iter()
            .all(|artifact| artifact.kind == crate::causal_tree::state::ObservedArtifactKind::Memory),
        "selected memory formatter received a non-memory artifact"
    );
    if let Some(formatter) = formatter {
        let summaries: Vec<&str> = artifacts.iter().map(|artifact| artifact.summary.as_str()).collect();
        return formatter.format_context(&summaries);
    }
    let mut context = String::new();
    for artifact in artifacts {
        context.push_str(&artifact.summary);
        context.push('\n');
    }
    Ok(context)
}

/// Keep document and session context outside the caller's memory-only presentation hook.
pub(crate) fn format_selected_context(
    formatter: Option<&dyn SelectedMemoryFormatter>,
    artifacts: &[crate::causal_tree::state::ObservedArtifact],
) -> anyhow::Result<String> {
    use crate::causal_tree::state::ObservedArtifactKind;
    if artifacts
        .iter()
        .all(|artifact| artifact.kind == ObservedArtifactKind::Memory)
    {
        return format_selected_memory(formatter, artifacts);
    }
    anyhow::ensure!(
        artifacts.iter().all(|artifact| matches!(
            artifact.kind,
            ObservedArtifactKind::Memory | ObservedArtifactKind::Document | ObservedArtifactKind::Session
        )),
        "selected context contains unsupported evidence"
    );
    let mut context = String::new();
    if let Some(formatter) = formatter {
        let summaries: Vec<&str> = artifacts
            .iter()
            .filter(|artifact| artifact.kind == ObservedArtifactKind::Memory)
            .map(|artifact| artifact.summary.as_str())
            .collect();
        if !summaries.is_empty() {
            context = formatter.format_context(&summaries)?;
            if !context.is_empty() && !context.ends_with('\n') {
                context.push('\n');
            }
        }
    } else {
        for artifact in artifacts
            .iter()
            .filter(|artifact| artifact.kind == ObservedArtifactKind::Memory)
        {
            context.push_str(&artifact.summary);
            context.push('\n');
        }
    }
    for artifact in artifacts
        .iter()
        .filter(|artifact| artifact.kind != ObservedArtifactKind::Memory)
    {
        context.push_str(if artifact.kind == ObservedArtifactKind::Document {
            "[Selected document]\n"
        } else {
            "[Selected session history]\n"
        });
        context.push_str(&artifact.summary);
        context.push('\n');
    }
    Ok(context)
}

/// The legacy callback is retained, but the backend it receives has only the
/// read authority issued for this selected Memory hop. Its text is presentation
/// supplied by extension code, never evidence or an admission receipt.
pub(crate) struct LoadedSelectedMemory {
    pub(crate) hits: Vec<crate::memory::traits::DecisionMemoryHit>,
    pub(crate) context: String,
}

pub(crate) async fn load_selected_memory(
    loader: &dyn MemoryLoader,
    backend: &dyn Memory,
    input: &str,
    scope: &crate::memory::principal::MemoryWriteContext,
    session: &str,
    deadline: std::time::Instant,
) -> anyhow::Result<LoadedSelectedMemory> {
    anyhow::ensure!(
        scope.workspace_id.as_deref().is_some_and(|id| !id.trim().is_empty())
            && scope.owner_id.as_deref().is_some_and(|id| !id.trim().is_empty()),
        "selected custom retrieval requires source workspace and owner"
    );
    let view = SelectedMemoryView {
        backend,
        scope,
        session,
        deadline,
        calls: std::sync::atomic::AtomicUsize::new(0),
        reserved: std::sync::atomic::AtomicUsize::new(0),
        hits: parking_lot::Mutex::new(Vec::new()),
    };
    anyhow::ensure!(
        std::time::Instant::now() < deadline,
        "selected custom retrieval deadline expired"
    );
    let context = tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        loader.load_context(&view, input),
    )
    .await
    .map_err(|_| anyhow::anyhow!("selected custom retrieval deadline expired"))??;
    anyhow::ensure!(
        std::time::Instant::now() < deadline,
        "selected custom retrieval deadline expired"
    );
    // Same five 256-character summaries as selected Memory evidence, with a
    // separate token ceiling before the shared provider context-budget gate.
    anyhow::ensure!(
        context.len() <= 5 * 256 * 4 && context.chars().count() <= 5 * 256,
        "selected custom retrieval context exceeds bounds"
    );
    let context = crate::security::redaction::redact_secrets(&context);
    anyhow::ensure!(
        context.len() <= 5 * 256 * 4 && context.chars().count() <= 5 * 256,
        "redacted custom retrieval context exceeds bounds"
    );
    anyhow::ensure!(
        crate::agent::context_budget::count_tokens_with_source(&context).tokens <= 2048,
        "selected custom retrieval context exceeds token bounds"
    );
    anyhow::ensure!(
        std::time::Instant::now() < deadline,
        "selected custom retrieval deadline expired"
    );
    let hits = view.hits.into_inner();
    anyhow::ensure!(
        context.is_empty() || !hits.is_empty(),
        "custom retrieval context requires actual bounded Memory hits"
    );
    Ok(LoadedSelectedMemory { hits, context })
}

struct SelectedMemoryView<'a> {
    backend: &'a dyn Memory,
    scope: &'a crate::memory::principal::MemoryWriteContext,
    session: &'a str,
    deadline: std::time::Instant,
    calls: std::sync::atomic::AtomicUsize,
    reserved: std::sync::atomic::AtomicUsize,
    hits: parking_lot::Mutex<Vec<crate::memory::traits::DecisionMemoryHit>>,
}

#[async_trait]
impl Memory for SelectedMemoryView<'_> {
    fn name(&self) -> &str {
        "selected_memory_read"
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        use std::sync::atomic::Ordering;
        anyhow::ensure!(
            query.len() <= 1024 && (1..=5).contains(&limit),
            "custom retrieval query or limit exceeds bounds"
        );
        if let Some(session) = session_id {
            anyhow::ensure!(
                session == self.session,
                "custom retrieval cannot select another session"
            );
        }
        anyhow::ensure!(
            std::time::Instant::now() < self.deadline,
            "selected custom retrieval deadline expired"
        );
        self.calls
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < 5).then_some(used + 1)
            })
            .map_err(|_| anyhow::anyhow!("custom retrieval read count exceeds bounds"))?;
        self.reserved
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(limit).filter(|total| *total <= 5)
            })
            .map_err(|_| anyhow::anyhow!("custom retrieval total hit limit exceeded"))?;
        let mut hits = tokio::time::timeout_at(
            tokio::time::Instant::from_std(self.deadline),
            self.backend
                .retrieve_decision_memory(query, u8::try_from(limit)?, self.scope, session_id, self.deadline),
        )
        .await
        .map_err(|_| anyhow::anyhow!("selected custom retrieval deadline expired"))??;
        anyhow::ensure!(
            hits.len() <= limit
                && hits.iter().all(|hit| hit.id.len() <= 128
                    && hit.key.len() <= 1024
                    && hit.category.to_string().len() <= 128
                    && hit.session_id.as_ref().is_none_or(|id| id.len() <= 512)
                    && session_id.is_none_or(|session| hit.session_id.as_deref() == Some(session))
                    && hit.content.len() <= 4096
                    && hit.created_at.len() <= 64
                    && hit.source_event_id.as_ref().is_none_or(|id| id.len() <= 128)),
            "selected Memory backend exceeded its result contract"
        );
        let now = chrono::Utc::now();
        hits.retain(|hit| {
            !hit.content.trim().is_empty()
                && chrono::DateTime::parse_from_rfc3339(&hit.created_at)
                    .ok()
                    .and_then(|created| u64::try_from(now.signed_duration_since(created).num_milliseconds()).ok())
                    .is_some_and(|age| age <= crate::decision::builtin::weights::STALE_ARTIFACT_MS)
        });
        self.reserved.fetch_sub(limit - hits.len(), Ordering::AcqRel);
        let entries = hits
            .iter()
            .map(|hit| crate::memory::MemoryEntry {
                id: hit.id.clone(),
                key: hit.key.clone(),
                content: hit.content.clone(),
                category: hit.category.clone(),
                timestamp: hit.created_at.clone(),
                session_id: hit.session_id.clone(),
                score: None,
                tags: None,
                access_count: None,
                useful_count: None,
                source: None,
                source_confidence: None,
                verification_status: None,
                lifecycle_state: None,
                compressed_from: None,
            })
            .collect();
        self.hits.lock().extend(hits);
        Ok(entries)
    }

    async fn store(&self, _: &str, _: &str, _: crate::memory::MemoryCategory, _: Option<&str>) -> anyhow::Result<()> {
        anyhow::bail!("custom retrieval Memory view is read-only")
    }
    async fn get(&self, _: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
        anyhow::bail!("custom retrieval get requires migration to bounded recall")
    }
    async fn list(
        &self,
        _: Option<&crate::memory::MemoryCategory>,
        _: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        anyhow::bail!("custom retrieval list requires migration to bounded recall")
    }
    async fn forget(&self, _: &str) -> anyhow::Result<bool> {
        anyhow::bail!("custom retrieval Memory view is read-only")
    }
    async fn count(&self) -> anyhow::Result<usize> {
        anyhow::bail!("custom retrieval count is outside the selected read")
    }
    async fn recall_with_context(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let _ = (query, limit, session_id, context);
        anyhow::bail!("custom retrieval recall_with_context is outside bounded recall; migration required")
    }
    async fn recall_with_context_mode(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let _ = (query, limit, session_id, context, mode);
        anyhow::bail!("custom retrieval recall_with_context_mode is outside bounded recall; migration required")
    }
    async fn get_with_context(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        let _ = (key, context);
        anyhow::bail!("custom retrieval get_with_context is outside bounded recall; migration required")
    }
    async fn get_with_context_mode(
        &self,
        key: &str,
        context: Option<&MemoryWriteContext>,
        mode: MemoryReadMode,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        let _ = (key, context, mode);
        anyhow::bail!("custom retrieval get_with_context_mode is outside bounded recall; migration required")
    }
    async fn forget_with_context(&self, key: &str, context: Option<&MemoryWriteContext>) -> anyhow::Result<bool> {
        let _ = (key, context);
        anyhow::bail!("custom retrieval forget_with_context is outside bounded recall; migration required")
    }
    async fn list_owner_ids(
        &self,
        category: &MemoryCategory,
    ) -> anyhow::Result<Option<std::collections::HashMap<String, Option<String>>>> {
        let _ = (category,);
        anyhow::bail!("custom retrieval list_owner_ids is outside bounded recall; migration required")
    }
    async fn mirror_operator_write(&self, key: &str, content: &str, category: &MemoryCategory) -> anyhow::Result<()> {
        let _ = (key, content, category);
        anyhow::bail!("custom retrieval mirror_operator_write is outside bounded recall; migration required")
    }
    async fn increment_useful_count(&self, id: &str) -> anyhow::Result<()> {
        let _ = (id,);
        anyhow::bail!("custom retrieval increment_useful_count is outside bounded recall; migration required")
    }
    async fn reindex(&self) -> anyhow::Result<usize> {
        anyhow::bail!("custom retrieval reindex is outside bounded recall; migration required")
    }
    async fn upsert_chat_profile_metadata(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        title: Option<&str>,
    ) -> anyhow::Result<()> {
        let _ = (channel, chat_id, chat_kind, title);
        anyhow::bail!("custom retrieval upsert_chat_profile_metadata is outside bounded recall; migration required")
    }
    async fn get_chat_profile(&self, channel: &str, chat_id: &str) -> anyhow::Result<Option<ChatProfile>> {
        let _ = (channel, chat_id);
        anyhow::bail!("custom retrieval get_chat_profile is outside bounded recall; migration required")
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
        anyhow::bail!("custom retrieval append_conversation_turn is outside bounded recall; migration required")
    }
    async fn list_conversation_sessions(
        &self,
        limit: usize,
        offset: usize,
        channel: Option<&str>,
    ) -> anyhow::Result<Vec<ConversationSessionSummary>> {
        let _ = (limit, offset, channel);
        anyhow::bail!("custom retrieval list_conversation_sessions is outside bounded recall; migration required")
    }
    async fn get_conversation_session(&self, session_key: &str) -> anyhow::Result<Option<ConversationSessionSummary>> {
        let _ = (session_key,);
        anyhow::bail!("custom retrieval get_conversation_session is outside bounded recall; migration required")
    }
    async fn list_conversation_turns(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<ConversationTurn>> {
        let _ = (principal, session_key, limit, offset);
        anyhow::bail!("custom retrieval list_conversation_turns is outside bounded recall; migration required")
    }
    async fn load_recent_conversation_histories(
        &self,
        principal: &MemoryPrincipal,
        max_turns_per_session: usize,
        max_sessions: usize,
    ) -> anyhow::Result<HashMap<String, Vec<ConversationTurn>>> {
        let _ = (principal, max_turns_per_session, max_sessions);
        anyhow::bail!(
            "custom retrieval load_recent_conversation_histories is outside bounded recall; migration required"
        )
    }
    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        let _ = (workspace_id, idempotency_key);
        anyhow::bail!(
            "custom retrieval find_message_event_by_idempotency_key is outside bounded recall; migration required"
        )
    }
    fn supports_document_ingest(&self) -> bool {
        false
    }
    async fn health_check(&self) -> bool {
        std::time::Instant::now() < self.deadline
    }
}

pub struct DefaultMemoryLoader {
    limit: usize,
    min_relevance_score: f64,
}

impl Default for DefaultMemoryLoader {
    fn default() -> Self {
        Self {
            limit: 5,
            min_relevance_score: 0.4,
        }
    }
}

impl DefaultMemoryLoader {
    pub fn new(limit: usize, min_relevance_score: f64) -> Self {
        Self {
            limit: limit.max(1),
            min_relevance_score,
        }
    }
}

#[async_trait]
impl MemoryLoader for DefaultMemoryLoader {
    async fn load_context(&self, memory: &dyn Memory, user_message: &str) -> anyhow::Result<String> {
        let entries = memory.recall(user_message, self.limit, None).await?;
        if entries.is_empty() {
            return Ok(String::new());
        }

        let mut context = String::from("[Memory context]\n");
        for entry in entries {
            if memory::is_assistant_autosave_key(&entry.key) {
                continue;
            }
            if let Some(score) = entry.score {
                if score < self.min_relevance_score {
                    continue;
                }
            }
            let _ = writeln!(context, "- {}: {}", entry.key, entry.content);
        }

        // If all entries were below threshold, return empty
        if context == "[Memory context]\n" {
            return Ok(String::new());
        }

        context.push('\n');
        Ok(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Memory, MemoryCategory, MemoryEntry};
    use std::sync::Arc;

    struct SelectedFormatter(std::sync::atomic::AtomicUsize);
    impl SelectedMemoryFormatter for SelectedFormatter {
        fn format_context(&self, summaries: &[&str]) -> anyhow::Result<String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(format!("[Selected memory]\n{}\n", summaries.join("\n")))
        }
    }

    fn selected_memory_artifact(summary: &str) -> crate::causal_tree::state::ObservedArtifact {
        crate::causal_tree::state::ObservedArtifact {
            kind: crate::causal_tree::state::ObservedArtifactKind::Memory,
            age_ms: 0,
            summary: summary.to_owned(),
            summary_was_redacted: false,
            summary_was_truncated: false,
            source_event_id: None,
        }
    }

    #[test]
    fn selected_memory_formatter_runs_only_for_admitted_memory_summaries() {
        use std::sync::atomic::Ordering;
        let formatter = SelectedFormatter(std::sync::atomic::AtomicUsize::new(0));
        assert!(format_selected_memory(Some(&formatter), &[]).unwrap().is_empty());
        assert_eq!(formatter.0.load(Ordering::SeqCst), 0);
        let artifacts = [
            selected_memory_artifact("project schedule"),
            selected_memory_artifact("review window"),
        ];
        assert_eq!(
            format_selected_memory(Some(&formatter), &artifacts).unwrap(),
            "[Selected memory]\nproject schedule\nreview window\n"
        );
        assert_eq!(formatter.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            format_selected_memory(None, &artifacts).unwrap(),
            "project schedule\nreview window\n"
        );
    }

    #[test]
    fn selected_memory_formatter_rejects_other_artifact_kinds_before_callback() {
        let formatter = SelectedFormatter(std::sync::atomic::AtomicUsize::new(0));
        for kind in [
            crate::causal_tree::state::ObservedArtifactKind::Document,
            crate::causal_tree::state::ObservedArtifactKind::Session,
            crate::causal_tree::state::ObservedArtifactKind::Discovery,
        ] {
            let mut artifact = selected_memory_artifact("other selected evidence");
            artifact.kind = kind;
            let artifacts = [selected_memory_artifact("permitted memory"), artifact];
            assert!(format_selected_memory(None, &artifacts).is_err());
            assert!(format_selected_memory(Some(&formatter), &artifacts).is_err());
        }
        assert_eq!(formatter.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn selected_documents_do_not_enter_memory_formatter() {
        let formatter = SelectedFormatter(std::sync::atomic::AtomicUsize::new(0));
        let mut document = selected_memory_artifact("document evidence");
        document.kind = crate::causal_tree::state::ObservedArtifactKind::Document;
        assert_eq!(
            format_selected_context(Some(&formatter), &[document.clone()]).unwrap(),
            "[Selected document]\ndocument evidence\n"
        );
        assert_eq!(formatter.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        let mixed = [selected_memory_artifact("memory evidence"), document];
        assert_eq!(
            format_selected_context(Some(&formatter), &mixed).unwrap(),
            "[Selected memory]\nmemory evidence\n[Selected document]\ndocument evidence\n"
        );
        assert_eq!(formatter.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn selected_session_history_does_not_enter_memory_formatter() {
        let formatter = SelectedFormatter(std::sync::atomic::AtomicUsize::new(0));
        let mut artifact = selected_memory_artifact("assistant: prior response");
        artifact.kind = crate::causal_tree::state::ObservedArtifactKind::Session;
        assert_eq!(
            format_selected_context(Some(&formatter), &[artifact.clone()]).unwrap(),
            "[Selected session history]\nassistant: prior response\n"
        );
        assert_eq!(formatter.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        artifact.kind = crate::causal_tree::state::ObservedArtifactKind::Discovery;
        assert!(format_selected_context(Some(&formatter), &[artifact]).is_err());
        assert_eq!(formatter.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn default_selected_context_preserves_evidence_types_and_memory_text() {
        use crate::causal_tree::state::ObservedArtifactKind;
        let memory = selected_memory_artifact("memory fact");
        let mut document = selected_memory_artifact("document fact");
        document.kind = ObservedArtifactKind::Document;
        let mut session = selected_memory_artifact("user: prior question");
        session.kind = ObservedArtifactKind::Session;
        assert_eq!(
            format_selected_context(None, std::slice::from_ref(&memory)).unwrap(),
            "memory fact\n"
        );
        assert_eq!(
            format_selected_context(None, &[memory, document, session]).unwrap(),
            "memory fact\n[Selected document]\ndocument fact\n[Selected session history]\nuser: prior question\n"
        );
        assert!(format_selected_context(None, &[]).unwrap().is_empty());
        let mut discovery = selected_memory_artifact("tool metadata");
        discovery.kind = ObservedArtifactKind::Discovery;
        assert!(format_selected_context(None, &[discovery]).is_err());
    }

    #[test]
    fn selected_memory_formatter_failure_is_not_replaced_with_unscoped_recall() {
        struct FailingFormatter;
        impl SelectedMemoryFormatter for FailingFormatter {
            fn format_context(&self, _: &[&str]) -> anyhow::Result<String> {
                anyhow::bail!("formatter rejected selected context")
            }
        }
        let result = format_selected_memory(Some(&FailingFormatter), &[selected_memory_artifact("project schedule")]);
        assert!(matches!(result, Err(error) if error.to_string() == "formatter rejected selected context"));
    }

    struct MockMemory;
    struct MockMemoryWithEntries {
        entries: Arc<Vec<MemoryEntry>>,
    }

    #[async_trait]
    impl Memory for MockMemory {
        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            if limit == 0 {
                return Ok(vec![]);
            }
            Ok(vec![MemoryEntry {
                id: "1".into(),
                key: "k".into(),
                content: "v".into(),
                category: MemoryCategory::Conversation,
                timestamp: "now".into(),
                session_id: None,
                score: None,
                tags: None,
                access_count: None,
                useful_count: None,
                source: None,
                source_confidence: None,
                verification_status: None,
                lifecycle_state: None,
                compressed_from: None,
            }])
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    #[async_trait]
    impl Memory for MockMemoryWithEntries {
        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(self.entries.as_ref().clone())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(self.entries.len())
        }

        async fn health_check(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "mock-with-entries"
        }
    }

    #[tokio::test]
    async fn default_loader_formats_context() {
        let loader = DefaultMemoryLoader::default();
        let context = loader.load_context(&MockMemory, "hello").await.unwrap();
        assert!(context.contains("[Memory context]"));
        assert!(context.contains("- k: v"));
    }

    #[tokio::test]
    async fn default_loader_skips_legacy_assistant_autosave_entries() {
        let loader = DefaultMemoryLoader::new(5, 0.0);
        let memory = MockMemoryWithEntries {
            entries: Arc::new(vec![
                MemoryEntry {
                    id: "1".into(),
                    key: "assistant_resp_legacy".into(),
                    content: "fabricated detail".into(),
                    category: MemoryCategory::Daily,
                    timestamp: "now".into(),
                    session_id: None,
                    score: Some(0.95),
                    tags: None,
                    access_count: None,
                    useful_count: None,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                },
                MemoryEntry {
                    id: "2".into(),
                    key: "user_fact".into(),
                    content: "User prefers concise answers".into(),
                    category: MemoryCategory::Conversation,
                    timestamp: "now".into(),
                    session_id: None,
                    score: Some(0.9),
                    tags: None,
                    access_count: None,
                    useful_count: None,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                },
            ]),
        };

        let context = loader.load_context(&memory, "answer style").await.unwrap();
        assert!(context.contains("user_fact"));
        assert!(!context.contains("assistant_resp_legacy"));
        assert!(!context.contains("fabricated detail"));
    }
}
