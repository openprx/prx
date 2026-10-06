#![cfg(test)]
use super::*;
use crate::agent::turn_contracts::{ToolLoopOutcome, ToolLoopTrace};
use crate::hooks::HookManager;
use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage, TokenUsageSource};
use crate::observability::NoopObserver;
use crate::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, ChatTrace, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    ToolCall, ToolCallChunk,
};
use crate::security::SecurityPolicy;
use crate::tools::{Tool, ToolResult};
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const FIXTURE_PROVIDER: &str = "custom:http://127.0.0.1:1/v1";
const FIXTURE_MODEL: &str = "fixture-model";
const FIXTURE_TOOL: &str = "fixture_write";
const FIXTURE_DRAFT: &str = "step-7-1-draft";

#[derive(Debug, Clone)]
enum FixtureStep {
    Tool {
        usage: TokenUsage,
    },
    Final {
        chunks: Vec<String>,
        reasoning: String,
        usage: TokenUsage,
    },
    ContextOverflow,
    Block,
}

fn reported_usage(prompt: u32, completion: u32) -> TokenUsage {
    TokenUsage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        total_tokens: Some(prompt.saturating_add(completion)),
        source: TokenUsageSource::Reported,
        ..TokenUsage::default()
    }
}

fn tool_then_final_script() -> Vec<FixtureStep> {
    vec![
        FixtureStep::Tool {
            usage: reported_usage(10, 2),
        },
        FixtureStep::Final {
            chunks: vec!["final ".to_string(), "answer".to_string()],
            reasoning: "fixture final reasoning".to_string(),
            usage: reported_usage(5, 3),
        },
    ]
}

fn overflow_then_final_script() -> Vec<FixtureStep> {
    vec![
        FixtureStep::ContextOverflow,
        FixtureStep::Final {
            chunks: vec!["recovered".to_string()],
            reasoning: String::new(),
            usage: reported_usage(4, 1),
        },
    ]
}

struct FixtureProvider {
    steps: Mutex<VecDeque<FixtureStep>>,
    captured_histories: Mutex<Vec<Vec<ChatMessage>>>,
    stream_calls: AtomicUsize,
    traced_calls: AtomicUsize,
}

impl FixtureProvider {
    fn new(steps: Vec<FixtureStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            captured_histories: Mutex::new(Vec::new()),
            stream_calls: AtomicUsize::new(0),
            traced_calls: AtomicUsize::new(0),
        }
    }

    fn next_step(&self) -> FixtureStep {
        self.steps.lock().pop_front().expect("fixture step")
    }

    fn capture(&self, messages: &[ChatMessage]) {
        self.captured_histories.lock().push(messages.to_vec());
    }

    fn histories(&self) -> Vec<Vec<ChatMessage>> {
        self.captured_histories.lock().clone()
    }

    fn stream_calls(&self) -> usize {
        self.stream_calls.load(Ordering::SeqCst)
    }

    fn traced_calls(&self) -> usize {
        self.traced_calls.load(Ordering::SeqCst)
    }

    fn response_for(step: FixtureStep) -> anyhow::Result<ChatTrace> {
        let (response, usage) = match step {
            FixtureStep::Tool { usage } => (
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "fixture-call-1".to_string(),
                        name: FIXTURE_TOOL.to_string(),
                        arguments: r#"{"value":"x"}"#.to_string(),
                    }],
                    reasoning_content: Some("fixture tool reasoning".to_string()),
                },
                usage,
            ),
            FixtureStep::Final {
                chunks,
                reasoning,
                usage,
            } => (
                ChatResponse {
                    text: Some(chunks.concat()),
                    tool_calls: Vec::new(),
                    reasoning_content: Some(reasoning),
                },
                usage,
            ),
            FixtureStep::ContextOverflow => {
                anyhow::bail!("context_length_exceeded: shared fixture")
            }
            FixtureStep::Block => {
                anyhow::bail!("blocking fixture must be awaited by the provider adapter")
            }
        };
        let now = chrono::Utc::now();
        Ok(ChatTrace {
            response,
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: FIXTURE_PROVIDER.to_string(),
                model: FIXTURE_MODEL.to_string(),
                started_at: now,
                finished_at: now,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: FIXTURE_PROVIDER.to_string(),
            final_model: FIXTURE_MODEL.to_string(),
            served_member: None,
            tokens_used: usage,
        })
    }

    fn stream_for(step: FixtureStep) -> BoxStream<'static, StreamResult<StreamChunk>> {
        match step {
            FixtureStep::Tool { usage } => stream::iter(vec![
                Ok(StreamChunk::tool_call_chunk(vec![ToolCallChunk::new(
                    "fixture-call-1",
                    FIXTURE_TOOL,
                    r#"{"value":"x"}"#,
                    0,
                )])),
                Ok(StreamChunk::usage(usage)),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed(),
            FixtureStep::Final {
                chunks,
                reasoning,
                usage,
            } => {
                let mut frames = chunks
                    .into_iter()
                    .map(|chunk| Ok(StreamChunk::delta(chunk)))
                    .collect::<Vec<_>>();
                if !reasoning.is_empty() {
                    frames.push(Ok(StreamChunk::reasoning_delta(reasoning)));
                }
                frames.push(Ok(StreamChunk::usage(usage)));
                frames.push(Ok(StreamChunk::final_chunk()));
                stream::iter(frames).boxed()
            }
            FixtureStep::ContextOverflow => stream::iter(vec![Err(crate::providers::traits::StreamError::Provider(
                "context_length_exceeded: shared fixture".to_string(),
            ))])
            .boxed(),
            FixtureStep::Block => stream::pending().boxed(),
        }
    }
}

