use super::layout::{
    BOTTOM_CHROME_MAX_HEIGHT, BOTTOM_CHROME_MIN_HEIGHT, BottomChromeView, FullscreenTranscriptScroll,
    TRANSCRIPT_CHROME_GAP_HEIGHT, TranscriptSelection, fullscreen_tail_marker, fullscreen_transcript_area,
    fullscreen_transcript_top_scroll, session_footer_desired_rows, wrapped_rows_for_lines,
};
use super::render_chrome::{
    current_animation_tick, format_turn_duration, render_active_session_view, render_fullscreen_overlays,
    render_status_bar, spinner_frame_for_tick, turn_elapsed_ms,
};
use super::render_input::{
    input_visual_rows_for_width, render_approval, render_fullscreen_footer, render_input, wrap_line_ranges,
};
use super::tool_card::{estimate_reasoning_tokens, render_reasoning_card, render_tool_result, tool_card_glyphs};
use super::{
    ASSISTANT_MARKDOWN_CACHE_CAPACITY, ConversationLine, INPUT_MAX_VISIBLE_ROWS,
    STREAMING_MARKDOWN_HIGHLIGHT_MAX_BYTES, StreamingDraft, ToolStatus,
};
use parking_lot::Mutex;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph, Widget, Wrap};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock};

pub(super) static ASSISTANT_MARKDOWN_CACHE: LazyLock<Mutex<AssistantMarkdownCache>> =
    LazyLock::new(|| Mutex::new(AssistantMarkdownCache::new(ASSISTANT_MARKDOWN_CACHE_CAPACITY)));

#[derive(Debug)]
pub(super) struct AssistantMarkdownCache {
    pub(super) capacity: usize,
    pub(super) order: VecDeque<String>,
    pub(super) entries: HashMap<String, Arc<Vec<Line<'static>>>>,
}

impl AssistantMarkdownCache {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::new(),
            entries: HashMap::new(),
        }
    }

    pub(super) fn get_or_render(&mut self, content: &str) -> Arc<Vec<Line<'static>>> {
        if let Some(lines) = self.entries.get(content) {
            return Arc::clone(lines);
        }
        let rendered = Arc::new(render_assistant_markdown_lines(content));
        if self.capacity == 0 {
            return rendered;
        }
        while self.entries.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
        self.order.push_back(content.to_string());
        self.entries.insert(content.to_string(), Arc::clone(&rendered));
        rendered
    }

    #[cfg(test)]
    pub(super) fn clear(&mut self) {
        self.order.clear();
        self.entries.clear();
    }
}

pub(crate) fn transcript_render_row_at_point<V: BottomChromeView + ?Sized>(
    state: &V,
    scroll: &FullscreenTranscriptScroll,
    total_width: u16,
    total_height: u16,
    column: u16,
    row: u16,
) -> Option<(usize, usize)> {
    if state.switcher().is_some()
        || state.saved_session_picker().is_some()
        || state.slash_menu().is_some()
        || state.pending_tool_approval().is_some()
    {
        return None;
    }
    let area = fullscreen_transcript_area(state, total_width, total_height);
    if area.width == 0
        || area.height == 0
        || column < area.x
        || column >= area.x.saturating_add(area.width)
        || row < area.y
        || row >= area.y.saturating_add(area.height)
    {
        return None;
    }
    Some((
        fullscreen_transcript_top_scroll(state, scroll, area).saturating_add(usize::from(row.saturating_sub(area.y))),
        usize::from(column.saturating_sub(area.x)),
    ))
}

pub(crate) fn transcript_plain_rows<V: BottomChromeView + ?Sized>(state: &V, width: u16) -> Vec<String> {
    let mut lines: Vec<Line<'_>> = Vec::new();
    push_conversation_transcript_lines(&mut lines, state);
    push_live_turn_transcript_lines(&mut lines, state);
    if lines.is_empty() {
        lines.push(Line::from("(transcript is empty)"));
    }

    lines
        .iter()
        .flat_map(|line| {
            let text: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
            wrap_line_ranges(&text, usize::from(width.max(1)))
                .into_iter()
                .map(move |(start, end)| text.get(start..end).unwrap_or("").to_string())
        })
        .collect()
}

