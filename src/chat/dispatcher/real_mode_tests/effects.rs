#![cfg(test)]

use super::*;

#[tokio::test]
async fn queued_save_acknowledgement_preserves_loaded_session_and_next_snapshot_key() {
    let temp_memory = tempfile::TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp_memory.path()).unwrap());
    let (deps, mut action_rx, _hooks, _temp) = build_deps(Arc::clone(&memory), CancellationToken::new());
    let executor = EffectExecutor::new_with_deps(deps);
    let mut old = ChatSession::new("prx-provider", "prx-model");
    old.id = "prx-old-session".into();
    executor.execute(Effect::SaveSession(old)).await;
    let acknowledgement = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(&acknowledgement, Action::SessionSaved { id } if id == "prx-old-session"));

    // A newer load can already be ahead of this save acknowledgement in the
    // reducer queue. Its identity must survive the real executor's old reply.
    let mut state = crate::chat::state::ChatState::new(
        Arc::from("prx-provider"),
        Arc::from("prx-model"),
        CancellationToken::new(),
    );
    let mut loaded = ChatSession::new("prx-provider", "prx-model");
    loaded.id = "prx-new-session".into();
    let _ = state.reduce(Action::SessionLoaded(loaded));
    let _ = state.reduce(acknowledgement);
    assert_eq!(state.session.id, "prx-new-session");
    let _ = state.reduce(Action::RecordUserTurn("new request".into()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "prx-new-draft".into(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "new reply".into(),
    });
    let effects = state.reduce(Action::StreamCompleted {
        draft_id: "prx-new-draft".into(),
        final_text: "new reply".into(),
        reasoning: String::new(),
    });
    let save = effects
        .into_iter()
        .find(|effect| matches!(effect, Effect::SaveSession(_)))
        .expect("completed new turn must save its own snapshot");
    executor.execute(save).await;

    let saved = memory.get("chat_session:prx-new-session").await.unwrap().unwrap();
    let saved = ChatSession::from_json(&saved.content).unwrap();
    assert_eq!(saved.turns.len(), 2);
    assert_eq!(
        saved.turns.first().map(|turn| turn.content.as_str()),
        Some("new request")
    );
    let previous = memory.get("chat_session:prx-old-session").await.unwrap().unwrap();
    assert!(ChatSession::from_json(&previous.content).unwrap().turns.is_empty());
}

#[tokio::test]
async fn real_mode_save_session_triggers_memory_store() {
    // Prove that the reducer's pre-sanitized snapshot is really written to Memory after the dispatcher's
    // second sanitization, and that marker/hash/byte-count stay single-copy.
    let temp_memory = tempfile::TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(temp_memory.path()).unwrap());
    let shutdown = CancellationToken::new();
    let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
    let executor = EffectExecutor::new_with_deps(deps.clone());
    assert!(!executor.is_shadow());

    let secret = "AKIAABCDEFGHIJKLMNOP";
    let long_content = format!(
        "raw effect {secret} Authorization: Bearer abcdefghijklmnop\n{}",
        "\u{20ac}".repeat(5_000)
    );
    let pre_truncate_len = crate::security::redaction::redact_secrets(&long_content).len();
    let mut state = crate::chat::state::ChatState::new(Arc::from("prov"), Arc::from("model"), CancellationToken::new());
    state.session.id = "dispatcher-sanitize-session".to_string();
    let _ = state.reduce(Action::RecordUserTurn(long_content));
    let effects = state.reduce(Action::BackgroundSessionRecorded {
        summary: crate::chat::sessions::PersistedSessionSummary {
            id: "child".to_string(),
            seq: 1,
            kind: "agent".to_string(),
            origin: "model".to_string(),
            status: "completed".to_string(),
            title: "child".to_string(),
            summary: "done".to_string(),
            token_usage_records: Vec::new(),
            created_at: chrono::Utc::now(),
        },
    });
    let save_effect = effects
        .into_iter()
        .find(|effect| matches!(effect, Effect::SaveSession(_)))
        .expect("reducer must emit sanitized SaveSession");
    let memory_key = format!("{}:{}", crate::chat::session::SESSION_MEMORY_PREFIX, state.session.id);
    executor.execute(save_effect).await;

    let stored = deps.memory.get(&memory_key).await.unwrap().unwrap();
    assert!(!stored.content.contains(secret));
    assert!(stored.content.contains('\u{20ac}'));
    let stored_session = ChatSession::from_json(&stored.content).unwrap();
    let stored_content = stored_session.turns.first().map(|turn| turn.content.as_str()).unwrap();
    assert!(stored_content.len() <= 10 * 1024);
    assert_eq!(stored_content.matches("[... truncated (").count(), 1);
    assert_eq!(stored_content.matches("bytes total, ref:").count(), 1);
    assert!(stored_content.contains(&format!("{pre_truncate_len} bytes total")));
    assert_eq!(
        crate::chat::sanitize::sanitize_for_persistence(stored_content),
        stored_content,
        "dispatcher-stored projection must be idempotent"
    );
    let recalled = deps.memory.recall(secret, 10, None).await.unwrap();
    assert!(recalled.iter().all(|entry| !entry.content.contains(secret)));
    let mut raw_session = ChatSession::new("prov", "model");
    raw_session.id = "dispatcher-raw-session".to_string();
    raw_session.add_assistant_turn(&format!("manual raw effect {secret}"), Vec::new());
    let raw_key = raw_session.memory_key();
    executor.execute(Effect::SaveSession(raw_session)).await;
    let raw_stored = deps.memory.get(&raw_key).await.unwrap().unwrap();
    assert!(!raw_stored.content.contains(secret));
    // RAII scope: the guard must reset automatically once the subtask finishes (it must not stick).
    assert!(
        !deps.dual_write_guard.is_active(),
        "dual_write_guard should auto-clear after SaveSession completes (RAII scope)"
    );
}

