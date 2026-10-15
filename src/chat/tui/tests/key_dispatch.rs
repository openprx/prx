#![cfg(test)]

use super::*;

// ── P2-Integration: global key dispatch tests ────────────────────────────

#[test]
fn dispatch_tab_toggles_last_tool_result_card() {
    let mut state = TuiState::new("p", "m");
    // Without any foldable card the Tab keystroke is still consumed (no-op).
    let out = dispatch_global_key(key(KeyCode::Tab), &mut state);
    assert_eq!(out, KeyDispatch::Consumed);

    // Push a tool result and verify Tab flips its folded flag.
    state.push_tool_result_started("shell", "{}");
    let folded_before = match state.conversation_lines.last() {
        Some(ConversationLine::ToolResult { folded, .. }) => *folded,
        _ => panic!("test: expected ToolResult at end"),
    };
    let out = dispatch_global_key(key(KeyCode::Tab), &mut state);
    assert_eq!(out, KeyDispatch::Consumed);
    let folded_after = match state.conversation_lines.last() {
        Some(ConversationLine::ToolResult { folded, .. }) => *folded,
        _ => panic!("test: expected ToolResult at end"),
    };
    assert_ne!(folded_before, folded_after, "Tab must flip folded state");
    // Tab is consumed by the dispatcher → input buffer untouched.
    assert!(state.input.is_empty(), "Tab must not fall through to input box");
}

#[test]
fn dispatch_tab_mid_edit_inserts_tab_instead_of_folding() {
    let mut state = TuiState::new("p", "m");
    state.input.set_text("alpha");
    state.push_tool_result_started("shell", "{}");
    let folded_before = match state.conversation_lines.last() {
        Some(ConversationLine::ToolResult { folded, .. }) => *folded,
        _ => panic!("test: expected ToolResult at end"),
    };

    let out = dispatch_global_key(key(KeyCode::Tab), &mut state);

    assert_eq!(out, KeyDispatch::Consumed);
    assert_eq!(state.input.text(), "alpha\t");
    let folded_after = match state.conversation_lines.last() {
        Some(ConversationLine::ToolResult { folded, .. }) => *folded,
        _ => panic!("test: expected ToolResult at end"),
    };
    assert_eq!(folded_before, folded_after, "mid-edit Tab must not fold cards");
}

#[test]
fn dispatch_backtab_cycles_chat_mode_when_input_empty() {
    let mut state = TuiState::new("p", "m");
    state.chat_mode = ChatMode::Plan;

    let out = dispatch_global_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT), &mut state);

    assert_eq!(out, KeyDispatch::ModeChanged(ChatMode::Edit));
    assert_eq!(state.chat_mode, ChatMode::Edit);
    assert!(state.input.is_empty());

    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT), &mut state),
        KeyDispatch::ModeChanged(ChatMode::Auto)
    );
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT), &mut state),
        KeyDispatch::ModeChanged(ChatMode::Plan)
    );
}

#[test]
fn dispatch_backtab_mid_edit_does_not_cycle_mode() {
    let mut state = TuiState::new("p", "m");
    state.input.set_text("alpha");
    state.input.cursor = (0, 2);
    state.chat_mode = ChatMode::Plan;

    let out = dispatch_global_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT), &mut state);

    assert_eq!(out, KeyDispatch::Consumed);
    assert_eq!(state.chat_mode, ChatMode::Plan);
    assert_eq!(state.input.text(), "alpha");
    assert_eq!(state.input.cursor, (0, 2));
}

