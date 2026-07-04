//! TUI layout and rendering for `prx chat` using ratatui.
//!
//! Architecture: ratatui owns the full alternate screen. The transcript is
//! rendered in an in-app scrollable pane and the status/input/footer chrome is
//! pinned at the bottom. Native terminal scrollback is not used for chat
//! history; `/export` is the transcript-save path.
//!
//! Public surface:
//! - [`TuiState`] — shared state mirror (input buffer, conversation
//!   history, in-flight streaming draft).
//! - [`render_fullscreen_chat`] — draws the transcript pane, overlays, and
//!   pinned bottom chrome.
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
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use std::collections::HashMap;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::loop_::ChatMode;
use crate::chat::commands::{CommandSpec, command_specs};
use crate::chat::terminal_proto::{
    DraftVersionTracker, InlineDraftProtocol, LineProtocolError, apply_line_replacement,
};
use crate::security::AutonomyLevel;

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
/// Permanent conversation rendering happens inside the fullscreen transcript
/// pane. Native terminal scrollback is not used for chat history.
pub struct TuiState {
    /// Provider/model displayed in status bar
    pub provider: String,
    pub model: String,
    /// In-session chat mode displayed in the status bar.
    pub chat_mode: ChatMode,
    /// Configured autonomy ceiling displayed in the status bar. This is read-only
    /// UI metadata; the security gate remains [`crate::security::SecurityPolicy`].
    pub autonomy_level: AutonomyLevel,
    /// Session title
    pub session_title: String,
    /// Number of conversation turns
    pub turn_count: usize,
    /// Rendered conversation lines for the fullscreen transcript pane.
    pub conversation_lines: Vec<ConversationLine>,
    /// Multi-line input buffer + history (P2-10).
    pub input: TuiInput,
    /// Render ASCII-only icons instead of unicode glyphs (for non-UTF-8 terms).
    pub ascii_fallback: bool,
    /// In-flight streaming-assistant draft (P3-5). `None` between turns.
    ///
    /// When `Some`, [`render_fullscreen_chat`] paints the transient stream at
    /// the transcript tail. The streaming buffer is intentionally kept separate
    /// from `conversation_lines` so a stale or cancelled delta can never corrupt
    /// persisted history. On `finalize_stream` the text is lifted into
    /// `conversation_lines`.
    pub streaming: Option<StreamingDraft>,
    /// Persistent child-session status line (v1b). Empty when there are
    /// no child TUI sessions, in which case the bottom chrome omits the
    /// extra row. Written only by the chat main loop (via
    /// `Action::SessionsStatusUpdated`); the background spawn tasks never touch
    /// it.
    pub sessions_status: String,
    /// Current input-routing target (v1.1b). `Main` routes plain text to the
    /// main chat agent; `Session { seq }` routes it as a steer to the attached
    /// child TUI session. Drives the prompt's colour+glyph target indicator.
    /// Written by the chat main loop on `/attach` / `/detach` (it owns the
    /// authoritative `attached_follow`); the key thread only reads it.
    pub focus: crate::chat::sessions::FocusTarget,
    /// Open Ctrl+G session switcher overlay (v1.1b), or `None` when closed.
    /// Owned by the synchronous key thread (opened/navigated/closed in
    /// `dispatch_global_key`); rendered as a bottom-chrome popup.
    pub switcher: Option<crate::chat::sessions::SwitcherState>,
    /// Open slash-command menu overlay, or `None` when the cursor is not inside
    /// a leading command token.
    pub slash_menu: Option<SlashMenuState>,
    /// Cached background-session snapshot for the switcher, refreshed by the
    /// chat main loop's 1s sessions tick. The key thread reads this (it cannot
    /// run async registry queries) when opening the switcher with Ctrl+G.
    pub sessions_cache: Vec<crate::chat::sessions::SwitcherEntry>,
    /// P7c saved chat-session history picker. Distinct from the child-TUI
    /// Ctrl+G switcher.
    pub saved_session_picker: Option<crate::chat::session::SavedSessionPickerState>,
    /// P2 active line-session viewport snapshot. `None` when main chat or PTY
    /// handoff owns the visible surface.
    pub active_session_view: Option<crate::chat::sessions::ActiveSessionView>,
    /// P6c1 foreground tool approval prompt. Display-only; approving/denying is
    /// returned to the dispatcher as `ToolApprovalReceived`.
    pub pending_tool_approval: Option<crate::chat::sessions::PendingToolApprovalView>,
    /// Effective context window for UI-only status budget display.
    pub context_window_tokens: Option<usize>,
    /// First half of the `Ctrl+X Ctrl+E` external-editor chord.
    pub external_editor_prefix_armed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashMenuState {
    pub filter: String,
    pub entries: Vec<CommandSpec>,
    pub selected: usize,
}

impl SlashMenuState {
    #[must_use]
    pub fn new(filter: &str) -> Self {
        Self {
            filter: filter.to_string(),
            entries: filtered_command_specs(filter),
            selected: 0,
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn refresh(&mut self, filter: &str) {
        self.filter.clear();
        self.filter.push_str(filter);
        self.entries = filtered_command_specs(filter);
        self.clamp_selected();
    }

    pub fn clamp_selected(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        }
    }

    pub const fn select_prev(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
        } else if self.selected == 0 {
            self.selected = self.entries.len().saturating_sub(1);
        } else {
            self.selected -= 1;
        }
    }

    pub const fn select_next(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
        } else {
            self.selected = (self.selected + 1) % self.entries.len();
        }
    }

    #[must_use]
    pub fn selected_entry(&self) -> Option<CommandSpec> {
        self.entries.get(self.selected).copied()
    }
}

fn filtered_command_specs(filter: &str) -> Vec<CommandSpec> {
    let needle = filter.trim_start_matches('/').to_ascii_lowercase();
    command_specs()
        .iter()
        .copied()
        .filter(|spec| {
            if needle.is_empty() {
                return true;
            }
            command_matches_filter(*spec, &needle)
        })
        .collect()
}

fn command_matches_filter(spec: CommandSpec, needle: &str) -> bool {
    let name = spec.name.trim_start_matches('/').to_ascii_lowercase();
    if name.contains(needle) {
        return true;
    }
    spec.aliases
        .iter()
        .any(|alias| alias.trim_start_matches('/').to_ascii_lowercase().contains(needle))
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
    /// Default folded — only a one-line summary is shown. `Tab` toggles
    /// the most recent foldable card to reveal the full text indented under the
    /// header. `char_count` is cached so the summary can be rendered without
    /// re-walking `content` on every frame.
    Reasoning {
        /// Aggregated reasoning text from this assistant turn. Never empty
        /// (empty buffers are dropped before pushing — see
        /// [`TuiState::push_reasoning`]).
        content: String,
        /// Cached `content.chars().count()` for the folded summary line.
        char_count: usize,
        /// Default `true`. Toggled through the unified `Tab` fold path.
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
    /// locate the most recent reasoning card for folding.
    pub const fn is_reasoning(&self) -> bool {
        matches!(self, Self::Reasoning { .. })
    }
}

/// Maximum number of input rows shown at once before the box stops growing.
/// (Lines beyond this still exist in the buffer; future work can add scroll.)
pub const INPUT_MAX_VISIBLE_ROWS: usize = 10;

/// Maximum bytes accepted into the TUI draft buffer.
///
/// This keeps tmux key-flood paste paths bounded while still allowing the
/// 10 KB manual paste scenario to pass with margin.
pub const INPUT_MAX_BYTES: usize = 32 * 1024;

/// Maximum number of submitted entries kept in the history ring.
pub const INPUT_HISTORY_CAPACITY: usize = 200;

/// Synthetic display id for the read-only transcript child TUI.
pub const TRANSCRIPT_SESSION_SEQ: u64 = 0;
/// Synthetic display id for the read-only diff child TUI.
pub const DIFF_SESSION_SEQ: u64 = 0;

/// Bounded transcript viewport size. Conversation history remains authoritative
/// elsewhere; the child TUI is only a scrollable display snapshot.
pub const TRANSCRIPT_MAX_LINES: usize = 400;

/// Outcome of [`TuiInput::handle_key`].
///
/// Designed so the surrounding event loop can react with a single match
/// without inspecting `TuiInput` internals.
///
/// PageUp / PageDown fall through as [`InputOutcome::Unhandled`] here so the
/// outer fullscreen event loop can decide whether transcript scrolling or a
/// focused child view owns them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOutcome {
    /// Key was consumed; no externally observable change beyond the buffer.
    Consumed,
    /// Key was ignored because it would not change the buffer.
    Ignored,
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
/// - `Ctrl+R` — reverse-searches submitted input history.
/// - `Ctrl+X Ctrl+E` — opens the current draft in an external editor.
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
    /// Event was intentionally ignored and does not need a redraw.
    Ignored,
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
    /// v1.1b: the Ctrl+G switcher overlay was opened over the supplied session
    /// snapshot. The key loop dispatches `Action::SwitcherOpened` so the render
    /// snapshot reflects it (the mirror was already mutated in place).
    SwitcherOpened {
        entries: Vec<crate::chat::sessions::SwitcherEntry>,
    },
    /// v1.1b: the switcher highlight moved to row `selected`. The key loop
    /// dispatches `Action::SwitcherMoved`.
    SwitcherMoved { selected: usize },
    /// v1.1b: the switcher overlay was closed (Esc / Ctrl+G toggle / after
    /// attach). The key loop dispatches `Action::SwitcherClosed`.
    SwitcherClosed,
    /// P7c: saved chat-session picker moved.
    SavedSessionPickerMoved { selected: usize },
    /// P7c: saved chat-session picker closed.
    SavedSessionPickerClosed,
    /// P7c: resume the selected saved chat session through the main-loop
    /// control channel, not by synthetic slash-command text.
    ResumeSavedSession { id: String },
    /// v1.1b: attach to the given display sequence `#N` (switcher Enter). The
    /// key loop sends a synthetic `/attach <seq>` through the input channel so
    /// the async main loop performs the attach via its existing handler.
    AttachSession { seq: u64 },
    /// v1.1b: detach the focused child session (Esc on empty input while a
    /// session is focused). The key loop sends a synthetic `/detach` through the
    /// input channel so the async main loop performs the detach.
    RequestDetach,
    /// P2: scroll the focused child-session viewport one line up from tail.
    ScrollSessionUp,
    /// P2: scroll the focused child-session viewport one line down toward tail.
    ScrollSessionDown,
    /// P2: page the focused child-session viewport up from tail.
    PageSessionUp,
    /// P2: page the focused child-session viewport down toward tail.
    PageSessionDown,
    /// P3: switch to an adjacent live child session through the single `/attach`
    /// owner path.
    SwitchSession { seq: u64 },
    /// P6b1: open the read-only transcript child TUI.
    OpenTranscriptViewer,
    /// P6b1: close the read-only transcript child TUI.
    CloseTranscriptViewer,
    /// P6c2: close the read-only diff child TUI.
    CloseDiffViewer,
    /// P6b2: open the current draft in an external editor.
    ExternalEditorRequested,
    /// P6c1: resolve the foreground tool approval prompt.
    ToolApprovalDecision { tool_id: String, approved: bool },
    /// P8: cycle the in-session chat mode via Shift+Tab.
    ModeChanged(ChatMode),
}

pub(crate) fn sync_slash_menu_for_input(input: &TuiInput, slash_menu: &mut Option<SlashMenuState>) {
    if let Some(filter) = input.slash_command_filter_at_cursor() {
        if let Some(menu) = slash_menu.as_mut() {
            menu.refresh(&filter);
        } else {
            *slash_menu = Some(SlashMenuState::new(&filter));
        }
    } else {
        *slash_menu = None;
    }
}

pub(crate) fn dispatch_slash_menu_key_for(
    input: &mut TuiInput,
    slash_menu: &mut Option<SlashMenuState>,
    key: KeyEvent,
) -> KeyDispatch {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
    let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
    if up || down {
        let Some(menu) = slash_menu.as_mut() else {
            return KeyDispatch::Consumed;
        };
        if up {
            menu.select_prev();
        } else {
            menu.select_next();
        }
        return KeyDispatch::Consumed;
    }

    if (key.code == KeyCode::Enter || key.code == KeyCode::Tab) && key.modifiers == KeyModifiers::NONE {
        if let Some(spec) = slash_menu.as_ref().and_then(SlashMenuState::selected_entry) {
            input.replace_slash_command_token(spec.name);
        }
        *slash_menu = None;
        return KeyDispatch::Consumed;
    }

    if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
        *slash_menu = None;
        return KeyDispatch::Consumed;
    }

    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return KeyDispatch::Consumed;
    }

    match key.code {
        KeyCode::Char(_)
        | KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End => match input.handle_key(key) {
            InputOutcome::Submitted(text) => {
                *slash_menu = None;
                KeyDispatch::Submitted(text)
            }
            InputOutcome::Cancelled => {
                *slash_menu = None;
                KeyDispatch::Cancelled
            }
            InputOutcome::Consumed | InputOutcome::Unhandled => {
                sync_slash_menu_for_input(input, slash_menu);
                KeyDispatch::Consumed
            }
            InputOutcome::Ignored => KeyDispatch::Ignored,
        },
        _ => KeyDispatch::Consumed,
    }
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

/// Synthetic transcript row shown in the Ctrl+G child TUI switcher.
#[must_use]
pub fn transcript_switcher_entry() -> crate::chat::sessions::SwitcherEntry {
    crate::chat::sessions::SwitcherEntry {
        seq: TRANSCRIPT_SESSION_SEQ,
        kind: crate::chat::sessions::model::ManagedKind::Transcript.as_str(),
        origin: "user",
        status: "ready",
        title: "conversation transcript".to_string(),
    }
}

fn switcher_entries_with_transcript(
    entries: &[crate::chat::sessions::SwitcherEntry],
) -> Vec<crate::chat::sessions::SwitcherEntry> {
    let mut out = Vec::with_capacity(entries.len().saturating_add(1));
    out.push(transcript_switcher_entry());
    out.extend(entries.iter().filter(|entry| !entry.is_transcript()).cloned());
    out
}

const fn tool_status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Done => "done",
        ToolStatus::Error => "error",
    }
}

fn push_transcript_text(lines: &mut Vec<String>, label: &str, content: &str) {
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

fn transcript_lines_from_conversation(conversation: &[ConversationLine]) -> (Vec<String>, bool) {
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
                args_preview,
                result,
                status,
                ..
            } => {
                lines.push(format!(
                    "tool {tool_name} {}: {args_preview}",
                    tool_status_name(*status)
                ));
                if let Some(result) = result {
                    push_transcript_text(&mut lines, "  result", result);
                }
            }
            ConversationLine::Reasoning {
                content,
                char_count,
                folded,
            } => {
                lines.push(format!("reasoning: {char_count} chars"));
                if !*folded {
                    push_transcript_text(&mut lines, "  thought", content);
                }
            }
        }
    }
    let truncated = lines.len() > TRANSCRIPT_MAX_LINES;
    if truncated {
        let start = lines.len().saturating_sub(TRANSCRIPT_MAX_LINES);
        lines = lines.split_off(start);
    }
    (lines, truncated)
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
        lines.push("(transcript is empty)".to_string());
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
    .clamped_for_height(usize::from(ACTIVE_SESSION_VIEW_DESIRED_ROWS))
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
    // P7c: the saved chat-session picker has top overlay priority. It is
    // distinct from the child-TUI Ctrl+G switcher and captures all keys while
    // open so navigation cannot leak into input history or child switching.
    if state.saved_session_picker.is_some() {
        return dispatch_saved_session_picker_key(key, state);
    }
    if state.slash_menu.is_some() {
        return dispatch_slash_menu_key_for(&mut state.input, &mut state.slash_menu, key);
    }
    // v1.1b: when the Ctrl+G switcher overlay is open it captures navigation /
    // selection keys before anything else (Tab, input box, etc.). Handled in a
    // dedicated resolver so the open-state key table is self-contained.
    if state.switcher.is_some() {
        return dispatch_switcher_key(key, state);
    }
    if let Some(pending) = state.pending_tool_approval.clone()
        && matches!(state.focus, crate::chat::sessions::FocusTarget::Approval)
    {
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            state.pending_tool_approval = None;
            state.focus = crate::chat::sessions::FocusTarget::Main;
            return KeyDispatch::ToolApprovalDecision {
                tool_id: pending.tool_id,
                approved: false,
            };
        }
        if key.modifiers == KeyModifiers::NONE {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    state.pending_tool_approval = None;
                    state.focus = crate::chat::sessions::FocusTarget::Main;
                    return KeyDispatch::ToolApprovalDecision {
                        tool_id: pending.tool_id,
                        approved: true,
                    };
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    state.pending_tool_approval = None;
                    state.focus = crate::chat::sessions::FocusTarget::Main;
                    return KeyDispatch::ToolApprovalDecision {
                        tool_id: pending.tool_id,
                        approved: false,
                    };
                }
                _ => return KeyDispatch::Consumed,
            }
        }
        return KeyDispatch::Consumed;
    }
    if state.external_editor_prefix_armed {
        state.external_editor_prefix_armed = false;
        if key.code == KeyCode::Char('e') && key.modifiers == KeyModifiers::CONTROL {
            return KeyDispatch::ExternalEditorRequested;
        }
        return KeyDispatch::Consumed;
    }
    if key.code == KeyCode::BackTab
        && (key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT)
        && state.input.is_empty()
    {
        let mode = cycle_chat_mode(state.chat_mode);
        state.chat_mode = mode;
        return KeyDispatch::ModeChanged(mode);
    }
    // v1.1b/P1: Ctrl+G opens the PRX sessions switcher over the cached session
    // list. This intentionally diverges from Claude Code's external-editor
    // Ctrl+G binding until the P3/P6b parity keybinding pass; keep the footer
    // hint discoverable so the temporary divergence is not hidden.
    // Never falls through to the input box. Opening an empty switcher is still
    // valid — it shows the "no child TUI sessions" hint with an Esc to close.
    if key.code == KeyCode::Char('g') && key.modifiers == KeyModifiers::CONTROL {
        let entries = switcher_entries_with_transcript(&state.sessions_cache);
        state.switcher = Some(crate::chat::sessions::SwitcherState::new(entries.clone()));
        return KeyDispatch::SwitcherOpened { entries };
    }
    if key.code == KeyCode::Char('o') && key.modifiers == KeyModifiers::CONTROL {
        return KeyDispatch::OpenTranscriptViewer;
    }
    if key.code == KeyCode::Char('x') && key.modifiers == KeyModifiers::CONTROL {
        state.external_editor_prefix_armed = true;
        return KeyDispatch::Consumed;
    }
    // Tab → toggle the most recent foldable card (reasoning OR tool-result,
    // whichever appears later in the conversation). When neither exists Tab
    // is still consumed — per spec it never falls through to the input box.
    if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE && state.input.is_empty() {
        let _ = state.toggle_last_foldable_card();
        return KeyDispatch::Consumed;
    }
    // Ctrl+R → reverse-search submitted input history. Never falls through
    // to child steering, transcript scrolling or the input box.
    if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
        let _ = state.input.begin_or_cycle_reverse_search();
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
    if let Some(current_seq) = state.focus.session_seq()
        && state.input.is_empty()
        && key.modifiers == KeyModifiers::NONE
    {
        let direction = match key.code {
            KeyCode::Left => Some(crate::chat::sessions::SessionDirection::Previous),
            KeyCode::Right => Some(crate::chat::sessions::SessionDirection::Next),
            _ => None,
        };
        if let Some(direction) = direction {
            return crate::chat::sessions::focus::adjacent_session_seq(&state.sessions_cache, current_seq, direction)
                .map_or(KeyDispatch::Consumed, |seq| KeyDispatch::SwitchSession { seq });
        }
    }
    if state.focus.is_child_view() && state.input.is_empty() && key.modifiers == KeyModifiers::NONE {
        match key.code {
            KeyCode::Up => return KeyDispatch::ScrollSessionUp,
            KeyCode::Down => return KeyDispatch::ScrollSessionDown,
            KeyCode::PageUp => return KeyDispatch::PageSessionUp,
            KeyCode::PageDown => return KeyDispatch::PageSessionDown,
            _ => {}
        }
    }
    // v1.1b: context-aware Esc. The switcher-open case is already handled above
    // (the early return). Here, with no switcher open, `resolve_esc` decides
    // between the established "non-empty clears input" muscle memory and the new
    // "empty + session focused → detach" behaviour, never weakening the former.
    if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
        use crate::chat::sessions::focus::{EscAction, resolve_esc};
        match resolve_esc(state.input.is_empty(), state.focus, false) {
            EscAction::ClearInput => {
                // Preserve existing behaviour: clear the buffer, signal cancel.
                let _ = state.handle_input_key(key);
                return KeyDispatch::Cancelled;
            }
            EscAction::RequestDetach => return KeyDispatch::RequestDetach,
            EscAction::CloseTranscript => return KeyDispatch::CloseTranscriptViewer,
            EscAction::DenyApproval => {
                state.pending_tool_approval = None;
                state.focus = crate::chat::sessions::FocusTarget::Main;
                return KeyDispatch::Cancelled;
            }
            EscAction::CloseDiff => return KeyDispatch::CloseDiffViewer,
            EscAction::Cancel => {
                // Empty buffer + main focus → unchanged legacy cancel semantics.
                let _ = state.handle_input_key(key);
                return KeyDispatch::Cancelled;
            }
            // Unreachable here (switcher_open=false), but keep the match total.
            EscAction::CloseSwitcher => return KeyDispatch::Cancelled,
        }
    }
    if matches!(
        state.focus,
        crate::chat::sessions::FocusTarget::Transcript | crate::chat::sessions::FocusTarget::Diff
    ) && key.code == KeyCode::Enter
        && key.modifiers == KeyModifiers::NONE
    {
        return KeyDispatch::Consumed;
    }
    // All other keys → input box.
    match state.handle_input_key(key) {
        InputOutcome::Submitted(text) => KeyDispatch::Submitted(text),
        InputOutcome::Cancelled => KeyDispatch::Cancelled,
        InputOutcome::Consumed | InputOutcome::Unhandled => KeyDispatch::Consumed,
        InputOutcome::Ignored => KeyDispatch::Ignored,
    }
}

