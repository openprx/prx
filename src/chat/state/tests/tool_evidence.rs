#![cfg(test)]

use super::super::*;
use crate::agent::tool_evidence::{ToolCallOutcome, TurnToolEvidence};
use crate::chat::action::{Action, CompactReason};
use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
use crate::tools::ApprovalDenialCause;
use tokio_util::sync::CancellationToken;

fn s() -> ChatState {
    ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
}

/// One recorded call of a driver turn: tool name and outcome.
type Call = (&'static str, ToolCallOutcome);

fn start_driver_turn(state: &mut ChatState, task_id: TurnTaskId, sequence: u64) {
    let _ = state.reduce(Action::StartLLMTurn {
        provider_turn_task_id: Some(task_id),
        provider_turn_sequence: Some(sequence),
        draft_id: format!("draft-{sequence}"),
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

fn record_outcome(state: &mut ChatState, task_id: Option<TurnTaskId>, call: Call) -> Vec<Effect> {
    let (name, outcome) = call;
    state.reduce(Action::ToolOutcomeRecorded {
        task_id,
        tool_call_id: format!("call-{name}"),
        name: name.to_owned(),
        outcome,
    })
}

/// A full driver exchange: the user turn, the driver turn with `calls`, and
/// the committed reply.
fn driver_exchange(state: &mut ChatState, scheduler: &mut TurnScheduler, question: &str, reply: &str, calls: &[Call]) {
    let task_id = scheduler.enqueue(question, TurnPriority::Normal, state.session.history.len());
    let sequence = scheduler.task(task_id).expect("test: scheduled task").sequence;
    let _ = state.reduce(Action::RecordUserTurn(question.to_owned()));
    start_driver_turn(state, task_id, sequence);
    for call in calls {
        let _ = record_outcome(state, Some(task_id), *call);
    }
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: Some(task_id),
        content: reply.to_owned(),
    });
}

fn evidence_of(calls: &[Call]) -> TurnToolEvidence {
    let mut evidence = TurnToolEvidence::default();
    for (name, outcome) in calls {
        evidence.record(name, *outcome);
    }
    evidence
}

const SHELL_OK: Call = ("shell", ToolCallOutcome::Ok);
const FILE_READ_OK: Call = ("file_read", ToolCallOutcome::Ok);
const EDIT_DENIED: Call = (
    "file_edit",
    ToolCallOutcome::Denied {
        cause: Some(ApprovalDenialCause::UserRejected),
    },
);

/// A session of three driver exchanges under a leading system prompt:
/// history `[sys, q1, a1, q2, a2, q3, a3]`.
fn three_exchange_state() -> ChatState {
    let mut state = s();
    let mut scheduler = TurnScheduler::new();
    let _ = state.reduce(Action::SetLeadingSystemPrompt {
        content: "sys".to_owned(),
    });
    driver_exchange(&mut state, &mut scheduler, "q1", "a1", &[SHELL_OK]);
    driver_exchange(&mut state, &mut scheduler, "q2", "a2", &[FILE_READ_OK, FILE_READ_OK]);
    driver_exchange(&mut state, &mut scheduler, "q3", "a3", &[EDIT_DENIED]);
    state
}

/// A real compaction patch over the reducer history: `[1..3]` (q1, a1) is
/// replaced by a summary, followed by the summary's refresh aside.
fn fold_first_exchange(history: &[ChatMessage]) -> crate::agent::turn_contracts::CompactionPatch {
    let guard = crate::agent::compaction::compaction_patch_guard_for(history, 1, 3)
        .expect("test: the range lies inside the history");
    crate::agent::turn_contracts::CompactionPatch {
        replacement: vec![ChatMessage::assistant("summary of the first exchange")],
        append_after: vec![ChatMessage::user(format!(
            "{} aside",
            crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER
        ))],
        guard,
    }
}

fn apply_patch(state: &mut ChatState, patch: crate::agent::turn_contracts::CompactionPatch) -> Vec<Effect> {
    state.reduce(Action::HistoryCompactionPatchApplied {
        reason: CompactReason::Manual,
        patch,
        compaction_config: crate::config::AgentCompactionConfig::default(),
    })
}

/// `(content, evidence)` of every assistant turn.
fn assistant_evidence(state: &ChatState) -> Vec<(String, Option<TurnToolEvidence>)> {
    state
        .session
        .turns
        .iter()
        .filter(|turn| turn.role == "assistant")
        .map(|turn| (turn.content.clone(), turn.tool_evidence.clone()))
        .collect()
}

#[test]
fn driver_turn_records_outcomes_into_the_assistant_turn() {
    let mut state = s();
    let mut scheduler = TurnScheduler::new();

    driver_exchange(&mut state, &mut scheduler, "run it", "done", &[SHELL_OK, EDIT_DENIED]);

    let last = state.session.turns.last().expect("test: assistant turn");
    assert_eq!(last.role, "assistant", "the reply is the newest turn");
    assert_eq!(last.content, "done", "the saved text is the reply alone");
    assert_eq!(
        last.tool_evidence,
        Some(evidence_of(&[SHELL_OK, EDIT_DENIED])),
        "the reply carries every outcome the driver turn recorded, in order"
    );
    assert_eq!(
        last.tool_evidence.as_ref().map(TurnToolEvidence::suffix).as_deref(),
        Some("\n\n[tool evidence: shell ok; file_edit denied(user_rejected)]"),
        "the recorded evidence renders the denial with its cause"
    );
    assert!(
        state.control.tool_buffers.is_empty(),
        "taking the record leaves no buffer behind"
    );
    assert!(
        state
            .session
            .history
            .iter()
            .all(|message| !message.content.contains("[tool evidence:")),
        "the reducer history never holds evidence"
    );
}

#[test]
fn turn_without_start_llm_turn_records_no_evidence() {
    let mut state = s();
    let _ = state.reduce(Action::RecordUserTurn("question".to_owned()));
    let _ = state.reduce(Action::TurnStarted {
        draft_id: "legacy".to_owned(),
        cancel: CancellationToken::new(),
    });

    let effects = record_outcome(&mut state, None, SHELL_OK);
    let _ = state.reduce(Action::RecordAssistantTurn {
        task_id: None,
        content: "answer".to_owned(),
    });

    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::LogTrace { level, msg } if *level == tracing::Level::DEBUG && msg.contains("untracked")
        )),
        "an outcome without a started record is traced, not recorded: {effects:?}"
    );
    assert_eq!(
        state.session.turns.last().and_then(|turn| turn.tool_evidence.clone()),
        None,
        "an untracked turn gets no evidence, not even none"
    );
}

