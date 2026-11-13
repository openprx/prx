//! Free-standing advisory decision driver and its execution-owned gate inputs.
//!
//! The helpers here rebuild the hard-gate inputs from execution-owned facts
//! (`AdmissionFactsV1`) and, on an authenticated resume turn, from the
//! verified resume evidence. The execution path (`open_turn_context_observed`)
//! and the advisory driver use the same helpers, so resume annotations and
//! execution gating stay identical.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::causal_tree::gates::GateInputs;
use crate::causal_tree::hard_gates_probe::HardGatesProbe;
use crate::config::Config;
use crate::config::schema::decision_engine::{DecisionEngineConfig, DecisionMode, effective_engine_backend};
use crate::cost::decision_attempt::DecisionAttemptAccounting;
use crate::decision_engine::admission_probe::{AdmissionProbe, AdvisoryProbe};
use crate::decision_engine::candidate::{CandidateId, Ns8};
use crate::decision_engine::candidate_generator::{
    CandidatePolicy, CandidateSet, CausalCandidateGenerator, DiscoveredRefs,
};
use crate::decision_engine::observe::{ObserveQueue, SqliteTelemetryWriter};
use crate::decision_engine::prediction::factory::{DecisionBackendFactory, FactoryBuildError, FactoryDeps};
use crate::decision_engine::prediction::{CandidateQuestion, GlobalQuestion, QuestionId, RequestLimits};
use crate::decision_engine::prediction_port::PredictionPort;
use crate::decision_engine::questions::{CompilerPolicy, QuestionCompiler};
use crate::decision_engine::state::{CausalState, DerivedState, HopLineage};
use crate::decision_engine::{DecisionEngine, IntentAnalysis, IntentInput, TurnOutcome};
use crate::observability::causal_event::CausalLineage;
use crate::providers::catalog::ResolvedModel;
use crate::runtime::decision_accounting::AccountingScope;
use crate::runtime::decision_accounting::analysis_bound::{AnalysisAccounting, analysis_allowance};
use crate::runtime::turn_context::TurnRuntimeContext;
use crate::security::decision_id::DecisionId;
use crate::security::runtime_key::DecisionKey;
use crate::tools::capability::admission_facts::AdmissionFactsV1;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogView, SurfacePolicy};
use crate::tools::capability::contract::{CapabilityRef, CatalogGeneration, CausalActionKind};
use crate::tools::capability::execution_trace::ExecutionTraceSnapshot;
use crate::tools::capability::resume_evidence::VerifiedResumeEvidenceV1;
use crate::tools::capability::turn_state::TrustedState;

/// Exact call fingerprints by candidate id.
pub(crate) type CallFingerprints = BTreeMap<CandidateId, [u8; 32]>;
/// Verified exact capability scope by call digest.
pub(crate) type VerifiedApprovalCalls = BTreeMap<[u8; 32], BTreeSet<CapabilityRef>>;

/// The baseline candidate maps of one verified resume: both candidate ids of
/// the resumed entry (unbound and namespace-bound with `state_key`) map to the
/// pending call digest, and the digest maps to the single resumed capability.
pub(crate) fn resume_gate_maps(
    evidence: &VerifiedResumeEvidenceV1,
    state_key: &DecisionKey,
) -> (CallFingerprints, VerifiedApprovalCalls) {
    let digest = *evidence.call_digest();
    let calls = BTreeMap::from([
        (
            CandidateId::new(CausalActionKind::UseTool, evidence.family(), None),
            digest,
        ),
        (
            CandidateId::new(
                CausalActionKind::UseTool,
                evidence.family(),
                Some(Ns8::of(state_key, evidence.namespace())),
            ),
            digest,
        ),
    ]);
    let approvals = BTreeMap::from([(digest, BTreeSet::from([evidence.capability().clone()]))]);
    (calls, approvals)
}

