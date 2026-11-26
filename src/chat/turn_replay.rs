//! What chat keeps of a finished turn beyond its text, so a later turn's
//! provider view replays it: the attached text and date line the user's request
//! was sent with (the recalled prefix is sent with the live request only) and
//! the tool rounds of the run that made the reply.
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
use crate::context::reduce::ladder;
use crate::context::reduce::stub_replayed::{ReplayedView, StubReplayed};
use crate::context::transcript::{Attachment, Transcript, TurnReplay};
use crate::memory::Memory;
use crate::providers::ChatMessage;
use std::sync::Arc;

/// The live request as it is sent, and what was put around the typed
/// `user_input`. `prompt` is `user_input` possibly followed by attached file
/// text; when it does not start with `user_input`, no sent form is kept and the
/// request is `prefix + prompt + time_tail`, as it always was. A later turn
/// replays the returned attachment as its text and tail only.
pub(super) fn sent_request(
    prefix: &str,
    user_input: &str,
    prompt: &str,
    time_tail: String,
) -> (String, Option<Attachment>) {
    // The attached file text is masked like the prefix, before it is sent,
    // stored and written to the attachment row: the three stay identical.
    let prefix = crate::security::redaction::redact_secrets(prefix);
    let Some(file_text) = prompt.strip_prefix(user_input) else {
        let tail = crate::security::redaction::redact_secrets(&format!("{prompt}{time_tail}"));
        return (format!("{prefix}{tail}"), None);
    };
    let sent = Attachment {
        prefix,
        tail: crate::security::redaction::redact_secrets(&format!("{file_text}{time_tail}")),
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

/// A loaded session, projected from its MessageEvent rows: the sent form of
/// each user turn and the tool rounds of each reply come from the durable log.
/// `turn_event_ids` pairs each turn with its stored event (index for index with
/// `session.turns`; empty or `None` when the turns were not projected from
/// events). A paired user turn takes its stored attachment; a paired reply takes
/// the rounds of its event's run. Any other reply is tied to its run by its
/// text, from the newest back; a reply that does not match, and every older
/// one, keeps its tool evidence line.
pub(super) async fn attach_stored_replays(
    mem: &dyn Memory,
    workspace_id: &str,
    session: &mut ChatSession,
    turn_event_ids: &[Option<String>],
) {
    let has_user_event = turn_event_ids.iter().flatten().next().is_some();
    if !has_user_event && !session.turns.iter().any(|turn| turn.role == "assistant") {
        return;
    }
    let principal = chat_runtime_envelope(workspace_id, &session.id).memory_principal();
    let log = Transcript::new(mem, principal).replay_log().await;
    // The text match is the fallback for a reply without a stored event; it is
    // computed over every reply, indexed by reply order.
    let text_runs: Vec<Option<String>> = {
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
    let (mut sent_from_log, mut replays_by_run, mut replays_by_text) = (0_usize, 0_usize, 0_usize);
    let mut reply_ordinal = 0_usize;
    for (index, turn) in session.turns.iter_mut().enumerate() {
        let event_id = turn_event_ids.get(index).and_then(Option::as_deref);
        match turn.role.as_str() {
            "user" => {
                let Some(attachment) = event_id.and_then(|event_id| log.attachment_of(event_id)) else {
                    continue;
                };
                turn.tool_evidence.get_or_insert_with(Default::default).sent = Some(attachment.clone());
                sent_from_log += 1;
            }
            "assistant" => {
                let ordinal = reply_ordinal;
                reply_ordinal += 1;
                let by_event = event_id.and_then(|event_id| log.run_of_event(event_id));
                let (run, by_run) = match (by_event, text_runs.get(ordinal).and_then(Option::as_deref)) {
                    (Some(run), _) => (run, true),
                    (None, Some(run)) => (run, false),
                    (None, None) => continue,
                };
                if let Some(replay) = log.replay_of_run(run) {
                    turn.tool_evidence.get_or_insert_with(Default::default).replay = Some(Arc::new(replay));
                    if by_run {
                        replays_by_run += 1;
                    } else {
                        replays_by_text += 1;
                    }
                }
            }
            _ => {}
        }
    }
    tracing::debug!(
        sent_from_log,
        replays_by_run,
        replays_by_text,
        "attached stored replays"
    );
}

/// Before a turn: replace the oldest replayed tool results held in `view` by a
/// stub while `view` and `request` are over the warning threshold. The replayed
/// rounds are those of `session`'s turns. Returns how many were stubbed.
pub(super) async fn stub_replayed_results(
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
    let mut state = ReplayedView {
        view,
        replays,
        request,
        config,
    };
    match Box::pin(ladder(&[&StubReplayed], &mut state)).await {
        Ok(report) => report.removed_items(),
        Err(error) => {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::warn!(error_len, error_sha256, "clearing replayed tool results failed");
            0
        }
    }
}
