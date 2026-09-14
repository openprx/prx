//! Redux-like Action algebra. Every state change must be applied through reduce.
//!
//! [`Action`] is the single event algebra, covering every state-change point of the chat main loop
//! (23 variants in total).
//! Design principles:
//! - every variant must be `Send + Sync` (they are passed across tasks over channels)
//! - they carry no Provider/Memory/Channel handles (those are dependencies of the Effect executor)
//! - Clone rather than Copy (they contain String/CancellationToken)

use crossterm::event::KeyEvent;
use tokio_util::sync::CancellationToken;

use crate::agent::loop_::ChatMode;
use crate::chat::session::{ChatSession, MainSessionTokenUsageRecord};
use crate::llm::route_decision::TokenUsage;

/// Main-session input backlog status for human-visible orchestration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MainQueueStatus {
    pub queued: usize,
    pub priority: usize,
}

/// Main-session provider worker lifecycle status for human-visible orchestration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderWorkerStatus {
    pub running: usize,
    pub cancelling: usize,
    pub awaiting_commit: usize,
    pub finalized_payloads: usize,
    pub finalized_total_tokens: u64,
    pub oldest_started_at_ms: Option<i64>,
    pub rows: Vec<ProviderWorkerStatusRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderWorkerStatusRow {
    pub task_id: u64,
    pub sequence: u64,
    pub kind: ProviderWorkerRowKind,
    pub state: ProviderWorkerRowState,
    pub started_at_ms: i64,
    pub finalized_total_tokens: Option<u64>,
    pub completion_ready: bool,
    pub recent_tool_call: Option<String>,
}

impl ProviderWorkerStatusRow {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self.state,
            ProviderWorkerRowState::Running
                | ProviderWorkerRowState::Cancelling
                | ProviderWorkerRowState::AwaitingCommit
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderWorkerRowKind {
    ForegroundAwaited,
    Detached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderWorkerRowState {
    Running,
    Cancelling,
    AwaitingCommit,
    Committed,
    Cancelled,
    Failed,
}

pub const PROVIDER_WORKER_VIEW_KIND: &str = "worker";
const PROVIDER_WORKER_VIEW_DESIRED_ROWS: usize = 24;

pub fn build_provider_worker_active_view(
    status: &ProviderWorkerStatus,
    sequence: u64,
    scroll_offset: usize,
) -> crate::chat::sessions::ActiveSessionView {
    build_provider_worker_active_view_with_io(status, sequence, scroll_offset, Vec::new())
}

pub fn build_provider_worker_active_view_with_io(
    status: &ProviderWorkerStatus,
    sequence: u64,
    scroll_offset: usize,
    io_lines: Vec<String>,
) -> crate::chat::sessions::ActiveSessionView {
    let row = status.rows.iter().find(|row| row.sequence == sequence);
    let title = format!("main provider w#{sequence}");
    let mut lines = Vec::new();
    if let Some(row) = row {
        lines.push(format!("worker: w#{}", row.sequence));
        lines.push(format!("task: {}", row.task_id));
        lines.push(format!("kind: {}", provider_worker_row_kind_label(row.kind)));
        lines.push(format!("state: {}", provider_worker_row_state_label(row.state)));
        lines.push(format!(
            "completion: {}",
            if row.completion_ready { "ready" } else { "pending" }
        ));
        lines.push(format!("elapsed: {}", provider_worker_elapsed_label(row.started_at_ms)));
        lines.push(match row.finalized_total_tokens {
            Some(tokens) if tokens > 0 => format!("tokens: {}", format_provider_worker_tokens_compact(tokens)),
            _ => "tokens: pending".to_string(),
        });
        lines.push(format!(
            "summary: {} running, {} cancelling, {} awaiting commit",
            status.running, status.cancelling, status.awaiting_commit
        ));
        lines.push(format!(
            "finalized: {} payloads, {} tokens",
            status.finalized_payloads,
            format_provider_worker_tokens_compact(status.finalized_total_tokens)
        ));
    } else {
        lines.push(format!("worker: w#{sequence}"));
        lines.push("state: no longer retained".to_string());
        lines.push("detail: use /workers for the current retained provider worker report".to_string());
    }
    if !io_lines.is_empty() {
        lines.push("io: recent provider turn".to_string());
        lines.extend(io_lines);
    }
    lines.push("view: read-only provider worker detail".to_string());
    lines.push("keys: PageUp/PageDown scroll, Esc returns to main".to_string());
    crate::chat::sessions::ActiveSessionView {
        seq: sequence,
        kind: PROVIDER_WORKER_VIEW_KIND.to_string(),
        title,
        lines,
        truncated: false,
        scroll_offset,
    }
    .clamped_for_height(PROVIDER_WORKER_VIEW_DESIRED_ROWS)
}

#[must_use]
pub fn build_provider_worker_active_view_with_io_preserving_scroll(
    status: &ProviderWorkerStatus,
    sequence: u64,
    previous: Option<&crate::chat::sessions::ActiveSessionView>,
    io_lines: Vec<String>,
) -> crate::chat::sessions::ActiveSessionView {
    let mut view = build_provider_worker_active_view_with_io(status, sequence, 0, io_lines);
    view.scroll_offset = provider_worker_refresh_scroll_offset(previous, &view);
    view.clamped_for_height(PROVIDER_WORKER_VIEW_DESIRED_ROWS)
}

fn provider_worker_refresh_scroll_offset(
    previous: Option<&crate::chat::sessions::ActiveSessionView>,
    refreshed: &crate::chat::sessions::ActiveSessionView,
) -> usize {
    let Some(previous) = previous
        .filter(|view| view.kind == PROVIDER_WORKER_VIEW_KIND && view.seq == refreshed.seq && view.scroll_offset > 0)
    else {
        return 0;
    };
    let appended = refreshed.lines.len().saturating_sub(previous.lines.len());
    previous
        .scroll_offset
        .saturating_add(appended)
        .min(refreshed.max_scroll_offset(PROVIDER_WORKER_VIEW_DESIRED_ROWS))
}

pub const fn provider_worker_row_state_label(state: ProviderWorkerRowState) -> &'static str {
    match state {
        ProviderWorkerRowState::Running => "running",
        ProviderWorkerRowState::Cancelling => "cancelling",
        ProviderWorkerRowState::AwaitingCommit => "awaiting_commit",
        ProviderWorkerRowState::Committed => "committed",
        ProviderWorkerRowState::Cancelled => "cancelled",
        ProviderWorkerRowState::Failed => "failed",
    }
}

pub const fn provider_worker_row_kind_label(kind: ProviderWorkerRowKind) -> &'static str {
    match kind {
        ProviderWorkerRowKind::ForegroundAwaited => "foreground_awaited",
        ProviderWorkerRowKind::Detached => "detached",
    }
}

