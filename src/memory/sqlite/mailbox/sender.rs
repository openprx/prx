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
        if source.instance != store.local_instance || scope.owner_id != source.owner {
            return Err(MailboxError::Unauthorized);
        }
        store.validate_scope(&scope, &source)?;
        Ok(Self {
            store,
            source,
            scope,
            resolver,
            inherited,
        })
    }
}
#[async_trait]
impl MailboxSender for SqliteMailboxSender {
    fn source(&self) -> &SessionAddr {
        &self.source
    }
    async fn send(&self, target: SessionAddr, text: String, mime: String) -> MailboxResult<MailboxPublished> {
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
        if resolved.authority.source != self.source
            || resolved.authority.workspace_id != self.scope.workspace_id
            || resolved.authority.destination != target
        {
            return Err(MailboxError::Unauthorized);
        }
        let id = Uuid::new_v4();
        let message = PrxMessage {
            id,
            idempotency: IdempotencyKey::parse(&id.to_string()).map_err(|_| MailboxError::InvalidMetadata)?,
            stream_id: None,
            seq: 0,
            ts: self.store.clock.now(),
            from: self.source.clone(),
            to: target,
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
        self.store.publish_current(
            &resolved.authority,
            message,
            &self.scope,
            resolved.pin_expires_at,
            self.inherited.as_ref(),
        )
    }
}
