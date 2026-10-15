#![cfg(test)]

use super::*;
use crate::chat::action::Action;
use crate::chat::state::ChatState;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn make_state_with_lines() -> ChatState {
    let mut s = ChatState::new(Arc::from("p-x"), Arc::from("m-x"), CancellationToken::new());
    s.session.title = "demo session".to_string();
    s.ui.turn_count = 3;
    s.ui.conversation_lines.push(ConversationLine::User {
        content: "first".to_string(),
    });
    s.ui.conversation_lines.push(ConversationLine::Assistant {
        content: "second".to_string(),
    });
    s
}

/// Parity check: after the same ChatState is mapped into both TuiState (the
/// mirror-compatible field set) and UiSnapshot, `fullscreen_bottom_chrome_height`
/// returns the same value on both views.
#[test]
fn s4_a_2_fullscreen_bottom_chrome_height_parity_tui_vs_snapshot() {
    let mut state = make_state_with_lines();
    let snap = state.build_ui_snapshot(1);

    // Build a TuiState aligned with snap's fields (mirror-compatible field set).
    let mut tui = TuiState::new(&state.session.provider, &state.session.model);
    tui.session_title = state.session.title.clone();
    tui.turn_count = state.ui.turn_count;
    tui.ascii_fallback = state.ui.ascii_fallback;
    tui.conversation_lines = state.ui.conversation_lines.clone();
    tui.streaming = state.stream.primary_streaming_draft().cloned();
    tui.input = state.ui.input.clone();

    assert_eq!(
        fullscreen_bottom_chrome_height(&tui),
        fullscreen_bottom_chrome_height(&snap),
        "TuiState vs UiSnapshot must report the same height for the same fixture"
    );
}

/// Parity check: the two views still report the same height while streaming.
#[test]
fn s4_a_2_fullscreen_bottom_chrome_height_parity_streaming() {
    let mut state = make_state_with_lines();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamChunkReceived {
        draft_id: "d-1".to_string(),
        delta: "streaming…".to_string(),
        version: 3,
    });
    let snap = state.build_ui_snapshot(2);

    let mut tui = TuiState::new(&state.session.provider, &state.session.model);
    tui.session_title = state.session.title.clone();
    tui.turn_count = state.ui.turn_count;
    tui.ascii_fallback = state.ui.ascii_fallback;
    tui.conversation_lines = state.ui.conversation_lines.clone();
    tui.streaming = state.stream.primary_streaming_draft().cloned();
    tui.input = state.ui.input.clone();

    let h_tui = fullscreen_bottom_chrome_height(&tui);
    let h_snap = fullscreen_bottom_chrome_height(&snap);
    assert_eq!(
        h_tui, h_snap,
        "heights must match while streaming (tui={h_tui}, snap={h_snap})"
    );
    assert_eq!(
        h_tui,
        fullscreen_bottom_chrome_height(&TuiState::new(&state.session.provider, &state.session.model)),
        "streaming must not take up the fullscreen bottom chrome twice"
    );
}

