//! Unified context-overflow recovery (FIX-P1-12).
//!
//! Historically the three LLM-call paths handled a "context window exceeded"
//! provider error in three different ways:
//!
//! * Path A (direct `Agent::turn`) aborted immediately with no recovery.
//! * Path B (streaming) compacted history in place, then retried.
//! * Path C (non-streaming tool loop) compacted history in place, then retried.
//!
//! This module collapses all three into one canonical detect + compact + retry
//! decision so every path reacts identically to the same failure class.

use crate::providers::ChatMessage;

/// Outcome of an overflow-recovery attempt, returned by
/// [`handle_overflow_with_retry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowOutcome {
    /// The error was a context overflow, history was successfully compacted, and
    /// the caller should retry the LLM call.
    Retry,
    /// The error was a context overflow but nothing could be compacted (history
    /// already minimal) or the retry budget is exhausted — surface the error.
    GiveUp,
    /// The error was not a context overflow — the caller must propagate it
    /// unchanged.
    NotOverflow,
}

/// Classify whether an error represents an LLM context-window / token-limit
/// overflow. Kept deliberately broad (substring match on the lowercased message)
/// because provider error strings vary; the cost of a false positive is one
/// extra compaction, while a false negative re-introduces the silent-abort bug.
#[must_use]
pub fn is_context_overflow_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    // "context length" / "context window" / "maximum context"
    (message.contains("context")
        && (message.contains("window") || message.contains("length") || message.contains("token")))
        // OpenAI-style: "maximum context length is N tokens"
        || message.contains("maximum context")
        // OpenAI error code, surfaced verbatim in some provider error strings.
        || message.contains("context_length_exceeded")
        // Anthropic-style: "prompt is too long"
        || (message.contains("prompt") && message.contains("too long"))
        || message.contains("too many tokens")
        || message.contains("reduce the length")
        // Provider phrasings that omit the word "context".
        || message.contains("token limit")
        || message.contains("max_tokens")
}

/// Compact a conversation history in place by dropping the oldest non-leading
/// messages, preserving any leading `system` / `developer` messages and the most
/// recent `keep_recent` messages. Returns the number of messages removed.
///
/// `keep_recent` is clamped to at least 1 so a retry always keeps the latest
/// user turn. The function is a no-op (returns 0) when there is nothing safe to
/// drop, which lets the caller distinguish "compacted, retry" from "already
/// minimal, give up".
#[must_use]
pub fn compact_history_in_place(history: &mut Vec<ChatMessage>, keep_recent: usize) -> usize {
    let keep_recent = keep_recent.max(1);
    let leading = leading_pinned_count(history);
    let original_len = history.len();

    // Nothing droppable: everything is either pinned leading context or within
    // the recent window.
    if original_len <= leading + keep_recent {
        return 0;
    }

    let drop_count = original_len - leading - keep_recent;
    if drop_count == 0 {
        return 0;
    }
    history.drain(leading..leading + drop_count);
    drop_count
}

/// Count leading messages that must be pinned (never dropped): a contiguous run
/// of `system` / `developer` roles at the front of the history.
fn leading_pinned_count(history: &[ChatMessage]) -> usize {
    history
        .iter()
        .take_while(|m| {
            let role = m.role.as_str();
            role == "system" || role == "developer"
        })
        .count()
}

