use super::layout::ACTIVE_SESSION_VIEW_DESIRED_ROWS;
use super::render_chrome::{format_worker_tokens_compact, truncate_chars_with_ellipsis};
use super::{
    ConversationLine, DIFF_SESSION_SEQ, PROVIDER_WORKER_SWITCHER_KIND, PROVIDER_WORKER_SWITCHER_SEQ_BASE,
    StreamingDraft, TRANSCRIPT_SESSION_SEQ, ToolStatus,
};
use crate::chat::action::{ProviderWorkerRowState, ProviderWorkerStatus, ProviderWorkerStatusRow};

/// Identifies which kind of foldable card was toggled by the unified `Tab`
/// keybinding. Reasoning remains a legacy variant for transcript data, while
/// the primary TUI only exposes tool cards as foldable rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldableKind {
    /// A `ConversationLine::Reasoning` card was flipped.
    Reasoning,
    /// A `ConversationLine::ToolResult` card was flipped.
    ToolResult,
}

/// Synthetic transcript row shown in the Ctrl+G child TUI switcher.
#[must_use]
pub fn transcript_switcher_entry() -> crate::chat::sessions::SwitcherEntry {
    let now = chrono::Utc::now();
    crate::chat::sessions::SwitcherEntry {
        seq: TRANSCRIPT_SESSION_SEQ,
        kind: crate::chat::sessions::model::ManagedKind::Transcript.as_str(),
        origin: "user",
        status: "ready",
        title: "conversation transcript".to_string(),
        created_at: now,
        updated_at: now,
        token_usage_records: Vec::new(),
        idle_warning: false,
    }
}

pub(super) fn switcher_entries_with_transcript_and_workers(
    entries: &[crate::chat::sessions::SwitcherEntry],
    workers: ProviderWorkerStatus,
    focus: crate::chat::sessions::FocusTarget,
) -> Vec<crate::chat::sessions::SwitcherEntry> {
    let worker_entries = provider_worker_switcher_entries(workers, focus);
    let mut out = Vec::with_capacity(entries.len().saturating_add(worker_entries.len()).saturating_add(1));
    out.push(transcript_switcher_entry());
    out.extend(worker_entries);
    out.extend(entries.iter().filter(|entry| !entry.is_transcript()).cloned());
    out
}

pub(super) fn provider_worker_switcher_entries(
    workers: ProviderWorkerStatus,
    focus: crate::chat::sessions::FocusTarget,
) -> Vec<crate::chat::sessions::SwitcherEntry> {
    let focused_worker = focus.worker_sequence();
    workers
        .rows
        .iter()
        .filter(|row| row.is_active() || focused_worker == Some(row.sequence))
        .map(provider_worker_switcher_entry)
        .collect()
}

pub(super) fn provider_worker_switcher_entry(row: &ProviderWorkerStatusRow) -> crate::chat::sessions::SwitcherEntry {
    let now = chrono::Utc::now();
    let created_at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(row.started_at_ms).unwrap_or(now);
    let status = match row.state {
        ProviderWorkerRowState::Running => "running",
        ProviderWorkerRowState::Cancelling => "cancelling",
        ProviderWorkerRowState::AwaitingCommit => "awaiting-commit",
        ProviderWorkerRowState::Committed => "completed",
        ProviderWorkerRowState::Cancelled => "cancelled",
        ProviderWorkerRowState::Failed => "failed",
    };
    let mut title = format!(
        "main provider w#{} {} task={}",
        row.sequence,
        crate::chat::action::provider_worker_row_kind_label(row.kind),
        row.task_id
    );
    if let Some(tokens) = row.finalized_total_tokens.filter(|tokens| *tokens > 0) {
        title.push_str(" tokens=");
        title.push_str(&format_worker_tokens_compact(tokens));
    }
    if row.is_active()
        && let Some(summary) = row.recent_tool_call.as_deref().filter(|summary| !summary.is_empty())
    {
        title.push_str(" · ");
        title.push_str(summary);
    }
    crate::chat::sessions::SwitcherEntry {
        seq: PROVIDER_WORKER_SWITCHER_SEQ_BASE.saturating_add(row.sequence),
        kind: PROVIDER_WORKER_SWITCHER_KIND,
        origin: "provider",
        status,
        title,
        created_at,
        updated_at: now,
        token_usage_records: Vec::new(),
        idle_warning: false,
    }
}

pub(super) fn is_provider_worker_switcher_entry(entry: &crate::chat::sessions::SwitcherEntry) -> bool {
    entry.kind == PROVIDER_WORKER_SWITCHER_KIND
}

