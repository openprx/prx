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
    calls: Arc<std::sync::atomic::AtomicUsize>,
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
            crate::runtime::turn_context::AgentTurnKind::SingleShot,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("disabled engine opens a turn context");
    assert_eq!(crate::agent::send_admission::live_g6_tokens(&context), 3584);
}

/// Cancelled token plus engine off: `open_issued_turn_context` returns Cancelled
/// through `open_turn_context` (L1).
#[tokio::test]
async fn issued_turn_context_cancelled_with_engine_off_returns_cancelled() {
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
        .open_issued_turn_context(
            decision.proof,
            crate::runtime::turn_context::AgentTurnKind::SingleShot,
            &cancellation,
        )
        .await;
    assert!(
        matches!(result, Err(crate::runtime::turn_context::AgentDecisionError::Cancelled)),
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
            crate::runtime::turn_context::AgentTurnKind::SingleShot,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("disabled engine opens a turn context");
    assert_eq!(decision.service.prediction_port_calls(), before);
    let opened = TurnDecision {
        service: decision.service,
        context: &context,
        runtime: context.runtime(),
        proof: decision.proof,
    };
    let published = Arc::new(Mutex::new(Vec::new()));
    let provider = EngineOffProvider {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
    use crate::memory::{MemoryVisibility, MessageEventScope};
    use crate::runtime::child_source::ChildProviderBinding;
    use crate::runtime::turn_context::AgentTurnKind;
    use crate::runtime::turn_publication::{PublicationInputs, build_publication};
    use crate::tools::capability::catalog::CatalogSources;
    use crate::tools::capability::turn_state::TurnSurfaceKind;
    use crate::tools::execution::{
        AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, ToolExecutionCommand,
        ToolExecutionService, ToolExecutionStatus, TracingToolExecutionAudit,
    };
    use crate::tools::sessions_spawn::SessionsSpawnTool;
    use std::collections::BTreeMap;

    let workspace = tempfile::tempdir().expect("test: child-stamp workspace");
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
        Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: child-stamp memory"));
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
        .expect("test: parent selected sessions_spawn");
    let incoming = admitted.adapter().tool_execution_context;
    let (_, parent_bound) = decision
        .service
        .bind_agent_execution_with_runtime(decision.proof, decision.context, &incoming, &selected.specs, None)
        .expect("test: parent bind stamps an outgoing grant");
    let grant = parent_bound
        .child_grant
        .clone()
        .expect("test: parent bind stamps an outgoing grant");
    let recursive = grant
        .for_spawn_request(true, "ollama", "prx-local-model")
        .expect("test: recursive child retains sessions_spawn");
    let credential = crate::providers::selection::credential_for(&recursive.generation().effective, "ollama");
    let binding = ChildProviderBinding::build(Arc::clone(&recursive), "ollama", "prx-local-model", credential)
        .expect("test: child provider binding");
    let provider = binding.provider();
    let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.path().to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let mut task_scope = MessageEventScope::new("sessions_spawn", MemoryVisibility::Workspace)
        .with_session_key("prx_child_stamp_session")
        .with_run_id("prx_child_stamp_task");
    recursive
        .bind_event_scope(&mut task_scope)
        .expect("test: grant binds the child event scope");
    let receipt = crate::runtime::child_source::record_child_input(
        &fabric,
        task_scope,
        "sessions_spawn: Explain a nested integer.",
        Some("prx_child_stamp_task:input".into()),
        None,
        Some(recursive.as_ref()),
    )
    .await
    .expect("test: record child input")
    .expect("test: durable child input receipt");
    let child_proof = binding
        .source_proof(recursive.as_ref(), &receipt, &provider)
        .expect("test: child source proof");
    let mut child_execution_context = receipt
        .execution_context(&child_proof)
        .expect("test: child execution context");
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
    let child_service = build_publication(
        TurnSurfaceKind::Worker,
        PublicationInputs {
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
    .expect("test: child worker service");
    let child_context = child_service
        .open_issued_turn_context(
            &child_proof,
            AgentTurnKind::Repl,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("test: child turn admission");
    let child_specs = child_service
        .agent_execution_specs(&child_proof, &child_context)
        .expect("test: child execution specs");
    let (execution, bound) = child_service
        .bind_agent_execution_with_runtime(
            &child_proof,
            &child_context,
            &child_execution_context,
            &child_specs.specs,
            None,
        )
        .expect("test: child bind stamps scope, pin and grant");
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

/// Live parent-issued child admitted through `ChildSegmentTurn` (no decision
/// gate). Callers prove the stamped budget and 02c outside-manifest refuse.
pub(crate) struct AdmittedBoundChild {
    pub turn: crate::runtime::child_turn::ChildSegmentTurn,
    pub service: Arc<crate::runtime::turn_decision::TurnDecisionService>,
    owner: Arc<crate::runtime::turn_cleanup::CleanupTracker>,
    _workspace: tempfile::TempDir,
}

impl AdmittedBoundChild {
    pub(crate) async fn shutdown(self) {
        let _ = self
            .turn
            .fail(&anyhow::anyhow!("fixture closes the admitted child"), false)
            .await;
        drop(self.turn);
        let _ = self
            .owner
            .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(15))
            .await;
    }
}

pub(crate) async fn admit_bound_child_segment() -> AdmittedBoundChild {
    admit_bound_child_segment_with_pricing(false).await
}

pub(crate) async fn admit_priced_bound_child_segment() -> AdmittedBoundChild {
    admit_bound_child_segment_with_pricing(true).await
}

async fn admit_bound_child_segment_with_pricing(priced: bool) -> AdmittedBoundChild {
    use crate::agent::test_admission::{AdmittedTurn, NoExternalTransport, SilentEvents, registry_with_search};
    use crate::memory::{MemoryVisibility, MessageEventScope};
    use crate::runtime::child_source::ChildProviderBinding;
    use crate::runtime::turn_publication::{PublicationInputs, build_publication};
    use crate::tools::capability::catalog::CatalogSources;
    use crate::tools::capability::turn_state::TurnSurfaceKind;
    use crate::tools::execution::{
        AdapterOwnedPreparation, DenyApprovalStrategy, SecurityEffectPolicy, ToolExecutionService,
        TracingToolExecutionAudit,
    };
    use crate::tools::sessions_spawn::SessionsSpawnTool;
    use std::collections::BTreeMap;

    let workspace = tempfile::tempdir().expect("test: segment-admit workspace");
    let mut config = crate::config::Config::default();
    if priced {
        config.cost.prices.insert(
            "ollama/prx-local-model".into(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
    }
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
        Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: segment-admit memory"));
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
        .expect("test: parent selected sessions_spawn");
    let incoming = admitted.adapter().tool_execution_context;
    let (_, parent_bound) = decision
        .service
        .bind_agent_execution_with_runtime(decision.proof, decision.context, &incoming, &selected.specs, None)
        .expect("test: parent bind stamps an outgoing grant");
    let grant = parent_bound
        .child_grant
        .clone()
        .expect("test: parent bind stamps an outgoing grant");
    let recursive = grant
        .for_spawn_request(true, "ollama", "prx-local-model")
        .expect("test: recursive child retains sessions_spawn");
    let credential = crate::providers::selection::credential_for(&recursive.generation().effective, "ollama");
    let binding = ChildProviderBinding::build(Arc::clone(&recursive), "ollama", "prx-local-model", credential)
        .expect("test: child provider binding");
    let provider = binding.provider();
    let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.path().to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let mut task_scope = MessageEventScope::new("sessions_spawn", MemoryVisibility::Workspace)
        .with_session_key("prx_child_segment_session")
        .with_run_id("prx_child_segment_task");
    recursive
        .bind_event_scope(&mut task_scope)
        .expect("test: grant binds the child event scope");
    let receipt = crate::runtime::child_source::record_child_input(
        &fabric,
        task_scope,
        "sessions_spawn: Explain a nested integer.",
        Some("prx_child_segment_task:input".into()),
        None,
        Some(recursive.as_ref()),
    )
    .await
    .expect("test: record child input")
    .expect("test: durable child input receipt");
    let child_proof = Arc::new(
        binding
            .source_proof(recursive.as_ref(), &receipt, &provider)
            .expect("test: child source proof"),
    );
    let child_execution_context = receipt
        .execution_context(child_proof.as_ref())
        .expect("test: child execution context");
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
    let child_registry: Arc<Vec<Box<dyn crate::tools::Tool>>> = Arc::new(vec![
        Box::new(child_spawn) as Box<dyn crate::tools::Tool>,
        Box::new(RecordingTool {
            name: "project_tool".to_owned(),
            execution_order: Arc::new(std::sync::Mutex::new(Vec::new())),
        }) as Box<dyn crate::tools::Tool>,
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
    let child_service = build_publication(
        TurnSurfaceKind::Worker,
        PublicationInputs {
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
    .expect("test: child worker service");
    let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    owner.retain_service(Arc::clone(&child_service));
    let cancellation = tokio_util::sync::CancellationToken::new();
    let turn = crate::runtime::child_turn::ChildSegmentTurn::admit(
        Arc::clone(&child_service),
        (child_proof, child_execution_context),
        fabric,
        Arc::clone(&generation.effective),
        &owner,
        &cancellation,
        None,
    )
    .await
    .expect("test: ungated child segment admits");
    AdmittedBoundChild {
        turn,
        service: child_service,
        owner,
        _workspace: workspace,
    }
}

/// Default engine-off surface: a Searchable tool is offered after
/// `capability_search` and then dispatched.
#[tokio::test]
async fn default_config_discovered_tool_is_published_then_dispatched() {
    let workspace = tempfile::tempdir().expect("test: discovered-tool workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    assert!(
        !config.decision_engine.enabled,
        "this cell runs with the decision engine off"
    );
    config.causal_tree.policy.extra_latency_budget_ms = 1_000;
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let registry = crate::agent::test_admission::registry_with_search(vec![Box::new(RecordingTool {
        name: "discovered_probe".to_owned(),
        execution_order: Arc::clone(&dispatches),
    }) as Box<dyn Tool>]);
    let admitted = AdmittedTurn::with_config(config, "find discovered_probe", Arc::clone(&registry)).await;
    let decision = admitted.decision();
    let context = decision
        .service
        .open_turn_context(
            decision.proof,
            crate::runtime::turn_context::AgentTurnKind::SingleShot,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("test: disabled engine opens a turn context");
    let opened = TurnDecision {
        service: decision.service,
        context: &context,
        runtime: context.runtime(),
        proof: decision.proof,
    };
    let published = Arc::new(Mutex::new(Vec::new()));
    let provider = EngineOffProvider {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        published: Arc::clone(&published),
        responses: Mutex::new(
            vec![
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "search-1".into(),
                        name: "capability_search".into(),
                        arguments: serde_json::json!({"query": "discovered_probe", "limit": 1}).to_string(),
                    }],
                    reasoning_content: None,
                },
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "discovered_probe".into(),
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
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("find discovered_probe")];
    run_turn(
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
    .expect("test: discovered-tool turn completes");
    assert!(
        published
            .lock()
            .expect("test: published tools")
            .iter()
            .any(|name| name == "discovered_probe"),
        "discovered tool must be published after search: {:?}",
        published.lock().expect("test: published tools")
    );
    assert_eq!(
        *dispatches.lock().expect("test: dispatch log"),
        vec!["discovered_probe".to_owned()],
        "discovered tool must be dispatched"
    );
}

fn engine_off_config(workspace: &std::path::Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_owned();
    config.config_path = workspace.join("config.toml");
    config.default_provider = Some("ollama".to_owned());
    config.default_model = Some("prx-local-model".to_owned());
    config.memory.auto_save = false;
    config.cost.prices.insert(
        "ollama/prx-local-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config
        .providers
        .models
        .push(crate::config::schema::ProviderModelConfig {
            provider: "ollama".to_owned(),
            model: Some("prx-local-model".to_owned()),
            operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
            native_tools: Some(true),
            vision_input: None,
            streaming: None,
            structured_output: None,
            tool_schema_dialect: None,
            tool_call_dialect: None,
            context_window: None,
        });
    assert!(
        !config.decision_engine.enabled,
        "this cell runs with the decision engine off"
    );
    config
}

async fn drive_agent_entry(workspace: &std::path::Path, config: crate::config::Config) {
    drive_agent_entry_with_input(workspace, config, "hello").await;
}

async fn drive_agent_entry_with_input(workspace: &std::path::Path, mut config: crate::config::Config, input: &str) {
    config.workspace_dir = workspace.to_owned();
    let memory: Arc<dyn crate::memory::Memory> =
        Arc::new(crate::memory::SqliteMemory::new(workspace).expect("test: agent sqlite"));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let published = Arc::new(Mutex::new(Vec::new()));
    let provider = EngineOffProvider {
        calls: Arc::clone(&calls),
        published: Arc::clone(&published),
        responses: Mutex::new(
            vec![ChatResponse {
                text: Some("ok".into()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            }]
            .into(),
        ),
    };
    config.tool_tiering.always_include.push("project_tool".into());
    let mut agent = crate::agent::agent::Agent::builder()
        .runtime_config(config, "ollama")
        .workspace_dir(workspace.to_owned())
        .provider(Box::new(provider))
        .tools(vec![Box::new(RecordingTool {
            name: "project_tool".into(),
            execution_order: Arc::new(Mutex::new(Vec::new())),
        }) as Box<dyn Tool>])
        .memory(memory)
        .observer(Arc::new(crate::observability::NoopObserver))
        .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
        .build()
        .expect("test: agent entry");
    assert_eq!(
        agent.turn(input).await.expect("test: entry completes"),
        "ok",
        "test: real provider response"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "test: agent provider called once"
    );
    assert_eq!(
        *published.lock().expect("test: published surface"),
        vec!["project_tool"],
        "test: default Direct surface survives engine-off backend configuration"
    );
}

/// Disabled engine never constructs a factory, telemetry store, or credential
/// resolution on any production entry. Recovery still follows `[causal_tree]`.
#[tokio::test]
async fn disabled_engine_constructs_nothing() {
    fn assert_cell(name: &str, workspace: &std::path::Path, spy: &crate::decision_engine::access_spy::AccessSpy) {
        spy.assert_constructs_nothing(name);
        assert!(
            workspace.join("self/causal_tree/decisions.db").is_file(),
            "{name}: recovery database exists"
        );
        assert!(
            !workspace.join("decision-engine.sqlite").exists(),
            "{name}: decision-engine.sqlite must not be created"
        );
    }

    {
        let workspace = tempfile::tempdir().expect("test: engine-off workspace");
        let spy = crate::decision_engine::access_spy::AccessSpy::new(workspace.path());
        drive_agent_entry(workspace.path(), engine_off_config(workspace.path())).await;
        assert_cell("agent", workspace.path(), &spy);
    }
    {
        let workspace = tempfile::tempdir().expect("test: engine-off workspace");
        let spy = crate::decision_engine::access_spy::AccessSpy::new(workspace.path());
        crate::chat::dispatcher::drive_disabled_engine_chat_entry(workspace.path()).await;
        assert_cell("chat", workspace.path(), &spy);
    }
    {
        let workspace = tempfile::tempdir().expect("test: engine-off workspace");
        let spy = crate::decision_engine::access_spy::AccessSpy::new(workspace.path());
        crate::channels::drive_disabled_engine_channel_entry(workspace.path()).await;
        assert_cell("channel", workspace.path(), &spy);
    }
    {
        let workspace = tempfile::tempdir().expect("test: engine-off workspace");
        let spy = crate::decision_engine::access_spy::AccessSpy::new(workspace.path());
        crate::gateway::drive_disabled_engine_webhook_entry(workspace.path()).await;
        assert_cell("gateway", workspace.path(), &spy);
    }
    {
        let workspace = tempfile::tempdir().expect("test: engine-off workspace");
        let spy = crate::decision_engine::access_spy::AccessSpy::new(workspace.path());
        crate::gateway::drive_disabled_engine_console_entry(workspace.path()).await;
        assert_cell("console", workspace.path(), &spy);
    }

    for kind in [
        crate::config::schema::causal_tree::BackendKind::Builtin,
        crate::config::schema::causal_tree::BackendKind::CompatibleHttp,
    ] {
        let workspace = tempfile::tempdir().expect("test: local backend workspace");
        let spy = crate::decision_engine::access_spy::AccessSpy::new(workspace.path());
        let mut config = engine_off_config(workspace.path());
        config.causal_tree.enabled = true;
        config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(kind);
        if kind == crate::config::schema::causal_tree::BackendKind::CompatibleHttp {
            config.causal_tree.backend.endpoint = "http://127.0.0.1:9/v1".into();
            config.causal_tree.backend.model = "prx-decision-probe".into();
            config.providers.models.push(serde_json::from_value(serde_json::json!({"provider":"custom:http://127.0.0.1:9/v1", "model":"prx-decision-probe", "operations":["decision_endpoint"]})).expect("test: declared loopback decision model"));
        }
        drive_agent_entry(workspace.path(), config).await;
        assert_cell(&format!("local backend {kind:?}"), workspace.path(), &spy);
        assert!(
            workspace.path().join("self/causal_tree/decisions.db").is_file(),
            "test: recovery decisions database exists"
        );
    }
    let workspace = tempfile::tempdir().expect("test: remote-keys workspace");
    let spy = crate::decision_engine::access_spy::AccessSpy::new(workspace.path());
    let mut config = engine_off_config(workspace.path());
    config.causal_tree.enabled = true;
    config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(
        crate::config::schema::causal_tree::BackendKind::CompatibleHttp,
    );
    config.causal_tree.backend.remote_enabled = true;
    config.causal_tree.backend.endpoint = "https://example.invalid/v1".into();
    config.causal_tree.backend.model = "decision-model".into();
    config.causal_tree.backend.credential_ref =
        crate::config::schema::causal_tree::CredentialRefSetting::Profile("cred-engine-off".into());
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider": "custom:https://example.invalid/v1",
            "model": "decision-model",
            "operations": ["decision_endpoint"],
        }))
        .expect("test: decision model declaration"),
    );
    drive_agent_entry(workspace.path(), config).await;
    spy.assert_constructs_nothing("causal_tree remote keys");
    assert!(
        workspace.path().join("self/causal_tree/decisions.db").exists(),
        "recovery stays on when [causal_tree].enabled is true"
    );
}

/// Enabled engine projects an advisory brief and leaves the published tool
/// surface identical to the disabled path.
#[tokio::test]
async fn enabled_engine_adds_brief_but_never_changes_tool_surface() {
    async fn published_tools(enabled: bool) -> (Vec<String>, Vec<String>) {
        let workspace = tempfile::tempdir().expect("test: brief workspace");
        let mut config = engine_off_config(workspace.path());
        config.decision_engine.enabled = enabled;
        config.tool_tiering.always_include.push("project_tool".into());
        let memory: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: brief sqlite"));
        let published = Arc::new(Mutex::new(Vec::new()));
        let systems = Arc::new(Mutex::new(Vec::new()));
        struct BriefProvider {
            published: Arc<Mutex<Vec<String>>>,
            systems: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl Provider for BriefProvider {
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
                *self.published.lock().expect("test: published") = request
                    .tools
                    .unwrap_or(&[])
                    .iter()
                    .map(|spec| spec.name.clone())
                    .collect();
                *self.systems.lock().expect("test: systems") = request
                    .messages
                    .iter()
                    .filter(|message| message.role == "system")
                    .map(|message| message.content.clone())
                    .collect();
                Ok(ChatResponse {
                    text: Some("ok".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
        }
        let mut agent = crate::agent::agent::Agent::builder()
            .runtime_config(config, "ollama")
            .workspace_dir(workspace.path().to_owned())
            .provider(Box::new(BriefProvider {
                published: Arc::clone(&published),
                systems: Arc::clone(&systems),
            }))
            .tools(vec![Box::new(RecordingTool {
                name: "project_tool".to_owned(),
                execution_order: Arc::new(Mutex::new(Vec::new())),
            }) as Box<dyn Tool>])
            .memory(memory)
            .observer(Arc::new(crate::observability::NoopObserver))
            .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
            .build()
            .expect("test: brief agent");
        agent.turn("hello").await.expect("test: advisory entry completes");
        (
            published.lock().expect("test: published").clone(),
            systems.lock().expect("test: systems").clone(),
        )
    }
    let (disabled_tools, disabled_systems) = published_tools(false).await;
    let (enabled_tools, enabled_systems) = published_tools(true).await;
    assert_eq!(disabled_tools, enabled_tools, "tool surface must not change");
    assert!(
        disabled_systems
            .iter()
            .all(|text| !text.contains("Decision advice (advisory only):")),
        "disabled engine must not add a brief: {disabled_systems:?}"
    );
    assert!(
        enabled_systems
            .iter()
            .any(|text| text.contains("Decision advice (advisory only):")),
        "enabled engine must add a brief: {enabled_systems:?}"
    );
}

async fn run_plain_fallback_cell(input: &str) {
    let workspace = tempfile::tempdir().expect("test: plain workspace");
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.path().to_owned();
    config.causal_tree.enabled = true;
    config.causal_tree.backend.kind = crate::config::schema::causal_tree::BackendSelection::Fixed(
        crate::config::schema::causal_tree::BackendKind::Builtin,
    );
    config.causal_tree.policy.extra_latency_budget_ms = 5_000;
    // Pin confidence abstention even when the catalog makes Answer the clear winner.
    config.causal_tree.policy.commit_threshold = 1.0;
    let spy = crate::decision_engine::access_spy::AccessSpy::new(workspace.path());
    // No metadata discovery: this cell must exercise confidence abstention,
    // rather than the separate committed Answer after a successful catalog read.
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let admitted = crate::agent::turn_tests::reachability::boxed_root_probe(|| {
        AdmittedTurn::with_config(config, input, Arc::clone(&registry))
    })
    .await;
    let published = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = EngineOffProvider {
        calls: Arc::clone(&calls),
        published: Arc::clone(&published),
        responses: Mutex::new(
            vec![ChatResponse {
                text: Some("plain answer".into()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            }]
            .into(),
        ),
    };
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user(input)];
    let multimodal = crate::config::MultimodalConfig::default();
    let hooks = crate::hooks::HookManager::new(workspace.path().to_path_buf());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        crate::agent::turn_tests::reachability::boxed_root_probe(|| {
            run_turn(
                &mut history,
                TurnRequest {
                    advice: None,
                    channel_name: "wacli",
                    scope: None,
                    mode: ChatMode::default(),
                    silent: true,
                    expose_stay_silent: false,
                    multimodal: &multimodal,
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
                    hooks: &hooks,
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
        }),
    )
    .await
    .expect("test: plain turn deadline")
    .expect("test: plain turn completes");
    assert_eq!(result.0.into_text(), "plain answer", "test: serving reply is published");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "test: exactly one serving request"
    );
    assert!(
        published.lock().expect("test: surface").is_empty(),
        "test: fallback offers zero tools"
    );
    assert!(
        result
            .1
            .execution_trace
            .as_ref()
            .is_none_or(|trace| trace.calls.is_empty()),
        "test: fallback executes zero tools"
    );

    spy.assert_constructs_nothing("blank Direct input");
}

#[tokio::test]
async fn disabled_blank_direct_run_turn_calls_provider_without_prediction() {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        crate::agent::turn_tests::reachability::boxed_root_probe(|| run_plain_fallback_cell("  ")),
    )
    .await
    .expect("test: complete turn deadline");
}

#[tokio::test]
async fn disabled_whitespace_agent_entry_is_direct_without_prediction() {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        crate::agent::turn_tests::reachability::boxed_root_probe(|| async {
            let workspace = tempfile::tempdir().expect("test: whitespace entry workspace");
            let spy = crate::decision_engine::access_spy::AccessSpy::new(workspace.path());
            drive_agent_entry_with_input(workspace.path(), engine_off_config(workspace.path()), "  ").await;
            spy.assert_constructs_nothing("whitespace Agent entry");
        }),
    )
    .await
    .expect("test: whitespace Agent entry deadline");
}

/// What one Agent turn showed its provider and did, for the on/off comparison.
struct OpenedTurn {
    reply: String,
    provider_requests: usize,
    tools: Vec<String>,
    tools_fingerprint: String,
    systems: Vec<String>,
    executions: Vec<String>,
}

/// Runs one Agent turn whose provider calls `project_tool` once and then
/// answers. The provider records every request's tool list and system text.
async fn open_agent_turn(enabled: bool, configure: impl FnOnce(&mut crate::config::Config)) -> OpenedTurn {
    struct ScriptedProvider {
        requests: Arc<Mutex<Vec<(Vec<String>, String, Vec<String>)>>>,
        responses: Mutex<VecDeque<ChatResponse>>,
    }
    #[async_trait]
    impl Provider for ScriptedProvider {
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
            let specs = request.tools.unwrap_or(&[]);
            let names = specs.iter().map(|spec| spec.name.clone()).collect();
            let fingerprint = crate::agent::prompt::prompt_sha256(
                &specs
                    .iter()
                    .map(|spec| format!("{}\u{0}{}\u{0}{}", spec.name, spec.description, spec.parameters))
                    .collect::<Vec<_>>()
                    .join("\u{1}"),
            );
            let systems = request
                .messages
                .iter()
                .filter(|message| message.role == "system")
                .map(|message| message.content.clone())
                .collect();
            self.requests
                .lock()
                .expect("test: requests")
                .push((names, fingerprint, systems));
            self.responses
                .lock()
                .expect("test: scripted responses")
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("scripted provider exhausted"))
        }
    }
    let workspace = tempfile::tempdir().expect("test: switch workspace");
    let mut config = engine_off_config(workspace.path());
    config.decision_engine.enabled = enabled;
    config.tool_tiering.always_include.push("project_tool".into());
    configure(&mut config);
    let memory: Arc<dyn crate::memory::Memory> =
        Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: switch sqlite"));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let executions = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        requests: Arc::clone(&requests),
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
    let mut agent = crate::agent::agent::Agent::builder()
        .runtime_config(config, "ollama")
        .workspace_dir(workspace.path().to_owned())
        .provider(Box::new(provider))
        .tools(vec![Box::new(RecordingTool {
            name: "project_tool".to_owned(),
            execution_order: Arc::clone(&executions),
        }) as Box<dyn Tool>])
        .memory(memory)
        .observer(Arc::new(crate::observability::NoopObserver))
        .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
        .build()
        .expect("test: switch agent");
    let reply = agent.turn("run the project tool").await.expect("test: turn is served");
    let requests = requests.lock().expect("test: requests").clone();
    let (tools, tools_fingerprint, systems) = requests.first().cloned().expect("test: one provider request");
    OpenedTurn {
        reply,
        provider_requests: requests.len(),
        tools,
        tools_fingerprint,
        systems,
        executions: executions.lock().expect("test: executions").clone(),
    }
}

const BRIEF_MARKER: &str = "Decision advice (advisory only):";

/// R9 (P1-I1): engine-on equals engine-off plus the advisory brief. Both
/// open identical turns (the same execution refs and published surface);
/// with a zero-cost (builtin) backend the provider sees the same tool list
/// and tool fingerprint, the same tool executes once, and the same reply is
/// published; only the brief differs. Refuse/Clarify predictions are covered
/// by `high_confidence_refuse_or_clarify_advice_never_changes_the_turn`.
/// Historical resume is waived (owner rule, P1-08/P1-09), so no resume turn
/// is opened here.
#[tokio::test]
async fn engine_on_and_off_open_identical_turns() {
    let off = Box::pin(open_agent_turn(false, |_| {})).await;
    let on = Box::pin(open_agent_turn(true, |_| {})).await;
    assert_eq!(on.reply, off.reply, "same published reply");
    assert_eq!(on.tools, off.tools, "same provider tool list");
    assert_eq!(on.tools_fingerprint, off.tools_fingerprint, "same tool fingerprint");
    assert_eq!(on.executions, off.executions, "same executions");
    assert_eq!(off.executions, vec!["project_tool".to_owned()]);
    assert_eq!(on.provider_requests, off.provider_requests, "same provider requests");
    assert!(off.systems.iter().all(|text| !text.contains(BRIEF_MARKER)));
    assert!(on.systems.iter().any(|text| text.contains(BRIEF_MARKER)));
    let without_brief = |systems: &[String]| -> Vec<String> {
        systems
            .iter()
            .map(|text| {
                // The brief is one block (its rows end at a blank line); the
                // coalesced system prompt may continue after it.
                text.find(BRIEF_MARKER).map_or_else(
                    || text.trim().to_owned(),
                    |at| {
                        let end = text[at..].find("\n\n").map_or(text.len(), |offset| at + offset);
                        format!("{}{}", text[..at].trim_end(), &text[end..]).trim().to_owned()
                    },
                )
            })
            .filter(|text| !text.is_empty())
            // Each run owns its temporary workspace; that line differs by
            // construction, not because of the engine.
            .map(|text| {
                text.lines()
                    .filter(|line| !line.starts_with("Working directory: "))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect()
    };
    assert_eq!(
        without_brief(&on.systems),
        without_brief(&off.systems),
        "engine-on adds only the brief"
    );

    // The opened turns themselves are identical.
    let (off_refs, off_published) = opened_turn_refs(false).await;
    let (on_refs, on_published) = opened_turn_refs(true).await;
    assert!(
        off_refs
            .iter()
            .any(|reference| reference.id.as_str().contains("project_tool")),
        "the fixture tool is an execution ref: {off_refs:?}"
    );
    assert_eq!(on_refs, off_refs, "same execution refs");
    assert_eq!(on_published, off_published, "same published surface");
}

/// The execution refs and published surface of one turn opened through the
/// production path with the engine `enabled` or not.
async fn opened_turn_refs(
    enabled: bool,
) -> (
    std::collections::BTreeSet<crate::tools::capability::contract::CapabilityRef>,
    std::collections::BTreeSet<crate::tools::capability::contract::CapabilityRef>,
) {
    let workspace = tempfile::tempdir().expect("test: opened-turn workspace");
    let mut config = engine_off_config(workspace.path());
    config.decision_engine.enabled = enabled;
    config.tool_tiering.always_include.push("project_tool".into());
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(RecordingTool {
        name: "project_tool".to_owned(),
        execution_order: Arc::new(Mutex::new(Vec::new())),
    }) as Box<dyn Tool>]);
    let admitted = AdmittedTurn::with_config(config, "run the project tool", registry).await;
    let decision = admitted.decision();
    let context = decision
        .service
        .open_turn_context(
            decision.proof,
            crate::runtime::turn_context::AgentTurnKind::SingleShot,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("test: production turn opening");
    (context.execution_refs().clone(), context.runtime().published.clone())
}

/// R9 (P1-I1): a backend that predicts Refuse or Clarify with full
/// confidence (a real scripted prediction through the advisory driver, not
/// a policy knob) changes nothing but the advice: the turn sends exactly one
/// provider request with the same tools and publishes the same reply as the
/// engine-off turn.
#[tokio::test]
async fn high_confidence_refuse_or_clarify_advice_never_changes_the_turn() {
    use crate::decision_engine::IntentValue;
    let mut served = Vec::new();
    for prefer in [None, Some("refuse"), Some("clarify")] {
        let workspace = tempfile::tempdir().expect("test: advice workspace");
        let mut config = engine_off_config(workspace.path());
        config.tool_tiering.always_include.push("project_tool".into());
        let endpoint = match prefer {
            None => None,
            Some(prefer) => Some(ScriptedDecisionEndpoint::start(prefer).await),
        };
        if let Some(endpoint) = &endpoint {
            config.decision_engine.enabled = true;
            endpoint.configure(&mut config);
        }
        let dispatches = Arc::new(Mutex::new(Vec::new()));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(RecordingTool {
            name: "project_tool".to_owned(),
            execution_order: Arc::clone(&dispatches),
        }) as Box<dyn Tool>]);
        let admitted = AdmittedTurn::with_config(config.clone(), "run the project tool", Arc::clone(&registry)).await;
        let decision = admitted.decision();
        let cancellation = tokio_util::sync::CancellationToken::new();
        // Production entries open turns under a cleanup owner, which owns the
        // turn's accounting scope.
        let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let context = cleanup
            .scope(decision.service.open_turn_context(
                decision.proof,
                crate::runtime::turn_context::AgentTurnKind::SingleShot,
                &cancellation,
            ))
            .await
            .expect("test: production turn opening");
        let advice = match (prefer, &endpoint) {
            (Some(prefer), Some(endpoint)) => {
                // The publication's own advisory driver, whose factory
                // `from_config` built from the configured backend.
                let runtime = context.runtime();
                let scope = Arc::clone(
                    runtime
                        .accounting
                        .as_ref()
                        .expect("test: a production-opened turn has an accounting scope"),
                );
                let before = scope.remaining_budgets();
                let analysis = decision
                    .service
                    .advisory()
                    .analyze_turn(runtime, &cancellation)
                    .await
                    .expect("test: the configured backend's prediction is analysed");
                assert_eq!(endpoint.requests(), 1, "{prefer}: one request reached the endpoint");
                let after = scope.remaining_budgets();
                assert_eq!(
                    before.tokens - after.tokens,
                    1_200,
                    "{prefer}: the attempt's reported tokens are charged to the turn"
                );
                assert!(
                    after.micro_usd < before.micro_usd,
                    "{prefer}: the attempt's cost is charged to the turn"
                );
                let Some(IntentValue::Choice {
                    probabilities,
                    confidence,
                }) = analysis.global.get("next_action")
                else {
                    panic!("{prefer}: no next-action prediction: {:?}", analysis.global);
                };
                assert_eq!(probabilities.get(prefer), Some(&1.0), "{prefer} is predicted");
                assert_eq!(*confidence, 1.0, "{prefer} with full confidence");
                Some(analysis)
            }
            _ => None,
        };
        let published = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = EngineOffProvider {
            calls: Arc::clone(&calls),
            published: Arc::clone(&published),
            responses: Mutex::new(
                vec![ChatResponse {
                    text: Some("done".into()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                }]
                .into(),
            ),
        };
        let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("run the project tool")];
        let (outcome, _) = run_turn(
            &mut history,
            TurnRequest {
                advice,
                channel_name: "wacli",
                scope: None,
                mode: ChatMode::default(),
                silent: false,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: TurnSchedule {
                    read_only_tool_concurrency_window: 1,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: None,
                decision: TurnDecision {
                    service: decision.service,
                    context: &context,
                    runtime: context.runtime(),
                    proof: decision.proof,
                },
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
        .expect("test: the turn is served");
        if let Some(endpoint) = endpoint {
            endpoint.stop().await;
        }
        served.push((
            outcome.into_text(),
            calls.load(std::sync::atomic::Ordering::SeqCst),
            published.lock().expect("test: published tools").clone(),
            dispatches.lock().expect("test: dispatches").clone(),
        ));
    }
    let (off, advised) = served.split_first().expect("test: engine-off turn");
    assert_eq!(off.0, "done", "the engine-off reply is published");
    assert_eq!(off.1, 1, "one provider request");
    assert!(off.2.iter().any(|name| name == "project_tool"), "the tool is offered");
    for on in advised {
        assert_eq!(on, off, "Refuse/Clarify advice changes nothing but the brief");
    }
}

/// R9.5 / P1-I3: a priced analysis that does not fit the turn's remaining
/// budget plus the first send's minimum is skipped (no backend attempt, no
/// analysis); with ample budget it runs on the turn's own accounting scope.
/// An engine-on turn whose analysis is skipped is still served, without a
/// brief.
#[tokio::test]
async fn tight_budget_skips_analysis_and_never_refuses_the_request() {
    use crate::runtime::decision_advisory::DecisionAdvisory;
    use crate::runtime::decision_advisory::tests::{RecordingBackend, analysis_input, enabled_config};
    let temp = tempfile::tempdir().expect("test: budget workspace");
    let mut config = (*enabled_config(temp.path())).clone();
    config.decision_engine.backend = Some(crate::config::schema::causal_tree::DecisionBackendConfig {
        kind: crate::config::schema::causal_tree::BackendSelection::Fixed(
            crate::config::schema::causal_tree::BackendKind::Mock,
        ),
        model: "recording-model".into(),
        ..Default::default()
    });
    crate::runtime::decision_advisory::tests::price_mock_model(&mut config, "recording-model");
    let config = Arc::new(config);
    let key = crate::decision_engine::state::fixtures::key();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let advisory_with = |attempts: &Arc<std::sync::atomic::AtomicUsize>| {
        let factory = crate::decision_engine::prediction::factory::DecisionBackendFactory::from_slots(
            vec![Box::new(RecordingBackend {
                identity: crate::cost::decision_attempt::BackendIdentity {
                    kind: crate::config::schema::causal_tree::BackendKind::Mock,
                    provider: None,
                    configured_model: Arc::from("recording-model"),
                },
                key: Arc::clone(&key),
                attempts: Arc::clone(attempts),
                request_bytes: Arc::new(parking_lot::Mutex::new(Vec::new())),
                prefer: "answer",
            })],
            Arc::clone(&key),
            crate::decision_engine::prediction::outbound::OutboundSecrets::default(),
            crate::decision_engine::prediction::factory::SlotLimits::default(),
        );
        DecisionAdvisory::new(Arc::clone(&config), Some(factory))
    };

    // Tight: the remaining budget cannot pay the worst case plus the first send.
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let advisory = advisory_with(&attempts);
    let mut tight = analysis_input(&config, &key, advisory.request_limits());
    tight.remaining = (1, 1);
    tight.first_send_minimum = Some((1, 1));
    assert!(advisory.analyze(tight, &cancellation).await.is_none(), "no analysis");
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no backend attempt"
    );

    // Ample: the analysis runs and is charged to the turn's accounting scope.
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let advisory = advisory_with(&attempts);
    let mut ample = analysis_input(&config, &key, advisory.request_limits());
    let scope = Arc::new(crate::runtime::decision_accounting::AccountingScope::new(
        Arc::clone(&config),
        Arc::clone(&key),
        Arc::default(),
        [5; 32],
        crate::tools::capability::turn_state::TurnBudgets {
            tokens: 8_000,
            latency_ms: 30_000,
            micro_usd: 1_000_000,
        },
        std::time::Instant::now() + std::time::Duration::from_secs(30),
    ));
    ample.accounting = Arc::clone(&scope);
    ample.remaining = (8_000, 1_000_000);
    ample.first_send_minimum = Some((128, 10_000));
    let before = scope.remaining_budgets();
    assert!(advisory.analyze(ample, &cancellation).await.is_some(), "analysis runs");
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one attempt charged to the turn's accounting scope"
    );
    let after = scope.remaining_budgets();
    assert_eq!(
        before.tokens - after.tokens,
        1_200,
        "reported tokens charged to the turn"
    );
    assert_eq!(
        before.micro_usd - after.micro_usd,
        1_200,
        "reported cost charged to the turn"
    );
    assert!(
        after.tokens >= 128 && after.micro_usd >= 10_000,
        "the protected first send stays admissible"
    );

    // Served: an engine-on turn whose priced analysis cannot be afforded
    // (no price for the remote decision model) is answered without a brief.
    let served = Box::pin(open_agent_turn(true, |config| {
        config.decision_engine.backend = Some(crate::config::schema::causal_tree::DecisionBackendConfig {
            kind: crate::config::schema::causal_tree::BackendSelection::Fixed(
                crate::config::schema::causal_tree::BackendKind::CompatibleHttp,
            ),
            remote_enabled: true,
            endpoint: "https://example.invalid/v1".into(),
            model: "decision-model".into(),
            ..Default::default()
        });
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": "custom:https://example.invalid/v1",
                "model": "decision-model",
                "operations": ["decision_endpoint"],
            }))
            .expect("test: decision model declaration"),
        );
    }))
    .await;
    assert_eq!(served.reply, "done", "the request is served");
    assert_eq!(served.provider_requests, 2, "the turn is not refused or shortened");
    assert_eq!(served.executions, vec!["project_tool".to_owned()]);
    assert!(
        served.systems.iter().all(|text| !text.contains(BRIEF_MARKER)),
        "a skipped analysis adds no brief"
    );

    // Oversized brief: the builtin analysis runs, but its brief does not fit
    // the first send's remaining G6 tokens (capped at the answer estimate),
    // so it is dropped and the request is still served.
    let oversized = {
        use crate::agent::send_admission::{HookReset, TEST_TOKEN_CAP};
        let _reset = HookReset;
        TEST_TOKEN_CAP.set(crate::cost::estimate::candidate_tokens(0));
        Box::pin(open_agent_turn(true, |_| {})).await
    };
    assert_eq!(oversized.reply, "done", "the request is served");
    assert_eq!(oversized.provider_requests, 2, "the turn is not refused or shortened");
    assert!(
        oversized.systems.iter().all(|text| !text.contains(BRIEF_MARKER)),
        "a brief larger than the first send's remaining tokens is dropped"
    );
}

/// P1-01: a configured non-builtin backend runs on a turn opened by the
/// production path with a valid configuration. The publication builds the
/// configured compatible-HTTP backend through `from_config`; a bounded
/// loopback endpoint the test owns answers it. The analysis is bounded in
/// tokens (the G6 slice is at most 8,192), one physical attempt is charged to
/// the turn's own accounting scope, and the first send stays admissible.
#[tokio::test]
async fn configured_backend_runs_on_a_production_opened_turn() {
    let workspace = tempfile::tempdir().expect("test: production-opened workspace");
    let endpoint = ScriptedDecisionEndpoint::start("answer").await;
    let mut config = engine_off_config(workspace.path());
    config.decision_engine.enabled = true;
    endpoint.configure(&mut config);
    config.agent.compaction.max_context_tokens = 131_072;
    config.agent.compaction.max_context_tokens_explicit = true;
    config.agent.compaction.reserve_tokens = 4_096;
    config.causal_tree.policy.extra_token_ratio_limit = 1.0;
    let admitted = AdmittedTurn::with_config(config, "analyse the opened turn", Arc::new(Vec::new())).await;
    let decision = admitted.decision();
    let cancellation = tokio_util::sync::CancellationToken::new();
    // Production entries open turns under a cleanup owner, which owns the
    // turn's accounting scope.
    let cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    let context = cleanup
        .scope(decision.service.open_turn_context(
            decision.proof,
            crate::runtime::turn_context::AgentTurnKind::SingleShot,
            &cancellation,
        ))
        .await
        .expect("test: production turn opening");
    let runtime = context.runtime();
    let scope = Arc::clone(
        runtime
            .accounting
            .as_ref()
            .expect("a production-opened turn has an accounting scope"),
    );
    let before = (runtime.remaining_g6_tokens(), runtime.remaining_micro_usd());
    assert_eq!(before.0, 8_192, "the G6 slice is the token bound of the analysis");
    let analysis = decision.service.advisory().analyze_turn(runtime, &cancellation).await;
    assert!(analysis.is_some(), "the configured backend's analysis runs");
    assert_eq!(endpoint.requests(), 1, "one physical attempt on the configured backend");
    let after = scope.remaining_budgets();
    assert_eq!(
        before.0 - runtime.remaining_g6_tokens(),
        1_200,
        "reported tokens charged to the turn's scope"
    );
    assert!(after.micro_usd < before.1, "reported cost charged to the turn's scope");
    assert!(
        crate::cost::estimate::candidate_tokens(0) <= runtime.remaining_g6_tokens(),
        "the first send's G6 admission still fits"
    );
    endpoint.stop().await;
}

/// A bounded loopback System One endpoint for the configured
/// compatible-HTTP backend. It answers each real compiled request with
/// [`scripted_answers`](crate::runtime::decision_advisory::tests::scripted_answers)
/// predicting `prefer` with full confidence (usage: 1,000 input and 200
/// output tokens) and counts the requests it received. Request bodies are
/// capped, and `stop` ends the listener within a bounded wait.
struct ScriptedDecisionEndpoint {
    endpoint: String,
    requests: Arc<std::sync::atomic::AtomicUsize>,
    stop: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl ScriptedDecisionEndpoint {
    const MODEL: &'static str = "prx-scripted-decision";

    async fn start(prefer: &'static str) -> Self {
        use axum::response::IntoResponse;
        async fn reply(
            axum::extract::State((prefer, requests)): axum::extract::State<(
                &'static str,
                Arc<std::sync::atomic::AtomicUsize>,
            )>,
            axum::Json(body): axum::Json<serde_json::Value>,
        ) -> axum::response::Response {
            requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::runtime::decision_advisory::tests::scripted_answers(&body, prefer).map_or_else(
                |_| axum::http::StatusCode::BAD_REQUEST.into_response(),
                |answer| axum::Json(answer).into_response(),
            )
        }
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("test: loopback decision listener");
        let endpoint = format!(
            "http://{}",
            listener.local_addr().expect("test: loopback decision address")
        );
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = tokio_util::sync::CancellationToken::new();
        let shutdown = stop.clone();
        let app = axum::Router::new()
            .route("/v1/systemone", axum::routing::post(reply))
            .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
            .with_state((prefer, Arc::clone(&requests)));
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Self {
            endpoint,
            requests,
            stop,
            task,
        }
    }

    /// Configure the engine backend as this endpoint: a fixed loopback
    /// compatible-HTTP backend with a declared decision model, priced at one
    /// micro-USD per token.
    fn configure(&self, config: &mut crate::config::Config) {
        let backend = crate::config::schema::causal_tree::DecisionBackendConfig {
            kind: crate::config::schema::causal_tree::BackendSelection::Fixed(
                crate::config::schema::causal_tree::BackendKind::CompatibleHttp,
            ),
            endpoint: self.endpoint.clone(),
            model: Self::MODEL.into(),
            ..Default::default()
        };
        let provider = crate::config::schema::causal_tree::configured_selection(&backend)
            .expect("test: a fixed compatible backend is a configured selection")
            .provider;
        config.decision_engine.backend = Some(backend);
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider": provider,
                "model": Self::MODEL,
                "operations": ["decision_endpoint"],
            }))
            .expect("test: decision model declaration"),
        );
        config.cost.prices.insert(
            crate::config::schema::usage_pricing_key(&provider, Self::MODEL),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
    }

    fn requests(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn stop(self) {
        self.stop.cancel();
        let mut task = self.task;
        if tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
}
