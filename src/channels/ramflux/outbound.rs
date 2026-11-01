//! Outbound bridge methods. Every frame carries `turn_id`.

use super::Inner;
use super::protocol::{
    self, AttachmentFetchParams, AttachmentFetchResult, ReplyAttachmentParams, ReplySurfaceParams,
    ReplyTaskProposeParams, ReplyTaskUpdateParams, ReplyTextParams, ReplyTypingParams, TaskStepWire, ToolEventParams,
    TurnStatusParams,
};
use super::rpc::RpcHub;
use crate::channels::rich::{
    AttachmentReply, RichReply, SurfaceReply, TaskProposeReply, TaskStep, TaskUpdateReply, TextReply, ToolEvent,
    TurnStatus, TypingReply,
};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Send one rich reply.
///
/// # Errors
///
/// Returns an error when the bridge write fails or the reply kind is not
/// available yet. A rejected kind also emits `turn.status`.
pub async fn send_reply(inner: &Inner, reply: &RichReply) -> anyhow::Result<()> {
    match reply {
        RichReply::Text(text) => {
            let mut text = text.clone();
            if text.reply_target.is_none() {
                text.reply_target = inner.reply_target_for(&text.conversation_id);
            }
            send_text(&inner.rpc, &text).await
        }
        RichReply::Typing(typing) => send_typing(&inner.rpc, typing).await,
        RichReply::Status(status) => send_status(&inner.rpc, status).await,
        RichReply::Surface(surface) => send_surface(inner, surface).await,
        RichReply::TaskUpdate(update) => send_task_update(&inner.rpc, update).await,
        RichReply::TaskPropose(propose) => send_task_propose(&inner.rpc, propose).await,
        RichReply::Attachment(attachment) => send_attachment(&inner.rpc, attachment).await,
    }
}

pub async fn send_surface(inner: &super::Inner, surface: &SurfaceReply) -> anyhow::Result<()> {
    if let Err(error) =
        super::surface_schema::validate(surface, &inner.config.surface_limits, &inner.config.allowed_block_types)
    {
        let status = crate::channels::rich::TurnStatus {
            conversation_id: surface.conversation_id.clone(),
            turn_id: surface.turn_id.clone(),
            state: crate::channels::rich::TurnState::Failed,
            error: Some(crate::channels::rich::TurnError {
                code: "surface_rejected".to_string(),
                message: error.to_string(),
            }),
        };
        send_status(&inner.rpc, &status).await?;
        anyhow::bail!("surface rejected: {error}");
    }
    send_surface_frame(&inner.rpc, surface).await
}

async fn send_surface_frame(rpc: &RpcHub, surface: &SurfaceReply) -> anyhow::Result<()> {
    let params = ReplySurfaceParams {
        conversation_id: &surface.conversation_id,
        turn_id: &surface.turn_id,
        op: surface.op,
        surface_id: &surface.surface_id,
        version: surface.version,
        base_hash: surface.base_hash.as_deref(),
        blocks: surface.blocks.as_deref(),
        actions: surface.actions.as_deref(),
        ops: surface.ops.as_ref(),
        block_id: surface.block_id.as_deref(),
        text_delta: surface.text_delta.as_deref(),
        is_final: surface.is_final,
        fallback_text: surface.fallback_text.as_deref(),
    };
    request(rpc, protocol::METHOD_REPLY_SURFACE, &params).await
}

pub async fn send_task_update(rpc: &RpcHub, update: &TaskUpdateReply) -> anyhow::Result<()> {
    let steps = wire_steps(&update.steps);
    let params = ReplyTaskUpdateParams {
        conversation_id: &update.conversation_id,
        turn_id: &update.turn_id,
        task_id: &update.task_id,
        version: update.version,
        state: &update.state,
        steps: &steps,
        result_ref: update.result_ref.as_deref(),
        result_summary: update.result_summary.as_deref(),
    };
    request(rpc, protocol::METHOD_REPLY_TASK_UPDATE, &params).await
}

