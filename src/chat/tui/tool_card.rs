use super::render_chrome::{current_animation_tick, spinner_frame_for_tick, truncate_chars_with_ellipsis};
use super::render_conversation::ansi_sgr_to_lines;
use super::status_style;
use super::{
    ARGS_PREVIEW_ELLIPSIS, ARGS_PREVIEW_ELLIPSIS_ASCII, ARGS_PREVIEW_MAX_CHARS, TOOL_ARG_VALUE_MAX_CHARS,
    TOOL_ERROR_REASON_MAX_CHARS, TOOL_EXPANDED_OUTPUT_LINE_MAX_CHARS, TOOL_EXPANDED_OUTPUT_MAX_LINES,
    TOOL_FOLDED_RESULT_PREVIEW_CHARS, TOOL_FOLDED_RESULT_PREVIEW_LINES, ToolStatus,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Build a single-line preview of a raw args string, truncated to `max_chars`
/// characters with the supplied ellipsis. Newlines are collapsed to spaces so
/// the preview stays on one row.
pub(super) fn build_args_preview(raw: &str, max_chars: usize, ellipsis: &str) -> String {
    // Collapse whitespace runs (incl. newlines) so the preview is single-line.
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    // Char-count truncation (avoids splitting multi-byte chars).
    let char_count = collapsed.chars().count();
    if char_count <= max_chars {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(max_chars).collect();
    format!("{truncated}{ellipsis}")
}

/// Render a compact `ToolResult` card using the same hierarchy as Codex:
/// a low-emphasis action row followed by indented result metadata and preview.
///
/// Folded layout (default):
/// ```text
/// ● Ran shell(command="ls /tmp")
///   └ 234ms · 12 lines · 1.4kB
/// ```
/// Expanded layout shows readable input plus bounded output/error rows. While
/// While `Running`, no follow-on row is shown — just the animated tool header.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_tool_result<'a>(
    lines: &mut Vec<Line<'a>>,
    tool_name: &'a str,
    args_preview: &'a str,
    args_full: &'a str,
    result: Option<&'a str>,
    status: ToolStatus,
    elapsed_ms: Option<u64>,
    folded: bool,
    ascii: bool,
) {
    let (_, hook) = tool_card_glyphs(ascii);
    let (status_glyph, status_color, action) = match status {
        ToolStatus::Running => (
            spinner_frame_for_tick(ascii, current_animation_tick()),
            status_style::ACTIVE,
            "Running ",
        ),
        ToolStatus::Done => (if ascii { "*" } else { "\u{25CF}" }, status_style::SUCCESS, "Ran "),
        ToolStatus::Cancelled => ("-", status_style::ATTENTION, "Cancelled "),
        ToolStatus::Error => (if ascii { "x" } else { "\u{00D7}" }, status_style::FAILURE, "Failed "),
    };
    let preview_ellipsis = if ascii {
        ARGS_PREVIEW_ELLIPSIS_ASCII
    } else {
        ARGS_PREVIEW_ELLIPSIS
    };
    let formatted_preview = build_tool_args_preview(tool_name, args_full, ARGS_PREVIEW_MAX_CHARS, preview_ellipsis);
    let display_preview = if formatted_preview.is_empty() {
        args_preview.to_string()
    } else {
        formatted_preview
    };
    let header = if display_preview.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name}({display_preview})")
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{status_glyph} "), Style::default().fg(status_color)),
        Span::styled(action, Style::default().fg(status_style::tool_color(status))),
        Span::raw(header),
    ]));

    // No follow-on row while still running — the animated header owns the
    // in-flight state. The card reveals timing once a terminal result arrives.
    if matches!(status, ToolStatus::Running) {
        return;
    }

    if folded {
        push_folded_tool_summary(lines, hook, tool_name, status, elapsed_ms, result, ascii);
        return;
    }

    push_expanded_tool_io(
        lines,
        hook,
        tool_name,
        status,
        &display_preview,
        args_full,
        result,
        ascii,
    );
}

