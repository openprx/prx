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
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::loop_::ChatMode;
use crate::chat::action::{MainQueueStatus, ProviderWorkerRowState, ProviderWorkerStatus, ProviderWorkerStatusRow};
use crate::chat::commands::{CommandArgCandidate, CommandArgSource, CommandSpec, command_specs};
use crate::chat::session::MainSessionTokenUsageSummary;
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
    /// In-flight visible drafts keyed by real provider worker sequence.
    pub visible_streaming_drafts: Arc<Vec<crate::chat::state::VisibleStreamingDraftView>>,
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
    /// UI-only bottom-strip selection for direct Alt+arrow navigation. This is
    /// deliberately separate from [`crate::chat::sessions::FocusTarget`]: it
    /// highlights a strip entry but never changes input routing until Alt+Enter
    /// reuses the existing synthetic `/attach N` path.
    pub strip_selection: Option<u64>,
    /// Open slash-command menu overlay, or `None` when the cursor is not inside
    /// a leading command token.
    pub slash_menu: Option<SlashMenuState>,
    /// Cached background-session snapshot for the switcher, refreshed by the
    /// chat main loop's 1s sessions tick. The key thread reads this (it cannot
    /// run async registry queries) when opening the switcher with Ctrl+G.
    pub sessions_cache: Vec<crate::chat::sessions::SwitcherEntry>,
    /// Main-session input backlog status for orchestration observation.
    pub main_queue_status: MainQueueStatus,
    /// Main-session provider worker status for orchestration observation.
    pub provider_worker_status: ProviderWorkerStatus,
    /// Cached saved chat sessions for `/resume` slash-menu argument candidates.
    pub saved_sessions_cache: Vec<crate::chat::session::SavedSessionPickerEntry>,
    /// Enumerable model candidates grouped by provider for slash-menu drill-down.
    pub provider_model_catalog: Vec<SlashProviderModelCatalog>,
    /// Security-filtered `@path` completion candidates sourced by the TUI loop.
    pub at_path_candidates: Vec<AtPathCandidate>,
    /// P7c saved chat-session history picker. Distinct from the child-TUI
    /// Ctrl+G switcher.
    pub saved_session_picker: Option<crate::chat::session::SavedSessionPickerState>,
    /// P2 active line-session viewport snapshot. `None` when main chat or PTY
    /// handoff owns the visible surface.
    pub active_session_view: Option<crate::chat::sessions::ActiveSessionView>,
    /// P6c1 foreground tool approval prompt. Display-only; approving/denying is
    /// returned to the dispatcher as `ToolApprovalReceived`.
    pub pending_tool_approval: Option<crate::chat::sessions::PendingToolApprovalView>,
    /// Current planned context usage for UI-only status budget display.
    pub context_used_tokens: Option<usize>,
    /// Effective context window for UI-only status budget display.
    pub context_window_tokens: Option<usize>,
    /// Main-session cumulative token/cost summary.
    pub token_usage_summary: MainSessionTokenUsageSummary,
    /// First half of the `Ctrl+X Ctrl+E` external-editor chord.
    pub external_editor_prefix_armed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashMenuState {
    pub filter: String,
    pub entries: Vec<SlashMenuEntry>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashMenuEntry {
    pub label: String,
    pub args_hint: String,
    pub description: String,
    pub insert_text: String,
    pub append_space: bool,
}

pub use crate::chat::slash_types::{AtPathCandidate, SlashModelCandidate, SlashProviderModelCatalog};

pub(crate) struct SlashMenuSources<'a> {
    pub live_sessions: &'a [crate::chat::sessions::SwitcherEntry],
    pub saved_sessions: &'a [crate::chat::session::SavedSessionPickerEntry],
    pub provider_model_catalog: &'a [SlashProviderModelCatalog],
    pub at_path_candidates: &'a [AtPathCandidate],
    pub current_provider: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SlashCursorContext {
    Command {
        filter: String,
    },
    Argument {
        command: String,
        arg_index: usize,
        filter: String,
        previous_args: Vec<String>,
    },
    AtPath {
        filter: String,
    },
}

impl SlashMenuState {
    #[must_use]
    pub fn new(filter: &str) -> Self {
        Self {
            filter: filter.to_string(),
            entries: filtered_command_entries(filter),
            selected: 0,
        }
    }

    #[must_use]
    pub fn new_with_entries(filter: &str, entries: Vec<SlashMenuEntry>) -> Self {
        Self {
            filter: filter.to_string(),
            entries,
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
        if self.filter == filter {
            self.clamp_selected();
            return;
        }
        self.filter.clear();
        self.filter.push_str(filter);
        self.entries = filtered_command_entries(filter);
        self.selected = 0;
    }

    pub fn refresh_with_entries(&mut self, filter: &str, entries: Vec<SlashMenuEntry>) {
        if self.filter == filter && self.entries == entries {
            self.clamp_selected();
            return;
        }
        self.filter.clear();
        self.filter.push_str(filter);
        self.entries = entries;
        self.selected = 0;
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
    pub fn selected_entry(&self) -> Option<&SlashMenuEntry> {
        self.entries.get(self.selected)
    }
}

impl SlashMenuEntry {
    fn from_command(spec: CommandSpec) -> Self {
        Self {
            label: spec.name.to_string(),
            args_hint: spec.args_hint.to_string(),
            description: spec.description.to_string(),
            insert_text: spec.name.to_string(),
            append_space: true,
        }
    }

    fn argument(value: impl Into<String>, description: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            args_hint: String::new(),
            description: description.into(),
            insert_text: value,
            append_space: true,
        }
    }

    fn at_path(candidate: &AtPathCandidate) -> Self {
        Self {
            label: candidate.path.clone(),
            args_hint: String::new(),
            description: if candidate.is_dir {
                "directory".to_string()
            } else {
                "file".to_string()
            },
            insert_text: format!("@{}", candidate.path),
            append_space: !candidate.is_dir,
        }
    }
}

#[must_use]
pub fn slash_provider_model_catalog_from_config(config: &crate::config::Config) -> Vec<SlashProviderModelCatalog> {
    let mut catalog: Vec<SlashProviderModelCatalog> = Vec::new();
    if let (Some(provider), Some(model)) = (config.default_provider.as_deref(), config.default_model.as_deref()) {
        push_model_candidate(&mut catalog, provider, model, "Configured default model");
    }
    for route in &config.model_routes {
        push_model_candidate(
            &mut catalog,
            &route.provider,
            &route.model,
            format!("Model route hint: {}", route.hint),
        );
    }
    for model in &config.router.models {
        push_model_candidate(&mut catalog, &model.provider, &model.model_id, "Router model candidate");
    }
    catalog
}

fn push_model_candidate(
    catalog: &mut Vec<SlashProviderModelCatalog>,
    provider: &str,
    model: &str,
    description: impl Into<String>,
) {
    let provider = provider.trim();
    let model = model.trim();
    if provider.is_empty() || model.is_empty() {
        return;
    }
    let description = description.into();
    if let Some(entry) = catalog
        .iter_mut()
        .find(|entry| entry.provider.eq_ignore_ascii_case(provider))
    {
        if !entry.models.iter().any(|candidate| candidate.name == model) {
            entry.models.push(SlashModelCandidate {
                name: model.to_string(),
                description,
            });
        }
        return;
    }
    catalog.push(SlashProviderModelCatalog {
        provider: provider.to_string(),
        models: vec![SlashModelCandidate {
            name: model.to_string(),
            description,
        }],
    });
}

fn filtered_command_entries(filter: &str) -> Vec<SlashMenuEntry> {
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
        .map(SlashMenuEntry::from_command)
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

const TOOL_EXPANDED_OUTPUT_MAX_LINES: usize = 40;
const TOOL_EXPANDED_OUTPUT_LINE_MAX_CHARS: usize = 240;
const TOOL_FOLDED_RESULT_PREVIEW_LINES: usize = 3;
const TOOL_FOLDED_RESULT_PREVIEW_CHARS: usize = 180;
const TOOL_ERROR_REASON_MAX_CHARS: usize = 120;
const TOOL_ARG_VALUE_MAX_CHARS: usize = 80;

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
const PASTE_FOLD_LINE_THRESHOLD: usize = 5;
const PASTE_FOLD_BYTE_THRESHOLD: usize = 1024;
const PASTE_CHIP_SENTINEL_START: char = '\u{E000}';
const PASTE_CHIP_SENTINEL_END: char = '\u{E001}';

/// Maximum number of submitted entries kept in the history ring.
pub const INPUT_HISTORY_CAPACITY: usize = 200;

/// Synthetic display id for the read-only transcript child TUI.
pub const TRANSCRIPT_SESSION_SEQ: u64 = 0;
/// Synthetic display id for the read-only diff child TUI.
pub const DIFF_SESSION_SEQ: u64 = 0;
/// Synthetic switcher-only provider worker rows live outside the managed session
/// sequence space so Enter can route to `/workers` instead of `/attach`.
const PROVIDER_WORKER_SWITCHER_SEQ_BASE: u64 = u64::MAX - 10_000;
const PROVIDER_WORKER_SWITCHER_KIND: &str = "worker";

/// Bounded transcript viewport size. Conversation history remains authoritative
/// elsewhere; the child TUI is only a scrollable display snapshot.
pub const TRANSCRIPT_MAX_LINES: usize = 400;
const ASSISTANT_MARKDOWN_CACHE_CAPACITY: usize = 512;
const STREAMING_MARKDOWN_HIGHLIGHT_MAX_BYTES: usize = 32 * 1024;

static ASSISTANT_MARKDOWN_CACHE: LazyLock<Mutex<AssistantMarkdownCache>> =
    LazyLock::new(|| Mutex::new(AssistantMarkdownCache::new(ASSISTANT_MARKDOWN_CACHE_CAPACITY)));

#[derive(Debug)]
struct AssistantMarkdownCache {
    capacity: usize,
    order: VecDeque<String>,
    entries: HashMap<String, Arc<Vec<Line<'static>>>>,
}

impl AssistantMarkdownCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::new(),
            entries: HashMap::new(),
        }
    }

    fn get_or_render(&mut self, content: &str) -> Arc<Vec<Line<'static>>> {
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
    fn clear(&mut self) {
        self.order.clear();
        self.entries.clear();
    }
}

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
/// - `Ctrl+R` — reverse-searches submitted input history.
/// - `Ctrl+X Ctrl+E` — opens the current draft in an external editor.
/// - `Ctrl+C` — interrupt the current turn (caller cancels in-flight work)
/// - `Ctrl+D` — EOF when the input buffer is logically empty
/// - everything else — forwarded to the input box; submissions surface as
///   `Submitted(text)`.
///
/// Keeping the dispatch separate from the actual I/O loop lets us unit-test
/// the keybindings without spinning up a terminal.
#[derive(Debug, Clone, PartialEq)]
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
    /// Phase B: the bottom strip's UI-only selection changed. `None` clears the
    /// highlight without changing input-routing focus.
    StripSelectionChanged { selected: Option<u64> },
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
    /// P6Y: open the read-only main provider worker detail TUI.
    OpenProviderWorkerView { sequence: u64 },
    /// P6Y: close the read-only main provider worker detail TUI.
    CloseProviderWorkerView,
    /// P6c2: close the read-only diff child TUI.
    CloseDiffViewer,
    /// P6b2: open the current draft in an external editor.
    ExternalEditorRequested,
    /// P6c1: resolve the foreground tool approval prompt.
    ToolApprovalDecision { tool_id: String, approved: bool },
    /// P8: cycle the in-session chat mode via Shift+Tab.
    ModeChanged(ChatMode),
}

pub(crate) fn sync_slash_menu_for_sources(
    input: &TuiInput,
    slash_menu: &mut Option<SlashMenuState>,
    sources: SlashMenuSources<'_>,
) {
    let Some(context) = input.completion_cursor_context() else {
        *slash_menu = None;
        return;
    };
    match context {
        SlashCursorContext::Command { filter } => {
            let entries = filtered_command_entries(&filter);
            if entries.is_empty() {
                *slash_menu = None;
            } else if let Some(menu) = slash_menu.as_mut() {
                menu.refresh(&filter);
            } else {
                *slash_menu = Some(SlashMenuState::new_with_entries(&filter, entries));
            }
        }
        SlashCursorContext::Argument {
            command,
            arg_index,
            filter,
            previous_args,
        } => {
            let entries = argument_candidate_entries(&command, arg_index, &filter, &previous_args, sources);
            if entries.is_empty() {
                *slash_menu = None;
            } else if let Some(menu) = slash_menu.as_mut() {
                menu.refresh_with_entries(&filter, entries);
            } else {
                *slash_menu = Some(SlashMenuState::new_with_entries(&filter, entries));
            }
        }
        SlashCursorContext::AtPath { filter } => {
            let entries = at_path_candidate_entries(&filter, sources.at_path_candidates);
            if entries.is_empty() {
                *slash_menu = None;
            } else if let Some(menu) = slash_menu.as_mut() {
                menu.refresh_with_entries(&filter, entries);
            } else {
                *slash_menu = Some(SlashMenuState::new_with_entries(&filter, entries));
            }
        }
    }
}

fn argument_candidate_entries(
    command: &str,
    arg_index: usize,
    filter: &str,
    previous_args: &[String],
    sources: SlashMenuSources<'_>,
) -> Vec<SlashMenuEntry> {
    let Some(spec) = command_specs()
        .iter()
        .find(|spec| spec.name == command || spec.aliases.iter().any(|alias| *alias == command))
        .copied()
    else {
        return Vec::new();
    };
    let source = if spec.name == "/provider" && arg_index == 1 {
        CommandArgSource::ProviderModels
    } else if arg_index == 0 {
        spec.arg.source
    } else {
        CommandArgSource::None
    };
    let needle = filter.to_ascii_lowercase();
    let mut entries = match source {
        CommandArgSource::None | CommandArgSource::FreeText => Vec::new(),
        CommandArgSource::Static(candidates) => static_candidate_entries(candidates),
        CommandArgSource::Themes => theme_candidate_entries(),
        CommandArgSource::LiveSessions => live_session_candidate_entries(sources.live_sessions),
        CommandArgSource::SavedSessions => saved_session_candidate_entries(sources.saved_sessions),
        CommandArgSource::Providers => provider_candidate_entries(),
        CommandArgSource::CurrentProviderModels => {
            model_candidate_entries(sources.provider_model_catalog, sources.current_provider)
        }
        CommandArgSource::ProviderModels => {
            let provider = previous_args.first().map_or("", String::as_str);
            model_candidate_entries(sources.provider_model_catalog, provider)
        }
    };
    if needle.is_empty() {
        return entries;
    }
    entries.retain(|entry| {
        entry.label.to_ascii_lowercase().contains(&needle)
            || entry.description.to_ascii_lowercase().contains(&needle)
            || entry.insert_text.to_ascii_lowercase().contains(&needle)
    });
    entries
}

fn static_candidate_entries(candidates: &[CommandArgCandidate]) -> Vec<SlashMenuEntry> {
    candidates
        .iter()
        .map(|candidate| SlashMenuEntry::argument(candidate.value, candidate.description))
        .collect()
}

fn theme_candidate_entries() -> Vec<SlashMenuEntry> {
    ["dark", "light", "monokai"]
        .into_iter()
        .map(|name| SlashMenuEntry::argument(name, "Chat color theme"))
        .collect()
}

fn live_session_candidate_entries(entries: &[crate::chat::sessions::SwitcherEntry]) -> Vec<SlashMenuEntry> {
    entries
        .iter()
        .filter(|entry| !entry.is_transcript())
        .map(|entry| {
            SlashMenuEntry::argument(
                format!("#{}", entry.seq),
                format!("{} {} - {}", entry.kind, entry.status, entry.title),
            )
        })
        .collect()
}

fn saved_session_candidate_entries(entries: &[crate::chat::session::SavedSessionPickerEntry]) -> Vec<SlashMenuEntry> {
    let mut out = vec![SlashMenuEntry::argument("last", "Most recently saved session")];
    out.extend(entries.iter().map(|entry| {
        let title = if entry.title.is_empty() {
            "(untitled)".to_string()
        } else {
            entry.title.clone()
        };
        SlashMenuEntry::argument(
            entry.id.clone(),
            format!("{} turns - {} / {}", entry.turn_count, title, entry.model),
        )
    }));
    out
}

fn provider_candidate_entries() -> Vec<SlashMenuEntry> {
    crate::providers::list_providers()
        .into_iter()
        .map(|provider| {
            let mut description = provider.display_name.to_string();
            if provider.local {
                description.push_str(" - local");
            }
            SlashMenuEntry::argument(provider.name, description)
        })
        .collect()
}

fn model_candidate_entries(catalog: &[SlashProviderModelCatalog], provider: &str) -> Vec<SlashMenuEntry> {
    let provider = provider.trim();
    if provider.is_empty() {
        return Vec::new();
    }
    catalog
        .iter()
        .find(|entry| entry.provider.eq_ignore_ascii_case(provider))
        .map_or_else(Vec::new, |entry| {
            entry
                .models
                .iter()
                .map(|model| SlashMenuEntry::argument(model.name.clone(), model.description.clone()))
                .collect()
        })
}

fn at_path_candidate_entries(filter: &str, candidates: &[AtPathCandidate]) -> Vec<SlashMenuEntry> {
    let needle = filter.to_ascii_lowercase();
    candidates
        .iter()
        .filter(|candidate| {
            if needle.is_empty() {
                return true;
            }
            let path = candidate.path.to_ascii_lowercase();
            path.contains(&needle) || fuzzy_path_match(&path, &needle)
        })
        .take(50)
        .map(SlashMenuEntry::at_path)
        .collect()
}

pub(crate) fn fuzzy_path_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut chars = needle.chars();
    let Some(mut wanted) = chars.next() else {
        return true;
    };
    for ch in haystack.chars() {
        if ch == wanted {
            if let Some(next) = chars.next() {
                wanted = next;
            } else {
                return true;
            }
        }
    }
    false
}

pub(crate) fn dispatch_slash_menu_key_with_sources(
    input: &mut TuiInput,
    slash_menu: &mut Option<SlashMenuState>,
    key: KeyEvent,
    sources: SlashMenuSources<'_>,
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
        if let Some(entry) = slash_menu.as_ref().and_then(SlashMenuState::selected_entry).cloned() {
            match input.completion_cursor_context() {
                Some(SlashCursorContext::Command { .. }) => {
                    if key.code == KeyCode::Enter && entry.args_hint.is_empty() && input.slash_command_suffix_is_empty()
                    {
                        input.replace_slash_command_token(&entry.insert_text, false);
                        let text = input.text();
                        input.record_history(text.clone());
                        input.clear();
                        *slash_menu = None;
                        return KeyDispatch::Submitted(text);
                    }
                    input.replace_slash_command_token(&entry.insert_text, true);
                }
                Some(SlashCursorContext::Argument { .. }) => {
                    input.replace_slash_argument_token(&entry.insert_text, entry.append_space);
                }
                Some(SlashCursorContext::AtPath { .. }) => {
                    input.replace_at_path_token(&entry.insert_text, entry.append_space);
                }
                None => {}
            }
        }
        sync_slash_menu_for_sources(input, slash_menu, sources);
        return KeyDispatch::Consumed;
    }

    if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
        if matches!(
            input.completion_cursor_context(),
            Some(SlashCursorContext::Command { .. })
        ) {
            input.clear();
        }
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
                sync_slash_menu_for_sources(input, slash_menu, sources);
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

fn switcher_entries_with_transcript(
    entries: &[crate::chat::sessions::SwitcherEntry],
) -> Vec<crate::chat::sessions::SwitcherEntry> {
    let mut out = Vec::with_capacity(entries.len().saturating_add(1));
    out.push(transcript_switcher_entry());
    out.extend(entries.iter().filter(|entry| !entry.is_transcript()).cloned());
    out
}