pub const fn provider_worker_row_kind_compact(kind: ProviderWorkerRowKind) -> &'static str {
    match kind {
        ProviderWorkerRowKind::ForegroundAwaited => "fg",
        ProviderWorkerRowKind::Detached => "detached",
    }
}

fn provider_worker_elapsed_label(started_at_ms: i64) -> String {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let elapsed_ms = now_ms.saturating_sub(started_at_ms).max(0);
    let elapsed_secs = u64::try_from(elapsed_ms / 1000).unwrap_or_default();
    crate::chat::sessions::model::format_elapsed_compact(elapsed_secs)
}

pub fn format_provider_worker_tokens_compact(tokens: u64) -> String {
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

/// History navigation direction.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum HistoryDir {
    /// Upwards (older)
    Up,
    /// Downwards (newer)
    Down,
}

/// Why a history compaction was triggered (used for trace / test assertions).
///
/// S2-B Step 1: introduced together with the `HistoryCompacted` Action so the reducer can tell in the
/// trace whether this was an automatic context-overflow compaction or a manual user/test trigger, and
/// so unit tests can assert that the reason field reaches the `Effect::LogTrace` output.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactReason {
    /// Automatic compaction after exceeding the context window (the chat::run main-loop overflow
    /// retry path).
    ContextOverflow,
    /// Manually triggered by the user (e.g. a /compact command, reserved for a later step).
    Manual,
}

/// Identity for main-session provider usage records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderUsageRecordKind {
    /// A final per-turn aggregate produced when provider completion is consumed.
    FinalAggregate,
    /// A non-final metering segment. These are intentionally not deduped by task
    /// id. No production path emits this yet; it is reserved for future
    /// incremental metering and its "not deduped" contract is locked by tests so
    /// the dedup rule stays scoped to `FinalAggregate` only.
    Incremental,
}

