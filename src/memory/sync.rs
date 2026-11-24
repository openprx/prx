//! Header-only rf journal and consistent inventory contract.
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Resource and cursor failures exposed by the sync routes.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("sync storage unavailable")]
    Storage,
    #[error("invalid sync source or header identity")]
    InvalidSource,
    #[error("the snapshot page token expired")]
    SnapshotExpired,
    #[error("two snapshots are already active")]
    RateLimited,
    #[error("invalid sync request")]
    Invalid,
}

/// Journal bounds and one bounded batch of projected events.
pub struct JournalPage {
    pub epoch: String,
    pub head: i64,
    /// The highest pruned sequence. A cursor at this boundary is valid.
    pub floor: i64,
    pub events: Vec<Value>,
}

/// One page of the fixed inventory cut. `next` is an opaque owner-bound token.
#[derive(Serialize)]
pub struct SnapshotPage {
    pub epoch: String,
    pub cursor: String,
    pub snapshot_id: String,
    pub rows: Vec<Value>,
    pub next: Option<String>,
}

/// A candidate whose admitting process must be checked before interruption.
pub struct RestartCandidate {
    pub workspace: String,
    pub owner: String,
    pub run_id: String,
    pub lifetime: Option<String>,
}

/// Transactional SQLite sync surface; callers offload synchronous store work.
pub trait SyncStore: Send + Sync {
    fn session_metadata(&self, owner: &str, session: &str) -> anyhow::Result<Option<Value>>;
    fn live_scope(&self, owner: &str, event: &str) -> anyhow::Result<Option<Value>>;
    fn notify(&self) -> Arc<Notify>;
    fn connect(&self, owner: &str) -> Arc<CancellationToken>;
    fn disconnect(&self, owner: &str, connection: &Arc<CancellationToken>);
    fn journal(&self, owner: &str, after: i64, limit: usize) -> anyhow::Result<JournalPage>;
    fn snapshot(&self, owner: &str, token: Option<&str>, limit: usize) -> anyhow::Result<SnapshotPage>;
    fn configure(&self, workspace: &str, owner: &str) -> anyhow::Result<()>;
    fn refresh_config(&self, config: &crate::config::Config, revision: &str) -> anyhow::Result<()>;
    fn restart_candidates(&self, after: &str) -> anyhow::Result<Vec<RestartCandidate>>;
    /// Cancel the approvals that message-woken runs of a dead process left open; returns the runs swept.
    fn cancel_orphaned_woken_approvals(&self) -> anyhow::Result<usize>;
    fn shutdown_signal(&self) -> CancellationToken;
}
