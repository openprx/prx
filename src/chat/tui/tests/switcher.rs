#![cfg(test)]

use super::*;

#[test]
fn alt_arrows_do_not_change_bottom_focus() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1), entry(2), entry(3)];

    for code in [KeyCode::Left, KeyCode::Right, KeyCode::Up, KeyCode::Down] {
        assert_eq!(
            dispatch_global_key(key_mod(code, KeyModifiers::ALT), &mut state),
            KeyDispatch::Consumed
        );
    }
    assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
}

#[test]
fn alt_enter_still_inserts_newline() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1)];
    dispatch_global_key(key(KeyCode::Char('a')), &mut state);

    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Enter, KeyModifiers::ALT), &mut state),
        KeyDispatch::Consumed
    );
    dispatch_global_key(key(KeyCode::Char('b')), &mut state);
    assert_eq!(state.input.text(), "a\nb");

    let mut shift_state = TuiState::new("p", "m");
    dispatch_global_key(key(KeyCode::Char('x')), &mut shift_state);
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT), &mut shift_state),
        KeyDispatch::Consumed
    );
    dispatch_global_key(key(KeyCode::Char('y')), &mut shift_state);
    assert_eq!(shift_state.input.text(), "x\ny", "Shift+Enter still inserts newline");
}

#[test]
fn alt_enter_does_not_attach_bottom_entry() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1)];
    dispatch_global_key(key(KeyCode::Char('a')), &mut state);

    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Enter, KeyModifiers::ALT), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "a\n", "Alt+Enter remains a newline chord");
    assert!(state.conversation_lines.is_empty());
}

#[test]
fn esc_keeps_normal_cancel_or_detach_semantics() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1)];
    dispatch_global_key(key(KeyCode::Char('x')), &mut state);

    assert_eq!(
        dispatch_global_key(key(KeyCode::Esc), &mut state),
        KeyDispatch::Cancelled
    );
    assert_eq!(state.input.text(), "", "Esc now follows normal input cancel semantics");
}

#[test]
fn bare_arrows_scroll_transcript_and_child_views_not_history() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1), entry(2)];
    dispatch_global_key(key(KeyCode::Char('a')), &mut state);
    assert_eq!(
        dispatch_global_key(key(KeyCode::Enter), &mut state),
        KeyDispatch::Submitted("a".to_string())
    );

    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::ScrollTranscriptUp,
        "main+empty bare Up scrolls transcript even with bottom entries present"
    );
    assert!(state.input.is_empty(), "bare Up must not recall history");

    state.sessions_cache.clear();
    assert_eq!(
        dispatch_global_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL), &mut state),
        KeyDispatch::Consumed
    );
    assert_eq!(state.input.text(), "a", "Ctrl+P recalls input history");

    assert_eq!(
        dispatch_global_key(key(KeyCode::Left), &mut state),
        KeyDispatch::Consumed,
        "bare Left still edits the input cursor"
    );
    assert_eq!(state.input.cursor, (0, 0));

    state.input.clear();
    state.focus = crate::chat::sessions::FocusTarget::Session { seq: 1 };
    state.active_session_view = Some(crate::chat::sessions::ActiveSessionView {
        seq: 1,
        kind: "agent".to_string(),
        title: "task 1".to_string(),
        lines: vec!["one".to_string(), "two".to_string(), "three".to_string()],
        truncated: false,
        scroll_offset: 0,
    });
    assert_eq!(
        dispatch_global_key(key(KeyCode::PageUp), &mut state),
        KeyDispatch::PageSessionUp,
        "PageUp keeps the focused child scroll binding"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::Up), &mut state),
        KeyDispatch::ScrollSessionUp,
        "with no bottom entries, bare Up keeps the focused child scroll binding"
    );
}

#[test]
fn ctrl_g_still_opens_switcher() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1), entry(2)];

    let out = dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);

    assert!(matches!(out, KeyDispatch::SwitcherOpened { .. }));
    assert!(state.switcher.is_some(), "Ctrl+G modal remains available");
}

#[test]
fn p0_8_esc_empty_main_cancels() {
    // Empty input + main focus → unchanged legacy cancel semantics.
    let mut state = TuiState::new("p", "m");
    assert!(state.input.is_empty());
    let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
    assert_eq!(out, KeyDispatch::Cancelled);
}

