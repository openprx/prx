#![cfg(test)]

//! Tests for P3-2: `TerminalGuard` RAII + strengthened panic hook.
//!
//! These tests intentionally do NOT call `TerminalGuard::enter()` because
//! the test harness is not connected to a real TTY — `enable_raw_mode`
//! would fail on most CI runners. Instead we exercise:
//!
//!   * `leave()` idempotency on a guard constructed in the "inactive"
//!     state (no crossterm calls issued).
//!   * Concurrent `leave()` from multiple threads (Drop + manual).
//!   * `restore_terminal_state()` does not panic when invoked outside
//!     raw mode (the panic-hook fast path).
use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct FakeTerminalModeOps {
    calls: Vec<&'static str>,
    fail_enable_bracketed_paste: bool,
    fail_enable_mouse_capture: bool,
    keyboard_enhancement_supported: bool,
    fail_push_keyboard_enhancement: bool,
}

impl TerminalModeOps for FakeTerminalModeOps {
    fn enable_raw_mode(&mut self) -> std::io::Result<()> {
        self.calls.push("enable_raw_mode");
        Ok(())
    }

    fn disable_raw_mode(&mut self) -> std::io::Result<()> {
        self.calls.push("disable_raw_mode");
        Ok(())
    }

    fn supports_keyboard_enhancement(&mut self) -> std::io::Result<bool> {
        self.calls.push("supports_keyboard_enhancement");
        Ok(self.keyboard_enhancement_supported)
    }

    fn push_keyboard_enhancement_flags(&mut self) -> std::io::Result<()> {
        self.calls.push("push_keyboard_enhancement_flags");
        if self.fail_push_keyboard_enhancement {
            Err(std::io::Error::other("push keyboard enhancement failed"))
        } else {
            Ok(())
        }
    }

    fn pop_keyboard_enhancement_flags(&mut self) -> std::io::Result<()> {
        self.calls.push("pop_keyboard_enhancement_flags");
        Ok(())
    }

    fn enable_bracketed_paste(&mut self) -> std::io::Result<()> {
        self.calls.push("enable_bracketed_paste");
        if self.fail_enable_bracketed_paste {
            Err(std::io::Error::other("enable bracketed paste failed"))
        } else {
            Ok(())
        }
    }

    fn disable_bracketed_paste(&mut self) -> std::io::Result<()> {
        self.calls.push("disable_bracketed_paste");
        Ok(())
    }

    fn enable_mouse_capture(&mut self) -> std::io::Result<()> {
        self.calls.push("enable_mouse_capture");
        if self.fail_enable_mouse_capture {
            Err(std::io::Error::other("enable mouse capture failed"))
        } else {
            Ok(())
        }
    }

    fn disable_mouse_capture(&mut self) -> std::io::Result<()> {
        self.calls.push("disable_mouse_capture");
        Ok(())
    }

    fn enter_alternate_screen(&mut self) -> std::io::Result<()> {
        self.calls.push("enter_alternate_screen");
        Ok(())
    }

    fn leave_alternate_screen(&mut self) -> std::io::Result<()> {
        self.calls.push("leave_alternate_screen");
        Ok(())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.calls.push("show_cursor");
        Ok(())
    }
}

/// Build a `TerminalGuard` in the inactive state (no real terminal
/// mutation), suitable for unit-testing the bookkeeping.
fn inactive_guard() -> TerminalGuard {
    TerminalGuard {
        raw_mode_active: AtomicBool::new(false),
        bracketed_paste_active: AtomicBool::new(false),
        keyboard_enhancement_active: AtomicBool::new(false),
        mouse_capture_active: AtomicBool::new(false),
        alternate_screen_active: AtomicBool::new(false),
    }
}

/// Build a "fake-active" guard whose flags are set but no real terminal
/// state was touched. `leave()` will issue crossterm calls but they
/// no-op safely on a non-TTY test harness (raw mode was never on, so
/// `disable_raw_mode` is a cheap kernel call returning Ok or Err — both
/// are swallowed).
fn fake_active_guard() -> TerminalGuard {
    TerminalGuard {
        raw_mode_active: AtomicBool::new(true),
        bracketed_paste_active: AtomicBool::new(true),
        keyboard_enhancement_active: AtomicBool::new(true),
        mouse_capture_active: AtomicBool::new(true),
        alternate_screen_active: AtomicBool::new(true),
    }
}

