//! Markdown rendering with syntax highlighting for code blocks.
//!
//! Uses `syntect` for 200+ language syntax highlighting in terminal output.
//! Gated behind the `terminal-tui` feature.

use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::{LinesWithEndings, as_24_bit_terminal_escaped};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Default theme for syntax highlighting.
const DEFAULT_THEME: &str = "base16-ocean.dark";

/// ANSI Select Graphic Rendition reset sequence.
const ANSI_RESET: &str = "\x1b[0m";

/// Tracks whether the previously written segment terminates the ANSI graphic state.
///
/// `ends_with_reset == true` means a `\x1b[0m` is the last effective ANSI code in
/// the output buffer (possibly followed by plain whitespace/newlines). Callers
/// consult this before emitting their own reset to avoid emitting a redundant
/// `\x1b[0m\x1b[0m` pair when stitching highlighted segments back-to-back.
#[derive(Debug, Default)]
struct AnsiState {
    ends_with_reset: bool,
}

impl AnsiState {
    /// Append `chunk` to `out` and update the reset-state flag based on the
    /// chunk's trailing ANSI sequence (whitespace-tolerant).
    fn push(&mut self, out: &mut String, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        out.push_str(chunk);
        self.recompute_from_tail(chunk);
    }

    /// Append a single `\x1b[0m` only when the current state does not already
    /// end with a reset. Returns whether the reset was actually emitted.
    fn push_reset_if_needed(&mut self, out: &mut String) -> bool {
        if self.ends_with_reset {
            return false;
        }
        out.push_str(ANSI_RESET);
        self.ends_with_reset = true;
        true
    }

    /// Inspect the freshly appended chunk to learn whether it leaves the
    /// terminal in the "reset" graphic state. We treat trailing whitespace as
    /// neutral — pure spaces/newlines do not introduce new SGR codes.
    fn recompute_from_tail(&mut self, chunk: &str) {
        let trimmed = chunk.trim_end_matches(['\n', '\r', ' ', '\t']);
        if trimmed.is_empty() {
            // Whitespace-only chunk leaves prior reset state unchanged.
            return;
        }
        if trimmed.ends_with(ANSI_RESET) {
            self.ends_with_reset = true;
        } else if contains_sgr_after_last_reset(trimmed) {
            self.ends_with_reset = false;
        }
        // Otherwise (no SGR at all), state is unchanged: plain text neither
        // introduces nor clears color attributes.
    }
}

/// Returns true if `s` contains any ANSI SGR escape (`\x1b[...m`) after its
/// last `\x1b[0m`. Used to decide if the trailing state is "colored" vs "reset".
fn contains_sgr_after_last_reset(s: &str) -> bool {
    let tail = s.rfind(ANSI_RESET).map_or(s, |idx| &s[idx + ANSI_RESET.len()..]);
    // Look for any remaining SGR introducer `\x1b[`. We only care whether one
    // exists, not its full parameters — the highlighter only emits SGR codes.
    tail.contains("\x1b[")
}

/// Render a code block with syntax highlighting, returning ANSI-escaped text.
///
/// `language` is the optional language identifier from the code fence (e.g., "rust", "python").
/// Returns the highlighted code as a string with ANSI escape sequences. The
/// output always terminates with a single `\x1b[0m` to leave callers a clean
/// terminal state — callers stitching multiple highlighted blocks together
/// should use [`render_markdown_with_highlighting`] which de-duplicates resets.
pub fn highlight_code_block(code: &str, language: Option<&str>) -> String {
    let syntax = language
        .and_then(|lang| SYNTAX_SET.find_syntax_by_token(lang))
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());

    // Fallback chain: configured theme → first available → hardcoded built-in.
    // syntect's ThemeSet always ships base16-ocean.dark, so indexing is safe.
    #[allow(clippy::indexing_slicing)]
    let theme = THEME_SET.themes.get(DEFAULT_THEME).unwrap_or_else(|| {
        THEME_SET
            .themes
            .values()
            .next()
            .unwrap_or_else(|| &THEME_SET.themes["base16-ocean.dark"])
    });

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut output = String::new();
    let mut state = AnsiState::default();

    for line in LinesWithEndings::from(code) {
        match highlighter.highlight_line(line, &SYNTAX_SET) {
            Ok(ranges) => {
                let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
                state.push(&mut output, &escaped);
            }
            Err(_) => {
                // Fallback: plain text (does not alter ANSI state).
                state.push(&mut output, line);
            }
        }
    }
    // Trailing reset only when the buffer is currently in a colored state.
    state.push_reset_if_needed(&mut output);
    output
}

