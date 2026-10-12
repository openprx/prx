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

mod approval;
mod child_recovery;
mod delegation;
mod discovery;
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
use crate::causal_tree::branch::{CapabilityRef, CapabilityTarget, CatalogGeneration};
use crate::causal_tree::candidate_generator::DiscoveredRefs;
use crate::causal_tree::decision_engine::{
    CausalDecisionEngine, DiscoveryIssueError, EngineDecision, EngineError, EngineInputs, RetrievalIssueError,
};
use crate::causal_tree::decision_rehearsal::RehearsalPorts;
use crate::causal_tree::decision_rehearsal::{CatalogProbe, MemoryProbe, PinnedCount, ProbeUnavailable, SkillProbe};
use crate::causal_tree::directive::{
    DirectiveContext, DirectiveError, TurnExecutionDirective, directive_from_selection, render_terminal_directive,
};
use crate::causal_tree::gates::GateInputs;
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

#[derive(Debug, thiserror::Error)]
pub(crate) enum TurnServiceBuildError {
    #[error("child service does not match its issued parent authority")]
    ChildAuthority,
    #[error("catalog sources do not match the pinned configuration generation")]
    SourceGeneration,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Parity(#[from] ParityError),
    #[error(transparent)]
    Factory(#[from] FactoryBuildError),
    #[error("pinned decision policy cannot be fingerprinted")]
    PolicyFingerprint(#[source] serde_json::Error),
    #[error("durable causal events require an active runtime")]
    EventRuntime(#[from] tokio::runtime::TryCurrentError),
    #[error("enabled causal decisions require a durable message-event memory backend")]
    EventStoreUnsupported,
    #[error("enabled causal decisions require user message event recording")]
    InboundEventsDisabled,
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
    owner_filesystem: Option<Arc<crate::tools::owner_filesystem::OwnerFilesystem>>,
    accounting: Option<Arc<crate::runtime::decision_accounting::AccountingScope>>,
    resume: Option<Arc<pending_approval::AdmittedDecisionResume>>,
    source_actor: (Arc<str>, Arc<str>),
    service_id: uuid::Uuid,
    record_admitted: AtomicBool,
    execution_trace: std::sync::OnceLock<crate::tools::capability::execution_trace::ExecutionTrace>,
    served_auxiliary: std::sync::OnceLock<crate::agent::terminal::served_auxiliary::ServedAuxiliaryLink>,
    execution_closed: AtomicBool,
    approval_collection_started: AtomicBool,
    collected_pending: std::sync::OnceLock<Box<pending_approval::CollectedPendingCommitment>>,
    discovery_offers: std::sync::OnceLock<parking_lot::Mutex<BTreeMap<String, CapabilityRef>>>,
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
                && hop.state.routing_input.as_str() == proof.routing_input()
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

    #[cfg(feature = "llm-router")]
    pub(crate) const fn auxiliary_deadline(&self) -> std::time::Instant {
        self.engine.deadline
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

    pub(crate) const fn terminal_outcome_action(&self) -> Option<crate::causal_tree::branch::CausalActionKind> {
        use crate::causal_tree::branch::CausalActionKind;
        match &self.directive {
            TurnExecutionDirective::AnswerDirectly => Some(CausalActionKind::Answer),
            TurnExecutionDirective::Execute { .. } => Some(CausalActionKind::UseTool),
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
            TurnExecutionDirective::AnswerDirectly
            | TurnExecutionDirective::Execute { .. }
            | TurnExecutionDirective::Delegate { .. }
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
    /// response reserve. Very small windows fail closed instead of silently
    /// authorizing a zero-token decision budget.
    pub(crate) fn from_effective_compaction(
        config: &crate::config::AgentCompactionConfig,
    ) -> Result<Self, DecisionReserveUnavailable> {
        let available = config
            .max_context_tokens
            .checked_sub(config.reserve_tokens)
            .ok_or(DecisionReserveUnavailable)?;
        let decision_tokens = (available / 16).min(4096);
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
    ) -> Result<crate::config::AgentCompactionConfig, DecisionReserveUnavailable> {
        let expected = Self::from_effective_compaction(effective)?;
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
        let refs = decision.issue_selected_discovery(
            catalog,
            &DirectiveContext {
                current_generation: catalog.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
            proof.routing_input(),
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
        let selected_family = decision.validate_selected_retrieval(
            &catalog,
            &DirectiveContext {
                current_generation: catalog.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
            proof.routing_input(),
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
                            proof.routing_input(),
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
                                proof.routing_input(),
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
                            .retrieve_decision_documents(proof.routing_input(), 5, &principal, decision.deadline)
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
        let mut context = self
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
        self.record_issued_decision(proof, context, cancellation).await
    }

    async fn record_issued_decision(
        &self,
        proof: &dyn TurnSourceProof,
        context: Arc<TurnDecisionContext>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        if cancellation.is_cancelled() && !context.router_cleanup_close() {
            return Err(AgentDecisionError::Cancelled);
        }
        if self.durable_key && self.recordable && self.generation.effective.causal_tree.enabled {
            let records = DecisionRecordBuilder::build_enabled_chain(
                &context,
                &self.state_key,
                true,
                self.generation.effective.causal_tree.store.record_redacted_projection,
            )?;
            if cancellation.is_cancelled() && !context.router_cleanup_close() {
                return Err(AgentDecisionError::Cancelled);
            }
            let writer = self.store.writer().ok_or(AgentDecisionError::RecordWriterUnavailable)?;
            // Admission to the bounded queue is not a durability receipt.
            // The writer reports a dropped chain through its incomplete event.
            if writer.record_chain(records) == crate::causal_tree::store::Enqueue::Queued {
                context.record_admitted.store(true, Ordering::Release);
            }
        }
        if matches!(context.directive, TurnExecutionDirective::Delegate { .. }) {
            if self.generation.effective.causal_tree.enabled {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(AgentDecisionError::Cancelled),
                    verified = self.validate_delegation_storage(Arc::clone(&context)) => {
                        verified.map_err(AgentDecisionError::DelegationStorage)?;
                    }
                }
            } else {
                self.loop_runtime
                    .as_ref()
                    .context("ordinary delegation runtime missing")
                    .map_err(AgentDecisionError::DelegationScope)?;
                self.verified_delegation_scope(&context)
                    .map_err(AgentDecisionError::DelegationScope)?;
            }
            if cancellation.is_cancelled() && !context.router_cleanup_close() {
                return Err(AgentDecisionError::Cancelled);
            }
        }
        if matches!(context.directive, TurnExecutionDirective::RequestApproval { .. }) {
            self.record_pending_decision_approval(proof, &context)
                .await
                .map_err(AgentDecisionError::PendingApproval)?;
        }
        Ok(context)
    }

    async fn decide_agent_initial_inner(
        &self,
        proof: &dyn TurnSourceProof,
        kind: AgentTurnKind,
        cancellation: &CancellationToken,
        resume: Option<&pending_approval::AdmittedDecisionResume>,
        segment_observation: Option<&segment_observation::SegmentObservation>,
        #[cfg(feature = "llm-router")] mut routing: Option<&mut routing::RouterContinuation<'_>>,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        if cancellation.is_cancelled() {
            return Err(AgentDecisionError::Cancelled);
        }
        let model = self.resolve_agent_model(proof)?;
        super::ingress_recovery::service_ready(self)
            .await
            .map_err(AgentDecisionError::IngressRecovery)?;
        let budget = self.agent_budget_snapshot(proof)?;
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(budget.budgets.latency_ms))
            .ok_or(AgentDecisionError::Deadline)?;
        let accounting = accounting::TurnAccounting::new(self, proof, budget.budgets, deadline);
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
        let mut state = CausalState {
            trusted,
            observed: segment_observation::initial_observations(segment_observation, deadline, cancellation)?,
            predicted: None,
            derived: DerivedState::default(),
            routing_input: RawRoutingInput::new(proof.routing_input().to_string()),
            goal: RawRoutingInput::new(proof.routing_input().to_string()),
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
        let observed_latency = self.observed_capability_latency(proof.owner_id(), catalog.generation());
        let cost_model = super::turn_cost::RuntimeCostModel::new(&model, &config.cost, &observed_latency);
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
        let memory_rehearsal = matches!(self.memory.name(), "sqlite" | "lucid").then_some(ScopedMemoryRehearsal {
            memory: self.memory.as_ref(),
            scope: proof.memory_scope(),
            query: proof.routing_input(),
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
                resume: None,
                accounting: accounting.retained_scope(),
                owner_filesystem: owner_filesystem.clone(),
                source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
                service_id: self.service_id,
                record_admitted: AtomicBool::new(false),
                execution_trace: std::sync::OnceLock::new(),
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
                return Ok(Arc::new(TurnDecisionContext {
                    resume: None,
                    accounting: accounting.retained_scope(),
                    owner_filesystem: owner_filesystem.clone(),
                    source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
                    service_id: self.service_id,
                    record_admitted: AtomicBool::new(false),
                    execution_trace: std::sync::OnceLock::new(),
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
                    selected_retrieved: Vec::new(),
                    selected_custom_memory_context: None,
                    router_transition: None,
                }));
            }
            prior_hops.push(decision);
        }
        let mut selected_retrieved = Vec::new();
        let mut selected_custom_memory_context = None;
        let max_hops = config.causal_tree.policy.max_decision_hops;
        for hop in state.lineage.hop()..=max_hops {
            if cancellation.is_cancelled() {
                if has_router_transition {
                    return terminal_after_catalog_drift(&state, prior_hops);
                }
                return Err(AgentDecisionError::Cancelled);
            }
            if self.check_live_parity().is_err() {
                return terminal_after_catalog_drift(&state, prior_hops);
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
                    return terminal_after_catalog_drift(&state, prior_hops);
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
                        if has_router_transition { return terminal_after_catalog_drift(&state, prior_hops); }
                        return Err(AgentDecisionError::Cancelled);
                    },
                    decided = CausalDecisionEngine::decide_prepared_with_collection(prepared, proof.approval_review_adapter()) => decided?,
                }
            };
            if cancellation.is_cancelled() {
                if has_router_transition {
                    return terminal_after_catalog_drift(&state, prior_hops);
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
                return terminal_after_catalog_drift(&state, prior_hops);
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
                        if has_router_transition { return terminal_after_catalog_drift(&state, prior_hops); }
                        return Err(AgentDecisionError::Cancelled);
                    },
                        observed = self.observe_selected_agent_memory(proof, &decision) => observed,
                    };
                    let observed = match observed {
                        Ok(observed) => observed,
                        Err(AgentRetrievalError::Parity(_)) => {
                            return terminal_after_catalog_drift(&state, prior_hops);
                        }
                        Err(error) => {
                            if has_router_transition {
                                return terminal_after_catalog_drift(&state, prior_hops);
                            }
                            return Err(error.into());
                        }
                    };
                    if cancellation.is_cancelled() {
                        if has_router_transition {
                            return terminal_after_catalog_drift(&state, prior_hops);
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
                            return terminal_after_catalog_drift(&state, prior_hops);
                        }
                        Err(error) => {
                            if has_router_transition {
                                return terminal_after_catalog_drift(&state, prior_hops);
                            }
                            return Err(error.into());
                        }
                    };
                    let previous_refs = discovered_refs.clone();
                    let added_refs = discovered_refs.extend_selected(selected_refs);
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
                TurnExecutionDirective::CollectApprovalArguments(_)
                | TurnExecutionDirective::AnswerDirectly
                | TurnExecutionDirective::Clarify { .. }
                | TurnExecutionDirective::RequestApproval { .. }
                | TurnExecutionDirective::Refuse { .. } => {
                    if cancellation.is_cancelled() {
                        if has_router_transition {
                            return terminal_after_catalog_drift(&state, prior_hops);
                        }
                        return Err(AgentDecisionError::Cancelled);
                    }
                    return Ok(Arc::new(TurnDecisionContext {
                        resume: None,
                        accounting: accounting.retained_scope(),
                        owner_filesystem: owner_filesystem.clone(),
                        source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
                        service_id: self.service_id,
                        record_admitted: AtomicBool::new(false),
                        execution_trace: std::sync::OnceLock::new(),
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
                    }));
                }
                TurnExecutionDirective::Execute { .. } | TurnExecutionDirective::Delegate { .. } => {
                    let execution_refs = selected_execute_refs(&decision, &directive, &catalog)?;
                    if self.check_live_parity().is_err() {
                        return terminal_after_catalog_drift(&state, prior_hops);
                    }
                    if cancellation.is_cancelled() {
                        if has_router_transition {
                            return terminal_after_catalog_drift(&state, prior_hops);
                        }
                        return Err(AgentDecisionError::Cancelled);
                    }
                    return Ok(Arc::new(TurnDecisionContext {
                        resume: None,
                        accounting: accounting.retained_scope(),
                        owner_filesystem: owner_filesystem.clone(),
                        source_actor: (Arc::from(proof.policy_channel()), Arc::from(proof.policy_sender())),
                        service_id: self.service_id,
                        record_admitted: AtomicBool::new(false),
                        execution_trace: std::sync::OnceLock::new(),
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
                    }));
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

    fn observed_capability_latency(&self, owner_id: &str, generation: CatalogGeneration) -> BTreeMap<String, u32> {
        let Some(workspace_id) = self.store.workspace_id() else {
            return BTreeMap::new();
        };
        match self
            .store
            .reader()
            .capability_latency_p50(&workspace_id, owner_id, &generation.to_hex())
        {
            Ok(samples) => samples,
            Err(error) => {
                tracing::warn!(error = %error, "causal outcome latency snapshot unavailable; using pinned defaults");
                BTreeMap::new()
            }
        }
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
        let effective = crate::router::resolve_effective_compaction_config(
            &config.agent.compaction,
            proof.provider_name(),
            proof.model_name(),
            &config.router,
            &config.model_routes,
        );
        let reservation = DecisionTokenReservation::from_effective_compaction(&effective.config)?;
        let main_compaction = reservation.reserved_compaction(&effective.config)?;
        let remaining = crate::cost::tracker::remaining_micro_usd_read_only_with_security_ceiling(
            &config.cost,
            &config.workspace_dir,
            self.security.max_cost_per_day_cents,
        )
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
pub(crate) fn build_turn_decision_service(
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
    let workspace_key = FactoryDeps::load_key(config);
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
    let store = if durable_key {
        CausalDecisionStore::open_shared(
            &config.workspace_dir,
            Retention::days(
                config.causal_tree.store.retention_days,
                config.self_system.fitness.retention_days,
            ),
            Arc::clone(&events),
            StoreMode::ReadWrite,
        )?
    } else {
        CausalDecisionStore::open_unshared_read_only(&config.workspace_dir)
    };
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
pub(crate) use tests::with_worker_probe_manifest;

#[cfg(test)]
mod tests {
    mod child_process;
    pub(crate) use child_process::worker_probe::with_manifest as with_worker_probe_manifest;
    mod discovery_surface;
    mod exact_call_approval;
    mod ledger_probe;
    mod performance;
    mod terminal_scope;

    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use chrono::TimeZone;

    use super::*;
    use crate::decision::backend::DecisionBackend;
    use crate::decision::builtin::BuiltinDecisionBackend;
    use crate::decision::factory::BackendSpec;
    use crate::decision::factory::SlotLimits;
    use crate::decision::outbound::{GuardedBody, OutboundSecrets};
    use crate::decision::protocol::{
        BackendIdentity, BackendKind, BackendLocality, DecisionAnswer, DecisionRequest, DecisionResponse,
        GlobalQuestion, NoulAnswer, Probability, QuestionId,
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

        async fn finalize_closed_turn(
            &self,
            _: Arc<TurnDecisionContext>,
            reply: &str,
        ) -> anyhow::Result<Self::Receipt> {
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

        async fn finalize_closed_turn(
            &self,
            _: Arc<TurnDecisionContext>,
            reply: &str,
        ) -> anyhow::Result<Self::Receipt> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut commit = self.commit.clone();
            commit.history = Some(crate::agent::terminal::TurnHistoryProjection {
                assistant_content: reply.to_owned(),
                history_commit_len: 0,
            });
            let receipt =
                crate::agent::terminal::finalize_turn(&self.fabric, commit, &self.cost, &self.workspace).await?;
            if let Some(cancellation) = &self.cancellation {
                cancellation.cancel();
            }
            Ok(receipt)
        }
    }

    #[test]
    fn selected_session_projection_keeps_kind_and_distrusts_event_identity() {
        let now = chrono::Utc::now();
        let hit = crate::memory::traits::DecisionSessionHit {
            event_id: "untrusted-row-event".to_owned(),
            role: "assistant".to_owned(),
            content: "prior local response".to_owned(),
            created_at: now.to_rfc3339(),
        };
        let artifacts = session_artifacts_from_selected_read(std::slice::from_ref(&hit), now);
        assert_eq!(artifacts.len(), 1);
        let artifact = artifacts.first().expect("selected session artifact");
        assert_eq!(artifact.kind, ObservedArtifactKind::Session);
        assert_eq!(artifact.summary, "assistant: prior local response");
        assert!(artifact.source_event_id.is_none());
        let mut invalid = hit.clone();
        invalid.role = "system".to_owned();
        assert!(
            session_artifacts_from_selected_read(&[invalid], now).is_empty(),
            "expected an empty value"
        );
        let mut oversized = hit;
        oversized.content = "x".repeat(4097);
        assert!(
            session_artifacts_from_selected_read(&[oversized], now).is_empty(),
            "expected an empty value"
        );
    }

    #[test]
    fn selected_memory_artifacts_redact_before_projection_cut_and_distrust_row_metadata() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 9, 29, 12, 0, 0)
            .single()
            .expect("fixed time");
        let hit = |content: &str, created_at: String, source_event_id: Option<&str>| DecisionMemoryHit {
            key: "prx_memory_key".into(),
            category: MemoryCategory::Core,
            session_id: None,
            id: "prx_memory_hit".into(),
            content: content.into(),
            created_at,
            source_event_id: source_event_id.map(str::to_string),
        };
        let hits = vec![
            hit("  ", now.to_rfc3339(), None),
            hit(
                "local evidence",
                (now - chrono::Duration::seconds(2)).to_rfc3339(),
                Some("spoofed-event"),
            ),
            hit(
                "future evidence",
                (now + chrono::Duration::seconds(1)).to_rfc3339(),
                None,
            ),
            hit("invalid time", "invalid timestamp".into(), None),
            hit(&"é".repeat(200), now.to_rfc3339(), None),
            hit("sixth result", now.to_rfc3339(), None),
        ];
        let artifacts = memory_artifacts_from_selected_read(&hits, now);
        let [first, future, malformed, unicode] = artifacts.as_slice() else {
            panic!("expected four bounded artifacts");
        };
        assert_eq!(first.kind, ObservedArtifactKind::Memory);
        assert_eq!(first.age_ms, 2_000);
        assert_eq!(first.summary, "local evidence");
        assert_eq!(
            first.source_event_id, None,
            "row metadata is not an authenticated receipt"
        );
        assert!(future.age_ms > STALE_ARTIFACT_MS, "future timestamps are stale");
        assert!(malformed.age_ms > STALE_ARTIFACT_MS, "malformed timestamps are stale");
        assert_eq!(unicode.summary.chars().count(), 200);
        assert!(!unicode.summary_was_truncated);
        assert!(!artifacts.iter().any(|artifact| artifact.summary == "sixth result"));
        assert!(
            memory_artifacts_from_selected_read(&[hit(&"x".repeat(4_097), now.to_rfc3339(), None)], now).is_empty(),
            "expected an empty value"
        );
        assert!(
            !format!("{artifacts:?}").contains("local evidence"),
            "Debug redacts raw content"
        );

        let secret = "enc2:123456789abcdef";
        let straddling = format!("{}{}", "a ".repeat(124), secret);
        let evidence = memory_artifacts_from_selected_read(&[hit(&straddling, now.to_rfc3339(), None)], now);
        let artifact = evidence.first().expect("one straddling artifact");
        assert_eq!(artifact.summary.chars().count(), 256);
        assert!(artifact.summary_was_redacted);
        assert!(artifact.summary_was_truncated);
        let key = crate::causal_tree::state::fixtures::key();
        let mut turn = crate::causal_tree::state::fixtures::state(&key, "retrieve evidence");
        turn.observed = ObservedState::from_runtime_records(Vec::new(), evidence, 0);
        let (projection, manifest) =
            crate::causal_tree::projection::redacted_projection(&turn).expect("redacted projection");
        let rendered = projection.as_json().to_string();
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("enc2:123"), "a truncated token prefix must not leak");
        assert_eq!(manifest.redacted_texts, 1, "one source artifact was redacted");
        assert_eq!(manifest.truncated, vec!["artifacts.summary"]);
    }

    struct CountingProbeMemory {
        calls: AtomicUsize,
        retrieval_calls: AtomicUsize,
        return_empty: AtomicBool,
        return_error: AtomicBool,
        return_never: AtomicBool,
        return_stale: AtomicBool,
        unique_results: AtomicBool,
        drift_on_read: Option<Arc<AtomicBool>>,
        read_entered: Arc<tokio::sync::Notify>,
        retrieval_args: parking_lot::Mutex<Option<(String, u8, MemoryWriteContext, Instant)>>,
    }

    #[async_trait]
    impl Memory for CountingProbeMemory {
        fn name(&self) -> &str {
            "prx_probe_fixture"
        }

        async fn store(&self, _: &str, _: &str, _: MemoryCategory, _: Option<&str>) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(&self, _: &str, _: usize, _: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(&self, _: Option<&MemoryCategory>, _: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }

        async fn count_rehearsal_hits(
            &self,
            _: &str,
            _: u8,
            _: &MemoryWriteContext,
            _: Instant,
        ) -> anyhow::Result<u32> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        }

        async fn retrieve_decision_memory(
            &self,
            query: &str,
            limit: u8,
            context: &MemoryWriteContext,
            session_id: Option<&str>,
            deadline: Instant,
        ) -> anyhow::Result<Vec<DecisionMemoryHit>> {
            let call = self.retrieval_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(drift) = &self.drift_on_read {
                drift.store(true, Ordering::SeqCst);
            }
            *self.retrieval_args.lock() = Some((query.to_string(), limit, context.clone(), deadline));
            self.read_entered.notify_one();
            if self.return_never.load(Ordering::SeqCst) {
                return std::future::pending().await;
            }
            if self.return_error.load(Ordering::SeqCst) {
                anyhow::bail!("prx fixture read failure");
            }
            if self.return_empty.load(Ordering::SeqCst) {
                return Ok(Vec::new());
            }
            Ok(vec![DecisionMemoryHit {
                key: "prx_memory_key".into(),
                category: MemoryCategory::Core,
                session_id: session_id.map(str::to_owned),
                id: "prx_memory_hit".into(),
                content: if self.unique_results.load(Ordering::SeqCst) {
                    format!("prx scoped memory evidence {call}")
                } else {
                    "prx scoped memory evidence".into()
                },
                created_at: if self.return_stale.load(Ordering::SeqCst) {
                    (chrono::Utc::now() - chrono::Duration::days(365)).to_rfc3339()
                } else {
                    chrono::Utc::now().to_rfc3339()
                },
                source_event_id: None,
            }])
        }
    }

    struct RetrieveProbeBackend {
        identity: BackendIdentity,
        requests: Arc<parking_lot::Mutex<Vec<(String, Option<String>)>>>,
        stall: bool,
        entered: Arc<tokio::sync::Notify>,
        drift_after_response: Option<Arc<AtomicBool>>,
    }

    #[async_trait]
    impl DecisionBackend for RetrieveProbeBackend {
        fn identity(&self) -> &BackendIdentity {
            &self.identity
        }

        fn locality(&self) -> BackendLocality {
            BackendLocality::Builtin
        }

        fn version_policy(&self) -> ModelVersionPolicy {
            ModelVersionPolicy::Unpinned
        }

        // Explicit accounting-entry support for this deterministic in-process
        // fixture. No physical send, receipt or fabricated charge is produced.
        async fn evaluate_with_accounting(
            &self,
            request: &DecisionRequest,
            body: &GuardedBody,
            deadline: Instant,
            _accounting: Option<&dyn crate::decision::budget::DecisionAttemptAccounting>,
        ) -> Result<DecisionResponse, DecisionError> {
            self.evaluate(request, body, deadline).await
        }

        async fn evaluate(
            &self,
            request: &DecisionRequest,
            _: &GuardedBody,
            _: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.requests.lock().push((
                request.manifest().state_fingerprint().to_string(),
                request
                    .state()
                    .as_json()
                    .get("prior_decision_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            ));
            if self.stall {
                self.entered.notify_one();
                return std::future::pending().await;
            }
            if let Some(drift) = &self.drift_after_response {
                drift.store(true, Ordering::SeqCst);
            }
            let mut response = BuiltinDecisionBackend::answer(request)?;
            response.backend = "mock".into();
            response.resolved_model = self.identity.configured_model.to_string();
            response.answers.insert(
                QuestionId::global(GlobalQuestion::EvidenceSufficient),
                DecisionAnswer::Noul(NoulAnswer {
                    probability_true: Probability::new(0.01)?,
                }),
            );
            if let std::collections::btree_map::Entry::Occupied(mut answer) = response
                .answers
                .entry(QuestionId::global(GlobalQuestion::NeedsFreshState))
            {
                answer.insert(DecisionAnswer::Noul(NoulAnswer {
                    probability_true: Probability::new(0.99)?,
                }));
            }
            Ok(response)
        }
    }

    struct ExecuteContextProbeBackend {
        identity: BackendIdentity,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DecisionBackend for ExecuteContextProbeBackend {
        fn identity(&self) -> &BackendIdentity {
            &self.identity
        }
        fn locality(&self) -> BackendLocality {
            BackendLocality::Builtin
        }
        fn version_policy(&self) -> ModelVersionPolicy {
            ModelVersionPolicy::Unpinned
        }
        // This in-process probe performs no provider I/O or synthetic charge.
        async fn evaluate_with_accounting(
            &self,
            request: &DecisionRequest,
            body: &GuardedBody,
            deadline: Instant,
            _accounting: Option<&dyn crate::decision::budget::DecisionAttemptAccounting>,
        ) -> Result<DecisionResponse, DecisionError> {
            self.evaluate(request, body, deadline).await
        }
        async fn evaluate(
            &self,
            request: &DecisionRequest,
            _: &GuardedBody,
            _: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut response = BuiltinDecisionBackend::answer(request)?;
            response.backend = "mock".to_owned();
            response.resolved_model = self.identity.configured_model.to_string();
            for (id, answer) in &mut response.answers {
                let work = id.as_str().starts_with("candidate.use_tool-native_tool.")
                    || id.as_str().starts_with("candidate.use_tool-native_tool-");
                match answer {
                    DecisionAnswer::Choice(choice) => {
                        let code = if id == &QuestionId::global(GlobalQuestion::NextAction) {
                            "use_tool"
                        } else {
                            "native_tool"
                        };
                        if let Some(chosen) = choice.probabilities.keys().find(|key| key.as_str() == code).cloned() {
                            for (key, probability) in &mut choice.probabilities {
                                *probability = Probability::new(if key == &chosen { 1.0 } else { 0.0 })?;
                            }
                            choice.chosen = chosen;
                            choice.confidence = Probability::new(1.0)?;
                        }
                    }
                    DecisionAnswer::Noul(noul) => {
                        noul.probability_true = Probability::new(
                            if id == &QuestionId::global(GlobalQuestion::EvidenceSufficient) || work {
                                1.0
                            } else {
                                0.0
                            },
                        )?;
                    }
                    DecisionAnswer::Score(score) => {
                        let gain = id.as_str().ends_with("gain") && (work || !id.as_str().starts_with("candidate."));
                        let chosen = if gain {
                            score.distribution.len().saturating_sub(1)
                        } else {
                            0
                        };
                        for (index, probability) in score.distribution.iter_mut().enumerate() {
                            *probability = Probability::new(if index == chosen { 1.0 } else { 0.0 })?;
                        }
                        score.expected = f64::from(u32::try_from(chosen).expect("bounded score level"));
                        score.confidence = Probability::new(1.0)?;
                    }
                }
            }
            Ok(response)
        }
    }

    #[tokio::test]
    async fn selected_execute_retains_exact_gated_refs_without_dispatch() {
        struct ReadFixtureTool {
            name: &'static str,
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Tool for ReadFixtureTool {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &str {
                "Read neutral project evidence"
            }
            fn categories(&self) -> &'static [crate::tools::traits::ToolCategory] {
                if self.name == "web_search_tool" {
                    &[crate::tools::traits::ToolCategory::WebBrowsing]
                } else {
                    &[]
                }
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object","properties":{}})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("context construction must not dispatch")
            }
        }
        for (enabled, keyless) in [(false, false), (true, false), (true, true)] {
            let workspace = tempfile::tempdir().expect("test workspace");
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_path_buf();
            config.causal_tree.enabled = enabled;
            if keyless {
                let directory = workspace.path().join("self/causal_tree");
                std::fs::create_dir_all(&directory).expect("fixture key directory");
                std::fs::write(directory.join(".decision_key"), b"short").expect("unavailable fixture key");
            }
            config.causal_tree.policy.extra_latency_budget_ms = 5_000;
            config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            )
            .expect("verified main model"),
        );
            let generation = crate::config::new_shared(config.clone()).pin();
            let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
            let dispatches = Arc::new(AtomicUsize::new(0));
            let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
                Box::new(ReadFixtureTool {
                    name: "file_read",
                    calls: Arc::clone(&dispatches),
                }),
                Box::new(ReadFixtureTool {
                    name: "web_search_tool",
                    calls: Arc::clone(&dispatches),
                }),
            ]);
            let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
                Arc::clone(&registry),
                Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
                Arc::new(DenyApprovalStrategy),
                Arc::new(AdapterOwnedPreparation),
                Arc::new(TracingToolExecutionAudit),
            ));
            let catalog = ProviderCatalogView::from_config(&config);
            let transports = CountingTransports(AtomicUsize::new(0));
            let memory: Arc<dyn Memory> = if enabled {
                Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"))
            } else {
                Arc::new(NoneMemory::new())
            };
            let mut service = build_turn_decision_service(
                TurnSurfaceKind::AgentLoop,
                TurnServiceBuild {
                    parent_grant: None,
                    generation: Arc::clone(&generation),
                    registry,
                    execution,
                    catalog_sources: sources(&generation, &config),
                    surface_policy: SurfacePolicy {
                        allowed_tools: None,
                        direct_names: BTreeSet::from(["file_read".to_owned(), "web_search_tool".to_owned()]),
                        ..SurfacePolicy::default()
                    },
                    provider_catalog: &catalog,
                    transports: &transports,
                    events: Arc::new(SilentEvents),
                    memory: Arc::clone(&memory),
                    security,
                },
            )
            .expect("source-owned fixture service");
            let predictions = Arc::new(AtomicUsize::new(0));
            let owning = Arc::get_mut(&mut service).expect("unshared fixture service");
            // Fixture-only deterministic probabilities. This does not establish
            // configured transport or budget admission through the constructor.
            owning.factory = DecisionBackendFactory::from_slots(
                vec![Box::new(ExecuteContextProbeBackend {
                    identity: BackendIdentity {
                        kind: BackendKind::Mock,
                        provider: None,
                        configured_model: Arc::from("prx-local-model"),
                    },
                    calls: Arc::clone(&predictions),
                })],
                Arc::clone(&owning.state_key),
                OutboundSecrets::default(),
                SlotLimits::default(),
            );
            let proof = if enabled {
                crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
                    &generation,
                    Arc::clone(&memory),
                    "Use file_read for the project evidence",
                    "prx_execute_context_run",
                )
                .await
            } else {
                crate::agent::entry::test_cli_agent_turn_proof_with_input(
                    &generation,
                    "ollama",
                    "prx-local-model",
                    "Use file_read for the project evidence",
                )
            };
            let cancelled = CancellationToken::new();
            cancelled.cancel();
            assert!(matches!(
                service
                    .decide_agent_initial(&proof, AgentTurnKind::Repl, &cancelled)
                    .await,
                Err(AgentDecisionError::Cancelled)
            ));
            assert_eq!(predictions.load(Ordering::SeqCst), 0);
            let context = service
                .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
                .await
                .expect("gated service result");
            assert!(
                context.audit_hops().any(|hop| matches!(&hop.selection,
                    crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. }
                        if candidate_id.action() == crate::causal_tree::branch::CausalActionKind::UseTool
                )),
                "fixture must actually select gated work before checking preservation"
            );
            assert!(
                matches!(context.directive(), TurnExecutionDirective::Execute { .. }),
                "selected Execute must be retained"
            );
            assert_eq!(context.hop_ids().len(), 1, "no additional terminal hop");
            assert_eq!(
                context.terminal_outcome_action(),
                None,
                "selected work has not completed"
            );
            let view = service.catalog.view(&CatalogScope::default());
            let entry = view
                .iter_entries()
                .find(|entry| entry.public_name == "file_read")
                .expect("selected exact capability");
            assert_eq!(
                context.execution_refs(),
                &BTreeSet::from([CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation
                }])
            );
            assert!(
                matches!(&context.engine().candidates,
                    crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. }
                        if candidates.iter().any(|candidate| candidate.target() == CapabilityTarget::Web)
                ),
                "another offered legal family must not enter selected execution refs"
            );
            assert_eq!(context.record_admitted.load(Ordering::Acquire), enabled && !keyless);
            if enabled && !keyless {
                let writer = service.store.writer().expect("recorded work writer");
                assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
                let scope = DecisionScope::Owner {
                    workspace_id: service.store.workspace_id().expect("store workspace"),
                    owner_id: Arc::from(proof.owner_id()),
                };
                let row = service
                    .store
                    .reader()
                    .get(&scope, &context.decision_id())
                    .expect("work record")
                    .expect("one selected work record");
                assert!(row.enabled_snapshot);
                assert_eq!(row.directive_kind, "use_tool");
                assert!(
                    row.outcomes.is_empty(),
                    "context construction must not settle selected work"
                );
                assert_eq!(
                    row.selected_candidate_id.as_deref(),
                    match &context.engine().selection {
                        crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. } =>
                            Some(candidate_id.as_str()),
                        _ => None,
                    }
                );
            }
            assert!(matches!(context.disposition(), Ok(TurnDisposition::Run)));
            let ports = ReceiptClosePorts {
                receipt: None,
                calls: AtomicUsize::new(0),
                cancellation: None,
            };
            assert!(
                service
                    .close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &CancellationToken::new())
                    .await
                    .is_err()
            );
            assert_eq!(ports.calls.load(Ordering::SeqCst), 0);
            assert_eq!(predictions.load(Ordering::SeqCst), 1);
            assert_eq!(transports.0.load(Ordering::SeqCst), 0);
            assert_eq!(dispatches.load(Ordering::SeqCst), 0);
            let mut expired = context;
            assert_execute_reference_contract_rejects_changes(
                Arc::get_mut(&mut expired).expect("unshared contract fixture"),
                &service.catalog,
            );
            Arc::get_mut(&mut expired)
                .expect("unshared returned context")
                .engine
                .deadline = Instant::now();
            assert!(matches!(expired.disposition(), Err(DirectiveError::NotAdmitted)));
        }
    }

    // Conversion mutations below are private contract fixtures. They do not
    // establish new service admission or authorize a multi-capability action.
    fn assert_execute_reference_contract_rejects_changes(
        context: &mut TurnDecisionContext,
        catalog: &CapabilityCatalog,
    ) {
        use crate::causal_tree::branch::{CandidateSpec, CausalCandidate};
        use crate::causal_tree::candidate_generator::CandidateSet;
        use crate::causal_tree::gates::GateOutcome;
        let view = catalog.view(&CatalogScope::default());
        let original_refs: Vec<_> = context.execution_refs().iter().cloned().collect();
        let extra = view
            .iter_entries()
            .find(|entry| entry.public_name == "web_search_tool")
            .expect("offered extra capability");
        let extra_ref = CapabilityRef {
            id: extra.id.clone(),
            fingerprint: extra.fingerprint,
            generation: extra.generation,
        };
        let original = match &context.engine.candidates {
            CandidateSet::Legal { candidates, .. } => candidates
                .iter()
                .find(|candidate| match &context.engine.selection {
                    crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. } => {
                        candidate.candidate_id() == candidate_id
                    }
                    _ => false,
                })
                .expect("selected fixture candidate")
                .clone(),
            _ => panic!("legal fixture candidates"),
        };
        let id = original.candidate_id().clone();
        let expected_ids: Vec<_> = original_refs.iter().map(|reference| reference.id.clone()).collect();
        for ids in [
            Vec::new(),
            [expected_ids.clone(), expected_ids.clone()].concat(),
            [expected_ids, vec![extra.id.clone()]].concat(),
        ] {
            assert!(
                selected_execute_refs(
                    &context.engine,
                    &TurnExecutionDirective::Execute {
                        allowed_capability_ids: ids
                    },
                    &view
                )
                .is_err()
            );
        }
        assert!(selected_execute_refs(&context.engine, &TurnExecutionDirective::AnswerDirectly, &view).is_err());
        let blocked = GateOutcome::Blocked {
            gate: 1,
            reason_code: "fixture_blocked",
        };
        let previous = context
            .engine
            .pre_commit
            .outcomes
            .insert(id.clone(), blocked.clone())
            .expect("original final gate");
        assert!(selected_execute_refs(&context.engine, &context.directive, &view).is_err());
        context.engine.pre_commit.outcomes.insert(id.clone(), previous);
        let CandidateSet::Legal { report, .. } = &mut context.engine.candidates else {
            panic!("legal fixture");
        };
        let previous = report
            .outcomes
            .insert(id.clone(), blocked)
            .expect("original admitted gate");
        assert!(selected_execute_refs(&context.engine, &context.directive, &view).is_err());
        let CandidateSet::Legal { report, .. } = &mut context.engine.candidates else {
            panic!("legal fixture");
        };
        report.outcomes.insert(id.clone(), previous);
        let mut stale_fingerprint = original_refs.first().expect("one ref").clone();
        *stale_fingerprint.fingerprint.0.first_mut().expect("fingerprint byte") ^= 1;
        let mut stale_generation = original_refs.first().expect("one ref").clone();
        *stale_generation.generation.0.first_mut().expect("generation byte") ^= 1;
        for refs in [
            Vec::new(),
            [original_refs.clone(), original_refs.clone()].concat(),
            vec![stale_fingerprint],
            vec![stale_generation],
        ] {
            let CandidateSet::Legal { candidates, .. } = &mut context.engine.candidates else {
                panic!("legal fixture");
            };
            let selected = candidates
                .iter_mut()
                .find(|candidate| candidate.candidate_id() == &id)
                .expect("selected fixture");
            *selected = CausalCandidate::new(
                original.action(),
                original.target(),
                CandidateSpec {
                    namespace: id.namespace(),
                    required_capabilities: refs,
                    side_effect: original.side_effect(),
                    approval: original.approval().clone(),
                    estimated_cost: original.estimated_cost(),
                    estimated_latency_ms: original.estimated_latency_ms(),
                },
            );
            assert!(selected_execute_refs(&context.engine, &context.directive, &view).is_err());
        }
        let CandidateSet::Legal { candidates, .. } = &mut context.engine.candidates else {
            panic!("legal fixture");
        };
        *candidates
            .iter_mut()
            .find(|candidate| candidate.candidate_id() == &id)
            .expect("selected fixture") = original.clone();
        let excluded = catalog.view(&CatalogScope {
            families: BTreeSet::from([CapabilityTarget::Web]),
            channel: false,
        });
        assert!(selected_execute_refs(&context.engine, &context.directive, &excluded).is_err());
        // The converter must preserve all selected refs, including a multi-ref
        // synthetic candidate. This is conversion coverage only.
        let refs = [original_refs.clone(), vec![extra_ref]].concat();
        let CandidateSet::Legal { candidates, .. } = &mut context.engine.candidates else {
            panic!("legal fixture");
        };
        *candidates
            .iter_mut()
            .find(|candidate| candidate.candidate_id() == &id)
            .expect("selected fixture") = CausalCandidate::new(
            original.action(),
            original.target(),
            CandidateSpec {
                namespace: id.namespace(),
                required_capabilities: refs.clone(),
                side_effect: original.side_effect(),
                approval: original.approval().clone(),
                estimated_cost: original.estimated_cost(),
                estimated_latency_ms: original.estimated_latency_ms(),
            },
        );
        let directive = directive_from_selection(
            &context.engine,
            &DirectiveContext {
                current_generation: view.generation(),
                approval_expires_at: chrono::Utc::now(),
                discovery_limit: 1,
            },
        )
        .expect("synthetic multi-ref conversion");
        assert_eq!(
            selected_execute_refs(&context.engine, &directive, &view).expect("exact multi-ref metadata"),
            refs.into_iter().collect()
        );
        let CandidateSet::Legal { candidates, .. } = &mut context.engine.candidates else {
            panic!("legal fixture");
        };
        *candidates
            .iter_mut()
            .find(|candidate| candidate.candidate_id() == &id)
            .expect("selected fixture") = original;
        assert_eq!(
            selected_execute_refs(&context.engine, &context.directive, &view).expect("restored real selection"),
            original_refs.into_iter().collect()
        );
    }

    struct UsageRecordProbeBackend {
        identity: BackendIdentity,
        request_ids: Arc<parking_lot::Mutex<Vec<String>>>,
        stall: bool,
        entered: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl DecisionBackend for UsageRecordProbeBackend {
        fn identity(&self) -> &BackendIdentity {
            &self.identity
        }

        fn locality(&self) -> BackendLocality {
            BackendLocality::Builtin
        }

        fn version_policy(&self) -> ModelVersionPolicy {
            ModelVersionPolicy::Unpinned
        }

        // Deterministic in-process protocol fixture, like RetrieveProbeBackend.
        // Accept the accounted entry explicitly without pretending a physical
        // send, reservation or vendor charge occurred. Production transports
        // retain the default refusal unless they implement real accounting.
        async fn evaluate_with_accounting(
            &self,
            request: &DecisionRequest,
            body: &GuardedBody,
            deadline: Instant,
            _accounting: Option<&dyn crate::decision::budget::DecisionAttemptAccounting>,
        ) -> Result<DecisionResponse, DecisionError> {
            self.evaluate(request, body, deadline).await
        }

        async fn evaluate(
            &self,
            request: &DecisionRequest,
            _: &GuardedBody,
            _: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            self.request_ids.lock().push(request.manifest().request_id().to_owned());
            if self.stall {
                self.entered.notify_one();
                return std::future::pending().await;
            }
            let mut response = BuiltinDecisionBackend::answer(request)?;
            response.backend = "mock".to_owned();
            response.resolved_model = self.identity.configured_model.to_string();
            response.usage = crate::decision::protocol::DecisionUsage {
                input_tokens: Some(17),
                output_tokens: Some(3),
                cost_micro_usd: Some(42),
            };
            response.provider_request_id = Some("prx_provider_request_1".to_owned());
            Ok(response)
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ProbeCancelMode {
        Never,
        Before,
        Backend,
        Read,
        CatalogDriftRead,
    }

    async fn run_retrieve_probe_case(
        unique_results: bool,
        mode: ProbeCancelMode,
    ) -> (
        usize,
        usize,
        Vec<(String, Option<String>)>,
        Vec<CausalEvent>,
        Result<Arc<TurnDecisionContext>, AgentDecisionError>,
    ) {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        // This fixture observes an in-process probe backend, not a durable
        // event-store adapter. Its source proof and phase assertions are off-mode.
        config.causal_tree.enabled = false;
        config.causal_tree.policy.max_decision_hops = 3;
        config.causal_tree.policy.extra_latency_budget_ms = 5_000;
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            )
            .expect("verified test model declaration"),
        );
        let generation = crate::config::new_shared(config.clone()).pin();
        let drift = Arc::new(AtomicBool::new(false));
        let memory = Arc::new(CountingProbeMemory {
            calls: AtomicUsize::new(0),
            retrieval_calls: AtomicUsize::new(0),
            return_empty: AtomicBool::new(false),
            return_error: AtomicBool::new(false),
            return_never: AtomicBool::new(mode == ProbeCancelMode::Read),
            return_stale: AtomicBool::new(false),
            unique_results: AtomicBool::new(unique_results),
            drift_on_read: (mode == ProbeCancelMode::CatalogDriftRead).then(|| Arc::clone(&drift)),
            read_entered: Arc::new(tokio::sync::Notify::new()),
            retrieval_args: parking_lot::Mutex::new(None),
        });
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::memory_search::MemorySearchTool::new(
            workspace.path().to_path_buf(),
            Arc::clone(&memory) as Arc<dyn Memory>,
            false,
        ))];
        if mode == ProbeCancelMode::CatalogDriftRead {
            tools.push(Box::new(DriftingCatalogTool {
                changed: Arc::clone(&drift),
                dispatches: Arc::new(AtomicUsize::new(0)),
            }));
        }
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(tools);
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let events = Arc::new(RecordingSink::default());
        let mut service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::clone(&events) as Arc<dyn CausalEventSink>,
                memory: Arc::clone(&memory) as Arc<dyn Memory>,
                security,
            },
        )
        .expect("test service");
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let backend_entered = Arc::new(tokio::sync::Notify::new());
        let service_mut = Arc::get_mut(&mut service).expect("unshared test service");
        service_mut.factory = DecisionBackendFactory::from_slots(
            vec![Box::new(RetrieveProbeBackend {
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("prx-local-model"),
                },
                requests: Arc::clone(&requests),
                stall: mode == ProbeCancelMode::Backend,
                entered: Arc::clone(&backend_entered),
                drift_after_response: None,
            })],
            Arc::clone(&service_mut.state_key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        let proof = crate::agent::entry::test_cli_agent_turn_proof_with_run(
            &generation,
            "ollama",
            "prx-local-model",
            "find the scoped memory evidence",
            "prx_run_retrieve",
        );
        let cancellation = CancellationToken::new();
        if mode == ProbeCancelMode::Before {
            cancellation.cancel();
        }
        let decision = service.decide_agent_initial(&proof, AgentTurnKind::SingleShot, &cancellation);
        let context = match mode {
            ProbeCancelMode::Never | ProbeCancelMode::Before | ProbeCancelMode::CatalogDriftRead => decision.await,
            ProbeCancelMode::Backend | ProbeCancelMode::Read => {
                let entered = if mode == ProbeCancelMode::Backend {
                    backend_entered
                } else {
                    Arc::clone(&memory.read_entered)
                };
                let cancel_when_entered = async {
                    entered.notified().await;
                    cancellation.cancel();
                };
                let watched = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::join!(cancel_when_entered, decision)
                })
                .await;
                match watched {
                    Ok(((), context)) => context,
                    Err(_) => {
                        cancellation.cancel();
                        panic!("probe entry/cancellation watchdog expired before both branches completed");
                    }
                }
            }
        };
        let calls = memory.retrieval_calls.load(Ordering::SeqCst);
        let transport_calls = transports.0.load(Ordering::SeqCst);
        let requests = requests.lock().clone();
        let emitted = events.events.lock().clone();
        (calls, transport_calls, requests, emitted, context)
    }

    async fn run_retrieve_probe(
        unique_results: bool,
    ) -> (usize, usize, Vec<(String, Option<String>)>, Arc<TurnDecisionContext>) {
        let (reads, transports, requests, _, context) =
            run_retrieve_probe_case(unique_results, ProbeCancelMode::Never).await;
        (reads, transports, requests, context.expect("bounded selected Retrieve"))
    }

    struct DriftingCatalogTool {
        changed: Arc<AtomicBool>,
        dispatches: Arc<AtomicUsize>,
    }

    struct PerfCatalogTool(ToolSpec);

    impl PerfCatalogTool {
        fn new(name: String) -> Self {
            Self(ToolSpec {
                name,
                description: "unrelated PRX performance fixture".to_string(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            })
        }
    }

    #[async_trait]
    impl Tool for PerfCatalogTool {
        fn name(&self) -> &str {
            &self.0.name
        }

        fn description(&self) -> &str {
            &self.0.description
        }

        fn parameters_schema(&self) -> serde_json::Value {
            self.0.parameters.clone()
        }

        fn immutable_catalog_specs(&self) -> Option<&[ToolSpec]> {
            Some(std::slice::from_ref(&self.0))
        }

        fn catalog_availability_is_configured(&self) -> bool {
            true
        }

        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
            anyhow::bail!("performance fixture must not execute a tool")
        }
    }

    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::print_stderr)] // Diagnostic numbers belong in the captured test log.
    async fn service_builtin_allocation_diagnostic_holds_candidate_count_fixed() {
        let mut measurements = Vec::new();
        // Run both sizes once before comparing them so process-global schema
        // and redaction initialization is outside the measured pair.
        for catalog_size in [64_usize, 0, 64, 0] {
            let workspace = tempfile::tempdir().expect("test workspace");
            let mut config = crate::config::Config::default();
            config.causal_tree.enabled = false;
            config.workspace_dir = workspace.path().to_path_buf();
            let generation = crate::config::new_shared(config.clone()).pin();
            let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
            let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(
                (0..catalog_size)
                    .map(|index| Box::new(PerfCatalogTool::new(format!("prx_perf_{index:04}"))) as Box<dyn Tool>)
                    .collect(),
            );
            let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
                Arc::clone(&registry),
                Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
                Arc::new(DenyApprovalStrategy),
                Arc::new(AdapterOwnedPreparation),
                Arc::new(TracingToolExecutionAudit),
            ));
            let provider_catalog = ProviderCatalogView::from_config(&config);
            let transports = CountingTransports(AtomicUsize::new(0));
            let service = build_turn_decision_service(
                TurnSurfaceKind::AgentLoop,
                TurnServiceBuild {
                    parent_grant: None,
                    generation: Arc::clone(&generation),
                    registry,
                    execution,
                    catalog_sources: sources(&generation, &config),
                    surface_policy: SurfacePolicy::default(),
                    provider_catalog: &provider_catalog,
                    transports: &transports,
                    events: Arc::new(SilentEvents),
                    memory: Arc::new(NoneMemory::new()),
                    security,
                },
            )
            .expect("synthetic disabled service");
            let proof = crate::agent::entry::test_cli_agent_turn_proof_with_input(
                &generation,
                "ollama",
                "prx-local-model",
                "summarize the neutral source",
            );
            let cancellation = CancellationToken::new();
            let specs_allocations: u64 = service
                .registry
                .iter()
                .map(|tool| crate::decision::builtin::allocation_counter::count(|| tool.catalog_specs()).1)
                .sum();
            let (_, descriptor_allocations) =
                crate::decision::builtin::allocation_counter::count(|| service.execution.current_descriptors());
            let (parity, parity_allocations) =
                crate::decision::builtin::allocation_counter::count(|| service.check_live_parity());
            parity.expect("fixture catalog parity");
            let model = service.resolve_agent_model(&proof).expect("fixture model");
            let catalog = service.catalog.view(&CatalogScope::default());
            let (_, cost_allocations) = crate::decision::builtin::allocation_counter::count(|| {
                super::super::turn_cost::build_cost_model(&catalog, &model, &config.cost, &BTreeMap::new())
            });
            let (result, allocations) = crate::decision::builtin::allocation_counter::count_future(
                service.decide_agent_initial(&proof, AgentTurnKind::SingleShot, &cancellation),
            )
            .await;
            let context = result.expect("builtin service decision");
            let candidates = match &context.engine().candidates {
                crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } => candidates.len(),
                crate::causal_tree::candidate_generator::CandidateSet::NoLegalPath { terminals, .. } => terminals.len(),
            };
            assert_eq!(transports.0.load(Ordering::SeqCst), 0);
            measurements.push((
                catalog_size,
                candidates,
                specs_allocations,
                descriptor_allocations,
                parity_allocations,
                cost_allocations,
                allocations,
            ));
        }
        let large = measurements.get(2).expect("warmed large catalog result");
        let first = measurements.get(3).expect("warmed small catalog result");
        assert_eq!(first.1, large.1, "candidate count must stay fixed");
        assert!(
            measurements
                .iter()
                .all(|(_, _, _, _, _, _, allocations)| *allocations > 0)
        );
        eprintln!("service allocation diagnostic (caller thread): {measurements:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::print_stderr)] // Diagnostic numbers belong in the captured test log.
    async fn enabled_service_allocation_diagnostic_holds_candidate_count_fixed() {
        let mut measurements = Vec::new();
        // Alternate sizes and compare the warmed pair: global regex and
        // projection initialization must not masquerade as catalog cost.
        for catalog_size in [64_usize, 0, 64, 0] {
            let workspace = tempfile::tempdir().expect("test workspace");
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_path_buf();
            config.causal_tree.enabled = true;
            config.causal_tree.policy.extra_latency_budget_ms = 5_000;
            config.providers.models.push(
                toml::from_str(
                    "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
                )
                .expect("verified local model fixture"),
            );
            let generation = crate::config::new_shared(config.clone()).pin();
            let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
            let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(PerfCatalogTool::new("file_read".to_string()))];
            tools.extend(
                (0..catalog_size)
                    .map(|index| Box::new(PerfCatalogTool::new(format!("prx_perf_{index:04}"))) as Box<dyn Tool>),
            );
            let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(tools);
            let surface_policy = SurfacePolicy {
                allowed_tools: None,
                direct_names: BTreeSet::from(["file_read".to_string()]),
                ..SurfacePolicy::default()
            };
            let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
                Arc::clone(&registry),
                Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
                Arc::new(DenyApprovalStrategy),
                Arc::new(AdapterOwnedPreparation),
                Arc::new(TracingToolExecutionAudit),
            ));
            let provider_catalog = ProviderCatalogView::from_config(&config);
            let transports = CountingTransports(AtomicUsize::new(0));
            let memory: Arc<dyn Memory> =
                Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"));
            let service = build_turn_decision_service(
                TurnSurfaceKind::AgentLoop,
                TurnServiceBuild {
                    parent_grant: None,
                    generation: Arc::clone(&generation),
                    registry,
                    execution,
                    catalog_sources: sources(&generation, &config),
                    surface_policy,
                    provider_catalog: &provider_catalog,
                    transports: &transports,
                    events: Arc::new(SilentEvents),
                    memory: Arc::clone(&memory),
                    security,
                },
            )
            .expect("enabled synthetic service");
            let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof(&generation, memory).await;
            let (result, allocations) = crate::decision::builtin::allocation_counter::count_future(
                service.decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new()),
            )
            .await;
            let context = result.expect("builtin service decision");
            let expected = service
                .catalog
                .view(&CatalogScope::default())
                .entries()
                .iter()
                .find(|entry| entry.public_name == "file_read")
                .map(|entry| CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                })
                .expect("fixed direct read capability");
            assert!(context.audit_hops().any(|hop| matches!(
                &hop.candidates,
                crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. }
                    if candidates.iter().any(|candidate| {
                        candidate.action() == crate::causal_tree::branch::CausalActionKind::UseTool
                            && candidate.required_capabilities() == [expected.clone()]
                    })
            )));
            let candidates = match &context.engine().candidates {
                crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } => candidates.len(),
                crate::causal_tree::candidate_generator::CandidateSet::NoLegalPath { terminals, .. } => terminals.len(),
            };
            assert_eq!(transports.0.load(Ordering::SeqCst), 0);
            let candidate_shape: Vec<_> = context
                .audit_hops()
                .map(|hop| {
                    let admitted = match &hop.candidates {
                        crate::causal_tree::candidate_generator::CandidateSet::Legal { candidates, .. } => candidates,
                        crate::causal_tree::candidate_generator::CandidateSet::NoLegalPath { terminals, .. } => {
                            terminals
                        }
                    };
                    admitted
                        .iter()
                        .map(|candidate| {
                            (
                                candidate.candidate_id().clone(),
                                candidate.action(),
                                candidate.target(),
                                candidate
                                    .required_capabilities()
                                    .iter()
                                    .map(|reference| (reference.id.clone(), reference.fingerprint))
                                    .collect::<Vec<_>>(),
                                candidate.side_effect(),
                                candidate.approval().clone(),
                                candidate.estimated_cost(),
                                candidate.estimated_latency_ms(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            let hop_contract: Vec<_> = context
                .audit_hops()
                .map(|hop| {
                    (
                        hop.request_compiled,
                        hop.prediction
                            .as_ref()
                            .map(|prediction| format!("{:?}", prediction.served_by)),
                        hop.fallback_cause,
                        hop.scored_count,
                        hop.scores.len(),
                        hop.selection.clone(),
                        format!("{:?}", hop.pre_commit.outcomes),
                    )
                })
                .collect();
            let directive = std::mem::discriminant(context.directive());
            measurements.push((
                catalog_size,
                candidates,
                allocations,
                candidate_shape,
                hop_contract,
                directive,
            ));
        }
        let large = measurements.get(2).expect("warmed large catalog measurement");
        let first = measurements.get(3).expect("warmed small catalog measurement");
        assert_eq!(first.1, large.1);
        assert_eq!(first.3, large.3, "each hop must admit the same candidate bodies");
        assert_eq!(first.4, large.4, "each hop must take the same prediction and gate path");
        assert_eq!(first.5, large.5, "the final directive must match");
        eprintln!("enabled service allocation diagnostic (caller thread): {measurements:?}");
    }

    #[async_trait]
    impl Tool for DriftingCatalogTool {
        fn name(&self) -> &str {
            "prx_drift_tool"
        }

        fn description(&self) -> &str {
            if self.changed.load(Ordering::SeqCst) {
                "changed PRX fixture description"
            } else {
                "initial PRX fixture description"
            }
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }

        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("catalog drift must not dispatch a tool")
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CatalogDriftMoment {
        BeforeTurn,
        AfterResponse,
        WhileBackendPending,
    }

    async fn run_catalog_drift_probe(
        moment: CatalogDriftMoment,
        cancel_while_pending: bool,
    ) -> (Result<Arc<TurnDecisionContext>, AgentDecisionError>, usize, usize) {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.policy.extra_latency_budget_ms = if moment == CatalogDriftMoment::WhileBackendPending {
            500
        } else {
            5_000
        };
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            )
            .expect("verified test model declaration"),
        );
        let generation = crate::config::new_shared(config.clone()).pin();
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("real fixture SQLite"));
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let changed = Arc::new(AtomicBool::new(false));
        let dispatches = Arc::new(AtomicUsize::new(0));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(DriftingCatalogTool {
            changed: Arc::clone(&changed),
            dispatches: Arc::clone(&dispatches),
        })]);
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let mut service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .expect("initial catalog parity");
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let backend_entered = Arc::new(tokio::sync::Notify::new());
        if moment != CatalogDriftMoment::BeforeTurn {
            let service_mut = Arc::get_mut(&mut service).expect("unshared fixture service");
            service_mut.factory = DecisionBackendFactory::from_slots(
                vec![Box::new(RetrieveProbeBackend {
                    identity: BackendIdentity {
                        kind: BackendKind::Mock,
                        provider: None,
                        configured_model: Arc::from("prx-local-model"),
                    },
                    requests: Arc::clone(&requests),
                    stall: moment == CatalogDriftMoment::WhileBackendPending,
                    entered: Arc::clone(&backend_entered),
                    drift_after_response: (moment == CatalogDriftMoment::AfterResponse).then(|| Arc::clone(&changed)),
                })],
                Arc::clone(&service_mut.state_key),
                OutboundSecrets::default(),
                SlotLimits::default(),
            );
        }

        let work = Box::pin(async {
            changed.store(moment == CatalogDriftMoment::BeforeTurn, Ordering::SeqCst);
            let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&memory),
                "prx_drift_tool",
                "prx_run_drift",
                ("ollama", "prx-local-model"),
            )
            .await;
            let cancellation = CancellationToken::new();
            let decision = service.decide_agent_initial(&proof, AgentTurnKind::Repl, &cancellation);
            let context = if moment == CatalogDriftMoment::WhileBackendPending {
                let change_during_wait = async {
                    backend_entered.notified().await;
                    changed.store(true, Ordering::SeqCst);
                    if cancel_while_pending {
                        cancellation.cancel();
                    }
                };
                let ((), result) = tokio::time::timeout(Duration::from_secs(3), async {
                    tokio::join!(change_during_wait, decision)
                })
                .await
                .expect("pending backend must reach bounded fallback");
                result
            } else {
                decision.await
            };
            let request_count = requests.lock().len();
            (context, request_count, dispatches.load(Ordering::SeqCst))
        });
        ledger_probe::finish_service_fixture(&service, work).await
    }

    #[tokio::test]
    async fn catalog_drift_closes_with_fresh_unlinked_terminal_and_no_dispatch() {
        for (moment, expected_requests) in [
            (CatalogDriftMoment::BeforeTurn, 0),
            (CatalogDriftMoment::AfterResponse, 1),
            (CatalogDriftMoment::WhileBackendPending, 1),
        ] {
            let (context, requests, dispatches) = run_catalog_drift_probe(moment, false).await;
            let context = context.expect("catalog drift closes locally");
            assert_eq!(requests, expected_requests);
            assert_eq!(dispatches, 0);
            assert_eq!(context.hop_ids().len(), 1, "stale work hop must be discarded");
            assert_eq!(context.audit_hops().count(), 1);
            assert_eq!(context.engine().state.lineage.prior_decision_id(), None);
            assert!(
                context.engine().prediction.is_none(),
                "recovery must not call a backend"
            );
            assert!(context.execution_refs().is_empty());
            assert!(matches!(
                context.directive(),
                TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. }
            ));
            let rows = DecisionRecordBuilder::build_enabled_chain(&context, &DecisionKey::ephemeral(), true, false)
                .expect("single local terminal audit row");
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows.first().expect("one local terminal row").lineage.prior_decision_id,
                None
            );
        }
    }

    #[tokio::test]
    async fn cancellation_during_backend_and_catalog_drift_wins_before_recovery() {
        let (result, requests, dispatches) =
            run_catalog_drift_probe(CatalogDriftMoment::WhileBackendPending, true).await;
        assert!(matches!(result, Err(AgentDecisionError::Cancelled)));
        assert_eq!(requests, 1);
        assert_eq!(dispatches, 0);
    }

    #[tokio::test]
    async fn catalog_drift_after_selected_read_discards_retrieved_work_chain() {
        let (reads, transports, requests, _, context) =
            run_retrieve_probe_case(true, ProbeCancelMode::CatalogDriftRead).await;
        let context = context.expect("selected read drift closes locally");
        assert_eq!(reads, 1);
        assert_eq!(requests.len(), 1);
        assert_eq!(transports, 0);
        assert_eq!(context.hop_ids().len(), 1);
        assert_eq!(context.engine().state.lineage.prior_decision_id(), None);
        assert!(context.engine().prediction.is_none());
        assert!(context.execution_refs().is_empty());
        assert!(matches!(context.directive(), TurnExecutionDirective::Refuse { .. }));
    }

    #[test]
    fn record_builder_distinguishes_local_terminal_from_failed_compiled_request() {
        use crate::causal_tree::branch::CausalActionKind;
        use crate::causal_tree::candidate_generator::CandidateSet;
        use crate::causal_tree::decision_select::Selection;
        use crate::causal_tree::gates::{GateOutcome, GateReport};
        use crate::causal_tree::state::fixtures;

        let key = fixtures::key();
        let state = fixtures::state(&key, "neutral request");
        let candidate = fixtures::candidate(&key, CausalActionKind::Refuse, CapabilityTarget::None, None).candidate;
        let candidate_id = candidate.candidate_id().clone();
        let gates = GateReport {
            outcomes: BTreeMap::from([(candidate_id.clone(), GateOutcome::Passed)]),
            gate_set_hash: [0; 32],
        };
        let decision_id = DecisionId::new_runtime();
        let engine = EngineDecision {
            decision_id,
            deadline: Instant::now() + Duration::from_secs(1),
            elapsed_ms: 7,
            state,
            candidates: CandidateSet::Legal {
                candidates: vec![candidate],
                exact_mentions: BTreeSet::new(),
                truncated: Vec::new(),
                discoverable_families: Default::default(),
                approval_scopes: BTreeMap::new(),
                report: gates.clone(),
            },
            selection: Selection::Abstain {
                action: CausalActionKind::Refuse,
                reason_code: "hop_stopped",
            },
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            audit_trace: None,
            fallback_cause: Some(crate::causal_tree::decision_engine::FallbackCause::HopStopped),
            scores: BTreeMap::new(),
            scored_count: 0,
            rehearsal: BTreeMap::new(),
            post_rehearsal: gates.clone(),
            pre_commit: gates,
            call_fingerprints: BTreeMap::new(),
            outgoing_child: None,
            outgoing_parent_allowed: None,
        };
        let mut context = TurnDecisionContext {
            resume: None,
            accounting: None,
            owner_filesystem: None,
            source_actor: (Arc::from("cli"), Arc::from("local-user")),
            service_id: uuid::Uuid::nil(),
            record_admitted: AtomicBool::new(false),
            execution_trace: std::sync::OnceLock::new(),
            served_auxiliary: std::sync::OnceLock::new(),
            execution_closed: AtomicBool::new(false),
            approval_collection_started: AtomicBool::new(false),
            collected_pending: std::sync::OnceLock::new(),
            discovery_offers: std::sync::OnceLock::new(),
            discovery_admissions: std::sync::OnceLock::new(),
            engine,
            prior_hops: Vec::new(),
            directive: TurnExecutionDirective::Refuse {
                policy_code: "hop_stopped".to_owned(),
            },
            main_compaction: crate::config::AgentCompactionConfig::default(),
            hop_ids: vec![decision_id],
            execution_refs: BTreeSet::new(),
            selected_retrieved: Vec::new(),
            selected_custom_memory_context: None,
            #[cfg(feature = "llm-router")]
            router_transition: None,
        };
        use crate::causal_tree::outcome::{DecisionRecordBuilder, RecordBuildError};
        assert!(matches!(
            DecisionRecordBuilder::build_enabled_chain(&context, &key, false, false),
            Err(RecordBuildError::Binding)
        ));
        let local =
            DecisionRecordBuilder::build_enabled_chain(&context, &key, true, false).expect("local terminal record");
        assert_eq!(local.len(), 1);
        let local_row = local.first().expect("one local terminal");
        assert_eq!(local_row.backend_kind, "none");
        assert_eq!(local_row.selected_candidate_id.as_deref(), Some(candidate_id.as_str()));
        assert_eq!(local_row.protocol_version, 0);
        assert_eq!(local_row.error_class, None);
        assert_eq!(local_row.abstention_reason.as_deref(), Some("hop_stopped"));
        context.engine.request_compiled = true;
        context.engine.fallback_cause = Some(crate::causal_tree::decision_engine::FallbackCause::DeadlineExceeded);
        let timed_out = DecisionRecordBuilder::build_enabled_chain(&context, &key, true, false)
            .expect("compiled request without validated answer");
        let timed_out_row = timed_out.first().expect("one compiled timeout");
        assert_eq!(timed_out_row.backend_kind, "none");
        assert_eq!(
            timed_out_row.protocol_version,
            u32::from(crate::decision::PROTOCOL_VERSION)
        );
        assert_eq!(timed_out_row.error_class, None);
        assert_eq!(timed_out_row.abstention_reason.as_deref(), Some("deadline_exceeded"));
        context.hop_ids.clear();
        assert!(matches!(
            DecisionRecordBuilder::build_enabled_chain(&context, &key, true, false),
            Err(RecordBuildError::Chain)
        ));
    }

    #[tokio::test]
    async fn agent_decision_cancellation_stops_before_backend_or_selected_read() {
        for mode in [ProbeCancelMode::Before, ProbeCancelMode::Backend, ProbeCancelMode::Read] {
            let (reads, transports, requests, emitted, result) = run_retrieve_probe_case(true, mode).await;
            assert!(matches!(result, Err(AgentDecisionError::Cancelled)));
            assert_eq!(transports, 0);
            assert!(emitted.iter().all(|event| {
                !matches!(
                    event.kind,
                    CausalEventKind::CandidateGated | CausalEventKind::CandidateScored
                ) || mode == ProbeCancelMode::Read
            }));
            match mode {
                ProbeCancelMode::Never | ProbeCancelMode::Before => {
                    assert_eq!(reads, 0);
                    assert!(requests.is_empty(), "expected an empty value");
                }
                ProbeCancelMode::Backend => {
                    assert_eq!(reads, 0);
                    assert_eq!(requests.len(), 1);
                }
                ProbeCancelMode::Read => {
                    assert_eq!(reads, 1);
                    assert_eq!(requests.len(), 1, "cancelled read cannot cause a dependent decision");
                }
                ProbeCancelMode::CatalogDriftRead => {
                    panic!("the cancellation test does not use catalog drift")
                }
            }
        }
    }

    #[tokio::test]
    async fn agent_decision_phases_follow_source_lineage_without_claiming_commit() {
        let (_, _, requests, emitted, context) = run_retrieve_probe_case(true, ProbeCancelMode::Never).await;
        let context = context.expect("bounded fixture decision");
        let phases: Vec<_> = emitted
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    CausalEventKind::SnapshotCreated
                        | CausalEventKind::CandidatesGenerated
                        | CausalEventKind::CandidateGated
                        | CausalEventKind::CandidateScored
                )
            })
            .collect();
        assert_eq!(context.hop_ids().len(), requests.len() + 1);
        for decision_id in context.hop_ids() {
            let id = decision_id.to_string();
            let hop: Vec<_> = phases
                .iter()
                .filter(|event| event.lineage.decision_id.as_deref() == Some(id.as_str()))
                .collect();
            assert_eq!(
                hop.first().map(|event| event.kind),
                Some(CausalEventKind::SnapshotCreated)
            );
            assert_eq!(
                hop.get(1).map(|event| event.kind),
                Some(CausalEventKind::CandidatesGenerated)
            );
            assert_eq!(
                hop.get(2).map(|event| event.kind),
                Some(CausalEventKind::CandidateGated)
            );
            assert!(hop.iter().all(|event| {
                event.lineage.run_id.as_deref() == Some("prx_run_retrieve")
                    && event.fields.item_count.is_none_or(|count| count <= 32)
            }));
        }
        let terminal = context.hop_ids().last().expect("terminal hop").to_string();
        assert!(phases.iter().any(|event| {
            event.kind == CausalEventKind::CandidateScored && event.fields.item_count.is_some_and(|count| count > 0)
        }));
        assert!(!phases.iter().any(|event| {
            event.kind == CausalEventKind::CandidateScored
                && event.lineage.decision_id.as_deref() == Some(terminal.as_str())
        }));
        assert!(!format!("{phases:?}").contains("find the scoped memory evidence"));
    }

    #[tokio::test]
    async fn selected_retrieve_redecides_only_after_material_evidence_and_honors_hop_cap() {
        let (reads, wire_calls, requests, context) = run_retrieve_probe(true).await;
        assert_eq!(reads, 3, "one selected read per allowed decision hop");
        assert_eq!(
            requests.len(),
            3,
            "terminal hop must not request another backend decision"
        );
        assert_eq!(
            requests
                .iter()
                .map(|request| &request.0)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
        assert!(requests.first().expect("first request").1.is_none());
        for (request, prior_id) in requests.iter().skip(1).zip(context.hop_ids()) {
            let prior = prior_id.as_uuid().to_string();
            assert_eq!(request.1.as_deref(), Some(prior.as_str()));
        }
        assert_eq!(context.hop_ids().len(), 4, "three decisions and one terminal gate");
        let recorded: Vec<_> = context.audit_hops().collect();
        assert_eq!(recorded.len(), 4, "each completed hop retains its own audit inputs");
        assert_eq!(
            recorded.iter().map(|hop| hop.decision_id).collect::<Vec<_>>(),
            context.hop_ids()
        );
        assert!(recorded.iter().take(3).all(|hop| hop.prediction.is_some()));
        assert!(
            recorded.last().is_some_and(|hop| hop.prediction.is_none()),
            "local terminal has no backend provenance"
        );
        assert_eq!(wire_calls, 0);

        let (reads, wire_calls, requests, context) = run_retrieve_probe(false).await;
        assert_eq!(reads, 2, "a repeated read may occur once before no-progress detection");
        assert_eq!(
            requests.len(),
            2,
            "no material change must stop another backend request"
        );
        assert_eq!(
            context.hop_ids().len(),
            3,
            "a local terminal gate still closes the turn"
        );
        assert_eq!(wire_calls, 0);
    }

    #[tokio::test]
    async fn memory_rehearsal_rejects_wrong_principal_or_query_before_backend() {
        let memory = CountingProbeMemory {
            calls: AtomicUsize::new(0),
            retrieval_calls: AtomicUsize::new(0),
            return_empty: AtomicBool::new(false),
            return_error: AtomicBool::new(false),
            return_never: AtomicBool::new(false),
            return_stale: AtomicBool::new(false),
            unique_results: AtomicBool::new(false),
            drift_on_read: None,
            read_entered: Arc::new(tokio::sync::Notify::new()),
            retrieval_args: parking_lot::Mutex::new(None),
        };
        let scope = MemoryWriteContext::default();
        let key = DecisionKey::ephemeral();
        let probe = ScopedMemoryRehearsal {
            memory: &memory,
            scope: &scope,
            query: "prx bound query",
            principal: "prx_owner",
            key: &key,
        };
        let query_ref = key.hmac(KeyDomain::StateFingerprint, &[b"rehearsal-query", b"prx bound query"]);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        assert!(probe.count_hits("foreign_owner", query_ref, 5, deadline).await.is_err());
        assert!(probe.count_hits("prx_owner", [0; 32], 5, deadline).await.is_err());
        assert!(probe.count_hits("prx_owner", query_ref, 0, deadline).await.is_err());
        assert_eq!(memory.calls.load(Ordering::SeqCst), 0);
        assert_eq!(probe.count_hits("prx_owner", query_ref, 5, deadline).await.unwrap(), 1);
        assert_eq!(memory.calls.load(Ordering::SeqCst), 1);
    }
    use async_trait::async_trait;

    struct RehearsalReadTool {
        name: &'static str,
        configured_only: bool,
    }

    struct AliasRootTool;

    #[async_trait]
    impl Tool for AliasRootTool {
        fn name(&self) -> &str {
            "prx_root"
        }

        fn description(&self) -> &str {
            "PRX alias root fixture"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}})
        }

        fn supports_name(&self, name: &str) -> bool {
            matches!(name, "prx_root" | "prx_alias" | "prx_other_alias")
        }

        fn specs(&self) -> Vec<crate::tools::traits::ToolSpec> {
            ["prx_root", "prx_alias", "prx_other_alias"]
                .into_iter()
                .map(|name| crate::tools::traits::ToolSpec {
                    name: name.to_string(),
                    description: self.description().to_string(),
                    parameters: self.parameters_schema(),
                })
                .collect()
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn agent_model_allowlist_keeps_selected_root_aliases_and_excludes_other_roots() {
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
            Box::new(AliasRootTool),
            Box::new(RehearsalReadTool {
                name: "prx_unrelated",
                configured_only: false,
            }),
        ]);
        let catalog =
            CapabilityCatalog::build(registry.as_ref(), &CatalogSources::default(), &SurfacePolicy::default())
                .expect("catalog fixture");
        let security = Arc::new(SecurityPolicy::default());
        let execution = ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        );
        catalog
            .check_parity(&execution)
            .expect("publishable root and alias catalog");
        let view = catalog.view(&CatalogScope::default());
        let mut config = crate::config::Config::default();
        config
            .tool_tiering
            .model_allowlists
            .insert("prx-local-model".into(), vec!["prx_alias".into()]);
        let exclusions = effective_agent_exclusions(
            &config,
            registry.as_ref(),
            &view,
            "prx-local-model",
            "prx-policy",
            &crate::causal_tree::state::fixtures::key(),
        );
        assert!(
            !exclusions
                .model_admission
                .as_ref()
                .unwrap()
                .excludes(view.model_admission_entry(catalog.entry_for_public_name("prx_alias").unwrap()))
        );
        assert!(
            !exclusions
                .model_admission
                .as_ref()
                .unwrap()
                .excludes(view.model_admission_entry(catalog.entry_for_public_name("prx_other_alias").unwrap()))
        );
        assert!(
            exclusions
                .model_admission
                .as_ref()
                .unwrap()
                .excludes(view.model_admission_entry(catalog.entry_for_public_name("prx_unrelated").unwrap()))
        );
    }

    #[async_trait]
    impl Tool for RehearsalReadTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "PRX catalog rehearsal fixture"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}})
        }

        fn availability(&self) -> crate::capability::CapabilityAvailability {
            if self.configured_only {
                crate::capability::CapabilityAvailability::configured("fixture config only")
            } else {
                crate::capability::CapabilityAvailability::ready("fixture backend registered")
            }
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn pinned_rehearsal_counts_only_visible_exact_refs_and_skill_family() {
        let registry: Vec<Box<dyn Tool>> = vec![
            Box::new(RehearsalReadTool {
                name: "prx_rehearsal_read",
                configured_only: false,
            }),
            Box::new(RehearsalReadTool {
                name: "prx_rehearsal_configured",
                configured_only: true,
            }),
        ];
        let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())
            .expect("catalog fixture");
        let view = catalog.view(&CatalogScope::default());
        let entry = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == "prx_rehearsal_read")
            .expect("ready fixture entry");
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: entry.generation,
        };
        let probe = PinnedCatalogRehearsal { catalog: &view };
        assert_eq!(
            probe
                .availability(view.generation(), std::slice::from_ref(&reference))
                .await
                .unwrap()
                .present,
            1
        );
        assert_eq!(
            probe
                .metadata(view.generation(), std::slice::from_ref(&reference))
                .await
                .unwrap()
                .present,
            0
        );
        let mut stale = reference.clone();
        stale.fingerprint.0[0] ^= 1;
        assert_eq!(
            probe.availability(view.generation(), &[stale]).await.unwrap().present,
            0
        );
        let mut stale_generation = view.generation();
        stale_generation.0[0] ^= 1;
        assert!(probe.availability(stale_generation, &[]).await.is_err());
        let configured = view
            .entries()
            .iter()
            .find(|entry| entry.public_name == "prx_rehearsal_configured")
            .expect("configured fixture entry");
        let configured_ref = CapabilityRef {
            id: configured.id.clone(),
            fingerprint: configured.fingerprint,
            generation: configured.generation,
        };
        assert_eq!(
            probe
                .availability(view.generation(), &[configured_ref])
                .await
                .unwrap()
                .present,
            0
        );
        let hidden_view = catalog.view(&CatalogScope {
            families: std::iter::once(CapabilityTarget::Memory).collect(),
            channel: false,
        });
        let hidden_probe = PinnedCatalogRehearsal { catalog: &hidden_view };
        assert_eq!(
            hidden_probe
                .availability(hidden_view.generation(), &[reference])
                .await
                .unwrap()
                .present,
            0
        );
    }

    #[test]
    fn decision_reservation_uses_effective_input_window_and_rejects_tiny_windows() {
        let mut config = crate::config::AgentCompactionConfig::default();
        config.max_context_tokens = 32_768;
        config.reserve_tokens = 4_096;
        let reservation = DecisionTokenReservation::from_effective_compaction(&config).unwrap();
        assert_eq!(reservation.decision_tokens, 1_792);
        assert_eq!(reservation.main_input_limit, 26_880);
        assert_eq!(reservation.decision_tokens + reservation.main_input_limit, 28_672);
        let reserved = reservation.reserved_compaction(&config).unwrap();
        assert_eq!(reserved.reserve_tokens, 5_888);
        assert_eq!(
            reserved.max_context_tokens - reserved.reserve_tokens,
            reservation.main_input_limit
        );
        assert_eq!(config.reserve_tokens, 4_096, "the effective route config is immutable");
        let mut changed_route = config.clone();
        changed_route.max_context_tokens = 16_384;
        assert!(
            matches!(
                reservation.reserved_compaction(&changed_route),
                Err(DecisionReserveUnavailable)
            ),
            "a reservation is bound to its effective input window"
        );

        config.max_context_tokens = 100_000;
        config.reserve_tokens = 1;
        let capped = DecisionTokenReservation::from_effective_compaction(&config).unwrap();
        assert_eq!(capped.decision_tokens, 4_096);
        assert_eq!(capped.main_input_limit, 95_903);

        config.max_context_tokens = 16;
        config.reserve_tokens = 0;
        let smallest = DecisionTokenReservation::from_effective_compaction(&config).unwrap();
        assert_eq!(smallest.decision_tokens, 1);
        assert_eq!(smallest.main_input_limit, 15);

        for (window, response_reserve) in [(0, 0), (4, 5), (15, 0), (4_096, 4_096)] {
            config.max_context_tokens = window;
            config.reserve_tokens = response_reserve;
            assert_eq!(
                DecisionTokenReservation::from_effective_compaction(&config),
                Err(DecisionReserveUnavailable)
            );
        }
    }

    #[test]
    fn agent_budget_uses_pinned_route_and_read_only_spend_snapshot() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.causal_tree.enabled = false;
        config.workspace_dir = workspace.path().to_path_buf();
        config.agent.compaction.max_context_tokens = 32_768;
        config.agent.compaction.max_context_tokens_explicit = true;
        config.agent.compaction.reserve_tokens = 4_096;
        config.cost.daily_limit_usd = 100.0;
        config.cost.monthly_limit_usd = 100.0;
        config.autonomy.max_cost_per_day_cents = 1_000;
        let tracker =
            crate::cost::tracker::CostTracker::new(config.cost.clone(), workspace.path()).expect("test cost ledger");
        tracker
            .record_usage(crate::cost::types::TokenUsage::new(
                "test/model",
                1_000_000,
                0,
                9.5,
                0.0,
            ))
            .expect("prior spend");
        let generation = crate::config::new_shared(config.clone()).pin();
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let service = disabled_agent_service(&generation, &provider_catalog);
        let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "prx-local-model");
        let snapshot = service.agent_budget_snapshot(&proof).expect("source-owned budget");
        assert_eq!(snapshot.budgets.tokens, 1_792);
        assert_eq!(snapshot.budgets.micro_usd, 500_000);
        assert_eq!(
            snapshot.budgets.latency_ms,
            config.causal_tree.policy.extra_latency_budget_ms
        );
        assert_eq!(snapshot.main_compaction.max_context_tokens, 32_768);
        assert_eq!(snapshot.main_compaction.reserve_tokens, 5_888);
        assert_eq!(config.agent.compaction.reserve_tokens, 4_096);
        assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());

        let mut other_config = config;
        other_config.agent.compaction.max_context_tokens = 15;
        let other_generation = crate::config::new_shared(other_config.clone()).pin();
        let other_service = disabled_agent_service(&other_generation, &ProviderCatalogView::from_config(&other_config));
        let other_proof =
            crate::agent::entry::test_cli_agent_turn_proof(&other_generation, "ollama", "prx-local-model");
        assert!(matches!(
            other_service.agent_budget_snapshot(&other_proof),
            Err(AgentDecisionBudgetError::Reserve(_))
        ));
        assert!(matches!(
            service.agent_budget_snapshot(&other_proof),
            Err(AgentDecisionBudgetError::Model(TurnModelError::Origin(_)))
        ));
    }

    #[tokio::test]
    async fn disabled_agent_initial_decision_runs_generator_gates_and_builtin() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = false;
        let generation = crate::config::new_shared(config.clone()).pin();
        let service = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config));
        let proof = crate::agent::entry::test_cli_agent_turn_proof_with_input(
            &generation,
            "ollama",
            "prx-local-model",
            "Explain `2 + 2`.",
        );
        let result = service
            .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
            .await
            .expect("source-bound builtin decision");
        assert!(matches!(result.directive(), TurnExecutionDirective::Refuse { .. }));
        assert!(matches!(
            &result.engine().candidates,
            crate::causal_tree::candidate_generator::CandidateSet::Legal { .. }
        ));
        assert_eq!(
            result
                .engine()
                .prediction
                .as_ref()
                .map(|prediction| prediction.served_by),
            Some(crate::decision::factory::ServedBy::Builtin)
        );
        assert!(!result.engine().pre_commit.outcomes.is_empty());
        assert_eq!(
            result.main_compaction().max_context_tokens,
            config.agent.compaction.max_context_tokens
        );
        assert_eq!(result.hop_ids(), &[result.decision_id()]);
        assert_eq!(
            result.catalog_generation(),
            result.engine().state.trusted.parts().catalog_generation
        );
        assert_eq!(
            result.inherited_constraint_codes(),
            result.engine().state.trusted.parts().constraints.as_slice()
        );
        assert!(result.execution_refs().is_empty());
        assert!(
            result.retrieved().expect("live selected evidence").is_empty(),
            "expected an empty value"
        );
        assert!(matches!(result.disposition(), Ok(TurnDisposition::Close { .. })));
        // Exercise fresh admission with this actual selected engine decision,
        // not a fabricated selection or an already-consumed context getter.
        let directive_context = DirectiveContext {
            current_generation: result.catalog_generation(),
            approval_expires_at: chrono::Utc::now(),
            discovery_limit: 1,
        };
        assert!(matches!(
            directive_from_selection(result.engine(), &directive_context),
            Ok(TurnExecutionDirective::Refuse { .. })
        ));
        let issued_id = result.decision_id();
        let issued_input = result.engine().state.routing_input.clone();
        let issued_trusted = result.engine().state.trusted.clone();
        let mut expired = result;
        Arc::get_mut(&mut expired)
            .expect("unshared fixture context")
            .engine
            .deadline = Instant::now();
        assert!(matches!(
            directive_from_selection(expired.engine(), &directive_context),
            Ok(TurnExecutionDirective::Refuse { .. })
        ));
        // Fixed local closure remains valid after the decision deadline; it
        // grants no provider execution. Actual terminal closure seals access.
        assert!(
            expired.retrieved().expect("already selected evidence").is_empty(),
            "expected an empty value"
        );
        assert!(matches!(expired.disposition(), Ok(TurnDisposition::Close { .. })));
        assert_eq!(expired.decision_id(), issued_id);
        assert_eq!(expired.engine().state.routing_input, issued_input);
        assert_eq!(expired.engine().state.trusted, issued_trusted);
        let _ = expired.terminal_execution_trace();
        assert!(matches!(expired.retrieved(), Err(DirectiveError::NotAdmitted)));
        // Sealing denies evidence access; rendering a fixed close remains safe.
        assert!(matches!(expired.disposition(), Ok(TurnDisposition::Close { .. })));
        assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
    }

    #[tokio::test]
    async fn disabled_or_keyless_fixed_close_has_no_applied_write() {
        for enabled in [false, true] {
            let workspace = tempfile::tempdir().expect("test workspace");
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_path_buf();
            config.causal_tree.enabled = enabled;
            if enabled {
                let directory = workspace.path().join("self/causal_tree");
                std::fs::create_dir_all(&directory).expect("fixture key directory");
                std::fs::write(directory.join(".decision_key"), b"short").expect("unavailable fixture key");
            }
            let memory: Arc<dyn Memory> =
                Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("ordinary history memory"));
            let generation = crate::config::new_shared(config.clone()).pin();
            let service = agent_service_with_memory(
                &generation,
                &ProviderCatalogView::from_config(&config),
                Arc::clone(&memory),
            );
            let proof = if enabled {
                crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
                    &generation,
                    Arc::clone(&memory),
                    "  ",
                    "prx_keyless_close_run",
                )
                .await
            } else {
                crate::agent::entry::test_cli_agent_turn_proof_with_input(
                    &generation,
                    "ollama",
                    "prx-local-model",
                    "  ",
                )
            };
            let context = service
                .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
                .await
                .expect("local fixed decision");
            assert!(matches!(context.disposition(), Ok(TurnDisposition::Close { .. })));
            let decision = if enabled {
                assert!(!service.durable_key, "corrupt key must not issue durable authority");
                assert!(!context.record_admitted.load(Ordering::Acquire));
                assert!(
                    service.terminal_settlement_ref(Arc::clone(&context)).is_err(),
                    "enabled keyless context must not issue a terminal decision"
                );
                None
            } else {
                Some(
                    service
                        .terminal_settlement_ref(Arc::clone(&context))
                        .expect("disabled service issues a genuine same-context terminal decision"),
                )
            };
            let now = chrono::Utc::now();
            let ports = FinalizingClosePorts {
                fabric: MemoryFabric::new(Arc::clone(&memory), proof.workspace_id()),
                commit: crate::agent::terminal::TurnTerminalDraft {
                    decision,
                    terminal_id: "prx_unrecorded_close".to_owned(),
                    scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                        .with_owner_id(proof.owner_id())
                        .with_channel("cli")
                        .with_session_key(proof.session_key())
                        .with_run_id(proof.run_id()),
                    status: crate::agent::terminal::TurnTerminalStatus::Completed,
                    history: None,
                    history_scope: None,
                    provider_outcome: None,
                    telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                        summary: "local close".to_owned(),
                        started_at: now,
                        finished_at: now,
                    },
                    delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                },
                cost: config.cost.clone(),
                workspace: workspace.path().to_path_buf(),
                calls: AtomicUsize::new(0),
                cancellation: None,
            };
            let close = service
                .close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &CancellationToken::new())
                .await;
            if enabled {
                let error = close.expect_err("enabled keyless close must fail at terminal linkage");
                assert_eq!(error.to_string(), "terminal draft has no service-issued decision");
                assert!(
                    memory
                        .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:prx_unrecorded_close:final")
                        .await
                        .expect("keyless terminal marker lookup")
                        .is_none(),
                    "keyless close must not persist a terminal marker"
                );
            } else {
                close.expect("ordinary close stays available");
            }
            assert_eq!(ports.calls.load(Ordering::SeqCst), 1);
            assert!(!context.record_admitted.load(Ordering::Acquire));
            assert!(service.durable_events.is_none());
            assert!(
                memory
                    .find_message_event_by_idempotency_key(
                        proof.workspace_id(),
                        &format!("causal:{}:directive-applied", context.decision_id())
                    )
                    .await
                    .expect("no applied write")
                    .is_none()
            );
            assert!(!workspace.path().join("self/causal_tree/decisions.db").exists());
        }
    }

    #[tokio::test]
    async fn agent_initial_decision_respects_configured_four_kib_wire_limit() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.causal_tree.enabled = false;
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.backend.max_request_bytes = 4096;
        let generation = crate::config::new_shared(config.clone()).pin();
        let service = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config));
        let raw = "PRX route evidence 01. ".repeat(1_000);
        let proof =
            crate::agent::entry::test_cli_agent_turn_proof_with_input(&generation, "ollama", "prx-local-model", &raw);
        assert!(matches!(
            service
                .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
                .await,
            Err(AgentDecisionError::Engine(EngineError::Compiler(
                crate::causal_tree::questions::CompileError::Budget
            )))
        ));
        let mut wide = config;
        wide.causal_tree.backend.max_request_bytes = 65_536;
        let wide_generation = crate::config::new_shared(wide.clone()).pin();
        let wide_service = disabled_agent_service(&wide_generation, &ProviderCatalogView::from_config(&wide));
        let wide_proof = crate::agent::entry::test_cli_agent_turn_proof_with_input(
            &wide_generation,
            "ollama",
            "prx-local-model",
            &raw,
        );
        let wide_result = wide_service
            .decide_agent_initial(&wide_proof, AgentTurnKind::SingleShot, &CancellationToken::new())
            .await
            .expect("the same projected request fits the wider configured cap");
        assert_eq!(
            wide_result
                .engine()
                .prediction
                .as_ref()
                .map(|prediction| prediction.served_by),
            Some(crate::decision::factory::ServedBy::Builtin)
        );
        assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
    }

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

    fn disabled_agent_service(
        generation: &Arc<ConfigGeneration>,
        provider_catalog: &ProviderCatalogView,
    ) -> Arc<TurnDecisionService> {
        agent_service_with_memory(generation, provider_catalog, Arc::new(NoneMemory::new()))
    }

    fn agent_service_with_memory(
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
        .expect("Agent fixture service")
    }

    #[tokio::test]
    async fn selected_memory_service_rejects_foreign_turn_before_backend_read() {
        use crate::causal_tree::branch::{
            ApprovalRequirement, CandidateSpec, CapabilityRef, CapabilityTarget, CausalActionKind, CausalCandidate,
            SideEffectClass,
        };
        use crate::causal_tree::candidate_generator::CandidateSet;
        use crate::causal_tree::decision_select::Selection;
        use crate::causal_tree::gates::{GateOutcome, GateReport};
        use crate::causal_tree::state::{RawRoutingInput, TrustedState, fixtures};

        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config.clone()).pin();
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let memory = Arc::new(CountingProbeMemory {
            calls: AtomicUsize::new(0),
            retrieval_calls: AtomicUsize::new(0),
            return_empty: AtomicBool::new(false),
            return_error: AtomicBool::new(false),
            return_never: AtomicBool::new(false),
            return_stale: AtomicBool::new(false),
            unique_results: AtomicBool::new(false),
            drift_on_read: None,
            read_entered: Arc::new(tokio::sync::Notify::new()),
            retrieval_args: parking_lot::Mutex::new(None),
        });
        let ledger_memory: Arc<dyn Memory> = Arc::new(ledger_probe::LedgerProbeMemory {
            ledger: Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("real fixture SQLite")),
            probe: Arc::clone(&memory),
        });
        let registry: Arc<Vec<Box<dyn Tool>>> =
            Arc::new(vec![Box::new(crate::tools::memory_search::MemorySearchTool::new(
                workspace.path().to_path_buf(),
                Arc::clone(&ledger_memory),
                false,
            ))]);
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let transports = CountingTransports(AtomicUsize::new(0));
        let service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&ledger_memory),
                security,
            },
        )
        .expect("SQLite-backed fixture service");
        let work = Box::pin(async {
            let proof_a = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&ledger_memory),
                "same query",
                "prx_run_a",
                ("ollama", "prx-local-model"),
            )
            .await;
            let proof_b = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&ledger_memory),
                "same query",
                "prx_run_b",
                ("ollama", "prx-local-model"),
            )
            .await;
            let model = service.resolve_agent_model(&proof_a).expect("pinned model");
            let view = service.catalog.view(&CatalogScope::default());
            let entry = view
                .entries()
                .iter()
                .find(|entry| entry.public_name == "memory_search")
                .expect("read catalog anchor");
            let reference = CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            };
            let mut trusted = fixtures::trusted();
            trusted.surface = TurnSurfaceKind::AgentLoop;
            trusted.channel = None;
            trusted.workspace_id = Arc::from(proof_a.workspace_id());
            trusted.owner_id = Some(Arc::from(proof_a.owner_id()));
            trusted.session_key = Some(Arc::from(proof_a.session_key()));
            trusted.run_id = Some(Arc::from(proof_a.run_id()));
            trusted.source_event_id = proof_a.source_event_id().map(Arc::from);
            trusted.catalog_generation = view.generation();
            trusted.policy_version = Arc::clone(&service.policy_version);
            trusted.resolved_model = Arc::from(format!("{}:{}", model.provider_label(), model.model()));
            let effective = effective_agent_exclusions(
                &config,
                &service.registry,
                &view,
                model.model(),
                &service.policy_version,
                &service.state_key,
            );
            trusted.exclusions = effective.names;
            trusted.model_admission = effective.model_admission;
            let mut state = fixtures::state(&service.state_key, "same query");
            state.trusted = TrustedState::from_runtime(trusted);
            state.observed = ObservedState::from_runtime_records(Vec::new(), Vec::new(), 0);
            state.routing_input = RawRoutingInput::new("same query".to_string());
            let candidate = CausalCandidate::new(
                CausalActionKind::Retrieve,
                CapabilityTarget::Memory,
                CandidateSpec {
                    namespace: None,
                    required_capabilities: vec![reference],
                    side_effect: SideEffectClass::ReadOnly,
                    approval: ApprovalRequirement::None,
                    estimated_cost: None,
                    estimated_latency_ms: None,
                },
            );
            let id = candidate.candidate_id().clone();
            let report = GateReport {
                outcomes: BTreeMap::from([(id.clone(), GateOutcome::Passed)]),
                gate_set_hash: [0; 32],
            };
            let mut decision = EngineDecision {
                decision_id: DecisionId::new_runtime(),
                deadline: Instant::now() + Duration::from_secs(5),
                elapsed_ms: 0,
                state,
                candidates: CandidateSet::Legal {
                    candidates: vec![candidate],
                    exact_mentions: BTreeSet::new(),
                    truncated: Vec::new(),
                    discoverable_families: Default::default(),
                    approval_scopes: BTreeMap::new(),
                    report: report.clone(),
                },
                selection: Selection::Commit {
                    candidate_id: id.clone(),
                    propensity: 1.0,
                },
                exploration_propensities: BTreeMap::new(),
                prediction: None,
                request_compiled: false,
                audit_trace: None,
                fallback_cause: None,
                scores: BTreeMap::new(),
                scored_count: 0,
                rehearsal: BTreeMap::new(),
                post_rehearsal: report.clone(),
                pre_commit: report,
                call_fingerprints: BTreeMap::new(),
                outgoing_child: None,
                outgoing_parent_allowed: None,
            };
            assert!(matches!(
                service.observe_selected_agent_memory(&proof_b, &decision).await,
                Err(AgentRetrievalError::Origin(_))
            ));
            assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 0);
            decision.state.lineage = HopLineage::After {
                hop: NonZeroU8::new(config.causal_tree.policy.max_decision_hops).expect("positive fixture limit"),
                prior_decision_id: decision.decision_id.as_uuid(),
            };
            assert!(matches!(
                service.observe_selected_agent_memory(&proof_a, &decision).await,
                Err(AgentRetrievalError::HopLimit)
            ));
            assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 0);
            decision.state.lineage = HopLineage::First;
            let observed = service
                .observe_selected_agent_memory(&proof_a, &decision)
                .await
                .expect("selected read and runtime observation");
            assert!(observed.fresh_read);
            assert_eq!(observed.selected_artifacts.len(), 1);
            let next = observed.state;
            assert_eq!(next.observed.artifacts().len(), 1);
            let [step] = next.observed.completed_steps() else {
                panic!("one runtime-owned read step");
            };
            assert_eq!(step.kind(), ObservedStepKind::Retrieve);
            assert_eq!(step.status(), ObservedStepStatus::Ok);
            assert_eq!(step.candidate(), Some(&id));
            assert_eq!(step.event_id(), &None);
            assert_eq!(next.lineage.hop(), 1);
            assert_eq!(next.lineage.prior_decision_id(), Some(decision.decision_id.as_uuid()));
            assert!(next.derived.candidates().is_empty(), "expected an empty value");
            assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 1);
            {
                let args = memory.retrieval_args.lock();
                let (query, limit, scope, deadline) = args.as_ref().expect("source-scoped read arguments");
                assert_eq!(query, proof_a.routing_input());
                assert_eq!(*limit, 5);
                assert_eq!(scope.workspace_id, proof_a.memory_scope().workspace_id);
                assert_eq!(scope.channel, proof_a.memory_scope().channel);
                assert_eq!(scope.chat_type, proof_a.memory_scope().chat_type);
                assert_eq!(scope.chat_id, proof_a.memory_scope().chat_id);
                assert_eq!(scope.sender_id, proof_a.memory_scope().sender_id);
                assert_eq!(scope.owner_id, proof_a.memory_scope().owner_id);
                assert_eq!(scope.raw_sender, proof_a.memory_scope().raw_sender);
                assert_eq!(scope.require_owner_match, proof_a.memory_scope().require_owner_match);
                assert_eq!(*deadline, decision.deadline);
            }

            let call_fingerprints = BTreeMap::new();
            let replayed_non_idempotent = BTreeSet::new();
            let verified_approval_calls = BTreeMap::new();
            let gates = GateInputs {
                policy: &service.security,
                owner_id: proof_a.owner_id(),
                sender: "local-user",
                policy_channel: "cli",
                chat_type: "private",
                model_ref: next.trusted.parts().resolved_model.as_ref(),
                model_can_chat: true,
                model_can_call_tools: false,
                decision_endpoint_ready: true,
                child_scope: None,
                verified_approval_calls,
                call_fingerprints: &call_fingerprints,
                failed_call_counts: BTreeMap::new(),
                replayed_non_idempotent: &replayed_non_idempotent,
                retry_limit: Some(0),
            };
            let cost_model = super::super::turn_cost::build_cost_model(&view, &model, &config.cost, &BTreeMap::new());
            let discovered_refs = DiscoveredRefs::empty();
            let rehearsal_ports = RehearsalPorts {
                memory: None,
                catalog: None,
                skill: None,
            };
            let inputs = EngineInputs {
                state: &next,
                catalog: &view,
                config: &config.causal_tree,
                key: &service.state_key,
                decision_id: DecisionId::new_runtime(),
                cost_model: &cost_model,
                discovered_refs: &discovered_refs,
                gates: &gates,
                delegation: None,
                delegation_max_depth: 0,
                prediction: service.factory.prediction_port(None),
                rehearsal_ports: &rehearsal_ports,
                request_limits: RequestLimits::default(),
                model: Some(model.model()),
                can_deliver_text: true,
                may_delegate: false,
                delegation_target: None,
                terminal_only: true,
                allow_terminal_answer: true,
                deadline: decision.deadline,
                inject_builtin_failure: false,
                after_prediction_pause: None,
            };
            let terminal = CausalDecisionEngine::decide_terminal_prepared(
                CausalDecisionEngine::prepare_hop(&inputs).expect("fresh terminal preparation"),
            )
            .expect("fresh terminal selection");
            assert!(matches!(
                directive_from_selection(
                    &terminal,
                    &DirectiveContext {
                        current_generation: view.generation(),
                        approval_expires_at: chrono::Utc::now(),
                        discovery_limit: 1,
                    },
                ),
                Ok(TurnExecutionDirective::AnswerDirectly)
            ));
            assert!(terminal.prediction.is_none(), "local terminal makes no backend call");

            let answer_context = TurnDecisionContext {
                resume: None,
                accounting: None,
                owner_filesystem: None,
                source_actor: (Arc::from("cli"), Arc::from("local-user")),
                service_id: uuid::Uuid::nil(),
                record_admitted: AtomicBool::new(false),
                execution_trace: std::sync::OnceLock::new(),
                served_auxiliary: std::sync::OnceLock::new(),
                execution_closed: AtomicBool::new(false),
                approval_collection_started: AtomicBool::new(false),
                collected_pending: std::sync::OnceLock::new(),
                discovery_offers: std::sync::OnceLock::new(),
                discovery_admissions: std::sync::OnceLock::new(),
                hop_ids: vec![decision.decision_id, terminal.decision_id],
                prior_hops: Vec::new(),
                engine: terminal,
                directive: TurnExecutionDirective::AnswerDirectly,
                main_compaction: config.agent.compaction.clone(),
                execution_refs: BTreeSet::new(),
                selected_retrieved: observed.selected_artifacts.clone(),
                selected_custom_memory_context: None,
                #[cfg(feature = "llm-router")]
                router_transition: None,
            };
            assert!(matches!(answer_context.disposition(), Ok(TurnDisposition::Run)));
            assert_eq!(answer_context.retrieved().expect("live selected evidence").len(), 1);
            let close_spy = CloseSpy::default();
            let mut answer_context = Arc::new(answer_context);
            assert!(
                close_directive_turn(Arc::clone(&answer_context), &close_spy)
                    .await
                    .is_err()
            );
            assert!(close_spy.replies.lock().is_empty(), "Run cannot call the terminal port");
            let answer_context_mut = Arc::get_mut(&mut answer_context).expect("exclusive context after close check");
            answer_context_mut.engine.deadline = Instant::now();
            // Decision work has ended; the admitted turn retains its selected
            // evidence until the real terminal lifecycle seals execution.
            assert!(matches!(answer_context.disposition(), Ok(TurnDisposition::Run)));
            assert_eq!(
                answer_context
                    .retrieved()
                    .expect("selected evidence before terminal closure")
                    .len(),
                1
            );
            let _ = answer_context.terminal_execution_trace();
            assert!(matches!(answer_context.disposition(), Err(DirectiveError::NotAdmitted)));
            assert!(matches!(answer_context.retrieved(), Err(DirectiveError::NotAdmitted)));
            assert!(
                close_directive_turn(Arc::clone(&answer_context), &close_spy)
                    .await
                    .is_err()
            );
            assert!(
                close_spy.replies.lock().is_empty(),
                "expired Run cannot call the terminal port"
            );
            // Even a corrupted internal reason code cannot escape through Close.
            Arc::get_mut(&mut answer_context)
                .expect("exclusive context after close check")
                .directive = TurnExecutionDirective::Refuse {
                policy_code: "HIDDEN-CAP-9V".to_string(),
            };
            let TurnDisposition::Close { reply } = answer_context.disposition().expect("fixed close reply") else {
                panic!("refusal must close");
            };
            assert_eq!(reply, "I can't complete this request under the current policy.");
            assert!(!reply.contains("HIDDEN-CAP-9V"));
            assert_eq!(
                close_directive_turn(Arc::clone(&answer_context), &close_spy)
                    .await
                    .expect("fixed refusal reaches terminal port"),
                "fixture terminal receipt"
            );
            assert_eq!(close_spy.replies.lock().as_slice(), &[reply.as_ref()]);
            close_spy.fail.store(true, Ordering::SeqCst);
            assert!(
                close_directive_turn(Arc::clone(&answer_context), &close_spy)
                    .await
                    .expect_err("terminal failure must propagate")
                    .to_string()
                    .contains("fixture terminal failure")
            );
            assert_eq!(close_spy.replies.lock().as_slice(), &[reply.as_ref(), reply.as_ref()]);
            close_spy.fail.store(false, Ordering::SeqCst);
            Arc::get_mut(&mut answer_context)
                .expect("exclusive context after close check")
                .directive = TurnExecutionDirective::Clarify {
                reason_code: "HIDDEN-CAP-9V".to_string(),
            };
            close_directive_turn(Arc::clone(&answer_context), &close_spy)
                .await
                .expect("clarification reaches terminal port");
            assert_eq!(close_spy.replies.lock().len(), 3);
            assert_eq!(
                close_spy.replies.lock().get(2).map(String::as_str),
                Some("Could you clarify the result you want?")
            );
            Arc::get_mut(&mut answer_context)
                .expect("exclusive context after close check")
                .directive = TurnExecutionDirective::Discover {
                families: Vec::new(),
                query: "fixture".to_string(),
                limit: 1,
            };
            assert!(
                close_directive_turn(Arc::clone(&answer_context), &close_spy)
                    .await
                    .is_err()
            );
            assert_eq!(close_spy.replies.lock().len(), 3, "unsupported directive cannot close");

            memory.return_empty.store(true, Ordering::SeqCst);
            let empty_observed = service
                .observe_selected_agent_memory(&proof_a, &decision)
                .await
                .expect("empty selected read still records a completed step");
            assert!(!empty_observed.fresh_read);
            assert!(empty_observed.selected_artifacts.is_empty(), "expected an empty value");
            let mut empty_comparison = empty_observed.state.clone();
            empty_comparison.derived = decision.state.derived.clone();
            let mut reduced = empty_comparison.trusted.parts().clone();
            reduced.budgets.latency_ms = 1;
            empty_comparison.trusted = TrustedState::from_runtime(reduced);
            assert_eq!(
                empty_comparison.evidence_fingerprint(&service.state_key),
                decision.state.evidence_fingerprint(&service.state_key),
                "an empty successful read stays no-progress as latency shrinks"
            );
            let empty_next = empty_observed.state;
            assert!(empty_next.observed.artifacts().is_empty(), "expected an empty value");
            assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 2);
            let empty_inputs = EngineInputs {
                state: &empty_next,
                decision_id: DecisionId::new_runtime(),
                allow_terminal_answer: empty_observed.fresh_read,
                ..inputs.clone()
            };
            let empty_terminal = CausalDecisionEngine::decide_terminal_prepared(
                CausalDecisionEngine::prepare_hop(&empty_inputs).expect("empty terminal preparation"),
            )
            .expect("empty terminal selection");
            assert!(matches!(
                directive_from_selection(
                    &empty_terminal,
                    &DirectiveContext {
                        current_generation: view.generation(),
                        approval_expires_at: chrono::Utc::now(),
                        discovery_limit: 1,
                    },
                ),
                Ok(TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. })
            ));
            assert!(
                empty_terminal.prediction.is_none(),
                "empty recovery makes no backend call"
            );

            memory.return_empty.store(false, Ordering::SeqCst);
            memory.return_stale.store(true, Ordering::SeqCst);
            let stale_observation = service
                .observe_selected_agent_memory(&proof_a, &decision)
                .await
                .expect("stale selected read remains auditable");
            assert!(!stale_observation.fresh_read);
            assert!(
                stale_observation.selected_artifacts.is_empty(),
                "stale rows cannot enter prompt context"
            );
            assert_eq!(stale_observation.state.observed.artifacts().len(), 1);
            assert!(
                stale_observation
                    .state
                    .observed
                    .artifacts()
                    .first()
                    .expect("one stale observation")
                    .age_ms
                    > STALE_ARTIFACT_MS
            );
            assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 3);

            memory.return_stale.store(false, Ordering::SeqCst);
            memory.return_error.store(true, Ordering::SeqCst);
            let prior_fresh = next
                .observed
                .artifacts()
                .first()
                .cloned()
                .expect("fresh prior artifact");
            decision.state.observed = ObservedState::from_runtime_records(Vec::new(), vec![prior_fresh], 0);
            let failed_observation = service
                .observe_selected_agent_memory(&proof_a, &decision)
                .await
                .expect("backend failure closes through an observed step");
            assert!(!failed_observation.fresh_read, "prior evidence cannot authorize Answer");
            assert!(
                failed_observation.selected_artifacts.is_empty(),
                "expected an empty value"
            );
            assert_eq!(failed_observation.state.observed.artifacts().len(), 1);
            let [failed_step] = failed_observation.state.observed.completed_steps() else {
                panic!("one failed runtime-owned read step");
            };
            assert_eq!(failed_step.status(), ObservedStepStatus::Failed);
            assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 4);
            let failed_inputs = EngineInputs {
                state: &failed_observation.state,
                decision_id: DecisionId::new_runtime(),
                allow_terminal_answer: failed_observation.fresh_read,
                ..inputs.clone()
            };
            let failed_terminal = CausalDecisionEngine::decide_terminal_prepared(
                CausalDecisionEngine::prepare_hop(&failed_inputs).expect("failed terminal preparation"),
            )
            .expect("failed terminal selection");
            assert!(matches!(
                directive_from_selection(
                    &failed_terminal,
                    &DirectiveContext {
                        current_generation: view.generation(),
                        approval_expires_at: chrono::Utc::now(),
                        discovery_limit: 1,
                    },
                ),
                Ok(TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. })
            ));
            assert!(
                failed_terminal.prediction.is_none(),
                "failed recovery makes no backend call"
            );

            memory.return_never.store(true, Ordering::SeqCst);
            decision.deadline = Instant::now() + Duration::from_secs(2);
            let cancelled = service
                .observe_selected_agent_memory(&proof_a, &decision)
                .await
                .expect("selected read timeout closes through an observed step");
            assert!(!cancelled.fresh_read);
            assert!(cancelled.selected_artifacts.is_empty(), "expected an empty value");
            let [cancelled_step] = cancelled.state.observed.completed_steps() else {
                panic!("one cancelled runtime-owned read step");
            };
            assert_eq!(cancelled_step.status(), ObservedStepStatus::Cancelled);
            assert_eq!(memory.retrieval_calls.load(Ordering::SeqCst), 5);
            let cancelled_inputs = EngineInputs {
                state: &cancelled.state,
                decision_id: DecisionId::new_runtime(),
                allow_terminal_answer: cancelled.fresh_read,
                deadline: decision.deadline,
                ..inputs.clone()
            };
            let cancelled_terminal = CausalDecisionEngine::decide_terminal_prepared(
                CausalDecisionEngine::prepare_hop(&cancelled_inputs).expect("cancelled terminal preparation"),
            )
            .expect("cancelled terminal selection");
            assert!(matches!(
                directive_from_selection(
                    &cancelled_terminal,
                    &DirectiveContext {
                        current_generation: view.generation(),
                        approval_expires_at: chrono::Utc::now(),
                        discovery_limit: 1,
                    },
                ),
                Ok(TurnExecutionDirective::Clarify { .. } | TurnExecutionDirective::Refuse { .. })
            ));
            assert!(cancelled_terminal.prediction.is_none());
        });
        ledger_probe::finish_service_fixture(&service, work).await
    }

    #[tokio::test]
    async fn builtin_agent_discovery_runs_a_bounded_dependent_hop() {
        struct FriendlierTool(Arc<AtomicUsize>);
        #[async_trait]
        impl Tool for FriendlierTool {
            fn name(&self) -> &str {
                "skills_list"
            }

            fn description(&self) -> &str {
                "Make text sound a little friendlier"
            }

            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object","properties":{}})
            }

            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
                self.0.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("discovery must not execute a tool")
            }
        }

        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.policy.extra_latency_budget_ms = 5_000;
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            )
            .expect("verified test model declaration"),
        );
        let generation = crate::config::new_shared(config.clone()).pin();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let calls = Arc::new(AtomicUsize::new(0));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(FriendlierTool(Arc::clone(&calls)))]);
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("real fixture SQLite"));
        let transports = CountingTransports(AtomicUsize::new(0));
        let service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .expect("SQLite-backed fixture service");
        let work = Box::pin(async {
            let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&memory),
                "could you make that sound a little friendlier please",
                "prx_run_discover",
                ("ollama", "prx-local-model"),
            )
            .await;
            assert!(
                service.model_can_call_tools(&proof),
                "declared model supports native tools"
            );
            let catalog = service.catalog.view(&CatalogScope::default());
            let discovered_entry = catalog
                .entries()
                .iter()
                .find(|entry| entry.public_name == "skills_list")
                .expect("read-only searchable fixture");
            assert_eq!(
                discovered_entry.effect,
                crate::causal_tree::branch::SideEffectClass::ReadOnly
            );
            assert!(matches!(discovered_entry.exposure, Exposure::Searchable));
            let context = service
                .decide_agent_initial(&proof, AgentTurnKind::Repl, &CancellationToken::new())
                .await
                .expect("bounded builtin decision");
            assert!(
                context.hop_ids().len() >= 2,
                "selected discovery should lead to a dependent hop: {:?}; candidates={:?}; gates={:?}",
                context.directive(),
                context.engine().candidates,
                context.engine().pre_commit.outcomes
            );
            assert!(
                context.hop_ids().len() <= usize::from(config.causal_tree.policy.max_decision_hops) + 1,
                "the configured hop cap bounds local discovery"
            );
            let prior = context
                .hop_ids()
                .iter()
                .rev()
                .nth(1)
                .copied()
                .expect("dependent hop has a predecessor");
            assert_eq!(
                context.engine().state.lineage.prior_decision_id(),
                Some(prior.as_uuid())
            );
            assert!(
                context.engine().state.observed.completed_steps().iter().any(|step| {
                    step.kind() == ObservedStepKind::Discover && step.status() == ObservedStepStatus::Ok
                }),
                "the dependent hop must carry the selected discovery observation"
            );
            assert!(matches!(
                context.disposition(),
                Ok(TurnDisposition::Close { .. } | TurnDisposition::Run)
            ));
            assert_eq!(
                transports.0.load(Ordering::SeqCst),
                0,
                "builtin discovery makes no network transport"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "selected discovery never executes a tool"
            );
        });
        ledger_probe::finish_service_fixture(&service, work).await
    }

    #[tokio::test]
    async fn selected_discovery_is_source_bound_and_never_dispatches_a_tool() {
        use crate::causal_tree::branch::{
            ApprovalRequirement, CandidateSpec, CapabilityRef, CapabilityTarget, CausalActionKind, CausalCandidate,
            SideEffectClass,
        };
        use crate::causal_tree::candidate_generator::CandidateSet;
        use crate::causal_tree::decision_select::Selection;
        use crate::causal_tree::gates::{GateOutcome, GateReport};
        use crate::causal_tree::state::{RawRoutingInput, TrustedState, fixtures};

        struct NoDispatchTool(Arc<AtomicUsize>);
        #[async_trait]
        impl Tool for NoDispatchTool {
            fn name(&self) -> &str {
                "prx_searchable"
            }

            fn description(&self) -> &str {
                "Searchable PRX fixture"
            }

            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object","properties":{}})
            }

            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
                self.0.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("selected discovery must not execute a tool")
            }
        }

        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config.clone()).pin();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let calls = Arc::new(AtomicUsize::new(0));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(NoDispatchTool(Arc::clone(&calls)))]);
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("real fixture SQLite"));
        let transports = CountingTransports(AtomicUsize::new(0));
        let service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .expect("SQLite-backed fixture service");
        let work = Box::pin(async {
            let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&memory),
                "prx_searchable",
                "prx_run_a",
                ("ollama", "prx-local-model"),
            )
            .await;
            let foreign = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
                &generation,
                Arc::clone(&memory),
                "prx_searchable",
                "prx_run_b",
                ("ollama", "prx-local-model"),
            )
            .await;
            let model = service.resolve_agent_model(&proof).expect("pinned model");
            let view = service.catalog.view(&CatalogScope::default());
            let entry = view
                .entries()
                .iter()
                .find(|entry| entry.public_name == "prx_searchable")
                .expect("searchable catalog entry");
            let reference = CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            };
            let mut trusted = fixtures::trusted();
            trusted.surface = TurnSurfaceKind::AgentLoop;
            trusted.channel = None;
            trusted.workspace_id = Arc::from(proof.workspace_id());
            trusted.owner_id = Some(Arc::from(proof.owner_id()));
            trusted.session_key = Some(Arc::from(proof.session_key()));
            trusted.run_id = Some(Arc::from(proof.run_id()));
            trusted.source_event_id = proof.source_event_id().map(Arc::from);
            trusted.catalog_generation = view.generation();
            trusted.policy_version = Arc::clone(&service.policy_version);
            trusted.resolved_model = Arc::from(format!("{}:{}", model.provider_label(), model.model()));
            let effective = effective_agent_exclusions(
                &config,
                &service.registry,
                &view,
                model.model(),
                &service.policy_version,
                &service.state_key,
            );
            trusted.exclusions = effective.names;
            trusted.model_admission = effective.model_admission;
            let mut state = fixtures::state(&service.state_key, "prx_searchable");
            state.trusted = TrustedState::from_runtime(trusted);
            state.routing_input = RawRoutingInput::new("prx_searchable".to_string());
            let candidate = CausalCandidate::new(
                CausalActionKind::Discover,
                CapabilityTarget::NativeTool,
                CandidateSpec {
                    namespace: None,
                    required_capabilities: vec![reference.clone()],
                    side_effect: SideEffectClass::ReadOnly,
                    approval: ApprovalRequirement::None,
                    estimated_cost: None,
                    estimated_latency_ms: None,
                },
            );
            let id = candidate.candidate_id().clone();
            let report = GateReport {
                outcomes: BTreeMap::from([(id.clone(), GateOutcome::Passed)]),
                gate_set_hash: [0; 32],
            };
            let mut decision = EngineDecision {
                decision_id: DecisionId::new_runtime(),
                deadline: Instant::now() + Duration::from_secs(5),
                elapsed_ms: 0,
                state,
                candidates: CandidateSet::Legal {
                    candidates: vec![candidate],
                    exact_mentions: BTreeSet::new(),
                    truncated: Vec::new(),
                    discoverable_families: Default::default(),
                    approval_scopes: BTreeMap::new(),
                    report: report.clone(),
                },
                selection: Selection::Commit {
                    candidate_id: id,
                    propensity: 1.0,
                },
                exploration_propensities: BTreeMap::new(),
                prediction: None,
                request_compiled: false,
                audit_trace: None,
                fallback_cause: None,
                scores: BTreeMap::new(),
                scored_count: 0,
                rehearsal: BTreeMap::new(),
                post_rehearsal: report.clone(),
                pre_commit: report,
                call_fingerprints: BTreeMap::new(),
                outgoing_child: None,
                outgoing_parent_allowed: None,
            };
            assert!(matches!(
                service.discover_selected_agent_refs(&foreign, &decision, &view),
                Err(AgentDiscoveryError::Origin(_))
            ));
            assert!(
                service
                    .discover_selected_agent_refs(&proof, &decision, &view)
                    .expect("selected local discovery")
                    .contains(&reference)
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0, "discovery is metadata-only");
            decision.deadline = Instant::now();
            assert!(matches!(
                service.discover_selected_agent_refs(&proof, &decision, &view),
                Err(AgentDiscoveryError::Selected(_))
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
        ledger_probe::finish_service_fixture(&service, work).await
    }

    #[test]
    fn enabled_agent_rejects_memory_without_message_events_before_key_or_store() {
        for backend in ["none", "markdown"] {
            let workspace = tempfile::tempdir().expect("test workspace");
            let mut config = crate::config::Config::default();
            config.workspace_dir = workspace.path().to_path_buf();
            config.memory.backend = backend.to_string();
            config.causal_tree.enabled = true;
            let generation = crate::config::new_shared(config.clone()).pin();
            let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
            let security = Arc::new(SecurityPolicy::default());
            let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
                Arc::clone(&registry),
                Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
                Arc::new(DenyApprovalStrategy),
                Arc::new(AdapterOwnedPreparation),
                Arc::new(TracingToolExecutionAudit),
            ));
            let memory: Arc<dyn Memory> = if backend == "none" {
                Arc::new(NoneMemory::new())
            } else {
                Arc::new(crate::memory::MarkdownMemory::new(workspace.path()))
            };
            let provider_catalog = ProviderCatalogView::from_config(&config);
            let transports = CountingTransports(AtomicUsize::new(0));
            let result = build_turn_decision_service(
                TurnSurfaceKind::AgentLoop,
                TurnServiceBuild {
                    parent_grant: None,
                    generation: Arc::clone(&generation),
                    registry,
                    execution,
                    catalog_sources: sources(&generation, &config),
                    surface_policy: SurfacePolicy::default(),
                    provider_catalog: &provider_catalog,
                    transports: &transports,
                    events: Arc::new(SilentEvents),
                    memory,
                    security,
                },
            );
            assert!(matches!(result, Err(TurnServiceBuildError::EventStoreUnsupported)));
            assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
            assert!(!workspace.path().join("self/causal_tree/decisions.db").exists());
        }
    }

    #[test]
    fn enabled_agent_rejects_disabled_user_event_recording_before_key_or_store() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = true;
        config.memory.events.record_user_messages = false;
        let generation = crate::config::new_shared(config.clone()).pin();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        let security = Arc::new(SecurityPolicy::default());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let result = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory")),
                security,
            },
        );
        assert!(matches!(result, Err(TurnServiceBuildError::InboundEventsDisabled)));
        assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
        assert!(!workspace.path().join("self/causal_tree/decisions.db").exists());
    }

    #[test]
    fn agent_hint_route_drift_is_refused_before_credential_resolution() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.causal_tree.enabled = false;
        config.workspace_dir = workspace.path().to_path_buf();
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "fast".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            api_key: None,
            credential_ref: None,
        });
        let generation = crate::config::new_shared(config.clone()).pin();
        let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "hint:fast");
        let matching = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config));
        assert!(matching.resolve_agent_model(&proof).is_ok());
        assert!(
            matching
                .validate_agent_execution_model(&proof, "ollama", "hint:fast")
                .is_ok()
        );
        assert!(
            matching
                .validate_agent_execution_model(&proof, "openai", "gpt-4o-mini")
                .is_ok()
        );
        for (provider, model) in [("ollama", "gpt-4o-mini"), ("openai", "hint:fast"), ("openai", "gpt-4o")] {
            assert!(matches!(
                matching.validate_agent_execution_model(&proof, provider, model),
                Err(TurnModelError::SelectionDrift)
            ));
        }

        let mut foreign = config.clone();
        foreign.model_routes.first_mut().expect("configured route").model = "gpt-4o".to_string();
        let stale = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&foreign));
        assert!(matches!(
            stale.resolve_agent_model(&proof),
            Err(TurnModelError::RouteDrift)
        ));
        assert!(matches!(
            stale.validate_agent_execution_model(&proof, "openai", "gpt-4o-mini"),
            Err(TurnModelError::RouteDrift)
        ));
        assert!(!stale.model_can_call_tools(&proof));

        let route = foreign.model_routes.first_mut().expect("configured route");
        route.model = "gpt-4o-mini".to_string();
        route.credential_ref = Some("cred-other".to_string());
        let wrong_owner = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&foreign));
        assert!(matches!(
            wrong_owner.resolve_agent_model(&proof),
            Err(TurnModelError::RouteDrift)
        ));
        assert!(!wrong_owner.model_can_call_tools(&proof));
    }

    #[test]
    fn verified_native_tools_requires_matching_source_selection() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.causal_tree.enabled = false;
        config.workspace_dir = workspace.path().to_path_buf();
        config
            .providers
            .models
            .push(crate::config::schema::ProviderModelConfig {
                provider: "ollama".to_string(),
                model: Some("prx-local-model".to_string()),
                operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
                native_tools: Some(true),
                vision_input: None,
                streaming: None,
                structured_output: None,
                tool_schema_dialect: None,
                tool_call_dialect: None,
                context_window: None,
            });
        let generation = crate::config::new_shared(config.clone()).pin();
        let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "prx-local-model");
        let service = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config));
        assert!(service.resolve_agent_model(&proof).is_ok());
        assert!(service.model_can_call_tools(&proof));

        let mut foreign = config;
        foreign
            .providers
            .models
            .first_mut()
            .expect("configured model")
            .native_tools = Some(false);
        let stale = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&foreign));
        assert!(!stale.model_can_call_tools(&proof));
    }

    #[test]
    fn disabled_service_keeps_workspace_key_absent_and_builtin_only() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.causal_tree.enabled = false;
        config.workspace_dir = workspace.path().to_path_buf();
        assert!(!config.causal_tree.enabled);
        let generation = crate::config::new_shared(config.clone()).pin();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy::default()))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let events: Arc<dyn CausalEventSink> = Arc::new(SilentEvents);
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let mut stale_sources = sources(&generation, &config);
        stale_sources.config_generation_id = "stale_generation".to_string();
        let stale = build_turn_decision_service(
            TurnSurfaceKind::Interactive,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry: Arc::clone(&registry),
                execution: Arc::clone(&execution),
                catalog_sources: stale_sources,
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::clone(&events),
                memory: Arc::new(NoneMemory::new()),
                security: Arc::new(SecurityPolicy::default()),
            },
        );
        assert!(matches!(stale, Err(TurnServiceBuildError::SourceGeneration)));
        assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
        let bundle = TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry: Arc::clone(&registry),
            execution: Arc::clone(&execution),
            catalog_sources: sources(&generation, &config),
            surface_policy: SurfacePolicy::default(),
            provider_catalog: &provider_catalog,
            transports: &transports,
            events: Arc::clone(&events),
            memory: Arc::new(NoneMemory::new()),
            security: Arc::new(SecurityPolicy::default()),
        };
        let first = build_turn_decision_service(TurnSurfaceKind::AgentLoop, bundle).expect("disabled service");
        assert!(!first.durable_key);
        assert!(!first.recordable);
        let status = first.status();
        assert!(!status.backend.enabled);
        assert_eq!(status.catalog_generation, *first.catalog.generation());
        assert_eq!(status.store_mode, StoreMode::ReadOnly);
        assert!(!status.durable_key);
        assert!(!status.recordable);
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        let proof = crate::agent::entry::test_cli_agent_turn_proof(&generation, "ollama", "prx-local-model");
        let resolved = first.resolve_agent_model(&proof).expect("pinned PMM model");
        assert_eq!(resolved.model(), "prx-local-model");
        assert!(
            !first.model_can_call_tools(&proof),
            "unverified tools grant no G7 authority"
        );
        let mut other_config = config.clone();
        other_config.default_model = Some("prx_other_model".to_string());
        let other_generation = crate::config::new_shared(other_config).pin();
        let stale_proof =
            crate::agent::entry::test_cli_agent_turn_proof(&other_generation, "ollama", "prx-local-model");
        assert!(matches!(
            first.resolve_agent_model(&stale_proof),
            Err(TurnModelError::Origin(_))
        ));
        assert_eq!(first.store.mode(), StoreMode::ReadOnly);
        assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
        let second = build_turn_decision_service(
            TurnSurfaceKind::Interactive,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events,
                memory: Arc::new(NoneMemory::new()),
                security: Arc::new(SecurityPolicy::default()),
            },
        )
        .expect("second disabled service");
        let parts: &[&[u8]] = &[b"prx_turn"];
        assert_ne!(
            first.state_key.hmac(KeyDomain::StateFingerprint, parts),
            second.state_key.hmac(KeyDomain::StateFingerprint, parts)
        );
    }

    #[test]
    fn disabled_service_uses_an_independent_read_only_view_when_a_writer_is_shared() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.causal_tree.enabled = false;
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config.clone()).pin();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy::default()))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let events: Arc<dyn CausalEventSink> = Arc::new(SilentEvents);
        let store = CausalDecisionStore::open_shared(
            workspace.path(),
            Retention::days(1, 1),
            Arc::clone(&events),
            StoreMode::ReadWrite,
        )
        .expect("writable fixture store");
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let result = build_turn_decision_service(
            TurnSurfaceKind::Interactive,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events,
                memory: Arc::new(NoneMemory::new()),
                security: Arc::new(SecurityPolicy::default()),
            },
        );
        let disabled = result.expect("disabled service keeps a separate read-only view");
        assert_eq!(store.mode(), StoreMode::ReadWrite);
        assert_eq!(disabled.store.mode(), StoreMode::ReadOnly);
        assert!(!disabled.recordable);
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
    }

    #[test]
    fn enabled_service_with_unavailable_key_does_not_open_a_writable_store() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let key_dir = workspace.path().join("self/causal_tree");
        std::fs::create_dir_all(&key_dir).expect("test key directory");
        std::fs::write(key_dir.join(".decision_key"), b"short").expect("test malformed key");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = true;
        let generation = crate::config::new_shared(config.clone()).pin();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy::default()))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let events: Arc<dyn CausalEventSink> = Arc::new(SilentEvents);
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events,
                memory: Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory")),
                security: Arc::new(SecurityPolicy::default()),
            },
        )
        .expect("builtin-only service");
        assert!(!service.durable_key);
        assert!(!service.recordable);
        assert_eq!(service.store.mode(), StoreMode::ReadOnly);
        let status = service.status();
        assert!(
            status.backend.enabled,
            "configured plane and writable attachment are distinct"
        );
        assert_eq!(status.catalog_generation, *service.catalog.generation());
        assert_eq!(status.store_mode, StoreMode::ReadOnly);
        assert!(!status.durable_key);
        assert!(!status.recordable);
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        assert!(!key_dir.join("decisions.db").exists());
    }

    #[tokio::test]
    async fn validated_usage_and_provider_request_id_reach_the_durable_decision() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = true;
        config.causal_tree.policy.extra_latency_budget_ms = 5_000;
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            )
            .expect("verified local model declaration"),
        );
        let generation = crate::config::new_shared(config.clone()).pin();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        let security = Arc::new(SecurityPolicy::default());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"));
        let mut service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .expect("recordable service");
        let request_ids = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let service_mut = Arc::get_mut(&mut service).expect("unshared service");
        service_mut.factory = DecisionBackendFactory::from_slots(
            vec![Box::new(UsageRecordProbeBackend {
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("prx-local-model"),
                },
                request_ids: Arc::clone(&request_ids),
                stall: false,
                entered: Arc::new(tokio::sync::Notify::new()),
            })],
            Arc::clone(&service_mut.state_key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
            &generation,
            memory,
            "summarize the neutral record",
            "prx_usage_run",
        )
        .await;
        let context = service
            .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
            .await
            .expect("scripted validated decision");
        let request_id = {
            let ids = request_ids.lock();
            assert_eq!(ids.len(), 1, "one sealed request for the single hop");
            ids.first().cloned().expect("one sealed request id")
        };
        assert_eq!(
            context
                .engine()
                .prediction
                .as_ref()
                .expect("validated prediction")
                .served
                .request_id(),
            request_id,
            "validated request id must equal the sealed manifest"
        );
        let writer = service.store.writer().expect("decision writer");
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let db = rusqlite::Connection::open_with_flags(
            workspace.path().join("self/causal_tree/decisions.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("read-only decision database");
        let (usage_json, provider_request_id): (Option<String>, Option<String>) = db
            .query_row(
                "SELECT usage_json, provider_request_id FROM decisions WHERE decision_id = ?1",
                [context.decision_id().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("durable decision row");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&usage_json.expect("reported usage")).expect("usage JSON"),
            serde_json::json!({"input_tokens":17,"output_tokens":3,"cost_micro_usd":42})
        );
        assert_eq!(provider_request_id.as_deref(), Some("prx_provider_request_1"));
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancellation_while_backend_pending_admits_no_decision_or_outcome() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = true;
        config.causal_tree.policy.extra_latency_budget_ms = 5_000;
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"prx-local-model\"\noperations = [\"chat\"]\nnative_tools = true\n",
            )
            .expect("verified local model declaration"),
        );
        let generation = crate::config::new_shared(config.clone()).pin();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        let security = Arc::new(SecurityPolicy::default());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"));
        let events = Arc::new(RecordingSink::default());
        let mut service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::clone(&events) as Arc<dyn CausalEventSink>,
                memory: Arc::clone(&memory),
                security,
            },
        )
        .expect("recordable service");
        let request_ids = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let entered = Arc::new(tokio::sync::Notify::new());
        let service_mut = Arc::get_mut(&mut service).expect("unshared service");
        service_mut.factory = DecisionBackendFactory::from_slots(
            vec![Box::new(UsageRecordProbeBackend {
                identity: BackendIdentity {
                    kind: BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("prx-local-model"),
                },
                request_ids: Arc::clone(&request_ids),
                stall: true,
                entered: Arc::clone(&entered),
            })],
            Arc::clone(&service_mut.state_key),
            OutboundSecrets::default(),
            SlotLimits::default(),
        );
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
            &generation,
            memory,
            "summarize the neutral record",
            "prx_cancel_run",
        )
        .await;
        let cancellation = CancellationToken::new();
        let decision = service.decide_agent_initial(&proof, AgentTurnKind::SingleShot, &cancellation);
        let cancel_when_entered = async {
            entered.notified().await;
            cancellation.cancel();
        };
        let ((), result) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(cancel_when_entered, decision)
        })
        .await
        .expect("pending backend reached and cancellation returned");
        assert!(matches!(result, Err(AgentDecisionError::Cancelled)));
        assert_eq!(request_ids.lock().len(), 1);
        let writer = service.store.writer().expect("decision writer");
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let counts = service.store.reader().counts().expect("durable counts");
        assert_eq!(counts.decisions, 0);
        assert_eq!(counts.outcomes, 0);
        assert!(
            events
                .events
                .lock()
                .iter()
                .all(|event| event.kind != CausalEventKind::OutcomeRecorded),
            "unadmitted cancellation cannot produce a success outcome"
        );
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn recorded_clarify_and_refuse_each_finalize_once_with_their_own_outcome() {
        use crate::causal_tree::branch::CausalActionKind;

        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = true;
        let generation = crate::config::new_shared(config.clone()).pin();
        let changed = Arc::new(AtomicBool::new(false));
        let dispatches = Arc::new(AtomicUsize::new(0));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(DriftingCatalogTool {
            changed: Arc::clone(&changed),
            dispatches: Arc::clone(&dispatches),
        })]);
        let security = Arc::new(SecurityPolicy::default());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"));
        let observer = Arc::new(RecordingSink::default());
        let service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: observer.clone(),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .expect("recordable drift service");
        changed.store(true, Ordering::SeqCst);
        let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.path().to_string_lossy());
        let mut applied_count = 0;
        for (input, run_id, kind, expected_action, expected_classification) in [
            (
                "  ",
                "prx_clarify_run",
                AgentTurnKind::Repl,
                CausalActionKind::Clarify,
                "clarification",
            ),
            (
                "prx test routing input",
                "prx_refuse_run",
                AgentTurnKind::SingleShot,
                CausalActionKind::Refuse,
                "refusal",
            ),
        ] {
            let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_input(
                &generation,
                Arc::clone(&memory),
                input,
                run_id,
            )
            .await;
            let mut context = service
                .decide_agent_initial(&proof, kind, &CancellationToken::new())
                .await
                .expect("catalog drift closes through a local terminal");
            assert_eq!(context.terminal_outcome_action(), Some(expected_action));
            let failure = ReceiptClosePorts {
                receipt: None,
                calls: AtomicUsize::new(0),
                cancellation: None,
            };
            let original_directive = context.directive.clone();
            Arc::get_mut(&mut context).expect("unshared Run fixture").directive =
                TurnExecutionDirective::AnswerDirectly;
            assert!(
                service
                    .close_agent_directive_turn(&proof, Arc::clone(&context), &failure, &CancellationToken::new())
                    .await
                    .is_err()
            );
            assert_eq!(
                failure.calls.load(Ordering::SeqCst),
                0,
                "Run never invokes a closing port"
            );
            Arc::get_mut(&mut context).expect("restore Close fixture").directive = original_directive;
            let original_service_id = context.service_id;
            Arc::get_mut(&mut context).expect("foreign service fixture").service_id = uuid::Uuid::new_v4();
            assert!(
                service
                    .close_agent_directive_turn(&proof, Arc::clone(&context), &failure, &CancellationToken::new())
                    .await
                    .is_err()
            );
            assert_eq!(failure.calls.load(Ordering::SeqCst), 0);
            Arc::get_mut(&mut context).expect("restore service fixture").service_id = original_service_id;
            let original_trusted = context.engine.state.trusted.clone();
            let mutations: [fn(&mut TrustedParts); 8] = [
                |parts| parts.owner_id = Some(Arc::from("prx_foreign_owner")),
                |parts| parts.session_key = Some(Arc::from("prx_foreign_session")),
                |parts| parts.run_id = Some(Arc::from("prx_foreign_run")),
                |parts| parts.source_event_id = Some(Arc::from("prx_foreign_source")),
                |parts| parts.workspace_id = Arc::from("prx_foreign_workspace"),
                |parts| parts.policy_version = Arc::from("prx_foreign_policy"),
                |parts| parts.catalog_generation = CatalogGeneration([0; 32]),
                |parts| parts.channel = Some(Arc::from("prx_foreign_channel")),
            ];
            for mutate in mutations {
                let mut parts = original_trusted.parts().clone();
                mutate(&mut parts);
                Arc::get_mut(&mut context)
                    .expect("unshared negative fixture")
                    .engine
                    .state
                    .trusted = TrustedState::from_runtime(parts);
                assert!(
                    service
                        .close_agent_directive_turn(&proof, Arc::clone(&context), &failure, &CancellationToken::new())
                        .await
                        .is_err()
                );
                assert_eq!(
                    failure.calls.load(Ordering::SeqCst),
                    0,
                    "scope mismatch must precede port invocation"
                );
            }
            Arc::get_mut(&mut context)
                .expect("restore exact context")
                .engine
                .state
                .trusted = original_trusted;
            let cancelled = CancellationToken::new();
            cancelled.cancel();
            assert!(
                service
                    .close_agent_directive_turn(&proof, Arc::clone(&context), &failure, &cancelled)
                    .await
                    .is_err()
            );
            assert_eq!(failure.calls.load(Ordering::SeqCst), 0);
            assert!(
                service
                    .close_agent_directive_turn(&proof, Arc::clone(&context), &failure, &CancellationToken::new())
                    .await
                    .is_err()
            );
            assert_eq!(failure.calls.load(Ordering::SeqCst), 1, "port errors propagate");

            let terminal_id = format!("{run_id}-terminal");
            let now = chrono::Utc::now();
            let commit = crate::agent::terminal::TurnTerminalDraft {
                decision: Some(
                    service
                        .settlement_ref(Arc::clone(&context))
                        .expect("recordable terminal link"),
                ),
                terminal_id: terminal_id.clone(),
                scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                    .with_owner_id(proof.owner_id())
                    .with_channel("cli")
                    .with_session_key(proof.session_key())
                    .with_run_id(proof.run_id()),
                status: crate::agent::terminal::TurnTerminalStatus::Completed,
                history: Some(crate::agent::terminal::TurnHistoryProjection {
                    assistant_content: "PRX local terminal response".to_owned(),
                    history_commit_len: 0,
                }),
                history_scope: None,
                provider_outcome: None,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: "local terminal close".to_owned(),
                    started_at: now,
                    finished_at: now,
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
            };
            let ports = FinalizingClosePorts {
                fabric: fabric.clone(),
                commit,
                cost: config.cost.clone(),
                workspace: workspace.path().to_path_buf(),
                calls: AtomicUsize::new(0),
                cancellation: None,
            };
            let cancelled_during_finalize = CancellationToken::new();
            let cancelled_ports = FinalizingClosePorts {
                fabric: ports.fabric.clone(),
                commit: ports.commit.clone(),
                cost: ports.cost.clone(),
                workspace: ports.workspace.clone(),
                calls: AtomicUsize::new(0),
                cancellation: Some(cancelled_during_finalize.clone()),
            };
            assert!(
                service
                    .close_agent_directive_turn(
                        &proof,
                        Arc::clone(&context),
                        &cancelled_ports,
                        &cancelled_during_finalize
                    )
                    .await
                    .is_err()
            );
            assert_eq!(cancelled_ports.calls.load(Ordering::SeqCst), 1);
            assert!(
                memory
                    .find_message_event_by_idempotency_key(proof.workspace_id(), &format!("turn:{terminal_id}:final"))
                    .await
                    .expect("committed cancelled-helper marker")
                    .is_some()
            );
            assert!(
                memory
                    .find_message_event_by_idempotency_key(
                        proof.workspace_id(),
                        &format!("causal:{}:directive-applied", context.decision_id())
                    )
                    .await
                    .expect("no application before valid retry")
                    .is_none()
            );
            let first = service
                .close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &CancellationToken::new())
                .await
                .expect("completed local terminal");
            let replay = service
                .close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &CancellationToken::new())
                .await
                .expect("idempotent local terminal replay");
            assert_eq!(
                ports.calls.load(Ordering::SeqCst),
                2,
                "idempotent finalizer receives each retry"
            );
            assert_eq!(first.terminal_event.id, replay.terminal_event.id);
            let concurrent_cancel_a = CancellationToken::new();
            let concurrent_cancel_b = CancellationToken::new();
            let (concurrent_a, concurrent_b) = tokio::join!(
                service.close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &concurrent_cancel_a),
                service.close_agent_directive_turn(&proof, Arc::clone(&context), &ports, &concurrent_cancel_b),
            );
            assert_eq!(
                concurrent_a.expect("concurrent retry").terminal_event.id,
                first.terminal_event.id
            );
            assert_eq!(
                concurrent_b.expect("concurrent retry").terminal_event.id,
                first.terminal_event.id
            );
            let mut forged = first.clone();
            forged.terminal_event.event_id = "prx_unpersisted_event".to_owned();
            for receipt in [forged, {
                let mut failed = first.clone();
                let mut marker: serde_json::Value =
                    serde_json::from_str(failed.terminal_event.raw_payload_json.as_deref().expect("marker"))
                        .expect("JSON");
                *marker.get_mut("status").expect("terminal marker status") = serde_json::json!("failed");
                failed.terminal_event.raw_payload_json = Some(marker.to_string());
                failed
            }] {
                let returned = ReceiptClosePorts {
                    receipt: Some(receipt),
                    calls: AtomicUsize::new(0),
                    cancellation: None,
                };
                assert!(
                    service
                        .close_agent_directive_turn(&proof, Arc::clone(&context), &returned, &CancellationToken::new())
                        .await
                        .is_err()
                );
            }
            let cancelled = CancellationToken::new();
            let returned = ReceiptClosePorts {
                receipt: Some(first.clone()),
                calls: AtomicUsize::new(0),
                cancellation: Some(cancelled.clone()),
            };
            assert!(
                service
                    .close_agent_directive_turn(&proof, Arc::clone(&context), &returned, &cancelled)
                    .await
                    .is_err()
            );
            assert_eq!(returned.calls.load(Ordering::SeqCst), 1);
            service.shutdown(Instant::now() + Duration::from_secs(5)).await;
            let applied = memory
                .find_message_event_by_idempotency_key(
                    proof.workspace_id(),
                    &format!("causal:{}:directive-applied", context.decision_id()),
                )
                .await
                .expect("read applied directive")
                .expect("successful close must publish an applied directive");
            assert_eq!(applied.event_type, "causal.directive.applied");
            applied_count += 1;
            assert_eq!(
                observer
                    .events
                    .lock()
                    .iter()
                    .filter(|event| event.kind == CausalEventKind::DirectiveApplied)
                    .count(),
                applied_count,
                "only durable insertion winners notify the observer"
            );
            assert_eq!(applied.owner_id.as_deref(), Some(proof.owner_id()));
            assert_eq!(applied.session_key.as_deref(), Some(proof.session_key()));
            assert_eq!(applied.run_id.as_deref(), Some(proof.run_id()));
            assert_eq!(
                applied.correlation_id.as_deref(),
                Some(context.decision_id().to_string().as_str())
            );

            assert_eq!(
                applied.causation_event_id.as_deref(),
                Some(first.terminal_event.event_id.as_str())
            );

            assert_eq!(
                first.assistant_event.as_ref().map(|event| event.id),
                replay.assistant_event.as_ref().map(|event| event.id)
            );
            let marker: serde_json::Value = serde_json::from_str(
                first
                    .terminal_event
                    .raw_payload_json
                    .as_deref()
                    .expect("terminal marker"),
            )
            .expect("terminal marker JSON");
            assert_eq!(
                marker.get("status").and_then(serde_json::Value::as_str),
                Some("completed")
            );
            assert_eq!(
                marker.get("decision_id").and_then(serde_json::Value::as_str),
                Some(context.decision_id().to_string().as_str())
            );
            let writer = service.store.writer().expect("decision writer");
            assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
            let scope = DecisionScope::Owner {
                workspace_id: service.store.workspace_id().expect("store workspace"),
                owner_id: Arc::from(proof.owner_id()),
            };
            let row = service
                .store
                .reader()
                .get(&scope, &context.decision_id())
                .expect("read local terminal decision")
                .expect("local terminal decision row");
            assert_eq!(row.outcomes.len(), 1);
            assert_eq!(
                row.outcomes.first().expect("one terminal outcome").classification,
                expected_classification
            );
            assert_eq!(
                row.outcomes.first().expect("one terminal outcome").settlement_id,
                terminal_id
            );
        }
        assert_eq!(transports.0.load(Ordering::SeqCst), 0);
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn enabled_agent_service_records_only_a_successful_decision_chain() {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.causal_tree.enabled = true;
        config.causal_tree.policy.extra_latency_budget_ms = 5_000;
        config.cost.prices.insert(
            "prx_test/model".to_owned(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 0.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let generation = crate::config::new_shared(config.clone()).pin();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        let security = Arc::new(SecurityPolicy::default());
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let provider_catalog = ProviderCatalogView::from_config(&config);
        let transports = CountingTransports(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite memory"));
        let service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry,
                execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&memory),
                security,
            },
        )
        .expect("recordable service");
        assert!(service.durable_key && service.recordable);
        let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof(&generation, Arc::clone(&memory)).await;
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            service
                .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &cancelled)
                .await,
            Err(AgentDecisionError::Cancelled)
        ));
        assert_eq!(service.store.reader().counts().expect("counts").decisions, 0);

        let mut context = service
            .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
            .await
            .expect("successful local decision");
        {
            let mutable = Arc::get_mut(&mut context).expect("exclusive returned context");
            assert!(mutable.record_admitted.swap(false, Ordering::AcqRel));
        }
        assert!(
            service.settlement_ref(Arc::clone(&context)).is_err(),
            "unadmitted context cannot be linked"
        );
        {
            let mutable = Arc::get_mut(&mut context).expect("exclusive returned context");
            mutable.record_admitted.store(true, Ordering::Release);
            mutable.service_id = uuid::Uuid::nil();
        }
        assert!(
            service.settlement_ref(Arc::clone(&context)).is_err(),
            "a foreign service context cannot be linked"
        );
        Arc::get_mut(&mut context)
            .expect("exclusive returned context")
            .service_id = service.service_id;
        let writer = service.store.writer().expect("writable decision store");
        for n in 1_u8..=5 {
            let decision_id = DecisionId::new_runtime();
            let mut row = crate::causal_tree::store::testing::record(
                decision_id,
                proof.workspace_id(),
                Some(proof.owner_id()),
                chrono::Utc::now().timestamp_millis() + i64::from(n),
            );
            row.catalog_generation = context.catalog_generation().to_hex();
            assert_eq!(writer.record_decision(row), crate::causal_tree::store::Enqueue::Queued);
            assert!(
                writer.drain(Instant::now() + Duration::from_secs(5)).await,
                "decision row must commit before its synthetic outcome"
            );
            crate::causal_tree::store::testing::write_single_call_latency(
                workspace.path(),
                decision_id,
                &format!("prx_latency_{n}"),
                "native_tool:prx_project:read",
                u32::from(n) * 10,
            );
        }
        assert_eq!(
            service
                .observed_capability_latency(proof.owner_id(), context.catalog_generation())
                .get("native_tool:prx_project:read"),
            Some(&30),
            "the service must use the store's stable workspace ID"
        );
        let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), proof.workspace_id());
        let now = chrono::Utc::now();
        let terminal = crate::agent::terminal::finalize_turn(
            &fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: None,
                terminal_id: "unlinked-terminal-1".to_owned(),
                scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                    .with_owner_id(proof.owner_id())
                    .with_channel("cli")
                    .with_session_key(proof.session_key())
                    .with_run_id(proof.run_id()),
                status: crate::agent::terminal::TurnTerminalStatus::Failed,
                history: None,
                history_scope: None,
                provider_outcome: None,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: "local failure".to_owned(),
                    started_at: now,
                    finished_at: now,
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
            },
            &config.cost,
            workspace.path(),
        )
        .await
        .expect_err("missing terminal decision must fail before persistence");
        assert_eq!(terminal.to_string(), "terminal draft has no service-issued decision");
        assert!(
            memory
                .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:unlinked-terminal-1:final")
                .await
                .expect("unlinked terminal marker lookup")
                .is_none(),
            "an unlinked terminal must not persist a marker"
        );
        let linked_commit = crate::agent::terminal::TurnTerminalDraft {
            decision: Some(
                service
                    .settlement_ref(Arc::clone(&context))
                    .expect("recordable service issues the terminal link"),
            ),
            terminal_id: "linked-terminal-1".to_owned(),
            scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                .with_owner_id(proof.owner_id())
                .with_channel("cli")
                .with_session_key(proof.session_key())
                .with_run_id(proof.run_id()),
            status: crate::agent::terminal::TurnTerminalStatus::Failed,
            history: None,
            history_scope: None,
            provider_outcome: None,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "local failure".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        };
        let mut foreign = linked_commit.clone();
        foreign.terminal_id = "foreign-terminal-1".to_owned();
        foreign.scope.owner_id = Some("foreign_owner".to_owned());
        assert!(
            crate::agent::terminal::finalize_turn(&fabric, foreign, &config.cost, workspace.path())
                .await
                .is_err(),
            "a foreign terminal scope must fail before marker write"
        );
        assert!(
            memory
                .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:foreign-terminal-1:final")
                .await
                .expect("read foreign marker")
                .is_none()
        );
        let mut wrong_workspace = linked_commit.clone();
        wrong_workspace.terminal_id = "wrong-workspace-terminal-1".to_owned();
        assert!(
            crate::agent::terminal::finalize_turn(
                &fabric,
                wrong_workspace,
                &config.cost,
                workspace.path().join("other").as_path(),
            )
            .await
            .is_err(),
            "the cost ledger workspace must match before a terminal write"
        );
        assert!(
            memory
                .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:wrong-workspace-terminal-1:final")
                .await
                .expect("read workspace marker")
                .is_none()
        );
        let mut wrong_cost = config.cost.clone();
        wrong_cost.daily_limit_usd += 1.0;
        let mut wrong_tariff = linked_commit.clone();
        wrong_tariff.terminal_id = "wrong-cost-terminal-1".to_owned();
        assert!(
            crate::agent::terminal::finalize_turn(&fabric, wrong_tariff, &wrong_cost, workspace.path())
                .await
                .is_err(),
            "the cost configuration must match before a terminal write"
        );
        assert!(
            memory
                .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:wrong-cost-terminal-1:final")
                .await
                .expect("read cost marker")
                .is_none()
        );
        let linked =
            crate::agent::terminal::finalize_turn(&fabric, linked_commit.clone(), &config.cost, workspace.path())
                .await
                .expect("linked terminal after marker");
        let marker: serde_json::Value = serde_json::from_str(
            linked
                .terminal_event
                .raw_payload_json
                .as_deref()
                .expect("persisted marker payload"),
        )
        .expect("terminal marker JSON");
        assert_eq!(
            marker.get("decision_id").and_then(serde_json::Value::as_str),
            Some(context.decision_id().to_string().as_str())
        );
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let replay =
            crate::agent::terminal::finalize_turn(&fabric, linked_commit.clone(), &config.cost, workspace.path())
                .await
                .expect("same terminal replay");
        assert_eq!(replay.terminal_event.id, linked.terminal_event.id);
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let linked_scope = crate::causal_tree::store::DecisionScope::Owner {
            workspace_id: service.store.workspace_id().expect("store scope"),
            owner_id: Arc::from(proof.owner_id()),
        };
        let linked_row = service
            .store
            .reader()
            .get(&linked_scope, &context.decision_id())
            .expect("read linked decision")
            .expect("linked decision row");
        assert_eq!(linked_row.outcomes.len(), 1, "same terminal replay is idempotent");
        let outcome = linked_row.outcomes.first().expect("one linked outcome");
        assert_eq!(outcome.settlement_id, "linked-terminal-1");
        assert_eq!(outcome.classification, "failed");
        let conflicting_context = service
            .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
            .await
            .expect("second local decision");
        let mut conflicting_commit = linked_commit.clone();
        conflicting_commit.decision = Some(
            service
                .settlement_ref(Arc::clone(&conflicting_context))
                .expect("second decision is recordable"),
        );
        assert!(
            crate::agent::terminal::finalize_turn(&fabric, conflicting_commit, &config.cost, workspace.path())
                .await
                .is_err(),
            "a terminal ID cannot be reassigned to another decision"
        );
        assert_eq!(
            memory
                .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:linked-terminal-1:final")
                .await
                .expect("read retained marker")
                .expect("retained marker")
                .id,
            linked.terminal_event.id
        );
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let conflicting_scope = crate::causal_tree::store::DecisionScope::Owner {
            workspace_id: service.store.workspace_id().expect("store scope"),
            owner_id: Arc::from(proof.owner_id()),
        };
        assert!(
            service
                .store
                .reader()
                .get(&conflicting_scope, &conflicting_context.decision_id())
                .expect("read conflicting decision")
                .expect("conflicting decision row")
                .outcomes
                .is_empty(),
            "a conflicting replay must not write an outcome"
        );
        let mut concurrent_a = linked_commit.clone();
        concurrent_a.terminal_id = "concurrent-terminal-1".to_owned();
        let mut concurrent_b = concurrent_a.clone();
        concurrent_b.decision = Some(
            service
                .settlement_ref(Arc::clone(&conflicting_context))
                .expect("second concurrent decision link"),
        );
        let priced_outcome =
            |decision_id: DecisionId, prompt_tokens: u32| crate::llm::route_decision::ProviderExecutionOutcome {
                decision_id: decision_id.to_string(),
                started_at: now,
                finished_at: now,
                attempts: Vec::new(),
                final_provider: "prx_test".to_owned(),
                final_model: "model".to_owned(),
                status: crate::llm::route_decision::ExecutionStatus::AllFailed {
                    last_error_class: "local_failure".to_owned(),
                },
                fallback_reason: None,
                tokens_used: crate::llm::route_decision::TokenUsage {
                    prompt_tokens: Some(prompt_tokens),
                    total_tokens: Some(prompt_tokens),
                    ..Default::default()
                },
            };
        concurrent_a.provider_outcome = Some(priced_outcome(context.decision_id(), 10));
        concurrent_b.provider_outcome = Some(priced_outcome(conflicting_context.decision_id(), 20));
        let (first, second) = tokio::join!(
            crate::agent::terminal::finalize_turn(&fabric, concurrent_a, &config.cost, workspace.path()),
            crate::agent::terminal::finalize_turn(&fabric, concurrent_b, &config.cost, workspace.path())
        );
        assert_ne!(
            first.is_ok(),
            second.is_ok(),
            "exactly one decision must claim the terminal ID"
        );
        let winner = first
            .as_ref()
            .ok()
            .or_else(|| second.as_ref().ok())
            .expect("one terminal winner");
        let claim = memory
            .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:concurrent-terminal-1:decision-claim")
            .await
            .expect("read terminal claim")
            .expect("terminal claim exists");
        let claimed: serde_json::Value =
            serde_json::from_str(claim.raw_payload_json.as_deref().expect("claim payload")).expect("claim JSON");
        let winner_marker: serde_json::Value = serde_json::from_str(
            winner
                .terminal_event
                .raw_payload_json
                .as_deref()
                .expect("winner marker payload"),
        )
        .expect("winner marker JSON");
        assert_eq!(claimed.get("decision_id"), winner_marker.get("decision_id"));
        let winner_tokens = winner_marker
            .pointer("/usage_settlement/prompt_tokens")
            .and_then(serde_json::Value::as_u64)
            .expect("winner metered tokens");
        let ledger =
            std::fs::read_to_string(workspace.path().join("state/costs.jsonl")).expect("persisted cost ledger");
        let concurrent_costs: Vec<serde_json::Value> = ledger
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|row| row.get("settlement_id").and_then(serde_json::Value::as_str) == Some("concurrent-terminal-1"))
            .collect();
        assert_eq!(concurrent_costs.len(), 1, "the loser must not write a cost settlement");
        assert_eq!(
            concurrent_costs
                .first()
                .expect("one concurrent cost settlement")
                .pointer("/usage/input_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(winner_tokens),
            "the cost ledger must belong to the claimed winner"
        );
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let concurrent_count = [context.decision_id(), conflicting_context.decision_id()]
            .into_iter()
            .map(|id| {
                service
                    .store
                    .reader()
                    .get(&conflicting_scope, &id)
                    .expect("read concurrent decision")
                    .expect("concurrent decision row")
                    .outcomes
                    .iter()
                    .filter(|outcome| outcome.settlement_id == "concurrent-terminal-1")
                    .count()
            })
            .sum::<usize>();
        assert_eq!(concurrent_count, 1, "the losing decision cannot settle an outcome");
        let mut divergent_a = linked_commit.clone();
        divergent_a.terminal_id = "divergent-terminal-1".to_owned();
        divergent_a.provider_outcome = Some(priced_outcome(context.decision_id(), 30));
        let mut divergent_b = divergent_a.clone();
        divergent_b.provider_outcome = Some(priced_outcome(context.decision_id(), 40));
        let (first, second) = tokio::join!(
            crate::agent::terminal::finalize_turn(&fabric, divergent_a, &config.cost, workspace.path()),
            crate::agent::terminal::finalize_turn(&fabric, divergent_b, &config.cost, workspace.path())
        );
        assert_ne!(
            first.is_ok(),
            second.is_ok(),
            "divergent facts cannot share a decision claim"
        );
        let winner = first
            .as_ref()
            .ok()
            .or_else(|| second.as_ref().ok())
            .expect("one semantic winner");
        let winner_marker: serde_json::Value = serde_json::from_str(
            winner
                .terminal_event
                .raw_payload_json
                .as_deref()
                .expect("semantic winner marker"),
        )
        .expect("semantic winner JSON");
        let winner_tokens = winner_marker
            .pointer("/usage_settlement/prompt_tokens")
            .and_then(serde_json::Value::as_u64)
            .expect("semantic winner tokens");
        let ledger = std::fs::read_to_string(workspace.path().join("state/costs.jsonl")).expect("semantic cost ledger");
        let divergent_costs: Vec<serde_json::Value> = ledger
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|row| row.get("settlement_id").and_then(serde_json::Value::as_str) == Some("divergent-terminal-1"))
            .collect();
        assert_eq!(divergent_costs.len(), 1);
        assert_eq!(
            divergent_costs
                .first()
                .expect("one semantic cost settlement")
                .pointer("/usage/input_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(winner_tokens),
            "only the semantic claim winner can write cost"
        );
        let mut scope_commit = linked_commit.clone();
        scope_commit.terminal_id = "scope-replay-terminal-1".to_owned();
        scope_commit.provider_outcome = Some(priced_outcome(context.decision_id(), 50));
        let scope_marker =
            crate::agent::terminal::finalize_turn(&fabric, scope_commit.clone(), &config.cost, workspace.path())
                .await
                .expect("claimed terminal before scope replay");
        let mut changed_scope = scope_commit;
        changed_scope.scope.recipient = Some("prx_other_recipient".to_owned());
        assert!(
            crate::agent::terminal::finalize_turn(&fabric, changed_scope, &config.cost, workspace.path())
                .await
                .is_err(),
            "same decision with changed persisted scope must fail before cost"
        );
        assert_eq!(
            memory
                .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:scope-replay-terminal-1:final")
                .await
                .expect("read scope replay marker")
                .expect("retained scope marker")
                .id,
            scope_marker.terminal_event.id
        );
        let scope_ledger =
            std::fs::read_to_string(workspace.path().join("state/costs.jsonl")).expect("scope replay cost ledger");
        assert_eq!(
            scope_ledger
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|row| {
                    row.get("settlement_id").and_then(serde_json::Value::as_str) == Some("scope-replay-terminal-1")
                })
                .count(),
            1,
            "changed scope cannot add a cost settlement"
        );
        let rejoin_context = service
            .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
            .await
            .expect("decision before terminal-only crash point");
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let rejoin_marker = crate::agent::terminal::finalize_turn_stop_after_marker(
            &fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: Some(
                    service
                        .settlement_ref(Arc::clone(&rejoin_context))
                        .expect("recordable link"),
                ),
                terminal_id: "rejoin-terminal-1".to_owned(),
                scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                    .with_owner_id(proof.owner_id())
                    .with_channel("cli")
                    .with_session_key(proof.session_key())
                    .with_run_id(proof.run_id()),
                status: crate::agent::terminal::TurnTerminalStatus::Failed,
                history: None,
                history_scope: None,
                provider_outcome: None,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: "local failure".to_owned(),
                    started_at: now,
                    finished_at: now,
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
            },
            &config.cost,
            workspace.path(),
        )
        .await
        .expect("real finalizer marker before outcome admission");
        let rejoin_scope = crate::causal_tree::store::DecisionScope::Owner {
            workspace_id: service.store.workspace_id().expect("store scope"),
            owner_id: Arc::from(proof.owner_id()),
        };
        assert!(
            service
                .store
                .reader()
                .get(&rejoin_scope, &rejoin_context.decision_id())
                .expect("read pending decision")
                .expect("pending decision row")
                .outcomes
                .is_empty(),
            "expected an empty value"
        );
        let late_context = service
            .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
            .await
            .expect("decision for delayed terminal task");
        let late_commit = crate::agent::terminal::TurnTerminalDraft {
            decision: Some(
                service
                    .settlement_ref(Arc::clone(&late_context))
                    .expect("delayed decision link"),
            ),
            terminal_id: "late-async-terminal-1".to_owned(),
            scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                .with_owner_id(proof.owner_id())
                .with_channel("cli")
                .with_session_key(proof.session_key())
                .with_run_id(proof.run_id()),
            status: crate::agent::terminal::TurnTerminalStatus::Failed,
            history: None,
            history_scope: None,
            provider_outcome: None,
            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                summary: "delayed local failure".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
        };
        let (release_late, await_late) = tokio::sync::oneshot::channel::<()>();
        let late_fabric = fabric.clone();
        let late_cost = config.cost.clone();
        let late_workspace = workspace.path().to_path_buf();
        let late_task = tokio::spawn(async move {
            await_late.await.expect("release delayed terminal task");
            crate::agent::terminal::finalize_turn(&late_fabric, late_commit, &late_cost, &late_workspace).await
        });
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        assert!(
            service
                .store
                .reader()
                .get(&rejoin_scope, &late_context.decision_id())
                .expect("read delayed decision")
                .expect("delayed decision row")
                .outcomes
                .is_empty(),
            "the delayed task cannot settle before its terminal marker"
        );
        release_late.send(()).expect("release delayed terminal task");
        let late_receipt = late_task
            .await
            .expect("join delayed terminal task")
            .expect("late terminal join");
        assert_eq!(late_receipt.terminal_event.event_type, "turn.finalized");
        assert!(writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let late_row = service
            .store
            .reader()
            .get(&rejoin_scope, &late_context.decision_id())
            .expect("read settled delayed decision")
            .expect("settled delayed decision row");
        assert_eq!(late_row.outcomes.len(), 1);
        assert_eq!(
            late_row.outcomes.first().expect("one late outcome").settlement_id,
            "late-async-terminal-1"
        );
        assert_eq!(
            late_row.outcomes.first().expect("one late outcome").classification,
            "failed"
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        assert_eq!(
            service.store.reader().counts().expect("counts").decisions,
            i64::try_from(
                context.hop_ids().len()
                    + conflicting_context.hop_ids().len()
                    + rejoin_context.hop_ids().len()
                    + late_context.hop_ids().len()
            )
            .expect("bounded hop count")
                + 5
        );
        let scope = crate::causal_tree::store::DecisionScope::Owner {
            workspace_id: service.store.workspace_id().expect("store scope"),
            owner_id: Arc::from(proof.owner_id()),
        };
        for (index, hop) in context.audit_hops().enumerate() {
            let row = service
                .store
                .reader()
                .get(&scope, &hop.decision_id)
                .expect("read")
                .expect("recorded hop");
            assert_eq!(row.decision_id, hop.decision_id);
            assert_eq!(
                row.prior_decision_id,
                index
                    .checked_sub(1)
                    .and_then(|prior| context.hop_ids().get(prior))
                    .copied()
            );
            assert_eq!(row.run_id.as_deref(), Some(proof.run_id()));
            assert!(row.enabled_snapshot);
            assert!(
                row.selected_candidate_id
                    .as_ref()
                    .is_some_and(|selected| { row.candidates.iter().any(|candidate| &candidate.id == selected) })
            );
        }
        assert_eq!(transports.0.load(Ordering::SeqCst), 0, "no external transport");
        let old_store = Arc::clone(&service.store);
        drop(linked_commit);
        drop(writer);
        drop(service);
        CausalDecisionStore::close(old_store, Instant::now() + Duration::from_secs(5)).await;
        let reopened_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        let reopened_security = Arc::new(SecurityPolicy::default());
        let reopened_execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&reopened_registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&reopened_security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let reopened = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&generation),
                registry: reopened_registry,
                execution: reopened_execution,
                catalog_sources: sources(&generation, &config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::clone(&memory),
                security: reopened_security,
            },
        )
        .expect("reopened recordable service");
        // Reconstructed services read their own committed source marker.
        // There is no caller backend or workspace parameter to substitute.
        assert_eq!(
            reopened
                .rejoin_terminal("rejoin-terminal-1", proof.owner_id())
                .await
                .expect("rejoin reads the service-owned persisted marker"),
            Enqueue::Queued
        );
        assert!(
            reopened
                .rejoin_terminal("rejoin-terminal-1", "foreign_owner")
                .await
                .is_err(),
            "a foreign owner cannot rejoin the outcome"
        );
        for (terminal, owner) in [
            ("prx_missing_terminal", proof.owner_id()),
            ("", proof.owner_id()),
            ("rejoin-terminal-1", ""),
        ] {
            assert!(reopened.rejoin_terminal(terminal, owner).await.is_err());
        }
        let foreign_workspace = tempfile::tempdir().expect("foreign workspace");
        let mut foreign_config = config.clone();
        foreign_config.workspace_dir = foreign_workspace.path().to_path_buf();
        let foreign_generation = crate::config::new_shared(foreign_config.clone()).pin();
        let foreign_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
        let foreign_security = Arc::new(SecurityPolicy::default());
        let foreign_execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&foreign_registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(&foreign_security))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        ));
        let foreign_service = build_turn_decision_service(
            TurnSurfaceKind::AgentLoop,
            TurnServiceBuild {
                parent_grant: None,
                generation: Arc::clone(&foreign_generation),
                registry: foreign_registry,
                execution: foreign_execution,
                catalog_sources: sources(&foreign_generation, &foreign_config),
                surface_policy: SurfacePolicy::default(),
                provider_catalog: &provider_catalog,
                transports: &transports,
                events: Arc::new(SilentEvents),
                memory: Arc::new(crate::memory::SqliteMemory::new(foreign_workspace.path()).expect("foreign SQLite")),
                security: foreign_security,
            },
        )
        .expect("foreign recordable service");
        assert!(
            foreign_service
                .rejoin_terminal("rejoin-terminal-1", proof.owner_id())
                .await
                .is_err(),
            "a different workspace cannot read or settle this service's marker"
        );
        foreign_service.shutdown(Instant::now() + Duration::from_secs(5)).await;
        for _ in 0..2 {
            assert_eq!(
                reopened
                    .rejoin_terminal("rejoin-terminal-1", proof.owner_id())
                    .await
                    .expect("rejoin from persisted marker and decision"),
                Enqueue::Queued
            );
        }
        let reopened_writer = reopened.store.writer().expect("reopened writer");
        assert!(reopened_writer.drain(Instant::now() + Duration::from_secs(5)).await);
        assert_eq!(
            reopened
                .store
                .reader()
                .get(&rejoin_scope, &rejoin_context.decision_id())
                .expect("read rejoined decision")
                .expect("rejoined decision row")
                .outcomes
                .len(),
            1
        );
        assert_eq!(
            memory
                .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:rejoin-terminal-1:final")
                .await
                .expect("read retained rejoin marker")
                .expect("retained rejoin marker")
                .id,
            rejoin_marker.terminal_event.id
        );
        let completed_context = reopened
            .decide_agent_initial(&proof, AgentTurnKind::SingleShot, &CancellationToken::new())
            .await
            .expect("completed decision after service reopen");
        let (expected_classification, completed_outcome) = match completed_context.terminal_outcome_action() {
            Some(crate::causal_tree::branch::CausalActionKind::Clarify) => ("clarification", None),
            Some(crate::causal_tree::branch::CausalActionKind::Refuse) => ("refusal", None),
            Some(crate::causal_tree::branch::CausalActionKind::Answer) => (
                "success",
                Some(crate::llm::route_decision::ProviderExecutionOutcome {
                    decision_id: completed_context.decision_id().to_string(),
                    started_at: now,
                    finished_at: now,
                    attempts: Vec::new(),
                    final_provider: "prx_test".to_owned(),
                    final_model: "prx_test".to_owned(),
                    status: crate::llm::route_decision::ExecutionStatus::Success,
                    fallback_reason: None,
                    tokens_used: Default::default(),
                }),
            ),
            _ => panic!("the selected terminal action is unsupported by CDP-23"),
        };
        assert!(reopened_writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let completed_marker = crate::agent::terminal::finalize_turn_stop_after_marker(
            &fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: Some(
                    reopened
                        .settlement_ref(Arc::clone(&completed_context))
                        .expect("completed link"),
                ),
                terminal_id: "completed-rejoin-terminal-1".to_owned(),
                scope: crate::memory::MessageEventScope::new("agent", crate::memory::MemoryVisibility::Session)
                    .with_owner_id(proof.owner_id())
                    .with_channel("cli")
                    .with_session_key(proof.session_key())
                    .with_run_id(proof.run_id()),
                status: crate::agent::terminal::TurnTerminalStatus::Completed,
                history: None,
                history_scope: None,
                provider_outcome: completed_outcome,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: "local completion".to_owned(),
                    started_at: now,
                    finished_at: now,
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
            },
            &config.cost,
            workspace.path(),
        )
        .await
        .expect("completed marker before outcome admission");
        assert_eq!(
            reopened
                .rejoin_terminal("completed-rejoin-terminal-1", proof.owner_id())
                .await
                .expect("completed rejoin from persisted selected action"),
            Enqueue::Queued
        );
        assert!(reopened_writer.drain(Instant::now() + Duration::from_secs(5)).await);
        let completed_row = reopened
            .store
            .reader()
            .get(&rejoin_scope, &completed_context.decision_id())
            .expect("read completed decision")
            .expect("completed decision row");
        assert_eq!(completed_row.outcomes.len(), 1);
        assert_eq!(
            completed_row
                .outcomes
                .first()
                .expect("one completed outcome")
                .classification,
            expected_classification
        );
        assert_eq!(
            memory
                .find_message_event_by_idempotency_key(proof.workspace_id(), "turn:completed-rejoin-terminal-1:final")
                .await
                .expect("read completed marker")
                .expect("completed marker")
                .id,
            completed_marker.terminal_event.id
        );
    }

    #[cfg(unix)]
    #[test]
    fn disabled_service_ignores_an_unsafe_store_path_without_touching_it() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("test workspace");
        let outside = tempfile::tempdir().expect("test outside directory");
        std::fs::create_dir(workspace.path().join("self")).expect("test self directory");
        symlink(outside.path(), workspace.path().join("self/causal_tree")).expect("test unsafe store path");
        let mut config = crate::config::Config::default();
        config.causal_tree.enabled = false;
        config.workspace_dir = workspace.path().to_path_buf();
        let generation = crate::config::new_shared(config.clone()).pin();
        let service = disabled_agent_service(&generation, &ProviderCatalogView::from_config(&config));
        assert_eq!(service.store.mode(), StoreMode::ReadOnly);
        assert!(!service.recordable);
        assert!(
            std::fs::read_dir(outside.path())
                .expect("outside directory remains readable")
                .next()
                .is_none()
        );
    }
}

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
mod selected_custom_memory_scope_tests {
    use super::selected_custom_memory_read_scope;
    use crate::memory::Memory;
    use crate::memory::principal::MemoryWriteContext;