#[test]
fn dispatch_backtab_is_captured_by_modal_layers_before_mode_cycle() {
    let backtab = key_mod(KeyCode::BackTab, KeyModifiers::SHIFT);

    let mut picker = TuiState::new("p", "m");
    picker.chat_mode = ChatMode::Plan;
    picker.input.set_text("draft");
    picker.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
        saved_picker_entry("saved", "saved session", false),
    ]));
    assert_eq!(dispatch_global_key(backtab, &mut picker), KeyDispatch::Consumed);
    assert_eq!(picker.chat_mode, ChatMode::Plan);
    assert_eq!(picker.input.text(), "draft");

    let mut switcher = TuiState::new("p", "m");
    switcher.chat_mode = ChatMode::Plan;
    switcher.sessions_cache = vec![crate::chat::sessions::SwitcherEntry {
        seq: 1,
        kind: "agent",
        origin: "model",
        status: "running",
        title: "child".to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
        idle_warning: false,
    }];
    assert!(matches!(
        dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut switcher),
        KeyDispatch::SwitcherOpened { .. }
    ));
    assert_eq!(dispatch_global_key(backtab, &mut switcher), KeyDispatch::Consumed);
    assert_eq!(switcher.chat_mode, ChatMode::Plan);

    let mut approval = approval_state();
    approval.chat_mode = ChatMode::Plan;
    approval.input.set_text("hold");
    let cursor = approval.input.cursor;
    assert_eq!(dispatch_global_key(backtab, &mut approval), KeyDispatch::Consumed);
    assert_eq!(approval.chat_mode, ChatMode::Plan);
    assert_eq!(approval.input.text(), "hold");
    assert_eq!(approval.input.cursor, cursor);

    let mut editor_prefix = TuiState::new("p", "m");
    editor_prefix.chat_mode = ChatMode::Plan;
    editor_prefix.input.set_text("edit me");
    editor_prefix.external_editor_prefix_armed = true;
    assert_eq!(dispatch_global_key(backtab, &mut editor_prefix), KeyDispatch::Consumed);
    assert_eq!(editor_prefix.chat_mode, ChatMode::Plan);
    assert_eq!(editor_prefix.input.text(), "edit me");
    assert!(!editor_prefix.external_editor_prefix_armed);
}

#[test]
fn dispatch_tab_ignores_hidden_reasoning_and_toggles_tool() {
    let mut state = TuiState::new("p", "m");
    state.push_tool_result_started("shell", "{}");
    assert!(state.push_reasoning("step 1\nstep 2"));

    let out = dispatch_global_key(key(KeyCode::Tab), &mut state);
    assert_eq!(out, KeyDispatch::Consumed);
    match state.conversation_lines.last() {
        Some(ConversationLine::Reasoning { folded, .. }) => assert!(*folded, "hidden reasoning stays folded"),
        other => panic!("test: expected Reasoning at end, got {other:?}"),
    }
    let tool_idx = state
        .conversation_lines
        .iter()
        .position(ConversationLine::is_tool_result)
        .expect("test: tool card exists");
    match state.conversation_lines.get(tool_idx).expect("test: tool idx valid") {
        ConversationLine::ToolResult { folded, .. } => assert!(!*folded, "visible tool card toggled"),
        other => panic!("test: expected ToolResult, got {other:?}"),
    }
}

#[test]
fn toggle_last_foldable_card_returns_kind_tag() {
    let mut state = TuiState::new("p", "m");
    assert_eq!(state.toggle_last_foldable_card(), None);

    state.push_tool_result_started("shell", "{}");
    assert_eq!(
        state.toggle_last_foldable_card(),
        Some((FoldableKind::ToolResult, false))
    );

    assert!(state.push_reasoning("thinking"));
    assert_eq!(
        state.toggle_last_foldable_card(),
        Some((FoldableKind::ToolResult, true))
    );
}

#[test]
fn dispatch_ctrl_r_opens_reverse_search_without_reasoning_toggle() {
    let mut state = TuiState::new("p", "m");
    state.input.history = vec!["alpha".to_string(), "beta".to_string()];
    assert!(state.push_reasoning("step 1\nstep 2"));
    let folded_before = match state.conversation_lines.last() {
        Some(ConversationLine::Reasoning { folded, .. }) => *folded,
        _ => panic!("test: expected Reasoning at end"),
    };

    let out = dispatch_global_key(key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut state);
    assert_eq!(out, KeyDispatch::Consumed);
    assert!(state.input.is_reverse_search_active(), "Ctrl+R opens reverse-search");
    assert_eq!(state.input.text(), "beta", "initial Ctrl+R recalls latest history item");

    let folded_after = match state.conversation_lines.last() {
        Some(ConversationLine::Reasoning { folded, .. }) => *folded,
        _ => panic!("test: expected Reasoning at end"),
    };
    assert_eq!(folded_before, folded_after, "Ctrl+R must not fold reasoning after P6b2");
}

