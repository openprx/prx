#![cfg(test)]
//! Decision-free ingress model and publication admission. These tests reach the
//! production turn service (`AdmittedTurn`) and, for the refusal path, `run_turn`
//! itself, instead of rebuilding the surface by hand.
use super::*;
use crate::agent::turn_contracts::ChatMode;
use crate::agent::turn_engine::run_turn;
use crate::agent::turn_request::{TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
use crate::agent::turn_tests::admission::AdmittedTurn;
use crate::tools::{ToolExecutionCommand, ToolExecutionStatus};

fn workspace_config(workspace: &std::path::Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config
}

fn project_tool_registry(dispatches: &Arc<Mutex<Vec<String>>>, names: &[&str]) -> Arc<Vec<Box<dyn Tool>>> {
    Arc::new(
        names
            .iter()
            .map(|name| {
                Box::new(RecordingTool {
                    name: (*name).to_owned(),
                    execution_order: Arc::clone(dispatches),
                }) as Box<dyn Tool>
            })
            .collect(),
    )
}

struct SurfaceProvider {
    sends: AtomicUsize,
}
#[async_trait]
impl Provider for SurfaceProvider {
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("fixture requires a structured request")
    }
    async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        assert!(request.tools.expect("explicit publication").is_empty());
        Ok(ChatResponse {
            text: Some("ok".into()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

/// An execution model outside the ingress binding is refused by the real turn,
/// before any provider request or tool dispatch.
#[tokio::test]
async fn execution_model_differs_from_bound_model_is_refused() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry = project_tool_registry(&dispatches, &["project_tool"]);
    let admitted = AdmittedTurn::with_config(workspace_config(workspace.path()), "reply", Arc::clone(&registry)).await;
    let decision = admitted.decision();
    assert!(
        decision
            .service
            .validate_agent_execution_model(decision.proof, "ollama", "prx-local-model")
            .is_ok(),
        "the ingress binding must admit itself"
    );
    for (provider_name, model) in [("ollama", "prx-other-model"), ("openai", "prx-local-model")] {
        assert!(
            decision
                .service
                .validate_agent_execution_model(decision.proof, provider_name, model)
                .is_err(),
            "{provider_name}/{model} is outside the ingress binding"
        );
        let provider = NativeScriptedProvider::new(vec![ChatResponse {
            text: Some("done".into()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        }]);
        let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("reply")];
        let result = run_turn(
            &mut history,
            TurnRequest {
                advice: None,
                channel_name: "cli",
                scope: None,
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: TurnSchedule {
                    read_only_tool_concurrency_window: 1,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: None,
                decision: admitted.decision(),
            },
            TurnServices {
                provider: &provider,
                provider_name,
                model,
                temperature: 0.0,
                tools_registry: Arc::clone(&registry),
                observer: &crate::observability::NoopObserver,
                hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admitted.adapter()),
                memory: admitted.memory_runtime(),
            },
            TurnDelivery {
                cancellation: None,
                on_delta: None,
                on_tool_call: None,
            },
        )
        .await;
        assert!(result.is_err(), "run_turn accepted {provider_name}/{model}");
        assert!(provider.requests().is_empty(), "a refused model reached the provider");
    }
    assert!(dispatches.lock().unwrap().is_empty());
}

/// A model without verified native tools gets an empty surface from the
/// production builder, so the provider sees no tools and a guessed name is denied.
#[tokio::test]
async fn model_without_native_tools_gets_no_tool_surface() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = workspace_config(workspace.path());
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider":"ollama", "model":"prx-local-model", "operations":["chat"], "native_tools":false,
        }))
        .expect("fixture model descriptor"),
    );
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry = project_tool_registry(&dispatches, &["project_tool"]);
    let admitted = AdmittedTurn::with_config(config, "reply", registry).await;
    let decision = admitted.decision();
    let surface = decision.service.turn_tool_surface(decision.proof, &[]);
    assert!(!surface.contains("project_tool"));
    let adapter = admitted.adapter();
    let service = adapter
        .tool_execution_service
        .clone()
        .expect("fixture execution service");
    let specs: Vec<crate::tools::ToolSpec> = service
        .descriptors()
        .into_iter()
        .filter(|descriptor| surface.contains(&descriptor.public_name))
        .map(|descriptor| crate::tools::ToolSpec {
            name: descriptor.public_name,
            description: descriptor.description,
            parameters: descriptor.parameters,
        })
        .collect();
    assert!(specs.is_empty(), "no native-tool model may receive a tool schema");
    let provider = SurfaceProvider {
        sends: AtomicUsize::new(0),
    };
    provider
        .chat(
            ChatRequest {
                messages: &[ChatMessage::user("reply")],
                tools: Some(&specs),
            },
            "prx-local-model",
            0.0,
        )
        .await
        .expect("provider call with an empty surface");
    let mut context = adapter.tool_execution_context.clone();
    assert!(context.execution_bindings.is_none());
    context.turn_surface = Some(Arc::new(surface));
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert_eq!(provider.sends.load(Ordering::SeqCst), 1);
    assert!(dispatches.lock().unwrap().is_empty());
}