#[tokio::test]
async fn real_mode_cancel_draft_invokes_channel() {
    // CancelDraft is a short synchronous path, awaited directly; not panicking means the path is clear.
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
    let executor = EffectExecutor::new_with_deps(deps);

    executor.execute(Effect::CancelDraft("draft-x".to_string())).await;
    // passing means no panic and no hang; TerminalChannel.cancel_draft always returns Ok.
}

/// T3-3-c-3 closed loop: the reducer dispatches `StreamCompleted` → the effects include SaveSession, and
/// after EffectExecutor::execute_real memory.store is called once (reducer single-source persistence works).
#[tokio::test]
async fn t3_3c_stream_completed_drives_save_session_through_executor() {
    use crate::chat::action::Action;
    use crate::chat::state::ChatState;

    let store_count = Arc::new(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
        inner: NoneMemory::new(),
        store_count: Arc::clone(&store_count),
    });
    let shutdown = CancellationToken::new();
    let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
    let executor = EffectExecutor::new_with_deps(deps.clone());

    // use the real reducer to generate the Effect sequence (with SaveSession) and feed them to the executor.
    let mut state = ChatState::new(
        Arc::from("test-prov"),
        Arc::from("test-model"),
        CancellationToken::new(),
    );
    state.session.id = "t3-3c-session".to_string();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-T3-3c".to_string(),
        cancel: CancellationToken::new(),
    });
    let effects = state.reduce(Action::StreamCompleted {
        draft_id: "d-T3-3c".to_string(),
        final_text: "the answer".to_string(),
        reasoning: String::new(),
    });
    let mut had_save_session = false;
    for effect in effects {
        if matches!(effect, Effect::SaveSession(_)) {
            had_save_session = true;
        }
        executor.execute(effect).await;
    }
    assert!(had_save_session, "reducer must emit SaveSession for StreamCompleted");

    let final_count = wait_for_count(&store_count, 1, Duration::from_secs(2)).await;
    assert_eq!(
        final_count, 1,
        "a reducer-emitted SaveSession must trigger memory.store once"
    );
}

/// T3-3-fixA P0-2: Exit-after-completed persistence equivalence across the four modes.
///
/// Verify that the SaveSession emitted by the reducer after a complete turn triggers exactly one
/// memory.store in every mode — that is, the reducer persistence path is **mode independent**.
/// After the fixA P0-2 fix the Pure mode reducer is the only persistence source, and this test confirms
/// it agrees with Off/Both/Redux on reducer persistence semantics (write count + content).
///
/// Note: the legacy save_session on chat::run main loop exit is gated by the ReduxMode guard, whose
/// truth table is already covered by pure_mode_skips_legacy_exit_save_via_redux_mode_guard.
#[tokio::test]
async fn t3_3_fix_a_exit_after_completed_persistence_parity() {
    use crate::chat::action::Action;
    use crate::chat::state::ChatState;

    for tag in ["off", "both", "redux", "pure"] {
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let mut state = ChatState::new(
            Arc::from("test-prov"),
            Arc::from("test-model"),
            CancellationToken::new(),
        );
        state.session.id = format!("sess-fixA-{tag}");

        // complete turn: user → turn started → assistant recorded → stream completed
        let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
        let _ = state.reduce(Action::TurnStarted {
            draft_id: format!("d-{tag}"),
            cancel: CancellationToken::new(),
        });
        let _ = state.reduce(Action::RecordAssistantTurn {
            task_id: None,
            content: "a".to_string(),
        });
        let effects = state.reduce(Action::StreamCompleted {
            draft_id: format!("d-{tag}"),
            final_text: "a".to_string(),
            reasoning: String::new(),
        });

        let mut had_save = false;
        for effect in effects {
            if matches!(effect, Effect::SaveSession(_)) {
                had_save = true;
            }
            executor.execute(effect).await;
        }
        assert!(
            had_save,
            "[{tag}] the reducer must emit SaveSession when a turn completes"
        );

        let final_count = wait_for_count(&store_count, 1, Duration::from_secs(2)).await;
        assert_eq!(
            final_count, 1,
            "[{tag}] reducer persistence must be mode independent — a complete turn triggers memory.store once",
        );
    }
}