#[test]
fn reverse_search_filters_history_accepts_and_cancels() {
    let mut input = TuiInput::new();
    input.history = vec![
        "alpha one".to_string(),
        "beta two".to_string(),
        "alpha three".to_string(),
    ];
    input.set_text("draft");
    assert!(input.begin_or_cycle_reverse_search());
    assert!(input.is_reverse_search_active());
    assert_eq!(input.text(), "alpha three");

    assert_eq!(input.handle_key(key(KeyCode::Char('b'))), InputOutcome::Consumed);
    assert_eq!(input.text(), "beta two", "query filters to matching history entry");
    assert_eq!(input.handle_key(key(KeyCode::Enter)), InputOutcome::Consumed);
    assert!(!input.is_reverse_search_active());
    assert_eq!(input.text(), "beta two", "Enter accepts match without submitting");

    input.set_text("draft");
    assert!(input.begin_or_cycle_reverse_search());
    assert_eq!(input.handle_key(key(KeyCode::Char('z'))), InputOutcome::Consumed);
    assert_eq!(input.text(), "draft", "no-match restores visible draft while searching");
    assert_eq!(input.handle_key(key(KeyCode::Esc)), InputOutcome::Cancelled);
    assert_eq!(input.text(), "draft", "Esc cancels and restores saved draft");
    assert!(!input.is_reverse_search_active());
}

#[test]
fn reverse_search_ctrl_r_cycles_older_matches() {
    let mut input = TuiInput::new();
    input.history = vec!["alpha first".to_string(), "zzz".to_string(), "alpha second".to_string()];
    assert!(input.begin_or_cycle_reverse_search());
    assert_eq!(input.text(), "alpha second");
    assert_eq!(
        input.handle_key(key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        InputOutcome::Consumed
    );
    assert_eq!(input.text(), "zzz", "empty query cycles older entries");
    assert_eq!(input.handle_key(key(KeyCode::Char('a'))), InputOutcome::Consumed);
    assert_eq!(input.text(), "alpha second", "query starts from latest matching entry");
    assert_eq!(
        input.handle_key(key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        InputOutcome::Consumed
    );
    assert_eq!(input.text(), "alpha first", "Ctrl+R cycles older query matches");
}

#[test]
fn dispatch_ctrl_r_focus_input_matrix_main_session_transcript() {
    let focuses = [
        crate::chat::sessions::FocusTarget::Main,
        crate::chat::sessions::FocusTarget::Session { seq: 7 },
        crate::chat::sessions::FocusTarget::Transcript,
    ];
    for focus in focuses {
        for seed in ["", "draft"] {
            let mut state = TuiState::new("p", "m");
            state.focus = focus;
            state.input.history = vec!["history item".to_string()];
            if !seed.is_empty() {
                state.input.set_text(seed);
            }
            let out = dispatch_global_key(key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut state);
            assert_eq!(
                out,
                KeyDispatch::Consumed,
                "Ctrl+R consumed for focus={focus:?} seed={seed:?}"
            );
            assert!(
                state.input.is_reverse_search_active(),
                "reverse-search active for focus={focus:?} seed={seed:?}"
            );
            assert_eq!(
                state.input.text(),
                "history item",
                "no steer/scroll leak for focus={focus:?}"
            );
        }
    }
}

#[test]
fn saved_session_picker_captures_navigation_enter_and_esc() {
    let mut state = TuiState::new("p", "m");
    state.input.history = vec!["history item".to_string()];
    state.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
        saved_picker_entry("latest", "latest session", true),
        saved_picker_entry("older", "older session", false),
    ]));

    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::SavedSessionPickerMoved { selected: 1 }
    );
    assert!(
        state.input.is_empty(),
        "Down must not navigate input history while picker is open"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::SavedSessionPickerMoved { selected: 0 }
    );
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::SavedSessionPickerMoved { selected: 1 }
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::ResumeSavedSession {
            id: "older".to_string()
        }
    );
    assert!(
        state.saved_session_picker.is_none(),
        "Enter closes picker before control event"
    );

    state.input.set_text("draft");
    state.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
        saved_picker_entry("latest", "latest session", true),
    ]));
    assert_eq!(
        dispatch_global_key(key(KeyCode::Esc), &mut state),
        KeyDispatch::SavedSessionPickerClosed
    );
    assert_eq!(state.input.text(), "draft", "Esc close must preserve draft input");
}

