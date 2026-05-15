//! Markdown rendering with syntax highlighting for code blocks.
//!
//! Uses `syntect` for 200+ language syntax highlighting in terminal output.
//! Gated behind the `terminal-tui` feature.

use std::borrow::Cow;
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

// --- Unified diff ANSI palette ------------------------------------------------
// Pure foreground colors keep contrast high without the harshness of full-line
// background fills; matches the rendering used by `codex` and most terminal
// pagers (less +F, git --color=always).
/// Bold cyan — hunk headers (`@@ -a,b +c,d @@`).
const ANSI_DIFF_HUNK: &str = "\x1b[1;36m";
/// Bold white — file header lines (`--- a/path`, `+++ b/path`, `diff --git ...`).
const ANSI_DIFF_FILE_HEADER: &str = "\x1b[1;37m";
/// Green — additions (`+` lines that are not the `+++` file header).
const ANSI_DIFF_ADD: &str = "\x1b[32m";
/// Red — deletions (`-` lines that are not the `---` file header).
const ANSI_DIFF_DEL: &str = "\x1b[31m";

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

/// Returns `true` when the given language tag denotes a unified-diff payload.
///
/// We accept the canonical `diff`, the broader `patch` (used by `git
/// format-patch`-style fences), and the explicit `unified-diff` synonym. Case
/// is normalised because LLMs frequently emit `Diff` or `DIFF`.
fn is_diff_language(lang: &str) -> bool {
    matches!(
        lang.to_ascii_lowercase().as_str(),
        "diff" | "patch" | "unified-diff" | "udiff"
    )
}

/// Returns `true` when the line is a unified-diff hunk header:
/// `@@ -<n>[,<m>] +<n>[,<m>] @@[ optional context]`.
///
/// Matched without regex to avoid pulling a dependency for a single shape:
/// the prefix `@@ -` followed by digits, optional `,digits`, ` +`, digits,
/// optional `,digits`, then ` @@`. Anything after `@@` (function context) is
/// allowed.
fn is_hunk_header(line: &str) -> bool {
    let rest = match line.strip_prefix("@@ -") {
        Some(r) => r,
        None => return false,
    };
    // Parse first number
    let (consumed, rest) = take_digits(rest);
    if consumed == 0 {
        return false;
    }
    // Optional ,digits
    let rest = if let Some(r) = rest.strip_prefix(',') {
        let (n, r) = take_digits(r);
        if n == 0 {
            return false;
        }
        r
    } else {
        rest
    };
    let rest = match rest.strip_prefix(" +") {
        Some(r) => r,
        None => return false,
    };
    let (consumed, rest) = take_digits(rest);
    if consumed == 0 {
        return false;
    }
    let rest = if let Some(r) = rest.strip_prefix(',') {
        let (n, r) = take_digits(r);
        if n == 0 {
            return false;
        }
        r
    } else {
        rest
    };
    rest.starts_with(" @@")
}

/// Helper for [`is_hunk_header`]: returns the count of leading ASCII digits and
/// the remainder of the input.
fn take_digits(s: &str) -> (usize, &str) {
    let end = s.bytes().take_while(u8::is_ascii_digit).count();
    (end, &s[end..])
}

/// Heuristically detect whether a buffer of text is a unified diff.
///
/// Triggers when ANY of:
/// - the explicit language tag is one of [`is_diff_language`]
/// - the first non-empty line starts with `diff --git ` or `--- ` (file header)
/// - the buffer contains at least one [`is_hunk_header`]
///
/// We intentionally do NOT trigger on the mere presence of `+`/`-` prefixes:
/// regular markdown lists (`- item`, `+ item`) would falsely match.
fn is_diff_block(buffer: &str, language: Option<&str>) -> bool {
    if let Some(lang) = language
        && is_diff_language(lang)
    {
        return true;
    }
    let mut saw_hunk = false;
    let mut first_nonempty: Option<&str> = None;
    for line in buffer.lines() {
        if first_nonempty.is_none() && !line.is_empty() {
            first_nonempty = Some(line);
        }
        if is_hunk_header(line) {
            saw_hunk = true;
            break;
        }
    }
    if saw_hunk {
        return true;
    }
    matches!(
        first_nonempty,
        Some(l) if l.starts_with("diff --git ") || l.starts_with("--- a/") || l.starts_with("--- /")
    )
}

