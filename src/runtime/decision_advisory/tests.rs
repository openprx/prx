#![cfg(test)]
use super::*;
use crate::decision_engine::observe::TelemetryWriter;

struct GatedWriter {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}

impl TelemetryWriter for GatedWriter {
    fn append_fifo(&mut self, _: crate::decision_engine::TelemetryRecord, _: usize) -> anyhow::Result<()> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        self.release.recv()?;
        Ok(())
    }
}

pub(crate) fn enabled_config(workspace: &std::path::Path) -> Arc<Config> {
    let mut config = Config {
        workspace_dir: workspace.into(),
        ..Default::default()
    };
    config.decision_engine.enabled = true;
    Arc::new(config)
}

#[tokio::test(start_paused = true)]
async fn late_advisory_drain_closes_admission_and_completes_after_release() {
    let temp = tempfile::tempdir().unwrap();
    let advisory = DecisionAdvisory::new(enabled_config(temp.path()), None);
    let cancellation = CancellationToken::new();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (observations, worker) = ObserveQueue::start(2, 10, cancellation.clone(), || {
        Ok(Box::new(GatedWriter {
            entered: Some(entered_tx),
            release: release_rx,
        }))
    })
    .unwrap();
    assert_eq!(
        observations.observe(
            TurnOutcome::new(
                None,
                vec![],
                crate::decision_engine::TurnResultClass::Completed,
                None,
                0
            )
            .unwrap()
        ),
        crate::decision_engine::ObserveAdmission::Accepted
    );
    entered_rx.await.unwrap();
    assert!(
        advisory
            .telemetry
            .set(AdvisoryRuntime {
                engine: crate::decision_engine::causal_adapter::CausalEngine { observations },
                cancellation,
                worker: tokio::sync::Mutex::new(worker),
            })
            .is_ok()
    );
    assert!(
        !advisory
            .drain(std::time::Instant::now() + std::time::Duration::from_millis(20))
            .await,
        "a gated worker is still running at the deadline"
    );
    let runtime = advisory.telemetry.get().unwrap();
    assert!(
        runtime.cancellation.is_cancelled(),
        "drain must close observation admission"
    );
    release_tx.send(()).unwrap();
    tokio::time::resume();
    assert!(
        advisory
            .drain(std::time::Instant::now() + std::time::Duration::from_secs(2))
            .await
    );
}

