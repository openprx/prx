//! TUI layout and rendering for `prx chat` using ratatui.
//!
//! Provides a three-area layout:
//! - Status bar (top): provider/model, session info, turn count
//! - Output area (middle): scrollable conversation display
//! - Input area (bottom): prompt text
//!
//! Gated behind the `terminal-tui` feature.

use async_trait::async_trait;
use parking_lot::Mutex;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use std::collections::HashMap;

use crate::chat::terminal_proto::{
    DraftVersionTracker, InlineDraftProtocol, LineProtocolError, apply_line_replacement,
};

/// State for the TUI layout.
pub struct TuiState {
    /// Provider/model displayed in status bar
    pub provider: String,
    pub model: String,
    /// Session title
    pub session_title: String,
    /// Number of conversation turns
    pub turn_count: usize,
    /// Rendered conversation lines
    pub conversation_lines: Vec<ConversationLine>,
    /// Current scroll offset (0 = bottom, latest)
    pub scroll_offset: usize,
    /// Current input text
    pub input_text: String,
    /// Viewport height for the output area (updated on render)
    pub viewport_height: u16,
    /// Render ASCII-only icons instead of unicode glyphs (for non-UTF-8 terms).
    pub ascii_fallback: bool,
}

/// Maximum width (in chars) for the args preview shown in folded tool cards.
///
/// Anything longer is truncated and ends with [`ARGS_PREVIEW_ELLIPSIS`]. The
/// full text is preserved separately in [`ConversationLine::ToolResult::args_full`].
pub const ARGS_PREVIEW_MAX_CHARS: usize = 80;

/// Ellipsis appended to a truncated args preview. Single-char so we don't
/// have to think about grapheme widths on the terminal.
pub const ARGS_PREVIEW_ELLIPSIS: &str = "…";

/// ASCII fallback for the ellipsis when the terminal is in non-UTF-8 mode.
pub const ARGS_PREVIEW_ELLIPSIS_ASCII: &str = "...";

/// Status of a tool call card.
///
/// `Running` means the tool was invoked but no result has been received yet;
/// `Done` and `Error` are terminal states carrying the result string and a
/// completion duration. The card is always rendered with the same shape — only
/// the status icon and the trailing badge change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Error,
}

/// A single line in the conversation display.
///
/// Variants other than [`ConversationLine::ToolResult`] correspond to plain
/// text bubbles (user / assistant / system) and a legacy short tool-call
/// indicator. `ToolResult` is the rich, foldable tool-invocation card
/// introduced by P2-7.
#[derive(Clone, Debug)]
pub enum ConversationLine {
    /// User-sent message.
    User { content: String },
    /// Assistant response (final, post-streaming).
    Assistant { content: String },
    /// System / status message (dimmed in render).
    System { content: String },
    /// Legacy single-line tool indicator (kept for back-compat with
    /// [`TuiState::push_tool_call`]; new code should prefer `ToolResult`).
    Tool { name: String, success: bool },
    /// Tool invocation card with args + result, default folded.
    ///
    /// `args_preview` is a short, single-line summary; `args_full` keeps the
    /// raw JSON so the expanded view can show it verbatim. `result` is
    /// `None` while the tool is still running and `Some(_)` once finished.
    ToolResult {
        tool_name: String,
        args_preview: String,
        args_full: String,
        result: Option<String>,
        status: ToolStatus,
        elapsed_ms: Option<u64>,
        folded: bool,
    },
}

impl ConversationLine {
    /// True if this line is a `ToolResult` variant. Used by [`TuiState`] to
    /// locate the most recent tool card for `Tab` toggling without exposing
    /// pattern-matching to callers.
    pub const fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult { .. })
    }
}

