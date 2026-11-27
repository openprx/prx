//! The memory keys a tool row prints or carries, shared by the plaintext
//! sweep of derived copies (`sqlite/derived_sweep.rs`) and the encrypted scope,
//! which stores the hashes of these keys beside its sealed tool rows.
use std::collections::HashSet;

/// Tool results of the memory tools that quote a memory's text.
pub(crate) const MEMORY_TOOLS: [&str; 3] = ["memory_recall", "memory_search", "memory_get"];
/// Tools whose call arguments carry a memory's key (`key`) and text
/// (`content`), `tools/memory_store.rs`; it is the only memory write tool.
pub(crate) const MEMORY_WRITE_TOOLS: [&str; 1] = ["memory_store"];
/// What a swept row prints instead of the quoted memory.
pub(crate) const MEMORY_REMOVED_TEXT: &str = "[output removed: it quoted a memory that was forgotten]";

/// Longest printed key whose hash an encrypted row stores; a longer key is
/// not hashed, so its copy is not found (residual r-E3 of the design).
const MAX_HASHED_KEY_BYTES: usize = 512;

/// The distinct keys the printed `output` of `tool` names, as owned text, for
/// the writer of an encrypted row. Same forms as [`printed_keys`].
pub(crate) fn printed_memory_keys(tool: &str, output: &str) -> Vec<String> {
    let mut keys: Vec<String> = printed_keys(tool, output, MAX_HASHED_KEY_BYTES)
        .exact
        .into_iter()
        .map(str::to_owned)
        .collect();
    keys.sort_unstable();
    keys
}

/// The keys a printed result holds (see [`printed_keys`]).
#[derive(Default)]
pub(crate) struct PrintedKeys<'a> {
    /// Keys in the tool's own entry form.
    pub(crate) exact: HashSet<&'a str>,
    /// `memory_recall` keys that end in the redaction mask and whose `:`
    /// separator the mask swallowed: redaction of the printed line carries an
    /// unquoted value through the `:` that follows it, so
    /// `- [core] token=<secret>: text` is stored as
    /// `- [core] token=[REDACTED] text` (or `- [core] token=[REDACTED]` at
    /// the end of the line). Only a redacted key form can match one.
    pub(crate) swallowed: HashSet<&'a str>,
}

/// Start of a `memory_recall` entry line. An entry line may also start with
/// the mask: an entry whose key names a credential and whose content is empty
/// (`- [core] token:` at the end of its line) has the next line's `-` taken
/// into the redacted value, so the next entry reads `[REDACTED] [core] key: text`.
const RECALL_ENTRY_STARTS: [&str; 2] = ["- [", "[REDACTED] ["];

/// The keys of the memory entries that the printed `output` of `tool` holds,
/// each in the form the tool prints it, as the stored (redacted) copy shows
/// it; a key longer than `max_len` bytes is not collected (no probe has one).
/// One pass over the output, so a result is parsed once however many probes
/// there are:
/// - `memory_recall` (`tools/memory_recall.rs`): `- [category] key: content`,
///   or `- [category] key:` at the end of the line; a redacted key may have
///   lost its `:` (see [`PrintedKeys::swallowed`]);
/// - `memory_search` (`tools/memory_search.rs`): a line `- key: key`;
/// - `memory_get` (`tools/memory_get.rs`, `render_range`): the output starts
///   with `key lines a-b:` (or `key is empty.`).
///
/// A key may itself hold `] ` or `: `, so every split of a line that fits is
/// collected; the probes decide which of them is a real key.
pub(crate) fn printed_keys<'a>(tool: &str, output: &'a str, max_len: usize) -> PrintedKeys<'a> {
    let mut keys = PrintedKeys::default();
    match tool {
        "memory_recall" => {
            let mask = crate::security::redaction::REDACTED;
            let ends_entry = |line: &str, at: usize| {
                line.get(at..)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
            };
            for line in output
                .lines()
                .filter(|line| RECALL_ENTRY_STARTS.iter().any(|start| line.starts_with(start)))
            {
                let ends: Vec<usize> = line
                    .match_indices(':')
                    .map(|(at, _)| at)
                    .filter(|at| ends_entry(line, at.saturating_add(1)))
                    .collect();
                let masked_ends: Vec<usize> = line
                    .match_indices(mask)
                    .map(|(at, _)| at.saturating_add(mask.len()))
                    .filter(|end| ends_entry(line, *end))
                    .collect();
                for (at, _) in line.match_indices("] ") {
                    let start = at.saturating_add(2);
                    for (found, set) in [(&ends, &mut keys.exact), (&masked_ends, &mut keys.swallowed)] {
                        let first = found.partition_point(|end| *end < start);
                        for end in found
                            .iter()
                            .skip(first)
                            .take_while(|end| end.saturating_sub(start) <= max_len)
                        {
                            set.extend(line.get(start..*end));
                        }
                    }
                }
            }
        }
        "memory_search" => {
            keys.exact
                .extend(output.lines().filter_map(|line| line.strip_prefix("- key: ")));
        }
        "memory_get" => {
            for marker in [" lines ", " is empty."] {
                for (at, _) in output.match_indices(marker).take_while(|(at, _)| *at <= max_len) {
                    let rest = output.get(at..).unwrap_or_default();
                    let whole = marker != " is empty." || rest == marker || rest.starts_with(" is empty.\n");
                    if whole {
                        keys.exact.extend(output.get(..at));
                    }
                }
            }
        }
        _ => {}
    }
    keys
}
