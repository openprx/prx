#![cfg(test)]
mod child_process;
pub(crate) use child_process::worker_probe::with_manifest as with_worker_probe_manifest;
mod agent_decision;
mod catalog_drift;
mod discovery_surface;
mod enabled_service;
pub(crate) mod exact_call_approval;
mod ledger_probe;
mod performance;
mod recorded_chain;
mod recursive_budget;
mod selected_memory;
mod selection_probes;
mod skill_receipts;
mod terminal_scope;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use chrono::TimeZone;

use super::*;
use crate::causal_tree::decision_select::Selection;
use crate::decision::backend::DecisionBackend;
use crate::decision::builtin::BuiltinDecisionBackend;
use crate::decision::factory::BackendSpec;
use crate::decision::factory::SlotLimits;
use crate::decision::outbound::{GuardedBody, OutboundSecrets};
use crate::decision::protocol::{
    BackendIdentity, BackendKind, BackendLocality, DecisionAnswer, DecisionRequest, DecisionResponse, GlobalQuestion,
    NoulAnswer, Probability, QuestionId,
};
use crate::decision::telemetry::CausalEvent;
use crate::decision::telemetry::testing::RecordingSink;
use crate::decision::validate::ModelVersionPolicy;
use crate::decision::{DecisionError, KeyDomain};
use crate::memory::{MemoryCategory, MemoryEntry, MemoryFabric, NoneMemory};
use crate::security::SecurityPolicy;
use crate::tools::execution::{
    AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, TracingToolExecutionAudit,
};
use crate::tools::traits::{ToolResult, ToolSpec};

#[derive(Default)]
struct CloseSpy {
    replies: parking_lot::Mutex<Vec<String>>,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl CloseTurnPorts for CloseSpy {
    type Receipt = &'static str;

    async fn finalize_closed_turn(&self, _: Arc<TurnDecisionContext>, reply: &str) -> anyhow::Result<Self::Receipt> {
        self.replies.lock().push(reply.to_owned());
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("fixture terminal failure");
        }
        Ok("fixture terminal receipt")
    }
}

struct ReceiptClosePorts {
    receipt: Option<crate::agent::terminal::TurnTerminalReceipt>,
    calls: AtomicUsize,
    cancellation: Option<CancellationToken>,
}

#[async_trait::async_trait]
impl CloseTurnPorts for ReceiptClosePorts {
    type Receipt = crate::agent::terminal::TurnTerminalReceipt;

    async fn finalize_closed_turn(&self, _: Arc<TurnDecisionContext>, _: &str) -> anyhow::Result<Self::Receipt> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
        self.receipt.clone().context("fixture terminal failure")
    }
}

struct FinalizingClosePorts {
    fabric: MemoryFabric,
    commit: crate::agent::terminal::TurnTerminalDraft,
    cost: crate::config::schema::CostConfig,
    workspace: std::path::PathBuf,
    calls: AtomicUsize,
    cancellation: Option<CancellationToken>,
}

#[async_trait::async_trait]
impl CloseTurnPorts for FinalizingClosePorts {
    type Receipt = crate::agent::terminal::TurnTerminalReceipt;

    async fn finalize_closed_turn(&self, _: Arc<TurnDecisionContext>, reply: &str) -> anyhow::Result<Self::Receipt> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut commit = self.commit.clone();
        commit.history = Some(crate::agent::terminal::TurnHistoryProjection {
            assistant_content: reply.to_owned(),
            history_commit_len: 0,
        });
        let receipt = crate::agent::terminal::finalize_turn(&self.fabric, commit, &self.cost, &self.workspace).await?;
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
        Ok(receipt)
    }
}

use async_trait::async_trait;
use catalog_drift::DriftingCatalogTool;
use selection_probes::{
    CountingProbeMemory, ExecuteContextProbeBackend, ProbeCancelMode, RetrieveProbeBackend, UsageRecordProbeBackend,
    run_retrieve_probe, run_retrieve_probe_case,
};

struct SilentEvents;

impl CausalEventSink for SilentEvents {
    fn emit(&self, _event: CausalEvent) {}
}

struct CountingTransports(AtomicUsize);

impl DecisionTransports for CountingTransports {
    fn build(
        &self,
        _spec: &BackendSpec<'_>,
    ) -> Result<Box<dyn crate::decision::backend::DecisionBackend>, DecisionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(DecisionError::Unavailable)
    }
}

fn sources(generation: &ConfigGeneration, config: &crate::config::Config) -> CatalogSources {
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
    let key = DecisionKey::ephemeral();
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
    build_turn_decision_service(
        TurnSurfaceKind::AgentLoop,
        TurnServiceBuild {
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
