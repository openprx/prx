#![cfg(test)]

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct FakeTerminalMode {
    suspended: Arc<AtomicUsize>,
    restored: Arc<AtomicUsize>,
}

impl ExternalEditorTerminalMode for FakeTerminalMode {
    fn suspend_for_editor(&self) {
        self.suspended.fetch_add(1, Ordering::SeqCst);
    }

    fn restore_after_editor(&self) {
        self.restored.fetch_add(1, Ordering::SeqCst);
    }
}

fn shell_editor_script(body: &str) -> (tempfile::NamedTempFile, String) {
    let mut script = tempfile::NamedTempFile::new().expect("test: script temp file");
    script.write_all(body.as_bytes()).expect("test: write editor script");
    script.flush().expect("test: flush editor script");
    let command = format!("sh {}", script.path().display());
    (script, command)
}

#[test]
fn external_editor_success_rewrites_draft_and_restores_terminal() {
    let (_script, editor) = shell_editor_script("printf 'edited draft' > \"$1\"\n");
    let terminal = FakeTerminalMode::default();
    let result = edit_text_with_external_editor("old draft", Some(editor), &terminal);
    assert_eq!(result, ExternalEditorResult::Edited("edited draft".to_string()));
    assert_eq!(terminal.suspended.load(Ordering::SeqCst), 1);
    assert_eq!(terminal.restored.load(Ordering::SeqCst), 1);
}

#[test]
fn external_editor_nonzero_exit_keeps_draft_unchanged_and_restores_terminal() {
    let (_script, editor) = shell_editor_script("exit 7\n");
    let terminal = FakeTerminalMode::default();
    let result = edit_text_with_external_editor("old draft", Some(editor), &terminal);
    match result {
        ExternalEditorResult::Unchanged(reason) => {
            assert!(
                reason.contains("exited with status"),
                "reason should mention nonzero exit: {reason}"
            );
        }
        other => panic!("expected unchanged result, got {other:?}"),
    }
    assert_eq!(terminal.suspended.load(Ordering::SeqCst), 1);
    assert_eq!(terminal.restored.load(Ordering::SeqCst), 1);
}

#[test]
fn external_editor_spawn_failure_keeps_draft_unchanged_and_restores_terminal() {
    let terminal = FakeTerminalMode::default();
    let result = edit_text_with_external_editor_with_runner(
        "old draft",
        Some("unused-editor".to_string()),
        &terminal,
        |_editor, _path| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "spawn failed")),
    );
    match result {
        ExternalEditorResult::Unchanged(reason) => {
            assert!(
                reason.contains("failed to start"),
                "reason should mention spawn failure: {reason}"
            );
        }
        other => panic!("expected unchanged result, got {other:?}"),
    }
    assert_eq!(terminal.suspended.load(Ordering::SeqCst), 1);
    assert_eq!(
        terminal.restored.load(Ordering::SeqCst),
        1,
        "spawn failure must still restore terminal mode"
    );
}

#[test]
fn external_editor_missing_env_keeps_draft_unchanged_without_terminal_handoff() {
    let terminal = FakeTerminalMode::default();
    let result = edit_text_with_external_editor("old draft", None, &terminal);
    match result {
        ExternalEditorResult::Unchanged(reason) => {
            assert!(reason.contains("VISUAL") && reason.contains("EDITOR"));
        }
        other => panic!("expected unchanged result, got {other:?}"),
    }
    assert_eq!(terminal.suspended.load(Ordering::SeqCst), 0);
    assert_eq!(terminal.restored.load(Ordering::SeqCst), 0);
}

#[test]
fn external_editor_fullscreen_suspend_leaves_alt_and_restore_reenters() {
    CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.store(false, Ordering::SeqCst);
    let previous_mouse_capture = CHAT_MOUSE_CAPTURE_ACTIVE.swap(true, Ordering::SeqCst);
    let mut suspend = Vec::new();
    write_external_editor_suspend_sequences(&mut suspend);
    let suspend = String::from_utf8(suspend).expect("test: suspend escape bytes are utf-8");
    assert!(
        suspend.contains("\x1b[?1049l") && suspend.contains("\x1b[?47l"),
        "fullscreen editor suspend must leave chat alt-screen: {suspend:?}"
    );

    let mut restore = Vec::new();
    write_external_editor_restore_sequences(&mut restore);
    let restore = String::from_utf8(restore).expect("test: restore escape bytes are utf-8");
    CHAT_MOUSE_CAPTURE_ACTIVE.store(previous_mouse_capture, Ordering::SeqCst);
    assert!(
        restore.contains("\x1b[?1049l") && restore.contains("\x1b[?47l"),
        "fullscreen editor restore must reset any child/editor alt-screen first: {restore:?}"
    );
    let alt_enter = restore
        .find("\x1b[?1049h")
        .expect("test: fullscreen editor restore must re-enter chat alt-screen");
    let mouse_enable = restore
        .find("\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h")
        .expect("test: fullscreen editor restore must re-enable chat mouse capture");
    assert!(
        alt_enter < mouse_enable,
        "fullscreen editor restore must re-enable chat mouse capture after chat alt-screen: {restore:?}"
    );
}

#[test]
fn external_editor_handoff_pops_and_repushes_keyboard_flags_when_active() {
    let previous = CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.swap(true, Ordering::SeqCst);

    let mut suspend = Vec::new();
    write_external_editor_suspend_sequences(&mut suspend);
    let suspend = String::from_utf8(suspend).expect("test: suspend escape bytes are utf-8");
    assert!(
        suspend.starts_with("\x1b[<1u"),
        "active keyboard enhancement must be popped before editor handoff: {suspend:?}"
    );

    let mut restore = Vec::new();
    write_external_editor_restore_sequences(&mut restore);
    let restore = String::from_utf8(restore).expect("test: restore escape bytes are utf-8");
    assert!(
        restore.ends_with("\x1b[>15u"),
        "active keyboard enhancement must be re-pushed after editor handoff: {restore:?}"
    );

    CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.store(previous, Ordering::SeqCst);
}

#[test]
fn directional_switch_debounce_suppresses_rapid_attach_dispatches() {
    let key = crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Right, crossterm::event::KeyModifiers::NONE);
    let start = Instant::now();
    let mut last = None;

    assert_eq!(
        debounce_directional_switch_dispatch(key, tui::KeyDispatch::AttachSession { seq: 1 }, &mut last, start,),
        tui::KeyDispatch::AttachSession { seq: 1 }
    );
    assert_eq!(
        debounce_directional_switch_dispatch(
            key,
            tui::KeyDispatch::SwitchSession { seq: 2 },
            &mut last,
            start + Duration::from_millis(20),
        ),
        tui::KeyDispatch::Consumed,
        "rapid Left/Right repeats must not enqueue another synthetic attach"
    );
    assert_eq!(
        debounce_directional_switch_dispatch(
            key,
            tui::KeyDispatch::SwitchSession { seq: 2 },
            &mut last,
            start + Duration::from_millis(120),
        ),
        tui::KeyDispatch::SwitchSession { seq: 2 }
    );
}
