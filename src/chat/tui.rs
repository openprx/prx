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
}

/// A single line in the conversation display.
#[derive(Clone, Debug)]
pub struct ConversationLine {
    pub role: ConversationRole,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationRole {
    User,
    Assistant,
    System,
    Tool { name: String, success: bool },
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
        }
    }

    /// Add a user message to the conversation display.
    pub fn push_user_message(&mut self, content: &str) {
        self.conversation_lines.push(ConversationLine {
            role: ConversationRole::User,
            content: content.to_string(),
        });
        self.turn_count += 1;
        self.scroll_to_bottom();
    }

    /// Add an assistant message to the conversation display.
    pub fn push_assistant_message(&mut self, content: &str) {
        self.conversation_lines.push(ConversationLine {
            role: ConversationRole::Assistant,
            content: content.to_string(),
        });
        self.scroll_to_bottom();
    }

    /// Add a tool call indicator.
    pub fn push_tool_call(&mut self, name: &str, success: bool) {
        self.conversation_lines.push(ConversationLine {
            role: ConversationRole::Tool {
                name: name.to_string(),
                success,
            },
            content: String::new(),
        });
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

    /// Total content lines (estimated).
    fn total_content_lines(&self) -> usize {
        self.conversation_lines
            .iter()
            .map(|line| {
                // Rough estimate: each line of content plus blank line between turns
                line.content.lines().count().max(1) + 1
            })
            .sum()
    }
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
        match &conv_line.role {
            ConversationRole::User => {
                lines.push(Line::from(vec![Span::styled(
                    "You: ",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                )]));
                for text_line in conv_line.content.lines() {
                    lines.push(Line::from(format!("  {text_line}")));
                }
                lines.push(Line::from(""));
            }
            ConversationRole::Assistant => {
                lines.push(Line::from(vec![Span::styled(
                    "PRX: ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )]));
                for text_line in conv_line.content.lines() {
                    lines.push(Line::from(format!("  {text_line}")));
                }
                lines.push(Line::from(""));
            }
            ConversationRole::System => {
                for text_line in conv_line.content.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {text_line}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                lines.push(Line::from(""));
            }
            ConversationRole::Tool { name, success } => {
                let icon = if *success { "✓" } else { "✗" };
                let color = if *success { Color::Green } else { Color::Red };
                lines.push(Line::from(Span::styled(
                    format!("  {icon} {name}"),
                    Style::default().fg(color),
                )));
            }
        }
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
    fn conversation_line_roles() {
        let user = ConversationLine {
            role: ConversationRole::User,
            content: "test".to_string(),
        };
        assert_eq!(user.role, ConversationRole::User);

        let tool = ConversationLine {
            role: ConversationRole::Tool {
                name: "shell".to_string(),
                success: true,
            },
            content: String::new(),
        };
        assert!(matches!(tool.role, ConversationRole::Tool { .. }));
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
