//! Publication-time builders: one pinned runtime generation, its
//! publication-scoped `TurnRuntimeHolder` and its `DecisionAdvisory` (R26).
//! Every turn of the publication later opens its own `TurnRuntimeContext`
//! from the holder these builders create.
use std::sync::Arc;

use super::PublicationBuildError;
use crate::config::ConfigGeneration;
use crate::decision_engine::prediction::factory::{DecisionBackendFactory, DecisionTransports, FactoryDeps};
use crate::memory::Memory;
use crate::observability::causal_event::CausalEventSink;
use crate::providers::catalog::CatalogView as ProviderCatalogView;
use crate::runtime::decision_journal::store::StoreMode;
use crate::runtime::turn_decision::{TurnDecisionService, pinned_policy_version};
use crate::security::SecurityPolicy;
use crate::security::runtime_key::DecisionKey;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
use crate::tools::capability::turn_state::TurnSurfaceKind;
use crate::tools::execution::ToolExecutionService;
use crate::tools::traits::Tool;

/// Runtime-owned snapshots that must all describe the same generation.
/// A caller supplies the finalized registry and its execution service,
/// rather than letting the decision plane infer capabilities from config.
pub(crate) struct PublicationInputs<'a> {
    pub parent_grant: Option<Arc<crate::runtime::child_source::IssuedChildGrant>>,
    pub generation: Arc<ConfigGeneration>,
    pub registry: Arc<Vec<Box<dyn Tool>>>,
    pub execution: Arc<ToolExecutionService>,
    pub catalog_sources: CatalogSources,
    pub surface_policy: SurfacePolicy,
    pub provider_catalog: &'a ProviderCatalogView,
    pub transports: &'a dyn DecisionTransports,
    pub events: Arc<dyn CausalEventSink>,
    pub memory: Arc<dyn Memory>,
    pub security: Arc<SecurityPolicy>,
}

/// Actual ingress runtime instances associated with the published catalog.
#[derive(Clone)]
pub(crate) struct PublicationLoopRuntime {
    pub(crate) hooks: Arc<crate::hooks::HookManager>,
    pub(crate) observer: Arc<dyn crate::observability::Observer>,
}

