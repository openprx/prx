#![cfg(test)]

use super::*;

#[test]
fn fullscreen_waiting_activity_is_clear_at_compact_and_wide_widths() {
    for width in [40, 80, 120] {
        let mut state = TuiState::new("provider", "model");
        state.push_user_message("check service status");
        state.start_stream("draft-waiting");
        state.stream_started_at_ms = Some(chrono::Utc::now().timestamp_millis().saturating_sub(2_000));
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, width, 16, &mut scroll);
        let rendered = rows.join("\n");

        assert!(rendered.contains("Working"), "width {width}: {rows:?}");
        assert!(rendered.contains("Esc to interrupt"), "width {width}: {rows:?}");
        assert!(!rendered.contains("● ▌"), "width {width}: {rows:?}");
        assert!(!rendered.contains("turns"), "width {width}: {rows:?}");
        assert!(rendered.contains("› "), "width {width}: main input prompt: {rows:?}");
    }
}

#[test]
fn fullscreen_thinking_progress_replaces_working_line() {
    let mut state = TuiState::new("provider", "model");
    state.push_user_message("check service status");
    state.start_stream("draft-thinking");
    state.stream_started_at_ms = Some(chrono::Utc::now().timestamp_millis().saturating_sub(3_000));
    let draft = state.streaming.as_mut().expect("test: draft present");
    crate::chat::state::apply_reasoning_progress(draft, "\u{ac00}\u{ac01}\u{ac02}\u{ac03} \u{2705} ");
    crate::chat::state::apply_reasoning_progress(draft, "\u{ac04}\u{ac05}\u{ac06}");
    let expected_chars = "\u{ac00}\u{ac01}\u{ac02}\u{ac03} \u{2705} \u{ac04}\u{ac05}\u{ac06}"
        .chars()
        .count();

    let mut scroll = FullscreenTranscriptScroll::default();
    let rows = fullscreen_rows(&state, 100, 18, &mut scroll);
    let rendered = rows.join("\n");

    assert!(rendered.contains("Thinking"), "thinking label: {rows:?}");
    assert!(
        !rendered.contains("Working"),
        "Working must not be shown while thinking: {rows:?}"
    );
    assert!(
        rendered.contains(&format!("{expected_chars} chars")),
        "live character count: {rows:?}"
    );
    assert!(rendered.contains('~'), "estimated token count: {rows:?}");
    assert!(rendered.contains("tok"), "estimated token unit: {rows:?}");
    assert!(
        rendered.contains("Esc to interrupt"),
        "interrupt hint is preserved: {rows:?}"
    );
    // Wide chars are expanded cell by cell when the test extracts rows, so strip
    // whitespace before comparing.
    let compact: String = rendered.chars().filter(|ch| !ch.is_whitespace()).collect();
    assert!(
        compact.contains("\u{ac04}\u{ac05}\u{ac06}"),
        "single-line live preview: {rows:?}"
    );
}

#[test]
fn fullscreen_activity_line_stays_working_without_reasoning() {
    let mut state = TuiState::new("provider", "model");
    state.push_user_message("check service status");
    state.start_stream("draft-plain-wait");
    state.stream_started_at_ms = Some(chrono::Utc::now().timestamp_millis().saturating_sub(1_000));

    let mut scroll = FullscreenTranscriptScroll::default();
    let rows = fullscreen_rows(&state, 100, 18, &mut scroll);
    let rendered = rows.join("\n");

    assert!(
        rendered.contains("Working"),
        "stays unchanged when there is no reasoning: {rows:?}"
    );
    assert!(
        !rendered.contains("Thinking ("),
        "no thinking counter when there is no reasoning: {rows:?}"
    );
}

#[test]
fn thinking_preview_is_bounded_to_one_clipped_line() {
    let mut draft = StreamingDraft::new("d");
    for round in 0..40 {
        crate::chat::state::apply_reasoning_progress(
            &mut draft,
            &format!("\u{ac00}\u{ac01} {round} \u{ac02}\u{ac03}\u{ac04}\u{ac05} \u{1f600}\n"),
        );
    }
    let preview = draft.reasoning_preview().expect("test: preview present");
    assert!(!preview.contains('\n'), "preview folds into a single line: {preview}");
    assert_eq!(
        draft.reasoning_tail.chars().count(),
        REASONING_TAIL_MAX_CHARS,
        "tail is bounded"
    );
    let clipped = crate::util::truncate_with_ellipsis(&preview, THINKING_PREVIEW_MAX_CHARS);
    assert!(
        clipped.chars().count() <= THINKING_PREVIEW_MAX_CHARS.saturating_add(3),
        "clipped once more before rendering: {clipped}"
    );
}

