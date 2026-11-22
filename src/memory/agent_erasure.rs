//! Content-free contracts for resumable agent history erasure and purge.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, thiserror::Error)]
pub enum ErasureRefusal {
    #[error("idempotency_conflict")]
    IdempotencyConflict,
    #[error("incarnation_mismatch")]
    IncarnationMismatch(String),
    #[error("history_pending")]
    Pending,
    #[error("agent_not_retired")]
    NotRetired,
    #[error("agent_unknown")]
    Unknown,
    #[error("agent_backend_unsupported")]
    Unsupported,
    #[error("erase manifest does not cover stamped store {0}")]
    UnknownStore(String),
}

#[derive(Clone)]
pub struct HistoryErase {
    pub configured: bool,
    pub workspace: String,
    pub owner: String,
    pub session: String,
    pub agent: String,
    pub incarnation: String,
    pub key: String,
    pub forget_writes: bool,
}

/// A durable, payload-free work description. Kept across retries and restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EraseWork {
    pub erase_id: String,
    pub scope_id: String,
    pub workspace: String,
    pub owner: String,
    pub session: String,
    pub agent: String,
    pub incarnation: String,
    pub generation: i64,
    pub forget_writes: bool,
    pub scopes: Vec<String>,
    pub runs: Vec<String>,
    pub jobs: Vec<String>,
}

pub enum ErasePrepared {
    Replay(serde_json::Value),
    Work(Box<EraseWork>),
}
