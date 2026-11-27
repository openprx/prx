//! The durable and the projection phase of the hard budget preflight before a
//! provider call: the summary or handoff first, the lossy trim of this
//! request's view after it.

use crate::agent::compaction::{
    CompactionApply, apply_configurable_compaction_over_view, persist_context_switch_degradation,
};
use crate::agent::context_budget::{
    ContextBudgetSnapshot, PRE_TURN_FLUSH_THRESHOLD, budgeted_request_tool_tokens, budgeted_tokens,
    estimate_budgeted_request_tokens, plan_context_budget, reserve_request_tool_surface, token_source_label,
    trim_history_to_context_budget, trim_history_to_context_budget_preserving_compaction_replacement_with_floor,
};
use crate::agent::context_preflight::{
    RestoredRequestPosition, capture_current_turn_request, restore_current_turn_request,
};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_history::TurnHistory;
use crate::providers::{ChatMessage, Provider};
use anyhow::Result;

/// The compaction config one provider request is budgeted with. The
/// published tool schemas ride in the same context window as the messages,
/// and the prompt sections compiled from them are appended to the system
/// message after the preflight, so the window left for history is the
/// configured one minus both. Reserving them keeps a single budget policy: the
/// warning threshold, summary compaction and the token-aware trim all move
/// together, and a request that publishes no tool at all is budgeted exactly
/// as configured.
pub(crate) fn preflight_config(
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
pub(crate) struct PreflightCompaction {
    /// The canonical history before the summary or handoff this phase applied
    /// to it; taken by the caller that reports the compaction.
    pub(crate) source: Option<crate::agent::turn_history::CanonicalHistory>,
    /// The applied replacement's length in the provider view.
    pub(crate) replacement_len: Option<usize>,
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
pub(crate) async fn preflight_durable_compaction(
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
        Ok(CompactionApply::Applied(replacement_len)) => {
            canonical.restore_request_at_tail(request, trigger);
            Ok(PreflightCompaction {
                source: Some(before),
                replacement_len: Some(replacement_len),
                degraded_reason: None,
            })
        }
        // A suppressed automatic switch leaves the turn to its trim below.
        Ok(CompactionApply::NotNeeded(reason)) => Ok(PreflightCompaction {
            source: None,
            replacement_len: None,
            degraded_reason: is_switch.then_some(reason.degraded_reason()),
        }),
        Ok(CompactionApply::Suppressed) => Ok(PreflightCompaction {
            source: None,
            replacement_len: None,
            degraded_reason: None,
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
                replacement_len: None,
                degraded_reason: Some("handoff_failed"),
            })
        }
    }
}

/// The projection phase of the hard budget preflight: a provider view still
/// over the hard limit is trimmed for this request only (the canonical history
/// keeps every turn), preserving the replacement the durable phase applied.
/// The request goes back when the trim took it.
pub(crate) async fn preflight_trim_projection(
    view: &mut Vec<ChatMessage>,
    turn_history: &mut TurnHistory,
    config: &crate::config::AgentCompactionConfig,
    compaction: &PreflightCompaction,
    audit: Option<&DocumentIngestRuntime>,
    trigger: &'static str,
) -> ContextBudgetSnapshot {
    let budget = plan_context_budget(view, config, PRE_TURN_FLUSH_THRESHOLD);
    if !budget.over_hard_limit {
        return budget;
    }
    let is_switch = matches!(config.mode, crate::config::AgentCompactionMode::Switch);
    let replacement_len = compaction.replacement_len;
    let degraded_reason = compaction
        .degraded_reason
        .or_else(|| is_switch.then_some("over_hard_limit_after_handoff"));
    // With a tool surface larger than the whole window every trim below runs
    // down to its floor. The current turn's user message is the request
    // itself: answering a question that is no longer in the transcript is
    // worse than serving an over-budget request, which the provider-side
    // overflow retry already covers. Which message is the request, and why it
    // is not simply the last `user` one, is `capture_current_turn_request`.
    // Only a compaction that replaced a front range put a summary above the
    // request, and only then must the request land after it.
    let restore_position = if replacement_len.is_some() {
        RestoredRequestPosition::Tail
    } else {
        RestoredRequestPosition::AboveEverythingLeftStanding
    };
    let trimmed: bool = turn_history.reshape(view, |view| {
        let current_request = capture_current_turn_request(view);
        let applied = if let Some(replacement_len) = replacement_len {
            trim_history_to_context_budget_preserving_compaction_replacement_with_floor(view, config, replacement_len)
        } else {
            trim_history_to_context_budget(view, config)
        };
        restore_current_turn_request(view, current_request, trigger, restore_position);
        applied
    });
    let after = plan_context_budget(view, config, PRE_TURN_FLUSH_THRESHOLD);
    tracing::warn!(
        before_used_tokens = budget.used_tokens,
        after_used_tokens = after.used_tokens,
        hard_limit = after.available_input_tokens,
        trimmed,
        mode = ?config.mode,
        "context budget preflight applied token-aware trim"
    );
    if let Some(degraded) = degraded_reason {
        let reason: &'static str = degraded;
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
    after
}
