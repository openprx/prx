use crate::markdown::walk_markdown_lines;
use crate::providers::ChatMessage;
use std::borrow::Cow;

pub(crate) const EMPTY_ASSISTANT_RESPONSE_MESSAGE: &str = "model returned empty response";

pub(super) const EMPTY_ASSISTANT_RESPONSE_RETRY_MESSAGE: &str = "The previous assistant response was empty. \
     Continue the pending work now: either invoke the next required tools or provide a complete final answer. \
     Do not return an empty response.";

pub(super) const TOOL_SUMMARY_ONLY_RESPONSE_MESSAGE: &str = "The previous assistant response was an internal tool-use summary, not a valid answer. \
     Either invoke the required tools now or provide a complete final answer. \
     Do not output a '[Used tools: ...]' summary.";

/// Marks the runtime's own words inside a user turn.
///
/// A corrective retry instruction is not something the operator said, so it is
/// labelled. Without the label the model reads it as the user's latest request
/// and can answer it ("understood, I will not return an empty reply") or quote
/// it back, and anything downstream that shows the user their own prompt cannot
/// tell the two apart. The prefix is the split point: everything before the
/// first occurrence is the operator's text, everything from it on is this
/// runtime's.
pub(crate) const CORRECTIVE_ASIDE_PREFIX: &str = "[system note]";

/// Append a corrective instruction to the request as a labelled aside.
///
/// It is folded into the trailing user turn when there is one rather than
/// pushed as a second user message: `[system, user, user]` is a shape no
/// provider in this crate coalesces (`src/providers/anthropic.rs`,
/// `src/providers/gemini.rs` forward same-role runs verbatim), and strict chat
/// templates reject alternation breaks outright. When the tail is not a user
/// turn — a tool result, for instance — the aside becomes its own user message,
/// which cannot create an adjacent pair either.
pub(super) fn append_corrective_aside(messages: &mut Vec<ChatMessage>, instruction: &str) {
    match messages.last_mut() {
        Some(last) if last.role == "user" => {
            last.content.push_str("\n\n");
            last.content.push_str(CORRECTIVE_ASIDE_PREFIX);
            last.content.push(' ');
            last.content.push_str(instruction);
        }
        _ => messages.push(ChatMessage::user(format!("{CORRECTIVE_ASIDE_PREFIX} {instruction}"))),
    }
}

pub(crate) fn is_empty_assistant_response(text: &str, has_tool_calls: bool) -> bool {
    text.trim().is_empty() && !has_tool_calls
}

pub(crate) fn is_tool_summary_only_response(text: &str) -> bool {
    let trimmed = text.trim();
    let Some(tool_names) = trimmed
        .strip_prefix("[Used tools:")
        .and_then(|value| value.strip_suffix(']'))
    else {
        return false;
    };

    !tool_names.trim().is_empty() && !tool_names.contains(['\n', '\r'])
}

/// True when `text` carries at least one fabricated `[Used tools: ...]` line.
///
/// The runtime has never produced that marker: it only ever appeared in model
/// output. Once ~150 of those assistant turns were persisted, the model started
/// imitating the shape in long contexts and claiming tool runs it never made.
/// A marker is recognized per *line* (surrounding whitespace allowed), which is
/// exactly the shape `is_tool_summary_only_response` accepts for a whole
/// response, so producer and detector cannot drift apart.
pub(crate) fn contains_fabricated_tool_marker(text: &str) -> bool {
    text.lines().any(is_tool_summary_only_response)
}

/// Remove every fabricated `[Used tools: ...]` line from `text`.
///
/// Borrowed (zero allocation) when there is nothing to strip. Only whole lines
/// are dropped; a marker embedded inside a sentence and any other bracketed text
/// are left alone. When a line is dropped the surrounding blank space is trimmed
/// so the text does not start or end with the hole the marker left behind.
///
/// This is the coarse grain only. Delivery, history and replay go through
/// [`strip_assistant_fabricated_tool_markers`], which adds the in-line grain
/// with code-preservation rules; quoted context goes through
/// [`strip_quoted_fabricated_tool_markers`]. Keeping this one whole-line is what
/// lets both of them drop a marker line without leaving a blank line behind.
pub(crate) fn strip_fabricated_tool_markers(text: &str) -> Cow<'_, str> {
    if !contains_fabricated_tool_marker(text) {
        return Cow::Borrowed(text);
    }

    let mut stripped = String::with_capacity(text.len());
    for line in text.lines().filter(|line| !is_tool_summary_only_response(line)) {
        if !stripped.is_empty() {
            stripped.push('\n');
        }
        stripped.push_str(line);
    }
    let leading = stripped.len() - stripped.trim_start().len();
    stripped.drain(..leading);
    stripped.truncate(stripped.trim_end().len());
    Cow::Owned(stripped)
}