/// T3-3-fixA P0-2: Exit-while-streaming consistency (no partial save) across the four modes.
///
/// When exiting during streaming (the user did not wait for the stream to finish) the reducer must not
/// emit SaveSession — a direct consequence of the Cancelled/Error rows of the appendix B decision table.
/// This test simulates the mid-exit window where a turn started but neither RecordAssistantTurn nor
/// StreamCompleted arrived, and verifies memory.store == 0 (no partial state is persisted).
#[tokio::test]
async fn t3_3_fix_a_exit_while_streaming_no_partial_save() {
    use crate::chat::action::Action;
    use crate::chat::state::ChatState;

    for tag in ["off", "both", "redux", "pure"] {
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let mut state = ChatState::new(
            Arc::from("test-prov"),
            Arc::from("test-model"),
            CancellationToken::new(),
        );
        state.session.id = format!("sess-stream-{tag}");

        // user already recorded, turn already streaming, but the stream never completed (mid-exit window)
        let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
        let user_effects: Vec<Effect> = state
            .reduce(Action::TurnStarted {
                draft_id: format!("d-{tag}"),
                cancel: CancellationToken::new(),
            })
            .into_iter()
            .chain(state.reduce(Action::StreamChunkReceived {
                draft_id: format!("d-{tag}"),
                delta: "partial".to_string(),
                version: 1,
            }))
            .collect();

        assert!(
            !user_effects.iter().any(|e| matches!(e, Effect::SaveSession(_))),
            "[{tag}] effects mid-stream must not contain SaveSession"
        );

        // execute all already emitted effects (including LogTrace / RequestRedraw etc.)
        for effect in user_effects {
            executor.execute(effect).await;
        }

        // the negative assertion keeps a fixed wait: confirm no spawned subtask wrote within the window
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            store_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "[{tag}] memory.store must be 0 when exiting mid-stream (no partial save)",
        );
    }
}

