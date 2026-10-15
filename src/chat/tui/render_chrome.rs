use super::keys::{bottom_chrome_session_entries_with_workers, bottom_list_selection_index};
use super::layout::{BOTTOM_CHROME_MIN_HEIGHT, BottomChromeView, fullscreen_bottom_chrome_height};
use super::slash_menu::{SlashMenuEntry, SlashMenuState};
use super::views::{focus_active_entry_seq, switcher_entry_display_id};
use super::{ConversationLine, ToolStatus};
use crate::agent::turn_contracts::ChatMode;
use crate::chat::action::{MainQueueStatus, ProviderWorkerRowState, ProviderWorkerStatus, ProviderWorkerStatusRow};
use crate::chat::session::MainSessionTokenUsageSummary;
use crate::security::AutonomyLevel;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) fn render_fullscreen_overlays<V: BottomChromeView + ?Sized>(frame: &mut Frame, frame_area: Rect, state: &V) {
    if let Some(picker) = state.saved_session_picker() {
        let area = centered_overlay_rect(frame_area, 92, 85, BOTTOM_CHROME_MIN_HEIGHT);
        frame.render_widget(Clear, area);
        render_saved_session_picker(frame, area, picker, state.ascii_fallback());
        return;
    }
    if let Some(menu) = state.slash_menu() {
        let area = slash_menu_overlay_rect(frame_area, menu, fullscreen_bottom_chrome_height(state));
        frame.render_widget(Clear, area);
        render_slash_menu(frame, area, menu, state.ascii_fallback());
        return;
    }
    if let Some(switcher) = state.switcher() {
        let area = centered_overlay_rect(frame_area, 92, 85, BOTTOM_CHROME_MIN_HEIGHT);
        frame.render_widget(Clear, area);
        render_switcher(frame, area, switcher, state.ascii_fallback());
    }
}

pub(super) fn centered_overlay_rect(frame_area: Rect, width_pct: u16, height_pct: u16, min_height: u16) -> Rect {
    let width = frame_area
        .width
        .saturating_mul(width_pct.min(100))
        .checked_div(100)
        .unwrap_or(frame_area.width)
        .max(1);
    let height = frame_area
        .height
        .saturating_mul(height_pct.min(100))
        .checked_div(100)
        .unwrap_or(frame_area.height)
        .max(min_height.min(frame_area.height))
        .min(frame_area.height);
    let x = frame_area.x.saturating_add(frame_area.width.saturating_sub(width) / 2);
    let y = frame_area
        .y
        .saturating_add(frame_area.height.saturating_sub(height) / 2);
    Rect { x, y, width, height }
}

pub(super) fn slash_menu_overlay_rect(frame_area: Rect, menu: &SlashMenuState, bottom_chrome_height: u16) -> Rect {
    let horizontal_margin = 2u16.min(frame_area.width.saturating_div(2));
    let width = frame_area
        .width
        .saturating_sub(horizontal_margin.saturating_mul(2))
        .clamp(1, 80);
    let visible_items = u16::try_from(menu.len().clamp(1, 10)).unwrap_or(10);
    let height = visible_items
        .saturating_add(2)
        .min(frame_area.height.saturating_sub(bottom_chrome_height).max(1));
    let x = frame_area.x.saturating_add(horizontal_margin);
    let bottom_y = frame_area.y.saturating_add(
        frame_area
            .height
            .saturating_sub(bottom_chrome_height.min(frame_area.height)),
    );
    let y = bottom_y.saturating_sub(height);
    Rect { x, y, width, height }
}

pub(super) fn truncate_chars_with_ellipsis(input: &str, max_width: u16, ascii: bool) -> String {
    let max = usize::from(max_width);
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(input) <= max {
        return input.to_string();
    }
    let ellipsis = if ascii { "..." } else { "\u{2026}" };
    let ellipsis_cols = UnicodeWidthStr::width(ellipsis);
    if max <= ellipsis_cols {
        return ellipsis.chars().take(max).collect();
    }
    let keep = max.saturating_sub(ellipsis_cols);
    let mut width = 0usize;
    let mut truncated = String::new();
    for ch in input.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width.saturating_add(ch_width) > keep {
            break;
        }
        width = width.saturating_add(ch_width);
        truncated.push(ch);
    }
    truncated.push_str(ellipsis);
    truncated
}

pub(super) const fn session_active_marker(active: bool, ascii: bool) -> &'static str {
    if !active {
        " "
    } else if ascii {
        ">"
    } else {
        "\u{25B8}"
    }
}

pub(super) const SPINNER_FRAME_MS: u64 = 150;
pub(super) const ASCII_SPINNER_FRAMES: [&str; 4] = ["-", "\\", "|", "/"];
pub(super) const UNICODE_SPINNER_FRAMES: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];

