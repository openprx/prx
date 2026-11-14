//! One pinned runtime generation: the publication-scoped turn service.
//!
//! The constructor receives the already finalized runtime catalog inputs.
//! Rebuilding them from configuration alone would drop live MCP, Skill, hook
//! and node state, so a source-generation mismatch rejects the service.
//! Methods that act on one opened turn live in `runtime::turn_context`.

use anyhow::Context;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use super::PublicationLoopRuntime;
use crate::config::ConfigGeneration;
use crate::cost::estimate::DecisionReserveUnavailable;
use crate::decision_engine::prediction::factory::DecisionBackendFactory;
use crate::memory::Memory;
use crate::observability::causal_event::CausalEventSink;
use crate::providers::catalog::{CatalogView as ProviderCatalogView, ResolvedModel};
use crate::runtime::child_source::publication::OrdinaryChildPublication;
use crate::runtime::ingress_recovery::status::DecisionPlaneStatus;
use crate::runtime::turn_context::{OpenedTurn, TurnOriginMismatch};
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::SecurityPolicy;
use crate::security::runtime_key::{DecisionKey, KeyDomain};
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogView, ParityError, SurfacePolicy};
use crate::tools::capability::turn_state::{TurnBudgets, TurnSurfaceKind};
use crate::tools::execution::ToolExecutionService;
use crate::tools::traits::Tool;

pub(crate) struct TurnRuntimeService {
    /// Publication-scoped runtime holder (recovery, recursive allowances,
    /// child funding, spend inputs) shared with every opened turn.
    pub(in crate::runtime) runtime: Arc<crate::runtime::turn_context::TurnRuntimeHolder>,
    pub(in crate::runtime) advisory: crate::runtime::decision_advisory::DecisionAdvisory,
    pub(in crate::runtime) memory_loader: Option<Arc<dyn crate::agent::memory_loader::MemoryLoader>>,
    pub(in crate::runtime) loop_runtime: Option<PublicationLoopRuntime>,
    pub(in crate::runtime) publication_policy: SurfacePolicy,
    pub(in crate::runtime) ordinary_parent: Option<OrdinaryChildPublication>,
    pub(in crate::runtime) operator_task: Option<Arc<crate::runtime::operator_spawn::task::OperatorTaskAuthority>>,
    pub(in crate::runtime) operator_process:
        Option<Arc<crate::runtime::child_source::operator_process::OperatorProcessAuthority>>,
    pub(in crate::runtime) ordinary_task:
        Option<Arc<crate::runtime::child_source::ordinary_task::OrdinaryTaskAuthority>>,
    pub(in crate::runtime) ordinary_process:
        Option<Arc<crate::runtime::child_source::ordinary_process::OrdinaryProcessAuthority>>,
    pub(in crate::runtime) service_id: uuid::Uuid,
    pub(in crate::runtime) surface: TurnSurfaceKind,
    pub(in crate::runtime) generation: Arc<ConfigGeneration>,
    pub(in crate::runtime) catalog: Arc<CapabilityCatalog>,
    pub(in crate::runtime) provider_catalog: ProviderCatalogView,
    pub(in crate::runtime) factory: DecisionBackendFactory,
    pub(in crate::runtime) events: Arc<dyn CausalEventSink>,
    pub(in crate::runtime) state_key: Arc<DecisionKey>,
    pub(in crate::runtime) policy_version: Arc<str>,
    pub(in crate::runtime) durable_key: bool,
    pub(in crate::runtime) recordable: bool,
    pub(in crate::runtime) registry: Arc<Vec<Box<dyn Tool>>>,
    pub(in crate::runtime) execution: Arc<ToolExecutionService>,
    pub(in crate::runtime) memory: Arc<dyn Memory>,
    pub(in crate::runtime) security: Arc<SecurityPolicy>,
}

impl TurnRuntimeService {
    /// The turn's advisory decision driver.
    #[must_use]
    pub(crate) const fn advisory(&self) -> &crate::runtime::decision_advisory::DecisionAdvisory {
        &self.advisory
    }

