#![cfg(test)]
mod child_process;
pub(crate) use child_process::worker_probe::with_manifest as with_worker_probe_manifest;
mod agent_decision;
mod catalog_drift;
mod discovery_surface;
mod enabled_service;
mod execution;
mod recorded_chain;
mod recursive_budget;
mod skill_receipts;
mod terminal_scope;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::config::schema::causal_tree::{BackendKind, BackendLocality};
use crate::cost::decision_attempt::BackendIdentity;
use crate::cost::decision_attempt::error::DecisionError;
use crate::decision_engine::prediction::backend::DecisionBackend;
use crate::decision_engine::prediction::builtin::BuiltinDecisionBackend;
use crate::decision_engine::prediction::factory::BackendSpec;
use crate::decision_engine::prediction::factory::SlotLimits;
use crate::decision_engine::prediction::outbound::{GuardedBody, OutboundSecrets};
use crate::decision_engine::prediction::protocol::{
    DecisionAnswer, DecisionRequest, DecisionResponse, GlobalQuestion, Probability, QuestionId,
};
use crate::decision_engine::prediction::validate::ModelVersionPolicy;
use crate::memory::{MemoryFabric, NoneMemory};
use crate::observability::causal_event::CausalEvent;
use crate::observability::causal_event::testing::RecordingSink;
use crate::security::SecurityPolicy;
use crate::security::runtime_key::KeyDomain;
use crate::tools::execution::{
    AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, TracingToolExecutionAudit,
};
use crate::tools::traits::{ToolResult, ToolSpec};

use async_trait::async_trait;
use catalog_drift::DriftingCatalogTool;

/// Fixtures and field access for the historical approval tests that live in
/// `security::approval_ledger::tests` (R54).
pub(crate) mod historical_support {
    use std::sync::Arc;

    use super::super::TurnDecisionService;
    use crate::decision_engine::prediction::factory::DecisionBackendFactory;
    use crate::security::runtime_key::DecisionKey;

    pub(crate) use super::{CountingTransports, SilentEvents, sources};

    pub(crate) fn install_factory(service: &mut TurnDecisionService, factory: DecisionBackendFactory) {
        service.factory = factory;
    }

    pub(crate) fn state_key(service: &TurnDecisionService) -> Arc<DecisionKey> {
        Arc::clone(&service.state_key)
    }

    pub(crate) fn recordable_and_durable(service: &TurnDecisionService) -> bool {
        service.recordable && service.durable_key
    }

    pub(crate) fn force_recordable_durable(service: &mut TurnDecisionService, key: Arc<DecisionKey>) {
        service.recordable = true;
        service.durable_key = true;
        service.state_key = key;
    }
}

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

#[test]
fn pinned_policy_version_is_order_independent_and_tracks_policy_changes() {
    let key = crate::security::runtime_key::test_support::ephemeral_key();
    let security = SecurityPolicy::default();
    let surface = SurfacePolicy::default();
    let mut first = crate::config::Config::default();
    let mut second = first.clone();
    let a = crate::config::schema::ModelPricing {
        input: 1.0,
        output: 2.0,
        cache_write: 0.5,
        cache_read: 0.25,
    };
    let b = crate::config::schema::ModelPricing {
        input: 3.0,
        output: 4.0,
        cache_write: 1.0,
        cache_read: 0.5,
    };
    first.cost.prices.insert("provider/a".to_string(), a.clone());
    first.cost.prices.insert("provider/b".to_string(), b.clone());
    second.cost.prices.insert("provider/b".to_string(), b);
    second.cost.prices.insert("provider/a".to_string(), a);
    first
        .tool_tiering
        .model_allowlists
        .insert("model/a".to_string(), vec!["tool/b".to_string(), "tool/a".to_string()]);
    second
        .tool_tiering
        .model_allowlists
        .insert("model/a".to_string(), vec!["tool/a".to_string(), "tool/b".to_string()]);
    let stable = pinned_policy_version(&first, &security, &surface, &key).unwrap();
    assert_eq!(
        stable,
        pinned_policy_version(&second, &security, &surface, &key).unwrap()
    );
    assert!(stable.starts_with("cdp-policy-v1:"));
    assert!(!stable.contains("provider/a"));
    second.cost.daily_limit_usd += 1.0;
    assert_ne!(
        stable,
        pinned_policy_version(&second, &security, &surface, &key).unwrap()
    );
    second.cost.daily_limit_usd -= 1.0;
    second.cost.prices.get_mut("provider/a").unwrap().cache_read += 0.25;
    assert_ne!(
        stable,
        pinned_policy_version(&second, &security, &surface, &key).unwrap()
    );
    let mut changed_security = security;
    changed_security.scope_default_allow = !changed_security.scope_default_allow;
    assert_ne!(
        stable,
        pinned_policy_version(&first, &changed_security, &surface, &key).unwrap()
    );
}

pub(super) async fn disabled_agent_service(
    generation: &Arc<ConfigGeneration>,
    provider_catalog: &ProviderCatalogView,
) -> Arc<TurnDecisionService> {
    agent_service_with_memory(generation, provider_catalog, Arc::new(NoneMemory::new())).await
}

async fn agent_service_with_memory(
    generation: &Arc<ConfigGeneration>,
    provider_catalog: &ProviderCatalogView,
    memory: Arc<dyn Memory>,
) -> Arc<TurnDecisionService> {
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
            parent_grant: None,
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
