//! Free-standing advisory decision driver. The analysed candidate set is
//! structural only; no admission outcome annotates or filters it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::config::schema::decision_engine::{DecisionEngineConfig, DecisionMode, effective_engine_backend};
use crate::cost::decision_attempt::DecisionAttemptAccounting;
use crate::decision_engine::candidate_generator::{
    CandidatePolicy, CandidateSet, CausalCandidateGenerator, DiscoveredRefs,
};
use crate::decision_engine::observe::{ObserveQueue, SqliteTelemetryWriter};
use crate::decision_engine::prediction::factory::{DecisionBackendFactory, FactoryBuildError, FactoryDeps};
use crate::decision_engine::prediction::{CandidateQuestion, GlobalQuestion, QuestionId, RequestLimits};
use crate::decision_engine::prediction_port::PredictionPort;
use crate::decision_engine::questions::{CompilerPolicy, QuestionCompiler};
use crate::decision_engine::state::{CausalState, DerivedState, HopLineage};
use crate::decision_engine::{
    AnalysisRecord, IntentAnalysis, IntentInput, TelemetryRecord, TurnOutcome, TurnResultClass,
};
use crate::observability::causal_event::CausalLineage;
use crate::providers::catalog::ResolvedModel;
use crate::runtime::decision_accounting::AccountingScope;
use crate::runtime::decision_accounting::analysis_bound::{AnalysisAccounting, analysis_allowance};
use crate::runtime::turn_context::TurnRuntimeContext;
use crate::security::decision_id::DecisionId;
use crate::security::runtime_key::DecisionKey;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogView, SurfacePolicy};
use crate::tools::capability::contract::CatalogGeneration;
use crate::tools::capability::execution_trace::ExecutionTraceSnapshot;
use crate::tools::capability::turn_state::TrustedState;

/// The owned input of one advisory analysis, cloned from the turn's
/// [`TurnRuntimeContext`].
pub(crate) struct AdvisoryInput {
    pub(crate) decision_id: DecisionId,
    pub(crate) state: CausalState,
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
    /// Completion of the single opener task, kept for the advisory's whole
    /// life: every drain joins a clone, so a drain that times out leaves it
    /// owned here and a later drain waits for the same task (P1-10).
    opener: parking_lot::Mutex<Option<OpenerJoin>>,
}

/// Why the opener task ended without moving the pre-open queue.
#[derive(Clone, Copy, Debug)]
enum OpenerFailure {
    Panicked,
    Cancelled,
}

/// The shared, typed completion of the opener task.
type OpenerJoin = futures::future::Shared<futures::future::BoxFuture<'static, Result<(), OpenerFailure>>>;

/// Observation admission before the telemetry store is open.
enum PendingTelemetry {
    /// Nothing has been observed yet.
    Idle,
    /// The single opener task is running. Outcomes wait in a queue bounded
    /// like the [`ObserveQueue`] they move to; an outcome that does not fit
    /// is dropped and counted in `dropped`.
    Opening {
        outcomes: tokio::sync::mpsc::Sender<TelemetryRecord>,
        dropped: u64,
    },
    /// The opener moved the queue and its drops into the open store's
    /// [`ObserveQueue`], under the same lock that sets this state;
    /// observations go there directly.
    Opened,
}

