#![cfg(test)]
#![allow(clippy::indexing_slicing)]

//! SL07: the deterministic gate, gate receipts and the optional judge. The
//! evaluations are real SL06 evaluations made with a scripted executor; the
//! judge is a deterministic mock.

use super::activate;
use super::evaluate::{
    Fixture, ORACLE, Scripted, case, fixture, good, skill, suite_with, validation_suite, write_revision,
};
use crate::skills::lifecycle::evaluate::rewrite_evaluation_for_test;
use crate::skills::lifecycle::gate::rewrite_gate_for_test;
use crate::skills::lifecycle::proposal::proposal_id_of;
use crate::skills::lifecycle::proposer_model::TallyCounters;
use crate::skills::lifecycle::{
    AppliedEdit, AuthorBinding, BoundedDiff, BudgetRecord, CaseExecutor, EditOp, EvalSplit, EvaluationError,
    EvaluationPin, EvaluationRequest, ExecutionRequest, FrozenSuite, GateDecision, GateError, GateJudge, GatePolicy,
    GateReason, GateRequest, JudgeBrief, JudgeError, JudgeOpinion, JudgeReply, JudgeResult, MAX_GATE_EPOCHS,
    ModelBinding, ModelCallError, ModelJudge, ModelReply, ProposalBasis, ProposalOperation, ProposalStatus,
    ProposerModel, RejectionRecord, ReportedUsage, RunObservation, SkillProposal,
};
use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn policy() -> GatePolicy {
    GatePolicy {
        version: 1,
        max_regressions: 0,
        valid_for_secs: 3600,
        judge_required: false,
        judge_timeout_ms: 1_000,
        require_holdout: false,
        min_cases_floor: 2,
        judge_identity: None,
    }
}

/// Stores a proposal for the fixture's candidate, staged and `Evaluating`
/// under `suite`.
fn evaluating_proposal(fx: &Fixture, suite: &FrozenSuite) -> SkillProposal {
    evaluating_proposal_as(fx, suite, "request-1")
}

fn evaluating_proposal_as(fx: &Fixture, suite: &FrozenSuite, request_id: &str) -> SkillProposal {
    let owner = fx.store.owner_scope().clone();
    let id = proposal_id_of(&owner, request_id);
    let bundle = fx
        .store
        .read_revision(&skill(), &fx.candidate, None)
        .expect("test: candidate")
        .bundle;
    let now = Utc::now();
    let proposal = SkillProposal {
        schema_version: 1,
        record_id: id.clone(),
        owner_scope: owner,
        proposal_id: id.clone(),
        request_id: request_id.to_string(),
        operation: ProposalOperation::Revise,
        skill_id: skill(),
        requested_patterns: Vec::new(),
        expected_generation: 1,
        baseline_revision: fx.baseline.clone(),
        candidate_revision: Some(fx.candidate.clone()),
        pattern_ids: Vec::new(),
        evidence_ids: Vec::new(),
        basis: ProposalBasis {
            failed_tasks_on_baseline: 2,
            contradicting_tasks: 0,
            adherence_verified: false,
        },
        author: AuthorBinding {
            model: ModelBinding {
                provider_label: "test".to_string(),
                model: "m".to_string(),
                temperature_milli: 0,
            },
            system_prompt_version: "v".to_string(),
            system_prompt_sha256: "0".repeat(64),
        },
        environment_binding: "env-test".to_string(),
        budget: BudgetRecord {
            max_input_bytes: 1000,
            max_output_bytes: 1000,
            call_timeout_ms: 1000,
        },
        status: ProposalStatus::Draft,
        status_reason: None,
        failure: None,
        diff: Some(BoundedDiff {
            edits: vec![AppliedEdit {
                op: EditOp::Replace,
                old: Some("ANSWER=draft".to_string()),
                new: "ANSWER=report-v2".to_string(),
            }],
            description_change: None,
            changed_tokens: 2,
            baseline_tokens: 20,
            allowed_tokens: 4,
            summary: None,
        }),
        attempts: Vec::new(),
        required_suite: None,
        evaluation_epoch: 0,
        evaluating_since: None,
        created_at: now,
        updated_at: now,
    };
    {
        let lock = fx.store.lock_proposals().expect("test: lock");
        fx.store.stage_candidate(&id, &bundle).expect("test: stage");
        fx.store.write_proposal(&lock, &proposal).expect("test: write proposal");
    }
    fx.store.write_suite(suite).expect("test: write suite");
    fx.store
        .advance_proposal(&id, ProposalStatus::SourceReviewed, "reviewer test")
        .expect("test: source review");
    fx.store
        .begin_evaluation(&id, suite.digest(), "evaluation test")
        .expect("test: begin evaluation")
}

fn mark_evaluated(fx: &Fixture, proposal: &SkillProposal) -> SkillProposal {
    fx.store
        .advance_proposal(&proposal.proposal_id, ProposalStatus::Evaluated, "evaluated test")
        .expect("test: evaluated")
}

/// A proposal that was evaluated once with `exec` on the validation split.
async fn evaluated(fx: &Fixture, suite: &FrozenSuite, exec: &Scripted) -> (SkillProposal, EvaluationPin) {
    let proposal = evaluating_proposal(fx, suite);
    let pin = evaluate(fx, suite, exec).await;
    (mark_evaluated(fx, &proposal), pin)
}

async fn evaluate(fx: &Fixture, suite: &FrozenSuite, exec: &Scripted) -> EvaluationPin {
    let recorded = fx
        .store
        .evaluate(
            &super::evaluate::request(fx, suite, EvalSplit::Validation),
            exec,
            &CancellationToken::new(),
        )
        .await
        .expect("test: evaluate");
    EvaluationPin {
        evaluation_id: recorded.receipt.record_id.clone(),
        receipt_digest: recorded.receipt_digest,
    }
}

/// The candidate answers the oracle on every case, the baseline on none.
fn all_better() -> Scripted {
    Scripted::new(|r| Ok(good(r)))
}

/// Neither arm answers the oracle: a tie.
fn tie() -> Scripted {
    Scripted::new(|r| {
        let mut o = good(r);
        o.response = "notes: nothing useful".to_string();
        Ok(o)
    })
}

async fn gate(
    fx: &Fixture,
    proposal: &SkillProposal,
    suite: &FrozenSuite,
    pins: &[EvaluationPin],
    policy: &GatePolicy,
    judge: Option<&dyn GateJudge>,
) -> Result<crate::skills::lifecycle::GateOutcome, GateError> {
    fx.store
        .gate_proposal(
            &GateRequest {
                proposal_id: &proposal.proposal_id,
                suite_digest: suite.digest(),
                evaluations: pins,
                policy,
            },
            judge,
            &CancellationToken::new(),
        )
        .await
}

fn status(fx: &Fixture, proposal: &SkillProposal) -> ProposalStatus {
    fx.store
        .read_proposal(&proposal.proposal_id)
        .expect("test: read")
        .expect("test: present")
        .status
}

struct MockJudge {
    behavior: Behavior,
    calls: AtomicU32,
}

