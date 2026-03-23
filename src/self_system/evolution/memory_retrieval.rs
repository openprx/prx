use crate::memory::{LifecycleState, Memory, MemoryCategory, MemoryEntry};
use crate::self_system::SELF_SYSTEM_SESSION_ID;
use crate::self_system::evolution::safety_utils::{is_raw_debug_enabled, sha256_hex};
use crate::self_system::evolution::{
    Actor, AsyncJsonlWriter, EvolutionConfig, MemoryAccessLog, MemoryAction, SharedEvolutionConfig, TaskType,
    current_trace,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::Arc;

/// Evolution-aware memory retrieval pipeline.
#[async_trait]
pub trait EvolutionAwareRetrieval: Send + Sync {
    /// Retrieve memory entries for a query under a token budget.
    async fn retrieve(&self, query: &str, token_budget: usize) -> Result<Vec<MemoryEntry>>;
}

/// Default implementation of [`EvolutionAwareRetrieval`].
pub struct EvolutionMemoryRetriever {
    memory: Arc<dyn Memory>,
    config: SharedEvolutionConfig,
    writer: Option<Arc<AsyncJsonlWriter>>,
}

impl EvolutionMemoryRetriever {
    pub fn new(memory: Arc<dyn Memory>, config: SharedEvolutionConfig, writer: Option<Arc<AsyncJsonlWriter>>) -> Self {
        Self { memory, config, writer }
    }

    async fn base_retrieval(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        let query_terms: Vec<String> = query
            .to_ascii_lowercase()
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .map(str::to_string)
            .collect();
        let entries = self.memory.list(None, Some(SELF_SYSTEM_SESSION_ID)).await?;

        Ok(entries
            .into_iter()
            .filter(is_self_system_owned)
            .filter(is_active)
            .filter(|entry| keyword_or_tag_match(entry, &query_terms))
            .collect())
    }

    async fn maybe_vector_retrieve(&self, query: &str, threshold: usize) -> Result<Vec<MemoryEntry>> {
        let count = self.memory.count().await?;
        if count > threshold {
            // Reserved for vector retrieval integration.
            tracing::debug!(
                query_hash = %crate::self_system::evolution::safety_utils::sha256_hex(query),
                query_len = query.len(),
                memory_count = count,
                threshold,
                "vector retrieval reserved hook reached"
            );
        }
        Ok(Vec::new())
    }

    fn score_entry(entry: &MemoryEntry, config: &EvolutionConfig) -> f64 {
        let weights = &config.retrieval.score_weights;
        let recency = recency_score(&entry.timestamp);
        let access_freq = access_frequency_score(entry.access_count);
        let category_weight = category_weight_score(&entry.category);
        let useful_ratio = useful_ratio_score(entry.useful_count, entry.access_count);
        let source_confidence = entry.source_confidence.unwrap_or(0.5).clamp(0.0, 1.0);

        weights.source_confidence.mul_add(
            source_confidence,
            weights.useful_ratio.mul_add(
                useful_ratio,
                weights.category_weight.mul_add(
                    category_weight,
                    weights.recency.mul_add(recency, weights.access_freq * access_freq),
                ),
            ),
        )
    }

    async fn write_access_logs(&self, query: &str, selected: &[MemoryEntry], token_budget: usize) -> Result<()> {
        let Some(writer) = &self.writer else {
            return Ok(());
        };

        let trace = current_trace().unwrap_or_default();
        for entry in selected {
            let consumed = estimate_tokens(&entry.content) as u32;
            let log = MemoryAccessLog {
                timestamp: Utc::now().to_rfc3339(),
                experiment_id: trace.experiment_id.clone(),
                trace_id: trace.trace_id.clone(),
                action: MemoryAction::Search,
                memory_id: entry.id.clone(),
                task_context: format_task_context(query, token_budget),
                task_type: TaskType::Planning,
                actor: Actor::Agent,
                was_useful: None,
                useful_annotation_source: None,
                annotation_confidence: None,
                tokens_consumed: consumed,
            };
            writer.append_memory_access(&log).await?;
        }
        Ok(())
    }
}

fn format_task_context(query: &str, token_budget: usize) -> String {
    if is_raw_debug_enabled() {
        return format!("query={query};budget={token_budget}");
    }
    format!(
        "query_sha256={};query_len={};budget={token_budget}",
        sha256_hex(query),
        query.len()
    )
}

#[async_trait]
impl EvolutionAwareRetrieval for EvolutionMemoryRetriever {
    async fn retrieve(&self, query: &str, token_budget: usize) -> Result<Vec<MemoryEntry>> {
        let config = self.config.load_full();

        let mut candidates = self.base_retrieval(query).await?;
        let vector_candidates = self
            .maybe_vector_retrieve(query, config.retrieval.vector_retrieval_threshold)
            .await?;
        candidates.extend(vector_candidates);

        candidates.sort_by(|a, b| {
            let a_score = Self::score_entry(a, &config);
            let b_score = Self::score_entry(b, &config);
            b_score.total_cmp(&a_score)
        });

        let mut selected = Vec::new();
        let mut spent = 0usize;
        for entry in candidates {
            let entry_tokens = estimate_tokens(&entry.content);
            if spent + entry_tokens > token_budget {
                continue;
            }
            spent += entry_tokens;
            selected.push(entry);
        }

        self.write_access_logs(query, &selected, token_budget).await?;
        Ok(selected)
    }
}

const fn is_active(entry: &MemoryEntry) -> bool {
    match entry.lifecycle_state {
        Some(LifecycleState::Active) | None => true,
        Some(LifecycleState::Archived | LifecycleState::Tombstoned) => false,
    }
}

fn is_self_system_owned(entry: &MemoryEntry) -> bool {
    entry.session_id.as_deref() == Some(SELF_SYSTEM_SESSION_ID) && entry.key.starts_with("self/")
}

fn keyword_or_tag_match(entry: &MemoryEntry, query_terms: &[String]) -> bool {
    if query_terms.is_empty() {
        return true;
    }

    let key_lc = entry.key.to_ascii_lowercase();
    let content_lc = entry.content.to_ascii_lowercase();
    let tags_lc = entry
        .tags
        .as_ref()
        .map(|tags| tags.iter().map(|v| v.to_ascii_lowercase()).collect::<Vec<String>>());

    query_terms.iter().any(|term| {
        key_lc.contains(term)
            || content_lc.contains(term)
            || tags_lc
                .as_ref()
                .is_some_and(|tags| tags.iter().any(|tag| tag.contains(term)))
    })
}

fn recency_score(timestamp: &str) -> f64 {
    let Ok(dt) = DateTime::parse_from_rfc3339(timestamp) else {
        return 0.0;
    };
    let age_days = (Utc::now() - dt.with_timezone(&Utc)).num_days().max(0) as f64;
    1.0 / (1.0 + age_days)
}

fn access_frequency_score(access_count: Option<u32>) -> f64 {
    let count = f64::from(access_count.unwrap_or(0));
    (count / 20.0).clamp(0.0, 1.0)
}

fn useful_ratio_score(useful_count: Option<u32>, access_count: Option<u32>) -> f64 {
    let access = access_count.unwrap_or(0);
    if access == 0 {
        return 0.0;
    }
    let useful = useful_count.unwrap_or(0);
    (f64::from(useful) / f64::from(access)).clamp(0.0, 1.0)
}

const fn category_weight_score(category: &MemoryCategory) -> f64 {
    match category {
        MemoryCategory::Core => 1.0,
        MemoryCategory::Conversation => 0.9,
        MemoryCategory::Daily => 0.75,
        MemoryCategory::Custom(_) => 0.8,
    }
}

fn estimate_tokens(content: &str) -> usize {
    let chars = content.chars().count().max(1);
    chars.div_ceil(4)
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::storage::{AsyncJsonlWriter, JsonlRetentionPolicy, JsonlStoragePaths};
    use crate::self_system::evolution::{EvolutionConfig, new_shared_evolution_config};
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{Duration, Utc};
    use std::sync::Arc;
    use tempfile::tempdir;

    struct MockMemory {
        entries: Vec<MemoryEntry>,
    }

    #[async_trait]
    impl Memory for MockMemory {
        fn name(&self) -> &str {
            "mock-memory"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }

        async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            Ok(self.entries.clone())
        }

        async fn forget(&self, _key: &str) -> Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> Result<usize> {
            Ok(self.entries.len())
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    fn build_entry(
        id: &str,
        content: &str,
        session_id: Option<&str>,
        lifecycle_state: Option<LifecycleState>,
        access_count: Option<u32>,
        useful_count: Option<u32>,
        source_confidence: Option<f64>,
    ) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            key: id.into(),
            content: content.into(),
            category: MemoryCategory::Core,
            timestamp: (Utc::now() - Duration::hours(1)).to_rfc3339(),
            session_id: session_id.map(str::to_string),
            score: None,
            tags: Some(vec!["alpha".into()]),
            access_count,
            useful_count,
            source: None,
            source_confidence,
            verification_status: None,
            lifecycle_state,
            compressed_from: None,
        }
    }

    #[tokio::test]
    async fn retrieval_filters_non_active_entries() {
        let memory = Arc::new(MockMemory {
            entries: vec![
                build_entry(
                    "self/active",
                    "alpha fact",
                    Some(SELF_SYSTEM_SESSION_ID),
                    Some(LifecycleState::Active),
                    Some(4),
                    Some(2),
                    Some(0.8),
                ),
                build_entry(
                    "self/archived",
                    "alpha archived fact",
                    Some(SELF_SYSTEM_SESSION_ID),
                    Some(LifecycleState::Archived),
                    Some(100),
                    Some(100),
                    Some(1.0),
                ),
            ],
        });
        let retriever =
            EvolutionMemoryRetriever::new(memory, new_shared_evolution_config(EvolutionConfig::default()), None);

        let result = retriever.retrieve("alpha", 128).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "self/active");
    }

    #[tokio::test]
    async fn retrieval_respects_token_budget_and_sorting() {
        let memory = Arc::new(MockMemory {
            entries: vec![
                build_entry(
                    "self/high-score",
                    "alpha short",
                    Some(SELF_SYSTEM_SESSION_ID),
                    Some(LifecycleState::Active),
                    Some(10),
                    Some(9),
                    Some(0.9),
                ),
                build_entry(
                    "self/low-score",
                    "alpha very long long long long long long long long long long",
                    Some(SELF_SYSTEM_SESSION_ID),
                    Some(LifecycleState::Active),
                    Some(1),
                    Some(0),
                    Some(0.1),
                ),
            ],
        });
        let retriever =
            EvolutionMemoryRetriever::new(memory, new_shared_evolution_config(EvolutionConfig::default()), None);

        let result = retriever.retrieve("alpha", 6).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "self/high-score");
    }

    #[tokio::test]
    async fn retrieval_logs_hashed_query_context_by_default() {
        let dir = tempdir().unwrap();
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(dir.path().join("logs")),
                JsonlRetentionPolicy::default(),
                1,
            )
            .await
            .unwrap(),
        );
        let memory = Arc::new(MockMemory {
            entries: vec![build_entry(
                "self/active",
                "alpha fact",
                Some(SELF_SYSTEM_SESSION_ID),
                Some(LifecycleState::Active),
                Some(1),
                Some(1),
                Some(0.9),
            )],
        });
        let retriever = EvolutionMemoryRetriever::new(
            memory,
            new_shared_evolution_config(EvolutionConfig::default()),
            Some(writer.clone()),
        );

        let _ = retriever.retrieve("alpha api_key=secret", 128).await.unwrap();
        writer.flush().await.unwrap();
        let logs = writer
            .read_memory_access_since(Utc::now() - Duration::hours(1))
            .await
            .unwrap();
        assert!(!logs.is_empty());
        assert!(logs[0].task_context.contains("query_sha256="));
        assert!(!logs[0].task_context.contains("api_key=secret"));
    }

    #[tokio::test]
    async fn retrieval_scopes_to_self_system_session_and_prefix() {
        let memory = Arc::new(MockMemory {
            entries: vec![
                build_entry(
                    "self/allowed",
                    "alpha retained",
                    Some(SELF_SYSTEM_SESSION_ID),
                    Some(LifecycleState::Active),
                    Some(2),
                    Some(1),
                    Some(0.9),
                ),
                build_entry(
                    "user/private",
                    "alpha leaked",
                    Some(SELF_SYSTEM_SESSION_ID),
                    Some(LifecycleState::Active),
                    Some(10),
                    Some(10),
                    Some(1.0),
                ),
                build_entry(
                    "self/wrong-session",
                    "alpha wrong session",
                    Some("other-session"),
                    Some(LifecycleState::Active),
                    Some(10),
                    Some(10),
                    Some(1.0),
                ),
            ],
        });
        let retriever =
            EvolutionMemoryRetriever::new(memory, new_shared_evolution_config(EvolutionConfig::default()), None);

        let result = retriever.retrieve("alpha", 128).await.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].key, "self/allowed");
    }
}
