//! Stored copies of a memory read that shares a tool batch with a later
//! `memory_forget` of a key it printed.
//!
//! The result rows of a batch are written after the whole batch ran, so the
//! forget's sweep of derived copies (`memory/sqlite/derived_sweep.rs`) never
//! sees the rows of an earlier read in the same batch. The decision is made
//! from the batch itself: the forget call's `key` argument and its own result.
//! Only the stored rows change; the text the running turn already holds does
//! not (the model read it before the forget ran).

use crate::agent::tool_call_parser::ParsedToolCall;
use crate::agent::tool_result_recording::ObservedToolResult;
use crate::memory::printed_keys::{MEMORY_REMOVED_TEXT, MEMORY_TOOLS, printed_keys};

/// Start of the output `memory_forget` prints when it removed a memory
/// (`tools/memory_forget.rs`); a missing key prints a different line.
const FORGOT_PREFIX: &str = "Forgot memory: ";

/// The `(call index, key)` of every `memory_forget` of the batch that removed
/// a memory.
pub(super) fn forgotten_in_batch(calls: &[ParsedToolCall], outcomes: &[ObservedToolResult]) -> Vec<(usize, String)> {
    calls
        .iter()
        .zip(outcomes)
        .enumerate()
        .filter(|(_, (call, outcome))| {
            call.name == "memory_forget" && outcome.succeeded() && outcome.content.starts_with(FORGOT_PREFIX)
        })
        .filter_map(|(index, (call, _))| {
            let key = call.arguments.get("key")?.as_str()?;
            (!key.is_empty()).then(|| (index, key.to_owned()))
        })
        .collect()
}

/// The text to store instead of `text`, the effective result of call `index`
/// of tool `tool`, when a later call of the batch forgot a memory that `text`
/// prints. A key is compared in its own form and in its redacted form, as the
/// sweep does.
pub(super) fn masked_for_forget(
    index: usize,
    tool: &str,
    text: &str,
    forgotten: &[(usize, String)],
) -> Option<&'static str> {
    if !MEMORY_TOOLS.contains(&tool) || text == MEMORY_REMOVED_TEXT {
        return None;
    }
    let later = || forgotten.iter().filter(|(forget_index, _)| *forget_index > index);
    let max_len = later().map(|(_, key)| key.len()).max()?;
    let redacted: Vec<String> = later()
        .map(|(_, key)| crate::security::redaction::redact_secrets(key))
        .collect();
    let max_len = redacted.iter().map(String::len).fold(max_len, usize::max);
    let printed = printed_keys(tool, text, max_len);
    later()
        .zip(&redacted)
        .any(|((_, key), redacted)| {
            printed.exact.contains(key.as_str())
                || printed.exact.contains(redacted.as_str())
                || printed.swallowed.contains(redacted.as_str())
        })
        .then_some(MEMORY_REMOVED_TEXT)
}
