#![cfg(test)]

use super::*;

#[test]
fn p2_10_enter_submits_and_clears() {
    let mut input = TuiInput::new();
    type_str(&mut input, "hello");
    assert_eq!(input.text(), "hello");
    let out = input.handle_key(key(KeyCode::Enter));
    assert_eq!(out, InputOutcome::Submitted("hello".to_string()));
    assert!(input.is_empty(), "buffer cleared after submit");
    // History got the entry.
    assert_eq!(input.history, vec!["hello".to_string()]);
}

#[test]
fn p2_10_enter_on_empty_buffer_is_consumed_no_submit() {
    let mut input = TuiInput::new();
    let out = input.handle_key(key(KeyCode::Enter));
    assert_eq!(out, InputOutcome::Consumed);
    assert!(input.history.is_empty(), "empty enter does not record");
}

#[test]
fn p2_10_shift_enter_inserts_newline() {
    let mut input = TuiInput::new();
    type_str(&mut input, "a");
    let out = input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
    assert_eq!(out, InputOutcome::Consumed);
    type_str(&mut input, "b");
    assert_eq!(input.text(), "a\nb");
    assert_eq!(input.lines.len(), 2);
    assert!(!input.is_single_line());
}

#[test]
fn p2_10_alt_enter_inserts_newline() {
    let mut input = TuiInput::new();
    type_str(&mut input, "a");
    let out = input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::ALT));
    assert_eq!(out, InputOutcome::Consumed);
    type_str(&mut input, "b");
    assert_eq!(input.text(), "a\nb");
    assert_eq!(input.lines.len(), 2);
    assert!(!input.is_single_line());
}

#[test]
fn p2_10_backslash_enter_continues_without_submitting() {
    let mut input = TuiInput::new();
    type_str(&mut input, "echo \\");
    let out = input.handle_key(key(KeyCode::Enter));
    assert_eq!(out, InputOutcome::Consumed);
    assert_eq!(input.text(), "echo \n");
    assert!(input.history.is_empty(), "continuation does not submit");
    type_str(&mut input, "next");
    let out = input.handle_key(key(KeyCode::Enter));
    assert_eq!(out, InputOutcome::Submitted("echo \nnext".to_string()));
}

#[test]
fn p2_10_cursor_moves_across_line_boundaries() {
    let mut input = TuiInput::new();
    type_str(&mut input, "ab");
    input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
    type_str(&mut input, "cd");
    // cursor at (1, 2). Move left twice → (1,0); once more → (0, 2).
    assert_eq!(input.cursor, (1, 2));
    input.handle_key(key(KeyCode::Left));
    input.handle_key(key(KeyCode::Left));
    assert_eq!(input.cursor, (1, 0));
    input.handle_key(key(KeyCode::Left));
    assert_eq!(input.cursor, (0, 2));
    // Move right → (1, 0).
    input.handle_key(key(KeyCode::Right));
    assert_eq!(input.cursor, (1, 0));
}

#[test]
fn p2_10_history_ctrl_p_recalls_last_submission() {
    let mut input = TuiInput::new();
    type_str(&mut input, "first");
    input.handle_key(key(KeyCode::Enter));
    type_str(&mut input, "second");
    input.handle_key(key(KeyCode::Enter));
    let out = input.handle_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(out, InputOutcome::Consumed);
    assert_eq!(input.text(), "second");
    input.handle_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(input.text(), "first");
    input.handle_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(input.text(), "second");
    input.handle_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert!(input.is_empty(), "back to fresh draft");
}

#[test]
fn p2_10_history_preserves_in_flight_draft() {
    let mut input = TuiInput::new();
    type_str(&mut input, "old");
    input.handle_key(key(KeyCode::Enter));
    // Start typing a new draft, then navigate back through history and
    // forward to the draft with Ctrl+P/N.
    type_str(&mut input, "wip");
    input.handle_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(input.text(), "old");
    input.handle_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(input.text(), "wip", "draft restored");
}

#[test]
fn p2_10_history_dedups_consecutive_duplicates() {
    let mut input = TuiInput::new();
    type_str(&mut input, "x");
    input.handle_key(key(KeyCode::Enter));
    type_str(&mut input, "x");
    input.handle_key(key(KeyCode::Enter));
    assert_eq!(input.history.len(), 1, "consecutive dupes collapsed");
}