#[test]
fn fullscreen_streaming_tail_is_not_duplicated_in_bottom_chrome() {
    let mut state = TuiState::new("provider", "model");
    state.streaming = Some(StreamingDraft {
        draft_id: "draft-1".to_string(),
        accumulated: "phase2 unique streaming tail".to_string(),
        version: 1,
        reasoning_chars: 0,
        reasoning_tail: String::new(),
    });
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 72, 16, &mut scroll);
    let occurrences = rows
        .iter()
        .filter(|row| row.contains("phase2 unique streaming tail"))
        .count();
    assert_eq!(
        occurrences, 1,
        "fullscreen should render streaming only at transcript tail, not chrome preview: {rows:?}"
    );
}

#[test]
fn fullscreen_switcher_overlay_renders_over_large_frame_with_chrome_pinned() {
    let mut state = TuiState::new("provider", "model");
    state.switcher = Some(crate::chat::sessions::SwitcherState::new(vec![entry(1), entry(2)]));
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 80, 24, &mut scroll);

    assert!(
        rows.iter().any(|row| row.contains("Sessions - child TUI registry")),
        "switcher overlay title rendered: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("task 1")),
        "switcher row rendered: {rows:?}"
    );
    assert!(
        rows.last().is_some_and(|row| row.contains("Ctrl+G")),
        "bottom chrome footer remains pinned below overlay: {rows:?}"
    );
}

#[test]
fn fullscreen_saved_session_picker_fits_as_overlay_with_chrome_pinned() {
    let mut state = TuiState::new("provider", "model");
    state.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
        saved_picker_entry("saved-1", "saved session one", false),
        saved_picker_entry("saved-2", "saved session two", true),
    ]));
    let mut scroll = FullscreenTranscriptScroll::default();

    let rows = fullscreen_rows(&state, 86, 24, &mut scroll);
    assert!(
        rows.iter().any(|row| row.contains("Saved chat sessions")),
        "saved-session picker title rendered: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("saved session one")),
        "saved-session picker rows rendered: {rows:?}"
    );
    assert!(
        rows.last().is_some_and(|row| row.contains("Ctrl+G")),
        "bottom chrome footer remains pinned below saved picker: {rows:?}"
    );
}

