// Included in decision_engine::tests so card test names remain exact.
use crate::decision_engine::config::{DecisionEngineConfig, DecisionMode};
use crate::providers::{ChatMessage, ChatRequest, ChatResponse, Provider, ToolCall};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

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
        let mut requests = self.requests.lock().unwrap();
        let first = requests.is_empty();
        requests.push(serde_json::json!({"messages": request.messages, "tools": request.tools}));
        Ok(ChatResponse {
            text: Some(if first { "" } else { "done" }.into()),
            reasoning_content: None,
            tool_calls: if first {
                vec![ToolCall {
                    id: "call_a".into(),
                    name: "advice_probe".into(),
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
        "advice_probe"
    }
    fn description(&self) -> &str {
        "Records one admitted dispatch"
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
    _directory: tempfile::TempDir,
}
async fn exercise(config: Option<DecisionEngineConfig>, advice: Option<IntentAnalysis>) -> TurnProbe {
    use crate::agent::turn_request::*;
    let directory = tempfile::tempdir().unwrap();
    let mut runtime = crate::config::Config {
        workspace_dir: directory.path().to_path_buf(),
        ..Default::default()
    };
    runtime.autonomy.level = crate::security::AutonomyLevel::Full;
    if let Some(config) = config {
        runtime.decision_engine = config;
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(vec![Box::new(DispatchSpy(Arc::clone(&calls)))]);
    let admission =
        crate::agent::test_admission::AdmittedTurn::with_config(runtime, "Use advice_probe.", Arc::clone(&tools)).await;
    let provider = RequestSpy {
        store: Some(directory.path().join("decision-engine.sqlite")),
        ..Default::default()
    };
    let mut history = vec![
        ChatMessage::system("system policy"),
        ChatMessage::user("Use advice_probe."),
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
    drop(admission);
    TurnProbe {
        requests: provider.requests.into_inner().unwrap(),
        trace,
        store,
        dispatches: calls.load(Ordering::SeqCst),
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
    assert_eq!(before.requests[0]["tools"], after.requests[0]["tools"]);
    assert_eq!(
        crate::agent::prompt::prompt_sha256(&before.requests[0]["tools"].to_string()),
        crate::agent::prompt::prompt_sha256(&after.requests[0]["tools"].to_string())
    );
    assert_eq!(before.dispatches, 1);
    assert_eq!(after.dispatches, 1);
    assert!(!before.store.exists());
    assert!(!after.store.exists());
    assert!(after.trace.decision_brief_sha256.is_none());
}
#[tokio::test]
async fn pre_mode_brief_is_bounded_redacted_deterministic_and_traced() {
    let first = exercise(Some(enabled(DecisionMode::Pre)), Some(long_analysis())).await;
    let second = exercise(Some(enabled(DecisionMode::Pre)), Some(long_analysis())).await;
    let messages = first.requests[0]["messages"].as_array().unwrap();
    let brief = messages.iter().rev().find(|m| m["role"] == "system").unwrap()["content"]
        .as_str()
        .unwrap();
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
    assert!(probe.trace.decision_brief_sha256.is_none());
    assert!(
        !serde_json::to_string(&probe.requests)
            .unwrap()
            .contains("Decision advice")
    );
    let records = recorded(&probe).await;
    assert_eq!(records.len(), 1);
    assert!(records[0].succeeded());
    assert!(records[0].decision_id().is_none());
    assert_eq!(probe.dispatches, 1);
}
#[tokio::test]
async fn both_mode_links_outcome_to_analysis_decision_id() {
    let probe = exercise(Some(enabled(DecisionMode::Both)), Some(long_analysis())).await;
    assert!(probe.trace.decision_brief_sha256.is_some());
    assert_eq!(recorded(&probe).await[0].decision_id(), Some("decision_a"));
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
