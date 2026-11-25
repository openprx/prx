//! The summary or handoff steps of the agent loop: the reducers that move old
//! turns out of the window when the request is over its limit.

use super::engine::{PreTurn, Round, history_before_change, report_durable_compaction};
use super::preflight::{preflight_config, preflight_durable_compaction};
use super::{Outcome, Reason, Reducer, Removed, Tier};
use crate::agent::compaction::{CompactionApply, apply_configurable_compaction_over_view};
use crate::agent::context_budget::{PRE_TURN_FLUSH_THRESHOLD, plan_context_budget};
use anyhow::Result;
use async_trait::async_trait;

/// The compaction before the loop opens: when the turn's request already sits
/// above the warning threshold, run the configured compaction once, unless the
/// caller's own pre-turn rollover is current.
pub(crate) struct PreTurnCompact;

#[async_trait]
impl<'a> Reducer<PreTurn<'a>> for PreTurnCompact {
    fn name(&self) -> &'static str {
        "pre_turn_compact"
    }

    fn tier(&self) -> Tier {
        Tier::Handoff
    }

    fn applies(&self, state: &PreTurn<'a>) -> bool {
        !state.turn_history.caller_rollover_current(state.history)
            && plan_context_budget(state.history, state.config, PRE_TURN_FLUSH_THRESHOLD).over_warning
    }

    async fn apply(&self, state: &mut PreTurn<'a>) -> Result<Outcome> {
        let budget = plan_context_budget(state.history, state.config, PRE_TURN_FLUSH_THRESHOLD);
        tracing::info!(
            token_estimate = budget.used_tokens,
            threshold = budget.warning_threshold_tokens,
            "Pre-turn memory flush: context near limit, running pre-emptive compaction"
        );
        let messages_before = state.history.len();
        let services = state.services;
        // Gated on the provider view, built from the canonical history.
        let canonical = state.turn_history.canonical_mut(state.history);
        let before = history_before_change(services.runtime_adapter, canonical);
        // Measured on the provider view the gate saw (the enriched
        // request included), built from and applied to the canonical history.
        match apply_configurable_compaction_over_view(
            canonical,
            state.history,
            services.provider,
            services.model,
            state.config,
            services.audit,
            "pre_turn",
        )
        .await
        {
            Ok(CompactionApply::Applied(_)) => {
                report_durable_compaction(
                    services.runtime_adapter,
                    before,
                    state.turn_history.finish_durable_compaction(state.history),
                    state.config,
                )
                .await?;
                (services.rederive)(state.turn_history, state.history, services.hooks, services.channel_name).await;
            }
            Ok(CompactionApply::NotNeeded | CompactionApply::Suppressed) => {}
            Err(error) => {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::warn!(error_len, error_sha256, "Pre-turn compaction failed");
            }
        }
        Ok(Outcome {
            reason: Reason::OverWarning,
            removed: Removed {
                items: messages_before.saturating_sub(state.history.len()),
                ..Removed::default()
            },
        })
    }
}

/// The hard budget preflight's durable phase before a provider call: when the
/// provider view is over the hard limit, the summary or handoff is built from
/// and applied to the canonical history. The step enters on every round, since
/// the warning-threshold log line belongs to it.
pub(crate) struct RoundCompact;

#[async_trait]
impl<'a> Reducer<Round<'a>> for RoundCompact {
    fn name(&self) -> &'static str {
        "round_compact"
    }

    fn tier(&self) -> Tier {
        Tier::Handoff
    }

    fn applies(&self, _state: &Round<'a>) -> bool {
        true
    }

    async fn apply(&self, state: &mut Round<'a>) -> Result<Outcome> {
        let messages_before = state.history.len();
        let services = state.services;
        // A summary or handoff applied here (typically after tool output
        // grew the history mid-turn) is reported to the session authority
        // like every durable in-turn compaction; a trim after it is this
        // request's projection.
        // The summary or handoff is built from the canonical history, so a
        // trim that ran earlier in this turn cannot cost it any turn.
        let reserved_config = preflight_config(state.config, state.tool_surface);
        let mut compaction = preflight_durable_compaction(
            state.history,
            state.turn_history,
            services.provider,
            services.model,
            &reserved_config,
            state.tool_surface,
            services.audit,
            "pre_provider_call",
        )
        .await?;
        if let Some(before) = compaction.source.take() {
            report_durable_compaction(
                services.runtime_adapter,
                Some(before),
                state.turn_history.finish_durable_compaction(state.history),
                state.config,
            )
            .await?;
            (services.rederive)(state.turn_history, state.history, services.hooks, services.channel_name).await;
        }
        state.compaction = compaction;
        Ok(Outcome {
            reason: Reason::OverHardLimit,
            removed: Removed {
                items: messages_before.saturating_sub(state.history.len()),
                ..Removed::default()
            },
        })
    }
}