#[test]
fn saved_session_picker_priority_blocks_child_switcher_and_stale_index_clamps() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![crate::chat::sessions::SwitcherEntry {
        seq: 1,
        kind: "agent",
        origin: "model",
        status: "running",
        title: "child".to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
        idle_warning: false,
    }];
    state.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState {
        entries: vec![saved_picker_entry("only", "only session", false)],
        selected: 99,
    });

    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed,
        "Ctrl+G must not open child switcher while saved-session picker is open"
    );
    assert!(state.switcher.is_none());
    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::ResumeSavedSession { id: "only".to_string() },
        "stale picker selection clamps to the last valid row"
    );

    let out = dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
    assert!(matches!(out, KeyDispatch::SwitcherOpened { .. }));
    assert!(state.switcher.is_some(), "Ctrl+G works again after picker closes");
}

#[test]
fn saved_session_picker_row_truncates_to_unicode_width() {
    let entry = saved_picker_entry(
        "wide",
        "\u{ac00}\u{ac01}\u{ac02}\u{ac03}\u{ac04}\u{ac05}\u{ac06} mixed ascii text",
        true,
    );
    let row = render_saved_session_picker_row(&entry, false, 18, false);
    assert!(
        UnicodeWidthStr::width(row.as_str()) <= 18,
        "row must fit width, got width={} row={row:?}",
        UnicodeWidthStr::width(row.as_str())
    );
    assert!(row.contains('\u{2026}'), "truncated row should show ellipsis: {row:?}");
}

#[test]
fn saved_session_picker_closed_keeps_ctrl_p_n_input_history_behavior() {
    let mut state = TuiState::new("p", "m");
    state.input.history = vec!["alpha".to_string(), "beta".to_string()];
    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::ScrollTranscriptUp
    );
    assert!(state.input.is_empty());
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "beta");
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert!(state.input.is_empty());

    state.sessions_cache = vec![entry(1)];
    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::ScrollTranscriptDown,
        "bare Down keeps transcript scroll even when bottom entries exist"
    );
}

#[test]
fn dispatch_ctrl_x_ctrl_e_requests_external_editor_without_input_leak() {
    let mut state = TuiState::new("p", "m");
    state.input.set_text("draft");
    let first = dispatch_global_key(key_mod(KeyCode::Char('x'), KeyModifiers::CONTROL), &mut state);
    assert_eq!(first, KeyDispatch::Consumed);
    assert_eq!(state.input.text(), "draft");
    let second = dispatch_global_key(key_mod(KeyCode::Char('e'), KeyModifiers::CONTROL), &mut state);
    assert_eq!(second, KeyDispatch::ExternalEditorRequested);
    assert_eq!(state.input.text(), "draft", "chord must not mutate input itself");
}

#[test]
fn dispatch_ctrl_x_non_editor_second_key_does_not_leak() {
    let mut state = TuiState::new("p", "m");
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('x'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Char('q')), &mut state),
        KeyDispatch::Consumed
    );
    assert!(state.input.is_empty(), "non-E chord tail must not enter input");
    assert!(!state.external_editor_prefix_armed, "non-E chord tail clears latch");
}

#[test]
fn approval_child_y_sends_tool_approval_received_true() {
    let mut state = approval_state();
    let out = dispatch_global_key(key(KeyCode::Char('y')), &mut state);
    assert_eq!(
        out,
        KeyDispatch::ToolApprovalDecision {
            tool_id: "call-1".to_string(),
            approved: true
        }
    );
    assert!(state.pending_tool_approval.is_none());
    assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
}