/// Counts WARN events whose message contains `needle` on this thread.
struct WarnCounter {
    needle: &'static str,
    count: Arc<std::sync::atomic::AtomicUsize>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnCounter {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        if *event.metadata().level() == tracing::Level::WARN {
            let mut message = Message(String::new());
            event.record(&mut message);
            if message.0.contains(self.needle) {
                self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

/// Usage every [`RecordingBackend`] attempt reports: 1,200 tokens, which a
/// one-micro-USD-per-token price settles at 1,200 micro-USD.
pub(crate) const RECORDED_USAGE: crate::cost::decision_attempt::DecisionUsage =
    crate::cost::decision_attempt::DecisionUsage {
        input_tokens: Some(1_000),
        output_tokens: Some(200),
        cost_micro_usd: None,
    };

/// One micro-USD per token for `mock`/`model` in every price column.
pub(crate) fn price_mock_model(config: &mut Config, model: &str) {
    config.cost.prices.insert(
        crate::config::schema::usage_pricing_key("mock", model),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
}

/// Answers every compiled request with a plain Answer and records what the
/// factory hands the backend: the request limits and, like a real remote
/// transport, one physical attempt begun on the accounting owner it is given
/// and settled with [`RECORDED_USAGE`]. `attempts` counts begun attempts only.
pub(crate) struct RecordingBackend {
    pub(crate) identity: crate::cost::decision_attempt::BackendIdentity,
    pub(crate) key: Arc<DecisionKey>,
    pub(crate) attempts: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) request_bytes: Arc<parking_lot::Mutex<Vec<usize>>>,
    /// The candidate action the scripted answers predict with full
    /// confidence (`"answer"`, `"refuse"`, `"clarify"`, ...).
    pub(crate) prefer: &'static str,
}

/// Wire answers to every question of a real compiled request that predict
/// `prefer` with full confidence: the next-action choice picks it when it is
/// an option, every candidate of that action applies, succeeds and has the
/// highest gain, every other candidate does not apply and has no gain.
pub(crate) fn scripted_answers(body: &serde_json::Value, prefer: &str) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context as _;
    use serde_json::{Value, json};
    let candidates = body
        .pointer("/state/candidates")
        .and_then(Value::as_array)
        .context("candidate projection")?;
    let preferred = |instruction: &str| {
        candidates.iter().enumerate().any(|(index, candidate)| {
            instruction.contains(&format!("$.candidates[{index}]"))
                && candidate.get("action").and_then(Value::as_str) == Some(prefer)
        })
    };
    let names_candidate = |instruction: &str| instruction.contains("$.candidates[");
    let questions = body
        .get("questions")
        .and_then(Value::as_object)
        .context("wire questions")?;
    let mut answers = serde_json::Map::new();
    for (id, question) in questions {
        let instruction = question
            .get("instructions")
            .and_then(Value::as_str)
            .context("question instructions")?;
        let answer = match question.get("type").and_then(Value::as_str) {
            Some("choice") => {
                let options = question
                    .get("criteria")
                    .and_then(Value::as_object)
                    .context("choice criteria")?;
                let chosen = [prefer, "none", "other"]
                    .into_iter()
                    .find(|option| options.contains_key(*option))
                    .or_else(|| options.keys().next().map(String::as_str))
                    .context("choice option")?;
                let probabilities: serde_json::Map<String, Value> = options
                    .keys()
                    .map(|option| (option.clone(), json!(if option == chosen { 1.0 } else { 0.0 })))
                    .collect();
                json!({"type":"choice","choice":chosen,"probabilities":probabilities,"confidence":1.0})
            }
            Some("noul") => {
                let yes = if names_candidate(instruction) {
                    preferred(instruction)
                } else {
                    instruction.starts_with("Is the observed evidence sufficient")
                };
                json!({"type":"noul","noul":if yes {1.0} else {0.0}})
            }
            Some("score") => {
                let levels = question
                    .get("criteria")
                    .and_then(Value::as_array)
                    .context("score criteria")?;
                let last = levels.len().checked_sub(1).context("score legend")?;
                let high = if instruction.contains("risk") {
                    false
                } else {
                    !names_candidate(instruction) || preferred(instruction)
                };
                let selected = if high { last } else { 0 };
                let legend: serde_json::Map<String, Value> = levels
                    .iter()
                    .enumerate()
                    .map(|(index, level)| (index.to_string(), level.clone()))
                    .collect();
                let probabilities: serde_json::Map<String, Value> = levels
                    .iter()
                    .enumerate()
                    .map(|(index, _)| (index.to_string(), json!(if index == selected { 1.0 } else { 0.0 })))
                    .collect();
                json!({"type":"score","score":selected,"legend":legend,"probabilities":probabilities,"confidence":1.0})
            }
            _ => anyhow::bail!("unexpected decision primitive"),
        };
        answers.insert(id.clone(), answer);
    }
    Ok(json!({
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "answers": answers,
        "usage": {"input_tokens": 1_000, "output_tokens": 200},
    }))
}

#[async_trait::async_trait]
impl crate::decision_engine::prediction::backend::DecisionBackend for RecordingBackend {
    fn identity(&self) -> &crate::cost::decision_attempt::BackendIdentity {
        &self.identity
    }
    fn locality(&self) -> crate::config::schema::causal_tree::BackendLocality {
        crate::config::schema::causal_tree::BackendLocality::Builtin
    }
    fn version_policy(&self) -> crate::decision_engine::prediction::ModelVersionPolicy {
        crate::decision_engine::prediction::ModelVersionPolicy::ExactPinned(Arc::clone(&self.identity.configured_model))
    }
    async fn evaluate_with_accounting(
        &self,
        request: &crate::decision_engine::prediction::DecisionRequest,
        body: &crate::decision_engine::prediction::outbound::GuardedBody,
        deadline: std::time::Instant,
        accounting: Option<&dyn DecisionAttemptAccounting>,
    ) -> Result<crate::decision_engine::prediction::DecisionResponse, crate::cost::decision_attempt::DecisionError>
    {
        use crate::cost::decision_attempt::DecisionError;
        const TRANSPORT_BINDING: [u8; 32] = [9; 32];
        self.request_bytes.lock().push(body.bytes().len());
        let receipt = accounting
            .ok_or(DecisionError::BudgetExceeded)?
            .begin(crate::cost::decision_attempt::AttemptBinding {
                backend: &self.identity,
                provider: "mock",
                model: &self.identity.configured_model,
                transport_binding: &TRANSPORT_BINDING,
                guarded_body: body.bytes(),
                request_id: request.request_id(),
                deadline,
                remaining_attempts: 1,
            })
            .await?;
        self.attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let response = self.evaluate(request, body, deadline).await?;
        receipt.settle(RECORDED_USAGE).await?;
        Ok(response)
    }
    async fn evaluate(
        &self,
        request: &crate::decision_engine::prediction::DecisionRequest,
        body: &crate::decision_engine::prediction::outbound::GuardedBody,
        _deadline: std::time::Instant,
    ) -> Result<crate::decision_engine::prediction::DecisionResponse, crate::cost::decision_attempt::DecisionError>
    {
        use crate::cost::decision_attempt::DecisionError;
        let wire: serde_json::Value = serde_json::from_slice(body.bytes()).map_err(|_| DecisionError::Unavailable)?;
        let answer = scripted_answers(&wire, self.prefer).map_err(|_| DecisionError::Unavailable)?;
        let bytes = serde_json::to_vec(&answer).map_err(|_| DecisionError::Unavailable)?;
        crate::decision_engine::prediction::wire::SystemOneCodec::new(request, &self.key)?.decode_response(
            &bytes,
            crate::decision_engine::prediction::wire::ResponseEnvelope {
                backend: "mock",
                provider_request_id: None,
                elapsed_ms: 0,
            },
        )
    }
}

pub(crate) fn analysis_input(
    config: &Arc<Config>,
    key: &Arc<DecisionKey>,
    request_limits: RequestLimits,
) -> AdvisoryInput {
    let catalog = Arc::new(
        CapabilityCatalog::build(&[], &Default::default(), &SurfacePolicy::default()).expect("test: empty catalog"),
    );
    let mut trusted = crate::decision_engine::state::fixtures::trusted();
    trusted.catalog_generation = *catalog.generation();
    let budgets = trusted.budgets;
    let model = crate::providers::selection::check_default(
        config,
        crate::providers::catalog::resolver::UnknownPolicy::Invocation,
    )
    .expect("test: default model");
    let accounting = Arc::new(AccountingScope::new(
        Arc::clone(config),
        Arc::clone(key),
        Arc::default(),
        [3; 32],
        budgets,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
    ));
    let state = CausalState {
        trusted: TrustedState::from_runtime(trusted),
        observed: Default::default(),
        derived: DerivedState::default(),
        has_conversation_history: false,
        routing_input: crate::decision_engine::state::RawRoutingInput::new("hello".into()),
        goal: crate::decision_engine::state::RawRoutingInput::new("hello".into()),
        lineage: HopLineage::First,
        built_at_ms: 0,
    };
    AdvisoryInput {
        decision_id: DecisionId::new_runtime(),
        state,
        facts: AdmissionFactsV1 {
            policy: Arc::new(crate::security::SecurityPolicy::default()),
            owner_id: Arc::from("owner-1"),
            sender: Arc::from("owner-1"),
            policy_channel: Arc::from("cli"),
            chat_type: Arc::from("dm"),
            model_ref: Arc::from("anthropic:claude"),
            model_can_chat: true,
            model_can_call_tools: true,
            child_scope: None,
            child_discovery: false,
            child_served: None,
        },
        state_key: Arc::clone(key),
        catalog_generation: *catalog.generation(),
        catalog,
        publication_policy: SurfacePolicy::default(),
        accounting,
        model,
        request_limits,
        remaining: (u64::MAX, u64::MAX),
        first_send_minimum: Some((0, 0)),
    }
}

/// R9: the driver analyses with no turn service. A configured non-builtin
/// slot is charged through the input's accounting scope with the input's
/// request limits; the builtin spends nothing.
#[tokio::test]
async fn decision_advisory_runs_without_turn_decision_service() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = (*enabled_config(temp.path())).clone();
    // Cheap enough that the whole 8,000-token fixture slice fits its money.
    for model in ["recording-model", "typesafe/jev-1.13"] {
        price_mock_model(&mut config, model);
        if let Some(price) = config
            .cost
            .prices
            .get_mut(&crate::config::schema::usage_pricing_key("mock", model))
        {
            price.cache_write = 0.0;
            price.cache_read = 0.0;
            price.input = 0.25;
            price.output = 0.25;
        }
    }
    let config = Arc::new(config);
    let key = crate::decision_engine::state::fixtures::key();
    let cancellation = CancellationToken::new();
    for model in ["recording-model", "typesafe/jev-1.13"] {
        // Each model runs as its own turn in its own workspace: the fixture
        // scope id is fixed, so a shared workspace cost journal would charge
        // both attempts to one turn's money limit.
        let model_workspace = tempfile::tempdir().unwrap();
        let mut model_config = (*config).clone();
        model_config.workspace_dir = model_workspace.path().into();
        let model_config = Arc::new(model_config);
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request_bytes = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let factory = DecisionBackendFactory::from_slots(
            vec![Box::new(RecordingBackend {
                identity: crate::cost::decision_attempt::BackendIdentity {
                    kind: crate::config::schema::causal_tree::BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from(model),
                },
                key: Arc::clone(&key),
                attempts: Arc::clone(&attempts),
                request_bytes: Arc::clone(&request_bytes),
                prefer: "answer",
            })],
            Arc::clone(&key),
            crate::decision_engine::prediction::outbound::OutboundSecrets::default(),
            crate::decision_engine::prediction::factory::SlotLimits::default(),
        );
        let advisory = DecisionAdvisory::new(Arc::clone(&model_config), Some(factory));
        let limits = advisory.request_limits();
        let input = analysis_input(&model_config, &key, limits);
        let scope = Arc::clone(&input.accounting);
        let before = scope.remaining_budgets();
        let analysis = advisory.analyze(input, &cancellation).await;
        assert!(analysis.is_some(), "{model}: analysis runs without a turn service");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{model}: cache miss; analysis: {analysis:?}; factory: {:?}",
            advisory.factory().map(DecisionBackendFactory::status)
        );
        // The attempt is charged to the input's scope: the reported 1,200
        // tokens at 0.25 micro-USD per token settle at 300 micro-USD.
        let after = scope.remaining_budgets();
        assert_eq!(before.tokens - after.tokens, 1_200, "{model}: tokens charged");
        assert_eq!(before.micro_usd - after.micro_usd, 300, "{model}: money charged");
        assert!(
            request_bytes
                .lock()
                .iter()
                .all(|bytes| *bytes <= limits.max_request_bytes as usize),
            "{model}: the input's request limits bound the backend request"
        );
        assert!(
            advisory
                .factory()
                .is_some_and(|factory| factory.prediction_port_calls() == 1)
        );
    }
    // The builtin factory begins no accounting attempt.
    let builtin = DecisionBackendFactory::from_config(
        &config,
        crate::decision_engine::prediction::factory::FactoryDeps {
            catalog: &crate::providers::catalog::CatalogView::from_config(&config),
            key: Some(Arc::clone(&key)),
            transports: &crate::decision_engine::prediction::compatible_http::HttpDecisionTransports,
            config_generation: 1,
            events: Arc::new(crate::observability::causal_event::testing::RecordingSink::default()),
        },
    )
    .expect("test: builtin factory");
    let advisory = DecisionAdvisory::new(Arc::clone(&config), Some(builtin));
    let input = analysis_input(&config, &key, advisory.request_limits());
    let accounting = Arc::clone(&input.accounting);
    let before = accounting.remaining_budgets();
    assert!(advisory.analyze(input, &cancellation).await.is_some());
    assert_eq!(accounting.remaining_budgets(), before, "builtin spends nothing");
}

/// R10 L1: a forced drop on workspace A shows only in A's status row.
#[tokio::test]
async fn forced_drop_is_reported_only_for_its_workspace() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut queues = Vec::new();
    let mut releases = Vec::new();
    for workspace in [a.path(), b.path()] {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (observations, worker) = ObserveQueue::start(1, 10, CancellationToken::new(), || {
            Ok(Box::new(GatedWriter {
                entered: Some(entered_tx),
                release: release_rx,
            }))
        })
        .unwrap();
        observations.register_status(workspace);
        if workspace == a.path() {
            // One outcome blocks the writer, one fills the queue, the third drops.
            observations.observe(
                TurnOutcome::new(
                    None,
                    vec![],
                    crate::decision_engine::TurnResultClass::Completed,
                    None,
                    0,
                )
                .unwrap(),
            );
            entered_rx.await.unwrap();
            observations.observe(
                TurnOutcome::new(
                    None,
                    vec![],
                    crate::decision_engine::TurnResultClass::Completed,
                    None,
                    0,
                )
                .unwrap(),
            );
            assert_eq!(
                observations.observe(
                    TurnOutcome::new(
                        None,
                        vec![],
                        crate::decision_engine::TurnResultClass::Completed,
                        None,
                        0
                    )
                    .unwrap()
                ),
                crate::decision_engine::ObserveAdmission::Dropped
            );
        }
        queues.push((observations, worker));
        releases.push(release_tx);
    }
    let status = |path: &std::path::Path| {
        crate::decision_engine::status::snapshot(&Config {
            workspace_dir: path.into(),
            ..Default::default()
        })
    };
    // Both queues stay alive (and registered) while the status is read.
    assert_eq!(queues.len(), 2);
    assert_eq!(status(a.path()).dropped, 1);
    assert_eq!(status(b.path()).dropped, 0);
    for release in releases {
        for _ in 0..3 {
            let _ = release.send(());
        }
    }
}