enum Behavior {
    Approve,
    Concern(&'static str),
    Fail,
    Hang,
}

impl MockJudge {
    fn new(behavior: Behavior) -> Self {
        Self {
            behavior,
            calls: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl GateJudge for MockJudge {
    fn identity(&self) -> String {
        "mock-judge".to_string()
    }

    async fn review(&self, _brief: &JudgeBrief<'_>, tally: Arc<TallyCounters>) -> Result<JudgeReply, JudgeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tally.record_reservation();
        match &self.behavior {
            Behavior::Approve => {
                tally.record_settlement();
                Ok(JudgeReply {
                    opinion: JudgeOpinion::Approve,
                    usage: tally.snapshot(),
                })
            }
            Behavior::Concern(reason) => {
                tally.record_settlement();
                Ok(JudgeReply {
                    opinion: JudgeOpinion::Concern {
                        reason: (*reason).to_string(),
                    },
                    usage: tally.snapshot(),
                })
            }
            Behavior::Fail => Err(JudgeError::Failed {
                detail: "provider unavailable".to_string(),
                usage: tally.snapshot(),
            }),
            Behavior::Hang => {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Err(JudgeError::BudgetDenied)
            }
        }
    }
}

// ---- accept and the receipt ------------------------------------------------

#[tokio::test]
async fn a_verified_gain_is_accepted_and_the_receipt_verifies_for_publication() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let outcome = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Accept);
    assert_eq!(outcome.gate.decision.reasons, Vec::<GateReason>::new());
    assert_eq!(outcome.proposal.status, ProposalStatus::Eligible);
    assert!(outcome.rejection.is_none());
    // The metrics in the receipt are the recomputed ones.
    let entry = &outcome.gate.decision.evaluations[0];
    assert_eq!((entry.summary.baseline_passed, entry.summary.candidate_passed), (0, 2));
    assert_eq!(entry.receipt_digest, pin.receipt_digest);

    let gate_id = outcome.gate.decision.record_id.clone();
    let verified = fx
        .store
        .verify_gate(&gate_id, &outcome.gate.digest, &policy(), suite.digest())
        .expect("test: verify gate");
    assert!(verified.is_accept());

    // Gating again with the same pins converges on the same receipt.
    let again = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
        .await
        .expect("test: gate again");
    assert_eq!(again.gate.digest, outcome.gate.digest);

    // A pin that is not the receipt's digest, another policy and another
    // current suite are all refused.
    assert!(
        fx.store
            .verify_gate(&gate_id, &"0".repeat(64), &policy(), suite.digest())
            .is_err()
    );
    let stricter = GatePolicy { version: 2, ..policy() };
    assert!(
        fx.store
            .verify_gate(&gate_id, &outcome.gate.digest, &stricter, suite.digest())
            .is_err()
    );
    assert!(matches!(
        fx.store
            .verify_gate(&gate_id, &outcome.gate.digest, &policy(), &"1".repeat(64)),
        Err(GateError::SuiteChanged(_))
    ));
}

#[tokio::test]
async fn a_forged_gate_receipt_does_not_verify_even_when_resealed_and_pinned() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &tie()).await;
    let outcome = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
    let gate_id = outcome.gate.decision.record_id.clone();

    // Flipping the decision and resealing: the original pin no longer matches.
    let forged = rewrite_gate_for_test(&fx.store, &gate_id, |d| {
        d.decision = GateDecision::Accept;
        d.reasons.clear();
        d.deterministic_reasons.clear();
    });
    assert!(
        fx.store
            .verify_gate(&gate_id, &outcome.gate.digest, &policy(), suite.digest())
            .is_err()
    );
    // An attacker who pins the forged digest too is caught by the recomputation.
    let error = fx
        .store
        .verify_gate(&gate_id, &forged, &policy(), suite.digest())
        .expect_err("test: forged accept");
    assert!(error.to_string().contains("recomputed"), "{error}");
}

// ---- forged and resealed evaluations ---------------------------------------

#[tokio::test]
async fn a_forged_score_is_refused() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &tie()).await;
    // A fixed improvement written into the receipt and correctly re-sealed.
    rewrite_evaluation_for_test(&fx.store, &pin.evaluation_id, |receipt, _| {
        receipt.outcome = crate::skills::lifecycle::EvaluationOutcome::Improved;
    });
    // Under the original pin the forged receipt is refused.
    let error = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
        .await
        .expect_err("test: forged outcome under the old pin");
    assert!(matches!(error, GateError::Evaluation(_)), "{error}");
    // Pinning the forged digest does not help: the outcome is recomputed.
    let forged_digest = forged_pin_digest(&fx, &pin.evaluation_id);
    let forged_pin = EvaluationPin {
        evaluation_id: pin.evaluation_id.clone(),
        receipt_digest: forged_digest.expect("test: forged digest"),
    };
    let error = gate(&fx, &proposal, &suite, &[forged_pin], &policy(), None)
        .await
        .expect_err("test: forged outcome under the forged pin");
    assert!(error.to_string().contains("recomputed"), "{error}");
    assert_eq!(status(&fx, &proposal), ProposalStatus::Evaluated);
}

/// The digest of the receipt as currently stored (what an attacker would pin).
fn forged_pin_digest(fx: &Fixture, id: &str) -> Option<String> {
    let text = std::fs::read_to_string(fx.store.root().join("evaluations").join(id).join("receipt.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("digest")?.as_str().map(str::to_string)
}

#[tokio::test]
async fn a_resealed_evaluation_without_a_matching_pin_is_refused() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    // A consistent rewrite that keeps the outcome true but changes the receipt.
    rewrite_evaluation_for_test(&fx.store, &pin.evaluation_id, |receipt, _| {
        receipt.seed_control = "resealed".to_string();
    });
    let error = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
        .await
        .expect_err("test: resealed receipt");
    assert!(error.to_string().contains("pinned digest"), "{error}");
    assert_eq!(status(&fx, &proposal), ProposalStatus::Evaluated);
}

#[tokio::test]
async fn missing_and_unpinned_evaluations_are_refused() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    // No evaluation at all.
    assert!(matches!(
        gate(&fx, &proposal, &suite, &[], &policy(), None).await,
        Err(GateError::MissingEvaluation {
            split: EvalSplit::Validation
        })
    ));
    // An evaluation that does not exist.
    let ghost = EvaluationPin {
        evaluation_id: format!("eval-{}", "a".repeat(32)),
        receipt_digest: pin.receipt_digest.clone(),
    };
    assert!(gate(&fx, &proposal, &suite, &[ghost], &policy(), None).await.is_err());
    // A deleted case file.
    std::fs::remove_file(
        fx.store
            .root()
            .join("evaluations")
            .join(&pin.evaluation_id)
            .join("cases.jsonl"),
    )
    .expect("test: remove");
    assert!(
        gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
            .await
            .is_err()
    );
    // A policy that is not valid decides nothing.
    let bad = GatePolicy {
        valid_for_secs: 0,
        ..policy()
    };
    assert!(matches!(
        gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &bad, None).await,
        Err(GateError::Policy(_))
    ));
    assert_eq!(status(&fx, &proposal), ProposalStatus::Evaluated);
}

