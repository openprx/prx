//! Publication-time builders: one pinned runtime generation, its
//! publication-scoped `TurnRuntimeHolder` and its `DecisionAdvisory` (R26).
//! Every turn of the publication later opens its own `TurnRuntimeContext`
//! from the holder these builders create.
use std::sync::Arc;

use super::TurnRuntimeBuildError;
use super::service::{TurnRuntimeService, pinned_policy_version};
use crate::config::ConfigGeneration;
use crate::decision_engine::prediction::factory::{DecisionBackendFactory, DecisionTransports, FactoryDeps};
use crate::memory::Memory;
use crate::observability::causal_event::CausalEventSink;
use crate::providers::catalog::CatalogView as ProviderCatalogView;
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogSources, SurfacePolicy};
use crate::tools::capability::turn_state::TurnSurfaceKind;
use crate::tools::execution::ToolExecutionService;
use crate::tools::traits::Tool;

/// Runtime-owned snapshots that must all describe the same generation.
/// A caller supplies the finalized registry and its execution service,
/// rather than letting the decision plane infer capabilities from config.
pub(crate) struct PublicationInputs<'a> {
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
) -> Result<Arc<TurnRuntimeService>, TurnRuntimeBuildError> {
    if build.catalog_sources.config_generation_id != build.generation.id.0.to_string() {
        return Err(TurnRuntimeBuildError::SourceGeneration);
    }
    // Worker children publish through the ordinary or operator process path.
    if surface == TurnSurfaceKind::Worker {
        return Err(TurnRuntimeBuildError::WorkerSurface);
    }
    let config = &build.generation.effective;
    if config.causal_tree.enabled && !crate::memory::serves_tool_execution_ledger(build.memory.name()) {
        return Err(TurnRuntimeBuildError::EventStoreUnsupported);
    }
    if config.causal_tree.enabled && !config.memory.events.record_user_messages {
        return Err(TurnRuntimeBuildError::InboundEventsDisabled);
    }
    // The runtime key is the turn's state key always (OV-2). The legacy plane
    // consumers still see it only while the plane is enabled.
    let runtime_key = load_runtime_key(Arc::clone(&build.generation)).await?;
    let workspace_key = config.causal_tree.enabled.then(|| Arc::clone(&runtime_key));
    let durable_key = workspace_key.is_some();
    let recovery_owner = crate::runtime::turn_cleanup::current_tracker().unwrap_or_default();
    let events = Arc::clone(&build.events);
    let runtime = crate::runtime::turn_context::TurnRuntimeHolder::prepare(
        &build.generation,
        Arc::clone(&build.memory),
        surface,
        crate::runtime::turn_context::RecoveryPreparation { owner: recovery_owner },
        &build.security,
    )
    .await
    .map_err(TurnRuntimeBuildError::RecoveryPreparation)?;
    let state_key = runtime_key;
    let policy_version = pinned_policy_version(config, &build.security, &build.surface_policy, &state_key)
        .map_err(TurnRuntimeBuildError::PolicyFingerprint)?;
    let mut catalog_sources = build.catalog_sources;
    // Node ids and engine child seals must use the same key in this service.
    // The source's independent key cannot assert workspace provenance.
    catalog_sources.node_key = Some(Arc::clone(&state_key));
    let catalog = Arc::new(CapabilityCatalog::build(
        build.registry.as_ref(),
        &catalog_sources,
        &build.surface_policy,
    )?);
    catalog.check_parity(&build.execution)?;

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
    // The advisory engine's factory is built from the effective engine
    // backend with this service's state key, never through the plane switch
    // (R55, R9). A disabled engine builds no factory.
    let advisory = crate::runtime::decision_advisory::DecisionAdvisory::for_publication(
        &build.generation.effective,
        FactoryDeps {
            catalog: build.provider_catalog,
            key: Some(Arc::clone(&state_key)),
            transports: build.transports,
            config_generation: build.generation.id.0,
            events: Arc::clone(&events),
        },
    )?;
    Ok(Arc::new(TurnRuntimeService {
        runtime: Arc::new(runtime),
        advisory,
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
        recordable: durable_key,
        registry: build.registry,
        execution: build.execution,
        memory: build.memory,
        security: build.security,
    }))
}

/// The turn's state key: the workspace runtime key at
/// `<ws>/self/runtime/runtime.key` (OV-2), loaded or created off the Tokio
/// workers. There is no plane condition and no ephemeral fallback: an
/// unavailable runtime key refuses the publication.
pub(in crate::runtime) async fn load_runtime_key(
    generation: Arc<ConfigGeneration>,
) -> Result<Arc<crate::security::runtime_key::DecisionKey>, TurnRuntimeBuildError> {
    let ticket = crate::runtime::turn_cleanup::current_tracker()
        .map(|owner| owner.register(tokio_util::sync::CancellationToken::new()))
        .transpose()
        .map_err(TurnRuntimeBuildError::PreparationOwner)?;
    let key = tokio::task::spawn_blocking(move || {
        crate::runtime::decision_accounting::initialize_tokenizers();
        let key = crate::security::runtime_key::DecisionKey::load_or_create(&generation.effective.workspace_dir);
        if let Some(ticket) = ticket {
            ticket.finish(true);
        }
        key
    })
    .await?;
    key.map_err(TurnRuntimeBuildError::RuntimeKey)
}