pub(super) const fn provider_worker_sequence_from_switcher_seq(seq: u64) -> Option<u64> {
    if seq >= PROVIDER_WORKER_SWITCHER_SEQ_BASE {
        Some(seq.saturating_sub(PROVIDER_WORKER_SWITCHER_SEQ_BASE))
    } else {
        None
    }
}

pub(super) fn switcher_entry_display_id(entry: &crate::chat::sessions::SwitcherEntry) -> String {
    if is_provider_worker_switcher_entry(entry) {
        format!("w#{}", entry.seq.saturating_sub(PROVIDER_WORKER_SWITCHER_SEQ_BASE))
    } else {
        format!("#{}", entry.seq)
    }
}

pub(super) fn focus_active_entry_seq(focus: crate::chat::sessions::FocusTarget) -> Option<u64> {
    focus.session_seq().or_else(|| {
        focus
            .worker_sequence()
            .map(|sequence| PROVIDER_WORKER_SWITCHER_SEQ_BASE.saturating_add(sequence))
    })
}

pub(super) const fn tool_status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Done => "done",
        ToolStatus::Error => "error",
    }
}

pub(super) fn push_transcript_text(lines: &mut Vec<String>, label: &str, content: &str) {
    let mut parts = content.lines();
    if let Some(first) = parts.next() {
        lines.push(format!("{label}: {first}"));
        for part in parts {
            lines.push(format!("  {part}"));
        }
    } else {
        lines.push(format!("{label}:"));
    }
}

pub(super) fn transcript_lines_from_conversation(conversation: &[ConversationLine]) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    for item in conversation {
        match item {
            ConversationLine::User { content } => push_transcript_text(&mut lines, "user", content),
            ConversationLine::Assistant { content } => push_transcript_text(&mut lines, "assistant", content),
            ConversationLine::StreamingAssistant { content } => {
                push_transcript_text(&mut lines, "assistant (streaming)", content);
            }
            ConversationLine::System { content } => push_transcript_text(&mut lines, "system", content),
            ConversationLine::Tool { name, success } => {
                let status = if *success { "done" } else { "error" };
                lines.push(format!("tool {name}: {status}"));
            }
            ConversationLine::ToolResult {
                tool_name,
                args_full,
                result,
                status,
                ..
            } => {
                lines.push(format!("tool {tool_name} {}: {args_full}", tool_status_name(*status)));
                if let Some(result) = result {
                    push_transcript_text(&mut lines, "  result", result);
                }
            }
            ConversationLine::Reasoning {
                content,
                char_count,
                folded: _,
            } => {
                lines.push(format!("reasoning: {char_count} chars"));
                push_transcript_text(&mut lines, "  thought", content);
            }
        }
    }
    (lines, false)
}

pub(super) const PROVIDER_WORKER_IO_MAX_SOURCE_ITEMS: usize = 12;
pub(super) const PROVIDER_WORKER_IO_LINE_MAX_CHARS: u16 = 180;
pub(super) const PROVIDER_WORKER_IO_RESULT_LINES: usize = 3;

pub(super) fn provider_worker_io_clip(text: &str) -> String {
    truncate_chars_with_ellipsis(text.trim(), PROVIDER_WORKER_IO_LINE_MAX_CHARS, false)
}

pub(super) const PROVIDER_WORKER_TITLE_TOOL_MAX_CHARS: u16 = 64;

#[must_use]
pub fn latest_provider_worker_tool_summary_from_conversation(
    conversation: &[ConversationLine],
    ascii: bool,
) -> Option<String> {
    conversation
        .iter()
        .rev()
        .take(PROVIDER_WORKER_IO_MAX_SOURCE_ITEMS)
        .find_map(|item| match item {
            ConversationLine::ToolResult {
                tool_name,
                args_preview,
                args_full,
                status,
                ..
            } => {
                let args = if args_preview.trim().is_empty() {
                    args_full
                } else {
                    args_preview
                };
                let summary = format!(
                    "run {} {}: {}",
                    tool_name,
                    tool_status_name(*status),
                    provider_worker_io_clip(args)
                );
                Some(truncate_chars_with_ellipsis(
                    &summary,
                    PROVIDER_WORKER_TITLE_TOOL_MAX_CHARS,
                    ascii,
                ))
            }
            ConversationLine::Tool { name, success } => {
                let status = if *success { "done" } else { "error" };
                let summary = format!("run {name} {status}");
                Some(truncate_chars_with_ellipsis(
                    &summary,
                    PROVIDER_WORKER_TITLE_TOOL_MAX_CHARS,
                    ascii,
                ))
            }
            ConversationLine::User { .. }
            | ConversationLine::Assistant { .. }
            | ConversationLine::StreamingAssistant { .. }
            | ConversationLine::System { .. }
            | ConversationLine::Reasoning { .. } => None,
        })
}

