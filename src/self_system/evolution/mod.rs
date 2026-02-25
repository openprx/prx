//! Evolution subsystem record/storage/config/trace primitives.

pub mod analyzer;
pub mod annotation;
pub mod anti_pattern;
pub mod config;
pub mod engine;
pub mod gate;
pub mod index;
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
pub mod test_suite;
pub mod trace;

#[allow(unused_imports)]
pub use analyzer::{
    CandidatePriority, ConfigEfficiencyIssue, DailyDigest, EvolutionAnalyzer, EvolutionCandidate,
    MetricShift, NoiseMemoryPattern, TaskTypeDigest, TaskTypeWeakness, TrendAnalysis,
    UserCorrectionCluster,
};
#[allow(unused_imports)]
pub use annotation::{AnnotationPipeline, AnnotationReport, AnnotationUpdate};
#[allow(unused_imports)]
pub use anti_pattern::{AntiPattern, AntiPatternStore};
#[allow(unused_imports)]
pub use config::{
    new_shared_evolution_config, EvolutionRetrievalConfig, RetrievalScoreWeights,
    SharedEvolutionConfig,
};
pub use config::{
    DataThresholds, EvolutionConfig, EvolutionGateConfig, EvolutionMode, EvolutionRetentionConfig,
    EvolutionRuntimeConfig, EvolutionRuntimeConfigManager, MemoryEvolutionConfig,
    PromptEvolutionConfig, RetrievalFusionWeights, RollbackConfig, StrategyEvolutionConfig,
};
pub use engine::{CycleResult, EngineCycleInput, EvolutionEngine};
#[allow(unused_imports)]
pub use gate::{EvolutionGate, GateMetrics, GateRejection, GateResult};
#[allow(unused_imports)]
pub use index::{ImportSummary, JsonlToSqliteIndexer, SearchHit};
#[allow(unused_imports)]
pub use judge::{
    JudgeConfig, JudgeDriftAlert, JudgeEngine, JudgeHealthMonitor, JudgeHealthReport, JudgeResult,
    JudgeScoringModel, MockJudgeModel, StructuredScores,
};
#[allow(unused_imports)]
pub use memory_compressor::{
    CompressionLimits, CompressionResult, DefaultSimilarityDetector, FidelityReport,
    MemoryCompressor, SimilarityDetector,
};
pub use memory_evolution::MemoryEvolutionEngine;
#[allow(unused_imports)]
pub use memory_retrieval::{EvolutionAwareRetrieval, EvolutionMemoryRetriever};
#[allow(unused_imports)]
pub use memory_safety::{
    ConflictChecker, MemorySafetyFilter, SafetyCheckResult, SafetyIssue, SafetyIssueKind,
    SourceMetadata,
};
#[allow(unused_imports)]
pub use pipeline::{EvolutionPipeline, EvolutionTrigger, PipelineRunReport};
pub use prompt_evolution::{PromptEvolutionEngine, PromptMutationType};
pub use record::{
    Actor, AnnotationSource, ChangeType, DataBasis, DecisionLog, DecisionType, EvolutionLayer,
    EvolutionLog, EvolutionResult, MemoryAccessLog, MemoryAction, Outcome, TaskType,
};
pub use rollback::{CircuitBreaker, CircuitBreakerState, RollbackManager, VersionSnapshot};
#[allow(unused_imports)]
pub use scheduler::{EvolutionScheduler, SchedulerRunSummary, SchedulerState};
pub use storage::{AsyncJsonlWriter, JsonlRetentionPolicy, JsonlStoragePaths};
pub use strategy_evolution::{StrategyEvolutionEngine, TaskDailySummary};
#[allow(unused_imports)]
pub use test_suite::{TestSplit, TestSuite, TestTask};
pub use trace::{
    current_trace, generate_experiment_id, generate_trace_id, with_trace, TraceContext,
};
