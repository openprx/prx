#![cfg(test)]
#![allow(clippy::indexing_slicing)]

//! SL08 authorization part: Eligible -> PublicationAuthorized -> Committed,
//! with crash points, stale writers, rollback and in-flight pins. Deterministic
//! fixtures only (scripted executor, no network).

use super::activate;
use super::evaluate::{Fixture, fixture, skill, validation_suite};
use super::gate::{all_better, edit_proposal, evaluate, evaluated, gate, policy, status, tie};
use crate::skills::lifecycle::gate::rewrite_gate_for_test;
use crate::skills::lifecycle::publish::crash_hooks;
use crate::skills::lifecycle::{
    FrozenSuite, GateError, GatePolicy, LifecycleError, MAX_PUBLICATION_ATTEMPTS, ProposalFailureKind, ProposalStatus,
    SkillProposal, TransactionState, release_task_pins, task_pin,
};
use chrono::Utc;
use std::time::Duration;

struct Eligible {
    suite: FrozenSuite,
    proposal: SkillProposal,
    digest: String,
    gate_id: String,
}

async fn eligible(fx: &Fixture) -> Eligible {
    let suite = validation_suite(2);
    let (proposal, pin) = evaluated(fx, &suite, &all_better()).await;
    let outcome = gate(fx, &proposal, &suite, &[pin], &policy(), None)
        .await
        .expect("test: gate");
    assert_eq!(outcome.proposal.status, ProposalStatus::Eligible);
    Eligible {
        suite,
        proposal,
        digest: outcome.gate.digest,
        gate_id: outcome.gate.decision.record_id,
    }
}

fn authorize(fx: &Fixture, e: &Eligible) -> Result<SkillProposal, GateError> {
    fx.store
        .authorize_publication(&e.proposal.proposal_id, &e.digest, &policy(), e.suite.digest())
}

fn publish(fx: &Fixture, e: &Eligible) -> Result<crate::skills::lifecycle::PublicationOutcome, GateError> {
    fx.store
        .publish_authorized(&e.proposal.proposal_id, &policy(), e.suite.digest())
}

fn generation(fx: &Fixture) -> u64 {
    fx.store
        .active_binding(&skill())
        .expect("test: binding")
        .map_or(0, |b| b.generation)
}

fn active_revision(fx: &Fixture) -> crate::skills::lifecycle::RevisionId {
    fx.store
        .active_binding(&skill())
        .expect("test: binding")
        .expect("test: bound")
        .revision_id
}

fn record(fx: &Fixture, e: &Eligible) -> SkillProposal {
    fx.store
        .read_proposal(&e.proposal.proposal_id)
        .expect("test: read")
        .expect("test: present")
}

#[tokio::test]
async fn authorization_moves_eligible_to_authorized_and_records_the_gate_digest() {
    let fx = fixture();
    let e = eligible(&fx).await;
    let authorized = authorize(&fx, &e).expect("test: authorize");
    assert_eq!(authorized.status, ProposalStatus::PublicationAuthorized);
    let auth = authorized.authorization.expect("test: authorization record");
    assert_eq!(auth.gate_digest, e.digest);
    assert_eq!(auth.gate_id, e.gate_id);
    assert_eq!(status(&fx, &e.proposal), ProposalStatus::PublicationAuthorized);
    // Idempotent with the same digest; another digest is refused.
    let again = authorize(&fx, &e).expect("test: idempotent");
    assert_eq!(again.authorization.map(|a| a.authorized_at), Some(auth.authorized_at));
    assert!(
        fx.store
            .authorize_publication(&e.proposal.proposal_id, &"0".repeat(64), &policy(), e.suite.digest())
            .is_err()
    );
    // Nothing was published by authorizing.
    assert_eq!(generation(&fx), 1);
}

