//! Run ownership survives a pause, and every nonpending lifetime retires.
#![cfg(test)]
use super::*;
use crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input;

#[tokio::test]
async fn unused_admitted_allowance_survives_pause_and_is_retired_at_expiry() {
    let workspace = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.default_provider = Some("ollama".into());
    config.default_model = Some("prx-local-model".into());
    let generation = crate::config::new_shared(config).pin();
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
    let proof = test_recorded_cli_agent_turn_proof_with_input(&generation, memory, "fixture", "prx_original").await;
    let registry = Arc::new(Registry::default());
    let runs = RunAllowances(OnceLock::from(Arc::clone(&registry)));
    tokio::time::pause();
    let lease = runs.issue(&proof, Arc::new(AtomicU64::new(100_000)));
    let owner = Arc::clone(&lease);
    owner.retain_pending(chrono::Utc::now() + chrono::Duration::seconds(60));
    drop(lease);
    assert_eq!(registry.lock().len(), 1);
    let resumed = runs.inherit_resume(&proof, proof.run_id());
    assert!(Arc::ptr_eq(&owner, &resumed));
    owner.ensure_available().unwrap();
    assert!(owner.carve_child(10).is_ok());
    owner.retain_pending(chrono::Utc::now() + chrono::Duration::seconds(60));
    registry.lock().insert(
        RunAllowances::key_for_run(&proof, "prx_resume_alias"),
        Arc::clone(&owner),
    );
    assert_eq!(registry.lock().len(), 2);
    let expiry = (*owner.pending_until.lock()).expect("test: pending deadline");
    drop(resumed);
    // std::Instant advances during setup while Tokio's test clock is paused.
    // Waiting beyond the actual deadline also lets both expiry tasks finish.
    tokio::time::sleep_until((expiry + std::time::Duration::from_secs(1)).into()).await;
    assert!(registry.lock().is_empty());
}

#[tokio::test]
async fn context_drop_and_denied_pending_remove_every_run_alias() {
    let workspace = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.default_provider = Some("ollama".into());
    config.default_model = Some("prx-local-model".into());
    let generation = crate::config::new_shared(config).pin();
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).unwrap());
    let proof = test_recorded_cli_agent_turn_proof_with_input(&generation, memory, "fixture", "prx_original").await;
    let registry = Arc::new(Registry::default());
    let runs = RunAllowances(OnceLock::from(Arc::clone(&registry)));
    let first = runs.issue(&proof, Arc::new(AtomicU64::new(100_000)));
    let owner = Arc::clone(&first);
    let second = ContextAllowance::new(Arc::clone(&owner));
    drop(first);
    assert_eq!(registry.lock().len(), 1);
    registry
        .lock()
        .insert(RunAllowances::key_for_run(&proof, "prx_drop_alias"), Arc::clone(&owner));
    assert_eq!(registry.lock().len(), 2);
    drop(second);
    assert!(registry.lock().is_empty());
    let owner = runs.issue(&proof, Arc::new(AtomicU64::new(100_000)));
    owner.retain_pending(chrono::Utc::now() + chrono::Duration::minutes(5));
    registry.lock().insert(
        RunAllowances::key_for_run(&proof, "prx_denied_alias"),
        Arc::clone(&owner),
    );
    assert_eq!(registry.lock().len(), 2);
    runs.retire_pending(
        proof.workspace_id(),
        proof.owner_id(),
        proof.session_key(),
        proof.run_id(),
    );
    assert!(registry.lock().is_empty());
}