pub(super) fn current_animation_tick() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis() / u128::from(SPINNER_FRAME_MS)).unwrap_or(u64::MAX)
        })
}

pub(super) fn spinner_frame_for_tick(ascii: bool, tick: u64) -> &'static str {
    let frames = if ascii {
        &ASCII_SPINNER_FRAMES
    } else {
        &UNICODE_SPINNER_FRAMES
    };
    let idx = usize::try_from(tick % u64::try_from(frames.len()).unwrap_or(1)).unwrap_or(0);
    frames.get(idx).copied().unwrap_or("-")
}

pub(super) fn session_status_glyph_for_tick(
    entry: &crate::chat::sessions::SwitcherEntry,
    ascii: bool,
    tick: u64,
) -> &'static str {
    if entry.is_transcript() {
        return if ascii { "[T]" } else { "T" };
    }
    if entry.status == "running" {
        return spinner_frame_for_tick(ascii, tick);
    }
    if ascii {
        match entry.status {
            "needs-input" => "[?]",
            "completed" => "[x]",
            "cancelled" => "[-]",
            _ => "[!]",
        }
    } else {
        entry.status_glyph()
    }
}

pub(super) fn session_status_glyph(entry: &crate::chat::sessions::SwitcherEntry, ascii: bool) -> &'static str {
    session_status_glyph_for_tick(entry, ascii, current_animation_tick())
}

pub(super) fn render_sessions_list_entry_line(
    entry: &crate::chat::sessions::SwitcherEntry,
    active_seq: Option<u64>,
    ascii: bool,
    width: u16,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let active = active_seq == Some(entry.seq);
    let marker = session_active_marker(active, ascii);
    let glyph = session_status_glyph(entry, ascii);
    let idle = if entry.idle_warning {
        if ascii { " [idle]" } else { " ⚠ idle" }
    } else {
        ""
    };
    let usage = entry
        .token_usage_summary()
        .and_then(crate::chat::session::format_session_token_usage_inline)
        .unwrap_or_else(|| "0 tok | $0.0000".to_string());
    let sep = if ascii { " | " } else { " · " };
    let display_id = switcher_entry_display_id(entry);
    let prefix = format!(
        "{marker}{glyph} {display_id} {} {} {}{}{}{}",
        entry.kind,
        entry.origin,
        entry.status,
        sep,
        entry.elapsed_label(),
        idle
    );
    let mut text = format!("{prefix}{sep}{usage}");
    if !entry.title.is_empty() {
        text.push_str(sep);
        text.push_str(&entry.title);
    }
    let text = truncate_chars_with_ellipsis(&text, width, ascii);
    let style = if active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else if entry.is_terminal() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan)
    };
    Line::from(Span::styled(text, style))
}

pub(super) fn render_main_session_list_line<V: BottomChromeView + ?Sized>(state: &V, width: u16) -> Line<'static> {
    let ascii = state.ascii_fallback();
    let active = matches!(state.focus(), crate::chat::sessions::FocusTarget::Main);
    let marker = session_active_marker(active, ascii);
    let sep = if ascii { " | " } else { " · " };
    let usage = render_main_token_usage(state.token_usage_summary());
    let text = format!(
        "{marker} main chat {}{}{}{}{}/{}",
        if active { "active" } else { "ready" },
        sep,
        usage,
        sep,
        crate::providers::redact_endpoint_userinfo(state.provider()),
        state.model()
    );
    let text = truncate_chars_with_ellipsis(&text, width, ascii);
    let style = if active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Line::from(Span::styled(text, style))
}

pub(super) const fn sessions_navigation_hint(ascii: bool, child_focus: bool) -> &'static str {
    match (ascii, child_focus) {
        (true, true) => " Up/Down scroll | Left/Right switch | Esc back ",
        (false, true) => " \u{2191}/\u{2193} scroll \u{00B7} \u{2190}/\u{2192} switch \u{00B7} Esc back ",
        (true, false) => " Left/Right session ",
        (false, false) => " \u{2190}/\u{2192} session ",
    }
}

pub(super) fn render_sessions_navigation_hint_line(
    ascii: bool,
    focus: crate::chat::sessions::FocusTarget,
    width: u16,
) -> Line<'static> {
    let text = truncate_chars_with_ellipsis(sessions_navigation_hint(ascii, focus.is_child_view()), width, ascii);
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

