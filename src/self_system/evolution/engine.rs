use crate::self_system::evolution::analyzer::EvolutionCandidate;
use crate::self_system::evolution::cycle_types::{EvolutionCycle, EvolutionProposal, FitnessTrend};
use crate::self_system::evolution::record::{EvolutionLayer, EvolutionLog};
use crate::self_system::evolution::rollback::RollbackManager;
use crate::self_system::evolution::safety_utils::atomic_write;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A change an engine proposes but has not written (D-046). Engines never
/// write their target in `run_cycle`: the pipeline judges the proposal and
/// applies this only after an accepted verdict, in a task the daemon waits
/// for on shutdown.
#[async_trait]
pub trait PendingChange: Send + Sync + std::fmt::Debug {
    /// Write the change. The pipeline calls it at most once.
    async fn apply(&self) -> Result<()>;
}

/// The common pending change: snapshot the target for a later manual
/// rollback, then replace it atomically.
pub struct FileChange {
    workspace_root: PathBuf,
    target: PathBuf,
    content: String,
    backup: RollbackManager,
}

impl FileChange {
    pub const fn new(workspace_root: PathBuf, target: PathBuf, content: String, backup: RollbackManager) -> Self {
        Self {
            workspace_root,
            target,
            content,
            backup,
        }
    }
}

impl std::fmt::Debug for FileChange {
    // The content can be a whole prompt; it is never logged (FIX-P0-05).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileChange")
            .field("target", &self.target)
            .field("content_bytes", &self.content.len())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl PendingChange for FileChange {
    async fn apply(&self) -> Result<()> {
        self.backup.backup_current_version().await?;
        if let Some(parent) = self.target.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        atomic_write(&self.workspace_root, &self.target, self.content.as_bytes()).await
    }
}

/// Engine input payload shared by layer-specific evolution executors.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EngineCycleInput {
    pub cycle_id: String,
    pub analyzer_candidates: Vec<EvolutionCandidate>,
    /// Real closed-window fitness evidence. `None` means no publishable fitness
    /// snapshot exists; engines must not invent before/after scores.
    pub fitness_trend: Option<FitnessTrend>,
}

/// Ephemeral review projection. Debug output exposes only its size.
#[derive(Clone)]
pub struct ReviewData(String);
impl ReviewData {
    pub const fn new(content: String) -> Self {
        Self(content)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for ReviewData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewData").field("bytes", &self.0.len()).finish()
    }
}

/// Normalized output from a single evolution engine cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleResult {
    /// Ephemeral bounded input for model review; never persisted in cycle JSON.
    #[serde(skip)]
    pub review_data: Option<ReviewData>,
    pub layer: EvolutionLayer,
    pub proposal: Option<EvolutionProposal>,
    pub cycle: EvolutionCycle,
    pub evolution_log: Option<EvolutionLog>,
    pub needs_human_approval: bool,
    pub shadow_mode: bool,
    /// The change to write if the verdict accepts it; `None` when the cycle
    /// proposes nothing to write (DraftOnly, Shadow, a refused gate).
    #[serde(skip)]
    pub pending: Option<Arc<dyn PendingChange>>,
}

/// Unified trait for evolution layer executors.
#[async_trait]
pub trait EvolutionEngine: Send + Sync {
    fn name(&self) -> &'static str;
    fn layer(&self) -> EvolutionLayer;
    /// Propose one cycle's change. Never writes the layer's target: the
    /// change to write goes in [`CycleResult::pending`] (D-046).
    async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult>;

    /// Run with the owner's cancellation token when the engine supports it.
    async fn run_cycle_with_cancellation(
        &mut self,
        input: EngineCycleInput,
        cancel: &CancellationToken,
    ) -> Result<CycleResult> {
        anyhow::ensure!(!cancel.is_cancelled(), "evolution cycle cancelled before start");
        self.run_cycle(input).await
    }
}

/// Execute a layer engine and normalize into the cycle result contract.
pub async fn run_engine_cycle(
    engine: &mut dyn EvolutionEngine,
    cycle_id: impl Into<String>,
    analyzer_candidates: Vec<EvolutionCandidate>,
    fitness_trend: Option<FitnessTrend>,
) -> Result<CycleResult> {
    engine
        .run_cycle(EngineCycleInput {
            cycle_id: cycle_id.into(),
            analyzer_candidates,
            fitness_trend,
        })
        .await
}
