use crate::agent::compaction::{
    CompactionApply, apply_configurable_compaction_over_view, persist_context_degradation_with_details,
};
use crate::agent::context_budget::{
    ContextBudgetSnapshot, FitDemand, PRE_TURN_FLUSH_THRESHOLD, budgeted_request_tool_tokens, budgeted_tokens,
    estimate_budgeted_request_tokens, plan_context_budget, reserve_request_tool_surface, token_source_label,
};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::history::latest_user_query_index;
use crate::agent::turn_history::TurnHistory;
use crate::providers::{ChatMessage, Provider};
use anyhow::Result;

#[cfg(test)]
pub(super) async fn preflight_context_budget_before_provider_call(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    tool_surface: crate::agent::turn_context::RequestToolMeasurement,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<ContextBudgetSnapshot> {
    let config = preflight_config(config, tool_surface);
    let mut turn_history = TurnHistory::new(history, None, false);
    let compaction = preflight_durable_compaction(
        history,
        &mut turn_history,
        provider,
        model,
        &config,
        tool_surface,
        audit,
        trigger,
    )
    .await?;
    if compaction.source.is_some() {
        turn_history.finish_durable_compaction(history);
        let projected = turn_history.project(history);
        turn_history.adopt(history, projected);
    }
    preflight_trim_projection(history, &mut turn_history, &config, &compaction, audit, trigger).await
}

/// The compaction config one provider request is budgeted with. The
/// published tool schemas ride in the same context window as the messages,
/// and the prompt sections compiled from them are appended to the system
/// message after the preflight, so the window left for history is the
/// configured one minus both. Reserving them keeps a single budget policy: the
/// warning threshold, summary compaction and the token-aware trim all move
/// together, and a request that publishes no tool at all is budgeted exactly
/// as configured.
pub(super) fn preflight_config(
    config: &crate::config::AgentCompactionConfig,
    tool_surface: crate::agent::turn_context::RequestToolMeasurement,
) -> std::borrow::Cow<'_, crate::config::AgentCompactionConfig> {
    if budgeted_request_tool_tokens(tool_surface) == 0 {
        std::borrow::Cow::Borrowed(config)
    } else {
        std::borrow::Cow::Owned(reserve_request_tool_surface(config, tool_surface))
    }
}

/// What the durable phase of the preflight did.
#[derive(Default)]
pub(super) struct PreflightCompaction {
    /// The canonical history before the summary or handoff this phase applied
    /// to it; taken by the caller that reports the compaction.
    pub(super) source: Option<crate::agent::turn_history::CanonicalHistory>,
    /// Why a `switch` rollover could not stay lossless.
    degraded_reason: Option<&'static str>,
}

