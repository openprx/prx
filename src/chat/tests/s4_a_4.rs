#![cfg(test)]

use super::*;
use crate::chat::state::{ChatState, UiSnapshot};
use crate::chat::tui::{ConversationLine, TuiState};
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

fn build_state_with_lines() -> ChatState {
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    state.ui.conversation_lines.push(ConversationLine::User {
        content: "a".to_string(),
    });
    state.ui.conversation_lines.push(ConversationLine::Assistant {
        content: "b".to_string(),
    });
    state
}

/// RenderSource enum dispatch: both the mirror and the snapshot construction take the correct
/// branch.
#[test]
fn s4_a_4_render_source_enum_dispatch() {
    // Mirror path.
    let tui = TuiState::new("p", "m");
    let mirror = Arc::new(parking_lot::Mutex::new(tui));
    let src_mirror = RenderSource::Mirror(Arc::clone(&mirror));
    src_mirror.with_view(|view| {
        assert_eq!(view.provider(), "p");
        assert_eq!(view.model(), "m");
    });

    // Snapshot path.
    let snap = Arc::new(UiSnapshot::initial(Arc::from("ps"), Arc::from("ms")));
    let (_tx, rx) = watch::channel(snap);
    let src_snap = RenderSource::Snapshot(rx);
    src_snap.with_view(|view| {
        assert_eq!(view.provider(), "ps");
        assert_eq!(view.model(), "ms");
    });
}

#[test]
fn snapshot_render_source_syncs_provider_worker_status_into_key_mirror() {
    let mirror = Arc::new(parking_lot::Mutex::new(TuiState::new("p", "m")));
    let mut state = ChatState::new(Arc::from("ps"), Arc::from("ms"), CancellationToken::new());
    state.ui.provider_worker_status = crate::chat::action::ProviderWorkerStatus {
        running: 1,
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 0,
        finalized_total_tokens: 0,
        oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis()),
        rows: vec![crate::chat::action::ProviderWorkerStatusRow {
            task_id: 42,
            sequence: 7,
            kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
            state: crate::chat::action::ProviderWorkerRowState::Running,
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            finalized_total_tokens: None,
            completion_ready: false,
            recent_tool_call: None,
        }],
    };
    let snap = Arc::new(state.build_ui_snapshot(1));
    let (_tx, rx) = watch::channel(snap);
    let src = RenderSource::Snapshot(rx);

    sync_key_mirror_observation_state(&src, &mirror);

    let status = mirror.lock().provider_worker_status.clone();
    assert_eq!(status.running, 1);
    assert_eq!(status.rows.first().map(|row| row.sequence), Some(7));
}