fn switcher_entries_with_transcript_and_workers(
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

fn provider_worker_switcher_entries(
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

fn provider_worker_switcher_entry(row: &ProviderWorkerStatusRow) -> crate::chat::sessions::SwitcherEntry {
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

fn is_provider_worker_switcher_entry(entry: &crate::chat::sessions::SwitcherEntry) -> bool {
    entry.kind == PROVIDER_WORKER_SWITCHER_KIND
}

const fn provider_worker_sequence_from_switcher_seq(seq: u64) -> Option<u64> {
    if seq >= PROVIDER_WORKER_SWITCHER_SEQ_BASE {
        Some(seq.saturating_sub(PROVIDER_WORKER_SWITCHER_SEQ_BASE))
    } else {
        None
    }
}

fn switcher_entry_display_id(entry: &crate::chat::sessions::SwitcherEntry) -> String {
    if is_provider_worker_switcher_entry(entry) {
        format!("w#{}", entry.seq.saturating_sub(PROVIDER_WORKER_SWITCHER_SEQ_BASE))
    } else {
        format!("#{}", entry.seq)
    }
}

fn focus_active_entry_seq(focus: crate::chat::sessions::FocusTarget) -> Option<u64> {
    focus.session_seq().or_else(|| {
        focus
            .worker_sequence()
            .map(|sequence| PROVIDER_WORKER_SWITCHER_SEQ_BASE.saturating_add(sequence))
    })
}

pub(crate) fn strip_selection_index(
    entries: &[crate::chat::sessions::SwitcherEntry],
    selected: Option<u64>,
    focus: crate::chat::sessions::FocusTarget,
) -> Option<usize> {
    selected
        .and_then(|seq| entries.iter().position(|entry| entry.seq == seq))
        .or_else(|| focus_active_entry_seq(focus).and_then(|seq| entries.iter().position(|entry| entry.seq == seq)))
}

const MAIN_SESSION_SELECTION_SEQ: u64 = 0;

fn bottom_list_selection_index(
    entries: &[crate::chat::sessions::SwitcherEntry],
    selected: Option<u64>,
    focus: crate::chat::sessions::FocusTarget,
) -> usize {
    if selected == Some(MAIN_SESSION_SELECTION_SEQ) {
        return 0;
    }
    if let Some(idx) = selected.and_then(|seq| entries.iter().position(|entry| entry.seq == seq)) {
        return idx.saturating_add(1);
    }
    focus_active_entry_seq(focus)
        .and_then(|seq| entries.iter().position(|entry| entry.seq == seq))
        .map_or(0, |idx| idx.saturating_add(1))
}

fn bottom_list_seq_at(entries: &[crate::chat::sessions::SwitcherEntry], idx: usize) -> Option<u64> {
    if idx == 0 {
        Some(MAIN_SESSION_SELECTION_SEQ)
    } else {
        entries.get(idx.saturating_sub(1)).map(|entry| entry.seq)
    }
}

fn move_bottom_list_selection(
    entries: &[crate::chat::sessions::SwitcherEntry],
    selected: Option<u64>,
    focus: crate::chat::sessions::FocusTarget,
    direction: crate::chat::sessions::SessionDirection,
) -> Option<u64> {
    if entries.is_empty() {
        return None;
    }
    let total = entries.len().saturating_add(1);
    let current = bottom_list_selection_index(entries, selected, focus).min(total.saturating_sub(1));
    let target_idx = match direction {
        crate::chat::sessions::SessionDirection::Previous => {
            current.checked_sub(1).unwrap_or_else(|| total.saturating_sub(1))
        }
        crate::chat::sessions::SessionDirection::Next => {
            let next = current.saturating_add(1);
            if next >= total { 0 } else { next }
        }
    };
    bottom_list_seq_at(entries, target_idx)
}

fn bottom_chrome_session_entries(
    entries: &[crate::chat::sessions::SwitcherEntry],
    focus: crate::chat::sessions::FocusTarget,
) -> Vec<crate::chat::sessions::SwitcherEntry> {
    let active_seq = focus_active_entry_seq(focus);
    entries
        .iter()
        .filter(|entry| entry.is_transcript() || !entry.is_terminal() || active_seq == Some(entry.seq))
        .cloned()
        .collect()
}

fn bottom_chrome_session_entries_with_workers(
    entries: &[crate::chat::sessions::SwitcherEntry],
    workers: ProviderWorkerStatus,
    focus: crate::chat::sessions::FocusTarget,
) -> Vec<crate::chat::sessions::SwitcherEntry> {
    let mut out = provider_worker_switcher_entries(workers, focus);
    out.extend(bottom_chrome_session_entries(entries, focus));
    out
}

pub(crate) fn move_strip_selection(
    entries: &[crate::chat::sessions::SwitcherEntry],
    selected: Option<u64>,
    focus: crate::chat::sessions::FocusTarget,
    direction: crate::chat::sessions::SessionDirection,
) -> Option<u64> {
    if entries.is_empty() {
        return None;
    }
    let current = strip_selection_index(entries, selected, focus);
    let target_idx = match (current, direction) {
        (Some(idx), crate::chat::sessions::SessionDirection::Previous) => {
            idx.checked_sub(1).unwrap_or_else(|| entries.len().saturating_sub(1))
        }
        (Some(idx), crate::chat::sessions::SessionDirection::Next) => {
            let next = idx.saturating_add(1);
            if next >= entries.len() { 0 } else { next }
        }
        (None, crate::chat::sessions::SessionDirection::Previous) => entries.len().saturating_sub(1),
        (None, crate::chat::sessions::SessionDirection::Next) => 0,
    };
    entries.get(target_idx).map(|entry| entry.seq)
}

pub(crate) fn selected_strip_entry<'a>(
    entries: &'a [crate::chat::sessions::SwitcherEntry],
    selected: Option<u64>,
) -> Option<&'a crate::chat::sessions::SwitcherEntry> {
    let seq = selected?;
    entries.iter().find(|entry| entry.seq == seq)
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

const PROVIDER_WORKER_IO_MAX_SOURCE_ITEMS: usize = 12;
const PROVIDER_WORKER_IO_LINE_MAX_CHARS: u16 = 180;
const PROVIDER_WORKER_IO_RESULT_LINES: usize = 3;

fn provider_worker_io_clip(text: &str) -> String {
    truncate_chars_with_ellipsis(text.trim(), PROVIDER_WORKER_IO_LINE_MAX_CHARS, false)
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
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        state.clear_pending_tool_approval();
        return KeyDispatch::InterruptTurn;
    }
    if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL && state.input.is_empty() {
        return KeyDispatch::Exit;
    }
    // P7c: the saved chat-session picker has top overlay priority. It is
    // distinct from the child-TUI Ctrl+G switcher and captures all keys while
    // open so navigation cannot leak into input history or child switching.
    if state.saved_session_picker.is_some() {
        return dispatch_saved_session_picker_key(key, state);
    }
    if state.slash_menu.is_some() {
        let sources = SlashMenuSources {
            live_sessions: &state.sessions_cache,
            saved_sessions: &state.saved_sessions_cache,
            provider_model_catalog: &state.provider_model_catalog,
            at_path_candidates: &state.at_path_candidates,
            current_provider: &state.provider,
        };
        return dispatch_slash_menu_key_with_sources(&mut state.input, &mut state.slash_menu, key, sources);
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
        let entries = switcher_entries_with_transcript_and_workers(
            &state.sessions_cache,
            state.provider_worker_status.clone(),
            state.focus,
        );
        state.switcher = Some(crate::chat::sessions::SwitcherState::new(entries.clone()));
        return KeyDispatch::SwitcherOpened { entries };
    }
    if state.input.is_empty()
        && key.modifiers == KeyModifiers::NONE
        && matches!(state.focus, crate::chat::sessions::FocusTarget::Main)
    {
        let bottom_entries = bottom_chrome_session_entries_with_workers(
            &state.sessions_cache,
            state.provider_worker_status.clone(),
            state.focus,
        );
        let direction = match key.code {
            KeyCode::Left | KeyCode::Up => Some(crate::chat::sessions::SessionDirection::Previous),
            KeyCode::Right | KeyCode::Down => Some(crate::chat::sessions::SessionDirection::Next),
            _ => None,
        };
        if let Some(direction) = direction {
            if bottom_entries.is_empty() {
                return KeyDispatch::Consumed;
            }
            let selected = move_bottom_list_selection(&bottom_entries, state.strip_selection, state.focus, direction);
            state.strip_selection = selected;
            return KeyDispatch::StripSelectionChanged { selected };
        }
        if key.code == KeyCode::Enter
            && let Some(selected) = state.strip_selection
        {
            if selected == MAIN_SESSION_SELECTION_SEQ {
                state.strip_selection = None;
                return KeyDispatch::Consumed;
            }
            if let Some(entry) = selected_strip_entry(&bottom_entries, Some(selected)) {
                state.strip_selection = None;
                if is_provider_worker_switcher_entry(entry)
                    && let Some(sequence) = provider_worker_sequence_from_switcher_seq(entry.seq)
                {
                    return KeyDispatch::OpenProviderWorkerView { sequence };
                }
                return KeyDispatch::AttachSession { seq: entry.seq };
            }
            state.strip_selection = None;
            state.push_system_message("session gone");
            return KeyDispatch::Consumed;
        }
    }
    if key.modifiers == KeyModifiers::ALT {
        let direction = match key.code {
            KeyCode::Left | KeyCode::Up => Some(crate::chat::sessions::SessionDirection::Previous),
            KeyCode::Right | KeyCode::Down => Some(crate::chat::sessions::SessionDirection::Next),
            _ => None,
        };
        if let Some(direction) = direction {
            let entries = bottom_chrome_session_entries_with_workers(
                &state.sessions_cache,
                state.provider_worker_status.clone(),
                state.focus,
            );
            let selected = move_bottom_list_selection(&entries, state.strip_selection, state.focus, direction);
            state.strip_selection = selected;
            return KeyDispatch::StripSelectionChanged { selected };
        }
        if key.code == KeyCode::Enter {
            if let Some(selected) = state.strip_selection {
                let entries = bottom_chrome_session_entries_with_workers(
                    &state.sessions_cache,
                    state.provider_worker_status.clone(),
                    state.focus,
                );
                if selected == MAIN_SESSION_SELECTION_SEQ {
                    state.strip_selection = None;
                    return KeyDispatch::RequestDetach;
                }
                if let Some(entry) = selected_strip_entry(&entries, Some(selected)) {
                    state.strip_selection = None;
                    if is_provider_worker_switcher_entry(entry)
                        && let Some(sequence) = provider_worker_sequence_from_switcher_seq(entry.seq)
                    {
                        return KeyDispatch::OpenProviderWorkerView { sequence };
                    }
                    return KeyDispatch::AttachSession { seq: entry.seq };
                }
                state.strip_selection = None;
                state.push_system_message("session gone");
                return KeyDispatch::Consumed;
            }
        }
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
            KeyCode::Left | KeyCode::Up => Some(crate::chat::sessions::SessionDirection::Previous),
            KeyCode::Right | KeyCode::Down => Some(crate::chat::sessions::SessionDirection::Next),
            _ => None,
        };
        if let Some(direction) = direction {
            let bottom_entries = bottom_chrome_session_entries_with_workers(
                &state.sessions_cache,
                state.provider_worker_status.clone(),
                state.focus,
            );
            let selected = move_bottom_list_selection(&bottom_entries, None, state.focus, direction);
            return match selected {
                Some(MAIN_SESSION_SELECTION_SEQ) => KeyDispatch::RequestDetach,
                Some(seq) => {
                    if let Some(entry) = selected_strip_entry(&bottom_entries, Some(seq))
                        && is_provider_worker_switcher_entry(entry)
                        && let Some(sequence) = provider_worker_sequence_from_switcher_seq(entry.seq)
                    {
                        return KeyDispatch::OpenProviderWorkerView { sequence };
                    }
                    if seq != current_seq {
                        return KeyDispatch::SwitchSession { seq };
                    }
                    KeyDispatch::Consumed
                }
                None => KeyDispatch::Consumed,
            };
        }
    }
    if matches!(state.focus, crate::chat::sessions::FocusTarget::Worker { .. })
        && state.input.is_empty()
        && key.modifiers == KeyModifiers::NONE
    {
        let direction = match key.code {
            KeyCode::Left | KeyCode::Up => Some(crate::chat::sessions::SessionDirection::Previous),
            KeyCode::Right | KeyCode::Down => Some(crate::chat::sessions::SessionDirection::Next),
            _ => None,
        };
        if let Some(direction) = direction {
            let bottom_entries = bottom_chrome_session_entries_with_workers(
                &state.sessions_cache,
                state.provider_worker_status.clone(),
                state.focus,
            );
            let selected = move_bottom_list_selection(&bottom_entries, None, state.focus, direction);
            return match selected {
                Some(MAIN_SESSION_SELECTION_SEQ) => KeyDispatch::CloseProviderWorkerView,
                Some(seq) => {
                    if let Some(entry) = selected_strip_entry(&bottom_entries, Some(seq)) {
                        if is_provider_worker_switcher_entry(entry)
                            && let Some(sequence) = provider_worker_sequence_from_switcher_seq(entry.seq)
                        {
                            return KeyDispatch::OpenProviderWorkerView { sequence };
                        }
                    }
                    KeyDispatch::SwitchSession { seq }
                }
                None => KeyDispatch::Consumed,
            };
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
        if state.strip_selection.take().is_some() {
            return KeyDispatch::StripSelectionChanged { selected: None };
        }
        use crate::chat::sessions::focus::{EscAction, resolve_esc};
        match resolve_esc(state.input.is_empty(), state.focus, false, state.streaming.is_some()) {
            EscAction::CancelGenerating => return KeyDispatch::InterruptTurn,
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
            EscAction::CloseWorker => return KeyDispatch::CloseProviderWorkerView,
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
        crate::chat::sessions::FocusTarget::Transcript
            | crate::chat::sessions::FocusTarget::Diff
            | crate::chat::sessions::FocusTarget::Worker { .. }
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
            if is_provider_worker_switcher_entry(&entry) {
                return provider_worker_sequence_from_switcher_seq(entry.seq)
                    .map_or(KeyDispatch::SwitcherClosed, |sequence| {
                        KeyDispatch::OpenProviderWorkerView { sequence }
                    });
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
    pending_draft: Option<InputDraftSnapshot>,
    /// Original payloads hidden behind folded paste chips in `lines`.
    paste_chips: Vec<PasteChip>,
    /// Monotonic display id for folded paste chips in this draft.
    next_paste_chip_id: usize,
    /// True when text was ignored because the input reached INPUT_MAX_BYTES.
    pub truncated: bool,
    /// Active reverse history search state (`Ctrl+R`), if any.
    reverse_search: Option<ReverseSearchState>,
}

/// Ephemeral reverse-search state for the input history ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverseSearchState {
    /// Draft buffer before the search started; restored on Esc.
    saved_draft: InputDraftSnapshot,
    /// User-entered incremental search query.
    pub query: String,
    /// Currently selected history entry.
    pub match_pos: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PasteChip {
    token: String,
    placeholder: String,
    content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputDraftSnapshot {
    lines: Vec<String>,
    paste_chips: Vec<PasteChip>,
    next_paste_chip_id: usize,
    truncated: bool,
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
            paste_chips: Vec::new(),
            next_paste_chip_id: 1,
            truncated: false,
            reverse_search: None,
        }
    }

    /// Joined buffer contents (lines separated by '\n').
    pub fn text(&self) -> String {
        self.expand_paste_chips(&self.lines.join("\n"))
    }

    fn expand_paste_chips(&self, stored: &str) -> String {
        if self.paste_chips.is_empty() {
            return stored.to_string();
        }
        let mut expanded = stored.to_string();
        for chip in &self.paste_chips {
            expanded = expanded.replace(&chip.token, &chip.content);
        }
        expanded
    }

    fn paste_chip_token(id: usize) -> String {
        format!("{PASTE_CHIP_SENTINEL_START}paste:{id}{PASTE_CHIP_SENTINEL_END}")
    }

    fn chip_at_offset<'a>(&'a self, line: &str, offset: usize) -> Option<&'a PasteChip> {
        let tail = line.get(offset..)?;
        self.paste_chips.iter().find(|chip| tail.starts_with(&chip.token))
    }

    fn display_line(&self, line: &str) -> String {
        if self.paste_chips.is_empty() {
            return line.to_string();
        }
        let mut out = String::new();
        let mut offset = 0usize;
        while offset < line.len() {
            if let Some(chip) = self.chip_at_offset(line, offset) {
                out.push_str(&chip.placeholder);
                offset = offset.saturating_add(chip.token.len());
                continue;
            }
            let Some(ch) = line.get(offset..).and_then(|tail| tail.chars().next()) else {
                break;
            };
            out.push(ch);
            offset = offset.saturating_add(ch.len_utf8());
        }
        out
    }

    fn display_lines(&self) -> Vec<String> {
        self.lines.iter().map(|line| self.display_line(line)).collect()
    }

    fn display_cursor_offset(&self, line_idx: usize, storage_cursor: usize) -> usize {
        let Some(line) = self.lines.get(line_idx) else {
            return 0;
        };
        let cursor = storage_cursor.min(line.len());
        let mut storage_offset = 0usize;
        let mut display_offset = 0usize;
        while storage_offset < line.len() {
            if cursor <= storage_offset {
                return display_offset;
            }
            if let Some(chip) = self.chip_at_offset(line, storage_offset) {
                let chip_end = storage_offset.saturating_add(chip.token.len());
                if cursor < chip_end {
                    return display_offset;
                }
                storage_offset = chip_end;
                display_offset = display_offset.saturating_add(chip.placeholder.len());
                continue;
            }
            let Some(ch) = line.get(storage_offset..).and_then(|tail| tail.chars().next()) else {
                break;
            };
            storage_offset = storage_offset.saturating_add(ch.len_utf8());
            display_offset = display_offset.saturating_add(ch.len_utf8());
        }
        display_offset
    }

    fn chip_range_matching<F>(&self, line: &str, mut matches: F) -> Option<(usize, usize)>
    where
        F: FnMut(usize, usize) -> bool,
    {
        for chip in &self.paste_chips {
            for (start, _) in line.match_indices(&chip.token) {
                let end = start.saturating_add(chip.token.len());
                if matches(start, end) {
                    return Some((start, end));
                }
            }
        }
        None
    }

    fn chip_range_containing_cursor(&self, line: &str, offset: usize) -> Option<(usize, usize)> {
        self.chip_range_matching(line, |start, end| start < offset && offset < end)
    }

    fn chip_range_before_or_containing_cursor(&self, line: &str, offset: usize) -> Option<(usize, usize)> {
        self.chip_range_matching(line, |start, end| start < offset && offset <= end)
    }

    fn chip_range_at_or_containing_cursor(&self, line: &str, offset: usize) -> Option<(usize, usize)> {
        self.chip_range_matching(line, |start, end| start <= offset && offset < end)
    }

    fn remove_chip_range(&mut self, line_idx: usize, start: usize, end: usize) {
        let removed = self
            .lines
            .get(line_idx)
            .and_then(|line| line.get(start..end))
            .map(str::to_string);
        if let Some(line) = self.lines.get_mut(line_idx) {
            line.replace_range(start..end, "");
            self.cursor = (line_idx, start.min(line.len()));
        }
        if let Some(token) = removed {
            self.paste_chips.retain(|chip| chip.token != token);
        }
    }

    fn snap_cursor_after_chip(&mut self) {
        let (line_idx, offset) = self.cursor;
        let Some(line) = self.lines.get(line_idx) else {
            return;
        };
        if let Some((_start, end)) = self.chip_range_containing_cursor(line, offset) {
            self.cursor = (line_idx, end);
        }
    }

    fn draft_snapshot(&self) -> InputDraftSnapshot {
        InputDraftSnapshot {
            lines: self.lines.clone(),
            paste_chips: self.paste_chips.clone(),
            next_paste_chip_id: self.next_paste_chip_id,
            truncated: self.truncated,
        }
    }

    fn restore_draft_snapshot(&mut self, snapshot: InputDraftSnapshot) {
        self.lines = if snapshot.lines.is_empty() {
            vec![String::new()]
        } else {
            snapshot.lines
        };
        self.paste_chips = snapshot.paste_chips;
        self.next_paste_chip_id = snapshot.next_paste_chip_id.max(1);
        self.truncated = snapshot.truncated;
        let last_line_idx = self.lines.len().saturating_sub(1);
        let last_len = self.lines.get(last_line_idx).map_or(0, String::len);
        self.cursor = (last_line_idx, last_len);
    }

    /// True when the buffer is logically empty (single empty line).
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines.first().is_none_or(String::is_empty)
    }

    /// Filter text for a leading slash command when the cursor is inside the
    /// command token. Returns `None` once the cursor moves into arguments.
    pub fn slash_command_filter_at_cursor(&self) -> Option<String> {
        match self.slash_cursor_context() {
            Some(SlashCursorContext::Command { filter }) => Some(filter),
            _ => None,
        }
    }

    /// Filter text after a word-start `@` token at the cursor, used by the
    /// TUI loop to source file candidates through the file-read security gate.
    pub(crate) fn at_path_filter_at_cursor(&self) -> Option<String> {
        match self.at_path_cursor_context() {
            Some(SlashCursorContext::AtPath { filter }) => Some(filter),
            _ => None,
        }
    }

    fn completion_cursor_context(&self) -> Option<SlashCursorContext> {
        self.slash_cursor_context().or_else(|| self.at_path_cursor_context())
    }

    fn slash_cursor_context(&self) -> Option<SlashCursorContext> {
        let (line_idx, cursor_offset) = self.cursor;
        if line_idx != 0 {
            return None;
        }
        let line = self.lines.get(line_idx)?;
        if !line.starts_with('/') {
            return None;
        }
        let cursor = cursor_offset.min(line.len());
        let command_end = line.find(char::is_whitespace).unwrap_or(line.len());
        if cursor <= command_end {
            return line.get(1..cursor).map(|filter| SlashCursorContext::Command {
                filter: filter.to_string(),
            });
        }
        let command = line.get(..command_end)?.to_string();
        let Some(args_start) = line
            .get(command_end..)?
            .find(|ch: char| !ch.is_whitespace())
            .map(|offset| command_end.saturating_add(offset))
        else {
            return Some(SlashCursorContext::Argument {
                command,
                arg_index: 0,
                filter: String::new(),
                previous_args: Vec::new(),
            });
        };
        if cursor < args_start {
            return None;
        }
        let before_cursor = line.get(args_start..cursor)?;
        let mut parts = before_cursor.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        let cursor_after_whitespace = before_cursor.chars().last().is_some_and(char::is_whitespace);
        let (arg_index, filter, previous_args) = if cursor_after_whitespace {
            (parts.len(), String::new(), parts)
        } else if let Some(filter) = parts.pop() {
            (parts.len(), filter, parts)
        } else {
            (0, String::new(), Vec::new())
        };
        Some(SlashCursorContext::Argument {
            command,
            arg_index,
            filter,
            previous_args,
        })
    }

    fn at_path_cursor_context(&self) -> Option<SlashCursorContext> {
        let (line_idx, cursor_offset) = self.cursor;
        let line = self.lines.get(line_idx)?;
        let cursor = cursor_offset.min(line.len());
        let mut token_start = 0;
        for (offset, ch) in line.get(..cursor)?.char_indices() {
            if ch.is_whitespace() {
                token_start = offset.saturating_add(ch.len_utf8());
            }
        }
        let token = line.get(token_start..cursor)?;
        if !token.starts_with('@') {
            return None;
        }
        let before = line.get(..token_start).unwrap_or_default();
        if before.chars().last().is_some_and(|ch| !ch.is_whitespace()) {
            return None;
        }
        Some(SlashCursorContext::AtPath {
            filter: token.get(1..).unwrap_or_default().to_string(),
        })
    }

    /// Replace the current leading slash-command token with `command`, leaving a
    /// trailing space so the operator can immediately type arguments.
    fn slash_command_suffix_is_empty(&self) -> bool {
        let (line_idx, _cursor_offset) = self.cursor;
        let Some(line) = self.lines.get(line_idx) else {
            return false;
        };
        if !line.starts_with('/') {
            return false;
        }
        let token_end = line.find(char::is_whitespace).unwrap_or(line.len());
        line.get(token_end..).unwrap_or_default().trim().is_empty()
    }

    fn replace_slash_command_token(&mut self, command: &str, append_space: bool) {
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
            if append_space {
                format!("{command} ")
            } else {
                command.to_string()
            }
        } else {
            format!("{command} {suffix}")
        };
        *line = replacement;
        let cursor = if append_space {
            command.len().saturating_add(1)
        } else {
            command.len()
        };
        self.cursor = (line_idx, cursor.min(line.len()));
        self.history_pos = None;
        self.pending_draft = None;
        self.reverse_search = None;
    }

    fn replace_slash_argument_token(&mut self, value: &str, append_space: bool) {
        let (line_idx, cursor_offset) = self.cursor;
        let Some(line) = self.lines.get_mut(line_idx) else {
            return;
        };
        if !line.starts_with('/') {
            return;
        }
        let cursor = cursor_offset.min(line.len());
        let command_end = line.find(char::is_whitespace).unwrap_or(line.len());
        if cursor <= command_end {
            return;
        }
        let Some(args_offset) = line
            .get(command_end..)
            .and_then(|tail| tail.find(|ch: char| !ch.is_whitespace()))
        else {
            let insertion = if append_space {
                format!("{value} ")
            } else {
                value.to_string()
            };
            line.insert_str(cursor, &insertion);
            self.cursor = (line_idx, cursor.saturating_add(insertion.len()).min(line.len()));
            self.history_pos = None;
            self.pending_draft = None;
            self.reverse_search = None;
            return;
        };
        let args_start = command_end.saturating_add(args_offset);
        if cursor < args_start {
            return;
        }

        let mut token_start = args_start;
        for (offset, ch) in line.get(args_start..cursor).unwrap_or_default().char_indices() {
            if ch.is_whitespace() {
                token_start = args_start.saturating_add(offset).saturating_add(ch.len_utf8());
            }
        }
        let token_end = line
            .get(cursor..)
            .and_then(|tail| tail.find(char::is_whitespace))
            .map_or(line.len(), |offset| cursor.saturating_add(offset));
        let suffix = line.get(token_end..).unwrap_or_default().to_string();
        let insertion = if append_space {
            format!("{value} ")
        } else {
            value.to_string()
        };
        line.replace_range(token_start..token_end, &insertion);
        self.cursor = (line_idx, token_start.saturating_add(insertion.len()).min(line.len()));
        if !suffix.is_empty() && !line.ends_with(&suffix) {
            line.push_str(&suffix);
        }
        self.history_pos = None;
        self.pending_draft = None;
        self.reverse_search = None;
    }

    fn replace_at_path_token(&mut self, value: &str, append_space: bool) {
        let (line_idx, cursor_offset) = self.cursor;
        let Some(line) = self.lines.get_mut(line_idx) else {
            return;
        };
        let cursor = cursor_offset.min(line.len());
        let mut token_start = 0;
        for (offset, ch) in line.get(..cursor).unwrap_or_default().char_indices() {
            if ch.is_whitespace() {
                token_start = offset.saturating_add(ch.len_utf8());
            }
        }
        let Some(token) = line.get(token_start..cursor) else {
            return;
        };
        if !token.starts_with('@') {
            return;
        }
        let token_end = line
            .get(cursor..)
            .and_then(|tail| tail.find(char::is_whitespace))
            .map_or(line.len(), |offset| cursor.saturating_add(offset));
        let insertion = if append_space {
            format!("{value} ")
        } else {
            value.to_string()
        };
        line.replace_range(token_start..token_end, &insertion);
        self.cursor = (line_idx, token_start.saturating_add(insertion.len()).min(line.len()));
        self.history_pos = None;
        self.pending_draft = None;
        self.reverse_search = None;
    }

    /// Current draft size in bytes, counting newline separators between rows.
    pub fn byte_len(&self) -> usize {
        self.text().len()
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
        self.paste_chips.clear();
        self.next_paste_chip_id = 1;
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
        self.paste_chips.clear();
        self.next_paste_chip_id = 1;
        self.truncated = false;
        self.reverse_search = None;
    }

    /// Insert a single grapheme (`ch`) at the cursor.
    fn insert_char(&mut self, ch: char) -> bool {
        self.snap_cursor_after_chip();
        if self.byte_len().saturating_add(ch.len_utf8()) > INPUT_MAX_BYTES {
            self.truncated = true;
            return false;
        }
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            self.history_pos = None;
            self.pending_draft = None;
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
        self.snap_cursor_after_chip();
        self.history_pos = None;
        self.pending_draft = None;
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

    fn insert_chip_token(&mut self, token: &str) {
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            self.history_pos = None;
            self.pending_draft = None;
            let clamped = off.min(line.len());
            line.insert_str(clamped, token);
            self.cursor = (li, clamped.saturating_add(token.len()));
        }
    }

    fn insert_folded_paste(&mut self, text: &str) {
        let remaining = INPUT_MAX_BYTES.saturating_sub(self.byte_len());
        if remaining == 0 {
            self.truncated = true;
            return;
        }
        let content = if text.len() > remaining {
            self.truncated = true;
            clamp_str_to_byte_len(text, remaining).to_string()
        } else {
            text.to_string()
        };
        if content.is_empty() {
            return;
        }
        let id = self.next_paste_chip_id;
        self.next_paste_chip_id = self.next_paste_chip_id.saturating_add(1);
        let line_count = pasted_line_count(&content);
        let token = Self::paste_chip_token(id);
        let placeholder = format!("[Pasted text #{id}: {line_count} lines]");
        self.paste_chips.push(PasteChip {
            token: token.clone(),
            placeholder,
            content,
        });
        self.insert_chip_token(&token);
    }

    /// Split the current line at the cursor (`Shift+Enter`).
    fn insert_newline(&mut self) {
        self.snap_cursor_after_chip();
        if self.byte_len().saturating_add(1) > INPUT_MAX_BYTES {
            self.truncated = true;
            return;
        }
        self.history_pos = None;
        self.pending_draft = None;
        let (li, off) = self.cursor;
        if let Some(line) = self.lines.get_mut(li) {
            let clamped = off.min(line.len());
            let tail: String = line.split_off(clamped);
            self.lines.insert(li + 1, tail);
            self.cursor = (li + 1, 0);
        }
    }

    fn consume_backslash_line_continuation(&mut self) -> bool {
        let (li, off) = self.cursor;
        let Some(line) = self.lines.get_mut(li) else {
            return false;
        };
        if off != line.len() || !line.ends_with('\\') {
            return false;
        }
        line.pop();
        self.cursor = (li, line.len());
        self.insert_newline();
        true
    }

    /// Delete the character before the cursor; join with previous line if at
    /// column 0.
    fn backspace(&mut self) {
        self.history_pos = None;
        self.pending_draft = None;
        let (li, off) = self.cursor;
        if off > 0 {
            if let Some(line) = self.lines.get(li)
                && let Some((start, end)) = self.chip_range_before_or_containing_cursor(line, off)
            {
                self.remove_chip_range(li, start, end);
                if self.byte_len() < INPUT_MAX_BYTES {
                    self.truncated = false;
                }
                return;
            }
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
        self.history_pos = None;
        self.pending_draft = None;
        let (li, off) = self.cursor;
        let line_len = self.lines.get(li).map_or(0, String::len);
        if off < line_len {
            if let Some(line) = self.lines.get(li)
                && let Some((start, end)) = self.chip_range_at_or_containing_cursor(line, off)
            {
                self.remove_chip_range(li, start, end);
                if self.byte_len() < INPUT_MAX_BYTES {
                    self.truncated = false;
                }
                return;
            }
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
                if let Some((start, _end)) = self.chip_range_before_or_containing_cursor(line, off) {
                    self.cursor = (li, start);
                    return;
                }
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
                if let Some((_start, end)) = self.chip_range_at_or_containing_cursor(line, off) {
                    self.cursor = (li, end);
                    return;
                }
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
        self.snap_cursor_after_chip();
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
        self.snap_cursor_after_chip();
        true
    }

    /// Delete from start of current line up to cursor (`Ctrl+U`).
    fn delete_to_line_start(&mut self) {
        self.snap_cursor_after_chip();
        self.history_pos = None;
        self.pending_draft = None;
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
                saved_draft: self.draft_snapshot(),
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
            self.restore_draft_snapshot(search.saved_draft.clone());
        }
    }

    fn cancel_reverse_search(&mut self) {
        if let Some(search) = self.reverse_search.take() {
            self.restore_draft_snapshot(search.saved_draft);
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
                self.pending_draft = Some(self.draft_snapshot());
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
                self.restore_draft_snapshot(draft);
            } else {
                self.lines = vec![String::new()];
                self.paste_chips.clear();
                self.next_paste_chip_id = 1;
                self.truncated = false;
                self.cursor = (0, 0);
            }
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
                if self.consume_backslash_line_continuation() {
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
        if should_fold_paste(text) {
            self.insert_folded_paste(text);
        } else {
            self.insert_str(text);
        }
    }
}

fn pasted_line_count(text: &str) -> usize {
    if text.is_empty() { 0 } else { text.split('\n').count() }
}

fn should_fold_paste(text: &str) -> bool {
    pasted_line_count(text) > PASTE_FOLD_LINE_THRESHOLD || text.len() > PASTE_FOLD_BYTE_THRESHOLD
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
            visible_streaming_drafts: Arc::new(Vec::new()),
            sessions_status: String::new(),
            focus: crate::chat::sessions::FocusTarget::Main,
            switcher: None,
            strip_selection: None,
            slash_menu: None,
            sessions_cache: Vec::new(),
            main_queue_status: MainQueueStatus::default(),
            provider_worker_status: ProviderWorkerStatus::default(),
            saved_sessions_cache: Vec::new(),
            provider_model_catalog: Vec::new(),
            at_path_candidates: Vec::new(),
            saved_session_picker: None,
            active_session_view: None,
            pending_tool_approval: None,
            context_used_tokens: None,
            context_window_tokens: None,
            token_usage_summary: MainSessionTokenUsageSummary::default(),
            external_editor_prefix_armed: false,
        }
    }

    #[must_use]
    pub fn streaming_draft_for_worker(&self, sequence: u64) -> Option<&StreamingDraft> {
        self.visible_streaming_drafts
            .iter()
            .find(|draft| draft.sequence == sequence)
            .map(|draft| &draft.draft)
    }

    pub fn clear_pending_tool_approval(&mut self) -> bool {
        let had_pending = self.pending_tool_approval.take().is_some();
        let had_approval_focus = matches!(self.focus, crate::chat::sessions::FocusTarget::Approval);
        if had_approval_focus {
            self.focus = crate::chat::sessions::FocusTarget::Main;
        }
        had_pending || had_approval_focus
    }

    pub fn update_at_path_candidates(&mut self, candidates: Vec<AtPathCandidate>) {
        if self.at_path_candidates == candidates {
            return;
        }
        self.at_path_candidates = candidates;
        let sources = SlashMenuSources {
            live_sessions: &self.sessions_cache,
            saved_sessions: &self.saved_sessions_cache,
            provider_model_catalog: &self.provider_model_catalog,
            at_path_candidates: &self.at_path_candidates,
            current_provider: &self.provider,
        };
        sync_slash_menu_for_sources(&self.input, &mut self.slash_menu, sources);
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
                let sources = SlashMenuSources {
                    live_sessions: &self.sessions_cache,
                    saved_sessions: &self.saved_sessions_cache,
                    provider_model_catalog: &self.provider_model_catalog,
                    at_path_candidates: &self.at_path_candidates,
                    current_provider: &self.provider,
                };
                sync_slash_menu_for_sources(&self.input, &mut self.slash_menu, sources);
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

    #[must_use]
    pub fn execution_activity_active(&self) -> bool {
        execution_activity_active_for_view(self)
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
        let args_preview = build_tool_args_preview(tool_name, args_full, ARGS_PREVIEW_MAX_CHARS, preview_ellipsis);
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
            folded, result, status, ..
        } => {
            // Claude-Code style: bullet header (1 row) + an optional follow-on
            // block. While running there is no follow-on yet.
            if matches!(status, ToolStatus::Running) {
                1
            } else if *folded {
                // header + `⎿ output ✓ metrics` summary row + bounded preview.
                let result_text = result.as_deref().unwrap_or("");
                let preview_rows = if result_text.trim().is_empty() {
                    0
                } else {
                    let total = result_text.lines().count();
                    total.min(TOOL_FOLDED_RESULT_PREVIEW_LINES) + usize::from(total > TOOL_FOLDED_RESULT_PREVIEW_LINES)
                };
                2 + preview_rows
            } else {
                // header + input row + output/error label + bounded body rows
                let body = result.as_deref().filter(|s| !s.is_empty()).unwrap_or("");
                let body_line_count = body.lines().count();
                let body_rows = body_line_count.clamp(1, TOOL_EXPANDED_OUTPUT_MAX_LINES);
                let trunc_row = usize::from(body_line_count > TOOL_EXPANDED_OUTPUT_MAX_LINES);
                3 + body_rows + trunc_row
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
    /// Main-session input backlog status.
    fn main_queue_status(&self) -> MainQueueStatus;
    /// Main-session provider worker status.
    fn provider_worker_status(&self) -> ProviderWorkerStatus;
    /// UI-only bottom-strip selection, separate from input-routing focus.
    fn strip_selection(&self) -> Option<u64>;
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
    fn strip_selection(&self) -> Option<u64> {
        self.strip_selection
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
    fn strip_selection(&self) -> Option<u64> {
        self.strip_selection
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
    anchor_top_row: Option<usize>,
    last_tail_marker: usize,
    pub new_output_below: bool,
}

impl FullscreenTranscriptScroll {
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

fn fullscreen_transcript_area<V: BottomChromeView + ?Sized>(state: &V, total_width: u16, total_height: u16) -> Rect {
    let chrome_height = fullscreen_bottom_chrome_height_for_width(state, total_width).min(total_height);
    let content_area = Rect {
        x: 0,
        y: 0,
        width: total_width,
        height: total_height.saturating_sub(chrome_height),
    };
    fullscreen_content_areas(content_area, state).0
}

fn fullscreen_tail_marker<V: BottomChromeView + ?Sized>(state: &V) -> usize {
    let finalized = state.conversation_lines().len().saturating_mul(1_000_000);
    let streaming_chars = state
        .streaming()
        .map_or(0usize, |streaming| streaming.accumulated.chars().count());
    finalized.saturating_add(streaming_chars)
}

fn session_footer_has_sessions<V: BottomChromeView + ?Sized>(state: &V) -> bool {
    let entries = bottom_chrome_session_entries_with_workers(
        state.sessions_entries(),
        state.provider_worker_status(),
        state.focus(),
    );
    if entries.is_empty() {
        return !state.sessions_status().is_empty();
    }
    true
}

fn session_footer_desired_rows<V: BottomChromeView + ?Sized>(state: &V) -> u16 {
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
        visible_entries.len().saturating_add(1)
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

/// Toggle a visible reasoning-card header at a fullscreen mouse coordinate.
///
/// This mirrors the transcript renderer's viewport math: frame dimensions are
/// converted into the transcript area, the current scroll state chooses the
/// top visible row, and only rows occupied by a `Reasoning` header are
/// actionable. Body rows and other conversation lines are ignored.
pub fn toggle_reasoning_at_fullscreen_point(
    state: &mut TuiState,
    scroll: &FullscreenTranscriptScroll,
    total_width: u16,
    total_height: u16,
    column: u16,
    row: u16,
) -> bool {
    let area = fullscreen_transcript_area(state, total_width, total_height);
    if area.width == 0
        || area.height == 0
        || column < area.x
        || column >= area.x.saturating_add(area.width)
        || row < area.y
        || row >= area.y.saturating_add(area.height)
    {
        return false;
    }

    let top_scroll = fullscreen_transcript_top_scroll(state, scroll, area);
    let clicked_row = top_scroll.saturating_add(usize::from(row.saturating_sub(area.y)));
    reasoning_index_at_rendered_row(state, area.width.max(1), clicked_row).is_some_and(|idx| {
        if let Some(ConversationLine::Reasoning { folded, .. }) = state.conversation_lines.get_mut(idx) {
            *folded = !*folded;
            return true;
        }
        false
    })
}

fn fullscreen_transcript_top_scroll<V: BottomChromeView + ?Sized>(
    state: &V,
    scroll: &FullscreenTranscriptScroll,
    area: Rect,
) -> usize {
    if area.height == 0 {
        return 0;
    }
    let mut lines: Vec<Line<'_>> = Vec::new();
    push_conversation_transcript_lines(&mut lines, state);
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

fn push_conversation_transcript_lines<'a, V: BottomChromeView + ?Sized>(lines: &mut Vec<Line<'a>>, state: &'a V) {
    for line in state.conversation_lines() {
        render_conversation_line(lines, line, state.ascii_fallback());
    }
}

fn reasoning_index_at_rendered_row(state: &TuiState, width: u16, rendered_row: usize) -> Option<usize> {
    let mut cursor = 0usize;
    for (idx, conv_line) in state.conversation_lines.iter().enumerate() {
        let mut rendered = Vec::new();
        render_conversation_line(&mut rendered, conv_line, state.ascii_fallback());
        let line_rows = usize::from(measure_wrapped_rows(&rendered, width));
        if let ConversationLine::Reasoning { .. } = conv_line {
            let header_rows = rendered.first().map_or(1, |header| {
                let header_lines = vec![header.clone()];
                usize::from(measure_wrapped_rows(&header_lines, width))
            });
            if rendered_row >= cursor && rendered_row < cursor.saturating_add(header_rows) {
                return Some(idx);
            }
        }
        cursor = cursor.saturating_add(line_rows);
    }
    None
}

fn fullscreen_bottom_chrome_base_height<V: BottomChromeView + ?Sized>(state: &V) -> u16 {
    let visible_input_rows = state.input().lines.len().clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let footer_rows = session_footer_desired_rows(state);
    1u16.saturating_add(input_height).saturating_add(footer_rows)
}

fn fullscreen_bottom_chrome_height_for_width<V: BottomChromeView + ?Sized>(state: &V, width: u16) -> u16 {
    let visible_input_rows = input_visual_rows_for_width(state, width).clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let footer_rows = session_footer_desired_rows(state);
    1u16.saturating_add(input_height)
        .saturating_add(footer_rows)
        .clamp(BOTTOM_CHROME_MIN_HEIGHT, BOTTOM_CHROME_MAX_HEIGHT)
}

fn render_fullscreen_bottom_chrome_at<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    area: Rect,
    state: &V,
    show_new_output_below: bool,
) {
    let visible_input_rows = input_visual_rows_for_width(state, area.width).clamp(1, INPUT_MAX_VISIBLE_ROWS);
    let input_height = u16::try_from(visible_input_rows.saturating_add(1)).unwrap_or(2);
    let max_footer_rows = area.height.saturating_sub(1).saturating_sub(input_height).max(1);
    let footer_rows = session_footer_desired_rows(state).min(max_footer_rows);

    let fixed_rows = 1u16.saturating_add(input_height).saturating_add(footer_rows);
    let spacer_rows = area.height.saturating_sub(fixed_rows);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(spacer_rows),
            Constraint::Length(input_height),
            Constraint::Length(footer_rows),
        ])
        .split(area);

    #[allow(clippy::indexing_slicing)]
    {
        render_status_bar(frame, chunks[0], state);
        render_input(frame, chunks[2], state);
        render_fullscreen_footer(frame, chunks[3], state, show_new_output_below);
    }
}

pub fn render_fullscreen_chat<V: BottomChromeView + ?Sized>(
    frame: &mut Frame,
    state: &V,
    scroll: &mut FullscreenTranscriptScroll,
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
        scroll.anchor_top_row = None;
        return;
    }

    let mut lines: Vec<Line<'_>> = Vec::new();
    push_conversation_transcript_lines(&mut lines, state);
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
}

fn render_fullscreen_overlays<V: BottomChromeView + ?Sized>(frame: &mut Frame, frame_area: Rect, state: &V) {
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

fn slash_menu_overlay_rect(frame_area: Rect, menu: &SlashMenuState, bottom_chrome_height: u16) -> Rect {
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

const fn session_active_marker(active: bool, _ascii: bool) -> &'static str {
    if active { ">" } else { " " }
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

const SESSION_STRIP_CHIP_MAX_WIDTH: u16 = 24;
const SESSION_STRIP_USAGE_CHIP_MAX_WIDTH: u16 = 48;
const SESSION_STRIP_IDLE_CHIP_MAX_WIDTH: u16 = 30;
const SESSION_STRIP_TITLE_MAX_WIDTH: u16 = 10;

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
    let elapsed = entry.elapsed_label();
    let idle = if entry.idle_warning {
        if ascii { " [idle]" } else { " ⚠ idle" }
    } else {
        ""
    };
    let usage = entry
        .token_usage_summary()
        .and_then(crate::chat::session::format_session_token_usage_inline);
    let chip_cap = if usage.is_some() {
        SESSION_STRIP_USAGE_CHIP_MAX_WIDTH
    } else if entry.idle_warning {
        SESSION_STRIP_IDLE_CHIP_MAX_WIDTH
    } else {
        SESSION_STRIP_CHIP_MAX_WIDTH
    };
    let chip_width = max_width.min(chip_cap);
    let display_id = switcher_entry_display_id(entry);
    let prefix = usage.map_or_else(
        || format!("{marker}{glyph} {display_id} {} {elapsed}{idle}", entry.kind),
        |usage| format!("{marker}{glyph} {display_id} {} {elapsed}{idle} {usage}", entry.kind),
    );
    let prefix_cols = UnicodeWidthStr::width(prefix.as_str());
    let max = usize::from(chip_width);
    if prefix_cols >= max {
        return truncate_chars_with_ellipsis(&prefix, chip_width, ascii);
    }
    let remaining = max.saturating_sub(prefix_cols);
    if remaining <= 1 {
        return prefix;
    }
    let title_budget = u16::try_from(remaining.saturating_sub(1))
        .unwrap_or(u16::MAX)
        .min(SESSION_STRIP_TITLE_MAX_WIDTH);
    let title = truncate_chars_with_ellipsis(&entry.title, title_budget, ascii);
    if title.is_empty() {
        prefix
    } else {
        format!("{prefix} {title}")
    }
}

fn render_sessions_list_entry_line(
    entry: &crate::chat::sessions::SwitcherEntry,
    active_seq: Option<u64>,
    strip_selection: Option<u64>,
    ascii: bool,
    width: u16,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let active = active_seq == Some(entry.seq);
    let selected = strip_selection == Some(entry.seq);
    let marker = session_active_marker(active, ascii);
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
        "{marker} {display_id} {} {} {}{}{}{}",
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
    let style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else if entry.is_terminal() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan)
    };
    Line::from(Span::styled(text, style))
}

fn render_main_session_list_line<V: BottomChromeView + ?Sized>(state: &V, width: u16) -> Line<'static> {
    let ascii = state.ascii_fallback();
    let active = matches!(state.focus(), crate::chat::sessions::FocusTarget::Main);
    let selected = state.strip_selection() == Some(MAIN_SESSION_SELECTION_SEQ);
    let marker = session_active_marker(active, ascii);
    let sep = if ascii { " | " } else { " · " };
    let usage = render_main_token_usage(state.token_usage_summary());
    let text = format!(
        "{marker} main chat {}{}{}{}{}/{}",
        if active { "active" } else { "ready" },
        sep,
        usage,
        sep,
        state.provider(),
        state.model()
    );
    let text = truncate_chars_with_ellipsis(&text, width, ascii);
    let style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Line::from(Span::styled(text, style))
}