#[test]
fn approval_child_n_or_esc_sends_tool_approval_received_false() {
    for key_event in [key(KeyCode::Char('n')), key(KeyCode::Esc)] {
        let mut state = approval_state();
        let out = dispatch_global_key(key_event, &mut state);
        assert_eq!(
            out,
            KeyDispatch::ToolApprovalDecision {
                tool_id: "call-1".to_string(),
                approved: false
            }
        );
        assert!(state.pending_tool_approval.is_none());
        assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
    }
}

#[test]
fn approval_child_text_does_not_submit_or_steer() {
    let mut state = approval_state();
    let out = dispatch_global_key(key(KeyCode::Char('x')), &mut state);
    assert_eq!(out, KeyDispatch::Consumed);
    assert!(state.input.is_empty(), "approval focus must not edit the main input");
    assert!(
        state.pending_tool_approval.is_some(),
        "plain text must not close approval"
    );
    assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Approval);
}

#[test]
fn approval_child_arrows_select_and_enter_confirms() {
    let mut state = approval_state();
    assert!(
        !state
            .pending_tool_approval
            .as_ref()
            .expect("approval starts pending")
            .selected_approval,
        "approval defaults to the safe deny choice"
    );

    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::Consumed
    );
    assert!(
        state
            .pending_tool_approval
            .as_ref()
            .expect("approval remains pending after navigation")
            .selected_approval,
        "Right selects approve"
    );
    let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
    assert_eq!(
        out,
        KeyDispatch::ToolApprovalDecision {
            tool_id: "call-1".to_string(),
            approved: true
        }
    );
    assert!(state.pending_tool_approval.is_none());
    assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
}

#[test]
fn approval_child_enter_uses_default_deny_and_left_selects_deny() {
    let mut default_state = approval_state();
    let out = dispatch_global_key(key(KeyCode::Enter), &mut default_state);
    assert_eq!(
        out,
        KeyDispatch::ToolApprovalDecision {
            tool_id: "call-1".to_string(),
            approved: false
        }
    );

    let mut state = approval_state();
    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Left), &mut state),
        KeyDispatch::Consumed
    );
    let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
    assert_eq!(
        out,
        KeyDispatch::ToolApprovalDecision {
            tool_id: "call-1".to_string(),
            approved: false
        }
    );
}

#[test]
fn approval_child_ctrl_c_keeps_global_interrupt_semantics() {
    let mut state = approval_state();

    let out = dispatch_global_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut state);

    assert_eq!(out, KeyDispatch::InterruptTurn);
    assert!(
        state.pending_tool_approval.is_none(),
        "Ctrl+C must clear mirror approval so later input is not swallowed"
    );
    assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
}

#[test]
fn approval_child_ctrl_c_allows_next_message_submission() {
    let mut state = approval_state();

    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::InterruptTurn
    );
    for ch in "next".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Submitted("next".to_string())
    );
    assert!(state.pending_tool_approval.is_none());
    assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
}

#[test]
fn overlay_open_ctrl_c_and_empty_ctrl_d_keep_global_semantics() {
    let mut state = TuiState::new("p", "m");
    for ch in "/mo".chars() {
        let _ = dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
    }
    assert!(state.slash_menu.is_some());

    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::InterruptTurn
    );
    state.input.clear();
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('d'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Exit
    );
}

#[test]
fn slash_menu_refresh_preserves_selected_command_row() {
    let mut state = TuiState::new("p", "m");
    let _ = dispatch_global_key(key(KeyCode::Char('/')), &mut state);
    assert!(state.slash_menu.as_ref().is_some_and(|menu| menu.len() > 1));

    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.slash_menu.as_ref().map(|menu| menu.selected), Some(1));

    state.update_at_path_candidates(Vec::new());

    assert_eq!(
        state.slash_menu.as_ref().map(|menu| menu.selected),
        Some(1),
        "unrelated @path refresh must not reset slash command selection"
    );
}