/// Parity check: every BottomChromeView getter returns the same field on
/// TuiState and on UiSnapshot.
#[test]
fn s4_a_2_view_getters_parity() {
    let mut state = make_state_with_lines();
    let session_entry = crate::chat::sessions::SwitcherEntry {
        seq: 7,
        kind: "agent",
        origin: "model",
        status: "running",
        title: "non-empty parity session".to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
        idle_warning: false,
    };
    state.ui.sessions_entries = vec![session_entry.clone()];
    state.ui.context_used_tokens = Some(2_500);
    state.ui.context_window_tokens = Some(10_000_000);
    state.ui.chat_mode = ChatMode::Auto;
    state.ui.autonomy_level = AutonomyLevel::ReadOnly;
    let active_view = crate::chat::sessions::ActiveSessionView {
        seq: DIFF_SESSION_SEQ,
        kind: crate::chat::sessions::model::ManagedKind::Diff.as_str().to_string(),
        title: "workspace diff".to_string(),
        lines: vec![
            "diff --git a/src/lib.rs b/src/lib.rs".to_string(),
            "+line a".to_string(),
        ],
        truncated: true,
        scroll_offset: 1,
    };
    state.ui.focus = crate::chat::sessions::FocusTarget::Diff;
    state.ui.active_session_view = Some(active_view.clone());
    let saved_picker = crate::chat::session::SavedSessionPickerState {
        entries: vec![saved_picker_entry("saved-1", "saved parity session", true)],
        selected: 0,
    };
    state.ui.saved_session_picker = Some(saved_picker.clone());
    let snap = state.build_ui_snapshot(5);

    let mut tui = TuiState::new(&state.session.provider, &state.session.model);
    tui.session_title = state.session.title.clone();
    tui.turn_count = state.ui.turn_count;
    tui.ascii_fallback = state.ui.ascii_fallback;
    tui.conversation_lines = state.ui.conversation_lines.clone();
    tui.streaming = state.stream.primary_streaming_draft().cloned();
    tui.input = state.ui.input.clone();
    tui.sessions_cache = vec![session_entry];
    tui.chat_mode = ChatMode::Auto;
    tui.autonomy_level = AutonomyLevel::ReadOnly;
    tui.focus = crate::chat::sessions::FocusTarget::Diff;
    tui.active_session_view = Some(active_view);
    tui.context_used_tokens = Some(2_500);
    tui.context_window_tokens = Some(10_000_000);
    tui.saved_session_picker = Some(saved_picker);

    assert_eq!(BottomChromeView::provider(&tui), BottomChromeView::provider(&snap));
    assert_eq!(BottomChromeView::model(&tui), BottomChromeView::model(&snap));
    assert_eq!(BottomChromeView::chat_mode(&tui), BottomChromeView::chat_mode(&snap));
    assert_eq!(
        BottomChromeView::autonomy_level(&tui),
        BottomChromeView::autonomy_level(&snap)
    );
    assert_eq!(
        BottomChromeView::session_title(&tui),
        BottomChromeView::session_title(&snap)
    );
    assert_eq!(BottomChromeView::turn_count(&tui), BottomChromeView::turn_count(&snap));
    assert_eq!(
        BottomChromeView::conversation_lines(&tui).len(),
        BottomChromeView::conversation_lines(&snap).len()
    );
    assert_eq!(
        BottomChromeView::streaming(&tui).is_some(),
        BottomChromeView::streaming(&snap).is_some()
    );
    assert_eq!(
        BottomChromeView::sessions_entries(&tui).len(),
        BottomChromeView::sessions_entries(&snap).len()
    );
    let [tui_entry] = BottomChromeView::sessions_entries(&tui) else {
        panic!("TuiState fixture should expose exactly one session entry");
    };
    let [snap_entry] = BottomChromeView::sessions_entries(&snap) else {
        panic!("UiSnapshot fixture should expose exactly one session entry");
    };
    assert_eq!(tui_entry.seq, snap_entry.seq);
    assert_eq!(tui_entry.kind, snap_entry.kind);
    assert_eq!(tui_entry.origin, snap_entry.origin);
    assert_eq!(tui_entry.status, snap_entry.status);
    assert_eq!(tui_entry.title, snap_entry.title);
    assert_eq!(
        BottomChromeView::active_session_view(&tui),
        BottomChromeView::active_session_view(&snap)
    );
    assert_eq!(BottomChromeView::focus(&tui), BottomChromeView::focus(&snap));
    assert_eq!(
        BottomChromeView::pending_tool_approval(&tui),
        BottomChromeView::pending_tool_approval(&snap)
    );
    assert_eq!(
        BottomChromeView::context_used_tokens(&tui),
        BottomChromeView::context_used_tokens(&snap)
    );
    assert_eq!(
        BottomChromeView::context_window_tokens(&tui),
        BottomChromeView::context_window_tokens(&snap)
    );
    let tui_picker = BottomChromeView::saved_session_picker(&tui).expect("tui saved picker");
    let snap_picker = BottomChromeView::saved_session_picker(&snap).expect("snapshot saved picker");
    assert_eq!(tui_picker.selected, snap_picker.selected);
    let [tui_saved] = tui_picker.entries.as_slice() else {
        panic!("TuiState fixture should expose exactly one saved picker entry");
    };
    let [snap_saved] = snap_picker.entries.as_slice() else {
        panic!("UiSnapshot fixture should expose exactly one saved picker entry");
    };
    assert_eq!(tui_saved.id, snap_saved.id);
    assert_eq!(tui_saved.title, snap_saved.title);
    assert_eq!(tui_saved.turn_count, snap_saved.turn_count);
    assert_eq!(tui_saved.provider, snap_saved.provider);
    assert_eq!(tui_saved.model, snap_saved.model);
    assert_eq!(tui_saved.is_current, snap_saved.is_current);
}

#[test]
fn s4_a_2_pending_tool_approval_parity() {
    let mut state = make_state_with_lines();
    let pending = crate::chat::sessions::PendingToolApprovalView {
        task_id: None,
        tool_id: "call-approval".to_string(),
        name: "shell".to_string(),
        args: r#"{"cmd":"rm -rf /tmp/nope"}"#.to_string(),
        selected_approval: false,
    };
    state.ui.pending_tool_approval = Some(pending.clone());
    state.ui.focus = crate::chat::sessions::FocusTarget::Approval;
    let snap = state.build_ui_snapshot(11);

    let mut tui = TuiState::new(&state.session.provider, &state.session.model);
    tui.pending_tool_approval = Some(pending);
    tui.focus = crate::chat::sessions::FocusTarget::Approval;

    assert_eq!(
        BottomChromeView::pending_tool_approval(&tui),
        BottomChromeView::pending_tool_approval(&snap)
    );
    assert_eq!(BottomChromeView::focus(&tui), BottomChromeView::focus(&snap));
    assert_eq!(
        fullscreen_bottom_chrome_height(&tui),
        fullscreen_bottom_chrome_height(&snap)
    );
}

