#![cfg(test)]

use super::*;

// ── P2-7: ToolResult card tests ──────────────────────────────────────────

#[test]
fn push_tool_result_started_inserts_running_card() {
    let mut state = TuiState::new("p", "m");
    state.push_tool_result_started("shell", r#"{"command":"ls -la /tmp"}"#);
    let idx = state.last_tool_result_index().expect("test: tool result exists");
    let line = state.conversation_lines.get(idx).expect("test: idx valid");
    match line {
        ConversationLine::ToolResult {
            tool_name,
            args_full,
            status,
            result,
            folded,
            elapsed_ms,
            args_preview,
            ..
        } => {
            assert_eq!(tool_name, "shell");
            assert_eq!(args_full, r#"{"command":"ls -la /tmp"}"#);
            assert_eq!(*status, ToolStatus::Running);
            assert!(result.is_none());
            assert!(*folded, "default state is folded");
            assert!(elapsed_ms.is_none());
            assert_eq!(args_preview, r#"command="ls -la /tmp""#);
        }
        other => panic!("test: expected ToolResult, got {other:?}"),
    }
}

#[test]
fn file_edit_and_write_cards_default_folded() {
    for tool_name in ["file_edit", "file_write"] {
        let mut state = TuiState::new("p", "m");
        state.push_tool_result_started(tool_name, r#"{"path":"src/lib.rs"}"#);
        let idx = state.last_tool_result_index().expect("test: tool result exists");
        match state.conversation_lines.get(idx).expect("test: idx valid") {
            ConversationLine::ToolResult { folded, .. } => {
                assert!(*folded, "{tool_name} should default to folded");
            }
            other => panic!("test: expected ToolResult, got {other:?}"),
        }
    }
}

#[test]
fn args_preview_truncates_long_input_and_collapses_newlines() {
    let raw = "x".repeat(150);
    let prev = build_args_preview(&raw, ARGS_PREVIEW_MAX_CHARS, ARGS_PREVIEW_ELLIPSIS);
    // 80 chars + 1-char unicode ellipsis → 81 chars total.
    assert_eq!(prev.chars().count(), ARGS_PREVIEW_MAX_CHARS + 1);
    assert!(prev.ends_with(ARGS_PREVIEW_ELLIPSIS));

    let multiline = "line1\n  line2\nline3";
    let prev2 = build_args_preview(multiline, ARGS_PREVIEW_MAX_CHARS, ARGS_PREVIEW_ELLIPSIS);
    // Newlines collapsed → single line, no ellipsis (well under 80 chars).
    assert_eq!(prev2, "line1 line2 line3");

    // ASCII fallback ellipsis.
    let prev3 = build_args_preview(&raw, 10, ARGS_PREVIEW_ELLIPSIS_ASCII);
    assert!(prev3.ends_with("..."));
    assert_eq!(prev3.chars().count(), 13); // 10 + "..."
}

#[test]
fn mark_last_tool_result_finished_transitions_status() {
    let mut state = TuiState::new("p", "m");
    state.push_tool_result_started("shell", "{}");
    // Match a different name → no-op.
    assert!(!state.mark_last_tool_result_finished("other", true, 100, None));
    // Correct name → update.
    assert!(state.mark_last_tool_result_finished("shell", true, 234, Some("ok".to_string())));
    let idx = state.last_tool_result_index().expect("test: idx");
    match state.conversation_lines.get(idx).expect("test: idx valid") {
        ConversationLine::ToolResult {
            status,
            elapsed_ms,
            result,
            ..
        } => {
            assert_eq!(*status, ToolStatus::Done);
            assert_eq!(*elapsed_ms, Some(234));
            assert_eq!(result.as_deref(), Some("ok"));
        }
        other => panic!("test: expected ToolResult, got {other:?}"),
    }
}

#[test]
fn mark_last_tool_result_finished_error_status() {
    let mut state = TuiState::new("p", "m");
    state.push_tool_result_started("write", "{}");
    assert!(state.mark_last_tool_result_finished("write", false, 50, Some("oops".to_string())));
    match state.conversation_lines.last().expect("test: last line present") {
        ConversationLine::ToolResult { status, .. } => assert_eq!(*status, ToolStatus::Error),
        other => panic!("test: expected ToolResult, got {other:?}"),
    }
}

#[test]
fn toggle_last_tool_result_folded_flips_state() {
    let mut state = TuiState::new("p", "m");
    // No ToolResult yet → returns None.
    assert_eq!(state.toggle_last_tool_result_folded(), None);
    state.push_tool_result_started("shell", "{}");
    // Default folded=true → first toggle → expanded.
    assert_eq!(state.toggle_last_tool_result_folded(), Some(false));
    assert_eq!(state.toggle_last_tool_result_folded(), Some(true));
}

#[test]
fn render_folded_tool_card_shows_status_and_glyph() {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let card = ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: r#"command="ls""#.to_string(),
        args_full: r#"{"command":"ls"}"#.to_string(),
        result: None,
        status: ToolStatus::Running,
        elapsed_ms: None,
        folded: true,
    };
    render_conversation_line(&mut lines, &card, false);
    // While running we render one animated run header with no follow-on
    // summary row yet.
    assert_eq!(lines.len(), 1, "running folded card renders to 1 line");
    let rendered: String = lines
        .first()
        .expect("test: at least one line")
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        UNICODE_SPINNER_FRAMES.iter().any(|frame| rendered.contains(frame)),
        "uses animated spinner: {rendered}"
    );
    assert!(rendered.contains("Running "), "shows running marker: {rendered}");
    assert!(
        rendered.contains(r#"shell(command="ls")"#),
        "shows Tool(args) preview: {rendered}"
    );
    assert_eq!(span_fg(&lines, 0, 0), Some(Color::Cyan));
}

#[test]
fn render_folded_tool_card_done_shows_hook_summary() {
    // Codex-style follow-on: `  └ 234ms · 3 lines · 5B` under
    // the low-emphasis action header once the tool finishes.
    let mut lines: Vec<Line<'_>> = Vec::new();
    let card = ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: r#"command="ls""#.to_string(),
        args_full: r#"{"command":"ls"}"#.to_string(),
        result: Some("a\nb\nc".to_string()),
        status: ToolStatus::Done,
        elapsed_ms: Some(234),
        folded: true,
    };
    render_conversation_line(&mut lines, &card, false);
    assert_eq!(lines.len(), 5, "done folded card renders header + summary + preview");
    assert!(
        line_to_plain(lines.first().expect("test: header")).starts_with("\u{25CF} Ran "),
        "completed tool uses the same filled neutral marker as assistant output"
    );
    let summary: String = lines
        .get(1)
        .expect("test: summary line present")
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(summary.contains("\u{2514}"), "uses └ branch glyph: {summary}");
    assert!(summary.contains("234ms"), "shows elapsed ms: {summary}");
    assert!(summary.contains("3 lines"), "shows result line count: {summary}");
    assert!(summary.contains("5B"), "shows result byte count: {summary}");
    assert_eq!(line_to_plain(lines.get(2).expect("test: preview line")), "    │ a");
    assert_eq!(line_to_plain(lines.get(3).expect("test: preview line")), "    │ b");
    assert_eq!(line_to_plain(lines.get(4).expect("test: preview line")), "    │ c");
    assert_eq!(span_fg(&lines, 0, 0), Some(Color::DarkGray));
    assert_eq!(span_fg(&lines, 1, 0), Some(Color::DarkGray));
}

#[test]
fn folded_tool_card_preview_preserves_indent_and_shows_hidden_line_count() {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let result = [
        "    let value = \"\u{ac00}\u{ac01}\u{ac02}\u{ac03}\u{ac04}\u{ac05}\u{ac06}\u{ac07}\";",
        "        println!(\"still indented\");",
        "    }",
        "extra hidden line one",
        "extra hidden line two",
    ]
    .join("\n");
    let card = ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "command=\"cargo test\"".to_string(),
        args_full: r#"{"command":"cargo test"}"#.to_string(),
        result: Some(result),
        status: ToolStatus::Done,
        elapsed_ms: Some(50),
        folded: true,
    };

    render_conversation_line(&mut lines, &card, false);

    let first_preview = line_to_plain(lines.get(2).expect("first preview line"));
    assert!(
        first_preview.starts_with("    │     let value"),
        "folded preview should preserve original code indentation: {first_preview:?}"
    );
    assert!(
        UnicodeWidthStr::width(first_preview.trim_start_matches("    │ ")) <= TOOL_FOLDED_RESULT_PREVIEW_CHARS,
        "folded preview should be width-bounded, not char-count bounded: {first_preview:?}"
    );
    let hidden = line_to_plain(lines.get(5).expect("hidden line count"));
    assert!(hidden.contains("+2 lines"), "hidden line count missing: {hidden:?}");
}

#[test]
fn render_folded_tool_card_error_shows_reason_and_red_marker() {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let card = ConversationLine::ToolResult {
        tool_name: "file_write".to_string(),
        args_preview: "path=src/lib.rs, bytes=3B".to_string(),
        args_full: r#"{"path":"src/lib.rs","content":"abc"}"#.to_string(),
        result: Some("permission denied\nstack trace".to_string()),
        status: ToolStatus::Error,
        elapsed_ms: Some(50),
        folded: true,
    };
    render_conversation_line(&mut lines, &card, false);

    let header = line_to_plain(lines.first().expect("test: header"));
    let summary = line_to_plain(lines.get(1).expect("test: summary"));

    assert!(header.starts_with("\u{00D7} Failed "), "error header marker: {header}");
    assert!(
        summary.contains("error \u{00B7} 50ms \u{00B7} permission denied"),
        "error summary: {summary}"
    );
    assert_eq!(span_fg(&lines, 0, 0), Some(Color::Red));
    assert_eq!(span_fg(&lines, 1, 0), Some(Color::Red));
    assert_eq!(
        line_to_plain(lines.get(2).expect("error preview first line")),
        "    │ permission denied",
        "folded error cards should preview the error body"
    );
    assert_eq!(
        line_to_plain(lines.get(3).expect("error preview second line")),
        "    │ stack trace",
        "folded error cards should preview follow-up error lines"
    );
}

#[test]
fn render_expanded_tool_card_shows_args_and_result() {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let card = ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "ls".to_string(),
        args_full: "{\"command\":\"ls -la /tmp\"}".to_string(),
        result: Some("total 24\ndrwxrwxrwt".to_string()),
        status: ToolStatus::Done,
        elapsed_ms: Some(234),
        folded: false,
    };
    render_conversation_line(&mut lines, &card, false);
    // Compact expanded style:
    //   row 0  `● Ran shell(command="ls -la /tmp")`
    //   row 1  `  └ input  command="ls -la /tmp"`
    //   row 2  `  └ output ✓ 2 lines · 20B`
    //   row 3+ output body
    assert_eq!(lines.len(), 5, "expanded card line count: {}", lines.len());
    let join = |i: usize| -> String {
        lines
            .get(i)
            .expect("test: line idx")
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    };
    assert!(
        join(0).starts_with("\u{25CF} Ran "),
        "uses filled action marker: {}",
        join(0)
    );
    assert!(
        join(0).contains("shell(command=\"ls -la /tmp\")"),
        "shows readable args: {}",
        join(0)
    );
    assert!(join(1).contains("\u{2514}"), "uses └ branch on input row: {}", join(1));
    assert!(
        join(1).contains("input  command=\"ls -la /tmp\""),
        "input row: {}",
        join(1)
    );
    assert!(join(2).contains("output"), "output label: {}", join(2));
    assert!(join(2).contains("2 lines"), "output metrics: {}", join(2));
    assert!(join(3).contains("total 24"), "first body row: {}", join(3));
    assert!(join(4).contains("drwxrwxrwt"), "second body row: {}", join(4));
}

