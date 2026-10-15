#![cfg(test)]

use super::*;

// ── P3 chat TUI rearch tests (2026-05-13) ────────────────────────────
//
// These pin the contract relied on by the new unified TUI loop in
// `chat/mod.rs::run_tui_unified_loop`:
//   1. `TuiInput::paste` splits multi-line text into the `lines` Vec
//      and lands the cursor at the end of the last pasted row.
//   2. `push_system_message` appends a `ConversationLine::System` —
//      banner + slash-command output go through this on the TUI path.
//   3. `dispatch_global_key` accepts CJK characters via plain
//      `KeyCode::Char(_)` (bracketed-paste decodes them, but unicode
//      chars also flow through as KeyEvents on graphical terminals).

#[test]
fn paste_with_newline_splits_into_rows_and_lands_cursor_at_end() {
    let mut input = TuiInput::new();
    input.paste("hello\nworld");
    assert_eq!(input.lines, vec!["hello".to_string(), "world".to_string()]);
    // Cursor lands at the end of the last pasted row.
    assert_eq!(input.cursor, (1, "world".len()));
}

#[test]
fn large_paste_folds_to_chip_but_submits_original_text() {
    let mut input = TuiInput::new();
    let pasted = "one\ntwo\nthree\nfour\nfive\nsix";

    input.paste(pasted);

    assert_eq!(input.display_lines(), vec!["[Pasted text #1: 6 lines]".to_string()]);
    assert_eq!(input.text(), pasted);
    match input.handle_key(key(KeyCode::Enter)) {
        InputOutcome::Submitted(submitted) => assert_eq!(submitted, pasted),
        other => panic!("expected folded paste to submit, got {other:?}"),
    }
}

#[test]
fn folded_paste_chips_increment_and_restore_through_history_navigation() {
    let mut input = TuiInput::new();
    input.record_history("older".to_string());
    let first = "a\nb\nc\nd\ne\nf";
    let second = "x".repeat(PASTE_FOLD_BYTE_THRESHOLD + 1);

    input.paste(first);
    input.paste(" ");
    input.paste(&second);

    assert_eq!(
        input.display_lines(),
        vec!["[Pasted text #1: 6 lines] [Pasted text #2: 1 lines]"]
    );
    assert_eq!(input.text(), format!("{first} {second}"));
    assert!(input.history_prev(), "moves to history");
    assert_eq!(input.text(), "older");
    assert!(input.history_next(), "restores draft");
    assert_eq!(
        input.display_lines(),
        vec!["[Pasted text #1: 6 lines] [Pasted text #2: 1 lines]"]
    );
    assert_eq!(input.text(), format!("{first} {second}"));
}

#[test]
fn folded_paste_backspace_removes_whole_chip_without_placeholder_leak() {
    let mut input = TuiInput::new();
    let pasted = "one\ntwo\nthree\nfour\nfive\nsix";
    input.paste(pasted);

    input.handle_key(key(KeyCode::Backspace));

    assert_eq!(input.text(), "");
    assert_eq!(input.display_lines(), vec![String::new()]);
    assert!(
        !input.text().contains("[Pasted text"),
        "damaged placeholder must never leak into submitted payload"
    );
}

#[test]
fn folded_paste_cursor_inside_chip_cannot_insert_into_chip() {
    let mut input = TuiInput::new();
    let pasted = "one\ntwo\nthree\nfour\nfive\nsix";
    input.paste(pasted);
    input.cursor = (0, 1);

    assert!(input.insert_char('X'));

    assert_eq!(input.text(), format!("{pasted}X"));
    assert_eq!(input.display_lines(), vec!["[Pasted text #1: 6 lines]X".to_string()]);
}

#[test]
fn small_paste_matching_chip_placeholder_is_not_expanded() {
    let mut input = TuiInput::new();
    let literal = "[Pasted text #1: 6 lines]";

    input.paste(literal);

    assert_eq!(input.lines, vec![literal.to_string()]);
    assert_eq!(input.text(), literal);
}

