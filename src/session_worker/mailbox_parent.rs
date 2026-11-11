//! Explicit parent-side IPC/store binding. No production factory registers it.
//! The owning controller keeps this object through worker/pipe join, and its
//! erase drainer cancels and joins that owner before dropping the scope lease.
use super::protocol::{WorkerControlFrame, WorkerOutputFrame};
use crate::memory::sqlite::mailbox::{MailboxDelivery, MailboxExecution, SqliteMailbox};
use crate::messaging::mailbox::{
    MailboxAuthority, MailboxError, MailboxReceipt, MailboxResult, MailboxScope, MailboxSendState,
};
use crate::messaging::{DeliveryState, InheritedProvenance, SessionAddr};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub struct WorkerMailboxParent {
    store: SqliteMailbox,
    worker: SessionAddr,
    worker_scope: MailboxScope,
    parent: SessionAddr,
    parent_scope: MailboxScope,
    incoming: MailboxAuthority,
    inherited: Option<InheritedProvenance>,
    pending: HashMap<Uuid, MailboxDelivery>,
    completed: HashSet<Uuid>,
    _execution: MailboxExecution,
}
impl WorkerMailboxParent {
    pub fn new(
        store: SqliteMailbox,
        worker: SessionAddr,
        worker_scope: MailboxScope,
        parent: SessionAddr,
        parent_scope: MailboxScope,
        run: String,
        inherited: Option<InheritedProvenance>,
    ) -> MailboxResult<Self> {
        if worker_scope.workspace_id != parent_scope.workspace_id {
            return Err(MailboxError::Unauthorized);
        }
        store.validate_scope(&parent_scope, &parent)?;
        let mut incoming = MailboxAuthority::local(worker_scope.workspace_id.clone(), worker.clone(), parent.clone())?;
        if let Some(inherited) = &inherited {
            incoming = incoming.inherit_remote(inherited);
        }
        let execution = store.execution_guard(&worker_scope, &worker, run)?;
        Ok(Self {
            store,
            worker,
            worker_scope,
            parent,
            parent_scope,
            incoming,
            inherited,
            pending: HashMap::new(),
            completed: HashSet::new(),
            _execution: execution,
        })
    }
    /// Hold the delivery lease through the actual queue and wire handoff.
    pub fn next_delivery(&mut self) -> MailboxResult<Option<WorkerControlFrame>> {
        let Some(delivery) = self.store.deliver(&self.worker_scope, &self.worker)? else {
            return Ok(None);
        };
        let id = delivery.message().id;
        if self.pending.contains_key(&id) {
            return Err(MailboxError::CorruptState);
        }
        let frame = WorkerControlFrame::Message {
            envelope: Box::new(delivery.message().clone()),
        };
        self.pending.insert(id, delivery);
        Ok(Some(frame))
    }
    /// A worker's asserted `from` never supplies identity. The spawning
    /// parent's sealed address and inherited context own publishing authority.
    pub fn receive(&mut self, frame: WorkerOutputFrame) -> MailboxResult<Option<MailboxReceipt>> {
        match frame {
            WorkerOutputFrame::Message { envelope } => {
                let mut message = *envelope;
                // For this local pipe the store verifies the original pin and
                // persists its authenticated expiry. It never fills a pin.
                let published = self.store.publish(
                    &self.incoming,
                    message.clone(),
                    &self.worker_scope,
                    i64::MAX,
                    self.inherited.as_ref(),
                )?;
                if published.state == MailboxSendState::Failed {
                    return Err(MailboxError::Fenced);
                }
                message.seq = published.seq;
                if let Some(inherited) = &self.inherited {
                    crate::messaging::stamp_outbound(&inherited.chain, &mut message);
                }
                let accepted = self.store.accept(&self.incoming, message)?;
                self.store
                    .acknowledge(&self.worker_scope, &self.parent, published.message_id, accepted.state)?;
                Ok(Some(accepted))
            }
            WorkerOutputFrame::Ack { message_id, state } => {
                if self.completed.contains(&message_id) {
                    return Ok(None);
                }
                let delivery = self.pending.get(&message_id).ok_or(MailboxError::InvalidMetadata)?;
                if state == DeliveryState::Consumed {
                    let adopted = self.store.adopt(delivery)?;
                    self.inherited = adopted.remote.clone().map(|remote| InheritedProvenance {
                        remote,
                        chain: adopted.chain.clone(),
                    });
                    self.incoming = MailboxAuthority::local(
                        self.worker_scope.workspace_id.clone(),
                        self.worker.clone(),
                        self.parent.clone(),
                    )?;
                    if let Some(inherited) = &self.inherited {
                        self.incoming = self.incoming.clone().inherit_remote(inherited);
                    }
                    self.store
                        .acknowledge(&self.parent_scope, &self.worker, message_id, adopted.state)?;
                    self.pending.remove(&message_id);
                    self.completed.insert(message_id);
                    if i64::try_from(self.completed.len()).map_err(|_| MailboxError::CorruptState)?
                        > crate::messaging::mailbox::MAX_ROWS
                    {
                        return Err(MailboxError::Quota("rows"));
                    }
                } else {
                    self.store
                        .acknowledge(&self.parent_scope, &self.worker, message_id, state)?;
                }
                Ok(None)
            }
            WorkerOutputFrame::BrokerCall { .. } | WorkerOutputFrame::BrokerCancel { .. } => {
                Err(MailboxError::Unauthorized)
            }
        }
    }
}
