use crate::cron::{Schedule, next_run_for_schedule};
use crate::self_system::evolution::analyzer::{DailyDigest, EvolutionAnalyzer};
use crate::self_system::evolution::config::{EvolutionMode, SharedEvolutionConfig};
use crate::self_system::evolution::engine::EvolutionEngine;
use crate::self_system::evolution::pipeline::{
    CommitDecision, EvolutionPipeline, EvolutionTrigger, JudgeOutcome, PipelineRunReport,
};
use crate::self_system::evolution::record::EvolutionLayer;
use crate::self_system::evolution::rollback::CircuitBreaker;
use crate::self_system::evolution::safety_utils::atomic_write;
use anyhow::{Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

const SCHEDULED_LAYERS: [EvolutionLayer; 1] = [EvolutionLayer::Prompt];

const DAILY_DIGEST_CRON: &str = "0 2 * * *";
const EVOLUTION_CYCLE_CRON: &str = "0 3 */3 * *";

/// Persisted scheduler checkpoints used for cron due-time recovery.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SchedulerState {
    pub last_digest_at: Option<String>,
    pub next_digest_at: Option<String>,
    pub last_cycle_at: Option<String>,
    pub next_cycle_at: Option<String>,
}

/// Aggregated outcome of one scheduler tick.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SchedulerRunSummary {
    pub digest_ran: bool,
    pub cycle_ran: bool,
    pub digest: Option<DailyDigest>,
    pub layer_reports: Vec<PipelineRunReport>,
}

/// Scheduler that coordinates digest generation and layered evolution cycles.
pub struct EvolutionScheduler {
    shared_config: SharedEvolutionConfig,
    analyzer: Arc<EvolutionAnalyzer>,
    pipeline: EvolutionPipeline,
    state_path: PathBuf,
    circuit_breaker: CircuitBreaker,
    prompt_engine: Box<dyn EvolutionEngine>,
}

impl EvolutionScheduler {
    /// Create a scheduler with the lifecycle prompt proposer.
    pub fn new(
        shared_config: SharedEvolutionConfig,
        analyzer: Arc<EvolutionAnalyzer>,
        pipeline: EvolutionPipeline,
        state_path: impl AsRef<Path>,
        prompt_engine: Box<dyn EvolutionEngine>,
    ) -> Self {
        let cfg = shared_config.load_full();
        let breaker = CircuitBreaker::new(
            cfg.rollback.circuit_breaker_threshold,
            cfg.rollback.cooldown_after_rollback_hours,
        );

        Self {
            shared_config,
            analyzer,
            pipeline,
            state_path: state_path.as_ref().to_path_buf(),
            circuit_breaker: breaker,
            prompt_engine,
        }
    }

    /// The active generation's existing retention policy for skill experiences.
    pub fn experience_retention(&self) -> super::config::EvolutionRetentionConfig {
        self.shared_config.load_full().runtime.retention.clone()
    }

