//! One pinned runtime generation for causal turn decisions.
//!
//! The constructor receives the already finalized runtime catalog inputs.
//! Rebuilding them from configuration alone would drop live MCP, Skill, hook
//! and node state, so a source-generation mismatch rejects the service.

#[cfg(test)]
mod ordinary_child_fixture;
#[cfg(test)]
pub(crate) use ordinary_child_fixture::{
    with_inherited_scored_prediction, with_scored_prediction, with_scored_publication,
};

#[cfg(test)]
use crate::tools::capability::turn_state::ConstraintCode;

#[cfg(test)]
pub(crate) mod evidence_fixture;
#[cfg(test)]
pub(crate) mod selected_loader_fixture;

mod accounting;
mod auxiliary;
mod build_error;
mod issued;
mod request_parameters;
mod store_preparation;

pub(crate) use build_error::TurnServiceBuildError;

mod approval;
mod call_approval;
mod child_recovery;
#[cfg(test)]
pub(crate) use child_recovery::child_recovery_gate;
mod delegation;
mod discovery;
use super::child_source::recursive_allowance;
mod execution;
mod recursive_budget;
pub(super) mod segment_observation;
pub(crate) use execution::{OperatorChildPublication, OrdinaryChildPublication, OrdinaryProcessLaunch};
mod owner_filesystem;
mod pending_approval;
pub(crate) use pending_approval::AdmittedDecisionResume;
pub(crate) use pending_approval::DecisionApprovalView;
pub(crate) use pending_approval::ResumeEvidenceWitness;
#[path = "turn_decision/routing/replay_facts.rs"]
mod router_replay_facts;
pub(crate) use router_replay_facts::provider_binding as router_provider_binding;
#[cfg(feature = "llm-router")]
mod routing;

#[cfg(feature = "llm-router")]
pub(crate) use routing::commit_training_owned;

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
use crate::causal_tree::branch::{CandidateSpec, CausalCandidate};
use crate::causal_tree::candidate_generator::CandidateSet;
use crate::causal_tree::candidate_generator::DiscoveredRefs;
use crate::causal_tree::decision_engine::{DiscoveryIssueError, EngineDecision, EngineError, RetrievalIssueError};
#[cfg(test)]
use crate::tools::capability::contract::CapabilityTarget;
use crate::tools::capability::contract::{CapabilityRef, CatalogGeneration, CausalActionKind};

use crate::causal_tree::directive::{DirectiveError, TurnExecutionDirective};
use crate::causal_tree::gates::{GateInputs, GateOutcome, HardGates};
use crate::causal_tree::outcome::{DecisionRecordBuilder, OutcomeCorrelator, RecordBuildError, TerminalFacts};
use crate::causal_tree::state::{CausalState, DerivedState, HopLineage, RawRoutingInput};
#[cfg(test)]
use crate::causal_tree::state::{ObservedStepKind, ObservedStepStatus};
use crate::config::ConfigGeneration;
use crate::decision::factory::{
    BackendPlaneStatus, DecisionBackendFactory, DecisionTransports, FactoryBuildError, FactoryDeps,
};
use crate::memory::Memory;
use crate::observability::causal_event::{
    CausalEvent, CausalEventFields, CausalEventKind, CausalEventSink, CausalLineage,
};
use crate::providers::catalog::{CatalogView as ProviderCatalogView, Requirement, ResolvedModel, UnknownPolicy};
use crate::runtime::decision_journal::store::{
    CausalDecisionStore, DecisionScope, Enqueue, Retention, StoreError, StoreMode,
};
use crate::security::SecurityPolicy;
use crate::security::decision_id::DecisionId;
use crate::security::runtime_key::{DecisionKey, KeyDomain};
use crate::tools::capability::catalog::{
    CapabilityCatalog, CatalogError, CatalogScope, CatalogSources, CatalogView, Exposure, ParityError, SurfacePolicy,
};
use crate::tools::capability::turn_state::{
    DeliveryConstraints, TrustedParts, TrustedState, TurnBudgets, TurnSurfaceKind,
};
use crate::tools::execution::ToolExecutionService;
use crate::tools::traits::Tool;

/// Runtime-owned snapshots that must all describe the same generation.
/// A caller supplies the finalized registry and its execution service,
/// rather than letting the decision plane infer capabilities from config.
pub(crate) struct TurnServiceBuild<'a> {
    pub parent_grant: Option<Arc<super::child_source::IssuedChildGrant>>,
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
pub(crate) struct TurnLoopRuntime {
    pub(crate) hooks: Arc<crate::hooks::HookManager>,
    pub(crate) observer: Arc<dyn crate::observability::Observer>,
}

/// Proof that child execution retains the exact policy of an admitted service.
/// Only this module and its delegation producer can construct its fields.
pub(super) struct ChildExecutionPolicy {
    security: Arc<SecurityPolicy>,
}

impl ChildExecutionPolicy {
    pub(super) fn matches(&self, security: &Arc<SecurityPolicy>) -> bool {
        Arc::ptr_eq(&self.security, security)
    }
}

pub(crate) struct TurnDecisionService {
    recovery: Arc<super::ingress_recovery::RecoveryScope>,
    advisory: crate::runtime::decision_advisory::DecisionAdvisory,
    child_recovery: child_recovery::ChildRecoveryState,
    recursive_allowances: recursive_allowance::RunAllowances,
    memory_loader: Option<Arc<dyn crate::agent::memory_loader::MemoryLoader>>,
    loop_runtime: Option<TurnLoopRuntime>,
    publication_policy: SurfacePolicy,
    ordinary_parent: Option<OrdinaryChildPublication>,
    ordinary_money: Option<Arc<std::sync::atomic::AtomicU64>>,
    operator_task: Option<Arc<super::operator_spawn::task::OperatorTaskAuthority>>,
    operator_process: Option<Arc<super::child_source::operator_process::OperatorProcessAuthority>>,
    ordinary_task: Option<Arc<super::child_source::ordinary_task::OrdinaryTaskAuthority>>,
    ordinary_process: Option<Arc<super::child_source::ordinary_process::OrdinaryProcessAuthority>>,
    parent_grant: Option<Arc<super::child_source::IssuedChildGrant>>,
    service_id: uuid::Uuid,
    surface: TurnSurfaceKind,
    generation: Arc<ConfigGeneration>,
    catalog: Arc<CapabilityCatalog>,
    provider_catalog: ProviderCatalogView,
    factory: DecisionBackendFactory,
    events: Arc<dyn CausalEventSink>,
    state_key: Arc<DecisionKey>,
    policy_version: Arc<str>,
    durable_key: bool,
    recordable: bool,
    registry: Arc<Vec<Box<dyn Tool>>>,
    execution: Arc<ToolExecutionService>,
    memory: Arc<dyn Memory>,
    security: Arc<SecurityPolicy>,
    store: Arc<CausalDecisionStore>,
    durable_events: Option<Arc<super::causal_events::DurableCausalEventSink>>,
}

