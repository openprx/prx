use similar::{Algorithm, TextDiff};

const DIFF_CONTEXT_RADIUS: usize = 3;
const DIFF_MAX_LINES: usize = 240;
const PREVIEW_MAX_LINES: usize = 40;
const PREVIEW_LINE_MAX_CHARS: usize = 220;

pub(crate) fn build_unified_diff(path: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::configure().algorithm(Algorithm::Myers).diff_lines(old, new);
    let rendered = diff
        .unified_diff()
        .context_radius(DIFF_CONTEXT_RADIUS)
        .missing_newline_hint(false)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    cap_diff_lines(&rendered)
}

pub(crate) fn build_new_file_preview(path: &str, content: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("--- /dev/null\n+++ b/{path}\n"));
    out.push_str(&format!("@@ new file preview: first {PREVIEW_MAX_LINES} lines @@\n"));

    let mut shown = 0usize;
    let mut truncated_line = false;
    for (idx, line) in content.lines().take(PREVIEW_MAX_LINES).enumerate() {
        shown = shown.saturating_add(1);
        let rendered = truncate_chars(line, PREVIEW_LINE_MAX_CHARS);
        truncated_line |= rendered.len() != line.len();
        out.push_str(&format!("{:>4} | {rendered}\n", idx.saturating_add(1)));
    }

    if content.is_empty() {
        out.push_str("   1 | \n");
    }

    let total_lines = content.lines().count();
    if total_lines > shown {
        out.push_str(&format!("... +{} more lines\n", total_lines.saturating_sub(shown)));
    } else if truncated_line {
        out.push_str("... one or more lines truncated\n");
    }
    out
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out = input.chars().take(max_chars).collect::<String>();
    out.push('…');
    out
}

fn cap_diff_lines(diff: &str) -> String {
    let mut out = String::new();
    let mut truncated = false;
    for (idx, line) in diff.lines().enumerate() {
        if idx >= DIFF_MAX_LINES {
            truncated = true;
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if truncated {
        out.push_str(&format!("... diff truncated after {DIFF_MAX_LINES} lines\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_includes_headers_hunk_context_and_changes() {
        let old = "one\ntwo\nthree\nfour\n";
        let new = "one\ntwo changed\nthree\nfour\n";
        let diff = build_unified_diff("f.txt", old, new);
        assert!(diff.contains("--- a/f.txt"));
        assert!(diff.contains("+++ b/f.txt"));
        assert!(diff.contains("@@ -1,4 +1,4 @@"));
        assert!(diff.contains(" one"));
        assert!(diff.contains("-two"));
        assert!(diff.contains("+two changed"));
    }

    #[test]
    fn new_file_preview_has_line_numbers_and_caps_rows() {
        let content = (1..=45).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let preview = build_new_file_preview("new.txt", &content);
        assert!(preview.contains("--- /dev/null"));
        assert!(preview.contains("+++ b/new.txt"));
        assert!(preview.contains("   1 | line 1"));
        assert!(preview.contains("  40 | line 40"));
        assert!(preview.contains("... +5 more lines"));
        assert!(!preview.contains("line 41\n"));
    }
}
