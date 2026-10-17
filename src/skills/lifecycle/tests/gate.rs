#![cfg(test)]
#![allow(clippy::indexing_slicing)]

//! SL07: the deterministic gate, gate receipts and the optional judge. The
//! evaluations are real SL06 evaluations made with a scripted executor; the
//! judge is a deterministic mock.

use super::activate;
use super::evaluate::{Fixture, ORACLE, Scripted, case, fixture, good, skill, suite_with, validation_suite};
use crate::skills::lifecycle::evaluate::rewrite_evaluation_for_test;
use crate::skills::lifecycle::gate::rewrite_gate_for_test;
use crate::skills::lifecycle::proposal::proposal_id_of;
use crate::skills::lifecycle::proposer_model::TallyCounters;
use crate::skills::lifecycle::{
    AppliedEdit, AuthorBinding, BoundedDiff, BudgetRecord, EditOp, EvalSplit, EvaluationPin, FrozenSuite, GateDecision,
    GateError, GateJudge, GatePolicy, GateReason, GateRequest, JudgeBrief, JudgeError, JudgeOpinion, JudgeReply,
    JudgeResult, ModelBinding, ModelCallError, ModelJudge, ModelReply, ProposalBasis, ProposalOperation,
    ProposalStatus, ProposerModel, RejectionRecord, ReportedUsage, SkillProposal,
};
use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn policy() -> GatePolicy {
    GatePolicy {
        version: 1,
        max_regressions: 0,
        valid_for_secs: 3600,
        judge_required: false,
        judge_timeout_ms: 1_000,
    }
}

/// Stores a proposal for the fixture's candidate, staged and `Evaluated`.
fn evaluated_proposal(fx: &Fixture) -> SkillProposal {
    let owner = fx.store.owner_scope().clone();
    let id = proposal_id_of(&owner, "request-1");
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
        request_id: "request-1".to_string(),
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
        created_at: now,
        updated_at: now,
    };
    {
        let lock = fx.store.lock_proposals().expect("test: lock");
        fx.store.stage_candidate(&id, &bundle).expect("test: stage");
        fx.store.write_proposal(&lock, &proposal).expect("test: write proposal");
    }
    for next in [
        ProposalStatus::SourceReviewed,
        ProposalStatus::Evaluating,
        ProposalStatus::Evaluated,
    ] {
        fx.store
            .advance_proposal(&id, next, "test")
            .expect("test: advance proposal");
    }
    fx.store.read_proposal(&id).expect("test: read").expect("test: present")
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
    let pin = evaluate(&fx, &suite, &all_better()).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &tie()).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &tie()).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &all_better()).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &all_better()).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &exec).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &tie()).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &exec).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &all_better()).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &all_better()).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &all_better()).await;
    let proposal = evaluated_proposal(&fx);
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
    let pin = evaluate(&fx, &suite, &all_better()).await;
    let proposal = evaluated_proposal(&fx);
    // The state machine does not let a caller make a proposal eligible by hand.
    assert!(
        fx.store
            .advance_proposal(&proposal.proposal_id, ProposalStatus::Eligible, "by hand")
            .is_err()
    );
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
    let pin = evaluate(&fx, &suite, &tie()).await;
    let proposal = evaluated_proposal(&fx);
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
        let pin = evaluate(&fx, &suite, &all_better()).await;
        let proposal = evaluated_proposal(&fx);
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
        let pin = evaluate(&fx, &suite, &all_better()).await;
        let proposal = evaluated_proposal(&fx);
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
        let pin = evaluate(&fx, &suite, &all_better()).await;
        let proposal = evaluated_proposal(&fx);
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
        let pin = evaluate(&fx, &suite, &all_better()).await;
        let proposal = evaluated_proposal(&fx);
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
        let pin = evaluate(&fx, &suite, &all_better()).await;
        let proposal = evaluated_proposal(&fx);
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
        let pin = evaluate(&fx, &suite, &all_better()).await;
        let proposal = evaluated_proposal(&fx);
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