/// A `hint:` route executes a different spelling, but the operator allowlist is
/// keyed on the model pinned at ingress.
#[tokio::test]
async fn hint_routed_turn_applies_the_pinned_model_allowlist() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut config = workspace_config(workspace.path());
    config.model_routes = vec![crate::config::ModelRouteConfig {
        settings: Default::default(),
        hint: "fast".to_string(),
        provider: "ollama".to_string(),
        model: "prx-local-model".to_string(),
        api_key: None,
        credential_ref: None,
    }];
    // The route destination must verify NativeTools, the same as a pinned turn.
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider":"ollama", "model":"prx-local-model", "operations":["chat"], "native_tools":true,
        }))
        .expect("fixture route destination descriptor"),
    );
    config
        .tool_tiering
        .model_allowlists
        .insert("prx-local-model".into(), vec!["project_tool".into()]);
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry = project_tool_registry(&dispatches, &["project_tool", "other_tool"]);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("test memory"));
    let admitted = AdmittedTurn::with_model(config, "reply", registry, memory, ("ollama", "hint:fast")).await;
    let decision = admitted.decision();
    assert_eq!(decision.proof.model_name(), "hint:fast");
    assert_eq!(decision.proof.model_binding().pinned_model(), "prx-local-model");
    assert!(
        decision.service.model_can_call_tools(decision.proof),
        "the route destination must verify NativeTools"
    );
    let surface = decision.service.turn_tool_surface(decision.proof, &[]);
    assert!(surface.contains("project_tool"), "the allowed tool was not published");
    assert!(
        !surface.contains("other_tool"),
        "the hint spelling bypassed the pinned model allowlist"
    );
}

// ---------------------------------------------------------------------------
// Provider-send admission (02c R2a). No test here builds a decision context of
// its own: the budget is read from the admitted turn's pinned configuration.
// ---------------------------------------------------------------------------

