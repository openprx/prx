//! Canonical-state tests of the exact-call observations (moved from
//! `state/exact_calls.rs` with the neutral turn scope, R15).
use crate::tools::capability::turn_state::exact_calls::*;
use std::sync::Arc;

#[test]
fn absent_observations_keep_legacy_canonical_and_present_facts_bind_both_fingerprints() {
    let key = crate::causal_tree::state::fixtures::key();
    let mut state = crate::causal_tree::state::fixtures::state(&key, "prx observation");
    let original = [
        state.canonical(false).expect("test: canonical state serializes"),
        state.canonical(true).expect("test: canonical state serializes"),
    ];
    state.observed = state.observed.clone().with_exact_call_observations(None);
    assert_eq!(
        [
            state.canonical(false).expect("test: canonical state serializes"),
            state.canonical(true).expect("test: canonical state serializes")
        ],
        original
    );
    for value in &original {
        assert!(!value.contains("exact_call_observations"));
    }
    let facts = ExactCallObservations {
        version: 1,
        incomplete: false,
        calls: vec![ExactCallObservation {
            fingerprint: [9; 32],
            ledger: LedgerDisposition::Reserved,
            reservation_attempt: Some(0),
            backend_started: true,
            completion: BackendCompletion::Failed,
        }],
    };
    state.observed = state
        .observed
        .clone()
        .with_exact_call_observations(Some(Arc::new(facts.clone())));
    let before = [
        state
            .material_fingerprint(&key)
            .expect("test: canonical state serializes"),
        state
            .record_fingerprint(&key)
            .expect("test: canonical state serializes"),
    ];
    let mut changed = facts;
    changed.calls.first_mut().expect("actual fact").fingerprint = [8; 32];
    state.observed = state
        .observed
        .clone()
        .with_exact_call_observations(Some(Arc::new(changed)));
    assert_ne!(
        state
            .material_fingerprint(&key)
            .expect("test: canonical state serializes"),
        before.first().expect("material").as_str()
    );
    assert_ne!(
        state
            .record_fingerprint(&key)
            .expect("test: canonical state serializes"),
        before.get(1).expect("record").as_str()
    );
}
