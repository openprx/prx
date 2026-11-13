//! One pinned runtime generation for causal turn decisions.
//!
//! The constructor receives the already finalized runtime catalog inputs.
//! Rebuilding them from configuration alone would drop live MCP, Skill, hook
//! and node state, so a source-generation mismatch rejects the service.

#[cfg(test)]
pub(in crate::runtime) mod ordinary_child_fixture;
#[cfg(test)]
pub(crate) use ordinary_child_fixture::{
    with_inherited_scored_prediction, with_scored_prediction, with_scored_publication,
};

use crate::cost::estimate::DecisionReserveUnavailable;
#[cfg(test)]
use crate::tools::capability::turn_state::ConstraintCode;

#[cfg(test)]
pub(crate) mod evidence_fixture;
#[cfg(test)]
pub(crate) mod selected_loader_fixture;

mod issued;
mod opening;
pub(in crate::runtime) mod store_preparation;

use super::turn_publication::{PublicationBuildError, PublicationLoopRuntime};
#[cfg(test)]
use super::turn_publication::{PublicationInputs, build_publication};

mod call_approval;
#[cfg(test)]
pub(crate) use crate::runtime::ingress_recovery::child_telemetry::child_recovery_gate;
mod delegation;
mod discovery;
mod execution;
use super::child_source::publication::OrdinaryChildPublication;
use super::child_turn::segment_observation;
mod owner_filesystem;
mod pending_approval;
use crate::security::approval_ledger::historical::AdmittedDecisionResume;

use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use super::turn_source::TurnSourceProof;
#[cfg(test)]
use crate::agent::entry::AgentTurnProof;
use crate::capability::CapabilityAvailabilityLevel;
use crate::causal_tree::decision_engine::{DiscoveryIssueError, EngineDecision, RetrievalIssueError};
use crate::decision_engine::candidate_generator::DiscoveredRefs;
use crate::runtime::turn_context::{
    AgentDecisionError, AgentTurnKind, TurnOriginMismatch, validate_agent_retrieval_provenance,
    validate_agent_turn_origin,
};
use crate::tools::capability::contract::{CapabilityRef, CatalogGeneration};

use crate::causal_tree::outcome::DecisionRecordBuilder;
use crate::config::ConfigGeneration;
#[cfg(test)]
use crate::decision_engine::prediction::factory::DecisionTransports;
use crate::decision_engine::prediction::factory::{DecisionBackendFactory, FactoryDeps};
use crate::decision_engine::state::{CausalState, DerivedState, HopLineage, RawRoutingInput};
use crate::memory::Memory;
use crate::observability::causal_event::CausalEventSink;
use crate::providers::catalog::{CatalogView as ProviderCatalogView, ResolvedModel};
use crate::runtime::decision_journal::store::{CausalDecisionStore, Retention, StoreMode};
#[cfg(test)]
use crate::runtime::decision_journal::store::{DecisionScope, Enqueue};
use crate::security::SecurityPolicy;
use crate::security::decision_id::DecisionId;
use crate::security::runtime_key::{DecisionKey, KeyDomain};
#[cfg(test)]
use crate::tools::capability::catalog::CatalogSources;
use crate::tools::capability::catalog::{
    CapabilityCatalog, CatalogScope, CatalogView, Exposure, ParityError, SurfacePolicy,
};
use crate::tools::capability::turn_state::{
    DeliveryConstraints, TrustedParts, TrustedState, TurnBudgets, TurnSurfaceKind,
};
use crate::tools::execution::ToolExecutionService;
use crate::tools::traits::Tool;