impl TurnDecisionService {
    /// The turn's advisory decision driver.
    #[must_use]
    pub(crate) fn advisory(&self) -> &crate::runtime::decision_advisory::DecisionAdvisory {
        &self.advisory
    }

    #[must_use]
    pub(crate) fn memory(&self) -> &Arc<dyn Memory> {
        &self.memory
    }

    #[must_use]
    pub(crate) fn execution(&self) -> &ToolExecutionService {
        self.execution.as_ref()
    }

    #[must_use]
    pub(crate) fn catalog(&self) -> &CapabilityCatalog {
        self.catalog.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn prediction_port_calls(&self) -> usize {
        self.factory.prediction_port_calls()
            + self.advisory.factory().map_or(
                0,
                crate::decision::factory::DecisionBackendFactory::prediction_port_calls,
            )
    }

    #[must_use]
    pub(crate) const fn publication_policy(&self) -> &SurfacePolicy {
        &self.publication_policy
    }

    #[must_use]
    pub(crate) fn security(&self) -> &SecurityPolicy {
        self.security.as_ref()
    }

    #[must_use]
    pub(crate) fn tool_tiering(&self) -> &crate::config::ToolTieringConfig {
        &self.generation.effective.tool_tiering
    }

    #[must_use]
    pub(crate) const fn channel_surface(&self) -> bool {
        matches!(self.surface, TurnSurfaceKind::Channel)
    }

    /// Bind runtime adapter ceilings to the provider catalog pinned by this
    /// exact configuration generation.
    pub(crate) fn effective_provider_capabilities(
        &self,
        provider: &str,
        model: &str,
        adapter: crate::providers::ProviderCapabilities,
    ) -> crate::providers::ProviderCapabilities {
        self.provider_catalog
            .effective_provider_capabilities(provider, model, adapter)
    }

    /// Resolve model limits against the provider catalog pinned to this
    /// configuration generation. Runtime surfaces must not reconstruct a
    /// release-only view after admission has already selected stronger
    /// operator or discovery evidence.
    pub(crate) fn effective_compaction(
        &self,
        provider: &str,
        model: &str,
    ) -> crate::providers::context_window::EffectiveCompactionConfig {
        let config = &self.generation.effective;
        crate::providers::context_window::resolve_effective_compaction_config_with_catalog(
            &config.agent.compaction,
            provider,
            model,
            crate::providers::context_window::ContextWindowSources::from_config(config),
            &config.model_routes,
            &self.provider_catalog,
        )
    }
}

/// Read-only status of the exact generation attached to a runtime surface.
/// The factory's configured slots are distinct from the catalog and store
/// attachment. `backend.enabled` reports configuration, not remote admission;
/// callers must also inspect exclusions and recordability. None of these
/// fields confers execution authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecisionPlaneStatus {
    pub(crate) backend: BackendPlaneStatus,
    pub(crate) catalog_generation: CatalogGeneration,
    pub(crate) store_mode: StoreMode,
    pub(crate) durable_key: bool,
    pub(crate) recordable: bool,
}

impl TurnDecisionService {
    /// Attach runtime instances before the service is shared with any caller.
    pub(crate) fn with_loop_runtime(mut self: Arc<Self>, runtime: TurnLoopRuntime) -> anyhow::Result<Arc<Self>> {
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

    fn catalog_view(&self) -> CatalogView<'_> {
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
        self.recovery.drain(deadline).await && decision_drained
    }
}

/// The version binds the code's gate semantics to the exact policy snapshots
/// this service publishes. Maps are sorted before encoding; dynamic spend,
/// grants and child state belong to the turn snapshot instead.
fn pinned_policy_version(
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
        super::turn_cost::LATENCY_DEFAULT_VERSION,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("turn origin does not match the pinned decision service")]
pub(crate) struct TurnOriginMismatch;

/// A source-owned proof is still checked against this service's pinned
/// generation and surface. Neither a public envelope nor a caller-supplied
/// generation number can replace the proof.
pub(crate) fn validate_agent_turn_origin(
    generation: &ConfigGeneration,
    surface: TurnSurfaceKind,
    proof: &dyn TurnSourceProof,
) -> Result<(), TurnOriginMismatch> {
    if surface != proof.surface()
        || (surface == TurnSurfaceKind::Worker
            && proof.child_scope().is_none()
            && proof.operator_task_authority_id().is_none()
            && proof.operator_process_authority_id().is_none())
        || (surface != TurnSurfaceKind::Worker && proof.child_scope().is_some())
        || proof.config_generation_id() != generation.id.0
        || proof.config_source_revision() != generation.source_revision.fingerprint_sha256
        || proof.workspace_id() != generation.effective.workspace_dir.to_string_lossy()
    {
        return Err(TurnOriginMismatch);
    }
    Ok(())
}

/// Join a selected decision to the exact source-owned CLI turn before any
/// content read. Matching text and catalog generation do not establish the
/// owner, session, run, or event that supplied the decision's policy inputs.
pub(crate) fn validate_agent_retrieval_provenance(
    proof: &dyn TurnSourceProof,
    trusted: &TrustedParts,
    model_ref: &str,
    policy_version: &str,
    exclusions: &[Arc<str>],
    model_admission: Option<&dyn crate::tools::capability::turn_state::model_admission::ModelAdmissionView>,
) -> Result<(), TurnOriginMismatch> {
    if trusted.owner_isolated() != proof.owner_isolated()
        || trusted.surface != proof.surface()
        || trusted.channel.is_some()
        || trusted.workspace_id.as_ref() != proof.workspace_id()
        || trusted.owner_id.as_deref() != Some(proof.owner_id())
        || trusted.session_key.as_deref() != Some(proof.session_key())
        || trusted.run_id.as_deref() != Some(proof.run_id())
        || trusted.task_id.as_deref() != proof.task_id()
        || trusted.source_event_id.as_deref() != proof.source_event_id()
        || trusted.resolved_model.as_ref() != model_ref
        || trusted.policy_version.as_ref() != policy_version
        || trusted.exclusions != exclusions
        || trusted.model_admission.as_deref().map(|policy| policy.digest())
            != model_admission.map(|policy| policy.digest())
        || trusted
            .constraints
            .iter()
            .any(|code| code.as_str() == "intercepts_writes")
            != proof.intercepts_writes()
    {
        return Err(TurnOriginMismatch);
    }
    Ok(())
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

/// A marginal slice of the resolved Agent input window held for causal
/// decision work. The main turn must use `main_input_limit` at every
/// preflight and immediately before each provider request; merely computing
/// this value does not reserve context in a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecisionTokenReservation {
    pub(crate) decision_tokens: usize,
    pub(crate) main_input_limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("resolved Agent context window has no safe decision reserve")]