#[test]
fn p2_10_backspace_at_line_start_merges_lines() {
    let mut input = TuiInput::new();
    type_str(&mut input, "ab");
    input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
    type_str(&mut input, "cd");
    // cursor at (1, 2). Move to start of line 1.
    input.handle_key(key(KeyCode::Home));
    assert_eq!(input.cursor, (1, 0));
    input.handle_key(key(KeyCode::Backspace));
    assert_eq!(input.text(), "abcd");
    assert_eq!(input.cursor, (0, 2), "cursor at original line-1 start");
    assert!(input.is_single_line());
}

#[test]
fn p2_10_backspace_inside_line_removes_one_char() {
    let mut input = TuiInput::new();
    type_str(&mut input, "abc");
    input.handle_key(key(KeyCode::Backspace));
    assert_eq!(input.text(), "ab");
    assert_eq!(input.cursor, (0, 2));
}

#[test]
fn p2_10_ctrl_u_kills_to_line_start() {
    let mut input = TuiInput::new();
    type_str(&mut input, "hello world");
    // Move left 5 chars → cursor at index 6.
    for _ in 0..5 {
        input.handle_key(key(KeyCode::Left));
    }
    assert_eq!(input.cursor, (0, 6));
    let out = input.handle_key(key_mod(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(out, InputOutcome::Consumed);
    assert_eq!(input.text(), "world");
    assert_eq!(input.cursor, (0, 0));
}

#[test]
fn p2_10_ctrl_a_and_ctrl_e_jump_line_endpoints() {
    let mut input = TuiInput::new();
    type_str(&mut input, "abc");
    input.handle_key(key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(input.cursor, (0, 0));
    input.handle_key(key_mod(KeyCode::Char('e'), KeyModifiers::CONTROL));
    assert_eq!(input.cursor, (0, 3));
}

#[test]
fn p2_10_paste_with_newlines_creates_multiple_lines() {
    let mut input = TuiInput::new();
    input.paste("alpha\nbeta\ngamma");
    assert_eq!(input.lines.len(), 3);
    assert_eq!(input.text(), "alpha\nbeta\ngamma");
    // Cursor lands at end of pasted content.
    assert_eq!(input.cursor, (2, 5));
    // Now ↑ should NOT recall history (we have a multi-line buffer).
    let out = input.handle_key(key(KeyCode::Up));
    assert_eq!(out, InputOutcome::Consumed);
    // Cursor moved up one row, not to history.
    assert_eq!(input.cursor.0, 1);
}

#[test]
fn p2_10_large_paste_100kb_is_bounded_and_recoverable() {
    let mut input = TuiInput::new();
    let line = "a".repeat(1024);
    let pasted = std::iter::repeat_n(line.as_str(), 100).collect::<Vec<_>>().join("\n");

    input.paste(&pasted);

    assert_eq!(pasted.len(), 102_499);
    assert_eq!(input.byte_len(), INPUT_MAX_BYTES);
    assert!(input.truncated, "input should report ignored overflow bytes");
    assert!(pasted.starts_with(&input.text()));

    let out = input.handle_key(key(KeyCode::Enter));
    match out {
        InputOutcome::Submitted(submitted) => {
            assert_eq!(submitted.len(), INPUT_MAX_BYTES);
            assert_eq!(input.history, vec![submitted]);
        }
        other => panic!("expected bounded large input to submit, got {other:?}"),
    }
    assert!(input.is_empty(), "buffer cleared after large submit");
}

#[test]
fn p2_10_key_flood_stops_at_input_cap_and_ctrl_u_recovers() {
    let mut input = TuiInput::new();
    for _ in 0..(INPUT_MAX_BYTES + 1024) {
        input.handle_key(key(KeyCode::Char('p')));
    }

    assert_eq!(input.byte_len(), INPUT_MAX_BYTES);
    assert!(input.truncated);
    assert_eq!(input.cursor, (0, INPUT_MAX_BYTES));

    let out = input.handle_key(key_mod(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(out, InputOutcome::Consumed);
    assert!(input.is_empty());
    assert!(!input.truncated);
}

#[test]
fn p2_10_over_cap_plain_key_is_ignored_without_redraw() {
    let mut state = TuiState::new("provider", "model");
    state.input.paste(&"p".repeat(INPUT_MAX_BYTES));

    let out = dispatch_global_key(key(KeyCode::Char('p')), &mut state);

    assert_eq!(out, KeyDispatch::Ignored);
    assert_eq!(state.input.byte_len(), INPUT_MAX_BYTES);
    assert!(state.input.truncated);
}

#[test]
fn p2_10_multi_line_up_down_moves_cursor_not_history() {
    let mut input = TuiInput::new();
    type_str(&mut input, "x");
    input.handle_key(key(KeyCode::Enter)); // submits "x", history = ["x"]
    type_str(&mut input, "row1");
    input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
    type_str(&mut input, "row2");
    // Multi-line: ↑ should move cursor, not recall "x".
    input.handle_key(key(KeyCode::Up));
    assert_eq!(input.text(), "row1\nrow2", "still editing same draft");
    assert_eq!(input.cursor.0, 0);
}

#[test]
fn p2_10_esc_clears_buffer_and_returns_cancelled() {
    let mut input = TuiInput::new();
    type_str(&mut input, "garbage");
    let out = input.handle_key(key(KeyCode::Esc));
    assert_eq!(out, InputOutcome::Cancelled);
    assert!(input.is_empty());
    // Esc on empty still returns Cancelled.
    let out2 = input.handle_key(key(KeyCode::Esc));
    assert_eq!(out2, InputOutcome::Cancelled);
}

#[test]
fn p2_10_delete_forward_joins_next_line() {
    let mut input = TuiInput::new();
    type_str(&mut input, "ab");
    input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
    type_str(&mut input, "cd");
    input.handle_key(key(KeyCode::Up));
    input.handle_key(key(KeyCode::End));
    assert_eq!(input.cursor, (0, 2));
    input.handle_key(key(KeyCode::Delete));
    assert_eq!(input.text(), "abcd");
    assert_eq!(input.lines.len(), 1);
}

#[test]
fn input_pageup_pagedown_fall_through_to_outer_loop() {
    // Fullscreen transcript scroll is handled by the outer TUI loop.
    // The input subsystem must report these keys as Unhandled.
    let mut input = TuiInput::new();
    assert_eq!(input.handle_key(key(KeyCode::PageUp)), InputOutcome::Unhandled);
    assert_eq!(input.handle_key(key(KeyCode::PageDown)), InputOutcome::Unhandled);
}

#[test]
fn p2_10_utf8_grapheme_safe_backspace() {
    let mut input = TuiInput::new();
    // Three East Asian wide chars (3 bytes each in UTF-8).
    type_str(&mut input, "\u{ac00}\u{ac01}\u{ac02}");
    assert_eq!(input.cursor, (0, 9));
    input.handle_key(key(KeyCode::Backspace));
    assert_eq!(input.text(), "\u{ac00}\u{ac01}");
    assert_eq!(input.cursor, (0, 6));
    // Move left then right: must land on char boundaries.
    input.handle_key(key(KeyCode::Left));
    assert_eq!(input.cursor, (0, 3));
    input.handle_key(key(KeyCode::Right));
    assert_eq!(input.cursor, (0, 6));
}

#[test]
fn p2_10_tui_state_routes_keys_to_input() {
    let mut state = TuiState::new("p", "m");
    let out = state.handle_input_key(key(KeyCode::Char('z')));
    assert_eq!(out, InputOutcome::Consumed);
    assert_eq!(state.input.text(), "z");
}

#[test]
fn slash_menu_opens_when_input_starts_with_slash() {
    let mut state = TuiState::new("p", "m");

    let out = dispatch_global_key(key(KeyCode::Char('/')), &mut state);

    assert_eq!(out, KeyDispatch::Consumed);
    let menu = state.slash_menu.as_ref().expect("slash menu open");
    assert!(menu.len() >= 30, "registry-backed menu should include all commands");
    assert!(menu.entries.iter().any(|entry| entry.label == "/help"));
    assert_eq!(state.input.text(), "/");
}

#[test]
fn slash_menu_filters_from_command_token() {
    let mut state = TuiState::new("p", "m");
    for ch in "/mo".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    let menu = state.slash_menu.as_ref().expect("slash menu open");
    assert_eq!(menu.filter, "mo");
    assert!(menu.entries.iter().any(|entry| entry.label == "/model"));
    assert!(
        menu.entries
            .iter()
            .all(|entry| entry.label.trim_start_matches('/').contains("mo")),
        "U5 /mo filter should only include matching command names: {:?}",
        menu.entries
    );
}

#[test]
fn slash_menu_ignores_description_only_matches() {
    let mut state = TuiState::new("p", "m");
    for ch in "/conversation".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    assert_eq!(state.input.text(), "/conversation");
    assert!(
        state.slash_menu.is_none(),
        "description-only matches must not keep slash menu open"
    );
}

#[test]
fn slash_menu_closes_when_filter_has_no_matches() {
    let mut state = TuiState::new("p", "m");
    for ch in "/zzzz".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    assert_eq!(state.input.text(), "/zzzz");
    assert!(state.slash_menu.is_none(), "no matching overlay must close");
}

#[test]
fn slash_menu_only_triggers_at_first_line_start() {
    let mut state = TuiState::new("p", "m");
    state.input.lines = vec!["open".to_string(), String::new()];
    state.input.cursor = (1, 0);
    for ch in "/he".chars() {
        let _ = dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
    }

    assert_eq!(state.input.text(), "open\n/he");
    assert!(
        state.slash_menu.is_none(),
        "second-line slash command prefix with matches must not open slash menu"
    );
}

#[test]
fn slash_menu_overlay_rect_stays_above_bottom_chrome() {
    let frame = Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 30,
    };
    let bottom_chrome_height = 5;
    let menu = SlashMenuState::new("");

    let rect = slash_menu_overlay_rect(frame, &menu, bottom_chrome_height);

    assert!(rect.width <= 80, "slash menu width should be capped: {rect:?}");
    assert_eq!(rect.x, 2, "slash menu should keep a horizontal margin");
    assert!(
        rect.y.saturating_add(rect.height) <= frame.height.saturating_sub(bottom_chrome_height),
        "slash menu should sit above bottom chrome: {rect:?}"
    );
    assert!(rect.height >= 1, "slash menu should remain visible: {rect:?}");
}

#[test]
fn slash_menu_navigation_and_enter_insert_command_name() {
    let mut state = TuiState::new("p", "m");
    for ch in "/".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }
    let first = state.slash_menu.as_ref().expect("slash menu open").selected;

    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::Consumed
    );
    let moved = state.slash_menu.as_ref().expect("slash menu still open").selected;
    assert_ne!(first, moved, "Down moves slash menu selection");
    assert_eq!(dispatch_global_key(key(KeyCode::Up), &mut state), KeyDispatch::Consumed);
    assert_eq!(
        state.slash_menu.as_ref().expect("slash menu still open").selected,
        first,
        "Up moves selection back"
    );

    for ch in "mo".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }
    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "/model ");
    assert_eq!(state.input.cursor, (0, "/model ".len()));
    assert!(state.slash_menu.is_none(), "selecting closes slash menu");
}

