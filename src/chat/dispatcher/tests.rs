#![cfg(test)]

use super::*;
use crate::chat::action::Action;
use crate::providers::router::MockEnvProvider;

/// F1: reasoning deltas are very fragmented, so the throttler must accumulate the fragments inside a
/// window and lose no characters (the next window emits the accumulated plus the new ones together).
#[test]
fn reasoning_progress_batcher_throttles_without_losing_chars() {
    let mut batcher = ReasoningProgressBatcher::default();
    let first = batcher
        .push("\u{20ac}\u{2192}", 1_000)
        .expect("test: first delta publishes immediately");
    assert_eq!(first, "\u{20ac}\u{2192}");

    // every fragment inside the same window is suppressed
    assert!(batcher.push("\u{20ac}", 1_010).is_none());
    assert!(batcher.push("……", 1_050).is_none());
    assert!(
        batcher
            .push("😀", 1_000 + REASONING_PROGRESS_MIN_INTERVAL_MS - 1)
            .is_none(),
        "must not publish before the window elapses"
    );

    // window elapsed: the accumulated fragments come out at once, in the same order
    let batch = batcher
        .push("!", 1_000 + REASONING_PROGRESS_MIN_INTERVAL_MS)
        .expect("test: window elapsed publishes batch");
    assert_eq!(
        batch, "\u{20ac}……😀!",
        "batch must preserve order and lose no characters"
    );

    // the buffer is cleared after publishing
    assert!(batcher.push("x", 1_000 + REASONING_PROGRESS_MIN_INTERVAL_MS).is_none());
    let next = batcher
        .push("y", 1_000 + 2 * REASONING_PROGRESS_MIN_INTERVAL_MS)
        .expect("test: next window publishes");
    assert_eq!(next, "xy");

    // an empty delta triggers no publish
    assert!(batcher.push("", 9_999_999).is_none());
}

/// The tail of the last throttle window is the part a stream is most likely
/// to end inside: reasoning stops, the window never fills, and the counter
/// silently stayed short of the body published by the terminal action.
///
/// MUTATION GUARD: make `flush` return `None` and the first assertion goes
/// red.
#[test]
fn reasoning_progress_batcher_flushes_the_final_window() {
    let mut batcher = ReasoningProgressBatcher::default();
    let first = batcher.push("thinking", 1_000).expect("test: first delta publishes");
    assert_eq!(first, "thinking");

    // Everything after this lands inside one unfinished window.
    assert!(batcher.push(" about", 1_010).is_none());
    assert!(batcher.push(" the answer", 1_020).is_none());

    assert_eq!(
        batcher.flush().as_deref(),
        Some(" about the answer"),
        "stream end must publish the buffered tail"
    );
    assert!(batcher.flush().is_none(), "a second flush has nothing left to publish");

    // And a flush of an untouched batcher stays quiet.
    assert!(ReasoningProgressBatcher::default().flush().is_none());
}

#[test]
fn redux_turn_uses_same_canonical_session_scope_as_chat_messages() {
    let policy = full_tool_security_policy();
    let spawn_context = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
        "turn-run".to_string(),
        "chat:chat-stable".to_string(),
    );

    let context = chat_tool_execution_context(policy.as_ref(), Some(&spawn_context), None, "draft");

    assert_eq!(
        context.envelope.session_key,
        crate::runtime::envelope::RuntimeEnvelope::chat_canonical_session_key("chat-stable")
    );
    assert_eq!(context.envelope.legacy_session_key.as_deref(), Some("chat:chat-stable"));
    assert_eq!(context.envelope.run_id.as_deref(), Some("turn-run"));
}

// ── BUG-07: ModelSlot hot-swap ─────────────────────────────────────
#[test]
fn model_slot_current_reflects_set() {
    let slot = ModelSlot::from("anthropic/claude-sonnet-4");
    assert_eq!(&*slot.current(), "anthropic/claude-sonnet-4");
    // A clone shares the same inner cell (cross-spawn-boundary handle).
    let handle = slot.clone();
    handle.set(Arc::from("openai/gpt-4o"));
    assert_eq!(
        &*slot.current(),
        "openai/gpt-4o",
        "set via handle is visible through original"
    );
}

