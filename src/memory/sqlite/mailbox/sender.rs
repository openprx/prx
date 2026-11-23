//! Explicit dormant adapter used by tools and slash commands.
use super::{SqliteMailbox, pin::check_scope, storage_error};
use crate::messaging::mailbox::{MailboxError, MailboxPublished, MailboxResult, MailboxScope};
use crate::messaging::send::{MailboxResolver, MailboxSender};
use crate::messaging::{IdempotencyKey, InheritedProvenance, MessageBody, MessageKind, PrxMessage, SessionAddr};
use async_trait::async_trait;
use rusqlite::{Transaction, TransactionBehavior};
use std::sync::Arc;
use uuid::Uuid;

// Payload drops before its scope lease on every resolver/error/cancel path.
struct PendingSend {
    text: String,
    _lease: super::runtime::Lease,
}

pub struct SqliteMailboxSender {
    chain: Option<crate::messaging::CausalChain>,
    store: SqliteMailbox,
    source: SessionAddr,
    scope: MailboxScope,
    resolver: Arc<dyn MailboxResolver>,
    inherited: Option<InheritedProvenance>,
}
impl SqliteMailboxSender {
    pub fn new(
        store: SqliteMailbox,
        source: SessionAddr,
        scope: MailboxScope,
        resolver: Arc<dyn MailboxResolver>,
        inherited: Option<InheritedProvenance>,
    ) -> MailboxResult<Self> {
        crate::messaging::mailbox::validate_address(&source)?;
        if source.instance != store.local_instance {
            return Err(crate::messaging::mailbox::unauthorized(
                crate::messaging::mailbox::UnauthorizedReason::LocalInstanceMismatch,
            ));
        }
        if scope.owner_id != source.owner {
            return Err(crate::messaging::mailbox::unauthorized(
                crate::messaging::mailbox::UnauthorizedReason::SenderOwnerMismatch,
            ));
        }
        store.validate_scope(&scope, &source)?;
        Ok(Self {
            chain: None,
            store,
            source,
            scope,
            resolver,
            inherited,
        })
    }
    pub fn with_chain(mut self, chain: Option<crate::messaging::CausalChain>) -> Self {
        self.chain = chain;
        self
    }
}
#[async_trait]
impl MailboxSender for SqliteMailboxSender {
    fn source(&self) -> &SessionAddr {
        &self.source
    }
    async fn send(&self, target: SessionAddr, text: String, mime: String) -> MailboxResult<MailboxPublished> {
        self.send_with_id(target, text, mime, Uuid::new_v4()).await
    }
}
impl SqliteMailboxSender {
    pub(crate) async fn send_with_id(
        &self,
        target: SessionAddr,
        text: String,
        mime: String,
        id: Uuid,
    ) -> MailboxResult<MailboxPublished> {
        crate::messaging::mailbox::validate_body(&mime, text.as_bytes())?;
        if text.is_empty() {
            return Err(MailboxError::InvalidMetadata);
        }
        let pending = {
            let writer = self.store.memory.pool.write();
            let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).map_err(storage_error)?;
            check_scope(&tx, &self.scope, &self.source, None, true)?;
            let lease = self.store.memory.mailbox_runtime.cache_lease(self.scope.scope_id);
            tx.commit().map_err(storage_error)?;
            PendingSend { text, _lease: lease }
        };
        let resolved = self.resolver.resolve(&target).await?;
        if resolved.authority.source != self.source {
            return Err(crate::messaging::mailbox::unauthorized(
                crate::messaging::mailbox::UnauthorizedReason::SenderIdentityMismatch,
            ));
        }
        if resolved.authority.workspace_id != self.scope.workspace_id {
            return Err(crate::messaging::mailbox::unauthorized(
                crate::messaging::mailbox::UnauthorizedReason::SenderWorkspaceMismatch,
            ));
        }
        let destination = &resolved.authority.destination;
        // Only a verified remote pin response may supply the receiving owner.
        let remote_owner = target.instance != self.source.instance
            && destination.instance == target.instance
            && destination.session == target.session
            && destination.agent == target.agent;
        if destination != &target && !remote_owner {
            return Err(crate::messaging::mailbox::unauthorized(
                crate::messaging::mailbox::UnauthorizedReason::DestinationMismatch,
            ));
        }
        let mut message = PrxMessage {
            id,
            idempotency: IdempotencyKey::parse(&id.to_string()).map_err(|_| MailboxError::InvalidMetadata)?,
            stream_id: None,
            seq: 0,
            ts: self.store.clock.now(),
            from: self.source.clone(),
            to: resolved.authority.destination.clone(),
            to_pin: resolved.pin,
            in_reply_to: None,
            hop_count: 0,
            ancestry: Vec::new(),
            kind: MessageKind::Data,
            body: MessageBody {
                mime,
                bytes: pending.text.into_bytes(),
            },
            origin: None,
            auth: None,
            exec: None,
            canonical_hash: None,
        };
        if let Some(chain) = &self.chain {
            crate::messaging::stamp_outbound(chain, &mut message);
        }
        self.store.publish_current(
            &resolved.authority,
            message,
            &self.scope,
            resolved.pin_expires_at,
            self.inherited.as_ref(),
        )
    }
}
