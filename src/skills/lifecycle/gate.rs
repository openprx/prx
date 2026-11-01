//! The deterministic skill gate (SL07).
//!
//! [`SkillLifecycleStore::gate_proposal`] decides whether an evaluated
//! proposal may go on toward publication. The decision rests on verified
//! evaluation receipts and nothing else:
//!
//! * every evaluation is re-read through `verify_evaluation` with the receipt
//!   digest the caller pinned (never `None`), which recomputes verdicts and the
//!   outcome from the case receipts; the gate then recomputes the metrics it
//!   decides on from the verified summary and from the frozen suite, and never
//!   uses a stored score, a stored outcome or a caller-supplied `passed`;
//! * a receipt that is missing, tampered with, resealed without the pin, bound
//!   to another skill, candidate, baseline or suite, or stale (the skill moved,
//!   or the suite changed) is refused with an error and decides nothing;
//! * a validation evaluation is required, and a holdout evaluation too when the
//!   suite declares holdout cases. Development evaluations never gate;
//! * `Accept` needs every evaluation to show a strict gain of at least the
//!   suite's prespecified minimum, no critical regression or failure, no more
//!   regressions than `min(suite allowance, policy bound)`, a complete and
//!   valid sample, and budgets respected. Anything else is `Reject`.
//!
//! Binding to the proposal state machine (spec section 8.1): an `Evaluated`
//! proposal becomes `Eligible` on accept, `Rejected` when the evidence measured
//! a regression, no sufficient gain, a critical failure or a judge concern (the
//! rejection is recorded through `record_rejection`, so the same material is
//! suppressed), and `Inconclusive` when the evidence is uncertain (too few
//! cases, invalid runs, a missing case, an exhausted budget, a judge that did
//! not answer). Uncertain evidence is a reject for the publication decision but
//! is deliberately not entered in the rejection history: the spec lets an
//! inconclusive proposal be evaluated again, and suppressing its material
//! because a provider failed would punish the change for the infrastructure.
//!
//! The decision is stored as a sealed gate receipt under `gates/`. SL08's
//! publication authorization verifies it with [`SkillLifecycleStore::verify_gate`]
//! (pinned digest, caller-owned policy, current suite), which re-verifies every
//! evaluation and recomputes the decision. Nothing here authorizes or commits a
//! publication; the receipt only carries what that step must be able to check.
//!
//! An optional [`GateJudge`] can add a reason to reject and nothing else.

use super::consolidate::reserved_lock;
use super::evaluate::{EvaluationError, EvaluationOutcome, EvaluationSummary, IncompleteReason, VerifiedEvaluation};
use super::gate_judge::{GateJudge, JudgeBrief, JudgeError, JudgeOpinion, JudgedEvaluation, sanitize_reason};
use super::generation::ensure_dir_synced;
use super::proposal::{ProposalStatus, SkillProposal, bounded};
use super::proposer_model::{ReportedUsage, TallyCounters};
use super::rejection::{NewRejection, ProposalOperation, RejectionRecord};
use super::store::{SkillLifecycleStore, check_private, read_record, record_bytes};
use super::suite::{EvalSplit, EvalSuite, MAX_ALLOWED_REGRESSIONS};
use super::types::{LifecycleError, OwnerScope, RevisionId, SCHEMA_VERSION, SkillId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

mod experiment;
pub use experiment::GateExperimentBinding;
pub(crate) use experiment::GateReviewSubject;

/// Identity stamped on every gate receipt as the reviewer.
pub const GATE_IDENTITY: &str = "prx-skill-gate/1";
const GATES_DIR: &str = "gates";
const GATE_LOCK_ID: &str = "_skill-gates";
const GATE_ID_PREFIX: &str = "gate-";
const GATE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const GATE_SEAL_DOMAIN: &[u8] = b"prx-skill-gate-receipt\0";
const MIN_VALID_FOR_SECS: u64 = 60;
const MAX_VALID_FOR_SECS: u64 = 7 * 24 * 3600;
const MAX_JUDGE_TIMEOUT_MS: u64 = 600_000;
/// Evaluation epochs one proposal may use (spec 8.1: a recorded new attempt per
/// retry). It bounds retries after an uncertain outcome, an exhausted budget
/// included.
pub const MAX_GATE_EPOCHS: u32 = 3;
const MAX_JUDGE_IDENTITY_BYTES: usize = 256;

/// Failure to gate a proposal or to verify a gate receipt. Every variant is a
/// refusal; none of them can be read as an accept.
#[derive(Debug, Error)]
pub enum GateError {
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error(transparent)]
    Evaluation(#[from] EvaluationError),
    #[error("invalid gate policy: {0}")]
    Policy(String),
    #[error("proposal {proposal_id} is {status}; only an evaluated proposal can be gated")]
    NotGateable { proposal_id: String, status: &'static str },
    #[error("the gate does not handle this proposal: {0}")]
    Unsupported(String),
    #[error("evaluation is not usable for this proposal: {0}")]
    Unbound(String),
    #[error("the gate needs an evaluation of the {split:?} split")]
    MissingEvaluation { split: EvalSplit },
    #[error("the governing suite is below the policy's floor: {0}")]
    SuiteBelowFloor(String),
    #[error("new or changed evidence appeared after the decision: {0}")]
    NewFailingEvidence(String),
    #[error("not an accepting gate decision: {0}")]
    NotAccepted(String),
    #[error("evaluation is stale: {0}")]
    Stale(String),
    #[error("evaluation was made with a suite that is no longer the current one: {0}")]
    SuiteChanged(String),
    #[error("gate receipt expired at {0}")]
    Expired(DateTime<Utc>),
    #[error("historical gate has insufficient temporal evidence: {0}")]
    InsufficientTemporalEvidence(String),
    #[error("gate receipt is invalid: {0}")]
    Tampered(String),
    #[error("gating was cancelled before a decision was made")]
    Cancelled,
    #[error("proposal {proposal_id} is {status}; publication needs a proposal in the right authorization state")]
    NotAuthorized { proposal_id: String, status: &'static str },
    #[error(
        "publication transaction {transaction_id} of proposal {proposal_id} aborted; the next attempt uses a new one"
    )]
    PublicationAborted {
        proposal_id: String,
        transaction_id: String,
    },
}

/// The rules a gate decision is made under. There is no default: absent policy
/// is a configuration error, not a permissive gate. The caller owns the policy
/// and passes the same one to [`SkillLifecycleStore::verify_gate`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatePolicy {
    pub version: u32,
    /// Most non-critical regressions tolerated next to a gain. The suite's own
    /// allowance applies too; the smaller bound wins.
    pub max_regressions: u32,
    /// How long an accept stays usable by publication.
    pub valid_for_secs: u64,
    /// A missing judge makes an otherwise passing proposal non-passing.
    pub judge_required: bool,
    /// Deadline of one judge call.
    pub judge_timeout_ms: u64,
    /// A governing suite without a holdout split is refused when set.
    pub require_holdout: bool,
    /// Smallest `min_cases` a governing suite may declare.
    pub min_cases_floor: u32,
    /// When set, only this judge identity may answer, and a receipt that names
    /// another one does not verify.
    pub judge_identity: Option<String>,
}

impl GatePolicy {
    pub fn validate(&self) -> Result<(), GateError> {
        let refuse = |reason: &str| Err(GateError::Policy(reason.to_string()));
        if self.version == 0 {
            return refuse("version must be at least 1");
        }
        if self
            .judge_identity
            .as_ref()
            .is_some_and(|identity| identity.trim().is_empty() || identity.len() > MAX_JUDGE_IDENTITY_BYTES)
        {
            return refuse("judge_identity must be 1..=256 bytes");
        }
        if self.max_regressions > MAX_ALLOWED_REGRESSIONS {
            return refuse("max_regressions exceeds the suite maximum of 3");
        }
        if !(MIN_VALID_FOR_SECS..=MAX_VALID_FOR_SECS).contains(&self.valid_for_secs) {
            return refuse("valid_for_secs must be 60..=604800");
        }
        if self.judge_timeout_ms == 0 || self.judge_timeout_ms > MAX_JUDGE_TIMEOUT_MS {
            return refuse("judge_timeout_ms must be 1..=600000");
        }
        Ok(())
    }
}

