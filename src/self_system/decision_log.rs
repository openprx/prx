use crate::memory::{Memory, MemoryCategory};
use crate::self_system::SELF_SYSTEM_SESSION_ID;
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChangeProposalLog {
    key: String,
    proposal_text: String,
    expected_outcome: String,
    logged_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChangeOutcomeLog {
    key: String,
    actual_outcome: String,
    fitness_delta: f64,
    logged_at: String,
}

/// Write a self-change proposal log entry into memory.
///
/// Key format: `self/decisions/YYYY-MM-DD/proposal_N`, category `core`.
pub async fn log_change_proposal(
    memory: &dyn Memory,
    key: &str,
    proposal_text: &str,
    expected_outcome: &str,
) -> Result<()> {
    let day = Utc::now().date_naive().to_string();
    let index = next_core_index(memory, &day, "proposal_").await?;
    let log_key = format!("self/decisions/{day}/proposal_{index}");

    let payload = ChangeProposalLog {
        key: key.to_string(),
        proposal_text: proposal_text.to_string(),
        expected_outcome: expected_outcome.to_string(),
        logged_at: Utc::now().to_rfc3339(),
    };

    memory
        .store(
            &log_key,
            &serde_json::to_string_pretty(&payload)?,
            MemoryCategory::Core,
            Some(SELF_SYSTEM_SESSION_ID),
        )
        .await
}

/// Write a self-change outcome log entry into memory.
///
/// Key format: `self/decisions/YYYY-MM-DD/outcome_N`, category `core`.
pub async fn log_change_outcome(
    memory: &dyn Memory,
    key: &str,
    actual_outcome: &str,
    fitness_delta: f64,
) -> Result<()> {
    let day = Utc::now().date_naive().to_string();
    let index = next_core_index(memory, &day, "outcome_").await?;
    let log_key = format!("self/decisions/{day}/outcome_{index}");

    let payload = ChangeOutcomeLog {
        key: key.to_string(),
        actual_outcome: actual_outcome.to_string(),
        fitness_delta,
        logged_at: Utc::now().to_rfc3339(),
    };

    memory
        .store(
            &log_key,
            &serde_json::to_string_pretty(&payload)?,
            MemoryCategory::Core,
            Some(SELF_SYSTEM_SESSION_ID),
        )
        .await
}

async fn next_core_index(memory: &dyn Memory, day: &str, prefix: &str) -> Result<usize> {
    let key_prefix = format!("self/decisions/{day}/{prefix}");
    let count = memory
        .list(Some(&MemoryCategory::Core), None)
        .await?
        .into_iter()
        .filter(|entry| {
            matches!(
                entry.session_id.as_deref(),
                Some(SELF_SYSTEM_SESSION_ID) | None
            ) && entry.key.starts_with("self/decisions/")
        })
        .filter(|entry| entry.key.starts_with(&key_prefix))
        .count();
    Ok(count + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryEntry;
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::Utc;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    struct TestMemory {
        entries: Mutex<HashMap<String, MemoryEntry>>,
    }

    impl TestMemory {
        fn new() -> Self {
            Self {
                entries: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl Memory for TestMemory {
        fn name(&self) -> &str {
            "test-memory"
        }

        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> Result<()> {
            let mut entries = self.entries.lock().await;
            entries.insert(
                key.to_string(),
                MemoryEntry {
                    id: key.to_string(),
                    key: key.to_string(),
                    content: content.to_string(),
                    category,
                    timestamp: Utc::now().to_rfc3339(),
                    session_id: session_id.map(str::to_string),
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: None,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                },
            );
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
            let entries = self.entries.lock().await;
            Ok(entries.get(key).cloned())
        }

        async fn list(
            &self,
            category: Option<&MemoryCategory>,
            session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            let entries = self.entries.lock().await;
            Ok(entries
                .values()
                .filter(|entry| category.is_none_or(|c| &entry.category == c))
                .filter(|entry| {
                    session_id.is_none_or(|sid| entry.session_id.as_deref() == Some(sid))
                })
                .cloned()
                .collect())
        }

        async fn forget(&self, key: &str) -> Result<bool> {
            let mut entries = self.entries.lock().await;
            Ok(entries.remove(key).is_some())
        }

        async fn count(&self) -> Result<usize> {
            let entries = self.entries.lock().await;
            Ok(entries.len())
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn proposal_log_uses_core_category_and_incrementing_index() {
        let memory = TestMemory::new();
        log_change_proposal(
            &memory,
            "policy.rate_limit",
            "raise to 200",
            "fewer retries",
        )
        .await
        .unwrap();
        log_change_proposal(
            &memory,
            "policy.timeout",
            "set to 8s",
            "reduce hanging calls",
        )
        .await
        .unwrap();

        let day = Utc::now().date_naive().to_string();
        let key1 = format!("self/decisions/{day}/proposal_1");
        let key2 = format!("self/decisions/{day}/proposal_2");

        let entry1 = memory.get(&key1).await.unwrap().unwrap();
        let entry2 = memory.get(&key2).await.unwrap().unwrap();

        assert_eq!(entry1.category, MemoryCategory::Core);
        assert_eq!(entry2.category, MemoryCategory::Core);
        assert!(entry1.content.contains("policy.rate_limit"));
        assert!(entry2.content.contains("policy.timeout"));
    }

    #[tokio::test]
    async fn outcome_log_uses_separate_core_index_space() {
        let memory = TestMemory::new();
        log_change_proposal(&memory, "k1", "proposal", "expect")
            .await
            .unwrap();
        log_change_outcome(&memory, "k1", "actual", 0.12)
            .await
            .unwrap();
        log_change_outcome(&memory, "k2", "actual-2", -0.05)
            .await
            .unwrap();

        let day = Utc::now().date_naive().to_string();
        let outcome1 = format!("self/decisions/{day}/outcome_1");
        let outcome2 = format!("self/decisions/{day}/outcome_2");

        assert!(memory.get(&outcome1).await.unwrap().is_some());
        assert!(memory.get(&outcome2).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn proposal_log_counts_legacy_entries_without_session_id() {
        let memory = TestMemory::new();
        let day = Utc::now().date_naive().to_string();
        memory
            .store(
                &format!("self/decisions/{day}/proposal_1"),
                "legacy",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();

        log_change_proposal(&memory, "policy.window", "raise to 50", "match new default")
            .await
            .unwrap();

        let new_key = format!("self/decisions/{day}/proposal_2");
        assert!(memory.get(&new_key).await.unwrap().is_some());
    }

    struct SessionIgnoringMemory {
        entries: Mutex<HashMap<String, MemoryEntry>>,
    }

    impl SessionIgnoringMemory {
        fn new() -> Self {
            Self {
                entries: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl Memory for SessionIgnoringMemory {
        fn name(&self) -> &str {
            "session-ignoring-memory"
        }

        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> Result<()> {
            let mut entries = self.entries.lock().await;
            entries.insert(
                key.to_string(),
                MemoryEntry {
                    id: key.to_string(),
                    key: key.to_string(),
                    content: content.to_string(),
                    category,
                    timestamp: Utc::now().to_rfc3339(),
                    session_id: session_id.map(str::to_string),
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: None,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                },
            );
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
            let entries = self.entries.lock().await;
            Ok(entries.get(key).cloned())
        }

        async fn list(
            &self,
            category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            let entries = self.entries.lock().await;
            Ok(entries
                .values()
                .filter(|entry| category.is_none_or(|c| &entry.category == c))
                .cloned()
                .collect())
        }

        async fn forget(&self, key: &str) -> Result<bool> {
            let mut entries = self.entries.lock().await;
            Ok(entries.remove(key).is_some())
        }

        async fn count(&self) -> Result<usize> {
            let entries = self.entries.lock().await;
            Ok(entries.len())
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn proposal_log_does_not_double_count_when_backend_ignores_session_filter() {
        let memory = SessionIgnoringMemory::new();
        let day = Utc::now().date_naive().to_string();
        memory
            .store(
                &format!("self/decisions/{day}/proposal_1"),
                "legacy",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();

        log_change_proposal(&memory, "policy.window", "raise to 50", "match new default")
            .await
            .unwrap();

        assert!(memory
            .get(&format!("self/decisions/{day}/proposal_2"))
            .await
            .unwrap()
            .is_some());
        assert!(memory
            .get(&format!("self/decisions/{day}/proposal_3"))
            .await
            .unwrap()
            .is_none());
    }
}