pub(super) fn render_sessions_list_lines<V: BottomChromeView + ?Sized>(
    state: &V,
    width: u16,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let summary = state.sessions_status();
    let focus = state.focus();
    let entries =
        bottom_chrome_session_entries_with_workers(state.sessions_entries(), state.provider_worker_status(), focus);
    let ascii = state.ascii_fallback();
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }
    if entries.is_empty() {
        if summary.is_empty() {
            return Vec::new();
        }
        return vec![render_main_session_list_line(state, width)];
    }
    let active_seq = focus_active_entry_seq(focus);
    let total = entries.len().saturating_add(1);
    let list_rows = max_rows.saturating_sub(1).max(1);
    let target_idx = bottom_list_selection_index(&entries, focus).min(total.saturating_sub(1));
    let start = if total <= list_rows {
        0
    } else {
        target_idx.saturating_add(1).saturating_sub(list_rows)
    };
    let mut lines = Vec::new();
    for idx in start..total.min(start.saturating_add(list_rows)) {
        if idx == 0 {
            lines.push(render_main_session_list_line(state, width));
        } else if let Some(entry) = entries.get(idx.saturating_sub(1)) {
            lines.push(render_sessions_list_entry_line(entry, active_seq, ascii, width));
        }
    }
    if lines.len() < max_rows {
        lines.push(render_sessions_navigation_hint_line(ascii, focus, width));
    }
    lines
}

pub(super) fn active_session_visible_lines(
    view: &crate::chat::sessions::ActiveSessionView,
    visible_rows: usize,
) -> Vec<String> {
    if visible_rows == 0 || view.lines.is_empty() {
        return Vec::new();
    }
    let offset = view.scroll_offset.min(view.lines.len().saturating_sub(visible_rows));
    let end = view.lines.len().saturating_sub(offset).min(view.lines.len());
    let start = end.saturating_sub(visible_rows);
    view.lines
        .get(start..end)
        .map(|lines| lines.to_vec())
        .unwrap_or_default()
}

pub(super) fn render_active_session_view(
    frame: &mut Frame,
    area: Rect,
    view: &crate::chat::sessions::ActiveSessionView,
    ascii: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut lines: Vec<Line<'_>> = Vec::new();
    let max_width = area.width.saturating_sub(1);
    let marker = if ascii { ">" } else { "\u{25B8}" };
    let prefix = if view.kind == crate::chat::sessions::model::ManagedKind::Transcript.as_str() {
        format!("{marker} transcript ")
    } else if view.kind == crate::chat::sessions::model::ManagedKind::Diff.as_str() {
        format!("{marker} diff ")
    } else if view.kind == crate::chat::sessions::model::ManagedKind::Worker.as_str() {
        format!("{marker} worker w#{} ", view.seq)
    } else {
        format!("{marker} attached #{} {} ", view.seq, view.kind)
    };
    let suffix = if view.truncated { " [output truncated]" } else { "" };
    let fixed_cols = prefix.chars().count().saturating_add(suffix.chars().count());
    let header = if fixed_cols >= usize::from(max_width) {
        truncate_chars_with_ellipsis(&format!("{}{}", prefix.trim_end(), suffix), max_width, ascii)
    } else {
        let title_budget = u16::try_from(usize::from(max_width).saturating_sub(fixed_cols)).unwrap_or(u16::MAX);
        format!(
            "{prefix}{}{suffix}",
            truncate_chars_with_ellipsis(&view.title, title_budget, ascii)
        )
    };
    lines.push(Line::from(Span::styled(
        header,
        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
    )));

    let body_rows = usize::from(area.height.saturating_sub(1));
    for line in active_session_visible_lines(view, body_rows) {
        lines.push(Line::from(Span::styled(
            truncate_chars_with_ellipsis(&line, max_width, ascii),
            Style::default().fg(Color::White),
        )));
    }
    let widget = Paragraph::new(Text::from(lines)).style(Style::default().bg(Color::Black));
    frame.render_widget(widget, area);
}

pub(super) fn render_saved_session_picker_row(
    entry: &crate::chat::session::SavedSessionPickerEntry,
    narrow: bool,
    max_width: u16,
    ascii: bool,
) -> String {
    if max_width == 0 {
        return String::new();
    }
    let title = if entry.title.trim().is_empty() {
        "(untitled)".to_string()
    } else {
        entry.title.clone()
    };
    let title = if entry.is_current {
        format!("{title} (current)")
    } else {
        title
    };
    let meta = if narrow {
        format!("{} turns", entry.turn_count)
    } else {
        format!(
            "{} turns | {}/{} | {}",
            entry.turn_count,
            crate::providers::redact_endpoint_userinfo(&entry.provider),
            entry.model,
            entry.updated_at.format("%Y-%m-%d %H:%M")
        )
    };
    truncate_chars_with_ellipsis(&format!("{title} | {meta}"), max_width, ascii)
}