#[test]
fn paste_into_existing_buffer_preserves_suffix() {
    let mut input = TuiInput::new();
    type_str(&mut input, "abXY");
    // Move cursor between 'b' and 'X'.
    input.handle_key(key(KeyCode::Left));
    input.handle_key(key(KeyCode::Left));
    assert_eq!(input.cursor, (0, 2));
    input.paste("1\n2");
    // After paste: row 0 = "ab1", row 1 = "2XY" (suffix moved down).
    assert_eq!(input.lines, vec!["ab1".to_string(), "2XY".to_string()]);
    // Cursor lands after "2", before "XY".
    assert_eq!(input.cursor, (1, 1));
}

#[test]
fn render_input_scrolls_visible_window_to_cursor() {
    let mut state = TuiState::new("p", "m");
    state.input.lines = (0..15).map(|idx| format!("scroll-line-{idx:02}")).collect();
    state.input.cursor = (14, "scroll-line-14".len());
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 90, 24, &mut scroll);
    let joined = rows.join("\n");

    assert!(
        joined.contains("scroll-line-14"),
        "cursor row should be visible: {joined}"
    );
    assert!(
        !joined.contains("scroll-line-00"),
        "top rows should scroll out once cursor is below visible input window: {joined}"
    );
}

#[test]
fn input_wrap_ranges_respect_unicode_display_width() {
    let line = "ab\u{ac00}\u{ac01}c";
    let ranges = wrap_line_ranges(line, 4);
    let wrapped = ranges
        .iter()
        .map(|(start, end)| line.get(*start..*end).unwrap_or(""))
        .collect::<Vec<_>>();

    assert_eq!(wrapped, vec!["ab\u{ac00}", "\u{ac01}c"]);
    assert!(
        wrapped.iter().all(|row| UnicodeWidthStr::width(*row) <= 4),
        "wrapped rows fit display width: {wrapped:?}"
    );
}

#[test]
fn long_single_input_line_uses_wrapped_chrome_height() {
    let mut state = TuiState::new("p", "m");
    state.input.set_text(&"abcdefghij".repeat(8));

    let narrow = fullscreen_bottom_chrome_height_for_width(&state, 16);
    let wide = fullscreen_bottom_chrome_height_for_width(&state, 120);

    assert!(narrow > wide, "narrow input should reserve wrapped rows");
    assert!(narrow <= BOTTOM_CHROME_MAX_HEIGHT);
}

#[test]
fn render_input_soft_wrap_scrolls_to_cursor_tail() {
    let mut state = TuiState::new("p", "m");
    let text = format!("HEAD-{}-TAIL", "abcdefghij".repeat(20));
    state.input.set_text(&text);
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 14, 24, &mut scroll);
    let joined = rows.join("\n");

    assert!(
        joined.contains("TAIL"),
        "wrapped cursor tail should be visible: {joined}"
    );
    assert!(
        !joined.contains("HEAD"),
        "wrapped input should scroll old visual rows out when cursor is at tail: {joined}"
    );
}

#[test]
fn fullscreen_footer_advertises_copy_paths() {
    let state = TuiState::new("p", "m");
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 180, 24, &mut scroll);
    let joined = rows.join("\n");

    assert!(
        joined.contains("/copy latest"),
        "footer should advertise /copy: {joined}"
    );
    assert!(
        joined.contains("drag select/copy"),
        "footer should advertise native selection: {joined}"
    );
}

#[test]
fn fullscreen_footer_becomes_session_list_when_sessions_exist() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1)];
    state.sessions_status = "sessions: 1 running".to_string();
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 100, 24, &mut scroll);
    let joined = rows.join("\n");
    let bottom = rows.iter().find(|row| row.contains("#1")).cloned().unwrap_or_default();

    assert!(
        joined.contains("main chat"),
        "bottom list should include the main session: {rows:?}"
    );
    assert!(bottom.contains("#1"), "bottom list should show session seq: {rows:?}");
    assert!(
        bottom.contains("agent"),
        "bottom list should show session kind: {rows:?}"
    );
    assert!(
        bottom.contains("0 tok"),
        "bottom list should show cumulative token usage: {rows:?}"
    );
    assert!(
        bottom.contains("task 1"),
        "bottom list should show the running task title, not shortcut-only chrome: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("←/→ session")),
        "session list footer should expose navigation hint: {rows:?}"
    );
    assert!(
        !bottom.contains("Ctrl+G sessions"),
        "session list should replace shortcut footer while sessions exist: {rows:?}"
    );
    assert_eq!(
        joined.matches("#1").count(),
        1,
        "session should render once below input, never duplicated above it: {rows:?}"
    );
    assert!(
        UNICODE_SPINNER_FRAMES.iter().any(|frame| joined.contains(frame)) && !joined.contains('✓'),
        "running session list rows should use animated spinner without terminal success icon: {rows:?}"
    );
}

