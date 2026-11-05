#![cfg(test)]

use super::*;
use crate::decision::*;
use observe::{ObserveQueue, SqliteTelemetryWriter, TelemetryWriter};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn causal_adapter_maps_all_ten_questions() {
    let key = CandidateKey::from_parts("read", "file", None);
    let mut answers = BTreeMap::new();
    for q in GlobalQuestion::ALL {
        let answer = match q {
            GlobalQuestion::NextAction | GlobalQuestion::CapabilityTarget => DecisionAnswer::Choice(ChoiceAnswer {
                chosen: OptionId::code("read"),
                probabilities: BTreeMap::from([(OptionId::code("read"), Probability::new(1.0).unwrap())]),
                confidence: Probability::new(0.8).unwrap(),
            }),
            GlobalQuestion::SemanticRisk | GlobalQuestion::ExpectedGain => score(),
            _ => probability(),
        };
        answers.insert(QuestionId::global(q), answer);
    }
    for q in CandidateQuestion::ALL {
        answers.insert(
            QuestionId::candidate(&key, q),
            match q {
                CandidateQuestion::Risk | CandidateQuestion::Gain => score(),
                _ => probability(),
            },
        );
    }
    let engine = causal_adapter::CausalEngine {
        observations: ObserveQueue::disabled(),
    };
    let analysis = engine
        .analyze(IntentInput {
            decision_id: "decision_a",
            answers: &answers,
            candidates: std::slice::from_ref(&key),
        })
        .await
        .unwrap();
    assert_eq!(analysis.decision_id, "decision_a");
    assert_eq!(analysis.global.len(), 6);
    let candidate_answers = analysis
        .candidates
        .get(key.as_str())
        .unwrap_or_else(|| panic!("missing candidate key {}", key.as_str()));
    assert_eq!(candidate_answers.len(), 4);
    for q in GlobalQuestion::ALL {
        let value = analysis
            .global
            .get(q.as_str())
            .unwrap_or_else(|| panic!("missing global question key {}", q.as_str()));
        match q {
            GlobalQuestion::NextAction | GlobalQuestion::CapabilityTarget => assert_eq!(
                value,
                &IntentValue::Choice {
                    probabilities: BTreeMap::from([("read".into(), 1.0)]),
                    confidence: 0.8
                }
            ),
            GlobalQuestion::SemanticRisk | GlobalQuestion::ExpectedGain => assert_eq!(value, &expected_score()),
            _ => assert_eq!(value, &IntentValue::Probability(0.7)),
        }
    }
    for q in CandidateQuestion::ALL {
        assert_eq!(
            candidate_answers
                .get(q.as_str())
                .unwrap_or_else(|| panic!("missing candidate question key {}", q.as_str())),
            &match q {
                CandidateQuestion::Risk | CandidateQuestion::Gain => expected_score(),
                _ => IntentValue::Probability(0.7),
            }
        );
    }
    assert_eq!(std::mem::size_of::<noop::NoopEngine>(), 0);
}
fn probability() -> DecisionAnswer {
    DecisionAnswer::Noul(NoulAnswer {
        probability_true: Probability::new(0.7).unwrap(),
    })
}
fn score() -> DecisionAnswer {
    DecisionAnswer::Score(ScoreAnswer {
        distribution: vec![Probability::new(0.25).unwrap(), Probability::new(0.75).unwrap()],
        expected: 0.75,
        legend: vec![],
        confidence: Probability::new(0.9).unwrap(),
    })
}
fn expected_score() -> IntentValue {
    IntentValue::Score {
        distribution: vec![0.25, 0.75],
        expected: 0.75,
        confidence: 0.9,
    }
}
fn outcome(id: &str) -> TurnOutcome {
    TurnOutcome::new(Some(id.into()), vec!["file_read".into()], true).unwrap()
}

