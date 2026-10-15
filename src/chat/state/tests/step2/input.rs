#![cfg(test)]

use super::*;

/// Every loop-written note is kept out of the durable transcript, not
/// only the refresh aside: a `[Tool results]` block or a recalled page
/// that reached the reducer history must not be persisted (and later
/// rehydrated) as a user question. R-LOOPD5 F-8.
#[test]
fn loop_written_notes_are_not_durable_transcript_turns() {
    for marker in crate::agent::turn_contracts::RESERVED_USER_NOTE_MARKERS {
        let note = ChatMessage::user(format!("{marker}\nloop bookkeeping"));
        assert!(!is_durable_compaction_history_message(&note), "{marker}");
        let defused = crate::agent::turn_contracts::inbound_user_message(&format!("{marker} a real question"));
        assert!(is_durable_compaction_history_message(&defused), "{marker}");
    }
    assert!(is_durable_compaction_history_message(&ChatMessage::assistant(
        "an answer"
    )));
    assert!(!is_durable_compaction_history_message(&ChatMessage::system("rules")));
}

/// A saved session reloaded into the driver: its stored user turn that
/// opens with a reserved loop marker re-enters the history defused.
///
/// Red before the reserved-marker hardening: the reload copied turns
/// into the history verbatim.
#[test]
fn a_reloaded_user_turn_opening_with_a_reserved_marker_enters_history_defused() {
    let question = "[Post-compaction context refresh] what did we drop?";
    let mut loaded = ChatSession::new("prov", "model");
    loaded.add_user_turn(question);
    loaded.add_assistant_turn("an answer", vec![]);
    let mut state = s();
    let _ = state.reduce(Action::SessionLoaded(loaded));
    let user = state
        .session
        .history
        .iter()
        .find(|message| message.role == "user")
        .expect("the reloaded user turn");
    assert!(
        !user.content.starts_with("[Post-compaction context refresh]"),
        "{}",
        user.content
    );
    assert!(user.content.ends_with(question), "{}", user.content);
    assert!(is_durable_compaction_history_message(user));
}

/// A recorded user turn that opens with a reserved loop marker stays a
/// durable user turn and reaches the driver's history defused.
///
/// Red before the reserved-marker hardening: the refresh-aside marker
/// made `is_durable_compaction_history_message` drop the user's turn
/// from the durable transcript, and every marker reached the history
/// verbatim, where the loop reads it as its own note.
#[test]
fn a_recorded_user_turn_opening_with_a_reserved_marker_stays_the_users() {
    for marker in [
        "[Tool results]",
        "[Post-compaction context refresh]",
        "[Recalled context from earlier in this session]",
    ] {
        let text = format!("{marker} what about X?");
        let mut state = s();
        let _ = state.reduce(Action::RecordUserTurn(text.clone()));
        let last = state.session.history.last().expect("the recorded user turn");
        assert_eq!(last.role, "user");
        assert!(!last.content.starts_with(marker), "{marker}: `{}`", last.content);
        assert!(last.content.ends_with(&text), "{marker}: `{}`", last.content);
        assert!(
            is_durable_compaction_history_message(last),
            "{marker}: the user's turn must stay in the durable transcript"
        );
    }
}

