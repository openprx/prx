use super::input::TuiInput;
use super::keys::bottom_chrome_session_entries_with_workers;
use super::render_conversation::{
    fullscreen_bottom_chrome_base_height, fullscreen_bottom_chrome_height_for_width, fullscreen_content_areas,
    measure_wrapped_rows, push_conversation_transcript_lines, push_live_turn_transcript_lines,
};
use super::slash_menu::SlashMenuState;
use super::{ConversationLine, StreamingDraft, TuiState};
use crate::agent::turn_contracts::ChatMode;
use crate::chat::action::{MainQueueStatus, ProviderWorkerStatus};
use crate::chat::session::MainSessionTokenUsageSummary;
use crate::security::AutonomyLevel;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Count the visible rows a `Vec<Line>` will consume at the given
/// width, assuming `Paragraph::wrap(Wrap { trim: false })` semantics:
/// each `Line` takes `ceil(display_width / width)` rows, or `1` if
/// empty. Saturates at `u16::MAX`.
pub(super) fn wrapped_rows_for_lines(lines: &[Line<'_>], width: u16) -> u16 {
    let w = usize::from(width.max(1));
    let mut total: usize = 0;
    for line in lines {
        let display_width: usize = line
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        let rows = if display_width == 0 {
            1
        } else {
            display_width.div_ceil(w)
        };
        total = total.saturating_add(rows);
    }
    u16::try_from(total).unwrap_or(u16::MAX)
}

/// Render-source abstraction: lets the fullscreen renderer support both `TuiState`
/// (the chat_mirror path) and `UiSnapshot` (the S4-A Pure-mode watch path).
///
/// S4-A Commit 2: extracts the minimal field set the renderer needs into a trait and
/// generifies every `&TuiState` parameter into `&V: BottomChromeView`. This commit does
/// not switch the render source yet; it only generifies the function signatures and adds
/// the two impls, with no behaviour change.
pub trait BottomChromeView {
    fn provider(&self) -> &str;
    fn model(&self) -> &str;
    fn chat_mode(&self) -> ChatMode;
    fn autonomy_level(&self) -> AutonomyLevel;
    fn session_title(&self) -> &str;
    fn turn_count(&self) -> usize;
    fn ascii_fallback(&self) -> bool;
    fn conversation_lines(&self) -> &[ConversationLine];
    fn streaming(&self) -> Option<&StreamingDraft>;
    /// Wall-clock start of the active main turn, when available.
    fn active_turn_started_at_ms(&self) -> Option<i64> {
        None
    }
    /// Duration of the most recently completed main turn, if any.
    fn last_turn_duration_ms(&self) -> Option<u64> {
        None
    }
    fn input(&self) -> &TuiInput;
    /// Persistent child-session status line (v1b). Empty hides the row.
    fn sessions_status(&self) -> &str;
    /// Structured child-session entries for the always-visible P1 strip.
    fn sessions_entries(&self) -> &[crate::chat::sessions::SwitcherEntry];
    /// Main-session input backlog status.
    fn main_queue_status(&self) -> MainQueueStatus;
    /// Main-session provider worker status.
    fn provider_worker_status(&self) -> ProviderWorkerStatus;
    /// Focused line-session viewport (P2), if any.
    fn active_session_view(&self) -> Option<&crate::chat::sessions::ActiveSessionView>;
    /// Foreground tool approval prompt (P6c1), if any.
    fn pending_tool_approval(&self) -> Option<&crate::chat::sessions::PendingToolApprovalView>;
    /// Current planned context usage for UI-only status budget display.
    fn context_used_tokens(&self) -> Option<usize>;
    /// Effective context window for UI-only status budget display.
    fn context_window_tokens(&self) -> Option<usize>;
    /// Main-session cumulative token/cost summary.
    fn token_usage_summary(&self) -> MainSessionTokenUsageSummary;
    /// Current input-routing target (v1.1b). Drives the prompt's colour+glyph
    /// target indicator (`main >` vs `<kind> #N ▸`).
    fn focus(&self) -> crate::chat::sessions::FocusTarget;
    /// Open Ctrl+G session switcher overlay (v1.1b), or `None` when closed.
    fn switcher(&self) -> Option<&crate::chat::sessions::SwitcherState>;
    /// Open slash-command menu overlay, or `None` when closed.
    fn slash_menu(&self) -> Option<&SlashMenuState>;
    /// Open saved chat-session picker overlay (P7c), or `None` when closed.
    fn saved_session_picker(&self) -> Option<&crate::chat::session::SavedSessionPickerState>;
}

impl BottomChromeView for TuiState {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn chat_mode(&self) -> ChatMode {
        self.chat_mode
    }
    fn autonomy_level(&self) -> AutonomyLevel {
        self.autonomy_level
    }
    fn session_title(&self) -> &str {
        &self.session_title
    }
    fn turn_count(&self) -> usize {
        self.turn_count
    }
    fn ascii_fallback(&self) -> bool {
        self.ascii_fallback
    }
    fn conversation_lines(&self) -> &[ConversationLine] {
        &self.conversation_lines
    }
    fn streaming(&self) -> Option<&StreamingDraft> {
        self.streaming.as_ref()
    }
    fn active_turn_started_at_ms(&self) -> Option<i64> {
        self.stream_started_at_ms
    }
    fn input(&self) -> &TuiInput {
        &self.input
    }
    fn sessions_status(&self) -> &str {
        &self.sessions_status
    }
    fn sessions_entries(&self) -> &[crate::chat::sessions::SwitcherEntry] {
        &self.sessions_cache
    }
    fn main_queue_status(&self) -> MainQueueStatus {
        self.main_queue_status
    }
    fn provider_worker_status(&self) -> ProviderWorkerStatus {
        self.provider_worker_status.clone()
    }
    fn active_session_view(&self) -> Option<&crate::chat::sessions::ActiveSessionView> {
        self.active_session_view.as_ref()
    }
    fn pending_tool_approval(&self) -> Option<&crate::chat::sessions::PendingToolApprovalView> {
        self.pending_tool_approval.as_ref()
    }
    fn context_used_tokens(&self) -> Option<usize> {
        self.context_used_tokens
    }
    fn context_window_tokens(&self) -> Option<usize> {
        self.context_window_tokens
    }
    fn token_usage_summary(&self) -> MainSessionTokenUsageSummary {
        self.token_usage_summary
    }
    fn focus(&self) -> crate::chat::sessions::FocusTarget {
        self.focus
    }
    fn switcher(&self) -> Option<&crate::chat::sessions::SwitcherState> {
        self.switcher.as_ref()
    }
    fn slash_menu(&self) -> Option<&SlashMenuState> {
        self.slash_menu.as_ref()
    }
    fn saved_session_picker(&self) -> Option<&crate::chat::session::SavedSessionPickerState> {
        self.saved_session_picker.as_ref()
    }
}

impl BottomChromeView for crate::chat::state::UiSnapshot {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn chat_mode(&self) -> ChatMode {
        self.chat_mode
    }
    fn autonomy_level(&self) -> AutonomyLevel {
        self.autonomy_level
    }
    fn session_title(&self) -> &str {
        &self.session_title
    }
    fn turn_count(&self) -> usize {
        self.turn_count
    }
    fn ascii_fallback(&self) -> bool {
        self.ascii_fallback
    }
    fn conversation_lines(&self) -> &[ConversationLine] {
        &self.conversation_lines
    }
    fn streaming(&self) -> Option<&StreamingDraft> {
        self.streaming.as_ref()
    }
    fn active_turn_started_at_ms(&self) -> Option<i64> {
        self.active_turn_started_at_ms
    }
    fn last_turn_duration_ms(&self) -> Option<u64> {
        self.last_turn_duration_ms
    }
    fn input(&self) -> &TuiInput {
        &self.input
    }
    fn sessions_status(&self) -> &str {
        &self.sessions_status
    }
    fn sessions_entries(&self) -> &[crate::chat::sessions::SwitcherEntry] {
        self.sessions_entries.as_slice()
    }
    fn main_queue_status(&self) -> MainQueueStatus {
        self.main_queue_status
    }
    fn provider_worker_status(&self) -> ProviderWorkerStatus {
        self.provider_worker_status.clone()
    }
    fn active_session_view(&self) -> Option<&crate::chat::sessions::ActiveSessionView> {
        self.active_session_view.as_ref()
    }
    fn pending_tool_approval(&self) -> Option<&crate::chat::sessions::PendingToolApprovalView> {
        self.pending_tool_approval.as_ref()
    }
    fn context_used_tokens(&self) -> Option<usize> {
        self.context_used_tokens
    }
    fn context_window_tokens(&self) -> Option<usize> {
        self.context_window_tokens
    }
    fn token_usage_summary(&self) -> MainSessionTokenUsageSummary {
        self.token_usage_summary
    }
    fn focus(&self) -> crate::chat::sessions::FocusTarget {
        self.focus
    }
    fn switcher(&self) -> Option<&crate::chat::sessions::SwitcherState> {
        self.switcher.as_ref()
    }
    fn slash_menu(&self) -> Option<&SlashMenuState> {
        self.slash_menu.as_ref()
    }
    fn saved_session_picker(&self) -> Option<&crate::chat::session::SavedSessionPickerState> {
        self.saved_session_picker.as_ref()
    }
}

/// One blank row separating conversation output from the persistent chrome.
/// This keeps the final assistant/tool line from touching the gray status bar.
pub(super) const TRANSCRIPT_CHROME_GAP_HEIGHT: u16 = 1;

/// Minimum height (rows) of the pinned fullscreen bottom chrome. Reserves space
/// for the transcript gap, status, input, and footer.
pub const BOTTOM_CHROME_MIN_HEIGHT: u16 = 4;

/// Hard upper bound on the pinned fullscreen bottom chrome height.
pub const BOTTOM_CHROME_MAX_HEIGHT: u16 = 24;

/// Desired body height for the focused line-session viewport (P2). Header is
/// additional. The actual render height is capped by the available terminal
/// area so short terminals degrade without overlapping input/footer.
pub const ACTIVE_SESSION_VIEW_DESIRED_ROWS: u16 = 10;

/// Fullscreen transcript scroll state.
///
/// Stored as rows from the tail so new output naturally follows when the offset
/// is zero. Phase 1 keeps this renderer-local; later phases can lift it into UI
/// state if richer scroll focus/search needs reducer ownership.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FullscreenTranscriptScroll {
    pub offset_from_bottom: usize,
    pub(super) anchor_top_row: Option<usize>,
    pub(super) last_tail_marker: usize,
    pub new_output_below: bool,
}

