#![cfg(test)]
//! Decision-free ingress model and publication admission. The three rewritten
//! surface-owner cells build no `AdmittedTurn` and call no `decision.service`.
use super::*;
use crate::agent::turn_contracts::ChatMode;
use crate::agent::turn_engine::run_turn;
use crate::agent::turn_request::{TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
use crate::agent::turn_tests::admission::AdmittedTurn;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogSources, SurfacePolicy};
use crate::tools::{
    AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, ToolExecutionCommand, ToolExecutionService,
    ToolExecutionStatus, TracingToolExecutionAudit,
};

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

fn decision_free_surface(
    tools: Vec<Box<dyn Tool>>,
    tiering: crate::config::ToolTieringConfig,
    pinned_model: &str,
    channel: bool,
    tool_calls_allowed: bool,
) -> (ToolExecutionService, crate::tools::execution::admission::TurnSurface) {
    let publication_policy = SurfacePolicy {
        channel_allowed: channel,
        ..SurfacePolicy::default()
    };
    let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &publication_policy).expect("catalog");
    let security = crate::security::SecurityPolicy::default();
    let service = ToolExecutionService::from_boxed(
        tools,
        Arc::new(SecurityEffectPolicy::new(Arc::new(security.clone()))),
        Arc::new(DenyApprovalStrategy),
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    );
    let surface = crate::tools::surface_owner::build(crate::tools::surface_owner::SurfaceParts {
        execution: &service,
        catalog: &catalog,
        publication_policy: &publication_policy,
        tiering: &tiering,
        security: &security,
        pinned_model,
        channel,
        inherited_exclusions: &[],
        sender: "prx_user",
        channel_name: if channel { "wacli" } else { "cli" },
        chat_type: if channel { "dm" } else { "private" },
        tool_calls_allowed,
    });
    (service, surface)
}

/// An execution model outside the ingress binding is refused by `run_turn`
/// before the provider is reached. Card: assertions unchanged (decision-bound
/// cell is fine; L2 restores the original `run_turn` + provider-not-reached).
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
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "project_tool".to_owned(),
        execution_order: Arc::clone(&dispatches),
    })];
    let (service, surface) = decision_free_surface(
        tools,
        crate::config::ToolTieringConfig::default(),
        "prx-local-model",
        false,
        false,
    );
    assert!(!surface.contains("project_tool"));
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
    let mut context = crate::tools::ToolExecutionContext::new(
        crate::runtime::envelope::RuntimeEnvelope::agent("prx_workspace", "prx_session"),
        "private",
    )
    .with_dispatch_origin(crate::tools::DispatchOrigin::ModelTurn);
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
    let surface = crate::tools::surface_owner::build(crate::tools::surface_owner::SurfaceParts {
        execution: decision.service.execution(),
        catalog: decision.service.catalog(),
        publication_policy: decision.service.publication_policy(),
        tiering: decision.service.tool_tiering(),
        security: decision.service.security(),
        pinned_model: decision.proof.model_binding().pinned_model(),
        channel: decision.service.channel_surface(),
        inherited_exclusions: &[],
        sender: decision.proof.policy_sender(),
        channel_name: decision.proof.policy_channel(),
        chat_type: decision.proof.policy_chat_type(),
        tool_calls_allowed: decision.service.model_can_call_tools(decision.proof),
    });
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

