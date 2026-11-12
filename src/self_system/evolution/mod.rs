//! Evolution subsystem record/storage/config/trace primitives.

pub mod analyzer;
pub mod commit;
pub mod config;
pub mod cycle_types;
pub mod engine;
pub mod gate;
pub mod judge;
pub mod memory_safety;
pub mod pipeline;
pub mod prompt_evolution;
pub mod proposal;
pub mod record;
pub mod rollback;
pub mod safety_utils;
pub mod scheduler;
pub mod storage;
pub mod trace;

pub use analyzer::{CandidatePriority, EvolutionAnalyzer, EvolutionCandidate, TrendAnalysis};
pub use config::{
    EvolutionConfig, EvolutionMode, EvolutionRetentionConfig, EvolutionRuntimeConfig, SharedEvolutionConfig,
    new_shared_evolution_config,
};
pub use cycle_types::{
    ChangeOperation, ChangeTarget, CycleOutcome, EvolutionCycle, EvolutionProposal, EvolutionSignals,
    EvolutionValidation, FitnessTrend, RiskLevel, ValidationStatus,
};
pub use engine::{CycleResult, EngineCycleInput, EvolutionEngine, FileChange, PendingChange, run_engine_cycle};
pub use memory_safety::{MemorySafetyFilter, SafetyIssueKind, SourceMetadata};
pub use pipeline::{CommitDecision, EvolutionPipeline, EvolutionTrigger, JudgeOutcome, PipelineRunReport};
pub use prompt_evolution::PromptEvolutionEngine;
pub use proposal::{
    EvolutionProposalDraft, EvolutionTargetResource, JudgeVerdict, ProposalFilter, ProposalStatusUpdate,
    ProposedChange, RollbackAnchor,
};
pub use record::{
    Actor, ChangeType, DataBasis, DecisionLog, DecisionType, EvolutionLayer, EvolutionLog, EvolutionResult,
    MemoryAccessLog, MemoryAction, Outcome, TaskType,
};
pub use rollback::{CircuitBreaker, CircuitBreakerState, RollbackManager};
pub use scheduler::EvolutionScheduler;
pub use storage::{AsyncJsonlWriter, JsonlRetentionPolicy, JsonlStoragePaths};
pub use trace::{TraceContext, current_trace, with_trace};