pub(super) fn push_folded_tool_summary<'a>(
    lines: &mut Vec<Line<'a>>,
    hook: &str,
    tool_name: &str,
    status: ToolStatus,
    elapsed_ms: Option<u64>,
    result: Option<&str>,
    ascii: bool,
) {
    let metrics_style = Style::default().fg(Color::DarkGray);
    let result_text = result.unwrap_or("");
    let metrics = tool_card_metrics(status, elapsed_ms, result_text, ascii);
    let prefix = match status {
        ToolStatus::Error => "error · ",
        ToolStatus::Cancelled => "cancelled · ",
        _ => "",
    };
    lines.push(Line::from(Span::styled(
        format!("  {hook} {prefix}{metrics}"),
        if matches!(status, ToolStatus::Error | ToolStatus::Cancelled) {
            Style::default().fg(status_style::tool_color(status))
        } else {
            metrics_style
        },
    )));
    push_folded_tool_result_preview(lines, tool_name, result_text, ascii);
}

pub(super) fn push_folded_tool_result_preview<'a>(
    lines: &mut Vec<Line<'a>>,
    tool_name: &str,
    result_text: &str,
    ascii: bool,
) {
    if result_text.trim().is_empty() {
        return;
    }
    let ellipsis = if ascii {
        ARGS_PREVIEW_ELLIPSIS_ASCII
    } else {
        ARGS_PREVIEW_ELLIPSIS
    };
    let body_style = Style::default().fg(Color::DarkGray);
    let pipe = if ascii { "|" } else { "\u{2502}" };
    if let Some(rendered) = tool_result_rendered_diff_body_lines(tool_name, result_text, body_style) {
        let total_lines = rendered.len();
        for body in rendered.iter().take(TOOL_FOLDED_RESULT_PREVIEW_LINES) {
            push_prefixed_tool_body_line(lines, format!("    {pipe} "), body.clone(), body_style);
        }
        let hidden_lines = total_lines.saturating_sub(TOOL_FOLDED_RESULT_PREVIEW_LINES);
        if hidden_lines > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "    {pipe} {ellipsis} +{hidden_lines} {}",
                    if hidden_lines == 1 { "line" } else { "lines" }
                ),
                body_style,
            )));
        }
        return;
    }

    let result_lines = result_text.lines().collect::<Vec<_>>();
    let total_lines = result_lines.len();
    for body in result_lines.iter().take(TOOL_FOLDED_RESULT_PREVIEW_LINES) {
        let rendered = truncate_chars_with_ellipsis(body, TOOL_FOLDED_RESULT_PREVIEW_CHARS as u16, ascii);
        lines.push(Line::from(Span::styled(format!("    {pipe} {rendered}"), body_style)));
    }
    let hidden_lines = total_lines.saturating_sub(TOOL_FOLDED_RESULT_PREVIEW_LINES);
    if hidden_lines > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "    {pipe} {ellipsis} +{hidden_lines} {}",
                if hidden_lines == 1 { "line" } else { "lines" }
            ),
            body_style,
        )));
    }
}

