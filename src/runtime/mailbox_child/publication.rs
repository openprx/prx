//! A fresh capped publication; no ended turn context or approval grant is kept.
use crate::runtime::turn_publication::TurnRuntimeService;
use std::sync::Arc;
pub(super) fn fresh(
    parent: &Arc<TurnRuntimeService>,
    level: crate::security::AutonomyLevel,
    config: &crate::config::Config,
) -> anyhow::Result<Arc<TurnRuntimeService>> {
    use crate::decision_engine::prediction::factory::{DecisionBackendFactory, FactoryDeps};
    let generation = crate::config::ConfigGeneration::for_child(config)?;
    let provider_catalog = crate::providers::selection::catalog_for(config);
    let security = Arc::new(crate::security::SecurityPolicy {
        autonomy: level,
        ..crate::security::SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
    });
    let mut policy = crate::runtime::turn_publication::surface_policy(
        parent.surface,
        &config.tool_tiering.at_level(level, security.evaluation_sandbox),
    );
    policy
        .direct_names
        .extend(parent.publication_policy.direct_names.iter().cloned());
    policy
        .excluded_names
        .extend(parent.publication_policy.excluded_names.iter().cloned());
    if level != crate::security::AutonomyLevel::Full {
        policy.allowed_tools = parent.publication_policy.allowed_tools.clone();
    }
    let policy_version = crate::runtime::turn_publication::service::pinned_policy_version(
        config,
        &security,
        &policy,
        &parent.state_key,
    )?;
    let deps = || FactoryDeps {
        catalog: &provider_catalog,
        key: Some(Arc::clone(&parent.state_key)),
        transports: &crate::decision_engine::prediction::compatible_http::HttpDecisionTransports,
        config_generation: generation.id.0,
        events: Arc::clone(&parent.events),
    };
    Ok(Arc::new(TurnRuntimeService {
        runtime: Arc::new(crate::runtime::turn_context::TurnRuntimeHolder::new(
            Arc::new(crate::runtime::ingress_recovery::RecoveryScope::unrecorded(
                Arc::clone(&parent.memory),
                config.workspace_dir.clone(),
                crate::tools::capability::turn_state::TurnSurfaceKind::Server,
            )),
            None,
            config,
            &security,
        )),
        advisory: crate::runtime::decision_advisory::DecisionAdvisory::for_publication(&generation.effective, deps())?,
        memory_loader: parent.memory_loader.clone(),
        loop_runtime: parent.loop_runtime.clone(),
        publication_policy: policy,
        chat_lineage: parent.chat_lineage,
        ordinary_parent: None,
        operator_task: None,
        operator_process: None,
        ordinary_task: None,
        ordinary_process: None,
        service_id: uuid::Uuid::new_v4(),
        surface: crate::tools::capability::turn_state::TurnSurfaceKind::Server,
        generation: Arc::clone(&generation),
        catalog: Arc::clone(&parent.catalog),
        provider_catalog: provider_catalog.clone(),
        factory: DecisionBackendFactory::from_config(config, deps())?,
        events: Arc::clone(&parent.events),
        state_key: Arc::clone(&parent.state_key),
        policy_version,
        recordable: parent.recordable,
        registry: Arc::clone(&parent.registry),
        execution: Arc::clone(&parent.execution),
        memory: Arc::clone(&parent.memory),
        security,
    }))
}