#[test]
fn fullscreen_footer_hides_completed_sessions_from_active_bottom_list() {
    let mut state = TuiState::new("p", "m");
    let mut done = entry(1);
    done.status = "completed";
    state.sessions_cache = vec![done];
    state.sessions_status = "sessions: 1 completed".to_string();
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 100, 24, &mut scroll);
    let joined = rows.join("\n");

    assert!(
        !joined.contains("#1"),
        "completed child sessions should leave the active bottom list: {rows:?}"
    );
    assert!(
        joined.contains("Ctrl+G sessions"),
        "history-only sessions should restore the normal footer: {rows:?}"
    );
}

#[test]
fn fullscreen_footer_keeps_focused_completed_session_until_detach() {
    let mut state = TuiState::new("p", "m");
    let mut done = entry(1);
    done.status = "completed";
    state.sessions_cache = vec![done];
    state.sessions_status = "sessions: 1 completed".to_string();
    state.focus = crate::chat::sessions::FocusTarget::Session { seq: 1 };
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 100, 24, &mut scroll);
    let joined = rows.join("\n");

    assert!(
        joined.contains("#1") && joined.contains("completed"),
        "the focused terminal child remains visible until the user detaches: {rows:?}"
    );
}

#[test]
fn push_system_message_appends_system_conversation_line() {
    let mut state = TuiState::new("p", "m");
    state.push_system_message("test banner");
    let last = state
        .conversation_lines
        .last()
        .expect("test: conversation_lines non-empty after push_system_message");
    match last {
        ConversationLine::System { content } => assert_eq!(content, "test banner"),
        other => panic!("test: expected ConversationLine::System, got {other:?}"),
    }
    // Pushing system messages must not bump the *user* turn counter
    // (that drives the status-bar "N turns" display).
    assert_eq!(state.turn_count, 0);
}

#[test]
fn execution_activity_active_tracks_streaming_and_running_tools() {
    let mut state = TuiState::new("p", "m");
    assert!(!state.execution_activity_active());

    state.start_stream("d1");
    assert!(state.execution_activity_active());
    state.cancel_stream("d1");
    assert!(!state.execution_activity_active());

    state.conversation_lines.push(ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: String::new(),
        args_full: String::new(),
        result: None,
        status: ToolStatus::Running,
        elapsed_ms: None,
        folded: false,
    });
    assert!(state.execution_activity_active());
}

#[test]
fn dispatch_cjk_character_lands_in_input_buffer() {
    // Bracketed paste handles long IME commits, but graphical
    // terminals also deliver single CJK chars as `KeyCode::Char(_)`
    // KeyEvents. Verify the dispatcher does not swallow them.
    let mut state = TuiState::new("p", "m");
    for ch in "\u{ac00}\u{ac01}".chars() {
        let out = dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
    }
    assert_eq!(state.input.text(), "\u{ac00}\u{ac01}");
}

