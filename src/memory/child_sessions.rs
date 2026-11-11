//! Transactional child-session records, separate from operator saved chats.
use crate::messaging::SessionAddr;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Supplied by the trusted parent registry, never inferred from child JSON.
#[derive(Clone)]
pub struct ChildSessionContext {
    pub workspace_id: String,
    pub addr: SessionAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChildSessionError {
    #[error("child_session_unbound_scope")]
    UnboundScope,
    #[error("child_session_identity_mismatch")]
    IdentityMismatch,
    #[error("child_session_invalid_record")]
    InvalidRecord,
    #[error("child_session_scope_refused")]
    ScopeRefused,
}

pub struct ChildMailboxTurn {
    pub event_id: Uuid,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

/// A history cache keeps its scope lease until the parent drops/drains it.
/// The lease itself is deliberately opaque to the chat/worker layers.
pub struct ScopedChildRecord {
    pub record: String,
    pub scope: crate::messaging::mailbox::MailboxScope,
    pub cursors: BTreeMap<i64, u64>,
    pub mailbox_turns: Vec<ChildMailboxTurn>,
    pub lease: Box<dyn Send + Sync>,
}

/// Opaque record schema belongs to chat; ownership and scope belong to storage.
pub trait ChildSessionStore: Send + Sync {
    fn load(&self, key: &str, context: &ChildSessionContext) -> anyhow::Result<Option<ScopedChildRecord>>;
    /// Invoke the callback once inside the writer transaction. Failure leaves
    /// the record unchanged. New writes always require a complete live scope.
    fn update(
        &self,
        key: &str,
        context: &ChildSessionContext,
        update: &mut dyn FnMut(Option<&str>) -> anyhow::Result<String>,
    ) -> anyhow::Result<ScopedChildRecord>;
}
