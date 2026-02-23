use crate::config::Config;
use crate::cron::{self, CronRun};
use crate::health::{self, HealthSnapshot};
use crate::memory::{Memory, MemoryCategory};
use crate::self_system::decision_log::{log_change_outcome, log_change_proposal};
use crate::self_system::experiment::{complete_experiment, rollback_experiment, start_experiment};
use crate::self_system::fitness::{run_fitness_report, FitnessReport};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const EVOLUTION_STATE_KEY: &str = "self/evolution/state";
const EVOLUTION_CYCLE_PREFIX: &str = "self/evolution/cycles/";
const EVOLUTION_ALERT_PREFIX: &str = "self/evolution/alerts/";
const FITNESS_PREFIX: &str = "self/fitness/daily/";
const TREND_WINDOW: usize = 5;
const REGRESSION_EPSILON: f64 = 0.01;
const MAX_CRON_RUNS: usize = 50;
const MAX_CONSECUTIVE_REGRESSED: u8 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ChangeTarget {
    ConfigFile { path: String },
    CronFile { path: String },
    WorkspaceFile { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum ChangeOperation {
    Append { content: String },
    Replace { from: String, to: String },
    Write { content: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionProposal {
    pub id: String,
    pub summary: String,
    pub rationale: String,
    pub risk_level: RiskLevel,
    pub target: ChangeTarget,
    pub operation: ChangeOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionSignals {
    pub memory_count: usize,
    pub health_components: usize,
    pub health_error_components: usize,
    pub cron_runs: usize,
    pub cron_failure_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitnessTrend {
    pub window: usize,
    pub previous_average: f64,
    pub latest_score: f64,
    pub is_declining: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    Improved,
    Unchanged,
    Regressed,
    Skipped,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CycleOutcome {
    Applied,
    Paused,
    Halted,
    NoAction,
    ApprovalRequired,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionValidation {
    pub status: ValidationStatus,
    pub before_score: f64,
    pub after_score: f64,
    pub delta: f64,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionCycle {
    pub id: String,
    pub started_at: String,
    pub finished_at: String,
    pub signals: EvolutionSignals,
    pub trend: FitnessTrend,
    pub proposal: Option<EvolutionProposal>,
    pub validation: EvolutionValidation,
    pub outcome: CycleOutcome,
    pub alert: Option<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EvolutionState {
    pub paused: bool,
    pub halted: bool,
    pub consecutive_regressed: u8,
    pub last_cycle_id: Option<String>,
    pub last_updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct AppliedChange {
    path: PathBuf,
    existed_before: bool,
    previous_content: Option<String>,
}

#[async_trait]
pub trait HealthSource: Send + Sync {
    fn snapshot(&self) -> HealthSnapshot;
}

pub struct RuntimeHealth;

#[async_trait]
impl HealthSource for RuntimeHealth {
    fn snapshot(&self) -> HealthSnapshot {
        health::snapshot()
    }
}

#[async_trait]
pub trait CronStore: Send + Sync {
    async fn list_recent_runs(&self, limit: usize) -> anyhow::Result<Vec<CronRun>>;
}

pub struct RuntimeCronStore {
    config: Config,
}

impl RuntimeCronStore {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

#[async_trait]
impl CronStore for RuntimeCronStore {
    async fn list_recent_runs(&self, limit: usize) -> anyhow::Result<Vec<CronRun>> {
        let jobs = cron::list_jobs(&self.config)?;
        let per_job_limit = limit.max(1);
        let mut all_runs = Vec::new();

        for job in jobs {
            let mut runs = cron::list_runs(&self.config, &job.id, per_job_limit)?;
            all_runs.append(&mut runs);
        }

        all_runs.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
        all_runs.truncate(limit.max(1));
        Ok(all_runs)
    }
}

pub async fn run_evolution_cycle(
    memory: &dyn Memory,
    health: &dyn HealthSource,
    cron_store: &dyn CronStore,
) -> EvolutionCycle {
    let started_at = Utc::now().to_rfc3339();
    let mut errors = Vec::new();

    let mut state = load_state(memory).await.unwrap_or_else(|error| {
        errors.push(error.to_string());
        EvolutionState::default()
    });

    let cycle_id = Uuid::new_v4().to_string();
    let signals = collect_signals(memory, health, cron_store, &mut errors).await;

    let before_score = current_fitness_score(memory, &mut errors).await;
    let trend = build_trend(memory, before_score, &mut errors).await;

    if state.paused {
        let cycle = EvolutionCycle {
            id: cycle_id,
            started_at: started_at.clone(),
            finished_at: Utc::now().to_rfc3339(),
            signals,
            trend,
            proposal: None,
            validation: EvolutionValidation {
                status: ValidationStatus::Skipped,
                before_score,
                after_score: before_score,
                delta: 0.0,
                notes: "evolution paused".to_string(),
            },
            outcome: CycleOutcome::Paused,
            alert: None,
            errors,
        };
        persist_cycle_and_state(memory, &cycle, &mut state).await;
        return cycle;
    }

    if state.halted {
        let cycle = EvolutionCycle {
            id: cycle_id,
            started_at: started_at.clone(),
            finished_at: Utc::now().to_rfc3339(),
            signals,
            trend,
            proposal: None,
            validation: EvolutionValidation {
                status: ValidationStatus::Skipped,
                before_score,
                after_score: before_score,
                delta: 0.0,
                notes: "evolution halted due to repeated regressions".to_string(),
            },
            outcome: CycleOutcome::Halted,
            alert: None,
            errors,
        };
        persist_cycle_and_state(memory, &cycle, &mut state).await;
        return cycle;
    }

    let proposal = generate_proposal(&signals, &trend);
    let Some(proposal) = proposal else {
        let cycle = EvolutionCycle {
            id: cycle_id,
            started_at: started_at.clone(),
            finished_at: Utc::now().to_rfc3339(),
            signals,
            trend,
            proposal: None,
            validation: EvolutionValidation {
                status: ValidationStatus::Skipped,
                before_score,
                after_score: before_score,
                delta: 0.0,
                notes: "no proposal generated".to_string(),
            },
            outcome: CycleOutcome::NoAction,
            alert: None,
            errors,
        };
        persist_cycle_and_state(memory, &cycle, &mut state).await;
        return cycle;
    };

    if let Err(error) = log_change_proposal(
        memory,
        &proposal.id,
        &proposal.summary,
        "improve fitness trend",
    )
    .await
    {
        errors.push(format!("proposal log failed: {error}"));
    }

    if proposal.risk_level == RiskLevel::High {
        let cycle = EvolutionCycle {
            id: cycle_id,
            started_at: started_at.clone(),
            finished_at: Utc::now().to_rfc3339(),
            signals,
            trend,
            proposal: Some(proposal),
            validation: EvolutionValidation {
                status: ValidationStatus::Skipped,
                before_score,
                after_score: before_score,
                delta: 0.0,
                notes: "high-risk proposal recorded for manual approval".to_string(),
            },
            outcome: CycleOutcome::ApprovalRequired,
            alert: None,
            errors,
        };
        persist_cycle_and_state(memory, &cycle, &mut state).await;
        return cycle;
    }

    let experiment = start_experiment(
        memory,
        "self-evolution-cycle",
        before_score,
        &proposal.summary,
    )
    .await;

    let mut experiment_record = None;
    match experiment {
        Ok(record) => {
            experiment_record = Some(record);
        }
        Err(error) => {
            errors.push(format!("failed to start experiment: {error}"));
        }
    }

    let apply_result = apply_change(&proposal).await;
    let (validation, outcome, alert) = match apply_result {
        Ok(applied_change) => {
            let after_score = current_fitness_score(memory, &mut errors).await;
            let delta = after_score - before_score;
            let status = if delta > REGRESSION_EPSILON {
                ValidationStatus::Improved
            } else if delta < -REGRESSION_EPSILON {
                ValidationStatus::Regressed
            } else {
                ValidationStatus::Unchanged
            };

            if let Some(record) = experiment_record.as_ref() {
                if status == ValidationStatus::Regressed {
                    if let Err(error) = rollback_file_change(&applied_change).await {
                        errors.push(format!("rollback file change failed: {error}"));
                    }
                    if let Err(error) =
                        rollback_experiment(memory, &record.id, "fitness regressed").await
                    {
                        errors.push(format!("rollback experiment failed: {error}"));
                    }
                } else if let Err(error) =
                    complete_experiment(memory, &record.id, after_score).await
                {
                    errors.push(format!("complete experiment failed: {error}"));
                }
            }

            if let Err(error) =
                log_change_outcome(memory, &proposal.id, "cycle validation", delta).await
            {
                errors.push(format!("outcome log failed: {error}"));
            }

            let (next_state, maybe_alert) = apply_validation_to_state(state, &status);
            state = next_state;
            let cycle_outcome = if status == ValidationStatus::Regressed {
                CycleOutcome::Failed
            } else {
                CycleOutcome::Applied
            };

            (
                EvolutionValidation {
                    status,
                    before_score,
                    after_score,
                    delta,
                    notes: "single change executed".to_string(),
                },
                cycle_outcome,
                maybe_alert,
            )
        }
        Err(error) => {
            errors.push(format!("apply change failed: {error}"));
            (
                EvolutionValidation {
                    status: ValidationStatus::Error,
                    before_score,
                    after_score: before_score,
                    delta: 0.0,
                    notes: "change apply failed".to_string(),
                },
                CycleOutcome::Failed,
                None,
            )
        }
    };

    if let Some(message) = alert.as_deref() {
        if let Err(error) = persist_alert(memory, message).await {
            errors.push(format!("persist alert failed: {error}"));
        }
    }

    let cycle = EvolutionCycle {
        id: cycle_id,
        started_at,
        finished_at: Utc::now().to_rfc3339(),
        signals,
        trend,
        proposal: Some(proposal),
        validation,
        outcome,
        alert,
        errors,
    };

    persist_cycle_and_state(memory, &cycle, &mut state).await;
    cycle
}

pub async fn get_evolution_history(memory: &dyn Memory, limit: usize) -> Vec<EvolutionCycle> {
    let Ok(entries) = memory.list(Some(&MemoryCategory::Core), None).await else {
        return Vec::new();
    };

    let mut cycles = entries
        .into_iter()
        .filter(|entry| entry.key.starts_with(EVOLUTION_CYCLE_PREFIX))
        .filter_map(|entry| serde_json::from_str::<EvolutionCycle>(&entry.content).ok())
        .collect::<Vec<_>>();

    cycles.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    cycles.truncate(limit.max(1));
    cycles
}

pub async fn pause_evolution(memory: &dyn Memory) -> anyhow::Result<()> {
    let mut state = load_state(memory).await.unwrap_or_default();
    state.paused = true;
    state.last_updated_at = Some(Utc::now().to_rfc3339());
    store_state(memory, &state).await
}

pub async fn resume_evolution(memory: &dyn Memory) -> anyhow::Result<()> {
    let mut state = load_state(memory).await.unwrap_or_default();
    state.paused = false;
    state.last_updated_at = Some(Utc::now().to_rfc3339());
    store_state(memory, &state).await
}

fn generate_proposal(
    signals: &EvolutionSignals,
    trend: &FitnessTrend,
) -> Option<EvolutionProposal> {
    if !trend.is_declining {
        return None;
    }

    if signals.health_error_components >= 2 {
        return Some(EvolutionProposal {
            id: Uuid::new_v4().to_string(),
            summary: "tune scheduler config guardrails".to_string(),
            rationale: "multiple health components are in error and fitness is declining"
                .to_string(),
            risk_level: RiskLevel::High,
            target: ChangeTarget::ConfigFile {
                path: "config.toml".to_string(),
            },
            operation: ChangeOperation::Append {
                content: "\n# self-evolution: investigate repeated health errors\n".to_string(),
            },
        });
    }

    if signals.cron_failure_ratio >= 0.3 {
        return Some(EvolutionProposal {
            id: Uuid::new_v4().to_string(),
            summary: "add cron recovery note".to_string(),
            rationale: "cron failure ratio indicates unstable autonomous tasks".to_string(),
            risk_level: RiskLevel::Medium,
            target: ChangeTarget::CronFile {
                path: "HEARTBEAT.md".to_string(),
            },
            operation: ChangeOperation::Append {
                content: "\n- self-evolution: investigate latest cron failures\n".to_string(),
            },
        });
    }

    Some(EvolutionProposal {
        id: Uuid::new_v4().to_string(),
        summary: "append self-evolution workspace note".to_string(),
        rationale: "fitness trend declined without severe health or cron signals".to_string(),
        risk_level: RiskLevel::Low,
        target: ChangeTarget::WorkspaceFile {
            path: "SELF_EVOLUTION.md".to_string(),
        },
        operation: ChangeOperation::Append {
            content: format!(
                "\n- {}: review trend and collect more learning evidence\n",
                Utc::now().to_rfc3339()
            ),
        },
    })
}

async fn collect_signals(
    memory: &dyn Memory,
    health: &dyn HealthSource,
    cron_store: &dyn CronStore,
    errors: &mut Vec<String>,
) -> EvolutionSignals {
    let memory_count = memory.count().await.unwrap_or_else(|error| {
        errors.push(format!("memory count failed: {error}"));
        0
    });

    let snapshot = health.snapshot();
    let health_components = snapshot.components.len();
    let health_error_components = snapshot
        .components
        .values()
        .filter(|component| component.status != "ok")
        .count();

    let runs = cron_store
        .list_recent_runs(MAX_CRON_RUNS)
        .await
        .unwrap_or_else(|error| {
            errors.push(format!("cron runs lookup failed: {error}"));
            Vec::new()
        });

    let cron_runs = runs.len();
    let failed = runs
        .iter()
        .filter(|run| !run.status.eq_ignore_ascii_case("ok"))
        .count();

    let cron_failure_ratio = if cron_runs == 0 {
        0.0
    } else {
        failed as f64 / cron_runs as f64
    };

    EvolutionSignals {
        memory_count,
        health_components,
        health_error_components,
        cron_runs,
        cron_failure_ratio,
    }
}

async fn current_fitness_score(memory: &dyn Memory, errors: &mut Vec<String>) -> f64 {
    let mut reports = load_recent_fitness_reports(memory, TREND_WINDOW).await;

    if reports.is_empty() {
        match run_fitness_report().await {
            Ok(report) => reports.push(report),
            Err(error) => errors.push(format!("fitness run failed: {error}")),
        }
    }

    reports
        .last()
        .map_or(0.5, |report| clamp_0_1(report.final_score))
}

async fn build_trend(
    memory: &dyn Memory,
    latest_score: f64,
    errors: &mut Vec<String>,
) -> FitnessTrend {
    let reports = load_recent_fitness_reports(memory, TREND_WINDOW).await;
    if reports.len() < 2 {
        return FitnessTrend {
            window: reports.len().max(1),
            previous_average: latest_score,
            latest_score,
            is_declining: false,
        };
    }

    let previous_slice = &reports[..reports.len().saturating_sub(1)];
    let previous_average = if previous_slice.is_empty() {
        latest_score
    } else {
        let sum: f64 = previous_slice.iter().map(|report| report.final_score).sum();
        sum / previous_slice.len() as f64
    };

    let is_declining = latest_score + REGRESSION_EPSILON < previous_average;

    if previous_slice.is_empty() {
        errors.push("trend analysis had insufficient previous scores".to_string());
    }

    FitnessTrend {
        window: reports.len(),
        previous_average,
        latest_score,
        is_declining,
    }
}

async fn load_recent_fitness_reports(memory: &dyn Memory, limit: usize) -> Vec<FitnessReport> {
    let Ok(entries) = memory.list(Some(&MemoryCategory::Core), None).await else {
        return Vec::new();
    };

    let mut reports = entries
        .into_iter()
        .filter(|entry| entry.key.starts_with(FITNESS_PREFIX))
        .filter_map(|entry| serde_json::from_str::<FitnessReport>(&entry.content).ok())
        .collect::<Vec<_>>();

    reports.sort_by(|a, b| a.window.end.cmp(&b.window.end));
    if reports.len() > limit {
        reports = reports.split_off(reports.len() - limit);
    }
    reports
}

fn apply_validation_to_state(
    mut state: EvolutionState,
    status: &ValidationStatus,
) -> (EvolutionState, Option<String>) {
    if *status == ValidationStatus::Regressed {
        state.consecutive_regressed = state.consecutive_regressed.saturating_add(1);
    } else if *status == ValidationStatus::Improved || *status == ValidationStatus::Unchanged {
        state.consecutive_regressed = 0;
    }

    if state.consecutive_regressed >= MAX_CONSECUTIVE_REGRESSED {
        state.halted = true;
        let alert = format!(
            "auto evolution halted after {} consecutive regressions",
            state.consecutive_regressed
        );
        return (state, Some(alert));
    }

    (state, None)
}

async fn apply_change(proposal: &EvolutionProposal) -> anyhow::Result<AppliedChange> {
    let path = proposal_path(&proposal.target);

    if is_protected_workspace_target(&proposal.target, &path) {
        anyhow::bail!("workspace file target SOUL.md is protected");
    }

    let previous_content = fs::read_to_string(&path).await.ok();
    let existed_before = previous_content.is_some();

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).await?;
        }
    }

    match &proposal.operation {
        ChangeOperation::Append { content } => {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await?;
            file.write_all(content.as_bytes()).await?;
        }
        ChangeOperation::Replace { from, to } => {
            let current = fs::read_to_string(&path).await?;
            if !current.contains(from) {
                anyhow::bail!("replace source token not found");
            }
            let updated = current.replacen(from, to, 1);
            fs::write(&path, updated).await?;
        }
        ChangeOperation::Write { content } => {
            fs::write(&path, content).await?;
        }
    }

    Ok(AppliedChange {
        path,
        existed_before,
        previous_content,
    })
}

async fn rollback_file_change(change: &AppliedChange) -> anyhow::Result<()> {
    if change.existed_before {
        if let Some(previous) = &change.previous_content {
            fs::write(&change.path, previous).await?;
        }
    } else if fs::metadata(&change.path).await.is_ok() {
        fs::remove_file(&change.path).await?;
    }

    Ok(())
}

fn is_protected_workspace_target(target: &ChangeTarget, path: &Path) -> bool {
    if !matches!(target, ChangeTarget::WorkspaceFile { .. }) {
        return false;
    }

    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("SOUL.md"))
}

fn proposal_path(target: &ChangeTarget) -> PathBuf {
    match target {
        ChangeTarget::ConfigFile { path }
        | ChangeTarget::CronFile { path }
        | ChangeTarget::WorkspaceFile { path } => PathBuf::from(path),
    }
}

async fn persist_alert(memory: &dyn Memory, alert: &str) -> anyhow::Result<()> {
    let key = format!(
        "{EVOLUTION_ALERT_PREFIX}{}",
        Utc::now().format("%Y%m%dT%H%M%S")
    );
    memory.store(&key, alert, MemoryCategory::Core, None).await
}

async fn persist_cycle_and_state(
    memory: &dyn Memory,
    cycle: &EvolutionCycle,
    state: &mut EvolutionState,
) {
    state.last_cycle_id = Some(cycle.id.clone());
    state.last_updated_at = Some(Utc::now().to_rfc3339());

    let cycle_key = format!(
        "{EVOLUTION_CYCLE_PREFIX}{}-{}",
        Utc::now().format("%Y%m%dT%H%M%S"),
        cycle.id
    );

    if let Ok(payload) = serde_json::to_string_pretty(cycle) {
        let _ = memory
            .store(&cycle_key, &payload, MemoryCategory::Core, None)
            .await;
    }

    let _ = store_state(memory, state).await;
}

async fn load_state(memory: &dyn Memory) -> anyhow::Result<EvolutionState> {
    let maybe_entry = memory.get(EVOLUTION_STATE_KEY).await?;
    let Some(entry) = maybe_entry else {
        return Ok(EvolutionState::default());
    };

    let state = serde_json::from_str::<EvolutionState>(&entry.content)?;
    Ok(state)
}

async fn store_state(memory: &dyn Memory, state: &EvolutionState) -> anyhow::Result<()> {
    let payload = serde_json::to_string_pretty(state)?;
    memory
        .store(EVOLUTION_STATE_KEY, &payload, MemoryCategory::Core, None)
        .await
}

fn clamp_0_1(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryEntry;
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::Utc;
    use std::collections::{BTreeMap, HashMap};
    use tempfile::tempdir;
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
                .filter(|entry| category.is_none_or(|kind| &entry.category == kind))
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

    struct StaticHealth {
        snapshot: HealthSnapshot,
    }

    #[async_trait]
    impl HealthSource for StaticHealth {
        fn snapshot(&self) -> HealthSnapshot {
            self.snapshot.clone()
        }
    }

    struct StaticCronStore {
        runs: Vec<CronRun>,
    }

    #[async_trait]
    impl CronStore for StaticCronStore {
        async fn list_recent_runs(&self, _limit: usize) -> anyhow::Result<Vec<CronRun>> {
            Ok(self.runs.clone())
        }
    }

    fn seeded_fitness(score: f64, day: &str) -> FitnessReport {
        FitnessReport {
            version: "p0-1".to_string(),
            window: crate::self_system::fitness::FitnessWindow {
                date: day.to_string(),
                start: format!("{day}T00:00:00Z"),
                end: format!("{day}T23:59:59Z"),
            },
            subscores: crate::self_system::fitness::FitnessSubscores {
                task_quality: score,
                no_repeat: score,
                proactive: score,
                learning: score,
                efficiency: score,
            },
            weights: crate::self_system::fitness::FitnessWeights {
                task_quality: 0.35,
                no_repeat: 0.25,
                proactive: 0.2,
                learning: 0.1,
                efficiency: 0.1,
            },
            final_score: score,
            confidence: 0.8,
            evidence: crate::self_system::fitness::FitnessEvidence::default(),
        }
    }

    async fn store_fitness(memory: &dyn Memory, day: &str, score: f64) {
        let key = format!("self/fitness/daily/{day}");
        let payload = serde_json::to_string_pretty(&seeded_fitness(score, day)).unwrap();
        memory
            .store(&key, &payload, MemoryCategory::Core, None)
            .await
            .unwrap();
    }

    fn ok_health() -> HealthSnapshot {
        let mut components = BTreeMap::new();
        components.insert(
            "runtime".to_string(),
            crate::health::ComponentHealth {
                status: "ok".to_string(),
                updated_at: Utc::now().to_rfc3339(),
                last_ok: Some(Utc::now().to_rfc3339()),
                last_error: None,
                restart_count: 0,
            },
        );
        HealthSnapshot {
            pid: 123,
            updated_at: Utc::now().to_rfc3339(),
            uptime_seconds: 10,
            components,
        }
    }

    #[tokio::test]
    async fn pause_and_resume_toggle_state() {
        let memory = TestMemory::new();

        pause_evolution(&memory).await.unwrap();
        let state = load_state(&memory).await.unwrap();
        assert!(state.paused);

        resume_evolution(&memory).await.unwrap();
        let state = load_state(&memory).await.unwrap();
        assert!(!state.paused);
    }

    #[tokio::test]
    async fn high_risk_proposal_is_not_auto_executed() {
        let memory = TestMemory::new();
        store_fitness(&memory, "2026-02-20", 0.9).await;
        store_fitness(&memory, "2026-02-21", 0.8).await;
        store_fitness(&memory, "2026-02-22", 0.6).await;

        let mut components = BTreeMap::new();
        components.insert(
            "provider".to_string(),
            crate::health::ComponentHealth {
                status: "error".to_string(),
                updated_at: Utc::now().to_rfc3339(),
                last_ok: None,
                last_error: Some("err".to_string()),
                restart_count: 1,
            },
        );
        components.insert(
            "runtime".to_string(),
            crate::health::ComponentHealth {
                status: "error".to_string(),
                updated_at: Utc::now().to_rfc3339(),
                last_ok: None,
                last_error: Some("err".to_string()),
                restart_count: 1,
            },
        );

        let health = StaticHealth {
            snapshot: HealthSnapshot {
                pid: 1,
                updated_at: Utc::now().to_rfc3339(),
                uptime_seconds: 1,
                components,
            },
        };

        let cron_store = StaticCronStore { runs: Vec::new() };
        let cycle = run_evolution_cycle(&memory, &health, &cron_store).await;

        assert_eq!(cycle.outcome, CycleOutcome::ApprovalRequired);
        assert_eq!(cycle.validation.status, ValidationStatus::Skipped);
    }

    #[tokio::test]
    async fn workspace_target_cannot_modify_soul_md() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("SOUL.md");
        fs::write(&path, "immutable").await.unwrap();

        let proposal = EvolutionProposal {
            id: Uuid::new_v4().to_string(),
            summary: "bad change".to_string(),
            rationale: "test".to_string(),
            risk_level: RiskLevel::Low,
            target: ChangeTarget::WorkspaceFile {
                path: path.to_string_lossy().to_string(),
            },
            operation: ChangeOperation::Append {
                content: "\nnew line".to_string(),
            },
        };

        let result = apply_change(&proposal).await;
        assert!(result.is_err());

        let content = fs::read_to_string(path).await.unwrap();
        assert_eq!(content, "immutable");
    }

    #[test]
    fn three_regressions_halt_auto_evolution() {
        let mut state = EvolutionState::default();

        let (s1, a1) = apply_validation_to_state(state, &ValidationStatus::Regressed);
        assert!(a1.is_none());
        assert_eq!(s1.consecutive_regressed, 1);

        let (s2, a2) = apply_validation_to_state(s1, &ValidationStatus::Regressed);
        assert!(a2.is_none());
        assert_eq!(s2.consecutive_regressed, 2);

        let (s3, a3) = apply_validation_to_state(s2, &ValidationStatus::Regressed);
        assert!(a3.is_some());
        assert!(s3.halted);
        assert_eq!(s3.consecutive_regressed, 3);

        state = s3;
        let (s4, _) = apply_validation_to_state(state, &ValidationStatus::Improved);
        assert_eq!(s4.consecutive_regressed, 0);
    }

    #[tokio::test]
    async fn cycle_executes_single_low_risk_change_when_declining() {
        let memory = TestMemory::new();
        store_fitness(&memory, "2026-02-20", 0.8).await;
        store_fitness(&memory, "2026-02-21", 0.7).await;
        store_fitness(&memory, "2026-02-22", 0.6).await;

        let health = StaticHealth {
            snapshot: ok_health(),
        };
        let cron_store = StaticCronStore { runs: Vec::new() };

        let cycle = run_evolution_cycle(&memory, &health, &cron_store).await;

        assert!(matches!(
            cycle.outcome,
            CycleOutcome::Applied | CycleOutcome::Failed
        ));
        assert!(cycle.proposal.is_some());
    }
}