#[test]
fn slash_menu_enter_submits_no_arg_command() {
    let mut state = TuiState::new("p", "m");
    for ch in "/he".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Submitted("/help".to_string())
    );
    assert!(state.input.is_empty());
    assert!(state.slash_menu.is_none());
}

#[test]
fn slash_menu_tab_inserts_command_and_esc_clears_command_trigger() {
    let mut tab_state = TuiState::new("p", "m");
    for ch in "/expo".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut tab_state),
            KeyDispatch::Consumed
        );
    }
    assert_eq!(
        dispatch_global_key(key(KeyCode::Tab), &mut tab_state),
        KeyDispatch::Consumed
    );
    assert_eq!(tab_state.input.text(), "/export ");
    assert!(
        tab_state
            .slash_menu
            .as_ref()
            .is_some_and(|menu| menu.entries.iter().any(|entry| entry.label == "json")),
        "selecting /export should drill down to format candidates"
    );

    let mut esc_state = TuiState::new("p", "m");
    for ch in "/mo".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut esc_state),
            KeyDispatch::Consumed
        );
    }
    assert_eq!(
        dispatch_global_key(key(KeyCode::Esc), &mut esc_state),
        KeyDispatch::Consumed
    );
    assert!(esc_state.input.is_empty(), "Esc clears slash-command draft");
    assert!(esc_state.slash_menu.is_none(), "Esc closes menu");
}