#[tokio::test]
async fn authorization_refuses_a_wrong_policy_suite_or_digest_without_changing_state() {
    let fx = fixture();
    let e = eligible(&fx).await;
    let other_policy = GatePolicy {
        max_regressions: 1,
        ..policy()
    };
    let id = &e.proposal.proposal_id;
    assert!(matches!(
        fx.store
            .authorize_publication(id, &e.digest, &other_policy, e.suite.digest()),
        Err(GateError::Tampered(_))
    ));
    assert!(matches!(
        fx.store
            .authorize_publication(id, &e.digest, &policy(), "sha256:not-the-suite"),
        Err(GateError::SuiteChanged(_))
    ));
    assert!(matches!(
        fx.store
            .authorize_publication(id, &"f".repeat(64), &policy(), e.suite.digest()),
        Err(GateError::Tampered(_))
    ));
    assert_eq!(status(&fx, &e.proposal), ProposalStatus::Eligible);
    assert!(record(&fx, &e).authorization.is_none());
}

#[tokio::test]
async fn a_proposal_that_is_not_eligible_cannot_be_authorized_or_published() {
    let fx = fixture();
    let e = eligible(&fx).await;
    assert!(matches!(
        publish(&fx, &e),
        Err(GateError::NotAuthorized { status: "eligible", .. })
    ));
    fx.store
        .cancel_proposal(&e.proposal.proposal_id, "operator")
        .expect("test: cancel");
    assert!(matches!(authorize(&fx, &e), Err(GateError::NotAuthorized { .. })));
}

#[tokio::test]
async fn an_expired_eligible_proposal_becomes_stale_and_is_not_authorized() {
    let fx = fixture();
    let e = eligible(&fx).await;
    let expired = rewrite_gate_for_test(&fx.store, &e.gate_id, |d| {
        d.decided_at = Utc::now() - chrono::Duration::hours(2);
        d.expires_at = d.decided_at + chrono::Duration::seconds(3600);
    });
    let result = fx
        .store
        .authorize_publication(&e.proposal.proposal_id, &expired, &policy(), e.suite.digest());
    assert!(matches!(result, Err(GateError::Expired(_))), "{result:?}");
    assert_eq!(status(&fx, &e.proposal), ProposalStatus::Stale);
    assert_eq!(generation(&fx), 1);
}

#[tokio::test]
async fn new_failing_evidence_refuses_authorization_and_publication_without_state_change() {
    let fx = fixture();
    let e = eligible(&fx).await;
    evaluate(&fx, &e.suite, &tie()).await;
    assert!(matches!(authorize(&fx, &e), Err(GateError::NewFailingEvidence(_))));
    assert_eq!(status(&fx, &e.proposal), ProposalStatus::Eligible);

    // Same after authorization: publication re-verifies and refuses.
    let fx = fixture();
    let e = eligible(&fx).await;
    authorize(&fx, &e).expect("test: authorize");
    evaluate(&fx, &e.suite, &tie()).await;
    assert!(matches!(publish(&fx, &e), Err(GateError::NewFailingEvidence(_))));
    assert_eq!(status(&fx, &e.proposal), ProposalStatus::PublicationAuthorized);
    assert_eq!(generation(&fx), 1);
    assert!(
        fx.store
            .journal_entry(&tx_of(&record(&fx, &e)))
            .expect("test: journal")
            .is_none()
    );
}

fn tx_of(proposal: &SkillProposal) -> crate::skills::lifecycle::TransactionId {
    let attempt = proposal.authorization.as_ref().map_or(1, |a| a.attempt);
    let digest = proposal.proposal_id.trim_start_matches("prop-");
    crate::skills::lifecycle::TransactionId::parse(&format!("pub-{digest}-{attempt}")).expect("test: tx")
}