/// T3-3-fixB B5: the SaveSession snapshot on the driver path must contain this turn's assistant.
///
/// End to end: use the MockEnvProvider default stream (one final chunk), drive the driver through a full
/// run, feed the fed-back Action sequence into the reducer in order, and assert that:
///   1. the driver sends RecordAssistantTurn first, then StreamCompleted (the B5 ordering contract)
///   2. the SaveSession.turns.last() triggered by StreamCompleted is this turn's assistant (the fixA P0-1 contract)
///   3. turns.len() is strictly 2 (user + assistant, no double write)
///
/// Before the fixB B5 fix the driver sent StreamCompleted directly, so session.turns was missing this
/// turn's assistant when the reducer built the SaveSession snapshot; a failure here pinpoints a regression.
#[tokio::test]
async fn t3_3_fix_b_driver_path_save_session_includes_assistant() {
    use crate::chat::action::Action;
    use crate::chat::state::ChatState;

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let (binding, fixture_cleanup) =
        admit_owned_start_turn_fixture(&deps, "draft-fixB-B5", "stream the fixture reply", false)
            .await
            .expect("Q42 genuine admission prerequisite");
    let executor = EffectExecutor::new_with_deps(deps);

    // ── start the driver ──
    executor
        .execute(Effect::StartTurn {
            provider_turn_task_id: None,
            draft_id: "draft-fixB-B5".to_string(),
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

    // ── collect the Actions fed back by the driver and feed them to the reducer ──
    let mut state = ChatState::new(
        Arc::from("test-prov"),
        Arc::from("test-model"),
        CancellationToken::new(),
    );
    state.session.id = "sess-fixB-B5".to_string();
    // the user turn is dispatched by the chat::run main loop before the driver starts, so add it by hand here.
    let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "draft-fixB-B5".to_string(),
        cancel: CancellationToken::new(),
    });

    let mut saw_record = false;
    let mut save_snapshot: Option<crate::chat::session::ChatSession> = None;
    for _ in 0..8 {
        let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("driver action within 1.5s")
            .expect("action received");
        match action {
            Action::RecordAssistantTurn { content: text, .. } => {
                assert!(!saw_record, "RecordAssistantTurn must be sent only once");
                saw_record = true;
                let _ = state.reduce(Action::RecordAssistantTurn {
                    task_id: None,
                    content: text,
                });
            }
            Action::StreamCompleted {
                draft_id,
                final_text,
                reasoning,
            } => {
                assert!(
                    saw_record,
                    "B5 ordering contract: StreamCompleted must come after RecordAssistantTurn"
                );
                let effects = state.reduce(Action::StreamCompleted {
                    draft_id,
                    final_text,
                    reasoning,
                });
                for e in effects {
                    if let Effect::SaveSession(session) = e {
                        save_snapshot = Some(session);
                        break;
                    }
                }
                break;
            }
            _ => {
                // other actions (StreamChunkReceived etc.) do not affect the ordering contract check.
            }
        }
    }

    assert!(saw_record, "the driver must send RecordAssistantTurn");
    let snap = save_snapshot.expect("StreamCompleted must emit SaveSession");
    let last = snap.turns.last().expect("SaveSession.turns must not be empty");
    assert_eq!(
        last.role, "assistant",
        "the last snapshot entry must be this turn's assistant"
    );
    // double-write guard: the reducer records RecordAssistantTurn once; the duplicate dispatch was removed.
    assert_eq!(
        snap.turns.len(),
        2,
        "turns.len() must be strictly 2 (user+assistant, zero double write)"
    );
    finish_owned_start_turn_fixture(fixture_cleanup).await;
}