#[test]
fn model_handle_exposes_deps_slot() {
    // real-deps executor returns Some(slot); shadow returns None.
    let shadow = EffectExecutor::new_shadow();
    assert!(shadow.model_handle().is_none(), "shadow has no model slot");
}

// ── Bug #3: ProviderSlot hot-swap ──────────────────────────────────
#[test]
fn provider_slot_current_reflects_set() {
    let a: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
    let b: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
    let slot = ProviderSlot::new(Arc::clone(&a));
    assert!(Arc::ptr_eq(&slot.current(), &a), "initial provider is observable");
    // A clone shares the same inner cell (cross-spawn-boundary handle).
    let handle = slot.clone();
    handle.set(Arc::clone(&b));
    assert!(
        Arc::ptr_eq(&slot.current(), &b),
        "set via clone is visible through the original handle"
    );
}

#[test]
fn provider_handle_exposes_deps_slot_only_in_real_mode() {
    // shadow has no provider slot; real-deps construction exposes one.
    let shadow = EffectExecutor::new_shadow();
    assert!(shadow.provider_handle().is_none(), "shadow has no provider slot");
}

#[tokio::test]
async fn dispatcher_try_send_ok() {
    let (dispatcher, mut rx) = ChatDispatcher::new();
    let result = dispatcher.try_dispatch(Action::ForceQuit);
    assert_eq!(result, DispatchResult::Sent);
    let received = rx.recv().await.expect("expected one Action");
    assert!(matches!(received, Action::ForceQuit));
}

#[tokio::test]
async fn dispatcher_channel_closed() {
    let (dispatcher, rx) = ChatDispatcher::new();
    drop(rx);
    let result = dispatcher.try_dispatch(Action::ForceQuit);
    assert_eq!(result, DispatchResult::ChannelClosed);
}

#[tokio::test]
async fn coalescer_passthrough_when_not_full() {
    let (tx, mut rx) = mpsc::channel::<Action>(16);
    let mut coalescer = StreamChunkCoalescer::new(tx);

    coalescer.try_send_chunk("d1".to_string(), "hello ".to_string(), 1);
    coalescer.try_send_chunk("d1".to_string(), "world".to_string(), 2);

    // Both should pass through individually (channel not full)
    let a1 = rx.recv().await.expect("expected chunk 1");
    let a2 = rx.recv().await.expect("expected chunk 2");
    match a1 {
        Action::StreamChunkReceived { delta, .. } => assert_eq!(delta, "hello "),
        other => panic!("unexpected action {other:?}"),
    }
    match a2 {
        Action::StreamChunkReceived { delta, .. } => assert_eq!(delta, "world"),
        other => panic!("unexpected action {other:?}"),
    }
    assert!(coalescer.pending_for_test().is_none());
}