#[test]
fn phase2_snapshot_mirror_sync_uses_focused_worker_draft_not_primary() {
    let mirror = Arc::new(parking_lot::Mutex::new(TuiState::new("p", "m")));
    {
        let mut guard = mirror.lock();
        guard.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 20 };
    }
    let mut state = ChatState::new(Arc::from("ps"), Arc::from("ms"), CancellationToken::new());
    state.ui.provider_worker_status = crate::chat::action::ProviderWorkerStatus {
        running: 2,
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 0,
        finalized_total_tokens: 0,
        oldest_started_at_ms: Some(0),
        rows: vec![
            crate::chat::action::ProviderWorkerStatusRow {
                task_id: 10,
                sequence: 10,
                kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                state: crate::chat::action::ProviderWorkerRowState::Running,
                started_at_ms: 0,
                finalized_total_tokens: None,
                completion_ready: false,
                recent_tool_call: None,
            },
            crate::chat::action::ProviderWorkerStatusRow {
                task_id: 20,
                sequence: 20,
                kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                state: crate::chat::action::ProviderWorkerRowState::Running,
                started_at_ms: 0,
                finalized_total_tokens: None,
                completion_ready: false,
                recent_tool_call: None,
            },
        ],
    };
    let _ = state.reduce(crate::chat::action::Action::StartLLMTurn {
        provider_turn_task_id: None,
        provider_turn_sequence: Some(10),
        draft_id: "draft-a".to_string(),
        history: vec![crate::providers::ChatMessage::user("first")],
        compaction_guard_history: None,
        compaction_config: None,
        cancel: CancellationToken::new(),
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });
    let _ = state.reduce(crate::chat::action::Action::StartLLMTurn {
        provider_turn_task_id: None,
        provider_turn_sequence: Some(20),
        draft_id: "draft-b".to_string(),
        history: vec![crate::providers::ChatMessage::user("second")],
        compaction_guard_history: None,
        compaction_config: None,
        cancel: CancellationToken::new(),
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });
    let _ = state.reduce(crate::chat::action::Action::StreamChunkReceived {
        draft_id: "draft-a".to_string(),
        delta: "A primary live".to_string(),
        version: 1,
    });
    let _ = state.reduce(crate::chat::action::Action::StreamChunkReceived {
        draft_id: "draft-b".to_string(),
        delta: "B focused live".to_string(),
        version: 1,
    });
    let snap = Arc::new(state.build_ui_snapshot(1));
    assert_eq!(
        snap.streaming.as_ref().map(|draft| draft.draft_id.as_str()),
        Some("draft-a")
    );
    assert_eq!(
        snap.streaming_draft_for_worker(20)
            .map(|draft| draft.accumulated.as_str()),
        Some("B focused live")
    );
    let (_tx, rx) = watch::channel(snap);
    let src = RenderSource::Snapshot(rx);

    sync_key_mirror_observation_state(&src, &mirror);

    let guard = mirror.lock();
    assert_eq!(
        guard
            .streaming_draft_for_worker(20)
            .map(|draft| draft.accumulated.as_str()),
        Some("B focused live")
    );
    let view = guard.active_session_view.as_ref().expect("focused worker view");
    let text = view.lines.join("\n");
    assert!(text.contains("assistant streaming: B focused live"), "{text}");
    assert!(!text.contains("A primary live"), "{text}");
    drop(guard);

    let (dispatcher, mut action_rx) = crate::chat::dispatcher::ChatDispatcher::new();
    open_provider_worker_view(&mirror, &dispatcher, None, 20);
    match action_rx.try_recv().expect("switcher close action") {
        crate::chat::action::Action::SwitcherClosed => {}
        other => panic!("expected SwitcherClosed, got {other:?}"),
    }
    match action_rx.try_recv().expect("focus action") {
        crate::chat::action::Action::SessionFocusChanged { focus } => {
            assert_eq!(focus, crate::chat::sessions::FocusTarget::Worker { sequence: 20 });
        }
        other => panic!("expected SessionFocusChanged, got {other:?}"),
    }
    match action_rx.try_recv().expect("active worker view action") {
        crate::chat::action::Action::ActiveSessionViewUpdated { view } => {
            let text = view.expect("worker view").lines.join("\n");
            assert!(text.contains("assistant streaming: B focused live"), "{text}");
            assert!(!text.contains("A primary live"), "{text}");
        }
        other => panic!("expected ActiveSessionViewUpdated, got {other:?}"),
    }
}

#[test]
fn plain_character_keys_are_not_logged_as_tui_input_events() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    assert!(!super::should_log_tui_key_event(&KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::NONE,
    )));
    assert!(super::should_log_tui_key_event(&KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )));
    assert!(super::should_log_tui_key_event(&KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
}

#[test]
fn plain_character_key_detection_is_limited_to_unmodified_chars() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    assert!(super::is_plain_character_key(&KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::NONE,
    )));
    assert!(!super::is_plain_character_key(&KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )));
    assert!(!super::is_plain_character_key(&KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
}

#[test]
fn plain_character_key_char_excludes_control_and_release_events() {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    assert_eq!(
        super::plain_character_from_key(&KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
        Some('p')
    );
    assert_eq!(
        super::plain_character_from_key(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        None
    );

    let release = KeyEvent {
        code: KeyCode::Char('p'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Release,
        state: crossterm::event::KeyEventState::NONE,
    };
    assert_eq!(super::plain_character_from_key(&release), None);
}

/// Verify that on the snapshot path with_view sees the new content after the watch pushes a new
/// value.
#[tokio::test]
async fn s4_a_4_unified_loop_redraw_on_snapshot_change() {
    let mut state = build_state_with_lines();
    let snap0 = Arc::new(state.build_ui_snapshot(1));
    let (tx, rx) = watch::channel(Arc::clone(&snap0));
    let src = RenderSource::Snapshot(rx);

    // Initially: 2 lines.
    src.with_view(|view| assert_eq!(view.conversation_lines().len(), 2));

    // Push a new snapshot: 3 lines (pushing directly bypasses reduce, so the cache must be
    // cleared by hand for build_ui_snapshot to rebuild the Arc).
    let mut state2 = state;
    state2.ui.conversation_lines.push(ConversationLine::System {
        content: "c".to_string(),
    });
    // After writing lines while bypassing reduce, the cache must be cleared (the shared Arc cache
    // introduced in S4-A Commit B).
    let _ = state2.reduce_tracked(crate::chat::action::Action::RedrawRequested);
    // RedrawRequested marks dirty=true, which clears cached_lines_arc, so the next build rebuilds
    // the Arc and reflects the new push.
    let snap1 = Arc::new(state2.build_ui_snapshot(2));
    tx.send(snap1).expect("send snap1");

    // The new view must see 3 lines.
    src.with_view(|view| {
        assert_eq!(
            view.conversation_lines().len(),
            3,
            "the new line must be visible after the watch push"
        );
    });
}
