#![cfg(test)]

use super::*;

/// **S3 T3-1 Step 4**: basic ApprovalRouter resolve / register path.
#[tokio::test]
async fn t31_approval_router_register_and_resolve_basic() {
    let router = ApprovalRouter::new();
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    router.register("call-1".to_string(), tx);
    assert!(router.resolve("call-1", true), "resolve should find the tx");
    assert!(rx.await.expect("oneshot rx must resolve"));
    // resolving the same id a second time must return false (nothing pending)
    assert!(!router.resolve("call-1", false), "second resolve must miss");
}

/// The default causal plane turns an approval-required model call into a
/// durable pending decision. Chat may continue the provider loop with that
/// result, but it must not ask the legacy same-turn router for authority or
/// start the backend. Review and execution happen later through the
/// explicit decision approval and resume commands.
#[tokio::test]
async fn t31_driver_default_causal_approval_persists_without_backend_execution() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
        ToolCallChunk,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

    struct PendingShell {
        executed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl crate::tools::Tool for PendingShell {
        fn name(&self) -> &str {
            "shell"
        }

        fn description(&self) -> &str {
            "shell"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
            self.executed.store(true, AtomicOrdering::SeqCst);
            Ok(crate::tools::ToolResult {
                success: true,
                output: "unexpected execution".into(),
                error: None,
            })
        }
    }

    struct ToolThenText {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for ToolThenText {
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
            if self.calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                stream::iter(vec![
                    Ok(StreamChunk::tool_call_chunk(vec![ToolCallChunk::new(
                        "call-pending-shell",
                        "shell",
                        r#"{"command":"echo pending"}"#,
                        0,
                    )])),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            } else {
                stream::iter(vec![
                    Ok(StreamChunk::delta("approval pending")),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
        }

        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let executed = Arc::new(AtomicBool::new(false));
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(ToolThenText {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    deps.tools_registry = Some(Arc::new(vec![Box::new(PendingShell {
        executed: Arc::clone(&executed),
    }) as Box<dyn crate::tools::Tool>]));
    deps.tool_security_policy = tool_security_policy(crate::security::AutonomyLevel::Supervised);
    let (binding, cleanup) =
        admit_owned_start_turn_fixture(&deps, "draft-pending-approval", "stream the fixture reply", true)
            .await
            .expect("Q42 genuine admission prerequisite");
    let executor = EffectExecutor::new_with_deps(deps);
    executor
        .execute(Effect::StartTurn {
            provider_turn_task_id: None,
            draft_id: "draft-pending-approval".into(),
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

    let mut saw_pending = false;
    let mut saw_completion = false;
    for _ in 0..32 {
        let action = tokio::time::timeout(Duration::from_secs(3), action_rx.recv())
            .await
            .expect("action within 3s")
            .expect("must arrive");
        match action {
            Action::ToolApprovalRequested { .. } => {
                panic!("default causal approval must not use same-turn router authority")
            }
            Action::ToolStarted { .. } => panic!("pending approval must not start the backend"),
            Action::ToolFinished {
                success, name, result, ..
            } => {
                assert_eq!(name, "shell");
                assert!(!success, "pending approval is not an executed tool success");
                let result = result.expect("pending approval result");
                assert!(
                    result.contains("Approval is required for decision"),
                    "result={result:?}"
                );
                assert!(result.contains("operation has not run"), "result={result:?}");
                saw_pending = true;
            }
            Action::StreamCompleted { final_text, .. } => {
                assert!(
                    final_text.contains("Approval is required for decision")
                        && final_text.contains("operation has not run"),
                    "final_text={final_text:?}"
                );
                saw_completion = true;
                break;
            }
            Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
            _ => {}
        }
    }
    assert!(saw_pending, "Chat must publish the durable pending decision result");
    assert!(
        saw_completion,
        "Chat must finish after reporting the pending decision to the provider"
    );
    assert!(!executed.load(AtomicOrdering::SeqCst), "unapproved tool backend ran");
    finish_owned_start_turn_fixture(cleanup).await;
}

#[tokio::test]
async fn t31_driver_approval_review_then_resume_executes_exact_call() {
    crate::runtime::turn_decision::run_chat_approval_contract_fixture(false).await;
}

#[tokio::test]
async fn t31_driver_approval_denial_then_resume_skips_tool_execution() {
    crate::runtime::turn_decision::run_chat_approval_contract_fixture(true).await;
}

/// **S3 T3-1 Step 5**: io error retry — the driver succeeds on attempt 3 after io errors on attempts 1 and 2.
///
/// A short backoff keeps the unit test fast (1s+2s, 3.5s in total, comfortably inside the unit
/// test timeout).
#[tokio::test]
async fn t31_driver_network_timeout_retries_with_backoff_then_succeeds() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError, StreamOptions,
        StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct FlakyProvider {
        counter: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for FlakyProvider {
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
            if n < 2 {
                stream::iter(vec![Err::<StreamChunk, _>(StreamError::Io(std::io::Error::other(
                    "simulated network timeout",
                )))])
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
    deps.provider = Arc::new(FlakyProvider {
        counter: Arc::new(AtomicUsize::new(0)),
    });
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);
            let binding = admit_start_turn_fixture(&fixture_deps, "draft-flaky", "stream the fixture reply", false)
                .await
                .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-flaky".into(),
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

            let mut retry_attempts: u8 = 0;
            let mut saw_completion = false;
            // total time budget: ~3.5s of real sleep + some RTT, with 8s of headroom.
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            loop {
                assert!(std::time::Instant::now() < deadline, "test deadline exceeded");
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let action = match tokio::time::timeout(remaining.min(Duration::from_secs(4)), action_rx.recv()).await {
                    Ok(Some(a)) => a,
                    Ok(None) => break,
                    Err(_) => continue,
                };
                match action {
                    Action::StreamRetryAttempt { attempt, .. } => {
                        retry_attempts = retry_attempts.max(attempt);
                    }
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("recovered"));
                        saw_completion = true;
                        break;
                    }
                    Action::StreamFailed { err, .. } => {
                        panic!("driver should retry, not fail: {err}");
                    }
                    _ => {}
                }
            }
            assert!(
                retry_attempts >= 1,
                "must emit at least one StreamRetryAttempt (got {retry_attempts})"
            );
            assert!(saw_completion, "must complete after backoff retries");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}

/// A network error after any model output is not safe to retry: replaying
/// the request would duplicate streamed text/reasoning and could rebuild a
/// partial tool call with different arguments.
#[tokio::test]
async fn driver_does_not_retry_network_error_after_reasoning_output() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError, StreamOptions,
        StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct PartialThenErrorProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for PartialThenErrorProvider {
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
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            stream::iter(vec![
                Ok(StreamChunk::reasoning_delta("partial reasoning")),
                Err(StreamError::Io(std::io::Error::other("stream interrupted"))),
            ])
            .boxed()
        }

        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(PartialThenErrorProvider {
        calls: Arc::clone(&calls),
    });
    let (binding, fixture_cleanup) =
        admit_owned_start_turn_fixture(&deps, "draft-partial-network-error", "stream the fixture reply", false)
            .await
            .expect("Q42 genuine admission prerequisite");
    let executor = EffectExecutor::new_with_deps(deps);
    executor
        .execute(Effect::StartTurn {
            provider_turn_task_id: None,
            draft_id: "draft-partial-network-error".into(),
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

    let mut retry_attempts = 0_u8;
    let failed = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match action_rx.recv().await {
                Some(Action::StreamRetryAttempt { attempt, .. }) => retry_attempts = retry_attempts.max(attempt),
                Some(Action::StreamFailed { err, retryable, .. }) => break (err, retryable),
                Some(_) => {}
                None => panic!("action channel closed before StreamFailed"),
            }
        }
    })
    .await
    .expect("partial stream must fail promptly");

    assert_eq!(
        calls.load(AtomicOrdering::SeqCst),
        1,
        "partial response must not be replayed"
    );
    assert_eq!(retry_attempts, 0, "partial response must not enter the retry loop");
    assert!(!failed.1, "a partial response cannot be retried safely");
    assert!(failed.0.contains("after model output"), "err={}", failed.0);
    finish_owned_start_turn_fixture(fixture_cleanup).await;
}

/// **S3 T3-1 Step 5**: persistent io errors → retries exhausted → StreamFailed(retryable=false).
#[tokio::test]
async fn t31_driver_network_timeout_exhausted_emits_stream_failed() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError, StreamOptions,
        StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct AlwaysIoErrProvider;
    #[async_trait]
    impl Provider for AlwaysIoErrProvider {
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
            stream::iter(vec![Err::<StreamChunk, _>(StreamError::Io(std::io::Error::other(
                "persistent network failure",
            )))])
            .boxed()
        }
        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    deps.provider = Arc::new(AlwaysIoErrProvider);
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("Q42 fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);
            let binding = admit_start_turn_fixture(&fixture_deps, "draft-net-fail", "stream the fixture reply", false)
                .await
                .expect("Q42 genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-net-fail".into(),
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

            let mut saw_failed = false;
            // backoff = 500ms + 1s + 2s = 3.5s + RTT, with 10s of headroom.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                assert!(std::time::Instant::now() < deadline, "test deadline exceeded");
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let action = match tokio::time::timeout(remaining.min(Duration::from_secs(5)), action_rx.recv()).await {
                    Ok(Some(a)) => a,
                    Ok(None) => break,
                    Err(_) => continue,
                };
                if let Action::StreamFailed { err, retryable, .. } = action {
                    assert!(!retryable, "exhausted retries must be non-retryable");
                    assert!(err.contains("network retries exhausted"), "err={err}");
                    saw_failed = true;
                    break;
                }
            }
            assert!(saw_failed, "must emit StreamFailed after exhausting network retries");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("Q42 fixture cleanup");
}