#[async_trait]
impl Provider for FixtureProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(String::new())
    }

    async fn chat_traced(
        &self,
        request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ChatTrace> {
        self.traced_calls.fetch_add(1, Ordering::SeqCst);
        self.capture(request.messages);
        let step = self.next_step();
        if matches!(step, FixtureStep::Block) {
            return futures::future::pending().await;
        }
        Self::response_for(step)
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
        _options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        self.capture(messages);
        Self::stream_for(self.next_step())
    }
}

struct FixtureTool {
    calls: Arc<AtomicUsize>,
    arguments: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        FIXTURE_TOOL
    }

    fn description(&self) -> &str {
        "Step 7.1 shared characterization tool"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.arguments.lock().push(arguments);
        Ok(ToolResult {
            success: true,
            output: "fixture-tool-ok".to_string(),
            error: None,
        })
    }
}

fn admission_config(workspace: &std::path::Path) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.workspace_dir = workspace.to_path_buf();
    config.causal_tree.enabled = false;
    config.cost.prices.insert(
        FIXTURE_MODEL.to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config
}

fn fixture_tool(calls: Arc<AtomicUsize>, arguments: Arc<Mutex<Vec<serde_json::Value>>>) -> Box<dyn Tool> {
    Box::new(FixtureTool { calls, arguments })
}

fn initial_history() -> Vec<ChatMessage> {
    vec![ChatMessage::system("fixture system"), ChatMessage::user("fixture user")]
}

fn overflow_history() -> Vec<ChatMessage> {
    let mut history = initial_history();
    history.extend((0..30).map(|index| ChatMessage::user(format!("fixture overflow turn {index}"))));
    history
}

async fn run_chat_fixture(
    provider: Arc<FixtureProvider>,
    registry: Option<Arc<Vec<Box<dyn Tool>>>>,
    cancellation: CancellationToken,
) -> Vec<Action> {
    run_chat_fixture_with_history(provider, registry, cancellation, initial_history()).await
}

pub(super) struct ChatFixtureAdmission {
    pub(super) binding: Arc<crate::chat::decision_source::ChatTurnBinding>,
    pub(super) registry: Arc<Vec<Box<dyn Tool>>>,
    pub(super) service: Arc<ToolExecutionService>,
    pub(super) context: crate::tools::ToolExecutionContext,
    pub(super) fabric: crate::memory::MemoryFabric,
}

pub(super) async fn admit_chat_fixture(
    provider: Arc<dyn Provider>,
    registry: Option<Arc<Vec<Box<dyn Tool>>>>,
    workspace: &std::path::Path,
    selection: (&str, &str, Option<&str>),
    source_input: (&str, &str),
    action_tx: mpsc::Sender<Action>,
    cancellation: CancellationToken,
) -> ChatFixtureAdmission {
    admit_chat_fixture_with_config(
        provider,
        registry,
        admission_config(workspace),
        selection,
        source_input,
        action_tx,
        cancellation,
    )
    .await
}

