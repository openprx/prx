#![cfg(test)]

use super::*;
use std::sync::Arc;

#[test]
fn open_transcript_view_sets_read_only_focus_without_session_seq() {
    let mirror = Arc::new(parking_lot::Mutex::new(tui::TuiState::new("p", "m")));
    {
        let mut guard = mirror.lock();
        guard.session_title = "main chat".to_string();
        guard.conversation_lines.push(tui::ConversationLine::User {
            content: "hello".to_string(),
        });
    }
    let (dispatcher, mut action_rx) = crate::chat::dispatcher::ChatDispatcher::new();
    let (redraw_tx, mut redraw_rx) = mpsc::channel(1);

    open_transcript_view(&mirror, &dispatcher, Some(&redraw_tx), None);

    {
        let guard = mirror.lock();
        assert_eq!(guard.focus, crate::chat::sessions::FocusTarget::Transcript);
        assert_eq!(
            guard.focus.session_seq(),
            None,
            "transcript focus must not become a steerable attached session"
        );
        assert!(
            guard
                .active_session_view
                .as_ref()
                .is_some_and(|view| view.kind == crate::chat::sessions::model::ManagedKind::Transcript.as_str())
        );
    }
    match action_rx.try_recv().expect("focus action") {
        crate::chat::action::Action::SessionFocusChanged { focus } => {
            assert_eq!(focus, crate::chat::sessions::FocusTarget::Transcript);
            assert_eq!(focus.session_seq(), None);
        }
        other => panic!("expected SessionFocusChanged, got {other:?}"),
    }
    match action_rx.try_recv().expect("active view action") {
        crate::chat::action::Action::ActiveSessionViewUpdated { view } => {
            let view = view.expect("transcript view");
            assert_eq!(
                view.kind,
                crate::chat::sessions::model::ManagedKind::Transcript.as_str()
            );
        }
        other => panic!("expected ActiveSessionViewUpdated, got {other:?}"),
    }
    assert!(redraw_rx.try_recv().is_ok(), "open should request redraw");
}

#[test]
fn open_transcript_view_prefers_redux_snapshot_over_empty_mirror() {
    let mirror = Arc::new(parking_lot::Mutex::new(tui::TuiState::new("p", "m")));
    {
        let guard = mirror.lock();
        assert!(
            guard.conversation_lines.is_empty(),
            "test must model the real Redux-only shape: mirror transcript is empty"
        );
    }

    let mut state = crate::chat::state::ChatState::new(
        Arc::from("p"),
        Arc::from("m"),
        tokio_util::sync::CancellationToken::new(),
    );
    state.session.title = "redux chat".to_string();
    state.ui.conversation_lines.push(tui::ConversationLine::User {
        content: "redux-only user".to_string(),
    });
    state.ui.conversation_lines.push(tui::ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "echo ok".to_string(),
        args_full: "echo ok".to_string(),
        status: tui::ToolStatus::Done,
        elapsed_ms: Some(12),
        result: Some("ok".to_string()),
        folded: false,
    });
    let snapshot = Arc::new(state.build_ui_snapshot(1));
    let (_snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(snapshot);

    let (dispatcher, mut action_rx) = crate::chat::dispatcher::ChatDispatcher::new();

    open_transcript_view(&mirror, &dispatcher, None, Some(&snapshot_rx));

    let view = {
        let guard = mirror.lock();
        guard.active_session_view.clone().expect("transcript view")
    };
    assert_eq!(view.title, "redux chat");
    assert!(
        view.lines.iter().any(|line| line.contains("redux-only user")),
        "transcript must render Redux-only user line: {:?}",
        view.lines
    );
    assert!(
        view.lines.iter().any(|line| line.contains("tool shell")),
        "transcript must render Redux-only tool output line: {:?}",
        view.lines
    );
    assert!(
        view.lines.iter().all(|line| !line.contains("(transcript is empty)")),
        "snapshot-backed transcript must not fall back to empty mirror: {:?}",
        view.lines
    );

    match action_rx.try_recv().expect("focus action") {
        crate::chat::action::Action::SessionFocusChanged { focus } => {
            assert_eq!(focus, crate::chat::sessions::FocusTarget::Transcript);
        }
        other => panic!("expected SessionFocusChanged, got {other:?}"),
    }
    match action_rx.try_recv().expect("active view action") {
        crate::chat::action::Action::ActiveSessionViewUpdated { view } => {
            let view = view.expect("transcript view action");
            assert!(
                view.lines.iter().any(|line| line.contains("redux-only user")),
                "dispatched view must carry Redux-only transcript: {:?}",
                view.lines
            );
        }
        other => panic!("expected ActiveSessionViewUpdated, got {other:?}"),
    }
}

#[test]
fn close_transcript_view_returns_main_focus_and_clears_view() {
    let mirror = Arc::new(parking_lot::Mutex::new(tui::TuiState::new("p", "m")));
    {
        let mut guard = mirror.lock();
        guard.focus = crate::chat::sessions::FocusTarget::Transcript;
        guard.active_session_view = Some(tui::build_transcript_view("", &[], 0));
    }
    let (dispatcher, mut action_rx) = crate::chat::dispatcher::ChatDispatcher::new();
    let (redraw_tx, mut redraw_rx) = mpsc::channel(1);

    close_transcript_view(&mirror, &dispatcher, &redraw_tx);

    {
        let guard = mirror.lock();
        assert_eq!(guard.focus, crate::chat::sessions::FocusTarget::Main);
        assert!(guard.active_session_view.is_none());
    }
    match action_rx.try_recv().expect("focus action") {
        crate::chat::action::Action::SessionFocusChanged { focus } => {
            assert_eq!(focus, crate::chat::sessions::FocusTarget::Main);
        }
        other => panic!("expected SessionFocusChanged, got {other:?}"),
    }
    match action_rx.try_recv().expect("active view action") {
        crate::chat::action::Action::ActiveSessionViewUpdated { view } => {
            assert!(view.is_none());
        }
        other => panic!("expected ActiveSessionViewUpdated, got {other:?}"),
    }
    assert!(redraw_rx.try_recv().is_ok(), "close should request redraw");
}
