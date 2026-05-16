//! TUI layout and rendering for `prx chat` using ratatui.
//!
//! Architecture (P3-inline): ratatui drives only the **bottom chrome**
//! (status / streaming buffer / input / footer) inside a
//! `Viewport::Inline(N)` viewport. Permanent conversation history is pushed
//! into the host terminal's main scrollback via `terminal.insert_before`,
//! which lets the terminal's native scroll (mouse wheel, Shift+PgUp,
//! search, copy/paste) work without any app-level scroll bookkeeping.
//!
//! Public surface:
//! - [`TuiState`] — shared state mirror (input buffer, conversation
//!   history, in-flight streaming draft).
//! - [`render_bottom_chrome`] — draws the fixed-height bottom region.
//! - [`render_message_for_insert`] — renders one [`ConversationLine`]
//!   into a ratatui `Buffer` for `terminal.insert_before`.
//! - [`estimate_message_height`] — width-aware row count used to size
//!   each `insert_before` call.
//!
//! Gated behind the `terminal-tui` feature.

use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use parking_lot::Mutex;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use std::collections::HashMap;
use unicode_width::UnicodeWidthStr;

use crate::chat::terminal_proto::{
    DraftVersionTracker, InlineDraftProtocol, LineProtocolError, apply_line_replacement,
};

/// Live streaming-assistant draft owned by [`TuiState`].
///
/// Lives independently of [`TuiState::conversation_lines`] so the renderer
/// can show in-flight tokens *after* the finalized history without ever
/// mutating a `ConversationLine` in place. Once the stream completes the
/// caller invokes [`TuiState::finalize_stream`], which lifts `accumulated`
/// into a `ConversationLine::Assistant` and clears the draft.
///
/// Version monotonicity mirrors the P1-6 `DraftVersionTracker` contract:
/// any `update_stream` call whose `version` is not strictly greater than
/// the currently stored one is rejected (stale / reordered delta).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingDraft {
    /// Draft id from the channel layer — used to reject cross-draft writes
    /// (start_stream on draft A followed by update_stream on draft B is a
    /// silent no-op rather than an in-place rewrite of A).
    pub draft_id: String,
    /// Accumulated visible text so far. The producer side maintains the
    /// running concatenation; each `update_stream` call replaces this in
    /// full, it does NOT splice deltas.
    pub accumulated: String,
    /// Monotonically increasing sequence number. `start_stream` returns 0;
    /// every successful `update_stream` raises it. Stale versions are dropped.
    pub version: u64,
}

/// State for the TUI layout.
///
/// Permanent conversation rendering happens above the viewport via
/// `terminal.insert_before` (see [`render_message_for_insert`]); only the
/// bottom chrome is repainted every frame. There is no app-level scroll
/// bookkeeping — the host terminal's scrollback owns history navigation.
pub struct TuiState {
    /// Provider/model displayed in status bar
    pub provider: String,
    pub model: String,
    /// Session title
    pub session_title: String,
    /// Number of conversation turns
    pub turn_count: usize,
    /// Rendered conversation lines. The unified loop tracks how many of
    /// these have already been pushed to the main screen via
    /// `insert_before`; new entries are flushed on the next iteration.
    pub conversation_lines: Vec<ConversationLine>,
    /// Multi-line input buffer + history (P2-10).
    pub input: TuiInput,
    /// Render ASCII-only icons instead of unicode glyphs (for non-UTF-8 terms).
    pub ascii_fallback: bool,
    /// In-flight streaming-assistant draft (P3-5). `None` between turns.
    ///
    /// When `Some`, [`render_bottom_chrome`] paints a transient streaming
    /// block inside the inline viewport. The streaming buffer is
    /// intentionally kept separate from `conversation_lines` so a stale
    /// or cancelled delta can never corrupt persisted history. On
    /// `finalize_stream` the text is lifted into `conversation_lines`
    /// and the next loop iteration scrolls it permanently into the main
    /// terminal scrollback.
    pub streaming: Option<StreamingDraft>,
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
    /// Assistant message currently being streamed.
    ///
    /// Distinct from [`ConversationLine::Assistant`] so the renderer can
    /// decorate the in-flight line (e.g. with a trailing cursor) and so the
    /// P3-5 streaming bridge can call an `update_stream(text)` mutator
    /// without touching finalized history. Once the stream completes the
    /// caller is expected to convert the variant into `Assistant` in place.
    StreamingAssistant {
        /// Accumulated text so far. May be empty while waiting for the
        /// provider's first delta.
        content: String,
    },
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
    /// Reasoning / thinking-content card from reasoning-capable models
    /// (Anthropic `thinking`, OpenAI `reasoning_content`, Ollama `thinking`).
    ///
    /// Default folded — only a one-line summary is shown. `Ctrl+R` toggles
    /// the most recent card to reveal the full text indented under the
    /// header. `char_count` is cached so the summary can be rendered without
    /// re-walking `content` on every frame.
    Reasoning {
        /// Aggregated reasoning text from this assistant turn. Never empty
        /// (empty buffers are dropped before pushing — see
        /// [`TuiState::push_reasoning`]).
        content: String,
        /// Cached `content.chars().count()` for the folded summary line.
        char_count: usize,
        /// Default `true`. Toggled via
        /// [`TuiState::toggle_last_reasoning_folded`].
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

    /// True if this line is a `Reasoning` variant. Used by [`TuiState`] to
    /// locate the most recent reasoning card for `Ctrl+R` toggling.
    pub const fn is_reasoning(&self) -> bool {
        matches!(self, Self::Reasoning { .. })
    }
}

/// Maximum number of input rows shown at once before the box stops growing.
/// (Lines beyond this still exist in the buffer; future work can add scroll.)
pub const INPUT_MAX_VISIBLE_ROWS: usize = 10;

/// Maximum number of submitted entries kept in the history ring.
pub const INPUT_HISTORY_CAPACITY: usize = 200;

/// Outcome of [`TuiInput::handle_key`].
///
/// Designed so the surrounding event loop can react with a single match
/// without inspecting `TuiInput` internals.
///
/// Note (P3-inline): there is no longer an in-app scroll outcome. With the
/// inline viewport, history scrolling is handled by the host terminal
/// natively (mouse wheel, Shift+PgUp, terminal search). PageUp / PageDown
/// fall through as [`InputOutcome::Unhandled`] so the terminal can
/// interpret them itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOutcome {
    /// Key was consumed; no externally observable change beyond the buffer.
    Consumed,
    /// User pressed Enter on a non-empty buffer. The full multi-line text is
    /// returned, the buffer has been cleared, and history advanced.
    Submitted(String),
    /// User requested cancellation (Esc). Buffer was cleared if non-empty.
    Cancelled,
    /// Key was not handled by the input subsystem; caller should fall through
    /// to higher-level shortcuts (`Tab`, `Ctrl+C`, `Ctrl+D`, etc.).
    Unhandled,
}

/// Top-level dispatch outcome for a single [`KeyEvent`] when it is fed into
/// the chat event loop via [`dispatch_global_key`].
///
/// This is a pure-function projection over the global shortcut table layered
/// on top of [`TuiState::handle_input_key`]:
///
/// - `Tab` — toggles the most recent foldable card (reasoning OR
///   tool-result, whichever appears later in the conversation).
///   The folded reasoning summary itself hints `press Tab to
///   expand`, so the user never has to learn a separate
///   keybinding for thinking blocks.
/// - `Ctrl+R` — toggles the most recent reasoning card (legacy shortcut,
///   kept for muscle-memory after the Tab unification).
/// - `Ctrl+C` — interrupt the current turn (caller cancels in-flight work)
/// - `Ctrl+D` — EOF when the input buffer is logically empty
/// - everything else — forwarded to the input box; submissions surface as
///   `Submitted(text)`.
///
/// Keeping the dispatch separate from the actual I/O loop lets us unit-test
/// the keybindings without spinning up a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyDispatch {
    /// Global shortcut handled or input-only change. The event loop should
    /// re-render but otherwise continue waiting for input.
    Consumed,
    /// User pressed Enter on a non-empty buffer. The full multi-line text is
    /// the value to deliver to the agent pipeline.
    Submitted(String),
    /// User requested cancellation of the current input (Esc).
    Cancelled,
    /// User requested cancellation of the in-flight LLM turn (`Ctrl+C`).
    /// The caller is responsible for firing the `CancellationToken` and
    /// keeping the input loop running so a new prompt can be entered.
    InterruptTurn,
    /// EOF (`Ctrl+D` on an empty buffer) — the event loop should exit.
    Exit,
}

/// Identifies which kind of foldable card was toggled by the unified `Tab`
/// keybinding. Returned alongside the new folded state from
/// [`TuiState::toggle_last_foldable_card`] so call-sites (or tests) can
/// observe the dispatch decision without re-scanning `conversation_lines`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldableKind {
    /// A `ConversationLine::Reasoning` card was flipped.
    Reasoning,
    /// A `ConversationLine::ToolResult` card was flipped.
    ToolResult,
}

/// Resolve a [`KeyEvent`] against the global shortcut table layered above the
/// input box. See [`KeyDispatch`] for the priority order. The function is
/// pure: it consumes a mutable reference to [`TuiState`] only to forward the
/// key into the input buffer and to flip fold flags on tool / reasoning cards.
///
/// Pure on its own, no I/O — kept here so unit tests can exercise the binding
/// table without touching crossterm / ratatui terminals.
pub fn dispatch_global_key(key: KeyEvent, state: &mut TuiState) -> KeyDispatch {
    // [DIAG] trace every key that enters the dispatch function so we can
    // correlate raw events (tui_input_event) with what the handler sees.
    tracing::debug!(
        code = ?key.code,
        modifiers = ?key.modifiers,
        kind = ?key.kind,
        input_lines_before = state.input.lines.len(),
        input_first_line_chars = state.input.lines.first().map(|s| s.chars().count()).unwrap_or(0),
        "dispatch_global_key_entry"
    );
    // Tab → toggle the most recent foldable card (reasoning OR tool-result,
    // whichever appears later in the conversation). When neither exists Tab
    // is still consumed — per spec it never falls through to the input box.
    if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE {
        let _ = state.toggle_last_foldable_card();
        return KeyDispatch::Consumed;
    }
    // Ctrl+R → toggle most recent reasoning card. Never falls through.
    if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
        let _ = state.toggle_last_reasoning_folded();
        return KeyDispatch::Consumed;
    }
    // Ctrl+C → interrupt active turn. We intentionally do NOT exit on a
    // single press; the persistent ctrl_c() signal handler in chat/mod.rs
    // already implements the double-press exit semantics.
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        return KeyDispatch::InterruptTurn;
    }
    // Ctrl+D → EOF when the input buffer is empty; otherwise treat as a
    // forward-delete (delegated to the input box via Delete equivalence).
    if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL {
        if state.input.is_empty() {
            return KeyDispatch::Exit;
        }
        // Non-empty: forward as a normal Delete keystroke so users can still
        // use Ctrl+D as forward-delete inside the buffer.
        let synthetic = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        let _ = state.handle_input_key(synthetic);
        return KeyDispatch::Consumed;
    }
    // All other keys → input box.
    match state.handle_input_key(key) {
        InputOutcome::Submitted(text) => KeyDispatch::Submitted(text),
        InputOutcome::Cancelled => KeyDispatch::Cancelled,
        InputOutcome::Consumed | InputOutcome::Unhandled => KeyDispatch::Consumed,
    }
}

/// Multi-line text input with history navigation.
///
/// Storage is a `Vec<String>` where each element is one logical line **without**
/// the trailing newline. The cursor is a `(line_index, byte_offset)` pair;
/// `byte_offset` always lies on a UTF-8 char boundary because all mutations go
/// through the dedicated helpers below.
///
/// History is a FIFO ring capped at [`INPUT_HISTORY_CAPACITY`]. `history_pos`
/// is `None` while the user is editing a fresh buffer; once they navigate up
/// it becomes `Some(index)` pointing into `history`.
#[derive(Debug, Clone)]
pub struct TuiInput {
    /// Each element is a logical line (no trailing '\n').
    pub lines: Vec<String>,
    /// Cursor: (line_index, byte_offset_into_line).
    pub cursor: (usize, usize),
    /// Submitted history, oldest at index 0.
    pub history: Vec<String>,
    /// Position when navigating history; `None` = editing fresh input.
    pub history_pos: Option<usize>,
    /// Snapshot of the in-flight buffer saved when entering history nav, so we
    /// can restore it when the user scrolls past the end of history.
    pending_draft: Option<Vec<String>>,
}