pub(super) async fn admit_chat_fixture_with_config(
    provider: Arc<dyn Provider>,
    registry: Option<Arc<Vec<Box<dyn Tool>>>>,
    config: crate::config::Config,
    selection: (&str, &str, Option<&str>),
    source_input: (&str, &str),
    action_tx: mpsc::Sender<Action>,
    cancellation: CancellationToken,
) -> ChatFixtureAdmission {
    admit_chat_fixture_with_mode(
        provider,
        registry,
        config,
        selection,
        (
            source_input.0,
            source_input.1,
            crate::agent::turn_contracts::ChatMode::Edit,
        ),
        action_tx,
        cancellation,
    )
    .await
}

async fn admit_chat_fixture_with_mode(
    provider: Arc<dyn Provider>,
    registry: Option<Arc<Vec<Box<dyn Tool>>>>,
    mut config: crate::config::Config,
    selection: (&str, &str, Option<&str>),
    source_input: (&str, &str, crate::agent::turn_contracts::ChatMode),
    action_tx: mpsc::Sender<Action>,
    cancellation: CancellationToken,
) -> ChatFixtureAdmission {
    let workspace_path = config.workspace_dir.clone();
    let workspace = workspace_path.as_path();
    let ledger: Arc<dyn crate::memory::Memory> =
        Arc::new(crate::memory::SqliteMemory::new(workspace).expect("chat fixture sqlite"));
    let registry = registry.unwrap_or_else(|| Arc::new(Vec::new()));
    if let Some(endpoint) = selection.2 {
        config.api_url = Some(endpoint.to_owned());
        config.api_key = Some("test-key".to_owned());
    }
    config.cost.prices.insert(
        selection.1.to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    config.providers.models.push(
        serde_json::from_value(serde_json::json!({
            "provider": selection.0, "model": selection.1,
            "operations": ["chat"], "native_tools": true,
        }))
        .expect("fixture model descriptor"),
    );
    let generation = crate::config::new_shared(config.clone()).pin();
    let policy = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace));
    let service = Arc::new(chat_tool_execution_service(
        Arc::clone(&registry),
        Some(Arc::clone(&ledger)),
        Arc::clone(&policy),
        Arc::new(ApprovalRouter::new()),
        action_tx.clone(),
        cancellation.clone(),
        None,
    ));
    let diagnostics = crate::hooks::HookDiagnostics {
        configured: false,
        valid: true,
        config_path: String::new(),
        timeout_ms: 0,
        action_count: 0,
        events: std::collections::BTreeMap::new(),
        catalog_digest: None,
        error: None,
        runs: false,
        blocked: None,
    };
    let provider_catalog = crate::providers::catalog::CatalogView::from_config(&config);
    let decisions = crate::runtime::turn_decision::build_turn_decision_service(
        crate::causal_tree::state::TurnSurfaceKind::Interactive,
        crate::runtime::turn_decision::TurnServiceBuild {
            parent_grant: None,
            generation: Arc::clone(&generation),
            registry: Arc::clone(&registry),
            execution: Arc::clone(&service),
            catalog_sources: crate::tools::capability::catalog::CatalogSources::from_runtime_inputs(
                generation.id.0.to_string(),
                &config,
                Vec::new(),
                &diagnostics,
                0,
            ),
            surface_policy: crate::tools::capability::catalog::SurfacePolicy {
                excluded_names: config.tool_tiering.always_exclude.iter().cloned().collect(),
                direct_names: config
                    .tool_tiering
                    .always_include
                    .iter()
                    .filter(|name| !config.tool_tiering.always_exclude.contains(name))
                    .cloned()
                    .collect(),
                ..Default::default()
            },
            provider_catalog: &provider_catalog,
            transports: &crate::agent::test_admission::NoExternalTransport,
            events: Arc::new(crate::agent::test_admission::SilentEvents),
            memory: Arc::clone(&ledger),
            security: policy,
        },
    )
    .expect("actual chat decision service");
    let envelope = crate::chat::chat_runtime_envelope(&workspace.to_string_lossy(), "chat:characterization")
        .with_config_generation(&generation)
        .with_run_id(source_input.0.to_owned());
    let fabric = crate::memory::MemoryFabric::new(Arc::clone(&ledger), workspace.to_string_lossy())
        .with_event_recording(config.memory.event_recording_config());
    let input = source_input.1;
    let event = fabric
        .record_inbound_user_message(envelope.message_scope(), input, None, None)
        .await
        .expect("recorded chat source");
    let source = crate::chat::decision_source::ChatSourceReceipt::issue(
        ledger.as_ref(),
        &generation,
        "chat:characterization",
        source_input.0,
        input,
        &event,
    )
    .await
    .expect("durable chat source proof");
    let selected =
        crate::chat::decision_source::ChatProviderBinding::capture(&generation, &provider, selection.0, selection.1)
            .expect("chat model binding")
            .expect("selected chat model");
    let binding = source
        .bind_provider(
            &selected,
            crate::chat::decision_source::ChatAdmissionMode::Model(source_input.2),
        )
        .expect("source/provider binding");
    // The scenarios cancel after admission, while the provider turn is running.
    binding
        .decide(&decisions, &CancellationToken::new())
        .await
        .expect("chat decision");
    let context = crate::tools::ToolExecutionContext::new(binding.envelope().clone(), "private");

    ChatFixtureAdmission {
        binding,
        registry,
        service,
        context,
        fabric,
    }
}