pub(super) fn push_expanded_tool_io<'a>(
    lines: &mut Vec<Line<'a>>,
    hook: &str,
    tool_name: &str,
    status: ToolStatus,
    display_preview: &str,
    args_full: &str,
    result: Option<&str>,
    ascii: bool,
) {
    let input = if display_preview.is_empty() {
        build_args_preview(
            args_full,
            ARGS_PREVIEW_MAX_CHARS,
            if ascii {
                ARGS_PREVIEW_ELLIPSIS_ASCII
            } else {
                ARGS_PREVIEW_ELLIPSIS
            },
        )
    } else {
        display_preview.to_string()
    };
    let body_style = Style::default().fg(Color::DarkGray);
    lines.push(Line::from(vec![
        Span::styled(format!("  {hook} "), body_style),
        Span::styled(
            "input",
            Style::default().fg(status_style::INPUT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {input}"), body_style),
    ]));

    let label = match status {
        ToolStatus::Error => "error",
        ToolStatus::Cancelled => "cancelled",
        _ => "output",
    };
    let label_color = status_style::tool_color(status);
    let result_text = result.unwrap_or("");
    let metrics = tool_card_metrics(status, None, result_text, ascii);
    let (status_glyph, status_color) = tool_status_marker(status, ascii);
    lines.push(Line::from(vec![
        Span::styled(format!("  {hook} "), body_style),
        Span::styled(label, Style::default().fg(label_color).add_modifier(Modifier::BOLD)),
        Span::styled(" ", body_style),
        Span::styled(status_glyph, Style::default().fg(status_color)),
        Span::styled(format!(" {metrics}"), body_style),
    ]));
    if result_text.is_empty() {
        lines.push(Line::from(Span::styled("    (empty)", body_style)));
        return;
    }

    if let Some(rendered) = tool_result_rendered_diff_body_lines(tool_name, result_text, body_style) {
        let total_lines = rendered.len();
        let mut shown_lines = 0usize;
        for body in rendered.iter().take(TOOL_EXPANDED_OUTPUT_MAX_LINES) {
            push_prefixed_tool_body_line(lines, "    ".to_string(), body.clone(), body_style);
            shown_lines = shown_lines.saturating_add(1);
        }
        let hidden_lines = total_lines.saturating_sub(shown_lines);
        if hidden_lines > 0 {
            let ellipsis = if ascii {
                ARGS_PREVIEW_ELLIPSIS_ASCII
            } else {
                ARGS_PREVIEW_ELLIPSIS
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "    {ellipsis} truncated: {hidden_lines} {} hidden · Ctrl+O for full transcript",
                    if hidden_lines == 1 { "line" } else { "lines" }
                ),
                body_style,
            )));
        }
        return;
    }

    let ellipsis = if ascii {
        ARGS_PREVIEW_ELLIPSIS_ASCII
    } else {
        ARGS_PREVIEW_ELLIPSIS
    };
    let result_lines = result_text.lines().collect::<Vec<_>>();
    let total_lines = result_lines.len();
    let total_bytes = result_text.len();
    let mut shown_lines = 0usize;
    let mut shown_bytes = 0usize;
    let mut truncated = false;
    for (idx, body) in result_lines.iter().take(TOOL_EXPANDED_OUTPUT_MAX_LINES).enumerate() {
        let rendered = clamp_one_line(body, TOOL_EXPANDED_OUTPUT_LINE_MAX_CHARS, ellipsis);
        let line_truncated = rendered.as_str() != *body;
        truncated |= line_truncated;
        shown_bytes = shown_bytes.saturating_add(if line_truncated { rendered.len() } else { body.len() });
        if idx + 1 < total_lines {
            shown_bytes = shown_bytes.saturating_add(1);
        }
        shown_lines = shown_lines.saturating_add(1);
        lines.push(Line::from(Span::styled(format!("    {rendered}"), body_style)));
    }
    if total_lines > shown_lines || truncated {
        let hidden_lines = total_lines.saturating_sub(shown_lines);
        let hidden_bytes = total_bytes.saturating_sub(shown_bytes);
        lines.push(Line::from(Span::styled(
            format!(
                "    {ellipsis} truncated: {hidden_lines} {} · {} hidden · Ctrl+O for full transcript",
                if hidden_lines == 1 { "line" } else { "lines" },
                format_bytes(hidden_bytes)
            ),
            body_style,
        )));
    }
}

pub(super) fn tool_result_rendered_diff_body_lines(
    tool_name: &str,
    result_text: &str,
    prefix_style: Style,
) -> Option<Vec<Line<'static>>> {
    if !matches!(tool_name, "file_edit" | "file_write") {
        return None;
    }
    let diff_start = find_tool_result_diff_start(result_text)?;
    let (prefix, diff) = result_text.split_at(diff_start);
    if !crate::chat::renderer::is_diff_block(diff, None) {
        return None;
    }

    let mut lines = prefix
        .trim_end_matches('\n')
        .lines()
        .map(|line| Line::from(Span::styled(line.to_string(), prefix_style)))
        .collect::<Vec<_>>();
    lines.extend(ansi_sgr_to_lines(&crate::chat::renderer::render_diff_block(diff)));
    Some(lines)
}