pub(crate) struct DecisionReserveUnavailable;

/// Budget facts for one authenticated Agent turn. The effective route window
/// and remaining money come from pinned runtime state, never an input object
/// supplied by a decision backend or caller.
pub(crate) struct AgentDecisionBudget {
    pub(crate) budgets: TurnBudgets,
    pub(crate) main_compaction: crate::config::AgentCompactionConfig,
}

pub(crate) struct SendLimits {
    pub(crate) response_reserve: usize,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentTurnKind {
    SingleShot,
    Repl,
}

/// The immutable result of one source-bound decision chain. Only the service
/// constructs it; later record and execution streams consume these facts.
pub(crate) struct TurnDecisionContext {
    _event_memory: Arc<super::causal_events::routes::EventMemory>,
    model_catalog: Option<Arc<crate::providers::catalog::ReleaseCatalog>>,
    owner_filesystem: Option<Arc<crate::tools::owner_filesystem::OwnerFilesystem>>,
    accounting: Option<Arc<crate::runtime::decision_accounting::AccountingScope>>,
    resume: Option<Arc<pending_approval::AdmittedDecisionResume>>,
    /// The execution-owned runtime context of this turn (R60).
    runtime: Arc<crate::runtime::turn_context::TurnRuntimeContext>,
    source_actor: (Arc<str>, Arc<str>),
    service_id: uuid::Uuid,
    record_admitted: AtomicBool,
    recursive_allowance: std::sync::OnceLock<recursive_allowance::ContextAllowance>,
    main_parameters: std::sync::OnceLock<crate::providers::parameter_capture::Capture>,
    execution_trace: std::sync::OnceLock<crate::tools::capability::execution_trace::ExecutionTrace>,
    execution_closed: Arc<AtomicBool>,

    collected_approval_result: std::sync::OnceLock<Box<pending_approval::CollectedApprovalResult>>,
    discovery_offers: std::sync::OnceLock<Arc<parking_lot::Mutex<discovery::DiscoveryOffers>>>,
    discovery_admissions: std::sync::OnceLock<parking_lot::Mutex<BTreeMap<String, discovery::DiscoveryAdmission>>>,
    engine: EngineDecision,
    /// Completed dependent hops retained for the recorder. Nothing is
    /// written until the whole chain returns successfully.
    prior_hops: Vec<EngineDecision>,
    directive: TurnExecutionDirective,
    main_compaction: crate::config::AgentCompactionConfig,
    hop_ids: Vec<DecisionId>,
    execution_refs: BTreeSet<CapabilityRef>,
}

/// Ids of the discovery admissions that can run without an approval. One that
/// still waits for approval may never have been attempted, so it says nothing
/// about whether a run could have called it.
fn unprompted_admission_ids(
    admissions: &BTreeMap<String, discovery::DiscoveryAdmission>,
) -> impl Iterator<Item = String> {
    admissions
        .values()
        .filter(|admission| !admission.requires_approval)
        .map(|admission| admission.reference.id.as_str().to_owned())
}

/// Applied-event admission issued by the owning service after terminal commit
/// or reconstructed from an authenticated historical terminal replay.
pub(super) struct VerifiedDirectiveApplication {
    event: CausalEvent,
    idempotency_key: String,
    original_generation: Option<(u64, Arc<str>)>,
}

impl VerifiedDirectiveApplication {
    /// Reconstruct admission from authenticated source and decision evidence.
    pub(super) fn from_replay(replay: &crate::agent::terminal::VerifiedTerminalReplay) -> anyhow::Result<Self> {
        let source = replay.source();
        let decision = replay.decision();
        let original_generation = (
            source
                .config_generation_id
                .context("original source generation is missing")?,
            Arc::from(
                source
                    .config_source_revision
                    .as_deref()
                    .context("original source revision is missing")?,
            ),
        );
        Ok(Self {
            event: CausalEvent {
                kind: CausalEventKind::DirectiveApplied,
                lineage: CausalLineage {
                    workspace_id: Arc::from(source.workspace_id.as_str()),
                    owner_id: source.owner_id.as_deref().map(Arc::from),
                    session_key: source.session_key.as_deref().map(Arc::from),
                    run_id: source.run_id.as_deref().map(Arc::from),
                    source_event_id: Some(Arc::from(source.event_id.as_str())),
                    causation_event_id: Some(Arc::from(replay.terminal_event_id())),
                    decision_id: Some(Arc::from(decision.decision_id.to_string())),
                    ..CausalLineage::default()
                },
                fields: CausalEventFields {
                    item_count: Some(1),
                    ..CausalEventFields::default()
                },
            },
            idempotency_key: format!("causal:{}:directive-applied", decision.decision_id),
            original_generation: Some(original_generation),
        })
    }

    #[cfg(test)]
    pub(super) const fn fixture(event: CausalEvent, idempotency_key: String) -> Self {
        Self {
            event,
            idempotency_key,
            original_generation: None,
        }
    }