pub(super) fn push_conversation_transcript_lines<'a, V: BottomChromeView + ?Sized>(
    lines: &mut Vec<Line<'a>>,
    state: &'a V,
) {
    for line in state.conversation_lines() {
        // Reasoning is diagnostic data, not a second completion summary. Keep
        // it in the backing conversation for Ctrl+O's verbose transcript, but
        // omit it from the primary conversation flow.
        if matches!(line, ConversationLine::Reasoning { .. }) {
            continue;
        }
        render_conversation_line(lines, line, state.ascii_fallback());
    }
}

pub(super) fn push_live_turn_transcript_lines<'a, V: BottomChromeView + ?Sized>(
    lines: &mut Vec<Line<'a>>,
    state: &'a V,
) {
    let Some(streaming) = state.streaming() else {
        return;
    };
    if streaming.accumulated.is_empty() {
        let running_tool = state.conversation_lines().iter().any(|line| {
            matches!(
                line,
                ConversationLine::ToolResult {
                    status: ToolStatus::Running,
                    ..
                }
            )
        });
        if !running_tool {
            render_turn_activity_line(lines, state, streaming);
        }
        return;
    }
    push_blank_line_if_needed(lines);
    push_assistant_rendered_lines(
        lines,
        render_streaming_assistant_markdown_lines(&streaming.accumulated, state.ascii_fallback()),
        state.ascii_fallback(),
    );
    lines.push(Line::from(""));
}

/// Render the pre-first-token activity line.
///
/// While the model is only emitting reasoning ("thinking") deltas there is no
/// visible text to show, so the label becomes `Thinking` and the detail block
/// carries the live character / estimated-token counters plus a single clipped
/// preview line of the newest reasoning text. Without reasoning progress the
/// line is the unchanged `Working (...)` spinner.
pub(super) fn render_turn_activity_line<'a, V: BottomChromeView + ?Sized>(
    lines: &mut Vec<Line<'a>>,
    state: &V,
    streaming: &StreamingDraft,
) {
    let spinner = spinner_frame_for_tick(state.ascii_fallback(), current_animation_tick());
    let separator = if state.ascii_fallback() { " | " } else { " · " };
    let thinking = Some(streaming).filter(|draft| draft.reasoning_chars > 0);
    let mut detail = String::new();
    if let Some(started_at_ms) = state.active_turn_started_at_ms() {
        detail.push_str(&format_turn_duration(turn_elapsed_ms(started_at_ms)));
        detail.push_str(separator);
    }
    if let Some(draft) = thinking {
        detail.push_str(&format_thinking_progress(draft.reasoning_chars));
        detail.push_str(separator);
    }
    detail.push_str("Esc to interrupt");
    let label = if thinking.is_some() { "Thinking" } else { "Working" };
    lines.push(Line::from(vec![
        Span::styled(format!("{spinner} "), Style::default().fg(Color::Cyan)),
        Span::styled(label, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" ({detail})"), Style::default().fg(Color::DarkGray)),
    ]));
    if let Some(preview) = thinking.and_then(StreamingDraft::reasoning_preview) {
        lines.push(Line::from(Span::styled(
            format!(
                "  {}",
                crate::util::truncate_with_ellipsis(&preview, THINKING_PREVIEW_MAX_CHARS)
            ),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )));
    }
    lines.push(Line::from(""));
}

/// Maximum width of the live reasoning preview line.
pub(super) const THINKING_PREVIEW_MAX_CHARS: usize = 96;

/// `"1234 chars (~309 tok)"` — live thinking counters for the activity line.
pub(super) fn format_thinking_progress(char_count: usize) -> String {
    let tokens = estimate_reasoning_tokens(char_count);
    format!("{char_count} chars (~{tokens} tok)")
}