/// T3-3-fixB D1: SaveSession completion (disk write end) must be strictly before RequestRedraw (refresh).
///
/// With the original spawn version the ordering of the SaveSession subtask and RequestRedraw could not be
/// guaranteed; after the inline await fix the main loop's executor.execute(effect).await is serial throughout.
///
/// How it is verified: SlowMemory.store sleeps 20ms and then pushes "save_end",
/// MockRedrawRx pushes "redraw" on try_send, and we assert the log index of "save_end" < that of "redraw".
/// Repeated N=5 times to rule out luck (the spawn version sometimes got lucky; several rounds expose it).
#[tokio::test]
async fn t3_3_fix_b_effect_save_then_redraw_strict_order() {
    use crate::memory::MemoryEntry;
    use parking_lot::Mutex;

    struct SlowMemory {
        log: Arc<Mutex<Vec<&'static str>>>,
    }
    #[async_trait::async_trait]
    impl Memory for SlowMemory {
        fn name(&self) -> &str {
            "slow"
        }
        async fn store(&self, _k: &str, _c: &str, _cat: MemoryCategory, _s: Option<&str>) -> anyhow::Result<()> {
            self.log.lock().push("save_start");
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.log.lock().push("save_end");
            Ok(())
        }
        async fn recall(&self, _q: &str, _l: usize, _s: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn get(&self, _k: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }
        async fn list(&self, _c: Option<&MemoryCategory>, _s: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn forget(&self, _k: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    for trial in 0..5 {
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let memory: Arc<dyn Memory> = Arc::new(SlowMemory { log: Arc::clone(&log) });
        let shutdown = CancellationToken::new();

        // build deps separately: SlowMemory + a custom redraw_tx that intercepts the try_send order.
        let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
        let (hooks, _temp) = build_hook_manager();
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
        let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
        let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);

        // a listener task on redraw_rx pushes "redraw" into the log.
        let log_for_redraw = Arc::clone(&log);
        let redraw_listener = tokio::spawn(async move {
            if redraw_rx.recv().await.is_some() {
                log_for_redraw.lock().push("redraw");
            }
        });

        let deps = EffectDeps {
            provider,
            memory,
            memory_event_recording: MemoryEventRecording::default(),
            channel,
            hooks,
            observer,
            action_tx,
            provider_turn_lifecycle_tx: None,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: Some(redraw_tx),
            #[cfg(feature = "terminal-tui")]
            tui_mirror: None,
            shutdown: shutdown.clone(),
            model: ModelSlot::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            approval_router: Arc::new(ApprovalRouter::new()),
            tool_security_policy: full_tool_security_policy(),
            tool_tiering: crate::config::ToolTieringConfig::default(),
        };
        let executor = EffectExecutor::new_with_deps(deps);

        let session = ChatSession::new("prov", "model");
        // serial main loop: SaveSession → RequestRedraw (the reducer's actual order).
        executor.execute(Effect::SaveSession(session)).await;
        executor.execute(Effect::RequestRedraw).await;

        // wait for redraw_listener to finish (it exits after receiving redraw).
        let _ = tokio::time::timeout(Duration::from_millis(500), redraw_listener).await;

        let snap = log.lock().clone();
        let save_end_idx = snap
            .iter()
            .position(|&s| s == "save_end")
            .unwrap_or_else(|| panic!("[trial {trial}] save_end did not appear: log={snap:?}"));
        let redraw_idx = snap
            .iter()
            .position(|&s| s == "redraw")
            .unwrap_or_else(|| panic!("[trial {trial}] redraw did not appear: log={snap:?}"));
        assert!(
            save_end_idx < redraw_idx,
            "[trial {trial}] D1 order: save_end ({save_end_idx}) must precede redraw ({redraw_idx}); {snap:?}"
        );
    }
}

#[tokio::test]
async fn real_mode_quit_cancels_shutdown_token() {
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
    let executor = EffectExecutor::new_with_deps(deps);

    assert!(!shutdown.is_cancelled());
    executor.execute(Effect::Quit).await;
    assert!(shutdown.is_cancelled(), "Effect::Quit must cancel shutdown token");
}

#[tokio::test]
async fn real_mode_request_redraw_pings_renderer() {
    // RequestRedraw wakes the main loop through deps.redraw_tx in real mode.
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
    let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
    let (hooks, _temp) = build_hook_manager();
    let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
    let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
    let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);
    let deps = EffectDeps {
        provider,
        memory,
        memory_event_recording: MemoryEventRecording::default(),
        channel,
        hooks,
        observer,
        action_tx,
        provider_turn_lifecycle_tx: None,
        dual_write_guard: RuntimeDualWriteGuard::new(),
        redraw_tx: Some(redraw_tx),
        #[cfg(feature = "terminal-tui")]
        tui_mirror: None,
        shutdown: shutdown.clone(),
        model: ModelSlot::from("test-model"),
        temperature: 0.0,
        tools_registry: None,
        approval_router: Arc::new(ApprovalRouter::new()),
        tool_security_policy: full_tool_security_policy(),
        tool_tiering: crate::config::ToolTieringConfig::default(),
    };
    let executor = EffectExecutor::new_with_deps(deps);
    executor.execute(Effect::RequestRedraw).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), redraw_rx.recv())
            .await
            .expect("redraw within 200ms")
            .is_some(),
        "RequestRedraw should ping the redraw channel"
    );
}

/// With a renderer attached the notice is already in the reducer's
/// transcript ledger, so the effect owes it a frame.
#[tokio::test]
async fn surface_notice_pings_renderer_when_one_is_attached() {
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);
    deps.redraw_tx = Some(redraw_tx);
    let executor = EffectExecutor::new_with_deps(deps);

    executor
        .execute(Effect::SurfaceNotice {
            text: "Context trimmed (lossy): 3 older messages dropped.".to_string(),
        })
        .await;

    assert!(
        tokio::time::timeout(Duration::from_millis(200), redraw_rx.recv())
            .await
            .expect("redraw within 200ms")
            .is_some(),
        "SurfaceNotice must ping the renderer so the line is drawn"
    );
}

#[cfg(feature = "terminal-tui")]
#[tokio::test]
async fn request_approval_in_tui_opens_surface_without_auto_send() {
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let mirror = Arc::new(ParkingMutex::new(crate::chat::tui::TuiState::new("p", "m")));
    deps.tui_mirror = Some(Arc::clone(&mirror));
    let executor = EffectExecutor::new_with_deps(deps);

    executor
        .execute(Effect::RequestApproval {
            task_id: None,
            tool_id: "call-tui".to_string(),
            name: "shell".to_string(),
            args: r#"{"cmd":"echo secure"}"#.to_string(),
        })
        .await;

    assert!(
        tokio::time::timeout(Duration::from_millis(50), action_rx.recv())
            .await
            .is_err(),
        "TUI RequestApproval must not auto-send ToolApprovalReceived"
    );
    let state = mirror.lock();
    let pending = state
        .pending_tool_approval
        .as_ref()
        .expect("approval surface should be visible");
    assert_eq!(pending.tool_id, "call-tui");
    assert_eq!(pending.name, "shell");
    assert!(pending.args.contains("echo secure"));
    assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Approval);
}

