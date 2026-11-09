#![cfg(test)]
//! Surface ownership with the decision engine off. Card 03 adds enabled-engine cells.
use super::*;
use crate::agent::turn_contracts::ChatMode;
use crate::agent::turn_engine::run_turn;
use crate::agent::turn_request::{TurnDecision, TurnDelivery, TurnRequest, TurnSchedule, TurnServices};
use crate::agent::turn_tests::admission::AdmittedTurn;
use crate::providers::traits::ProviderCapabilities;
use crate::providers::{ChatMessage, ChatRequest, ChatResponse, Provider, ToolCall};
use std::collections::VecDeque;

fn channel_message() -> crate::channels::traits::ChannelMessage {
    crate::channels::traits::ChannelMessage {
        id: "prx-channel-surface".to_owned(),
        sender: "prx_user".to_owned(),
        reply_target: "prx_channel".to_owned(),
        content: "run the project tool".to_owned(),
        channel: "wacli".to_owned(),
        timestamp: 0,
        chat_kind: crate::channels::traits::ChatKind::Dm,
        chat_title: None,
        sender_display: None,
        thread_ts: None,
        mentioned_uuids: Vec::new(),
        mentioned: false,
        is_group_hint: false,
        sender_is_bot: false,
    }
}

struct EngineOffProvider {
    published: Arc<Mutex<Vec<String>>>,
    responses: Mutex<VecDeque<ChatResponse>>,
}

#[async_trait]
impl Provider for EngineOffProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("fixture requires a structured request")
    }
    async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        let names = request
            .tools
            .unwrap_or(&[])
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        *self.published.lock().expect("published tools") = names;
        self.responses
            .lock()
            .expect("scripted responses")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("scripted provider exhausted"))
    }
}