pub(super) fn render_saved_session_picker(
    frame: &mut Frame,
    area: Rect,
    picker: &crate::chat::session::SavedSessionPickerState,
    ascii: bool,
) {
    let marker = session_active_marker(true, ascii);
    let block = Block::default()
        .borders(Borders::TOP)
        .title(" Saved chat sessions (/resume) ")
        .border_style(Style::default().fg(Color::Green));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let hint_rows: u16 = 1;
    let list_height = inner.height.saturating_sub(hint_rows) as usize;
    if picker.is_empty() {
        let empty =
            Paragraph::new(" No saved chat sessions. Esc to close. ").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(empty, inner);
        return;
    }

    let total = picker.len();
    let selected = picker.selected.min(total.saturating_sub(1));
    let start = if list_height == 0 || total <= list_height {
        0
    } else {
        let half = list_height / 2;
        selected.saturating_sub(half).min(total.saturating_sub(list_height))
    };
    let end = (start + list_height).min(total);
    let narrow = inner.width < 64;
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(list_height.saturating_add(1));
    for (idx, entry) in picker.entries.get(start..end).unwrap_or(&[]).iter().enumerate() {
        let abs = start + idx;
        let is_selected = abs == selected;
        let head = if is_selected {
            format!("{marker} ")
        } else {
            "  ".to_string()
        };
        let body = render_saved_session_picker_row(entry, narrow, inner.width.saturating_sub(2), ascii);
        let style = if is_selected {
            Style::default()
                .fg(Color::White)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else if entry.is_current {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(format!("{head}{body}"), style)));
    }

    let hidden = total.saturating_sub(end).saturating_add(start);
    let hint = if hidden > 0 {
        format!(" \u{2191}\u{2193}/Ctrl+N/P move \u{00B7} Enter resume \u{00B7} Esc close \u{00B7} {hidden} more ")
    } else {
        " \u{2191}\u{2193}/Ctrl+N/P move \u{00B7} Enter resume \u{00B7} Esc close ".to_string()
    };
    let hint = if ascii {
        hint.replace('\u{2191}', "Up").replace('\u{2193}', "Down")
    } else {
        hint
    };
    lines.push(Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))));

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

pub(super) fn slash_menu_command_spans(entry: &SlashMenuEntry, filter: &str, selected: bool) -> Vec<Span<'static>> {
    let style = if selected {
        Style::default()
            .fg(Color::White)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    };
    let highlight = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    };
    let filter = filter.trim_start_matches('/').to_ascii_lowercase();
    let name = entry.label.clone();
    if filter.is_empty() {
        return vec![Span::styled(name, style)];
    }
    let name_without_slash = entry.label.trim_start_matches('/');
    let Some(pos) = name_without_slash.to_ascii_lowercase().find(&filter) else {
        return vec![Span::styled(name, style)];
    };
    let start = pos.saturating_add(1);
    let end = start.saturating_add(filter.len()).min(entry.label.len());
    let mut spans = Vec::new();
    if let Some(prefix) = entry.label.get(..start)
        && !prefix.is_empty()
    {
        spans.push(Span::styled(prefix.to_string(), style));
    }
    if let Some(matched) = entry.label.get(start..end)
        && !matched.is_empty()
    {
        spans.push(Span::styled(matched.to_string(), highlight));
    }
    if let Some(suffix) = entry.label.get(end..)
        && !suffix.is_empty()
    {
        spans.push(Span::styled(suffix.to_string(), style));
    }
    spans
}

pub(super) fn render_slash_menu_row(
    entry: &SlashMenuEntry,
    filter: &str,
    selected: bool,
    max_width: u16,
) -> Line<'static> {
    let base_style = if selected {
        Style::default()
            .fg(Color::White)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    if max_width == 0 {
        return Line::default();
    }
    let command_width = 18usize.min(usize::from(max_width));
    let mut spans = slash_menu_command_spans(entry, filter, selected);
    let usage_tail = if entry.args_hint.is_empty() {
        String::new()
    } else {
        format!(" {}", entry.args_hint)
    };
    let usage_cols = entry.label.chars().count().saturating_add(usage_tail.chars().count());
    if !usage_tail.is_empty() && usage_cols <= command_width {
        spans.push(Span::styled(usage_tail, base_style));
    }
    let used_cols = usage_cols.min(command_width);
    if used_cols < command_width {
        spans.push(Span::styled(" ".repeat(command_width - used_cols), base_style));
    } else {
        spans.push(Span::styled(" ", base_style));
    }
    let description_budget = usize::from(max_width).saturating_sub(command_width.saturating_add(1));
    if description_budget > 0 {
        let description = truncate_chars_with_ellipsis(
            &entry.description,
            u16::try_from(description_budget).unwrap_or(u16::MAX),
            true,
        );
        spans.push(Span::styled(description, base_style));
    }
    Line::from(spans)
}

