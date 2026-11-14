//! Bind an authenticated operator worker to its actual execution service.
use std::collections::BTreeSet;
use std::sync::Arc;

use super::{PublicationBuildError, PublicationInputs, PublicationLoopRuntime};
use crate::decision_engine::prediction::factory::{DecisionBackendFactory, FactoryDeps};
use crate::runtime::turn_decision::{TurnDecisionService, pinned_policy_version};
use crate::tools::capability::catalog::CatalogScope;
use crate::tools::capability::contract::CapabilityRef;
use crate::tools::capability::turn_state::TurnSurfaceKind;

pub(super) async fn operator_process_publication(
    authority: Arc<crate::runtime::child_source::operator_process::OperatorProcessAuthority>,
    mut build: PublicationInputs<'_>,
    runtime: PublicationLoopRuntime,
) -> anyhow::Result<Arc<TurnDecisionService>> {
    authority.verify()?;
    let snapshots = authority.snapshots();
    anyhow::ensure!(
        Arc::ptr_eq(&build.generation, &snapshots.generation)
            && Arc::ptr_eq(&build.memory, &authority.source_fabric().memory())
            && build.catalog_sources.config_generation_id == snapshots.generation.id.0.to_string(),
        "operator worker service differs from source"
    );
    snapshots
        .publication
        .verify_policy(&build.security, &snapshots.parent, &snapshots.catalog)?;
    // The frozen capability set is a membership limit; Full has no permission
    // limits (R51/OV-4), so a Full worker keeps the actual surface policy.
    let full = build.security.autonomy == crate::security::AutonomyLevel::Full;
    let view = snapshots.catalog.view(&CatalogScope::default());
    let mut names = BTreeSet::new();
    for entry in view.iter_entries() {
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        if snapshots.publication.allowed().contains(&reference) {
            anyhow::ensure!(
                build
                    .surface_policy
                    .permits(&entry.public_name, &entry.root, &entry.target)
                    && !build.surface_policy.excluded_names.contains(&entry.public_name)
                    && !build.surface_policy.excluded_names.contains(&entry.root),
                "operator worker capability exceeds actual surface policy"
            );
            names.insert(entry.public_name.clone());
        } else if !full {
            build.surface_policy.excluded_names.insert(entry.public_name.clone());
        }
    }
    if !full {
        build.surface_policy.allowed_tools = Some(names);
    }
    snapshots
        .catalog
        .check_published_parity(&build.execution, snapshots.publication.allowed())?;
    let config = &snapshots.generation.effective;
    anyhow::ensure!(
        !config.causal_tree.enabled
            || (crate::memory::serves_tool_execution_ledger(build.memory.name())
                && config.memory.events.record_user_messages),
        "operator worker decision recording is unavailable"
    );
    // The runtime key is the turn's state key always (OV-2). The legacy plane
    // consumers still see it only while the plane is enabled.
    let runtime_key = super::builder::load_key(Arc::clone(&build.generation)).await?;
    let workspace_key = config.causal_tree.enabled.then(|| Arc::clone(&runtime_key));
    let durable_key = workspace_key.is_some();
    let recovery_owner = crate::runtime::turn_cleanup::current_tracker().unwrap_or_default();
    let events = Arc::clone(&build.events);
    let recovery = crate::runtime::ingress_recovery::RecoveryScope::prepare(
        &build.generation,
        Arc::clone(&build.memory),
        TurnSurfaceKind::Worker,
        &recovery_owner,
    )
    .await?;
    let state_key = runtime_key;
    let policy_version = pinned_policy_version(config, &build.security, &build.surface_policy, &state_key)
        .map_err(PublicationBuildError::PolicyFingerprint)?;

    let factory = DecisionBackendFactory::from_config(
        config,
        FactoryDeps {
            catalog: &snapshots.provider_catalog,
            key: workspace_key,
            transports: build.transports,
            config_generation: snapshots.generation.id.0,
            events: Arc::clone(&events),
        },
    )?;
    let service = Arc::new(TurnDecisionService {
        runtime: Arc::new(crate::runtime::turn_context::TurnRuntimeHolder::new(
            recovery,
            None,
            &snapshots.generation.effective,
            &build.security,
        )),
        // R9/R26: an enabled engine analyses the worker's turns on its own
        // runtime context and accounting scope.
        advisory: crate::runtime::decision_advisory::DecisionAdvisory::for_publication(
            &snapshots.generation.effective,
            FactoryDeps {
                catalog: &snapshots.provider_catalog,
                key: Some(Arc::clone(&state_key)),
                transports: build.transports,
                config_generation: snapshots.generation.id.0,
                events: Arc::clone(&events),
            },
        )?,
        memory_loader: None,
        loop_runtime: Some(runtime),
        publication_policy: build.surface_policy,
        operator_process: Some(Arc::clone(&authority)),
        operator_task: None,
        ordinary_parent: None,
        ordinary_task: None,
        ordinary_process: None,
        service_id: uuid::Uuid::now_v7(),
        surface: TurnSurfaceKind::Worker,
        generation: Arc::clone(&snapshots.generation),
        catalog: Arc::clone(&snapshots.catalog),
        provider_catalog: snapshots.provider_catalog.as_ref().clone(),
        factory,
        events,
        state_key,
        policy_version,
        durable_key,
        recordable: durable_key,
        registry: build.registry,
        execution: build.execution,
        memory: build.memory,
        security: build.security,
    });
    service.resolve_agent_model(&authority.source_proof()?)?;
    service.check_live_parity()?;
    Ok(service)
}
