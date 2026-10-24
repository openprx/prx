use crate::approval::ApprovalRequest;
use crate::llm::route_decision::TokenUsage;
use crate::providers::ChatMessage;
use crate::security::SecurityPolicy;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::Arc;

/// Interactive chat operating mode. Aligned with codex CLI's `plan`/`edit`/`auto`
/// concepts. Lives in the lib crate so non-chat callers (channels, gateway,
/// delegate, sessions_spawn, …) can default it to [`ChatMode::Edit`].
///
/// - [`ChatMode::Plan`] — read-only: write/shell/git-mutating tools are
///   intercepted by `execute_one_unrecorded` (`agent/tool_scheduler.rs`, reached via
///   `execute_tools_with_service`) before execution and a simulated
///   "[plan mode] would call <tool>" result is fed back to the model. Read
///   tools (file_read, grep, web_search, memory_recall, …) execute normally.
/// - [`ChatMode::Edit`] — default. No filtering: all tools that pass the
///   normal approval/policy pipeline run.
/// - [`ChatMode::Auto`] — same tool set as `Edit`. Reserved for future
///   "skip approval prompts" behaviour; today the chat-loop already does not
///   block on interactive prompts, so the practical difference vs `Edit` is
///   the user-visible label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChatMode {
    /// Plan-only: simulate write tools, run read tools normally.
    Plan,
    /// Default — all tools enabled, normal approval flow.
    #[default]
    Edit,
    /// All tools, approval prompts suppressed.
    Auto,
}

impl ChatMode {
    /// Short label for status / confirmation messages.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Edit => "edit",
            Self::Auto => "auto",
        }
    }

    /// Returns `true` when this mode should intercept write/mutating tools
    /// and synthesize a simulated result instead of executing them.
    pub const fn intercepts_writes(self) -> bool {
        matches!(self, Self::Plan)
    }
}

/// Lightweight notification for tool call progress (used by chat/TUI integration).
#[derive(Debug, Clone)]
pub enum ToolCallNotification {
    /// A tool call is about to be executed.
    Started { name: String, args_summary: String },
    /// A tool call has finished executing.
    Finished {
        name: String,
        success: bool,
        duration_ms: u64,
    },
    /// Progress indication for multi-iteration tool loops.
    Progress { iteration: usize },
}

/// Context for scope-based tool access control.
/// When present, `run_turn` will check each tool call against
/// the security policy's scope rules before execution.
pub(crate) struct ScopeContext<'a> {
    pub policy: &'a SecurityPolicy,
    pub sender: &'a str,
    pub channel: &'a str,
    pub chat_type: &'a str,
    pub chat_id: &'a str,
    pub session_key: Option<&'a str>,
    pub owner_id: Option<&'a str>,
    pub owner_isolated: bool,
    pub topic_id: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub source_message_event_id: Option<&'a str>,
    pub config_generation_id: Option<u64>,
    pub config_source_revision: Option<&'a str>,
}