#[test]
fn fullscreen_approval_panel_stays_visible_above_input_when_transcript_scrolled() {
    let mut state = TuiState::new("provider", "model");
    state.conversation_lines.push(ConversationLine::Assistant {
        content: (0..60)
            .map(|idx| format!("approval line {idx:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    state.focus = crate::chat::sessions::FocusTarget::Approval;
    state.pending_tool_approval = Some(crate::chat::sessions::PendingToolApprovalView {
        task_id: None,
        tool_id: "tool-1".to_string(),
        name: "danger_tool".to_string(),
        args: "{\"path\":\"/tmp/demo\"}".to_string(),
        selected_approval: false,
    });
    let mut scroll = FullscreenTranscriptScroll::default();
    scroll.jump_top();

    let rows = fullscreen_rows(&state, 80, 24, &mut scroll);
    assert!(
        rows.iter().any(|row| row.contains("Tool Approval")),
        "approval panel visible outside transcript scroll: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("selected: deny") && row.contains("Enter confirm")),
        "approval panel exposes arrow selection state and Enter confirmation: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("danger_tool")),
        "approval tool name visible: {rows:?}"
    );
    assert!(
        rows.last().is_some_and(|row| row.contains("Ctrl+G")),
        "input footer remains pinned below approval panel: {rows:?}"
    );
}

#[test]
fn fullscreen_child_and_diff_views_use_large_panel_above_input() {
    let mut state = TuiState::new("provider", "model");
    state.conversation_lines.push(ConversationLine::User {
        content: "main transcript".to_string(),
    });
    state.focus = crate::chat::sessions::FocusTarget::Session { seq: 7 };
    state.active_session_view = Some(crate::chat::sessions::ActiveSessionView {
        seq: 7,
        kind: "agent".to_string(),
        title: "phase2 child".to_string(),
        lines: (0..40).map(|idx| format!("child-line-{idx:03}")).collect(),
        truncated: false,
        scroll_offset: 0,
    });
    let mut scroll = FullscreenTranscriptScroll::default();
    let child_rows = fullscreen_rows(&state, 84, 26, &mut scroll);
    assert!(
        child_rows.iter().any(|row| row.contains("attached #7 agent")),
        "child view header rendered in fullscreen panel: {child_rows:?}"
    );
    assert!(
        child_rows.iter().any(|row| row.contains("child-line-039")),
        "child view tail rendered in fullscreen panel: {child_rows:?}"
    );

    state.focus = crate::chat::sessions::FocusTarget::Diff;
    state.active_session_view = Some(crate::chat::sessions::ActiveSessionView {
        seq: DIFF_SESSION_SEQ,
        kind: crate::chat::sessions::model::ManagedKind::Diff.as_str().to_string(),
        title: "workspace diff".to_string(),
        lines: vec!["diff --git a/src/lib.rs b/src/lib.rs".to_string()],
        truncated: false,
        scroll_offset: 0,
    });
    let diff_rows = fullscreen_rows(&state, 84, 26, &mut scroll);
    assert!(
        diff_rows.iter().any(|row| row.contains("diff workspace diff")),
        "diff view header rendered in fullscreen panel: {diff_rows:?}"
    );
    assert!(
        diff_rows.iter().any(|row| row.contains("diff --git")),
        "diff body rendered in fullscreen panel: {diff_rows:?}"
    );
}

#[test]
fn fullscreen_scroll_focus_rules_do_not_steal_input_or_child_keys() {
    let mut input_state = TuiState::new("provider", "model");
    input_state.input.set_text("draft");
    assert!(
        !fullscreen_transcript_scroll_available(&input_state),
        "draft input keeps Home/End/Page keys for the input box"
    );

    let mut history_state = TuiState::new("provider", "model");
    history_state.input.history = vec!["older".to_string(), "newer".to_string()];
    let out = dispatch_global_key(key(KeyCode::Up), &mut history_state);
    assert_eq!(out, KeyDispatch::ScrollTranscriptUp);
    assert!(history_state.input.is_empty());
    let out = dispatch_global_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL), &mut history_state);
    assert_eq!(out, KeyDispatch::Consumed);
    assert_eq!(history_state.input.text(), "newer", "Ctrl+P navigates input history");

    let mut strip_state = TuiState::new("provider", "model");
    strip_state.sessions_cache = vec![entry(1)];
    strip_state.input.history = vec!["older".to_string(), "newer".to_string()];
    let out = dispatch_global_key(key(KeyCode::Down), &mut strip_state);
    assert_eq!(
        out,
        KeyDispatch::ScrollTranscriptDown,
        "bottom entries do not steal bare Down from transcript scrolling"
    );
    assert!(strip_state.input.is_empty());

    let mut child_state = TuiState::new("provider", "model");
    child_state.focus = crate::chat::sessions::FocusTarget::Session { seq: 3 };
    child_state.active_session_view = Some(crate::chat::sessions::ActiveSessionView {
        seq: 3,
        kind: "agent".to_string(),
        title: "child".to_string(),
        lines: vec!["child output".to_string()],
        truncated: false,
        scroll_offset: 0,
    });
    assert!(
        !fullscreen_transcript_scroll_available(&child_state),
        "focused child view owns PageUp/PageDown in fullscreen"
    );
    assert_eq!(
        dispatch_global_key(key(KeyCode::PageUp), &mut child_state),
        KeyDispatch::PageSessionUp
    );
}

#[test]
fn session_list_footer_adds_one_row_per_extra_session() {
    let mut state = TuiState::new("p", "m");
    let idle = fullscreen_bottom_chrome_height(&state);
    assert!(!session_footer_has_sessions(&state), "empty session footer hidden");

    state.sessions_cache = vec![entry(1)];
    assert!(session_footer_has_sessions(&state), "non-empty session footer shown");
    let with_one = fullscreen_bottom_chrome_height(&state);
    assert_eq!(
        with_one,
        idle.saturating_add(2),
        "one child session adds one row plus the session navigation hint"
    );

    state.sessions_cache = vec![entry(1), entry(2), entry(3)];
    let with_three = fullscreen_bottom_chrome_height(&state);
    assert_eq!(
        with_three,
        idle.saturating_add(4),
        "session list adds one row per child plus the navigation hint"
    );

    state.sessions_cache.clear();
    assert!(!session_footer_has_sessions(&state));
    assert_eq!(fullscreen_bottom_chrome_height(&state), idle);
}

#[test]
fn session_list_footer_stays_within_height_budget() {
    let mut state = TuiState::new("p", "m");
    state.sessions_cache = long_strip_entries(30);
    state.start_stream("d");
    for _ in 0..(INPUT_MAX_VISIBLE_ROWS + 4) {
        state.input.lines.push(String::new());
    }
    assert!(
        session_footer_has_sessions(&state),
        "the session list remains available under real inputs"
    );
    assert!(fullscreen_bottom_chrome_height(&state) <= BOTTOM_CHROME_MAX_HEIGHT);
}

#[test]
fn session_list_footer_degrades_within_budget() {
    // Forward-compat guard: even an oversized session list cannot grow the
    // pinned chrome beyond the hard maximum.
    let without_sessions = 1u16 // status
            + u16::try_from(INPUT_MAX_VISIBLE_ROWS + 1).unwrap_or(11)
            + 1; // footer
    assert!(
        without_sessions < BOTTOM_CHROME_MAX_HEIGHT,
        "guard threshold: row drops once the rest reaches BOTTOM_CHROME_MAX_HEIGHT"
    );
}

#[test]
fn status_bar_renders_reported_tokens_and_cost() {
    let mut state = TuiState::new("provider", "model");
    state.session_title = "tokens".to_string();
    state.token_usage_summary = MainSessionTokenUsageSummary {
        prompt_tokens: 1_000,
        completion_tokens: 500,
        total_tokens: 1_500,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        reported_tokens: 1_500,
        estimated_tokens: 0,
        request_count: 1,
        known_cost_usd: 0.0105,
        unknown_cost_requests: 0,
    };

    let line = render_status_bar_text(&state, 120);
    assert!(
        line.contains("1.5k tok | $0.0105"),
        "status should include cumulative reported tokens and cost: {line}"
    );
    assert!(
        !line.contains("~1.5k"),
        "reported-only usage must not be marked estimated: {line}"
    );
}

#[test]
fn status_bar_renders_chat_mode_and_autonomy_ceiling() {
    let mut state = TuiState::new("provider", "model");
    state.chat_mode = ChatMode::Auto;
    state.autonomy_level = AutonomyLevel::ReadOnly;

    let line = render_status_bar_text(&state, 120);

    assert!(
        line.contains("mode:auto auth:read_only"),
        "permission status missing: {line}"
    );
    assert!(
        !line.contains("bypass"),
        "status copy must not imply permissions are bypassed: {line}"
    );
}

#[test]
fn status_bar_renders_main_queue_status_when_present() {
    let mut state = TuiState::new("provider", "model");
    state.main_queue_status = MainQueueStatus { queued: 4, priority: 1 };

    let line = render_status_bar_text(&state, 140);

    assert!(
        line.contains("queue:4 priority:1"),
        "status should expose main input backlog: {line}"
    );
}

#[test]
fn status_bar_renders_provider_worker_status_when_present() {
    let mut state = TuiState::new("provider", "model");
    state.main_queue_status = MainQueueStatus { queued: 2, priority: 0 };
    state.provider_worker_status = ProviderWorkerStatus {
        running: 1,
        cancelling: 1,
        awaiting_commit: 1,
        finalized_payloads: 1,
        finalized_total_tokens: 1_250,
        oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis().saturating_sub(3_000)),
        rows: vec![
            ProviderWorkerStatusRow {
                task_id: 77,
                sequence: 7,
                kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
                state: ProviderWorkerRowState::Running,
                started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(3_000),
                finalized_total_tokens: None,
                completion_ready: false,
                recent_tool_call: None,
            },
            ProviderWorkerStatusRow {
                task_id: 88,
                sequence: 8,
                kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                state: ProviderWorkerRowState::Committed,
                started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(9_000),
                finalized_total_tokens: Some(1_250),
                completion_ready: true,
                recent_tool_call: None,
            },
        ],
    };

    let line = render_status_bar_text(&state, 220);

    assert!(line.contains("queue:2"), "status should retain queue status: {line}");
    assert!(
        line.contains("workers:1"),
        "status should expose running provider workers: {line}"
    );
    assert!(
        line.contains("cancelling:1"),
        "status should expose cancelling provider workers: {line}"
    );
    assert!(
        line.contains("commit:1"),
        "status should expose commit-pending provider workers: {line}"
    );
    assert!(
        line.contains("welapsed:"),
        "status should expose provider worker elapsed time: {line}"
    );
    assert!(
        line.contains("wtok:1.2k"),
        "status should expose finalized provider worker tokens: {line}"
    );
    assert!(
        line.contains("w#7:fg:run:"),
        "status should expose per-worker running detail: {line}"
    );
    assert!(
        !line.contains("w#8:detached:done:1.2k"),
        "status should not keep completed workers in the switchable row list: {line}"
    );
}

#[test]
fn status_bar_hides_terminal_only_worker_accounting() {
    let mut state = TuiState::new("provider", "model");
    state.provider_worker_status = ProviderWorkerStatus {
        running: 0,
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 1,
        finalized_total_tokens: 68_800,
        oldest_started_at_ms: None,
        rows: Vec::new(),
    };

    let line = render_status_bar_text(&state, 180);

    assert!(!line.contains("wtok:"), "terminal worker usage is stale chrome: {line}");
    assert!(
        !line.contains("wpayload:"),
        "terminal payload count is stale chrome: {line}"
    );
}

#[test]
fn status_bar_renders_completion_ready_worker_row() {
    let mut state = TuiState::new("provider", "model");
    state.provider_worker_status = ProviderWorkerStatus {
        running: 1,
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 0,
        finalized_total_tokens: 0,
        oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis().saturating_sub(3_000)),
        rows: vec![ProviderWorkerStatusRow {
            task_id: 77,
            sequence: 7,
            kind: crate::chat::action::ProviderWorkerRowKind::Detached,
            state: ProviderWorkerRowState::Running,
            started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(3_000),
            finalized_total_tokens: None,
            completion_ready: true,
            recent_tool_call: None,
        }],
    };

    let line = render_status_bar_text(&state, 180);

    assert!(
        !line.contains("workers:") && !line.contains("w#7:"),
        "one ordinary main-turn worker is transcript activity, not persistent chrome: {line}"
    );
}