async fn run_chat_fixture_with_history(
    provider: Arc<FixtureProvider>,
    registry: Option<Arc<Vec<Box<dyn Tool>>>>,
    cancellation: CancellationToken,
    history: Vec<ChatMessage>,
) -> Vec<Action> {
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(128);
    let ledger_dir = tempfile::TempDir::new().expect("chat fixture ledger");
    let provider: Arc<dyn Provider> = provider;
    let input = &history
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .expect("source input")
        .content;
    let admitted = admit_chat_fixture(
        Arc::clone(&provider),
        registry,
        ledger_dir.path(),
        (FIXTURE_PROVIDER, FIXTURE_MODEL, None),
        (FIXTURE_DRAFT, input),
        action_tx.clone(),
        cancellation.clone(),
    )
    .await;

    drive_start_turn_stream(
        Some(admitted.binding),
        None,
        provider,
        history.clone(),
        history,
        FIXTURE_MODEL.to_string(),
        0.0,
        None,
        cancellation,
        FIXTURE_DRAFT.to_string(),
        action_tx.clone(),
        Some(admitted.registry),
        Some(admitted.service),
        admitted.context,
        admitted.fabric,
        None,
        crate::agent::turn_contracts::ChatMode::Edit,
        Arc::new(crate::observability::noop::NoopObserver),
        Arc::new(crate::hooks::HookManager::new(std::path::PathBuf::new())),
    )
    .await;

    drop(action_tx);
    let mut actions = Vec::new();
    while let Some(action) = action_rx.recv().await {
        actions.push(action);
    }
    actions
}

#[tokio::test]
async fn queued_plan_mode_change_fails_before_fixed_reply_and_commits_one_failed_terminal() {
    use crate::agent::turn_contracts::ChatMode;
    let dir = tempfile::tempdir().unwrap();
    let config = admission_config(dir.path());
    let provider = Arc::new(FixtureProvider::new(Vec::new()));
    let provider_port: Arc<dyn Provider> = provider.clone();
    let (action_tx, mut action_rx) = mpsc::channel(8);
    let cancellation = CancellationToken::new();
    let admitted = admit_chat_fixture_with_mode(
        provider_port.clone(),
        None,
        config.clone(),
        (FIXTURE_PROVIDER, FIXTURE_MODEL, None),
        (FIXTURE_DRAFT, "prx queued plan input", ChatMode::Plan),
        action_tx.clone(),
        cancellation.clone(),
    )
    .await;
    let owner = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
    crate::chat::decision_source::admission_failure_tests::install_fixture_terminal(
        &admitted.binding,
        &owner,
        admitted.fabric.clone(),
    );
    assert!(admitted.binding.fixed_reply().unwrap().is_some());
    drive_start_turn_stream(
        Some(Arc::clone(&admitted.binding)),
        None,
        provider_port,
        initial_history(),
        initial_history(),
        FIXTURE_MODEL.to_owned(),
        0.0,
        None,
        cancellation,
        FIXTURE_DRAFT.to_owned(),
        action_tx,
        Some(admitted.registry),
        Some(admitted.service),
        admitted.context,
        admitted.fabric.clone(),
        None,
        ChatMode::Auto,
        Arc::new(crate::observability::noop::NoopObserver),
        Arc::new(crate::hooks::HookManager::new(dir.path().to_path_buf())),
    )
    .await;
    let action = action_rx.recv().await.expect("mode mismatch action");
    let Some(TurnOutcomeKind::Failed { err, retryable }) = extract_turn_outcome(&action) else {
        panic!("mode mismatch did not produce a failed outcome");
    };
    assert!(!retryable);
    assert_eq!(err, "chat mode changed after admission");
    assert_eq!(provider.stream_calls(), 0);
    assert_eq!(provider.traced_calls(), 0);
    crate::chat::finalize_chat_non_success_turn(
        Some(&admitted.binding),
        &admitted.fabric,
        FIXTURE_DRAFT,
        admitted.binding.envelope().message_scope(),
        crate::agent::terminal::TurnTerminalStatus::Failed,
        None,
        chrono::Utc::now(),
        &err,
        &config.cost,
        dir.path(),
    )
    .await
    .expect("actual failed terminal owner");
    let memory = admitted.fabric.memory();
    let final_event = memory
        .find_message_event_by_idempotency_key(admitted.fabric.workspace_id(), &format!("turn:{FIXTURE_DRAFT}:final"))
        .await
        .unwrap()
        .expect("one idempotent terminal");
    assert_eq!(final_event.event_type, "turn.finalized");
    let payload: serde_json::Value = serde_json::from_str(final_event.raw_payload_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        payload.get("status").and_then(serde_json::Value::as_str),
        Some("failed")
    );
    assert!(payload.get("provider_outcome").is_none_or(serde_json::Value::is_null));
    assert!(payload.get("assistant_event_id").is_none_or(serde_json::Value::is_null));
    drop(admitted.binding);
    owner
        .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
        .await
        .unwrap();
}

