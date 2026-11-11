#![cfg(test)]

//! Provider overflow through actual StartLLMTurn, event sink and ordered commit.
use super::*;
use crate::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError, StreamOptions,
    StreamResult, ToolCallChunk,
};
use futures_util::{StreamExt, stream};

const REQUEST: &str = "Use file_read on overflow-probe.txt nine times.";
const CONTENT: &str = "prx overflow probe content";

struct OverflowProvider {
    calls: AtomicUsize,
    rounds: usize,
    always_overflow: bool,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
}

impl OverflowProvider {
    fn response(&self, messages: &[ChatMessage]) -> anyhow::Result<ChatResponse> {
        self.requests.lock().push(messages.to_vec());
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.rounds {
            return Ok(ChatResponse {
                text: None,
                tool_calls: vec![crate::providers::ToolCall {
                    id: format!("overflow-read-{call}"),
                    name: "file_read".into(),
                    arguments: r#"{"path":"overflow-probe.txt"}"#.into(),
                }],
                reasoning_content: None,
            });
        }
        if self.always_overflow || call == self.rounds {
            anyhow::bail!("context window exceeded by scripted provider");
        }
        Ok(ChatResponse {
            text: Some("probe complete".into()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

#[async_trait::async_trait]
impl Provider for OverflowProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: self.rounds > 0,
            vision: false,
        }
    }
    fn supports_streaming(&self) -> bool {
        true
    }
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        Ok(
            "## Decisions\nRetain completed work.\n## Constraints\nPreserve the request.\n## Pending\nFinish the task."
                .into(),
        )
    }
    async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        let requirement = if self.rounds > 0 {
            super::super::assessment_fixture::EXECUTION_REQUIRED
        } else {
            super::super::assessment_fixture::INFORMATIONAL
        };
        if let Some(response) = super::super::assessment_fixture::response(&request, requirement) {
            return Ok(response);
        }
        self.response(request.messages)
    }
    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        _: &str,
        _: f64,
        _: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let chunks = match self.response(messages) {
            Ok(response) => {
                let mut chunks = Vec::new();
                if let Some(text) = response.text {
                    chunks.push(Ok(StreamChunk::delta(text)));
                }
                if !response.tool_calls.is_empty() {
                    chunks.push(Ok(StreamChunk::tool_call_chunk(
                        response
                            .tool_calls
                            .into_iter()
                            .enumerate()
                            .map(|(index, call)| ToolCallChunk::new(call.id, call.name, call.arguments, index))
                            .collect(),
                    )));
                }
                chunks.push(Ok(StreamChunk::final_chunk()));
                chunks
            }
            Err(error) => vec![Err(StreamError::Provider(error.to_string()))],
        };
        stream::iter(chunks).boxed()
    }
}