#[tokio::test]
async fn resuming_an_eligible_or_authorized_proposal_with_new_evidence_is_new_failing_evidence() {
    for authorize_first in [false, true] {
        let fx = fixture();
        let suite = validation_suite(2);
        let (proposal, pin) = evaluated(&fx, &suite, &all_better()).await;
        let outcome = gate(&fx, &proposal, &suite, std::slice::from_ref(&pin), &policy(), None)
            .await
            .expect("test: gate");
        if authorize_first {
            fx.store
                .authorize_publication(&proposal.proposal_id, &outcome.gate.digest, &policy(), suite.digest())
                .expect("test: authorize");
        }
        evaluate(&fx, &suite, &tie()).await;
        let resumed = gate(&fx, &proposal, &suite, &[pin], &policy(), None).await;
        assert!(matches!(resumed, Err(GateError::NewFailingEvidence(_))), "{resumed:?}");
        let expected = if authorize_first {
            ProposalStatus::PublicationAuthorized
        } else {
            ProposalStatus::Eligible
        };
        assert_eq!(status(&fx, &proposal), expected);
    }
}

#[tokio::test]
async fn publication_commits_once_records_the_generation_and_is_idempotent() {
    let fx = fixture();
    let e = eligible(&fx).await;
    authorize(&fx, &e).expect("test: authorize");
    let done = publish(&fx, &e).expect("test: publish");
    assert!(!done.transaction.replayed);
    assert_eq!(done.proposal.status, ProposalStatus::Committed);
    let published = done.proposal.publication.clone().expect("test: publication record");
    assert_eq!(published.generation, 2);
    assert_eq!(published.revision_id, fx.candidate);
    assert_eq!(
        published.transaction_id,
        done.transaction.entry.transaction_id.to_string()
    );
    assert_eq!(generation(&fx), 2);
    assert_eq!(active_revision(&fx), fx.candidate);
    // Retry returns the same publication without a new generation.
    let again = publish(&fx, &e).expect("test: replay");
    assert!(again.transaction.replayed);
    assert_eq!(again.proposal.publication, Some(published));
    assert_eq!(generation(&fx), 2);
}

#[tokio::test]
async fn a_stale_writer_is_rejected_when_the_generation_moved_since_authorization() {
    let fx = fixture();
    let e = eligible(&fx).await;
    authorize(&fx, &e).expect("test: authorize");
    // Another writer publishes an unrelated revision first.
    let other = super::evaluate::write_revision(&fx.store, "ANSWER=someone-else");
    activate(&fx.store, &skill(), &other, 1, true);
    let result = publish(&fx, &e);
    assert!(matches!(result, Err(GateError::Stale(_))), "{result:?}");
    assert_eq!(status(&fx, &e.proposal), ProposalStatus::Stale);
    assert_eq!(generation(&fx), 2);
    assert_eq!(active_revision(&fx), other.0);
}

/// Crash at `point`, check the invariants of the half-way state, then resume.
async fn crash_then_resume(point: &'static str, explicit_recovery: bool) {
    let fx = fixture();
    let e = eligible(&fx).await;
    authorize(&fx, &e).expect("test: authorize");
    crash_hooks::arm(point);
    let crashed = publish(&fx, &e);
    assert!(crashed.is_err(), "{point}: {crashed:?}");
    crash_hooks::disarm();
    // The proposal never moved past authorized, the binding is old or new but
    // never torn.
    assert_eq!(
        status(&fx, &e.proposal),
        ProposalStatus::PublicationAuthorized,
        "{point}"
    );
    let swapped = matches!(point, "after_binding" | "publish_before_finalize");
    assert_eq!(generation(&fx), if swapped { 2 } else { 1 }, "{point}");
    if explicit_recovery {
        fx.store.recover(Duration::ZERO).expect("test: recover");
    }
    let mut result = publish(&fx, &e);
    if matches!(point, "after_prepared" | "binding_staged") {
        // The PREPARED attempt never swapped: it aborts, never re-runs, and the
        // next attempt uses a new transaction id.
        assert!(
            matches!(result, Err(GateError::PublicationAborted { .. })),
            "{point}: {result:?}"
        );
        assert_eq!(generation(&fx), 1, "{point}");
        assert_eq!(status(&fx, &e.proposal), ProposalStatus::PublicationAuthorized);
        assert_eq!(record(&fx, &e).authorization.map(|a| a.attempt), Some(2));
        result = publish(&fx, &e);
    }
    let done = result.unwrap_or_else(|err| panic!("{point}: resume failed: {err}"));
    assert_eq!(done.proposal.status, ProposalStatus::Committed, "{point}");
    assert_eq!(done.transaction.entry.state, TransactionState::Committed);
    assert_eq!(generation(&fx), 2, "{point}");
    assert_eq!(active_revision(&fx), fx.candidate);
    // Exactly one committed publication of this candidate.
    let committed = fx
        .store
        .journal_entries_for(&skill())
        .expect("test: journal")
        .into_iter()
        .filter(|entry| entry.state == TransactionState::Committed && entry.candidate_revision_id == fx.candidate)
        .count();
    assert_eq!(committed, 1, "{point}");
}

