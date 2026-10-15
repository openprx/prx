#![cfg(test)]

use super::*;

// ─── S2.5 P1-A: dispatch_or_log failure handling ─────────────────────────

/// S2.5 P1-A: happy path — dispatch_or_log returns Sent and the Action really enqueues.
///
/// Does not assert a change in the drops counter (it is globally shared and parallel tests pollute the reading);
/// the +1 semantics of the drops counter are already covered by the Backpressured/Closed tests.
#[tokio::test]
async fn s2_5_p1_a_dispatch_or_log_normal_sent() {
    let (dispatcher, mut rx) = ChatDispatcher::new();
    let result = dispatcher.dispatch_or_log(Action::CancelRequested, "test.normal");
    assert!(
        matches!(result, DispatchResult::Sent),
        "the happy path must return Sent (got {result:?})"
    );
    // verify the Action really enqueued
    let recv = rx.try_recv().expect("test: action should be in queue");
    assert!(matches!(recv, Action::CancelRequested));
}

/// S2.5 P1-A: full channel — dispatch_or_log returns Backpressured and backpressured goes up by >= 1.
///
/// Because the backpressured counter is globally shared, we assert `after > before` (at least +1)
/// rather than exactly +1, to avoid parallel-test interference; the core contract is that this dispatch inc'd.
#[tokio::test]
async fn s2_5_p1_a_dispatch_or_log_full_warns_and_counts() {
    use crate::observability::chat_metrics;
    let (tx, _rx) = mpsc::channel::<Action>(1);
    let dispatcher = ChatDispatcher { action_tx: tx };
    // fill the capacity of 1.
    let _ = dispatcher.try_dispatch(Action::CancelRequested);

    let before = chat_metrics::get_dispatch_drops_count("backpressured");
    let result = dispatcher.dispatch_or_log(Action::CancelRequested, "test.full");
    assert!(
        matches!(result, DispatchResult::Backpressured),
        "channel full → Backpressured (got {result:?})"
    );
    let after = chat_metrics::get_dispatch_drops_count("backpressured");
    assert!(
        after > before,
        "backpressured counter should >= before+1 on full dispatch (before={before}, after={after})"
    );
}

/// S2.5 P1-A: closed channel — dispatch_or_log returns ChannelClosed and the closed counter goes up by at least 1.
#[tokio::test]
async fn s2_5_p1_a_dispatch_or_log_closed_warns() {
    use crate::observability::chat_metrics;
    let (dispatcher, rx) = ChatDispatcher::new();
    drop(rx);

    let before = chat_metrics::get_dispatch_drops_count("closed");
    let result = dispatcher.dispatch_or_log(Action::CancelRequested, "test.closed");
    assert!(
        matches!(result, DispatchResult::ChannelClosed),
        "channel closed → ChannelClosed (got {result:?})"
    );
    let after = chat_metrics::get_dispatch_drops_count("closed");
    assert!(
        after > before,
        "closed counter should >= before+1 on channel-closed dispatch (before={before}, after={after})"
    );
}

// ─── S3 T3-1 four-part tests ───────────────────────────────────────────────