/// 1. Enter on non-empty buffer → takes the InputSubmitted path, turn_count += 1
#[test]
fn test_reduce_key_pressed_enter_returns_input_submitted() {
    let mut state = s();
    // simulate the user typing "hi"
    for ch in "hi".chars() {
        let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    assert_eq!(state.ui.input.text(), "hi");
    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert!(has_log_trace(&effects), "Enter must trigger LogTrace");
    assert!(has_request_redraw(&effects), "Enter must trigger RequestRedraw");
    assert_eq!(state.ui.turn_count, 1, "turn_count must increment");
    assert_eq!(state.ui.last_submitted.as_deref(), Some("hi"));
    assert!(state.ui.input.is_empty(), "buffer must be cleared after submit");
}

/// 2. Tab → ToolCardFoldToggled, returns RequestRedraw
#[test]
fn test_reduce_key_pressed_tab_returns_fold_toggled() {
    let mut state = s();
    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    assert!(has_request_redraw(&effects));
    // Tab must not enter the input buffer
    assert!(state.ui.input.is_empty());
}

/// 3. single Ctrl+C → only records the window, does not return Quit
#[test]
fn test_reduce_key_pressed_ctrl_c_single_returns_cancel() {
    let mut state = s();
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let effects = state.reduce_with_now(Action::KeyPressed(key), 10_000);
    assert!(!has_quit(&effects), "a single Ctrl+C must not Quit");
    assert_eq!(state.ui.last_ctrlc_ms, 10_000, "records the window timestamp");
}

/// 4. Ctrl+C pressed twice within 500ms → Quit
#[test]
fn test_reduce_key_pressed_ctrl_c_double_within_500ms_returns_shutdown() {
    let mut state = s();
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let _ = state.reduce_with_now(Action::KeyPressed(key.clone()), 10_000);
    // Ctrl+C again 100ms later
    let effects = state.reduce_with_now(Action::KeyPressed(key), 10_100);
    assert!(has_quit(&effects), "a Ctrl+C double press must Quit");
}

/// 5. Ctrl+C pressed again after more than 500ms → only recorded, no Quit
#[test]
fn test_reduce_key_pressed_ctrl_c_double_after_500ms_returns_cancel_only() {
    let mut state = s();
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let _ = state.reduce_with_now(Action::KeyPressed(key.clone()), 10_000);
    // 600ms later — outside the window
    let effects = state.reduce_with_now(Action::KeyPressed(key), 10_600);
    assert!(!has_quit(&effects), "a second press after 500ms is not a double press");
    assert_eq!(state.ui.last_ctrlc_ms, 10_600);
}

/// 6. Ctrl+D on empty buffer → Quit
#[test]
fn test_reduce_key_pressed_ctrl_d_empty_buffer_returns_quit() {
    let mut state = s();
    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    )));
    assert!(has_quit(&effects), "Ctrl+D on an empty buffer must Quit");
}

/// 7. Ctrl+D on a non-empty buffer → forward-delete, no Quit
#[test]
fn test_reduce_key_pressed_ctrl_d_non_empty_buffer_inserts_char_or_eof() {
    let mut state = s();
    // type "abc" then press Home to move to the start of the line
    for ch in "abc".chars() {
        let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
    assert_eq!(state.ui.input.text(), "abc");
    // Ctrl+D should forward-delete 'a'
    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    )));
    assert!(!has_quit(&effects), "Ctrl+D on a non-empty buffer must not Quit");
    assert_eq!(state.ui.input.text(), "bc", "forward-delete must remove 'a'");
}

/// 8. PasteReceived → content is appended to the input buffer
#[test]
fn test_reduce_paste_received_appends_to_input() {
    let mut state = s();
    let effects = state.reduce(Action::PasteReceived("pasted-text".to_string()));
    assert!(has_request_redraw(&effects));
    assert_eq!(state.ui.input.text(), "pasted-text");
}

#[test]
fn test_large_paste_is_bounded_before_submit() {
    let mut state = s();
    let line = "b".repeat(1024);
    let pasted = std::iter::repeat_n(line.as_str(), 100).collect::<Vec<_>>().join("\n");

    let effects = state.reduce(Action::PasteReceived(pasted.clone()));
    assert!(has_request_redraw(&effects));
    assert_eq!(state.ui.input.byte_len(), crate::chat::tui::INPUT_MAX_BYTES);
    assert!(state.ui.input.truncated);
    assert!(pasted.starts_with(&state.ui.input.text()));
    let bounded = state.ui.input.text();

    let effects = state.reduce(Action::InputSubmitted(state.ui.input.text()));
    assert!(has_request_redraw(&effects));
    assert!(has_log_trace(&effects));
    assert_eq!(state.ui.last_submitted.as_deref(), Some(bounded.as_str()));
    assert!(state.ui.input.is_empty());
}

/// 9. TerminalResized → RequestRedraw
#[test]
fn test_reduce_terminal_resized_returns_redraw() {
    let mut state = s();
    let effects = state.reduce(Action::TerminalResized { w: 120, h: 40 });
    assert!(has_request_redraw(&effects));
}