    pub(super) fn into_parts(self) -> (CausalEvent, String, Option<(u64, Arc<str>)>) {
        (self.event, self.idempotency_key, self.original_generation)
    }
}

/// The bounded direct core available while the serving model decides whether
/// to answer or call a tool. These references are publication authority only:
/// binding and dispatch recheck the live catalog, source, schema, security and
/// approval policy. Delegators additionally require the sealed child manifest
/// retained by this exact decision.
fn exact_delegator_ref(decision: &EngineDecision, reference: &CapabilityRef) -> bool {
    let (candidates, exact_mentions) = match &decision.candidates {
        CandidateSet::Legal {
            candidates,
            exact_mentions,
            ..
        } => (candidates, exact_mentions),
        CandidateSet::NoLegalPath { .. } => return false,
    };
    candidates.iter().any(|candidate| {
        candidate.action() == CausalActionKind::Delegate
            && exact_mentions.contains(candidate.candidate_id())
            && candidate.required_capabilities() == std::slice::from_ref(reference)
    })
}

pub(super) const fn sealed_direct_delegator(decision: &EngineDecision) -> bool {
    decision.prediction.is_none() && decision.outgoing_child.is_some()
}

fn direct_answer_refs(
    decision: &EngineDecision,
    catalog: &CatalogView<'_>,
    gates: &GateInputs<'_>,
    selected_discovery_refs: &BTreeSet<CapabilityRef>,
    estimate: &dyn Fn(usize) -> Option<crate::tools::capability::contract::CostEstimate>,
) -> BTreeSet<CapabilityRef> {
    if !gates.model_can_call_tools {
        return BTreeSet::new();
    }
    let trusted = decision.state.trusted.parts();
    catalog
        .iter_entries()
        .filter_map(|entry| {
            let delegation = matches!(entry.public_name.as_str(), "delegate" | "sessions_spawn");
            let reference = CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            };
            let directly_exposed = matches!(entry.exposure, Exposure::Direct);
            let exact_delegation = delegation && exact_delegator_ref(decision, &reference);
            // Direct-surface mint already sealed a child manifest for a named
            // or Direct delegator. Publish that entry so the serving model can
            // call it without a hop-derived Delegate candidate.
            let minted_delegator =
                delegation && sealed_direct_delegator(decision) && trusted.surface != TurnSurfaceKind::Interactive;
            // A selected local search is already a bounded, turn-scoped
            // relevance decision. Carry that exact schema into the serving
            // model's final snapshot when the following hop answers, while
            // retaining the same publication gates as the direct core.
            let selected_discovery = !delegation
                && matches!(entry.exposure, Exposure::Searchable)
                && selected_discovery_refs.contains(&reference);
            if !directly_exposed && !exact_delegation && !selected_discovery && !minted_delegator {
                return None;
            }
            if delegation && decision.outgoing_child.is_none() && !directly_exposed {
                return None;
            }
            if trusted.constraints.iter().any(|code| {
                matches!(code.as_str(), "resume_exact_call" | "tools_off")
                    || code.as_str() == "intercepts_writes" && entry.plan_intercepted()
            }) || trusted.side_effect_mode == crate::tools::capability::turn_state::SideEffectMode::ReadOnly
                && entry.effect > crate::tools::capability::contract::SideEffectClass::ReadOnly
            {
                return None;
            }
            let candidate = CausalCandidate::new(
                if delegation {
                    CausalActionKind::Delegate
                } else {
                    CausalActionKind::UseTool
                },
                entry.family,
                CandidateSpec {
                    namespace: None,
                    required_capabilities: vec![reference.clone()],
                    side_effect: entry.effect,
                    approval: entry.approval.clone(),
                    estimated_cost: gates.child_scope.and_then(|_| estimate(entry.schema_bytes)),
                    estimated_latency_ms: None,
                },
            );
            let publication = HardGates::check_capability_publication(&decision.state, catalog, &candidate, gates);
            if matches!(publication, GateOutcome::Blocked { .. }) {
                return None;
            }
            Some(reference)
        })
        .collect()
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
        if let Some(trace) = self.execution_trace.get() {
            trace.validate_terminal_status(status, outcome)
        } else {
            anyhow::ensure!(
                status != crate::agent::terminal::TurnTerminalStatus::Silent,
                "silent terminal has no actual execution trace"
            );
            Ok(())
        }
    }

    pub(crate) fn terminal_execution_trace(&self) -> crate::tools::capability::execution_trace::ExecutionTraceSnapshot {
        self.execution_closed.store(true, Ordering::Release);
        if let Some(offers) = self.discovery_offers.get() {
            offers.lock().clear();
        }
        if let Some(admissions) = self.discovery_admissions.get() {
            // Wait for an admission that entered before closure, so terminal
            // evidence sees its final metadata. Later admissions check closed.
            drop(admissions.lock());
        }
        self.execution_trace.get_or_init(Default::default).seal()
    }

    #[cfg(test)]
    pub(crate) const fn engine(&self) -> &EngineDecision {
        &self.engine
    }

    pub(crate) const fn directive(&self) -> &TurnExecutionDirective {
        &self.directive
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

    pub(crate) fn terminal_outcome_action(&self) -> Option<crate::tools::capability::contract::CausalActionKind> {
        use crate::tools::capability::contract::CausalActionKind;
        match &self.directive {
            TurnExecutionDirective::AnswerDirectly => Some(CausalActionKind::Answer),
            TurnExecutionDirective::Execute { .. } => self
                .execution_trace
                .get()
                .is_some_and(crate::tools::capability::execution_trace::ExecutionTrace::is_sealed)
                .then_some(CausalActionKind::UseTool),
            TurnExecutionDirective::Delegate { .. } => Some(CausalActionKind::Delegate),
            _ => None,
        }
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

    pub(crate) const fn execution_refs(&self) -> &BTreeSet<CapabilityRef> {
        &self.execution_refs
    }

    /// Canonical ids of every capability this turn could execute: the issued
    /// references and those admitted by discovery without an approval.
    /// Read-only; the skill lifecycle uses it to tell a skipped tool from one
    /// that was never there.
    pub(crate) fn executable_capability_ids(&self) -> BTreeSet<String> {
        let mut ids: BTreeSet<String> = self
            .execution_refs
            .iter()
            .map(|reference| reference.id.as_str().to_owned())
            .collect();
        if let Some(admissions) = self.discovery_admissions.get() {
            ids.extend(unprompted_admission_ids(&admissions.lock()));
        }
        ids
    }

    pub(crate) fn is_run(&self) -> Result<bool, DirectiveError> {
        Ok(!self.execution_closed.load(Ordering::Acquire))
    }

    pub(crate) const fn pending_approval_id(&self) -> Option<DecisionId> {
        None
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentDecisionError {
    #[error("decision cancelled")]
    Cancelled,
    #[error(transparent)]
    Model(#[from] TurnModelError),
    #[error(transparent)]
    Budget(#[from] AgentDecisionBudgetError),
    #[error(transparent)]
    Parity(#[from] ParityError),
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Directive(#[from] DirectiveError),
    #[error("decision deadline cannot be represented")]
    Deadline,
    #[error(transparent)]
    Retrieval(#[from] AgentRetrievalError),
    #[error(transparent)]
    Discovery(#[from] AgentDiscoveryError),
    #[error(transparent)]
    Record(#[from] RecordBuildError),
    #[error("recordable decision store has no writer")]
    RecordWriterUnavailable,
    #[error("decision evidence could not be durably recorded")]
    DecisionEvidence(#[source] anyhow::Error),
    #[error("delegation parent could not be durably verified")]
    DelegationStorage(#[source] anyhow::Error),
    #[error("ordinary delegation scope could not be verified")]
    DelegationScope(#[source] anyhow::Error),
    #[error("decision event memory could not be bound to its source")]
    EventMemory(#[source] anyhow::Error),
    #[error("durable ingress recovery is not ready")]
    IngressRecovery(#[source] anyhow::Error),

    #[error("previous segment observation could not be verified")]
    SegmentObservation(#[source] anyhow::Error),
    #[error("pending approval could not be persisted")]
    PendingApproval(#[source] anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("decision context is not recordable by this pinned service")]
pub(crate) struct SettlementRefUnavailable;

/// Only this module can authorize construction of a terminal decision link.
pub(crate) struct SettlementIssueToken(());

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

impl DecisionTokenReservation {
    /// Use the selected route's effective compaction window, after its
    /// response reserve. The eighth-window slice funds the complete bounded
    /// structured question set; a sixteenth/4096 slice was smaller than real
    /// default-registry Jev requests. The marginal slice is also capped by the
    /// configured ratio to that baseline window. Very small windows and a
    /// ratio that cannot fund one token fail closed instead of silently
    /// authorizing a zero-token decision budget.
    pub(crate) fn from_effective_compaction(
        config: &crate::config::AgentCompactionConfig,
        extra_token_ratio_limit: f32,
    ) -> Result<Self, DecisionReserveUnavailable> {
        let available = config
            .max_context_tokens
            .checked_sub(config.reserve_tokens)
            .ok_or(DecisionReserveUnavailable)?;
        if !extra_token_ratio_limit.is_finite() || extra_token_ratio_limit < 0.0 {
            return Err(DecisionReserveUnavailable);
        }
        let ratio_limit = ((available as f64) * f64::from(extra_token_ratio_limit)).floor();
        let ratio_limit = if ratio_limit >= usize::MAX as f64 {
            usize::MAX
        } else {
            ratio_limit as usize
        };
        let decision_tokens = (available / 8).min(8192).min(ratio_limit);
        let main_input_limit = available
            .checked_sub(decision_tokens)
            .ok_or(DecisionReserveUnavailable)?;
        if decision_tokens == 0 || main_input_limit == 0 {
            return Err(DecisionReserveUnavailable);
        }
        Ok(Self {
            decision_tokens,
            main_input_limit,
        })
    }

    /// Carry this turn's slice through the existing compaction preflight.
    /// The returned clone must be used for every iteration of that turn; the
    /// source-owned turn origin checks bind the selected route and generation.
    /// This arithmetic only binds the window and response reserve.
    pub(crate) fn reserved_compaction(
        self,
        effective: &crate::config::AgentCompactionConfig,
        extra_token_ratio_limit: f32,
    ) -> Result<crate::config::AgentCompactionConfig, DecisionReserveUnavailable> {
        let expected = Self::from_effective_compaction(effective, extra_token_ratio_limit)?;
        if self != expected {
            return Err(DecisionReserveUnavailable);
        }
        let mut reserved = effective.clone();
        reserved.reserve_tokens = reserved
            .reserve_tokens
            .checked_add(self.decision_tokens)
            .ok_or(DecisionReserveUnavailable)?;
        if reserved.max_context_tokens.checked_sub(reserved.reserve_tokens) != Some(self.main_input_limit) {
            return Err(DecisionReserveUnavailable);
        }
        Ok(reserved)
    }
}

impl TurnDecisionContext {
    /// The execution-owned runtime context of this turn.
    pub(crate) fn runtime(&self) -> &crate::runtime::turn_context::TurnRuntimeContext {
        &self.runtime
    }

    /// Remaining G6 tokens: the decision slice after every narrowing the engine
    /// and the live decision-attempt accounting have already applied.
    pub(crate) fn remaining_g6_tokens(&self) -> u64 {
        let trusted = self.engine.state.trusted.parts().budgets.tokens;
        self.accounting
            .as_ref()
            .map_or(trusted, |scope| trusted.min(scope.remaining_budgets().tokens))
    }
}

impl TurnDecisionService {
    /// Disabled decisions still constrain execution, but cannot authorize a
    /// decision-store write. Enabled decisions retain strict receipt checks.
    pub(crate) fn terminal_settlement_ref(
        &self,
        context: Arc<TurnDecisionContext>,
    ) -> Result<crate::agent::terminal::DecisionSettlementRef, SettlementRefUnavailable> {
        if context.service_id != self.service_id {
            return Err(SettlementRefUnavailable);
        }
        if !self.generation.effective.causal_tree.enabled {
            let trusted = context
                .audit_hops()
                .last()
                .ok_or(SettlementRefUnavailable)?
                .state
                .trusted
                .parts();
            if trusted.surface != self.surface
                || trusted.workspace_id.as_ref() != self.generation.effective.workspace_dir.to_string_lossy()
                || trusted.catalog_generation != *self.catalog.generation()
                || trusted.policy_version != self.policy_version
                || context.hop_ids().last().copied() != Some(context.decision_id())
            {
                return Err(SettlementRefUnavailable);
            }
            return Ok(crate::agent::terminal::DecisionSettlementRef::issued_disabled(
                context,
                self.generation.effective.workspace_dir.clone(),
                self.generation.effective.cost.clone(),
                Arc::clone(&self.memory),
                SettlementIssueToken(()),
            ));
        }
        self.settlement_ref(context)
    }

    /// Issue a terminal link only after this service returned a recordable
    /// decision. The caller cannot choose its decision id or writer scope.
    pub(crate) fn settlement_ref(
        &self,
        context: Arc<TurnDecisionContext>,
    ) -> Result<crate::agent::terminal::DecisionSettlementRef, SettlementRefUnavailable> {
        let trusted = context
            .audit_hops()
            .last()
            .ok_or(SettlementRefUnavailable)?
            .state
            .trusted
            .parts();
        if !self.generation.effective.causal_tree.enabled
            || context.service_id != self.service_id
            || !context.record_admitted.load(Ordering::Acquire)
            || !self.durable_key
            || !self.recordable
            || !matches!(
                self.surface,
                TurnSurfaceKind::AgentLoop
                    | TurnSurfaceKind::Interactive
                    | TurnSurfaceKind::Server
                    | TurnSurfaceKind::Cron
                    | TurnSurfaceKind::Xin
                    | TurnSurfaceKind::Worker
                    | TurnSurfaceKind::Channel
            )
            || trusted.surface != self.surface
            || trusted.workspace_id.as_ref() != self.generation.effective.workspace_dir.to_string_lossy()
            || trusted.catalog_generation != *self.catalog.generation()
            || trusted.policy_version != self.policy_version
            || context.hop_ids().last().copied() != Some(context.decision_id())
        {
            return Err(SettlementRefUnavailable);
        }
        let writer = self.store.writer().ok_or(SettlementRefUnavailable)?;
        let store_workspace_id = self.store.workspace_id().ok_or(SettlementRefUnavailable)?;
        Ok(crate::agent::terminal::DecisionSettlementRef::issued(
            context,
            writer,
            self.store.reader(),
            store_workspace_id,
            Arc::clone(&self.state_key),
            self.generation.effective.workspace_dir.clone(),
            self.generation.effective.cost.clone(),
            Arc::clone(&self.memory),
            SettlementIssueToken(()),
        )
        .with_parent_grant(self.parent_grant.clone())
        .with_operator_parent(
            self.operator_task
                .as_ref()
                .map(|task| Arc::new(task.parent_source()))
                .or_else(|| {
                    self.operator_process
                        .as_ref()
                        .map(|process| Arc::new(process.snapshots().parent.clone()))
                }),
        ))
    }

    /// Recovery retains its own pins and queues independently of this service.
    pub(crate) fn recovery_scope(&self) -> Arc<super::ingress_recovery::RecoveryScope> {
        Arc::clone(&self.recovery)
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
        self.recovery_scope().rejoin_terminal(terminal_id, owner_id).await
    }

    /// Inspect the attached generation without probing a backend or opening
    /// a new store. Admin and control-ladder readers can use this snapshot.
    pub(crate) fn status(&self) -> DecisionPlaneStatus {
        DecisionPlaneStatus {
            backend: self.factory.status(),
            catalog_generation: *self.catalog.generation(),
            store_mode: self.store.mode(),
            durable_key: self.durable_key,
            recordable: self.recordable,
        }
    }

    pub(crate) fn prompt_memory_loader(&self) -> Option<&dyn crate::agent::memory_loader::MemoryLoader> {
        self.memory_loader.as_deref()
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

    async fn open_turn_context_observed(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
        observation: Option<&segment_observation::SegmentObservation>,
        resume: Option<pending_approval::AdmittedDecisionResume>,
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
            predicted: None,
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
        let (call_fingerprints, verified_approval_calls) = resume_evidence.as_ref().map_or_else(
            || (BTreeMap::new(), BTreeMap::new()),
            |evidence| crate::runtime::decision_advisory::resume_gate_maps(evidence, &self.state_key),
        );
        let replayed_non_idempotent = BTreeSet::new();
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
        };
        let gates = crate::runtime::decision_advisory::gate_inputs(
            &admission_facts,
            &call_fingerprints,
            verified_approval_calls,
            &replayed_non_idempotent,
        );
        let execution_refs = direct_answer_refs(&engine, &catalog, &gates, &BTreeSet::new(), &|bytes| {
            crate::cost::estimate::estimate_cost(&model, &self.generation.effective.cost, bytes)
        });
        // The turn's one accounting scope, created at opening. Without the
        // prediction hop nothing has been charged to it yet.
        let accounting = accounting::opening_scope(self, proof, budgets, deadline);
        let resume = resume.map(Arc::new);
        let runtime = Arc::new(crate::runtime::turn_context::TurnRuntimeContext {
            decision_id,
            trusted: trusted_parts,
            routing_input,
            goal,
            has_conversation_history: proof.has_conversation_history(),
            observed,
            budgets,
            accounting: accounting.clone(),
            send_limits: limits,
            catalog: Arc::clone(&self.catalog),
            publication_policy: self.publication_policy.clone(),
            catalog_generation: catalog.generation(),
            admission: admission_facts,
            resume: resume.clone(),
            resume_evidence: resume_evidence.map(Arc::new),
            state_key: Arc::clone(&self.state_key),
        });
        Ok(Arc::new(TurnDecisionContext {
            _event_memory: event_memory,
            model_catalog: self.provider_catalog.release_pin(),
            runtime,
            resume,
            accounting,
            owner_filesystem,
            source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
            service_id: self.service_id,
            record_admitted: AtomicBool::new(false),
            recursive_allowance: self.recursive_allowances.retain(proof),
            main_parameters: Default::default(),
            execution_trace: std::sync::OnceLock::new(),
            execution_closed: Arc::new(AtomicBool::new(false)),
            collected_approval_result: std::sync::OnceLock::new(),
            discovery_offers: std::sync::OnceLock::new(),
            discovery_admissions: std::sync::OnceLock::new(),
            engine,
            hop_ids: vec![decision_id],
            prior_hops: Vec::new(),
            directive: TurnExecutionDirective::AnswerDirectly,
            main_compaction: budget.main_compaction,
            execution_refs,
        }))
    }

    pub(crate) async fn open_issued_resume_turn_context(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        let admitted = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(AgentDecisionError::Cancelled),
            admitted = self.admit_decision_resume(proof) => admitted.map_err(AgentDecisionError::PendingApproval)?,
        };
        let _resume_allowance = self
            .generation
            .effective
            .sessions_spawn
            .allow_recursive_delegation
            .then(|| self.recursive_allowances.inherit_resume(proof, admitted.original_run()));
        let opened = self
            .open_turn_context_observed(proof, kind, cancellation, None, Some(admitted))
            .await?;
        self.record_opened_context(proof, opened, cancellation).await
    }

    /// Snapshot the initial decision budget after the source proof and PMM
    /// selection have been rechecked. Execution must recheck spend under its
    /// own ledger lock; this read-only snapshot grants no tool authority.
    pub(crate) fn agent_budget_snapshot(
        &self,
        proof: &dyn TurnSourceProof,
    ) -> Result<AgentDecisionBudget, AgentDecisionBudgetError> {
        let _model = self.resolve_agent_model(proof)?;
        let config = &self.generation.effective;
        let effective = self.effective_compaction(proof.provider_name(), proof.model_name());
        let extra_token_ratio_limit = config.causal_tree.policy.extra_token_ratio_limit;
        let reservation =
            DecisionTokenReservation::from_effective_compaction(&effective.config, extra_token_ratio_limit)?;
        let main_compaction = reservation.reserved_compaction(&effective.config, extra_token_ratio_limit)?;
        let remaining = self
            .spend_ceiling(proof)?
            .live()
            .map_err(AgentDecisionBudgetError::Spend)?;
        Ok(AgentDecisionBudget {
            budgets: TurnBudgets {
                tokens: u64::try_from(reservation.decision_tokens).unwrap_or(u64::MAX),
                latency_ms: config.causal_tree.policy.extra_latency_budget_ms,
                micro_usd: remaining,
            },
            main_compaction,
        })
    }

    /// Live spend limit and price inputs for one provider send of this turn.
    /// The spend limit is re-read from the ledger on every call so an earlier
    /// round's charge is visible.
    pub(crate) fn send_limits(&self, proof: &dyn TurnSourceProof) -> Result<SendLimits, AgentDecisionBudgetError> {
        let budget = self.agent_budget_snapshot(proof)?;
        let effective = self.effective_compaction(proof.provider_name(), proof.model_name());
        Ok(SendLimits {
            response_reserve: effective.config.reserve_tokens,
            remaining_micro_usd: budget.budgets.micro_usd,
            model: self.resolve_agent_model(proof)?,
            cost: self.generation.effective.cost.clone(),
        })
    }

    /// A published catalog cannot authorize a capability after its live
    /// registry schema or binding has changed. Call before candidate exposure
    /// and again immediately before committing a directive.
    pub(crate) fn check_live_parity(&self) -> Result<(), ParityError> {
        if let Some(parent) = &self.ordinary_parent {
            parent.verify_live().map_err(|_| ParityError)?;
        }
        if let Some(task) = &self.ordinary_task {
            task.verify().map_err(|_| ParityError)?;
        }
        if let Some(task) = &self.operator_task {
            task.verify().map_err(|_| ParityError)?;
        }
        if let Some(process) = &self.operator_process {
            process.verify().map_err(|_| ParityError)?;
            return self
                .catalog
                .check_published_parity(&self.execution, process.snapshots().publication.allowed());
        }
        if let Some(process) = &self.ordinary_process {
            process.verify().map_err(|_| ParityError)?;
        }
        self.child_scope().map_or_else(
            || self.catalog.check_parity(&self.execution),
            |scope| self.catalog.check_child_parity(&self.execution, scope),
        )
    }

    fn child_scope(&self) -> Option<&Arc<crate::tools::capability::child::VerifiedChildScope>> {
        self.parent_grant
            .as_ref()
            .map(|grant| grant.scope())
            .or_else(|| self.ordinary_parent.as_ref().map(OrdinaryChildPublication::scope))
            .or_else(|| self.ordinary_task.as_ref().map(|task| task.publication().scope()))
            .or_else(|| self.ordinary_process.as_ref().map(|process| &process.snapshots().scope))
    }

    pub(crate) fn resolve_agent_model(&self, proof: &dyn TurnSourceProof) -> Result<ResolvedModel, TurnModelError> {
        validate_agent_turn_origin(&self.generation, self.surface, proof)?;
        match &self.operator_process {
            Some(process) => process
                .verify_source(proof)
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?,
            None if proof.operator_process_authority_id().is_some() => {
                return Err(TurnModelError::Origin(TurnOriginMismatch));
            }
            None => {}
        }
        match &self.operator_task {
            Some(task) => task
                .verify_source(proof)
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?,
            None if proof.operator_task_authority_id().is_some() => {
                return Err(TurnModelError::Origin(TurnOriginMismatch));
            }
            None => {}
        }
        match &self.ordinary_process {
            Some(process) => process
                .verify_source(proof)
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?,
            None if proof.ordinary_process_authority_id().is_some() => {
                return Err(TurnModelError::Origin(TurnOriginMismatch));
            }
            None => {}
        }

        if let Some(parent) = &self.ordinary_parent {
            parent
                .verify_live()
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?;
        }
        match &self.ordinary_task {
            Some(task) => task
                .verify_source(proof)
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?,
            None if proof.ordinary_task_authority_id().is_some() => {
                return Err(TurnModelError::Origin(TurnOriginMismatch));
            }
            None => {}
        }
        match (self.child_scope(), proof.child_scope()) {
            (None, None) => {}
            (Some(issued), Some(scope)) if Arc::ptr_eq(issued, scope) => {}
            _ => return Err(TurnModelError::Origin(TurnOriginMismatch)),
        }
        if let Some(scope) = proof.child_scope() {
            scope
                .validate_runtime(
                    &self.state_key,
                    proof.owner_id(),
                    proof.workspace_id(),
                    self.catalog_view().generation(),
                )
                .map_err(|_| TurnModelError::Origin(TurnOriginMismatch))?;
        }
        let config = &self.generation.effective;
        let pinned_route = crate::providers::selection::route_target(
            &self.provider_catalog,
            proof.provider_name(),
            proof.model_name(),
        )?;
        if pinned_route != proof.model_binding().route() {
            return Err(TurnModelError::RouteDrift);
        }
        let resolved = crate::providers::selection::check_with(
            config,
            &self.provider_catalog,
            proof.provider_name(),
            proof.model_name(),
            self.credential_for_proof(proof),
            &Requirement::chat(),
            UnknownPolicy::Invocation,
        )?;
        if !proof.model_binding().matches(&resolved) {
            return Err(TurnModelError::SelectionDrift);
        }
        Ok(resolved)
    }

    fn credential_for_proof(&self, proof: &dyn TurnSourceProof) -> crate::providers::credentials::CredentialRef {
        if self.child_scope().is_some() || self.operator_task.is_some() || self.operator_process.is_some() {
            proof.model_binding().credential_reference().clone()
        } else {
            crate::providers::selection::credential_for(&self.generation.effective, proof.provider_name())
        }
    }

    /// G7 permits tool calls only when ingress configuration allows them
    /// and PMM verified NativeTools for the exact model on this catalog.
    /// An id-only invocation can still chat
    /// under PMM's invocation policy, but cannot gain tool authority.
    pub(crate) fn model_can_call_tools(&self, proof: &dyn TurnSourceProof) -> bool {
        if !proof.tool_calls_allowed() || self.resolve_agent_model(proof).is_err() {
            return false;
        }
        crate::runtime::turn_model::can_call_tools(
            &self.generation.effective,
            &self.provider_catalog,
            proof,
            self.credential_for_proof(proof),
        )
    }
}

/// Construct an ingress or a durably authorized child service. Ordinary
/// in-process children derive from `OrdinaryChildPublication` instead.
/// The factory receives the persisted workspace key or `None`; an ephemeral
/// state key never enables a remote backend or cross-process child authority.
pub(crate) async fn build_turn_decision_service(
    surface: TurnSurfaceKind,
    build: TurnServiceBuild<'_>,
) -> Result<Arc<TurnDecisionService>, TurnServiceBuildError> {
    if build.catalog_sources.config_generation_id != build.generation.id.0.to_string() {
        return Err(TurnServiceBuildError::SourceGeneration);
    }
    match (&build.parent_grant, surface) {
        (Some(grant), TurnSurfaceKind::Worker) if Arc::ptr_eq(grant.generation(), &build.generation) => {}
        (None, surface) if surface != TurnSurfaceKind::Worker => {}
        _ => return Err(TurnServiceBuildError::ChildAuthority),
    }
    let config = &build.generation.effective;
    if config.causal_tree.enabled && !crate::memory::serves_tool_execution_ledger(build.memory.name()) {
        return Err(TurnServiceBuildError::EventStoreUnsupported);
    }
    if config.causal_tree.enabled && !config.memory.events.record_user_messages {
        return Err(TurnServiceBuildError::InboundEventsDisabled);
    }
    // `load_key` deliberately does not touch the workspace key while the
    // plane is disabled. Its single preparation result is shared with recovery;
    // an unavailable key leaves both the factory builtin-only and recovery unrecorded.
    let workspace_key = store_preparation::load_key(Arc::clone(&build.generation)).await?;
    let durable_key = config.causal_tree.enabled && workspace_key.is_some();
    if let Some(grant) = &build.parent_grant {
        let key = workspace_key
            .as_ref()
            .filter(|_| durable_key)
            .ok_or(TurnServiceBuildError::ChildAuthority)?;
        let (manifest, _) = grant.scope().gate_inputs();
        grant
            .scope()
            .validate_runtime(
                key,
                &manifest.authority.owner,
                &config.workspace_dir.to_string_lossy(),
                grant.catalog().view(&CatalogScope::default()).generation(),
            )
            .map_err(|_| TurnServiceBuildError::ChildAuthority)?;
        for reference in &manifest.allowed {
            let view = grant.catalog().view(&CatalogScope::default());
            let entry = view
                .entry_any(&reference.id)
                .ok_or(TurnServiceBuildError::ChildAuthority)?;
            if !build
                .surface_policy
                .permits(&entry.public_name, &entry.root, &entry.target)
                || build.surface_policy.excluded_names.contains(&entry.public_name)
                || build.surface_policy.excluded_names.contains(&entry.root)
            {
                return Err(TurnServiceBuildError::ChildAuthority);
            }
        }
        grant.catalog().check_child_parity(&build.execution, grant.scope())?;
    }
    let recovery_owner = super::turn_cleanup::current_tracker().unwrap_or_default();
    let opened = if durable_key {
        Some(
            super::ingress_recovery::open_advisory_store(
                &build.generation,
                Arc::clone(&build.memory),
                Arc::clone(&build.events),
                &recovery_owner,
            )
            .await
            .map_err(TurnServiceBuildError::RecoveryPreparation)?,
        )
    } else {
        None
    };
    let store = match &opened {
        Some((store, _)) => Arc::clone(store),
        None => {
            store_preparation::open_store(Arc::clone(&build.generation), Arc::clone(&build.events), None, false).await?
        }
    };
    let durable_events = opened.as_ref().map(|(_, events)| Arc::clone(events));
    let events: Arc<dyn CausalEventSink> = durable_events.as_ref().map_or_else(
        || Arc::clone(&build.events),
        |events| Arc::clone(events) as Arc<dyn CausalEventSink>,
    );
    let recovery = super::ingress_recovery::RecoveryScope::prepare(
        &build.generation,
        Arc::clone(&build.memory),
        surface,
        build.parent_grant.as_deref(),
        &recovery_owner,
        workspace_key.clone(),
        match &opened {
            Some((store, events)) => {
                super::ingress_recovery::PreparedAdvisory::opened(Arc::clone(store), Arc::clone(events))
                    .map_err(TurnServiceBuildError::RecoveryPreparation)?
            }
            None => super::ingress_recovery::PreparedAdvisory::default(),
        },
    )
    .await
    .map_err(TurnServiceBuildError::RecoveryPreparation)?;
    let state_key = workspace_key.clone().unwrap_or_else(DecisionKey::ephemeral);
    let policy_version = pinned_policy_version(config, &build.security, &build.surface_policy, &state_key)
        .map_err(TurnServiceBuildError::PolicyFingerprint)?;
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
    let factory = evidence_fixture::wrap_factory(factory, config, Arc::clone(&state_key), Arc::clone(&events));
    #[cfg(test)]
    let factory = selected_loader_fixture::wrap_factory(factory, config, Arc::clone(&state_key), Arc::clone(&events));
    #[cfg(test)]
    let factory = ordinary_child_fixture::wrap_factory(factory, Arc::clone(&state_key), Arc::clone(&events));
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
        recovery,
        advisory: crate::runtime::decision_advisory::DecisionAdvisory::new(
            Arc::clone(&build.generation.effective),
            advisory_factory,
        ),
        child_recovery: Default::default(),
        recursive_allowances: Default::default(),
        memory_loader: None,
        loop_runtime: None,
        publication_policy: build.surface_policy,
        ordinary_parent: None,
        ordinary_money: None,
        operator_task: None,
        operator_process: None,
        ordinary_task: None,
        ordinary_process: None,
        parent_grant: build.parent_grant,
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

#[cfg(test)]
pub(crate) use tests::with_worker_probe_manifest;

#[cfg(test)]
mod tests;

pub(crate) fn selected_custom_memory_read_scope(
    write_scope: &crate::memory::principal::MemoryWriteContext,
    workspace: &str,
    owner: &str,
) -> anyhow::Result<crate::memory::principal::MemoryWriteContext> {
    anyhow::ensure!(
        !workspace.is_empty()
            && !owner.is_empty()
            && write_scope.workspace_id.as_deref() == Some(workspace)
            && write_scope.owner_id.as_deref().is_none_or(|present| present == owner),
        "selected custom retrieval scope differs from its source"
    );
    let mut scope = write_scope.clone();
    scope.owner_id = Some(owner.to_owned());
    scope.require_owner_match = true;
    Ok(scope)
}

#[cfg(test)]
mod selected_custom_memory_scope_tests;
