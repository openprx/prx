//! Agent incarnation registry (MA-01b1b-2): the backend contract for lazily
//! minting an incarnation at the first authorized resolve and for retiring it
//! when its agent is removed. Only a backend with a transactional store
//! implements it; every other backend reports `None` from
//! `Memory::agent_registry`.

use std::collections::{BTreeMap, BTreeSet};

/// Registry key of Main.
pub const MAIN_AGENT_KEY: &str = "m";

/// Main never depends on config: its row's config revision is the hash of
/// this constant.
pub const MAIN_REVISION_PREIMAGE: &str = "prx.ma.main.v1";

/// The live row of one (workspace, owner, key): its retiring row if there is
/// one, else its active row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveIncarnation {
    pub incarnation: String,
    /// Retired, purge pending: no turn and no new mint.
    pub retiring: bool,
}

/// Why a resolve minted nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRefusal {
    /// No such agent in the currently active generation.
    Unknown,
    /// Disabled, removed, retiring or not yet reconciled.
    Disabled,
}

/// One registry row a commit-time retire flipped to `retiring`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredIncarnation {
    pub workspace_id: String,
    pub owner_id: String,
    pub agent_key: String,
    pub incarnation: String,
}

pub trait AgentRegistry: Send + Sync {
    fn begin_history_erase(
        &self,
        _request: &super::agent_erasure::HistoryErase,
    ) -> anyhow::Result<super::agent_erasure::ErasePrepared> {
        Err(super::agent_erasure::ErasureRefusal::Unsupported.into())
    }

    fn finish_history_erase(&self, _work: &super::agent_erasure::EraseWork) -> anyhow::Result<serde_json::Value> {
        Err(super::agent_erasure::ErasureRefusal::Unsupported.into())
    }

    /// The active incarnation of (workspace, owner, key), minted if absent.
    /// A retiring row and `admit` are checked inside the write transaction, so
    /// a refusal writes nothing. `admit` carries the caller's own barriers.
    fn resolve_incarnation(
        &self,
        workspace_id: &str,
        owner_id: &str,
        agent_key: &str,
        config_revision: &str,
        admit: &dyn Fn() -> Result<(), AgentRefusal>,
    ) -> anyhow::Result<Result<String, AgentRefusal>>;

    /// The live row of (workspace, owner, key), if any; nothing is written.
    fn live_incarnation(
        &self,
        workspace_id: &str,
        owner_id: &str,
        agent_key: &str,
    ) -> anyhow::Result<Option<LiveIncarnation>>;

    /// Set `retiring` on every active row of `agent_keys` in one workspace, for
    /// every owner, in one transaction, and return the rows it flipped.
    fn retire_removed(
        &self,
        workspace_id: &str,
        agent_keys: &BTreeSet<String>,
    ) -> anyhow::Result<Vec<RetiredIncarnation>>;

    /// Set back to `active` exactly those of `rows` that are still `retiring`.
    fn reactivate(&self, rows: &[RetiredIncarnation]) -> anyhow::Result<()>;

    /// In one transaction: retire every active named row of the workspace whose
    /// key is absent from `revisions`, and refresh the config revision of the
    /// others.
    fn reconcile(&self, workspace_id: &str, revisions: &BTreeMap<String, String>) -> anyhow::Result<()>;
}