#[test]
fn measure_wrapped_rows_matches_ratatui_wordwrap() {
    // `measure_wrapped_rows` must return the EXACT number of rows
    // ratatui's word-wrapping produces (not the char-count upper bound
    // from `wrapped_rows_for_lines`). The streaming-preview scroll math
    // depends on this: an over-count scrolls past the newest tokens and
    // leaves the visible body truncated (chat-demo defect #1).
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let samples = [
        "I am a lightweight AI assistant built with Rust that can help you run terminal commands, read and write files, and manage projects.",
        "\u{ac00}\u{ac01},\u{ac02}\u{ac03}\u{ac04}\u{ac05} Rust \u{ac06}\u{ac07}\u{ac08} AI \u{ac09}\u{ac0a},\u{ac0b}\u{ac0c}\u{ac0d}\u{ac0e}.",
        "short",
        "a b c d e f g h i j k l m n o p q r s t u v w x y z one two three",
    ];
    for s in samples {
        let lines = vec![Line::from(s.to_string())];
        for width in [20u16, 40, 80, 191] {
            let measured = measure_wrapped_rows(&lines, width);
            // Ground truth: render at a generous height and read back the
            // last populated row.
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: 64,
            };
            let mut buf = Buffer::empty(area);
            Paragraph::new(Text::from(lines.clone()))
                .wrap(Wrap { trim: false })
                .render(area, &mut buf);
            let mut actual = 0u16;
            for y in 0..area.height {
                for x in 0..width {
                    if let Some(c) = buf.cell((x, y)) {
                        if !c.symbol().trim().is_empty() {
                            actual = y + 1;
                            break;
                        }
                    }
                }
            }
            let actual = actual.max(1);
            assert_eq!(
                measured, actual,
                "measure_wrapped_rows({width}) on {s:?}: got {measured}, ratatui rendered {actual}"
            );
        }
    }
}

#[test]
fn streaming_preview_scroll_keeps_tail_visible() {
    // Regression for chat-demo defect #1: a streaming body longer than
    // the preview window must scroll so the LAST rows (newest tokens)
    // are the ones rendered. We assert the scroll offset derived from
    // `measure_wrapped_rows` lands the tail inside the window.
    let body: String = (0..30).map(|i| format!("line number {i}\n")).collect();
    let cursor = "\u{258C}";
    let mut body_lines: Vec<&str> = body.lines().collect();
    if body.ends_with('\n') {
        body_lines.push("");
    }
    let last_idx = body_lines.len().saturating_sub(1);
    let mut sink: Vec<Line<'_>> = Vec::new();
    for (i, t) in body_lines.iter().enumerate() {
        if i == last_idx {
            sink.push(Line::from(format!("{t}{cursor}")));
        } else {
            sink.push(Line::from((*t).to_string()));
        }
    }
    let width = 80u16;
    let window = 6;
    let total = measure_wrapped_rows(&sink, width);
    assert!(total > window, "test setup: body must overflow the window");
    let scroll = total.saturating_sub(window);
    // After scrolling, the visible window [scroll, scroll+window) must
    // include the final content row (total-1, since the trailing cursor
    // row is the newest output).
    let last_content_row = total.saturating_sub(1);
    assert!(
        (scroll..scroll.saturating_add(window)).contains(&last_content_row),
        "scroll {scroll} + window {window} must cover last row {last_content_row} of {total}"
    );
}

#[test]
fn fullscreen_bottom_chrome_height_expands_with_input_not_streaming() {
    // Resting state: transcript gap (1) + status (1) + input border (1) +
    // input row (1) + footer (1) = 5. Streaming is rendered at the
    // transcript tail, not in bottom chrome. A long multi-line input adds
    // rows up to the visible cap.
    let mut state = TuiState::new("p", "m");
    let idle = fullscreen_bottom_chrome_height(&state);
    assert!(idle >= BOTTOM_CHROME_MIN_HEIGHT);
    assert!(idle <= BOTTOM_CHROME_MAX_HEIGHT);

    state.start_stream("d-live");
    let streaming = fullscreen_bottom_chrome_height(&state);
    assert_eq!(streaming, idle, "streaming must not duplicate into fullscreen chrome");

    // Multi-line input drives growth too (until clamped).
    state.cancel_stream("d-live");
    for _ in 0..6 {
        state.input.lines.push(String::new());
    }
    let tall = fullscreen_bottom_chrome_height(&state);
    assert!(tall > idle, "multi-line input must add rows: tall={tall}, idle={idle}");
    assert!(
        tall <= BOTTOM_CHROME_MAX_HEIGHT,
        "must be clamped to BOTTOM_CHROME_MAX_HEIGHT"
    );
}

