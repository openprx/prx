#![cfg(test)]

use super::*;

#[test]
fn diff_command_args_include_no_ext_diff_and_cached_flag() {
    let workspace = diff_command_args(false);
    assert_eq!(
        workspace,
        vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color", "--unified=3"]
    );
    for flag in crate::security::git_launch::DIFF_SAFETY_ARGS {
        assert!(workspace.contains(&flag), "{flag}");
    }
    assert!(
        workspace.contains(&"--no-ext-diff"),
        "git diff viewer must disable diff.external"
    );

    let cached = diff_command_args(true);
    assert!(cached.contains(&"--no-ext-diff"));
    assert!(cached.contains(&"--cached"));
}

#[test]
fn bounded_diff_lines_caps_bytes_lines_and_marks_truncated() {
    let raw = (0..12).map(|i| format!("+line {i}")).collect::<Vec<_>>().join("\n");
    let (lines, truncated) = bounded_diff_lines(&raw, raw.len(), 5);
    assert!(truncated);
    assert_eq!(lines.len(), 6, "5 retained lines plus truncation marker");
    assert_eq!(lines.last().expect("marker"), "[output truncated]");

    // Four 3-byte `\u{20ac}` (EURO) chars: the 5-byte cap lands mid-character.
    let (wide_lines, wide_truncated) = bounded_diff_lines("+\u{20ac}\u{20ac}\u{20ac}\u{20ac}", 5, 20);
    assert!(wide_truncated);
    let first = wide_lines.first().expect("line");
    assert!(
        first.len() <= 5,
        "byte cap must be enforced before line bounding, got {} bytes",
        first.len()
    );
    assert!(first.is_char_boundary(first.len()), "byte cap must not split utf-8");
}

#[test]
fn git_diff_error_line_is_single_bounded_line() {
    let line = git_diff_error_line(b"fatal: not a git repository\nsecond line", b"");
    assert_eq!(line, "diff unavailable: fatal: not a git repository");
}

#[tokio::test]
async fn collect_workspace_diff_git_failure_is_soft() {
    crate::security::git_launch::hermetic_git_for_tests();
    let temp = tempfile::TempDir::new().expect("tempdir");
    let source = collect_workspace_diff(temp.path(), false).await;
    assert_eq!(source.title, "workspace diff");
    assert_eq!(source.lines.len(), 1);
    assert!(
        source
            .lines
            .first()
            .is_some_and(|line| line.starts_with("diff unavailable:"))
    );
    assert!(!source.truncated);
}