#[test]
fn keyboard_enhancements_preserve_plain_utf8_for_ime() {
    use crossterm::event::KeyboardEnhancementFlags;

    let flags = chat_keyboard_enhancement_flags();
    assert!(flags.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    assert!(flags.contains(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS));
    assert!(flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES));
    assert!(
        !flags.contains(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES),
        "REPORT_ALL requires associated-text support or IME text is lost"
    );
}

#[test]
fn leave_is_idempotent_on_inactive_guard() {
    let guard = inactive_guard();
    // Multiple calls must not panic and must not flip the flags.
    guard.leave();
    guard.leave();
    guard.leave();
    assert!(!guard.raw_mode_active.load(Ordering::Acquire));
    assert!(!guard.bracketed_paste_active.load(Ordering::Acquire));
    assert!(!guard.keyboard_enhancement_active.load(Ordering::Acquire));
    assert!(!guard.mouse_capture_active.load(Ordering::Acquire));
    assert!(!guard.alternate_screen_active.load(Ordering::Acquire));
}

#[test]
fn leave_flips_flags_exactly_once() {
    let guard = fake_active_guard();
    assert!(guard.raw_mode_active.load(Ordering::Acquire));
    assert!(guard.bracketed_paste_active.load(Ordering::Acquire));
    assert!(guard.mouse_capture_active.load(Ordering::Acquire));
    assert!(guard.alternate_screen_active.load(Ordering::Acquire));
    guard.leave();
    assert!(!guard.raw_mode_active.load(Ordering::Acquire));
    assert!(!guard.bracketed_paste_active.load(Ordering::Acquire));
    assert!(!guard.keyboard_enhancement_active.load(Ordering::Acquire));
    assert!(!guard.mouse_capture_active.load(Ordering::Acquire));
    assert!(!guard.alternate_screen_active.load(Ordering::Acquire));
    // Second leave is a no-op (CAS fails, no crossterm calls).
    guard.leave();
    assert!(!guard.raw_mode_active.load(Ordering::Acquire));
    assert!(!guard.keyboard_enhancement_active.load(Ordering::Acquire));
    assert!(!guard.bracketed_paste_active.load(Ordering::Acquire));
    assert!(!guard.mouse_capture_active.load(Ordering::Acquire));
    assert!(!guard.alternate_screen_active.load(Ordering::Acquire));
}

#[cfg(feature = "terminal-tui")]
#[test]
fn plain_mode_disables_terminal_tui_even_for_tty() {
    assert!(
        !should_enable_terminal_tui(true, true, Some("1")),
        "--plain must bypass TUI even when PRX_TUI asks for TUI"
    );
    assert!(
        should_enable_terminal_tui(false, true, Some("1")),
        "TTY + PRX_TUI=1 should enable TUI when not plain"
    );
    assert!(
        !should_enable_terminal_tui(false, true, Some("0")),
        "PRX_TUI=0 remains an explicit TUI opt-out"
    );
    assert!(
        !should_enable_terminal_tui(false, false, Some("1")),
        "non-TTY stdin must not enter TUI"
    );
}

#[test]
fn fullscreen_terminal_lifecycle_enters_and_leaves_alternate_screen_in_order() {
    let mut ops = FakeTerminalModeOps::default();
    let state = enter_terminal_state_with_ops(&mut ops).unwrap();
    assert!(
        state.mouse_capture_active,
        "fullscreen enables transcript mouse scrolling and drag selection by default"
    );
    leave_terminal_state_with_ops(&mut ops, state);

    assert_eq!(
        ops.calls,
        vec![
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "enable_bracketed_paste",
            "disable_bracketed_paste",
            "show_cursor",
            "disable_mouse_capture",
            "leave_alternate_screen",
            "disable_raw_mode"
        ]
    );
}

#[test]
fn fullscreen_terminal_lifecycle_can_disable_mouse_capture() {
    let mut ops = FakeTerminalModeOps::default();
    let state = enter_terminal_state_with_ops_inner(&mut ops, false).unwrap();
    assert!(!state.mouse_capture_active);
    leave_terminal_state_with_ops(&mut ops, state);

    assert_eq!(
        ops.calls,
        vec![
            "enable_raw_mode",
            "enter_alternate_screen",
            "supports_keyboard_enhancement",
            "enable_bracketed_paste",
            "disable_bracketed_paste",
            "show_cursor",
            "leave_alternate_screen",
            "disable_raw_mode"
        ]
    );
}