pub(super) fn fullscreen_bottom_chrome_base_height<V: BottomChromeView + ?Sized>(state: &V) -> u16 {
    let visible_input_rows = state.input().lines.len().clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let footer_rows = session_footer_desired_rows(state);
    TRANSCRIPT_CHROME_GAP_HEIGHT
        .saturating_add(1)
        .saturating_add(input_height)
        .saturating_add(footer_rows)
}

pub(super) fn fullscreen_bottom_chrome_height_for_width<V: BottomChromeView + ?Sized>(state: &V, width: u16) -> u16 {
    let visible_input_rows = input_visual_rows_for_width(state, width).clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let footer_rows = session_footer_desired_rows(state);
    TRANSCRIPT_CHROME_GAP_HEIGHT
        .saturating_add(1)
        .saturating_add(input_height)
        .saturating_add(footer_rows)
        .clamp(BOTTOM_CHROME_MIN_HEIGHT, BOTTOM_CHROME_MAX_HEIGHT)
}

pub(super) fn render_fullscreen_bottom_chrome_at<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    area: Rect,
    state: &V,
    show_new_output_below: bool,
) {
    let visible_input_rows = input_visual_rows_for_width(state, area.width).clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let max_footer_rows = area
        .height
        .saturating_sub(TRANSCRIPT_CHROME_GAP_HEIGHT)
        .saturating_sub(1)
        .saturating_sub(input_height)
        .max(1);
    let footer_rows = session_footer_desired_rows(state).min(max_footer_rows);

    let fixed_rows = TRANSCRIPT_CHROME_GAP_HEIGHT
        .saturating_add(1)
        .saturating_add(input_height)
        .saturating_add(footer_rows);
    let spacer_rows = area.height.saturating_sub(fixed_rows);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(TRANSCRIPT_CHROME_GAP_HEIGHT),
            Constraint::Length(1),
            Constraint::Length(spacer_rows),
            Constraint::Length(input_height),
            Constraint::Length(footer_rows),
        ])
        .split(area);

    #[allow(clippy::indexing_slicing)]
    {
        render_status_bar(frame, chunks[1], state);
        render_input(frame, chunks[3], state);
        render_fullscreen_footer(frame, chunks[4], state, show_new_output_below);
    }
}

pub fn render_fullscreen_chat<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    state: &V,
    scroll: &mut FullscreenTranscriptScroll,
) {
    render_fullscreen_chat_with_selection(frame, state, scroll, None);
}

pub fn render_fullscreen_chat_with_selection<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    state: &V,
    scroll: &mut FullscreenTranscriptScroll,
    selection: Option<&TranscriptSelection>,
) {
    let frame_area = frame.area();
    frame.render_widget(Clear, frame_area);

    let chrome_height = fullscreen_bottom_chrome_height_for_width(state, frame_area.width).min(frame_area.height);
    let content_height = frame_area.height.saturating_sub(chrome_height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(content_height), Constraint::Length(chrome_height)])
        .split(frame_area);

    #[allow(clippy::indexing_slicing)]
    {
        let (transcript_area, panel_area) = fullscreen_content_areas(chunks[0], state);
        render_fullscreen_transcript(frame, transcript_area, state, scroll, selection);
        render_fullscreen_panel(frame, panel_area, state);
        render_fullscreen_bottom_chrome_at(frame, chunks[1], state, scroll.new_output_below);
        render_fullscreen_overlays(frame, chunks[0], state);
    }
}

