//! Dormant IPC inbox. Data is ordinary model input; it never enters the
//! operator steering/approval parser. The parent alone owns the durable ledger.
use super::protocol::WorkerOutputFrame;
use crate::messaging::mailbox::{MailboxError, MailboxResult, validate_address, validate_body};
use crate::messaging::{DeliveryState, MessageKind, PrxMessage, SessionAddr};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

type LedgerKey = (String, String);
struct InboxReceipt {
    id: uuid::Uuid,
    digest: String,
    state: DeliveryState,
}
type InboxLedger = Arc<parking_lot::Mutex<HashMap<LedgerKey, InboxReceipt>>>;

pub struct WorkerMailboxSender {
    address: SessionAddr,
    run: String,
    sender: mpsc::Sender<PrxMessage>,
    seen: InboxLedger,
}
pub struct WorkerMailboxInbox {
    receiver: mpsc::Receiver<PrxMessage>,
    seen: InboxLedger,
}

pub fn worker_mailbox(address: SessionAddr, run: String) -> MailboxResult<(WorkerMailboxSender, WorkerMailboxInbox)> {
    validate_address(&address)?;
    crate::messaging::mailbox::bounded(&run, 512)?;
    let (sender, receiver) = mpsc::channel(
        usize::try_from(crate::messaging::mailbox::MAX_QUEUE).map_err(|_| MailboxError::InvalidMetadata)?,
    );
    let seen = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    Ok((
        WorkerMailboxSender {
            address,
            run,
            sender,
            seen: Arc::clone(&seen),
        },
        WorkerMailboxInbox { receiver, seen },
    ))
}
impl WorkerMailboxSender {
    /// Called only on the pipe owned by the authenticated spawning parent.
    pub fn enqueue(&mut self, message: PrxMessage) -> MailboxResult<WorkerOutputFrame> {
        if message.kind != MessageKind::Data || message.to != self.address {
            return Err(MailboxError::Unauthorized);
        }
        validate_address(&message.from)?;
        validate_body(&message.body.mime, &message.body.bytes)?;
        if message.stream_id.is_none() {
            return Err(MailboxError::StreamIdRequired);
        }
        if message.seq == 0 || message.to_pin.generation < 1 || message.to_pin.token.is_empty() {
            return Err(MailboxError::InvalidPin);
        }
        if message
            .exec
            .as_ref()
            .and_then(|exec| exec.run.as_deref())
            .is_some_and(|run| run != self.run)
        {
            return Err(MailboxError::StaleRun);
        }
        if message.ancestry.len() != usize::from(message.hop_count)
            || message.hop_count > 8
            || message.ancestry.last().copied() != message.in_reply_to
            || message.ancestry.contains(&message.id)
        {
            return Err(MailboxError::InvalidMetadata);
        }
        let unique: std::collections::HashSet<_> = message.ancestry.iter().collect();
        if unique.len() != message.ancestry.len()
            || message.origin.is_none()
            || (message.origin == Some(crate::messaging::MessageOrigin::RemotePeer)) != message.auth.is_some()
        {
            return Err(MailboxError::InvalidMetadata);
        }
        let identity = (message.from.instance.clone(), message.idempotency.as_str().to_owned());
        let id = message.id;
        let digest = crate::messaging::request_digest(&message);
        {
            let seen = self.seen.lock();
            if let Some(saved) = seen.get(&identity) {
                if saved.id != id || saved.digest != digest {
                    return Err(MailboxError::IdempotencyConflict);
                }
                return Ok(WorkerOutputFrame::Ack {
                    message_id: id,
                    state: saved.state,
                });
            }
            if i64::try_from(seen.len()).map_err(|_| MailboxError::CorruptState)? >= crate::messaging::mailbox::MAX_ROWS
            {
                return Err(MailboxError::Quota("rows"));
            }
        }
        // Reserve before enqueue: a fast async consumer can mark Consumed
        // before blocking_send returns. Never overwrite that state afterwards.
        self.seen.lock().insert(
            identity.clone(),
            InboxReceipt {
                id,
                digest,
                state: DeliveryState::Delivered,
            },
        );
        if self.sender.blocking_send(message).is_err() {
            self.seen.lock().remove(&identity);
            return Err(MailboxError::StaleRun);
        }
        let state = self
            .seen
            .lock()
            .get(&identity)
            .map_or(DeliveryState::Delivered, |entry| entry.state);
        Ok(WorkerOutputFrame::Ack { message_id: id, state })
    }
}
impl WorkerMailboxInbox {
    pub fn try_next(&mut self) -> Option<PrxMessage> {
        self.receiver.try_recv().ok()
    }
    pub(crate) fn consumed(&self, message: &PrxMessage) -> MailboxResult<WorkerOutputFrame> {
        let mut seen = self.seen.lock();
        let entry = seen
            .get_mut(&(message.from.instance.clone(), message.idempotency.as_str().to_owned()))
            .ok_or(MailboxError::CorruptState)?;
        if entry.id != message.id || entry.digest != crate::messaging::request_digest(message) {
            return Err(MailboxError::IdempotencyConflict);
        }
        entry.state = DeliveryState::Consumed;
        Ok(WorkerOutputFrame::Ack {
            message_id: message.id,
            state: DeliveryState::Consumed,
        })
    }
}

/// Match the parent's transcript projection without interpreting slash commands.
pub fn user_text(message: &PrxMessage) -> MailboxResult<String> {
    validate_body(&message.body.mime, &message.body.bytes)?;
    if matches!(message.body.mime.as_str(), "text/plain" | "text/markdown")
        && let Ok(text) = std::str::from_utf8(&message.body.bytes)
    {
        return Ok(text.to_owned());
    }
    use base64::Engine as _;
    Ok(format!(
        "[mailbox MIME {} base64]\n{}",
        serde_json::to_string(&message.body.mime).map_err(|_| MailboxError::InvalidMetadata)?,
        base64::engine::general_purpose::STANDARD.encode(&message.body.bytes)
    ))
}