#[test]
fn fullscreen_enter_rolls_back_alternate_screen_when_mouse_capture_fails() {
    let mut ops = FakeTerminalModeOps {
        fail_enable_mouse_capture: true,
        ..FakeTerminalModeOps::default()
    };

    let err = enter_terminal_state_with_ops_inner(&mut ops, true).unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::Other);
    assert_eq!(
        ops.calls,
        vec![
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_mouse_capture",
            "leave_alternate_screen",
            "disable_raw_mode"
        ]
    );
}

#[test]
fn fullscreen_enter_rolls_back_alternate_screen_when_bracketed_paste_fails() {
    let mut ops = FakeTerminalModeOps {
        fail_enable_bracketed_paste: true,
        ..FakeTerminalModeOps::default()
    };

    let err = enter_terminal_state_with_ops(&mut ops).unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::Other);
    assert_eq!(
        ops.calls,
        vec![
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "enable_bracketed_paste",
            "disable_mouse_capture",
            "leave_alternate_screen",
            "disable_raw_mode"
        ]
    );
}

#[test]
fn fullscreen_enter_rolls_back_when_keyboard_enhancement_push_fails() {
    let mut ops = FakeTerminalModeOps {
        keyboard_enhancement_supported: true,
        fail_push_keyboard_enhancement: true,
        ..FakeTerminalModeOps::default()
    };

    let err = enter_terminal_state_with_ops(&mut ops).unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::Other);
    assert!(!CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.load(Ordering::Acquire));
    assert!(
        !CHAT_FULLSCREEN_ACTIVE.load(Ordering::Acquire),
        "failed keyboard-enhancement push must roll back fullscreen active flag"
    );
    assert_eq!(
        ops.calls,
        vec![
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "push_keyboard_enhancement_flags",
            "disable_mouse_capture",
            "leave_alternate_screen",
            "disable_raw_mode"
        ]
    );
}

#[test]
fn fullscreen_terminal_lifecycle_pushes_and_pops_keyboard_enhancement_when_supported() {
    let mut ops = FakeTerminalModeOps {
        keyboard_enhancement_supported: true,
        ..FakeTerminalModeOps::default()
    };

    let state = enter_terminal_state_with_ops(&mut ops).unwrap();
    assert!(state.keyboard_enhancement_active);
    leave_terminal_state_with_ops(&mut ops, state);

    assert_eq!(
        ops.calls,
        vec![
            "enable_raw_mode",
            "enter_alternate_screen",
            "enable_mouse_capture",
            "supports_keyboard_enhancement",
            "push_keyboard_enhancement_flags",
            "enable_bracketed_paste",
            "disable_bracketed_paste",
            "show_cursor",
            "pop_keyboard_enhancement_flags",
            "disable_mouse_capture",
            "leave_alternate_screen",
            "disable_raw_mode"
        ]
    );
}

#[test]
fn panic_restore_leaves_alternate_screen_only_when_fullscreen_active() {
    let mut inline_ops = FakeTerminalModeOps::default();
    restore_terminal_state_with_ops(&mut inline_ops, false);
    assert_eq!(
        inline_ops.calls,
        vec!["disable_bracketed_paste", "show_cursor", "disable_raw_mode"]
    );

    let mut fullscreen_ops = FakeTerminalModeOps::default();
    restore_terminal_state_with_ops(&mut fullscreen_ops, true);
    assert_eq!(
        fullscreen_ops.calls,
        vec![
            "disable_bracketed_paste",
            "show_cursor",
            "leave_alternate_screen",
            "disable_raw_mode"
        ]
    );
}

#[cfg(feature = "terminal-tui")]
#[test]
fn mouse_wheel_scrolls_fullscreen_transcript_by_three_rows() {
    let mut scroll = tui::FullscreenTranscriptScroll::default();

    assert!(apply_fullscreen_mouse_scroll(
        crossterm::event::MouseEventKind::ScrollUp,
        &mut scroll
    ));
    assert_eq!(scroll.offset_from_bottom, MOUSE_WHEEL_TRANSCRIPT_ROWS);

    assert!(apply_fullscreen_mouse_scroll(
        crossterm::event::MouseEventKind::ScrollDown,
        &mut scroll
    ));
    assert_eq!(scroll.offset_from_bottom, 0);

    assert!(!apply_fullscreen_mouse_scroll(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        &mut scroll
    ));
    assert_eq!(scroll.offset_from_bottom, 0);
}

