use crate::agent::compaction::{
    apply_configurable_compaction_with_replacement_len, persist_context_switch_degradation,
};
use crate::agent::context_budget::{
    ContextBudgetSnapshot, PRE_TURN_FLUSH_THRESHOLD, budgeted_request_tool_tokens, budgeted_tokens,
    estimate_budgeted_request_tokens, plan_context_budget, reserve_request_tool_surface,
    trim_history_to_context_budget, trim_history_to_context_budget_preserving_compaction_replacement_with_floor,
};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::history::{history_non_system_start, latest_user_query_index};
use crate::providers::{ChatMessage, Provider};
use anyhow::Result;

pub(super) async fn preflight_context_budget_before_provider_call(
    history: &mut Vec<ChatMessage>,
    provider: &dyn Provider,
    model: &str,
    config: &crate::config::AgentCompactionConfig,
    tool_surface: crate::agent::turn_context::RequestToolMeasurement,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &str,
) -> Result<ContextBudgetSnapshot> {
    // The published tool schemas ride in the same context window as the
    // messages, and the prompt sections compiled from them are appended to the
    // system message after this runs, so the window left for history is the
    // configured one minus both. Reserving them keeps a single budget policy:
    // the warning threshold, summary compaction and the token-aware trim all
    // move together, and a request that publishes no tool at all is budgeted
    // exactly as before.
    let reserved_config;
    let config = if budgeted_request_tool_tokens(tool_surface) == 0 {
        config
    } else {
        reserved_config = reserve_request_tool_surface(config, tool_surface);
        &reserved_config
    };
    let mut budget = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
    if budget.over_warning {
        tracing::info!(
            used_tokens = budget.used_tokens,
            measured_tokens = budget.measured_tokens,
            native_tool_tokens = budgeted_tokens(tool_surface.native_payload.measurement),
            instruction_tokens = budgeted_tokens(tool_surface.instructions),
            request_tokens = estimate_budgeted_request_tokens(budget.measurement(), tool_surface),
            warning_threshold = budget.warning_threshold_tokens,
            hard_limit = budget.available_input_tokens,
            source = ?budget.measurement_source,
            "context budget warning threshold crossed"
        );
    }
    if !budget.over_hard_limit {
        return Ok(budget);
    }

    // From here something will be removed from history, and with a tool
    // surface larger than the whole window every trim below runs down to its
    // floor. The current turn's user message is the request itself: answering
    // a question that is no longer in the transcript is worse than serving an
    // over-budget request, which the provider-side overflow retry already
    // covers. Cloned once, on a path that is already about to summarize or
    // tokenize the whole history, and only to put it back if this preflight
    // took it. Which message is the request, and why it is not simply the last
    // `user` one, is `capture_current_turn_request`.
    let current_request = capture_current_turn_request(history);

    let is_switch = matches!(config.mode, crate::config::AgentCompactionMode::Switch);
    let mut replacement_len = None;
    // A `switch` rollover that cannot stay lossless must not abort the turn.
    // Failing closed here made every session that contains tool calls answer
    // each user message with the same non-retryable error, because the tool
    // round-trip messages kept provenance unresolvable and history never
    // shrank. Degrade to a lossy trim and record the downgrade instead.
    let mut degraded_reason: Option<&'static str> = None;
    if !matches!(config.mode, crate::config::AgentCompactionMode::Off) {
        match apply_configurable_compaction_with_replacement_len(history, provider, model, config, audit, trigger).await
        {
            Ok(applied_replacement_len) => {
                replacement_len = applied_replacement_len;
                if is_switch && applied_replacement_len.is_none() {
                    degraded_reason = Some("handoff_unavailable");
                }
            }
            Err(error) => {
                if !is_switch {
                    return Err(error);
                }
                tracing::warn!(
                    error = %error,
                    trigger,
                    "context switch rollover failed; continuing the turn on a lossy trim"
                );
                degraded_reason = Some("handoff_failed");
            }
        }
    }

    budget = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
    let over_hard_limit_after_compaction = budget.over_hard_limit;
    let trimmed = if over_hard_limit_after_compaction {
        if is_switch && degraded_reason.is_none() {
            degraded_reason = Some("over_hard_limit_after_handoff");
        }
        if let Some(replacement_len) = replacement_len {
            trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
                history,
                config,
                replacement_len,
            )
        } else {
            trim_history_to_context_budget(history, config)
        }
    } else {
        false
    };
    // The restore is checked on both paths. Summary compaction cuts at the
    // first message that may head the hot window, which in a tool round is the
    // assistant envelope *after* the request, so compaction alone can take the
    // request and still leave history under the limit — a case no trim follows.
    //
    // Where it goes back is decided by what *this* call did, not by inspecting
    // the history shape afterwards: only a compaction that replaced a front
    // range put a summary above the request, and only then must the request
    // land after it. `replacement_len` is that fact, already computed above.
    let restore_position = if replacement_len.is_some() {
        RestoredRequestPosition::Tail
    } else {
        RestoredRequestPosition::AboveEverythingLeftStanding
    };
    let restored = restore_current_turn_request(history, current_request, trigger, restore_position);
    if over_hard_limit_after_compaction {
        let after = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
        tracing::warn!(
            before_used_tokens = budget.used_tokens,
            after_used_tokens = after.used_tokens,
            hard_limit = after.available_input_tokens,
            trimmed,
            mode = ?config.mode,
            "context budget preflight applied token-aware trim"
        );
        if let Some(reason) = degraded_reason {
            tracing::warn!(
                trigger,
                reason,
                "context switch degraded to a lossy token-aware trim so the turn can continue"
            );
            persist_context_switch_degradation(
                audit,
                trigger,
                reason,
                budget.used_tokens,
                after.used_tokens,
                after.available_input_tokens,
            )
            .await;
        }
        budget = after;
    } else if restored {
        budget = plan_context_budget(history, config, PRE_TURN_FLUSH_THRESHOLD);
    }
    Ok(budget)
}

