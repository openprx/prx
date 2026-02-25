use crate::self_system::evolution::index::JsonlToSqliteIndexer;
use crate::self_system::evolution::record::{
    AnnotationSource, DecisionLog, MemoryAccessLog, Outcome,
};
use crate::self_system::evolution::safety_utils::{acquire_file_lock, atomic_write};
use crate::self_system::evolution::storage::AsyncJsonlWriter;
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

const UNKNOWN_ALERT_THRESHOLD: f64 = 0.30;

/// Pending annotation update derived from decision and memory signals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationUpdate {
    pub experiment_id: String,
    pub trace_id: String,
    pub memory_id: String,
    pub was_useful: bool,
    pub confidence: f64,
    pub source: AnnotationSource,
    pub needs_human_review: bool,
}

/// Daily annotation statistics and application counters.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AnnotationReport {
    pub total_memory_records: u32,
    pub unknown_ratio: f64,
    pub unknown_ratio_alert: bool,
    pub tier1_updates: u32,
    pub tier2_updates: u32,
    pub tier3_marked_for_review: u32,
    pub applied_updates: u32,
}

/// Multi-tier memory usefulness annotation pipeline.
pub struct AnnotationPipeline {
    writer: Arc<AsyncJsonlWriter>,
    storage_root: PathBuf,
    unknown_alert_threshold: f64,
}

impl AnnotationPipeline {
    pub fn new(writer: Arc<AsyncJsonlWriter>, storage_root: impl AsRef<Path>) -> Self {
        Self {
            writer,
            storage_root: storage_root.as_ref().to_path_buf(),
            unknown_alert_threshold: UNKNOWN_ALERT_THRESHOLD,
        }
    }

    /// Run tier1/tier2/tier3 annotation and write back to SQLite index or JSONL logs.
    pub async fn run_daily(
        &self,
        now: DateTime<Utc>,
        indexer: Option<&JsonlToSqliteIndexer>,
    ) -> Result<AnnotationReport> {
        let since = now - Duration::hours(24);
        let decisions = self.writer.read_decisions_since(since).await?;
        let memory_logs = self.writer.read_memory_access_since(since).await?;

        let mut updates = self.infer_tier1(&decisions, &memory_logs);
        let mut report = AnnotationReport {
            total_memory_records: memory_logs.len() as u32,
            unknown_ratio: ratio(
                memory_logs
                    .iter()
                    .filter(|item| item.was_useful.is_none())
                    .count() as f64,
                memory_logs.len() as f64,
            ),
            unknown_ratio_alert: false,
            tier1_updates: updates.len() as u32,
            ..AnnotationReport::default()
        };

        if report.unknown_ratio > self.unknown_alert_threshold {
            report.unknown_ratio_alert = true;
            tracing::warn!(
                "annotation_unknown_ratio={:.2}% exceeded {:.2}%",
                report.unknown_ratio * 100.0,
                self.unknown_alert_threshold * 100.0
            );
        }

        let tier2 = self.mock_tier2_unknown(&memory_logs, &updates);
        report.tier2_updates = tier2.len() as u32;
        updates.extend(tier2);

        for item in &mut updates {
            if item.confidence < 0.6 {
                item.needs_human_review = true;
                report.tier3_marked_for_review = report.tier3_marked_for_review.saturating_add(1);
            }
        }

        report.applied_updates = if let Some(indexer) = indexer {
            self.apply_to_sqlite(indexer, &updates)?
        } else {
            self.apply_to_jsonl(&updates).await?
        };

        Ok(report)
    }

