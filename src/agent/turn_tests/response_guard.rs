#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use super::*;
use std::borrow::Cow;

#[test]
fn strip_inline_fabricated_tool_markers_only_touches_the_marker() {
    assert_eq!(
        strip_inline_fabricated_tool_markers("Root is at 41%. [Used tools: shell, session_status]").as_ref(),
        "Root is at 41%."
    );
    assert_eq!(
        strip_inline_fabricated_tool_markers("A [Used tools: shell] B [Used tools: gateway] C").as_ref(),
        "A B C"
    );
    assert_eq!(strip_inline_fabricated_tool_markers("[Used tools: shell]").as_ref(), "");
    for untouched in [
        "Plain answer.",
        "[Used tools:]",
        "[Used tools: ]",
        "Used tools: shell",
        "See [1] and [note: unrelated] plus [Used tools] alone.",
        "[Used tools: shell\nstill going]",
        "[IMAGE:/tmp/a.png] stays",
    ] {
        let result = strip_inline_fabricated_tool_markers(untouched);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "a text without an in-line marker must not allocate: {untouched:?}"
        );
        assert_eq!(result.as_ref(), untouched);
    }
}

/// The assistant-surface cleaner has to see both grains a model writes the
/// marker in, and must leave everything else — other brackets, fenced code,
/// inline code spans — byte-for-byte alone.
#[test]
fn strip_assistant_fabricated_tool_markers_handles_both_grains() {
    assert_eq!(
        strip_assistant_fabricated_tool_markers("Fixture checked. [Used tools: shell]").as_ref(),
        "Fixture checked."
    );
    assert_eq!(
        strip_assistant_fabricated_tool_markers("Done.\n[Used tools: shell]\nMore.").as_ref(),
        "Done.\nMore.",
        "a whole marker line leaves no blank line behind"
    );
    assert_eq!(
        strip_assistant_fabricated_tool_markers("A [Used tools: shell] B\nC [Used tools: gateway] D").as_ref(),
        "A B\nC D"
    );
    assert_eq!(
        strip_assistant_fabricated_tool_markers("Root is at 41%[Used tools: shell].").as_ref(),
        "Root is at 41%.",
        "punctuation after the marker must not gain a stray separator"
    );
    assert!(
        strip_assistant_fabricated_tool_markers("[Used tools: shell]")
            .as_ref()
            .is_empty(),
        "expected an empty value"
    );
}

/// Preservation rules. A marker inside a fenced block or an inline code span
/// is documentation about the syntax — including this project's own docs —
/// not a claim of work, and legitimate bracketed prose was never a marker.
#[test]
fn strip_assistant_fabricated_tool_markers_preserves_code_and_other_brackets() {
    for untouched in [
        "Plain answer.",
        "The runtime never writes `[Used tools: shell]` itself.",
        "Example:\n```text\n[Used tools: shell]\n```\nThat shape is fabricated.",
        "Example:\n~~~\nDone. [Used tools: shell]\n~~~",
        "[Used tools:]",
        "Used tools: shell",
        "See [1] and [note: unrelated] plus [Used tools] alone.",
        "[Used tools: shell\nstill going]",
        "[IMAGE:/tmp/a.png] stays",
    ] {
        let result = strip_assistant_fabricated_tool_markers(untouched);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "text with nothing to strip must not allocate: {untouched:?}"
        );
        assert_eq!(result.as_ref(), untouched);
        assert!(
            !contains_assistant_fabricated_tool_marker(untouched),
            "an exempt marker must not buy a corrective retry either: {untouched:?}"
        );
    }

    assert_eq!(
        strip_assistant_fabricated_tool_markers("Checked. [Used tools: shell] See `[Used tools: x]`.").as_ref(),
        "Checked. See `[Used tools: x]`.",
        "the claim is cut out, the quoted syntax is kept"
    );
}

/// The detector that spends the one corrective retry and the cleaner that
/// runs before delivery must agree on every input, or a turn can be retried
/// for a marker that is then not removed (or delivered with one that never
/// bought a retry).
#[test]
fn contains_assistant_fabricated_tool_marker_agrees_with_the_cleaner() {
    for text in [
        "Fixture checked. [Used tools: shell]",
        "[Used tools: shell]",
        "Done.\n[Used tools: shell]\nMore.",
        "Plain answer.",
        "The runtime never writes `[Used tools: shell]` itself.",
        "Example:\n```\n[Used tools: shell]\n```",
        "[Used tools:]",
        "See [1] and [note: unrelated].",
    ] {
        assert_eq!(
            contains_assistant_fabricated_tool_marker(text),
            strip_assistant_fabricated_tool_markers(text).as_ref() != text,
            "detector and cleaner disagree on {text:?}"
        );
    }
}

