//! The recent history the plain REPL prints when it resumes a saved session
//! (`--session <id>` or `--continue`), read from the stored transcript.

use super::input_queue::truncate_chars_for_queued_notice;
use super::session::ChatSession;

/// Most recent user and assistant messages shown.
const RECAP_MESSAGES: usize = 10;
/// Longest message text shown, the `…` included.
const RECAP_MESSAGE_CHARS: usize = 500;
const COMPACTED_TURNS_TEXT: &str = "[earlier turns were compacted]";

/// The recap of `session`'s last messages, oldest first, under one header
/// line; `None` when the session holds no user or assistant message.
pub(super) fn format_resume_recap(session: &ChatSession) -> Option<String> {
    let messages: Vec<_> = session
        .turns
        .iter()
        .filter(|turn| matches!(turn.role.as_str(), "user" | "assistant"))
        .collect();
    let total = messages.len();
    if total == 0 {
        return None;
    }
    let skipped = total.saturating_sub(RECAP_MESSAGES);
    let mut recap = format!(
        "[previous session {}: last {} of {total} messages]",
        session.id,
        total.min(RECAP_MESSAGES)
    );
    for turn in messages.iter().skip(skipped) {
        let prefix = if turn.role == "user" { "you> " } else { "prx> " };
        let text =
            if turn.context_handoff || crate::agent::compaction::is_context_handoff_turn(&turn.role, &turn.content) {
                COMPACTED_TURNS_TEXT.to_owned()
            } else {
                truncate_chars_for_queued_notice(&turn.content, RECAP_MESSAGE_CHARS)
            };
        recap.push('\n');
        recap.push_str(prefix);
        recap.push_str(&text);
    }
    Some(recap)
}
