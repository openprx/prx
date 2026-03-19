use crate::self_system::evolution::record::{DecisionLog, MemoryAccessLog, Outcome, TaskType};
use crate::self_system::evolution::safety_utils::atomic_write;
use crate::self_system::evolution::storage::AsyncJsonlWriter;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

const DEFAULT_SHIFT_THRESHOLD: f64 = 0.15;
const DEFAULT_UNKNOWN_ALERT_THRESHOLD: f64 = 0.30;
const DEFAULT_MEMORY_CONFIDENCE_THRESHOLD: f64 = 0.7;
const DEFAULT_BACKFILL_AFTER_DAYS: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidatePriority {
    Low,
    Medium,
    High,
}

/// Structured evolution action candidate extracted from trends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionCandidate {
    pub target: BTreeMap<String, String>,
    pub current_value: String,
    pub suggested_value: String,
    pub evidence_ids: Vec<String>,
    pub priority: CandidatePriority,
    pub backfill_after_days: u32,
}

/// Per-task-type effectiveness summary.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskTypeDigest {
    pub total: u32,
    pub success: u32,
    pub failure: u32,
    pub corrected: u32,
    pub avg_tokens: f64,
    pub success_rate: f64,
    pub failure_rate: f64,
}

/// Significant metric drift against previous daily digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricShift {
    pub metric: String,
    pub previous: f64,
    pub current: f64,
    pub delta_ratio: f64,
}

/// Daily 24h digest summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyDigest {
    pub date: String,
    pub window_start: String,
    pub window_end: String,
    pub total_tasks: u32,
    pub successful_tasks: u32,
    pub failed_tasks: u32,
    pub corrected_tasks: u32,
    pub memory_hit_rate: f64,
    pub avg_tokens_consumed: f64,
    pub unknown_annotation_ratio: f64,
    pub task_type_distribution: BTreeMap<String, TaskTypeDigest>,
    pub alerts: Vec<String>,
    pub significant_changes: Vec<MetricShift>,
}

/// Noisy memory pattern within trend window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoiseMemoryPattern {
    pub memory_id: String,
    pub load_count: u32,
    pub evidence_ids: Vec<String>,
}

/// Weakest task type in trend window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTypeWeakness {
    pub task_type: String,
    pub failure_rate: f64,
    pub evidence_ids: Vec<String>,
}

/// Lowest efficiency config/hash in trend window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEfficiencyIssue {
    pub config_snapshot_hash: String,
    pub token_per_success: f64,
    pub evidence_ids: Vec<String>,
}

/// Clustered user correction pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCorrectionCluster {
    pub pattern: String,
    pub count: u32,
    pub evidence_ids: Vec<String>,
}

/// Three-day trend analysis output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendAnalysis {
    pub start_date: String,
    pub end_date: String,
    pub digests: Vec<DailyDigest>,
    pub noise_memories: Vec<NoiseMemoryPattern>,
    pub weakest_task_type: Option<TaskTypeWeakness>,
    pub lowest_efficiency_config: Option<ConfigEfficiencyIssue>,
    pub user_correction_clusters: Vec<UserCorrectionCluster>,
    pub candidates: Vec<EvolutionCandidate>,
}

/// Rule-based analyzer over evolution JSONL logs.
pub struct EvolutionAnalyzer {
    writer: Arc<AsyncJsonlWriter>,
    data_source: Option<Arc<dyn AnalyzerDataSource>>,
    analysis_root: PathBuf,
    memory_confidence_threshold: f64,
    shift_threshold: f64,
    unknown_alert_threshold: f64,
}

/// Optional query backend used by analyzer. Defaults to JSONL writer reads.
#[async_trait]
pub trait AnalyzerDataSource: Send + Sync {
    async fn read_decisions_since(&self, since: DateTime<Utc>) -> Result<Vec<DecisionLog>>;
    async fn read_memory_access_since(&self, since: DateTime<Utc>) -> Result<Vec<MemoryAccessLog>>;
}

impl EvolutionAnalyzer {
    pub fn new(writer: Arc<AsyncJsonlWriter>, analysis_root: impl AsRef<Path>) -> Self {
        Self {
            writer,
            data_source: None,
            analysis_root: analysis_root.as_ref().to_path_buf(),
            memory_confidence_threshold: DEFAULT_MEMORY_CONFIDENCE_THRESHOLD,
            shift_threshold: DEFAULT_SHIFT_THRESHOLD,
            unknown_alert_threshold: DEFAULT_UNKNOWN_ALERT_THRESHOLD,
        }
    }

