//! Run-owned budgets retire with their contexts or explicit denial.
#![cfg(test)]
use super::*;
use crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input;

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
    use crate::tools::capability::child::{AuthorityEnvelope, ChildCapabilityManifest};
    use crate::tools::capability::contract::{
        CapabilityId, CapabilityRef, CapabilityTarget, CatalogGeneration, SchemaFingerprint,
    };
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
        crate::security::runtime_key::DecisionKey::ephemeral(),
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