pub(super) fn render_slash_menu(frame: &mut Frame, area: Rect, menu: &SlashMenuState, ascii: bool) {
    let block = Block::default()
        .borders(Borders::TOP)
        .title(" Slash commands ")
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let hint_rows: u16 = 1;
    let list_height = inner.height.saturating_sub(hint_rows) as usize;
    if menu.is_empty() {
        return;
    }

    let total = menu.len();
    let selected = menu.selected.min(total.saturating_sub(1));
    let start = if list_height == 0 || total <= list_height {
        0
    } else {
        let half = list_height / 2;
        selected.saturating_sub(half).min(total.saturating_sub(list_height))
    };
    let end = (start + list_height).min(total);
    let marker = session_active_marker(true, ascii);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(list_height.saturating_add(1));
    for (idx, entry) in menu.entries.get(start..end).unwrap_or(&[]).iter().enumerate() {
        let abs = start + idx;
        let selected = abs == selected;
        let mut row_spans = Vec::new();
        let head = if selected {
            format!("{marker} ")
        } else {
            "  ".to_string()
        };
        let head_style = if selected {
            Style::default().fg(Color::White).bg(Color::Magenta)
        } else {
            Style::default().fg(Color::White)
        };
        row_spans.push(Span::styled(head, head_style));
        row_spans.extend(render_slash_menu_row(entry, &menu.filter, selected, inner.width.saturating_sub(2)).spans);
        lines.push(Line::from(row_spans));
    }

    let hidden = total.saturating_sub(end).saturating_add(start);
    let hint = if hidden > 0 {
        format!(" \u{2191}\u{2193}/Ctrl+N/P move \u{00B7} Enter/Tab insert \u{00B7} Esc close \u{00B7} {hidden} more ")
    } else {
        " \u{2191}\u{2193}/Ctrl+N/P move \u{00B7} Enter/Tab insert \u{00B7} Esc close ".to_string()
    };
    let hint = if ascii {
        hint.replace('\u{2191}', "Up").replace('\u{2193}', "Down")
    } else {
        hint
    };
    lines.push(Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))));

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Render the Ctrl+G session switcher popup (v1.1b) inside the reserved bottom
/// chrome (never an alternate screen).
///
/// Layout: a bordered block titled `Sessions`, one row per session, then a
/// trailing hint row. The highlighted row is reverse-video + bold (plus a `>`
/// marker so it reads even without colour). When the list is taller than the
/// available rows it scrolls to keep the selection visible and shows a
/// `N more` overflow hint (narrow/short-terminal degrade, plan §0.4).
/// Build the text body for one switcher row (v5).
///
/// Pure (no `Frame`/`Rect`/lock) so it is unit-testable. Layout:
/// - wide:   `<glyph> #N <kind> <origin> <status> <title>`
/// - narrow: `<glyph> #N <kind> <title>` (origin + long status dropped, title
///   hard-truncated to fit `max_width` columns) so a row never overflows a
///   small terminal.
///
/// `max_width` is the column budget for the body (the caller already reserves
/// space for the selection marker). A `0` budget yields an empty string. Title
/// truncation counts `char`s (not bytes) and appends `…` when it elides.
pub(super) fn render_switcher_row(
    entry: &crate::chat::sessions::SwitcherEntry,
    glyph: &str,
    narrow: bool,
    max_width: u16,
) -> String {
    if max_width == 0 {
        return String::new();
    }
    let max_width = max_width as usize;
    // Fixed prefix (everything but the title), then fit the title into whatever
    // columns remain so the whole row stays within `max_width`.
    let usage = entry
        .token_usage_summary()
        .and_then(crate::chat::session::format_session_token_usage_inline);
    let display_id = switcher_entry_display_id(entry);
    let prefix = if narrow {
        usage.map_or_else(
            || format!("{glyph} {display_id} {} {} ", entry.kind, entry.elapsed_label()),
            |usage| format!("{glyph} {display_id} {} {} {usage} ", entry.kind, entry.elapsed_label()),
        )
    } else {
        usage.map_or_else(
            || {
                format!(
                    "{glyph} {display_id} {} {} {} {} ",
                    entry.kind,
                    entry.origin,
                    entry.status,
                    entry.elapsed_label()
                )
            },
            |usage| {
                format!(
                    "{glyph} {display_id} {} {} {} {} {usage} ",
                    entry.kind,
                    entry.origin,
                    entry.status,
                    entry.elapsed_label()
                )
            },
        )
    };
    let prefix_cols = prefix.chars().count();
    if prefix_cols >= max_width {
        // No room for the title at all: clamp the prefix itself.
        return prefix.chars().take(max_width).collect();
    }
    let title_budget = max_width - prefix_cols;
    let title_cols = entry.title.chars().count();
    let title = if title_cols <= title_budget {
        entry.title.clone()
    } else if title_budget == 0 {
        String::new()
    } else {
        // Reserve one column for the ellipsis when we actually elide.
        let take = title_budget.saturating_sub(1);
        let mut t: String = entry.title.chars().take(take).collect();
        t.push('\u{2026}');
        t
    };
    format!("{prefix}{title}")
}