#[tokio::test]
async fn crash_before_the_store_call_resumes_with_the_same_transaction() {
    crash_then_resume("publish_before_store", false).await;
}

#[tokio::test]
async fn crash_after_prepared_aborts_that_attempt_and_resumes_with_a_new_one() {
    crash_then_resume("after_prepared", false).await;
    crash_then_resume("after_prepared", true).await;
}

#[tokio::test]
async fn crash_with_a_staged_binding_keeps_the_old_binding_and_resumes() {
    crash_then_resume("binding_staged", false).await;
}

#[tokio::test]
async fn crash_after_the_binding_swap_is_detected_as_published_on_resume() {
    crash_then_resume("after_binding", false).await;
    crash_then_resume("after_binding", true).await;
}

#[tokio::test]
async fn crash_after_the_journal_commit_finishes_the_proposal_on_resume() {
    crash_then_resume("publish_before_finalize", false).await;
}

#[tokio::test]
async fn crashes_around_authorization_leave_a_consistent_proposal() {
    // Before the record is written: still Eligible, authorizing again works.
    let fx = fixture();
    let e = eligible(&fx).await;
    crash_hooks::arm("authorize_verified");
    assert!(authorize(&fx, &e).is_err());
    crash_hooks::disarm();
    assert_eq!(status(&fx, &e.proposal), ProposalStatus::Eligible);
    authorize(&fx, &e).expect("test: authorize after crash");
    // After the record is written: authorized, the retry is idempotent.
    let fx = fixture();
    let e = eligible(&fx).await;
    crash_hooks::arm("authorize_written");
    assert!(authorize(&fx, &e).is_err());
    crash_hooks::disarm();
    assert_eq!(status(&fx, &e.proposal), ProposalStatus::PublicationAuthorized);
    let again = authorize(&fx, &e).expect("test: idempotent authorize");
    assert_eq!(again.status, ProposalStatus::PublicationAuthorized);
    assert_eq!(
        publish(&fx, &e).expect("test: publish").proposal.status,
        ProposalStatus::Committed
    );
}

#[tokio::test]
async fn repeatedly_aborted_attempts_fail_the_proposal_and_leave_the_binding_alone() {
    let fx = fixture();
    let e = eligible(&fx).await;
    authorize(&fx, &e).expect("test: authorize");
    for attempt in 1..=MAX_PUBLICATION_ATTEMPTS {
        crash_hooks::arm("after_prepared");
        assert!(publish(&fx, &e).is_err());
        crash_hooks::disarm();
        let aborted = publish(&fx, &e);
        assert!(
            matches!(aborted, Err(GateError::PublicationAborted { .. })),
            "{attempt}: {aborted:?}"
        );
    }
    let failed = record(&fx, &e);
    assert_eq!(failed.status, ProposalStatus::Failed);
    assert_eq!(
        failed.failure.map(|f| f.kind),
        Some(ProposalFailureKind::PublicationAborted)
    );
    assert_eq!(generation(&fx), 1);
    assert_eq!(active_revision(&fx), fx.baseline);
    assert!(matches!(publish(&fx, &e), Err(GateError::NotAuthorized { .. })));
}

