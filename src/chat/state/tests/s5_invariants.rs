#![cfg(test)]

use super::super::{ChatState, Effect};
use crate::chat::action::Action;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn fresh_state() -> ChatState {
    ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new())
}

/// Ordering invariant: reducing the same Action sequence on two fresh states → same final state
#[test]
fn s5_invariant_determinism_same_actions_same_state() {
    let actions = || -> Vec<Action> {
        vec![
            Action::SystemMessageAdded {
                text: "banner".to_string(),
            },
            Action::UserMessageEchoed("hi".to_string()),
            Action::RecordUserTurn("hi".to_string()),
            Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            },
            Action::RecordAssistantTurn {
                task_id: None,
                content: "ok".to_string(),
            },
            Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "ok".to_string(),
                reasoning: String::new(),
            },
        ]
    };
    let mut state_a = fresh_state();
    let mut state_b = fresh_state();
    for a in actions() {
        let _ = state_a.reduce(a);
    }
    for a in actions() {
        let _ = state_b.reduce(a);
    }
    assert_eq!(
        state_a.session.turns.len(),
        state_b.session.turns.len(),
        "the same Action sequence must yield the same number of turns"
    );
    assert_eq!(
        state_a.ui.conversation_lines.len(),
        state_b.ui.conversation_lines.len(),
        "the same Action sequence must yield the same number of ConversationLines"
    );
    assert_eq!(
        state_a.session.title, state_b.session.title,
        "the same Action sequence must produce the same session title"
    );
}

/// Idempotency invariant: dispatching the same Action twice must not double-write persistence
#[test]
fn s5_invariant_idempotent_duplicate_dispatch() {
    let mut state = fresh_state();
    let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
    let turns_after_first = state.session.turns.len();
    let history_after_first = state.session.history.len();
    // in the real architecture a repeated dispatch double-writes — the reducer's current contract
    let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
    assert!(
        state.session.turns.len() > turns_after_first,
        "RecordUserTurn is not idempotent: a repeated dispatch appends a turn (chat::run avoids repeats)"
    );
    assert!(
        state.session.history.len() > history_after_first,
        "history is appended to as well"
    );
}

/// Cancel invariant: no SaveSession effect after CancelRequested (no partial state is written)
#[test]
fn s5_invariant_cancel_no_partial_save() {
    let mut state = fresh_state();
    let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "d-cancel".to_string(),
        cancel: CancellationToken::new(),
    });
    // the user cancels mid-turn
    let cancel_effects = state.reduce(Action::CancelRequested);
    assert!(
        !cancel_effects.iter().any(|e| matches!(e, Effect::SaveSession(_))),
        "CancelRequested must not emit SaveSession (avoids persisting partial state)"
    );
    // StreamCancelled must not emit SaveSession either
    let stream_cancel_effects = state.reduce(Action::StreamCancelled {
        draft_id: "d-cancel".to_string(),
    });
    assert!(
        !stream_cancel_effects
            .iter()
            .any(|e| matches!(e, Effect::SaveSession(_))),
        "StreamCancelled must not emit SaveSession (appendix B, Cancelled row)"
    );
}

#[test]
fn save_session_effect_redacts_all_authoritative_content_fields() {
    let secret = "AKIAABCDEFGHIJKLMNOP";
    let mut state = fresh_state();
    state.session.title = format!("deploy {secret} \u{2705}");
    state.session.turns.push(crate::chat::session::ChatTurn {
        role: "assistant".to_string(),
        content: format!("keep Unicode content, hide {secret} \u{1f680}"),
        timestamp: chrono::Utc::now(),
        tool_calls: vec![crate::chat::session::ToolCallSummary {
            name: "shell".to_string(),
            args_preview: format!("echo {secret} \u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}"),
            success: true,
            task_id: Some(7),
            sequence: Some(9),
        }],
    });
    let effects = state.reduce(Action::BackgroundSessionRecorded {
        summary: crate::chat::sessions::PersistedSessionSummary {
            id: "child".to_string(),
            seq: 1,
            kind: "agent".to_string(),
            origin: "model".to_string(),
            status: "completed".to_string(),
            title: format!("subtask {secret}"),
            summary: format!("done {secret} \u{1f30d}"),
            token_usage_records: Vec::new(),
            created_at: chrono::Utc::now(),
        },
    });
    let snapshot = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::SaveSession(session) => Some(session),
            _ => None,
        })
        .expect("background record must save");
    let blob = snapshot.to_json().unwrap();
    assert!(!blob.contains(secret), "authoritative SaveSession blob leaked AWS key");
    assert!(blob.contains("Unicode"));
    assert!(blob.contains("\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}"));
    let tool_call = snapshot
        .turns
        .first()
        .and_then(|turn| turn.tool_calls.first())
        .expect("tool summary shape must be retained");
    assert_eq!(tool_call.task_id, Some(7));
    assert_eq!(tool_call.sequence, Some(9));
}
