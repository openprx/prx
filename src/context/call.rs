//! The per-turn call context.
//!
//! One immutable value, built where the turn is opened and carried to every
//! tool call of that turn. A tool takes the level, the workspace and the
//! owner from it instead of re-reading configuration, so a named agent or a
//! child at an effective level behaves at that level in every tool.
//!
//! First slice: identity, authority, place, model, the catalog generation and
//! the memory partition. The budget handle, the published surface and the
//! control handles (audit sink, log sink, hook dispatcher) are not carried
//! yet; no consumer reads them.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::memory::PartitionKey;
use crate::security::AutonomyLevel;
use crate::tools::capability::contract::CatalogGeneration;
use crate::tools::capability::turn_state::TurnSurfaceKind;

tokio::task_local! {
    static CURRENT: Option<Arc<TurnCallContext>>;
}

/// Why the call runs at its level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LevelReason {
    /// A child turn runs at the level its sealed scope carries: its parent's
    /// level, or a named agent's own level.
    ChildScope,
    /// The turn runs at the level of the service that hosts it.
    ServicePolicy,
}

/// Who the call acts for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallIdentity {
    pub owner_id: Arc<str>,
    pub session_key: Arc<str>,
    pub run_id: Arc<str>,
    /// The named agent running the turn; `None` for Main.
    pub agent_id: Option<Arc<str>>,
    pub surface: TurnSurfaceKind,
    /// The channel the turn arrived on, as the policy knows it.
    pub channel: Arc<str>,
    /// The task or job id of a scheduled or task origin.
    pub task_id: Option<Arc<str>>,
}

/// The effective autonomy level of the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallAuthority {
    pub level: AutonomyLevel,
    pub reason: LevelReason,
}

/// Where the call runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallPlace {
    pub workspace_root: PathBuf,
    pub working_dir: PathBuf,
}

/// The model serving the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallModel {
    pub provider: Arc<str>,
    pub model: Arc<str>,
    /// The effective context window, in tokens.
    pub window_tokens: usize,
    pub can_call_tools: bool,
}

/// The immutable per-turn call context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnCallContext {
    pub identity: CallIdentity,
    pub authority: CallAuthority,
    pub place: CallPlace,
    pub model: CallModel,
    pub catalog_generation: CatalogGeneration,
    /// The memory partition recall and memory tools use.
    pub memory_partition: Option<PartitionKey>,
}

/// Run `future` with `call` as the current call context.
pub(crate) async fn scope<T>(call: Option<Arc<TurnCallContext>>, future: impl Future<Output = T>) -> T {
    CURRENT.scope(call, future).await
}

/// The level of the current call, when a call context is set.
pub(crate) fn level() -> Option<AutonomyLevel> {
    CURRENT
        .try_with(|call| call.as_ref().map(|call| call.authority.level))
        .ok()
        .flatten()
}

/// The working directory of the current call, when a call context is set.
pub(crate) fn working_dir() -> Option<PathBuf> {
    CURRENT
        .try_with(|call| call.as_ref().map(|call| call.place.working_dir.clone()))
        .ok()
        .flatten()
}