#[tokio::test]
async fn failed_publication_and_rollback_keep_in_flight_pins_and_the_previous_binding() {
    let fx = fixture();
    let who = fx.store.owner_scope().clone();
    let e = eligible(&fx).await;
    authorize(&fx, &e).expect("test: authorize");
    let issue = || fx.store.pin_active(&skill());
    let held = task_pin(&who, "sl08-pin-held", &skill(), issue)
        .expect("test: pin")
        .expect("test: pinned");
    assert_eq!(held.revision_id, fx.baseline);

    // A failed publication changes neither the binding nor the pin.
    crash_hooks::arm("after_prepared");
    assert!(publish(&fx, &e).is_err());
    crash_hooks::disarm();
    assert_eq!(active_revision(&fx), fx.baseline);
    let still = task_pin(&who, "sl08-pin-held", &skill(), issue)
        .expect("test: pin")
        .expect("test: pinned");
    assert_eq!(still.revision_id, fx.baseline);

    // After the successful retry the in-flight task still reads the baseline
    // revision, verified against the digest it was pinned with; a new task gets
    // the candidate.
    assert!(matches!(publish(&fx, &e), Err(GateError::PublicationAborted { .. })));
    publish(&fx, &e).expect("test: publish");
    assert_eq!(active_revision(&fx), fx.candidate);
    let kept = task_pin(&who, "sl08-pin-held", &skill(), issue)
        .expect("test: pin")
        .expect("test: pinned");
    assert_eq!(kept.revision_id, fx.baseline);
    fx.store
        .read_revision(&skill(), &kept.revision_id, Some(&kept.record_digest))
        .expect("test: pinned revision still verifies");
    let fresh = task_pin(&who, "sl08-pin-new", &skill(), issue)
        .expect("test: pin")
        .expect("test: pinned");
    assert_eq!(fresh.revision_id, fx.candidate);

    // Rolling back to the baseline moves the binding under a new generation and
    // leaves both pinned tasks on the revisions they hold.
    let request = crate::skills::lifecycle::RollbackRequest {
        transaction_id: crate::skills::lifecycle::TransactionId::parse("sl08-rollback").expect("test: tx"),
        skill_id: skill(),
        target: fx.baseline.clone(),
        expected_generation: 2,
    };
    fx.store.rollback(&request).expect("test: rollback");
    assert_eq!(generation(&fx), 3);
    assert_eq!(active_revision(&fx), fx.baseline);
    for (task, revision) in [("sl08-pin-held", &fx.baseline), ("sl08-pin-new", &fx.candidate)] {
        let pin = task_pin(&who, task, &skill(), issue)
            .expect("test: pin")
            .expect("test: pinned");
        assert_eq!(&pin.revision_id, revision, "{task}");
        fx.store
            .read_revision(&skill(), &pin.revision_id, Some(&pin.record_digest))
            .expect("test: pinned revision still verifies");
    }
    release_task_pins(&who, "sl08-pin-held");
    release_task_pins(&who, "sl08-pin-new");
}

#[tokio::test]
async fn a_tampered_authorization_record_cannot_publish_another_digest() {
    let fx = fixture();
    let e = eligible(&fx).await;
    authorize(&fx, &e).expect("test: authorize");
    edit_proposal(&fx, &e.proposal.proposal_id, |p| {
        if let Some(auth) = p.authorization.as_mut() {
            auth.candidate_record_digest = "0".repeat(64);
        }
    });
    let result = publish(&fx, &e);
    assert!(
        matches!(result, Err(GateError::Lifecycle(LifecycleError::Tampered { .. }))),
        "{result:?}"
    );
    assert_eq!(generation(&fx), 1);
}
