//! Test support for the turn service: fixture services and their inputs.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::TurnRuntimeService;
use crate::config::ConfigGeneration;
use crate::cost::decision_attempt::error::DecisionError;
use crate::decision_engine::prediction::factory::{BackendSpec, DecisionTransports};
use crate::memory::{Memory, NoneMemory};
use crate::observability::causal_event::{CausalEvent, CausalEventSink};
use crate::providers::catalog::CatalogView as ProviderCatalogView;
use crate::runtime::turn_publication::{PublicationInputs, build_publication};
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::{CatalogSources, SurfacePolicy};
use crate::tools::capability::turn_state::TurnSurfaceKind;
use crate::tools::execution::{
    AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, ToolExecutionService,
    TracingToolExecutionAudit,
};
use crate::tools::traits::Tool;

pub(crate) struct SilentEvents;

impl CausalEventSink for SilentEvents {
    fn emit(&self, _event: CausalEvent) {}
}

pub(crate) struct CountingTransports(pub(crate) AtomicUsize);

impl DecisionTransports for CountingTransports {
    fn build(
        &self,
        _spec: &BackendSpec<'_>,
    ) -> Result<Box<dyn crate::decision_engine::prediction::backend::DecisionBackend>, DecisionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(DecisionError::Unavailable)
    }
}

pub(crate) fn sources(generation: &ConfigGeneration, config: &crate::config::Config) -> CatalogSources {
    let hooks = crate::hooks::HookDiagnostics {
        configured: false,
        valid: true,
        config_path: String::new(),
        timeout_ms: 0,
        action_count: 0,
        events: BTreeMap::new(),
        catalog_digest: None,
        error: None,
        runs: false,
        blocked: None,
    };
    CatalogSources::from_runtime_inputs(generation.id.0.to_string(), config, Vec::new(), &hooks, 0)
}

pub(crate) async fn disabled_agent_service(
    generation: &Arc<ConfigGeneration>,
    provider_catalog: &ProviderCatalogView,
) -> Arc<TurnRuntimeService> {
    agent_service_with_memory(generation, provider_catalog, Arc::new(NoneMemory::new())).await
}

pub(crate) async fn agent_service_with_memory(
    generation: &Arc<ConfigGeneration>,
    provider_catalog: &ProviderCatalogView,
    memory: Arc<dyn Memory>,
) -> Arc<TurnRuntimeService> {
    let config = &generation.effective;
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
        Arc::clone(&registry),
        Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    ));
    let events: Arc<dyn CausalEventSink> = Arc::new(SilentEvents);
    let transports = CountingTransports(AtomicUsize::new(0));
    build_publication(
        TurnSurfaceKind::AgentLoop,
        PublicationInputs {
            generation: Arc::clone(generation),
            registry,
            execution,
            catalog_sources: sources(generation, config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog,
            transports: &transports,
            events,
            memory,
            security,
        },
    )
    .await
    .expect("Agent fixture service")
}

/// Rejoin a committed terminal through the service's recovery holder, as
/// restart recovery does.
pub(crate) async fn rejoin_terminal(
    service: &TurnRuntimeService,
    terminal_id: &str,
    owner_id: &str,
) -> anyhow::Result<()> {
    service
        .runtime
        .recovery_scope()
        .rejoin_terminal(terminal_id, owner_id)
        .await
}