pub(super) fn find_tool_result_diff_start(result_text: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in result_text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        if content.starts_with("diff --git ") || content.starts_with("--- a/") || content.starts_with("--- /") {
            return Some(offset);
        }
        offset = offset.saturating_add(line.len());
    }
    let trailing = &result_text[offset..];
    if trailing.starts_with("diff --git ") || trailing.starts_with("--- a/") || trailing.starts_with("--- /") {
        return Some(offset);
    }
    None
}

pub(super) fn push_prefixed_tool_body_line<'a>(
    lines: &mut Vec<Line<'a>>,
    prefix: String,
    body: Line<'static>,
    prefix_style: Style,
) {
    let mut spans = Vec::with_capacity(body.spans.len().saturating_add(1));
    spans.push(Span::styled(prefix, prefix_style));
    spans.extend(body.spans);
    lines.push(Line::from(spans));
}

pub(super) fn tool_card_metrics(status: ToolStatus, elapsed_ms: Option<u64>, result_text: &str, ascii: bool) -> String {
    match status {
        ToolStatus::Done => {
            let line_count = tool_result_line_count(result_text);
            let byte_count = result_text.len();
            let mut parts = Vec::new();
            if let Some(ms) = elapsed_ms {
                parts.push(format!("{ms}ms"));
            }
            parts.push(format!(
                "{line_count} {}",
                if line_count == 1 { "line" } else { "lines" }
            ));
            parts.push(format_bytes(byte_count));
            parts.join(" \u{00B7} ")
        }
        ToolStatus::Error | ToolStatus::Cancelled => {
            let mut parts = Vec::new();
            if let Some(ms) = elapsed_ms {
                parts.push(format!("{ms}ms"));
            }
            parts.push(tool_error_reason(result_text, ascii));
            parts.join(" \u{00B7} ")
        }
        ToolStatus::Running => String::new(),
    }
}

pub(super) fn tool_result_line_count(result_text: &str) -> usize {
    if result_text.is_empty() {
        0
    } else {
        result_text.lines().count()
    }
}

pub(super) fn tool_error_reason(result_text: &str, ascii: bool) -> String {
    let ellipsis = if ascii {
        ARGS_PREVIEW_ELLIPSIS_ASCII
    } else {
        ARGS_PREVIEW_ELLIPSIS
    };
    let reason = result_text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("error");
    clamp_one_line(reason, TOOL_ERROR_REASON_MAX_CHARS, ellipsis)
}

pub(super) fn format_bytes(bytes: usize) -> String {
    if bytes < 1_000 {
        return format!("{bytes}B");
    }
    if bytes < 1_000_000 {
        return format!("{:.1}kB", bytes as f64 / 1_000.0);
    }
    format!("{:.1}MB", bytes as f64 / 1_000_000.0)
}

pub(crate) fn build_tool_args_preview(tool_name: &str, raw: &str, max_chars: usize, ellipsis: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(raw);
    let Ok(value) = parsed else {
        return build_args_preview(raw, max_chars, ellipsis);
    };
    let Some(map) = value.as_object() else {
        return clamp_one_line(&compact_json_value(&value), max_chars, ellipsis);
    };

    let preview = match tool_name {
        "file_read" => format_file_read_preview(map),
        "file_write" => format_file_write_preview(map),
        "file_edit" => format_file_edit_preview(map),
        "shell" => format_shell_preview(map),
        "managed_session" => format_managed_session_preview(map),
        "sessions_spawn" | "delegate" | "subagents" | "session_worker" | "nodes" => {
            format_session_tool_preview(tool_name, map)
        }
        "web_fetch" | "http_request" | "web_search_tool" | "web_search" => format_web_preview(tool_name, map),
        _ => None,
    };
    let preview = preview.unwrap_or_else(|| compact_json_value(&value));
    clamp_one_line(&preview, max_chars, ellipsis)
}

