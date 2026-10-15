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

fn start_task(state: &mut ChatState, task_id: TurnTaskId, sequence: u64, draft_id: &str) {
    let _ = state.reduce(Action::StartLLMTurn {
        provider_turn_task_id: Some(task_id),
        provider_turn_sequence: Some(sequence),
        draft_id: draft_id.to_string(),
        history: Vec::new(),
        compaction_guard_history: None,
        compaction_config: None,
        cancel: CancellationToken::new(),
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding: None,
    });
}

fn start_tool(state: &mut ChatState, task_id: TurnTaskId, sequence: u64, name: &str, args: &str) {
    let _ = state.reduce(Action::ToolStarted {
        task_id: Some(task_id),
        sequence: Some(sequence),
        tool_call_id: None,
        name: name.to_string(),
        args: args.to_string(),
    });
}

fn finish_tool(state: &mut ChatState, task_id: TurnTaskId, sequence: u64, name: &str, success: bool) {
    let _ = state.reduce(Action::ToolFinished {
        task_id: Some(task_id),
        sequence: Some(sequence),
        tool_call_id: None,
        name: name.to_string(),
        success,
        duration_ms: 7,
        result: Some(format!("{name}-result")),
    });
}

// Reads the TUI conversation lines, which exist only with the
// `terminal-tui` feature (BENCH-FEAT b: `--no-default-features
// --all-targets` must build).
#[cfg(feature = "terminal-tui")]
#[test]
fn p3a_cancelled_task_finalizes_only_its_tool_buffer() {
    let mut state = s();
    let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
    start_task(&mut state, task_a, seq_a, "draft-a");
    start_task(&mut state, task_b, seq_b, "draft-b");
    start_tool(&mut state, task_a, seq_a, "shell", r#"{"cmd":"sleep 1"}"#);
    start_tool(&mut state, task_b, seq_b, "grep", r#"{"q":"needle"}"#);

    let _ = state.reduce(Action::StreamCancelled {
        draft_id: "draft-a".to_string(),
    });

    assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_a)), 0);
    assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 1);
    assert!(
        state.ui.conversation_lines.iter().any(|line| matches!(
            line,
            crate::chat::tui::ConversationLine::ToolResult {
                tool_name,
                status: crate::chat::tui::ToolStatus::Running,
                ..
            } if tool_name == "grep"
        )),
        "task B running tool card must survive task A cancellation"
    );
}

#[test]
fn p3a_completed_task_clears_only_matching_buffer() {
    let mut state = s();
    let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
    start_task(&mut state, task_a, seq_a, "draft-a");
    start_task(&mut state, task_b, seq_b, "draft-b");
    start_tool(&mut state, task_a, seq_a, "search", r#"{"q":"a"}"#);
    finish_tool(&mut state, task_a, seq_a, "search", true);
    start_tool(&mut state, task_b, seq_b, "fetch", r#"{"url":"b"}"#);

    let _ = state.reduce(Action::StreamCompleted {
        draft_id: "draft-a".to_string(),
        final_text: "a done".to_string(),
        reasoning: String::new(),
    });

    assert_eq!(state.control.tool_call_count(ToolTaskKey::Task(task_a)), 0);
    assert_eq!(state.control.tool_arg_count(ToolTaskKey::Task(task_b)), 1);
    assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 1);
}

#[test]
fn p3a_record_assistant_turn_drains_only_requested_task_calls() {
    let mut state = s();
    let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
    start_task(&mut state, task_a, seq_a, "draft-a");
    start_task(&mut state, task_b, seq_b, "draft-b");
    start_tool(&mut state, task_a, seq_a, "search", r#"{"q":"a"}"#);
    finish_tool(&mut state, task_a, seq_a, "search", true);
    start_tool(&mut state, task_b, seq_b, "fetch", r#"{"url":"b"}"#);
    finish_tool(&mut state, task_b, seq_b, "fetch", false);

    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: Some(task_b),
        content: "b answer".to_string(),
    });

    let last = state.session.turns.last().expect("test: assistant turn");
    assert_eq!(last.tool_calls.len(), 1);
    let call = last.tool_calls.first().expect("test: tool call");
    assert_eq!(call.name, "fetch");
    assert!(!call.success);
    assert_eq!(call.task_id, Some(task_b.get()));
    assert_eq!(call.sequence, Some(seq_b));
    assert_eq!(state.control.tool_call_count(ToolTaskKey::Task(task_a)), 1);
    assert_eq!(state.control.tool_call_count(ToolTaskKey::Task(task_b)), 0);
}

#[test]
fn p3a_same_tool_name_args_are_isolated_by_task() {
    let mut state = s();
    let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
    start_task(&mut state, task_a, seq_a, "draft-a");
    start_task(&mut state, task_b, seq_b, "draft-b");
    start_tool(&mut state, task_a, seq_a, "shell", r#"{"cmd":"echo a"}"#);
    start_tool(&mut state, task_b, seq_b, "shell", r#"{"cmd":"echo b"}"#);

    finish_tool(&mut state, task_b, seq_b, "shell", true);

    assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_a)), 1);
    assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 0);
    let b_call = state
        .control
        .tool_buffers
        .get(&ToolTaskKey::Task(task_b))
        .and_then(|buffer| buffer.tool_calls.first())
        .expect("test: task b call");
    assert!(b_call.args_preview.contains("echo b"));
    assert_eq!(state.control.tool_arg_count(ToolTaskKey::Task(task_a)), 1);

    finish_tool(&mut state, task_a, seq_a, "shell", true);
    let a_call = state
        .control
        .tool_buffers
        .get(&ToolTaskKey::Task(task_a))
        .and_then(|buffer| buffer.tool_calls.first())
        .expect("test: task a call");
    assert!(a_call.args_preview.contains("echo a"));
}

#[test]
fn p3a_legacy_primary_tool_buffer_path_still_records_tool_calls() {
    let mut state = s();
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "primary-draft".to_string(),
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
        duration_ms: 3,
        result: Some("ok".to_string()),
    });
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "primary answer".to_string(),
    });

    let last = state.session.turns.last().expect("test: primary assistant turn");
    assert_eq!(last.tool_calls.len(), 1);
    let call = last.tool_calls.first().expect("test: primary tool call");
    assert_eq!(call.name, "shell");
    assert!(call.success);
    assert_eq!(call.task_id, None);
    assert_eq!(call.sequence, None);
    assert!(!state.control.tool_buffers.contains_key(&ToolTaskKey::Primary));
}