#[test]
fn at_path_refresh_preserves_selected_candidate_for_enter() {
    let mut state = TuiState::new("p", "m");
    state.input.set_text("@s");
    let candidates = vec![
        AtPathCandidate {
            path: "src/".to_string(),
            is_dir: true,
        },
        AtPathCandidate {
            path: "setup.rs".to_string(),
            is_dir: false,
        },
    ];
    state.update_at_path_candidates(candidates.clone());
    assert_eq!(state.slash_menu.as_ref().map(|menu| menu.selected), Some(0));

    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.slash_menu.as_ref().map(|menu| menu.selected), Some(1));

    state.update_at_path_candidates(candidates);
    assert_eq!(
        state.slash_menu.as_ref().map(|menu| menu.selected),
        Some(1),
        "unchanged @path refresh must keep the highlighted row"
    );

    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(
        state.input.text(),
        "@setup.rs ",
        "Enter must insert the row that was still highlighted after refresh"
    );
}

#[test]
fn esc_during_generation_interrupts_before_clearing_input() {
    let mut state = TuiState::new("p", "m");
    state.input.set_text("draft text");
    state.start_stream("draft-1");

    assert_eq!(
        dispatch_global_key(key(KeyCode::Esc), &mut state),
        KeyDispatch::InterruptTurn
    );
    assert_eq!(state.input.text(), "draft text");
}

#[test]
fn approval_focus_without_pending_esc_resets_to_main() {
    let mut state = TuiState::new("p", "m");
    state.focus = crate::chat::sessions::FocusTarget::Approval;
    state.pending_tool_approval = None;

    let out = dispatch_global_key(key(KeyCode::Esc), &mut state);

    assert_eq!(out, KeyDispatch::Cancelled);
    assert!(state.pending_tool_approval.is_none());
    assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
}

#[test]
fn dispatch_ctrl_o_opens_transcript_viewer_without_input_leak() {
    let mut state = TuiState::new("p", "m");
    let out = dispatch_global_key(key_mod(KeyCode::Char('o'), KeyModifiers::CONTROL), &mut state);
    assert_eq!(out, KeyDispatch::OpenTranscriptViewer);
    assert!(state.input.is_empty(), "Ctrl+O must not insert text into input");
}

#[test]
fn dispatch_typing_and_enter_yields_submission() {
    let mut state = TuiState::new("p", "m");
    for ch in "Hello".chars() {
        let out = dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
    }
    let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
    assert_eq!(out, KeyDispatch::Submitted("Hello".to_string()));
    assert!(state.input.is_empty(), "buffer cleared after submit");
}

#[test]
fn dispatch_ctrl_c_signals_interrupt_turn() {
    let mut state = TuiState::new("p", "m");
    let out = dispatch_global_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut state);
    assert_eq!(out, KeyDispatch::InterruptTurn);
}

#[test]
fn dispatch_ctrl_d_on_empty_buffer_signals_exit() {
    let mut state = TuiState::new("p", "m");
    let out = dispatch_global_key(key_mod(KeyCode::Char('d'), KeyModifiers::CONTROL), &mut state);
    assert_eq!(out, KeyDispatch::Exit);
}

#[test]
fn dispatch_ctrl_d_on_non_empty_buffer_deletes_forward() {
    let mut state = TuiState::new("p", "m");
    // Type "abc" then move cursor to start.
    for ch in "abc".chars() {
        dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
    }
    dispatch_global_key(key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL), &mut state);
    assert_eq!(state.input.text(), "abc");
    // Ctrl+D should forward-delete 'a' instead of exiting.
    let out = dispatch_global_key(key_mod(KeyCode::Char('d'), KeyModifiers::CONTROL), &mut state);
    assert_eq!(out, KeyDispatch::Consumed);
    assert_eq!(state.input.text(), "bc");
}