#[cfg(feature = "terminal-tui")]
#[test]
fn request_approval_in_tui_ignores_openprx_override_parent() {
    let exe = std::env::current_exe().expect("current test exe");
    let status = std::process::Command::new(exe)
        .arg("--exact")
        .arg("chat::dispatcher::tests::request_approval_in_tui_ignores_openprx_override_child")
        .arg("--nocapture")
        .env("OPENPRX_APPROVAL_OVERRIDE", "allow")
        .env("ISS018_OVERRIDE_CHILD", "1")
        .status()
        .expect("run child override test");
    assert!(status.success(), "child override test failed: {status}");
}

#[cfg(feature = "terminal-tui")]
#[tokio::test]
async fn request_approval_in_tui_ignores_openprx_override_child() {
    if std::env::var_os("ISS018_OVERRIDE_CHILD").is_none() {
        return;
    }
    assert_eq!(
        std::env::var("OPENPRX_APPROVAL_OVERRIDE").as_deref(),
        Ok("allow"),
        "child test must exercise the env override condition"
    );

    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let mirror = Arc::new(ParkingMutex::new(crate::chat::tui::TuiState::new("p", "m")));
    deps.tui_mirror = Some(Arc::clone(&mirror));
    let executor = EffectExecutor::new_with_deps(deps);

    executor
        .execute(Effect::RequestApproval {
            task_id: None,
            tool_id: "call-tui-override".to_string(),
            name: "shell".to_string(),
            args: r#"{"cmd":"echo secure"}"#.to_string(),
        })
        .await;

    assert!(
        tokio::time::timeout(Duration::from_millis(50), action_rx.recv())
            .await
            .is_err(),
        "TUI RequestApproval must ignore OPENPRX_APPROVAL_OVERRIDE and wait for human input"
    );
    let state = mirror.lock();
    assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Approval);
    assert_eq!(
        state
            .pending_tool_approval
            .as_ref()
            .map(|pending| pending.tool_id.as_str()),
        Some("call-tui-override")
    );
}

/// P1-1: EmitChannelMessage → channel.send is really called.
#[tokio::test]
async fn real_mode_emit_channel_message_triggers_channel_send() {
    let send_count = Arc::new(AtomicUsize::new(0));
    let finalize_count = Arc::new(AtomicUsize::new(0));
    let shutdown = CancellationToken::new();
    let (deps, _rx, _temp) =
        build_counting_channel_deps(Arc::clone(&send_count), Arc::clone(&finalize_count), shutdown);
    let executor = EffectExecutor::new_with_deps(deps.clone());

    use crate::channels::traits::SendMessage;
    let msg = SendMessage::new("hello from effect".to_string(), "user");
    executor.execute(Effect::EmitChannelMessage(msg)).await;

    let final_count = wait_for_count(&send_count, 1, Duration::from_secs(2)).await;
    assert_eq!(
        final_count, 1,
        "channel.send should be called exactly once for EmitChannelMessage"
    );
    // RAII scope: the guard must reset automatically once the subtask finishes (it must not stick).
    assert!(
        !deps.dual_write_guard.is_active(),
        "dual_write_guard should auto-clear after EmitChannelMessage completes (RAII scope)"
    );
}

/// P1-2: PersistToMemory → memory.store is really called (using the existing CountingMemory).
#[tokio::test]
async fn real_mode_persist_to_memory_triggers_memory_store() {
    let store_count = Arc::new(AtomicUsize::new(0));
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(CountingMemory {
        inner: crate::memory::NoneMemory::new(),
        store_count: Arc::clone(&store_count),
    });
    let shutdown = CancellationToken::new();
    let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
    let executor = EffectExecutor::new_with_deps(deps.clone());

    executor
        .execute(Effect::PersistToMemory {
            key: "persist-test-key".to_string(),
            value: "test-value".to_string(),
            category: crate::memory::MemoryCategory::Conversation,
        })
        .await;

    let final_count = wait_for_count(&store_count, 1, Duration::from_secs(2)).await;
    assert_eq!(
        final_count, 1,
        "memory.store should be called exactly once for PersistToMemory"
    );
    // RAII scope: the guard must reset automatically once the subtask finishes (it must not stick).
    assert!(
        !deps.dual_write_guard.is_active(),
        "dual_write_guard should auto-clear after PersistToMemory completes (RAII scope)"
    );
}

