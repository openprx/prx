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

mod draft_buffer;
mod input;
mod keys;
mod layout;
mod render_chrome;
mod render_conversation;
mod render_input;
mod sinks;
mod slash_menu;
mod tool_card;
mod views;

use crate::agent::turn_contracts::ChatMode;
use crate::chat::action::{MainQueueStatus, ProviderWorkerStatus};
#[cfg(test)]
use crate::chat::action::{ProviderWorkerRowState, ProviderWorkerStatusRow};
use crate::chat::session::MainSessionTokenUsageSummary;
#[cfg(test)]
use crate::chat::terminal_proto::{InlineDraftProtocol, LineProtocolError};
use crate::security::AutonomyLevel;
use crossterm::event::KeyEvent;
#[cfg(test)]
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
pub use draft_buffer::DraftLineBuffer;
pub use input::{ReverseSearchState, TuiInput};
pub use keys::{InputOutcome, KeyDispatch, dispatch_global_key};
#[cfg(test)]
use layout::session_footer_has_sessions;
pub use layout::{
    ACTIVE_SESSION_VIEW_DESIRED_ROWS, BOTTOM_CHROME_MAX_HEIGHT, BOTTOM_CHROME_MIN_HEIGHT, BottomChromeView,
    FullscreenTranscriptScroll, TranscriptSelection, fullscreen_bottom_chrome_height, fullscreen_transcript_page_rows,
    fullscreen_transcript_scroll_available, toggle_reasoning_at_fullscreen_point,
};
#[cfg(test)]
use parking_lot::Mutex;
#[cfg(test)]
use ratatui::buffer::Buffer;
#[cfg(test)]
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::style::{Color, Modifier, Style};
#[cfg(test)]
use ratatui::text::{Line, Text};
#[cfg(test)]
use ratatui::widgets::{Paragraph, Widget, Wrap};
pub(crate) use render_chrome::turn_elapsed_ms;
#[cfg(test)]
use render_chrome::{
    ASCII_SPINNER_FRAMES, UNICODE_SPINNER_FRAMES, active_session_visible_lines, render_saved_session_picker_row,
    render_sessions_list_lines, render_status_bar_text, render_switcher_row, session_status_glyph_for_tick,
    slash_menu_overlay_rect, truncate_chars_with_ellipsis,
};
pub use render_chrome::{
    bottom_running_entry_active_for_view, execution_activity_active_for_view, periodic_redraw_active_for_view,
};
#[cfg(test)]
use render_conversation::{
    ASSISTANT_MARKDOWN_CACHE, THINKING_PREVIEW_MAX_CHARS, ansi_sgr_to_lines, cached_finalized_assistant_markdown_lines,
    fullscreen_bottom_chrome_height_for_width, measure_wrapped_rows, render_conversation_line,
    render_streaming_assistant_markdown_lines,
};
pub use render_conversation::{render_fullscreen_chat, render_fullscreen_chat_with_selection};
pub(crate) use render_conversation::{transcript_plain_rows, transcript_render_row_at_point};
pub use render_input::render_approval;
#[cfg(test)]
use render_input::{focused_session_kind, footer_text, prompt_indicator, wrap_line_ranges};
pub use sinks::{SnapshotDispatcherSink, TuiStateMirrorSink};
pub use slash_menu::{SlashMenuEntry, SlashMenuState, slash_provider_model_catalog_from_config};
pub(crate) use slash_menu::{
    SlashMenuSources, dispatch_slash_menu_key_with_sources, fuzzy_path_match, sync_slash_menu_for_sources,
};
use std::sync::Arc;
#[cfg(test)]
use std::sync::LazyLock;
pub(crate) use tool_card::build_tool_args_preview;
#[cfg(test)]
use tool_card::{
    build_args_preview, estimate_reasoning_tokens, one_line_summary, reasoning_card_glyphs, render_tool_result,
    tool_card_glyphs, tool_status_marker,
};
#[cfg(test)]
use unicode_width::UnicodeWidthStr;
#[cfg(test)]
use views::provider_worker_switcher_entry;
pub use views::{
    FoldableKind, build_diff_view, build_transcript_view, latest_provider_worker_tool_summary_from_conversation,
    provider_worker_io_lines_for_streaming_draft, provider_worker_io_lines_from_conversation,
    transcript_switcher_entry,
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
    /// Number of reasoning ("thinking") characters streamed so far for this
    /// draft.
    ///
    /// The full reasoning body is deliberately NOT stored here: the streaming
    /// driver keeps the single authoritative copy and replays it in
    /// `Action::StreamCompleted`, which is what becomes the final reasoning
    /// card. The draft only carries derived progress so the transient activity
    /// line can advance while the model is still thinking.
    pub reasoning_chars: usize,
    /// Bounded tail of the reasoning stream (at most
    /// [`REASONING_TAIL_MAX_CHARS`] characters) used for the one-line live
    /// preview under the thinking indicator.
    pub reasoning_tail: String,
}

/// Maximum number of characters retained from the reasoning stream for the
/// live preview. Bounded so a 40s thinking burst cannot grow the draft.
pub const REASONING_TAIL_MAX_CHARS: usize = 240;

impl StreamingDraft {
    /// Fresh draft with no accumulated text and no thinking progress.
    #[must_use]
    pub fn new(draft_id: impl Into<String>) -> Self {
        Self {
            draft_id: draft_id.into(),
            accumulated: String::new(),
            version: 0,
            reasoning_chars: 0,
            reasoning_tail: String::new(),
        }
    }

    /// One-line preview of the most recent reasoning text, whitespace
    /// collapsed. `None` when nothing has streamed yet.
    #[must_use]
    pub fn reasoning_preview(&self) -> Option<String> {
        let collapsed = self.reasoning_tail.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() { None } else { Some(collapsed) }
    }
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
    /// Wall-clock start of the active main turn, used only for live activity
    /// feedback. Cleared when the draft reaches a terminal state.
    pub stream_started_at_ms: Option<i64>,
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

pub use crate::chat::slash_types::{AtPathCandidate, SlashModelCandidate, SlashProviderModelCatalog};

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
            stream_started_at_ms: None,
            visible_streaming_drafts: Arc::new(Vec::new()),
            sessions_status: String::new(),
            focus: crate::chat::sessions::FocusTarget::Main,
            switcher: None,
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
        self.stream_started_at_ms = Some(chrono::Utc::now().timestamp_millis());
        self.streaming = Some(StreamingDraft::new(draft_id));
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
        self.stream_started_at_ms = None;
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
            self.stream_started_at_ms = None;
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

    #[must_use]
    pub fn periodic_redraw_active(&self) -> bool {
        periodic_redraw_active_for_view(self)
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
            folded: !tool_result_defaults_expanded(tool_name),
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

    /// Toggle the folded flag of the most recent visible `ToolResult` card.
    ///
    /// Reasoning records stay in `conversation_lines` for the verbose
    /// transcript, but the primary TUI does not render or interact with them.
    pub fn toggle_last_foldable_card(&mut self) -> Option<(FoldableKind, bool)> {
        for line in self.conversation_lines.iter_mut().rev() {
            if let ConversationLine::ToolResult { folded, .. } = line {
                *folded = !*folded;
                return Some((FoldableKind::ToolResult, *folded));
            }
        }
        None
    }
}

pub(crate) const fn tool_result_defaults_expanded(tool_name: &str) -> bool {
    let _ = tool_name;
    false
}

#[cfg(test)]
mod tests;
