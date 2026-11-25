//! The trim steps of the agent loop: the last resort that drops messages from
//! this request's view. The canonical history keeps every turn.

use super::engine::{AfterTools, Round};
use super::preflight::{preflight_config, preflight_trim_projection};
use super::{Outcome, Reason, Reducer, Removed, Tier};
use crate::agent::context_budget::{estimate_history_tokens, planning_history_tokens, trim_history_token_aware};
use crate::agent::context_preflight::{
    RestoredRequestPosition, capture_current_turn_request, restore_current_turn_request,
};
use anyhow::Result;
use async_trait::async_trait;

/// The projection phase of the hard budget preflight: a provider view still
/// over the hard limit is trimmed for this request only, preserving the
/// replacement the compaction step applied. The step enters on every round: the
/// budget it returns is what the request event reports.
pub(crate) struct RoundTrim;

#[async_trait]
impl<'a> Reducer<Round<'a>> for RoundTrim {
    fn name(&self) -> &'static str {
        "round_trim"
    }

    fn tier(&self) -> Tier {
        Tier::Trim
    }

    fn applies(&self, _state: &Round<'a>) -> bool {
        true
    }

    async fn apply(&self, state: &mut Round<'a>) -> Result<Outcome> {
        let messages_before = state.history.len();
        let reserved_config = preflight_config(state.config, state.tool_surface);
        let budget = preflight_trim_projection(
            state.history,
            state.turn_history,
            &reserved_config,
            &state.compaction,
            state.services.audit,
            "pre_provider_call",
        )
        .await;
        state.budget = Some(budget);
        Ok(Outcome {
            reason: Reason::OverHardLimit,
            removed: Removed {
                items: messages_before.saturating_sub(state.history.len()),
                ..Removed::default()
            },
        })
    }
}

/// The token-aware trim after a round's tool results (primary) against the
/// same reserved window the preflight uses.
///
/// Under the same request floor the preflight has. This trim removes the
/// oldest non-system message until the history fits, and the turn's own
/// request is the oldest thing in it: on a saturated window it goes first,
/// *before* the next iteration's preflight can capture it, after which that
/// preflight has nothing to put back and the turn runs on without the question
/// it was asked. Captured inside the step, so a round that needs no trim still
/// clones nothing.
pub(crate) struct MidTurnTrim;

fn mid_turn_limit(config: &crate::config::AgentCompactionConfig) -> usize {
    config.max_context_tokens.saturating_sub(config.reserve_tokens)
}

#[async_trait]
impl<'a> Reducer<AfterTools<'a>> for MidTurnTrim {
    fn name(&self) -> &'static str {
        "mid_turn_trim"
    }

    fn tier(&self) -> Tier {
        Tier::Trim
    }

    fn applies(&self, state: &AfterTools<'a>) -> bool {
        planning_history_tokens(state.history) > mid_turn_limit(state.reserved_config)
    }

    async fn apply(&self, state: &mut AfterTools<'a>) -> Result<Outcome> {
        let messages_before = state.history.len();
        let mid_turn_tokens = estimate_history_tokens(state.history);
        let mid_turn_limit: usize = mid_turn_limit(state.reserved_config);
        tracing::warn!(mid_turn_tokens, mid_turn_limit, "Mid-turn token trim triggered");
        // A projection: the canonical history keeps the trimmed turns.
        state.turn_history.reshape(state.history, |view| {
            let current_request = capture_current_turn_request(view);
            trim_history_token_aware(view, mid_turn_limit);
            restore_current_turn_request(
                view,
                current_request,
                "mid_turn_trim",
                RestoredRequestPosition::AboveEverythingLeftStanding,
            );
        });
        Ok(Outcome {
            reason: Reason::OverHardLimit,
            removed: Removed {
                items: messages_before.saturating_sub(state.history.len()),
                tokens: Some(mid_turn_tokens.saturating_sub(estimate_history_tokens(state.history))),
                ..Removed::default()
            },
        })
    }
}
