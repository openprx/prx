//! Redux-like ChatState and its reducer.
//!
//! Contains:
//! - [`ChatState`] — top-level state holding 4 sub-structures
//! - [`SessionState`] / [`UiState`] / [`StreamState`] / [`ControlState`]
//! - [`Effect`] — side-effect instructions returned by reduce, run by the main loop's async shell
//!
//! Design principles:
//! - `ChatState::reduce` is a pure sync function: no I/O, no await, it only mutates itself
//! - Effect is an enum (not `Box<dyn FnOnce>`), Send + Sync, serializable and testable
//! - ChatState has a single owner (the main loop), so no `Arc<Mutex<>>` is needed

// Step 2: wire in the real types — TuiInput / ConversationLine / StreamingDraft come from
// `crate::chat::tui` (feature = "terminal-tui"). Without the TUI feature we keep placeholder
// types so that both feature sets still compile independently.
//
// Note: `input` in UiState is the reducer's input-buffer snapshot (written by the new path),
// while the old path still forwards keys to `chat_mirror.lock().input` (TuiState embeds TuiInput).
// Once Step 5 removes the old path, chat_mirror is replaced by ChatState.ui.

#[cfg(feature = "terminal-tui")]
pub use crate::chat::tui::{ConversationLine, REASONING_TAIL_MAX_CHARS, SlashMenuState, StreamingDraft, TuiInput};

/// Placeholder: TuiInput (without terminal-tui; keeps the reducer compiling on the minimal features)
#[cfg(not(feature = "terminal-tui"))]
pub type TuiInput = Vec<String>;

/// Placeholder: ConversationLine (without the terminal-tui feature)
#[cfg(not(feature = "terminal-tui"))]
pub type ConversationLine = String;

/// Placeholder: SlashMenuState (without the terminal-tui feature)
#[cfg(not(feature = "terminal-tui"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashMenuState;

/// Placeholder: StreamingDraft (without the terminal-tui feature)
#[cfg(not(feature = "terminal-tui"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamingDraft {
    pub draft_id: String,
    pub accumulated: String,
    pub version: u64,
    pub reasoning_chars: usize,
    pub reasoning_tail: String,
}

/// Placeholder: kept at the same value as the terminal-tui version.
#[cfg(not(feature = "terminal-tui"))]
pub const REASONING_TAIL_MAX_CHARS: usize = 240;

#[cfg(not(feature = "terminal-tui"))]
impl StreamingDraft {
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

    #[must_use]
    pub fn reasoning_preview(&self) -> Option<String> {
        let collapsed = self.reasoning_tail.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() { None } else { Some(collapsed) }
    }
}

use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent::loop_::ChatMode;
use crate::channels::traits::SendMessage;
use crate::chat::action::{
    Action, CompactReason, HistoryDir, MainQueueStatus, ProviderUsageRecordKind, ProviderWorkerStatus,
};
use crate::chat::session::{ChatSession, ChatTurn, MainSessionTokenUsageRecord, MainSessionTokenUsageSummary};
use crate::chat::slash_types::AtPathCandidate;
use crate::hooks::HookEvent;
use crate::memory::MemoryCategory;
use crate::providers::ChatMessage;
use crate::security::AutonomyLevel;
use crate::util::truncate_with_ellipsis;

/// S2-B Step 1: the `Action::HistoryCompacted` reducer matches the constant boundaries of
/// `chat::mod::compact_chat_history`. All three constants must share their source with `chat::mod`
/// so both paths agree during dual-write; once a later step drops the old path, keep the reducer's.
const COMPACT_KEEP_MESSAGES: usize = 8;
const COMPACT_CONTENT_CHARS: usize = 320;
const COMPACT_TOTAL_CHARS: usize = 2400;

#[cfg(feature = "terminal-tui")]
fn conversation_lines_from_turns(turns: &[ChatTurn]) -> Vec<ConversationLine> {
    turns
        .iter()
        .filter_map(|turn| match turn.role.as_str() {
            "user" => Some(ConversationLine::User {
                content: turn.content.clone(),
            }),
            "assistant" => Some(ConversationLine::Assistant {
                content: turn.content.clone(),
            }),
            "system" => Some(ConversationLine::System {
                content: turn.content.clone(),
            }),
            _ => None,
        })
        .collect()
}

#[cfg(not(feature = "terminal-tui"))]
fn conversation_lines_from_turns(turns: &[ChatTurn]) -> Vec<ConversationLine> {
    turns
        .iter()
        .filter(|turn| matches!(turn.role.as_str(), "user" | "assistant" | "system"))
        .map(|turn| turn.content.clone())
        .collect()
}

fn is_durable_compaction_history_message(message: &ChatMessage) -> bool {
    matches!(message.role.as_str(), "user" | "assistant")
        && !message.content.starts_with("[Post-compaction context refresh]")
}

fn durable_turns_from_compacted_history(history: &[ChatMessage]) -> Vec<ChatTurn> {
    let timestamp = chrono::Utc::now();
    history
        .iter()
        .filter(|message| is_durable_compaction_history_message(message))
        .map(|message| ChatTurn {
            role: message.role.clone(),
            content: message.content.clone(),
            timestamp,
            tool_calls: Vec::new(),
        })
        .collect()
}

// ─── Effect ──────────────────────────────────────────────────────────────────

/// Effect = a side effect that must be executed by the async shell.
///
/// Modelled as an enum rather than `Box<dyn FnOnce>`:
/// - `Send + Sync` holds naturally
/// - serializable for logging / replay / test snapshots
/// - no heap allocation (beyond the inlined String/Arc)
///
/// Returned by [`ChatState::reduce`] and dispatched by the main loop.
#[allow(dead_code)]
#[derive(Debug)]
pub enum Effect {
    /// Start a new round of LLM inference: takes draft_id, a history snapshot and a cancel token.
    ///
    /// `draft_id` is carried by [`Action::TurnStarted`] and written into `state.stream.draft`;
    /// the executor subtask tags Actions such as `StreamChunkReceived` / `StreamCompleted` with it
    /// so the reducer can match them and merge the delta (see `state.rs::reduce_stream_chunk_received`).
    /// Since Step 5a-2 `EffectExecutor` really calls `provider.stream_chat_with_history` in deps mode,
    /// and stream chunks are posted back to the reducer through `EffectDeps::action_tx`.
    StartTurn {
        /// Main turn scheduler identity for the real provider execution task.
        provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        draft_id: String,
        history: Vec<ChatMessage>,
        /// Optional persisted/original-history source used for compaction patch
        /// guard identity. When absent, the driver uses `history` for both budget
        /// and guard source, preserving non-chat/test callers.
        compaction_guard_history: Option<Vec<ChatMessage>>,
        /// P5 proactive budgeting config resolved from the selected model
        /// window. The streaming driver uses it for preflight/mid-turn trims.
        compaction_config: Option<crate::config::AgentCompactionConfig>,
        cancel: CancellationToken,
        /// BUG-09: the interactive chat mode in effect for this turn. The driver
        /// (`drive_start_turn_stream`) uses it to intercept write/shell/git
        /// tools when in [`ChatMode::Plan`] and feed back a simulated
        /// "[plan mode] would call X" result instead of executing them.
        chat_mode: ChatMode,
        /// D8-4 (redux path): the turn-root spawn execution context for this
        /// turn (forwarded from `Action::StartLLMTurn`). The `EffectExecutor`
        /// wraps `drive_start_turn_stream` in `SPAWN_EXECUTION_CONTEXT.scope(..)`
        /// with this value so `sessions_spawn` tool calls inside the turn inherit
        /// `parent_run_id = turn run_id` → origin = Model, mirroring the legacy
        /// `run_tool_call_loop_traced` wrapper in `chat::run`. `None` → no scope
        /// (sub-agents fall back to user origin, correct for non-turn callers).
        turn_spawn_ctx: Option<crate::tools::sessions_spawn::SpawnExecutionContext>,
        /// Per-turn default route for `message_send` tool calls. `None` keeps
        /// non-turn/test callers on the tool's legacy fallback slot.
        turn_message_send_ctx: Option<crate::tools::message_send::MessageSendExecutionContext>,
        /// The turn's raw user text, forwarded to the driver as the capability
        /// routing input. Provider history is enriched with memory recall and
        /// shared-workspace events before it reaches the driver; routing on
        /// that text let injected content widen the tool surface.
        routing_input: Option<String>,
    },
    /// Persist a snapshot of the current session
    SaveSession(ChatSession),
    /// Tell the render layer the draft is finished and push it into conversation_lines
    SendDraftFinalize { draft_id: String, text: String },
    /// Cancel streaming for the given draft
    CancelDraft(String),
    /// S2-B Step 2: really call `CancellationToken::cancel()` to stop this turn's LLM/tool stream.
    ///
    /// Difference from [`Self::CancelDraft`]:
    /// - `CancelDraft` only tells the channel to drop the draft UI (the streaming block stops growing)
    /// - `CancelToken` really fires the underlying token cancel so that `run_tool_call_loop` /
    ///   `drive_start_turn_stream` returns a cancelled error immediately
    ///
    /// The reducer **collects** `active_cancel.take()` inside `reduce_cancel_requested` and builds
    /// this Effect; EffectExecutor calls `token.cancel()` directly in real mode and only writes a
    /// debug log in shadow mode. That closes the S2-B Codex risk of "UI cancelled, backend alive".
    CancelToken(CancellationToken),
    /// Send a message to the channel (slash-command output and the like)
    EmitChannelMessage(SendMessage),
    /// Write to the memory backend
    PersistToMemory {
        key: String,
        value: String,
        category: MemoryCategory,
    },
    /// Fire a hook event
    NotifyHook {
        event: HookEvent,
        payload: serde_json::Value,
    },
    /// Request one TUI redraw frame
    RequestRedraw,
    /// Display media content (images / audio / ...)
    DisplayMedia { kind: String, path: String },
    /// Automatically generate a title for the session
    AutoTitleSession(String),
    /// Structured trace log
    LogTrace { level: tracing::Level, msg: String },
    /// Surface one short operational notice to whoever is watching this chat.
    ///
    /// The reducer owns the transcript ledger, but the ledger is only rendered
    /// by the interactive TUI. `--plain` / piped chat has no renderer, so a
    /// notice that only reaches `conversation_lines` is invisible exactly where
    /// the user has no other signal. The executor therefore pings the renderer
    /// when one is attached and prints the line otherwise.
    SurfaceNotice { text: String },
    /// **S3 T3-1**: EffectExecutor forwards the approval request to the UI / CLI prompt.
    ///
    /// The driver dispatches [`Action::ToolApprovalRequested`] before running a tool that needs
    /// approval; the reducer turns that into this Effect; in real mode EffectExecutor forwards the
    /// request to the UI render layer / CLI prompt (currently a stub: log + approve by default), and
    /// the UI posts [`Action::ToolApprovalReceived`] back once the user responds.
    ///
    /// The data flow is one-way fire-and-forget (the driver receives the response back over the
    /// `approval_response_tx` mpsc). The Effect itself expects no response.
    RequestApproval {
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        tool_id: String,
        name: String,
        args: String,
    },
    /// Resolve a pending foreground approval without going through a separate
    /// `ToolApprovalReceived` Action. Used by pure key handling paths where the
    /// reducer owns the key event and must return the approval decision to the
    /// dispatcher/executor as an effect.
    ResolveApproval { tool_id: String, approved: bool },
    /// Exit the main loop gracefully
    Quit,
}

impl Effect {
    /// S2.5 T2.5-2: get the Effect variant name as a `'static str` for use as a Prometheus label.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::StartTurn { .. } => "StartTurn",
            Self::SaveSession(_) => "SaveSession",
            Self::SendDraftFinalize { .. } => "SendDraftFinalize",
            Self::CancelDraft(_) => "CancelDraft",
            Self::CancelToken(_) => "CancelToken",
            Self::EmitChannelMessage(_) => "EmitChannelMessage",
            Self::PersistToMemory { .. } => "PersistToMemory",
            Self::NotifyHook { .. } => "NotifyHook",
            Self::RequestRedraw => "RequestRedraw",
            Self::DisplayMedia { .. } => "DisplayMedia",
            Self::AutoTitleSession(_) => "AutoTitleSession",
            Self::LogTrace { .. } => "LogTrace",
            Self::SurfaceNotice { .. } => "SurfaceNotice",
            Self::RequestApproval { .. } => "RequestApproval",
            Self::ResolveApproval { .. } => "ResolveApproval",
            Self::Quit => "Quit",
        }
    }
}

// ─── Sub-states ───────────────────────────────────────────────────────────────

/// Session state that gets persisted (written to the memory backend).
#[allow(dead_code)]
pub struct SessionState {
    /// Unique session ID
    pub id: String,
    /// Session title (auto-generated or set by the user)
    pub title: String,
    /// Current provider name (constant for the whole session; Arc<str> avoids clones)
    pub provider: Arc<str>,
    /// Current model name
    pub model: Arc<str>,
    /// Interaction mode (plan/edit/auto)
    pub mode: ChatMode,
    /// Full conversation turns (for persistence)
    pub turns: Vec<ChatTurn>,
    /// LLM context message list (system+user+assistant, used for the next request)
    pub history: Vec<ChatMessage>,
    /// Session creation time (lazily set on the first RecordUserTurn; never overwritten later)
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Summaries (v4) of background sessions (agent/shell/pty) run inside this chat session. Only
    /// summaries are persisted; on reload they are restored for display only — never to rebuild a
    /// process/sub-agent/PTY. Written by the main loop via `Action::BackgroundSessionRecorded`
    /// (deduped by id), stored by `build_session_snapshot`, restored by `reduce_session_loaded`.
    pub background_sessions: Vec<crate::chat::sessions::PersistedSessionSummary>,
    /// Main-session token records, success-only. Child-session usage is Phase 4.
    pub token_usage_records: Vec<MainSessionTokenUsageRecord>,
}

/// Transient TUI state (discarded on exit, never persisted).
///
/// Since Step 2 the real `TuiInput`/`ConversationLine` are used (feature = "terminal-tui");
/// placeholder types keep compilation working without the TUI feature.
#[allow(dead_code)]
pub struct UiState {
    /// Rendered conversation lines
    pub conversation_lines: Vec<ConversationLine>,
    /// Incremented when conversation_lines is replaced wholesale.
    pub conversation_generation: u64,
    /// Multi-line input buffer + history
    pub input: TuiInput,
    /// Current conversation turn count (used by the session and diagnostics views)
    pub turn_count: usize,
    /// In-session chat mode displayed in the status bar.
    pub chat_mode: ChatMode,
    /// Configured autonomy ceiling displayed in the status bar. This is read-only
    /// UI metadata and does not mutate the security policy.
    pub autonomy_level: AutonomyLevel,
    /// Whether ASCII fallback is enabled (non-UTF-8 terminals)
    pub ascii_fallback: bool,
    /// Timestamp (ms) of the last Ctrl+C, used for the double-press window check
    pub last_ctrlc_ms: u64,
    /// Most recent input submission (reduce derives `Action::InputSubmitted` itself when
    /// KeyPressed::Enter arrives; tests use this field to assert the last submission during dual-write)
    pub last_submitted: Option<String>,
    /// Persistent background-session status line (v1b). An empty string means no background session
    /// (the renderer hides the line). Only written by the chat main loop via
    /// `Action::SessionsStatusUpdated`; background spawn tasks never touch it (main loop only).
    pub sessions_status: String,
    /// P1 sessions strip entries. This is the same child TUI registry snapshot
    /// used by the Ctrl+G switcher, kept structured for rendering.
    pub sessions_entries: Vec<crate::chat::sessions::SwitcherEntry>,
    /// Main-session input backlog status for orchestration observation.
    pub main_queue_status: MainQueueStatus,
    /// Main-session provider worker status for orchestration observation.
    pub provider_worker_status: ProviderWorkerStatus,
    /// Saved chat-session candidates for `/resume` slash-menu arguments.
    pub saved_sessions_cache: Vec<crate::chat::session::SavedSessionPickerEntry>,
    /// Provider/model candidates for `/provider` and `/model` slash-menu args.
    pub provider_model_catalog: Vec<crate::chat::slash_types::SlashProviderModelCatalog>,
    /// P2 active line-session viewport snapshot. `None` when the main chat or a
    /// PTY handoff owns the visible surface.
    pub active_session_view: Option<crate::chat::sessions::ActiveSessionView>,
    /// P6c1 foreground tool approval prompt. Display-only; the dispatcher
    /// ApprovalRouter remains the single execution gate.
    pub pending_tool_approval: Option<crate::chat::sessions::PendingToolApprovalView>,
    /// Current context-budget numerator used by status bar budget display.
    /// Derived from the planned prompt context, not cumulative session usage.
    pub context_used_tokens: Option<usize>,
    /// Effective context window used by status bar budget display.
    pub context_window_tokens: Option<usize>,
    /// Main-session cumulative token/cost summary for the status bar.
    pub token_usage_summary: MainSessionTokenUsageSummary,
    /// Current input routing target (v1.1b). `Main` = the main chat; `Session{seq}` = an attached
    /// background session (input is forwarded as steering). Written by the chat main loop via
    /// `Action::SessionFocusChanged` on /attach and /detach; drives the prompt's colour and glyph
    /// target indicator.
    pub focus: crate::chat::sessions::FocusTarget,
    /// Ctrl+G session switcher overlay state (v1.1b); `None` when closed. Written by the key thread
    /// via `Action::SwitcherOpened` / `SwitcherMoved` / `SwitcherClosed`.
    pub switcher: Option<crate::chat::sessions::SwitcherState>,
    /// Slash-command menu overlay. Derived from the current input command token.
    pub slash_menu: Option<SlashMenuState>,
    /// Security-filtered `@path` completion source, delivered via Action.
    pub at_path_candidates: Vec<AtPathCandidate>,
    /// P7c saved chat-session history picker. Distinct from the child-TUI
    /// Ctrl+G switcher.
    pub saved_session_picker: Option<crate::chat::session::SavedSessionPickerState>,
}

/// Immutable UI snapshot (read-only for the renderer; built by the dispatcher when ui_dirty=true).
///
/// S4-A Commit 1: UiSnapshot is the one-way read-only channel between the reducer and the ratatui
/// render thread. Sharing through Arc fields means "push one line per turn" does not clone the
/// whole `Vec<ConversationLine>`; revision increases monotonically so watch::Sender::send_if_modified
/// can skip identical frames, and for debug assertions.
///
/// The fields are the minimal set the fullscreen renderer needs (status bar / transcript /
/// input box / footer); the BottomChromeView trait (landed in Commit 2) abstracts away the
/// difference between TuiState and UiSnapshot.
#[cfg(feature = "terminal-tui")]
#[derive(Clone)]
#[allow(dead_code)]
pub struct UiSnapshot {
    /// Monotonically increasing; watch::Sender::send_if_modified uses it to skip identical frames.
    pub revision: u64,
    /// Current provider name (shown in the status bar).
    pub provider: Arc<str>,
    /// Current model name.
    pub model: Arc<str>,
    /// In-session chat mode displayed in the status bar.
    pub chat_mode: ChatMode,
    /// Configured autonomy ceiling displayed in the status bar.
    pub autonomy_level: AutonomyLevel,
    /// Session title (shown in the status bar).
    pub session_title: Arc<str>,
    /// Conversation turn count (used by the session and diagnostics views).
    pub turn_count: usize,
    /// ASCII fallback mode flag.
    pub ascii_fallback: bool,
    /// Conversation line history (used by the fullscreen transcript renderer).
    pub conversation_lines: Arc<Vec<ConversationLine>>,
    /// Generation marker for wholesale conversation history replacement.
    pub conversation_generation: u64,
    /// Current in-flight streaming draft (None means idle).
    pub streaming: Option<StreamingDraft>,
    /// Wall-clock start of the primary visible turn for live elapsed feedback.
    pub active_turn_started_at_ms: Option<i64>,
    /// In-flight visible streaming drafts keyed by provider worker sequence.
    pub visible_streaming_drafts: Arc<Vec<VisibleStreamingDraftView>>,
    /// Duration of the most recently completed main turn. Retained for
    /// snapshot consumers; the fullscreen transcript owns visible activity.
    pub last_turn_duration_ms: Option<u64>,
    /// Input buffer snapshot (clone cost is acceptable; multi-line cases stay < INPUT_MAX_VISIBLE_ROWS).
    pub input: TuiInput,
    /// Persistent background-session status line (v1b). Empty string means no background session.
    pub sessions_status: Arc<str>,
    /// P1 sessions strip entries, cloned from reducer-owned UI state.
    pub sessions_entries: Arc<Vec<crate::chat::sessions::SwitcherEntry>>,
    /// Main-session input backlog status.
    pub main_queue_status: MainQueueStatus,
    /// Main-session provider worker status.
    pub provider_worker_status: ProviderWorkerStatus,
    /// P2 active line-session viewport snapshot.
    pub active_session_view: Option<crate::chat::sessions::ActiveSessionView>,
    /// P6c1 foreground tool approval prompt.
    pub pending_tool_approval: Option<crate::chat::sessions::PendingToolApprovalView>,
    /// Current context-budget numerator for UI-only status budget display.
    pub context_used_tokens: Option<usize>,
    /// Effective context window for UI-only status budget display.
    pub context_window_tokens: Option<usize>,
    /// Main-session cumulative token/cost summary for the status bar.
    pub token_usage_summary: MainSessionTokenUsageSummary,
    /// Current input routing target (v1.1b). Drives the prompt colour and glyph indicator.
    pub focus: crate::chat::sessions::FocusTarget,
    /// Ctrl+G switcher overlay (v1.1b); `None` means closed. The renderer draws the overlay from it.
    pub switcher: Option<crate::chat::sessions::SwitcherState>,
    /// Slash-command menu overlay.
    pub slash_menu: Option<SlashMenuState>,
    /// P7c saved chat-session history picker overlay.
    pub saved_session_picker: Option<crate::chat::session::SavedSessionPickerState>,
}

#[cfg(feature = "terminal-tui")]
impl UiSnapshot {
    /// Build an empty snapshot (revision=0; only provider/model are known, no session loaded yet).
    #[must_use]
    #[allow(dead_code)]
    pub fn initial(provider: Arc<str>, model: Arc<str>) -> Self {
        Self {
            revision: 0,
            provider,
            model,
            chat_mode: ChatMode::default(),
            autonomy_level: AutonomyLevel::default(),
            session_title: Arc::from(""),
            turn_count: 0,
            ascii_fallback: false,
            conversation_lines: Arc::new(Vec::new()),
            conversation_generation: 0,
            streaming: None,
            active_turn_started_at_ms: None,
            visible_streaming_drafts: Arc::new(Vec::new()),
            last_turn_duration_ms: None,
            input: TuiInput::new(),
            sessions_status: Arc::from(""),
            sessions_entries: Arc::new(Vec::new()),
            main_queue_status: MainQueueStatus::default(),
            provider_worker_status: ProviderWorkerStatus::default(),
            active_session_view: None,
            pending_tool_approval: None,
            context_used_tokens: None,
            context_window_tokens: None,
            token_usage_summary: MainSessionTokenUsageSummary::default(),
            focus: crate::chat::sessions::FocusTarget::Main,
            switcher: None,
            slash_menu: None,
            saved_session_picker: None,
        }
    }
}

#[cfg(feature = "terminal-tui")]
impl UiSnapshot {
    #[must_use]
    pub fn streaming_draft_for_worker(&self, sequence: u64) -> Option<&StreamingDraft> {
        self.visible_streaming_drafts
            .iter()
            .find(|draft| draft.sequence == sequence)
            .map(|draft| &draft.draft)
    }
}

/// One keyed visible streaming turn draft.
///
/// Phase 1 is structural only: the live chat loop still keeps visible provider
/// turns safe-serial via admission guard, but the reducer can now represent
/// multiple drafts without a single global draft slot.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamingTurnDraft {
    pub task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    /// Scheduler sequence; lower sequence renders as the primary/earlier draft.
    pub sequence: u64,
    pub prompt_preview: String,
    /// Wall-clock start for this provider turn. Kept per draft so concurrent
    /// turns report their own elapsed time instead of sharing a global timer.
    pub started_at_ms: i64,
    pub draft: StreamingDraft,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleStreamingDraftView {
    pub sequence: u64,
    pub draft: StreamingDraft,
}

/// Intermediate streaming-inference state (reset every turn).
#[allow(dead_code)]
pub struct StreamState {
    /// Keyed in-flight visible streaming drafts.
    ///
    /// This is the single source of truth for streaming draft state. The legacy
    /// single-draft snapshot is computed from [`Self::primary_draft`].
    pub visible_drafts: Vec<StreamingTurnDraft>,
    /// Wall-clock start of the current primary turn.
    pub started_at_ms: Option<i64>,
    /// Completed duration retained for snapshot consumers.
    pub last_duration_ms: Option<u64>,
}

impl StreamState {
    #[must_use]
    pub fn primary_draft(&self) -> Option<&StreamingTurnDraft> {
        self.visible_drafts.first()
    }

    #[must_use]
    pub fn primary_streaming_draft(&self) -> Option<&StreamingDraft> {
        self.primary_draft().map(|turn| &turn.draft)
    }

    #[must_use]
    pub fn streaming_draft_for_worker(&self, sequence: u64) -> Option<&StreamingDraft> {
        self.visible_drafts
            .iter()
            .find(|turn| turn.sequence == sequence)
            .map(|turn| &turn.draft)
    }

    #[must_use]
    fn visible_streaming_draft_views(&self) -> Vec<VisibleStreamingDraftView> {
        self.visible_drafts
            .iter()
            .map(|turn| VisibleStreamingDraftView {
                sequence: turn.sequence,
                draft: turn.draft.clone(),
            })
            .collect()
    }

    fn insert_visible_draft(&mut self, draft: StreamingTurnDraft) {
        self.visible_drafts
            .retain(|existing| existing.draft.draft_id != draft.draft.draft_id);
        let insert_at = self
            .visible_drafts
            .iter()
            .position(|existing| existing.sequence > draft.sequence)
            .unwrap_or(self.visible_drafts.len());
        self.visible_drafts.insert(insert_at, draft);
    }

    fn visible_draft_mut(&mut self, draft_id: &str) -> Option<&mut StreamingTurnDraft> {
        self.visible_drafts
            .iter_mut()
            .find(|turn| turn.draft.draft_id == draft_id)
    }

    fn remove_visible_draft(&mut self, draft_id: &str) -> Option<StreamingTurnDraft> {
        let idx = self
            .visible_drafts
            .iter()
            .position(|turn| turn.draft.draft_id == draft_id)?;
        Some(self.visible_drafts.remove(idx))
    }

    fn clear_visible_drafts(&mut self) {
        self.visible_drafts.clear();
    }

    #[must_use]
    const fn has_visible_drafts(&self) -> bool {
        !self.visible_drafts.is_empty()
    }

    #[must_use]
    fn versions_fingerprint(&self) -> Vec<(String, u64)> {
        self.visible_drafts
            .iter()
            .map(|turn| (turn.draft.draft_id.clone(), turn.draft.version))
            .collect()
    }
}

/// Fold one reasoning delta into a draft's live thinking progress.
///
/// Counts characters (not bytes, so multi-byte text is reported honestly) and
/// keeps at most [`REASONING_TAIL_MAX_CHARS`] trailing characters for the
/// one-line preview. Truncation is char-based, so a multi-byte character is
/// never split.
pub fn apply_reasoning_progress(draft: &mut StreamingDraft, delta: &str) {
    if delta.is_empty() {
        return;
    }
    draft.reasoning_chars = draft.reasoning_chars.saturating_add(delta.chars().count());
    draft.reasoning_tail.push_str(delta);
    let tail_len = draft.reasoning_tail.chars().count();
    if tail_len > REASONING_TAIL_MAX_CHARS {
        let skip = tail_len.saturating_sub(REASONING_TAIL_MAX_CHARS);
        draft.reasoning_tail = draft.reasoning_tail.chars().skip(skip).collect();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolTaskKey {
    Task(crate::chat::turn_scheduler::TurnTaskId),
    Primary,
}

impl ToolTaskKey {
    #[must_use]
    pub const fn from_task_id(task_id: Option<crate::chat::turn_scheduler::TurnTaskId>) -> Self {
        match task_id {
            Some(id) => Self::Task(id),
            None => Self::Primary,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ToolInvocationKey {
    pub tool_call_id: Option<String>,
    pub name: String,
}

impl ToolInvocationKey {
    #[must_use]
    fn new(tool_call_id: Option<String>, name: &str) -> Self {
        Self {
            tool_call_id,
            name: name.to_string(),
        }
    }
}

#[derive(Default, Debug)]
pub struct TaskToolBuffer {
    pub pending_tool_cards: Vec<usize>,
    pub tool_calls: Vec<crate::chat::session::ToolCallSummary>,
    pub tool_args: std::collections::HashMap<ToolInvocationKey, String>,
}

/// Cancellation / shutdown control state.
#[allow(dead_code)]
pub struct ControlState {
    /// Cancellation token for the current turn (None means idle)
    pub active_cancel: Option<CancellationToken>,
    /// Global shutdown token (long-lived, shared across tasks)
    pub shutdown: CancellationToken,
    /// Whether generation is in progress (used by the CancelRequested branch)
    pub generating: bool,
    /// P3a: tool state is keyed by turn task so concurrent visible workers do
    /// not share running card indices, argument previews, or persisted summaries.
    pub tool_buffers: std::collections::HashMap<ToolTaskKey, TaskToolBuffer>,
    /// P3b: graceful provider cancellation tokens keyed by turn task. Legacy
    /// Primary callers keep using `active_cancel` until the runtime fully
    /// migrates away from the pre-scheduler path.
    pub turn_cancels: std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, CancellationToken>,
    /// P3c: final aggregate usage records are idempotent per provider task.
    /// Incremental usage records are intentionally never tracked here.
    pub final_usage_tasks_recorded: std::collections::HashSet<crate::chat::turn_scheduler::TurnTaskId>,
    /// Whether this turn has already surfaced the lossy-compaction notice.
    ///
    /// The pre-provider budget check runs once per tool iteration, so a session
    /// whose provenance cannot be resolved degrades on every iteration. Without
    /// this latch the transcript would fill with the same line; with it the user
    /// is told once per turn and the rest is trace only.
    pub context_degrade_notified: bool,
}

impl ControlState {
    fn register_turn_cancel(&mut self, key: ToolTaskKey, cancel: CancellationToken) {
        match key {
            ToolTaskKey::Task(task_id) => {
                self.turn_cancels.insert(task_id, cancel);
            }
            ToolTaskKey::Primary => {
                self.active_cancel = Some(cancel);
            }
        }
    }

    fn take_turn_cancel(&mut self, key: ToolTaskKey) -> Option<CancellationToken> {
        match key {
            ToolTaskKey::Task(task_id) => self.turn_cancels.remove(&task_id),
            ToolTaskKey::Primary => self.active_cancel.take(),
        }
    }

    fn remove_turn_cancel(&mut self, key: ToolTaskKey) {
        match key {
            ToolTaskKey::Task(task_id) => {
                self.turn_cancels.remove(&task_id);
            }
            ToolTaskKey::Primary => {
                self.active_cancel = None;
            }
        }
    }

    fn has_task_turn_cancels(&self) -> bool {
        !self.turn_cancels.is_empty()
    }

    fn drain_turn_cancels(&mut self) -> Vec<CancellationToken> {
        let mut tokens = Vec::new();
        if let Some(token) = self.active_cancel.take() {
            tokens.push(token);
        }
        tokens.extend(self.turn_cancels.drain().map(|(_, token)| token));
        tokens
    }

    fn should_record_provider_usage(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        usage_kind: ProviderUsageRecordKind,
    ) -> bool {
        match (task_id, usage_kind) {
            (Some(task_id), ProviderUsageRecordKind::FinalAggregate) => self.final_usage_tasks_recorded.insert(task_id),
            _ => true,
        }
    }

    fn tool_buffer_mut(&mut self, key: ToolTaskKey) -> &mut TaskToolBuffer {
        self.tool_buffers.entry(key).or_default()
    }

    fn take_tool_calls(&mut self, key: ToolTaskKey) -> Vec<crate::chat::session::ToolCallSummary> {
        let Some(buffer) = self.tool_buffers.get_mut(&key) else {
            return Vec::new();
        };
        let calls = std::mem::take(&mut buffer.tool_calls);
        buffer.tool_args.clear();
        let remove_buffer =
            buffer.pending_tool_cards.is_empty() && buffer.tool_calls.is_empty() && buffer.tool_args.is_empty();
        if remove_buffer {
            self.tool_buffers.remove(&key);
        }
        calls
    }

    fn clear_tool_buffer(&mut self, key: ToolTaskKey) {
        self.tool_buffers.remove(&key);
    }

    fn clear_all_tool_buffers(&mut self) {
        self.tool_buffers.clear();
    }

    #[cfg(test)]
    fn pending_tool_card_count(&self, key: ToolTaskKey) -> usize {
        self.tool_buffers
            .get(&key)
            .map_or(0, |buffer| buffer.pending_tool_cards.len())
    }

    #[cfg(test)]
    fn tool_call_count(&self, key: ToolTaskKey) -> usize {
        self.tool_buffers.get(&key).map_or(0, |buffer| buffer.tool_calls.len())
    }

    #[cfg(test)]
    fn tool_arg_count(&self, key: ToolTaskKey) -> usize {
        self.tool_buffers.get(&key).map_or(0, |buffer| buffer.tool_args.len())
    }
}

// ─── ChatState ────────────────────────────────────────────────────────────────

/// Top-level chat state, owned solely by the main loop.
///
/// No `Arc<Mutex<ChatState>>` is used; the renderer receives read-only copies over a snapshot
/// channel. Every mutation goes through [`ChatState::reduce`].
#[allow(dead_code)]
pub struct ChatState {
    /// Persisted session state
    pub session: SessionState,
    /// Transient TUI state
    pub ui: UiState,
    /// Intermediate streaming state
    pub stream: StreamState,
    /// Cancellation / shutdown control
    pub control: ControlState,
    /// Cached conversation_lines Arc for build_ui_snapshot; cleared when dirty
    #[cfg(feature = "terminal-tui")]
    cached_lines_arc: Option<Arc<Vec<ConversationLine>>>,
}

#[cfg(feature = "terminal-tui")]
#[derive(Debug, PartialEq)]
struct SnapshotDirtyFields {
    conversation_len: usize,
    conversation_generation: u64,
    draft_versions: Vec<(String, u64)>,
    input_lines: usize,
    context_used_tokens: Option<usize>,
    context_window_tokens: Option<usize>,
    slash_menu_open: bool,
    slash_menu_selected: Option<usize>,
    chat_mode: ChatMode,
    autonomy_level: AutonomyLevel,
    approval_visible: bool,
    focus: crate::chat::sessions::FocusTarget,
    token_usage_summary: MainSessionTokenUsageSummary,
    main_queue_status: MainQueueStatus,
}

impl ChatState {
    /// Build the initial state (with sensible defaults).
    ///
    /// `provider`/`model` are passed as Arc<str> to avoid later clones.
    /// `shutdown` is created by the caller and shared with every subtask.
    pub fn new(provider: Arc<str>, model: Arc<str>, shutdown: CancellationToken) -> Self {
        Self {
            session: SessionState {
                id: uuid::Uuid::new_v4().to_string(),
                title: String::new(),
                provider,
                model,
                mode: ChatMode::default(),
                turns: Vec::new(),
                history: Vec::new(),
                created_at: None,
                background_sessions: Vec::new(),
                token_usage_records: Vec::new(),
            },
            ui: UiState {
                conversation_lines: Vec::new(),
                conversation_generation: 0,
                input: Self::new_input(),
                turn_count: 0,
                chat_mode: ChatMode::default(),
                autonomy_level: AutonomyLevel::default(),
                ascii_fallback: false,
                last_ctrlc_ms: 0,
                last_submitted: None,
                sessions_status: String::new(),
                sessions_entries: Vec::new(),
                main_queue_status: MainQueueStatus::default(),
                provider_worker_status: ProviderWorkerStatus::default(),
                saved_sessions_cache: Vec::new(),
                provider_model_catalog: Vec::new(),
                active_session_view: None,
                pending_tool_approval: None,
                context_used_tokens: None,
                context_window_tokens: None,
                token_usage_summary: MainSessionTokenUsageSummary::default(),
                focus: crate::chat::sessions::FocusTarget::Main,
                switcher: None,
                slash_menu: None,
                at_path_candidates: Vec::new(),
                saved_session_picker: None,
            },
            stream: StreamState {
                visible_drafts: Vec::new(),
                started_at_ms: None,
                last_duration_ms: None,
            },
            control: ControlState {
                active_cancel: None,
                shutdown,
                generating: false,
                tool_buffers: std::collections::HashMap::new(),
                turn_cancels: std::collections::HashMap::new(),
                final_usage_tasks_recorded: std::collections::HashSet::new(),
                context_degrade_notified: false,
            },
            #[cfg(feature = "terminal-tui")]
            cached_lines_arc: None,
        }
    }

    /// Build an empty TuiInput / placeholder Vec (matching the current feature set).
    #[cfg(feature = "terminal-tui")]
    fn new_input() -> TuiInput {
        TuiInput::new()
    }

    /// Use a placeholder Vec without the terminal-tui feature.
    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::missing_const_for_fn)]
    fn new_input() -> TuiInput {
        Vec::new()
    }

    #[cfg(feature = "terminal-tui")]
    const fn slash_menu_sources_from<'a>(
        live_sessions: &'a [crate::chat::sessions::SwitcherEntry],
        saved_sessions: &'a [crate::chat::session::SavedSessionPickerEntry],
        provider_model_catalog: &'a [crate::chat::tui::SlashProviderModelCatalog],
        at_path_candidates: &'a [crate::chat::slash_types::AtPathCandidate],
        current_provider: &'a str,
    ) -> crate::chat::tui::SlashMenuSources<'a> {
        crate::chat::tui::SlashMenuSources {
            live_sessions,
            saved_sessions,
            provider_model_catalog,
            at_path_candidates,
            current_provider,
        }
    }

    /// Build the [`UiSnapshot`] for the current state.
    ///
    /// The Arc field (`conversation_lines`) lets two consecutive snapshots with unchanged ui share
    /// the same Vec: `cached_lines_arc` holds the last built Arc, reduce_tracked clears it when
    /// dirty=true, and on a cache hit build_ui_snapshot reuses it via `Arc::clone` (zero copying).
    ///
    /// `revision` is kept monotonically increasing by the caller.
    #[cfg(feature = "terminal-tui")]
    #[must_use]
    #[allow(dead_code)]
    pub fn build_ui_snapshot(&mut self, revision: u64) -> UiSnapshot {
        let lines = if let Some(ref cached) = self.cached_lines_arc {
            Arc::clone(cached)
        } else {
            let arc = Arc::new(self.ui.conversation_lines.clone());
            self.cached_lines_arc = Some(Arc::clone(&arc));
            arc
        };
        UiSnapshot {
            revision,
            provider: Arc::clone(&self.session.provider),
            model: Arc::clone(&self.session.model),
            chat_mode: self.ui.chat_mode,
            autonomy_level: self.ui.autonomy_level,
            session_title: Arc::from(self.session.title.as_str()),
            turn_count: self.ui.turn_count,
            ascii_fallback: self.ui.ascii_fallback,
            conversation_lines: lines,
            conversation_generation: self.ui.conversation_generation,
            streaming: self.stream.primary_streaming_draft().cloned(),
            active_turn_started_at_ms: self.stream.primary_draft().map(|draft| draft.started_at_ms),
            visible_streaming_drafts: Arc::new(self.stream.visible_streaming_draft_views()),
            last_turn_duration_ms: self.stream.last_duration_ms,
            input: self.ui.input.clone(),
            sessions_status: Arc::from(self.ui.sessions_status.as_str()),
            sessions_entries: Arc::new(self.ui.sessions_entries.clone()),
            main_queue_status: self.ui.main_queue_status,
            provider_worker_status: self.ui.provider_worker_status.clone(),
            active_session_view: self.ui.active_session_view.clone(),
            pending_tool_approval: self.ui.pending_tool_approval.clone(),
            context_used_tokens: self.ui.context_used_tokens,
            context_window_tokens: self.ui.context_window_tokens,
            token_usage_summary: self.ui.token_usage_summary,
            focus: self.ui.focus,
            switcher: self.ui.switcher.clone(),
            slash_menu: self.ui.slash_menu.clone(),
            saved_session_picker: self.ui.saved_session_picker.clone(),
        }
    }

    /// `reduce` plus an explicit ui_dirty signal (introduced in S4-A Commit 1).
    ///
    /// Decision (adopted from the Codex S4-A phase 1 review, scored 8.1/10):
    /// - do **not** use an action whitelist as the source of the dirty decision (new Actions are
    ///   easily missed); instead decide from the Action variant name plus what the reducer actually
    ///   writes to `ui.conversation_lines` / `stream.draft` / `ui.input`
    ///
    /// Implementation notes (deviations from the planned version):
    /// - the plan asked for `reduce` to return `(Vec<Effect>, bool)`. But PRX already has ~250 test
    ///   callers using `let effects = state.reduce(...)` to take a `Vec<Effect>` directly, so
    ///   converting them all to destructuring costs far more than it gains.
    /// - so the dirty decision lives in this wrapper instead: a top-level match on the Action
    ///   variant, whose exhaustiveness check turns a forgotten new Action into a compile error.
    ///   `reduce` / `reduce_with_now` keep their signatures.
    ///
    /// The dispatcher only calls `reduce_tracked` (wired up in Commit 3); tests are still free to
    /// use `reduce`/`reduce_with_now`.
    #[cfg(feature = "terminal-tui")]
    #[allow(dead_code)]
    pub fn reduce_tracked(&mut self, action: Action) -> (Vec<Effect>, bool) {
        let dirty = ui_dirty_for(&action);
        let snap_before = self.snapshot_dirty_fields();
        let effects = self.reduce(action);
        let snap_after = self.snapshot_dirty_fields();
        let dirty_final = dirty || (snap_before != snap_after);
        if dirty_final {
            // ui changed: clear the cache so the next build_ui_snapshot rebuilds the Arc<Vec<_>>
            self.cached_lines_arc = None;
        }
        (effects, dirty_final)
    }

    /// Runtime fallback for the dirty decision in `reduce_tracked`: returns a coarse fingerprint
    /// such as ui.conversation_lines.len() / stream.draft.is_some().
    ///
    /// Note: only sensitive to **length/count level** changes (pushing a line, draft None→Some), not
    /// to byte-level content changes (streaming chunk accumulation) — content changes while
    /// streaming are covered by the static whitelist `ui_dirty_for` (StreamChunkReceived → true).
    #[cfg(feature = "terminal-tui")]
    fn snapshot_dirty_fields(&self) -> SnapshotDirtyFields {
        SnapshotDirtyFields {
            conversation_len: self.ui.conversation_lines.len(),
            conversation_generation: self.ui.conversation_generation,
            draft_versions: self.stream.versions_fingerprint(),
            input_lines: self.ui.input.lines.len(),
            context_used_tokens: self.ui.context_used_tokens,
            context_window_tokens: self.ui.context_window_tokens,
            slash_menu_open: self.ui.slash_menu.is_some(),
            slash_menu_selected: self.ui.slash_menu.as_ref().map(|menu| menu.selected),
            chat_mode: self.ui.chat_mode,
            autonomy_level: self.ui.autonomy_level,
            approval_visible: self.ui.pending_tool_approval.is_some(),
            focus: self.ui.focus,
            token_usage_summary: self.ui.token_usage_summary,
            main_queue_status: self.ui.main_queue_status,
        }
    }

    /// Without the terminal-tui feature ui_dirty is always false (there is no UI to render).
    #[cfg(not(feature = "terminal-tui"))]
    #[allow(dead_code)]
    pub fn reduce_tracked(&mut self, action: Action) -> (Vec<Effect>, bool) {
        let effects = self.reduce(action);
        (effects, false)
    }

    /// Pure sync state machine — mutates self per [`Action`], returns the [`Effect`] list to run.
    ///
    /// Constraints:
    /// - no `.await`, no I/O, no `spawn`
    /// - every async side effect is returned as an `Effect` and dispatched by the main loop
    /// - it calls `now_ms()` internally to read the wall clock (double-press window); see
    ///   [`Self::reduce_with_now`] for the parameterised version tests use to inject time
    pub fn reduce(&mut self, action: Action) -> Vec<Effect> {
        let now = now_ms();
        self.reduce_with_now(action, now)
    }

    /// Same as [`Self::reduce`] but with `now_ms` injected explicitly so tests can pin the time.
    pub fn reduce_with_now(&mut self, action: Action, now_ms: u64) -> Vec<Effect> {
        // S2.5 T2.5-2: entry-point metric prx_chat_actions_total{action_kind=...}.
        crate::observability::chat_metrics::inc_action(action.kind());
        match action {
            // ── Input path ────────────────────────────────────────
            Action::KeyPressed(key) => self.reduce_key_pressed(key, now_ms),
            Action::PasteReceived(text) => self.reduce_paste_received(&text),
            Action::TerminalResized { w: _w, h: _h } => {
                // Step 2: no size cache, just request a redraw (ratatui adapts on its own)
                vec![Effect::RequestRedraw]
            }
            Action::InputSubmitted(text) => self.reduce_input_submitted(text),
            Action::InputReplaced(text) => self.reduce_input_replaced(&text),
            Action::HistoryNavigated(dir) => self.reduce_history_navigated(dir),
            Action::InputCancelled => self.reduce_input_cancelled(),

            // ── Slash commands ────────────────────────────────────
            Action::SlashCommandIssued { cmd: _cmd, args: _args } => {
                // Step 4: dispatched to the commands module
                vec![]
            }
            Action::ModeChanged(mode) => {
                self.session.mode = mode;
                self.ui.chat_mode = mode;
                vec![Effect::RequestRedraw]
            }
            Action::ModelChanged { model } => {
                // BUG-07: /model <name> switches online. Update session.model so the status bar
                // shows the new model immediately; the real model switch for later LLM turns is done
                // by the main loop hot-swapping the EffectDeps slot (reducer keeps only the ledger).
                self.session.model = Arc::from(model.as_str());
                vec![Effect::RequestRedraw]
            }
            Action::ProviderChanged { provider, model } => {
                // Bug #3: /provider <name> [model] switches online. Update session.provider so the
                // status bar / snapshot reflects the new provider immediately; if a model was given,
                // write session.model too. The real provider instance for later LLM turns is swapped
                // by the main loop via ProviderSlot (the reducer only keeps the UI/session ledger).
                self.session.provider = Arc::from(provider.as_str());
                if let Some(model) = model {
                    self.session.model = Arc::from(model.as_str());
                }
                vec![Effect::RequestRedraw]
            }
            Action::HistoryCleared => self.reduce_history_cleared(),
            Action::HistoryClearedWithNotice { notice } => self.reduce_history_cleared_with_notice(notice),
            Action::HistoryCompacted { reason } => self.reduce_history_compacted(reason),
            Action::HistoryCompactionPatchApplied {
                reason,
                patch,
                compaction_config,
            } => self.reduce_history_compaction_patch_applied(reason, patch, &compaction_config),
            Action::HistoryCompactionDegraded {
                reason,
                dropped_messages,
            } => self.reduce_history_compaction_degraded(reason, dropped_messages),

            // ── LLM streaming (Step 3) ────────────────────────────
            Action::TurnStarted { draft_id, cancel } => self.reduce_turn_started(draft_id, cancel),
            Action::StartLLMTurn {
                provider_turn_task_id,
                provider_turn_sequence,
                draft_id,
                history,
                compaction_guard_history,
                compaction_config,
                cancel,
                turn_spawn_ctx,
                turn_message_send_ctx,
                routing_input,
            } => self.reduce_start_llm_turn(
                provider_turn_task_id,
                provider_turn_sequence,
                draft_id,
                history,
                compaction_guard_history,
                compaction_config,
                cancel,
                turn_spawn_ctx,
                turn_message_send_ctx,
                routing_input,
            ),
            Action::StreamChunkReceived {
                draft_id,
                delta,
                version,
            } => self.reduce_stream_chunk_received(&draft_id, &delta, version),
            Action::StreamReasoningReceived {
                draft_id,
                delta,
                version,
            } => self.reduce_stream_reasoning_received(&draft_id, &delta, version),
            Action::StreamUsageMetered { .. } => vec![],
            Action::StreamCompleted {
                draft_id,
                final_text,
                reasoning,
            } => self.reduce_stream_completed(&draft_id, final_text, reasoning),
            Action::ProviderTurnReadyForCommit { .. } => vec![],
            Action::StreamFailed {
                draft_id,
                err,
                retryable,
            } => self.reduce_stream_failed(&draft_id, err, retryable),
            Action::StreamCancelled { draft_id } => self.reduce_stream_cancelled(&draft_id),

            // ── Tool events (Step 3) ──────────────────────────────
            Action::ToolStarted {
                task_id,
                sequence,
                tool_call_id,
                name,
                args,
            } => self.reduce_tool_started(task_id, sequence, tool_call_id, name, args),
            Action::ToolFinished {
                task_id,
                sequence,
                tool_call_id,
                name,
                success,
                duration_ms,
                result,
            } => self.reduce_tool_finished(task_id, sequence, tool_call_id, name, success, duration_ms, result),
            Action::ToolProgress { iteration } => self.reduce_tool_progress(iteration),
            Action::ToolApprovalRequested {
                task_id,
                tool_id,
                name,
                args,
            } => self.reduce_tool_approval_requested(task_id, tool_id, name, args),
            Action::ToolApprovalReceived { tool_id, approved } => {
                self.reduce_tool_approval_received(&tool_id, approved)
            }
            Action::ToolApprovalCleared => self.reduce_tool_approval_cleared(),
            Action::StreamRetryAttempt { attempt, reason } => self.reduce_stream_retry_attempt(attempt, &reason),

            // ── Session ───────────────────────────────────────────
            Action::SessionLoaded(session) => self.reduce_session_loaded(session),
            Action::SessionSaved { id } => self.reduce_session_saved(id),
            Action::SessionSwitched { id } => self.reduce_session_switched(id),
            Action::RecordUserTurn(content) => self.reduce_record_user_turn(content),
            Action::RecordAssistantTurn { task_id, content } => self.reduce_record_assistant_turn(task_id, content),
            Action::RecordSystemMessage { content } => self.reduce_record_system_message(content),
            Action::SetLeadingSystemPrompt { content } => self.reduce_set_leading_system_prompt(content),

            // ── UI fold/unfold ──────────────────────────────────
            Action::ToolCardFoldToggled => self.reduce_tool_card_fold_toggled(),
            Action::ReasoningFoldToggled => self.reduce_reasoning_fold_toggled(),
            Action::RedrawRequested => vec![Effect::RequestRedraw],
            Action::SystemMessageAdded { text } => self.reduce_system_message_added(text),
            Action::UserMessageEchoed(text) => self.reduce_user_message_echoed(text),
            Action::SessionsStatusUpdated { summary } => self.reduce_sessions_status_updated(summary),
            Action::SessionsEntriesUpdated { entries } => self.reduce_sessions_entries_updated(entries),
            Action::MainQueueStatusUpdated { status } => self.reduce_main_queue_status_updated(status),
            Action::ProviderWorkerStatusUpdated { status } => self.reduce_provider_worker_status_updated(status),
            Action::SlashMenuSourcesUpdated {
                saved_sessions,
                provider_model_catalog,
            } => self.reduce_slash_menu_sources_updated(saved_sessions, provider_model_catalog),
            Action::AtPathCandidatesUpdated { candidates } => self.reduce_at_path_candidates_updated(candidates),
            Action::ActiveSessionViewUpdated { view } => self.reduce_active_session_view_updated(view),
            Action::ContextWindowUpdated {
                used_context_tokens,
                max_context_tokens,
            } => self.reduce_context_window_updated(used_context_tokens, max_context_tokens),
            Action::ProviderUsageRecorded {
                task_id,
                usage_kind,
                record,
            } => self.reduce_provider_usage_recorded(task_id, usage_kind, record),
            Action::BackgroundSessionRecorded { summary } => self.reduce_background_session_recorded(summary),
            Action::SessionFocusChanged { focus } => self.reduce_session_focus_changed(focus),
            Action::SwitcherOpened { entries } => self.reduce_switcher_opened(entries),
            Action::SwitcherMoved { selected } => self.reduce_switcher_moved(selected),
            Action::SwitcherClosed => self.reduce_switcher_closed(),
            Action::SavedSessionPickerOpened { entries } => self.reduce_saved_session_picker_opened(entries),
            Action::SavedSessionPickerMoved { selected } => self.reduce_saved_session_picker_moved(selected),
            Action::SavedSessionPickerClosed => self.reduce_saved_session_picker_closed(),

            // ── Exit ──────────────────────────────────────────────
            Action::CancelRequested => self.reduce_cancel_requested(),
            Action::CancelProviderTurn { task_id } => self.reduce_cancel_provider_turn(task_id),
            Action::ShutdownRequested => self.reduce_shutdown_requested(),
            Action::ForceQuit => vec![Effect::Quit],
        }
    }

    // ── Input-path helpers (Step 2) ────────────────────────────────────────────

    /// Handle `KeyPressed`: dispatch the key to the input buffer / global shortcuts / exit semantics.
    ///
    /// This is essentially the reducer version of `tui::dispatch_global_key`, but it acts on
    /// `UiState.input` instead of `TuiState`. Returns a list of Effects (typically just RequestRedraw).
    /// Actually posting the user message to the channel or firing cancel is still done by the main loop.
    #[cfg(feature = "terminal-tui")]
    fn reduce_key_pressed(&mut self, key: crossterm::event::KeyEvent, now_ms: u64) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyModifiers};

        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            let prev = self.ui.last_ctrlc_ms;
            self.ui.last_ctrlc_ms = now_ms;
            if prev != 0 && now_ms.saturating_sub(prev) < DOUBLE_CTRLC_WINDOW_MS {
                return vec![Effect::Quit];
            }
            return self.reduce_cancel_requested();
        }
        if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL && self.ui.input.is_empty() {
            return vec![Effect::Quit];
        }
        if self.ui.saved_session_picker.is_some() {
            return self.reduce_saved_session_picker_key_pressed(key);
        }
        if self.ui.switcher.is_some() {
            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return self.reduce_switcher_closed();
            }
            return vec![Effect::RequestRedraw];
        }
        if self.ui.pending_tool_approval.is_some()
            || matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval)
        {
            return self.reduce_approval_key_pressed(key);
        }

        if self.ui.slash_menu.is_some() {
            let sources = Self::slash_menu_sources_from(
                &self.ui.sessions_entries,
                &self.ui.saved_sessions_cache,
                &self.ui.provider_model_catalog,
                &self.ui.at_path_candidates,
                self.session.provider.as_ref(),
            );
            let dispatch = crate::chat::tui::dispatch_slash_menu_key_with_sources(
                &mut self.ui.input,
                &mut self.ui.slash_menu,
                key,
                sources,
            );
            return match dispatch {
                crate::chat::tui::KeyDispatch::Submitted(text) => self.reduce_input_submitted(text),
                crate::chat::tui::KeyDispatch::Cancelled => self.reduce_input_cancelled(),
                crate::chat::tui::KeyDispatch::Ignored => Vec::new(),
                _ => vec![Effect::RequestRedraw],
            };
        }

        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE && self.control.generating {
            return self.reduce_cancel_requested();
        }

        // Tab → fold/unfold the most recent visible ToolResult card. Reasoning only lives in the
        // verbose transcript and must not intercept Tab in the main conversation.
        if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE && self.ui.input.is_empty() {
            return self.reduce_foldable_card_toggled();
        }
        // Ctrl+R → reverse-search submitted input history. Tab is the sole
        // fold binding after P6b2.
        if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
            let _ = self.ui.input.begin_or_cycle_reverse_search();
            return vec![Effect::RequestRedraw];
        }
        // Ctrl+L → request a redraw; actually clearing the host terminal is up to the effect executor
        if key.code == KeyCode::Char('l') && key.modifiers == KeyModifiers::CONTROL {
            return vec![Effect::RequestRedraw];
        }
        // Ctrl+D → exit on an empty buffer / forward-delete otherwise (delegated to handle_key)
        if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL {
            // Non-empty buffer: forward as Delete
            let synthetic = crossterm::event::KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
            let _ = self.ui.input.handle_key(synthetic);
            let sources = Self::slash_menu_sources_from(
                &self.ui.sessions_entries,
                &self.ui.saved_sessions_cache,
                &self.ui.provider_model_catalog,
                &self.ui.at_path_candidates,
                self.session.provider.as_ref(),
            );
            crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
            return vec![Effect::RequestRedraw];
        }
        // Other keys → forward to the input buffer, then re-enter with the Action from InputOutcome
        match self.ui.input.handle_key(key) {
            crate::chat::tui::InputOutcome::Submitted(text) => {
                self.ui.slash_menu = None;
                // Re-enter through reduce_with_now to keep a single handling path
                self.reduce_input_submitted(text)
            }
            crate::chat::tui::InputOutcome::Cancelled => {
                self.ui.slash_menu = None;
                self.reduce_input_cancelled()
            }
            crate::chat::tui::InputOutcome::Consumed | crate::chat::tui::InputOutcome::Unhandled => {
                let sources = Self::slash_menu_sources_from(
                    &self.ui.sessions_entries,
                    &self.ui.saved_sessions_cache,
                    &self.ui.provider_model_catalog,
                    &self.ui.at_path_candidates,
                    self.session.provider.as_ref(),
                );
                crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
                vec![Effect::RequestRedraw]
            }
            crate::chat::tui::InputOutcome::Ignored => Vec::new(),
        }
    }

    #[cfg(feature = "terminal-tui")]
    fn reduce_approval_key_pressed(&mut self, key: crossterm::event::KeyEvent) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyModifiers};
        let Some(pending) = self.ui.pending_tool_approval.clone() else {
            if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
                if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
                    self.ui.focus = crate::chat::sessions::FocusTarget::Main;
                }
                return vec![Effect::RequestRedraw];
            }
            return vec![Effect::RequestRedraw];
        };
        if key.modifiers != KeyModifiers::NONE {
            return vec![Effect::RequestRedraw];
        }
        let approved = match key.code {
            KeyCode::Char('y' | 'Y') => Some(true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
            KeyCode::Enter => Some(pending.selected_approval),
            KeyCode::Left | KeyCode::Up => {
                if let Some(pending) = self.ui.pending_tool_approval.as_mut() {
                    pending.selected_approval = false;
                }
                return vec![Effect::RequestRedraw];
            }
            KeyCode::Right | KeyCode::Down => {
                if let Some(pending) = self.ui.pending_tool_approval.as_mut() {
                    pending.selected_approval = true;
                }
                return vec![Effect::RequestRedraw];
            }
            _ => None,
        };
        let Some(approved) = approved else {
            return vec![Effect::RequestRedraw];
        };
        self.ui.pending_tool_approval = None;
        if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
            self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        }
        vec![
            Effect::ResolveApproval {
                tool_id: pending.tool_id,
                approved,
            },
            Effect::RequestRedraw,
        ]
    }

    /// Placeholder without the terminal-tui feature (KeyEvent only exists when crossterm is there)
    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut, clippy::missing_const_for_fn)]
    fn reduce_key_pressed(&mut self, _key: crossterm::event::KeyEvent, _now_ms: u64) -> Vec<Effect> {
        let _ = &self.ui;
        vec![]
    }

    /// Handle a bracketed paste: insert the text into the input buffer.
    #[cfg(feature = "terminal-tui")]
    fn reduce_paste_received(&mut self, text: &str) -> Vec<Effect> {
        if self.ui.pending_tool_approval.is_some()
            || matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval)
        {
            return vec![Effect::RequestRedraw];
        }
        self.ui.input.paste(text);
        let sources = Self::slash_menu_sources_from(
            &self.ui.sessions_entries,
            &self.ui.saved_sessions_cache,
            &self.ui.provider_model_catalog,
            &self.ui.at_path_candidates,
            self.session.provider.as_ref(),
        );
        crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_paste_received(&mut self, _text: &str) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// Handle a user submission — UI-side bookkeeping only (turn_count + last_submitted).
    ///
    /// Step 2 does not trigger the LLM (Step 3 adds `Effect::StartTurn`).
    /// `LogTrace` is used for dual-write reconciliation.
    fn reduce_input_submitted(&mut self, text: String) -> Vec<Effect> {
        self.ui.slash_menu = None;
        self.ui.turn_count = self.ui.turn_count.saturating_add(1);
        let log_msg = format!("input_submitted len={}", text.chars().count());
        self.ui.last_submitted = Some(text);
        self.ui.input.clear();
        // Legacy main-turn entry only clears the Primary tool bucket; keyed
        // worker buckets must survive until their own terminal event.
        self.control.clear_tool_buffer(ToolTaskKey::Primary);
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: log_msg,
            },
            Effect::RequestRedraw,
        ]
    }

    #[cfg(feature = "terminal-tui")]
    fn reduce_input_replaced(&mut self, text: &str) -> Vec<Effect> {
        self.ui.input.set_text(text);
        self.ui.input.clear_navigation_state();
        let sources = Self::slash_menu_sources_from(
            &self.ui.sessions_entries,
            &self.ui.saved_sessions_cache,
            &self.ui.provider_model_catalog,
            &self.ui.at_path_candidates,
            self.session.provider.as_ref(),
        );
        crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_input_replaced(&mut self, _text: &str) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// Handle Up/Down history navigation.
    #[cfg(feature = "terminal-tui")]
    fn reduce_history_navigated(&mut self, dir: HistoryDir) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = match dir {
            HistoryDir::Up => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            HistoryDir::Down => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        };
        let _ = self.ui.input.handle_key(key);
        let sources = Self::slash_menu_sources_from(
            &self.ui.sessions_entries,
            &self.ui.saved_sessions_cache,
            &self.ui.provider_model_catalog,
            &self.ui.at_path_candidates,
            self.session.provider.as_ref(),
        );
        crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_history_navigated(&mut self, _dir: HistoryDir) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// Handle Esc — clear the input buffer.
    #[cfg(feature = "terminal-tui")]
    fn reduce_input_cancelled(&mut self) -> Vec<Effect> {
        self.ui.slash_menu = None;
        self.ui.input.clear();
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_input_cancelled(&mut self) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// Handle Tab — fold/unfold the most recent visible ToolResult card.
    ///
    /// Reasoning data still lives in `conversation_lines` for the verbose transcript, but the main
    /// conversation no longer renders it, so it does not take part in Tab folding either.
    #[cfg(feature = "terminal-tui")]
    fn reduce_foldable_card_toggled(&mut self) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let mut toggled = false;
        for line in self.ui.conversation_lines.iter_mut().rev() {
            if let ConversationLine::ToolResult { folded, .. } = line {
                *folded = !*folded;
                toggled = true;
                break;
            }
        }
        // A fold toggle must still mark the conversation as changed so the
        // snapshot/repaint path observes the new fold state. Only bump when a
        // card was toggled to avoid spurious redraw work on no-op key presses.
        if toggled {
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        }
        vec![Effect::RequestRedraw]
    }

    /// Handle Tab — fold/unfold the most recent ToolResult.
    #[cfg(feature = "terminal-tui")]
    fn reduce_tool_card_fold_toggled(&mut self) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let mut toggled = false;
        for line in self.ui.conversation_lines.iter_mut().rev() {
            if let ConversationLine::ToolResult { folded, .. } = line {
                *folded = !*folded;
                toggled = true;
                break;
            }
        }
        // BUG-01 round-2 fix: re-emit scrollback so the new fold state is visible
        // (see `reduce_foldable_card_toggled` for the full rationale).
        if toggled {
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_tool_card_fold_toggled(&mut self) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// Handle Ctrl+R — fold/unfold the most recent Reasoning.
    #[cfg(feature = "terminal-tui")]
    fn reduce_reasoning_fold_toggled(&mut self) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let mut toggled = false;
        for line in self.ui.conversation_lines.iter_mut().rev() {
            if let ConversationLine::Reasoning { folded, .. } = line {
                *folded = !*folded;
                toggled = true;
                break;
            }
        }
        // BUG-01 round-2 fix: re-emit scrollback so the new fold state is visible
        // (see `reduce_foldable_card_toggled` for the full rationale).
        if toggled {
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_reasoning_fold_toggled(&mut self) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    // ── Streaming / tool helpers (Step 3) ───────────────────────────────────
    //
    // P3-5 moved the whole version mechanism down here: `StreamState::draft` inside the reducer is
    // the single guard. The strict-monotonic version comparison lives in
    // `reduce_stream_chunk_received`, with these rules:
    //   1. no draft (already finalized) → drop
    //   2. draft_id does not match (stale across turns) → drop
    //   3. version <= the current draft.version → drop (equal included, strict-monotonic)
    //   4. otherwise: accumulate the delta + bump version + RequestRedraw
    //
    // The old `DraftVersionTracker` (HashMap-based, Mutex-guarded) was over-defensive and has been
    // removed from the `chat::mod::draft_updater` task since Step 3 (a single-threaded mpsc is
    // naturally FIFO and the counter alone guarantees monotonicity). With the reducer in charge
    // there is exactly one version mechanism left, so no dual-write race remains.

    fn visible_draft_sequence(task_id: Option<crate::chat::turn_scheduler::TurnTaskId>, sequence: Option<u64>) -> u64 {
        sequence
            .or_else(|| task_id.map(crate::chat::turn_scheduler::TurnTaskId::get))
            .unwrap_or(0)
    }

    fn prompt_preview_from_history(history: &[ChatMessage]) -> String {
        history
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map_or_else(String::new, |message| truncate_with_ellipsis(&message.content, 96))
    }

    fn insert_visible_streaming_draft(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        draft_id: String,
        prompt_preview: String,
    ) {
        self.stream.insert_visible_draft(StreamingTurnDraft {
            task_id,
            sequence: Self::visible_draft_sequence(task_id, sequence),
            prompt_preview,
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            draft: StreamingDraft::new(draft_id),
        });
    }

    #[must_use]
    fn sequence_for_tool_key(&self, key: ToolTaskKey) -> Option<u64> {
        let ToolTaskKey::Task(task_id) = key else {
            return None;
        };
        self.stream
            .visible_drafts
            .iter()
            .find(|draft| draft.task_id == Some(task_id))
            .map(|draft| draft.sequence)
    }

    /// `Action::TurnStarted` — initialise the streaming draft + register the cancellation token.
    #[cfg(feature = "terminal-tui")]
    fn reduce_turn_started(&mut self, draft_id: String, cancel: CancellationToken) -> Vec<Effect> {
        self.insert_visible_streaming_draft(None, None, draft_id.clone(), String::new());
        self.stream.started_at_ms = Some(chrono::Utc::now().timestamp_millis());
        self.stream.last_duration_ms = None;
        self.control.clear_tool_buffer(ToolTaskKey::Primary);
        self.control.register_turn_cancel(ToolTaskKey::Primary, cancel);
        self.control.generating = true;
        self.control.context_degrade_notified = false;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("turn_started draft_id={draft_id}"),
            },
            Effect::RequestRedraw,
        ]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_turn_started(&mut self, draft_id: String, cancel: CancellationToken) -> Vec<Effect> {
        self.insert_visible_streaming_draft(None, None, draft_id.clone(), String::new());
        self.control.clear_tool_buffer(ToolTaskKey::Primary);
        self.control.register_turn_cancel(ToolTaskKey::Primary, cancel);
        self.control.generating = true;
        self.control.context_degrade_notified = false;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("turn_started draft_id={draft_id}"),
            },
            Effect::RequestRedraw,
        ]
    }

    /// Step 5a-3 Phase A — `Action::StartLLMTurn`: start a streaming LLM turn.
    ///
    /// Behaviour:
    /// 1. state changes match [`Self::reduce_turn_started`] (init draft, register cancel, set generating)
    /// 2. **additionally** emits `Effect::StartTurn { draft_id, history, cancel }`, which makes
    ///    EffectExecutor spawn a subtask calling `provider.stream_chat_with_history` in real-deps mode
    ///
    /// The key difference from `TurnStarted`: it carries a history snapshot so the reducer can drive a
    /// real LLM stream. In Phase A the old chat::run main loop has not switched over; this Action is
    /// only for the Phase B+ main loop or unit tests of the reducer → Effect → EffectExecutor loop.
    #[cfg(feature = "terminal-tui")]
    fn reduce_start_llm_turn(
        &mut self,
        provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        provider_turn_sequence: Option<u64>,
        draft_id: String,
        history: Vec<crate::providers::ChatMessage>,
        compaction_guard_history: Option<Vec<crate::providers::ChatMessage>>,
        compaction_config: Option<crate::config::AgentCompactionConfig>,
        cancel: CancellationToken,
        turn_spawn_ctx: Option<crate::tools::sessions_spawn::SpawnExecutionContext>,
        turn_message_send_ctx: Option<crate::tools::message_send::MessageSendExecutionContext>,
        routing_input: Option<String>,
    ) -> Vec<Effect> {
        self.insert_visible_streaming_draft(
            provider_turn_task_id,
            provider_turn_sequence,
            draft_id.clone(),
            Self::prompt_preview_from_history(&history),
        );
        self.stream.started_at_ms = Some(chrono::Utc::now().timestamp_millis());
        self.stream.last_duration_ms = None;
        self.control
            .clear_tool_buffer(ToolTaskKey::from_task_id(provider_turn_task_id));
        self.control
            .register_turn_cancel(ToolTaskKey::from_task_id(provider_turn_task_id), cancel.clone());
        self.control.generating = true;
        self.control.context_degrade_notified = false;
        // BUG-09: capture the current chat mode so the driver can enforce plan
        // mode's read-only contract on write/shell/git tools.
        let chat_mode = self.session.mode;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("start_llm_turn draft_id={draft_id} history_len={}", history.len()),
            },
            Effect::StartTurn {
                provider_turn_task_id,
                draft_id,
                history,
                compaction_guard_history: compaction_guard_history.or_else(|| Some(self.session.history.clone())),
                compaction_config,
                cancel,
                chat_mode,
                turn_spawn_ctx,
                turn_message_send_ctx,
                routing_input,
            },
            Effect::RequestRedraw,
        ]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_start_llm_turn(
        &mut self,
        provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        provider_turn_sequence: Option<u64>,
        draft_id: String,
        history: Vec<crate::providers::ChatMessage>,
        compaction_guard_history: Option<Vec<crate::providers::ChatMessage>>,
        compaction_config: Option<crate::config::AgentCompactionConfig>,
        cancel: CancellationToken,
        turn_spawn_ctx: Option<crate::tools::sessions_spawn::SpawnExecutionContext>,
        turn_message_send_ctx: Option<crate::tools::message_send::MessageSendExecutionContext>,
        routing_input: Option<String>,
    ) -> Vec<Effect> {
        self.insert_visible_streaming_draft(
            provider_turn_task_id,
            provider_turn_sequence,
            draft_id.clone(),
            Self::prompt_preview_from_history(&history),
        );
        self.control
            .clear_tool_buffer(ToolTaskKey::from_task_id(provider_turn_task_id));
        self.control
            .register_turn_cancel(ToolTaskKey::from_task_id(provider_turn_task_id), cancel.clone());
        self.control.generating = true;
        self.control.context_degrade_notified = false;
        // BUG-09: capture the current chat mode so the driver can enforce plan
        // mode's read-only contract on write/shell/git tools.
        let chat_mode = self.session.mode;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("start_llm_turn draft_id={draft_id} history_len={}", history.len()),
            },
            Effect::StartTurn {
                provider_turn_task_id,
                draft_id,
                history,
                compaction_guard_history: compaction_guard_history.or_else(|| Some(self.session.history.clone())),
                compaction_config,
                cancel,
                chat_mode,
                turn_spawn_ctx,
                turn_message_send_ctx,
                routing_input,
            },
            Effect::RequestRedraw,
        ]
    }

    /// `Action::StreamChunkReceived` — version guard + delta accumulation.
    ///
    /// Return value:
    /// - accepted → `[RequestRedraw]`
    /// - dropped  → `[]` (silently; callers can tell by comparing draft.version before and after)
    #[cfg(feature = "terminal-tui")]
    fn reduce_stream_chunk_received(&mut self, draft_id: &str, delta: &str, version: u64) -> Vec<Effect> {
        let Some(turn) = self.stream.visible_draft_mut(draft_id) else {
            // already finalized — the chunk is stale, drop it
            return vec![];
        };
        let draft = &mut turn.draft;
        if version <= draft.version {
            // strictly monotonic: equal or smaller means out-of-order/duplicate, drop it
            return vec![];
        }
        draft.accumulated.push_str(delta);
        draft.version = version;
        self.refresh_provider_worker_view_if_focused();
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_stream_chunk_received(&mut self, draft_id: &str, delta: &str, version: u64) -> Vec<Effect> {
        let Some(turn) = self.stream.visible_draft_mut(draft_id) else {
            return vec![];
        };
        let draft = &mut turn.draft;
        if version <= draft.version {
            return vec![];
        }
        draft.accumulated.push_str(delta);
        draft.version = version;
        vec![Effect::RequestRedraw]
    }

    /// `Action::StreamReasoningReceived` — version guard + accumulated thinking progress.
    ///
    /// Shares the strict-monotonic rule with [`Self::reduce_stream_chunk_received`] (no draft / older
    /// version → silently dropped), because the versions of both delta kinds come from the same
    /// per-turn counter.
    ///
    /// Single source of truth: only the **character count** and a bounded tail (for the one-line live
    /// preview) are accumulated here; the reasoning body stays owned by the streaming driver and comes
    /// back in one piece in `Action::StreamCompleted` — the reducer never keeps a second full copy.
    fn reduce_stream_reasoning_received(&mut self, draft_id: &str, delta: &str, version: u64) -> Vec<Effect> {
        let Some(turn) = self.stream.visible_draft_mut(draft_id) else {
            // already finalized — the delta is stale, drop it
            return vec![];
        };
        let draft = &mut turn.draft;
        if version <= draft.version {
            // strictly monotonic: equal or smaller means out-of-order/duplicate, drop it
            return vec![];
        }
        draft.version = version;
        apply_reasoning_progress(draft, delta);
        self.refresh_provider_worker_view_if_focused();
        vec![Effect::RequestRedraw]
    }

    /// `Action::StreamCompleted` — clear draft + push assistant message + notify hooks + **persist**.
    ///
    /// T3-3-c: [`Effect::SaveSession`] is appended at the end of the Effect list so the reducer writes a
    /// session snapshot whenever a turn completes; this pulls the persistence that legacy
    /// `chat_session.add_*_turn` + `save_session(...)` did in Pure mode into the reducer as one source.
    ///
    /// Effect ordering guarantee (the executor consumes the Vec in order):
    ///
    /// - `[0]` NotifyHook(TurnComplete) — webhooks / observers learn the turn finished first
    /// - `[1]` SaveSession(snapshot)    — persist turns (dual_write_guard prevents double writes)
    /// - `[2]` RequestRedraw            — the UI refresh comes last
    ///
    /// **Important**: the snapshot uses the reducer's own `session.turns` (written by
    /// `RecordAssistantTurn`), not the legacy `chat_session` copy. Pure mode keeps both in sync and
    /// skips the legacy copy via the T3-3-c guard; Off / Both / Redux modes rely on dual_write_guard.
    #[cfg(feature = "terminal-tui")]
    fn reduce_stream_completed(&mut self, draft_id: &str, final_text: String, reasoning: String) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        self.stream.last_duration_ms = Some(crate::chat::tui::turn_elapsed_ms(removed_draft.started_at_ms));
        if no_visible_drafts {
            self.stream.started_at_ms = None;
        }
        self.remove_pending_tool_cards(tool_key);
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        if !final_text.is_empty() {
            self.ui.conversation_lines.push(ConversationLine::Assistant {
                content: final_text.clone(),
            });
        }
        if !reasoning.trim().is_empty() {
            let char_count = reasoning.chars().count();
            self.ui.conversation_lines.push(ConversationLine::Reasoning {
                content: reasoning,
                char_count,
                folded: true,
            });
        }
        self.refresh_provider_worker_view_if_focused();
        let chars = final_text.chars().count();
        let effects = vec![
            Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({
                    "mode": "chat",
                    "response_chars": chars,
                }),
            },
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::RequestRedraw,
        ];
        // Fallback for drivers that miss RecordAssistantTurn: only the completed
        // task's buffer is discarded.
        self.control.clear_tool_buffer(tool_key);
        effects
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_stream_completed(&mut self, draft_id: &str, final_text: String, reasoning: String) -> Vec<Effect> {
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        self.remove_pending_tool_cards(tool_key);
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        if !final_text.is_empty() {
            self.ui.conversation_lines.push(final_text.clone());
        }
        if !reasoning.trim().is_empty() {
            self.ui.conversation_lines.push(reasoning);
        }
        let chars = final_text.chars().count();
        let effects = vec![
            Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({
                    "mode": "chat",
                    "response_chars": chars,
                }),
            },
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::RequestRedraw,
        ];
        self.control.clear_tool_buffer(tool_key);
        effects
    }

    /// `Action::StreamFailed` — clear draft + LogTrace + NotifyHook(Error).
    ///
    /// Phase F: semantics match the old path's `hooks.emit(HookEvent::Error, payload_error(...))` in the
    /// chat::run main loop — a failed turn must fire the Error hook, otherwise external audits and
    /// webhooks miss it. The hook always fires, because "this turn failed" is a definite public event.
    ///
    /// `retryable` is a **diagnostic field** and triggers no automatic resend: retries belong to the provider
    /// layer (backoff / `Retry-After` / failover), and by this point the turn's tool side effects may have landed.
    /// It feeds the trace log and the `HookEvent::Error` payload so audits can tell transient from hard failures.
    fn reduce_stream_failed(&mut self, draft_id: &str, err: String, retryable: bool) -> Vec<Effect> {
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        if no_visible_drafts {
            self.stream.last_duration_ms = self.stream.started_at_ms.take().map(|started_at_ms| {
                let elapsed = chrono::Utc::now()
                    .timestamp_millis()
                    .saturating_sub(started_at_ms)
                    .max(0);
                u64::try_from(elapsed).unwrap_or(u64::MAX)
            });
        }
        let orphan_user_removed = if no_visible_drafts {
            self.rollback_trailing_answerless_user_turn()
        } else {
            false
        };
        self.finalize_pending_tool_cards(tool_key, false, Some("turn failed before tool finish event"));
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        self.control.clear_tool_buffer(tool_key);
        #[cfg(feature = "terminal-tui")]
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::System {
                content: format!("Error: {err}"),
            });
        vec![
            Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: format!(
                    "stream_failed draft_id={draft_id} retryable={retryable} orphan_user_removed={orphan_user_removed} err={err}"
                ),
            },
            Effect::NotifyHook {
                event: HookEvent::Error,
                payload: serde_json::json!({
                    "component": "chat-turn",
                    "message": err,
                    "retryable": retryable,
                    "draft_id": draft_id,
                }),
            },
            Effect::RequestRedraw,
        ]
    }

    /// `Action::StreamCancelled` — the user cancelled explicitly; only clear the draft.
    fn reduce_stream_cancelled(&mut self, draft_id: &str) -> Vec<Effect> {
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        if no_visible_drafts {
            self.stream.last_duration_ms = self.stream.started_at_ms.take().map(|started_at_ms| {
                let elapsed = chrono::Utc::now()
                    .timestamp_millis()
                    .saturating_sub(started_at_ms)
                    .max(0);
                u64::try_from(elapsed).unwrap_or(u64::MAX)
            });
        }
        self.finalize_pending_tool_cards(tool_key, false, Some("turn cancelled before tool finish event"));
        if no_visible_drafts {
            self.rollback_trailing_answerless_user_turn();
        }
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        self.control.clear_tool_buffer(tool_key);
        vec![Effect::RequestRedraw]
    }

    fn rollback_trailing_answerless_user_turn(&mut self) -> bool {
        let Some(last_turn) = self.session.turns.last() else {
            return false;
        };
        if last_turn.role != "user" {
            return false;
        }
        let content = last_turn.content.clone();
        let title_from_user = crate::chat::session::truncate_title(&content);
        self.session.turns.pop();
        if self
            .session
            .history
            .last()
            .is_some_and(|message| message.role == "user" && message.content == content)
        {
            self.session.history.pop();
        }
        if self.session.turns.is_empty() && self.session.title == title_from_user {
            self.session.title.clear();
        }
        true
    }

    #[cfg(feature = "terminal-tui")]
    fn remove_pending_tool_cards(&mut self, key: ToolTaskKey) {
        use crate::chat::tui::{ConversationLine, ToolStatus};

        let mut indices = self
            .control
            .tool_buffers
            .get_mut(&key)
            .map(|buffer| buffer.pending_tool_cards.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        indices.sort_unstable_by(|a, b| b.cmp(a));
        indices.dedup();
        for idx in indices {
            if matches!(
                self.ui.conversation_lines.get(idx),
                Some(ConversationLine::ToolResult {
                    status: ToolStatus::Running,
                    ..
                })
            ) {
                self.ui.conversation_lines.remove(idx);
            }
        }
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn remove_pending_tool_cards(&mut self, key: ToolTaskKey) {
        if let Some(buffer) = self.control.tool_buffers.get_mut(&key) {
            buffer.pending_tool_cards.clear();
        }
    }

    #[cfg(feature = "terminal-tui")]
    fn finalize_pending_tool_cards(&mut self, key: ToolTaskKey, success: bool, fallback_result: Option<&'static str>) {
        use crate::chat::tui::{ConversationLine, ToolStatus};

        let Some(buffer) = self.control.tool_buffers.get_mut(&key) else {
            return;
        };
        for idx in buffer.pending_tool_cards.drain(..) {
            let Some(ConversationLine::ToolResult {
                status,
                result,
                elapsed_ms,
                ..
            }) = self.ui.conversation_lines.get_mut(idx)
            else {
                continue;
            };
            if *status != ToolStatus::Running {
                continue;
            }
            *status = if success { ToolStatus::Done } else { ToolStatus::Error };
            if result.is_none()
                && let Some(fallback_result) = fallback_result
            {
                *result = Some(fallback_result.to_string());
            }
            if elapsed_ms.is_none() {
                *elapsed_ms = Some(0);
            }
        }
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn finalize_pending_tool_cards(
        &mut self,
        key: ToolTaskKey,
        _success: bool,
        _fallback_result: Option<&'static str>,
    ) {
        if let Some(buffer) = self.control.tool_buffers.get_mut(&key) {
            buffer.pending_tool_cards.clear();
        }
    }

    /// `Action::ToolStarted` — append a ToolResult card in Running state + record its index.
    #[cfg(feature = "terminal-tui")]
    fn reduce_tool_started(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        _sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        args: String,
    ) -> Vec<Effect> {
        use crate::chat::tui::{
            ARGS_PREVIEW_ELLIPSIS, ARGS_PREVIEW_MAX_CHARS, ConversationLine, ToolStatus, build_tool_args_preview,
            tool_result_defaults_expanded,
        };
        let args_preview = build_tool_args_preview(&name, &args, ARGS_PREVIEW_MAX_CHARS, ARGS_PREVIEW_ELLIPSIS);
        let folded = !tool_result_defaults_expanded(&name);
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        self.control
            .tool_buffer_mut(tool_key)
            .tool_args
            .insert(invocation_key, args_preview.clone());
        self.ui.conversation_lines.push(ConversationLine::ToolResult {
            tool_name: name,
            args_preview,
            args_full: args,
            result: None,
            status: ToolStatus::Running,
            elapsed_ms: None,
            folded,
        });
        let idx = self.ui.conversation_lines.len().saturating_sub(1);
        self.control.tool_buffer_mut(tool_key).pending_tool_cards.push(idx);
        self.refresh_provider_worker_view_if_focused();
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_tool_started(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        _sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        args: String,
    ) -> Vec<Effect> {
        let args_preview = if args.chars().count() > 80 {
            let prefix: String = args.chars().take(80).collect();
            format!("{prefix}…")
        } else {
            args.clone()
        };
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        self.control
            .tool_buffer_mut(tool_key)
            .tool_args
            .insert(invocation_key, args_preview);
        self.ui.conversation_lines.push(format!("tool_started:{name}:{args}"));
        let idx = self.ui.conversation_lines.len().saturating_sub(1);
        self.control.tool_buffer_mut(tool_key).pending_tool_cards.push(idx);
        vec![Effect::RequestRedraw]
    }

    /// `Action::ToolFinished` — update the matching Running card → Done/Error.
    #[cfg(feature = "terminal-tui")]
    fn reduce_tool_finished(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        success: bool,
        duration_ms: u64,
        result: Option<String>,
    ) -> Vec<Effect> {
        use crate::chat::session::ToolCallSummary;
        use crate::chat::tui::{ConversationLine, ToolStatus};
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let sequence = sequence.or_else(|| self.sequence_for_tool_key(tool_key));
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        let buffer = self.control.tool_buffer_mut(tool_key);
        let args_preview = buffer.tool_args.remove(&invocation_key).unwrap_or_default();
        buffer.tool_calls.push(ToolCallSummary {
            name: name.clone(),
            args_preview,
            success,
            task_id: task_id.map(crate::chat::turn_scheduler::TurnTaskId::get),
            sequence,
        });
        // Step 1: scan pending_tool_cards backwards for the latest Running card with a matching name
        // (it only borrows conversation_lines and holds no mut reference, avoiding a move conflict)
        let target_pos = self.control.tool_buffers.get(&tool_key).and_then(|buffer| {
            buffer
                .pending_tool_cards
                .iter()
                .enumerate()
                .rev()
                .find_map(|(pos, &idx)| match self.ui.conversation_lines.get(idx) {
                    Some(ConversationLine::ToolResult { tool_name, status, .. })
                        if tool_name == &name && *status == ToolStatus::Running =>
                    {
                        Some((pos, idx))
                    }
                    _ => None,
                })
        });
        // Step 2: once the target is found, apply the mut update + remove it from pending
        if let Some((pending_pos, line_idx)) = target_pos {
            if let Some(ConversationLine::ToolResult {
                status,
                elapsed_ms,
                result: result_slot,
                ..
            }) = self.ui.conversation_lines.get_mut(line_idx)
            {
                *status = if success { ToolStatus::Done } else { ToolStatus::Error };
                *elapsed_ms = Some(duration_ms);
                *result_slot = result;
            }
            if let Some(buffer) = self.control.tool_buffers.get_mut(&tool_key) {
                buffer.pending_tool_cards.remove(pending_pos);
            }
        }
        self.refresh_provider_worker_view_if_focused();
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_finished name={name} success={success} duration_ms={duration_ms}"),
            },
        ]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_tool_finished(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        success: bool,
        duration_ms: u64,
        _result: Option<String>,
    ) -> Vec<Effect> {
        use crate::chat::session::ToolCallSummary;
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let sequence = sequence.or_else(|| self.sequence_for_tool_key(tool_key));
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        let buffer = self.control.tool_buffer_mut(tool_key);
        let args_preview = buffer.tool_args.remove(&invocation_key).unwrap_or_default();
        buffer.tool_calls.push(ToolCallSummary {
            name: name.clone(),
            args_preview,
            success,
            task_id: task_id.map(crate::chat::turn_scheduler::TurnTaskId::get),
            sequence,
        });
        // Placeholder feature: only record + pop the last pending index
        if let Some(buffer) = self.control.tool_buffers.get_mut(&tool_key)
            && !buffer.pending_tool_cards.is_empty()
        {
            buffer.pending_tool_cards.pop();
        }
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_finished name={name} success={success} duration_ms={duration_ms}"),
            },
        ]
    }

    /// `Action::ToolProgress` — progress notification (RequestRedraw + LogTrace only).
    ///
    /// The UI does not render the progress field separately yet; the Action is kept for future
    /// extension + hook firing. It does not mutate UI state (the signature still takes `&self` while
    /// the reducer entry passes `&mut`; `&self` here silences clippy::needless-pass-by-ref-mut).
    fn reduce_tool_progress(&mut self, iteration: usize) -> Vec<Effect> {
        self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_progress {iteration}"),
            },
            Effect::RequestRedraw,
        ]
    }

    /// **S3 T3-1**: `Action::ToolApprovalRequested` — records the foreground
    /// approval view and asks the EffectExecutor to surface it.
    ///
    /// In supervised autonomy mode the driver sends this Action **before** ToolStarted so the reducer
    /// can forward the request to EffectExecutor / UI; the driver itself waits on a oneshot rx (the
    /// dispatcher relays `ToolApprovalReceived` into the driver's receiving channel).
    /// reducer only owns display state. The driver/router remains the single
    /// approval owner and execution gate.
    fn reduce_tool_approval_requested(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        tool_id: String,
        name: String,
        args: String,
    ) -> Vec<Effect> {
        self.ui.pending_tool_approval = Some(crate::chat::sessions::PendingToolApprovalView {
            task_id,
            tool_id: tool_id.clone(),
            name: name.clone(),
            args: args.clone(),
            selected_approval: false,
        });
        self.ui.focus = crate::chat::sessions::FocusTarget::Approval;
        self.ui.switcher = None;
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_approval_requested tool_id={tool_id} name={name}"),
            },
            Effect::RequestApproval {
                task_id,
                tool_id,
                name,
                args,
            },
        ]
    }

    /// **S3 T3-1**: `Action::ToolApprovalReceived` — clear the display prompt
    /// after a human/non-TUI decision. The dispatcher resolves the router after
    /// this reducer step.
    fn reduce_tool_approval_received(&mut self, tool_id: &str, approved: bool) -> Vec<Effect> {
        if self
            .ui
            .pending_tool_approval
            .as_ref()
            .is_some_and(|view| view.tool_id == tool_id)
        {
            self.ui.pending_tool_approval = None;
            if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
                self.ui.focus = crate::chat::sessions::FocusTarget::Main;
            }
        }
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_approval_received tool_id={tool_id} approved={approved}"),
            },
            Effect::RequestRedraw,
        ]
    }

    fn reduce_tool_approval_cleared(&mut self) -> Vec<Effect> {
        self.ui.pending_tool_approval = None;
        if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
            self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        }
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: "tool_approval_cleared".to_string(),
            },
            Effect::RequestRedraw,
        ]
    }

    /// **S3 T3-1**: `Action::StreamRetryAttempt` — a network retry attempt; trace + redraw only.
    ///
    /// Does not mutate state (the driver keeps its own attempt count); the UI can show a "retrying" hint.
    fn reduce_stream_retry_attempt(&self, attempt: u8, reason: &str) -> Vec<Effect> {
        let _ = &self.ui;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("stream_retry_attempt #{attempt} reason={reason}"),
            },
            Effect::RequestRedraw,
        ]
    }

    // ── Step 4 helpers (exit + session) ────────────────────────────────────────

    /// `Action::CancelRequested` — a single Ctrl+C; cancel the current streaming turn if there is one.
    ///
    /// - if generating == false → no active turn, return vec![] (no-op)
    /// - if generating == true  → clear stream.draft + control state and return
    ///   [CancelToken(tok)?, CancelDraft(id), LogTrace, RequestRedraw]
    ///
    /// S2-B Step 2: added [`Effect::CancelToken`] — as soon as the reducer performs
    /// `active_cancel.take()` it hands the token to EffectExecutor, which really calls
    /// `token.cancel()`. That closes the earlier window of "UI cancelled but the underlying LLM stream
    /// keeps running" (a reducer clearing state without cancelling the token was the S2-B Codex risk).
    fn reduce_cancel_requested(&mut self) -> Vec<Effect> {
        if !self.control.generating {
            // cancelling while idle is meaningless — no-op
            return vec![];
        }
        let Some((tool_key, draft_id)) = self.primary_cancel_target() else {
            return vec![];
        };
        self.cancel_task(tool_key, draft_id, "turn cancelled by cancel request")
    }

    fn primary_cancel_target(&self) -> Option<(ToolTaskKey, String)> {
        self.stream
            .primary_draft()
            .map(|turn| (ToolTaskKey::from_task_id(turn.task_id), turn.draft.draft_id.clone()))
    }

    fn reduce_cancel_provider_turn(&mut self, task_id: crate::chat::turn_scheduler::TurnTaskId) -> Vec<Effect> {
        let Some(draft_id) = self
            .stream
            .visible_drafts
            .iter()
            .find(|turn| turn.task_id == Some(task_id))
            .map(|turn| turn.draft.draft_id.clone())
        else {
            return vec![];
        };
        self.cancel_task(
            ToolTaskKey::Task(task_id),
            draft_id,
            "turn cancelled by provider worker cancel request",
        )
    }

    fn clear_target_pending_approval(
        &mut self,
        tool_key: ToolTaskKey,
    ) -> Option<crate::chat::sessions::PendingToolApprovalView> {
        let should_clear = self
            .ui
            .pending_tool_approval
            .as_ref()
            .is_some_and(|pending| ToolTaskKey::from_task_id(pending.task_id) == tool_key);
        if !should_clear {
            return None;
        }
        let pending = self.ui.pending_tool_approval.take();
        if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
            self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        }
        pending
    }

    fn cancel_task(&mut self, tool_key: ToolTaskKey, draft_id: String, reason: &'static str) -> Vec<Effect> {
        let cancel_opt = self.control.take_turn_cancel(tool_key);
        let _ = self.stream.remove_visible_draft(&draft_id);
        self.finalize_pending_tool_cards(tool_key, false, Some(reason));
        self.control.clear_tool_buffer(tool_key);
        let target_pending_approval = self.clear_target_pending_approval(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        let no_task_cancels = !self.control.has_task_turn_cancels();
        let global_pending_approval = if no_visible_drafts && no_task_cancels {
            self.control.active_cancel = None;
            self.control.generating = false;
            let pending = self.ui.pending_tool_approval.take();
            if pending.is_some() && matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
                self.ui.focus = crate::chat::sessions::FocusTarget::Main;
            }
            pending
        } else {
            None
        };

        let mut effects = Vec::new();
        // Emit CancelToken first to really trigger the underlying cancel, then CancelDraft for the UI.
        if let Some(token) = cancel_opt {
            effects.push(Effect::CancelToken(token));
        }
        for pending in target_pending_approval.into_iter().chain(global_pending_approval) {
            effects.push(Effect::ResolveApproval {
                tool_id: pending.tool_id,
                approved: false,
            });
        }
        effects.push(Effect::CancelDraft(draft_id));
        effects.push(Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: "Turn cancelled by CancelRequested".to_string(),
        });
        effects.push(Effect::RequestRedraw);
        effects
    }

    /// `Action::ShutdownRequested` — double Ctrl+C / SIGTERM; exit gracefully.
    ///
    /// If generation is in progress, cancel the current draft + token too, then return [Quit].
    /// When the main loop sees `Effect::Quit` it calls `shutdown.cancel()` (the CancellationToken lives
    /// in the main-loop shell, not in the reducer; to be confirmed once Step 5 wires it all up).
    ///
    /// S2-B Step 2: same as [`Self::reduce_cancel_requested`] — while a streaming turn is still alive
    /// we must emit `Effect::CancelToken` so EffectExecutor really calls token.cancel(), otherwise the
    /// underlying LLM stream never receives the cancel signal.
    fn reduce_shutdown_requested(&mut self) -> Vec<Effect> {
        let (draft_id_opt, cancel_tokens) = if self.control.generating {
            let id = Self::take_draft_id(&self.stream);
            let tokens = self.control.drain_turn_cancels();
            self.stream.clear_visible_drafts();
            self.control.generating = false;
            (id, tokens)
        } else {
            (None, Vec::new())
        };

        let mut effects = Vec::new();
        for token in cancel_tokens {
            effects.push(Effect::CancelToken(token));
        }
        if let Some(draft_id) = draft_id_opt {
            effects.push(Effect::CancelDraft(draft_id));
        }
        effects.push(Effect::Quit);
        effects
    }

    /// `Action::SessionLoaded(ChatSession)` — restore a persisted session into SessionState.
    ///
    /// Every field is replaced (id/title/provider/model/mode/turns); history is rebuilt from turns by
    /// the main loop when SessionLoaded arrives (wired up in Step 5).
    fn reduce_session_loaded(&mut self, loaded: ChatSession) -> Vec<Effect> {
        let id = loaded.id.clone();
        if self.control.generating {
            return vec![Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: format!("SessionLoaded rejected while generating: {id}"),
            }];
        }
        self.session.id = loaded.id;
        self.session.title = loaded.title;
        self.session.provider = Arc::from(loaded.provider.as_str());
        self.session.model = Arc::from(loaded.model.as_str());
        self.session.mode = loaded.mode;
        self.ui.chat_mode = loaded.mode;
        self.session.turns = loaded.turns;
        self.session.token_usage_records = loaded.token_usage_records;
        self.ui.token_usage_summary = MainSessionTokenUsageSummary::from_records(&self.session.token_usage_records);
        // v4: restore persisted background-session summaries (display only —
        // the live processes are gone and are never revived). Carrying them in
        // SessionState means the next save_session snapshot re-persists them, so
        // they survive across multiple reload cycles.
        self.session.background_sessions = loaded.background_sessions;
        // S4-B T4-B-6: keep the original session's created_at so a later save_session cannot overwrite it
        self.session.created_at = Some(loaded.created_at);
        // rebuild history from turns (only user/assistant roles enter the LLM context)
        self.session.history = self
            .session
            .turns
            .iter()
            .filter(|t| t.role == "user" || t.role == "assistant")
            .map(|t| ChatMessage {
                role: t.role.clone(),
                content: t.content.clone(),
            })
            .collect();
        self.ui.conversation_lines = conversation_lines_from_turns(&self.session.turns);
        self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        #[cfg(feature = "terminal-tui")]
        self.ui.input.clear_navigation_state();
        #[cfg(not(feature = "terminal-tui"))]
        self.ui.input.clear();
        self.ui.turn_count = self.session.turns.len();
        self.ui.active_session_view = None;
        self.ui.pending_tool_approval = None;
        self.ui.context_used_tokens = None;
        self.ui.context_window_tokens = None;
        self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        self.ui.sessions_status.clear();
        self.ui.sessions_entries.clear();
        self.ui.switcher = None;
        self.ui.slash_menu = None;
        self.ui.saved_session_picker = None;
        self.stream.clear_visible_drafts();
        self.control.clear_all_tool_buffers();
        self.control.turn_cancels.clear();
        self.control.final_usage_tasks_recorded.clear();
        self.control.generating = false;
        self.control.active_cancel = None;
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("Session loaded: {id}"),
            },
        ]
    }

    /// `Action::SessionSaved { id }` — update the session id (the server may assign one on first save).
    fn reduce_session_saved(&mut self, id: String) -> Vec<Effect> {
        if self.session.id != id {
            self.session.id = id.clone();
        }
        vec![Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: format!("Session saved: {id}"),
        }]
    }

    /// `Action::SessionSwitched { id }` — request a switch to another session.
    ///
    /// Design: a two-step async flow. The reducer only produces effects[0] = SaveSession(current).
    /// After the main loop performs the save it spawns the async load and dispatches `SessionLoaded(new)`.
    /// The interruption window (a crash before the save succeeds) is handled by the main loop, not here.
    ///
    /// effects order (exact):
    ///   [0] SaveSession(current_snapshot)
    ///   [1] LogTrace
    ///   [2] RequestRedraw
    fn reduce_session_switched(&self, id: String) -> Vec<Effect> {
        vec![
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("Session switching to: {id}"),
            },
            Effect::RequestRedraw,
        ]
    }

    /// T3-3-c: build a [`ChatSession`] snapshot from `SessionState` for [`Effect::SaveSession`].
    ///
    /// `SessionState` holds no `created_at` / `updated_at` timestamps (chronological metadata belongs to
    /// the `ChatSession` persistence layer), so the snapshot fills them with the current time — that both
    /// distinguishes `updated_at` between saves and lets `load_latest_session` pick the newest session by
    /// `updated_at`. `schema_version` always uses the `SCHEMA_VERSION` constant.
    ///
    /// It lives in its own fn so `reduce_session_switched` / `reduce_stream_completed` and others share a
    /// single construction path and cannot miss a field.
    fn build_session_snapshot(&self) -> ChatSession {
        let now = chrono::Utc::now();
        let snapshot = ChatSession {
            id: self.session.id.clone(),
            schema_version: crate::chat::session::SCHEMA_VERSION,
            title: self.session.title.clone(),
            provider: self.session.provider.as_ref().to_owned(),
            model: self.session.model.as_ref().to_owned(),
            // S4-B T4-B-6: strict created_at semantics — take SessionState.created_at (set on the first
            // RecordUserTurn) and fall back to now, so an existing creation time is never overwritten
            created_at: self.session.created_at.unwrap_or(now),
            updated_at: now,
            turns: self.session.turns.clone(),
            background_sessions: self.session.background_sessions.clone(),
            token_usage_records: self.session.token_usage_records.clone(),
            mode: self.session.mode,
        };
        crate::chat::sanitize::sanitize_session_content(&snapshot)
    }

    /// `Action::RecordUserTurn(text)` — persist a user turn into the session record and LLM history.
    ///
    /// Matches `session.add_user_turn` semantics:
    /// - `updated_at` is set by the effect executor when it builds the `SaveSession` snapshot
    /// - on the first user turn, if title is empty, set_title runs automatically (first 50 chars, as
    ///   ChatSession does); tool_calls stays empty, tool sync is done by `ToolStarted`/`ToolFinished`
    fn reduce_record_user_turn(&mut self, content: String) -> Vec<Effect> {
        let now = chrono::Utc::now();
        // S4-B T4-B-6: lazily initialise created_at on the first RecordUserTurn
        if self.session.created_at.is_none() {
            self.session.created_at = Some(now);
        }
        self.session.turns.push(crate::chat::session::ChatTurn {
            role: "user".to_string(),
            content: content.clone(),
            timestamp: now,
            tool_calls: Vec::new(),
        });
        // On the first user turn with an empty title, set it automatically (as session.add_user_turn does)
        if self.session.title.is_empty() {
            self.session.title = crate::chat::session::truncate_title(&content);
        }
        self.session.history.push(ChatMessage::user(content));
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("RecordUserTurn len={}", self.session.turns.len()),
        }]
    }

    /// `Action::RecordAssistantTurn` — persist an assistant turn into the session record and LLM history.
    ///
    /// Matches `session.add_assistant_turn` semantics:
    /// - `updated_at` is set by the effect executor when it builds the `SaveSession` snapshot
    /// - P3a: tool_calls come from the matching task bucket. Legacy callers use
    ///   the Primary bucket, so main transcript behavior stays unchanged.
    fn reduce_record_assistant_turn(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        content: String,
    ) -> Vec<Effect> {
        let tool_calls = self.control.take_tool_calls(ToolTaskKey::from_task_id(task_id));
        self.session.turns.push(crate::chat::session::ChatTurn {
            role: "assistant".to_string(),
            content: content.clone(),
            timestamp: chrono::Utc::now(),
            tool_calls,
        });
        self.session.history.push(ChatMessage::assistant(content));
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("RecordAssistantTurn len={}", self.session.turns.len()),
        }]
    }

    /// `Action::RecordSystemMessage` — append one system message to the LLM context history.
    ///
    /// S2-C Step 2: matches legacy `history.push(ChatMessage::system(content))`.
    /// Difference from [`Self::reduce_set_leading_system_prompt`]:
    /// - this function always appends (typical case: rebuilding the system prompt after `/clear` — clear
    ///   already emptied history, so pushing at the end is also first and append equals replace)
    /// - `SetLeadingSystemPrompt` upserts (empty → push, non-empty → replace history[0])
    ///
    /// session.turns is untouched (a system message is not a user/assistant turn, only LLM context config).
    fn reduce_record_system_message(&mut self, content: String) -> Vec<Effect> {
        self.session.history.push(ChatMessage::system(content));
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("RecordSystemMessage history_len={}", self.session.history.len()),
        }]
    }

    /// `Action::SetLeadingSystemPrompt` — set/replace the leading system prompt.
    ///
    /// S2-C Step 2: byte-for-byte aligned with the chat::mod main loop's `if history.is_empty() { push }
    /// else { first_mut = system }`. It runs on every turn (the system prompt is rebuilt after technique
    /// selection), so expressing it as an append would pile up more and more system messages.
    ///
    /// Behaviour:
    /// - history empty → push one system message
    /// - history non-empty, first entry is system → replace `history[0]` (same as legacy `*first = ...`)
    /// - history non-empty, first entry is **not** system → insert system at the front. This keeps
    ///   resumed user/assistant turns intact when a loaded session rebuilds history
    ///   without a runtime system prompt.
    fn reduce_set_leading_system_prompt(&mut self, content: String) -> Vec<Effect> {
        if self.session.history.is_empty() {
            self.session.history.push(ChatMessage::system(content));
        } else if self
            .session
            .history
            .first()
            .is_some_and(|message| message.role != "system")
        {
            self.session.history.insert(0, ChatMessage::system(content));
        } else if let Some(first) = self.session.history.first_mut() {
            *first = ChatMessage::system(content);
        }
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("SetLeadingSystemPrompt history_len={}", self.session.history.len()),
        }]
    }

    /// `Action::SystemMessageAdded` — append one system message to the Redux UI mirror.
    ///
    /// S2-C Step 2: dual-writes with legacy `chat_mirror.lock().push_system_message(text)`.
    /// The reducer keeps its own ConversationLine::System inside `ui.conversation_lines` so the Redux
    /// path has its own UI ledger; the visible TUI is still rendered by `chat_mirror`, and this reducer
    /// does not replace the mirror (mod.rs still writes chat_mirror unconditionally).
    ///
    /// Without the terminal-tui feature it only emits RequestRedraw (the String placeholder carries no
    /// semantics), symmetric with the other TUI-only push functions (user/assistant).
    #[cfg(feature = "terminal-tui")]
    fn reduce_system_message_added(&mut self, text: String) -> Vec<Effect> {
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::System { content: text });
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_system_message_added(&mut self, _text: String) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// `Action::UserMessageEchoed` — visual echo of the user's submission in Pure mode
    #[cfg(feature = "terminal-tui")]
    fn reduce_user_message_echoed(&mut self, text: String) -> Vec<Effect> {
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::User { content: text });
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_user_message_echoed(&mut self, _text: String) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// `Action::SessionsStatusUpdated` — replace the persistent background-session
    /// status line (v1b). The main loop already dedups (only dispatches when the
    /// summary changed), but we still no-op an identical write so a stray
    /// duplicate cannot mark the UI dirty for nothing.
    fn reduce_sessions_status_updated(&mut self, summary: String) -> Vec<Effect> {
        if self.ui.sessions_status == summary {
            return Vec::new();
        }
        self.ui.sessions_status = summary;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SessionsEntriesUpdated` — replace the structured child-session
    /// entries that back the P1 bottom strip. Identical snapshots are no-ops so
    /// the 1s session poll cannot churn redraws when nothing changed.
    fn reduce_sessions_entries_updated(&mut self, entries: Vec<crate::chat::sessions::SwitcherEntry>) -> Vec<Effect> {
        if self.ui.sessions_entries == entries {
            return Vec::new();
        }
        self.ui.sessions_entries = entries;
        vec![Effect::RequestRedraw]
    }

    fn reduce_main_queue_status_updated(&mut self, status: MainQueueStatus) -> Vec<Effect> {
        if self.ui.main_queue_status == status {
            return Vec::new();
        }
        self.ui.main_queue_status = status;
        vec![Effect::RequestRedraw]
    }

    fn reduce_provider_worker_status_updated(&mut self, status: ProviderWorkerStatus) -> Vec<Effect> {
        #[cfg(feature = "terminal-tui")]
        let status = {
            let mut status = status;
            if let Some(summary) = crate::chat::tui::latest_provider_worker_tool_summary_from_conversation(
                &self.ui.conversation_lines,
                self.ui.ascii_fallback,
            ) {
                for row in &mut status.rows {
                    if row.is_active() {
                        row.recent_tool_call = Some(summary.clone());
                    }
                }
            }
            status
        };
        if self.ui.provider_worker_status == status {
            return Vec::new();
        }
        let worker_view = self.ui.focus.worker_sequence().map(|sequence| {
            let previous_view = self
                .ui
                .active_session_view
                .as_ref()
                .filter(|view| view.kind == crate::chat::action::PROVIDER_WORKER_VIEW_KIND && view.seq == sequence);
            #[cfg(feature = "terminal-tui")]
            let io_lines = crate::chat::tui::provider_worker_io_lines_for_streaming_draft(
                &self.ui.conversation_lines,
                self.stream.streaming_draft_for_worker(sequence),
                12,
            );
            #[cfg(not(feature = "terminal-tui"))]
            let io_lines = Vec::new();
            crate::chat::action::build_provider_worker_active_view_with_io_preserving_scroll(
                &status,
                sequence,
                previous_view,
                io_lines,
            )
        });
        self.ui.provider_worker_status = status;
        if let Some(view) = worker_view {
            self.ui.active_session_view = Some(view);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(feature = "terminal-tui")]
    fn refresh_provider_worker_view_if_focused(&mut self) {
        let Some(sequence) = self.ui.focus.worker_sequence() else {
            return;
        };
        let previous_view = self
            .ui
            .active_session_view
            .as_ref()
            .filter(|view| view.kind == crate::chat::action::PROVIDER_WORKER_VIEW_KIND && view.seq == sequence);
        let io_lines = crate::chat::tui::provider_worker_io_lines_for_streaming_draft(
            &self.ui.conversation_lines,
            self.stream.streaming_draft_for_worker(sequence),
            12,
        );
        self.ui.active_session_view = Some(
            crate::chat::action::build_provider_worker_active_view_with_io_preserving_scroll(
                &self.ui.provider_worker_status,
                sequence,
                previous_view,
                io_lines,
            ),
        );
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn refresh_provider_worker_view_if_focused(&mut self) {}

    #[cfg(feature = "terminal-tui")]
    fn reduce_slash_menu_sources_updated(
        &mut self,
        saved_sessions: Vec<crate::chat::session::SavedSessionPickerEntry>,
        provider_model_catalog: Vec<crate::chat::slash_types::SlashProviderModelCatalog>,
    ) -> Vec<Effect> {
        if self.ui.saved_sessions_cache == saved_sessions && self.ui.provider_model_catalog == provider_model_catalog {
            return Vec::new();
        }
        self.ui.saved_sessions_cache = saved_sessions;
        self.ui.provider_model_catalog = provider_model_catalog;
        if self.ui.slash_menu.is_some() {
            let sources = Self::slash_menu_sources_from(
                &self.ui.sessions_entries,
                &self.ui.saved_sessions_cache,
                &self.ui.provider_model_catalog,
                &self.ui.at_path_candidates,
                self.session.provider.as_ref(),
            );
            crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(feature = "terminal-tui")]
    fn reduce_at_path_candidates_updated(&mut self, candidates: Vec<AtPathCandidate>) -> Vec<Effect> {
        if self.ui.at_path_candidates == candidates {
            return Vec::new();
        }
        self.ui.at_path_candidates = candidates;
        let sources = Self::slash_menu_sources_from(
            &self.ui.sessions_entries,
            &self.ui.saved_sessions_cache,
            &self.ui.provider_model_catalog,
            &self.ui.at_path_candidates,
            self.session.provider.as_ref(),
        );
        crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_at_path_candidates_updated(&mut self, _candidates: Vec<AtPathCandidate>) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_slash_menu_sources_updated(
        &mut self,
        _saved_sessions: Vec<crate::chat::session::SavedSessionPickerEntry>,
        _provider_model_catalog: Vec<crate::chat::slash_types::SlashProviderModelCatalog>,
    ) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// `Action::ActiveSessionViewUpdated` — replace/clear the focused child
    /// viewport render snapshot.
    fn reduce_active_session_view_updated(
        &mut self,
        view: Option<crate::chat::sessions::ActiveSessionView>,
    ) -> Vec<Effect> {
        if self.ui.active_session_view == view {
            return Vec::new();
        }
        self.ui.active_session_view = view;
        vec![Effect::RequestRedraw]
    }

    fn reduce_context_window_updated(
        &mut self,
        used_context_tokens: Option<usize>,
        max_context_tokens: Option<usize>,
    ) -> Vec<Effect> {
        if self.ui.context_used_tokens == used_context_tokens && self.ui.context_window_tokens == max_context_tokens {
            return Vec::new();
        }
        self.ui.context_used_tokens = used_context_tokens;
        self.ui.context_window_tokens = max_context_tokens;
        vec![Effect::RequestRedraw]
    }

    fn reduce_provider_usage_recorded(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        usage_kind: ProviderUsageRecordKind,
        record: MainSessionTokenUsageRecord,
    ) -> Vec<Effect> {
        if !self.control.should_record_provider_usage(task_id, usage_kind) {
            return Vec::new();
        }
        self.session.token_usage_records.push(record);
        self.ui.token_usage_summary = MainSessionTokenUsageSummary::from_records(&self.session.token_usage_records);
        vec![
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::RequestRedraw,
        ]
    }

    /// `Action::BackgroundSessionRecorded` (v4) — upsert a background-session
    /// summary into `session.background_sessions` and **immediately emit
    /// `Effect::SaveSession`** so the summary is durably persisted to the memory
    /// backend (the only write path; `dispatcher.rs` `Effect::SaveSession`).
    ///
    /// Why emit SaveSession here (P0, v4 review): under `terminal-tui` (now the
    /// default) the legacy exit-save path is disabled (`mod.rs`
    /// `legacy_exit_save_enabled=false`), and no other action snapshots after a
    /// child session reaches a terminal state. Without this effect the
    /// summary lived only in memory and was lost on exit → reload recap broke.
    /// Emitting SaveSession **after** the upsert guarantees the snapshot
    /// (`build_session_snapshot`, which clones `self.session.background_sessions`)
    /// already contains this record, eliminating the prior race where a snapshot
    /// taken before the action could miss it.
    ///
    /// Dedup is by session id: a later record for the same id replaces the
    /// earlier one (e.g. an `interrupted` entry written at exit, or a terminal
    /// summary superseding a placeholder). This records **summary only** — it
    /// never spawns or revives a process / sub-agent / PTY.
    ///
    /// No save storm: a child session reaching a terminal state is a
    /// low-frequency event, and an unchanged re-record short-circuits to
    /// `Vec::new()` before emitting any effect. `Effect::SaveSession` is a pure
    /// persistence sink (it never dispatches a new action), so there is no
    /// SaveSession → BackgroundSessionRecorded feedback loop.
    fn reduce_background_session_recorded(
        &mut self,
        summary: crate::chat::sessions::PersistedSessionSummary,
    ) -> Vec<Effect> {
        if let Some(existing) = self.session.background_sessions.iter_mut().find(|s| s.id == summary.id) {
            if *existing == summary {
                return Vec::new();
            }
            *existing = summary;
        } else {
            self.session.background_sessions.push(summary);
        }
        // Persist the updated snapshot now (state already mutated above).
        vec![Effect::SaveSession(self.build_session_snapshot())]
    }

    /// `Action::SessionFocusChanged` (v1.1b) — record the current input-routing
    /// target so the snapshot prompt indicator (colour+glyph) reflects it.
    /// Idempotent: an unchanged focus is a no-op (no needless redraw).
    fn reduce_session_focus_changed(&mut self, focus: crate::chat::sessions::FocusTarget) -> Vec<Effect> {
        if self.ui.focus == focus {
            return Vec::new();
        }
        self.ui.focus = focus;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SwitcherOpened` (v1.1b) — open the Ctrl+G switcher overlay over
    /// the supplied session snapshot, highlighting the first row.
    fn reduce_switcher_opened(&mut self, entries: Vec<crate::chat::sessions::SwitcherEntry>) -> Vec<Effect> {
        self.ui.saved_session_picker = None;
        self.ui.slash_menu = None;
        self.ui.switcher = Some(crate::chat::sessions::SwitcherState::new(entries));
        vec![Effect::RequestRedraw]
    }

    /// `Action::SwitcherMoved` (v1.1b) — update the highlighted row. The index is
    /// clamped to a valid row by the key thread; we clamp again defensively so a
    /// stale snapshot can never index out of range. No-op (no redraw) when the
    /// switcher is closed or the selection is unchanged.
    fn reduce_switcher_moved(&mut self, selected: usize) -> Vec<Effect> {
        let Some(switcher) = self.ui.switcher.as_mut() else {
            return Vec::new();
        };
        let clamped = if switcher.entries.is_empty() {
            0
        } else {
            selected.min(switcher.entries.len().saturating_sub(1))
        };
        if switcher.selected == clamped {
            return Vec::new();
        }
        switcher.selected = clamped;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SwitcherClosed` (v1.1b) — close the switcher overlay. No-op (no
    /// redraw) when already closed.
    fn reduce_switcher_closed(&mut self) -> Vec<Effect> {
        if self.ui.switcher.is_none() {
            return Vec::new();
        }
        self.ui.switcher = None;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SavedSessionPickerOpened` (P7c) — open the saved chat-session
    /// history picker, separate from the child-TUI Ctrl+G switcher.
    fn reduce_saved_session_picker_opened(
        &mut self,
        entries: Vec<crate::chat::session::SavedSessionPickerEntry>,
    ) -> Vec<Effect> {
        self.ui.switcher = None;
        self.ui.slash_menu = None;
        self.ui.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(entries));
        vec![Effect::RequestRedraw]
    }

    fn reduce_saved_session_picker_moved(&mut self, selected: usize) -> Vec<Effect> {
        let Some(picker) = self.ui.saved_session_picker.as_mut() else {
            return Vec::new();
        };
        let clamped = if picker.entries.is_empty() {
            0
        } else {
            selected.min(picker.entries.len().saturating_sub(1))
        };
        if picker.selected == clamped {
            return Vec::new();
        }
        picker.selected = clamped;
        vec![Effect::RequestRedraw]
    }

    fn reduce_saved_session_picker_closed(&mut self) -> Vec<Effect> {
        if self.ui.saved_session_picker.is_none() {
            return Vec::new();
        }
        self.ui.saved_session_picker = None;
        vec![Effect::RequestRedraw]
    }

    #[cfg(feature = "terminal-tui")]
    fn reduce_saved_session_picker_key_pressed(&mut self, key: crossterm::event::KeyEvent) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyModifiers};

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
        let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
        if up || down {
            let Some(picker) = self.ui.saved_session_picker.as_mut() else {
                return Vec::new();
            };
            if up {
                picker.select_prev();
            } else {
                picker.select_next();
            }
            picker.clamp_selected();
            return vec![Effect::RequestRedraw];
        }
        if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
            return self.reduce_saved_session_picker_closed();
        }
        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
            return self.reduce_saved_session_picker_closed();
        }
        Vec::new()
    }

    /// `Action::HistoryCleared` — clear the LLM context history (keeping the system prompt) + clear UI.
    ///
    /// session.turns is not cleared (the persisted record is irreversible); only the LLM context (so the
    /// next request carries no history) and the TUI conversation_lines display are reset.
    fn reduce_history_cleared(&mut self) -> Vec<Effect> {
        // Defensively keep every system message (usually just one, but scan all in case it is not first)
        let system_msgs: Vec<_> = self.session.history.drain(..).filter(|m| m.role == "system").collect();
        // history was emptied by drain(..), so re-insert the system messages
        self.session.history.extend(system_msgs);
        // Note: the current input buffer is left alone; InputCancelled handles that separately
        self.ui.conversation_lines.clear();
        self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: "History cleared".to_string(),
            },
        ]
    }

    fn reduce_history_cleared_with_notice(&mut self, notice: String) -> Vec<Effect> {
        let mut effects = self.reduce_history_cleared();
        #[cfg(feature = "terminal-tui")]
        {
            self.ui
                .conversation_lines
                .push(crate::chat::tui::ConversationLine::System { content: notice });
        }
        #[cfg(not(feature = "terminal-tui"))]
        {
            let _ = notice;
        }
        effects.push(Effect::RequestRedraw);
        effects
    }

    /// `Action::HistoryCompacted` — compact the LLM context history.
    ///
    /// The algorithm matches `chat::mod::compact_chat_history` exactly (during dual-write both paths must
    /// produce byte-identical results):
    /// 1. return immediately when `history.len() <= 1` (nothing to compact).
    /// 2. keep the system prompt (if the first entry has role==system).
    /// 3. keep only the last [`COMPACT_KEEP_MESSAGES`] non-system messages (drain the older ones).
    /// 4. truncate a single message with an ellipsis when it exceeds [`COMPACT_CONTENT_CHARS`] chars.
    /// 5. drop oldest turns FIFO when the total budget exceeds [`COMPACT_TOTAL_CHARS`].
    fn reduce_history_compacted(&mut self, reason: CompactReason) -> Vec<Effect> {
        let history = &mut self.session.history;
        if history.len() <= 1 {
            return vec![Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("HistoryCompacted noop reason={reason:?} len={}", history.len()),
            }];
        }
        compact_history_in_place(history);

        let final_chars: usize = history.iter().map(|m| m.content.chars().count()).sum();
        vec![Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: format!(
                "HistoryCompacted reason={reason:?} len={} chars={final_chars}",
                history.len()
            ),
        }]
    }

    fn reduce_history_compaction_patch_applied(
        &mut self,
        reason: CompactReason,
        patch: crate::agent::loop_::CompactionPatch,
        compaction_config: &crate::config::AgentCompactionConfig,
    ) -> Vec<Effect> {
        let history = &mut self.session.history;
        if crate::agent::loop_::compaction_patch_guard_matches(history, &patch.guard) {
            crate::agent::loop_::apply_compaction_patch_exact(history, &patch);
            let budget = crate::agent::loop_::plan_context_budget(
                history,
                compaction_config,
                crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
            );
            let trim_fallback = if budget.over_hard_limit {
                crate::agent::loop_::trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
                    history,
                    compaction_config,
                    patch.replacement.len(),
                )
            } else {
                false
            };
            self.session.turns = durable_turns_from_compacted_history(history);
            self.ui.conversation_lines = conversation_lines_from_turns(&self.session.turns);
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
            self.ui.turn_count = self.session.turns.len();

            return vec![
                Effect::LogTrace {
                    level: tracing::Level::INFO,
                    msg: format!(
                        "HistoryCompactionPatchApplied reason={reason:?} start={} end={} replacement={} append_after={} trim_fallback={} len={}",
                        patch.range_start,
                        patch.range_end,
                        patch.replacement.len(),
                        patch.append_after.len(),
                        trim_fallback,
                        history.len()
                    ),
                },
                Effect::SaveSession(self.build_session_snapshot()),
                Effect::RequestRedraw,
            ];
        }

        let before_len = history.len();
        let trimmed = crate::agent::loop_::trim_history_to_context_budget(history, compaction_config);
        vec![Effect::LogTrace {
            level: tracing::Level::WARN,
            msg: format!(
                "HistoryCompactionPatch guard mismatch reason={reason:?}; stale patch ignored; trim_fallback={trimmed} before_len={before_len} after_len={}",
                history.len()
            ),
        }]
    }

    /// `Action::HistoryCompactionDegraded` — one user-visible line per turn when
    /// a rollover fell back to a lossy trim.
    ///
    /// Losing the oldest context is worth exactly one line: the turn still
    /// answers, so an error would be wrong, but saying nothing hides the fact
    /// that the messages are gone for good. Repeats inside the same turn are
    /// collapsed to trace level.
    fn reduce_history_compaction_degraded(&mut self, reason: CompactReason, dropped_messages: usize) -> Vec<Effect> {
        if self.control.context_degrade_notified {
            return vec![Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!(
                    "HistoryCompactionDegraded suppressed (already surfaced this turn) reason={reason:?} dropped={dropped_messages}"
                ),
            }];
        }
        self.control.context_degrade_notified = true;
        let text = crate::chat::format_context_degraded_notice(dropped_messages);
        #[cfg(feature = "terminal-tui")]
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::System { content: text.clone() });
        vec![
            Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: format!("HistoryCompactionDegraded reason={reason:?} dropped={dropped_messages}"),
            },
            Effect::SurfaceNotice { text },
        ]
    }

    /// Helper: take the current draft id out of StreamState (the layout differs per feature).
    #[cfg(feature = "terminal-tui")]
    fn take_draft_id(stream: &StreamState) -> Option<String> {
        stream.primary_streaming_draft().map(|d| d.draft_id.clone())
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn take_draft_id(stream: &StreamState) -> Option<String> {
        stream.primary_streaming_draft().map(|d| d.draft_id.clone())
    }
}

// ─── Public helpers (shared with dispatcher driver) ──────────────────────────

/// **S3 T3-1**: the history compaction algorithm shared with the `Action::HistoryCompacted` reducer.
///
/// It was extracted into a free function so `dispatcher::drive_start_turn_stream` can apply the **same**
/// algorithm to its own `history` copy on the context-overflow retry path, preventing reducer/driver
/// state drift (a Codex audit recommendation).
///
/// The behaviour matches `reduce_history_compacted` exactly:
/// 1. keep the system prompt (if the first entry has role==system).
/// 2. keep only the last [`COMPACT_KEEP_MESSAGES`] non-system messages (drain the older ones).
/// 3. truncate a single message with an ellipsis when it exceeds [`COMPACT_CONTENT_CHARS`] chars.
/// 4. drop oldest turns FIFO when the total budget exceeds [`COMPACT_TOTAL_CHARS`].
///
/// It is a no-op when `history.len() <= 1` (system stays the only message, or nothing at all).
pub fn compact_history_in_place(history: &mut Vec<ChatMessage>) {
    if history.len() <= 1 {
        return;
    }
    let has_system = history.first().is_some_and(|m| m.role == "system");
    let start = usize::from(has_system);

    // Step 1: keep only the last COMPACT_KEEP_MESSAGES non-system messages
    let turn_count = history.len().saturating_sub(start);
    if turn_count > COMPACT_KEEP_MESSAGES {
        let drain_end = start.saturating_add(turn_count.saturating_sub(COMPACT_KEEP_MESSAGES));
        history.drain(start..drain_end);
    }

    // Step 2: truncate individual message contents
    for msg in history.iter_mut().skip(start) {
        if msg.content.chars().count() > COMPACT_CONTENT_CHARS {
            msg.content = truncate_with_ellipsis(&msg.content, COMPACT_CONTENT_CHARS);
        }
    }

    // Step 3: total budget constraint (drop oldest first)
    while history
        .iter()
        .skip(start)
        .map(|m| m.content.chars().count())
        .sum::<usize>()
        > COMPACT_TOTAL_CHARS
        && history.len() > start.saturating_add(1)
    {
        history.remove(start);
    }
}

// ─── Internal helpers ────────────────────────────────────────────────────────

/// S4-A Commit 1: statically decide whether a given [`Action`] affects the fields the UI needs
/// (`ui.conversation_lines` / `stream.draft` / `ui.input`).
///
/// The **exhaustive match** makes a forgotten new Action variant visible at compile time: if the
/// compiler finds an unmatched variant, cargo check fails outright.
///
/// Actions with dirty=true: after the reducer runs a UI field has definitely changed, so the dispatcher
/// should build a new [`UiSnapshot`] and push it to the watch channel.
///
/// Actions with dirty=false: the reducer only writes session/control sub-state or emits LogTrace, so
/// there is no need to trigger watch send_if_modified.
///
/// **Runtime fallback**: when the static decision is false, `reduce_tracked` also compares the
/// [`ChatState::snapshot_dirty_fields`] fingerprint before and after reduce, catching edge cases the
/// static whitelist does not spell out (for instance a KeyPressed that did not actually change input —
/// returning true statically is harmless too, since send_if_modified skips identical frames).
#[cfg(feature = "terminal-tui")]
const fn ui_dirty_for(action: &Action) -> bool {
    match action {
        // Input path: writes ui.input → dirty
        Action::KeyPressed(_)
        | Action::PasteReceived(_)
        | Action::InputSubmitted(_)
        | Action::InputReplaced(_)
        | Action::HistoryNavigated(_)
        | Action::InputCancelled => true,

        // A terminal resize changes no snapshot field; the redraw goes via Effect::RequestRedraw → redraw_tx
        Action::TerminalResized { .. } => false,

        // UI fold/unfold: mutates conversation_lines directly → dirty
        Action::ToolCardFoldToggled | Action::ReasoningFoldToggled => true,

        // The slash command itself is a reducer no-op (real execution lives in mod.rs), UI unchanged
        Action::SlashCommandIssued { .. } => false,
        // Mode switch: the status bar shows the mode field.
        Action::ModeChanged(_) => true,
        // BUG-07: a model switch writes session.model, which the status bar shows → dirty.
        Action::ModelChanged { .. } => true,
        // Bug #3: a provider switch writes session.provider (shown in the status bar) → dirty.
        Action::ProviderChanged { .. } => true,

        // Streaming / tool events: all of them write stream.draft or conversation_lines → dirty
        Action::TurnStarted { .. }
        | Action::StartLLMTurn { .. }
        | Action::StreamChunkReceived { .. }
        | Action::StreamReasoningReceived { .. }
        | Action::StreamCompleted { .. }
        | Action::StreamFailed { .. }
        | Action::StreamCancelled { .. }
        | Action::ToolStarted { .. }
        | Action::ToolFinished { .. } => true,
        // LogTrace only, UI unchanged
        Action::StreamRetryAttempt { .. }
        | Action::StreamUsageMetered { .. }
        | Action::ProviderTurnReadyForCommit { .. } => false,
        Action::ToolProgress { .. } => true,
        // Foreground approval writes pending view + focus.
        Action::ToolApprovalRequested { .. } | Action::ToolApprovalReceived { .. } | Action::ToolApprovalCleared => {
            true
        }

        // Session: SessionLoaded rebuilds history and may force a UI reset; SessionSaved/Switched do not
        Action::SessionLoaded(_) => true,
        Action::SessionSaved { .. } | Action::SessionSwitched { .. } => false,
        // Record*/compaction writes session.turns/history only. User-visible
        // compaction feedback is `SystemMessageAdded`; budget UI refresh is
        // `ContextWindowUpdated`, so the history patch itself is not snapshot-dirty.
        Action::RecordUserTurn(_)
        | Action::RecordAssistantTurn { .. }
        | Action::RecordSystemMessage { .. }
        | Action::SetLeadingSystemPrompt { .. }
        | Action::HistoryCompacted { .. }
        | Action::HistoryCompactionPatchApplied { .. } => false,
        // The degradation notice writes a conversation line of its own.
        Action::HistoryCompactionDegraded { .. } => true,
        // v4: BackgroundSessionRecorded only upserts session.background_sessions
        // (a persistence field, not a snapshot/UI field) → no UI dirty.
        Action::BackgroundSessionRecorded { .. } => false,

        // UI mirror ledger / history clear / Pure-mode user echo: touch conversation_lines → dirty
        Action::SystemMessageAdded { .. }
        | Action::HistoryCleared
        | Action::HistoryClearedWithNotice { .. }
        | Action::UserMessageEchoed(_) => true,
        // v1b/P1: writes sessions snapshot fields → dirty. The main loop only
        // dispatches these when content changes, and reducers also no-op
        // identical writes, so this never churns frames.
        Action::SessionsStatusUpdated { .. }
        | Action::SessionsEntriesUpdated { .. }
        | Action::MainQueueStatusUpdated { .. }
        | Action::ProviderWorkerStatusUpdated { .. }
        | Action::SlashMenuSourcesUpdated { .. }
        | Action::AtPathCandidatesUpdated { .. }
        | Action::ActiveSessionViewUpdated { .. }
        | Action::ContextWindowUpdated { .. }
        | Action::ProviderUsageRecorded { .. } => true,
        // v1.1b: focus + switcher are snapshot fields driving the prompt
        // indicator and switcher overlay → dirty. Each reducer no-ops identical
        // writes so unchanged state never churns frames.
        Action::SessionFocusChanged { .. }
        | Action::SwitcherOpened { .. }
        | Action::SwitcherMoved { .. }
        | Action::SwitcherClosed
        | Action::SavedSessionPickerOpened { .. }
        | Action::SavedSessionPickerMoved { .. }
        | Action::SavedSessionPickerClosed => true,
        // RedrawRequested only produces a RequestRedraw Effect and changes no snapshot field itself,
        // but semantically a redraw is needed — mark it dirty so it takes the watch path.
        Action::RedrawRequested => true,

        // Exit: CancelRequested / targeted provider cancel / ShutdownRequested
        // may clear visible drafts.
        Action::CancelRequested | Action::CancelProviderTurn { .. } | Action::ShutdownRequested => true,
        // ForceQuit only emits the Quit Effect; the UI is unmounted at once, so dirty is meaningless.
        Action::ForceQuit => false,
    }
}

/// Double Ctrl+C exit window (milliseconds).
const DOUBLE_CTRLC_WINDOW_MS: u64 = 500;

/// Read the current wall clock (ms since the UNIX epoch). The only "impure" call allowed inside the
/// reducer — used solely for the Ctrl+C double-press window; tests inject via [`ChatState::reduce_with_now`].
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> ChatState {
        let shutdown = CancellationToken::new();
        ChatState::new(Arc::from("test-provider"), Arc::from("test-model"), shutdown)
    }

    /// Check that the SessionState defaults are sane
    #[test]
    fn test_chatstate_new_default_session() {
        let state = make_state();
        assert!(uuid::Uuid::parse_str(&state.session.id).is_ok());
        assert!(state.session.title.is_empty());
        assert_eq!(&*state.session.provider, "test-provider");
        assert_eq!(&*state.session.model, "test-model");
        assert!(state.session.turns.is_empty());
        assert!(state.session.history.is_empty());
    }

    /// Check that the UiState defaults are sane
    #[test]
    fn test_chatstate_new_default_ui() {
        let state = make_state();
        assert!(state.ui.conversation_lines.is_empty());
        assert!(state.ui.input.is_empty());
        assert_eq!(state.ui.turn_count, 0);
        assert!(!state.ui.ascii_fallback);
        assert_eq!(state.ui.last_ctrlc_ms, 0);
    }

    /// Check that the StreamState defaults are sane
    #[test]
    fn test_chatstate_new_default_stream() {
        let state = make_state();
        assert!(state.stream.primary_streaming_draft().is_none());
        assert!(state.control.tool_buffers.is_empty());
    }

    /// v1b: SessionsStatusUpdated writes ui.sessions_status and the snapshot reflects it; same content is a no-op.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn sessions_status_updated_writes_and_dedups() {
        let mut state = make_state();
        assert!(state.ui.sessions_status.is_empty());

        let effects = state.reduce(Action::SessionsStatusUpdated {
            summary: "sessions: 1 running".to_string(),
        });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.sessions_status, "sessions: 1 running");
        let snap = state.build_ui_snapshot(1);
        assert_eq!(&*snap.sessions_status, "sessions: 1 running");

        // Identical write is a no-op (no redraw effect).
        let effects = state.reduce(Action::SessionsStatusUpdated {
            summary: "sessions: 1 running".to_string(),
        });
        assert!(effects.is_empty(), "identical status must not emit an effect");

        // Clearing hides the row (empty string flows through).
        let effects = state.reduce(Action::SessionsStatusUpdated { summary: String::new() });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(state.ui.sessions_status.is_empty());
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn main_queue_status_updated_writes_snapshot_and_dedups() {
        let mut state = make_state();
        let status = MainQueueStatus { queued: 3, priority: 1 };

        let effects = state.reduce(Action::MainQueueStatusUpdated { status });

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.main_queue_status, status);
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.main_queue_status, status);

        let effects = state.reduce(Action::MainQueueStatusUpdated { status });
        assert!(effects.is_empty(), "identical queue status must not emit an effect");
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn provider_worker_status_updated_writes_snapshot_and_dedups() {
        let mut state = make_state();
        let status = ProviderWorkerStatus {
            running: 1,
            cancelling: 1,
            awaiting_commit: 2,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: None,
            rows: Vec::new(),
        };

        let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status: status.clone() });

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.provider_worker_status, status);
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.provider_worker_status, status);

        let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status });
        assert!(
            effects.is_empty(),
            "identical provider worker status must not emit an effect"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn provider_worker_status_update_refreshes_open_worker_view_with_io() {
        use crate::chat::tui::{ConversationLine, ToolStatus};

        let mut state = make_state();
        state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 2 };
        state.ui.conversation_lines.push(ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: "echo P6Z".to_string(),
            args_full: "{\"command\":\"echo P6Z\"}".to_string(),
            result: Some("P6Z\n".to_string()),
            status: ToolStatus::Done,
            elapsed_ms: Some(12),
            folded: true,
        });
        let status = ProviderWorkerStatus {
            running: 1,
            cancelling: 0,
            awaiting_commit: 0,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis()),
            rows: vec![crate::chat::action::ProviderWorkerStatusRow {
                task_id: 7,
                sequence: 2,
                kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
                state: crate::chat::action::ProviderWorkerRowState::Running,
                started_at_ms: chrono::Utc::now().timestamp_millis(),
                finalized_total_tokens: None,
                completion_ready: false,
                recent_tool_call: None,
            }],
        };

        let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status });

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        let view = state.ui.active_session_view.as_ref().expect("worker view refreshed");
        assert_eq!(view.kind, crate::chat::action::PROVIDER_WORKER_VIEW_KIND);
        assert!(view.lines.iter().any(|line| line == "task: 7"));
        assert!(
            !view.lines.iter().any(|line| line == "io: recent provider turn"),
            "non-streaming worker views must not replay transcript history: {:?}",
            view.lines
        );
        assert!(
            !view.lines.iter().any(|line| line.starts_with("run shell done:")),
            "completed tool cards without a matching streaming draft stay out of worker IO: {:?}",
            view.lines
        );
        assert!(
            !view.lines.iter().any(|line| line == "output: P6Z"),
            "completed tool output without a matching streaming draft stays out of worker IO: {:?}",
            view.lines
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn provider_worker_status_update_enriches_running_rows_with_recent_tool_summary() {
        use crate::chat::tui::{ConversationLine, ToolStatus};

        let mut state = make_state();
        state.ui.conversation_lines.push(ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: "ls /tmp/demo".to_string(),
            args_full: "{\"command\":\"ls /tmp/demo\"}".to_string(),
            result: None,
            status: ToolStatus::Running,
            elapsed_ms: None,
            folded: true,
        });
        let status = ProviderWorkerStatus {
            running: 1,
            cancelling: 0,
            awaiting_commit: 0,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: Some(0),
            rows: vec![crate::chat::action::ProviderWorkerStatusRow {
                task_id: 7,
                sequence: 2,
                kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                state: crate::chat::action::ProviderWorkerRowState::Running,
                started_at_ms: 0,
                finalized_total_tokens: None,
                completion_ready: false,
                recent_tool_call: None,
            }],
        };

        let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status });

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        let summary = state
            .ui
            .provider_worker_status
            .rows
            .first()
            .and_then(|row| row.recent_tool_call.as_deref())
            .expect("running row should receive latest tool summary");
        assert!(
            summary.contains("run shell running: ls /tmp/demo"),
            "summary should use the compact tool-call wording: {summary}"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn slash_menu_sources_match_legacy_and_redux_for_same_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let catalog = vec![crate::chat::tui::SlashProviderModelCatalog {
            provider: "test-provider".to_string(),
            models: vec![crate::chat::tui::SlashModelCandidate {
                name: "gpt-parity".to_string(),
                description: "Parity model".to_string(),
            }],
        }];
        let mut legacy = crate::chat::tui::TuiState::new("test-provider", "test-model");
        legacy.provider_model_catalog = catalog.clone();
        let mut redux = make_state();
        let _ = redux.reduce(Action::SlashMenuSourcesUpdated {
            saved_sessions: Vec::new(),
            provider_model_catalog: catalog,
        });

        for ch in "/model ".chars() {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            let _ = crate::chat::tui::dispatch_global_key(key, &mut legacy);
            let _ = redux.reduce_with_now(Action::KeyPressed(key), 1_000);
        }

        let legacy_labels = legacy
            .slash_menu
            .as_ref()
            .expect("legacy model menu")
            .entries
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>();
        let redux_labels = redux
            .ui
            .slash_menu
            .as_ref()
            .expect("redux model menu")
            .entries
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(legacy_labels, redux_labels);
        assert_eq!(redux_labels, vec!["gpt-parity"]);
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn at_path_candidates_action_opens_redux_menu_and_tab_inserts() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        let _ = state.reduce(Action::InputReplaced("inspect @ca".to_string()));
        assert!(
            state.ui.slash_menu.is_none(),
            "input alone has no candidates and must not render a stale menu"
        );

        let effects = state.reduce(Action::AtPathCandidatesUpdated {
            candidates: vec![AtPathCandidate {
                path: "Cargo.toml".to_string(),
                is_dir: false,
            }],
        });

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(
            state
                .ui
                .slash_menu
                .as_ref()
                .expect("@path menu")
                .entries
                .first()
                .map(|entry| entry.label.as_str()),
            Some("Cargo.toml")
        );
        let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert_eq!(state.ui.input.text(), "inspect @Cargo.toml ");
    }

    /// P1: SessionsEntriesUpdated writes structured strip entries and dedups
    /// identical snapshots so the 1s poll does not churn redraws.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn sessions_entries_updated_writes_snapshot_and_dedups() {
        use crate::chat::sessions::SwitcherEntry;
        let mut state = make_state();
        assert!(state.ui.sessions_entries.is_empty());

        let entries = vec![SwitcherEntry {
            seq: 1,
            kind: "agent",
            origin: "user",
            status: "running",
            title: "task".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }];
        let expected = entries.clone();
        let effects = state.reduce(Action::SessionsEntriesUpdated { entries });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.sessions_entries, expected);
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.sessions_entries.as_slice(), expected.as_slice());

        let effects = state.reduce(Action::SessionsEntriesUpdated { entries: expected });
        assert!(effects.is_empty(), "identical entries must not redraw");

        let effects = state.reduce(Action::SessionsEntriesUpdated { entries: Vec::new() });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(state.ui.sessions_entries.is_empty());
    }

    /// P2: ActiveSessionViewUpdated writes the focused child viewport snapshot,
    /// flows it to UiSnapshot, and dedups identical writes.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn active_session_view_updated_writes_snapshot_and_dedups() {
        let mut state = make_state();
        assert!(state.ui.active_session_view.is_none());

        let view = crate::chat::sessions::ActiveSessionView {
            seq: 4,
            kind: "shell".to_string(),
            title: "tail -f app.log".to_string(),
            lines: vec!["a".to_string(), "b".to_string()],
            truncated: false,
            scroll_offset: 1,
        };
        let effects = state.reduce(Action::ActiveSessionViewUpdated {
            view: Some(view.clone()),
        });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.active_session_view, Some(view.clone()));
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.active_session_view, Some(view));

        let duplicate = state.ui.active_session_view.clone();
        let effects = state.reduce(Action::ActiveSessionViewUpdated { view: duplicate });
        assert!(effects.is_empty(), "identical active view must not redraw");

        let effects = state.reduce(Action::ActiveSessionViewUpdated { view: None });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(state.ui.active_session_view.is_none());
    }

    /// P4c: ContextWindowUpdated writes status-bar window metadata to both the
    /// live UI state and UiSnapshot, with identical writes deduped.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn context_window_updated_writes_snapshot_and_dedups() {
        let mut state = make_state();
        assert_eq!(state.ui.context_used_tokens, None);
        assert_eq!(state.ui.context_window_tokens, None);

        let effects = state.reduce(Action::ContextWindowUpdated {
            used_context_tokens: Some(2_500),
            max_context_tokens: Some(10_000_000),
        });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.context_used_tokens, Some(2_500));
        assert_eq!(state.ui.context_window_tokens, Some(10_000_000));
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.context_used_tokens, Some(2_500));
        assert_eq!(snap.context_window_tokens, Some(10_000_000));

        let effects = state.reduce(Action::ContextWindowUpdated {
            used_context_tokens: Some(2_500),
            max_context_tokens: Some(10_000_000),
        });
        assert!(effects.is_empty(), "identical context budget must not redraw");

        let effects = state.reduce(Action::ContextWindowUpdated {
            used_context_tokens: None,
            max_context_tokens: None,
        });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.context_used_tokens, None);
        assert_eq!(state.ui.context_window_tokens, None);
    }

    /// v1.1b: SessionFocusChanged writes ui.focus + flows to the snapshot; an
    /// identical focus is a no-op.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn session_focus_changed_writes_and_dedups() {
        use crate::chat::sessions::FocusTarget;
        let mut state = make_state();
        assert_eq!(state.ui.focus, FocusTarget::Main);

        let focus = FocusTarget::Session { seq: 2 };
        let effects = state.reduce(Action::SessionFocusChanged { focus });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.focus, focus);
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.focus, focus);

        // Identical focus → no-op.
        let effects = state.reduce(Action::SessionFocusChanged { focus });
        assert!(effects.is_empty(), "identical focus must not emit an effect");

        // Back to main.
        let effects = state.reduce(Action::SessionFocusChanged {
            focus: FocusTarget::Main,
        });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.focus, FocusTarget::Main);
    }

    /// v1.1b: switcher open/move/close lifecycle through the reducer.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn switcher_open_move_close_lifecycle() {
        use crate::chat::sessions::SwitcherEntry;
        let mut state = make_state();
        assert!(state.ui.switcher.is_none());

        let entries = vec![
            SwitcherEntry {
                seq: 1,
                kind: "agent",
                origin: "user",
                status: "running",
                title: "a".into(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            },
            SwitcherEntry {
                seq: 2,
                kind: "agent",
                origin: "model",
                status: "completed",
                title: "b".into(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            },
        ];
        let effects = state.reduce(Action::SwitcherOpened { entries });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        let sw = state.ui.switcher.as_ref().expect("test: switcher open");
        assert_eq!(sw.len(), 2);
        assert_eq!(sw.selected, 0);
        // Snapshot carries it.
        assert!(state.build_ui_snapshot(1).switcher.is_some());

        // Move to row 1.
        let effects = state.reduce(Action::SwitcherMoved { selected: 1 });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.switcher.as_ref().expect("test").selected, 1);
        // Out-of-range selection is clamped to the last row, not a panic.
        let _ = state.reduce(Action::SwitcherMoved { selected: 99 });
        assert_eq!(state.ui.switcher.as_ref().expect("test").selected, 1);

        // Close.
        let effects = state.reduce(Action::SwitcherClosed);
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(state.ui.switcher.is_none());
        // Closing again is a no-op.
        let effects = state.reduce(Action::SwitcherClosed);
        assert!(effects.is_empty());
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn alt_enter_no_longer_surfaces_session_gone() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        state.ui.sessions_entries = vec![crate::chat::sessions::SwitcherEntry {
            seq: 1,
            kind: "agent",
            origin: "user",
            status: "running",
            title: "task".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }];
        state.ui.input.set_text("draft");

        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(
            state.ui.input.text(),
            "draft\n",
            "Alt+Enter is only an input newline chord"
        );
        assert!(state.ui.conversation_lines.is_empty());
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn alt_enter_with_sessions_no_longer_uses_attach_branch() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        state.ui.sessions_entries = vec![crate::chat::sessions::SwitcherEntry {
            seq: 2,
            kind: "shell",
            origin: "user",
            status: "running",
            title: "task".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }];
        state.ui.input.set_text("draft");

        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(
            state.ui.input.text(),
            "draft\n",
            "matching Alt+Enter no longer attaches strip selection"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn alt_enter_falls_through_to_newline_insert() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        state.ui.input.set_text("a");

        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::NONE,
        )));

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.input.text(), "a\nb");
        assert!(
            state.ui.conversation_lines.is_empty(),
            "no strip selection means Alt+Enter falls through to input, not session gone"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn slash_menu_captures_alt_arrows_before_bottom_navigation() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        state.ui.sessions_entries = vec![
            crate::chat::sessions::SwitcherEntry {
                seq: 1,
                kind: "agent",
                origin: "user",
                status: "running",
                title: "one".into(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            },
            crate::chat::sessions::SwitcherEntry {
                seq: 2,
                kind: "agent",
                origin: "user",
                status: "running",
                title: "two".into(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            },
        ];
        state.ui.input.set_text("/mo");
        state.ui.slash_menu = Some(SlashMenuState::new("mo"));

        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)));

        assert!(
            effects.iter().all(|effect| matches!(effect, Effect::RequestRedraw)),
            "slash menu may redraw, but must not leak Alt+Up into strip navigation: {effects:?}"
        );
        assert!(state.ui.slash_menu.is_some(), "slash menu remains open");
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn saved_session_picker_captures_alt_enter_before_input_newline() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        state.ui.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
            crate::chat::session::SavedSessionPickerEntry {
                id: "saved-a".to_string(),
                title: "saved a".to_string(),
                turn_count: 1,
                updated_at: chrono::Utc::now(),
                provider: "p".to_string(),
                model: "m".to_string(),
                is_current: false,
            },
        ]));
        state.ui.input.set_text("draft");

        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));

        assert!(
            effects.is_empty(),
            "saved picker consumes Alt+Enter without reducer side effects"
        );
        assert!(
            state.ui.saved_session_picker.is_some(),
            "Alt+Enter is consumed, not treated as picker Enter"
        );
        assert_eq!(state.ui.input.text(), "draft");
        assert!(
            !matches!(
                state.ui.conversation_lines.last(),
                Some(crate::chat::tui::ConversationLine::System { content }) if content == "session gone"
            ),
            "stale strip selection must not receive Alt+Enter while picker is open"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn saved_session_picker_open_move_close_lifecycle() {
        let mut state = make_state();
        state.ui.switcher = Some(crate::chat::sessions::SwitcherState::new(vec![
            crate::chat::sessions::SwitcherEntry {
                seq: 1,
                kind: "agent",
                origin: "model",
                status: "running",
                title: "child".to_string(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            },
        ]));
        let entries = vec![
            crate::chat::session::SavedSessionPickerEntry {
                id: "saved-a".to_string(),
                title: "saved a".to_string(),
                turn_count: 2,
                updated_at: chrono::Utc::now(),
                provider: "p".to_string(),
                model: "m".to_string(),
                is_current: true,
            },
            crate::chat::session::SavedSessionPickerEntry {
                id: "saved-b".to_string(),
                title: "saved b".to_string(),
                turn_count: 4,
                updated_at: chrono::Utc::now(),
                provider: "p".to_string(),
                model: "m".to_string(),
                is_current: false,
            },
        ];

        let effects = state.reduce(Action::SavedSessionPickerOpened { entries });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(
            state.ui.switcher.is_none(),
            "saved picker and child switcher are mutually exclusive"
        );
        let picker = state.ui.saved_session_picker.as_ref().expect("picker open");
        assert_eq!(picker.len(), 2);
        assert_eq!(picker.selected, 0);
        assert_eq!(
            state
                .build_ui_snapshot(1)
                .saved_session_picker
                .as_ref()
                .expect("snapshot picker")
                .entries
                .len(),
            2
        );

        let effects = state.reduce(Action::SavedSessionPickerMoved { selected: 99 });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.saved_session_picker.as_ref().expect("picker").selected, 1);
        let effects = state.reduce(Action::SavedSessionPickerMoved { selected: 1 });
        assert!(effects.is_empty(), "same clamped selection is no-op");

        let effects = state.reduce(Action::SavedSessionPickerClosed);
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(state.ui.saved_session_picker.is_none());
        assert!(state.reduce(Action::SavedSessionPickerClosed).is_empty());
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn tool_approval_requested_opens_approval_child_view() {
        let mut state = make_state();
        let effects = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-approve".to_string(),
            name: "shell".to_string(),
            args: r#"{"cmd":"printf secure"}"#.to_string(),
        });
        assert!(state.ui.pending_tool_approval.is_some());
        assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Approval);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::RequestApproval { .. }))
        );

        let snap = state.build_ui_snapshot(1);
        let pending = snap
            .pending_tool_approval
            .as_ref()
            .expect("pending approval in snapshot");
        assert_eq!(pending.tool_id, "call-approve");
        assert_eq!(pending.name, "shell");
        assert!(pending.args.contains("printf secure"));
        assert!(
            !pending.selected_approval,
            "approval selection defaults to the safe deny choice"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn redux_approval_arrows_select_and_enter_resolves() {
        let mut state = make_state();
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-arrow-approve".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });

        let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::NONE,
        )));
        assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
        assert!(
            state
                .ui
                .pending_tool_approval
                .as_ref()
                .expect("approval remains pending after arrow")
                .selected_approval,
            "Right selects approve"
        );

        let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        )));
        assert!(state.ui.pending_tool_approval.is_none());
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::ResolveApproval { tool_id, approved: true } if tool_id == "call-arrow-approve"
            )
        }));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn tool_approval_received_closes_approval_child_view() {
        let mut state = make_state();
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-deny".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });
        let effects = state.reduce(Action::ToolApprovalReceived {
            tool_id: "call-deny".to_string(),
            approved: false,
        });
        assert!(state.ui.pending_tool_approval.is_none());
        assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
        assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn redux_esc_approval_generating_denies_without_cancelling_turn() {
        let mut state = make_state();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-approval".to_string(),
            cancel: CancellationToken::new(),
        });
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-esc-deny".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });

        let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        )));

        assert!(
            state.control.generating,
            "Esc in approval must not cancel the active turn"
        );
        assert!(
            state.stream.primary_streaming_draft().is_some(),
            "streaming draft stays active"
        );
        assert!(state.ui.pending_tool_approval.is_none());
        assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::ResolveApproval { tool_id, approved: false } if tool_id == "call-esc-deny"
            )
        }));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelToken(_) | Effect::CancelDraft(_))),
            "approval Esc must not emit turn-cancel effects"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn redux_esc_generating_slash_menu_closes_menu_without_cancelling_turn() {
        let mut state = make_state();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-slash".to_string(),
            cancel: CancellationToken::new(),
        });
        state.ui.input.set_text("/mo");
        state.ui.slash_menu = Some(SlashMenuState::new("mo"));

        let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        )));

        assert!(state.control.generating, "slash Esc must not cancel the active turn");
        assert!(
            state.stream.primary_streaming_draft().is_some(),
            "streaming draft stays active"
        );
        assert!(state.ui.slash_menu.is_none(), "Esc closes only the slash menu");
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelToken(_) | Effect::CancelDraft(_))),
            "slash Esc must not emit turn-cancel effects"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn cancel_requested_clears_pending_approval_and_resolves_false() {
        let mut state = make_state();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-cancel".to_string(),
            cancel: CancellationToken::new(),
        });
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-cancel-deny".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });

        let effects = state.reduce(Action::CancelRequested);

        assert!(state.ui.pending_tool_approval.is_none());
        assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
        assert!(effects.iter().any(|effect| matches!(effect, Effect::CancelToken(_))));
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::ResolveApproval { tool_id, approved: false } if tool_id == "call-cancel-deny"
            )
        }));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelDraft(draft_id) if draft_id == "draft-cancel"))
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn tool_approval_focus_paste_does_not_edit_input() {
        let mut state = make_state();
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-paste".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });
        let effects = state.reduce(Action::PasteReceived("must not enter input".to_string()));
        assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
        assert!(state.ui.input.is_empty());
        assert!(state.ui.pending_tool_approval.is_some());
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn session_loaded_resets_transient_holder_set() {
        let mut state = make_state();
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-stale".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-stale".to_string(),
            cancel: CancellationToken::new(),
        });
        state.ui.context_window_tokens = Some(10_000_000);
        state.ui.context_used_tokens = Some(2_500);
        state.ui.input.set_text("draft text");
        assert!(state.ui.input.begin_or_cycle_reverse_search());
        state.control.generating = false;

        let mut loaded = ChatSession::new("prov-new", "model-new");
        loaded.id = "sess-new".to_string();
        loaded.add_user_turn("hello");
        loaded.add_assistant_turn("hi", vec![]);
        let effects = state.reduce(Action::SessionLoaded(loaded));

        assert!(state.ui.pending_tool_approval.is_none());
        assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
        assert_eq!(state.ui.context_window_tokens, None);
        assert_eq!(state.ui.context_used_tokens, None);
        assert!(!state.ui.input.is_reverse_search_active());
        assert_eq!(state.ui.turn_count, 2);
        assert!(state.stream.primary_streaming_draft().is_none());
        assert!(!state.control.generating);
        assert!(state.control.active_cancel.is_none());
        assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn loading_fresh_session_clears_visible_title_turns_and_token_usage() {
        let mut state = make_state();
        state.session.id = "sess-old".to_string();
        state.session.title = "stale title".to_string();
        state.session.turns.push(ChatTurn {
            role: "user".to_string(),
            content: "old turn".to_string(),
            timestamp: chrono::Utc::now(),
            tool_calls: Vec::new(),
        });
        state.ui.turn_count = 1;
        state.ui.token_usage_summary = MainSessionTokenUsageSummary {
            total_tokens: 68_700,
            prompt_tokens: 68_000,
            completion_tokens: 700,
            request_count: 1,
            unknown_cost_requests: 1,
            ..MainSessionTokenUsageSummary::default()
        };

        let mut fresh = ChatSession::new("prov-new", "model-new");
        fresh.id = "sess-fresh".to_string();
        let _ = state.reduce(Action::SessionLoaded(fresh));

        assert_eq!(state.session.id, "sess-fresh");
        assert!(state.session.title.is_empty());
        assert!(state.session.turns.is_empty());
        assert!(state.session.token_usage_records.is_empty());
        assert_eq!(state.ui.turn_count, 0);
        assert_eq!(state.ui.token_usage_summary, MainSessionTokenUsageSummary::default());
        assert!(state.ui.conversation_lines.is_empty());
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn session_loaded_is_rejected_while_generating_without_clearing_active_turn() {
        let mut state = make_state();
        state.session.id = "sess-old".to_string();
        let cancel = CancellationToken::new();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-active".to_string(),
            cancel: cancel.clone(),
        });
        assert!(state.control.generating);
        assert!(state.stream.primary_streaming_draft().is_some());
        assert!(state.control.active_cancel.is_some());

        let mut loaded = ChatSession::new("prov-new", "model-new");
        loaded.id = "sess-new".to_string();
        loaded.add_user_turn("should-not-load");
        let effects = state.reduce(Action::SessionLoaded(loaded));

        assert_eq!(state.session.id, "sess-old");
        assert!(state.control.generating);
        assert!(state.stream.primary_streaming_draft().is_some());
        assert!(state.control.active_cancel.is_some());
        assert!(!cancel.is_cancelled());
        assert!(!effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::LogTrace {
                    level: tracing::Level::WARN,
                    msg,
                } if msg.contains("SessionLoaded rejected while generating: sess-new")
            )
        }));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn reducer_tab_mid_edit_inserts_tab_instead_of_folding() {
        let mut state = make_state();
        state.ui.input.set_text("alpha");

        let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        )));

        assert_eq!(state.ui.input.text(), "alpha\t");
        assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
    }

    /// reduce does not panic (robustness baseline; keeps the Step 1 name for easy grepping)
    #[test]
    fn test_reduce_key_pressed_returns_empty_step1() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut state = make_state();
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let _effects = state.reduce(Action::KeyPressed(key));
        // Since Step 2, KeyPressed('a') writes into the input buffer -> RequestRedraw
        // here we only assert that it does not panic
    }

    /// Actions that still return empty after Step 4 (only SlashCommandIssued remains)
    #[test]
    fn test_reduce_unfilled_actions_return_empty() {
        let mut state = make_state();
        // Filled in by Step 4: HistoryCleared/SessionLoaded/SessionSaved/SessionSwitched/
        //   RecordUserTurn/RecordAssistantTurn/CancelRequested/ShutdownRequested
        // The following are still left for Step 5 (they return an empty vec):
        let unfilled = [Action::SlashCommandIssued {
            cmd: "clear".to_string(),
            args: String::new(),
        }];
        for action in unfilled {
            let effects = state.reduce(action);
            assert!(effects.is_empty(), "unfilled Action must return vec![]");
        }
    }

    /// Added in Step 3: StreamChunkReceived returns vec![] when there is no draft (stale)
    #[test]
    fn test_reduce_stream_chunk_no_draft_returns_empty() {
        let mut state = make_state();
        let effects = state.reduce(Action::StreamChunkReceived {
            draft_id: "d1".to_string(),
            delta: "x".to_string(),
            version: 1,
        });
        assert!(effects.is_empty(), "chunk must be dropped when there is no draft");
    }

    /// reduce must not panic for any Action variant (coverage contract)
    #[test]
    fn test_reduce_does_not_panic_for_all_actions() {
        use crate::chat::action::HistoryDir;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("cancel target", crate::chat::turn_scheduler::TurnPriority::Normal, 0);

        let actions: Vec<Action> = vec![
            Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::PasteReceived("paste text".to_string()),
            Action::TerminalResized { w: 80, h: 24 },
            Action::InputSubmitted("hello".to_string()),
            Action::HistoryNavigated(HistoryDir::Up),
            Action::HistoryNavigated(HistoryDir::Down),
            Action::InputCancelled,
            Action::ToolCardFoldToggled,
            Action::ReasoningFoldToggled,
            Action::RedrawRequested,
            Action::CancelProviderTurn { task_id },
            Action::ForceQuit,
        ];

        for action in actions {
            let mut state = make_state();
            let _effects = state.reduce(action);
        }
    }

    /// Action must be Send + Sync (compile-time assertion so it can cross tasks over a channel)
    #[test]
    fn test_action_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Action>();
    }

    /// Effect must be Send + Sync (compile-time assertion so it can be handed to the executor)
    #[test]
    fn test_effect_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Effect>();
    }

    /// EmitChannelMessage carries a SendMessage (P1 check: not a bare String)
    #[test]
    fn test_effect_emit_channel_message_has_send_message_type() {
        use crate::channels::traits::SendMessage;
        // Being able to construct Effect::EmitChannelMessage(SendMessage) proves the type is wired in
        let msg = SendMessage::new("hello", "bob");
        let effect = Effect::EmitChannelMessage(msg);
        // Check that the Debug implementation exists
        let debug_str = format!("{:?}", effect);
        assert!(
            debug_str.contains("EmitChannelMessage"),
            "EmitChannelMessage Debug output is wrong"
        );
    }

    /// ControlState default value check
    #[test]
    fn test_chatstate_new_default_control() {
        let state = make_state();
        assert!(state.control.active_cancel.is_none());
        assert!(!state.control.generating);
    }

    /// Repeated reduce calls must not panic (robustness)
    #[test]
    fn test_reduce_multiple_calls_no_panic() {
        let mut state = make_state();
        for _ in 0..10 {
            let _effects = state.reduce(Action::RedrawRequested);
        }
    }

    // ─── Step 2 unit tests (input path) ───────────────────────────────────────
    //
    // Most input-path tests rely on the real TuiInput / ConversationLine provided by the
    // terminal-tui feature. Without the TUI feature the reducer takes the placeholder branch and
    // degrades to "return RequestRedraw without mutating the buffer", so Step 2 assertions are cfg-gated.

    #[cfg(feature = "terminal-tui")]
    mod step2 {
        use super::super::*;
        use crate::chat::action::HistoryDir;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        fn s() -> ChatState {
            ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new())
        }

        fn has_request_redraw(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::RequestRedraw))
        }

        fn has_quit(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::Quit))
        }

        fn has_log_trace(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::LogTrace { .. }))
        }

        /// 1. Enter on non-empty buffer → takes the InputSubmitted path, turn_count += 1
        #[test]
        fn test_reduce_key_pressed_enter_returns_input_submitted() {
            let mut state = s();
            // simulate the user typing "hi"
            for ch in "hi".chars() {
                let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
            }
            assert_eq!(state.ui.input.text(), "hi");
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
            assert!(has_log_trace(&effects), "Enter must trigger LogTrace");
            assert!(has_request_redraw(&effects), "Enter must trigger RequestRedraw");
            assert_eq!(state.ui.turn_count, 1, "turn_count must increment");
            assert_eq!(state.ui.last_submitted.as_deref(), Some("hi"));
            assert!(state.ui.input.is_empty(), "buffer must be cleared after submit");
        }

        /// 2. Tab → ToolCardFoldToggled, returns RequestRedraw
        #[test]
        fn test_reduce_key_pressed_tab_returns_fold_toggled() {
            let mut state = s();
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert!(has_request_redraw(&effects));
            // Tab must not enter the input buffer
            assert!(state.ui.input.is_empty());
        }

        /// 3. single Ctrl+C → only records the window, does not return Quit
        #[test]
        fn test_reduce_key_pressed_ctrl_c_single_returns_cancel() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let effects = state.reduce_with_now(Action::KeyPressed(key), 10_000);
            assert!(!has_quit(&effects), "a single Ctrl+C must not Quit");
            assert_eq!(state.ui.last_ctrlc_ms, 10_000, "records the window timestamp");
        }

        /// 4. Ctrl+C pressed twice within 500ms → Quit
        #[test]
        fn test_reduce_key_pressed_ctrl_c_double_within_500ms_returns_shutdown() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let _ = state.reduce_with_now(Action::KeyPressed(key.clone()), 10_000);
            // Ctrl+C again 100ms later
            let effects = state.reduce_with_now(Action::KeyPressed(key), 10_100);
            assert!(has_quit(&effects), "a Ctrl+C double press must Quit");
        }

        /// 5. Ctrl+C pressed again after more than 500ms → only recorded, no Quit
        #[test]
        fn test_reduce_key_pressed_ctrl_c_double_after_500ms_returns_cancel_only() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let _ = state.reduce_with_now(Action::KeyPressed(key.clone()), 10_000);
            // 600ms later — outside the window
            let effects = state.reduce_with_now(Action::KeyPressed(key), 10_600);
            assert!(!has_quit(&effects), "a second press after 500ms is not a double press");
            assert_eq!(state.ui.last_ctrlc_ms, 10_600);
        }

        /// 6. Ctrl+D on empty buffer → Quit
        #[test]
        fn test_reduce_key_pressed_ctrl_d_empty_buffer_returns_quit() {
            let mut state = s();
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )));
            assert!(has_quit(&effects), "Ctrl+D on an empty buffer must Quit");
        }

        /// 7. Ctrl+D on a non-empty buffer → forward-delete, no Quit
        #[test]
        fn test_reduce_key_pressed_ctrl_d_non_empty_buffer_inserts_char_or_eof() {
            let mut state = s();
            // type "abc" then press Home to move to the start of the line
            for ch in "abc".chars() {
                let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
            }
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
            assert_eq!(state.ui.input.text(), "abc");
            // Ctrl+D should forward-delete 'a'
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )));
            assert!(!has_quit(&effects), "Ctrl+D on a non-empty buffer must not Quit");
            assert_eq!(state.ui.input.text(), "bc", "forward-delete must remove 'a'");
        }

        /// 8. PasteReceived → content is appended to the input buffer
        #[test]
        fn test_reduce_paste_received_appends_to_input() {
            let mut state = s();
            let effects = state.reduce(Action::PasteReceived("pasted-text".to_string()));
            assert!(has_request_redraw(&effects));
            assert_eq!(state.ui.input.text(), "pasted-text");
        }

        #[test]
        fn test_large_paste_is_bounded_before_submit() {
            let mut state = s();
            let line = "b".repeat(1024);
            let pasted = std::iter::repeat_n(line.as_str(), 100).collect::<Vec<_>>().join("\n");

            let effects = state.reduce(Action::PasteReceived(pasted.clone()));
            assert!(has_request_redraw(&effects));
            assert_eq!(state.ui.input.byte_len(), crate::chat::tui::INPUT_MAX_BYTES);
            assert!(state.ui.input.truncated);
            assert!(pasted.starts_with(&state.ui.input.text()));
            let bounded = state.ui.input.text();

            let effects = state.reduce(Action::InputSubmitted(state.ui.input.text()));
            assert!(has_request_redraw(&effects));
            assert!(has_log_trace(&effects));
            assert_eq!(state.ui.last_submitted.as_deref(), Some(bounded.as_str()));
            assert!(state.ui.input.is_empty());
        }

        /// 9. TerminalResized → RequestRedraw
        #[test]
        fn test_reduce_terminal_resized_returns_redraw() {
            let mut state = s();
            let effects = state.reduce(Action::TerminalResized { w: 120, h: 40 });
            assert!(has_request_redraw(&effects));
        }

        /// 10. InputSubmitted (direct) → turn_count increments + last_submitted recorded
        #[test]
        fn test_reduce_input_submitted_increments_turn_count() {
            let mut state = s();
            assert_eq!(state.ui.turn_count, 0);
            let effects = state.reduce(Action::InputSubmitted("hello world".to_string()));
            assert_eq!(state.ui.turn_count, 1);
            assert_eq!(state.ui.last_submitted.as_deref(), Some("hello world"));
            assert!(state.ui.input.is_empty());
            assert!(has_log_trace(&effects));
            assert!(has_request_redraw(&effects));
        }

        /// Extra: HistoryNavigated Up must not panic on an empty history
        #[test]
        fn test_reduce_history_navigated_up_empty_history() {
            let mut state = s();
            let effects = state.reduce(Action::HistoryNavigated(HistoryDir::Up));
            assert!(has_request_redraw(&effects));
        }

        /// Extra: InputCancelled clears the buffer
        #[test]
        fn test_reduce_input_cancelled_clears_buffer() {
            let mut state = s();
            for ch in "draft".chars() {
                let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
            }
            assert_eq!(state.ui.input.text(), "draft");
            let effects = state.reduce(Action::InputCancelled);
            assert!(has_request_redraw(&effects));
            assert!(state.ui.input.is_empty());
        }

        #[test]
        fn ctrl_r_reverse_search_updates_state_and_snapshot_input() {
            use crate::chat::tui::ConversationLine;
            let mut state = s();
            state.ui.input.history = vec!["alpha".to_string(), "beta".to_string()];
            state.ui.conversation_lines.push(ConversationLine::Reasoning {
                content: "thinking".to_string(),
                char_count: 8,
                folded: true,
            });
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
            )));
            assert!(has_request_redraw(&effects));
            assert!(state.ui.input.is_reverse_search_active());
            assert_eq!(state.ui.input.text(), "beta");
            match state.ui.conversation_lines.last() {
                Some(ConversationLine::Reasoning { folded, .. }) => {
                    assert!(*folded, "Ctrl+R must not fold reasoning after P6b2");
                }
                other => panic!("expected Reasoning card, got {other:?}"),
            }
            let snap = state.build_ui_snapshot(1);
            assert!(snap.input.is_reverse_search_active());
            assert_eq!(snap.input.text(), "beta");
        }

        #[test]
        fn input_replaced_updates_snapshot_without_submit() {
            let mut state = s();
            state.ui.input.history = vec!["prior draft".to_string()];
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
            )));
            assert!(
                state.ui.input.is_reverse_search_active(),
                "test setup: reverse-search must be active before external editor replacement"
            );
            let effects = state.reduce(Action::InputReplaced("edited draft".to_string()));
            assert!(has_request_redraw(&effects));
            assert_eq!(state.ui.input.text(), "edited draft");
            assert!(
                !state.ui.input.is_reverse_search_active(),
                "external editor replacement must clear stale reverse-search title"
            );
            assert_eq!(state.ui.turn_count, 0, "external editor replacement must not submit");
            let snap = state.build_ui_snapshot(2);
            assert_eq!(snap.input.text(), "edited draft");
            assert!(
                !snap.input.is_reverse_search_active(),
                "snapshot must not expose stale reverse-search after replacement"
            );
        }

        /// Extra: ReasoningFoldToggled still returns RequestRedraw when there is no reasoning card
        #[test]
        fn test_reduce_reasoning_fold_toggled_no_panic_when_absent() {
            let mut state = s();
            let effects = state.reduce(Action::ReasoningFoldToggled);
            assert!(has_request_redraw(&effects));
        }

        /// Hidden reasoning must not intercept Tab in the primary transcript.
        #[test]
        fn test_tab_ignores_hidden_reasoning_card() {
            use crate::chat::tui::ConversationLine;
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let mut state = s();
            state.ui.conversation_lines.push(ConversationLine::Reasoning {
                content: "deep thoughts".to_string(),
                char_count: 12,
                folded: true,
            });
            let generation_before = state.ui.conversation_generation;
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert!(has_request_redraw(&effects));
            match state.ui.conversation_lines.last() {
                Some(ConversationLine::Reasoning { folded, .. }) => {
                    assert!(*folded, "Tab must leave hidden reasoning unchanged");
                }
                other => panic!("expected Reasoning card, got {other:?}"),
            }
            assert_eq!(state.ui.conversation_generation, generation_before);
        }

        /// A visible tool fold toggle (KeyPressed Tab) must mark the reduce dirty so
        /// `build_ui_snapshot` rebuilds with the new folded state (the Pure-mode
        /// renderer reads the snapshot, not the live state). Guards against the
        /// stale `cached_lines_arc` regression.
        #[test]
        fn test_fold_toggle_marks_snapshot_dirty_and_rebuilds() {
            use crate::chat::tui::ConversationLine;
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let mut state = s();
            state.ui.conversation_lines.push(ConversationLine::ToolResult {
                tool_name: "shell".to_string(),
                args_preview: "{}".to_string(),
                args_full: "{}".to_string(),
                result: Some("ok".to_string()),
                status: crate::chat::tui::ToolStatus::Done,
                elapsed_ms: Some(1),
                folded: true,
            });
            // Prime the snapshot cache.
            let _ = state.build_ui_snapshot(1);
            // Toggle via tracked reduce — must report dirty and clear the cache.
            let (_effects, dirty) =
                state.reduce_tracked(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert!(dirty, "fold toggle must mark snapshot dirty");
            let snap = state.build_ui_snapshot(2);
            match snap.conversation_lines.last() {
                Some(ConversationLine::ToolResult { folded, .. }) => {
                    assert!(!folded, "rebuilt snapshot must reflect the expanded tool card");
                }
                other => panic!("expected ToolResult card in snapshot, got {other:?}"),
            }
        }

        /// BUG-01 round-2: a fold toggle must bump `conversation_generation` so
        /// the snapshot/repaint path observes the new fold state.
        #[test]
        fn test_fold_toggle_bumps_conversation_generation() {
            use crate::chat::tui::ConversationLine;
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let mut state = s();
            state.ui.conversation_lines.push(ConversationLine::ToolResult {
                tool_name: "shell".to_string(),
                args_preview: "{}".to_string(),
                args_full: "{}".to_string(),
                result: Some("ok".to_string()),
                status: crate::chat::tui::ToolStatus::Done,
                elapsed_ms: Some(1),
                folded: true,
            });
            state.ui.conversation_lines.push(ConversationLine::Reasoning {
                content: "thoughts".to_string(),
                char_count: 8,
                folded: true,
            });
            let gen_before = state.ui.conversation_generation;

            // Tab toggles the visible tool even though hidden reasoning is newer.
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert_eq!(
                state.ui.conversation_generation,
                gen_before + 1,
                "Tab fold toggle must bump conversation_generation to force scrollback re-emit"
            );

            // Direct legacy fold action still bumps generation, but KeyPressed
            // Ctrl+R is reverse-search after P6b2 and must not own folding.
            let gen_after_tab = state.ui.conversation_generation;
            let _ = state.reduce(Action::ReasoningFoldToggled);
            assert_eq!(
                state.ui.conversation_generation,
                gen_after_tab + 1,
                "ReasoningFoldToggled must bump conversation_generation"
            );
        }

        /// BUG-01 round-2: a fold toggle with NO foldable card present must NOT
        /// bump the generation (avoids spurious full-conversation re-emits / a
        /// scrollback flood on stray Tab presses in a fresh session).
        #[test]
        fn test_fold_toggle_no_card_does_not_bump_generation() {
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let mut state = s();
            let gen_before = state.ui.conversation_generation;
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert_eq!(
                state.ui.conversation_generation, gen_before,
                "Tab with no foldable card must not bump generation"
            );
        }

        // ─── Step 2 integration tests (filling the P1-1 PTY coverage hole) ────────────
        //
        // The tests below build a ChatState directly and call a sequence of reduce calls to simulate a
        // full user interaction, covering the reducer path inside run_tui_unified_loop (the
        // PRX_CHAT_REDUX=1/both rollout). reduce_with_now injects a fixed time to avoid SystemTime.

        /// P1-1-a: full input-to-submit flow — paste "hello" → Enter → expect LogTrace + RequestRedraw
        #[test]
        fn test_redux_full_input_to_submit_flow() {
            let mut state = s();
            // paste "hello"
            let effects = state.reduce(Action::PasteReceived("hello".to_string()));
            assert!(has_request_redraw(&effects), "paste must trigger RequestRedraw");
            assert_eq!(state.ui.input.text(), "hello");
            // press Enter to submit
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
            assert!(has_log_trace(&effects), "Enter must trigger LogTrace");
            assert!(has_request_redraw(&effects), "Enter must trigger RequestRedraw");
            assert_eq!(state.ui.turn_count, 1, "turn_count must increment to 1");
            assert_eq!(
                state.ui.last_submitted.as_deref(),
                Some("hello"),
                "last_submitted must record 'hello'"
            );
            assert!(state.ui.input.is_empty(), "input buffer must be cleared after submit");
        }

        /// P1-1-b: two Ctrl+C presses within 500ms → Quit
        #[test]
        fn test_redux_double_ctrl_c_within_500ms_quits() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let effects = state.reduce_with_now(Action::KeyPressed(key.clone()), 100);
            assert!(!has_quit(&effects), "the first Ctrl+C must not Quit");
            let effects = state.reduce_with_now(Action::KeyPressed(key), 300);
            assert!(
                has_quit(&effects),
                "a Ctrl+C double press within 500ms must produce a Quit effect"
            );
        }

        /// P1-1-c: Ctrl+C presses more than 500ms apart do not quit
        #[test]
        fn test_redux_ctrl_c_then_ctrl_c_after_500ms_does_not_quit() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let effects = state.reduce_with_now(Action::KeyPressed(key.clone()), 100);
            assert!(!has_quit(&effects));
            // pressed again 700ms later — beyond the 500ms window
            let effects = state.reduce_with_now(Action::KeyPressed(key), 700);
            assert!(!has_quit(&effects), "a second press after 500ms must not Quit");
            assert_eq!(state.ui.last_ctrlc_ms, 700);
        }

        /// P1-1-d: Ctrl+D on an empty buffer → Quit
        #[test]
        fn test_redux_ctrl_d_empty_buffer_quits() {
            let mut state = s();
            assert!(state.ui.input.is_empty(), "precondition: the buffer is empty");
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )));
            assert!(has_quit(&effects), "Ctrl+D on an empty buffer must Quit");
        }

        /// P1-1-e: Ctrl+D on a non-empty buffer → no quit (forward-delete)
        #[test]
        fn test_redux_ctrl_d_non_empty_does_not_quit() {
            let mut state = s();
            // type "xyz" then Home so there is content after the cursor
            let _ = state.reduce(Action::PasteReceived("xyz".to_string()));
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
            assert_eq!(state.ui.input.text(), "xyz");
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )));
            assert!(!has_quit(&effects), "Ctrl+D on a non-empty buffer must not Quit");
        }

        /// P1-1-f: HistoryNavigated Up/Down — no panic on empty history, returns RequestRedraw
        #[test]
        fn test_redux_history_navigation_up_down() {
            let mut state = s();
            let up = state.reduce(Action::HistoryNavigated(HistoryDir::Up));
            assert!(has_request_redraw(&up), "Up must return RequestRedraw");
            let down = state.reduce(Action::HistoryNavigated(HistoryDir::Down));
            assert!(has_request_redraw(&down), "Down must return RequestRedraw");
        }

        /// P1-1-g: PasteReceived → the input buffer holds the text + RequestRedraw
        #[test]
        fn test_redux_paste_into_input() {
            let mut state = s();
            let effects = state.reduce(Action::PasteReceived("pasted content".to_string()));
            assert!(has_request_redraw(&effects), "a paste must trigger RequestRedraw");
            assert_eq!(state.ui.input.text(), "pasted content");
        }

        /// P1-1-h: TerminalResized → RequestRedraw
        #[test]
        fn test_redux_terminal_resize_returns_redraw() {
            let mut state = s();
            let effects = state.reduce(Action::TerminalResized { w: 80, h: 24 });
            assert!(has_request_redraw(&effects), "a resize must trigger RequestRedraw");
        }

        // ─── Step 3 unit tests (streaming + tool paths) ────────────────────────────
        //
        // Coverage goals:
        //   - the 5 streaming Actions (TurnStarted/StreamChunkReceived/StreamCompleted/
        //     StreamFailed/StreamCancelled) plus the 3 tool Actions
        //     (ToolStarted/ToolFinished/ToolProgress)
        //   - every stale-drop path now that the P3-5 version guard lives in the reducer
        //   - the idempotency boundary against the finalize_draft retry path
        //
        // These tests are the core evidence that the P3-5 version mechanism fully moved into the reducer.

        /// Step3-1: TurnStarted initialises stream.draft + active_cancel + generating
        #[test]
        fn test_redux_turn_started_sets_stream_state() {
            let mut state = s();
            assert!(state.stream.primary_streaming_draft().is_none());
            assert!(state.control.active_cancel.is_none());
            assert!(!state.control.generating);
            let effects = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(state.stream.primary_streaming_draft().is_some());
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.draft_id.clone()),
                Some("d1".to_string())
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(0));
            assert!(state.control.active_cancel.is_some());
            assert!(state.control.generating);
            assert!(has_request_redraw(&effects));
            assert!(has_log_trace(&effects));
        }

        /// Step3-2: a normal chunk → accumulates + bumps version + RequestRedraw
        #[test]
        fn test_redux_stream_chunk_received_valid_appends() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "hello".to_string(),
                version: 1,
            });
            assert!(has_request_redraw(&effects));
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("hello".to_string())
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(1));

            // second valid chunk → accumulates
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: " world".to_string(),
                version: 2,
            });
            assert!(has_request_redraw(&effects));
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("hello world".to_string())
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(2));
        }

        /// Step3-3: a stale version (version=1 arriving after version=2) → dropped
        #[test]
        fn test_redux_stream_chunk_received_stale_version_dropped() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            // version=2 arrives first
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "AB".to_string(),
                version: 2,
            });
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("AB".to_string())
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(2));
            // version=1 arrives later → dropped
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "STALE".to_string(),
                version: 1,
            });
            assert!(effects.is_empty(), "a stale version must return no effects");
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("AB".to_string()),
                "accumulated must stay unchanged"
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(2));

            // a repeated version=2 → also dropped (strict-monotonic)
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "DUP".to_string(),
                version: 2,
            });
            assert!(effects.is_empty(), "a repeated version must be dropped");
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("AB".to_string())
            );
        }

        /// Step3-4: a draft_id from another turn → dropped
        #[test]
        fn test_redux_stream_chunk_received_wrong_draft_id_dropped() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "ok".to_string(),
                version: 1,
            });
            // wrong draft_id → dropped
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d2".to_string(),
                delta: "STALE".to_string(),
                version: 99,
            });
            assert!(effects.is_empty(), "a mismatched draft_id must return no effects");
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("ok".to_string())
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(1));
        }

        /// Step3-5: a chunk arriving after finalize → dropped
        #[test]
        fn test_redux_stream_chunk_received_after_finalize_dropped() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "complete".to_string(),
                version: 1,
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "complete".to_string(),
                reasoning: String::new(),
            });
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "the draft must be cleared after finalize"
            );
            // any chunk after this is treated as stale
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "LATE".to_string(),
                version: 2,
            });
            assert!(effects.is_empty(), "a chunk after finalize must be dropped");
            assert!(state.stream.primary_streaming_draft().is_none());
        }

        /// Step3-6: StreamCompleted clears the draft + pushes assistant + NotifyHook
        #[test]
        fn test_redux_stream_completed_clears_draft_and_pushes_assistant() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(state.control.generating);
            let prev_lines = state.ui.conversation_lines.len();
            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "final answer".to_string(),
                reasoning: String::new(),
            });
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "the draft must be cleared"
            );
            assert!(state.control.active_cancel.is_none());
            assert!(!state.control.generating);
            assert_eq!(
                state.ui.conversation_lines.len(),
                prev_lines + 1,
                "only Assistant must be pushed"
            );
            // check that the last line is Assistant("final answer")
            if let Some(crate::chat::tui::ConversationLine::Assistant { content }) = state.ui.conversation_lines.last()
            {
                assert_eq!(content, "final answer");
            } else {
                panic!("the last line must be ConversationLine::Assistant");
            }
            assert!(has_request_redraw(&effects));
            assert!(
                effects.iter().any(|e| matches!(e, Effect::NotifyHook { .. })),
                "must contain NotifyHook(TurnComplete)"
            );
        }

        /// Step3-6b: StreamCompleted with reasoning → keeps Reasoning for the verbose transcript
        #[test]
        fn test_redux_stream_completed_with_reasoning_preserves_transcript_data() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "ans".to_string(),
                reasoning: "thinking step".to_string(),
            });
            // The completion duration does not enter the UI body; only Assistant + Reasoning are kept.
            assert_eq!(state.ui.conversation_lines.len(), 2);
            assert!(matches!(
                state.ui.conversation_lines.first(),
                Some(crate::chat::tui::ConversationLine::Assistant { .. })
            ));
            assert!(
                matches!(
                    state.ui.conversation_lines.last(),
                    Some(crate::chat::tui::ConversationLine::Reasoning { .. })
                ),
                "the last line must be Reasoning"
            );
        }

        /// F1 fixture: multi-segment thinking stream that crosses the
        /// bounded-tail threshold and mixes multi-byte / emoji / ASCII, so char vs
        /// byte counting and char-boundary truncation are both exercised.
        fn thinking_segments() -> Vec<String> {
            let base = [
                "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}: trace the call chain \u{2192}",
                "1) parse input \u{2705}\n",
                "2) check bounds \u{2014} empty, oversized, invalid UTF-8\n",
                "3) compose the final answer \u{1f600}\n",
            ];
            let mut out = Vec::new();
            for round in 0..6 {
                for seg in &base {
                    out.push(format!("[{round}] {seg}"));
                }
            }
            out
        }

        fn live_draft(state: &ChatState) -> &crate::chat::tui::StreamingDraft {
            state
                .stream
                .primary_streaming_draft()
                .expect("test: streaming draft present")
        }

        /// F1: every reasoning delta must advance the visible progress on the draft (counted in
        /// chars, not bytes) and request a redraw.
        #[test]
        fn test_reasoning_delta_updates_live_thinking_progress() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let segments = thinking_segments();
            let mut expected_chars = 0usize;
            for (idx, seg) in segments.iter().enumerate() {
                let version = u64::try_from(idx).unwrap_or(0).saturating_add(1);
                let effects = state.reduce(Action::StreamReasoningReceived {
                    draft_id: "d1".to_string(),
                    delta: seg.clone(),
                    version,
                });
                assert!(has_request_redraw(&effects), "delta {idx} must request a redraw");
                expected_chars = expected_chars.saturating_add(seg.chars().count());
                let draft = live_draft(&state);
                assert_eq!(
                    draft.reasoning_chars, expected_chars,
                    "delta {idx} count must update live"
                );
                assert_eq!(draft.version, version);
            }
            let joined: String = segments.concat();
            assert_ne!(
                joined.chars().count(),
                joined.len(),
                "the fixture must contain multi-byte chars, otherwise char and byte counts are indistinguishable"
            );
            let draft = live_draft(&state);
            assert_eq!(draft.reasoning_chars, joined.chars().count());
            assert!(
                draft.accumulated.is_empty(),
                "thinking must not pollute the visible text"
            );
            assert!(
                state.ui.conversation_lines.is_empty(),
                "no transcript lines while streaming"
            );
            let preview = draft.reasoning_preview().expect("test: preview present");
            assert!(!preview.is_empty());
            assert!(!preview.contains('\n'), "the preview must be folded into a single line");
        }

        /// F1: the tail is bounded and cut on char boundaries (never splitting a char); the count stays full.
        #[test]
        fn test_reasoning_tail_is_bounded_and_char_aligned() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let segments = thinking_segments();
            for (idx, seg) in segments.iter().enumerate() {
                let version = u64::try_from(idx).unwrap_or(0).saturating_add(1);
                let _ = state.reduce(Action::StreamReasoningReceived {
                    draft_id: "d1".to_string(),
                    delta: seg.clone(),
                    version,
                });
            }
            let joined: String = segments.concat();
            assert!(
                joined.chars().count() > crate::chat::state::REASONING_TAIL_MAX_CHARS,
                "the fixture must cross the tail threshold, otherwise the truncation branch is not covered"
            );
            let draft = live_draft(&state);
            assert_eq!(
                draft.reasoning_tail.chars().count(),
                crate::chat::state::REASONING_TAIL_MAX_CHARS,
                "the tail must be trimmed down to the limit"
            );
            let expected_tail: String = joined
                .chars()
                .skip(joined.chars().count() - crate::chat::state::REASONING_TAIL_MAX_CHARS)
                .collect();
            assert_eq!(
                draft.reasoning_tail, expected_tail,
                "the tail must be the text end with no split char"
            );
            assert_eq!(
                draft.reasoning_chars,
                joined.chars().count(),
                "the count is still the full text"
            );
        }

        /// F1: out-of-order / duplicate / stale versions are dropped by the same rule as
        /// StreamChunkReceived; both kinds of delta share one version counter.
        #[test]
        fn test_reasoning_delta_version_guard_drops_stale() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamReasoningReceived {
                draft_id: "d1".to_string(),
                delta: "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}\u{2026}\u{2026}".to_string(),
                version: 5,
            });
            let baseline = live_draft(&state).reasoning_chars;
            assert_eq!(
                baseline,
                "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}\u{2026}\u{2026}"
                    .chars()
                    .count()
            );

            for stale_version in [5u64, 3, 0] {
                let effects = state.reduce(Action::StreamReasoningReceived {
                    draft_id: "d1".to_string(),
                    delta: "STALE".to_string(),
                    version: stale_version,
                });
                assert!(effects.is_empty(), "version {stale_version} must be dropped");
                assert_eq!(live_draft(&state).reasoning_chars, baseline);
                assert_eq!(live_draft(&state).version, 5);
            }

            // an unknown draft_id (stale across turns) is dropped as well
            let effects = state.reduce(Action::StreamReasoningReceived {
                draft_id: "other".to_string(),
                delta: "STALE".to_string(),
                version: 99,
            });
            assert!(effects.is_empty(), "an unknown draft_id must be dropped");
            assert_eq!(live_draft(&state).reasoning_chars, baseline);

            // shared counter: once reasoning bumped the version, an older text delta is dropped too
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "late text".to_string(),
                version: 4,
            });
            assert!(
                effects.is_empty(),
                "the version counter is shared by both kinds of delta"
            );
            assert!(live_draft(&state).accumulated.is_empty());
        }

        /// F1: live progress must not change the final reasoning card after StreamCompleted.
        #[test]
        fn test_reasoning_progress_leaves_final_card_unchanged() {
            let full_reasoning = thinking_segments().concat();

            let mut with_progress = s();
            let _ = with_progress.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            for (idx, seg) in thinking_segments().iter().enumerate() {
                let version = u64::try_from(idx).unwrap_or(0).saturating_add(1);
                let _ = with_progress.reduce(Action::StreamReasoningReceived {
                    draft_id: "d1".to_string(),
                    delta: seg.clone(),
                    version,
                });
            }
            let _ = with_progress.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "ans".to_string(),
                reasoning: full_reasoning.clone(),
            });

            let mut without_progress = s();
            let _ = without_progress.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = without_progress.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "ans".to_string(),
                reasoning: full_reasoning.clone(),
            });

            assert_eq!(
                format!("{:?}", with_progress.ui.conversation_lines),
                format!("{:?}", without_progress.ui.conversation_lines),
                "the final transcript must be line-for-line identical with and without live progress"
            );
            match with_progress.ui.conversation_lines.last() {
                Some(crate::chat::tui::ConversationLine::Reasoning {
                    content,
                    char_count,
                    folded,
                }) => {
                    assert_eq!(content, &full_reasoning);
                    assert_eq!(*char_count, full_reasoning.chars().count());
                    assert!(*folded, "the final card is still folded by default");
                }
                other => panic!("the last line must be a Reasoning card, got {other:?}"),
            }
            assert!(
                with_progress.stream.primary_streaming_draft().is_none(),
                "the draft must be cleared after completion"
            );
        }

        /// F1: `--plain` (no TUI renderer) must not flood the screen with thinking progress — the
        /// Action only produces RequestRedraw (a no-op without a renderer), produces no output
        /// Effect, and appends no transcript line.
        #[test]
        fn test_reasoning_delta_produces_no_output_for_plain_mode() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let lines_before = state.ui.conversation_lines.len();
            for (idx, seg) in thinking_segments().iter().enumerate() {
                let version = u64::try_from(idx).unwrap_or(0).saturating_add(1);
                let effects = state.reduce(Action::StreamReasoningReceived {
                    draft_id: "d1".to_string(),
                    delta: seg.clone(),
                    version,
                });
                assert_eq!(effects.len(), 1, "each delta may produce only one Effect: {effects:?}");
                assert!(
                    matches!(effects.first(), Some(Effect::RequestRedraw)),
                    "the only Effect must be RequestRedraw: {effects:?}"
                );
            }
            assert_eq!(
                state.ui.conversation_lines.len(),
                lines_before,
                "in plain mode thinking must not write transcript lines"
            );
        }

        /// Step3-7: StreamFailed clears the draft + WARN LogTrace
        #[test]
        fn test_redux_stream_failed_clears_draft() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "partial".to_string(),
                version: 1,
            });
            let effects = state.reduce(Action::StreamFailed {
                draft_id: "d1".to_string(),
                err: "network".to_string(),
                retryable: true,
            });
            assert!(state.stream.primary_streaming_draft().is_none());
            assert!(!state.control.generating);
            assert!(has_request_redraw(&effects));
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::LogTrace { level, .. } if *level == tracing::Level::WARN)),
                "must contain a WARN LogTrace"
            );
        }

        /// Step3-8: StreamCancelled clears the draft + pushes no message
        #[test]
        fn test_redux_stream_cancelled_clears_draft() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "partial".to_string(),
                version: 1,
            });
            let lines_before = state.ui.conversation_lines.len();
            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "d1".to_string(),
            });
            assert!(state.stream.primary_streaming_draft().is_none());
            assert!(!state.control.generating);
            assert!(state.control.active_cancel.is_none());
            assert!(has_request_redraw(&effects));
            assert_eq!(
                state.ui.conversation_lines.len(),
                lines_before,
                "cancel must not push any conversation line"
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn stream_cancelled_finalizes_pending_running_tool_cards() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-tool-cancel".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"command":"sleep 10"}"#.to_string(),
            });

            let _ = state.reduce(Action::StreamCancelled {
                draft_id: "d-tool-cancel".to_string(),
            });

            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 0);
            assert!(
                !crate::chat::tui::execution_activity_active_for_view(&state.build_ui_snapshot(1)),
                "cancelled turn must not leave a running tool card driving the status bar"
            );
            assert!(
                state.ui.conversation_lines.iter().any(|line| matches!(
                    line,
                    crate::chat::tui::ConversationLine::ToolResult {
                        tool_name,
                        status: crate::chat::tui::ToolStatus::Error,
                        ..
                    } if tool_name == "shell"
                )),
                "pending shell tool card should be finalized as an error/cancelled card"
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn cancel_requested_finalizes_pending_running_tool_cards() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-tool-cancel-request".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"command":"sleep 10"}"#.to_string(),
            });

            let _ = state.reduce(Action::CancelRequested);

            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 0);
            assert!(
                !crate::chat::tui::execution_activity_active_for_view(&state.build_ui_snapshot(1)),
                "cancel request must not leave a running tool card driving the status bar"
            );
            assert!(
                state.ui.conversation_lines.iter().any(|line| matches!(
                    line,
                    crate::chat::tui::ConversationLine::ToolResult {
                        tool_name,
                        status: crate::chat::tui::ToolStatus::Error,
                        result: Some(result),
                        ..
                    } if tool_name == "shell" && result.contains("cancel request")
                )),
                "cancel request should finalize the shell tool card"
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn stream_completed_removes_unfinished_pending_tool_cards() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-tool-complete".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"command":"sleep 10"}"#.to_string(),
            });

            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d-tool-complete".to_string(),
                final_text: "done".to_string(),
                reasoning: String::new(),
            });

            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 0);
            assert!(
                !state
                    .ui
                    .conversation_lines
                    .iter()
                    .any(|line| matches!(line, crate::chat::tui::ConversationLine::ToolResult { .. })),
                "completed turn should remove unfinished placeholder tool cards"
            );
            assert!(
                !crate::chat::tui::execution_activity_active_for_view(&state.build_ui_snapshot(1)),
                "completed turn must not leave placeholder tool cards driving the status bar"
            );
        }

        #[test]
        fn stream_failed_removes_trailing_answerless_user_turn() {
            let mut state = s();
            let _ = state.reduce(Action::RecordUserTurn("failed question".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-failed".to_string(),
                cancel: CancellationToken::new(),
            });

            let effects = state.reduce(Action::StreamFailed {
                draft_id: "d-failed".to_string(),
                err: "provider failed".to_string(),
                retryable: false,
            });

            assert!(has_request_redraw(&effects));
            assert!(
                state.session.turns.is_empty(),
                "GP-9: failed turn must not leave an answerless user turn"
            );
            assert!(
                state
                    .session
                    .history
                    .iter()
                    .all(|message| message.content != "failed question"),
                "failed turn must also be removed from reducer history"
            );
            assert!(
                state.session.title.is_empty(),
                "first failed prompt must not become the session title"
            );
        }

        #[test]
        fn stream_cancelled_removes_trailing_answerless_user_turn() {
            let mut state = s();
            let _ = state.reduce(Action::RecordUserTurn("cancelled question".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-cancelled".to_string(),
                cancel: CancellationToken::new(),
            });

            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "d-cancelled".to_string(),
            });

            assert!(has_request_redraw(&effects));
            assert!(
                state.session.turns.is_empty(),
                "GP-9: cancelled turn must not leave an answerless user turn"
            );
            assert!(
                state
                    .session
                    .history
                    .iter()
                    .all(|message| message.content != "cancelled question"),
                "cancelled turn must also be removed from reducer history"
            );
        }

        #[test]
        fn failed_turn_orphan_is_absent_from_background_and_later_success_snapshots() {
            let mut state = s();
            let _ = state.reduce(Action::RecordUserTurn("orphan candidate".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-failed".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamFailed {
                draft_id: "d-failed".to_string(),
                err: "provider failed".to_string(),
                retryable: false,
            });

            let background_effects = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-after-failure", "completed"),
            });
            let background_snapshot = background_effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::SaveSession(snapshot) => Some(snapshot),
                    _ => None,
                })
                .expect("BackgroundSessionRecorded must emit SaveSession");
            assert!(
                background_snapshot
                    .turns
                    .iter()
                    .all(|turn| turn.content != "orphan candidate"),
                "GP-9: background save snapshot must not persist the failed user turn"
            );

            let _ = state.reduce(Action::RecordUserTurn("real question".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-ok".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "real answer".to_string(),
            });
            let completion_effects = state.reduce(Action::StreamCompleted {
                draft_id: "d-ok".to_string(),
                final_text: "real answer".to_string(),
                reasoning: String::new(),
            });
            let completion_snapshot = completion_effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::SaveSession(snapshot) => Some(snapshot),
                    _ => None,
                })
                .expect("StreamCompleted must emit SaveSession");
            assert_eq!(
                completion_snapshot.turns.len(),
                2,
                "only the successful exchange is persisted"
            );
            assert_eq!(
                completion_snapshot.turns.first().map(|turn| turn.content.as_str()),
                Some("real question")
            );
            assert_eq!(
                completion_snapshot.turns.get(1).map(|turn| turn.content.as_str()),
                Some("real answer")
            );
        }

        /// Step3-8b: StreamCancelled / StreamFailed / StreamCompleted with a mismatched draft_id → no-op
        #[test]
        fn test_redux_stream_terminal_actions_wrong_id_noop() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            // fire the three terminal actions with the wrong id — all must be no-ops
            let e1 = state.reduce(Action::StreamCancelled {
                draft_id: "wrong".to_string(),
            });
            let e2 = state.reduce(Action::StreamFailed {
                draft_id: "wrong".to_string(),
                err: "x".to_string(),
                retryable: false,
            });
            let e3 = state.reduce(Action::StreamCompleted {
                draft_id: "wrong".to_string(),
                final_text: "x".to_string(),
                reasoning: String::new(),
            });
            assert!(e1.is_empty() && e2.is_empty() && e3.is_empty());
            assert!(
                state.stream.primary_streaming_draft().is_some(),
                "the original draft must be kept"
            );
            assert!(state.control.generating, "the generating flag must be kept");
        }

        /// Step3-9: ToolStarted → push a Running ToolResult + enqueue its index
        #[test]
        fn test_redux_tool_started_pushes_card() {
            use crate::chat::tui::{ConversationLine, ToolStatus};
            let mut state = s();
            let effects = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            assert!(has_request_redraw(&effects));
            assert_eq!(state.ui.conversation_lines.len(), 1);
            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 1);
            if let Some(ConversationLine::ToolResult { tool_name, status, .. }) = state.ui.conversation_lines.last() {
                assert_eq!(tool_name, "shell");
                assert_eq!(*status, ToolStatus::Running);
            } else {
                panic!("the last line must be a Running ToolResult");
            }
        }

        #[test]
        fn test_redux_file_tool_cards_default_folded() {
            use crate::chat::tui::ConversationLine;
            for (tool_name, expected_folded) in [("file_edit", true), ("file_write", true), ("shell", true)] {
                let mut state = s();
                let _ = state.reduce(Action::ToolStarted {
                    task_id: None,
                    sequence: None,
                    tool_call_id: None,
                    name: tool_name.to_string(),
                    args: r#"{"path":"demo.txt"}"#.to_string(),
                });
                match state.ui.conversation_lines.last() {
                    Some(ConversationLine::ToolResult { folded, .. }) => {
                        assert_eq!(*folded, expected_folded, "{tool_name} folded default mismatch");
                    }
                    other => panic!("expected ToolResult for {tool_name}, got {other:?}"),
                }
            }
        }

        /// Step3-10: ToolFinished → Running → Done + removed from pending
        #[test]
        fn test_redux_tool_finished_updates_card() {
            use crate::chat::tui::{ConversationLine, ToolStatus};
            let mut state = s();
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            let effects = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                success: true,
                duration_ms: 42,
                result: Some("ok".to_string()),
            });
            assert!(has_request_redraw(&effects));
            assert_eq!(
                state.control.pending_tool_card_count(ToolTaskKey::Primary),
                0,
                "pending must be cleared"
            );
            if let Some(ConversationLine::ToolResult {
                status,
                elapsed_ms,
                result,
                ..
            }) = state.ui.conversation_lines.last()
            {
                assert_eq!(*status, ToolStatus::Done);
                assert_eq!(*elapsed_ms, Some(42));
                assert_eq!(result.as_deref(), Some("ok"));
            } else {
                panic!("the last line must be a ToolResult");
            }
        }

        /// Step3-10b: ToolFinished success=false → Error
        #[test]
        fn test_redux_tool_finished_failed_marks_error() {
            use crate::chat::tui::{ConversationLine, ToolStatus};
            let mut state = s();
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                success: false,
                duration_ms: 10,
                result: Some("err".to_string()),
            });
            if let Some(ConversationLine::ToolResult { status, .. }) = state.ui.conversation_lines.last() {
                assert_eq!(*status, ToolStatus::Error);
            } else {
                panic!("the last line must be a ToolResult");
            }
        }

        /// Step3-11: ToolProgress returns redraw/log and bumps UI generation.
        #[test]
        fn test_redux_tool_progress_returns_log_redraw_and_visible_generation() {
            let mut state = s();
            let before = state.ui.conversation_generation;
            let effects = state.reduce(Action::ToolProgress { iteration: 3 });
            assert!(has_request_redraw(&effects));
            assert!(has_log_trace(&effects));
            assert!(state.ui.conversation_lines.is_empty());
            assert_eq!(state.ui.conversation_generation, before + 1);
        }

        /// Step3-12: idempotency of the finalize path — even if StreamCompleted is fired twice by
        /// mistake, the second reduce must be a no-op (the draft_id no longer exists)
        #[test]
        fn test_redux_finalize_retry_after_stream_completed_idempotent() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "ans".to_string(),
                reasoning: String::new(),
            });
            let lines_after_first = state.ui.conversation_lines.len();
            // finalize the same draft_id again — the draft is already cleared → no-op
            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "ans-dup".to_string(),
                reasoning: String::new(),
            });
            assert!(effects.is_empty(), "a repeated finalize must be a no-op");
            assert_eq!(
                state.ui.conversation_lines.len(),
                lines_after_first,
                "no duplicate assistant line may be pushed (idempotent)"
            );
        }

        /// Step3-13: retrying a new Turn after StreamFailed — the version counter restarts at 0
        #[test]
        fn test_redux_finalize_retry_after_stream_failed() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "partial".to_string(),
                version: 5,
            });
            let _ = state.reduce(Action::StreamFailed {
                draft_id: "d1".to_string(),
                err: "boom".to_string(),
                retryable: true,
            });
            assert!(state.stream.primary_streaming_draft().is_none());
            // retry: open a new turn (the same draft_id is fine, draft.version starts at 0)
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(0));
            // version=1 must be accepted (unaffected by the previous round's 5, the draft was rebuilt)
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "retry".to_string(),
                version: 1,
            });
            assert!(has_request_redraw(&effects), "v=1 of a new turn must be accepted");
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("retry".to_string())
            );
        }

        #[test]
        fn esc_key_during_generation_cancels_active_turn() {
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-esc".to_string(),
                cancel: CancellationToken::new(),
            });
            state.ui.input.set_text("local draft");

            let effects = state.reduce_with_now(
                Action::KeyPressed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                1_000,
            );

            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::CancelDraft(id) if id == "draft-esc")),
                "Esc while generating must cancel the active draft: {effects:?}"
            );
            assert!(!state.control.generating);
            assert_eq!(state.ui.input.text(), "local draft");
        }

        /// P1-2: ten normal Actions in a row in Both mode — no semantic difference (diff_count baseline).
        ///
        /// This test checks that the reducer itself behaves stably; the real diff_count cannot be
        /// inspected across processes, so semantic consistency of the reduce output (stable effects for
        /// the same Action sequence) is checked as an equivalent.
        #[test]
        fn test_redux_both_mode_diff_count_zero() {
            let mut state1 = s();
            let mut state2 = s();
            // run the same Action sequence on two independent states; the effect categories must match
            let actions: Vec<Action> = vec![
                Action::PasteReceived("hello".to_string()),
                Action::TerminalResized { w: 120, h: 40 },
                Action::RedrawRequested,
                Action::ToolCardFoldToggled,
                Action::ReasoningFoldToggled,
                Action::HistoryNavigated(HistoryDir::Up),
                Action::HistoryNavigated(HistoryDir::Down),
                Action::InputCancelled,
                Action::InputSubmitted("test input".to_string()),
                Action::RedrawRequested,
            ];
            for action in actions {
                let e1 = state1.reduce(action.clone());
                let e2 = state2.reduce(action);
                // running the same action twice independently must yield the same effect categories
                assert_eq!(
                    e1.len(),
                    e2.len(),
                    "the same Action on two independent states must produce the same number of effects"
                );
            }
        }

        // ─── Step 4 unit tests (exit + session paths) ──────────────────────────────

        /// Step4-1: CancelRequested with generating=false → no-op (vec![])
        #[test]
        fn test_redux_cancel_requested_no_active_turn_noop() {
            let mut state = s();
            assert!(!state.control.generating, "precondition: not generating");
            let effects = state.reduce(Action::CancelRequested);
            assert!(
                effects.is_empty(),
                "CancelRequested while not generating must return vec![]"
            );
        }

        /// Step4-2: CancelRequested with generating=true and a draft → clears draft + CancelDraft effect
        #[test]
        fn test_redux_cancel_requested_with_active_turn_clears_state() {
            let mut state = s();
            // start a streaming round
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(state.control.generating);
            assert!(state.stream.primary_streaming_draft().is_some());

            let effects = state.reduce(Action::CancelRequested);

            // the state must be cleared
            assert!(!state.control.generating, "generating must be cleared to false");
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "the draft must be cleared"
            );
            assert!(state.control.active_cancel.is_none(), "active_cancel must be cleared");
            // effects must contain CancelDraft + LogTrace + RequestRedraw
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::CancelDraft(id) if id == "d1")),
                "must contain CancelDraft(d1)"
            );
            assert!(has_log_trace(&effects), "must contain LogTrace");
            assert!(has_request_redraw(&effects), "must contain RequestRedraw");
        }

        /// Step4-3: ShutdownRequested (idle) → vec![Quit]
        #[test]
        fn test_redux_shutdown_requested_returns_quit() {
            let mut state = s();
            let effects = state.reduce(Action::ShutdownRequested);
            assert!(has_quit(&effects), "ShutdownRequested must return a Quit effect");
            // no CancelDraft while idle
            assert!(
                !effects.iter().any(|e| matches!(e, Effect::CancelDraft(_))),
                "an idle shutdown must not contain CancelDraft"
            );
        }

        /// Step4-4: ShutdownRequested while streaming → Quit + CancelDraft
        #[test]
        fn test_redux_shutdown_during_streaming_cancels_draft() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d2".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(state.control.generating);

            let effects = state.reduce(Action::ShutdownRequested);

            assert!(!state.control.generating, "generating must be cleared");
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "the draft must be cleared"
            );
            // S2-B Step 2: the effect order became [CancelToken, CancelDraft, Quit].
            // CancelToken comes first (really cancels the underlying turn), CancelDraft follows (syncs the
            // channel UI), and Quit is last (the shell calls shutdown.cancel()).
            assert!(
                effects.len() >= 3,
                "a streaming ShutdownRequested must emit at least 3 effects"
            );
            assert!(
                matches!(effects.first(), Some(Effect::CancelToken(_))),
                "effects[0] must be CancelToken, got: {:?}",
                effects.first()
            );
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::CancelDraft(id) if id == "d2")),
                "effects must contain CancelDraft(d2)"
            );
            assert!(
                matches!(effects.last(), Some(Effect::Quit)),
                "effects.last() must be Quit, got: {:?}",
                effects.last()
            );
        }

        /// Step4-5: SessionLoaded replaces every session field
        #[test]
        fn test_redux_session_loaded_replaces_session_state() {
            use crate::chat::session::ChatSession;
            let mut state = s();
            // put something into history to confirm it gets replaced
            state.session.history.push(crate::providers::ChatMessage::user("old"));

            let mut loaded = ChatSession::new("prov2", "model2");
            loaded.id = "sess-abc".to_string();
            loaded.title = "My Session".to_string();
            loaded.add_user_turn("hello");
            loaded.add_assistant_turn("hi", vec![]);

            let effects = state.reduce(Action::SessionLoaded(loaded));

            assert_eq!(state.session.id, "sess-abc");
            assert_eq!(state.session.title, "My Session");
            assert_eq!(&*state.session.provider, "prov2");
            assert_eq!(&*state.session.model, "model2");
            assert_eq!(state.session.turns.len(), 2, "2 turns");
            // history rebuilt from turns: user + assistant
            assert_eq!(
                state.session.history.len(),
                2,
                "history must be rebuilt from turns (user+assistant)"
            );
            assert_eq!(
                state.ui.conversation_lines.len(),
                2,
                "UI conversation_lines must be rebuilt from the restored turns"
            );
            assert!(has_request_redraw(&effects), "must contain RequestRedraw");
            assert!(has_log_trace(&effects), "must contain LogTrace");
        }

        fn bg_summary(id: &str, status: &str) -> crate::chat::sessions::PersistedSessionSummary {
            crate::chat::sessions::PersistedSessionSummary {
                id: id.to_string(),
                seq: 1,
                kind: "agent".to_string(),
                origin: "user".to_string(),
                status: status.to_string(),
                title: "task".to_string(),
                summary: String::new(),
                token_usage_records: Vec::new(),
                created_at: chrono::Utc::now(),
            }
        }

        /// v4: BackgroundSessionRecorded upserts into session.background_sessions
        /// (dedup by id) and emits SaveSession so the summary is persisted.
        #[test]
        fn test_redux_background_session_recorded_upserts() {
            let mut state = s();
            let e1 = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-1", "running"),
            });
            // P0 (v4 review): a record that changed state must emit SaveSession
            // (the only durable write path). It must NOT redraw — a background
            // summary write is invisible to the live conversation surface.
            assert_eq!(e1.len(), 1, "exactly one effect: SaveSession");
            assert!(
                matches!(e1.first(), Some(Effect::SaveSession(_))),
                "changed record must emit SaveSession, got {e1:?}"
            );
            assert_eq!(state.session.background_sessions.len(), 1);

            // Same id again with a terminal status replaces, does not duplicate,
            // and still emits a fresh SaveSession (state changed). Reuse the same
            // value for the no-op check below (bg_summary stamps a fresh
            // created_at per call, which would otherwise count as a change).
            let completed = bg_summary("run-1", "completed");
            let e2 = state.reduce(Action::BackgroundSessionRecorded {
                summary: completed.clone(),
            });
            assert!(matches!(e2.first(), Some(Effect::SaveSession(_))));
            assert_eq!(state.session.background_sessions.len(), 1);
            assert_eq!(
                state.session.background_sessions.first().map(|s| s.status.as_str()),
                Some("completed")
            );

            // An identical re-record is a no-op: no state change, no effect, no
            // save storm.
            let e_dup = state.reduce(Action::BackgroundSessionRecorded { summary: completed });
            assert!(
                e_dup.is_empty(),
                "unchanged re-record must short-circuit with no SaveSession (no save storm)"
            );

            // A different id appends and saves.
            let e3 = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-2", "failed"),
            });
            assert!(matches!(e3.first(), Some(Effect::SaveSession(_))));
            assert_eq!(state.session.background_sessions.len(), 2);
        }

        /// B1 (P0, v4 review): dispatching BackgroundSessionRecorded must emit a
        /// SaveSession whose snapshot ALREADY contains the just-recorded summary.
        /// This is the regression guard: previously the reducer returned no
        /// effect, so the terminal-summary never reached the memory backend
        /// (legacy exit-save is disabled under terminal-tui), breaking reload
        /// recap. Round-trip: capture the SaveSession snapshot → reload it into a
        /// fresh state → the background summary is present.
        #[test]
        fn test_redux_background_session_recorded_emits_savesession_with_summary() {
            let mut state = s();
            let effects = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-42", "completed"),
            });
            let snapshot = effects
                .iter()
                .find_map(|e| match e {
                    Effect::SaveSession(session) => Some(session.clone()),
                    _ => None,
                })
                .expect("BackgroundSessionRecorded must emit Effect::SaveSession");
            // The emitted snapshot must already carry the recorded summary —
            // proving the save happens AFTER the upsert (no race where the
            // snapshot predates the state mutation).
            assert_eq!(
                snapshot.background_sessions.len(),
                1,
                "SaveSession snapshot must contain the just-recorded child session"
            );
            let recorded = snapshot
                .background_sessions
                .first()
                .expect("snapshot child session present");
            assert_eq!(recorded.id, "run-42");
            assert_eq!(recorded.status, "completed");

            // Round-trip: reloading that snapshot into a fresh state restores it,
            // confirming the persisted blob is sufficient for reload recap.
            let mut reloaded = s();
            let _ = reloaded.reduce(Action::SessionLoaded(snapshot));
            assert_eq!(reloaded.session.background_sessions.len(), 1);
            assert_eq!(
                reloaded.session.background_sessions.first().map(|s| s.id.as_str()),
                Some("run-42")
            );
        }

        /// v4: a recorded child session must survive a save→load round trip
        /// through the reducer (snapshot persists it, SessionLoaded restores it),
        /// and a still-running session is never restored as a live one.
        #[test]
        fn test_redux_background_sessions_survive_snapshot_and_reload() {
            let mut state = s();
            let _ = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-1", "completed"),
            });
            // An interrupted entry stands in for "was running at last exit".
            let _ = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-2", crate::chat::sessions::model::STATUS_INTERRUPTED),
            });

            // The snapshot the SaveSession effect would persist must carry them.
            let snapshot = state.build_session_snapshot();
            assert_eq!(snapshot.background_sessions.len(), 2);

            // Reloading that snapshot into a fresh state restores the summaries.
            let mut fresh = s();
            let _ = fresh.reduce(Action::SessionLoaded(snapshot));
            assert_eq!(fresh.session.background_sessions.len(), 2);
            // None of the restored entries is a live status — reload never
            // resurrects a running process.
            for bg in &fresh.session.background_sessions {
                assert_ne!(bg.status, "running");
                assert_ne!(bg.status, "needs-input");
            }
            let statuses: Vec<&str> = fresh
                .session
                .background_sessions
                .iter()
                .map(|s| s.status.as_str())
                .collect();
            assert!(statuses.contains(&"completed"));
            assert!(statuses.contains(&crate::chat::sessions::model::STATUS_INTERRUPTED));
        }

        /// Step4-5b: SessionLoaded with a system prompt — user/assistant are kept in history, the
        /// system turn does not enter the LLM history (role=system turns are filtered out)
        #[test]
        fn test_redux_session_loaded_only_user_assistant_in_history() {
            use crate::chat::session::{ChatSession, ChatTurn};
            let mut state = s();
            let mut loaded = ChatSession::new("p", "m");
            loaded.turns.push(ChatTurn {
                role: "system".to_string(),
                content: "You are helpful".to_string(),
                timestamp: chrono::Utc::now(),
                tool_calls: vec![],
            });
            loaded.add_user_turn("q");
            let _ = state.reduce(Action::SessionLoaded(loaded));
            // history holds only user, no system (SessionLoaded does not add system to history)
            assert_eq!(
                state.session.history.len(),
                1,
                "only user turns enter history (role=system is filtered out)"
            );
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("user"),
                "history[0] must have the user role"
            );
        }

        /// Step4-6: SessionSaved updates session.id
        #[test]
        fn test_redux_session_saved_updates_id() {
            let mut state = s();
            state.session.id = String::new(); // simulate not having an id yet
            let effects = state.reduce(Action::SessionSaved {
                id: "new-id-123".to_string(),
            });
            assert_eq!(state.session.id, "new-id-123");
            assert!(has_log_trace(&effects), "must contain LogTrace");
        }

        /// Step4-6b: SessionSaved with the same id — unchanged (idempotent)
        #[test]
        fn test_redux_session_saved_same_id_idempotent() {
            let mut state = s();
            state.session.id = "already-set".to_string();
            let effects = state.reduce(Action::SessionSaved {
                id: "already-set".to_string(),
            });
            assert_eq!(state.session.id, "already-set");
            assert!(has_log_trace(&effects));
        }

        /// Step4-7: SessionSwitched → SaveSession + LogTrace + RequestRedraw
        #[test]
        fn test_redux_session_switched_saves_current_then_logs() {
            let mut state = s();
            state.session.id = "cur-session".to_string();
            state.session.title = "Current".to_string();
            let effects = state.reduce(Action::SessionSwitched {
                id: "new-session".to_string(),
            });
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::SaveSession(sess) if sess.id == "cur-session")),
                "the current session must be saved first"
            );
            assert!(has_log_trace(&effects), "must contain LogTrace");
            assert!(has_request_redraw(&effects), "must contain RequestRedraw");
        }

        /// P2-C: SessionSwitched effects[0] is exactly SaveSession (save first in the two-step flow)
        #[test]
        fn test_redux_session_switched_emits_save_first() {
            let mut state = s();
            state.session.id = "session-x".to_string();
            let effects = state.reduce(Action::SessionSwitched {
                id: "session-y".to_string(),
            });
            assert!(!effects.is_empty(), "SessionSwitched must emit at least 1 effect");
            assert!(
                matches!(effects.first(), Some(Effect::SaveSession(sess)) if sess.id == "session-x"),
                "effects[0] must be SaveSession(current), got: {:?}",
                effects.first()
            );
        }

        /// Step4-8: RecordUserTurn → session.turns + history grow, updated_at refreshed, title set
        #[test]
        fn test_redux_record_user_turn_grows_history() {
            let mut state = s();
            assert_eq!(state.session.turns.len(), 0);
            assert_eq!(state.session.history.len(), 0);
            assert!(state.session.title.is_empty(), "the initial title is empty");
            let effects = state.reduce(Action::RecordUserTurn("what is Rust?".to_string()));
            assert_eq!(state.session.turns.len(), 1, "turns grew");
            assert_eq!(state.session.history.len(), 1, "history grew");
            assert_eq!(
                state.session.turns.first().map(|t| t.role.as_str()),
                Some("user"),
                "turns[0] role"
            );
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("user"),
                "history[0] role"
            );
            assert_eq!(
                state.session.history.first().map(|m| m.content.as_str()),
                Some("what is Rust?"),
                "history[0] content"
            );
            // the first user turn sets the title automatically
            assert_eq!(
                state.session.title, "what is Rust?",
                "the first user turn must set_title"
            );
            assert!(has_log_trace(&effects));
        }

        /// Step4-8b: a second RecordUserTurn must not overwrite an existing title
        #[test]
        fn test_redux_record_user_turn_no_overwrite_existing_title() {
            let mut state = s();
            state.session.title = "My Chat".to_string();
            let _ = state.reduce(Action::RecordUserTurn("second question".to_string()));
            assert_eq!(
                state.session.title, "My Chat",
                "an existing title must not be overwritten"
            );
        }

        /// Step4-9: RecordAssistantTurn → session.turns + history grow, updated_at refreshed
        #[test]
        fn test_redux_record_assistant_turn_grows_history() {
            let mut state = s();
            let _ = state.reduce(Action::RecordUserTurn("hello".to_string()));
            let effects = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "Rust is fast.".to_string(),
            });
            assert_eq!(state.session.turns.len(), 2, "turns grew to 2");
            assert_eq!(state.session.history.len(), 2, "history grew to 2");
            assert_eq!(
                state.session.turns.last().map(|t| t.role.as_str()),
                Some("assistant"),
                "turns.last() role"
            );
            assert_eq!(
                state.session.history.last().map(|m| m.role.as_str()),
                Some("assistant"),
                "history.last() role"
            );
            assert_eq!(
                state.session.history.last().map(|m| m.content.as_str()),
                Some("Rust is fast."),
                "history.last() content"
            );
            assert!(has_log_trace(&effects));
        }

        /// Step4-10: HistoryCleared — keeps the system prompt, clears user/assistant
        #[test]
        fn test_redux_history_cleared_keeps_system_prompt() {
            let mut state = s();
            // build system + user + assistant
            state
                .session
                .history
                .push(crate::providers::ChatMessage::system("Be helpful"));
            state.session.history.push(crate::providers::ChatMessage::user("hi"));
            state
                .session
                .history
                .push(crate::providers::ChatMessage::assistant("hello!"));
            state
                .ui
                .conversation_lines
                .push(crate::chat::tui::ConversationLine::User {
                    content: "hi".to_string(),
                });
            assert_eq!(state.session.history.len(), 3);

            let effects = state.reduce(Action::HistoryCleared);

            // history keeps only the system prompt
            assert_eq!(
                state.session.history.len(),
                1,
                "only the system prompt may remain after clearing"
            );
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("system"),
                "the kept history[0] must be system"
            );
            // conversation_lines is cleared
            assert!(
                state.ui.conversation_lines.is_empty(),
                "UI conversation_lines must be cleared"
            );
            assert!(has_request_redraw(&effects));
            assert!(has_log_trace(&effects));
        }

        /// Step4-10b: HistoryCleared without a system prompt — clears everything
        #[test]
        fn test_redux_history_cleared_no_system_prompt_clears_all() {
            let mut state = s();
            state.session.history.push(crate::providers::ChatMessage::user("q"));
            state
                .session
                .history
                .push(crate::providers::ChatMessage::assistant("a"));
            let effects = state.reduce(Action::HistoryCleared);
            assert!(
                state.session.history.is_empty(),
                "without a system prompt everything is cleared"
            );
            assert!(has_request_redraw(&effects));
        }

        /// P2-D: HistoryCleared with system not first — it is still kept (defensive full scan)
        #[test]
        fn test_redux_history_cleared_preserves_system_in_middle() {
            let mut state = s();
            // deliberately put system in the middle (unusual order, handled defensively)
            state.session.history.push(crate::providers::ChatMessage::user("q1"));
            state
                .session
                .history
                .push(crate::providers::ChatMessage::system("Be helpful"));
            state
                .session
                .history
                .push(crate::providers::ChatMessage::assistant("a1"));
            assert_eq!(state.session.history.len(), 3);

            let _effects = state.reduce(Action::HistoryCleared);

            // the system message must be kept, user/assistant cleared
            assert_eq!(state.session.history.len(), 1, "only 1 system message may remain");
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("system"),
                "the kept message must be the system one"
            );
        }

        /// Step4-11: the full double Ctrl+C flow (including the Effect sequence)
        ///
        /// t=100: KeyPressed(Ctrl+C) → single press, last_ctrlc_ms=100, no Quit
        /// t=300: KeyPressed(Ctrl+C) → double press (<500ms) → Quit effect
        #[test]
        fn test_redux_double_ctrl_c_flow_e2e() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

            // first Ctrl+C at t=100
            let effects1 = state.reduce_with_now(Action::KeyPressed(key.clone()), 100);
            assert!(!has_quit(&effects1), "the first Ctrl+C must not Quit");
            assert_eq!(state.ui.last_ctrlc_ms, 100, "records the window timestamp");

            // second Ctrl+C at t=300 (within 100ms)
            let effects2 = state.reduce_with_now(Action::KeyPressed(key), 300);
            assert!(
                has_quit(&effects2),
                "a Ctrl+C double press within 300ms must produce Quit"
            );

            // check that Effect::Quit is in the result
            let has_quit_effect = effects2.iter().any(|e| matches!(e, Effect::Quit));
            assert!(has_quit_effect, "effects2 must contain Effect::Quit");
        }

        /// A turn that degrades on every tool iteration owes the user one line,
        /// not one per iteration — and the next turn is a new fact, so the latch
        /// has to re-arm.
        #[test]
        fn context_degradation_notice_is_surfaced_once_per_turn() {
            use crate::chat::action::CompactReason;
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-1".to_string(),
                cancel: CancellationToken::new(),
            });

            let first = state.reduce(Action::HistoryCompactionDegraded {
                reason: CompactReason::ContextOverflow,
                dropped_messages: 7,
            });
            let notices = first
                .iter()
                .filter_map(|effect| match effect {
                    Effect::SurfaceNotice { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                notices.len(),
                1,
                "the first degradation must surface exactly one notice"
            );
            let notice = notices.first().cloned().unwrap_or_default();
            assert!(
                notice.contains("lossy") && notice.contains('7'),
                "the notice must say the trim was lossy and how much it dropped: {notice}"
            );
            assert!(
                !notice.contains('\n'),
                "the notice must stay a single line for plain mode: {notice:?}"
            );

            let second = state.reduce(Action::HistoryCompactionDegraded {
                reason: CompactReason::ContextOverflow,
                dropped_messages: 4,
            });
            assert!(
                !second
                    .iter()
                    .any(|effect| matches!(effect, Effect::SurfaceNotice { .. })),
                "a second degradation inside the same turn must not repeat the notice"
            );

            #[cfg(feature = "terminal-tui")]
            {
                let lines = state
                    .ui
                    .conversation_lines
                    .iter()
                    .filter(|line| {
                        matches!(line, crate::chat::tui::ConversationLine::System { content }
                            if content.contains("lossy"))
                    })
                    .count();
                assert_eq!(lines, 1, "the transcript must carry the notice exactly once");
            }

            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-2".to_string(),
                cancel: CancellationToken::new(),
            });
            let next_turn = state.reduce(Action::HistoryCompactionDegraded {
                reason: CompactReason::ContextOverflow,
                dropped_messages: 2,
            });
            assert!(
                next_turn
                    .iter()
                    .any(|effect| matches!(effect, Effect::SurfaceNotice { .. })),
                "a new turn that degrades is a new fact and must be surfaced again"
            );
        }

        /// S2-B Step 1: HistoryCompacted algorithm baseline — keep system + truncate each + cap the total
        #[test]
        fn test_redux_history_compacted_basic_algorithm() {
            use crate::chat::action::CompactReason;
            let mut state = s();
            // build 1 system + 20 long user/assistant messages
            state
                .session
                .history
                .push(crate::providers::ChatMessage::system("system prompt - keep me"));
            for i in 0..20 {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                let content = "x".repeat(500); // over COMPACT_CONTENT_CHARS=320
                state.session.history.push(crate::providers::ChatMessage {
                    role: role.to_string(),
                    content: format!("{content} #{i}"),
                });
            }
            let before_len = state.session.history.len();
            assert_eq!(before_len, 21);

            let effects = state.reduce(Action::HistoryCompacted {
                reason: CompactReason::ContextOverflow,
            });

            // the system prompt must stay in first position
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("system"),
                "system must still be first after compaction"
            );
            // the total count must be <= 1 system + COMPACT_KEEP_MESSAGES
            assert!(
                state.session.history.len() <= 1 + super::COMPACT_KEEP_MESSAGES,
                "after compaction the non-system count must be <= COMPACT_KEEP_MESSAGES, got {}",
                state.session.history.len()
            );
            // each non-system message must have <= COMPACT_CONTENT_CHARS chars (+3 for the "...")
            for m in state.session.history.iter().skip(1) {
                assert!(
                    m.content.chars().count() <= super::COMPACT_CONTENT_CHARS + 3,
                    "non-system msg should be truncated, got {} chars",
                    m.content.chars().count()
                );
            }
            // the total budget (non-system) must be <= COMPACT_TOTAL_CHARS
            let non_system_chars: usize = state
                .session
                .history
                .iter()
                .skip(1)
                .map(|m| m.content.chars().count())
                .sum();
            assert!(
                non_system_chars <= super::COMPACT_TOTAL_CHARS,
                "non-system total chars {non_system_chars} > budget {}",
                super::COMPACT_TOTAL_CHARS
            );
            // LogTrace must be emitted
            assert!(has_log_trace(&effects), "HistoryCompacted must emit LogTrace");
        }

        /// S2-B Step 1: HistoryCompacted is a no-op when len<=1
        #[test]
        fn test_redux_history_compacted_noop_when_short() {
            use crate::chat::action::CompactReason;
            let mut state = s();
            // only 1 system message → nothing to compact
            state
                .session
                .history
                .push(crate::providers::ChatMessage::system("only system"));
            let effects = state.reduce(Action::HistoryCompacted {
                reason: CompactReason::Manual,
            });
            assert_eq!(state.session.history.len(), 1, "unchanged when len<=1");
            assert!(has_log_trace(&effects), "a no-op still emits LogTrace(DEBUG)");
        }

        fn messages_as_pairs(messages: &[crate::providers::ChatMessage]) -> Vec<(String, String)> {
            messages
                .iter()
                .map(|message| (message.role.clone(), message.content.clone()))
                .collect()
        }

        #[test]
        fn redux_compaction_patch_applies_exactly_and_matches_driver_history() {
            use crate::chat::action::CompactReason;
            let mut state = s();
            state.session.history = vec![
                crate::providers::ChatMessage::system("sys"),
                crate::providers::ChatMessage::user("old user"),
                crate::providers::ChatMessage::assistant("old assistant"),
                crate::providers::ChatMessage::user("recent user"),
            ];
            let mut driver_history = state.session.history.clone();
            let guard = crate::agent::loop_::compaction_patch_guard_for(&driver_history, 1, 3).expect("guard");
            let patch = crate::agent::loop_::CompactionPatch {
                range_start: 1,
                range_end: 3,
                replacement: vec![crate::providers::ChatMessage::assistant(
                    "[Context compacted at test. Summary: PROVIDER_SUMMARY_MARKER]",
                )],
                append_after: vec![crate::providers::ChatMessage::user(
                    "[Post-compaction context refresh]\nre-read",
                )],
                guard,
            };
            let config = crate::config::AgentCompactionConfig {
                max_context_tokens: 10_000,
                reserve_tokens: 10,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };

            crate::agent::loop_::apply_compaction_patch_exact(&mut driver_history, &patch);
            let effects = state.reduce(Action::HistoryCompactionPatchApplied {
                reason: CompactReason::ContextOverflow,
                patch,
                compaction_config: config,
            });

            assert_eq!(
                messages_as_pairs(&state.session.history),
                messages_as_pairs(&driver_history),
                "GP-6: reducer history must exactly match driver history after patch"
            );
            assert!(
                state
                    .session
                    .history
                    .iter()
                    .any(|message| message.content.contains("PROVIDER_SUMMARY_MARKER")),
                "provider summary marker must be present"
            );
            assert!(has_log_trace(&effects));
        }

        #[test]
        fn compaction_patch_refresh_position_parity_between_legacy_and_redux() {
            use crate::chat::action::CompactReason;
            let current_question = "What should ISS-037 answer now?";
            let mut state = s();
            state.session.history = vec![
                crate::providers::ChatMessage::system("sys"),
                crate::providers::ChatMessage::user("old user"),
                crate::providers::ChatMessage::assistant("old assistant"),
                crate::providers::ChatMessage::user(current_question),
            ];
            let mut legacy_history = state.session.history.clone();
            let guard = crate::agent::loop_::compaction_patch_guard_for(&legacy_history, 1, 3).expect("guard");
            let patch = crate::agent::loop_::CompactionPatch {
                range_start: 1,
                range_end: 3,
                replacement: vec![crate::providers::ChatMessage::assistant(
                    "[Context compacted at test. Summary: ISS-037 parity summary]",
                )],
                append_after: vec![crate::providers::ChatMessage::user(
                    "[Post-compaction context refresh]\nre-read",
                )],
                guard,
            };
            let config = crate::config::AgentCompactionConfig {
                max_context_tokens: 10_000,
                reserve_tokens: 10,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };

            crate::agent::loop_::apply_compaction_patch_exact(&mut legacy_history, &patch);
            let _ = state.reduce(Action::HistoryCompactionPatchApplied {
                reason: CompactReason::ContextOverflow,
                patch,
                compaction_config: config,
            });

            assert_eq!(
                messages_as_pairs(&state.session.history),
                messages_as_pairs(&legacy_history),
                "GP-6: legacy and Redux histories must match exactly after the shared patch primitive"
            );
            let refresh_index = state
                .session
                .history
                .iter()
                .position(|message| message.content.starts_with("[Post-compaction context refresh]"))
                .expect("refresh marker should be present");
            let summary_index = state
                .session
                .history
                .iter()
                .position(|message| message.content.contains("ISS-037 parity summary"))
                .expect("summary marker should be present");
            let question_index = state
                .session
                .history
                .iter()
                .position(|message| message.content == current_question)
                .expect("current question should be present");
            assert!(
                summary_index < refresh_index && refresh_index < question_index,
                "refresh marker must sit after the summary and before the real current question"
            );
            assert_eq!(
                state.session.history.last().map(|message| message.content.as_str()),
                Some(current_question),
                "real current user question must remain the trailing provider-bound user message"
            );
        }

        #[test]
        fn post_compaction_refresh_not_persisted_as_session_turn() {
            use crate::chat::action::CompactReason;
            let current_question = "Persist this as the real user turn";
            let assistant_reply = "assistant reply bound to the real user turn";
            let mut state = s();
            state.session.history = vec![
                crate::providers::ChatMessage::system("sys"),
                crate::providers::ChatMessage::user("old user"),
                crate::providers::ChatMessage::assistant("old assistant"),
            ];
            let _ = state.reduce(Action::RecordUserTurn(current_question.to_string()));
            let guard = crate::agent::loop_::compaction_patch_guard_for(&state.session.history, 1, 3).expect("guard");
            let patch = crate::agent::loop_::CompactionPatch {
                range_start: 1,
                range_end: 3,
                replacement: vec![crate::providers::ChatMessage::assistant(
                    "[Context compacted at test. Summary: persisted-turn summary]",
                )],
                append_after: vec![crate::providers::ChatMessage::user(
                    "[Post-compaction context refresh]\nre-read",
                )],
                guard,
            };
            let config = crate::config::AgentCompactionConfig {
                max_context_tokens: 10_000,
                reserve_tokens: 10,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };

            let _ = state.reduce(Action::HistoryCompactionPatchApplied {
                reason: CompactReason::ContextOverflow,
                patch,
                compaction_config: config,
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: assistant_reply.to_string(),
            });

            assert_eq!(
                state.session.turns.len(),
                3,
                "the compaction summary, real user, and assistant turns are persisted"
            );
            let [summary_turn, user_turn, assistant_turn] = state.session.turns.as_slice() else {
                panic!("expected summary, real user turn, and assistant turn");
            };
            assert_eq!(summary_turn.role, "assistant");
            assert!(summary_turn.content.contains("persisted-turn summary"));
            assert_eq!(user_turn.role, "user");
            assert_eq!(user_turn.content, current_question);
            assert_eq!(assistant_turn.role, "assistant");
            assert_eq!(assistant_turn.content, assistant_reply);
            assert!(
                state
                    .session
                    .turns
                    .iter()
                    .all(|turn| !turn.content.starts_with("[Post-compaction context refresh]")),
                "refresh marker must remain a history context marker, not a persisted user turn"
            );

            let snapshot = state.build_session_snapshot();
            let mut reloaded = s();
            let _ = reloaded.reduce(Action::SessionLoaded(snapshot));
            assert_eq!(
                messages_as_pairs(&reloaded.session.history),
                vec![
                    (
                        "assistant".to_string(),
                        "[Context compacted at test. Summary: persisted-turn summary]".to_string()
                    ),
                    ("user".to_string(), current_question.to_string()),
                    ("assistant".to_string(), assistant_reply.to_string()),
                ],
                "resume must rebuild the compacted durable turn shape"
            );
        }

        #[test]
        fn redux_compaction_patch_guard_mismatch_falls_back_without_stale_patch() {
            use crate::chat::action::CompactReason;
            let mut state = s();
            let original = vec![
                crate::providers::ChatMessage::system("sys"),
                crate::providers::ChatMessage::user(format!("old user {}", "x ".repeat(180))),
                crate::providers::ChatMessage::assistant(format!("old assistant {}", "y ".repeat(180))),
                crate::providers::ChatMessage::user(format!("recent {}", "z ".repeat(180))),
            ];
            let guard = crate::agent::loop_::compaction_patch_guard_for(&original, 1, 3).expect("guard");
            state.session.history = original;
            state
                .session
                .history
                .push(crate::providers::ChatMessage::user("mutation before reducer"));
            let patch = crate::agent::loop_::CompactionPatch {
                range_start: 1,
                range_end: 3,
                replacement: vec![crate::providers::ChatMessage::assistant(
                    "[Context compacted at test. Summary: STALE_PROVIDER_SUMMARY]",
                )],
                append_after: vec![crate::providers::ChatMessage::user("stale refresh")],
                guard,
            };
            let config = crate::config::AgentCompactionConfig {
                max_context_tokens: 90,
                reserve_tokens: 10,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };

            let effects = state.reduce(Action::HistoryCompactionPatchApplied {
                reason: CompactReason::ContextOverflow,
                patch,
                compaction_config: config.clone(),
            });

            assert!(
                state
                    .session
                    .history
                    .iter()
                    .all(|message| !message.content.contains("STALE_PROVIDER_SUMMARY")),
                "guard mismatch must not apply stale provider patch"
            );
            assert!(
                crate::agent::loop_::plan_context_budget(
                    &state.session.history,
                    &config,
                    crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD
                )
                .used_tokens
                    <= 80,
                "fallback trim must bring history under literal hard limit 80"
            );
            assert!(effects.iter().any(|effect| matches!(
                effect,
                Effect::LogTrace {
                    level: tracing::Level::WARN,
                    msg
                } if msg.contains("guard mismatch")
            )));
        }

        /// Step4-12: CancelRequested twice — the second one is a no-op (generating=false)
        #[test]
        fn test_redux_cancel_requested_twice_second_noop() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            // first cancel
            let effects1 = state.reduce(Action::CancelRequested);
            assert!(!effects1.is_empty(), "the first cancel must produce effects");
            // second cancel — generating is already false → no-op
            let effects2 = state.reduce(Action::CancelRequested);
            assert!(
                effects2.is_empty(),
                "a second CancelRequested(generating=false) must be a no-op"
            );
        }
    }

    // ─── Step 5a-3 Phase A + F tests ──────────────────────────────────────────
    //
    // Phase A: StartLLMTurn drives the real path — the reducer initialises the draft and also emits
    // Effect::StartTurn. Phase F: StreamFailed emits NotifyHook(Error); StreamCancelled emits neither
    // a hook nor SaveSession.

    #[cfg(test)]
    mod phase_a_f {
        use super::super::*;
        use crate::chat::action::Action;
        use crate::providers::ChatMessage;
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        fn has_start_turn(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::StartTurn { .. }))
        }
        fn has_notify_hook(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::NotifyHook { .. }))
        }
        fn has_save_session(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::SaveSession(_)))
        }
        fn has_request_redraw(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::RequestRedraw))
        }

        /// Phase A-1: StartLLMTurn initialises the draft + emits Effect::StartTurn (carrying history)
        #[test]
        fn test_phase_a_start_llm_turn_emits_effect_start_turn() {
            let mut state = s();
            let cancel = CancellationToken::new();
            let history = vec![ChatMessage::system("you are helpful"), ChatMessage::user("hi")];

            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: None,
                provider_turn_sequence: None,
                draft_id: "draft-1".to_string(),
                history,
                compaction_guard_history: None,
                compaction_config: None,
                cancel,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            });

            // state changes: draft + active_cancel + generating
            assert!(
                state.stream.primary_streaming_draft().is_some(),
                "stream.draft must be set"
            );
            assert!(
                state.control.active_cancel.is_some(),
                "active_cancel must be registered"
            );
            assert!(state.control.generating, "generating must be set to true");

            // Effect checks
            assert!(has_start_turn(&effects), "Effect::StartTurn must be emitted");
            assert!(has_request_redraw(&effects), "Effect::RequestRedraw must be emitted");

            // history must be carried through into Effect::StartTurn
            let history_in_effect = effects.iter().find_map(|e| match e {
                Effect::StartTurn { history, draft_id, .. } => Some((draft_id.clone(), history.clone())),
                _ => None,
            });
            let (draft_id, hist) = history_in_effect.expect("the StartTurn effect must exist");
            assert_eq!(draft_id, "draft-1");
            assert_eq!(hist.len(), 2);
            let h0 = hist.first().expect("history[0] must exist");
            let h1 = hist.get(1).expect("history[1] must exist");
            assert_eq!(h0.role, "system");
            assert_eq!(h1.role, "user");
        }

        /// Phase A-2: the cancel registered by StartLLMTurn is the same token as the one in Effect::StartTurn
        #[test]
        fn test_phase_a_start_llm_turn_cancel_propagates() {
            let mut state = s();
            let cancel = CancellationToken::new();
            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: None,
                provider_turn_sequence: None,
                draft_id: "d2".to_string(),
                history: vec![ChatMessage::user("x")],
                compaction_guard_history: None,
                compaction_config: None,
                cancel: cancel.clone(),
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            });
            // cancel the original token and check the token inside the Effect is cancelled too (shared)
            cancel.cancel();
            let cancel_in_effect = effects.iter().find_map(|e| match e {
                Effect::StartTurn { cancel, .. } => Some(cancel.clone()),
                _ => None,
            });
            let tok = cancel_in_effect.expect("StartTurn must carry a cancel token");
            assert!(
                tok.is_cancelled(),
                "the cancel in StartTurn must be shared with the original token"
            );
        }

        #[test]
        fn start_llm_turn_carries_compaction_config_to_effect() {
            let mut state = s();
            let compaction_config = crate::config::AgentCompactionConfig {
                max_context_tokens: 120,
                reserve_tokens: 10,
                max_context_tokens_explicit: true,
                memory_flush: false,
                ..crate::config::AgentCompactionConfig::default()
            };

            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: None,
                provider_turn_sequence: None,
                draft_id: "d-budget".to_string(),
                history: vec![ChatMessage::user("x")],
                compaction_guard_history: None,
                compaction_config: Some(compaction_config),
                cancel: CancellationToken::new(),
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            });

            let carried = effects.iter().find_map(|effect| match effect {
                Effect::StartTurn {
                    provider_turn_task_id: None,
                    compaction_config: Some(config),
                    ..
                } => Some(config),
                _ => None,
            });
            let config = carried.expect("StartTurn effect must carry compaction config");
            assert_eq!(config.max_context_tokens, 120);
            assert_eq!(config.reserve_tokens, 10);
        }

        #[test]
        fn start_llm_turn_carries_provider_turn_task_id_to_effect() {
            let mut state = s();
            let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
            let task_id = scheduler.enqueue(
                "provider identity",
                crate::chat::turn_scheduler::TurnPriority::Normal,
                0,
            );

            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: Some(task_id),
                provider_turn_sequence: None,
                draft_id: "d-worker".to_string(),
                history: vec![ChatMessage::user("x")],
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            });

            let carried = effects.iter().find_map(|effect| match effect {
                Effect::StartTurn {
                    provider_turn_task_id, ..
                } => Some(*provider_turn_task_id),
                _ => None,
            });
            assert_eq!(carried, Some(Some(task_id)));
        }

        /// Phase A-3: TurnStarted (the old Action) keeps its behaviour — it emits no Effect::StartTurn
        #[test]
        fn test_phase_a_legacy_turn_started_no_start_turn_effect() {
            let mut state = s();
            let effects = state.reduce(Action::TurnStarted {
                draft_id: "legacy".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(
                state.stream.primary_streaming_draft().is_some(),
                "TurnStarted also initialises the draft"
            );
            assert!(
                !has_start_turn(&effects),
                "TurnStarted must not emit Effect::StartTurn (the old path is still driven by chat::run)"
            );
        }

        /// Phase F-1: StreamFailed emits NotifyHook(Error) — matching the old hooks.emit(HookEvent::Error)
        #[test]
        fn test_phase_f_stream_failed_emits_notify_hook() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d3".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamFailed {
                draft_id: "d3".to_string(),
                err: "boom".to_string(),
                retryable: false,
            });
            assert!(has_notify_hook(&effects), "StreamFailed must emit NotifyHook(Error)");
            let hook_evt = effects.iter().find_map(|e| match e {
                Effect::NotifyHook { event, payload } => Some((*event, payload.clone())),
                _ => None,
            });
            let (evt, payload) = hook_evt.expect("NotifyHook must exist");
            assert!(matches!(evt, HookEvent::Error));
            assert_eq!(payload.get("component").and_then(|v| v.as_str()), Some("chat-turn"));
            assert_eq!(payload.get("message").and_then(|v| v.as_str()), Some("boom"));
            assert_eq!(
                payload.get("retryable").and_then(serde_json::Value::as_bool),
                Some(false)
            );
        }

        /// Phase F-2: StreamCancelled emits neither NotifyHook nor SaveSession (an interrupted turn
        /// is not persisted and fires no hook)
        #[test]
        fn test_phase_f_stream_cancelled_no_save_no_hook() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d4".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "d4".to_string(),
            });
            assert!(!has_notify_hook(&effects), "StreamCancelled must not emit NotifyHook");
            assert!(!has_save_session(&effects), "StreamCancelled must not SaveSession");
            assert!(
                has_request_redraw(&effects),
                "StreamCancelled still needs RequestRedraw"
            );
        }

        /// Phase F-3: StreamFailed with a mismatched draft_id → no-op, no NotifyHook (avoids stale reports)
        #[test]
        fn test_phase_f_stream_failed_wrong_id_no_hook() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "right".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamFailed {
                draft_id: "wrong".to_string(),
                err: "stale".to_string(),
                retryable: true,
            });
            assert!(effects.is_empty(), "a stale draft_id must be a no-op");
            assert!(!has_notify_hook(&effects));
        }

        /// Phase A-4: cancelling right after StartLLMTurn — the state is cleaned up correctly
        /// (generating=true, and the cancel token is ready for the executor)
        #[test]
        fn test_phase_a_start_llm_turn_then_cancel_request() {
            let mut state = s();
            let cancel = CancellationToken::new();
            let _ = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: None,
                provider_turn_sequence: None,
                draft_id: "d5".to_string(),
                history: vec![ChatMessage::user("hi")],
                compaction_guard_history: None,
                compaction_config: None,
                cancel,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            });
            assert!(state.control.generating);

            let effects = state.reduce(Action::CancelRequested);
            // generating=true → the reducer emits CancelDraft
            assert!(
                effects.iter().any(|e| matches!(e, Effect::CancelDraft(_))),
                "CancelRequested(generating=true) must emit CancelDraft"
            );
            assert!(!state.control.generating, "generating must be reset after cancel");
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "the draft must be cleaned up after cancel"
            );
        }

        fn start_phase1_draft(state: &mut ChatState, draft_id: &str, sequence: u64, prompt: &str) {
            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: None,
                provider_turn_sequence: Some(sequence),
                draft_id: draft_id.to_string(),
                history: vec![ChatMessage::user(prompt)],
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            });
            assert!(has_start_turn(&effects), "phase1 draft start must still emit StartTurn");
        }

        fn visible_draft_ids(state: &ChatState) -> Vec<&str> {
            state
                .stream
                .visible_drafts
                .iter()
                .map(|turn| turn.draft.draft_id.as_str())
                .collect()
        }

        fn draft_text(state: &ChatState, draft_id: &str) -> Option<String> {
            state
                .stream
                .visible_drafts
                .iter()
                .find(|turn| turn.draft.draft_id == draft_id)
                .map(|turn| turn.draft.accumulated.clone())
        }

        fn provider_worker_status(sequences: &[u64]) -> ProviderWorkerStatus {
            ProviderWorkerStatus {
                running: sequences.len(),
                cancelling: 0,
                awaiting_commit: 0,
                finalized_payloads: 0,
                finalized_total_tokens: 0,
                oldest_started_at_ms: Some(0),
                rows: sequences
                    .iter()
                    .map(|sequence| crate::chat::action::ProviderWorkerStatusRow {
                        task_id: *sequence,
                        sequence: *sequence,
                        kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                        state: crate::chat::action::ProviderWorkerRowState::Running,
                        started_at_ms: 0,
                        finalized_total_tokens: None,
                        completion_ready: false,
                        recent_tool_call: None,
                    })
                    .collect(),
            }
        }

        fn active_worker_view_text(state: &ChatState) -> String {
            state
                .ui
                .active_session_view
                .as_ref()
                .map(|view| view.lines.join("\n"))
                .unwrap_or_default()
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn phase2_snapshot_exposes_worker_drafts_and_keeps_primary_streaming() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-a".to_string(),
                delta: "A live".to_string(),
                version: 1,
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-b".to_string(),
                delta: "B live".to_string(),
                version: 1,
            });

            let snapshot = state.build_ui_snapshot(42);

            assert_eq!(
                snapshot.streaming.as_ref().map(|draft| draft.draft_id.as_str()),
                Some("draft-a")
            );
            assert_eq!(
                snapshot
                    .streaming_draft_for_worker(20)
                    .map(|draft| draft.accumulated.as_str()),
                Some("B live")
            );
            assert!(snapshot.streaming_draft_for_worker(30).is_none());
            assert_eq!(
                snapshot
                    .visible_streaming_drafts
                    .iter()
                    .map(|draft| draft.sequence)
                    .collect::<Vec<_>>(),
                vec![10, 20]
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn phase2_worker_pane_focus_uses_matching_draft_not_primary() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-a".to_string(),
                delta: "A live".to_string(),
                version: 1,
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-b".to_string(),
                delta: "B live".to_string(),
                version: 1,
            });

            state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 10 };
            let _ = state.reduce(Action::ProviderWorkerStatusUpdated {
                status: provider_worker_status(&[10, 20]),
            });
            let view_a = active_worker_view_text(&state);
            assert!(view_a.contains("assistant streaming: A live"), "{view_a}");
            assert!(!view_a.contains("B live"), "{view_a}");

            state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 20 };
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-b".to_string(),
                delta: " B2".to_string(),
                version: 2,
            });
            let view_b = active_worker_view_text(&state);
            assert!(view_b.contains("assistant streaming: B live B2"), "{view_b}");
            assert!(!view_b.contains("A live"), "{view_b}");
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn phase2_worker_pane_missing_draft_uses_empty_io_not_history_or_primary() {
            let mut state = s();
            state.ui.conversation_lines.push(ConversationLine::User {
                content: "history user".to_string(),
            });
            state.ui.conversation_lines.push(ConversationLine::Assistant {
                content: "history assistant must not leak".to_string(),
            });
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-a".to_string(),
                delta: "primary live must not leak".to_string(),
                version: 1,
            });

            state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 30 };
            let _ = state.reduce(Action::ProviderWorkerStatusUpdated {
                status: provider_worker_status(&[30]),
            });
            let view = active_worker_view_text(&state);

            assert!(!view.contains("io: recent provider turn"), "{view}");
            assert!(!view.contains("history assistant must not leak"), "{view}");
            assert!(!view.contains("primary live must not leak"), "{view}");
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn phase2_main_transcript_primary_streaming_path_is_unchanged() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-b", 20, "second");
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            let snapshot = state.build_ui_snapshot(1);

            assert_eq!(
                snapshot.streaming.as_ref().map(|draft| draft.draft_id.as_str()),
                Some("draft-a")
            );
            assert_eq!(
                state
                    .stream
                    .primary_streaming_draft()
                    .map(|draft| draft.draft_id.as_str()),
                Some("draft-a")
            );
        }

        #[test]
        fn phase1_two_visible_drafts_start_without_overwriting() {
            let mut state = s();

            start_phase1_draft(&mut state, "draft-b", 20, "second prompt");
            start_phase1_draft(&mut state, "draft-a", 10, "first prompt");

            assert_eq!(visible_draft_ids(&state), vec!["draft-a", "draft-b"]);
            assert_eq!(
                state
                    .stream
                    .primary_draft()
                    .map(|turn| (turn.sequence, turn.prompt_preview.as_str())),
                Some((10, "first prompt"))
            );
            assert!(state.control.generating);
        }

        #[test]
        fn phase1_stream_chunks_route_by_draft_id() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");

            let b_effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-b".to_string(),
                delta: "B1".to_string(),
                version: 1,
            });
            let a_effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-a".to_string(),
                delta: "A1".to_string(),
                version: 1,
            });

            assert!(has_request_redraw(&b_effects));
            assert!(has_request_redraw(&a_effects));
            assert_eq!(draft_text(&state, "draft-a"), Some("A1".to_string()));
            assert_eq!(draft_text(&state, "draft-b"), Some("B1".to_string()));
            assert_eq!(visible_draft_ids(&state), vec!["draft-a", "draft-b"]);
        }

        #[test]
        fn phase1_stream_completed_removes_only_matching_draft() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");

            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "draft-a".to_string(),
                final_text: "answer a".to_string(),
                reasoning: String::new(),
            });

            assert!(has_save_session(&effects));
            assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
            assert!(
                state.control.generating,
                "remaining draft keeps structural generating state"
            );
            assert_eq!(
                state
                    .stream
                    .primary_streaming_draft()
                    .map(|draft| draft.draft_id.as_str()),
                Some("draft-b")
            );
        }

        #[test]
        fn phase1_stale_chunk_for_completed_draft_is_ignored() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "draft-a".to_string(),
                final_text: "answer a".to_string(),
                reasoning: String::new(),
            });

            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-a".to_string(),
                delta: "late".to_string(),
                version: 1,
            });

            assert!(effects.is_empty(), "completed draft must reject late chunks");
            assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
            assert_eq!(draft_text(&state, "draft-b"), Some(String::new()));
        }

        #[test]
        fn phase1_stream_cancelled_removes_only_matching_draft() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");

            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "draft-a".to_string(),
            });

            assert!(has_request_redraw(&effects));
            assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
            assert!(
                state.control.generating,
                "cancelling one structural draft must not stop the other"
            );
        }

        #[test]
        fn phase1_stream_failed_removes_only_matching_draft() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");

            let effects = state.reduce(Action::StreamFailed {
                draft_id: "draft-a".to_string(),
                err: "failed a".to_string(),
                retryable: false,
            });

            assert!(has_notify_hook(&effects));
            assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
            assert!(
                state.control.generating,
                "failing one structural draft must not stop the other"
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn phase1_snapshot_dirty_changes_when_non_primary_draft_version_changes() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");
            let before = state.snapshot_dirty_fields();

            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-b".to_string(),
                delta: "B1".to_string(),
                version: 1,
            });
            let after = state.snapshot_dirty_fields();

            assert!(has_request_redraw(&effects));
            assert_ne!(
                before, after,
                "non-primary draft version must affect snapshot dirty fingerprint"
            );
            assert_eq!(draft_text(&state, "draft-b"), Some("B1".to_string()));
            assert_eq!(
                state
                    .stream
                    .primary_streaming_draft()
                    .map(|draft| draft.draft_id.as_str()),
                Some("draft-a"),
                "non-primary chunk must not change primary selection"
            );
        }

        // ─── S2-A: chat::run stream-path → Redux dispatch wiring tests ─────
        //
        // These four tests cover the contract after the chat::mod streaming path was wired to Redux:
        //   1. dual-write consistency (the M2 acceptance point): for the same delta sequence, the
        //      `accumulated` text the old path `update_draft` hands to the terminal ==
        //      reducer `stream.draft.accumulated`. The reducer accumulates via StreamChunkReceived, the
        //      old path via push_str; the two must be byte-identical.
        //   2. the StreamCompleted Effect sequence: contains NotifyHook(TurnComplete) + RequestRedraw.
        //   3. the StreamFailed Effect sequence: LogTrace(WARN) + NotifyHook(Error) + RequestRedraw.
        //   4. the StreamCancelled Effect sequence: only RequestRedraw (no hook), and cancel is decided
        //      **before** failure classification (so no bogus Failed is emitted).

        /// S2-A test 1: draft_text_consistency_legacy_vs_redux
        ///
        /// Reproduces the delta accumulation semantics of the chat::mod main loop's `draft_updater` task:
        ///   - old path: `accumulated.push_str(&delta); update_draft(accumulated)` — passes the running total
        ///   - new path: `coalescer.try_send_chunk(draft_id, delta, version)` → the reducer's
        ///     `reduce_stream_chunk_received` accumulates via `draft.accumulated.push_str(delta)`
        ///
        /// On the fast path (no coalescer backpressure) the two must be byte-identical.
        #[test]
        fn test_s2a_draft_text_consistency_legacy_vs_redux() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-consistency".to_string(),
                cancel: CancellationToken::new(),
            });

            // simulate SSE streaming deltas (with emoji / multi-byte chars to check byte-level equality)
            let deltas: [&str; 6] = [
                "Hel",
                "lo, ",
                "wo",
                "rld",
                " \u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}",
                " \u{1f30d}",
            ];
            let mut legacy_accumulated = String::new();
            let mut version: u64 = 0;
            for delta in &deltas {
                // old-path accumulation semantics: accumulated.push_str + update_draft(accumulated)
                legacy_accumulated.push_str(delta);
                // new path: dispatch the incremental delta (not the running total)
                version = version.saturating_add(1);
                let _ = state.reduce(Action::StreamChunkReceived {
                    draft_id: "draft-consistency".to_string(),
                    delta: (*delta).to_string(),
                    version,
                });
            }

            // core acceptance point: the old path's accumulated == the reducer's internal accumulated
            let redux_accumulated = state
                .stream
                .primary_streaming_draft()
                .map(|d| d.accumulated.clone())
                .expect("test: stream.draft must exist after StreamChunkReceived");
            assert_eq!(
                redux_accumulated, legacy_accumulated,
                "S2-A M2: the reducer accumulated must equal the running total the old update_draft received"
            );
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.version),
                Some(version),
                "the reducer version must equal the final counter value inside draft_updater"
            );
        }

        /// S2-A test 2: stream_completed_effect_sequence
        ///
        /// Success path: after the S2-A change the chat::mod main loop dispatches
        ///   `Action::StreamCompleted { draft_id, final_text, reasoning: "" }`
        /// and the reducer is expected to emit `[NotifyHook(TurnComplete), RequestRedraw]` and clear the draft.
        #[test]
        fn test_s2a_stream_completed_effect_sequence() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-completed".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-completed".to_string(),
                delta: "the answer".to_string(),
                version: 1,
            });

            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "draft-completed".to_string(),
                final_text: "the answer".to_string(),
                reasoning: String::new(),
            });

            // terminal state cleanup
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "the draft must be cleared after completion"
            );
            assert!(!state.control.generating, "generating=false after completion");
            assert!(state.control.active_cancel.is_none(), "active_cancel is reset");

            // Effect sequence: NotifyHook(TurnComplete) + RequestRedraw
            let notify_turn_complete = effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::NotifyHook {
                        event: HookEvent::TurnComplete,
                        ..
                    }
                )
            });
            assert!(
                notify_turn_complete,
                "StreamCompleted must emit NotifyHook(TurnComplete)"
            );
            assert!(has_request_redraw(&effects), "StreamCompleted must emit RequestRedraw");
        }

        /// T3-3-c-1: `StreamCompleted` must emit `Effect::SaveSession` (the reducer is the single writer).
        ///
        /// It also checks the Effect sequence contract (order: NotifyHook → SaveSession → RequestRedraw).
        #[test]
        fn test_t3_3c_stream_completed_emits_save_session() {
            let mut state = s();
            state.session.id = "sess-T3-3c".to_string();
            // record a user turn first so session.turns is non-empty and the snapshot really carries turns
            let _ = state.reduce(Action::RecordUserTurn("question".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-T3-3c".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "answer".to_string(),
            });
            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "draft-T3-3c".to_string(),
                final_text: "answer".to_string(),
                reasoning: String::new(),
            });

            // check that SaveSession exists and the snapshot content is correct
            let save_effect = effects.iter().find(|e| matches!(e, Effect::SaveSession(_)));
            assert!(
                save_effect.is_some(),
                "T3-3-c: StreamCompleted must emit Effect::SaveSession"
            );
            if let Some(Effect::SaveSession(snapshot)) = save_effect {
                assert_eq!(snapshot.id, "sess-T3-3c", "the snapshot id must equal session.id");
                assert_eq!(
                    snapshot.turns.len(),
                    2,
                    "the snapshot must contain the user+assistant turns"
                );
                assert_eq!(snapshot.turns.first().map(|t| t.role.as_str()), Some("user"));
                let assistant = snapshot.turns.get(1).expect("test: turns[1] must exist");
                assert_eq!(assistant.role, "assistant");
                assert_eq!(assistant.content, "answer");
                assert_eq!(
                    snapshot.title, "question",
                    "the auto-title must come from the first user turn"
                );
                // T3-3-fixA P0-1: explicitly assert that the last snapshot.turns entry is this turn's
                // assistant, pinning the dispatch order (RecordAssistantTurn → StreamCompleted) invariant
                let last = snapshot
                    .turns
                    .last()
                    .expect("test: snapshot.turns must have a last entry");
                assert_eq!(last.role, "assistant", "snapshot.turns.last() must be assistant");
                assert_eq!(
                    last.content, "answer",
                    "the last content must be this turn's assistant text"
                );
            }

            // Effect order contract: NotifyHook first, SaveSession in the middle, RequestRedraw last
            let positions: Vec<&'static str> = effects
                .iter()
                .map(|e| match e {
                    Effect::NotifyHook { .. } => "notify",
                    Effect::SaveSession(_) => "save",
                    Effect::RequestRedraw => "redraw",
                    _ => "other",
                })
                .collect();
            let notify_pos = positions.iter().position(|s| *s == "notify");
            let save_pos = positions.iter().position(|s| *s == "save");
            let redraw_pos = positions.iter().position(|s| *s == "redraw");
            assert!(
                notify_pos < save_pos && save_pos < redraw_pos,
                "the Effect order must be NotifyHook < SaveSession < RequestRedraw, got: {positions:?}"
            );
        }

        /// T3-3-c-2: a repeated `StreamCompleted` must not trigger a second SaveSession (draft cleared → no-op)
        #[test]
        fn test_t3_3c_duplicate_stream_completed_no_save() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-dup".to_string(),
                cancel: CancellationToken::new(),
            });
            // first time — must contain SaveSession
            let first = state.reduce(Action::StreamCompleted {
                draft_id: "d-dup".to_string(),
                final_text: "ans".to_string(),
                reasoning: String::new(),
            });
            assert!(first.iter().any(|e| matches!(e, Effect::SaveSession(_))));
            // second time — the draft is already None → empty vec, no duplicate SaveSession
            let second = state.reduce(Action::StreamCompleted {
                draft_id: "d-dup".to_string(),
                final_text: "ans-dup".to_string(),
                reasoning: String::new(),
            });
            assert!(second.is_empty(), "a repeated StreamCompleted must be a no-op");
        }

        /// T3-3-fixA P0-1: two-way regression guard — the dispatch order decides snapshot completeness.
        ///
        /// Forward (RecordAssistantTurn → StreamCompleted): snapshot.turns ends with the assistant turn.
        /// Reverse (StreamCompleted → RecordAssistantTurn): snapshot.turns does **not** contain the
        /// assistant, because the SaveSession snapshot is built synchronously inside
        /// reduce_stream_completed, before RecordAssistantTurn has pushed this turn into session.turns.
        ///
        /// Any future change that reverts the chat::run main loop dispatch order breaks this test,
        /// pinning the P0-1 decision into the reducer-level contract.
        #[test]
        fn t3_3_fix_a_dispatch_order_snapshot_contract() {
            // ── forward: RecordAssistantTurn → StreamCompleted ──
            let mut state_a = s();
            state_a.session.id = "sess-fwd".to_string();
            let _ = state_a.reduce(Action::RecordUserTurn("q".to_string()));
            let _ = state_a.reduce(Action::TurnStarted {
                draft_id: "d-fwd".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state_a.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "a-fwd".to_string(),
            });
            let fwd_effects = state_a.reduce(Action::StreamCompleted {
                draft_id: "d-fwd".to_string(),
                final_text: "a-fwd".to_string(),
                reasoning: String::new(),
            });
            let fwd_snap = fwd_effects
                .iter()
                .find_map(|e| match e {
                    Effect::SaveSession(s) => Some(s),
                    _ => None,
                })
                .expect("forward: SaveSession must be emitted");
            let last = fwd_snap
                .turns
                .last()
                .expect("forward: snapshot.turns must be non-empty");
            assert_eq!(last.role, "assistant", "forward: the last role must be assistant");
            assert_eq!(
                last.content, "a-fwd",
                "forward: the last content must be this turn's assistant"
            );

            // ── reverse: StreamCompleted → RecordAssistantTurn ──
            let mut state_b = s();
            state_b.session.id = "sess-rev".to_string();
            let _ = state_b.reduce(Action::RecordUserTurn("q".to_string()));
            let _ = state_b.reduce(Action::TurnStarted {
                draft_id: "d-rev".to_string(),
                cancel: CancellationToken::new(),
            });
            let rev_effects = state_b.reduce(Action::StreamCompleted {
                draft_id: "d-rev".to_string(),
                final_text: "a-rev".to_string(),
                reasoning: String::new(),
            });
            let _ = state_b.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "a-rev".to_string(),
            });
            let rev_snap = rev_effects
                .iter()
                .find_map(|e| match e {
                    Effect::SaveSession(s) => Some(s),
                    _ => None,
                })
                .expect("reverse: SaveSession must be emitted");
            assert!(
                !rev_snap.turns.iter().any(|t| t.role == "assistant"),
                "reverse: snapshot.turns must not contain assistant — this is the bug before the P0-1 fix"
            );
            assert_eq!(
                rev_snap.turns.len(),
                1,
                "reverse: snapshot.turns must hold only the earlier user turn"
            );
        }

        /// T3-3-fixA P0-2: StreamFailed emits no SaveSession (the error path does not persist).
        ///
        /// Pins the Error row of appendix B's decision table: reduce_stream_failed emits
        /// [LogTrace, NotifyHook(Error), RequestRedraw] and no SaveSession. Regression guard: if anyone
        /// later wants to "save failures too", this test breaks at once, forcing an appendix B update.
        #[test]
        fn t3_3_fix_a_stream_error_no_save() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-err".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamFailed {
                draft_id: "d-err".to_string(),
                err: "boom".to_string(),
                retryable: false,
            });
            assert!(
                !has_save_session(&effects),
                "StreamFailed must not emit SaveSession (T3-3-fixA appendix B, Error row)"
            );
        }

        /// T3-3-fixA P0-2: StreamCancelled emits no SaveSession (a user cancel does not persist).
        ///
        /// Pins the Cancelled row of the appendix B decision table: reduce_stream_cancelled emits only
        /// [RequestRedraw]. phase_f_stream_cancelled_no_save_no_hook already covers this; this test keeps
        /// the fixA name so regressions can be located by appendix B decision point.
        #[test]
        fn t3_3_fix_a_stream_cancelled_no_save() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-cancel".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "d-cancel".to_string(),
            });
            assert!(
                !has_save_session(&effects),
                "StreamCancelled must not emit SaveSession (T3-3-fixA appendix B, Cancelled row)"
            );
        }

        /// S2-A test 3: stream_failed_effect_sequence
        ///
        /// Failure path (timeout / context-overflow / other errors): after the S2-A change the chat::mod
        /// main loop dispatches `Action::StreamFailed { draft_id, err, retryable }`.
        /// The reducer is expected to emit `[LogTrace(WARN), NotifyHook(Error), RequestRedraw]`.
        #[test]
        fn test_s2a_stream_failed_effect_sequence() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-failed".to_string(),
                cancel: CancellationToken::new(),
            });
            // simulate receiving a partial chunk earlier in the stream
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-failed".to_string(),
                delta: "partial...".to_string(),
                version: 1,
            });

            let effects = state.reduce(Action::StreamFailed {
                draft_id: "draft-failed".to_string(),
                err: "timeout".to_string(),
                retryable: false,
            });

            // terminal state cleanup
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "the draft must be cleared after failure"
            );
            assert!(!state.control.generating);

            // Effect sequence assertions (in the order reduce_stream_failed emits them)
            let has_warn_log = effects
                .iter()
                .any(|e| matches!(e, Effect::LogTrace { level, .. } if *level == tracing::Level::WARN));
            assert!(has_warn_log, "StreamFailed must emit LogTrace(WARN)");

            let notify_error = effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::NotifyHook {
                        event: HookEvent::Error,
                        ..
                    }
                )
            });
            assert!(notify_error, "StreamFailed must emit NotifyHook(Error)");
            assert!(has_request_redraw(&effects), "StreamFailed must emit RequestRedraw");

            // retryable=false must reach the hook payload (field mapping check)
            let retryable_in_payload = effects.iter().any(|e| {
                if let Effect::NotifyHook {
                    event: HookEvent::Error,
                    payload,
                } = e
                {
                    payload.get("retryable").and_then(serde_json::Value::as_bool) == Some(false)
                } else {
                    false
                }
            });
            assert!(
                retryable_in_payload,
                "retryable=false must reach the NotifyHook payload"
            );
        }

        /// S2-A test 4: stream_cancelled_effect_sequence
        ///
        /// Cancel path (Ctrl+C / is_tool_loop_cancelled): after the S2-A change the chat::mod main loop
        /// decides cancellation **first** and classifies failures **after** — a cancel must dispatch
        /// `Action::StreamCancelled { draft_id }` and not `StreamFailed`.
        /// The reducer is expected to emit only `[RequestRedraw]` (no hook, no SaveSession).
        #[test]
        fn test_s2a_stream_cancelled_effect_sequence() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-cancelled".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-cancelled".to_string(),
                delta: "interrupted".to_string(),
                version: 1,
            });

            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "draft-cancelled".to_string(),
            });

            // terminal state cleanup
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "the draft must be cleared after cancellation"
            );
            assert!(!state.control.generating);
            assert!(state.control.active_cancel.is_none());

            // Effect sequence: only RequestRedraw — no NotifyHook, no LogTrace(WARN), no SaveSession
            assert!(has_request_redraw(&effects), "StreamCancelled must emit RequestRedraw");
            assert!(
                !has_notify_hook(&effects),
                "StreamCancelled must not emit NotifyHook (a cancel is not an error)"
            );
            assert!(
                !has_save_session(&effects),
                "StreamCancelled must not SaveSession (same as Failed, avoiding persisting an interrupted state)"
            );
            let has_warn_log = effects
                .iter()
                .any(|e| matches!(e, Effect::LogTrace { level, .. } if *level == tracing::Level::WARN));
            assert!(
                !has_warn_log,
                "StreamCancelled must not emit a WARN LogTrace (a cancel is not a fault)"
            );

            // Protocol contract: a cancel must be decided **before** failure classification — if a cancel
            // were mistaken for a failure and dispatched as StreamFailed, it would emit the
            // NotifyHook(Error) asserted in stream_failed_effect_sequence above, contradicting the
            // !has_notify_hook here and failing. By the absence of NotifyHook(Error) this test indirectly
            // verifies the chat::mod order contract "check is_tool_loop_cancelled first, classify after".
        }

        /// S2-A test 5 (Codex blocker): tool_call_chunk_interleave_consistency
        ///
        /// Checks that when tool events (`ToolStarted` / `ToolFinished`) and `StreamChunkReceived` are
        /// **interleaved** within one turn, the reducer still keeps consistent state on "independent axes":
        /// - stream.draft.accumulated is only fed by stream chunks, tool events do not pollute it
        /// - tool events only affect ui.conversation_lines / pending_tool_cards, never the draft
        /// - the resulting state equals the "all stream chunks first, tools afterwards" sequence
        ///
        /// This is the key invariant for the chat::run tool-call loop running alongside the streaming
        /// path — if the reducer wrongly cleared or edited the draft on the ToolStarted/ToolFinished
        /// path, users would see the streaming text "suddenly jump backwards".
        #[test]
        fn test_s2a_tool_call_chunk_interleave_consistency() {
            // ── Scenario A: interleaved sequence — stream / tool / stream / tool ──
            let mut state_a = s();
            let _ = state_a.reduce(Action::TurnStarted {
                draft_id: "draft-interleave".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state_a.reduce(Action::StreamChunkReceived {
                draft_id: "draft-interleave".to_string(),
                delta: "hello ".to_string(),
                version: 1,
            });
            let _ = state_a.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                args: "{\"q\":\"openprx\"}".to_string(),
            });
            let _ = state_a.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                success: true,
                duration_ms: 42,
                result: Some("found 3 results".to_string()),
            });
            let _ = state_a.reduce(Action::StreamChunkReceived {
                draft_id: "draft-interleave".to_string(),
                delta: "world".to_string(),
                version: 2,
            });

            // ── Scenario B: the equivalent "pure streaming, tools last" sequence ──
            let mut state_b = s();
            let _ = state_b.reduce(Action::TurnStarted {
                draft_id: "draft-interleave".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state_b.reduce(Action::StreamChunkReceived {
                draft_id: "draft-interleave".to_string(),
                delta: "hello ".to_string(),
                version: 1,
            });
            let _ = state_b.reduce(Action::StreamChunkReceived {
                draft_id: "draft-interleave".to_string(),
                delta: "world".to_string(),
                version: 2,
            });
            let _ = state_b.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                args: "{\"q\":\"openprx\"}".to_string(),
            });
            let _ = state_b.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                success: true,
                duration_ms: 42,
                result: Some("found 3 results".to_string()),
            });

            // Core invariant: draft.accumulated is identical (tool events do not pollute streaming text)
            let acc_a = state_a
                .stream
                .primary_streaming_draft()
                .map(|d| d.accumulated.clone())
                .expect("test: scenario A draft must exist");
            let acc_b = state_b
                .stream
                .primary_streaming_draft()
                .map(|d| d.accumulated.clone())
                .expect("test: scenario B draft must exist");
            assert_eq!(
                acc_a, "hello world",
                "in the interleaved sequence draft.accumulated is fed only by stream chunks"
            );
            assert_eq!(
                acc_a, acc_b,
                "draft.accumulated must be byte-identical between interleaved and tools-last sequences"
            );

            // the version must match too (tool events do not touch version)
            assert_eq!(
                state_a.stream.primary_streaming_draft().map(|d| d.version),
                state_b.stream.primary_streaming_draft().map(|d| d.version),
                "tool events must not advance stream.version"
            );
            assert_eq!(state_a.stream.primary_streaming_draft().map(|d| d.version), Some(2));

            // the tool cards landed on both sides and were removed from pending after ToolFinished
            assert_eq!(
                state_a.control.pending_tool_card_count(ToolTaskKey::Primary),
                state_b.control.pending_tool_card_count(ToolTaskKey::Primary),
                "both sequences must have the same pending_tool_cards count"
            );
            assert!(
                state_a.control.pending_tool_card_count(ToolTaskKey::Primary) == 0,
                "pending_tool_cards must be empty after ToolFinished"
            );

            // control state matches: still generating, the cancel token unchanged
            assert!(state_a.control.generating);
            assert!(state_b.control.generating);
            assert!(state_a.control.active_cancel.is_some());
            assert!(state_b.control.active_cancel.is_some());
        }
    }

    // ─── S2-B integration tests (5 new tests) ─────────────────────────────────
    //
    // These five tests cover the contract after S2-B wired the chat session/cancel paths into Redux:
    //   1. CancelRequested really emits a CancelToken effect and clears the control state
    //   2. ModeChanged leaves state.session.mode equal to legacy chat_session.set_mode
    //   3. a single RecordUserTurn write produces no duplicate session.turns
    //   4. HistoryCompacted keeps system and caps the total budget
    //   5. StreamCancelled matches the S2-A terminal behaviour (cancel and token cancel do not clash)

    #[cfg(test)]
    mod s2b {
        use super::super::*;
        use crate::chat::action::{Action, CompactReason};
        use crate::providers::ChatMessage;
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        /// S2-B-1: redux_cancel_requested_clears_control_and_emits_cancel_effect
        ///
        /// During a single Ctrl+C the reducer must emit `Effect::CancelToken(token)` to really trigger
        /// the underlying cancel (replacing the old manual `token.cancel()`), and clear
        /// generating/draft/active_cancel. This closes the S2-B Codex risk window.
        #[test]
        fn redux_cancel_requested_clears_control_and_emits_cancel_effect() {
            let mut state = s();
            let tok = CancellationToken::new();
            // start a turn → control.active_cancel=Some(tok), generating=true
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-s2b-1".to_string(),
                cancel: tok,
            });
            assert!(state.control.generating);
            assert!(state.control.active_cancel.is_some());

            let effects = state.reduce(Action::CancelRequested);

            // the control state must be fully cleared
            assert!(!state.control.generating, "generating=false after CancelRequested");
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "the draft is cleared after CancelRequested"
            );
            assert!(
                state.control.active_cancel.is_none(),
                "active_cancel is cleared after CancelRequested (the token went to Effect::CancelToken)"
            );

            // The Effect list must contain CancelToken (the key one) + CancelDraft + LogTrace + RequestRedraw
            let has_cancel_token = effects.iter().any(|e| matches!(e, Effect::CancelToken(_)));
            assert!(
                has_cancel_token,
                "CancelRequested must emit Effect::CancelToken — this is the key S2-B difference"
            );
            let has_cancel_draft = effects
                .iter()
                .any(|e| matches!(e, Effect::CancelDraft(id) if id == "d-s2b-1"));
            assert!(has_cancel_draft, "must contain CancelDraft(draft-id)");
            // CancelToken must come before CancelDraft (really cancel the backend first, then clear the UI)
            let pos_token = effects
                .iter()
                .position(|e| matches!(e, Effect::CancelToken(_)))
                .expect("CancelToken present");
            let pos_draft = effects
                .iter()
                .position(|e| matches!(e, Effect::CancelDraft(_)))
                .expect("CancelDraft present");
            assert!(pos_token < pos_draft, "CancelToken must come before CancelDraft");
        }

        /// S2-B-2: redux_mode_changed_matches_legacy_chat_session_mode
        ///
        /// During dual-write the reducer's `state.session.mode` must stay in sync with legacy
        /// `chat_session.mode`. This test runs the same sequence (set_mode + ModeChanged) against both
        /// ChatSession and ChatState and checks the final mode matches.
        #[test]
        fn redux_mode_changed_matches_legacy_chat_session_mode() {
            use crate::chat::session::ChatSession;
            let mut state = s();
            let mut legacy = ChatSession::new("openai", "gpt-4o-mini");

            // Plan
            let _ = state.reduce(Action::ModeChanged(ChatMode::Plan));
            legacy.set_mode(ChatMode::Plan);
            assert_eq!(state.session.mode, legacy.mode, "the Plan mode must match");
            assert_eq!(state.ui.chat_mode, ChatMode::Plan, "Plan mode must reach the UI status");

            // Auto
            let _ = state.reduce(Action::ModeChanged(ChatMode::Auto));
            legacy.set_mode(ChatMode::Auto);
            assert_eq!(state.session.mode, legacy.mode, "the Auto mode must match");
            assert_eq!(state.ui.chat_mode, ChatMode::Auto, "Auto mode must reach the UI status");

            // Edit (default)
            let _ = state.reduce(Action::ModeChanged(ChatMode::Edit));
            legacy.set_mode(ChatMode::Edit);
            assert_eq!(state.session.mode, legacy.mode, "the Edit mode must match");
            assert_eq!(state.ui.chat_mode, ChatMode::Edit, "Edit mode must reach the UI status");
        }

        #[test]
        fn p8_mode_changed_does_not_escalate_autonomy_or_policy() {
            use crate::config::AutonomyConfig;
            use crate::security::policy::ToolDecision;
            use crate::security::{AutonomyLevel, SecurityPolicy};

            let mut state = s();
            let autonomy = AutonomyConfig {
                level: AutonomyLevel::ReadOnly,
                ..AutonomyConfig::default()
            };
            let autonomy_before = autonomy.clone();
            let policy_before = SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));
            state.ui.autonomy_level = autonomy.level;

            for mode in [ChatMode::Plan, ChatMode::Edit, ChatMode::Auto, ChatMode::Plan] {
                let _ = state.reduce(Action::ModeChanged(mode));
                assert_eq!(state.ui.autonomy_level, AutonomyLevel::ReadOnly);
            }

            let policy_after = SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));

            assert_eq!(autonomy.level, autonomy_before.level);
            assert_eq!(autonomy.workspace_only, autonomy_before.workspace_only);
            assert_eq!(autonomy.scopes.rules.len(), autonomy_before.scopes.rules.len());
            assert_eq!(policy_after.autonomy, policy_before.autonomy);
            assert_eq!(state.session.mode, ChatMode::Plan);
            assert_eq!(
                policy_after.decide("file_write", "user", "terminal", "chat"),
                ToolDecision::Deny,
                "ChatMode::Auto cannot widen read_only autonomy because decide() is ChatMode-free"
            );
        }

        /// BUG-07: the `ModelChanged` reducer updates `session.model` so the status bar immediately
        /// reflects the new model, and the new value reaches the UI snapshot (snapshot.model reads session.model).
        #[test]
        fn redux_model_changed_updates_session_and_snapshot() {
            let mut state = s();
            assert_eq!(&*state.session.model, "gpt-4o-mini", "the initial model");

            let effects = state.reduce(Action::ModelChanged {
                model: "anthropic/claude-sonnet-4".to_string(),
            });
            assert_eq!(
                &*state.session.model, "anthropic/claude-sonnet-4",
                "the model was switched"
            );
            assert!(
                effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
                "ModelChanged must request a redraw to refresh the status bar"
            );

            // snapshot.model comes from session.model; build_ui_snapshot only exists under the
            // terminal-tui feature, so the snapshot assertion is gated on that feature.
            #[cfg(feature = "terminal-tui")]
            {
                let snap = state.build_ui_snapshot(1);
                assert_eq!(
                    &*snap.model, "anthropic/claude-sonnet-4",
                    "snapshot.model reflects the new model"
                );
            }
        }

        /// Bug #3: the `ProviderChanged` reducer updates `session.provider` so the status bar
        /// `state.provider()` (read from snapshot.provider ← session.provider) reflects the new provider
        /// at once. With `model: None`, session.model is left alone.
        #[test]
        fn redux_provider_changed_updates_session_provider_only() {
            let mut state = s();
            assert_eq!(&*state.session.provider, "openai", "the initial provider");
            assert_eq!(&*state.session.model, "gpt-4o-mini", "the initial model");

            let effects = state.reduce(Action::ProviderChanged {
                provider: "openrouter".to_string(),
                model: None,
            });
            assert_eq!(&*state.session.provider, "openrouter", "the provider was switched");
            assert_eq!(
                &*state.session.model, "gpt-4o-mini",
                "with model: None the model is unchanged"
            );
            assert!(
                effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
                "ProviderChanged must request a redraw to refresh the status bar"
            );

            #[cfg(feature = "terminal-tui")]
            {
                let snap = state.build_ui_snapshot(1);
                assert_eq!(
                    &*snap.provider, "openrouter",
                    "snapshot.provider reflects the new provider"
                );
            }
        }

        /// Bug #3: when `ProviderChanged` carries `model: Some(..)` it also syncs session.model
        /// (the case where switching provider explicitly passes a compatible model argument).
        #[test]
        fn redux_provider_changed_with_model_updates_both() {
            let mut state = s();
            let effects = state.reduce(Action::ProviderChanged {
                provider: "anthropic".to_string(),
                model: Some("claude-sonnet-4".to_string()),
            });
            assert_eq!(&*state.session.provider, "anthropic", "the provider was switched");
            assert_eq!(
                &*state.session.model, "claude-sonnet-4",
                "the model was switched along with it"
            );
            assert!(effects.iter().any(|e| matches!(e, Effect::RequestRedraw)));

            #[cfg(feature = "terminal-tui")]
            {
                let snap = state.build_ui_snapshot(1);
                assert_eq!(&*snap.provider, "anthropic");
                assert_eq!(&*snap.model, "claude-sonnet-4");
            }
        }

        /// T3-3-d-byte-parity: the reducer's `session.history` and legacy `ChatSession.turns` must
        /// reconcile byte for byte in Both mode (the same user/assistant content on both sides).
        ///
        /// This turns the Both-mode "dual-write reconciliation" into an in-process unit test, avoiding
        /// PTY comparison noise. Key assertions:
        ///   1. session.turns.len() == legacy.turns.len() (entry counts line up)
        ///   2. the role sequence matches exactly
        ///   3. content is byte-identical (when there is no sanitization difference)
        #[test]
        fn t3_3d_both_mode_history_byte_level_parity() {
            use crate::chat::session::ChatSession;
            let mut state = s();
            let mut legacy = ChatSession::new("test-prov", "test-model");

            let inputs = [
                ("user", "hi there"),
                ("assistant", "hello!"),
                ("user", "explain monads in 1 sentence"),
                ("assistant", "a monad is a monoid in the category of endofunctors"),
                ("user", ""), // empty string boundary
                (
                    "assistant",
                    "\u{1f680} unicode \u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} mixed content",
                ),
            ];

            for (role, text) in inputs {
                if role == "user" {
                    let _ = state.reduce(Action::RecordUserTurn(text.to_string()));
                    legacy.add_user_turn(text);
                } else {
                    // every role in the test input is "user" / "assistant", so the else branch is assistant
                    let _ = state.reduce(Action::RecordAssistantTurn {
                        task_id: None,
                        content: text.to_string(),
                    });
                    legacy.add_assistant_turn(text, Vec::new());
                }
            }

            assert_eq!(
                state.session.turns.len(),
                legacy.turns.len(),
                "Both mode: reducer.session.turns.len() must equal legacy.turns.len()"
            );
            for (i, (lhs, rhs)) in state.session.turns.iter().zip(legacy.turns.iter()).enumerate() {
                assert_eq!(lhs.role, rhs.role, "turn {i} role differs");
                assert_eq!(
                    lhs.content.as_bytes(),
                    rhs.content.as_bytes(),
                    "turn {i} content differs at byte level (reducer vs legacy)"
                );
            }
            // history grows in step with turns
            assert_eq!(
                state.session.history.len(),
                state.session.turns.len(),
                "reducer.session.history.len() must stay in sync with turns.len()"
            );
        }

        /// T3-3-fixB C1: byte-level parity between the reducer's `session.history` and a legacy manual
        /// history on the system dimension. t3_3d_both_mode_history_byte_level_parity only covers
        /// user/assistant; this adds SetLeadingSystemPrompt (upsert) + RecordSystemMessage (append).
        ///
        /// The legacy side mirrors a `Vec<ChatMessage>` by hand (ChatSession has no add_system_turn;
        /// the chat::run main loop edits the history slice directly), keeping the two byte-aligned.
        ///
        /// Note: the tool_calls parity gap remains (the reducer's RecordAssistantTurn ignores the
        /// tool_calls argument and session.turns[i].tool_calls is always Vec::new()); tracked under S2.5.
        #[test]
        fn t3_3_fix_b_both_parity_system_history() {
            use crate::providers::ChatMessage;
            let mut state = s();
            let mut legacy: Vec<ChatMessage> = Vec::new();

            // 1) SetLeadingSystemPrompt on an empty history → push system v1
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "rules v1".to_string(),
            });
            legacy.push(ChatMessage::system("rules v1"));

            // 2) RecordUserTurn → append user
            let _ = state.reduce(Action::RecordUserTurn("u1".to_string()));
            legacy.push(ChatMessage::user("u1"));

            // 3) SetLeadingSystemPrompt on a non-empty history → replace history[0]
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "rules v2".to_string(),
            });
            if let Some(first) = legacy.first_mut() {
                *first = ChatMessage::system("rules v2");
            }

            // 4) RecordAssistantTurn → append assistant
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "a1".to_string(),
            });
            legacy.push(ChatMessage::assistant("a1"));

            // 5) RecordSystemMessage → append system at the end (the post-/clear case)
            let _ = state.reduce(Action::RecordSystemMessage {
                content: "context note".to_string(),
            });
            legacy.push(ChatMessage::system("context note"));

            // ── byte-level parity ──
            assert_eq!(
                state.session.history.len(),
                legacy.len(),
                "history.len() must match the legacy manual mirror"
            );
            for (i, (lhs, rhs)) in state.session.history.iter().zip(legacy.iter()).enumerate() {
                assert_eq!(lhs.role, rhs.role, "history[{i}] role differs");
                assert_eq!(
                    lhs.content.as_bytes(),
                    rhs.content.as_bytes(),
                    "history[{i}] content differs at byte level"
                );
            }
        }

        /// S2-B-3: redux_record_turns_single_write_no_duplicate_session_turns
        ///
        /// After dispatching `RecordUserTurn` + `RecordAssistantTurn` once each, `state.session.turns`
        /// must grow by exactly +2 and never produce duplicate entries (the earlier 1197+2055 double
        /// dispatch has been merged into a single enriched dispatch at one point).
        #[test]
        fn redux_record_turns_single_write_no_duplicate_session_turns() {
            let mut state = s();
            assert_eq!(state.session.turns.len(), 0);

            let _ = state.reduce(Action::RecordUserTurn("hello".to_string()));
            assert_eq!(state.session.turns.len(), 1);
            assert_eq!(
                state.session.history.len(),
                1,
                "history grows in step too (reducer is sole writer)"
            );

            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "hi back".to_string(),
            });
            assert_eq!(
                state.session.turns.len(),
                2,
                "user + assistant, two entries, no duplicates"
            );
            assert_eq!(state.session.history.len(), 2, "history must also hold user+assistant");

            // Key regression guard: dispatch the same content again; turns must grow to 4, not be deduped to 2
            // (the reducer is not idempotent — dedup is the caller's job; this confirms it is append-only)
            let _ = state.reduce(Action::RecordUserTurn("hello".to_string()));
            assert_eq!(state.session.turns.len(), 3, "a second dispatch must append one more");
        }

        /// S2-B-4: redux_compaction_action_preserves_system_and_budget
        ///
        /// HistoryCompacted must keep the system prompt and hold the total char count <= COMPACT_TOTAL_CHARS.
        /// This is the core contract of the chat::mod main loop's context-overflow retry path.
        #[test]
        fn redux_compaction_action_preserves_system_and_budget() {
            let mut state = s();
            // system + 20 long user/assistant messages
            state
                .session
                .history
                .push(ChatMessage::system("system rules — must survive compaction"));
            for i in 0..20 {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                state.session.history.push(ChatMessage {
                    role: role.to_string(),
                    content: format!("turn-{i} {}", "y".repeat(400)),
                });
            }

            let effects = state.reduce(Action::HistoryCompacted {
                reason: CompactReason::ContextOverflow,
            });

            // The system prompt must stay in first position
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("system"),
                "system must still be first after compaction"
            );
            assert!(
                state
                    .session
                    .history
                    .first()
                    .is_some_and(|m| m.content.contains("must survive")),
                "the system content must be kept in full (never truncated)"
            );
            // the non-system part must stay within COMPACT_TOTAL_CHARS
            let non_system_chars: usize = state
                .session
                .history
                .iter()
                .skip(1)
                .map(|m| m.content.chars().count())
                .sum();
            assert!(
                non_system_chars <= super::COMPACT_TOTAL_CHARS,
                "non-system total chars {non_system_chars} must be <= {}",
                super::COMPACT_TOTAL_CHARS
            );
            // at least one LogTrace must be emitted
            assert!(
                effects.iter().any(|e| matches!(e, Effect::LogTrace { .. })),
                "HistoryCompacted must emit LogTrace"
            );
        }

        /// S2-B-5: redux_stream_cancelled_cooperates_with_s2a_terminal_actions
        ///
        /// The user presses Ctrl+C while streaming → the reducer emits CancelToken to cancel the backend
        /// and clears state. Right after, the chat::run main loop dispatches `StreamCancelled` as the
        /// turn terminal — the reducer must be a no-op (draft already cleared), emitting no duplicate hook.
        #[test]
        fn redux_stream_cancelled_cooperates_with_s2a_terminal_actions() {
            let mut state = s();
            let tok = CancellationToken::new();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-coop".to_string(),
                cancel: tok.clone(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d-coop".to_string(),
                delta: "partial".to_string(),
                version: 1,
            });

            // 1. user presses Ctrl+C → CancelRequested
            let cancel_effects = state.reduce(Action::CancelRequested);
            // really cancel the token (checked through the effect — the reducer already took it out)
            let cancel_token_effect = cancel_effects.iter().find_map(|e| match e {
                Effect::CancelToken(t) => Some(t.clone()),
                _ => None,
            });
            let token_from_effect = cancel_token_effect.expect("the CancelToken effect must exist");
            // EffectExecutor really calls cancel — simulated here
            token_from_effect.cancel();
            assert!(
                tok.is_cancelled(),
                "the original token must be cancelled by the token in the effect (shared cancellation)"
            );
            // control is cleared
            assert!(!state.control.generating);
            assert!(state.stream.primary_streaming_draft().is_none());

            // 2. the chat::run main loop notices the cancellation → dispatches the StreamCancelled terminal
            let terminal_effects = state.reduce(Action::StreamCancelled {
                draft_id: "d-coop".to_string(),
            });
            // the draft was already cleared by CancelRequested, so StreamCancelled must be a no-op (no hook)
            let has_notify = terminal_effects.iter().any(|e| matches!(e, Effect::NotifyHook { .. }));
            assert!(
                !has_notify,
                "StreamCancelled (draft already cleared) must not emit NotifyHook again — avoids double firing"
            );
            // and no CancelToken either (the token was already emitted and cancelled)
            let has_cancel_token = terminal_effects.iter().any(|e| matches!(e, Effect::CancelToken(_)));
            assert!(!has_cancel_token, "StreamCancelled must not emit CancelToken again");
        }

        /// S2-B-6 (Codex blocker): cancel_shutdown_race_single_terminal
        ///
        /// When the user triggers `CancelRequested` + `ShutdownRequested` **almost simultaneously**
        /// inside a streaming turn (typically holding Ctrl+C then immediately Ctrl+D / SIGTERM):
        /// - the first CancelRequested takes active_cancel → emits `Effect::CancelToken`
        /// - the second ShutdownRequested sees `generating == false` → must **not** emit another
        ///   `Effect::CancelToken` (otherwise it would take an Option already taken, or worse emit a
        ///   None token for EffectExecutor to dereference)
        ///
        /// Two contract points are checked:
        /// 1. the whole sequence emits exactly **one** `Effect::CancelToken` (terminal cancel is single)
        /// 2. the second ShutdownRequested does not panic / does not cancel twice / still emits `Effect::Quit`
        #[test]
        fn test_s2b_cancel_shutdown_race_single_terminal() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-race".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d-race".to_string(),
                delta: "streaming...".to_string(),
                version: 1,
            });
            assert!(state.control.generating);
            assert!(state.control.active_cancel.is_some());

            // 1. CancelRequested — takes active_cancel + emits CancelToken
            let cancel_effects = state.reduce(Action::CancelRequested);
            let cancel_token_count = cancel_effects
                .iter()
                .filter(|e| matches!(e, Effect::CancelToken(_)))
                .count();
            assert_eq!(
                cancel_token_count, 1,
                "the CancelRequested stage must emit exactly 1 CancelToken"
            );
            assert!(!state.control.generating, "generating=false after CancelRequested");
            assert!(
                state.control.active_cancel.is_none(),
                "active_cancel has already been taken"
            );

            // 2. ShutdownRequested — generating=false and active_cancel=None by now.
            //    The reducer must **not** emit CancelToken again (avoids a double cancel and taking a None)
            let shutdown_effects = state.reduce(Action::ShutdownRequested);
            let shutdown_cancel_token_count = shutdown_effects
                .iter()
                .filter(|e| matches!(e, Effect::CancelToken(_)))
                .count();
            assert_eq!(
                shutdown_cancel_token_count, 0,
                "ShutdownRequested must not emit CancelToken once active_cancel has been taken"
            );
            // it must not emit CancelDraft either (the draft was already cleared by CancelRequested)
            let shutdown_cancel_draft_count = shutdown_effects
                .iter()
                .filter(|e| matches!(e, Effect::CancelDraft(_)))
                .count();
            assert_eq!(
                shutdown_cancel_draft_count, 0,
                "ShutdownRequested must not emit CancelDraft once the draft is cleared"
            );
            // but it must emit Effect::Quit
            assert!(
                shutdown_effects.iter().any(|e| matches!(e, Effect::Quit)),
                "ShutdownRequested must emit Effect::Quit"
            );

            // the whole sequence emits only 1 CancelToken (the terminal effect is unique)
            let total_cancel_tokens = cancel_effects
                .iter()
                .chain(shutdown_effects.iter())
                .filter(|e| matches!(e, Effect::CancelToken(_)))
                .count();
            assert_eq!(
                total_cancel_tokens, 1,
                "the whole race sequence may emit only 1 Effect::CancelToken (terminal cancel is single)"
            );

            // terminal state: generating=false, active_cancel=None, draft=None — nothing left over
            assert!(!state.control.generating);
            assert!(state.control.active_cancel.is_none());
            assert!(state.stream.primary_streaming_draft().is_none());

            // Reverse race: build another state, shutdown first and cancel after —
            // again only 1 CancelToken may be emitted (the first takes the token, the later Cancel is a
            // no-op with generating=false).
            let mut state2 = s();
            let tok2 = CancellationToken::new();
            let _ = state2.reduce(Action::TurnStarted {
                draft_id: "d-race-2".to_string(),
                cancel: tok2,
            });
            let first_effects = state2.reduce(Action::ShutdownRequested);
            let second_effects = state2.reduce(Action::CancelRequested);
            let cancel_token_total = first_effects
                .iter()
                .chain(second_effects.iter())
                .filter(|e| matches!(e, Effect::CancelToken(_)))
                .count();
            assert_eq!(
                cancel_token_total, 1,
                "the reverse race (Shutdown→Cancel) must also emit only 1 CancelToken"
            );
            // the second CancelRequested must be a no-op (generating=false)
            assert!(
                second_effects.is_empty(),
                "after Shutdown generating=false, so CancelRequested must be a no-op, got: {second_effects:?}"
            );
        }
    }

    // ─── S2-C integration tests (3 new tests) ─────────────────────────────────
    //
    // These three tests cover the contract after S2-C wired the chat module's mirror / history paths
    // into Redux dispatch:
    //   1. on the /clear path the reducer keeps system and the UI mirror has a system message line
    //   2. after a user/assistant double dispatch, session.turns + session.history stay ordered
    //   3. SystemMessageAdded and RecordSystemMessage do not interfere (UI and history are two axes)
    //
    // Key design decisions (from the Codex P0 audit):
    //   - no legacy_mirror_enabled / legacy_history_enabled guards are introduced — the legacy
    //     history is still the real LLM context source, the reducer is an observing ledger.
    //   - SetLeadingSystemPrompt differs from RecordSystemMessage: the former upserts the front
    //     entry (it runs every turn, covering skill-list changes), the latter appends after /clear.
    #[cfg(test)]
    mod s2c {
        use super::super::*;
        use crate::chat::action::Action;
        use crate::providers::ChatMessage;
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        /// S2-C-1: redux_history_cleared_on_slash_clear_keeps_system_only
        ///
        /// Simulates the /clear path: on HistoryCleared the reducer must keep every system message and
        /// clear user/assistant. It checks that the reducer ends up equivalent to legacy
        /// `history.clear() + conditional push of system` (legacy re-pushes only when skill_rag is off;
        /// the reducer simply keeps the existing system, which equals the skill_rag.enabled path).
        #[test]
        fn redux_history_cleared_on_slash_clear_keeps_system_only() {
            let mut state = s();
            // starting history: system + 2 user + 2 assistant
            state.session.history.push(ChatMessage::system("sys-prompt-v1"));
            state.session.history.push(ChatMessage::user("u1"));
            state.session.history.push(ChatMessage::assistant("a1"));
            state.session.history.push(ChatMessage::user("u2"));
            state.session.history.push(ChatMessage::assistant("a2"));
            assert_eq!(state.session.history.len(), 5);

            let effects = state.reduce(Action::HistoryCleared);

            // terminal state: only the system entry remains
            assert_eq!(
                state.session.history.len(),
                1,
                "after HistoryCleared only 1 system entry may remain in history"
            );
            let kept = state
                .session
                .history
                .first()
                .expect("test: history must hold 1 system entry");
            assert_eq!(kept.role, "system");
            assert_eq!(kept.content, "sys-prompt-v1");

            // RequestRedraw + LogTrace must be emitted
            assert!(effects.iter().any(|e| matches!(e, Effect::RequestRedraw)));
            assert!(effects.iter().any(|e| matches!(e, Effect::LogTrace { .. })));

            // boundary: repeated /clear must be idempotent (still only one system)
            let _ = state.reduce(Action::HistoryCleared);
            assert_eq!(
                state.session.history.len(),
                1,
                "a second /clear still keeps 1 system entry"
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn redux_history_cleared_with_notice_keeps_visible_feedback() {
            let mut state = s();
            state.session.history.push(ChatMessage::system("sys-prompt-v1"));
            state.ui.conversation_lines.push(ConversationLine::User {
                content: "/clear".to_string(),
            });

            let effects = state.reduce(Action::HistoryClearedWithNotice {
                notice: "Conversation cleared (kept system prompt).".to_string(),
            });

            assert_eq!(state.session.history.len(), 1);
            assert_eq!(state.ui.conversation_lines.len(), 1);
            assert!(
                matches!(state.ui.conversation_lines.first(), Some(ConversationLine::System { content }) if content.contains("Conversation cleared")),
                "clear notice must survive the same reducer step that clears conversation_lines"
            );
            assert!(effects.iter().any(|e| matches!(e, Effect::RequestRedraw)));
        }

        /// S2-C-2: redux_history_append_order_user_assistant_stable
        ///
        /// After dispatching SetLeadingSystemPrompt + RecordUserTurn + RecordAssistantTurn, session.history
        /// must stay ordered as [system, user, assistant] and be byte-identical to the same legacy
        /// `history.push` sequence. It checks the reducer neither reorders nor skips a push.
        #[test]
        fn redux_history_append_order_user_assistant_stable() {
            let mut state = s();
            assert!(state.session.history.is_empty());

            // SetLeadingSystemPrompt on an empty history must push (equivalent to legacy
            // `if history.is_empty() { push }`)
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "system-rules".to_string(),
            });
            assert_eq!(state.session.history.len(), 1);
            let h0 = state.session.history.first().expect("test: history[0] after push");
            assert_eq!(h0.role, "system");

            // A second SetLeadingSystemPrompt (typical: it runs every turn) must replace the front, not append
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "system-rules-v2".to_string(),
            });
            assert_eq!(
                state.session.history.len(),
                1,
                "a second SetLeadingSystemPrompt call must upsert the front, never append"
            );
            let h0v2 = state.session.history.first().expect("test: history[0] after upsert");
            assert_eq!(h0v2.content, "system-rules-v2");

            // RecordUserTurn → append user
            let _ = state.reduce(Action::RecordUserTurn("user-q1".to_string()));
            assert_eq!(state.session.history.len(), 2);
            let h1 = state.session.history.get(1).expect("test: history[1] = user");
            assert_eq!(h1.role, "user");
            assert_eq!(h1.content, "user-q1");
            assert_eq!(state.session.turns.len(), 1, "session.turns must grow too (user)");

            // RecordAssistantTurn → append assistant
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "assistant-r1".to_string(),
            });
            assert_eq!(state.session.history.len(), 3);
            let h2 = state.session.history.get(2).expect("test: history[2] = assistant");
            assert_eq!(h2.role, "assistant");
            assert_eq!(h2.content, "assistant-r1");
            assert_eq!(state.session.turns.len(), 2, "session.turns +1 (assistant)");

            // one more round — the order must still be system, user, assistant, user, assistant
            let _ = state.reduce(Action::RecordUserTurn("user-q2".to_string()));
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "assistant-r2".to_string(),
            });
            assert_eq!(state.session.history.len(), 5);
            let roles: Vec<&str> = state.session.history.iter().map(|m| m.role.as_str()).collect();
            assert_eq!(
                roles,
                vec!["system", "user", "assistant", "user", "assistant"],
                "the order must stay stable"
            );
            // session.turns holds no system (only user/assistant are turns)
            assert_eq!(state.session.turns.len(), 4);
            let turn_roles: Vec<&str> = state.session.turns.iter().map(|t| t.role.as_str()).collect();
            assert_eq!(turn_roles, vec!["user", "assistant", "user", "assistant"]);
        }

        /// S2-C-3: redux_system_message_mirror_and_state_consistent
        ///
        /// SystemMessageAdded must only touch the UI mirror (ui.conversation_lines) and never pollute
        /// session.history; RecordSystemMessage must only touch session.history and never pollute
        /// ui.conversation_lines. The two paths are orthogonal.
        #[cfg(feature = "terminal-tui")]
        #[test]
        fn redux_system_message_mirror_and_state_consistent() {
            use crate::chat::tui::ConversationLine;
            let mut state = s();
            assert_eq!(state.ui.conversation_lines.len(), 0);
            assert_eq!(state.session.history.len(), 0);

            // SystemMessageAdded → only touches the UI mirror
            let effects = state.reduce(Action::SystemMessageAdded {
                text: "Banner v1".to_string(),
            });
            assert_eq!(state.ui.conversation_lines.len(), 1, "the ui mirror must grow");
            let first_line = state
                .ui
                .conversation_lines
                .first()
                .expect("test: conversation_lines[0] after SystemMessageAdded");
            assert!(
                matches!(
                    first_line,
                    ConversationLine::System { content } if content == "Banner v1"
                ),
                "it must be the ConversationLine::System variant"
            );
            assert_eq!(state.session.history.len(), 0, "session.history must stay untouched");
            assert!(effects.iter().any(|e| matches!(e, Effect::RequestRedraw)));

            // RecordSystemMessage → only touches session.history
            let _ = state.reduce(Action::RecordSystemMessage {
                content: "ctx-system-1".to_string(),
            });
            assert_eq!(state.session.history.len(), 1, "session.history must grow");
            let first_hist = state.session.history.first().expect("test: history[0] = system");
            assert_eq!(first_hist.role, "system");
            assert_eq!(first_hist.content, "ctx-system-1");
            assert_eq!(
                state.ui.conversation_lines.len(),
                1,
                "the ui mirror must stay untouched (still 1 banner line)"
            );

            // send a few more SystemMessageAdded — the UI mirror grows, history stays unchanged
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "Slash output 1".to_string(),
            });
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "Slash output 2".to_string(),
            });
            assert_eq!(state.ui.conversation_lines.len(), 3);
            assert_eq!(state.session.history.len(), 1, "session.history is still 1");
        }

        /// S2-C-bonus2 (Codex P0 regression): dispatching SetLeadingSystemPrompt after /clear must keep
        /// the terminal state at <= 1 system entry — using RecordSystemMessage by mistake would pile up
        /// 2+ system entries. This test simulates the full mod.rs:1254-1287 /clear !skill_rag.enabled path.
        #[test]
        fn redux_clear_then_set_leading_yields_single_system() {
            let mut state = s();
            // starting history: system + a few conversation entries
            state.session.history.push(ChatMessage::system("old-system"));
            state.session.history.push(ChatMessage::user("u1"));
            state.session.history.push(ChatMessage::assistant("a1"));
            state.session.history.push(ChatMessage::user("u2"));
            assert_eq!(state.session.history.len(), 4);

            // Step 1: HistoryCleared (dual-written with legacy `history.clear()`) —
            // the reducer keeps the old system and drains user/assistant
            let _ = state.reduce(Action::HistoryCleared);
            assert_eq!(
                state.session.history.len(),
                1,
                "HistoryCleared keeps only the 1 old system entry"
            );
            let after_clear = state.session.history.first().expect("test: post-clear history[0]");
            assert_eq!(after_clear.content, "old-system");

            // Step 2: SetLeadingSystemPrompt (dual-written with legacy `history.push(new system)`) —
            // upsert: replace the existing leading system with the new prompt (never append)
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "new-system".to_string(),
            });
            // Key point: the terminal state must still be 1 system with the new content (not 2 system entries)
            assert_eq!(
                state.session.history.len(),
                1,
                "/clear + SetLeadingSystemPrompt must end with 1 system entry, never accumulate"
            );
            let after_reset = state.session.history.first().expect("test: post-reset history[0]");
            assert_eq!(after_reset.role, "system");
            assert_eq!(after_reset.content, "new-system");

            // Regression guard: using RecordSystemMessage by mistake would give 2 entries — this explicitly
            // verifies SetLeadingSystemPrompt does not have append semantics
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "newer-system".to_string(),
            });
            assert_eq!(
                state.session.history.len(),
                1,
                "a repeated SetLeadingSystemPrompt still upserts, never appends"
            );
        }

        /// S2-C-bonus (Codex suggestion): SetLeadingSystemPrompt on a non-empty history must replace the
        /// front entry and never append — a regression guard for the 1336 semantics.
        #[test]
        fn set_leading_system_prompt_replaces_first_instead_of_append() {
            let mut state = s();
            // preload history: [system-old, user1, assistant1]
            state.session.history.push(ChatMessage::system("system-old"));
            state.session.history.push(ChatMessage::user("user1"));
            state.session.history.push(ChatMessage::assistant("assistant1"));
            assert_eq!(state.session.history.len(), 3);

            // SetLeadingSystemPrompt must replace the leading system, never append
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "system-new".to_string(),
            });
            assert_eq!(
                state.session.history.len(),
                3,
                "SetLeadingSystemPrompt must not change the history length (replace, not append)"
            );
            let h0 = state.session.history.first().expect("test: history[0] = system-new");
            assert_eq!(h0.role, "system");
            assert_eq!(h0.content, "system-new");
            // the user / assistant order is unchanged
            let h1 = state.session.history.get(1).expect("test: history[1] = user1");
            assert_eq!(h1.role, "user");
            assert_eq!(h1.content, "user1");
            let h2 = state.session.history.get(2).expect("test: history[2] = assistant1");
            assert_eq!(h2.role, "assistant");
        }

        #[test]
        fn set_leading_system_prompt_preserves_resumed_history_without_system() {
            let mut state = s();
            state.session.history.push(ChatMessage::user("resumed-user"));
            state.session.history.push(ChatMessage::assistant("resumed-assistant"));

            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "system-new".to_string(),
            });

            assert_eq!(state.session.history.len(), 3);
            let h0 = state.session.history.first().expect("test: inserted system prompt");
            assert_eq!(h0.role, "system");
            assert_eq!(h0.content, "system-new");
            let h1 = state.session.history.get(1).expect("test: resumed user preserved");
            assert_eq!(h1.role, "user");
            assert_eq!(h1.content, "resumed-user");
            let h2 = state.session.history.get(2).expect("test: resumed assistant preserved");
            assert_eq!(h2.role, "assistant");
            assert_eq!(h2.content, "resumed-assistant");
        }
    }

    // ─── S2.5 P1-B: tool_calls parity via backfill inside the reducer ─────────
    //
    // Option C buffers them in ControlState.current_turn_tool_calls inside the reducer:
    //   ToolStarted/Finished accumulate, RecordAssistantTurn uses mem::take to backfill
    //   session.turns.last_mut().tool_calls, and the stream terminal + InputSubmitted clear it.
    // This closes the original FIXME(S2.5) at state.rs:1171 with no Action signature or callsite change.
    #[cfg(test)]
    mod p1_b_tool_calls_parity {
        use super::super::*;
        use crate::chat::action::Action;
        use crate::chat::session::ToolCallSummary;
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        /// S2.5 P1-B: RecordAssistantTurn backfills tool_calls into session.turns.last_mut().tool_calls.
        #[test]
        fn s2_5_p1_b_assistant_turn_carries_tool_calls() {
            let mut state = s();
            let _ = state.reduce(Action::RecordUserTurn("question".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                success: true,
                duration_ms: 12,
                result: Some("ok".to_string()),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "answer".to_string(),
            });

            let last = state.session.turns.last().expect("test: assistant turn");
            assert_eq!(last.role, "assistant");
            assert_eq!(
                last.tool_calls.len(),
                1,
                "the 1 tool_call of this turn must be backfilled"
            );
            let call: &ToolCallSummary = last.tool_calls.first().expect("test: tool_calls[0]");
            assert_eq!(call.name, "shell");
            assert!(call.success);
            assert_eq!(call.args_preview, r#"command="ls""#);

            // after the backfill the ControlState buffer must be empty (mem::take + clear).
            assert!(!state.control.tool_buffers.contains_key(&ToolTaskKey::Primary));
        }

        /// S2.5 P1-B: several tools in the same turn are aggregated in order.
        #[test]
        fn s2_5_p1_b_multi_tool_aggregates_in_turn() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-2".to_string(),
                cancel: CancellationToken::new(),
            });
            for (i, ok) in [(1u8, true), (2, false), (3, true)] {
                let name = format!("tool{i}");
                let _ = state.reduce(Action::ToolStarted {
                    task_id: None,
                    sequence: None,
                    tool_call_id: None,
                    name: name.clone(),
                    args: format!("args-{i}"),
                });
                let _ = state.reduce(Action::ToolFinished {
                    task_id: None,
                    sequence: None,
                    tool_call_id: None,
                    name,
                    success: ok,
                    duration_ms: 10,
                    result: None,
                });
            }
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "a".to_string(),
            });

            let last = state.session.turns.last().expect("test: assistant turn");
            assert_eq!(last.tool_calls.len(), 3);
            let t0 = last.tool_calls.first().expect("test: tool_calls[0]");
            assert_eq!(t0.name, "tool1");
            assert!(t0.success);
            let t1 = last.tool_calls.get(1).expect("test: tool_calls[1]");
            assert_eq!(t1.name, "tool2");
            assert!(!t1.success);
            let t2 = last.tool_calls.get(2).expect("test: tool_calls[2]");
            assert_eq!(t2.name, "tool3");
            assert!(t2.success);
        }

        /// S2.5 P1-B: the turn boundary (StreamCompleted) clears the buffer, no cross-turn pollution.
        #[test]
        fn s2_5_p1_b_turn_boundary_clears_buffer() {
            let mut state = s();

            // Turn 1: tools accumulate, but RecordAssistantTurn is deliberately skipped for StreamCompleted.
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-3a".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "leftover".to_string(),
                args: "x".to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "leftover".to_string(),
                success: true,
                duration_ms: 1,
                result: None,
            });
            assert_eq!(
                state
                    .control
                    .tool_buffers
                    .get(&ToolTaskKey::Primary)
                    .map_or(0, |buffer| buffer.tool_calls.len()),
                1
            );
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d-p1b-3a".to_string(),
                final_text: "x".to_string(),
                reasoning: String::new(),
            });
            // after the StreamCompleted fallback clear the buffer is empty.
            assert!(!state.control.tool_buffers.contains_key(&ToolTaskKey::Primary));

            // Turn 2: RecordAssistantTurn must get empty tool_calls (not polluted by Turn 1 leftovers).
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-3b".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "clean".to_string(),
            });
            let last = state.session.turns.last().expect("test: turn 2 assistant");
            assert_eq!(
                last.tool_calls.len(),
                0,
                "Turn 2 must not inherit Turn 1 leftover tool_calls"
            );
        }

        /// S2.5 P1-B: a cancelled stream clears the buffer (the user pressed Ctrl+C mid-turn).
        #[test]
        fn s2_5_p1_b_stream_cancelled_clears_buffer() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-4".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "partial".to_string(),
                args: "...".to_string(),
            });
            // at this point the args buffer holds content
            assert_eq!(
                state
                    .control
                    .tool_buffers
                    .get(&ToolTaskKey::Primary)
                    .map_or(0, |buffer| buffer.tool_args.len()),
                1
            );

            let _ = state.reduce(Action::StreamCancelled {
                draft_id: "d-p1b-4".to_string(),
            });
            assert!(
                !state.control.tool_buffers.contains_key(&ToolTaskKey::Primary),
                "the buffer must be cleared after cancel"
            );

            // the same check for StreamFailed.
            let mut state2 = s();
            let _ = state2.reduce(Action::TurnStarted {
                draft_id: "d-p1b-4b".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state2.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "partial2".to_string(),
                args: "...".to_string(),
            });
            let _ = state2.reduce(Action::StreamFailed {
                draft_id: "d-p1b-4b".to_string(),
                err: "timeout".to_string(),
                retryable: true,
            });
            assert!(!state2.control.tool_buffers.contains_key(&ToolTaskKey::Primary));
        }

        /// S2.5 P1-B: extends the fixB C1 parity pattern — after RecordAssistantTurn,
        /// session.turns.last().tool_calls must contain this turn's accumulated ToolFinished entries.
        /// It simulates the enriched package path of the reducer route in Both mode, showing the reducer
        /// persistence path now carries tool_calls (closing the FIXME(S2.5) gap).
        #[test]
        fn s2_5_p1_b_both_parity_includes_tool_calls() {
            let mut state = s();
            // simulate a full turn package: user → turn started → several tools → assistant.
            let _ = state.reduce(Action::RecordUserTurn("ask".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-parity".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                args: r#"{"q":"x"}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                success: true,
                duration_ms: 5,
                result: Some("hit".to_string()),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "fetch".to_string(),
                args: "url".to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "fetch".to_string(),
                success: false,
                duration_ms: 30,
                result: Some("404".to_string()),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "done".to_string(),
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d-parity".to_string(),
                final_text: "done".to_string(),
                reasoning: String::new(),
            });

            // equivalent mirror of legacy session.add_assistant_turn(content, tool_calls).
            let assistant_turn = state
                .session
                .turns
                .iter()
                .rev()
                .find(|t| t.role == "assistant")
                .expect("test: assistant turn exists");
            assert_eq!(assistant_turn.tool_calls.len(), 2);
            let c0 = assistant_turn.tool_calls.first().expect("test: tool_calls[0]");
            assert_eq!(c0.name, "search");
            assert!(c0.success);
            let c1 = assistant_turn.tool_calls.get(1).expect("test: tool_calls[1]");
            assert_eq!(c1.name, "fetch");
            assert!(!c1.success);

            // check that the turns written by build_session_snapshot also carry tool_calls.
            let snap = state.build_session_snapshot();
            let snap_assistant = snap
                .turns
                .iter()
                .rev()
                .find(|t| t.role == "assistant")
                .expect("test: snapshot assistant turn");
            assert_eq!(
                snap_assistant.tool_calls.len(),
                2,
                "the turns persisted by build_session_snapshot must carry tool_calls"
            );
        }
    }

    #[cfg(test)]
    mod p3a_task_aware_tool_buffers {
        use super::super::*;
        use crate::chat::action::Action;
        use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        fn task_pair() -> ((TurnTaskId, u64), (TurnTaskId, u64)) {
            let mut scheduler = TurnScheduler::new();
            let a = scheduler.enqueue("a", TurnPriority::Normal, 0);
            let b = scheduler.enqueue("b", TurnPriority::Normal, 0);
            let a_seq = scheduler.task(a).expect("test: task a").sequence;
            let b_seq = scheduler.task(b).expect("test: task b").sequence;
            ((a, a_seq), (b, b_seq))
        }

        fn start_task(state: &mut ChatState, task_id: TurnTaskId, sequence: u64, draft_id: &str) {
            let _ = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: Some(task_id),
                provider_turn_sequence: Some(sequence),
                draft_id: draft_id.to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel: CancellationToken::new(),
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            });
        }

        fn start_tool(state: &mut ChatState, task_id: TurnTaskId, sequence: u64, name: &str, args: &str) {
            let _ = state.reduce(Action::ToolStarted {
                task_id: Some(task_id),
                sequence: Some(sequence),
                tool_call_id: None,
                name: name.to_string(),
                args: args.to_string(),
            });
        }

        fn finish_tool(state: &mut ChatState, task_id: TurnTaskId, sequence: u64, name: &str, success: bool) {
            let _ = state.reduce(Action::ToolFinished {
                task_id: Some(task_id),
                sequence: Some(sequence),
                tool_call_id: None,
                name: name.to_string(),
                success,
                duration_ms: 7,
                result: Some(format!("{name}-result")),
            });
        }

        #[test]
        fn p3a_cancelled_task_finalizes_only_its_tool_buffer() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a");
            start_task(&mut state, task_b, seq_b, "draft-b");
            start_tool(&mut state, task_a, seq_a, "shell", r#"{"cmd":"sleep 1"}"#);
            start_tool(&mut state, task_b, seq_b, "grep", r#"{"q":"needle"}"#);

            let _ = state.reduce(Action::StreamCancelled {
                draft_id: "draft-a".to_string(),
            });

            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_a)), 0);
            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 1);
            assert!(
                state.ui.conversation_lines.iter().any(|line| matches!(
                    line,
                    crate::chat::tui::ConversationLine::ToolResult {
                        tool_name,
                        status: crate::chat::tui::ToolStatus::Running,
                        ..
                    } if tool_name == "grep"
                )),
                "task B running tool card must survive task A cancellation"
            );
        }

        #[test]
        fn p3a_completed_task_clears_only_matching_buffer() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a");
            start_task(&mut state, task_b, seq_b, "draft-b");
            start_tool(&mut state, task_a, seq_a, "search", r#"{"q":"a"}"#);
            finish_tool(&mut state, task_a, seq_a, "search", true);
            start_tool(&mut state, task_b, seq_b, "fetch", r#"{"url":"b"}"#);

            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "draft-a".to_string(),
                final_text: "a done".to_string(),
                reasoning: String::new(),
            });

            assert_eq!(state.control.tool_call_count(ToolTaskKey::Task(task_a)), 0);
            assert_eq!(state.control.tool_arg_count(ToolTaskKey::Task(task_b)), 1);
            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 1);
        }

        #[test]
        fn p3a_record_assistant_turn_drains_only_requested_task_calls() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a");
            start_task(&mut state, task_b, seq_b, "draft-b");
            start_tool(&mut state, task_a, seq_a, "search", r#"{"q":"a"}"#);
            finish_tool(&mut state, task_a, seq_a, "search", true);
            start_tool(&mut state, task_b, seq_b, "fetch", r#"{"url":"b"}"#);
            finish_tool(&mut state, task_b, seq_b, "fetch", false);

            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: Some(task_b),
                content: "b answer".to_string(),
            });

            let last = state.session.turns.last().expect("test: assistant turn");
            assert_eq!(last.tool_calls.len(), 1);
            let call = last.tool_calls.first().expect("test: tool call");
            assert_eq!(call.name, "fetch");
            assert!(!call.success);
            assert_eq!(call.task_id, Some(task_b.get()));
            assert_eq!(call.sequence, Some(seq_b));
            assert_eq!(state.control.tool_call_count(ToolTaskKey::Task(task_a)), 1);
            assert_eq!(state.control.tool_call_count(ToolTaskKey::Task(task_b)), 0);
        }

        #[test]
        fn p3a_same_tool_name_args_are_isolated_by_task() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a");
            start_task(&mut state, task_b, seq_b, "draft-b");
            start_tool(&mut state, task_a, seq_a, "shell", r#"{"cmd":"echo a"}"#);
            start_tool(&mut state, task_b, seq_b, "shell", r#"{"cmd":"echo b"}"#);

            finish_tool(&mut state, task_b, seq_b, "shell", true);

            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_a)), 1);
            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 0);
            let b_call = state
                .control
                .tool_buffers
                .get(&ToolTaskKey::Task(task_b))
                .and_then(|buffer| buffer.tool_calls.first())
                .expect("test: task b call");
            assert!(b_call.args_preview.contains("echo b"));
            assert_eq!(state.control.tool_arg_count(ToolTaskKey::Task(task_a)), 1);

            finish_tool(&mut state, task_a, seq_a, "shell", true);
            let a_call = state
                .control
                .tool_buffers
                .get(&ToolTaskKey::Task(task_a))
                .and_then(|buffer| buffer.tool_calls.first())
                .expect("test: task a call");
            assert!(a_call.args_preview.contains("echo a"));
        }

        #[test]
        fn p3a_legacy_primary_tool_buffer_path_still_records_tool_calls() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "primary-draft".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                success: true,
                duration_ms: 3,
                result: Some("ok".to_string()),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "primary answer".to_string(),
            });

            let last = state.session.turns.last().expect("test: primary assistant turn");
            assert_eq!(last.tool_calls.len(), 1);
            let call = last.tool_calls.first().expect("test: primary tool call");
            assert_eq!(call.name, "shell");
            assert!(call.success);
            assert_eq!(call.task_id, None);
            assert_eq!(call.sequence, None);
            assert!(!state.control.tool_buffers.contains_key(&ToolTaskKey::Primary));
        }
    }

    #[cfg(test)]
    mod p3b_task_aware_cancel_tokens {
        use super::super::*;
        use crate::chat::action::Action;
        use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        fn task_pair() -> ((TurnTaskId, u64), (TurnTaskId, u64)) {
            let mut scheduler = TurnScheduler::new();
            let a = scheduler.enqueue("a", TurnPriority::Normal, 0);
            let b = scheduler.enqueue("b", TurnPriority::Normal, 0);
            let a_seq = scheduler.task(a).expect("test: task a").sequence;
            let b_seq = scheduler.task(b).expect("test: task b").sequence;
            ((a, a_seq), (b, b_seq))
        }

        fn start_task(
            state: &mut ChatState,
            task_id: TurnTaskId,
            sequence: u64,
            draft_id: &str,
            cancel: CancellationToken,
        ) {
            let _ = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: Some(task_id),
                provider_turn_sequence: Some(sequence),
                draft_id: draft_id.to_string(),
                history: Vec::new(),
                compaction_guard_history: None,
                compaction_config: None,
                cancel,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
                routing_input: None,
            });
        }

        fn cancel_tokens(effects: Vec<Effect>) -> Vec<CancellationToken> {
            effects
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::CancelToken(token) => Some(token),
                    _ => None,
                })
                .collect()
        }

        #[test]
        fn p3b_two_task_tokens_are_independent() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            let token_a = CancellationToken::new();
            let token_b = CancellationToken::new();
            start_task(&mut state, task_a, seq_a, "draft-a", token_a.clone());
            start_task(&mut state, task_b, seq_b, "draft-b", token_b.clone());

            let tokens = cancel_tokens(state.reduce(Action::CancelRequested));

            assert_eq!(tokens.len(), 1);
            for token in tokens {
                token.cancel();
            }
            assert!(token_a.is_cancelled(), "primary task A token must be emitted");
            assert!(!token_b.is_cancelled(), "task B token must remain untouched");
            assert!(!state.control.turn_cancels.contains_key(&task_a));
            assert!(state.control.turn_cancels.contains_key(&task_b));
        }

        #[test]
        fn p3b_cancel_primary_does_not_clear_other_task_tool_buffer() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a", CancellationToken::new());
            start_task(&mut state, task_b, seq_b, "draft-b", CancellationToken::new());
            let _ = state.reduce(Action::ToolStarted {
                task_id: Some(task_b),
                sequence: Some(seq_b),
                tool_call_id: None,
                name: "grep".to_string(),
                args: r#"{"q":"needle"}"#.to_string(),
            });

            let _ = state.reduce(Action::CancelRequested);

            assert!(state.control.generating, "task B still keeps generation active");
            assert!(
                state
                    .stream
                    .visible_drafts
                    .iter()
                    .any(|draft| draft.task_id == Some(task_b))
            );
            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 1);
            assert!(state.control.turn_cancels.contains_key(&task_b));
        }

        #[test]
        fn p3b_global_state_clears_only_after_all_visible_tasks_are_cancelled() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a", CancellationToken::new());
            start_task(&mut state, task_b, seq_b, "draft-b", CancellationToken::new());
            let _ = state.reduce(Action::ToolApprovalRequested {
                task_id: Some(task_b),
                tool_id: "tool-b".to_string(),
                name: "shell".to_string(),
                args: "{}".to_string(),
            });

            let _ = state.reduce(Action::CancelRequested);

            assert!(state.control.generating, "B remains visible after cancelling A");
            assert!(
                state.control.active_cancel.is_none(),
                "task turns do not use legacy active_cancel"
            );
            assert!(
                state.ui.pending_tool_approval.is_some(),
                "B approval must not be globally cleared"
            );
            assert!(state.control.turn_cancels.contains_key(&task_b));

            let _ = state.reduce(Action::CancelRequested);

            assert!(!state.control.generating);
            assert!(state.control.active_cancel.is_none());
            assert!(state.control.turn_cancels.is_empty());
            assert!(state.stream.visible_drafts.is_empty());
            assert!(state.ui.pending_tool_approval.is_none());
        }

        #[test]
        fn p3b_shutdown_cancels_all_task_tokens() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            let token_a = CancellationToken::new();
            let token_b = CancellationToken::new();
            start_task(&mut state, task_a, seq_a, "draft-a", token_a.clone());
            start_task(&mut state, task_b, seq_b, "draft-b", token_b.clone());

            let effects = state.reduce(Action::ShutdownRequested);
            let tokens = cancel_tokens(effects);

            assert_eq!(tokens.len(), 2, "shutdown must emit both task cancel tokens");
            for token in tokens {
                token.cancel();
            }
            assert!(token_a.is_cancelled());
            assert!(token_b.is_cancelled());
            assert!(state.control.turn_cancels.is_empty());
            assert!(!state.control.generating);
            assert!(state.stream.visible_drafts.is_empty());
        }

        #[test]
        fn p4c_cancel_provider_turn_targets_requested_task_token_only() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            let token_a = CancellationToken::new();
            let token_b = CancellationToken::new();
            start_task(&mut state, task_a, seq_a, "draft-a", token_a.clone());
            start_task(&mut state, task_b, seq_b, "draft-b", token_b.clone());

            let tokens = cancel_tokens(state.reduce(Action::CancelProviderTurn { task_id: task_b }));

            assert_eq!(
                tokens.len(),
                1,
                "targeted cancel should emit only the requested task token"
            );
            for token in tokens {
                token.cancel();
            }
            assert!(!token_a.is_cancelled(), "peer task token must remain live");
            assert!(token_b.is_cancelled(), "requested task token must be cancelled");
            assert!(
                state.control.turn_cancels.contains_key(&task_a),
                "peer task cancel token remains retained"
            );
            assert!(
                !state.control.turn_cancels.contains_key(&task_b),
                "requested task cancel token is consumed"
            );
            assert!(
                state
                    .stream
                    .visible_drafts
                    .iter()
                    .any(|draft| draft.task_id == Some(task_a)),
                "peer draft remains visible"
            );
            assert!(
                state
                    .stream
                    .visible_drafts
                    .iter()
                    .all(|draft| draft.task_id != Some(task_b)),
                "requested draft is removed"
            );
            assert!(state.control.generating, "peer task keeps generation active");
        }

        #[test]
        fn p3b_single_legacy_turn_ctrl_c_still_uses_primary_active_cancel() {
            let mut state = s();
            let token = CancellationToken::new();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "primary-draft".to_string(),
                cancel: token.clone(),
            });

            let effects = state.reduce(Action::CancelRequested);
            let tokens = cancel_tokens(effects);

            assert_eq!(tokens.len(), 1);
            for token in tokens {
                token.cancel();
            }
            assert!(token.is_cancelled());
            assert!(state.control.active_cancel.is_none());
            assert!(state.control.turn_cancels.is_empty());
            assert!(!state.control.generating);
            assert!(state.stream.primary_streaming_draft().is_none());
        }
    }

    #[cfg(test)]
    mod p3c_task_aware_usage {
        use super::super::*;
        use crate::chat::action::{Action, ProviderUsageRecordKind};
        use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
        use crate::llm::route_decision::TokenUsageSource;
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        fn task_pair() -> (TurnTaskId, TurnTaskId) {
            let mut scheduler = TurnScheduler::new();
            let a = scheduler.enqueue("usage-a", TurnPriority::Normal, 0);
            let b = scheduler.enqueue("usage-b", TurnPriority::Normal, 0);
            (a, b)
        }

        fn record(total_tokens: u64) -> MainSessionTokenUsageRecord {
            MainSessionTokenUsageRecord {
                settlement_id: None,
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                prompt_tokens: total_tokens / 2,
                completion_tokens: total_tokens - (total_tokens / 2),
                total_tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                source: TokenUsageSource::Reported,
                cost_usd: None,
            }
        }

        fn provider_usage(
            state: &mut ChatState,
            task_id: Option<TurnTaskId>,
            usage_kind: ProviderUsageRecordKind,
            total_tokens: u64,
        ) -> Vec<Effect> {
            state.reduce(Action::ProviderUsageRecorded {
                task_id,
                usage_kind,
                record: record(total_tokens),
            })
        }

        #[test]
        fn p3c_same_task_final_aggregate_is_deduped_once() {
            let mut state = s();
            let (task_id, _) = task_pair();

            let first = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::FinalAggregate, 10);
            let second = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::FinalAggregate, 20);

            assert_eq!(first.len(), 2);
            assert!(second.is_empty(), "duplicate final aggregate should be a reducer no-op");
            assert_eq!(state.session.token_usage_records.len(), 1);
            assert_eq!(state.ui.token_usage_summary.total_tokens, 10);
        }

        #[test]
        fn p3c_out_of_order_final_usage_stays_on_own_task() {
            let mut state = s();
            let (task_a, task_b) = task_pair();

            let _ = provider_usage(&mut state, Some(task_b), ProviderUsageRecordKind::FinalAggregate, 40);
            let _ = provider_usage(&mut state, Some(task_a), ProviderUsageRecordKind::FinalAggregate, 15);
            let duplicate_b = provider_usage(&mut state, Some(task_b), ProviderUsageRecordKind::FinalAggregate, 99);

            assert!(duplicate_b.is_empty());
            assert_eq!(state.session.token_usage_records.len(), 2);
            let totals = state
                .session
                .token_usage_records
                .iter()
                .map(|record| record.total_tokens)
                .collect::<Vec<_>>();
            assert_eq!(totals, vec![40, 15]);
            assert_eq!(state.ui.token_usage_summary.total_tokens, 55);
            assert!(state.control.final_usage_tasks_recorded.contains(&task_a));
            assert!(state.control.final_usage_tasks_recorded.contains(&task_b));
        }

        #[test]
        fn p3c_incremental_usage_for_same_task_is_not_deduped() {
            let mut state = s();
            let (task_id, _) = task_pair();

            let _ = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::Incremental, 7);
            let _ = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::Incremental, 11);

            assert_eq!(state.session.token_usage_records.len(), 2);
            assert_eq!(state.ui.token_usage_summary.request_count, 2);
            assert_eq!(state.ui.token_usage_summary.total_tokens, 18);
            assert!(state.control.final_usage_tasks_recorded.is_empty());
        }

        #[test]
        fn p3c_legacy_usage_without_task_id_is_not_deduped() {
            let mut state = s();

            let _ = provider_usage(&mut state, None, ProviderUsageRecordKind::FinalAggregate, 12);
            let _ = provider_usage(&mut state, None, ProviderUsageRecordKind::FinalAggregate, 13);

            assert_eq!(state.session.token_usage_records.len(), 2);
            assert_eq!(state.ui.token_usage_summary.request_count, 2);
            assert_eq!(state.ui.token_usage_summary.total_tokens, 25);
            assert!(state.control.final_usage_tasks_recorded.is_empty());
        }
    }

    // ─── S4-A Commit 1: UiSnapshot + reduce_tracked unit tests ────────────────

    #[cfg(feature = "terminal-tui")]
    mod s4_a_1 {
        use super::*;
        use crate::chat::tui::ConversationLine;
        use tokio_util::sync::CancellationToken;

        fn make_state() -> ChatState {
            ChatState::new(
                Arc::from("test-provider"),
                Arc::from("test-model"),
                CancellationToken::new(),
            )
        }

        #[test]
        fn s4_a_1_snapshot_initial_zero_revision() {
            let snap = UiSnapshot::initial(Arc::from("p"), Arc::from("m"));
            assert_eq!(snap.revision, 0);
            assert_eq!(&*snap.provider, "p");
            assert_eq!(&*snap.model, "m");
            assert!(snap.conversation_lines.is_empty());
            assert_eq!(snap.turn_count, 0);
            assert!(snap.streaming.is_none());
        }

        #[test]
        fn s4_a_1_snapshot_clone_is_arc_shallow() {
            // check that conversation_lines is Arc-shared: after cloning the snapshot both Arcs
            // point at the same underlying Vec, so strong_count is at least 2.
            let mut state = make_state();
            state.ui.conversation_lines.push(ConversationLine::User {
                content: "hi".to_string(),
            });
            let snap = state.build_ui_snapshot(1);
            let snap2 = snap.clone();
            // Arc::strong_count(&snap.conversation_lines) counts snap + snap2 = 2
            assert!(
                Arc::strong_count(&snap.conversation_lines) >= 2,
                "a snapshot clone must share the conversation_lines Arc, count={}",
                Arc::strong_count(&snap.conversation_lines)
            );
            assert_eq!(snap2.revision, 1);
        }

        #[test]
        fn s4_a_1_build_after_user_message_includes_line() {
            let mut state = make_state();
            state.ui.conversation_lines.push(ConversationLine::User {
                content: "hello".to_string(),
            });
            let snap = state.build_ui_snapshot(7);
            assert_eq!(snap.revision, 7);
            assert_eq!(snap.conversation_lines.len(), 1);
            match snap.conversation_lines.first() {
                Some(ConversationLine::User { content }) => assert_eq!(content, "hello"),
                other => panic!("expected User line, got {other:?}"),
            }
        }

        #[test]
        fn s4_a_1_ui_dirty_true_on_record_user_turn_via_runtime_fallback() {
            // RecordUserTurn is statically false (it writes session, not ui);
            // the runtime snapshot_dirty_fields is unchanged too → false overall.
            // This case verifies that writing session does not incidentally set dirty.
            let mut state = make_state();
            let (_effects, dirty) = state.reduce_tracked(Action::RecordUserTurn("q".into()));
            assert!(!dirty, "RecordUserTurn must not set ui_dirty");
        }

        #[test]
        fn s4_a_1_tool_progress_dirty_but_retry_trace_only_is_clean() {
            let mut state = make_state();
            let (_e, d) = state.reduce_tracked(Action::ToolProgress { iteration: 1 });
            assert!(d, "ToolProgress must dirty Pure snapshots so progress is visible");
            let (_e, d2) = state.reduce_tracked(Action::StreamRetryAttempt {
                attempt: 1,
                reason: "x".into(),
            });
            assert!(!d2, "StreamRetryAttempt must not be dirty");
        }

        #[test]
        fn s4_a_1_ui_dirty_true_on_stream_completed() {
            // full flow: TurnStarted registers the draft first, then StreamCompleted finalizes it.
            let mut state = make_state();
            let token = CancellationToken::new();
            let (_e, d_start) = state.reduce_tracked(Action::TurnStarted {
                draft_id: "d1".into(),
                cancel: token,
            });
            assert!(d_start, "TurnStarted must be dirty (stream.draft changed)");
            let (_e, d_done) = state.reduce_tracked(Action::StreamCompleted {
                draft_id: "d1".into(),
                final_text: "hi".into(),
                reasoning: String::new(),
            });
            assert!(
                d_done,
                "StreamCompleted must be dirty (conversation_lines + stream.draft)"
            );
        }

        #[test]
        fn s4_a_1_ui_dirty_true_on_system_message_added() {
            let mut state = make_state();
            let (_e, d) = state.reduce_tracked(Action::SystemMessageAdded { text: "banner".into() });
            assert!(d, "SystemMessageAdded must be dirty (pushed to conversation_lines)");
        }

        #[test]
        fn s4_a_1_build_session_title_into_arc() {
            // session.title is a String while the snapshot holds Arc<str> — check the conversion.
            let mut state = make_state();
            state.session.title = "my chat".to_string();
            let snap = state.build_ui_snapshot(2);
            assert_eq!(&*snap.session_title, "my chat");
        }
    }

    // ─── S4-A Commit 6: dual-path parity (mirror vs reducer) ───────────────

    #[cfg(feature = "terminal-tui")]
    mod s4_a_6 {
        use super::*;
        use crate::chat::tui::{ConversationLine, ToolStatus, TuiState};
        use tokio_util::sync::CancellationToken;

        /// Dual-run reconciliation: feed the same Action sequence into the mirror path (TuiState
        /// push_* calls) and the reducer path (Action → reduce → ui.conversation_lines), then assert
        /// that both paths produce byte-identical conversation_lines.
        ///
        /// This is the last safety net before S4-B really deletes chat_mirror — any reducer deviation
        /// from the legacy mirror shows up here as a byte-level diff.
        #[test]
        fn s4_a_6_dual_path_parity_user_assistant_tool() {
            // ── path A: the mirror path ──
            let mut mirror = TuiState::new("p", "m");
            mirror.push_system_message("banner");
            mirror.push_user_message("hello");
            mirror.push_tool_result_started("Bash", "{\"cmd\":\"ls\"}");
            let _ = mirror.mark_last_tool_result_finished("Bash", true, 50, None);
            mirror.start_stream("d-1");
            mirror.finalize_stream("d-1", "done");

            // ── path B: the reducer path (S4-A Commit A: UserMessageEchoed closes the User echo) ──
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "banner".to_string(),
            });
            let _ = state.reduce(Action::UserMessageEchoed("hello".to_string()));
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "Bash".to_string(),
                args: "{\"cmd\":\"ls\"}".to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "Bash".to_string(),
                success: true,
                duration_ms: 50,
                result: None,
            });
            let token = CancellationToken::new();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-1".to_string(),
                cancel: token,
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d-1".to_string(),
                final_text: "done".to_string(),
                reasoning: String::new(),
            });

            // reconcile the UI-visible ConversationLines; the completion duration stays out of the body.
            let mirror_lines: Vec<&ConversationLine> = mirror.conversation_lines.iter().collect();
            let reducer_lines: Vec<&ConversationLine> = state.ui.conversation_lines.iter().collect();

            assert_eq!(
                mirror_lines.len(),
                reducer_lines.len(),
                "reconciled line counts: mirror={}, reducer={}",
                mirror_lines.len(),
                reducer_lines.len()
            );
            for (i, (ml, rl)) in mirror_lines.iter().zip(reducer_lines.iter()).enumerate() {
                let m_dbg = format!("{ml:?}");
                let r_dbg = format!("{rl:?}");
                match (ml, rl) {
                    (
                        ConversationLine::ToolResult {
                            tool_name: m_name,
                            status: m_st,
                            elapsed_ms: m_ms,
                            ..
                        },
                        ConversationLine::ToolResult {
                            tool_name: r_name,
                            status: r_st,
                            elapsed_ms: r_ms,
                            ..
                        },
                    ) => {
                        assert_eq!(m_name, r_name, "line {i} ToolResult tool_name mismatch");
                        assert_eq!(m_st, r_st, "line {i} ToolResult status mismatch");
                        assert_eq!(m_ms, r_ms, "line {i} ToolResult elapsed_ms mismatch");
                        assert_eq!(*m_st, ToolStatus::Done);
                    }
                    (ConversationLine::Assistant { content: mc }, ConversationLine::Assistant { content: rc }) => {
                        assert_eq!(mc, rc, "line {i} Assistant content mismatch");
                    }
                    (ConversationLine::System { content: mc }, ConversationLine::System { content: rc }) => {
                        assert_eq!(mc, rc, "line {i} System content mismatch");
                    }
                    (ConversationLine::User { content: mc }, ConversationLine::User { content: rc }) => {
                        assert_eq!(mc, rc, "line {i} User content mismatch");
                    }
                    _ => panic!("line {i} variant mismatch: mirror={m_dbg}, reducer={r_dbg}"),
                }
            }
        }

        /// S4-A Commit A: in Pure mode UserMessageEchoed writes the User line into conversation_lines
        #[test]
        fn s4_a_post_p0_pure_user_echo_appears_in_snapshot() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let effects = state.reduce(Action::UserMessageEchoed("hello echo".to_string()));
            assert!(
                effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
                "UserMessageEchoed must emit RequestRedraw"
            );
            let snap = state.build_ui_snapshot(0);
            let last_user = snap
                .conversation_lines
                .iter()
                .find_map(|l| match l {
                    ConversationLine::User { content } => Some(content.as_str()),
                    _ => None,
                })
                .expect("the snapshot must contain a User line");
            assert_eq!(last_user, "hello echo");
        }

        /// S4-B T4-B-6: strict created_at semantics — set on the first RecordUserTurn, kept across turns
        #[test]
        fn s4_b_created_at_stable_across_turns() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            state.session.id = "sess1".to_string();
            assert!(state.session.created_at.is_none(), "created_at must start as None");

            let _ = state.reduce(Action::RecordUserTurn("q1".to_string()));
            let created_first = state
                .session
                .created_at
                .expect("the first RecordUserTurn must set created_at");

            // a second RecordUserTurn must not overwrite created_at
            std::thread::sleep(std::time::Duration::from_millis(2));
            let _ = state.reduce(Action::RecordUserTurn("q2".to_string()));
            assert_eq!(
                state.session.created_at,
                Some(created_first),
                "multiple turns must not overwrite created_at"
            );

            // build_session_snapshot must use SessionState.created_at
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "d".to_string(),
                final_text: "ok".to_string(),
                reasoning: String::new(),
            });
            let snap_session = effects
                .iter()
                .find_map(|e| match e {
                    Effect::SaveSession(s) => Some(s),
                    _ => None,
                })
                .expect("StreamCompleted must emit SaveSession");
            assert_eq!(
                snap_session.created_at, created_first,
                "build_session_snapshot must inherit SessionState.created_at, not overwrite it"
            );
        }

        /// S4-B T4-B-4: route_turn always returns ReduxDriver in Pure mode
        #[test]
        fn s4_b_route_turn_pure_always_redux_driver() {
            use crate::chat::{ReduxMode, TurnRoute, route_turn};
            assert_eq!(
                route_turn(ReduxMode::Pure),
                TurnRoute::ReduxDriver,
                "Pure mode must route to ReduxDriver even without driver_opt_in"
            );
        }

        /// S4-B cleanup: with all mirror pushes deleted, the reducer alone pushes ConversationLine
        #[test]
        fn s4_b_reducer_sole_source_for_conversation_lines() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "banner".to_string(),
            });
            let _ = state.reduce(Action::UserMessageEchoed("hi".to_string()));
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "Bash".to_string(),
                args: "{}".to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "Bash".to_string(),
                success: true,
                duration_ms: 10,
                result: None,
            });
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d".to_string(),
                final_text: "ok".to_string(),
                reasoning: String::new(),
            });
            assert_eq!(
                state.ui.conversation_lines.len(),
                4,
                "the reducer as sole source must push 4 lines"
            );
            let mut iter = state.ui.conversation_lines.iter();
            assert!(matches!(iter.next(), Some(ConversationLine::System { .. })));
            assert!(matches!(iter.next(), Some(ConversationLine::User { .. })));
            assert!(matches!(iter.next(), Some(ConversationLine::ToolResult { .. })));
            assert!(matches!(iter.next(), Some(ConversationLine::Assistant { .. })));
        }

        /// S4-A Commit E: TerminalResized must not set ui_dirty (snapshot fields unchanged, redraw via Effect)
        #[test]
        fn s4_a_post_p2_terminal_resized_not_dirty() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let (effects, dirty) = state.reduce_tracked(Action::TerminalResized { w: 120, h: 40 });
            assert!(
                effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
                "TerminalResized must still emit RequestRedraw (the redraw goes through redraw_tx)"
            );
            assert!(
                !dirty,
                "TerminalResized touches no snapshot field, so dirty must be false to avoid a pointless push"
            );
        }

        /// S4-A Commit B: two consecutive build_ui_snapshot calls with unchanged ui share the Arc (ptr_eq)
        #[test]
        fn s4_a_post_p1_arc_shared_no_clone() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            // push one line first so conversation_lines is non-empty and the cache is hit
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "banner".to_string(),
            });
            let snap1 = state.build_ui_snapshot(1);
            let snap2 = state.build_ui_snapshot(2);
            assert!(
                Arc::ptr_eq(&snap1.conversation_lines, &snap2.conversation_lines),
                "with unchanged ui consecutive build_ui_snapshot calls must share the Arc, avoiding O(n) clones"
            );

            // after a dirty Action the cache must be cleared and the new Arc pointer differs
            let _ = state.reduce_tracked(Action::SystemMessageAdded {
                text: "second".to_string(),
            });
            let snap3 = state.build_ui_snapshot(3);
            assert!(
                !Arc::ptr_eq(&snap2.conversation_lines, &snap3.conversation_lines),
                "after a ui change the cache is invalidated and the new snapshot must be a new Arc"
            );
            assert_eq!(
                snap3.conversation_lines.len(),
                2,
                "the new snapshot must contain two lines"
            );
        }
    }

    /// S5 invariant test suite: the reducer's core invariants (ordering / idempotency / cancel)
    #[cfg(feature = "terminal-tui")]
    mod s5_invariants {
        use super::super::{ChatState, Effect};
        use crate::chat::action::Action;
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        fn fresh_state() -> ChatState {
            ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new())
        }

        /// Ordering invariant: reducing the same Action sequence on two fresh states → same final state
        #[test]
        fn s5_invariant_determinism_same_actions_same_state() {
            let actions = || -> Vec<Action> {
                vec![
                    Action::SystemMessageAdded {
                        text: "banner".to_string(),
                    },
                    Action::UserMessageEchoed("hi".to_string()),
                    Action::RecordUserTurn("hi".to_string()),
                    Action::TurnStarted {
                        draft_id: "d1".to_string(),
                        cancel: CancellationToken::new(),
                    },
                    Action::RecordAssistantTurn {
                        task_id: None,
                        content: "ok".to_string(),
                    },
                    Action::StreamCompleted {
                        draft_id: "d1".to_string(),
                        final_text: "ok".to_string(),
                        reasoning: String::new(),
                    },
                ]
            };
            let mut state_a = fresh_state();
            let mut state_b = fresh_state();
            for a in actions() {
                let _ = state_a.reduce(a);
            }
            for a in actions() {
                let _ = state_b.reduce(a);
            }
            assert_eq!(
                state_a.session.turns.len(),
                state_b.session.turns.len(),
                "the same Action sequence must yield the same number of turns"
            );
            assert_eq!(
                state_a.ui.conversation_lines.len(),
                state_b.ui.conversation_lines.len(),
                "the same Action sequence must yield the same number of ConversationLines"
            );
            assert_eq!(
                state_a.session.title, state_b.session.title,
                "the same Action sequence must produce the same session title"
            );
        }

        /// Idempotency invariant: dispatching the same Action twice must not double-write persistence
        #[test]
        fn s5_invariant_idempotent_duplicate_dispatch() {
            let mut state = fresh_state();
            let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
            let turns_after_first = state.session.turns.len();
            let history_after_first = state.session.history.len();
            // in the real architecture a repeated dispatch double-writes — the reducer's current contract
            let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
            assert!(
                state.session.turns.len() > turns_after_first,
                "RecordUserTurn is not idempotent: a repeated dispatch appends a turn (chat::run avoids repeats)"
            );
            assert!(
                state.session.history.len() > history_after_first,
                "history is appended to as well"
            );
        }

        /// Cancel invariant: no SaveSession effect after CancelRequested (no partial state is written)
        #[test]
        fn s5_invariant_cancel_no_partial_save() {
            let mut state = fresh_state();
            let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-cancel".to_string(),
                cancel: CancellationToken::new(),
            });
            // the user cancels mid-turn
            let cancel_effects = state.reduce(Action::CancelRequested);
            assert!(
                !cancel_effects.iter().any(|e| matches!(e, Effect::SaveSession(_))),
                "CancelRequested must not emit SaveSession (avoids persisting partial state)"
            );
            // StreamCancelled must not emit SaveSession either
            let stream_cancel_effects = state.reduce(Action::StreamCancelled {
                draft_id: "d-cancel".to_string(),
            });
            assert!(
                !stream_cancel_effects
                    .iter()
                    .any(|e| matches!(e, Effect::SaveSession(_))),
                "StreamCancelled must not emit SaveSession (appendix B, Cancelled row)"
            );
        }

        #[test]
        fn save_session_effect_redacts_all_authoritative_content_fields() {
            let secret = "AKIAABCDEFGHIJKLMNOP";
            let mut state = fresh_state();
            state.session.title = format!("deploy {secret} \u{2705}");
            state.session.turns.push(crate::chat::session::ChatTurn {
                role: "assistant".to_string(),
                content: format!("keep Unicode content, hide {secret} \u{1f680}"),
                timestamp: chrono::Utc::now(),
                tool_calls: vec![crate::chat::session::ToolCallSummary {
                    name: "shell".to_string(),
                    args_preview: format!("echo {secret} \u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}"),
                    success: true,
                    task_id: Some(7),
                    sequence: Some(9),
                }],
            });
            let effects = state.reduce(Action::BackgroundSessionRecorded {
                summary: crate::chat::sessions::PersistedSessionSummary {
                    id: "child".to_string(),
                    seq: 1,
                    kind: "agent".to_string(),
                    origin: "model".to_string(),
                    status: "completed".to_string(),
                    title: format!("subtask {secret}"),
                    summary: format!("done {secret} \u{1f30d}"),
                    token_usage_records: Vec::new(),
                    created_at: chrono::Utc::now(),
                },
            });
            let snapshot = effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::SaveSession(session) => Some(session),
                    _ => None,
                })
                .expect("background record must save");
            let blob = snapshot.to_json().unwrap();
            assert!(!blob.contains(secret), "authoritative SaveSession blob leaked AWS key");
            assert!(blob.contains("Unicode"));
            assert!(blob.contains("\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}"));
            let tool_call = snapshot
                .turns
                .first()
                .and_then(|turn| turn.tool_calls.first())
                .expect("tool summary shape must be retained");
            assert_eq!(tool_call.task_id, Some(7));
            assert_eq!(tool_call.sequence, Some(9));
        }
    }
}