pub(super) fn render_switcher(
    frame: &mut Frame,
    area: Rect,
    switcher: &crate::chat::sessions::SwitcherState,
    ascii: bool,
) {
    let marker = session_active_marker(true, ascii);
    let block = Block::default()
        .borders(Borders::TOP)
        .title(" Sessions - child TUI registry (Ctrl+G) ")
        .border_style(Style::default().fg(Color::Blue));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    // Reserve the last inner row for the hint/footer; the rest lists sessions.
    let hint_rows: u16 = 1;
    let list_height = inner.height.saturating_sub(hint_rows) as usize;

    if switcher.is_empty() {
        let empty =
            Paragraph::new(" No child TUI sessions. Esc to close. ").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(empty, inner);
        return;
    }

    // Scroll so the selected row stays visible when the list overflows.
    let total = switcher.len();
    let start = if list_height == 0 || total <= list_height {
        0
    } else {
        // Keep the selection roughly centred but clamped to valid bounds.
        let half = list_height / 2;
        switcher
            .selected
            .saturating_sub(half)
            .min(total.saturating_sub(list_height))
    };
    let end = (start + list_height).min(total);

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(list_height.saturating_add(1));
    #[allow(clippy::indexing_slicing)]
    for (idx, entry) in switcher.entries.get(start..end).unwrap_or(&[]).iter().enumerate() {
        let abs = start + idx;
        let selected = abs == switcher.selected;
        let head = if selected {
            format!("{marker} ")
        } else {
            "  ".to_string()
        };
        // Accessibility (§0.2.1 F): status is conveyed by a leading glyph
        // (shape), not only by the grey-out colour, so it survives no-color
        // terminals. We also tag the kind (agent/shell/pty) and origin
        // (user/model, §17) so the operator can tell at a glance which sessions
        // the model started for itself. On a narrow terminal we drop the origin
        // tag and hard-truncate the title so the row never overflows / wraps.
        let glyph = session_status_glyph(entry, ascii);
        let narrow = inner.width < 48;
        let body = render_switcher_row(entry, glyph, narrow, inner.width.saturating_sub(2));
        let style = if selected {
            Style::default()
                .fg(Color::White)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD)
        } else if entry.is_terminal() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(format!("{head}{body}"), style)));
    }

    // Hint / overflow row.
    let hidden = total.saturating_sub(end).saturating_add(start);
    let hint = if hidden > 0 {
        format!(" \u{2191}\u{2193}/Ctrl+N/P move \u{00B7} Enter open \u{00B7} Esc close \u{00B7} {hidden} more ")
    } else {
        " \u{2191}\u{2193}/Ctrl+N/P move \u{00B7} Enter open \u{00B7} Esc close ".to_string()
    };
    let hint = if ascii {
        hint.replace('\u{2191}', "Up").replace('\u{2193}', "Down")
    } else {
        hint
    };
    lines.push(Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))));

    let widget = Paragraph::new(Text::from(lines));
    frame.render_widget(widget, inner);
}

pub(super) fn render_status_bar<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) {
    let status_text = render_status_bar_text(state, area.width);
    let status = Paragraph::new(status_text).style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(status, area);
}

#[allow(clippy::option_if_let_else)]
pub(super) fn render_status_bar_text<V: BottomChromeView + ?Sized>(state: &V, width: u16) -> String {
    let title_str = state.session_title();
    let title = if title_str.is_empty() {
        "(new session)"
    } else {
        title_str
    };

    let usage = render_main_status_usage(
        state.token_usage_summary(),
        state.context_used_tokens(),
        state.context_window_tokens(),
    );
    let queue = render_main_queue_status(state.main_queue_status());
    let usage = if let Some(queue) = queue.as_deref() {
        format!("{usage} | {queue}")
    } else {
        usage
    };
    let worker_status = state.provider_worker_status();
    let workers = render_provider_worker_status(worker_status.clone());
    let workers_compact = render_provider_worker_status_compact(worker_status);
    let usage = if let Some(workers) = workers.as_deref() {
        format!("{usage} | {workers}")
    } else {
        usage
    };
    let permissions = render_permission_status(state.chat_mode(), state.autonomy_level());
    let full = format!(
        " PRX Chat | {}/{} | {} | {permissions} | {usage} ",
        crate::providers::redact_endpoint_userinfo(state.provider()),
        state.model(),
        title,
    );
    if full.chars().count() <= usize::from(width) {
        return full;
    }

    let compact = format!(
        " PRX Chat | {}/{} | {permissions} | {usage} ",
        crate::providers::redact_endpoint_userinfo(state.provider()),
        state.model()
    );
    if compact.chars().count() <= usize::from(width) {
        return compact;
    }

    let minimal = if let Some(queue) = queue.as_deref() {
        format!(" PRX | {permissions} | {queue} ")
    } else if let Some(workers) = workers.as_deref() {
        let detailed = format!(" PRX | {permissions} | {workers} ");
        if detailed.chars().count() <= usize::from(width) {
            detailed
        } else if let Some(workers) = workers_compact.as_deref() {
            format!(" PRX | {permissions} | {workers} ")
        } else {
            detailed
        }
    } else {
        format!(" PRX | {permissions} | {usage} ")
    };
    truncate_chars_with_ellipsis(&minimal, width, state.ascii_fallback())
}