    #[test]
    fn source_owner_read_scope_is_stricter_and_rejects_conflicting_identity() {
        let write = MemoryWriteContext {
            workspace_id: Some("prx_workspace".into()),
            chat_type: Some("private".into()),
            chat_id: Some("prx_session".into()),
            ..Default::default()
        };
        let read = selected_custom_memory_read_scope(&write, "prx_workspace", "prx_owner").unwrap();
        assert_eq!(read.owner_id.as_deref(), Some("prx_owner"));
        assert!(read.require_owner_match);
        assert_eq!(read.chat_id, write.chat_id);
        assert_eq!(read.chat_type, write.chat_type);
        assert!(write.owner_id.is_none());
        assert!(!write.require_owner_match);
        assert!(selected_custom_memory_read_scope(&write, "prx_other_workspace", "prx_owner").is_err());
        let mut conflicting = write.clone();
        conflicting.owner_id = Some("prx_foreign_owner".into());
        assert!(selected_custom_memory_read_scope(&conflicting, "prx_workspace", "prx_owner").is_err());
        assert!(selected_custom_memory_read_scope(&write, "prx_workspace", "").is_err());
    }

    #[tokio::test]
    async fn selected_custom_loader_reads_actual_owner_rows_only() {
        struct Loader;
        #[async_trait::async_trait]
        impl crate::agent::memory_loader::MemoryLoader for Loader {
            async fn load_context(&self, memory: &dyn Memory, input: &str) -> anyhow::Result<String> {
                let hits = memory.recall(input, 5, None).await?;
                assert_eq!(hits.len(), 1);
                let hit = hits.first().expect("exactly one owned memory hit");
                assert_eq!(hit.content, "prx_query owned evidence");
                Ok(hit.content.clone())
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let memory = crate::memory::SqliteMemory::new(tmp.path()).unwrap();
        let write = MemoryWriteContext {
            workspace_id: Some("prx_workspace".into()),
            channel: Some("agent_api".into()),
            raw_sender: Some("prx_user".into()),
            chat_type: Some("private".into()),
            chat_id: Some("prx_session".into()),
            ..Default::default()
        };
        for (owner, content) in [
            ("prx_owner", "prx_query owned evidence"),
            ("prx_foreign_owner", "prx_query foreign evidence"),
        ] {
            let mut scope = write.clone();
            scope.owner_id = Some(owner.into());
            scope.require_owner_match = true;
            memory
                .store_with_context_and_metadata(
                    owner,
                    content,
                    crate::memory::MemoryCategory::Core,
                    Some("prx_session"),
                    Some(&scope),
                    crate::memory::MemoryStoreMetadata {
                        workspace_id: Some("prx_workspace".into()),
                        owner_id: Some(owner.into()),
                        channel: Some("agent_api".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        let scope = selected_custom_memory_read_scope(&write, "prx_workspace", "prx_owner").unwrap();
        let loaded = crate::agent::memory_loader::load_selected_memory(
            &Loader,
            &memory,
            "prx_query",
            &scope,
            "prx_session",
            std::time::Instant::now() + std::time::Duration::from_millis(500),
        )
        .await
        .unwrap();
        assert_eq!(loaded.hits.len(), 1);
        assert_eq!(loaded.context, "prx_query owned evidence");
    }
}
