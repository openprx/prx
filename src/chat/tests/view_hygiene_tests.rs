//! View enrichment hygiene (M3) over a real dispatcher, a real
//! persistence-only executor and real SQLite: a turn's request is sent with
//! its enrichment (recalled context, attached files), and its commit leaves
//! the run loop's provider view holding the plain stored request, equal to
//! the view rebuilt from the acknowledged session.
#![cfg(test)]

use super::*;
use crate::agent::tool_evidence::ToolCallOutcome;
use crate::chat::acked_session::{Ack, TestSessionDispatcher};
use crate::chat::action::{Action, CompactReason};
use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
use crate::tools::ApprovalDenialCause;

/// Upper bound for one test.
const DEADLINE: Duration = Duration::from_secs(20);
const SYSTEM: &str = "view hygiene system prompt";
/// The recalled-context block a request is sent with.
const RECALL_BLOCK: &str = "[Current session context]\n- [user] an earlier line\n\n";
/// The attached-file block a request is sent with.
const FILE_BLOCK: &str = "\n\n[Attached file context from @path mentions]\n### @notes.md\nPath: notes.md\n\nnotes\n[End attached file context]";

/// One recorded tool call of a turn: tool name and outcome.
type Call = (&'static str, ToolCallOutcome);

const SHELL_OK: Call = ("shell", ToolCallOutcome::Ok);
const EDIT_DENIED: Call = (
    "file_edit",
    ToolCallOutcome::Denied {
        cause: Some(ApprovalDenialCause::UserRejected),
    },
);

/// One scripted turn: the plain request, the enriched form it is sent in,
/// the tool calls it records and its reply.
struct Turn {
    plain: &'static str,
    enriched: String,
    calls: &'static [Call],
    reply: &'static str,
}

impl Turn {
    fn recalled(plain: &'static str, calls: &'static [Call], reply: &'static str) -> Self {
        Self {
            plain,
            enriched: format!("{RECALL_BLOCK}{plain}"),
            calls,
            reply,
        }
    }

    fn with_file(plain: &'static str, reply: &'static str) -> Self {
        Self {
            plain,
            enriched: format!("{RECALL_BLOCK}{plain}{FILE_BLOCK}"),
            calls: &[],
            reply,
        }
    }
}

fn sqlite_memory() -> (tempfile::TempDir, Arc<dyn Memory>) {
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("test: sqlite memory"));
    (dir, memory)
}

fn harness_with_system_prompt(memory: Arc<dyn Memory>) -> TestSessionDispatcher {
    let mut state = TestSessionDispatcher::state_with_history(Vec::new());
    let _ = state.reduce(Action::SetLeadingSystemPrompt {
        content: SYSTEM.to_owned(),
    });
    TestSessionDispatcher::spawn(memory, state)
}

fn per_turn_context(
    task_id: TurnTaskId,
    draft_id: &str,
    user_input: &str,
    compaction_epoch_at_start: u64,
) -> PerTurnContext {
    PerTurnContext {
        turn_binding: None,
        task_id,
        draft_id: draft_id.to_owned(),
        delta_tx: None,
        tool_event_tx: None,
        draft_updater: None,
        tool_event_forwarder: None,
        user_input: user_input.to_owned(),
        turn_run_id: format!("view-hygiene-run-{}", task_id.get()),
        route_scope: crate::memory::MessageEventScope::new("chat", crate::memory::MemoryVisibility::Workspace),
        route_decision: RouteDecision::single_candidate("mock", "mock"),
        provider_started_at: chrono::Utc::now(),
        provider_name: "mock".to_owned(),
        model_name: "mock".to_owned(),
        history_len_before_user_turn: 0,
        compaction_epoch_at_start,
        assignment: None,
    }
}