    pub fn with_data_source(mut self, data_source: Arc<dyn AnalyzerDataSource>) -> Self {
        self.data_source = Some(data_source);
        self
    }

    pub fn with_thresholds(
        mut self,
        memory_confidence_threshold: f64,
        shift_threshold: f64,
        unknown_alert_threshold: f64,
    ) -> Self {
        self.memory_confidence_threshold = memory_confidence_threshold.clamp(0.0, 1.0);
        self.shift_threshold = shift_threshold.max(0.0);
        self.unknown_alert_threshold = unknown_alert_threshold.clamp(0.0, 1.0);
        self
    }

    /// Build and persist a daily digest for last 24 hours ending at `now`.
    pub async fn generate_daily_digest(&self, now: DateTime<Utc>) -> Result<DailyDigest> {
        let window_start = now - Duration::hours(24);

        let decisions = self
            .read_decisions_since(window_start)
            .await?
            .into_iter()
            .filter(|item| parse_ts(&item.timestamp).is_some_and(|ts| ts <= now))
            .collect::<Vec<DecisionLog>>();
        let memory = self
            .read_memory_access_since(window_start)
            .await?
            .into_iter()
            .filter(|item| parse_ts(&item.timestamp).is_some_and(|ts| ts <= now))
            .collect::<Vec<MemoryAccessLog>>();

        let total_tasks = decisions.len() as u32;
        let successful_tasks = decisions
            .iter()
            .filter(|item| item.outcome == Outcome::Success)
            .count() as u32;
        let failed_tasks = decisions
            .iter()
            .filter(|item| matches!(item.outcome, Outcome::Failure | Outcome::RolledBack))
            .count() as u32;
        let corrected_tasks = decisions
            .iter()
            .filter(|item| {
                item.user_correction
                    .as_ref()
                    .is_some_and(|v| !v.trim().is_empty())
            })
            .count() as u32;

        let avg_tokens_consumed = average_u32(decisions.iter().map(|item| item.tokens_used));

        let memory_hit_matches = memory
            .iter()
            .filter(|item| {
                item.was_useful == Some(true)
                    && item
                        .annotation_confidence
                        .is_some_and(|v| v >= self.memory_confidence_threshold)
            })
            .count() as f64;
        let memory_hit_rate = ratio(memory_hit_matches, memory.len() as f64);

        let unknown_annotations = memory
            .iter()
            .filter(|item| item.was_useful.is_none())
            .count() as f64;
        let unknown_annotation_ratio = ratio(unknown_annotations, memory.len() as f64);

        let task_type_distribution = build_task_type_distribution(&decisions);

        let date = now.date_naive().to_string();
        let mut digest = DailyDigest {
            date,
            window_start: window_start.to_rfc3339(),
            window_end: now.to_rfc3339(),
            total_tasks,
            successful_tasks,
            failed_tasks,
            corrected_tasks,
            memory_hit_rate,
            avg_tokens_consumed,
            unknown_annotation_ratio,
            task_type_distribution,
            alerts: Vec::new(),
            significant_changes: Vec::new(),
        };

        if unknown_annotation_ratio > self.unknown_alert_threshold {
            digest.alerts.push(format!(
                "unknown_annotation_ratio {:.2}% exceeded threshold {:.2}%",
                unknown_annotation_ratio * 100.0,
                self.unknown_alert_threshold * 100.0
            ));
        }

        if let Some(previous) = self
            .load_daily_digest(now.date_naive() - Duration::days(1))
            .await?
        {
            digest.significant_changes =
                compare_daily_digest(&previous, &digest, self.shift_threshold);
        }

        self.persist_daily_digest(&digest).await?;
        Ok(digest)
    }