#[test]
fn driver_turn_without_tools_records_none_evidence() {
    let mut state = s();
    let mut scheduler = TurnScheduler::new();

    driver_exchange(&mut state, &mut scheduler, "say ack", "ACK-7", &[]);

    let evidence = state.session.turns.last().and_then(|turn| turn.tool_evidence.clone());
    assert_eq!(
        evidence,
        Some(TurnToolEvidence::default()),
        "a driver turn that ran no tool is recorded as such"
    );
    assert_eq!(
        evidence.as_ref().map(TurnToolEvidence::suffix).as_deref(),
        Some("\n\n[tool evidence: none]"),
        "and is replayed as none"
    );
}

#[test]
fn compaction_patch_keeps_evidence_of_retained_turns_and_drops_folded() {
    let mut state = three_exchange_state();
    let patch = fold_first_exchange(&state.session.history);

    let effects = apply_patch(&mut state, patch);

    assert!(
        effects.iter().any(|effect| matches!(effect, Effect::SaveSession(_))),
        "the patch applies and is saved: {effects:?}"
    );
    assert_eq!(
        assistant_evidence(&state),
        vec![
            ("summary of the first exchange".to_owned(), None),
            ("a2".to_owned(), Some(evidence_of(&[FILE_READ_OK, FILE_READ_OK]))),
            ("a3".to_owned(), Some(evidence_of(&[EDIT_DENIED]))),
        ],
        "kept replies keep their evidence; the summary that folded a1 has none"
    );
    assert!(
        !effects.iter().any(|effect| matches!(
            effect,
            Effect::LogTrace { level, .. } if *level == tracing::Level::WARN
        )),
        "an aligned history carries without a warning: {effects:?}"
    );
}

#[test]
fn misaligned_history_stops_carrying_evidence() {
    let mut state = three_exchange_state();
    // The history disagrees with the stored turns from q3 (index 5) on.
    let q3 = state.session.history.get_mut(5).expect("test: q3 in the history");
    assert_eq!(q3.content, "q3", "test: index 5 is q3");
    q3.content = "q3 edited elsewhere".to_owned();
    let patch = fold_first_exchange(&state.session.history);

    let effects = apply_patch(&mut state, patch);

    assert_eq!(
        assistant_evidence(&state),
        vec![
            ("summary of the first exchange".to_owned(), None),
            ("a2".to_owned(), Some(evidence_of(&[FILE_READ_OK, FILE_READ_OK]))),
            ("a3".to_owned(), None),
        ],
        "evidence before the mismatch is carried; from the mismatch on none is"
    );
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::LogTrace { level, msg }
                if *level == tracing::Level::WARN && msg.contains("tool evidence not carried")
        )),
        "the lost evidence is reported: {effects:?}"
    );
}

#[test]
fn whole_history_replacement_keeps_evidence_of_the_repeated_tail() {
    let mut state = three_exchange_state();
    // A worker compaction: the whole history is replaced by the system
    // prompt, a handoff and the retained turns repeated verbatim.
    let history = state.session.history.clone();
    let guard = crate::agent::compaction::compaction_patch_guard_for(&history, 0, history.len())
        .expect("test: the whole history is a valid range");
    let mut replacement = vec![
        ChatMessage::system("sys"),
        ChatMessage::assistant("handoff of the first two exchanges"),
    ];
    replacement.extend(history.iter().skip(history.len() - 2).cloned());
    let patch = crate::agent::turn_contracts::CompactionPatch {
        replacement,
        append_after: Vec::new(),
        guard,
    };

    let _ = apply_patch(&mut state, patch);

    assert_eq!(
        assistant_evidence(&state),
        vec![
            ("handoff of the first two exchanges".to_owned(), None),
            ("a3".to_owned(), Some(evidence_of(&[EDIT_DENIED]))),
        ],
        "the repeated retained reply keeps its evidence; the handoff gets none"
    );
}