/// P1-3a: NotifyHook → no panic, and the RAII scope makes sure the guard resets after the subtask finishes.
///
/// HookManager is not a trait so it cannot be wrapped for counting; behavioural check:
/// the guard clearing itself after emit means the executor took the real path and RAII did not stick.
/// HookManager has no registered hooks → emit is a fast no-op and does not slow the test down.
#[tokio::test]
async fn real_mode_notify_hook_guard_does_not_stick() {
    use crate::hooks::HookEvent;
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
    let executor = EffectExecutor::new_with_deps(deps.clone());

    assert!(!deps.dual_write_guard.is_active(), "guard should start inactive");

    executor
        .execute(Effect::NotifyHook {
            event: HookEvent::TurnComplete,
            payload: serde_json::json!({"test": "notify-hook"}),
        })
        .await;

    // the spawned subtask is async; give it enough time to finish
    tokio::time::sleep(Duration::from_millis(200)).await;

    // RAII scope: the guard must reset automatically once the subtask finishes (it must not stick).
    assert!(
        !deps.dual_write_guard.is_active(),
        "dual_write_guard must auto-clear after NotifyHook completes (RAII scope prevents sticking)"
    );
}

/// P1-3b: NotifyHook → hooks.emit is really called (a real hook in a temp dir touches a sentinel file).
///
/// HookManager is not a trait and cannot be mocked. Instead we register a real hook:
/// write hooks.json in a temp directory registering a turn_complete event that runs `touch <sentinel>`,
/// and after emit the existence of the sentinel file proves emit really ran the hook action.
#[tokio::test]
async fn real_mode_notify_hook_triggers_emit() {
    use crate::hooks::HookEvent;

    // build a temp directory + register a real hook (touch a sentinel file)
    let temp = TempDir::new().expect("tempdir");
    let sentinel = temp.path().join("hook_was_called");
    let sentinel_str = sentinel.to_str().expect("valid path");

    let hooks_json = serde_json::json!({
        "enabled": true,
        "hooks": {
            "turn_complete": [
                {
                    "command": "touch",
                    "args": [sentinel_str],
                    "stdin_json": false
                }
            ]
        }
    });
    std::fs::write(temp.path().join("hooks.json"), hooks_json.to_string()).expect("write hooks.json");

    // The operator opted in to workspace hooks (HOOKS-ENV).
    let hooks =
        Arc::new(HookManager::new(temp.path().to_path_buf()).allowing_workspace_hooks(temp.path().join("hook-launch")));
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
    let provider: Arc<dyn crate::providers::Provider> = Arc::new(crate::providers::router::MockEnvProvider::from_env());
    let channel: Arc<dyn crate::channels::Channel> = Arc::new(crate::channels::TerminalChannel::new(true));
    let observer: Arc<dyn crate::observability::Observer> = Arc::new(crate::observability::NoopObserver);
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
            payload: serde_json::json!({"turn": "test"}),
        })
        .await;

    // The hook is delivered by the background dispatcher; wait for the
    // delivery signal instead of a fixed delay.
    tokio::time::timeout(Duration::from_secs(30), hooks.wait_delivered(1))
        .await
        .expect("the hook delivery finishes");

    assert!(
        sentinel.exists(),
        "hooks.emit should have executed 'touch {sentinel_str}' — sentinel file not found, emit was not called"
    );
}

/// P1-4a: SendDraftFinalize → no panic, no blocking, and the RAII guard does not stick.
///
/// Checks: (1) non-blocking (returns immediately) (2) the guard resets after the subtask finishes (no sticking)
/// (3) channel.finalize_draft is really called (finalize_count == 1).
#[tokio::test]
async fn real_mode_send_draft_finalize_triggers_channel_finalize() {
    let send_count = Arc::new(AtomicUsize::new(0));
    let finalize_count = Arc::new(AtomicUsize::new(0));
    let shutdown = CancellationToken::new();
    let (deps, _rx, _temp) =
        build_counting_channel_deps(Arc::clone(&send_count), Arc::clone(&finalize_count), shutdown);
    let executor = EffectExecutor::new_with_deps(deps.clone());

    let start = std::time::Instant::now();
    executor
        .execute(Effect::SendDraftFinalize {
            draft_id: "draft-finalize-test".to_string(),
            text: "final response text".to_string(),
        })
        .await;
    // non-blocking: returns immediately after spawning
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "SendDraftFinalize should not block (Codex P0-1)"
    );

    // wait for the subtask to finish
    tokio::time::sleep(Duration::from_millis(200)).await;

    // channel.finalize_draft is really called
    assert_eq!(
        finalize_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "channel.finalize_draft should be called exactly once for SendDraftFinalize"
    );
    // RAII scope: the guard resets automatically after the subtask finishes (no sticking)
    assert!(
        !deps.dual_write_guard.is_active(),
        "dual_write_guard must auto-clear after SendDraftFinalize completes (RAII scope)"
    );
}