/// R10 L2: a late telemetry drain fails with exactly one warning, also with
/// causal persistence off.
#[tokio::test(start_paused = true)]
async fn late_advisory_drain_warns_exactly_once() {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let subscriber = tracing_subscriber::layer::SubscriberExt::with(
        tracing_subscriber::registry(),
        WarnCounter {
            needle: "decision engine telemetry did not drain",
            count: Arc::clone(&count),
        },
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    let temp = tempfile::tempdir().unwrap();
    let mut config = Config {
        workspace_dir: temp.path().into(),
        ..Default::default()
    };
    config.decision_engine.enabled = true;
    config.causal_tree.enabled = false;
    let advisory = DecisionAdvisory::new(Arc::new(config), None);
    let cancellation = CancellationToken::new();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (observations, worker) = ObserveQueue::start(2, 10, cancellation.clone(), || {
        Ok(Box::new(GatedWriter {
            entered: Some(entered_tx),
            release: release_rx,
        }))
    })
    .unwrap();
    observations.observe(
        TurnOutcome::new(
            None,
            vec![],
            crate::decision_engine::TurnResultClass::Completed,
            None,
            0,
        )
        .unwrap(),
    );
    entered_rx.await.unwrap();
    assert!(
        advisory
            .telemetry
            .set(AdvisoryRuntime {
                engine: crate::decision_engine::causal_adapter::CausalEngine { observations },
                cancellation,
                worker: tokio::sync::Mutex::new(worker),
            })
            .is_ok()
    );
    assert!(
        !advisory
            .drain_or_warn(std::time::Instant::now() + std::time::Duration::from_millis(20))
            .await
    );
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    release_tx.send(()).unwrap();
}

/// R10 L3: `pre` mode reports the effective cap 0.
#[test]
fn pre_mode_status_reports_effective_cap_zero() {
    let mut config = Config::default();
    config.decision_engine.enabled = true;
    config.decision_engine.mode = DecisionMode::Pre;
    config.decision_engine.telemetry_cap = 1000;
    assert_eq!(crate::decision_engine::status::snapshot(&config).telemetry_cap, 0);
    config.decision_engine.mode = DecisionMode::Both;
    assert_eq!(crate::decision_engine::status::snapshot(&config).telemetry_cap, 1000);
}

/// R13: from an `AdvisoryInput` alone (no turn service) the driver generates
/// through `AdvisoryProbe`, so a candidate the hard gates block (a write tool
/// in a read-only turn, G9) still reaches the `IntentAnalysis`.
#[tokio::test]
async fn engine_driver_generates_with_advisory_probe() {
    use crate::tools::traits::Tool;
    let temp = tempfile::tempdir().unwrap();
    let config = enabled_config(temp.path());
    let key = crate::decision_engine::state::fixtures::key();
    let cancellation = CancellationToken::new();
    let builtin = DecisionBackendFactory::from_config(
        &config,
        crate::decision_engine::prediction::factory::FactoryDeps {
            catalog: &crate::providers::catalog::CatalogView::from_config(&config),
            key: Some(Arc::clone(&key)),
            transports: &crate::decision_engine::prediction::compatible_http::HttpDecisionTransports,
            config_generation: 1,
            events: Arc::new(crate::observability::causal_event::testing::RecordingSink::default()),
        },
    )
    .expect("test: builtin factory");
    let advisory = DecisionAdvisory::new(Arc::clone(&config), Some(builtin));
    let mut input = analysis_input(&config, &key, advisory.request_limits());
    let surface = SurfacePolicy {
        direct_names: BTreeSet::from(["file_write".into()]),
        ..SurfacePolicy::default()
    };
    let security = Arc::new(crate::security::SecurityPolicy::default());
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::file_write::FileWriteTool::new(security))];
    let catalog =
        Arc::new(CapabilityCatalog::build(&registry, &Default::default(), &surface).expect("test: write-tool catalog"));
    let mut trusted = input.state.trusted.parts().clone();
    trusted.catalog_generation = *catalog.generation();
    trusted.side_effect_mode = crate::tools::capability::turn_state::SideEffectMode::ReadOnly;
    trusted.channel = None;
    trusted.exclusions.clear();
    input.state.trusted = TrustedState::from_runtime(trusted);
    input.catalog_generation = *catalog.generation();
    input.catalog = catalog;
    input.publication_policy = surface;

    // The keys each probe admits into the analysed set.
    let view = input.catalog_view();
    let (calls, approvals) = (BTreeMap::new(), BTreeMap::new());
    let replayed = BTreeSet::new();
    let gates = gate_inputs(&input.facts, &calls, approvals, &replayed);
    let probe = HardGatesProbe {
        state: &input.state,
        catalog: &view,
        gates: &gates,
    };
    let observed_latency = BTreeMap::new();
    let cost_model =
        crate::decision_engine::cost_model::RuntimeCostModel::new(&input.model, &config, &observed_latency);
    let discovered = DiscoveredRefs::empty();
    let policy = CandidatePolicy {
        key: &key,
        max_candidates: usize::from(config.causal_tree.policy.max_candidates),
        can_deliver_text: true,
        may_delegate: false,
        delegation_target: None,
        cost_model: &cost_model,
        discovered_refs: &discovered,
    };
    let keys = |set: CandidateSet| match set {
        CandidateSet::Legal { candidates, .. } => candidates
            .iter()
            .map(|candidate| candidate.candidate_id().as_str().to_owned())
            .collect::<BTreeSet<_>>(),
        CandidateSet::NoLegalPath { .. } => BTreeSet::new(),
    };
    let execution = keys(CausalCandidateGenerator::generate(&input.state, &view, &policy, &probe).unwrap());
    let advised = keys(
        CausalCandidateGenerator::generate(&input.state, &view, &policy, &AdvisoryProbe { inner: &probe }).unwrap(),
    );
    let blocked: Vec<_> = advised.difference(&execution).cloned().collect();
    assert!(
        blocked.iter().any(|id| id.starts_with("use_tool-")),
        "the read-only turn blocks the write tool: {advised:?} vs {execution:?}"
    );
    drop(view);

    let analysis = advisory
        .analyze(input, &cancellation)
        .await
        .expect("test: analysis without a turn service");
    for id in &blocked {
        assert!(
            analysis.candidates.contains_key(id),
            "blocked candidate {id} reaches the analysis: {:?}",
            analysis.candidates.keys()
        );
    }
}

