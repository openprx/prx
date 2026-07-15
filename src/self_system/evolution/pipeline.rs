use crate::security::SideEffectGate;
use crate::security::policy::{ResourceRiskLevel, SecurityPolicy};
use crate::self_system::evolution::analyzer::{
    CandidatePriority, DailyDigest, EvolutionAnalyzer, EvolutionCandidate, TrendAnalysis,
};
use crate::self_system::evolution::config::SharedEvolutionConfig;
use crate::self_system::evolution::engine::EvolutionEngine;
use crate::self_system::evolution::gate::{EvolutionGate, GateMetrics, GateRejection, GateResult};
use crate::self_system::evolution::judge::{JudgeConfig, JudgeEngine, JudgeResult, JudgeScoringModel, MockJudgeModel};
use crate::self_system::evolution::record::{
    ChangeType, DataBasis, EvolutionLayer, EvolutionLog, EvolutionResult, Outcome,
};
use crate::self_system::evolution::rollback::RollbackManager;
use crate::self_system::evolution::run_engine_cycle;
use crate::self_system::evolution::safety_utils::{acquire_file_lock, validate_path_in_workspace};
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
/// Derived, append-only file holding inferred outcomes for stale evolution logs.
/// Backfill writes here instead of mutating the source evolution JSONL in place,
/// preserving the append-only invariant of the primary audit log.
const RESULT_HISTORY_FILE: &str = "result_history.jsonl";

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
    /// FIX-P0-40: `true` when the side-effect gate denied this layer's commit and
    /// the engine was therefore never executed (no config/strategy/prompt write,
    /// no `append_evolution`). Lets the scheduler/daemon distinguish "completed"
    /// from "skipped by gate" instead of treating a denial as a silent success.
    pub gate_denied: bool,
    pub errors: Vec<String>,
}

/// Coordinates analyzer, gate, judge and rollback for each evolution layer.
pub struct EvolutionPipeline {
    analyzer: Arc<EvolutionAnalyzer>,
    writer: Arc<AsyncJsonlWriter>,
    gate: EvolutionGate,
    judge: JudgeEngine,
    shared_config: SharedEvolutionConfig,
    workspace_root: PathBuf,
    /// FIX-P0-40: optional side-effect gate enforced just before an evolution
    /// log is committed (the real mutation point). `None` preserves the prior
    /// behaviour (no side-effect gate) so existing callers are unaffected; the
    /// daemon wires a real policy via [`Self::with_security_policy`].
    security_policy: Option<Arc<SecurityPolicy>>,
}

impl EvolutionPipeline {
    /// Build a pipeline instance with the default mock judge model.
    pub fn new(
        shared_config: SharedEvolutionConfig,
        analyzer: Arc<EvolutionAnalyzer>,
        writer: Arc<AsyncJsonlWriter>,
        workspace_root: impl AsRef<Path>,
    ) -> Self {
        Self::with_judge_model(
            shared_config,
            analyzer,
            writer,
            workspace_root,
            Arc::new(MockJudgeModel),
        )
    }

    /// Build a pipeline instance with a caller-supplied judge scoring model.
    ///
    /// The judge pass threshold is read from `JudgeConfig` (configurable) and the
    /// human-review queue is rooted under the configured evolution storage dir.
    pub fn with_judge_model(
        shared_config: SharedEvolutionConfig,
        analyzer: Arc<EvolutionAnalyzer>,
        writer: Arc<AsyncJsonlWriter>,
        workspace_root: impl AsRef<Path>,
        judge_model: Arc<dyn JudgeScoringModel>,
    ) -> Self {
        let cfg = shared_config.load_full();
        let storage_dir = PathBuf::from(cfg.runtime.storage_dir.clone());
        let judge = JudgeEngine::new(JudgeConfig::default(), judge_model)
            .with_review_queue(storage_dir.join("judge_review_queue.jsonl"));
        Self {
            analyzer,
            writer,
            gate: EvolutionGate::from_evolution_config(cfg.as_ref()),
            judge,
            shared_config,
            workspace_root: workspace_root.as_ref().to_path_buf(),
            security_policy: None,
        }
    }