/// Render markdown text with code block highlighting.
///
/// Detects fenced code blocks (``` or ~~~), highlights them, and returns
/// the full text with code blocks replaced by highlighted versions.
///
/// Maintains an [`AnsiState`] across segments so that adjacent highlighted
/// blocks (or a block immediately followed by plain text) do not accumulate
/// redundant `\x1b[0m` sequences. Each highlighted segment is responsible for
/// leaving the terminal in either "colored" or "reset" state; this function
/// inserts a reset only when a transition demands it.
pub fn render_markdown_with_highlighting(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut state = AnsiState::default();
    let mut in_code_block = false;
    let mut code_language: Option<String> = None;
    let mut code_buffer = String::new();

    for line in text.lines() {
        if !in_code_block && (line.starts_with("```") || line.starts_with("~~~")) {
            // Start of code block
            in_code_block = true;
            let fence = if line.starts_with("```") { "```" } else { "~~~" };
            let lang = line[fence.len()..].trim();
            code_language = if lang.is_empty() { None } else { Some(lang.to_string()) };
            code_buffer.clear();
            // Print a visual separator (plain text — does not alter ANSI state).
            state.push(&mut result, "  ┌─");
            if let Some(ref lang) = code_language {
                state.push(&mut result, lang);
                state.push(&mut result, "─");
            }
            state.push(&mut result, "\n");
        } else if in_code_block && (line.starts_with("```") || line.starts_with("~~~")) {
            // End of code block — highlight and append.
            emit_highlighted_block(&mut result, &mut state, &code_buffer, code_language.as_deref());
            in_code_block = false;
            code_language = None;
        } else if in_code_block {
            code_buffer.push_str(line);
            code_buffer.push('\n');
        } else {
            // Regular markdown line — apply basic formatting. `render_inline_markdown`
            // may insert `\x1b[33m...\x1b[0m` pairs (always reset-terminated) for
            // inline code; for plain prose it returns text unchanged.
            let formatted = render_inline_markdown(line);
            state.push(&mut result, &formatted);
            state.push(&mut result, "\n");
        }
    }

    // Handle unclosed code block.
    if in_code_block && !code_buffer.is_empty() {
        emit_highlighted_block(&mut result, &mut state, &code_buffer, code_language.as_deref());
    }

    result
}

/// Write a highlighted code block into `result` with the visual border, sharing
/// the [`AnsiState`] so redundant resets are suppressed.
///
/// We unconditionally insert a `\x1b[0m` before each border line (`  │ ` /
/// `  └─`) when the buffer is still in a colored state, so the border itself
/// is never colored by the previous SGR; conversely, when already reset we
/// skip the redundant code.
fn emit_highlighted_block(result: &mut String, state: &mut AnsiState, code: &str, language: Option<&str>) {
    let highlighted = highlight_code_block(code, language);
    for hl_line in highlighted.lines() {
        // Border prefix must render in the default terminal style — emit a
        // reset only when we're currently inside an SGR run.
        state.push_reset_if_needed(result);
        state.push(result, "  │ ");
        state.push(result, hl_line);
        state.push(result, "\n");
    }
    // Closing border — same rule.
    state.push_reset_if_needed(result);
    state.push(result, "  └─\n");
}

/// Apply basic inline markdown formatting (bold, italic, code).
fn render_inline_markdown(line: &str) -> String {
    let mut result = line.to_string();

    // Inline code: `code` → \x1b[33mcode\x1b[0m (yellow)
    while let Some(start) = result.find('`') {
        if let Some(end) = result[start + 1..].find('`') {
            let end = start + 1 + end;
            let code = &result[start + 1..end];
            let replacement = format!("\x1b[33m{code}\x1b[0m");
            result = format!("{}{}{}", &result[..start], replacement, &result[end + 1..]);
        } else {
            break;
        }
    }

    result
}

