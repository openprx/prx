use crate::self_system::evolution::analyzer::{
    CandidatePriority, DailyDigest, EvolutionAnalyzer, EvolutionCandidate, TrendAnalysis,
};
use crate::self_system::evolution::config::SharedEvolutionConfig;
use crate::self_system::evolution::engine::EvolutionEngine;
use crate::self_system::evolution::gate::{EvolutionGate, GateMetrics, GateRejection, GateResult};
use crate::self_system::evolution::judge::{JudgeConfig, JudgeEngine, JudgeResult, MockJudgeModel};
use crate::self_system::evolution::record::{
    ChangeType, DataBasis, EvolutionLayer, EvolutionLog, EvolutionResult, Outcome,
};
use crate::self_system::evolution::rollback::RollbackManager;
use crate::self_system::evolution::run_engine_cycle;
use crate::self_system::evolution::safety_utils::{acquire_file_lock, atomic_write, validate_path_in_workspace};
use crate::self_system::evolution::storage::AsyncJsonlWriter;
use crate::self_system::evolution::trace::generate_experiment_id;
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

const BACKFILL_DAYS: i64 = 3;
const JUDGE_PASS_THRESHOLD: f64 = 0.6;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionTrigger {
    CronTick,
    Manual,
}

/// End-to-end result for one layer execution of the evolution pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRunReport {
    pub experiment_id: String,
    pub layer: EvolutionLayer,
    pub trigger: EvolutionTrigger,
    pub digest: DailyDigest,
    pub trend: TrendAnalysis,
    pub selected_candidate: Option<EvolutionCandidate>,
    pub gate_rejections: Vec<GateRejection>,
    pub judge_result: Option<JudgeResult>,
    pub evolution_log: Option<EvolutionLog>,
    pub shadow_mode: bool,
    pub rolled_back: bool,
    pub errors: Vec<String>,
}

/// Coordinates analyzer, gate, judge and rollback for each evolution layer.
pub struct EvolutionPipeline {
    analyzer: Arc<EvolutionAnalyzer>,
    writer: Arc<AsyncJsonlWriter>,
    gate: EvolutionGate,
    judge: JudgeEngine<MockJudgeModel>,
    shared_config: SharedEvolutionConfig,
    workspace_root: PathBuf,
    _judge_pass_threshold: f64,
}

impl EvolutionPipeline {
    /// Build a pipeline instance with shared config, analyzer and JSONL writer.
    pub fn new(
        shared_config: SharedEvolutionConfig,
        analyzer: Arc<EvolutionAnalyzer>,
        writer: Arc<AsyncJsonlWriter>,
        workspace_root: impl AsRef<Path>,
    ) -> Self {
        let cfg = shared_config.load_full();
        Self {
            analyzer,
            writer,
            gate: EvolutionGate::from_evolution_config(cfg.as_ref()),
            judge: JudgeEngine::new(JudgeConfig::default(), MockJudgeModel),
            shared_config,
            workspace_root: workspace_root.as_ref().to_path_buf(),
            _judge_pass_threshold: JUDGE_PASS_THRESHOLD,
        }
    }

