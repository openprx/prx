#![cfg(test)]

use super::*;

#[tokio::test]
async fn dual_write_guard_default_is_inactive() {
    let g = RuntimeDualWriteGuard::new();
    assert!(!g.is_active());
    let scope = g.enter_scope();
    assert!(g.is_active());
    assert_eq!(g.active_count(), 1);
    drop(scope);
    assert!(!g.is_active());
    assert_eq!(g.active_count(), 0);
}

#[tokio::test]
async fn dual_write_guard_clone_shares_state() {
    let g1 = RuntimeDualWriteGuard::new();
    let g2 = g1.clone();
    assert!(!g2.is_active());
    let _scope = g1.enter_scope();
    assert!(g2.is_active(), "clone shares the same Arc<AtomicU64>");
}

/// 5a-5 Codex P1 fix verification: when several RAII scopes exist concurrently,
/// dropping one scope must not make the guard inactive; it only returns to 0 after every scope drops.
/// The old AtomicBool implementation had a serious timing window: an earlier drop also cleared later ones.
#[tokio::test]
async fn dual_write_guard_counting_raii_prevents_early_release() {
    let g = RuntimeDualWriteGuard::new();
    let s1 = g.enter_scope();
    assert!(g.is_active());
    assert_eq!(g.active_count(), 1);
    let s2 = g.enter_scope();
    assert_eq!(g.active_count(), 2);
    let s3 = g.enter_scope();
    assert_eq!(g.active_count(), 3);

    // drop one in the middle; the guard stays active.
    drop(s2);
    assert!(
        g.is_active(),
        "guard must remain active while sibling scopes are alive (5a-5 P1 fix)"
    );
    assert_eq!(g.active_count(), 2);

    drop(s1);
    assert!(g.is_active(), "guard remains active with last scope alive");
    assert_eq!(g.active_count(), 1);

    drop(s3);
    assert!(!g.is_active(), "guard becomes inactive only after all scopes drop");
    assert_eq!(g.active_count(), 0);
}

/// Ratatui path Ctrl+C regression guard (reduced to ChatState + a double Ctrl+C dispatch verifying Effect::Quit).
///
/// **Background**: Codex P1 pointed out that the PTY test uses `PRX_TUI=0` and goes through reedline, so it
/// does not cover the Ctrl+C branch of `run_tui_unified_loop`. Capturing full-screen ratatui TUI output
/// from a PTY is hard, so this is reduced to a reducer + executor unit test:
///   - build a ChatState simulating a double Ctrl+C in DOUBLE_CTRLC_WINDOW_MS on the ratatui path
///   - verify the reducer returns Effect::Quit
///   - feed Effect::Quit to a real-mode EffectExecutor and verify shutdown.cancel() is called
/// This is the minimal verifiable subset of the end-to-end "Ctrl+C → exit" chain, covering the core
/// defensive path of the round 2 hang bug (reducer decision + executor trigger).
#[cfg(feature = "terminal-tui")]
#[tokio::test]
async fn ratatui_path_double_ctrlc_exits_via_reducer_and_executor() {
    use crate::chat::state::ChatState;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let shutdown = CancellationToken::new();
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), shutdown.clone());
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

    // first Ctrl+C @ t=1000ms — must not trigger Quit (it only records the window; the reducer requires prev != 0)
    let effects1 = state.reduce_with_now(Action::KeyPressed(ctrl_c), 1000);
    let has_quit_1 = effects1.iter().any(|e| matches!(e, Effect::Quit));
    assert!(!has_quit_1, "first Ctrl+C should not Quit");

    // second Ctrl+C @ t=1200ms — inside the 500ms window (200ms apart), so it must Quit
    let effects2 = state.reduce_with_now(Action::KeyPressed(ctrl_c), 1200);
    let has_quit_2 = effects2.iter().any(|e| matches!(e, Effect::Quit));
    assert!(has_quit_2, "double Ctrl+C within 500ms must Quit");

    // feed it to a real-mode EffectExecutor and verify shutdown.cancel() really runs
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
    let executor = EffectExecutor::new_with_deps(deps);
    for e in effects2 {
        executor.execute(e).await;
    }
    assert!(
        shutdown.is_cancelled(),
        "real-mode executor must propagate Effect::Quit to shutdown.cancel()"
    );
}