#[test]
fn slash_command_submit_leaves_input_render_empty() {
    let mut state = TuiState::new("p", "m");
    for ch in "/help".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Submitted("/help".to_string())
    );
    assert!(state.input.is_empty());
    assert_eq!(state.input.display_lines(), vec![String::new()]);

    let mut scroll = FullscreenTranscriptScroll::default();
    let rows = fullscreen_rows(&state, 90, 24, &mut scroll);
    let joined = rows.join("\n");
    assert!(
        !rows.iter().any(|row| row.trim_start().starts_with("> /help")),
        "submitted slash command must not remain as bright input text: {joined}"
    );
}

#[test]
fn input_history_moves_to_ctrl_p_n_when_slash_menu_closed() {
    let mut state = TuiState::new("p", "m");
    state.input.history.push("older command".to_string());

    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::ScrollTranscriptUp
    );
    assert!(state.input.is_empty());
    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::ScrollTranscriptDown
    );
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "older command");
    assert!(state.slash_menu.is_none());
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert!(state.input.is_empty());

    state.sessions_cache = vec![entry(1)];
    state.input.history.push("newer command".to_string());
    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::ScrollTranscriptDown
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Right), &mut state),
        KeyDispatch::AttachSession { seq: 1 }
    );
    assert!(state.input.is_empty());
}

