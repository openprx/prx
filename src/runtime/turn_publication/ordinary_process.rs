//! Bind a restored ordinary worker source to this process's actual executor.
use std::sync::Arc;

use anyhow::Context;

use super::{PublicationInputs, PublicationLoopRuntime};
use crate::decision_engine::prediction::factory::{DecisionBackendFactory, FactoryDeps};
use crate::runtime::decision_journal::store::CausalDecisionStore;
use crate::runtime::turn_decision::{TurnDecisionService, pinned_policy_version};
use crate::tools::capability::catalog::CatalogScope;
use crate::tools::capability::turn_state::TurnSurfaceKind;

/// Bind the restored source to this process's actual executor and runtime
/// extensions. The signed catalog alone cannot authorize execution.
pub(super) fn ordinary_process_publication(
    authority: Arc<crate::runtime::child_source::ordinary_process::OrdinaryProcessAuthority>,
    build: PublicationInputs<'_>,
    runtime: PublicationLoopRuntime,
) -> anyhow::Result<Arc<TurnDecisionService>> {
    authority.verify()?;
    let snapshots = authority.snapshots();
    anyhow::ensure!(
        build.parent_grant.is_none()
            && Arc::ptr_eq(&build.generation, &snapshots.generation)
            && Arc::ptr_eq(&build.memory, &authority.source_fabric().memory())
            && build.catalog_sources.config_generation_id == snapshots.generation.id.0.to_string(),
        "ordinary worker service differs from restored source"
    );
    let (child, parent) = snapshots.scope.gate_inputs();
    let view = snapshots
        .catalog
        .view(&CatalogScope::default())
        .with_surface(&build.surface_policy);
    for reference in &child.allowed {
        let entry = view
            .entry_any(&reference.id)
            .context("ordinary worker capability missing")?;
        anyhow::ensure!(
            !view.surface_excludes(entry),
            "ordinary worker capability exceeds executor policy"
        );
    }
    snapshots
        .catalog
        .check_child_parity(&build.execution, &snapshots.scope)?;
    let config = &snapshots.generation.effective;
    let policy_version = pinned_policy_version(config, &build.security, &build.surface_policy, &parent.key)?;
    let factory = DecisionBackendFactory::from_config(
        config,
        FactoryDeps {
            catalog: &snapshots.provider_catalog,
            key: None,
            transports: build.transports,
            config_generation: snapshots.generation.id.0,
            events: Arc::clone(&build.events),
        },
    )?
    .with_unproven_budget_gate();
    let store = CausalDecisionStore::open_unshared_read_only(&config.workspace_dir);
    let service = Arc::new(TurnDecisionService {
        runtime: Arc::new(crate::runtime::turn_context::TurnRuntimeHolder::new(
            Arc::new(crate::runtime::ingress_recovery::RecoveryScope::unrecorded(
                Arc::clone(&build.memory),
                config.workspace_dir.clone(),
                TurnSurfaceKind::Worker,
            )),
            Some(Arc::new(std::sync::atomic::AtomicU64::new(
                u64::from(child.authority.max_cost_cents) * 10_000,
            ))),
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
                key: Some(Arc::clone(&parent.key)),
                transports: build.transports,
                config_generation: snapshots.generation.id.0,
                events: Arc::clone(&build.events),
            },
        )?,
        child_recovery: Default::default(),
        memory_loader: None,
        loop_runtime: Some(runtime),
        publication_policy: build.surface_policy,
        ordinary_parent: None,
        operator_task: None,
        operator_process: None,
        ordinary_task: None,
        ordinary_process: Some(Arc::clone(&authority)),
        service_id: uuid::Uuid::now_v7(),
        surface: TurnSurfaceKind::Worker,
        generation: Arc::clone(&snapshots.generation),
        catalog: Arc::clone(&snapshots.catalog),
        provider_catalog: snapshots.provider_catalog.as_ref().clone(),
        factory,
        events: build.events,
        state_key: Arc::clone(&parent.key),
        policy_version,
        durable_key: false,
        recordable: false,
        registry: build.registry,
        execution: build.execution,
        memory: build.memory,
        security: build.security,
        store,
        durable_events: None,
    });
    service.check_live_parity()?;
    Ok(service)
}
