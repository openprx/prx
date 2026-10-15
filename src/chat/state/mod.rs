//! Redux-like ChatState and its reducer.
//!
//! Contains:
//! - [`ChatState`] — top-level state holding 4 sub-structures
//! - [`SessionState`] / [`UiState`] / [`StreamState`] / [`ControlState`]
//! - [`Effect`] — side-effect instructions returned by reduce, run by the main loop's async shell
//!
//! Design principles:
//! - `ChatState::reduce` is a pure sync function: no I/O, no await, it only mutates itself

mod reduce_input;
mod reduce_session;
mod reduce_stream;
mod reduce_tools;
mod reduce_ui;

use crate::agent::turn_contracts::ChatMode;
use crate::channels::traits::SendMessage;
use crate::chat::action::{Action, MainQueueStatus, ProviderUsageRecordKind, ProviderWorkerStatus};
use crate::chat::session::{ChatSession, ChatTurn, MainSessionTokenUsageRecord, MainSessionTokenUsageSummary};
use crate::chat::slash_types::AtPathCandidate;
use crate::hooks::HookEvent;
use crate::memory::MemoryCategory;
use crate::providers::ChatMessage;
use crate::security::AutonomyLevel;
use crate::util::truncate_with_ellipsis;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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

/// A compacted history message that belongs in the durable transcript: a user
/// or assistant turn that is not one of the loop's own notes (the refresh
/// aside, a `[Tool results]` block, a recalled page). A real user turn that
/// opened with a marker was neutralised on entry and stays durable.
fn is_durable_compaction_history_message(message: &ChatMessage) -> bool {
    matches!(message.role.as_str(), "user" | "assistant")
        && !crate::agent::turn_contracts::is_loop_written_user_note(&message.content)
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
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "Preserve inline public variants without a per-turn heap allocation"
)]
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
        /// `parent_run_id = turn run_id` → origin = Model, mirroring the scope wrapper
        /// around the legacy tool-loop path's `run_turn` call in `chat::run`. `None` → no scope
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
        turn_binding: Option<Arc<super::decision_source::ChatTurnBinding>>,
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
    /// - `CancelToken` really fires the underlying token cancel so that `run_turn` /
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

    #[cfg(feature = "terminal-tui")]
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

    #[cfg(feature = "terminal-tui")]
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
                turn_binding,
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
                turn_binding,
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
#[cfg(feature = "terminal-tui")]
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
mod tests;
