//! History compaction, context-budget status and the legacy compaction audit.

#[cfg(feature = "terminal-tui")]
use super::tui;
use super::{dispatcher, sanitize};
use crate::agent::compaction::apply_compaction_patch_exact;
use crate::agent::compaction::build_configurable_compaction_patch_with_source_history;
use crate::agent::context_budget::measure_history_tokens;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::config::Config;
use crate::memory::CompactionRunInput;
use crate::memory::Memory;
use crate::memory::MemoryCategory;
use crate::memory::MemoryStoreMetadata;
use crate::providers::ChatMessage;
use crate::providers::Provider;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::util::truncate_with_ellipsis;
use std::sync::Arc;

/// Keep last N non-system messages during history compaction.
pub(super) const COMPACT_KEEP_MESSAGES: usize = 8;

/// Per-message character limit during compaction.
pub(super) const COMPACT_CONTENT_CHARS: usize = 320;

/// Total character budget for compacted history (excluding system prompt).
pub(super) const COMPACT_TOTAL_CHARS: usize = 2400;

pub(super) fn estimate_chat_history_tokens(history: &[ChatMessage]) -> usize {
    measure_history_tokens(history)
}

fn format_compact_token_count(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

pub(super) fn format_compact_feedback(
    turns_before: usize,
    turns_after: usize,
    tokens_before: usize,
    tokens_after: usize,
    model_window_tokens: usize,
) -> String {
    let reclaimed = tokens_before.saturating_sub(tokens_after);
    let reclaim_pct = if tokens_before == 0 {
        0
    } else {
        reclaimed.saturating_mul(100).saturating_div(tokens_before).min(100)
    };
    let window = format_compact_token_count(model_window_tokens);
    if turns_before == turns_after && tokens_before == tokens_after {
        format!(
            "Context already compact: {turns_after} turns / ~{tokens_after} tokens / {window} window (nothing to drop)."
        )
    } else {
        format!(
            "Compacted context: {turns_before} -> {turns_after} turns, ~{tokens_before} -> ~{tokens_after} tokens / {window} window; reclaimed ~{reclaimed} tokens ({reclaim_pct}%)."
        )
    }
}

/// One plain line saying that local vector recall is on because the shipped
/// default changed under this installation, not because anyone asked for it.
///
/// Collapsing whitespace guarantees a single line: the notice is rendered both
/// into the TUI transcript, where a newline would break the message frame, and
/// onto stderr, where it has to stay one grep-able line.
pub(crate) fn format_embedding_upgrade_notice(notice: &str) -> String {
    format!("note > {}", notice.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// The line `prx chat` shows for an inherited embedding default, or `None`
/// once the operator has written the key either way.
pub(crate) fn chat_embedding_upgrade_notice(config: &Config) -> Option<String> {
    config
        .embedding_default_upgrade_notice()
        .map(format_embedding_upgrade_notice)
}

/// Print the inherited-default notice where a `prx chat` user can actually see
/// it, before the TUI takes the screen.
///
/// `prx chat` redirects *tracing* into `chat.log`, so the warning `dispatch`
/// logs for every long-running runtime is the one thing chat users never read.
/// The process stderr is not redirected, so this reaches the terminal.
// stderr is the only channel this has: the log goes to a file and the TUI has
// not started yet.
#[allow(clippy::print_stderr)]
pub fn print_embedding_upgrade_notice_to_stderr(config: &Config) {
    if let Some(line) = chat_embedding_upgrade_notice(config) {
        eprintln!("{line}");
    }
}

/// One line telling the user that context was dropped and cannot be recovered.
///
/// A `switch` rollover that cannot resolve exact provenance has no summary and
/// no event to look the dropped messages up with, so the wording has to say
/// "lossy" rather than "compacted": the difference decides whether the user
/// re-states what the model just lost.
pub(crate) fn format_context_degraded_notice(dropped_messages: usize) -> String {
    let plural = if dropped_messages == 1 { "message" } else { "messages" };
    format!("Context trimmed (lossy): {dropped_messages} older {plural} dropped without a recoverable handoff.")
}

/// Whether an explicit `/compact` has anything to compact: more non-system
/// messages in `source_history` than `keep_recent_messages` keeps.
pub(super) fn manual_compact_has_compactable(
    source_history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
) -> bool {
    let system_count = usize::from(source_history.first().is_some_and(|m| m.role == "system"));
    source_history.len().saturating_sub(system_count) > compaction_config.keep_recent_messages.max(1)
}

/// Feedback when `/compact` finds no durable transcript to bind to (D7).
pub(super) const NOTHING_TO_COMPACT_NO_TRANSCRIPT: &str = "Nothing to compact (no durable transcript yet).";

/// Everything an explicit `/compact` works on.
pub(super) struct ManualCompact<'a> {
    pub(super) history: &'a mut Vec<ChatMessage>,
    pub(super) chat_session: &'a super::session::ChatSession,
    pub(super) provider: &'a dyn Provider,
    pub(super) model: &'a str,
    /// The tool-surface-reserved budget (one budget with the preflight).
    pub(super) config: &'a crate::config::AgentCompactionConfig,
    pub(super) memory: Arc<dyn Memory>,
    /// The current chat session's envelope; the audit runtime binds to it.
    pub(super) envelope: &'a RuntimeEnvelope,
    pub(super) chat_dispatcher: &'a dispatcher::ChatDispatcher,
    /// Commit-failure latch for the awaited reducer acknowledgement.
    pub(super) commits: &'a dispatcher::TurnCompletionSignal,
}

/// Feedback when no handoff or summary can be built: the session is left
/// unchanged and each request is trimmed on its own.
pub(super) const NO_DURABLE_COMPACTION_FEEDBACK: &str =
    "No durable compaction available; requests will be trimmed per request. Nothing was changed.";

/// What an explicit `/compact` or a chat preflight compaction did. Callers
/// print from this and refresh the TUI budget; nothing else reports success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ManualCompactOutcome {
    /// The patch was applied to its source and handed to the reducer.
    /// `generation` is the newest handoff generation in the compacted view
    /// (0 when the mode wrote a summary rather than a handoff).
    Compacted {
        generation: u64,
        turns_after: usize,
        tokens_after: usize,
    },
    /// No handoff or summary could be built. Nothing was changed in the
    /// session: a lossy trim is never persisted, so each provider request is
    /// trimmed on its own (a per-request projection) while the session keeps
    /// every turn a later handoff can still recover.
    NoDurableCompaction,
    /// Nothing was changed. `reason` is the user-facing explanation.
    Refused { reason: String },
}

impl ManualCompactOutcome {
    /// The line printed for this outcome. `turns_before` / `tokens_before`
    /// describe the provider view before compaction.
    pub(super) fn feedback(
        &self,
        turns_before: usize,
        tokens_before: usize,
        config: &crate::config::AgentCompactionConfig,
    ) -> String {
        match self {
            Self::Compacted {
                turns_after,
                tokens_after,
                ..
            } => format_compact_feedback(
                turns_before,
                *turns_after,
                tokens_before,
                *tokens_after,
                config.max_context_tokens,
            ),
            Self::NoDurableCompaction => NO_DURABLE_COMPACTION_FEEDBACK.to_owned(),
            Self::Refused { reason } => reason.clone(),
        }
    }
}

/// Non-system message count and token estimate of a provider view.
pub(super) fn chat_history_turns_and_tokens(history: &[ChatMessage]) -> (usize, usize) {
    let system_count = usize::from(history.first().is_some_and(|m| m.role == "system"));
    (
        history.len().saturating_sub(system_count),
        estimate_chat_history_tokens(history),
    )
}

/// Run an explicit `/compact`.
///
/// Forced: every message beyond `keep_recent_messages` is compacted whatever
/// the budget says. The source is the system prompt plus the acknowledged
/// session turns; the reducer receives the same patch. The audit runtime binds
/// to the session's newest persisted user-input event, so `switch` mode writes
/// a durable context-switch handoff. When no handoff or summary can be built
/// the session is left unchanged ([`ManualCompactOutcome::NoDurableCompaction`]):
/// a lossy trim is never persisted.
pub(super) async fn run_manual_compact(
    request: ManualCompact<'_>,
    emit: &mut impl FnMut(&str),
) -> ManualCompactOutcome {
    let ManualCompact {
        history,
        chat_session,
        provider,
        model,
        config,
        memory,
        envelope,
        chat_dispatcher,
        commits,
    } = request;
    let system_count = usize::from(history.first().is_some_and(|m| m.role == "system"));
    let mut source_history = Vec::with_capacity(chat_session.turns.len().saturating_add(system_count));
    if let Some(system) = history.first().filter(|message| message.role == "system") {
        source_history.push(system.clone());
    }
    source_history.extend(super::session_store::session_turns_to_history(chat_session));
    if !manual_compact_has_compactable(&source_history, config) {
        return ManualCompactOutcome::Refused {
            reason: format_nothing_to_compact_feedback(history, config),
        };
    }
    let audit = match DocumentIngestRuntime::from_envelope(memory, envelope)
        .bound_to_newest_user_input()
        .await
    {
        Ok(Some(audit)) => audit,
        Ok(None) => {
            return ManualCompactOutcome::Refused {
                reason: NOTHING_TO_COMPACT_NO_TRANSCRIPT.to_owned(),
            };
        }
        Err(error) => {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::warn!(
                error_len,
                error_sha256,
                "manual compaction could not read the durable transcript"
            );
            return ManualCompactOutcome::Refused {
                reason: format!("Compaction unavailable: the durable transcript could not be read ({error})."),
            };
        }
    };
    emit("Compacting conversation...");
    let patch = build_chat_compaction_patch(
        history,
        &source_history,
        provider,
        model,
        config,
        Some(&audit),
        crate::agent::compaction::MANUAL_COMPACT_TRIGGER,
    )
    .await;
    if let Some(patch) = patch {
        return apply_chat_compaction_patch_and_sync(
            history,
            &source_history,
            None,
            patch,
            config,
            crate::chat::action::CompactReason::Manual,
            chat_dispatcher,
            commits,
        )
        .await;
    }
    ManualCompactOutcome::NoDurableCompaction
}

/// The provider view of an acknowledged snapshot: the current view's leading
/// system prompt (if any) followed by the acknowledged turns. Older-turn
/// enrichment is not carried (R2), so every later acknowledged read describes
/// exactly the messages this view holds.
pub(super) fn provider_view_from_acked(
    current_view: &[ChatMessage],
    acked: &crate::chat::acked_session::AckedSession,
) -> Vec<ChatMessage> {
    let system = current_view
        .first()
        .filter(|message| message.role == "system")
        .map(|message| message.content.as_str());
    super::session_store::provider_view_for_session(&acked.session, system)
}

/// The provider view after a turn whose commit follows a worker (automatic)
/// compaction: rebuilt from the committed acknowledged snapshot, so the next
/// turn never resends the replaced cold prefix. Only the committed request
/// keeps its enrichment (as in the manual and preflight helpers): the newest
/// stored user turn, when it is that request, becomes `enriched_request`.
/// The TUI finalizer uses it after a detached worker compaction, the inline
/// tool loop after an in-turn overflow compaction.
pub(super) fn provider_view_after_compacted_commit(
    current_view: &[ChatMessage],
    acked: &crate::chat::acked_session::AckedSession,
    plain_request: &str,
    enriched_request: &ChatMessage,
) -> Vec<ChatMessage> {
    let mut view = provider_view_from_acked(current_view, acked);
    let plain = crate::agent::turn_contracts::inbound_user_message(plain_request);
    if let Some(slot) = view.iter_mut().rev().find(|message| message.role == "user")
        && slot.content == plain.content
    {
        *slot = enriched_request.clone();
    }
    view
}

/// The provider view after a turn that failed or was cancelled once a
/// compaction had been adopted during it: rebuilt from the acknowledged
/// snapshot, so the next turn never resends the replaced cold prefix. The
/// turn's own request is not kept: a failed or cancelled request is never
/// part of the next turn's view (the reducer rolls its recorded copy back
/// too), so a trailing stored copy of it is dropped.
pub(super) fn provider_view_after_compacted_abort(
    current_view: &[ChatMessage],
    acked: &crate::chat::acked_session::AckedSession,
    plain_request: &str,
) -> Vec<ChatMessage> {
    let mut view = provider_view_from_acked(current_view, acked);
    let plain = crate::agent::turn_contracts::inbound_user_message(plain_request);
    if view
        .last()
        .is_some_and(|message| message.role == "user" && message.content == plain.content)
    {
        view.pop();
    }
    view
}

pub(super) fn format_nothing_to_compact_feedback(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
) -> String {
    let system_count = usize::from(history.first().is_some_and(|m| m.role == "system"));
    let turns = history.len().saturating_sub(system_count);
    let tokens = estimate_chat_history_tokens(history);
    let window = format_compact_token_count(compaction_config.max_context_tokens);
    format!("Nothing to compact: {turns} turns / ~{tokens} tokens / {window} window.")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "terminal-tui")]