/// Start `turn` as the run loop does: its provider view is the current view
/// plus the enriched request, its tool outcomes are recorded under its task.
/// Returns the turn's task, draft and the request it sent.
fn start_turn(
    harness: &TestSessionDispatcher,
    scheduler: &mut TurnScheduler,
    view: &[ChatMessage],
    turn: &Turn,
) -> (TurnTaskId, String, ChatMessage) {
    let task_id = scheduler.enqueue(turn.plain, TurnPriority::Normal, view.len());
    let sequence = scheduler.task(task_id).expect("test: scheduled task").sequence;
    let draft_id = format!("draft-{sequence}");
    let request = crate::agent::turn_contracts::inbound_user_message(&turn.enriched);
    let mut sent = view.to_vec();
    sent.push(request.clone());
    let started = harness.dispatcher.dispatch_or_log(
        Action::StartLLMTurn {
            provider_turn_task_id: Some(task_id),
            provider_turn_sequence: Some(sequence),
            draft_id: draft_id.clone(),
            history: sent,
            compaction_guard_history: None,
            compaction_config: None,
            cancel: CancellationToken::new(),
            turn_spawn_ctx: None,
            turn_message_send_ctx: None,
            routing_input: Some(turn.plain.to_owned()),
            turn_binding: None,
        },
        "test.start_llm_turn",
    );
    assert!(
        matches!(started, crate::chat::dispatcher::DispatchResult::Sent),
        "the turn starts: {started:?}"
    );
    for (name, outcome) in turn.calls {
        let recorded = harness.dispatcher.dispatch_or_log(
            Action::ToolOutcomeRecorded {
                task_id: Some(task_id),
                tool_call_id: format!("call-{name}-{sequence}"),
                name: (*name).to_owned(),
                outcome: *outcome,
            },
            "test.tool_outcome_recorded",
        );
        assert!(
            matches!(recorded, crate::chat::dispatcher::DispatchResult::Sent),
            "the tool outcome is recorded: {recorded:?}"
        );
    }
    (task_id, draft_id, request)
}

/// Commit the started turn through the TUI's ordered commit and adopt it into
/// `view` exactly as the run loop's finalizer does.
async fn commit_turn(
    harness: &TestSessionDispatcher,
    view: &mut Vec<ChatMessage>,
    turn: &Turn,
    task_id: TurnTaskId,
    draft_id: &str,
    compaction_epoch_at_start: u64,
) {
    let committed = dispatch_ordered_provider_turn_commit_and_wait(
        &harness.dispatcher,
        &harness.commits,
        task_id,
        OrderedCommitTerminal::StreamCompleted {
            draft_id,
            reasoning: "",
        },
        turn.plain,
        turn.reply,
        false,
        true,
    )
    .await;
    assert_eq!(committed, Ok(()), "the turn `{}` is committed", turn.plain);
    let pending = per_turn_context(task_id, draft_id, turn.plain, compaction_epoch_at_start);
    adopt_committed_turn(view, &harness.dispatcher, &pending, Some(turn.reply));
}