pub(super) fn fullscreen_content_areas<V: BottomChromeView + ?Sized>(area: Rect, state: &V) -> (Rect, Option<Rect>) {
    if area.height == 0 || (state.pending_tool_approval().is_none() && state.active_session_view().is_none()) {
        return (area, None);
    }
    let min_panel = if state.pending_tool_approval().is_some() { 6 } else { 8 };
    let desired = if state.pending_tool_approval().is_some() {
        area.height.saturating_mul(35).checked_div(100).unwrap_or(0)
    } else {
        area.height.saturating_mul(55).checked_div(100).unwrap_or(0)
    };
    let max_panel = area.height.saturating_sub(1);
    let panel_height = desired.max(min_panel.min(area.height)).min(max_panel);
    if panel_height == 0 {
        return (area, None);
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(panel_height)),
            Constraint::Length(panel_height),
        ])
        .split(area);
    #[allow(clippy::indexing_slicing)]
    (chunks[0], Some(chunks[1]))
}

pub(super) fn render_fullscreen_panel<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Option<Rect>, state: &V) {
    let Some(area) = area else {
        return;
    };
    frame.render_widget(Clear, area);
    if let Some(approval) = state.pending_tool_approval() {
        render_approval(
            frame,
            area,
            &approval.name,
            &approval.args,
            approval.selected_approval,
            state.ascii_fallback(),
        );
    } else if let Some(view) = state.active_session_view() {
        render_active_session_view(frame, area, view, state.ascii_fallback());
    }
}

pub(super) fn render_fullscreen_transcript<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    area: Rect,
    state: &V,
    scroll: &mut FullscreenTranscriptScroll,
    selection: Option<&TranscriptSelection>,
) {
    if area.height == 0 {
        scroll.offset_from_bottom = 0;
        scroll.anchor_top_row = None;
        return;
    }

    let mut lines: Vec<Line<'_>> = Vec::new();
    push_conversation_transcript_lines(&mut lines, state);
    push_live_turn_transcript_lines(&mut lines, state);
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(transcript is empty - PageUp/Home view history; /help for copy/export)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let total_rows = usize::from(measure_wrapped_rows(&lines, area.width.max(1)));
    let visible_rows = usize::from(area.height.max(1));
    let max_scroll = total_rows.saturating_sub(visible_rows);
    let tail_marker = fullscreen_tail_marker(state);
    let tail_advanced = tail_marker > scroll.last_tail_marker && scroll.last_tail_marker > 0;

    let top_scroll = if max_scroll == 0 {
        scroll.offset_from_bottom = 0;
        scroll.anchor_top_row = None;
        0
    } else if scroll.offset_from_bottom == 0 {
        scroll.anchor_top_row = None;
        max_scroll
    } else if let Some(anchor_top_row) = scroll.anchor_top_row {
        let top_scroll = anchor_top_row.min(max_scroll);
        scroll.offset_from_bottom = max_scroll.saturating_sub(top_scroll);
        top_scroll
    } else {
        scroll.offset_from_bottom = scroll.offset_from_bottom.min(max_scroll);
        let top_scroll = max_scroll.saturating_sub(scroll.offset_from_bottom);
        scroll.anchor_top_row = Some(top_scroll);
        top_scroll
    };

    if scroll.offset_from_bottom == 0 {
        scroll.new_output_below = false;
        scroll.anchor_top_row = None;
    } else if tail_advanced {
        scroll.new_output_below = true;
    }
    scroll.last_tail_marker = tail_marker;
    let top_scroll = u16::try_from(top_scroll).unwrap_or(u16::MAX);

    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((top_scroll, 0));
    frame.render_widget(paragraph, area);
    if let Some(selection) = selection {
        let top_scroll = usize::from(top_scroll);
        let highlight = Style::default().bg(Color::DarkGray);
        for screen_row in 0..area.height {
            let rendered_row = top_scroll.saturating_add(usize::from(screen_row));
            if let Some((start, end)) = selection.columns_for_row(rendered_row, usize::from(area.width)) {
                for column in start..end {
                    if let Some(cell) = frame.buffer_mut().cell_mut((
                        area.x.saturating_add(u16::try_from(column).unwrap_or(u16::MAX)),
                        area.y.saturating_add(screen_row),
                    )) {
                        cell.set_style(highlight);
                    }
                }
            }
        }
    }
}