#[test]
fn switcher_row_wide_includes_kind_origin_status() {
    let e = pty_entry(3, "vim notes.md");
    let row = render_switcher_row(&e, "⏳", false, 80);
    assert!(row.contains("#3"), "row carries the seq: {row}");
    assert!(row.contains("pty"), "row carries the kind: {row}");
    assert!(row.contains("user"), "row carries the origin: {row}");
    assert!(row.contains("running"), "wide row carries the status text: {row}");
    assert!(row.contains("vim notes.md"), "row carries the title: {row}");
}

#[test]
fn switcher_row_includes_elapsed() {
    let e = elapsed_entry(4, "running", 63);
    let row = render_switcher_row(&e, "⏳", false, 80);
    assert!(row.contains("1m03s"), "row carries compact elapsed: {row}");
}

#[test]
fn switcher_row_includes_subsession_token_usage() {
    let mut e = elapsed_entry(4, "completed", 63);
    e.token_usage_records = vec![usage_record(crate::llm::route_decision::TokenUsageSource::Reported)];

    let row = render_switcher_row(&e, "✓", false, 96);

    assert!(
        row.contains("1m03s 12.3k tok | $0.0042"),
        "row carries elapsed plus usage: {row}"
    );
}

#[test]
fn switcher_row_narrow_drops_origin_and_keeps_kind() {
    // narrow=true → origin + long status text dropped to save columns.
    let e = pty_entry(3, "vim notes.md");
    let row = render_switcher_row(&e, "⏳", true, 30);
    assert!(row.contains("#3"));
    assert!(row.contains("pty"), "kind is still shown when narrow: {row}");
    assert!(!row.contains("user"), "origin tag dropped when narrow: {row}");
}

#[test]
fn switcher_row_never_exceeds_budget_and_truncates_title() {
    let e = pty_entry(
        7,
        "a-very-long-interactive-command-that-will-not-fit-in-a-tiny-terminal",
    );
    let budget: u16 = 24;
    let row = render_switcher_row(&e, "⏳", true, budget);
    assert!(
        row.chars().count() <= budget as usize,
        "row ({} cols) must fit budget {budget}: {row}",
        row.chars().count()
    );
    assert!(row.contains('\u{2026}'), "an elided title ends with an ellipsis: {row}");
}

#[test]
fn switcher_row_zero_budget_is_empty() {
    let e = pty_entry(1, "x");
    assert!(
        render_switcher_row(&e, "⏳", false, 0).is_empty(),
        "expected an empty value"
    );
}

#[test]
fn running_status_glyph_animates_with_shared_spinner_frames() {
    let running = elapsed_entry(1, "running", 0);
    assert_eq!(session_status_glyph_for_tick(&running, false, 0), "⠋");
    assert_eq!(session_status_glyph_for_tick(&running, false, 1), "⠙");
    assert_eq!(session_status_glyph_for_tick(&running, true, 0), "-");
    assert_eq!(session_status_glyph_for_tick(&running, true, 1), "\\");

    let completed = elapsed_entry(2, "completed", 0);
    assert_eq!(
        session_status_glyph_for_tick(&completed, false, 0),
        session_status_glyph_for_tick(&completed, false, 1),
        "non-running rows must stay static"
    );
}

#[test]
fn periodic_redraw_tracks_running_bottom_entries_only() {
    let mut state = TuiState::new("p", "m");
    assert!(
        !periodic_redraw_active_for_view(&state),
        "idle chat has no animation tick"
    );

    state.sessions_cache = vec![elapsed_entry(1, "completed", 0)];
    assert!(
        !periodic_redraw_active_for_view(&state),
        "completed bottom entries do not drive periodic redraw"
    );

    state.sessions_cache = vec![elapsed_entry(1, "running", 0)];
    assert!(
        periodic_redraw_active_for_view(&state),
        "running bottom sessions drive the spinner redraw"
    );

    state.sessions_cache.clear();
    state.provider_worker_status = ProviderWorkerStatus {
        running: 1,
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 0,
        finalized_total_tokens: 0,
        oldest_started_at_ms: Some(0),
        rows: vec![ProviderWorkerStatusRow {
            task_id: 9,
            sequence: 9,
            kind: crate::chat::action::ProviderWorkerRowKind::Detached,
            state: ProviderWorkerRowState::Running,
            started_at_ms: 0,
            finalized_total_tokens: None,
            completion_ready: false,
            recent_tool_call: None,
        }],
    };
    assert!(
        !periodic_redraw_active_for_view(&state),
        "a hidden single main worker does not create a duplicate bottom animation"
    );

    let row = state
        .provider_worker_status
        .rows
        .first_mut()
        .expect("test status has one row");
    row.state = ProviderWorkerRowState::Committed;
    state.provider_worker_status.running = 0;
    assert!(
        !periodic_redraw_active_for_view(&state),
        "terminal worker rows do not keep the periodic redraw alive"
    );
}

