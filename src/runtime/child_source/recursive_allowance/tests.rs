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

#[test]
fn recursive_allowance_exhausted_refuses_spawn_without_decision_service() -> anyhow::Result<()> {
    use crate::causal_tree::branch::{
        CapabilityId, CapabilityRef, CapabilityTarget, CatalogGeneration, SchemaFingerprint,
    };
    use crate::tools::capability::child::{AuthorityEnvelope, ChildCapabilityManifest};
    let generation = CatalogGeneration([7; 32]);
    let allowed = ["prx_read", "sessions_spawn"]
        .into_iter()
        .map(|name| {
            Ok(CapabilityRef {
                id: CapabilityId::new(CapabilityTarget::NativeTool, "native", name)?,
                fingerprint: SchemaFingerprint([1; 32]),
                generation,
            })
        })
        .collect::<anyhow::Result<std::collections::BTreeSet<_>>>()?;
    let parent = crate::runtime::child_source::ParentAuthority::for_run(
        "prx_run",
        allowed,
        AuthorityEnvelope {
            owner: "prx_user".into(),
            workspace: "prx_workspace".into(),
            level: crate::security::AutonomyLevel::Supervised,
            max_cost_cents: 10,
            remaining_depth: 2,
            delegation: None,
        },
        generation,
        crate::decision::key::DecisionKey::ephemeral(),
        Arc::new(RecursiveAllowance::new(100_000)),
    );
    let child = ChildCapabilityManifest::derive_strict_subset(&parent.constraints, &[])?;
    child.verify(&parent.constraints)?;
    let (cents, child_allowance) = parent.allowance.carve_child(child.authority.max_cost_cents)?;
    assert_eq!(cents, 5);
    assert_eq!(child_allowance.money().load(Ordering::Acquire), 50_000);
    assert!(parent.allowance.ensure_available().is_err());
    let retained = Arc::clone(&parent.allowance);
    let refusal = retained
        .carve_child(child.authority.max_cost_cents)
        .err()
        .expect("test: exhausted recursive authority must refuse another child");
    assert!(
        refusal
            .to_string()
            .contains("already allocated its one child allowance")
    );
    assert_eq!(parent.allowance.money().load(Ordering::Acquire), 50_000);
    assert!(RecursiveAllowance::new(0).carve_child(10).is_err());
    Ok(())
}