/// An unpriced remote model is not zero-price exempt. The decision gate already
/// leaves it without an executable selection, so `run_turn` sends 0 on both
/// transports; the send guard itself is driven on the admitted turn directly
/// and refuses with its own cost error. The gate's refusal carries a different
/// error, so this run_turn cell cannot assert the guard's message; the guard's
/// wiring into the ordinary round is covered by
/// `send_time_money_ceiling_refuses_ordinary_round_send`.
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
    let workspace = tempfile::tempdir().expect("test workspace");
    let registry = project_tool_registry(&Arc::new(Mutex::new(Vec::new())), &[]);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("test memory"));
    let admitted = AdmittedTurn::with_model(
        workspace_config(workspace.path()),
        "reply",
        registry,
        memory,
        ("openai", "prx-unpriced-model"),
    )
    .await;
    let decision = admitted.decision();
    let error = crate::agent::send_admission::admit_provider_send(
        decision.service,
        decision.context,
        decision.proof,
        &[ChatMessage::user("reply")],
        None,
        crate::tools::ToolSchemaDialect::Raw,
    )
    .expect_err("the send guard must refuse an unpriced remote request");
    assert!(
        format!("{error:#}").contains("the request cost cannot be estimated"),
        "{error:#}"
    );
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
/// remaining ceiling. An ordinary turn only publishes a tool the decision
/// selected, so the guard is driven on the admitted turn with the costly
/// request: refused with the cost error, and the same request without the tool
/// fits the same ceiling (the run_turn control below sends exactly once).
#[tokio::test]
async fn estimate_with_tool_schemas_above_positive_remaining_refuses_send() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(CostlyTool)]);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace.path()).expect("test memory"));
    let admitted = AdmittedTurn::with_model(
        priced_config(workspace.path(), 0.0004),
        "reply",
        registry,
        memory,
        ("ollama", "prx-local-model"),
    )
    .await;
    let decision = admitted.decision();
    let specs = [crate::tools::ToolSpec {
        name: CostlyTool.name().to_owned(),
        description: CostlyTool.description().to_owned(),
        parameters: CostlyTool.parameters_schema(),
    }];
    let messages = [ChatMessage::system("sys"), ChatMessage::user("reply")];
    for dialect in [
        crate::tools::ToolSchemaDialect::Raw,
        crate::tools::ToolSchemaDialect::Gemini,
    ] {
        let error = crate::agent::send_admission::admit_provider_send(
            decision.service,
            decision.context,
            decision.proof,
            &messages,
            Some(&specs),
            dialect,
        )
        .expect_err("the costly request exceeds the remaining ceiling");
        assert!(format!("{error:#}").contains("exceeds the"), "{dialect:?}: {error:#}");
    }
    crate::agent::send_admission::admit_provider_send(
        decision.service,
        decision.context,
        decision.proof,
        &messages,
        None,
        crate::tools::ToolSchemaDialect::Raw,
    )
    .expect("the same request without the costly tool fits");
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

/// Answer 128 against live remaining G6 tokens, narrowed after the decision was
/// admitted: 127 remain is refused with the specific budget error and 0 sends;
/// 128 remain is admitted with exactly 1 send, also when an oversized tool is
/// published but unused.
#[tokio::test]
async fn estimate_tokens_above_turn_budget_refuses_send() {
    let _reset = crate::agent::send_admission::HookReset;
    for streaming in [false, true] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let spy = SendSpy::new();
        let registry = project_tool_registry(&Arc::new(Mutex::new(Vec::new())), &[]);
        crate::agent::send_admission::TEST_TOKEN_CAP.set(127);
        let error = send_once(
            workspace_config(workspace.path()),
            registry,
            ("ollama", "prx-local-model"),
            "reply",
            &spy,
            streaming,
        )
        .await
        .expect_err("127 remaining tokens cannot fund a 128 token answer");
        assert!(
            format!("{error:#}").contains("answer estimates 128 tokens but 127 remain"),
            "streaming={streaming}: {error:#}"
        );
        assert_eq!(spy.sends(), 0, "streaming={streaming}");

        crate::agent::send_admission::TEST_TOKEN_CAP.set(128);
        for tools in [Vec::new(), vec![Box::new(TokenHeavyTool) as Box<dyn Tool>]] {
            let workspace = tempfile::tempdir().expect("test workspace");
            let spy = SendSpy::new();
            send_once(
                workspace_config(workspace.path()),
                Arc::new(tools),
                ("ollama", "prx-local-model"),
                "reply",
                &spy,
                streaming,
            )
            .await
            .expect("128 remaining tokens fund the answer, with or without an unused oversized tool");
            assert_eq!(spy.sends(), 1, "streaming={streaming}");
        }
    }
}