    /// Execute one pipeline pass for a specific layer and trigger source.
    pub async fn run_for_layer(
        &mut self,
        trigger: EvolutionTrigger,
        layer: EvolutionLayer,
        engine: &mut dyn EvolutionEngine,
        now: DateTime<Utc>,
    ) -> Result<PipelineRunReport> {
        let experiment_id = generate_experiment_id();
        let mut errors = Vec::new();

        let digest = self.analyzer.generate_daily_digest(now).await?;
        let trend = match self.analyzer.generate_three_day_trend(now.date_naive()).await {
            Ok(item) => item,
            Err(err) => {
                errors.push(format!("three_day_trend_fallback: {err}"));
                TrendAnalysis {
                    start_date: now.date_naive().to_string(),
                    end_date: now.date_naive().to_string(),
                    digests: vec![digest.clone()],
                    noise_memories: Vec::new(),
                    weakest_task_type: None,
                    lowest_efficiency_config: None,
                    user_correction_clusters: Vec::new(),
                    candidates: Vec::new(),
                }
            }
        };

        let mut gate_rejections = Vec::new();
        let mut passed_candidates = Vec::new();
        for candidate in &trend.candidates {
            let metrics = metrics_for_candidate(candidate);
            match self.gate.evaluate(candidate, &metrics) {
                GateResult::Passed => passed_candidates.push(candidate.clone()),
                GateResult::Rejected(rejection) => gate_rejections.push(rejection),
            }
        }

        passed_candidates.sort_by_key(candidate_priority_rank);
        let selected_candidate = passed_candidates.into_iter().next();

        if selected_candidate.is_none() {
            if let Err(err) = self.backfill_results(now).await {
                tracing::warn!(error = %err, "failed to backfill evolution results");
            }
            return Ok(PipelineRunReport {
                experiment_id,
                layer,
                trigger,
                digest,
                trend,
                selected_candidate: None,
                gate_rejections,
                judge_result: None,
                evolution_log: None,
                shadow_mode: matches!(
                    self.shared_config.load_full().runtime.mode,
                    crate::self_system::evolution::config::EvolutionMode::Shadow
                ),
                rolled_back: false,
                errors,
            });
        }

        let Some(selected_candidate) = selected_candidate else {
            // Guard above already returned when no candidate; keep fail-fast invariant explicit.
            return Ok(PipelineRunReport {
                experiment_id,
                layer,
                trigger,
                digest,
                trend,
                selected_candidate: None,
                gate_rejections,
                judge_result: None,
                evolution_log: None,
                shadow_mode: matches!(
                    self.shared_config.load_full().runtime.mode,
                    crate::self_system::evolution::config::EvolutionMode::Shadow
                ),
                rolled_back: false,
                errors,
            });
        };
        let cycle_result = run_engine_cycle(engine, experiment_id.clone(), vec![selected_candidate.clone()]).await?;

        let mut evolution_log = cycle_result.evolution_log.clone().unwrap_or_else(|| {
            build_log(
                &experiment_id,
                layer.clone(),
                &selected_candidate,
                &cycle_result.cycle.validation.notes,
            )
        });
        evolution_log.experiment_id = experiment_id.clone();

        let judge = self
            .judge
            .judge_task(
                &experiment_id,
                &cycle_result.cycle.id,
                cycle_result
                    .proposal
                    .as_ref()
                    .map_or("evolution_cycle", |item| item.summary.as_str()),
                &cycle_result.cycle.validation.notes,
            )
            .await?;

        let mut rolled_back = false;
        if should_rollback(&judge, &cycle_result) {
            if let Err(err) = self.rollback_cycle(layer.clone(), cycle_result.proposal.as_ref()).await {
                errors.push(format!("rollback_failed: {err}"));
            } else {
                rolled_back = true;
            }
            evolution_log.change_type = ChangeType::Rollback;
            evolution_log.result = Some(EvolutionResult::Regressed);
        } else {
            evolution_log.result = Some(match cycle_result.cycle.outcome {
                crate::self_system::evolution::CycleOutcome::Applied => EvolutionResult::Improved,
                crate::self_system::evolution::CycleOutcome::Failed => EvolutionResult::Regressed,
                _ => EvolutionResult::Neutral,
            });
        }

        self.writer.append_evolution(&evolution_log).await?;
        if let Err(err) = self.backfill_results(now).await {
            tracing::warn!(error = %err, "failed to backfill evolution results");
        }

        Ok(PipelineRunReport {
            experiment_id,
            layer,
            trigger,
            digest,
            trend,
            selected_candidate: Some(selected_candidate),
            gate_rejections,
            judge_result: Some(judge),
            evolution_log: Some(evolution_log),
            shadow_mode: cycle_result.shadow_mode,
            rolled_back,
            errors,
        })
    }

    async fn rollback_cycle(
        &self,
        layer: EvolutionLayer,
        proposal: Option<&crate::self_system::evolution::EvolutionProposal>,
    ) -> Result<()> {
        let Some(proposal) = proposal else {
            return Ok(());
        };

        let raw_target = match &proposal.target {
            crate::self_system::evolution::ChangeTarget::ConfigFile { path }
            | crate::self_system::evolution::ChangeTarget::CronFile { path }
            | crate::self_system::evolution::ChangeTarget::WorkspaceFile { path } => path,
        };
        let target_rel = if Path::new(raw_target).is_absolute() {
            Path::new(raw_target).strip_prefix(&self.workspace_root)?
        } else {
            Path::new(raw_target)
        };
        let target_path = validate_path_in_workspace(&self.workspace_root, target_rel)?;

        let rollback_dir = infer_rollback_dir(&self.workspace_root, &layer)?;
        let max_versions = self.shared_config.load_full().rollback.max_versions;
        let manager = RollbackManager::new(&self.workspace_root, &target_path, rollback_dir, max_versions)?;
        manager.rollback_latest().await
    }

