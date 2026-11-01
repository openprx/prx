//! Inbound bridge methods.
//!
//! `bridge.hello` and `bridge.ping` maintain the session. Messages, surface
//! actions, and task events are handed to the rich listener. Control events
//! cancel or pause turns. Approval decisions complete waiters and are never
//! inferred from chat text. Message bodies are not logged.

use super::Inner;
use super::protocol::{
    self, ApprovalDecisionParams, BridgeHello, BridgeHelloAck, ControlParams, Envelope, InboundMessageParams,
    MessageDeletedParams, RpcErrorBody, SurfaceActionParams, TaskEventParams, TurnCancelParams,
};
use super::socket::unix_ms;
use crate::channels::rich::{
    ControlInbound, ControlKind, MessageDeletedInbound, RichInbound, RichMessage, SurfaceActionInbound,
    TaskEventInbound,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

#[derive(Debug, thiserror::Error)]
pub enum InboundError {
    #[error("bridge hello is required")]
    HelloRequired,
    #[error("account is paused")]
    Paused,
    #[error("protocol version {0} is not supported")]
    Version(String),
    #[error("unknown bridge method {0}")]
    UnknownMethod(String),
    #[error(transparent)]
    BadParams(#[from] serde_json::Error),
    #[error("{0}")]
    Failed(String),
    #[error("not authorized")]
    NotAuthorized,
    #[error("queue_full depth={0}")]
    QueueFull(usize),
}

/// Handle one frame from the agent-host.
///
/// # Errors
///
/// Returns [`InboundError`] when the frame is not valid JSON-RPC or the
/// method is rejected. A response has already been written for requests.
pub async fn on_frame(inner: &Inner, bytes: &[u8]) -> Result<(), InboundError> {
    let envelope = match inner.rpc.parse(bytes) {
        Ok(envelope) => envelope,
        Err(error) => {
            let code = protocol::parse_error_code(&error);
            inner.rpc.emit_null_error(code, &error.to_string()).await;
            return Err(InboundError::Failed(error.to_string()));
        }
    };
    match envelope {
        Envelope::Response { id, body } => {
            inner.rpc.complete(&id, body);
            Ok(())
        }
        Envelope::Notification { method, params } => {
            let _ = dispatch(inner, &method, &params).await?;
            Ok(())
        }
        Envelope::Request { id, method, params } => {
            if method == protocol::METHOD_INBOUND_DELETED {
                return schedule_delete(inner, id, &params).await;
            }
            if let Some(cached) = protocol::idempotency_key(&params).and_then(|key| inner.rpc.cached(&key)) {
                inner
                    .rpc
                    .respond(&id, &cached)
                    .await
                    .map_err(|error| InboundError::Failed(error.to_string()))?;
                return Ok(());
            }
            match dispatch(inner, &method, &params).await {
                Ok(result) => {
                    if let Some(key) = protocol::idempotency_key(&params) {
                        inner.rpc.remember(key, result.clone());
                    }
                    inner
                        .rpc
                        .respond(&id, &result)
                        .await
                        .map_err(|error| InboundError::Failed(error.to_string()))?;
                    if method == protocol::METHOD_HELLO {
                        inner.rpc.flush_replay().await;
                        inner.flush_lost_status().await;
                    }
                    Ok(())
                }
                Err(error) => {
                    let body = error_body(&error);
                    let _ = inner.rpc.respond_error(&id, &body).await;
                    Err(error)
                }
            }
        }
    }
}

async fn dispatch(inner: &Inner, method: &str, params: &Value) -> Result<Value, InboundError> {
    if method != protocol::METHOD_HELLO && !inner.hello_done.load(Ordering::Acquire) {
        return Err(InboundError::HelloRequired);
    }
    match method {
        protocol::METHOD_HELLO => on_hello(inner, params),
        protocol::METHOD_PING => on_ping(inner),
        protocol::METHOD_INBOUND_MESSAGE => on_message(inner, params).await,
        protocol::METHOD_INBOUND_SURFACE => on_surface(inner, params).await,
        protocol::METHOD_INBOUND_TASK => on_task(inner, params).await,
        protocol::METHOD_INBOUND_CONTROL => on_control(inner, params).await,
        protocol::METHOD_INBOUND_DELETED => Err(InboundError::Failed(
            "message delete is scheduled off the reader".to_string(),
        )),
        protocol::METHOD_APPROVAL_DECISION => on_decision(inner, params),
        protocol::METHOD_TURN_CANCEL => on_cancel(inner, params),
        other => Err(InboundError::UnknownMethod(other.to_string())),
    }
}

fn on_hello(inner: &Inner, params: &Value) -> Result<Value, InboundError> {
    let hello: BridgeHello = parse(params)?;
    if hello.protocol_version != protocol::PROTOCOL_VERSION {
        return Err(InboundError::Version(hello.protocol_version));
    }
    if hello.agent_account_id.is_empty() {
        return Err(InboundError::Failed("agent_account_id is empty".to_string()));
    }
    if super::accounts::is_account_removed(&inner.store, &hello.agent_account_id) {
        return Err(InboundError::NotAuthorized);
    }
    if let Err(error) = inner.crypto.unlock(&hello.agent_account_id) {
        if super::accounts::is_account_removed(&inner.store, &hello.agent_account_id) {
            return Err(InboundError::NotAuthorized);
        }
        return Err(InboundError::Failed(error.to_string()));
    }
    inner.auth.set_hello(hello.authorization);
    inner.set_account(&hello.agent_account_id);
    let persona = super::accounts::resolve(&inner.config, &hello.agent_account_id, &inner.workspace);
    let prompt_bytes = persona.system_prompt.as_ref().map(String::len).unwrap_or(0);
    let model = persona.model.clone();
    let provider = persona.provider.clone();
    tracing::info!(
        account_len = hello.agent_account_id.len(),
        authorization = hello.authorization.as_str(),
        effective = inner.auth.effective().as_str(),
        prompt_bytes,
        model = model.as_deref().unwrap_or(""),
        provider = provider.as_deref().unwrap_or(""),
        "ramflux bridge hello"
    );
    *inner.persona.lock() = Some(persona);
    inner.hello_done.store(true, Ordering::Release);
    inner.paused.store(false, Ordering::Release);
    let ack = BridgeHelloAck {
        ok: true,
        prx_version: env!("CARGO_PKG_VERSION").to_string(),
        effective_autonomy: inner.auth.effective().as_str().to_string(),
    };
    serde_json::to_value(ack).map_err(InboundError::BadParams)
}

fn on_ping(inner: &Inner) -> Result<Value, InboundError> {
    inner.note_ping(unix_ms());
    Ok(json!({"ok": true}))
}

async fn on_message(inner: &Inner, params: &Value) -> Result<Value, InboundError> {
    ensure_running(inner)?;
    let message: InboundMessageParams = parse(params)?;
    tracing::debug!(
        conversation_id = %message.conversation_id,
        message_id = %message.message_id,
        bytes = message.text.len(),
        "ramflux inbound message"
    );
    let account_id = inner.account_id.lock().clone();
    if let Some(account_id) = account_id {
        if super::accounts::is_removed(&inner.store, &account_id, &message.conversation_id) {
            tracing::info!(
                conversation_id = %message.conversation_id,
                "ramflux message was not started because the agent was removed"
            );
            return Err(InboundError::NotAuthorized);
        }
        let persona = inner.persona.lock().clone();
        let scope = super::accounts::memory_scope(persona.as_ref(), &account_id, &message.conversation_id);
        inner
            .store
            .put_message(
                &inner.crypto,
                &account_id,
                &scope,
                &message.message_id,
                message.text.as_bytes(),
            )
            .map_err(|error| InboundError::Failed(error.to_string()))?;
    }
    let rich = RichMessage {
        conversation_id: message.conversation_id,
        message_id: message.message_id,
        sender: message.sender,
        chat: message.chat,
        text: message.text,
        attachments: message.attachments,
        reply_target: message.reply_target,
        timestamp_ms: message.timestamp_ms,
    };
    if let Some(target) = rich.reply_target.clone() {
        inner.remember_reply_target(&rich.conversation_id, &target);
    }
    let objects: Vec<String> = rich.attachments.iter().map(|item| item.object_id.clone()).collect();
    inner.track_message(
        &rich.message_id,
        super::TrackedMessage {
            conversation_id: rich.conversation_id.clone(),
            objects,
        },
    );
    match inner.queue.admit(rich) {
        super::turn_queue::Admission::Interrupt(message) => {
            inner.note_slot_message(&message.message_id);
            inner.cancel_tokens(&message.conversation_id);
            inner.emit_inbound(RichInbound::Message(message)).await?;
        }
        super::turn_queue::Admission::Start(message) => {
            inner.note_slot_message(&message.message_id);
            inner.emit_inbound(RichInbound::Message(message)).await?;
        }
        super::turn_queue::Admission::Queued => {}
        super::turn_queue::Admission::Rejected { depth } => {
            return Err(InboundError::QueueFull(depth));
        }
    }
    Ok(json!({"ok": true}))
}

async fn on_surface(inner: &Inner, params: &Value) -> Result<Value, InboundError> {
    ensure_running(inner)?;
    let action: SurfaceActionParams = parse(params)?;
    tracing::debug!(
        conversation_id = %action.conversation_id,
        surface_id = %action.surface_id,
        action_id = %action.action_id,
        "ramflux surface action"
    );
    inner
        .emit_inbound(RichInbound::SurfaceAction(SurfaceActionInbound {
            conversation_id: action.conversation_id,
            sender: action.sender,
            surface_id: action.surface_id,
            version: action.version,
            surface_hash: action.surface_hash,
            action_id: action.action_id,
            values: action.values,
            args_hash: action.args_hash,
            message_id: action.message_id,
        }))
        .await?;
    Ok(json!({"ok": true}))
}

async fn on_task(inner: &Inner, params: &Value) -> Result<Value, InboundError> {
    ensure_running(inner)?;
    let event: TaskEventParams = parse(params)?;
    inner
        .emit_inbound(RichInbound::TaskEvent(TaskEventInbound {
            conversation_id: event.conversation_id,
            sender: event.sender,
            task_id: event.task_id,
            version: event.version,
            kind: event.kind,
            body: event.body,
        }))
        .await?;
    Ok(json!({"ok": true}))
}

async fn on_control(inner: &Inner, params: &Value) -> Result<Value, InboundError> {
    let control: ControlParams = parse(params)?;
    tracing::info!(
        conversation_id = %control.conversation_id,
        kind = ?control.kind,
        "ramflux control event"
    );
    let account_id = inner.account_id.lock().clone().unwrap_or_default();
    match control.kind {
        ControlKind::AgentRemoved => {
            inner.cancel_conversation(&control.conversation_id);
            super::erase::erase_account(&inner.store, &inner.crypto, &account_id)
                .map_err(|error| InboundError::Failed(error.to_string()))?;
            super::accounts::mark_account_removed(&inner.store, &account_id)
                .map_err(|error| InboundError::Failed(error.to_string()))?;
        }
        ControlKind::AgentRevoked => {
            inner.cancel_conversation(&control.conversation_id);
            super::hooks::erase_conversation_scope(inner, &control.conversation_id)
                .await
                .map_err(InboundError::Failed)?;
            super::accounts::mark_removed(
                &inner.store,
                &account_id,
                &control.conversation_id,
                control.timestamp_ms,
            )
            .map_err(|error| InboundError::Failed(error.to_string()))?;
        }
        ControlKind::AgentOffline => {
            inner.crypto.lock(&account_id);
            inner.paused.store(true, Ordering::Release);
            inner.cancel_all();
        }
        ControlKind::AgentOnline => {
            inner
                .crypto
                .unlock(&account_id)
                .map_err(|error| InboundError::Failed(error.to_string()))?;
            inner.paused.store(false, Ordering::Release);
        }
        ControlKind::AgentAdded => {
            super::accounts::clear_removed(&inner.store, &account_id, &control.conversation_id)
                .map_err(|error| InboundError::Failed(error.to_string()))?;
        }
        ControlKind::MemberJoined | ControlKind::MemberLeft | ControlKind::ConversationRenamed => {}
    }
    inner
        .emit_inbound(RichInbound::Control(ControlInbound {
            conversation_id: control.conversation_id,
            kind: control.kind,
            actor: control.actor,
            timestamp_ms: control.timestamp_ms,
        }))
        .await?;
    Ok(json!({"ok": true}))
}

/// Queue the erase and return to the read loop. The JSON-RPC result is sent
/// by a separate task after the eraser finishes, so this task can still read
/// an ingress ack.
async fn schedule_delete(inner: &Inner, id: super::protocol::RpcId, params: &Value) -> Result<(), InboundError> {
    let deleted: MessageDeletedParams = parse(params)?;
    tracing::info!(
        conversation_id = %deleted.conversation_id,
        message_id = %deleted.message_id,
        "ramflux message deleted"
    );
    let account = inner.account_id.lock().clone().unwrap_or_default();
    let entry = super::erase_index::load(
        &inner.store,
        &inner.crypto,
        &account,
        &deleted.conversation_id,
        &deleted.message_id,
    );
    let run_ids = entry
        .as_ref()
        .map(|entry| super::eraser::run_ids_from_keys(&entry.memory_keys))
        .unwrap_or_default();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let job = super::eraser::EraseJob {
        account,
        conversation: deleted.conversation_id.clone(),
        message_ids: vec![deleted.message_id.clone()],
        run_ids,
        done: done_tx,
    };
    let sender = inner.erase_tx.lock().clone();
    let Some(sender) = sender else {
        return Err(InboundError::Failed("eraser is not running".to_string()));
    };
    match sender.try_send(job) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            let body = error_body(&InboundError::QueueFull(super::eraser::QUEUE_CAP));
            let _ = inner.rpc.respond_error(&id, &body).await;
            return Ok(());
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            return Err(InboundError::Failed("eraser is not running".to_string()));
        }
    }
    let rpc = std::sync::Arc::clone(&inner.rpc);
    let inbound = inner.inbound_tx.lock().clone();
    let notice = MessageDeletedInbound {
        conversation_id: deleted.conversation_id,
        message_id: deleted.message_id,
        deleted_by: deleted.deleted_by,
        timestamp_ms: deleted.timestamp_ms,
    };
    tokio::spawn(async move {
        let outcome = done_rx.await.unwrap_or_else(|_| Err("eraser stopped".to_string()));
        match outcome {
            Ok(()) => {
                let _ = rpc.respond(&id, &json!({"erased": true})).await;
                if let Some(tx) = inbound {
                    let _ = tx.send(RichInbound::MessageDeleted(notice)).await;
                }
            }
            Err(error) => {
                let body = super::protocol::RpcErrorBody::new(-32603, error);
                let _ = rpc.respond_error(&id, &body).await;
            }
        }
    });
    Ok(())
}

