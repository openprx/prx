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
    #[error("{0}")]
    TurnId(String),
    #[error("memory capability unsupported")]
    Unsupported,
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
pub async fn on_frame(inner: &std::sync::Arc<Inner>, bytes: &[u8]) -> Result<(), InboundError> {
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
            if needs_background_control(&method, &params) {
                let inner = std::sync::Arc::clone(inner);
                tokio::spawn(async move {
                    if let Err(error) = dispatch(&inner, &method, &params).await {
                        tracing::warn!(%error, "ramflux control failed");
                    }
                });
                return Ok(());
            }
            let _ = dispatch(inner, &method, &params).await?;
            Ok(())
        }
        Envelope::Request { id, method, params } => {
            if needs_background_control(&method, &params) {
                let inner = std::sync::Arc::clone(inner);
                tokio::spawn(async move {
                    match dispatch(&inner, &method, &params).await {
                        Ok(result) => {
                            let _ = inner.rpc.respond(&id, &result).await;
                        }
                        Err(error) => {
                            let _ = inner.rpc.respond_error(&id, &error_body(&error)).await;
                        }
                    }
                });
                return Ok(());
            }
            if method == protocol::METHOD_INBOUND_DELETED {
                if let Err(error) = schedule_delete(inner, id.clone(), &params) {
                    let _ = inner.rpc.respond_error(&id, &error_body(&error)).await;
                    return Err(error);
                }
                return Ok(());
            }
            if method == protocol::METHOD_INBOUND_MESSAGE || method == protocol::METHOD_INBOUND_SURFACE {
                let checked = if method == protocol::METHOD_INBOUND_MESSAGE {
                    parse::<InboundMessageParams>(&params).and_then(|message| {
                        inner
                            .rpc
                            .turn_ids
                            .check_message(
                                &message.conversation_id,
                                &message.message_id,
                                message.turn_id.as_deref(),
                            )
                            .map(|_| ())
                            .map_err(InboundError::TurnId)
                    })
                } else {
                    parse::<SurfaceActionParams>(&params).and_then(|action| {
                        super::turn_ids::TurnIds::validate(action.turn_id.as_deref()).map_err(InboundError::TurnId)
                    })
                };
                if let Err(error) = checked {
                    let _ = inner.rpc.respond_error(&id, &error_body(&error)).await;
                    return Err(error);
                }
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

fn needs_background_control(method: &str, params: &Value) -> bool {
    method == protocol::METHOD_INBOUND_CONTROL
        && matches!(
            params.get("kind").and_then(Value::as_str),
            Some("agent_revoked" | "agent_removed")
        )
}

async fn dispatch(inner: &std::sync::Arc<Inner>, method: &str, params: &Value) -> Result<Value, InboundError> {
    if method != protocol::METHOD_HELLO && !inner.hello_done.load(Ordering::Acquire) {
        let readd = method == protocol::METHOD_INBOUND_CONTROL
            && params.get("kind").and_then(Value::as_str) == Some("agent_added");
        if !readd {
            return Err(InboundError::HelloRequired);
        }
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
    if inner
        .account_id
        .lock()
        .as_ref()
        .is_some_and(|account| account != &hello.agent_account_id)
    {
        return Err(InboundError::NotAuthorized);
    }
    if super::accounts::is_account_removed(&inner.store, &hello.agent_account_id) {
        // Remember the identified account so the authenticated socket peer can
        // deliver agent_added before a successful hello after a restart.
        inner.set_account(&hello.agent_account_id);
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

pub(super) fn prepare_message(
    inner: &Inner,
    params: &Value,
) -> Result<Option<super::turn_queue::Admission>, InboundError> {
    prepare_reserved_message(inner, params, None)
}

pub(super) fn prepare_reserved_message(
    inner: &Inner,
    params: &Value,
    reserved: Option<std::sync::Arc<super::turn_ids::Reservation>>,
) -> Result<Option<super::turn_queue::Admission>, InboundError> {
    let _admission_guard = inner.admission.lock();
    ensure_running(inner)?;
    let message: InboundMessageParams = parse(params)?;
    if reserved.is_none()
        && inner
            .rpc
            .turn_ids
            .check_message(
                &message.conversation_id,
                &message.message_id,
                message.turn_id.as_deref(),
            )
            .map_err(InboundError::TurnId)?
    {
        return Ok(Some(super::turn_queue::Admission::Queued));
    }
    super::turn_ids::TurnIds::validate(message.turn_id.as_deref()).map_err(InboundError::TurnId)?;
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
        let backend = inner.store.backend().map_err(|_| InboundError::Unsupported)?;
        let source = inner.source_message_id(&message.conversation_id, &message.message_id);
        let origin = crate::memory::erasure::ErasureOrigin::new(&account_id, &message.conversation_id, &source);
        if inner
            .deleting
            .lock()
            .contains(&(account_id, message.conversation_id.clone(), source))
            || backend
                .is_deleted(&origin)
                .map_err(|error| InboundError::Failed(error.to_string()))?
        {
            return Ok(None);
        }
        // The indexed inbound event is the durable content copy. Queued input
        // remains in the identity-owned queue until that transaction commits.
    }
    inner
        .conversations
        .lock()
        .insert(message.conversation_id.clone(), message.chat.clone());
    let reservation = match reserved {
        Some(reserved) => reserved,
        None => inner
            .rpc
            .turn_ids
            .reserve(
                &message.conversation_id,
                &message.message_id,
                message.turn_id.as_deref(),
                |run| inner.rpc.has_pending(run),
            )
            .map_err(InboundError::TurnId)?,
    };
    let conversation = message.conversation_id.clone();
    let message_id = message.message_id.clone();
    let host_id = message.turn_id;
    let rich = RichMessage {
        reservation: Some(reservation),
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
            sender: rich.sender.clone(),
        },
    );
    let admission = inner.queue.admit(rich);
    if !matches!(admission, super::turn_queue::Admission::Rejected { .. }) {
        inner
            .rpc
            .turn_ids
            .remember(&conversation, &message_id, host_id.as_deref());
    }
    let pending = inner.queue.pending_messages(&conversation);
    inner.rpc.turn_ids.release_displaced(&conversation, &pending);
    if let super::turn_queue::Admission::Start(message) | super::turn_queue::Admission::Interrupt(message) = &admission
    {
        inner.note_slot_message(&message.conversation_id, &message.message_id);
    }
    Ok(Some(admission))
}

pub(super) async fn on_message(inner: &std::sync::Arc<Inner>, params: &Value) -> Result<Value, InboundError> {
    let Some(admission) = prepare_message(inner, params)? else {
        return Ok(json!({"ok":true,"deleted":true}));
    };
    match admission {
        super::turn_queue::Admission::Interrupt(message) => {
            inner.cancel_tokens(&message.conversation_id);
            inner.emit_message(message).await?;
        }
        super::turn_queue::Admission::Start(message) => {
            inner.emit_message(message).await?;
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
    super::turn_ids::TurnIds::validate(action.turn_id.as_deref()).map_err(InboundError::TurnId)?;
    if action.values.get("approval_id").is_some() {
        // Grants are authenticated through approval.decision, never card values.
        return Err(InboundError::NotAuthorized);
    }
    tracing::debug!(
        conversation_id = %action.conversation_id,
        surface_id = %action.surface_id,
        action_id = %action.action_id,
        "ramflux surface action"
    );
    let derived_message_id = format!("{}:{}:{}", action.message_id, action.action_id, uuid::Uuid::new_v4());
    let reservation = {
        let _admission = inner.admission.lock();
        ensure_running(inner)?;
        inner
            .rpc
            .turn_ids
            .reserve(
                &action.conversation_id,
                &derived_message_id,
                action.turn_id.as_deref(),
                |run| inner.rpc.has_pending(run),
            )
            .map_err(InboundError::TurnId)?
    };
    inner
        .emit_inbound(RichInbound::SurfaceAction(SurfaceActionInbound {
            derived_message_id,
            reservation: Some(reservation),
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
    let account_id = inner
        .account_id
        .lock()
        .clone()
        .or_else(|| {
            (inner.config.accounts.len() == 1)
                .then(|| inner.config.accounts.keys().next().cloned())
                .flatten()
        })
        .ok_or(InboundError::HelloRequired)?;
    inner.set_account(&account_id);
    match control.kind {
        ControlKind::AgentRemoved => {
            super::accounts::mark_account_removed(&inner.store, &account_id)
                .map_err(|error| InboundError::Failed(error.to_string()))?;
            inner.paused.store(true, Ordering::Release);
            super::hooks::erase_conversation_scope(inner, &control.conversation_id, true)
                .await
                .map_err(InboundError::Failed)?;
            inner.cancel_conversation(&control.conversation_id);
            super::erase::erase_account(&inner.store, &inner.crypto, &account_id)
                .map_err(|error| InboundError::Failed(error.to_string()))?;
            super::accounts::mark_account_removed(&inner.store, &account_id)
                .map_err(|error| InboundError::Failed(error.to_string()))?;
        }
        ControlKind::AgentRevoked => {
            super::accounts::mark_removed(
                &inner.store,
                &account_id,
                &control.conversation_id,
                control.timestamp_ms,
            )
            .map_err(|error| InboundError::Failed(error.to_string()))?;
            super::hooks::erase_conversation_scope(inner, &control.conversation_id, false)
                .await
                .map_err(InboundError::Failed)?;
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
            super::accounts::clear_account_removed(&inner.store, &account_id)
                .map_err(|error| InboundError::Failed(error.to_string()))?;
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
fn schedule_delete(
    inner: &std::sync::Arc<Inner>,
    id: super::protocol::RpcId,
    params: &Value,
) -> Result<(), InboundError> {
    if !inner.hello_done.load(Ordering::Acquire) {
        return Err(InboundError::HelloRequired);
    }
    ensure_running(inner)?;
    let deleted: MessageDeletedParams = parse(params)?;
    inner.store.backend().map_err(|_| InboundError::Unsupported)?;
    tracing::info!(
        conversation_id = %deleted.conversation_id,
        message_id = %deleted.message_id,
        "ramflux message deleted"
    );
    // Refusal must have no admission side effects. Reserve the worker slot
    // before cancelling input or installing the deletion barrier.
    let sender = inner
        .erase_tx
        .lock()
        .clone()
        .ok_or_else(|| InboundError::Failed("eraser is not running".to_owned()))?;
    let permit = sender.try_reserve_owned().map_err(|error| match error {
        tokio::sync::mpsc::error::TrySendError::Full(_) => InboundError::QueueFull(super::eraser::QUEUE_CAP),
        tokio::sync::mpsc::error::TrySendError::Closed(_) => InboundError::Failed("eraser is not running".to_owned()),
    })?;
    let account = inner.account_id.lock().clone().unwrap_or_default();
    {
        let _admission = inner.admission.lock();
        inner.deleting.lock().insert((
            account.clone(),
            deleted.conversation_id.clone(),
            deleted.message_id.clone(),
        ));
        inner
            .queue
            .suspend_delete(&deleted.conversation_id, &deleted.message_id);
        inner.cancel_message(&deleted.conversation_id, &deleted.message_id);
    }
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let job = super::eraser::EraseJob {
        account,
        conversation: deleted.conversation_id.clone(),
        message_ids: vec![deleted.message_id.clone()],
        done: done_tx,
    };
    permit.send(job);
    let recovery = std::sync::Arc::clone(inner);
    let recovery_account = inner.account_id.lock().clone().unwrap_or_default();
    let recovery_conversation = deleted.conversation_id.clone();
    let recovery_message = deleted.message_id.clone();
    let rpc = std::sync::Arc::clone(&inner.rpc);
    let inbound = inner.inbound_tx.lock().clone();
    let notice = MessageDeletedInbound {
        conversation_id: deleted.conversation_id,
        message_id: deleted.message_id,
        deleted_by: deleted.deleted_by,
        timestamp_ms: deleted.timestamp_ms,
    };
    tokio::spawn(async move {
        let outcome = done_rx.await.unwrap_or_else(|_| {
            recovery.resume_delete(&recovery_account, &recovery_conversation, &recovery_message);
            Err("eraser stopped".to_owned())
        });
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
    let matched = inner.approvals.decide(
        &decision.approval_id,
        decision.decision,
        &decision.conversation_id,
        &decision.decided_by,
    );
    Ok(json!({"ok": matched}))
}

fn on_cancel(inner: &Inner, params: &Value) -> Result<Value, InboundError> {
    let cancel: TurnCancelParams = parse(params)?;
    tracing::info!(
        conversation_id = %cancel.conversation_id,
        turn_id = %cancel.turn_id,
        "ramflux turn cancel"
    );
    super::turn_ids::TurnIds::validate(Some(&cancel.turn_id)).map_err(InboundError::TurnId)?;
    let cancelled = inner
        .rpc
        .turn_ids
        .resolve(&cancel.conversation_id, &cancel.turn_id)
        .is_some_and(|run| inner.cancel_turn(&cancel.conversation_id, &run));
    Ok(json!({"ok": cancelled}))
}

fn ensure_running(inner: &Inner) -> Result<(), InboundError> {
    if inner.paused.load(Ordering::Acquire) || inner.revocations.load(Ordering::Acquire) != 0 {
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
        InboundError::Unsupported => (-32601, error.to_string()),
        InboundError::UnknownMethod(_) => (-32601, error.to_string()),
        InboundError::TurnId(message) => (-32602, message.clone()),
        InboundError::BadParams(_) => (-32602, "invalid bridge params".to_string()),
        InboundError::Failed(message) => (-32603, message.clone()),
    };
    RpcErrorBody::new(code, message)
}