#[test]
fn render_file_edit_diff_card_uses_colored_spans() {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let diff = "Applied 1 replacement in src/lib.rs\n\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n context\n";
    let card = ConversationLine::ToolResult {
        tool_name: "file_edit".to_string(),
        args_preview: "path=src/lib.rs".to_string(),
        args_full: r#"{"path":"src/lib.rs"}"#.to_string(),
        result: Some(diff.to_string()),
        status: ToolStatus::Done,
        elapsed_ms: Some(42),
        folded: false,
    };

    render_conversation_line(&mut lines, &card, false);

    let rendered = lines.iter().map(line_to_plain).collect::<Vec<_>>().join("\n");
    assert!(rendered.contains("@@ -1,2 +1,2 @@"), "hunk visible: {rendered}");
    assert!(rendered.contains("-old"), "deletion visible: {rendered}");
    assert!(rendered.contains("+new"), "addition visible: {rendered}");
    assert_eq!(span_fg_for_content(&lines, "-old"), Some(Color::Red));
    assert_eq!(span_fg_for_content(&lines, "+new"), Some(Color::Green));
    assert_eq!(span_fg_for_content(&lines, "@@ -1,2 +1,2 @@"), Some(Color::Cyan));
}

#[test]
fn render_folded_file_write_diff_preview_keeps_color() {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let diff = "Written 5 bytes to new.txt\n\n--- /dev/null\n+++ b/new.txt\n@@ new file preview: first 40 lines @@\n   1 | hello\n";
    let card = ConversationLine::ToolResult {
        tool_name: "file_write".to_string(),
        args_preview: "path=new.txt, bytes=5B".to_string(),
        args_full: r#"{"path":"new.txt","content":"hello"}"#.to_string(),
        result: Some(diff.to_string()),
        status: ToolStatus::Done,
        elapsed_ms: Some(10),
        folded: true,
    };

    render_conversation_line(&mut lines, &card, false);

    assert_eq!(
        span_fg_for_content(&lines, "--- /dev/null"),
        Some(Color::Gray),
        "file header should be highlighted"
    );
    assert_eq!(
        span_fg_for_content(&lines, "+++ b/new.txt"),
        Some(Color::Gray),
        "new file header should be highlighted"
    );
}