/// Counts provider sends per transport; the streaming and buffered paths each
/// return one short final answer.
struct SendSpy {
    chats: AtomicUsize,
    streams: AtomicUsize,
}
impl SendSpy {
    fn new() -> Self {
        Self {
            chats: AtomicUsize::new(0),
            streams: AtomicUsize::new(0),
        }
    }
    fn sends(&self) -> usize {
        self.chats.load(Ordering::SeqCst) + self.streams.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl Provider for SendSpy {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }
    fn supports_streaming(&self) -> bool {
        true
    }
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("fixture requires a structured request")
    }
    async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        self.chats.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            text: Some("ok".into()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
    fn stream_chat_with_history(
        &self,
        _: &[ChatMessage],
        _: &str,
        _: f64,
        _: crate::providers::traits::StreamOptions,
    ) -> futures::stream::BoxStream<
        'static,
        crate::providers::traits::StreamResult<crate::providers::traits::StreamChunk>,
    > {
        use futures::StreamExt;
        self.streams.fetch_add(1, Ordering::SeqCst);
        futures::stream::iter(vec![
            Ok(crate::providers::traits::StreamChunk::delta("ok")),
            Ok(crate::providers::traits::StreamChunk::final_chunk()),
        ])
        .boxed()
    }
}

/// A tool whose published schema is large enough to dominate a request's price.
struct CostlyTool;
#[async_trait]
impl Tool for CostlyTool {
    fn name(&self) -> &str {
        "costly_tool"
    }
    fn description(&self) -> &str {
        static TEXT: std::sync::LazyLock<String> =
            std::sync::LazyLock::new(|| (0..600).map(|index| format!("word{index} ")).collect());
        &TEXT
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        anyhow::bail!("the budget tests never execute the costly tool")
    }
}

fn priced_config(workspace: &std::path::Path, daily_limit_usd: f64) -> crate::config::Config {
    let mut config = workspace_config(workspace);
    config.cost.prices.insert(
        "ollama/prx-local-model".into(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    config.cost.daily_limit_usd = daily_limit_usd;
    config.agent.compaction.max_context_tokens = 100_000;
    config.agent.compaction.max_context_tokens_explicit = true;
    config.agent.compaction.reserve_tokens = 100;
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider":"ollama", "model":"prx-local-model", "operations":["chat"], "native_tools":true,
        }))
        .expect("fixture model descriptor"),
    );
    config
}

/// One real `run_turn` over the chosen transport with no decision context of the
/// test's own. Returns the loop result; the spy holds the send count.
async fn send_once(
    config: crate::config::Config,
    registry: Arc<Vec<Box<dyn Tool>>>,
    selection: (&str, &str),
    input: &str,
    spy: &SendSpy,
    streaming: bool,
) -> anyhow::Result<()> {
    let workspace = config.workspace_dir.clone();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).expect("test memory"));
    let admitted = AdmittedTurn::with_model(config, input, Arc::clone(&registry), memory, selection).await;
    let mut adapter = admitted.adapter();
    adapter.stream_provider = streaming;
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user(input)];
    run_turn(
        &mut history,
        TurnRequest {
            advice: None,
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: TurnSchedule {
                read_only_tool_concurrency_window: 1,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admitted.decision(),
        },
        TurnServices {
            provider: spy,
            provider_name: selection.0,
            model: selection.1,
            temperature: 0.0,
            tools_registry: registry,
            observer: &crate::observability::NoopObserver,
            hooks: &crate::hooks::HookManager::new(workspace),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(adapter),
            memory: admitted.memory_runtime(),
        },
        TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|_| ())
}

/// An unpriced remote model is not zero-price exempt: 0 sends on both transports.
#[tokio::test]
async fn unpriced_model_refuses_provider_send() {
    for streaming in [false, true] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let spy = SendSpy::new();
        let registry = project_tool_registry(&Arc::new(Mutex::new(Vec::new())), &[]);
        let result = send_once(
            workspace_config(workspace.path()),
            registry,
            ("openai", "prx-unpriced-model"),
            "reply",
            &spy,
            streaming,
        )
        .await;
        assert!(result.is_err(), "streaming={streaming}: an unpriced send was admitted");
        assert_eq!(spy.sends(), 0, "streaming={streaming}");
    }
}

/// A local model with no price row is free by the shared exemption: exactly 1 send.
#[tokio::test]
async fn zero_price_exempt_model_is_admitted() {
    for streaming in [false, true] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let spy = SendSpy::new();
        let registry = project_tool_registry(&Arc::new(Mutex::new(Vec::new())), &[]);
        send_once(
            workspace_config(workspace.path()),
            registry,
            ("ollama", "prx-local-model"),
            "reply",
            &spy,
            streaming,
        )
        .await
        .expect("an exempt local model is admitted");
        assert_eq!(spy.sends(), 1, "streaming={streaming}");
    }
}