impl TuiState {
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            session_title: String::new(),
            turn_count: 0,
            conversation_lines: Vec::new(),
            scroll_offset: 0,
            input_text: String::new(),
            viewport_height: 0,
            ascii_fallback: false,
        }
    }

    /// Toggle ASCII fallback mode for icons (`▸/▾` → `>/v`, `…` → `...`).
    pub const fn set_ascii_fallback(&mut self, on: bool) {
        self.ascii_fallback = on;
    }

    /// Add a user message to the conversation display.
    pub fn push_user_message(&mut self, content: &str) {
        self.conversation_lines.push(ConversationLine::User {
            content: content.to_string(),
        });
        self.turn_count += 1;
        self.scroll_to_bottom();
    }

    /// Add an assistant message to the conversation display.
    pub fn push_assistant_message(&mut self, content: &str) {
        self.conversation_lines.push(ConversationLine::Assistant {
            content: content.to_string(),
        });
        self.scroll_to_bottom();
    }

    /// Add a system / status message.
    pub fn push_system_message(&mut self, content: &str) {
        self.conversation_lines.push(ConversationLine::System {
            content: content.to_string(),
        });
        self.scroll_to_bottom();
    }

    /// Add a legacy single-line tool call indicator.
    pub fn push_tool_call(&mut self, name: &str, success: bool) {
        self.conversation_lines.push(ConversationLine::Tool {
            name: name.to_string(),
            success,
        });
    }

    /// Push a new `ToolResult` card in the `Running` state.
    ///
    /// `args_full` is preserved verbatim for the expanded view; a truncated
    /// preview is derived via [`build_args_preview`]. The card is folded by
    /// default — call [`Self::toggle_last_tool_result_folded`] to expand.
    pub fn push_tool_result_started(&mut self, tool_name: &str, args_full: &str) {
        let preview_ellipsis = if self.ascii_fallback {
            ARGS_PREVIEW_ELLIPSIS_ASCII
        } else {
            ARGS_PREVIEW_ELLIPSIS
        };
        let args_preview = build_args_preview(args_full, ARGS_PREVIEW_MAX_CHARS, preview_ellipsis);
        self.conversation_lines.push(ConversationLine::ToolResult {
            tool_name: tool_name.to_string(),
            args_preview,
            args_full: args_full.to_string(),
            result: None,
            status: ToolStatus::Running,
            elapsed_ms: None,
            folded: true,
        });
        self.scroll_to_bottom();
    }

    /// Find the most recent `Running` tool card whose name matches and update
    /// it with the terminal status (`Done` / `Error`), the elapsed time, and
    /// an optional result. If no such card exists this is a no-op.
    ///
    /// Returns `true` if a card was updated.
    pub fn mark_last_tool_result_finished(
        &mut self,
        tool_name: &str,
        success: bool,
        duration_ms: u64,
        result: Option<String>,
    ) -> bool {
        for line in self.conversation_lines.iter_mut().rev() {
            if let ConversationLine::ToolResult {
                tool_name: name,
                status,
                elapsed_ms,
                result: r,
                ..
            } = line
                && *status == ToolStatus::Running
                && name == tool_name
            {
                *status = if success { ToolStatus::Done } else { ToolStatus::Error };
                *elapsed_ms = Some(duration_ms);
                *r = result;
                return true;
            }
        }
        false
    }

    /// Toggle the folded state of the last `ToolResult` line (if any).
    ///
    /// Returns the new `folded` value, or `None` if no `ToolResult` exists.
    /// This implements the simplified Tab key path (no per-line selection).
    pub fn toggle_last_tool_result_folded(&mut self) -> Option<bool> {
        for line in self.conversation_lines.iter_mut().rev() {
            if let ConversationLine::ToolResult { folded, .. } = line {
                *folded = !*folded;
                return Some(*folded);
            }
        }
        None
    }

    /// Index of the most recent `ToolResult` line, if any. Exposed for tests
    /// and future per-line selection logic.
    pub fn last_tool_result_index(&self) -> Option<usize> {
        self.conversation_lines
            .iter()
            .rposition(ConversationLine::is_tool_result)
    }

    /// Scroll to the bottom of the conversation.
    pub const fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Scroll up by n lines.
    pub const fn scroll_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
    }

    /// Scroll down by n lines.
    pub const fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    /// Total content lines (estimated). Used for scrollbar sizing.
    fn total_content_lines(&self) -> usize {
        self.conversation_lines.iter().map(estimate_line_height).sum()
    }
}

/// Estimate the number of terminal rows a single `ConversationLine` will
/// occupy in the output area. Always >= 1.
fn estimate_line_height(line: &ConversationLine) -> usize {
    match line {
        ConversationLine::User { content } | ConversationLine::Assistant { content } => {
            // header + body lines + trailing blank
            content.lines().count().max(1) + 2
        }
        ConversationLine::System { content } => content.lines().count().max(1) + 1,
        ConversationLine::Tool { .. } => 1,
        ConversationLine::ToolResult {
            folded,
            args_full,
            result,
            ..
        } => {
            if *folded {
                1
            } else {
                // header + "args:" line + args body + "result:" line + result body
                let args_h = args_full.lines().count().max(1);
                let result_h = result.as_deref().map(|r| r.lines().count().max(1)).unwrap_or(0);
                1 + 1 + args_h + if result_h > 0 { 1 + result_h } else { 0 }
            }
        }
    }
}