/// P1-5: DisplayMedia → no panic, takes the trace/debug path (in 5a-1 the legacy path drives media display).
#[tokio::test]
async fn real_mode_display_media_does_not_panic() {
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
    let executor = EffectExecutor::new_with_deps(deps);

    // no panic = the path is clear; in stage 5a-1 there is only a debug log and no external side effect
    executor
        .execute(Effect::DisplayMedia {
            kind: "IMAGE".to_string(),
            path: "/tmp/test_image.png".to_string(),
        })
        .await;
    // passing = no panic
}

/// P1-6: AutoTitleSession → no panic, takes the debug trace path.
#[tokio::test]
async fn real_mode_auto_title_session_does_not_panic() {
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
    let executor = EffectExecutor::new_with_deps(deps);

    executor
        .execute(Effect::AutoTitleSession("session-title-test".to_string()))
        .await;
    // passing = no panic; in stage 5a-1 AutoTitleSession only writes a debug log
}

/// P1-7: LogTrace in real mode — verify no tracing::Level panics (real mode uses the same path as shadow).
#[tokio::test]
async fn real_mode_log_trace_all_levels_do_not_panic() {
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
    let shutdown = CancellationToken::new();
    let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
    let executor = EffectExecutor::new_with_deps(deps);

    let levels = [
        tracing::Level::ERROR,
        tracing::Level::WARN,
        tracing::Level::INFO,
        tracing::Level::DEBUG,
        tracing::Level::TRACE,
    ];
    for level in levels {
        executor
            .execute(Effect::LogTrace {
                level,
                msg: format!("real-mode log test at {level}"),
            })
            .await;
    }
    // all passing = no panic; in real mode LogTrace takes the same emit_trace path as shadow
}

/// P0-2 check: after injecting redraw_tx through the redraw_handle Arc, RequestRedraw really triggers a redraw.
///
/// Simulates the chat::run scenario: first build the EffectExecutor (redraw_tx=None),
/// take the redraw_handle, "spawn" (executed directly here), then fill in redraw_tx,
/// and verify the RequestRedraw effect really triggers a redraw.
#[tokio::test]
async fn redraw_handle_injection_enables_request_redraw() {
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
    let shutdown = CancellationToken::new();
    // at construction deps.redraw_tx = Some (build_deps defaults to Some); we use None to simulate the race
    let provider: Arc<dyn crate::providers::Provider> = Arc::new(crate::providers::router::MockEnvProvider::from_env());
    let channel: Arc<dyn crate::channels::Channel> = Arc::new(crate::channels::TerminalChannel::new(true));
    let (hooks, _temp) = build_hook_manager();
    let observer: Arc<dyn crate::observability::Observer> = Arc::new(crate::observability::NoopObserver);
    let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
    let deps = EffectDeps {
        provider,
        memory,
        memory_event_recording: MemoryEventRecording::default(),
        channel,
        hooks,
        observer,
        action_tx,
        provider_turn_lifecycle_tx: None,
        dual_write_guard: RuntimeDualWriteGuard::new(),
        redraw_tx: None, // simulate redraw_tx not existing yet at construction
        #[cfg(feature = "terminal-tui")]
        tui_mirror: None,
        shutdown: shutdown.clone(),
        model: ModelSlot::from("test-model"),
        temperature: 0.0,
        tools_registry: None,
        approval_router: Arc::new(ApprovalRouter::new()),
        tool_security_policy: full_tool_security_policy(),
        tool_tiering: crate::config::ToolTieringConfig::default(),
    };

    let executor = EffectExecutor::new_with_deps(deps);

    // take the redraw_handle (simulating chat::run saving the Arc early)
    let redraw_slot = executor.redraw_handle();

    // RequestRedraw before injection — slot is None, should be no-op (no panic)
    executor.execute(Effect::RequestRedraw).await;

    // inject redraw_tx afterwards (simulating injection once TUI init completes)
    let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);
    *redraw_slot.lock() = Some(redraw_tx);

    // after injection RequestRedraw must really fire
    executor.execute(Effect::RequestRedraw).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), redraw_rx.recv())
            .await
            .expect("redraw within 200ms after injection")
            .is_some(),
        "RequestRedraw should trigger after redraw_handle injection (P0-2)"
    );
}
