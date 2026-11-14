use super::*;
use crate::runtime::h2_entry::{EntryAction, EntryInvocation, EntryKind, profile::tests::Fixture};

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
async fn waiting_review(fixture: &Fixture) -> (String, String) {
    for _ in 0..500 {
        if let Some((id, pending)) = fixture.profile.reviews.lock().iter().next() {
            return (id.clone(), pending.call_digest.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("test: the lifecycle call never asked the local owner");
}

/// Performs the admitted call the way the backend does: one consume per run.
fn perform(invocation: &EntryInvocation, action: EntryAction, performed: &mut usize) {
    if invocation.consume(action, &"a".repeat(64)).is_ok() {
        *performed += 1;
    }
}

async fn approved_performs_once_and_denied_never_performs(action: EntryAction) {
    let fixture = Fixture::new(EntryKind::Gateway);
    fixture.profile.own_reader(std::thread::spawn(|| ())).unwrap();
    let authority = LocalControlAuthority::from_producer(fixture.profile.clone()).unwrap();

    // Approved: the exact call is admitted and performed exactly once.
    let admitted = tokio::spawn(EntryInvocation::admit_reviewed(
        fixture.origin(),
        lifecycle_call(&fixture, action),
    ));
    let (id, digest) = waiting_review(&fixture).await;
    assert!(authority.review_command(&["approve", &id, &"c".repeat(64)]).is_err());
    authority.review_command(&["approve", &id, &digest]).unwrap();
    let invocation = admitted.await.unwrap().unwrap();
    let mut performed = 0;
    perform(&invocation, action, &mut performed);
    perform(&invocation, action, &mut performed);
    assert_eq!(performed, 1);
    assert!(fixture.profile.reviews.lock().is_empty());
    assert!(authority.review_command(&["approve", &id, &digest]).is_err());

    // Denied: nothing is admitted, so nothing can be performed.
    let refused = tokio::spawn(EntryInvocation::admit_reviewed(
        fixture.origin(),
        lifecycle_call(&fixture, action),
    ));
    let (id, _) = waiting_review(&fixture).await;
    authority.review_command(&["deny", &id]).unwrap();
    assert!(refused.await.unwrap().is_err());
    assert!(fixture.profile.reviews.lock().is_empty());

    // Without a review the same Gateway mutation never admits.
    assert!(EntryInvocation::admit(fixture.origin(), lifecycle_call(&fixture, action), None).is_err());
}

#[tokio::test]
async fn local_owner_review_gates_publish() {
    approved_performs_once_and_denied_never_performs(EntryAction::Publish).await;
}
#[tokio::test]
async fn local_owner_review_gates_annotate() {
    approved_performs_once_and_denied_never_performs(EntryAction::Annotate).await;
}
#[tokio::test]
async fn local_owner_review_gates_rollback() {
    approved_performs_once_and_denied_never_performs(EntryAction::Rollback).await;
}
#[tokio::test]
async fn local_owner_review_gates_retire() {
    approved_performs_once_and_denied_never_performs(EntryAction::Retire).await;
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