#[test]
fn session_list_footer_includes_navigation_hint_when_entries_exist() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![elapsed_entry(1, "running", 0)];

    let lines = render_sessions_list_lines(&state, 100, 4);
    let rows = lines.iter().map(line_to_plain).collect::<Vec<_>>();

    assert!(
        rows.iter().any(|row| row.contains("←/→ session")),
        "session footer should expose horizontal session navigation: {rows:?}"
    );
    assert!(
        footer_text(false, true).contains("←/→ session") && footer_text(false, true).contains("Ctrl+P/N history"),
        "static footer text helper also includes the dynamic hint"
    );
}

#[test]
fn footer_exposes_history_and_plain_copy_guidance() {
    let footer = footer_text(false, false);

    assert!(
        footer.contains("Home/PageUp history"),
        "footer should expose in-app history scrolling: {footer}"
    );
    assert!(
        footer.contains("drag select/copy"),
        "footer should point users to native drag-copy mode: {footer}"
    );
}

#[test]
fn transcript_selection_normalizes_drag_direction() {
    let mut selection = TranscriptSelection::new(8, 4);
    assert!(!selection.moved());

    selection.update(3, 2);

    assert!(selection.moved());
    assert_eq!(selection.ordered_points(), ((3, 2), (8, 4)));
}

#[test]
fn transcript_plain_rows_follow_rendered_wrapping() {
    let mut state = TuiState::new("p", "m");
    state.conversation_lines.push(ConversationLine::User {
        content: "abcdefgh".to_string(),
    });

    assert_eq!(transcript_plain_rows(&state, 4), vec!["> ab", "cdef", "gh", ""]);
}

#[test]
fn provider_worker_switcher_title_appends_latest_tool_summary_for_running_rows() {
    let conversation = vec![ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "ls -la /tmp/demo".to_string(),
        args_full: "{\"command\":\"ls -la /tmp/demo\"}".to_string(),
        result: None,
        status: ToolStatus::Running,
        elapsed_ms: None,
        folded: true,
    }];
    let summary = latest_provider_worker_tool_summary_from_conversation(&conversation, false).expect("tool summary");
    assert!(
        summary.contains("run shell running: ls -la /tmp/demo"),
        "summary uses the compact provider worker IO wording: {summary}"
    );

    let running = ProviderWorkerStatusRow {
        task_id: 3,
        sequence: 3,
        kind: crate::chat::action::ProviderWorkerRowKind::Detached,
        state: ProviderWorkerRowState::Running,
        started_at_ms: 0,
        finalized_total_tokens: None,
        completion_ready: false,
        recent_tool_call: Some(summary.clone()),
    };
    let entry = provider_worker_switcher_entry(&running);
    assert!(
        entry.title.contains(&summary),
        "running worker title should include latest tool summary: {}",
        entry.title
    );

    let mut completed = running;
    completed.state = ProviderWorkerRowState::Committed;
    let entry = provider_worker_switcher_entry(&completed);
    assert!(
        !entry.title.contains(&summary),
        "terminal worker titles stay unchanged: {}",
        entry.title
    );
}

#[test]
fn truncation_is_cjk_column_accurate() {
    let line = truncate_chars_with_ellipsis("\u{ac00}\u{ac01}\u{ac02}\u{ac03}abc", 5, false);
    assert!(
        UnicodeWidthStr::width(line.as_str()) <= 5,
        "wide chars must fit the column budget, got {:?} width {}",
        line,
        UnicodeWidthStr::width(line.as_str())
    );
    assert!(
        line.ends_with('\u{2026}'),
        "wide truncation should use ellipsis: {line:?}"
    );
}