// ---- regression, gain and uncertainty ---------------------------------------

#[tokio::test]
async fn a_regression_is_rejected_and_recorded_so_the_same_material_is_suppressed() {
    let fx = fixture();
    let suite = validation_suite(3);
    // The baseline answers case 0 only; the candidate answers cases 1 and 2.
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        let candidate = super::evaluate::is_candidate(r);
        let case0 = r.prompt.contains("number 0");
        o.response = if candidate != case0 {
            format!("notes: {ORACLE}")
        } else {
            "notes: draft".to_string()
        };
        Ok(o)
    });
    let (proposal, pin) = evaluated(&fx, &suite, &exec).await;
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
    assert!(
        outcome
            .gate
            .decision
            .reasons
            .iter()
            .any(|r| matches!(r, GateReason::Regression { regressed: 1, .. })),
        "{:?}",
        outcome.gate.decision.reasons
    );
    assert_eq!(outcome.proposal.status, ProposalStatus::Rejected);

    // The rejection is on record ...
    let rejection = outcome.rejection.expect("test: rejection recorded");
    assert_eq!(rejection.proposal_id, proposal.proposal_id);
    assert_eq!(fx.store.rejections_for(&skill()).expect("test: rejections").len(), 1);
    // ... and the same material, in the same environment, is suppressed.
    let material = proposal.material_texts();
    let refs: Vec<&str> = material.iter().map(String::as_str).collect();
    let suppressing = fx
        .store
        .suppressing_rejection(
            &skill(),
            ProposalOperation::Revise,
            &refs,
            &proposal.evidence_ids,
            &proposal.environment_binding,
        )
        .expect("test: suppression check");
    assert_eq!(suppressing.map(|r| r.proposal_id), Some(proposal.proposal_id.clone()));
    // A rejected proposal cannot be gated into an accept by gating it again.
    let status_after = status(&fx, &proposal);
    assert_eq!(status_after, ProposalStatus::Rejected);
}

#[tokio::test]
async fn a_tie_or_too_small_a_gain_is_rejected() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &tie()).await;
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
    assert!(matches!(
        outcome.gate.decision.reasons[0],
        GateReason::InsufficientGain {
            gain: 0,
            required: 1,
            ..
        }
    ));
    assert_eq!(outcome.proposal.status, ProposalStatus::Rejected);
    assert!(outcome.rejection.is_some());
}

#[tokio::test]
async fn invalid_runs_are_uncertain_and_do_not_suppress_the_material() {
    let fx = fixture();
    let suite = validation_suite(2);
    let exec = Scripted::new(|r| {
        let mut o = good(r);
        if r.prompt.contains("number 1") {
            o.response = String::new();
        }
        Ok(o)
    });
    let (proposal, pin) = evaluated(&fx, &suite, &exec).await;
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
    assert!(
        outcome
            .gate
            .decision
            .reasons
            .iter()
            .any(|r| matches!(r, GateReason::InvalidRuns { .. }))
    );
    assert!(!outcome.gate.decision.reasons.iter().any(GateReason::is_measured));
    assert_eq!(outcome.proposal.status, ProposalStatus::Inconclusive);
    assert!(outcome.rejection.is_none());
    assert_eq!(
        fx.store.rejections_for(&skill()).expect("test: rejections"),
        Vec::<RejectionRecord>::new()
    );
}

#[tokio::test]
async fn an_exhausted_budget_is_uncertain() {
    let fx = fixture();
    let mut spec = suite_with(vec![
        case("case-0", EvalSplit::Validation, "write release notes number 0"),
        case("case-1", EvalSplit::Validation, "write release notes number 1"),
    ]);
    // Four runs of 10 tokens each against a ceiling of 25.
    spec.budget.max_total_tokens = 25;
    let suite = spec.freeze().expect("test: freeze");
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
    assert!(
        outcome
            .gate
            .decision
            .reasons
            .iter()
            .any(|r| matches!(r, GateReason::BudgetExhausted { .. })),
        "{:?}",
        outcome.gate.decision.reasons
    );
    assert_eq!(outcome.proposal.status, ProposalStatus::Inconclusive);
}

// ---- stale ------------------------------------------------------------------

#[tokio::test]
async fn an_evaluation_of_a_moved_skill_is_stale_and_the_proposal_becomes_stale() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    // Someone publishes the candidate (or anything else) first.
    let digest = fx
        .store
        .read_revision(&skill(), &fx.candidate, None)
        .expect("test: candidate")
        .record_digest;
    activate(&fx.store, &skill(), &(fx.candidate.clone(), digest), 1, true);
    let error = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect_err("test: stale");
    assert!(matches!(error, GateError::Stale(_)), "{error}");
    assert_eq!(status(&fx, &proposal), ProposalStatus::Stale);
}

#[tokio::test]
async fn an_evaluation_made_with_another_suite_is_refused() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    // The suite was edited and refrozen: a different digest is now current.
    let mut spec = suite.suite().clone();
    spec.suite_id = "notes-suite-v2".to_string();
    let newer = spec.freeze().expect("test: freeze");
    fx.store.write_suite(&newer).expect("test: write suite");
    let error = gate(&fx, &proposal, &newer, &[pin], &policy(), None)
        .await
        .expect_err("test: suite changed");
    assert!(matches!(error, GateError::SuiteChanged(_)), "{error}");
    assert_eq!(status(&fx, &proposal), ProposalStatus::Evaluated);
}

#[tokio::test]
async fn a_proposal_that_is_not_evaluated_is_not_gated() {
    let fx = fixture();
    let suite = validation_suite(2);
    let proposal = evaluating_proposal(&fx, &suite);
    let pin = evaluate(&fx, &suite, &all_better()).await;
    // Still `Evaluating`: nothing says the evaluation is finished.
    assert!(matches!(
        gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None).await,
        Err(GateError::NotGateable {
            status: "evaluating",
            ..
        })
    ));
    // The state machine does not let a caller make a proposal eligible by hand,
    // nor enter evaluation without binding a suite.
    for next in [
        ProposalStatus::Eligible,
        ProposalStatus::Evaluating,
        ProposalStatus::Rejected,
    ] {
        assert!(
            fx.store
                .advance_proposal(&proposal.proposal_id, next, "by hand")
                .is_err()
        );
    }
    mark_evaluated(&fx, &proposal);
    let outcome = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.proposal.status, ProposalStatus::Eligible);
}

// ---- the judge --------------------------------------------------------------