#[test]
fn s4_a_2_snapshot_prompt_uses_shell_kind_for_attached_session() {
    let mut state = make_state_with_lines();
    state.ui.focus = crate::chat::sessions::FocusTarget::Session { seq: 1 };
    state.ui.sessions_entries = vec![crate::chat::sessions::SwitcherEntry {
        seq: 1,
        kind: crate::chat::sessions::model::ManagedKind::Shell.as_str(),
        origin: crate::chat::sessions::model::SessionOrigin::User.as_str(),
        status: crate::chat::sessions::model::ManagedStatus::Running.as_str(),
        title: "echo ok".to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        token_usage_records: Vec::new(),
        idle_warning: false,
    }];
    let snap = state.build_ui_snapshot(12);

    let (span, width) = prompt_indicator(
        snap.focus(),
        snap.ascii_fallback(),
        snap.focus()
            .session_seq()
            .and_then(|seq| focused_session_kind(&snap, seq)),
    );

    assert!(
        span.content.contains("shell #1"),
        "attached shell prompt must identify the ManagedKind: {}",
        span.content
    );
    assert!(
        !span.content.contains("agent #1"),
        "shell attach prompt must not fall back to agent: {}",
        span.content
    );
    assert_eq!(width, UnicodeWidthStr::width(span.content.as_ref()));
}

#[test]
fn s4_a_2_transcript_view_and_switcher_parity() {
    let mut state = make_state_with_lines();
    let transcript_entry = transcript_switcher_entry();
    let transcript_view = build_transcript_view(&state.session.title, &state.ui.conversation_lines, 0);
    state.ui.focus = crate::chat::sessions::FocusTarget::Transcript;
    state.ui.switcher = Some(crate::chat::sessions::SwitcherState::new(vec![
        transcript_entry.clone(),
    ]));
    state.ui.active_session_view = Some(transcript_view.clone());
    let snap = state.build_ui_snapshot(9);

    let mut tui = TuiState::new(&state.session.provider, &state.session.model);
    tui.session_title = state.session.title.clone();
    tui.turn_count = state.ui.turn_count;
    tui.ascii_fallback = state.ui.ascii_fallback;
    tui.conversation_lines = state.ui.conversation_lines.clone();
    tui.input = state.ui.input.clone();
    tui.focus = crate::chat::sessions::FocusTarget::Transcript;
    tui.switcher = Some(crate::chat::sessions::SwitcherState::new(vec![transcript_entry]));
    tui.active_session_view = Some(transcript_view);

    assert_eq!(BottomChromeView::focus(&tui), BottomChromeView::focus(&snap));
    assert_eq!(
        BottomChromeView::active_session_view(&tui),
        BottomChromeView::active_session_view(&snap)
    );
    let tui_switcher = BottomChromeView::switcher(&tui).expect("tui transcript switcher");
    let snap_switcher = BottomChromeView::switcher(&snap).expect("snapshot transcript switcher");
    let [tui_entry] = tui_switcher.entries.as_slice() else {
        panic!("TuiState transcript switcher should expose exactly one entry");
    };
    let [snap_entry] = snap_switcher.entries.as_slice() else {
        panic!("UiSnapshot transcript switcher should expose exactly one entry");
    };
    assert!(tui_entry.is_transcript());
    assert_eq!(tui_entry.seq, 0);
    assert_eq!(tui_entry.seq, snap_entry.seq);
    assert_eq!(tui_entry.kind, snap_entry.kind);
    assert_eq!(tui_entry.origin, snap_entry.origin);
    assert_eq!(tui_entry.status, snap_entry.status);
    assert_eq!(tui_entry.title, snap_entry.title);
}

/// Fullscreen chrome parity: tool cards live in the transcript/panel, so
/// bottom chrome height must stay identical for TuiState and UiSnapshot.
#[test]
fn s4_a_2_fullscreen_chrome_parity_tool_card() {
    let mut state = make_state_with_lines();
    state.ui.conversation_lines.push(ConversationLine::ToolResult {
        tool_name: "memory_recall".to_string(),
        args_preview: "{\"q\":\"x\"}".to_string(),
        args_full: "{\"query\":\"x\"}".to_string(),
        result: Some("ok".to_string()),
        status: ToolStatus::Done,
        elapsed_ms: Some(123),
        folded: true,
    });
    let snap = state.build_ui_snapshot(7);
    let mut tui = TuiState::new(&state.session.provider, &state.session.model);
    tui.session_title = state.session.title.clone();
    tui.turn_count = state.ui.turn_count;
    tui.ascii_fallback = state.ui.ascii_fallback;
    tui.conversation_lines = state.ui.conversation_lines.clone();
    tui.streaming = state.stream.primary_streaming_draft().cloned();
    tui.input = state.ui.input.clone();

    assert_eq!(
        fullscreen_bottom_chrome_height(&tui),
        fullscreen_bottom_chrome_height(&snap),
        "heights must match when a tool card is present"
    );
}
