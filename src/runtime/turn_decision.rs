//! One pinned runtime generation for causal turn decisions.
//!
//! The constructor receives the already finalized runtime catalog inputs.
//! Rebuilding them from configuration alone would drop live MCP, Skill, hook
//! and node state, so a source-generation mismatch rejects the service.

#[cfg(test)]
mod ordinary_child_fixture;

#[cfg(test)]
use crate::causal_tree::state::ConstraintCode;

#[cfg(test)]
pub(crate) mod progress_diagnostic;

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
mod child_recovery;
mod delegation;
mod discovery;
pub(super) mod recursive_allowance;
pub(super) mod segment_observation;
pub(crate) use discovery::CapabilitySearchAuthority;
mod execution;
pub(crate) use execution::{OperatorChildPublication, OrdinaryChildPublication, OrdinaryProcessLaunch};
mod owner_filesystem;
mod pending_approval;
pub(crate) use pending_approval::collection::ApprovalCollectionLease;
#[path = "turn_decision/routing/replay_facts.rs"]
mod router_replay_facts;
pub(crate) use router_replay_facts::{RouterReadFacts, provider_binding as router_provider_binding};
mod latency_snapshot;
#[cfg(feature = "llm-router")]
mod routing;
#[cfg(feature = "llm-router")]
pub(crate) use routing::{RouterEmbedding, RouterModelReadmission, commit_training_owned};

#[cfg(feature = "llm-router")]
#[inline(never)]
fn boxed_router_history_field<'a, F>(
    make: impl FnOnce() -> F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + Send + 'a>>
where
    F: std::future::Future + Send + 'a,
{
    Box::pin(make())
}

use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU8;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use super::turn_source::TurnSourceProof;
#[cfg(test)]
use crate::agent::entry::AgentTurnProof;
use crate::capability::CapabilityAvailabilityLevel;
use crate::causal_tree::branch::{
    CandidateSpec, CapabilityRef, CapabilityTarget, CatalogGeneration, CausalActionKind, CausalCandidate,
};
use crate::causal_tree::candidate_generator::{CandidateSet, DiscoveredRefs};
use crate::causal_tree::decision_engine::{
    CausalDecisionEngine, DiscoveryIssueError, EngineDecision, EngineError, EngineInputs, RetrievalIssueError,
};
use crate::causal_tree::decision_rehearsal::RehearsalPorts;
use crate::causal_tree::decision_rehearsal::{CatalogProbe, MemoryProbe, PinnedCount, ProbeUnavailable, SkillProbe};
use crate::causal_tree::directive::{
    DirectiveContext, DirectiveError, TurnExecutionDirective, directive_from_selection, render_terminal_directive,
};
use crate::causal_tree::gates::{GateInputs, GateOutcome, HardGates};
use crate::causal_tree::ids::DecisionId;
use crate::causal_tree::outcome::{DecisionRecordBuilder, OutcomeCorrelator, RecordBuildError, TerminalFacts};
use crate::causal_tree::state::{
    CausalState, DeliveryConstraints, DerivedState, HopLineage, ObservedArtifact, ObservedArtifactKind, ObservedState,
    ObservedStep, ObservedStepKind, ObservedStepStatus, RawRoutingInput, TrustedParts, TrustedState, TurnBudgets,
    TurnSurfaceKind,
};
use crate::causal_tree::store::{CausalDecisionStore, DecisionScope, Enqueue, Retention, StoreError, StoreMode};
use crate::config::ConfigGeneration;
use crate::decision::builtin::weights::STALE_ARTIFACT_MS;
use crate::decision::factory::{
    BackendPlaneStatus, DecisionBackendFactory, DecisionTransports, FactoryBuildError, FactoryDeps,
};
use crate::decision::telemetry::{CausalEvent, CausalEventFields, CausalEventKind, CausalEventSink, CausalLineage};
use crate::decision::{DecisionKey, KeyDomain, RequestLimits};
use crate::memory::Memory;
use crate::memory::principal::MemoryWriteContext;
use crate::memory::traits::DecisionMemoryHit;
use crate::providers::catalog::{
    CatalogView as ProviderCatalogView, Feature, Requirement, ResolvedModel, UnknownPolicy, Verification,
};
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::{
    CapabilityCatalog, CatalogError, CatalogScope, CatalogSources, CatalogView, Exposure, ParityError, SurfacePolicy,
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
    child_recovery: child_recovery::ChildRecoveryState,
    recursive_allowances: recursive_allowance::RunAllowances,
    memory_loader: Option<Arc<dyn crate::agent::memory_loader::MemoryLoader>>,
    loop_runtime: Option<TurnLoopRuntime>,
    publication_policy: SurfacePolicy,
    ordinary_parent: Option<OrdinaryChildPublication>,
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
    ) -> crate::router::context::EffectiveCompactionConfig {
        let config = &self.generation.effective;
        crate::router::context::resolve_effective_compaction_config_with_catalog(
            &config.agent.compaction,
            provider,
            model,
            &config.router,
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

/// Metadata-only rehearsal over the already pinned, filtered catalog view.
/// It never refreshes MCP or starts a discovery process.
struct PinnedCatalogRehearsal<'view, 'catalog> {
    catalog: &'view CatalogView<'catalog>,
}

/// A query and visibility scope captured by the authenticated CLI ingress.
/// The opaque probe arguments can only name this exact source-owned query.
struct ScopedMemoryRehearsal<'a> {
    memory: &'a dyn Memory,
    scope: &'a MemoryWriteContext,
    query: &'a str,
    principal: &'a str,
    key: &'a DecisionKey,
}

#[async_trait::async_trait]
impl MemoryProbe for ScopedMemoryRehearsal<'_> {
    async fn count_hits(
        &self,
        principal: &str,
        query_ref: [u8; 32],
        k: u8,
        deadline: tokio::time::Instant,
    ) -> Result<u32, ProbeUnavailable> {
        let expected = self.key.hmac(
            KeyDomain::StateFingerprint,
            &[b"rehearsal-query", self.query.as_bytes()],
        );
        if principal != self.principal || query_ref != expected || !(1..=16).contains(&k) {
            return Err(ProbeUnavailable);
        }
        self.memory
            .count_rehearsal_hits(self.query, k, self.scope, deadline.into_std())
            .await
            .map_err(|_| ProbeUnavailable)
    }
}

impl PinnedCatalogRehearsal<'_, '_> {
    fn count(
        &self,
        generation: CatalogGeneration,
        ids: &[CapabilityRef],
        skill_only: bool,
    ) -> Result<PinnedCount, ProbeUnavailable> {
        if generation != self.catalog.generation() {
            return Err(ProbeUnavailable);
        }
        let present = ids
            .iter()
            .filter(|reference| {
                self.catalog.contains(&reference.id)
                    && self.catalog.entry_any(&reference.id).is_some_and(|entry| {
                        reference.generation == generation
                            && reference.fingerprint == entry.fingerprint
                            && !matches!(entry.exposure, Exposure::Unavailable { .. })
                            && (!skill_only || entry.family == CapabilityTarget::Skill)
                            && (skill_only || entry.availability.level >= CapabilityAvailabilityLevel::Ready)
                    })
            })
            .count();
        Ok(PinnedCount {
            generation,
            present: u32::try_from(present).map_err(|_| ProbeUnavailable)?,
        })
    }
}

#[async_trait::async_trait]
impl CatalogProbe for PinnedCatalogRehearsal<'_, '_> {
    async fn availability(
        &self,
        generation: CatalogGeneration,
        ids: &[CapabilityRef],
    ) -> Result<PinnedCount, ProbeUnavailable> {
        self.count(generation, ids, false)
    }
}

#[async_trait::async_trait]
impl SkillProbe for PinnedCatalogRehearsal<'_, '_> {
    async fn metadata(
        &self,
        generation: CatalogGeneration,
        ids: &[CapabilityRef],
    ) -> Result<PinnedCount, ProbeUnavailable> {
        self.count(generation, ids, true)
    }
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

    fn emit_agent_phase(
        &self,
        proof: &dyn TurnSourceProof,
        decision_id: DecisionId,
        kind: CausalEventKind,
        item_count: Option<usize>,
    ) {
        self.events.emit(CausalEvent {
            kind,
            lineage: CausalLineage {
                workspace_id: Arc::from(proof.workspace_id()),
                owner_id: Some(Arc::from(proof.owner_id())),
                session_key: Some(Arc::from(proof.session_key())),
                run_id: Some(Arc::from(proof.run_id())),
                source_event_id: proof.source_event_id().map(Arc::from),
                decision_id: Some(Arc::from(decision_id.to_string())),
                ..CausalLineage::default()
            },
            fields: CausalEventFields {
                item_count: item_count.map(|count| u16::try_from(count).unwrap_or(u16::MAX)),
                ..CausalEventFields::default()
            },
        });
    }

    /// Flush queued decision writes and then their causal events before the
    /// owning runtime exits. A stalled backend cannot hold CLI shutdown open.
    pub(crate) async fn shutdown(&self, deadline: Instant) {
        let _ = self.drain_writes(deadline).await;
    }