/// P1-04: a turn's observation never waits for telemetry I/O. On a
/// current-thread runtime nothing else runs while `observe` executes, so the
/// store is provably not opened on the turn's path; the background opening
/// admits the outcome and shutdown drain waits for it and flushes it.
#[tokio::test(flavor = "current_thread")]
async fn observation_never_waits_for_the_telemetry_open() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = (*enabled_config(temp.path())).clone();
    config.decision_engine.mode = DecisionMode::Both;
    let advisory = DecisionAdvisory::new(Arc::new(config), None);
    advisory.observe(
        ExecutionTraceSnapshot::default,
        Some("decision-1".into()),
        completed_end(),
    );
    assert!(!advisory.initialized(), "observe returned before any telemetry open");
    assert!(!temp.path().join("decision-engine.sqlite").exists());
    assert!(
        advisory
            .drain(std::time::Instant::now() + std::time::Duration::from_secs(5))
            .await,
        "drain waits for the background opening and the worker"
    );
    assert!(advisory.initialized());
    let store = SqliteTelemetryWriter::open(&temp.path().join("decision-engine.sqlite")).unwrap();
    assert_eq!(
        store.outcomes(100_000).unwrap().len(),
        1,
        "the outcome was admitted and flushed"
    );
}

/// P1-10: before the telemetry store is open, admission is bounded and never
/// waits. On a current-thread runtime the single opener task cannot run while
/// the test does not yield, so it stays stalled for every observation below:
/// the queue holds exactly the `observe_queue` bound, the overflow is
/// counted, and each observation returns without yielding. Once the
/// opener runs, the queued outcomes reach the store and the overflow reaches
/// the store's existing drop accounting.
#[tokio::test(flavor = "current_thread")]
async fn stalled_opener_leaves_a_bounded_queue_and_counts_the_overflow() {
    const BOUND: u32 = 4;
    const OBSERVED: u64 = 11;
    let temp = tempfile::tempdir().unwrap();
    let mut config = (*enabled_config(temp.path())).clone();
    config.decision_engine.mode = DecisionMode::Both;
    config.decision_engine.observe_queue = BOUND;
    let advisory = DecisionAdvisory::new(Arc::new(config), None);
    // `observe` is synchronous: each observation returns without yielding,
    // so the opener cannot run in between.
    for index in 0..OBSERVED {
        advisory.observe(
            ExecutionTraceSnapshot::default,
            Some(format!("decision-{index}")),
            completed_end(),
        );
    }
    assert!(!advisory.initialized(), "the opener has not run");
    assert!(!temp.path().join("decision-engine.sqlite").exists());
    {
        let pending = advisory.pending.lock();
        let PendingTelemetry::Opening { outcomes, dropped } = &*pending else {
            panic!("the first observation starts the opener");
        };
        assert!(advisory.opener.lock().is_some(), "one opener task is running");
        assert_eq!(outcomes.max_capacity(), BOUND as usize, "the ObserveQueue bound");
        assert_eq!(outcomes.capacity(), 0, "the queue holds exactly its bound");
        assert_eq!(*dropped, OBSERVED - u64::from(BOUND), "the overflow is counted");
    }
    assert!(
        advisory
            .drain(std::time::Instant::now() + std::time::Duration::from_secs(5))
            .await,
        "drain waits for the opener and the worker"
    );
    assert!(matches!(*advisory.pending.lock(), PendingTelemetry::Opened));
    let runtime = advisory.telemetry.get().unwrap();
    assert_eq!(
        runtime
            .engine
            .observations
            .counters
            .dropped
            .load(std::sync::atomic::Ordering::SeqCst),
        OBSERVED - u64::from(BOUND),
        "the overflow is in the existing drop accounting"
    );
    let store = SqliteTelemetryWriter::open(&temp.path().join("decision-engine.sqlite")).unwrap();
    assert_eq!(
        store.outcomes(100_000).unwrap().len(),
        BOUND as usize,
        "every queued outcome is flushed"
    );
}