async fn run_agent_fixture(
    provider: Arc<FixtureProvider>,
    tools: Vec<Box<dyn Tool>>,
    cancellation: CancellationToken,
    on_delta: Option<mpsc::Sender<String>>,
    on_tool_call: Option<mpsc::Sender<crate::agent::turn_contracts::ToolCallNotification>>,
) -> (anyhow::Result<(ToolLoopOutcome, ToolLoopTrace)>, Vec<ChatMessage>) {
    run_agent_fixture_with_history(provider, tools, cancellation, on_delta, on_tool_call, initial_history()).await
}

async fn run_agent_fixture_with_history(
    provider: Arc<FixtureProvider>,
    tools: Vec<Box<dyn Tool>>,
    cancellation: CancellationToken,
    on_delta: Option<mpsc::Sender<String>>,
    on_tool_call: Option<mpsc::Sender<crate::agent::turn_contracts::ToolCallNotification>>,
    history: Vec<ChatMessage>,
) -> (anyhow::Result<(ToolLoopOutcome, ToolLoopTrace)>, Vec<ChatMessage>) {
    let temp = tempfile::TempDir::new().expect("fixture tempdir");
    let memory: Arc<dyn crate::memory::Memory> =
        Arc::new(crate::memory::SqliteMemory::new(temp.path()).expect("agent fixture sqlite"));
    let mut history = history;
    let tools = crate::agent::test_admission::registry_with_search(tools);
    let admission = crate::agent::test_admission::AdmittedTurn::with_model(
        admission_config(temp.path()),
        &history
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .expect("source input")
            .content,
        Arc::clone(&tools),
        memory,
        (FIXTURE_PROVIDER, FIXTURE_MODEL),
    )
    .await;
    let result = crate::agent::turn_engine::run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: crate::agent::turn_contracts::ChatMode::Edit,
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: provider.as_ref(),
            provider_name: FIXTURE_PROVIDER,
            model: FIXTURE_MODEL,
            temperature: 0.0,
            tools_registry: tools,
            observer: &NoopObserver,
            hooks: &HookManager::new(temp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: admission.memory_runtime_with_ingest(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: Some(cancellation),
            on_delta,
            on_tool_call,
        },
    )
    .await;
    (result, history)
}

fn message_roles(history: &[ChatMessage]) -> Vec<&str> {
    history.iter().map(|message| message.role.as_str()).collect()
}

async fn wait_for_call_count(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while counter.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture provider call should start");
}