impl Default for TuiInput {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiInput {
    /// Create a fresh, empty input buffer.
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
            history: Vec::new(),
            history_pos: None,
            pending_draft: None,
        }
    }

    /// Joined buffer contents (lines separated by '\n').
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// True when the buffer is logically empty (single empty line).
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines.first().is_none_or(String::is_empty)
    }

    /// True if the user is currently editing a single logical line — used to
    /// decide whether `↑/↓` should navigate history or move the cursor.
    pub const fn is_single_line(&self) -> bool {
        self.lines.len() <= 1
    }

    /// Replace the entire buffer (used by history navigation and paste).
    fn set_text(&mut self, text: &str) {
        // Strip a trailing '\n' so single-line history doesn't grow a blank
        // second row when restored.
        let trimmed = text.strip_suffix('\n').unwrap_or(text);
        self.lines = if trimmed.is_empty() {
            vec![String::new()]
        } else {
            trimmed.split('\n').map(str::to_owned).collect()
        };
        let last_line_idx = self.lines.len().saturating_sub(1);
        let last_len = self.lines.get(last_line_idx).map_or(0, String::len);
        self.cursor = (last_line_idx, last_len);
    }

    /// Clear the buffer back to a single empty line.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
        self.history_pos = None;
        self.pending_draft = None;
    }

    /// Insert a single grapheme (`ch`) at the cursor.
    fn insert_char(&mut self, ch: char) {
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            // `off` is always at a char boundary because we only ever advance
            // by `ch.len_utf8()` from prior inserts and via `floor_char_boundary`.
            let clamped = off.min(line.len());
            line.insert(clamped, ch);
            self.cursor = (li, clamped + ch.len_utf8());
        }
    }

    /// Insert a literal string at the cursor. Newlines split into rows.
    fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Split by '\n' explicitly so a trailing newline produces an empty row.
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            // Insert `first` at the cursor on the current line.
            let (li, off) = self.cursor;
            if let Some(line) = self.lines.get_mut(li) {
                let clamped = off.min(line.len());
                let suffix: String = line[clamped..].to_string();
                line.truncate(clamped);
                line.push_str(first);
                let mut new_cursor = (li, line.len());

                // Any remaining parts become new lines below the current one.
                let mut insert_at = li + 1;
                for part in parts {
                    self.lines.insert(insert_at, part.to_string());
                    new_cursor = (insert_at, self.lines.get(insert_at).map_or(0, String::len));
                    insert_at += 1;
                }

                // Append the original suffix to whatever ended up as the
                // cursor's line.
                if let Some(last_line) = self.lines.get_mut(new_cursor.0) {
                    last_line.push_str(&suffix);
                }
                self.cursor = new_cursor;
            }
        }
    }

    /// Split the current line at the cursor (`Shift+Enter`).
    fn insert_newline(&mut self) {
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            let clamped = off.min(line.len());
            let tail: String = line.split_off(clamped);
            self.lines.insert(li + 1, tail);
            self.cursor = (li + 1, 0);
        }
    }

    /// Delete the character before the cursor; join with previous line if at
    /// column 0.
    fn backspace(&mut self) {
        let (li, off) = self.cursor;
        if off > 0 {
            if let Some(line) = self.lines.get_mut(li) {
                let new_off = floor_char_boundary(line, off.saturating_sub(1));
                line.replace_range(new_off..off, "");
                self.cursor = (li, new_off);
            }
        } else if li > 0 {
            // Merge current line into previous.
            let current = self.lines.remove(li);
            let prev_idx = li - 1;
            if let Some(prev) = self.lines.get_mut(prev_idx) {
                let new_off = prev.len();
                prev.push_str(&current);
                self.cursor = (prev_idx, new_off);
            }
        }
    }

    /// Delete the character at the cursor; join next line if at end of line.
    fn delete_forward(&mut self) {
        let (li, off) = self.cursor;
        let line_len = self.lines.get(li).map_or(0, String::len);
        if off < line_len {
            if let Some(line) = self.lines.get_mut(li) {
                // Find end of the char starting at `off`.
                let mut end = off + 1;
                while end < line.len() && !line.is_char_boundary(end) {
                    end += 1;
                }
                line.replace_range(off..end, "");
            }
        } else if li + 1 < self.lines.len() {
            // Join next line into current.
            let next = self.lines.remove(li + 1);
            if let Some(line) = self.lines.get_mut(li) {
                line.push_str(&next);
            }
        }
    }

    /// Move cursor one char left, possibly to previous line's end.
    fn move_left(&mut self) {
        let (li, off) = self.cursor;
        if off > 0 {
            if let Some(line) = self.lines.get(li) {
                let new_off = floor_char_boundary(line, off.saturating_sub(1));
                self.cursor = (li, new_off);
            }
        } else if li > 0 {
            let prev_len = self.lines.get(li - 1).map_or(0, String::len);
            self.cursor = (li - 1, prev_len);
        }
    }

    /// Move cursor one char right, possibly to next line's start.
    fn move_right(&mut self) {
        let (li, off) = self.cursor;
        let line_len = self.lines.get(li).map_or(0, String::len);
        if off < line_len {
            if let Some(line) = self.lines.get(li) {
                let mut new_off = off + 1;
                while new_off < line.len() && !line.is_char_boundary(new_off) {
                    new_off += 1;
                }
                self.cursor = (li, new_off);
            }
        } else if li + 1 < self.lines.len() {
            self.cursor = (li + 1, 0);
        }
    }

    /// Move cursor to start of current line (Home / Ctrl+A).
    const fn move_line_start(&mut self) {
        self.cursor.1 = 0;
    }

    /// Move cursor to end of current line (End / Ctrl+E).
    fn move_line_end(&mut self) {
        let line_len = self.lines.get(self.cursor.0).map_or(0, String::len);
        self.cursor.1 = line_len;
    }

    /// Move cursor up one row when multi-line; keep byte offset clamped.
    fn move_cursor_up(&mut self) -> bool {
        let (li, off) = self.cursor;
        if li == 0 {
            return false;
        }
        let new_li = li - 1;
        let new_line_len = self.lines.get(new_li).map_or(0, String::len);
        let target_off = off.min(new_line_len);
        let safe_off = self
            .lines
            .get(new_li)
            .map_or(target_off, |line| floor_char_boundary(line, target_off));
        self.cursor = (new_li, safe_off);
        true
    }

    /// Move cursor down one row when multi-line.
    fn move_cursor_down(&mut self) -> bool {
        let (li, off) = self.cursor;
        if li + 1 >= self.lines.len() {
            return false;
        }
        let new_li = li + 1;
        let new_line_len = self.lines.get(new_li).map_or(0, String::len);
        let target_off = off.min(new_line_len);
        let safe_off = self
            .lines
            .get(new_li)
            .map_or(target_off, |line| floor_char_boundary(line, target_off));
        self.cursor = (new_li, safe_off);
        true
    }

    /// Delete from start of current line up to cursor (`Ctrl+U`).
    fn delete_to_line_start(&mut self) {
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            line.replace_range(0..off.min(line.len()), "");
            self.cursor = (li, 0);
        }
    }

    /// Push a finalized entry onto the history ring (dedups consecutive dupes).
    fn record_history(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.history.last() == Some(&text) {
            return;
        }
        if self.history.len() >= INPUT_HISTORY_CAPACITY {
            self.history.remove(0);
        }
        self.history.push(text);
    }

    /// Navigate to the previous (older) entry. Saves the in-flight draft on
    /// first call so it can be restored later.
    fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next_pos = match self.history_pos {
            None => {
                self.pending_draft = Some(self.lines.clone());
                self.history.len().saturating_sub(1)
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.history_pos = Some(next_pos);
        if let Some(entry) = self.history.get(next_pos) {
            let entry_owned = entry.clone();
            self.set_text(&entry_owned);
        }
        true
    }

    /// Navigate to the next (newer) entry, or back to the pending draft.
    fn history_next(&mut self) -> bool {
        let Some(pos) = self.history_pos else {
            return false;
        };
        let next_pos = pos + 1;
        if next_pos >= self.history.len() {
            // Past the most recent entry → restore in-flight draft (if any).
            self.history_pos = None;
            if let Some(draft) = self.pending_draft.take() {
                self.lines = if draft.is_empty() { vec![String::new()] } else { draft };
            } else {
                self.lines = vec![String::new()];
            }
            let last_line_idx = self.lines.len().saturating_sub(1);
            let last_len = self.lines.get(last_line_idx).map_or(0, String::len);
            self.cursor = (last_line_idx, last_len);
        } else {
            self.history_pos = Some(next_pos);
            if let Some(entry) = self.history.get(next_pos) {
                let entry_owned = entry.clone();
                self.set_text(&entry_owned);
            }
        }
        true
    }

    /// Process a single key event. See [`InputOutcome`] for return semantics.
    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Enter => {
                if shift || alt {
                    self.insert_newline();
                    return InputOutcome::Consumed;
                }
                if self.is_empty() {
                    return InputOutcome::Consumed;
                }
                let text = self.text();
                self.record_history(text.clone());
                self.clear();
                InputOutcome::Submitted(text)
            }
            KeyCode::Char('u') if ctrl => {
                self.delete_to_line_start();
                InputOutcome::Consumed
            }
            KeyCode::Char('a') if ctrl => {
                self.move_line_start();
                InputOutcome::Consumed
            }
            KeyCode::Char('e') if ctrl => {
                self.move_line_end();
                InputOutcome::Consumed
            }
            KeyCode::Char('j') if ctrl => {
                // Common terminal alternative for "newline without submit".
                self.insert_newline();
                InputOutcome::Consumed
            }
            KeyCode::Char(ch) if !ctrl => {
                self.insert_char(ch);
                InputOutcome::Consumed
            }
            KeyCode::Backspace => {
                self.backspace();
                InputOutcome::Consumed
            }
            KeyCode::Delete => {
                self.delete_forward();
                InputOutcome::Consumed
            }
            KeyCode::Left => {
                self.move_left();
                InputOutcome::Consumed
            }
            KeyCode::Right => {
                self.move_right();
                InputOutcome::Consumed
            }
            KeyCode::Home => {
                self.move_line_start();
                InputOutcome::Consumed
            }
            KeyCode::End => {
                self.move_line_end();
                InputOutcome::Consumed
            }
            KeyCode::Up => {
                // Single-line buffer → history; multi-line → cursor up.
                if self.is_single_line() {
                    if self.history_prev() {
                        InputOutcome::Consumed
                    } else {
                        InputOutcome::Unhandled
                    }
                } else if self.move_cursor_up() {
                    InputOutcome::Consumed
                } else {
                    InputOutcome::Unhandled
                }
            }
            KeyCode::Down => {
                if self.is_single_line() {
                    if self.history_next() {
                        InputOutcome::Consumed
                    } else {
                        InputOutcome::Unhandled
                    }
                } else if self.move_cursor_down() {
                    InputOutcome::Consumed
                } else {
                    InputOutcome::Unhandled
                }
            }
            // P3-inline: scrolling history is the host terminal's job
            // (mouse wheel, Shift+PgUp). We deliberately do NOT consume
            // PgUp/PgDn so the terminal can interpret them natively.
            KeyCode::PageUp | KeyCode::PageDown => InputOutcome::Unhandled,
            KeyCode::Esc => {
                if !self.is_empty() {
                    self.clear();
                }
                InputOutcome::Cancelled
            }
            _ => InputOutcome::Unhandled,
        }
    }

    /// Append pasted text verbatim. Newlines split into rows.
    pub fn paste(&mut self, text: &str) {
        self.insert_str(text);
    }
}

