use crate::config::{Config, FitnessConfig};
use crate::cron;
use crate::memory::{self, Memory, MemoryPrincipal, MessageEvent};
use crate::self_system::fitness_store::FitnessStore;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const FITNESS_REPORT_VERSION: &str = "2";
const MAX_EVENT_SCAN: usize = 100_000;
const BASELINE_DAYS: i64 = 30;

const WEIGHT_TASK_QUALITY: f64 = 0.35;
const WEIGHT_NO_REPEAT: f64 = 0.25;
const WEIGHT_PROACTIVE: f64 = 0.20;
const WEIGHT_LEARNING: f64 = 0.10;
const WEIGHT_EFFICIENCY: f64 = 0.10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitnessWindow {
    pub date: String,
    pub timezone: String,
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FitnessSubscores {
    pub task_quality: Option<f64>,
    pub no_repeat: Option<f64>,
    pub proactive: Option<f64>,
    pub learning: Option<f64>,
    pub efficiency: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitnessWeights {
    pub task_quality: f64,
    pub no_repeat: f64,
    pub proactive: f64,
    pub learning: f64,
    pub efficiency: f64,
}

impl Default for FitnessWeights {
    fn default() -> Self {
        Self {
            task_quality: WEIGHT_TASK_QUALITY,
            no_repeat: WEIGHT_NO_REPEAT,
            proactive: WEIGHT_PROACTIVE,
            learning: WEIGHT_LEARNING,
            efficiency: WEIGHT_EFFICIENCY,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MetricStatus {
    Available,
    InsufficientData,
    #[default]
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FitnessMetricStatus {
    pub task_quality: MetricStatus,
    pub no_repeat: MetricStatus,
    pub proactive: MetricStatus,
    pub learning: MetricStatus,
    pub efficiency: MetricStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FitnessReportStatus {
    Final,
    Provisional,
    InsufficientData,
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
    pub status: FitnessReportStatus,
    pub window: FitnessWindow,
    pub subscores: FitnessSubscores,
    pub metric_status: FitnessMetricStatus,
    pub weights: FitnessWeights,
    pub final_score: Option<f64>,
    pub confidence: f64,
    pub coverage: f64,
    pub evidence: FitnessEvidence,
    pub generated_at: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowBounds {
    pub(crate) day: NaiveDate,
    pub(crate) start: DateTime<Utc>,
    pub(crate) end: DateTime<Utc>,
}

#[derive(Debug)]
struct MetricResult {
    score: Option<f64>,
    status: MetricStatus,
    confidence: f64,
    evidence: serde_json::Value,
}

impl MetricResult {
    const fn available(score: f64, confidence: f64, evidence: serde_json::Value) -> Self {
        Self {
            score: Some(clamp_0_1(score)),
            status: MetricStatus::Available,
            confidence: clamp_0_1(confidence),
            evidence,
        }
    }

    const fn insufficient(evidence: serde_json::Value) -> Self {
        Self {
            score: None,
            status: MetricStatus::InsufficientData,
            confidence: 0.0,
            evidence,
        }
    }

    const fn unavailable(evidence: serde_json::Value) -> Self {
        Self {
            score: None,
            status: MetricStatus::Unavailable,
            confidence: 0.0,
            evidence,
        }
    }
}

/// Score the most recently closed local calendar day and persist it in the
/// dedicated fitness store. This function never writes a `MemoryCategory`.
pub async fn run_fitness_report_with_config(config: &Config) -> Result<FitnessReport> {
    let window = latest_closed_window(Utc::now(), &config.self_system.fitness)?;
    run_fitness_report_for_window(config, window).await
}

pub(crate) async fn run_fitness_report_for_window(config: &Config, window: WindowBounds) -> Result<FitnessReport> {
    validate_fitness_config(&config.self_system.fitness)?;
    let memory = memory::create_memory_with_storage_and_routes(
        &config.memory,
        &config.embedding_routes,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        config.api_key.as_deref(),
    )?;
    let report = build_fitness_report(memory.as_ref(), config, window).await?;
    FitnessStore::new(&config.workspace_dir).store(&report, config.self_system.fitness.retention_days)?;
    Ok(report)
}

async fn build_fitness_report(memory: &dyn Memory, config: &Config, window: WindowBounds) -> Result<FitnessReport> {
    let evidence_start = window.start - Duration::days(BASELINE_DAYS);
    let (events, event_error) = match load_evidence_events(memory, config, evidence_start, window.end).await {
        Ok(events) => (events, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    let day_events: Vec<&MessageEvent> = events
        .iter()
        .filter(|event| event_time(event).is_some_and(|time| time >= window.start && time < window.end))
        .collect();
    let baseline_events: Vec<&MessageEvent> = events
        .iter()
        .filter(|event| event_time(event).is_some_and(|time| time >= evidence_start && time < window.start))
        .collect();
    let min_samples = config.self_system.fitness.min_samples.max(1);

    let task_quality = event_error.as_ref().map_or_else(
        || task_quality_from_events(&day_events, min_samples),
        |error| event_ledger_unavailable("turn.finalized", error),
    );
    let no_repeat = event_error.as_ref().map_or_else(
        || no_repeat_from_events(&day_events, min_samples),
        |error| event_ledger_unavailable("tool.execution.finalized", error),
    );
    let proactive = proactive_from_cron_runs(config, window, min_samples);
    let learning = MetricResult::unavailable(serde_json::json!({
        "source": "retrieval_outcome",
        "reason": "no durable retrieval usefulness outcome is recorded yet",
        "required_evidence": "retrieval selection linked to a later successful terminal outcome"
    }));
    let efficiency = event_error.as_ref().map_or_else(
        || efficiency_from_events(&day_events, &baseline_events, min_samples),
        |error| event_ledger_unavailable("turn.finalized", error),
    );

    let metrics = [
        (&task_quality, WEIGHT_TASK_QUALITY),
        (&no_repeat, WEIGHT_NO_REPEAT),
        (&proactive, WEIGHT_PROACTIVE),
        (&learning, WEIGHT_LEARNING),
        (&efficiency, WEIGHT_EFFICIENCY),
    ];
    let coverage: f64 = metrics
        .iter()
        .filter(|(metric, _)| metric.score.is_some())
        .map(|(_, weight)| *weight)
        .sum();
    let coverage = if coverage == 0.0 { 0.0 } else { coverage };
    let weighted_score: f64 = metrics
        .iter()
        .filter_map(|(metric, weight)| metric.score.map(|score| score * weight))
        .sum();
    let min_coverage = config.self_system.fitness.min_coverage.clamp(0.0, 1.0);
    let final_score = (coverage >= min_coverage && coverage > 0.0).then(|| clamp_0_1(weighted_score / coverage));
    let confidence = if coverage > 0.0 {
        clamp_0_1(
            metrics
                .iter()
                .filter(|(metric, _)| metric.score.is_some())
                .map(|(metric, weight)| metric.confidence * weight)
                .sum::<f64>()
                / coverage
                * coverage,
        )
    } else {
        0.0
    };
    let status = if final_score.is_none() {
        FitnessReportStatus::InsufficientData
    } else if coverage >= 0.80 {
        FitnessReportStatus::Final
    } else {
        FitnessReportStatus::Provisional
    };

    Ok(FitnessReport {
        version: FITNESS_REPORT_VERSION.to_string(),
        status,
        window: FitnessWindow {
            date: window.day.to_string(),
            timezone: config.self_system.fitness.timezone.clone(),
            start: window.start.to_rfc3339(),
            end: window.end.to_rfc3339(),
        },
        subscores: FitnessSubscores {
            task_quality: task_quality.score,
            no_repeat: no_repeat.score,
            proactive: proactive.score,
            learning: learning.score,
            efficiency: efficiency.score,
        },
        metric_status: FitnessMetricStatus {
            task_quality: task_quality.status,
            no_repeat: no_repeat.status,
            proactive: proactive.status,
            learning: learning.status,
            efficiency: efficiency.status,
        },
        weights: FitnessWeights::default(),
        final_score,
        confidence,
        coverage: clamp_0_1(coverage),
        evidence: FitnessEvidence {
            task_quality: task_quality.evidence,
            no_repeat: no_repeat.evidence,
            proactive: proactive.evidence,
            learning: learning.evidence,
            efficiency: efficiency.evidence,
        },
        generated_at: Utc::now().to_rfc3339(),
    })
}

fn event_ledger_unavailable(event_type: &str, error: &str) -> MetricResult {
    MetricResult::unavailable(serde_json::json!({
        "source": format!("message_events.{event_type}"),
        "error": error
    }))
}

async fn load_evidence_events(
    memory: &dyn Memory,
    config: &Config,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<MessageEvent>> {
    let workspace_id = config.workspace_dir.to_string_lossy().to_string();
    let principal = MemoryPrincipal {
        workspace_id: workspace_id.clone(),
        agent_id: Some("self_system".to_string()),
        ..MemoryPrincipal::default()
    };
    let worker_workspace_root = config
        .sessions_spawn
        .worker_workspace_root
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                config.workspace_dir.join(path)
            }
        })
        .unwrap_or_else(|| config.workspace_dir.join("workers"));
    let worker_workspace_root = worker_workspace_root.to_string_lossy().to_string();
    let output = memory
        .list_message_events_time_range(
            &principal,
            Some(&worker_workspace_root),
            &start.to_rfc3339(),
            &end.to_rfc3339(),
            MAX_EVENT_SCAN,
        )
        .await
        .context("failed to read fitness evidence from message event ledger")?
        .into_iter()
        .filter(|event| {
            event.workspace_id == workspace_id
                || event.workspace_id == worker_workspace_root
                || std::path::Path::new(&event.workspace_id).starts_with(&worker_workspace_root)
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        output.len() < MAX_EVENT_SCAN,
        "fitness evidence query exceeded {MAX_EVENT_SCAN} events"
    );
    Ok(output)
}

fn task_quality_from_events(events: &[&MessageEvent], min_samples: usize) -> MetricResult {
    let mut completed = 0_usize;
    let mut failed = 0_usize;
    let mut cancelled = 0_usize;
    for event in events.iter().filter(|event| event.event_type == "turn.finalized") {
        match json_string(event, "/status").as_deref() {
            Some("completed" | "silent") => completed += 1,
            Some("failed") => failed += 1,
            Some("cancelled") => cancelled += 1,
            _ => {}
        }
    }
    let samples = completed + failed;
    let evidence = serde_json::json!({
        "source": "message_events.turn.finalized",
        "completed": completed,
        "failed": failed,
        "excluded_cancelled": cancelled,
        "samples": samples,
        "min_samples": min_samples
    });
    if samples < min_samples {
        return MetricResult::insufficient(evidence);
    }
    MetricResult::available(
        completed as f64 / samples as f64,
        sample_confidence(samples, min_samples),
        evidence,
    )
}

fn no_repeat_from_events(events: &[&MessageEvent], min_samples: usize) -> MetricResult {
    let mut tool_runs = 0_usize;
    let mut failed_runs = 0_usize;
    let mut repeated_failures = 0_usize;
    let mut seen_failures = HashSet::new();
    for event in events
        .iter()
        .filter(|event| event.event_type == "tool.execution.finalized")
    {
        if json_bool(event, "/outcome/replayed") == Some(true) {
            continue;
        }
        tool_runs += 1;
        let status = json_string(event, "/outcome/status").unwrap_or_default();
        if !matches!(status.as_str(), "failed" | "indeterminate") {
            continue;
        }
        failed_runs += 1;
        let fingerprint = format!(
            "{}|{}|{}|{}",
            event.session_key.as_deref().unwrap_or(""),
            json_string(event, "/reservation/capability").unwrap_or_default(),
            json_string(event, "/reservation/input_sha256").unwrap_or_default(),
            normalized_error(json_string(event, "/outcome/error").as_deref().unwrap_or(""))
        );
        if !seen_failures.insert(fingerprint) {
            repeated_failures += 1;
        }
    }
    let evidence = serde_json::json!({
        "source": "message_events.tool.execution.finalized",
        "tool_runs": tool_runs,
        "failed_runs": failed_runs,
        "repeated_identical_failures": repeated_failures,
        "fingerprint": "session+capability+input_sha256+normalized_error",
        "min_samples": min_samples
    });
    if tool_runs < min_samples {
        return MetricResult::insufficient(evidence);
    }
    let score = 1.0 - repeated_failures as f64 / tool_runs as f64;
    MetricResult::available(score, sample_confidence(tool_runs, min_samples), evidence)
}

fn proactive_from_cron_runs(config: &Config, window: WindowBounds, min_samples: usize) -> MetricResult {
    let jobs = match cron::list_jobs(config) {
        Ok(jobs) => jobs,
        Err(error) => {
            return MetricResult::unavailable(serde_json::json!({
                "source": "cron_runs",
                "error": error.to_string()
            }));
        }
    };
    let proactive_jobs: Vec<_> = jobs.into_iter().filter(is_proactive_job).collect();
    if proactive_jobs.is_empty() {
        return MetricResult::unavailable(serde_json::json!({
            "source": "cron_runs",
            "reason": "no enabled heartbeat or proactive jobs configured"
        }));
    }
    let mut runs = 0_usize;
    let mut successes = 0_usize;
    for job in &proactive_jobs {
        for run in cron::list_runs(config, &job.id, config.cron.max_run_history as usize).unwrap_or_default() {
            if run.started_at < window.start || run.started_at >= window.end {
                continue;
            }
            runs += 1;
            if matches!(run.status.as_str(), "ok" | "succeeded") {
                successes += 1;
            }
        }
    }
    let evidence = serde_json::json!({
        "source": "cron_runs",
        "jobs": proactive_jobs.len(),
        "runs": runs,
        "successes": successes,
        "min_samples": min_samples
    });
    if runs < min_samples {
        return MetricResult::insufficient(evidence);
    }
    MetricResult::available(
        successes as f64 / runs as f64,
        sample_confidence(runs, min_samples),
        evidence,
    )
}

fn efficiency_from_events(current: &[&MessageEvent], baseline: &[&MessageEvent], min_samples: usize) -> MetricResult {
    let current_samples = successful_turn_efficiency_samples(current);
    let baseline_samples = successful_turn_efficiency_samples(baseline);
    let evidence = serde_json::json!({
        "source": "message_events.turn.finalized",
        "basis": "successful-turn token and latency medians versus prior 30 days",
        "current_samples": current_samples.len(),
        "baseline_samples": baseline_samples.len(),
        "min_samples": min_samples,
        "cost_note": "zero local API price is not treated as perfect efficiency"
    });
    if current_samples.len() < min_samples || baseline_samples.len() < min_samples {
        return MetricResult::insufficient(evidence);
    }
    let current_tokens = median(current_samples.iter().map(|sample| sample.0).collect());
    let baseline_tokens = median(baseline_samples.iter().map(|sample| sample.0).collect());
    let current_latency = median(current_samples.iter().map(|sample| sample.1).collect());
    let baseline_latency = median(baseline_samples.iter().map(|sample| sample.1).collect());
    let token_score = relative_efficiency(current_tokens, baseline_tokens);
    let latency_score = relative_efficiency(current_latency, baseline_latency);
    MetricResult::available(
        f64::midpoint(token_score, latency_score),
        sample_confidence(current_samples.len(), min_samples),
        serde_json::json!({
            "source": "message_events.turn.finalized",
            "current_samples": current_samples.len(),
            "baseline_samples": baseline_samples.len(),
            "current_median_tokens": current_tokens,
            "baseline_median_tokens": baseline_tokens,
            "current_median_latency_ms": current_latency,
            "baseline_median_latency_ms": baseline_latency,
            "token_score": token_score,
            "latency_score": latency_score
        }),
    )
}

fn successful_turn_efficiency_samples(events: &[&MessageEvent]) -> Vec<(f64, f64)> {
    events
        .iter()
        .filter(|event| event.event_type == "turn.finalized")
        .filter(|event| matches!(json_string(event, "/status").as_deref(), Some("completed" | "silent")))
        .filter_map(|event| {
            let value = raw_json(event)?;
            let tokens = value.pointer("/usage_settlement/total_tokens")?.as_u64()? as f64;
            let started = value.pointer("/telemetry/started_at")?.as_str()?;
            let finished = value.pointer("/telemetry/finished_at")?.as_str()?;
            let started = DateTime::parse_from_rfc3339(started).ok()?;
            let finished = DateTime::parse_from_rfc3339(finished).ok()?;
            let latency = (finished - started).num_milliseconds().max(0) as f64;
            Some((tokens, latency))
        })
        .collect()
}

fn relative_efficiency(current: f64, baseline: f64) -> f64 {
    if current <= baseline || current <= f64::EPSILON {
        1.0
    } else if baseline <= f64::EPSILON {
        0.0
    } else {
        clamp_0_1(baseline / current)
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    let right = values.get(middle).copied().unwrap_or_default();
    if values.len() % 2 == 0 {
        let left = values.get(middle.saturating_sub(1)).copied().unwrap_or_default();
        f64::midpoint(left, right)
    } else {
        right
    }
}

fn raw_json(event: &MessageEvent) -> Option<serde_json::Value> {
    event
        .raw_payload_json
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
}

fn json_string(event: &MessageEvent, pointer: &str) -> Option<String> {
    raw_json(event)?
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .map(str::to_ascii_lowercase)
}

fn json_bool(event: &MessageEvent, pointer: &str) -> Option<bool> {
    raw_json(event)?.pointer(pointer).and_then(serde_json::Value::as_bool)
}

fn event_time(event: &MessageEvent) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&event.created_at)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn normalized_error(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(256)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn is_proactive_job(job: &cron::CronJob) -> bool {
    if !job.enabled {
        return false;
    }
    let name = job.name.as_deref().unwrap_or_default().to_ascii_lowercase();
    let command = job.command.to_ascii_lowercase();
    let prompt = job.prompt.as_deref().unwrap_or_default().to_ascii_lowercase();
    ["heartbeat", "proactive", "xin"]
        .iter()
        .any(|needle| name.contains(needle) || command.contains(needle) || prompt.contains(needle))
}

fn sample_confidence(samples: usize, min_samples: usize) -> f64 {
    (samples as f64 / min_samples.saturating_mul(2).max(1) as f64).clamp(0.0, 1.0)
}

pub(crate) fn validate_fitness_config(config: &FitnessConfig) -> Result<()> {
    config
        .timezone
        .parse::<Tz>()
        .with_context(|| format!("invalid self_system.fitness.timezone: {}", config.timezone))?;
    parse_run_at(&config.run_at)?;
    anyhow::ensure!(
        config.min_coverage.is_finite() && (0.0..=1.0).contains(&config.min_coverage),
        "self_system.fitness.min_coverage must be between 0 and 1"
    );
    anyhow::ensure!(
        config.min_samples > 0,
        "self_system.fitness.min_samples must be greater than zero"
    );
    anyhow::ensure!(
        config.retention_days > 0,
        "self_system.fitness.retention_days must be greater than zero"
    );
    anyhow::ensure!(
        config.max_backfill_days > 0,
        "self_system.fitness.max_backfill_days must be greater than zero"
    );
    Ok(())
}

pub(crate) fn latest_closed_window(now: DateTime<Utc>, config: &FitnessConfig) -> Result<WindowBounds> {
    let timezone = config
        .timezone
        .parse::<Tz>()
        .with_context(|| format!("invalid self_system.fitness.timezone: {}", config.timezone))?;
    let day = now.with_timezone(&timezone).date_naive() - Duration::days(1);
    window_for_day(day, timezone)
}

pub(crate) fn configured_window_for_day(day: NaiveDate, config: &FitnessConfig) -> Result<WindowBounds> {
    let timezone = config
        .timezone
        .parse::<Tz>()
        .with_context(|| format!("invalid self_system.fitness.timezone: {}", config.timezone))?;
    window_for_day(day, timezone)
}

pub(crate) fn next_scheduled_run(now: DateTime<Utc>, config: &FitnessConfig) -> Result<DateTime<Utc>> {
    let timezone = config
        .timezone
        .parse::<Tz>()
        .with_context(|| format!("invalid self_system.fitness.timezone: {}", config.timezone))?;
    let run_at = parse_run_at(&config.run_at)?;
    let local_now = now.with_timezone(&timezone);
    let mut day = local_now.date_naive();
    let mut scheduled = resolve_local_datetime(timezone, day.and_time(run_at))?;
    if scheduled <= now {
        day += Duration::days(1);
        scheduled = resolve_local_datetime(timezone, day.and_time(run_at))?;
    }
    Ok(scheduled)
}

fn window_for_day(day: NaiveDate, timezone: Tz) -> Result<WindowBounds> {
    let start = resolve_local_datetime(
        timezone,
        day.and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow::anyhow!("failed to build fitness window start"))?,
    )?;
    let next_day = day + Duration::days(1);
    let end = resolve_local_datetime(
        timezone,
        next_day
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow::anyhow!("failed to build fitness window end"))?,
    )?;
    Ok(WindowBounds { day, start, end })
}

fn resolve_local_datetime(timezone: Tz, local: NaiveDateTime) -> Result<DateTime<Utc>> {
    match timezone.from_local_datetime(&local) {
        LocalResult::Single(value) => Ok(value.with_timezone(&Utc)),
        LocalResult::Ambiguous(first, _) => Ok(first.with_timezone(&Utc)),
        LocalResult::None => anyhow::bail!("local fitness schedule time does not exist: {local} {timezone}"),
    }
}

fn parse_run_at(raw: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(raw.trim(), "%H:%M")
        .with_context(|| format!("invalid self_system.fitness.run_at '{raw}'; expected HH:MM"))
}

const fn clamp_0_1(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::memory::{MemoryCategory, MemoryVisibility, MessageEventSource, SqliteMemory};
    use chrono::TimeZone;

    fn event(id: i64, event_type: &str, created_at: DateTime<Utc>, payload: serde_json::Value) -> MessageEvent {
        MessageEvent {
            id,
            event_id: format!("event-{id}"),
            idempotency_key: None,
            workspace_id: "/workspace".to_string(),
            owner_id: None,
            source: MessageEventSource::Other("test".to_string()),
            channel: None,
            session_key: Some("session-1".to_string()),
            parent_session_key: None,
            run_id: None,
            parent_run_id: None,
            agent_id: None,
            persona_id: None,
            sender: None,
            recipient: None,
            role: "system".to_string(),
            event_type: event_type.to_string(),
            subject: None,
            goal_id: None,
            causation_event_id: None,
            correlation_id: None,
            attempt_id: None,
            lease_epoch: None,
            config_generation_id: None,
            config_source_revision: None,
            content: String::new(),
            content_hash: None,
            raw_payload_json: Some(payload.to_string()),
            visibility: MemoryVisibility::Workspace,
            created_at: created_at.to_rfc3339(),
            updated_at: created_at.to_rfc3339(),
        }
    }

    #[test]
    fn task_quality_uses_terminal_outcomes_and_excludes_cancellation() {
        let at = Utc.with_ymd_and_hms(2026, 9, 10, 1, 0, 0).unwrap();
        let events = [
            event(1, "turn.finalized", at, serde_json::json!({"status":"completed"})),
            event(2, "turn.finalized", at, serde_json::json!({"status":"failed"})),
            event(3, "turn.finalized", at, serde_json::json!({"status":"cancelled"})),
        ];
        let refs = events.iter().collect::<Vec<_>>();
        let metric = task_quality_from_events(&refs, 2);
        assert_eq!(metric.status, MetricStatus::Available);
        assert_eq!(metric.score, Some(0.5));
        assert_eq!(metric.evidence.get("excluded_cancelled"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn no_repeat_detects_identical_failed_tool_action() {
        let at = Utc.with_ymd_and_hms(2026, 9, 10, 1, 0, 0).unwrap();
        let payload = serde_json::json!({
            "reservation": {"capability":"shell", "input_sha256":"abc"},
            "outcome": {"status":"failed", "error":"boom", "replayed":false}
        });
        let events = [
            event(1, "tool.execution.finalized", at, payload.clone()),
            event(2, "tool.execution.finalized", at, payload),
        ];
        let refs = events.iter().collect::<Vec<_>>();
        let metric = no_repeat_from_events(&refs, 2);
        assert_eq!(metric.status, MetricStatus::Available);
        assert_eq!(metric.score, Some(0.5));
        assert_eq!(
            metric.evidence.get("repeated_identical_failures"),
            Some(&serde_json::json!(1))
        );
    }

    #[test]
    fn missing_metric_is_not_replaced_by_a_fallback_score() {
        let metric = task_quality_from_events(&[], 5);
        assert_eq!(metric.status, MetricStatus::InsufficientData);
        assert_eq!(metric.score, None);
    }

    #[test]
    fn closed_window_uses_configured_calendar_timezone() {
        let config = FitnessConfig {
            timezone: "Asia/Tbilisi".to_string(),
            ..FitnessConfig::default()
        };
        let now = Utc.with_ymd_and_hms(2026, 9, 11, 8, 0, 0).unwrap();
        let window = latest_closed_window(now, &config).unwrap();
        assert_eq!(window.day.to_string(), "2026-09-10");
        assert_eq!(window.start, Utc.with_ymd_and_hms(2026, 9, 9, 20, 0, 0).unwrap());
        assert_eq!(window.end, Utc.with_ymd_and_hms(2026, 9, 10, 20, 0, 0).unwrap());
    }

    #[test]
    fn next_run_is_calendar_based_without_startup_tick() {
        let config = FitnessConfig {
            timezone: "Asia/Tbilisi".to_string(),
            run_at: "00:10".to_string(),
            ..FitnessConfig::default()
        };
        let now = Utc.with_ymd_and_hms(2026, 9, 11, 8, 0, 0).unwrap();
        assert_eq!(
            next_scheduled_run(now, &config).unwrap(),
            Utc.with_ymd_and_hms(2026, 9, 11, 20, 10, 0).unwrap()
        );
    }

    #[tokio::test]
    async fn persisted_report_never_enters_memory_or_markdown_projection() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            workspace_dir: tmp.path().to_path_buf(),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        let memory = SqliteMemory::new(&config.workspace_dir).unwrap();
        memory
            .store("keep", "sentinel", MemoryCategory::Core, None)
            .await
            .unwrap();
        let day = Utc::now().date_naive() - Duration::days(1);
        let window = window_for_day(day, chrono_tz::UTC).unwrap();

        let report = run_fitness_report_for_window(&config, window).await.unwrap();

        assert_eq!(report.window.date, day.to_string());
        assert_eq!(report.coverage, 0.0);
        assert!(!report.coverage.is_sign_negative());
        assert!(FitnessStore::new(&config.workspace_dir).load(day).unwrap().is_some());
        assert!(
            memory
                .get(&format!("{LEGACY_TEST_PREFIX}{day}"))
                .await
                .unwrap()
                .is_none()
        );
        let markdown = std::fs::read_to_string(config.workspace_dir.join("MEMORY.md")).unwrap();
        assert!(!markdown.contains("self/fitness/daily/"));
        assert!(markdown.contains("sentinel"));
    }

    const LEGACY_TEST_PREFIX: &str = "self/fitness/daily/";
}