impl FullscreenTranscriptScroll {
    pub fn line_up(&mut self) {
        self.page_up(1);
    }

    pub fn line_down(&mut self) {
        self.page_down(1);
    }

    pub fn page_up(&mut self, rows: usize) {
        self.offset_from_bottom = self.offset_from_bottom.saturating_add(rows.max(1));
        self.anchor_top_row = None;
    }

    pub fn page_down(&mut self, rows: usize) {
        self.offset_from_bottom = self.offset_from_bottom.saturating_sub(rows.max(1));
        self.anchor_top_row = None;
        if self.offset_from_bottom == 0 {
            self.new_output_below = false;
        }
    }

    pub const fn jump_top(&mut self) {
        self.offset_from_bottom = usize::MAX;
        self.anchor_top_row = None;
    }

    pub const fn jump_bottom(&mut self) {
        self.offset_from_bottom = 0;
        self.anchor_top_row = None;
        self.new_output_below = false;
    }
}

/// A mouse drag selection over rendered transcript rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptSelection {
    pub(super) anchor_row: usize,
    pub(super) anchor_column: usize,
    pub(super) active_row: usize,
    pub(super) active_column: usize,
}

impl TranscriptSelection {
    #[must_use]
    pub const fn new(row: usize, column: usize) -> Self {
        Self {
            anchor_row: row,
            anchor_column: column,
            active_row: row,
            active_column: column,
        }
    }