#[test]
fn dispatch_page_home_end_scroll_main_transcript_even_with_input() {
    let mut state = TuiState::new("p", "m");
    let out = dispatch_global_key(key(KeyCode::PageUp), &mut state);
    assert_eq!(out, KeyDispatch::PageTranscriptUp);
    let out = dispatch_global_key(key(KeyCode::PageDown), &mut state);
    assert_eq!(out, KeyDispatch::PageTranscriptDown);
    assert_eq!(
        dispatch_global_key(key(KeyCode::Home), &mut state),
        KeyDispatch::TranscriptHome
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::End), &mut state),
        KeyDispatch::TranscriptEnd
    );
    assert!(state.input.is_empty(), "PgUp/PgDn must not leak into input");

    dispatch_global_key(key(KeyCode::Char('x')), &mut state);
    assert_eq!(
        dispatch_global_key(key(KeyCode::PageUp), &mut state),
        KeyDispatch::PageTranscriptUp,
        "PageUp still scrolls transcript with a draft present"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::PageDown), &mut state),
        KeyDispatch::PageTranscriptDown,
        "PageDown still scrolls transcript with a draft present"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Home), &mut state),
        KeyDispatch::TranscriptHome,
        "Home still jumps transcript with a draft present"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::End), &mut state),
        KeyDispatch::TranscriptEnd,
        "End still jumps transcript with a draft present"
    );
    assert_eq!(state.input.text(), "x", "Page/Home/End must not edit the draft");
}

#[test]
fn dispatch_child_view_scroll_keys_only_when_child_focus_and_empty_input() {
    let mut state = TuiState::new("p", "m");
    state.focus = crate::chat::sessions::FocusTarget::Session { seq: 9 };

    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::ScrollSessionUp
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::ScrollSessionDown
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::PageUp), &mut state),
        KeyDispatch::PageSessionUp
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::PageDown), &mut state),
        KeyDispatch::PageSessionDown
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Home), &mut state),
        KeyDispatch::SessionHome
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::End), &mut state),
        KeyDispatch::SessionEnd
    );

    state.sessions_cache = vec![entry(9), entry(10)];
    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::ScrollSessionDown,
        "with bottom entries present, bare Down still scrolls child output"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::SwitchSession { seq: 10 },
        "bare Right switches child sessions"
    );
    state.sessions_cache.clear();

    state.focus = crate::chat::sessions::FocusTarget::Main;
    assert_eq!(
        dispatch_global_key(key(KeyCode::PageUp), &mut state),
        KeyDispatch::PageTranscriptUp,
        "main focus routes PgUp into transcript scrolling"
    );

    state.focus = crate::chat::sessions::FocusTarget::Transcript;
    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::ScrollSessionUp,
        "transcript focus must reuse child viewport scrolling"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::PageDown), &mut state),
        KeyDispatch::PageSessionDown,
        "transcript focus must support page scrolling"
    );
    let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
    assert_eq!(
        out,
        KeyDispatch::Consumed,
        "transcript focus is read-only and must not submit/steer"
    );

    state.focus = crate::chat::sessions::FocusTarget::Diff;
    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::ScrollSessionDown,
        "diff focus must reuse child viewport scrolling"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::PageUp), &mut state),
        KeyDispatch::PageSessionUp,
        "diff focus must support page scrolling"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Consumed,
        "diff focus is read-only and must not submit/steer"
    );

    state.focus = crate::chat::sessions::FocusTarget::Session { seq: 9 };
    dispatch_global_key(key(KeyCode::Char('x')), &mut state);
    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::Consumed,
        "non-empty input keeps Up editing/history semantics instead of child scroll"
    );
    assert_eq!(state.input.text(), "x");
    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::Consumed,
        "non-empty input keeps Down editing/history semantics instead of child scroll"
    );
    assert_eq!(state.input.text(), "x");
    assert_eq!(
        dispatch_global_key(key(KeyCode::PageDown), &mut state),
        KeyDispatch::PageSessionDown,
        "PageDown still scrolls child output with a draft present"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Home), &mut state),
        KeyDispatch::SessionHome,
        "Home still jumps child output with a draft present"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::End), &mut state),
        KeyDispatch::SessionEnd,
        "End still jumps child output with a draft present"
    );

    state.input.clear();
    state.focus = crate::chat::sessions::FocusTarget::Diff;
    dispatch_global_key(key(KeyCode::Char('z')), &mut state);
    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::Consumed,
        "diff+non-empty input also keeps edit/history semantics"
    );
    assert_eq!(state.input.text(), "z");
}