/// Construct an ingress or a durably authorized child service. Ordinary
/// in-process children derive from `OrdinaryChildPublication` instead.
/// The factory receives the persisted workspace key or `None`; an ephemeral
/// state key never enables a remote backend or cross-process child authority.
pub(crate) async fn build_publication(
    surface: TurnSurfaceKind,
    build: PublicationInputs<'_>,
) -> Result<Arc<TurnDecisionService>, PublicationBuildError> {
    if build.catalog_sources.config_generation_id != build.generation.id.0.to_string() {
        return Err(PublicationBuildError::SourceGeneration);
    }
    match (&build.parent_grant, surface) {
        (Some(grant), TurnSurfaceKind::Worker) if Arc::ptr_eq(grant.generation(), &build.generation) => {}
        (None, surface) if surface != TurnSurfaceKind::Worker => {}
        _ => return Err(PublicationBuildError::ChildAuthority),
    }
    let config = &build.generation.effective;
    if config.causal_tree.enabled && !crate::memory::serves_tool_execution_ledger(build.memory.name()) {
        return Err(PublicationBuildError::EventStoreUnsupported);
    }
    if config.causal_tree.enabled && !config.memory.events.record_user_messages {
        return Err(PublicationBuildError::InboundEventsDisabled);
    }
    // `load_key` deliberately does not touch the workspace key while the
    // plane is disabled. Its single preparation result is shared with recovery;
    // an unavailable key leaves both the factory builtin-only and recovery unrecorded.
    let workspace_key =
        crate::runtime::turn_decision::store_preparation::load_key(Arc::clone(&build.generation)).await?;
    let durable_key = config.causal_tree.enabled && workspace_key.is_some();
    if let Some(grant) = &build.parent_grant {
        let key = workspace_key
            .as_ref()
            .filter(|_| durable_key)
            .ok_or(PublicationBuildError::ChildAuthority)?;
        let (manifest, _) = grant.scope().gate_inputs();
        grant
            .scope()
            .validate_runtime(
                key,
                &manifest.authority.owner,
                &config.workspace_dir.to_string_lossy(),
                grant.catalog().view(&CatalogScope::default()).generation(),
            )
            .map_err(|_| PublicationBuildError::ChildAuthority)?;
        for reference in &manifest.allowed {
            let view = grant.catalog().view(&CatalogScope::default());
            let entry = view
                .entry_any(&reference.id)
                .ok_or(PublicationBuildError::ChildAuthority)?;
            if !build
                .surface_policy
                .permits(&entry.public_name, &entry.root, &entry.target)
                || build.surface_policy.excluded_names.contains(&entry.public_name)
                || build.surface_policy.excluded_names.contains(&entry.root)
            {
                return Err(PublicationBuildError::ChildAuthority);
            }
        }
        grant.catalog().check_child_parity(&build.execution, grant.scope())?;
    }
    let recovery_owner = crate::runtime::turn_cleanup::current_tracker().unwrap_or_default();
    let opened = if durable_key {
        Some(
            crate::runtime::ingress_recovery::open_advisory_store(
                &build.generation,
                Arc::clone(&build.memory),
                Arc::clone(&build.events),
                &recovery_owner,
            )
            .await
            .map_err(PublicationBuildError::RecoveryPreparation)?,
        )
    } else {
        None
    };
    let store = match &opened {
        Some((store, _)) => Arc::clone(store),
        None => {
            crate::runtime::turn_decision::store_preparation::open_store(
                Arc::clone(&build.generation),
                Arc::clone(&build.events),
                None,
                false,
            )
            .await?
        }
    };
    let durable_events = opened.as_ref().map(|(_, events)| Arc::clone(events));
    let events: Arc<dyn CausalEventSink> = durable_events.as_ref().map_or_else(
        || Arc::clone(&build.events),
        |events| Arc::clone(events) as Arc<dyn CausalEventSink>,
    );
    let runtime = crate::runtime::turn_context::TurnRuntimeHolder::prepare(
        &build.generation,
        Arc::clone(&build.memory),
        surface,
        build.parent_grant.clone(),
        crate::runtime::turn_context::RecoveryPreparation {
            owner: recovery_owner,
            workspace_key: workspace_key.clone(),
            advisory: match &opened {
                Some((store, events)) => {
                    crate::runtime::ingress_recovery::PreparedAdvisory::opened(Arc::clone(store), Arc::clone(events))
                        .map_err(PublicationBuildError::RecoveryPreparation)?
                }
                None => crate::runtime::ingress_recovery::PreparedAdvisory::default(),
            },
        },
        &build.security,
    )
    .await
    .map_err(PublicationBuildError::RecoveryPreparation)?;
    let state_key = workspace_key.clone().unwrap_or_else(DecisionKey::ephemeral);
    let policy_version = pinned_policy_version(config, &build.security, &build.surface_policy, &state_key)
        .map_err(PublicationBuildError::PolicyFingerprint)?;
    let mut catalog_sources = build.catalog_sources;
    // Node ids and engine child seals must use the same key in this service.
    // The source's independent key cannot assert workspace provenance.
    catalog_sources.node_key = Some(Arc::clone(&state_key));
    let catalog = match &build.parent_grant {
        Some(grant) => Arc::clone(grant.catalog()),
        None => {
            let catalog = Arc::new(CapabilityCatalog::build(
                build.registry.as_ref(),
                &catalog_sources,
                &build.surface_policy,
            )?);
            catalog.check_parity(&build.execution)?;
            catalog
        }
    };

    let factory = DecisionBackendFactory::from_config(
        config,
        FactoryDeps {
            catalog: build.provider_catalog,
            key: workspace_key,
            transports: build.transports,
            config_generation: build.generation.id.0,
            events: Arc::clone(&events),
        },
    )?;
    #[cfg(test)]
    let factory = crate::runtime::turn_decision::evidence_fixture::wrap_factory(
        factory,
        config,
        Arc::clone(&state_key),
        Arc::clone(&events),
    );
    #[cfg(test)]
    let factory = crate::runtime::turn_decision::selected_loader_fixture::wrap_factory(
        factory,
        config,
        Arc::clone(&state_key),
        Arc::clone(&events),
    );
    #[cfg(test)]
    let factory = crate::runtime::turn_decision::ordinary_child_fixture::wrap_factory(
        factory,
        Arc::clone(&state_key),
        Arc::clone(&events),
    );
    // The advisory engine's factory is built from the effective engine
    // backend with this service's state key, never through the plane switch
    // (R55, R9). A disabled engine builds no factory.
    let advisory_factory = if config.decision_engine.enabled {
        Some(DecisionBackendFactory::from_config(
            config,
            FactoryDeps {
                catalog: build.provider_catalog,
                key: Some(Arc::clone(&state_key)),
                transports: build.transports,
                config_generation: build.generation.id.0,
                events: Arc::clone(&events),
            },
        )?)
    } else {
        None
    };
    let recordable = durable_key && store.mode() == StoreMode::ReadWrite;
    Ok(Arc::new(TurnDecisionService {
        runtime: Arc::new(runtime),
        advisory: crate::runtime::decision_advisory::DecisionAdvisory::new(
            Arc::clone(&build.generation.effective),
            advisory_factory,
        ),
        child_recovery: Default::default(),
        memory_loader: None,
        loop_runtime: None,
        publication_policy: build.surface_policy,
        ordinary_parent: None,
        operator_task: None,
        operator_process: None,
        ordinary_task: None,
        ordinary_process: None,
        service_id: uuid::Uuid::now_v7(),
        surface,
        generation: build.generation,
        catalog,
        provider_catalog: build.provider_catalog.clone(),
        factory,
        events,
        state_key,
        policy_version,
        durable_key,
        recordable,
        registry: build.registry,
        execution: build.execution,
        memory: build.memory,
        security: build.security,
        store,
        durable_events,
    }))
}