impl DecisionAdvisory {
    pub(crate) fn new(config: Arc<Config>, factory: Option<DecisionBackendFactory>) -> Self {
        Self {
            config,
            factory,
            telemetry: Arc::new(tokio::sync::OnceCell::new()),
            pending: Arc::new(parking_lot::Mutex::new(PendingTelemetry::Idle)),
            opener: parking_lot::Mutex::new(None),
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
    pub(crate) const fn factory(&self) -> Option<&DecisionBackendFactory> {
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
        cell.get_or_init(|| Self::start_runtime(config)).await
    }

    /// Open the store and start its worker; an unavailable store yields a
    /// disabled queue.
    async fn start_runtime(config: &Config) -> AdvisoryRuntime {
        let engine_config = &config.decision_engine;
        // Analyses (pre/both) and outcomes (post/both) share one cap, each
        // table FIFO-evicted to it; 0 opens no file (P2-R10).
        let cap = engine_config.telemetry_cap;
        let queue_size = engine_config.observe_queue as usize;
        let path = config
            .workspace_dir
            .join(crate::decision_engine::observe::TELEMETRY_FILE);
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
    }

    /// Close observation admission and wait for the telemetry worker until
    /// `deadline`. `false` means the opener or the worker is still running,
    /// or the opener ended without moving the pre-open queue.
    ///
    /// Whenever an opener exists, drain joins it before it cancels the
    /// writer, whatever the admission state: the opener moves every queued
    /// outcome and the drop count into the store before it ends. The join is
    /// a clone of the advisory-owned completion, so a timeout detaches
    /// nothing and a later drain waits for the same task.
    pub(crate) async fn drain(&self, deadline: std::time::Instant) -> bool {
        let deadline = tokio::time::Instant::from_std(deadline);
        let opener = self.opener.lock().clone();
        let mut transferred = true;
        if let Some(join) = opener {
            match tokio::time::timeout_at(deadline, join).await {
                Ok(Ok(())) => {}
                Ok(Err(failure)) => {
                    tracing::warn!(?failure, "decision engine telemetry opening failed");
                    transferred = false;
                }
                Err(_) => return false,
            }
        }
        let Some(runtime) = self.telemetry.get() else {
            return transferred;
        };
        runtime.cancellation.cancel();
        let mut worker = runtime.worker.lock().await;
        if let Some(completion) = worker.as_mut() {
            match tokio::time::timeout_at(deadline, completion).await {
                Ok(Ok(())) => {
                    worker.take();
                }
                _ => return false,
            }
        }
        transferred
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
        let accounting = match crate::config::schema::decision_engine::configured_selection(backend) {
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
        let observed_latency = BTreeMap::new();
        let cost_model =
            crate::decision_engine::cost_model::RuntimeCostModel::new(&input.model, &self.config, &observed_latency);
        let discovered = DiscoveredRefs::empty();
        let policy = CandidatePolicy {
            key: &input.state_key,
            max_candidates: usize::from(self.config.decision_engine.max_candidates),
            can_deliver_text: true,
            may_delegate: false,
            delegation_target: None,
            cost_model: &cost_model,
            discovered_refs: &discovered,
        };
        // The analysed set is structural; the advice never reaches execution.
        let candidates = CausalCandidateGenerator::generate(&input.state, &view, &policy).map_err(|_| ())?;
        let keys = match &candidates {
            CandidateSet::Legal { candidates, .. } => candidates
                .iter()
                .map(|candidate| candidate.candidate_id().key())
                .collect::<Vec<_>>(),
            CandidateSet::NoLegalPath => Vec::new(),
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
    /// It is synchronous, so a caller cannot wait on it; it must run inside a
    /// Tokio runtime because the first pre-open outcome spawns the opener.
    pub(crate) fn observe(
        &self,
        trace: impl FnOnce() -> ExecutionTraceSnapshot,
        decision_id: Option<String>,
        ended: TurnEnd<'_>,
    ) {
        let config = self.config();
        if !config.enabled || config.mode == DecisionMode::Pre {
            return;
        }
        let tools = trace().calls.into_iter().map(|call| call.capability_id).collect();
        let cost_micro_usd = ended.usage.and_then(|usage| self.settled_cost_micro_usd(&usage));
        let outcome = match TurnOutcome::new(decision_id, tools, ended.result, cost_micro_usd, ended.duration_ms) {
            Ok(outcome) => outcome,
            Err(_) => {
                tracing::warn!("decision engine outcome exceeds observation bounds");
                return;
            }
        };
        self.admit(TelemetryRecord::Outcome(outcome));
    }

    /// Record the analysis whose redacted `brief` was sent to the serving
    /// model (P2-R10). Only `pre`/`both` produce a brief.
    pub(crate) fn record_analysis(&self, analysis: &IntentAnalysis, brief: &str) {
        let config = self.config();
        if !config.enabled || config.mode == DecisionMode::Post || config.telemetry_cap == 0 {
            return;
        }
        let redacted = crate::agent::terminal::redact_persisted_text(brief);
        let created_at_ms = chrono::Utc::now().timestamp_millis();
        let candidates = usize::try_from(config.candidates).unwrap_or(usize::MAX);
        match AnalysisRecord::new(analysis, redacted, created_at_ms, candidates) {
            Ok(record) => self.admit(TelemetryRecord::Analysis(record)),
            Err(_) => tracing::warn!("decision engine analysis exceeds observation bounds"),
        }
    }

    /// The turn's settled cost: the same priced usage record the terminal
    /// settles as `turn:<id>:usage`. `None` for unknown or subscription
    /// pricing.
    fn settled_cost_micro_usd(&self, usage: &TurnUsage<'_>) -> Option<u64> {
        let provider = crate::providers::redact_endpoint_userinfo(usage.provider);
        let record = crate::llm::route_decision::MeteredTokenUsageRecord::from_parts(
            &provider,
            usage.model,
            usage.tokens,
            &self.config.cost,
        )?;
        let cost = record.cost_usd?;
        let micro = crate::cost::tracker::ceil_micro_usd(cost).ok()?;
        u64::try_from(micro).ok()
    }

    fn admit(&self, outcome: TelemetryRecord) {
        let config = self.config();
        if let Some(runtime) = self.telemetry.get() {
            runtime.engine.observations.record(outcome);
            return;
        }
        let mut pending = self.pending.lock();
        // The opener publishes the store and then takes the queue over under
        // this lock, so an outcome admitted here is either queued before the
        // takeover or observed directly after it.
        if let Some(runtime) = self.telemetry.get() {
            drop(pending);
            runtime.engine.observations.record(outcome);
            return;
        }
        if matches!(*pending, PendingTelemetry::Idle) {
            let bound = config.observe_queue as usize;
            if !(1..=OBSERVE_QUEUE_MAX).contains(&bound) {
                tracing::warn!("decision engine observation queue bound is invalid; outcome dropped");
                return;
            }
            let (outcomes, receiver) = tokio::sync::mpsc::channel(bound);
            // The completion slot is held from the spawn until it is filled,
            // so no drain can see a running opener without its completion.
            let mut slot = self.opener.lock();
            let handle = tokio::spawn(Self::open_and_take_over(
                Arc::clone(&self.telemetry),
                Arc::clone(&self.config),
                Arc::clone(&self.pending),
                receiver,
            ));
            *slot = Some(opener_join(handle));
            drop(slot);
            *pending = PendingTelemetry::Opening { outcomes, dropped: 0 };
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
    /// and the pre-open drop count into its [`ObserveQueue`]. The whole move
    /// and the switch to `Opened` happen under one admission lock, so no
    /// state observes `Opened` before the move is complete.
    async fn open_and_take_over(
        cell: Arc<tokio::sync::OnceCell<AdvisoryRuntime>>,
        config: Arc<Config>,
        pending: Arc<parking_lot::Mutex<PendingTelemetry>>,
        mut receiver: tokio::sync::mpsc::Receiver<TelemetryRecord>,
    ) {
        let runtime = Self::open_telemetry(&cell, &config).await;
        let mut admission = pending.lock();
        // Every queued send completes under this lock, and later observations
        // see the open store, so this drains the whole queue.
        receiver.close();
        while let Ok(record) = receiver.try_recv() {
            runtime.engine.observations.record(record);
        }
        let dropped = match &*admission {
            PendingTelemetry::Opening { dropped, .. } => *dropped,
            PendingTelemetry::Idle | PendingTelemetry::Opened => 0,
        };
        runtime
            .engine
            .observations
            .counters
            .dropped
            .fetch_add(dropped, std::sync::atomic::Ordering::Relaxed);
        *admission = PendingTelemetry::Opened;
    }
}

/// The shared completion of the opener task. A panicked or cancelled task
/// never moved the queue, which drain reports instead of success.
fn opener_join(handle: tokio::task::JoinHandle<()>) -> OpenerJoin {
    use futures::FutureExt as _;
    async move {
        handle.await.map_err(|error| {
            if error.is_panic() {
                OpenerFailure::Panicked
            } else {
                OpenerFailure::Cancelled
            }
        })
    }
    .boxed()
    .shared()
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

/// How one turn ended, for its telemetry outcome (P2-R10).
pub(crate) struct TurnEnd<'a> {
    pub result: TurnResultClass,
    pub duration_ms: u64,
    /// The serving provider, model and aggregated usage the terminal settles:
    /// the returned trace of a completed turn, the retained provider evidence
    /// of a failed, cancelled or refused one.
    pub usage: Option<TurnUsage<'a>>,
}

/// The serving label and usage the terminal settles.
pub(crate) struct TurnUsage<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub tokens: &'a crate::llm::route_decision::TokenUsage,
}