#[tokio::test]
async fn observe_queue_full_drops_and_counts_without_blocking() {
    let cancel = CancellationToken::new();
    struct GatedWriter {
        started: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        written: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }
    impl TelemetryWriter for GatedWriter {
        fn append_fifo(&mut self, _: TurnOutcome, _: usize) -> anyhow::Result<()> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
                self.release.recv()?;
            }
            self.written.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let written = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (queue, worker) = ObserveQueue::start(1, 10, cancel.clone(), || {
        Ok(Box::new(GatedWriter {
            started: Some(started),
            release: gate,
            written: std::sync::Arc::clone(&written),
        }))
    })
    .unwrap();
    assert_eq!(queue.observe(outcome("a")), ObserveAdmission::Accepted);
    // A blocking writer must leave this single-thread async runtime responsive.
    ready.await.unwrap();
    assert_eq!(queue.observe(outcome("b")), ObserveAdmission::Accepted);
    assert_eq!(queue.observe(outcome("c")), ObserveAdmission::Dropped);
    assert_eq!(queue.counters.dropped.load(Ordering::Relaxed), 1);
    cancel.cancel();
    release.send(()).unwrap();
    worker.unwrap().await.unwrap();
    assert_eq!(
        written.load(Ordering::Relaxed),
        2,
        "cancellation must drain accepted outcomes"
    );
    assert_eq!(queue.observe(outcome("closed")), ObserveAdmission::Dropped);
    assert_eq!(queue.counters.dropped.load(Ordering::Relaxed), 2);
    let (disabled, worker) = ObserveQueue::start(1, 0, CancellationToken::new(), || {
        panic!("zero cap must not open a store")
    })
    .unwrap();
    assert!(worker.is_none());
    assert_eq!(disabled.observe(outcome("c")), ObserveAdmission::Disabled);
    assert!(TurnOutcome::new(None, vec!["x".into(); 65], false).is_err());
    assert!(TurnOutcome::new(None, vec!["x".repeat(129)], false).is_err());
    struct FailingWriter;
    impl TelemetryWriter for FailingWriter {
        fn append_fifo(&mut self, _: TurnOutcome, _: usize) -> anyhow::Result<()> {
            anyhow::bail!("test store failure")
        }
    }
    let (queue, worker) = ObserveQueue::start(1, 1, CancellationToken::new(), || Ok(Box::new(FailingWriter))).unwrap();
    let counters = std::sync::Arc::clone(&queue.counters);
    assert_eq!(queue.observe(outcome("error")), ObserveAdmission::Accepted);
    drop(queue);
    worker.unwrap().await.unwrap();
    assert_eq!(counters.store_errors.load(Ordering::Relaxed), 1);
}
#[tokio::test]
async fn observe_evicts_fifo_at_telemetry_cap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outcomes.sqlite");
    let (queue, worker) = ObserveQueue::start(4, 2, CancellationToken::new(), || {
        Ok(Box::new(SqliteTelemetryWriter::open(&path)?))
    })
    .unwrap();
    for id in ["a", "b", "c"] {
        assert_eq!(queue.observe(outcome(id)), ObserveAdmission::Accepted);
    }
    drop(queue);
    worker.unwrap().await.unwrap();
    let mut store = SqliteTelemetryWriter::open(&path).unwrap();
    assert_eq!(store.outcomes().unwrap(), vec![outcome("b"), outcome("c")]);
    store.append_fifo(outcome("d"), 2).unwrap();
    assert_eq!(store.outcomes().unwrap(), vec![outcome("c"), outcome("d")]);
}

#[test]
fn observe_without_runtime_returns_typed_error_before_opening_store() {
    let result = ObserveQueue::start(1, 1, CancellationToken::new(), || panic!("must not open"));
    assert!(matches!(result, Err(observe::ObserveStartError::Runtime(_))));
}

#[test]
fn dedicated_observation_worker_does_not_hold_runtime_shutdown() {
    struct Writer;
    impl TelemetryWriter for Writer {
        fn append_fifo(&mut self, _: TurnOutcome, _: usize) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let cancel = CancellationToken::new();
    let (queue, worker) =
        runtime.block_on(async { ObserveQueue::start(1, 1, cancel.clone(), || Ok(Box::new(Writer))).unwrap() });
    let (done, shutdown) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        drop(runtime);
        done.send(()).unwrap();
    });
    let result = shutdown.recv_timeout(std::time::Duration::from_secs(2));
    cancel.cancel();
    drop(queue);
    thread.join().unwrap();
    assert!(result.is_ok(), "idle telemetry must not hold runtime shutdown");
    assert!(worker.unwrap().blocking_recv().is_ok());
}

#[test]
fn disabled_direct_backend_timeout_obeys_legacy_upper_bound() {
    let mut config = config::DecisionEngineConfig {
        backend: Some(Default::default()),
        ..Default::default()
    };
    for timeout in [9, 5_001, u32::MAX] {
        config.backend.as_mut().unwrap().timeout_ms = timeout;
        assert!(config.validate().is_err());
    }
    for timeout in [10, 200, 500] {
        config.backend.as_mut().unwrap().timeout_ms = timeout;
        assert!(config.validate().is_ok());
    }
}