/// An evaluation the caller relies on, pinned by its sealed receipt digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvaluationPin {
    pub evaluation_id: String,
    pub receipt_digest: String,
}

/// What to gate.
#[derive(Clone, Copy, Debug)]
pub struct GateRequest<'a> {
    pub proposal_id: &'a str,
    /// Confirmation field, not an input: the governing suite is bound to the
    /// proposal by `begin_evaluation`, and this argument only has to equal it
    /// (it catches a caller that believes another suite governs).
    pub suite_digest: &'a str,
    pub evaluations: &'a [EvaluationPin],
    pub policy: &'a GatePolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    Accept,
    Reject,
}

/// One structured reason a proposal did not pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateReason {
    Regression {
        split: EvalSplit,
        regressed: u32,
        critical: u32,
        allowed: u32,
    },
    InsufficientGain {
        split: EvalSplit,
        gain: u32,
        required: u32,
    },
    CriticalFailure {
        split: EvalSplit,
        count: u32,
    },
    TooFewCases {
        split: EvalSplit,
        cases: u32,
        required: u32,
    },
    InvalidRuns {
        split: EvalSplit,
        count: u32,
    },
    BudgetExhausted {
        split: EvalSplit,
    },
    Incomplete {
        split: EvalSplit,
        detail: String,
    },
    /// Another recorded evaluation of the same subject does not pass; a pass
    /// cannot be picked out of several runs.
    UnpinnedFailure {
        split: EvalSplit,
        evaluation_id: String,
    },
    /// Another recorded evaluation of the same subject has no receipt (still
    /// running, or abandoned) or did not reach a verdict. It blocks an accept
    /// but is not a measured failure, so it suppresses nothing.
    UnpinnedPending {
        split: EvalSplit,
        evaluation_id: String,
    },
    JudgeConcern {
        reason: String,
    },
    JudgeUnavailable {
        detail: String,
    },
    JudgeRequired,
}

impl GateReason {
    /// Whether the reason comes from another recorded evaluation rather than
    /// from the pinned ones.
    #[must_use]
    pub const fn is_sibling(&self) -> bool {
        matches!(self, Self::UnpinnedFailure { .. } | Self::UnpinnedPending { .. })
    }

    /// Whether the reason is a measured defect of the change (as opposed to
    /// evidence too thin or too broken to measure anything).
    #[must_use]
    pub const fn is_measured(&self) -> bool {
        matches!(
            self,
            Self::Regression { .. }
                | Self::InsufficientGain { .. }
                | Self::CriticalFailure { .. }
                | Self::UnpinnedFailure { .. }
                | Self::JudgeConcern { .. }
        )
    }

    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Regression {
                split,
                regressed,
                critical,
                allowed,
            } => format!("regression on {split:?}: {regressed} regressed ({critical} critical), {allowed} allowed"),
            Self::InsufficientGain { split, gain, required } => {
                format!("insufficient gain on {split:?}: {gain} net cases, {required} required")
            }
            Self::CriticalFailure { split, count } => {
                format!("{count} critical case(s) fail with the candidate on {split:?}")
            }
            Self::TooFewCases { split, cases, required } => {
                format!("too few cases on {split:?}: {cases}, {required} required")
            }
            Self::InvalidRuns { split, count } => format!("{count} invalid run(s) on {split:?}"),
            Self::BudgetExhausted { split } => format!("evaluation budget exhausted on {split:?}"),
            Self::Incomplete { split, detail } => format!("incomplete evaluation on {split:?}: {detail}"),
            Self::UnpinnedFailure { split, evaluation_id } => {
                format!("another recorded {split:?} evaluation does not pass: {evaluation_id}")
            }
            Self::UnpinnedPending { split, evaluation_id } => {
                format!("another recorded {split:?} evaluation has no verdict yet: {evaluation_id}")
            }
            Self::JudgeConcern { reason } => format!("judge concern: {reason}"),
            Self::JudgeUnavailable { detail } => format!("judge did not give a usable answer: {detail}"),
            Self::JudgeRequired => "the policy requires a judge and none gave an answer".to_string(),
        }
    }
}