    /// Run daily digest and/or prompt proposal cycle when schedules are due.
    pub async fn run_scheduled(&mut self, now: DateTime<Utc>) -> Result<SchedulerRunSummary> {
        let mut state = match self.load_state().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to load scheduler state, using defaults");
                SchedulerState::default()
            }
        };
        ensure_next_schedule(&mut state, now)?;

        let mut summary = SchedulerRunSummary::default();

        if due(now, state.next_digest_at.as_deref()) {
            let digest = self.analyzer.generate_daily_digest(now).await?;
            summary.digest_ran = true;
            summary.digest = Some(digest);
            state.last_digest_at = Some(now.to_rfc3339());
            state.next_digest_at = Some(next_daily(now)?.to_rfc3339());
        }

        if due(now, state.next_cycle_at.as_deref()) {
            summary.cycle_ran = true;
            state.last_cycle_at = Some(now.to_rfc3339());
            state.next_cycle_at = Some(next_cycle(now)?.to_rfc3339());

            if self.circuit_breaker.can_execute(now) {
                for layer in SCHEDULED_LAYERS {
                    self.enter_freeze_window(layer.clone());

                    let mode = self.shared_config.load_full().runtime.mode.clone();
                    if matches!(mode, EvolutionMode::Shadow) {
                        // Shadow mode still executes pipeline path; layer engines decide no-op apply.
                    }

                    if self.circuit_breaker.can_mutate_layer(layer.clone()) {
                        match self.run_layer(layer.clone(), now).await {
                            Ok(report) => {
                                // FIX-P0-40: a gate denial is neither a success nor a
                                // failure of the evolution itself — the engine never
                                // ran. Surface it explicitly so the daemon/operator
                                // does not read "no error" as "evolution applied".
                                if report.gate_denied {
                                    tracing::warn!(
                                        target: "self_system",
                                        layer = ?report.layer,
                                        experiment_id = %report.experiment_id,
                                        "evolution layer skipped by side-effect gate (autonomy denied self-modification)"
                                    );
                                } else if matches!(
                                    report.commit,
                                    CommitDecision::Rejected | CommitDecision::Deferred | CommitDecision::ApplyFailed
                                ) || report.judge_outcome == JudgeOutcome::Failed
                                {
                                    // A proposed change that was not written, or
                                    // (R-PMMCh S-1) a judge that gave no verdict
                                    // even where nothing was proposed (DraftOnly,
                                    // Shadow), is a failed run.
                                    self.circuit_breaker.record_failure(now);
                                } else {
                                    self.circuit_breaker.record_success();
                                }
                                summary.layer_reports.push(report);
                            }
                            Err(err) => {
                                self.circuit_breaker.record_failure(now);
                                summary.layer_reports.push(PipelineRunReport {
                                    experiment_id: String::new(),
                                    layer: layer.clone(),
                                    trigger: EvolutionTrigger::CronTick,
                                    digest: self.analyzer.generate_daily_digest(now).await?,
                                    trend: self
                                        .analyzer
                                        .generate_three_day_trend(now.date_naive())
                                        .await
                                        .unwrap_or_else(|_| crate::self_system::evolution::TrendAnalysis {
                                            start_date: now.date_naive().to_string(),
                                            end_date: now.date_naive().to_string(),
                                            digests: Vec::new(),
                                            noise_memories: Vec::new(),
                                            weakest_task_type: None,
                                            lowest_efficiency_config: None,
                                            user_correction_clusters: Vec::new(),
                                            candidates: Vec::new(),
                                        }),
                                    selected_candidate: None,
                                    gate_rejections: Vec::new(),
                                    judge_result: None,
                                    evolution_log: None,
                                    shadow_mode: mode.is_draft_like(),
                                    commit: CommitDecision::NothingProposed,
                                    gate_denied: false,
                                    judge_outcome: JudgeOutcome::NotRun,
                                    errors: vec![err.to_string()],
                                });
                            }
                        }
                    }

                    self.exit_freeze_window(layer);
                }
            }
        }

        self.persist_state(&state).await?;
        Ok(summary)
    }

    fn enter_freeze_window(&mut self, active_layer: EvolutionLayer) {
        self.circuit_breaker.begin_layer_evaluation(active_layer.clone());
        for layer in SCHEDULED_LAYERS {
            if layer != active_layer {
                self.circuit_breaker.freeze_layer(layer);
            }
        }
    }

    fn exit_freeze_window(&mut self, active_layer: EvolutionLayer) {
        for layer in SCHEDULED_LAYERS {
            if layer != active_layer {
                self.circuit_breaker.unfreeze_layer(layer);
            }
        }
        self.circuit_breaker.end_layer_evaluation();
    }

    async fn run_layer(&mut self, layer: EvolutionLayer, now: DateTime<Utc>) -> Result<PipelineRunReport> {
        match layer {
            EvolutionLayer::Prompt => {
                self.pipeline
                    .run_for_layer(EvolutionTrigger::CronTick, layer, self.prompt_engine.as_mut(), now)
                    .await
            }
            _ => bail!("unsupported evolution layer for scheduler run: {:?}", layer),
        }
    }

    async fn load_state(&self) -> Result<SchedulerState> {
        if fs::metadata(&self.state_path).await.is_err() {
            return Ok(SchedulerState::default());
        }
        let raw = fs::read_to_string(&self.state_path).await?;
        match serde_json::from_str(&raw) {
            Ok(state) => Ok(state),
            Err(err) => {
                tracing::warn!(
                    path = %self.state_path.display(),
                    error = %err,
                    "failed to parse scheduler state; falling back to defaults"
                );
                Ok(SchedulerState::default())
            }
        }
    }

    async fn persist_state(&self, state: &SchedulerState) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).await?;
            }
        }
        let workspace_root = self.state_path.parent().unwrap_or_else(|| Path::new("."));
        let payload = serde_json::to_string_pretty(state)?;
        atomic_write(workspace_root, &self.state_path, payload.as_bytes()).await?;
        Ok(())
    }
}

