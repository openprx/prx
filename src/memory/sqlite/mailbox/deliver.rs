//! FIFO selection and scoped queue/IPC leases. A lease survives handoff.
use super::{
    SqliteMailbox,
    model::{DeliveryMeta, Hole, decode, encode, sequence},
    pin::check_scope,
    receipt, storage_error, stream,
};
use crate::messaging::mailbox::{GAP_SECONDS, MAX_HOLES, MailboxError, MailboxResult, MailboxScope, validate_body};
use crate::messaging::{DeliveryState, ExecRef, MessageBody, MessageKind, PrxMessage, RecipientPin, SessionAddr};
use rusqlite::{Transaction, TransactionBehavior, params};

pub struct MailboxDelivery {
    row_id: i64,
    scope: MailboxScope,
    message: PrxMessage,
    _lease: super::runtime::Lease,
}
impl MailboxDelivery {
    pub const fn row_id(&self) -> i64 {
        self.row_id
    }
    pub const fn scope(&self) -> &MailboxScope {
        &self.scope
    }
    pub const fn message(&self) -> &PrxMessage {
        &self.message
    }
}

impl SqliteMailbox {
    /// Called by the owning runtime when an execution opens/closes. There is
    /// no production registration in this module; Steer remains separate.
    pub fn set_execution(&self, scope: &MailboxScope, address: &SessionAddr, run: Option<String>) -> MailboxResult<()> {
        let writer = self.memory.pool.write();
        check_scope(&writer, scope, address, None, run.is_some())?;
        if address.instance != self.local_instance
            || run
                .as_ref()
                .is_some_and(|r| r.is_empty() || r.len() > 512 || r.contains('\0'))
        {
            return Err(MailboxError::InvalidAddress);
        }
        self.memory
            .mailbox_runtime
            .set_execution(address.clone(), scope.clone(), run);
        Ok(())
    }

    pub fn close_execution(
        &self,
        scope: &MailboxScope,
        address: &SessionAddr,
        expected_run: &str,
    ) -> MailboxResult<()> {
        let writer = self.memory.pool.write();
        check_scope(&writer, scope, address, None, false)?;
        self.memory
            .mailbox_runtime
            .close_execution(address, scope, expected_run);
        Ok(())
    }