pub(super) fn format_file_read_preview(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let path = json_string_field(map, &["path", "file_path"])?;
    let mut fields = vec![format!("path={path}")];
    if let Some(limit) = json_display_field(map, &["max_bytes", "limit", "max_lines"]) {
        fields.push(format!("limit={limit}"));
    }
    Some(fields.join(", "))
}

pub(super) fn format_file_write_preview(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let path = json_string_field(map, &["path", "file_path"])?;
    let bytes = map
        .get("content")
        .and_then(serde_json::Value::as_str)
        .map(str::len)
        .unwrap_or(0);
    Some(format!("path={path}, bytes={}", format_bytes(bytes)))
}

pub(super) fn format_file_edit_preview(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let path = json_string_field(map, &["path", "file_path"])?;
    let mut fields = vec![format!("path={path}")];
    if let Some(replace_all) = map.get("replace_all").and_then(serde_json::Value::as_bool)
        && replace_all
    {
        fields.push("replace_all=true".to_string());
    }
    if let Some(old) = map.get("old_string").and_then(serde_json::Value::as_str) {
        fields.push(format!("old_bytes={}", format_bytes(old.len())));
    }
    if let Some(new) = map.get("new_string").and_then(serde_json::Value::as_str) {
        fields.push(format!("new_bytes={}", format_bytes(new.len())));
    }
    Some(fields.join(", "))
}

pub(super) fn format_shell_preview(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let command = json_string_field(map, &["command", "cmd"])?;
    let mut fields = vec![format!(
        "command={}",
        quoted_summary(&command, TOOL_ARG_VALUE_MAX_CHARS)
    )];
    if let Some(cwd) = json_string_field(map, &["cwd", "workdir"]) {
        fields.push(format!("cwd={cwd}"));
    }
    Some(fields.join(", "))
}

pub(super) fn format_session_tool_preview(
    tool_name: &str,
    map: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let meta = extract_subagent_meta_from_map(map);
    if meta.is_empty() {
        if let Some(action) = json_string_field(map, &["action", "operation"]) {
            return Some(format!("action={action}"));
        }
        return None;
    }
    let mut fields = Vec::new();
    if let Some(task) = meta.task.as_deref() {
        fields.push(format!("task={}", quoted_summary(task, TOOL_ARG_VALUE_MAX_CHARS)));
    } else if let Some(action) = json_string_field(map, &["action", "operation"]) {
        fields.push(format!("action={action}"));
    }
    if let Some(agent) = meta.agent.as_deref() {
        fields.push(format!("agent={agent}"));
    }
    if let Some(model) = meta.model.as_deref() {
        fields.push(format!("model={model}"));
    }
    if fields.is_empty() {
        Some(tool_name.to_string())
    } else {
        Some(fields.join(", "))
    }
}

pub(super) fn format_managed_session_preview(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let action = json_string_field(map, &["action"])?;
    let mut fields = vec![format!("action={action}")];
    if let Some(command) = json_string_field(map, &["command"]) {
        fields.push(format!(
            "command={}",
            quoted_summary(&command, TOOL_ARG_VALUE_MAX_CHARS)
        ));
    }
    if let Some(session_id) = json_string_field(map, &["session_id"]) {
        fields.push(format!("session={}", one_line_summary(&session_id, 12)));
    }
    Some(fields.join(", "))
}

pub(super) fn format_web_preview(tool_name: &str, map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    if let Some(url) = json_string_field(map, &["url", "uri", "endpoint"]) {
        let mut fields = vec![format!("url={url}")];
        if let Some(max_chars) = json_display_field(map, &["max_chars", "limit"]) {
            fields.push(format!("limit={max_chars}"));
        }
        return Some(fields.join(", "));
    }
    if let Some(query) = json_string_field(map, &["query", "q"]) {
        return Some(format!("query={}", quoted_summary(&query, TOOL_ARG_VALUE_MAX_CHARS)));
    }
    if tool_name == "http_request" {
        return json_string_field(map, &["method"]).map(|method| format!("method={method}"));
    }
    None
}