pub(super) struct TuiContextBudgetStatus {
    pub(super) used_context_tokens: usize,
    pub(super) max_context_tokens: usize,
}

#[cfg(feature = "terminal-tui")]
pub(super) fn context_budget_status_for_tui(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
    tool_surface_reserve: &crate::agent::context_budget::ToolSurfaceReserve,
    terminal_tui_enabled: bool,
) -> Option<TuiContextBudgetStatus> {
    if !terminal_tui_enabled {
        return None;
    }
    let budget = crate::agent::context_budget::plan_context_budget(
        history,
        &tool_surface_reserve.reserved_config(compaction_config),
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    Some(TuiContextBudgetStatus {
        used_context_tokens: tool_surface_reserve.request_used_tokens(budget.used_tokens),
        max_context_tokens: budget.max_context_tokens,
    })
}

#[cfg(feature = "terminal-tui")]
pub(super) fn refresh_context_budget_for_tui(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
    tool_surface_reserve: &crate::agent::context_budget::ToolSurfaceReserve,
    terminal_tui_enabled: bool,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
) {
    let Some(context_budget) =
        context_budget_status_for_tui(history, compaction_config, tool_surface_reserve, terminal_tui_enabled)
    else {
        return;
    };
    let context_used_tokens = Some(context_budget.used_context_tokens);
    let context_window_tokens = Some(context_budget.max_context_tokens);
    {
        let mut mirror = chat_mirror.lock();
        mirror.context_used_tokens = context_used_tokens;
        mirror.context_window_tokens = context_window_tokens;
    }
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::ContextWindowUpdated {
            used_context_tokens: context_used_tokens,
            max_context_tokens: context_window_tokens,
        },
        "chat.context_window_updated",
    );
}

