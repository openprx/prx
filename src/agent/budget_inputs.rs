//! Window-relative budget inputs shared by the turn engine preflight and the
//! chat front end.
//!
//! Three fixed numbers used to be applied whatever the context window was: the
//! reply reserve, the measured tool surface and the count of messages a
//! `switch` keeps word for word. On a small window they consumed it. Each is
//! now bounded by a share of the window, and an operator's explicit value is
//! honoured as an upper bound.

use crate::agent::context_budget::{budgeted_tokens, estimate_history_tokens_with_source};
use crate::config::AgentCompactionConfig;
use crate::providers::ChatMessage;

/// The reply reserve never exceeds `1 / REPLY_RESERVE_WINDOW_DIVISOR` of the window.
pub(crate) const REPLY_RESERVE_WINDOW_DIVISOR: usize = 4;

/// The reply reserve plus the tool surface never exceed
/// `1 / TOTAL_RESERVE_WINDOW_DIVISOR` of the window.
pub(crate) const TOTAL_RESERVE_WINDOW_DIVISOR: usize = 2;

/// The published tool definitions may take at most
/// `1 / TOOL_SURFACE_WINDOW_DIVISOR` of the window and still be published in
/// full. A quarter is what is left of the half that [`reserved_budget_config`]
/// allows reply plus tools once the reply reserve has taken its own quarter,
/// so a surface within it never meets the total cap.
pub(crate) const TOOL_SURFACE_WINDOW_DIVISOR: usize = 4;

/// Whether `tool_tokens` of tool definitions fit their share of `window_tokens`.
pub(crate) const fn tool_surface_fits_window(window_tokens: usize, tool_tokens: usize) -> bool {
    tool_tokens.saturating_mul(TOOL_SURFACE_WINDOW_DIVISOR) <= window_tokens
}

/// The share of the window the tool definitions may take, in percent.
pub(crate) const fn tool_surface_share_percent() -> usize {
    100 / TOOL_SURFACE_WINDOW_DIVISOR
}

/// The messages a switch keeps word for word occupy at most
/// `1 / KEEP_RECENT_WINDOW_DIVISOR` of the window.
pub(crate) const KEEP_RECENT_WINDOW_DIVISOR: usize = 4;

/// The reply reserve for a window: the configured value, bounded by its share
/// of the window.
pub(crate) const fn capped_reply_reserve(max_context_tokens: usize, reserve_tokens: usize) -> usize {
    let cap = max_context_tokens / REPLY_RESERVE_WINDOW_DIVISOR;
    if reserve_tokens < cap { reserve_tokens } else { cap }
}

/// The config one request is budgeted with: the reply reserve plus
/// `tool_tokens` (the budgeted tool surface), the sum bounded by its share of
/// the window and never below the configured reserve itself.
///
/// The turn engine preflight and the chat pre-turn check both derive their
/// budget here, so the same inputs give the same limit.
pub(crate) fn reserved_budget_config(config: &AgentCompactionConfig, tool_tokens: usize) -> AgentCompactionConfig {
    let total_cap = (config.max_context_tokens / TOTAL_RESERVE_WINDOW_DIVISOR).max(config.reserve_tokens);
    AgentCompactionConfig {
        reserve_tokens: config.reserve_tokens.saturating_add(tool_tokens).min(total_cap),
        ..config.clone()
    }
}

/// Token budget of the messages kept word for word.
fn keep_recent_token_budget(config: &AgentCompactionConfig) -> usize {
    let window_share = config.max_context_tokens / KEEP_RECENT_WINDOW_DIVISOR;
    let input_share = config.max_context_tokens.saturating_sub(config.reserve_tokens) / 2;
    window_share.min(input_share)
}

/// The most messages a switch may keep word for word out of `non_system_len`:
/// the configured `keep_recent_messages`, at least one and at most the messages
/// there are.
pub(crate) fn keep_recent_cap(config: &AgentCompactionConfig, non_system_len: usize) -> usize {
    config.keep_recent_messages.max(1).min(non_system_len)
}

/// How many of the newest messages a switch keeps word for word, given the
/// budgeted token weight of each message.
///
/// `weights` is index-aligned with the non-system messages, oldest first.
/// Newest first, a message is kept while the kept messages stay within a share
/// of the window; the newest one is always kept. `cap` (see
/// [`keep_recent_cap`]) is an upper bound, so a short history of large messages
/// folds, and a long one keeps at most the configured count.
pub(crate) fn effective_keep_recent_weighted(weights: &[usize], cap: usize, config: &AgentCompactionConfig) -> usize {
    let budget = keep_recent_token_budget(config);
    let mut used = 0usize;
    let mut kept = 0usize;
    for tokens in weights.iter().rev().take(cap) {
        used = used.saturating_add(*tokens);
        if kept >= 1 && used > budget {
            break;
        }
        kept += 1;
    }
    kept
}

/// How many of the newest `non_system` messages a switch keeps word for word,
/// each weighed by its own text. See [`effective_keep_recent_weighted`].
pub(crate) fn effective_keep_recent_messages(non_system: &[ChatMessage], config: &AgentCompactionConfig) -> usize {
    let cap = keep_recent_cap(config, non_system.len());
    let weights: Vec<usize> = non_system
        .get(non_system.len().saturating_sub(cap)..)
        .unwrap_or_default()
        .iter()
        .map(|message| budgeted_tokens(estimate_history_tokens_with_source(std::slice::from_ref(message))))
        .collect();
    effective_keep_recent_weighted(&weights, cap, config)
}