#[tokio::test]
async fn overflow_patch_and_ordered_commit_preserve_one_pending_request() {
    crate::runtime::turn_decision::with_scored_prediction(async {
        let (mut deps, mut actions, _hooks, temp) = build_deps(Arc::new(NoneMemory::new()), CancellationToken::new());
        std::fs::write(temp.path().join("overflow-probe.txt"), CONTENT).expect("real read fixture");
        let security = Arc::new(crate::security::SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Full,
            workspace_dir: temp.path().to_path_buf(),
            ..crate::security::SecurityPolicy::default()
        });
        deps.tool_security_policy = security.clone();
        deps.tools_registry = Some(Arc::new(vec![Box::new(crate::tools::FileReadTool::new(security, false))]));
        let provider = Arc::new(OverflowProvider { calls: AtomicUsize::new(0), rounds: 9, always_overflow: false, requests: Mutex::new(Vec::new()) });
        deps.provider = provider.clone();
        let fixture_deps = deps.clone();
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new()).expect("cleanup owner");
        cleanup.run(async {
            let binding = admit_start_turn_fixture(&fixture_deps, "prx-overflow-reducer", REQUEST, true).await?;
            let executor = EffectExecutor::new_with_deps(deps);
            let mut state = ChatState::new(Arc::from("ollama"), Arc::from("test-model"), CancellationToken::new());
            state.reduce(Action::RecordSystemMessage { content: "sys".into() });
            // An identical old input must not stand in for the pending request.
            state.reduce(Action::RecordUserTurn(REQUEST.into()));
            state.reduce(Action::RecordAssistantTurn { task_id: None, content: "older completed turn".into() });
            for i in 0..12 {
                state.reduce(Action::RecordUserTurn(format!("old input {i}")));
                state.reduce(Action::RecordAssistantTurn { task_id: None, content: format!("old reply {i}") });
            }
            let base = state.session.history.clone();
            let mut worker = base.clone();
            worker.push(ChatMessage::user(REQUEST));
            let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
            let task = scheduler.enqueue(REQUEST, crate::chat::turn_scheduler::TurnPriority::Normal, base.len());
            let config = crate::config::AgentCompactionConfig {
                mode: crate::config::AgentCompactionMode::Aggressive,
                max_context_tokens: 1_000_000,
                keep_recent_messages: 12,
                memory_flush: false,
                ..crate::config::AgentCompactionConfig::default()
            };
            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: Some(task), provider_turn_sequence: Some(1), draft_id: "prx-overflow-reducer".into(),
                history: worker.clone(), compaction_guard_history: Some(worker), compaction_config: Some(config),
                cancel: CancellationToken::new(), turn_spawn_ctx: None, turn_message_send_ctx: None, routing_input: None, turn_binding: Some(binding),
            });
            assert_eq!(state.session.history.len(), base.len(), "StartTurn leaves the pending input uncommitted");
            for effect in effects { if matches!(effect, Effect::StartTurn { .. }) { executor.execute(effect).await; } }
            let mut saw_patch = false;
            let (final_text, reasoning) = loop {
                let action = tokio::time::timeout(Duration::from_secs(10), actions.recv()).await.expect("driver progress").expect("driver action");
                match action {
                    Action::HistoryCompactionPatchApplied { .. } => {
                        let effects = state.reduce(action);
                        assert!(effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))), "real reducer must accept the guard");
                        assert_eq!(state.session.history.iter().filter(|message| message.content == REQUEST).count(), 0, "pending input stays with its ordered owner");
                        assert_eq!(state.stream.primary_draft().map(|draft| draft.prompt_preview.as_str()), Some(REQUEST), "Q is still present once in the live draft");
                        assert!(!state.session.history.iter().any(|message| message.role == "tool" || message.content.contains("overflow-read-")));
                        saw_patch = true;
                    }
                    Action::ProviderTurnReadyForCommit { final_text, reasoning, .. } => break (final_text, reasoning),
                    Action::StreamFailed { err, .. } => panic!("overflow must recover: {err}"),
                    action => { state.reduce(action); }
                }
            };
            assert!(saw_patch);
            let (dispatcher, mut commits) = ChatDispatcher::new();
            crate::chat::provider_turns::dispatch_ordered_provider_turn_commit(&dispatcher, task, "prx-overflow-reducer", REQUEST, &final_text, &reasoning, false, true).expect("real ordered commit producer");
            let mut saved = None;
            while let Ok(action) = commits.try_recv() {
                for effect in state.reduce(action) { if let Effect::SaveSession(session) = effect { saved = Some(session); } }
            }
            assert_eq!(state.session.history.iter().filter(|message| message.content == REQUEST).count(), 1);
            assert_eq!(state.session.turns.iter().filter(|turn| turn.content == REQUEST).count(), 1);
            assert_eq!(saved.expect("turn commit saves the session").turns.iter().filter(|turn| turn.content == REQUEST).count(), 1);
            let requests = provider.requests.lock();
            let before = requests.get(9).expect("overflow after nine actual reads");
            assert_eq!(before.iter().filter(|message| message.role == "tool" && message.content.contains(CONTENT)).count(), 9);
            let retry = requests.get(10).expect("recovered provider request");
            assert_eq!(retry.iter().filter(|message| message.content == REQUEST).count(), 1);
            let summary = retry.iter().find(|message| message.role == "assistant"
                && message.content.starts_with("[Context compacted at ")).expect("worker summary").clone();
            let expected = vec![
                ChatMessage::system("sys"), summary,
                ChatMessage::user(REQUEST), ChatMessage::assistant(final_text),
            ];
            let pairs = |messages: &[ChatMessage]| messages.iter().map(|message| (message.role.clone(), message.content.clone())).collect::<Vec<_>>();
            assert_eq!(pairs(&state.session.history), pairs(&expected), "saved view keeps the worker summary and commits Q once without native frames");
            Ok::<_, anyhow::Error>(())
        }).await.expect("actual turn cleanup");
    }).await;
}

#[tokio::test]
async fn engine_capacity_failure_reaches_non_retryable_stream_failed() {
    crate::runtime::turn_decision::with_scored_prediction(async {
        let (mut deps, mut actions, _hooks, _temp) = build_deps(Arc::new(NoneMemory::new()), CancellationToken::new());
        let provider = Arc::new(OverflowProvider {
            calls: AtomicUsize::new(0),
            rounds: 0,
            always_overflow: true,
            requests: Mutex::new(Vec::new()),
        });
        deps.provider = provider.clone();
        let fixture_deps = deps.clone();
        let cleanup =
            crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new()).expect("cleanup owner");
        cleanup
            .run(async {
                let binding =
                    admit_start_turn_fixture(&fixture_deps, "prx-capacity-error", "Describe the project.", false)
                        .await?;
                let executor = EffectExecutor::new_with_deps(deps);
                executor
                    .execute(Effect::StartTurn {
                        provider_turn_task_id: None,
                        draft_id: "prx-capacity-error".into(),
                        history: vec![ChatMessage::system("sys"), ChatMessage::user("Describe the project.")],
                        compaction_guard_history: None,
                        compaction_config: None,
                        cancel: CancellationToken::new(),
                        chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                        turn_spawn_ctx: None,
                        turn_message_send_ctx: None,
                        routing_input: None,
                        turn_binding: Some(binding),
                    })
                    .await;
                loop {
                    let action = tokio::time::timeout(Duration::from_secs(10), actions.recv())
                        .await
                        .expect("capacity action")
                        .expect("driver action");
                    if let Action::StreamFailed { err, retryable, .. } = action {
                        assert!(!retryable);
                        assert!(err.starts_with("Context capacity exceeded"), "{err}");
                        assert!(
                            err.contains("made no progress") && err.contains("request was preserved"),
                            "{err}"
                        );
                        break;
                    }
                    assert!(
                        !matches!(action, Action::StreamCompleted { .. }),
                        "engine must fail with capacity"
                    );
                }
                assert_eq!(
                    provider.calls.load(Ordering::SeqCst),
                    1,
                    "engine NoProgress stops provider replay"
                );
                Ok::<_, anyhow::Error>(())
            })
            .await
            .expect("actual turn cleanup");
    })
    .await;
}