/// The single event algebra; every state change must be applied through reduce.
///
/// `Send + Sync` (passed across tasks over channels), and every case is expressible without
/// `Box<dyn>`. Step 1: the type skeleton; Steps 2-5 wire it into the call paths one by one.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum Action {
    // ── Input path ──────────────────────────────────────────────
    /// Raw keyboard event
    KeyPressed(KeyEvent),
    /// Bracketed paste
    PasteReceived(String),
    /// Terminal size change
    TerminalResized { w: u16, h: u16 },
    /// A submission parsed by the dispatcher (the user pressed Enter)
    InputSubmitted(String),
    /// Replace the visible draft buffer without submitting it.
    InputReplaced(String),
    /// Up/Down history navigation
    HistoryNavigated(HistoryDir),
    /// Esc — cancel the current input
    InputCancelled,

    // ── Slash commands ──────────────────────────────────────────
    /// The user entered a slash command (/plan, /clear, ...)
    SlashCommandIssued { cmd: String, args: String },
    /// Mode switch (/plan /edit /auto)
    ModeChanged(ChatMode),
    /// Online model switch (/model <name>) — BUG-07.
    ///
    /// The reducer updates `session.model` so the status bar reflects the new model immediately; what
    /// actually affects the model of later LLM turns is the main loop writing the new value into the
    /// hot-swap slot of `EffectDeps` (same provider, different model). This only does bookkeeping plus
    /// RequestRedraw and produces no other side effects.
    ModelChanged { model: String },
    /// Online provider switch (/provider <name> [model]) — Bug #3.
    ///
    /// The reducer updates `session.provider` (and `session.model` when needed) so the status bar and
    /// the session snapshot reflect the new provider immediately; what actually affects the provider
    /// instance of later LLM turns is the main loop rebuilding it and writing it into the
    /// `ProviderSlot` hot-swap slot (the reducer holds no provider instance, so it only owns the
    /// UI / session ledger). A `Some` `model` means the switch also changed the model (the command
    /// carried a compatible model argument, or the current model already changed), and the reducer
    /// syncs `session.model` along with it.
    ProviderChanged { provider: String, model: Option<String> },
    /// Clear the context of the current session (/clear). /new uses SessionLoaded to switch to a new
    /// session.
    HistoryCleared,
    /// Clear the history and append a user-visible receipt in the same UI snapshot.
    HistoryClearedWithNotice { notice: String },

    // ── LLM streaming ───────────────────────────────────────────
    /// A new LLM inference turn starts, carrying the draft_id and cancellation token
    TurnStarted {
        draft_id: String,
        cancel: CancellationToken,
    },
    /// Step 5a-3 Phase A — the truly leading path: start a streaming LLM turn.
    ///
    /// Relationship to [`Self::TurnStarted`]:
    /// - `TurnStarted` is the historical Action, used only for reducer state initialization (set the
    ///   draft, register active_cancel, raise generating); the chat::run main loop dispatches it
    ///   synchronously before calling the old `run_tool_call_loop`, and it emits no `Effect::StartTurn`
    /// - `StartLLMTurn` carries a full `history` snapshot, and besides initializing the draft the
    ///   reducer **also** emits `Effect::StartTurn { draft_id, history, cancel }`, which the
    ///   EffectExecutor really wires to `provider.stream_chat_with_history`
    ///
    /// During Phase A both coexist and the main loop is still led by the old path; after Phase B the
    /// old path is deleted and `TurnStarted` is fully replaced by `StartLLMTurn`.
    StartLLMTurn {
        /// Main turn scheduler identity for this provider execution. `None`
        /// preserves non-chat/test callers, but live chat should pass the
        /// `TurnTaskId` that owns this provider worker.
        provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        /// Scheduler sequence for visible ordering. `None` preserves tests and
        /// non-chat callers; live chat should pass the sequence from
        /// `TurnScheduler`.
        provider_turn_sequence: Option<u64>,
        draft_id: String,
        history: Vec<crate::providers::ChatMessage>,
        /// Optional original/persisted history source for compaction guards and
        /// injection-overbudget diagnostics. Live chat passes the same
        /// canonical view used for persistence so provider-only enrichment never
        /// becomes the reducer's guard source.
        compaction_guard_history: Option<Vec<crate::providers::ChatMessage>>,
        /// P5 proactive budgeting: resolved context budget for this turn.
        /// `None` keeps tests and non-chat callers budget-neutral.
        compaction_config: Option<crate::config::AgentCompactionConfig>,
        cancel: CancellationToken,
        /// D8-4 (redux path): the turn-root spawn execution context seeded by
        /// `chat::run` for this turn. Threaded through the reducer into
        /// `Effect::StartTurn` so the Redux driver can `SPAWN_EXECUTION_CONTEXT
        /// .scope(...)` its tool-call loop. Without this, sub-agents spawned via
        /// `sessions_spawn` on the redux path see `parent_run_id = None` and are
        /// mislabeled as user-originated instead of model-originated.
        ///
        /// `None` means "no turn-root context" (e.g. tests, or callers that do
        /// not originate a chat turn) — sub-agents then fall back to user origin,
        /// which is correct for non-turn paths such as the `/bg` slash command.
        turn_spawn_ctx: Option<crate::tools::sessions_spawn::SpawnExecutionContext>,
        /// Per-turn default route for `message_send` tool calls. This mirrors
        /// `turn_spawn_ctx`: the real Redux driver runs in a spawned task, so
        /// the routing default must be carried through the reducer/effect
        /// boundary and scoped at the actual tool execution site.
        turn_message_send_ctx: Option<crate::tools::message_send::MessageSendExecutionContext>,
        /// The turn's raw user text, before memory recall, `@path` expansion,
        /// or the `[Recent shared workspace events]` block are prepended to it.
        /// Capability routing runs on this and nothing else: every other entry
        /// point already passes `ToolLoopMemory::with_routing_input`, and chat
        /// was the one surface routing on injected context, which let recalled
        /// text and shared-workspace events widen the published tool surface.
        /// `None` keeps non-chat/test callers on the driver's history fallback.
        routing_input: Option<String>,
    },
    /// Received one streaming delta chunk
    StreamChunkReceived {
        draft_id: String,
        delta: String,
        version: u64,
    },
    /// Received a reasoning ("thinking") delta.
    ///
    /// Carries only the delta: the reducer keeps a character counter plus a
    /// bounded tail on the draft so the TUI can show live thinking progress.
    /// The authoritative full reasoning body stays in the streaming driver and
    /// is replayed once in [`Self::StreamCompleted`], so there is no second
    /// copy of the whole body in reducer state.
    ///
    /// `version` is drawn from the same per-turn counter as
    /// [`Self::StreamChunkReceived`], so the reducer's strict-monotonic guard
    /// drops stale/reordered reasoning deltas exactly like text deltas.
    StreamReasoningReceived {
        draft_id: String,
        delta: String,
        version: u64,
    },
    /// Provider-reported or estimated usage for a streaming turn.
    StreamUsageMetered { draft_id: String, usage: TokenUsage },
    /// Streaming completed, carrying the final text and the reasoning summary
    StreamCompleted {
        draft_id: String,
        final_text: String,
        reasoning: String,
    },
    /// Provider turn completed, but assistant/session persistence must wait for
    /// the ordered commit gate. The dispatcher records this as a turn
    /// completion signal and the reducer intentionally performs no state change.
    ProviderTurnReadyForCommit {
        draft_id: String,
        final_text: String,
        reasoning: String,
    },
    /// Streaming failed
    StreamFailed {
        draft_id: String,
        err: String,
        retryable: bool,
    },
    /// Streaming was cancelled
    StreamCancelled { draft_id: String },

    // ── Tool events ─────────────────────────────────────────────
    /// A tool call started
    ToolStarted {
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        args: String,
    },
    /// A tool call finished
    ToolFinished {
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        success: bool,
        duration_ms: u64,
        result: Option<String>,
    },
    /// Tool call progress
    ToolProgress { iteration: usize },
    /// **S3 T3-1**: the driver asks the UI to approve a tool call (triggered in supervised autonomy
    /// mode).
    ///
    /// The reducer only produces `Effect::RequestApproval`; the driver itself waits for the response
    /// on a oneshot rx. `tool_id` is the `tool_call_id` given by the LLM, used to correlate the
    /// response [`Self::ToolApprovalReceived`] with the specific pending oneshot.
    ToolApprovalRequested {
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        tool_id: String,
        name: String,
        args: String,
    },
    /// **S3 T3-1**: the UI / EffectExecutor posts the user's approval decision back to the driver.
    ///
    /// When the dispatcher receives this Action it forwards the decision to the waiting driver over
    /// `approval_response_tx` (the single mpsc entry point injected into the driver); the driver side
    /// matches it by `tool_id` to the pending oneshot::Sender<bool> and resolves it.
    ToolApprovalReceived { tool_id: String, approved: bool },
    /// Clear any visible approval prompt without approving anything.
    ///
    /// Used when a session switch fail-closes outstanding approval routes before
    /// swapping per-session state.
    ToolApprovalCleared,
    /// **S3 T3-1**: notification of a retry attempt after a transient network failure (for UI / trace
    /// only, it does not change state).
    ///
    /// `attempt` is counted from 1 (the 1st failure → attempt=1, after which the retry sleep starts).
    /// The failure cause goes into `reason` so the UI can display it.
    StreamRetryAttempt { attempt: u8, reason: String },

    // ── Session ─────────────────────────────────────────────────
    /// The session finished loading
    SessionLoaded(ChatSession),
    /// The session has been persisted
    SessionSaved { id: String },
    /// Switch to the given session
    SessionSwitched { id: String },
    /// Ask the reducer to persist the user turn (writes session.turns + LLM history)
    RecordUserTurn(String),
    /// Ask the reducer to persist the assistant turn (writes session.turns + LLM history)
    RecordAssistantTurn {
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        content: String,
    },
    /// Ask the reducer to append one system message to `session.history` (for cases such as rebuilding
    /// the system prompt after `/clear`).
    ///
    /// S2-C Step 2: matches legacy `history.push(ChatMessage::system(...))`.
    /// Append only — no upsert; for overwriting the leading system message use
    /// [`Self::SetLeadingSystemPrompt`].
    RecordSystemMessage { content: String },
    /// Ask the reducer to set/replace the leading system prompt — push when history is empty,
    /// otherwise replace `history[0]` (which must have the system role).
    ///
    /// S2-C Step 2: matches the semantics of the chat::mod main loop's `if history.is_empty() { push }
    /// else { first_mut = system }` — this is the system prompt rebuild path that runs on every turn
    /// (the prompt injection after technique selection), and it cannot be expressed with append.
    SetLeadingSystemPrompt { content: String },
    /// Ask the reducer to compact the LLM context history (keep system + the last N messages + a total
    /// budget).
    ///
    /// S2-B Step 1: matches the semantics of chat::mod's `compact_chat_history` — the truncation
    /// happens inside the reducer, with no side effects, producing only a LogTrace. The `reason` field
    /// lets test assertions and the trace tell the context-overflow path from the manual one.
    HistoryCompacted { reason: CompactReason },
    /// Apply an async provider-backed compaction result computed by the Redux
    /// driver. The reducer remains pure: it only validates the guard token and
    /// applies the exact patch, or falls back to deterministic trim on mismatch.
    HistoryCompactionPatchApplied {
        reason: CompactReason,
        patch: crate::agent::loop_::CompactionPatch,
        compaction_config: crate::config::AgentCompactionConfig,
    },
    /// A rollover could not produce a lossless handoff and the turn continued on
    /// a token-aware trim that dropped `dropped_messages` older messages.
    ///
    /// The degradation used to end the turn with a non-retryable `StreamFailed`.
    /// Continuing is the right call, but continuing *silently* replaced a loud
    /// failure with an invisible one: the session keeps answering while its
    /// oldest context is deleted with no summary and no durable event to recover
    /// it from. This action surfaces one short notice instead, and the reducer
    /// collapses repeats so a turn that degrades on every tool iteration still
    /// prints a single line.
    HistoryCompactionDegraded {
        reason: CompactReason,
        dropped_messages: usize,
    },

    // ── UI fold/unfold ──────────────────────────────────────────
    /// Tab — fold/unfold the tool card
    ToolCardFoldToggled,
    /// Legacy direct action for tests/tools that explicitly fold reasoning.
    /// `Ctrl+R` no longer maps here; P6b2 reserves it for reverse-search.
    ReasoningFoldToggled,
    /// Request a redraw
    RedrawRequested,
    /// A system message has been appended to the UI mirror (banner / slash command output / error
    /// notice, ...).
    ///
    /// S2-C Step 2: dual-written with legacy `chat_mirror.lock().push_system_message(text)` — the
    /// reducer pushes the message into `ui.conversation_lines` as Redux's own UI ledger.
    /// **Note**: the actually visible TUI is still rendered from `chat_mirror`; this Action only keeps
    /// a consistent UI state mirror for the Redux path plus test assertions. The legacy mirror is
    /// removed once S2-D/E switches over to Redux as the single source.
    SystemMessageAdded { text: String },
    /// The visual echo of what the user submitted in Pure mode — the reducer pushes one
    /// ConversationLine::User into ui.conversation_lines. In legacy mode chat_mirror.push_user_message
    /// does this; the Pure-mode guard skips the mirror write and uses this Action so the reducer takes
    /// over the echo as the single source.
    UserMessageEchoed(String),
    /// Update of the persistent background-session status line (v1b). An empty `summary` means there
    /// is no background session (the line is hidden). The chat main loop dispatches it as needed after
    /// polling the registry (only when the content changed), and the reducer writes it into
    /// `ui.sessions_status`, which reaches the renderer through `build_ui_snapshot`.
    SessionsStatusUpdated { summary: String },
    /// P1 sessions strip entries. This stays separate from the aggregate
    /// `sessions_status` text so the renderer does not parse display strings.
    SessionsEntriesUpdated {
        entries: Vec<crate::chat::sessions::SwitcherEntry>,
    },
    /// Main-session input backlog status. Updated by the chat main loop when
    /// active-turn input is drained or queued input is popped.
    MainQueueStatusUpdated { status: MainQueueStatus },
    /// Main-session provider worker status. Updated by the chat main loop from
    /// `ProviderTurnWorkerRegistry` so the TUI can show foreground/detached
    /// provider lifecycle progress without parsing logs.
    ProviderWorkerStatusUpdated { status: ProviderWorkerStatus },
    /// Slash-menu argument candidate sources that are not derivable from the
    /// structured live-session strip.
    SlashMenuSourcesUpdated {
        saved_sessions: Vec<crate::chat::session::SavedSessionPickerEntry>,
        provider_model_catalog: Vec<crate::chat::slash_types::SlashProviderModelCatalog>,
    },
    /// `@path` completion candidates sourced by the TUI loop after applying
    /// the file-read security policy. Kept as an Action so the reducer path
    /// never renders with an empty source while the legacy mirror has entries.
    AtPathCandidatesUpdated {
        candidates: Vec<crate::chat::slash_types::AtPathCandidate>,
    },
    /// P2 active line-oriented child session viewport snapshot. `None` clears
    /// the child viewport when focus returns to main or PTY handoff resumes.
    ActiveSessionViewUpdated {
        view: Option<crate::chat::sessions::ActiveSessionView>,
    },
    /// Current context-budget usage for UI-only status display. `used_context_tokens`
    /// is the planned prompt context size for this turn, not cumulative session
    /// token usage.
    ContextWindowUpdated {
        used_context_tokens: Option<usize>,
        max_context_tokens: Option<usize>,
    },
    /// Main-session provider usage has been recorded for a successful turn.
    ProviderUsageRecorded {
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        usage_kind: ProviderUsageRecordKind,
        record: MainSessionTokenUsageRecord,
    },
    /// Record the summary of a background session that reached a terminal state (or was interrupted on
    /// exit) (v4). The chat main loop dispatches it once per finished session surfaced by
    /// `poll_finished`, and once per still-running session on exit. The reducer upserts the summary
    /// (deduplicated by id) into `session.background_sessions`, which is persisted by the next
    /// `SaveSession` and displayed after a reload.
    /// **Summaries only — never rebuild a process/sub-agent/PTY.**
    BackgroundSessionRecorded {
        summary: crate::chat::sessions::PersistedSessionSummary,
    },
    /// The input routing target changed (v1.1b). Dispatched by the chat main loop on
    /// `/attach` / `/detach` (it exclusively owns the authoritative `attached_follow`); the reducer
    /// writes `ui.focus`, which drives the prompt's color + glyph target indicator through the
    /// snapshot. `None` is equivalent to `FocusTarget::Main`.
    SessionFocusChanged { focus: crate::chat::sessions::FocusTarget },
    /// Ctrl+G opens the session switcher overlay (v1.1b). `entries` is the session snapshot at open
    /// time (from the 1s polling cache). The reducer writes `ui.switcher = Some(..)`.
    SwitcherOpened {
        entries: Vec<crate::chat::sessions::SwitcherEntry>,
    },
    /// The switcher selection moved (v1.1b). `selected` is the new highlight index (already clamped to
    /// a valid range by the key thread). The reducer updates `selected` of `ui.switcher`.
    SwitcherMoved { selected: usize },
    /// Close the switcher overlay (v1.1b). The reducer writes `ui.switcher = None`.
    SwitcherClosed,
    /// P7c: open the saved chat-session history picker. Distinct from the
    /// child-TUI Ctrl+G switcher.
    SavedSessionPickerOpened {
        entries: Vec<crate::chat::session::SavedSessionPickerEntry>,
    },
    /// P7c: move the saved chat-session picker highlight.
    SavedSessionPickerMoved { selected: usize },
    /// P7c: close the saved chat-session picker.
    SavedSessionPickerClosed,

    // ── Exit ────────────────────────────────────────────────────
    /// A single Ctrl+C — cancel the current generation
    CancelRequested,
    /// Cancel a specific visible provider turn by scheduler task id.
    CancelProviderTurn {
        task_id: crate::chat::turn_scheduler::TurnTaskId,
    },
    /// Double Ctrl+C / Ctrl+D / SIGTERM — graceful shutdown
    ShutdownRequested,
    /// Last-resort forced exit
    ForceQuit,
}

