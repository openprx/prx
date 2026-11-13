use super::*;
use crate::decision_engine::observe::TelemetryWriter;

struct GatedWriter {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}

impl TelemetryWriter for GatedWriter {
    fn append_fifo(&mut self, _: TurnOutcome, _: usize) -> anyhow::Result<()> {
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
        observations.observe(TurnOutcome::new(None, vec![], true).unwrap()),
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

/// Answers every compiled request with a plain Answer and records what the
/// factory hands the backend: the accounting owner and the request limits.
pub(crate) struct RecordingBackend {
    pub(crate) identity: crate::cost::decision_attempt::BackendIdentity,
    pub(crate) key: Arc<DecisionKey>,
    pub(crate) attempts: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) request_bytes: Arc<parking_lot::Mutex<Vec<usize>>>,
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
        if accounting.is_some() {
            self.attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.request_bytes.lock().push(body.bytes().len());
        self.evaluate(request, body, deadline).await
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
        let answer = crate::agent::agent::native_decision_fixture::decision_answers_for_exact(&wire, "answer", "none")
            .map_err(|_| DecisionError::Unavailable)?;
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
        predicted: None,
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
        },
        resume_evidence: None,
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
    let config = enabled_config(temp.path());
    let key = crate::decision_engine::state::fixtures::key();
    let cancellation = CancellationToken::new();
    for model in ["recording-model", "typesafe/jev-1.13"] {
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
            })],
            Arc::clone(&key),
            crate::decision_engine::prediction::outbound::OutboundSecrets::default(),
            crate::decision_engine::prediction::factory::SlotLimits::default(),
        );
        let advisory = DecisionAdvisory::new(Arc::clone(&config), Some(factory));
        let limits = advisory.request_limits();
        let analysis = advisory
            .analyze(analysis_input(&config, &key, limits), &cancellation)
            .await;
        assert!(analysis.is_some(), "{model}: analysis runs without a turn service");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{model}: cache miss"
        );
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
            observations.observe(TurnOutcome::new(None, vec![], true).unwrap());
            entered_rx.await.unwrap();
            observations.observe(TurnOutcome::new(None, vec![], true).unwrap());
            assert_eq!(
                observations.observe(TurnOutcome::new(None, vec![], true).unwrap()),
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
    observations.observe(TurnOutcome::new(None, vec![], true).unwrap());
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