pub(crate) struct TurnDecisionService {
    /// Publication-scoped runtime holder (recovery, recursive allowances,
    /// child funding, spend inputs) shared with every opened turn.
    pub(in crate::runtime) runtime: Arc<crate::runtime::turn_context::TurnRuntimeHolder>,
    pub(in crate::runtime) advisory: crate::runtime::decision_advisory::DecisionAdvisory,
    pub(in crate::runtime) child_recovery: crate::runtime::ingress_recovery::child_telemetry::ChildRecoveryState,
    pub(in crate::runtime) memory_loader: Option<Arc<dyn crate::agent::memory_loader::MemoryLoader>>,
    pub(in crate::runtime) loop_runtime: Option<PublicationLoopRuntime>,
    pub(in crate::runtime) publication_policy: SurfacePolicy,
    pub(in crate::runtime) ordinary_parent: Option<OrdinaryChildPublication>,
    pub(in crate::runtime) operator_task: Option<Arc<super::operator_spawn::task::OperatorTaskAuthority>>,
    pub(in crate::runtime) operator_process:
        Option<Arc<super::child_source::operator_process::OperatorProcessAuthority>>,
    pub(in crate::runtime) ordinary_task: Option<Arc<super::child_source::ordinary_task::OrdinaryTaskAuthority>>,
    pub(in crate::runtime) ordinary_process:
        Option<Arc<super::child_source::ordinary_process::OrdinaryProcessAuthority>>,
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
    pub(in crate::runtime) store: Arc<CausalDecisionStore>,
    pub(in crate::runtime) durable_events: Option<Arc<super::causal_events::DurableCausalEventSink>>,
}

impl TurnDecisionService {
    /// The turn's advisory decision driver.
    #[must_use]
    pub(crate) const fn advisory(&self) -> &crate::runtime::decision_advisory::DecisionAdvisory {
        &self.advisory
    }

    /// The publication-scoped model, limits and parity owner (R25); the
    /// service lends its fields until the Part-2 builders own them.
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

use crate::runtime::ingress_recovery::status::DecisionPlaneStatus;

impl TurnDecisionService {
    /// Attach runtime instances before the service is shared with any caller.
    pub(crate) fn with_loop_runtime(mut self: Arc<Self>, runtime: PublicationLoopRuntime) -> anyhow::Result<Arc<Self>> {
        let service = Arc::get_mut(&mut self).context("decision service already published")?;
        anyhow::ensure!(service.loop_runtime.is_none(), "decision loop runtime already attached");
        service.loop_runtime = Some(runtime);
        Ok(self)
    }