#[test]
fn active_session_visible_lines_follow_tail_and_scroll_slice() {
    let view = active_view(4, (0..20).map(|i| format!("line {i}")).collect(), 0);
    assert_eq!(
        active_session_visible_lines(&view, 3),
        vec!["line 17".to_string(), "line 18".to_string(), "line 19".to_string()]
    );

    let scrolled = active_view(4, (0..20).map(|i| format!("line {i}")).collect(), 2);
    assert_eq!(
        active_session_visible_lines(&scrolled, 3),
        vec!["line 15".to_string(), "line 16".to_string(), "line 17".to_string()]
    );
}

#[test]
fn transcript_view_is_full_and_handles_empty_history() {
    let empty = build_transcript_view("", &[], 0);
    assert_eq!(empty.seq, TRANSCRIPT_SESSION_SEQ);
    assert_eq!(
        empty.kind,
        crate::chat::sessions::model::ManagedKind::Transcript.as_str()
    );
    assert_eq!(
        empty.lines,
        vec!["(transcript is empty - PageUp/Home view history; /help for copy/export)".to_string()]
    );

    let long = ConversationLine::Assistant {
        content: (0..(TRANSCRIPT_MAX_LINES + 25))
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let view = build_transcript_view("demo", &[long], usize::MAX);
    assert_eq!(view.lines.len(), TRANSCRIPT_MAX_LINES + 25);
    assert!(!view.truncated, "transcript viewer retains full content");
    assert_eq!(
        view.scroll_offset,
        usize::MAX,
        "viewer no longer clamps away scroll range"
    );
    assert!(
        view.lines.first().is_some_and(|line| line.contains("line 0")),
        "oldest lines are retained: {:?}",
        view.lines.first()
    );
}

#[test]
fn transcript_view_expands_folded_tool_and_reasoning_content() {
    let tool = ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "cmd=short".to_string(),
        args_full: "{\"cmd\":\"long\"}".to_string(),
        result: Some("out-1\nout-2".to_string()),
        status: ToolStatus::Done,
        elapsed_ms: Some(1),
        folded: true,
    };
    let reasoning = ConversationLine::Reasoning {
        content: "hidden thought".to_string(),
        char_count: 12,
        folded: true,
    };

    let view = build_transcript_view("demo", &[tool, reasoning], 0);

    assert!(
        view.lines.iter().any(|line| line.contains("{\"cmd\":\"long\"}")),
        "verbose transcript should use full tool args: {:?}",
        view.lines
    );
    assert!(
        view.lines.iter().any(|line| line.contains("out-2")),
        "verbose transcript should include full tool result: {:?}",
        view.lines
    );
    assert!(
        view.lines.iter().any(|line| line.contains("hidden thought")),
        "verbose transcript should include folded reasoning: {:?}",
        view.lines
    );
}

#[test]
fn diff_view_handles_empty_bounded_and_clamps_scroll() {
    let empty = build_diff_view("", Vec::new(), false, 0);
    assert_eq!(empty.seq, DIFF_SESSION_SEQ);
    assert_eq!(empty.kind, crate::chat::sessions::model::ManagedKind::Diff.as_str());
    assert_eq!(empty.title, "workspace diff");
    assert_eq!(empty.lines, vec!["(no workspace diff)".to_string()]);

    let view = build_diff_view(
        "staged diff",
        (0..24).map(|i| format!("+\u{ac00}\u{ac01}\u{ac02}{i}")).collect(),
        true,
        usize::MAX,
    );
    assert_eq!(view.title, "staged diff");
    assert!(view.truncated);
    assert_eq!(
        view.scroll_offset,
        view.max_scroll_offset(usize::from(ACTIVE_SESSION_VIEW_DESIRED_ROWS)),
        "oversized diff offset clamps to the oldest retained visible page"
    );
}

#[test]
fn p0_8_esc_nonempty_clears_input_even_when_attached() {
    // Muscle memory preserved: non-empty input clears first, never detaches.
    let mut state = TuiState::new("p", "m");
    state.focus = crate::chat::sessions::FocusTarget::Session { seq: 2 };
    for ch in "hi".chars() {
        dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
    }
    let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
    assert_eq!(out, KeyDispatch::Cancelled);
    assert!(state.input.is_empty(), "non-empty Esc clears, does not detach");
}

#[test]
fn p0_8_esc_empty_attached_requests_detach() {
    // Empty input + session focus → detach.
    let mut state = TuiState::new("p", "m");
    state.focus = crate::chat::sessions::FocusTarget::Session { seq: 3 };
    let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
    assert_eq!(out, KeyDispatch::RequestDetach);
}