#[test]
fn process_seal_matches_inherited_backend_and_rejects_changed_override() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config {
        config_path: dir.path().join("config.toml"),
        workspace_dir: dir.path().join("workspace"),
        ..Default::default()
    };
    config.causal_tree.backend.timeout_ms = 2_000;
    let generation = crate::config::new_shared(config.clone()).pin();
    let transport = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx-engine-seal-fixture");
    let key = crate::decision::DecisionKey::from_worker_channel(&transport, "prx-workspace", "prx-run", "prx-config")
        .unwrap();
    let binding = generation.bind_process(&key).unwrap();
    config.decision_engine.backend = Some(config.causal_tree.backend.clone());
    crate::config::ConfigGeneration::restore_process(config.clone(), &binding, &key).unwrap();
    let mut changed_advice = config.clone();
    changed_advice.decision_engine.prompt_budget_tokens += 1;
    assert!(crate::config::ConfigGeneration::restore_process(changed_advice, &binding, &key).is_err());
    config.decision_engine.backend.as_mut().unwrap().timeout_ms = 250;
    assert!(crate::config::ConfigGeneration::restore_process(config, &binding, &key).is_err());
}

#[test]
fn rendered_brief_is_bounded_redacted_deterministic() {
    let mut analysis = IntentAnalysis::default();
    for i in 0..100 {
        analysis
            .global
            .insert(format!("question_{i:03}"), IntentValue::Probability(0.7));
    }
    analysis.global.insert(
        "authorization: Bearer sk-test-private-credential-value".into(),
        IntentValue::Probability(0.5),
    );
    let brief = render::render_decision_brief(&analysis, 50, 1);
    assert_eq!(brief, render::render_decision_brief(&analysis, 50, 1));
    assert!(crate::agent::context_budget::count_tokens(&brief) <= 50);
    assert!(!brief.contains("sk-test-private-credential-value"));
    assert!(brief.contains("question_"));
    assert_eq!(render::render_decision_brief(&analysis, 0, 0), "");
}

