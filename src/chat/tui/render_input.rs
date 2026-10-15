use super::layout::{BottomChromeView, session_footer_has_sessions};
use super::render_chrome::{render_sessions_list_lines, sessions_navigation_hint};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Build the prompt's input-target indicator span + its display width (v1.1b).
///
/// The target is dual-encoded with **colour AND text/glyph** so it is never
/// colour-only (colour-blind / no-color terminals still read the target):
/// - [`FocusTarget::Main`] → dim cyan `› ` (`> ` in ASCII fallback).
/// - [`FocusTarget::Session`] → blue bold `<kind> #N ▸ ` (or `<kind> #N > `
///   under ASCII fallback). The literal kind + sequence text carries the meaning
///   even with styling stripped.
///
/// Returns the [`Span`] plus its column width so the continuation rows and the
/// terminal cursor can align under the typed text.
pub(super) fn prompt_indicator(
    focus: crate::chat::sessions::FocusTarget,
    ascii: bool,
    session_kind: Option<&str>,
) -> (Span<'static>, usize) {
    match focus {
        crate::chat::sessions::FocusTarget::Main => {
            let marker = if ascii { "> " } else { "› " };
            let span = Span::styled(marker, Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM));
            (span, 2)
        }
        crate::chat::sessions::FocusTarget::Session { seq } => {
            let arrow = if ascii { ">" } else { "\u{25B8}" }; // ▸
            let kind = session_kind
                .map(str::trim)
                .filter(|kind| !kind.is_empty())
                .unwrap_or(crate::chat::sessions::model::ManagedKind::Agent.as_str());
            let label = format!("{kind} #{seq} {arrow} ");
            let width = UnicodeWidthStr::width(label.as_str());
            let span = Span::styled(label, Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD));
            (span, width)
        }
        crate::chat::sessions::FocusTarget::Transcript => {
            let arrow = if ascii { ">" } else { "\u{25B8}" }; // ▸
            let label = format!("transcript {arrow} ");
            let width = UnicodeWidthStr::width(label.as_str());
            let span = Span::styled(label, Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD));
            (span, width)
        }
        crate::chat::sessions::FocusTarget::Approval => {
            let arrow = if ascii { ">" } else { "\u{25B8}" }; // ▸
            let label = format!("approval {arrow} ");
            let width = UnicodeWidthStr::width(label.as_str());
            let span = Span::styled(label, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
            (span, width)
        }
        crate::chat::sessions::FocusTarget::Diff => {
            let arrow = if ascii { ">" } else { "\u{25B8}" }; // ▸
            let label = format!("diff {arrow} ");
            let width = UnicodeWidthStr::width(label.as_str());
            let span = Span::styled(label, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD));
            (span, width)
        }
        crate::chat::sessions::FocusTarget::Worker { sequence } => {
            let arrow = if ascii { ">" } else { "\u{25B8}" }; // ▸
            let label = format!("worker w#{sequence} {arrow} ");
            let width = UnicodeWidthStr::width(label.as_str());
            let span = Span::styled(label, Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD));
            (span, width)
        }
    }
}

pub(super) fn focused_session_kind<V: BottomChromeView + ?Sized>(state: &V, seq: u64) -> Option<&str> {
    if let Some(view) = state.active_session_view().filter(|view| view.seq == seq) {
        return Some(view.kind.as_str());
    }
    state
        .sessions_entries()
        .iter()
        .find(|entry| entry.seq == seq)
        .map(|entry| entry.kind)
}

pub(super) struct WrappedInputRow {
    pub(super) logical_idx: usize,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) text: String,
    pub(super) first_for_logical: bool,
}

pub(super) fn input_prompt_width<V: BottomChromeView + ?Sized>(state: &V) -> usize {
    let session_kind = state
        .focus()
        .session_seq()
        .and_then(|seq| focused_session_kind(state, seq));
    let (_, width) = prompt_indicator(state.focus(), state.ascii_fallback(), session_kind);
    width
}

pub(super) fn input_content_width<V: BottomChromeView + ?Sized>(state: &V, total_width: u16) -> usize {
    usize::from(total_width)
        .saturating_sub(input_prompt_width(state))
        .max(1)
}

pub(super) fn input_visual_rows_for_width<V: BottomChromeView + ?Sized>(state: &V, total_width: u16) -> usize {
    let content_width = input_content_width(state, total_width);
    state
        .input()
        .display_lines()
        .iter()
        .map(|line| wrap_line_ranges(line, content_width).len())
        .sum::<usize>()
        .max(1)
}