    /// Aggregate the latest three daily digests and generate trend analysis.
    pub async fn generate_three_day_trend(&self, end_date: NaiveDate) -> Result<TrendAnalysis> {
        let dates = [
            end_date - Duration::days(2),
            end_date - Duration::days(1),
            end_date,
        ];

        let mut digests = Vec::with_capacity(3);
        for day in dates {
            let digest = self
                .load_daily_digest(day)
                .await?
                .with_context(|| format!("missing daily digest for {day}"))?;
            digests.push(digest);
        }

        let range_start =
            DateTime::<Utc>::from_naive_utc_and_offset(dates[0].and_time(NaiveTime::MIN), Utc);
        let range_end = DateTime::<Utc>::from_naive_utc_and_offset(
            (end_date + Duration::days(1)).and_time(NaiveTime::MIN),
            Utc,
        );

        let decisions = self
            .read_decisions_since(range_start)
            .await?
            .into_iter()
            .filter(|item| parse_ts(&item.timestamp).is_some_and(|ts| ts < range_end))
            .collect::<Vec<_>>();
        let memory = self
            .read_memory_access_since(range_start)
            .await?
            .into_iter()
            .filter(|item| parse_ts(&item.timestamp).is_some_and(|ts| ts < range_end))
            .collect::<Vec<_>>();

        let noise_memories = extract_noise_memories(&memory);
        let weakest_task_type = extract_weakest_task_type(&decisions);
        let lowest_efficiency_config = extract_lowest_efficiency_config(&decisions);
        let user_correction_clusters = extract_user_correction_clusters(&decisions);

        let mut candidates = Vec::new();
        for item in noise_memories.iter().take(3) {
            let mut target = BTreeMap::new();
            target.insert("memory_id".to_string(), item.memory_id.clone());
            candidates.push(EvolutionCandidate {
                target,
                current_value: format!("loaded {} times and never useful", item.load_count),
                suggested_value: "suppress_in_retrieval_or_archive".to_string(),
                evidence_ids: item.evidence_ids.clone(),
                priority: CandidatePriority::High,
                backfill_after_days: DEFAULT_BACKFILL_AFTER_DAYS,
            });
        }

        if let Some(item) = weakest_task_type.as_ref() {
            let mut target = BTreeMap::new();
            target.insert("task_type".to_string(), item.task_type.clone());
            candidates.push(EvolutionCandidate {
                target,
                current_value: format!("failure_rate={:.2}", item.failure_rate),
                suggested_value: "increase_validation_and_retry_policy".to_string(),
                evidence_ids: item.evidence_ids.clone(),
                priority: CandidatePriority::High,
                backfill_after_days: DEFAULT_BACKFILL_AFTER_DAYS,
            });
        }

        if let Some(item) = lowest_efficiency_config.as_ref() {
            let mut target = BTreeMap::new();
            target.insert(
                "config_snapshot_hash".to_string(),
                item.config_snapshot_hash.clone(),
            );
            candidates.push(EvolutionCandidate {
                target,
                current_value: format!("token_per_success={:.2}", item.token_per_success),
                suggested_value: "tune_generation_params_for_efficiency".to_string(),
                evidence_ids: item.evidence_ids.clone(),
                priority: CandidatePriority::Medium,
                backfill_after_days: DEFAULT_BACKFILL_AFTER_DAYS,
            });
        }

        for item in user_correction_clusters.iter().take(2) {
            let mut target = BTreeMap::new();
            target.insert("user_correction_pattern".to_string(), item.pattern.clone());
            candidates.push(EvolutionCandidate {
                target,
                current_value: format!("frequency={}", item.count),
                suggested_value: "add_targeted_safety_or_tooling_guard".to_string(),
                evidence_ids: item.evidence_ids.clone(),
                priority: CandidatePriority::Medium,
                backfill_after_days: DEFAULT_BACKFILL_AFTER_DAYS,
            });
        }

        Ok(TrendAnalysis {
            start_date: dates[0].to_string(),
            end_date: end_date.to_string(),
            digests,
            noise_memories,
            weakest_task_type,
            lowest_efficiency_config,
            user_correction_clusters,
            candidates,
        })
    }

    async fn persist_daily_digest(&self, digest: &DailyDigest) -> Result<()> {
        let path = self.daily_digest_path_from_str(&digest.date)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let payload = serde_json::to_string_pretty(digest)?;
        atomic_write(&self.analysis_root, &path, payload.as_bytes()).await?;
        Ok(())
    }

