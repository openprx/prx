//! Restart recovery: adopted messages whose waking turn never started.
use rusqlite::{Transaction, TransactionBehavior, params};

use super::{
    SqliteMailbox,
    deliver::wire_message,
    model::{DeliveryMeta, decode, encode},
    pin::check_scope,
    receipt, storage_error, stream,
};
use crate::messaging::mailbox::{MailboxError, MailboxReceipt, MailboxResult, MailboxScope};
use crate::messaging::{DeliveryState, PrxMessage, SessionAddr};

/// A restart can only strand this many queued wakes (the ready queue cap).
const MAX_RECOVERED: i64 = 64;

impl SqliteMailbox {
    /// Adopted messages of `address` with no continuation input yet, oldest
    /// first. The adoption commit and the wake are separate steps: a message
    /// is "adopted, not woken" while no `message_events` row names its adoption
    /// row as `causation_event_id`. The woken turn writes that row first, so a
    /// turn that started (even if it never finished) is never replayed.
    pub(crate) fn unwoken_adopted(
        &self,
        scope: &MailboxScope,
        address: &SessionAddr,
    ) -> MailboxResult<Vec<(PrxMessage, MailboxReceipt)>> {
        let writer = self.memory.pool.write();
        let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Deferred).map_err(storage_error)?;
        check_scope(&tx, scope, address, None, true)?;
        let ids = {
            let mut query = tx
                .prepare(
                    "SELECT m.id FROM prx_mailbox_messages m \
                     JOIN prx_mailbox_seq s ON s.stream_id=m.stream_id \
                     WHERE m.ma_scope_id=?1 AND s.to_addr=?2 AND m.state=2 \
                     ORDER BY m.id DESC LIMIT ?3",
                )
                .map_err(storage_error)?;
            query
                .query_map(params![scope.scope_id.to_string(), encode(address)?, MAX_RECOVERED], |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(storage_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage_error)?
        };
        let mut found = Vec::new();
        for id in ids {
            let saved = receipt::load(&tx, id)?;
            let Some(event) = saved.meta.adopted_message_event_id else {
                continue;
            };
            if saved.state != DeliveryState::Consumed || saved.meta.body_erased_at.is_some() || saved.scope != *scope {
                continue;
            }
            let woken: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM message_events WHERE workspace_id=?1 AND causation_event_id=?2)",
                    params![scope.workspace_id, event.to_string()],
                    |r| r.get(0),
                )
                .map_err(storage_error)?;
            if woken {
                continue;
            }
            let body: (String, Vec<u8>, String) = tx
                .query_row(
                    "SELECT body_mime,body_bytes,delivery_meta FROM prx_mailbox_messages WHERE id=?1 AND ma_scope_id=?2",
                    params![id, scope.scope_id.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(storage_error)?;
            let pin: DeliveryMeta = decode(&body.2)?;
            let stream = stream::load(&tx, saved.stream_id)?;
            let message = wire_message(&saved, &stream, scope, body.0, body.1, pin.to_pin_token);
            if crate::messaging::request_digest(&message) != saved.meta.common.request_digest {
                return Err(MailboxError::IdentityImmutable);
            }
            found.push((message, saved.result(&tx)?));
        }
        found.reverse();
        Ok(found)
    }
}
