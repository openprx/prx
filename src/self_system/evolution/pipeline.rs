use crate::security::SideEffectGate;
use crate::security::policy::{ResourceRiskLevel, SecurityPolicy};
use crate::self_system::evolution::FitnessTrend;
use crate::self_system::evolution::analyzer::{
    CandidatePriority, DailyDigest, EvolutionAnalyzer, EvolutionCandidate, TrendAnalysis,
};
use crate::self_system::evolution::commit::CommitTracker;
use crate::self_system::evolution::config::SharedEvolutionConfig;
use crate::self_system::evolution::engine::EngineCycleInput;
use crate::self_system::evolution::engine::EvolutionEngine;
use crate::self_system::evolution::gate::{EvolutionGate, GateMetrics, GateRejection, GateResult};
use crate::self_system::evolution::judge::{JudgeConfig, JudgeEngine, JudgeResult, JudgeScoringModel, MockJudgeModel};
use crate::self_system::evolution::record::{ChangeType, DataBasis, EvolutionLayer, EvolutionLog, EvolutionResult};
use crate::self_system::evolution::safety_utils::acquire_file_lock;
use crate::self_system::evolution::storage::AsyncJsonlWriter;
use crate::self_system::evolution::trace::generate_experiment_id;
use crate::self_system::fitness_store::FitnessStore;
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio_util::sync::CancellationToken;

const BACKFILL_DAYS: i64 = 3;
/// Derived, append-only file holding inferred outcomes for stale evolution logs.
/// Backfill writes here instead of mutating the source evolution JSONL in place,
/// preserving the append-only invariant of the primary audit log.
const RESULT_HISTORY_FILE: &str = "result_history.jsonl";
/// Default bound on one evolution judge call (D-044). The provider stack has
/// no whole-response timeout, so without it a stalled judge would hold an
/// applied change unjudged for as long as the connection stays open.
pub const DEFAULT_JUDGE_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(3);

/// What became of a cycle's verdict.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JudgeOutcome {
    /// No cycle ran, so nothing was judged.
    #[default]
    NotRun,
    /// A model judge scored the cycle.
    Scored,
    /// Only the mock scorer was available; the verdict was deferred.
    Degraded,
    /// The model judge gave no verdict (error, unparseable reply, timeout or
    /// cancellation); the verdict was deferred.
    Failed,
}

/// What the pipeline did with the change a cycle proposed (D-046). An
/// engine never writes its target: the change is written only after an
/// accepted verdict.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommitDecision {
    /// The cycle proposed nothing to write (DraftOnly, Shadow, a refused
    /// gate, a change that needs human approval).
    #[default]
    NothingProposed,
    /// A model judge accepted the change and it was written.
    Applied,
    /// A model judge scored the change below the pass threshold; nothing was
    /// written.
    Rejected,
    /// No verdict accepted the change in time (the judge failed or was only
    /// the mock, or the run was cancelled or shut down); nothing was written.
    Deferred,
    /// The verdict accepted the change but writing it failed.
    ApplyFailed,
}

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
    /// What became of the proposed change (D-046).
    #[serde(default)]
    pub commit: CommitDecision,
    /// FIX-P0-40: `true` when the side-effect gate denied this layer's commit and
    /// the engine was therefore never executed (no config/strategy/prompt write,
    /// no `append_evolution`). Lets the scheduler/daemon distinguish "completed"
    /// from "skipped by gate" instead of treating a denial as a silent success.
    pub gate_denied: bool,
    /// What became of the verdict; `Failed` and `Degraded` are deferrals.
    #[serde(default)]
    pub judge_outcome: JudgeOutcome,
    pub errors: Vec<String>,
}

/// Coordinates analyzer, gate, judge and rollback for each evolution layer.
pub struct EvolutionPipeline {
    analyzer: Arc<EvolutionAnalyzer>,
    writer: Arc<AsyncJsonlWriter>,
    gate: EvolutionGate,
    judge: Arc<JudgeEngine>,
    judge_timeout: std::time::Duration,
    /// Where verdict tasks run; the daemon passes the tracker it drains on
    /// shutdown (D-046).
    commits: CommitTracker,
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
            judge: Arc::new(judge),
            judge_timeout: DEFAULT_JUDGE_TIMEOUT,
            commits: CommitTracker::default(),
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