    pub(crate) fn with_memory_loader(
        mut self: Arc<Self>,
        loader: Option<Arc<dyn crate::agent::memory_loader::MemoryLoader>>,
    ) -> anyhow::Result<Arc<Self>> {
        let service = Arc::get_mut(&mut self).context("decision service already published")?;
        anyhow::ensure!(
            service.memory_loader.is_none(),
            "decision memory loader already attached"
        );
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
        let decision_drained = self.drain_decision_work(deadline).await;
        self.runtime.recovery_scope().drain(deadline).await && decision_drained
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

struct EffectiveAgentExclusions {
    names: Vec<Arc<str>>,
    model_admission: Option<Arc<dyn crate::tools::capability::turn_state::model_admission::ModelAdmissionView>>,
}

fn effective_agent_exclusions(
    config: &crate::config::Config,
    registry: &[Box<dyn Tool>],
    catalog: &CatalogView<'_>,
    model_name: &str,
    policy_version: &str,
    key: &DecisionKey,
) -> EffectiveAgentExclusions {
    let names: BTreeSet<Arc<str>> = config
        .tool_tiering
        .always_exclude
        .iter()
        .map(|name| Arc::from(name.as_str()))
        .collect();
    let model_admission = config.tool_tiering.model_allowlists.get(model_name).map(|allowlist| {
        crate::tools::capability::model_policy::ModelAdmission::fresh(
            registry,
            catalog,
            model_name,
            allowlist,
            policy_version,
            key,
        ) as Arc<dyn crate::tools::capability::turn_state::model_admission::ModelAdmissionView>
    });
    EffectiveAgentExclusions {
        names: names.into_iter().collect(),
        model_admission,
    }
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

/// The immutable result of one source-bound decision chain. Only the service
/// constructs it; later record and execution streams consume these facts.
pub(crate) struct TurnDecisionContext {
    _event_memory: Arc<super::causal_events::routes::EventMemory>,
    owner_filesystem: Option<Arc<crate::tools::owner_filesystem::OwnerFilesystem>>,
    resume: Option<Arc<AdmittedDecisionResume>>,
    /// The execution-owned runtime context of this turn (R60).
    runtime: Arc<crate::runtime::turn_context::TurnRuntimeContext>,
    source_actor: (Arc<str>, Arc<str>),
    pub(in crate::runtime) service_id: uuid::Uuid,
    pub(in crate::runtime) record_admitted: AtomicBool,
    pub(in crate::runtime) execution_trace:
        std::sync::OnceLock<crate::tools::capability::execution_trace::ExecutionTrace>,
    pub(in crate::runtime) execution_closed: Arc<AtomicBool>,

    collected_approval_result:
        std::sync::OnceLock<Box<crate::security::approval_ledger::historical::CollectedApprovalResult>>,
    pub(in crate::runtime) engine: EngineDecision,
    /// Completed dependent hops retained for the recorder. Nothing is
    /// written until the whole chain returns successfully.
    prior_hops: Vec<EngineDecision>,
    main_compaction: crate::config::AgentCompactionConfig,
    hop_ids: Vec<DecisionId>,
}

impl TurnDecisionContext {
    pub(crate) fn claimed_resume_event(&self) -> Option<&crate::memory::MessageEvent> {
        self.resume.as_ref().and_then(|resume| resume.claimed_event())
    }
    pub(crate) fn source_channel(&self) -> &str {
        &self.source_actor.0
    }
    pub(crate) fn source_actor_matches(&self, channel: Option<&str>, sender: Option<&str>) -> bool {
        channel == Some(self.source_actor.0.as_ref()) && sender == Some(self.source_actor.1.as_ref())
    }

    pub(crate) fn validate_silent_terminal(
        &self,
        status: crate::agent::terminal::TurnTerminalStatus,
        outcome: Option<&crate::llm::route_decision::ProviderExecutionOutcome>,
    ) -> anyhow::Result<()> {
        crate::agent::terminal::binding::validate_silent_terminal(self.execution_trace.get(), status, outcome)
    }

    /// Delegates to the R25 owner in `runtime::turn_terminal::execution_close` (until Part 2).
    pub(crate) fn terminal_execution_trace(&self) -> crate::tools::capability::execution_trace::ExecutionTraceSnapshot {
        crate::runtime::turn_terminal::execution_close::terminal_execution_trace(
            &self.execution_closed,
            &self.runtime.discovery,
            &self.execution_trace,
        )
    }

    #[cfg(test)]
    pub(crate) const fn engine(&self) -> &EngineDecision {
        &self.engine
    }

    pub(crate) const fn main_compaction(&self) -> &crate::config::AgentCompactionConfig {
        &self.main_compaction
    }

    pub(crate) const fn decision_id(&self) -> DecisionId {
        self.engine.decision_id
    }

    pub(crate) fn hop_ids(&self) -> &[DecisionId] {
        &self.hop_ids
    }

    /// The complete bounded chain in decision order. CDP-22 consumes this
    /// only after a successful service return, so cancellation drops it.
    pub(crate) fn audit_hops(&self) -> impl Iterator<Item = &EngineDecision> {
        self.prior_hops.iter().chain(std::iter::once(&self.engine))
    }

    /// The only actions CDP-22 can classify from the terminal receipt alone.
    /// Tool and child actions need their execution trace before settlement.
    pub(crate) const fn terminal_outcome_target(&self) -> Option<&'static str> {
        let crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. } = &self.engine.selection;
        Some(candidate_id.target().as_str())
    }

    pub(crate) const fn terminal_outcome_action(&self) -> Option<crate::tools::capability::contract::CausalActionKind> {
        use crate::tools::capability::contract::CausalActionKind;
        // Every turn opens through the prediction-free path (R9): the
        // committed selection is always the answer candidate.
        Some(CausalActionKind::Answer)
    }

    pub(crate) const fn catalog_generation(&self) -> CatalogGeneration {
        self.engine.state.trusted.parts().catalog_generation
    }

    #[cfg(test)]
    pub(crate) fn inherited_constraint_codes(&self) -> &[ConstraintCode] {
        &self.engine.state.trusted.parts().constraints
    }

    /// Exact final-gated parent execution metadata. Reading these references
    /// grants no dispatch lease; the constrained reader must recheck scope,
    /// generation, security and approval at its execution boundary.
    pub(crate) fn collected_pending_event(&self) -> Option<&crate::memory::MessageEvent> {
        self.collected_approval_result
            .get()
            .map(|result| result.event())
            .filter(|event| event.event_type == "approval.decision.pending")
    }

    pub(crate) fn collected_declined_event(&self) -> Option<&crate::memory::MessageEvent> {
        self.collected_approval_result
            .get()
            .map(|result| result.event())
            .filter(|event| event.event_type == "approval.collection.declined")
    }

    pub(crate) fn execution_refs(&self) -> &BTreeSet<CapabilityRef> {
        &self.runtime.published
    }

    /// Canonical ids of every capability this turn could execute: the issued
    /// references and those admitted by discovery without an approval.
    /// Read-only; the skill lifecycle uses it to tell a skipped tool from one
    /// that was never there.
    pub(crate) fn executable_capability_ids(&self) -> BTreeSet<String> {
        let mut ids: BTreeSet<String> = self
            .runtime
            .published
            .iter()
            .map(|reference| reference.id.as_str().to_owned())
            .collect();
        ids.extend(self.runtime.discovery.unprompted_ids());
        ids
    }

    pub(crate) fn is_run(&self) -> bool {
        crate::runtime::decision_accounting::is_run(self)
    }

    pub(crate) const fn pending_approval_id(&self) -> Option<DecisionId> {
        None
    }
}

/// Delegation to the R25 owner in `runtime::ingress_recovery` (until Part 2).
impl TurnDecisionService {
    pub(crate) async fn reconcile_child_telemetry(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        lifetime: &Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        owner: &Arc<crate::runtime::turn_cleanup::CleanupTracker>,
        deadline: Instant,
    ) -> anyhow::Result<crate::runtime::ingress_recovery::child_telemetry::ChildRecoveryReport> {
        crate::runtime::ingress_recovery::child_telemetry::reconcile_child_telemetry(
            self, scope, lifetime, owner, deadline,
        )
        .await
    }
}

/// Delegations to the R25 owner in `runtime::decision_accounting` (until Part 2).
impl TurnDecisionService {
    pub(crate) fn auxiliary_accounting(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        purpose: crate::runtime::auxiliary_call::AuxiliaryPurpose,
    ) -> anyhow::Result<Arc<crate::runtime::auxiliary_call::AuxiliaryAccounting>> {
        crate::runtime::decision_accounting::auxiliary::auxiliary_accounting(self, proof, context, purpose)
    }