/// Build a single-line preview of a raw args string, truncated to `max_chars`
/// characters with the supplied ellipsis. Newlines are collapsed to spaces so
/// the preview stays on one row.
fn build_args_preview(raw: &str, max_chars: usize, ellipsis: &str) -> String {
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

/// Render the TUI layout to a ratatui frame.
pub fn render(frame: &mut Frame, state: &mut TuiState) {
    let area = frame.area();

    // Three-area layout: status bar (1 line) + output (flex) + input (3 lines) + footer (1 line)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Status bar
            Constraint::Min(5),    // Output area
            Constraint::Length(3), // Input area
            Constraint::Length(1), // Footer
        ])
        .split(area);

    // Layout::split always returns exactly as many chunks as constraints (4 here).
    #[allow(clippy::indexing_slicing)]
    {
        state.viewport_height = chunks[1].height;

        // ── Status bar ──
        render_status_bar(frame, chunks[0], state);

        // ── Output area ──
        render_output(frame, chunks[1], state);

        // ── Input area ──
        render_input(frame, chunks[2], state);

        // ── Footer ──
        render_footer(frame, chunks[3]);
    }
}

fn render_status_bar(frame: &mut Frame, area: Rect, state: &TuiState) {
    let title = if state.session_title.is_empty() {
        "(new session)".to_string()
    } else {
        state.session_title.clone()
    };

    let status_text = format!(
        " PRX Chat | {}/{} | {} | {} turns ",
        state.provider, state.model, title, state.turn_count
    );

    let status = Paragraph::new(status_text).style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(status, area);
}

fn render_output(frame: &mut Frame, area: Rect, state: &TuiState) {
    let mut lines: Vec<Line<'_>> = Vec::new();

    for conv_line in &state.conversation_lines {
        render_conversation_line(&mut lines, conv_line, state.ascii_fallback);
    }

    // Virtual scrolling: compute which lines are visible
    let total_lines = lines.len();
    let viewport = area.height as usize;
    let max_scroll = total_lines.saturating_sub(viewport);
    let effective_scroll = max_scroll.saturating_sub(state.scroll_offset);

    let output = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::NONE))
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll as u16, 0));

    frame.render_widget(output, area);

    // Scrollbar
    if total_lines > viewport {
        let mut scrollbar_state = ScrollbarState::new(total_lines)
            .position(effective_scroll)
            .viewport_content_length(viewport);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        frame.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
    }
}

