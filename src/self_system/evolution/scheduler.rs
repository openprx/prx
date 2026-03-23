use crate::cron::{Schedule, next_run_for_schedule};
use crate::self_system::evolution::analyzer::{DailyDigest, EvolutionAnalyzer};
use crate::self_system::evolution::config::{EvolutionMode, SharedEvolutionConfig};
use crate::self_system::evolution::engine::EvolutionEngine;
use crate::self_system::evolution::pipeline::{EvolutionPipeline, EvolutionTrigger, PipelineRunReport};
use crate::self_system::evolution::record::EvolutionLayer;
use crate::self_system::evolution::rollback::CircuitBreaker;
use crate::self_system::evolution::safety_utils::atomic_write;
use anyhow::{Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

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
    memory_engine: Box<dyn EvolutionEngine>,
    prompt_engine: Box<dyn EvolutionEngine>,
    strategy_engine: Box<dyn EvolutionEngine>,
}

impl EvolutionScheduler {
    #[allow(clippy::too_many_arguments)]
    /// Create a scheduler with dedicated engines for memory, prompt and policy layers.
    pub fn new(
        shared_config: SharedEvolutionConfig,
        analyzer: Arc<EvolutionAnalyzer>,
        pipeline: EvolutionPipeline,
        state_path: impl AsRef<Path>,
        memory_engine: Box<dyn EvolutionEngine>,
        prompt_engine: Box<dyn EvolutionEngine>,
        strategy_engine: Box<dyn EvolutionEngine>,
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
            memory_engine,
            prompt_engine,
            strategy_engine,
        }
    }

    /// Run daily digest and/or three-layer evolution cycle when schedules are due.
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
                for layer in [EvolutionLayer::Memory, EvolutionLayer::Prompt, EvolutionLayer::Policy] {
                    self.enter_freeze_window(layer.clone());

                    let mode = self.shared_config.load_full().runtime.mode.clone();
                    if matches!(mode, EvolutionMode::Shadow) {
                        // Shadow mode still executes pipeline path; layer engines decide no-op apply.
                    }

                    if self.circuit_breaker.can_mutate_layer(layer.clone()) {
                        match self.run_layer(layer.clone(), now).await {
                            Ok(report) => {
                                if report.rolled_back {
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
                                    shadow_mode: matches!(mode, EvolutionMode::Shadow),
                                    rolled_back: false,
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
        for layer in [EvolutionLayer::Memory, EvolutionLayer::Prompt, EvolutionLayer::Policy] {
            if layer != active_layer {
                self.circuit_breaker.freeze_layer(layer);
            }
        }
    }

    fn exit_freeze_window(&mut self, active_layer: EvolutionLayer) {
        for layer in [EvolutionLayer::Memory, EvolutionLayer::Prompt, EvolutionLayer::Policy] {
            if layer != active_layer {
                self.circuit_breaker.unfreeze_layer(layer);
            }
        }
        self.circuit_breaker.end_layer_evaluation();
    }

    async fn run_layer(&mut self, layer: EvolutionLayer, now: DateTime<Utc>) -> Result<PipelineRunReport> {
        match layer {
            EvolutionLayer::Memory => {
                self.pipeline
                    .run_for_layer(EvolutionTrigger::CronTick, layer, self.memory_engine.as_mut(), now)
                    .await
            }
            EvolutionLayer::Prompt => {
                self.pipeline
                    .run_for_layer(EvolutionTrigger::CronTick, layer, self.prompt_engine.as_mut(), now)
                    .await
            }
            EvolutionLayer::Policy => {
                self.pipeline
                    .run_for_layer(EvolutionTrigger::CronTick, layer, self.strategy_engine.as_mut(), now)
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
mod tests {
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
                        previous_average: 0.5,
                        latest_score: 0.6,
                        is_declining: false,
                    },
                    proposal: None,
                    validation: EvolutionValidation {
                        status: ValidationStatus::Improved,
                        before_score: 0.5,
                        after_score: 0.6,
                        delta: 0.1,
                        notes: "ok".to_string(),
                    },
                    outcome: CycleOutcome::Applied,
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
            })
        }
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
            Box::new(NoopEngine(EvolutionLayer::Memory)),
            Box::new(NoopEngine(EvolutionLayer::Prompt)),
            Box::new(NoopEngine(EvolutionLayer::Policy)),
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
}