    pub const fn update(&mut self, row: usize, column: usize) {
        self.active_row = row;
        self.active_column = column;
    }

    #[must_use]
    pub const fn moved(self) -> bool {
        self.anchor_row != self.active_row || self.anchor_column != self.active_column
    }

    #[must_use]
    pub fn ordered_points(self) -> ((usize, usize), (usize, usize)) {
        if (self.anchor_row, self.anchor_column) <= (self.active_row, self.active_column) {
            (
                (self.anchor_row, self.anchor_column),
                (self.active_row, self.active_column),
            )
        } else {
            (
                (self.active_row, self.active_column),
                (self.anchor_row, self.anchor_column),
            )
        }
    }

    #[must_use]
    pub fn columns_for_row(self, row: usize, width: usize) -> Option<(usize, usize)> {
        let ((start_row, start_column), (end_row, end_column)) = self.ordered_points();
        if row < start_row || row > end_row {
            return None;
        }
        if start_row == end_row {
            return Some((start_column, end_column.saturating_add(1).min(width)));
        }
        if row == start_row {
            Some((start_column, width))
        } else if row == end_row {
            Some((0, end_column.saturating_add(1).min(width)))
        } else {
            Some((0, width))
        }
    }
}

pub(super) fn fullscreen_transcript_area<V: BottomChromeView + ?Sized>(
    state: &V,
    total_width: u16,
    total_height: u16,
) -> Rect {
    let chrome_height = fullscreen_bottom_chrome_height_for_width(state, total_width).min(total_height);
    let content_area = Rect {
        x: 0,
        y: 0,
        width: total_width,
        height: total_height.saturating_sub(chrome_height),
    };
    fullscreen_content_areas(content_area, state).0
}

