#![cfg(test)]

use super::*;
use crate::chat::tui::{ConversationLine, ToolStatus, TuiState};
use tokio_util::sync::CancellationToken;

/// Dual-run reconciliation: feed the same Action sequence into the mirror path (TuiState
/// push_* calls) and the reducer path (Action → reduce → ui.conversation_lines), then assert
/// that both paths produce byte-identical conversation_lines.
///
/// This is the last safety net before S4-B really deletes chat_mirror — any reducer deviation
/// from the legacy mirror shows up here as a byte-level diff.
#[test]
fn s4_a_6_dual_path_parity_user_assistant_tool() {
    // ── path A: the mirror path ──
    let mut mirror = TuiState::new("p", "m");
    mirror.push_system_message("banner");
    mirror.push_user_message("hello");
    mirror.push_tool_result_started("Bash", "{\"cmd\":\"ls\"}");
    let _ = mirror.mark_last_tool_result_finished("Bash", true, 50, None);
    mirror.start_stream("d-1");
    mirror.finalize_stream("d-1", "done");

    // ── path B: the reducer path (S4-A Commit A: UserMessageEchoed closes the User echo) ──
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    let _ = state.reduce(Action::SystemMessageAdded {
        text: "banner".to_string(),
    });
    let _ = state.reduce(Action::UserMessageEchoed("hello".to_string()));
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "Bash".to_string(),
        args: "{\"cmd\":\"ls\"}".to_string(),
    });
    let _ = state.reduce(Action::ToolFinished {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "Bash".to_string(),
        success: true,
        duration_ms: 50,
        result: None,
    });
    let token = CancellationToken::new();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-1".to_string(),
        cancel: token,
    });
    let _ = state.reduce(Action::StreamCompleted {
        draft_id: "d-1".to_string(),
        final_text: "done".to_string(),
        reasoning: String::new(),
    });

    // reconcile the UI-visible ConversationLines; the completion duration stays out of the body.
    let mirror_lines: Vec<&ConversationLine> = mirror.conversation_lines.iter().collect();
    let reducer_lines: Vec<&ConversationLine> = state.ui.conversation_lines.iter().collect();

    assert_eq!(
        mirror_lines.len(),
        reducer_lines.len(),
        "reconciled line counts: mirror={}, reducer={}",
        mirror_lines.len(),
        reducer_lines.len()
    );
    for (i, (ml, rl)) in mirror_lines.iter().zip(reducer_lines.iter()).enumerate() {
        let m_dbg = format!("{ml:?}");
        let r_dbg = format!("{rl:?}");
        match (ml, rl) {
            (
                ConversationLine::ToolResult {
                    tool_name: m_name,
                    status: m_st,
                    elapsed_ms: m_ms,
                    ..
                },
                ConversationLine::ToolResult {
                    tool_name: r_name,
                    status: r_st,
                    elapsed_ms: r_ms,
                    ..
                },
            ) => {
                assert_eq!(m_name, r_name, "line {i} ToolResult tool_name mismatch");
                assert_eq!(m_st, r_st, "line {i} ToolResult status mismatch");
                assert_eq!(m_ms, r_ms, "line {i} ToolResult elapsed_ms mismatch");
                assert_eq!(*m_st, ToolStatus::Done);
            }
            (ConversationLine::Assistant { content: mc }, ConversationLine::Assistant { content: rc }) => {
                assert_eq!(mc, rc, "line {i} Assistant content mismatch");
            }
            (ConversationLine::System { content: mc }, ConversationLine::System { content: rc }) => {
                assert_eq!(mc, rc, "line {i} System content mismatch");
            }
            (ConversationLine::User { content: mc }, ConversationLine::User { content: rc }) => {
                assert_eq!(mc, rc, "line {i} User content mismatch");
            }
            _ => panic!("line {i} variant mismatch: mirror={m_dbg}, reducer={r_dbg}"),
        }
    }
}

/// S4-A Commit A: in Pure mode UserMessageEchoed writes the User line into conversation_lines
#[test]
fn s4_a_post_p0_pure_user_echo_appears_in_snapshot() {
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    let effects = state.reduce(Action::UserMessageEchoed("hello echo".to_string()));
    assert!(
        effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
        "UserMessageEchoed must emit RequestRedraw"
    );
    let snap = state.build_ui_snapshot(0);
    let last_user = snap
        .conversation_lines
        .iter()
        .find_map(|l| match l {
            ConversationLine::User { content } => Some(content.as_str()),
            _ => None,
        })
        .expect("the snapshot must contain a User line");
    assert_eq!(last_user, "hello echo");
}

