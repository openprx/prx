//! Evolution subsystem record/storage/config/trace primitives.

pub mod analyzer;
pub mod config;
pub mod cycle_types;
pub mod engine;
pub mod gate;
pub mod judge;
pub mod memory_compressor;
pub mod memory_evolution;
pub mod memory_retrieval;
pub mod memory_safety;
pub mod pipeline;
pub mod prompt_evolution;
pub mod record;
pub mod rollback;
pub mod safety_utils;
pub mod scheduler;
pub mod storage;
pub mod strategy_evolution;
pub mod trace;

#[allow(unused_imports)]
pub use analyzer::{CandidatePriority, EvolutionAnalyzer, EvolutionCandidate, TrendAnalysis};
pub use config::{
    EvolutionConfig, EvolutionMode, EvolutionRetentionConfig, EvolutionRuntimeConfig, SharedEvolutionConfig,
    new_shared_evolution_config,
};
pub use cycle_types::{
    ChangeOperation, ChangeTarget, CycleOutcome, EvolutionCycle, EvolutionProposal, EvolutionSignals,
    EvolutionValidation, FitnessTrend, RiskLevel, ValidationStatus,
};
#[allow(unused_imports)]
pub use engine::{CycleResult, EngineCycleInput, EvolutionEngine, run_engine_cycle};
pub use memory_evolution::MemoryEvolutionEngine;
#[allow(unused_imports)]
pub use memory_safety::{MemorySafetyFilter, SafetyIssueKind, SourceMetadata};
pub use pipeline::{EvolutionPipeline, EvolutionTrigger, PipelineRunReport};
pub use prompt_evolution::PromptEvolutionEngine;
#[allow(unused_imports)]
pub use record::{
    Actor, ChangeType, DataBasis, DecisionLog, DecisionType, EvolutionLayer, EvolutionLog, EvolutionResult,
    MemoryAccessLog, MemoryAction, Outcome, TaskType,
};
#[allow(unused_imports)]
pub use rollback::{CircuitBreaker, CircuitBreakerState, RollbackManager};
pub use scheduler::EvolutionScheduler;
pub use storage::{AsyncJsonlWriter, JsonlRetentionPolicy, JsonlStoragePaths};
pub use strategy_evolution::StrategyEvolutionEngine;
#[allow(unused_imports)]
pub use trace::{TraceContext, current_trace, with_trace};
