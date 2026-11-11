//! Sender acknowledgments and pending envelopes. These never emit an Ack.
use super::{
    SqliteMailbox,
    model::{OutboxMeta, decode, encode, sequence, uuid},
    pin::check_scope,
    storage_error, stream,
};
use crate::messaging::mailbox::{MailboxError, MailboxResult, MailboxScope, REPLAY_SECONDS, validate_body};
use crate::messaging::{
    DeliveryState, ExecRef, IdempotencyKey, MessageBody, MessageKind, PrxMessage, RecipientPin, SessionAddr,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

pub struct MailboxOutbound {
    message: PrxMessage,
    _lease: super::runtime::Lease,
}
impl MailboxOutbound {
    pub const fn message(&self) -> &PrxMessage {
        &self.message
    }
}

impl SqliteMailbox {
    pub fn pending_outbox(&self, scope: &MailboxScope, address: &SessionAddr) -> MailboxResult<Vec<MailboxOutbound>> {
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        check_scope(&tx, scope, address, None, true)?;
        if address.instance != self.local_instance {
            return Err(MailboxError::Unauthorized);
        }
        let mut query=tx.prepare("SELECT o.id,o.stream_id,o.seq,o.idempotency_key,o.message_id,o.body_bytes,o.envelope_meta FROM prx_mailbox_outbox o WHERE o.stream_id IN(SELECT stream_id FROM prx_mailbox_seq WHERE sender_ma_scope_id=?1 AND from_addr=?2) AND o.state IN(0,1) AND o.ma_scope_id=?1 ORDER BY o.stream_id,o.seq").map_err(storage_error)?;
        let rows = query
            .query_map(params![scope.scope_id.to_string(), encode(address)?], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                    r.get::<_, Vec<u8>>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        let mut messages = Vec::new();
        for (_, stream_id, seq, idempotency, id, bytes, json) in rows {
            let stream = stream::load(&tx, stream_id)?;
            if stream.sender.as_ref() != Some(scope) {
                return Err(MailboxError::StaleIncarnation);
            }
            let id = uuid(&id)?;
            let meta: OutboxMeta = decode(&json)?;
            meta.common.validate(id)?;
            if meta.body_erased_at.is_some() {
                return Err(MailboxError::Fenced);
            }
            if meta.next_attempt_at.is_some_and(|when| when > self.clock.now()) {
                continue;
            }
            let mime = meta.body_mime.ok_or(MailboxError::CorruptState)?;
            validate_body(&mime, &bytes)?;
            let token = meta.to_pin_token.ok_or(MailboxError::CorruptState)?;
            if token.is_empty() || meta.to_pin.generation < 1 {
                return Err(MailboxError::CorruptState);
            }
            let digest = meta.common.request_digest.clone();
            let outbound = MailboxOutbound {
                message: PrxMessage {
                    id,
                    idempotency: IdempotencyKey::parse(&idempotency).map_err(|_| MailboxError::CorruptState)?,
                    stream_id: Some(stream.wire_id),
                    seq: sequence(&seq)?,
                    ts: meta.wire_ts,
                    from: stream.from,
                    to: stream.to,
                    to_pin: RecipientPin {
                        incarnation: meta.to_pin.incarnation,
                        generation: meta.to_pin.generation,
                        token,
                    },
                    in_reply_to: meta.common.in_reply_to,
                    hop_count: meta.common.hop_count,
                    ancestry: meta.common.ancestry,
                    kind: MessageKind::Data,
                    body: MessageBody { mime, bytes },
                    origin: Some(meta.common.origin),
                    auth: meta.common.auth,
                    exec: meta.common.exec_run.map(|run| ExecRef { run: Some(run) }),
                    canonical_hash: None,
                },
                _lease: self.memory.mailbox_runtime.lease(scope.scope_id),
            };
            if crate::messaging::request_digest(outbound.message()) != digest {
                return Err(MailboxError::IdentityImmutable);
            }
            messages.push(outbound);
        }
        drop(query);
        tx.commit().map_err(storage_error)?;
        Ok(messages)
    }

    pub fn acknowledge(
        &self,
        scope: &MailboxScope,
        peer: &SessionAddr,
        id: Uuid,
        state: DeliveryState,
    ) -> MailboxResult<()> {
        self.update_outbox(scope, peer, id, Ok(state))
    }

    pub fn reject_outbox(
        &self,
        scope: &MailboxScope,
        peer: &SessionAddr,
        id: Uuid,
        error: MailboxError,
    ) -> MailboxResult<()> {
        self.update_outbox(scope, peer, id, Err(error))
    }

    fn update_outbox(
        &self,
        scope: &MailboxScope,
        peer: &SessionAddr,
        message_id: Uuid,
        outcome: MailboxResult<DeliveryState>,
    ) -> MailboxResult<()> {
        let now = self.clock.now();
        if now < 0 {
            return Err(MailboxError::InvalidMetadata);
        }
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        // Parent UNIQUE(W,O,id) is the query-backed lookup; ids are shared.
        let found=tx.query_row("SELECT o.id,o.stream_id,o.state,o.envelope_meta FROM prx_mailbox_parents p JOIN prx_mailbox_outbox o ON o.id=p.id WHERE p.workspace_id=?1 AND p.owner_id=?2 AND p.message_id=?3 AND o.ma_scope_id=?4",params![scope.workspace_id,scope.owner_id,message_id.as_bytes().as_slice(),scope.scope_id.to_string()],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?))).optional().map_err(storage_error)?.ok_or(MailboxError::InvalidMetadata)?;
        let stream = stream::load(&tx, found.1)?;
        if stream.sender.as_ref() != Some(scope) || *peer != stream.to || stream.from.instance != self.local_instance {
            return Err(MailboxError::Unauthorized);
        }
        check_scope(&tx, scope, &stream.from, None, true)?;
        let mut meta: OutboxMeta = decode(&found.3)?;
        meta.common.validate(message_id)?;
        if meta.body_erased_at.is_some() {
            return Err(MailboxError::Fenced);
        }
        let code = match outcome {
            Ok(state) => {
                // A delayed accepted Ack cannot regress a delivered/consumed Ack.
                let incoming = super::receipt::code(state);
                let previous = meta.ack_state.map(super::receipt::code).unwrap_or(-1);
                if incoming >= previous {
                    meta.ack_state = Some(state);
                }
                if found.2 == 3 {
                    return Ok(());
                }
                2
            }
            Err(error) => {
                if found.2 == 2 {
                    return Ok(());
                }
                meta.last_error = Some(error.to_string());
                3
            }
        };
        let terminal = *meta.terminal_at.get_or_insert(now);
        meta.updated_at = now;
        let expiry = terminal
            .checked_add(REPLAY_SECONDS)
            .ok_or(MailboxError::InvalidMetadata)?;
        tx.execute(
            "UPDATE prx_mailbox_outbox SET state=?1,expires_at=?2,envelope_meta=?3 WHERE id=?4",
            params![code, expiry, encode(&meta)?, found.0],
        )
        .map_err(storage_error)?;
        tx.commit().map_err(storage_error)
    }
}