/// The only `GateInputs` constructor of the advisory driver: execution-owned
/// facts plus the resume maps (empty on a non-resume turn). Failed-call
/// counts, replay and retry limits are empty, as on every turn opening.
pub(crate) fn gate_inputs<'a>(
    facts: &'a AdmissionFactsV1,
    call_fingerprints: &'a CallFingerprints,
    verified_approval_calls: VerifiedApprovalCalls,
    replayed: &'a BTreeSet<[u8; 32]>,
) -> GateInputs<'a> {
    GateInputs {
        policy: &facts.policy,
        owner_id: &facts.owner_id,
        sender: &facts.sender,
        policy_channel: &facts.policy_channel,
        chat_type: &facts.chat_type,
        model_ref: &facts.model_ref,
        model_can_chat: facts.model_can_chat,
        model_can_call_tools: facts.model_can_call_tools,
        decision_endpoint_ready: true,
        child_scope: facts.child_scope(),
        verified_approval_calls,
        call_fingerprints,
        failed_call_counts: BTreeMap::new(),
        replayed_non_idempotent: replayed,
        retry_limit: None,
    }
}

/// The owned input of one advisory analysis, cloned from the turn's
/// [`TurnRuntimeContext`]. It holds no execution probe: `analyze` builds the
/// borrowed hard-gate probe locally from the facts and the resume evidence.
pub(crate) struct AdvisoryInput {
    pub(crate) decision_id: DecisionId,
    pub(crate) state: CausalState,
    pub(crate) facts: AdmissionFactsV1,
    pub(crate) resume_evidence: Option<Arc<VerifiedResumeEvidenceV1>>,
    pub(crate) state_key: Arc<DecisionKey>,
    pub(crate) catalog: Arc<CapabilityCatalog>,
    pub(crate) publication_policy: SurfacePolicy,
    pub(crate) catalog_generation: CatalogGeneration,
    pub(crate) accounting: Arc<AccountingScope>,
    pub(crate) model: ResolvedModel,
    pub(crate) request_limits: RequestLimits,
    /// Remaining turn budget `(tokens, micro_usd)` when the input was built.
    pub(crate) remaining: (u64, u64),
    /// The first serving send's admission `(tokens, micro_usd)` that the
    /// analysis must leave untouched: the G6 answer estimate and the price of
    /// the largest prompt the send may carry plus its response reserve.
    /// `None` when its price cannot be estimated.
    pub(crate) first_send_minimum: Option<(u64, u64)>,
}

impl AdvisoryInput {
    /// Clone the analysis input from the turn's runtime context. A turn
    /// without an accounting scope cannot be charged, so it gets no input.
    pub(crate) fn of(runtime: &TurnRuntimeContext, request_limits: RequestLimits) -> Option<Self> {
        let accounting = Arc::clone(runtime.accounting.as_ref()?);
        let limits = &runtime.send_limits;
        // The brief and the prompt are not built yet, so the send is priced
        // at the largest prompt its compaction admits (P1-03).
        let first_send_minimum = crate::cost::estimate::estimate_request_cost(
            &limits.model,
            &limits.cost,
            u64::try_from(limits.main_input_limit).unwrap_or(u64::MAX),
            u64::try_from(limits.response_reserve).unwrap_or(u64::MAX),
        )
        .map(|estimate| (crate::cost::estimate::candidate_tokens(0), estimate.micro_usd));
        Some(Self {
            decision_id: runtime.decision_id,
            state: build_state(runtime),
            facts: runtime.admission.clone(),
            resume_evidence: runtime.resume_evidence.clone(),
            state_key: Arc::clone(&runtime.state_key),
            catalog: Arc::clone(&runtime.catalog),
            publication_policy: runtime.publication_policy.clone(),
            catalog_generation: runtime.catalog_generation,
            accounting,
            model: limits.model.clone(),
            request_limits,
            remaining: (runtime.remaining_g6_tokens(), runtime.remaining_micro_usd()),
            first_send_minimum,
        })
    }

    fn catalog_view(&self) -> CatalogView<'_> {
        self.catalog
            .view(&CatalogScope::default())
            .with_surface(&self.publication_policy)
    }
}

/// The advisory `CausalState` of a turn, built only from its runtime context.
pub(crate) fn build_state(runtime: &TurnRuntimeContext) -> CausalState {
    CausalState {
        trusted: TrustedState::from_runtime(runtime.trusted.clone()),
        observed: runtime.observed.clone(),
        derived: DerivedState::default(),
        has_conversation_history: runtime.has_conversation_history,
        routing_input: runtime.routing_input.clone(),
        goal: runtime.goal.clone(),
        lineage: HopLineage::First,
        built_at_ms: u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
    }
}