/// **S3 T3-1 Step 2**: driver path: ToolCallChunks from the streaming protocol can also drive tool execution.
///
/// The provider sends [Streaming delta, Streaming delta, Completed] instead of a single Completed —
/// the driver must still emit ToolStarted/ToolFinished and move to the next pass until the final text.
#[tokio::test]
async fn t31_driver_streaming_tool_call_protocol_executes_correctly() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
        ToolCallChunk, ToolCallChunkStatus,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct PingTool;
    #[async_trait]
    impl crate::tools::Tool for PingTool {
        fn name(&self) -> &str {
            "ping"
        }
        fn description(&self) -> &str {
            "ping"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: "pong".into(),
                error: None,
            })
        }
    }

    struct StreamingToolProvider {
        counter: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for StreamingToolProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
        fn supports_streaming(&self) -> bool {
            true
        }
        fn stream_chat_with_history(
            &self,
            _: &[PMsg],
            _: &str,
            _: f64,
            _: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
            if n == 0 {
                // send using the streaming protocol: 2 Streaming deltas first, then 1 Completed.
                let s1 = ToolCallChunk {
                    id: "call-1".into(),
                    name: "ping".into(),
                    args: String::new(),
                    index: 0,
                    arguments_delta: Some("{".into()),
                    status: ToolCallChunkStatus::Streaming,
                };
                let s2 = ToolCallChunk {
                    id: "call-1".into(),
                    name: "ping".into(),
                    args: String::new(),
                    index: 0,
                    arguments_delta: Some("}".into()),
                    status: ToolCallChunkStatus::Streaming,
                };
                let c = ToolCallChunk {
                    id: "call-1".into(),
                    name: "ping".into(),
                    args: "{}".into(),
                    index: 0,
                    arguments_delta: None,
                    status: ToolCallChunkStatus::Completed,
                };
                stream::iter(vec![
                    Ok(StreamChunk::tool_call_chunk(vec![s1])),
                    Ok(StreamChunk::tool_call_chunk(vec![s2])),
                    Ok(StreamChunk::tool_call_chunk(vec![c])),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            } else {
                stream::iter(vec![Ok(StreamChunk::delta("done")), Ok(StreamChunk::final_chunk())]).boxed()
            }
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(StreamingToolProvider {
        counter: Arc::new(AtomicUsize::new(0)),
    });
    deps.tools_registry = Some(Arc::new(vec![Box::new(PingTool) as Box<dyn crate::tools::Tool>]));
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);

            let binding =
                admit_start_turn_fixture(&fixture_deps, "draft-t31-streaming", "stream the fixture reply", true)
                    .await
                    .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-t31-streaming".into(),
                    history: Vec::new(),
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

            let mut saw_tool_started = false;
            let mut saw_tool_finished = false;
            let mut saw_completion = false;
            for _ in 0..32 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver action within 2s")
                    .expect("must arrive");
                match action {
                    Action::ToolStarted { name, .. } => {
                        assert_eq!(name, "ping");
                        saw_tool_started = true;
                    }
                    Action::ToolFinished { success, name, .. } => {
                        assert_eq!(name, "ping");
                        assert!(success);
                        saw_tool_finished = true;
                    }
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("done"), "want 'done' got {final_text:?}");
                        saw_completion = true;
                        break;
                    }
                    Action::StreamFailed { err, .. } => {
                        panic!("driver should not fail in happy path: {err}")
                    }
                    _ => {}
                }
            }
            assert!(saw_tool_started, "must see ToolStarted");
            assert!(saw_tool_finished, "must see ToolFinished");
            assert!(saw_completion, "must see StreamCompleted");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

#[tokio::test]
async fn dispatcher_tool_call_request_includes_reasoning_in_history() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
        ToolCallChunk,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct PingTool;
    #[async_trait]
    impl crate::tools::Tool for PingTool {
        fn name(&self) -> &str {
            "ping"
        }
        fn description(&self) -> &str {
            "ping"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: "pong".into(),
                error: None,
            })
        }
    }

    struct ReasoningToolProvider {
        counter: Arc<AtomicUsize>,
        second_history: Arc<Mutex<Option<Vec<PMsg>>>>,
    }
    #[async_trait]
    impl Provider for ReasoningToolProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: None,
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
        }
        fn supports_streaming(&self) -> bool {
            true
        }
        fn stream_chat_with_history(
            &self,
            messages: &[PMsg],
            _: &str,
            _: f64,
            _: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
            if n == 0 {
                stream::iter(vec![
                    Ok(StreamChunk::reasoning_delta("Need to call ping.")),
                    Ok(StreamChunk::tool_call_chunk(vec![ToolCallChunk::new(
                        "call-ping",
                        "ping",
                        "{}",
                        0,
                    )])),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            } else {
                *self.second_history.lock() = Some(messages.to_vec());
                stream::iter(vec![Ok(StreamChunk::delta("done")), Ok(StreamChunk::final_chunk())]).boxed()
            }
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let second_history = Arc::new(Mutex::new(None));
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(ReasoningToolProvider {
        counter: Arc::new(AtomicUsize::new(0)),
        second_history: Arc::clone(&second_history),
    });
    deps.tools_registry = Some(Arc::new(vec![Box::new(PingTool) as Box<dyn crate::tools::Tool>]));
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);

            let binding = admit_start_turn_fixture(
                &fixture_deps,
                "draft-reasoning-tool-history",
                "stream the fixture reply",
                true,
            )
            .await
            .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-reasoning-tool-history".into(),
                    history: Vec::new(),
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

            for _ in 0..32 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver action within 2s")
                    .expect("must arrive");
                match action {
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("done"));
                        break;
                    }
                    Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                    _ => {}
                }
            }

            let history = second_history.lock().clone().expect("second request history captured");
            let assistant = history
                .iter()
                .find(|message| message.role == "assistant")
                .expect("assistant tool-call history expected");
            let value: serde_json::Value = serde_json::from_str(&assistant.content).expect("assistant payload JSON");
            assert_eq!(
                value.get("reasoning_content").and_then(serde_json::Value::as_str),
                Some("Need to call ping.")
            );
            let call_id = value
                .get("tool_calls")
                .and_then(serde_json::Value::as_array)
                .and_then(|calls| calls.first())
                .and_then(|call| call.get("id"))
                .and_then(serde_json::Value::as_str);
            assert_eq!(call_id, Some("call-ping"));
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}
