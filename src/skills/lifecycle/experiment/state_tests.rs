#![cfg(test)]
#![allow(clippy::indexing_slicing)]
use super::tests::specification;
use super::*;
use chrono::Duration;
fn event(stage: &str, kind: &str) -> ExperimentEvent {
    match kind {
        "reserve" => ExperimentEvent::Reserved { stage: stage.into() },
        "dispatch" => ExperimentEvent::Dispatched { stage: stage.into() },
        "unknown" => ExperimentEvent::DispatchUnknown { stage: stage.into() },
        "cleanup" => ExperimentEvent::CleanupConfirmed { stage: stage.into() },
        _ => panic!("unknown test operation"),
    }
}
#[test]
fn unknown_dispatch_keeps_allocation_and_cannot_repeat_or_unlock_dependency() {
    let plan = specification().freeze().unwrap();
    let now = plan.spec().created_at;
    let mut state = ExperimentState::new(&plan);
    for kind in ["reserve", "dispatch", "unknown"] {
        state.apply(&plan, &event("proposal", kind), now).unwrap();
    }
    assert_eq!(state.committed(&plan).unwrap(), plan.spec().resources);
    for kind in ["reserve", "dispatch", "cleanup"] {
        assert!(state.apply(&plan, &event("proposal", kind), now).is_err());
    }
    assert!(state.apply(&plan, &event("verify", "reserve"), now).is_err());
    assert!(!state.stages["proposal"].successful());
}
#[test]
fn settlement_is_disjoint_cleanup_is_required_and_cancel_keeps_late_usage() {
    let plan = specification().freeze().unwrap();
    let now = plan.spec().created_at;
    let mut state = ExperimentState::new(&plan);
    for kind in ["reserve", "dispatch"] {
        state.apply(&plan, &event("proposal", kind), now).unwrap();
    }
    state.apply(&plan, &ExperimentEvent::Cancelled, now).unwrap();
    let usage = ExperimentResources {
        calls: 1,
        tokens: 10,
        micro_usd: 1,
        ..Default::default()
    };
    state
        .apply(
            &plan,
            &ExperimentEvent::Settled {
                stage: "proposal".into(),
                usage,
                evidence_sha256: "e".repeat(64),
                reference: None,
                successful: true,
            },
            plan.spec().expires_at + Duration::seconds(1),
        )
        .unwrap();
    assert_eq!(state.committed(&plan).unwrap(), plan.spec().resources);
    assert!(!state.stages["proposal"].successful());
    state.apply(&plan, &event("proposal", "cleanup"), now).unwrap();
    assert_eq!(state.committed(&plan).unwrap(), usage);
    assert!(state.apply(&plan, &event("verify", "reserve"), now).is_err());
}
#[test]
fn overrun_is_recorded_honestly_and_never_qualifies() {
    let plan = specification().freeze().unwrap();
    let now = plan.spec().created_at;
    let mut state = ExperimentState::new(&plan);
    for kind in ["reserve", "dispatch"] {
        state.apply(&plan, &event("proposal", kind), now).unwrap();
    }
    let usage = ExperimentResources {
        tokens: plan.spec().resources.tokens + 1,
        ..Default::default()
    };
    state
        .apply(
            &plan,
            &ExperimentEvent::Settled {
                stage: "proposal".into(),
                usage,
                evidence_sha256: "e".repeat(64),
                reference: None,
                successful: true,
            },
            now,
        )
        .unwrap();
    state.apply(&plan, &event("proposal", "cleanup"), now).unwrap();
    assert!(!state.stages["proposal"].successful());
    assert!(!state.committed(&plan).unwrap().within(plan.spec().resources));
}
#[test]
fn journal_reopen_idempotency_cas_and_owner_pins() {
    let temp = tempfile::tempdir().unwrap();
    let plan = specification().freeze().unwrap();
    let now = plan.spec().created_at;
    let store =
        crate::skills::lifecycle::SkillLifecycleStore::open(temp.path(), plan.spec().owner_scope.clone()).unwrap();
    let journal = store.admit_experiment(&plan, now).unwrap();
    let old_head = journal.head_sha256;
    let reserve = event("proposal", "reserve");
    let journal = store
        .append_experiment(
            &plan.spec().id,
            plan.digest(),
            0,
            &old_head,
            "reserve-1",
            reserve.clone(),
            now,
        )
        .unwrap();
    assert_eq!(journal.next_sequence(), 1);
    let replay = store
        .append_experiment(&plan.spec().id, plan.digest(), 0, &old_head, "reserve-1", reserve, now)
        .unwrap();
    assert_eq!(replay.head_sha256, journal.head_sha256);
    assert!(
        store
            .append_experiment(
                &plan.spec().id,
                plan.digest(),
                0,
                &old_head,
                "reserve-2",
                event("proposal", "dispatch"),
                now
            )
            .is_err()
    );
    assert!(
        store
            .append_experiment(
                &plan.spec().id,
                plan.digest(),
                0,
                &old_head,
                "reserve-1",
                event("proposal", "dispatch"),
                now
            )
            .is_err()
    );
    drop(store);
    let store =
        crate::skills::lifecycle::SkillLifecycleStore::open_existing(temp.path(), plan.spec().owner_scope.clone())
            .unwrap()
            .unwrap();
    assert_eq!(
        store
            .read_experiment(&plan.spec().id, plan.digest())
            .unwrap()
            .head_sha256,
        journal.head_sha256
    );
    assert!(store.read_experiment(&plan.spec().id, &"f".repeat(64)).is_err());
    let mut different = plan.spec().clone();
    different.resources.calls += 1;
    assert!(store.admit_experiment(&different.freeze().unwrap(), now).is_err());
    let path = store
        .root()
        .join("experiments")
        .join(plan.spec().id.as_str())
        .join("0000.json");
    let mut entry: ExperimentEntry = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    entry.previous_sha256 = "f".repeat(64);
    std::fs::write(path, serde_json::to_vec(&entry).unwrap()).unwrap();
    assert!(store.read_experiment(&plan.spec().id, plan.digest()).is_err());
}
#[test]
fn competing_writers_do_not_duplicate_reservations() {
    let temp = tempfile::tempdir().unwrap();
    let plan = std::sync::Arc::new(specification().freeze().unwrap());
    let now = plan.spec().created_at;
    let store = std::sync::Arc::new(
        crate::skills::lifecycle::SkillLifecycleStore::open(temp.path(), plan.spec().owner_scope.clone()).unwrap(),
    );
    let head = store.admit_experiment(&plan, now).unwrap().head_sha256;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles = [0, 1].map(|index| {
        let (store, plan, head, barrier) = (store.clone(), plan.clone(), head.clone(), barrier.clone());
        std::thread::spawn(move || {
            barrier.wait();
            store.append_experiment(
                &plan.spec().id,
                plan.digest(),
                0,
                &head,
                &format!("writer-{index}"),
                event("proposal", "reserve"),
                now,
            )
        })
    });
    assert_eq!(
        handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap().ok())
            .count(),
        1
    );
    assert_eq!(
        store
            .read_experiment(&plan.spec().id, plan.digest())
            .unwrap()
            .entries
            .len(),
        1
    );
}