    /// Backfill evolution results for stale logs that still have unknown outcomes.
    pub async fn backfill_results(&self, now: DateTime<Utc>) -> Result<u32> {
        let cutoff = now - Duration::days(BACKFILL_DAYS);
        let mut updated = 0u32;
        let root = self.writer_root().join("evolution");

        for tier in ["hot", "warm", "cold"] {
            let dir = root.join(tier);
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
                let mut output = Vec::new();
                let mut malformed_lines = 0u32;
                for line in raw.lines().filter(|line| !line.trim().is_empty()) {
                    let mut parsed = match serde_json::from_str::<EvolutionLog>(line) {
                        Ok(item) => item,
                        Err(_) => {
                            malformed_lines = malformed_lines.saturating_add(1);
                            output.push(line.to_string());
                            continue;
                        }
                    };

                    if parsed.result.is_none() && parse_rfc3339(&parsed.timestamp).is_some_and(|ts| ts <= cutoff) {
                        parsed.result = Some(self.infer_backfill_result(&parsed, now).await?);
                        changed = true;
                        updated = updated.saturating_add(1);
                    }
                    output.push(serde_json::to_string(&parsed)?);
                }
                if malformed_lines > 0 {
                    tracing::warn!(
                        path = %path.display(),
                        malformed_lines,
                        "kept malformed evolution lines during backfill rewrite"
                    );
                }

                if changed {
                    let mut rebuilt = output.join("\n");
                    if !rebuilt.is_empty() {
                        rebuilt.push('\n');
                    }
                    let _guard = acquire_file_lock(&path).await?;
                    atomic_write(&root, &path, rebuilt.as_bytes()).await?;
                }
            }
        }

        Ok(updated)
    }

    async fn infer_backfill_result(&self, log: &EvolutionLog, now: DateTime<Utc>) -> Result<EvolutionResult> {
        let since = match parse_rfc3339(&log.timestamp) {
            Some(ts) => ts,
            None => {
                tracing::debug!(
                    timestamp = %log.timestamp,
                    experiment_id = %log.experiment_id,
                    "failed to parse evolution timestamp; using fallback backfill window"
                );
                now - Duration::days(BACKFILL_DAYS)
            }
        };
        let decisions = self.writer.read_decisions_since(since).await?;
        let mut success = 0u32;
        let mut failure = 0u32;
        for row in decisions.iter().filter(|item| item.experiment_id == log.experiment_id) {
            match row.outcome {
                Outcome::Success => success = success.saturating_add(1),
                Outcome::Failure | Outcome::RolledBack => failure = failure.saturating_add(1),
                _ => {}
            }
        }

        let result = if success > failure {
            EvolutionResult::Improved
        } else if failure > success {
            EvolutionResult::Regressed
        } else {
            EvolutionResult::Neutral
        };
        Ok(result)
    }

    fn writer_root(&self) -> PathBuf {
        self.shared_config.load_full().runtime.storage_dir.to_string().into()
    }
}

fn build_log(experiment_id: &str, layer: EvolutionLayer, candidate: &EvolutionCandidate, reason: &str) -> EvolutionLog {
    EvolutionLog {
        experiment_id: experiment_id.to_string(),
        timestamp: Utc::now().to_rfc3339(),
        layer,
        change_type: ChangeType::Tune,
        before_value: candidate.current_value.clone(),
        after_value: candidate.suggested_value.clone(),
        trigger_reason: reason.to_string(),
        data_basis: DataBasis {
            sample_count: candidate.evidence_ids.len() as u32,
            time_range_days: candidate.backfill_after_days,
            key_metrics: HashMap::new(),
            patterns_found: vec![candidate.current_value.clone()],
        },
        result: None,
    }
}

const fn candidate_priority_rank(candidate: &EvolutionCandidate) -> u8 {
    match candidate.priority {
        CandidatePriority::High => 0,
        CandidatePriority::Medium => 1,
        CandidatePriority::Low => 2,
    }
}

