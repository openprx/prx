//! What chat keeps of a finished turn beyond its text, so a later turn's
//! provider view replays it as the turn had it: the form the user's request was
//! sent in.
//!
//! It rides on the stored turn's runtime record
//! ([`crate::agent::tool_evidence::TurnToolEvidence`]) and is rendered by
//! [`crate::agent::turn_history::project`], so the provider view, the
//! compaction guard and the reducer form keep describing the same canonical
//! messages (the canonical history holds the typed text only).

use super::dispatcher::{ChatDispatcher, TurnCompletionSignal};
use crate::agent::tool_replay::Attachment;

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

/// After a turn's commit: hand the reducer the sent form of the request, to
/// attach to the turn it just recorded. Nothing is sent when there is none.
pub(super) async fn attach_turn_records(
    chat_dispatcher: &ChatDispatcher,
    commits: &TurnCompletionSignal,
    user_text: &str,
    sent: Option<&Attachment>,
) {
    let Some(sent) = sent else { return };
    // A failed acknowledgement latches the unconfirmed-commit stop itself.
    let _ = chat_dispatcher
        .dispatch_awaited(
            super::action::Action::TurnRecordsAttached {
                user_text: user_text.to_owned(),
                sent: Some(sent.clone()),
            },
            commits,
        )
        .await;
}