    pub fn deliver(&self, scope: &MailboxScope, address: &SessionAddr) -> MailboxResult<Option<MailboxDelivery>> {
        if address.instance != self.local_instance {
            return Err(MailboxError::Unauthorized);
        }
        let now = self.clock.now();
        if now < 0 {
            return Err(MailboxError::InvalidMetadata);
        }
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        check_scope(&tx, scope, address, None, true)?;
        let live_run = self.memory.mailbox_runtime.execution(address, scope);
        let mut query=tx.prepare("SELECT stream_id FROM prx_mailbox_seq WHERE recipient_ma_scope_id=?1 AND to_addr=?2 ORDER BY stream_id").map_err(storage_error)?;
        let ids = query
            .query_map(params![scope.scope_id.to_string(), encode(address)?], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        drop(query);
        for id in ids {
            let mut stream = stream::load(&tx, id)?;
            if stream.retired {
                continue;
            }
            if stream.recipient.as_ref() != Some(scope) {
                return Err(MailboxError::StaleIncarnation);
            }
            let Some(next) = stream.progress.deliver_cursor.checked_add(1) else {
                continue;
            };
            let mut query=tx.prepare("SELECT id,seq FROM prx_mailbox_messages WHERE stream_id=?1 AND state IN(0,1) AND seq>=?2 ORDER BY seq LIMIT 1").map_err(storage_error)?;
            let candidates = query
                .query_map(params![id, next.to_be_bytes().as_slice()], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(storage_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage_error)?;
            drop(query);
            let Some((row_id, seq)) = candidates.first() else {
                continue;
            };
            let seq = sequence(seq)?;
            let mut saved = receipt::load(&tx, *row_id)?;
            if saved.scope != *scope || saved.stream_id != id || saved.seq != seq || saved.meta.body_erased_at.is_some()
            {
                return Err(MailboxError::CorruptState);
            }
            check_scope(&tx, scope, address, Some(&saved.meta.canonical_session_key), true)?;
            if seq > next {
                if stream.progress.gap_seq != Some(next) {
                    stream.progress.gap_seq = Some(next);
                    stream.progress.gap_started_at = Some(saved.meta.accepted_at);
                }
                let started = stream.progress.gap_started_at.ok_or(MailboxError::CorruptState)?;
                if now.saturating_sub(started) < GAP_SECONDS {
                    stream::save_progress(&tx, &stream, now)?;
                    continue;
                }
                let count = seq.checked_sub(next).ok_or(MailboxError::CorruptState)?;
                if count
                    > u64::try_from(MAX_HOLES.saturating_sub(stream.progress.seq_skipped.len()))
                        .map_err(|_| MailboxError::CorruptState)?
                {
                    return Err(MailboxError::Quota("seq"));
                }
                super::outbox::nack_gaps(&tx, &stream, next, seq, now)?;
                for missing in next..seq {
                    stream.progress.seq_skipped.push(Hole {
                        seq: missing,
                        skipped_at: now,
                    });
                }
                stream.progress.seq_skipped.sort_by_key(|hole| hole.seq);
                stream.progress.seq_skipped.dedup_by_key(|hole| hole.seq);
                stream.progress.deliver_cursor = seq.checked_sub(1).ok_or(MailboxError::CorruptState)?;
                while let Some(next) = stream.progress.consume_cursor.checked_add(1) {
                    if !stream.progress.seq_skipped.iter().any(|hole| hole.seq == next) {
                        break;
                    }
                    stream.progress.consume_cursor = next;
                }
            }
            stream.progress.gap_seq = None;
            stream.progress.gap_started_at = None;
            let Some(run) = live_run.as_deref() else {
                stream::save_progress(&tx, &stream, now)?;
                continue;
            };
            if saved.meta.common.exec_run.as_deref().is_some_and(|pin| pin != run) {
                stream::retire(&tx, &mut stream, now)?;
                tx.commit().map_err(storage_error)?;
                return Err(MailboxError::StaleRun);
            }
            let body:(String,Vec<u8>,String)=tx.query_row("SELECT body_mime,body_bytes,delivery_meta FROM prx_mailbox_messages WHERE id=?1 AND ma_scope_id=?2",params![row_id,scope.scope_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(storage_error)?;
            validate_body(&body.0, &body.1)?;
            let pin: DeliveryMeta = decode(&body.2)?;
            if pin.to_pin_token.is_empty() || pin.pin_expires_at < 0 {
                return Err(MailboxError::CorruptState);
            }
            let message = PrxMessage {
                id: saved.message_id,
                idempotency: saved.idempotency.clone(),
                stream_id: Some(stream.wire_id),
                seq: saved.seq,
                ts: saved.meta.wire_ts,
                from: stream.from.clone(),
                to: stream.to.clone(),
                to_pin: RecipientPin {
                    incarnation: scope.incarnation,
                    generation: scope.generation,
                    token: pin.to_pin_token,
                },
                in_reply_to: saved.meta.common.in_reply_to,
                hop_count: saved.meta.common.hop_count,
                ancestry: saved.meta.common.ancestry.clone(),
                kind: MessageKind::Data,
                body: MessageBody {
                    mime: body.0,
                    bytes: body.1,
                },
                origin: Some(saved.meta.common.origin),
                auth: saved.meta.common.auth.clone(),
                exec: saved.meta.common.exec_run.clone().map(|run| ExecRef { run: Some(run) }),
                canonical_hash: None,
            };
            if crate::messaging::request_digest(&message) != saved.meta.common.request_digest {
                return Err(MailboxError::IdentityImmutable);
            }
            saved.state = DeliveryState::Delivered;
            saved.meta.delivered_at.get_or_insert(now);
            saved.meta.updated_at = now;
            tx.execute(
                "UPDATE prx_mailbox_receipts SET state=1,receipt_meta=?1 WHERE id=?2 AND state IN(0,1)",
                params![encode(&saved.meta)?, row_id],
            )
            .map_err(storage_error)?;
            tx.execute(
                "UPDATE prx_mailbox_messages SET state=1 WHERE id=?1 AND state IN(0,1)",
                [row_id],
            )
            .map_err(storage_error)?;
            stream.progress.deliver_cursor = seq;
            stream::save_progress(&tx, &stream, now)?;
            let lease = self.memory.mailbox_runtime.lease(scope.scope_id);
            tx.commit().map_err(storage_error)?;
            return Ok(Some(MailboxDelivery {
                row_id: *row_id,
                scope: scope.clone(),
                message,
                _lease: lease,
            }));
        }
        tx.commit().map_err(storage_error)?;
        Ok(None)
    }

    /// Queue owner quiesces and drops old leases before requesting recovery.
    pub fn recover_delivery(&self, scope: &MailboxScope, address: &SessionAddr) -> MailboxResult<()> {
        let now = self.clock.now();
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        check_scope(&tx, scope, address, None, true)?;
        if self.memory.mailbox_runtime.has_leases(scope.scope_id) {
            return Err(MailboxError::RoleBusy);
        }
        let mut query = tx
            .prepare("SELECT stream_id FROM prx_mailbox_seq WHERE recipient_ma_scope_id=?1 AND to_addr=?2")
            .map_err(storage_error)?;
        let ids = query
            .query_map(params![scope.scope_id.to_string(), encode(address)?], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        drop(query);
        for id in ids {
            let mut stream = stream::load(&tx, id)?;
            if !stream.retired {
                stream.progress.deliver_cursor = stream.progress.consume_cursor;
                stream::save_progress(&tx, &stream, now)?;
            }
        }
        tx.commit().map_err(storage_error)
    }
}

/// Kept by a live task/process owner until its model/cache/IPC has actually
/// stopped. Dropping an old run cannot close a later run of the same address.
#[must_use]
pub struct MailboxExecution {
    runtime: std::sync::Arc<super::runtime::Runtime>,
    scope: MailboxScope,
    address: SessionAddr,
    run: String,
    _lease: super::runtime::Lease,
}
impl Drop for MailboxExecution {
    fn drop(&mut self) {
        self.runtime.close_execution(&self.address, &self.scope, &self.run);
    }
}
impl SqliteMailbox {
    pub fn execution_guard(
        &self,
        scope: &MailboxScope,
        address: &SessionAddr,
        run: String,
    ) -> MailboxResult<MailboxExecution> {
        crate::messaging::mailbox::bounded(&run, 512)?;
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
        check_scope(&tx, scope, address, None, true)?;
        if address.instance != self.local_instance {
            return Err(MailboxError::Unauthorized);
        }
        let runtime = std::sync::Arc::clone(&self.memory.mailbox_runtime);
        runtime.set_execution(address.clone(), scope.clone(), Some(run.clone()));
        let lease = runtime.cache_lease(scope.scope_id);
        if let Err(error) = tx.commit() {
            runtime.close_execution(address, scope, &run);
            return Err(storage_error(error));
        }
        Ok(MailboxExecution {
            runtime,
            scope: scope.clone(),
            address: address.clone(),
            run,
            _lease: lease,
        })
    }
}