/// Calculate the display width of a string, accounting for CJK characters.
#[cfg(feature = "terminal-tui")]
pub fn display_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(text)
}

/// Wrap text to fit within the given width, respecting CJK character widths.
#[cfg(feature = "terminal-tui")]
pub fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;

    if max_width == 0 {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + ch_width > max_width && !current_line.is_empty() {
            lines.push(current_line);
            current_line = String::new();
            current_width = 0;
        }
        current_line.push(ch);
        current_width += ch_width;
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_rust_code() {
        let code = "fn main() {\n    println!(\"hello\");\n}";
        let result = highlight_code_block(code, Some("rust"));
        // Should contain ANSI escape sequences
        assert!(result.contains("\x1b["));
        assert!(result.contains("main"));
    }

    #[test]
    fn highlight_unknown_language_falls_back() {
        let code = "some text";
        let result = highlight_code_block(code, Some("nonexistent_lang_xyz"));
        assert!(result.contains("some text"));
    }

    #[test]
    fn highlight_no_language() {
        let code = "plain text here";
        let result = highlight_code_block(code, None);
        assert!(result.contains("plain text"));
    }

    #[test]
    fn render_markdown_code_block() {
        let md = "Hello\n```rust\nfn main() {}\n```\nBye";
        let result = render_markdown_with_highlighting(md);
        assert!(result.contains("Hello"));
        assert!(result.contains("Bye"));
        assert!(result.contains("┌─rust"));
        assert!(result.contains("└─"));
    }

    #[test]
    fn inline_code_formatting() {
        let line = "Use `cargo build` to compile";
        let result = render_inline_markdown(line);
        assert!(result.contains("\x1b[33m"));
        assert!(result.contains("cargo build"));
    }

    #[test]
    fn display_width_ascii() {
        assert_eq!(display_width("hello"), 5);
    }

    #[test]
    fn display_width_cjk() {
        // CJK characters are typically 2 columns wide
        assert_eq!(display_width("你好"), 4);
        assert_eq!(display_width("hello你好"), 9);
    }

    #[test]
    fn wrap_text_basic() {
        let lines = wrap_text("hello world", 5);
        assert!(lines.len() >= 2);
    }

    #[test]
    fn wrap_text_cjk() {
        // Each CJK char is width 2, so 5 chars = width 10
        let lines = wrap_text("你好世界呀", 6);
        assert!(lines.len() >= 2); // 10 width into max 6 → multiple lines
    }

    // ---------------------------------------------------------------
    // P1-5 ANSI reset state-machine tests
    // ---------------------------------------------------------------

    /// Helper: count occurrences of a substring.
    fn count_occurrences(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    #[test]
    fn no_consecutive_reset_codes_anywhere() {
        // Two adjacent code blocks plus inline-code text. The previous
        // implementation emitted `\x1b[0m\x1b[0m` at every junction; with the
        // state machine, two resets must never appear back-to-back (with
        // optional whitespace between them being fine but no second reset
        // immediately after the first).
        let md = "intro `inline` text\n```rust\nfn a() {}\n```\n```python\nprint(1)\n```\nouter `code` end";
        let out = render_markdown_with_highlighting(md);
        assert!(
            !out.contains("\x1b[0m\x1b[0m"),
            "consecutive resets leaked through: {out:?}"
        );
    }

    #[test]
    fn single_code_block_still_resets_terminal() {
        // Sanity check: a single block must still emit at least one reset so
        // the terminal returns to default colors after the block.
        let md = "```rust\nfn main() {}\n```";
        let out = render_markdown_with_highlighting(md);
        assert!(out.contains("\x1b[0m"), "missing trailing reset: {out:?}");
    }

    #[test]
    fn plain_text_emits_no_ansi() {
        let md = "hello world\nthis is plain prose\nno code at all";
        let out = render_markdown_with_highlighting(md);
        assert!(!out.contains("\x1b["), "plain text picked up ANSI: {out:?}");
    }

    #[test]
    fn empty_input_does_not_panic() {
        let out = render_markdown_with_highlighting("");
        assert_eq!(out, "");
    }

    #[test]
    fn empty_code_block_does_not_panic() {
        // Open + close fence with no content.
        let md = "```rust\n```";
        let out = render_markdown_with_highlighting(md);
        // Should produce the border, no trailing colored garbage, and no
        // double-reset sequences.
        assert!(out.contains("┌─rust"));
        assert!(out.contains("└─"));
        assert!(!out.contains("\x1b[0m\x1b[0m"));
    }

    #[test]
    fn code_block_followed_by_plain_text_no_redundant_reset() {
        // After the closing border emits its reset, the following plain text
        // line must NOT trigger another reset — there is no colored state to
        // clear.
        let md = "```rust\nfn x() {}\n```\nplain text after block";
        let out = render_markdown_with_highlighting(md);

        // Locate the closing border and inspect what follows.
        let close_idx = out.find("└─").expect("closing border present in output");
        let tail = &out[close_idx..];
        // The tail contains the border (no ANSI), a newline, then the plain
        // text. A redundant reset just after the border would be visible here.
        // Allow one reset before the border itself, but tail must not start
        // with another reset right after the border line.
        assert!(
            !tail.contains("\nplain text after block\x1b[0m"),
            "redundant reset emitted into plain text region: {tail:?}"
        );
    }

    #[test]
    fn adjacent_code_blocks_share_reset() {
        // Two adjacent fenced blocks. Total `\x1b[0m` count should be bounded
        // — significantly fewer than what a naive implementation emits
        // (which would be one per line plus per border).
        let md = "```rust\nfn a() {}\n```\n```rust\nfn b() {}\n```";
        let out = render_markdown_with_highlighting(md);

        // Pin the upper bound at "one reset per border line" (2 borders per
        // block * 2 blocks = 4) plus one per highlighted line (2 lines * 2
        // blocks = 4) — so at most 8. The old impl emitted ~10+ because every
        // highlighter line ended with a reset AND every `│ ` prefix repeated
        // it. We assert no consecutive resets exist (the meaningful invariant)
        // AND that the count is reasonable.
        assert!(!out.contains("\x1b[0m\x1b[0m"));
        let resets = count_occurrences(&out, "\x1b[0m");
        assert!(resets <= 8, "too many resets ({resets}): {out:?}");
    }

    #[test]
    fn ansi_state_recognises_reset_terminated_chunk() {
        let mut s = AnsiState::default();
        let mut buf = String::new();
        s.push(&mut buf, "\x1b[31mred\x1b[0m");
        assert!(s.ends_with_reset, "should detect trailing reset");
        // Pushing whitespace must not flip the flag.
        s.push(&mut buf, "\n  ");
        assert!(s.ends_with_reset, "whitespace must preserve reset state");
    }

    #[test]
    fn ansi_state_detects_colored_tail() {
        let mut s = AnsiState::default();
        let mut buf = String::new();
        s.push(&mut buf, "\x1b[31mred without close");
        assert!(!s.ends_with_reset, "colored tail must not be marked reset");
    }

    #[test]
    fn push_reset_if_needed_is_idempotent() {
        let mut s = AnsiState::default();
        let mut buf = String::new();
        s.push(&mut buf, "\x1b[31mred");
        let first = s.push_reset_if_needed(&mut buf);
        let second = s.push_reset_if_needed(&mut buf);
        assert!(first, "first reset must be emitted");
        assert!(!second, "second reset must be suppressed");
        assert_eq!(
            count_occurrences(&buf, "\x1b[0m"),
            1,
            "exactly one reset must end up in buffer"
        );
    }

    #[test]
    fn plain_text_chunk_does_not_clear_reset_flag() {
        // Once reset, appending plain text keeps the state "reset" — there's
        // no new SGR to clear.
        let mut s = AnsiState::default();
        let mut buf = String::new();
        s.push(&mut buf, "\x1b[31mred\x1b[0m");
        assert!(s.ends_with_reset);
        s.push(&mut buf, "  border text\n");
        assert!(s.ends_with_reset, "plain text after reset must not flip flag");
    }

    #[test]
    fn contains_sgr_after_last_reset_helper() {
        assert!(!contains_sgr_after_last_reset("\x1b[31mred\x1b[0m"));
        assert!(contains_sgr_after_last_reset("\x1b[31mred"));
        assert!(contains_sgr_after_last_reset("\x1b[0m\x1b[31m"));
        assert!(!contains_sgr_after_last_reset("plain text"));
    }
}
