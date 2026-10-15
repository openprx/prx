#![cfg(test)]

use super::super::*;
use crate::chat::action::Action;
use crate::chat::session::ToolCallSummary;
use tokio_util::sync::CancellationToken;

fn s() -> ChatState {
    ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
}

/// S2.5 P1-B: RecordAssistantTurn backfills tool_calls into session.turns.last_mut().tool_calls.
#[test]
fn s2_5_p1_b_assistant_turn_carries_tool_calls() {
    let mut state = s();
    let _ = state.reduce(Action::RecordUserTurn("question".to_string()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-p1b-1".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "shell".to_string(),
        args: r#"{"cmd":"ls"}"#.to_string(),
    });
    let _ = state.reduce(Action::ToolFinished {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "shell".to_string(),
        success: true,
        duration_ms: 12,
        result: Some("ok".to_string()),
    });
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "answer".to_string(),
    });

    let last = state.session.turns.last().expect("test: assistant turn");
    assert_eq!(last.role, "assistant");
    assert_eq!(
        last.tool_calls.len(),
        1,
        "the 1 tool_call of this turn must be backfilled"
    );
    let call: &ToolCallSummary = last.tool_calls.first().expect("test: tool_calls[0]");
    assert_eq!(call.name, "shell");
    assert!(call.success);
    assert_eq!(call.args_preview, r#"command="ls""#);

    // after the backfill the ControlState buffer must be empty (mem::take + clear).
    assert!(!state.control.tool_buffers.contains_key(&ToolTaskKey::Primary));
}

/// S2.5 P1-B: several tools in the same turn are aggregated in order.
#[test]
fn s2_5_p1_b_multi_tool_aggregates_in_turn() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-p1b-2".to_string(),
        cancel: CancellationToken::new(),
    });
    for (i, ok) in [(1u8, true), (2, false), (3, true)] {
        let name = format!("tool{i}");
        let _ = state.reduce(Action::ToolStarted {
            task_id: None,
            sequence: None,
            tool_call_id: None,
            name: name.clone(),
            args: format!("args-{i}"),
        });
        let _ = state.reduce(Action::ToolFinished {
            task_id: None,
            sequence: None,
            tool_call_id: None,
            name,
            success: ok,
            duration_ms: 10,
            result: None,
        });
    }
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "a".to_string(),
    });

    let last = state.session.turns.last().expect("test: assistant turn");
    assert_eq!(last.tool_calls.len(), 3);
    let t0 = last.tool_calls.first().expect("test: tool_calls[0]");
    assert_eq!(t0.name, "tool1");
    assert!(t0.success);
    let t1 = last.tool_calls.get(1).expect("test: tool_calls[1]");
    assert_eq!(t1.name, "tool2");
    assert!(!t1.success);
    let t2 = last.tool_calls.get(2).expect("test: tool_calls[2]");
    assert_eq!(t2.name, "tool3");
    assert!(t2.success);
}

/// S2.5 P1-B: the turn boundary (StreamCompleted) clears the buffer, no cross-turn pollution.
#[test]
fn s2_5_p1_b_turn_boundary_clears_buffer() {
    let mut state = s();

    // Turn 1: tools accumulate, but RecordAssistantTurn is deliberately skipped for StreamCompleted.
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-p1b-3a".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "leftover".to_string(),
        args: "x".to_string(),
    });
    let _ = state.reduce(Action::ToolFinished {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "leftover".to_string(),
        success: true,
        duration_ms: 1,
        result: None,
    });
    assert_eq!(
        state
            .control
            .tool_buffers
            .get(&ToolTaskKey::Primary)
            .map_or(0, |buffer| buffer.tool_calls.len()),
        1
    );
    let _ = state.reduce(Action::StreamCompleted {
        draft_id: "d-p1b-3a".to_string(),
        final_text: "x".to_string(),
        reasoning: String::new(),
    });
    // after the StreamCompleted fallback clear the buffer is empty.
    assert!(!state.control.tool_buffers.contains_key(&ToolTaskKey::Primary));

    // Turn 2: RecordAssistantTurn must get empty tool_calls (not polluted by Turn 1 leftovers).
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-p1b-3b".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "clean".to_string(),
    });
    let last = state.session.turns.last().expect("test: turn 2 assistant");
    assert_eq!(
        last.tool_calls.len(),
        0,
        "Turn 2 must not inherit Turn 1 leftover tool_calls"
    );
}