/// The published tool schema alone pushes a positive estimate over a positive
/// remaining ceiling: 0 sends on both transports.
#[tokio::test]
async fn estimate_with_tool_schemas_above_positive_remaining_refuses_send() {
    for streaming in [false, true] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let spy = SendSpy::new();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(CostlyTool)]);
        let result = send_once(
            priced_config(workspace.path(), 0.0004),
            registry,
            ("ollama", "prx-local-model"),
            "reply",
            &spy,
            streaming,
        )
        .await;
        assert!(
            result.is_err(),
            "streaming={streaming}: the costly request was admitted"
        );
        assert_eq!(spy.sends(), 0, "streaming={streaming}");
    }
}

/// The same request without the costly tool fits the same ceiling: exactly 1 send.
#[tokio::test]
async fn same_request_without_the_costly_tool_is_admitted() {
    for streaming in [false, true] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let spy = SendSpy::new();
        let registry = project_tool_registry(&Arc::new(Mutex::new(Vec::new())), &[]);
        send_once(
            priced_config(workspace.path(), 0.0004),
            registry,
            ("ollama", "prx-local-model"),
            "reply",
            &spy,
            streaming,
        )
        .await
        .expect("the cheap request is admitted");
        assert_eq!(spy.sends(), 1, "streaming={streaming}");
    }
}

/// A tool whose schema estimate alone exceeds the decision slice of tokens.
struct TokenHeavyTool;
#[async_trait]
impl Tool for TokenHeavyTool {
    fn name(&self) -> &str {
        "token_heavy_tool"
    }
    fn description(&self) -> &str {
        "A tool with an oversized schema"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        // Above 8192 tokens at four bytes each, the decision slice's ceiling.
        serde_json::json!({
            "type": "object",
            "properties": {"note": {"type": "string", "description": "abcd ".repeat(10_000)}}
        })
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        anyhow::bail!("the budget tests never execute the heavy tool")
    }
}

/// A published tool whose action estimate exceeds the remaining G6 tokens is
/// refused with 0 sends; the same request without it is admitted with exactly 1.
#[tokio::test]
async fn estimate_tokens_above_turn_budget_refuses_send() {
    for streaming in [false, true] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let spy = SendSpy::new();
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(TokenHeavyTool)]);
        let result = send_once(
            priced_config(workspace.path(), 5.0),
            registry,
            ("ollama", "prx-local-model"),
            "reply",
            &spy,
            streaming,
        )
        .await;
        assert!(
            result.is_err(),
            "streaming={streaming}: the oversized estimate was admitted"
        );
        assert_eq!(spy.sends(), 0, "streaming={streaming}");

        let workspace = tempfile::tempdir().expect("test workspace");
        let control = SendSpy::new();
        let registry = project_tool_registry(&Arc::new(Mutex::new(Vec::new())), &[]);
        send_once(
            priced_config(workspace.path(), 5.0),
            registry,
            ("ollama", "prx-local-model"),
            "reply",
            &control,
            streaming,
        )
        .await
        .expect("the same request without the heavy tool is admitted");
        assert_eq!(control.sends(), 1, "streaming={streaming}");
    }
}

/// A priced request against an exhausted daily ceiling: 0 sends.
#[tokio::test]
async fn exhausted_daily_ceiling_refuses_provider_send() {
    for streaming in [false, true] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let spy = SendSpy::new();
        let registry = project_tool_registry(&Arc::new(Mutex::new(Vec::new())), &[]);
        let result = send_once(
            priced_config(workspace.path(), 0.0),
            registry,
            ("ollama", "prx-local-model"),
            "reply",
            &spy,
            streaming,
        )
        .await;
        assert!(
            result.is_err(),
            "streaming={streaming}: the exhausted ceiling admitted a send"
        );
        assert_eq!(spy.sends(), 0, "streaming={streaming}");
    }
}

/// Counts every admission attempt and denies it, as a read-only policy would.
struct CountingDeny(Arc<AtomicUsize>);
impl crate::tools::execution::EffectPolicy for CountingDeny {
    fn decide(
        &self,
        _: &crate::tools::ToolDescriptor,
        _: &crate::tools::ToolExecutionContext,
    ) -> crate::tools::ToolExecutionDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        crate::tools::ToolExecutionDecision::Deny
    }
}