/// P1-1: the bounded accounting of a priced analysis. Its worst case is the
/// allowance it may reserve, in tokens as the attempt reserves them and
/// priced as the attempt prices them, so worst case plus the protected first
/// send never exceeds the remaining budget. `None` (skip) when no allowance
/// is left or the send or the analysis cannot be priced.
fn bounded_accounting(input: &AdvisoryInput, provider: &str, model: &str) -> Option<AnalysisAccounting> {
    let protected = input.first_send_minimum?;
    let allowance = analysis_allowance(input.remaining, protected, |tokens| {
        input.accounting.worst_case_micro_usd(provider, model, tokens)
    })?;
    Some(AnalysisAccounting::new(
        Arc::clone(&input.accounting),
        protected,
        allowance,
    ))
}

/// Free-standing advisory driver: the only caller of the engine's prediction
/// path. It never changes admission, the tool surface or the turn's outcome.
pub(crate) struct DecisionAdvisory {
    config: Arc<Config>,
    /// Built only when the engine is enabled, with the turn service's state key.
    factory: Option<DecisionBackendFactory>,
    /// Opened by a background task, never on the turn's path (P1-04).
    telemetry: Arc<tokio::sync::OnceCell<AdvisoryRuntime>>,
    /// Admission before the store is open (P1-10): one opener task and one
    /// bounded queue, created by the first observation.
    pending: Arc<parking_lot::Mutex<PendingTelemetry>>,
}

/// Observation admission before the telemetry store is open.
enum PendingTelemetry {
    /// Nothing has been observed yet.
    Idle,
    /// The single opener task is running. Outcomes wait in a queue bounded
    /// like the [`ObserveQueue`] they move to; an outcome that does not fit
    /// is dropped and counted in `dropped`.
    Opening {
        outcomes: tokio::sync::mpsc::Sender<TurnOutcome>,
        dropped: u64,
        opener: Option<tokio::task::JoinHandle<()>>,
    },
    /// The opener moved the queue and its drops into the open store's
    /// [`ObserveQueue`]; observations go there directly.
    Opened,
}

impl DecisionAdvisory {
    pub(crate) fn new(config: Arc<Config>, factory: Option<DecisionBackendFactory>) -> Self {
        Self {
            config,
            factory,
            telemetry: Arc::new(tokio::sync::OnceCell::new()),
            pending: Arc::new(parking_lot::Mutex::new(PendingTelemetry::Idle)),
        }
    }

    /// The advisory driver of one publication (root, child or worker). A
    /// disabled engine builds no factory, store or credential (P1-I2); an
    /// enabled one builds its factory from the effective engine backend with
    /// the publication's state key in `deps` (R55, R9).
    pub(crate) fn for_publication(config: &Arc<Config>, deps: FactoryDeps<'_>) -> Result<Self, FactoryBuildError> {
        let factory = if config.decision_engine.enabled {
            Some(DecisionBackendFactory::from_config(config, deps)?)
        } else {
            None
        };
        Ok(Self::new(Arc::clone(config), factory))
    }

    pub(crate) fn config(&self) -> &DecisionEngineConfig {
        &self.config.decision_engine
    }

    #[cfg(test)]
    pub(crate) fn initialized(&self) -> bool {
        self.telemetry.initialized()
    }

    #[cfg(test)]
    pub(crate) fn factory(&self) -> Option<&DecisionBackendFactory> {
        self.factory.as_ref()
    }

    /// The request limits of the effective engine backend (R55).
    pub(crate) fn request_limits(&self) -> RequestLimits {
        RequestLimits {
            max_request_bytes: effective_engine_backend(&self.config).max_request_bytes,
            ..RequestLimits::default()
        }
    }