pub(super) fn wrap_input_rows(display_lines: &[String], content_width: usize) -> Vec<WrappedInputRow> {
    let mut rows = Vec::new();
    for (logical_idx, line) in display_lines.iter().enumerate() {
        for (range_idx, (start, end)) in wrap_line_ranges(line, content_width).into_iter().enumerate() {
            rows.push(WrappedInputRow {
                logical_idx,
                start,
                end,
                text: line.get(start..end).unwrap_or("").to_string(),
                first_for_logical: range_idx == 0,
            });
        }
    }
    if rows.is_empty() {
        rows.push(WrappedInputRow {
            logical_idx: 0,
            start: 0,
            end: 0,
            text: String::new(),
            first_for_logical: true,
        });
    }
    rows
}

pub(super) fn wrap_line_ranges(line: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    if line.is_empty() {
        return vec![(0, 0)];
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut cols = 0usize;
    for (idx, ch) in line.char_indices() {
        let end = idx.saturating_add(ch.len_utf8());
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cols > 0 && cols.saturating_add(ch_width) > width {
            ranges.push((start, idx));
            start = idx;
            cols = 0;
        }
        cols = cols.saturating_add(ch_width);
        if cols >= width && end < line.len() {
            ranges.push((start, end));
            start = end;
            cols = 0;
        }
    }
    if start <= line.len() {
        ranges.push((start, line.len()));
    }
    ranges
}

pub(super) fn render_input<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) {
    // Compose prompt lines: the first row gets the input-target indicator
    // (v1.1b), continuation rows are aligned with blanks of the same width.
    let input_ref = state.input();
    let session_kind = state
        .focus()
        .session_seq()
        .and_then(|seq| focused_session_kind(state, seq));
    let (prompt_span, prompt_width) = prompt_indicator(state.focus(), state.ascii_fallback(), session_kind);
    let continuation = " ".repeat(prompt_width);
    let max_visible_rows = area.height.saturating_sub(1).max(1) as usize;
    let content_width = usize::from(area.width).saturating_sub(prompt_width).max(1);
    let display_lines = input_ref.display_lines();
    let wrapped_rows = wrap_input_rows(&display_lines, content_width);
    let cursor_line = input_ref.cursor.0.min(input_ref.lines.len().saturating_sub(1));
    let storage_cursor_offset = input_ref
        .lines
        .get(cursor_line)
        .map_or(0, |line| input_ref.cursor.1.min(line.len()));
    let cursor_offset = input_ref.display_cursor_offset(cursor_line, storage_cursor_offset);
    let cursor_visual_row = wrapped_rows
        .iter()
        .position(|row| row.logical_idx == cursor_line && cursor_offset >= row.start && cursor_offset <= row.end)
        .unwrap_or_else(|| wrapped_rows.len().saturating_sub(1));
    let first_visible_line = cursor_visual_row.saturating_add(1).saturating_sub(max_visible_rows);
    let rendered_lines: Vec<Line<'_>> = wrapped_rows
        .iter()
        .enumerate()
        .skip(first_visible_line)
        .take(max_visible_rows)
        .map(|(_visual_idx, row)| {
            let prefix = if row.logical_idx == 0 && row.first_for_logical {
                prompt_span.clone()
            } else {
                Span::raw(continuation.clone())
            };
            Line::from(vec![prefix, Span::raw(row.text.clone())])
        })
        .collect();

    let input_title = input_ref.reverse_search_title().unwrap_or_else(|| {
        if input_ref.truncated {
            " Input - max 32768 bytes, extra ignored ".to_string()
        } else {
            " Input ".to_string()
        }
    });
    let input = Paragraph::new(Text::from(rendered_lines))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .title(input_title.as_str())
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .style(Style::default().fg(Color::White));
    frame.render_widget(input, area);

    // Place the terminal cursor at the visual cursor location inside the box.
    // Borders::TOP consumes the first row of `area`, so the body starts at
    // `area.y + 1` and the prompt prefix takes `prompt_width` columns (the
    // input-target indicator width, which varies between `main` and `agent #N`).
    let cursor_visible_row = cursor_visual_row.saturating_sub(first_visible_line);
    if cursor_line < input_ref.lines.len() && cursor_visible_row < max_visible_rows {
        let row_text = display_lines.get(cursor_line).map(String::as_str).unwrap_or("");
        // Width-aware column: count *display* columns (not char count) up to
        // the byte offset. CJK and wide East-Asian glyphs occupy 2 columns,
        // so a `chars().count()` here would leave the cursor mid-glyph and
        // give the impression that input is broken. `unicode-width` matches
        // ratatui's own width algorithm for `Paragraph`.
        let visual_col: usize = wrapped_rows
            .get(cursor_visual_row)
            .and_then(|row| row_text.get(row.start..cursor_offset.min(row.end).min(row_text.len())))
            .map_or(0, UnicodeWidthStr::width);
        let col_offset = u16::try_from(visual_col).unwrap_or(u16::MAX);
        let prefix_cols: u16 = u16::try_from(prompt_width).unwrap_or(2);
        let row_offset = u16::try_from(cursor_visible_row).unwrap_or(u16::MAX);
        let cx = area.x.saturating_add(prefix_cols).saturating_add(col_offset);
        let cy = area.y.saturating_add(1).saturating_add(row_offset);
        // Only place cursor if it falls within the box bounds.
        if cx < area.x.saturating_add(area.width) && cy < area.y.saturating_add(area.height) {
            frame.set_cursor_position((cx, cy));
        }
    }
}

