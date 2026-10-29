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
use super::suite::{EvalCase, EvalSplit, FixtureFile, FrozenSuite, SideEffects, Validator};
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
pub(super) const ATTEMPT_FILE: &str = "attempt.json";

/// Durable reservation of one evaluation attempt, written before the first run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AttemptRecord {
    pub(super) schema_version: u32,
    pub(super) evaluation_id: String,
    pub(super) owner_scope: OwnerScope,
    pub(super) skill_id: SkillId,
    pub(super) suite_digest: String,
    pub(super) split: EvalSplit,
    pub(super) split_digest: String,
    /// What the attempt evaluates, so a reservation that never produced a
    /// receipt can still be attributed to its subject. Absent in reservations
    /// written before SL07; those cannot be attributed without a receipt.
    #[serde(default)]
    pub(super) candidate_revision: Option<RevisionId>,
    #[serde(default)]
    pub(super) baseline_revision: Option<RevisionId>,
    #[serde(default)]
    pub(super) observed_generation: Option<u64>,
    pub(super) reserved_at: DateTime<Utc>,
}

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
    #[error(
        "the evaluation history holds {limit} entries; archive or remove finished evaluations of this store before starting another"
    )]
    HistoryFull { limit: usize },
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
    /// Bundle-relative path the runtime served.
    pub resource_path: String,
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
    /// Measured usage of the run from its own cost ledger; `None` when the
    /// ledger could not be read (the run is then invalid, never free).
    pub tokens: Option<u64>,
    pub cost_micros: Option<u64>,
    /// Models that actually served the run, from the ledger.
    pub models_used: Vec<String>,
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
    pub response_blank: bool,
    pub tokens: Option<u64>,
    pub cost_micros: Option<u64>,
    pub models_used: Vec<String>,
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
            response_blank: observation.response.trim().is_empty(),
            tokens: observation.tokens,
            cost_micros: observation.cost_micros,
            models_used: observation.models_used.clone(),
        }
    }

    /// Receipt of a run that produced no observation (the executor failed).
    fn unobserved(run_id: &str, elapsed_ms: u64) -> Self {
        Self {
            run_id: run_id.to_string(),
            terminal: RunTerminal::Failed,
            response_sha256: sha256_hex(b""),
            response_bytes: 0,
            elapsed_ms,
            environment_hash: String::new(),
            installed: None,
            hydrations: Vec::new(),
            experience_terminal: None,
            artifacts: BTreeMap::new(),
            cleanup: CleanupState::Removed,
            response_blank: true,
            tokens: None,
            cost_micros: None,
            models_used: Vec::new(),
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
    UsageUnmeasured,
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
    pub total_tokens: u64,
    pub total_cost_micros: u64,
    pub failure_categories: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReceipt {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<super::experiment::ExperimentRunBinding>,
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
    /// How the run's randomness was controlled ("unsupported" when it cannot be).
    pub seed_control: String,
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
    /// The only tools offered besides `skill_read`.
    pub required_tools: &'a [String],
    pub side_effects: SideEffects,
    /// Paths the task must not be able to reach (the lifecycle store, which
    /// holds the oracle).
    pub denied_paths: &'a [std::path::PathBuf],
}

/// Runs one case in a fresh owned workspace through the real agent path.
#[async_trait::async_trait]
pub trait CaseExecutor: Send + Sync {
    /// Stable description of the executor stamped on the evaluation receipt.
    fn identity(&self) -> String;

    /// How randomness is controlled for the runs (recorded on the receipt).
    fn seed_control(&self) -> String {
        "unsupported: the provider path exposes no seed".to_string()
    }

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

/// Digest of each file of an arm's bundle by path: what a hydration receipt
/// must match.
pub(super) type ResourceDigests = BTreeMap<String, String>;

pub(super) fn resource_digests(bundle: &Bundle) -> ResourceDigests {
    bundle
        .files()
        .iter()
        .map(|file| (file.path().to_string(), sha256_hex(file.bytes())))
        .collect()
}

/// Both arms' resource digests.
pub(super) struct ArmResources {
    pub baseline: Option<ResourceDigests>,
    pub candidate: ResourceDigests,
}

/// What decides a case. It is built from stored data only (the run receipt,
/// the recorded validator outcomes and the suite case), so the live path and
/// the verification path derive a verdict from exactly the same facts.
pub(super) struct DeriveInput<'a> {
    pub case: &'a EvalCase,
    pub run: &'a RunReceipt,
    pub validators: &'a [ValidatorOutcome],
    pub skill_id: &'a SkillId,
    pub arm: Option<&'a ArmBinding>,
    pub resources: Option<&'a ResourceDigests>,
    pub expected_run_id: Option<&'a str>,
    pub evaluation_env: Option<&'a str>,
}

fn unbound(detail: &str) -> CaseVerdict {
    CaseVerdict::invalid(InvalidReason::Unbound, detail)
}

fn check_binding(input: &DeriveInput<'_>) -> Result<(), CaseVerdict> {
    let run = input.run;
    if input.expected_run_id.is_some_and(|id| run.run_id != id) {
        return Err(unbound("the run receipt names a different run id than the one issued"));
    }
    match (input.arm, &run.installed) {
        (None, None) => {
            if run.hydrations.is_empty() {
                Ok(())
            } else {
                Err(unbound("a skill was read in an arm that has no skill"))
            }
        }
        (None, Some(_)) => Err(unbound("a skill was installed in an arm that has no skill")),
        (Some(_), None) => Err(unbound("the arm's skill revision was not installed")),
        (Some(binding), Some(installed)) => {
            if installed.skill_id != *input.skill_id
                || installed.revision_id != binding.revision_id
                || installed.bundle_manifest_hash != binding.bundle_manifest_hash
            {
                return Err(unbound("the installed skill is not the arm's revision"));
            }
            for hydration in &run.hydrations {
                if hydration.skill_id != *input.skill_id
                    || hydration.revision_id != binding.revision_id
                    || hydration.generation != installed.generation
                {
                    return Err(unbound("a skill read does not belong to the arm's revision"));
                }
                let matches_bundle = input
                    .resources
                    .and_then(|resources| resources.get(&hydration.resource_path))
                    .is_some_and(|digest| *digest == hydration.resource_sha256);
                if !matches_bundle {
                    return Err(unbound("the bytes the runtime served are not the arm's bundle content"));
                }
            }
            Ok(())
        }
    }
}

/// Checks the recorded validator outcomes against the suite case and, where the
/// run receipt carries enough (response and artifact digests, absence), against
/// the run itself. Substring validators cannot be rechecked from digests; their
/// recorded outcome stands and relies on the seal and the store's permissions.
fn effective_validators(input: &DeriveInput<'_>) -> Result<Vec<bool>, CaseVerdict> {
    if input.validators.len() != input.case.validators.len() {
        return Err(unbound("the recorded validators do not match the suite case"));
    }
    let mut passed = Vec::with_capacity(input.validators.len());
    for (validator, outcome) in input.case.validators.iter().zip(input.validators) {
        if outcome.kind != validator.kind() {
            return Err(unbound("the recorded validators do not match the suite case"));
        }
        let artifact = validator.artifact_path().map(|path| input.run.artifacts.get(path));
        passed.push(match validator {
            Validator::ResponseSha256 { sha256 } => input.run.response_sha256 == *sha256,
            Validator::ArtifactSha256 { sha256, .. } => matches!(
                artifact,
                Some(Some(Some(digest))) if digest.sha256 == *sha256
            ),
            Validator::ArtifactAbsent { .. } => matches!(artifact, Some(Some(None))),
            _ => outcome.passed,
        });
    }
    Ok(passed)
}

/// Derives the verdict of one run. Order matters: unusable evidence is
/// reported as invalid before any validator can make it look like a pass.
pub(super) fn derive_verdict(input: &DeriveInput<'_>) -> CaseVerdict {
    if let Err(verdict) = check_binding(input) {
        return verdict;
    }
    let run = input.run;
    let invalid = |reason, detail: &str| CaseVerdict::invalid(reason, detail);
    match run.terminal {
        RunTerminal::Completed => {}
        RunTerminal::Failed => return invalid(InvalidReason::Partial, "the run did not reach a completed terminal"),
        RunTerminal::TimedOut => return invalid(InvalidReason::TimedOut, "the run exceeded its elapsed ceiling"),
        RunTerminal::Cancelled => return invalid(InvalidReason::Cancelled, "the run was cancelled"),
    }
    if run.cleanup != CleanupState::Removed {
        return invalid(InvalidReason::CleanupFailed, "the task workspace was not removed");
    }
    if run.response_bytes == 0 || run.response_blank {
        return invalid(InvalidReason::ZeroBody, "the run returned no response body");
    }
    if input.evaluation_env.is_some_and(|env| env != run.environment_hash) {
        return invalid(
            InvalidReason::EnvironmentMismatch,
            "the run environment differs from the evaluation's",
        );
    }
    if run.tokens.is_none() || run.cost_micros.is_none() {
        return invalid(
            InvalidReason::UsageUnmeasured,
            "the run's tokens and cost were not measured",
        );
    }
    let mut reasons: Vec<String> = Vec::new();
    if !run.hydrations.is_empty() {
        match &run.experience_terminal {
            None => {
                return invalid(
                    InvalidReason::MissingTerminal,
                    "the skill was read but no terminal was joined to the task",
                );
            }
            Some(terminal) => match terminal.outcome {
                ExperienceOutcome::Success => {}
                ExperienceOutcome::Failure => reasons.push("terminal_failure".to_string()),
                ExperienceOutcome::Blocked => reasons.push("terminal_blocked".to_string()),
                ExperienceOutcome::Cancelled => return invalid(InvalidReason::Cancelled, "the task was cancelled"),
                ExperienceOutcome::Incomplete => {
                    return invalid(InvalidReason::MissingTerminal, "the task terminal is incomplete");
                }
                ExperienceOutcome::Unknown => {
                    return invalid(
                        InvalidReason::MissingTerminal,
                        "the task terminal outcome is unverified",
                    );
                }
            },
        }
    }
    let passed = match effective_validators(input) {
        Ok(passed) => passed,
        Err(verdict) => return verdict,
    };
    for (validator, ok) in input.case.validators.iter().zip(passed) {
        if !ok {
            reasons.push(format!("validator:{}", validator.kind()));
        }
    }
    if input.arm.is_some() {
        let read = !run.hydrations.is_empty();
        if read != input.case.expect_skill_read {
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
        CaseVerdict::Pass
    } else {
        CaseVerdict::Fail { reasons }
    }
}

/// What every receipt of one evaluation shares.
#[derive(Clone, Copy)]
pub(super) struct CaseContext<'a> {
    pub owner: &'a OwnerScope,
    pub evaluation_id: &'a str,
    pub suite: &'a FrozenSuite,
    pub observed_generation: u64,
    pub resources: &'a ArmResources,
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
    unobserved_elapsed_ms: u64,
    forced: Option<CaseVerdict>,
) -> Result<CaseReceipt, EvaluationError> {
    let CaseContext {
        owner,
        evaluation_id,
        suite,
        observed_generation,
        ..
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
                RunReceipt::unobserved(run_id, unobserved_elapsed_ms),
            )
        },
        |observation| {
            let run = RunReceipt::from_observation(observation);
            let validators = run_validators(case, &observation.response, &observation.artifacts);
            let resources = match arm {
                Arm::Baseline => context.resources.baseline.as_ref(),
                Arm::Candidate => Some(&context.resources.candidate),
            };
            let verdict = derive_verdict(&DeriveInput {
                case,
                run: &run,
                validators: &validators,
                skill_id: &skill_id,
                arm: arm_binding,
                resources,
                expected_run_id: Some(run_id),
                evaluation_env: None,
            });
            (verdict, validators, run)
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
    match seal_digest(&case.run) {
        Ok(digest) if digest == case.run_receipt_digest => None,
        _ => Some("the run receipt digest does not match".to_string()),
    }
}

const fn verdict_class(verdict: &CaseVerdict) -> u8 {
    match verdict {
        CaseVerdict::Pass => 0,
        CaseVerdict::Fail { .. } => 1,
        CaseVerdict::Invalid { .. } => 2,
    }
}

/// The run sequence the evaluator plans: cases in canonical order, trials,
/// and the arm that goes first alternating.
fn planned_sequence(suite: &FrozenSuite, split: EvalSplit) -> Vec<CaseKey> {
    let mut sequence = Vec::new();
    for (case_index, case) in suite.cases_of(split).enumerate() {
        for trial in 0..suite.suite().trials {
            let arms = if (case_index + usize::try_from(trial).unwrap_or(0)) % 2 == 0 {
                [Arm::Baseline, Arm::Candidate]
            } else {
                [Arm::Candidate, Arm::Baseline]
            };
            for arm in arms {
                sequence.push((case.case_id.clone(), trial, arm));
            }
        }
    }
    sequence
}

/// Whether the evaluation-level fields agree with the suite and the case
/// receipts. Anything a store writer could adjust to move the outcome has to
/// agree with data it cannot move consistently.
fn receipt_defect(
    suite: &FrozenSuite,
    receipt: &EvaluationReceipt,
    cases: &[CaseReceipt],
    planned: usize,
) -> Option<String> {
    let spec = suite.suite();
    let split_ids: Vec<String> = suite.cases_of(receipt.split).map(|case| case.case_id.clone()).collect();
    if receipt.trials != spec.trials || receipt.planned_cases != split_ids {
        return Some("the evaluation's trials or planned cases differ from the suite".to_string());
    }
    if usize::try_from(receipt.planned_runs).ok() != Some(planned)
        || usize::try_from(receipt.recorded_runs).ok() != Some(cases.len())
    {
        return Some("the evaluation's run counts differ from its receipts".to_string());
    }
    if receipt.total_elapsed_ms != cases.iter().map(|case| case.run.elapsed_ms).sum::<u64>() {
        return Some("the evaluation's elapsed total differs from its receipts".to_string());
    }
    if receipt.stale != (receipt.end_generation != receipt.observed_generation) {
        return Some("the stale flag disagrees with the recorded generations".to_string());
    }
    None
}

/// Recomputes the outcome and summary from the stored receipts. Verdicts are
/// never taken from storage: each is re-derived from the run receipt and the
/// suite case, and a stored verdict that disagrees makes the evaluation
/// incomplete.
pub(super) fn assess(
    suite: &FrozenSuite,
    receipt: &EvaluationReceipt,
    cases: &[CaseReceipt],
    resources: &ArmResources,
) -> (EvaluationOutcome, EvaluationSummary) {
    let spec = suite.suite();
    // A run whose usage was not measured is charged its share of the ceiling,
    // never nothing.
    let token_charge = spec.budget.unmeasured_tokens();
    let cost_charge = spec.budget.unmeasured_cost_micros();
    let mut summary = EvaluationSummary {
        trials: spec.trials,
        total_elapsed_ms: cases.iter().map(|case| case.run.elapsed_ms).sum(),
        total_tokens: cases.iter().fold(0_u64, |sum, case| {
            sum.saturating_add(case.run.tokens.unwrap_or(token_charge))
        }),
        total_cost_micros: cases.iter().fold(0_u64, |sum, case| {
            sum.saturating_add(case.run.cost_micros.unwrap_or(cost_charge))
        }),
        ..EvaluationSummary::default()
    };
    let split_cases: Vec<&EvalCase> = suite.cases_of(receipt.split).collect();
    summary.cases = u32::try_from(split_cases.len()).unwrap_or(u32::MAX);
    let sequence = planned_sequence(suite, receipt.split);
    let expected: BTreeSet<&CaseKey> = sequence.iter().collect();
    let mut defect: Option<IncompleteReason> =
        receipt_defect(suite, receipt, cases, sequence.len()).map(|detail| IncompleteReason::Unbound {
            case_id: "<evaluation>".to_string(),
            detail,
        });
    let mut ordered: Vec<&CaseReceipt> = cases.iter().collect();
    ordered.sort_by_key(|case| case.order);
    // The evaluation's environment is the first one observed; observed runs
    // without one are a defect, so the environment check cannot be skipped.
    let first_environment = ordered
        .iter()
        .map(|case| case.run.environment_hash.as_str())
        .find(|hash| !hash.is_empty());
    if first_environment != receipt.environment_hash.as_deref() {
        defect.get_or_insert_with(|| IncompleteReason::Unbound {
            case_id: "<evaluation>".to_string(),
            detail: "the evaluation's environment binding differs from its observed runs".to_string(),
        });
    }
    let mut recorded: BTreeMap<CaseKey, (&CaseReceipt, CaseVerdict)> = BTreeMap::new();
    let mut run_ids: BTreeSet<&str> = BTreeSet::new();
    for case in ordered {
        let key = (case.case_id.clone(), case.trial, case.arm);
        if !expected.contains(&key) {
            defect.get_or_insert_with(|| IncompleteReason::UnexpectedCase {
                case_id: case.case_id.clone(),
            });
            continue;
        }
        if sequence.get(usize::try_from(case.order).unwrap_or(usize::MAX)) != Some(&key) {
            defect.get_or_insert_with(|| IncompleteReason::Unbound {
                case_id: case.case_id.clone(),
                detail: "the run order differs from the counterbalanced plan".to_string(),
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
        let Some(suite_case) = suite.case(&case.case_id) else {
            continue;
        };
        let arm_binding = match case.arm {
            Arm::Baseline => receipt.baseline.as_ref(),
            Arm::Candidate => Some(&receipt.candidate),
        };
        let arm_resources = match case.arm {
            Arm::Baseline => resources.baseline.as_ref(),
            Arm::Candidate => Some(&resources.candidate),
        };
        let derived = if run_ids.insert(case.run.run_id.as_str()) {
            derive_verdict(&DeriveInput {
                case: suite_case,
                run: &case.run,
                validators: &case.validators,
                skill_id: &receipt.skill_id,
                arm: arm_binding,
                resources: arm_resources,
                expected_run_id: None,
                evaluation_env: receipt.environment_hash.as_deref(),
            })
        } else {
            CaseVerdict::invalid(
                InvalidReason::DuplicateRun,
                "the run id was already used by another run of this evaluation",
            )
        };
        if verdict_class(&derived) != verdict_class(&case.verdict) {
            defect.get_or_insert_with(|| IncompleteReason::Unbound {
                case_id: case.case_id.clone(),
                detail: "the stored verdict differs from the one re-derived from the run receipt".to_string(),
            });
        }
        if recorded.insert(key, (case, derived)).is_some() {
            defect.get_or_insert_with(|| IncompleteReason::DuplicateReceipt {
                case_id: case.case_id.clone(),
            });
        }
    }
    for (_, verdict) in recorded.values() {
        match verdict {
            CaseVerdict::Fail { reasons } => {
                for reason in reasons {
                    *summary.failure_categories.entry(reason.clone()).or_insert(0) += 1;
                }
            }
            CaseVerdict::Invalid { .. } => summary.invalid_runs += 1,
            CaseVerdict::Pass => {}
        }
    }
    let missing = expected.iter().filter(|key| !recorded.contains_key(**key)).count();
    // A case passes in an arm only when every trial of it passed.
    let arm_passes = |case_id: &str, arm: Arm| {
        (0..spec.trials).all(|trial| {
            recorded
                .get(&(case_id.to_string(), trial, arm))
                .is_some_and(|(_, verdict)| verdict.is_pass())
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
    let over_budget = summary.total_elapsed_ms > spec.budget.max_total_elapsed_ms
        || summary.total_tokens > spec.budget.max_total_tokens
        || summary.total_cost_micros > spec.budget.max_total_cost_micros;
    let budget_claim_holds = summary.total_elapsed_ms >= spec.budget.max_total_elapsed_ms
        || summary.total_tokens >= spec.budget.max_total_tokens
        || summary.total_cost_micros >= spec.budget.max_total_cost_micros;
    if receipt.budget_exhausted && !budget_claim_holds {
        defect.get_or_insert_with(|| IncompleteReason::Unbound {
            case_id: "<evaluation>".to_string(),
            detail: "the evaluation claims an exhausted budget its receipts do not show".to_string(),
        });
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
    } else if missing > 0 && !receipt.budget_exhausted {
        EvaluationOutcome::Incomplete {
            reason: IncompleteReason::MissingCases {
                count: u32::try_from(missing).unwrap_or(u32::MAX),
            },
        }
    } else if receipt.budget_exhausted || over_budget {
        EvaluationOutcome::BudgetExceeded
    } else if summary.invalid_runs > 0 {
        EvaluationOutcome::Incomplete {
            reason: IncompleteReason::InvalidRuns {
                count: summary.invalid_runs,
            },
        }
    } else if summary.cases < spec.min_cases {
        EvaluationOutcome::Inconclusive {
            cases: summary.cases,
            required: spec.min_cases,
        }
    } else if summary.critical_regressions > 0 || summary.regressed > spec.allowed_regressions {
        EvaluationOutcome::Regressed
    } else if summary.critical_failures > 0
        || summary.candidate_passed < summary.baseline_passed.saturating_add(spec.min_gain_cases)
    {
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
        // The directory was created by the attempt reservation.
        let dir = self.evaluation_dir(&receipt.record_id);
        check_private(&dir)?;
        if fs::symlink_metadata(dir.join(RECEIPT_FILE)).is_ok() {
            return Err(tampered(&dir, "an evaluation is immutable once its receipt is written"));
        }
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

    pub(super) fn read_sealed_receipt(&self, id: &str) -> Result<(EvaluationReceipt, String), EvaluationError> {
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
                return Err(EvaluationError::HistoryFull {
                    limit: MAX_EVALUATION_ENTRIES,
                });
            }
            ids.push(name);
        }
        ids.sort();
        Ok(ids)
    }

    /// The reservation of an evaluation, `None` when it is missing or cannot be
    /// read (the caller then has to rely on the receipt).
    pub(super) fn read_attempt(&self, id: &str) -> Option<AttemptRecord> {
        let path = self.evaluation_dir(id).join(ATTEMPT_FILE);
        check_private(&path).ok()?;
        read_record::<AttemptRecord>(&path).ok()
    }

    /// Reserves one attempt on `split` for a new evaluation before any run
    /// starts. The count, the limit check and the durable record happen under
    /// the store and skill locks, so concurrent evaluations cannot both take
    /// the last attempt, and a crash, drop or panic after this point cannot give
    /// the attempt back: a reserved attempt without a receipt is a spent one.
    /// The split is keyed by the digest of its own cases, so renaming the suite
    /// or editing other splits does not reopen a reserved split.
    pub(super) fn reserve_attempt(
        &self,
        suite: &FrozenSuite,
        split: EvalSplit,
        evaluation_id: &str,
        candidate: &RevisionId,
        baseline: Option<&RevisionId>,
        observed_generation: u64,
    ) -> Result<(), EvaluationError> {
        let skill = &suite.suite().skill_id;
        let split_digest = suite.split_digest(split);
        let _store = self.lock_store(true, super::publish::PUBLICATION_LOCK_TIMEOUT)?;
        let _skill = self.lock_skill(skill, super::publish::PUBLICATION_LOCK_TIMEOUT)?;
        if let Some(limit) = suite.attempt_limit(split) {
            let mut used = 0_u32;
            for id in self.list_evaluation_ids()? {
                let path = self.evaluation_dir(&id).join(ATTEMPT_FILE);
                let counts = match fs::symlink_metadata(&path) {
                    Ok(_) => match check_private(&path).and_then(|()| read_record::<AttemptRecord>(&path)) {
                        Ok(record) => record.split == split && record.split_digest == split_digest,
                        // Unreadable history still counts: it must not reopen a reserved split.
                        Err(_) => true,
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(_) => true,
                };
                used += u32::from(counts);
            }
            if used >= limit {
                return Err(EvaluationError::AttemptsExhausted { split, limit, used });
            }
        }
        let parent = self.root().join(EVALUATIONS_DIR);
        super::generation::ensure_dir_synced(self.root(), &parent)?;
        let dir = self.evaluation_dir(evaluation_id);
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir).map_err(|e| LifecycleError::io(&dir, e))?;
        let record = AttemptRecord {
            schema_version: EVALUATION_SCHEMA_VERSION,
            evaluation_id: evaluation_id.to_string(),
            owner_scope: self.owner_scope().clone(),
            skill_id: skill.clone(),
            suite_digest: suite.digest().to_string(),
            split,
            split_digest,
            candidate_revision: Some(candidate.clone()),
            baseline_revision: baseline.cloned(),
            observed_generation: Some(observed_generation),
            reserved_at: Utc::now(),
        };
        let path = dir.join(ATTEMPT_FILE);
        self.write_atomic(skill, &path, &record_bytes(&record, &path)?)?;
        super::store::sync_dir(&parent)?;
        Ok(())
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
        // The arms must still resolve to exactly the recorded bundles, whose
        // file digests the hydration receipts are checked against.
        let mut arm_digests: Vec<ResourceDigests> = Vec::new();
        for arm in receipt.baseline.iter().chain(std::iter::once(&receipt.candidate)) {
            let revision = self.read_revision(&receipt.skill_id, &arm.revision_id, None)?;
            if revision.revision.bundle_manifest_hash != arm.bundle_manifest_hash {
                return Err(tampered(
                    &dir,
                    "a revision no longer matches the bundle hash the evaluation recorded",
                ));
            }
            arm_digests.push(resource_digests(&revision.bundle));
        }
        let candidate_digests = arm_digests.pop().unwrap_or_default();
        let resources = ArmResources {
            baseline: arm_digests.pop(),
            candidate: candidate_digests,
        };
        let (outcome, summary) = assess(&suite, &receipt, &cases, &resources);
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

/// Test support: rewrites an evaluation's stored receipts the way a writer with
/// access to the store could, re-sealing every digest (the seal is a plain
/// SHA-256, so this is possible) and the case file digest.
#[cfg(test)]
pub(super) fn rewrite_evaluation_for_test(
    store: &SkillLifecycleStore,
    id: &str,
    mutate: impl FnOnce(&mut EvaluationReceipt, &mut Vec<CaseReceipt>),
) {
    let (mut receipt, _) = store.read_sealed_receipt(id).expect("test: receipt");
    let dir = store.evaluation_dir(id);
    let bytes = fs::read(dir.join(CASES_FILE)).expect("test: cases");
    let mut cases: Vec<CaseReceipt> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice::<Sealed<CaseReceipt>>(line)
                .expect("test: case")
                .receipt
        })
        .collect();
    mutate(&mut receipt, &mut cases);
    for case in &mut cases {
        case.run_receipt_digest = seal_digest(&case.run).expect("test: seal run");
    }
    let mut lines = Vec::new();
    for case in &cases {
        let sealed = Sealed {
            digest: seal_digest(case).expect("test: seal"),
            receipt: case,
        };
        serde_json::to_writer(&mut lines, &sealed).expect("test: json");
        lines.push(b'\n');
    }
    fs::write(dir.join(CASES_FILE), &lines).expect("test: write cases");
    receipt.cases_digest = sha256_hex(&lines);
    let sealed = Sealed {
        digest: seal_digest(&receipt).expect("test: seal receipt"),
        receipt: &receipt,
    };
    fs::write(
        dir.join(RECEIPT_FILE),
        serde_json::to_vec_pretty(&sealed).expect("test: json"),
    )
    .expect("test: write receipt");
}