fn role_and_content(messages: &[ChatMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .map(|message| (message.role.clone(), message.content.clone()))
        .collect()
}

/// The view rebuilt from the session's acknowledged snapshot (R2).
fn rebuilt_view(harness: &TestSessionDispatcher, view: &[ChatMessage]) -> Vec<ChatMessage> {
    provider_view_from_acked(view, &harness.dispatcher.session_reader().acked())
}

fn user_contents(view: &[ChatMessage]) -> Vec<String> {
    view.iter()
        .filter(|message| message.role == "user")
        .map(|message| message.content.clone())
        .collect()
}

#[tokio::test]
async fn committed_turns_keep_the_plain_request_in_the_view() {
    tokio::time::timeout(DEADLINE, async {
        let (_dir, memory) = sqlite_memory();
        let harness = harness_with_system_prompt(memory);
        let mut scheduler = TurnScheduler::new();
        let mut view = vec![ChatMessage::system(SYSTEM)];

        // An appended commit: no compaction since the turn copied its view.
        let first = Turn::with_file("summarize @notes.md", "summarized");
        let (task_id, draft_id, first_sent) = start_turn(&harness, &mut scheduler, &view, &first);
        commit_turn(&harness, &mut view, &first, task_id, &draft_id, 0).await;
        let after_append = view.clone();

        // A rebuilt commit: a compaction moves the acknowledged epoch while
        // the turn runs (the first exchange is folded into a summary).
        let second = Turn::recalled("and then?", &[], "then nothing");
        let (task_id, draft_id, second_sent) = start_turn(&harness, &mut scheduler, &view, &second);
        let acked = harness.dispatcher.session_reader().acked();
        let mut reducer_history = vec![ChatMessage::system(SYSTEM)];
        reducer_history.extend(session_turns_to_history(&acked.session));
        let guard = crate::agent::compaction::compaction_patch_guard_for(&reducer_history, 1, 3)
            .expect("test: the first exchange lies inside the reducer history");
        let compacted = harness
            .dispatcher
            .dispatch_awaited(
                Action::HistoryCompactionPatchApplied {
                    reason: CompactReason::Manual,
                    patch: crate::agent::turn_contracts::CompactionPatch {
                        replacement: vec![ChatMessage::assistant("summary of the first exchange")],
                        append_after: Vec::new(),
                        guard,
                    },
                    compaction_config: crate::config::AgentCompactionConfig::default(),
                },
                &harness.commits,
            )
            .await
            .expect("test: the compaction is acknowledged");
        assert!(
            matches!(compacted, Ack::Published(_)),
            "the compaction publishes a revision: {compacted:?}"
        );
        commit_turn(&harness, &mut view, &second, task_id, &draft_id, acked.compaction_epoch).await;
        let after_rebuild = view.clone();
        let rebuilt = rebuilt_view(&harness, &view);
        harness.stop().await;

        assert_eq!(
            first_sent.content,
            format!("{RECALL_BLOCK}summarize @notes.md{FILE_BLOCK}"),
            "the request is sent with its recalled context and attached file"
        );
        assert_eq!(
            user_contents(&after_append),
            vec!["summarize @notes.md".to_owned()],
            "an appended commit keeps the plain request"
        );
        assert_eq!(
            second_sent.content,
            format!("{RECALL_BLOCK}and then?"),
            "the next request still carries its own enrichment"
        );
        assert_eq!(
            user_contents(&after_rebuild),
            vec!["and then?".to_owned()],
            "a commit after a compaction keeps the plain request"
        );
        assert_eq!(
            role_and_content(&after_rebuild),
            role_and_content(&rebuilt),
            "the rebuilt commit is the acknowledged session's view"
        );
    })
    .await
    .expect("committed_turns_keep_the_plain_request_in_the_view exceeded DEADLINE");
}

#[tokio::test]
async fn in_process_view_equals_the_rebuilt_view_after_each_commit() {
    tokio::time::timeout(DEADLINE, async {
        let (_dir, memory) = sqlite_memory();
        let harness = harness_with_system_prompt(memory);
        let mut scheduler = TurnScheduler::new();
        let mut view = vec![ChatMessage::system(SYSTEM)];
        let turns = [
            Turn::recalled("hello", &[], "hi"),
            Turn::recalled("list the files", &[SHELL_OK], "three files"),
            Turn::recalled("edit the readme", &[EDIT_DENIED], "the edit was refused"),
            Turn::with_file("check @notes.md", "notes checked"),
            Turn::recalled("thanks", &[], "you are welcome"),
        ];
        let mut after_each_commit = Vec::new();
        for turn in &turns {
            let (task_id, draft_id, _sent) = start_turn(&harness, &mut scheduler, &view, turn);
            commit_turn(&harness, &mut view, turn, task_id, &draft_id, 0).await;
            after_each_commit.push((
                role_and_content(&view),
                role_and_content(&rebuilt_view(&harness, &view)),
            ));
        }
        harness.stop().await;

        for (index, (in_process, rebuilt)) in after_each_commit.iter().enumerate() {
            assert_eq!(
                in_process, rebuilt,
                "after commit {index} the in-process view equals the view rebuilt from the session"
            );
        }
        let Some((final_view, _)) = after_each_commit.last() else {
            panic!("test: five commits were adopted");
        };
        assert_eq!(
            final_view.iter().filter(|(role, _)| role == "user").count(),
            turns.len(),
            "every committed request is in the view once"
        );
        assert!(
            final_view
                .iter()
                .all(|(_, content)| !content.contains("[Current session context]")
                    && !content.contains("[Attached file context from @path mentions]")),
            "no committed message keeps a request's enrichment: {final_view:?}"
        );
        assert!(
            final_view.iter().any(|(role, content)| role == "assistant"
                && content.starts_with("three files")
                && content.contains("shell ok")),
            "the tool turn's reply is replayed with its evidence: {final_view:?}"
        );
        assert!(
            final_view.iter().any(|(role, content)| role == "assistant"
                && content.starts_with("the edit was refused")
                && content.contains("file_edit denied")),
            "the denied tool's reply is replayed with its evidence: {final_view:?}"
        );
    })
    .await
    .expect("in_process_view_equals_the_rebuilt_view_after_each_commit exceeded DEADLINE");
}
