use crate::self_system::evolution::analyzer::EvolutionCandidate;
use crate::self_system::evolution::record::{EvolutionLayer, EvolutionLog};
use crate::self_system::orchestrator::{EvolutionCycle, EvolutionProposal};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Engine input payload shared by layer-specific evolution executors.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EngineCycleInput {
    pub cycle_id: String,
    pub analyzer_candidates: Vec<EvolutionCandidate>,
}

/// Normalized output from a single evolution engine cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleResult {
    pub layer: EvolutionLayer,
    pub proposal: Option<EvolutionProposal>,
    pub cycle: EvolutionCycle,
    pub evolution_log: Option<EvolutionLog>,
    pub needs_human_approval: bool,
    pub shadow_mode: bool,
}

/// Unified trait for evolution layer executors.
#[async_trait]
pub trait EvolutionEngine: Send + Sync {
    fn name(&self) -> &'static str;
    fn layer(&self) -> EvolutionLayer;
    async fn run_cycle(&mut self, input: EngineCycleInput) -> Result<CycleResult>;
}