fn render_sessions_list_lines<V: BottomChromeView + ?Sized>(
    state: &V,
    width: u16,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let summary = state.sessions_status();
    let focus = state.focus();
    let entries =
        bottom_chrome_session_entries_with_workers(state.sessions_entries(), state.provider_worker_status(), focus);
    let strip_selection = state.strip_selection();
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
    let target_idx = bottom_list_selection_index(&entries, strip_selection, focus).min(total.saturating_sub(1));
    let start = if total <= max_rows {
        0
    } else {
        target_idx.saturating_add(1).saturating_sub(max_rows)
    };
    let mut lines = Vec::new();
    for idx in start..total.min(start.saturating_add(max_rows)) {
        if idx == 0 {
            lines.push(render_main_session_list_line(state, width));
        } else if let Some(entry) = entries.get(idx.saturating_sub(1)) {
            lines.push(render_sessions_list_entry_line(
                entry,
                active_seq,
                strip_selection,
                ascii,
                width,
            ));
        }
    }
    lines
}

fn render_sessions_strip_line(
    entries: &[crate::chat::sessions::SwitcherEntry],
    summary: &str,
    focus: crate::chat::sessions::FocusTarget,
    ascii: bool,
    width: u16,
) -> String {
    render_sessions_strip_line_with_selection(entries, summary, focus, None, ascii, width)
}

