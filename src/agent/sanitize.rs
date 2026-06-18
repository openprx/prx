//! Streaming chunk sanitization.
//!
//! When the agent loop relays LLM output to a channel via the streaming
//! sender, the raw text can occasionally contain tool-call artifacts that
//! leaked through the provider layer — for example an isolated
//! `<tool_call>...</tool_call>` block or a bare `{"name": "shell", ...}`
//! JSON object on its own line. The full-document scrubber
//! [`crate::channels::sanitize_channel_response`] is invoked *after* the
//! response has finished arriving, which means a streaming channel will
//! first paint the dirty artifacts on screen and only later replace them
//! with the cleaned text — producing a visible flicker.
//!
//! This module pushes the same cleaning logic down to the streaming entry
//! point so each chunk is already clean before it enters the channel
//! `mpsc::Sender`. Because [`crate::agent::stream_buffer::StreamBoundaryBuffer`]
//! only flushes at semantic boundaries (closed XML tags / balanced JSON
//! objects / arrays), every chunk we receive here is a well-formed unit
//! that the full-document scrubber can analyse in isolation.
//!
//! ## Hot-path performance
//!
//! Streaming chunks are produced word-by-word, so this function is on the
//! hot path. We apply a cheap byte-set prefilter (`<`, `{`, `[`) before any
//! allocation: if the chunk cannot possibly contain a tool-call artifact
//! we return [`Cow::Borrowed`] without touching the heap. Only when the
//! prefilter matches do we run the structural scrubbers, and even then we
//! return the original `Cow::Borrowed` when the scrubbers produce a
//! byte-identical result.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::LazyLock;

use regex::bytes::Regex;

use crate::channels::{
    strip_isolated_tool_json_artifacts_preserve_whitespace, strip_isolated_tool_tag_artifacts_preserve_whitespace,
};
use crate::tools::Tool;

/// Byte-level prefilter for the structural sanitisers.
///
/// The full-document scrubbers look for tool-call XML tags (`<tool_call>`,
/// `<tool_use>`, …) or JSON objects/arrays whose payload references a
/// known tool. Both anchor on `<`, `{`, or `[`. If a chunk contains none
/// of these bytes, no sanitisation can ever change it — short-circuit.
///
/// The regex matches bytes (not chars) because all three sentinel
/// characters are pure ASCII and a byte scan is the fastest possible test.
#[allow(clippy::expect_used)]
static PREFILTER: LazyLock<Regex> = LazyLock::new(|| {
    // The pattern is a compile-time constant character class. A failure
    // here is a programming error and would surface at first use, not in
    // production — `expect` is the documented convention in this crate
    // (see `tools/web_fetch.rs`, `memory/principal.rs`, …).
    Regex::new(r"[<\{\[]").expect("BUG: invalid hardcoded sanitize prefilter regex")
});

/// Lower-case names of every registered tool. Used by the scrubbers to
/// decide whether a JSON object that mentions a `name` / `function.name`
/// field really is a tool call.
///
/// Borrowing a precomputed set keeps the per-chunk cost to a hashset
/// lookup rather than rebuilding it on every word.
pub(crate) fn known_tool_names(tools: &[Box<dyn Tool>]) -> HashSet<String> {
    tools.iter().map(|tool| tool.name().to_ascii_lowercase()).collect()
}

