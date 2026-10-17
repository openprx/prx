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
    #[error("not an accepting gate decision: {0}")]
    NotAccepted(String),
    #[error("evaluation is stale: {0}")]
    Stale(String),
    #[error("evaluation was made with a suite that is no longer the current one: {0}")]
    SuiteChanged(String),
    #[error("gate receipt expired at {0}")]
    Expired(DateTime<Utc>),
    #[error("gate receipt is invalid: {0}")]
    Tampered(String),
    #[error("gating was cancelled before a decision was made")]
    Cancelled,
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
    /// Digest of the suite that governs the proposal. It is not a choice: the
    /// governing suite is bound to the proposal by `begin_evaluation`, and this
    /// argument only has to equal it.
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
    JudgeConcern {
        reason: String,
    },
    JudgeUnavailable {
        detail: String,
    },
    JudgeRequired,
}

impl GateReason {
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
    pub proposal_id: String,
    /// Evaluation epoch of the proposal this decision belongs to.
    pub epoch: u32,
    pub operation: ProposalOperation,
    pub skill_id: SkillId,
    pub baseline_revision: RevisionId,
    pub candidate_revision: RevisionId,
    pub expected_generation: u64,
    pub suite_digest: String,
    pub evaluations: Vec<GateEvaluationEntry>,
    pub policy: GatePolicy,
    /// Reasons found from the evaluations alone; empty means the deterministic
    /// checks passed.
    pub deterministic_reasons: Vec<GateReason>,
    pub judge: Option<JudgeRecord>,
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

fn gate_id_of(owner: &OwnerScope, proposal_id: &str, epoch: u32) -> String {
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

/// Everything resolved from the pinned evaluations.
struct Resolved {
    verified: Vec<VerifiedEvaluation>,
    /// Other recorded evaluations of the same subject that do not pass.
    unpinned: Vec<(EvalSplit, String)>,
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
    /// Known gap, a hard gate for SL09: `source_reviewed` is only a recorded
    /// state change here. A source review record (applicability, bounded
    /// changes, content safety) must exist before SL09 moves a proposal there,
    /// and the gate must check that record once it exists.
    pub fn advance_proposal(
        &self,
        id: &str,
        next: ProposalStatus,
        reason: &str,
    ) -> Result<SkillProposal, LifecycleError> {
        if !matches!(next, ProposalStatus::SourceReviewed | ProposalStatus::Evaluated) {
            return Err(LifecycleError::ProposalRefused(format!(
                "{} is entered through begin_evaluation or decided by the gate or by publication, not by hand",
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
    pub fn begin_evaluation(&self, id: &str, suite_digest: &str, reason: &str) -> Result<SkillProposal, GateError> {
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
        let lock = self.lock_proposals()?;
        let mut proposal = self
            .read_proposal_record(id)?
            .ok_or_else(|| LifecycleError::ProposalRefused(format!("no such proposal {id}")))?;
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
            return e.into();
        }
        error
    }

    /// Verifies the pinned evaluations and binds them to the proposal's
    /// governing suite, window and epoch. Staleness of the skill is returned as
    /// [`GateError::Stale`], a different suite as [`GateError::SuiteChanged`].
    ///
    /// Every other evaluation recorded for the same skill, baseline, candidate,
    /// generation and suite since the evaluation window opened is read too, and
    /// the ones that do not pass are reported: pinning only the passing run of
    /// several is not a way to pass. (An attempt that was reserved but never
    /// produced a receipt has nothing to read and is not seen here.)
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
                || receipt.baseline.as_ref().map(|arm| &arm.revision_id) != Some(&proposal.baseline_revision)
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
        for id in self.list_evaluation_ids()? {
            if pinned.contains(id.as_str()) {
                continue;
            }
            let receipt = match self.read_sealed_receipt(&id) {
                Ok((receipt, _)) => receipt,
                Err(EvaluationError::NotFound(_)) => continue,
                Err(other) => return Err(other.into()),
            };
            let same_subject = receipt.skill_id == proposal.skill_id
                && receipt.suite_digest == governing
                && receipt.split != EvalSplit::Development
                && receipt.candidate.revision_id == *candidate
                && receipt.baseline.as_ref().map(|arm| &arm.revision_id) == Some(&proposal.baseline_revision)
                && receipt.observed_generation == proposal.expected_generation
                && receipt.started_at >= since;
            if !same_subject {
                continue;
            }
            let sibling = self.verify_evaluation(&id, None)?;
            if !evaluation_reasons(suite.suite(), policy, &sibling).is_empty() {
                unpinned.push((receipt.split, id));
            }
        }
        // The skill must still be where the proposal found it.
        let binding = self.active_binding(&proposal.skill_id)?;
        let current = binding.as_ref().is_some_and(|binding| {
            binding.enabled
                && binding.generation == proposal.expected_generation
                && binding.revision_id == proposal.baseline_revision
        });
        if !current {
            return Err(GateError::Stale(
                "the active revision or generation changed since the proposal was prepared".to_string(),
            ));
        }
        Ok(Resolved {
            verified,
            unpinned,
            spec: suite.suite().clone(),
        })
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
    pub async fn gate_proposal(
        &self,
        request: &GateRequest<'_>,
        judge: Option<&dyn GateJudge>,
        cancel: &CancellationToken,
    ) -> Result<GateOutcome, GateError> {
        request.policy.validate()?;
        if cancel.is_cancelled() {
            return Err(GateError::Cancelled);
        }
        let proposal = self
            .read_proposal(request.proposal_id)?
            .ok_or_else(|| GateError::Unsupported(format!("no such proposal {}", request.proposal_id)))?;
        let Some(candidate) = proposal.candidate_revision.clone() else {
            return Err(GateError::Unsupported(
                "the proposal has no staged candidate".to_string(),
            ));
        };
        if proposal.operation != ProposalOperation::Revise {
            return Err(GateError::Unsupported(
                "only revisions of an existing skill are gated".to_string(),
            ));
        }
        let gate_id = gate_id_of(self.owner_scope(), &proposal.proposal_id, proposal.evaluation_epoch);
        if let Some(existing) = self.read_gate(&gate_id)? {
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
            let verified = self
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
            return self.settle_gate(RecordedGate {
                decision: verified.decision,
                digest: verified.digest,
            });
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
                Some(consult_judge(judge, &brief, request.policy, cancel).await?)
            }
            _ => None,
        };
        let (decision, reasons) = final_decision(&deterministic, judge_record.as_ref(), request.policy)?;
        let decided_at = Utc::now();
        let record = SkillGateDecision {
            schema_version: SCHEMA_VERSION,
            record_id: gate_id,
            owner_scope: self.owner_scope().clone(),
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
    fn settle_gate(&self, gate: RecordedGate) -> Result<GateOutcome, GateError> {
        let decision = &gate.decision;
        let target = status_for(decision.decision, &decision.reasons);
        let proposal = self
            .read_proposal(&decision.proposal_id)?
            .ok_or_else(|| GateError::Tampered("the gated proposal no longer exists".to_string()))?;
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
        let proposal = self.transition_proposal(&proposal.proposal_id, target, &reason, also_ok)?;
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
        let verified = self.verify_gate_inner(
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
    ) -> Result<VerifiedGate, GateError> {
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
        let deterministic = deterministic_reasons(&resolved, policy);
        if deterministic != decision.deterministic_reasons {
            return tampered("the recorded reasons differ from the ones recomputed from the receipts");
        }
        let (expected_decision, expected_reasons) = final_decision(&deterministic, decision.judge.as_ref(), policy)?;
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
        Ok(VerifiedGate {
            decision: decision.clone(),
            digest,
        })
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

/// All deterministic reasons: the pinned evaluations' own, plus one for every
/// other recorded evaluation of the same subject that does not pass.
fn deterministic_reasons(resolved: &Resolved, policy: &GatePolicy) -> Vec<GateReason> {
    let mut reasons: Vec<GateReason> = resolved
        .verified
        .iter()
        .flat_map(|evaluation| evaluation_reasons(&resolved.spec, policy, evaluation))
        .collect();
    reasons.extend(resolved.unpinned.iter().map(|(split, id)| GateReason::UnpinnedFailure {
        split: *split,
        evaluation_id: id.clone(),
    }));
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

/// Asks the judge under the policy's deadline and the caller's cancellation.
/// A failure, denial or timeout becomes a non-passing record that carries the
/// physical tally of the call; it is never an approval. A caller cancellation
/// returns [`GateError::Cancelled`] and records nothing.
async fn consult_judge(
    judge: &dyn GateJudge,
    brief: &JudgeBrief<'_>,
    policy: &GatePolicy,
    cancel: &CancellationToken,
) -> Result<JudgeRecord, GateError> {
    let tally = Arc::new(TallyCounters::default());
    let identity = judge.identity();
    let call = judge.review(brief, Arc::clone(&tally));
    let deadline = Duration::from_millis(policy.judge_timeout_ms);
    let unavailable = |detail: &str, usage: ReportedUsage| JudgeRecord {
        identity: identity.clone(),
        result: JudgeResult::Unavailable {
            detail: sanitize_reason(detail),
        },
        usage,
    };
    tokio::select! {
        () = cancel.cancelled() => Err(GateError::Cancelled),
        answer = tokio::time::timeout(deadline, call) => Ok(match answer {
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