use crate::decision_engine::config::{DecisionEngineConfig, DecisionMode};
use crate::providers::{ChatMessage, ChatRequest, ChatResponse, Provider, ToolCall};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct RequestSpy {
    requests: Mutex<Vec<serde_json::Value>>,
    store: Option<std::path::PathBuf>,
}
#[async_trait::async_trait]
impl Provider for RequestSpy {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("unexpected completion path")
    }
    async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        if let Some(store) = &self.store {
            assert!(!store.exists(), "observe must run after the final provider outcome");
        }
        let mut requests = self.requests.lock();
        let first = requests.is_empty();
        requests.push(serde_json::json!({"messages": request.messages, "tools": request.tools}));
        Ok(ChatResponse {
            text: Some(if first { "" } else { "done" }.into()),
            reasoning_content: None,
            tool_calls: if first {
                vec![ToolCall {
                    id: "call_a".into(),
                    name: "file_read".into(),
                    arguments: "{}".into(),
                }]
            } else {
                vec![]
            },
        })
    }
}
struct DispatchSpy(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl crate::tools::Tool for DispatchSpy {
    fn name(&self) -> &str {
        "file_read"
    }
    fn description(&self) -> &str {
        "Records one admitted dispatch"
    }
    fn categories(&self) -> &'static [crate::tools::ToolCategory] {
        &[crate::tools::ToolCategory::FileSystem]
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(crate::tools::ToolResult {
            success: true,
            output: "probe complete".into(),
            error: None,
        })
    }
}
struct TurnProbe {
    requests: Vec<serde_json::Value>,
    trace: crate::agent::turn_contracts::ToolLoopTrace,
    store: std::path::PathBuf,
    dispatches: usize,
    store_opens: usize,
    credential_reads: usize,
    _directory: tempfile::TempDir,
}
async fn exercise(config: Option<DecisionEngineConfig>, advice: Option<IntentAnalysis>) -> TurnProbe {
    use crate::agent::turn_request::*;
    let directory = tempfile::tempdir().unwrap();
    let spy = access_spy::AccessSpy::new(directory.path());
    let mut runtime = crate::config::Config {
        workspace_dir: directory.path().to_path_buf(),
        ..Default::default()
    };
    runtime.autonomy.level = crate::security::AutonomyLevel::Full;
    // Give the builtin selector a read-only filesystem identity and concrete
    // path intent; a generic native spy abstains at the commit threshold.
    // Publish the spy directly and retain real admission and execution gates.
    runtime.tool_tiering.always_include.push("file_read".into());
    if let Some(config) = config {
        runtime.decision_engine = config;
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(vec![Box::new(DispatchSpy(Arc::clone(&calls)))]);
    let admission = crate::agent::test_admission::AdmittedTurn::with_config(
        runtime,
        "Use file_read to inspect ./fixture.txt.",
        Arc::clone(&tools),
    )
    .await;
    let provider = RequestSpy {
        store: Some(directory.path().join("decision-engine.sqlite")),
        ..Default::default()
    };
    let mut history = vec![
        ChatMessage::system("system policy"),
        ChatMessage::user("Use file_read to inspect ./fixture.txt."),
    ];
    let (_, trace) = crate::agent::turn_engine::run_turn(
        &mut history,
        TurnRequest {
            advice,
            channel_name: "cli",
            scope: None,
            mode: Default::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: TurnSchedule {
                read_only_tool_concurrency_window: 1,
                priority_scheduling_enabled: false,
                low_priority_tool_names: vec![],
            },
            compaction: None,
            decision: admission.decision(),
        },
        TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry: tools,
            observer: &crate::observability::NoopObserver {},
            hooks: &crate::hooks::HookManager::new(directory.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .unwrap();
    let store = directory.path().join("decision-engine.sqlite");
    assert!(
        admission
            .decision()
            .service
            .drain_writes(std::time::Instant::now() + std::time::Duration::from_secs(2))
            .await
    );
    drop(admission);
    TurnProbe {
        requests: provider.requests.into_inner(),
        trace,
        store,
        dispatches: calls.load(Ordering::SeqCst),
        store_opens: spy.counts.stores.load(Ordering::SeqCst),
        credential_reads: spy.counts.credentials.load(Ordering::SeqCst),
        _directory: directory,
    }
}
fn enabled(mode: DecisionMode) -> DecisionEngineConfig {
    DecisionEngineConfig {
        enabled: true,
        mode,
        prompt_budget_tokens: 50,
        ..Default::default()
    }
}
fn long_analysis() -> IntentAnalysis {
    let mut analysis = IntentAnalysis {
        decision_id: "decision_a".into(),
        ..Default::default()
    };
    analysis.global.insert(
        "authorization: Bearer sk-test-private-credential-value".into(),
        IntentValue::Probability(0.5),
    );
    for i in 0..100 {
        analysis
            .global
            .insert(format!("question_{i:03}"), IntentValue::Probability(0.7));
    }
    analysis
}
async fn recorded(probe: &TurnProbe) -> Vec<TurnOutcome> {
    // Observe is nonblocking; wait for the real writer's committed transaction.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if probe.store.exists() {
            let store = SqliteTelemetryWriter::open(&probe.store).unwrap();
            let records = store.outcomes().unwrap();
            if !records.is_empty() {
                return records;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "observation worker did not persist outcome"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}
#[tokio::test]
async fn disabled_engine_is_byte_identical_in_one_build() {
    let before = exercise(None, None).await;
    let after = exercise(
        Some(DecisionEngineConfig {
            enabled: false,
            ..Default::default()
        }),
        None,
    )
    .await;
    assert_eq!(
        serde_json::to_vec(&before.requests).unwrap(),
        serde_json::to_vec(&after.requests).unwrap()
    );
    let before_tools = before
        .requests
        .get(0)
        .and_then(|request| request.get("tools"))
        .expect("disabled_engine_is_byte_identical_in_one_build: baseline request tools missing");
    let after_tools = after
        .requests
        .get(0)
        .and_then(|request| request.get("tools"))
        .expect("disabled_engine_is_byte_identical_in_one_build: disabled request tools missing");
    assert_eq!(before_tools, after_tools);
    let tools = before_tools.as_array().expect("baseline tools must be published");
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(serde_json::Value::as_str) == Some("file_read"))
    );
    assert_eq!(
        crate::agent::prompt::prompt_sha256(&before_tools.to_string()),
        crate::agent::prompt::prompt_sha256(&after_tools.to_string())
    );
    assert_eq!(before.dispatches, 1, "provider requests: {:?}", before.requests);
    assert_eq!(after.dispatches, 1);
    assert_eq!(before.store_opens, 0);
    assert_eq!(after.store_opens, 0);
    // Kept as a disabled-path guard against future credential work. Current advisory
    // projection reads validated answers only; actual-site positive control lives in
    // access_spies_observe_actual_store_and_credential_sites.
    assert_eq!(before.credential_reads, 0);
    assert_eq!(after.credential_reads, 0);
    assert!(!before.store.exists());
    assert!(!after.store.exists());
    assert!(after.trace.decision_brief_sha256.is_none());
}
#[tokio::test]
async fn pre_mode_brief_is_bounded_redacted_deterministic_and_traced() {
    let first = exercise(Some(enabled(DecisionMode::Pre)), Some(long_analysis())).await;
    let second = exercise(Some(enabled(DecisionMode::Pre)), Some(long_analysis())).await;
    let messages = first
        .requests
        .get(0)
        .and_then(|request| request.get("messages"))
        .and_then(serde_json::Value::as_array)
        .expect("pre_mode_brief_is_bounded_redacted_deterministic_and_traced: request messages missing");
    let system_messages = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.get("role").and_then(serde_json::Value::as_str) == Some("system"))
        .collect::<Vec<_>>();
    assert_eq!(system_messages.len(), 1);
    let (system_index, system_message) = system_messages
        .get(0)
        .expect("pre_mode_brief_is_bounded_redacted_deterministic_and_traced: leading system message missing");
    assert_eq!(*system_index, 0);
    let system = system_message
        .get("content")
        .and_then(serde_json::Value::as_str)
        .expect("pre_mode_brief_is_bounded_redacted_deterministic_and_traced: system content missing");
    let expected_brief = render::render_decision_brief(&long_analysis(), 50, enabled(DecisionMode::Pre).candidates);
    assert!(system.ends_with(&expected_brief));
    let brief = system
        .get(system.len() - expected_brief.len()..)
        .expect("pre_mode_brief_is_bounded_redacted_deterministic_and_traced: brief suffix missing");
    assert!(crate::agent::context_budget::count_tokens(brief) <= 50);
    assert!(!brief.contains("sk-test-private-credential-value"));
    assert_eq!(first.requests, second.requests);
    assert_eq!(
        first.trace.decision_brief_sha256.as_deref(),
        Some(crate::agent::prompt::prompt_sha256(brief).as_str())
    );
    assert_eq!(first.dispatches, 1);
}
#[tokio::test]
async fn post_mode_records_outcome_without_brief() {
    let probe = exercise(Some(enabled(DecisionMode::Post)), None).await;
    assert_eq!(probe.store_opens, 1, "post mode must open the telemetry store once");
    assert!(probe.trace.decision_brief_sha256.is_none());
    assert!(
        !serde_json::to_string(&probe.requests)
            .unwrap()
            .contains("Decision advice")
    );
    let records = recorded(&probe).await;
    assert_eq!(records.len(), 1);
    let outcome = records
        .get(0)
        .expect("post_mode_records_outcome_without_brief: recorded outcome missing");
    assert!(outcome.succeeded());
    assert!(outcome.decision_id().is_none());
    assert_eq!(probe.dispatches, 1);
}
#[tokio::test]
async fn both_mode_links_outcome_to_analysis_decision_id() {
    let probe = exercise(Some(enabled(DecisionMode::Both)), Some(long_analysis())).await;
    assert_eq!(probe.store_opens, 1, "both mode must open the telemetry store once");
    assert!(probe.trace.decision_brief_sha256.is_some());
    let records = recorded(&probe).await;
    let outcome = records
        .get(0)
        .expect("both_mode_links_outcome_to_analysis_decision_id: recorded outcome missing");
    assert_eq!(outcome.decision_id(), Some("decision_a"));
    assert_eq!(probe.dispatches, 1);
}
#[tokio::test]
async fn zero_telemetry_cap_opens_no_store_and_still_renders_brief() {
    let probe = exercise(
        Some(DecisionEngineConfig {
            telemetry_cap: 0,
            ..enabled(DecisionMode::Both)
        }),
        Some(long_analysis()),
    )
    .await;
    assert!(probe.trace.decision_brief_sha256.is_some());
    assert!(!probe.store.exists());
    assert_eq!(probe.dispatches, 1);
}
struct AnalysisFailure {
    hang: bool,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl DecisionEngine for AnalysisFailure {
    async fn analyze(&self, _: IntentInput<'_>) -> Result<IntentAnalysis, EngineError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.hang {
            std::future::pending::<()>().await;
        }
        Err(EngineError::OutcomeBounds)
    }
    fn observe(&self, _: TurnOutcome) -> ObserveAdmission {
        ObserveAdmission::Disabled
    }
    fn identity(&self) -> &'static str {
        "test"
    }
}
async fn failed_analysis(hang: bool) -> Option<IntentAnalysis> {
    let engine = AnalysisFailure {
        hang,
        calls: AtomicUsize::new(0),
    };
    let start = tokio::time::Instant::now();
    let config = enabled(DecisionMode::Pre);
    let advice = turn::analyze(
        &engine,
        &config,
        IntentInput {
            decision_id: "decision_a",
            answers: &BTreeMap::new(),
            candidates: &[],
        },
        &CancellationToken::new(),
    )
    .await;
    assert!(tokio::time::Instant::now() - start <= std::time::Duration::from_millis(u64::from(config.timeout_ms)));
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    advice
}
#[tokio::test(start_paused = true)]
async fn analyze_timeout_delays_at_most_timeout_ms_and_tool_still_dispatches() {
    let advice = failed_analysis(true).await;
    assert!(advice.is_none());
    tokio::time::resume();
    let probe = exercise(Some(enabled(DecisionMode::Pre)), advice).await;
    assert_eq!(probe.dispatches, 1);
    assert!(probe.trace.decision_brief_sha256.is_none());
}
#[tokio::test]
async fn analyze_error_yields_no_brief_and_tool_still_dispatches() {
    let advice = failed_analysis(false).await;
    assert!(advice.is_none());
    let probe = exercise(Some(enabled(DecisionMode::Pre)), advice).await;
    assert_eq!(probe.dispatches, 1);
    assert!(probe.trace.decision_brief_sha256.is_none());
}

