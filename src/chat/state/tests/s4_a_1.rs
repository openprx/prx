#![cfg(test)]

use super::*;
use crate::chat::tui::ConversationLine;
use tokio_util::sync::CancellationToken;

fn make_state() -> ChatState {
    ChatState::new(
        Arc::from("test-provider"),
        Arc::from("test-model"),
        CancellationToken::new(),
    )
}

#[test]
fn s4_a_1_snapshot_initial_zero_revision() {
    let snap = UiSnapshot::initial(Arc::from("p"), Arc::from("m"));
    assert_eq!(snap.revision, 0);
    assert_eq!(&*snap.provider, "p");
    assert_eq!(&*snap.model, "m");
    assert!(snap.conversation_lines.is_empty());
    assert_eq!(snap.turn_count, 0);
    assert!(snap.streaming.is_none());
}

#[test]
fn s4_a_1_snapshot_clone_is_arc_shallow() {
    // check that conversation_lines is Arc-shared: after cloning the snapshot both Arcs
    // point at the same underlying Vec, so strong_count is at least 2.
    let mut state = make_state();
    state.ui.conversation_lines.push(ConversationLine::User {
        content: "hi".to_string(),
    });
    let snap = state.build_ui_snapshot(1);
    let snap2 = snap.clone();
    // Arc::strong_count(&snap.conversation_lines) counts snap + snap2 = 2
    assert!(
        Arc::strong_count(&snap.conversation_lines) >= 2,
        "a snapshot clone must share the conversation_lines Arc, count={}",
        Arc::strong_count(&snap.conversation_lines)
    );
    assert_eq!(snap2.revision, 1);
}

#[test]
fn s4_a_1_build_after_user_message_includes_line() {
    let mut state = make_state();
    state.ui.conversation_lines.push(ConversationLine::User {
        content: "hello".to_string(),
    });
    let snap = state.build_ui_snapshot(7);
    assert_eq!(snap.revision, 7);
    assert_eq!(snap.conversation_lines.len(), 1);
    match snap.conversation_lines.first() {
        Some(ConversationLine::User { content }) => assert_eq!(content, "hello"),
        other => panic!("expected User line, got {other:?}"),
    }
}

#[test]
fn s4_a_1_ui_dirty_true_on_record_user_turn_via_runtime_fallback() {
    // RecordUserTurn is statically false (it writes session, not ui);
    // the runtime snapshot_dirty_fields is unchanged too → false overall.
    // This case verifies that writing session does not incidentally set dirty.
    let mut state = make_state();
    let (_effects, dirty) = state.reduce_tracked(Action::RecordUserTurn("q".into()));
    assert!(!dirty, "RecordUserTurn must not set ui_dirty");
}

#[test]
fn s4_a_1_tool_progress_dirty_but_retry_trace_only_is_clean() {
    let mut state = make_state();
    let (_e, d) = state.reduce_tracked(Action::ToolProgress { iteration: 1 });
    assert!(d, "ToolProgress must dirty Pure snapshots so progress is visible");
    let (_e, d2) = state.reduce_tracked(Action::StreamRetryAttempt {
        attempt: 1,
        reason: "x".into(),
    });
    assert!(!d2, "StreamRetryAttempt must not be dirty");
}

#[test]
fn s4_a_1_ui_dirty_true_on_stream_completed() {
    // full flow: TurnStarted registers the draft first, then StreamCompleted finalizes it.
    let mut state = make_state();
    let token = CancellationToken::new();
    let (_e, d_start) = state.reduce_tracked(Action::TurnStarted {
        draft_id: "d1".into(),
        cancel: token,
    });
    assert!(d_start, "TurnStarted must be dirty (stream.draft changed)");
    let (_e, d_done) = state.reduce_tracked(Action::StreamCompleted {
        draft_id: "d1".into(),
        final_text: "hi".into(),
        reasoning: String::new(),
    });
    assert!(
        d_done,
        "StreamCompleted must be dirty (conversation_lines + stream.draft)"
    );
}

#[test]
fn s4_a_1_ui_dirty_true_on_system_message_added() {
    let mut state = make_state();
    let (_e, d) = state.reduce_tracked(Action::SystemMessageAdded { text: "banner".into() });
    assert!(d, "SystemMessageAdded must be dirty (pushed to conversation_lines)");
}

#[test]
fn s4_a_1_build_session_title_into_arc() {
    // session.title is a String while the snapshot holds Arc<str> — check the conversion.
    let mut state = make_state();
    state.session.title = "my chat".to_string();
    let snap = state.build_ui_snapshot(2);
    assert_eq!(&*snap.session_title, "my chat");
}