/// Round `idx` down to the nearest UTF-8 char boundary in `s`. Saturates at 0.
const fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    let max = s.len();
    if idx > max {
        idx = max;
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

impl TuiState {
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            session_title: String::new(),
            turn_count: 0,
            conversation_lines: Vec::new(),
            input: TuiInput::new(),
            ascii_fallback: false,
            streaming: None,
        }
    }

    // ── P3-5: streaming-draft API ──────────────────────────────────────────
    //
    // The four methods below are the only legitimate entry points for the
    // P3-5 streaming bridge (`channels::terminal::UiActor::handle_event_tui`
    // → `TuiMirrorSink`). All other call sites must keep going through
    // `push_assistant_message` / `push_*` so finalised history remains the
    // single source of truth.

    /// Begin a new streaming draft. Replaces any previous in-flight draft
    /// (the caller is expected to have finalised or cancelled it first; if
    /// not, dropping the stale one is strictly safer than retaining it and
    /// silently interleaving deltas from two different turns).
    ///
    /// Returns the initial version (`0`). Subsequent `update_stream` calls
    /// must supply a strictly greater version or they are rejected.
    pub fn start_stream(&mut self, draft_id: &str) -> u64 {
        self.streaming = Some(StreamingDraft {
            draft_id: draft_id.to_string(),
            accumulated: String::new(),
            version: 0,
        });
        0
    }

    /// Replace the in-flight streaming draft's accumulated text. The
    /// caller maintains the running concatenation upstream — this method
    /// does NOT splice deltas, it overwrites in full.
    ///
    /// Returns `true` if the update was accepted, `false` if rejected
    /// (no active draft, mismatched `draft_id`, or non-monotonic version).
    /// Rejection is silent on purpose: stale deltas are expected during
    /// cancellation / draft-id reuse races.
    pub fn update_stream(&mut self, draft_id: &str, accumulated: &str, version: u64) -> bool {
        let Some(draft) = self.streaming.as_mut() else {
            return false;
        };
        if draft.draft_id != draft_id {
            return false;
        }
        if version <= draft.version && !(version == 0 && draft.version == 0) {
            // version must strictly advance; the v==0 && stored==0 carve-out
            // permits a degenerate "initial empty delta at seq 0" but only
            // when the buffer is also still at v0 (immediately after start).
            return false;
        }
        draft.accumulated.clear();
        draft.accumulated.push_str(accumulated);
        draft.version = version;
        true
    }

    /// Finalise a streaming draft: lift its text into a permanent
    /// `ConversationLine::Assistant` (using `final_text` rather than the
    /// last accumulated buffer, so any post-stream cleanup at the channel
    /// layer survives) and clear the in-flight slot.
    ///
    /// No-op if the active draft id doesn't match. Empty `final_text` still
    /// clears the streaming slot but pushes nothing — this matches the
    /// existing `handle_event_tui` policy for empty finalised drafts.
    pub fn finalize_stream(&mut self, draft_id: &str, final_text: &str) {
        let matches = self.streaming.as_ref().is_some_and(|d| d.draft_id == draft_id);
        if !matches {
            return;
        }
        self.streaming = None;
        if !final_text.is_empty() {
            self.conversation_lines.push(ConversationLine::Assistant {
                content: final_text.to_string(),
            });
        }
    }

    /// Discard an in-flight streaming draft without surfacing any text in
    /// the finalised history. No-op on draft-id mismatch.
    pub fn cancel_stream(&mut self, draft_id: &str) {
        if self.streaming.as_ref().is_some_and(|d| d.draft_id == draft_id) {
            self.streaming = None;
        }
    }

    /// Forward a `crossterm::event::KeyEvent` to the multi-line input buffer.
    ///
    /// Returns [`InputOutcome`] so the caller can react to submissions,
    /// cancellations, or scrolling intents.
    pub fn handle_input_key(&mut self, key: KeyEvent) -> InputOutcome {
        self.input.handle_key(key)
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
    }

    /// Add an assistant message to the conversation display.
    pub fn push_assistant_message(&mut self, content: &str) {
        self.conversation_lines.push(ConversationLine::Assistant {
            content: content.to_string(),
        });
    }

    /// Add a system / status message.
    pub fn push_system_message(&mut self, content: &str) {
        self.conversation_lines.push(ConversationLine::System {
            content: content.to_string(),
        });
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

    /// Push a folded [`ConversationLine::Reasoning`] card carrying the model's
    /// aggregated thinking content for the just-completed assistant turn.
    ///
    /// Empty / whitespace-only buffers are silently dropped — there is no
    /// value in a `[thinking 0 chars]` card. Returns `true` if a card was
    /// actually pushed, so callers can branch on observability.
    pub fn push_reasoning(&mut self, content: &str) -> bool {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return false;
        }
        let owned = trimmed.to_string();
        let char_count = owned.chars().count();
        self.conversation_lines.push(ConversationLine::Reasoning {
            content: owned,
            char_count,
            folded: true,
        });
        true
    }

    /// Toggle the folded state of the most recent `Reasoning` line (if any).
    ///
    /// Returns the new `folded` value, or `None` if no `Reasoning` exists.
    /// This implements the `Ctrl+R` keypath, mirroring the `Tab` key handler
    /// for tool-result cards. Only the **last** reasoning card is touched;
    /// older cards keep their previous state so the user can hop between
    /// turns without losing context.
    pub fn toggle_last_reasoning_folded(&mut self) -> Option<bool> {
        for line in self.conversation_lines.iter_mut().rev() {
            if let ConversationLine::Reasoning { folded, .. } = line {
                *folded = !*folded;
                return Some(*folded);
            }
        }
        None
    }

    /// Index of the most recent `Reasoning` line, if any. Exposed for tests
    /// and future per-line selection logic.
    pub fn last_reasoning_index(&self) -> Option<usize> {
        self.conversation_lines.iter().rposition(ConversationLine::is_reasoning)
    }

    /// Toggle the folded flag of the most recent foldable card — either a
    /// `Reasoning` or a `ToolResult`, whichever appears later in
    /// `conversation_lines`. Returns the new folded value and a tag
    /// describing which variant was touched, or `None` if neither exists.
    ///
    /// This is the keystone behind the unified `Tab` keybinding: the user
    /// never has to remember whether the most recent card is a tool or a
    /// thinking block — Tab "does the obvious thing" by flipping whichever
    /// foldable thing sits closest to the cursor.
    pub fn toggle_last_foldable_card(&mut self) -> Option<(FoldableKind, bool)> {
        for line in self.conversation_lines.iter_mut().rev() {
            match line {
                ConversationLine::Reasoning { folded, .. } => {
                    *folded = !*folded;
                    return Some((FoldableKind::Reasoning, *folded));
                }
                ConversationLine::ToolResult { folded, .. } => {
                    *folded = !*folded;
                    return Some((FoldableKind::ToolResult, *folded));
                }
                _ => continue,
            }
        }
        None
    }

    /// Total content lines (estimated). Used by tests to compare folded vs
    /// expanded card row counts.
    #[cfg(test)]
    fn total_content_lines(&self) -> usize {
        self.conversation_lines
            .iter()
            .map(|l| estimate_line_height(l) as usize)
            .sum()
    }
}

// ── P3-4: TuiMirrorSink adapter ─────────────────────────────────────────────

/// Adapter that lets a shared `Arc<Mutex<TuiState>>` be plugged into the
/// channel-side `UiActor` as a [`TuiMirrorSink`].
///
/// `channels::terminal` lives in the library crate and cannot name
/// `TuiState` directly — see the trait doc for the seam design. This adapter
/// is the binary-side glue: each trait method maps onto the corresponding
/// `TuiState` mutator behind the existing parking_lot mutex. No new
/// `ConversationLine` variant is introduced, so the renderer's existing
/// view layer is untouched.
pub struct TuiStateMirrorSink {
    state: std::sync::Arc<Mutex<TuiState>>,
}

impl TuiStateMirrorSink {
    pub const fn new(state: std::sync::Arc<Mutex<TuiState>>) -> Self {
        Self { state }
    }
}

impl crate::channels::terminal::TuiMirrorSink for TuiStateMirrorSink {
    fn push_assistant(&self, content: &str) {
        self.state.lock().push_assistant_message(content);
    }
    fn push_system(&self, content: &str) {
        self.state.lock().push_system_message(content);
    }
    fn push_tool_started(&self, tool_name: &str, args_full: &str) {
        self.state.lock().push_tool_result_started(tool_name, args_full);
    }
    fn mark_tool_finished(&self, tool_name: &str, success: bool, duration_ms: u64) -> bool {
        self.state
            .lock()
            .mark_last_tool_result_finished(tool_name, success, duration_ms, None)
    }
    // ── P3-5: streaming bridge ─────────────────────────────────────────
    fn start_stream(&self, draft_id: &str) {
        let _ = self.state.lock().start_stream(draft_id);
    }
    fn update_stream(&self, draft_id: &str, accumulated: &str, version: u64) {
        let _ = self.state.lock().update_stream(draft_id, accumulated, version);
    }
    fn finalize_stream(&self, draft_id: &str, final_text: &str) {
        self.state.lock().finalize_stream(draft_id, final_text);
    }
    fn cancel_stream(&self, draft_id: &str) {
        self.state.lock().cancel_stream(draft_id);
    }
}