    /// Bound each judge call (D-044). A judge that has not answered when it
    /// elapses gives no verdict (`judge_failed`), so an applied change is
    /// rolled back.
    #[must_use]
    pub const fn with_judge_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.judge_timeout = timeout;
        self
    }

    /// Run verdict tasks on `commits`, so whoever owns it can cancel and
    /// await them (the daemon does, before it returns: D-046).
    #[must_use]
    pub fn with_commit_tracker(mut self, commits: CommitTracker) -> Self {
        self.commits = commits;
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
                commit: CommitDecision::NothingProposed,
                gate_denied: false,
                judge_outcome: JudgeOutcome::NotRun,
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
                commit: CommitDecision::NothingProposed,
                gate_denied: false,
                judge_outcome: JudgeOutcome::NotRun,
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
                commit: CommitDecision::NothingProposed,
                gate_denied: true,
                judge_outcome: JudgeOutcome::NotRun,
                errors,
            });
        }

        let fitness_trend = load_fitness_trend(&self.workspace_root)?;
        let cancel = self.commits.task_token();
        let _cancel_when_dropped = cancel.clone().drop_guard();
        if cancel.is_cancelled() {
            let reason = "evolution cycle cancelled before engine start";
            let log = build_log(&experiment_id, layer.clone(), &selected_candidate, reason);
            let persisted = log.clone();
            let writer = Arc::clone(&self.writer);
            let recorded = self
                .commits
                .spawn(async move {
                    writer.append_evolution(&persisted).await?;
                    writer.flush_lines().await
                })
                .await;
            errors.push(reason.into());
            match recorded {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(format!("record_failed: {error}")),
                Err(error) => errors.push(format!("record_failed: {error}")),
            }
            return Ok(PipelineRunReport {
                experiment_id,
                layer,
                trigger,
                digest,
                trend,
                selected_candidate: Some(selected_candidate),
                gate_rejections,
                judge_result: None,
                evolution_log: Some(log),
                shadow_mode: self.shared_config.load_full().runtime.mode.is_proposal_only(),
                commit: CommitDecision::Deferred,
                gate_denied: false,
                judge_outcome: JudgeOutcome::NotRun,
                errors,
            });
        }
        let envelope = crate::runtime::evolution_model::EvolutionEnvelope::new(experiment_id.clone());
        let cycle_result = match envelope
            .scope(engine.run_cycle_with_cancellation(
                EngineCycleInput {
                    cycle_id: experiment_id.clone(),
                    analyzer_candidates: vec![selected_candidate.clone()],
                    fitness_trend,
                },
                &cancel,
            ))
            .await
        {
            Ok(result) => result,
            Err(error)
                if error
                    .downcast_ref::<super::prompt_evolution::NoManagedTarget>()
                    .is_some() =>
            {
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
                    commit: CommitDecision::NothingProposed,
                    gate_denied: false,
                    judge_outcome: JudgeOutcome::NotRun,
                    errors,
                });
            }
            Err(error) => return Err(error),
        };

        let mut evolution_log = cycle_result.evolution_log.clone().unwrap_or_else(|| {
            build_log(
                &experiment_id,
                layer.clone(),
                &selected_candidate,
                &cycle_result.cycle.validation.notes,
            )
        });
        evolution_log.experiment_id = experiment_id.clone();

        // D-046: the engine has only proposed its change. Judging, the write
        // an accepted verdict allows, and the log row run in a task that owns
        // everything it needs. If this run is dropped, the drop guard cancels
        // the task's token: the task stops waiting for the judge, writes
        // nothing and records the cycle as deferred.
        let stage = VerdictStage {
            judge: Arc::clone(&self.judge),
            writer: Arc::clone(&self.writer),
            judge_timeout: self.judge_timeout,
            layer: layer.clone(),
            experiment_id: experiment_id.clone(),
            cycle_result,
            evolution_log,
        };
        // The task's token is cancelled when this run is dropped (the guard)
        // and when the tracker is drained (daemon shutdown); the daemon waits
        // for the task before it returns.
        let cleanup = crate::runtime::turn_cleanup::current_tracker();
        let verdict = self
            .commits
            .spawn(async move {
                match cleanup {
                    Some(owner) => envelope.scope(owner.scope(stage.run(cancel))).await,
                    None => envelope.scope(stage.run(cancel)).await,
                }
            })
            .await
            .map_err(|err| anyhow::anyhow!("evolution verdict task failed: {err}"))??;
        errors.extend(verdict.errors);
        if let Some(what) = verdict.cancelled {
            // A drain (daemon shutdown) cancelled the task while this run
            // still waited for it.
            errors.push(format!("cancelled: the run was cancelled {what}"));
        }

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
            judge_result: verdict.judge,
            evolution_log: Some(verdict.evolution_log),
            shadow_mode: verdict.shadow_mode,
            commit: verdict.commit,
            gate_denied: false,
            judge_outcome: verdict.outcome,
            errors,
        })
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
        let mut unreadable_files = 0u32;

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

                // An unreadable entry is skipped, like a malformed line; the
                // experiments it holds are backfilled by a later run that can
                // read it (each id is backfilled at most once).
                let raw = match fs::read_to_string(&path).await {
                    Ok(raw) => raw,
                    Err(error) => {
                        unreadable_files = unreadable_files.saturating_add(1);
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "skipped an unreadable evolution file during backfill scan"
                        );
                        continue;
                    }
                };
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

                    let Some(result) = self.infer_backfill_result(&parsed, now)? else {
                        continue;
                    };
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

        if unreadable_files > 0 {
            tracing::warn!(
                unreadable_files,
                "backfill scanned the evolution logs without the files that could not be read"
            );
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

    fn infer_backfill_result(&self, log: &EvolutionLog, _now: DateTime<Utc>) -> Result<Option<EvolutionResult>> {
        let Some(changed_at) = parse_rfc3339(&log.timestamp) else {
            tracing::warn!(
                timestamp = %log.timestamp,
                experiment_id = %log.experiment_id,
                "fitness backfill skipped invalid evolution timestamp"
            );
            return Ok(None);
        };
        let reports = FitnessStore::new(&self.workspace_root).history(365)?;
        Ok(infer_fitness_result(reports, changed_at))
    }

    fn writer_root(&self) -> PathBuf {
        self.shared_config.load_full().runtime.storage_dir.to_string().into()
    }
}