pub async fn send_task_propose(rpc: &RpcHub, propose: &TaskProposeReply) -> anyhow::Result<()> {
    let steps = wire_steps(&propose.steps);
    let params = ReplyTaskProposeParams {
        conversation_id: &propose.conversation_id,
        turn_id: &propose.turn_id,
        task_id: &propose.task_id,
        title: &propose.title,
        steps: &steps,
    };
    request(rpc, protocol::METHOD_REPLY_TASK_PROPOSE, &params).await
}

pub async fn send_attachment(rpc: &RpcHub, attachment: &AttachmentReply) -> anyhow::Result<()> {
    let params = ReplyAttachmentParams {
        conversation_id: &attachment.conversation_id,
        turn_id: &attachment.turn_id,
        bytes_b64: attachment.bytes_b64.as_deref(),
        stream_id: attachment.stream_id.as_deref(),
        mime: &attachment.mime,
        name: attachment.name.as_deref(),
    };
    request(rpc, protocol::METHOD_REPLY_ATTACHMENT, &params).await
}

pub async fn send_tool_event(
    rpc: &RpcHub,
    conversation_id: &str,
    turn_id: &str,
    event: &ToolEvent,
) -> anyhow::Result<()> {
    let params = ToolEventParams {
        conversation_id,
        turn_id,
        kind: event.kind,
        tool_call_id: &event.tool_call_id,
        tool_name: &event.tool_name,
        iteration: event.iteration,
        duration_ms: event.duration_ms,
        summary: event.summary.as_deref(),
    };
    request(rpc, protocol::METHOD_TOOL_EVENT, &params).await
}

pub async fn fetch_attachment(
    rpc: &RpcHub,
    conversation_id: &str,
    turn_id: &str,
    object_id: &str,
    max_bytes: u64,
) -> anyhow::Result<AttachmentFetchResult> {
    let params = AttachmentFetchParams {
        conversation_id,
        turn_id,
        object_id,
        max_bytes,
    };
    let value = serde_json::to_value(&params)?;
    let result = rpc
        .request(protocol::METHOD_ATTACHMENT_FETCH, &value, SEND_TIMEOUT)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    Ok(serde_json::from_value(result)?)
}

fn wire_steps(steps: &[TaskStep]) -> Vec<TaskStepWire<'_>> {
    steps
        .iter()
        .map(|step| TaskStepWire {
            id: &step.id,
            title: &step.title,
            state: &step.state,
            needs_me: step.needs_me,
        })
        .collect()
}

pub async fn send_text(rpc: &RpcHub, text: &TextReply) -> anyhow::Result<()> {
    let params = ReplyTextParams {
        conversation_id: &text.conversation_id,
        turn_id: &text.turn_id,
        text: &text.text,
        is_final: text.is_final,
        reply_target: text.reply_target.as_deref(),
    };
    request(rpc, protocol::METHOD_REPLY_TEXT, &params).await
}

pub async fn send_typing(rpc: &RpcHub, typing: &TypingReply) -> anyhow::Result<()> {
    let params = ReplyTypingParams {
        conversation_id: &typing.conversation_id,
        turn_id: &typing.turn_id,
        on: typing.on,
    };
    request(rpc, protocol::METHOD_REPLY_TYPING, &params).await
}

pub async fn send_status(rpc: &RpcHub, status: &TurnStatus) -> anyhow::Result<()> {
    let params = TurnStatusParams {
        conversation_id: &status.conversation_id,
        turn_id: &status.turn_id,
        state: status.state,
        error: status.error.as_ref(),
    };
    request(rpc, protocol::METHOD_TURN_STATUS, &params).await
}

async fn request(rpc: &RpcHub, method: &str, params: &impl Serialize) -> anyhow::Result<()> {
    let value = serde_json::to_value(params)?;
    rpc.request(method, &value, SEND_TIMEOUT)
        .await
        .map(|_: Value| ())
        .map_err(|error| anyhow::anyhow!(error))
}