/// 10. InputSubmitted (direct) → turn_count increments + last_submitted recorded
#[test]
fn test_reduce_input_submitted_increments_turn_count() {
    let mut state = s();
    assert_eq!(state.ui.turn_count, 0);
    let effects = state.reduce(Action::InputSubmitted("hello world".to_string()));
    assert_eq!(state.ui.turn_count, 1);
    assert_eq!(state.ui.last_submitted.as_deref(), Some("hello world"));
    assert!(state.ui.input.is_empty());
    assert!(has_log_trace(&effects));
    assert!(has_request_redraw(&effects));
}

/// Extra: HistoryNavigated Up must not panic on an empty history
#[test]
fn test_reduce_history_navigated_up_empty_history() {
    let mut state = s();
    let effects = state.reduce(Action::HistoryNavigated(HistoryDir::Up));
    assert!(has_request_redraw(&effects));
}

/// Extra: InputCancelled clears the buffer
#[test]
fn test_reduce_input_cancelled_clears_buffer() {
    let mut state = s();
    for ch in "draft".chars() {
        let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
    }
    assert_eq!(state.ui.input.text(), "draft");
    let effects = state.reduce(Action::InputCancelled);
    assert!(has_request_redraw(&effects));
    assert!(state.ui.input.is_empty());
}

#[test]
fn ctrl_r_reverse_search_updates_state_and_snapshot_input() {
    use crate::chat::tui::ConversationLine;
    let mut state = s();
    state.ui.input.history = vec!["alpha".to_string(), "beta".to_string()];
    state.ui.conversation_lines.push(ConversationLine::Reasoning {
        content: "thinking".to_string(),
        char_count: 8,
        folded: true,
    });
    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )));
    assert!(has_request_redraw(&effects));
    assert!(state.ui.input.is_reverse_search_active());
    assert_eq!(state.ui.input.text(), "beta");
    match state.ui.conversation_lines.last() {
        Some(ConversationLine::Reasoning { folded, .. }) => {
            assert!(*folded, "Ctrl+R must not fold reasoning after P6b2");
        }
        other => panic!("expected Reasoning card, got {other:?}"),
    }
    let snap = state.build_ui_snapshot(1);
    assert!(snap.input.is_reverse_search_active());
    assert_eq!(snap.input.text(), "beta");
}

#[test]
fn input_replaced_updates_snapshot_without_submit() {
    let mut state = s();
    state.ui.input.history = vec!["prior draft".to_string()];
    let _ = state.reduce(Action::KeyPressed(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )));
    assert!(
        state.ui.input.is_reverse_search_active(),
        "test setup: reverse-search must be active before external editor replacement"
    );
    let effects = state.reduce(Action::InputReplaced("edited draft".to_string()));
    assert!(has_request_redraw(&effects));
    assert_eq!(state.ui.input.text(), "edited draft");
    assert!(
        !state.ui.input.is_reverse_search_active(),
        "external editor replacement must clear stale reverse-search title"
    );
    assert_eq!(state.ui.turn_count, 0, "external editor replacement must not submit");
    let snap = state.build_ui_snapshot(2);
    assert_eq!(snap.input.text(), "edited draft");
    assert!(
        !snap.input.is_reverse_search_active(),
        "snapshot must not expose stale reverse-search after replacement"
    );
}

/// Extra: ReasoningFoldToggled still returns RequestRedraw when there is no reasoning card
#[test]
fn test_reduce_reasoning_fold_toggled_no_panic_when_absent() {
    let mut state = s();
    let effects = state.reduce(Action::ReasoningFoldToggled);
    assert!(has_request_redraw(&effects));
}