    /// Report queue acknowledgement to owners that must reject an incomplete exit.
    pub(crate) async fn drain_writes(&self, deadline: Instant) -> bool {
        let mut drained = true;
        if let Some(writer) = self.store.writer() {
            #[cfg(test)]
            crate::runtime::turn_cleanup::diagnostic::queue_presence("decision_writer_present", true);
            let writer_drained = writer.drain(deadline).await;
            #[cfg(test)]
            crate::runtime::turn_cleanup::diagnostic::queue("decision_writer_drained", writer_drained);
            drained &= writer_drained;
            if !writer_drained {
                tracing::warn!("causal decision writer did not drain before shutdown deadline");
            }
        }
        #[cfg(test)]
        if self.store.writer().is_none() {
            crate::runtime::turn_cleanup::diagnostic::queue_presence("decision_writer_present", false);
        }
        #[cfg(test)]
        crate::runtime::turn_cleanup::diagnostic::queue_presence("event_queue_present", self.durable_events.is_some());
        if let Some(events) = &self.durable_events {
            let events_drained = events.drain(deadline).await;
            #[cfg(test)]
            crate::runtime::turn_cleanup::diagnostic::queue("event_queue_drained", events_drained);
            drained &= events_drained;
            if !events_drained {
                tracing::warn!("causal event queue did not drain before shutdown deadline");
            }
        }
        drained
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
    model_admission: Option<Arc<dyn crate::causal_tree::state::model_admission::ModelAdmissionView>>,
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
        ) as Arc<dyn crate::causal_tree::state::model_admission::ModelAdmissionView>
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
    model_admission: Option<&dyn crate::causal_tree::state::model_admission::ModelAdmissionView>,
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

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentDecisionBudgetError {
    #[error(transparent)]
    Model(#[from] TurnModelError),
    #[error(transparent)]
    Reserve(#[from] DecisionReserveUnavailable),
    #[error("remaining workspace spend cannot be read safely")]
    Spend(#[source] anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentTurnKind {
    SingleShot,
    Repl,
}

/// The immutable result of one source-bound decision chain. Only the service
/// constructs it; later record and execution streams consume these facts.
pub(crate) struct TurnDecisionContext {
    model_catalog: Option<Arc<crate::providers::catalog::ReleaseCatalog>>,
    owner_filesystem: Option<Arc<crate::tools::owner_filesystem::OwnerFilesystem>>,
    accounting: Option<Arc<crate::runtime::decision_accounting::AccountingScope>>,
    resume: Option<Arc<pending_approval::AdmittedDecisionResume>>,
    source_actor: (Arc<str>, Arc<str>),
    service_id: uuid::Uuid,
    record_admitted: AtomicBool,
    recursive_allowance: std::sync::OnceLock<recursive_allowance::ContextAllowance>,
    main_parameters: std::sync::OnceLock<crate::providers::parameter_capture::Capture>,
    execution_trace: std::sync::OnceLock<crate::tools::capability::execution_trace::ExecutionTrace>,
    #[cfg(feature = "llm-router")]
    automix_deadline: std::sync::OnceLock<std::time::Instant>,
    served_auxiliary: std::sync::OnceLock<crate::agent::terminal::served_auxiliary::ServedAuxiliaryLink>,
    execution_closed: AtomicBool,
    approval_collection_started: AtomicBool,
    collected_pending: std::sync::OnceLock<Box<pending_approval::CollectedPendingCommitment>>,
    discovery_offers: std::sync::OnceLock<parking_lot::Mutex<discovery::DiscoveryOffers>>,
    discovery_admissions: std::sync::OnceLock<parking_lot::Mutex<BTreeMap<String, discovery::DiscoveryAdmission>>>,
    engine: EngineDecision,
    /// Completed dependent hops retained for the recorder. Nothing is
    /// written until the whole chain returns successfully.
    prior_hops: Vec<EngineDecision>,
    directive: TurnExecutionDirective,
    main_compaction: crate::config::AgentCompactionConfig,
    hop_ids: Vec<DecisionId>,
    execution_refs: BTreeSet<CapabilityRef>,
    selected_retrieved: Vec<ObservedArtifact>,
    /// Extension-authored prompt text, deliberately excluded from observed
    /// evidence, fingerprints and retrieval progress.
    selected_custom_memory_context: Option<String>,
    #[cfg(feature = "llm-router")]
    router_transition: Option<Arc<routing::RouterTransition>>,
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

pub(crate) enum TurnDisposition {
    CollectApprovalArguments,
    Close { reply: std::borrow::Cow<'static, str> },
    Run,
}

/// CDP-23 supplies the durable terminal adapter. The service passes no caller
/// chosen scope, terminal ID, or reply text across this boundary.
#[async_trait::async_trait]
pub(crate) trait CloseTurnPorts: Send + Sync {
    type Receipt: Send;

    async fn finalize_closed_turn(
        &self,
        context: Arc<TurnDecisionContext>,
        reply: &str,
    ) -> anyhow::Result<Self::Receipt>;
}

#[cfg(test)]
pub(crate) async fn close_directive_turn<P: CloseTurnPorts>(
    context: Arc<TurnDecisionContext>,
    ports: &P,
) -> anyhow::Result<P::Receipt> {
    let TurnDisposition::Close { reply } = context.disposition()? else {
        anyhow::bail!("decision requires the constrained execution reader");
    };
    ports.finalize_closed_turn(context, &reply).await
}

/// Only the owning service issues an applied-event admission after checking
/// the source proof, durable decision chain and committed terminal receipt.
pub(super) struct VerifiedDirectiveApplication {
    event: CausalEvent,
    idempotency_key: String,
    original_generation: Option<(u64, Arc<str>)>,
}

impl VerifiedDirectiveApplication {
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

impl TurnDecisionService {
    /// Close an authenticated fixed terminal through its owning service.
    /// Applied telemetry is queued only after a matching durable receipt;
    /// persistence failure is reported as incomplete and remains retryable.
    pub(crate) async fn close_agent_directive_turn<P>(
        &self,
        proof: &dyn TurnSourceProof,
        context: Arc<TurnDecisionContext>,
        ports: &P,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<crate::agent::terminal::TurnTerminalReceipt>
    where
        P: CloseTurnPorts<Receipt = crate::agent::terminal::TurnTerminalReceipt>,
    {
        validate_agent_turn_origin(&self.generation, self.surface, proof)?;
        anyhow::ensure!(
            context.source_actor_matches(Some(proof.policy_channel()), Some(proof.policy_sender())),
            "closing decision belongs to another ingress actor"
        );
        let hop = context
            .audit_hops()
            .last()
            .context("closing decision has no final hop")?;
        let trusted = hop.state.trusted.parts();
        anyhow::ensure!(
            context.service_id == self.service_id
                && trusted.surface == self.surface
                && trusted.channel.is_none()
                && trusted.workspace_id.as_ref() == proof.workspace_id()
                && trusted.owner_id.as_deref() == Some(proof.owner_id())
                && trusted.session_key.as_deref() == Some(proof.session_key())
                && trusted.run_id.as_deref() == Some(proof.run_id())
                && trusted.task_id.as_deref() == proof.task_id()
                && trusted.source_event_id.as_deref() == proof.source_event_id()
                && hop.state.routing_input.as_str()
                    == crate::multimodal::project_image_markers_for_routing(proof.routing_input()).as_ref()
                && trusted.catalog_generation == *self.catalog.generation()
                && trusted.policy_version == self.policy_version
                && context.hop_ids().last().copied() == Some(context.decision_id()),
            "closing decision does not match its source-owned turn"
        );
        let TurnDisposition::Close { reply } = context.disposition()? else {
            anyhow::bail!("decision requires the constrained execution reader");
        };
        anyhow::ensure!(!cancellation.is_cancelled(), "terminal close was cancelled");
        let eligible = self.generation.effective.causal_tree.enabled
            && self.durable_key
            && self.recordable
            && context.record_admitted.load(Ordering::Acquire)
            && self.store.mode() == StoreMode::ReadWrite
            && self.durable_events.is_some();
        if eligible {
            self.settlement_ref(Arc::clone(&context))?
                .validate_durable_chain()
                .await?;
        }
        anyhow::ensure!(!cancellation.is_cancelled(), "terminal close was cancelled");
        let receipt = ports.finalize_closed_turn(Arc::clone(&context), &reply).await?;
        // Cancellation can leave an actually committed marker. Do not claim
        // application from this cancelled helper or pretend to undo that I/O.
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "terminal close was cancelled after finalization"
        );
        if eligible {
            let facts = TerminalFacts::from_receipt(&context, &receipt)?;
            anyhow::ensure!(
                facts.is_completed_local_close(),
                "terminal receipt is not a completed local close"
            );
            let key = format!("turn:{}:final", receipt.terminal_id);
            let stored = self
                .memory
                .find_message_event_by_idempotency_key(proof.workspace_id(), &key)
                .await?
                .context("terminal receipt is not in the owning service memory")?;
            anyhow::ensure!(
                serde_json::to_value(&stored)? == serde_json::to_value(&receipt.terminal_event)?,
                "terminal receipt differs from its committed marker"
            );
            anyhow::ensure!(
                !cancellation.is_cancelled(),
                "terminal close was cancelled before application"
            );
            let event = CausalEvent {
                kind: CausalEventKind::DirectiveApplied,
                lineage: CausalLineage {
                    workspace_id: Arc::from(proof.workspace_id()),
                    owner_id: Some(Arc::from(proof.owner_id())),
                    session_key: Some(Arc::from(proof.session_key())),
                    run_id: Some(Arc::from(proof.run_id())),
                    source_event_id: proof.source_event_id().map(Arc::from),
                    causation_event_id: Some(Arc::from(stored.event_id)),
                    decision_id: Some(Arc::from(context.decision_id().to_string())),
                    ..CausalLineage::default()
                },
                fields: CausalEventFields {
                    item_count: Some(1),
                    ..CausalEventFields::default()
                },
            };
            let application = VerifiedDirectiveApplication {
                event,
                idempotency_key: format!("causal:{}:directive-applied", context.decision_id()),
                original_generation: None,
            };
            anyhow::ensure!(
                !cancellation.is_cancelled(),
                "terminal close was cancelled before queue admission"
            );
            self.durable_events
                .as_ref()
                .context("applied event sink is unavailable")?
                .emit_applied(application);
        }
        Ok(receipt)
    }
}

/// Derive parent execution metadata from exactly one final-gated selection.
/// This private conversion issues no executor lease or dispatch authority.
fn selected_execute_refs(
    decision: &EngineDecision,
    directive: &TurnExecutionDirective,
    catalog: &CatalogView<'_>,
) -> Result<BTreeSet<CapabilityRef>, DirectiveError> {
    let checked = directive_from_selection(
        decision,
        &DirectiveContext {
            current_generation: catalog.generation(),
            approval_expires_at: chrono::Utc::now(),
            discovery_limit: 1,
        },
    )?;
    match (directive, checked) {
        (
            TurnExecutionDirective::Execute { allowed_capability_ids },
            TurnExecutionDirective::Execute {
                allowed_capability_ids: checked_ids,
            },
        ) if allowed_capability_ids == &checked_ids => {}
        (
            TurnExecutionDirective::Delegate { manifest },
            TurnExecutionDirective::Delegate {
                manifest: checked_manifest,
            },
        ) if manifest == &checked_manifest => {}
        (TurnExecutionDirective::Execute { .. } | TurnExecutionDirective::Delegate { .. }, _) => {
            return Err(DirectiveError::NotAdmitted);
        }
        _ => return Err(DirectiveError::ActionMismatch),
    }
    let selected_id = match &decision.selection {
        crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. }
        | crate::causal_tree::decision_select::Selection::Recover { candidate_id, .. } => candidate_id,
        _ => return Err(DirectiveError::NotAdmitted),
    };
    let crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } = &decision.candidates else {
        return Err(DirectiveError::NotAdmitted);
    };
    let selected = candidates
        .iter()
        .find(|candidate| candidate.candidate_id() == selected_id)
        .ok_or(DirectiveError::NotAdmitted)?;
    let references: BTreeSet<_> = selected.required_capabilities().iter().cloned().collect();
    if references.is_empty()
        || references.len() != selected.required_capabilities().len()
        || references.iter().any(|reference| {
            !catalog.contains(&reference.id)
                || reference.generation != catalog.generation()
                || !catalog.entry_any(&reference.id).is_some_and(|entry| {
                    entry.generation == reference.generation && entry.fingerprint == reference.fingerprint
                })
        })
    {
        return Err(DirectiveError::NotAdmitted);
    }
    Ok(references)
}

/// The bounded direct core available while the serving model decides whether
/// to answer or call a tool. These references are publication authority only:
/// binding and dispatch recheck the live catalog, source, schema, security and
/// approval policy. Delegators additionally require the sealed child manifest
/// retained by this exact decision.
fn exact_delegator_ref(decision: &EngineDecision, reference: &CapabilityRef) -> bool {
    let CandidateSet::Legal {
        candidates,
        exact_mentions,
        ..
    } = &decision.candidates
    else {
        return false;
    };
    candidates.iter().any(|candidate| {
        candidate.action() == CausalActionKind::Delegate
            && exact_mentions.contains(candidate.candidate_id())
            && candidate.required_capabilities() == std::slice::from_ref(reference)
    })
}

fn direct_answer_refs(
    decision: &EngineDecision,
    catalog: &CatalogView<'_>,
    gates: &GateInputs<'_>,
    selected_discovery_refs: &BTreeSet<CapabilityRef>,
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
            // A selected local search is already a bounded, turn-scoped
            // relevance decision. Carry that exact schema into the serving
            // model's final snapshot when the following hop answers, while
            // retaining the same publication gates as the direct core.
            let selected_discovery = !delegation
                && matches!(entry.exposure, Exposure::Searchable)
                && selected_discovery_refs.contains(&reference);
            if (!directly_exposed && !exact_delegation && !selected_discovery)
                || (delegation && decision.outgoing_child.is_none())
            {
                return None;
            }
            if trusted.constraints.iter().any(|code| {
                matches!(code.as_str(), "resume_exact_call" | "tools_off")
                    || code.as_str() == "intercepts_writes" && entry.plan_intercepted()
            }) || trusted.side_effect_mode == crate::causal_tree::state::SideEffectMode::ReadOnly
                && entry.effect > crate::causal_tree::branch::SideEffectClass::ReadOnly
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
                    estimated_cost: gates
                        .child_scope
                        .and_then(|_| delegation::publication_cost(decision, &reference, delegation)),
                    estimated_latency_ms: None,
                },
            );
            (!matches!(
                HardGates::check_capability_publication(&decision.state, catalog, &candidate, gates),
                GateOutcome::Blocked { .. }
            ))
            .then_some(reference)
        })
        .collect()
}

fn discovery_source_guards_clear(state: &CausalState) -> bool {
    !state.trusted.parts().approval_pending
        && !state
            .derived
            .candidates()
            .iter()
            .any(|candidate| candidate.exact_mention)
}

fn discovery_can_answer(decision: &EngineDecision) -> bool {
    discovery_source_guards_clear(&decision.state)
        && decision
            .prediction
            .as_ref()
            .is_none_or(crate::decision::factory::PredictionResult::served_attempt_succeeded)
}

impl TurnDecisionContext {
    /// Only already-issued Router work may finish its fixed close after the
    /// caller cancels. This is persistence cleanup, never an execution lease.
    pub(crate) const fn router_cleanup_close(&self) -> bool {
        #[cfg(feature = "llm-router")]
        {
            self.router_transition.is_some()
                && matches!(
                    self.directive,
                    TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. }
                )
        }
        #[cfg(not(feature = "llm-router"))]
        {
            false
        }
    }