    /// FIX-P0-40: attach the runtime [`SecurityPolicy`] whose [`SideEffectGate`]
    /// must authorize every evolution commit before it is persisted.
    ///
    /// Without this, an autonomous evolution cycle would write self-modifications
    /// to the audit log without ever passing through the same side-effect gate
    /// that governs tool execution. The daemon constructs the policy from the
    /// active config and installs it here; tests and the CLI may leave it unset to
    /// retain the un-gated behaviour.
    #[must_use]
    pub fn with_security_policy(mut self, policy: Arc<SecurityPolicy>) -> Self {
        self.security_policy = Some(policy);
        self
    }

    /// FIX-P0-40: enforce the side-effect gate for an evolution commit.
    ///
    /// Called *before* the layer engine executes, because the engines perform
    /// their target-file write and `append_evolution` inside `run_cycle`; gating
    /// afterwards would let those writes escape. The operation id encodes the
    /// layer and experiment so the audit trail records exactly which
    /// self-modification was authorized. When no policy is installed this is a
    /// no-op (preserving legacy behaviour). On denial the gate's rejection reason
    /// is returned as `Err(reason)` so the caller can record a structured
    /// [`GateRejection`], skip the engine entirely (fail-closed), and continue the
    /// scheduler tick instead of aborting on an error.
    fn authorize_commit(&self, layer: &EvolutionLayer, experiment_id: &str) -> Result<(), String> {
        let Some(policy) = self.security_policy.as_ref() else {
            return Ok(());
        };
        let layer_name = layer_slug(layer);
        let operation = format!("evolution:{layer_name}:{experiment_id}");
        SideEffectGate::new(policy.as_ref())
            .authorize_resource_operation("evolution", &operation, ResourceRiskLevel::Medium, None)
            .map(|_| ())
    }

