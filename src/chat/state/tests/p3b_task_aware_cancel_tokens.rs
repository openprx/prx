#![cfg(test)]

use super::super::*;
use crate::chat::action::Action;
use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
use tokio_util::sync::CancellationToken;

fn s() -> ChatState {
    ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
}

fn task_pair() -> ((TurnTaskId, u64), (TurnTaskId, u64)) {
    let mut scheduler = TurnScheduler::new();
    let a = scheduler.enqueue("a", TurnPriority::Normal, 0);
    let b = scheduler.enqueue("b", TurnPriority::Normal, 0);
    let a_seq = scheduler.task(a).expect("test: task a").sequence;
    let b_seq = scheduler.task(b).expect("test: task b").sequence;
    ((a, a_seq), (b, b_seq))
}

fn start_task(state: &mut ChatState, task_id: TurnTaskId, sequence: u64, draft_id: &str, cancel: CancellationToken) {
    let _ = state.reduce(Action::StartLLMTurn {
        provider_turn_task_id: Some(task_id),
        provider_turn_sequence: Some(sequence),
        draft_id: draft_id.to_string(),
        history: Vec::new(),
        compaction_guard_history: None,
        compaction_config: None,
        cancel,
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });
}

fn cancel_tokens(effects: Vec<Effect>) -> Vec<CancellationToken> {
    effects
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::CancelToken(token) => Some(token),
            _ => None,
        })
        .collect()
}

#[test]
fn p3b_two_task_tokens_are_independent() {
    let mut state = s();
    let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
    let token_a = CancellationToken::new();
    let token_b = CancellationToken::new();
    start_task(&mut state, task_a, seq_a, "draft-a", token_a.clone());
    start_task(&mut state, task_b, seq_b, "draft-b", token_b.clone());

    let tokens = cancel_tokens(state.reduce(Action::CancelRequested));

    assert_eq!(tokens.len(), 1);
    for token in tokens {
        token.cancel();
    }
    assert!(token_a.is_cancelled(), "primary task A token must be emitted");
    assert!(!token_b.is_cancelled(), "task B token must remain untouched");
    assert!(!state.control.turn_cancels.contains_key(&task_a));
    assert!(state.control.turn_cancels.contains_key(&task_b));
}

#[test]
fn p3b_cancel_primary_does_not_clear_other_task_tool_buffer() {
    let mut state = s();
    let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
    start_task(&mut state, task_a, seq_a, "draft-a", CancellationToken::new());
    start_task(&mut state, task_b, seq_b, "draft-b", CancellationToken::new());
    let _ = state.reduce(Action::ToolStarted {
        task_id: Some(task_b),
        sequence: Some(seq_b),
        tool_call_id: None,
        name: "grep".to_string(),
        args: r#"{"q":"needle"}"#.to_string(),
    });

    let _ = state.reduce(Action::CancelRequested);

    assert!(state.control.generating, "task B still keeps generation active");
    assert!(
        state
            .stream
            .visible_drafts
            .iter()
            .any(|draft| draft.task_id == Some(task_b))
    );
    assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 1);
    assert!(state.control.turn_cancels.contains_key(&task_b));
}

#[test]
fn p3b_global_state_clears_only_after_all_visible_tasks_are_cancelled() {
    let mut state = s();
    let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
    start_task(&mut state, task_a, seq_a, "draft-a", CancellationToken::new());
    start_task(&mut state, task_b, seq_b, "draft-b", CancellationToken::new());
    let _ = state.reduce(Action::ToolApprovalRequested {
        task_id: Some(task_b),
        tool_id: "tool-b".to_string(),
        name: "shell".to_string(),
        args: "{}".to_string(),
    });

    let _ = state.reduce(Action::CancelRequested);

    assert!(state.control.generating, "B remains visible after cancelling A");
    assert!(
        state.control.active_cancel.is_none(),
        "task turns do not use legacy active_cancel"
    );
    assert!(
        state.ui.pending_tool_approval.is_some(),
        "B approval must not be globally cleared"
    );
    assert!(state.control.turn_cancels.contains_key(&task_b));

    let _ = state.reduce(Action::CancelRequested);

    assert!(!state.control.generating);
    assert!(state.control.active_cancel.is_none());
    assert!(state.control.turn_cancels.is_empty());
    assert!(state.stream.visible_drafts.is_empty(), "expected an empty value");
    assert!(state.ui.pending_tool_approval.is_none());
}

#[test]
fn p3b_shutdown_cancels_all_task_tokens() {
    let mut state = s();
    let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
    let token_a = CancellationToken::new();
    let token_b = CancellationToken::new();
    start_task(&mut state, task_a, seq_a, "draft-a", token_a.clone());
    start_task(&mut state, task_b, seq_b, "draft-b", token_b.clone());

    let effects = state.reduce(Action::ShutdownRequested);
    let tokens = cancel_tokens(effects);

    assert_eq!(tokens.len(), 2, "shutdown must emit both task cancel tokens");
    for token in tokens {
        token.cancel();
    }
    assert!(token_a.is_cancelled());
    assert!(token_b.is_cancelled());
    assert!(state.control.turn_cancels.is_empty());
    assert!(!state.control.generating);
    assert!(state.stream.visible_drafts.is_empty(), "expected an empty value");
}

#[test]
fn p4c_cancel_provider_turn_targets_requested_task_token_only() {
    let mut state = s();
    let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
    let token_a = CancellationToken::new();
    let token_b = CancellationToken::new();
    start_task(&mut state, task_a, seq_a, "draft-a", token_a.clone());
    start_task(&mut state, task_b, seq_b, "draft-b", token_b.clone());

    let tokens = cancel_tokens(state.reduce(Action::CancelProviderTurn { task_id: task_b }));

    assert_eq!(
        tokens.len(),
        1,
        "targeted cancel should emit only the requested task token"
    );
    for token in tokens {
        token.cancel();
    }
    assert!(!token_a.is_cancelled(), "peer task token must remain live");
    assert!(token_b.is_cancelled(), "requested task token must be cancelled");
    assert!(
        state.control.turn_cancels.contains_key(&task_a),
        "peer task cancel token remains retained"
    );
    assert!(
        !state.control.turn_cancels.contains_key(&task_b),
        "requested task cancel token is consumed"
    );
    assert!(
        state
            .stream
            .visible_drafts
            .iter()
            .any(|draft| draft.task_id == Some(task_a)),
        "peer draft remains visible"
    );
    assert!(
        state
            .stream
            .visible_drafts
            .iter()
            .all(|draft| draft.task_id != Some(task_b)),
        "requested draft is removed"
    );
    assert!(state.control.generating, "peer task keeps generation active");
}

#[test]
fn p3b_single_legacy_turn_ctrl_c_still_uses_primary_active_cancel() {
    let mut state = s();
    let token = CancellationToken::new();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "primary-draft".to_string(),
        cancel: token.clone(),
    });

    let effects = state.reduce(Action::CancelRequested);
    let tokens = cancel_tokens(effects);

    assert_eq!(tokens.len(), 1);
    for token in tokens {
        token.cancel();
    }
    assert!(token.is_cancelled());
    assert!(state.control.active_cancel.is_none());
    assert!(state.control.turn_cancels.is_empty());
    assert!(!state.control.generating);
    assert!(state.stream.primary_streaming_draft().is_none());
}