/// The send-time money boundary is wired into the ordinary round: a ceiling the
/// admitted decision already passed refuses the real send on both transports
/// with the specific cost error and 0 sends; without it the same turn sends once.
#[tokio::test]
async fn send_time_money_ceiling_refuses_ordinary_round_send() {
    let _reset = crate::agent::send_admission::HookReset;
    for streaming in [false, true] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let spy = SendSpy::new();
        let registry = project_tool_registry(&Arc::new(Mutex::new(Vec::new())), &[]);
        crate::agent::send_admission::TEST_MONEY_CEILING.set(Some(1));
        let error = send_once(
            workspace_config(workspace.path()),
            registry,
            ("ollama", "prx-local-model"),
            "reply",
            &spy,
            streaming,
        )
        .await
        .expect_err("a 1 micro-USD send-time ceiling cannot fund the request");
        assert!(
            format!("{error:#}").contains("provider send refused: the estimated cost of"),
            "streaming={streaming}: {error:#}"
        );
        assert_eq!(spy.sends(), 0, "streaming={streaming}");

        crate::agent::send_admission::TEST_MONEY_CEILING.set(None);
        let workspace = tempfile::tempdir().expect("test workspace");
        let spy = SendSpy::new();
        send_once(
            workspace_config(workspace.path()),
            project_tool_registry(&Arc::new(Mutex::new(Vec::new())), &[]),
            ("ollama", "prx-local-model"),
            "reply",
            &spy,
            streaming,
        )
        .await
        .expect("without the send-time ceiling the same turn is admitted");
        assert_eq!(spy.sends(), 1, "streaming={streaming}");
    }
}

/// A remote model with a configured price override is not exempt and is
/// admitted by its priced estimate: exactly 1 send.
#[tokio::test]
async fn priced_remote_model_is_admitted() {
    for streaming in [false, true] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let mut config = workspace_config(workspace.path());
        config.cost.prices.insert(
            "openai/prx-priced-model".into(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let spy = SendSpy::new();
        let registry = project_tool_registry(&Arc::new(Mutex::new(Vec::new())), &[]);
        send_once(
            config,
            registry,
            ("openai", "prx-priced-model"),
            "reply",
            &spy,
            streaming,
        )
        .await
        .expect("a priced remote model is admitted");
        assert_eq!(spy.sends(), 1, "streaming={streaming}");
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

/// The production constructor builds the called-tool budget from the admitted
/// turn. Its estimator maps schema bytes to `64 + ceil(bytes/4) + 64`, and its
/// live remaining tokens follow the send-time cap.
#[tokio::test]
async fn production_budget_maps_schema_bytes_and_reads_the_live_cap() {
    use crate::agent::send_admission::{HookReset, TEST_TOKEN_CAP, call_token_budget};
    let _reset = HookReset;
    let workspace = tempfile::tempdir().expect("test workspace");
    let config = workspace_config(workspace.path());
    let admitted = AdmittedTurn::with_config(config, "reply", Arc::new(Vec::new())).await;
    let decision = admitted.decision();
    let budget = call_token_budget(decision.service, decision.context, decision.proof).expect("called-tool budget");
    for bytes in [0_usize, 1, 4, 5, 4000] {
        let expected = 64 + u64::try_from(bytes.div_ceil(4)).expect("bytes fit") + 64;
        assert_eq!(budget.tokens_for(bytes), Some(expected), "schema bytes {bytes}");
    }
    let live = budget.remaining();
    let narrowed = live.saturating_sub(1);
    TEST_TOKEN_CAP.set(narrowed);
    assert_eq!(
        budget.remaining(),
        narrowed,
        "the send-time cap narrows the live tokens"
    );
    assert!(narrowed < live || live == 0, "the narrowing must be observable");
}