#[test]
fn esc_empty_diff_closes_viewer() {
    let mut state = TuiState::new("p", "m");
    state.focus = crate::chat::sessions::FocusTarget::Diff;
    let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
    assert_eq!(out, KeyDispatch::CloseDiffViewer);
}

#[test]
fn ctrl_g_opens_switcher_over_cache() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1), entry(2)];
    let out = dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
    match out {
        KeyDispatch::SwitcherOpened { entries } => {
            assert_eq!(entries.len(), 3);
            assert!(
                entries.first().is_some_and(|entry| entry.is_transcript()),
                "transcript row is switcher-only first entry"
            );
            assert_eq!(
                state.sessions_cache.len(),
                2,
                "transcript row must not enter real-session cache"
            );
        }
        other => panic!("expected SwitcherOpened, got {other:?}"),
    }
    assert!(state.switcher.is_some(), "switcher opened in mirror");
}

#[test]
fn ctrl_g_includes_provider_worker_rows_and_enter_opens_worker_view() {
    let mut state = TuiState::new("p", "m");
    state.provider_worker_status = provider_worker_status_fixture();

    let out = dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
    match out {
        KeyDispatch::SwitcherOpened { entries } => {
            assert_eq!(entries.len(), 2, "transcript plus worker row: {entries:?}");
            let worker = entries.get(1).expect("worker row");
            assert_eq!(worker.kind, PROVIDER_WORKER_SWITCHER_KIND);
            assert_eq!(worker.origin, "provider");
            assert_eq!(worker.status, "running");
            assert!(
                worker.title.contains("w#3 foreground_awaited task=42"),
                "worker title: {}",
                worker.title
            );
            let row = render_switcher_row(worker, "⏳", false, 120);
            assert!(row.contains("w#3 worker provider running"), "worker row: {row}");
            assert!(row.contains("foreground_awaited task=42"), "worker row: {row}");
            assert!(
                !row.contains(&worker.seq.to_string()),
                "synthetic seq must not leak into worker row: {row}"
            );
        }
        other => panic!("expected SwitcherOpened, got {other:?}"),
    }

    assert_eq!(
        dispatch_global_key(key(KeyCode::Down), &mut state),
        KeyDispatch::SwitcherMoved { selected: 1 }
    );
    let out = dispatch_global_key(key(KeyCode::Enter), &mut state);

    assert_eq!(out, KeyDispatch::OpenProviderWorkerView { sequence: 3 });
    assert!(state.switcher.is_none(), "switcher closed after worker detail route");
}

#[test]
fn single_main_worker_is_not_a_bottom_session_target() {
    let mut state = TuiState::new("p", "m");
    state.provider_worker_status = provider_worker_status_fixture();
    state.visible_streaming_drafts = Arc::new(vec![crate::chat::state::VisibleStreamingDraftView {
        sequence: 3,
        draft: StreamingDraft {
            draft_id: "draft-worker-3".to_string(),
            accumulated: "worker 3 live".to_string(),
            version: 1,
            reasoning_chars: 0,
            reasoning_tail: String::new(),
        },
    }]);
    let out = dispatch_global_key(key(KeyCode::Right), &mut state);
    assert_eq!(
        out,
        KeyDispatch::Consumed,
        "an ordinary single main turn must not masquerade as a bottom child session"
    );
    assert_eq!(
        state
            .streaming_draft_for_worker(3)
            .map(|draft| draft.accumulated.as_str()),
        Some("worker 3 live")
    );
    assert!(
        state
            .streaming_draft_for_worker(PROVIDER_WORKER_SWITCHER_SEQ_BASE + 3)
            .is_none(),
        "synthetic switcher seq must not be used for draft lookup"
    );
}

#[test]
fn single_main_worker_does_not_leak_tool_command_into_composer_chrome() {
    let mut state = TuiState::new("p", "m");
    state.provider_worker_status = provider_worker_status_fixture();
    state
        .provider_worker_status
        .rows
        .first_mut()
        .expect("fixture has one worker")
        .recent_tool_call = Some("run shell running: command=\"npx browser-mcp\"".to_string());

    assert!(
        !session_footer_has_sessions(&state),
        "one ordinary main turn must not create a session footer"
    );
    let rows = render_sessions_list_lines(&state, 120, 4);
    assert!(
        rows.iter().all(|row| !line_to_plain(row).contains("npx browser-mcp")),
        "internal provider command must not appear under the input: {rows:?}"
    );

    let mut second = state
        .provider_worker_status
        .rows
        .first()
        .expect("fixture has one worker")
        .clone();
    second.task_id = 43;
    second.sequence = 4;
    second.recent_tool_call = Some("run web_search running".to_string());
    state.provider_worker_status.rows.push(second);
    state.provider_worker_status.running = 2;
    assert!(
        session_footer_has_sessions(&state),
        "real concurrent main turns remain discoverable"
    );
}