/// Hidden reasoning must not intercept Tab in the primary transcript.
#[test]
fn test_tab_ignores_hidden_reasoning_card() {
    use crate::chat::tui::ConversationLine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut state = s();
    state.ui.conversation_lines.push(ConversationLine::Reasoning {
        content: "deep thoughts".to_string(),
        char_count: 12,
        folded: true,
    });
    let generation_before = state.ui.conversation_generation;
    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    assert!(has_request_redraw(&effects));
    match state.ui.conversation_lines.last() {
        Some(ConversationLine::Reasoning { folded, .. }) => {
            assert!(*folded, "Tab must leave hidden reasoning unchanged");
        }
        other => panic!("expected Reasoning card, got {other:?}"),
    }
    assert_eq!(state.ui.conversation_generation, generation_before);
}

/// A visible tool fold toggle (KeyPressed Tab) must mark the reduce dirty so
/// `build_ui_snapshot` rebuilds with the new folded state (the Pure-mode
/// renderer reads the snapshot, not the live state). Guards against the
/// stale `cached_lines_arc` regression.
#[test]
fn test_fold_toggle_marks_snapshot_dirty_and_rebuilds() {
    use crate::chat::tui::ConversationLine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut state = s();
    state.ui.conversation_lines.push(ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "{}".to_string(),
        args_full: "{}".to_string(),
        result: Some("ok".to_string()),
        status: crate::chat::tui::ToolStatus::Done,
        elapsed_ms: Some(1),
        folded: true,
    });
    // Prime the snapshot cache.
    let _ = state.build_ui_snapshot(1);
    // Toggle via tracked reduce — must report dirty and clear the cache.
    let (_effects, dirty) = state.reduce_tracked(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    assert!(dirty, "fold toggle must mark snapshot dirty");
    let snap = state.build_ui_snapshot(2);
    match snap.conversation_lines.last() {
        Some(ConversationLine::ToolResult { folded, .. }) => {
            assert!(!folded, "rebuilt snapshot must reflect the expanded tool card");
        }
        other => panic!("expected ToolResult card in snapshot, got {other:?}"),
    }
}

/// BUG-01 round-2: a fold toggle must bump `conversation_generation` so
/// the snapshot/repaint path observes the new fold state.
#[test]
fn test_fold_toggle_bumps_conversation_generation() {
    use crate::chat::tui::ConversationLine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut state = s();
    state.ui.conversation_lines.push(ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "{}".to_string(),
        args_full: "{}".to_string(),
        result: Some("ok".to_string()),
        status: crate::chat::tui::ToolStatus::Done,
        elapsed_ms: Some(1),
        folded: true,
    });
    state.ui.conversation_lines.push(ConversationLine::Reasoning {
        content: "thoughts".to_string(),
        char_count: 8,
        folded: true,
    });
    let gen_before = state.ui.conversation_generation;

    // Tab toggles the visible tool even though hidden reasoning is newer.
    let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    assert_eq!(
        state.ui.conversation_generation,
        gen_before + 1,
        "Tab fold toggle must bump conversation_generation to force scrollback re-emit"
    );

    // Direct legacy fold action still bumps generation, but KeyPressed
    // Ctrl+R is reverse-search after P6b2 and must not own folding.
    let gen_after_tab = state.ui.conversation_generation;
    let _ = state.reduce(Action::ReasoningFoldToggled);
    assert_eq!(
        state.ui.conversation_generation,
        gen_after_tab + 1,
        "ReasoningFoldToggled must bump conversation_generation"
    );
}

/// BUG-01 round-2: a fold toggle with NO foldable card present must NOT
/// bump the generation (avoids spurious full-conversation re-emits / a
/// scrollback flood on stray Tab presses in a fresh session).
#[test]
fn test_fold_toggle_no_card_does_not_bump_generation() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut state = s();
    let gen_before = state.ui.conversation_generation;
    let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    assert_eq!(
        state.ui.conversation_generation, gen_before,
        "Tab with no foldable card must not bump generation"
    );
}

// ─── Step 2 integration tests (filling the P1-1 PTY coverage hole) ────────────
//
// The tests below build a ChatState directly and call a sequence of reduce calls to simulate a
// full user interaction, covering the reducer path inside run_tui_unified_loop (the
// PRX_CHAT_REDUX=1/both rollout). reduce_with_now injects a fixed time to avoid SystemTime.