#[test]
fn fullscreen_empty_chat_draws_transcript_pane_and_pinned_chrome() {
    let state = TuiState::new("provider", "model");
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 64, 18, &mut scroll);

    assert!(
        rows.iter().any(|row| row.contains("transcript is empty")),
        "empty transcript message rendered: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("PageUp") && row.contains("/help")),
        "empty transcript should guide history/copy discovery: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("PRX") && row.contains("mode:")),
        "status bar rendered: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Input")),
        "input chrome rendered: {rows:?}"
    );
    assert!(
        rows.last().is_some_and(|row| row.contains("Ctrl+G")),
        "footer pinned to bottom row: {rows:?}"
    );
    let status_row = rows
        .iter()
        .position(|row| row.contains("PRX") && row.contains("mode:"))
        .expect("status row");
    assert!(status_row > 0, "status row has a separator above it");
    assert!(
        rows.get(status_row - 1).is_some_and(|row| row.trim().is_empty()),
        "a fixed blank row separates transcript output from the gray status bar: {rows:?}"
    );
}

#[test]
fn fullscreen_status_bar_draws_metered_tokens_and_cost() {
    let mut state = TuiState::new("provider", "model");
    state.token_usage_summary = MainSessionTokenUsageSummary {
        prompt_tokens: 1_000,
        completion_tokens: 500,
        total_tokens: 1_500,
        reported_tokens: 1_500,
        request_count: 1,
        known_cost_usd: 0.0105,
        ..MainSessionTokenUsageSummary::default()
    };
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 96, 18, &mut scroll);

    assert!(
        rows.iter().any(|row| row.contains("1.5k tok | $0.0105")),
        "status bar should render metered token/cost summary: {rows:?}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("/ 128k") || row.contains("Total chars")),
        "status bar must not render the old chars/window gauge: {rows:?}"
    );
}