    /// Execute one pipeline pass for a specific layer and trigger source.
    pub async fn run_for_layer(
        &self,
        trigger: EvolutionTrigger,
        layer: EvolutionLayer,
        engine: &mut dyn EvolutionEngine,
        now: DateTime<Utc>,
    ) -> Result<PipelineRunReport> {
        let experiment_id = generate_experiment_id();
        let mut errors = Vec::new();

        // FIX-P0-40 (LOW): `generate_daily_digest` persists a daily digest to the
        // analysis directory *before* the side-effect gate runs. This is
        // intentional and deliberately NOT gated: the digest is a read-only
        // analysis/statistics artifact (aggregated success rate, token averages,
        // metric-shift alerts computed from already-recorded decision/memory logs).
        // It is the *input* the pipeline reads to decide whether an evolution
        // should happen — not an evolution self-modification. It never writes
        // config/strategy/prompt files and never calls `append_evolution`, so it is
        // outside the autonomy gate's scope and cannot be abused to apply a
        // self-modification. Only the engine commit below is gated.
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
                shadow_mode: self.shared_config.load_full().runtime.mode.is_proposal_only(),
                rolled_back: false,
                gate_denied: false,
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
                shadow_mode: self.shared_config.load_full().runtime.mode.is_proposal_only(),
                rolled_back: false,
                gate_denied: false,
                errors,
            });
        };

        // FIX-P0-40 (fail-closed): authorize the self-modification through the
        // side-effect gate *before* the engine runs. The layer engines write their
        // target file (config/strategy/prompt) and `append_evolution` *inside*
        // `run_engine_cycle`, so gating after the engine would let those writes
        // escape. Denying here means the engine is never invoked and no write of
        // any kind occurs — the only correct interpretation of "fail-closed".
        if let Err(reason) = self.authorize_commit(&layer, &experiment_id) {
            tracing::warn!(
                layer = ?layer,
                experiment_id = %experiment_id,
                reason = %reason,
                "evolution commit blocked by side-effect gate; engine skipped, no write performed"
            );
            gate_rejections.push(GateRejection {
                reason: "side_effect_gate_denied".to_string(),
                details: reason,
            });
            return Ok(PipelineRunReport {
                experiment_id,
                layer,
                trigger,
                digest,
                trend,
                selected_candidate: Some(selected_candidate),
                gate_rejections,
                judge_result: None,
                evolution_log: None,
                shadow_mode: self.shared_config.load_full().runtime.mode.is_proposal_only(),
                rolled_back: false,
                gate_denied: true,
                errors,
            });
        }

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
        if should_rollback(&judge, &cycle_result, self.judge.pass_threshold()) {
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

        // FIX-P0-40: the side-effect gate has already authorized this commit
        // *before* the engine ran (see the fail-closed check above). The engine's
        // own writes and this pipeline-level audit append therefore only happen on
        // an allow decision; a denial returned early and never reached here.
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
            gate_denied: false,
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

    /// Backfill inferred outcomes for stale evolution logs.
    ///
    /// FIX-P1-09 (append-only): the source evolution JSONL is the primary audit log and
    /// must never be rewritten in place. Instead of mutating `evolution/<tier>/*.jsonl`,
    /// this derives an outcome for each stale log whose `result` is still unknown and
    /// appends a `BackfillResultRecord` to a separate derived `result_history.jsonl`.
    /// Each `experiment_id` is backfilled at most once (idempotent across reruns).
    pub async fn backfill_results(&self, now: DateTime<Utc>) -> Result<u32> {
        let cutoff = now - Duration::days(BACKFILL_DAYS);
        let mut updated = 0u32;
        let root = self.writer_root().join("evolution");

        let history_path = root.join(RESULT_HISTORY_FILE);
        let already_backfilled = read_backfilled_ids(&history_path).await?;

        let mut pending: Vec<BackfillResultRecord> = Vec::new();
        let mut seen_this_run: std::collections::HashSet<String> = std::collections::HashSet::new();

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
                let mut malformed_lines = 0u32;
                for line in raw.lines().filter(|line| !line.trim().is_empty()) {
                    let parsed = match serde_json::from_str::<EvolutionLog>(line) {
                        Ok(item) => item,
                        Err(_) => {
                            malformed_lines = malformed_lines.saturating_add(1);
                            continue;
                        }
                    };

                    if parsed.result.is_some() {
                        continue;
                    }
                    if parse_rfc3339(&parsed.timestamp).is_none_or(|ts| ts > cutoff) {
                        continue;
                    }
                    if already_backfilled.contains(&parsed.experiment_id)
                        || !seen_this_run.insert(parsed.experiment_id.clone())
                    {
                        continue;
                    }

                    let result = self.infer_backfill_result(&parsed, now).await?;
                    pending.push(BackfillResultRecord {
                        experiment_id: parsed.experiment_id.clone(),
                        layer: parsed.layer.clone(),
                        original_timestamp: parsed.timestamp.clone(),
                        result,
                        backfilled_at: now.to_rfc3339(),
                    });
                    updated = updated.saturating_add(1);
                }
                if malformed_lines > 0 {
                    tracing::warn!(
                        path = %path.display(),
                        malformed_lines,
                        "skipped malformed evolution lines during backfill scan"
                    );
                }
            }
        }

        if !pending.is_empty() {
            if let Some(parent) = history_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            let mut appended = String::new();
            for record in &pending {
                appended.push_str(&serde_json::to_string(record)?);
                appended.push('\n');
            }
            let _guard = acquire_file_lock(&history_path).await?;
            append_text(&history_path, &appended).await?;
        }

        Ok(updated)
    }

    async fn infer_backfill_result(&self, log: &EvolutionLog, now: DateTime<Utc>) -> Result<EvolutionResult> {
        let since = parse_rfc3339(&log.timestamp).unwrap_or_else(|| {
            tracing::debug!(
                timestamp = %log.timestamp,
                experiment_id = %log.experiment_id,
                "failed to parse evolution timestamp; using fallback backfill window"
            );
            now - Duration::days(BACKFILL_DAYS)
        });
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

/// Derived, append-only backfill outcome record.
///
/// Stored in `result_history.jsonl` so inferred outcomes never mutate the primary,
/// append-only evolution audit JSONL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackfillResultRecord {
    pub experiment_id: String,
    pub layer: EvolutionLayer,
    pub original_timestamp: String,
    pub result: EvolutionResult,
    pub backfilled_at: String,
}

