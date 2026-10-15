#![cfg(test)]

use super::*;

#[tokio::test]
async fn real_mode_start_turn_spawns_subtask_and_does_not_block() {
    // StartTurn must spawn a subtask that feeds Actions back (Codex P0-1), without blocking the main loop.
    // From 5a-2 on: the subtask really calls provider.stream_chat_with_history and feeds the streaming
    // events back through action_tx — here we use the MockEnvProvider trait default implementation,
    // which emits one final error chunk, so we should get StreamChunkReceived → StreamCompleted.
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);

            let binding = admit_start_turn_fixture(&fixture_deps, "draft-real", "stream the fixture reply", false)
                .await
                .expect("Q42 genuine admission prerequisite");
            let start = std::time::Instant::now();
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-real".to_string(),
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
            // execute() returns immediately; the spawned subtask really calls the provider and feeds Actions back.
            assert!(
                start.elapsed() < Duration::from_millis(100),
                "StartTurn should not block (Codex P0-1)"
            );

            // first Action: the default stream implementation emits one error chunk (delta=error message,
            // is_final=true), which is converted into StreamChunkReceived (because delta is non-empty).
            let action = tokio::time::timeout(Duration::from_secs(1), action_rx.recv())
                .await
                .expect("action within 1s")
                .expect("action received");
            match action {
                Action::StreamChunkReceived { draft_id, version, .. } => {
                    assert_eq!(draft_id, "draft-real", "draft_id must propagate");
                    assert!(version >= 1, "version must start at 1+");
                }
                other => panic!("expected StreamChunkReceived, got {other:?}"),
            }

            // T3-3-fixB B5: the second one is RecordAssistantTurn (sent before StreamCompleted).
            let action = tokio::time::timeout(Duration::from_secs(1), action_rx.recv())
                .await
                .expect("RecordAssistantTurn within 1s")
                .expect("RecordAssistantTurn received");
            match action {
                Action::RecordAssistantTurn { .. } => {}
                other => panic!("expected RecordAssistantTurn (fixB B5 precondition), got {other:?}"),
            }

            // third: is_final=true breaks out of the loop and sends StreamCompleted.
            let action = tokio::time::timeout(Duration::from_secs(1), action_rx.recv())
                .await
                .expect("completion within 1s")
                .expect("completion received");
            match action {
                Action::StreamCompleted { draft_id, .. } => {
                    assert_eq!(draft_id, "draft-real", "completion draft_id must propagate");
                }
                other => panic!("expected StreamCompleted, got {other:?}"),
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

/// Step 5a-2 — StartTurn sends StreamCancelled immediately after a cancel pre-trigger.
#[tokio::test]
async fn real_mode_start_turn_pre_cancel_emits_stream_cancelled() {
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let executor = EffectExecutor::new_with_deps(deps);

    let cancel = CancellationToken::new();
    cancel.cancel(); // cancelled before start

    executor
        .execute(Effect::StartTurn {
            provider_turn_task_id: None,
            draft_id: "draft-cancelled".to_string(),
            history: Vec::new(),
            compaction_guard_history: None,
            compaction_config: None,
            cancel,
            chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
            turn_spawn_ctx: None,
            turn_message_send_ctx: None,
            routing_input: None,
            turn_binding: None,
        })
        .await;

    let action = tokio::time::timeout(Duration::from_millis(500), action_rx.recv())
        .await
        .expect("action within 500ms")
        .expect("action received");
    match action {
        Action::StreamCancelled { draft_id } => {
            assert_eq!(draft_id, "draft-cancelled");
        }
        other => panic!("expected StreamCancelled, got {other:?}"),
    }
}

/// Step 5a-2 — fake streaming provider verifies the chunk → completion sequence + strict version increase.
#[tokio::test]
async fn real_mode_start_turn_streams_chunks_then_completes() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct FakeStreamProvider;

    #[async_trait]
    impl Provider for FakeStreamProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: false,
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
            _temperature: f64,
            _options: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            let chunks: Vec<StreamResult<StreamChunk>> = vec![
                Ok(StreamChunk::delta("hello ")),
                Ok(StreamChunk::reasoning_delta("thinking…")),
                Ok(StreamChunk::delta("world")),
                Ok(StreamChunk::final_chunk()),
            ];
            stream::iter(chunks).boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(FakeStreamProvider);
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);

            let binding = admit_start_turn_fixture(&fixture_deps, "draft-stream", "stream the fixture reply", false)
                .await
                .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-stream".to_string(),
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

            // receive chunk 1 (delta="hello ")
            let a1 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("first chunk within 1.5s")
                .expect("first chunk received");
            match a1 {
                Action::StreamChunkReceived {
                    draft_id,
                    delta,
                    version,
                } => {
                    assert_eq!(draft_id, "draft-stream");
                    assert_eq!(delta, "hello ");
                    assert_eq!(version, 1, "first delta version must be 1");
                }
                other => panic!("expected StreamChunkReceived#1, got {other:?}"),
            }

            // F1: reasoning chunks are now dispatched as Actions too, so the TUI shows progress while thinking;
            // they share the same version counter as text deltas.
            let a_reasoning = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("reasoning progress within 1.5s")
                .expect("reasoning progress received");
            match a_reasoning {
                Action::StreamReasoningReceived {
                    draft_id,
                    delta,
                    version,
                } => {
                    assert_eq!(draft_id, "draft-stream");
                    assert_eq!(delta, "thinking…");
                    assert_eq!(version, 2, "reasoning shares the text delta version counter");
                }
                other => panic!("expected StreamReasoningReceived, got {other:?}"),
            }

            // the next one is still chunk 2 (delta="world"), and the version keeps increasing strictly
            let a2 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("second chunk within 1.5s")
                .expect("second chunk received");
            match a2 {
                Action::StreamChunkReceived { delta, version, .. } => {
                    assert_eq!(delta, "world");
                    assert_eq!(version, 3, "second delta version must strictly increase");
                }
                other => panic!("expected StreamChunkReceived#2, got {other:?}"),
            }

            // T3-3-fixB B5: RecordAssistantTurn is sent before StreamCompleted, and
            // final_text matches the RecordAssistantTurn content.
            let a_record = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("RecordAssistantTurn within 1.5s")
                .expect("RecordAssistantTurn received");
            match a_record {
                Action::RecordAssistantTurn { content: text, .. } => {
                    assert_eq!(text, "hello world", "RecordAssistantTurn content must match final_text");
                }
                other => panic!("expected RecordAssistantTurn (fixB B5 precondition), got {other:?}"),
            }

            // finally StreamCompleted, with accumulated final_text and reasoning containing the thinking text.
            let a3 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("completion within 1.5s")
                .expect("completion received");
            match a3 {
                Action::StreamCompleted {
                    draft_id,
                    final_text,
                    reasoning,
                } => {
                    assert_eq!(draft_id, "draft-stream");
                    assert_eq!(final_text, "hello world");
                    assert_eq!(reasoning, "thinking…");
                }
                other => panic!("expected StreamCompleted, got {other:?}"),
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

#[tokio::test]
async fn task_scoped_start_turn_emits_ready_for_ordered_commit() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct FakeTaskScopedStreamProvider;

    #[async_trait]
    impl Provider for FakeTaskScopedStreamProvider {
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
            _temperature: f64,
            _options: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            let chunks: Vec<StreamResult<StreamChunk>> = vec![
                Ok(StreamChunk::delta("ordered ")),
                Ok(StreamChunk::reasoning_delta("gate")),
                Ok(StreamChunk::delta("commit")),
                Ok(StreamChunk::final_chunk()),
            ];
            stream::iter(chunks).boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
    let task_id = scheduler.enqueue("task scoped", crate::chat::turn_scheduler::TurnPriority::Normal, 0);
    scheduler.start_task(task_id).expect("task starts");
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(FakeTaskScopedStreamProvider);
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);

            let binding =
                admit_start_turn_fixture(&fixture_deps, "draft-task-scoped", "stream the fixture reply", false)
                    .await
                    .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: Some(task_id),
                    draft_id: "draft-task-scoped".to_string(),
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

            // F1: a reasoning delta sandwiched between text deltas is dispatched separately as thinking progress.
            let mut text_deltas = Vec::new();
            let mut reasoning_deltas = Vec::new();
            while text_deltas.len() < 2 {
                let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                    .await
                    .expect("stream action within 1.5s")
                    .expect("stream action received");
                match action {
                    Action::StreamChunkReceived { delta, .. } => text_deltas.push(delta),
                    Action::StreamReasoningReceived { delta, .. } => reasoning_deltas.push(delta),
                    other => panic!("expected stream delta action, got {other:?}"),
                }
            }
            assert_eq!(text_deltas, vec!["ordered ".to_string(), "commit".to_string()]);
            assert_eq!(
                reasoning_deltas,
                vec!["gate".to_string()],
                "thinking progress must be dispatched live, not only on completion"
            );

            let terminal = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("ready signal within 1.5s")
                .expect("ready signal received");
            match terminal {
                Action::ProviderTurnReadyForCommit {
                    draft_id,
                    final_text,
                    reasoning,
                } => {
                    assert_eq!(draft_id, "draft-task-scoped");
                    assert_eq!(final_text, "ordered commit");
                    assert_eq!(reasoning, "gate");
                }
                other => {
                    panic!("task-scoped completion must wait for ordered commit gate, got {other:?}")
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

#[tokio::test]
async fn real_mode_empty_stream_retries_and_records_recovered_answer() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct EmptyThenAnswerProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for EmptyThenAnswerProvider {
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
                reasoning_content: Some("thinking only".to_string()),
            })
        }
        fn supports_streaming(&self) -> bool {
            true
        }
        fn stream_chat_with_history(
            &self,
            _messages: &[PMsg],
            _model: &str,
            _temperature: f64,
            _options: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                stream::iter(vec![
                    Ok(StreamChunk::reasoning_delta("thinking only")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            } else {
                stream::iter(vec![
                    Ok(StreamChunk::delta("recovered")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(EmptyThenAnswerProvider::default());
    let (binding, fixture_cleanup) =
        admit_owned_start_turn_fixture(&deps, "draft-empty", "stream the fixture reply", false)
            .await
            .expect("Q42 genuine admission prerequisite");
    let executor = EffectExecutor::new_with_deps(deps);

    executor
        .execute(Effect::StartTurn {
            provider_turn_task_id: None,
            draft_id: "draft-empty".to_string(),
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

    let mut saw_assistant_record = false;
    let mut saw_completion = false;
    for _ in 0..6 {
        let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("driver action within 1.5s")
            .expect("driver action received");
        match action {
            Action::SystemMessageAdded { text } => {
                panic!("a recovered empty response must not surface a failure notice: {text}");
            }
            Action::RecordAssistantTurn { content: text, .. } => {
                assert_eq!(text, "recovered");
                saw_assistant_record = true;
            }
            Action::StreamCompleted {
                draft_id,
                final_text,
                reasoning,
            } => {
                assert_eq!(draft_id, "draft-empty");
                assert_eq!(final_text, "recovered");
                let _ = reasoning;
                saw_completion = true;
                break;
            }
            _ => {}
        }
    }

    assert!(saw_assistant_record, "the recovered answer must be recorded");
    assert!(saw_completion, "the recovered turn must complete and clear the draft");
    finish_owned_start_turn_fixture(fixture_cleanup).await;
}

/// Step 5a-2 — send StreamFailed when the provider stream yields an Err (including the retryable verdict).
#[tokio::test]
async fn real_mode_start_turn_stream_error_emits_stream_failed() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError, StreamOptions,
        StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct FailingStreamProvider;

    #[async_trait]
    impl Provider for FailingStreamProvider {
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
            _temperature: f64,
            _options: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            let chunks: Vec<StreamResult<StreamChunk>> =
                vec![Err(StreamError::Provider("simulated failure".to_string()))];
            stream::iter(chunks).boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(FailingStreamProvider);
    let (binding, fixture_cleanup) =
        admit_owned_start_turn_fixture(&deps, "draft-fail", "stream the fixture reply", false)
            .await
            .expect("Q42 genuine admission prerequisite");
    let executor = EffectExecutor::new_with_deps(deps);

    executor
        .execute(Effect::StartTurn {
            provider_turn_task_id: None,
            draft_id: "draft-fail".to_string(),
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

    let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
        .await
        .expect("fail within 1.5s")
        .expect("fail action received");
    match action {
        Action::StreamFailed {
            draft_id,
            err,
            retryable,
        } => {
            assert_eq!(draft_id, "draft-fail");
            assert!(err.contains("simulated failure"));
            assert!(!retryable, "Provider error is non-retryable");
        }
        other => panic!("expected StreamFailed, got {other:?}"),
    }
    finish_owned_start_turn_fixture(fixture_cleanup).await;
}

/// Step 5a-2 — a cancel in mid-stream triggers StreamCancelled.
#[tokio::test]
async fn real_mode_start_turn_mid_stream_cancel_emits_cancelled() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct SlowStreamProvider;

    #[async_trait]
    impl Provider for SlowStreamProvider {
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
            _temperature: f64,
            _options: StreamOptions,
        ) -> BoxStream<'static, StreamResult<StreamChunk>> {
            // sleep 200ms between chunks to give cancel a chance
            let s = stream::unfold(0u32, |i| async move {
                if i >= 5 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                Some((Ok(StreamChunk::delta(format!("chunk{i} "))), i + 1))
            });
            s.boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(SlowStreamProvider);
    let (binding, fixture_cleanup) =
        admit_owned_start_turn_fixture(&deps, "draft-mid-cancel", "stream the fixture reply", false)
            .await
            .expect("Q42 genuine admission prerequisite");
    let executor = EffectExecutor::new_with_deps(deps);

    let cancel = CancellationToken::new();
    executor
        .execute(Effect::StartTurn {
            provider_turn_task_id: None,
            draft_id: "draft-mid-cancel".to_string(),
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

    // cancel after 250ms: at least 1 chunk should already have arrived, then cancel immediately.
    tokio::time::sleep(Duration::from_millis(250)).await;
    cancel.cancel();

    // receive a few Actions and find StreamCancelled.
    let mut found_cancelled = false;
    for _ in 0..10 {
        let action = match tokio::time::timeout(Duration::from_millis(800), action_rx.recv()).await {
            Ok(Some(a)) => a,
            _ => break,
        };
        if matches!(action, Action::StreamCancelled { .. }) {
            found_cancelled = true;
            break;
        }
    }
    assert!(
        found_cancelled,
        "mid-stream cancel must emit StreamCancelled within reasonable time"
    );
    finish_owned_start_turn_fixture(fixture_cleanup).await;
}