#[tokio::test]
async fn the_judge_cannot_turn_a_reject_into_an_accept() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &tie()).await;
    let judge = MockJudge::new(Behavior::Approve);
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), Some(&judge))
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
    assert_eq!(outcome.proposal.status, ProposalStatus::Rejected);
    // It is not even asked about a deterministic reject.
    assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
    assert!(outcome.gate.decision.judge.is_none());

    // A receipt that claims an approving judge for a deterministic reject
    // does not verify.
    let forged = rewrite_gate_for_test(&fx.store, &outcome.gate.decision.record_id, |d| {
        d.judge = Some(crate::skills::lifecycle::JudgeRecord {
            identity: "mock-judge".to_string(),
            result: JudgeResult::Approved,
            usage: ReportedUsage::default(),
        });
    });
    assert!(
        fx.store
            .verify_gate(&outcome.gate.decision.record_id, &forged, &policy(), suite.digest())
            .is_err()
    );
}

#[tokio::test]
async fn a_judge_can_add_a_reason_to_reject_but_an_approval_adds_nothing() {
    let suite = validation_suite(2);
    // Approval: accepted as before, the call and its tally are on the receipt.
    {
        let fx = fixture();
        let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
        let judge = MockJudge::new(Behavior::Approve);
        let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), Some(&judge))
            .await
            .expect("test: gate");
        assert_eq!(outcome.gate.decision.decision, GateDecision::Accept);
        let record = outcome.gate.decision.judge.expect("test: judge record");
        assert_eq!(record.result, JudgeResult::Approved);
        assert_eq!((record.usage.physical_requests, record.usage.settled_requests), (1, 1));
    }
    // A concern: rejected and recorded.
    {
        let fx = fixture();
        let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
        let judge = MockJudge::new(Behavior::Concern("widens tool access"));
        let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), Some(&judge))
            .await
            .expect("test: gate");
        assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
        assert!(matches!(
            outcome.gate.decision.reasons[0],
            GateReason::JudgeConcern { .. }
        ));
        assert_eq!(outcome.proposal.status, ProposalStatus::Rejected);
        assert!(outcome.rejection.is_some());
    }
    // A judge that fails is non-passing but not a measured rejection.
    {
        let fx = fixture();
        let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
        let judge = MockJudge::new(Behavior::Fail);
        let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), Some(&judge))
            .await
            .expect("test: gate");
        assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
        assert_eq!(outcome.proposal.status, ProposalStatus::Inconclusive);
        assert!(outcome.rejection.is_none());
    }
    // A judge that hangs hits the policy deadline.
    {
        let fx = fixture();
        let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
        let judge = MockJudge::new(Behavior::Hang);
        let quick = GatePolicy {
            judge_timeout_ms: 30,
            ..policy()
        };
        let outcome = gate(&fx, &proposal, &suite, &[pin], &quick, Some(&judge))
            .await
            .expect("test: gate");
        assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
        let record = outcome.gate.decision.judge.expect("test: judge record");
        assert!(matches!(record.result, JudgeResult::Unavailable { .. }));
        assert_eq!(record.usage.physical_requests, 1);
    }
    // A required judge that is absent is non-passing.
    {
        let fx = fixture();
        let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
        let required = GatePolicy {
            judge_required: true,
            ..policy()
        };
        let outcome = gate(&fx, &proposal, &suite, &[pin], &required, None)
            .await
            .expect("test: gate");
        assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
        assert_eq!(outcome.gate.decision.reasons, vec![GateReason::JudgeRequired]);
    }
}

struct ScriptedModel {
    binding: ModelBinding,
    reply: Mutex<Option<Result<String, ModelCallError>>>,
}

#[async_trait]
impl ProposerModel for ScriptedModel {
    fn binding(&self) -> &ModelBinding {
        &self.binding
    }

    async fn complete(
        &self,
        _system: &str,
        _user: &str,
        tally: Arc<TallyCounters>,
    ) -> Result<ModelReply, ModelCallError> {
        tally.record_reservation();
        tally.record_settlement();
        let scripted = self.reply.lock().take();
        match scripted {
            Some(Ok(text)) => Ok(ModelReply {
                text,
                usage: tally.snapshot(),
            }),
            Some(Err(e)) => Err(e),
            None => Err(ModelCallError::failed("no scripted reply")),
        }
    }
}

fn model_judge(reply: Result<String, ModelCallError>) -> ModelJudge {
    ModelJudge::new(Arc::new(ScriptedModel {
        binding: ModelBinding {
            provider_label: "test".to_string(),
            model: "judge-model".to_string(),
            temperature_milli: 0,
        },
        reply: Mutex::new(Some(reply)),
    }))
}

#[tokio::test]
async fn the_model_judge_reads_a_strict_verdict_and_fails_closed() {
    let suite = validation_suite(2);
    for (reply, expect) in [
        (Ok(r#"{"verdict":"approve"}"#.to_string()), GateDecision::Accept),
        (
            Ok("```json\n{\"verdict\":\"concern\",\"reason\":\"contradicts step 2\"}\n```".to_string()),
            GateDecision::Reject,
        ),
        (Ok("looks fine to me".to_string()), GateDecision::Reject),
        (Ok(r#"{"verdict":"concern"}"#.to_string()), GateDecision::Reject),
        (
            Ok(r#"{"verdict":"approve","passed":true}"#.to_string()),
            GateDecision::Reject,
        ),
        (Err(ModelCallError::BudgetDenied), GateDecision::Reject),
    ] {
        let fx = fixture();
        let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
        let judge = model_judge(reply);
        let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), Some(&judge))
            .await
            .expect("test: gate");
        assert_eq!(
            outcome.gate.decision.decision, expect,
            "{:?}",
            outcome.gate.decision.reasons
        );
        let record = outcome.gate.decision.judge.expect("test: judge record");
        assert!(record.identity.starts_with("model-judge/"), "{}", record.identity);
    }
}

// ---- governing suite, selection, epochs, races ---------------------------------

fn edit_proposal(fx: &Fixture, id: &str, edit: impl FnOnce(&mut SkillProposal)) {
    let lock = fx.store.lock_proposals().expect("test: lock");
    let mut proposal = fx
        .store
        .read_proposal_record(id)
        .expect("test: read")
        .expect("test: present");
    edit(&mut proposal);
    fx.store.write_proposal(&lock, &proposal).expect("test: write");
}

async fn gate_with_cancel(
    fx: &Fixture,
    proposal: &SkillProposal,
    suite: &FrozenSuite,
    pins: &[EvaluationPin],
    judge: Option<&dyn GateJudge>,
    cancel: &CancellationToken,
) -> Result<crate::skills::lifecycle::GateOutcome, GateError> {
    fx.store
        .gate_proposal(
            &GateRequest {
                proposal_id: &proposal.proposal_id,
                suite_digest: suite.digest(),
                evaluations: pins,
                policy: &policy(),
            },
            judge,
            cancel,
        )
        .await
}

#[tokio::test]
async fn a_caller_chosen_lax_suite_cannot_govern_the_gate() {
    let fx = fixture();
    let strict = validation_suite(2);
    let proposal = evaluating_proposal(&fx, &strict);
    // The caller freezes its own suite and evaluates against it.
    let mut lax_spec = strict.suite().clone();
    lax_spec.suite_id = "lax".to_string();
    lax_spec.allowed_regressions = 3;
    let lax = lax_spec.freeze().expect("test: freeze");
    let pin = evaluate(&fx, &lax, &all_better()).await;
    mark_evaluated(&fx, &proposal);
    // Naming the lax suite is refused: the proposal is bound to the other one.
    assert!(matches!(
        gate(&fx, &proposal, &lax, std::slice::from_ref(&pin), &policy(), None).await,
        Err(GateError::SuiteChanged(_))
    ));
    // Naming the governing suite does not make the lax evaluation count.
    assert!(matches!(
        gate(&fx, &proposal, &strict, std::slice::from_ref(&pin), &policy(), None).await,
        Err(GateError::SuiteChanged(_))
    ));
    assert_eq!(status(&fx, &proposal), ProposalStatus::Evaluated);
    // A suite cannot be rebound after the fact either.
    assert!(
        fx.store
            .begin_evaluation(&proposal.proposal_id, lax.digest(), "rebind")
            .is_err()
    );
}

#[tokio::test]
async fn verify_gate_checks_the_proposals_suite_not_the_callers() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    let mut other = suite.suite().clone();
    other.suite_id = "other".to_string();
    other.allowed_regressions = 3;
    let other = other.freeze().expect("test: freeze");
    fx.store.write_suite(&other).expect("test: write suite");
    assert!(matches!(
        fx.store.verify_gate(
            &outcome.gate.decision.record_id,
            &outcome.gate.digest,
            &policy(),
            other.digest()
        ),
        Err(GateError::SuiteChanged(_))
    ));
}

#[tokio::test]
async fn the_policy_floor_refuses_a_governing_suite_that_is_too_weak() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let needs_holdout = GatePolicy {
        require_holdout: true,
        ..policy()
    };
    assert!(matches!(
        gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &needs_holdout, None).await,
        Err(GateError::SuiteBelowFloor(_))
    ));
    let needs_cases = GatePolicy {
        min_cases_floor: 5,
        ..policy()
    };
    assert!(matches!(
        gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &needs_cases, None).await,
        Err(GateError::SuiteBelowFloor(_))
    ));
    assert_eq!(status(&fx, &proposal), ProposalStatus::Evaluated);
}