#[tokio::test]
async fn coalescer_merges_when_full() {
    // capacity-1 channel: full after one write
    let (tx, mut rx) = mpsc::channel::<Action>(1);
    let mut coalescer = StreamChunkCoalescer::new(tx);

    // first: succeeds
    let r1 = coalescer.try_send_chunk("d1".to_string(), "a".to_string(), 1);
    assert_eq!(r1, DispatchResult::Sent);
    // second: channel full, goes into pending
    let r2 = coalescer.try_send_chunk("d1".to_string(), "b".to_string(), 2);
    assert_eq!(r2, DispatchResult::Backpressured);
    // third: still full, merged with pending
    let r3 = coalescer.try_send_chunk("d1".to_string(), "c".to_string(), 3);
    assert_eq!(r3, DispatchResult::Backpressured);
    let pending = coalescer.pending_for_test().expect("pending should exist");
    assert_eq!(pending.1, "bc");
    assert_eq!(pending.2, 3, "version should be max(2,3)=3 (Codex P2 fix)");

    // consume the first one to free space
    let a1 = rx.recv().await.expect("first chunk");
    match a1 {
        Action::StreamChunkReceived { delta, version, .. } => {
            assert_eq!(delta, "a");
            assert_eq!(version, 1);
        }
        other => panic!("unexpected {other:?}"),
    }

    // flush pending
    let rf = coalescer.flush();
    assert_eq!(rf, DispatchResult::Sent);
    let a_merged = rx.recv().await.expect("merged chunk");
    match a_merged {
        Action::StreamChunkReceived { delta, version, .. } => {
            assert_eq!(delta, "bc");
            assert_eq!(version, 3, "merged version is max (Codex P2 fix)");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(coalescer.pending_for_test().is_none());
}

#[tokio::test]
async fn coalescer_cross_draft_drops_old_pending() {
    let (tx, mut rx) = mpsc::channel::<Action>(1);
    let mut coalescer = StreamChunkCoalescer::new(tx);

    // d1 chunk 1 → success (fills the channel)
    coalescer.try_send_chunk("d1".to_string(), "a".to_string(), 1);
    // d1 chunk 2 → full, goes into pending
    coalescer.try_send_chunk("d1".to_string(), "b".to_string(), 2);
    // d2 chunk 1 → different draft, the old pending is dropped
    // the channel is still full (d1 chunk 1 not consumed), so d2 chunk 1 goes into pending
    let r = coalescer.try_send_chunk("d2".to_string(), "x".to_string(), 5);
    assert_eq!(r, DispatchResult::Backpressured);
    let pending = coalescer.pending_for_test().expect("pending");
    assert_eq!(pending.0, "d2");
    assert_eq!(pending.1, "x");

    // drain d1 chunk 1
    let _ = rx.recv().await;
    let _ = coalescer.flush();
    let a = rx.recv().await.expect("d2 chunk");
    match a {
        Action::StreamChunkReceived {
            draft_id,
            delta,
            version,
        } => {
            assert_eq!(draft_id, "d2");
            assert_eq!(delta, "x");
            assert_eq!(version, 5);
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// P2 fix: a high version arrives first, a low one merges later — pending.version must stay at max.
///
/// Scenario: the version=5 chunk goes through try_send first (success), the version=2 chunk arrives later
/// (full, goes into pending), and the version=3 chunk arrives after that (full, merged with pending).
/// Expectation: pending.version = max(2,3) = 3 (not 2, and not an out-of-order regression).
/// Verifies the coalescer always takes the max version regardless of arrival order.
#[tokio::test]
async fn coalescer_handles_out_of_order_versions() {
    // capacity-1 channel
    let (tx, mut rx) = mpsc::channel::<Action>(1);
    let mut coalescer = StreamChunkCoalescer::new(tx);

    // first, version=5, succeeds (fills the channel)
    let r1 = coalescer.try_send_chunk("d1".to_string(), "a".to_string(), 5);
    assert_eq!(r1, DispatchResult::Sent, "first chunk should succeed");

    // second, version=2 (lower than the already sent 5) — channel full, goes into pending
    let r2 = coalescer.try_send_chunk("d1".to_string(), "b".to_string(), 2);
    assert_eq!(r2, DispatchResult::Backpressured);

    // third, version=3 — channel still full, merged with pending(version=2)
    // merge rule: version = max(2, 3) = 3
    let r3 = coalescer.try_send_chunk("d1".to_string(), "c".to_string(), 3);
    assert_eq!(r3, DispatchResult::Backpressured);

    let pending = coalescer.pending_for_test().expect("pending should exist");
    assert_eq!(pending.1, "bc", "delta should be concatenated");
    assert_eq!(
        pending.2, 3,
        "version must be max(2,3)=3 even with out-of-order arrival (P2)"
    );

    // consume, flush, and verify the final output
    let _ = rx.recv().await.expect("first chunk (version=5)");
    let rf = coalescer.flush();
    assert_eq!(rf, DispatchResult::Sent);
    let merged = rx.recv().await.expect("merged chunk");
    match merged {
        Action::StreamChunkReceived { delta, version, .. } => {
            assert_eq!(delta, "bc");
            assert_eq!(version, 3, "flushed version should be max(2,3)=3");
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// P2 extra: verify the reducer handles a high version first + a low version later in strict-monotonic mode.
/// The reducer should accept version=5 and then ignore version=2 because 2 < current_stream_version.
#[cfg(feature = "terminal-tui")]
#[tokio::test]
async fn redux_stream_chunk_strict_monotonic_high_then_low() {
    use crate::chat::state::ChatState;
    use tokio_util::sync::CancellationToken;

    let shutdown = CancellationToken::new();
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), shutdown);

    // start the turn
    let cancel = CancellationToken::new();
    let effects0 = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel,
    });
    // TurnStarted may produce a StartTurn effect; we only confirm it does not panic
    let _ = effects0;

    // version=5 arrives first — strict-monotonic: must be accepted (5 > 0)
    let e1 = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "high".to_string(),
        version: 5,
    });
    // a RequestRedraw effect should be produced (the chunk was accepted)
    assert!(
        e1.iter()
            .any(|e| matches!(e, crate::chat::state::Effect::RequestRedraw)),
        "version=5 chunk should be accepted and produce RequestRedraw"
    );

    // version=2 arrives later — strict-monotonic: must be dropped (2 < 5)
    let e2 = state.reduce(Action::StreamChunkReceived {
        draft_id: "d1".to_string(),
        delta: "low".to_string(),
        version: 2,
    });
    // the low-version chunk is silently dropped by the reducer, so no RequestRedraw
    assert!(
        !e2.iter()
            .any(|e| matches!(e, crate::chat::state::Effect::RequestRedraw)),
        "version=2 chunk (lower than 5) should be discarded by strict-monotonic reducer"
    );
}

#[tokio::test]
async fn redux_compaction_feedback_dedupes_same_state_until_changed() {
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
    let config = crate::config::AgentCompactionConfig {
        max_context_tokens: 1_000,
        reserve_tokens: 10,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let history = vec![
        crate::providers::traits::ChatMessage::system("sys"),
        crate::providers::traits::ChatMessage::user("same state"),
    ];
    let changed_history = vec![
        crate::providers::traits::ChatMessage::system("sys"),
        crate::providers::traits::ChatMessage::user("changed state"),
        crate::providers::traits::ChatMessage::assistant("summary"),
    ];
    let mut last_feedback = None;

    assert!(
        send_redux_compaction_feedback(&action_tx, 1, 20, &history, &config, "test", &mut last_feedback)
            .await
            .expect("first feedback send"),
        "first feedback should be emitted"
    );
    assert!(
        !send_redux_compaction_feedback(&action_tx, 1, 20, &history, &config, "test", &mut last_feedback)
            .await
            .expect("duplicate feedback send"),
        "identical feedback should be suppressed"
    );
    assert!(
        send_redux_compaction_feedback(&action_tx, 2, 40, &changed_history, &config, "test", &mut last_feedback,)
            .await
            .expect("changed feedback send"),
        "changed state should emit another feedback"
    );

    let sent = action_rx.try_recv().expect("first feedback action should be queued");
    assert!(matches!(sent, Action::SystemMessageAdded { .. }));
    let sent = action_rx.try_recv().expect("changed feedback action should be queued");
    assert!(matches!(sent, Action::SystemMessageAdded { .. }));
    assert!(
        action_rx.try_recv().is_err(),
        "duplicate feedback must not queue an action"
    );
}

#[tokio::test]
async fn redux_injection_overbudget_diagnostic_emits_only_for_injected_delta_and_dedupes() {
    use crate::providers::traits::ChatMessage as PMsg;

    let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 10,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 120,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let original_history = vec![PMsg::system("sys"), PMsg::user("please inspect @huge.txt")];
    let mut enriched_history = original_history.clone();
    let Some(user_message) = enriched_history.get_mut(1) else {
        panic!("test fixture must include a user message");
    };
    user_message.content.push_str(&format!(
        "\n\n[Attached file context from @path mentions]\nHIDDEN_FILE_SENTINEL {}\n[End attached file context]",
        "hidden ".repeat(1000)
    ));
    let text = redux_injection_overbudget_diagnostic_text(&enriched_history, &original_history, &config)
        .expect("injection-only pressure should produce a diagnostic");
    assert!(text.contains("@path/memory context"));
    assert!(text.contains('~'));
    assert!(
        !text.contains("HIDDEN_FILE_SENTINEL"),
        "diagnostic must not include injected content"
    );

    let mut last_feedback = None;
    assert!(
        send_redux_injection_overbudget_diagnostic(
            &action_tx,
            Some(text.clone()),
            "test_injection_diag",
            &mut last_feedback,
        )
        .await
        .expect("first diagnostic send"),
        "first diagnostic should be emitted"
    );
    assert!(
        !send_redux_injection_overbudget_diagnostic(&action_tx, Some(text), "test_injection_diag", &mut last_feedback,)
            .await
            .expect("duplicate diagnostic send"),
        "identical diagnostic should be suppressed"
    );

    let sent = action_rx.try_recv().expect("diagnostic action should be queued");
    match sent {
        Action::SystemMessageAdded { text } => {
            assert!(text.contains("@path/memory context"));
            assert!(!text.contains("HIDDEN_FILE_SENTINEL"));
        }
        other => panic!("expected SystemMessageAdded, got {other:?}"),
    }
    assert!(
        action_rx.try_recv().is_err(),
        "duplicate diagnostic must not queue an action"
    );
}

#[test]
fn redux_injection_overbudget_diagnostic_skips_normal_history_overflow() {
    use crate::providers::traits::ChatMessage as PMsg;

    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 10,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 120,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let normal_overflow = vec![
        PMsg::system("sys"),
        PMsg::user(format!("ordinary visible history {}", "visible ".repeat(1000))),
    ];
    assert!(
        redux_injection_overbudget_diagnostic_text(&normal_overflow, &normal_overflow, &config).is_none(),
        "normal history overflow must not be reported as injection-driven"
    );

    let original_already_over = vec![
        PMsg::system("sys"),
        PMsg::user(format!("large original user {}", "visible ".repeat(1000))),
    ];
    let mut enriched = original_already_over.clone();
    let Some(user_message) = enriched.get_mut(1) else {
        panic!("test fixture must include a user message");
    };
    user_message.content.push_str(&format!(" {}", "hidden ".repeat(1000)));
    assert!(
        redux_injection_overbudget_diagnostic_text(&enriched, &original_already_over, &config).is_none(),
        "when original is already over budget, D5 diagnostic must not fire"
    );
}

#[tokio::test]
async fn a_failed_turn_still_publishes_the_reasoning_it_produced() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    /// Emits reasoning inside one throttle window, then breaks. The batcher
    /// is still holding that tail when the turn fails.
    struct FailingAfterReasoningProvider;

    #[async_trait]
    impl Provider for FailingAfterReasoningProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            Ok("unused".to_string())
        }

        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("unused".into()),
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
            stream::iter(vec![
                Ok(StreamChunk::reasoning_delta("weighing the")),
                Ok(StreamChunk::reasoning_delta(" last option")),
                Err(crate::providers::traits::StreamError::Provider(
                    "upstream closed the connection".to_string(),
                )),
            ])
            .boxed()
        }

        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let history = vec![PMsg::system("sys"), PMsg::user("think then break")];
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(32);
    let workspace = tempfile::TempDir::new().expect("chat admission workspace");
    let provider: Arc<dyn Provider> = Arc::new(FailingAfterReasoningProvider);
    let admitted = super::turn_characterization_tests::admit_chat_fixture(
        Arc::clone(&provider),
        None,
        workspace.path(),
        ("ollama", "model", None),
        (
            "draft-failing-reasoning",
            &history.last().expect("source input").content,
        ),
        action_tx.clone(),
        CancellationToken::new(),
    )
    .await;

    drive_start_turn_stream(
        Some(admitted.binding),
        None,
        provider,
        history.clone(),
        history,
        "model".to_string(),
        0.0,
        None,
        CancellationToken::new(),
        "draft-failing-reasoning".to_string(),
        action_tx,
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

    let mut reasoning = String::new();
    let mut reasoning_before_failure = false;
    let mut failed = false;
    while let Ok(action) = action_rx.try_recv() {
        match action {
            Action::StreamReasoningReceived { delta, .. } => {
                reasoning.push_str(&delta);
                reasoning_before_failure = !failed;
            }
            Action::StreamFailed { .. } => failed = true,
            _ => {}
        }
    }
    assert!(failed, "fixture validity: the turn must actually fail");
    assert!(
        reasoning.contains("last option"),
        "the tail of the last throttle window must survive a failed turn: {reasoning:?}"
    );
    assert!(
        reasoning_before_failure,
        "the flush has to precede StreamFailed, which retires the draft"
    );
}

#[tokio::test]
async fn redux_driver_preflight_emits_injection_overbudget_diagnostic_without_hidden_content() {
    use crate::providers::traits::{
        ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
    };
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};

    struct DiagnosticProvider;

    #[async_trait]
    impl Provider for DiagnosticProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
            Ok("unused".to_string())
        }

        async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("unused".into()),
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
            stream::iter(vec![
                Ok(StreamChunk::delta("diagnostic-ok")),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed()
        }

        async fn warmup(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let original_history = vec![PMsg::system("sys"), PMsg::user("please inspect @huge.txt")];
    let mut enriched_history = original_history.clone();
    let Some(user_message) = enriched_history.get_mut(1) else {
        panic!("test fixture must include a user message");
    };
    user_message.content.push_str(&format!(
        "\n\n[Attached file context from @path mentions]\nHIDDEN_FILE_SENTINEL {}\n[End attached file context]",
        "hidden ".repeat(1000)
    ));
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 10,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 120,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(16);
    let workspace = tempfile::TempDir::new().expect("chat admission workspace");
    let provider: Arc<dyn Provider> = Arc::new(DiagnosticProvider);
    let admitted = super::turn_characterization_tests::admit_chat_fixture(
        Arc::clone(&provider),
        None,
        workspace.path(),
        ("ollama", "model", None),
        (
            "draft-injection-diagnostic",
            &original_history.last().expect("source input").content,
        ),
        action_tx.clone(),
        CancellationToken::new(),
    )
    .await;

    drive_start_turn_stream(
        Some(admitted.binding),
        None,
        provider,
        enriched_history,
        original_history.clone(),
        "model".to_string(),
        0.0,
        Some(config),
        CancellationToken::new(),
        "draft-injection-diagnostic".to_string(),
        action_tx,
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

    let mut saw_diagnostic = false;
    let mut saw_completion = false;
    while let Ok(action) = action_rx.try_recv() {
        match action {
            Action::SystemMessageAdded { text } if text.contains("@path/memory context") => {
                saw_diagnostic = true;
                assert!(
                    !text.contains("HIDDEN_FILE_SENTINEL"),
                    "diagnostic must not leak injected file content"
                );
            }
            Action::SystemMessageAdded { text } => {
                assert!(
                    !text.contains("HIDDEN_FILE_SENTINEL"),
                    "ordinary feedback must not leak injected file content"
                );
            }
            Action::StreamCompleted { final_text, .. } => {
                saw_completion = final_text.contains("diagnostic-ok");
            }
            Action::RecordUserTurn(content) => {
                assert!(
                    !content.contains("HIDDEN_FILE_SENTINEL"),
                    "driver must not persist enriched user content"
                );
            }
            _ => {}
        }
    }

    assert!(saw_diagnostic, "preflight must emit the D5 diagnostic");
    assert!(saw_completion, "driver must still complete after trim");
    assert!(
        original_history
            .iter()
            .all(|message| !message.content.contains("HIDDEN_FILE_SENTINEL")),
        "original guard history fixture remains free of injected content"
    );
}

#[tokio::test]
async fn effect_executor_shadow_log_trace_runs() {
    let executor = EffectExecutor::new_shadow();
    // LogTrace really executes (it runs in shadow too); here we only verify no panic / no external await
    executor
        .execute(Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: "shadow log".to_string(),
        })
        .await;
}

#[tokio::test]
async fn effect_executor_shadow_business_noop() {
    let executor = EffectExecutor::new_shadow();
    // every business effect is a no-op; we only verify there is no panic
    executor.execute(Effect::RequestRedraw).await;
    executor.execute(Effect::Quit).await;
    executor.execute(Effect::CancelDraft("d1".to_string())).await;
    executor
        .execute(Effect::SendDraftFinalize {
            draft_id: "d1".to_string(),
            text: "hello".to_string(),
        })
        .await;
}

// ── S5 P0-3 supervised approval fail-safe deny ──────────────────────────

#[test]
fn s5_release_p0_3_supervised_unset_env_denies_by_default() {
    // env unset → fail-safe deny (BREAKING — it used to auto-approve)
    assert!(!resolve_supervised_approval_override(None));
}

#[test]
fn s5_release_p0_3_supervised_env_allow_approves() {
    for v in ["allow", "ALLOW", " allow ", "y", "Y", "yes", "YES", "1"] {
        assert!(resolve_supervised_approval_override(Some(v)), "{v:?} must be approved");
    }
}

#[test]
fn s5_release_p0_3_supervised_env_deny_rejects() {
    for v in ["deny", "DENY", " deny ", "n", "N", "no", "NO", "0", "", "garbage"] {
        assert!(!resolve_supervised_approval_override(Some(v)), "{v:?} must be rejected");
    }
}

// ── S5 P0-1: stream error classification regression (driver protocol layer) ──────────

#[tokio::test]
async fn s5_release_p0_1_parallel_tool_calls_serialize() {
    // SCRIPT emits two tool_call chunks + final inside the same stream.
    // StreamChunkCoalescer uses ToolCallChunk to simulate the serial scenario; here we directly verify
    // that StreamChunk::tool_call_chunk can carry several ToolCallChunks with has_tool_calls()=true.
    use crate::providers::traits::{StreamChunk, ToolCallChunk};
    let calls = vec![
        ToolCallChunk::new("t1".to_string(), "shell".to_string(), "{}".to_string(), 0),
        ToolCallChunk::new("t2".to_string(), "file_read".to_string(), "{}".to_string(), 1),
    ];
    let chunk = StreamChunk::tool_call_chunk(calls);
    assert!(
        chunk.has_tool_calls(),
        "parallel tool calls must be recognised by the chunk"
    );
    assert_eq!(chunk.tool_calls.len(), 2, "must carry 2 tool calls");
    let first = chunk.tool_calls.first().expect("tool[0]");
    let second = chunk.tool_calls.get(1).expect("tool[1]");
    assert_eq!(first.id, "t1");
    assert_eq!(second.id, "t2");
    // order preserved: the driver serializes by the index field (we only assert the data layer here).
    assert_eq!(first.index, 0);
    assert_eq!(second.index, 1);
}

#[test]
fn s5_release_p0_3_full_autonomy_skips_approval_entirely() {
    let policy = crate::security::SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Full,
        ..crate::security::SecurityPolicy::default()
    };
    assert_eq!(
        policy.decide("shell", "user", "terminal", "private"),
        crate::security::policy::ToolDecision::Allow
    );
    assert_eq!(
        policy.decide("file_write", "user", "terminal", "private"),
        crate::security::policy::ToolDecision::Allow
    );
}