#[cfg(feature = "terminal-tui")]
#[test]
fn plain_mode_does_not_emit_context_budget_chrome() {
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Aggressive,
        reserve_tokens: 5,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 120,
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    };
    let history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("context pressure ".repeat(400)),
    ];
    let budget = crate::agent::context_budget::plan_context_budget(
        &history,
        &config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    assert!(budget.used_tokens > 0, "fixture must produce context usage");
    let terminal_tui_enabled = should_enable_terminal_tui(true, true, Some("1"));
    assert!(
        context_budget_status_for_tui(&history, &config, terminal_tui_enabled).is_none(),
        "--plain must not emit context budget chrome"
    );
    let status = context_budget_status_for_tui(&history, &config, true).expect("TUI context budget status");
    assert_eq!(status.used_context_tokens, budget.used_tokens);
    assert_eq!(status.max_context_tokens, budget.max_context_tokens);
}

#[test]
fn drop_after_manual_leave_is_safe() {
    // Drop must not double-restore — the AtomicBool CAS in leave()
    // ensures the cleanup runs at most once across leave() + drop().
    let guard = fake_active_guard();
    guard.leave();
    // Implicit drop here — must be a no-op (no panic, no extra calls).
}

#[test]
fn concurrent_leave_from_multiple_threads_is_safe() {
    // Simulate two threads racing to clean up the same guard
    // (e.g. manual `leave()` on the main thread + Drop on a panicking
    // background thread). Only one should win the CAS for each flag.
    let guard = Arc::new(fake_active_guard());
    let mut handles = Vec::new();
    for _ in 0..8 {
        let g = Arc::clone(&guard);
        handles.push(std::thread::spawn(move || {
            g.leave();
        }));
    }
    for h in handles {
        h.join().expect("test: worker thread should not panic");
    }
    // After the race both flags must be cleared exactly once.
    assert!(!guard.raw_mode_active.load(Ordering::Acquire));
    assert!(!guard.bracketed_paste_active.load(Ordering::Acquire));
    assert!(!guard.mouse_capture_active.load(Ordering::Acquire));
    assert!(!guard.alternate_screen_active.load(Ordering::Acquire));
}

#[test]
fn restore_terminal_state_is_safe_outside_raw_mode() {
    // The panic hook calls this from arbitrary terminal states. It must
    // never panic, even when raw mode was never enabled and we are not
    // inside an alternate screen.
    restore_terminal_state();
    // Calling twice in a row must also be safe (idempotent at the
    // crossterm level — both calls swallow errors).
    restore_terminal_state();
}

/// P3-6: the Resize branch in `run_tui_input_loop` only does
/// `redraw_tx.try_send(()).ok();`. This test pins the coalescing
/// contract of the cap=1 channel: multiple sends in a row must not
/// block, they collapse into a single pending wakeup, and the receive
/// side observes exactly one redraw signal until the next try_send
/// after the recv. If this contract ever changes, the Resize handler
/// would silently start blocking the input loop — the test fails
/// loudly instead.
#[cfg(feature = "terminal-tui")]
#[test]
fn resize_redraw_signal_coalesces() {
    use tokio::sync::mpsc;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: runtime builds");
    rt.block_on(async {
        let (tx, mut rx) = mpsc::channel::<()>(1);
        // Burst of resize-equivalent signals: first one fills the
        // buffer, the rest must fail with Full (non-blocking, no
        // panic). This mirrors a user dragging the window border.
        for _ in 0..16 {
            let _ = tx.try_send(());
        }
        // Exactly one wakeup observable.
        rx.recv().await.expect("test: receives first wakeup");
        // Channel must be drained now.
        assert!(rx.try_recv().is_err(), "expected coalescing to drain to one signal");
        // After drain, the channel must accept a fresh send again.
        tx.try_send(()).expect("test: send after drain succeeds");
        rx.recv().await.expect("test: receives second wakeup");
    });
}

#[test]
fn install_chat_panic_hook_is_idempotent() {
    // Second + later calls must be no-ops (OnceLock-guarded). This test
    // verifies the function does not panic when called repeatedly; the
    // OnceLock state is process-wide so we cannot meaningfully assert
    // which call performed the install — but the absence of panics +
    // unbounded hook nesting is the contract.
    install_chat_panic_hook();
    install_chat_panic_hook();
    install_chat_panic_hook();
}