fn render_sessions_strip_line_with_selection(
    entries: &[crate::chat::sessions::SwitcherEntry],
    summary: &str,
    focus: crate::chat::sessions::FocusTarget,
    strip_selection: Option<u64>,
    ascii: bool,
    width: u16,
) -> String {
    render_sessions_strip_styled_line(entries, summary, focus, strip_selection, ascii, width)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn render_sessions_strip_styled_line(
    entries: &[crate::chat::sessions::SwitcherEntry],
    summary: &str,
    focus: crate::chat::sessions::FocusTarget,
    strip_selection: Option<u64>,
    ascii: bool,
    width: u16,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let content_width = width.saturating_sub(1);
    if content_width == 0 {
        return Line::default();
    }
    if entries.is_empty() {
        if summary.is_empty() {
            return Line::default();
        }
        let text = truncate_chars_with_ellipsis(summary, content_width, ascii);
        return Line::from(vec![Span::raw(" "), Span::raw(text)]);
    }

    let active_seq = focus_active_entry_seq(focus);
    let target_idx = strip_selection_index(entries, strip_selection, focus);
    let start = target_idx
        .map(|target| sessions_strip_window_start(entries, active_seq, ascii, content_width, target))
        .unwrap_or(0);
    let mut spans = vec![Span::raw(" ")];
    spans.extend(render_sessions_strip_window(
        entries,
        start,
        active_seq,
        strip_selection,
        ascii,
        content_width,
    ));
    Line::from(spans)
}

fn sessions_strip_window_start(
    entries: &[crate::chat::sessions::SwitcherEntry],
    active_seq: Option<u64>,
    ascii: bool,
    width: u16,
    target_idx: usize,
) -> usize {
    for start in 0..=target_idx {
        let visible = sessions_strip_visible_indices(entries, start, active_seq, ascii, width);
        if visible.contains(&target_idx) {
            return start;
        }
    }
    target_idx.min(entries.len().saturating_sub(1))
}

fn sessions_strip_visible_indices(
    entries: &[crate::chat::sessions::SwitcherEntry],
    start: usize,
    active_seq: Option<u64>,
    ascii: bool,
    width: u16,
) -> Vec<usize> {
    let mut visible = Vec::new();
    let mut used_cols = 0usize;
    let width_cols = usize::from(width);
    if width_cols == 0 || start >= entries.len() {
        return visible;
    }
    let left = sessions_strip_left_overflow(start, ascii);
    if !left.is_empty() {
        let left_cols = UnicodeWidthStr::width(left);
        if left_cols >= width_cols {
            return visible;
        }
        used_cols = used_cols.saturating_add(left_cols);
    }
    for idx in start..entries.len() {
        let Some(entry) = entries.get(idx) else {
            break;
        };
        let sep_cols = usize::from(idx > start);
        let hidden_after = entries.len().saturating_sub(idx.saturating_add(1));
        let right_cols = sessions_strip_right_overflow_width(hidden_after, ascii);
        let available = width_cols
            .saturating_sub(used_cols)
            .saturating_sub(sep_cols)
            .saturating_sub(right_cols);
        if available == 0 {
            break;
        }
        let segment =
            render_sessions_strip_entry(entry, active_seq, ascii, u16::try_from(available).unwrap_or(u16::MAX));
        let segment_cols = UnicodeWidthStr::width(segment.as_str());
        let seq_marker = switcher_entry_display_id(entry);
        if segment.is_empty()
            || segment_cols == 0
            || !segment.contains(seq_marker.as_str())
            || used_cols
                .saturating_add(sep_cols)
                .saturating_add(segment_cols)
                .saturating_add(right_cols)
                > width_cols
        {
            break;
        }
        visible.push(idx);
        used_cols = used_cols.saturating_add(sep_cols).saturating_add(segment_cols);
    }
    visible
}

fn render_sessions_strip_window(
    entries: &[crate::chat::sessions::SwitcherEntry],
    start: usize,
    active_seq: Option<u64>,
    strip_selection: Option<u64>,
    ascii: bool,
    width: u16,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let width_cols = usize::from(width);
    if width_cols == 0 || start >= entries.len() {
        return spans;
    }

    let mut used_cols = 0usize;
    let left = sessions_strip_left_overflow(start, ascii);
    if !left.is_empty() {
        let left_cols = UnicodeWidthStr::width(left);
        if left_cols >= width_cols {
            spans.push(Span::raw(truncate_chars_with_ellipsis(left, width, ascii)));
            return spans;
        }
        spans.push(Span::styled(left.to_string(), Style::default().fg(Color::DarkGray)));
        used_cols = used_cols.saturating_add(left_cols);
    }

    let visible = sessions_strip_visible_indices(entries, start, active_seq, ascii, width);
    for (position, idx) in visible.iter().copied().enumerate() {
        let Some(entry) = entries.get(idx) else {
            break;
        };
        if position > 0 {
            spans.push(Span::raw(" "));
            used_cols = used_cols.saturating_add(1);
        }
        let hidden_after = entries.len().saturating_sub(idx.saturating_add(1));
        let right_cols = sessions_strip_right_overflow_width(hidden_after, ascii);
        let available = width_cols.saturating_sub(used_cols).saturating_sub(right_cols);
        if available == 0 {
            break;
        }
        let segment =
            render_sessions_strip_entry(entry, active_seq, ascii, u16::try_from(available).unwrap_or(u16::MAX));
        let segment_cols = UnicodeWidthStr::width(segment.as_str());
        if segment.is_empty()
            || segment_cols == 0
            || used_cols.saturating_add(segment_cols).saturating_add(right_cols) > width_cols
        {
            break;
        }
        let selected = strip_selection == Some(entry.seq);
        if selected {
            spans.push(Span::styled(
                segment,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw(segment));
        }
        used_cols = used_cols.saturating_add(segment_cols);
    }

    let rendered_last = visible.last().copied();
    let hidden_after = rendered_last.map_or_else(
        || entries.len().saturating_sub(start),
        |idx| entries.len().saturating_sub(idx.saturating_add(1)),
    );
    if hidden_after > 0 {
        let right = sessions_strip_right_overflow(hidden_after, ascii);
        let right_cols = UnicodeWidthStr::width(right.as_str());
        if used_cols.saturating_add(right_cols) <= width_cols {
            spans.push(Span::styled(right, Style::default().fg(Color::DarkGray)));
        }
    }

    spans
}

const fn sessions_strip_left_overflow(start: usize, ascii: bool) -> &'static str {
    if start == 0 {
        ""
    } else if ascii {
        "< "
    } else {
        "\u{2039} "
    }
}

fn sessions_strip_right_overflow(hidden_after: usize, ascii: bool) -> String {
    if hidden_after == 0 {
        String::new()
    } else if ascii {
        format!(" +{hidden_after}>")
    } else {
        format!(" +{hidden_after}\u{203A}")
    }
}

fn sessions_strip_right_overflow_width(hidden_after: usize, ascii: bool) -> usize {
    if hidden_after == 0 {
        0
    } else {
        UnicodeWidthStr::width(sessions_strip_right_overflow(hidden_after, ascii).as_str())
    }
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

fn slash_menu_command_spans(entry: &SlashMenuEntry, filter: &str, selected: bool) -> Vec<Span<'static>> {
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

fn render_slash_menu_row(entry: &SlashMenuEntry, filter: &str, selected: bool, max_width: u16) -> Line<'static> {
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

fn render_status_bar<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) {
    let status_text = render_status_bar_text(state, area.width);
    let status = Paragraph::new(status_text).style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(status, area);
}

#[allow(clippy::option_if_let_else)]
fn render_status_bar_text<V: BottomChromeView + ?Sized>(state: &V, width: u16) -> String {
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
    let activity = render_generation_activity(state);
    let permissions = render_permission_status(state.chat_mode(), state.autonomy_level());
    let full = activity.as_deref().map_or_else(
        || {
            format!(
                " PRX Chat | {}/{} | {} | {} turns | {permissions} | {usage} ",
                state.provider(),
                state.model(),
                title,
                state.turn_count(),
            )
        },
        |activity| {
            format!(
                " PRX Chat | {}/{} | {} | {} turns | {permissions} | {usage} | {activity} ",
                state.provider(),
                state.model(),
                title,
                state.turn_count(),
            )
        },
    );
    if full.chars().count() <= usize::from(width) {
        return full;
    }

    let compact = activity.as_deref().map_or_else(
        || {
            format!(
                " PRX Chat | {}/{} | {permissions} | {usage} ",
                state.provider(),
                state.model()
            )
        },
        |activity| {
            format!(
                " PRX Chat | {}/{} | {permissions} | {usage} | {activity} ",
                state.provider(),
                state.model()
            )
        },
    );
    if compact.chars().count() <= usize::from(width) {
        return compact;
    }

    let minimal = activity.as_deref().map_or_else(
        || format!(" PRX | {permissions} | {usage} "),
        |activity| {
            if let Some(queue) = queue.as_deref() {
                format!(" PRX | {permissions} | {queue} | {activity} ")
            } else if let Some(workers) = workers.as_deref() {
                let detailed = format!(" PRX | {permissions} | {workers} | {activity} ");
                if detailed.chars().count() <= usize::from(width) {
                    detailed
                } else if let Some(workers) = workers_compact.as_deref() {
                    format!(" PRX | {permissions} | {workers} | {activity} ")
                } else {
                    detailed
                }
            } else {
                format!(" PRX | {permissions} | {activity} ")
            }
        },
    );
    truncate_chars_with_ellipsis(&minimal, width, state.ascii_fallback())
}

fn render_generation_activity<V: BottomChromeView + ?Sized>(state: &V) -> Option<String> {
    if !execution_activity_active_for_view(state) {
        return None;
    }
    let frames = if state.ascii_fallback() {
        ["-", "\\", "|", "/"]
    } else {
        ["⠋", "⠙", "⠹", "⠸"]
    };
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis() / 50).unwrap_or(u64::MAX)
        });
    let idx = usize::try_from(tick % u64::try_from(frames.len()).unwrap_or(1)).unwrap_or(0);
    let frame = frames.get(idx).copied().unwrap_or("-");
    Some(format!("{frame} generating (esc to interrupt)"))
}

fn render_main_queue_status(status: MainQueueStatus) -> Option<String> {
    if status.queued == 0 && status.priority == 0 {
        return None;
    }
    if status.priority > 0 {
        Some(format!("queue:{} priority:{}", status.queued, status.priority))
    } else {
        Some(format!("queue:{}", status.queued))
    }
}

fn render_provider_worker_status(status: ProviderWorkerStatus) -> Option<String> {
    render_provider_worker_status_with_rows(status, true)
}

fn render_provider_worker_status_compact(status: ProviderWorkerStatus) -> Option<String> {
    render_provider_worker_status_with_rows(status, false)
}

fn render_provider_worker_status_with_rows(status: ProviderWorkerStatus, include_rows: bool) -> Option<String> {
    if status.running == 0 && status.cancelling == 0 && status.awaiting_commit == 0 && status.finalized_payloads == 0 {
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

fn render_provider_worker_rows(status: &ProviderWorkerStatus) -> Option<String> {
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

fn render_provider_worker_row(row: &ProviderWorkerStatusRow) -> String {
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

fn provider_worker_elapsed_label(started_at_ms: i64) -> String {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let elapsed_ms = now_ms.saturating_sub(started_at_ms).max(0);
    let elapsed_secs = u64::try_from(elapsed_ms / 1000).unwrap_or_default();
    crate::chat::sessions::model::format_elapsed_compact(elapsed_secs)
}

fn format_worker_tokens_compact(tokens: u64) -> String {
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

fn render_main_token_usage(summary: MainSessionTokenUsageSummary) -> String {
    crate::chat::session::format_session_token_usage_inline(summary).unwrap_or_else(|| "0 tok | $0.0000".to_string())
}

fn render_main_status_usage(
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

fn render_context_budget_usage(
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
            // PRX chat uses an explicit actor marker for assistant-authored
            // prose so it is visually distinct from tool IO and child-session
            // output when scanning a dense orchestration transcript.
            let rendered = cached_finalized_assistant_markdown_lines(content);
            push_assistant_rendered_lines(lines, rendered.iter().cloned(), ascii);
            lines.push(Line::from(""));
        }
        ConversationLine::StreamingAssistant { content } => {
            // Same actor marker as finalized assistant text. A trailing cursor
            // glyph (`▌`, or `_` in ASCII mode) signals that more bytes are
            // still inbound; once the stream finalises the variant becomes
            // `Assistant` and the cursor disappears.
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

fn push_assistant_rendered_lines<'a, I>(lines: &mut Vec<Line<'a>>, rendered: I, ascii: bool)
where
    I: IntoIterator<Item = Line<'static>>,
{
    let marker = if ascii { "o" } else { "\u{25CB}" }; // ○
    let marker_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
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

fn cached_finalized_assistant_markdown_lines(content: &str) -> Arc<Vec<Line<'static>>> {
    ASSISTANT_MARKDOWN_CACHE.lock().get_or_render(content)
}

fn render_streaming_assistant_markdown_lines(content: &str, ascii: bool) -> Vec<Line<'static>> {
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

fn render_assistant_markdown_lines(content: &str) -> Vec<Line<'static>> {
    let rendered = crate::chat::renderer::render_markdown_with_highlighting(content);
    ansi_sgr_to_lines(&rendered)
}

fn ansi_sgr_to_lines(input: &str) -> Vec<Line<'static>> {
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

fn find_csi_final(input: &str) -> Option<(usize, u8)> {
    input.bytes().enumerate().find(|(_, byte)| (0x40..=0x7e).contains(byte))
}

fn flush_ansi_segment(spans: &mut Vec<Span<'static>>, segment: &mut String, style: Style) {
    if segment.is_empty() {
        return;
    }
    spans.push(Span::styled(std::mem::take(segment), style));
}

fn apply_sgr_codes(mut style: Style, codes: &str) -> Style {
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

fn parse_sgr_values(codes: &str) -> Vec<u16> {
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

const fn ansi_basic_color(code: u16, bright: bool) -> Color {
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

/// Render a `ToolResult` card in Claude-Code style.
///
/// Folded layout (default):
/// ```text
/// ✓ run shell(command="ls /tmp")
///   ⎿ output ✓ 234ms · 12 lines · 1.4kB
/// ```
/// Expanded layout shows readable input plus bounded output/error rows. While
/// `Running` no follow-on row is shown — just the header `● run shell(...)`.
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
    let (_, hook) = tool_card_glyphs(ascii);
    let (status_glyph, status_color) = tool_status_marker(status, ascii);
    let preview_ellipsis = if ascii {
        ARGS_PREVIEW_ELLIPSIS_ASCII
    } else {
        ARGS_PREVIEW_ELLIPSIS
    };
    let formatted_preview = build_tool_args_preview(tool_name, args_full, ARGS_PREVIEW_MAX_CHARS, preview_ellipsis);
    let display_preview = if formatted_preview.is_empty() {
        args_preview.to_string()
    } else {
        formatted_preview
    };
    let header = if display_preview.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name}({display_preview})")
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{status_glyph} "), Style::default().fg(status_color)),
        Span::styled(
            "run ",
            Style::default()
                .fg(if matches!(status, ToolStatus::Error) {
                    Color::Red
                } else {
                    Color::Green
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(header),
    ]));

    // No follow-on row while still running — just the header is shown so the
    // user sees an in-flight indicator. (The status bar / footer carry the
    // spinner; the card itself reveals timing once we have it.)
    if matches!(status, ToolStatus::Running) {
        return;
    }

    if folded {
        push_folded_tool_summary(lines, hook, status, elapsed_ms, result, ascii);
        return;
    }

    push_expanded_tool_io(lines, hook, status, &display_preview, args_full, result, ascii);
}

fn push_folded_tool_summary<'a>(
    lines: &mut Vec<Line<'a>>,
    hook: &str,
    status: ToolStatus,
    elapsed_ms: Option<u64>,
    result: Option<&str>,
    ascii: bool,
) {
    let (status_glyph, status_color) = tool_status_marker(status, ascii);
    let metrics_style = Style::default().fg(Color::DarkGray);
    let result_text = result.unwrap_or("");
    let metrics = tool_card_metrics(status, elapsed_ms, result_text, ascii);
    let label = if matches!(status, ToolStatus::Error) {
        "error"
    } else {
        "output"
    };
    let label_style = Style::default()
        .fg(if matches!(status, ToolStatus::Error) {
            Color::Red
        } else {
            Color::Green
        })
        .add_modifier(Modifier::BOLD);

    lines.push(Line::from(vec![
        Span::styled(format!("  {hook} "), metrics_style),
        Span::styled(label, label_style),
        Span::styled(" ", metrics_style),
        Span::styled(status_glyph, Style::default().fg(status_color)),
        Span::styled(format!(" {metrics}"), metrics_style),
    ]));
    push_folded_tool_result_preview(lines, result_text, ascii);
}

fn push_folded_tool_result_preview<'a>(lines: &mut Vec<Line<'a>>, result_text: &str, ascii: bool) {
    if result_text.trim().is_empty() {
        return;
    }
    let ellipsis = if ascii {
        ARGS_PREVIEW_ELLIPSIS_ASCII
    } else {
        ARGS_PREVIEW_ELLIPSIS
    };
    let body_style = Style::default().fg(Color::DarkGray);
    let pipe = if ascii { "|" } else { "\u{2502}" };
    let result_lines = result_text.lines().collect::<Vec<_>>();
    let total_lines = result_lines.len();
    for body in result_lines.iter().take(TOOL_FOLDED_RESULT_PREVIEW_LINES) {
        let rendered = truncate_chars_with_ellipsis(body, TOOL_FOLDED_RESULT_PREVIEW_CHARS as u16, ascii);
        lines.push(Line::from(Span::styled(format!("    {pipe} {rendered}"), body_style)));
    }
    let hidden_lines = total_lines.saturating_sub(TOOL_FOLDED_RESULT_PREVIEW_LINES);
    if hidden_lines > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "    {pipe} {ellipsis} +{hidden_lines} {}",
                if hidden_lines == 1 { "line" } else { "lines" }
            ),
            body_style,
        )));
    }
}

fn push_expanded_tool_io<'a>(
    lines: &mut Vec<Line<'a>>,
    hook: &str,
    status: ToolStatus,
    display_preview: &str,
    args_full: &str,
    result: Option<&str>,
    ascii: bool,
) {
    let input = if display_preview.is_empty() {
        build_args_preview(
            args_full,
            ARGS_PREVIEW_MAX_CHARS,
            if ascii {
                ARGS_PREVIEW_ELLIPSIS_ASCII
            } else {
                ARGS_PREVIEW_ELLIPSIS
            },
        )
    } else {
        display_preview.to_string()
    };
    let body_style = Style::default().fg(Color::DarkGray);
    lines.push(Line::from(vec![
        Span::styled(format!("  {hook} "), body_style),
        Span::styled("input", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {input}"), body_style),
    ]));

    let label = if matches!(status, ToolStatus::Error) {
        "error"
    } else {
        "output"
    };
    let label_color = if matches!(status, ToolStatus::Error) {
        Color::Red
    } else {
        Color::Green
    };
    let result_text = result.unwrap_or("");
    let metrics = tool_card_metrics(status, None, result_text, ascii);
    let (status_glyph, status_color) = tool_status_marker(status, ascii);
    lines.push(Line::from(vec![
        Span::styled(format!("  {hook} "), body_style),
        Span::styled(label, Style::default().fg(label_color).add_modifier(Modifier::BOLD)),
        Span::styled(" ", body_style),
        Span::styled(status_glyph, Style::default().fg(status_color)),
        Span::styled(format!(" {metrics}"), body_style),
    ]));
    if result_text.is_empty() {
        lines.push(Line::from(Span::styled("    (empty)", body_style)));
        return;
    }

    let ellipsis = if ascii {
        ARGS_PREVIEW_ELLIPSIS_ASCII
    } else {
        ARGS_PREVIEW_ELLIPSIS
    };
    let result_lines = result_text.lines().collect::<Vec<_>>();
    let total_lines = result_lines.len();
    let total_bytes = result_text.len();
    let mut shown_lines = 0usize;
    let mut shown_bytes = 0usize;
    let mut truncated = false;
    for (idx, body) in result_lines.iter().take(TOOL_EXPANDED_OUTPUT_MAX_LINES).enumerate() {
        let rendered = clamp_one_line(body, TOOL_EXPANDED_OUTPUT_LINE_MAX_CHARS, ellipsis);
        let line_truncated = rendered.as_str() != *body;
        truncated |= line_truncated;
        shown_bytes = shown_bytes.saturating_add(if line_truncated { rendered.len() } else { body.len() });
        if idx + 1 < total_lines {
            shown_bytes = shown_bytes.saturating_add(1);
        }
        shown_lines = shown_lines.saturating_add(1);
        lines.push(Line::from(Span::styled(format!("    {rendered}"), body_style)));
    }
    if total_lines > shown_lines || truncated {
        let hidden_lines = total_lines.saturating_sub(shown_lines);
        let hidden_bytes = total_bytes.saturating_sub(shown_bytes);
        lines.push(Line::from(Span::styled(
            format!(
                "    {ellipsis} truncated: {hidden_lines} {} · {} hidden · Ctrl+O for full transcript",
                if hidden_lines == 1 { "line" } else { "lines" },
                format_bytes(hidden_bytes)
            ),
            body_style,
        )));
    }
}

fn tool_card_metrics(status: ToolStatus, elapsed_ms: Option<u64>, result_text: &str, ascii: bool) -> String {
    match status {
        ToolStatus::Done => {
            let line_count = tool_result_line_count(result_text);
            let byte_count = result_text.len();
            let mut parts = Vec::new();
            if let Some(ms) = elapsed_ms {
                parts.push(format!("{ms}ms"));
            }
            parts.push(format!(
                "{line_count} {}",
                if line_count == 1 { "line" } else { "lines" }
            ));
            parts.push(format_bytes(byte_count));
            parts.join(" \u{00B7} ")
        }
        ToolStatus::Error => {
            let mut parts = Vec::new();
            if let Some(ms) = elapsed_ms {
                parts.push(format!("{ms}ms"));
            }
            parts.push(tool_error_reason(result_text, ascii));
            parts.join(" \u{00B7} ")
        }
        ToolStatus::Running => String::new(),
    }
}

fn tool_result_line_count(result_text: &str) -> usize {
    if result_text.is_empty() {
        0
    } else {
        result_text.lines().count()
    }
}

fn tool_error_reason(result_text: &str, ascii: bool) -> String {
    let ellipsis = if ascii {
        ARGS_PREVIEW_ELLIPSIS_ASCII
    } else {
        ARGS_PREVIEW_ELLIPSIS
    };
    let reason = result_text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("error");
    clamp_one_line(reason, TOOL_ERROR_REASON_MAX_CHARS, ellipsis)
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1_000 {
        return format!("{bytes}B");
    }
    if bytes < 1_000_000 {
        return format!("{:.1}kB", bytes as f64 / 1_000.0);
    }
    format!("{:.1}MB", bytes as f64 / 1_000_000.0)
}

pub(crate) fn build_tool_args_preview(tool_name: &str, raw: &str, max_chars: usize, ellipsis: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(raw);
    let Ok(value) = parsed else {
        return build_args_preview(raw, max_chars, ellipsis);
    };
    let Some(map) = value.as_object() else {
        return clamp_one_line(&compact_json_value(&value), max_chars, ellipsis);
    };

    let preview = match tool_name {
        "file_read" => format_file_read_preview(map),
        "file_write" => format_file_write_preview(map),
        "file_edit" => format_file_edit_preview(map),
        "shell" => format_shell_preview(map),
        "managed_session" => format_managed_session_preview(map),
        "sessions_spawn" | "delegate" | "subagents" | "session_worker" | "nodes" => {
            format_session_tool_preview(tool_name, map)
        }
        "web_fetch" | "http_request" | "web_search_tool" | "web_search" => format_web_preview(tool_name, map),
        _ => None,
    };
    let preview = preview.unwrap_or_else(|| compact_json_value(&value));
    clamp_one_line(&preview, max_chars, ellipsis)
}

fn format_file_read_preview(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let path = json_string_field(map, &["path", "file_path"])?;
    let mut fields = vec![format!("path={path}")];
    if let Some(limit) = json_display_field(map, &["max_bytes", "limit", "max_lines"]) {
        fields.push(format!("limit={limit}"));
    }
    Some(fields.join(", "))
}

fn format_file_write_preview(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let path = json_string_field(map, &["path", "file_path"])?;
    let bytes = map
        .get("content")
        .and_then(serde_json::Value::as_str)
        .map(str::len)
        .unwrap_or(0);
    Some(format!("path={path}, bytes={}", format_bytes(bytes)))
}

fn format_file_edit_preview(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let path = json_string_field(map, &["path", "file_path"])?;
    let mut fields = vec![format!("path={path}")];
    if let Some(replace_all) = map.get("replace_all").and_then(serde_json::Value::as_bool)
        && replace_all
    {
        fields.push("replace_all=true".to_string());
    }
    if let Some(old) = map.get("old_string").and_then(serde_json::Value::as_str) {
        fields.push(format!("old_bytes={}", format_bytes(old.len())));
    }
    if let Some(new) = map.get("new_string").and_then(serde_json::Value::as_str) {
        fields.push(format!("new_bytes={}", format_bytes(new.len())));
    }
    Some(fields.join(", "))
}

fn format_shell_preview(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let command = json_string_field(map, &["command", "cmd"])?;
    let mut fields = vec![format!(
        "command={}",
        quoted_summary(&command, TOOL_ARG_VALUE_MAX_CHARS)
    )];
    if let Some(cwd) = json_string_field(map, &["cwd", "workdir"]) {
        fields.push(format!("cwd={cwd}"));
    }
    Some(fields.join(", "))
}

fn format_session_tool_preview(tool_name: &str, map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let meta = extract_subagent_meta_from_map(map);
    if meta.is_empty() {
        if let Some(action) = json_string_field(map, &["action", "operation"]) {
            return Some(format!("action={action}"));
        }
        return None;
    }
    let mut fields = Vec::new();
    if let Some(task) = meta.task.as_deref() {
        fields.push(format!("task={}", quoted_summary(task, TOOL_ARG_VALUE_MAX_CHARS)));
    } else if let Some(action) = json_string_field(map, &["action", "operation"]) {
        fields.push(format!("action={action}"));
    }
    if let Some(agent) = meta.agent.as_deref() {
        fields.push(format!("agent={agent}"));
    }
    if let Some(model) = meta.model.as_deref() {
        fields.push(format!("model={model}"));
    }
    if fields.is_empty() {
        Some(tool_name.to_string())
    } else {
        Some(fields.join(", "))
    }
}

fn format_managed_session_preview(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let action = json_string_field(map, &["action"])?;
    let mut fields = vec![format!("action={action}")];
    if let Some(command) = json_string_field(map, &["command"]) {
        fields.push(format!(
            "command={}",
            quoted_summary(&command, TOOL_ARG_VALUE_MAX_CHARS)
        ));
    }
    if let Some(session_id) = json_string_field(map, &["session_id"]) {
        fields.push(format!("session={}", one_line_summary(&session_id, 12)));
    }
    Some(fields.join(", "))
}

fn format_web_preview(tool_name: &str, map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    if let Some(url) = json_string_field(map, &["url", "uri", "endpoint"]) {
        let mut fields = vec![format!("url={url}")];
        if let Some(max_chars) = json_display_field(map, &["max_chars", "limit"]) {
            fields.push(format!("limit={max_chars}"));
        }
        return Some(fields.join(", "));
    }
    if let Some(query) = json_string_field(map, &["query", "q"]) {
        return Some(format!("query={}", quoted_summary(&query, TOOL_ARG_VALUE_MAX_CHARS)));
    }
    if tool_name == "http_request" {
        return json_string_field(map, &["method"]).map(|method| format!("method={method}"));
    }
    None
}

fn extract_subagent_meta_from_map(map: &serde_json::Map<String, serde_json::Value>) -> SubagentMeta {
    let str_field = |key: &str| {
        map.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut meta = SubagentMeta {
        agent: str_field("agent"),
        model: str_field("model"),
        task: str_field("prompt").or_else(|| str_field("task")),
    };
    if meta.task.is_none() {
        meta.task = str_field("action").or_else(|| str_field("operation"));
    }
    if let Some(task) = meta.task.take() {
        meta.task = Some(one_line_summary(&task, 60));
    }
    meta
}

fn json_string_field(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        map.get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn json_display_field(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let value = map.get(*key)?;
        if let Some(s) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            return Some(s.to_string());
        }
        if value.is_number() || value.is_boolean() {
            return Some(value.to_string());
        }
        None
    })
}