/// Remove fabricated `[Used tools: ...]` fragments that sit *inside* a line.
///
/// The line-level strip above cannot see a marker once the quoting preambles
/// have flattened a stored turn into a single prompt line
/// (`neutralize_quoted_content` replaces every `\n` with a space), which is
/// exactly the shape the persisted assistant rows take on their way back into
/// `[Current session context]`. Only a bracketed fragment that starts with
/// `[Used tools:`, names at least one tool and closes on the same line is
/// dropped; every other bracketed text is left byte-for-byte alone, so quoting
/// prose or media markers cannot be mangled. Whitespace around the removed
/// fragment is collapsed to a single separator so the sentence still reads.
pub(crate) fn strip_inline_fabricated_tool_markers(text: &str) -> Cow<'_, str> {
    const MARKER_PREFIX: &str = FABRICATED_TOOL_MARKER_PREFIX;

    if !text.contains(MARKER_PREFIX) {
        return Cow::Borrowed(text);
    }

    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    let mut stripped_any = false;
    while let Some(start) = rest.find(MARKER_PREFIX) {
        let after_prefix = &rest[start + MARKER_PREFIX.len()..];
        let Some(close) = after_prefix.find(']') else {
            break;
        };
        let tool_names = &after_prefix[..close];
        if tool_names.trim().is_empty() || tool_names.contains(['\n', '\r']) {
            // Not a marker (empty list, or the bracket spans a line break):
            // keep it verbatim and resume scanning after the prefix.
            kept.push_str(&rest[..start + MARKER_PREFIX.len()]);
            rest = after_prefix;
            continue;
        }
        kept.push_str(&rest[..start]);
        while kept.ends_with([' ', '\t']) {
            kept.pop();
        }
        rest = after_prefix[close + 1..].trim_start_matches([' ', '\t']);
        stripped_any = true;
        let needs_separator = !kept.is_empty()
            && !kept.ends_with('\n')
            && !rest.is_empty()
            && !rest.starts_with(['\n', '.', ',', ';', ':', '!', '?', ')']);
        if needs_separator {
            kept.push(' ');
        }
    }

    if !stripped_any {
        return Cow::Borrowed(text);
    }
    kept.push_str(rest);
    Cow::Owned(kept)
}

/// Prefix of the fabricated tool-use marker. Shared by every detector and
/// cleaner below so the grains can never disagree about what a marker looks
/// like.
const FABRICATED_TOOL_MARKER_PREFIX: &str = "[Used tools:";

/// End offset (exclusive) of the inline code span whose opening backtick run
/// starts at `open`, or `None` when that run is never closed by a run of the
/// same length.
fn inline_code_span_end(line: &str, open: usize) -> Option<usize> {
    let run = line[open..].bytes().take_while(|byte| *byte == b'`').count();
    let mut cursor = open + run;
    while let Some(offset) = line[cursor..].find('`') {
        let close = cursor + offset;
        let close_run = line[close..].bytes().take_while(|byte| *byte == b'`').count();
        if close_run == run {
            return Some(close + close_run);
        }
        cursor = close + close_run;
    }
    None
}

/// Remove in-line fabricated markers from one line, leaving inline code spans
/// (`` `[Used tools: shell]` ``) byte-for-byte alone.
///
/// Removal and whitespace-collapsing rules are the ones
/// [`strip_inline_fabricated_tool_markers`] documents; the only difference is
/// that a marker delimited by backticks is documentation about the syntax — the
/// shape this very module has to describe — not a claim about work, so it
/// survives.
fn strip_inline_markers_outside_code_spans(line: &str) -> Cow<'_, str> {
    if !line.contains(FABRICATED_TOOL_MARKER_PREFIX) {
        return Cow::Borrowed(line);
    }

    let mut kept = String::with_capacity(line.len());
    let mut rest = line;
    let mut stripped_any = false;
    while let Some(marker) = rest.find(FABRICATED_TOOL_MARKER_PREFIX) {
        // A code span opening before the candidate protects everything up to its
        // closing run: copy it verbatim and resume scanning after it.
        if let Some(tick) = rest.find('`')
            && tick < marker
            && let Some(span_end) = inline_code_span_end(rest, tick)
        {
            kept.push_str(&rest[..span_end]);
            rest = &rest[span_end..];
            continue;
        }

        let after_prefix = &rest[marker + FABRICATED_TOOL_MARKER_PREFIX.len()..];
        let Some(close) = after_prefix.find(']') else {
            break;
        };
        let tool_names = &after_prefix[..close];
        if tool_names.trim().is_empty() || tool_names.contains(['\n', '\r']) {
            // Not a marker (empty list, or the bracket spans a line break):
            // keep it verbatim and resume scanning after the prefix.
            kept.push_str(&rest[..marker + FABRICATED_TOOL_MARKER_PREFIX.len()]);
            rest = after_prefix;
            continue;
        }
        kept.push_str(&rest[..marker]);
        while kept.ends_with([' ', '\t']) {
            kept.pop();
        }
        rest = after_prefix[close + 1..].trim_start_matches([' ', '\t']);
        stripped_any = true;
        let needs_separator =
            !kept.is_empty() && !rest.is_empty() && !rest.starts_with(['.', ',', ';', ':', '!', '?', ')']);
        if needs_separator {
            kept.push(' ');
        }
    }

    if !stripped_any {
        return Cow::Borrowed(line);
    }
    kept.push_str(rest);
    Cow::Owned(kept)
}