#[test]
fn phase2_provider_worker_io_none_is_empty_not_history_fallback() {
    let conversation = vec![
        ConversationLine::User {
            content: "run command".to_string(),
        },
        ConversationLine::Assistant {
            content: "history assistant must not appear".to_string(),
        },
    ];

    let lines = provider_worker_io_lines_for_streaming_draft(&conversation, None, 8);

    assert!(
        lines.is_empty(),
        "missing worker draft must not replay transcript: {lines:?}"
    );
}

#[test]
fn provider_worker_focus_direction_and_esc_are_read_only_view_controls() {
    let mut state = TuiState::new("p", "m");
    state.provider_worker_status = provider_worker_status_fixture();
    state.sessions_cache = vec![entry(7)];
    state.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 3 };
    let out = dispatch_global_key(key(KeyCode::Up), &mut state);
    assert_eq!(out, KeyDispatch::ScrollSessionUp);
    let out = dispatch_global_key(key(KeyCode::Down), &mut state);
    assert_eq!(out, KeyDispatch::ScrollSessionDown);
    let out = dispatch_global_key(key(KeyCode::Right), &mut state);
    assert_eq!(out, KeyDispatch::SwitchSession { seq: 7 });
    let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
    assert_eq!(out, KeyDispatch::CloseProviderWorkerView);
}

#[test]
fn provider_worker_active_view_uses_worker_kind_and_lines() {
    let status = provider_worker_status_fixture();
    let view = crate::chat::action::build_provider_worker_active_view(&status, 3, 0);
    assert_eq!(view.kind, crate::chat::sessions::model::ManagedKind::Worker.as_str());
    assert_eq!(view.seq, 3);
    assert!(view.lines.iter().any(|line| line == "worker: w#3"));
    assert!(view.lines.iter().any(|line| line == "task: 42"));
    assert!(view.lines.iter().any(|line| line == "kind: foreground_awaited"));
    assert!(view.lines.iter().any(|line| line == "state: running"));
    assert!(view.lines.iter().any(|line| line == "completion: pending"));
}

#[test]
fn provider_worker_io_lines_include_tool_output_and_streaming_text() {
    let conversation = vec![
        ConversationLine::User {
            content: "run a shell command".to_string(),
        },
        ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: "sleep 1 && echo done".to_string(),
            args_full: "{\"command\":\"sleep 1 && echo done\"}".to_string(),
            result: Some("done\nsecond line".to_string()),
            status: ToolStatus::Done,
            elapsed_ms: Some(1004),
            folded: true,
        },
        ConversationLine::Assistant {
            content: "tool finished".to_string(),
        },
    ];
    let streaming = StreamingDraft {
        draft_id: "d".to_string(),
        accumulated: "partial answer".to_string(),
        version: 1,
        reasoning_chars: 0,
        reasoning_tail: String::new(),
    };

    let lines = provider_worker_io_lines_from_conversation(&conversation, Some(&streaming), 8);

    assert!(lines.iter().any(|line| line.contains("run shell done: sleep 1")));
    assert!(lines.iter().any(|line| line == "output: done"));
    assert!(lines.iter().any(|line| line == "output: second line"));
    assert!(lines.iter().any(|line| line == "assistant: tool finished"));
    assert!(lines.iter().any(|line| line == "assistant streaming: partial answer"));

    let view =
        crate::chat::action::build_provider_worker_active_view_with_io(&provider_worker_status_fixture(), 3, 0, lines);
    assert!(view.lines.iter().any(|line| line == "io: recent provider turn"));
    assert!(view.lines.iter().any(|line| line.starts_with("run shell done:")));
}

