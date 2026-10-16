//! Evaluation receipts, case judgement and receipt verification (SL06).
//!
//! An evaluation compares two arms (a baseline revision, or no skill at all,
//! and a candidate revision) on the cases of one split of a frozen suite. Every
//! run produces a [`CaseReceipt`] bound to the suite digest, the case id, the
//! arm's revision and bundle hash, the skill generation the proposal observed,
//! and a run receipt. Nothing is taken from the proposer: the verdict of a
//! case is derived here from what the executor observed.
//!
//! Hard rules, enforced by [`assess`] and therefore by
//! [`SkillLifecycleStore::verify_evaluation`], which recomputes the decision
//! from the stored receipts and never trusts a stored verdict:
//! * a zero-body, partial, timed-out, cancelled, unclean or unbound run is
//!   invalid and can never be a pass, and an invalid run makes the whole
//!   evaluation incomplete (it is not silently dropped from the denominator);
//! * a missing, extra, duplicated or foreign case receipt makes the evaluation
//!   incomplete;
//! * a skill generation that moved since the proposal observed it makes the
//!   evaluation stale;
//! * only [`EvaluationOutcome::Improved`] is a pass, and a tie is not an
//!   improvement.

use super::bundle::Bundle;
use super::capture::ExperienceOutcome;
use super::store::{SkillLifecycleStore, check_private, read_record, record_bytes};
use super::suite::{EvalCase, EvalSplit, FixtureFile, FrozenSuite, Validator};
use super::types::{LifecycleError, OwnerScope, RevisionId, SkillId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub const EVALUATION_SCHEMA_VERSION: u32 = 1;
/// Identity stamped on every receipt this runner emits.
pub const RUNNER_IDENTITY: &str = "prx-skill-evaluator/1";
pub(super) const EVALUATIONS_DIR: &str = "evaluations";
pub(super) const RECEIPT_FILE: &str = "receipt.json";
pub(super) const CASES_FILE: &str = "cases.jsonl";
const MAX_CASES_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EVALUATION_ENTRIES: usize = 4096;
const MAX_DETAIL_BYTES: usize = 240;
const EVALUATION_ID_PREFIX: &str = "eval-";

/// Failure to start, record or verify an evaluation. Every variant is a
/// refusal; none of them can be read as a pass.
#[derive(Debug, Error)]
pub enum EvaluationError {
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error("invalid evaluation suite: {0}")]
    InvalidSuite(String),
    #[error("evaluation is stale: {0}")]
    Stale(String),
    #[error("split {split:?} has {cases} cases but the suite requires at least {min}")]
    SplitTooSmall { split: EvalSplit, cases: usize, min: u32 },
    #[error("evaluation needs {needed} runs but the suite budget allows {allowed}")]
    RunBudget { needed: u64, allowed: u32 },
    #[error("the {split:?} split allows {limit} evaluation attempts per suite and {used} were already made")]
    AttemptsExhausted { split: EvalSplit, limit: u32, used: u32 },
    #[error("candidate and baseline are the same revision")]
    NothingToCompare,
    #[error("{0} not found")]
    NotFound(String),
    #[error("case executor failed: {0}")]
    Executor(String),
}

/// Which side of the pair a run belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    Baseline,
    Candidate,
}

/// How the run ended, as observed by the executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTerminal {
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

/// Whether the throw-away task workspace was removed after the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupState {
    Removed,
    Failed,
}

/// The skill revision the executor installed in the task workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledSkill {
    pub skill_id: SkillId,
    pub revision_id: RevisionId,
    pub bundle_manifest_hash: String,
    /// Generation of the binding inside the task workspace's own store.
    pub generation: u64,
}

/// A skill read the runtime actually served, taken from its hydration receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HydrationFact {
    pub skill_id: SkillId,
    pub revision_id: RevisionId,
    pub generation: u64,
    pub resource_sha256: String,
}

/// Terminal join recorded by the runtime for the task that read the skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperienceTerminalFact {
    pub terminal_receipt_id: String,
    pub outcome: ExperienceOutcome,
}