/// Collect experiment IDs already present in the derived `result_history.jsonl`.
async fn read_backfilled_ids(path: &Path) -> Result<std::collections::HashSet<String>> {
    let mut ids = std::collections::HashSet::new();
    if fs::metadata(path).await.is_err() {
        return Ok(ids);
    }
    let raw = fs::read_to_string(path).await?;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<BackfillResultRecord>(line) {
            Ok(record) => {
                ids.insert(record.experiment_id);
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "skipping malformed result_history line"
                );
            }
        }
    }
    Ok(ids)
}

/// Append text to a file, creating it if necessary (append-only).
async fn append_text(path: &Path, text: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut file = fs::OpenOptions::new().create(true).append(true).open(path).await?;
    file.write_all(text.as_bytes()).await?;
    file.flush().await?;
    Ok(())
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

fn should_rollback(
    judge: &JudgeResult,
    cycle_result: &crate::self_system::evolution::engine::CycleResult,
    pass_threshold: f64,
) -> bool {
    matches!(
        cycle_result.cycle.outcome,
        crate::self_system::evolution::CycleOutcome::Applied
    ) && judge.scores.overall() < pass_threshold
}

/// Stable short name for an evolution layer, used in gate operation ids and
/// rollback directory paths.
const fn layer_slug(layer: &EvolutionLayer) -> &'static str {
    match layer {
        EvolutionLayer::Memory => "memory",
        EvolutionLayer::Prompt => "prompt",
        EvolutionLayer::Policy => "strategy",
        EvolutionLayer::Tooling => "tooling",
        EvolutionLayer::Runtime => "runtime",
    }
}