    /// Open the telemetry store once. Runs only inside a background task, so
    /// its blocking SQLite open never delays a turn.
    async fn open_telemetry<'a>(
        cell: &'a tokio::sync::OnceCell<AdvisoryRuntime>,
        config: &Config,
    ) -> &'a AdvisoryRuntime {
        cell.get_or_init(|| async {
            let engine_config = &config.decision_engine;
            let cap = crate::decision_engine::status::effective_telemetry_cap(engine_config);
            let queue_size = engine_config.observe_queue as usize;
            let path = config.workspace_dir.join("decision-engine.sqlite");
            let cancellation = CancellationToken::new();
            let worker_cancellation = cancellation.clone();
            let opened = tokio::task::spawn_blocking(move || {
                ObserveQueue::start(queue_size, cap as usize, worker_cancellation, || {
                    Ok(Box::new(SqliteTelemetryWriter::open(&path)?))
                })
            })
            .await;
            let (observations, worker) = match opened {
                Ok(Ok(started)) => started,
                _ => {
                    tracing::warn!("decision engine telemetry unavailable");
                    (ObserveQueue::disabled(), None)
                }
            };
            observations.register_status(&config.workspace_dir);
            #[cfg(test)]
            crate::decision_engine::access_spy::record_constructor(&config.workspace_dir);
            AdvisoryRuntime {
                engine: crate::decision_engine::causal_adapter::CausalEngine { observations },
                cancellation,
                worker: tokio::sync::Mutex::new(worker),
            }
        })
        .await
    }

    /// Close observation admission and wait for the telemetry worker until
    /// `deadline`. `false` means the worker is still running.
    pub(crate) async fn drain(&self, deadline: std::time::Instant) -> bool {
        let opener = match &mut *self.pending.lock() {
            PendingTelemetry::Opening { opener, .. } => opener.take(),
            PendingTelemetry::Idle | PendingTelemetry::Opened => None,
        };
        if let Some(handle) = opener {
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), handle).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "decision engine telemetry opening failed"),
                Err(_) => return false,
            }
        }
        let Some(runtime) = self.telemetry.get() else {
            return true;
        };
        runtime.cancellation.cancel();
        let mut worker = runtime.worker.lock().await;
        if let Some(completion) = worker.as_mut() {
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), completion).await {
                Ok(Ok(())) => {
                    worker.take();
                }
                _ => return false,
            }
        }
        true
    }

    /// Drain at shutdown. A late telemetry drain warns exactly once; catalog
    /// and causal writes stay authoritative, so the caller is not failed.
    pub(crate) async fn drain_or_warn(&self, deadline: std::time::Instant) -> bool {
        let drained = self.drain(deadline).await;
        if !drained {
            tracing::warn!("decision engine telemetry did not drain before shutdown deadline");
        }
        drained
    }

    /// Analyse one opened turn. Disabled, `post` mode, an uncharged turn or a
    /// turn too tight for a priced analysis yields no advice; nothing here
    /// can refuse or end the turn.
    pub(crate) async fn analyze_turn(
        &self,
        runtime: &TurnRuntimeContext,
        cancellation: &CancellationToken,
    ) -> Option<IntentAnalysis> {
        let config = self.config();
        if !config.enabled || config.mode == DecisionMode::Post || self.factory.is_none() {
            return None;
        }
        let Some(input) = AdvisoryInput::of(runtime, self.request_limits()) else {
            tracing::debug!("decision engine analysis skipped: the turn has no accounting scope");
            return None;
        };
        self.analyze(input, cancellation).await
    }

    /// Analyse `input` within `timeout_ms`. Failure yields `None` and one
    /// warning, never an error.
    pub(crate) async fn analyze(
        &self,
        input: AdvisoryInput,
        cancellation: &CancellationToken,
    ) -> Option<IntentAnalysis> {
        let config = self.config();
        if !config.enabled || config.mode == DecisionMode::Post {
            return None;
        }
        let factory = self.factory.as_ref()?;
        #[cfg(test)]
        crate::decision_engine::access_spy::record_constructor(&self.config.workspace_dir);
        let backend = effective_engine_backend(&self.config);
        // Builtin and cache hits spend nothing; a configured backend runs
        // only inside an allowance that leaves the first send admissible.
        let accounting = match crate::config::schema::causal_tree::configured_selection(backend) {
            Some(selection) => {
                let Some(bounded) = bounded_accounting(&input, &selection.provider, backend.model.trim()) else {
                    tracing::debug!("decision engine analysis skipped: the turn budget is too tight");
                    return None;
                };
                Some(bounded)
            }
            None => None,
        };
        let accounting: &dyn DecisionAttemptAccounting = match accounting.as_ref() {
            Some(bounded) => bounded,
            None => input.accounting.as_ref(),
        };
        let timeout = Duration::from_millis(u64::from(config.timeout_ms));
        // The engine step warns itself; a failed or late prediction warns here.
        match tokio::time::timeout(
            timeout,
            self.analyze_within(factory, &input, accounting, timeout, cancellation),
        )
        .await
        {
            Ok(Ok(analysis)) => analysis,
            Ok(Err(())) | Err(_) => {
                tracing::warn!("decision engine analysis unavailable; continuing without advice");
                None
            }
        }
    }

    async fn analyze_within(
        &self,
        factory: &DecisionBackendFactory,
        input: &AdvisoryInput,
        accounting: &dyn DecisionAttemptAccounting,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<Option<IntentAnalysis>, ()> {
        let view = input.catalog_view();
        if view.generation() != input.catalog_generation {
            return Err(());
        }
        let (calls, approvals) = input.resume_evidence.as_deref().map_or_else(
            || (BTreeMap::new(), BTreeMap::new()),
            |evidence| resume_gate_maps(evidence, &input.state_key),
        );
        let replayed = BTreeSet::new();
        let gates = gate_inputs(&input.facts, &calls, approvals, &replayed);
        let observed_latency = BTreeMap::new();
        let cost_model =
            crate::decision_engine::cost_model::RuntimeCostModel::new(&input.model, &self.config, &observed_latency);
        let discovered = DiscoveredRefs::empty();
        let policy = CandidatePolicy {
            key: &input.state_key,
            max_candidates: usize::from(self.config.causal_tree.policy.max_candidates),
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &cost_model,
            discovered_refs: &discovered,
        };
        // R13: the execution probe only annotates the analysed set; no
        // admission outcome filters it, and the advice never reaches execution.
        // The probes borrow `dyn AdmissionProbe` (not `Sync`), so they end
        // before the prediction is awaited.
        let candidates = {
            let probe = HardGatesProbe {
                state: &input.state,
                catalog: &view,
                gates: &gates,
            };
            let advisory = AdvisoryProbe {
                inner: &probe as &dyn AdmissionProbe,
            };
            CausalCandidateGenerator::generate(&input.state, &view, &policy, &advisory).map_err(|_| ())?
        };
        let keys = match &candidates {
            CandidateSet::Legal { candidates, .. } => candidates
                .iter()
                .map(|candidate| candidate.candidate_id().key())
                .collect::<Vec<_>>(),
            CandidateSet::NoLegalPath { .. } => Vec::new(),
        };
        let mut answers = BTreeMap::new();
        if matches!(candidates, CandidateSet::Legal { .. }) {
            let expires_at = std::time::Instant::now().checked_add(timeout).ok_or(())?;
            // The compiler projects and checks the derived candidates of the
            // exact set it seals, as the retired hop did before compiling.
            let mut state = input.state.clone();
            state.derived =
                crate::decision_engine::prediction_port::derive_state(&candidates, &view).map_err(|_| ())?;
            let compiled = QuestionCompiler::compile(
                &state,
                &candidates,
                &CompilerPolicy {
                    key: &input.state_key,
                    model: factory.request_model().or_else(|| Some(input.model.model())),
                    expires_at,
                    limits: input.request_limits,
                },
            )
            .map_err(|_| ())?;
            let port = factory.prediction_port(Some(accounting));
            let lineage = lineage(&state);
            let prediction = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(None),
                prediction = port.predict(compiled.request(), &lineage) => prediction.map_err(|_| ())?,
            };
            let questions = GlobalQuestion::ALL
                .into_iter()
                .map(QuestionId::global)
                .chain(keys.iter().flat_map(|candidate| {
                    CandidateQuestion::ALL
                        .into_iter()
                        .map(move |question| QuestionId::candidate(candidate, question))
                }));
            for question in questions {
                if let Some(answer) = prediction.served.answer(&question) {
                    answers.insert(question, answer.clone());
                }
            }
        }
        // Analysis needs only validated answers. Telemetry is opened lazily by
        // the final observation so its I/O cannot extend this timeout.
        let engine = crate::decision_engine::causal_adapter::CausalEngine {
            observations: ObserveQueue::disabled(),
        };
        let decision_id = input.decision_id.to_string();
        Ok(crate::decision_engine::turn::analyze(
            &engine,
            self.config(),
            IntentInput {
                decision_id: &decision_id,
                answers: &answers,
                candidates: &keys,
            },
            cancellation,
        )
        .await)
    }

    /// Record the turn's outcome. `trace` is read only when observation runs.
    /// Admission never waits: an open store takes the outcome on its bounded
    /// queue at once (P1-04). Before the store is open, the outcome waits in
    /// one queue with the same bound while a single background task opens
    /// the store; an outcome that does not fit is dropped and counted (P1-10).
    pub(crate) async fn observe(
        &self,
        trace: impl FnOnce() -> ExecutionTraceSnapshot,
        decision_id: Option<String>,
        succeeded: bool,
    ) {
        let config = self.config();
        if !config.enabled || config.mode == DecisionMode::Pre {
            return;
        }
        let tools = trace().calls.into_iter().map(|call| call.capability_id).collect();
        let outcome = match TurnOutcome::new(decision_id, tools, succeeded) {
            Ok(outcome) => outcome,
            Err(_) => {
                tracing::warn!("decision engine outcome exceeds observation bounds");
                return;
            }
        };
        if let Some(runtime) = self.telemetry.get() {
            runtime.engine.observe(outcome);
            return;
        }
        let mut pending = self.pending.lock();
        // The opener publishes the store and then takes the queue over under
        // this lock, so an outcome admitted here is either queued before the
        // takeover or observed directly after it.
        if let Some(runtime) = self.telemetry.get() {
            drop(pending);
            runtime.engine.observe(outcome);
            return;
        }
        if matches!(*pending, PendingTelemetry::Idle) {
            let bound = config.observe_queue as usize;
            if !(1..=OBSERVE_QUEUE_MAX).contains(&bound) {
                tracing::warn!("decision engine observation queue bound is invalid; outcome dropped");
                return;
            }
            let (outcomes, receiver) = tokio::sync::mpsc::channel(bound);
            let opener = tokio::spawn(Self::open_and_take_over(
                Arc::clone(&self.telemetry),
                Arc::clone(&self.config),
                Arc::clone(&self.pending),
                receiver,
            ));
            *pending = PendingTelemetry::Opening {
                outcomes,
                dropped: 0,
                opener: Some(opener),
            };
        }
        match &mut *pending {
            PendingTelemetry::Opening { outcomes, dropped, .. } => {
                if outcomes.try_send(outcome).is_err() {
                    *dropped = dropped.saturating_add(1);
                }
            }
            // `Opened` is set only after the store is published, which the
            // check above has seen; `Idle` was replaced above.
            PendingTelemetry::Idle | PendingTelemetry::Opened => {
                tracing::warn!("decision engine observation arrived in an unexpected admission state; dropped");
            }
        }
    }

    /// The single opener task: open the store, then move the queued outcomes
    /// and the pre-open drop count into its [`ObserveQueue`].
    async fn open_and_take_over(
        cell: Arc<tokio::sync::OnceCell<AdvisoryRuntime>>,
        config: Arc<Config>,
        pending: Arc<parking_lot::Mutex<PendingTelemetry>>,
        mut receiver: tokio::sync::mpsc::Receiver<TurnOutcome>,
    ) {
        let runtime = Self::open_telemetry(&cell, &config).await;
        let dropped = match std::mem::replace(&mut *pending.lock(), PendingTelemetry::Opened) {
            PendingTelemetry::Opening { dropped, .. } => dropped,
            PendingTelemetry::Idle | PendingTelemetry::Opened => 0,
        };
        // Every queued send completed under the lock taken above, and later
        // observations see the open store, so this drains the whole queue.
        receiver.close();
        while let Ok(outcome) = receiver.try_recv() {
            runtime.engine.observe(outcome);
        }
        runtime
            .engine
            .observations
            .counters
            .dropped
            .fetch_add(dropped, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The largest observation queue bound ([`ObserveQueue::start`] and the
/// `observe_queue` config range).
const OBSERVE_QUEUE_MAX: usize = 1_024;

fn lineage(state: &CausalState) -> CausalLineage {
    let trusted = state.trusted.parts();
    CausalLineage {
        workspace_id: Arc::clone(&trusted.workspace_id),
        owner_id: trusted.owner_id.clone(),
        session_key: trusted.session_key.clone(),
        run_id: trusted.run_id.clone(),
        task_id: trusted.task_id.clone(),
        source_event_id: trusted.source_event_id.clone(),
        causation_event_id: trusted.causation_event_id.clone(),
        decision_id: None,
    }
}

struct AdvisoryRuntime {
    engine: crate::decision_engine::causal_adapter::CausalEngine,
    cancellation: CancellationToken,
    worker: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl Drop for AdvisoryRuntime {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
pub(crate) mod tests;