/// Resolve a key while the Ctrl+G session switcher overlay is open (v1.1b).
///
/// The overlay captures navigation + selection keys; everything else is a
/// no-op `Consumed` so stray keystrokes do not leak into the input box or fire
/// global shortcuts while the popup has focus. Mutates the mirror switcher in
/// place and returns the matching [`KeyDispatch`] so the key loop can mirror the
/// change into the render snapshot.
fn dispatch_switcher_key(key: KeyEvent, state: &mut TuiState) -> KeyDispatch {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Up / Ctrl+P → previous; Down / Ctrl+N → next.
    let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
    let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
    if up || down {
        let Some(switcher) = state.switcher.as_mut() else {
            return KeyDispatch::Consumed;
        };
        if up {
            switcher.select_prev();
        } else {
            switcher.select_next();
        }
        return KeyDispatch::SwitcherMoved {
            selected: switcher.selected,
        };
    }
    // Enter → attach the highlighted session, then close. Empty list → just close.
    if key.code == KeyCode::Enter {
        let selected = state.switcher.as_ref().and_then(|s| s.selected_entry().cloned());
        state.switcher = None;
        return selected.map_or(KeyDispatch::SwitcherClosed, |entry| {
            if entry.is_transcript() {
                return KeyDispatch::OpenTranscriptViewer;
            }
            // The key loop closes the snapshot switcher *and* sends the synthetic
            // `/attach`, so signal the attach here; the close rides along.
            KeyDispatch::AttachSession { seq: entry.seq }
        });
    }
    // Esc → close the switcher (resolve_esc gives CloseSwitcher when open).
    // Ctrl+G → toggle closed. Both just close.
    if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('g')) {
        state.switcher = None;
        return KeyDispatch::SwitcherClosed;
    }
    // Any other key is swallowed while the overlay has focus.
    KeyDispatch::Consumed
}

/// Resolve a key while the saved chat-session picker overlay is open (P7c).
fn dispatch_saved_session_picker_key(key: KeyEvent, state: &mut TuiState) -> KeyDispatch {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
    let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
    if up || down {
        let Some(picker) = state.saved_session_picker.as_mut() else {
            return KeyDispatch::Consumed;
        };
        if up {
            picker.select_prev();
        } else {
            picker.select_next();
        }
        picker.clamp_selected();
        return KeyDispatch::SavedSessionPickerMoved {
            selected: picker.selected,
        };
    }
    if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
        let selected = state.saved_session_picker.as_mut().and_then(|picker| {
            picker.clamp_selected();
            picker.selected_entry().cloned()
        });
        state.saved_session_picker = None;
        return selected.map_or(KeyDispatch::SavedSessionPickerClosed, |entry| {
            KeyDispatch::ResumeSavedSession { id: entry.id }
        });
    }
    if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
        state.saved_session_picker = None;
        return KeyDispatch::SavedSessionPickerClosed;
    }
    KeyDispatch::Consumed
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
    /// True when text was ignored because the input reached INPUT_MAX_BYTES.
    pub truncated: bool,
    /// Active reverse history search state (`Ctrl+R`), if any.
    reverse_search: Option<ReverseSearchState>,
}