#[tokio::test]
async fn a_holdout_is_required_when_the_governing_suite_has_one() {
    let fx = fixture();
    let suite = suite_with(vec![
        case("v-0", EvalSplit::Validation, "validation task zero"),
        case("v-1", EvalSplit::Validation, "validation task one"),
        case("h-0", EvalSplit::Holdout, "holdout task zero"),
        case("h-1", EvalSplit::Holdout, "holdout task one"),
    ])
    .freeze()
    .expect("test: freeze");
    let proposal = evaluating_proposal(&fx, &suite);
    let validation = evaluate(&fx, &suite, &all_better()).await;
    mark_evaluated(&fx, &proposal);
    assert!(matches!(
        gate(
            &fx,
            &proposal,
            &suite,
            std::slice::from_ref(&validation),
            &policy(),
            None
        )
        .await,
        Err(GateError::MissingEvaluation {
            split: EvalSplit::Holdout
        })
    ));
    let held = fx
        .store
        .evaluate(
            &super::evaluate::request(&fx, &suite, EvalSplit::Holdout),
            &all_better(),
            &CancellationToken::new(),
        )
        .await
        .expect("test: holdout evaluation");
    let holdout = EvaluationPin {
        evaluation_id: held.receipt.record_id.clone(),
        receipt_digest: held.receipt_digest,
    };
    let required = GatePolicy {
        require_holdout: true,
        ..policy()
    };
    let outcome = gate(&fx, &proposal, &suite, &[validation, holdout], &required, None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Accept);
    assert_eq!(outcome.gate.decision.evaluations.len(), 2);
}

#[tokio::test]
async fn pinning_only_the_passing_run_of_several_is_not_an_accept() {
    let fx = fixture();
    let suite = validation_suite(2);
    let proposal = evaluating_proposal(&fx, &suite);
    let good_now = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&good_now);
    let flaky = Scripted::new(move |r| {
        let mut o = good(r);
        if !flag.load(Ordering::SeqCst) {
            o.response = "notes: nothing useful".to_string();
        }
        Ok(o)
    });
    let failing = evaluate(&fx, &suite, &flaky).await;
    good_now.store(true, Ordering::SeqCst);
    let passing = evaluate(&fx, &suite, &flaky).await;
    mark_evaluated(&fx, &proposal);
    let outcome = gate(&fx, &proposal, &suite, std::slice::from_ref(&passing), &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
    assert!(
        outcome.gate.decision.reasons.iter().any(|r| matches!(
            r,
            GateReason::UnpinnedFailure { evaluation_id, .. } if *evaluation_id == failing.evaluation_id
        )),
        "{:?}",
        outcome.gate.decision.reasons
    );
    assert_eq!(outcome.proposal.status, ProposalStatus::Rejected);
}

#[tokio::test]
async fn evaluations_must_be_the_proposals_own_one_per_split() {
    let fx = fixture();
    let suite = validation_suite(2);
    let proposal = evaluating_proposal(&fx, &suite);
    let first = evaluate(&fx, &suite, &all_better()).await;
    let second = evaluate(&fx, &suite, &all_better()).await;
    mark_evaluated(&fx, &proposal);
    // Two evaluations of the same split.
    assert!(matches!(
        gate(&fx, &proposal, &suite, &[first.clone(), second], &policy(), None).await,
        Err(GateError::Unbound(_))
    ));
    // An evaluation that began before the proposal entered evaluation.
    edit_proposal(&fx, &proposal.proposal_id, |p| {
        p.evaluating_since = Some(Utc::now() + chrono::Duration::hours(1));
    });
    assert!(matches!(
        gate(&fx, &proposal, &suite, &[first], &policy(), None).await,
        Err(GateError::Unbound(_))
    ));
}

#[tokio::test]
async fn a_development_evaluation_cannot_gate() {
    let fx = fixture();
    let mut spec = suite_with(vec![
        case("v-0", EvalSplit::Validation, "validation task zero"),
        case("v-1", EvalSplit::Validation, "validation task one"),
        case("d-0", EvalSplit::Development, "development task zero"),
        case("d-1", EvalSplit::Development, "development task one"),
    ]);
    spec.suite_id = "with-development".to_string();
    let suite = spec.freeze().expect("test: freeze");
    let proposal = evaluating_proposal(&fx, &suite);
    let validation = evaluate(&fx, &suite, &all_better()).await;
    let dev = fx
        .store
        .evaluate(
            &super::evaluate::request(&fx, &suite, EvalSplit::Development),
            &all_better(),
            &CancellationToken::new(),
        )
        .await
        .expect("test: development evaluation");
    mark_evaluated(&fx, &proposal);
    let dev_pin = EvaluationPin {
        evaluation_id: dev.receipt.record_id.clone(),
        receipt_digest: dev.receipt_digest,
    };
    assert!(matches!(
        gate(&fx, &proposal, &suite, &[dev_pin, validation], &policy(), None).await,
        Err(GateError::Unbound(_))
    ));
}