#[test]
fn status_bar_keeps_queue_status_without_duplicate_generation_activity() {
    let mut state = TuiState::new("provider-with-long-name", "model-with-long-name");
    state.session_title = "long running orchestration title".to_string();
    state.main_queue_status = MainQueueStatus { queued: 5, priority: 0 };
    state.start_stream("draft-queue");

    let line = render_status_bar_text(&state, 72);

    assert!(
        line.contains("queue:5"),
        "compact active status should retain queue: {line}"
    );
    assert!(!line.contains("generating"), "activity belongs in transcript: {line}");
}

#[test]
fn status_bar_hides_single_main_worker_without_duplicate_activity() {
    let mut state = TuiState::new("provider-with-long-name", "model-with-long-name");
    state.session_title = "long running orchestration title".to_string();
    state.provider_worker_status = ProviderWorkerStatus {
        running: 1,
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 0,
        finalized_total_tokens: 0,
        oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis().saturating_sub(2_000)),
        rows: vec![ProviderWorkerStatusRow {
            task_id: 1,
            sequence: 1,
            kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
            state: ProviderWorkerRowState::Running,
            started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(2_000),
            finalized_total_tokens: None,
            completion_ready: false,
            recent_tool_call: None,
        }],
    };
    state.start_stream("draft-worker");

    let line = render_status_bar_text(&state, 72);

    assert!(
        !line.contains("workers:1"),
        "single main worker must stay out of chrome: {line}"
    );
    assert!(!line.contains("generating"), "activity belongs in transcript: {line}");
}