/// Render a unified diff with per-line ANSI colouring.
///
/// Colour rules (foreground only; backgrounds reserved for selection in the
/// host terminal):
/// - `@@ ... @@` hunk headers — bold cyan
/// - `--- ` / `+++ ` / `diff --git ` file headers — bold white
/// - `+` additions (not `+++`) — green
/// - `-` deletions (not `---`) — red
/// - context and anything else — default terminal colour (no SGR emitted)
///
/// Each coloured line is closed with [`ANSI_RESET`] so the trailing newline
/// renders in the default style. Lines without any SGR are emitted verbatim so
/// callers tracking [`AnsiState`] see clean transitions.
pub fn render_diff_block(diff: &str) -> String {
    let mut out = String::with_capacity(diff.len() + 64);
    let mut state = AnsiState::default();

    for line in LinesWithEndings::from(diff) {
        // Split the line into content + trailing newline(s) so the reset is
        // emitted INSIDE the visible line (before \n), not after — otherwise
        // the newline itself can be coloured on some terminals.
        let (content, eol) = split_eol(line);
        let prefix = classify_diff_line(content);
        match prefix {
            DiffLineKind::Hunk => {
                state.push(&mut out, ANSI_DIFF_HUNK);
                state.push(&mut out, content);
                state.push_reset_if_needed(&mut out);
            }
            DiffLineKind::FileHeader => {
                state.push(&mut out, ANSI_DIFF_FILE_HEADER);
                state.push(&mut out, content);
                state.push_reset_if_needed(&mut out);
            }
            DiffLineKind::Add => {
                state.push(&mut out, ANSI_DIFF_ADD);
                state.push(&mut out, content);
                state.push_reset_if_needed(&mut out);
            }
            DiffLineKind::Del => {
                state.push(&mut out, ANSI_DIFF_DEL);
                state.push(&mut out, content);
                state.push_reset_if_needed(&mut out);
            }
            DiffLineKind::Context => {
                // Plain text — no SGR, AnsiState stays in whatever reset
                // state it was previously in.
                state.push(&mut out, content);
            }
        }
        if !eol.is_empty() {
            state.push(&mut out, eol);
        }
    }
    // Belt-and-suspenders: callers stitch our output back into a larger buffer
    // via the shared AnsiState, so leaving a non-reset trailing state would
    // leak colour. push_reset_if_needed is a no-op when already reset.
    state.push_reset_if_needed(&mut out);
    out
}

/// Classification of a single line in a unified diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffLineKind {
    Hunk,
    FileHeader,
    Add,
    Del,
    Context,
}

/// Categorise `line` (content only, no trailing EOL) into a [`DiffLineKind`].
///
/// Order matters: the `+++` / `---` file headers must be checked BEFORE the
/// generic `+` / `-` add/delete prefixes, otherwise the file-name lines would
/// render as additions/deletions.
fn classify_diff_line(line: &str) -> DiffLineKind {
    if is_hunk_header(line) {
        return DiffLineKind::Hunk;
    }
    if line.starts_with("+++ ") || line.starts_with("--- ") || line.starts_with("diff --git ") {
        return DiffLineKind::FileHeader;
    }
    // Order: check the two-char `+++`/`---` first via the FileHeader branch
    // above; here a leading `+`/`-` is an add/delete.
    if let Some(b) = line.as_bytes().first() {
        match b {
            b'+' => return DiffLineKind::Add,
            b'-' => return DiffLineKind::Del,
            _ => {}
        }
    }
    DiffLineKind::Context
}