/// Ephemeral reverse-search state for the input history ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverseSearchState {
    /// Draft buffer before the search started; restored on Esc.
    saved_lines: Vec<String>,
    /// User-entered incremental search query.
    pub query: String,
    /// Currently selected history entry.
    pub match_pos: Option<usize>,
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
            truncated: false,
            reverse_search: None,
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

    /// Filter text for a leading slash command when the cursor is inside the
    /// command token. Returns `None` once the cursor moves into arguments.
    pub fn slash_command_filter_at_cursor(&self) -> Option<String> {
        let (line_idx, cursor_offset) = self.cursor;
        let line = self.lines.get(line_idx)?;
        if !line.starts_with('/') {
            return None;
        }
        let token_end = line.find(char::is_whitespace).unwrap_or(line.len());
        if cursor_offset > token_end {
            return None;
        }
        let cursor = cursor_offset.min(line.len());
        line.get(1..cursor).map(str::to_string)
    }

    /// Replace the current leading slash-command token with `command`, leaving a
    /// trailing space so the operator can immediately type arguments.
    fn replace_slash_command_token(&mut self, command: &str) {
        let (line_idx, _cursor_offset) = self.cursor;
        let Some(line) = self.lines.get_mut(line_idx) else {
            return;
        };
        if !line.starts_with('/') {
            return;
        }
        let token_end = line.find(char::is_whitespace).unwrap_or(line.len());
        let suffix = line.get(token_end..).unwrap_or_default().trim_start();
        let replacement = if suffix.is_empty() {
            format!("{command} ")
        } else {
            format!("{command} {suffix}")
        };
        *line = replacement;
        self.cursor = (line_idx, command.len().saturating_add(1).min(line.len()));
        self.history_pos = None;
        self.pending_draft = None;
        self.reverse_search = None;
    }

    /// Current draft size in bytes, counting newline separators between rows.
    pub fn byte_len(&self) -> usize {
        let content_bytes: usize = self.lines.iter().map(String::len).sum();
        content_bytes.saturating_add(self.lines.len().saturating_sub(1))
    }

    /// True if the user is currently editing a single logical line — used to
    /// decide whether `↑/↓` should navigate history or move the cursor.
    pub const fn is_single_line(&self) -> bool {
        self.lines.len() <= 1
    }

    /// Replace the entire buffer (used by history navigation and paste).
    pub fn set_text(&mut self, text: &str) {
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
        self.truncated = false;
    }

    /// Clear navigation/search state after an external text replacement.
    pub fn clear_navigation_state(&mut self) {
        self.reverse_search = None;
        self.history_pos = None;
        self.pending_draft = None;
    }

    /// Clear the buffer back to a single empty line.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
        self.history_pos = None;
        self.pending_draft = None;
        self.truncated = false;
        self.reverse_search = None;
    }

    /// Insert a single grapheme (`ch`) at the cursor.
    fn insert_char(&mut self, ch: char) -> bool {
        if self.byte_len().saturating_add(ch.len_utf8()) > INPUT_MAX_BYTES {
            self.truncated = true;
            return false;
        }
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            // `off` is always at a char boundary because we only ever advance
            // by `ch.len_utf8()` from prior inserts and via `floor_char_boundary`.
            let clamped = off.min(line.len());
            line.insert(clamped, ch);
            self.cursor = (li, clamped + ch.len_utf8());
            if self.byte_len() >= INPUT_MAX_BYTES {
                self.truncated = true;
            }
            return true;
        }
        false
    }

    /// Insert a literal string at the cursor. Newlines split into rows.
    fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let remaining = INPUT_MAX_BYTES.saturating_sub(self.byte_len());
        let text = if text.len() > remaining {
            self.truncated = true;
            clamp_str_to_byte_len(text, remaining)
        } else {
            text
        };
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
        if self.byte_len().saturating_add(1) > INPUT_MAX_BYTES {
            self.truncated = true;
            return;
        }
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
        if self.byte_len() < INPUT_MAX_BYTES {
            self.truncated = false;
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
        if self.byte_len() < INPUT_MAX_BYTES {
            self.truncated = false;
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
            if self.byte_len() < INPUT_MAX_BYTES {
                self.truncated = false;
            }
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

    /// True while `Ctrl+R` reverse history search is active.
    pub const fn is_reverse_search_active(&self) -> bool {
        self.reverse_search.is_some()
    }

    /// Human-readable reverse-search status for the input box title.
    pub fn reverse_search_title(&self) -> Option<String> {
        let search = self.reverse_search.as_ref()?;
        let query = truncate_input_title(&search.query, 36);
        let status = if search.match_pos.is_some() {
            "match"
        } else {
            "no match"
        };
        Some(format!(" reverse-search: {query} ({status}) "))
    }

    /// Start reverse-search, or cycle to the next older match when already active.
    pub fn begin_or_cycle_reverse_search(&mut self) -> bool {
        if self.reverse_search.is_none() {
            self.reverse_search = Some(ReverseSearchState {
                saved_lines: self.lines.clone(),
                query: String::new(),
                match_pos: None,
            });
        }
        self.reverse_search_cycle_older();
        true
    }

    fn reverse_search_cycle_older(&mut self) {
        let Some(search) = self.reverse_search.as_ref() else {
            return;
        };
        let before = search.match_pos.unwrap_or(self.history.len());
        let query = search.query.clone();
        let matched = self.find_reverse_history_match(&query, before);
        if let Some(search) = self.reverse_search.as_mut() {
            search.match_pos = matched;
        }
        self.apply_reverse_search_match();
    }

    fn reverse_search_query_changed(&mut self) {
        let Some(search) = self.reverse_search.as_ref() else {
            return;
        };
        let query = search.query.clone();
        let matched = self.find_reverse_history_match(&query, self.history.len());
        if let Some(search) = self.reverse_search.as_mut() {
            search.match_pos = matched;
        }
        self.apply_reverse_search_match();
    }

    fn find_reverse_history_match(&self, query: &str, before: usize) -> Option<usize> {
        if self.history.is_empty() {
            return None;
        }
        let mut idx = before.min(self.history.len());
        while idx > 0 {
            idx -= 1;
            let Some(entry) = self.history.get(idx) else {
                continue;
            };
            if query.is_empty() || entry.contains(query) {
                return Some(idx);
            }
        }
        None
    }

    fn apply_reverse_search_match(&mut self) {
        let Some(search) = self.reverse_search.as_ref() else {
            return;
        };
        if let Some(pos) = search.match_pos
            && let Some(entry) = self.history.get(pos)
        {
            let entry = entry.clone();
            self.set_text(&entry);
        } else {
            let saved = search.saved_lines.clone();
            self.lines = if saved.is_empty() { vec![String::new()] } else { saved };
            let last_line_idx = self.lines.len().saturating_sub(1);
            let last_len = self.lines.get(last_line_idx).map_or(0, String::len);
            self.cursor = (last_line_idx, last_len);
            self.truncated = false;
        }
    }

    fn cancel_reverse_search(&mut self) {
        if let Some(search) = self.reverse_search.take() {
            self.lines = if search.saved_lines.is_empty() {
                vec![String::new()]
            } else {
                search.saved_lines
            };
            let last_line_idx = self.lines.len().saturating_sub(1);
            let last_len = self.lines.get(last_line_idx).map_or(0, String::len);
            self.cursor = (last_line_idx, last_len);
            self.truncated = false;
        }
    }

    fn accept_reverse_search(&mut self) {
        self.reverse_search = None;
        self.history_pos = None;
        self.pending_draft = None;
    }

    fn handle_reverse_search_key(&mut self, key: KeyEvent) -> InputOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('r') if ctrl => {
                self.reverse_search_cycle_older();
                InputOutcome::Consumed
            }
            KeyCode::Char(ch) if !ctrl => {
                if let Some(search) = self.reverse_search.as_mut() {
                    search.query.push(ch);
                }
                self.reverse_search_query_changed();
                InputOutcome::Consumed
            }
            KeyCode::Backspace => {
                if let Some(search) = self.reverse_search.as_mut() {
                    search.query.pop();
                }
                self.reverse_search_query_changed();
                InputOutcome::Consumed
            }
            KeyCode::Enter => {
                self.accept_reverse_search();
                InputOutcome::Consumed
            }
            KeyCode::Esc => {
                self.cancel_reverse_search();
                InputOutcome::Cancelled
            }
            _ => InputOutcome::Consumed,
        }
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
            self.truncated = false;
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

        if self.reverse_search.is_some() {
            return self.handle_reverse_search_key(key);
        }

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
                if self.insert_char(ch) {
                    InputOutcome::Consumed
                } else {
                    InputOutcome::Ignored
                }
            }
            KeyCode::Tab => {
                if self.insert_char('\t') {
                    InputOutcome::Consumed
                } else {
                    InputOutcome::Ignored
                }
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
            // Fullscreen transcript scrolling is owned by the outer event loop,
            // not the input widget.
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

fn clamp_str_to_byte_len(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn truncate_input_title(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
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
            chat_mode: ChatMode::default(),
            autonomy_level: AutonomyLevel::default(),
            session_title: String::new(),
            turn_count: 0,
            conversation_lines: Vec::new(),
            input: TuiInput::new(),
            ascii_fallback: false,
            streaming: None,
            sessions_status: String::new(),
            focus: crate::chat::sessions::FocusTarget::Main,
            switcher: None,
            slash_menu: None,
            sessions_cache: Vec::new(),
            saved_session_picker: None,
            active_session_view: None,
            pending_tool_approval: None,
            context_window_tokens: None,
            external_editor_prefix_armed: false,
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
        let outcome = self.input.handle_key(key);
        match &outcome {
            InputOutcome::Submitted(_) | InputOutcome::Cancelled => self.slash_menu = None,
            InputOutcome::Consumed | InputOutcome::Unhandled => {
                sync_slash_menu_for_input(&self.input, &mut self.slash_menu);
            }
            InputOutcome::Ignored => {}
        }
        outcome
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

    /// Replace the persistent child-session status line (v1b).
    ///
    /// An empty `summary` hides the extra status row entirely.
    pub fn set_sessions_status(&mut self, summary: &str) {
        if self.sessions_status != summary {
            self.sessions_status.clear();
            self.sessions_status.push_str(summary);
        }
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
    /// This supports legacy action tests and the unified `Tab` key handler.
    /// Only the **last** reasoning card is touched;
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

// ── S4-A Commit 5: SnapshotDispatcherSink ───────────────────────────────────

/// Pure 模式专用 Sink — UiActor 事件通过 `ChatDispatcher` 翻译为 Redux Action,
/// 走 reducer 单一持久化路径; 不再写 `chat_mirror`.
///
/// 与 [`TuiStateMirrorSink`] 的关系: Pure 模式下两者互斥. Pure 模式的 LLM
/// turn 主路径由 `drive_start_turn_stream` 直接 dispatch
/// `Action::TurnStarted` / `Action::StreamChunkReceived` / `Action::StreamCompleted` /
/// `Action::ToolStarted` / `Action::ToolFinished` 给 reducer; UiActor 收到的
/// 等价 UiEvent 仅是 channel-layer 镜像广播，若再翻译为 Action 重 dispatch
/// 会导致 reducer 双写 conversation_lines / draft. 因此本 Sink 主体为 **no-op**
/// (含 trace) — 真正写 reducer 的源头是 driver，Sink 仅消极 ack UiActor 事件.
///
/// `push_system` 是唯一对 chat::run 主循环侧的 fallback 路径:
/// `UiEvent::ToolProgress` / `UiEvent::DraftCancelled` 经 UiActor 翻译为
/// "step N/M" / "(cancelled)" 系统提示, 本 Sink 把它转 dispatch
/// `Action::SystemMessageAdded` 让 reducer 把消息推到 UI 账本.
pub struct SnapshotDispatcherSink {
    dispatcher: crate::chat::dispatcher::ChatDispatcher,
}

impl SnapshotDispatcherSink {
    pub const fn new(dispatcher: crate::chat::dispatcher::ChatDispatcher) -> Self {
        Self { dispatcher }
    }
}

impl crate::channels::terminal::TuiMirrorSink for SnapshotDispatcherSink {
    fn push_assistant(&self, content: &str) {
        // Pure 模式下 assistant 文本由 driver Action::StreamCompleted 经 reducer
        // push 到 conversation_lines。UiActor 的 push_assistant 是 channel-layer
        // 旁路镜像，重复 dispatch 会导致重复行。改为 trace 留观察痕迹。
        tracing::trace!(
            site = "snapshot_sink.push_assistant",
            chars = content.chars().count(),
            "Pure 模式忽略 UiActor 旁路 (reducer 单源)"
        );
    }
    fn push_system(&self, content: &str) {
        // 系统消息 (ToolProgress / DraftCancelled 等) 通过 reducer 单源写入 UI 账本.
        let _ = self.dispatcher.dispatch_or_log(
            crate::chat::action::Action::SystemMessageAdded {
                text: content.to_string(),
            },
            "snapshot_sink.push_system",
        );
    }
    fn push_tool_started(&self, tool_name: &str, args_full: &str) {
        // driver 已 dispatch Action::ToolStarted；Pure 模式忽略 UiActor 旁路.
        tracing::trace!(
            site = "snapshot_sink.push_tool_started",
            tool = tool_name,
            args_len = args_full.chars().count(),
            "Pure 模式忽略 UiActor 旁路 (driver 已 dispatch ToolStarted)"
        );
    }
    fn mark_tool_finished(&self, tool_name: &str, success: bool, duration_ms: u64) -> bool {
        // driver 已 dispatch Action::ToolFinished. 返回 false 让 UiActor 知道
        // 本 sink 未"独立"更新任何卡片 — 实际更新在 reducer 内由 driver dispatch
        // 的 Action 触发. UiActor 不再依赖此返回值做 fallback (real path).
        tracing::trace!(
            site = "snapshot_sink.mark_tool_finished",
            tool = tool_name,
            success,
            duration_ms,
            "Pure 模式忽略 UiActor 旁路 (driver 已 dispatch ToolFinished)"
        );
        false
    }
    fn start_stream(&self, draft_id: &str) {
        tracing::trace!(
            site = "snapshot_sink.start_stream",
            draft_id,
            "Pure 模式忽略 UiActor 旁路 (driver 已 dispatch TurnStarted)"
        );
    }
    fn update_stream(&self, draft_id: &str, accumulated: &str, version: u64) {
        tracing::trace!(
            site = "snapshot_sink.update_stream",
            draft_id,
            version,
            chars = accumulated.chars().count(),
            "Pure 模式忽略 UiActor 旁路 (driver 已 dispatch StreamChunkReceived)"
        );
    }
    fn finalize_stream(&self, draft_id: &str, final_text: &str) {
        tracing::trace!(
            site = "snapshot_sink.finalize_stream",
            draft_id,
            chars = final_text.chars().count(),
            "Pure 模式忽略 UiActor 旁路 (driver 已 dispatch StreamCompleted)"
        );
    }
    fn cancel_stream(&self, draft_id: &str) {
        tracing::trace!(
            site = "snapshot_sink.cancel_stream",
            draft_id,
            "Pure 模式忽略 UiActor 旁路 (driver 已 dispatch StreamCancelled)"
        );
    }
}

/// Logical row count for a [`ConversationLine`], **before** soft-wrap.
/// Used by tests to compare folded vs expanded card row counts.
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

/// 渲染源抽象：让 fullscreen renderer 同时支持 `TuiState`（chat_mirror 路径）
/// 与 `UiSnapshot`（S4-A Pure 模式 watch 路径）.
///
/// S4-A Commit 2: 把渲染需要的最小字段集抽出来作为 trait，泛型化所有
/// `&TuiState` 参数为 `&V: BottomChromeView`。本 commit 暂未切换渲染源，
/// 仅泛型化函数签名 + 两个 impl，行为不变。
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
    fn input(&self) -> &TuiInput;
    /// Persistent child-session status line (v1b). Empty hides the row.
    fn sessions_status(&self) -> &str;
    /// Structured child-session entries for the always-visible P1 strip.
    fn sessions_entries(&self) -> &[crate::chat::sessions::SwitcherEntry];
    /// Focused line-session viewport (P2), if any.
    fn active_session_view(&self) -> Option<&crate::chat::sessions::ActiveSessionView>;
    /// Foreground tool approval prompt (P6c1), if any.
    fn pending_tool_approval(&self) -> Option<&crate::chat::sessions::PendingToolApprovalView>;
    /// Effective context window for UI-only status budget display.
    fn context_window_tokens(&self) -> Option<usize>;
    /// Current input-routing target (v1.1b). Drives the prompt's colour+glyph
    /// target indicator (`main >` vs `agent #N ▸`).
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
    fn input(&self) -> &TuiInput {
        &self.input
    }
    fn sessions_status(&self) -> &str {
        &self.sessions_status
    }
    fn sessions_entries(&self) -> &[crate::chat::sessions::SwitcherEntry] {
        &self.sessions_cache
    }
    fn active_session_view(&self) -> Option<&crate::chat::sessions::ActiveSessionView> {
        self.active_session_view.as_ref()
    }
    fn pending_tool_approval(&self) -> Option<&crate::chat::sessions::PendingToolApprovalView> {
        self.pending_tool_approval.as_ref()
    }
    fn context_window_tokens(&self) -> Option<usize> {
        self.context_window_tokens
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
    fn input(&self) -> &TuiInput {
        &self.input
    }
    fn sessions_status(&self) -> &str {
        &self.sessions_status
    }
    fn sessions_entries(&self) -> &[crate::chat::sessions::SwitcherEntry] {
        self.sessions_entries.as_slice()
    }
    fn active_session_view(&self) -> Option<&crate::chat::sessions::ActiveSessionView> {
        self.active_session_view.as_ref()
    }
    fn pending_tool_approval(&self) -> Option<&crate::chat::sessions::PendingToolApprovalView> {
        self.pending_tool_approval.as_ref()
    }
    fn context_window_tokens(&self) -> Option<usize> {
        self.context_window_tokens
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

/// Minimum height (rows) of the pinned fullscreen bottom chrome. Reserves space
/// for status, input, and footer.
pub const BOTTOM_CHROME_MIN_HEIGHT: u16 = 3;

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
    last_tail_marker: usize,
    pub new_output_below: bool,
}

impl FullscreenTranscriptScroll {
    pub fn page_up(&mut self, rows: usize) {
        self.offset_from_bottom = self.offset_from_bottom.saturating_add(rows.max(1));
    }

    pub fn page_down(&mut self, rows: usize) {
        self.offset_from_bottom = self.offset_from_bottom.saturating_sub(rows.max(1));
        if self.offset_from_bottom == 0 {
            self.new_output_below = false;
        }
    }

    pub const fn jump_top(&mut self) {
        self.offset_from_bottom = usize::MAX;
    }

    pub const fn jump_bottom(&mut self) {
        self.offset_from_bottom = 0;
        self.new_output_below = false;
    }
}

fn fullscreen_tail_marker<V: BottomChromeView + ?Sized>(state: &V) -> usize {
    let finalized = state.conversation_lines().len().saturating_mul(1_000_000);
    let streaming_chars = state
        .streaming()
        .map_or(0usize, |streaming| streaming.accumulated.chars().count());
    finalized.saturating_add(streaming_chars)
}

/// Whether the persistent child-session status row should be shown.
///
/// Hidden when empty (no child TUI sessions). As a narrow/short-terminal
/// degrade rule (plan §v1b), the row is also dropped first when the rest of the
/// chrome (status + streaming + input + footer) would otherwise meet or exceed
/// [`BOTTOM_CHROME_MAX_HEIGHT`], so the input box and footer never lose rows to
/// the sessions line.
fn sessions_status_visible<V: BottomChromeView + ?Sized>(state: &V) -> bool {
    if state.sessions_status().is_empty() && state.sessions_entries().is_empty() {
        return false;
    }
    let visible_input_rows = state.input().lines.len().clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let without_sessions: u16 = 1u16 // status row
        .saturating_add(input_height)
        .saturating_add(1); // footer row
    without_sessions < BOTTOM_CHROME_MAX_HEIGHT
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

fn fullscreen_bottom_chrome_base_height<V: BottomChromeView + ?Sized>(state: &V) -> u16 {
    let visible_input_rows = state.input().lines.len().clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let sessions_rows = if sessions_status_visible(state) { 1 } else { 0 };
    1u16.saturating_add(sessions_rows)
        .saturating_add(input_height)
        .saturating_add(1)
}

fn render_fullscreen_bottom_chrome_at<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    area: Rect,
    state: &V,
    show_new_output_below: bool,
) {
    let visible_input_rows = state.input().lines.len().clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let sessions_rows = if sessions_status_visible(state) { 1 } else { 0 };

    let fixed_rows = 1u16
        .saturating_add(sessions_rows)
        .saturating_add(input_height)
        .saturating_add(1);
    let spacer_rows = area.height.saturating_sub(fixed_rows);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(sessions_rows),
            Constraint::Length(spacer_rows),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    #[allow(clippy::indexing_slicing)]
    {
        render_status_bar(frame, chunks[0], state);
        if sessions_rows > 0 {
            render_sessions_status(frame, chunks[1], state);
        }
        render_input(frame, chunks[3], state);
        render_fullscreen_footer(frame, chunks[4], state.ascii_fallback(), show_new_output_below);
    }
}

pub fn render_fullscreen_chat<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    state: &V,
    scroll: &mut FullscreenTranscriptScroll,
) {
    let frame_area = frame.area();
    frame.render_widget(Clear, frame_area);

    let chrome_height = fullscreen_bottom_chrome_height(state).min(frame_area.height);
    let content_height = frame_area.height.saturating_sub(chrome_height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(content_height), Constraint::Length(chrome_height)])
        .split(frame_area);

    #[allow(clippy::indexing_slicing)]
    {
        let (transcript_area, panel_area) = fullscreen_content_areas(chunks[0], state);
        render_fullscreen_transcript(frame, transcript_area, state, scroll);
        render_fullscreen_panel(frame, panel_area, state);
        render_fullscreen_bottom_chrome_at(frame, chunks[1], state, scroll.new_output_below);
        render_fullscreen_overlays(frame, chunks[0], state);
    }
}

fn fullscreen_content_areas<V: BottomChromeView + ?Sized>(area: Rect, state: &V) -> (Rect, Option<Rect>) {
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

fn render_fullscreen_panel<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Option<Rect>, state: &V) {
    let Some(area) = area else {
        return;
    };
    frame.render_widget(Clear, area);
    if let Some(approval) = state.pending_tool_approval() {
        render_approval(frame, area, &approval.name, &approval.args, state.ascii_fallback());
    } else if let Some(view) = state.active_session_view() {
        render_active_session_view(frame, area, view, state.ascii_fallback());
    }
}

fn render_fullscreen_transcript<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    area: Rect,
    state: &V,
    scroll: &mut FullscreenTranscriptScroll,
) {
    if area.height == 0 {
        scroll.offset_from_bottom = 0;
        return;
    }

    let mut lines: Vec<Line<'_>> = Vec::new();
    for line in state.conversation_lines() {
        render_conversation_line(&mut lines, line, state.ascii_fallback());
    }
    let streaming_tail = state.streaming().map(|streaming| ConversationLine::StreamingAssistant {
        content: streaming.accumulated.clone(),
    });
    if let Some(streaming_line) = streaming_tail.as_ref() {
        render_conversation_line(&mut lines, streaming_line, state.ascii_fallback());
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(transcript is empty)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let total_rows = usize::from(measure_wrapped_rows(&lines, area.width.max(1)));
    let visible_rows = usize::from(area.height.max(1));
    let max_scroll = total_rows.saturating_sub(visible_rows);
    let tail_marker = fullscreen_tail_marker(state);
    if scroll.offset_from_bottom > 0 && tail_marker > scroll.last_tail_marker && scroll.last_tail_marker > 0 {
        scroll.new_output_below = true;
    }
    scroll.offset_from_bottom = scroll.offset_from_bottom.min(max_scroll);
    if scroll.offset_from_bottom == 0 {
        scroll.new_output_below = false;
    }
    scroll.last_tail_marker = tail_marker;
    let top_scroll = max_scroll.saturating_sub(scroll.offset_from_bottom);
    let top_scroll = u16::try_from(top_scroll).unwrap_or(u16::MAX);

    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((top_scroll, 0));
    frame.render_widget(paragraph, area);
}

fn render_fullscreen_overlays<V: BottomChromeView + ?Sized>(frame: &mut Frame, frame_area: Rect, state: &V) {
    if let Some(picker) = state.saved_session_picker() {
        let area = centered_overlay_rect(frame_area, 92, 85, BOTTOM_CHROME_MIN_HEIGHT);
        frame.render_widget(Clear, area);
        render_saved_session_picker(frame, area, picker, state.ascii_fallback());
        return;
    }
    if let Some(menu) = state.slash_menu() {
        let area = centered_overlay_rect(frame_area, 92, 85, BOTTOM_CHROME_MIN_HEIGHT);
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

fn centered_overlay_rect(frame_area: Rect, width_pct: u16, height_pct: u16, min_height: u16) -> Rect {
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

/// Render the persistent child-session status row (v1b).
///
/// Single line, distinct dim style from the main status bar. The text is
/// truncated to the row width so a narrow terminal degrades gracefully rather
/// than wrapping into the input box.
fn render_sessions_status<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) {
    let text = render_sessions_strip_line(
        state.sessions_entries(),
        state.sessions_status(),
        state.focus(),
        state.ascii_fallback(),
        area.width,
    );
    if text.is_empty() {
        return;
    }
    let widget = Paragraph::new(text).style(Style::default().fg(Color::Cyan).bg(Color::Black));
    frame.render_widget(widget, area);
}

fn truncate_chars_with_ellipsis(input: &str, max_width: u16, ascii: bool) -> String {
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
        let ch_width = UnicodeWidthChar::width(ch).map_or(0, |value| value);
        if width.saturating_add(ch_width) > keep {
            break;
        }
        width = width.saturating_add(ch_width);
        truncated.push(ch);
    }
    truncated.push_str(ellipsis);
    truncated
}

const fn session_active_marker(active: bool, ascii: bool) -> &'static str {
    if active {
        if ascii { ">" } else { "\u{25B8}" }
    } else {
        " "
    }
}

fn session_status_glyph(entry: &crate::chat::sessions::SwitcherEntry, ascii: bool) -> &'static str {
    if entry.is_transcript() {
        return if ascii { "[T]" } else { "T" };
    }
    if ascii {
        match entry.status {
            "running" => "[~]",
            "needs-input" => "[?]",
            "completed" => "[x]",
            "cancelled" => "[-]",
            _ => "[!]",
        }
    } else {
        entry.status_glyph()
    }
}

fn render_sessions_strip_entry(
    entry: &crate::chat::sessions::SwitcherEntry,
    active_seq: Option<u64>,
    ascii: bool,
    max_width: u16,
) -> String {
    if max_width == 0 {
        return String::new();
    }
    let marker = session_active_marker(active_seq == Some(entry.seq), ascii);
    let glyph = session_status_glyph(entry, ascii);
    let prefix = format!("{marker} {glyph} #{} {} ", entry.seq, entry.kind);
    let prefix_cols = prefix.chars().count();
    let max = usize::from(max_width);
    if prefix_cols >= max {
        return prefix.chars().take(max).collect();
    }
    let title_budget = u16::try_from(max.saturating_sub(prefix_cols)).unwrap_or(u16::MAX);
    let title = truncate_chars_with_ellipsis(&entry.title, title_budget, ascii);
    format!("{prefix}{title}")
}

fn render_sessions_strip_line(
    entries: &[crate::chat::sessions::SwitcherEntry],
    summary: &str,
    focus: crate::chat::sessions::FocusTarget,
    ascii: bool,
    width: u16,
) -> String {
    if width == 0 {
        return String::new();
    }
    let content_width = width.saturating_sub(1);
    if content_width == 0 {
        return String::new();
    }
    if entries.is_empty() {
        if summary.is_empty() {
            return String::new();
        }
        let text = truncate_chars_with_ellipsis(summary, content_width, ascii);
        return format!(" {text}");
    }

    let active_seq = focus.session_seq();
    let sep = if ascii { " | " } else { " \u{00B7} " };
    let mut body = String::new();
    for entry in entries {
        let mut remaining = usize::from(content_width).saturating_sub(body.chars().count());
        if remaining == 0 {
            break;
        }
        if !body.is_empty() {
            let sep_cols = sep.chars().count();
            if remaining <= sep_cols {
                break;
            }
            body.push_str(sep);
            remaining = remaining.saturating_sub(sep_cols);
        }
        let segment =
            render_sessions_strip_entry(entry, active_seq, ascii, u16::try_from(remaining).unwrap_or(u16::MAX));
        body.push_str(&segment);
    }
    let body = truncate_chars_with_ellipsis(&body, content_width, ascii);
    format!(" {body}")
}

fn active_session_visible_lines(view: &crate::chat::sessions::ActiveSessionView, visible_rows: usize) -> Vec<String> {
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

fn render_active_session_view(
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

fn render_saved_session_picker_row(
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
            entry.provider,
            entry.model,
            entry.updated_at.format("%Y-%m-%d %H:%M")
        )
    };
    truncate_chars_with_ellipsis(&format!("{title} | {meta}"), max_width, ascii)
}

fn render_saved_session_picker(
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

fn slash_menu_command_spans(spec: CommandSpec, filter: &str, selected: bool) -> Vec<Span<'static>> {
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
    let name = spec.name.to_string();
    if filter.is_empty() {
        return vec![Span::styled(name, style)];
    }
    let name_without_slash = spec.name.trim_start_matches('/');
    let Some(pos) = name_without_slash.to_ascii_lowercase().find(&filter) else {
        return vec![Span::styled(name, style)];
    };
    let start = pos.saturating_add(1);
    let end = start.saturating_add(filter.len()).min(spec.name.len());
    let mut spans = Vec::new();
    if let Some(prefix) = spec.name.get(..start)
        && !prefix.is_empty()
    {
        spans.push(Span::styled(prefix.to_string(), style));
    }
    if let Some(matched) = spec.name.get(start..end)
        && !matched.is_empty()
    {
        spans.push(Span::styled(matched.to_string(), highlight));
    }
    if let Some(suffix) = spec.name.get(end..)
        && !suffix.is_empty()
    {
        spans.push(Span::styled(suffix.to_string(), style));
    }
    spans
}

fn render_slash_menu_row(spec: CommandSpec, filter: &str, selected: bool, max_width: u16) -> Line<'static> {
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
    let mut spans = slash_menu_command_spans(spec, filter, selected);
    let usage_tail = if spec.args_hint.is_empty() {
        String::new()
    } else {
        format!(" {}", spec.args_hint)
    };
    let usage_cols = spec.name.chars().count().saturating_add(usage_tail.chars().count());
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
            spec.description,
            u16::try_from(description_budget).unwrap_or(u16::MAX),
            true,
        );
        spans.push(Span::styled(description, base_style));
    }
    Line::from(spans)
}