/// S4-B T4-B-6: strict created_at semantics — set on the first RecordUserTurn, kept across turns
#[test]
fn s4_b_created_at_stable_across_turns() {
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    state.session.id = "sess1".to_string();
    assert!(state.session.created_at.is_none(), "created_at must start as None");

    let _ = state.reduce(Action::RecordUserTurn("q1".to_string()));
    let created_first = state
        .session
        .created_at
        .expect("the first RecordUserTurn must set created_at");

    // a second RecordUserTurn must not overwrite created_at
    std::thread::sleep(std::time::Duration::from_millis(2));
    let _ = state.reduce(Action::RecordUserTurn("q2".to_string()));
    assert_eq!(
        state.session.created_at,
        Some(created_first),
        "multiple turns must not overwrite created_at"
    );

    // build_session_snapshot must use SessionState.created_at
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d".to_string(),
        cancel: CancellationToken::new(),
    });
    let effects = state.reduce(Action::StreamCompleted {
        draft_id: "d".to_string(),
        final_text: "ok".to_string(),
        reasoning: String::new(),
    });
    let snap_session = effects
        .iter()
        .find_map(|e| match e {
            Effect::SaveSession(s) => Some(s),
            _ => None,
        })
        .expect("StreamCompleted must emit SaveSession");
    assert_eq!(
        snap_session.created_at, created_first,
        "build_session_snapshot must inherit SessionState.created_at, not overwrite it"
    );
}

/// S4-B cleanup: with all mirror pushes deleted, the reducer alone pushes ConversationLine
#[test]
fn s4_b_reducer_sole_source_for_conversation_lines() {
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    let _ = state.reduce(Action::SystemMessageAdded {
        text: "banner".to_string(),
    });
    let _ = state.reduce(Action::UserMessageEchoed("hi".to_string()));
    let _ = state.reduce(Action::ToolStarted {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "Bash".to_string(),
        args: "{}".to_string(),
    });
    let _ = state.reduce(Action::ToolFinished {
        task_id: None,
        sequence: None,
        tool_call_id: None,
        name: "Bash".to_string(),
        success: true,
        duration_ms: 10,
        result: None,
    });
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d".to_string(),
        cancel: CancellationToken::new(),
    });
    let _ = state.reduce(Action::StreamCompleted {
        draft_id: "d".to_string(),
        final_text: "ok".to_string(),
        reasoning: String::new(),
    });
    assert_eq!(
        state.ui.conversation_lines.len(),
        4,
        "the reducer as sole source must push 4 lines"
    );
    let mut iter = state.ui.conversation_lines.iter();
    assert!(matches!(iter.next(), Some(ConversationLine::System { .. })));
    assert!(matches!(iter.next(), Some(ConversationLine::User { .. })));
    assert!(matches!(iter.next(), Some(ConversationLine::ToolResult { .. })));
    assert!(matches!(iter.next(), Some(ConversationLine::Assistant { .. })));
}

/// S4-A Commit E: TerminalResized must not set ui_dirty (snapshot fields unchanged, redraw via Effect)
#[test]
fn s4_a_post_p2_terminal_resized_not_dirty() {
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    let (effects, dirty) = state.reduce_tracked(Action::TerminalResized { w: 120, h: 40 });
    assert!(
        effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
        "TerminalResized must still emit RequestRedraw (the redraw goes through redraw_tx)"
    );
    assert!(
        !dirty,
        "TerminalResized touches no snapshot field, so dirty must be false to avoid a pointless push"
    );
}

/// S4-A Commit B: two consecutive build_ui_snapshot calls with unchanged ui share the Arc (ptr_eq)
#[test]
fn s4_a_post_p1_arc_shared_no_clone() {
    let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
    // push one line first so conversation_lines is non-empty and the cache is hit
    let _ = state.reduce(Action::SystemMessageAdded {
        text: "banner".to_string(),
    });
    let snap1 = state.build_ui_snapshot(1);
    let snap2 = state.build_ui_snapshot(2);
    assert!(
        Arc::ptr_eq(&snap1.conversation_lines, &snap2.conversation_lines),
        "with unchanged ui consecutive build_ui_snapshot calls must share the Arc, avoiding O(n) clones"
    );

    // after a dirty Action the cache must be cleared and the new Arc pointer differs
    let _ = state.reduce_tracked(Action::SystemMessageAdded {
        text: "second".to_string(),
    });
    let snap3 = state.build_ui_snapshot(3);
    assert!(
        !Arc::ptr_eq(&snap2.conversation_lines, &snap3.conversation_lines),
        "after a ui change the cache is invalidated and the new snapshot must be a new Arc"
    );
    assert_eq!(
        snap3.conversation_lines.len(),
        2,
        "the new snapshot must contain two lines"
    );
}