pub(super) fn render_main_queue_status(status: MainQueueStatus) -> Option<String> {
    if status.queued == 0 && status.priority == 0 {
        return None;
    }
    if status.priority > 0 {
        Some(format!("queue:{} priority:{}", status.queued, status.priority))
    } else {
        Some(format!("queue:{}", status.queued))
    }
}

pub(super) fn render_provider_worker_status(status: ProviderWorkerStatus) -> Option<String> {
    render_provider_worker_status_with_rows(status, true)
}

pub(super) fn render_provider_worker_status_compact(status: ProviderWorkerStatus) -> Option<String> {
    render_provider_worker_status_with_rows(status, false)
}

pub(super) fn render_provider_worker_status_with_rows(
    status: ProviderWorkerStatus,
    include_rows: bool,
) -> Option<String> {
    // Finalized payload counters are cumulative bookkeeping. Once no worker is
    // running, cancelling, or waiting to commit, showing `wtok` makes a
    // completed turn look active and survives `/new` as stale UI state.
    if status.running == 0 && status.cancelling == 0 && status.awaiting_commit == 0 {
        return None;
    }
    // One worker is the normal main turn. Its progress already appears in the
    // transcript; repeating it as `workers:1 w#...:detached` is internal
    // implementation leakage. Reserve status-bar worker telemetry for actual
    // concurrent turns.
    if status
        .running
        .saturating_add(status.cancelling)
        .saturating_add(status.awaiting_commit)
        <= 1
    {
        return None;
    }
    let mut parts = Vec::new();
    if status.running > 0 {
        parts.push(format!("workers:{}", status.running));
    }
    if status.cancelling > 0 {
        parts.push(format!("cancelling:{}", status.cancelling));
    }
    if status.awaiting_commit > 0 {
        parts.push(format!("commit:{}", status.awaiting_commit));
    }
    if let Some(started_at_ms) = status.oldest_started_at_ms {
        let elapsed = provider_worker_elapsed_label(started_at_ms);
        parts.push(format!("welapsed:{elapsed}"));
    }
    if status.finalized_total_tokens > 0 {
        parts.push(format!(
            "wtok:{}",
            format_worker_tokens_compact(status.finalized_total_tokens)
        ));
    } else if status.finalized_payloads > 0 {
        parts.push(format!("wpayload:{}", status.finalized_payloads));
    }
    if include_rows && let Some(rows) = render_provider_worker_rows(&status) {
        parts.push(rows);
    }
    Some(parts.join(" "))
}

pub(super) fn render_provider_worker_rows(status: &ProviderWorkerStatus) -> Option<String> {
    if status.rows.is_empty() {
        return None;
    }
    let mut rendered = Vec::new();
    let active_rows = status.rows.iter().filter(|row| row.is_active());
    for row in active_rows.take(2) {
        rendered.push(render_provider_worker_row(row));
    }
    let remaining = status
        .rows
        .iter()
        .filter(|row| row.is_active())
        .count()
        .saturating_sub(rendered.len());
    if remaining > 0 {
        rendered.push(format!("+{remaining}w"));
    }
    (!rendered.is_empty()).then(|| rendered.join(" "))
}

