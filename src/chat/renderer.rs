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

/// Render a code block with syntax highlighting, returning ANSI-escaped text.
///
/// `language` is the optional language identifier from the code fence (e.g., "rust", "python").
/// Returns the highlighted code as a string with ANSI escape sequences.
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

    for line in LinesWithEndings::from(code) {
        match highlighter.highlight_line(line, &SYNTAX_SET) {
            Ok(ranges) => {
                output.push_str(&as_24_bit_terminal_escaped(&ranges[..], false));
            }
            Err(_) => {
                // Fallback: plain text
                output.push_str(line);
            }
        }
    }
    // Reset ANSI colors
    output.push_str("\x1b[0m");
    output
}

/// Render markdown text with code block highlighting.
///
/// Detects fenced code blocks (``` or ~~~), highlights them, and returns
/// the full text with code blocks replaced by highlighted versions.
pub fn render_markdown_with_highlighting(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
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
            // Print a visual separator
            result.push_str("  ┌─");
            if let Some(ref lang) = code_language {
                result.push_str(lang);
                result.push('─');
            }
            result.push('\n');
        } else if in_code_block && (line.starts_with("```") || line.starts_with("~~~")) {
            // End of code block — highlight and append
            let highlighted = highlight_code_block(&code_buffer, code_language.as_deref());
            for hl_line in highlighted.lines() {
                result.push_str("  │ ");
                result.push_str(hl_line);
                result.push('\n');
            }
            result.push_str("  └─\n");
            in_code_block = false;
            code_language = None;
        } else if in_code_block {
            code_buffer.push_str(line);
            code_buffer.push('\n');
        } else {
            // Regular markdown line — apply basic formatting
            result.push_str(&render_inline_markdown(line));
            result.push('\n');
        }
    }

    // Handle unclosed code block
    if in_code_block && !code_buffer.is_empty() {
        let highlighted = highlight_code_block(&code_buffer, code_language.as_deref());
        for hl_line in highlighted.lines() {
            result.push_str("  │ ");
            result.push_str(hl_line);
            result.push('\n');
        }
        result.push_str("  └─\n");
    }

    result
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
}