#[test]
fn render_tool_card_status_glyphs_and_colors() {
    // Running/result metadata keep explicit markers; the completed card
    // header itself uses the same neutral filled circle as assistant text.
    let (bullet, hook) = tool_card_glyphs(false);
    assert_eq!(bullet, "\u{25CF}", "unicode bullet ●");
    assert_eq!(hook, "\u{2514}", "unicode branch └");
    assert_eq!(
        tool_status_marker(ToolStatus::Running, false),
        ("\u{25CF}", Color::White)
    );
    assert_eq!(tool_status_marker(ToolStatus::Done, false), ("\u{2713}", Color::Green));
    assert_eq!(tool_status_marker(ToolStatus::Error, false), ("\u{2717}", Color::Red));
}

#[test]
fn render_tool_card_ascii_fallback_uses_plain_glyphs() {
    let mut state = TuiState::new("p", "m");
    state.set_ascii_fallback(true);
    state.push_tool_result_started("shell", "x");
    // ASCII state: ellipsis switches to "..." path; verify preview uses
    // the ASCII ellipsis for over-long input.
    state.push_tool_result_started("shell", &"y".repeat(200));
    let last = state.conversation_lines.last().expect("test: last");
    if let ConversationLine::ToolResult { args_preview, .. } = last {
        assert!(args_preview.ends_with("..."), "ASCII ellipsis: {args_preview}");
    } else {
        panic!("test: expected ToolResult");
    }
    // Render in ASCII mode → bullet is `*`, hook is `L`.
    let (bullet, hook) = tool_card_glyphs(true);
    assert_eq!(bullet, "*", "ASCII bullet");
    assert_eq!(hook, "L", "ASCII hook");
    assert_eq!(tool_status_marker(ToolStatus::Done, true), ("v", Color::Green));
    assert_eq!(tool_status_marker(ToolStatus::Error, true), ("x", Color::Red));

    let card = ConversationLine::ToolResult {
        tool_name: "t".to_string(),
        args_preview: String::new(),
        args_full: "x".to_string(),
        result: None,
        status: ToolStatus::Running,
        elapsed_ms: None,
        folded: true,
    };
    let mut lines: Vec<Line<'_>> = Vec::new();
    render_conversation_line(&mut lines, &card, true);
    let rendered: String = lines
        .first()
        .expect("test: first line")
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        ASCII_SPINNER_FRAMES
            .iter()
            .any(|frame| rendered.starts_with(&format!("{frame} "))),
        "ASCII spinner header: {rendered}"
    );

    let mut done_lines: Vec<Line<'_>> = Vec::new();
    let done_card = ConversationLine::ToolResult {
        tool_name: "t".to_string(),
        args_preview: String::new(),
        args_full: "{}".to_string(),
        result: Some("ok".to_string()),
        status: ToolStatus::Done,
        elapsed_ms: Some(1),
        folded: true,
    };
    render_conversation_line(&mut done_lines, &done_card, true);
    let done_header = done_lines.first().expect("test: done header");
    assert!(line_to_plain(done_header).starts_with("* Ran "), "ASCII success marker");
}