impl Action {
    /// S2.5 T2.5-2: take the Action variant name as a `'static str` for use as a Prometheus label.
    ///
    /// Kept aligned with the big reduce match; every variant maps to a single string, with no
    /// allocation.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::KeyPressed(_) => "KeyPressed",
            Self::PasteReceived(_) => "PasteReceived",
            Self::TerminalResized { .. } => "TerminalResized",
            Self::InputSubmitted(_) => "InputSubmitted",
            Self::InputReplaced(_) => "InputReplaced",
            Self::HistoryNavigated(_) => "HistoryNavigated",
            Self::InputCancelled => "InputCancelled",
            Self::SlashCommandIssued { .. } => "SlashCommandIssued",
            Self::ModeChanged(_) => "ModeChanged",
            Self::ModelChanged { .. } => "ModelChanged",
            Self::ProviderChanged { .. } => "ProviderChanged",
            Self::HistoryCleared => "HistoryCleared",
            Self::HistoryClearedWithNotice { .. } => "HistoryClearedWithNotice",
            Self::TurnStarted { .. } => "TurnStarted",
            Self::StartLLMTurn { .. } => "StartLLMTurn",
            Self::StreamChunkReceived { .. } => "StreamChunkReceived",
            Self::StreamReasoningReceived { .. } => "StreamReasoningReceived",
            Self::StreamUsageMetered { .. } => "StreamUsageMetered",
            Self::StreamCompleted { .. } => "StreamCompleted",
            Self::ProviderTurnReadyForCommit { .. } => "ProviderTurnReadyForCommit",
            Self::StreamFailed { .. } => "StreamFailed",
            Self::StreamCancelled { .. } => "StreamCancelled",
            Self::ToolStarted { .. } => "ToolStarted",
            Self::ToolFinished { .. } => "ToolFinished",
            Self::ToolProgress { .. } => "ToolProgress",
            Self::ToolApprovalRequested { .. } => "ToolApprovalRequested",
            Self::ToolApprovalReceived { .. } => "ToolApprovalReceived",
            Self::ToolApprovalCleared => "ToolApprovalCleared",
            Self::StreamRetryAttempt { .. } => "StreamRetryAttempt",
            Self::SessionLoaded(_) => "SessionLoaded",
            Self::SessionSaved { .. } => "SessionSaved",
            Self::SessionSwitched { .. } => "SessionSwitched",
            Self::RecordUserTurn(_) => "RecordUserTurn",
            Self::RecordAssistantTurn { .. } => "RecordAssistantTurn",
            Self::RecordSystemMessage { .. } => "RecordSystemMessage",
            Self::SetLeadingSystemPrompt { .. } => "SetLeadingSystemPrompt",
            Self::HistoryCompacted { .. } => "HistoryCompacted",
            Self::HistoryCompactionPatchApplied { .. } => "HistoryCompactionPatchApplied",
            Self::HistoryCompactionDegraded { .. } => "HistoryCompactionDegraded",
            Self::ToolCardFoldToggled => "ToolCardFoldToggled",
            Self::ReasoningFoldToggled => "ReasoningFoldToggled",
            Self::RedrawRequested => "RedrawRequested",
            Self::SystemMessageAdded { .. } => "SystemMessageAdded",
            Self::UserMessageEchoed(_) => "UserMessageEchoed",
            Self::SessionsStatusUpdated { .. } => "SessionsStatusUpdated",
            Self::SessionsEntriesUpdated { .. } => "SessionsEntriesUpdated",
            Self::MainQueueStatusUpdated { .. } => "MainQueueStatusUpdated",
            Self::ProviderWorkerStatusUpdated { .. } => "ProviderWorkerStatusUpdated",
            Self::SlashMenuSourcesUpdated { .. } => "SlashMenuSourcesUpdated",
            Self::AtPathCandidatesUpdated { .. } => "AtPathCandidatesUpdated",
            Self::ActiveSessionViewUpdated { .. } => "ActiveSessionViewUpdated",
            Self::ContextWindowUpdated { .. } => "ContextWindowUpdated",
            Self::ProviderUsageRecorded { .. } => "ProviderUsageRecorded",
            Self::BackgroundSessionRecorded { .. } => "BackgroundSessionRecorded",
            Self::SessionFocusChanged { .. } => "SessionFocusChanged",
            Self::SwitcherOpened { .. } => "SwitcherOpened",
            Self::SwitcherMoved { .. } => "SwitcherMoved",
            Self::SwitcherClosed => "SwitcherClosed",
            Self::SavedSessionPickerOpened { .. } => "SavedSessionPickerOpened",
            Self::SavedSessionPickerMoved { .. } => "SavedSessionPickerMoved",
            Self::SavedSessionPickerClosed => "SavedSessionPickerClosed",
            Self::CancelRequested => "CancelRequested",
            Self::CancelProviderTurn { .. } => "CancelProviderTurn",
            Self::ShutdownRequested => "ShutdownRequested",
            Self::ForceQuit => "ForceQuit",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker_status(sequence: u64) -> ProviderWorkerStatus {
        ProviderWorkerStatus {
            running: 1,
            cancelling: 0,
            awaiting_commit: 0,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: Some(0),
            rows: vec![ProviderWorkerStatusRow {
                task_id: sequence,
                sequence,
                kind: ProviderWorkerRowKind::Detached,
                state: ProviderWorkerRowState::Running,
                started_at_ms: 0,
                finalized_total_tokens: None,
                completion_ready: false,
                recent_tool_call: None,
            }],
        }
    }

    #[test]
    fn phase2_provider_worker_scroll_preserves_top_on_append_and_follows_tail_at_bottom() {
        let status = worker_status(7);
        let old_io: Vec<String> = (0..30).map(|idx| format!("old io {idx}")).collect();
        let old_view = build_provider_worker_active_view_with_io(&status, 7, 3, old_io);
        assert_eq!(old_view.scroll_offset, 3);

        let new_io: Vec<String> = (0..32).map(|idx| format!("new io {idx}")).collect();
        let refreshed =
            build_provider_worker_active_view_with_io_preserving_scroll(&status, 7, Some(&old_view), new_io);
        assert_eq!(
            refreshed.scroll_offset, 5,
            "two appended lines should compensate tail-relative offset"
        );

        let tail_view =
            build_provider_worker_active_view_with_io(&status, 7, 0, (0..30).map(|idx| idx.to_string()).collect());
        let tail_refreshed = build_provider_worker_active_view_with_io_preserving_scroll(
            &status,
            7,
            Some(&tail_view),
            (0..32).map(|idx| idx.to_string()).collect(),
        );
        assert_eq!(tail_refreshed.scroll_offset, 0, "tail-follow offset must remain pinned");
    }
}