fn infer_fitness_result(
    reports: Vec<crate::self_system::fitness::FitnessReport>,
    changed_at: DateTime<Utc>,
) -> Option<EvolutionResult> {
    let mut before = Vec::new();
    let mut after = Vec::new();
    for report in reports {
        let Some(score) = report.final_score else {
            continue;
        };
        let Some(start) = parse_rfc3339(&report.window.start) else {
            continue;
        };
        let Some(end) = parse_rfc3339(&report.window.end) else {
            continue;
        };
        if end <= changed_at {
            before.push((end, score));
        } else if start >= changed_at {
            after.push((start, score));
        }
    }
    before.sort_by_key(|item| std::cmp::Reverse(item.0));
    after.sort_by_key(|item| item.0);
    let before = before.into_iter().take(3).map(|(_, score)| score).collect::<Vec<_>>();
    let after = after.into_iter().take(3).map(|(_, score)| score).collect::<Vec<_>>();
    if before.is_empty() || after.len() < 3 {
        return None;
    }
    let before_average = before.iter().sum::<f64>() / before.len() as f64;
    let after_average = after.iter().sum::<f64>() / after.len() as f64;
    let delta = after_average - before_average;
    Some(if delta > 0.01 {
        EvolutionResult::Improved
    } else if delta < -0.01 {
        EvolutionResult::Regressed
    } else {
        EvolutionResult::Neutral
    })
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

/// Everything the verdict of one engine cycle needs, owned, so that it can
/// finish in its own task when the run that started it is dropped.
struct VerdictStage {
    judge: Arc<JudgeEngine>,
    writer: Arc<AsyncJsonlWriter>,
    judge_timeout: std::time::Duration,
    layer: EvolutionLayer,
    experiment_id: String,
    cycle_result: crate::self_system::evolution::engine::CycleResult,
    evolution_log: EvolutionLog,
}

/// The verdict of one engine cycle, after any write and the log append.
struct Verdict {
    judge: Option<JudgeResult>,
    outcome: JudgeOutcome,
    commit: CommitDecision,
    evolution_log: EvolutionLog,
    shadow_mode: bool,
    /// How the task ended when it was cancelled (R-PMMCi F-3).
    cancelled: Option<&'static str>,
    errors: Vec<String>,
}

impl VerdictStage {
    /// Judge the proposal, write the change only if a model judge accepted
    /// it, and record the cycle. `cancel` fires when the run that started
    /// this stage is dropped: the judge is then abandoned, and nothing is
    /// written.
    async fn run(self, cancel: CancellationToken) -> Result<Verdict> {
        let Self {
            judge: judge_engine,
            writer,
            judge_timeout,
            layer,
            experiment_id,
            cycle_result,
            mut evolution_log,
        } = self;
        let mut errors = Vec::new();

        let task = cycle_result
            .proposal
            .as_ref()
            .map_or("evolution_cycle", |item| item.summary.as_str());
        // A judge that fails at call time (provider error, reply outside the
        // score schema), that does not answer within the timeout, or whose
        // run is cancelled gives no verdict (D-035): nothing is written.
        // R-PMMCi F-3: remember which branch won, so a later cancellation
        // warning says what actually happened.
        let cancelled_before_verdict;
        let judged = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                cancelled_before_verdict = true;
                Err(anyhow::anyhow!("the evolution run was cancelled before the judge answered"))
            }
            result = tokio::time::timeout(
                judge_timeout,
                judge_engine.judge_task(&experiment_id, &cycle_result.cycle.id, task, cycle_result.review_data.as_ref().map(|data| data.as_str()).unwrap_or(&cycle_result.cycle.validation.notes)),
            ) => {
                cancelled_before_verdict = false;
                result.unwrap_or_else(|_| {
                    Err(anyhow::anyhow!("the judge call timed out after {}s", judge_timeout.as_secs()))
                })
            }
        };
        let judge = match judged {
            Ok(judge) => Some(judge),
            Err(err) => {
                let err = crate::security::redaction::redact_provider_text(&format!("{err:#}"));
                errors.push(format!(
                    "judge_failed: the model judge gave no verdict ({err:#}), so the verdict is deferred and nothing \
                     is written"
                ));
                tracing::warn!(
                    target: "self_system",
                    layer = ?layer,
                    error = %err,
                    "evolution judge failed: the cycle's verdict is deferred"
                );
                None
            }
        };

        // D-035: a degraded (mock) judge cannot approve anything; its verdict
        // is a deferral.
        let mock_judge = judge_engine.is_degraded();
        let outcome = if judge.is_none() {
            JudgeOutcome::Failed
        } else if mock_judge {
            JudgeOutcome::Degraded
        } else {
            JudgeOutcome::Scored
        };
        if mock_judge {
            errors.push(
                "judge_degraded: no model judge is available, so the verdict is deferred and nothing is written"
                    .to_string(),
            );
            tracing::warn!(
                target: "self_system",
                layer = ?layer,
                "evolution judge is degraded: the cycle's verdict is deferred"
            );
        }
        let accepted = outcome == JudgeOutcome::Scored
            && judge
                .as_ref()
                .is_some_and(|judge| judge.scores.overall() >= judge_engine.pass_threshold());

        let commit = match cycle_result.pending.as_ref() {
            None => {
                evolution_log.result = match cycle_result.cycle.outcome {
                    crate::self_system::evolution::CycleOutcome::Applied => {
                        // D-046: engines propose; one that wrote its target
                        // itself broke the contract, and the pipeline cannot
                        // undo it.
                        errors.push(format!(
                            "engine_wrote_before_verdict: the {layer:?} engine applied its change itself; engines \
                             must return it as a pending change"
                        ));
                        None
                    }
                    crate::self_system::evolution::CycleOutcome::Failed => Some(EvolutionResult::Regressed),
                    _ => Some(EvolutionResult::Neutral),
                };
                CommitDecision::NothingProposed
            }
            Some(_) if outcome != JudgeOutcome::Scored => {
                evolution_log.result = Some(EvolutionResult::Deferred);
                CommitDecision::Deferred
            }
            Some(_) if !accepted => {
                evolution_log.result = Some(EvolutionResult::Rejected);
                CommitDecision::Rejected
            }
            Some(_) if cancel.is_cancelled() => {
                errors.push(
                    "commit_cancelled: the run was cancelled after an accepted verdict and before the change was \
                     written; nothing is written"
                        .to_string(),
                );
                evolution_log.result = Some(EvolutionResult::Deferred);
                CommitDecision::Deferred
            }
            Some(pending) => match pending.apply().await {
                Ok(()) => {
                    // Scored later by the fitness backfill.
                    evolution_log.result = None;
                    CommitDecision::Applied
                }
                Err(err) => {
                    let err = crate::security::redaction::redact_provider_text(&format!("{err:#}"));
                    errors.push(format!(
                        "apply_failed: the accepted change could not be written ({err:#})"
                    ));
                    evolution_log.result = Some(EvolutionResult::Deferred);
                    CommitDecision::ApplyFailed
                }
            },
        };

        // FIX-P0-40: the side-effect gate has already authorized this commit
        // *before* the engine ran; a denial returned early and never reached
        // here.
        // R-PMMCi F-2: the writer batches lines (64 in production) and
        // nothing flushes it on a timer or on drop, so a row left in its
        // buffer is lost with the generation. Evolution rows are rare: flush
        // each one at once. Retention runs separately: its failure does not
        // mean the row is missing.
        let appended = match writer.append_evolution(&evolution_log).await {
            Ok(()) => writer.flush_lines().await,
            Err(err) => Err(err),
        };
        if appended.is_ok()
            && let Err(err) = writer.enforce_retention().await
        {
            tracing::warn!(target: "self_system", error = %crate::security::redaction::redact_provider_text(&format!("{err:#}")), "evolution log retention failed");
        }
        let cancelled = cancel
            .is_cancelled()
            .then(|| cancellation_note(cancelled_before_verdict, outcome, commit));
        if let Some(what) = cancelled {
            // Nobody may await this task any more: the log line says what
            // happened.
            match &appended {
                Ok(()) => tracing::warn!(
                    target: "self_system",
                    layer = ?layer,
                    experiment_id = %experiment_id,
                    commit = ?commit,
                    errors = ?errors,
                    "evolution run was cancelled {what}; the cycle is recorded"
                ),
                Err(err) => tracing::error!(
                    target: "self_system",
                    layer = ?layer,
                    experiment_id = %experiment_id,
                    commit = ?commit,
                    errors = ?errors,
                    error = %crate::security::redaction::redact_provider_text(&format!("{err:#}")),
                    "evolution run was cancelled {what}, and its cycle could not be recorded"
                ),
            }
        }
        // R-PMMCj G-6: the commit decision stands whether or not its row
        // could be written; a lost row is reported, not turned into a failed
        // run that hides an applied change.
        if let Err(err) = appended {
            let err = crate::security::redaction::redact_provider_text(&format!("{err:#}"));
            // R-PMMCk K-3: a run nobody cancelled has no other log line for
            // this (the cancelled branch logged above).
            if cancelled.is_none() {
                tracing::error!(
                    target: "self_system",
                    layer = ?layer,
                    experiment_id = %experiment_id,
                    commit = ?commit,
                    error = %err,
                    "evolution cycle row could not be written; the commit decision stands"
                );
            }
            errors.push(format!(
                "record_failed: the cycle's evolution row could not be written ({err:#}); the commit decision \
                 stands"
            ));
        }

        Ok(Verdict {
            judge,
            outcome,
            commit,
            evolution_log,
            shadow_mode: cycle_result.shadow_mode,
            cancelled,
            errors,
        })
    }
}