/// Build compact live IO lines for the read-only provider worker detail view.
///
/// This is intentionally derived from the same conversation/tool cards the main
/// transcript renders, so the worker view stays an observation surface and does
/// not introduce a second history writer.
#[must_use]
pub fn provider_worker_io_lines_from_conversation(
    conversation: &[ConversationLine],
    streaming: Option<&StreamingDraft>,
    max_lines: usize,
) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for item in conversation
        .iter()
        .rev()
        .take(PROVIDER_WORKER_IO_MAX_SOURCE_ITEMS)
        .rev()
    {
        match item {
            ConversationLine::ToolResult {
                tool_name,
                args_preview,
                args_full,
                result,
                status,
                elapsed_ms,
                ..
            } => {
                let args = if args_preview.trim().is_empty() {
                    args_full
                } else {
                    args_preview
                };
                let mut head = format!(
                    "run {} {}: {}",
                    tool_name,
                    tool_status_name(*status),
                    provider_worker_io_clip(args)
                );
                if let Some(elapsed_ms) = elapsed_ms {
                    head.push_str(&format!(" ({elapsed_ms}ms)"));
                }
                lines.push(head);
                if let Some(result) = result {
                    for part in result.lines().take(PROVIDER_WORKER_IO_RESULT_LINES) {
                        lines.push(format!("output: {}", provider_worker_io_clip(part)));
                    }
                } else if matches!(status, ToolStatus::Running) {
                    lines.push("output: pending".to_string());
                }
            }
            ConversationLine::Tool { name, success } => {
                let status = if *success { "done" } else { "error" };
                lines.push(format!("run {name} {status}"));
            }
            ConversationLine::StreamingAssistant { content } => {
                if !content.trim().is_empty() {
                    lines.push(format!("assistant streaming: {}", provider_worker_io_clip(content)));
                }
            }
            ConversationLine::Assistant { content } => {
                if !content.trim().is_empty() {
                    lines.push(format!("assistant: {}", provider_worker_io_clip(content)));
                }
            }
            ConversationLine::Reasoning { char_count, .. } => {
                lines.push(format!("thinking: {char_count} chars"));
            }
            ConversationLine::User { .. } | ConversationLine::System { .. } => {}
        }
    }
    if let Some(streaming) = streaming
        && !streaming.accumulated.trim().is_empty()
    {
        lines.push(format!(
            "assistant streaming: {}",
            provider_worker_io_clip(&streaming.accumulated)
        ));
    }
    if lines.len() > max_lines {
        lines.drain(0..lines.len().saturating_sub(max_lines));
    }
    lines
}

#[must_use]
pub fn provider_worker_io_lines_for_streaming_draft(
    conversation: &[ConversationLine],
    streaming: Option<&StreamingDraft>,
    max_lines: usize,
) -> Vec<String> {
    streaming.map_or_else(Vec::new, |streaming| {
        provider_worker_io_lines_from_conversation(conversation, Some(streaming), max_lines)
    })
}

/// Build the read-only transcript child viewport from current conversation lines.
#[must_use]
pub fn build_transcript_view(
    session_title: &str,
    conversation: &[ConversationLine],
    scroll_offset: usize,
) -> crate::chat::sessions::ActiveSessionView {
    let (mut lines, truncated) = transcript_lines_from_conversation(conversation);
    if lines.is_empty() {
        lines.push("(transcript is empty - PageUp/Home view history; /help for copy/export)".to_string());
    }
    crate::chat::sessions::ActiveSessionView {
        seq: TRANSCRIPT_SESSION_SEQ,
        kind: crate::chat::sessions::model::ManagedKind::Transcript
            .as_str()
            .to_string(),
        title: if session_title.trim().is_empty() {
            "conversation transcript".to_string()
        } else {
            session_title.to_string()
        },
        lines,
        truncated,
        scroll_offset,
    }
}

/// Build the read-only diff child viewport from bounded unified diff lines.
#[must_use]
pub fn build_diff_view(
    title: &str,
    lines: Vec<String>,
    truncated: bool,
    scroll_offset: usize,
) -> crate::chat::sessions::ActiveSessionView {
    let mut lines = lines;
    if lines.is_empty() {
        lines.push("(no workspace diff)".to_string());
    }
    crate::chat::sessions::ActiveSessionView {
        seq: DIFF_SESSION_SEQ,
        kind: crate::chat::sessions::model::ManagedKind::Diff.as_str().to_string(),
        title: if title.trim().is_empty() {
            "workspace diff".to_string()
        } else {
            title.to_string()
        },
        lines,
        truncated,
        scroll_offset,
    }
    .clamped_for_height(usize::from(ACTIVE_SESSION_VIEW_DESIRED_ROWS))
}
