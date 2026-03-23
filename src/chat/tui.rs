//! TUI layout and rendering for `prx chat` using ratatui.
//!
//! Provides a three-area layout:
//! - Status bar (top): provider/model, session info, turn count
//! - Output area (middle): scrollable conversation display
//! - Input area (bottom): prompt text
//!
//! Gated behind the `terminal-tui` feature.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};

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
}
