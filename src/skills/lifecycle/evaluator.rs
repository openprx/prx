//! The baseline-versus-candidate evaluator (SL06).
//!
//! [`SkillLifecycleStore::evaluate`] runs every case of one split of a frozen
//! suite, `trials` times per arm, through a [`CaseExecutor`], and records one
//! bound [`CaseReceipt`] per run plus the evaluation receipt. Order is
//! counterbalanced (baseline first on alternating runs), runs are sequential
//! (one evaluation, one run at a time) and fresh-workspace isolation belongs
//! to the executor. Nothing here chooses a "best" run or discards a case.

use super::bundle::Bundle;
use super::evaluate::{
    Arm, ArmBinding, CaseContext, CaseExecutor, CaseReceipt, CaseVerdict, EVALUATION_SCHEMA_VERSION, EvaluationError,
    EvaluationOutcome, EvaluationReceipt, ExecutionRequest, InstallSkill, InvalidReason, RUNNER_IDENTITY,
    RunObservation, RunPlan, assess, build_case_receipt, new_evaluation_id,
};
use super::store::SkillLifecycleStore;
use super::suite::{EvalCase, EvalSplit, FrozenSuite};
use super::types::RevisionId;
use chrono::Utc;
use std::collections::BTreeSet;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Extra time an executor gets past a case's own ceiling to clean up before
/// the evaluator gives up on it.
const EXECUTOR_GRACE: Duration = Duration::from_secs(30);

/// What to evaluate.
#[derive(Clone, Debug)]
pub struct EvaluationRequest<'a> {
    pub suite: &'a FrozenSuite,
    pub split: EvalSplit,
    /// Revision the candidate is compared with; `None` compares with the same
    /// task done without the skill (a new skill).
    pub baseline: Option<RevisionId>,
    pub candidate: RevisionId,
    /// Generation of the skill the proposal was written against.
    pub observed_generation: u64,
}

fn arm_binding(
    store: &SkillLifecycleStore,
    suite: &FrozenSuite,
    revision: &RevisionId,
) -> Result<(ArmBinding, Bundle), EvaluationError> {
    let verified = store.read_revision(&suite.suite().skill_id, revision, None)?;
    Ok((
        ArmBinding {
            revision_id: revision.clone(),
            bundle_manifest_hash: verified.revision.bundle_manifest_hash.clone(),
        },
        verified.bundle,
    ))
}