fn on_decision(inner: &Inner, params: &Value) -> Result<Value, InboundError> {
    let decision: ApprovalDecisionParams = parse(params)?;
    tracing::info!(
        conversation_id = %decision.conversation_id,
        approval_id = %decision.approval_id,
        decision = ?decision.decision,
        "ramflux approval decision"
    );
    let matched = inner.approvals.decide(&decision.approval_id, decision.decision);
    Ok(json!({"ok": matched}))
}

fn on_cancel(inner: &Inner, params: &Value) -> Result<Value, InboundError> {
    let cancel: TurnCancelParams = parse(params)?;
    tracing::info!(
        conversation_id = %cancel.conversation_id,
        turn_id = %cancel.turn_id,
        "ramflux turn cancel"
    );
    let cancelled = inner.cancel_turn(&cancel.conversation_id, &cancel.turn_id);
    Ok(json!({"ok": cancelled}))
}

fn ensure_running(inner: &Inner) -> Result<(), InboundError> {
    if inner.paused.load(Ordering::Acquire) {
        Err(InboundError::Paused)
    } else {
        Ok(())
    }
}

fn parse<T: DeserializeOwned>(params: &Value) -> Result<T, InboundError> {
    serde_json::from_value(params.clone()).map_err(InboundError::BadParams)
}

fn error_body(error: &InboundError) -> RpcErrorBody {
    let (code, message) = match error {
        InboundError::HelloRequired => (-32002, error.to_string()),
        InboundError::Paused => (-32001, error.to_string()),
        InboundError::Version(_) | InboundError::NotAuthorized => (-32003, error.to_string()),
        InboundError::QueueFull(_) => (-32005, error.to_string()),
        InboundError::UnknownMethod(_) => (-32601, error.to_string()),
        InboundError::BadParams(_) => (-32602, "invalid bridge params".to_string()),
        InboundError::Failed(message) => (-32603, message.clone()),
    };
    RpcErrorBody::new(code, message)
}