    /// The publication-scoped model, limits and parity owner (R25).
    pub(crate) fn publication_scope(&self) -> crate::runtime::turn_context::publication_scope::PublicationScope<'_> {
        crate::runtime::turn_context::publication_scope::PublicationScope {
            generation: &self.generation,
            surface: self.surface,
            provider_catalog: &self.provider_catalog,
            catalog: &self.catalog,
            publication_policy: &self.publication_policy,
            execution: &self.execution,
            memory: &self.memory,
            security: &self.security,
            memory_loader: self.memory_loader.as_deref(),
            state_key: &self.state_key,
            holder: &self.runtime,
            ordinary_parent: self.ordinary_parent.as_ref(),
            ordinary_task: self.ordinary_task.as_deref(),
            operator_task: self.operator_task.as_deref(),
            operator_process: self.operator_process.as_deref(),
            ordinary_process: self.ordinary_process.as_deref(),
        }
    }

    #[must_use]
    pub(crate) fn memory(&self) -> &Arc<dyn Memory> {
        self.publication_scope().memory()
    }

    #[must_use]
    pub(crate) fn execution(&self) -> &ToolExecutionService {
        self.publication_scope().execution()
    }

    #[must_use]
    pub(crate) fn catalog(&self) -> &CapabilityCatalog {
        self.publication_scope().catalog()
    }

    #[cfg(test)]
    pub(crate) fn prediction_port_calls(&self) -> usize {
        self.factory.prediction_port_calls()
            + self.advisory.factory().map_or(
                0,
                crate::decision_engine::prediction::factory::DecisionBackendFactory::prediction_port_calls,
            )
    }

    #[must_use]
    pub(crate) const fn publication_policy(&self) -> &SurfacePolicy {
        &self.publication_policy
    }

    #[must_use]
    pub(crate) fn security(&self) -> &SecurityPolicy {
        self.publication_scope().security()
    }

    #[must_use]
    pub(crate) fn tool_tiering(&self) -> &crate::config::ToolTieringConfig {
        self.publication_scope().tool_tiering()
    }

    #[must_use]
    pub(crate) const fn channel_surface(&self) -> bool {
        matches!(self.surface, TurnSurfaceKind::Channel)
    }

    pub(crate) fn effective_provider_capabilities(
        &self,
        provider: &str,
        model: &str,
        adapter: crate::providers::ProviderCapabilities,
    ) -> crate::providers::ProviderCapabilities {
        self.publication_scope()
            .effective_provider_capabilities(provider, model, adapter)
    }

    pub(crate) fn effective_compaction(
        &self,
        provider: &str,
        model: &str,
    ) -> crate::providers::context_window::EffectiveCompactionConfig {
        self.publication_scope().effective_compaction(provider, model)
    }
}

impl TurnRuntimeService {
    /// Attach runtime instances before the service is shared with any caller.
    pub(crate) fn with_loop_runtime(mut self: Arc<Self>, runtime: PublicationLoopRuntime) -> anyhow::Result<Arc<Self>> {
        let service = Arc::get_mut(&mut self).context("turn service already published")?;
        anyhow::ensure!(service.loop_runtime.is_none(), "turn loop runtime already attached");
        service.loop_runtime = Some(runtime);
        Ok(self)
    }

    pub(crate) fn with_memory_loader(
        mut self: Arc<Self>,
        loader: Option<Arc<dyn crate::agent::memory_loader::MemoryLoader>>,
    ) -> anyhow::Result<Arc<Self>> {
        let service = Arc::get_mut(&mut self).context("turn service already published")?;
        anyhow::ensure!(service.memory_loader.is_none(), "turn memory loader already attached");
        anyhow::ensure!(
            loader.is_none() || service.surface == TurnSurfaceKind::AgentLoop,
            "custom Memory loader requires the embedded Agent surface"
        );
        service.memory_loader = loader;
        Ok(self)
    }

    pub(in crate::runtime) fn catalog_view(&self) -> CatalogView<'_> {
        self.catalog
            .view(&CatalogScope::default())
            .with_surface(&self.publication_policy)
    }

    /// Flush queued decision writes and then their causal events before the
    /// owning runtime exits. A stalled backend cannot hold CLI shutdown open.
    pub(crate) async fn shutdown(&self, deadline: Instant) {
        let _ = self.drain_writes(deadline).await;
    }

    /// Report queue acknowledgement to owners that must reject an incomplete exit.
    pub(crate) async fn drain_writes(&self, deadline: Instant) -> bool {
        self.drain_advisory(deadline).await
    }
}