/// The CommonMark fence model the cleaner and the detector share, on the
/// shapes a single `in_fenced_block` boolean gets wrong.
///
/// Each fixture is `(name, input, expected)`; `expected == input` means the
/// text must come back byte-for-byte and must therefore not buy a
/// corrective retry either. The first two inputs are the 2026-09-19
/// cross-audit probe's own cases, replayed verbatim, and pulldown-cmark
/// parses both the way these expectations describe.
///
/// MUTATION GUARD: drop the character/length check in `CodeFence::closes`
/// (accept any fence line as a closer again) and the mixed-fence, long-fence,
/// longer-closer, info-string-closer and CRLF fixtures all fail.
#[test]
fn the_shared_fence_model_follows_commonmark_closing_rules() {
    let fixtures: &[(&str, &str, &str)] = &[
        (
            "plain control",
            "Fixture checked. [Used tools: shell]",
            "Fixture checked.",
        ),
        (
            "mixed fence: a backtick line inside a tilde block is not a closer",
            "~~~text\n```example\n~~~\nFixture checked. [Used tools: shell]",
            "~~~text\n```example\n~~~\nFixture checked.",
        ),
        (
            "long fence: a shorter run inside a longer block is not a closer",
            "````text\n```\nDocumented [Used tools: shell]\n````",
            "````text\n```\nDocumented [Used tools: shell]\n````",
        ),
        (
            "fence-looking content inside a longer tilde fence",
            "~~~~\n~~~\n[Used tools: shell]\n~~~~",
            "~~~~\n~~~\n[Used tools: shell]\n~~~~",
        ),
        (
            "a closer may be longer than its opener",
            "```text\n[Used tools: shell]\n`````\nDone. [Used tools: shell]",
            "```text\n[Used tools: shell]\n`````\nDone.",
        ),
        (
            "a closing fence may not carry an info string",
            "```\n[Used tools: shell]\n```rust\nStill code. [Used tools: shell]\n```",
            "```\n[Used tools: shell]\n```rust\nStill code. [Used tools: shell]\n```",
        ),
        (
            "an opening backtick fence's info string may not contain a backtick",
            "```rust `x`\nNot code. [Used tools: shell]",
            "```rust `x`\nNot code.",
        ),
        (
            "inline code spans still document the syntax",
            "Checked. [Used tools: shell] See `[Used tools: x]`.",
            "Checked. See `[Used tools: x]`.",
        ),
        (
            "a backslash-escaped fence does not open a block",
            "\\```text\n[Used tools: shell]\nDone.",
            "\\```text\nDone.",
        ),
        (
            "three spaces of indent still open and close a fence",
            "   ```text\n   [Used tools: shell]\n   ```\nDone.",
            "   ```text\n   [Used tools: shell]\n   ```\nDone.",
        ),
        (
            "an unterminated fence runs to the end of the text",
            "```text\n[Used tools: shell]\nDocumented [Used tools: shell]",
            "```text\n[Used tools: shell]\nDocumented [Used tools: shell]",
        ),
        (
            "CRLF input walks the same fences LF input does",
            "~~~text\r\n```example\r\n~~~\r\nFixture checked. [Used tools: shell]",
            "~~~text\n```example\n~~~\nFixture checked.",
        ),
    ];

    for (name, input, expected) in fixtures {
        let cleaned = strip_assistant_fabricated_tool_markers(input);
        assert_eq!(cleaned.as_ref(), *expected, "cleaner disagrees on {name}");
        assert_eq!(
            contains_assistant_fabricated_tool_marker(input),
            cleaned.as_ref() != *input,
            "detector and cleaner disagree on {name}"
        );
        if cleaned.as_ref() == *input {
            assert!(
                matches!(cleaned, Cow::Borrowed(_)),
                "text that is not stripped must stay borrowed on {name}"
            );
        }
    }
}
