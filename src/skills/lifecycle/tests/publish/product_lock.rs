//! Original publisher lock/precondition and factual recovery contract.
//! These owner-mechanism fixtures do not manufacture a product GateReview.
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn refusal() -> LifecycleError {
    LifecycleError::ProposalRefused("publication precondition revoked".into())
}

#[test]
fn fresh_publication_rechecks_revocation_after_waiting_for_the_actual_skill_lock() {
    let (_tmp, store) = setup();
    let store = Arc::new(store);
    let revision = write(&store, "candidate");
    let request = request("pub-wait-revoked", &revision, 0);
    let skill_lock = store
        .lock_skill(&skill(), Duration::from_secs(5))
        .expect("actual skill owner");
    let revoked = Arc::new(AtomicBool::new(false));
    let called = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let worker = {
        let store = store.clone();
        let request = request.clone();
        let revoked = revoked.clone();
        let called = called.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let owner = store.lock_proposals().expect("original proposal owner");
            barrier.wait();
            store.publish_pinned_from_proposal(&owner, &request, &"a".repeat(64), &|| {
                called.fetch_add(1, Ordering::SeqCst);
                if revoked.load(Ordering::SeqCst) {
                    Err(refusal())
                } else {
                    Ok(())
                }
            })
        })
    };
    barrier.wait();
    assert_eq!(
        called.load(Ordering::SeqCst),
        0,
        "precondition cannot precede the held target lock"
    );
    revoked.store(true, Ordering::SeqCst);
    drop(skill_lock);
    assert!(matches!(
        worker.join().expect("publisher joins"),
        Err(LifecycleError::ProposalRefused(_))
    ));
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert!(binding(&store).is_none());
    assert!(store.journal_entry(&request.transaction_id).expect("journal").is_none());
}

#[test]
fn second_precondition_failure_retains_prepared_then_original_reconciliation_aborts_without_swap() {
    let (_tmp, store) = setup();
    let predecessor = write(&store, "predecessor");
    store
        .publish(&request("original-baseline", &predecessor, 0))
        .expect("baseline");
    let candidate = write(&store, "candidate");
    let request = request("pub-second-check", &candidate, 1);
    let before = binding(&store).expect("baseline binding");
    let checks = AtomicUsize::new(0);
    let owner = store.lock_proposals().expect("original proposal owner");
    let result = store.publish_pinned_from_proposal(&owner, &request, &"b".repeat(64), &|| {
        if checks.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(())
        } else {
            Err(refusal())
        }
    });
    assert!(matches!(result, Err(LifecycleError::ProposalRefused(_))));
    assert_eq!(checks.load(Ordering::SeqCst), 2);
    assert_eq!(binding(&store).expect("unchanged binding"), before);
    assert_eq!(
        store
            .journal_entry(&request.transaction_id)
            .expect("journal")
            .expect("real prepared")
            .state,
        TransactionState::Prepared
    );
    assert!(
        store
            .reconcile_existing_publication(&owner, &request, &"b".repeat(64))
            .is_err()
    );
    assert_eq!(
        store
            .journal_entry(&request.transaction_id)
            .expect("journal")
            .expect("settled fact")
            .state,
        TransactionState::Aborted
    );
    assert_eq!(binding(&store).expect("never swapped"), before);
}

#[test]
fn existing_only_missing_transaction_does_not_create_prepared_or_active_effect() {
    let (_tmp, store) = setup();
    let candidate = write(&store, "candidate");
    let request = request("pub-no-existing-attempt", &candidate, 0);
    let owner = store.lock_proposals().expect("original owner");
    assert!(
        store
            .reconcile_existing_publication(&owner, &request, &"c".repeat(64))
            .expect("read existing only")
            .is_none()
    );
    assert!(store.journal_entry(&request.transaction_id).expect("journal").is_none());
    assert!(binding(&store).is_none());
}

#[test]
fn committed_replay_after_rollback_returns_original_transaction_without_rechecking_or_swapping() {
    let (_tmp, store) = setup();
    let predecessor = write(&store, "predecessor");
    store
        .publish(&request("original-baseline", &predecessor, 0))
        .expect("baseline");
    let candidate = write(&store, "candidate");
    let request = request("pub-historical-replay", &candidate, 1);
    let committed = {
        let owner = store.lock_proposals().expect("original owner");
        store
            .publish_pinned_from_proposal(&owner, &request, &"d".repeat(64), &|| Ok(()))
            .expect("actual committed")
    };
    store
        .rollback(&rollback_request("rollback-to-baseline", &predecessor.0, 2))
        .expect("actual rollback");
    let rolled_back = binding(&store).expect("rollback binding");
    let checks = AtomicUsize::new(0);
    let owner = store.lock_proposals().expect("original owner");
    let replay = store
        .publish_pinned_from_proposal(&owner, &request, &"d".repeat(64), &|| {
            checks.fetch_add(1, Ordering::SeqCst);
            Err(refusal())
        })
        .expect("historical committed result");
    assert!(replay.replayed);
    assert_eq!(replay.entry, committed.entry);
    assert_eq!(checks.load(Ordering::SeqCst), 0);
    assert_eq!(binding(&store).expect("rollback preserved"), rolled_back);
    assert_eq!(rolled_back.revision_id, predecessor.0);
}