#[test]
fn tool_args_formatter_known_tools_are_readable() {
    assert_eq!(
        build_tool_args_preview(
            "file_read",
            r#"{"path":"src/chat/tui.rs","max_bytes":200}"#,
            ARGS_PREVIEW_MAX_CHARS,
            ARGS_PREVIEW_ELLIPSIS
        ),
        "path=src/chat/tui.rs, limit=200"
    );
    assert_eq!(
        build_tool_args_preview(
            "file_write",
            r#"{"path":"src/lib.rs","content":"hello"}"#,
            ARGS_PREVIEW_MAX_CHARS,
            ARGS_PREVIEW_ELLIPSIS
        ),
        "path=src/lib.rs, bytes=5B"
    );
    assert_eq!(
        build_tool_args_preview(
            "shell",
            r#"{"command":"cargo test -p openprx","cwd":"/opt/worker/code/prx"}"#,
            ARGS_PREVIEW_MAX_CHARS,
            ARGS_PREVIEW_ELLIPSIS
        ),
        "command=\"cargo test -p openprx\", cwd=/opt/worker/code/prx"
    );
    assert_eq!(
        build_tool_args_preview(
            "sessions_spawn",
            r#"{"task":"Audit session UX","model":"kimi-2.6","agent":"reviewer"}"#,
            ARGS_PREVIEW_MAX_CHARS,
            ARGS_PREVIEW_ELLIPSIS
        ),
        "task=\"Audit session UX\", agent=reviewer, model=kimi-2.6"
    );
    assert_eq!(
        build_tool_args_preview(
            "managed_session",
            r#"{"action":"shell","command":"for i in 1 2 3; do echo ok; sleep 1; done"}"#,
            ARGS_PREVIEW_MAX_CHARS,
            ARGS_PREVIEW_ELLIPSIS
        ),
        "action=shell, command=\"for i in 1 2 3; do echo ok; sleep 1; done\""
    );
    assert_eq!(
        build_tool_args_preview(
            "web_fetch",
            r#"{"url":"https://example.com/docs","max_chars":1200}"#,
            ARGS_PREVIEW_MAX_CHARS,
            ARGS_PREVIEW_ELLIPSIS
        ),
        "url=https://example.com/docs, limit=1200"
    );
}

