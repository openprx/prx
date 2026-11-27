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
    /// Durable spawn sessions belonging to this authenticated owner/workspace.
    fn sessions(&self, workspace: &str, owner: &str) -> anyhow::Result<Vec<ChildSessionRecord>>;
    /// Full model-visible history, excluding internal lifecycle and reasoning rows.
    fn history(&self, record: &ChildSessionRecord) -> anyhow::Result<Vec<ChildTurn>>;
    /// The agent partition the child's own rows were written under: `Named`
    /// for a named agent (its name and incarnation), `Main` otherwise.
    fn partition(&self, record: &ChildSessionRecord) -> anyhow::Result<Option<crate::memory::AgentPartition>>;
    /// Replace only a child's model-visible context and selection. Transcript
    /// erasure remains the owner's separate history-erase operation.
    fn replace_context(&self, record: &ChildSessionRecord, turns: &[ChildTurn]) -> anyhow::Result<()>;
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

/// A logical child session. Stored data is never execution authority: every
/// continuation must be admitted from a fresh parent/operator source.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildSessionRecord {
    pub workspace: String,
    pub owner: String,
    pub session_key: String,
    pub parent_session: String,
    pub parent_run: String,
    pub agent: String,
    pub mode: String,
    pub provider: String,
    pub model: String,
    pub level: crate::security::AutonomyLevel,
    pub created_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: String,
    pub runs: Vec<String>,
    pub context_after: i64,
    pub context: Vec<ChildTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildTurn {
    pub role: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    /// The stored row this turn was read from; `None` for a turn kept in the
    /// record's context or built in memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}