    async fn load_daily_digest(&self, date: NaiveDate) -> Result<Option<DailyDigest>> {
        let path = self.daily_digest_path(date);
        if fs::metadata(&path).await.is_err() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path).await?;
        let digest = serde_json::from_str::<DailyDigest>(&raw)?;
        Ok(Some(digest))
    }

    fn daily_digest_path(&self, date: NaiveDate) -> PathBuf {
        self.analysis_root
            .join("daily")
            .join(format!("{date}.json"))
    }

    fn daily_digest_path_from_str(&self, date: &str) -> Result<PathBuf> {
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .with_context(|| format!("invalid digest date: {date}"))?;
        Ok(self.daily_digest_path(date))
    }

    async fn read_decisions_since(&self, since: DateTime<Utc>) -> Result<Vec<DecisionLog>> {
        if let Some(source) = self.data_source.as_ref() {
            source.read_decisions_since(since).await
        } else {
            self.writer.read_decisions_since(since).await
        }
    }

    async fn read_memory_access_since(&self, since: DateTime<Utc>) -> Result<Vec<MemoryAccessLog>> {
        if let Some(source) = self.data_source.as_ref() {
            source.read_memory_access_since(since).await
        } else {
            self.writer.read_memory_access_since(since).await
        }
    }
}

fn build_task_type_distribution(decisions: &[DecisionLog]) -> BTreeMap<String, TaskTypeDigest> {
    let mut grouped: HashMap<String, Vec<&DecisionLog>> = HashMap::new();
    for item in decisions {
        grouped
            .entry(task_type_name(&item.task_type))
            .or_default()
            .push(item);
    }

    let mut out = BTreeMap::new();
    for (task_type, rows) in grouped {
        let total = rows.len() as u32;
        let success = rows
            .iter()
            .filter(|item| item.outcome == Outcome::Success)
            .count() as u32;
        let failure = rows
            .iter()
            .filter(|item| matches!(item.outcome, Outcome::Failure | Outcome::RolledBack))
            .count() as u32;
        let corrected = rows
            .iter()
            .filter(|item| {
                item.user_correction
                    .as_ref()
                    .is_some_and(|v| !v.trim().is_empty())
            })
            .count() as u32;
        let avg_tokens = average_u32(rows.iter().map(|item| item.tokens_used));

        out.insert(
            task_type,
            TaskTypeDigest {
                total,
                success,
                failure,
                corrected,
                avg_tokens,
                success_rate: ratio(success as f64, total as f64),
                failure_rate: ratio(failure as f64, total as f64),
            },
        );
    }

    out
}

fn compare_daily_digest(
    previous: &DailyDigest,
    current: &DailyDigest,
    threshold: f64,
) -> Vec<MetricShift> {
    let mut shifts = Vec::new();
    push_shift(
        &mut shifts,
        "total_tasks",
        previous.total_tasks as f64,
        current.total_tasks as f64,
        threshold,
    );
    push_shift(
        &mut shifts,
        "success_rate",
        ratio(
            previous.successful_tasks as f64,
            previous.total_tasks as f64,
        ),
        ratio(current.successful_tasks as f64, current.total_tasks as f64),
        threshold,
    );
    push_shift(
        &mut shifts,
        "failure_rate",
        ratio(previous.failed_tasks as f64, previous.total_tasks as f64),
        ratio(current.failed_tasks as f64, current.total_tasks as f64),
        threshold,
    );
    push_shift(
        &mut shifts,
        "memory_hit_rate",
        previous.memory_hit_rate,
        current.memory_hit_rate,
        threshold,
    );
    push_shift(
        &mut shifts,
        "avg_tokens_consumed",
        previous.avg_tokens_consumed,
        current.avg_tokens_consumed,
        threshold,
    );
    push_shift(
        &mut shifts,
        "unknown_annotation_ratio",
        previous.unknown_annotation_ratio,
        current.unknown_annotation_ratio,
        threshold,
    );
    shifts
}

fn push_shift(
    shifts: &mut Vec<MetricShift>,
    metric: &str,
    previous: f64,
    current: f64,
    threshold: f64,
) {
    let delta = (current - previous).abs();
    let denominator = if previous.abs() < 1e-9 {
        1.0
    } else {
        previous.abs()
    };
    let delta_ratio = delta / denominator;
    if delta_ratio > threshold {
        shifts.push(MetricShift {
            metric: metric.to_string(),
            previous,
            current,
            delta_ratio,
        });
    }
}

