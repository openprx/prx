#![cfg(test)]

use super::*;

#[test]
fn tui_state_turn_count_tracks_user_messages_only() {
    // TuiState owns the conversation log and turn counter. This pins the
    // contract that `turn_count` advances on user submissions only.
    let mut state = TuiState::new("test", "model");
    state.push_user_message("hello");
    state.push_assistant_message("world");
    assert_eq!(state.turn_count, 1, "only user messages bump turn_count");
    state.push_user_message("again");
    assert_eq!(state.turn_count, 2);
}

// ── BUG-11: sub-agent tool-card visibility ─────────────────────────

#[test]
fn one_line_summary_collapses_and_clips() {
    let raw = "line one\n  line   two\tand more words here to overflow the limit clearly";
    let out = one_line_summary(raw, 20);
    assert!(!out.contains('\n'), "newlines collapsed");
    assert!(out.chars().count() <= 20, "clipped to max");
    assert!(out.ends_with('…'), "overflow gets ellipsis");
}

#[test]
fn render_delegate_card_shows_readable_identity_preview() {
    // The rendered sub-agent card must surface agent/model/task in the
    // same Claude-like `Tool(preview)` header shape as every other tool.
    let mut lines: Vec<Line<'_>> = Vec::new();
    let args = r#"{"agent":"researcher","model":"kimi-2.6","prompt":"investigate the bug"}"#;
    render_tool_result(
        &mut lines,
        "delegate",
        "agent: researcher",
        args,
        Some("[Agent 'researcher' (openrouter/kimi-2.6)]\nFound the root cause."),
        ToolStatus::Done,
        Some(1200),
        true, // folded
        true, // ascii — deterministic, no unicode glyphs
    );
    let header = lines.first().map(line_to_plain).expect("test: header line present");
    assert!(header.starts_with("* Ran "), "success marker present: {header}");
    assert!(header.contains("delegate("), "tool name + preview: {header}");
    assert!(
        header.contains("task=\"investigate the bug\""),
        "task summary: {header}"
    );
    assert!(header.contains("agent=researcher"), "agent: {header}");
    assert!(header.contains("model=kimi-2.6"), "model: {header}");
}

#[test]
fn render_non_subagent_card_unchanged() {
    // A normal tool keeps the classic `Tool(args)` header — no robot.
    let mut lines: Vec<Line<'_>> = Vec::new();
    render_tool_result(
        &mut lines,
        "shell",
        "ls /tmp",
        r#"{"command":"ls /tmp"}"#,
        Some("file_a\nfile_b"),
        ToolStatus::Done,
        Some(34),
        true,
        true,
    );
    let header = lines.first().map(line_to_plain).expect("test: header line present");
    assert!(
        header.contains("shell(command=\"ls /tmp\")"),
        "classic readable header: {header}"
    );
    assert!(!header.contains("[bot]"), "no robot for normal tools: {header}");
}

#[test]
fn finalized_assistant_markdown_renders_inline_code_and_fenced_blocks() {
    let _guard = MARKDOWN_CACHE_TEST_LOCK.lock();
    ASSISTANT_MARKDOWN_CACHE.lock().clear();
    let line = ConversationLine::Assistant {
        content: "Use `cargo build`\n```rust\nfn main() {}\n```".to_string(),
    };
    let mut lines: Vec<Line<'_>> = Vec::new();
    render_conversation_line(&mut lines, &line, false);

    let rendered = lines.iter().map(line_to_plain).collect::<Vec<_>>().join("\n");
    assert!(
        rendered.contains("Use cargo build"),
        "inline text rendered: {rendered:?}"
    );
    assert!(rendered.contains("┌─rust"), "fenced code border rendered: {rendered:?}");
    assert!(rendered.contains("fn main() {}"), "code body rendered: {rendered:?}");
    assert_eq!(
        span_fg_for_content(&lines, "cargo build"),
        Some(Color::Yellow),
        "inline code should bridge ANSI yellow into ratatui style"
    );
}

#[test]
fn streaming_assistant_markdown_keeps_cursor_after_highlighted_content() {
    let line = ConversationLine::StreamingAssistant {
        content: "Use `cargo build`".to_string(),
    };
    let mut lines: Vec<Line<'_>> = Vec::new();
    render_conversation_line(&mut lines, &line, false);

    let body = lines.first().map(line_to_plain).expect("streaming body rendered");
    assert!(body.ends_with('\u{258C}'), "cursor remains at streaming tail: {body:?}");
    assert_eq!(
        span_fg_for_content(&lines, "cargo build"),
        Some(Color::Yellow),
        "streaming markdown should use the same ANSI bridge"
    );
}

#[test]
fn large_streaming_markdown_uses_plain_threshold_path_with_cursor() {
    let content = format!(
        "```rust\n{}\n```",
        "fn demo() {}\n".repeat((STREAMING_MARKDOWN_HIGHLIGHT_MAX_BYTES / 12).saturating_add(1))
    );

    let lines = render_streaming_assistant_markdown_lines(&content, false);
    let rendered = lines.iter().map(line_to_plain).collect::<Vec<_>>().join("\n");

    assert!(
        rendered.contains("```rust"),
        "large streaming markdown should stay plain"
    );
    assert!(rendered.ends_with('\u{258C}'), "cursor remains at large streaming tail");
    assert!(
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .all(|span| span.style == Style::default()),
        "large streaming threshold path should avoid expensive highlighted spans"
    );
}

#[test]
fn ansi_bridge_supports_indexed_color_and_skips_non_sgr_csi() {
    let lines = ansi_sgr_to_lines("plain\x1b[2K \x1b[38;5;196mred\x1b[0m done");
    assert_eq!(lines.len(), 1);
    assert_eq!(
        line_to_plain(lines.first().expect("one ANSI bridge line")),
        "plain red done"
    );
    assert_eq!(
        span_fg_for_content(&lines, "red"),
        Some(Color::Indexed(196)),
        "38;5 indexed colour should bridge into ratatui style"
    );
}

#[test]
fn finalized_assistant_markdown_uses_render_cache() {
    let _guard = MARKDOWN_CACHE_TEST_LOCK.lock();
    ASSISTANT_MARKDOWN_CACHE.lock().clear();
    let first = cached_finalized_assistant_markdown_lines("Use `cargo build`");
    let second = cached_finalized_assistant_markdown_lines("Use `cargo build`");
    assert!(
        Arc::ptr_eq(&first, &second),
        "finalized assistant markdown should reuse cached rendered spans"
    );
}