/// Count the exact number of rows ratatui's word-wrapping
/// (`Wrap { trim: false }`) needs to render `lines` at `width`.
///
/// [`wrapped_rows_for_lines`] divides display width by terminal width and
/// is therefore only an *upper bound* — ratatui breaks at word boundaries,
/// so the real row count is usually smaller. The streaming preview scroll
/// must use the real count (not the upper bound), otherwise it scrolls past
/// the newest tokens. We obtain the ground truth by rendering into an
/// off-screen scratch [`Buffer`] sized to the safe upper bound (so nothing
/// is clipped) and reading back the populated height.
pub(super) fn measure_wrapped_rows(lines: &[Line<'_>], width: u16) -> u16 {
    let w = width.max(1);
    // Word-wrapping (`trim: false`) can produce MORE rows than the
    // char-count estimate: breaking before a word that does not fit pushes
    // it to the next row, wasting trailing columns. A guaranteed upper bound
    // is therefore the char estimate PLUS one extra row per source line (the
    // worst case is each line losing up to a full row to word boundaries),
    // with a small constant margin so the scratch buffer never clips.
    let char_rows = usize::from(wrapped_rows_for_lines(lines, w));
    let cap = u16::try_from(char_rows.saturating_add(lines.len()).saturating_add(2))
        .unwrap_or(u16::MAX)
        .max(1);
    let area = Rect {
        x: 0,
        y: 0,
        width: w,
        height: cap,
    };
    let mut buf = Buffer::empty(area);
    Paragraph::new(Text::from(lines.to_vec()))
        .wrap(Wrap { trim: false })
        .render(area, &mut buf);
    // The highest row index that received any non-space cell is the last
    // visible content row; rows below it are unused padding from the upper
    // bound. `+1` converts the index into a row count.
    let mut used: u16 = 0;
    for y in 0..cap {
        let mut populated = false;
        for x in 0..w {
            if let Some(cell) = buf.cell((x, y)) {
                if !cell.symbol().trim().is_empty() {
                    populated = true;
                    break;
                }
            }
        }
        if populated {
            used = y.saturating_add(1);
        }
    }
    used.max(1)
}

/// Render a single conversation line into the ratatui `lines` buffer.
///
/// Pure function (apart from the &mut push target) — kept outside
/// [`render_fullscreen_transcript`] so unit tests can drive it with a
/// `Vec<Line<'_>>` sink.
pub(super) fn render_conversation_line<'a>(lines: &mut Vec<Line<'a>>, conv_line: &'a ConversationLine, ascii: bool) {
    match conv_line {
        ConversationLine::User { content } => {
            // Claude Code style: `> ` prompt in dim gray (no bold), content in
            // default foreground on the same row when single-line; multi-line
            // continuation rows are dedented two spaces to align under the
            // first character after the `> ` prompt.
            let mut iter = content.lines();
            let first = iter.next().unwrap_or("");
            lines.push(Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::DarkGray)),
                Span::raw(first),
            ]));
            for text_line in iter {
                lines.push(Line::from(format!("  {text_line}")));
            }
            lines.push(Line::from(""));
        }
        ConversationLine::Assistant { content } => {
            // PRX chat uses an explicit actor marker for assistant-authored
            // prose so it is visually distinct from tool IO and child-session
            // output when scanning a dense orchestration transcript.
            push_blank_line_if_needed(lines);
            let rendered = cached_finalized_assistant_markdown_lines(content);
            push_assistant_rendered_lines(lines, rendered.iter().cloned(), ascii);
            lines.push(Line::from(""));
        }
        ConversationLine::StreamingAssistant { content } => {
            // Same actor marker as finalized assistant text. A trailing cursor
            // glyph (`▌`, or `_` in ASCII mode) signals that more bytes are
            // still inbound; once the stream finalises the variant becomes
            // `Assistant` and the cursor disappears.
            push_blank_line_if_needed(lines);
            push_assistant_rendered_lines(lines, render_streaming_assistant_markdown_lines(content, ascii), ascii);
            lines.push(Line::from(""));
        }
        ConversationLine::System { content } => {
            // Claude Code style: dim gray italic, no prefix or indent.
            for text_line in content.lines() {
                lines.push(Line::from(Span::styled(
                    text_line.to_string(),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                )));
            }
            lines.push(Line::from(""));
        }
        ConversationLine::Tool { name, success } => {
            // Legacy single-line tool indicator. Same bullet shape as the
            // richer ToolResult card so the two stay visually consistent.
            let (bullet, _) = tool_card_glyphs(ascii);
            let color = if *success { Color::Green } else { Color::Red };
            lines.push(Line::from(vec![
                Span::styled(format!("{bullet} "), Style::default().fg(color)),
                Span::raw(name.as_str()),
            ]));
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
        ConversationLine::Reasoning {
            content,
            char_count,
            folded,
        } => render_reasoning_card(lines, content, *char_count, *folded, ascii),
    }
}