#[tokio::test]
async fn an_evaluation_of_other_revisions_is_unbound() {
    // Another candidate.
    let fx = fixture();
    let suite = validation_suite(2);
    let proposal = evaluating_proposal(&fx, &suite);
    let other = write_revision(&fx.store, "ANSWER=other");
    let recorded = fx
        .store
        .evaluate(
            &EvaluationRequest {
                suite: &suite,
                split: EvalSplit::Validation,
                baseline: Some(fx.baseline.clone()),
                candidate: other.0,
                observed_generation: 1,
            },
            &all_better(),
            &CancellationToken::new(),
        )
        .await
        .expect("test: evaluate");
    mark_evaluated(&fx, &proposal);
    let pin = EvaluationPin {
        evaluation_id: recorded.receipt.record_id.clone(),
        receipt_digest: recorded.receipt_digest,
    };
    assert!(matches!(
        gate(&fx, &proposal, &suite, &[pin], &policy(), None).await,
        Err(GateError::Unbound(_))
    ));
    // Another baseline.
    let fx = fixture();
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let candidate = fx.candidate.clone();
    edit_proposal(&fx, &proposal.proposal_id, |p| p.baseline_revision = candidate);
    assert!(matches!(
        gate(&fx, &proposal, &suite, &[pin], &policy(), None).await,
        Err(GateError::Unbound(_))
    ));
}

fn force_status(fx: &Fixture, id: &str, status: ProposalStatus) {
    edit_proposal(fx, id, |p| p.status = status);
}

#[tokio::test]
async fn concurrent_gates_of_one_proposal_converge_on_one_decision() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &tie()).await;
    let pins = [pin];
    let rules = policy();
    let (a, b) = tokio::join!(
        gate(&fx, &proposal, &suite, &pins, &rules, None),
        gate(&fx, &proposal, &suite, &pins, &rules, None)
    );
    let (a, b) = (a.expect("test: first gate"), b.expect("test: second gate"));
    assert_eq!(a.gate.digest, b.gate.digest);
    assert_eq!(a.proposal.status, ProposalStatus::Rejected);
    assert_eq!(b.proposal.status, ProposalStatus::Rejected);
    assert_eq!(fx.store.rejections_for(&skill()).expect("test: rejections").len(), 1);
}

#[tokio::test]
async fn a_crash_between_the_receipt_and_the_settlement_cannot_be_turned_into_an_accept() {
    let fx = fixture();
    let suite = validation_suite(2);
    let proposal = evaluating_proposal(&fx, &suite);
    let bad = evaluate(&fx, &suite, &tie()).await;
    let better = evaluate(&fx, &suite, &all_better()).await;
    mark_evaluated(&fx, &proposal);
    let first = gate(&fx, &proposal, &suite, std::slice::from_ref(&bad), &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(first.gate.decision.decision, GateDecision::Reject);
    // Simulate the crash: the receipt is stored, nothing was settled.
    force_status(&fx, &proposal.proposal_id, ProposalStatus::Evaluated);
    std::fs::remove_dir_all(fx.store.root().join("rejections")).expect("test: remove rejections");
    // Another pin set cannot get a second look.
    assert!(matches!(
        gate(&fx, &proposal, &suite, std::slice::from_ref(&better), &policy(), None).await,
        Err(GateError::Unbound(_))
    ));
    assert_eq!(status(&fx, &proposal), ProposalStatus::Evaluated);
    // The same evaluations finish the settlement, rejection history included.
    let resumed = gate(&fx, &proposal, &suite, std::slice::from_ref(&bad), &policy(), None)
        .await
        .expect("test: resume");
    assert_eq!(resumed.gate.digest, first.gate.digest);
    assert_eq!(resumed.proposal.status, ProposalStatus::Rejected);
    assert!(resumed.rejection.is_some());
    assert_eq!(fx.store.rejections_for(&skill()).expect("test: rejections").len(), 1);
}

#[tokio::test]
async fn a_resumed_gate_marks_a_moved_skill_stale() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &tie()).await;
    gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
        .await
        .expect("test: gate");
    force_status(&fx, &proposal.proposal_id, ProposalStatus::Evaluated);
    let digest = fx
        .store
        .read_revision(&skill(), &fx.candidate, None)
        .expect("test: candidate")
        .record_digest;
    activate(&fx.store, &skill(), &(fx.candidate.clone(), digest), 1, true);
    assert!(matches!(
        gate(&fx, &proposal, &suite, &[pin], &policy(), None).await,
        Err(GateError::Stale(_))
    ));
    assert_eq!(status(&fx, &proposal), ProposalStatus::Stale);
}

#[tokio::test]
async fn an_inconclusive_gate_can_be_retried_in_a_new_epoch_over_the_same_evaluations() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let down = MockJudge::new(Behavior::Fail);
    let first = gate(
        &fx,
        &proposal,
        &suite,
        std::slice::from_ref(&pin),
        &policy(),
        Some(&down),
    )
    .await
    .expect("test: first gate");
    assert_eq!(first.proposal.status, ProposalStatus::Inconclusive);
    // The same pin set does not get a second decision in the same epoch.
    let again = gate(
        &fx,
        &proposal,
        &suite,
        std::slice::from_ref(&pin),
        &policy(),
        Some(&down),
    )
    .await
    .expect("test: resume");
    assert_eq!(again.gate.digest, first.gate.digest);
    assert_eq!(down.calls.load(Ordering::SeqCst), 1);
    // A recorded new attempt with the same frozen bindings.
    fx.store
        .begin_evaluation(&proposal.proposal_id, suite.digest(), "judge was down")
        .expect("test: new epoch");
    mark_evaluated(&fx, &proposal);
    let up = MockJudge::new(Behavior::Approve);
    let second = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), Some(&up))
        .await
        .expect("test: second gate");
    assert_eq!(second.gate.decision.decision, GateDecision::Accept);
    assert_ne!(second.gate.decision.record_id, first.gate.decision.record_id);
    assert_eq!(second.proposal.status, ProposalStatus::Eligible);
}

#[tokio::test]
async fn retries_after_an_uncertain_outcome_are_bounded() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let down = MockJudge::new(Behavior::Fail);
    for epoch in 1..=MAX_GATE_EPOCHS {
        if epoch > 1 {
            fx.store
                .begin_evaluation(&proposal.proposal_id, suite.digest(), "retry")
                .expect("test: new epoch");
            mark_evaluated(&fx, &proposal);
        }
        let outcome = gate(
            &fx,
            &proposal,
            &suite,
            std::slice::from_ref(&pin),
            &policy(),
            Some(&down),
        )
        .await
        .expect("test: gate");
        assert_eq!(outcome.proposal.status, ProposalStatus::Inconclusive);
    }
    assert!(
        fx.store
            .begin_evaluation(&proposal.proposal_id, suite.digest(), "one more")
            .is_err()
    );
}