pub(super) fn fullscreen_tail_marker<V: BottomChromeView + ?Sized>(state: &V) -> usize {
    let finalized = state.conversation_lines().len().saturating_mul(1_000_000);
    let streaming_chars = state
        .streaming()
        .map_or(0usize, |streaming| streaming.accumulated.chars().count());
    finalized.saturating_add(streaming_chars)
}

pub(super) fn session_footer_has_sessions<V: BottomChromeView + ?Sized>(state: &V) -> bool {
    let entries = bottom_chrome_session_entries_with_workers(
        state.sessions_entries(),
        state.provider_worker_status(),
        state.focus(),
    );
    !entries.is_empty()
}

pub(super) fn session_footer_desired_rows<V: BottomChromeView + ?Sized>(state: &V) -> u16 {
    if !session_footer_has_sessions(state) {
        return 1;
    }
    let visible_entries = bottom_chrome_session_entries_with_workers(
        state.sessions_entries(),
        state.provider_worker_status(),
        state.focus(),
    );
    let rows = if visible_entries.is_empty() && !state.sessions_entries().is_empty() {
        0
    } else if visible_entries.is_empty() {
        1
    } else {
        visible_entries.len().saturating_add(2)
    };
    u16::try_from(rows).unwrap_or(u16::MAX).max(1)
}

/// Height of the pinned bottom chrome in fullscreen mode.
///
/// Ctrl+G and saved-session picker do not replace the chrome here; they render
/// as overlays above the full frame.
pub fn fullscreen_bottom_chrome_height<V: BottomChromeView + ?Sized>(state: &V) -> u16 {
    fullscreen_bottom_chrome_base_height(state).clamp(BOTTOM_CHROME_MIN_HEIGHT, BOTTOM_CHROME_MAX_HEIGHT)
}

pub fn fullscreen_transcript_page_rows<V: BottomChromeView + ?Sized>(state: &V, total_height: u16) -> usize {
    usize::from(
        total_height
            .saturating_sub(fullscreen_bottom_chrome_height(state).min(total_height))
            .max(1),
    )
}

pub fn fullscreen_transcript_scroll_available<V: BottomChromeView + ?Sized>(state: &V) -> bool {
    state.input().is_empty()
        && state.switcher().is_none()
        && state.saved_session_picker().is_none()
        && state.pending_tool_approval().is_none()
        && state.active_session_view().is_none()
        && !state.focus().is_child_view()
}

/// Legacy compatibility hook for reasoning-card clicks.
///
/// Reasoning is no longer rendered in the primary transcript, so no fullscreen
/// coordinate can target it. The data remains available through the verbose
/// transcript viewer.
pub const fn toggle_reasoning_at_fullscreen_point(
    _state: &mut TuiState,
    _scroll: &FullscreenTranscriptScroll,
    _total_width: u16,
    _total_height: u16,
    _column: u16,
    _row: u16,
) -> bool {
    false
}

pub(super) fn fullscreen_transcript_top_scroll<V: BottomChromeView + ?Sized>(
    state: &V,
    scroll: &FullscreenTranscriptScroll,
    area: Rect,
) -> usize {
    if area.height == 0 {
        return 0;
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
    if max_scroll == 0 {
        return 0;
    }
    if scroll.offset_from_bottom == 0 {
        return max_scroll;
    }
    if let Some(anchor_top_row) = scroll.anchor_top_row {
        return anchor_top_row.min(max_scroll);
    }
    max_scroll.saturating_sub(scroll.offset_from_bottom.min(max_scroll))
}
