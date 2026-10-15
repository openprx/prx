#![cfg(test)]

use super::*;

// ── Step 5a-4 mandatory tests (required by the Codex Phase 3 audit) ─────────────

/// P0-2: EffectDeps.model + temperature are really passed through to drive_start_turn_stream.
///
/// Uses a capture provider to assert that the model/temperature received by stream_chat_with_history
/// equal the injected deps values. Fixes the Codex P1 about hard-coded String::new()/0.0 in 5a-2.
#[tokio::test]
async fn real_mode_start_turn_passes_model_and_temperature_from_deps() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use parking_lot::Mutex as PMutex;

    #[derive(Default)]
    struct ParamCaptureProvider {
        captured_model: Arc<PMutex<String>>,
        captured_temp: Arc<PMutex<f64>>,
    }

    #[async_trait]
    impl Provider for ParamCaptureProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(
            &self,
            _sys: Option<&str>,
            _msg: &str,
            _model: &str,
            _temp: f64,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
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
            _messages: &[PMsg],
            model: &str,
            temperature: f64,
            _options: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            *self.captured_model.lock() = model.to_string();
            *self.captured_temp.lock() = temperature;
            let chunks: Vec<StreamResult<StreamChunk>> =
                vec![Ok(StreamChunk::delta("ok")), Ok(StreamChunk::final_chunk())];
            stream::iter(chunks).boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let captured_model = Arc::new(PMutex::new(String::new()));
    let captured_temp = Arc::new(PMutex::new(0.0_f64));
    let provider = Arc::new(ParamCaptureProvider {
        captured_model: Arc::clone(&captured_model),
        captured_temp: Arc::clone(&captured_temp),
    });

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = provider.clone();
    // inject a non-default model / temperature so capture can tell them apart.
    deps.model.set(Arc::from("gpt-test-99"));
    deps.temperature = 0.42;

    let (binding, fixture_cleanup) =
        admit_owned_start_turn_fixture(&deps, "draft-params", "stream the fixture reply", false)
            .await
            .expect("Q42 genuine admission prerequisite");
    let executor = EffectExecutor::new_with_deps(deps);
    executor
        .execute(Effect::StartTurn {
            provider_turn_task_id: None,
            draft_id: "draft-params".to_string(),
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

    // wait for the first chunk to make sure stream_chat_with_history was already called.
    let _ = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
        .await
        .expect("first chunk within 1.5s");

    let model_seen = captured_model.lock().clone();
    let temp_seen = *captured_temp.lock();
    assert_eq!(
        model_seen, "gpt-test-99",
        "model must be passed through from EffectDeps"
    );
    assert!(
        (temp_seen - 0.42).abs() < f64::EPSILON,
        "temperature must be passed through from EffectDeps (got {temp_seen})"
    );
    finish_owned_start_turn_fixture(fixture_cleanup).await;
}

/// P1-1: TurnCompletionSignal failure-path API contract.
///
/// Verifies `extract_turn_outcome(StreamFailed) → TurnOutcomeKind::Failed`,
/// and that after `record_and_notify` the `consume_outcome` reads the same Failed (with err / retryable).
/// The full driver chain (provider Err → drive_start_turn_stream sends StreamFailed) is already
/// covered by `real_mode_start_turn_stream_error_emits_stream_failed`;
/// the reducer chain (StreamFailed → NotifyHook(Error)) is already covered by state.rs unit tests.
/// This test pins down that TurnCompletionSignal does not lose the err semantics at the seam.
#[tokio::test]
async fn turn_signal_records_failed_outcome_from_stream_failed_action() {
    let signal = TurnCompletionSignal::new();
    let action = Action::StreamFailed {
        draft_id: "d1".to_string(),
        err: "simulated provider failure".to_string(),
        retryable: true,
    };
    let outcome = extract_turn_outcome(&action);
    assert!(matches!(outcome, Some(TurnOutcomeKind::Failed { .. })));
    if let Some(out) = outcome {
        signal.record_and_notify(out);
    }
    let consumed = signal.consume_outcome();
    match consumed {
        Some(TurnOutcomeKind::Failed { err, retryable }) => {
            assert!(err.contains("simulated"), "err must contain original message");
            assert!(retryable, "retryable bit must be preserved");
        }
        other => panic!("expected Failed outcome, got {other:?}"),
    }
    // the second consume must be None (consuming API).
    assert!(signal.consume_outcome().is_none(), "consume_outcome must drain slot");
}

#[tokio::test]
async fn turn_signal_records_keyed_outcome_and_usage_by_draft_id() {
    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("keyed turn", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    let signal = TurnCompletionSignal::new();
    signal.register_turn(task_id, "draft-keyed");
    let notified = signal.notified_for(task_id).expect("registered keyed waiter");

    let usage = TokenUsage {
        prompt_tokens: Some(10),
        completion_tokens: Some(5),
        total_tokens: Some(15),
        source: crate::llm::route_decision::TokenUsageSource::Reported,
        ..TokenUsage::default()
    };
    assert!(signal.record_usage_for_draft("draft-keyed", usage));
    assert!(signal.record_and_notify_for_draft(
        "draft-keyed",
        TurnOutcomeKind::Completed {
            final_text: "done".to_string(),
            reasoning: "because".to_string(),
        },
    ));
    tokio::time::timeout(std::time::Duration::from_secs(1), notified)
        .await
        .expect("keyed turn should notify");

    let keyed_usage = signal.consume_turn_usage(task_id);
    assert_eq!(keyed_usage.total_tokens, Some(15));
    match signal.consume_turn_outcome(task_id) {
        Some(TurnOutcomeKind::Completed { final_text, reasoning }) => {
            assert_eq!(final_text, "done");
            assert_eq!(reasoning, "because");
        }
        other => panic!("expected keyed completed outcome, got {other:?}"),
    }
    assert!(
        signal.consume_turn_outcome(task_id).is_none(),
        "keyed outcome is consume-once"
    );
    signal.unregister_turn(task_id);
    assert!(
        signal.notified_for(task_id).is_none(),
        "unregister removes keyed waiter"
    );
}

/// **5a-6 negative case**: the driver receives a tool_calls chunk but `tools_registry == None`,
/// so it must send `StreamFailed(retryable=false)`.
///
/// route_turn now lets the driver take a tool turn, but when `tools_registry` is None the driver
/// cannot execute the tool — it fails fast immediately so chat::run falls through.
#[tokio::test]
async fn driver_without_registry_rejects_tool_call_chunk() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
        ToolCallChunk,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct ToolCallProvider;
    #[async_trait]
    impl Provider for ToolCallProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(
            &self,
            _sys: Option<&str>,
            _msg: &str,
            _model: &str,
            _temp: f64,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
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
            _messages: &[PMsg],
            _model: &str,
            _temp: f64,
            _options: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            let calls = vec![ToolCallChunk::new("c1", "shell", r#"{"cmd":"ls"}"#, 0)];
            stream::iter(vec![
                Ok(StreamChunk::tool_call_chunk(calls)),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(ToolCallProvider);
    // explicit: no registry provided → the driver must fail.
    deps.tools_registry = None;
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);

            let cancel = CancellationToken::new();
            let binding =
                admit_start_turn_fixture(&fixture_deps, "draft-no-registry", "stream the fixture reply", true)
                    .await
                    .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-no-registry".to_string(),
                    history: Vec::new(),
                    compaction_guard_history: None,
                    compaction_config: None,
                    cancel,
                    chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                    turn_spawn_ctx: None,
                    turn_message_send_ctx: None,
                    routing_input: None,
                    turn_binding: Some(binding),
                })
                .await;

            // skip a potential ToolStarted (not sent on the no-registry path, since the registry check precedes it).
            let mut got_failed = false;
            for _ in 0..6 {
                let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                    .await
                    .expect("driver must respond within 1.5s")
                    .expect("action must be received");
                match action {
                    Action::StreamFailed {
                        draft_id,
                        err,
                        retryable,
                    } => {
                        assert_eq!(draft_id, "draft-no-registry");
                        assert!(!retryable, "no-registry rejection is permanent");
                        assert!(
                            err.contains("tools_registry") || err.contains("tool_calls"),
                            "err must hint at missing registry / tool_calls (got: {err})"
                        );
                        got_failed = true;
                        break;
                    }
                    Action::StreamChunkReceived { .. } | Action::ToolStarted { .. } | Action::ToolFinished { .. } => {
                        // permitted pre-failure noise; keep draining.
                    }
                    other => panic!("unexpected action before StreamFailed: {other:?}"),
                }
            }
            assert!(got_failed, "driver must emit StreamFailed within 6 actions");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

/// **5a-6 happy path**: the driver receives a tool_call → executes it through tools_registry → feeds the
/// tool result back into history → the next LLM pass returns the final text → StreamCompleted.
///
/// Simulates two passes: in pass 1 the provider sends ToolCall(echo-tool, {"text": "hi"}) and the driver runs
/// echo-tool returning "hi"; in pass 2 the provider sends final_text="done" and the driver finishes the turn.
#[tokio::test]
async fn driver_executes_tool_call_chunk_and_continues_to_completion() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
        ToolCallChunk,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use parking_lot::Mutex as PMutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    // ── Echo tool: returns args["text"] verbatim, so the driver feeds it back and we can verify history. ──
    struct EchoTool;
    #[async_trait]
    impl crate::tools::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo-tool"
        }
        fn description(&self) -> &str {
            "echoes back its text argument"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Ok(crate::tools::ToolResult {
                success: true,
                output: text,
                error: None,
            })
        }
    }

    // ── Provider: the first stream sends a tool_call, the second sends the final text. ──
    struct ToolThenTextProvider {
        counter: Arc<AtomicUsize>,
        captured_tool_counts: Arc<PMutex<Vec<usize>>>,
    }
    #[async_trait]
    impl Provider for ToolThenTextProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: true,
                vision: false,
            }
        }
        async fn chat_with_system(
            &self,
            _sys: Option<&str>,
            _msg: &str,
            _model: &str,
            _temp: f64,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
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
            _messages: &[PMsg],
            _model: &str,
            _temp: f64,
            options: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            self.captured_tool_counts
                .lock()
                .push(options.tools.as_ref().map_or(0, Vec::len));
            let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
            if n == 0 {
                let calls = vec![ToolCallChunk::new("tc-1", "echo-tool", r#"{"text":"echoed"}"#, 0)];
                stream::iter(vec![
                    Ok(StreamChunk::tool_call_chunk(calls)),
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
    let captured_tool_counts = Arc::new(PMutex::new(Vec::new()));
    deps.provider = Arc::new(ToolThenTextProvider {
        counter: Arc::new(AtomicUsize::new(0)),
        captured_tool_counts: Arc::clone(&captured_tool_counts),
    });
    deps.tools_registry = Some(Arc::new(vec![Box::new(EchoTool) as Box<dyn crate::tools::Tool>]));
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);

            let cancel = CancellationToken::new();
            let binding = admit_start_turn_fixture(&fixture_deps, "draft-tool-happy", "stream reply", true)
                .await
                .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-tool-happy".to_string(),
                    history: Vec::new(),
                    compaction_guard_history: None,
                    compaction_config: None,
                    cancel,
                    chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                    turn_spawn_ctx: None,
                    turn_message_send_ctx: None,
                    routing_input: None,
                    turn_binding: Some(binding),
                })
                .await;

            let mut saw_tool_started = false;
            let mut saw_tool_finished_success = false;
            let mut saw_completion = false;
            let mut final_text_seen = String::new();
            let mut observed = Vec::new();
            for _ in 0..16 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver should respond within 2s per action")
                    .expect("action must arrive");
                match action {
                    Action::ToolStarted { name, .. } => {
                        observed.push(format!("started:{name}"));
                        assert_eq!(name, "echo-tool");
                        saw_tool_started = true;
                    }
                    Action::ToolFinished {
                        name, success, result, ..
                    } => {
                        observed.push(format!("finished:{name}:{success}:{result:?}"));
                        assert_eq!(name, "echo-tool");
                        if success {
                            saw_tool_finished_success = true;
                            assert!(
                                result.as_deref().is_some_and(|s| s.contains("echoed")),
                                "tool result must echo back text arg (got {result:?})"
                            );
                        }
                    }
                    Action::StreamChunkReceived { delta, .. } => {
                        final_text_seen.push_str(&delta);
                    }
                    Action::StreamCompleted { final_text, .. } => {
                        observed.push(format!("completed:{final_text}"));
                        saw_completion = true;
                        assert!(
                            final_text.contains("done"),
                            "final text must contain 'done' (got {final_text:?})"
                        );
                        break;
                    }
                    Action::StreamFailed { err, .. } => {
                        panic!("driver should not fail in happy path: {err}");
                    }
                    _ => {}
                }
            }
            assert!(
                saw_tool_started,
                "must see ToolStarted; actions={observed:?}; offers={:?}",
                *captured_tool_counts.lock()
            );
            assert!(saw_tool_finished_success, "must see ToolFinished(success=true)");
            assert!(saw_completion, "must see StreamCompleted");
            assert!(
                final_text_seen.contains("done"),
                "streaming delta must include 'done' (got {final_text_seen:?})"
            );
            assert_eq!(
                *captured_tool_counts.lock(),
                vec![1, 1],
                "driver must pass registered tool specs to each streaming request"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

/// **D8-4 redux-path real fix regression**: a tool executed *inside* the
/// redux driver (`Effect::StartTurn` → spawn → `drive_start_turn_stream` →
/// `tool.execute`) must observe the turn-root `SPAWN_EXECUTION_CONTEXT`
/// seeded via `Effect::StartTurn { turn_spawn_ctx, .. }`.
///
/// This drives the *real* dispatcher execution path (not a hand-rolled
/// `tool.execute()` with a manual `.scope()` wrapper — the mistake the
/// previous "fix" made, which passed while the production redux path still
/// dropped the context). If the executor stops wrapping the driver future in
/// `SPAWN_EXECUTION_CONTEXT.scope(..)`, `try_with` returns `Err` and the
/// captured `parent_run_id` is `None`, failing this test — exactly the
/// observable cause of model-spawned sub-agents being mislabeled `user`.
///
/// The `None` (no-scope) leg — which keeps the `/bg` slash-command path on
/// user origin — is already covered by every other driver test in this file
/// (they all pass `turn_spawn_ctx: None`); this test asserts the `Some` leg.
#[tokio::test]
async fn driver_seeds_spawn_execution_context_for_tool_calls() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
        ToolCallChunk,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use parking_lot::Mutex as PMutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    // Tool that records what the spawn execution context looked like at the
    // moment it executed — i.e. whether the driver scoped the task-local.
    // `executed` distinguishes "tool never ran" from "ran but saw no scope".
    struct CtxProbeTool {
        executed: Arc<std::sync::atomic::AtomicBool>,
        seen_run_id: Arc<PMutex<Option<String>>>,
    }
    #[async_trait]
    impl crate::tools::Tool for CtxProbeTool {
        fn name(&self) -> &str {
            "ctx-probe"
        }
        fn description(&self) -> &str {
            "records the spawn execution context's run_id"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            // `try_with` is Ok only when the driver scoped the task-local;
            // None here means an unscoped task → sub-agents fall back to user.
            let observed = crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT
                .try_with(|ctx| ctx.run_id.clone())
                .ok();
            *self.seen_run_id.lock() = observed;
            self.executed.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::tools::ToolResult {
                success: true,
                output: "probed".to_string(),
                error: None,
            })
        }
    }

    struct ToolThenTextProvider {
        counter: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for ToolThenTextProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(
            &self,
            _sys: Option<&str>,
            _msg: &str,
            _model: &str,
            _temp: f64,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
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
            _messages: &[PMsg],
            _model: &str,
            _temp: f64,
            _options: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
            if n == 0 {
                let calls = vec![ToolCallChunk::new("tc-1", "ctx-probe", "{}", 0)];
                stream::iter(vec![
                    Ok(StreamChunk::tool_call_chunk(calls)),
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
    deps.provider = Arc::new(ToolThenTextProvider {
        counter: Arc::new(AtomicUsize::new(0)),
    });
    let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen_run_id = Arc::new(PMutex::new(None));
    deps.tools_registry = Some(Arc::new(vec![Box::new(CtxProbeTool {
        executed: Arc::clone(&executed),
        seen_run_id: Arc::clone(&seen_run_id),
    }) as Box<dyn crate::tools::Tool>]));
    let (binding, fixture_cleanup) =
        admit_owned_start_turn_fixture(&deps, "draft-ctx-probe", "stream the fixture reply", true)
            .await
            .expect("Q42 genuine admission prerequisite");
    let executor = EffectExecutor::new_with_deps(deps);

    let turn_run_id = "turn-run-id-xyz".to_string();
    let seed = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
        turn_run_id.clone(),
        "chat:test-scope".to_string(),
    );

    let cancel = CancellationToken::new();
    executor
        .execute(Effect::StartTurn {
            provider_turn_task_id: None,
            draft_id: "draft-ctx-probe".to_string(),
            history: Vec::new(),
            compaction_guard_history: None,
            compaction_config: None,
            cancel,
            chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
            turn_spawn_ctx: Some(seed),
            turn_message_send_ctx: None,
            routing_input: None,
            turn_binding: Some(binding),
        })
        .await;

    // Drain until completion so the spawned driver task has finished the tool.
    for _ in 0..16 {
        let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
            .await
            .expect("driver should respond within 2s per action")
            .expect("action must arrive");
        if matches!(action, Action::StreamCompleted { .. } | Action::StreamFailed { .. }) {
            break;
        }
    }

    assert!(
        executed.load(std::sync::atomic::Ordering::SeqCst),
        "ctx-probe tool must have executed inside the driver"
    );
    let observed = seen_run_id.lock().clone();
    assert_eq!(
        observed,
        Some(turn_run_id),
        "redux driver must scope SPAWN_EXECUTION_CONTEXT so the in-turn tool sees the turn run_id \
             (None here means model-spawned sub-agents would be mislabeled as user)"
    );
    finish_owned_start_turn_fixture(fixture_cleanup).await;
}

/// BUG-03 round-2: when a tool fails *unrecoverably* (permission denied) and
/// the model re-issues the identical blocked call, the driver must STOP early
/// instead of retrying indefinitely (the chat-demo defect where a blocked
/// shell command burned repeated LLM round-trips before erroring out).
///
/// Provider always emits the same `noop` tool call; the tool always returns
/// "permission denied". The fix must emit
/// `StreamFailed` with the *unrecoverable* message after the signature recurs
/// (≈ iteration 2) and crucially must NOT count the noop tool more than a
/// couple of times — proving it did not retry indefinitely.
#[tokio::test]
async fn driver_stops_early_on_repeated_unrecoverable_tool_failure() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
        ToolCallChunk,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct DeniedTool {
        exec_count: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl crate::tools::Tool for DeniedTool {
        fn name(&self) -> &str {
            "shell"
        }
        fn description(&self) -> &str {
            "shell"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            self.exec_count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(crate::tools::ToolResult {
                success: false,
                output: String::new(),
                error: Some("permission denied: command not allowed by policy".to_string()),
            })
        }
    }

    // A realistic model re-issues a *new* tool_call id on each retry (the
    // driver's id-based idempotency skip only suppresses literal duplicate
    // ids within one assistant turn). Use a counter so each blocked attempt
    // is a fresh execution — that is what makes the retry actually re-run and
    // lets the unrecoverable signature recur.
    struct AlwaysDeniedCallProvider {
        seq: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for AlwaysDeniedCallProvider {
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
            let n = self.seq.fetch_add(1, AtomicOrdering::SeqCst);
            let calls = vec![ToolCallChunk::new(
                format!("blocked-{n}"),
                "shell",
                r#"{"cmd":"rm -rf /"}"#,
                0,
            )];
            stream::iter(vec![
                Ok(StreamChunk::tool_call_chunk(calls)),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let exec_count = Arc::new(AtomicUsize::new(0));
    deps.provider = Arc::new(AlwaysDeniedCallProvider {
        seq: Arc::new(AtomicUsize::new(0)),
    });
    deps.tools_registry = Some(Arc::new(vec![Box::new(DeniedTool {
        exec_count: Arc::clone(&exec_count),
    }) as Box<dyn crate::tools::Tool>]));
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);

            let cancel = CancellationToken::new();
            let binding =
                admit_start_turn_fixture(&fixture_deps, "draft-unrecoverable", "stream the fixture reply", true)
                    .await
                    .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-unrecoverable".to_string(),
                    history: Vec::new(),
                    compaction_guard_history: None,
                    compaction_config: None,
                    cancel,
                    chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                    turn_spawn_ctx: None,
                    turn_message_send_ctx: None,
                    routing_input: None,
                    turn_binding: Some(binding),
                })
                .await;

            let mut got_failed = false;
            let mut failed_err = String::new();
            for _ in 0..32 {
                let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
                    .await
                    .expect("driver should respond per action within 2s")
                    .expect("action must arrive");
                if let Action::StreamFailed { err, retryable, .. } = &action {
                    assert!(!retryable, "unrecoverable stop is permanent");
                    failed_err = err.clone();
                    got_failed = true;
                    break;
                }
            }
            assert!(
                got_failed,
                "driver must emit StreamFailed on repeated unrecoverable failure"
            );
            assert!(
                failed_err.contains("unrecoverable") && failed_err.contains("shell"),
                "must stop with the unrecoverable message naming the blocked tool (got: {failed_err})"
            );
            // The blocked tool must have run only a small number of times (the first
            // failure plus the one retry that trips the early-stop).
            let runs = exec_count.load(AtomicOrdering::SeqCst);
            assert!(
                (1..=3).contains(&runs),
                "blocked tool should run ~twice before early-stop, not retry indefinitely (ran {runs} times)"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

/// P1-2: cancel mid-turn on the driver path — the select! inside drive_start_turn_stream takes the cancel branch.
///
/// Verifies that after cancel_token is cancelled, drive_start_turn_stream sends StreamCancelled
/// instead of continuing to consume the stream or sending StreamCompleted.
#[tokio::test]
async fn driver_mid_turn_cancel_emits_stream_cancelled() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    /// Provider returns a stream that never ends — cancel takes over.
    struct PendingStreamProvider;
    #[async_trait]
    impl Provider for PendingStreamProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat_with_system(
            &self,
            _sys: Option<&str>,
            _msg: &str,
            _model: &str,
            _temp: f64,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
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
            _messages: &[PMsg],
            _model: &str,
            _temp: f64,
            _options: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            // one delta + pending (stream::pending makes next() pend forever)
            stream::iter(vec![Ok(StreamChunk::delta("partial"))])
                .chain(stream::pending())
                .boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(PendingStreamProvider);
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);

            let cancel = CancellationToken::new();
            let binding =
                admit_start_turn_fixture(&fixture_deps, "draft-cancel-mid", "stream the fixture reply", false)
                    .await
                    .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-cancel-mid".to_string(),
                    history: Vec::new(),
                    compaction_guard_history: None,
                    compaction_config: None,
                    cancel: cancel.clone(),
                    chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                    turn_spawn_ctx: None,
                    turn_message_send_ctx: None,
                    routing_input: None,
                    turn_binding: Some(binding),
                })
                .await;

            // receive one delta (partial) first, proving the stream is already active.
            let a1 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("first delta within 1.5s")
                .expect("first delta received");
            assert!(
                matches!(a1, Action::StreamChunkReceived { ref delta, .. } if delta == "partial"),
                "expected first partial delta, got {a1:?}"
            );

            // cancel mid-turn
            cancel.cancel();

            // StreamCancelled must arrive immediately.
            let a2 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("StreamCancelled within 1.5s after cancel")
                .expect("StreamCancelled received");
            match a2 {
                Action::StreamCancelled { draft_id } => assert_eq!(draft_id, "draft-cancel-mid"),
                other => panic!("expected StreamCancelled after mid-turn cancel, got {other:?}"),
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

/// P0-1 simplified: try_dispatch returns ChannelClosed when the channel is closed
/// (the chat::run driver branch uses this to abort the turn + cleanup + continue).
#[tokio::test]
async fn chat_dispatcher_try_dispatch_returns_channel_closed_after_rx_drop() {
    let (dispatcher, rx) = ChatDispatcher::new();
    drop(rx);
    let result = dispatcher.try_dispatch(Action::StartLLMTurn {
        provider_turn_task_id: None,
        provider_turn_sequence: None,
        draft_id: "d1".to_string(),
        history: Vec::new(),
        compaction_guard_history: None,
        compaction_config: None,
        cancel: CancellationToken::new(),
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });
    assert!(
        matches!(result, DispatchResult::ChannelClosed),
        "after action_rx drop, try_dispatch must return ChannelClosed (got {result:?})"
    );
}