pub(super) fn push_assistant_rendered_lines<'a, I>(lines: &mut Vec<Line<'a>>, rendered: I, ascii: bool)
where
    I: IntoIterator<Item = Line<'static>>,
{
    let marker = if ascii { "*" } else { "\u{25CF}" }; // ●
    let marker_style = Style::default().fg(Color::DarkGray);
    let continuation_style = Style::default().fg(Color::DarkGray);
    let mut any = false;
    for (idx, line) in rendered.into_iter().enumerate() {
        any = true;
        let mut spans: Vec<Span<'a>> = Vec::with_capacity(line.spans.len().saturating_add(1));
        if idx == 0 {
            spans.push(Span::styled(format!("{marker} "), marker_style));
        } else {
            spans.push(Span::styled("  ", continuation_style));
        }
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
    if !any {
        lines.push(Line::from(Span::styled(format!("{marker} "), marker_style)));
    }
}

pub(super) fn push_blank_line_if_needed(lines: &mut Vec<Line<'_>>) {
    let last_is_blank = lines
        .last()
        .is_none_or(|line| line.spans.iter().all(|span| span.content.as_ref().trim().is_empty()));
    if !last_is_blank {
        lines.push(Line::from(""));
    }
}

pub(super) fn cached_finalized_assistant_markdown_lines(content: &str) -> Arc<Vec<Line<'static>>> {
    ASSISTANT_MARKDOWN_CACHE.lock().get_or_render(content)
}

pub(super) fn render_streaming_assistant_markdown_lines(content: &str, ascii: bool) -> Vec<Line<'static>> {
    let cursor = if ascii { "_" } else { "\u{258C}" }; // ▌
    let mut lines = if content.len() > STREAMING_MARKDOWN_HIGHLIGHT_MAX_BYTES {
        content
            .split('\n')
            .map(|line| Line::from(line.to_string()))
            .collect::<Vec<_>>()
    } else {
        render_assistant_markdown_lines(content)
    };
    if let Some(last) = lines.last_mut() {
        last.spans.push(Span::raw(cursor.to_string()));
    } else {
        lines.push(Line::from(cursor.to_string()));
    }
    lines
}

pub(super) fn render_assistant_markdown_lines(content: &str) -> Vec<Line<'static>> {
    let rendered = crate::chat::renderer::render_markdown_with_highlighting(content);
    ansi_sgr_to_lines(&rendered)
}

