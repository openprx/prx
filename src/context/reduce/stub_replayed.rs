//! Clearing replayed tool results: the oldest tool results a session replays
//! into the view give way to a one-line stub while the request is over the
//! warning threshold. The log is untouched.

use super::{Outcome, Reason, Reducer, Removed, Tier};
use crate::context::transcript::{TurnReplay, stub_replayed_results};
use crate::providers::ChatMessage;
use anyhow::Result;
use async_trait::async_trait;

/// What the step works on: the view, the rounds the view's turns replayed,
/// and the request that follows the view.
pub(crate) struct ReplayedView<'a> {
    pub(crate) view: &'a mut [ChatMessage],
    /// The replayed rounds of the view's turns, oldest first.
    pub(crate) replays: Vec<&'a TurnReplay>,
    pub(crate) request: &'a ChatMessage,
    pub(crate) config: &'a crate::config::AgentCompactionConfig,
}

/// Replace the oldest replayed tool results by a stub until the view and the
/// request fit under the warning threshold.
pub(crate) struct StubReplayed;

#[async_trait]
impl<'a> Reducer<ReplayedView<'a>> for StubReplayed {
    fn name(&self) -> &'static str {
        "stub_replayed"
    }

    fn tier(&self) -> Tier {
        Tier::Clear
    }

    fn applies(&self, state: &ReplayedView<'a>) -> bool {
        !state.replays.is_empty()
    }

    async fn apply(&self, state: &mut ReplayedView<'a>) -> Result<Outcome> {
        let stubbed = stub_replayed_results(state.view, &state.replays, state.request, state.config);
        Ok(Outcome {
            reason: Reason::OverWarning,
            removed: Removed {
                items: stubbed,
                ..Removed::default()
            },
        })
    }
}