pub(super) async fn build_chat_compaction_patch(
    budget_history: &[ChatMessage],
    source_history: &[ChatMessage],
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Option<crate::agent::turn_contracts::CompactionPatch> {
    match build_configurable_compaction_patch_with_source_history(
        budget_history,
        source_history,
        provider,
        model,
        config,
        audit,
        trigger,
    )
    .await
    {
        Ok(patch) => patch,
        Err(error) => {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::warn!(
                error_len,
                error_sha256,
                trigger,
                "chat summary compaction failed; no durable compaction this time"
            );
            None
        }
    }
}

/// Validate `patch` against its `source_history`, hand the same patch to the
/// reducer and wait for its acknowledgement; only then is the compacted source
/// the provider view.
///
/// `enriched_request` is the current turn's enriched request (preflight): it
/// replaces the compacted source's final message, the plain request, so the
/// request occurs exactly once. A patch that does not describe its source, or
/// that the reducer rejects, is refused with nothing changed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_chat_compaction_patch_and_sync(
    history: &mut Vec<ChatMessage>,
    source_history: &[ChatMessage],
    enriched_request: Option<ChatMessage>,
    patch: crate::agent::turn_contracts::CompactionPatch,
    config: &crate::config::AgentCompactionConfig,
    reason: crate::chat::action::CompactReason,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    commits: &dispatcher::TurnCompletionSignal,
) -> ManualCompactOutcome {
    let mut compacted = source_history.to_vec();
    if let Err(error) = apply_compaction_patch_exact(&mut compacted, &patch) {
        let range_start_index: usize = patch.range_start();
        let range_end_index: usize = patch.range_end();
        tracing::warn!(
            guard_history_len = patch.guard.history_len,
            range_start_index,
            range_end_index,
            source_len = source_history.len(),
            "chat compaction patch does not describe its source; nothing was changed"
        );
        return ManualCompactOutcome::Refused {
            reason: format!("Compaction refused: {error}. Nothing was changed."),
        };
    }
    // The loop's own bookkeeping notes the patch appends (a summary's refresh
    // aside) are not durable turns; they are put back after the replacement.
    let append_after = patch.append_after.clone();
    let replacement_tail = patch.replacement.last().cloned();
    let acked = match chat_dispatcher
        .dispatch_awaited(
            crate::chat::action::Action::HistoryCompactionPatchApplied {
                reason,
                patch,
                compaction_config: config.clone(),
            },
            commits,
        )
        .await
    {
        Ok(crate::chat::acked_session::Ack::Published(acked)) => acked,
        Ok(crate::chat::acked_session::Ack::Rejected(rejection)) => {
            return ManualCompactOutcome::Refused {
                reason: format!("Compaction refused: {rejection}. Nothing was changed."),
            };
        }
        Ok(crate::chat::acked_session::Ack::Unchanged(_)) => {
            return ManualCompactOutcome::Refused {
                reason: "Compaction refused: the session did not acknowledge the compaction. Nothing was changed."
                    .to_owned(),
            };
        }
        Err(error) => {
            return ManualCompactOutcome::Refused {
                reason: format!("Compaction failed: {error}. Nothing was changed."),
            };
        }
    };
    // The provider view is rebuilt from the acknowledged snapshot, so a later
    // acknowledged read holds exactly these turns. The refresh aside a summary
    // appends lives only in this provider projection, never in the session.
    // A view still over budget is trimmed per request by the turn's own
    // preflight; that trim is never persisted.
    let mut view = provider_view_from_acked(&compacted, &acked);
    if !append_after.is_empty() {
        let insert_at = replacement_tail
            .as_ref()
            .and_then(|tail| {
                view.iter()
                    .rposition(|message| message.role == tail.role && message.content == tail.content)
            })
            .map(|index| index.saturating_add(1));
        if let Some(insert_at) = insert_at {
            view.splice(insert_at..insert_at, append_after);
        }
    }
    if let Some(enriched) = enriched_request {
        replace_final_request(&mut view, source_history.last(), enriched);
    }
    *history = view;
    let (turns_after, tokens_after) = chat_history_turns_and_tokens(history);
    ManualCompactOutcome::Compacted {
        generation: crate::agent::compaction::context_handoff_generation(history),
        turns_after,
        tokens_after,
    }
}

