//! Transcript and checkpoint have exactly one commit boundary.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::{Transaction, TransactionBehavior, params};
use uuid::Uuid;

use super::{
    SqliteMailbox,
    model::{DeliveryMeta, EventRef, decode, encode},
    pin::check_scope,
    receipt, storage_error, stream,
};
use crate::messaging::DeliveryState;
use crate::messaging::mailbox::{MailboxError, MailboxReceipt, MailboxResult, validate_body};

impl SqliteMailbox {
    /// Adopt a delivered slot. The caller owns its scope delivery lease until
    /// this returns or its cancellation/IPC handoff completes.
    pub fn adopt(&self, delivery: &super::MailboxDelivery) -> MailboxResult<MailboxReceipt> {
        let scope = delivery.scope();
        let row_id = delivery.row_id();
        let now = self.clock.now();
        if now < 0 {
            return Err(MailboxError::InvalidMetadata);
        }
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        let key = crate::messaging::mailbox::ledger_key(&scope.workspace_id, delivery.message())?;
        let mut saved = receipt::by_key(&tx, &key)?.ok_or(MailboxError::ReceiptExpired)?;
        if saved.id != row_id || saved.message_id != delivery.message().id {
            return Err(MailboxError::ReceiptExpired);
        }
        if saved.scope != *scope {
            return Err(MailboxError::Unauthorized);
        }
        saved.matches(&tx, &scope.workspace_id, delivery.message())?;
        if saved.meta.body_erased_at.is_some() {
            return Err(MailboxError::Fenced);
        }
        let mut stream = stream::load(&tx, saved.stream_id)?;
        check_scope(&tx, scope, &stream.to, Some(&saved.meta.canonical_session_key), true)?;
        if saved.state == DeliveryState::Consumed {
            return saved.result(&tx);
        }
        let live_run = self.memory.mailbox_runtime.execution(&stream.to, scope);
        if saved
            .meta
            .common
            .exec_run
            .as_deref()
            .is_some_and(|run| Some(run) != live_run.as_deref())
        {
            return Err(MailboxError::StaleRun);
        }
        if stream.retired {
            return Err(MailboxError::SeqStreamRetired);
        }
        if saved.state != DeliveryState::Delivered {
            return Err(MailboxError::CorruptState);
        }
        // A later delivered message cannot be adopted before the lower slot.
        let mut next = stream
            .progress
            .consume_cursor
            .checked_add(1)
            .ok_or(MailboxError::CorruptState)?;
        while stream.progress.seq_skipped.iter().any(|h| h.seq == next) {
            next = next.checked_add(1).ok_or(MailboxError::CorruptState)?;
        }
        if saved.seq != next {
            return Err(MailboxError::SeqConflict);
        }
        let (mime,bytes,delivery_json):(String,Vec<u8>,String)=tx.query_row("SELECT body_mime,body_bytes,delivery_meta FROM prx_mailbox_messages WHERE id=?1 AND stream_id=?2 AND state=1 AND ma_scope_id=?3 AND seq=?4",params![row_id,stream.id,scope.scope_id.to_string(),saved.seq.to_be_bytes().as_slice()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(storage_error)?;
        validate_body(&mime, &bytes)?;
        if mime != delivery.message().body.mime || bytes != delivery.message().body.bytes {
            return Err(MailboxError::IdentityImmutable);
        }
        let pin: DeliveryMeta = decode(&delivery_json)?;
        if pin.to_pin_token.is_empty() {
            return Err(MailboxError::CorruptState);
        }
        let content = project_body(&mime, &bytes)?;
        let timestamp = chrono::DateTime::from_timestamp(now, 0)
            .ok_or(MailboxError::InvalidMetadata)?
            .to_rfc3339();
        let key = &saved.meta.canonical_session_key;
        tx.execute("INSERT INTO sessions(session_key,channel,sender,owner_id,created_at,updated_at) VALUES (?1,'mailbox',?2,?2,?3,?3) ON CONFLICT(session_key) DO NOTHING",params![key,scope.owner_id,timestamp]).map_err(storage_error)?;
        let owner: Option<String> = tx
            .query_row("SELECT owner_id FROM sessions WHERE session_key=?1", [key], |r| {
                r.get(0)
            })
            .map_err(storage_error)?;
        if owner.as_deref() != Some(scope.owner_id.as_str()) {
            return Err(MailboxError::Unauthorized);
        }
        let event_id = Uuid::new_v4();
        let reference = EventRef {
            workspace_id: scope.workspace_id.clone(),
            owner_id: scope.owner_id.clone(),
            session_id: stream.to.session.clone(),
            agent: stream.to.agent.clone(),
            from_instance: stream.from.instance.clone(),
            idempotency_key: saved.idempotency.as_str().to_owned(),
            message_id: saved.message_id,
            acceptance_id: saved.meta.acceptance_id,
            local_stream_id: stream.id,
            wire_stream_id: stream.wire_id,
            seq: saved.seq,
            scope: scope.clone(),
            common: saved.meta.common.clone(),
        };
        let agent = (stream.to.agent != "main").then_some(stream.to.agent.as_str());
        tx.execute("INSERT INTO message_events(event_id,idempotency_key,workspace_id,owner_id,source,session_key,role,event_type,source_ref_json,content,visibility,created_at,updated_at,agent_id,ma_scope_id) VALUES (?1,?2,?3,?4,'prx_mailbox',?5,'user','message.created',?6,?7,'private',?8,?8,?9,?10)",params![event_id.to_string(),format!("prx_mailbox/{}/{}",saved.message_id,saved.meta.acceptance_id),scope.workspace_id,scope.owner_id,key,encode(&reference)?,content,timestamp,agent,scope.scope_id.to_string()]).map_err(storage_error)?;
        tx.execute("INSERT INTO conversation_turns(session_key,owner_id,role,content,timestamp,message_id,message_event_id,agent_id,visibility,ma_scope_id) VALUES (?1,?2,'user',?3,?4,?5,?6,?7,'private',?8)",params![key,scope.owner_id,content,timestamp,saved.message_id.to_string(),event_id.to_string(),agent,scope.scope_id.to_string()]).map_err(storage_error)?;
        saved.adopted_turn_id = Some(tx.last_insert_rowid());
        tx.execute(
            "UPDATE sessions SET message_count=message_count+1,updated_at=?1 WHERE session_key=?2 AND owner_id=?3",
            params![timestamp, key, scope.owner_id],
        )
        .map_err(storage_error)?;
        saved.state = DeliveryState::Consumed;
        saved.meta.consumed_at = Some(now);
        saved.meta.updated_at = now;
        saved.meta.adopted_message_event_id = Some(event_id);
        tx.execute(
            "UPDATE prx_mailbox_receipts SET state=2,adopted_turn_id=?1,receipt_meta=?2 WHERE id=?3 AND state=1",
            params![saved.adopted_turn_id, encode(&saved.meta)?, row_id],
        )
        .map_err(storage_error)?;
        tx.execute(
            "UPDATE prx_mailbox_messages SET state=2 WHERE id=?1 AND state=1",
            [row_id],
        )
        .map_err(storage_error)?;
        stream.progress.consume_cursor = saved.seq;
        while let Some(next) = stream.progress.consume_cursor.checked_add(1) {
            if !stream.progress.seq_skipped.iter().any(|h| h.seq == next) {
                break;
            }
            stream.progress.consume_cursor = next;
        }
        stream.progress.deliver_cursor = stream.progress.deliver_cursor.max(stream.progress.consume_cursor);
        stream::save_progress(&tx, &stream, now)?;
        super::crash_barrier("inside_adopt_tx")?;
        let result = receipt::load(&tx, row_id)?.result(&tx)?;
        tx.commit().map_err(storage_error)?;
        super::crash_barrier("after_adopt_commit")?;
        Ok(result)
    }
}

fn project_body(mime: &str, bytes: &[u8]) -> MailboxResult<String> {
    if matches!(mime, "text/plain" | "text/markdown") {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Ok(text.to_owned());
        }
    }
    Ok(format!(
        "[mailbox MIME {} base64]\n{}",
        encode(&mime)?,
        STANDARD.encode(bytes)
    ))
}