/// Strip tool-call XML / JSON artifacts from a single streaming chunk.
///
/// Returns [`Cow::Borrowed`] when the chunk is already clean (the common
/// case for plain-text words on the hot path) and [`Cow::Owned`] only
/// when at least one artifact was removed.
///
/// This is a per-chunk replacement for the post-hoc
/// [`crate::channels::sanitize_channel_response`]: callers should invoke
/// it on every flushed chunk before forwarding it to the channel sender.
pub(crate) fn sanitize_stream_chunk<'a>(input: &'a str, known_tool_names: &HashSet<String>) -> Cow<'a, str> {
    // Fast path 1: empty input — nothing to do.
    if input.is_empty() {
        return Cow::Borrowed(input);
    }

    // Fast path 2: no `<`, `{`, or `[` in the chunk — no artifact possible.
    if !PREFILTER.is_match(input.as_bytes()) {
        return Cow::Borrowed(input);
    }

    // Fast path 3: no registered tool names — the scrubbers would not
    // match anything regardless of content.
    if known_tool_names.is_empty() {
        return Cow::Borrowed(input);
    }

    // Slow path: run the structural scrubbers. We honour the same order
    // as `sanitize_channel_response` (tag stripping first, then JSON) so
    // that behaviour is identical to the post-hoc pass.
    //
    // CRITICAL: use the `_preserve_whitespace` variants here. The
    // public scrubbers `.trim()` their result so that the *whole*
    // response renders cleanly after the LLM finishes — but on the
    // per-chunk streaming path that trim would eat code-block indents,
    // line-trailing newlines, and incidental spacing inside chunks like
    // `"  2 < 3\n"`. The streaming variants keep the surrounding
    // whitespace exactly as it appeared in the chunk.
    let cleaned_tags = strip_isolated_tool_tag_artifacts_preserve_whitespace(input, known_tool_names);
    let cleaned = strip_isolated_tool_json_artifacts_preserve_whitespace(&cleaned_tags, known_tool_names);

    // If the scrubbers produced byte-identical output, hand back the
    // borrowed input to avoid the allocation downstream.
    if cleaned == input {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(cleaned)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> HashSet<String> {
        list.iter().map(|n| (*n).to_ascii_lowercase()).collect()
    }

    #[test]
    fn pure_text_returns_borrowed() {
        let tools = names(&["shell"]);
        let input = "hello world, this is a plain chunk with no markup.";
        let out = sanitize_stream_chunk(input, &tools);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }

    #[test]
    fn empty_input_returns_borrowed() {
        let tools = names(&["shell"]);
        let out = sanitize_stream_chunk("", &tools);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), "");
    }

    #[test]
    fn cjk_text_is_not_corrupted() {
        // Pure CJK / emoji chunk — must round-trip byte-for-byte and
        // remain a borrowed slice (no multi-byte boundary issues, no
        // allocation).
        let tools = names(&["shell"]);
        let input = "你好，世界！这是一段中文测试。日本語もある。🚀";
        let out = sanitize_stream_chunk(input, &tools);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }

    #[test]
    fn isolated_tool_call_tag_is_stripped() {
        let tools = names(&["shell"]);
        let input = "<tool_call>\n{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}\n</tool_call>";
        let out = sanitize_stream_chunk(input, &tools);
        assert!(matches!(out, Cow::Owned(_)));
        // The scrubber collapses the block to an empty (trimmed) string.
        assert!(out.as_ref().trim().is_empty(), "expected empty, got {out:?}");
    }

    #[test]
    fn isolated_tool_json_is_stripped() {
        let tools = names(&["cron"]);
        let input = "{\"name\":\"cron\",\"parameters\":{\"action\":\"once\",\"message\":\"test\"}}";
        let out = sanitize_stream_chunk(input, &tools);
        assert!(matches!(out, Cow::Owned(_)));
        assert!(out.as_ref().trim().is_empty(), "expected empty, got {out:?}");
    }

    #[test]
    fn unknown_tool_json_is_preserved() {
        // JSON that references a non-registered tool must NOT be stripped.
        let tools = names(&["shell"]);
        let input = "{\"name\":\"profile\",\"parameters\":{\"timezone\":\"UTC\"}}";
        let out = sanitize_stream_chunk(input, &tools);
        // The result should be byte-identical to the input (modulo trim
        // performed by the scrubber). Since the input has no surrounding
        // whitespace, we expect a clean equality.
        assert_eq!(out.as_ref().trim(), input);
    }

    #[test]
    fn empty_tool_set_short_circuits() {
        // No registered tools — even a chunk that looks like a tool
        // call must pass through unchanged via the borrowed fast path.
        let tools: HashSet<String> = HashSet::new();
        let input = "<tool_call>{\"name\":\"shell\"}</tool_call>";
        let out = sanitize_stream_chunk(input, &tools);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }

    #[test]
    fn prefilter_byte_only_returns_borrowed() {
        // Chunk contains `<` but no tool-call structure — the structural
        // scrubber will leave it alone and we must hand back Borrowed.
        let tools = names(&["shell"]);
        let input = "2 < 3 && 3 > 2";
        let out = sanitize_stream_chunk(input, &tools);
        assert_eq!(out.as_ref(), input);
        // Borrowed because the scrubbers produced a byte-identical result.
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    // ── S1-B trim-regression coverage (whitespace preservation) ────────
    //
    // The full-document scrubbers `.trim()` their output so that the
    // final rendered response has no leading/trailing whitespace. On the
    // streaming path, however, each chunk is just a slice of the larger
    // response — trimming it would eat code-block indentation, swallow
    // the newline that separates two assistant lines, and corrupt plain
    // text such as `"  2 < 3\n"`. These tests pin the per-chunk
    // contract: whitespace is preserved unless the chunk *is* an
    // artifact that has to be removed.

    #[test]
    fn chunk_preserves_leading_trailing_whitespace() {
        let tools = names(&["shell"]);
        let input = "  hello world  \n";
        let out = sanitize_stream_chunk(input, &tools);
        // No `<`, `{`, `[` — must short-circuit via the prefilter.
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }

    #[test]
    fn chunk_preserves_code_block_indent() {
        // A streaming chunk containing the first line of an indented
        // code block must keep every leading space. Even though the
        // chunk contains `{`, no valid JSON object starts there so the
        // scrubber leaves it alone.
        let tools = names(&["shell"]);
        let input = "    fn main() {\n";
        let out = sanitize_stream_chunk(input, &tools);
        assert_eq!(out.as_ref(), input, "code-block indent corrupted: {out:?}");
    }

    #[test]
    fn chunk_with_lt_in_normal_text() {
        // Classic Markdown chunk with a `<` operator in arithmetic.
        // The prefilter triggers (because of `<`) but no tool-call tag
        // is found, so the chunk must round-trip with both surrounding
        // spaces and the trailing newline intact.
        let tools = names(&["shell"]);
        let input = " 2 < 3 \n";
        let out = sanitize_stream_chunk(input, &tools);
        assert_eq!(out.as_ref(), input, "whitespace was eaten around lt-operator: {out:?}");
    }

    #[test]
    fn chunk_with_curly_in_normal_text() {
        // The chunk has `{` and `}` but is not a JSON object that
        // references a known tool — sanitisation must be a no-op,
        // including the surrounding whitespace.
        let tools = names(&["shell"]);
        let input = "data = { key: value }";
        let out = sanitize_stream_chunk(input, &tools);
        assert_eq!(out.as_ref(), input, "non-tool curly text was corrupted: {out:?}");
    }

    #[test]
    fn chunk_with_real_tool_call_still_stripped() {
        // The whitespace-preserving variants must not regress the
        // S1-B feature: when a chunk really *is* a tool-call artifact,
        // we still strip the body. The function may legitimately leave
        // behind the surrounding whitespace, but the artifact itself
        // (the tag + payload) must be gone.
        let tools = names(&["shell"]);
        let input = "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool_call>";
        let out = sanitize_stream_chunk(input, &tools);
        assert!(
            !out.as_ref().contains("tool_call") && !out.as_ref().contains("\"shell\""),
            "artifact survived streaming sanitisation: {out:?}"
        );
    }

    #[test]
    fn chunk_with_trailing_newline_preserved() {
        // Specific regression: a chunk that is plain text plus a single
        // trailing `\n` was previously trimmed down to no newline,
        // collapsing two adjacent paragraphs in the rendered output.
        let tools = names(&["shell"]);
        let input = "First paragraph line.\n";
        let out = sanitize_stream_chunk(input, &tools);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
        assert!(out.as_ref().ends_with('\n'), "trailing newline was trimmed");
    }

    #[test]
    fn chunk_with_markdown_link_preserved() {
        // `[` triggers the prefilter but an empty tool set causes the
        // function to short-circuit via Fast path 3, returning Borrowed.
        // The Markdown link must survive completely unchanged.
        let tools: HashSet<String> = HashSet::new();
        let input = "[OpenAI](https://openai.com) is a research lab.";
        let result = sanitize_stream_chunk(input, &tools);
        assert_eq!(result, input);
        assert!(matches!(result, Cow::Borrowed(_)), "应走 Borrowed 短路");
    }

    #[test]
    fn chunk_with_crlf_preserved() {
        // CRLF Windows-style line endings must not be modified.
        // No `<`, `{`, or `[` in the chunk → prefilter short-circuits
        // immediately, returning Borrowed without touching the input.
        let tools: HashSet<String> = HashSet::new();
        let input = "line one\r\nline two\r\n";
        let result = sanitize_stream_chunk(input, &tools);
        assert_eq!(result, input);
        assert!(matches!(result, Cow::Borrowed(_)), "CRLF chunk 应走 Borrowed 短路");
    }
}