/// Engine disabled: the real `turn_engine` path publishes no channel-excluded
/// tool and a dispatch of that name is Denied with no backend call. Origin and
/// surface are left to production stamping.
#[tokio::test]
async fn tool_surface_is_built_without_decision_service() {
    let workspace = tempfile::tempdir().expect("channel workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    assert!(
        !config.decision_engine.enabled,
        "this cell runs with the decision engine off"
    );
    config.tool_tiering.channel_exclude.push("project_tool".into());
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(RecordingTool {
        name: "project_tool".to_owned(),
        execution_order: Arc::clone(&dispatches),
    }) as Box<dyn Tool>]);
    let admitted = AdmittedTurn::with_channel(config, channel_message(), Arc::clone(&registry)).await;
    let published = Arc::new(Mutex::new(Vec::new()));
    let provider = EngineOffProvider {
        published: Arc::clone(&published),
        responses: Mutex::new(
            vec![
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "project_tool".into(),
                        arguments: "{}".into(),
                    }],
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("done".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                },
            ]
            .into(),
        ),
    };
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("run the project tool")];
    let result = run_turn(
        &mut history,
        TurnRequest {
            advice: None,
            channel_name: "wacli",
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
    let _ = result;
    assert!(
        !published
            .lock()
            .expect("published tools")
            .iter()
            .any(|name| name == "project_tool"),
        "channel-excluded tool was published: {:?}",
        published.lock().expect("published tools")
    );
    assert!(dispatches.lock().expect("dispatch log").is_empty());
    let denied = history
        .iter()
        .any(|message| message.role == "tool" && message.content.contains("was not exposed for this turn"));
    assert!(
        denied,
        "engine-off dispatch of the excluded tool was not Denied: {history:?}"
    );
}

/// Disabled `open_turn_context` stamps the G6 slice from the reserved window:
/// (32768 − 4096) / 8 = 3584, below the 8192 cap, with ratio 1.0 not binding.
#[tokio::test]
async fn open_turn_context_g6_slice_is_literal() {
    let workspace = tempfile::tempdir().expect("open-turn-context workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    assert!(
        !config.decision_engine.enabled,
        "this cell runs with the decision engine off"
    );
    config.agent.compaction.max_context_tokens = 32_768;
    config.agent.compaction.max_context_tokens_explicit = true;
    config.agent.compaction.reserve_tokens = 4_096;
    config.causal_tree.policy.extra_token_ratio_limit = 1.0;
    let admitted = AdmittedTurn::with_config(config, "open the turn context", Arc::new(Vec::new())).await;
    let decision = admitted.decision();
    let context = decision
        .service
        .open_turn_context(
            decision.proof,
            crate::runtime::turn_decision::AgentTurnKind::SingleShot,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("disabled engine opens a turn context");
    assert_eq!(crate::agent::send_admission::live_g6_tokens(&context), 3584);
}

/// Cancelled token plus engine off: `decide_agent_initial` returns Cancelled
/// through `open_turn_context` (L1).
#[tokio::test]
async fn decide_agent_initial_cancelled_with_engine_off_returns_cancelled() {
    let workspace = tempfile::tempdir().expect("cancelled-open workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    assert!(
        !config.decision_engine.enabled,
        "this cell runs with the decision engine off"
    );
    let admitted = AdmittedTurn::with_config(config, "cancel the opened turn", Arc::new(Vec::new())).await;
    let decision = admitted.decision();
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();
    let result = decision
        .service
        .decide_agent_initial(
            decision.proof,
            crate::runtime::turn_decision::AgentTurnKind::SingleShot,
            &cancellation,
        )
        .await;
    assert!(
        matches!(
            result,
            Err(crate::runtime::turn_decision::AgentDecisionError::Cancelled)
        ),
        "expected Cancelled"
    );
}

/// Disabled `open_turn_context` plus `run_turn` stamps a finite call budget.
/// Answer estimate is 128; the live cap is 128 so the send is admitted and
/// `project_tool` (schema bytes > 0) is Denied. Skip-stamping `call_budget`
/// lets the tool run. `open_turn_context` and `run_turn` add 0 prediction-port
/// calls.
#[tokio::test]
async fn disabled_entry_turn_has_finite_call_budget() {
    use crate::agent::send_admission::{HookReset, TEST_TOKEN_CAP};
    let reset = HookReset;
    TEST_TOKEN_CAP.set(128);
    let workspace = tempfile::tempdir().expect("call-budget workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    assert!(
        !config.decision_engine.enabled,
        "this cell runs with the decision engine off"
    );
    config.tool_tiering.always_include.push("project_tool".into());
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(RecordingTool {
        name: "project_tool".to_owned(),
        execution_order: Arc::clone(&dispatches),
    }) as Box<dyn Tool>]);
    let admitted = AdmittedTurn::with_config(config, "run the project tool", Arc::clone(&registry)).await;
    let decision = admitted.decision();
    assert!(
        decision.service.model_can_call_tools(decision.proof),
        "this cell needs native tools so the Direct surface is published"
    );
    let before = decision.service.prediction_port_calls();
    let context = decision
        .service
        .open_turn_context(
            decision.proof,
            crate::runtime::turn_decision::AgentTurnKind::SingleShot,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("disabled engine opens a turn context");
    assert_eq!(decision.service.prediction_port_calls(), before);
    let opened = TurnDecision {
        service: decision.service,
        context: &context,
        proof: decision.proof,
    };
    let published = Arc::new(Mutex::new(Vec::new()));
    let provider = EngineOffProvider {
        published: Arc::clone(&published),
        responses: Mutex::new(
            vec![
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "project_tool".into(),
                        arguments: "{}".into(),
                    }],
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("done".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                },
            ]
            .into(),
        ),
    };
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("run the project tool")];
    let outcome = run_turn(
        &mut history,
        TurnRequest {
            advice: None,
            channel_name: "wacli",
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
            decision: opened,
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
            runtime_adapter: Some(admitted.adapter()),
            memory: admitted.memory_runtime(),
        },
        TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .expect("disabled entry turn completes");
    drop(reset);
    drop(outcome);
    assert!(
        published
            .lock()
            .expect("published tools")
            .iter()
            .any(|name| name == "project_tool"),
        "project_tool was not published: {:?}",
        published.lock().expect("published tools")
    );
    assert!(
        dispatches.lock().expect("dispatch log").is_empty(),
        "project_tool ran without a call budget: {:?}",
        dispatches.lock().expect("dispatch log")
    );
    let denied = history
        .iter()
        .any(|message| message.role == "tool" && message.content.contains("but 128 remain"));
    assert!(
        denied,
        "engine-off dispatch was not Denied at the call budget: {history:?}"
    );
    assert_eq!(decision.service.prediction_port_calls(), before);
}