#[test]
fn status_bar_permission_status_degrades_at_narrow_width() {
    let mut state = TuiState::new("provider", "model");
    state.chat_mode = ChatMode::Plan;
    state.autonomy_level = AutonomyLevel::Full;
    state.token_usage_summary = MainSessionTokenUsageSummary {
        total_tokens: 1_500,
        estimated_tokens: 1_500,
        known_cost_usd: 0.0006,
        request_count: 1,
        ..MainSessionTokenUsageSummary::default()
    };

    let line = render_status_bar_text(&state, 32);

    assert!(
        UnicodeWidthStr::width(line.as_str()) <= 32,
        "narrow status must fit display width: {line:?}"
    );
    assert!(
        line.contains("mode:"),
        "minimal status should retain mode before truncation: {line}"
    );
}

#[test]
fn status_bar_marks_estimated_usage_with_tilde() {
    let mut state = TuiState::new("provider", "model");
    state.token_usage_summary = MainSessionTokenUsageSummary {
        prompt_tokens: 1_000,
        completion_tokens: 1_000,
        total_tokens: 2_000,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        reported_tokens: 0,
        estimated_tokens: 2_000,
        request_count: 1,
        known_cost_usd: 0.002,
        unknown_cost_requests: 0,
    };

    let line = render_status_bar_text(&state, 120);
    assert!(
        line.contains("~2.0k tok | $0.0020"),
        "estimated usage should be marked: {line}"
    );
}