fn extract_noise_memories(memory: &[MemoryAccessLog]) -> Vec<NoiseMemoryPattern> {
    let mut grouped: HashMap<String, (u32, u32, Vec<String>)> = HashMap::new();
    for item in memory {
        let bucket = grouped
            .entry(item.memory_id.clone())
            .or_insert_with(|| (0, 0, Vec::new()));
        bucket.0 = bucket.0.saturating_add(1);
        if item.was_useful == Some(false) {
            bucket.1 = bucket.1.saturating_add(1);
        }
        bucket.2.push(item.trace_id.clone());
    }

    let mut result = grouped
        .into_iter()
        .filter(|(_, (loaded, false_count, _))| *loaded >= 2 && *false_count == *loaded)
        .map(
            |(memory_id, (load_count, _, evidence_ids))| NoiseMemoryPattern {
                memory_id,
                load_count,
                evidence_ids,
            },
        )
        .collect::<Vec<_>>();
    result.sort_by(|a, b| b.load_count.cmp(&a.load_count));
    result
}

fn extract_weakest_task_type(decisions: &[DecisionLog]) -> Option<TaskTypeWeakness> {
    let distribution = build_task_type_distribution(decisions);
    distribution
        .into_iter()
        .max_by(|(_, a), (_, b)| a.failure_rate.total_cmp(&b.failure_rate))
        .map(|(task_type, digest)| {
            let evidence_ids = decisions
                .iter()
                .filter(|item| task_type_name(&item.task_type) == task_type)
                .map(|item| item.trace_id.clone())
                .collect::<Vec<_>>();
            TaskTypeWeakness {
                task_type,
                failure_rate: digest.failure_rate,
                evidence_ids,
            }
        })
}

fn extract_lowest_efficiency_config(decisions: &[DecisionLog]) -> Option<ConfigEfficiencyIssue> {
    let mut grouped: HashMap<String, (u32, u32, u32, Vec<String>)> = HashMap::new();
    for item in decisions {
        let bucket = grouped
            .entry(item.config_snapshot_hash.clone())
            .or_insert_with(|| (0, 0, 0, Vec::new()));
        bucket.0 = bucket.0.saturating_add(item.tokens_used);
        bucket.1 = bucket.1.saturating_add(1);
        if item.outcome == Outcome::Success {
            bucket.2 = bucket.2.saturating_add(1);
        }
        bucket.3.push(item.trace_id.clone());
    }

    grouped
        .into_iter()
        .max_by(|(_, a), (_, b)| token_per_success(a).total_cmp(&token_per_success(b)))
        .map(|(config_snapshot_hash, bucket)| ConfigEfficiencyIssue {
            config_snapshot_hash,
            token_per_success: token_per_success(&bucket),
            evidence_ids: bucket.3,
        })
}

fn token_per_success(bucket: &(u32, u32, u32, Vec<String>)) -> f64 {
    if bucket.2 > 0 {
        bucket.0 as f64 / bucket.2 as f64
    } else {
        bucket.0 as f64 / bucket.1.max(1) as f64
    }
}

fn extract_user_correction_clusters(decisions: &[DecisionLog]) -> Vec<UserCorrectionCluster> {
    let mut grouped: HashMap<String, (u32, Vec<String>)> = HashMap::new();
    for item in decisions {
        let Some(raw) = item.user_correction.as_ref() else {
            continue;
        };
        let key = normalize_correction(raw);
        if key.is_empty() {
            continue;
        }
        let bucket = grouped.entry(key).or_insert_with(|| (0, Vec::new()));
        bucket.0 = bucket.0.saturating_add(1);
        bucket.1.push(item.trace_id.clone());
    }

    let mut out = grouped
        .into_iter()
        .filter(|(_, (count, _))| *count >= 2)
        .map(|(pattern, (count, evidence_ids))| UserCorrectionCluster {
            pattern,
            count,
            evidence_ids,
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| b.count.cmp(&a.count));
    out
}

fn normalize_correction(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().chars().take(80).collect()
}

fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator <= 0.0 {
        0.0
    } else {
        (numerator / denominator).clamp(0.0, 1.0)
    }
}

fn average_u32<I>(items: I) -> f64
where
    I: Iterator<Item = u32>,
{
    let mut total = 0u64;
    let mut count = 0u64;
    for item in items {
        total = total.saturating_add(item as u64);
        count = count.saturating_add(1);
    }
    if count == 0 {
        0.0
    } else {
        total as f64 / count as f64
    }
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    match DateTime::parse_from_rfc3339(raw) {
        Ok(dt) => Some(dt.with_timezone(&Utc)),
        Err(err) => {
            tracing::debug!(
                timestamp = raw,
                error = %err,
                "failed to parse analyzer timestamp"
            );
            None
        }
    }
}