/// The turn's own request, captured before a context-budget removal can take
/// it, with the number of messages that followed it at capture time.
pub(super) struct CapturedTurnRequest {
    message: ChatMessage,
    messages_after: usize,
}

/// Where a restored request goes back into history.
///
/// **One rule, and it is the same rule at both call sites: the request returns
/// above everything the removal left standing — unless that removal was a
/// summary compaction which *replaced* a front range, and only then it returns
/// at the tail.** The discriminator is what the removal did, so each call site
/// passes the variant its own removal earned rather than guessing from the
/// shape of the history afterwards.
#[derive(Debug, Clone, Copy)]
pub(super) enum RestoredRequestPosition {
    /// **Summary compaction replaced a front range: the request returns at the
    /// tail.**
    ///
    /// This is the one removal that *writes* as well as deletes. The summary it
    /// puts at the front of the replacement covers the conversation the request
    /// came after, and the refresh aside follows it, so the slot the request
    /// held now points inside or before a summary it belongs *after*. The tail
    /// is the only position that keeps that order. It cannot split a round trip
    /// either — appending after everything cannot fall between an assistant
    /// `tool_calls` envelope and the `tool` results answering it.
    ///
    /// **The shape this keeps, by design (D-011).** Here ordering wins over
    /// role alternation: the request served after a replacing compaction
    /// still opens on the summary's `assistant` turn, and prompt-guided the
    /// refresh aside, a `[Tool results]` block (from the second iteration
    /// on) and the request follow as adjacent `user` turns. That is the one
    /// exception to the shape [`Self::AboveEverythingLeftStanding`] repairs.
    /// If it is ever to be closed, the position is immediately after the
    /// refresh aside, not the tail.
    ///
    /// Used by the preflight, and only on the compaction-replaced path.
    Tail,
    /// **Every other removal deletes only: the request returns above everything
    /// left standing**, at the front of the non-system region.
    ///
    /// Used by the mid-turn trim, and by the preflight whenever no compaction
    /// replaced a range. The mid-turn trim always, and the preflight from the
    /// second iteration on, run on a history whose tail is the tool round just
    /// executed — an assistant `tool_calls` envelope plus `tool` results in
    /// native mode, an assistant turn plus a `[Tool results]` user block in
    /// prompt-guided mode — and there the tail is the wrong end (on the
    /// preflight's first iteration the tail is the request itself, and the two
    /// positions coincide):
    ///
    /// - It is not where the request belongs. A delete-only trim walks forward
    ///   from a fixed index at or ahead of the front, so every message still
    ///   standing came after the request; the front of the non-system region is
    ///   the position the request actually held relative to all of them.
    /// - Appending would break the shape. Prompt-guided, the tail is already a
    ///   `user` message, so a second one produces the adjacent `user, user` pair
    ///   no provider in this crate coalesces and strict chat templates reject
    ///   (see [`append_corrective_aside`]); native, the dialects that carry a
    ///   tool result as a user turn (`providers/anthropic.rs`,
    ///   `providers/bedrock.rs`, `providers/gemini.rs`) produce the same pair.
    ///   Appending also leaves the round's `assistant` turn as the first
    ///   non-system message, which the dialects that validate the opening role
    ///   reject outright.
    /// - It would cost the round. The request is what the next trim has to free
    ///   first if anything must go, and it is the one message that trim can put
    ///   back. Left at the tail it is the *last* candidate, so that trim eats
    ///   the assistant envelope instead and drops the tool results with it — the
    ///   turn keeps its question and loses the work it just paid for.
    ///
    /// Inserting at the front cannot split a round trip either: the trim's
    /// closing orphan sweep guarantees the first non-system message is not a
    /// `tool` result, so there is no envelope above the insertion point to
    /// separate from its answers.
    AboveEverythingLeftStanding,
}