#[tokio::test]
async fn step_7_1_same_success_fixture_characterizes_stream_tool_usage_history_and_terminal() {
    let chat_provider = Arc::new(FixtureProvider::new(tool_then_final_script()));
    let chat_tool_calls = Arc::new(AtomicUsize::new(0));
    let chat_arguments = Arc::new(Mutex::new(Vec::new()));
    let chat_registry = crate::agent::test_admission::registry_with_search(vec![fixture_tool(
        Arc::clone(&chat_tool_calls),
        Arc::clone(&chat_arguments),
    )]);
    let chat_actions = run_chat_fixture(
        Arc::clone(&chat_provider),
        Some(chat_registry),
        CancellationToken::new(),
    )
    .await;

    let agent_provider = Arc::new(FixtureProvider::new(tool_then_final_script()));
    let agent_tool_calls = Arc::new(AtomicUsize::new(0));
    let agent_arguments = Arc::new(Mutex::new(Vec::new()));
    let (delta_tx, mut delta_rx) = mpsc::channel::<String>(16);
    let (tool_tx, mut tool_rx) = mpsc::channel(16);
    let (agent_result, agent_history) = run_agent_fixture(
        Arc::clone(&agent_provider),
        vec![fixture_tool(
            Arc::clone(&agent_tool_calls),
            Arc::clone(&agent_arguments),
        )],
        CancellationToken::new(),
        Some(delta_tx),
        Some(tool_tx),
    )
    .await;
    let (agent_outcome, agent_trace) = agent_result.expect("agent fixture succeeds");

    assert_eq!(
        chat_provider.stream_calls(),
        2,
        "shared Agent owner must preserve Chat's real streaming adapter"
    );
    assert_eq!(chat_provider.traced_calls(), 0);
    assert_eq!(agent_provider.stream_calls(), 0);
    assert_eq!(
        agent_provider.traced_calls(),
        2,
        "the same owner keeps the buffered adapter for non-TUI callers"
    );

    assert_eq!(chat_tool_calls.load(Ordering::SeqCst), 1);
    assert_eq!(agent_tool_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        chat_arguments
            .lock()
            .first()
            .and_then(|arguments| arguments.get("value"))
            .and_then(serde_json::Value::as_str),
        Some("x")
    );
    assert_eq!(
        agent_arguments
            .lock()
            .first()
            .and_then(|arguments| arguments.get("value"))
            .and_then(serde_json::Value::as_str),
        Some("x")
    );

    let chat_deltas = chat_actions
        .iter()
        .filter_map(|action| match action {
            Action::StreamChunkReceived { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(chat_deltas, ["final ", "answer"]);
    let mut agent_deltas = Vec::new();
    while let Ok(delta) = delta_rx.try_recv() {
        agent_deltas.push(delta);
    }
    assert_eq!(agent_deltas.concat(), "final answer");

    let chat_usage = chat_actions.iter().find_map(|action| match action {
        Action::StreamUsageMetered { usage, .. } => Some(usage.clone()),
        _ => None,
    });
    assert_eq!(chat_usage.as_ref(), Some(&agent_trace.tokens_used));
    assert_eq!(agent_trace.tokens_used.total_tokens, Some(20));

    let chat_final = chat_actions
        .iter()
        .filter_map(|action| match action {
            Action::StreamCompleted {
                final_text, reasoning, ..
            } => Some((final_text.as_str(), reasoning.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(chat_final.len(), 1, "Chat emits one terminal action");
    let (chat_final_text, chat_final_reasoning) = chat_final.first().copied().expect("Chat terminal payload");
    assert_eq!(chat_final_text, "final answer");
    assert!(chat_final_reasoning.contains("fixture final reasoning"));
    assert!(matches!(agent_outcome, ToolLoopOutcome::Text(ref text) if text == "final answer"));
    assert_eq!(
        agent_history.last().map(|message| message.content.as_str()),
        Some("final answer")
    );

    assert_eq!(
        chat_actions
            .iter()
            .filter(|action| matches!(action, Action::RecordAssistantTurn { .. }))
            .count(),
        1,
        "Chat commits final assistant history through an action before terminal reduction"
    );
    assert_eq!(
        chat_actions
            .iter()
            .filter(|action| matches!(action, Action::ToolStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        chat_actions
            .iter()
            .filter(|action| matches!(action, Action::ToolFinished { success: true, .. }))
            .count(),
        1
    );
    let mut agent_started = 0;
    let mut agent_finished = 0;
    while let Ok(event) = tool_rx.try_recv() {
        match event {
            crate::agent::turn_contracts::ToolCallNotification::Started { .. } => agent_started += 1,
            crate::agent::turn_contracts::ToolCallNotification::Finished { success: true, .. } => agent_finished += 1,
            crate::agent::turn_contracts::ToolCallNotification::Finished { success: false, .. }
            | crate::agent::turn_contracts::ToolCallNotification::Progress { .. } => {}
        }
    }
    assert_eq!((agent_started, agent_finished), (1, 1));

    let chat_histories = chat_provider.histories();
    let agent_histories = agent_provider.histories();
    assert_eq!(chat_histories.len(), 2);
    assert_eq!(agent_histories.len(), 2);
    let chat_follow_up = chat_histories.get(1).expect("Chat follow-up provider history");
    let agent_follow_up = agent_histories.get(1).expect("Agent follow-up provider history");
    assert_eq!(message_roles(chat_follow_up), ["system", "user", "assistant", "tool"]);
    assert_eq!(message_roles(agent_follow_up), ["system", "user", "assistant", "tool"]);
    let chat_tool_message = chat_follow_up.get(3).expect("Chat tool result history");
    let agent_tool_message = agent_follow_up.get(3).expect("Agent tool result history");
    assert!(
        !chat_tool_message.content.contains("\"success\":true"),
        "UI status must not leak into provider-bound tool history"
    );
    assert_eq!(
        chat_tool_message.content, agent_tool_message.content,
        "Chat and buffered Agent adapters must now use one canonical tool-result payload"
    );
}

#[tokio::test]
async fn step_7_1_same_overflow_fixture_characterizes_recovery_signals() {
    let chat_provider = Arc::new(FixtureProvider::new(overflow_then_final_script()));
    let chat_actions = run_chat_fixture_with_history(
        Arc::clone(&chat_provider),
        None,
        CancellationToken::new(),
        overflow_history(),
    )
    .await;
    let agent_provider = Arc::new(FixtureProvider::new(overflow_then_final_script()));
    let (agent_result, agent_history) = run_agent_fixture_with_history(
        Arc::clone(&agent_provider),
        Vec::new(),
        CancellationToken::new(),
        None,
        None,
        overflow_history(),
    )
    .await;

    assert_eq!(chat_provider.stream_calls(), 2);
    assert_eq!(agent_provider.traced_calls(), 2);
    assert!(chat_actions.iter().any(|action| matches!(
        action,
        Action::HistoryCompacted {
            reason: crate::chat::action::CompactReason::ContextOverflow
        }
    )));
    assert!(chat_actions.iter().any(|action| matches!(
        action,
        Action::StreamCompleted { final_text, .. } if final_text == "recovered"
    )));
    let (outcome, _) = agent_result.expect("agent overflow retry succeeds");
    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "recovered"));
    assert_eq!(
        agent_history.last().map(|message| message.content.as_str()),
        Some("recovered")
    );
}

#[tokio::test]
async fn step_7_1_same_blocking_fixture_characterizes_cancellation_terminals() {
    let chat_provider = Arc::new(FixtureProvider::new(vec![FixtureStep::Block]));
    let chat_cancel = CancellationToken::new();
    let chat_task = tokio::spawn(run_chat_fixture(Arc::clone(&chat_provider), None, chat_cancel.clone()));
    wait_for_call_count(&chat_provider.stream_calls, 1).await;
    chat_cancel.cancel();
    let chat_actions = chat_task.await.expect("chat fixture join");
    assert_eq!(
        chat_actions
            .iter()
            .filter(|action| matches!(action, Action::StreamCancelled { .. }))
            .count(),
        1,
        "Chat cancellation is one typed terminal action"
    );
    assert!(
        !chat_actions
            .iter()
            .any(|action| matches!(action, Action::StreamCompleted { .. } | Action::StreamFailed { .. }))
    );

    let agent_provider = Arc::new(FixtureProvider::new(vec![FixtureStep::Block]));
    let agent_cancel = CancellationToken::new();
    let agent_task = tokio::spawn(run_agent_fixture(
        Arc::clone(&agent_provider),
        Vec::new(),
        agent_cancel.clone(),
        None,
        None,
    ));
    wait_for_call_count(&agent_provider.traced_calls, 1).await;
    agent_cancel.cancel();
    let (agent_result, agent_history) = agent_task.await.expect("agent fixture join");
    let error = agent_result.expect_err("agent cancellation returns an error terminal");
    assert!(error.to_string().contains("cancel"));
    assert_eq!(message_roles(&agent_history), ["system", "user"]);
    assert_eq!(
        agent_history.first().map(|message| message.content.as_str()),
        Some("fixture system")
    );
    assert_eq!(
        agent_history.get(1).map(|message| message.content.as_str()),
        Some("fixture user")
    );
}