/// Everything an executor observed about one run. The response and artifact
/// contents stay in memory for judgement; only their digests are persisted.
#[derive(Clone, Debug)]
pub struct RunObservation {
    pub run_id: String,
    pub terminal: RunTerminal,
    pub response: String,
    pub elapsed_ms: u64,
    pub environment_hash: String,
    pub installed: Option<InstalledSkill>,
    pub hydrations: Vec<HydrationFact>,
    pub experience_terminal: Option<ExperienceTerminalFact>,
    /// Requested artifact paths mapped to their content, `None` when absent.
    pub artifacts: BTreeMap<String, Option<Vec<u8>>>,
    pub cleanup: CleanupState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDigest {
    pub sha256: String,
    pub bytes: u64,
}

/// The persisted, content-free form of a [`RunObservation`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunReceipt {
    pub run_id: String,
    pub terminal: RunTerminal,
    pub response_sha256: String,
    pub response_bytes: u64,
    pub elapsed_ms: u64,
    pub environment_hash: String,
    pub installed: Option<InstalledSkill>,
    pub hydrations: Vec<HydrationFact>,
    pub experience_terminal: Option<ExperienceTerminalFact>,
    pub artifacts: BTreeMap<String, Option<ArtifactDigest>>,
    pub cleanup: CleanupState,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

impl RunReceipt {
    fn from_observation(observation: &RunObservation) -> Self {
        Self {
            run_id: observation.run_id.clone(),
            terminal: observation.terminal,
            response_sha256: sha256_hex(observation.response.as_bytes()),
            response_bytes: u64::try_from(observation.response.len()).unwrap_or(u64::MAX),
            elapsed_ms: observation.elapsed_ms,
            environment_hash: observation.environment_hash.clone(),
            installed: observation.installed.clone(),
            hydrations: observation.hydrations.clone(),
            experience_terminal: observation.experience_terminal.clone(),
            artifacts: observation
                .artifacts
                .iter()
                .map(|(path, content)| {
                    (
                        path.clone(),
                        content.as_ref().map(|bytes| ArtifactDigest {
                            sha256: sha256_hex(bytes),
                            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                        }),
                    )
                })
                .collect(),
            cleanup: observation.cleanup,
        }
    }

