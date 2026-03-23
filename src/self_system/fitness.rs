use crate::config::Config;
use crate::cost::tracker::CostTracker;
use crate::cron;
use crate::health;
use crate::memory::{self, Memory, MemoryCategory};
use crate::self_system::SELF_SYSTEM_SESSION_ID;
use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const FITNESS_REPORT_VERSION: &str = "p0-1";
const NO_REPEAT_FALLBACK_SCORE: f64 = 0.7;
const EFFICIENCY_FALLBACK_SCORE: f64 = 0.5;
const HEARTBEAT_TARGET_RUNS_PER_DAY: usize = 4;

const WEIGHT_TASK_QUALITY: f64 = 0.35;
const WEIGHT_NO_REPEAT: f64 = 0.25;
const WEIGHT_PROACTIVE: f64 = 0.20;
const WEIGHT_LEARNING: f64 = 0.10;
const WEIGHT_EFFICIENCY: f64 = 0.10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitnessWindow {
    pub date: String,
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitnessSubscores {
    pub task_quality: f64,
    pub no_repeat: f64,
    pub proactive: f64,
    pub learning: f64,
    pub efficiency: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitnessWeights {
    pub task_quality: f64,
    pub no_repeat: f64,
    pub proactive: f64,
    pub learning: f64,
    pub efficiency: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FitnessEvidence {
    pub task_quality: serde_json::Value,
    pub no_repeat: serde_json::Value,
    pub proactive: serde_json::Value,
    pub learning: serde_json::Value,
    pub efficiency: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitnessReport {
    pub version: String,
    pub window: FitnessWindow,
    pub subscores: FitnessSubscores,
    pub weights: FitnessWeights,
    pub final_score: f64,
    pub confidence: f64,
    pub evidence: FitnessEvidence,
}

#[derive(Debug, Clone, Copy)]
struct WindowBounds {
    day: NaiveDate,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

/// Run the P0-1 fitness report pipeline and persist the result to memory.
///
/// Output memory key format:
/// `self/fitness/daily/YYYY-MM-DD`, category `core`.
pub async fn run_fitness_report() -> Result<FitnessReport> {
    let config = Config::load_or_init().await?;
    let storage_provider = Some(&config.storage.provider.config);
    let memory = memory::create_memory_with_storage_and_routes(
        &config.memory,
        &config.embedding_routes,
        storage_provider,
        &config.workspace_dir,
        config.api_key.as_deref(),
    )?;

    let tracker = CostTracker::new(config.cost.clone(), &config.workspace_dir).ok();
    let tracker_ref = tracker.as_ref();

    build_and_store_fitness_report(memory.as_ref(), &config, tracker_ref).await
}

async fn build_and_store_fitness_report(
    memory: &dyn Memory,
    config: &Config,
    cost_tracker: Option<&CostTracker>,
) -> Result<FitnessReport> {
    let window = current_window()?;
    let (subscores, confidence, evidence) = compute_subscores(memory, config, cost_tracker, window).await;

    let final_score = clamp_0_1(
        subscores.efficiency.mul_add(
            WEIGHT_EFFICIENCY,
            subscores.learning.mul_add(
                WEIGHT_LEARNING,
                subscores.proactive.mul_add(
                    WEIGHT_PROACTIVE,
                    subscores
                        .task_quality
                        .mul_add(WEIGHT_TASK_QUALITY, subscores.no_repeat * WEIGHT_NO_REPEAT),
                ),
            ),
        ),
    );

    let report = FitnessReport {
        version: FITNESS_REPORT_VERSION.to_string(),
        window: FitnessWindow {
            date: window.day.to_string(),
            start: window.start.to_rfc3339(),
            end: window.end.to_rfc3339(),
        },
        subscores,
        weights: FitnessWeights {
            task_quality: WEIGHT_TASK_QUALITY,
            no_repeat: WEIGHT_NO_REPEAT,
            proactive: WEIGHT_PROACTIVE,
            learning: WEIGHT_LEARNING,
            efficiency: WEIGHT_EFFICIENCY,
        },
        final_score,
        confidence: clamp_0_1(confidence),
        evidence,
    };

    let report_key = format!("self/fitness/daily/{}", window.day);
    let report_json = serde_json::to_string_pretty(&report)?;
    memory
        .store(
            &report_key,
            &report_json,
            MemoryCategory::Core,
            Some(SELF_SYSTEM_SESSION_ID),
        )
        .await?;

    Ok(report)
}

async fn compute_subscores(
    memory: &dyn Memory,
    config: &Config,
    cost_tracker: Option<&CostTracker>,
    window: WindowBounds,
) -> (FitnessSubscores, f64, FitnessEvidence) {
    let health_snapshot = health::snapshot();
    let (task_quality_score, task_quality_confidence, task_quality_evidence) =
        task_quality_from_health(&health_snapshot);

    let (proactive_score, proactive_confidence, proactive_evidence) = proactive_from_cron_runs(config, window);

    let (learning_score, learning_confidence, learning_evidence) = learning_from_memory(memory, window).await;

    let (efficiency_score, efficiency_confidence, efficiency_evidence) = efficiency_from_cost(cost_tracker, window.day);

    let no_repeat_confidence = 0.3_f64;
    let no_repeat_evidence = serde_json::json!({
        "source": "fallback_constant",
        "note": "intent-level repeat detection not wired in P0",
        "value": NO_REPEAT_FALLBACK_SCORE
    });

    let confidence = weighted_confidence(&[
        (task_quality_confidence, WEIGHT_TASK_QUALITY),
        (no_repeat_confidence, WEIGHT_NO_REPEAT),
        (proactive_confidence, WEIGHT_PROACTIVE),
        (learning_confidence, WEIGHT_LEARNING),
        (efficiency_confidence, WEIGHT_EFFICIENCY),
    ]);

    (
        FitnessSubscores {
            task_quality: task_quality_score,
            no_repeat: NO_REPEAT_FALLBACK_SCORE,
            proactive: proactive_score,
            learning: learning_score,
            efficiency: efficiency_score,
        },
        confidence,
        FitnessEvidence {
            task_quality: task_quality_evidence,
            no_repeat: no_repeat_evidence,
            proactive: proactive_evidence,
            learning: learning_evidence,
            efficiency: efficiency_evidence,
        },
    )
}

fn task_quality_from_health(snapshot: &health::HealthSnapshot) -> (f64, f64, serde_json::Value) {
    let component_count = snapshot.components.len();
    if component_count == 0 {
        return (
            0.7,
            0.35,
            serde_json::json!({
                "source": "health_proxy",
                "reason": "no_components_registered",
                "score_basis": "fallback_constant"
            }),
        );
    }

    let ok_count = snapshot
        .components
        .values()
        .filter(|component| component.status == "ok")
        .count();
    let status_ratio = (ok_count as f64) / (component_count as f64);

    // No global observer-event store is currently available in P0.
    // Use component health ratio as deterministic proxy.
    (
        clamp_0_1(status_ratio),
        0.6,
        serde_json::json!({
            "source": "health_proxy",
            "component_count": component_count,
            "ok_components": ok_count,
            "health_ok_ratio": status_ratio,
            "tool_success_ratio_available": false
        }),
    )
}

fn proactive_from_cron_runs(config: &Config, window: WindowBounds) -> (f64, f64, serde_json::Value) {
    let jobs = match cron::list_jobs(config) {
        Ok(jobs) => jobs,
        Err(error) => {
            return (
                0.4,
                0.2,
                serde_json::json!({
                    "source": "cron_runs",
                    "error": error.to_string(),
                    "fallback": true
                }),
            );
        }
    };

    let heartbeat_jobs: Vec<_> = jobs.into_iter().filter(is_heartbeat_job).collect();

    if heartbeat_jobs.is_empty() {
        return (
            0.4,
            0.3,
            serde_json::json!({
                "source": "cron_runs",
                "heartbeat_jobs_found": 0,
                "fallback": true
            }),
        );
    }

    let mut run_count: usize = 0;
    let mut success_count: usize = 0;

    for job in &heartbeat_jobs {
        let runs = cron::list_runs(config, &job.id, config.cron.max_run_history as usize).unwrap_or_default();
        for run in runs {
            if run.started_at < window.start || run.started_at >= window.end {
                continue;
            }
            run_count += 1;
            if run.status == "ok" {
                success_count += 1;
            }
        }
    }

    if run_count == 0 {
        return (
            0.45,
            0.4,
            serde_json::json!({
                "source": "cron_runs",
                "heartbeat_jobs_found": heartbeat_jobs.len(),
                "runs_in_window": 0,
                "fallback": true
            }),
        );
    }

    let success_ratio = (success_count as f64) / (run_count as f64);
    let activity_ratio = (run_count as f64 / HEARTBEAT_TARGET_RUNS_PER_DAY as f64).clamp(0.0, 1.0);
    let score = clamp_0_1(0.7f64.mul_add(success_ratio, 0.3 * activity_ratio));

    (
        score,
        0.8,
        serde_json::json!({
            "source": "cron_runs",
            "heartbeat_jobs_found": heartbeat_jobs.len(),
            "runs_in_window": run_count,
            "success_runs": success_count,
            "success_ratio": success_ratio,
            "activity_ratio": activity_ratio
        }),
    )
}

async fn learning_from_memory(memory: &dyn Memory, window: WindowBounds) -> (f64, f64, serde_json::Value) {
    let mut entries = match memory.list(None, Some(SELF_SYSTEM_SESSION_ID)).await {
        Ok(entries) => entries,
        Err(error) => {
            return (
                0.3,
                0.2,
                serde_json::json!({
                    "source": "memory_list",
                    "error": error.to_string(),
                    "fallback": true
                }),
            );
        }
    };

    let legacy_entries = match memory.list(None, None).await {
        Ok(entries) => entries,
        Err(error) => {
            return (
                0.3,
                0.2,
                serde_json::json!({
                    "source": "memory_list",
                    "error": error.to_string(),
                    "fallback": true
                }),
            );
        }
    };
    entries.extend(
        legacy_entries
            .into_iter()
            .filter(|entry| entry.session_id.is_none() && entry.key.starts_with("self/")),
    );

    let mut seen = HashSet::new();
    let new_keys = entries
        .into_iter()
        .filter(|entry| {
            entry.session_id.as_deref() == Some(SELF_SYSTEM_SESSION_ID)
                || (entry.session_id.is_none() && entry.key.starts_with("self/"))
        })
        .filter(|entry| seen.insert(entry.key.clone()))
        .filter(|entry| !entry.key.starts_with("self/fitness/daily/"))
        .filter_map(|entry| parse_rfc3339_utc(&entry.timestamp).map(|ts| (entry.key, ts)))
        .filter(|(_, ts)| *ts >= window.start && *ts < window.end)
        .count();

    let score = (new_keys as f64 / 10.0).clamp(0.0, 1.0);
    (
        score,
        0.8,
        serde_json::json!({
            "source": "memory_list",
            "new_keys_in_window": new_keys,
            "normalization_target": 10
        }),
    )
}

fn efficiency_from_cost(cost_tracker: Option<&CostTracker>, day: NaiveDate) -> (f64, f64, serde_json::Value) {
    let Some(tracker) = cost_tracker else {
        return (
            EFFICIENCY_FALLBACK_SCORE,
            0.3,
            serde_json::json!({
                "source": "cost_tracker",
                "available": false,
                "fallback": true
            }),
        );
    };

    let daily_cost = match tracker.get_daily_cost(day) {
        Ok(cost) => cost,
        Err(error) => {
            return (
                EFFICIENCY_FALLBACK_SCORE,
                0.3,
                serde_json::json!({
                    "source": "cost_tracker",
                    "available": true,
                    "error": error.to_string(),
                    "fallback": true
                }),
            );
        }
    };

    // P0 linear scale: <=$1/day => excellent (1.0), >=$10/day => low efficiency (0.0).
    let score = if daily_cost <= 1.0 {
        1.0
    } else if daily_cost >= 10.0 {
        0.0
    } else {
        1.0 - ((daily_cost - 1.0) / 9.0)
    };

    (
        clamp_0_1(score),
        0.9,
        serde_json::json!({
            "source": "cost_tracker",
            "available": true,
            "daily_cost_usd": daily_cost
        }),
    )
}

fn is_heartbeat_job(job: &cron::CronJob) -> bool {
    let name = job.name.as_deref().unwrap_or_default().to_ascii_lowercase();
    let command = job.command.to_ascii_lowercase();
    let prompt = job.prompt.as_deref().unwrap_or_default().to_ascii_lowercase();

    name.contains("heartbeat") || command.contains("heartbeat") || prompt.contains("heartbeat")
}

fn parse_rfc3339_utc(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw).ok().map(|dt| dt.with_timezone(&Utc))
}

fn current_window() -> Result<WindowBounds> {
    let now = Utc::now();
    let day = now.date_naive();
    let start_naive = day
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow::anyhow!("failed to build daily window start"))?;
    let start = DateTime::<Utc>::from_naive_utc_and_offset(start_naive, Utc);
    let end = start + Duration::days(1);
    Ok(WindowBounds { day, start, end })
}

fn weighted_confidence(inputs: &[(f64, f64)]) -> f64 {
    let weight_sum: f64 = inputs.iter().map(|(_, weight)| *weight).sum();
    if weight_sum <= 0.0 {
        return 0.0;
    }
    let weighted: f64 = inputs.iter().map(|(value, weight)| clamp_0_1(*value) * *weight).sum();
    clamp_0_1(weighted / weight_sum)
}

const fn clamp_0_1(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::cron;
    use crate::memory::{MarkdownMemory, MemoryEntry, SqliteMemory};
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    #[tokio::test]
    async fn build_and_store_report_writes_daily_key() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let memory = SqliteMemory::new(&config.workspace_dir).unwrap();

        let report = build_and_store_fitness_report(&memory, &config, None).await.unwrap();

        assert_eq!(report.version, FITNESS_REPORT_VERSION);
        assert!(report.final_score >= 0.0 && report.final_score <= 1.0);
        let key = format!("self/fitness/daily/{}", report.window.date);
        assert!(memory.get(&key).await.unwrap().is_some());
    }

    #[test]
    fn heartbeat_job_detection_supports_name_command_and_prompt() {
        let mk = |name: Option<&str>, command: &str, prompt: Option<&str>| cron::CronJob {
            id: "id".into(),
            expression: "* * * * *".into(),
            schedule: cron::Schedule::Every { every_ms: 1000 },
            command: command.into(),
            prompt: prompt.map(str::to_string),
            name: name.map(str::to_string),
            job_type: cron::JobType::Shell,
            session_target: cron::SessionTarget::Isolated,
            model: None,
            enabled: true,
            delivery: cron::DeliveryConfig::default(),
            delete_after_run: false,
            created_at: Utc::now(),
            next_run: Utc::now(),
            last_run: None,
            last_status: None,
            last_output: None,
        };

        assert!(is_heartbeat_job(&mk(Some("daily-heartbeat"), "echo ok", None)));
        assert!(is_heartbeat_job(&mk(None, "run heartbeat check", None)));
        assert!(is_heartbeat_job(&mk(None, "echo ok", Some("heartbeat task"))));
        assert!(!is_heartbeat_job(&mk(
            Some("daily-check"),
            "echo ok",
            Some("normal task")
        )));
    }

    #[tokio::test]
    async fn learning_score_counts_window_entries() {
        struct TestMemory {
            entries: Vec<MemoryEntry>,
        }

        #[async_trait::async_trait]
        impl Memory for TestMemory {
            fn name(&self) -> &str {
                "test-memory"
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
                session_id: Option<&str>,
            ) -> Result<Vec<MemoryEntry>> {
                Ok(self
                    .entries
                    .iter()
                    .filter(|entry| session_id.is_none_or(|sid| entry.session_id.as_deref() == Some(sid)))
                    .cloned()
                    .collect())
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

        let start = Utc.with_ymd_and_hms(2026, 2, 23, 0, 0, 0).unwrap();
        let end = start + Duration::days(1);
        let memory = TestMemory {
            entries: vec![
                MemoryEntry {
                    id: "1".into(),
                    key: "k1".into(),
                    content: "v1".into(),
                    category: MemoryCategory::Core,
                    timestamp: (start + Duration::hours(1)).to_rfc3339(),
                    session_id: Some(SELF_SYSTEM_SESSION_ID.into()),
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
                MemoryEntry {
                    id: "2".into(),
                    key: "self/fitness/daily/2026-02-23".into(),
                    content: "ignored".into(),
                    category: MemoryCategory::Core,
                    timestamp: (start + Duration::hours(2)).to_rfc3339(),
                    session_id: Some(SELF_SYSTEM_SESSION_ID.into()),
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
                MemoryEntry {
                    id: "3".into(),
                    key: "k_old".into(),
                    content: "old".into(),
                    category: MemoryCategory::Core,
                    timestamp: (start - Duration::days(1)).to_rfc3339(),
                    session_id: Some(SELF_SYSTEM_SESSION_ID.into()),
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
                MemoryEntry {
                    id: "4".into(),
                    key: "user/k2".into(),
                    content: "external".into(),
                    category: MemoryCategory::Core,
                    timestamp: (start + Duration::hours(3)).to_rfc3339(),
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
                },
            ],
        };

        let (score, confidence, evidence) = learning_from_memory(
            &memory,
            WindowBounds {
                day: start.date_naive(),
                start,
                end,
            },
        )
        .await;

        assert!(score > 0.0);
        assert_eq!(confidence, 0.8);
        assert_eq!(evidence["new_keys_in_window"], 1);
    }

    #[tokio::test]
    async fn learning_score_includes_legacy_self_entries_and_deduplicates_keys() {
        struct TestMemory {
            entries: Vec<MemoryEntry>,
        }

        #[async_trait::async_trait]
        impl Memory for TestMemory {
            fn name(&self) -> &str {
                "test-memory"
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

        let start = Utc.with_ymd_and_hms(2026, 2, 23, 0, 0, 0).unwrap();
        let end = start + Duration::days(1);
        let memory = TestMemory {
            entries: vec![
                MemoryEntry {
                    id: "1".into(),
                    key: "self/evolution/plan".into(),
                    content: "new-format".into(),
                    category: MemoryCategory::Core,
                    timestamp: (start + Duration::hours(1)).to_rfc3339(),
                    session_id: Some(SELF_SYSTEM_SESSION_ID.into()),
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
                MemoryEntry {
                    id: "2".into(),
                    key: "self/retrospective/note".into(),
                    content: "legacy".into(),
                    category: MemoryCategory::Core,
                    timestamp: (start + Duration::hours(2)).to_rfc3339(),
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
                },
                MemoryEntry {
                    id: "3".into(),
                    key: "self/retrospective/note".into(),
                    content: "legacy-duplicate".into(),
                    category: MemoryCategory::Core,
                    timestamp: (start + Duration::hours(3)).to_rfc3339(),
                    session_id: Some(SELF_SYSTEM_SESSION_ID.into()),
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
                MemoryEntry {
                    id: "4".into(),
                    key: "user/k2".into(),
                    content: "external".into(),
                    category: MemoryCategory::Core,
                    timestamp: (start + Duration::hours(4)).to_rfc3339(),
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
                },
            ],
        };

        let (score, confidence, evidence) = learning_from_memory(
            &memory,
            WindowBounds {
                day: start.date_naive(),
                start,
                end,
            },
        )
        .await;

        assert!(score > 0.0);
        assert_eq!(confidence, 0.8);
        assert_eq!(evidence["new_keys_in_window"], 2);
    }

    #[tokio::test]
    async fn learning_score_reads_markdown_legacy_self_entries() {
        let tmp = TempDir::new().unwrap();
        let memory = MarkdownMemory::new(tmp.path());
        let day = "2026-02-23";
        let content = concat!(
            "# Long-Term Memory\n\n",
            "- **self/decisions/2026-02-23/proposal_1**: ",
            "{\"logged_at\":\"2026-02-23T02:00:00Z\",\"key\":\"router\",\"proposal_text\":\"p\",\"expected_outcome\":\"o\"}\n",
            "- **user/note**: external\n"
        );
        tokio::fs::write(tmp.path().join("MEMORY.md"), content).await.unwrap();

        let start = Utc.with_ymd_and_hms(2026, 2, 23, 0, 0, 0).unwrap();
        let end = start + Duration::days(1);
        let (score, confidence, evidence) = learning_from_memory(
            &memory,
            WindowBounds {
                day: day.parse().unwrap(),
                start,
                end,
            },
        )
        .await;

        assert!(score > 0.0);
        assert_eq!(confidence, 0.8);
        assert_eq!(evidence["new_keys_in_window"], 1);
    }
}