pub(super) fn ansi_sgr_to_lines(input: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut segment = String::new();
    let mut style = Style::default();
    let mut idx = 0usize;

    while idx < input.len() {
        if input[idx..].starts_with("\x1b[")
            && let Some((end_rel, final_byte)) = find_csi_final(&input[idx + 2..])
        {
            flush_ansi_segment(&mut spans, &mut segment, style);
            if final_byte == b'm' {
                let codes = &input[idx + 2..idx + 2 + end_rel];
                style = apply_sgr_codes(style, codes);
            }
            idx += 2 + end_rel + 1;
            continue;
        }

        let Some(ch) = input[idx..].chars().next() else {
            break;
        };
        idx += ch.len_utf8();
        match ch {
            '\n' => {
                flush_ansi_segment(&mut spans, &mut segment, style);
                lines.push(Line::from(std::mem::take(&mut spans)));
            }
            '\r' => {}
            _ => segment.push(ch),
        }
    }

    flush_ansi_segment(&mut spans, &mut segment, style);
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

pub(super) fn find_csi_final(input: &str) -> Option<(usize, u8)> {
    input.bytes().enumerate().find(|(_, byte)| (0x40..=0x7e).contains(byte))
}

pub(super) fn flush_ansi_segment(spans: &mut Vec<Span<'static>>, segment: &mut String, style: Style) {
    if segment.is_empty() {
        return;
    }
    spans.push(Span::styled(std::mem::take(segment), style));
}

pub(super) fn apply_sgr_codes(mut style: Style, codes: &str) -> Style {
    let values = parse_sgr_values(codes);
    if values.is_empty() {
        return Style::default();
    }
    let mut idx = 0usize;
    while idx < values.len() {
        let Some(code) = values.get(idx).copied() else {
            break;
        };
        match code {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => style = style.remove_modifier(Modifier::BOLD),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            30..=37 => {
                style = style.fg(ansi_basic_color(code, false));
            }
            39 => style = style.fg(Color::Reset),
            90..=97 => {
                style = style.fg(ansi_basic_color(code - 60, true));
            }
            40..=47 => {
                style = style.bg(ansi_basic_color(code - 10, false));
            }
            49 => style = style.bg(Color::Reset),
            100..=107 => {
                style = style.bg(ansi_basic_color(code - 70, true));
            }
            38 | 48 => match values.get(idx + 1).copied() {
                Some(2) => {
                    if let (Some(r), Some(g), Some(b)) = (
                        values.get(idx + 2).copied(),
                        values.get(idx + 3).copied(),
                        values.get(idx + 4).copied(),
                    ) {
                        let color = Color::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8);
                        if code == 38 {
                            style = style.fg(color);
                        } else {
                            style = style.bg(color);
                        }
                        idx += 4;
                    }
                }
                Some(5) => {
                    if let Some(indexed) = values.get(idx + 2).copied() {
                        let color = Color::Indexed(indexed.min(255) as u8);
                        if code == 38 {
                            style = style.fg(color);
                        } else {
                            style = style.bg(color);
                        }
                        idx += 2;
                    }
                }
                _ => {}
            },
            _ => {}
        }
        idx += 1;
    }
    style
}

pub(super) fn parse_sgr_values(codes: &str) -> Vec<u16> {
    if codes.is_empty() {
        return Vec::new();
    }
    codes
        .split(';')
        .filter_map(|part| {
            if part.is_empty() {
                Some(0)
            } else {
                part.parse::<u16>().ok()
            }
        })
        .collect()
}

pub(super) const fn ansi_basic_color(code: u16, bright: bool) -> Color {
    match (code, bright) {
        (30, false) => Color::Black,
        (31, false) => Color::Red,
        (32, false) => Color::Green,
        (33, false) => Color::Yellow,
        (34, false) => Color::Blue,
        (35, false) => Color::Magenta,
        (36, false) => Color::Cyan,
        (37, false) => Color::Gray,
        (30, true) => Color::DarkGray,
        (31, true) => Color::LightRed,
        (32, true) => Color::LightGreen,
        (33, true) => Color::LightYellow,
        (34, true) => Color::LightBlue,
        (35, true) => Color::LightMagenta,
        (36, true) => Color::LightCyan,
        (37, true) => Color::White,
        _ => Color::Reset,
    }
}