    /// Receipt of a run that produced no observation (the executor failed).
    fn unobserved(run_id: &str) -> Self {
        Self {
            run_id: run_id.to_string(),
            terminal: RunTerminal::Failed,
            response_sha256: sha256_hex(b""),
            response_bytes: 0,
            elapsed_ms: 0,
            environment_hash: String::new(),
            installed: None,
            hydrations: Vec::new(),
            experience_terminal: None,
            artifacts: BTreeMap::new(),
            cleanup: CleanupState::Removed,
        }
    }
}

/// Why a run is not usable as evidence either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidReason {
    ZeroBody,
    Partial,
    TimedOut,
    Cancelled,
    Unbound,
    ExecutorError,
    EnvironmentMismatch,
    DuplicateRun,
    CleanupFailed,
    MissingTerminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaseVerdict {
    Pass,
    /// The run was valid and the task was not done: stays in the denominator.
    Fail {
        reasons: Vec<String>,
    },
    /// The run proves nothing; the evaluation cannot pass.
    Invalid {
        reason: InvalidReason,
        detail: String,
    },
}

impl CaseVerdict {
    #[must_use]
    pub const fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    fn invalid(reason: InvalidReason, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > MAX_DETAIL_BYTES {
            let mut end = MAX_DETAIL_BYTES;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
        }
        Self::Invalid { reason, detail }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorOutcome {
    pub kind: String,
    pub passed: bool,
}

/// Evidence of one run of one case in one arm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseReceipt {
    pub schema_version: u32,
    pub record_id: String,
    pub owner_scope: OwnerScope,
    pub evaluation_id: String,
    pub suite_id: String,
    pub suite_digest: String,
    pub case_id: String,
    pub split: EvalSplit,
    pub trial: u32,
    pub arm: Arm,
    /// Position of this run in the counterbalanced execution order.
    pub order: u32,
    pub skill_id: SkillId,
    /// The arm's revision; `None` for a no-skill baseline.
    pub revision_id: Option<RevisionId>,
    pub bundle_manifest_hash: Option<String>,
    /// Generation of the skill the proposal observed (0: nothing published).
    pub observed_generation: u64,
    pub run: RunReceipt,
    /// Digest of `run`, so the run receipt is a bound, checkable part.
    pub run_receipt_digest: String,
    pub validators: Vec<ValidatorOutcome>,
    pub verdict: CaseVerdict,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmBinding {
    pub revision_id: RevisionId,
    pub bundle_manifest_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteReason {
    MissingCases { count: u32 },
    UnexpectedCase { case_id: String },
    DuplicateReceipt { case_id: String },
    Unbound { case_id: String, detail: String },
    InvalidRuns { count: u32 },
    Cancelled,
    NoCases,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EvaluationOutcome {
    /// Candidate strictly better, no critical regression, complete evidence.
    Improved,
    /// Complete evidence, no net gain (a tie is not an improvement).
    NotImproved,
    Regressed,
    Inconclusive {
        cases: u32,
        required: u32,
    },
    Incomplete {
        reason: IncompleteReason,
    },
    Stale,
    BudgetExceeded,
}

impl EvaluationOutcome {
    /// The only passing outcome.
    #[must_use]
    pub const fn is_pass(&self) -> bool {
        matches!(self, Self::Improved)
    }
}

/// Counts derived from the case receipts (never supplied by a caller).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationSummary {
    pub cases: u32,
    pub trials: u32,
    pub baseline_passed: u32,
    pub candidate_passed: u32,
    pub improved: u32,
    pub regressed: u32,
    pub critical_regressions: u32,
    pub critical_failures: u32,
    pub invalid_runs: u32,
    pub total_elapsed_ms: u64,
    pub failure_categories: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReceipt {
    pub schema_version: u32,
    pub record_id: String,
    pub owner_scope: OwnerScope,
    pub suite_id: String,
    pub suite_digest: String,
    pub split: EvalSplit,
    pub skill_id: SkillId,
    pub baseline: Option<ArmBinding>,
    pub candidate: ArmBinding,
    pub observed_generation: u64,
    /// Generation of the skill when the last run ended.
    pub end_generation: u64,
    /// Environment binding shared by every run (`None` when no run was observed).
    pub environment_hash: Option<String>,
    pub runner: String,
    pub executor: String,
    pub trials: u32,
    pub planned_cases: Vec<String>,
    pub planned_runs: u32,
    pub recorded_runs: u32,
    /// Digest of `cases.jsonl` as written.
    pub cases_digest: String,
    pub total_elapsed_ms: u64,
    pub cancelled: bool,
    pub budget_exhausted: bool,
    pub stale: bool,
    pub outcome: EvaluationOutcome,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sealed<T> {
    receipt: T,
    digest: String,
}

const SEAL_DOMAIN: &[u8] = b"prx-skill-eval-receipt\0";

pub(super) fn seal_digest<T: Serialize>(value: &T) -> Result<String, EvaluationError> {
    let bytes = serde_json::to_vec(value).map_err(|e| {
        EvaluationError::Lifecycle(LifecycleError::InvalidRecord {
            path: EVALUATIONS_DIR.into(),
            reason: e.to_string(),
        })
    })?;
    let mut hasher = Sha256::new();
    hasher.update(SEAL_DOMAIN);
    hasher.update(EVALUATION_SCHEMA_VERSION.to_be_bytes());
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Execution contract

/// The skill to install in a task workspace.
#[derive(Clone, Copy, Debug)]
pub struct InstallSkill<'a> {
    pub skill_id: &'a SkillId,
    pub bundle: &'a Bundle,
}

/// What an executor is handed. It carries the task, never the oracle: the
/// validators and their expected values are not reachable from here.
#[derive(Debug)]
pub struct ExecutionRequest<'a> {
    pub run_id: &'a str,
    pub prompt: &'a str,
    pub fixtures: &'a [FixtureFile],
    /// Workspace-relative files to read back after the run.
    pub collect_artifacts: &'a [String],
    pub skill: Option<InstallSkill<'a>>,
    pub deadline: Duration,
}

/// Runs one case in a fresh owned workspace through the real agent path.
#[async_trait::async_trait]
pub trait CaseExecutor: Send + Sync {
    /// Stable description of the executor stamped on the evaluation receipt.
    fn identity(&self) -> String;

    async fn run_case(
        &self,
        request: &ExecutionRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<RunObservation, EvaluationError>;
}

// ---------------------------------------------------------------------------
// Judgement

fn run_validators(
    case: &EvalCase,
    response: &str,
    artifacts: &BTreeMap<String, Option<Vec<u8>>>,
) -> Vec<ValidatorOutcome> {
    case.validators
        .iter()
        .map(|validator| {
            let artifact = validator
                .artifact_path()
                .and_then(|path| artifacts.get(path))
                .and_then(Option::as_ref);
            let passed = match validator {
                Validator::ResponseContains { text } => response.contains(text.as_str()),
                Validator::ResponseExcludes { text } => !response.contains(text.as_str()),
                Validator::ResponseSha256 { sha256 } => sha256_hex(response.as_bytes()) == *sha256,
                Validator::ArtifactContains { text, .. } => {
                    artifact.is_some_and(|bytes| String::from_utf8_lossy(bytes).contains(text.as_str()))
                }
                Validator::ArtifactSha256 { sha256, .. } => artifact.is_some_and(|bytes| sha256_hex(bytes) == *sha256),
                Validator::ArtifactAbsent { .. } => artifact.is_none(),
            };
            ValidatorOutcome {
                kind: validator.kind().to_string(),
                passed,
            }
        })
        .collect()
}

/// Binding of a run to the arm it was supposed to exercise.
fn check_binding(
    observation: &RunObservation,
    expected_run_id: &str,
    skill_id: &SkillId,
    arm: Option<&ArmBinding>,
) -> Result<(), String> {
    if observation.run_id != expected_run_id {
        return Err("the run receipt names a different run id than the one issued".to_string());
    }
    match (arm, &observation.installed) {
        (None, None) => {
            if observation.hydrations.is_empty() {
                Ok(())
            } else {
                Err("a skill was read in an arm that has no skill".to_string())
            }
        }
        (None, Some(_)) => Err("a skill was installed in an arm that has no skill".to_string()),
        (Some(_), None) => Err("the arm's skill revision was not installed".to_string()),
        (Some(binding), Some(installed)) => {
            if installed.skill_id != *skill_id
                || installed.revision_id != binding.revision_id
                || installed.bundle_manifest_hash != binding.bundle_manifest_hash
            {
                return Err("the installed skill is not the arm's revision".to_string());
            }
            for hydration in &observation.hydrations {
                if hydration.skill_id != *skill_id
                    || hydration.revision_id != binding.revision_id
                    || hydration.generation != installed.generation
                {
                    return Err("a skill read does not belong to the arm's revision".to_string());
                }
            }
            Ok(())
        }
    }
}

/// Derives the verdict of one run. Order matters: unusable evidence is
/// reported as invalid before any validator can make it look like a pass.
pub(super) fn judge_run(
    case: &EvalCase,
    observation: &RunObservation,
    expected_run_id: &str,
    skill_id: &SkillId,
    arm: Option<&ArmBinding>,
) -> (CaseVerdict, Vec<ValidatorOutcome>) {
    if let Err(detail) = check_binding(observation, expected_run_id, skill_id, arm) {
        return (CaseVerdict::invalid(InvalidReason::Unbound, detail), Vec::new());
    }
    let invalid = |reason, detail: &str| (CaseVerdict::invalid(reason, detail), Vec::new());
    match observation.terminal {
        RunTerminal::Completed => {}
        RunTerminal::Failed => return invalid(InvalidReason::Partial, "the run did not reach a completed terminal"),
        RunTerminal::TimedOut => return invalid(InvalidReason::TimedOut, "the run exceeded its elapsed ceiling"),
        RunTerminal::Cancelled => return invalid(InvalidReason::Cancelled, "the run was cancelled"),
    }
    if observation.cleanup != CleanupState::Removed {
        return invalid(InvalidReason::CleanupFailed, "the task workspace was not removed");
    }
    if observation.response.trim().is_empty() {
        return invalid(InvalidReason::ZeroBody, "the run returned no response body");
    }
    if !observation.hydrations.is_empty() && observation.experience_terminal.is_none() {
        return invalid(
            InvalidReason::MissingTerminal,
            "the skill was read but no terminal was joined to the task",
        );
    }
    let validators = run_validators(case, &observation.response, &observation.artifacts);
    let mut reasons: Vec<String> = validators
        .iter()
        .filter(|outcome| !outcome.passed)
        .map(|outcome| format!("validator:{}", outcome.kind))
        .collect();
    if arm.is_some() {
        let read = !observation.hydrations.is_empty();
        if read != case.expect_skill_read {
            reasons.push(
                if read {
                    "skill_read_unexpected"
                } else {
                    "skill_not_read"
                }
                .to_string(),
            );
        }
    }
    if reasons.is_empty() {
        (CaseVerdict::Pass, validators)
    } else {
        (CaseVerdict::Fail { reasons }, validators)
    }
}

/// What every receipt of one evaluation shares.
#[derive(Clone, Copy)]
pub(super) struct CaseContext<'a> {
    pub owner: &'a OwnerScope,
    pub evaluation_id: &'a str,
    pub suite: &'a FrozenSuite,
    pub observed_generation: u64,
}

/// One planned run.
#[derive(Clone, Copy)]
pub(super) struct RunPlan<'a> {
    pub case: &'a EvalCase,
    pub trial: u32,
    pub arm: Arm,
    pub order: u32,
    pub arm_binding: Option<&'a ArmBinding>,
}

/// Builds the receipt of a run. `observation` is `None` when the executor
/// failed to produce one; `forced` replaces the derived verdict with an
/// invalidity found by the evaluator itself (duplicate run, environment drift).
pub(super) fn build_case_receipt(
    context: &CaseContext<'_>,
    plan: &RunPlan<'_>,
    run_id: &str,
    observation: Option<&RunObservation>,
    forced: Option<CaseVerdict>,
) -> Result<CaseReceipt, EvaluationError> {
    let CaseContext {
        owner,
        evaluation_id,
        suite,
        observed_generation,
    } = *context;
    let RunPlan {
        case,
        trial,
        arm,
        order,
        arm_binding,
    } = *plan;
    let skill_id = suite.suite().skill_id.clone();
    let (mut verdict, validators, run) = observation.map_or_else(
        || {
            (
                CaseVerdict::invalid(InvalidReason::ExecutorError, "the executor produced no observation"),
                Vec::new(),
                RunReceipt::unobserved(run_id),
            )
        },
        |observation| {
            let (verdict, validators) = judge_run(case, observation, run_id, &skill_id, arm_binding);
            (verdict, validators, RunReceipt::from_observation(observation))
        },
    );
    if let Some(forced) = forced {
        verdict = forced;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"prx-skill-case-receipt\0");
    for part in [
        evaluation_id,
        case.case_id.as_str(),
        &trial.to_string(),
        &format!("{arm:?}"),
    ] {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let id = hex::encode(hasher.finalize());
    let run_receipt_digest = seal_digest(&run)?;
    Ok(CaseReceipt {
        schema_version: EVALUATION_SCHEMA_VERSION,
        record_id: format!("case-{}", id.get(..32).unwrap_or(&id)),
        owner_scope: owner.clone(),
        evaluation_id: evaluation_id.to_string(),
        suite_id: suite.suite().suite_id.clone(),
        suite_digest: suite.digest().to_string(),
        case_id: case.case_id.clone(),
        split: case.split,
        trial,
        arm,
        order,
        skill_id,
        revision_id: arm_binding.map(|binding| binding.revision_id.clone()),
        bundle_manifest_hash: arm_binding.map(|binding| binding.bundle_manifest_hash.clone()),
        observed_generation,
        run,
        run_receipt_digest,
        validators,
        verdict,
        recorded_at: Utc::now(),
    })
}

// ---------------------------------------------------------------------------
// Assessment: the decision, recomputed from receipts

type CaseKey = (String, u32, Arm);

/// Why a case receipt does not belong to this evaluation, if it does not.
fn binding_defect(receipt: &EvaluationReceipt, case: &CaseReceipt) -> Option<String> {
    if case.schema_version != EVALUATION_SCHEMA_VERSION {
        return Some("unsupported case receipt schema".to_string());
    }
    let same_evaluation = case.evaluation_id == receipt.record_id;
    if !same_evaluation
        || case.suite_digest != receipt.suite_digest
        || case.suite_id != receipt.suite_id
        || case.owner_scope != receipt.owner_scope
        || case.skill_id != receipt.skill_id
        || case.split != receipt.split
        || case.observed_generation != receipt.observed_generation
    {
        return Some(
            "the case receipt is bound to a different evaluation, suite, owner, skill or generation".to_string(),
        );
    }
    let arm = match case.arm {
        Arm::Baseline => receipt.baseline.as_ref(),
        Arm::Candidate => Some(&receipt.candidate),
    };
    let revision_matches = case.revision_id.as_ref() == arm.map(|binding| &binding.revision_id)
        && case.bundle_manifest_hash.as_ref() == arm.map(|binding| &binding.bundle_manifest_hash);
    if !revision_matches {
        return Some("the case receipt names a different revision than its arm".to_string());
    }
    if !matches!(case.verdict, CaseVerdict::Invalid { .. })
        && receipt
            .environment_hash
            .as_deref()
            .is_none_or(|hash| hash != case.run.environment_hash)
    {
        return Some("the run environment differs from the evaluation's".to_string());
    }
    match seal_digest(&case.run) {
        Ok(digest) if digest == case.run_receipt_digest => None,
        _ => Some("the run receipt digest does not match".to_string()),
    }
}

/// Recomputes the outcome and summary from the stored receipts.
pub(super) fn assess(
    suite: &FrozenSuite,
    receipt: &EvaluationReceipt,
    cases: &[CaseReceipt],
) -> (EvaluationOutcome, EvaluationSummary) {
    let mut summary = EvaluationSummary {
        trials: suite.suite().trials,
        total_elapsed_ms: cases.iter().map(|case| case.run.elapsed_ms).sum(),
        ..EvaluationSummary::default()
    };
    let split_cases: Vec<&EvalCase> = suite.cases_of(receipt.split).collect();
    summary.cases = u32::try_from(split_cases.len()).unwrap_or(u32::MAX);
    let mut expected: BTreeSet<CaseKey> = BTreeSet::new();
    for case in &split_cases {
        for trial in 0..suite.suite().trials {
            expected.insert((case.case_id.clone(), trial, Arm::Baseline));
            expected.insert((case.case_id.clone(), trial, Arm::Candidate));
        }
    }
    let mut recorded: BTreeMap<CaseKey, &CaseReceipt> = BTreeMap::new();
    let mut defect: Option<IncompleteReason> = None;
    for case in cases {
        let key = (case.case_id.clone(), case.trial, case.arm);
        if !expected.contains(&key) {
            defect.get_or_insert_with(|| IncompleteReason::UnexpectedCase {
                case_id: case.case_id.clone(),
            });
            continue;
        }
        if let Some(detail) = binding_defect(receipt, case) {
            defect.get_or_insert_with(|| IncompleteReason::Unbound {
                case_id: case.case_id.clone(),
                detail,
            });
            continue;
        }
        if recorded.insert(key, case).is_some() {
            defect.get_or_insert_with(|| IncompleteReason::DuplicateReceipt {
                case_id: case.case_id.clone(),
            });
        }
    }
    for case in recorded.values() {
        if let CaseVerdict::Fail { reasons } = &case.verdict {
            for reason in reasons {
                *summary.failure_categories.entry(reason.clone()).or_insert(0) += 1;
            }
        }
        if matches!(case.verdict, CaseVerdict::Invalid { .. }) {
            summary.invalid_runs += 1;
        }
    }
    let missing = expected.iter().filter(|key| !recorded.contains_key(*key)).count();
    // A case passes in an arm only when every trial of it passed.
    let arm_passes = |case_id: &str, arm: Arm| {
        (0..suite.suite().trials).all(|trial| {
            recorded
                .get(&(case_id.to_string(), trial, arm))
                .is_some_and(|case| case.verdict.is_pass())
        })
    };
    for case in &split_cases {
        let baseline = arm_passes(&case.case_id, Arm::Baseline);
        let candidate = arm_passes(&case.case_id, Arm::Candidate);
        summary.baseline_passed += u32::from(baseline);
        summary.candidate_passed += u32::from(candidate);
        summary.improved += u32::from(candidate && !baseline);
        summary.regressed += u32::from(baseline && !candidate);
        summary.critical_regressions += u32::from(case.critical && baseline && !candidate);
        summary.critical_failures += u32::from(case.critical && !candidate);
    }
    let outcome = if receipt.stale {
        EvaluationOutcome::Stale
    } else if let Some(reason) = defect {
        EvaluationOutcome::Incomplete { reason }
    } else if split_cases.is_empty() {
        EvaluationOutcome::Incomplete {
            reason: IncompleteReason::NoCases,
        }
    } else if receipt.cancelled {
        EvaluationOutcome::Incomplete {
            reason: IncompleteReason::Cancelled,
        }
    } else if missing > 0 {
        EvaluationOutcome::Incomplete {
            reason: IncompleteReason::MissingCases {
                count: u32::try_from(missing).unwrap_or(u32::MAX),
            },
        }
    } else if summary.invalid_runs > 0 {
        EvaluationOutcome::Incomplete {
            reason: IncompleteReason::InvalidRuns {
                count: summary.invalid_runs,
            },
        }
    } else if receipt.budget_exhausted || summary.total_elapsed_ms > suite.suite().budget.max_total_elapsed_ms {
        EvaluationOutcome::BudgetExceeded
    } else if summary.cases < suite.suite().min_cases {
        EvaluationOutcome::Inconclusive {
            cases: summary.cases,
            required: suite.suite().min_cases,
        }
    } else if summary.critical_regressions > 0 || summary.regressed > suite.suite().allowed_regressions {
        EvaluationOutcome::Regressed
    } else if summary.critical_failures > 0 || summary.candidate_passed <= summary.baseline_passed {
        EvaluationOutcome::NotImproved
    } else {
        EvaluationOutcome::Improved
    };
    (outcome, summary)
}

// ---------------------------------------------------------------------------
// Persistence and verification

/// Evaluation receipt re-read from the store with every binding checked and
/// the outcome recomputed from the case receipts.
#[derive(Clone, Debug)]
pub struct VerifiedEvaluation {
    pub receipt: EvaluationReceipt,
    pub cases: Vec<CaseReceipt>,
    pub summary: EvaluationSummary,
    pub outcome: EvaluationOutcome,
    /// Digest of the sealed receipt; a publication journal can pin it.
    pub receipt_digest: String,
}

fn is_evaluation_id(id: &str) -> bool {
    id.strip_prefix(EVALUATION_ID_PREFIX)
        .is_some_and(|rest| rest.len() == 32 && rest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

pub(super) fn new_evaluation_id() -> String {
    format!("{EVALUATION_ID_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

fn read_cases_file(path: &std::path::Path) -> Result<Vec<u8>, LifecycleError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|e| LifecycleError::io(path, e))?;
    let meta = file.metadata().map_err(|e| LifecycleError::io(path, e))?;
    if !meta.is_file() || meta.len() > MAX_CASES_FILE_BYTES {
        return Err(LifecycleError::InvalidRecord {
            path: path.to_path_buf(),
            reason: "not a regular file within the case receipt size limit".to_string(),
        });
    }
    let mut bytes = Vec::new();
    file.take(MAX_CASES_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| LifecycleError::io(path, e))?;
    Ok(bytes)
}

fn tampered(path: &std::path::Path, reason: impl Into<String>) -> EvaluationError {
    EvaluationError::Lifecycle(LifecycleError::Tampered {
        path: path.to_path_buf(),
        reason: reason.into(),
    })
}

impl SkillLifecycleStore {
    pub(super) fn evaluation_dir(&self, id: &str) -> std::path::PathBuf {
        self.root().join(EVALUATIONS_DIR).join(id)
    }

    /// Writes `cases.jsonl` and then `receipt.json` (the commit marker) of a
    /// new evaluation directory. An evaluation is immutable once written.
    pub(super) fn write_evaluation(
        &self,
        receipt: &EvaluationReceipt,
        cases: &[CaseReceipt],
    ) -> Result<String, EvaluationError> {
        let dir = self.evaluation_dir(&receipt.record_id);
        let parent = self.root().join(EVALUATIONS_DIR);
        super::generation::ensure_dir_synced(self.root(), &parent)?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir).map_err(|e| LifecycleError::io(&dir, e))?;
        let mut lines = Vec::new();
        for case in cases {
            let sealed = Sealed {
                digest: seal_digest(case)?,
                receipt: case,
            };
            serde_json::to_writer(&mut lines, &sealed).map_err(|e| LifecycleError::InvalidRecord {
                path: dir.clone(),
                reason: e.to_string(),
            })?;
            lines.push(b'\n');
        }
        let cases_path = dir.join(CASES_FILE);
        self.write_atomic(&receipt.skill_id, &cases_path, &lines)?;
        let digest = seal_digest(receipt)?;
        let sealed = Sealed {
            receipt,
            digest: digest.clone(),
        };
        let receipt_path = dir.join(RECEIPT_FILE);
        self.write_atomic(&receipt.skill_id, &receipt_path, &record_bytes(&sealed, &receipt_path)?)?;
        Ok(digest)
    }

    /// Digest of the case receipts exactly as they are written.
    pub(super) fn cases_digest(cases: &[CaseReceipt]) -> Result<String, EvaluationError> {
        let mut lines = Vec::new();
        for case in cases {
            let sealed = Sealed {
                digest: seal_digest(case)?,
                receipt: case,
            };
            serde_json::to_writer(&mut lines, &sealed).map_err(|e| LifecycleError::InvalidRecord {
                path: EVALUATIONS_DIR.into(),
                reason: e.to_string(),
            })?;
            lines.push(b'\n');
        }
        Ok(sha256_hex(&lines))
    }

    fn read_sealed_receipt(&self, id: &str) -> Result<(EvaluationReceipt, String), EvaluationError> {
        if !is_evaluation_id(id) {
            return Err(EvaluationError::NotFound(format!("evaluation {id:?}")));
        }
        let path = self.evaluation_dir(id).join(RECEIPT_FILE);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(EvaluationError::NotFound(format!("evaluation {id}")));
            }
            Err(e) => return Err(LifecycleError::io(&path, e).into()),
        }
        check_private(&path)?;
        let sealed: Sealed<EvaluationReceipt> = read_record(&path)?;
        if seal_digest(&sealed.receipt)? != sealed.digest {
            return Err(tampered(&path, "the evaluation receipt does not match its digest"));
        }
        self.check_owner_scope(&sealed.receipt.owner_scope)?;
        if sealed.receipt.record_id != id || sealed.receipt.schema_version != EVALUATION_SCHEMA_VERSION {
            return Err(tampered(&path, "the evaluation identity does not match its location"));
        }
        Ok((sealed.receipt, sealed.digest))
    }

    /// Ids of every recorded evaluation, bounded.
    pub fn list_evaluation_ids(&self) -> Result<Vec<String>, EvaluationError> {
        let dir = self.root().join(EVALUATIONS_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(LifecycleError::io(&dir, e).into()),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| LifecycleError::io(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_evaluation_id(&name) {
                continue;
            }
            if ids.len() >= MAX_EVALUATION_ENTRIES {
                return Err(LifecycleError::KnowledgeLimit {
                    what: "evaluation directories",
                    limit: MAX_EVALUATION_ENTRIES,
                }
                .into());
            }
            ids.push(name);
        }
        ids.sort();
        Ok(ids)
    }

    /// Evaluations already made on `split` of the suite with `digest`.
    pub(super) fn evaluation_attempts(&self, digest: &str, split: EvalSplit) -> Result<u32, EvaluationError> {
        let mut used = 0_u32;
        for id in self.list_evaluation_ids()? {
            match self.read_sealed_receipt(&id) {
                Ok((receipt, _)) => used += u32::from(receipt.suite_digest == digest && receipt.split == split),
                // An unreadable record still counts against a limited split:
                // unreadable history must not reopen the holdout.
                Err(_) => used += 1,
            }
        }
        Ok(used)
    }

    /// Reads an evaluation, verifies every binding and recomputes the outcome
    /// from the case receipts. A stored outcome that disagrees is tampering.
    /// With `expected_digest` (pinned elsewhere, e.g. by a publication
    /// journal) the sealed receipt must match it too.
    pub fn verify_evaluation(
        &self,
        id: &str,
        expected_digest: Option<&str>,
    ) -> Result<VerifiedEvaluation, EvaluationError> {
        let (receipt, receipt_digest) = self.read_sealed_receipt(id)?;
        let dir = self.evaluation_dir(id);
        if let Some(expected) = expected_digest
            && expected != receipt_digest
        {
            return Err(tampered(&dir, "the evaluation digest does not match the pinned digest"));
        }
        let suite = self.read_suite(&receipt.suite_digest)?;
        let cases_path = dir.join(CASES_FILE);
        check_private(&cases_path)?;
        let bytes = read_cases_file(&cases_path)?;
        if sha256_hex(&bytes) != receipt.cases_digest {
            return Err(tampered(
                &cases_path,
                "the case receipts do not match the evaluation's digest",
            ));
        }
        let mut cases = Vec::new();
        for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
            let sealed: Sealed<CaseReceipt> =
                serde_json::from_slice(line).map_err(|e| LifecycleError::InvalidRecord {
                    path: cases_path.clone(),
                    reason: e.to_string(),
                })?;
            if seal_digest(&sealed.receipt)? != sealed.digest {
                return Err(tampered(&cases_path, "a case receipt does not match its digest"));
            }
            cases.push(sealed.receipt);
        }
        // The arms must still resolve to exactly the recorded bundles.
        for arm in receipt.baseline.iter().chain(std::iter::once(&receipt.candidate)) {
            let revision = self.read_revision(&receipt.skill_id, &arm.revision_id, None)?;
            if revision.revision.bundle_manifest_hash != arm.bundle_manifest_hash {
                return Err(tampered(
                    &dir,
                    "a revision no longer matches the bundle hash the evaluation recorded",
                ));
            }
        }
        let (outcome, summary) = assess(&suite, &receipt, &cases);
        if outcome != receipt.outcome {
            return Err(tampered(
                &dir,
                "the stored outcome differs from the one recomputed from the case receipts",
            ));
        }
        Ok(VerifiedEvaluation {
            receipt,
            cases,
            summary,
            outcome,
            receipt_digest,
        })
    }

    /// Whether the skill is still at the generation the evaluation observed.
    /// A publication gate checks this immediately before committing.
    pub fn evaluation_is_current(&self, evaluation: &VerifiedEvaluation) -> Result<bool, EvaluationError> {
        let generation = self
            .active_binding(&evaluation.receipt.skill_id)?
            .map_or(0, |b| b.generation);
        Ok(generation == evaluation.receipt.observed_generation)
    }
}