/// The durable phase of the hard budget preflight before a provider call.
/// When the provider view is over the hard limit, the summary or handoff is
/// built from and applied to the turn's canonical history, never to the view:
/// the view may hold this turn's earlier trims, and a replacement built over
/// them would drop the turns they left out for good. The caller reports an
/// applied compaction to its session authority and re-derives the view from
/// the canonical history before [`preflight_trim_projection`].
pub(super) async fn preflight_durable_compaction(
    view: &[ChatMessage],
    turn_history: &mut TurnHistory,
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    tool_surface: crate::agent::turn_context::RequestToolMeasurement,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<PreflightCompaction> {
    let budget = plan_context_budget(view, config, PRE_TURN_FLUSH_THRESHOLD);
    if budget.over_warning {
        let token_source: &'static str = token_source_label(budget.measurement_source);
        tracing::info!(
            used_tokens = budget.used_tokens,
            measured_tokens = budget.measured_tokens,
            native_tool_tokens = budgeted_tokens(tool_surface.native_payload.measurement),
            instruction_tokens = budgeted_tokens(tool_surface.instructions),
            request_tokens = estimate_budgeted_request_tokens(budget.measurement(), tool_surface),
            warning_threshold = budget.warning_threshold_tokens,
            hard_limit = budget.available_input_tokens,
            source = token_source,
            "context budget warning threshold crossed"
        );
    }
    if !budget.over_hard_limit
        || matches!(config.mode, crate::config::AgentCompactionMode::Off)
        || turn_history.caller_rollover_current(view)
    {
        return Ok(PreflightCompaction::default());
    }
    let is_switch = matches!(config.mode, crate::config::AgentCompactionMode::Switch);
    let canonical = turn_history.canonical_mut(view);
    let before = canonical.clone();
    // The request is the turn's own message: summary compaction cuts at the
    // first message that may head the hot window, which in a tool round is the
    // assistant envelope after the request, so the compaction can take it. It
    // goes back after the replacement.
    let request = capture_current_turn_request(canonical.messages());
    // A `switch` rollover that cannot stay lossless must not abort the turn.
    // Failing closed here made every session that contains tool calls answer
    // each user message with the same non-retryable error, because the tool
    // round-trip messages kept provenance unresolvable and history never
    // shrank. The view degrades to a lossy trim and the downgrade is recorded.
    // Measured on the provider view the gate saw, built from and applied to
    // the canonical history.
    match apply_configurable_compaction_over_view(canonical, view, provider, model, config, audit, trigger).await {
        Ok(CompactionApply::Applied(_)) => {
            canonical.restore_request_at_tail(request, trigger);
            Ok(PreflightCompaction {
                source: Some(before),
                degraded_reason: None,
            })
        }
        // A switch that does not reclaim tokens is no handoff: the turn
        // degrades to the trim, as for any other handoff that is unavailable.
        Ok(CompactionApply::Suppressed) => Ok(PreflightCompaction::default()),
        Ok(CompactionApply::NotNeeded | CompactionApply::NoProgress { .. }) => Ok(PreflightCompaction {
            source: None,
            degraded_reason: is_switch.then_some("handoff_unavailable"),
        }),
        Err(error) => {
            if !is_switch {
                return Err(error);
            }
            // Whatever a failed rollover left behind is not a compaction.
            *canonical = before;
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::warn!(
                error_len,
                error_sha256,
                trigger,
                "context switch rollover failed; continuing the turn on a lossy trim"
            );
            Ok(PreflightCompaction {
                source: None,
                degraded_reason: Some("handoff_failed"),
            })
        }
    }
}

/// The projection phase of the hard budget preflight: a provider view still
/// over the hard limit is trimmed for this request only (the canonical history
/// keeps every turn), preserving the replacement the durable phase applied.
/// The request goes back when the trim took it.
pub(super) async fn preflight_trim_projection(
    view: &mut Vec<ChatMessage>,
    turn_history: &mut TurnHistory,
    config: &crate::config::AgentCompactionConfig,
    compaction: &PreflightCompaction,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> Result<ContextBudgetSnapshot> {
    let budget = plan_context_budget(view, config, PRE_TURN_FLUSH_THRESHOLD);
    if !budget.over_hard_limit {
        return Ok(budget);
    }
    let is_switch = matches!(config.mode, crate::config::AgentCompactionMode::Switch);
    let degraded_reason = compaction
        .degraded_reason
        .or_else(|| is_switch.then_some("over_hard_limit_after_handoff"));
    // The one fit (§3.3): the request, the newest handoff and the newest two
    // completed rounds are kept whatever the window's history says.
    let limit = config.max_context_tokens.saturating_sub(config.reserve_tokens);
    let report = turn_history.fit_view(view, limit, FitDemand::Budget, trigger)?;
    let after = plan_context_budget(view, config, PRE_TURN_FLUSH_THRESHOLD);
    tracing::warn!(
        before_used_tokens = budget.used_tokens,
        after_used_tokens = after.used_tokens,
        hard_limit = after.available_input_tokens,
        folded_rounds = report.folded_rounds,
        removed_messages = report.removed_messages,
        mode = ?config.mode,
        "context budget preflight applied the context fit"
    );
    let details = [
        ("folded_rounds", report.folded_rounds),
        ("removed_messages", report.removed_messages),
    ];
    if let Some(degraded) = degraded_reason {
        persist_context_degradation_with_details(
            audit,
            trigger,
            degraded,
            budget.used_tokens,
            after.used_tokens,
            after.available_input_tokens,
            &details,
        )
        .await;
    }
    if report.folded_rounds > 0 {
        persist_context_degradation_with_details(
            audit,
            trigger,
            crate::agent::context_budget::INFLIGHT_ROUNDS_FOLDED_REASON,
            budget.used_tokens,
            after.used_tokens,
            after.available_input_tokens,
            &details,
        )
        .await;
    }
    Ok(after)
}

/// The turn's own request, captured before a context-budget removal can take
/// it, with the number of messages that followed it at capture time.
#[derive(Debug, Clone)]
pub(crate) struct CapturedTurnRequest {
    message: ChatMessage,
    messages_after: usize,
    restore_message: Option<ChatMessage>,
}

impl CapturedTurnRequest {
    pub(crate) fn into_message(self) -> ChatMessage {
        self.message
    }

    /// The request's index in a history of `history_len` messages, the history it was
    /// captured from (or a history that kept it in place).
    pub(crate) fn index_in(&self, history_len: usize) -> Option<usize> {
        history_len.checked_sub(self.messages_after.saturating_add(1))
    }
}

/// A `user` message the loop wrote itself rather than one the user sent.
///
/// The exact reserved markers, the same set [`latest_user_query_index`] skips:
/// this decides whether a `user` message *below* the candidate is bookkeeping
/// the candidate may be read through, and reading a real turn as bookkeeping is
/// the failure mode being guarded against. A real turn cannot open with one,
/// because inbound user text is neutralised on entry.
fn is_loop_written_user_note(message: &ChatMessage) -> bool {
    crate::agent::turn_contracts::is_loop_written_user_note(&message.content)
}

/// Take **this** turn's request before a trim or a compaction can remove it.
///
/// Not "the last `user` message": a prompt-guided provider has no `tool` role,
/// so the loop feeds a tool round back as a `[Tool results]` user message, and
/// compaction appends its refresh aside as another one. Those are the loop's own
/// bookkeeping, and preserving one of them instead of the question is the
/// failure this floor exists to prevent — hence
/// [`latest_user_query_index`], the one predicate for "the user actually asked
/// this".
///
/// **But that predicate has no "not found" branch.** It falls through to the
/// next-older candidate, so when the current turn's own message is one it
/// screens out — an empty inbound message, which no channel guards against, or
/// a user whose text opens with `[Tool` — it silently returns the *previous*
/// turn's question. Restoring that is worse than restoring nothing: the model is
/// handed a question it was not asked this turn and answers it.
///
/// So the candidate is accepted only when it really is the current turn's: every
/// `user` message below it must be one the loop wrote itself
/// ([`is_loop_written_user_note`]). A real `user` turn below the candidate means
/// the predicate skipped the current request, and this floor then protects
/// nothing rather than protecting the wrong thing.
pub(crate) fn capture_current_turn_request(history: &[ChatMessage]) -> Option<CapturedTurnRequest> {
    let index = latest_user_query_index(history)?;
    let a_real_user_turn_follows = history
        .iter()
        .skip(index.saturating_add(1))
        .any(|message| message.role == "user" && !is_loop_written_user_note(message));
    if a_real_user_turn_follows {
        tracing::warn!(
            "the latest identifiable request is not this turn's; leaving the context budget without a request floor"
        );
        return None;
    }
    history.get(index).map(|message| CapturedTurnRequest {
        message: message.clone(),
        restore_message: None,
        messages_after: history.len().saturating_sub(index).saturating_sub(1),
    })
}

/// Keep prepared-message identity while restoring the admitted source text if
/// a request-local enrichment is removed. The positional guard remains the
/// same; enrichment that survives is never mistaken for a second request.
pub(crate) fn capture_current_turn_request_with_source(
    history: &[ChatMessage],
    source: &[ChatMessage],
) -> Option<CapturedTurnRequest> {
    let mut captured = capture_current_turn_request(history)?;
    if let Some(original) = capture_current_turn_request(source)
        && original.messages_after == captured.messages_after
    {
        captured.restore_message = Some(original.message);
    }
    Some(captured)
}

/// Put the turn's own request back at the tail when the removal it was captured
/// for took it. Only a durable compaction removes the request now: the context
/// fit keeps it.
///
/// **Identity is positional, not textual.** A request that survived sits at
/// `len - messages_after - 1`, so an older byte-identical message ("yes",
/// "continue") can neither suppress the restore nor make it duplicate the
/// request. At most one message goes back, and only when the removal took it.
pub(crate) fn restore_current_turn_request(
    history: &mut Vec<ChatMessage>,
    captured: Option<CapturedTurnRequest>,
    trigger: &'static str,
) -> bool {
    let Some(captured) = captured else {
        return false;
    };
    let survived = history
        .len()
        .checked_sub(captured.messages_after.saturating_add(1))
        .and_then(|index| history.get(index))
        .is_some_and(|present| present.role == captured.message.role && present.content == captured.message.content);
    if survived {
        return false;
    }
    tracing::warn!(
        trigger,
        "the context budget removed the current turn's request; restoring it at the tail"
    );
    history.push(captured.restore_message.unwrap_or(captured.message));
    true
}

/// Reject a request that cannot fit even after removing every older turn.
/// This is used by the TUI before handing its prepared history to the engine.
pub(crate) fn ensure_current_request_capacity(
    history: &[ChatMessage],
    config: &crate::config::AgentCompactionConfig,
) -> Result<()> {
    let Some(request) = capture_current_turn_request(history) else {
        return Ok(());
    };
    let mut irreducible: Vec<_> = history
        .iter()
        .filter(|message| message.role == "system")
        .cloned()
        .collect();
    irreducible.push(request.message);
    let budget = plan_context_budget(&irreducible, config, PRE_TURN_FLUSH_THRESHOLD);
    if budget.over_hard_limit {
        return Err(crate::agent::turn_contracts::ContextCapacityError::RequestTooLarge {
            used_tokens: budget.used_tokens,
            available_tokens: budget.available_input_tokens,
        }
        .into());
    }
    Ok(())
}

#[path = "turn_tests/context_preflight.rs"]
mod tests;
