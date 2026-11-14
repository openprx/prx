use super::*;
use crate::runtime::h2_entry::test_support::ReviewedGateway;
use crate::runtime::h2_entry::{EntryAction, EntryInvocation, EntryKind, profile::tests::Fixture};
use crate::skills::lifecycle::experiment::ExperimentId;
use crate::skills::lifecycle::{
    ExperimentAdoptionStatusRequest, ExperimentAnnotationRequest, ExperimentFinalExpectation, ExperimentProductError,
    ExperimentProductHandle, ExperimentPublishRequest, ExperimentRollbackRequest, LessonRef, RevisionId, SkillId,
};

fn lifecycle_call(fixture: &Fixture, action: EntryAction) -> EntryCall {
    let origin = fixture.origin();
    let source = origin.record();
    EntryCall {
        action,
        tool: "skills_manage".into(),
        args_sha256: "a".repeat(64),
        call_digest: "b".repeat(64),
        capability: "prx-capability".into(),
        capability_generation: hex::encode([2_u8; 32]),
        catalog_generation: hex::encode([3_u8; 32]),
        actor: source.actor.clone(),
        workspace: source.workspace.clone(),
        session: source.session.clone(),
        run: source.run.clone(),
        event: source.event.clone(),
    }
}

/// Waits until the executing call has posted its review to the local owner.
async fn waiting_review(fixture: &Fixture) -> String {
    for _ in 0..500 {
        if let Some(id) = fixture.profile.reviews.lock().keys().next() {
            return id.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("test: the lifecycle call never asked the local owner");
}

fn revision(digit: char) -> RevisionId {
    RevisionId::parse(&format!("r1-{}", digit.to_string().repeat(64))).unwrap()
}

/// A well-formed typed request of `action`, naming an experiment the
/// temporary lifecycle repository does not hold.
fn request(action: EntryAction) -> serde_json::Value {
    let handle = ExperimentProductHandle {
        experiment_id: ExperimentId::parse("prx-wiring-experiment").unwrap(),
        plan_sha256: "a".repeat(64),
    };
    let expected = ExperimentFinalExpectation {
        handle: handle.clone(),
        proposal_id: "prx-wiring-proposal".into(),
        proposal_stage: "gate-review".into(),
        skill_id: SkillId::parse("prx-skill").unwrap(),
        baseline_revision: revision('1'),
        baseline_record_sha256: "2".repeat(64),
        baseline_generation: 1,
        candidate_revision: revision('3'),
        candidate_record_sha256: "4".repeat(64),
        evaluation_epoch: 1,
        evaluating_since: chrono::Utc::now(),
        suite_sha256: "5".repeat(64),
        gate_stage: "gate-review".into(),
        gate_witness_sha256: "6".repeat(64),
        gate_id: "prx-wiring-gate".into(),
        gate_sha256: "7".repeat(64),
    };
    match action {
        EntryAction::Publish => serde_json::to_value(ExperimentPublishRequest { expected }),
        EntryAction::Annotate => serde_json::to_value(ExperimentAnnotationRequest {
            expected,
            publication_tx: "prx-wiring-tx".into(),
            task_family: "deploy".into(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        }),
        EntryAction::Rollback => serde_json::to_value(ExperimentRollbackRequest {
            handle,
            prior_revision: revision('1'),
            observed_generation: 1,
        }),
        EntryAction::Retire => serde_json::to_value(ExperimentAdoptionStatusRequest {
            handle,
            reference: LessonRef {
                adoption_id: "prx-wiring-adoption".into(),
                adoption_sha256: "8".repeat(64),
                publication_tx: "prx-wiring-tx".into(),
                adopted_revision: revision('3'),
                adopted_record_sha256: "9".repeat(64),
            },
            retired: true,
            invalidated: false,
        }),
        other => panic!("test: {other:?} is not a reviewed lifecycle action"),
    }
    .unwrap()
}

fn action_name(action: EntryAction) -> &'static str {
    match action {
        EntryAction::Publish => "lifecycle_experiment_publish",
        EntryAction::Annotate => "lifecycle_experiment_annotate",
        EntryAction::Rollback => "lifecycle_experiment_rollback",
        EntryAction::Retire => "lifecycle_experiment_retire",
        other => panic!("test: {other:?} is not a reviewed lifecycle action"),
    }
}

/// G3 + audit r1 F5 (wiring): each reviewed lifecycle action is dispatched
/// through production `skills_manage` execution against a temporary
/// lifecycle repository. Approved: the call passes consume, verify and the
/// exact grant and reaches the lifecycle service (its typed refusal, as the
/// named experiment does not exist). Denied: nothing is admitted, and without
/// an admitted invocation the backend refuses before the service. Repeated:
/// the consumed invocation refuses at consume, before the service.
async fn reviewed_action_reaches_the_service_once(action: EntryAction) {
    let fixture = Fixture::new(EntryKind::Gateway);
    let gateway = ReviewedGateway::over(&fixture.config);
    let service =
        crate::skills::lifecycle_runtime::open_service(&fixture.config.workspace_dir, &fixture.config.skills_lifecycle)
            .unwrap()
            .unwrap();
    let tool = gateway.tool(&fixture.config, service);
    let args = gateway.granted_args(action_name(action), request(action));

    // Approved: reaches the lifecycle service exactly once.
    let invocation = gateway.admit(&args, true).await.unwrap();
    assert!(gateway.no_pending_review());
    let first = gateway
        .dispatch(Some(Arc::clone(&invocation)), &tool, args.clone())
        .await
        .expect_err("test: the service refuses the unknown experiment");
    assert!(
        first.downcast_ref::<ExperimentProductError>().is_some(),
        "{action:?}: the approved call reaches the lifecycle service: {first:#}"
    );
    // Repeated: refused at consume, never reaching the service again.
    let repeated = gateway
        .dispatch(Some(invocation), &tool, args.clone())
        .await
        .expect_err("test: a consumed invocation never runs again");
    assert!(
        repeated.downcast_ref::<ExperimentProductError>().is_none()
            && format!("{repeated:#}").contains("already consumed"),
        "{action:?}: {repeated:#}"
    );

    // Denied: never admitted; without an invocation the backend refuses
    // before the service.
    assert!(gateway.admit(&args, false).await.is_err());
    assert!(gateway.no_pending_review());
    let unadmitted = gateway
        .dispatch(None, &tool, args)
        .await
        .expect_err("test: no admitted invocation");
    assert!(
        unadmitted.downcast_ref::<ExperimentProductError>().is_none(),
        "{action:?}: {unadmitted:#}"
    );

    // Without a review the same Gateway mutation never admits.
    assert!(EntryInvocation::admit(fixture.origin(), lifecycle_call(&fixture, action), None).is_err());
}

#[tokio::test]
async fn local_owner_review_gates_publish() {
    reviewed_action_reaches_the_service_once(EntryAction::Publish).await;
}
#[tokio::test]
async fn local_owner_review_gates_annotate() {
    reviewed_action_reaches_the_service_once(EntryAction::Annotate).await;
}
#[tokio::test]
async fn local_owner_review_gates_rollback() {
    reviewed_action_reaches_the_service_once(EntryAction::Rollback).await;
}
#[tokio::test]
async fn local_owner_review_gates_retire() {
    reviewed_action_reaches_the_service_once(EntryAction::Retire).await;
}
#[tokio::test]
async fn reader_exit_ends_waiting_review_without_performing() {
    let fixture = Fixture::new(EntryKind::Gateway);
    fixture.profile.own_reader(std::thread::spawn(|| ())).unwrap();
    let authority = LocalControlAuthority::from_producer(fixture.profile.clone()).unwrap();
    let waiting = tokio::spawn(EntryInvocation::admit_reviewed(
        fixture.origin(),
        lifecycle_call(&fixture, EntryAction::Publish),
    ));
    waiting_review(&fixture).await;
    authority.cancel_reviews();
    assert!(waiting.await.unwrap().is_err());
}

/// A waiting review answers only with the exact call digest, and a revoked
/// launch can neither approve a waiting call nor admit a new one.
#[tokio::test]
async fn revoked_launch_cannot_approve_or_admit() {
    let fixture = Fixture::new(EntryKind::Gateway);
    fixture.profile.own_reader(std::thread::spawn(|| ())).unwrap();
    let authority = LocalControlAuthority::from_producer(fixture.profile.clone()).unwrap();
    let waiting = tokio::spawn(EntryInvocation::admit_reviewed(
        fixture.origin(),
        lifecycle_call(&fixture, EntryAction::Retire),
    ));
    let id = waiting_review(&fixture).await;
    assert!(authority.review_command(&["approve", &id, &"c".repeat(64)]).is_err());
    fixture.profile.revoke();
    assert!(authority.review_command(&["approve", &id, &"b".repeat(64)]).is_err());
    assert!(
        EntryInvocation::admit_reviewed(fixture.origin(), lifecycle_call(&fixture, EntryAction::Retire))
            .await
            .is_err()
    );
    waiting.abort();
}
