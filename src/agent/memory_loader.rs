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

/// Formats authenticated semantic memory context supplied by the runtime.
/// Implementations receive no backend, source credentials, or mutable decision.
/// The returned text is prompt context, never retrieval or tool authority.
pub trait SelectedMemoryFormatter: Send + Sync {
    fn format_context(&self, summaries: &[&str]) -> anyhow::Result<String>;
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
    exclusion: (Option<&str>, Option<&str>),
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
        excluded_key: exclusion.0,
        excluded_event: exclusion.1,
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
        crate::tokens::count::count_tokens_with_source(&context).tokens <= 2048,
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
    excluded_key: Option<&'a str>,
    excluded_event: Option<&'a str>,
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
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < 5).then_some(used + 1)
            })
            .map_err(|_| anyhow::anyhow!("custom retrieval read count exceeds bounds"))?;
        self.reserved
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
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
            self.excluded_key != Some(hit.key.as_str())
                && (self.excluded_event.is_none() || hit.source_event_id.as_deref() != self.excluded_event)
                && !hit.content.trim().is_empty()
                && chrono::DateTime::parse_from_rfc3339(&hit.created_at)
                    .ok()
                    .and_then(|created| u64::try_from(now.signed_duration_since(created).num_milliseconds()).ok())
                    .is_some_and(|age| age <= crate::tools::capability::turn_state::STALE_ARTIFACT_MS)
        });
        self.reserved.fetch_sub(limit - hits.len(), Ordering::AcqRel);
        let entries = hits
            .iter()
            .map(|hit| crate::memory::MemoryEntry {
                origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                source_event_id: hit.source_event_id.clone(),
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
        agent_partition: Option<&crate::memory::AgentPartition>,
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
                origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                source_event_id: None,
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
                    origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                    source_event_id: None,
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
                    origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                    source_event_id: None,
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