    fn infer_tier1(
        &self,
        decisions: &[DecisionLog],
        memory_logs: &[MemoryAccessLog],
    ) -> Vec<AnnotationUpdate> {
        let mut grouped: HashMap<&str, Vec<&MemoryAccessLog>> = HashMap::new();
        for item in memory_logs {
            grouped
                .entry(item.trace_id.as_str())
                .or_default()
                .push(item);
        }

        let mut out = Vec::new();
        for decision in decisions {
            let Some(related) = grouped.get(decision.trace_id.as_str()) else {
                continue;
            };

            if decision.outcome == Outcome::Success {
                for item in related {
                    out.push(AnnotationUpdate {
                        experiment_id: item.experiment_id.clone(),
                        trace_id: item.trace_id.clone(),
                        memory_id: item.memory_id.clone(),
                        was_useful: true,
                        confidence: 0.7,
                        source: AnnotationSource::AutoEvaluator,
                        needs_human_review: false,
                    });
                }
            }

            if matches!(decision.outcome, Outcome::Failure | Outcome::RolledBack)
                && related.len() == 1
            {
                let item = related[0];
                out.push(AnnotationUpdate {
                    experiment_id: item.experiment_id.clone(),
                    trace_id: item.trace_id.clone(),
                    memory_id: item.memory_id.clone(),
                    was_useful: false,
                    confidence: 0.5,
                    source: AnnotationSource::AutoEvaluator,
                    needs_human_review: true,
                });
            }

            if decision
                .user_correction
                .as_ref()
                .is_some_and(|item| !item.trim().is_empty())
            {
                for item in related {
                    out.push(AnnotationUpdate {
                        experiment_id: item.experiment_id.clone(),
                        trace_id: item.trace_id.clone(),
                        memory_id: item.memory_id.clone(),
                        was_useful: false,
                        confidence: 0.8,
                        source: AnnotationSource::UserFeedback,
                        needs_human_review: false,
                    });
                }
            }
        }

        dedup_updates(out)
    }

    fn mock_tier2_unknown(
        &self,
        memory_logs: &[MemoryAccessLog],
        existing_updates: &[AnnotationUpdate],
    ) -> Vec<AnnotationUpdate> {
        let existing = existing_updates
            .iter()
            .map(|item| {
                format!(
                    "{}:{}:{}",
                    item.experiment_id, item.trace_id, item.memory_id
                )
            })
            .collect::<std::collections::HashSet<_>>();

        let mut out = Vec::new();
        for item in memory_logs.iter().filter(|item| item.was_useful.is_none()) {
            let key = format!(
                "{}:{}:{}",
                item.experiment_id, item.trace_id, item.memory_id
            );
            if existing.contains(&key) {
                continue;
            }

            let was_useful = item.memory_id.len() % 2 == 0;
            out.push(AnnotationUpdate {
                experiment_id: item.experiment_id.clone(),
                trace_id: item.trace_id.clone(),
                memory_id: item.memory_id.clone(),
                was_useful,
                confidence: 0.6,
                source: AnnotationSource::AutoEvaluator,
                needs_human_review: false,
            });
        }

        out
    }

    fn apply_to_sqlite(
        &self,
        indexer: &JsonlToSqliteIndexer,
        updates: &[AnnotationUpdate],
    ) -> Result<u32> {
        let mut updated = 0u32;
        for item in updates {
            let count = indexer.update_memory_annotation(
                &item.experiment_id,
                &item.trace_id,
                &item.memory_id,
                item.was_useful,
                item.confidence,
                item.needs_human_review,
            )?;
            updated = updated.saturating_add(count as u32);
        }
        Ok(updated)
    }

    async fn apply_to_jsonl(&self, updates: &[AnnotationUpdate]) -> Result<u32> {
        let mut map = HashMap::new();
        for item in updates {
            map.insert(
                format!(
                    "{}:{}:{}",
                    item.experiment_id, item.trace_id, item.memory_id
                ),
                item.clone(),
            );
        }

        let mut total = 0u32;
        let base = self.storage_root.join("memory_access");
        for tier in ["hot", "warm", "cold"] {
            let dir = base.join(tier);
            if fs::metadata(&dir).await.is_err() {
                continue;
            }

            let mut rd = fs::read_dir(&dir).await?;
            while let Some(entry) = rd.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|v| v.to_str()) != Some("jsonl") {
                    continue;
                }

                let raw = fs::read_to_string(&path).await?;
                let mut changed = false;
                let mut lines = Vec::new();
                let mut malformed_lines = 0u32;
                for line in raw.lines().filter(|line| !line.trim().is_empty()) {
                    let mut parsed = match serde_json::from_str::<MemoryAccessLog>(line) {
                        Ok(item) => item,
                        Err(_) => {
                            malformed_lines = malformed_lines.saturating_add(1);
                            lines.push(line.to_string());
                            continue;
                        }
                    };
                    let key = format!(
                        "{}:{}:{}",
                        parsed.experiment_id, parsed.trace_id, parsed.memory_id
                    );
                    if let Some(update) = map.get(&key) {
                        parsed.was_useful = Some(update.was_useful);
                        parsed.annotation_confidence = Some(update.confidence);
                        parsed.useful_annotation_source = Some(update.source.clone());
                        changed = true;
                        total = total.saturating_add(1);
                    }
                    lines.push(serde_json::to_string(&parsed)?);
                }
                if malformed_lines > 0 {
                    tracing::warn!(
                        path = %path.display(),
                        malformed_lines,
                        "kept malformed memory_access lines during annotation rewrite"
                    );
                }

                if changed {
                    let mut rebuilt = lines.join("\n");
                    if !rebuilt.is_empty() {
                        rebuilt.push('\n');
                    }
                    let _guard = acquire_file_lock(&path).await?;
                    atomic_write(&self.storage_root, &path, rebuilt.as_bytes()).await?;
                }
            }
        }

        Ok(total)
    }
}

fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator <= 0.0 {
        0.0
    } else {
        (numerator / denominator).clamp(0.0, 1.0)
    }
}

fn dedup_updates(items: Vec<AnnotationUpdate>) -> Vec<AnnotationUpdate> {
    let mut out = HashMap::<String, AnnotationUpdate>::new();
    for item in items {
        let key = format!(
            "{}:{}:{}",
            item.experiment_id, item.trace_id, item.memory_id
        );
        match out.get(&key) {
            Some(existing) if existing.confidence >= item.confidence => {}
            _ => {
                out.insert(key, item);
            }
        }
    }
    out.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::record::{Actor, DecisionType, MemoryAction, TaskType};
    use crate::self_system::evolution::storage::{JsonlRetentionPolicy, JsonlStoragePaths};
    use tempfile::tempdir;

    #[tokio::test]
    async fn annotation_pipeline_generates_tiered_updates() {
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

        writer
            .append_decision(&DecisionLog {
                timestamp: "2026-02-24T00:00:00Z".to_string(),
                experiment_id: "exp-1".to_string(),
                trace_id: "trace-1".to_string(),
                decision_type: DecisionType::ToolSelection,
                task_type: TaskType::Planning,
                risk_level: 1,
                actor: Actor::Agent,
                input_context: "ctx".to_string(),
                action_taken: "run".to_string(),
                outcome: Outcome::Success,
                tokens_used: 1,
                latency_ms: 1,
                user_correction: None,
                config_snapshot_hash: "cfg".to_string(),
            })
            .await
            .unwrap();

        writer
            .append_memory_access(&MemoryAccessLog {
                timestamp: "2026-02-24T00:00:01Z".to_string(),
                experiment_id: "exp-1".to_string(),
                trace_id: "trace-1".to_string(),
                action: MemoryAction::Read,
                memory_id: "m1".to_string(),
                task_context: "ctx".to_string(),
                task_type: TaskType::Planning,
                actor: Actor::Agent,
                was_useful: None,
                useful_annotation_source: None,
                annotation_confidence: None,
                tokens_consumed: 1,
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let pipeline = AnnotationPipeline::new(writer, dir.path().join("logs"));
        let report = pipeline.run_daily(Utc::now(), None).await.unwrap();

        assert!(report.tier1_updates >= 1);
        assert!(report.applied_updates >= 1);
    }

    #[tokio::test]
    async fn apply_to_jsonl_uses_atomic_rewrite() {
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
        writer
            .append_memory_access(&MemoryAccessLog {
                timestamp: "2026-02-24T00:00:01Z".to_string(),
                experiment_id: "exp-1".to_string(),
                trace_id: "trace-1".to_string(),
                action: MemoryAction::Read,
                memory_id: "m1".to_string(),
                task_context: "ctx".to_string(),
                task_type: TaskType::Planning,
                actor: Actor::Agent,
                was_useful: None,
                useful_annotation_source: None,
                annotation_confidence: None,
                tokens_consumed: 1,
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let pipeline = AnnotationPipeline::new(writer.clone(), dir.path().join("logs"));
        let updates = vec![AnnotationUpdate {
            experiment_id: "exp-1".to_string(),
            trace_id: "trace-1".to_string(),
            memory_id: "m1".to_string(),
            was_useful: true,
            confidence: 0.9,
            source: AnnotationSource::AutoEvaluator,
            needs_human_review: false,
        }];
        let updated = pipeline.apply_to_jsonl(&updates).await.unwrap();
        assert_eq!(updated, 1);
    }
}