fn task_type_name(task_type: &TaskType) -> String {
    serde_json::to_string(task_type)
        .unwrap_or_else(|_| "\"other\"".to_string())
        .trim_matches('"')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::record::{
        Actor, AnnotationSource, DecisionType, MemoryAction,
    };
    use crate::self_system::evolution::storage::{JsonlRetentionPolicy, JsonlStoragePaths};
    use tempfile::tempdir;

    #[tokio::test]
    async fn daily_digest_computes_metrics_and_alerts() {
        let dir = tempdir().unwrap();
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(dir.path().join("logs")),
                JsonlRetentionPolicy::default(),
                4,
            )
            .await
            .unwrap(),
        );

        writer
            .append_decision(&DecisionLog {
                timestamp: "2026-02-24T08:00:00Z".into(),
                experiment_id: "exp-a".into(),
                trace_id: "trace-a".into(),
                decision_type: DecisionType::ToolSelection,
                task_type: TaskType::ToolCall,
                risk_level: 1,
                actor: Actor::Agent,
                input_context: "ctx".into(),
                action_taken: "run".into(),
                outcome: Outcome::Success,
                tokens_used: 100,
                latency_ms: 12,
                user_correction: Some("add timeout".into()),
                config_snapshot_hash: "cfg-1".into(),
            })
            .await
            .unwrap();

        writer
            .append_decision(&DecisionLog {
                timestamp: "2026-02-24T09:00:00Z".into(),
                experiment_id: "exp-a".into(),
                trace_id: "trace-b".into(),
                decision_type: DecisionType::RuntimePolicy,
                task_type: TaskType::ToolCall,
                risk_level: 2,
                actor: Actor::Agent,
                input_context: "ctx".into(),
                action_taken: "run".into(),
                outcome: Outcome::Failure,
                tokens_used: 200,
                latency_ms: 20,
                user_correction: None,
                config_snapshot_hash: "cfg-2".into(),
            })
            .await
            .unwrap();

        writer
            .append_memory_access(&MemoryAccessLog {
                timestamp: "2026-02-24T08:10:00Z".into(),
                experiment_id: "exp-a".into(),
                trace_id: "trace-a".into(),
                action: MemoryAction::Read,
                memory_id: "m-1".into(),
                task_context: "ctx".into(),
                task_type: TaskType::Planning,
                actor: Actor::Agent,
                was_useful: Some(true),
                useful_annotation_source: Some(AnnotationSource::AutoEvaluator),
                annotation_confidence: Some(0.95),
                tokens_consumed: 10,
            })
            .await
            .unwrap();

        writer
            .append_memory_access(&MemoryAccessLog {
                timestamp: "2026-02-24T08:30:00Z".into(),
                experiment_id: "exp-a".into(),
                trace_id: "trace-b".into(),
                action: MemoryAction::Read,
                memory_id: "m-2".into(),
                task_context: "ctx".into(),
                task_type: TaskType::Planning,
                actor: Actor::Agent,
                was_useful: None,
                useful_annotation_source: None,
                annotation_confidence: None,
                tokens_consumed: 20,
            })
            .await
            .unwrap();

        writer.flush().await.unwrap();

        let analyzer = EvolutionAnalyzer::new(writer, dir.path().join("data/analysis"))
            .with_thresholds(0.7, 0.15, 0.30);
        let now = DateTime::parse_from_rfc3339("2026-02-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let digest = analyzer.generate_daily_digest(now).await.unwrap();

        assert_eq!(digest.total_tasks, 2);
        assert_eq!(digest.successful_tasks, 1);
        assert_eq!(digest.failed_tasks, 1);
        assert_eq!(digest.corrected_tasks, 1);
        assert!((digest.memory_hit_rate - 0.5).abs() < 1e-6);
        assert!((digest.avg_tokens_consumed - 150.0).abs() < 1e-6);
        assert!((digest.unknown_annotation_ratio - 0.5).abs() < 1e-6);
        assert!(!digest.alerts.is_empty());

        let persisted = fs::read_to_string(dir.path().join("data/analysis/daily/2026-02-24.json"))
            .await
            .unwrap();
        assert!(persisted.contains("memory_hit_rate"));
    }

    #[tokio::test]
    async fn three_day_trend_extracts_patterns_and_candidates() {
        let dir = tempdir().unwrap();
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(dir.path().join("logs")),
                JsonlRetentionPolicy::default(),
                2,
            )
            .await
            .unwrap(),
        );

        let analyzer = EvolutionAnalyzer::new(writer.clone(), dir.path().join("data/analysis"));

        let days = [
            "2026-02-22T12:00:00Z",
            "2026-02-23T12:00:00Z",
            "2026-02-24T12:00:00Z",
        ];

        for now in days {
            let now = DateTime::parse_from_rfc3339(now)
                .unwrap()
                .with_timezone(&Utc);
            writer
                .append_decision(&DecisionLog {
                    timestamp: now.to_rfc3339(),
                    experiment_id: "exp-1".into(),
                    trace_id: format!("trace-{}", now.date_naive()),
                    decision_type: DecisionType::ToolSelection,
                    task_type: TaskType::ToolCall,
                    risk_level: 1,
                    actor: Actor::Agent,
                    input_context: "ctx".into(),
                    action_taken: "run".into(),
                    outcome: Outcome::Failure,
                    tokens_used: 300,
                    latency_ms: 10,
                    user_correction: Some("please add timeout".into()),
                    config_snapshot_hash: "cfg-worst".into(),
                })
                .await
                .unwrap();
            writer
                .append_memory_access(&MemoryAccessLog {
                    timestamp: now.to_rfc3339(),
                    experiment_id: "exp-1".into(),
                    trace_id: format!("trace-{}", now.date_naive()),
                    action: MemoryAction::Read,
                    memory_id: "noise-memory".into(),
                    task_context: "ctx".into(),
                    task_type: TaskType::Planning,
                    actor: Actor::Agent,
                    was_useful: Some(false),
                    useful_annotation_source: Some(AnnotationSource::AutoEvaluator),
                    annotation_confidence: Some(0.9),
                    tokens_consumed: 30,
                })
                .await
                .unwrap();
            writer.flush().await.unwrap();
            analyzer.generate_daily_digest(now).await.unwrap();
        }

        let trend = analyzer
            .generate_three_day_trend(NaiveDate::from_ymd_opt(2026, 2, 24).unwrap())
            .await
            .unwrap();

        assert_eq!(trend.digests.len(), 3);
        assert!(!trend.noise_memories.is_empty());
        assert!(
            trend
                .weakest_task_type
                .as_ref()
                .is_some_and(|v| v.task_type == "tool_call")
        );
        assert!(trend.lowest_efficiency_config.is_some());
        assert!(!trend.user_correction_clusters.is_empty());
        assert!(!trend.candidates.is_empty());
        assert!(
            trend
                .candidates
                .iter()
                .all(|candidate| !candidate.evidence_ids.is_empty())
        );
    }

    #[test]
    fn compare_daily_digest_marks_large_shift() {
        let previous = DailyDigest {
            date: "2026-02-23".into(),
            window_start: "2026-02-22T12:00:00Z".into(),
            window_end: "2026-02-23T12:00:00Z".into(),
            total_tasks: 100,
            successful_tasks: 80,
            failed_tasks: 20,
            corrected_tasks: 5,
            memory_hit_rate: 0.8,
            avg_tokens_consumed: 100.0,
            unknown_annotation_ratio: 0.1,
            task_type_distribution: BTreeMap::new(),
            alerts: Vec::new(),
            significant_changes: Vec::new(),
        };
        let mut current = previous.clone();
        current.total_tasks = 120;
        current.avg_tokens_consumed = 130.0;

        let shifts = compare_daily_digest(&previous, &current, 0.15);
        assert!(shifts.iter().any(|item| item.metric == "total_tasks"));
        assert!(
            shifts
                .iter()
                .any(|item| item.metric == "avg_tokens_consumed")
        );
    }

    #[test]
    fn ratio_and_average_helpers_are_stable() {
        assert_eq!(ratio(1.0, 0.0), 0.0);
        assert_eq!(average_u32([1_u32, 2_u32, 3_u32].into_iter()), 2.0);
    }

    #[test]
    fn defaults_match_week3_contract() {
        assert_eq!(DEFAULT_SHIFT_THRESHOLD, 0.15);
        assert_eq!(DEFAULT_UNKNOWN_ALERT_THRESHOLD, 0.30);
        assert_eq!(DEFAULT_MEMORY_CONFIDENCE_THRESHOLD, 0.7);
        assert_eq!(DEFAULT_BACKFILL_AFTER_DAYS, 3);
    }
}