/// S2.5 P1-B: a cancelled stream clears the buffer (the user pressed Ctrl+C mid-turn).
#[test]
fn s2_5_p1_b_stream_cancelled_clears_buffer() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-p1b-4".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "partial".to_string(),
        args: "...".to_string(),
    });
    // at this point the args buffer holds content
    assert_eq!(
        state
            .control
            .tool_buffers
            .get(&ToolTaskKey::Primary)
            .map_or(0, |buffer| buffer.tool_args.len()),
        1
    );

    let _ = state.reduce(Action::StreamCancelled {
        draft_id: "d-p1b-4".to_string(),
    });
    assert!(
        !state.control.tool_buffers.contains_key(&ToolTaskKey::Primary),
        "the buffer must be cleared after cancel"
    );

    // the same check for StreamFailed.
    let mut state2 = s();
    let _ = state2.reduce(Action::TurnStarted {
        draft_id: "d-p1b-4b".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state2.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "partial2".to_string(),
        args: "...".to_string(),
    });
    let _ = state2.reduce(Action::StreamFailed {
        draft_id: "d-p1b-4b".to_string(),
        err: "timeout".to_string(),
        retryable: true,
    });
    assert!(!state2.control.tool_buffers.contains_key(&ToolTaskKey::Primary));
}

/// S2.5 P1-B: extends the fixB C1 parity pattern — after RecordAssistantTurn,
/// session.turns.last().tool_calls must contain this turn's accumulated ToolFinished entries.
/// It simulates the enriched package path of the reducer route in Both mode, showing the reducer
/// persistence path now carries tool_calls (closing the FIXME(S2.5) gap).
#[test]
fn s2_5_p1_b_both_parity_includes_tool_calls() {
    let mut state = s();
    // simulate a full turn package: user → turn started → several tools → assistant.
    let _ = state.reduce(Action::RecordUserTurn("ask".to_string()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-parity".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "search".to_string(),
        args: r#"{"q":"x"}"#.to_string(),
    });
    let _ = state.reduce(Action::ToolFinished {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "search".to_string(),
        success: true,
        duration_ms: 5,
        result: Some("hit".to_string()),
    });
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "fetch".to_string(),
        args: "url".to_string(),
    });
    let _ = state.reduce(Action::ToolFinished {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "fetch".to_string(),
        success: false,
        duration_ms: 30,
        result: Some("404".to_string()),
    });
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "done".to_string(),
    });
    let _ = state.reduce(Action::StreamCompleted {
        draft_id: "d-parity".to_string(),
        final_text: "done".to_string(),
        reasoning: String::new(),
    });

    // equivalent mirror of legacy session.add_assistant_turn(content, tool_calls).
    let assistant_turn = state
        .session
        .turns
        .iter()
        .rev()
        .find(|t| t.role == "assistant")
        .expect("test: assistant turn exists");
    assert_eq!(assistant_turn.tool_calls.len(), 2);
    let c0 = assistant_turn.tool_calls.first().expect("test: tool_calls[0]");
    assert_eq!(c0.name, "search");
    assert!(c0.success);
    let c1 = assistant_turn.tool_calls.get(1).expect("test: tool_calls[1]");
    assert_eq!(c1.name, "fetch");
    assert!(!c1.success);

    // check that the turns written by build_session_snapshot also carry tool_calls.
    let snap = state.build_session_snapshot();
    let snap_assistant = snap
        .turns
        .iter()
        .rev()
        .find(|t| t.role == "assistant")
        .expect("test: snapshot assistant turn");
    assert_eq!(
        snap_assistant.tool_calls.len(),
        2,
        "the turns persisted by build_session_snapshot must carry tool_calls"
    );
}