    #[cfg(feature = "llm-router")]
    pub(crate) fn router_replay_facts(&self, key: &DecisionKey) -> anyhow::Result<Option<RouterReadFacts>> {
        self.router_transition
            .as_ref()
            .map(|transition| transition.replay_facts(self, key))
            .transpose()
    }

    #[cfg(not(feature = "llm-router"))]
    pub(crate) const fn router_replay_facts(&self, key: &DecisionKey) -> anyhow::Result<Option<RouterReadFacts>> {
        let _ = key;
        Ok(None)
    }

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

    pub(crate) fn served_auxiliary(&self) -> Option<&crate::agent::terminal::served_auxiliary::ServedAuxiliaryLink> {
        self.served_auxiliary.get()
    }
    pub(crate) fn bind_served_auxiliary(
        &self,
        link: crate::agent::terminal::served_auxiliary::ServedAuxiliaryLink,
    ) -> anyhow::Result<()> {
        self.served_auxiliary
            .set(link)
            .map_err(|_| anyhow::anyhow!("auxiliary reply already bound"))
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
        match &self.engine.selection {
            crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. }
            | crate::causal_tree::decision_select::Selection::Recover { candidate_id, .. } => {
                Some(candidate_id.target().as_str())
            }
            crate::causal_tree::decision_select::Selection::Abstain { .. } => Some("none"),
            _ => None,
        }
    }

    pub(crate) fn terminal_outcome_action(&self) -> Option<crate::causal_tree::branch::CausalActionKind> {
        use crate::causal_tree::branch::CausalActionKind;
        match &self.directive {
            TurnExecutionDirective::AnswerDirectly => Some(CausalActionKind::Answer),
            TurnExecutionDirective::Execute { .. } => self
                .execution_trace
                .get()
                .is_some_and(crate::tools::capability::execution_trace::ExecutionTrace::is_sealed)
                .then_some(CausalActionKind::UseTool),
            TurnExecutionDirective::Delegate { .. } => Some(CausalActionKind::Delegate),
            TurnExecutionDirective::Clarify { .. } => Some(CausalActionKind::Clarify),
            TurnExecutionDirective::Refuse { .. } => Some(CausalActionKind::Refuse),
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
        self.collected_pending.get().map(|pending| pending.event())
    }

    pub(crate) const fn approval_collection_proof(
        &self,
    ) -> Option<&crate::causal_tree::approval_collection::DeferredArgumentProof> {
        match &self.engine.selection {
            crate::causal_tree::decision_select::Selection::CollectApprovalArguments(proof) => Some(proof),
            _ => None,
        }
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

    /// The selected evidence belongs to the issued turn, not a fresh read.
    /// Decision-stage timeouts do not expire the subsequent provider turn;
    /// closing that turn prevents its evidence from re-entering execution.
    pub(crate) fn retrieved(&self) -> Result<&[ObservedArtifact], DirectiveError> {
        if self.execution_closed.load(Ordering::Acquire) {
            return Err(DirectiveError::NotAdmitted);
        }
        Ok(&self.selected_retrieved)
    }

    pub(crate) fn custom_memory_context(&self) -> Result<Option<&str>, DirectiveError> {
        if self.execution_closed.load(Ordering::Acquire) {
            return Err(DirectiveError::NotAdmitted);
        }
        Ok(self.selected_custom_memory_context.as_deref())
    }

    pub(crate) fn disposition(&self) -> Result<TurnDisposition, DirectiveError> {
        match &self.directive {
            TurnExecutionDirective::CollectApprovalArguments(_) if !self.execution_closed.load(Ordering::Acquire) => {
                Ok(TurnDisposition::CollectApprovalArguments)
            }
            TurnExecutionDirective::CollectApprovalArguments(_) => Err(DirectiveError::NotAdmitted),
            TurnExecutionDirective::Clarify { .. }
            | TurnExecutionDirective::RequestApproval { .. }
            | TurnExecutionDirective::Refuse { .. } => Ok(TurnDisposition::Close {
                reply: render_terminal_directive(&self.directive)?,
            }),
            TurnExecutionDirective::AnswerDirectly if !self.execution_closed.load(Ordering::Acquire) => {
                Ok(TurnDisposition::Run)
            }
            TurnExecutionDirective::Execute { .. } | TurnExecutionDirective::Delegate { .. }
                if !self.execution_closed.load(Ordering::Acquire) =>
            {
                Ok(TurnDisposition::Run)
            }
            TurnExecutionDirective::AnswerDirectly
            | TurnExecutionDirective::Execute { .. }
            | TurnExecutionDirective::Delegate { .. } => Err(DirectiveError::NotAdmitted),
            _ => Err(DirectiveError::ActionMismatch),
        }
    }
}

struct MemoryHopObservation {
    state: CausalState,
    fresh_read: bool,
    selected_artifacts: Vec<ObservedArtifact>,
    custom_context: Option<String>,
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
    #[error("decision action has no Agent execution port")]
    UnsupportedAction,
    #[error(transparent)]
    Retrieval(#[from] AgentRetrievalError),
    #[error(transparent)]
    Discovery(#[from] AgentDiscoveryError),
    #[error(transparent)]
    Record(#[from] RecordBuildError),
    #[error("recordable decision store has no writer")]
    RecordWriterUnavailable,
    #[error("delegation parent could not be durably verified")]
    DelegationStorage(#[source] anyhow::Error),
    #[error("ordinary delegation scope could not be verified")]
    DelegationScope(#[source] anyhow::Error),
    #[error("durable ingress recovery is not ready")]
    IngressRecovery(#[source] anyhow::Error),
    #[cfg(feature = "llm-router")]
    #[error("Router purpose-bound decision failed")]
    Router(#[source] anyhow::Error),
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
    #[error("selected discovery exceeds the configured decision hop limit")]
    HopLimit,
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
    #[error("selected evidence read exceeded the pinned decision deadline")]
    Deadline,
    #[error("selected evidence read exceeds the configured decision hop limit")]
    HopLimit,
    #[error("selected evidence read failed")]
    Backend(#[source] anyhow::Error),
}

/// Convert selected read results into bounded runtime observations. A row's
/// source_event_id is stored metadata, not an authenticated event receipt.
/// The retrieval step itself receives a runtime-issued event id separately.
fn memory_artifacts_from_selected_read(
    hits: &[DecisionMemoryHit],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<ObservedArtifact> {
    hits.iter()
        .take(5)
        .filter_map(|hit| {
            // The selected reader caps rows at 4 KiB. Redact the complete
            // value before the 256-character summary cut; cutting raw input
            // first could expose a credential prefix.
            if hit.content.len() > 4_096 || hit.content.trim().is_empty() {
                return None;
            }
            let (summary, summary_was_redacted, summary_was_truncated) =
                crate::causal_tree::projection::bounded_artifact_summary(&hit.content);
            let age_ms = chrono::DateTime::parse_from_rfc3339(&hit.created_at)
                .ok()
                .and_then(|created| now.signed_duration_since(created).num_milliseconds().try_into().ok())
                .unwrap_or(STALE_ARTIFACT_MS.saturating_add(1));
            Some(ObservedArtifact {
                kind: ObservedArtifactKind::Memory,
                age_ms,
                summary,
                summary_was_redacted,
                summary_was_truncated,
                source_event_id: None,
            })
        })
        .collect()
}

fn document_artifacts_from_selected_read(
    hits: &[crate::memory::traits::DecisionDocumentHit],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<ObservedArtifact> {
    hits.iter()
        .take(5)
        .filter_map(|hit| {
            if hit.content.len() > 4096 || hit.content.trim().is_empty() || hit.created_at.len() > 64 {
                return None;
            }
            let (summary, summary_was_redacted, summary_was_truncated) =
                crate::causal_tree::projection::bounded_artifact_summary(&hit.content);
            let age_ms = chrono::DateTime::parse_from_rfc3339(&hit.created_at)
                .ok()
                .and_then(|created| now.signed_duration_since(created).num_milliseconds().try_into().ok())
                .unwrap_or(STALE_ARTIFACT_MS.saturating_add(1));
            Some(ObservedArtifact {
                kind: ObservedArtifactKind::Document,
                age_ms,
                summary,
                summary_was_redacted,
                summary_was_truncated,
                source_event_id: None,
            })
        })
        .collect()
}

fn session_artifacts_from_selected_read(
    hits: &[crate::memory::traits::DecisionSessionHit],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<ObservedArtifact> {
    hits.iter()
        .take(5)
        .filter_map(|hit| {
            if hit.content.len() > 4096
                || hit.content.trim().is_empty()
                || hit.created_at.len() > 64
                || !matches!(hit.role.as_str(), "user" | "assistant")
            {
                return None;
            }
            let text = format!("{}: {}", hit.role, hit.content);
            let (summary, summary_was_redacted, summary_was_truncated) =
                crate::causal_tree::projection::bounded_artifact_summary(&text);
            let age_ms = chrono::DateTime::parse_from_rfc3339(&hit.created_at)
                .ok()
                .and_then(|created| now.signed_duration_since(created).num_milliseconds().try_into().ok())
                .unwrap_or(STALE_ARTIFACT_MS.saturating_add(1));
            Some(ObservedArtifact {
                kind: ObservedArtifactKind::Session,
                age_ms,
                summary,
                summary_was_redacted,
                summary_was_truncated,
                source_event_id: None,
            })
        })
        .collect()
}

impl DecisionTokenReservation {
    /// Use the selected route's effective compaction window, after its
    /// response reserve. The fixed marginal slice is also capped by the
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
        let decision_tokens = (available / 16).min(4096).min(ratio_limit);
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
            self.generation.effective.causal_tree.store.record_redacted_projection,
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

    /// Rejoin an outcome from the committed marker and owner-scoped decision
    /// row in this service's pinned memory/workspace. Callers cannot substitute
    /// a backend or workspace label. This reads no live context and grants no
    /// execution authority.
    pub(crate) async fn rejoin_terminal(&self, terminal_id: &str, owner_id: &str) -> anyhow::Result<Enqueue> {
        self.rejoin_terminal_inner(terminal_id, owner_id, None).await
    }

    pub(crate) fn ingress_recovery_enabled(&self) -> bool {
        self.generation.effective.causal_tree.enabled && self.recordable && self.durable_key
    }
    pub(crate) fn accepts_ingress_recovery(
        &self,
        memory: &Arc<dyn Memory>,
        workspace: &str,
        surface: TurnSurfaceKind,
    ) -> bool {
        Arc::ptr_eq(memory, &self.memory)
            && self.generation.effective.workspace_dir.to_string_lossy() == workspace
            && self.surface == surface
            && self.generation.effective.causal_tree.enabled
            && self.recordable
            && self.durable_key
    }

    pub(crate) async fn rejoin_ingress_terminal(
        &self,
        capsule: &crate::memory::ingress_recovery::IngressCapsule,
        deadline: Instant,
        owner: &Arc<super::turn_cleanup::CleanupTracker>,
    ) -> anyhow::Result<Enqueue> {
        anyhow::ensure!(
            capsule.scope.workspace_id == self.generation.effective.workspace_dir.to_string_lossy(),
            "recovery service workspace differs from capsule"
        );
        self.rejoin_terminal_inner(
            &capsule.run_id,
            &capsule.scope.owner_id,
            Some((&capsule.scope, deadline, owner)),
        )
        .await
    }

    async fn rejoin_terminal_inner(
        &self,
        terminal_id: &str,
        owner_id: &str,
        bounded: Option<(
            &crate::memory::ingress_recovery::IngressRecoveryScope,
            Instant,
            &Arc<super::turn_cleanup::CleanupTracker>,
        )>,
    ) -> anyhow::Result<Enqueue> {
        #[cfg(test)]
        let mut stage = super::turn_cleanup::diagnostic::Stage::current(
            super::turn_cleanup::diagnostic::StageName::RejoinPreconditions,
        );
        let workspace_id = self.generation.effective.workspace_dir.to_string_lossy();
        anyhow::ensure!(
            matches!(
                self.surface,
                TurnSurfaceKind::AgentLoop
                    | TurnSurfaceKind::Interactive
                    | TurnSurfaceKind::Server
                    | TurnSurfaceKind::Cron
                    | TurnSurfaceKind::Xin
                    | TurnSurfaceKind::Worker
                    | TurnSurfaceKind::Channel
            ) && self.generation.effective.causal_tree.enabled
                && self.recordable
                && self.durable_key,
            "terminal rejoin requires an enabled pinned ingress store"
        );
        anyhow::ensure!(
            !terminal_id.is_empty() && !owner_id.is_empty(),
            "terminal rejoin scope is empty"
        );
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinLookup);
        let final_key = format!("turn:{terminal_id}:final");
        let event = if let Some((scope, deadline, _)) = bounded {
            self.memory
                .read_ingress_event(
                    scope,
                    &crate::memory::ingress_recovery::IngressEventLookup::IdempotencyKey(final_key.clone()),
                    deadline,
                )
                .await?
        } else {
            self.memory
                .find_message_event_by_idempotency_key(&workspace_id, &final_key)
                .await?
        }
        .context("committed terminal marker is missing")?;
        anyhow::ensure!(
            event.idempotency_key.as_deref() == Some(final_key.as_str()),
            "terminal lookup returned a different marker"
        );
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinReplay);
        let store_workspace_id = self.store.workspace_id().context("store workspace scope is missing")?;
        let replay = if let Some((_, deadline, owner)) = bounded {
            crate::agent::terminal::verify_terminal_replay_until(
                self.memory.as_ref(),
                &self.store.reader(),
                &self.state_key,
                &store_workspace_id,
                &workspace_id,
                owner_id,
                terminal_id,
                &event,
                deadline,
                owner,
            )
            .await?
        } else {
            crate::agent::terminal::verify_terminal_replay(
                self.memory.as_ref(),
                &self.store.reader(),
                &self.state_key,
                &store_workspace_id,
                &workspace_id,
                owner_id,
                terminal_id,
                &event,
            )
            .await?
        };
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinReplayScope);
        anyhow::ensure!(
            super::turn_source::ingress_event_surface(
                &replay.source().source,
                replay.source().channel.as_deref(),
                &replay.source().visibility
            ) == Some(self.surface),
            "terminal replay belongs to another ingress surface"
        );
        anyhow::ensure!(
            replay.parent_decision_id()
                == self
                    .parent_grant
                    .as_ref()
                    .map(|grant| grant.scope().gate_inputs().0.parent_decision_id),
            "terminal replay belongs to another parent decision"
        );
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinFacts);
        let decision = replay.decision();
        let facts = TerminalFacts::from_persisted_marker(&event, decision, owner_id, &workspace_id)?
            .with_verified_resume(&replay);
        let writer = self.store.writer().context("terminal rejoin writer is missing")?;
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinCorrelator);
        let expected = OutcomeCorrelator::persisted_record(decision, &facts)?;
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinRecord);
        let queued = writer.record_outcome(expected.clone());
        anyhow::ensure!(queued == Enqueue::Queued, "rejoined outcome could not be queued");
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinDrain);
        anyhow::ensure!(
            writer
                .drain(bounded.map_or_else(|| Instant::now() + Duration::from_secs(5), |(_, deadline, _)| deadline))
                .await,
            "rejoined outcome persistence timed out"
        );
        #[cfg(test)]
        stage.next(super::turn_cleanup::diagnostic::StageName::RejoinConfirmAndRepairs);
        let scope = DecisionScope::Owner {
            workspace_id: store_workspace_id,
            owner_id: Arc::from(owner_id),
        };
        anyhow::ensure!(
            if let Some((_, deadline, owner)) = bounded {
                let reader = self.store.reader();
                let scope = scope.clone();
                let expected = expected.clone();
                super::ingress_recovery::owned_store_read(owner, move || {
                    Ok(reader.outcome_matches_until(&scope, &expected, deadline)?)
                })
                .await?
            } else {
                self.store.reader().outcome_matches(&scope, &expected)?
            },
            "rejoined outcome is absent or conflicts with the authenticated terminal"
        );
        let (terminal_status, awaiting_approval, verified) = facts.recovered_signals(&decision.directive_kind);
        crate::skills::lifecycle::spawn_settle_task_terminal(
            self.generation.effective.workspace_dir.clone(),
            Some(owner_id.to_owned()),
            crate::skills::lifecycle::task_key(
                super::turn_source::subject_task_id(event.subject.as_ref()),
                event.run_id.as_deref(),
                event.parent_run_id.as_deref(),
            ),
            terminal_id.to_owned(),
            crate::skills::lifecycle::TerminalObservation {
                recovered: true,
                root: event.parent_run_id.is_none(),
                ..terminal_status.skill_observation(awaiting_approval, verified)
            },
        );
        if replay.parent_decision_id().is_some() {
            let link = crate::causal_tree::store::ChildOutcomeLink::from_replay(&replay, &expected)
                .context("child outcome lineage is inconsistent")?;
            anyhow::ensure!(
                writer.record_child_outcome(link) == Enqueue::Queued,
                "child outcome link could not be queued"
            );
            anyhow::ensure!(
                writer
                    .drain(bounded.map_or_else(|| Instant::now() + Duration::from_secs(5), |(_, deadline, _)| deadline))
                    .await,
                "child outcome link persistence timed out"
            );
            let expected_link = crate::causal_tree::store::ChildOutcomeLink::from_replay(&replay, &expected)
                .context("child outcome lineage changed")?;
            anyhow::ensure!(
                if let Some((_, deadline, owner)) = bounded {
                    let reader = self.store.reader();
                    let scope = scope.clone();
                    super::ingress_recovery::owned_store_read(owner, move || {
                        Ok(reader.child_outcome_matches_until(&scope, &expected_link, deadline)?)
                    })
                    .await?
                } else {
                    self.store.reader().child_outcome_matches(&scope, &expected_link)?
                },
                "child outcome link is absent or conflicts with authenticated facts"
            );
        }
        if replay.fixed_close()
            || facts.has_completed_run_evidence(decision, replay.run_capabilities())
            || (decision
                .selected()
                .is_some_and(|candidate| candidate.action == "use_tool")
                && facts.has_completed_resume_evidence(replay.run_capabilities()))
        {
            anyhow::ensure!(
                !replay.fixed_close() || facts.is_completed_local_close(),
                "replay is not a completed local close"
            );
            let source = replay.source();
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
            let application = VerifiedDirectiveApplication {
                event: CausalEvent {
                    kind: CausalEventKind::DirectiveApplied,
                    lineage: CausalLineage {
                        workspace_id: Arc::from(workspace_id.as_ref()),
                        owner_id: Some(Arc::from(owner_id)),
                        session_key: source.session_key.as_deref().map(Arc::from),
                        run_id: source.run_id.as_deref().map(Arc::from),
                        source_event_id: Some(Arc::from(source.event_id.as_str())),
                        causation_event_id: Some(Arc::from(event.event_id.as_str())),
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
            };
            let sink = self
                .durable_events
                .as_ref()
                .context("applied replay sink is unavailable")?;
            if let Some((_, deadline, _)) = bounded {
                sink.repair_applied_until(application, deadline).await?;
            } else {
                anyhow::ensure!(
                    sink.repair_applied(application, Instant::now() + Duration::from_secs(5))
                        .await,
                    "rejoined applied event is absent or conflicts with the authenticated terminal"
                );
            }
        }
        if replay.source().source == crate::memory::MessageEventSource::Console {
            // Only authenticated replay reaches this projection. Each event is
            // rechecked in the backend transaction; replay cannot duplicate it.
            if let Some((_, deadline, _)) = bounded {
                self.memory
                    .project_console_message_until(replay.source(), deadline)
                    .await?;
                if let Some(assistant) = replay.assistant() {
                    self.memory.project_console_message_until(assistant, deadline).await?;
                }
            } else {
                self.memory.project_console_message(replay.source()).await?;
                if let Some(assistant) = replay.assistant() {
                    self.memory.project_console_message(assistant).await?;
                }
            }
        }
        #[cfg(test)]
        stage.finish();
        Ok(queued)
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

    /// A selected Discover can inspect only the pinned catalog view for this
    /// exact source turn. It does not publish refs to the tool executor.
    fn discover_selected_agent_refs(
        &self,
        proof: &dyn TurnSourceProof,
        decision: &EngineDecision,
        catalog: &CatalogView<'_>,
    ) -> Result<DiscoveredRefs, AgentDiscoveryError> {
        let model = self.resolve_agent_model(proof)?;
        self.check_live_parity()?;
        let model_ref = format!("{}:{}", model.provider_label(), model.model());
        let exclusions = effective_agent_exclusions(
            &self.generation.effective,
            &self.registry,
            catalog,
            model.model(),
            &self.policy_version,
            &self.state_key,
        );
        validate_agent_retrieval_provenance(
            proof,
            decision.state.trusted.parts(),
            &model_ref,
            &self.policy_version,
            &exclusions.names,
            exclusions.model_admission.as_deref(),
        )?;
        let projected_input = crate::multimodal::project_image_markers_for_routing(proof.routing_input());
        let refs = decision.issue_selected_discovery(
            catalog,
            &DirectiveContext {
                current_generation: catalog.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
            projected_input.as_ref(),
        )?;
        self.check_live_parity()?;
        Ok(refs)
    }

    async fn retrieve_selected_agent_evidence_with_context(
        &self,
        proof: &dyn TurnSourceProof,
        decision: &EngineDecision,
    ) -> Result<(Vec<ObservedArtifact>, Option<String>), AgentRetrievalError> {
        let model = self.resolve_agent_model(proof)?;
        self.check_live_parity()?;
        let catalog = self.catalog_view();
        let model_ref = format!("{}:{}", model.provider_label(), model.model());
        let exclusions = effective_agent_exclusions(
            &self.generation.effective,
            &self.registry,
            &catalog,
            model.model(),
            &self.policy_version,
            &self.state_key,
        );
        validate_agent_retrieval_provenance(
            proof,
            decision.state.trusted.parts(),
            &model_ref,
            &self.policy_version,
            &exclusions.names,
            exclusions.model_admission.as_deref(),
        )?;
        let projected_input = crate::multimodal::project_image_markers_for_routing(proof.routing_input());
        let selected_family = decision.validate_selected_retrieval(
            &catalog,
            &DirectiveContext {
                current_generation: catalog.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
            projected_input.as_ref(),
        )?;
        if Instant::now() >= decision.deadline {
            return Err(AgentRetrievalError::Deadline);
        }
        let artifacts = tokio::time::timeout_at(tokio::time::Instant::from_std(decision.deadline), async {
            match selected_family {
                crate::causal_tree::branch::CapabilityTarget::Memory => {
                    if let Some(loader) = &self.memory_loader {
                        // memory_scope is also the legacy write projection and may
                        // omit owner_id. Bind this selected read to the already
                        // validated source owner instead of accepting an unscoped
                        // extension callback or refusing every embedded caller.
                        let scope = selected_custom_memory_read_scope(
                            proof.memory_scope(),
                            proof.workspace_id(),
                            proof.owner_id(),
                        )?;
                        let loaded = crate::agent::memory_loader::load_selected_memory(
                            loader.as_ref(),
                            self.memory.as_ref(),
                            projected_input.as_ref(),
                            &scope,
                            proof.session_key(),
                            decision.deadline,
                        )
                        .await?;
                        let artifacts = memory_artifacts_from_selected_read(&loaded.hits, chrono::Utc::now());
                        let context = artifacts
                            .iter()
                            .all(|artifact| artifact.age_ms <= STALE_ARTIFACT_MS)
                            .then_some(loaded.context);
                        Ok((artifacts, context))
                    } else {
                        let hits = self
                            .memory
                            .retrieve_decision_memory(
                                projected_input.as_ref(),
                                5,
                                proof.memory_scope(),
                                None,
                                decision.deadline,
                            )
                            .await?;
                        Ok((memory_artifacts_from_selected_read(&hits, chrono::Utc::now()), None))
                    }
                }
                crate::causal_tree::branch::CapabilityTarget::Document
                | crate::causal_tree::branch::CapabilityTarget::Session => {
                    let principal = proof.source_memory_principal();
                    anyhow::ensure!(
                        principal.workspace_id == proof.workspace_id()
                            && principal.owner_id.as_deref() == Some(proof.owner_id())
                            && principal.session_key.as_deref() == Some(proof.session_key())
                            && principal.require_owner_match == proof.memory_scope().require_owner_match,
                        "selected retrieval principal differs from its source"
                    );
                    if selected_family == crate::causal_tree::branch::CapabilityTarget::Document {
                        let hits = self
                            .memory
                            .retrieve_decision_documents(projected_input.as_ref(), 5, &principal, decision.deadline)
                            .await?;
                        Ok((document_artifacts_from_selected_read(&hits, chrono::Utc::now()), None))
                    } else {
                        let source = proof
                            .source_event_id()
                            .context("selected Session requires a durable source")?;
                        let hits = self
                            .memory
                            .retrieve_decision_session(&principal, source, 5, decision.deadline)
                            .await?;
                        Ok((session_artifacts_from_selected_read(&hits, chrono::Utc::now()), None))
                    }
                }
                _ => anyhow::bail!("unsupported selected retrieval source"),
            }
        })
        .await
        .map_err(|_| AgentRetrievalError::Deadline)?
        .map_err(AgentRetrievalError::Backend)?;
        if Instant::now() >= decision.deadline {
            return Err(AgentRetrievalError::Deadline);
        }
        self.check_live_parity()?;
        Ok(artifacts)
    }

    /// Advance a final-gated selected evidence read into runtime-owned evidence.
    /// A backend failure or deadline becomes a failed/cancelled read step for
    /// the fresh local terminal. Provenance and selection failures stay hard.
    /// Row metadata cannot mint an event receipt.
    async fn observe_selected_agent_memory(
        &self,
        proof: &dyn TurnSourceProof,
        decision: &EngineDecision,
    ) -> Result<MemoryHopObservation, AgentRetrievalError> {
        let hop = decision
            .state
            .lineage
            .hop()
            .checked_add(1)
            .filter(|hop| *hop <= self.generation.effective.causal_tree.policy.max_decision_hops)
            .and_then(NonZeroU8::new)
            .ok_or(AgentRetrievalError::HopLimit)?;
        let selected = match &decision.selection {
            crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. }
            | crate::causal_tree::decision_select::Selection::Recover { candidate_id, .. } => candidate_id.clone(),
            _ => return Err(AgentRetrievalError::Selected(RetrievalIssueError)),
        };
        let (artifacts, custom_context, status) = match self
            .retrieve_selected_agent_evidence_with_context(proof, decision)
            .await
        {
            Ok((artifacts, context)) => (artifacts, context, ObservedStepStatus::Ok),
            Err(AgentRetrievalError::Backend(_)) => (Vec::new(), None, ObservedStepStatus::Failed),
            Err(AgentRetrievalError::Deadline) => (Vec::new(), None, ObservedStepStatus::Cancelled),
            Err(error) => return Err(error),
        };
        let mut next = decision.state.clone();
        let mut steps = next.observed.completed_steps().to_vec();
        steps.push(ObservedStep::recorded_by_runtime(
            ObservedStepKind::Retrieve,
            status,
            Some(selected),
            None,
        ));
        let mut all_artifacts = next.observed.artifacts().to_vec();
        let mut fresh_read = false;
        // Prompt-facing selected evidence excludes stale or future-dated
        // rows. The full observations remain in the decision state for audit.
        let selected_artifacts = artifacts
            .iter()
            .filter(|artifact| artifact.age_ms <= STALE_ARTIFACT_MS)
            .cloned()
            .collect();
        for artifact in artifacts {
            if !all_artifacts.contains(&artifact) {
                fresh_read |= status == ObservedStepStatus::Ok && artifact.age_ms <= STALE_ARTIFACT_MS;
                all_artifacts.push(artifact);
            }
        }
        next.observed = ObservedState::from_runtime_records(steps, all_artifacts, 0)
            .with_exact_call_observations(next.observed.exact_call_observations().cloned());
        next.derived = DerivedState::default();
        next.lineage = HopLineage::After {
            hop,
            prior_decision_id: decision.decision_id.as_uuid(),
        };
        next.built_at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        Ok(MemoryHopObservation {
            state: next,
            fresh_read,
            selected_artifacts,
            custom_context,
        })
    }

    /// Run a source-bound Agent decision through the pinned catalog and gates.
    /// Selected evidence reads and catalog discoveries feed bounded dependent
    /// hops. Selected Execute is retained as exact immutable metadata;
    /// dispatch remains owned by the later constrained execution reader.
    pub(crate) async fn decide_agent_initial(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        let context = self
            .decide_agent_initial_inner(
                proof,
                kind,
                cancellation,
                None,
                None,
                #[cfg(feature = "llm-router")]
                None,
            )
            .await?;
        self.record_issued_decision(proof, context, cancellation).await
    }

    pub(crate) async fn decide_agent_resume(
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
        let (mut context, telemetry) = self
            .decide_agent_initial_inner(
                proof,
                kind,
                cancellation,
                Some(&admitted),
                None,
                #[cfg(feature = "llm-router")]
                None,
            )
            .await?;
        let owned = Arc::get_mut(&mut context).ok_or(AgentDecisionError::UnsupportedAction)?;
        owned.resume = Some(Arc::new(admitted));
        self.record_issued_decision(proof, (context, telemetry), cancellation)
            .await
    }

    async fn decide_agent_initial_inner(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
        resume: Option<&pending_approval::AdmittedDecisionResume>,
        segment_observation: Option<&segment_observation::SegmentObservation>,
        #[cfg(feature = "llm-router")] mut routing: Option<&mut routing::RouterContinuation<'_>>,
    ) -> Result<accounting::AdmissionDecision, AgentDecisionError> {
        if cancellation.is_cancelled() {
            return Err(AgentDecisionError::Cancelled);
        }
        let model = self.resolve_agent_model(proof)?;
        super::ingress_recovery::service_ready(self)
            .await
            .map_err(AgentDecisionError::IngressRecovery)?;
        let budget = self.agent_budget_snapshot(proof)?;
        let observed_latency = self
            .observed_capability_latency(proof.owner_id(), *self.catalog.generation())
            .await;
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(budget.budgets.latency_ms))
            .ok_or(AgentDecisionError::Deadline)?;
        let (accounting, telemetry) = accounting::admission_resources(self, proof, budget.budgets, deadline);
        let config = &self.generation.effective;
        let owner_filesystem = self
            .console_filesystem(proof)
            .await
            .map_err(AgentDecisionError::PendingApproval)?;
        let catalog = self.catalog_view();
        let model_ref: Arc<str> = Arc::from(format!("{}:{}", model.provider_label(), model.model()));
        let effective_exclusions = effective_agent_exclusions(
            config,
            &self.registry,
            &catalog,
            model.model(),
            &self.policy_version,
            &self.state_key,
        );
        let resume_evidence = resume
            .map(|resume| resume.gate_evidence(self, proof))
            .transpose()
            .map_err(AgentDecisionError::PendingApproval)?;
        let trusted = TrustedState::from_runtime(TrustedParts {
            workspace_id: Arc::from(proof.workspace_id()),
            owner_id: Some(Arc::from(proof.owner_id())),
            session_key: Some(Arc::from(proof.session_key())),
            run_id: Some(Arc::from(proof.run_id())),
            task_id: proof.task_id().map(Arc::from),
            // CLI is a local operator surface. The catalog's channel
            // eligibility bit applies to IM channels, not this source.
            channel: None,
            source_event_id: proof.source_event_id().map(Arc::from),
            causation_event_id: resume_evidence.as_ref().map(|evidence| Arc::clone(&evidence.causation)),
            surface: proof.surface(),
            // Runtime effects follow the pinned execution policy. Rehearsal
            // remains independently read-only; G1/G4 still narrow inherited
            // child authority and G9 still binds exact resume restrictions.
            side_effect_mode: match self.security.autonomy {
                crate::security::AutonomyLevel::ReadOnly => crate::causal_tree::state::SideEffectMode::ReadOnly,
                crate::security::AutonomyLevel::Supervised => {
                    crate::causal_tree::state::SideEffectMode::ApprovalRequired
                }
                crate::security::AutonomyLevel::Full => crate::causal_tree::state::SideEffectMode::GuardedWrite,
            },
            exclusions: effective_exclusions.names,
            model_admission: effective_exclusions.model_admission,
            constraints: {
                let mut constraints = Vec::new();
                if proof.owner_isolated() {
                    constraints.push(crate::causal_tree::state::ConstraintCode::new("owner_isolated"));
                }
                if resume_evidence.is_some() {
                    constraints.push(crate::causal_tree::state::ConstraintCode::new("resume_exact_call"));
                }
                if proof.intercepts_writes() {
                    constraints.push(crate::causal_tree::state::ConstraintCode::new("intercepts_writes"));
                }
                if owner_filesystem.is_some() {
                    constraints.push(crate::causal_tree::state::ConstraintCode::new(
                        "owner_private_filesystem_v1",
                    ));
                }
                constraints
            },
            approval_pending: false,
            approval_grants: resume_evidence
                .as_ref()
                .map_or_else(Vec::new, |evidence| evidence.grants.clone()),
            budgets: budget.budgets,
            delivery: DeliveryConstraints {
                // Only the sealed actual owner-review ingress may advertise approval.
                interactive: kind == AgentTurnKind::Repl || proof.approval_review_adapter().is_some(),
                can_ask_question: kind == AgentTurnKind::Repl || proof.approval_review_adapter().is_some(),
                approval_channel: proof.approval_review_adapter().is_some(),
            },
            catalog_generation: catalog.generation(),
            policy_version: Arc::clone(&self.policy_version),
            resolved_model: Arc::clone(&model_ref),
        });
        let projected_input = crate::multimodal::project_image_markers_for_routing(proof.routing_input()).into_owned();
        let mut state = CausalState {
            trusted,
            observed: segment_observation::initial_observations(segment_observation, deadline, cancellation)?,
            predicted: None,
            derived: DerivedState::default(),
            has_conversation_history: proof.has_conversation_history(),
            routing_input: RawRoutingInput::new(projected_input.clone()),
            goal: RawRoutingInput::new(projected_input),
            lineage: HopLineage::First,
            built_at_ms: u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
        };
        #[cfg(feature = "llm-router")]
        let mut routing_hop = None;
        #[cfg(feature = "llm-router")]
        let mut routing_closed = false;
        #[cfg(feature = "llm-router")]
        if let Some(continuation) = routing.as_deref_mut() {
            let (decision, next) = boxed_router_history_field(|| {
                self.router_history_hop(&state, deadline, continuation, cancellation, &accounting)
            })
            .await?;
            match next {
                Some(next) => state = next,
                None => {
                    state = decision.state.clone();
                    routing_closed = true;
                }
            }
            routing_hop = Some(decision);
        }
        #[cfg(feature = "llm-router")]
        let proof: &dyn TurnSourceProof = routing
            .as_deref()
            .map_or(proof, |continuation| continuation.source().as_ref());
        #[cfg(feature = "llm-router")]
        let router_transition = routing.as_deref().and_then(routing::RouterContinuation::transition);
        #[cfg(feature = "llm-router")]
        let has_router_transition = router_transition.is_some();
        #[cfg(not(feature = "llm-router"))]
        let has_router_transition = false;
        #[cfg(feature = "llm-router")]
        let routing_failed = routing.as_deref().is_some_and(routing::RouterContinuation::failed);
        #[cfg(not(feature = "llm-router"))]
        let routing_failed = false;
        #[cfg(feature = "llm-router")]
        let budget = if let Some(compaction) = routing
            .as_deref()
            .and_then(routing::RouterContinuation::main_compaction)
        {
            let mut compaction = compaction.clone();
            let original_limit = budget
                .main_compaction
                .max_context_tokens
                .saturating_sub(budget.main_compaction.reserve_tokens);
            let admitted_limit = compaction.max_context_tokens.saturating_sub(compaction.reserve_tokens);
            if admitted_limit > original_limit {
                compaction.reserve_tokens = compaction
                    .max_context_tokens
                    .checked_sub(original_limit)
                    .ok_or(AgentDecisionError::Deadline)?;
            }
            AgentDecisionBudget {
                budgets: budget.budgets,
                main_compaction: compaction,
            }
        } else {
            budget
        };
        // Only the history path re-admits a model. The ordinary path retains
        // its existing resolved value and allocation count.
        #[cfg(feature = "llm-router")]
        let (model, model_ref) = if routing.is_some() {
            let model = self.resolve_agent_model(proof)?;
            let model_ref = Arc::from(format!("{}:{}", model.provider_label(), model.model()));
            (model, model_ref)
        } else {
            (model, model_ref)
        };
        let decision_request_model = self.factory.request_model().or_else(|| Some(model.model()));
        let cost_model = super::turn_cost::RuntimeCostModel::new(&model, config, &observed_latency);
        let mut discovered_refs = DiscoveredRefs::empty();
        let call_fingerprints = resume_evidence
            .as_ref()
            .map_or_else(BTreeMap::new, |evidence| evidence.calls.clone());
        let replayed_non_idempotent = BTreeSet::new();
        let gates = GateInputs {
            policy: &self.security,
            owner_id: proof.owner_id(),
            sender: proof.policy_sender(),
            policy_channel: proof.policy_channel(),
            chat_type: proof.policy_chat_type(),
            model_ref: &model_ref,
            model_can_chat: true,
            model_can_call_tools: self.model_can_call_tools(proof),
            decision_endpoint_ready: true,
            child_scope: proof.child_scope().map(|scope| scope.gate_inputs()),
            verified_approval_calls: resume_evidence
                .as_ref()
                .map_or_else(BTreeMap::new, |evidence| evidence.approvals.clone()),
            call_fingerprints: &call_fingerprints,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &replayed_non_idempotent,
            retry_limit: None,
        };

        let pinned_rehearsal = PinnedCatalogRehearsal { catalog: &catalog };
        let rehearsal_query = state.routing_input.as_str().to_owned();
        let memory_rehearsal = matches!(self.memory.name(), "sqlite" | "lucid").then_some(ScopedMemoryRehearsal {
            memory: self.memory.as_ref(),
            scope: proof.memory_scope(),
            query: &rehearsal_query,
            principal: proof.owner_id(),
            key: &self.state_key,
        });
        let rehearsal_ports = RehearsalPorts {
            memory: memory_rehearsal.as_ref().map(|probe| probe as &dyn MemoryProbe),
            catalog: Some(&pinned_rehearsal),
            skill: Some(&pinned_rehearsal),
        };
        let configured_delegation_target = self.configured_delegation_target(proof);
        // A live registry drift cannot authorize a stale work selection. The
        // source and PMM model have already been checked above; rebuild only
        // a local gated Close directive from the pinned turn facts. The fresh
        // Router transitions retain every completed hop; ordinary drift keeps
        // its established single-close representation.
        let terminal_after_catalog_drift = |source: &CausalState,
                                            prior_hops: Vec<EngineDecision>|
         -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
            if cancellation.is_cancelled() && !has_router_transition {
                return Err(AgentDecisionError::Cancelled);
            }
            #[cfg(feature = "llm-router")]
            let prior_hops = if router_transition.is_some() {
                prior_hops
            } else {
                Vec::new()
            };
            #[cfg(not(feature = "llm-router"))]
            let prior_hops = {
                let _ = prior_hops;
                Vec::<EngineDecision>::new()
            };
            let mut terminal_state = source.clone();
            if prior_hops.is_empty() {
                terminal_state.observed = ObservedState::from_runtime_records(Vec::new(), Vec::new(), 0)
                    .with_exact_call_observations(terminal_state.observed.exact_call_observations().cloned());
            }
            terminal_state.lineage = match prior_hops.last() {
                Some(prior) => HopLineage::After {
                    hop: u8::try_from(prior_hops.len())
                        .ok()
                        .and_then(NonZeroU8::new)
                        .ok_or(AgentDecisionError::UnsupportedAction)?,
                    prior_decision_id: prior.decision_id.as_uuid(),
                },
                None => HopLineage::First,
            };
            if has_router_transition {
                let mut parts = terminal_state.trusted.parts().clone();
                parts.budgets.latency_ms = parts
                    .budgets
                    .latency_ms
                    .min(u64::try_from(deadline.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(0));
                terminal_state.trusted = TrustedState::from_runtime(parts);
            }
            terminal_state.derived = DerivedState::default();
            terminal_state.built_at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
            let no_work_refs = DiscoveredRefs::empty();
            let decision_id = DecisionId::new_runtime();
            self.emit_agent_phase(proof, decision_id, CausalEventKind::SnapshotCreated, None);
            let inputs = EngineInputs {
                state: &terminal_state,
                catalog: &catalog,
                config: &config.causal_tree,
                key: &self.state_key,
                decision_id,
                cost_model: &cost_model,
                discovered_refs: &no_work_refs,
                gates: &gates,
                delegation: None,
                delegation_max_depth: 0,
                prediction: self.factory.prediction_port(Some(&accounting)),
                rehearsal_ports: &rehearsal_ports,
                request_limits: RequestLimits {
                    max_request_bytes: config.causal_tree.backend.max_request_bytes,
                    ..RequestLimits::default()
                },
                model: decision_request_model,
                can_deliver_text: true,
                may_delegate: false,
                delegation_target: configured_delegation_target,
                terminal_only: true,
                allow_terminal_answer: false,
                deadline,
                #[cfg(test)]
                inject_builtin_failure: false,
                #[cfg(test)]
                after_prediction_pause: None,
            };
            let prepared = match resume_evidence.as_ref() {
                Some(evidence) => {
                    CausalDecisionEngine::prepare_hop_with_resume_target(&inputs, Some(&evidence.candidate_reference))?
                }
                None => CausalDecisionEngine::prepare_hop(&inputs)?,
            };
            self.emit_agent_phase(
                proof,
                decision_id,
                CausalEventKind::CandidatesGenerated,
                Some(prepared.candidate_count()),
            );
            let decision = CausalDecisionEngine::decide_terminal_prepared(prepared)?;
            self.emit_agent_phase(
                proof,
                decision_id,
                CausalEventKind::CandidateGated,
                Some(decision.pre_commit.outcomes.len()),
            );
            let directive = directive_from_selection(
                &decision,
                &DirectiveContext {
                    current_generation: catalog.generation(),
                    approval_expires_at: chrono::Utc::now(),
                    discovery_limit: 0,
                },
            )?;
            if !matches!(
                directive,
                TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. }
            ) {
                return Err(AgentDecisionError::UnsupportedAction);
            }
            if cancellation.is_cancelled() && !has_router_transition {
                return Err(AgentDecisionError::Cancelled);
            }
            Ok(Arc::new(TurnDecisionContext {
                model_catalog: self.provider_catalog.release_pin(),
                resume: None,
                accounting: accounting.retained_scope(),
                owner_filesystem: owner_filesystem.clone(),
                source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
                service_id: self.service_id,
                record_admitted: AtomicBool::new(false),
                recursive_allowance: self.recursive_allowances.retain(proof),
                main_parameters: Default::default(),
                execution_trace: std::sync::OnceLock::new(),
                #[cfg(feature = "llm-router")]
                automix_deadline: std::sync::OnceLock::new(),
                served_auxiliary: std::sync::OnceLock::new(),
                execution_closed: AtomicBool::new(false),
                approval_collection_started: AtomicBool::new(false),
                collected_pending: std::sync::OnceLock::new(),
                discovery_offers: std::sync::OnceLock::new(),
                discovery_admissions: std::sync::OnceLock::new(),
                engine: decision,
                hop_ids: prior_hops
                    .iter()
                    .map(|hop| hop.decision_id)
                    .chain(std::iter::once(decision_id))
                    .collect(),
                prior_hops,
                directive,
                main_compaction: budget.main_compaction.clone(),
                execution_refs: BTreeSet::new(),
                selected_retrieved: Vec::new(),
                selected_custom_memory_context: None,
                #[cfg(feature = "llm-router")]
                router_transition: router_transition.clone(),
            }))
        };
        let mut stop_hops = routing_failed;
        let mut allow_terminal_answer = false;
        let mut hop_ids = Vec::new();
        let mut prior_hops = Vec::new();
        #[cfg(feature = "llm-router")]
        if let Some(decision) = routing_hop.take() {
            hop_ids.push(decision.decision_id);
            if routing_closed {
                let directive = directive_from_selection(
                    &decision,
                    &DirectiveContext {
                        current_generation: catalog.generation(),
                        approval_expires_at: chrono::Utc::now(),
                        discovery_limit: 1,
                    },
                )?;
                let execution_refs = if matches!(directive, TurnExecutionDirective::AnswerDirectly) {
                    direct_answer_refs(&decision, &catalog, &gates, &BTreeSet::new())
                } else {
                    BTreeSet::new()
                };
                return Ok((
                    Arc::new(TurnDecisionContext {
                        model_catalog: self.provider_catalog.release_pin(),
                        resume: None,
                        accounting: accounting.retained_scope(),
                        owner_filesystem: owner_filesystem.clone(),
                        source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
                        service_id: self.service_id,
                        record_admitted: AtomicBool::new(false),
                        recursive_allowance: self.recursive_allowances.retain(proof),
                        main_parameters: Default::default(),
                        execution_trace: std::sync::OnceLock::new(),
                        #[cfg(feature = "llm-router")]
                        automix_deadline: std::sync::OnceLock::new(),
                        served_auxiliary: std::sync::OnceLock::new(),
                        execution_closed: AtomicBool::new(false),
                        approval_collection_started: AtomicBool::new(false),
                        collected_pending: std::sync::OnceLock::new(),
                        discovery_offers: std::sync::OnceLock::new(),
                        discovery_admissions: std::sync::OnceLock::new(),
                        engine: decision,
                        prior_hops,
                        directive,
                        main_compaction: budget.main_compaction,
                        hop_ids,
                        execution_refs,
                        selected_retrieved: Vec::new(),
                        selected_custom_memory_context: None,
                        router_transition: None,
                    }),
                    telemetry,
                ));
            }
            prior_hops.push(decision);
        }
        let mut selected_retrieved = Vec::new();
        let mut selected_custom_memory_context = None;
        let max_hops = config.causal_tree.policy.max_decision_hops;
        for hop in state.lineage.hop()..=max_hops {
            if cancellation.is_cancelled() {
                if has_router_transition {
                    return terminal_after_catalog_drift(&state, prior_hops).map(|context| (context, telemetry));
                }
                return Err(AgentDecisionError::Cancelled);
            }
            if self.check_live_parity().is_err() {
                return terminal_after_catalog_drift(&state, prior_hops).map(|context| (context, telemetry));
            }
            let terminal_only = stop_hops || hop == max_hops || Instant::now() >= deadline;
            let decision_id = DecisionId::new_runtime();
            self.emit_agent_phase(proof, decision_id, CausalEventKind::SnapshotCreated, None);
            let delegation = (!terminal_only && resume.is_none())
                .then(|| self.outgoing_delegation(proof, &state, decision_id, &discovered_refs, &catalog))
                .flatten();
            let inputs = EngineInputs {
                state: &state,
                catalog: &catalog,
                config: &config.causal_tree,
                key: &self.state_key,
                decision_id,
                cost_model: &cost_model,
                discovered_refs: &discovered_refs,
                gates: &gates,
                delegation: delegation
                    .as_ref()
                    .map(|issued| (&issued.parent, issued.requested.as_slice())),
                delegation_max_depth: delegation
                    .as_ref()
                    .map_or(0, |issued| issued.parent.authority.remaining_depth),
                prediction: self.factory.prediction_port(Some(&accounting)),
                rehearsal_ports: &rehearsal_ports,
                request_limits: RequestLimits {
                    max_request_bytes: config.causal_tree.backend.max_request_bytes,
                    ..RequestLimits::default()
                },
                model: decision_request_model,
                can_deliver_text: true,
                may_delegate: delegation.is_some(),
                delegation_target: configured_delegation_target,
                terminal_only,
                allow_terminal_answer,
                deadline,
                #[cfg(test)]
                inject_builtin_failure: false,
                #[cfg(test)]
                after_prediction_pause: None,
            };
            let prepared = match resume_evidence.as_ref() {
                Some(evidence) => {
                    CausalDecisionEngine::prepare_hop_with_resume_target(&inputs, Some(&evidence.candidate_reference))?
                }
                None => CausalDecisionEngine::prepare_hop(&inputs)?,
            };
            if cancellation.is_cancelled() {
                if has_router_transition {
                    return terminal_after_catalog_drift(&state, prior_hops).map(|context| (context, telemetry));
                }
                return Err(AgentDecisionError::Cancelled);
            }
            self.emit_agent_phase(
                proof,
                decision_id,
                CausalEventKind::CandidatesGenerated,
                Some(prepared.candidate_count()),
            );
            let decision = if terminal_only {
                CausalDecisionEngine::decide_terminal_prepared(prepared)?
            } else {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        if has_router_transition { return terminal_after_catalog_drift(&state, prior_hops).map(|context| (context, telemetry)); }
                        return Err(AgentDecisionError::Cancelled);
                    },
                    decided = CausalDecisionEngine::decide_prepared_with_collection(prepared, proof.approval_review_adapter()) => decided?,
                }
            };
            if cancellation.is_cancelled() {
                if has_router_transition {
                    return terminal_after_catalog_drift(&state, prior_hops).map(|context| (context, telemetry));
                }
                return Err(AgentDecisionError::Cancelled);
            }
            self.emit_agent_phase(
                proof,
                decision.decision_id,
                CausalEventKind::CandidateGated,
                Some(decision.pre_commit.outcomes.len()),
            );
            if decision.scored_count > 0 {
                self.emit_agent_phase(
                    proof,
                    decision.decision_id,
                    CausalEventKind::CandidateScored,
                    Some(decision.scored_count),
                );
            }
            hop_ids.push(decision.decision_id);
            if self.check_live_parity().is_err() {
                return terminal_after_catalog_drift(&state, prior_hops).map(|context| (context, telemetry));
            }
            let context = DirectiveContext {
                current_generation: catalog.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            };
            let directive = directive_from_selection(&decision, &context)?;
            let prior_evidence = decision.state.evidence_fingerprint(&self.state_key);
            let mut discovery_change = None;
            let mut retrieval_progress = None;
            match &directive {
                TurnExecutionDirective::Retrieve { .. } => {
                    let observed = tokio::select! {
                        biased;
                        () = cancellation.cancelled() => {
                        if has_router_transition { return terminal_after_catalog_drift(&state, prior_hops).map(|context| (context, telemetry)); }
                        return Err(AgentDecisionError::Cancelled);
                    },
                        observed = self.observe_selected_agent_memory(proof, &decision) => observed,
                    };
                    let observed = match observed {
                        Ok(observed) => observed,
                        Err(AgentRetrievalError::Parity(_)) => {
                            return terminal_after_catalog_drift(&state, prior_hops)
                                .map(|context| (context, telemetry));
                        }
                        Err(error) => {
                            if has_router_transition {
                                return terminal_after_catalog_drift(&state, prior_hops)
                                    .map(|context| (context, telemetry));
                            }
                            return Err(error.into());
                        }
                    };
                    if cancellation.is_cancelled() {
                        if has_router_transition {
                            return terminal_after_catalog_drift(&state, prior_hops)
                                .map(|context| (context, telemetry));
                        }
                        return Err(AgentDecisionError::Cancelled);
                    }
                    allow_terminal_answer = observed.fresh_read;
                    selected_retrieved = observed.selected_artifacts;
                    selected_custom_memory_context = observed.custom_context;
                    let mut comparison = observed.state.clone();
                    comparison.derived = decision.state.derived.clone();
                    retrieval_progress = Some(comparison.evidence_fingerprint(&self.state_key) != prior_evidence);
                    state = observed.state;
                }
                TurnExecutionDirective::Discover { .. } => {
                    let hop = decision
                        .state
                        .lineage
                        .hop()
                        .checked_add(1)
                        .filter(|hop| *hop <= max_hops)
                        .and_then(NonZeroU8::new)
                        .ok_or(AgentDiscoveryError::HopLimit)?;
                    let selected = match &decision.selection {
                        crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. }
                        | crate::causal_tree::decision_select::Selection::Recover { candidate_id, .. } => {
                            candidate_id.clone()
                        }
                        _ => return Err(AgentDiscoveryError::Selected(DiscoveryIssueError).into()),
                    };
                    let selected_refs = match self.discover_selected_agent_refs(proof, &decision, &catalog) {
                        Ok(refs) => refs,
                        Err(AgentDiscoveryError::Parity(_)) => {
                            return terminal_after_catalog_drift(&state, prior_hops)
                                .map(|context| (context, telemetry));
                        }
                        Err(error) => {
                            if has_router_transition {
                                return terminal_after_catalog_drift(&state, prior_hops)
                                    .map(|context| (context, telemetry));
                            }
                            return Err(error.into());
                        }
                    };
                    let previous_refs = discovered_refs.clone();
                    let added_refs = discovered_refs.extend_selected(selected_refs);
                    // Discovery is a bounded, gate-checked local read. Once it
                    // completes, its terminal fallback may answer from the
                    // resulting surface regardless of whether the source text
                    // contains a path, URL, code, or exact capability name.
                    // A rejected configured decision attempt remains fail
                    // closed: its builtin recovery may finish discovery, but
                    // must not turn the rejected decision into an answer.
                    // The discovered refs still pass the normal G1-G9 gates;
                    // this changes only the terminal fallback after the read.
                    allow_terminal_answer = discovery_can_answer(&decision);
                    discovery_change = Some((previous_refs, added_refs));
                    state = decision.state.clone();
                    let mut steps = state.observed.completed_steps().to_vec();
                    steps.push(ObservedStep::recorded_by_runtime(
                        ObservedStepKind::Discover,
                        ObservedStepStatus::Ok,
                        Some(selected),
                        None,
                    ));
                    state.observed = ObservedState::from_runtime_records(
                        steps,
                        state.observed.artifacts().to_vec(),
                        state.observed.retry_count(),
                    )
                    .with_exact_call_observations(state.observed.exact_call_observations().cloned());
                    state.derived = DerivedState::default();
                    state.lineage = HopLineage::After {
                        hop,
                        prior_decision_id: decision.decision_id.as_uuid(),
                    };
                    state.built_at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
                }
                TurnExecutionDirective::AnswerDirectly => {
                    let selected_discovery_refs = discovered_refs.iter().cloned().collect();
                    let execution_refs = direct_answer_refs(&decision, &catalog, &gates, &selected_discovery_refs);
                    if cancellation.is_cancelled() {
                        if has_router_transition {
                            return terminal_after_catalog_drift(&state, prior_hops)
                                .map(|context| (context, telemetry));
                        }
                        return Err(AgentDecisionError::Cancelled);
                    }
                    return Ok((
                        Arc::new(TurnDecisionContext {
                            model_catalog: self.provider_catalog.release_pin(),
                            resume: None,
                            accounting: accounting.retained_scope(),
                            owner_filesystem: owner_filesystem.clone(),
                            source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
                            service_id: self.service_id,
                            record_admitted: AtomicBool::new(false),
                            recursive_allowance: self.recursive_allowances.retain(proof),
                            main_parameters: Default::default(),
                            execution_trace: std::sync::OnceLock::new(),
                            #[cfg(feature = "llm-router")]
                            automix_deadline: std::sync::OnceLock::new(),
                            served_auxiliary: std::sync::OnceLock::new(),
                            execution_closed: AtomicBool::new(false),
                            approval_collection_started: AtomicBool::new(false),
                            collected_pending: std::sync::OnceLock::new(),
                            discovery_offers: std::sync::OnceLock::new(),
                            discovery_admissions: std::sync::OnceLock::new(),
                            engine: decision,
                            prior_hops,
                            directive,
                            main_compaction: budget.main_compaction,
                            hop_ids,
                            execution_refs,
                            selected_retrieved,
                            selected_custom_memory_context,
                            #[cfg(feature = "llm-router")]
                            router_transition: router_transition.clone(),
                        }),
                        telemetry,
                    ));
                }
                TurnExecutionDirective::CollectApprovalArguments(_)
                | TurnExecutionDirective::Clarify { .. }
                | TurnExecutionDirective::RequestApproval { .. }
                | TurnExecutionDirective::Refuse { .. } => {
                    if cancellation.is_cancelled() {
                        if has_router_transition {
                            return terminal_after_catalog_drift(&state, prior_hops)
                                .map(|context| (context, telemetry));
                        }
                        return Err(AgentDecisionError::Cancelled);
                    }
                    return Ok((
                        Arc::new(TurnDecisionContext {
                            model_catalog: self.provider_catalog.release_pin(),
                            resume: None,
                            accounting: accounting.retained_scope(),
                            owner_filesystem: owner_filesystem.clone(),
                            source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
                            service_id: self.service_id,
                            record_admitted: AtomicBool::new(false),
                            recursive_allowance: self.recursive_allowances.retain(proof),
                            main_parameters: Default::default(),
                            execution_trace: std::sync::OnceLock::new(),
                            #[cfg(feature = "llm-router")]
                            automix_deadline: std::sync::OnceLock::new(),
                            served_auxiliary: std::sync::OnceLock::new(),
                            execution_closed: AtomicBool::new(false),
                            approval_collection_started: AtomicBool::new(false),
                            collected_pending: std::sync::OnceLock::new(),
                            discovery_offers: std::sync::OnceLock::new(),
                            discovery_admissions: std::sync::OnceLock::new(),
                            engine: decision,
                            prior_hops,
                            directive,
                            main_compaction: budget.main_compaction,
                            hop_ids,
                            execution_refs: BTreeSet::new(),
                            selected_retrieved,
                            selected_custom_memory_context,
                            #[cfg(feature = "llm-router")]
                            router_transition: router_transition.clone(),
                        }),
                        telemetry,
                    ));
                }
                TurnExecutionDirective::Execute { .. } | TurnExecutionDirective::Delegate { .. } => {
                    let execution_refs = selected_execute_refs(&decision, &directive, &catalog)?;
                    if self.check_live_parity().is_err() {
                        return terminal_after_catalog_drift(&state, prior_hops).map(|context| (context, telemetry));
                    }
                    if cancellation.is_cancelled() {
                        if has_router_transition {
                            return terminal_after_catalog_drift(&state, prior_hops)
                                .map(|context| (context, telemetry));
                        }
                        return Err(AgentDecisionError::Cancelled);
                    }
                    return Ok((
                        Arc::new(TurnDecisionContext {
                            model_catalog: self.provider_catalog.release_pin(),
                            resume: None,
                            accounting: accounting.retained_scope(),
                            owner_filesystem: owner_filesystem.clone(),
                            source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
                            service_id: self.service_id,
                            record_admitted: AtomicBool::new(false),
                            recursive_allowance: self.recursive_allowances.retain(proof),
                            main_parameters: Default::default(),
                            execution_trace: std::sync::OnceLock::new(),
                            #[cfg(feature = "llm-router")]
                            automix_deadline: std::sync::OnceLock::new(),
                            served_auxiliary: std::sync::OnceLock::new(),
                            execution_closed: AtomicBool::new(false),
                            approval_collection_started: AtomicBool::new(false),
                            collected_pending: std::sync::OnceLock::new(),
                            discovery_offers: std::sync::OnceLock::new(),
                            discovery_admissions: std::sync::OnceLock::new(),
                            engine: decision,
                            prior_hops,
                            directive,
                            main_compaction: budget.main_compaction,
                            hop_ids,
                            execution_refs,
                            selected_retrieved,
                            selected_custom_memory_context,
                            #[cfg(feature = "llm-router")]
                            router_transition: router_transition.clone(),
                        }),
                        telemetry,
                    ));
                }
            };
            let mut parts = state.trusted.parts().clone();
            parts.budgets = accounting.remaining_budgets();
            state.trusted = TrustedState::from_runtime(parts);
            // A normal candidate preparation includes newly discovered work
            // refs. A terminal-only preparation would hide them and falsely
            // classify successful discovery as no progress. The current
            // unproven-budget factory admits only the local builtin.
            // The test-only observation returns each original operand unchanged.
            // Short-circuit order and each clock/fingerprint evaluation are preserved.
            macro_rules! progress_operand {
                ($label:literal, $value:expr) => {{
                    let value = $value;
                    #[cfg(test)]
                    progress_diagnostic::record($label, u64::from(value));
                    value
                }};
            }
            #[cfg(test)]
            progress_diagnostic::with_active(|trace| {
                trace.push("hop", u64::from(hop));
                trace.push("max_hops", u64::from(max_hops));
                if let Some((previous, _)) = &discovery_change {
                    trace.push("previous_refs", previous.iter().count() as u64);
                    trace.push("current_refs", discovered_refs.iter().count() as u64);
                }
            });
            if progress_operand!("hop_limit", hop >= max_hops)
                || progress_operand!("deadline_expired", Instant::now() >= deadline)
                || progress_operand!(
                    "no_added_discovery_refs",
                    discovery_change.as_ref().is_some_and(|(_, added)| !added)
                )
                || progress_operand!("retrieval_no_progress", retrieval_progress == Some(false))
            {
                stop_hops = true;
            } else if retrieval_progress == Some(true) {
                // Only new observed evidence authorizes a dependent Retrieve
                // hop. A changing G6 latency gate cannot turn an empty read
                // into progress.
                stop_hops = false;
            } else {
                let Some((previous_refs, _)) = discovery_change else {
                    return Err(AgentDecisionError::UnsupportedAction);
                };
                let next_inputs = EngineInputs {
                    state: &state,
                    catalog: &catalog,
                    config: &config.causal_tree,
                    key: &self.state_key,
                    decision_id: DecisionId::new_runtime(),
                    cost_model: &cost_model,
                    discovered_refs: &discovered_refs,
                    gates: &gates,
                    delegation: None,
                    delegation_max_depth: 0,
                    prediction: self.factory.prediction_port(Some(&accounting)),
                    rehearsal_ports: &rehearsal_ports,
                    request_limits: RequestLimits {
                        max_request_bytes: config.causal_tree.backend.max_request_bytes,
                        ..RequestLimits::default()
                    },
                    model: decision_request_model,
                    can_deliver_text: true,
                    may_delegate: false,
                    delegation_target: configured_delegation_target,
                    terminal_only: false,
                    allow_terminal_answer,
                    deadline,
                    #[cfg(test)]
                    inject_builtin_failure: false,
                    #[cfg(test)]
                    after_prediction_pause: None,
                };
                let next_prepared = match resume_evidence.as_ref() {
                    Some(evidence) => CausalDecisionEngine::prepare_hop_with_resume_target(
                        &next_inputs,
                        Some(&evidence.candidate_reference),
                    )?,
                    None => CausalDecisionEngine::prepare_hop(&next_inputs)?,
                };
                if cancellation.is_cancelled() {
                    return Err(AgentDecisionError::Cancelled);
                }
                // Compare two preparations at the same post-hop latency.
                // A shrinking budget alone cannot turn an empty search
                // into apparent discovery progress.
                let previous_inputs = EngineInputs {
                    discovered_refs: &previous_refs,
                    ..next_inputs.clone()
                };
                let previous_prepared = match resume_evidence.as_ref() {
                    Some(evidence) => CausalDecisionEngine::prepare_hop_with_resume_target(
                        &previous_inputs,
                        Some(&evidence.candidate_reference),
                    )?,
                    None => CausalDecisionEngine::prepare_hop(&previous_inputs)?,
                };
                if cancellation.is_cancelled() {
                    return Err(AgentDecisionError::Cancelled);
                }
                stop_hops = crate::causal_tree::decision_engine::discovery_has_no_progress(
                    || {
                        progress_operand!(
                            "same_evidence",
                            next_prepared.evidence_fingerprint() == previous_prepared.evidence_fingerprint()
                        )
                    },
                    || {
                        progress_operand!(
                            "same_candidate_bodies",
                            !next_prepared.candidate_bodies_changed_from_prepared(&previous_prepared)
                        )
                    },
                    || {
                        !progress_operand!(
                            "same_direct_readiness",
                            !next_prepared.gained_direct_candidate_from_prepared(&previous_prepared)
                        )
                    },
                );
            }
            #[cfg(test)]
            progress_diagnostic::record("stop_hops", u64::from(stop_hops));
            // Keep every completed hop's actual gates, prediction and
            // candidates. IDs alone cannot reconstruct a durable audit row.
            prior_hops.push(decision);
        }
        Err(AgentDecisionError::UnsupportedAction)
    }

    async fn observed_capability_latency(
        &self,
        owner_id: &str,
        generation: CatalogGeneration,
    ) -> BTreeMap<String, u32> {
        let Some(workspace_id) = self.store.workspace_id() else {
            return BTreeMap::new();
        };
        let store = Arc::clone(&self.store);
        let owner_id = owner_id.to_owned();
        latency_snapshot::read(move || {
            store
                .reader()
                .capability_latency_p50(&workspace_id, &owner_id, &generation.to_hex())
        })
        .await
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
        let remaining = crate::cost::tracker::remaining_micro_usd_read_only_with_security_ceiling(
            &config.cost,
            &config.workspace_dir,
            self.security.max_cost_per_day_cents,
        )
        .map_err(AgentDecisionBudgetError::Spend)?;
        let remaining = self
            .recursive_money(proof)
            .map_or(remaining, |money| remaining.min(money.load(Ordering::Acquire)));
        Ok(AgentDecisionBudget {
            budgets: TurnBudgets {
                tokens: u64::try_from(reservation.decision_tokens).unwrap_or(u64::MAX),
                latency_ms: config.causal_tree.policy.extra_latency_budget_ms,
                micro_usd: remaining,
            },
            main_compaction,
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

    /// Resolve the exact selected model again against this service's pinned
    /// PMM catalog and credential owner. The proof carries only the source's
    /// selected spelling; it cannot install a model capability fact.
    /// A routed provider receives either the original hint pair or its
    /// source-bound destination, never a mixture of those two identities.
    /// The original spelling keeps its fast path; execution admission still
    /// validates the full binding and gates before any provider call.
    pub(crate) fn validate_agent_execution_model(
        &self,
        proof: &dyn TurnSourceProof,
        provider: &str,
        model: &str,
    ) -> Result<(), TurnModelError> {
        if proof.provider_name() == provider && proof.model_name() == model {
            return Ok(());
        }
        let resolved = self.resolve_agent_model(proof)?;
        if resolved.provider_label() == provider && resolved.model() == model {
            Ok(())
        } else {
            Err(TurnModelError::SelectionDrift)
        }
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
        let config = &self.generation.effective;
        crate::providers::selection::check_with(
            config,
            &self.provider_catalog,
            proof.provider_name(),
            proof.model_name(),
            self.credential_for_proof(proof),
            &Requirement::chat().with(Feature::NativeTools),
            UnknownPolicy::Invocation,
        )
        .is_ok_and(|resolved| {
            resolved.verification() == &Verification::Verified && proof.model_binding().matches_identity(&resolved)
        })
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
    // plane is disabled. An unavailable key leaves the factory builtin-only.
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
    let durable_events = if durable_key {
        Some(Arc::new(super::causal_events::DurableCausalEventSink::new(
            &build.generation,
            Arc::clone(&build.memory),
            Arc::clone(&build.events),
        )?))
    } else {
        None
    };
    let events: Arc<dyn CausalEventSink> = durable_events.as_ref().map_or_else(
        || Arc::clone(&build.events),
        |events| Arc::clone(events) as Arc<dyn CausalEventSink>,
    );
    let store = store_preparation::open_store(
        Arc::clone(&build.generation),
        Arc::clone(&events),
        durable_events.clone(),
        durable_key,
    )
    .await?;
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
    let recordable = durable_key && store.mode() == StoreMode::ReadWrite;
    Ok(Arc::new(TurnDecisionService {
        child_recovery: Default::default(),
        recursive_allowances: Default::default(),
        memory_loader: None,
        loop_runtime: None,
        publication_policy: build.surface_policy,
        ordinary_parent: None,
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
pub(crate) use tests::exact_call_approval::collection::exact_file_collection_for_review as run_chat_approval_contract_fixture;
#[cfg(test)]
pub(crate) use tests::with_worker_probe_manifest;

#[cfg(test)]
mod tests;

fn selected_custom_memory_read_scope(
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