fn artifact_paths(case: &EvalCase) -> Vec<String> {
    let mut paths: Vec<String> = case
        .validators
        .iter()
        .filter_map(|validator| validator.artifact_path().map(str::to_string))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

impl SkillLifecycleStore {
    fn skill_generation(&self, suite: &FrozenSuite) -> Result<u64, EvaluationError> {
        Ok(self
            .active_binding(&suite.suite().skill_id)?
            .map_or(0, |binding| binding.generation))
    }

    /// Runs the evaluation and records its receipts.
    ///
    /// Refused before any run (no receipt, nothing spent): a stale request, a
    /// split below the suite's minimum sample, a run count over the budget, an
    /// exhausted attempt allowance on the validation or holdout split, and a
    /// candidate equal to its baseline. Once runs start, a receipt is always
    /// written, also when the evaluation is cancelled, over budget, stale at
    /// the end, or has invalid runs; such a receipt can never pass.
    pub async fn evaluate(
        &self,
        request: &EvaluationRequest<'_>,
        executor: &dyn CaseExecutor,
        cancel: &CancellationToken,
    ) -> Result<EvaluationReceipt, EvaluationError> {
        // Re-read the suite through the store: only a frozen, stored suite
        // whose digest verifies can be evaluated.
        self.write_suite(request.suite)?;
        let suite = self.read_suite(request.suite.digest())?;
        let spec = suite.suite();
        self.check_owner_scope(&spec.owner_scope)?;

        let cases: Vec<&EvalCase> = suite.cases_of(request.split).collect();
        if cases.len() < usize::try_from(spec.min_cases).unwrap_or(usize::MAX) {
            return Err(EvaluationError::SplitTooSmall {
                split: request.split,
                cases: cases.len(),
                min: spec.min_cases,
            });
        }
        let planned_runs = u64::try_from(cases.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(2)
            .saturating_mul(u64::from(spec.trials));
        if planned_runs > u64::from(spec.budget.max_runs) {
            return Err(EvaluationError::RunBudget {
                needed: planned_runs,
                allowed: spec.budget.max_runs,
            });
        }
        if let Some(limit) = suite.attempt_limit(request.split) {
            let used = self.evaluation_attempts(suite.digest(), request.split)?;
            if used >= limit {
                return Err(EvaluationError::AttemptsExhausted {
                    split: request.split,
                    limit,
                    used,
                });
            }
        }
        if request.baseline.as_ref() == Some(&request.candidate) {
            return Err(EvaluationError::NothingToCompare);
        }

        // Staleness before spending anything: the proposal must have been
        // written against the skill as it is now.
        let binding = self.active_binding(&spec.skill_id)?;
        let generation = binding.as_ref().map_or(0, |b| b.generation);
        if generation != request.observed_generation {
            return Err(EvaluationError::Stale(format!(
                "the skill is at generation {generation}, the proposal observed {}",
                request.observed_generation
            )));
        }
        if let Some(baseline) = &request.baseline
            && binding.as_ref().is_none_or(|b| b.revision_id != *baseline)
        {
            return Err(EvaluationError::Stale(
                "the baseline revision is not the active revision".to_string(),
            ));
        }
        let (candidate_binding, candidate_bundle) = arm_binding(self, &suite, &request.candidate)?;
        let baseline = request
            .baseline
            .as_ref()
            .map(|revision| arm_binding(self, &suite, revision))
            .transpose()?;

        let evaluation_id = new_evaluation_id();
        let context = CaseContext {
            owner: self.owner_scope(),
            evaluation_id: &evaluation_id,
            suite: &suite,
            observed_generation: request.observed_generation,
        };
        let started_at = Utc::now();
        let mut receipts: Vec<CaseReceipt> = Vec::new();
        let mut run_ids: BTreeSet<String> = BTreeSet::new();
        let mut environment_hash: Option<String> = None;
        let mut total_elapsed_ms: u64 = 0;
        let mut order: u32 = 0;
        let mut cancelled = false;
        let mut budget_exhausted = false;

        'cases: for (case_index, case) in cases.iter().enumerate() {
            let collect = artifact_paths(case);
            for trial in 0..spec.trials {
                // Counterbalanced: the arm that goes first alternates.
                let arms = if (case_index + usize::try_from(trial).unwrap_or(0)) % 2 == 0 {
                    [Arm::Baseline, Arm::Candidate]
                } else {
                    [Arm::Candidate, Arm::Baseline]
                };
                for arm in arms {
                    if cancel.is_cancelled() {
                        cancelled = true;
                        break 'cases;
                    }
                    if total_elapsed_ms >= spec.budget.max_total_elapsed_ms {
                        budget_exhausted = true;
                        break 'cases;
                    }
                    let (binding, bundle) = match arm {
                        Arm::Baseline => (
                            baseline.as_ref().map(|(binding, _)| binding),
                            baseline.as_ref().map(|(_, bundle)| bundle),
                        ),
                        Arm::Candidate => (Some(&candidate_binding), Some(&candidate_bundle)),
                    };
                    let run_id = format!("evalrun-{}", uuid::Uuid::new_v4().simple());
                    let deadline = Duration::from_millis(case.max_elapsed_ms);
                    let execution = ExecutionRequest {
                        run_id: &run_id,
                        prompt: &case.prompt,
                        fixtures: &case.fixtures,
                        collect_artifacts: &collect,
                        skill: bundle.map(|bundle| InstallSkill {
                            skill_id: &spec.skill_id,
                            bundle,
                        }),
                        deadline,
                    };
                    let observed = run_with_backstop(executor, &execution, cancel).await;
                    let (observation, forced) = match observed {
                        Ok(observation) => {
                            let forced = evaluator_invalidity(&observation, &mut run_ids, &mut environment_hash);
                            total_elapsed_ms = total_elapsed_ms.saturating_add(observation.elapsed_ms);
                            (Some(observation), forced)
                        }
                        Err(error) => {
                            tracing::warn!(case = %case.case_id, "evaluation case executor failed: {error}");
                            (None, None)
                        }
                    };
                    let plan = RunPlan {
                        case,
                        trial,
                        arm,
                        order,
                        arm_binding: binding,
                    };
                    receipts.push(build_case_receipt(
                        &context,
                        &plan,
                        &run_id,
                        observation.as_ref(),
                        forced,
                    )?);
                    order += 1;
                }
            }
        }

        // Stale again at the end: the skill may have been published meanwhile.
        let end_generation = self.skill_generation(&suite)?;
        let stale = end_generation != request.observed_generation;
        let mut receipt = EvaluationReceipt {
            schema_version: EVALUATION_SCHEMA_VERSION,
            record_id: evaluation_id.clone(),
            owner_scope: self.owner_scope().clone(),
            suite_id: spec.suite_id.clone(),
            suite_digest: suite.digest().to_string(),
            split: request.split,
            skill_id: spec.skill_id.clone(),
            baseline: baseline.map(|(binding, _)| binding),
            candidate: candidate_binding,
            observed_generation: request.observed_generation,
            end_generation,
            environment_hash,
            runner: RUNNER_IDENTITY.to_string(),
            executor: executor.identity(),
            trials: spec.trials,
            planned_cases: cases.iter().map(|case| case.case_id.clone()).collect(),
            planned_runs: u32::try_from(planned_runs).unwrap_or(u32::MAX),
            recorded_runs: u32::try_from(receipts.len()).unwrap_or(u32::MAX),
            cases_digest: Self::cases_digest(&receipts)?,
            total_elapsed_ms,
            cancelled,
            budget_exhausted,
            stale,
            outcome: EvaluationOutcome::Stale,
            started_at,
            finished_at: Utc::now(),
        };
        receipt.outcome = assess(&suite, &receipt, &receipts).0;
        self.write_evaluation(&receipt, &receipts)?;
        Ok(receipt)
    }
}

/// Invalidity only the evaluator can see across runs.
fn evaluator_invalidity(
    observation: &RunObservation,
    run_ids: &mut BTreeSet<String>,
    environment_hash: &mut Option<String>,
) -> Option<CaseVerdict> {
    if !run_ids.insert(observation.run_id.clone()) {
        return Some(CaseVerdict::Invalid {
            reason: InvalidReason::DuplicateRun,
            detail: "the run id was already used by another run of this evaluation".to_string(),
        });
    }
    match environment_hash {
        None => {
            *environment_hash = Some(observation.environment_hash.clone());
            None
        }
        Some(first) if *first != observation.environment_hash => Some(CaseVerdict::Invalid {
            reason: InvalidReason::EnvironmentMismatch,
            detail: "the run environment differs from the first run of this evaluation".to_string(),
        }),
        Some(_) => None,
    }
}

/// Runs the executor; an executor that outlives its deadline plus grace is cut
/// off and the run is recorded as invalid (executor error) rather than waited
/// for forever.
async fn run_with_backstop(
    executor: &dyn CaseExecutor,
    request: &ExecutionRequest<'_>,
    cancel: &CancellationToken,
) -> Result<RunObservation, EvaluationError> {
    tokio::time::timeout(request.deadline + EXECUTOR_GRACE, executor.run_case(request, cancel))
        .await
        .unwrap_or_else(|_| {
            Err(EvaluationError::Executor(
                "the executor did not return within its deadline".to_string(),
            ))
        })
}