/// How a cancelled verdict task ended, for its warning (R-PMMCi F-3). It
/// speaks of "its verdict" only when a model judge gave one (R-PMMCj G-5).
fn cancellation_note(cancelled_before_verdict: bool, outcome: JudgeOutcome, commit: CommitDecision) -> &'static str {
    if cancelled_before_verdict {
        return "before the judge answered: nothing was written";
    }
    if outcome != JudgeOutcome::Scored {
        return match commit {
            CommitDecision::NothingProposed => "after the judge gave no verdict: the cycle proposed nothing to write",
            _ => "after the judge gave no verdict: nothing was written",
        };
    }
    match commit {
        CommitDecision::Applied => "after its change was written: the change stands",
        CommitDecision::Deferred => "after its verdict and before any write: nothing was written",
        CommitDecision::Rejected => "after the judge rejected the change: nothing was written",
        CommitDecision::ApplyFailed => "while its accepted change failed to be written: nothing was written",
        CommitDecision::NothingProposed => "after its verdict: the cycle proposed nothing to write",
    }
}

/// Stable short name for an evolution layer, used in gate operation ids.
const fn layer_slug(layer: &EvolutionLayer) -> &'static str {
    match layer {
        EvolutionLayer::Memory => "memory",
        EvolutionLayer::Prompt => "prompt",
        EvolutionLayer::Policy => "strategy",
        EvolutionLayer::Tooling => "tooling",
        EvolutionLayer::Runtime => "runtime",
    }
}

/// Convert dedicated, publishable fitness snapshots into the trend carried by
/// an evolution cycle. Missing or provisional snapshots remain explicit and do
/// not become synthetic 0.5/0.6 values.
fn load_fitness_trend(workspace_root: &Path) -> Result<Option<FitnessTrend>> {
    let reports = FitnessStore::new(workspace_root).history(4)?;
    let scores: Vec<f64> = reports.into_iter().filter_map(|report| report.final_score).collect();
    let Some((&latest, previous)) = scores.split_first() else {
        return Ok(None);
    };
    let previous_average = (!previous.is_empty()).then(|| previous.iter().sum::<f64>() / previous.len() as f64);
    Ok(Some(FitnessTrend {
        window: scores.len(),
        previous_average,
        latest_score: Some(latest),
        is_declining: previous_average.map(|average| latest < average),
        source: "fitness_store.daily".to_string(),
    }))
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
mod tests;
