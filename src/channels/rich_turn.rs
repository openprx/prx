//! Glue between a channel turn and [`super::rich::RichChannel`].
//!
//! The turn loop stays in `turn.rs`. This module owns the resolver lookup, the
//! task-local turn id, versioned text streaming, and tool-event projection.

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

/// Send one streaming text delta as `reply.surface` `stream_append`.
pub async fn append_stream_delta(
    channel: &dyn Channel,
    conversation_id: &str,
    turn_id: &str,
    version: u64,
    delta: &str,
) {
    let Some(rich) = channel.as_rich() else {
        return;
    };
    let reply = super::rich::RichReply::Surface(super::rich::SurfaceReply {
        conversation_id: conversation_id.to_string(),
        turn_id: turn_id.to_string(),
        op: super::rich::SurfaceOp::StreamAppend,
        surface_id: format!("stream-{turn_id}"),
        version,
        base_hash: None,
        blocks: None,
        actions: None,
        ops: None,
        block_id: None,
        text_delta: Some(delta.to_string()),
        is_final: Some(false),
        fallback_text: Some(delta.to_string()),
    });
    if let Err(error) = rich.send_rich(&reply).await {
        tracing::debug!(error = %error, "ramflux stream delta was not sent");
    }
}

/// Close the streamed surface and send the final text.
pub async fn finish_stream(channel: &dyn Channel, conversation_id: &str, turn_id: &str, version: u64, text: &str) {
    let Some(rich) = channel.as_rich() else {
        return;
    };
    let close = super::rich::RichReply::Surface(super::rich::SurfaceReply {
        conversation_id: conversation_id.to_string(),
        turn_id: turn_id.to_string(),
        op: super::rich::SurfaceOp::Close,
        surface_id: format!("stream-{turn_id}"),
        version,
        base_hash: None,
        blocks: None,
        actions: None,
        ops: None,
        block_id: None,
        text_delta: None,
        is_final: Some(true),
        fallback_text: Some(text.to_string()),
    });
    if let Err(error) = rich.send_rich(&close).await {
        tracing::debug!(error = %error, "ramflux stream close was not sent");
    }
    let final_text = super::rich::RichReply::Text(super::rich::TextReply {
        conversation_id: conversation_id.to_string(),
        turn_id: turn_id.to_string(),
        text: text.to_string(),
        is_final: true,
        reply_target: None,
    });
    if let Err(error) = rich.send_rich(&final_text).await {
        tracing::debug!(error = %error, "ramflux final text was not sent");
    }
}

/// Read provider deltas and project them as versioned surface appends.
pub async fn consume_stream(
    channel: Arc<dyn Channel>,
    conversation_id: String,
    turn_id: String,
    mut deltas: tokio::sync::mpsc::Receiver<String>,
) -> u64 {
    let mut version = 0u64;
    while let Some(delta) = deltas.recv().await {
        version = version.saturating_add(1);
        append_stream_delta(channel.as_ref(), &conversation_id, &turn_id, version, &delta).await;
    }
    version
}

/// Map a channel tool notification onto [`super::rich::ToolEvent`] and send it.
pub async fn forward_tool_notification(
    channel: &dyn Channel,
    conversation_id: &str,
    turn_id: &str,
    notification: &crate::agent::turn_contracts::ToolCallNotification,
    iteration: u64,
) {
    let Some(rich) = channel.as_rich() else {
        return;
    };
    let Some(event) = tool_event_from_notification(notification, iteration) else {
        return;
    };
    scope(conversation_id, turn_id, rich.on_tool_event(&event)).await;
}

fn tool_event_from_notification(
    notification: &crate::agent::turn_contracts::ToolCallNotification,
    iteration: u64,
) -> Option<super::rich::ToolEvent> {
    use super::rich::{ToolEvent, ToolEventKind};
    use crate::agent::turn_contracts::ToolCallNotification;
    match notification {
        ToolCallNotification::Started { id, name, args_summary } => Some(ToolEvent {
            kind: ToolEventKind::Started,
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            iteration,
            duration_ms: None,
            summary: Some(format!("args_len={}", args_summary.len())),
        }),
        ToolCallNotification::Finished {
            id,
            name,
            success,
            duration_ms,
        } => Some(ToolEvent {
            kind: if *success {
                ToolEventKind::Finished
            } else {
                ToolEventKind::Failed
            },
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            iteration,
            duration_ms: Some(*duration_ms),
            summary: None,
        }),
        ToolCallNotification::Progress { iteration } => {
            let iteration = u64::try_from(*iteration).unwrap_or(u64::MAX);
            Some(ToolEvent {
                kind: ToolEventKind::Progress,
                tool_call_id: format!("progress:{iteration}"),
                tool_name: String::new(),
                iteration,
                duration_ms: None,
                summary: None,
            })
        }
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        crate::agent::channel_approval::clear(&self.turn_id);
        if let Some(rich) = self.channel.as_rich() {
            rich.unbind_turn(&self.conversation_id, &self.turn_id);
        }
    }
}

/// Close a streamed surface at `last_version + 1` and emit terminal status.
pub async fn complete_turn(
    channel: &dyn Channel,
    conversation_id: &str,
    turn_id: &str,
    last_version: u64,
    text: &str,
    state: super::rich::TurnState,
    code: &str,
) {
    let Some(rich) = channel.as_rich() else {
        return;
    };
    if last_version > 0 {
        finish_stream(channel, conversation_id, turn_id, last_version.saturating_add(1), text).await;
    }
    let _ = rich
        .send_rich(&super::rich::RichReply::Typing(super::rich::TypingReply {
            conversation_id: conversation_id.to_string(),
            turn_id: turn_id.to_string(),
            on: false,
        }))
        .await;
    let status = super::rich::TurnStatus {
        conversation_id: conversation_id.to_string(),
        turn_id: turn_id.to_string(),
        state,
        error: if matches!(state, super::rich::TurnState::Failed) {
            Some(super::rich::TurnError {
                code: code.to_string(),
                message: code.to_string(),
            })
        } else {
            None
        },
    };
    if let Err(error) = rich.send_rich(&super::rich::RichReply::Status(status)).await {
        tracing::debug!(error = %error, "ramflux turn status was not sent");
    }
}

/// Emits `turn.status` failed/cancelled when a rich turn returns without one.
pub struct StatusFallback {
    channel: Arc<dyn Channel>,
    conversation_id: String,
    turn_id: String,
    token: tokio_util::sync::CancellationToken,
    armed: bool,
}

impl StatusFallback {
    #[must_use]
    pub fn arm(
        channel: Arc<dyn Channel>,
        conversation_id: String,
        turn_id: String,
        token: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            channel,
            conversation_id,
            turn_id,
            token,
            armed: true,
        }
    }

    pub const fn defuse(&mut self) {
        self.armed = false;
    }
}

impl Drop for StatusFallback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let channel = Arc::clone(&self.channel);
        let conversation_id = std::mem::take(&mut self.conversation_id);
        let turn_id = std::mem::take(&mut self.turn_id);
        let code = if self.token.is_cancelled() {
            "cancelled"
        } else {
            "turn_aborted"
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                complete_turn(
                    channel.as_ref(),
                    &conversation_id,
                    &turn_id,
                    0,
                    "",
                    super::rich::TurnState::Failed,
                    code,
                )
                .await;
            });
        }
    }
}