pub(super) fn extract_subagent_meta_from_map(map: &serde_json::Map<String, serde_json::Value>) -> SubagentMeta {
    let str_field = |key: &str| {
        map.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut meta = SubagentMeta {
        agent: str_field("agent"),
        model: str_field("model"),
        task: str_field("prompt").or_else(|| str_field("task")),
    };
    if meta.task.is_none() {
        meta.task = str_field("action").or_else(|| str_field("operation"));
    }
    if let Some(task) = meta.task.take() {
        meta.task = Some(one_line_summary(&task, 60));
    }
    meta
}

pub(super) fn json_string_field(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        map.get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

pub(super) fn json_display_field(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let value = map.get(*key)?;
        if let Some(s) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            return Some(s.to_string());
        }
        if value.is_number() || value.is_boolean() {
            return Some(value.to_string());
        }
        None
    })
}

pub(super) fn compact_json_value(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

pub(super) fn quoted_summary(raw: &str, max_chars: usize) -> String {
    let summary = one_line_summary(raw, max_chars);
    format!("\"{}\"", summary.replace('"', "\\\""))
}

pub(super) fn clamp_one_line(raw: &str, max_chars: usize, ellipsis: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = collapsed.chars().count();
    if char_count <= max_chars {
        return collapsed;
    }
    let keep = max_chars.saturating_sub(ellipsis.chars().count()).max(1);
    let mut truncated: String = collapsed.chars().take(keep).collect();
    truncated.push_str(ellipsis);
    truncated
}

/// Render a `Reasoning` card in Claude-Code style.
///
/// Folded → `▸ Thinking (123 tokens)` (or `> Thinking (123 tokens)` in ASCII),
/// dim gray + italic so the line reads as a collapsed annotation rather than
/// primary content.
/// Expanded → header `▾ Thinking (123 tokens)` (or `v Thinking (...)`) on
///   row 0, followed by the body — each line indented by two spaces and
///   rendered in dim gray italic so the eye flows back to the visible
///   assistant text below.
///
/// Token count is estimated as `chars / 4`, matching the rough heuristic
/// used by [`StreamChunk::with_token_estimate`] elsewhere in the codebase.
/// `char_count` is taken straight from the cached field on
/// `ConversationLine::Reasoning` so we never re-walk the body on every
/// frame.
///
/// We do NOT apply markdown / syntax highlighting here — that interacts with
/// the P1-5 ANSI state machine and P2-9 diff renderer in ways that would
/// need dedicated isolation; reasoning text is dimmed plain text so the user
/// can always read it without ANSI bleed.
pub(super) fn render_reasoning_card<'a>(
    lines: &mut Vec<Line<'a>>,
    content: &'a str,
    char_count: usize,
    folded: bool,
    ascii: bool,
) {
    let (folded_icon, expanded_icon) = reasoning_card_glyphs(ascii);
    let tokens = estimate_reasoning_tokens(char_count);
    let token_word = if tokens == 1 { "token" } else { "tokens" };
    let header_style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);

    if folded {
        // Single-line folded summary — token count only, no noisy key-hint prose.
        let header = format!("{folded_icon} Thinking ({tokens} {token_word})");
        lines.push(Line::from(Span::styled(header, header_style)));
        return;
    }

    let header = if ascii {
        format!("{expanded_icon} Thinking ({tokens} {token_word})")
    } else {
        format!("{expanded_icon} Thinking ({tokens} {token_word}) - Tab to collapse")
    };
    lines.push(Line::from(Span::styled(header, header_style)));

    let body_style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
    for body_line in content.lines() {
        lines.push(Line::from(Span::styled(format!("  {body_line}"), body_style)));
    }
}