/// Canonical overflow recovery decision for every LLM-call path (FIX-P1-12).
///
/// * Returns [`OverflowOutcome::NotOverflow`] when `error` is not a context
///   overflow — the caller must propagate the error unchanged.
/// * Returns [`OverflowOutcome::GiveUp`] when the retry budget is exhausted
///   (`attempt >= max_retries`) or the history could not be compacted further.
/// * Otherwise compacts `history` in place and returns
///   [`OverflowOutcome::Retry`], signalling the caller to retry the call.
///
/// `attempt` is the zero-based count of overflow retries already performed.
pub fn handle_overflow_with_retry(
    error: &anyhow::Error,
    history: &mut Vec<ChatMessage>,
    attempt: usize,
    max_retries: usize,
    keep_recent: usize,
) -> OverflowOutcome {
    if !is_context_overflow_error(error) {
        return OverflowOutcome::NotOverflow;
    }
    if attempt >= max_retries {
        return OverflowOutcome::GiveUp;
    }
    let removed = compact_history_in_place(history, keep_recent);
    if removed == 0 {
        OverflowOutcome::GiveUp
    } else {
        OverflowOutcome::Retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(msg: &str) -> anyhow::Error {
        anyhow::anyhow!("{msg}")
    }

    #[test]
    fn detects_common_overflow_phrasings() {
        assert!(is_context_overflow_error(&err(
            "This model's maximum context length is 8192 tokens"
        )));
        assert!(is_context_overflow_error(&err("context window exceeded for request")));
        assert!(is_context_overflow_error(&err("prompt is too long")));
        assert!(is_context_overflow_error(&err(
            "Please reduce the length of the messages"
        )));
    }

    #[test]
    fn detector_is_superset_of_legacy_loop_phrasings() {
        // FIX-P1-12: the agent tool loop historically used its own detector with
        // these phrasings. The unified detector MUST catch every one of them so
        // replacing the loop-local detector does not regress detection.
        for phrasing in [
            "context_length_exceeded",
            "This model's maximum context length is 8192 tokens",
            "token limit reached",
            "too many tokens in request",
            "context window exceeded",
            "max_tokens is too large for this model",
            "prompt is too long",
        ] {
            assert!(
                is_context_overflow_error(&err(phrasing)),
                "unified detector must catch legacy loop phrasing: {phrasing:?}"
            );
        }
    }

    #[test]
    fn ignores_unrelated_errors() {
        assert!(!is_context_overflow_error(&err("rate limit exceeded (429)")));
        assert!(!is_context_overflow_error(&err("connection timed out")));
        assert!(!is_context_overflow_error(&err("unauthorized (401)")));
    }

    #[test]
    fn compaction_preserves_leading_system_and_recent_window() {
        let mut history = vec![
            ChatMessage::system("sys".to_string()),
            ChatMessage::user("old-1".to_string()),
            ChatMessage::assistant("old-2".to_string()),
            ChatMessage::user("old-3".to_string()),
            ChatMessage::assistant("recent-1".to_string()),
            ChatMessage::user("recent-2".to_string()),
        ];
        let removed = compact_history_in_place(&mut history, 2);
        assert_eq!(removed, 3, "three middle messages dropped");
        let roles: Vec<&str> = history.iter().map(|m| m.role.as_str()).collect();
        let contents: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
        // Leading system message preserved; most recent two messages survive.
        assert_eq!(roles, vec!["system", "assistant", "user"]);
        assert_eq!(contents, vec!["sys", "recent-1", "recent-2"]);
    }

    #[test]
    fn compaction_is_noop_when_already_minimal() {
        let mut history = vec![
            ChatMessage::system("sys".to_string()),
            ChatMessage::user("only".to_string()),
        ];
        let removed = compact_history_in_place(&mut history, 4);
        assert_eq!(removed, 0);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn unified_helper_retries_on_overflow_then_gives_up() {
        let mut history = vec![
            ChatMessage::system("sys".to_string()),
            ChatMessage::user("u1".to_string()),
            ChatMessage::assistant("a1".to_string()),
            ChatMessage::user("u2".to_string()),
            ChatMessage::assistant("a2".to_string()),
        ];
        let overflow = err("maximum context length is 100 tokens");

        // First overflow: compacts and signals retry.
        let outcome = handle_overflow_with_retry(&overflow, &mut history, 0, 3, 1);
        assert_eq!(outcome, OverflowOutcome::Retry);
        assert!(history.len() < 5);

        // Once compacted to the minimum, further overflow gives up rather than
        // spinning forever.
        let mut minimal = vec![
            ChatMessage::system("sys".to_string()),
            ChatMessage::user("only".to_string()),
        ];
        let outcome = handle_overflow_with_retry(&overflow, &mut minimal, 1, 3, 1);
        assert_eq!(outcome, OverflowOutcome::GiveUp);
    }

    #[test]
    fn unified_helper_passes_through_non_overflow_errors() {
        let mut history = vec![ChatMessage::user("u1".to_string())];
        let outcome = handle_overflow_with_retry(&err("rate limit (429)"), &mut history, 0, 3, 1);
        assert_eq!(outcome, OverflowOutcome::NotOverflow);
        assert_eq!(history.len(), 1, "non-overflow errors never mutate history");
    }

    #[test]
    fn unified_helper_respects_retry_budget() {
        let mut history = vec![
            ChatMessage::system("sys".to_string()),
            ChatMessage::user("u1".to_string()),
            ChatMessage::assistant("a1".to_string()),
            ChatMessage::user("u2".to_string()),
        ];
        // attempt == max_retries -> give up immediately without compacting.
        let outcome = handle_overflow_with_retry(&err("context window exceeded"), &mut history, 3, 3, 1);
        assert_eq!(outcome, OverflowOutcome::GiveUp);
        assert_eq!(history.len(), 4, "budget-exhausted path leaves history intact");
    }
}