fn ensure_next_schedule(state: &mut SchedulerState, now: DateTime<Utc>) -> Result<()> {
    if state.next_digest_at.is_none() {
        state.next_digest_at = Some(next_daily(now)?.to_rfc3339());
    }
    if state.next_cycle_at.is_none() {
        state.next_cycle_at = Some(next_cycle(now)?.to_rfc3339());
    }
    Ok(())
}

fn due(now: DateTime<Utc>, scheduled: Option<&str>) -> bool {
    let Some(scheduled) = scheduled else {
        return false;
    };
    parse_ts(scheduled).is_some_and(|ts| now >= ts)
}

fn next_daily(now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    next_run_for_schedule(
        &Schedule::Cron {
            expr: DAILY_DIGEST_CRON.to_string(),
            tz: None,
        },
        now,
    )
}

fn next_cycle(now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    next_run_for_schedule(
        &Schedule::Cron {
            expr: EVOLUTION_CYCLE_CRON.to_string(),
            tz: None,
        },
        now + Duration::seconds(1),
    )
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    match DateTime::parse_from_rfc3339(raw) {
        Ok(item) => Some(item.with_timezone(&Utc)),
        Err(err) => {
            tracing::debug!(
                timestamp = raw,
                error = %err,
                "failed to parse scheduler timestamp"
            );
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::self_system::evolution::config::{EvolutionConfig, EvolutionMode, new_shared_evolution_config};
    use crate::self_system::evolution::engine::{CycleResult, EngineCycleInput};
    use crate::self_system::evolution::record::{
        Actor, AnnotationSource, ChangeType, DataBasis, EvolutionLog, MemoryAction, Outcome, TaskType,
    };
    use crate::self_system::evolution::storage::{AsyncJsonlWriter, JsonlRetentionPolicy, JsonlStoragePaths};
    use crate::self_system::evolution::{
        CycleOutcome, EvolutionCycle, EvolutionProposal, EvolutionSignals, EvolutionValidation, FitnessTrend,
        RiskLevel, ValidationStatus,
    };
    use async_trait::async_trait;
    use std::collections::HashMap;
    use tempfile::tempdir;

    struct NoopEngine(EvolutionLayer);

    #[async_trait]
    impl EvolutionEngine for NoopEngine {
        fn name(&self) -> &'static str {
            "noop"
        }

        fn layer(&self) -> EvolutionLayer {
            self.0.clone()
        }

        async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
            Ok(CycleResult {
                review_data: None,
                layer: self.0.clone(),
                proposal: Some(EvolutionProposal {
                    id: input.cycle_id.clone(),
                    summary: "noop".to_string(),
                    rationale: "noop".to_string(),
                    risk_level: RiskLevel::Low,
                    target: crate::self_system::evolution::ChangeTarget::WorkspaceFile {
                        path: "README.md".to_string(),
                    },
                    operation: crate::self_system::evolution::ChangeOperation::Append {
                        content: "".to_string(),
                    },
                }),
                cycle: EvolutionCycle {
                    id: input.cycle_id.clone(),
                    started_at: Utc::now().to_rfc3339(),
                    finished_at: Utc::now().to_rfc3339(),
                    signals: EvolutionSignals {
                        memory_count: 1,
                        health_components: 1,
                        health_error_components: 0,
                        cron_runs: 0,
                        cron_failure_ratio: 0.0,
                    },
                    trend: FitnessTrend {
                        window: 1,
                        previous_average: Some(0.5),
                        latest_score: Some(0.6),
                        is_declining: Some(false),
                        source: "test".to_string(),
                    },
                    proposal: None,
                    validation: EvolutionValidation {
                        status: ValidationStatus::Improved,
                        before_score: Some(0.5),
                        after_score: Some(0.6),
                        delta: Some(0.1),
                        notes: "ok".to_string(),
                    },
                    outcome: CycleOutcome::NoAction,
                    alert: None,
                    errors: Vec::new(),
                },
                evolution_log: Some(EvolutionLog {
                    experiment_id: input.cycle_id,
                    timestamp: Utc::now().to_rfc3339(),
                    layer: self.0.clone(),
                    change_type: ChangeType::Tune,
                    before_value: "a".to_string(),
                    after_value: "b".to_string(),
                    trigger_reason: "r".to_string(),
                    data_basis: DataBasis {
                        sample_count: 1,
                        time_range_days: 1,
                        key_metrics: HashMap::new(),
                        patterns_found: Vec::new(),
                    },
                    result: None,
                }),
                needs_human_approval: false,
                shadow_mode: false,
                pending: None,
            })
        }
    }

    /// An engine that proposes but applies nothing (DraftOnly behaviour).
    struct DraftEngine(EvolutionLayer);

    #[async_trait]
    impl EvolutionEngine for DraftEngine {
        fn name(&self) -> &'static str {
            "draft"
        }

        fn layer(&self) -> EvolutionLayer {
            self.0.clone()
        }

        async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
            let mut result = NoopEngine(self.0.clone()).run_cycle(input).await?;
            result.cycle.outcome = CycleOutcome::NoAction;
            Ok(result)
        }
    }

    /// A model judge whose provider call always fails.
    struct FailingJudge;

    #[async_trait]
    impl crate::self_system::evolution::judge::JudgeScoringModel for FailingJudge {
        async fn score(
            &self,
            _task: &str,
            _result: &str,
            _config: &crate::self_system::evolution::judge::JudgeConfig,
        ) -> Result<String> {
            bail!("model judge provider call failed: 503")
        }
    }

    /// R-PMMCh S-1: a judge that gives no verdict is not a successful layer
    /// run, even in DraftOnly where nothing is applied or rolled back: it
    /// counts as a failure for the circuit breaker.
    #[tokio::test]
    async fn a_failed_judge_counts_as_a_breaker_failure_in_draft_only() {
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        let writer = Arc::new(
            AsyncJsonlWriter::new(JsonlStoragePaths::new(logs.clone()), JsonlRetentionPolicy::default(), 1)
                .await
                .unwrap(),
        );
        let now = Utc::now();
        writer
            .append_memory_access(&crate::self_system::evolution::record::MemoryAccessLog {
                timestamp: now.to_rfc3339(),
                experiment_id: "e1".to_string(),
                trace_id: "t1".to_string(),
                action: MemoryAction::Read,
                memory_id: "m1".to_string(),
                task_context: "ctx".to_string(),
                task_type: TaskType::ToolCall,
                actor: Actor::Agent,
                was_useful: Some(true),
                useful_annotation_source: Some(AnnotationSource::AutoEvaluator),
                annotation_confidence: Some(0.8),
                tokens_consumed: 1,
            })
            .await
            .unwrap();
        writer
            .append_decision(&crate::self_system::evolution::record::DecisionLog {
                timestamp: now.to_rfc3339(),
                experiment_id: "e1".to_string(),
                trace_id: "t1".to_string(),
                decision_type: crate::self_system::evolution::record::DecisionType::ToolSelection,
                task_type: TaskType::ToolCall,
                risk_level: 1,
                actor: Actor::Agent,
                input_context: "ctx".to_string(),
                action_taken: "act".to_string(),
                outcome: Outcome::Success,
                tokens_used: 1,
                latency_ms: 1,
                user_correction: None,
                config_snapshot_hash: "cfg".to_string(),
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
        for days in [2, 1, 0] {
            let _ = analyzer.generate_daily_digest(now - Duration::days(days)).await;
        }
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::DraftOnly;
        cfg.runtime.storage_dir = logs.to_string_lossy().to_string();
        cfg.rollback.circuit_breaker_threshold = 1;
        let shared = new_shared_evolution_config(cfg);
        let pipeline = EvolutionPipeline::with_judge_model(
            shared.clone(),
            analyzer.clone(),
            writer,
            dir.path(),
            Arc::new(FailingJudge),
        );
        let mut scheduler = EvolutionScheduler::new(
            shared,
            analyzer,
            pipeline,
            dir.path().join("scheduler_state.json"),
            Box::new(DraftEngine(EvolutionLayer::Prompt)),
        );
        let state = SchedulerState {
            next_cycle_at: Some((now - Duration::minutes(1)).to_rfc3339()),
            ..SchedulerState::default()
        };
        fs::write(
            dir.path().join("scheduler_state.json"),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .await
        .unwrap();

        let summary = scheduler.run_scheduled(now).await.unwrap();
        assert!(
            summary
                .layer_reports
                .iter()
                .any(|report| report.errors.iter().any(|error| error.contains("judge_failed"))),
            "test: the judge ran and failed: {:?}",
            summary
                .layer_reports
                .iter()
                .map(|report| &report.errors)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            scheduler.circuit_breaker.state(),
            crate::self_system::evolution::rollback::CircuitBreakerState::Open,
            "a failed judge was counted as a successful run"
        );
    }

    #[tokio::test]
    async fn scheduler_runs_digest_and_persists_state() {
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        let writer = Arc::new(
            AsyncJsonlWriter::new(JsonlStoragePaths::new(logs.clone()), JsonlRetentionPolicy::default(), 1)
                .await
                .unwrap(),
        );

        let now = Utc::now();
        writer
            .append_memory_access(&crate::self_system::evolution::record::MemoryAccessLog {
                timestamp: now.to_rfc3339(),
                experiment_id: "e1".to_string(),
                trace_id: "t1".to_string(),
                action: MemoryAction::Read,
                memory_id: "m1".to_string(),
                task_context: "ctx".to_string(),
                task_type: TaskType::Planning,
                actor: Actor::Agent,
                was_useful: Some(true),
                useful_annotation_source: Some(AnnotationSource::AutoEvaluator),
                annotation_confidence: Some(0.9),
                tokens_consumed: 1,
            })
            .await
            .unwrap();
        writer
            .append_decision(&crate::self_system::evolution::record::DecisionLog {
                timestamp: now.to_rfc3339(),
                experiment_id: "e1".to_string(),
                trace_id: "t1".to_string(),
                decision_type: crate::self_system::evolution::record::DecisionType::ToolSelection,
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
        writer.flush().await.unwrap();

        let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Auto;
        cfg.runtime.storage_dir = logs.to_string_lossy().to_string();
        let shared = new_shared_evolution_config(cfg);
        let pipeline = EvolutionPipeline::new(shared.clone(), analyzer.clone(), writer, dir.path());

        let mut scheduler = EvolutionScheduler::new(
            shared,
            analyzer,
            pipeline,
            dir.path().join("scheduler_state.json"),
            Box::new(NoopEngine(EvolutionLayer::Prompt)),
        );

        let mut state = SchedulerState::default();
        state.next_digest_at = Some((now - Duration::minutes(1)).to_rfc3339());
        state.next_cycle_at = Some((now - Duration::minutes(1)).to_rfc3339());
        fs::write(
            dir.path().join("scheduler_state.json"),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .await
        .unwrap();

        let summary = scheduler.run_scheduled(now).await.unwrap();
        assert!(summary.digest_ran);
        assert!(summary.cycle_ran);

        let persisted = fs::read_to_string(dir.path().join("scheduler_state.json"))
            .await
            .unwrap();
        let loaded: SchedulerState = serde_json::from_str(&persisted).unwrap();
        assert!(loaded.next_digest_at.is_some());
        assert!(loaded.next_cycle_at.is_some());
    }

    #[test]
    fn scheduler_state_deserializes_from_legacy_empty_object() {
        let state: SchedulerState = serde_json::from_str("{}").unwrap();
        assert_eq!(state, SchedulerState::default());
    }

    #[test]
    fn scheduler_state_deserializes_with_partial_fields() {
        let state: SchedulerState = serde_json::from_str(r#"{"last_digest_at":"2026-02-24T00:00:00Z"}"#).unwrap();
        assert_eq!(state.last_digest_at.as_deref(), Some("2026-02-24T00:00:00Z"));
        assert!(state.next_digest_at.is_none());
        assert!(state.last_cycle_at.is_none());
        assert!(state.next_cycle_at.is_none());
    }
    #[derive(Default)]
    pub(crate) struct CountingJudge {
        pub(crate) calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl crate::self_system::evolution::judge::JudgeScoringModel for CountingJudge {
        async fn score(
            &self,
            _: &str,
            _: &str,
            _: &crate::self_system::evolution::judge::JudgeConfig,
        ) -> Result<String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(r#"{"task_completion":0.9,"correctness":0.9,"efficiency":0.9,"safety_compliance":0.9,"reasoning":"verified"}"#.into())
        }
    }

    pub(crate) async fn seeded_analysis(dir: &Path) -> (Arc<AsyncJsonlWriter>, Arc<EvolutionAnalyzer>) {
        seeded_analysis_at(dir, Utc::now()).await
    }

    async fn seeded_analysis_at(dir: &Path, now: DateTime<Utc>) -> (Arc<AsyncJsonlWriter>, Arc<EvolutionAnalyzer>) {
        let logs = dir.join("logs");
        let writer = Arc::new(
            AsyncJsonlWriter::new(JsonlStoragePaths::new(logs), JsonlRetentionPolicy::default(), 1)
                .await
                .unwrap(),
        );
        writer
            .append_memory_access(&crate::self_system::evolution::record::MemoryAccessLog {
                timestamp: now.to_rfc3339(),
                experiment_id: "seed".into(),
                trace_id: "seed".into(),
                action: MemoryAction::Read,
                memory_id: "m1".into(),
                task_context: "ctx".into(),
                task_type: TaskType::ToolCall,
                actor: Actor::Agent,
                was_useful: Some(true),
                useful_annotation_source: Some(AnnotationSource::AutoEvaluator),
                annotation_confidence: Some(0.8),
                tokens_consumed: 1,
            })
            .await
            .unwrap();
        writer
            .append_decision(&crate::self_system::evolution::record::DecisionLog {
                timestamp: now.to_rfc3339(),
                experiment_id: "seed".into(),
                trace_id: "seed".into(),
                decision_type: crate::self_system::evolution::record::DecisionType::ToolSelection,
                task_type: TaskType::ToolCall,
                risk_level: 1,
                actor: Actor::Agent,
                input_context: "ctx".into(),
                action_taken: "act".into(),
                outcome: Outcome::Success,
                tokens_used: 1,
                latency_ms: 1,
                user_correction: None,
                config_snapshot_hash: "cfg".into(),
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();
        let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.join("analysis")));
        for days in [2, 1, 0] {
            analyzer
                .generate_daily_digest(now - Duration::days(days))
                .await
                .unwrap();
        }
        (writer, analyzer)
    }

    pub(crate) async fn prompt_scheduler(
        dir: &Path,
        shared: SharedEvolutionConfig,
        engine: Box<dyn EvolutionEngine>,
    ) -> EvolutionScheduler {
        let (writer, analyzer) = seeded_analysis(dir).await;
        let pipeline = EvolutionPipeline::with_judge_model(
            shared.clone(),
            analyzer.clone(),
            writer,
            dir,
            Arc::new(CountingJudge::default()),
        );
        EvolutionScheduler::new(shared, analyzer, pipeline, dir.join("scheduler_state.json"), engine)
    }

    pub(crate) async fn due_cycle(
        scheduler: &mut EvolutionScheduler,
        dir: &Path,
        now: DateTime<Utc>,
    ) -> SchedulerRunSummary {
        // Fresh evidence follows the scheduler's simulated time. Lifecycle
        // evidence and proposal persistence still use the real store clock.
        let _ = seeded_analysis_at(dir, now).await;
        let state = SchedulerState {
            next_cycle_at: Some((now - Duration::minutes(1)).to_rfc3339()),
            next_digest_at: Some((now + Duration::days(1)).to_rfc3339()),
            ..Default::default()
        };
        fs::write(dir.join("scheduler_state.json"), serde_json::to_vec(&state).unwrap())
            .await
            .unwrap();
        scheduler.run_scheduled(now).await.unwrap()
    }

    pub(crate) async fn next_due_time(dir: &Path) -> DateTime<Utc> {
        let state: SchedulerState =
            serde_json::from_slice(&fs::read(dir.join("scheduler_state.json")).await.unwrap()).unwrap();
        DateTime::parse_from_rfc3339(state.next_cycle_at.as_deref().unwrap())
            .unwrap()
            .with_timezone(&Utc)
            + Duration::seconds(1)
    }

    #[tokio::test]
    async fn scheduler_runs_only_the_prompt_layer_and_leaves_legacy_targets_alone() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("decision_policy.toml"), "legacy policy")
            .await
            .unwrap();
        fs::write(dir.path().join("evolution_config.toml"), "legacy config")
            .await
            .unwrap();
        let mut scheduler = prompt_scheduler(
            dir.path(),
            new_shared_evolution_config(EvolutionConfig::default()),
            Box::new(NoopEngine(EvolutionLayer::Prompt)),
        )
        .await;
        let summary = due_cycle(&mut scheduler, dir.path(), Utc::now()).await;
        assert_eq!(summary.layer_reports.len(), 1);
        assert_eq!(summary.layer_reports.first().unwrap().layer, EvolutionLayer::Prompt);
        assert!(summary.layer_reports.first().unwrap().selected_candidate.is_some());
        assert_eq!(
            fs::read(dir.path().join("decision_policy.toml")).await.unwrap(),
            b"legacy policy"
        );
        assert_eq!(
            fs::read(dir.path().join("evolution_config.toml")).await.unwrap(),
            b"legacy config"
        );
    }

    async fn no_target_cycles(skills: &[(&str, bool)]) {
        struct CountedEngine {
            inner: crate::self_system::evolution::PromptEvolutionEngine,
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        #[async_trait]
        impl EvolutionEngine for CountedEngine {
            fn name(&self) -> &'static str {
                "counted_managed_proposer"
            }
            fn layer(&self) -> EvolutionLayer {
                EvolutionLayer::Prompt
            }
            async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.run_cycle(input).await
            }
        }
        let dir = tempdir().unwrap();
        let service = crate::skills::lifecycle::test_support::managed_prompt_fixture(dir.path(), skills);
        let model = crate::skills::lifecycle::test_support::ManagedPromptModel::new("Verify the result.");
        let shared = new_shared_evolution_config(EvolutionConfig::default());
        let engine = crate::self_system::evolution::PromptEvolutionEngine::new(shared.clone(), dir.path(), None)
            .with_lifecycle(Some(service.clone()))
            .with_proposer(model.clone());
        let (writer, analyzer) = seeded_analysis(dir.path()).await;
        let judge = Arc::new(CountingJudge::default());
        let pipeline = EvolutionPipeline::with_judge_model(
            shared.clone(),
            analyzer.clone(),
            writer.clone(),
            dir.path(),
            judge.clone(),
        );
        let engine_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let engine = CountedEngine {
            inner: engine,
            calls: engine_calls.clone(),
        };
        let mut scheduler = EvolutionScheduler::new(
            shared,
            analyzer,
            pipeline,
            dir.path().join("scheduler_state.json"),
            Box::new(engine),
        );
        let mut now = Utc::now();
        let since = now - Duration::days(1);
        for _ in 0..6 {
            // Advance beyond the actual checkpoint chosen by the scheduler.
            let summary = due_cycle(&mut scheduler, dir.path(), now).await;
            now = next_due_time(dir.path()).await;
            assert_eq!(summary.layer_reports.len(), 1);
            let report = &summary.layer_reports.first().unwrap();
            assert_eq!(report.commit, CommitDecision::NothingProposed);
            assert_eq!(report.judge_outcome, JudgeOutcome::NotRun);
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert_eq!(
                scheduler.circuit_breaker.state(),
                super::super::rollback::CircuitBreakerState::Closed
            );
            assert_eq!(scheduler.circuit_breaker.failure_count(), 0);
        }
        assert_eq!(engine_calls.load(std::sync::atomic::Ordering::SeqCst), 6);
        assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(judge.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(service.list_proposals(None).unwrap().len(), 0);
        assert!(writer.read_evolution_since(since).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_managed_target_never_trips_the_breaker() {
        no_target_cycles(&[]).await;
    }

    #[tokio::test]
    async fn skills_without_usable_evidence_never_trip_the_breaker() {
        no_target_cycles(&[("a", false), ("b", false)]).await;
    }
}