/// The same policy-denied call twice stops the loop after 2 provider sends and
/// 2 admission attempts, with 0 backend executions and no third of either.
#[tokio::test]
async fn repeated_unrecoverable_failure_stops_the_loop() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry = project_tool_registry(&dispatches, &["project_tool"]);
    let admitted = AdmittedTurn::with_config(workspace_config(workspace.path()), "reply", Arc::clone(&registry)).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut adapter = admitted.adapter();
    adapter.tool_execution_service = Some(Arc::new(
        crate::tools::ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(CountingDeny(Arc::clone(&attempts))),
            Arc::new(crate::tools::DenyApprovalStrategy),
            Arc::new(crate::tools::AdapterOwnedPreparation),
            Arc::new(crate::tools::TracingToolExecutionAudit),
        ),
    ));
    let call = || ChatResponse {
        text: None,
        tool_calls: vec![ToolCall {
            id: "call-1".into(),
            name: "project_tool".into(),
            arguments: "{}".into(),
        }],
        reasoning_content: None,
    };
    let done = ChatResponse {
        text: Some("unreachable".into()),
        tool_calls: Vec::new(),
        reasoning_content: None,
    };
    let provider = NativeScriptedProvider::new(vec![call(), call(), done]);
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("reply")];
    let result = run_turn(
        &mut history,
        TurnRequest {
            advice: None,
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: TurnSchedule {
                read_only_tool_concurrency_window: 1,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admitted.decision(),
        },
        TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry: registry,
            observer: &crate::observability::NoopObserver,
            hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(adapter),
            memory: admitted.memory_runtime(),
        },
        TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await;
    let error = result.expect_err("the repeated failure must end the turn").to_string();
    assert!(
        error.contains("stopped after a repeated unrecoverable tool failure"),
        "{error}"
    );
    assert_eq!(provider.requests().len(), 2, "no third provider send");
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "no third admission attempt");
    assert!(dispatches.lock().unwrap().is_empty(), "no backend execution");
    assert_eq!(
        provider.responses.lock().unwrap().len(),
        1,
        "the third response stays unused"
    );
}

/// Plan mode synthesizes one "would call" result for each non-read-only effect
/// class and dispatches nothing to a backend.
#[tokio::test]
async fn plan_mode_intercepts_every_non_read_only_class() {
    // LocalWrite, ProcessExec, ExternalSend, CredentialOrConfig, Destructive.
    let names = ["file_write", "shell", "message_send", "config_reload", "nodes"];
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn Tool>> = names
        .iter()
        .map(|name| Box::new(DelayTool::new(name, 1, Arc::clone(&active), Arc::clone(&max_active))) as Box<dyn Tool>)
        .collect();
    let ledger_dir = TempDir::new().expect("test ledger dir");
    let adapter = scheduler_test_adapter(tools, &ledger_dir);
    let calls: Vec<crate::agent::tool_call_parser::ParsedToolCall> = names
        .iter()
        .map(|name| crate::agent::tool_call_parser::ParsedToolCall {
            name: (*name).to_owned(),
            arguments: serde_json::json!({"value": "x"}),
        })
        .collect();
    let recorder = crate::agent::tool_result_recording::ToolResultRecorder::new(calls.len());
    let results = crate::agent::tool_scheduler::execute_tools_with_service(
        &calls,
        &[],
        &adapter,
        crate::agent::tool_scheduler::ReadOnlyToolScheduleConfig {
            concurrency_window: 1,
            priority_enabled: false,
            low_priority_tool_names: std::collections::HashSet::new(),
        },
        None,
        ChatMode::Plan,
        &crate::observability::NoopObserver,
        &recorder,
    )
    .await
    .expect("plan mode returns synthesized results");
    assert_eq!(results.len(), names.len(), "one synthesized result per class");
    for (name, result) in names.iter().zip(&results) {
        assert!(
            result.content.starts_with(&format!("[plan mode] would call {name}")),
            "{name}: {}",
            result.content
        );
    }
    assert_eq!(max_active.load(Ordering::SeqCst), 0, "no backend dispatch");
}
