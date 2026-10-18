//! Canonical context-overflow classification for the turn engine.
//!
//! Provider adapters use this predicate to avoid transport retries and ingress
//! adapters use it for user-facing failure classification. History compaction
//! and provider replay remain exclusively in `crate::agent::turn_engine`.

/// Classify whether an error represents an LLM context-window / token-limit
/// overflow. Every provider and ingress delegates to this predicate so the
/// engine is the sole owner of the classification used to compact and replay a
/// request.
#[must_use]
pub fn is_context_overflow_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| is_context_overflow_message(&cause.to_string()))
}

fn is_context_overflow_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
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
        || message.contains("input token count")
        || message.contains("input is too long")
        // Provider phrasings that omit the word "context".
        || message.contains("token limit")
        || message.contains("max_tokens")
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
            "input is too long",
            "input token count exceeds this model's limit",
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
    fn detector_reads_wrapped_error_causes() {
        let error = err("input is too long").context("provider request failed");
        assert!(is_context_overflow_error(&error));
    }
}