fn compact_json_value(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn quoted_summary(raw: &str, max_chars: usize) -> String {
    let summary = one_line_summary(raw, max_chars);
    format!("\"{}\"", summary.replace('"', "\\\""))
}

fn clamp_one_line(raw: &str, max_chars: usize, ellipsis: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = collapsed.chars().count();
    if char_count <= max_chars {
        return collapsed;
    }
    let keep = max_chars.saturating_sub(ellipsis.chars().count()).max(1);
    let mut truncated: String = collapsed.chars().take(keep).collect();
    truncated.push_str(ellipsis);
    truncated
}

/// Render a `Reasoning` card in Claude-Code style.
///
/// Folded → `▸ Thinking (123 tokens)` (or `> Thinking (123 tokens)` in ASCII),
/// dim gray + italic so the line reads as a collapsed annotation rather than
/// primary content.
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
        // Single-line folded summary — token count only, no noisy key-hint prose.
        let header = format!("{folded_icon} Thinking ({tokens} {token_word})");
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

/// Pick the running bullet (`●` / `*`) and hook glyph (`⎿` / `L`) used by tool cards.
///
/// Claude Code uses a single status-colored bullet for the header and a dim
/// hook for the follow-on summary / body — far less visually noisy than the
/// previous `[name] running...` header.
const fn tool_card_glyphs(ascii: bool) -> (&'static str, &'static str) {
    if ascii { ("*", "L") } else { ("\u{25CF}", "\u{23BF}") }
}

/// Status → header/result marker + marker color.
const fn tool_status_marker(status: ToolStatus, ascii: bool) -> (&'static str, Color) {
    match status {
        ToolStatus::Running => {
            if ascii {
                ("*", Color::White)
            } else {
                ("\u{25CF}", Color::White)
            }
        }
        ToolStatus::Done => {
            if ascii {
                ("v", Color::Green)
            } else {
                ("\u{2713}", Color::Green)
            }
        }
        ToolStatus::Error => {
            if ascii {
                ("x", Color::Red)
            } else {
                ("\u{2717}", Color::Red)
            }
        }
    }
}

// ── BUG-11 / UX-E: sub-agent preview visibility ─────────────────────────────
//
// The delegate / sessions_spawn / subagents family of tools each spawn a *child*
// agent that runs its own LLM turns and tool calls. Without observer streaming
// (NoopObserver lives behind the tools boundary), the TUI only sees the parent
// tool card spin and then a flat text result. To keep the card shape consistent
// with Claude-like tool rendering, we surface the agent/model/task in the normal
// `tool(preview)` header instead of a separate robot card.

/// Names of the tools that spawn / drive a sub-agent. A card for any of these
/// gets an enriched readable args preview.
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
/// - [`FocusTarget::Session`] → blue bold `<kind> #N ▸ ` (or `<kind> #N > `
///   under ASCII fallback). The literal kind + sequence text carries the meaning
///   even with styling stripped.
///
/// Returns the [`Span`] plus its column width so the continuation rows and the
/// terminal cursor can align under the typed text.
fn prompt_indicator(
    focus: crate::chat::sessions::FocusTarget,
    ascii: bool,
    session_kind: Option<&str>,
) -> (Span<'static>, usize) {
    match focus {
        crate::chat::sessions::FocusTarget::Main => {
            // Calmer dim cyan `> ` (matches the long-standing Claude Code prompt).
            let span = Span::styled("> ", Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM));
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

fn focused_session_kind<V: BottomChromeView + ?Sized>(state: &V, seq: u64) -> Option<&str> {
    if let Some(view) = state.active_session_view().filter(|view| view.seq == seq) {
        return Some(view.kind.as_str());
    }
    state
        .sessions_entries()
        .iter()
        .find(|entry| entry.seq == seq)
        .map(|entry| entry.kind)
}

struct WrappedInputRow {
    logical_idx: usize,
    start: usize,
    end: usize,
    text: String,
    first_for_logical: bool,
}

fn input_prompt_width<V: BottomChromeView + ?Sized>(state: &V) -> usize {
    let session_kind = state
        .focus()
        .session_seq()
        .and_then(|seq| focused_session_kind(state, seq));
    let (_, width) = prompt_indicator(state.focus(), state.ascii_fallback(), session_kind);
    width
}

fn input_content_width<V: BottomChromeView + ?Sized>(state: &V, total_width: u16) -> usize {
    usize::from(total_width)
        .saturating_sub(input_prompt_width(state))
        .max(1)
}

fn input_visual_rows_for_width<V: BottomChromeView + ?Sized>(state: &V, total_width: u16) -> usize {
    let content_width = input_content_width(state, total_width);
    state
        .input()
        .display_lines()
        .iter()
        .map(|line| wrap_line_ranges(line, content_width).len())
        .sum::<usize>()
        .max(1)
}

fn wrap_input_rows(display_lines: &[String], content_width: usize) -> Vec<WrappedInputRow> {
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

fn wrap_line_ranges(line: &str, width: usize) -> Vec<(usize, usize)> {
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

fn render_input<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) {
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

fn render_footer(frame: &mut Frame, area: Rect) {
    // Claude Code style: dim gray, middle-dot separators, action-oriented
    // hints rather than key/label pairs.
    // P6b2: Ctrl+G remains PRX's sessions switcher; Claude-style external
    // editor parity is available through the alternate Ctrl+X Ctrl+E chord.
    let footer = Paragraph::new(
        " Ctrl+G sessions \u{00B7} Ctrl+O transcript \u{00B7} /copy latest \u{00B7} drag select/copy \u{00B7} Shift+Enter newline \u{00B7} Ctrl+X Ctrl+E edit \u{00B7} Tab fold \u{00B7} Esc cancel ",
    )
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, area);
}

fn render_session_list_footer<V: BottomChromeView + ?Sized>(frame: &mut Frame, area: Rect, state: &V) -> bool {
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

fn render_fullscreen_footer<V: BottomChromeView + ?Sized>(
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

    static MARKDOWN_CACHE_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
    fn render_delegate_card_shows_readable_identity_preview() {
        // The rendered sub-agent card must surface agent/model/task in the
        // same Claude-like `Tool(preview)` header shape as every other tool.
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
        assert!(header.starts_with("v "), "success marker present: {header}");
        assert!(header.contains("delegate("), "tool name + preview: {header}");
        assert!(
            header.contains("task=\"investigate the bug\""),
            "task summary: {header}"
        );
        assert!(header.contains("agent=researcher"), "agent: {header}");
        assert!(header.contains("model=kimi-2.6"), "model: {header}");
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
        assert!(
            header.contains("shell(command=\"ls /tmp\")"),
            "classic readable header: {header}"
        );
        assert!(!header.contains("[bot]"), "no robot for normal tools: {header}");
    }

    /// Flatten a rendered `Line` into plain text for assertions.
    fn line_to_plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn span_fg(lines: &[Line<'_>], line_idx: usize, span_idx: usize) -> Option<Color> {
        lines
            .get(line_idx)
            .and_then(|line| line.spans.get(span_idx))
            .and_then(|span| span.style.fg)
    }

    fn span_fg_for_content(lines: &[Line<'_>], content: &str) -> Option<Color> {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == content)
            .and_then(|span| span.style.fg)
    }

    #[test]
    fn finalized_assistant_markdown_renders_inline_code_and_fenced_blocks() {
        let _guard = MARKDOWN_CACHE_TEST_LOCK.lock();
        ASSISTANT_MARKDOWN_CACHE.lock().clear();
        let line = ConversationLine::Assistant {
            content: "Use `cargo build`\n```rust\nfn main() {}\n```".to_string(),
        };
        let mut lines: Vec<Line<'_>> = Vec::new();
        render_conversation_line(&mut lines, &line, false);

        let rendered = lines.iter().map(line_to_plain).collect::<Vec<_>>().join("\n");
        assert!(
            rendered.contains("Use cargo build"),
            "inline text rendered: {rendered:?}"
        );
        assert!(rendered.contains("┌─rust"), "fenced code border rendered: {rendered:?}");
        assert!(rendered.contains("fn main() {}"), "code body rendered: {rendered:?}");
        assert_eq!(
            span_fg_for_content(&lines, "cargo build"),
            Some(Color::Yellow),
            "inline code should bridge ANSI yellow into ratatui style"
        );
    }

    #[test]
    fn streaming_assistant_markdown_keeps_cursor_after_highlighted_content() {
        let line = ConversationLine::StreamingAssistant {
            content: "Use `cargo build`".to_string(),
        };
        let mut lines: Vec<Line<'_>> = Vec::new();
        render_conversation_line(&mut lines, &line, false);

        let body = lines.first().map(line_to_plain).expect("streaming body rendered");
        assert!(body.ends_with('\u{258C}'), "cursor remains at streaming tail: {body:?}");
        assert_eq!(
            span_fg_for_content(&lines, "cargo build"),
            Some(Color::Yellow),
            "streaming markdown should use the same ANSI bridge"
        );
    }

    #[test]
    fn large_streaming_markdown_uses_plain_threshold_path_with_cursor() {
        let content = format!(
            "```rust\n{}\n```",
            "fn demo() {}\n".repeat((STREAMING_MARKDOWN_HIGHLIGHT_MAX_BYTES / 12).saturating_add(1))
        );

        let lines = render_streaming_assistant_markdown_lines(&content, false);
        let rendered = lines.iter().map(line_to_plain).collect::<Vec<_>>().join("\n");

        assert!(
            rendered.contains("```rust"),
            "large streaming markdown should stay plain"
        );
        assert!(rendered.ends_with('\u{258C}'), "cursor remains at large streaming tail");
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .all(|span| span.style == Style::default()),
            "large streaming threshold path should avoid expensive highlighted spans"
        );
    }

    #[test]
    fn ansi_bridge_supports_indexed_color_and_skips_non_sgr_csi() {
        let lines = ansi_sgr_to_lines("plain\x1b[2K \x1b[38;5;196mred\x1b[0m done");
        assert_eq!(lines.len(), 1);
        assert_eq!(
            line_to_plain(lines.first().expect("one ANSI bridge line")),
            "plain red done"
        );
        assert_eq!(
            span_fg_for_content(&lines, "red"),
            Some(Color::Indexed(196)),
            "38;5 indexed colour should bridge into ratatui style"
        );
    }

    #[test]
    fn finalized_assistant_markdown_uses_render_cache() {
        let _guard = MARKDOWN_CACHE_TEST_LOCK.lock();
        ASSISTANT_MARKDOWN_CACHE.lock().clear();
        let first = cached_finalized_assistant_markdown_lines("Use `cargo build`");
        let second = cached_finalized_assistant_markdown_lines("Use `cargo build`");
        assert!(
            Arc::ptr_eq(&first, &second),
            "finalized assistant markdown should reuse cached rendered spans"
        );
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
                assert_eq!(args_preview, r#"command="ls -la /tmp""#);
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
            args_preview: r#"command="ls""#.to_string(),
            args_full: r#"{"command":"ls"}"#.to_string(),
            result: None,
            status: ToolStatus::Running,
            elapsed_ms: None,
            folded: true,
        };
        render_conversation_line(&mut lines, &card, false);
        // Claude-Code style: while running we render just the run header
        // (`● run shell(ls)`) with no follow-on summary row yet.
        assert_eq!(lines.len(), 1, "running folded card renders to 1 line");
        let rendered: String = lines
            .first()
            .expect("test: at least one line")
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(rendered.contains("\u{25CF}"), "uses ● bullet: {rendered}");
        assert!(rendered.contains("run "), "shows run marker: {rendered}");
        assert!(
            rendered.contains(r#"shell(command="ls")"#),
            "shows Tool(args) preview: {rendered}"
        );
        assert_eq!(span_fg(&lines, 0, 0), Some(Color::White));
    }

    #[test]
    fn render_folded_tool_card_done_shows_hook_summary() {
        // Claude-Code style follow-on: `  ⎿ output ✓ 234ms · 3 lines · 5B`
        // under the run header once the tool finishes.
        let mut lines: Vec<Line<'_>> = Vec::new();
        let card = ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: r#"command="ls""#.to_string(),
            args_full: r#"{"command":"ls"}"#.to_string(),
            result: Some("a\nb\nc".to_string()),
            status: ToolStatus::Done,
            elapsed_ms: Some(234),
            folded: true,
        };
        render_conversation_line(&mut lines, &card, false);
        assert_eq!(lines.len(), 5, "done folded card renders header + summary + preview");
        let summary: String = lines
            .get(1)
            .expect("test: summary line present")
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(summary.contains("\u{23BF}"), "uses ⎿ hook glyph: {summary}");
        assert!(summary.contains("output"), "labels output stream: {summary}");
        assert!(summary.contains("\u{2713}"), "shows success check: {summary}");
        assert!(summary.contains("234ms"), "shows elapsed ms: {summary}");
        assert!(summary.contains("3 lines"), "shows result line count: {summary}");
        assert!(summary.contains("5B"), "shows result byte count: {summary}");
        assert_eq!(line_to_plain(lines.get(2).expect("test: preview line")), "    │ a");
        assert_eq!(line_to_plain(lines.get(3).expect("test: preview line")), "    │ b");
        assert_eq!(line_to_plain(lines.get(4).expect("test: preview line")), "    │ c");
        assert_eq!(span_fg(&lines, 0, 0), Some(Color::Green));
        assert_eq!(span_fg(&lines, 1, 1), Some(Color::Green));
        assert_eq!(span_fg(&lines, 1, 3), Some(Color::Green));
    }

    #[test]
    fn folded_tool_card_preview_preserves_indent_and_shows_hidden_line_count() {
        let mut lines: Vec<Line<'_>> = Vec::new();
        let result = [
            "    let value = \"你好你好你好你好你好你好你好你好\";",
            "        println!(\"still indented\");",
            "    }",
            "extra hidden line one",
            "extra hidden line two",
        ]
        .join("\n");
        let card = ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: "command=\"cargo test\"".to_string(),
            args_full: r#"{"command":"cargo test"}"#.to_string(),
            result: Some(result),
            status: ToolStatus::Done,
            elapsed_ms: Some(50),
            folded: true,
        };

        render_conversation_line(&mut lines, &card, false);

        let first_preview = line_to_plain(lines.get(2).expect("first preview line"));
        assert!(
            first_preview.starts_with("    │     let value"),
            "folded preview should preserve original code indentation: {first_preview:?}"
        );
        assert!(
            UnicodeWidthStr::width(first_preview.trim_start_matches("    │ ")) <= TOOL_FOLDED_RESULT_PREVIEW_CHARS,
            "folded preview should be width-bounded, not char-count bounded: {first_preview:?}"
        );
        let hidden = line_to_plain(lines.get(5).expect("hidden line count"));
        assert!(hidden.contains("+2 lines"), "hidden line count missing: {hidden:?}");
    }

    #[test]
    fn render_folded_tool_card_error_shows_reason_and_red_marker() {
        let mut lines: Vec<Line<'_>> = Vec::new();
        let card = ConversationLine::ToolResult {
            tool_name: "file_write".to_string(),
            args_preview: "path=src/lib.rs, bytes=3B".to_string(),
            args_full: r#"{"path":"src/lib.rs","content":"abc"}"#.to_string(),
            result: Some("permission denied\nstack trace".to_string()),
            status: ToolStatus::Error,
            elapsed_ms: Some(50),
            folded: true,
        };
        render_conversation_line(&mut lines, &card, false);

        let header = line_to_plain(lines.first().expect("test: header"));
        let summary = line_to_plain(lines.get(1).expect("test: summary"));

        assert!(header.starts_with("\u{2717} run "), "error header marker: {header}");
        assert!(
            summary.contains("error \u{2717} 50ms \u{00B7} permission denied"),
            "error summary: {summary}"
        );
        assert_eq!(span_fg(&lines, 0, 0), Some(Color::Red));
        assert_eq!(span_fg(&lines, 1, 1), Some(Color::Red));
        assert_eq!(
            line_to_plain(lines.get(2).expect("error preview first line")),
            "    │ permission denied",
            "folded error cards should preview the error body"
        );
        assert_eq!(
            line_to_plain(lines.get(3).expect("error preview second line")),
            "    │ stack trace",
            "folded error cards should preview follow-up error lines"
        );
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
        //   row 0  `✓ run shell(command="ls -la /tmp")`
        //   row 1  `  ⎿ input  command="ls -la /tmp"`
        //   row 2  `  ⎿ output ✓ 2 lines · 20B`
        //   row 3+ output body
        assert_eq!(lines.len(), 5, "expanded card line count: {}", lines.len());
        let join = |i: usize| -> String {
            lines
                .get(i)
                .expect("test: line idx")
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert!(join(0).contains("\u{2713}"), "uses ✓ marker: {}", join(0));
        assert!(join(0).contains("run "), "uses run marker: {}", join(0));
        assert!(
            join(0).contains("shell(command=\"ls -la /tmp\")"),
            "shows readable args: {}",
            join(0)
        );
        assert!(join(1).contains("\u{23BF}"), "uses ⎿ hook on input row: {}", join(1));
        assert!(
            join(1).contains("input  command=\"ls -la /tmp\""),
            "input row: {}",
            join(1)
        );
        assert!(join(2).contains("output"), "output label: {}", join(2));
        assert!(join(2).contains("2 lines"), "output metrics: {}", join(2));
        assert!(join(3).contains("total 24"), "first body row: {}", join(3));
        assert!(join(4).contains("drwxrwxrwt"), "second body row: {}", join(4));
    }

    #[test]
    fn render_tool_card_status_glyphs_and_colors() {
        // Running keeps the white bullet; terminal states get explicit markers.
        let (bullet, hook) = tool_card_glyphs(false);
        assert_eq!(bullet, "\u{25CF}", "unicode bullet ●");
        assert_eq!(hook, "\u{23BF}", "unicode hook ⎿");
        assert_eq!(
            tool_status_marker(ToolStatus::Running, false),
            ("\u{25CF}", Color::White)
        );
        assert_eq!(tool_status_marker(ToolStatus::Done, false), ("\u{2713}", Color::Green));
        assert_eq!(tool_status_marker(ToolStatus::Error, false), ("\u{2717}", Color::Red));
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
        assert_eq!(tool_status_marker(ToolStatus::Done, true), ("v", Color::Green));
        assert_eq!(tool_status_marker(ToolStatus::Error, true), ("x", Color::Red));

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

        let mut done_lines: Vec<Line<'_>> = Vec::new();
        let done_card = ConversationLine::ToolResult {
            tool_name: "t".to_string(),
            args_preview: String::new(),
            args_full: "{}".to_string(),
            result: Some("ok".to_string()),
            status: ToolStatus::Done,
            elapsed_ms: Some(1),
            folded: true,
        };
        render_conversation_line(&mut done_lines, &done_card, true);
        let done_header = done_lines.first().expect("test: done header");
        assert!(line_to_plain(done_header).starts_with("v run "), "ASCII success marker");
    }

    #[test]
    fn tool_args_formatter_known_tools_are_readable() {
        assert_eq!(
            build_tool_args_preview(
                "file_read",
                r#"{"path":"src/chat/tui.rs","max_bytes":200}"#,
                ARGS_PREVIEW_MAX_CHARS,
                ARGS_PREVIEW_ELLIPSIS
            ),
            "path=src/chat/tui.rs, limit=200"
        );
        assert_eq!(
            build_tool_args_preview(
                "file_write",
                r#"{"path":"src/lib.rs","content":"hello"}"#,
                ARGS_PREVIEW_MAX_CHARS,
                ARGS_PREVIEW_ELLIPSIS
            ),
            "path=src/lib.rs, bytes=5B"
        );
        assert_eq!(
            build_tool_args_preview(
                "shell",
                r#"{"command":"cargo test -p openprx","cwd":"/opt/worker/code/prx"}"#,
                ARGS_PREVIEW_MAX_CHARS,
                ARGS_PREVIEW_ELLIPSIS
            ),
            "command=\"cargo test -p openprx\", cwd=/opt/worker/code/prx"
        );
        assert_eq!(
            build_tool_args_preview(
                "sessions_spawn",
                r#"{"task":"Audit session UX","model":"kimi-2.6","agent":"reviewer"}"#,
                ARGS_PREVIEW_MAX_CHARS,
                ARGS_PREVIEW_ELLIPSIS
            ),
            "task=\"Audit session UX\", agent=reviewer, model=kimi-2.6"
        );
        assert_eq!(
            build_tool_args_preview(
                "managed_session",
                r#"{"action":"shell","command":"for i in 1 2 3; do echo ok; sleep 1; done"}"#,
                ARGS_PREVIEW_MAX_CHARS,
                ARGS_PREVIEW_ELLIPSIS
            ),
            "action=shell, command=\"for i in 1 2 3; do echo ok; sleep 1; done\""
        );
        assert_eq!(
            build_tool_args_preview(
                "web_fetch",
                r#"{"url":"https://example.com/docs","max_chars":1200}"#,
                ARGS_PREVIEW_MAX_CHARS,
                ARGS_PREVIEW_ELLIPSIS
            ),
            "url=https://example.com/docs, limit=1200"
        );
    }

    #[test]
    fn tool_args_formatter_unknown_uses_compact_json_and_clamps() {
        let preview = build_tool_args_preview(
            "custom_tool",
            r#"{"z":2,"nested":{"long":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}"#,
            40,
            ARGS_PREVIEW_ELLIPSIS,
        );

        assert!(preview.starts_with('{'), "compact JSON fallback: {preview}");
        assert!(preview.ends_with(ARGS_PREVIEW_ELLIPSIS), "clamped fallback: {preview}");
        assert!(!preview.contains('\n'), "single-line fallback: {preview}");
    }

    #[test]
    fn tool_args_formatter_malformed_json_does_not_panic() {
        let preview = build_tool_args_preview("file_read", "{not valid json", 20, ARGS_PREVIEW_ELLIPSIS_ASCII);

        assert_eq!(preview, "{not valid json");
    }

    #[test]
    fn expanded_tool_output_truncates_long_body() {
        let mut lines: Vec<Line<'_>> = Vec::new();
        let output = (0..(TOOL_EXPANDED_OUTPUT_MAX_LINES + 3))
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let card = ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: "command=\"generate\"".to_string(),
            args_full: r#"{"command":"generate"}"#.to_string(),
            result: Some(output),
            status: ToolStatus::Done,
            elapsed_ms: Some(10),
            folded: false,
        };

        render_conversation_line(&mut lines, &card, false);
        let rendered = lines.iter().map(line_to_plain).collect::<Vec<_>>().join("\n");

        assert!(
            rendered.contains("truncated:"),
            "long output has truncation summary: {rendered}"
        );
        assert!(
            rendered.contains("Ctrl+O for full transcript"),
            "expanded truncation hint should point to the verbose transcript: {rendered}"
        );
        assert!(
            lines.len() <= TOOL_EXPANDED_OUTPUT_MAX_LINES + 4,
            "expanded output is bounded: {}",
            lines.len()
        );
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
        // Folded summary: `▸ Thinking (N tokens)` without noisy key-hint prose.
        assert!(rendered.starts_with("\u{25B8} "), "uses ▸ folded icon: {rendered}");
        assert!(rendered.contains("Thinking"), "shows Thinking label: {rendered}");
        assert!(rendered.contains("tokens"), "shows token count: {rendered}");
        assert!(
            !rendered.contains("press Tab"),
            "folded header trims noisy Tab prose: {rendered}"
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
            !rendered.contains("press Tab"),
            "ASCII folded header trims noisy Tab prose: {rendered}"
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
    fn mouse_click_on_reasoning_header_toggles_fold_state() {
        let mut state = TuiState::new("p", "m");
        assert!(state.push_reasoning("first step\nsecond step"));
        let scroll = FullscreenTranscriptScroll::default();

        assert!(toggle_reasoning_at_fullscreen_point(&mut state, &scroll, 80, 24, 0, 0));
        match state.conversation_lines.last() {
            Some(ConversationLine::Reasoning { folded, .. }) => assert!(!*folded, "click expands folded card"),
            other => panic!("test: expected Reasoning, got {other:?}"),
        }

        assert!(
            !toggle_reasoning_at_fullscreen_point(&mut state, &scroll, 80, 24, 0, 1),
            "clicking expanded body must not toggle"
        );
        match state.conversation_lines.last() {
            Some(ConversationLine::Reasoning { folded, .. }) => assert!(!*folded, "body click leaves card expanded"),
            other => panic!("test: expected Reasoning, got {other:?}"),
        }

        assert!(toggle_reasoning_at_fullscreen_point(&mut state, &scroll, 80, 24, 0, 0));
        match state.conversation_lines.last() {
            Some(ConversationLine::Reasoning { folded, .. }) => assert!(*folded, "second header click collapses"),
            other => panic!("test: expected Reasoning, got {other:?}"),
        }
    }

    #[test]
    fn mouse_click_outside_transcript_does_not_toggle_reasoning() {
        let mut state = TuiState::new("p", "m");
        assert!(state.push_reasoning("first step"));
        let scroll = FullscreenTranscriptScroll::default();

        assert!(
            !toggle_reasoning_at_fullscreen_point(&mut state, &scroll, 80, 6, 0, 5),
            "bottom chrome row is outside the transcript"
        );
        match state.conversation_lines.last() {
            Some(ConversationLine::Reasoning { folded, .. }) => assert!(*folded, "outside click leaves default fold"),
            other => panic!("test: expected Reasoning, got {other:?}"),
        }
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
    fn p2_10_alt_enter_inserts_newline() {
        let mut input = TuiInput::new();
        type_str(&mut input, "a");
        let out = input.handle_key(key_mod(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(out, InputOutcome::Consumed);
        type_str(&mut input, "b");
        assert_eq!(input.text(), "a\nb");
        assert_eq!(input.lines.len(), 2);
        assert!(!input.is_single_line());
    }

    #[test]
    fn p2_10_backslash_enter_continues_without_submitting() {
        let mut input = TuiInput::new();
        type_str(&mut input, "echo \\");
        let out = input.handle_key(key(KeyCode::Enter));
        assert_eq!(out, InputOutcome::Consumed);
        assert_eq!(input.text(), "echo \n");
        assert!(input.history.is_empty(), "continuation does not submit");
        type_str(&mut input, "next");
        let out = input.handle_key(key(KeyCode::Enter));
        assert_eq!(out, InputOutcome::Submitted("echo \nnext".to_string()));
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
        assert!(menu.entries.iter().any(|entry| entry.label == "/help"));
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
        assert!(menu.entries.iter().any(|entry| entry.label == "/model"));
        assert!(
            menu.entries
                .iter()
                .all(|entry| entry.label.trim_start_matches('/').contains("mo")),
            "U5 /mo filter should only include matching command names: {:?}",
            menu.entries
        );
    }

    #[test]
    fn slash_menu_ignores_description_only_matches() {
        let mut state = TuiState::new("p", "m");
        for ch in "/conversation".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        assert_eq!(state.input.text(), "/conversation");
        assert!(
            state.slash_menu.is_none(),
            "description-only matches must not keep slash menu open"
        );
    }

    #[test]
    fn slash_menu_closes_when_filter_has_no_matches() {
        let mut state = TuiState::new("p", "m");
        for ch in "/zzzz".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        assert_eq!(state.input.text(), "/zzzz");
        assert!(state.slash_menu.is_none(), "no matching overlay must close");
    }

    #[test]
    fn slash_menu_only_triggers_at_first_line_start() {
        let mut state = TuiState::new("p", "m");
        state.input.lines = vec!["open".to_string(), String::new()];
        state.input.cursor = (1, 0);
        for ch in "/he".chars() {
            let _ = dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
        }

        assert_eq!(state.input.text(), "open\n/he");
        assert!(
            state.slash_menu.is_none(),
            "second-line slash command prefix with matches must not open slash menu"
        );
    }

    #[test]
    fn slash_menu_overlay_rect_stays_above_bottom_chrome() {
        let frame = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 30,
        };
        let bottom_chrome_height = 5;
        let menu = SlashMenuState::new("");

        let rect = slash_menu_overlay_rect(frame, &menu, bottom_chrome_height);

        assert!(rect.width <= 80, "slash menu width should be capped: {rect:?}");
        assert_eq!(rect.x, 2, "slash menu should keep a horizontal margin");
        assert!(
            rect.y.saturating_add(rect.height) <= frame.height.saturating_sub(bottom_chrome_height),
            "slash menu should sit above bottom chrome: {rect:?}"
        );
        assert!(rect.height >= 1, "slash menu should remain visible: {rect:?}");
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
    fn slash_menu_enter_submits_no_arg_command() {
        let mut state = TuiState::new("p", "m");
        for ch in "/he".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Submitted("/help".to_string())
        );
        assert!(state.input.is_empty());
        assert!(state.slash_menu.is_none());
    }

    #[test]
    fn slash_menu_tab_inserts_command_and_esc_clears_command_trigger() {
        let mut tab_state = TuiState::new("p", "m");
        for ch in "/expo".chars() {
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
        assert!(
            tab_state
                .slash_menu
                .as_ref()
                .is_some_and(|menu| menu.entries.iter().any(|entry| entry.label == "json")),
            "selecting /export should drill down to format candidates"
        );

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
        assert!(esc_state.input.is_empty(), "Esc clears slash-command draft");
        assert!(esc_state.slash_menu.is_none(), "Esc closes menu");
    }

    #[test]
    fn slash_command_submit_leaves_input_render_empty() {
        let mut state = TuiState::new("p", "m");
        for ch in "/help".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Submitted("/help".to_string())
        );
        assert!(state.input.is_empty());
        assert_eq!(state.input.display_lines(), vec![String::new()]);

        let mut scroll = FullscreenTranscriptScroll::default();
        let rows = fullscreen_rows(&state, 90, 24, &mut scroll);
        let joined = rows.join("\n");
        assert!(
            !joined.contains("/help"),
            "submitted slash command must not remain as bright input text: {joined}"
        );
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
    fn slash_menu_export_arg_shows_static_candidates() {
        let mut state = TuiState::new("p", "m");
        for ch in "/export ".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        let menu = state.slash_menu.as_ref().expect("export arg menu open");
        assert!(menu.entries.iter().any(|entry| entry.label == "md"));
        assert!(menu.entries.iter().any(|entry| entry.label == "json"));
    }

    #[test]
    fn slash_menu_free_text_command_has_no_second_level_menu() {
        let mut state = TuiState::new("p", "m");
        for ch in "/bg ".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        assert!(state.slash_menu.is_none(), "free-text /bg should not force candidates");
    }

    #[test]
    fn at_path_menu_opens_at_word_start_and_inserts_file_with_space() {
        let mut state = TuiState::new("p", "m");
        state.input.set_text("inspect @ca");

        state.update_at_path_candidates(vec![AtPathCandidate {
            path: "Cargo.toml".to_string(),
            is_dir: false,
        }]);

        let menu = state.slash_menu.as_ref().expect("@path menu open");
        assert_eq!(menu.filter, "ca");
        assert_eq!(
            menu.entries.first().map(|entry| entry.label.as_str()),
            Some("Cargo.toml")
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.input.text(), "inspect @Cargo.toml ");
        assert_eq!(state.input.cursor, (0, "inspect @Cargo.toml ".len()));
    }

    #[test]
    fn at_path_menu_keeps_directory_candidate_open_for_drilldown() {
        let mut state = TuiState::new("p", "m");
        state.input.set_text("@s");

        state.update_at_path_candidates(vec![AtPathCandidate {
            path: "src/".to_string(),
            is_dir: true,
        }]);

        assert_eq!(
            dispatch_global_key(key(KeyCode::Tab), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.input.text(), "@src/");
        assert!(
            state.input.text().ends_with('/'),
            "directory completion must not append a separating space"
        );
    }

    #[test]
    fn at_path_menu_requires_word_start() {
        let mut state = TuiState::new("p", "m");
        state.input.set_text("mail@example");

        state.update_at_path_candidates(vec![AtPathCandidate {
            path: "example.rs".to_string(),
            is_dir: false,
        }]);

        assert!(state.slash_menu.is_none(), "email-like @ must not open path menu");
    }

    #[test]
    fn slash_menu_kill_arg_uses_live_session_cache() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![crate::chat::sessions::SwitcherEntry {
            seq: 7,
            kind: "agent",
            origin: "user",
            status: "running",
            title: "build release".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }];
        for ch in "/kill ".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        let menu = state.slash_menu.as_ref().expect("kill arg menu open");
        assert!(
            menu.entries
                .iter()
                .any(|entry| entry.label == "#7" && entry.description.contains("build release")),
            "live session row rendered from cache: {:?}",
            menu.entries
        );
    }

    #[test]
    fn slash_menu_provider_and_model_candidates_degrade_from_catalog() {
        let mut state = TuiState::new("openai", "gpt-5.2");
        state.provider_model_catalog = vec![SlashProviderModelCatalog {
            provider: "openai".to_string(),
            models: vec![SlashModelCandidate {
                name: "gpt-5.2".to_string(),
                description: "Configured default model".to_string(),
            }],
        }];
        for ch in "/provider ".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }
        assert!(
            state
                .slash_menu
                .as_ref()
                .expect("provider menu open")
                .entries
                .iter()
                .any(|entry| entry.label == "openai")
        );

        for ch in "openai ".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }
        let menu = state.slash_menu.as_ref().expect("provider model menu open");
        assert!(menu.entries.iter().any(|entry| entry.label == "gpt-5.2"));
    }

    #[test]
    fn slash_menu_model_arg_uses_current_provider_models() {
        let mut state = TuiState::new("openai", "gpt-5.2");
        state.provider_model_catalog = vec![SlashProviderModelCatalog {
            provider: "openai".to_string(),
            models: vec![SlashModelCandidate {
                name: "gpt-5-mini".to_string(),
                description: "Router model candidate".to_string(),
            }],
        }];
        for ch in "/model ".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        let menu = state.slash_menu.as_ref().expect("model arg menu open");
        assert!(menu.entries.iter().any(|entry| entry.label == "gpt-5-mini"));
    }

    #[test]
    fn slash_menu_enter_inserts_argument_candidate_token() {
        let mut state = TuiState::new("p", "m");
        for ch in "/export j".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.input.text(), "/export json ");
        assert!(state.slash_menu.is_none());
    }

    #[test]
    fn slash_menu_enter_inserts_argument_candidate_at_trailing_space() {
        let mut state = TuiState::new("p", "m");
        for ch in "/export ".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        let menu = state.slash_menu.as_ref().expect("export arg menu open");
        assert_eq!(menu.selected_entry().map(|entry| entry.label.as_str()), Some("md"));
        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.input.text(), "/export md ");
        assert!(state.slash_menu.is_none());
    }

    #[test]
    fn slash_menu_down_enter_inserts_argument_candidate_at_trailing_space() {
        let mut state = TuiState::new("p", "m");
        for ch in "/export ".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::Consumed
        );
        let menu = state.slash_menu.as_ref().expect("export arg menu open");
        assert_eq!(menu.selected_entry().map(|entry| entry.label.as_str()), Some("json"));
        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.input.text(), "/export json ");
        assert!(state.slash_menu.is_none());
    }

    #[test]
    fn input_history_edit_then_up_down_steps_clean_entries() {
        let mut state = TuiState::new("p", "m");
        state.input.history.push("one".to_string());
        state.input.history.push("two".to_string());

        assert_eq!(dispatch_global_key(key(KeyCode::Up), &mut state), KeyDispatch::Consumed);
        assert_eq!(state.input.text(), "two");
        assert_eq!(
            dispatch_global_key(key(KeyCode::Char('!')), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.input.text(), "two!");
        assert_eq!(dispatch_global_key(key(KeyCode::Up), &mut state), KeyDispatch::Consumed);
        assert_eq!(state.input.text(), "two", "Up restarts from a clean history entry");
        assert_eq!(dispatch_global_key(key(KeyCode::Up), &mut state), KeyDispatch::Consumed);
        assert_eq!(state.input.text(), "one");
        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.input.text(), "two");
        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.input.text(), "two!", "Down restores the edited draft");
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
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
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
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
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
    fn approval_child_ctrl_c_keeps_global_interrupt_semantics() {
        let mut state = approval_state();

        let out = dispatch_global_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut state);

        assert_eq!(out, KeyDispatch::InterruptTurn);
        assert!(
            state.pending_tool_approval.is_none(),
            "Ctrl+C must clear mirror approval so later input is not swallowed"
        );
        assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
    }

    #[test]
    fn approval_child_ctrl_c_allows_next_message_submission() {
        let mut state = approval_state();

        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut state),
            KeyDispatch::InterruptTurn
        );
        for ch in "next".chars() {
            assert_eq!(
                dispatch_global_key(key(KeyCode::Char(ch)), &mut state),
                KeyDispatch::Consumed
            );
        }

        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Submitted("next".to_string())
        );
        assert!(state.pending_tool_approval.is_none());
        assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);
    }

    #[test]
    fn overlay_open_ctrl_c_and_empty_ctrl_d_keep_global_semantics() {
        let mut state = TuiState::new("p", "m");
        for ch in "/mo".chars() {
            let _ = dispatch_global_key(key(KeyCode::Char(ch)), &mut state);
        }
        assert!(state.slash_menu.is_some());

        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut state),
            KeyDispatch::InterruptTurn
        );
        state.input.clear();
        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Char('d'), KeyModifiers::CONTROL), &mut state),
            KeyDispatch::Exit
        );
    }

    #[test]
    fn slash_menu_refresh_preserves_selected_command_row() {
        let mut state = TuiState::new("p", "m");
        let _ = dispatch_global_key(key(KeyCode::Char('/')), &mut state);
        assert!(state.slash_menu.as_ref().is_some_and(|menu| menu.len() > 1));

        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.slash_menu.as_ref().map(|menu| menu.selected), Some(1));

        state.update_at_path_candidates(Vec::new());

        assert_eq!(
            state.slash_menu.as_ref().map(|menu| menu.selected),
            Some(1),
            "unrelated @path refresh must not reset slash command selection"
        );
    }

    #[test]
    fn at_path_refresh_preserves_selected_candidate_for_enter() {
        let mut state = TuiState::new("p", "m");
        state.input.set_text("@s");
        let candidates = vec![
            AtPathCandidate {
                path: "src/".to_string(),
                is_dir: true,
            },
            AtPathCandidate {
                path: "setup.rs".to_string(),
                is_dir: false,
            },
        ];
        state.update_at_path_candidates(candidates.clone());
        assert_eq!(state.slash_menu.as_ref().map(|menu| menu.selected), Some(0));

        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.slash_menu.as_ref().map(|menu| menu.selected), Some(1));

        state.update_at_path_candidates(candidates);
        assert_eq!(
            state.slash_menu.as_ref().map(|menu| menu.selected),
            Some(1),
            "unchanged @path refresh must keep the highlighted row"
        );

        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(
            state.input.text(),
            "@setup.rs ",
            "Enter must insert the row that was still highlighted after refresh"
        );
    }

    #[test]
    fn esc_during_generation_interrupts_before_clearing_input() {
        let mut state = TuiState::new("p", "m");
        state.input.set_text("draft text");
        state.start_stream("draft-1");

        assert_eq!(
            dispatch_global_key(key(KeyCode::Esc), &mut state),
            KeyDispatch::InterruptTurn
        );
        assert_eq!(state.input.text(), "draft text");
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
            KeyDispatch::StripSelectionChanged { selected: Some(1) },
            "main+empty Right selects the first bottom-rail session"
        );
        state.strip_selection = None;
        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::StripSelectionChanged { selected: Some(1) },
            "main+empty Down selects the first bottom-list session"
        );
        assert_eq!(state.strip_selection, Some(1));
        assert_eq!(
            dispatch_global_key(key(KeyCode::Up), &mut state),
            KeyDispatch::StripSelectionChanged {
                selected: Some(MAIN_SESSION_SELECTION_SEQ)
            },
            "main+empty Up moves from first child back to main"
        );
        assert_eq!(state.strip_selection, Some(MAIN_SESSION_SELECTION_SEQ));
        assert_eq!(
            dispatch_global_key(key(KeyCode::Left), &mut state),
            KeyDispatch::StripSelectionChanged { selected: Some(3) },
            "main+empty Left wraps from main to the last child session"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::AttachSession { seq: 3 },
            "main+empty Enter attaches the selected bottom-rail session"
        );
        assert_eq!(
            state.strip_selection, None,
            "attach consumes the UI-only strip selection so Esc detaches the child view next"
        );

        state.strip_selection = None;
        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 2 };
        assert_eq!(
            dispatch_global_key(key(KeyCode::Right), &mut state),
            KeyDispatch::SwitchSession { seq: 3 },
            "session+empty Right switches to the visual neighbor on the right"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Up), &mut state),
            KeyDispatch::SwitchSession { seq: 1 },
            "session+empty Up switches to the visual neighbor above"
        );
        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 1 };
        assert_eq!(
            dispatch_global_key(key(KeyCode::Up), &mut state),
            KeyDispatch::RequestDetach,
            "first child+empty Up switches back to main"
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
    fn bottom_directional_selection_skips_completed_history_rows() {
        let mut state = TuiState::new("p", "m");
        let mut completed = entry(1);
        completed.status = "completed";
        state.sessions_cache = vec![completed, entry(2)];

        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::StripSelectionChanged { selected: Some(2) },
            "bare Down should select the first active child, not completed history"
        );
        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::AttachSession { seq: 2 },
            "Enter attaches the visible active child"
        );

        let mut history_only = TuiState::new("p", "m");
        let mut done = entry(1);
        done.status = "completed";
        history_only.sessions_cache = vec![done];

        assert_eq!(
            dispatch_global_key(key(KeyCode::Char('x')), &mut history_only),
            KeyDispatch::Consumed
        );
        assert_eq!(
            history_only.input.text(),
            "x",
            "history-only completed sessions must not swallow normal input"
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
        let now = chrono::Utc::now();
        crate::chat::sessions::SwitcherEntry {
            seq,
            kind: "agent",
            origin: "user",
            status: "running",
            title: format!("task {seq}"),
            created_at: now,
            updated_at: now,
            token_usage_records: Vec::new(),
            idle_warning: false,
        }
    }

    fn kind_entry(seq: u64, kind: &'static str) -> crate::chat::sessions::SwitcherEntry {
        let mut entry = entry(seq);
        entry.kind = kind;
        entry
    }

    #[test]
    fn alt_arrows_move_ui_only_strip_selection_without_focus_change() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1), entry(2), entry(3)];

        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Right, KeyModifiers::ALT), &mut state),
            KeyDispatch::StripSelectionChanged { selected: Some(1) }
        );
        assert_eq!(state.strip_selection, Some(1));
        assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Main);

        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Right, KeyModifiers::ALT), &mut state),
            KeyDispatch::StripSelectionChanged { selected: Some(2) }
        );
        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Left, KeyModifiers::ALT), &mut state),
            KeyDispatch::StripSelectionChanged { selected: Some(1) }
        );
        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Up, KeyModifiers::ALT), &mut state),
            KeyDispatch::StripSelectionChanged { selected: Some(3) }
        );
        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Down, KeyModifiers::ALT), &mut state),
            KeyDispatch::StripSelectionChanged { selected: Some(1) }
        );
        assert_eq!(
            state.focus,
            crate::chat::sessions::FocusTarget::Main,
            "strip selection must not change input-routing focus"
        );
    }

    #[test]
    fn alt_arrows_seed_from_focused_session_when_no_strip_selection_exists() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1), entry(2), entry(3)];
        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 2 };

        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Right, KeyModifiers::ALT), &mut state),
            KeyDispatch::StripSelectionChanged { selected: Some(3) }
        );
        assert_eq!(state.focus, crate::chat::sessions::FocusTarget::Session { seq: 2 });
    }

    #[test]
    fn alt_enter_attaches_selected_strip_entry_for_all_session_kinds() {
        for (seq, kind) in [(1, "agent"), (2, "shell"), (3, "pty")] {
            let mut state = TuiState::new("p", "m");
            state.sessions_cache = vec![kind_entry(seq, kind)];
            state.strip_selection = Some(seq);

            assert_eq!(
                dispatch_global_key(key_mod(KeyCode::Enter, KeyModifiers::ALT), &mut state),
                KeyDispatch::AttachSession { seq },
                "Alt+Enter reuses the single attach dispatch for {kind}"
            );
            assert_eq!(
                state.strip_selection, None,
                "Alt+Enter attach consumes the UI-only strip selection for {kind}"
            );
        }
    }

    #[test]
    fn alt_enter_without_strip_selection_falls_through_to_newline_insert() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1)];
        dispatch_global_key(key(KeyCode::Char('a')), &mut state);

        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Enter, KeyModifiers::ALT), &mut state),
            KeyDispatch::Consumed
        );
        dispatch_global_key(key(KeyCode::Char('b')), &mut state);
        assert_eq!(state.input.text(), "a\nb");

        let mut shift_state = TuiState::new("p", "m");
        dispatch_global_key(key(KeyCode::Char('x')), &mut shift_state);
        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Enter, KeyModifiers::SHIFT), &mut shift_state),
            KeyDispatch::Consumed
        );
        dispatch_global_key(key(KeyCode::Char('y')), &mut shift_state);
        assert_eq!(shift_state.input.text(), "x\ny", "Shift+Enter still inserts newline");
    }

    #[test]
    fn alt_enter_stale_strip_selection_is_consumed_with_session_gone_status() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1)];
        state.strip_selection = Some(2);
        dispatch_global_key(key(KeyCode::Char('a')), &mut state);

        assert_eq!(
            dispatch_global_key(key_mod(KeyCode::Enter, KeyModifiers::ALT), &mut state),
            KeyDispatch::Consumed
        );
        assert_eq!(state.strip_selection, None);
        assert_eq!(state.input.text(), "a", "stale Alt+Enter must not insert a newline");
        assert!(
            matches!(
                state.conversation_lines.last(),
                Some(ConversationLine::System { content }) if content == "session gone"
            ),
            "stale selection should surface a status message"
        );
    }

    #[test]
    fn esc_clears_strip_selection_before_normal_cancel_or_detach() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1)];
        state.strip_selection = Some(1);
        dispatch_global_key(key(KeyCode::Char('x')), &mut state);

        assert_eq!(
            dispatch_global_key(key(KeyCode::Esc), &mut state),
            KeyDispatch::StripSelectionChanged { selected: None }
        );
        assert_eq!(state.strip_selection, None);
        assert_eq!(
            state.input.text(),
            "x",
            "Esc clears strip highlight before touching input"
        );
    }

    #[test]
    fn bare_arrows_history_cursor_and_child_scroll_are_not_stolen_by_strip_selection() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1), entry(2)];
        state.strip_selection = Some(2);
        dispatch_global_key(key(KeyCode::Char('a')), &mut state);
        assert_eq!(
            dispatch_global_key(key(KeyCode::Enter), &mut state),
            KeyDispatch::Submitted("a".to_string())
        );

        assert_eq!(dispatch_global_key(key(KeyCode::Up), &mut state), KeyDispatch::Consumed);
        assert_eq!(state.input.text(), "a", "bare Up still recalls input history");
        assert_eq!(state.strip_selection, Some(2));

        assert_eq!(
            dispatch_global_key(key(KeyCode::Left), &mut state),
            KeyDispatch::Consumed,
            "bare Left still edits the input cursor"
        );
        assert_eq!(state.input.cursor, (0, 0));

        state.input.clear();
        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 1 };
        state.active_session_view = Some(crate::chat::sessions::ActiveSessionView {
            seq: 1,
            kind: "agent".to_string(),
            title: "task 1".to_string(),
            lines: vec!["one".to_string(), "two".to_string(), "three".to_string()],
            truncated: false,
            scroll_offset: 0,
        });
        assert_eq!(
            dispatch_global_key(key(KeyCode::PageUp), &mut state),
            KeyDispatch::PageSessionUp,
            "PageUp keeps the focused child scroll binding"
        );
    }

    #[test]
    fn ctrl_g_still_opens_switcher_when_strip_selection_exists() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1), entry(2)];
        state.strip_selection = Some(2);

        let out = dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);

        assert!(matches!(out, KeyDispatch::SwitcherOpened { .. }));
        assert!(state.switcher.is_some(), "Ctrl+G modal remains available");
        assert_eq!(state.strip_selection, Some(2), "modal does not reuse routing focus");
    }

    fn usage_record(
        source: crate::llm::route_decision::TokenUsageSource,
    ) -> crate::llm::route_decision::MeteredTokenUsageRecord {
        crate::llm::route_decision::MeteredTokenUsageRecord {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            prompt_tokens: 8_000,
            completion_tokens: 4_300,
            total_tokens: 12_300,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            source,
            cost_usd: Some(0.0042),
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
        let now = chrono::Utc::now();
        crate::chat::sessions::SwitcherEntry {
            seq,
            kind: "pty",
            origin: "user",
            status: "running",
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            token_usage_records: Vec::new(),
            idle_warning: false,
        }
    }

    fn elapsed_entry(seq: u64, status: &'static str, elapsed_seconds: i64) -> crate::chat::sessions::SwitcherEntry {
        let created_at = chrono::DateTime::parse_from_rfc3339("2026-07-04T12:00:00Z")
            .expect("test timestamp")
            .with_timezone(&chrono::Utc);
        crate::chat::sessions::SwitcherEntry {
            seq,
            kind: "agent",
            origin: "model",
            status,
            title: format!("elapsed task {seq}"),
            created_at,
            updated_at: created_at + chrono::Duration::seconds(elapsed_seconds),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }
    }

    fn long_strip_entries(count: u64) -> Vec<crate::chat::sessions::SwitcherEntry> {
        (1..=count)
            .map(|seq| {
                let mut entry = elapsed_entry(seq, "running", i64::try_from(seq).unwrap_or(i64::MAX));
                entry.title =
                    format!("List 3 strengths of a terminal chat UI. Return only concise bullets for run {seq}");
                entry
            })
            .collect()
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
    fn switcher_row_includes_elapsed() {
        let e = elapsed_entry(4, "running", 63);
        let row = render_switcher_row(&e, "⏳", false, 80);
        assert!(row.contains("1m03s"), "row carries compact elapsed: {row}");
    }

    #[test]
    fn switcher_row_includes_subsession_token_usage() {
        let mut e = elapsed_entry(4, "completed", 63);
        e.token_usage_records = vec![usage_record(crate::llm::route_decision::TokenUsageSource::Reported)];

        let row = render_switcher_row(&e, "✓", false, 96);

        assert!(
            row.contains("1m03s 12.3k tok | $0.0042"),
            "row carries elapsed plus usage: {row}"
        );
    }

    #[test]
    fn sessions_strip_marks_estimated_subsession_usage() {
        let mut e = elapsed_entry(4, "running", 63);
        e.token_usage_records = vec![usage_record(crate::llm::route_decision::TokenUsageSource::Estimated)];

        let line = render_sessions_strip_line(
            &[e],
            "",
            crate::chat::sessions::FocusTarget::Session { seq: 4 },
            true,
            96,
        );

        assert!(
            line.contains("1m03s ~12.3k tok | $0.0042"),
            "strip carries estimated usage marker: {line}"
        );
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
        assert!(!session_footer_has_sessions(&state));
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
    fn sessions_strip_entry_shows_elapsed() {
        let entries = vec![elapsed_entry(2, "running", 3)];
        let line = render_sessions_strip_line(&entries, "", crate::chat::sessions::FocusTarget::Main, false, 80);
        assert!(line.contains("3s"), "strip carries compact elapsed: {line}");
    }

    #[test]
    fn sessions_strip_entry_shows_idle_warning() {
        let mut entry = elapsed_entry(2, "running", 601);
        entry.kind = "shell";
        entry.idle_warning = true;
        let line = render_sessions_strip_line(&[entry], "", crate::chat::sessions::FocusTarget::Main, false, 80);
        assert!(line.contains("⚠ idle"), "strip carries idle warning: {line}");
    }

    #[test]
    fn sessions_strip_multiple_entries_share_one_row() {
        let entries = vec![entry(1), entry(2)];
        let line = render_sessions_strip_line(&entries, "", crate::chat::sessions::FocusTarget::Main, false, 80);
        assert!(line.contains("#1"), "first session visible: {line}");
        assert!(line.contains("#2"), "second session visible: {line}");
        assert!(line.contains(" #2"), "entries separated in one row: {line}");
    }

    #[test]
    fn sessions_strip_long_titles_render_multiple_compact_chips() {
        let entries = long_strip_entries(6);
        let line = render_sessions_strip_line(&entries, "", crate::chat::sessions::FocusTarget::Main, false, 94);

        assert!(line.contains("#1"), "first compact chip visible: {line}");
        assert!(line.contains("#2"), "second compact chip visible: {line}");
        assert!(line.contains("#3"), "third compact chip visible: {line}");
        assert!(
            !line.contains("Return only concise bullets"),
            "strip keeps long descriptions out of compact chips: {line}"
        );
        assert!(
            UnicodeWidthStr::width(line.as_str()) <= 94,
            "compact strip must fit 94 cols, got {}: {line}",
            UnicodeWidthStr::width(line.as_str())
        );
    }

    #[test]
    fn sessions_strip_selection_beyond_initial_window_is_visible_and_highlighted() {
        let entries = long_strip_entries(8);
        let line = render_sessions_strip_styled_line(
            &entries,
            "",
            crate::chat::sessions::FocusTarget::Main,
            Some(8),
            false,
            58,
        );
        let plain = line_to_plain(&line);

        assert!(plain.contains("#8"), "selected seq is windowed into view: {plain}");
        assert!(
            plain.contains('\u{2039}'),
            "leading overflow indicator appears: {plain}"
        );

        let selected = line
            .spans
            .iter()
            .find(|span| span.content.contains("#8"))
            .expect("test: selected span");
        assert_eq!(selected.style.bg, Some(Color::Cyan));
        assert_eq!(selected.style.fg, Some(Color::Black));
        assert!(selected.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn sessions_strip_active_entry_drives_window_when_no_selection() {
        let entries = long_strip_entries(8);
        let line = render_sessions_strip_line(
            &entries,
            "",
            crate::chat::sessions::FocusTarget::Session { seq: 7 },
            false,
            58,
        );

        assert!(line.contains("#7"), "active seq is windowed into view: {line}");
        assert!(line.contains('\u{25B8}'), "active marker remains distinct: {line}");
        assert!(line.contains('\u{2039}'), "leading overflow indicator appears: {line}");
    }

    #[test]
    fn sessions_strip_overflow_indicator_has_ascii_fallback() {
        let entries = long_strip_entries(8);
        let line = render_sessions_strip_line_with_selection(
            &entries,
            "",
            crate::chat::sessions::FocusTarget::Main,
            Some(3),
            true,
            58,
        );

        assert!(line.contains("#3"), "selected seq is visible in ASCII mode: {line}");
        assert!(
            line.contains('>') && line.contains('+'),
            "ASCII right overflow/count affordance appears when entries remain hidden: {line}"
        );
    }

    #[test]
    fn sessions_strip_zero_width_and_one_entry_edge_cases_do_not_panic() {
        let entries = long_strip_entries(1);
        let empty = render_sessions_strip_line_with_selection(
            &entries,
            "",
            crate::chat::sessions::FocusTarget::Session { seq: 1 },
            Some(1),
            false,
            0,
        );
        assert!(empty.is_empty());

        let one = render_sessions_strip_line_with_selection(
            &entries,
            "",
            crate::chat::sessions::FocusTarget::Session { seq: 1 },
            Some(1),
            false,
            18,
        );
        assert!(one.contains("#1"), "single selected chip still renders: {one}");
        assert!(
            UnicodeWidthStr::width(one.as_str()) <= 18,
            "single-chip strip must fit narrow width: {one}"
        );
    }

    #[test]
    fn sessions_strip_selected_entry_is_highlighted_separately_from_focus() {
        let entries = vec![entry(1), entry(2)];
        let line = render_sessions_strip_styled_line(
            &entries,
            "",
            crate::chat::sessions::FocusTarget::Session { seq: 1 },
            Some(2),
            false,
            96,
        );
        let plain = line_to_plain(&line);
        assert!(plain.contains("#1"), "focused entry remains visible: {plain}");
        assert!(plain.contains("#2"), "selected entry remains visible: {plain}");

        let selected = line
            .spans
            .iter()
            .find(|span| span.content.contains("#2"))
            .expect("test: selected span");
        assert_eq!(selected.style.bg, Some(Color::Cyan));
        assert_eq!(selected.style.fg, Some(Color::Black));
        assert!(selected.style.add_modifier.contains(Modifier::BOLD));

        let focused = line
            .spans
            .iter()
            .find(|span| span.content.contains("#1"))
            .expect("test: focused span");
        assert_eq!(
            focused.style.bg, None,
            "input-routing focus uses the active marker, not selected highlight"
        );
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
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
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
    fn transcript_view_is_full_and_handles_empty_history() {
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
        assert_eq!(view.lines.len(), TRANSCRIPT_MAX_LINES + 25);
        assert!(!view.truncated, "transcript viewer retains full content");
        assert_eq!(
            view.scroll_offset,
            usize::MAX,
            "viewer no longer clamps away scroll range"
        );
        assert!(
            view.lines.first().is_some_and(|line| line.contains("line 0")),
            "oldest lines are retained: {:?}",
            view.lines.first()
        );
    }

    #[test]
    fn transcript_view_expands_folded_tool_and_reasoning_content() {
        let tool = ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: "cmd=short".to_string(),
            args_full: "{\"cmd\":\"long\"}".to_string(),
            result: Some("out-1\nout-2".to_string()),
            status: ToolStatus::Done,
            elapsed_ms: Some(1),
            folded: true,
        };
        let reasoning = ConversationLine::Reasoning {
            content: "hidden thought".to_string(),
            char_count: 12,
            folded: true,
        };

        let view = build_transcript_view("demo", &[tool, reasoning], 0);

        assert!(
            view.lines.iter().any(|line| line.contains("{\"cmd\":\"long\"}")),
            "verbose transcript should use full tool args: {:?}",
            view.lines
        );
        assert!(
            view.lines.iter().any(|line| line.contains("out-2")),
            "verbose transcript should include full tool result: {:?}",
            view.lines
        );
        assert!(
            view.lines.iter().any(|line| line.contains("hidden thought")),
            "verbose transcript should include folded reasoning: {:?}",
            view.lines
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

    fn provider_worker_status_fixture() -> ProviderWorkerStatus {
        ProviderWorkerStatus {
            running: 1,
            cancelling: 0,
            awaiting_commit: 0,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis().saturating_sub(2_000)),
            rows: vec![ProviderWorkerStatusRow {
                task_id: 42,
                sequence: 3,
                kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
                state: ProviderWorkerRowState::Running,
                started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(2_000),
                finalized_total_tokens: None,
                completion_ready: false,
            }],
        }
    }

    #[test]
    fn ctrl_g_includes_provider_worker_rows_and_enter_opens_worker_view() {
        let mut state = TuiState::new("p", "m");
        state.provider_worker_status = provider_worker_status_fixture();

        let out = dispatch_global_key(key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL), &mut state);
        match out {
            KeyDispatch::SwitcherOpened { entries } => {
                assert_eq!(entries.len(), 2, "transcript plus worker row: {entries:?}");
                let worker = entries.get(1).expect("worker row");
                assert_eq!(worker.kind, PROVIDER_WORKER_SWITCHER_KIND);
                assert_eq!(worker.origin, "provider");
                assert_eq!(worker.status, "running");
                assert!(
                    worker.title.contains("w#3 foreground_awaited task=42"),
                    "worker title: {}",
                    worker.title
                );
                let row = render_switcher_row(worker, "⏳", false, 120);
                assert!(row.contains("w#3 worker provider running"), "worker row: {row}");
                assert!(row.contains("foreground_awaited task=42"), "worker row: {row}");
                assert!(
                    !row.contains(&worker.seq.to_string()),
                    "synthetic seq must not leak into worker row: {row}"
                );
            }
            other => panic!("expected SwitcherOpened, got {other:?}"),
        }

        assert_eq!(
            dispatch_global_key(key(KeyCode::Down), &mut state),
            KeyDispatch::SwitcherMoved { selected: 1 }
        );
        let out = dispatch_global_key(key(KeyCode::Enter), &mut state);

        assert_eq!(out, KeyDispatch::OpenProviderWorkerView { sequence: 3 });
        assert!(state.switcher.is_none(), "switcher closed after worker detail route");
    }

    #[test]
    fn phase2_bottom_direction_selects_provider_worker_and_enter_opens_worker_view() {
        let mut state = TuiState::new("p", "m");
        state.provider_worker_status = provider_worker_status_fixture();
        state.visible_streaming_drafts = Arc::new(vec![crate::chat::state::VisibleStreamingDraftView {
            sequence: 3,
            draft: StreamingDraft {
                draft_id: "draft-worker-3".to_string(),
                accumulated: "worker 3 live".to_string(),
                version: 1,
            },
        }]);
        let out = dispatch_global_key(key(KeyCode::Down), &mut state);
        assert_eq!(
            out,
            KeyDispatch::StripSelectionChanged {
                selected: Some(PROVIDER_WORKER_SWITCHER_SEQ_BASE + 3)
            }
        );
        let out = dispatch_global_key(key(KeyCode::Enter), &mut state);
        assert_eq!(out, KeyDispatch::OpenProviderWorkerView { sequence: 3 });
        assert_eq!(
            state
                .streaming_draft_for_worker(3)
                .map(|draft| draft.accumulated.as_str()),
            Some("worker 3 live")
        );
        assert!(
            state
                .streaming_draft_for_worker(PROVIDER_WORKER_SWITCHER_SEQ_BASE + 3)
                .is_none(),
            "synthetic switcher seq must not be used for draft lookup"
        );
        assert_eq!(state.strip_selection, None);
    }

    #[test]
    fn phase2_provider_worker_io_none_is_empty_not_history_fallback() {
        let conversation = vec![
            ConversationLine::User {
                content: "run command".to_string(),
            },
            ConversationLine::Assistant {
                content: "history assistant must not appear".to_string(),
            },
        ];

        let lines = provider_worker_io_lines_for_streaming_draft(&conversation, None, 8);

        assert!(
            lines.is_empty(),
            "missing worker draft must not replay transcript: {lines:?}"
        );
    }

    #[test]
    fn provider_worker_focus_direction_and_esc_are_read_only_view_controls() {
        let mut state = TuiState::new("p", "m");
        state.provider_worker_status = provider_worker_status_fixture();
        state.sessions_cache = vec![entry(7)];
        state.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 3 };
        let out = dispatch_global_key(key(KeyCode::Up), &mut state);
        assert_eq!(out, KeyDispatch::CloseProviderWorkerView);
        let out = dispatch_global_key(key(KeyCode::Down), &mut state);
        assert_eq!(out, KeyDispatch::SwitchSession { seq: 7 });
        let out = dispatch_global_key(key(KeyCode::Esc), &mut state);
        assert_eq!(out, KeyDispatch::CloseProviderWorkerView);
    }

    #[test]
    fn provider_worker_active_view_uses_worker_kind_and_lines() {
        let status = provider_worker_status_fixture();
        let view = crate::chat::action::build_provider_worker_active_view(&status, 3, 0);
        assert_eq!(view.kind, crate::chat::sessions::model::ManagedKind::Worker.as_str());
        assert_eq!(view.seq, 3);
        assert!(view.lines.iter().any(|line| line == "worker: w#3"));
        assert!(view.lines.iter().any(|line| line == "task: 42"));
        assert!(view.lines.iter().any(|line| line == "kind: foreground_awaited"));
        assert!(view.lines.iter().any(|line| line == "state: running"));
        assert!(view.lines.iter().any(|line| line == "completion: pending"));
    }

    #[test]
    fn provider_worker_io_lines_include_tool_output_and_streaming_text() {
        let conversation = vec![
            ConversationLine::User {
                content: "run a shell command".to_string(),
            },
            ConversationLine::ToolResult {
                tool_name: "shell".to_string(),
                args_preview: "sleep 1 && echo done".to_string(),
                args_full: "{\"command\":\"sleep 1 && echo done\"}".to_string(),
                result: Some("done\nsecond line".to_string()),
                status: ToolStatus::Done,
                elapsed_ms: Some(1004),
                folded: true,
            },
            ConversationLine::Assistant {
                content: "tool finished".to_string(),
            },
        ];
        let streaming = StreamingDraft {
            draft_id: "d".to_string(),
            accumulated: "partial answer".to_string(),
            version: 1,
        };

        let lines = provider_worker_io_lines_from_conversation(&conversation, Some(&streaming), 8);

        assert!(lines.iter().any(|line| line.contains("run shell done: sleep 1")));
        assert!(lines.iter().any(|line| line == "output: done"));
        assert!(lines.iter().any(|line| line == "output: second line"));
        assert!(lines.iter().any(|line| line == "assistant: tool finished"));
        assert!(lines.iter().any(|line| line == "assistant streaming: partial answer"));

        let view = crate::chat::action::build_provider_worker_active_view_with_io(
            &provider_worker_status_fixture(),
            3,
            0,
            lines,
        );
        assert!(view.lines.iter().any(|line| line == "io: recent provider turn"));
        assert!(view.lines.iter().any(|line| line.starts_with("run shell done:")));
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
        let (main_span, main_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Main, false, None);
        assert_eq!(main_span.content.as_ref(), "> ");
        assert_eq!(main_w, 2);
        let (sess_span, sess_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Session { seq: 4 }, false, None);
        assert!(sess_span.content.contains("agent #4"), "carries the target as text");
        assert!(sess_span.content.contains('\u{25B8}'), "uses the ▸ glyph");
        assert_eq!(sess_w, UnicodeWidthStr::width(sess_span.content.as_ref()));
        // ASCII fallback drops the unicode glyph but keeps the text target.
        let (ascii_span, _) = prompt_indicator(crate::chat::sessions::FocusTarget::Session { seq: 4 }, true, None);
        assert!(ascii_span.content.contains("agent #4"));
        assert!(!ascii_span.content.contains('\u{25B8}'), "ascii fallback omits ▸");
        let (shell_span, _) = prompt_indicator(
            crate::chat::sessions::FocusTarget::Session { seq: 4 },
            false,
            Some("shell"),
        );
        assert!(shell_span.content.contains("shell #4"));
        assert!(!shell_span.content.contains("agent #4"));
        let (transcript_span, transcript_w) =
            prompt_indicator(crate::chat::sessions::FocusTarget::Transcript, false, None);
        assert!(transcript_span.content.contains("transcript"));
        assert!(transcript_span.content.contains('\u{25B8}'));
        assert_eq!(transcript_w, UnicodeWidthStr::width(transcript_span.content.as_ref()));
        let (diff_span, diff_w) = prompt_indicator(crate::chat::sessions::FocusTarget::Diff, false, None);
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
    fn large_paste_folds_to_chip_but_submits_original_text() {
        let mut input = TuiInput::new();
        let pasted = "one\ntwo\nthree\nfour\nfive\nsix";

        input.paste(pasted);

        assert_eq!(input.display_lines(), vec!["[Pasted text #1: 6 lines]".to_string()]);
        assert_eq!(input.text(), pasted);
        match input.handle_key(key(KeyCode::Enter)) {
            InputOutcome::Submitted(submitted) => assert_eq!(submitted, pasted),
            other => panic!("expected folded paste to submit, got {other:?}"),
        }
    }

    #[test]
    fn folded_paste_chips_increment_and_restore_through_history_navigation() {
        let mut input = TuiInput::new();
        input.record_history("older".to_string());
        let first = "a\nb\nc\nd\ne\nf";
        let second = "x".repeat(PASTE_FOLD_BYTE_THRESHOLD + 1);

        input.paste(first);
        input.paste(" ");
        input.paste(&second);

        assert_eq!(
            input.display_lines(),
            vec!["[Pasted text #1: 6 lines] [Pasted text #2: 1 lines]"]
        );
        assert_eq!(input.text(), format!("{first} {second}"));
        assert!(input.history_prev(), "moves to history");
        assert_eq!(input.text(), "older");
        assert!(input.history_next(), "restores draft");
        assert_eq!(
            input.display_lines(),
            vec!["[Pasted text #1: 6 lines] [Pasted text #2: 1 lines]"]
        );
        assert_eq!(input.text(), format!("{first} {second}"));
    }

    #[test]
    fn folded_paste_backspace_removes_whole_chip_without_placeholder_leak() {
        let mut input = TuiInput::new();
        let pasted = "one\ntwo\nthree\nfour\nfive\nsix";
        input.paste(pasted);

        input.handle_key(key(KeyCode::Backspace));

        assert_eq!(input.text(), "");
        assert_eq!(input.display_lines(), vec![String::new()]);
        assert!(
            !input.text().contains("[Pasted text"),
            "damaged placeholder must never leak into submitted payload"
        );
    }

    #[test]
    fn folded_paste_cursor_inside_chip_cannot_insert_into_chip() {
        let mut input = TuiInput::new();
        let pasted = "one\ntwo\nthree\nfour\nfive\nsix";
        input.paste(pasted);
        input.cursor = (0, 1);

        assert!(input.insert_char('X'));

        assert_eq!(input.text(), format!("{pasted}X"));
        assert_eq!(input.display_lines(), vec!["[Pasted text #1: 6 lines]X".to_string()]);
    }

    #[test]
    fn small_paste_matching_chip_placeholder_is_not_expanded() {
        let mut input = TuiInput::new();
        let literal = "[Pasted text #1: 6 lines]";

        input.paste(literal);

        assert_eq!(input.lines, vec![literal.to_string()]);
        assert_eq!(input.text(), literal);
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
    fn render_input_scrolls_visible_window_to_cursor() {
        let mut state = TuiState::new("p", "m");
        state.input.lines = (0..15).map(|idx| format!("scroll-line-{idx:02}")).collect();
        state.input.cursor = (14, "scroll-line-14".len());
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 90, 24, &mut scroll);
        let joined = rows.join("\n");

        assert!(
            joined.contains("scroll-line-14"),
            "cursor row should be visible: {joined}"
        );
        assert!(
            !joined.contains("scroll-line-00"),
            "top rows should scroll out once cursor is below visible input window: {joined}"
        );
    }

    #[test]
    fn input_wrap_ranges_respect_unicode_display_width() {
        let line = "ab你好c";
        let ranges = wrap_line_ranges(line, 4);
        let wrapped = ranges
            .iter()
            .map(|(start, end)| line.get(*start..*end).unwrap_or(""))
            .collect::<Vec<_>>();

        assert_eq!(wrapped, vec!["ab你", "好c"]);
        assert!(
            wrapped.iter().all(|row| UnicodeWidthStr::width(*row) <= 4),
            "wrapped rows fit display width: {wrapped:?}"
        );
    }

    #[test]
    fn long_single_input_line_uses_wrapped_chrome_height() {
        let mut state = TuiState::new("p", "m");
        state.input.set_text(&"abcdefghij".repeat(8));

        let narrow = fullscreen_bottom_chrome_height_for_width(&state, 16);
        let wide = fullscreen_bottom_chrome_height_for_width(&state, 120);

        assert!(narrow > wide, "narrow input should reserve wrapped rows");
        assert!(narrow <= BOTTOM_CHROME_MAX_HEIGHT);
    }

    #[test]
    fn render_input_soft_wrap_scrolls_to_cursor_tail() {
        let mut state = TuiState::new("p", "m");
        let text = format!("HEAD-{}-TAIL", "abcdefghij".repeat(20));
        state.input.set_text(&text);
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 14, 24, &mut scroll);
        let joined = rows.join("\n");

        assert!(
            joined.contains("TAIL"),
            "wrapped cursor tail should be visible: {joined}"
        );
        assert!(
            !joined.contains("HEAD"),
            "wrapped input should scroll old visual rows out when cursor is at tail: {joined}"
        );
    }

    #[test]
    fn fullscreen_footer_advertises_copy_paths() {
        let state = TuiState::new("p", "m");
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 180, 24, &mut scroll);
        let joined = rows.join("\n");

        assert!(
            joined.contains("/copy latest"),
            "footer should advertise /copy: {joined}"
        );
        assert!(
            joined.contains("drag select/copy"),
            "footer should advertise native selection: {joined}"
        );
    }

    #[test]
    fn fullscreen_footer_becomes_session_list_when_sessions_exist() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = vec![entry(1)];
        state.sessions_status = "sessions: 1 running".to_string();
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 100, 24, &mut scroll);
        let joined = rows.join("\n");
        let bottom = rows.last().cloned().unwrap_or_default();

        assert!(
            joined.contains("main chat"),
            "bottom list should include the main session: {rows:?}"
        );
        assert!(bottom.contains("#1"), "bottom list should show session seq: {rows:?}");
        assert!(
            bottom.contains("agent"),
            "bottom list should show session kind: {rows:?}"
        );
        assert!(
            bottom.contains("0 tok"),
            "bottom list should show cumulative token usage: {rows:?}"
        );
        assert!(
            bottom.contains("task 1"),
            "bottom list should show the running task title, not shortcut-only chrome: {rows:?}"
        );
        assert!(
            !bottom.contains("Ctrl+G sessions"),
            "session list should replace shortcut footer while sessions exist: {rows:?}"
        );
        assert_eq!(
            joined.matches("#1").count(),
            1,
            "session should render once below input, never duplicated above it: {rows:?}"
        );
        assert!(
            !joined.contains('⏳') && !joined.contains('✓'),
            "session list should use text status rather than status icons: {rows:?}"
        );
    }

    #[test]
    fn fullscreen_footer_hides_completed_sessions_from_active_bottom_list() {
        let mut state = TuiState::new("p", "m");
        let mut done = entry(1);
        done.status = "completed";
        state.sessions_cache = vec![done];
        state.sessions_status = "sessions: 1 completed".to_string();
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 100, 24, &mut scroll);
        let joined = rows.join("\n");

        assert!(
            !joined.contains("#1"),
            "completed child sessions should leave the active bottom list: {rows:?}"
        );
        assert!(
            joined.contains("Ctrl+G sessions"),
            "history-only sessions should restore the normal footer: {rows:?}"
        );
    }

    #[test]
    fn fullscreen_footer_keeps_focused_completed_session_until_detach() {
        let mut state = TuiState::new("p", "m");
        let mut done = entry(1);
        done.status = "completed";
        state.sessions_cache = vec![done];
        state.sessions_status = "sessions: 1 completed".to_string();
        state.focus = crate::chat::sessions::FocusTarget::Session { seq: 1 };
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 100, 24, &mut scroll);
        let joined = rows.join("\n");

        assert!(
            joined.contains("#1") && joined.contains("completed"),
            "the focused terminal child remains visible until the user detaches: {rows:?}"
        );
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
    fn execution_activity_active_tracks_streaming_and_running_tools() {
        let mut state = TuiState::new("p", "m");
        assert!(!state.execution_activity_active());

        state.start_stream("d1");
        assert!(state.execution_activity_active());
        state.cancel_stream("d1");
        assert!(!state.execution_activity_active());

        state.conversation_lines.push(ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: String::new(),
            args_full: String::new(),
            result: None,
            status: ToolStatus::Running,
            elapsed_ms: None,
            folded: false,
        });
        assert!(state.execution_activity_active());
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
            rows.iter().any(|row| row.contains("PRX") && row.contains("mode:")),
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
    fn fullscreen_status_bar_draws_metered_tokens_and_cost() {
        let mut state = TuiState::new("provider", "model");
        state.token_usage_summary = MainSessionTokenUsageSummary {
            prompt_tokens: 1_000,
            completion_tokens: 500,
            total_tokens: 1_500,
            reported_tokens: 1_500,
            request_count: 1,
            known_cost_usd: 0.0105,
            ..MainSessionTokenUsageSummary::default()
        };
        let mut scroll = FullscreenTranscriptScroll::default();

        let rows = fullscreen_rows(&state, 96, 18, &mut scroll);

        assert!(
            rows.iter().any(|row| row.contains("1.5k tok | $0.0105")),
            "status bar should render metered token/cost summary: {rows:?}"
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("/ 128k") || row.contains("Total chars")),
            "status bar must not render the old chars/window gauge: {rows:?}"
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
    fn fullscreen_scrolled_transcript_keeps_content_anchor_when_output_arrives() {
        let mut state = TuiState::new("provider", "model");
        state.conversation_lines.push(ConversationLine::Assistant {
            content: (0..60)
                .map(|idx| format!("anchor line {idx:03}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        let mut scroll = FullscreenTranscriptScroll::default();
        scroll.page_up(8);

        let before_rows = fullscreen_rows(&state, 72, 14, &mut scroll);
        let before_first = before_rows
            .iter()
            .find(|row| row.contains("anchor line"))
            .map(|row| row.trim().to_string())
            .expect("scrolled transcript should expose an anchor line");

        state.conversation_lines.push(ConversationLine::Assistant {
            content: (60..66)
                .map(|idx| format!("anchor line {idx:03}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        let after_rows = fullscreen_rows(&state, 72, 14, &mut scroll);
        let after_first = after_rows
            .iter()
            .find(|row| row.contains("anchor line"))
            .map(|row| row.trim().to_string())
            .expect("scrolled transcript should keep the anchor line visible");

        assert_eq!(
            after_first, before_first,
            "new output should not push the reading position: before={before_rows:?}, after={after_rows:?}"
        );
        assert!(
            !after_rows.iter().any(|row| row.contains("anchor line 065")),
            "scrolled transcript should not jump to the new tail: {after_rows:?}"
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
            "shortcut footer remains pinned after multiline input without child sessions: {rows:?}"
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
    fn session_list_footer_adds_one_row_per_extra_session() {
        let mut state = TuiState::new("p", "m");
        let idle = fullscreen_bottom_chrome_height(&state);
        assert!(!session_footer_has_sessions(&state), "empty session footer hidden");

        state.sessions_cache = vec![entry(1)];
        assert!(session_footer_has_sessions(&state), "non-empty session footer shown");
        let with_one = fullscreen_bottom_chrome_height(&state);
        assert_eq!(
            with_one,
            idle.saturating_add(1),
            "one child session adds one row because the list also includes main"
        );

        state.sessions_cache = vec![entry(1), entry(2), entry(3)];
        let with_three = fullscreen_bottom_chrome_height(&state);
        assert_eq!(
            with_three,
            idle.saturating_add(3),
            "session list adds one row per child while retaining main"
        );

        state.sessions_cache.clear();
        assert!(!session_footer_has_sessions(&state));
        assert_eq!(fullscreen_bottom_chrome_height(&state), idle);
    }

    #[test]
    fn session_list_footer_stays_within_height_budget() {
        let mut state = TuiState::new("p", "m");
        state.sessions_cache = long_strip_entries(30);
        state.start_stream("d");
        for _ in 0..(INPUT_MAX_VISIBLE_ROWS + 4) {
            state.input.lines.push(String::new());
        }
        assert!(
            session_footer_has_sessions(&state),
            "the session list remains available under real inputs"
        );
        assert!(fullscreen_bottom_chrome_height(&state) <= BOTTOM_CHROME_MAX_HEIGHT);
    }

    #[test]
    fn session_list_footer_degrades_within_budget() {
        // Forward-compat guard: even an oversized session list cannot grow the
        // pinned chrome beyond the hard maximum.
        let without_sessions = 1u16 // status
            + u16::try_from(INPUT_MAX_VISIBLE_ROWS + 1).unwrap_or(11)
            + 1; // footer
        assert!(
            without_sessions < BOTTOM_CHROME_MAX_HEIGHT,
            "guard threshold: row drops once the rest reaches BOTTOM_CHROME_MAX_HEIGHT"
        );
    }

    #[test]
    fn status_bar_renders_reported_tokens_and_cost() {
        let mut state = TuiState::new("provider", "model");
        state.session_title = "tokens".to_string();
        state.token_usage_summary = MainSessionTokenUsageSummary {
            prompt_tokens: 1_000,
            completion_tokens: 500,
            total_tokens: 1_500,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            reported_tokens: 1_500,
            estimated_tokens: 0,
            request_count: 1,
            known_cost_usd: 0.0105,
            unknown_cost_requests: 0,
        };

        let line = render_status_bar_text(&state, 120);
        assert!(
            line.contains("1.5k tok | $0.0105"),
            "status should include cumulative reported tokens and cost: {line}"
        );
        assert!(
            !line.contains("~1.5k"),
            "reported-only usage must not be marked estimated: {line}"
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
    fn status_bar_renders_main_queue_status_when_present() {
        let mut state = TuiState::new("provider", "model");
        state.main_queue_status = MainQueueStatus { queued: 4, priority: 1 };

        let line = render_status_bar_text(&state, 140);

        assert!(
            line.contains("queue:4 priority:1"),
            "status should expose main input backlog: {line}"
        );
    }

    #[test]
    fn status_bar_renders_provider_worker_status_when_present() {
        let mut state = TuiState::new("provider", "model");
        state.main_queue_status = MainQueueStatus { queued: 2, priority: 0 };
        state.provider_worker_status = ProviderWorkerStatus {
            running: 1,
            cancelling: 1,
            awaiting_commit: 1,
            finalized_payloads: 1,
            finalized_total_tokens: 1_250,
            oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis().saturating_sub(3_000)),
            rows: vec![
                ProviderWorkerStatusRow {
                    task_id: 77,
                    sequence: 7,
                    kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
                    state: ProviderWorkerRowState::Running,
                    started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(3_000),
                    finalized_total_tokens: None,
                    completion_ready: false,
                },
                ProviderWorkerStatusRow {
                    task_id: 88,
                    sequence: 8,
                    kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                    state: ProviderWorkerRowState::Committed,
                    started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(9_000),
                    finalized_total_tokens: Some(1_250),
                    completion_ready: true,
                },
            ],
        };

        let line = render_status_bar_text(&state, 220);

        assert!(line.contains("queue:2"), "status should retain queue status: {line}");
        assert!(
            line.contains("workers:1"),
            "status should expose running provider workers: {line}"
        );
        assert!(
            line.contains("cancelling:1"),
            "status should expose cancelling provider workers: {line}"
        );
        assert!(
            line.contains("commit:1"),
            "status should expose commit-pending provider workers: {line}"
        );
        assert!(
            line.contains("welapsed:"),
            "status should expose provider worker elapsed time: {line}"
        );
        assert!(
            line.contains("wtok:1.2k"),
            "status should expose finalized provider worker tokens: {line}"
        );
        assert!(
            line.contains("w#7:fg:run:"),
            "status should expose per-worker running detail: {line}"
        );
        assert!(
            !line.contains("w#8:detached:done:1.2k"),
            "status should not keep completed workers in the switchable row list: {line}"
        );
    }

    #[test]
    fn status_bar_renders_completion_ready_worker_row() {
        let mut state = TuiState::new("provider", "model");
        state.provider_worker_status = ProviderWorkerStatus {
            running: 1,
            cancelling: 0,
            awaiting_commit: 0,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis().saturating_sub(3_000)),
            rows: vec![ProviderWorkerStatusRow {
                task_id: 77,
                sequence: 7,
                kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                state: ProviderWorkerRowState::Running,
                started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(3_000),
                finalized_total_tokens: None,
                completion_ready: true,
            }],
        };

        let line = render_status_bar_text(&state, 180);

        assert!(
            line.contains("w#7:detached:ready:"),
            "completion-ready workers should not look like still-running provider execution: {line}"
        );
    }

    #[test]
    fn status_bar_keeps_queue_status_when_generating_and_compact() {
        let mut state = TuiState::new("provider-with-long-name", "model-with-long-name");
        state.session_title = "long running orchestration title".to_string();
        state.main_queue_status = MainQueueStatus { queued: 5, priority: 0 };
        state.start_stream("draft-queue");

        let line = render_status_bar_text(&state, 72);

        assert!(
            line.contains("queue:5"),
            "compact active status should retain queue: {line}"
        );
        assert!(
            line.contains("generating"),
            "compact active status should retain activity: {line}"
        );
    }

    #[test]
    fn status_bar_keeps_provider_worker_status_when_generating_and_compact() {
        let mut state = TuiState::new("provider-with-long-name", "model-with-long-name");
        state.session_title = "long running orchestration title".to_string();
        state.provider_worker_status = ProviderWorkerStatus {
            running: 1,
            cancelling: 0,
            awaiting_commit: 0,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis().saturating_sub(2_000)),
            rows: vec![ProviderWorkerStatusRow {
                task_id: 1,
                sequence: 1,
                kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
                state: ProviderWorkerRowState::Running,
                started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(2_000),
                finalized_total_tokens: None,
                completion_ready: false,
            }],
        };
        state.start_stream("draft-worker");

        let line = render_status_bar_text(&state, 72);

        assert!(
            line.contains("workers:1"),
            "compact active status should retain worker status: {line}"
        );
        assert!(
            line.contains("generating"),
            "compact active status should retain activity: {line}"
        );
    }

    #[test]
    fn status_bar_permission_status_degrades_at_narrow_width() {
        let mut state = TuiState::new("provider", "model");
        state.chat_mode = ChatMode::Plan;
        state.autonomy_level = AutonomyLevel::Full;
        state.token_usage_summary = MainSessionTokenUsageSummary {
            total_tokens: 1_500,
            estimated_tokens: 1_500,
            known_cost_usd: 0.0006,
            request_count: 1,
            ..MainSessionTokenUsageSummary::default()
        };

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
    fn status_bar_marks_estimated_usage_with_tilde() {
        let mut state = TuiState::new("provider", "model");
        state.token_usage_summary = MainSessionTokenUsageSummary {
            prompt_tokens: 1_000,
            completion_tokens: 1_000,
            total_tokens: 2_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            reported_tokens: 0,
            estimated_tokens: 2_000,
            request_count: 1,
            known_cost_usd: 0.002,
            unknown_cost_requests: 0,
        };

        let line = render_status_bar_text(&state, 120);
        assert!(
            line.contains("~2.0k tok | $0.0020"),
            "estimated usage should be marked: {line}"
        );
    }

    #[test]
    fn status_bar_renders_context_budget_percent_from_current_context_source() {
        let mut state = TuiState::new("provider", "model");
        state.context_used_tokens = Some(8_500);
        state.context_window_tokens = Some(10_000);
        state.token_usage_summary = MainSessionTokenUsageSummary {
            total_tokens: 50_000,
            estimated_tokens: 50_000,
            request_count: 1,
            known_cost_usd: 0.002,
            ..MainSessionTokenUsageSummary::default()
        };

        let line = render_status_bar_text(&state, 140);

        assert!(
            line.contains("~50.0k tok"),
            "estimated token count remains marked: {line}"
        );
        assert!(
            line.contains("ctx:85% used!"),
            "context budget uses current context numerator, not cumulative tokens: {line}"
        );
        assert!(
            !line.contains("~ctx") && !line.contains("ctx:100"),
            "context budget should not inherit token estimate marker or cumulative total: {line}"
        );
    }

    #[test]
    fn status_bar_caps_context_budget_percent_at_100() {
        let mut state = TuiState::new("provider", "model");
        state.context_used_tokens = Some(15_000);
        state.context_window_tokens = Some(10_000);

        let line = render_status_bar_text(&state, 140);

        assert!(line.contains("ctx:100% used!"), "context budget caps at 100%: {line}");
        assert!(!line.contains("ctx:150"), "context budget must not exceed 100%: {line}");
    }

    #[test]
    fn status_bar_shows_generation_interrupt_hint() {
        let mut state = TuiState::new("provider", "model");
        state.start_stream("draft-1");

        let line = render_status_bar_text(&state, 120);

        assert!(line.contains("generating"), "status shows generation activity: {line}");
        assert!(
            !line.contains("generating 0s"),
            "status must not fake elapsed time: {line}"
        );
        assert!(
            line.contains("(esc to interrupt)"),
            "status exposes esc interrupt affordance: {line}"
        );
    }

    #[test]
    fn status_bar_shows_generation_activity_for_running_tool_without_streaming() {
        let mut state = TuiState::new("provider", "model");
        state.push_tool_result_started("shell", "{}");

        let line = render_status_bar_text(&state, 120);

        assert!(
            line.contains("generating") && line.contains("(esc to interrupt)"),
            "running tool keeps generation activity visible: {line}"
        );
    }

    #[test]
    fn status_bar_renders_unknown_cost_for_unpriced_usage() {
        let mut state = TuiState::new("provider", "model");
        state.token_usage_summary = MainSessionTokenUsageSummary {
            total_tokens: 42,
            reported_tokens: 42,
            request_count: 1,
            unknown_cost_requests: 1,
            ..MainSessionTokenUsageSummary::default()
        };

        let line = render_status_bar_text(&state, 120);
        assert!(
            line.contains("42 tok | cost unknown"),
            "unknown cost should be explicit: {line}"
        );
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
        // Assistant text now has an actor marker prefix; the CJK payload before
        // the cursor must still be contiguous in the diff output.
        assert!(
            trimmed.contains("你好世界"),
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
        use crate::chat::action::Action;
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
            tui.streaming = state.stream.primary_streaming_draft().cloned();
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
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d-1".to_string(),
                delta: "streaming…".to_string(),
                version: 3,
            });
            let snap = state.build_ui_snapshot(2);

            let mut tui = TuiState::new(&state.session.provider, &state.session.model);
            tui.session_title = state.session.title.clone();
            tui.turn_count = state.ui.turn_count;
            tui.ascii_fallback = state.ui.ascii_fallback;
            tui.conversation_lines = state.ui.conversation_lines.clone();
            tui.streaming = state.stream.primary_streaming_draft().cloned();
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
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            };
            state.ui.sessions_entries = vec![session_entry.clone()];
            state.ui.strip_selection = Some(7);
            state.ui.context_used_tokens = Some(2_500);
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
            tui.streaming = state.stream.primary_streaming_draft().cloned();
            tui.input = state.ui.input.clone();
            tui.sessions_cache = vec![session_entry];
            tui.strip_selection = Some(7);
            tui.chat_mode = ChatMode::Auto;
            tui.autonomy_level = AutonomyLevel::ReadOnly;
            tui.focus = crate::chat::sessions::FocusTarget::Diff;
            tui.active_session_view = Some(active_view);
            tui.context_used_tokens = Some(2_500);
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
                BottomChromeView::strip_selection(&tui),
                BottomChromeView::strip_selection(&snap)
            );
            assert_eq!(
                BottomChromeView::pending_tool_approval(&tui),
                BottomChromeView::pending_tool_approval(&snap)
            );
            assert_eq!(
                BottomChromeView::context_used_tokens(&tui),
                BottomChromeView::context_used_tokens(&snap)
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
        fn s4_a_2_snapshot_prompt_uses_shell_kind_for_attached_session() {
            let mut state = make_state_with_lines();
            state.ui.focus = crate::chat::sessions::FocusTarget::Session { seq: 1 };
            state.ui.sessions_entries = vec![crate::chat::sessions::SwitcherEntry {
                seq: 1,
                kind: crate::chat::sessions::model::ManagedKind::Shell.as_str(),
                origin: crate::chat::sessions::model::SessionOrigin::User.as_str(),
                status: crate::chat::sessions::model::ManagedStatus::Running.as_str(),
                title: "echo ok".to_string(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            }];
            let snap = state.build_ui_snapshot(12);

            let (span, width) = prompt_indicator(
                snap.focus(),
                snap.ascii_fallback(),
                snap.focus()
                    .session_seq()
                    .and_then(|seq| focused_session_kind(&snap, seq)),
            );

            assert!(
                span.content.contains("shell #1"),
                "attached shell prompt must identify the ManagedKind: {}",
                span.content
            );
            assert!(
                !span.content.contains("agent #1"),
                "shell attach prompt must not fall back to agent: {}",
                span.content
            );
            assert_eq!(width, UnicodeWidthStr::width(span.content.as_ref()));
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
            tui.streaming = state.stream.primary_streaming_draft().cloned();
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