#[test]
fn status_bar_renders_context_budget_percent_from_current_context_source() {
    let mut state = TuiState::new("provider", "model");
    state.context_used_tokens = Some(8_500);
    state.context_window_tokens = Some(10_000);
    state.token_usage_summary = MainSessionTokenUsageSummary {
        total_tokens: 50_000,
        estimated_tokens: 50_000,
        request_count: 1,
        known_cost_usd: 0.002,
        ..MainSessionTokenUsageSummary::default()
    };

    let line = render_status_bar_text(&state, 140);

    assert!(
        line.contains("~50.0k tok"),
        "estimated token count remains marked: {line}"
    );
    assert!(
        line.contains("ctx:85% used!"),
        "context budget uses current context numerator, not cumulative tokens: {line}"
    );
    assert!(
        !line.contains("~ctx") && !line.contains("ctx:100"),
        "context budget should not inherit token estimate marker or cumulative total: {line}"
    );
}

#[test]
fn status_bar_caps_context_budget_percent_at_100() {
    let mut state = TuiState::new("provider", "model");
    state.context_used_tokens = Some(15_000);
    state.context_window_tokens = Some(10_000);

    let line = render_status_bar_text(&state, 140);

    assert!(line.contains("ctx:100% used!"), "context budget caps at 100%: {line}");
    assert!(!line.contains("ctx:150"), "context budget must not exceed 100%: {line}");
}

#[test]
fn waiting_stream_renders_working_activity_in_transcript() {
    let mut state = TuiState::new("provider", "model");
    state.stream_started_at_ms = Some(chrono::Utc::now().timestamp_millis().saturating_sub(3_000));
    state.start_stream("draft-1");
    state.stream_started_at_ms = Some(chrono::Utc::now().timestamp_millis().saturating_sub(3_000));

    let rows = transcript_plain_rows(&state, 120);
    let rendered = rows.join("\n");

    assert!(
        rendered.contains("Working (3s · Esc to interrupt)"),
        "waiting state is explicit and timed: {rendered}"
    );
    assert!(
        !rendered.contains("● ▌"),
        "empty assistant placeholder must not look stuck: {rendered}"
    );
}

#[test]
fn running_tool_owns_activity_without_duplicate_working_line() {
    let mut state = TuiState::new("provider", "model");
    state.start_stream("draft-1");
    state.push_tool_result_started("shell", "{}");

    let rows = transcript_plain_rows(&state, 120);
    let rendered = rows.join("\n");

    assert!(
        ASCII_SPINNER_FRAMES
            .iter()
            .chain(UNICODE_SPINNER_FRAMES.iter())
            .any(|frame| rendered.contains(frame)),
        "running tool renders an animated marker: {rendered}"
    );
    assert!(
        !rendered.contains("Working"),
        "tool card is the sole activity owner: {rendered}"
    );
}

#[test]
fn status_bar_omits_turn_count_generation_and_completion_activity() {
    let mut snapshot = crate::chat::state::UiSnapshot::initial(Arc::from("provider"), Arc::from("model"));
    snapshot.turn_count = 4;
    snapshot.last_turn_duration_ms = Some(3_200);

    let line = render_status_bar_text(&snapshot, 120);

    assert!(!line.contains("turns"), "turn count is not persistent chrome: {line}");
    assert!(!line.contains("Worked"), "completion belongs in transcript: {line}");
    assert!(!line.contains("generating"), "generation belongs in transcript: {line}");
}

#[test]
fn status_bar_renders_unknown_cost_for_unpriced_usage() {
    let mut state = TuiState::new("provider", "model");
    state.token_usage_summary = MainSessionTokenUsageSummary {
        total_tokens: 42,
        reported_tokens: 42,
        request_count: 1,
        unknown_cost_requests: 1,
        ..MainSessionTokenUsageSummary::default()
    };

    let line = render_status_bar_text(&state, 120);
    assert!(
        line.contains("42 tok | cost unknown"),
        "unknown cost should be explicit: {line}"
    );
}