#[tokio::test]
async fn a_caller_cancel_during_the_judge_writes_nothing() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let judge = MockJudge::new(Behavior::Hang);
    let token = CancellationToken::new();
    let trigger = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let result = gate_with_cancel(&fx, &proposal, &suite, &[pin], Some(&judge), &token).await;
    assert!(matches!(result, Err(GateError::Cancelled)));
    let stored = std::fs::read_dir(fx.store.root().join("gates")).map_or(0, Iterator::count);
    assert_eq!(stored, 0);
    assert_eq!(status(&fx, &proposal), ProposalStatus::Evaluated);
}

#[tokio::test]
async fn an_expired_or_misdated_receipt_does_not_verify_and_an_expired_accept_is_not_settled() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let outcome = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
        .await
        .expect("test: gate");
    let id = outcome.gate.decision.record_id.clone();
    let lifetime = chrono::Duration::seconds(3600);
    // Expired, consistently dated.
    let expired = rewrite_gate_for_test(&fx.store, &id, |d| {
        d.decided_at = Utc::now() - chrono::Duration::hours(2);
        d.expires_at = d.decided_at + lifetime;
    });
    assert!(matches!(
        fx.store.verify_gate(&id, &expired, &policy(), suite.digest()),
        Err(GateError::Expired(_))
    ));
    // An expiry that was stretched.
    let stretched = rewrite_gate_for_test(&fx.store, &id, |d| {
        d.expires_at = d.decided_at + chrono::Duration::days(3)
    });
    assert!(matches!(
        fx.store.verify_gate(&id, &stretched, &policy(), suite.digest()),
        Err(GateError::Tampered(_))
    ));
    // Dated in the future.
    let future = rewrite_gate_for_test(&fx.store, &id, |d| {
        d.decided_at = Utc::now() + chrono::Duration::hours(1);
        d.expires_at = d.decided_at + lifetime;
    });
    assert!(matches!(
        fx.store.verify_gate(&id, &future, &policy(), suite.digest()),
        Err(GateError::Tampered(_))
    ));
    // A crash left the expired accept unsettled: it is not settled late.
    rewrite_gate_for_test(&fx.store, &id, |d| {
        d.decided_at = Utc::now() - chrono::Duration::hours(2);
        d.expires_at = d.decided_at + lifetime;
    });
    force_status(&fx, &proposal.proposal_id, ProposalStatus::Evaluated);
    assert!(matches!(
        gate(&fx, &proposal, &suite, &[pin], &policy(), None).await,
        Err(GateError::Expired(_))
    ));
    assert_eq!(status(&fx, &proposal), ProposalStatus::Inconclusive);
}

#[tokio::test]
async fn publication_verification_refuses_a_reject_and_the_policy_pins_the_judge() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &tie()).await;
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    assert!(matches!(
        fx.store.verify_gate(
            &outcome.gate.decision.record_id,
            &outcome.gate.digest,
            &policy(),
            suite.digest()
        ),
        Err(GateError::NotAccepted(_))
    ));

    let fx = fixture();
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let pinned = GatePolicy {
        judge_identity: Some("someone-else".to_string()),
        ..policy()
    };
    let judge = MockJudge::new(Behavior::Approve);
    assert!(matches!(
        gate(
            &fx,
            &proposal,
            &suite,
            std::slice::from_ref(&pin),
            &pinned,
            Some(&judge)
        )
        .await,
        Err(GateError::Policy(_))
    ));
    assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
    let matching = GatePolicy {
        judge_identity: Some("mock-judge".to_string()),
        ..policy()
    };
    let outcome = gate(&fx, &proposal, &suite, &[pin], &matching, Some(&judge))
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Accept);
}

// ---- evaluations without a verdict, windows, epochs ---------------------------

fn reserve_for(fx: &Fixture, suite: &FrozenSuite, candidate: &crate::skills::lifecycle::RevisionId) -> String {
    let id = super::super::evaluate::new_evaluation_id();
    fx.store
        .reserve_attempt(suite, EvalSplit::Validation, &id, candidate, Some(&fx.baseline), 1)
        .expect("test: reserve");
    id
}

#[tokio::test]
async fn an_evaluation_without_a_receipt_blocks_an_accept_but_suppresses_nothing() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    // A second run was started (or abandoned) and never wrote a receipt.
    let pending = reserve_for(&fx, &suite, &fx.candidate);
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
    assert!(
        outcome.gate.decision.reasons.iter().any(|r| matches!(
            r,
            GateReason::UnpinnedPending { evaluation_id, .. } if *evaluation_id == pending
        )),
        "{:?}",
        outcome.gate.decision.reasons
    );
    assert_eq!(outcome.proposal.status, ProposalStatus::Inconclusive);
    assert!(outcome.rejection.is_none());
}

#[tokio::test]
async fn evaluations_of_other_subjects_never_block_or_break_a_gate() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let other = write_revision(&fx.store, "ANSWER=other");
    let foreign = reserve_for(&fx, &suite, &other.0);
    // Its receipt is garbage; it belongs to another candidate, so it is not read.
    std::fs::write(
        fx.store.root().join("evaluations").join(&foreign).join("receipt.json"),
        b"not json",
    )
    .expect("test: corrupt receipt");
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Accept);
}

#[tokio::test]
async fn a_retry_with_a_clean_evaluation_after_an_uncertain_one_can_be_accepted() {
    let fx = fixture();
    let suite = validation_suite(2);
    let proposal = evaluating_proposal(&fx, &suite);
    let broken = Scripted::new(|r| {
        let mut o = good(r);
        if r.prompt.contains("number 1") {
            o.response = String::new();
        }
        Ok(o)
    });
    let first = evaluate(&fx, &suite, &broken).await;
    mark_evaluated(&fx, &proposal);
    let one = gate(&fx, &proposal, &suite, std::slice::from_ref(&first), &policy(), None)
        .await
        .expect("test: first gate");
    assert_eq!(one.proposal.status, ProposalStatus::Inconclusive);
    // Epoch 2 with a new clean evaluation: the uncertain one was adjudicated.
    fx.store
        .begin_evaluation(&proposal.proposal_id, suite.digest(), "retry")
        .expect("test: new epoch");
    let second = evaluate(&fx, &suite, &all_better()).await;
    mark_evaluated(&fx, &proposal);
    let two = gate(&fx, &proposal, &suite, &[second], &policy(), None)
        .await
        .expect("test: second gate");
    assert_eq!(two.gate.decision.decision, GateDecision::Accept);
    assert_eq!(two.proposal.status, ProposalStatus::Eligible);
    assert_eq!(two.gate.decision.adjudicated_pending, vec![first.evaluation_id]);
    fx.store
        .verify_gate(
            &two.gate.decision.record_id,
            &two.gate.digest,
            &policy(),
            suite.digest(),
        )
        .expect("test: verifies");
}

/// Executor whose first run waits until it is released, so an evaluation can be
/// in flight (reserved, no receipt) while other things happen; it then fails
/// the task.
struct Gated {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    entered: AtomicBool,
}