/// Replace the view's final message, when it is the source's plain request,
/// with the enriched request; append the enriched request when the plain one
/// did not survive at the tail. Either way the request occurs exactly once.
pub(super) fn replace_final_request(
    view: &mut Vec<ChatMessage>,
    source_request: Option<&ChatMessage>,
    enriched: ChatMessage,
) {
    let tail_is_request = source_request.is_some_and(|request| {
        request.role == "user"
            && view
                .last()
                .is_some_and(|last| last.role == request.role && last.content == request.content)
    });
    if tail_is_request {
        view.pop();
    }
    view.push(enriched);
}

pub(super) fn bounded_legacy_chat_compaction_audit_source(history: &[ChatMessage]) -> Vec<ChatMessage> {
    let has_system = history.first().is_some_and(|msg| msg.role == "system");
    let start = if has_system { 1 } else { 0 };
    let mut source = Vec::new();
    if let Some(system) = history.first().filter(|_| has_system) {
        source.push(ChatMessage::system(truncate_with_ellipsis(
            &system.content,
            COMPACT_CONTENT_CHARS,
        )));
    }
    let non_system = history.len().saturating_sub(start);
    let keep_start = start + non_system.saturating_sub(COMPACT_KEEP_MESSAGES);
    for msg in history.iter().skip(keep_start) {
        let content = truncate_with_ellipsis(&msg.content, COMPACT_CONTENT_CHARS);
        source.push(ChatMessage {
            role: msg.role.clone(),
            content,
        });
    }
    source
}

