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

/// Compact conversation history in-place to fit within context window limits.
///
/// Preserves the system prompt (index 0), keeps the last [`COMPACT_KEEP_MESSAGES`]
/// non-system messages, truncates each to [`COMPACT_CONTENT_CHARS`], and enforces
/// a total character budget of [`COMPACT_TOTAL_CHARS`].
pub(super) fn compact_chat_history(history: &mut Vec<ChatMessage>) {
    if history.len() <= 1 {
        return;
    }

    // Separate system prompt from conversation turns
    let has_system = history.first().map(|m| m.role == "system").unwrap_or(false);
    let start = if has_system { 1 } else { 0 };

    // Keep only the last COMPACT_KEEP_MESSAGES conversation turns
    let turn_count = history.len() - start;
    if turn_count > COMPACT_KEEP_MESSAGES {
        let drain_end = start + turn_count - COMPACT_KEEP_MESSAGES;
        history.drain(start..drain_end);
    }

    // Truncate individual messages
    for msg in history.iter_mut().skip(start) {
        if msg.content.chars().count() > COMPACT_CONTENT_CHARS {
            msg.content = truncate_with_ellipsis(&msg.content, COMPACT_CONTENT_CHARS);
        }
    }

    // Enforce total character budget (drop oldest turns first)
    while history
        .iter()
        .skip(start)
        .map(|m| m.content.chars().count())
        .sum::<usize>()
        > COMPACT_TOTAL_CHARS
        && history.len() > start + 1
    {
        history.remove(start);
    }
}

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

pub(super) fn manual_compact_below_trigger_threshold(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
) -> bool {
    !crate::agent::context_budget::plan_context_budget(
        history,
        compaction_config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    )
    .over_hard_limit
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

pub(super) fn format_compact_feedback_after_history(
    turns_before: usize,
    tokens_before: usize,
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
) -> String {
    let system_count = usize::from(history.first().is_some_and(|m| m.role == "system"));
    format_compact_feedback(
        turns_before,
        history.len().saturating_sub(system_count),
        tokens_before,
        estimate_chat_history_tokens(history),
        compaction_config.max_context_tokens,
    )
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
    terminal_tui_enabled: bool,
) -> Option<TuiContextBudgetStatus> {
    if !terminal_tui_enabled {
        return None;
    }
    let budget = crate::agent::context_budget::plan_context_budget(
        history,
        compaction_config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    Some(TuiContextBudgetStatus {
        used_context_tokens: budget.used_tokens,
        max_context_tokens: budget.max_context_tokens,
    })
}

#[cfg(feature = "terminal-tui")]
pub(super) fn refresh_context_budget_for_tui(
    history: &[ChatMessage],
    compaction_config: &crate::config::AgentCompactionConfig,
    terminal_tui_enabled: bool,
    chat_mirror: &Arc<parking_lot::Mutex<tui::TuiState>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
) {
    let Some(context_budget) = context_budget_status_for_tui(history, compaction_config, terminal_tui_enabled) else {
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
    trigger: &str,
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
            tracing::warn!(%error, trigger, "chat summary compaction failed; falling back to deterministic trim");
            None
        }
    }
}

pub(super) fn apply_chat_compaction_patch_and_sync(
    history: &mut Vec<ChatMessage>,
    patch_source_history: Option<&[ChatMessage]>,
    patch: crate::agent::turn_contracts::CompactionPatch,
    config: &crate::config::AgentCompactionConfig,
    reason: crate::chat::action::CompactReason,
    chat_dispatcher: &dispatcher::ChatDispatcher,
) {
    let replacement_len = patch.replacement.len();
    if let Some(source_history) = patch_source_history {
        let mut source_history = source_history.to_vec();
        apply_compaction_patch_exact(&mut source_history, &patch);
        *history = source_history;
    } else {
        apply_compaction_patch_exact(history, &patch);
    }
    let budget = crate::agent::context_budget::plan_context_budget(
        history,
        config,
        crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD,
    );
    if budget.over_hard_limit {
        let trimmed =
            crate::agent::context_budget::trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
                history,
                config,
                replacement_len,
            );
        tracing::warn!(
            used_tokens = budget.used_tokens,
            hard_limit = budget.available_input_tokens,
            trimmed,
            "chat summary compaction applied preserving trim"
        );
    }
    let _ = chat_dispatcher.dispatch_or_log(
        crate::chat::action::Action::HistoryCompactionPatchApplied {
            reason,
            patch,
            compaction_config: config.clone(),
        },
        "chat.history_compaction_patch_applied",
    );
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
    trigger: &str,
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
            tracing::debug!(error = %error, "failed to resolve legacy chat compaction MessageEvent provenance");
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
        tracing::debug!(error = %error, "failed to persist legacy chat compaction summary memory");
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
                tracing::debug!(error = %error, "failed to append legacy chat compaction summary MessageEvent");
                None
            }
        }
    } else {
        None
    };
    if let Err(error) = mem
        .append_compaction_run(CompactionRunInput {
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
        tracing::debug!(error = %error, "failed to append legacy chat compaction run");
    }
}