/// Extra regression guard: a single Ctrl+C during an in-flight turn must not exit (it only cancels the turn).
#[cfg(feature = "terminal-tui")]
#[tokio::test]
async fn single_ctrlc_during_turn_does_not_exit() {
    use crate::chat::state::ChatState;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let shutdown = CancellationToken::new();
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), shutdown.clone());
    // simulate a turn in progress (generating=true) — set through the TurnStarted action
    let cancel = CancellationToken::new();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d1".to_string(),
        cancel: cancel.clone(),
    });
    assert!(state.control.generating);

    // single Ctrl+C — while generating it must cancel the draft, not exit.
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let effects = state.reduce_with_now(Action::KeyPressed(ctrl_c), 1000);
    let has_quit = effects.iter().any(|e| matches!(e, Effect::Quit));
    assert!(!has_quit, "single Ctrl+C in flight turn must not Quit");

    // shutdown must not be cancelled
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
    let executor = EffectExecutor::new_with_deps(deps);
    for e in effects {
        executor.execute(e).await;
    }
    assert!(!shutdown.is_cancelled(), "single Ctrl+C must not cancel shutdown");
}

/// Regression guard: the Mutex test avoids ".unwrap()" — parking_lot is mandatory.
#[tokio::test]
async fn parking_lot_mutex_in_test() {
    let m: Mutex<u32> = Mutex::new(0);
    *m.lock() = 42;
    assert_eq!(*m.lock(), 42);
}

// ─── P0: dedicated DualWriteGuardScope RAII unit tests ───────────────────

/// P0-scope-1: DualWriteGuardScope::enter → guard true; Drop → guard false.
#[tokio::test]
async fn dual_write_guard_scope_clears_on_drop() {
    let guard = RuntimeDualWriteGuard::new();
    assert!(!guard.is_active(), "guard should start false");

    {
        let _scope = guard.enter_scope();
        assert!(guard.is_active(), "guard should be true while scope is held");
    } // scope drops here

    assert!(
        !guard.is_active(),
        "guard should be false after scope drop (RAII cleared)"
    );
}

/// P0-scope-2: DualWriteGuardScope::drop still runs on a panic path (unwind safety).
#[tokio::test]
async fn dual_write_guard_scope_panic_safe() {
    let guard = RuntimeDualWriteGuard::new();
    let inner = Arc::clone(&guard.active);

    let result = std::panic::catch_unwind(move || {
        let scope = DualWriteGuardScope::enter(Arc::clone(&inner));
        assert!(
            inner.load(Ordering::Acquire) > 0,
            "count should be positive inside scope"
        );
        // panic on purpose; drop must run during unwind
        let _keep = scope;
        panic!("deliberate test panic");
    });

    assert!(result.is_err(), "catch_unwind should have caught the panic");
    // Drop runs after the panic → the guard must reset to false
    assert!(
        !guard.is_active(),
        "guard must be false after panic unwind (Drop still runs)"
    );
}

/// P0-scope-3: after real_mode SaveSession completes the guard does not stick (the spawn scope clears it).
///
/// More focused on the guard lifetime than real_mode_save_session_triggers_memory_store:
/// the guard is briefly true after execute() and resets to false once the subtask finishes.
#[tokio::test]
async fn real_mode_save_session_clears_guard_after_completion() {
    let store_count = Arc::new(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
        inner: NoneMemory::new(),
        store_count: Arc::clone(&store_count),
    });
    let shutdown = CancellationToken::new();
    let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
    let executor = EffectExecutor::new_with_deps(deps.clone());

    let session = crate::chat::session::ChatSession::new("prov", "model");
    executor.execute(Effect::SaveSession(session)).await;

    // poll until the subtask finishes (at most 500ms)
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        if store_count.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
            break;
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "memory.store not called within 500ms"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // the guard must reset automatically once the subtask finishes
    assert!(
        !deps.dual_write_guard.is_active(),
        "dual_write_guard must be false after SaveSession subtask completes (RAII scope auto-cleared)"
    );
}
