//! Plain (non-TUI) chat output helpers.

#[cfg(feature = "terminal-tui")]
use super::renderer;
use std::io::IsTerminal as _;
use std::io::Write as _;

pub(crate) fn print_fallback_chat_output(text: &str) {
    let out = format_fallback_chat_output_for(text, std::io::stdout().is_terminal());
    print!("{out}");
    let _ = std::io::stdout().flush();
}

pub(super) fn format_fallback_chat_output_for(text: &str, stdout_is_terminal: bool) -> String {
    if !stdout_is_terminal {
        let mut out = String::with_capacity(text.len() + 1);
        out.push_str(text);
        out.push('\n');
        return out;
    }

    let mut out = String::with_capacity(text.len() + 2);
    let mut prev_was_cr = false;
    for ch in text.chars() {
        if ch == '\n' && !prev_was_cr {
            out.push('\r');
        }
        out.push(ch);
        prev_was_cr = ch == '\r';
    }
    out.push_str("\r\n");
    out
}

// ── Response rendering ───────────────────────────────────────────────────

/// Idempotency policy for the `finalize_draft` failure path.
///
/// Returns `true` if the caller should re-send the full response as a fresh
/// message when `finalize_draft` returns `Err`. Today this is always `false`
/// when a draft was active: streamed deltas have already painted the full
/// text on screen via `update_draft`, so resending would duplicate the
/// assistant turn (this was the P1-4 regression).
///
/// `had_active_draft = true` means a `send_draft` previously succeeded and
/// the user has seen the streamed output. The function is intentionally a
/// pure decision so it can be unit-tested without spinning up a channel.
pub(super) const fn should_resend_on_finalize_failure(had_active_draft: bool) -> bool {
    // If a draft was active, the user already saw the streamed response —
    // resending would duplicate it. The "no draft" path is handled by the
    // caller directly (it is the genuine first send, not a fallback).
    !had_active_draft
}

/// Apply markdown highlighting to a response (when terminal-tui feature is active).
/// Falls back to plain formatting with newline wrapping.
pub(super) fn render_response(response: &str) -> String {
    #[cfg(feature = "terminal-tui")]
    {
        format!("\n{}\n", renderer::render_markdown_with_highlighting(response))
    }
    #[cfg(not(feature = "terminal-tui"))]
    {
        format!("\n{response}\n")
    }
}
