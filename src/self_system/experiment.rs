use crate::memory::{Memory, MemoryCategory};
use crate::self_system::SELF_SYSTEM_SESSION_ID;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus {
    Running,
    Succeeded,
    Failed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentRecord {
    pub id: String,
    pub name: String,
    pub baseline_fitness: f64,
    pub change_description: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub outcome_fitness: Option<f64>,
    pub rollback_reason: Option<String>,
    pub status: ExperimentStatus,
}

/// Start a new experiment and persist it in memory.
///
/// Key format: `self/experiments/ID`, category `core`.
pub async fn start_experiment(
    memory: &dyn Memory,
    name: &str,
    baseline_fitness: f64,
    change_desc: &str,
) -> Result<ExperimentRecord> {
    let record = ExperimentRecord {
        id: Uuid::new_v4().to_string(),
        name: name.to_string(),
        baseline_fitness,
        change_description: change_desc.to_string(),
        started_at: Utc::now().to_rfc3339(),
        ended_at: None,
        outcome_fitness: None,
        rollback_reason: None,
        status: ExperimentStatus::Running,
    };

    persist_experiment(memory, &record).await?;
    Ok(record)
}

/// Complete a running experiment and persist final outcome.
///
/// Status is marked `succeeded` if `outcome_fitness >= baseline_fitness`,
/// otherwise `failed`.
pub async fn complete_experiment(memory: &dyn Memory, id: &str, outcome_fitness: f64) -> Result<ExperimentRecord> {
    let mut record = load_experiment(memory, id).await?;
    if record.status != ExperimentStatus::Running {
        bail!("experiment {id} is not running");
    }

    record.outcome_fitness = Some(outcome_fitness);
    record.ended_at = Some(Utc::now().to_rfc3339());
    record.status = if outcome_fitness >= record.baseline_fitness {
        ExperimentStatus::Succeeded
    } else {
        ExperimentStatus::Failed
    };

    persist_experiment(memory, &record).await?;
    Ok(record)
}

/// Roll back a running experiment with a reason and persist it.
pub async fn rollback_experiment(memory: &dyn Memory, id: &str, reason: &str) -> Result<ExperimentRecord> {
    let mut record = load_experiment(memory, id).await?;
    if record.status != ExperimentStatus::Running {
        bail!("experiment {id} is not running");
    }

    record.rollback_reason = Some(reason.to_string());
    record.ended_at = Some(Utc::now().to_rfc3339());
    record.status = ExperimentStatus::RolledBack;

    persist_experiment(memory, &record).await?;
    Ok(record)
}

fn experiment_key(id: &str) -> String {
    format!("self/experiments/{id}")
}

async fn persist_experiment(memory: &dyn Memory, record: &ExperimentRecord) -> Result<()> {
    memory
        .store(
            &experiment_key(&record.id),
            &serde_json::to_string_pretty(record)?,
            MemoryCategory::Core,
            Some(SELF_SYSTEM_SESSION_ID),
        )
        .await
}

async fn load_experiment(memory: &dyn Memory, id: &str) -> Result<ExperimentRecord> {
    let key = experiment_key(id);
    let entry = memory
        .get(&key)
        .await?
        .ok_or_else(|| anyhow!("experiment not found: {id}"))?;

    serde_json::from_str::<ExperimentRecord>(&entry.content)
        .with_context(|| format!("failed to parse experiment record at key {key}"))
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

        async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
            let entries = self.entries.lock().await;
            Ok(entries.get(key).cloned())
        }

        async fn list(&self, category: Option<&MemoryCategory>, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
            let entries = self.entries.lock().await;
            Ok(entries
                .values()
                .filter(|entry| category.map_or(true, |c| &entry.category == c))
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
    async fn start_experiment_persists_running_record() {
        let memory = TestMemory::new();
        let record = start_experiment(&memory, "tool-timeout-test", 0.62, "increase timeout")
            .await
            .unwrap();

        assert_eq!(record.status, ExperimentStatus::Running);
        assert!(record.ended_at.is_none());

        let key = format!("self/experiments/{}", record.id);
        let saved = memory.get(&key).await.unwrap().unwrap();
        assert_eq!(saved.category, MemoryCategory::Core);
        assert!(saved.content.contains("tool-timeout-test"));
    }

    #[tokio::test]
    async fn complete_experiment_marks_success_or_failure() {
        let memory = TestMemory::new();
        let record = start_experiment(&memory, "exp-complete", 0.8, "small change")
            .await
            .unwrap();

        let completed = complete_experiment(&memory, &record.id, 0.75).await.unwrap();
        assert_eq!(completed.status, ExperimentStatus::Failed);
        assert_eq!(completed.outcome_fitness, Some(0.75));
        assert!(completed.ended_at.is_some());
    }

    #[tokio::test]
    async fn rollback_experiment_persists_reason_and_status() {
        let memory = TestMemory::new();
        let record = start_experiment(&memory, "exp-rollback", 0.5, "risky mutation")
            .await
            .unwrap();

        let rolled_back = rollback_experiment(&memory, &record.id, "latency regression")
            .await
            .unwrap();
        assert_eq!(rolled_back.status, ExperimentStatus::RolledBack);
        assert_eq!(rolled_back.rollback_reason.as_deref(), Some("latency regression"));
        assert!(rolled_back.ended_at.is_some());
    }
}