    pub(crate) fn smart_group_accounting(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
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

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentDiscoveryError {
    #[error(transparent)]
    Origin(#[from] TurnOriginMismatch),
    #[error(transparent)]
    Model(#[from] TurnModelError),
    #[error(transparent)]
    Parity(#[from] ParityError),
    #[error(transparent)]
    Selected(#[from] DiscoveryIssueError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentRetrievalError {
    #[error(transparent)]
    Origin(#[from] TurnOriginMismatch),
    #[error(transparent)]
    Model(#[from] TurnModelError),
    #[error(transparent)]
    Parity(#[from] ParityError),
    #[error(transparent)]
    Selected(#[from] RetrievalIssueError),
}

impl TurnDecisionContext {
    /// The execution-owned runtime context of this turn.
    pub(crate) const fn runtime(&self) -> &Arc<crate::runtime::turn_context::TurnRuntimeContext> {
        &self.runtime
    }
}

impl TurnDecisionService {
    /// Disabled decisions still constrain execution, but cannot authorize a
    /// decision-store write. Enabled decisions retain strict receipt checks.
    /// Body in `runtime::turn_terminal::settlement` (R25); delegates until Part 2.
    pub(crate) fn terminal_settlement_ref(
        &self,
        context: Arc<TurnDecisionContext>,
    ) -> Result<crate::agent::terminal::DecisionSettlementRef, SettlementRefUnavailable> {
        super::turn_terminal::settlement::terminal_settlement_ref(self, context)
    }

    /// The execution-owned terminal binding of a turn this service opened
    /// (R29). Body in `runtime::turn_terminal::settlement`.
    pub(crate) fn terminal_binding(
        &self,
        context: &TurnDecisionContext,
    ) -> Result<crate::agent::terminal::TerminalBinding, SettlementRefUnavailable> {
        super::turn_terminal::settlement::terminal_binding(self, context)
    }

    /// Issue a terminal link only after this service returned a recordable
    /// decision. The caller cannot choose its decision id or writer scope.
    pub(crate) fn settlement_ref(
        &self,
        context: Arc<TurnDecisionContext>,
    ) -> Result<crate::agent::terminal::DecisionSettlementRef, SettlementRefUnavailable> {
        super::turn_terminal::settlement::settlement_ref(self, context)
    }

    /// The publication-scoped runtime holder shared with every opened turn.
    pub(crate) const fn runtime_holder(&self) -> &Arc<crate::runtime::turn_context::TurnRuntimeHolder> {
        &self.runtime
    }

    pub(super) fn has_decision_work(&self) -> bool {
        self.generation.effective.causal_tree.enabled
    }

    pub(super) async fn drain_decision_work(&self, deadline: Instant) -> bool {
        // Advisory telemetry is closed first so its owners release before the recovery drains,
        // but a late telemetry drain only warns: catalog and causal writes stay authoritative.
        self.advisory.drain_or_warn(deadline).await;
        true
    }

    #[cfg(test)]
    pub(crate) async fn rejoin_terminal(&self, terminal_id: &str, owner_id: &str) -> anyhow::Result<Enqueue> {
        self.runtime
            .recovery_scope()
            .rejoin_terminal(terminal_id, owner_id)
            .await
    }

    pub(crate) fn status(&self) -> DecisionPlaneStatus {
        crate::runtime::ingress_recovery::status::plane_status(
            self.factory.status(),
            crate::runtime::ingress_recovery::status::JournalAttachment {
                catalog_generation: *self.catalog.generation(),
                store_mode: self.store.mode(),
                durable_key: self.durable_key,
                recordable: self.recordable,
            },
        )
    }

    pub(crate) fn prompt_memory_loader(&self) -> Option<&dyn crate::agent::memory_loader::MemoryLoader> {
        self.publication_scope().prompt_memory_loader()
    }

    /// Build the turn context, G6 slice and SendLimits without a prediction
    /// port or a directive from selection. Engine-on advice is projected
    /// separately through `DecisionAdvisory`. The turn engine stamps the
    /// call budget from this context.
    pub(crate) async fn open_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        self.open_turn_context_observed(proof, kind, cancellation, None, None)
            .await
    }

    /// Segment admission uses the same builder as a first turn, with the
    /// previous segment's verified observation folded into the trusted state.
    pub(in crate::runtime) async fn open_segment_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        observation: Option<&segment_observation::SegmentObservation>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        self.open_turn_context_observed(proof, AgentTurnKind::SingleShot, cancellation, observation, None)
            .await
    }

    pub(in crate::runtime) async fn open_turn_context_observed(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
        observation: Option<&segment_observation::SegmentObservation>,
        resume: Option<AdmittedDecisionResume>,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        if cancellation.is_cancelled() {
            return Err(AgentDecisionError::Cancelled);
        }
        let event_namespace = self
            .store
            .workspace_id()
            .unwrap_or_else(|| Arc::from(self.generation.effective.workspace_dir.to_string_lossy().as_ref()));
        let event_memory = super::causal_events::routes::register(
            &event_namespace,
            proof.workspace_id(),
            proof.run_id(),
            Arc::clone(&self.memory),
            self.generation.id.0,
            &self.generation.source_revision.fingerprint_sha256,
        )
        .map_err(AgentDecisionError::EventMemory)?;
        let model = self.resolve_agent_model(proof)?;
        super::ingress_recovery::service_ready(self)
            .await
            .map_err(AgentDecisionError::IngressRecovery)?;
        let budget = self.agent_budget_snapshot(proof)?;
        let limits = self.send_limits(proof)?;
        let deadline = crate::runtime::logical_clock::now()
            .checked_add(Duration::from_millis(budget.budgets.latency_ms))
            .ok_or(AgentDecisionError::Deadline)?;
        let owner_filesystem = self
            .console_filesystem(proof)
            .await
            .map_err(AgentDecisionError::PendingApproval)?;
        let catalog = self.catalog_view();
        let model_ref: Arc<str> = Arc::from(format!("{}:{}", model.provider_label(), model.model()));
        let effective_exclusions = effective_agent_exclusions(
            &self.generation.effective,
            &self.registry,
            &catalog,
            model.model(),
            &self.policy_version,
            &self.state_key,
        );
        let g6_slice = budget.budgets.tokens;
        let resume_evidence = resume
            .as_ref()
            .map(|resume| resume.gate_evidence(self, proof))
            .transpose()
            .map_err(AgentDecisionError::PendingApproval)?;
        let trusted = TrustedState::from_runtime(TrustedParts {
            workspace_id: Arc::from(proof.workspace_id()),
            owner_id: Some(Arc::from(proof.owner_id())),
            session_key: Some(Arc::from(proof.session_key())),
            run_id: Some(Arc::from(proof.run_id())),
            task_id: proof.task_id().map(Arc::from),
            channel: None,
            source_event_id: proof.source_event_id().map(Arc::from),
            causation_event_id: resume_evidence
                .as_ref()
                .map(|evidence| Arc::clone(evidence.causation())),
            surface: proof.surface(),
            side_effect_mode: match self.security.autonomy {
                crate::security::AutonomyLevel::ReadOnly => {
                    crate::tools::capability::turn_state::SideEffectMode::ReadOnly
                }
                crate::security::AutonomyLevel::Supervised => {
                    crate::tools::capability::turn_state::SideEffectMode::ApprovalRequired
                }
                crate::security::AutonomyLevel::Full => {
                    crate::tools::capability::turn_state::SideEffectMode::GuardedWrite
                }
            },
            exclusions: effective_exclusions.names,
            model_admission: effective_exclusions.model_admission,
            constraints: {
                let mut constraints = Vec::new();
                if proof.owner_isolated() {
                    constraints.push(crate::tools::capability::turn_state::ConstraintCode::new(
                        "owner_isolated",
                    ));
                }
                if resume_evidence.is_some() {
                    constraints.push(crate::tools::capability::turn_state::ConstraintCode::new(
                        "resume_exact_call",
                    ));
                }
                if proof.intercepts_writes() {
                    constraints.push(crate::tools::capability::turn_state::ConstraintCode::new(
                        "intercepts_writes",
                    ));
                }
                if owner_filesystem.is_some() {
                    constraints.push(crate::tools::capability::turn_state::ConstraintCode::new(
                        "owner_private_filesystem_v1",
                    ));
                }
                constraints
            },
            approval_pending: false,
            approval_grants: resume_evidence
                .as_ref()
                .map_or_else(Vec::new, |evidence| evidence.grants().to_vec()),
            budgets: TurnBudgets {
                tokens: g6_slice,
                latency_ms: budget.budgets.latency_ms,
                micro_usd: limits.remaining_micro_usd,
            },
            delivery: DeliveryConstraints {
                interactive: kind == AgentTurnKind::Repl || proof.approval_review_adapter().is_some(),
                can_ask_question: kind == AgentTurnKind::Repl || proof.approval_review_adapter().is_some(),
                approval_channel: proof.approval_review_adapter().is_some(),
            },
            catalog_generation: catalog.generation(),
            policy_version: Arc::clone(&self.policy_version),
            resolved_model: Arc::clone(&model_ref),
            resolved_provider: Arc::from(model.provider_label()),
            resolved_model_id: Arc::from(model.model()),
        });
        let projected_input = crate::multimodal::project_image_markers_for_routing(proof.routing_input()).into_owned();
        let goal = resume_evidence.as_ref().map_or_else(
            || projected_input.clone(),
            |evidence| {
                let original = crate::multimodal::project_image_markers_for_routing(evidence.original_request());
                format!(
                    "Resume the exact approved tool call for the original request below. Current hard gates and the single-use execution claim still apply. Original user request (data): {original}"
                )
            },
        );
        let decision_id = DecisionId::new_runtime();
        let trusted_parts = trusted.parts().clone();
        let budgets = trusted_parts.budgets;
        let observed = segment_observation::initial_observations(observation, deadline, cancellation)?;
        let routing_input = RawRoutingInput::new(projected_input);
        let goal = RawRoutingInput::new(goal);
        let state = CausalState {
            trusted,
            observed: observed.clone(),
            derived: DerivedState::default(),
            has_conversation_history: proof.has_conversation_history(),
            routing_input: routing_input.clone(),
            goal: goal.clone(),
            lineage: HopLineage::First,
            built_at_ms: u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
        };
        let mut engine = EngineDecision::without_prediction(state, deadline, decision_id);
        if let Some(issued) =
            self.outgoing_delegation(proof, &engine.state, decision_id, &DiscoveredRefs::empty(), &catalog)
        {
            engine.outgoing_child = Some(
                crate::tools::capability::child::ChildCapabilityManifest::derive_strict_subset(
                    &issued.parent,
                    &issued.requested,
                )
                .map_err(crate::causal_tree::decision_engine::EngineError::from)?,
            );
            engine.outgoing_parent_allowed = Some(issued.parent.allowed);
        }
        let admission_facts = crate::tools::capability::admission_facts::AdmissionFactsV1 {
            policy: Arc::clone(&self.security),
            owner_id: Arc::from(proof.owner_id()),
            sender: Arc::from(proof.policy_sender()),
            policy_channel: Arc::from(proof.policy_channel()),
            chat_type: Arc::from(proof.policy_chat_type()),
            model_ref: Arc::clone(&model_ref),
            model_can_chat: true,
            model_can_call_tools: self.model_can_call_tools(proof),
            child_scope: proof.child_scope().map(|scope| {
                let (manifest, constraints) = scope.gate_inputs();
                (Arc::new(manifest.clone()), Arc::new(constraints.clone()))
            }),
            child_discovery: proof.child_scope().is_some()
                && self.execution.current_descriptor("capability_search").is_some(),
        };
        // The published surface is an execution rule over the pinned scope,
        // the admission facts and the catalog (R28).
        let published = crate::tools::surface_owner::publication::PublicationRules {
            trusted: &trusted_parts,
            facts: &admission_facts,
            catalog: &catalog,
        }
        .surface(resume_evidence.as_ref(), engine.outgoing_child.is_some(), &|bytes| {
            crate::cost::estimate::estimate_cost(&model, &self.generation.effective.cost, bytes)
        });
        let outgoing_delegation = engine
            .outgoing_child
            .clone()
            .zip(engine.outgoing_parent_allowed.clone())
            .map(
                |(manifest, parent_allowed)| crate::runtime::turn_context::OutgoingDelegation {
                    manifest,
                    parent_allowed,
                },
            );
        // The turn's one accounting scope, created at opening. Without the
        // prediction hop nothing has been charged to it yet.
        let accounting = opening::accounting_scope(self, proof, budgets, deadline);
        let resume = resume.map(Arc::new);
        let runtime = Arc::new(crate::runtime::turn_context::TurnRuntimeContext {
            decision_id,
            trusted: trusted_parts,
            routing_input,
            goal,
            has_conversation_history: proof.has_conversation_history(),
            observed,
            budgets,
            accounting,
            send_limits: limits,
            catalog: Arc::clone(&self.catalog),
            publication_policy: self.publication_policy.clone(),
            catalog_generation: catalog.generation(),
            admission: admission_facts,
            published,
            discovery: crate::tools::capability::search_tool::DiscoveryState::default(),
            outgoing_delegation,
            resume: resume.clone(),
            resume_evidence: resume_evidence.map(Arc::new),
            state_key: Arc::clone(&self.state_key),
            holder: Arc::clone(&self.runtime),
            model_catalog: self.provider_catalog.release_pin(),
            main_parameters: std::sync::OnceLock::new(),
            recursive_allowance: self.runtime.recursive_allowances().retain(proof),
        });
        Ok(Arc::new(TurnDecisionContext {
            _event_memory: event_memory,
            runtime,
            resume,
            owner_filesystem,
            source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
            service_id: self.service_id,
            record_admitted: AtomicBool::new(false),
            execution_trace: std::sync::OnceLock::new(),
            execution_closed: Arc::new(AtomicBool::new(false)),
            collected_approval_result: std::sync::OnceLock::new(),
            engine,
            hop_ids: vec![decision_id],
            prior_hops: Vec::new(),
            main_compaction: budget.main_compaction,
        }))
    }

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
}

#[cfg(test)]
pub(crate) use tests::with_worker_probe_manifest;

#[cfg(test)]
pub(crate) mod tests;