/// The version binds the code's gate semantics to the exact policy snapshots
/// this service publishes. Maps are sorted before encoding; dynamic spend,
/// grants and child state belong to the turn snapshot instead.
pub(in crate::runtime) fn pinned_policy_version(
    config: &crate::config::Config,
    security: &SecurityPolicy,
    surface: &SurfacePolicy,
    key: &DecisionKey,
) -> Result<Arc<str>, serde_json::Error> {
    let prices: BTreeMap<_, _> = config
        .cost
        .prices
        .iter()
        .map(|(name, price)| {
            (
                name,
                (
                    price.input.to_bits(),
                    price.output.to_bits(),
                    price.cache_write.to_bits(),
                    price.cache_read.to_bits(),
                ),
            )
        })
        .collect();
    let allowlists: BTreeMap<_, _> = config
        .tool_tiering
        .model_allowlists
        .iter()
        .map(|(model, names)| {
            let mut names = names.clone();
            names.sort();
            (model, names)
        })
        .collect();
    let encoded = serde_json::to_vec(&(
        "cdp-policy-semantics-v1",
        crate::decision_engine::cost_model::LATENCY_DEFAULT_VERSION,
        &config.causal_tree,
        (
            &security.autonomy,
            &security.workspace_dir,
            security.workspace_only,
            &security.forbidden_paths,
            security.max_cost_per_day_cents,
            &security.scope_rules,
            security.scope_default_allow,
            &security.assign_owners,
            &security.audit_config,
        ),
        (&surface.excluded_names, &surface.direct_names, surface.channel_allowed),
        (
            &config.tool_tiering.always_include,
            &config.tool_tiering.always_exclude,
            &config.tool_tiering.channel_exclude,
            allowlists,
        ),
        (
            config.cost.daily_limit_usd.to_bits(),
            config.cost.monthly_limit_usd.to_bits(),
            config.cost.warn_at_percent,
            config.cost.allow_override,
            prices,
        ),
    ))?;
    let digest = match &surface.allowed_tools {
        Some(allowed) => key.hmac(
            KeyDomain::StateFingerprint,
            &[
                b"policy-version-v1",
                &encoded,
                b"surface-allowlist-v1",
                &serde_json::to_vec(allowed)?,
            ],
        ),
        None => key.hmac(KeyDomain::StateFingerprint, &[b"policy-version-v1", &encoded]),
    };
    Ok(Arc::from(format!("cdp-policy-v1:{}", hex::encode(digest))))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TurnModelError {
    #[error(transparent)]
    Origin(#[from] TurnOriginMismatch),
    #[error(transparent)]
    Resolution(#[from] crate::providers::catalog::ResolutionError),
    #[error("selected provider route differs from the pinned decision catalog")]
    RouteDrift,
    #[error("selected provider identity or credential reference changed before decision")]
    SelectionDrift,
}

/// Budget facts for one authenticated Agent turn. The effective route window
/// and remaining money come from pinned runtime state, never an input object
/// supplied by a decision backend or caller.
pub(crate) struct AgentDecisionBudget {
    pub(crate) budgets: TurnBudgets,
    pub(crate) main_compaction: crate::config::AgentCompactionConfig,
}

pub(crate) struct SendLimits {
    pub(crate) response_reserve: usize,
    /// The largest prompt a send of this turn may carry: the effective
    /// window minus the response and decision reserves.
    pub(crate) main_input_limit: usize,
    pub(crate) remaining_micro_usd: u64,
    pub(crate) model: ResolvedModel,
    pub(crate) cost: crate::config::schema::CostConfig,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentDecisionBudgetError {
    #[error(transparent)]
    Model(#[from] TurnModelError),
    #[error(transparent)]
    Reserve(#[from] DecisionReserveUnavailable),
    #[error("remaining workspace spend cannot be read safely")]
    Spend(#[source] anyhow::Error),
    #[error("child spend funding is unavailable")]
    ChildFunding,
}

/// Delegations to the R25 owner in `runtime::decision_accounting`.
impl TurnRuntimeService {
    pub(crate) fn auxiliary_accounting(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
        purpose: crate::runtime::auxiliary_call::AuxiliaryPurpose,
    ) -> anyhow::Result<Arc<crate::runtime::auxiliary_call::AuxiliaryAccounting>> {
        crate::runtime::decision_accounting::auxiliary::auxiliary_accounting(self, proof, context, purpose)
    }

    pub(crate) fn smart_group_accounting(
        &self,
        proof: &dyn TurnSourceProof,
        context: &OpenedTurn,
        classifier_provider: &str,
        classifier_model: &str,
    ) -> anyhow::Result<Arc<crate::runtime::auxiliary_call::AuxiliaryAccounting>> {
        crate::runtime::decision_accounting::auxiliary::smart_group_accounting(
            self,
            proof,
            context,
            classifier_provider,
            classifier_model,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("decision context is not recordable by this pinned service")]
pub(crate) struct SettlementRefUnavailable;

impl TurnRuntimeService {
    /// The execution-owned terminal binding of a turn this service opened
    /// (R29). Body in `runtime::turn_terminal::settlement`.
    pub(crate) fn terminal_binding(
        &self,
        context: &OpenedTurn,
    ) -> Result<crate::agent::terminal::TerminalBinding, SettlementRefUnavailable> {
        crate::runtime::turn_terminal::settlement::terminal_binding(self, context)
    }

    /// The publication-scoped runtime holder shared with every opened turn.
    pub(crate) const fn runtime_holder(&self) -> &Arc<crate::runtime::turn_context::TurnRuntimeHolder> {
        &self.runtime
    }

    /// Close the advisory telemetry of this publication. A late telemetry
    /// drain only warns; a disabled advisory drains as a no-op.
    pub(in crate::runtime) async fn drain_advisory(&self, deadline: Instant) -> bool {
        self.advisory.drain_or_warn(deadline).await;
        true
    }

    pub(crate) fn status(&self) -> DecisionPlaneStatus {
        crate::runtime::ingress_recovery::status::plane_status(
            self.factory.status(),
            crate::runtime::ingress_recovery::status::JournalAttachment {
                catalog_generation: *self.catalog.generation(),
                durable_key: self.durable_key,
                recordable: self.recordable,
            },
        )
    }

    pub(crate) fn prompt_memory_loader(&self) -> Option<&dyn crate::agent::memory_loader::MemoryLoader> {
        self.publication_scope().prompt_memory_loader()
    }
}

impl TurnRuntimeService {
    pub(crate) fn agent_budget_snapshot(
        &self,
        proof: &dyn TurnSourceProof,
    ) -> Result<AgentDecisionBudget, AgentDecisionBudgetError> {
        self.publication_scope().agent_budget_snapshot(proof)
    }

    pub(crate) fn send_limits(&self, proof: &dyn TurnSourceProof) -> Result<SendLimits, AgentDecisionBudgetError> {
        self.publication_scope().send_limits(proof)
    }

    pub(crate) fn check_live_parity(&self) -> Result<(), ParityError> {
        self.publication_scope().check_live_parity()
    }

    pub(in crate::runtime) fn child_scope(&self) -> Option<&Arc<crate::tools::capability::child::VerifiedChildScope>> {
        self.publication_scope().child_scope()
    }

    pub(crate) fn resolve_agent_model(&self, proof: &dyn TurnSourceProof) -> Result<ResolvedModel, TurnModelError> {
        self.publication_scope().resolve_agent_model(proof)
    }

    pub(in crate::runtime) fn credential_for_proof(
        &self,
        proof: &dyn TurnSourceProof,
    ) -> crate::providers::credentials::CredentialRef {
        self.publication_scope().credential_for_proof(proof)
    }

    pub(crate) fn model_can_call_tools(&self, proof: &dyn TurnSourceProof) -> bool {
        self.publication_scope().model_can_call_tools(proof)
    }

    /// The pinned catalog entries this service's own executor serves with
    /// descriptor parity. A child's catalog is its parent's pinned one, so it
    /// can hold entries the child's executor does not run (interactive-only
    /// tools of a process worker, tools the child host never registers).
    pub(in crate::runtime) fn child_served_entries(
        &self,
        catalog: &CatalogView<'_>,
    ) -> std::collections::BTreeSet<crate::tools::capability::contract::CapabilityId> {
        catalog
            .iter_entries()
            .filter(|entry| {
                let reference = crate::tools::surface_owner::publication::entry_ref(entry);
                self.catalog
                    .check_published_parity(&self.execution, &std::collections::BTreeSet::from([reference]))
                    .is_ok()
            })
            .map(|entry| entry.id.clone())
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;