/// Remove fabricated `[Used tools: ...]` markers from text the assistant itself
/// authored — the delivered answer, the assistant history turn that accompanies
/// real tool calls, replayed assistant turns and compaction summaries.
///
/// Both grains the model writes are handled here, which is what the line-level
/// [`strip_fabricated_tool_markers`] alone could not do: a marker that owns a
/// whole line is dropped with its line (leaving no hole behind), and a marker
/// embedded in prose (`Fixture checked. [Used tools: shell]`) is cut out of the
/// sentence with the surrounding whitespace collapsed to one separator.
///
/// Preservation rules, so the cleaner cannot mangle a legitimate answer:
/// * fenced code blocks are copied verbatim, marker lines included; the block
///   runs from its opening fence to the first line that closes it under
///   [`CodeFence`] (same character, at least as long, nothing but trailing
///   whitespace after it) or, when there is none, to the end of the text;
/// * inline code spans are copied verbatim, so prose *about* the marker syntax
///   survives when it is quoted as code;
/// * every other bracketed text is untouched — only a fragment that starts with
///   `[Used tools:`, names at least one tool and closes on the same line is a
///   marker;
/// * user turns are never passed through this function (see
///   [`strip_fabricated_tool_markers_from_assistant_turns`]), so an operator
///   quoting a marker back keeps their own words.
///
/// Borrowed (zero allocation) whenever there is nothing to strip, which is the
/// overwhelmingly common case: this runs on every delivered response and every
/// replayed history turn.
///
/// Quoted context uses [`strip_quoted_fabricated_tool_markers`] instead: that
/// content has already been flattened to a single line, so code fences no longer
/// exist there to be recognized.
pub(crate) fn strip_assistant_fabricated_tool_markers(text: &str) -> Cow<'_, str> {
    if !text.contains(FABRICATED_TOOL_MARKER_PREFIX) {
        return Cow::Borrowed(text);
    }

    let mut stripped = String::with_capacity(text.len());
    let mut changed = false;
    for (line, in_fenced_block) in walk_markdown_lines(text) {
        if !in_fenced_block {
            if is_tool_summary_only_response(line) {
                // A whole marker line carries nothing else: drop it outright so
                // no blank line is left where it stood.
                changed = true;
                continue;
            }
            let cleaned = strip_inline_markers_outside_code_spans(line);
            if let Cow::Owned(cleaned) = cleaned {
                changed = true;
                if !stripped.is_empty() {
                    stripped.push('\n');
                }
                stripped.push_str(&cleaned);
                continue;
            }
        }
        if !stripped.is_empty() {
            stripped.push('\n');
        }
        stripped.push_str(line);
    }

    if !changed {
        return Cow::Borrowed(text);
    }
    let leading = stripped.len() - stripped.trim_start().len();
    stripped.drain(..leading);
    stripped.truncate(stripped.trim_end().len());
    Cow::Owned(stripped)
}

/// True when `text` carries a fabricated marker in any grain the model can write
/// it: on a line of its own, or embedded in a sentence.
///
/// Defined as "the assistant-surface cleaner would change this text", so the
/// detector that spends the one corrective retry and the cleaner that runs
/// before delivery can never disagree — including about the code-span and code
/// fence exemptions, which must not buy a retry either.
pub(crate) fn contains_assistant_fabricated_tool_marker(text: &str) -> bool {
    matches!(strip_assistant_fabricated_tool_markers(text), Cow::Owned(_))
}

/// Strip fabricated tool markers from content that is about to be *quoted*
/// back into a prompt (session/shared event preambles, document evidence,
/// recalled cold pages).
///
/// Quoted content is flattened to one line on the way in, so both grains are
/// needed: whole marker lines first (they carry no other content and should
/// leave no blank line behind), then the in-line remnants the flattening would
/// otherwise hide from the line-level detector.
pub(crate) fn strip_quoted_fabricated_tool_markers(text: &str) -> Cow<'_, str> {
    match strip_fabricated_tool_markers(text) {
        Cow::Borrowed(borrowed) => strip_inline_fabricated_tool_markers(borrowed),
        Cow::Owned(owned) => Cow::Owned(strip_inline_fabricated_tool_markers(&owned).into_owned()),
    }
}

/// Strip fabricated tool markers from assistant turns of a replayed history.
///
/// Only the copy handed to the model is touched; the persisted rows keep the
/// bytes the model actually produced. User turns are never rewritten — an
/// operator quoting the marker back is real conversation content.
///
/// Inline-aware ([`strip_assistant_fabricated_tool_markers`]): rows persisted
/// before the delivery guard existed carry markers in both grains, and a
/// sentence-embedded one replays into every later prompt exactly like a
/// standalone line does.
pub(crate) fn strip_fabricated_tool_markers_from_assistant_turns(turns: &mut [ChatMessage]) {
    for turn in turns.iter_mut().filter(|turn| turn.role == "assistant") {
        if let Cow::Owned(stripped) = strip_assistant_fabricated_tool_markers(&turn.content) {
            turn.content = stripped;
        }
    }
}

#[path = "turn_tests/response_guard.rs"]
mod tests;