fn infer_rollback_dir(workspace_root: &Path, layer: &EvolutionLayer) -> Result<PathBuf> {
    let layer_name = layer_slug(layer);

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
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
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

    /// FIX-P0-40 fail-closed spy: an engine that records whether it was invoked
    /// and writes a sentinel file the instant its `run_cycle` runs. Used to prove
    /// that a side-effect gate denial prevents the engine (and therefore every
    /// engine-internal write: config/strategy/prompt file + append_evolution)
    /// from ever executing.
    struct WriteSpyEngine {
        invoked: Arc<std::sync::atomic::AtomicBool>,
        sentinel_path: PathBuf,
    }

    #[async_trait]
    impl EvolutionEngine for WriteSpyEngine {
        fn name(&self) -> &'static str {
            "write_spy"
        }

        fn layer(&self) -> EvolutionLayer {
            EvolutionLayer::Memory
        }

        async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult> {
            // Mark invocation and emulate an engine that writes to disk during its
            // cycle (exactly the behaviour the gate must prevent on denial).
            self.invoked.store(true, std::sync::atomic::Ordering::SeqCst);
            fs::write(&self.sentinel_path, b"engine wrote this").await?;
            let mut engine = MockEngine;
            engine.run_cycle(input).await
        }
    }

    /// FIX-P0-40 (hard acceptance): under a Supervised policy with no runtime
    /// grant, the side-effect gate must DENY the evolution commit *before* the
    /// engine runs. Verifies the engine is never invoked, no sentinel file is
    /// written, no evolution JSONL is appended, and the report flags `gate_denied`.
    #[tokio::test]
    async fn gate_denied_blocks_engine_and_all_writes() {
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

        // Seed analyzer inputs so a candidate is produced and the pipeline reaches
        // the gate (otherwise it would early-return before authorize_commit).
        let now = Utc::now();
        for offset in 0..3 {
            let ts = (now - chrono::Duration::days(offset)).to_rfc3339();
            writer
                .append_decision(&crate::self_system::evolution::record::DecisionLog {
                    timestamp: ts.clone(),
                    experiment_id: "exp-seed".to_string(),
                    trace_id: "trace-seed".to_string(),
                    decision_type: DecisionType::ToolSelection,
                    task_type: TaskType::ToolCall,
                    risk_level: 1,
                    actor: Actor::Agent,
                    input_context: "ctx".to_string(),
                    action_taken: "act".to_string(),
                    outcome: Outcome::Failure,
                    tokens_used: 1,
                    latency_ms: 1,
                    user_correction: Some("please do X instead".to_string()),
                    config_snapshot_hash: "cfg".to_string(),
                })
                .await
                .unwrap();
            writer
                .append_memory_access(&crate::self_system::evolution::record::MemoryAccessLog {
                    timestamp: ts,
                    experiment_id: "exp-seed".to_string(),
                    trace_id: "trace-seed".to_string(),
                    action: MemoryAction::Read,
                    memory_id: "m1".to_string(),
                    task_context: "ctx".to_string(),
                    task_type: TaskType::ToolCall,
                    actor: Actor::Agent,
                    was_useful: Some(false),
                    useful_annotation_source: Some(AnnotationSource::AutoEvaluator),
                    annotation_confidence: Some(0.8),
                    tokens_consumed: 1,
                })
                .await
                .unwrap();
        }
        writer.flush().await.unwrap();

        let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.path().join("analysis")));
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.mode = EvolutionMode::Auto;
        cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
        let shared = new_shared_evolution_config(cfg);

        for offset in (0..3).rev() {
            let _ = analyzer
                .generate_daily_digest(now - chrono::Duration::days(offset))
                .await;
        }

        // Supervised + require_approval_for_medium_risk (the default) denies a
        // Medium-risk evolution commit when no runtime grant is present.
        let policy = Arc::new(SecurityPolicy {
            workspace_dir: dir.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let pipeline =
            EvolutionPipeline::new(shared, analyzer, writer.clone(), dir.path()).with_security_policy(policy);

        let invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sentinel_path = dir.path().join("engine_sentinel.txt");
        let mut engine = WriteSpyEngine {
            invoked: Arc::clone(&invoked),
            sentinel_path: sentinel_path.clone(),
        };

        let report = pipeline
            .run_for_layer(EvolutionTrigger::CronTick, EvolutionLayer::Memory, &mut engine, now)
            .await
            .unwrap();

        // The seeded analyzer inputs (a declining task with user corrections over
        // three days) deterministically produce a candidate that passes the
        // evolution gate, so the pipeline reaches the side-effect gate. Assert it
        // so a regression in candidate generation can never silently turn this
        // fail-closed test into a no-op.
        assert!(
            report.selected_candidate.is_some(),
            "test setup must produce a candidate so the side-effect gate is actually exercised"
        );

        // Hard acceptance criteria for FIX-P0-40 fail-closed behaviour:
        assert!(report.gate_denied, "report must flag gate_denied on a deny decision");
        assert!(
            report.evolution_log.is_none(),
            "no evolution_log may be committed on deny"
        );
        assert!(
            !invoked.load(std::sync::atomic::Ordering::SeqCst),
            "engine run_cycle MUST NOT run when the gate denies the commit"
        );
        assert!(
            !sentinel_path.exists(),
            "engine must not write any file when the gate denies the commit"
        );
        // No evolution JSONL was appended by the pipeline either.
        let evo_root = storage_root.join("evolution");
        let mut appended = false;
        for tier in ["hot", "warm", "cold"] {
            let tier_dir = evo_root.join(tier);
            if let Ok(mut rd) = fs::read_dir(&tier_dir).await {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    if entry.path().extension().and_then(|v| v.to_str()) == Some("jsonl") {
                        let raw = fs::read_to_string(entry.path()).await.unwrap_or_default();
                        if !raw.trim().is_empty() {
                            appended = true;
                        }
                    }
                }
            }
        }
        assert!(!appended, "no evolution log line may be appended on a gate deny");

        // Structured gate rejection is recorded for observability.
        assert!(
            report
                .gate_rejections
                .iter()
                .any(|rejection| rejection.reason == "side_effect_gate_denied"),
            "a structured side_effect_gate_denied rejection must be recorded"
        );
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

        let pipeline = EvolutionPipeline::new(shared, analyzer, writer.clone(), dir.path());
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
            .unwrap_or_else(|_| TrendAnalysis {
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

    async fn gated_pipeline(dir: &std::path::Path, policy: Arc<SecurityPolicy>) -> EvolutionPipeline {
        let storage_root = dir.join("logs");
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(storage_root.clone()),
                JsonlRetentionPolicy::default(),
                1,
            )
            .await
            .unwrap(),
        );
        let analyzer = Arc::new(EvolutionAnalyzer::new(writer.clone(), dir.join("analysis")));
        let mut cfg = EvolutionConfig::default();
        cfg.runtime.storage_dir = storage_root.to_string_lossy().to_string();
        let shared = new_shared_evolution_config(cfg);
        EvolutionPipeline::new(shared, analyzer, writer, dir).with_security_policy(policy)
    }

    #[tokio::test]
    async fn authorize_commit_is_noop_without_policy() {
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
        // No policy installed → legacy behaviour, commit authorization is a no-op.
        let pipeline = EvolutionPipeline::new(shared, analyzer, writer, dir.path());
        assert!(pipeline.authorize_commit(&EvolutionLayer::Memory, "exp-x").is_ok());
    }

    #[tokio::test]
    async fn authorize_commit_blocks_under_supervised_policy_without_grant() {
        let dir = tempdir().unwrap();
        // Explicit Supervised policy requires a runtime grant for this
        // Medium-risk evolution commit.
        let policy = Arc::new(SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        });
        let pipeline = gated_pipeline(dir.path(), policy).await;
        let reason = pipeline
            .authorize_commit(&EvolutionLayer::Prompt, "exp-blocked")
            .expect_err("supervised policy must block ungranted evolution commit");
        // The raw gate deny reason is surfaced so the caller can record it as a
        // structured GateRejection detail.
        assert!(
            reason.contains("requires runtime approval grant"),
            "unexpected deny reason: {reason}"
        );
    }

    #[tokio::test]
    async fn authorize_commit_allows_under_full_policy() {
        let dir = tempdir().unwrap();
        // Full autonomy → no medium-risk approval requirement, commit authorized.
        let policy = Arc::new(SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Full,
            ..SecurityPolicy::default()
        });
        let pipeline = gated_pipeline(dir.path(), policy).await;
        assert!(pipeline.authorize_commit(&EvolutionLayer::Memory, "exp-ok").is_ok());
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
        let source_before = fs::read_to_string(&path).await.unwrap();

        // FIX-P1-09: backfill now appends to a derived result_history.jsonl under file lock,
        // never mutating the source evolution jsonl in place.
        let history_path = storage_root.join("evolution").join("result_history.jsonl");
        fs::write(&history_path, "").await.unwrap();
        let guard = acquire_file_lock(&history_path).await.unwrap();
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

        // Derived history holds the inferred result; source jsonl is byte-for-byte unchanged.
        let history = fs::read_to_string(&history_path).await.unwrap();
        assert!(history.contains("\"result\":\"neutral\""));
        assert!(history.contains("\"experiment_id\":\"exp-lock\""));
        let source_after = fs::read_to_string(&path).await.unwrap();
        assert_eq!(source_before, source_after);
    }

    #[tokio::test]
    async fn backfill_results_is_idempotent_across_reruns() {
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
        let line = r#"{"experiment_id":"exp-once","timestamp":"2026-02-20T00:00:00Z","layer":"memory","change_type":"tune","before_value":"a","after_value":"b","trigger_reason":"r","data_basis":{"sample_count":1,"time_range_days":1,"key_metrics":{},"patterns_found":[]},"result":null}"#;
        fs::write(&path, format!("{line}\n")).await.unwrap();

        let now = chrono::DateTime::parse_from_rfc3339("2026-02-24T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let first = pipeline.backfill_results(now).await.unwrap();
        let second = pipeline.backfill_results(now).await.unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0, "already-backfilled experiment must not be re-appended");

        let history_path = storage_root.join("evolution").join("result_history.jsonl");
        let history = fs::read_to_string(&history_path).await.unwrap();
        let count = history.lines().filter(|l| l.contains("exp-once")).count();
        assert_eq!(count, 1);
    }
}