#[test]
fn fullscreen_slash_menu_renders_as_overlay_with_filtered_command() {
    let mut state = TuiState::new("provider", "model");
    state.slash_menu = Some(SlashMenuState::new("mo"));
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 80, 24, &mut scroll);

    assert!(
        rows.iter().any(|row| row.contains("Slash commands")),
        "slash menu title rendered: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("/model")),
        "filtered command rendered: {rows:?}"
    );
}

#[test]
fn slash_menu_export_arg_shows_static_candidates() {
    let mut state = TuiState::new("p", "m");
    for ch in "/export ".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    let menu = state.slash_menu.as_ref().expect("export arg menu open");
    assert!(menu.entries.iter().any(|entry| entry.label == "md"));
    assert!(menu.entries.iter().any(|entry| entry.label == "json"));
}

#[test]
fn slash_menu_free_text_command_has_no_second_level_menu() {
    let mut state = TuiState::new("p", "m");
    for ch in "/bg ".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    assert!(state.slash_menu.is_none(), "free-text /bg should not force candidates");
}

#[test]
fn at_path_menu_opens_at_word_start_and_inserts_file_with_space() {
    let mut state = TuiState::new("p", "m");
    state.input.set_text("inspect @ca");

    state.update_at_path_candidates(vec![AtPathCandidate {
        path: "Cargo.toml".to_string(),
        is_dir: false,
    }]);

    let menu = state.slash_menu.as_ref().expect("@path menu open");
    assert_eq!(menu.filter, "ca");
    assert_eq!(
        menu.entries.first().map(|entry| entry.label.as_str()),
        Some("Cargo.toml")
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "inspect @Cargo.toml ");
    assert_eq!(state.input.cursor, (0, "inspect @Cargo.toml ".len()));
}

#[test]
fn at_path_menu_keeps_directory_candidate_open_for_drilldown() {
    let mut state = TuiState::new("p", "m");
    state.input.set_text("@s");

    state.update_at_path_candidates(vec![AtPathCandidate {
        path: "src/".to_string(),
        is_dir: true,
    }]);

    assert_eq!(
        dispatch_global_key(key(KeyCode::Tab), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "@src/");
    assert!(
        state.input.text().ends_with('/'),
        "directory completion must not append a separating space"
    );
}

#[test]
fn at_path_menu_requires_word_start() {
    let mut state = TuiState::new("p", "m");
    state.input.set_text("mail@example");

    state.update_at_path_candidates(vec![AtPathCandidate {
        path: "example.rs".to_string(),
        is_dir: false,
    }]);

    assert!(state.slash_menu.is_none(), "email-like @ must not open path menu");
}

#[test]
fn slash_menu_kill_arg_uses_live_session_cache() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![crate::chat::sessions::SwitcherEntry {
        seq: 7,
        kind: "agent",
        origin: "user",
        status: "running",
        title: "build release".to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
        idle_warning: false,
    }];
    for ch in "/kill ".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    let menu = state.slash_menu.as_ref().expect("kill arg menu open");
    assert!(
        menu.entries
            .iter()
            .any(|entry| entry.label == "#7" && entry.description.contains("build release")),
        "live session row rendered from cache: {:?}",
        menu.entries
    );
}

