use super::*;
use crate::agent::loop_::{ToolConcurrencyGovernanceConfig, ToolLoopOutcome, ToolLoopTrace};
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

const FIXTURE_PROVIDER: &str = "turn-characterization";
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
            FixtureStep::ContextOverflow => anyhow::bail!("context_length_exceeded: shared fixture"),
            FixtureStep::Block => anyhow::bail!("blocking fixture must be awaited by the provider adapter"),
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

fn fixture_tool(calls: Arc<AtomicUsize>, arguments: Arc<Mutex<Vec<serde_json::Value>>>) -> Box<dyn Tool> {
    Box::new(FixtureTool { calls, arguments })
}

fn initial_history() -> Vec<ChatMessage> {
    vec![ChatMessage::system("fixture system"), ChatMessage::user("fixture user")]
}

async fn run_chat_fixture(
    provider: Arc<FixtureProvider>,
    registry: Option<Arc<Vec<Box<dyn Tool>>>>,
    cancellation: CancellationToken,
) -> Vec<Action> {
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(128);
    let policy = Arc::new(SecurityPolicy::default());
    let context = chat_tool_execution_context(policy.as_ref(), None, None, FIXTURE_DRAFT);
    let service = registry.as_ref().map(|registry| {
        Arc::new(chat_tool_execution_service(
            Arc::clone(registry),
            Arc::clone(&policy),
            Arc::new(ApprovalRouter::new()),
            action_tx.clone(),
            cancellation.clone(),
            None,
        ))
    });

    drive_start_turn_stream(
        None,
        provider,
        initial_history(),
        initial_history(),
        FIXTURE_MODEL.to_string(),
        0.0,
        None,
        cancellation,
        FIXTURE_DRAFT.to_string(),
        action_tx.clone(),
        registry,
        service,
        context,
        4,
        crate::agent::loop_::ChatMode::Edit,
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

async fn run_agent_fixture(
    provider: Arc<FixtureProvider>,
    tools: Vec<Box<dyn Tool>>,
    cancellation: CancellationToken,
    on_delta: Option<mpsc::Sender<String>>,
    on_tool_call: Option<mpsc::Sender<crate::agent::loop_::ToolCallNotification>>,
) -> (anyhow::Result<(ToolLoopOutcome, ToolLoopTrace)>, Vec<ChatMessage>) {
    let temp = tempfile::TempDir::new().expect("fixture tempdir");
    let mut history = initial_history();
    let result = crate::agent::loop_::run_tool_call_loop_outcome(
        provider.as_ref(),
        &mut history,
        Arc::new(tools),
        &NoopObserver,
        &HookManager::new(temp.path().to_path_buf()),
        FIXTURE_PROVIDER,
        FIXTURE_MODEL,
        0.0,
        true,
        None,
        "terminal",
        &crate::config::MultimodalConfig::default(),
        4,
        false,
        2,
        30,
        false,
        Vec::new(),
        ToolConcurrencyGovernanceConfig::default(),
        None,
        Some(cancellation),
        on_delta,
        None,
        on_tool_call,
        None,
        None,
        crate::agent::loop_::ChatMode::Edit,
        None,
        false,
        None,
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
    let chat_registry = Arc::new(vec![fixture_tool(
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
            crate::agent::loop_::ToolCallNotification::Started { .. } => agent_started += 1,
            crate::agent::loop_::ToolCallNotification::Finished { success: true, .. } => agent_finished += 1,
            crate::agent::loop_::ToolCallNotification::Finished { success: false, .. }
            | crate::agent::loop_::ToolCallNotification::Progress { .. } => {}
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
    let chat_actions = run_chat_fixture(Arc::clone(&chat_provider), None, CancellationToken::new()).await;
    let agent_provider = Arc::new(FixtureProvider::new(overflow_then_final_script()));
    let (agent_result, agent_history) = run_agent_fixture(
        Arc::clone(&agent_provider),
        Vec::new(),
        CancellationToken::new(),
        None,
        None,
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