impl<'a> ScopeContext<'a> {
    pub(crate) fn from_execution_context(
        policy: &'a SecurityPolicy,
        context: &'a crate::tools::ToolExecutionContext,
        chat_id: &'a str,
    ) -> Self {
        let envelope = &context.envelope;
        Self {
            policy,
            sender: envelope.sender.as_deref().unwrap_or_default(),
            channel: envelope.channel.as_deref().unwrap_or_default(),
            chat_type: context.chat_type.as_str(),
            chat_id,
            session_key: Some(envelope.session_key.as_str()),
            owner_id: envelope.owner_id.as_deref(),
            owner_isolated: envelope.owner_isolated,
            topic_id: envelope.topic_id.as_deref(),
            task_id: envelope.task_id.as_deref(),
            source_message_event_id: envelope.source_message_event_id.as_deref(),
            config_generation_id: envelope.config_generation_id,
            config_source_revision: envelope.config_source_revision.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPatchGuard {
    pub history_len: usize,
    pub range_start: usize,
    pub range_end: usize,
    pub replaced_hash: String,
}

#[derive(Debug, Clone)]
pub struct CompactionPatch {
    pub range_start: usize,
    pub range_end: usize,
    pub replacement: Vec<ChatMessage>,
    pub append_after: Vec<ChatMessage>,
    pub guard: CompactionPatchGuard,
}

/// Opens the loop-authored tool-result message in prompt-guided mode.
/// Producers, history restoration and reserved-marker checks share these exact
/// bytes. Recalled or user-supplied text does not acquire loop authority by
/// mentioning the marker; ingress still neutralizes reserved prefixes.
pub(crate) const TOOL_RESULTS_PREFIX: &str = "[Tool results]";

/// The marker opening the refresh aside summary compaction appends after a
/// replaced range. The producer (`post_compaction_refresh_aside`) and every
/// matcher ([`is_loop_written_user_note`]) share it, so the matchers recognise
/// exactly what compaction writes.
pub(crate) const POST_COMPACTION_REFRESH_MARKER: &str = "[Post-compaction context refresh]";

/// Marker prefix used for recalled page content injected back into history.
pub(crate) const OS_PAGING_RECALL_MARKER: &str = "[Recalled context from earlier in this session]";

/// Every marker the loop opens one of its own `user`-role notes with: the
/// prompt-guided tool-result block ([`TOOL_RESULTS_PREFIX`]),
/// the post-compaction refresh aside and a recalled OS-paging page.
///
/// A `user` message that starts with one of them is read as the loop's
/// bookkeeping, never as the user's question: the recall query, the preflight
/// floor that keeps this turn's request, and the durable chat transcript all
/// skip it. So a user must not be able to write one. Inbound user text passes
/// through [`neutralize_reserved_markers`] before it enters a history.
pub(crate) const RESERVED_USER_NOTE_MARKERS: [&str; 3] = [
    TOOL_RESULTS_PREFIX,
    POST_COMPACTION_REFRESH_MARKER,
    OS_PAGING_RECALL_MARKER,
];

/// Put in front of inbound user text that opens with a reserved marker: a
/// ZERO WIDTH SPACE. The model reads the same words, `trim` does not remove it
/// (it is not whitespace), and no reserved-marker predicate matches the
/// message any more.
pub(crate) const RESERVED_MARKER_ESCAPE: char = '\u{200b}';

/// Whether `content` opens with one of [`RESERVED_USER_NOTE_MARKERS`], exactly.
pub(crate) fn is_loop_written_user_note(content: &str) -> bool {
    RESERVED_USER_NOTE_MARKERS
        .iter()
        .any(|marker| content.starts_with(marker))
}

/// Inbound user text as it may enter a history: unchanged, unless it opens
/// with a reserved marker, which [`RESERVED_MARKER_ESCAPE`] then defuses.
pub(crate) fn neutralize_reserved_markers(text: &str) -> std::borrow::Cow<'_, str> {
    if is_loop_written_user_note(text) {
        std::borrow::Cow::Owned(format!("{RESERVED_MARKER_ESCAPE}{text}"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// The one way user-authored text enters a history: a `user` message whose
/// content cannot open with a reserved marker. Every ingress uses it (pinned by
/// the architecture guard `user_text_enters_histories_only_neutralized`).
pub(crate) fn inbound_user_message(text: &str) -> ChatMessage {
    ChatMessage::user(neutralize_reserved_markers(text))
}

/// Fresh inbound text already owned by the caller. Preserve its allocation
/// unless a reserved opening needs the same escape as borrowed input.
pub(crate) fn inbound_user_message_owned(text: String) -> ChatMessage {
    let content = match neutralize_reserved_markers(&text) {
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(escaped) => escaped,
    };
    ChatMessage::user(content)
}

/// A stored conversation turn (a chat session turn, a channel or console
/// transcript row) rebuilt into a history message. A `user` turn is
/// neutralised like fresh input: it may have been stored before inbound text
/// was neutralised.
pub(crate) fn stored_turn_message(role: String, content: String) -> ChatMessage {
    let content = if role == "user" {
        match neutralize_reserved_markers(&content) {
            std::borrow::Cow::Borrowed(_) => content,
            std::borrow::Cow::Owned(escaped) => escaped,
        }
    } else {
        content
    };
    ChatMessage { role, content }
}

#[derive(Debug)]
pub(crate) struct ToolLoopCancelled;

impl std::fmt::Display for ToolLoopCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tool loop cancelled")
    }
}

impl std::error::Error for ToolLoopCancelled {}

pub(crate) fn is_tool_loop_cancelled(err: &anyhow::Error) -> bool {
    err.chain().any(|source| source.is::<ToolLoopCancelled>())
}

// ── Agent Tool-Call Loop ──────────────────────────────────────────────────
// Core agentic iteration: send conversation to the LLM, parse any tool
// calls from the response, execute them, append results to history, and
// repeat until the LLM produces a final text-only answer.
//
// Loop invariant: at the start of each iteration, `history` contains the
// full conversation so far (system prompt + user messages + prior tool
// results). The loop exits when:
//   • the LLM returns no tool calls (final answer), or
//   • the cancellation token fires (external abort).

/// Provider-attribution trace for a completed (or returning) tool-call loop.
///
/// FIX-P0-30/31: the final answer alone drops the provider/model that actually
/// served the request and the failover attempt sequence. `run_turn` returns
/// them next to the outcome in this struct so the chat orchestration layer can
/// attribute the provider
/// outcome to the *real* serving model instead of the routed
/// `decision.selected.model`.
///
/// Multi-turn note: an agent loop may call the provider once per iteration
/// (each tool round-trip is a separate `chat` call). The `final_*` / `attempts`
/// fields carry the attribution of the **final, returning turn** — the turn
/// that produced the text answer handed back to the caller — since that is the
/// most meaningful single attribution for the response the user sees.
///
/// FIX #2: `any_turn_had_fallback` additionally aggregates across *every* turn:
/// it is `true` when any turn (intermediate or final) recovered via a
/// retry/provider/model fallback. Without it, a tool-call turn that fell back
/// followed by a clean final-answer turn would be reported as a clean
/// `Success`, silently erasing the fact that a fallback occurred during the
/// user turn. The chat layer ORs this into the returning turn's own
/// classification so the recorded status reflects the whole user turn. Full
/// per-turn attempt-detail aggregation remains out of scope (only the status
/// signal is aggregated, kept deliberately lightweight).
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolLoopTrace {
    /// Parameters prepared by real request serializers, in preparation order.
    pub prepared_parameters: Vec<crate::providers::catalog::parameters::ResolvedParameters>,
    /// Provider that produced the final returning turn, if a provider call ran.
    pub final_provider: Option<String>,
    /// Model that produced the final returning turn, if a provider call ran.
    pub final_model: Option<String>,
    /// Attempt sequence of the final returning turn (failures + terminal
    /// success). Empty when no provider call completed (e.g. early cancel).
    pub attempts: Vec<crate::llm::route_decision::ProviderAttempt>,
    /// FIX #2: whether *any* turn in the loop (not just the returning one)
    /// recovered through a retry or provider/model fallback. A turn had a
    /// fallback when its `chat_traced` recorded more than one attempt (each
    /// failed attempt before the terminal success adds an entry).
    pub any_turn_had_fallback: bool,
    /// Aggregated provider usage across successful provider calls in this user
    /// turn. Empty/estimated by default until provider capture lands.
    pub tokens_used: TokenUsage,
    /// Actual content-free tool observations from the decision-owned service.
    pub execution_trace: Option<crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
}

/// Content-free accounting retained by a terminal owner across future drop.
#[derive(Default)]
pub(crate) struct RetainedProviderTrace {
    state: parking_lot::Mutex<RetainedProviderState>,
}

#[derive(Default)]
struct RetainedProviderState {
    completed: Option<ToolLoopTrace>,
    in_flight: bool,
    uncertain: bool,
}

impl RetainedProviderTrace {
    pub(crate) fn begin(&self) {
        self.state.lock().in_flight = true;
    }

    pub(crate) fn failed(&self) {
        let mut state = self.state.lock();
        state.in_flight = false;
        state.uncertain = true;
    }

    pub(crate) fn completed(&self, trace: &ToolLoopTrace, usage: TokenUsage, had_fallback: bool, reported: bool) {
        let mut snapshot = trace.clone();
        for attempt in &mut snapshot.attempts {
            attempt.error_message = None;
        }
        snapshot.tokens_used = usage;
        snapshot.any_turn_had_fallback = had_fallback;
        let mut state = self.state.lock();
        // A final successful response does not report the billing of earlier
        // failed attempts inside the provider's retry/failover chain.
        state.uncertain |= !reported
            || snapshot
                .attempts
                .iter()
                .any(|attempt| attempt.status == crate::llm::route_decision::AttemptStatus::Failed);
        state.completed = Some(snapshot);
        state.in_flight = false;
    }

    pub(crate) fn snapshot(&self) -> (Option<ToolLoopTrace>, bool) {
        let state = self.state.lock();
        (state.completed.clone(), state.in_flight || state.uncertain)
    }

    pub(crate) fn has_unknown_usage(&self) -> bool {
        let state = self.state.lock();
        state.in_flight || state.uncertain
    }
}

/// Neutral event stream emitted by the existing turn owner.
///
/// Most ingress paths consume the loop as a buffered call/return API. Chat's
/// TUI supplies a sink so the same owner can project true provider deltas and
/// recovery/tool progress without importing Redux actions into `agent::turn_engine`.
#[derive(Debug, Clone)]
pub(crate) enum ToolLoopEvent {
    TextDelta(String),
    ReasoningDelta(String),
    RetryAttempt {
        attempt: u8,
        reason: String,
    },
    ContextCompacted,
    ContextCompactionPatch {
        patch: CompactionPatch,
        config: crate::config::AgentCompactionConfig,
        turns_before: usize,
        tokens_before: usize,
        turns_after: usize,
        tokens_after: usize,
        used_context_tokens: usize,
    },
    ToolStarted {
        tool_call_id: String,
        name: String,
        args: String,
    },
    ToolFinished {
        tool_call_id: String,
        name: String,
        success: bool,
        duration_ms: u64,
        result: String,
    },
}

#[async_trait::async_trait]
pub(crate) trait ToolLoopEventSink: Send + Sync {
    async fn emit(&self, event: ToolLoopEvent) -> Result<()>;
}

/// Optional adapters for an ingress that needs live provider events and an
/// already-assembled canonical tool execution service.
///
/// Supplying this does not create a second loop: provider iterations, overflow
/// recovery, history mutation, usage aggregation, cancellation, and terminal
/// outcome remain inside `turn_engine::run_turn`.
#[derive(Clone)]
pub(crate) struct ToolLoopRuntimeAdapter {
    /// Opt-in for interactive chat; other hosts retain their completion contract.
    pub verify_chat_completion: bool,
    pub provider_trace: Option<Arc<RetainedProviderTrace>>,
    pub events: Option<Arc<dyn ToolLoopEventSink>>,
    pub stream_provider: bool,
    pub tool_execution_service: Option<Arc<crate::tools::ToolExecutionService>>,
    pub tool_execution_context: crate::tools::ToolExecutionContext,
    /// Request-local execution allowlist compiled from the exact ToolSpecs
    /// advertised to the model. `None` is retained for direct scheduler tests;
    /// the production loop always replaces it with the current turn snapshot.
    pub allowed_tool_names: Option<Arc<HashSet<String>>>,
}

#[derive(Debug)]
struct ToolLoopEventSinkClosed;

impl std::fmt::Display for ToolLoopEventSinkClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("turn event sink closed")
    }
}

impl std::error::Error for ToolLoopEventSinkClosed {}

pub(crate) fn is_tool_loop_event_sink_closed(err: &anyhow::Error) -> bool {
    err.chain().any(|source| source.is::<ToolLoopEventSinkClosed>())
}

/// Terminal outcome of the agent tool-call loop.
///
/// The loop produces a text answer, or a silent turn when the model decided
/// to stay silent. Callers that need the historical `String` collapse the
/// outcome with [`ToolLoopOutcome::into_text`].
#[derive(Debug, Clone)]
pub(crate) enum ToolLoopOutcome {
    /// The loop produced a final text answer.
    Text(String),
    /// A source-owned pending request was persisted; no further model iteration.
    AwaitingApproval { reply: String },
    /// The loop intentionally produced no user-facing text.
    ///
    /// `reason` is a short machine/operator-readable explanation of why the
    /// turn stayed silent (e.g. an explicit `stay_silent` decision).
    Silent { reason: String },
}

impl ToolLoopOutcome {
    /// Collapse the outcome to the historical `String` representation: a
    /// [`Text`](Self::Text) maps to its inner string, a silent turn to an
    /// empty one.
    pub(crate) fn into_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::AwaitingApproval { reply } => reply,
            Self::Silent { reason } => {
                tracing::debug!(
                    reason = reason.as_str(),
                    "tool loop produced a silent outcome on a legacy text path"
                );
                String::new()
            }
        }
    }
}

