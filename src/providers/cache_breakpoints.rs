//! Cache breakpoint rule shared by the providers that send explicit
//! `cache_control` marks (native Anthropic, and OpenRouter for `anthropic/`
//! models). The rule lives here once; each provider applies the indices to its
//! own wire shape.

use serde::Serialize;

/// System prompts longer than this many bytes carry a breakpoint.
const SYSTEM_CACHE_MIN_BYTES: usize = 3072;

/// Conversations with more non-system messages than this carry breakpoints.
const CONVERSATION_CACHE_MIN_MESSAGES: usize = 4;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CacheControl {
    #[serde(rename = "type")]
    cache_type: String,
}

impl CacheControl {
    pub(crate) fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".to_string(),
        }
    }
}

/// Whether a system prompt of this text carries a breakpoint.
pub(crate) const fn should_cache_system(text: &str) -> bool {
    text.len() > SYSTEM_CACHE_MIN_BYTES
}

/// Whether the conversation carries breakpoints, given its count of
/// non-system messages.
pub(crate) const fn should_cache_conversation(non_system_count: usize) -> bool {
    non_system_count > CONVERSATION_CACHE_MIN_MESSAGES
}

/// Indices, among the non-system messages, that carry a conversation cache
/// breakpoint. The last message always carries one. When a live user message
/// follows at least one earlier message, the message just before it (the end
/// of the replayed history) carries one too. At most two indices, so a request
/// with tools and a large system prompt stays within Anthropic's four markers.
pub(crate) fn conversation_breakpoint_indices(roles: &[&str]) -> Vec<usize> {
    let Some(last) = roles.len().checked_sub(1) else {
        return Vec::new();
    };
    let mut indices = Vec::with_capacity(2);
    if let Some(history_end) = roles
        .iter()
        .rposition(|role| *role == "user")
        .and_then(|live| live.checked_sub(1))
    {
        indices.push(history_end);
    }
    indices.push(last);
    indices
}