#[tokio::test]
async fn cancelled_analysis_is_not_polled() {
    let engine = AnalysisFailure {
        hang: true,
        calls: AtomicUsize::new(0),
    };
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let advice = turn::analyze(
        &engine,
        &enabled(DecisionMode::Pre),
        IntentInput {
            decision_id: "decision_a",
            answers: &BTreeMap::new(),
            candidates: &[],
        },
        &cancellation,
    )
    .await;
    assert!(advice.is_none());
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn render_top_k_uses_highest_expected_gain() {
    let mut analysis = IntentAnalysis::default();
    for (name, expected) in [("a_low", 0.1), ("z_high", 0.9)] {
        analysis.candidates.insert(
            name.into(),
            BTreeMap::from([(
                "gain".into(),
                IntentValue::Score {
                    distribution: vec![],
                    expected,
                    confidence: 1.0,
                },
            )]),
        );
    }
    let brief = render::render_decision_brief(&analysis, 2000, 1);
    assert!(brief.contains("z_high"));
    assert!(!brief.contains("a_low"));
}

#[tokio::test]
async fn pre_and_both_preserve_disabled_serving_tools_and_fingerprint() {
    let disabled = exercise(None, None).await;
    let baseline = disabled
        .requests
        .get(0)
        .and_then(|request| request.get("tools"))
        .expect("pre_and_both_preserve_disabled_serving_tools_and_fingerprint: disabled tools missing");
    assert!(baseline.is_array());
    for mode in [DecisionMode::Pre, DecisionMode::Both] {
        let enabled = exercise(Some(enabled(mode)), Some(long_analysis())).await;
        let tools = enabled
            .requests
            .get(0)
            .and_then(|request| request.get("tools"))
            .expect("pre_and_both_preserve_disabled_serving_tools_and_fingerprint: enabled tools missing");
        assert_eq!(tools, baseline);
        assert_eq!(
            crate::agent::prompt::prompt_sha256(&tools.to_string()),
            crate::agent::prompt::prompt_sha256(&baseline.to_string())
        );
    }
}

#[test]
fn access_spies_observe_actual_store_and_credential_sites() {
    use crate::providers::credentials::{ConfigCredentialSource, CredentialRef, CredentialSource};
    let directory = tempfile::tempdir().unwrap();
    let spy = access_spy::AccessSpy::new(directory.path());
    let config = crate::config::Config {
        workspace_dir: directory.path().to_owned(),
        ..Default::default()
    };
    SqliteTelemetryWriter::open(&directory.path().join("decision-engine.sqlite")).unwrap();
    ConfigCredentialSource::new(&config)
        .material(&CredentialRef::None)
        .unwrap();
    assert_eq!(spy.counts.stores.load(Ordering::SeqCst), 1);
    assert_eq!(spy.counts.credentials.load(Ordering::SeqCst), 1);
}