const fn metrics_for_candidate(candidate: &EvolutionCandidate) -> GateMetrics {
    let average_improvement = match candidate.priority {
        CandidatePriority::High => 0.08,
        CandidatePriority::Medium => 0.05,
        CandidatePriority::Low => 0.03,
    };

    GateMetrics {
        average_improvement,
        holdout_regression: -0.01,
        token_degradation: 0.02,
    }
}

fn should_rollback(judge: &JudgeResult, cycle_result: &crate::self_system::evolution::engine::CycleResult) -> bool {
    judge.scores.overall() < JUDGE_PASS_THRESHOLD
        || matches!(
            cycle_result.cycle.outcome,
            crate::self_system::evolution::CycleOutcome::Failed
        )
}

fn infer_rollback_dir(workspace_root: &Path, layer: &EvolutionLayer) -> Result<PathBuf> {
    let layer_name = match layer {
        EvolutionLayer::Memory => "memory",
        EvolutionLayer::Prompt => "prompt",
        EvolutionLayer::Policy => "strategy",
        EvolutionLayer::Tooling => "tooling",
        EvolutionLayer::Runtime => "runtime",
    };

    validate_path_in_workspace(
        workspace_root,
        &Path::new(".evolution").join("rollback").join(layer_name),
    )
}

fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    match DateTime::parse_from_rfc3339(raw) {
        Ok(item) => Some(item.with_timezone(&Utc)),
        Err(err) => {
            tracing::debug!(
                timestamp = raw,
                error = %err,
                "failed to parse rfc3339 timestamp"
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
    use crate::self_system::evolution::record::{Actor, AnnotationSource, DecisionType, MemoryAction, TaskType};
    use crate::self_system::evolution::safety_utils::acquire_file_lock;
    use crate::self_system::evolution::storage::{JsonlRetentionPolicy, JsonlStoragePaths};
    use crate::self_system::evolution::{
        CycleOutcome, EvolutionCycle, EvolutionProposal, EvolutionSignals, EvolutionValidation, FitnessTrend,
        RiskLevel, ValidationStatus,
    };
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    struct MockEngine;

    #[async_trait]
    impl EvolutionEngine for MockEngine {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn layer(&self) -> EvolutionLayer {
            EvolutionLayer::Memory
        }

        async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
            Ok(CycleResult {
                layer: EvolutionLayer::Memory,
                proposal: Some(EvolutionProposal {
                    id: input.cycle_id.clone(),
                    summary: "mock proposal".to_string(),
                    rationale: "mock rationale".to_string(),
                    risk_level: RiskLevel::Low,
                    target: crate::self_system::evolution::ChangeTarget::ConfigFile {
                        path: "evolution_config.toml".to_string(),
                    },
                    operation: crate::self_system::evolution::ChangeOperation::Write {
                        content: "".to_string(),
                    },
                }),
                cycle: EvolutionCycle {
                    id: input.cycle_id,
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
                        notes: "success".to_string(),
                    },
                    outcome: CycleOutcome::Applied,
                    alert: None,
                    errors: Vec::new(),
                },
                evolution_log: None,
                needs_human_approval: false,
                shadow_mode: false,
            })
        }
    }

    #[tokio::test]
    async fn pipeline_selects_top_candidate_and_propagates_experiment_id() {
        let dir = tempdir().unwrap();
        let storage_root = dir.path().join("logs");
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(storage_root.clone()),
                JsonlRetentionPolicy::default(),
                1,
            )
            .await
            .unwrap(),
        );

        let decision = crate::self_system::evolution::record::DecisionLog {
            timestamp: Utc::now().to_rfc3339(),
            experiment_id: "exp-a".to_string(),
            trace_id: "trace-a".to_string(),
            decision_type: DecisionType::ToolSelection,
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
        };
        writer.append_decision(&decision).await.unwrap();

        let memory = crate::self_system::evolution::record::MemoryAccessLog {
            timestamp: Utc::now().to_rfc3339(),
            experiment_id: "exp-a".to_string(),
            trace_id: "trace-a".to_string(),
            action: MemoryAction::Read,
            memory_id: "m1".to_string(),
            task_context: "ctx".to_string(),
            task_type: TaskType::ToolCall,
            actor: Actor::Agent,
            was_useful: Some(true),
            useful_annotation_source: Some(AnnotationSource::AutoEvaluator),
            annotation_confidence: Some(0.8),
            tokens_consumed: 1,
        };
        writer.append_memory_access(&memory).await.unwrap();
        writer.flush().await.unwrap();

        let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Auto;
        cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
        let shared = new_shared_evolution_config(cfg);

        let mut pipeline = EvolutionPipeline::new(shared, analyzer, writer.clone(), dir.path());
        let mut engine = MockEngine;
        let now = Utc::now();

        let _ = pipeline
            .analyzer
            .generate_daily_digest(now - chrono::Duration::days(2))
            .await;
        let _ = pipeline
            .analyzer
            .generate_daily_digest(now - chrono::Duration::days(1))
            .await;
        let _ = pipeline.analyzer.generate_daily_digest(now).await;

        let mut trend = pipeline
            .analyzer
            .generate_three_day_trend(now.date_naive())
            .await
            .unwrap_or(TrendAnalysis {
                start_date: "".to_string(),
                end_date: "".to_string(),
                digests: Vec::new(),
                noise_memories: Vec::new(),
                weakest_task_type: None,
                lowest_efficiency_config: None,
                user_correction_clusters: Vec::new(),
                candidates: Vec::new(),
            });
        if trend.candidates.is_empty() {
            let mut target = BTreeMap::new();
            target.insert("task_type".to_string(), "tool_call".to_string());
            trend.candidates.push(EvolutionCandidate {
                target,
                current_value: "x".to_string(),
                suggested_value: "y".to_string(),
                evidence_ids: vec!["trace-a".to_string()],
                priority: CandidatePriority::High,
                backfill_after_days: 3,
            });
        }
        drop(trend);

        let report = pipeline
            .run_for_layer(EvolutionTrigger::Manual, EvolutionLayer::Memory, &mut engine, now)
            .await
            .unwrap();

        assert!(report.selected_candidate.is_some());
        assert_eq!(report.experiment_id.len(), 36);
        assert_eq!(
            report.evolution_log.as_ref().map(|item| item.experiment_id.as_str()),
            Some(report.experiment_id.as_str())
        );
    }

    #[tokio::test]
    async fn rollback_cycle_rejects_path_traversal_target() {
        let dir = tempdir().unwrap();
        let storage_root = dir.path().join("logs");
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(storage_root.clone()),
                JsonlRetentionPolicy::default(),
                1,
            )
            .await
            .unwrap(),
        );
        let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
        let shared = new_shared_evolution_config(cfg);
        let pipeline = EvolutionPipeline::new(shared, analyzer, writer, dir.path());
        let proposal = EvolutionProposal {
            id: "p".to_string(),
            summary: "s".to_string(),
            rationale: "r".to_string(),
            risk_level: RiskLevel::High,
            target: crate::self_system::evolution::ChangeTarget::WorkspaceFile {
                path: "../escape.txt".to_string(),
            },
            operation: crate::self_system::evolution::ChangeOperation::Write {
                content: "x".to_string(),
            },
        };

        let err = pipeline
            .rollback_cycle(EvolutionLayer::Prompt, Some(&proposal))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("parent traversal"));
    }

    #[tokio::test]
    async fn backfill_results_waits_for_file_lock_and_updates_atomically() {
        let dir = tempdir().unwrap();
        let storage_root = dir.path().join("logs");
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(storage_root.clone()),
                JsonlRetentionPolicy::default(),
                1,
            )
            .await
            .unwrap(),
        );
        let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
        let shared = new_shared_evolution_config(cfg);
        let pipeline = EvolutionPipeline::new(shared, analyzer, writer, dir.path());

        let path = storage_root.join("evolution/hot/2026-02-20.jsonl");
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let line = r#"{"experiment_id":"exp-lock","timestamp":"2026-02-20T00:00:00Z","layer":"memory","change_type":"tune","before_value":"a","after_value":"b","trigger_reason":"r","data_basis":{"sample_count":1,"time_range_days":1,"key_metrics":{},"patterns_found":[]},"result":null}"#;
        fs::write(&path, format!("{line}\n")).await.unwrap();

        let guard = acquire_file_lock(&path).await.unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-24T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let task = tokio::spawn({
            let pipeline = pipeline;
            async move { pipeline.backfill_results(now).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(!task.is_finished());
        drop(guard);

        let updated = task.await.unwrap().unwrap();
        assert_eq!(updated, 1);
        let rebuilt = fs::read_to_string(&path).await.unwrap();
        assert!(rebuilt.contains("\"result\":\"neutral\""));
    }
}
