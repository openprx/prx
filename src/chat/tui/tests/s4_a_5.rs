#![cfg(test)]

use super::*;
use crate::channels::terminal::TuiMirrorSink;
use crate::chat::action::Action;
use crate::chat::dispatcher::ChatDispatcher;
use crate::chat::state::ChatState;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// No UiEvent translation method panics, and push_system really dispatches
/// SystemMessageAdded.
#[tokio::test]
async fn s4_a_5_snapshot_sink_each_event_maps_to_action() {
    let (dispatcher, mut rx) = ChatDispatcher::new();
    let sink = SnapshotDispatcherSink::new(dispatcher);

    // None of the methods panic — most of them are no-op traces.
    sink.push_assistant("hi");
    sink.push_tool_started("Bash", "{\"cmd\":\"ls\"}");
    let _ = sink.mark_tool_finished("Bash", true, 123);
    sink.start_stream("d-1");
    sink.update_stream("d-1", "He", 1);
    sink.finalize_stream("d-1", "Hello");
    sink.cancel_stream("d-1");
    // push_system must dispatch SystemMessageAdded — check the channel receives it.
    sink.push_system("system note");

    let action = rx.recv().await.expect("dispatcher should receive SystemMessageAdded");
    match action {
        Action::SystemMessageAdded { text } => assert_eq!(text, "system note"),
        other => panic!("expected SystemMessageAdded, got {other:?}"),
    }
}

/// The dispatch_or_log path does not panic when the channel is full
/// (Backpressured fallback).
#[tokio::test]
async fn s4_a_5_sink_handles_dispatcher_full_gracefully() {
    let (dispatcher, _rx) = ChatDispatcher::new();
    let sink = SnapshotDispatcherSink::new(dispatcher);
    // Call push_system repeatedly; once the channel cap (ACTION_CHANNEL_CAPACITY)
    // is reached, dispatch_or_log takes the backpressured path without panicking.
    for i in 0..2000 {
        sink.push_system(&format!("note {i}"));
    }
    // Reaching this point is the assertion (no panic / no hang).
}

/// In Pure mode Stream UiEvents reach the reducer through driver Actions, so
/// snapshot.streaming must be Some(...) — the Sink itself writes nothing, but the
/// driver path goes through the reducer.
///
/// This test simulates a driver-style dispatch (TurnStarted → reducer writes
/// stream.draft) and checks snapshot.streaming.is_some() — the core Pure-mode path.
#[test]
fn s4_a_5_pure_streaming_propagates_to_snapshot() {
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    let token = CancellationToken::new();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-1".to_string(),
        cancel: token,
    });
    let snap = state.build_ui_snapshot(1);
    assert!(
        snap.streaming.is_some(),
        "TurnStarted must set snapshot.streaming = Some in Pure mode"
    );
    let draft = snap.streaming.as_ref().expect("checked above");
    assert_eq!(draft.draft_id, "d-1");
}

/// In Pure mode a ToolStarted Action reaches the reducer and a ToolResult card
/// (Running) shows up in snapshot.conversation_lines.
#[test]
fn s4_a_5_pure_tool_card_appears_in_snapshot() {
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "Bash".to_string(),
        args: "{\"cmd\":\"ls\"}".to_string(),
    });
    let snap = state.build_ui_snapshot(1);
    assert!(
        snap.conversation_lines
            .iter()
            .any(|l| matches!(l, ConversationLine::ToolResult { tool_name, .. } if tool_name == "Bash")),
        "ToolStarted must make a Bash ToolResult card appear in the snapshot in Pure mode"
    );
}

/// In Pure mode the banner goes through a SystemMessageAdded Action and the
/// reducer pushes one line; chat_mirror also writes in the other modes — but under
/// Pure it must write nothing.
///
/// This test unit-checks the reducer path: SystemMessageAdded → snapshot contains
/// a ConversationLine::System.
#[test]
fn s4_a_5_pure_banner_via_dispatch_not_mirror() {
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    let _ = state.reduce(Action::SystemMessageAdded {
        text: "prx 0.3.6 mock/mock".to_string(),
    });
    let snap = state.build_ui_snapshot(1);
    let has_banner = snap
        .conversation_lines
        .iter()
        .any(|l| matches!(l, ConversationLine::System { content } if content.contains("mock/mock")));
    assert!(
        has_banner,
        "the Pure-mode banner must reach the reducer via SystemMessageAdded, snapshot contains a System line"
    );
}
