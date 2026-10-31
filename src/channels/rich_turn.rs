//! Glue between a channel turn and [`super::rich::RichChannel`].
//!
//! The turn loop stays in `turn.rs`. This module owns the small pieces that
//! loop should not: which resolver to pass, the task-local turn id the
//! approval bridge reads, and (in later stages) stream and tool projection.

use super::rich::RichChannel;
use super::traits::Channel;
use crate::agent::turn_contracts::ApprovalResolver;
use std::future::Future;
use std::sync::Arc;

tokio::task_local! {
    static ACTIVE_TURN: ActiveTurn;
}

/// Conversation and turn the current task is serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTurn {
    pub conversation_id: String,
    pub turn_id: String,
}

/// The turn scope, when this task was entered through [`scope`].
#[must_use]
pub fn active_turn() -> Option<ActiveTurn> {
    ACTIVE_TURN.try_with(Clone::clone).ok()
}

/// Resolver supplied by a rich channel, if any.
#[must_use]
pub fn resolver_for(channel: &dyn Channel) -> Option<Arc<dyn ApprovalResolver>> {
    channel.as_rich().and_then(RichChannel::approval_resolver)
}

/// Run `fut` with [`active_turn`] set.
pub async fn scope<F>(conversation_id: &str, turn_id: &str, fut: F) -> F::Output
where
    F: Future,
{
    ACTIVE_TURN
        .scope(
            ActiveTurn {
                conversation_id: conversation_id.to_string(),
                turn_id: turn_id.to_string(),
            },
            fut,
        )
        .await
}

/// Scope `fut` only when `channel` implements [`RichChannel`].
pub async fn scope_if_rich<F>(
    channel: Option<&Arc<dyn Channel>>,
    conversation_id: &str,
    turn_id: &str,
    fut: F,
) -> F::Output
where
    F: Future,
{
    if channel.is_some_and(|candidate| candidate.as_rich().is_some()) {
        scope(conversation_id, turn_id, fut).await
    } else {
        fut.await
    }
}

/// Unbinds a turn when dropped, including when the turn task is aborted.
pub struct TurnGuard {
    channel: Arc<dyn Channel>,
    conversation_id: String,
    turn_id: String,
}

impl TurnGuard {
    #[must_use]
    pub fn bind(
        channel: Arc<dyn Channel>,
        conversation_id: &str,
        turn_id: &str,
        token: tokio_util::sync::CancellationToken,
    ) -> Option<Self> {
        let rich = channel.as_rich()?;
        rich.bind_turn(conversation_id, turn_id, token);
        Some(Self {
            channel,
            conversation_id: conversation_id.to_string(),
            turn_id: turn_id.to_string(),
        })
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if let Some(rich) = self.channel.as_rich() {
            rich.unbind_turn(&self.conversation_id, &self.turn_id);
        }
    }
}