/// P1-1-a: full input-to-submit flow — paste "hello" → Enter → expect LogTrace + RequestRedraw
#[test]
fn test_redux_full_input_to_submit_flow() {
    let mut state = s();
    // paste "hello"
    let effects = state.reduce(Action::PasteReceived("hello".to_string()));
    assert!(has_request_redraw(&effects), "paste must trigger RequestRedraw");
    assert_eq!(state.ui.input.text(), "hello");
    // press Enter to submit
    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert!(has_log_trace(&effects), "Enter must trigger LogTrace");
    assert!(has_request_redraw(&effects), "Enter must trigger RequestRedraw");
    assert_eq!(state.ui.turn_count, 1, "turn_count must increment to 1");
    assert_eq!(
        state.ui.last_submitted.as_deref(),
        Some("hello"),
        "last_submitted must record 'hello'"
    );
    assert!(state.ui.input.is_empty(), "input buffer must be cleared after submit");
}

/// P1-1-b: two Ctrl+C presses within 500ms → Quit
#[test]
fn test_redux_double_ctrl_c_within_500ms_quits() {
    let mut state = s();
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let effects = state.reduce_with_now(Action::KeyPressed(key.clone()), 100);
    assert!(!has_quit(&effects), "the first Ctrl+C must not Quit");
    let effects = state.reduce_with_now(Action::KeyPressed(key), 300);
    assert!(
        has_quit(&effects),
        "a Ctrl+C double press within 500ms must produce a Quit effect"
    );
}

/// P1-1-c: Ctrl+C presses more than 500ms apart do not quit
#[test]
fn test_redux_ctrl_c_then_ctrl_c_after_500ms_does_not_quit() {
    let mut state = s();
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let effects = state.reduce_with_now(Action::KeyPressed(key.clone()), 100);
    assert!(!has_quit(&effects));
    // pressed again 700ms later — beyond the 500ms window
    let effects = state.reduce_with_now(Action::KeyPressed(key), 700);
    assert!(!has_quit(&effects), "a second press after 500ms must not Quit");
    assert_eq!(state.ui.last_ctrlc_ms, 700);
}

/// P1-1-d: Ctrl+D on an empty buffer → Quit
#[test]
fn test_redux_ctrl_d_empty_buffer_quits() {
    let mut state = s();
    assert!(state.ui.input.is_empty(), "precondition: the buffer is empty");
    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    )));
    assert!(has_quit(&effects), "Ctrl+D on an empty buffer must Quit");
}

/// P1-1-e: Ctrl+D on a non-empty buffer → no quit (forward-delete)
#[test]
fn test_redux_ctrl_d_non_empty_does_not_quit() {
    let mut state = s();
    // type "xyz" then Home so there is content after the cursor
    let _ = state.reduce(Action::PasteReceived("xyz".to_string()));
    let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
    assert_eq!(state.ui.input.text(), "xyz");
    let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
    )));
    assert!(!has_quit(&effects), "Ctrl+D on a non-empty buffer must not Quit");
}

/// P1-1-f: HistoryNavigated Up/Down — no panic on empty history, returns RequestRedraw
#[test]
fn test_redux_history_navigation_up_down() {
    let mut state = s();
    let up = state.reduce(Action::HistoryNavigated(HistoryDir::Up));
    assert!(has_request_redraw(&up), "Up must return RequestRedraw");
    let down = state.reduce(Action::HistoryNavigated(HistoryDir::Down));
    assert!(has_request_redraw(&down), "Down must return RequestRedraw");
}

/// P1-1-g: PasteReceived → the input buffer holds the text + RequestRedraw
#[test]
fn test_redux_paste_into_input() {
    let mut state = s();
    let effects = state.reduce(Action::PasteReceived("pasted content".to_string()));
    assert!(has_request_redraw(&effects), "a paste must trigger RequestRedraw");
    assert_eq!(state.ui.input.text(), "pasted content");
}

/// P1-1-h: TerminalResized → RequestRedraw
#[test]
fn test_redux_terminal_resize_returns_redraw() {
    let mut state = s();
    let effects = state.reduce(Action::TerminalResized { w: 80, h: 24 });
    assert!(has_request_redraw(&effects), "a resize must trigger RequestRedraw");
}