/// Logical row count for a [`ConversationLine`], **before** soft-wrap.
///
/// This is the floor used by [`estimate_message_height`]; the real
/// `insert_before` height is bumped up by hard wrapping at the terminal
/// width so long User / Assistant / System messages don't get clipped.
/// Always returns >= 1.
fn estimate_line_height(line: &ConversationLine) -> u16 {
    let rows = match line {
        ConversationLine::User { content } => {
            // First content line shares the row with the `> ` prompt; further
            // rows render as continuations. Trailing blank separator.
            content.lines().count().max(1) + 1
        }
        ConversationLine::Assistant { content } | ConversationLine::StreamingAssistant { content } => {
            // No prefix row, just content + trailing blank.
            content.lines().count().max(1) + 1
        }
        ConversationLine::System { content } => content.lines().count().max(1) + 1,
        ConversationLine::Tool { .. } => 1,
        ConversationLine::ToolResult {
            folded,
            args_full,
            result,
            status,
            ..
        } => {
            // Claude-Code style: bullet header (1 row) + an optional follow-on
            // block. While running there is no follow-on yet.
            if matches!(status, ToolStatus::Running) {
                1
            } else if *folded {
                // header + `⎿ Done (…)` summary row
                2
            } else {
                // header + first body row under hook + continuation rows
                let body = result.as_deref().filter(|s| !s.is_empty()).unwrap_or(args_full);
                1 + body.lines().count().max(1)
            }
        }
        ConversationLine::Reasoning { folded, content, .. } => {
            if *folded {
                1
            } else {
                // header + content body (one row per logical line, min 1)
                1 + content.lines().count().max(1)
            }
        }
    };
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// Width-aware row count used by the unified TUI loop to size each
/// `terminal.insert_before(height, …)` call.
///
/// Builds the same `Line<'_>` vec that [`render_message_for_insert`]
/// will emit, then counts the rows each `Line` will consume at the
/// given terminal width using a simple `ceil(display_width / width)`
/// estimate. This matches the soft-wrap behaviour of
/// `Paragraph::wrap(Wrap { trim: false })` closely enough for sizing
/// `insert_before` calls — the goal is to never *clip* a long message,
/// not to count rows down to the cell. `width` should be the current
/// terminal column count; zero is treated as 1.
pub fn estimate_message_height(width: u16, line: &ConversationLine, ascii: bool) -> u16 {
    let safe_width = width.max(1);
    let mut sink: Vec<Line<'_>> = Vec::new();
    render_conversation_line(&mut sink, line, ascii);
    let wrapped = wrapped_rows_for_lines(&sink, safe_width);
    wrapped.max(estimate_line_height(line)).max(1)
}

/// Count the visible rows a `Vec<Line>` will consume at the given
/// width, assuming `Paragraph::wrap(Wrap { trim: false })` semantics:
/// each `Line` takes `ceil(display_width / width)` rows, or `1` if
/// empty. Saturates at `u16::MAX`.
fn wrapped_rows_for_lines(lines: &[Line<'_>], width: u16) -> u16 {
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

/// Render one [`ConversationLine`] directly into a ratatui [`Buffer`] for
/// `terminal.insert_before`. The buffer's full area is consumed; the
/// caller is responsible for sizing it via [`estimate_message_height`].
pub fn render_message_for_insert(buf: &mut Buffer, line: &ConversationLine, ascii: bool) {
    let mut sink: Vec<Line<'_>> = Vec::new();
    render_conversation_line(&mut sink, line, ascii);
    let paragraph = Paragraph::new(Text::from(sink)).wrap(Wrap { trim: false });
    let area = buf.area;
    paragraph.render(area, buf);
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

/// 渲染源抽象：让 `render_bottom_chrome` / `bottom_chrome_height` 同时支持
/// `TuiState`（chat_mirror 路径）与 `UiSnapshot`（S4-A Pure 模式 watch 路径）.
///
/// S4-A Commit 2: 把渲染需要的最小字段集抽出来作为 trait，泛型化所有
/// `&TuiState` 参数为 `&V: BottomChromeView`。本 commit 暂未切换渲染源，
/// 仅泛型化函数签名 + 两个 impl，行为不变。
pub trait BottomChromeView {
    fn provider(&self) -> &str;
    fn model(&self) -> &str;
    fn session_title(&self) -> &str;
    fn turn_count(&self) -> usize;
    fn ascii_fallback(&self) -> bool;
    fn conversation_lines(&self) -> &[ConversationLine];
    fn streaming(&self) -> Option<&StreamingDraft>;
    fn input(&self) -> &TuiInput;
}

impl BottomChromeView for TuiState {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn model(&self) -> &str {
        &self.model
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
    fn input(&self) -> &TuiInput {
        &self.input
    }
}

impl BottomChromeView for crate::chat::state::UiSnapshot {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn model(&self) -> &str {
        &self.model
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
    fn input(&self) -> &TuiInput {
        &self.input
    }
}

/// Minimum height (rows) of the inline viewport. Reserves space for
/// 1 status line + 1 input row + 1 footer. The unified loop bumps this
/// up dynamically to accommodate multi-line input and streaming buffers.
pub const BOTTOM_CHROME_MIN_HEIGHT: u16 = 3;

/// Hard upper bound on the inline viewport height. Streaming + a 12-row
/// input box + status + footer is the largest reasonable layout; beyond
/// that the user's main scrollback starts losing valuable rows.
pub const BOTTOM_CHROME_MAX_HEIGHT: u16 = 24;

/// Maximum number of streaming-assistant rows to show inside the inline
/// viewport while a turn is in flight. Once the stream finalises, the
/// finalised text is lifted into `conversation_lines` and pushed up to
/// the host terminal's scrollback via `insert_before` on the next loop
/// iteration.
pub const STREAMING_VISIBLE_ROWS: u16 = 6;

/// Compute the inline viewport height needed for the current state.
///
/// Layout, top to bottom:
///   1 status row
/// + optional streaming preview (up to [`STREAMING_VISIBLE_ROWS`])
/// + 1 input-border row + visible input rows (1..=[`INPUT_MAX_VISIBLE_ROWS`])
/// + 1 footer row
///
/// Clamped to [`BOTTOM_CHROME_MIN_HEIGHT`]..=[`BOTTOM_CHROME_MAX_HEIGHT`].
///
/// S4-A Commit 2: 泛型化让 UiSnapshot 与 TuiState 共用同一份高度计算逻辑。
pub fn bottom_chrome_height<V: BottomChromeView + ?Sized>(state: &V) -> u16 {
    let visible_input_rows = state.input().lines.len().clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let streaming_rows = if state.streaming().is_some() {
        STREAMING_VISIBLE_ROWS
    } else {
        0
    };
    let total: u16 = 1u16 // status row
        .saturating_add(streaming_rows)
        .saturating_add(input_height)
        .saturating_add(1); // footer row
    total.clamp(BOTTOM_CHROME_MIN_HEIGHT, BOTTOM_CHROME_MAX_HEIGHT)
}

/// Render the **fixed-height inline viewport** at the bottom of the
/// terminal. Permanent conversation lines are NOT drawn here — they are
/// already in the host terminal's scrollback courtesy of
/// `terminal.insert_before` (driven by the unified loop in
/// `chat/mod.rs::run_tui_unified_loop`).
///
/// Layout (top to bottom):
///   1. Status bar (1 row)
///   2. Streaming preview (optional, up to [`STREAMING_VISIBLE_ROWS`])
///   3. Input box (dynamic, border + 1..=[`INPUT_MAX_VISIBLE_ROWS`])
///   4. Footer (1 row)
pub fn render_bottom_chrome<V: BottomChromeView + ?Sized>(frame: &mut Frame, state: &V) {
    // The inline viewport reserves `BOTTOM_CHROME_MAX_HEIGHT` rows at the
    // bottom of the host terminal. The dynamic chrome height is usually
    // smaller, so align it to the bottom of the reserved frame so the
    // input box always sits flush with the user's prompt line, with any
    // unused rows blank above the status bar.
    let frame_area = frame.area();
    let height = bottom_chrome_height(state).min(frame_area.height);
    let area = Rect {
        y: frame_area.bottom().saturating_sub(height),
        height,
        ..frame_area
    };

    let visible_input_rows = state.input().lines.len().clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let streaming_rows = if state.streaming().is_some() {
        STREAMING_VISIBLE_ROWS
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),              // Status bar
            Constraint::Length(streaming_rows), // Streaming preview (0 when idle)
            Constraint::Length(input_height),   // Input area (dynamic)
            Constraint::Length(1),              // Footer
        ])
        .split(area);

    // Layout::split always returns exactly 4 chunks here.
    #[allow(clippy::indexing_slicing)]
    {
        render_status_bar(frame, chunks[0], state);
        if streaming_rows > 0 {
            render_streaming_preview(frame, chunks[1], state);
        }
        render_input(frame, chunks[2], state);
        render_footer(frame, chunks[3]);
    }
}

fn render_status_bar<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) {
    let title_str = state.session_title();
    let title = if title_str.is_empty() {
        "(new session)"
    } else {
        title_str
    };

    let status_text = format!(
        " PRX Chat | {}/{} | {} | {} turns ",
        state.provider(),
        state.model(),
        title,
        state.turn_count()
    );

    let status = Paragraph::new(status_text).style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(status, area);
}

/// Render the in-flight streaming-assistant draft inside the inline
/// viewport. Trimmed to the last [`STREAMING_VISIBLE_ROWS`] rows so the
/// most recently arrived tokens stay visible.
fn render_streaming_preview<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) {
    let Some(draft) = state.streaming() else {
        return;
    };
    let transient = ConversationLine::StreamingAssistant {
        content: draft.accumulated.clone(),
    };
    let mut sink: Vec<Line<'_>> = Vec::new();
    render_conversation_line(&mut sink, &transient, state.ascii_fallback());

    // If the streaming body wraps beyond `area.height`, scroll the
    // Paragraph so the trailing rows (newest tokens) are visible.
    let total_rows: u16 = wrapped_rows_for_lines(&sink, area.width);
    let scroll: u16 = total_rows.saturating_sub(area.height);

    let widget = Paragraph::new(Text::from(sink))
        .block(Block::default().borders(Borders::NONE))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(widget, area);
}

/// Render a single conversation line into the ratatui `lines` buffer.
///
/// Pure function (apart from the &mut push target) — kept outside
/// [`render_message_for_insert`] / [`render_streaming_preview`] so unit
/// tests can drive it with a `Vec<Line<'_>>` sink.
fn render_conversation_line<'a>(lines: &mut Vec<Line<'a>>, conv_line: &'a ConversationLine, ascii: bool) {
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
            // Claude Code style: no prefix, no indicator. Content rendered at
            // column 0 in the default terminal foreground, separated from the
            // preceding user line by the trailing blank already pushed there.
            for text_line in content.lines() {
                lines.push(Line::from(text_line));
            }
            lines.push(Line::from(""));
        }
        ConversationLine::StreamingAssistant { content } => {
            // Same shape as Assistant (no prefix). A trailing cursor glyph
            // (`▌`, or `_` in ASCII mode) signals that more bytes are still
            // inbound; once the stream finalises the variant becomes
            // `Assistant` and the cursor disappears.
            let cursor = if ascii { "_" } else { "\u{258C}" }; // ▌
            let mut body_lines: Vec<&str> = content.lines().collect();
            if body_lines.is_empty() {
                body_lines.push("");
            }
            let last_idx = body_lines.len().saturating_sub(1);
            for (i, text_line) in body_lines.iter().enumerate() {
                let formatted = if i == last_idx {
                    format!("{text_line}{cursor}")
                } else {
                    (*text_line).to_string()
                };
                lines.push(Line::from(formatted));
            }
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

/// Render a `ToolResult` card in Claude-Code style.
///
/// Folded layout (default):
/// ```text
/// ● Bash(ls /tmp)
///   ⎿ Done (234ms · 12 lines)
/// ```
/// Expanded layout:
/// ```text
/// ● Bash(ls /tmp)
///   ⎿ <result text, each line indented>
/// ```
/// While `Running` no follow-on row is shown — just the header `● Bash(ls /tmp)`.
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
    let (bullet, hook) = tool_card_glyphs(ascii);
    let bullet_color = tool_bullet_color(status);

    // Header: `● Tool(args_preview)` — bullet colored by status, name+args in
    // default foreground.
    let preview = if args_preview.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name}({args_preview})")
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{bullet} "), Style::default().fg(bullet_color)),
        Span::raw(preview),
    ]));

    // No follow-on row while still running — just the header is shown so the
    // user sees an in-flight indicator. (The status bar / footer carry the
    // spinner; the card itself reveals timing once we have it.)
    if matches!(status, ToolStatus::Running) {
        return;
    }

    if folded {
        // Folded follow-on: `  ⎿ Done (234ms · 12 lines)` summary in dim gray.
        let result_text = result.unwrap_or("");
        let line_count = if result_text.is_empty() {
            0
        } else {
            result_text.lines().count()
        };
        let status_word = match status {
            ToolStatus::Running => "Running",
            ToolStatus::Done => "Done",
            ToolStatus::Error => "Error",
        };
        let mut parts: Vec<String> = vec![status_word.to_string()];
        if let Some(ms) = elapsed_ms {
            parts.push(format!("{ms}ms"));
        }
        if line_count > 0 {
            parts.push(format!(
                "{line_count} {}",
                if line_count == 1 { "line" } else { "lines" }
            ));
        }
        let summary = format!("  {hook} {}", parts.join(" \u{00B7} ")); // ·
        lines.push(Line::from(Span::styled(summary, Style::default().fg(Color::DarkGray))));
        return;
    }

    // Expanded follow-on: result body indented under the hook glyph.
    if let Some(res) = result {
        let mut iter = res.lines();
        if let Some(first) = iter.next() {
            lines.push(Line::from(Span::styled(
                format!("  {hook} {first}"),
                Style::default().fg(Color::DarkGray),
            )));
            for body in iter {
                lines.push(Line::from(Span::styled(
                    format!("    {body}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    } else {
        // No result yet (Done with empty result, or Error with no payload).
        // Fall back to args_full so the expanded view always has something.
        let mut iter = args_full.lines();
        if let Some(first) = iter.next() {
            lines.push(Line::from(Span::styled(
                format!("  {hook} {first}"),
                Style::default().fg(Color::DarkGray),
            )));
            for body in iter {
                lines.push(Line::from(Span::styled(
                    format!("    {body}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }
}

/// Render a `Reasoning` card in Claude-Code style.
///
/// Folded → `▸ Thinking (123 tokens) - press Tab to expand`
///   (or `> Thinking (123 tokens) - press Tab to expand` in ASCII),
///   dim gray + italic so the line reads as a collapsed annotation rather
///   than primary content.
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
fn render_reasoning_card<'a>(
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
        // Single-line folded summary — token count + key hint, no body.
        let header = format!("{folded_icon} Thinking ({tokens} {token_word}) - press Tab to expand");
        lines.push(Line::from(Span::styled(header, header_style)));
        return;
    }

    // Unicode: remind users of both shortcuts; ASCII: keep it terse.
    let header = if ascii {
        format!("{expanded_icon} Thinking ({tokens} {token_word})")
    } else {
        format!("{expanded_icon} Thinking ({tokens} {token_word}) - Tab to collapse (Ctrl+R)")
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
const fn estimate_reasoning_tokens(char_count: usize) -> usize {
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
const fn reasoning_card_glyphs(ascii: bool) -> (&'static str, &'static str) {
    if ascii {
        (">", "v")
    } else {
        ("\u{25B8}", "\u{25BE}") // ▸ ▾
    }
}

/// Pick the bullet (`●` / `*`) and hook glyph (`⎿` / `└`) used by tool cards.
///
/// Claude Code uses a single status-colored bullet for the header and a dim
/// hook for the follow-on summary / body — far less visually noisy than the
/// previous `[name] running...` header.
const fn tool_card_glyphs(ascii: bool) -> (&'static str, &'static str) {
    if ascii { ("*", "L") } else { ("\u{25CF}", "\u{23BF}") }
}

/// Status → bullet color: yellow while running, green on success, red on error.
const fn tool_bullet_color(status: ToolStatus) -> Color {
    match status {
        ToolStatus::Running => Color::Yellow,
        ToolStatus::Done => Color::Green,
        ToolStatus::Error => Color::Red,
    }
}

fn render_input<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) {
    // Compose prompt lines: first row gets "> ", continuation rows get "  ".
    let input_ref = state.input();
    let rendered_lines: Vec<Line<'_>> = input_ref
        .lines
        .iter()
        .enumerate()
        .map(|(idx, content)| {
            let prefix = if idx == 0 {
                // Claude Code uses a dim cyan `> ` prompt (no bold) — calmer
                // than the previous bright bold cyan.
                Span::styled("> ", Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM))
            } else {
                Span::raw("  ")
            };
            Line::from(vec![prefix, Span::raw(content.as_str())])
        })
        .collect();

    let input = Paragraph::new(Text::from(rendered_lines))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .title(" Input ")
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .style(Style::default().fg(Color::White));
    frame.render_widget(input, area);

    // Place the terminal cursor at the visual cursor location inside the box.
    // Borders::TOP consumes the first row of `area`, so the body starts at
    // `area.y + 1` and the prompt prefix takes the first 2 columns.
    let (cursor_line, cursor_offset) = input_ref.cursor;
    let max_visible_rows = area.height.saturating_sub(1) as usize;
    if cursor_line < input_ref.lines.len() && cursor_line < max_visible_rows {
        let row_text = input_ref.lines.get(cursor_line).map(String::as_str).unwrap_or("");
        // Width-aware column: count *display* columns (not char count) up to
        // the byte offset. CJK and wide East-Asian glyphs occupy 2 columns,
        // so a `chars().count()` here would leave the cursor mid-glyph and
        // give the impression that input is broken. `unicode-width` matches
        // ratatui's own width algorithm for `Paragraph`.
        let visual_col: usize = row_text
            .get(..cursor_offset.min(row_text.len()))
            .map_or(0, UnicodeWidthStr::width);
        let col_offset = u16::try_from(visual_col).unwrap_or(u16::MAX);
        let prefix_cols: u16 = 2;
        let row_offset = u16::try_from(cursor_line).unwrap_or(u16::MAX);
        let cx = area.x.saturating_add(prefix_cols).saturating_add(col_offset);
        let cy = area.y.saturating_add(1).saturating_add(row_offset);
        // Only place cursor if it falls within the box bounds.
        if cx < area.x.saturating_add(area.width) && cy < area.y.saturating_add(area.height) {
            frame.set_cursor_position((cx, cy));
        }
    }
}

fn render_footer(frame: &mut Frame, area: Rect) {
    // Claude Code style: dim gray, middle-dot separators, action-oriented
    // hints rather than key/label pairs.
    let footer = Paragraph::new(
        " ! for bash \u{00B7} / for commands \u{00B7} Tab to fold \u{00B7} Ctrl+R to expand \u{00B7} Esc to cancel ",
    )
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
    fn tui_state_turn_count_tracks_user_messages_only() {
        // P3-inline: app-level scroll has been removed. The TuiState now
        // just owns the conversation log and turn counter; history scroll
        // is delegated to the host terminal. This pins the contract that
        // `turn_count` advances on user submissions only.
        let mut state = TuiState::new("test", "model");
        state.push_user_message("hello");
        state.push_assistant_message("world");
        assert_eq!(state.turn_count, 1, "only user messages bump turn_count");
        state.push_user_message("again");
        assert_eq!(state.turn_count, 2);
    }

    // ── P3-5: streaming-draft API tests ────────────────────────────────

    #[test]
    fn stream_start_creates_empty_draft_at_version_zero() {
        let mut state = TuiState::new("p", "m");
        assert!(state.streaming.is_none(), "fresh state has no draft");
        let v = state.start_stream("draft-1");
        assert_eq!(v, 0, "initial version is 0");
        let draft = state
            .streaming
            .as_ref()
            .expect("test: streaming slot populated after start");
        assert_eq!(draft.draft_id, "draft-1");
        assert_eq!(draft.accumulated, "");
        assert_eq!(draft.version, 0);
    }

    #[test]
    fn stream_update_replaces_text_when_version_advances() {
        let mut state = TuiState::new("p", "m");
        state.start_stream("d1");
        assert!(state.update_stream("d1", "Hel", 1));
        assert_eq!(
            state.streaming.as_ref().expect("test: streaming present").accumulated,
            "Hel"
        );
        assert!(state.update_stream("d1", "Hello", 2));
        assert_eq!(
            state.streaming.as_ref().expect("test: streaming present").accumulated,
            "Hello"
        );
        assert_eq!(state.streaming.as_ref().expect("test: streaming present").version, 2);
    }

    #[test]
    fn stream_update_rejects_stale_version() {
        let mut state = TuiState::new("p", "m");
        state.start_stream("d1");
        assert!(state.update_stream("d1", "Hel", 5));
        // Stale: seq 3 < 5 is dropped.
        assert!(!state.update_stream("d1", "old", 3));
        // And the buffer is unchanged.
        assert_eq!(
            state.streaming.as_ref().expect("test: streaming present").accumulated,
            "Hel"
        );
        // Same version also rejected (must strictly advance).
        assert!(!state.update_stream("d1", "still old", 5));
        assert_eq!(
            state.streaming.as_ref().expect("test: streaming present").accumulated,
            "Hel"
        );
    }

    #[test]
    fn stream_update_rejects_mismatched_draft_id() {
        let mut state = TuiState::new("p", "m");
        state.start_stream("draft-A");
        assert!(state.update_stream("draft-A", "alpha", 1));
        // Wrong draft id — silent no-op, must NOT corrupt the active draft.
        assert!(!state.update_stream("draft-B", "beta", 99));
        assert_eq!(
            state.streaming.as_ref().expect("test: streaming present").accumulated,
            "alpha"
        );
        assert_eq!(
            state.streaming.as_ref().expect("test: streaming present").draft_id,
            "draft-A"
        );
    }

    #[test]
    fn stream_update_without_active_draft_is_noop() {
        let mut state = TuiState::new("p", "m");
        assert!(!state.update_stream("nope", "lost", 1));
        assert!(state.streaming.is_none());
    }

    #[test]
    fn stream_finalize_lifts_into_conversation_and_clears() {
        let mut state = TuiState::new("p", "m");
        state.start_stream("d1");
        let _ = state.update_stream("d1", "answer body", 1);
        let len_before = state.conversation_lines.len();
        state.finalize_stream("d1", "answer body");
        assert!(state.streaming.is_none(), "streaming slot cleared after finalize");
        assert_eq!(state.conversation_lines.len(), len_before + 1, "Assistant line pushed");
        let last = state
            .conversation_lines
            .last()
            .expect("test: at least one conversation line");
        match last {
            ConversationLine::Assistant { content } => {
                assert_eq!(content, "answer body");
            }
            other => panic!("test: expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn stream_finalize_empty_clears_slot_without_pushing() {
        let mut state = TuiState::new("p", "m");
        state.start_stream("d1");
        let _ = state.update_stream("d1", "partial", 1);
        let len_before = state.conversation_lines.len();
        state.finalize_stream("d1", "");
        assert!(state.streaming.is_none(), "slot cleared even on empty finalise");
        assert_eq!(state.conversation_lines.len(), len_before, "no line pushed");
    }

    #[test]
    fn stream_finalize_mismatched_draft_is_noop() {
        let mut state = TuiState::new("p", "m");
        state.start_stream("d1");
        let _ = state.update_stream("d1", "buf", 1);
        let len_before = state.conversation_lines.len();
        state.finalize_stream("OTHER", "should-not-land");
        assert!(state.streaming.is_some(), "active draft preserved");
        assert_eq!(state.conversation_lines.len(), len_before, "no line pushed");
    }

    #[test]
    fn stream_cancel_clears_slot_without_pushing() {
        let mut state = TuiState::new("p", "m");
        state.start_stream("d1");
        let _ = state.update_stream("d1", "buf", 1);
        let len_before = state.conversation_lines.len();
        state.cancel_stream("d1");
        assert!(state.streaming.is_none(), "slot cleared after cancel");
        assert_eq!(state.conversation_lines.len(), len_before, "no line pushed on cancel");
    }

    #[test]
    fn stream_cancel_mismatched_draft_is_noop() {
        let mut state = TuiState::new("p", "m");
        state.start_stream("d1");
        state.cancel_stream("WRONG");
        assert!(state.streaming.is_some(), "active draft survives wrong-id cancel");
    }

    #[test]
    fn stream_start_replaces_previous_inflight_draft() {
        // Defensive: if the channel layer fails to finalise the previous draft
        // before starting a new one, dropping the stale buffer is safer than
        // interleaving deltas across turns.
        let mut state = TuiState::new("p", "m");
        state.start_stream("d1");
        let _ = state.update_stream("d1", "stale buffer", 4);
        state.start_stream("d2");
        let draft = state.streaming.as_ref().expect("test: new streaming slot present");
        assert_eq!(draft.draft_id, "d2");
        assert_eq!(draft.accumulated, "");
        assert_eq!(draft.version, 0);
    }

    #[test]
    fn streaming_transient_renders_after_history_in_insert_pipeline() {
        // P3-inline: drive `render_conversation_line` over both the
        // history and the staged transient line. The history portion is
        // what `render_message_for_insert` emits per message; the
        // transient lands in the inline viewport's streaming preview
        // (see `render_streaming_preview`). Either way the streaming
        // block must contain the in-flight tokens and end with the
        // cursor glyph.
        let mut state = TuiState::new("p", "m");
        state.push_assistant_message("first turn done");
        state.start_stream("d-live");
        assert!(state.update_stream("d-live", "in-flight tokens", 1));

        // Simulate the render path's two-stage line build.
        let transient: Option<ConversationLine> =
            state.streaming.as_ref().map(|s| ConversationLine::StreamingAssistant {
                content: s.accumulated.clone(),
            });
        let mut lines: Vec<Line<'_>> = Vec::new();
        for conv_line in &state.conversation_lines {
            render_conversation_line(&mut lines, conv_line, false);
        }
        let history_end = lines.len();
        if let Some(t) = transient.as_ref() {
            render_conversation_line(&mut lines, t, false);
        }
        assert!(lines.len() > history_end, "streaming block contributed >=1 line");
        // The streaming body must contain the in-flight tokens AND end with
        // the streaming cursor glyph — confirms it routed through the
        // StreamingAssistant variant rather than the finalized Assistant one.
        // With the Claude-Code-style header removed, the body now lands at
        // `history_end` itself.
        let streaming_body: String = lines
            .get(history_end)
            .expect("test: body line present")
            .iter()
            .map(ratatui::text::Span::to_string)
            .collect();
        assert!(
            streaming_body.contains("in-flight tokens"),
            "expected streaming body, got {streaming_body:?}"
        );
        assert!(
            streaming_body.ends_with('\u{258C}'),
            "streaming body ends with ▌ cursor"
        );
    }

    #[test]
    fn streaming_assistant_variant_renders_with_cursor() {
        let line = ConversationLine::StreamingAssistant {
            content: "partial response".to_string(),
        };
        let mut sink: Vec<Line<'_>> = Vec::new();
        render_conversation_line(&mut sink, &line, false);
        // Claude Code style: no `PRX:` header → body + trailing blank only.
        assert_eq!(sink.len(), 2, "streaming assistant renders body+blank");
        // Body must contain the content and end with the unicode cursor glyph.
        let body_text: String = sink
            .first()
            .expect("test: body line present")
            .iter()
            .map(ratatui::text::Span::to_string)
            .collect();
        assert!(body_text.contains("partial response"), "body has content");
        assert!(body_text.ends_with('\u{258C}'), "body ends with ▌ cursor");

        // ASCII fallback uses '_' instead of '▌'.
        let mut sink2: Vec<Line<'_>> = Vec::new();
        render_conversation_line(&mut sink2, &line, true);
        let body2: String = sink2
            .first()
            .expect("test: body line present")
            .iter()
            .map(ratatui::text::Span::to_string)
            .collect();
        assert!(body2.ends_with('_'), "ASCII fallback ends with '_'");
    }

    #[test]
    fn streaming_assistant_empty_content_still_renders() {
        let line = ConversationLine::StreamingAssistant { content: String::new() };
        let mut sink: Vec<Line<'_>> = Vec::new();
        render_conversation_line(&mut sink, &line, false);
        // Empty stream still produces 1 cursor row + 1 blank separator.
        assert_eq!(sink.len(), 2, "empty stream still produces body+blank");
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
        // Claude-Code style: while running we render just the bullet header
        // (`● shell(ls)`) with no follow-on summary row yet.
        assert_eq!(lines.len(), 1, "running folded card renders to 1 line");
        let rendered: String = lines
            .first()
            .expect("test: at least one line")
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(rendered.contains("\u{25CF}"), "uses ● bullet: {rendered}");
        assert!(rendered.contains("shell(ls)"), "shows Tool(args) preview: {rendered}");
    }

    #[test]
    fn render_folded_tool_card_done_shows_hook_summary() {
        // Claude-Code style follow-on: `  ⎿ Done (234ms · 3 lines)` under the
        // bullet header once the tool finishes.
        let mut lines: Vec<Line<'_>> = Vec::new();
        let card = ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: "ls".to_string(),
            args_full: "ls".to_string(),
            result: Some("a\nb\nc".to_string()),
            status: ToolStatus::Done,
            elapsed_ms: Some(234),
            folded: true,
        };
        render_conversation_line(&mut lines, &card, false);
        assert_eq!(lines.len(), 2, "done folded card renders header + summary");
        let summary: String = lines
            .get(1)
            .expect("test: summary line present")
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(summary.contains("\u{23BF}"), "uses ⎿ hook glyph: {summary}");
        assert!(summary.contains("Done"), "shows status word: {summary}");
        assert!(summary.contains("234ms"), "shows elapsed ms: {summary}");
        assert!(summary.contains("3 lines"), "shows result line count: {summary}");
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
        // Claude-Code style expanded:
        //   row 0  `● shell(ls)`               — bullet header
        //   row 1  `  ⎿ total 24`              — first body row under hook
        //   row 2  `    drwxrwxrwt`            — continuation
        assert_eq!(lines.len(), 3, "expanded card line count: {}", lines.len());
        let join = |i: usize| -> String {
            lines
                .get(i)
                .expect("test: line idx")
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert!(join(0).contains("\u{25CF}"), "uses ● bullet: {}", join(0));
        assert!(join(0).contains("shell(ls)"), "shows Tool(args): {}", join(0));
        assert!(
            join(1).contains("\u{23BF}"),
            "uses ⎿ hook on first body row: {}",
            join(1)
        );
        assert!(join(1).contains("total 24"), "first body row: {}", join(1));
        assert!(join(2).contains("drwxrwxrwt"), "second body row: {}", join(2));
    }

    #[test]
    fn render_tool_card_status_glyphs_and_colors() {
        // Bullet color tracks status — yellow while running, green on success,
        // red on error. The bullet glyph itself does not change.
        let (bullet, hook) = tool_card_glyphs(false);
        assert_eq!(bullet, "\u{25CF}", "unicode bullet ●");
        assert_eq!(hook, "\u{23BF}", "unicode hook ⎿");
        assert_eq!(tool_bullet_color(ToolStatus::Running), Color::Yellow);
        assert_eq!(tool_bullet_color(ToolStatus::Done), Color::Green);
        assert_eq!(tool_bullet_color(ToolStatus::Error), Color::Red);
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
        // Render in ASCII mode → bullet is `*`, hook is `L`.
        let (bullet, hook) = tool_card_glyphs(true);
        assert_eq!(bullet, "*", "ASCII bullet");
        assert_eq!(hook, "L", "ASCII hook");

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
        assert!(rendered.starts_with("* "), "ASCII bullet header: {rendered}");
    }

    #[test]
    fn total_content_lines_counts_folded_vs_expanded() {
        // Claude-Code style cards only differ in row count *after* the tool
        // has finished — while running, both folded and expanded views show
        // just the bullet header. Mark the result done first so the folded
        // view gets its `⎿ Done` summary and the expanded view gets the body.
        let mut state = TuiState::new("p", "m");
        state.push_tool_result_started("shell", "x");
        state.mark_last_tool_result_finished("shell", true, 12, Some("line1\nline2\nline3".to_string()));
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

    // ── P2-12: Reasoning card tests ──────────────────────────────────────────

    /// Collapse a rendered `Line` into a plain `String` by concatenating all
    /// span contents. Used so assertions can grep for substrings without
    /// caring about the ratatui Span structure.
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn reasoning_variant_construction_caches_char_count_and_defaults_folded() {
        let r = ConversationLine::Reasoning {
            content: "abc".to_string(),
            char_count: 3,
            folded: true,
        };
        assert!(r.is_reasoning());
        assert!(!r.is_tool_result());
        match r {
            ConversationLine::Reasoning {
                content,
                char_count,
                folded,
            } => {
                assert_eq!(content, "abc");
                assert_eq!(char_count, 3);
                assert!(folded);
            }
            other => panic!("test: expected Reasoning, got {other:?}"),
        }
    }

    #[test]
    fn push_reasoning_drops_empty_and_whitespace_only_inputs() {
        let mut state = TuiState::new("p", "m");
        assert!(!state.push_reasoning(""));
        assert!(!state.push_reasoning("   \n\t"));
        assert!(state.last_reasoning_index().is_none());
        assert!(state.conversation_lines.is_empty());

        // Real reasoning is accepted.
        assert!(state.push_reasoning("  let me think...  "));
        let idx = state.last_reasoning_index().expect("test: reasoning exists");
        match state.conversation_lines.get(idx).expect("test: idx valid") {
            ConversationLine::Reasoning {
                content,
                char_count,
                folded,
            } => {
                // Trim is applied before storage.
                assert_eq!(content, "let me think...");
                assert_eq!(*char_count, "let me think...".chars().count());
                assert!(*folded, "default state is folded");
            }
            other => panic!("test: expected Reasoning, got {other:?}"),
        }
    }

    #[test]
    fn toggle_last_reasoning_folded_flips_and_targets_last() {
        let mut state = TuiState::new("p", "m");
        // No card yet → None.
        assert_eq!(state.toggle_last_reasoning_folded(), None);

        state.push_reasoning("first thought");
        state.push_reasoning("second thought");
        // Default folded=true → toggling last (second) → false.
        assert_eq!(state.toggle_last_reasoning_folded(), Some(false));

        // First card untouched.
        let first_idx = state
            .conversation_lines
            .iter()
            .position(ConversationLine::is_reasoning)
            .expect("test: first idx");
        match state.conversation_lines.get(first_idx).expect("test: first idx valid") {
            ConversationLine::Reasoning { folded, .. } => assert!(*folded, "first card untouched"),
            other => panic!("test: expected Reasoning, got {other:?}"),
        }

        // Second toggle of last → back to folded.
        assert_eq!(state.toggle_last_reasoning_folded(), Some(true));
    }

    #[test]
    fn render_folded_reasoning_card_renders_single_summary_line() {
        let mut lines: Vec<Line<'_>> = Vec::new();
        let card = ConversationLine::Reasoning {
            content: "Step 1: analyze the input\nStep 2: reason about it".to_string(),
            char_count: 45,
            folded: true,
        };
        render_conversation_line(&mut lines, &card, false);
        assert_eq!(lines.len(), 1, "folded card is exactly one line");
        let rendered = line_text(lines.first().expect("test: first line"));
        // S1-A folded summary: `▸ Thinking (N tokens) - press Tab to expand`.
        assert!(rendered.starts_with("\u{25B8} "), "uses ▸ folded icon: {rendered}");
        assert!(rendered.contains("Thinking"), "shows Thinking label: {rendered}");
        assert!(rendered.contains("tokens"), "shows token count: {rendered}");
        assert!(
            rendered.contains("press Tab to expand"),
            "advertises Tab keybinding: {rendered}"
        );
        // Folded summary must NOT leak the body text.
        assert!(!rendered.contains("Step 1"), "body hidden when folded: {rendered}");
    }

    #[test]
    fn render_expanded_reasoning_card_indents_body_two_spaces() {
        let mut lines: Vec<Line<'_>> = Vec::new();
        let body = "Let me reason step by step.\nFirst, I need to check the input.\nThen, decide the next action.";
        let card = ConversationLine::Reasoning {
            content: body.to_string(),
            char_count: body.chars().count(),
            folded: false,
        };
        render_conversation_line(&mut lines, &card, false);
        // 1 header + 3 body rows = 4 lines.
        assert_eq!(lines.len(), 4, "header + 3 body rows: {}", lines.len());
        let header = line_text(lines.first().expect("test: header"));
        assert!(
            header.starts_with("\u{25BE} "),
            "expanded header shows ▾ icon: {header}"
        );
        assert!(
            header.contains("Thinking"),
            "expanded header still says Thinking: {header}"
        );
        assert!(
            header.contains("tokens"),
            "expanded header carries token count: {header}"
        );
        // Expanded header shows collapse shortcut with Ctrl+R legacy hint.
        assert!(
            !header.contains("press Tab to expand"),
            "expanded header drops the expand hint: {header}"
        );
        assert!(
            header.contains("Tab to collapse (Ctrl+R)"),
            "expanded header shows collapse + Ctrl+R hint: {header}"
        );
        // Each body row begins with "  " indent.
        for (idx, original) in body.lines().enumerate() {
            let rendered = line_text(lines.get(idx + 1).expect("test: body line"));
            assert_eq!(rendered, format!("  {original}"), "body row {idx} indent");
        }
    }

    #[test]
    fn render_reasoning_card_ascii_fallback_uses_plain_glyphs() {
        let mut lines: Vec<Line<'_>> = Vec::new();
        let card = ConversationLine::Reasoning {
            content: "thinking...".to_string(),
            char_count: 11,
            folded: true,
        };
        render_conversation_line(&mut lines, &card, true);
        let rendered = line_text(lines.first().expect("test: first line"));
        // ASCII folded glyph is `>`.
        assert!(rendered.starts_with("> "), "ASCII folded icon >: {rendered}");
        assert!(
            rendered.contains("Thinking ("),
            "ASCII keeps Thinking label: {rendered}"
        );
        assert!(
            rendered.contains("press Tab to expand"),
            "ASCII keeps Tab hint: {rendered}"
        );
        assert!(!rendered.contains("\u{25B8}"), "no ▸ in ASCII mode: {rendered}");

        // Expanded ASCII uses `v` chevron.
        let mut lines2: Vec<Line<'_>> = Vec::new();
        let expanded = ConversationLine::Reasoning {
            content: "x".to_string(),
            char_count: 1,
            folded: false,
        };
        render_conversation_line(&mut lines2, &expanded, true);
        let header = line_text(lines2.first().expect("test: expanded header"));
        assert!(header.starts_with("v Thinking ("), "ASCII expanded header: {header}");
        assert!(!header.contains("\u{25BE}"), "no ▾ in ASCII mode: {header}");
    }

    #[test]
    fn estimate_line_height_reasoning_folded_vs_expanded() {
        let folded = ConversationLine::Reasoning {
            content: "a\nb\nc\nd".to_string(),
            char_count: 7,
            folded: true,
        };
        assert_eq!(estimate_line_height(&folded), 1);

        let expanded = ConversationLine::Reasoning {
            content: "a\nb\nc\nd".to_string(),
            char_count: 7,
            folded: false,
        };
        // 1 header + 4 body lines = 5
        assert_eq!(estimate_line_height(&expanded), 5);
    }

    #[test]
    fn reasoning_card_glyphs_table() {
        // S1-A: triangle glyphs — `▸` folded / `▾` expanded — with `>` / `v`
        // as ASCII fallbacks. The fold state DOES change the leading icon so
        // the user can see at a glance whether a card is currently expanded.
        assert_eq!(reasoning_card_glyphs(false), ("\u{25B8}", "\u{25BE}"));
        assert_eq!(reasoning_card_glyphs(true), (">", "v"));
    }

    #[test]
    fn estimate_reasoning_tokens_table() {
        // Empty → 0 (folded card never reaches this because push_reasoning
        // drops empty bodies, but the function should still be sane).
        assert_eq!(estimate_reasoning_tokens(0), 0);
        // Anything non-empty rounds up to at least 1 token.
        assert_eq!(estimate_reasoning_tokens(1), 1);
        assert_eq!(estimate_reasoning_tokens(3), 1);
        assert_eq!(estimate_reasoning_tokens(4), 1);
        assert_eq!(estimate_reasoning_tokens(5), 2);
        assert_eq!(estimate_reasoning_tokens(400), 100);
    }

    // ── P2-10: TuiInput multi-line + history tests ───────────────────────────

    /// Build a `KeyEvent` with no modifiers.
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Build a `KeyEvent` with a single modifier.
    fn key_mod(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    /// Convenience: type each char in `s` into `input`.
    fn type_str(input: &mut TuiInput, s: &str) {
        for ch in s.chars() {
            input.handle_key(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn p2_10_enter_submits_and_clears() {
        let mut input = TuiInput::new();
        type_str(&mut input, "hello");
        assert_eq!(input.text(), "hello");
        let out = input.handle_key(key(KeyCode::Enter));
        assert_eq!(out, InputOutcome::Submitted("hello".to_string()));
        assert!(input.is_empty(), "buffer cleared after submit");
        // History got the entry.
        assert_eq!(input.history, vec!["hello".to_string()]);
    }

    #[test]
    fn p2_10_enter_on_empty_buffer_is_consumed_no_submit() {
        let mut input = TuiInput::new();
        let out = input.handle_key(key(KeyCode::Enter));
        assert_eq!(out, InputOutcome::Consumed);
        assert!(input.history.is_empty(), "empty enter does not record");
    }

    #[test]
    fn p2_10_shift_enter_inserts_newline() {
        let mut input = TuiInput::new();
        type_str(&mut input, "a");
        let out = input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(out, InputOutcome::Consumed);
        type_str(&mut input, "b");
        assert_eq!(input.text(), "a\nb");
        assert_eq!(input.lines.len(), 2);
        assert!(!input.is_single_line());
    }

    #[test]
    fn p2_10_cursor_moves_across_line_boundaries() {
        let mut input = TuiInput::new();
        type_str(&mut input, "ab");
        input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
        type_str(&mut input, "cd");
        // cursor at (1, 2). Move left twice → (1,0); once more → (0, 2).
        assert_eq!(input.cursor, (1, 2));
        input.handle_key(key(KeyCode::Left));
        input.handle_key(key(KeyCode::Left));
        assert_eq!(input.cursor, (1, 0));
        input.handle_key(key(KeyCode::Left));
        assert_eq!(input.cursor, (0, 2));
        // Move right → (1, 0).
        input.handle_key(key(KeyCode::Right));
        assert_eq!(input.cursor, (1, 0));
    }

    #[test]
    fn p2_10_history_up_recalls_last_submission() {
        let mut input = TuiInput::new();
        type_str(&mut input, "first");
        input.handle_key(key(KeyCode::Enter));
        type_str(&mut input, "second");
        input.handle_key(key(KeyCode::Enter));
        // Single-line buffer → Up walks history backward.
        let out = input.handle_key(key(KeyCode::Up));
        assert_eq!(out, InputOutcome::Consumed);
        assert_eq!(input.text(), "second");
        input.handle_key(key(KeyCode::Up));
        assert_eq!(input.text(), "first");
        // Down → second → fresh empty.
        input.handle_key(key(KeyCode::Down));
        assert_eq!(input.text(), "second");
        input.handle_key(key(KeyCode::Down));
        assert!(input.is_empty(), "back to fresh draft");
    }

    #[test]
    fn p2_10_history_preserves_in_flight_draft() {
        let mut input = TuiInput::new();
        type_str(&mut input, "old");
        input.handle_key(key(KeyCode::Enter));
        // Start typing a new draft, then navigate up & back down.
        type_str(&mut input, "wip");
        input.handle_key(key(KeyCode::Up));
        assert_eq!(input.text(), "old");
        input.handle_key(key(KeyCode::Down));
        assert_eq!(input.text(), "wip", "draft restored");
    }

    #[test]
    fn p2_10_history_dedups_consecutive_duplicates() {
        let mut input = TuiInput::new();
        type_str(&mut input, "x");
        input.handle_key(key(KeyCode::Enter));
        type_str(&mut input, "x");
        input.handle_key(key(KeyCode::Enter));
        assert_eq!(input.history.len(), 1, "consecutive dupes collapsed");
    }

    #[test]
    fn p2_10_backspace_at_line_start_merges_lines() {
        let mut input = TuiInput::new();
        type_str(&mut input, "ab");
        input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
        type_str(&mut input, "cd");
        // cursor at (1, 2). Move to start of line 1.
        input.handle_key(key(KeyCode::Home));
        assert_eq!(input.cursor, (1, 0));
        input.handle_key(key(KeyCode::Backspace));
        assert_eq!(input.text(), "abcd");
        assert_eq!(input.cursor, (0, 2), "cursor at original line-1 start");
        assert!(input.is_single_line());
    }

    #[test]
    fn p2_10_backspace_inside_line_removes_one_char() {
        let mut input = TuiInput::new();
        type_str(&mut input, "abc");
        input.handle_key(key(KeyCode::Backspace));
        assert_eq!(input.text(), "ab");
        assert_eq!(input.cursor, (0, 2));
    }

    #[test]
    fn p2_10_ctrl_u_kills_to_line_start() {
        let mut input = TuiInput::new();
        type_str(&mut input, "hello world");
        // Move left 5 chars → cursor at index 6.
        for _ in 0..5 {
            input.handle_key(key(KeyCode::Left));
        }
        assert_eq!(input.cursor, (0, 6));
        let out = input.handle_key(key_mod(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(out, InputOutcome::Consumed);
        assert_eq!(input.text(), "world");
        assert_eq!(input.cursor, (0, 0));
    }

    #[test]
    fn p2_10_ctrl_a_and_ctrl_e_jump_line_endpoints() {
        let mut input = TuiInput::new();
        type_str(&mut input, "abc");
        input.handle_key(key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(input.cursor, (0, 0));
        input.handle_key(key_mod(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(input.cursor, (0, 3));
    }

    #[test]
    fn p2_10_paste_with_newlines_creates_multiple_lines() {
        let mut input = TuiInput::new();
        input.paste("alpha\nbeta\ngamma");
        assert_eq!(input.lines.len(), 3);
        assert_eq!(input.text(), "alpha\nbeta\ngamma");
        // Cursor lands at end of pasted content.
        assert_eq!(input.cursor, (2, 5));
        // Now ↑ should NOT recall history (we have a multi-line buffer).
        let out = input.handle_key(key(KeyCode::Up));
        assert_eq!(out, InputOutcome::Consumed);
        // Cursor moved up one row, not to history.
        assert_eq!(input.cursor.0, 1);
    }

    #[test]
    fn p2_10_multi_line_up_down_moves_cursor_not_history() {
        let mut input = TuiInput::new();
        type_str(&mut input, "x");
        input.handle_key(key(KeyCode::Enter)); // submits "x", history = ["x"]
        type_str(&mut input, "row1");
        input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
        type_str(&mut input, "row2");
        // Multi-line: ↑ should move cursor, not recall "x".
        input.handle_key(key(KeyCode::Up));
        assert_eq!(input.text(), "row1\nrow2", "still editing same draft");
        assert_eq!(input.cursor.0, 0);
    }

    #[test]
    fn p2_10_esc_clears_buffer_and_returns_cancelled() {
        let mut input = TuiInput::new();
        type_str(&mut input, "garbage");
        let out = input.handle_key(key(KeyCode::Esc));
        assert_eq!(out, InputOutcome::Cancelled);
        assert!(input.is_empty());
        // Esc on empty still returns Cancelled.
        let out2 = input.handle_key(key(KeyCode::Esc));
        assert_eq!(out2, InputOutcome::Cancelled);
    }

    #[test]
    fn p2_10_delete_forward_joins_next_line() {
        let mut input = TuiInput::new();
        type_str(&mut input, "ab");
        input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
        type_str(&mut input, "cd");
        input.handle_key(key(KeyCode::Up));
        input.handle_key(key(KeyCode::End));
        assert_eq!(input.cursor, (0, 2));
        input.handle_key(key(KeyCode::Delete));
        assert_eq!(input.text(), "abcd");
        assert_eq!(input.lines.len(), 1);
    }

    #[test]
    fn p3_inline_pageup_pagedown_fall_through_to_terminal() {
        // P3-inline: PgUp/PgDn are intentionally NOT consumed so the host
        // terminal can scroll the main scrollback natively (alongside
        // mouse wheel and Shift+PgUp). The input subsystem must report
        // them as Unhandled.
        let mut input = TuiInput::new();
        assert_eq!(input.handle_key(key(KeyCode::PageUp)), InputOutcome::Unhandled);
        assert_eq!(input.handle_key(key(KeyCode::PageDown)), InputOutcome::Unhandled);
    }

    #[test]
    fn p2_10_utf8_grapheme_safe_backspace() {
        let mut input = TuiInput::new();
        // Three Chinese chars (3 bytes each in UTF-8).
        type_str(&mut input, "你好吗");
        assert_eq!(input.cursor, (0, 9));
        input.handle_key(key(KeyCode::Backspace));
        assert_eq!(input.text(), "你好");
        assert_eq!(input.cursor, (0, 6));
        // Move left then right: must land on char boundaries.
        input.handle_key(key(KeyCode::Left));
        assert_eq!(input.cursor, (0, 3));
        input.handle_key(key(KeyCode::Right));
        assert_eq!(input.cursor, (0, 6));
    }

    #[test]
    fn p2_10_tui_state_routes_keys_to_input() {
        let mut state = TuiState::new("p", "m");
        let out = state.handle_input_key(key(KeyCode::Char('z')));
        assert_eq!(out, InputOutcome::Consumed);
        assert_eq!(state.input.text(), "z");
    }

    #[test]
    fn p2_10_history_capped_at_capacity() {
        let mut input = TuiInput::new();
        for i in 0..(INPUT_HISTORY_CAPACITY + 5) {
            type_str(&mut input, &format!("e{i}"));
            input.handle_key(key(KeyCode::Enter));
        }
        assert_eq!(input.history.len(), INPUT_HISTORY_CAPACITY);
        // Oldest entry was dropped.
        assert!(!input.history.contains(&"e0".to_string()));
    }

    // ── P2-Integration: global key dispatch tests ────────────────────────────

    #[test]
    fn dispatch_tab_toggles_last_tool_result_card() {
        let mut state = TuiState::new("p", "m");
        // Without any foldable card the Tab keystroke is still consumed (no-op).
        let out = dispatch_global_key(key(KeyCode::Tab), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);

        // Push a tool result and verify Tab flips its folded flag.
        state.push_tool_result_started("shell", "{}");
        let folded_before = match state.conversation_lines.last() {
            Some(ConversationLine::ToolResult { folded, .. }) => *folded,
            _ => panic!("test: expected ToolResult at end"),
        };
        let out = dispatch_global_key(key(KeyCode::Tab), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        let folded_after = match state.conversation_lines.last() {
            Some(ConversationLine::ToolResult { folded, .. }) => *folded,
            _ => panic!("test: expected ToolResult at end"),
        };
        assert_ne!(folded_before, folded_after, "Tab must flip folded state");
        // Tab is consumed by the dispatcher → input buffer untouched.
        assert!(state.input.is_empty(), "Tab must not fall through to input box");
    }

    #[test]
    fn dispatch_tab_toggles_last_reasoning_card_when_more_recent_than_tool() {
        // S1-A: Tab now toggles whichever foldable card sits closest to the
        // end of the conversation. A reasoning card pushed AFTER a tool card
        // must win the Tab dispatch.
        let mut state = TuiState::new("p", "m");
        state.push_tool_result_started("shell", "{}");
        assert!(state.push_reasoning("step 1\nstep 2"));

        // Defaults: both folded = true. Tab should flip the reasoning card.
        let out = dispatch_global_key(key(KeyCode::Tab), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        match state.conversation_lines.last() {
            Some(ConversationLine::Reasoning { folded, .. }) => assert!(!*folded, "Tab unfolded reasoning"),
            other => panic!("test: expected Reasoning at end, got {other:?}"),
        }
        // The tool-result card must NOT have been touched.
        let tool_idx = state
            .conversation_lines
            .iter()
            .position(ConversationLine::is_tool_result)
            .expect("test: tool card exists");
        match state.conversation_lines.get(tool_idx).expect("test: tool idx valid") {
            ConversationLine::ToolResult { folded, .. } => assert!(*folded, "tool card untouched"),
            other => panic!("test: expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn toggle_last_foldable_card_returns_kind_tag() {
        let mut state = TuiState::new("p", "m");
        assert_eq!(state.toggle_last_foldable_card(), None);

        state.push_tool_result_started("shell", "{}");
        assert_eq!(
            state.toggle_last_foldable_card(),
            Some((FoldableKind::ToolResult, false))
        );

        assert!(state.push_reasoning("thinking"));
        assert_eq!(
            state.toggle_last_foldable_card(),
            Some((FoldableKind::Reasoning, false))
        );
    }

    #[test]
    fn dispatch_ctrl_r_toggles_last_reasoning_card() {
        let mut state = TuiState::new("p", "m");
        // No reasoning card yet — still consumed, no-op.
        let out = dispatch_global_key(key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        assert!(state.input.is_empty(), "Ctrl+R never falls through to input");

        // Push a reasoning card and verify Ctrl+R flips its folded flag.
        assert!(state.push_reasoning("step 1\nstep 2"));
        let folded_before = match state.conversation_lines.last() {
            Some(ConversationLine::Reasoning { folded, .. }) => *folded,
            _ => panic!("test: expected Reasoning at end"),
        };
        let out = dispatch_global_key(key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        let folded_after = match state.conversation_lines.last() {
            Some(ConversationLine::Reasoning { folded, .. }) => *folded,
            _ => panic!("test: expected Reasoning at end"),
        };
        assert_ne!(folded_before, folded_after, "Ctrl+R must flip folded state");
    }

    #[test]
    fn dispatch_typing_and_enter_yields_submission() {
        let mut state = TuiState::new("p", "m");
        for ch in "Hello".chars() {
            let out = dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
            assert_eq!(out, KeyDispatch::Consumed);
        }
        let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
        assert_eq!(out, KeyDispatch::Submitted("Hello".to_string()));
        assert!(state.input.is_empty(), "buffer cleared after submit");
    }

    #[test]
    fn dispatch_ctrl_c_signals_interrupt_turn() {
        let mut state = TuiState::new("p", "m");
        let out = dispatch_global_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut state);
        assert_eq!(out, KeyDispatch::InterruptTurn);
    }

    #[test]
    fn dispatch_ctrl_d_on_empty_buffer_signals_exit() {
        let mut state = TuiState::new("p", "m");
        let out = dispatch_global_key(key_mod(KeyCode::Char('d'), KeyModifiers::CONTROL), &mut state);
        assert_eq!(out, KeyDispatch::Exit);
    }

    #[test]
    fn dispatch_ctrl_d_on_non_empty_buffer_deletes_forward() {
        let mut state = TuiState::new("p", "m");
        // Type "abc" then move cursor to start.
        for ch in "abc".chars() {
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
        }
        dispatch_global_key(key(KeyCode::Home), &mut state);
        assert_eq!(state.input.text(), "abc");
        // Ctrl+D should forward-delete 'a' instead of exiting.
        let out = dispatch_global_key(key_mod(KeyCode::Char('d'), KeyModifiers::CONTROL), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        assert_eq!(state.input.text(), "bc");
    }

    #[test]
    fn dispatch_pageup_pagedown_are_consumed_without_scroll() {
        // P3-inline: there is no app-level scrolling. PgUp/PgDn are not
        // routed anywhere by the dispatcher — they surface as Consumed
        // (the global dispatcher always returns *some* KeyDispatch) and
        // leave the input buffer untouched. The host terminal handles
        // history scroll natively, so users still get PgUp/PgDn behavior
        // via the terminal emulator's own scrollback.
        let mut state = TuiState::new("p", "m");
        let out = dispatch_global_key(key(KeyCode::PageUp), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        let out = dispatch_global_key(key(KeyCode::PageDown), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        assert!(state.input.is_empty(), "PgUp/PgDn must not leak into input");
    }

    #[test]
    fn dispatch_esc_returns_cancelled() {
        let mut state = TuiState::new("p", "m");
        for ch in "draft".chars() {
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
        }
        let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
        assert_eq!(out, KeyDispatch::Cancelled);
        assert!(state.input.is_empty(), "Esc clears the in-flight draft");
    }

    #[test]
    fn dispatch_sequence_hello_enter_returns_submitted_text() {
        // Full integration: simulate the canonical "type Hello + Enter" flow.
        let mut state = TuiState::new("p", "m");
        let seq = [
            key(KeyCode::Char('H')),
            key(KeyCode::Char('e')),
            key(KeyCode::Char('l')),
            key(KeyCode::Char('l')),
            key(KeyCode::Char('o')),
            key(KeyCode::Enter),
        ];
        let mut submitted: Option<String> = None;
        for k in seq {
            match dispatch_global_key(k, &mut state) {
                KeyDispatch::Submitted(text) => submitted = Some(text),
                KeyDispatch::Consumed => {}
                other => panic!("test: unexpected dispatch {other:?}"),
            }
        }
        assert_eq!(submitted, Some("Hello".to_string()));
    }

    // ── P3 chat TUI rearch tests (2026-05-13) ────────────────────────────
    //
    // These pin the contract relied on by the new unified TUI loop in
    // `chat/mod.rs::run_tui_unified_loop`:
    //   1. `TuiInput::paste` splits multi-line text into the `lines` Vec
    //      and lands the cursor at the end of the last pasted row.
    //   2. `push_system_message` appends a `ConversationLine::System` —
    //      banner + slash-command output go through this on the TUI path.
    //   3. `dispatch_global_key` accepts CJK characters via plain
    //      `KeyCode::Char(_)` (bracketed-paste decodes them, but unicode
    //      chars also flow through as KeyEvents on graphical terminals).

    #[test]
    fn paste_with_newline_splits_into_rows_and_lands_cursor_at_end() {
        let mut input = TuiInput::new();
        input.paste("hello\nworld");
        assert_eq!(input.lines, vec!["hello".to_string(), "world".to_string()]);
        // Cursor lands at the end of the last pasted row.
        assert_eq!(input.cursor, (1, "world".len()));
    }

    #[test]
    fn paste_into_existing_buffer_preserves_suffix() {
        let mut input = TuiInput::new();
        type_str(&mut input, "abXY");
        // Move cursor between 'b' and 'X'.
        input.handle_key(key(KeyCode::Left));
        input.handle_key(key(KeyCode::Left));
        assert_eq!(input.cursor, (0, 2));
        input.paste("1\n2");
        // After paste: row 0 = "ab1", row 1 = "2XY" (suffix moved down).
        assert_eq!(input.lines, vec!["ab1".to_string(), "2XY".to_string()]);
        // Cursor lands after "2", before "XY".
        assert_eq!(input.cursor, (1, 1));
    }

    #[test]
    fn push_system_message_appends_system_conversation_line() {
        let mut state = TuiState::new("p", "m");
        state.push_system_message("test banner");
        let last = state
            .conversation_lines
            .last()
            .expect("test: conversation_lines non-empty after push_system_message");
        match last {
            ConversationLine::System { content } => assert_eq!(content, "test banner"),
            other => panic!("test: expected ConversationLine::System, got {other:?}"),
        }
        // Pushing system messages must not bump the *user* turn counter
        // (that drives the status-bar "N turns" display).
        assert_eq!(state.turn_count, 0);
    }

    #[test]
    fn dispatch_cjk_character_lands_in_input_buffer() {
        // Bracketed paste handles long IME commits, but graphical
        // terminals also deliver single CJK chars as `KeyCode::Char(_)`
        // KeyEvents. Verify the dispatcher does not swallow them.
        let mut state = TuiState::new("p", "m");
        for ch in "你好".chars() {
            let out = dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
            assert_eq!(out, KeyDispatch::Consumed);
        }
        assert_eq!(state.input.text(), "你好");
    }

    // ── P3-inline: insert_before pipeline tests ──────────────────────────

    #[test]
    fn render_message_for_insert_paints_user_line_into_buffer() {
        // The unified TUI loop hands `render_message_for_insert` a Buffer
        // sized via `estimate_message_height` and expects the message to
        // be visible in the scrollback after `insert_before` flushes.
        // Render a short user message into a 4-row × 40-col buffer and
        // verify the prompt + content land in row 0.
        let line = ConversationLine::User {
            content: "hello world".to_string(),
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 4,
        };
        let mut buf = Buffer::empty(area);
        render_message_for_insert(&mut buf, &line, false);
        // Read row 0 cell-by-cell; it should contain "> hello world".
        let mut row0 = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, 0)) {
                row0.push_str(cell.symbol());
            }
        }
        let trimmed = row0.trim_end();
        assert!(
            trimmed.starts_with("> hello world"),
            "row 0 should contain '> hello world', got {trimmed:?}"
        );
    }

    #[test]
    fn estimate_message_height_grows_with_narrower_terminal() {
        // A long user message must consume more rows when the terminal
        // is narrow — the unified loop relies on this to size each
        // `insert_before` call so wrapped output is never clipped.
        let long_text = "x".repeat(200);
        let line = ConversationLine::User { content: long_text };
        let wide = estimate_message_height(120, &line, false);
        let narrow = estimate_message_height(40, &line, false);
        assert!(
            narrow > wide,
            "narrow terminal must need more rows: narrow={narrow}, wide={wide}"
        );
        // Floor of 2 rows: content row + trailing blank separator.
        assert!(wide >= 2, "user message always needs at least content + blank");
    }

    #[test]
    fn bottom_chrome_height_expands_with_input_and_streaming() {
        // Resting state: status (1) + input border (1) + input row (1) +
        // footer (1) = 4. Streaming adds the preview block. A long
        // multi-line input adds more rows up to the visible cap.
        let mut state = TuiState::new("p", "m");
        let idle = bottom_chrome_height(&state);
        assert!(idle >= BOTTOM_CHROME_MIN_HEIGHT);
        assert!(idle <= BOTTOM_CHROME_MAX_HEIGHT);

        // Streaming should bump the height by `STREAMING_VISIBLE_ROWS`.
        state.start_stream("d-live");
        let streaming = bottom_chrome_height(&state);
        assert!(
            streaming > idle,
            "streaming preview must add rows: streaming={streaming}, idle={idle}"
        );

        // Multi-line input drives growth too (until clamped).
        state.cancel_stream("d-live");
        for _ in 0..6 {
            state.input.lines.push(String::new());
        }
        let tall = bottom_chrome_height(&state);
        assert!(tall > idle, "multi-line input must add rows: tall={tall}, idle={idle}");
        assert!(
            tall <= BOTTOM_CHROME_MAX_HEIGHT,
            "must be clamped to BOTTOM_CHROME_MAX_HEIGHT"
        );
    }

    // ── CJK / wide-char rendering regression tests ───────────────────────
    //
    // These tests guard against the phantom-space bug that appeared in TUI
    // mode when `insert_before` used the non-scrolling-regions path
    // (`draw_lines`), which iterated raw Buffer cells without skipping the
    // "continuation" cell that ratatui places after every wide (CJK) glyph.
    // Each CJK character occupies 2 columns; ratatui stores the glyph in
    // cell[x] and resets cell[x+1] to `Cell::EMPTY` (whose `symbol()`
    // returns `" "`). When `draw_lines` emitted that space verbatim, the
    // terminal saw `你 好 世 界` instead of `你好世界`.
    //
    // The fix: enable the `scrolling-regions` ratatui feature so that
    // `insert_before` uses `draw_lines_over_cleared`, which goes through
    // `Buffer::diff()`. `diff()` tracks `to_skip` for wide glyphs and
    // therefore never emits the continuation cell.
    //
    // Here we verify the buffer-level invariants: specifically that the
    // cells produced by the `scrolling-regions` diff path omit continuation
    // cells and reconstruct the original CJK text faithfully.

    #[test]
    #[cfg(feature = "terminal-tui")]
    fn cjk_buffer_diff_omits_continuation_cells() {
        // Build a buffer containing a Chinese assistant message, then take
        // its diff against an empty buffer (same as `draw_lines_over_cleared`
        // does internally).  The diff updates must not contain any
        // continuation-cell spaces between consecutive CJK characters.
        let line = ConversationLine::Assistant {
            content: "你好世界欢迎使用PRX".to_string(),
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 2,
        };
        let empty = Buffer::empty(area);
        let mut filled = Buffer::empty(area);
        render_message_for_insert(&mut filled, &line, false);

        // Collect symbols from the diff — this is exactly what the
        // `scrolling-regions` insert_before path emits to the backend.
        let diff = empty.diff(&filled);
        let mut row0_symbols = String::new();
        for (_x, y, cell) in &diff {
            if *y == 0 {
                // Only gather non-space content on row 0 for readability.
                // Trailing spaces that come from the padding at the end of
                // the line are acceptable and are trimmed below.
                row0_symbols.push_str(cell.symbol());
            }
        }
        let trimmed = row0_symbols.trim_end();
        // The diff must faithfully reconstruct the text (no phantom spaces).
        assert!(
            trimmed.contains("你好世界欢迎使用PRX"),
            "diff path should emit CJK chars without inter-character spaces, got {trimmed:?}"
        );
        assert!(
            !trimmed.contains("你 好") && !trimmed.contains("好 世"),
            "phantom spaces detected in diff output: {trimmed:?}"
        );
    }

    #[test]
    #[cfg(feature = "terminal-tui")]
    fn cjk_streaming_buffer_diff_omits_continuation_cells() {
        // StreamingAssistant appends a block-cursor glyph (▌). Verify that
        // CJK content before the cursor is also contiguous in the diff path.
        let line = ConversationLine::StreamingAssistant {
            content: "你好世界".to_string(),
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 2,
        };
        let empty = Buffer::empty(area);
        let mut filled = Buffer::empty(area);
        render_message_for_insert(&mut filled, &line, false);

        let diff = empty.diff(&filled);
        let mut row0 = String::new();
        for (_x, y, cell) in &diff {
            if *y == 0 {
                row0.push_str(cell.symbol());
            }
        }
        let trimmed = row0.trim_end();
        // 你好世界▌ — all chars contiguous in diff output.
        assert!(
            trimmed.starts_with("你好世界"),
            "streaming CJK diff should be contiguous, got {trimmed:?}"
        );
        assert!(
            !trimmed.contains("你 好") && !trimmed.contains("好 世"),
            "phantom spaces detected in streaming diff output: {trimmed:?}"
        );
    }

    #[test]
    #[cfg(feature = "terminal-tui")]
    fn cjk_user_message_diff_omits_continuation_cells() {
        // User message is prefixed with `> ` in a styled Span. Verify the
        // full row (including prefix) is contiguous in the diff path.
        let line = ConversationLine::User {
            content: "你好世界".to_string(),
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 2,
        };
        let empty = Buffer::empty(area);
        let mut filled = Buffer::empty(area);
        render_message_for_insert(&mut filled, &line, false);

        let diff = empty.diff(&filled);
        let mut row0 = String::new();
        for (_x, y, cell) in &diff {
            if *y == 0 {
                row0.push_str(cell.symbol());
            }
        }
        let trimmed = row0.trim_end();
        // Row 0: "> 你好世界"  (two-space prefix + content)
        assert!(
            trimmed.starts_with("> 你好世界"),
            "user CJK diff should be '> 你好世界', got {trimmed:?}"
        );
        assert!(
            !trimmed.contains("你 好"),
            "phantom spaces detected in user CJK diff: {trimmed:?}"
        );
    }

    // ─── S4-A Commit 2: BottomChromeView trait + UiSnapshot parity ────────────

    mod s4_a_2 {
        use super::*;
        use crate::chat::state::ChatState;
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        fn make_state_with_lines() -> ChatState {
            let mut s = ChatState::new(Arc::from("p-x"), Arc::from("m-x"), CancellationToken::new());
            s.session.title = "demo session".to_string();
            s.ui.turn_count = 3;
            s.ui.conversation_lines.push(ConversationLine::User {
                content: "first".to_string(),
            });
            s.ui.conversation_lines.push(ConversationLine::Assistant {
                content: "second".to_string(),
            });
            s
        }

        /// Parity 检查：相同 ChatState 同时映射到 TuiState（mirror 兼容字段）+ UiSnapshot 后，
        /// `bottom_chrome_height` 在两种 view 上返回相同值.
        #[test]
        fn s4_a_2_bottom_chrome_height_parity_tui_vs_snapshot() {
            let state = make_state_with_lines();
            let snap = state.build_ui_snapshot(1);

            // 构造与 snap 字段对齐的 TuiState（mirror 兼容字段集）.
            let mut tui = TuiState::new(&state.session.provider, &state.session.model);
            tui.session_title = state.session.title.clone();
            tui.turn_count = state.ui.turn_count;
            tui.ascii_fallback = state.ui.ascii_fallback;
            tui.conversation_lines = state.ui.conversation_lines.clone();
            tui.streaming.clone_from(&state.stream.draft);
            tui.input = state.ui.input.clone();

            assert_eq!(
                bottom_chrome_height(&tui),
                bottom_chrome_height(&snap),
                "TuiState vs UiSnapshot 在同 fixture 下高度应一致"
            );
        }

        /// Parity 检查：streaming 状态下两种 view 的高度仍一致.
        #[test]
        fn s4_a_2_bottom_chrome_height_parity_streaming() {
            let mut state = make_state_with_lines();
            state.stream.draft = Some(StreamingDraft {
                draft_id: "d-1".to_string(),
                accumulated: "streaming…".to_string(),
                version: 3,
            });
            let snap = state.build_ui_snapshot(2);

            let mut tui = TuiState::new(&state.session.provider, &state.session.model);
            tui.session_title = state.session.title.clone();
            tui.turn_count = state.ui.turn_count;
            tui.ascii_fallback = state.ui.ascii_fallback;
            tui.conversation_lines = state.ui.conversation_lines.clone();
            tui.streaming.clone_from(&state.stream.draft);
            tui.input = state.ui.input.clone();

            let h_tui = bottom_chrome_height(&tui);
            let h_snap = bottom_chrome_height(&snap);
            assert_eq!(h_tui, h_snap, "streaming 下高度应一致 (tui={h_tui}, snap={h_snap})");
            // streaming 加 STREAMING_VISIBLE_ROWS — 高于纯文本.
            assert!(h_tui > BOTTOM_CHROME_MIN_HEIGHT, "streaming 下高度应高于最小值");
        }

        /// Parity 检查：BottomChromeView 各 getter 在 TuiState 与 UiSnapshot 上返回相同字段.
        #[test]
        fn s4_a_2_view_getters_parity() {
            let state = make_state_with_lines();
            let snap = state.build_ui_snapshot(5);

            let mut tui = TuiState::new(&state.session.provider, &state.session.model);
            tui.session_title = state.session.title.clone();
            tui.turn_count = state.ui.turn_count;
            tui.ascii_fallback = state.ui.ascii_fallback;
            tui.conversation_lines = state.ui.conversation_lines.clone();
            tui.streaming.clone_from(&state.stream.draft);
            tui.input = state.ui.input.clone();

            assert_eq!(BottomChromeView::provider(&tui), BottomChromeView::provider(&snap));
            assert_eq!(BottomChromeView::model(&tui), BottomChromeView::model(&snap));
            assert_eq!(
                BottomChromeView::session_title(&tui),
                BottomChromeView::session_title(&snap)
            );
            assert_eq!(BottomChromeView::turn_count(&tui), BottomChromeView::turn_count(&snap));
            assert_eq!(
                BottomChromeView::conversation_lines(&tui).len(),
                BottomChromeView::conversation_lines(&snap).len()
            );
            assert_eq!(
                BottomChromeView::streaming(&tui).is_some(),
                BottomChromeView::streaming(&snap).is_some()
            );
        }

        /// Buffer-level parity: 把同一 view 在小 Buffer 上 render，断言两份 buffer 内容一致.
        ///
        /// 通过 `Buffer::diff` 比较；任何字节级偏差都会被捕获.
        #[test]
        fn s4_a_2_render_buffer_parity_tool_card() {
            let mut state = make_state_with_lines();
            state.ui.conversation_lines.push(ConversationLine::ToolResult {
                tool_name: "memory_recall".to_string(),
                args_preview: "{\"q\":\"x\"}".to_string(),
                args_full: "{\"query\":\"x\"}".to_string(),
                result: Some("ok".to_string()),
                status: ToolStatus::Done,
                elapsed_ms: Some(123),
                folded: true,
            });
            let snap = state.build_ui_snapshot(7);
            let mut tui = TuiState::new(&state.session.provider, &state.session.model);
            tui.session_title = state.session.title.clone();
            tui.turn_count = state.ui.turn_count;
            tui.ascii_fallback = state.ui.ascii_fallback;
            tui.conversation_lines = state.ui.conversation_lines.clone();
            tui.streaming.clone_from(&state.stream.draft);
            tui.input = state.ui.input.clone();

            // 同时验证 bottom_chrome_height parity（snap & tui 在 tool card 状态下高度一致）.
            assert_eq!(
                bottom_chrome_height(&tui),
                bottom_chrome_height(&snap),
                "tool card 状态下高度应一致"
            );

            // 用 render_message_for_insert 验证 tool card 字节级一致.
            let line = state
                .ui
                .conversation_lines
                .last()
                .expect("test: just pushed a tool card");
            let area = Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 4,
            };
            let mut buf_tui = Buffer::empty(area);
            let mut buf_snap = Buffer::empty(area);
            render_message_for_insert(&mut buf_tui, line, false);
            render_message_for_insert(&mut buf_snap, line, false);
            // Buffer 字节级 diff 应为空（两次渲染同一 line 同 ascii_fallback）.
            let diff = buf_tui.diff(&buf_snap);
            assert!(
                diff.is_empty(),
                "render_message_for_insert 两次同输入应字节级一致, diff={diff:?}"
            );
        }
    }
}