/// Split `line` into `(content, eol)` where `eol` is `\n`, `\r\n`, or empty.
fn split_eol(line: &str) -> (&str, &str) {
    line.strip_suffix("\r\n").map_or_else(
        || line.strip_suffix('\n').map_or((line, ""), |stripped| (stripped, "\n")),
        |stripped| (stripped, "\r\n"),
    )
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
    // Unified diff blocks bypass syntect (its `diff` syntax does highlight,
    // but on a dark base16 theme `+`/`-` end up in muted shades). We use a
    // dedicated line-level renderer that matches the convention used by
    // `git diff --color` and codex.
    let highlighted = if is_diff_block(code, language) {
        render_diff_block(code)
    } else {
        highlight_code_block(code, language)
    };
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
///
/// Returns `Cow::Borrowed(line)` when no inline code is found, avoiding an
/// allocation for plain-prose lines. When backtick pairs are found the string
/// is rebuilt with `\x1b[33m…\x1b[0m` wrapping (always reset-terminated so
/// the shared [`AnsiState`] in the caller can track the trailing reset via
/// `recompute_from_tail`).
fn render_inline_markdown(line: &str) -> Cow<'_, str> {
    // Fast path: no backtick ⇒ borrow the original slice, zero allocation.
    if !line.contains('`') {
        return Cow::Borrowed(line);
    }

    let mut result = line.to_owned();

    // Inline code: `code` → \x1b[33mcode\x1b[0m (yellow)
    while let Some(start) = result.find('`') {
        if let Some(rel_end) = result[start + 1..].find('`') {
            let end = start + 1 + rel_end;
            let code = result[start + 1..end].to_owned();
            let replacement = format!("\x1b[33m{code}\x1b[0m");
            result = format!("{}{}{}", &result[..start], replacement, &result[end + 1..]);
        } else {
            break;
        }
    }

    Cow::Owned(result)
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

    // ---------------------------------------------------------------
    // P2-9 Unified diff renderer tests
    // ---------------------------------------------------------------

    #[test]
    fn hunk_header_recognised() {
        assert!(is_hunk_header("@@ -1,4 +1,5 @@"));
        assert!(is_hunk_header("@@ -10 +12 @@"));
        assert!(is_hunk_header("@@ -1,4 +1,5 @@ fn main() {"));
        assert!(!is_hunk_header("@@ broken"));
        assert!(!is_hunk_header("@ -1 +1 @"));
        assert!(!is_hunk_header(""));
        assert!(!is_hunk_header("plain @@ text"));
    }

    #[test]
    fn diff_language_alias_detection() {
        assert!(is_diff_language("diff"));
        assert!(is_diff_language("DIFF"));
        assert!(is_diff_language("Patch"));
        assert!(is_diff_language("unified-diff"));
        assert!(is_diff_language("udiff"));
        assert!(!is_diff_language("rust"));
        assert!(!is_diff_language(""));
    }

    #[test]
    fn is_diff_block_triggers_on_lang_tag() {
        let buf = "no hunks here\njust text";
        assert!(is_diff_block(buf, Some("diff")));
        assert!(is_diff_block(buf, Some("patch")));
    }

    #[test]
    fn is_diff_block_triggers_on_hunk_header() {
        let buf = "some context\n@@ -1,2 +1,3 @@\n+added\n";
        assert!(is_diff_block(buf, None));
    }

    #[test]
    fn is_diff_block_triggers_on_file_header() {
        assert!(is_diff_block("--- a/foo.rs\n+++ b/foo.rs\n", None));
        assert!(is_diff_block("diff --git a/x b/x\n", None));
    }

    #[test]
    fn is_diff_block_rejects_plain_text_with_dashes() {
        // Markdown list items must NOT be confused with diff lines.
        let buf = "- item one\n- item two\n+ another";
        assert!(!is_diff_block(buf, None));
        assert!(!is_diff_block(buf, Some("markdown")));
    }

    #[test]
    fn render_diff_block_colours_each_line_kind() {
        let diff = "\
--- a/foo.rs
+++ b/foo.rs
@@ -1,3 +1,4 @@
 context line
-old line
+new line
";
        let out = render_diff_block(diff);
        // Hunk header colour
        assert!(out.contains(ANSI_DIFF_HUNK), "hunk header should be cyan");
        assert!(out.contains("@@ -1,3 +1,4 @@"));
        // File header colour
        assert!(out.contains(ANSI_DIFF_FILE_HEADER), "file headers should be bold white");
        // Add / Del colours
        assert!(out.contains(ANSI_DIFF_ADD), "+ lines should be green");
        assert!(out.contains(ANSI_DIFF_DEL), "- lines should be red");
        // Context line stays uncoloured: no SGR introducer touches it.
        // We verify by checking the substring " context line\n" appears
        // directly (the leading space is the context marker).
        assert!(out.contains(" context line"));
        // Output must end cleanly (no dangling colour).
        assert!(
            out.ends_with(ANSI_RESET) || !out.contains(ANSI_DIFF_ADD) || {
                // The last non-empty colour run must be followed by a reset.
                let trimmed = out.trim_end_matches('\n');
                trimmed.ends_with(ANSI_RESET)
            }
        );
    }

    #[test]
    fn render_diff_block_file_header_takes_precedence_over_minus() {
        let diff = "--- a/file.rs\n-deleted\n";
        let out = render_diff_block(diff);
        // The `--- a/...` line must be coloured as a file header (bold white),
        // not as a deletion (red).
        let header_pos = out.find("--- a/file.rs").expect("file header text must appear");
        // Look backwards from header_pos for the most recent ANSI sequence.
        let prefix = &out[..header_pos];
        let last_sgr_start = prefix.rfind("\x1b[").expect("colour applied to header");
        let last_sgr = &prefix[last_sgr_start..];
        assert!(
            last_sgr.starts_with(ANSI_DIFF_FILE_HEADER),
            "file header should use bold-white SGR, got {last_sgr:?}"
        );
    }

    #[test]
    fn render_diff_block_empty_input_no_panic() {
        let out = render_diff_block("");
        // Mirrors `highlight_code_block("")`: the trailing reset is emitted by
        // `AnsiState::push_reset_if_needed` because the default state is
        // "needs reset". Acceptable values are `""` or a single bare reset.
        assert!(out.is_empty() || out == ANSI_RESET);
        // Critically: must not panic and must not contain colour codes.
        assert!(!out.contains(ANSI_DIFF_ADD));
        assert!(!out.contains(ANSI_DIFF_DEL));
        assert!(!out.contains(ANSI_DIFF_HUNK));
    }

    #[test]
    fn render_diff_block_handles_crlf_line_endings() {
        let diff = "@@ -1 +1 @@\r\n+added\r\n";
        let out = render_diff_block(diff);
        assert!(out.contains("@@ -1 +1 @@"));
        assert!(out.contains("+added"));
        // CRLF preserved
        assert!(out.contains("\r\n"));
    }

    #[test]
    fn markdown_fenced_diff_block_uses_diff_renderer() {
        // The `diff` language tag must route to render_diff_block, NOT syntect.
        // syntect's diff syntax produces different SGR codes (24-bit base16
        // theme), whereas our renderer uses the basic palette `\x1b[1;36m`
        // etc. Presence of `\x1b[1;36m` (hunk header bold cyan) confirms the
        // dispatch.
        let md = "before\n```diff\n@@ -1 +1 @@\n+new\n-old\n```\nafter";
        let out = render_markdown_with_highlighting(md);
        assert!(out.contains(ANSI_DIFF_HUNK), "diff renderer should colour hunk header");
        assert!(out.contains(ANSI_DIFF_ADD));
        assert!(out.contains(ANSI_DIFF_DEL));
    }

    #[test]
    fn fenced_block_without_diff_lang_and_no_hunks_does_not_route_to_diff() {
        // A markdown list inside a fenced block should NOT be mis-detected as
        // a diff just because it has `-` prefixes.
        let md = "```\n- item\n+ plus\n```";
        let out = render_markdown_with_highlighting(md);
        // None of the diff-specific colours should appear.
        assert!(!out.contains(ANSI_DIFF_ADD));
        assert!(!out.contains(ANSI_DIFF_DEL));
        assert!(!out.contains(ANSI_DIFF_HUNK));
    }

    #[test]
    fn fenced_block_with_hunk_header_auto_detected_even_without_lang() {
        let md = "```\n@@ -1,2 +1,3 @@\n+x\n-y\n```";
        let out = render_markdown_with_highlighting(md);
        assert!(out.contains(ANSI_DIFF_HUNK));
        assert!(out.contains(ANSI_DIFF_ADD));
        assert!(out.contains(ANSI_DIFF_DEL));
    }

    #[test]
    fn adjacent_diff_blocks_share_reset() {
        // Two consecutive diff fences must not produce \x1b[0m\x1b[0m at the
        // junction — the AnsiState shared with the surrounding pipeline
        // suppresses redundant resets.
        let md = "```diff\n@@ -1 +1 @@\n+a\n```\n```diff\n@@ -1 +1 @@\n+b\n```";
        let out = render_markdown_with_highlighting(md);
        assert!(
            !out.contains("\x1b[0m\x1b[0m"),
            "redundant double-reset between diff blocks: {out:?}"
        );
    }

    #[test]
    fn diff_block_followed_by_plain_text_no_redundant_reset() {
        // A diff block borders should still leave terminal in clean state and
        // not insert a stray reset into the plain text after.
        let md = "```diff\n@@ -1 +1 @@\n+x\n```\nplain trailing line";
        let out = render_markdown_with_highlighting(md);
        assert!(!out.contains("\x1b[0m\x1b[0m"));
        assert!(out.contains("plain trailing line"));
        // The plain text region should not be preceded by a colour escape.
        let plain_idx = out.find("plain trailing line").expect("plain text present");
        let before = &out[..plain_idx];
        // The character immediately before "plain..." should be '\n', not 'm'
        // (which would indicate an SGR code right before).
        let prev_char = before.chars().next_back();
        assert!(
            matches!(prev_char, Some('\n')),
            "plain text should follow a newline cleanly, got {prev_char:?}"
        );
    }

    #[test]
    fn diff_context_line_has_no_ansi_codes() {
        // The " context" line (leading space) must be emitted verbatim with
        // no SGR colouring around it. A trailing bare reset is acceptable
        // (mirrors `highlight_code_block` behaviour for the empty/no-SGR
        // case — the default `AnsiState` is "needs reset").
        let diff = " context only line\n";
        let out = render_diff_block(diff);
        assert!(out.starts_with(" context only line\n"));
        // No diff-specific colours leaked into a context line.
        assert!(!out.contains(ANSI_DIFF_ADD));
        assert!(!out.contains(ANSI_DIFF_DEL));
        assert!(!out.contains(ANSI_DIFF_HUNK));
        assert!(!out.contains(ANSI_DIFF_FILE_HEADER));
    }

    #[test]
    fn classify_diff_line_ordering() {
        assert_eq!(classify_diff_line("@@ -1 +1 @@"), DiffLineKind::Hunk);
        assert_eq!(classify_diff_line("--- a/foo"), DiffLineKind::FileHeader);
        assert_eq!(classify_diff_line("+++ b/foo"), DiffLineKind::FileHeader);
        assert_eq!(classify_diff_line("diff --git a/x b/x"), DiffLineKind::FileHeader);
        assert_eq!(classify_diff_line("+added"), DiffLineKind::Add);
        assert_eq!(classify_diff_line("-deleted"), DiffLineKind::Del);
        assert_eq!(classify_diff_line(" context"), DiffLineKind::Context);
        assert_eq!(classify_diff_line(""), DiffLineKind::Context);
    }

    #[test]
    fn split_eol_variants() {
        assert_eq!(split_eol("abc\n"), ("abc", "\n"));
        assert_eq!(split_eol("abc\r\n"), ("abc", "\r\n"));
        assert_eq!(split_eol("abc"), ("abc", ""));
        assert_eq!(split_eol(""), ("", ""));
    }

    // ---------------------------------------------------------------
    // S1-C — AnsiState append-chain integration tests
    // ---------------------------------------------------------------

    /// Simulates the real append-chunk + push_reset_if_needed call pattern:
    /// multiple coloured segments, each terminated by push_reset_if_needed,
    /// must never accumulate consecutive `\x1b[0m\x1b[0m` pairs regardless of
    /// how many segments are appended in sequence.
    #[test]
    fn ansi_state_consecutive_reset_chunks_no_double_reset() {
        // Each "segment" mirrors what emit_highlighted_block does per line:
        // 1. push_reset_if_needed (border prefix guard)
        // 2. push colour introducer
        // 3. push content
        // 4. push_reset_if_needed (close colour)
        let segments: &[(&str, &str)] = &[
            ("\x1b[31m", "red line"),
            ("\x1b[32m", "green line"),
            ("\x1b[1;36m", "hunk header"),
            ("\x1b[32m", "another add line"),
        ];
        let mut s = AnsiState::default();
        let mut buf = String::new();
        for (color, text) in segments {
            // Guard before border — idempotent on first iteration (default state
            // is "needs reset"), no-op on subsequent iterations that left reset.
            s.push_reset_if_needed(&mut buf);
            s.push(&mut buf, color);
            s.push(&mut buf, text);
            s.push_reset_if_needed(&mut buf);
        }
        assert!(
            !buf.contains("\x1b[0m\x1b[0m"),
            "consecutive resets found in output: {buf:?}"
        );
        // Exactly one reset must close each segment (4 segments → 4 resets).
        // The initial push_reset_if_needed for the first segment uses the
        // default "needs reset" state — so it emits one extra. Total: 5.
        let resets = count_occurrences(&buf, "\x1b[0m");
        assert!(resets <= 5, "too many resets for 4 segments ({resets}): {buf:?}");
    }

    /// Inline code (`render_inline_markdown`) always ends with \x1b[0m.
    /// When a fenced code block follows immediately on the next line the
    /// shared AnsiState must not insert a spurious second reset between
    /// the inline-code reset and the opening border of the block.
    #[test]
    fn inline_code_before_code_block_no_double_reset() {
        // The inline `word` on line 1 ends with \x1b[0m.  The opening border
        // `  ┌─rust─` emits via state.push (plain text — no SGR).  The first
        // highlighted line inside the block begins with state.push_reset_if_needed,
        // which must be a no-op because ends_with_reset is already true.
        let md = "Use `word` before block\n```rust\nfn f() {}\n```";
        let out = render_markdown_with_highlighting(md);
        assert!(
            !out.contains("\x1b[0m\x1b[0m"),
            "double reset at inline-code / code-block junction: {out:?}"
        );
        // The inline-code yellow colour must still be present.
        assert!(out.contains("\x1b[33m"), "inline code colour missing");
        // The fenced block border must still appear.
        assert!(out.contains("┌─rust"), "opening border missing");
    }
}