pub(super) fn render_provider_worker_row(row: &ProviderWorkerStatusRow) -> String {
    let state = if row.completion_ready
        && matches!(
            row.state,
            ProviderWorkerRowState::Running | ProviderWorkerRowState::Cancelling
        ) {
        "ready"
    } else {
        match row.state {
            ProviderWorkerRowState::Running => "run",
            ProviderWorkerRowState::Cancelling => "cancel",
            ProviderWorkerRowState::AwaitingCommit => "commit",
            ProviderWorkerRowState::Committed => "done",
            ProviderWorkerRowState::Cancelled => "cancelled",
            ProviderWorkerRowState::Failed => "failed",
        }
    };
    let mut label = format!(
        "w#{}:{}:{state}",
        row.sequence,
        crate::chat::action::provider_worker_row_kind_compact(row.kind)
    );
    match row.state {
        ProviderWorkerRowState::Running | ProviderWorkerRowState::Cancelling => {
            label.push(':');
            label.push_str(&provider_worker_elapsed_label(row.started_at_ms));
        }
        ProviderWorkerRowState::AwaitingCommit
        | ProviderWorkerRowState::Committed
        | ProviderWorkerRowState::Cancelled
        | ProviderWorkerRowState::Failed => {
            if let Some(tokens) = row.finalized_total_tokens.filter(|tokens| *tokens > 0) {
                label.push(':');
                label.push_str(&format_worker_tokens_compact(tokens));
            }
        }
    }
    label
}

pub(super) fn provider_worker_elapsed_label(started_at_ms: i64) -> String {
    let elapsed_secs = turn_elapsed_ms(started_at_ms) / 1_000;
    crate::chat::sessions::model::format_elapsed_compact(elapsed_secs)
}

pub(crate) fn turn_elapsed_ms(started_at_ms: i64) -> u64 {
    let elapsed_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(started_at_ms)
        .max(0);
    u64::try_from(elapsed_ms).unwrap_or(u64::MAX)
}

pub(super) fn format_turn_duration(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        return format!("{duration_ms}ms");
    }
    let seconds = duration_ms / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

pub(super) fn format_worker_tokens_compact(tokens: u64) -> String {
    if tokens >= 1_000 {
        let whole = tokens / 1_000;
        let decimal = (tokens % 1_000) / 100;
        if decimal == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{decimal}k")
        }
    } else {
        tokens.to_string()
    }
}

pub fn execution_activity_active_for_view<V: BottomChromeView + ?Sized>(state: &V) -> bool {
    state.streaming().is_some()
        || state.conversation_lines().iter().any(|line| {
            matches!(
                line,
                ConversationLine::ToolResult {
                    status: ToolStatus::Running,
                    ..
                }
            )
        })
}

pub fn bottom_running_entry_active_for_view<V: BottomChromeView + ?Sized>(state: &V) -> bool {
    bottom_chrome_session_entries_with_workers(state.sessions_entries(), state.provider_worker_status(), state.focus())
        .iter()
        .any(|entry| entry.status == "running")
}

pub fn periodic_redraw_active_for_view<V: BottomChromeView + ?Sized>(state: &V) -> bool {
    execution_activity_active_for_view(state) || bottom_running_entry_active_for_view(state)
}

pub(super) fn render_permission_status(mode: ChatMode, autonomy: AutonomyLevel) -> String {
    format!("mode:{} auth:{}", mode.label(), autonomy_label(autonomy))
}

pub(super) const fn autonomy_label(level: AutonomyLevel) -> &'static str {
    match level {
        AutonomyLevel::ReadOnly => "read_only",
        AutonomyLevel::Supervised => "supervised",
        AutonomyLevel::Full => "full",
    }
}

pub(super) const fn cycle_chat_mode(mode: ChatMode) -> ChatMode {
    match mode {
        ChatMode::Plan => ChatMode::Edit,
        ChatMode::Edit => ChatMode::Auto,
        ChatMode::Auto => ChatMode::Plan,
    }
}

pub(super) fn render_main_token_usage(summary: MainSessionTokenUsageSummary) -> String {
    crate::chat::session::format_session_token_usage_inline(summary).unwrap_or_else(|| "0 tok | $0.0000".to_string())
}

pub(super) fn render_main_status_usage(
    summary: MainSessionTokenUsageSummary,
    context_used_tokens: Option<usize>,
    context_window_tokens: Option<usize>,
) -> String {
    let mut usage = render_main_token_usage(summary);
    if let Some(context) = render_context_budget_usage(context_used_tokens, context_window_tokens) {
        usage.push_str(" | ");
        usage.push_str(&context);
    }
    usage
}

pub(super) fn render_context_budget_usage(
    context_used_tokens: Option<usize>,
    context_window_tokens: Option<usize>,
) -> Option<String> {
    let window = u64::try_from(context_window_tokens?).ok()?.max(1);
    let used = u64::try_from(context_used_tokens?).ok()?;
    let pct = used
        .saturating_mul(100)
        .saturating_add(window.saturating_sub(1))
        .saturating_div(window)
        .min(100);
    let suffix = if pct >= 85 { "!" } else { "" };
    Some(format!("ctx:{pct}% used{suffix}"))
}