/// Decision returned by an [`ApprovalResolver`] for a pending tool call:
/// chat's approval path and its `/approve` / `/deny` handler construct it.
#[derive(Debug, Clone)]
pub(crate) enum ApprovalDecision {
    /// Deny the call; the loop substitutes a denial result.
    Deny,
    /// Allow the call and inject a runtime approval grant, mirroring the
    /// interactive `Yes`/`Always` track that mints an `ApprovalGrantV2`.
    Grant,
}

/// Out-of-band approval decision point for the tool-call loop.
///
/// Phase-0 foundation: when an [`ApprovalResolver`] is supplied, the loop awaits
/// it at the approval decision point instead of falling back to the synchronous
/// CLI prompt / implicit deny. This is the seam phase 1 uses to drive
/// TUI approval cards / suspended-turn resumption.
///
/// **Behavior-preserving for non-opt-in callers:** a caller that threads `None`
/// keeps the existing `SideEffectGate` / CLI-prompt behavior. Only the chat
/// `/bg` background path supplies a resolver (to suspend on the approval gate);
/// channels / gateway keep `None`.
#[async_trait::async_trait]
pub(crate) trait ApprovalResolver: Send + Sync {
    /// Resolve an approval decision for a tool call after the authoritative
    /// `SecurityPolicy::decide` returned `Ask`.
    ///
    /// Implementations may await an external decision (e.g. a UI card). The
    /// returned [`ApprovalDecision`] determines whether the call proceeds, is
    /// denied, or proceeds with a runtime grant.
    async fn resolve(&self, request: &ApprovalRequest, channel: &str) -> ApprovalDecision;
}

pub(super) async fn emit_tool_loop_event(adapter: &ToolLoopRuntimeAdapter, event: ToolLoopEvent) -> Result<()> {
    // Anything reaching a user-visible sink is progress by definition.
    crate::agent::idle::beat(crate::agent::idle::ProgressKind::ChannelOutput);
    let Some(events) = adapter.events.as_ref() else {
        return Ok(());
    };
    events
        .emit(event)
        .await
        .map_err(|_| anyhow::Error::new(ToolLoopEventSinkClosed))
}

/// Internal scheduler control signal emitted only from a typed service status.
/// Model text cannot manufacture a pending approval transition.
#[derive(Debug, thiserror::Error)]
#[error("tool loop awaits durable approval")]
pub(crate) struct ToolLoopAwaitingApproval {
    pub(crate) reply: String,
}