#[test]
fn switcher_navigation_and_enter_attaches_selected() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1), entry(2), entry(3)];
    dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
    // Transcript is row 0; Down three times → select #3.
    let m1 = dispatch_global_key(key(KeyCode::Down), &mut state);
    assert_eq!(m1, KeyDispatch::SwitcherMoved { selected: 1 });
    dispatch_global_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL), &mut state);
    dispatch_global_key(key(KeyCode::Down), &mut state);
    // Enter attaches the highlighted session and closes the switcher.
    let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
    assert_eq!(out, KeyDispatch::AttachSession { seq: 3 });
    assert!(state.switcher.is_none(), "switcher closed after attach");
}

#[test]
fn switcher_esc_closes_without_attaching() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1)];
    dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
    let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
    assert_eq!(out, KeyDispatch::SwitcherClosed);
    assert!(state.switcher.is_none());
}

#[test]
fn switcher_ctrl_g_toggles_closed() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1)];
    dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
    assert!(state.switcher.is_some());
    let out = dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
    assert_eq!(out, KeyDispatch::SwitcherClosed);
    assert!(state.switcher.is_none(), "second Ctrl+G toggles closed");
}

#[test]
fn switcher_transcript_enter_opens_viewer_without_attach_zero() {
    let mut state = TuiState::new("p", "m");
    // No cached real sessions; Ctrl+G still offers the transcript child TUI.
    dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
    let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
    assert_eq!(
        out,
        KeyDispatch::OpenTranscriptViewer,
        "transcript row must open the viewer, never /attach 0"
    );
    assert!(state.switcher.is_none());
}

#[test]
fn switcher_open_swallows_plain_keys() {
    // While the overlay has focus, plain typing must not leak into the input.
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = vec![entry(1)];
    dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
    let out = dispatch_global_key(key(KeyCode::Char('x')), &mut state);
    assert_eq!(out, KeyDispatch::Consumed);
    assert!(state.input.is_empty(), "switcher swallowed the keystroke");
}

#[test]
fn prompt_indicator_main_vs_session() {
    let (main_span, main_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Main, false, None);
    assert_eq!(main_span.content.as_ref(), "› ");
    assert_eq!(main_w, 2);
    let (main_ascii_span, _) = prompt_indicator(crate::chat::sessions::FocusTarget::Main, true, None);
    assert_eq!(main_ascii_span.content.as_ref(), "> ");
    let (sess_span, sess_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Session { seq: 4 }, false, None);
    assert!(sess_span.content.contains("agent #4"), "carries the target as text");
    assert!(sess_span.content.contains('\u{25B8}'), "uses the ▸ glyph");
    assert_eq!(sess_w, UnicodeWidthStr::width(sess_span.content.as_ref()));
    // ASCII fallback drops the unicode glyph but keeps the text target.
    let (ascii_span, _) = prompt_indicator(crate::chat::sessions::FocusTarget::Session { seq: 4 }, true, None);
    assert!(ascii_span.content.contains("agent #4"));
    assert!(!ascii_span.content.contains('\u{25B8}'), "ascii fallback omits ▸");
    let (shell_span, _) = prompt_indicator(
        crate::chat::sessions::FocusTarget::Session { seq: 4 },
        false,
        Some("shell"),
    );
    assert!(shell_span.content.contains("shell #4"));
    assert!(!shell_span.content.contains("agent #4"));
    let (transcript_span, transcript_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Transcript, false, None);
    assert!(transcript_span.content.contains("transcript"));
    assert!(transcript_span.content.contains('\u{25B8}'));
    assert_eq!(transcript_w, UnicodeWidthStr::width(transcript_span.content.as_ref()));
    let (diff_span, diff_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Diff, false, None);
    assert!(diff_span.content.contains("diff"));
    assert_eq!(diff_w, UnicodeWidthStr::width(diff_span.content.as_ref()));
}

#[test]
fn dispatch_sequence_hello_enter_returns_submitted_text() {
    // Full integration: simulate the canonical "type Hello + Enter" flow.
    let mut state = TuiState::new("p", "m");
    let seq = [
        key(KeyCode::Char('H')),
        key(KeyCode::Char('e')),
        key(KeyCode::Char('l')),
        key(KeyCode::Char('l')),
        key(KeyCode::Char('o')),
        key(KeyCode::Enter),
    ];
    let mut submitted: Option<String> = None;
    for k in seq {
        match dispatch_global_key(k, &mut state) {
            KeyDispatch::Submitted(text) => submitted = Some(text),
            KeyDispatch::Consumed => {}
            other => panic!("test: unexpected dispatch {other:?}"),
        }
    }
    assert_eq!(submitted, Some("Hello".to_string()));
}