#[test]
fn tool_args_formatter_unknown_uses_compact_json_and_clamps() {
    let preview = build_tool_args_preview(
        "custom_tool",
        r#"{"z":2,"nested":{"long":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}"#,
        40,
        ARGS_PREVIEW_ELLIPSIS,
    );

    assert!(preview.starts_with('{'), "compact JSON fallback: {preview}");
    assert!(preview.ends_with(ARGS_PREVIEW_ELLIPSIS), "clamped fallback: {preview}");
    assert!(!preview.contains('\n'), "single-line fallback: {preview}");
}

#[test]
fn tool_args_formatter_malformed_json_does_not_panic() {
    let preview = build_tool_args_preview("file_read", "{not valid json", 20, ARGS_PREVIEW_ELLIPSIS_ASCII);

    assert_eq!(preview, "{not valid json");
}

#[test]
fn expanded_tool_output_truncates_long_body() {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let output = (0..(TOOL_EXPANDED_OUTPUT_MAX_LINES + 3))
        .map(|idx| format!("line {idx}"))
        .collect::<Vec<_>>()
        .join("\n");
    let card = ConversationLine::ToolResult {
        tool_name: "shell".to_string(),
        args_preview: "command=\"generate\"".to_string(),
        args_full: r#"{"command":"generate"}"#.to_string(),
        result: Some(output),
        status: ToolStatus::Done,
        elapsed_ms: Some(10),
        folded: false,
    };

    render_conversation_line(&mut lines, &card, false);
    let rendered = lines.iter().map(line_to_plain).collect::<Vec<_>>().join("\n");

    assert!(
        rendered.contains("truncated:"),
        "long output has truncation summary: {rendered}"
    );
    assert!(
        rendered.contains("Ctrl+O for full transcript"),
        "expanded truncation hint should point to the verbose transcript: {rendered}"
    );
    assert!(
        lines.len() <= TOOL_EXPANDED_OUTPUT_MAX_LINES + 4,
        "expanded output is bounded: {}",
        lines.len()
    );
}

#[test]
fn total_content_lines_counts_folded_vs_expanded() {
    // Claude-Code style cards only differ in row count *after* the tool
    // has finished — while running, both folded and expanded views show
    // just the bullet header. Mark the result done first so the folded
    // view gets its `⎿ Done` summary and the expanded view gets the body.
    let mut state = TuiState::new("p", "m");
    state.push_tool_result_started("shell", "x");
    state.mark_last_tool_result_finished("shell", true, 12, Some("line1\nline2\nline3".to_string()));
    // rows as the production renderer draws them
    let rendered_rows = |state: &TuiState| {
        let mut lines: Vec<Line<'_>> = Vec::new();
        for line in &state.conversation_lines {
            render_conversation_line(&mut lines, line, false);
        }
        lines.len()
    };
    let folded_total = rendered_rows(&state);
    state.toggle_last_tool_result_folded();
    let expanded_total = rendered_rows(&state);
    assert!(
        expanded_total > folded_total,
        "expanded card takes more rows: {expanded_total} vs {folded_total}"
    );
}