struct SpawnFixtureChannel;

#[async_trait]
impl crate::channels::traits::Channel for SpawnFixtureChannel {
    fn name(&self) -> &str {
        "prx_grant_fixture"
    }
    async fn send(&self, _: &crate::channels::traits::SendMessage) -> anyhow::Result<()> {
        anyhow::bail!("grant fixture must not announce")
    }
    async fn listen(
        &self,
        _: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("grant fixture has no listener")
    }
}

/// Child proof through `bind_agent_execution_with_runtime` keeps scope, pin
/// and grant. Deleting the `child_pin` stamp fails this cell. A call outside
/// the sealed manifest is StaleCapability (02c `admit_child`).
#[tokio::test]
async fn bound_child_context_keeps_scope_pin_and_grant() {
    use crate::agent::test_admission::{AdmittedTurn, NoExternalTransport, SilentEvents, registry_with_search};
    use crate::causal_tree::state::TurnSurfaceKind;
    use crate::memory::{MemoryVisibility, MessageEventScope};
    use crate::runtime::child_source::ChildProviderBinding;
    use crate::runtime::turn_decision::{AgentTurnKind, TurnServiceBuild, build_turn_decision_service};
    use crate::tools::capability::catalog::CatalogSources;
    use crate::tools::execution::{
        AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, ToolExecutionCommand,
        ToolExecutionService, ToolExecutionStatus, TracingToolExecutionAudit,
    };
    use crate::tools::sessions_spawn::SessionsSpawnTool;
    use std::collections::BTreeMap;

    let workspace = tempfile::tempdir().expect("child-stamp workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.config_path = workspace.path().join("config.toml");
    config.causal_tree.enabled = true;
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    config.sessions_spawn.allow_recursive_delegation = true;
    config.tool_tiering.always_include.push("sessions_spawn".into());
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &config.autonomy,
        workspace.path(),
    ));
    let memory: Arc<dyn crate::memory::Memory> =
        Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("child-stamp memory"));
    let spawn = SessionsSpawnTool::new(
        Arc::new(SpawnFixtureChannel),
        "ollama",
        "prx-local-model",
        0.0,
        Arc::clone(&security),
        workspace.path().to_owned(),
        config.multimodal.clone(),
        Default::default(),
        None,
        Default::default(),
        config.sessions_spawn.clone(),
    )
    .with_shared_memory(Arc::clone(&memory));
    let parent_registry = registry_with_search(vec![Box::new(spawn)]);
    let admitted = AdmittedTurn::with_enabled_builtin_model(
        config.clone(),
        "sessions_spawn: Explain two integers.",
        Arc::clone(&parent_registry),
        Arc::clone(&memory),
        ("ollama", "prx-local-model"),
    )
    .await;
    let decision = admitted.decision();
    let selected = decision
        .service
        .agent_execution_specs(decision.proof, decision.context)
        .expect("parent selected sessions_spawn");
    let incoming = admitted.adapter().tool_execution_context;
    let (_, parent_bound) = decision
        .service
        .bind_agent_execution_with_runtime(decision.proof, decision.context, &incoming, &selected.specs, None)
        .expect("parent bind stamps an outgoing grant");
    let grant = parent_bound
        .child_grant
        .clone()
        .expect("parent bind stamps an outgoing grant");
    let recursive = grant
        .for_spawn_request(true, "ollama", "prx-local-model")
        .expect("recursive child retains sessions_spawn");
    let credential = crate::providers::selection::credential_for(&recursive.generation().effective, "ollama");
    let binding = ChildProviderBinding::build(Arc::clone(&recursive), "ollama", "prx-local-model", credential)
        .expect("child provider binding");
    let provider = binding.provider();
    let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.path().to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let mut task_scope = MessageEventScope::new("sessions_spawn", MemoryVisibility::Workspace)
        .with_session_key("prx_child_stamp_session")
        .with_run_id("prx_child_stamp_task");
    recursive
        .bind_event_scope(&mut task_scope)
        .expect("grant binds the child event scope");
    let receipt = crate::runtime::child_source::record_child_input(
        &fabric,
        task_scope,
        "sessions_spawn: Explain a nested integer.",
        Some("prx_child_stamp_task:input".into()),
        None,
        Some(recursive.as_ref()),
    )
    .await
    .expect("record child input")
    .expect("durable child input receipt");
    let child_proof = binding
        .source_proof(recursive.as_ref(), &receipt, &provider)
        .expect("child source proof");
    let mut child_execution_context = receipt
        .execution_context(&child_proof)
        .expect("child execution context");
    child_execution_context.child_pin = None;
    let child_spawn = SessionsSpawnTool::new(
        Arc::new(SpawnFixtureChannel),
        "ollama",
        "prx-local-model",
        0.0,
        Arc::clone(recursive.security()),
        workspace.path().to_owned(),
        config.multimodal.clone(),
        Default::default(),
        None,
        Default::default(),
        config.sessions_spawn.clone(),
    )
    .with_shared_memory(Arc::clone(&memory));
    let child_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
        Box::new(child_spawn) as Box<dyn Tool>,
        Box::new(RecordingTool {
            name: "project_tool".to_owned(),
            execution_order: Arc::new(Mutex::new(Vec::new())),
        }) as Box<dyn Tool>,
    ]);
    let child_execution = Arc::new(
        ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&child_registry),
            Arc::new(SecurityEffectPolicy::new(Arc::clone(recursive.security()))),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        )
        .with_idempotency_memory(Arc::clone(&memory)),
    );
    let generation = Arc::clone(recursive.generation());
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
    let provider_catalog = crate::providers::catalog::CatalogView::from_config(&generation.effective);
    let child_service = build_turn_decision_service(
        TurnSurfaceKind::Worker,
        TurnServiceBuild {
            parent_grant: Some(Arc::clone(&recursive)),
            generation: Arc::clone(&generation),
            registry: Arc::clone(&child_registry),
            execution: Arc::clone(&child_execution),
            catalog_sources: CatalogSources::from_runtime_inputs(
                generation.id.0.to_string(),
                &generation.effective,
                Vec::new(),
                &hooks,
                0,
            ),
            surface_policy: crate::runtime::turn_publication::surface_policy(
                TurnSurfaceKind::Worker,
                &generation.effective.tool_tiering,
            ),
            provider_catalog: &provider_catalog,
            transports: &NoExternalTransport,
            events: Arc::new(SilentEvents),
            memory: Arc::clone(&memory),
            security: Arc::clone(recursive.security()),
        },
    )
    .await
    .expect("child worker service");
    let child_context = child_service
        .decide_agent_initial(
            &child_proof,
            AgentTurnKind::Repl,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("child turn admission");
    let child_specs = child_service
        .agent_execution_specs(&child_proof, &child_context)
        .expect("child execution specs");
    let (execution, bound) = child_service
        .bind_agent_execution_with_runtime(
            &child_proof,
            &child_context,
            &child_execution_context,
            &child_specs.specs,
            None,
        )
        .expect("child bind stamps scope, pin and grant");
    assert!(bound.child_scope.is_some());
    assert!(bound.child_pin.is_some());
    assert!(bound.child_grant.is_some());
    let outside = execution
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({})),
            bound,
            None,
        )
        .await;
    assert_eq!(outside.status, ToolExecutionStatus::StaleCapability);
}
