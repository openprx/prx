//! Canonical-state tests of the exact-call observations (moved from
//! `state/exact_calls.rs` with the neutral turn scope, R15).
use crate::tools::capability::turn_state::exact_calls::*;
use std::sync::Arc;

#[test]
fn absent_observations_keep_legacy_canonical_and_present_facts_bind_the_fingerprint() {
    let key = crate::decision_engine::state::fixtures::key();
    let mut state = crate::decision_engine::state::fixtures::state(&key, "prx observation");
    let original = state.canonical().expect("test: canonical state serializes");
    state.observed = state.observed.clone().with_exact_call_observations(None);
    assert_eq!(state.canonical().expect("test: canonical state serializes"), original);
    assert!(!original.contains("exact_call_observations"));
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
    let before = state
        .material_fingerprint(&key)
        .expect("test: canonical state serializes");
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
        before
    );
}