impl RestoredRequestPosition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Tail => "tail",
            Self::AboveEverythingLeftStanding => "above_everything_left_standing",
        }
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
pub(super) fn capture_current_turn_request(history: &[ChatMessage]) -> Option<CapturedTurnRequest> {
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
        messages_after: history.len().saturating_sub(index).saturating_sub(1),
    })
}

/// Put the turn's own request back when the removal it was captured for took
/// it.
///
/// **Identity is positional, not textual.** Every removal either side of this
/// call walks forward from a fixed index at or ahead of the front, so nothing
/// behind a surviving request is ever removed: compaction replaces a range that
/// starts at the first non-system message, `trim_history_token_aware` removes
/// repeatedly at the first non-system index, and
/// `trim_history_to_context_budget_preserving_compaction_replacement` — the one
/// removal that is *not* front-first — removes repeatedly at `start +
/// replacement_len`, deliberately behind the summary it preserves, but still
/// only ever forwards from there. A request that survived therefore still has
/// exactly the messages that followed it behind it and can only be at
/// `len - messages_after - 1`. Comparing that one slot means an older
/// byte-identical message ("yes", "continue") can neither suppress the restore
/// nor make it duplicate the request, which a content-equality search over the
/// whole history can.
///
/// **Termination.** At most one message goes back, it was in history when the
/// capture ran, and it goes back only when the removal took it — so a
/// capture/remove/restore round is never net-lengthening, and the request the
/// caller deleted for good is not in history to be captured again.
///
/// The insertion rule is [`RestoredRequestPosition`].
pub(super) fn restore_current_turn_request(
    history: &mut Vec<ChatMessage>,
    captured: Option<CapturedTurnRequest>,
    trigger: &str,
    position: RestoredRequestPosition,
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
        restored_at = position.as_str(),
        "the context budget removed the current turn's request; restoring it over the budget"
    );
    match position {
        RestoredRequestPosition::Tail => history.push(captured.message),
        RestoredRequestPosition::AboveEverythingLeftStanding => {
            // `history_non_system_start` inspects `history[0]` only, so "the
            // front of the non-system region" is exact for the leading system
            // prompt and no more. With OS-paging recall active and the default
            // `RetrievalInjectionRole::System`, `inject_recalled_context` puts a
            // *second* system message at index 1 and the request is inserted
            // above it. That is safe only because
            // `coalesce_system_messages_at_front` hoists and merges it on the
            // prepared copy before the request is serialized — without that
            // pass this insert would hand a strict template a system message
            // that is not the first transcript item.
            let index = history_non_system_start(history).min(history.len());
            history.insert(index, captured.message);
        }
    }
    true
}

#[path = "turn_tests/context_preflight.rs"]
mod tests;