pub(super) fn original_legacy_chat_compaction_audit_source(history: &[ChatMessage]) -> Vec<ChatMessage> {
    if history.len() <= 1 {
        return Vec::new();
    }

    let has_system = history.first().is_some_and(|msg| msg.role == "system");
    let start = if has_system { 1 } else { 0 };
    let non_system_count = history.len().saturating_sub(start);
    if non_system_count == 0 {
        return Vec::new();
    }

    let mut lost_indices = std::collections::BTreeSet::new();
    let mut retained: Vec<(usize, &ChatMessage)> = history.iter().enumerate().skip(start).collect();
    if non_system_count > COMPACT_KEEP_MESSAGES {
        let drop_count = non_system_count.saturating_sub(COMPACT_KEEP_MESSAGES);
        for (index, _) in retained.iter().take(drop_count) {
            lost_indices.insert(*index);
        }
        retained.drain(0..drop_count);
    }

    let mut compacted_chars = 0usize;
    let mut budget_retained = Vec::new();
    for (index, msg) in retained {
        if msg.content.chars().count() > COMPACT_CONTENT_CHARS {
            lost_indices.insert(index);
        }
        let compacted_len = msg.content.chars().count().min(COMPACT_CONTENT_CHARS);
        compacted_chars = compacted_chars.saturating_add(compacted_len);
        budget_retained.push((index, compacted_len));
    }

    while compacted_chars > COMPACT_TOTAL_CHARS && budget_retained.len() > 1 {
        let (index, compacted_len) = budget_retained.remove(0);
        lost_indices.insert(index);
        compacted_chars = compacted_chars.saturating_sub(compacted_len);
    }

    history
        .iter()
        .enumerate()
        .filter(|(index, _)| lost_indices.contains(index))
        .map(|(_, msg)| msg.clone())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LegacyCompactionTokenMetadata {
    pub(super) provider_token_estimate: usize,
    pub(super) persisted_token_estimate: usize,
    pub(super) enrichment_token_delta: isize,
}

pub(super) fn legacy_compaction_token_metadata(
    provider_history: &[ChatMessage],
    persisted_history: &[ChatMessage],
) -> LegacyCompactionTokenMetadata {
    let provider_token_estimate = estimate_chat_history_tokens(provider_history);
    let persisted_token_estimate = estimate_chat_history_tokens(persisted_history);
    LegacyCompactionTokenMetadata {
        provider_token_estimate,
        persisted_token_estimate,
        enrichment_token_delta: provider_token_estimate as isize - persisted_token_estimate as isize,
    }
}

pub(super) async fn persist_legacy_chat_compaction_audit(
    mem: &dyn Memory,
    envelope: &RuntimeEnvelope,
    source_history: &[ChatMessage],
    summary_projection: &[ChatMessage],
    token_metadata: LegacyCompactionTokenMetadata,
    trigger: &'static str,
) {
    if source_history.is_empty() {
        return;
    }
    let run_id = uuid::Uuid::new_v4().to_string();
    let summary_memory_key = format!("compaction_summary_{}", run_id.replace('-', "_"));
    let source_message_count = source_history.len();
    let provenance = match crate::agent::compaction::resolve_compaction_event_provenance(
        mem,
        envelope.memory_principal(),
        source_history,
    )
    .await
    {
        Ok(provenance) => provenance,
        Err(error) => {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::debug!(
                error_len,
                error_sha256,
                "failed to resolve legacy chat compaction MessageEvent provenance"
            );
            None
        }
    };
    let provenance_status = if provenance.is_some() { "exact" } else { "unavailable" };
    let fidelity_status = if provenance.is_some() {
        "accepted_legacy_deterministic"
    } else {
        "accepted_legacy_content_hash_fallback"
    };
    let source_event_ids_json = provenance
        .as_ref()
        .and_then(|provenance| serde_json::to_string(&provenance.source_event_ids).ok());
    let source_event_range_json = provenance
        .as_ref()
        .and_then(|provenance| serde_json::to_string(&provenance.covered_range).ok());
    let source_document_refs_json = serde_json::to_string(
        &source_history
            .iter()
            .enumerate()
            .map(|(index, message)| {
                serde_json::json!({
                    "kind": "message_content_hash",
                    "index": index,
                    "role": message.role,
                    "content_sha256": crate::self_system::evolution::safety_utils::sha256_hex(
                        &sanitize::sanitize_for_persistence(&message.content)
                    )
                })
            })
            .collect::<Vec<_>>(),
    )
    .ok();
    let summary = format!(
        "Legacy chat context overflow compaction preserved the system prompt, kept the last {COMPACT_KEEP_MESSAGES} non-system messages, truncated turns to {COMPACT_CONTENT_CHARS} chars, and capped retained chat context at {COMPACT_TOTAL_CHARS} chars."
    );
    let owner = envelope.owner_principal();
    let metadata = MemoryStoreMetadata {
        workspace_id: Some(envelope.workspace_id.clone()),
        owner_id: Some(owner.owner_id.clone()),
        agent_id: envelope.agent_id.clone(),
        persona_id: envelope.persona_id.clone(),
        source_event_id: provenance
            .as_ref()
            .map(|provenance| provenance.covered_range.last_event_id.clone()),
        source: Some("legacy_chat_compaction_summary".to_string()),
        topic_id: None,
        channel: None,
    };
    if let Err(error) = mem
        .store_with_metadata(
            &summary_memory_key,
            &summary,
            MemoryCategory::Conversation,
            Some(&envelope.session_key),
            metadata,
        )
        .await
    {
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::debug!(
            error_len,
            error_sha256,
            "failed to persist legacy chat compaction summary memory"
        );
    }
    let summary_message_event_id = if let Some(provenance) = provenance.as_ref() {
        let raw_payload_json = serde_json::json!({
            "compaction_run_id": run_id,
            "trigger": trigger,
            "mode": "legacy_chat_overflow",
            "fidelity_status": fidelity_status,
            "source_event_ids": provenance.source_event_ids,
            "covered_event_range": provenance.covered_range
        })
        .to_string();
        match mem
            .append_message_event(crate::memory::MessageEventInput {
                agent_partition: envelope.agent_partition.clone(),
                event_id: None,
                idempotency_key: Some(format!("compaction:{run_id}:summary")),
                workspace_id: envelope.workspace_id.clone(),
                owner_id: Some(owner.owner_id.clone()),
                source: "compaction".into(),
                channel: envelope.channel.clone(),
                session_key: Some(envelope.session_key.clone()),
                parent_session_key: None,
                run_id: Some(run_id.clone()),
                parent_run_id: None,
                agent_id: envelope.agent_id.clone(),
                persona_id: envelope.persona_id.clone(),
                sender: Some("compaction".to_string()),
                recipient: envelope.sender.clone(),
                role: "event".to_string(),
                event_type: "compaction.summary.created".to_string(),
                subject: Some(crate::memory::MessageEventSubject::Conversation(
                    envelope.session_key.clone(),
                )),
                goal_id: None,
                causation_event_id: Some(provenance.covered_range.last_event_id.clone()),
                correlation_id: Some(run_id.clone()),
                attempt_id: None,
                lease_epoch: None,
                config_generation_id: envelope.config_generation_id,
                config_source_revision: envelope.config_source_revision.clone(),
                content: sanitize::sanitize_for_persistence(&summary),
                raw_payload_json: Some(raw_payload_json),
                visibility: envelope.visibility.clone(),
            })
            .await
        {
            Ok(event) => Some(event.event_id),
            Err(error) => {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::debug!(
                    error_len,
                    error_sha256,
                    "failed to append legacy chat compaction summary MessageEvent"
                );
                None
            }
        }
    } else {
        None
    };
    if let Err(error) = mem
        .append_compaction_run(CompactionRunInput {
            agent_partition: envelope.agent_partition.clone(),
            run_id: Some(run_id),
            workspace_id: envelope.workspace_id.clone(),
            owner_id: Some(owner.owner_id),
            session_key: Some(envelope.session_key.clone()),
            agent_id: envelope.agent_id.clone(),
            persona_id: envelope.persona_id.clone(),
            trigger: trigger.to_string(),
            mode: "legacy_chat_overflow".to_string(),
            source_message_count,
            source_token_estimate: estimate_chat_history_tokens(source_history),
            summary,
            summary_memory_key: Some(summary_memory_key),
            source_event_ids_json,
            source_event_range_json,
            source_document_refs_json,
            fidelity_status: fidelity_status.to_string(),
            payload_json: Some(
                serde_json::json!({
                    "compact_keep_messages": COMPACT_KEEP_MESSAGES,
                    "compact_content_chars": COMPACT_CONTENT_CHARS,
                    "compact_total_chars": COMPACT_TOTAL_CHARS,
                    "summary_projection_message_count": summary_projection.len(),
                    "summary_projection_token_estimate": estimate_chat_history_tokens(summary_projection),
                    "provider_token_estimate": token_metadata.provider_token_estimate,
                    "persisted_token_estimate": token_metadata.persisted_token_estimate,
                    "enrichment_token_delta": token_metadata.enrichment_token_delta,
                    "source_event_provenance_status": provenance_status,
                    "summary_message_event_id": summary_message_event_id
                })
                .to_string(),
            ),
        })
        .await
    {
        let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
        tracing::debug!(error_len, error_sha256, "failed to append legacy chat compaction run");
    }
}

/// The inline tool loop's compaction sink: chat turns that run the shared
/// loop in the run loop's task (non-TUI builds, draftless turns) hand every
/// overflow compaction to the session authority instead of keeping it as a
/// private mutation of the provider view.
///
/// The turn's request is already recorded in the reducer (`UserTurnAdmitted` at
/// admission), so the reducer's target is the compacted view in its durable
/// form (native tool envelopes and loop notes removed, as for a worker) with
/// the recorded plain request in place of the enriched one. The patch is
/// guarded by the reducer history this sink tracks and is awaited: the reducer
/// applies it with its typed apply, and a refusal changes nothing. Either way
/// [`Self::compacted`] reports that the provider view diverged from the
/// pre-turn state, and the run loop rebuilds that view from the acknowledged
/// session after the turn, so `/compact`, the saved session and the next
/// provider request read one history.
pub(super) struct InlineCompactionSink {
    chat_dispatcher: dispatcher::ChatDispatcher,
    commits: dispatcher::TurnCompletionSignal,
    user_input: String,
    /// The reducer history the next patch is guarded by.
    reducer_history: parking_lot::Mutex<Vec<ChatMessage>>,
    compacted: std::sync::atomic::AtomicBool,
}

impl InlineCompactionSink {
    pub(super) fn new(
        chat_dispatcher: dispatcher::ChatDispatcher,
        commits: dispatcher::TurnCompletionSignal,
        user_input: &str,
        reducer_history: Vec<ChatMessage>,
    ) -> Arc<Self> {
        Arc::new(Self {
            chat_dispatcher,
            commits,
            user_input: user_input.to_owned(),
            reducer_history: parking_lot::Mutex::new(reducer_history),
            compacted: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Whether the loop compacted or trimmed its view during the turn.
    pub(super) fn compacted(&self) -> bool {
        self.compacted.load(std::sync::atomic::Ordering::Acquire)
    }

    fn mark_compacted(&self) {
        self.compacted.store(true, std::sync::atomic::Ordering::Release);
    }

    async fn adopt_patch(
        &self,
        source_history: Vec<ChatMessage>,
        patch: crate::agent::turn_contracts::CompactionPatch,
        config: crate::config::AgentCompactionConfig,
    ) -> anyhow::Result<()> {
        self.mark_compacted();
        let mut compacted = source_history;
        if let Err(error) = apply_compaction_patch_exact(&mut compacted, &patch) {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::warn!(
                error_len,
                error_sha256,
                guard_history_len = patch.guard.history_len,
                source_len = compacted.len(),
                "inline compaction patch does not match its source; reducer patch not sent"
            );
            return Ok(());
        }
        let mut target = dispatcher::reducer_history_from_compacted_worker(&compacted);
        target.push(crate::agent::turn_contracts::inbound_user_message(&self.user_input));
        let reducer_patch = {
            let source = self.reducer_history.lock();
            let guard = crate::agent::compaction::compaction_patch_guard_for(&source, 0, source.len())
                .ok_or_else(|| anyhow::anyhow!("invalid reducer compaction source"))?;
            crate::agent::turn_contracts::CompactionPatch {
                replacement: target.clone(),
                append_after: Vec::new(),
                guard,
            }
        };
        let ack = self
            .chat_dispatcher
            .dispatch_awaited(
                crate::chat::action::Action::HistoryCompactionPatchApplied {
                    reason: crate::chat::action::CompactReason::ContextOverflow,
                    patch: reducer_patch,
                    compaction_config: config,
                },
                &self.commits,
            )
            .await
            .map_err(|error| anyhow::anyhow!("Chat stopped after an unconfirmed history commit: {error}"))?;
        match ack {
            crate::chat::acked_session::Ack::Published(_) => *self.reducer_history.lock() = target,
            crate::chat::acked_session::Ack::Unchanged(_) | crate::chat::acked_session::Ack::Rejected(_) => {
                tracing::warn!(
                    replacement_len = target.len(),
                    "inline compaction was not adopted by the session; the provider view follows the acknowledged session after the turn"
                );
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::agent::turn_contracts::ToolLoopEventSink for InlineCompactionSink {
    async fn emit(&self, event: crate::agent::turn_contracts::ToolLoopEvent) -> anyhow::Result<()> {
        match event {
            crate::agent::turn_contracts::ToolLoopEvent::ContextCompactionPatch {
                source_history,
                patch,
                config,
                ..
            } => self.adopt_patch(source_history, patch, config).await,
            // Text and tool progress reach the terminal through the turn's
            // own delta and tool-call channels.
            crate::agent::turn_contracts::ToolLoopEvent::TextDelta(_)
            | crate::agent::turn_contracts::ToolLoopEvent::ReasoningDelta(_)
            | crate::agent::turn_contracts::ToolLoopEvent::RetryAttempt { .. }
            | crate::agent::turn_contracts::ToolLoopEvent::ToolStarted { .. }
            | crate::agent::turn_contracts::ToolLoopEvent::ToolFinished { .. } => Ok(()),
        }
    }
}

/// Bring the run loop's provider view up to the committed session after an
/// inline turn's commit. When the view changed during the turn other than by
/// appending (`changed_in_turn`: a reported compaction, or a trim that was the
/// projection of one request) or the acknowledged compaction epoch moved
/// since `compaction_epoch_before_preflight` (the preflight's own compaction
/// included), the view is rebuilt from the
/// committed snapshot with the committed request in its enriched form
/// (`committed`). A failed or cancelled turn is settled instead
/// (`provider_turns::settle_aborted_turn`). Returns whether it rebuilt the view.
pub(super) fn reconcile_inline_turn(
    history: &mut Vec<ChatMessage>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
    changed_in_turn: bool,
    compaction_epoch_before_preflight: u64,
    user_input: &str,
    committed: &ChatMessage,
) -> bool {
    let acked = chat_dispatcher.session_reader().acked();
    if acked.compaction_epoch == compaction_epoch_before_preflight && !changed_in_turn {
        return false;
    }
    let view = provider_view_after_compacted_commit(history, &acked, user_input, committed);
    tracing::info!(
        messages_before = history.len(),
        messages_after = view.len(),
        "inline turn's provider view rebuilt from the committed session after a compaction"
    );
    *history = view;
    true
}

/// The digest of the provider view an inline turn starts from. A turn only
/// appends to its view, unless it compacted, trimmed or rewrote it; then the
/// view no longer starts with this prefix and must follow the session again.
pub(super) struct ViewPrefix {
    len: usize,
    hash: String,
}

impl ViewPrefix {
    pub(super) fn of(view: &[ChatMessage]) -> Self {
        Self {
            len: view.len(),
            hash: crate::agent::compaction::compaction_patch_guard_for(view, 0, view.len())
                .map(|guard| guard.replaced_hash)
                .unwrap_or_default(),
        }
    }

    /// Whether `view` changed other than by appending since [`Self::of`].
    pub(super) fn changed_in(&self, view: &[ChatMessage]) -> bool {
        view.get(..self.len)
            .and_then(|prefix| crate::agent::compaction::compaction_patch_guard_for(prefix, 0, prefix.len()))
            .is_none_or(|guard| guard.replaced_hash != self.hash)
    }
}