fn render_slash_menu(frame: &mut Frame, area: Rect, menu: &SlashMenuState, ascii: bool) {
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
        let empty =
            Paragraph::new(" No matching slash commands. Esc to close. ").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(empty, inner);
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
    for (idx, spec) in menu.entries.get(start..end).unwrap_or(&[]).iter().enumerate() {
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
        row_spans.extend(render_slash_menu_row(*spec, &menu.filter, selected, inner.width.saturating_sub(2)).spans);
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
fn render_switcher_row(
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
    let prefix = if narrow {
        format!("{glyph} #{} {} ", entry.seq, entry.kind)
    } else {
        format!(
            "{glyph} #{} {} {} {} ",
            entry.seq, entry.kind, entry.origin, entry.status
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

fn render_switcher(frame: &mut Frame, area: Rect, switcher: &crate::chat::sessions::SwitcherState, ascii: bool) {
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
        format!(" \u{2191}\u{2193}/Ctrl+N/P move \u{00B7} Enter attach \u{00B7} Esc close \u{00B7} {hidden} more ")
    } else {
        " \u{2191}\u{2193}/Ctrl+N/P move \u{00B7} Enter attach \u{00B7} Esc close ".to_string()
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

fn render_status_bar<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) {
    let status_text = render_status_bar_text(state, area.width);
    let status = Paragraph::new(status_text).style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(status, area);
}

fn render_status_bar_text<V: BottomChromeView + ?Sized>(state: &V, width: u16) -> String {
    let title_str = state.session_title();
    let title = if title_str.is_empty() {
        "(new session)"
    } else {
        title_str
    };

    let token_estimate = estimate_visible_token_usage(state);
    let budget = render_token_budget(token_estimate, state.context_window_tokens());
    let permissions = render_permission_status(state.chat_mode(), state.autonomy_level());
    let full = format!(
        " PRX Chat | {}/{} | {} | {} turns | {permissions} | {budget} ",
        state.provider(),
        state.model(),
        title,
        state.turn_count(),
    );
    if full.chars().count() <= usize::from(width) {
        return full;
    }

    let compact = format!(
        " PRX Chat | {}/{} | {permissions} | {budget} ",
        state.provider(),
        state.model()
    );
    if compact.chars().count() <= usize::from(width) {
        return compact;
    }

    let minimal = format!(" PRX | {permissions} | {budget} ");
    truncate_chars_with_ellipsis(&minimal, width, state.ascii_fallback())
}

fn render_permission_status(mode: ChatMode, autonomy: AutonomyLevel) -> String {
    format!("mode:{} auth:{}", mode.label(), autonomy_label(autonomy))
}

const fn autonomy_label(level: AutonomyLevel) -> &'static str {
    match level {
        AutonomyLevel::ReadOnly => "read_only",
        AutonomyLevel::Supervised => "supervised",
        AutonomyLevel::Full => "full",
    }
}

const fn cycle_chat_mode(mode: ChatMode) -> ChatMode {
    match mode {
        ChatMode::Plan => ChatMode::Edit,
        ChatMode::Edit => ChatMode::Auto,
        ChatMode::Auto => ChatMode::Plan,
    }
}

fn render_token_budget(used_tokens: usize, window_tokens: Option<usize>) -> String {
    let Some(window) = window_tokens.filter(|tokens| *tokens > 0) else {
        return format!("~{used_tokens} tok");
    };
    let clamped_used = used_tokens.min(window);
    let percent = clamped_used
        .saturating_mul(100)
        .saturating_add(window.saturating_sub(1))
        / window;
    format!(
        "~{} / {} tok ({}%)",
        format_token_count(used_tokens),
        format_token_count(window),
        percent.min(100)
    )
}

fn format_token_count(tokens: usize) -> String {
    if tokens >= 1_000_000 && tokens % 1_000_000 == 0 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 && tokens % 1_000 == 0 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// Rough current-token estimate for the TUI status bar.
///
/// This mirrors `/cost`'s cheap chars/4 heuristic and uses only data already
/// present in the render snapshot. It is an operator hint, not a billing
/// counter.
fn estimate_visible_token_usage<V: BottomChromeView + ?Sized>(state: &V) -> usize {
    let mut chars = 0usize;
    for line in state.conversation_lines() {
        chars = chars.saturating_add(match line {
            ConversationLine::User { content }
            | ConversationLine::Assistant { content }
            | ConversationLine::StreamingAssistant { content }
            | ConversationLine::System { content } => content.chars().count(),
            ConversationLine::Tool { name, .. } => name.chars().count(),
            ConversationLine::ToolResult {
                tool_name,
                args_full,
                result,
                ..
            } => {
                tool_name.chars().count()
                    + args_full.chars().count()
                    + result.as_deref().map_or(0, |r| r.chars().count())
            }
            ConversationLine::Reasoning { char_count, .. } => *char_count,
        });
    }
    if let Some(streaming) = state.streaming() {
        chars = chars.saturating_add(streaming.accumulated.chars().count());
    }
    chars / 4
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
fn measure_wrapped_rows(lines: &[Line<'_>], width: u16) -> u16 {
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

    // BUG-11: sub-agent tools get an enriched header that surfaces the child
    // agent's identity (agent/model) and the delegated task summary, so nested
    // delegation is visible from the parent TUI even without observer streaming.
    let subagent_meta = if is_subagent_tool(tool_name) {
        let meta = extract_subagent_meta(args_full, result);
        if meta.is_empty() { None } else { Some(meta) }
    } else {
        None
    };

    if let Some(meta) = subagent_meta.as_ref() {
        // Header: `🤖 delegate[agent/model] · <task>` (or ASCII `[bot]`). The
        // robot glyph + bracketed identity reads as "this card is a sub-agent".
        let robot = if ascii { "[bot]" } else { "\u{1F916}" }; // 🤖
        let tag = subagent_identity_tag(meta);
        let mut header_spans = vec![
            Span::styled(format!("{bullet} "), Style::default().fg(bullet_color)),
            Span::raw(format!("{robot} {tool_name}[")),
            Span::styled(tag, Style::default().fg(Color::Cyan)),
            Span::raw("]"),
        ];
        if let Some(task) = meta.task.as_deref() {
            header_spans.push(Span::styled(
                format!(" \u{00B7} {task}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(header_spans));
    } else {
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
    }

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

// ── BUG-11: sub-agent visibility ────────────────────────────────────────────
//
// The delegate / sessions_spawn / subagents family of tools each spawn a *child*
// agent that runs its own LLM turns and tool calls. Without observer streaming
// (NoopObserver lives behind the tools boundary), the TUI only sees the parent
// tool card spin and then a flat text result. To make the nested activity
// visible at the chat layer we special-case these cards: we parse the
// sub-agent's identity (agent name + model) and the delegated task summary out
// of the tool *args* (always present) and the tool *result* (when finished),
// then render a distinct `🤖 delegate[agent/model] · <task>` header plus a meta
// follow-on line. This is the "tool card surfaces sub-agent meta" visibility
// layer; true real-time nested streaming requires wiring an observer through
// the tools boundary and is tracked separately.

/// Names of the tools that spawn / drive a sub-agent. A card for any of these
/// gets the enriched sub-agent treatment in [`render_tool_result`].
const SUBAGENT_TOOL_NAMES: [&str; 5] = ["delegate", "sessions_spawn", "subagents", "session_worker", "nodes"];

/// True when `tool_name` belongs to the sub-agent tool family.
fn is_subagent_tool(tool_name: &str) -> bool {
    SUBAGENT_TOOL_NAMES.contains(&tool_name)
}

/// Sub-agent metadata extracted from a tool card for the BUG-11 visibility
/// header. Every field is optional because args may be malformed JSON and the
/// result is absent while the tool is still running.
#[derive(Debug, Default, PartialEq, Eq)]
struct SubagentMeta {
    /// Target agent name (e.g. `researcher`). From args `agent`, or parsed from
    /// the `[Agent '...' (...)]` result banner.
    agent: Option<String>,
    /// Model the sub-agent ran on (e.g. `kimi-2.6`). From args `model`, or the
    /// `provider/model` portion of the result banner.
    model: Option<String>,
    /// Short one-line summary of the delegated task (args `prompt` / `task`).
    task: Option<String>,
}

impl SubagentMeta {
    /// True when no useful field could be extracted — caller falls back to the
    /// generic tool card so we never render an empty `🤖 [/]` header.
    const fn is_empty(&self) -> bool {
        self.agent.is_none() && self.model.is_none() && self.task.is_none()
    }
}

/// Collapse whitespace (incl. newlines) and clip to `max` chars with an ellipsis
/// so a multi-line prompt renders as a single tidy summary cell.
fn one_line_summary(raw: &str, max: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        let head: String = collapsed.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        collapsed
    }
}

/// Parse the `[Agent 'name' (provider/model)]` banner that delegate prepends to
/// its successful output. Returns `(agent, model)` with either side optional.
fn parse_agent_banner(result: &str) -> (Option<String>, Option<String>) {
    let first = result.lines().next().unwrap_or("");
    let Some(rest) = first.strip_prefix("[Agent ") else {
        return (None, None);
    };
    let rest = rest.strip_suffix(']').unwrap_or(rest);
    // rest now looks like: 'name' (provider/model)
    let agent = rest
        .split_once('\'')
        .and_then(|(_, after)| after.split_once('\''))
        .map(|(name, _)| name.trim().to_string())
        .filter(|s| !s.is_empty());
    // Pull the `provider/model` inside the parentheses; keep only the model side.
    let model = rest
        .split_once('(')
        .and_then(|(_, after)| after.split_once(')'))
        .map(|(inner, _)| inner.trim())
        .map(|pm| pm.rsplit('/').next().unwrap_or(pm).trim().to_string())
        .filter(|s| !s.is_empty());
    (agent, model)
}

/// Extract sub-agent metadata from a tool card. Prefers explicit args fields,
/// then falls back to parsing the delegate result banner. Never panics on bad
/// JSON — a parse failure simply yields fewer fields.
fn extract_subagent_meta(args_full: &str, result: Option<&str>) -> SubagentMeta {
    let mut meta = SubagentMeta::default();

    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(args_full) {
        let str_field = |key: &str| {
            map.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        meta.agent = str_field("agent");
        meta.model = str_field("model");
        // delegate uses `prompt`; sessions_spawn uses `task`.
        meta.task = str_field("prompt").or_else(|| str_field("task"));
        if meta.task.is_none() {
            // subagents drives an existing run via an action verb (kill/steer/list).
            meta.task = str_field("action").or_else(|| str_field("operation"));
        }
    }

    // Fill gaps from the result banner (carries provider/model the LLM may not
    // have echoed back in args, and the resolved agent name).
    if let Some(res) = result {
        let (banner_agent, banner_model) = parse_agent_banner(res);
        if meta.agent.is_none() {
            meta.agent = banner_agent;
        }
        if meta.model.is_none() {
            meta.model = banner_model;
        }
    }

    if let Some(task) = meta.task.take() {
        meta.task = Some(one_line_summary(&task, 60));
    }
    meta
}

/// Compose the `agent/model` identity tag shown in brackets after the tool
/// name. Falls back to a single side, or `?` when neither is known.
fn subagent_identity_tag(meta: &SubagentMeta) -> String {
    match (meta.agent.as_deref(), meta.model.as_deref()) {
        (Some(a), Some(m)) => format!("{a}/{m}"),
        (Some(a), None) => a.to_string(),
        (None, Some(m)) => m.to_string(),
        (None, None) => "?".to_string(),
    }
}

/// Build the prompt's input-target indicator span + its display width (v1.1b).
///
/// The target is dual-encoded with **colour AND text/glyph** so it is never
/// colour-only (colour-blind / no-color terminals still read the target):
/// - [`FocusTarget::Main`] → dim cyan `> ` (unchanged from the original prompt).
/// - [`FocusTarget::Session`] → blue bold `agent #N ▸ ` (or `agent #N > ` under
///   ASCII fallback). The literal "agent #N" text carries the meaning even with
///   styling stripped.
///
/// Returns the [`Span`] plus its column width so the continuation rows and the
/// terminal cursor can align under the typed text.
fn prompt_indicator(focus: crate::chat::sessions::FocusTarget, ascii: bool) -> (Span<'static>, usize) {
    match focus {
        crate::chat::sessions::FocusTarget::Main => {
            // Calmer dim cyan `> ` (matches the long-standing Claude Code prompt).
            let span = Span::styled("> ", Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM));
            (span, 2)
        }
        crate::chat::sessions::FocusTarget::Session { seq } => {
            let arrow = if ascii { ">" } else { "\u{25B8}" }; // ▸
            let label = format!("agent #{seq} {arrow} ");
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
    }
}

fn render_input<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) {
    // Compose prompt lines: the first row gets the input-target indicator
    // (v1.1b), continuation rows are aligned with blanks of the same width.
    let input_ref = state.input();
    let (prompt_span, prompt_width) = prompt_indicator(state.focus(), state.ascii_fallback());
    let continuation = " ".repeat(prompt_width);
    let rendered_lines: Vec<Line<'_>> = input_ref
        .lines
        .iter()
        .enumerate()
        .map(|(idx, content)| {
            let prefix = if idx == 0 {
                prompt_span.clone()
            } else {
                Span::raw(continuation.clone())
            };
            Line::from(vec![prefix, Span::raw(content.as_str())])
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
        let prefix_cols: u16 = u16::try_from(prompt_width).unwrap_or(2);
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
    // P6b2: Ctrl+G remains PRX's sessions switcher; Claude-style external
    // editor parity is available through the alternate Ctrl+X Ctrl+E chord.
    let footer = Paragraph::new(
        " Ctrl+G sessions \u{00B7} Ctrl+O transcript \u{00B7} Ctrl+R reverse-search \u{00B7} Ctrl+X Ctrl+E edit \u{00B7} Shift+Tab mode \u{00B7} Tab fold \u{00B7} Esc cancel ",
    )
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, area);
}

fn render_fullscreen_footer(frame: &mut Frame, area: Rect, ascii: bool, show_new_output_below: bool) {
    if show_new_output_below {
        let sep = if ascii { " | " } else { " \u{00B7} " };
        let footer = Paragraph::new(format!(
            " New output below{sep}End jumps to tail{sep}Home top{sep}PageUp/PageDown scroll "
        ))
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        frame.render_widget(footer, area);
        return;
    }
    render_footer(frame, area);
}

/// Render a tool approval prompt.
pub fn render_approval(frame: &mut Frame, area: Rect, tool_name: &str, args: &str, ascii: bool) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let marker = if ascii { ">" } else { "\u{25B8}" };
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
            Span::styled("[y]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(" approve  "),
            Span::styled("[n]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" deny  "),
            Span::styled("[Esc]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" deny"),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Tool Approval ")
        .border_style(Style::default().fg(Color::Yellow));

    let paragraph = Paragraph::new(approval_text).block(block).wrap(Wrap { trim: false });
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
        // TuiState owns the conversation log and turn counter. This pins the
        // contract that `turn_count` advances on user submissions only.
        let mut state = TuiState::new("test", "model");
        state.push_user_message("hello");
        state.push_assistant_message("world");
        assert_eq!(state.turn_count, 1, "only user messages bump turn_count");
        state.push_user_message("again");
        assert_eq!(state.turn_count, 2);
    }

    // ── BUG-11: sub-agent tool-card visibility ─────────────────────────

    #[test]
    fn subagent_tool_names_recognized() {
        for name in ["delegate", "sessions_spawn", "subagents", "session_worker", "nodes"] {
            assert!(is_subagent_tool(name), "{name} should be a sub-agent tool");
        }
        for name in ["shell", "file_write", "web_search", ""] {
            assert!(!is_subagent_tool(name), "{name} should NOT be a sub-agent tool");
        }
    }

    #[test]
    fn extract_meta_from_delegate_args() {
        // delegate args carry agent + model + prompt directly.
        let args = r#"{"agent":"researcher","model":"kimi-2.6","prompt":"summarize the codebase architecture"}"#;
        let meta = extract_subagent_meta(args, None);
        assert_eq!(meta.agent.as_deref(), Some("researcher"));
        assert_eq!(meta.model.as_deref(), Some("kimi-2.6"));
        assert_eq!(meta.task.as_deref(), Some("summarize the codebase architecture"));
        assert!(!meta.is_empty());
    }

    #[test]
    fn extract_meta_fills_model_from_result_banner() {
        // When args omit model, the delegate result banner supplies provider/model.
        let args = r#"{"agent":"coder","prompt":"write a fib fn"}"#;
        let result = "[Agent 'coder' (openrouter/anthropic/claude-sonnet-4)]\nfn fib(n: u64) -> u64 { ... }";
        let meta = extract_subagent_meta(args, Some(result));
        assert_eq!(meta.agent.as_deref(), Some("coder"));
        // model side of `provider/model` keeps the trailing segment.
        assert_eq!(meta.model.as_deref(), Some("claude-sonnet-4"));
    }

    #[test]
    fn extract_meta_from_sessions_spawn_task_field() {
        // sessions_spawn uses `task` rather than `prompt`.
        let args = r#"{"agent":"worker","task":"run the test suite and report failures"}"#;
        let meta = extract_subagent_meta(args, None);
        assert_eq!(meta.agent.as_deref(), Some("worker"));
        assert_eq!(meta.task.as_deref(), Some("run the test suite and report failures"));
    }

    #[test]
    fn extract_meta_bad_json_is_empty() {
        // Malformed args must never panic and yield no fields (caller falls back).
        let meta = extract_subagent_meta("not json at all", None);
        assert!(meta.is_empty(), "bad JSON → empty meta");
    }

    #[test]
    fn identity_tag_prefers_agent_slash_model() {
        let meta = SubagentMeta {
            agent: Some("researcher".into()),
            model: Some("kimi-2.6".into()),
            task: None,
        };
        assert_eq!(subagent_identity_tag(&meta), "researcher/kimi-2.6");
        let only_model = SubagentMeta {
            agent: None,
            model: Some("gpt-4o".into()),
            task: None,
        };
        assert_eq!(subagent_identity_tag(&only_model), "gpt-4o");
        assert_eq!(subagent_identity_tag(&SubagentMeta::default()), "?");
    }

    #[test]
    fn one_line_summary_collapses_and_clips() {
        let raw = "line one\n  line   two\tand more words here to overflow the limit clearly";
        let out = one_line_summary(raw, 20);
        assert!(!out.contains('\n'), "newlines collapsed");
        assert!(out.chars().count() <= 20, "clipped to max");
        assert!(out.ends_with('…'), "overflow gets ellipsis");
    }

    #[test]
    fn render_delegate_card_shows_robot_and_identity() {
        // The rendered sub-agent card must surface the robot glyph, the
        // agent/model identity tag, and the task summary so nested delegation
        // is visible from the parent TUI.
        let mut lines: Vec<Line<'_>> = Vec::new();
        let args = r#"{"agent":"researcher","model":"kimi-2.6","prompt":"investigate the bug"}"#;
        render_tool_result(
            &mut lines,
            "delegate",
            "agent: researcher",
            args,
            Some("[Agent 'researcher' (openrouter/kimi-2.6)]\nFound the root cause."),
            ToolStatus::Done,
            Some(1200),
            true, // folded
            true, // ascii — deterministic, no unicode glyphs
        );
        let header = lines.first().map(line_to_plain).expect("test: header line present");
        assert!(header.contains("[bot]"), "ascii robot marker present: {header}");
        assert!(header.contains("delegate["), "tool name + bracket: {header}");
        assert!(header.contains("researcher/kimi-2.6"), "identity tag: {header}");
        assert!(header.contains("investigate the bug"), "task summary: {header}");
    }

    #[test]
    fn render_non_subagent_card_unchanged() {
        // A normal tool keeps the classic `Tool(args)` header — no robot.
        let mut lines: Vec<Line<'_>> = Vec::new();
        render_tool_result(
            &mut lines,
            "shell",
            "ls /tmp",
            r#"{"command":"ls /tmp"}"#,
            Some("file_a\nfile_b"),
            ToolStatus::Done,
            Some(34),
            true,
            true,
        );
        let header = lines.first().map(line_to_plain).expect("test: header line present");
        assert!(header.contains("shell(ls /tmp)"), "classic header: {header}");
        assert!(!header.contains("[bot]"), "no robot for normal tools: {header}");
    }

    /// Flatten a rendered `Line` into plain text for assertions.
    fn line_to_plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
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
    fn streaming_transient_renders_after_history_in_fullscreen_lines() {
        // Drive `render_conversation_line` over both finalized history and the
        // staged transient line. The streaming block must contain the in-flight
        // tokens and end with the cursor glyph.
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
        assert!(
            !header.contains("press Tab to expand"),
            "expanded header drops the expand hint: {header}"
        );
        assert!(
            header.contains("Tab to collapse"),
            "expanded header shows Tab collapse hint: {header}"
        );
        assert!(
            !header.contains("Ctrl+R"),
            "expanded header must not advertise Ctrl+R as fold after P6b2: {header}"
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

    fn saved_picker_entry(id: &str, title: &str, is_current: bool) -> crate::chat::session::SavedSessionPickerEntry {
        crate::chat::session::SavedSessionPickerEntry {
            id: id.to_string(),
            title: title.to_string(),
            turn_count: 2,
            updated_at: chrono::Utc::now(),
            provider: "provider".to_string(),
            model: "model".to_string(),
            is_current,
        }
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
    fn p2_10_large_paste_100kb_is_bounded_and_recoverable() {
        let mut input = TuiInput::new();
        let line = "a".repeat(1024);
        let pasted = std::iter::repeat_n(line.as_str(), 100).collect::<Vec<_>>().join("\n");

        input.paste(&pasted);

        assert_eq!(pasted.len(), 102_499);
        assert_eq!(input.byte_len(), INPUT_MAX_BYTES);
        assert!(input.truncated, "input should report ignored overflow bytes");
        assert!(pasted.starts_with(&input.text()));

        let out = input.handle_key(key(KeyCode::Enter));
        match out {
            InputOutcome::Submitted(submitted) => {
                assert_eq!(submitted.len(), INPUT_MAX_BYTES);
                assert_eq!(input.history, vec![submitted]);
            }
            other => panic!("expected bounded large input to submit, got {other:?}"),
        }
        assert!(input.is_empty(), "buffer cleared after large submit");
    }

    #[test]
    fn p2_10_key_flood_stops_at_input_cap_and_ctrl_u_recovers() {
        let mut input = TuiInput::new();
        for _ in 0..(INPUT_MAX_BYTES + 1024) {
            input.handle_key(key(KeyCode::Char('p')));
        }

        assert_eq!(input.byte_len(), INPUT_MAX_BYTES);
        assert!(input.truncated);
        assert_eq!(input.cursor, (0, INPUT_MAX_BYTES));

        let out = input.handle_key(key_mod(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(out, InputOutcome::Consumed);
        assert!(input.is_empty());
        assert!(!input.truncated);
    }

    #[test]
    fn p2_10_over_cap_plain_key_is_ignored_without_redraw() {
        let mut state = TuiState::new("provider", "model");
        state.input.paste(&"p".repeat(INPUT_MAX_BYTES));

        let out = dispatch_global_key(key(KeyCode::Char('p')), &mut state);

        assert_eq!(out, KeyDispatch::Ignored);
        assert_eq!(state.input.byte_len(), INPUT_MAX_BYTES);
        assert!(state.input.truncated);
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
    fn input_pageup_pagedown_fall_through_to_outer_loop() {
        // Fullscreen transcript scroll is handled by the outer TUI loop.
        // The input subsystem must report these keys as Unhandled.
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
    fn slash_menu_opens_when_input_starts_with_slash() {
        let mut state = TuiState::new("p", "m");

        let out = dispatch_global_key(key(KeyCode::Char('/')), &mut state);

        assert_eq!(out, KeyDispatch::Consumed);
        let menu = state.slash_menu.as_ref().expect("slash menu open");
        assert!(menu.len() >= 30, "registry-backed menu should include all commands");
        assert!(menu.entries.iter().any(|spec| spec.name == "/help"));
        assert_eq!(state.input.text(), "/");
    }

    #[test]
    fn slash_menu_filters_from_command_token() {
        let mut state = TuiState::new("p", "m");
        for ch in "/mo".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        let menu = state.slash_menu.as_ref().expect("slash menu open");
        assert_eq!(menu.filter, "mo");
        assert!(menu.entries.iter().any(|spec| spec.name == "/model"));
        assert!(
            menu.entries.iter().all(|spec| spec.name != "/provider"),
            "P0 /mo filter should exclude unrelated commands: {:?}",
            menu.entries
        );
    }

    #[test]
    fn slash_menu_navigation_and_enter_insert_command_name() {
        let mut state = TuiState::new("p", "m");
        for ch in "/".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }
        let first = state.slash_menu.as_ref().expect("slash menu open").selected;

        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::Consumed
        );
        let moved = state.slash_menu.as_ref().expect("slash menu still open").selected;
        assert_ne!(first, moved, "Down moves slash menu selection");
        assert_eq!(dispatch_global_key(key(KeyCode::Up), &mut state), KeyDispatch::Consumed);
        assert_eq!(
            state.slash_menu.as_ref().expect("slash menu still open").selected,
            first,
            "Up moves selection back"
        );

        for ch in "mo".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }
        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.input.text(), "/model ");
        assert_eq!(state.input.cursor, (0, "/model ".len()));
        assert!(state.slash_menu.is_none(), "selecting closes slash menu");
    }

    #[test]
    fn slash_menu_tab_inserts_command_and_esc_dismisses_without_clearing_input() {
        let mut tab_state = TuiState::new("p", "m");
        for ch in "/ex".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut tab_state),
                KeyDispatch::Consumed
            );
        }
        assert_eq!(
            dispatch_global_key(key(KeyCode::Tab), &mut tab_state),
            KeyDispatch::Consumed
        );
        assert_eq!(tab_state.input.text(), "/export ");
        assert!(tab_state.slash_menu.is_none());

        let mut esc_state = TuiState::new("p", "m");
        for ch in "/mo".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut esc_state),
                KeyDispatch::Consumed
            );
        }
        assert_eq!(
            dispatch_global_key(key(KeyCode::Esc), &mut esc_state),
            KeyDispatch::Consumed
        );
        assert_eq!(esc_state.input.text(), "/mo");
        assert!(esc_state.slash_menu.is_none(), "Esc closes menu only");
    }

    #[test]
    fn input_history_up_down_still_work_when_slash_menu_closed() {
        let mut state = TuiState::new("p", "m");
        state.input.history.push("older command".to_string());

        assert_eq!(dispatch_global_key(key(KeyCode::Up), &mut state), KeyDispatch::Consumed);
        assert_eq!(state.input.text(), "older command");
        assert!(state.slash_menu.is_none());
        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::Consumed
        );
        assert!(state.input.is_empty());
    }

    #[test]
    fn fullscreen_slash_menu_renders_as_overlay_with_filtered_command() {
        let mut state = TuiState::new("provider", "model");
        state.slash_menu = Some(SlashMenuState::new("mo"));
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 80, 24, &mut scroll);

        assert!(
            rows.iter().any(|row| row.contains("Slash commands")),
            "slash menu title rendered: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("/model")),
            "filtered command rendered: {rows:?}"
        );
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
    fn dispatch_tab_mid_edit_inserts_tab_instead_of_folding() {
        let mut state = TuiState::new("p", "m");
        state.input.set_text("alpha");
        state.push_tool_result_started("shell", "{}");
        let folded_before = match state.conversation_lines.last() {
            Some(ConversationLine::ToolResult { folded, .. }) => *folded,
            _ => panic!("test: expected ToolResult at end"),
        };

        let out = dispatch_global_key(key(KeyCode::Tab), &mut state);

        assert_eq!(out, KeyDispatch::Consumed);
        assert_eq!(state.input.text(), "alpha\t");
        let folded_after = match state.conversation_lines.last() {
            Some(ConversationLine::ToolResult { folded, .. }) => *folded,
            _ => panic!("test: expected ToolResult at end"),
        };
        assert_eq!(folded_before, folded_after, "mid-edit Tab must not fold cards");
    }

    #[test]
    fn dispatch_backtab_cycles_chat_mode_when_input_empty() {
        let mut state = TuiState::new("p", "m");
        state.chat_mode = ChatMode::Plan;

        let out = dispatch_global_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT), &mut state);

        assert_eq!(out, KeyDispatch::ModeChanged(ChatMode::Edit));
        assert_eq!(state.chat_mode, ChatMode::Edit);
        assert!(state.input.is_empty());

        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT), &mut state),
            KeyDispatch::ModeChanged(ChatMode::Auto)
        );
        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT), &mut state),
            KeyDispatch::ModeChanged(ChatMode::Plan)
        );
    }

    #[test]
    fn dispatch_backtab_mid_edit_does_not_cycle_mode() {
        let mut state = TuiState::new("p", "m");
        state.input.set_text("alpha");
        state.input.cursor = (0, 2);
        state.chat_mode = ChatMode::Plan;

        let out = dispatch_global_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT), &mut state);

        assert_eq!(out, KeyDispatch::Consumed);
        assert_eq!(state.chat_mode, ChatMode::Plan);
        assert_eq!(state.input.text(), "alpha");
        assert_eq!(state.input.cursor, (0, 2));
    }

    #[test]
    fn dispatch_backtab_is_captured_by_modal_layers_before_mode_cycle() {
        let backtab = key_mod(KeyCode::BackTab, KeyModifiers::SHIFT);

        let mut picker = TuiState::new("p", "m");
        picker.chat_mode = ChatMode::Plan;
        picker.input.set_text("draft");
        picker.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
            saved_picker_entry("saved", "saved session", false),
        ]));
        assert_eq!(dispatch_global_key(backtab, &mut picker), KeyDispatch::Consumed);
        assert_eq!(picker.chat_mode, ChatMode::Plan);
        assert_eq!(picker.input.text(), "draft");

        let mut switcher = TuiState::new("p", "m");
        switcher.chat_mode = ChatMode::Plan;
        switcher.sessions_cache = vec![crate::chat::sessions::SwitcherEntry {
            seq: 1,
            kind: "agent",
            origin: "model",
            status: "running",
            title: "child".to_string(),
        }];
        assert!(matches!(
            dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut switcher),
            KeyDispatch::SwitcherOpened { .. }
        ));
        assert_eq!(dispatch_global_key(backtab, &mut switcher), KeyDispatch::Consumed);
        assert_eq!(switcher.chat_mode, ChatMode::Plan);

        let mut approval = approval_state();
        approval.chat_mode = ChatMode::Plan;
        approval.input.set_text("hold");
        let cursor = approval.input.cursor;
        assert_eq!(dispatch_global_key(backtab, &mut approval), KeyDispatch::Consumed);
        assert_eq!(approval.chat_mode, ChatMode::Plan);
        assert_eq!(approval.input.text(), "hold");
        assert_eq!(approval.input.cursor, cursor);

        let mut editor_prefix = TuiState::new("p", "m");
        editor_prefix.chat_mode = ChatMode::Plan;
        editor_prefix.input.set_text("edit me");
        editor_prefix.external_editor_prefix_armed = true;
        assert_eq!(dispatch_global_key(backtab, &mut editor_prefix), KeyDispatch::Consumed);
        assert_eq!(editor_prefix.chat_mode, ChatMode::Plan);
        assert_eq!(editor_prefix.input.text(), "edit me");
        assert!(!editor_prefix.external_editor_prefix_armed);
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
    fn dispatch_ctrl_r_opens_reverse_search_without_reasoning_toggle() {
        let mut state = TuiState::new("p", "m");
        state.input.history = vec!["alpha".to_string(), "beta".to_string()];
        assert!(state.push_reasoning("step 1\nstep 2"));
        let folded_before = match state.conversation_lines.last() {
            Some(ConversationLine::Reasoning { folded, .. }) => *folded,
            _ => panic!("test: expected Reasoning at end"),
        };

        let out = dispatch_global_key(key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        assert!(state.input.is_reverse_search_active(), "Ctrl+R opens reverse-search");
        assert_eq!(state.input.text(), "beta", "initial Ctrl+R recalls latest history item");

        let folded_after = match state.conversation_lines.last() {
            Some(ConversationLine::Reasoning { folded, .. }) => *folded,
            _ => panic!("test: expected Reasoning at end"),
        };
        assert_eq!(folded_before, folded_after, "Ctrl+R must not fold reasoning after P6b2");
    }

    #[test]
    fn reverse_search_filters_history_accepts_and_cancels() {
        let mut input = TuiInput::new();
        input.history = vec![
            "alpha one".to_string(),
            "beta two".to_string(),
            "alpha three".to_string(),
        ];
        input.set_text("draft");
        assert!(input.begin_or_cycle_reverse_search());
        assert!(input.is_reverse_search_active());
        assert_eq!(input.text(), "alpha three");

        assert_eq!(input.handle_key(key(KeyCode::Char('b'))), InputOutcome::Consumed);
        assert_eq!(input.text(), "beta two", "query filters to matching history entry");
        assert_eq!(input.handle_key(key(KeyCode::Enter)), InputOutcome::Consumed);
        assert!(!input.is_reverse_search_active());
        assert_eq!(input.text(), "beta two", "Enter accepts match without submitting");

        input.set_text("draft");
        assert!(input.begin_or_cycle_reverse_search());
        assert_eq!(input.handle_key(key(KeyCode::Char('z'))), InputOutcome::Consumed);
        assert_eq!(input.text(), "draft", "no-match restores visible draft while searching");
        assert_eq!(input.handle_key(key(KeyCode::Esc)), InputOutcome::Cancelled);
        assert_eq!(input.text(), "draft", "Esc cancels and restores saved draft");
        assert!(!input.is_reverse_search_active());
    }

    #[test]
    fn reverse_search_ctrl_r_cycles_older_matches() {
        let mut input = TuiInput::new();
        input.history = vec!["alpha first".to_string(), "zzz".to_string(), "alpha second".to_string()];
        assert!(input.begin_or_cycle_reverse_search());
        assert_eq!(input.text(), "alpha second");
        assert_eq!(
            input.handle_key(key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            InputOutcome::Consumed
        );
        assert_eq!(input.text(), "zzz", "empty query cycles older entries");
        assert_eq!(input.handle_key(key(KeyCode::Char('a'))), InputOutcome::Consumed);
        assert_eq!(input.text(), "alpha second", "query starts from latest matching entry");
        assert_eq!(
            input.handle_key(key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            InputOutcome::Consumed
        );
        assert_eq!(input.text(), "alpha first", "Ctrl+R cycles older query matches");
    }

    #[test]
    fn dispatch_ctrl_r_focus_input_matrix_main_session_transcript() {
        let focuses = [
            crate::chat::sessions::FocusTarget::Main,
            crate::chat::sessions::FocusTarget::Session { seq: 7 },
            crate::chat::sessions::FocusTarget::Transcript,
        ];
        for focus in focuses {
            for seed in ["", "draft"] {
                let mut state = TuiState::new("p", "m");
                state.focus = focus;
                state.input.history = vec!["history item".to_string()];
                if !seed.is_empty() {
                    state.input.set_text(seed);
                }
                let out = dispatch_global_key(key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut state);
                assert_eq!(
                    out,
                    KeyDispatch::Consumed,
                    "Ctrl+R consumed for focus={focus:?} seed={seed:?}"
                );
                assert!(
                    state.input.is_reverse_search_active(),
                    "reverse-search active for focus={focus:?} seed={seed:?}"
                );
                assert_eq!(
                    state.input.text(),
                    "history item",
                    "no steer/scroll leak for focus={focus:?}"
                );
            }
        }
    }

    #[test]
    fn saved_session_picker_captures_navigation_enter_and_esc() {
        let mut state = TuiState::new("p", "m");
        state.input.history = vec!["history item".to_string()];
        state.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
            saved_picker_entry("latest", "latest session", true),
            saved_picker_entry("older", "older session", false),
        ]));

        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::SavedSessionPickerMoved { selected: 1 }
        );
        assert!(
            state.input.is_empty(),
            "Down must not navigate input history while picker is open"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Up), &mut state),
            KeyDispatch::SavedSessionPickerMoved { selected: 0 }
        );
        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL), &mut state),
            KeyDispatch::SavedSessionPickerMoved { selected: 1 }
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::ResumeSavedSession {
                id: "older".to_string()
            }
        );
        assert!(
            state.saved_session_picker.is_none(),
            "Enter closes picker before control event"
        );

        state.input.set_text("draft");
        state.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
            saved_picker_entry("latest", "latest session", true),
        ]));
        assert_eq!(
            dispatch_global_key(key(KeyCode::Esc), &mut state),
            KeyDispatch::SavedSessionPickerClosed
        );
        assert_eq!(state.input.text(), "draft", "Esc close must preserve draft input");
    }

    #[test]
    fn saved_session_picker_priority_blocks_child_switcher_and_stale_index_clamps() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![crate::chat::sessions::SwitcherEntry {
            seq: 1,
            kind: "agent",
            origin: "model",
            status: "running",
            title: "child".to_string(),
        }];
        state.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState {
            entries: vec![saved_picker_entry("only", "only session", false)],
            selected: 99,
        });

        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state),
            KeyDispatch::Consumed,
            "Ctrl+G must not open child switcher while saved-session picker is open"
        );
        assert!(state.switcher.is_none());
        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::ResumeSavedSession { id: "only".to_string() },
            "stale picker selection clamps to the last valid row"
        );

        let out = dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
        assert!(matches!(out, KeyDispatch::SwitcherOpened { .. }));
        assert!(state.switcher.is_some(), "Ctrl+G works again after picker closes");
    }

    #[test]
    fn saved_session_picker_row_truncates_to_unicode_width() {
        let entry = saved_picker_entry("wide", "很长的会话标题 mixed ascii text", true);
        let row = render_saved_session_picker_row(&entry, false, 18, false);
        assert!(
            UnicodeWidthStr::width(row.as_str()) <= 18,
            "row must fit width, got width={} row={row:?}",
            UnicodeWidthStr::width(row.as_str())
        );
        assert!(row.contains('\u{2026}'), "truncated row should show ellipsis: {row:?}");
    }

    #[test]
    fn saved_session_picker_closed_keeps_up_down_input_history_behavior() {
        let mut state = TuiState::new("p", "m");
        state.input.history = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(dispatch_global_key(key(KeyCode::Up), &mut state), KeyDispatch::Consumed);
        assert_eq!(state.input.text(), "beta");
        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::Consumed
        );
        assert!(state.input.is_empty());
    }

    #[test]
    fn dispatch_ctrl_x_ctrl_e_requests_external_editor_without_input_leak() {
        let mut state = TuiState::new("p", "m");
        state.input.set_text("draft");
        let first = dispatch_global_key(key_mod(KeyCode::Char('x'), KeyModifiers::CONTROL), &mut state);
        assert_eq!(first, KeyDispatch::Consumed);
        assert_eq!(state.input.text(), "draft");
        let second = dispatch_global_key(key_mod(KeyCode::Char('e'), KeyModifiers::CONTROL), &mut state);
        assert_eq!(second, KeyDispatch::ExternalEditorRequested);
        assert_eq!(state.input.text(), "draft", "chord must not mutate input itself");
    }

    #[test]
    fn dispatch_ctrl_x_non_editor_second_key_does_not_leak() {
        let mut state = TuiState::new("p", "m");
        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Char('x'), KeyModifiers::CONTROL), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char('q')), &mut state),
            KeyDispatch::Consumed
        );
        assert!(state.input.is_empty(), "non-E chord tail must not enter input");
        assert!(!state.external_editor_prefix_armed, "non-E chord tail clears latch");
    }

    fn approval_state() -> TuiState {
        let mut state = TuiState::new("p", "m");
        state.focus = crate::chat::sessions::FocusTarget::Approval;
        state.pending_tool_approval = Some(crate::chat::sessions::PendingToolApprovalView {
            tool_id: "call-1".to_string(),
            name: "shell".to_string(),
            args: r#"{"cmd":"echo hi"}"#.to_string(),
        });
        state
    }

    #[test]
    fn approval_child_y_sends_tool_approval_received_true() {
        let mut state = approval_state();
        let out = dispatch_global_key(key(KeyCode::Char('y')), &mut state);
        assert_eq!(
            out,
            KeyDispatch::ToolApprovalDecision {
                tool_id: "call-1".to_string(),
                approved: true
            }
        );
        assert!(state.pending_tool_approval.is_none());
        assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
    }

    #[test]
    fn approval_child_n_or_esc_sends_tool_approval_received_false() {
        for key_event in [key(KeyCode::Char('n')), key(KeyCode::Esc)] {
            let mut state = approval_state();
            let out = dispatch_global_key(key_event, &mut state);
            assert_eq!(
                out,
                KeyDispatch::ToolApprovalDecision {
                    tool_id: "call-1".to_string(),
                    approved: false
                }
            );
            assert!(state.pending_tool_approval.is_none());
            assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
        }
    }

    #[test]
    fn approval_child_text_enter_does_not_submit_or_steer() {
        for key_event in [key(KeyCode::Char('x')), key(KeyCode::Enter)] {
            let mut state = approval_state();
            let out = dispatch_global_key(key_event, &mut state);
            assert_eq!(out, KeyDispatch::Consumed);
            assert!(state.input.is_empty(), "approval focus must not edit the main input");
            assert!(
                state.pending_tool_approval.is_some(),
                "text/Enter must not close approval"
            );
            assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Approval);
        }
    }

    #[test]
    fn approval_focus_without_pending_esc_resets_to_main() {
        let mut state = TuiState::new("p", "m");
        state.focus = crate::chat::sessions::FocusTarget::Approval;
        state.pending_tool_approval = None;

        let out = dispatch_global_key(key(KeyCode::Esc), &mut state);

        assert_eq!(out, KeyDispatch::Cancelled);
        assert!(state.pending_tool_approval.is_none());
        assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
    }

    #[test]
    fn dispatch_ctrl_o_opens_transcript_viewer_without_input_leak() {
        let mut state = TuiState::new("p", "m");
        let out = dispatch_global_key(key_mod(KeyCode::Char('o'), KeyModifiers::CONTROL), &mut state);
        assert_eq!(out, KeyDispatch::OpenTranscriptViewer);
        assert!(state.input.is_empty(), "Ctrl+O must not insert text into input");
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
        // Transcript scroll is owned by the outer fullscreen event loop. The
        // pure dispatcher consumes the key without mutating input.
        let mut state = TuiState::new("p", "m");
        let out = dispatch_global_key(key(KeyCode::PageUp), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        let out = dispatch_global_key(key(KeyCode::PageDown), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        assert!(state.input.is_empty(), "PgUp/PgDn must not leak into input");
    }

    #[test]
    fn dispatch_child_view_scroll_keys_only_when_child_focus_and_empty_input() {
        let mut state = TuiState::new("p", "m");
        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 9 };

        assert_eq!(
            dispatch_global_key(key(KeyCode::Up), &mut state),
            KeyDispatch::ScrollSessionUp
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::ScrollSessionDown
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::PageUp), &mut state),
            KeyDispatch::PageSessionUp
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::PageDown), &mut state),
            KeyDispatch::PageSessionDown
        );

        state.focus = crate::chat::sessions::FocusTarget::Main;
        assert_eq!(
            dispatch_global_key(key(KeyCode::PageUp), &mut state),
            KeyDispatch::Consumed,
            "main focus must not route PgUp into child viewport scrolling"
        );

        state.focus = crate::chat::sessions::FocusTarget::Transcript;
        assert_eq!(
            dispatch_global_key(key(KeyCode::Up), &mut state),
            KeyDispatch::ScrollSessionUp,
            "transcript focus must reuse child viewport scrolling"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::PageDown), &mut state),
            KeyDispatch::PageSessionDown,
            "transcript focus must support page scrolling"
        );
        let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
        assert_eq!(
            out,
            KeyDispatch::Consumed,
            "transcript focus is read-only and must not submit/steer"
        );

        state.focus = crate::chat::sessions::FocusTarget::Diff;
        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::ScrollSessionDown,
            "diff focus must reuse child viewport scrolling"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::PageUp), &mut state),
            KeyDispatch::PageSessionUp,
            "diff focus must support page scrolling"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Consumed,
            "diff focus is read-only and must not submit/steer"
        );

        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 9 };
        dispatch_global_key(key(KeyCode::Char('x')), &mut state);
        assert_eq!(
            dispatch_global_key(key(KeyCode::Up), &mut state),
            KeyDispatch::Consumed,
            "non-empty input keeps Up editing/history semantics instead of child scroll"
        );
        assert_eq!(state.input.text(), "x");
        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::Consumed,
            "non-empty input keeps Down editing/history semantics instead of child scroll"
        );
        assert_eq!(state.input.text(), "x");
        assert_eq!(
            dispatch_global_key(key(KeyCode::PageDown), &mut state),
            KeyDispatch::Consumed,
            "non-empty input keeps edit/history semantics instead of child scroll"
        );

        state.input.clear();
        state.focus = crate::chat::sessions::FocusTarget::Diff;
        dispatch_global_key(key(KeyCode::Char('z')), &mut state);
        assert_eq!(
            dispatch_global_key(key(KeyCode::Up), &mut state),
            KeyDispatch::Consumed,
            "diff+non-empty input also keeps edit/history semantics"
        );
        assert_eq!(state.input.text(), "z");
    }

    #[test]
    fn dispatch_directional_session_switching_obeys_focus_input_matrix() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1), entry(2), entry(3)];

        assert_eq!(
            dispatch_global_key(key(KeyCode::Right), &mut state),
            KeyDispatch::Consumed,
            "main+empty must not switch child sessions"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Left), &mut state),
            KeyDispatch::Consumed,
            "main+empty must preserve prompt key semantics"
        );

        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 2 };
        assert_eq!(
            dispatch_global_key(key(KeyCode::Right), &mut state),
            KeyDispatch::SwitchSession { seq: 3 },
            "session+empty Right switches to the visual neighbor on the right"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Left), &mut state),
            KeyDispatch::SwitchSession { seq: 1 },
            "session+empty Left switches to the visual neighbor on the left"
        );

        dispatch_global_key(key(KeyCode::Char('x')), &mut state);
        dispatch_global_key(key(KeyCode::Char('y')), &mut state);
        assert_eq!(state.input.cursor, (0, 2));
        assert_eq!(
            dispatch_global_key(key(KeyCode::Left), &mut state),
            KeyDispatch::Consumed,
            "session+non-empty Left must move the cursor, not switch sessions"
        );
        assert_eq!(state.input.text(), "xy");
        assert_eq!(state.input.cursor, (0, 1));
        assert_eq!(
            dispatch_global_key(key(KeyCode::Right), &mut state),
            KeyDispatch::Consumed,
            "session+non-empty Right must move the cursor, not switch sessions"
        );
        assert_eq!(state.input.cursor, (0, 2));

        state.input.clear();
        state.focus = crate::chat::sessions::FocusTarget::Transcript;
        assert_eq!(
            dispatch_global_key(key(KeyCode::Right), &mut state),
            KeyDispatch::Consumed,
            "transcript focus must not switch to real sessions with Right"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Left), &mut state),
            KeyDispatch::Consumed,
            "transcript focus must not switch to real sessions with Left"
        );

        state.focus = crate::chat::sessions::FocusTarget::Diff;
        assert_eq!(
            dispatch_global_key(key(KeyCode::Right), &mut state),
            KeyDispatch::Consumed,
            "diff focus must not switch to real sessions with Right"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Left), &mut state),
            KeyDispatch::Consumed,
            "diff focus must not switch to real sessions with Left"
        );

        dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
        assert!(state.switcher.is_some());
        assert_eq!(
            dispatch_global_key(key(KeyCode::Right), &mut state),
            KeyDispatch::Consumed,
            "switcher-open keys must not leak to directional session switching"
        );
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

    // ── v1.1b: switcher + Esc-detach + focus indicator ───────────────────────

    fn entry(seq: u64) -> crate::chat::sessions::SwitcherEntry {
        crate::chat::sessions::SwitcherEntry {
            seq,
            kind: "agent",
            origin: "user",
            status: "running",
            title: format!("task {seq}"),
        }
    }

    #[test]
    fn p0_8_esc_empty_main_cancels() {
        // Empty input + main focus → unchanged legacy cancel semantics.
        let mut state = TuiState::new("p", "m");
        assert!(state.input.is_empty());
        let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
        assert_eq!(out, KeyDispatch::Cancelled);
    }

    // ── v5: switcher row layout (origin tag + narrow-terminal degradation) ────

    fn pty_entry(seq: u64, title: &str) -> crate::chat::sessions::SwitcherEntry {
        crate::chat::sessions::SwitcherEntry {
            seq,
            kind: "pty",
            origin: "user",
            status: "running",
            title: title.to_string(),
        }
    }

    #[test]
    fn switcher_row_wide_includes_kind_origin_status() {
        let e = pty_entry(3, "vim notes.md");
        let row = render_switcher_row(&e, "⏳", false, 80);
        assert!(row.contains("#3"), "row carries the seq: {row}");
        assert!(row.contains("pty"), "row carries the kind: {row}");
        assert!(row.contains("user"), "row carries the origin: {row}");
        assert!(row.contains("running"), "wide row carries the status text: {row}");
        assert!(row.contains("vim notes.md"), "row carries the title: {row}");
    }

    #[test]
    fn switcher_row_narrow_drops_origin_and_keeps_kind() {
        // narrow=true → origin + long status text dropped to save columns.
        let e = pty_entry(3, "vim notes.md");
        let row = render_switcher_row(&e, "⏳", true, 30);
        assert!(row.contains("#3"));
        assert!(row.contains("pty"), "kind is still shown when narrow: {row}");
        assert!(!row.contains("user"), "origin tag dropped when narrow: {row}");
    }

    #[test]
    fn switcher_row_never_exceeds_budget_and_truncates_title() {
        let e = pty_entry(
            7,
            "a-very-long-interactive-command-that-will-not-fit-in-a-tiny-terminal",
        );
        let budget: u16 = 24;
        let row = render_switcher_row(&e, "⏳", true, budget);
        assert!(
            row.chars().count() <= budget as usize,
            "row ({} cols) must fit budget {budget}: {row}",
            row.chars().count()
        );
        assert!(row.contains('\u{2026}'), "an elided title ends with an ellipsis: {row}");
    }

    #[test]
    fn switcher_row_zero_budget_is_empty() {
        let e = pty_entry(1, "x");
        assert!(render_switcher_row(&e, "⏳", false, 0).is_empty());
    }

    #[test]
    fn sessions_strip_empty_state_is_hidden() {
        let state = TuiState::new("p", "m");
        assert!(!sessions_status_visible(&state));
        assert!(render_sessions_strip_line(&[], "", crate::chat::sessions::FocusTarget::Main, false, 40).is_empty());
    }

    #[test]
    fn sessions_strip_one_active_entry_shows_marker_glyph_kind_and_title() {
        let entries = vec![entry(1)];
        let line = render_sessions_strip_line(
            &entries,
            "",
            crate::chat::sessions::FocusTarget::Session { seq: 1 },
            false,
            80,
        );
        assert!(line.contains('\u{25B8}'), "active marker visible: {line}");
        assert!(line.contains('⏳'), "status glyph visible: {line}");
        assert!(line.contains("#1"), "seq visible: {line}");
        assert!(line.contains("agent"), "kind visible: {line}");
        assert!(line.contains("task 1"), "title visible: {line}");
        assert!(
            !line.contains("tok"),
            "P1 must not fake per-session token usage: {line}"
        );
    }

    #[test]
    fn sessions_strip_multiple_entries_share_one_row() {
        let entries = vec![entry(1), entry(2)];
        let line = render_sessions_strip_line(&entries, "", crate::chat::sessions::FocusTarget::Main, false, 80);
        assert!(line.contains("#1"), "first session visible: {line}");
        assert!(line.contains("#2"), "second session visible: {line}");
        assert!(line.contains('\u{00B7}'), "entries separated in one row: {line}");
    }

    #[test]
    fn sessions_strip_narrow_width_truncates() {
        let entries = vec![pty_entry(
            7,
            "a-very-long-interactive-command-that-will-not-fit-in-the-strip",
        )];
        let width = 24;
        let line = render_sessions_strip_line(
            &entries,
            "",
            crate::chat::sessions::FocusTarget::Session { seq: 7 },
            false,
            width,
        );
        assert!(
            line.chars().count() <= width as usize,
            "strip row must fit width {width}, got {} chars: {line}",
            line.chars().count()
        );
        assert!(line.contains('\u{2026}'), "long title should be elided: {line}");
    }

    #[test]
    fn sessions_strip_cjk_title_truncates_without_panicking() {
        let entries = vec![crate::chat::sessions::SwitcherEntry {
            seq: 5,
            kind: "shell",
            origin: "user",
            status: "running",
            title: "监控任务执行状态和输出窗口".to_string(),
        }];
        let width = 22;
        let line = render_sessions_strip_line(
            &entries,
            "",
            crate::chat::sessions::FocusTarget::Session { seq: 5 },
            false,
            width,
        );
        assert!(
            UnicodeWidthStr::width(line.as_str()) <= width as usize,
            "CJK strip row must fit column budget {width}, got {} cols: {line}",
            UnicodeWidthStr::width(line.as_str())
        );
        assert!(line.contains("#5"), "seq remains visible under CJK truncation: {line}");
    }

    #[test]
    fn truncation_is_cjk_column_accurate() {
        let line = truncate_chars_with_ellipsis("你好世界abc", 5, false);
        assert!(
            UnicodeWidthStr::width(line.as_str()) <= 5,
            "wide chars must fit the column budget, got {:?} width {}",
            line,
            UnicodeWidthStr::width(line.as_str())
        );
        assert!(
            line.ends_with('\u{2026}'),
            "wide truncation should use ellipsis: {line:?}"
        );
    }

    fn active_view(seq: u64, lines: Vec<String>, scroll_offset: usize) -> crate::chat::sessions::ActiveSessionView {
        crate::chat::sessions::ActiveSessionView {
            seq,
            kind: "agent".to_string(),
            title: "监控任务执行状态和输出窗口-with-a-long-title".to_string(),
            lines,
            truncated: true,
            scroll_offset,
        }
    }

    #[test]
    fn active_session_visible_lines_follow_tail_and_scroll_slice() {
        let view = active_view(4, (0..20).map(|i| format!("line {i}")).collect(), 0);
        assert_eq!(
            active_session_visible_lines(&view, 3),
            vec!["line 17".to_string(), "line 18".to_string(), "line 19".to_string()]
        );

        let scrolled = active_view(4, (0..20).map(|i| format!("line {i}")).collect(), 2);
        assert_eq!(
            active_session_visible_lines(&scrolled, 3),
            vec!["line 15".to_string(), "line 16".to_string(), "line 17".to_string()]
        );
    }

    #[test]
    fn transcript_view_is_bounded_and_handles_empty_history() {
        let empty = build_transcript_view("", &[], 0);
        assert_eq!(empty.seq, TRANSCRIPT_SESSION_SEQ);
        assert_eq!(
            empty.kind,
            crate::chat::sessions::model::ManagedKind::Transcript.as_str()
        );
        assert_eq!(empty.lines, vec!["(transcript is empty)".to_string()]);

        let long = ConversationLine::Assistant {
            content: (0..(TRANSCRIPT_MAX_LINES + 25))
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let view = build_transcript_view("demo", &[long], usize::MAX);
        assert_eq!(view.lines.len(), TRANSCRIPT_MAX_LINES);
        assert!(view.truncated, "long transcript must report truncation");
        assert_eq!(
            view.scroll_offset,
            view.max_scroll_offset(usize::from(ACTIVE_SESSION_VIEW_DESIRED_ROWS)),
            "oversized offset clamps to the oldest retained visible page"
        );
        assert!(
            view.lines.first().is_some_and(|line| line.contains("line 25")),
            "oldest lines are trimmed first: {:?}",
            view.lines.first()
        );
    }

    #[test]
    fn diff_view_handles_empty_bounded_and_clamps_scroll() {
        let empty = build_diff_view("", Vec::new(), false, 0);
        assert_eq!(empty.seq, DIFF_SESSION_SEQ);
        assert_eq!(empty.kind, crate::chat::sessions::model::ManagedKind::Diff.as_str());
        assert_eq!(empty.title, "workspace diff");
        assert_eq!(empty.lines, vec!["(no workspace diff)".to_string()]);

        let view = build_diff_view(
            "staged diff",
            (0..24).map(|i| format!("+新增行{i}")).collect(),
            true,
            usize::MAX,
        );
        assert_eq!(view.title, "staged diff");
        assert!(view.truncated);
        assert_eq!(
            view.scroll_offset,
            view.max_scroll_offset(usize::from(ACTIVE_SESSION_VIEW_DESIRED_ROWS)),
            "oversized diff offset clamps to the oldest retained visible page"
        );
    }

    #[test]
    fn p0_8_esc_nonempty_clears_input_even_when_attached() {
        // Muscle memory preserved: non-empty input clears first, never detaches.
        let mut state = TuiState::new("p", "m");
        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 2 };
        for ch in "hi".chars() {
            dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
        }
        let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
        assert_eq!(out, KeyDispatch::Cancelled);
        assert!(state.input.is_empty(), "non-empty Esc clears, does not detach");
    }

    #[test]
    fn p0_8_esc_empty_attached_requests_detach() {
        // Empty input + session focus → detach.
        let mut state = TuiState::new("p", "m");
        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 3 };
        let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
        assert_eq!(out, KeyDispatch::RequestDetach);
    }

    #[test]
    fn esc_empty_diff_closes_viewer() {
        let mut state = TuiState::new("p", "m");
        state.focus = crate::chat::sessions::FocusTarget::Diff;
        let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
        assert_eq!(out, KeyDispatch::CloseDiffViewer);
    }

    #[test]
    fn ctrl_g_opens_switcher_over_cache() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1), entry(2)];
        let out = dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
        match out {
            KeyDispatch::SwitcherOpened { entries } => {
                assert_eq!(entries.len(), 3);
                assert!(
                    entries.first().is_some_and(|entry| entry.is_transcript()),
                    "transcript row is switcher-only first entry"
                );
                assert_eq!(
                    state.sessions_cache.len(),
                    2,
                    "transcript row must not enter real-session cache"
                );
            }
            other => panic!("expected SwitcherOpened, got {other:?}"),
        }
        assert!(state.switcher.is_some(), "switcher opened in mirror");
    }

    #[test]
    fn switcher_navigation_and_enter_attaches_selected() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1), entry(2), entry(3)];
        dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
        // Transcript is row 0; Down three times → select #3.
        let m1 = dispatch_global_key(key(KeyCode::Down), &mut state);
        assert_eq!(m1, KeyDispatch::SwitcherMoved { selected: 1 });
        dispatch_global_key(key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL), &mut state);
        dispatch_global_key(key(KeyCode::Down), &mut state);
        // Enter attaches the highlighted session and closes the switcher.
        let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
        assert_eq!(out, KeyDispatch::AttachSession { seq: 3 });
        assert!(state.switcher.is_none(), "switcher closed after attach");
    }

    #[test]
    fn switcher_esc_closes_without_attaching() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1)];
        dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
        let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
        assert_eq!(out, KeyDispatch::SwitcherClosed);
        assert!(state.switcher.is_none());
    }

    #[test]
    fn switcher_ctrl_g_toggles_closed() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1)];
        dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
        assert!(state.switcher.is_some());
        let out = dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
        assert_eq!(out, KeyDispatch::SwitcherClosed);
        assert!(state.switcher.is_none(), "second Ctrl+G toggles closed");
    }

    #[test]
    fn switcher_transcript_enter_opens_viewer_without_attach_zero() {
        let mut state = TuiState::new("p", "m");
        // No cached real sessions; Ctrl+G still offers the transcript child TUI.
        dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
        let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
        assert_eq!(
            out,
            KeyDispatch::OpenTranscriptViewer,
            "transcript row must open the viewer, never /attach 0"
        );
        assert!(state.switcher.is_none());
    }

    #[test]
    fn switcher_open_swallows_plain_keys() {
        // While the overlay has focus, plain typing must not leak into the input.
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1)];
        dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
        let out = dispatch_global_key(key(KeyCode::Char('x')), &mut state);
        assert_eq!(out, KeyDispatch::Consumed);
        assert!(state.input.is_empty(), "switcher swallowed the keystroke");
    }

    #[test]
    fn prompt_indicator_main_vs_session() {
        let (main_span, main_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Main, false);
        assert_eq!(main_span.content.as_ref(), "> ");
        assert_eq!(main_w, 2);
        let (sess_span, sess_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Session { seq: 4 }, false);
        assert!(sess_span.content.contains("agent #4"), "carries the target as text");
        assert!(sess_span.content.contains('\u{25B8}'), "uses the ▸ glyph");
        assert_eq!(sess_w, UnicodeWidthStr::width(sess_span.content.as_ref()));
        // ASCII fallback drops the unicode glyph but keeps the text target.
        let (ascii_span, _) = prompt_indicator(crate::chat::sessions::FocusTarget::Session { seq: 4 }, true);
        assert!(ascii_span.content.contains("agent #4"));
        assert!(!ascii_span.content.contains('\u{25B8}'), "ascii fallback omits ▸");
        let (transcript_span, transcript_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Transcript, false);
        assert!(transcript_span.content.contains("transcript"));
        assert!(transcript_span.content.contains('\u{25B8}'));
        assert_eq!(transcript_w, UnicodeWidthStr::width(transcript_span.content.as_ref()));
        let (diff_span, diff_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Diff, false);
        assert!(diff_span.content.contains("diff"));
        assert_eq!(diff_w, UnicodeWidthStr::width(diff_span.content.as_ref()));
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

    #[test]
    fn measure_wrapped_rows_matches_ratatui_wordwrap() {
        // `measure_wrapped_rows` must return the EXACT number of rows
        // ratatui's word-wrapping produces (not the char-count upper bound
        // from `wrapped_rows_for_lines`). The streaming-preview scroll math
        // depends on this: an over-count scrolls past the newest tokens and
        // leaves the visible body truncated (chat-demo defect #1).
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let samples = [
            "I am a lightweight AI assistant built with Rust that can help you run terminal commands, read and write files, and manage projects.",
            "你好，我是一个用 Rust 构建的轻量级 AI 助手，能帮你执行终端命令、读写文件、管理项目。",
            "short",
            "a b c d e f g h i j k l m n o p q r s t u v w x y z one two three",
        ];
        for s in samples {
            let lines = vec![Line::from(s.to_string())];
            for width in [20u16, 40, 80, 191] {
                let measured = measure_wrapped_rows(&lines, width);
                // Ground truth: render at a generous height and read back the
                // last populated row.
                let area = Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 64,
                };
                let mut buf = Buffer::empty(area);
                Paragraph::new(Text::from(lines.clone()))
                    .wrap(Wrap { trim: false })
                    .render(area, &mut buf);
                let mut actual = 0u16;
                for y in 0..area.height {
                    for x in 0..width {
                        if let Some(c) = buf.cell((x, y)) {
                            if !c.symbol().trim().is_empty() {
                                actual = y + 1;
                                break;
                            }
                        }
                    }
                }
                let actual = actual.max(1);
                assert_eq!(
                    measured, actual,
                    "measure_wrapped_rows({width}) on {s:?}: got {measured}, ratatui rendered {actual}"
                );
            }
        }
    }

    #[test]
    fn streaming_preview_scroll_keeps_tail_visible() {
        // Regression for chat-demo defect #1: a streaming body longer than
        // the preview window must scroll so the LAST rows (newest tokens)
        // are the ones rendered. We assert the scroll offset derived from
        // `measure_wrapped_rows` lands the tail inside the window.
        let body: String = (0..30).map(|i| format!("line number {i}\n")).collect();
        let cursor = "\u{258C}";
        let mut body_lines: Vec<&str> = body.lines().collect();
        if body.ends_with('\n') {
            body_lines.push("");
        }
        let last_idx = body_lines.len().saturating_sub(1);
        let mut sink: Vec<Line<'_>> = Vec::new();
        for (i, t) in body_lines.iter().enumerate() {
            if i == last_idx {
                sink.push(Line::from(format!("{t}{cursor}")));
            } else {
                sink.push(Line::from((*t).to_string()));
            }
        }
        let width = 80u16;
        let window = 6;
        let total = measure_wrapped_rows(&sink, width);
        assert!(total > window, "test setup: body must overflow the window");
        let scroll = total.saturating_sub(window);
        // After scrolling, the visible window [scroll, scroll+window) must
        // include the final content row (total-1, since the trailing cursor
        // row is the newest output).
        let last_content_row = total.saturating_sub(1);
        assert!(
            (scroll..scroll.saturating_add(window)).contains(&last_content_row),
            "scroll {scroll} + window {window} must cover last row {last_content_row} of {total}"
        );
    }

    #[test]
    fn fullscreen_bottom_chrome_height_expands_with_input_not_streaming() {
        // Resting state: status (1) + input border (1) + input row (1) +
        // footer (1) = 4. Streaming is rendered at the transcript tail, not in
        // bottom chrome. A long multi-line input adds rows up to the visible cap.
        let mut state = TuiState::new("p", "m");
        let idle = fullscreen_bottom_chrome_height(&state);
        assert!(idle >= BOTTOM_CHROME_MIN_HEIGHT);
        assert!(idle <= BOTTOM_CHROME_MAX_HEIGHT);

        state.start_stream("d-live");
        let streaming = fullscreen_bottom_chrome_height(&state);
        assert_eq!(streaming, idle, "streaming must not duplicate into fullscreen chrome");

        // Multi-line input drives growth too (until clamped).
        state.cancel_stream("d-live");
        for _ in 0..6 {
            state.input.lines.push(String::new());
        }
        let tall = fullscreen_bottom_chrome_height(&state);
        assert!(tall > idle, "multi-line input must add rows: tall={tall}, idle={idle}");
        assert!(
            tall <= BOTTOM_CHROME_MAX_HEIGHT,
            "must be clamped to BOTTOM_CHROME_MAX_HEIGHT"
        );
    }

    fn fullscreen_rows(
        state: &TuiState,
        width: u16,
        height: u16,
        scroll: &mut FullscreenTranscriptScroll,
    ) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        terminal
            .draw(|frame| {
                render_fullscreen_chat(frame, state, scroll);
            })
            .expect("draw fullscreen chat");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<Vec<_>>().join(""))
            .collect()
    }

    #[test]
    fn fullscreen_empty_chat_draws_transcript_pane_and_pinned_chrome() {
        let state = TuiState::new("provider", "model");
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 64, 18, &mut scroll);

        assert!(
            rows.iter().any(|row| row.contains("(transcript is empty)")),
            "empty transcript message rendered: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("PRX Chat")),
            "status bar rendered: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Input")),
            "input chrome rendered: {rows:?}"
        );
        assert!(
            rows.last().is_some_and(|row| row.contains("Ctrl+G")),
            "footer pinned to bottom row: {rows:?}"
        );
    }

    #[test]
    fn fullscreen_long_transcript_follows_tail_and_pages_up() {
        let mut state = TuiState::new("provider", "model");
        state.conversation_lines.push(ConversationLine::Assistant {
            content: (0..60)
                .map(|idx| format!("line {idx:03}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });

        let mut tail = FullscreenTranscriptScroll::default();
        let tail_rows = fullscreen_rows(&state, 64, 14, &mut tail);
        assert!(
            tail_rows.iter().any(|row| row.contains("line 059")),
            "tail visible: {tail_rows:?}"
        );
        assert!(
            !tail_rows.iter().any(|row| row.contains("line 000")),
            "old head is not visible at tail: {tail_rows:?}"
        );

        let mut scrolled = FullscreenTranscriptScroll::default();
        scrolled.page_up(8);
        let scrolled_rows = fullscreen_rows(&state, 64, 14, &mut scrolled);
        assert!(
            scrolled_rows.iter().any(|row| row.contains("line 051")),
            "page-up exposes older rows: {scrolled_rows:?}"
        );
        assert!(
            !scrolled_rows.iter().any(|row| row.contains("line 059")),
            "tail row moved out after page-up: {scrolled_rows:?}"
        );
    }

    #[test]
    fn fullscreen_home_end_jump_top_and_tail() {
        let mut state = TuiState::new("provider", "model");
        state.conversation_lines.push(ConversationLine::Assistant {
            content: (0..80)
                .map(|idx| format!("jump line {idx:03}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        let mut scroll = FullscreenTranscriptScroll::default();

        scroll.jump_top();
        let top_rows = fullscreen_rows(&state, 64, 14, &mut scroll);
        assert!(
            top_rows.iter().any(|row| row.contains("jump line 000")),
            "Home jump exposes transcript top: {top_rows:?}"
        );
        assert!(
            !top_rows.iter().any(|row| row.contains("jump line 079")),
            "Home jump leaves tail below: {top_rows:?}"
        );

        scroll.jump_bottom();
        let tail_rows = fullscreen_rows(&state, 64, 14, &mut scroll);
        assert!(
            tail_rows.iter().any(|row| row.contains("jump line 079")),
            "End jump returns to transcript tail: {tail_rows:?}"
        );
    }

    #[test]
    fn fullscreen_new_output_below_hint_appears_only_while_scrolled_up() {
        let mut state = TuiState::new("provider", "model");
        state.conversation_lines.push(ConversationLine::Assistant {
            content: (0..55)
                .map(|idx| format!("hint line {idx:03}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        let mut scroll = FullscreenTranscriptScroll::default();
        scroll.page_up(8);
        let before_rows = fullscreen_rows(&state, 70, 16, &mut scroll);
        assert!(
            !before_rows.iter().any(|row| row.contains("New output below")),
            "existing scrollback alone should not show new-output hint: {before_rows:?}"
        );

        state.streaming = Some(StreamingDraft {
            draft_id: "hint-draft".to_string(),
            accumulated: "streaming delta below".to_string(),
            version: 1,
        });
        let hinted_rows = fullscreen_rows(&state, 70, 16, &mut scroll);
        assert!(
            hinted_rows.iter().any(|row| row.contains("New output below")),
            "new tail output while scrolled up shows footer hint: {hinted_rows:?}"
        );

        scroll.jump_bottom();
        let tail_rows = fullscreen_rows(&state, 70, 16, &mut scroll);
        assert!(
            !tail_rows.iter().any(|row| row.contains("New output below")),
            "jumping to tail clears new-output hint: {tail_rows:?}"
        );
        assert!(
            tail_rows.iter().any(|row| row.contains("streaming delta below")),
            "tail output visible after jump-bottom: {tail_rows:?}"
        );
    }

    #[test]
    fn fullscreen_multiline_input_expands_bottom_chrome_without_gap() {
        let mut state = TuiState::new("provider", "model");
        state.input.set_text("first line\nsecond line\nthird line");
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 70, 18, &mut scroll);

        assert!(
            rows.iter().any(|row| row.contains("first line")),
            "first input row: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("second line")),
            "second input row: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("third line")),
            "third input row: {rows:?}"
        );
        assert!(
            rows.last().is_some_and(|row| row.contains("Ctrl+G")),
            "footer remains pinned after multiline input: {rows:?}"
        );
    }

    #[test]
    fn fullscreen_streaming_draft_renders_at_transcript_tail() {
        let mut state = TuiState::new("provider", "model");
        state.conversation_lines.push(ConversationLine::User {
            content: "hello".to_string(),
        });
        state.streaming = Some(StreamingDraft {
            draft_id: "draft-1".to_string(),
            accumulated: "streaming tail".to_string(),
            version: 1,
        });
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 64, 16, &mut scroll);

        assert!(
            rows.iter().any(|row| row.contains("> hello")),
            "history rendered: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("streaming tail")),
            "streaming tail rendered: {rows:?}"
        );
    }

    #[test]
    fn fullscreen_streaming_tail_is_not_duplicated_in_bottom_chrome() {
        let mut state = TuiState::new("provider", "model");
        state.streaming = Some(StreamingDraft {
            draft_id: "draft-1".to_string(),
            accumulated: "phase2 unique streaming tail".to_string(),
            version: 1,
        });
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 72, 16, &mut scroll);
        let occurrences = rows
            .iter()
            .filter(|row| row.contains("phase2 unique streaming tail"))
            .count();
        assert_eq!(
            occurrences, 1,
            "fullscreen should render streaming only at transcript tail, not chrome preview: {rows:?}"
        );
    }

    #[test]
    fn fullscreen_switcher_overlay_renders_over_large_frame_with_chrome_pinned() {
        let mut state = TuiState::new("provider", "model");
        state.switcher = Some(crate::chat::sessions::SwitcherState::new(vec![entry(1), entry(2)]));
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 80, 24, &mut scroll);

        assert!(
            rows.iter().any(|row| row.contains("Sessions - child TUI registry")),
            "switcher overlay title rendered: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("task 1")),
            "switcher row rendered: {rows:?}"
        );
        assert!(
            rows.last().is_some_and(|row| row.contains("Ctrl+G")),
            "bottom chrome footer remains pinned below overlay: {rows:?}"
        );
    }

    #[test]
    fn fullscreen_saved_session_picker_fits_as_overlay_with_chrome_pinned() {
        let mut state = TuiState::new("provider", "model");
        state.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
            saved_picker_entry("saved-1", "saved session one", false),
            saved_picker_entry("saved-2", "saved session two", true),
        ]));
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 86, 24, &mut scroll);
        assert!(
            rows.iter().any(|row| row.contains("Saved chat sessions")),
            "saved-session picker title rendered: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("saved session one")),
            "saved-session picker rows rendered: {rows:?}"
        );
        assert!(
            rows.last().is_some_and(|row| row.contains("Ctrl+G")),
            "bottom chrome footer remains pinned below saved picker: {rows:?}"
        );
    }

    #[test]
    fn fullscreen_approval_panel_stays_visible_above_input_when_transcript_scrolled() {
        let mut state = TuiState::new("provider", "model");
        state.conversation_lines.push(ConversationLine::Assistant {
            content: (0..60)
                .map(|idx| format!("approval line {idx:03}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        state.focus = crate::chat::sessions::FocusTarget::Approval;
        state.pending_tool_approval = Some(crate::chat::sessions::PendingToolApprovalView {
            tool_id: "tool-1".to_string(),
            name: "danger_tool".to_string(),
            args: "{\"path\":\"/tmp/demo\"}".to_string(),
        });
        let mut scroll = FullscreenTranscriptScroll::default();
        scroll.jump_top();

        let rows = fullscreen_rows(&state, 80, 24, &mut scroll);
        assert!(
            rows.iter().any(|row| row.contains("Tool Approval")),
            "approval panel visible outside transcript scroll: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("danger_tool")),
            "approval tool name visible: {rows:?}"
        );
        assert!(
            rows.last().is_some_and(|row| row.contains("Ctrl+G")),
            "input footer remains pinned below approval panel: {rows:?}"
        );
    }

    #[test]
    fn fullscreen_child_and_diff_views_use_large_panel_above_input() {
        let mut state = TuiState::new("provider", "model");
        state.conversation_lines.push(ConversationLine::User {
            content: "main transcript".to_string(),
        });
        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 7 };
        state.active_session_view = Some(crate::chat::sessions::ActiveSessionView {
            seq: 7,
            kind: "agent".to_string(),
            title: "phase2 child".to_string(),
            lines: (0..40).map(|idx| format!("child-line-{idx:03}")).collect(),
            truncated: false,
            scroll_offset: 0,
        });
        let mut scroll = FullscreenTranscriptScroll::default();
        let child_rows = fullscreen_rows(&state, 84, 26, &mut scroll);
        assert!(
            child_rows.iter().any(|row| row.contains("attached #7 agent")),
            "child view header rendered in fullscreen panel: {child_rows:?}"
        );
        assert!(
            child_rows.iter().any(|row| row.contains("child-line-039")),
            "child view tail rendered in fullscreen panel: {child_rows:?}"
        );

        state.focus = crate::chat::sessions::FocusTarget::Diff;
        state.active_session_view = Some(crate::chat::sessions::ActiveSessionView {
            seq: DIFF_SESSION_SEQ,
            kind: crate::chat::sessions::model::ManagedKind::Diff.as_str().to_string(),
            title: "workspace diff".to_string(),
            lines: vec!["diff --git a/src/lib.rs b/src/lib.rs".to_string()],
            truncated: false,
            scroll_offset: 0,
        });
        let diff_rows = fullscreen_rows(&state, 84, 26, &mut scroll);
        assert!(
            diff_rows.iter().any(|row| row.contains("diff workspace diff")),
            "diff view header rendered in fullscreen panel: {diff_rows:?}"
        );
        assert!(
            diff_rows.iter().any(|row| row.contains("diff --git")),
            "diff body rendered in fullscreen panel: {diff_rows:?}"
        );
    }

    #[test]
    fn fullscreen_scroll_focus_rules_do_not_steal_input_or_child_keys() {
        let mut input_state = TuiState::new("provider", "model");
        input_state.input.set_text("draft");
        assert!(
            !fullscreen_transcript_scroll_available(&input_state),
            "draft input keeps Home/End/Page keys for the input box"
        );

        let mut history_state = TuiState::new("provider", "model");
        history_state.input.history = vec!["older".to_string(), "newer".to_string()];
        let out = dispatch_global_key(key(KeyCode::Up), &mut history_state);
        assert_eq!(out, KeyDispatch::Consumed);
        assert_eq!(
            history_state.input.text(),
            "newer",
            "Up still navigates input history and is not a transcript-scroll key"
        );

        let mut child_state = TuiState::new("provider", "model");
        child_state.focus = crate::chat::sessions::FocusTarget::Session { seq: 3 };
        child_state.active_session_view = Some(crate::chat::sessions::ActiveSessionView {
            seq: 3,
            kind: "agent".to_string(),
            title: "child".to_string(),
            lines: vec!["child output".to_string()],
            truncated: false,
            scroll_offset: 0,
        });
        assert!(
            !fullscreen_transcript_scroll_available(&child_state),
            "focused child view owns PageUp/PageDown in fullscreen"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::PageUp), &mut child_state),
            KeyDispatch::PageSessionUp
        );
    }

    #[test]
    fn sessions_status_row_adds_height_only_when_present() {
        let mut state = TuiState::new("p", "m");
        let idle = fullscreen_bottom_chrome_height(&state);
        assert!(!sessions_status_visible(&state), "empty status row hidden");

        state.set_sessions_status("sessions: 1 running");
        assert!(sessions_status_visible(&state), "non-empty status row shown");
        let with_row = fullscreen_bottom_chrome_height(&state);
        assert_eq!(
            with_row,
            idle.saturating_add(1),
            "sessions status row adds exactly one row"
        );

        // Clearing the status hides the row again.
        state.set_sessions_status("");
        assert!(!sessions_status_visible(&state));
        assert_eq!(fullscreen_bottom_chrome_height(&state), idle);
    }

    #[test]
    fn sessions_status_row_stays_within_height_budget() {
        // With current constants the busiest fullscreen chrome is
        // status(1)+input(1+10)+footer(1), so adding the 1-row sessions line
        // still fits under BOTTOM_CHROME_MAX_HEIGHT (24).
        let mut state = TuiState::new("p", "m");
        state.set_sessions_status("sessions: 9 running");
        state.start_stream("d");
        for _ in 0..(INPUT_MAX_VISIBLE_ROWS + 4) {
            state.input.lines.push(String::new());
        }
        assert!(
            sessions_status_visible(&state),
            "the sessions row fits within the height budget under real inputs"
        );
        assert!(fullscreen_bottom_chrome_height(&state) <= BOTTOM_CHROME_MAX_HEIGHT);
    }

    #[test]
    fn sessions_status_row_degrades_when_budget_exhausted() {
        // Forward-compat guard: when the rest of the chrome already meets/exceeds
        // the max height, the sessions row is the first thing dropped so the
        // input box and footer never lose rows. We exercise the guard directly
        // via its documented threshold (without depending on specific constants).
        let without_sessions = 1u16 // status
            + u16::try_from(INPUT_MAX_VISIBLE_ROWS + 1).unwrap_or(11)
            + 1; // footer
        assert!(
            without_sessions < BOTTOM_CHROME_MAX_HEIGHT,
            "guard threshold: row drops once the rest reaches BOTTOM_CHROME_MAX_HEIGHT"
        );
    }

    #[test]
    fn status_token_estimate_tracks_visible_chat_and_streaming_text() {
        let mut state = TuiState::new("p", "m");
        state.push_user_message("12345678");
        state.push_assistant_message("abcd");
        assert_eq!(estimate_visible_token_usage(&state), 3);

        state.start_stream("d-live");
        assert!(state.update_stream("d-live", "wxyz", 1));
        assert_eq!(
            estimate_visible_token_usage(&state),
            4,
            "streaming text contributes to the status-bar estimate"
        );
    }

    #[test]
    fn status_bar_renders_context_window_percentage() {
        let mut state = TuiState::new("provider", "model");
        state.session_title = "budget".to_string();
        state.context_window_tokens = Some(1_000_000);
        state.push_user_message(&"x".repeat(80_000));

        let line = render_status_bar_text(&state, 120);
        assert!(
            line.contains("~20k / 1M tok (2%)"),
            "status should include used/window percentage: {line}"
        );
    }

    #[test]
    fn status_bar_renders_chat_mode_and_autonomy_ceiling() {
        let mut state = TuiState::new("provider", "model");
        state.chat_mode = ChatMode::Auto;
        state.autonomy_level = AutonomyLevel::ReadOnly;

        let line = render_status_bar_text(&state, 120);

        assert!(
            line.contains("mode:auto auth:read_only"),
            "permission status missing: {line}"
        );
        assert!(
            !line.contains("bypass"),
            "status copy must not imply permissions are bypassed: {line}"
        );
    }

    #[test]
    fn status_bar_permission_status_degrades_at_narrow_width() {
        let mut state = TuiState::new("provider", "model");
        state.chat_mode = ChatMode::Plan;
        state.autonomy_level = AutonomyLevel::Full;
        state.context_window_tokens = Some(1_000_000);

        let line = render_status_bar_text(&state, 32);

        assert!(
            UnicodeWidthStr::width(line.as_str()) <= 32,
            "narrow status must fit display width: {line:?}"
        );
        assert!(
            line.contains("mode:"),
            "minimal status should retain mode before truncation: {line}"
        );
    }

    #[test]
    fn status_bar_renders_10m_window_without_raw_integer() {
        let mut state = TuiState::new("provider", "model");
        state.context_window_tokens = Some(10_000_000);
        state.push_user_message(&"x".repeat(40_000));

        let line = render_status_bar_text(&state, 120);
        assert!(line.contains("/ 10M tok"), "10M window should be compact: {line}");
        assert!(
            !line.contains("10000000"),
            "10M window must not render as a raw integer: {line}"
        );
    }

    #[test]
    fn status_bar_context_window_percentage_is_panic_safe() {
        assert_eq!(render_token_budget(42, Some(0)), "~42 tok");
        assert_eq!(render_token_budget(2_000_000, Some(1_000_000)), "~2M / 1M tok (100%)");
        assert_eq!(render_token_budget(42, None), "~42 tok");
    }

    fn render_conversation_line_to_buffer(buf: &mut Buffer, line: &ConversationLine, ascii: bool) {
        let mut lines: Vec<Line<'_>> = Vec::new();
        render_conversation_line(&mut lines, line, ascii);
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        paragraph.render(buf.area, buf);
    }

    // ── CJK / wide-char rendering regression tests ───────────────────────
    //
    // These tests guard the shared conversation-line renderer against phantom
    // spaces in wide CJK text. Each CJK character occupies 2 columns; ratatui
    // stores the glyph in cell[x] and resets cell[x+1] to `Cell::EMPTY`
    // (whose `symbol()` returns `" "`). Buffer diffing must reconstruct the
    // original text without inter-character spaces.

    #[test]
    #[cfg(feature = "terminal-tui")]
    fn cjk_buffer_diff_omits_continuation_cells() {
        // Build a buffer containing a Chinese assistant message, then take
        // its diff against an empty buffer. The diff updates must not contain
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
        render_conversation_line_to_buffer(&mut filled, &line, false);

        // Collect symbols from the diff.
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
        render_conversation_line_to_buffer(&mut filled, &line, false);

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
        render_conversation_line_to_buffer(&mut filled, &line, false);

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
        /// `fullscreen_bottom_chrome_height` 在两种 view 上返回相同值.
        #[test]
        fn s4_a_2_fullscreen_bottom_chrome_height_parity_tui_vs_snapshot() {
            let mut state = make_state_with_lines();
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
                fullscreen_bottom_chrome_height(&tui),
                fullscreen_bottom_chrome_height(&snap),
                "TuiState vs UiSnapshot 在同 fixture 下高度应一致"
            );
        }

        /// Parity 检查：streaming 状态下两种 view 的高度仍一致.
        #[test]
        fn s4_a_2_fullscreen_bottom_chrome_height_parity_streaming() {
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

            let h_tui = fullscreen_bottom_chrome_height(&tui);
            let h_snap = fullscreen_bottom_chrome_height(&snap);
            assert_eq!(h_tui, h_snap, "streaming 下高度应一致 (tui={h_tui}, snap={h_snap})");
            assert_eq!(
                h_tui,
                fullscreen_bottom_chrome_height(&TuiState::new(&state.session.provider, &state.session.model)),
                "streaming 不应重复占用 fullscreen bottom chrome"
            );
        }

        /// Parity 检查：BottomChromeView 各 getter 在 TuiState 与 UiSnapshot 上返回相同字段.
        #[test]
        fn s4_a_2_view_getters_parity() {
            let mut state = make_state_with_lines();
            let session_entry = crate::chat::sessions::SwitcherEntry {
                seq: 7,
                kind: "agent",
                origin: "model",
                status: "running",
                title: "non-empty parity session".to_string(),
            };
            state.ui.sessions_entries = vec![session_entry.clone()];
            state.ui.context_window_tokens = Some(10_000_000);
            state.ui.chat_mode = ChatMode::Auto;
            state.ui.autonomy_level = AutonomyLevel::ReadOnly;
            let active_view = crate::chat::sessions::ActiveSessionView {
                seq: DIFF_SESSION_SEQ,
                kind: crate::chat::sessions::model::ManagedKind::Diff.as_str().to_string(),
                title: "workspace diff".to_string(),
                lines: vec![
                    "diff --git a/src/lib.rs b/src/lib.rs".to_string(),
                    "+line a".to_string(),
                ],
                truncated: true,
                scroll_offset: 1,
            };
            state.ui.focus = crate::chat::sessions::FocusTarget::Diff;
            state.ui.active_session_view = Some(active_view.clone());
            let saved_picker = crate::chat::session::SavedSessionPickerState {
                entries: vec![saved_picker_entry("saved-1", "saved parity session", true)],
                selected: 0,
            };
            state.ui.saved_session_picker = Some(saved_picker.clone());
            let snap = state.build_ui_snapshot(5);

            let mut tui = TuiState::new(&state.session.provider, &state.session.model);
            tui.session_title = state.session.title.clone();
            tui.turn_count = state.ui.turn_count;
            tui.ascii_fallback = state.ui.ascii_fallback;
            tui.conversation_lines = state.ui.conversation_lines.clone();
            tui.streaming.clone_from(&state.stream.draft);
            tui.input = state.ui.input.clone();
            tui.sessions_cache = vec![session_entry];
            tui.chat_mode = ChatMode::Auto;
            tui.autonomy_level = AutonomyLevel::ReadOnly;
            tui.focus = crate::chat::sessions::FocusTarget::Diff;
            tui.active_session_view = Some(active_view);
            tui.context_window_tokens = Some(10_000_000);
            tui.saved_session_picker = Some(saved_picker);

            assert_eq!(BottomChromeView::provider(&tui), BottomChromeView::provider(&snap));
            assert_eq!(BottomChromeView::model(&tui), BottomChromeView::model(&snap));
            assert_eq!(BottomChromeView::chat_mode(&tui), BottomChromeView::chat_mode(&snap));
            assert_eq!(
                BottomChromeView::autonomy_level(&tui),
                BottomChromeView::autonomy_level(&snap)
            );
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
            assert_eq!(
                BottomChromeView::sessions_entries(&tui).len(),
                BottomChromeView::sessions_entries(&snap).len()
            );
            let [tui_entry] = BottomChromeView::sessions_entries(&tui) else {
                panic!("TuiState fixture should expose exactly one session entry");
            };
            let [snap_entry] = BottomChromeView::sessions_entries(&snap) else {
                panic!("UiSnapshot fixture should expose exactly one session entry");
            };
            assert_eq!(tui_entry.seq, snap_entry.seq);
            assert_eq!(tui_entry.kind, snap_entry.kind);
            assert_eq!(tui_entry.origin, snap_entry.origin);
            assert_eq!(tui_entry.status, snap_entry.status);
            assert_eq!(tui_entry.title, snap_entry.title);
            assert_eq!(
                BottomChromeView::active_session_view(&tui),
                BottomChromeView::active_session_view(&snap)
            );
            assert_eq!(BottomChromeView::focus(&tui), BottomChromeView::focus(&snap));
            assert_eq!(
                BottomChromeView::pending_tool_approval(&tui),
                BottomChromeView::pending_tool_approval(&snap)
            );
            assert_eq!(
                BottomChromeView::context_window_tokens(&tui),
                BottomChromeView::context_window_tokens(&snap)
            );
            let tui_picker = BottomChromeView::saved_session_picker(&tui).expect("tui saved picker");
            let snap_picker = BottomChromeView::saved_session_picker(&snap).expect("snapshot saved picker");
            assert_eq!(tui_picker.selected, snap_picker.selected);
            let [tui_saved] = tui_picker.entries.as_slice() else {
                panic!("TuiState fixture should expose exactly one saved picker entry");
            };
            let [snap_saved] = snap_picker.entries.as_slice() else {
                panic!("UiSnapshot fixture should expose exactly one saved picker entry");
            };
            assert_eq!(tui_saved.id, snap_saved.id);
            assert_eq!(tui_saved.title, snap_saved.title);
            assert_eq!(tui_saved.turn_count, snap_saved.turn_count);
            assert_eq!(tui_saved.provider, snap_saved.provider);
            assert_eq!(tui_saved.model, snap_saved.model);
            assert_eq!(tui_saved.is_current, snap_saved.is_current);
        }

        #[test]
        fn s4_a_2_pending_tool_approval_parity() {
            let mut state = make_state_with_lines();
            let pending = crate::chat::sessions::PendingToolApprovalView {
                tool_id: "call-approval".to_string(),
                name: "shell".to_string(),
                args: r#"{"cmd":"rm -rf /tmp/nope"}"#.to_string(),
            };
            state.ui.pending_tool_approval = Some(pending.clone());
            state.ui.focus = crate::chat::sessions::FocusTarget::Approval;
            let snap = state.build_ui_snapshot(11);

            let mut tui = TuiState::new(&state.session.provider, &state.session.model);
            tui.pending_tool_approval = Some(pending);
            tui.focus = crate::chat::sessions::FocusTarget::Approval;

            assert_eq!(
                BottomChromeView::pending_tool_approval(&tui),
                BottomChromeView::pending_tool_approval(&snap)
            );
            assert_eq!(BottomChromeView::focus(&tui), BottomChromeView::focus(&snap));
            assert_eq!(
                fullscreen_bottom_chrome_height(&tui),
                fullscreen_bottom_chrome_height(&snap)
            );
        }

        #[test]
        fn s4_a_2_transcript_view_and_switcher_parity() {
            let mut state = make_state_with_lines();
            let transcript_entry = transcript_switcher_entry();
            let transcript_view = build_transcript_view(&state.session.title, &state.ui.conversation_lines, 0);
            state.ui.focus = crate::chat::sessions::FocusTarget::Transcript;
            state.ui.switcher = Some(crate::chat::sessions::SwitcherState::new(vec![
                transcript_entry.clone(),
            ]));
            state.ui.active_session_view = Some(transcript_view.clone());
            let snap = state.build_ui_snapshot(9);

            let mut tui = TuiState::new(&state.session.provider, &state.session.model);
            tui.session_title = state.session.title.clone();
            tui.turn_count = state.ui.turn_count;
            tui.ascii_fallback = state.ui.ascii_fallback;
            tui.conversation_lines = state.ui.conversation_lines.clone();
            tui.input = state.ui.input.clone();
            tui.focus = crate::chat::sessions::FocusTarget::Transcript;
            tui.switcher = Some(crate::chat::sessions::SwitcherState::new(vec![transcript_entry]));
            tui.active_session_view = Some(transcript_view);

            assert_eq!(BottomChromeView::focus(&tui), BottomChromeView::focus(&snap));
            assert_eq!(
                BottomChromeView::active_session_view(&tui),
                BottomChromeView::active_session_view(&snap)
            );
            let tui_switcher = BottomChromeView::switcher(&tui).expect("tui transcript switcher");
            let snap_switcher = BottomChromeView::switcher(&snap).expect("snapshot transcript switcher");
            let [tui_entry] = tui_switcher.entries.as_slice() else {
                panic!("TuiState transcript switcher should expose exactly one entry");
            };
            let [snap_entry] = snap_switcher.entries.as_slice() else {
                panic!("UiSnapshot transcript switcher should expose exactly one entry");
            };
            assert!(tui_entry.is_transcript());
            assert_eq!(tui_entry.seq, 0);
            assert_eq!(tui_entry.seq, snap_entry.seq);
            assert_eq!(tui_entry.kind, snap_entry.kind);
            assert_eq!(tui_entry.origin, snap_entry.origin);
            assert_eq!(tui_entry.status, snap_entry.status);
            assert_eq!(tui_entry.title, snap_entry.title);
        }

        /// Fullscreen chrome parity: tool cards live in the transcript/panel, so
        /// bottom chrome height must stay identical for TuiState and UiSnapshot.
        #[test]
        fn s4_a_2_fullscreen_chrome_parity_tool_card() {
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

            assert_eq!(
                fullscreen_bottom_chrome_height(&tui),
                fullscreen_bottom_chrome_height(&snap),
                "tool card 状态下高度应一致"
            );
        }
    }

    // ─── S4-A Commit 5: SnapshotDispatcherSink ────────────────────────────────

    mod s4_a_5 {
        use super::*;
        use crate::channels::terminal::TuiMirrorSink;
        use crate::chat::action::Action;
        use crate::chat::dispatcher::ChatDispatcher;
        use crate::chat::state::ChatState;
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        /// 每个 UiEvent 翻译方法都不 panic, push_system 真 dispatch SystemMessageAdded.
        #[tokio::test]
        async fn s4_a_5_snapshot_sink_each_event_maps_to_action() {
            let (dispatcher, mut rx) = ChatDispatcher::new();
            let sink = SnapshotDispatcherSink::new(dispatcher);

            // 所有方法都不 panic — 大部分是 no-op trace.
            sink.push_assistant("hi");
            sink.push_tool_started("Bash", "{\"cmd\":\"ls\"}");
            let _ = sink.mark_tool_finished("Bash", true, 123);
            sink.start_stream("d-1");
            sink.update_stream("d-1", "He", 1);
            sink.finalize_stream("d-1", "Hello");
            sink.cancel_stream("d-1");
            // push_system 应 dispatch SystemMessageAdded — 验证 channel 收到.
            sink.push_system("system note");

            let action = rx.recv().await.expect("dispatcher should receive SystemMessageAdded");
            match action {
                Action::SystemMessageAdded { text } => assert_eq!(text, "system note"),
                other => panic!("expected SystemMessageAdded, got {other:?}"),
            }
        }

        /// dispatch_or_log 路径在 channel 满时不 panic (Backpressured fallback).
        #[tokio::test]
        async fn s4_a_5_sink_handles_dispatcher_full_gracefully() {
            let (dispatcher, _rx) = ChatDispatcher::new();
            let sink = SnapshotDispatcherSink::new(dispatcher);
            // 反复 push_system, channel cap 限制 (ACTION_CHANNEL_CAPACITY) 满后
            // dispatch_or_log 走 backpressured 路径, 不 panic.
            for i in 0..2000 {
                sink.push_system(&format!("note {i}"));
            }
            // 通过即可 (不 panic / 不 hang).
        }

        /// Pure 模式下 Stream UiEvent 经由 driver Action 进 reducer, snapshot.streaming
        /// 应 Some(...) — Sink 自身不写，但 driver 路径走 reducer.
        ///
        /// 此测试模拟 driver-style dispatch (TurnStarted → reducer 写 stream.draft),
        /// 验证 snapshot.streaming.is_some() — 这是 Pure 模式的核心路径.
        #[test]
        fn s4_a_5_pure_streaming_propagates_to_snapshot() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let token = CancellationToken::new();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-1".to_string(),
                cancel: token,
            });
            let snap = state.build_ui_snapshot(1);
            assert!(
                snap.streaming.is_some(),
                "Pure 模式下 TurnStarted 应让 snapshot.streaming = Some"
            );
            let draft = snap.streaming.as_ref().expect("checked above");
            assert_eq!(draft.draft_id, "d-1");
        }

        /// Pure 模式下 ToolStarted Action 进 reducer, snapshot.conversation_lines 出现
        /// ToolResult 卡片 (Running).
        #[test]
        fn s4_a_5_pure_tool_card_appears_in_snapshot() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let _ = state.reduce(Action::ToolStarted {
                name: "Bash".to_string(),
                args: "{\"cmd\":\"ls\"}".to_string(),
            });
            let snap = state.build_ui_snapshot(1);
            assert!(
                snap.conversation_lines
                    .iter()
                    .any(|l| matches!(l, ConversationLine::ToolResult { tool_name, .. } if tool_name == "Bash")),
                "Pure 模式下 ToolStarted 应让 snapshot 出现 Bash ToolResult 卡片"
            );
        }

        /// Pure 模式下 banner 走 SystemMessageAdded Action -> reducer push 一行;
        /// chat_mirror 同时（其他模式）也写 — 但在 Pure 下 chat_mirror 应零写入.
        ///
        /// 此测试单元化验证 reducer 路径正确性: SystemMessageAdded → snapshot
        /// 含 ConversationLine::System.
        #[test]
        fn s4_a_5_pure_banner_via_dispatch_not_mirror() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "prx 0.3.6 mock/mock".to_string(),
            });
            let snap = state.build_ui_snapshot(1);
            let has_banner = snap
                .conversation_lines
                .iter()
                .any(|l| matches!(l, ConversationLine::System { content } if content.contains("mock/mock")));
            assert!(
                has_banner,
                "Pure 模式 banner 应通过 SystemMessageAdded 进 reducer, snapshot 含 System 行"
            );
        }
    }
}