/// Rough token estimate for a reasoning body. Mirrors the `len / 4` heuristic
/// used by [`StreamChunk::with_token_estimate`] but on a `char` count to stay
/// stable for non-ASCII content. Always at least `1` for non-empty bodies so
/// the folded summary never advertises a `(0 tokens)` card; `0` is reserved
/// for the empty case (and `push_reasoning` already drops empty inputs).
pub(super) const fn estimate_reasoning_tokens(char_count: usize) -> usize {
    if char_count == 0 {
        return 0;
    }
    let est = char_count.div_ceil(4);
    if est == 0 { 1 } else { est }
}

/// Pick the folded / expanded leading triangle glyph used by a reasoning card.
///
/// Folded   → `▸` (or `>` in ASCII) — points right, "click me to open".
/// Expanded → `▾` (or `v` in ASCII) — points down, "the body follows".
pub(super) const fn reasoning_card_glyphs(ascii: bool) -> (&'static str, &'static str) {
    if ascii {
        (">", "v")
    } else {
        ("\u{25B8}", "\u{25BE}") // ▸ ▾
    }
}

/// Pick the bullet and branch glyph used by tool cards.
///
/// Completed cards use the hook for the follow-on summary/body. Running rich
/// cards replace the legacy bullet with the shared spinner at render time.
pub(super) const fn tool_card_glyphs(ascii: bool) -> (&'static str, &'static str) {
    if ascii { ("*", "L") } else { ("\u{25CF}", "\u{2514}") }
}

/// Status → header/result marker + marker color.
pub(super) const fn tool_status_marker(status: ToolStatus, ascii: bool) -> (&'static str, Color) {
    match status {
        ToolStatus::Cancelled => ("-", status_style::ATTENTION),
        ToolStatus::Running => {
            if ascii {
                ("*", status_style::ACTIVE)
            } else {
                ("\u{25CF}", status_style::ACTIVE)
            }
        }
        ToolStatus::Done => {
            if ascii {
                ("v", Color::Green)
            } else {
                ("\u{2713}", Color::Green)
            }
        }
        ToolStatus::Error => {
            if ascii {
                ("x", Color::Red)
            } else {
                ("\u{2717}", Color::Red)
            }
        }
    }
}

// ── BUG-11 / UX-E: sub-agent preview visibility ─────────────────────────────
//
// The delegate / sessions_spawn / subagents family of tools each spawn a *child*
// agent that runs its own LLM turns and tool calls. Without observer streaming
// (NoopObserver lives behind the tools boundary), the TUI only sees the parent
// tool card spin and then a flat text result. To keep the card shape consistent
// with Claude-like tool rendering, we surface the agent/model/task in the normal
// `tool(preview)` header instead of a separate robot card.

/// Sub-agent metadata extracted from a tool card for the BUG-11 visibility
/// header. Every field is optional because args may be malformed JSON and the
/// result is absent while the tool is still running.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct SubagentMeta {
    /// Target agent name (e.g. `researcher`). From args `agent`, or parsed from
    /// the `[Agent '...' (...)]` result banner.
    pub(super) agent: Option<String>,
    /// Model the sub-agent ran on (e.g. `kimi-2.6`). From args `model`, or the
    /// `provider/model` portion of the result banner.
    pub(super) model: Option<String>,
    /// Short one-line summary of the delegated task (args `prompt` / `task`).
    pub(super) task: Option<String>,
}

impl SubagentMeta {
    /// True when no useful field could be extracted — caller falls back to the
    /// generic tool card so we never render an empty `🤖 [/]` header.
    pub(super) const fn is_empty(&self) -> bool {
        self.agent.is_none() && self.model.is_none() && self.task.is_none()
    }
}

/// Collapse whitespace (incl. newlines) and clip to `max` chars with an ellipsis
/// so a multi-line prompt renders as a single tidy summary cell.
pub(super) fn one_line_summary(raw: &str, max: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        let head: String = collapsed.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        collapsed
    }
}