#[test]
fn fullscreen_long_transcript_follows_tail_and_pages_up() {
    let mut state = TuiState::new("provider", "model");
    state.conversation_lines.push(ConversationLine::Assistant {
        content: (0..60)
            .map(|idx| format!("line {idx:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });

    let mut tail = FullscreenTranscriptScroll::default();
    let tail_rows = fullscreen_rows(&state, 64, 14, &mut tail);
    assert!(
        tail_rows.iter().any(|row| row.contains("line 059")),
        "tail visible: {tail_rows:?}"
    );
    assert!(
        !tail_rows.iter().any(|row| row.contains("line 000")),
        "old head is not visible at tail: {tail_rows:?}"
    );

    let mut scrolled = FullscreenTranscriptScroll::default();
    scrolled.page_up(8);
    let scrolled_rows = fullscreen_rows(&state, 64, 14, &mut scrolled);
    assert!(
        scrolled_rows.iter().any(|row| row.contains("line 051")),
        "page-up exposes older rows: {scrolled_rows:?}"
    );
    assert!(
        !scrolled_rows.iter().any(|row| row.contains("line 059")),
        "tail row moved out after page-up: {scrolled_rows:?}"
    );
}

#[test]
fn fullscreen_scrolled_transcript_keeps_content_anchor_when_output_arrives() {
    let mut state = TuiState::new("provider", "model");
    state.conversation_lines.push(ConversationLine::Assistant {
        content: (0..60)
            .map(|idx| format!("anchor line {idx:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    let mut scroll = FullscreenTranscriptScroll::default();
    scroll.page_up(8);

    let before_rows = fullscreen_rows(&state, 72, 14, &mut scroll);
    let before_first = before_rows
        .iter()
        .find(|row| row.contains("anchor line"))
        .map(|row| row.trim().to_string())
        .expect("scrolled transcript should expose an anchor line");

    state.conversation_lines.push(ConversationLine::Assistant {
        content: (60..66)
            .map(|idx| format!("anchor line {idx:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    let after_rows = fullscreen_rows(&state, 72, 14, &mut scroll);
    let after_first = after_rows
        .iter()
        .find(|row| row.contains("anchor line"))
        .map(|row| row.trim().to_string())
        .expect("scrolled transcript should keep the anchor line visible");

    assert_eq!(
        after_first, before_first,
        "new output should not push the reading position: before={before_rows:?}, after={after_rows:?}"
    );
    assert!(
        !after_rows.iter().any(|row| row.contains("anchor line 065")),
        "scrolled transcript should not jump to the new tail: {after_rows:?}"
    );
}

#[test]
fn fullscreen_home_end_jump_top_and_tail() {
    let mut state = TuiState::new("provider", "model");
    state.conversation_lines.push(ConversationLine::Assistant {
        content: (0..80)
            .map(|idx| format!("jump line {idx:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    let mut scroll = FullscreenTranscriptScroll::default();

    scroll.jump_top();
    let top_rows = fullscreen_rows(&state, 64, 14, &mut scroll);
    assert!(
        top_rows.iter().any(|row| row.contains("jump line 000")),
        "Home jump exposes transcript top: {top_rows:?}"
    );
    assert!(
        !top_rows.iter().any(|row| row.contains("jump line 079")),
        "Home jump leaves tail below: {top_rows:?}"
    );

    scroll.jump_bottom();
    let tail_rows = fullscreen_rows(&state, 64, 14, &mut scroll);
    assert!(
        tail_rows.iter().any(|row| row.contains("jump line 079")),
        "End jump returns to transcript tail: {tail_rows:?}"
    );
}

#[test]
fn fullscreen_new_output_below_hint_appears_only_while_scrolled_up() {
    let mut state = TuiState::new("provider", "model");
    state.conversation_lines.push(ConversationLine::Assistant {
        content: (0..55)
            .map(|idx| format!("hint line {idx:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    let mut scroll = FullscreenTranscriptScroll::default();
    scroll.page_up(8);
    let before_rows = fullscreen_rows(&state, 70, 16, &mut scroll);
    assert!(
        !before_rows.iter().any(|row| row.contains("New output below")),
        "existing scrollback alone should not show new-output hint: {before_rows:?}"
    );

    state.streaming = Some(StreamingDraft {
        draft_id: "hint-draft".to_string(),
        accumulated: "streaming delta below".to_string(),
        version: 1,
        reasoning_chars: 0,
        reasoning_tail: String::new(),
    });
    let hinted_rows = fullscreen_rows(&state, 70, 16, &mut scroll);
    assert!(
        hinted_rows.iter().any(|row| row.contains("New output below")),
        "new tail output while scrolled up shows footer hint: {hinted_rows:?}"
    );

    scroll.jump_bottom();
    let tail_rows = fullscreen_rows(&state, 70, 16, &mut scroll);
    assert!(
        !tail_rows.iter().any(|row| row.contains("New output below")),
        "jumping to tail clears new-output hint: {tail_rows:?}"
    );
    assert!(
        tail_rows.iter().any(|row| row.contains("streaming delta below")),
        "tail output visible after jump-bottom: {tail_rows:?}"
    );
}

#[test]
fn fullscreen_multiline_input_expands_bottom_chrome_without_gap() {
    let mut state = TuiState::new("provider", "model");
    state.input.set_text("first line\nsecond line\nthird line");
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 70, 18, &mut scroll);

    assert!(
        rows.iter().any(|row| row.contains("first line")),
        "first input row: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("second line")),
        "second input row: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("third line")),
        "third input row: {rows:?}"
    );
    assert!(
        rows.last().is_some_and(|row| row.contains("Ctrl+G")),
        "shortcut footer remains pinned after multiline input without child sessions: {rows:?}"
    );
}

#[test]
fn fullscreen_streaming_draft_renders_at_transcript_tail() {
    let mut state = TuiState::new("provider", "model");
    state.conversation_lines.push(ConversationLine::User {
        content: "hello".to_string(),
    });
    state.streaming = Some(StreamingDraft {
        draft_id: "draft-1".to_string(),
        accumulated: "streaming tail".to_string(),
        version: 1,
        reasoning_chars: 0,
        reasoning_tail: String::new(),
    });
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 64, 16, &mut scroll);

    assert!(
        rows.iter().any(|row| row.contains("> hello")),
        "history rendered: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("streaming tail")),
        "streaming tail rendered: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("Working")),
        "first token replaces the waiting activity: {rows:?}"
    );
}