/// P1-10 r3: drain during the pre-open handover. The store's opening is held
/// by a pre-initialising task so every outcome queues before the store
/// exists; then a thread holds the admission lock so the opener has the open
/// store but cannot move the queue (the stall between open and transfer).
/// Drains inside that window report failure without cancelling the writer,
/// also after a timed-out drain; once the transfer runs, drain succeeds and
/// every queued outcome is either stored or counted as dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn drain_during_the_pre_open_handover_waits_for_the_transfer() {
    const BOUND: u32 = 4;
    const OBSERVED: u64 = 7;
    let temp = tempfile::tempdir().unwrap();
    let mut config = (*enabled_config(temp.path())).clone();
    config.decision_engine.mode = DecisionMode::Both;
    config.decision_engine.observe_queue = BOUND;
    let config = Arc::new(config);
    let advisory = DecisionAdvisory::new(Arc::clone(&config), None);

    // Stall the opening: the cell's initialiser waits for `open_gate`.
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
    let (open_tx, open_rx) = tokio::sync::oneshot::channel::<()>();
    let cell = Arc::clone(&advisory.telemetry);
    let init_config = Arc::clone(&config);
    let initialiser = tokio::spawn(async move {
        cell.get_or_init(|| async move {
            entered_tx.send(()).unwrap();
            open_rx.await.unwrap();
            DecisionAdvisory::start_runtime(&init_config).await
        })
        .await;
    });
    entered_rx.await.unwrap();

    for index in 0..OBSERVED {
        advisory.observe(
            ExecutionTraceSnapshot::default,
            Some(format!("decision-{index}")),
            completed_end(),
        );
    }
    {
        let pending = advisory.pending.lock();
        let PendingTelemetry::Opening { dropped, .. } = &*pending else {
            panic!("every outcome is admitted before the store is open");
        };
        assert_eq!(*dropped, OBSERVED - u64::from(BOUND));
    }
    assert!(advisory.opener.lock().is_some());

    // Hold the admission lock: the opener can open but not transfer.
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let admission = Arc::clone(&advisory.pending);
    let holder = std::thread::spawn(move || {
        let _guard = admission.lock();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.await.unwrap();
    open_tx.send(()).unwrap();
    initialiser.await.unwrap();
    assert!(advisory.initialized(), "the store is open, the transfer is stalled");
    let runtime = advisory.telemetry.get().unwrap();

    assert!(
        !advisory
            .drain(std::time::Instant::now() + std::time::Duration::from_millis(100))
            .await,
        "drain cannot succeed before the queued outcomes are transferred"
    );
    assert!(
        !runtime.cancellation.is_cancelled(),
        "the writer stays open while the transfer is pending"
    );
    assert!(
        !advisory
            .drain(std::time::Instant::now() + std::time::Duration::from_millis(100))
            .await,
        "a drain after a timed-out drain still waits for the same opener"
    );
    assert!(!runtime.cancellation.is_cancelled());
    assert!(advisory.opener.lock().is_some(), "a timeout detaches nothing");

    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert!(
        advisory
            .drain(std::time::Instant::now() + std::time::Duration::from_secs(10))
            .await,
        "drain succeeds once the transfer and the worker are done"
    );
    assert!(matches!(*advisory.pending.lock(), PendingTelemetry::Opened));
    assert!(runtime.cancellation.is_cancelled());
    let dropped = runtime
        .engine
        .observations
        .counters
        .dropped
        .load(std::sync::atomic::Ordering::SeqCst);
    let store = SqliteTelemetryWriter::open(&temp.path().join("decision-engine.sqlite")).unwrap();
    let stored = store.outcomes(100_000).unwrap().len() as u64;
    assert_eq!(stored + dropped, OBSERVED, "no queued outcome is lost");
    assert_eq!(stored, u64::from(BOUND), "every queued outcome is stored");
}

/// A completed turn without usage, for observation cells.
fn completed_end() -> super::TurnEnd<'static> {
    super::TurnEnd {
        result: crate::decision_engine::TurnResultClass::Completed,
        duration_ms: 1,
        usage: None,
    }
}