pub(super) fn footer_text(ascii: bool, show_session_hint: bool) -> String {
    // Claude Code style: dim gray, middle-dot separators, action-oriented
    // hints rather than key/label pairs.
    // P6b2: Ctrl+G remains PRX's sessions switcher; Claude-style external
    // editor parity is available through the alternate Ctrl+X Ctrl+E chord.
    let sep = if ascii { " | " } else { " \u{00B7} " };
    let mut parts = Vec::from([
        if ascii {
            "Up/Down scroll"
        } else {
            "\u{2191}/\u{2193} scroll"
        },
        "Home/PageUp history",
        "Ctrl+P/N history",
        "Ctrl+G sessions",
        "Ctrl+O transcript",
        "/copy latest",
        "drag select/copy",
        "Shift+Enter newline",
        "Ctrl+X Ctrl+E edit",
        "Tab fold",
        "Esc cancel",
    ]);
    if show_session_hint {
        parts.insert(1, sessions_navigation_hint(ascii, false).trim());
    }
    format!(" {} ", parts.join(sep))
}

pub(super) fn render_footer(frame: &mut Frame, area: Rect, ascii: bool, show_session_hint: bool) {
    let footer = Paragraph::new(footer_text(ascii, show_session_hint)).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, area);
}

pub(super) fn render_session_list_footer<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    area: Rect,
    state: &V,
) -> bool {
    if !session_footer_has_sessions(state) {
        return false;
    }
    let lines = render_sessions_list_lines(state, area.width, usize::from(area.height));
    if lines.is_empty() {
        return false;
    }
    let widget = Paragraph::new(Text::from(lines)).style(Style::default().bg(Color::Black));
    frame.render_widget(widget, area);
    true
}

pub(super) fn render_fullscreen_footer<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    area: Rect,
    state: &V,
    show_new_output_below: bool,
) {
    if show_new_output_below {
        let sep = if state.ascii_fallback() { " | " } else { " \u{00B7} " };
        let footer = Paragraph::new(format!(
            " New output below{sep}End jumps to tail{sep}Home top{sep}PageUp/PageDown scroll "
        ))
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        frame.render_widget(footer, area);
        return;
    }
    if render_session_list_footer(frame, area, state) {
        return;
    }
    render_footer(frame, area, state.ascii_fallback(), session_footer_has_sessions(state));
}

/// Render a tool approval prompt.
pub fn render_approval(
    frame: &mut Frame,
    area: Rect,
    tool_name: &str,
    args: &str,
    selected_approval: bool,
    ascii: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let marker = if ascii { ">" } else { "\u{25B8}" };
    let approve_marker = if selected_approval { marker } else { " " };
    let deny_marker = if selected_approval { " " } else { marker };
    let approve_style = if selected_approval {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    };
    let deny_style = if selected_approval {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD)
    };
    let select_hint = if ascii { "Left/Right" } else { "\u{2190}/\u{2192}" };
    let approval_text = vec![
        Line::from(vec![
            Span::styled(
                format!("{marker} Tool: "),
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
            Span::styled(format!("{approve_marker} [y]"), approve_style),
            Span::raw(" approve  "),
            Span::styled(format!("{deny_marker} [n]"), deny_style),
            Span::raw(" deny  "),
            Span::styled("[Esc]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" deny"),
        ]),
        Line::from(Span::styled(
            format!("{select_hint} select · Enter confirm"),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let selected_label = if selected_approval { "approve" } else { "deny" };
    let title = if ascii {
        format!(" Tool Approval - selected: {selected_label} - Left/Right select - Enter confirm ")
    } else {
        format!(" Tool Approval - selected: {selected_label} - ←/→ select - Enter confirm ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Yellow));

    let paragraph = Paragraph::new(approval_text).block(block).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}