#[test]
fn slash_menu_provider_and_model_candidates_degrade_from_catalog() {
    let mut state = TuiState::new("openai", "gpt-5.2");
    state.provider_model_catalog = vec![SlashProviderModelCatalog {
        provider: "openai".to_string(),
        models: vec![SlashModelCandidate {
            name: "gpt-5.2".to_string(),
            description: "Configured default model".to_string(),
        }],
    }];
    for ch in "/provider ".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }
    assert!(
        state
            .slash_menu
            .as_ref()
            .expect("provider menu open")
            .entries
            .iter()
            .any(|entry| entry.label == "openai")
    );

    for ch in "openai ".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }
    let menu = state.slash_menu.as_ref().expect("provider model menu open");
    assert!(menu.entries.iter().any(|entry| entry.label == "gpt-5.2"));
}

#[test]
fn slash_menu_model_arg_uses_current_provider_models() {
    let mut state = TuiState::new("openai", "gpt-5.2");
    state.provider_model_catalog = vec![SlashProviderModelCatalog {
        provider: "openai".to_string(),
        models: vec![SlashModelCandidate {
            name: "gpt-5-mini".to_string(),
            description: "Router model candidate".to_string(),
        }],
    }];
    for ch in "/model ".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    let menu = state.slash_menu.as_ref().expect("model arg menu open");
    assert!(menu.entries.iter().any(|entry| entry.label == "gpt-5-mini"));
}

#[test]
fn slash_menu_enter_inserts_argument_candidate_token() {
    let mut state = TuiState::new("p", "m");
    for ch in "/export j".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "/export json ");
    assert!(state.slash_menu.is_none());
}

#[test]
fn slash_menu_enter_inserts_argument_candidate_at_trailing_space() {
    let mut state = TuiState::new("p", "m");
    for ch in "/export ".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    let menu = state.slash_menu.as_ref().expect("export arg menu open");
    assert_eq!(menu.selected_entry().map(|entry| entry.label.as_str()), Some("md"));
    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "/export md ");
    assert!(state.slash_menu.is_none());
}

#[test]
fn slash_menu_down_enter_inserts_argument_candidate_at_trailing_space() {
    let mut state = TuiState::new("p", "m");
    for ch in "/export ".chars() {
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
            KeyDispatch::Consumed
        );
    }

    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::Consumed
    );
    let menu = state.slash_menu.as_ref().expect("export arg menu open");
    assert_eq!(menu.selected_entry().map(|entry| entry.label.as_str()), Some("json"));
    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "/export json ");
    assert!(state.slash_menu.is_none());
}

#[test]
fn input_history_edit_then_ctrl_p_n_steps_clean_entries() {
    let mut state = TuiState::new("p", "m");
    state.input.history.push("one".to_string());
    state.input.history.push("two".to_string());

    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "two");
    assert_eq!(
        dispatch_global_key(key(KeyCode::Char('!')), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "two!");
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "two", "Ctrl+P restarts from a clean history entry");
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "one");
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "two");
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "two!", "Ctrl+N restores the edited draft");
}

#[test]
fn p2_10_history_capped_at_capacity() {
    let mut input = TuiInput::new();
    for i in 0..(INPUT_HISTORY_CAPACITY + 5) {
        type_str(&mut input, &format!("e{i}"));
        input.handle_key(key(KeyCode::Enter));
    }
    assert_eq!(input.history.len(), INPUT_HISTORY_CAPACITY);
    // Oldest entry was dropped.
    assert!(!input.history.contains(&"e0".to_string()));
}