#[test]
fn dispatch_directional_session_switching_obeys_focus_input_matrix() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1), entry(2), entry(3)];

    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::AttachSession { seq: 1 },
        "main+empty Right immediately attaches the first bottom entry"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::ScrollTranscriptUp,
        "main+empty Up scrolls the transcript"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::ScrollTranscriptDown,
        "main+empty Down scrolls the transcript"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Left), &mut state),
        KeyDispatch::AttachSession { seq: 3 },
        "main+empty Left wraps from main to the last bottom entry"
    );
    assert_eq!(
        dispatch_global_key(key_repeat(KeyCode::Left), &mut state),
        KeyDispatch::Consumed,
        "held Left/Right repeats are debounced before synthetic attach"
    );

    state.focus = crate::chat::sessions::FocusTarget::Session { seq: 2 };
    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::SwitchSession { seq: 3 },
        "session+empty Right switches to the visual neighbor on the right"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::ScrollSessionUp,
        "session+empty Up scrolls the child viewport"
    );
    state.focus = crate::chat::sessions::FocusTarget::Session { seq: 1 };
    assert_eq!(
        dispatch_global_key(key(KeyCode::Left), &mut state),
        KeyDispatch::RequestDetach,
        "first child+empty Left switches back to main"
    );
    state.focus = crate::chat::sessions::FocusTarget::Session { seq: 2 };
    assert_eq!(
        dispatch_global_key(key_repeat(KeyCode::Right), &mut state),
        KeyDispatch::Consumed,
        "held Right repeat is debounced for focused sessions"
    );

    dispatch_global_key(key(KeyCode::Char('x')), &mut state);
    dispatch_global_key(key(KeyCode::Char('y')), &mut state);
    assert_eq!(state.input.cursor, (0, 2));
    assert_eq!(
        dispatch_global_key(key(KeyCode::Left), &mut state),
        KeyDispatch::Consumed,
        "session+non-empty Left must move the cursor, not switch sessions"
    );
    assert_eq!(state.input.text(), "xy");
    assert_eq!(state.input.cursor, (0, 1));
    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::Consumed,
        "session+non-empty Right must move the cursor, not switch sessions"
    );
    assert_eq!(state.input.cursor, (0, 2));

    state.input.clear();
    state.focus = crate::chat::sessions::FocusTarget::Transcript;
    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::SwitchSession { seq: 1 },
        "transcript focus can switch to bottom entries with Right"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Left), &mut state),
        KeyDispatch::SwitchSession { seq: 3 },
        "transcript focus can switch to bottom entries with Left"
    );

    state.focus = crate::chat::sessions::FocusTarget::Diff;
    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::SwitchSession { seq: 1 },
        "diff focus can switch to bottom entries with Right"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Left), &mut state),
        KeyDispatch::SwitchSession { seq: 3 },
        "diff focus can switch to bottom entries with Left"
    );

    dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
    assert!(state.switcher.is_some());
    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::Consumed,
        "switcher-open keys must not leak to directional session switching"
    );
}

#[test]
fn bottom_directional_switch_skips_completed_history_rows() {
    let mut state = TuiState::new("p", "m");
    let mut completed = entry(1);
    completed.status = "completed";
    state.sessions_cache = vec![completed, entry(2)];

    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::AttachSession { seq: 2 },
        "bare Right should attach the first active child, not completed history"
    );

    let mut history_only = TuiState::new("p", "m");
    let mut done = entry(1);
    done.status = "completed";
    history_only.sessions_cache = vec![done];

    assert_eq!(
        dispatch_global_key(key(KeyCode::Char('x')), &mut history_only),
        KeyDispatch::Consumed
    );
    assert_eq!(
        history_only.input.text(),
        "x",
        "history-only completed sessions must not swallow normal input"
    );
}

#[test]
fn dispatch_esc_returns_cancelled() {
    let mut state = TuiState::new("p", "m");
    for ch in "draft".chars() {
        dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
    }
    let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
    assert_eq!(out, KeyDispatch::Cancelled);
    assert!(state.input.is_empty(), "Esc clears the in-flight draft");
}
