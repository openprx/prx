//! What chat keeps of a finished turn beyond its text, so a later turn's
//! provider view replays it as the turn had it: the form the user's request was
//! sent in and the tool rounds of the run that made the reply.
//!
//! Both ride on the stored turn's runtime record
//! ([`crate::agent::tool_evidence::TurnToolEvidence`]) and are rendered by
//! [`crate::agent::turn_history::project`], so the provider view, the
//! compaction guard and the reducer form keep describing the same canonical
//! messages. The tool rounds come from the durable log
//! ([`crate::context::transcript`]); they are attached after each commit and when
//! a saved session is loaded, and never saved with the session.

use super::chat_events::chat_runtime_envelope;
use super::dispatcher::{ChatDispatcher, TurnCompletionSignal};
use super::session::ChatSession;
use crate::context::transcript::{Attachment, Transcript, TurnReplay};
use crate::memory::Memory;
use crate::providers::ChatMessage;
use std::sync::Arc;

/// The request as it is sent, and what was put around the typed `user_input`.
/// `prompt` is `user_input` possibly followed by attached file text; when it
/// does not start with `user_input`, no sent form is kept and the request is
/// `prefix + prompt + time_tail`, as it always was.
pub(super) fn sent_request(
    prefix: &str,
    user_input: &str,
    prompt: &str,
    time_tail: String,
) -> (String, Option<Attachment>) {
    let prefix = crate::security::redaction::redact_secrets(prefix);
    let Some(file_text) = prompt.strip_prefix(user_input) else {
        return (format!("{prefix}{prompt}{time_tail}"), None);
    };
    let sent = Attachment {
        prefix,
        tail: format!("{file_text}{time_tail}"),
    };
    (sent.compose(user_input), Some(sent))
}

/// After a turn's commit: hand the reducer the sent form of the request and the
/// tool rounds of run `run_id`, to attach to the turns it just recorded. The
/// rounds are read back from the durable log. Nothing is sent when there is
/// nothing to attach.
#[allow(clippy::too_many_arguments)]
pub(super) async fn attach_turn_records(
    chat_dispatcher: &ChatDispatcher,
    commits: &TurnCompletionSignal,
    memory: &dyn Memory,
    workspace_id: &str,
    chat_session_key: &str,
    run_id: &str,
    user_text: &str,
    sent: Option<&Attachment>,
    reply: Option<&str>,
) {
    let replay = if reply.is_some() {
        let principal = chat_runtime_envelope(workspace_id, chat_session_key).memory_principal();
        Transcript::new(memory, principal)
            .tool_log()
            .await
            .replay_of_run(run_id)
            .map(Arc::new)
    } else {
        None
    };
    if sent.is_none() && replay.is_none() {
        return;
    }
    // A failed acknowledgement latches the unconfirmed-commit stop itself.
    let _ = chat_dispatcher
        .dispatch_awaited(
            super::action::Action::TurnRecordsAttached {
                user_text: user_text.to_owned(),
                sent: sent.cloned(),
                reply: reply.map(str::to_owned),
                replay,
            },
            commits,
        )
        .await;
}

/// A loaded session: attach to each assistant reply the tool rounds of the run
/// that made it, read from the durable log. A reply is tied to its run by its
/// text, from the newest back; a reply that does not match, and every older
/// one, keeps its tool evidence line.
pub(super) async fn attach_stored_replays(mem: &dyn Memory, workspace_id: &str, session: &mut ChatSession) {
    if !session.turns.iter().any(|turn| turn.role == "assistant") {
        return;
    }
    let principal = chat_runtime_envelope(workspace_id, &session.id).memory_principal();
    let log = Transcript::new(mem, principal).replay_log().await;
    let runs: Vec<Option<String>> = {
        let replies: Vec<&str> = session
            .turns
            .iter()
            .filter(|turn| turn.role == "assistant")
            .map(|turn| turn.content.as_str())
            .collect();
        log.runs_of_replies(&replies)
            .into_iter()
            .map(|run| run.map(str::to_owned))
            .collect()
    };
    let replies = session.turns.iter_mut().filter(|turn| turn.role == "assistant");
    for (turn, run) in replies.zip(runs) {
        if let Some(replay) = run.and_then(|run| log.replay_of_run(&run)) {
            turn.tool_evidence.get_or_insert_with(Default::default).replay = Some(Arc::new(replay));
        }
    }
}

/// Before a turn: replace the oldest replayed tool results held in `view` by a
/// stub while `view` and `request` are over the warning threshold. The replayed
/// rounds are those of `session`'s turns. Returns how many were stubbed.
pub(super) fn stub_replayed_results(
    view: &mut [ChatMessage],
    session: &ChatSession,
    request: &ChatMessage,
    config: &crate::config::AgentCompactionConfig,
) -> usize {
    let replays: Vec<&TurnReplay> = session
        .turns
        .iter()
        .filter_map(|turn| turn.tool_evidence.as_ref()?.replay.as_deref())
        .collect();
    if replays.is_empty() {
        return 0;
    }
    crate::context::transcript::stub_replayed_results(view, &replays, request, config)
}