impl Gated {
    fn new() -> Self {
        Self {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            entered: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl CaseExecutor for Gated {
    fn identity(&self) -> String {
        "gated".to_string()
    }

    async fn run_case(
        &self,
        request: &ExecutionRequest<'_>,
        _cancel: &CancellationToken,
    ) -> Result<RunObservation, EvaluationError> {
        if !self.entered.swap(true, Ordering::SeqCst) {
            self.started.notify_one();
            self.release.notified().await;
        }
        let mut observation = good(request);
        observation.response = "notes: nothing useful".to_string();
        Ok(observation)
    }
}

#[tokio::test]
async fn an_adjudicated_pending_run_that_later_fails_makes_verification_refuse() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let slow = Gated::new();
    let run = async {
        fx.store
            .evaluate(
                &super::evaluate::request(&fx, &suite, EvalSplit::Validation),
                &slow,
                &CancellationToken::new(),
            )
            .await
            .expect("test: slow evaluation")
    };
    let choreography = async {
        slow.started.notified().await;
        // Epoch 1 meets the run in flight: blocked once, inconclusive.
        let one = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
            .await
            .expect("test: first gate");
        assert_eq!(one.proposal.status, ProposalStatus::Inconclusive);
        assert!(
            one.gate
                .decision
                .reasons
                .iter()
                .any(|r| matches!(r, GateReason::UnpinnedPending { .. }))
        );
        assert!(one.rejection.is_none());
        // Epoch 2: it was adjudicated, so it no longer blocks.
        fx.store
            .begin_evaluation(&proposal.proposal_id, suite.digest(), "retry")
            .expect("test: new epoch");
        mark_evaluated(&fx, &proposal);
        let two = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
            .await
            .expect("test: second gate");
        assert_eq!(two.gate.decision.decision, GateDecision::Accept);
        assert_eq!(two.gate.decision.adjudicated_pending.len(), 1);
        slow.release.notify_one();
        two
    };
    let (_finished, two) = tokio::join!(run, choreography);
    // The run finished as a measured failure after the accept.
    assert!(matches!(
        fx.store.verify_gate(
            &two.gate.decision.record_id,
            &two.gate.digest,
            &policy(),
            suite.digest()
        ),
        Err(GateError::NewFailingEvidence(_))
    ));
}

#[tokio::test]
async fn a_stored_non_accept_is_settled_even_when_its_pending_run_has_since_finished() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let slow = Gated::new();
    let run = async {
        fx.store
            .evaluate(
                &super::evaluate::request(&fx, &suite, EvalSplit::Validation),
                &slow,
                &CancellationToken::new(),
            )
            .await
            .expect("test: slow evaluation")
    };
    let choreography = async {
        slow.started.notified().await;
        let one = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
            .await
            .expect("test: gate");
        assert_eq!(one.proposal.status, ProposalStatus::Inconclusive);
        // The process dies before the settlement.
        force_status(&fx, &proposal.proposal_id, ProposalStatus::Evaluated);
        slow.release.notify_one();
    };
    tokio::join!(run, choreography);
    let resumed = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: resume is not stuck");
    assert_eq!(resumed.proposal.status, ProposalStatus::Inconclusive);
}

#[tokio::test]
async fn an_accept_that_meets_new_evidence_on_resume_settles_inconclusive() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let accepted = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(accepted.gate.decision.decision, GateDecision::Accept);
    force_status(&fx, &proposal.proposal_id, ProposalStatus::Evaluated);
    evaluate(&fx, &suite, &tie()).await;
    let resumed = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: resume");
    assert_eq!(resumed.proposal.status, ProposalStatus::Inconclusive);
}

#[tokio::test]
async fn a_new_proposal_with_the_same_content_does_not_get_a_fresh_start() {
    let fx = fixture();
    let suite = validation_suite(2);
    // The first proposal evaluates the candidate, fails, and is never gated.
    let _first = evaluating_proposal_as(&fx, &suite, "request-1");
    let failing = evaluate(&fx, &suite, &tie()).await;
    // A second proposal with the identical bundle evaluates it again and passes.
    let second = evaluating_proposal_as(&fx, &suite, "request-2");
    let passing = evaluate(&fx, &suite, &all_better()).await;
    mark_evaluated(&fx, &second);
    let outcome = gate(&fx, &second, &suite, &[passing], &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.gate.decision.decision, GateDecision::Reject);
    assert!(
        outcome.gate.decision.reasons.iter().any(|r| matches!(
            r,
            GateReason::UnpinnedFailure { evaluation_id, .. } if *evaluation_id == failing.evaluation_id
        )),
        "{:?}",
        outcome.gate.decision.reasons
    );
}

#[tokio::test]
async fn new_failing_evidence_after_an_accept_has_its_own_error() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    fx.store
        .verify_gate(
            &outcome.gate.decision.record_id,
            &outcome.gate.digest,
            &policy(),
            suite.digest(),
        )
        .expect("test: verifies before");
    evaluate(&fx, &suite, &tie()).await;
    assert!(matches!(
        fx.store.verify_gate(
            &outcome.gate.decision.record_id,
            &outcome.gate.digest,
            &policy(),
            suite.digest()
        ),
        Err(GateError::NewFailingEvidence(_))
    ));
}

#[tokio::test]
async fn a_decision_cannot_settle_a_proposal_that_moved_to_another_epoch() {
    let fx = fixture();
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(&fx, &suite, &tie()).await;
    let outcome = gate(&fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    edit_proposal(&fx, &proposal.proposal_id, |p| {
        p.status = ProposalStatus::Evaluated;
        p.evaluation_epoch = 2;
    });
    assert!(fx.store.settle_gate(outcome.gate).is_err());
    assert_eq!(status(&fx, &proposal), ProposalStatus::Evaluated);
}

#[tokio::test]
async fn a_damaged_receipt_of_this_subject_is_never_adjudicated_away() {
    for damage in ["garbage receipt", "tampered cases"] {
        let fx = fixture();
        let suite = validation_suite(2);
        let proposal = evaluating_proposal(&fx, &suite);
        let bad = evaluate(&fx, &suite, &tie()).await;
        let clean = evaluate(&fx, &suite, &all_better()).await;
        mark_evaluated(&fx, &proposal);
        let dir = fx.store.root().join("evaluations").join(&bad.evaluation_id);
        if damage == "garbage receipt" {
            std::fs::write(dir.join("receipt.json"), b"not json").expect("test: damage");
        } else {
            let cases = std::fs::read_to_string(dir.join("cases.jsonl")).expect("test: read");
            std::fs::write(
                dir.join("cases.jsonl"),
                cases.replacen("\"tokens\":10", "\"tokens\":11", 1),
            )
            .expect("test: damage");
        }
        let error = gate(&fx, &proposal, &suite, std::slice::from_ref(&clean), &policy(), None)
            .await
            .expect_err("test: damaged evidence");
        assert!(matches!(error, GateError::Tampered(_)), "{damage}: {error}");
        assert_eq!(status(&fx, &proposal), ProposalStatus::Evaluated);
    }
}
