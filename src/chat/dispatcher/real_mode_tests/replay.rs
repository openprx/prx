#![cfg(test)]

use super::*;

// ─── S2.5 T2.5-3: Effect replay idempotency tests ──────────────────────────

/// S2.5 T2.5-3: dispatching SaveSession twice in a row with the same snapshot gives store_count == 2,
/// and the state converges (idempotent-overwrite semantics: each write overwrites, ending consistent).
#[tokio::test]
async fn s2_5_t2_5_3_save_session_dispatch_idempotent() {
    let store_count = Arc::new(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
        inner: NoneMemory::new(),
        store_count: Arc::clone(&store_count),
    });
    let shutdown = CancellationToken::new();
    let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let executor = EffectExecutor::new_with_deps(deps);

    let session = ChatSession::new("prov", "model");
    executor.execute(Effect::SaveSession(session.clone())).await;
    executor.execute(Effect::SaveSession(session)).await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        store_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "two SaveSessions must trigger memory.store twice (idempotent-overwrite, not deduplication)"
    );
}

/// S2.5 T2.5-3: two NotifyHooks for one event hit the real hook twice (fire-and-forget does not dedupe).
///
/// Verified by registering a real hook + touching a sentinel + counting file size (a second touch updates
/// mtime but not size), so appending a line with `>>` is more reliable: first creates, second grows by 1 byte.
#[tokio::test]
async fn s2_5_t2_5_3_notify_hook_repeat_no_double_fire() {
    use crate::hooks::HookEvent;

    let temp = TempDir::new().expect("tempdir");
    let counter = temp.path().join("hook_counter.log");
    let counter_str = counter.to_str().expect("valid path");

    // each trigger appends one character to the counter file (append implemented with sh -c).
    let hooks_json = serde_json::json!({
        "enabled": true,
        "hooks": {
            "turn_complete": [
                {
                    "command": "sh",
                    "args": ["-c", format!("printf x >> {counter_str}")],
                    "stdin_json": false
                }
            ]
        }
    });
    std::fs::write(temp.path().join("hooks.json"), hooks_json.to_string()).expect("write hooks.json");

    // The operator opted in to workspace hooks (HOOKS-ENV).
    let hooks =
        Arc::new(HookManager::new(temp.path().to_path_buf()).allowing_workspace_hooks(temp.path().join("hook-launch")));
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
    let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
    let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
    let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
    let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
    let shutdown = CancellationToken::new();
    let deps = EffectDeps {
        provider,
        memory,
        memory_event_recording: MemoryEventRecording::default(),
        channel,
        hooks: Arc::clone(&hooks),
        observer,
        action_tx,
        provider_turn_lifecycle_tx: None,
        dual_write_guard: RuntimeDualWriteGuard::new(),
        redraw_tx: Some(redraw_tx),
        #[cfg(feature = "terminal-tui")]
        tui_mirror: None,
        shutdown,
        model: ModelSlot::from("test-model"),
        temperature: 0.0,
        tools_registry: None,
        approval_router: Arc::new(ApprovalRouter::new()),
        tool_security_policy: full_tool_security_policy(),
        tool_tiering: crate::config::ToolTieringConfig::default(),
    };
    let executor = EffectExecutor::new_with_deps(deps);

    executor
        .execute(Effect::NotifyHook {
            event: HookEvent::TurnComplete,
            payload: serde_json::json!({"seq": 1}),
        })
        .await;
    executor
        .execute(Effect::NotifyHook {
            event: HookEvent::TurnComplete,
            payload: serde_json::json!({"seq": 2}),
        })
        .await;

    // the hook command is spawned; give it enough time.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let bytes = std::fs::read(&counter).unwrap_or_default();
    assert_eq!(
        bytes.len(),
        2,
        "two NotifyHooks must run the hook twice → the counter accumulates 2 bytes, measured {} bytes",
        bytes.len()
    );
}

/// S2.5 T2.5-3: cancelling a CancelToken three times does not panic (the token is internally idempotent).
///
/// The first cancel makes token.is_cancelled() == true; later dispatches must keep it true
/// with no panic and no new side effects.
#[tokio::test]
async fn s2_5_t2_5_3_cancel_token_triple_cancel_no_panic() {
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let executor = EffectExecutor::new_with_deps(deps);

    let token = CancellationToken::new();
    assert!(!token.is_cancelled(), "token initially not cancelled");

    for _ in 0..3 {
        executor.execute(Effect::CancelToken(token.clone())).await;
    }
    // all three must keep the token at cancelled = true, with no panic.
    assert!(
        token.is_cancelled(),
        "the token must stay cancelled after three cancels"
    );
}

/// S2.5 T2.5-3: dispatching two SaveSessions concurrently has no race / no deadlock
/// (regression guard after the T3-3-fixB D1 inline await).
///
/// Two spawns call execute; once the tasks finish store_count == 2, with no panic and no hang.
#[tokio::test]
async fn s2_5_t2_5_3_save_session_concurrent_dispatch_no_race() {
    let store_count = Arc::new(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
        inner: NoneMemory::new(),
        store_count: Arc::clone(&store_count),
    });
    let shutdown = CancellationToken::new();
    let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let executor = Arc::new(EffectExecutor::new_with_deps(deps));

    let session = ChatSession::new("prov", "model");
    let exec1 = Arc::clone(&executor);
    let sess1 = session.clone();
    let h1 = tokio::spawn(async move {
        exec1.execute(Effect::SaveSession(sess1)).await;
    });
    let exec2 = Arc::clone(&executor);
    let sess2 = session;
    let h2 = tokio::spawn(async move {
        exec2.execute(Effect::SaveSession(sess2)).await;
    });

    tokio::time::timeout(Duration::from_secs(5), async {
        let _ = h1.await;
        let _ = h2.await;
    })
    .await
    .expect("test: concurrent dispatch should not deadlock");

    // the SaveSession subtask is spawned asynchronously; wait for it to finish.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        store_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "both concurrent SaveSessions must trigger memory.store once each (two in total)"
    );
}