/// Render a single conversation line into the ratatui `lines` buffer.
///
/// Pure function (apart from the &mut push target) — kept outside
/// [`render_output`] so unit tests can drive it with a `Vec<Line<'_>>` sink.
fn render_conversation_line<'a>(lines: &mut Vec<Line<'a>>, conv_line: &'a ConversationLine, ascii: bool) {
    match conv_line {
        ConversationLine::User { content } => {
            lines.push(Line::from(vec![Span::styled(
                "You: ",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            )]));
            for text_line in content.lines() {
                lines.push(Line::from(format!("  {text_line}")));
            }
            lines.push(Line::from(""));
        }
        ConversationLine::Assistant { content } => {
            lines.push(Line::from(vec![Span::styled(
                "PRX: ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )]));
            for text_line in content.lines() {
                lines.push(Line::from(format!("  {text_line}")));
            }
            lines.push(Line::from(""));
        }
        ConversationLine::System { content } => {
            for text_line in content.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {text_line}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            lines.push(Line::from(""));
        }
        ConversationLine::Tool { name, success } => {
            let icon = if *success { "\u{2713}" } else { "\u{2717}" };
            let color = if *success { Color::Green } else { Color::Red };
            lines.push(Line::from(Span::styled(
                format!("  {icon} {name}"),
                Style::default().fg(color),
            )));
        }
        ConversationLine::ToolResult {
            tool_name,
            args_preview,
            args_full,
            result,
            status,
            elapsed_ms,
            folded,
        } => render_tool_result(
            lines,
            tool_name,
            args_preview,
            args_full,
            result.as_deref(),
            *status,
            *elapsed_ms,
            *folded,
            ascii,
        ),
    }
}

/// Render a `ToolResult` card. Folded → 1 line; expanded → header + args block
/// + optional result block.
#[allow(clippy::too_many_arguments)]
fn render_tool_result<'a>(
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
    let (fold_glyph, color) = tool_card_glyph_and_color(status, folded, ascii);
    let header = tool_card_header_text(fold_glyph, tool_name, status, elapsed_ms);

    if folded {
        // Folded: header + short args preview (so the user sees enough to
        // decide whether to expand).
        let summary = if args_preview.is_empty() {
            header
        } else {
            format!("{header} {args_preview}")
        };
        lines.push(Line::from(Span::styled(summary, Style::default().fg(color))));
        return;
    }

    // Expanded view: header (bold), args block, result block.
    lines.push(Line::from(Span::styled(
        header,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "  args:".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    for arg_line in args_full.lines() {
        lines.push(Line::from(format!("    {arg_line}")));
    }
    if let Some(res) = result {
        lines.push(Line::from(Span::styled(
            "  result:".to_string(),
            Style::default().fg(Color::DarkGray),
        )));
        for res_line in res.lines() {
            lines.push(Line::from(format!("    {res_line}")));
        }
    }
}

/// Pick the fold glyph (▸/▾ or >/v) and status color for a tool card.
const fn tool_card_glyph_and_color(status: ToolStatus, folded: bool, ascii: bool) -> (&'static str, Color) {
    let glyph = match (folded, ascii) {
        (true, false) => "\u{25B8}",  // ▸
        (false, false) => "\u{25BE}", // ▾
        (true, true) => ">",
        (false, true) => "v",
    };
    let color = match status {
        ToolStatus::Running => Color::Yellow,
        ToolStatus::Done => Color::Green,
        ToolStatus::Error => Color::Red,
    };
    (glyph, color)
}

/// Build the single-line header text shown on a tool card.
///
/// Example outputs:
/// - `▸ [shell] running...`
/// - `▾ [shell] done (234ms)`
/// - `▸ [shell] error`
fn tool_card_header_text(fold_glyph: &str, tool_name: &str, status: ToolStatus, elapsed_ms: Option<u64>) -> String {
    let status_suffix = match status {
        ToolStatus::Running => "running...".to_string(),
        ToolStatus::Done => elapsed_ms.map_or_else(|| "done".to_string(), |ms| format!("done ({ms}ms)")),
        ToolStatus::Error => elapsed_ms.map_or_else(|| "error".to_string(), |ms| format!("error ({ms}ms)")),
    };
    format!("{fold_glyph} [{tool_name}] {status_suffix}")
}

fn render_input(frame: &mut Frame, area: Rect, state: &TuiState) {
    let input = Paragraph::new(state.input_text.as_str())
        .block(
            Block::default()
                .borders(Borders::TOP)
                .title(" Input ")
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .style(Style::default().fg(Color::White));
    frame.render_widget(input, area);
}

fn render_footer(frame: &mut Frame, area: Rect) {
    let footer = Paragraph::new(" Ctrl+C cancel | Ctrl+D exit | /help commands | ↑↓ history ")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, area);
}

/// Render a tool approval prompt.
pub fn render_approval(frame: &mut Frame, area: Rect, tool_name: &str, args: &str) {
    let approval_text = vec![
        Line::from(vec![
            Span::styled(
                "Tool: ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(tool_name),
        ]),
        Line::from(vec![
            Span::styled("Args: ", Style::default().fg(Color::DarkGray)),
            Span::raw(args),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("[y]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(" approve  "),
            Span::styled("[n]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" deny  "),
            Span::styled("[a]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" always"),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Tool Approval ")
        .border_style(Style::default().fg(Color::Yellow));

    let paragraph = Paragraph::new(approval_text).block(block);
    frame.render_widget(paragraph, area);
}

// ── P2-11: incremental inline-redraw buffer ─────────────────────────────────
//
// `DraftLineBuffer` is the TUI-side implementation of the
// `InlineDraftProtocol` trait defined in `chat/terminal_proto.rs`. It owns
// one `Vec<String>` of rendered lines per active `draft_id` and applies
// fine-grained line replacements in place, rejecting stale versions via a
// shared `DraftVersionTracker`.
//
// This struct deliberately operates only on its own line-buffer storage and
// does NOT touch `TuiState`, `ConversationLine`, the input box, or any
// reasoning UI — those concerns are owned by the second-batch P2-7 / P2-10 /
// P2-12 work. A consumer that wants to splice this back into the visible
// conversation can pull a snapshot via [`DraftLineBuffer::snapshot`].

/// Per-draft line storage used by the incremental inline-redraw protocol.
///
/// Shared between producers (streaming tasks) and the renderer through `Arc`.
/// All mutation is guarded by a single internal mutex so external callers can
/// remain `&self`. The mutex is `parking_lot::Mutex` (no poison, no `.unwrap`).
#[derive(Debug, Default)]
pub struct DraftLineBuffer {
    state: Mutex<DraftLineState>,
    versions: DraftVersionTracker,
}

#[derive(Debug, Default)]
struct DraftLineState {
    /// `draft_id` → ordered line buffer.
    drafts: HashMap<String, Vec<String>>,
}

impl DraftLineBuffer {
    /// Create an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Overwrite the entire line buffer for `draft_id` with the lines
    /// derived from `text`. This is the fallback path used by full-snapshot
    /// `update_draft` deltas (P1-6).
    ///
    /// Stale `version` values (≤ last accepted for this draft) are rejected.
    pub fn set_draft(&self, draft_id: &str, text: &str, version: u64) -> Result<(), LineProtocolError> {
        let current = self.versions.current(draft_id).unwrap_or(0);
        if !self.versions.accept(draft_id, version) {
            return Err(LineProtocolError::StaleVersion { got: version, current });
        }
        let lines: Vec<String> = if text.is_empty() {
            Vec::new()
        } else {
            text.split('\n').map(str::to_owned).collect()
        };
        self.state.lock().drafts.insert(draft_id.to_string(), lines);
        Ok(())
    }

    /// Return a clone of the current line buffer for `draft_id`, if any.
    pub fn snapshot(&self, draft_id: &str) -> Option<Vec<String>> {
        self.state.lock().drafts.get(draft_id).cloned()
    }

    /// Forget all state for a draft (call on finalize/cancel).
    pub fn finalize(&self, draft_id: &str) {
        self.state.lock().drafts.remove(draft_id);
        self.versions.clear(draft_id);
    }

    /// Number of drafts currently tracked. For diagnostics/tests.
    pub fn tracked_count(&self) -> usize {
        self.state.lock().drafts.len()
    }

    /// Synchronous helper used by both `replace_lines` and tests.
    ///
    /// Performs the version check, then applies the line-range replacement
    /// to the in-place vector. Returns the typed protocol error on any
    /// failure (stale version, unknown draft, out-of-bounds range).
    fn replace_lines_sync(
        &self,
        draft_id: &str,
        start_line: usize,
        line_count: usize,
        new_content: &str,
        version: u64,
    ) -> Result<(), LineProtocolError> {
        // 1. Version gate first — a stale delta must not be allowed to
        //    surface a misleading RangeOutOfBounds error.
        let current = self.versions.current(draft_id).unwrap_or(0);
        if !self.versions.accept(draft_id, version) {
            return Err(LineProtocolError::StaleVersion { got: version, current });
        }
        // 2. Look up the draft buffer; must exist (caller should have
        //    seeded it via `set_draft` or a prior insertion at start=0).
        let mut guard = self.state.lock();
        let Some(buf) = guard.drafts.get_mut(draft_id) else {
            return Err(LineProtocolError::UnknownDraft(draft_id.to_string()));
        };
        // 3. Apply the splice. On error the buffer is left unmodified;
        //    note that the version high-water mark has *already* advanced
        //    above, which is intentional — the sender's intent has been
        //    seen, even though it could not be carried out.
        apply_line_replacement(buf, start_line, line_count, new_content)
    }
}

#[async_trait]
impl InlineDraftProtocol for DraftLineBuffer {
    async fn replace_lines(
        &self,
        _role: &str,
        draft_id: &str,
        start_line: usize,
        line_count: usize,
        new_content: &str,
        version: u64,
    ) -> Result<(), LineProtocolError> {
        self.replace_lines_sync(draft_id, start_line, line_count, new_content, version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_state_scroll() {
        let mut state = TuiState::new("test", "model");
        state.push_user_message("hello");
        state.push_assistant_message("world");
        assert_eq!(state.turn_count, 1); // only user messages count
        assert_eq!(state.scroll_offset, 0);

        state.scroll_up(5);
        assert_eq!(state.scroll_offset, 5);
        state.scroll_down(3);
        assert_eq!(state.scroll_offset, 2);
        state.scroll_to_bottom();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn conversation_line_variants() {
        let user = ConversationLine::User {
            content: "test".to_string(),
        };
        assert!(matches!(user, ConversationLine::User { .. }));

        let tool = ConversationLine::Tool {
            name: "shell".to_string(),
            success: true,
        };
        assert!(matches!(tool, ConversationLine::Tool { .. }));
        assert!(!tool.is_tool_result());

        let tr = ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: "ls".to_string(),
            args_full: "{\"command\":\"ls\"}".to_string(),
            result: None,
            status: ToolStatus::Running,
            elapsed_ms: None,
            folded: true,
        };
        assert!(tr.is_tool_result());
    }

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
                // Short args fit in the preview verbatim (collapsed whitespace).
                assert_eq!(args_preview, r#"{"command":"ls -la /tmp"}"#);
            }
            other => panic!("test: expected ToolResult, got {other:?}"),
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
            args_preview: "ls".to_string(),
            args_full: "ls".to_string(),
            result: None,
            status: ToolStatus::Running,
            elapsed_ms: None,
            folded: true,
        };
        render_conversation_line(&mut lines, &card, false);
        assert_eq!(lines.len(), 1, "folded card renders to 1 line");
        let rendered: String = lines
            .first()
            .expect("test: at least one line")
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(rendered.contains("\u{25B8}"), "uses ▸ glyph: {rendered}");
        assert!(rendered.contains("[shell]"));
        assert!(rendered.contains("running..."));
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
        // header + "args:" + 1 args body + "result:" + 2 result body = 6
        assert_eq!(lines.len(), 6, "expanded card line count: {}", lines.len());
        let join = |i: usize| -> String {
            lines
                .get(i)
                .expect("test: line idx")
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert!(join(0).contains("\u{25BE}"), "uses ▾ glyph");
        assert!(join(0).contains("done (234ms)"));
        assert!(join(1).contains("args:"));
        assert!(join(2).contains("ls -la /tmp"));
        assert!(join(3).contains("result:"));
        assert!(join(4).contains("total 24"));
    }

    #[test]
    fn render_tool_card_status_glyphs_and_colors() {
        // Running → yellow + ▸
        let (g, c) = tool_card_glyph_and_color(ToolStatus::Running, true, false);
        assert_eq!(g, "\u{25B8}");
        assert_eq!(c, Color::Yellow);
        // Done → green + ▾ when expanded
        let (g, c) = tool_card_glyph_and_color(ToolStatus::Done, false, false);
        assert_eq!(g, "\u{25BE}");
        assert_eq!(c, Color::Green);
        // Error → red
        let (_, c) = tool_card_glyph_and_color(ToolStatus::Error, true, false);
        assert_eq!(c, Color::Red);
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
        // Render in ASCII mode → glyph is ">"
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
        assert!(rendered.starts_with('>'), "ASCII fold glyph: {rendered}");
    }

    #[test]
    fn header_text_for_each_status() {
        assert_eq!(
            tool_card_header_text("\u{25B8}", "shell", ToolStatus::Running, None),
            "\u{25B8} [shell] running..."
        );
        assert_eq!(
            tool_card_header_text("\u{25BE}", "shell", ToolStatus::Done, Some(234)),
            "\u{25BE} [shell] done (234ms)"
        );
        assert_eq!(
            tool_card_header_text("\u{25B8}", "shell", ToolStatus::Error, None),
            "\u{25B8} [shell] error"
        );
    }

    #[test]
    fn total_content_lines_counts_folded_vs_expanded() {
        let mut state = TuiState::new("p", "m");
        state.push_tool_result_started("shell", "x");
        let folded_total = state.total_content_lines();
        state.toggle_last_tool_result_folded();
        let expanded_total = state.total_content_lines();
        assert!(
            expanded_total > folded_total,
            "expanded card takes more rows: {expanded_total} vs {folded_total}"
        );
    }

    // ── P2-11: DraftLineBuffer tests ─────────────────────────────────────────

    fn lines(slice: &[&str]) -> Vec<String> {
        slice.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn draft_buffer_set_draft_seeds_lines() {
        let buf = DraftLineBuffer::new();
        buf.set_draft("d1", "a\nb\nc", 1).expect("test: seed v1");
        assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "b", "c"])));
        assert_eq!(buf.tracked_count(), 1);
    }

    #[test]
    fn draft_buffer_replace_middle_line() {
        let buf = DraftLineBuffer::new();
        buf.set_draft("d1", "a\nb\nc\nd", 1).expect("test: seed");
        // Replace just line 2 (index 2 = "c") with two new lines.
        buf.replace_lines_sync("d1", 2, 1, "X\nY", 2).expect("test: splice");
        assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "b", "X", "Y", "d"])));
    }

    #[test]
    fn draft_buffer_insert_with_zero_count() {
        let buf = DraftLineBuffer::new();
        buf.set_draft("d1", "a\nb\nc", 1).expect("test: seed");
        // count=0 → pure insertion before line at index 1.
        buf.replace_lines_sync("d1", 1, 0, "INS", 2).expect("test: insert");
        assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "INS", "b", "c"])));
    }

    #[test]
    fn draft_buffer_out_of_bounds_is_safe_error() {
        let buf = DraftLineBuffer::new();
        buf.set_draft("d1", "a\nb", 1).expect("test: seed");
        let err = buf
            .replace_lines_sync("d1", 5, 1, "X", 2)
            .expect_err("test: must reject");
        assert!(matches!(err, LineProtocolError::RangeOutOfBounds { .. }));
        // Buffer unchanged after the failed splice.
        assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "b"])));
    }

    #[test]
    fn draft_buffer_unknown_draft_rejected() {
        let buf = DraftLineBuffer::new();
        let err = buf
            .replace_lines_sync("ghost", 0, 1, "X", 1)
            .expect_err("test: unknown draft");
        assert!(matches!(err, LineProtocolError::UnknownDraft(ref id) if id == "ghost"));
    }

    #[test]
    fn draft_buffer_stale_version_rejected() {
        let buf = DraftLineBuffer::new();
        buf.set_draft("d1", "a\nb\nc", 5).expect("test: seed v5");
        // Lower version must be dropped — buffer untouched.
        let err = buf.replace_lines_sync("d1", 1, 1, "X", 3).expect_err("test: stale");
        match err {
            LineProtocolError::StaleVersion { got, current } => {
                assert_eq!(got, 3);
                assert_eq!(current, 5);
            }
            other => panic!("test: expected StaleVersion, got {other:?}"),
        }
        assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "b", "c"])));
        // Exact duplicate also rejected.
        assert!(buf.replace_lines_sync("d1", 1, 1, "X", 5).is_err());
        // Newer version accepted and applied.
        buf.replace_lines_sync("d1", 1, 1, "X", 6).expect("test: v6");
        assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "X", "c"])));
    }

    #[test]
    fn draft_buffer_set_then_replace_mixed_flow() {
        // Mirrors a real producer that alternates between full snapshot
        // (P1-6 `update_draft`) and fine-grained edits (P2-11).
        let buf = DraftLineBuffer::new();
        buf.set_draft("d1", "row1\nrow2\nrow3", 1).expect("test: v1");
        buf.replace_lines_sync("d1", 1, 1, "row2-edited", 2).expect("test: v2");
        assert_eq!(buf.snapshot("d1"), Some(lines(&["row1", "row2-edited", "row3"])));
        // A later full snapshot at higher version overwrites everything.
        buf.set_draft("d1", "a\nb", 3).expect("test: v3 full");
        assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "b"])));
    }

    #[test]
    fn draft_buffer_finalize_clears_state() {
        let buf = DraftLineBuffer::new();
        buf.set_draft("d1", "a\nb", 1).expect("test: seed");
        assert_eq!(buf.tracked_count(), 1);
        buf.finalize("d1");
        assert_eq!(buf.tracked_count(), 0);
        assert_eq!(buf.snapshot("d1"), None);
        // After finalize, low versions are fresh again.
        buf.set_draft("d1", "fresh", 1).expect("test: reseed");
        assert_eq!(buf.snapshot("d1"), Some(lines(&["fresh"])));
    }

    #[tokio::test]
    async fn draft_buffer_implements_trait_for_async_callers() {
        let buf = DraftLineBuffer::new();
        buf.set_draft("d1", "a\nb\nc", 1).expect("test: seed");
        let ch: &dyn InlineDraftProtocol = &buf;
        ch.replace_lines("assistant", "d1", 1, 1, "MID", 2)
            .await
            .expect("test: trait call");
        assert_eq!(buf.snapshot("d1"), Some(lines(&["a", "MID", "c"])));
    }
}