/// One evaluation as the gate verified and recomputed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateEvaluationEntry {
    pub evaluation_id: String,
    pub split: EvalSplit,
    pub receipt_digest: String,
    /// Recomputed from the case receipts by `verify_evaluation`.
    pub outcome: EvaluationOutcome,
    /// Recomputed from the case receipts; never read from storage.
    pub summary: EvaluationSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum JudgeResult {
    Approved,
    Concern { reason: String },
    Unavailable { detail: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeRecord {
    pub identity: String,
    pub result: JudgeResult,
    /// Physical requests the judge call made and settled.
    pub usage: ReportedUsage,
}

/// The gate decision (spec 6.4 `SkillGateDecision`): decision, structured
/// reasons, the complete receipt hashes, policy version, review identity and
/// expiry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillGateDecision {
    pub schema_version: u32,
    pub record_id: String,
    pub owner_scope: OwnerScope,
    /// Captured only from an admitted runtime at gate creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<GateExperimentBinding>,
    pub proposal_id: String,
    /// Evaluation epoch of the proposal this decision belongs to.
    pub epoch: u32,
    pub operation: ProposalOperation,
    pub skill_id: SkillId,
    /// The revision the candidate is compared with; `None` for a new skill,
    /// whose baseline is doing the same tasks without the skill.
    pub baseline_revision: Option<RevisionId>,
    pub candidate_revision: RevisionId,
    pub expected_generation: u64,
    pub suite_digest: String,
    pub evaluations: Vec<GateEvaluationEntry>,
    pub policy: GatePolicy,
    /// Reasons found from the evaluations alone; empty means the deterministic
    /// checks passed.
    pub deterministic_reasons: Vec<GateReason>,
    pub judge: Option<JudgeRecord>,
    /// Other evaluations of the subject that had no verdict, were blocked by an
    /// earlier epoch's decision, and are not counted again here: that decision
    /// adjudicated them. If one later completes as a measured failure it counts
    /// again. Any change to the list (such a run failing, or passing) makes
    /// `verify_gate` refuse, which is conservative: a valid accept may be
    /// refused and decided again.
    #[serde(default)]
    pub adjudicated_pending: Vec<String>,
    pub decision: GateDecision,
    /// Every reason behind `decision`, deterministic and judge.
    pub reasons: Vec<GateReason>,
    pub review_identity: String,
    pub decided_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedGate {
    decision: SkillGateDecision,
    digest: String,
}

fn seal_digest(decision: &SkillGateDecision) -> Result<String, GateError> {
    let bytes = serde_json::to_vec(decision).map_err(|e| {
        GateError::Lifecycle(LifecycleError::InvalidRecord {
            path: GATES_DIR.into(),
            reason: e.to_string(),
        })
    })?;
    let mut hasher = Sha256::new();
    hasher.update(GATE_SEAL_DOMAIN);
    hasher.update(SCHEMA_VERSION.to_be_bytes());
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// A stored gate decision and the digest of its sealed receipt. A publication
/// journal pins `digest` and passes it to [`SkillLifecycleStore::verify_gate`].
#[derive(Clone, Debug)]
pub struct RecordedGate {
    pub decision: SkillGateDecision,
    pub digest: String,
}

/// A gate receipt that was re-verified end to end.
#[derive(Clone, Debug)]
pub struct VerifiedGate {
    pub decision: SkillGateDecision,
    pub digest: String,
}

impl VerifiedGate {
    /// Whether the verified decision lets the proposal go on.
    #[must_use]
    pub const fn is_accept(&self) -> bool {
        matches!(self.decision.decision, GateDecision::Accept)
    }
}

/// What gating did.
#[derive(Clone, Debug)]
pub struct GateOutcome {
    pub gate: RecordedGate,
    pub proposal: SkillProposal,
    /// The history entry written for a rejected proposal.
    pub rejection: Option<RejectionRecord>,
}

pub(super) fn gate_id_of(owner: &OwnerScope, proposal_id: &str, epoch: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"prx-skill-gate-v2\0");
    for part in [owner.as_str(), proposal_id] {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.update(epoch.to_be_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("{GATE_ID_PREFIX}{}", digest.get(..32).unwrap_or(&digest))
}

fn is_gate_id(id: &str) -> bool {
    id.strip_prefix(GATE_ID_PREFIX)
        .is_some_and(|rest| rest.len() == 32 && rest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

// ---------------------------------------------------------------------------
// The decision, recomputed from verified evaluations

/// Reasons one verified evaluation does not pass; empty when it does. Uncertain
/// evidence is reported alone: a partial sample has no trustworthy gain or
/// regression count to report.
fn evaluation_reasons(spec: &EvalSuite, policy: &GatePolicy, verified: &VerifiedEvaluation) -> Vec<GateReason> {
    let split = verified.receipt.split;
    let summary = &verified.summary;
    let receipt = &verified.receipt;
    let mut uncertain = Vec::new();
    if receipt.cancelled {
        uncertain.push(GateReason::Incomplete {
            split,
            detail: "the evaluation was cancelled".to_string(),
        });
    }
    if receipt.recorded_runs != receipt.planned_runs && !receipt.budget_exhausted {
        uncertain.push(GateReason::Incomplete {
            split,
            detail: format!(
                "{} of {} planned runs were recorded",
                receipt.recorded_runs, receipt.planned_runs
            ),
        });
    }
    if summary.invalid_runs > 0 {
        uncertain.push(GateReason::InvalidRuns {
            split,
            count: summary.invalid_runs,
        });
    }
    let over_budget = summary.total_elapsed_ms > spec.budget.max_total_elapsed_ms
        || summary.total_tokens > spec.budget.max_total_tokens
        || summary.total_cost_micros > spec.budget.max_total_cost_micros;
    if receipt.budget_exhausted || over_budget || matches!(verified.outcome, EvaluationOutcome::BudgetExceeded) {
        uncertain.push(GateReason::BudgetExhausted { split });
    }
    if summary.cases < spec.min_cases {
        uncertain.push(GateReason::TooFewCases {
            split,
            cases: summary.cases,
            required: spec.min_cases,
        });
    }
    if uncertain.is_empty()
        && let EvaluationOutcome::Incomplete { reason } = &verified.outcome
        && !matches!(reason, IncompleteReason::InvalidRuns { .. })
    {
        uncertain.push(GateReason::Incomplete {
            split,
            detail: format!("{reason:?}"),
        });
    }
    if !uncertain.is_empty() {
        return uncertain;
    }
    let mut measured = Vec::new();
    let allowed = spec.allowed_regressions.min(policy.max_regressions);
    if summary.critical_regressions > 0 || summary.regressed > allowed {
        measured.push(GateReason::Regression {
            split,
            regressed: summary.regressed,
            critical: summary.critical_regressions,
            allowed,
        });
    }
    if summary.critical_failures > 0 {
        measured.push(GateReason::CriticalFailure {
            split,
            count: summary.critical_failures,
        });
    }
    let gain = summary.candidate_passed.saturating_sub(summary.baseline_passed);
    // A tie is never a gain, whatever the suite declares.
    let required = spec.min_gain_cases.max(1);
    if gain < required {
        measured.push(GateReason::InsufficientGain { split, gain, required });
    }
    if measured.is_empty() && !verified.outcome.is_pass() {
        measured.push(GateReason::Incomplete {
            split,
            detail: format!("the evaluation outcome is {:?}", verified.outcome),
        });
    }
    measured
}

/// Combines the deterministic reasons with the judge's record. The judge can
/// only add a reason; it is not consulted for a deterministic reject, and an
/// approval changes nothing.
fn final_decision(
    deterministic: &[GateReason],
    judge: Option<&JudgeRecord>,
    policy: &GatePolicy,
) -> Result<(GateDecision, Vec<GateReason>), GateError> {
    if !deterministic.is_empty() {
        if judge.is_some() {
            return Err(GateError::Tampered(
                "a judge answered for a proposal the deterministic checks had rejected".to_string(),
            ));
        }
        return Ok((GateDecision::Reject, deterministic.to_vec()));
    }
    Ok(match judge.map(|record| &record.result) {
        None if policy.judge_required => (GateDecision::Reject, vec![GateReason::JudgeRequired]),
        None | Some(JudgeResult::Approved) => (GateDecision::Accept, Vec::new()),
        Some(JudgeResult::Concern { reason }) => (
            GateDecision::Reject,
            vec![GateReason::JudgeConcern { reason: reason.clone() }],
        ),
        Some(JudgeResult::Unavailable { detail }) => (
            GateDecision::Reject,
            vec![GateReason::JudgeUnavailable { detail: detail.clone() }],
        ),
    })
}

/// The proposal state a decision leads to.
fn status_for(decision: GateDecision, reasons: &[GateReason]) -> ProposalStatus {
    match decision {
        GateDecision::Accept => ProposalStatus::Eligible,
        GateDecision::Reject if reasons.iter().any(GateReason::is_measured) => ProposalStatus::Rejected,
        GateDecision::Reject => ProposalStatus::Inconclusive,
    }
}

const fn split_order(split: EvalSplit) -> u8 {
    match split {
        EvalSplit::Development => 0,
        EvalSplit::Validation => 1,
        EvalSplit::Holdout => 2,
    }
}

fn entry_of(verified: &VerifiedEvaluation) -> GateEvaluationEntry {
    GateEvaluationEntry {
        evaluation_id: verified.receipt.record_id.clone(),
        split: verified.receipt.split,
        receipt_digest: verified.receipt_digest.clone(),
        outcome: verified.outcome.clone(),
        summary: verified.summary.clone(),
    }
}

const fn split_label(split: EvalSplit) -> &'static str {
    match split {
        EvalSplit::Development => "development",
        EvalSplit::Validation => "validation",
        EvalSplit::Holdout => "holdout",
    }
}

/// Who is verifying a gate receipt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum VerifyMode {
    /// Publication authorization: the receipt must be unexpired and settled.
    Publication,
    /// `gate_proposal` finishing a decision it already stored.
    Resume,
}

/// Another recorded evaluation of the same subject that does not pass.
struct Sibling {
    split: EvalSplit,
    evaluation_id: String,
    /// A measured failure; otherwise it has no verdict (no receipt, or an
    /// uncertain outcome).
    measured: bool,
}

/// Everything resolved from the pinned evaluations.
struct Resolved {
    verified: Vec<VerifiedEvaluation>,
    /// Other recorded evaluations of the same subject that do not pass.
    unpinned: Vec<Sibling>,
    /// Siblings without a verdict that an earlier epoch's decision already
    /// adjudicated; they do not block, and are recorded in the decision.
    adjudicated: Vec<String>,
    spec: EvalSuite,
}
impl SkillLifecycleStore {
    fn gates_dir(&self) -> std::path::PathBuf {
        self.root().join(GATES_DIR)
    }

    /// Moves a proposal through the pre-gate steps `source_reviewed` and
    /// `evaluated`. `evaluating` is entered through [`Self::begin_evaluation`]
    /// (it binds the governing suite), and everything after `evaluated` belongs
    /// to the gate and to publication, so those targets are refused here: a
    /// caller cannot make a proposal eligible by hand. A non-empty `reason`
    /// (the reviewer or the review note) is required and recorded.
    ///
    /// `source_reviewed` is not reachable here: it needs the recorded source
    /// review (`review_source`), which the gate and the authorization check
    /// again.
    pub(in crate::skills) fn advance_proposal(
        &self,
        id: &str,
        next: ProposalStatus,
        reason: &str,
    ) -> Result<SkillProposal, LifecycleError> {
        if next != ProposalStatus::Evaluated {
            return Err(LifecycleError::ProposalRefused(format!(
                "{} is entered through the source review or begin_evaluation, or decided by the gate or by publication, not by hand",
                next.as_str()
            )));
        }
        if reason.trim().is_empty() {
            return Err(LifecycleError::ProposalRefused(
                "a reviewer or review note is required".to_string(),
            ));
        }
        self.transition_proposal(id, next, reason, &[])
    }

    /// Moves a `SourceReviewed` or `Inconclusive` proposal to `Evaluating` and
    /// binds the suite that governs it. The binding is durable and independent
    /// of whoever gates later: the gate and `verify_gate` use it, and their
    /// `suite_digest` argument may only equal it. The first call also fixes the
    /// start of the evaluation window (an evaluation that began earlier cannot
    /// count for this proposal); every call opens a new evaluation epoch, the
    /// recorded new attempt with the same frozen bindings of spec 8.1. A
    /// proposal gets at most [`MAX_GATE_EPOCHS`] epochs, so an uncertain
    /// outcome (including an exhausted budget) cannot be retried forever.
    pub(in crate::skills) fn begin_evaluation(
        &self,
        id: &str,
        suite_digest: &str,
        reason: &str,
    ) -> Result<SkillProposal, GateError> {
        if reason.trim().is_empty() {
            return Err(GateError::Unsupported(
                "a reviewer or review note is required".to_string(),
            ));
        }
        let suite = self.read_suite(suite_digest)?;
        let lock = self.lock_proposals()?;
        let mut proposal = self
            .read_proposal_record(id)?
            .ok_or_else(|| GateError::Unsupported(format!("no such proposal {id}")))?;
        self.require_source_review(&proposal)?;
        if suite.suite().skill_id != proposal.skill_id {
            return Err(GateError::Unbound("the suite belongs to another skill".to_string()));
        }
        if !matches!(
            proposal.status,
            ProposalStatus::SourceReviewed | ProposalStatus::Inconclusive
        ) {
            return Err(LifecycleError::InvalidProposalTransition {
                proposal_id: proposal.proposal_id,
                from: proposal.status.as_str(),
                to: ProposalStatus::Evaluating.as_str(),
            }
            .into());
        }
        if let Some(bound) = &proposal.required_suite
            && bound != suite_digest
        {
            return Err(GateError::SuiteChanged(format!(
                "the proposal is bound to suite {bound}, not {suite_digest}"
            )));
        }
        if proposal.evaluation_epoch >= MAX_GATE_EPOCHS {
            return Err(GateError::Unsupported(format!(
                "the proposal used its {MAX_GATE_EPOCHS} evaluation epochs"
            )));
        }
        let now = Utc::now();
        proposal.required_suite = Some(suite_digest.to_string());
        proposal.evaluation_epoch += 1;
        proposal.evaluating_since.get_or_insert(now);
        proposal.status = ProposalStatus::Evaluating;
        proposal.status_reason = Some(bounded(reason));
        proposal.updated_at = now;
        self.write_proposal(&lock, &proposal)?;
        Ok(proposal)
    }

    /// Moves a proposal to `next` under the proposal lock. `also_ok` lists
    /// states in which the proposal already counts as moved (idempotent retry).
    fn transition_proposal(
        &self,
        id: &str,
        next: ProposalStatus,
        reason: &str,
        also_ok: &[ProposalStatus],
    ) -> Result<SkillProposal, LifecycleError> {
        self.transition_proposal_in(id, next, reason, also_ok, None)
    }

    /// Like [`Self::transition_proposal`]; with `epoch` the proposal must still
    /// be in that evaluation epoch, checked under the lock.
    fn transition_proposal_in(
        &self,
        id: &str,
        next: ProposalStatus,
        reason: &str,
        also_ok: &[ProposalStatus],
        epoch: Option<u32>,
    ) -> Result<SkillProposal, LifecycleError> {
        let lock = self.lock_proposals()?;
        let mut proposal = self
            .read_proposal_record(id)?
            .ok_or_else(|| LifecycleError::ProposalRefused(format!("no such proposal {id}")))?;
        if epoch.is_some_and(|epoch| proposal.evaluation_epoch != epoch) {
            return Err(LifecycleError::ProposalRefused(format!(
                "proposal {id} moved to evaluation epoch {}; the decision belongs to an earlier one",
                proposal.evaluation_epoch
            )));
        }
        if proposal.status == ProposalStatus::PublicationAuthorized && next != ProposalStatus::Committed {
            // A publication that went live (crash before the proposal was
            // finalized) must not be stranded as stale, failed or cancelled.
            self.settle_publication(&lock, &mut proposal)?;
        }
        if proposal.status == next || also_ok.contains(&proposal.status) {
            return Ok(proposal);
        }
        if !proposal.status.can_become(next) {
            return Err(LifecycleError::InvalidProposalTransition {
                proposal_id: proposal.proposal_id,
                from: proposal.status.as_str(),
                to: next.as_str(),
            });
        }
        proposal.status = next;
        proposal.status_reason = Some(bounded(reason));
        proposal.updated_at = Utc::now();
        self.write_proposal(&lock, &proposal)?;
        Ok(proposal)
    }

    /// Marks the proposal `Stale` when `error` says the skill moved, and hands
    /// the error back.
    fn stale_on(&self, proposal_id: &str, error: GateError) -> GateError {
        if let GateError::Stale(reason) = &error
            && let Err(e) = self.transition_proposal(proposal_id, ProposalStatus::Stale, reason, &[])
        {
            // A proposal whose publication is already live cannot go stale;
            // the transition settled it as committed and the original error stands.
            if matches!(e, LifecycleError::InvalidProposalTransition { .. }) {
                return error;
            }
            return e.into();
        }
        error
    }

    /// Verifies the pinned evaluations and binds them to the proposal's
    /// governing suite, evaluation window and epoch. Staleness of the skill is
    /// returned as [`GateError::Stale`], a different suite as
    /// [`GateError::SuiteChanged`].
    ///
    /// Every other evaluation of the same skill, baseline, candidate,
    /// generation and suite counts, whenever it started, so a pass cannot be
    /// picked out of several runs. A measured failure counts forever. One
    /// without a verdict (no receipt, or an uncertain outcome) blocks once: an
    /// earlier epoch's decision of this proposal that pinned it or recorded it
    /// as pending adjudicated it, and from then on it no longer blocks (if it
    /// later completes as a measured failure it counts again). A reservation
    /// attributes an evaluation that has no receipt.
    fn resolve_evaluations(
        &self,
        proposal: &SkillProposal,
        candidate: &RevisionId,
        policy: &GatePolicy,
        suite_digest: &str,
        pins: &[EvaluationPin],
    ) -> Result<Resolved, GateError> {
        let unbound_suite = || {
            GateError::Unsupported(
                "no governing suite is bound; the proposal must enter evaluation through begin_evaluation".to_string(),
            )
        };
        let governing = proposal.required_suite.as_deref().ok_or_else(unbound_suite)?;
        let since = proposal.evaluating_since.ok_or_else(unbound_suite)?;
        if suite_digest != governing {
            return Err(GateError::SuiteChanged(format!(
                "the proposal is governed by suite {governing}, not {suite_digest}"
            )));
        }
        let suite = self.read_suite(governing)?;
        if suite.suite().skill_id != proposal.skill_id {
            return Err(GateError::Unbound("the suite belongs to another skill".to_string()));
        }
        let has_holdout = suite.cases_of(EvalSplit::Holdout).next().is_some();
        if policy.require_holdout && !has_holdout {
            return Err(GateError::SuiteBelowFloor(
                "the policy requires a holdout split and the governing suite has none".to_string(),
            ));
        }
        if suite.suite().min_cases < policy.min_cases_floor {
            return Err(GateError::SuiteBelowFloor(format!(
                "the governing suite needs {} cases per split, the policy requires at least {}",
                suite.suite().min_cases,
                policy.min_cases_floor
            )));
        }
        let mut verified: Vec<VerifiedEvaluation> = Vec::new();
        let mut seen: BTreeSet<EvalSplit> = BTreeSet::new();
        for pin in pins {
            // The digest is always pinned: a resealed receipt cannot verify.
            let evaluation = self.verify_evaluation(&pin.evaluation_id, Some(&pin.receipt_digest))?;
            let receipt = &evaluation.receipt;
            if receipt.split == EvalSplit::Development {
                return Err(GateError::Unbound(
                    "a development evaluation cannot gate a proposal".to_string(),
                ));
            }
            if !seen.insert(receipt.split) {
                return Err(GateError::Unbound(format!(
                    "two evaluations of the {:?} split were given",
                    receipt.split
                )));
            }
            if receipt.skill_id != proposal.skill_id
                || receipt.candidate.revision_id != *candidate
                || receipt.baseline.as_ref().map(|arm| &arm.revision_id) != proposal.baseline_revision.as_ref()
            {
                return Err(GateError::Unbound(
                    "the evaluation compares other revisions than the proposal's baseline and candidate".to_string(),
                ));
            }
            if receipt.suite_digest != governing {
                return Err(GateError::SuiteChanged(format!(
                    "evaluation {} used suite {}, the proposal is governed by {governing}",
                    receipt.record_id, receipt.suite_digest
                )));
            }
            if receipt.started_at < since {
                return Err(GateError::Unbound(format!(
                    "evaluation {} started before the proposal entered evaluation",
                    receipt.record_id
                )));
            }
            if receipt.stale || matches!(evaluation.outcome, EvaluationOutcome::Stale) {
                return Err(GateError::Stale(format!(
                    "evaluation {} ended with the skill at another generation",
                    receipt.record_id
                )));
            }
            if receipt.observed_generation != proposal.expected_generation {
                return Err(GateError::Stale(format!(
                    "evaluation {} observed generation {}, the proposal was prepared against {}",
                    receipt.record_id, receipt.observed_generation, proposal.expected_generation
                )));
            }
            verified.push(evaluation);
        }
        if !seen.contains(&EvalSplit::Validation) {
            return Err(GateError::MissingEvaluation {
                split: EvalSplit::Validation,
            });
        }
        if has_holdout && !seen.contains(&EvalSplit::Holdout) {
            return Err(GateError::MissingEvaluation {
                split: EvalSplit::Holdout,
            });
        }
        verified.sort_by_key(|evaluation| split_order(evaluation.receipt.split));
        let pinned: BTreeSet<&str> = pins.iter().map(|pin| pin.evaluation_id.as_str()).collect();
        let mut unpinned = Vec::new();
        let mut adjudicated: Vec<String> = Vec::new();
        let adjudicated_before = self.adjudicated_before(proposal)?;
        let is_subject = |skill: &SkillId,
                          suite_digest: &str,
                          split: EvalSplit,
                          candidate_revision: &RevisionId,
                          baseline: Option<&RevisionId>,
                          generation: u64| {
            *skill == proposal.skill_id
                && suite_digest == governing
                && split != EvalSplit::Development
                && candidate_revision == candidate
                && baseline == proposal.baseline_revision.as_ref()
                && generation == proposal.expected_generation
        };
        // The reservation (written before the first run) attributes an
        // evaluation that has no receipt. It is not sealed, so a receipt that
        // matches this subject overrides a reservation that names another one.
        for id in self.list_evaluation_ids()? {
            if pinned.contains(id.as_str()) {
                continue;
            }
            let attempt = self.read_attempt(&id);
            let attributed = attempt.as_ref().and_then(|a| {
                Some(is_subject(
                    &a.skill_id,
                    &a.suite_digest,
                    a.split,
                    a.candidate_revision.as_ref()?,
                    a.baseline_revision.as_ref(),
                    a.observed_generation?,
                ))
            });
            let receipt = self.read_sealed_receipt(&id);
            let receipt_subject = receipt.as_ref().ok().map(|(r, _)| {
                is_subject(
                    &r.skill_id,
                    &r.suite_digest,
                    r.split,
                    &r.candidate.revision_id,
                    r.baseline.as_ref().map(|arm| &arm.revision_id),
                    r.observed_generation,
                )
            });
            if attributed == Some(false) && receipt_subject == Some(true) {
                tracing::warn!(
                    evaluation = %id,
                    "an evaluation reservation names another subject than its receipt; the receipt is used"
                );
            }
            if attributed == Some(true) && receipt_subject == Some(false) {
                tracing::warn!(
                    evaluation = %id,
                    "an evaluation reservation names this subject but its receipt names another; counted as pending"
                );
            }
            let ours = receipt_subject == Some(true) || attributed == Some(true);
            if !ours {
                if attributed.is_none() && receipt.is_err() {
                    tracing::warn!(
                        evaluation = %id,
                        "an evaluation reservation without a subject and without a readable receipt is ignored"
                    );
                }
                continue;
            }
            // A receipt of this subject that is present but unusable is never
            // an in-flight run: it cannot be adjudicated away, so the gate
            // refuses instead of treating it as pending.
            if let Err(error) = &receipt
                && !matches!(error, EvaluationError::NotFound(_))
            {
                return Err(GateError::Tampered(format!(
                    "the receipt of evaluation {id} for this subject cannot be read: {error}"
                )));
            }
            let (split, measured) = match receipt {
                Ok((receipt, _)) if receipt_subject == Some(true) => {
                    let sibling = self.verify_evaluation(&id, None).map_err(|error| {
                        GateError::Tampered(format!("the evaluation {id} of this subject does not verify: {error}"))
                    })?;
                    let reasons = evaluation_reasons(suite.suite(), policy, &sibling);
                    if reasons.is_empty() {
                        continue;
                    }
                    let measured = reasons.iter().any(GateReason::is_measured);
                    (receipt.split, measured)
                }
                // No usable receipt of this subject: in flight, abandoned or
                // damaged, attributed by its reservation.
                _ => match attempt {
                    Some(attempt) => (attempt.split, false),
                    None => continue,
                },
            };
            if measured || !adjudicated_before.contains(&id) {
                unpinned.push(Sibling {
                    split,
                    evaluation_id: id,
                    measured,
                });
            } else {
                adjudicated.push(id);
            }
        }
        // The skill must still be where the proposal found it.
        let binding = self.active_binding(&proposal.skill_id)?;
        let current = match (&proposal.baseline_revision, &binding) {
            (Some(baseline), Some(binding)) => {
                binding.enabled
                    && binding.generation == proposal.expected_generation
                    && binding.revision_id == *baseline
            }
            // A new skill: nothing may have been published for it meanwhile.
            (None, None) => proposal.expected_generation == 0,
            _ => false,
        };
        if !current {
            return Err(GateError::Stale(
                "the active revision or generation changed since the proposal was prepared".to_string(),
            ));
        }
        Ok(Resolved {
            verified,
            unpinned,
            adjudicated,
            spec: suite.suite().clone(),
        })
    }

    /// Evaluations that earlier epochs of this proposal already decided on:
    /// the ones they pinned or recorded as pending.
    fn adjudicated_before(&self, proposal: &SkillProposal) -> Result<BTreeSet<String>, GateError> {
        let mut ids = BTreeSet::new();
        for epoch in 1..proposal.evaluation_epoch {
            let id = gate_id_of(self.owner_scope(), &proposal.proposal_id, epoch);
            let Some(earlier) = self.read_gate(&id)? else {
                continue;
            };
            let decision = earlier.decision;
            ids.extend(decision.evaluations.iter().map(|entry| entry.evaluation_id.clone()));
            ids.extend(decision.adjudicated_pending);
            ids.extend(
                decision
                    .deterministic_reasons
                    .into_iter()
                    .filter_map(|reason| match reason {
                        GateReason::UnpinnedPending { evaluation_id, .. } => Some(evaluation_id),
                        _ => None,
                    }),
            );
        }
        Ok(ids)
    }

    /// Gates an `Evaluated` proposal.
    ///
    /// Evaluations are verified with the pinned digests against the proposal's
    /// governing suite and the decision is recomputed from them; see the module
    /// documentation. There is one gate decision per proposal and evaluation
    /// epoch. The decision is stored first as an immutable gate receipt, then
    /// (for a measured rejection) entered in the rejection history, then the
    /// proposal moves to `Eligible`, `Rejected` or `Inconclusive`. A crash
    /// between the steps is repaired by calling `gate_proposal` again with the
    /// same evaluations: the stored receipt is verified and its remaining steps
    /// are completed (the judge is not asked twice); other evaluations are
    /// refused, so a second look cannot turn a reject into an accept. When the
    /// skill moved since the proposal was prepared, the proposal becomes
    /// `Stale` and the call fails. An accept that expired before it could be
    /// settled sends the proposal back to `Inconclusive` and fails.
    ///
    /// `judge` is consulted only when the deterministic checks pass, and it can
    /// only turn an accept into a reject. A cancellation by the caller returns
    /// [`GateError::Cancelled`] and writes nothing; after an `Inconclusive`
    /// result, `begin_evaluation` opens a new epoch over the same evaluations.
    pub(in crate::skills) async fn gate_proposal(
        &self,
        request: &GateRequest<'_>,
        judge: Option<&dyn GateJudge>,
        cancel: &CancellationToken,
    ) -> Result<GateOutcome, GateError> {
        request.policy.validate()?;
        let experiment = self.current_gate_experiment_binding()?;
        let expected_subject = self.current_gate_review_subject()?;
        if cancel.is_cancelled() {
            return Err(GateError::Cancelled);
        }
        let proposal = self
            .read_proposal(request.proposal_id)?
            .ok_or_else(|| GateError::Unsupported(format!("no such proposal {}", request.proposal_id)))?;
        // This is the exact immutable clone later used by resolve_evaluations,
        // JudgeBrief and the sealed decision. Never silently capture a new epoch.
        self.require_expected_gate_subject(
            &proposal,
            request.suite_digest,
            request.evaluations,
            expected_subject.as_ref(),
        )?;
        let Some(candidate) = proposal.candidate_revision.clone() else {
            return Err(GateError::Unsupported(
                "the proposal has no staged candidate".to_string(),
            ));
        };
        if !matches!(
            proposal.operation,
            ProposalOperation::Revise | ProposalOperation::Create
        ) {
            return Err(GateError::Unsupported(
                "only revisions of an existing skill and new skills are gated".to_string(),
            ));
        }
        let gate_id = gate_id_of(self.owner_scope(), &proposal.proposal_id, proposal.evaluation_epoch);
        if let Some(existing) = self.read_gate(&gate_id)? {
            if existing.decision.experiment != experiment {
                return Err(GateError::Unbound(
                    "an existing gate belongs to another physical accounting context".into(),
                ));
            }
            // Resume: the decision for this epoch was already made and sealed.
            let mut stored: Vec<(&str, &str)> = existing
                .decision
                .evaluations
                .iter()
                .map(|entry| (entry.evaluation_id.as_str(), entry.receipt_digest.as_str()))
                .collect();
            let mut given: Vec<(&str, &str)> = request
                .evaluations
                .iter()
                .map(|pin| (pin.evaluation_id.as_str(), pin.receipt_digest.as_str()))
                .collect();
            stored.sort_unstable();
            given.sort_unstable();
            if stored != given {
                return Err(GateError::Unbound(
                    "a gate decision for this evaluation epoch exists and rests on other evaluations".to_string(),
                ));
            }
            let (verified, new_evidence) = self
                .verify_gate_inner(
                    &existing.decision,
                    &existing.digest,
                    request.policy,
                    request.suite_digest,
                    VerifyMode::Resume,
                )
                .map_err(|e| self.stale_on(&proposal.proposal_id, e))?;
            if verified.is_accept() && Utc::now() > verified.decision.expires_at {
                if proposal.status == ProposalStatus::Evaluated {
                    self.transition_proposal(
                        &proposal.proposal_id,
                        ProposalStatus::Inconclusive,
                        "the gate accept expired before it was settled",
                        &[],
                    )?;
                }
                return Err(GateError::Expired(verified.decision.expires_at));
            }
            let recorded = RecordedGate {
                decision: verified.decision,
                digest: verified.digest,
            };
            if new_evidence
                && recorded.decision.decision == GateDecision::Accept
                && matches!(
                    proposal.status,
                    ProposalStatus::Eligible | ProposalStatus::PublicationAuthorized
                )
            {
                // Already settled: the proposal cannot go back to evaluation,
                // so the new evidence is a refusal, not a state change.
                return Err(GateError::NewFailingEvidence(
                    "another evaluation of the same subject was recorded or changed after the decision".to_string(),
                ));
            }
            if new_evidence && recorded.decision.decision == GateDecision::Accept {
                // The accept no longer rests on all the evidence: it is not
                // settled; the proposal goes back to be evaluated again.
                let proposal = self.transition_proposal_in(
                    &proposal.proposal_id,
                    ProposalStatus::Inconclusive,
                    "new or changed evidence appeared before the accept was settled",
                    &[],
                    Some(recorded.decision.epoch),
                )?;
                return Ok(GateOutcome {
                    gate: recorded,
                    proposal,
                    rejection: None,
                });
            }
            return self.settle_gate(recorded);
        }
        if proposal.status != ProposalStatus::Evaluated {
            return Err(GateError::NotGateable {
                proposal_id: proposal.proposal_id,
                status: proposal.status.as_str(),
            });
        }
        if let (Some(want), Some(judge)) = (&request.policy.judge_identity, judge)
            && judge.identity() != *want
        {
            return Err(GateError::Policy(
                "the judge is not the one the policy pins".to_string(),
            ));
        }
        let resolved = self
            .resolve_evaluations(
                &proposal,
                &candidate,
                request.policy,
                request.suite_digest,
                request.evaluations,
            )
            .map_err(|e| self.stale_on(&proposal.proposal_id, e))?;
        // The recorded source review is part of what a decision rests on.
        self.require_source_review(&proposal)?;
        let entries: Vec<GateEvaluationEntry> = resolved.verified.iter().map(entry_of).collect();
        let deterministic = deterministic_reasons(&resolved, request.policy);
        let judge_record = match (deterministic.is_empty(), judge) {
            (true, Some(judge)) => {
                let metrics: Vec<JudgedEvaluation> = resolved
                    .verified
                    .iter()
                    .map(|evaluation| JudgedEvaluation {
                        split: split_label(evaluation.receipt.split),
                        outcome: evaluation.outcome.clone(),
                        summary: evaluation.summary.clone(),
                    })
                    .collect();
                let brief = JudgeBrief {
                    proposal: &proposal,
                    metrics: &metrics,
                };
                Some(consult_judge(judge, &brief, request.policy, cancel, judge_limit(self, request.policy)).await?)
            }
            _ => None,
        };
        let (decision, reasons) = final_decision(&deterministic, judge_record.as_ref(), request.policy)?;
        let decided_at = Utc::now();
        let record = SkillGateDecision {
            schema_version: SCHEMA_VERSION,
            record_id: gate_id,
            owner_scope: self.owner_scope().clone(),
            experiment,
            proposal_id: proposal.proposal_id.clone(),
            epoch: proposal.evaluation_epoch,
            operation: proposal.operation,
            skill_id: proposal.skill_id.clone(),
            baseline_revision: proposal.baseline_revision.clone(),
            candidate_revision: candidate,
            expected_generation: proposal.expected_generation,
            suite_digest: request.suite_digest.to_string(),
            evaluations: entries,
            policy: request.policy.clone(),
            deterministic_reasons: deterministic,
            judge: judge_record,
            adjudicated_pending: resolved.adjudicated.clone(),
            decision,
            reasons,
            review_identity: GATE_IDENTITY.to_string(),
            decided_at,
            expires_at: expiry_of(decided_at, request.policy),
        };
        // The first receipt written for the epoch wins; a concurrent gate of
        // the same proposal settles that one.
        let recorded = self.write_gate_if_absent(&record)?;
        self.settle_gate(recorded)
    }

    /// Enters a measured rejection in the rejection history and moves the
    /// proposal to the state the stored decision leads to. Both steps are
    /// idempotent.
    pub(super) fn settle_gate(&self, gate: RecordedGate) -> Result<GateOutcome, GateError> {
        let decision = &gate.decision;
        let target = status_for(decision.decision, &decision.reasons);
        let proposal = self
            .read_proposal(&decision.proposal_id)?
            .ok_or_else(|| GateError::Tampered("the gated proposal no longer exists".to_string()))?;
        if proposal.evaluation_epoch != decision.epoch {
            return Err(GateError::Unsupported(
                "the gate decision belongs to an earlier evaluation epoch of the proposal".to_string(),
            ));
        }
        let rejection = if target == ProposalStatus::Rejected {
            Some(self.record_rejection(&NewRejection {
                proposal_id: proposal.proposal_id.clone(),
                operation: proposal.operation,
                skill_id: proposal.skill_id.clone(),
                material: proposal.material_texts(),
                pattern_ids: proposal.pattern_ids.clone(),
                evidence_ids: proposal.evidence_ids.clone(),
                environment_binding: proposal.environment_binding.clone(),
                reasons: decision.reasons.iter().map(GateReason::describe).collect(),
            })?)
        } else {
            None
        };
        let reason = decision.reasons.first().map_or_else(
            || format!("accepted by gate {}", decision.record_id),
            GateReason::describe,
        );
        let also_ok: &[ProposalStatus] = if target == ProposalStatus::Eligible {
            &[ProposalStatus::PublicationAuthorized]
        } else {
            &[]
        };
        let proposal =
            self.transition_proposal_in(&proposal.proposal_id, target, &reason, also_ok, Some(decision.epoch))?;
        Ok(GateOutcome {
            gate,
            proposal,
            rejection,
        })
    }

    /// Verifies an accepting gate receipt for the publication step.
    ///
    /// `expected_digest` is the digest pinned when the gate was decided,
    /// `policy` the caller's own policy (a receipt decided under another policy
    /// is refused) and `suite_digest` the suite the proposal is governed by (it
    /// is checked against the proposal's own binding, never trusted). Every
    /// evaluation is re-verified with its recorded digest, the decision is
    /// recomputed, and the receipt must be unexpired and still current: the
    /// skill at the generation and revision the proposal was prepared against,
    /// the proposal in the state the decision leads to. A receipt that is not an
    /// accept is refused with [`GateError::NotAccepted`].
    ///
    /// An expired receipt leaves the proposal `Eligible`; the state machine has
    /// no way back to evaluation from there, so publication authorization moves
    /// such a proposal to `Stale` (allowed from any pre-commit state) and a new
    /// proposal is made.
    pub fn verify_gate(
        &self,
        gate_id: &str,
        expected_digest: &str,
        policy: &GatePolicy,
        suite_digest: &str,
    ) -> Result<VerifiedGate, GateError> {
        let stored = self
            .read_gate(gate_id)?
            .ok_or_else(|| GateError::Tampered(format!("no gate receipt {gate_id}")))?;
        let (verified, _) = self.verify_gate_inner(
            &stored.decision,
            expected_digest,
            policy,
            suite_digest,
            VerifyMode::Publication,
        )?;
        if !verified.is_accept() {
            return Err(GateError::NotAccepted(format!(
                "gate {gate_id} decided to reject the proposal"
            )));
        }
        Ok(verified)
    }

    fn verify_gate_inner(
        &self,
        decision: &SkillGateDecision,
        expected_digest: &str,
        policy: &GatePolicy,
        suite_digest: &str,
        mode: VerifyMode,
    ) -> Result<(VerifiedGate, bool), GateError> {
        policy.validate()?;
        let tampered = |reason: &str| Err(GateError::Tampered(reason.to_string()));
        let digest = seal_digest(decision)?;
        if digest != expected_digest {
            return tampered("the gate receipt does not match the pinned digest");
        }
        if decision.policy != *policy {
            return tampered("the gate decided under a different policy than the one required");
        }
        if let Some(want) = &policy.judge_identity
            && decision.judge.as_ref().is_some_and(|judge| judge.identity != *want)
        {
            return tampered("the judge is not the one the policy pins");
        }
        if decision.review_identity != GATE_IDENTITY || decision.schema_version != SCHEMA_VERSION {
            return tampered("the gate receipt names another reviewer or schema");
        }
        if decision.suite_digest != suite_digest {
            return Err(GateError::SuiteChanged(format!(
                "the gate used suite {}, the proposal is governed by {suite_digest}",
                decision.suite_digest
            )));
        }
        if decision.decided_at > Utc::now() {
            return tampered("the gate receipt is dated in the future");
        }
        if decision.expires_at != expiry_of(decision.decided_at, policy) {
            return tampered("the expiry differs from the decision time plus the policy's validity");
        }
        if gate_id_of(self.owner_scope(), &decision.proposal_id, decision.epoch) != decision.record_id {
            return tampered("the gate identity does not match its proposal and epoch");
        }
        let proposal = self
            .read_proposal(&decision.proposal_id)?
            .ok_or_else(|| GateError::Tampered("the gated proposal no longer exists".to_string()))?;
        if proposal.candidate_revision.as_ref() != Some(&decision.candidate_revision)
            || proposal.baseline_revision != decision.baseline_revision
            || proposal.skill_id != decision.skill_id
            || proposal.operation != decision.operation
            || proposal.expected_generation != decision.expected_generation
            || proposal.evaluation_epoch != decision.epoch
        {
            return tampered("the gate receipt does not describe the stored proposal");
        }
        let pins: Vec<EvaluationPin> = decision
            .evaluations
            .iter()
            .map(|entry| EvaluationPin {
                evaluation_id: entry.evaluation_id.clone(),
                receipt_digest: entry.receipt_digest.clone(),
            })
            .collect();
        let resolved =
            self.resolve_evaluations(&proposal, &decision.candidate_revision, policy, suite_digest, &pins)?;
        let recomputed: Vec<GateEvaluationEntry> = resolved.verified.iter().map(entry_of).collect();
        if recomputed != decision.evaluations {
            return tampered("the recorded metrics differ from the ones recomputed from the receipts");
        }
        // Evidence about other evaluations that changed since the decision.
        let deterministic = deterministic_reasons(&resolved, policy);
        let mut new_evidence = false;
        if deterministic != decision.deterministic_reasons || resolved.adjudicated != decision.adjudicated_pending {
            let stored_pinned: Vec<GateReason> = decision
                .deterministic_reasons
                .iter()
                .filter(|reason| !reason.is_sibling())
                .cloned()
                .collect();
            if stored_pinned != pinned_reasons(&resolved, policy) {
                return tampered("the recorded reasons differ from the ones recomputed from the receipts");
            }
            match mode {
                VerifyMode::Publication => {
                    return Err(GateError::NewFailingEvidence(
                        "another evaluation of the same subject was recorded or changed after the decision".to_string(),
                    ));
                }
                // A stored non-accept is settled as it was decided; only an
                // accept has to answer to the new evidence.
                VerifyMode::Resume => new_evidence = true,
            }
        }
        let (expected_decision, expected_reasons) =
            final_decision(&decision.deterministic_reasons, decision.judge.as_ref(), policy)?;
        if expected_decision != decision.decision || expected_reasons != decision.reasons {
            return tampered("the recorded decision differs from the one recomputed");
        }
        let target = status_for(decision.decision, &decision.reasons);
        let status_ok = proposal.status == target
            || (target == ProposalStatus::Eligible && proposal.status == ProposalStatus::PublicationAuthorized)
            // A resumed gate has not settled its proposal yet.
            || (mode == VerifyMode::Resume && proposal.status == ProposalStatus::Evaluated);
        if !status_ok {
            return tampered("the proposal is not in the state the gate decision leads to");
        }
        if mode == VerifyMode::Publication && Utc::now() > decision.expires_at {
            return Err(GateError::Expired(decision.expires_at));
        }
        Ok((
            VerifiedGate {
                decision: decision.clone(),
                digest,
            },
            new_evidence,
        ))
    }

    /// The sealed gate decision of the proposal's current evaluation epoch, if
    /// one was made. Its digest is what authorization pins.
    pub(super) fn recorded_gate_for(&self, proposal: &SkillProposal) -> Result<Option<RecordedGate>, GateError> {
        if proposal.evaluation_epoch == 0 {
            return Ok(None);
        }
        self.read_gate(&gate_id_of(
            self.owner_scope(),
            &proposal.proposal_id,
            proposal.evaluation_epoch,
        ))
    }

    fn read_gate(&self, id: &str) -> Result<Option<RecordedGate>, GateError> {
        if !is_gate_id(id) {
            return Err(GateError::Tampered(format!("{id:?} is not a gate id")));
        }
        let path = self.gates_dir().join(format!("{id}.json"));
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LifecycleError::io(&path, e).into()),
        }
        check_private(&path)?;
        let sealed: SealedGate = read_record(&path)?;
        if seal_digest(&sealed.decision)? != sealed.digest {
            return Err(GateError::Tampered(
                "the gate receipt does not match its digest".to_string(),
            ));
        }
        self.check_owner_scope(&sealed.decision.owner_scope)?;
        if sealed.decision.record_id != id {
            return Err(GateError::Tampered(
                "the gate identity does not match its location".to_string(),
            ));
        }
        Ok(Some(RecordedGate {
            decision: sealed.decision,
            digest: sealed.digest,
        }))
    }

    /// Writes the receipt unless one exists for the same gate id; concurrent
    /// gates of one proposal converge on the first receipt written.
    fn write_gate_if_absent(&self, decision: &SkillGateDecision) -> Result<RecordedGate, GateError> {
        let dir = self.gates_dir();
        ensure_dir_synced(self.root(), &dir)?;
        let lock_id = reserved_lock(GATE_LOCK_ID)?;
        let _store = self.lock_store(true, GATE_LOCK_TIMEOUT)?;
        let _skill = self.lock_skill(&lock_id, GATE_LOCK_TIMEOUT)?;
        if let Some(existing) = self.read_gate(&decision.record_id)? {
            if existing.decision.experiment != decision.experiment {
                return Err(GateError::Unbound(
                    "a concurrent gate won under another physical accounting context".into(),
                ));
            }
            if decision.experiment.is_some()
                && (existing.decision.proposal_id != decision.proposal_id
                    || existing.decision.epoch != decision.epoch
                    || existing.decision.candidate_revision != decision.candidate_revision
                    || existing.decision.baseline_revision != decision.baseline_revision
                    || existing.decision.expected_generation != decision.expected_generation
                    || existing.decision.suite_digest != decision.suite_digest
                    || existing.decision.policy != decision.policy
                    || existing.decision.evaluations != decision.evaluations)
            {
                return Err(GateError::Unbound(
                    "a concurrent gate captured different immutable subject/evaluation inputs".into(),
                ));
            }
            return Ok(existing);
        }
        let digest = seal_digest(decision)?;
        let path = dir.join(format!("{}.json", decision.record_id));
        let sealed = SealedGate {
            decision: decision.clone(),
            digest: digest.clone(),
        };
        self.write_atomic(&lock_id, &path, &record_bytes(&sealed, &path)?)?;
        Ok(RecordedGate {
            decision: decision.clone(),
            digest,
        })
    }
}

fn expiry_of(decided_at: DateTime<Utc>, policy: &GatePolicy) -> DateTime<Utc> {
    let valid_for = chrono::Duration::seconds(i64::try_from(policy.valid_for_secs).unwrap_or(i64::MAX));
    decided_at.checked_add_signed(valid_for).unwrap_or(decided_at)
}

/// Reasons of the pinned evaluations themselves.
fn pinned_reasons(resolved: &Resolved, policy: &GatePolicy) -> Vec<GateReason> {
    resolved
        .verified
        .iter()
        .flat_map(|evaluation| evaluation_reasons(&resolved.spec, policy, evaluation))
        .collect()
}

/// One reason for every other recorded evaluation of the same subject that does
/// not pass: a measured failure rejects, one without a verdict is uncertain.
fn sibling_reasons(resolved: &Resolved) -> Vec<GateReason> {
    resolved
        .unpinned
        .iter()
        .map(|sibling| {
            if sibling.measured {
                GateReason::UnpinnedFailure {
                    split: sibling.split,
                    evaluation_id: sibling.evaluation_id.clone(),
                }
            } else {
                GateReason::UnpinnedPending {
                    split: sibling.split,
                    evaluation_id: sibling.evaluation_id.clone(),
                }
            }
        })
        .collect()
}

/// All deterministic reasons.
fn deterministic_reasons(resolved: &Resolved, policy: &GatePolicy) -> Vec<GateReason> {
    let mut reasons = pinned_reasons(resolved, policy);
    reasons.extend(sibling_reasons(resolved));
    reasons
}

/// The usage to record for a judge call: the larger of what the reply reported
/// and what the per-call tally counted, so a reply cannot under-report.
fn merged_usage(reported: ReportedUsage, tally: ReportedUsage) -> ReportedUsage {
    ReportedUsage {
        physical_requests: reported.physical_requests.max(tally.physical_requests),
        settled_requests: reported.settled_requests.max(tally.settled_requests),
        ..reported
    }
}

/// The policy deadline, plus the test clock installed on this store.
#[cfg(test)]
fn judge_limit(store: &SkillLifecycleStore, policy: &GatePolicy) -> super::judge_deadline::Limit {
    // The stage worker is a spawned task. The clock is stored against the
    // store address so this call sees the test's clock.
    super::judge_deadline::Limit::from_millis(policy.judge_timeout_ms)
        .with_clock(super::judge_deadline::installed(store))
}

/// The policy deadline. Production has no logical judge clock.
#[cfg(not(test))]
const fn judge_limit(_store: &SkillLifecycleStore, policy: &GatePolicy) -> super::judge_deadline::Limit {
    super::judge_deadline::Limit::from_millis(policy.judge_timeout_ms)
}

/// Asks the judge under the policy's deadline and the caller's cancellation.
/// A failure, denial or timeout becomes a non-passing record that carries the
/// physical tally of the call; it is never an approval. A caller cancellation
/// returns [`GateError::Cancelled`] and records nothing.
async fn consult_judge(
    judge: &dyn GateJudge,
    brief: &JudgeBrief<'_>,
    _policy: &GatePolicy,
    cancel: &CancellationToken,
    limit: super::judge_deadline::Limit,
) -> Result<JudgeRecord, GateError> {
    let tally = Arc::new(TallyCounters::default());
    let identity = judge.identity();
    let call = judge.review(brief, Arc::clone(&tally));
    let unavailable = |detail: &str, usage: ReportedUsage| JudgeRecord {
        identity: identity.clone(),
        result: JudgeResult::Unavailable {
            detail: sanitize_reason(detail),
        },
        usage,
    };
    tokio::select! {
        () = cancel.cancelled() => Err(GateError::Cancelled),
        answer = limit.apply(call) => Ok(match answer {
            Err(_) => unavailable("the judge did not answer before its deadline", tally.snapshot()),
            Ok(Err(JudgeError::BudgetDenied)) => unavailable("the judge budget reservation was denied", tally.snapshot()),
            Ok(Err(JudgeError::Failed { detail, usage })) => unavailable(&detail, merged_usage(usage, tally.snapshot())),
            Ok(Ok(reply)) => JudgeRecord {
                identity: identity.clone(),
                result: match reply.opinion {
                    JudgeOpinion::Approve => JudgeResult::Approved,
                    JudgeOpinion::Concern { reason } => JudgeResult::Concern { reason: sanitize_reason(&reason) },
                },
                usage: merged_usage(reply.usage, tally.snapshot()),
            },
        }),
    }
}

/// Test support: rewrites a stored gate receipt the way a writer with access to
/// the store could, re-sealing it (the seal is a plain SHA-256). Returns the
/// new digest.
#[cfg(test)]
pub(super) fn rewrite_gate_for_test(
    store: &SkillLifecycleStore,
    id: &str,
    mutate: impl FnOnce(&mut SkillGateDecision),
) -> String {
    let mut stored = store
        .read_gate(id)
        .expect("test: read gate")
        .expect("test: gate exists");
    mutate(&mut stored.decision);
    let digest = seal_digest(&stored.decision).expect("test: seal");
    let sealed = SealedGate {
        decision: stored.decision,
        digest: digest.clone(),
    };
    fs::write(
        store.gates_dir().join(format!("{id}.json")),
        serde_json::to_vec_pretty(&sealed).expect("test: json"),
    )
    .expect("test: write gate");
    digest
}
