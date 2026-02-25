pub mod decision_log;
pub mod evolution;
pub mod experiment;
pub mod fitness;
pub mod orchestrator;
pub mod policy_compiler;
pub mod tools_alignment;

#[allow(unused_imports)]
pub use decision_log::{log_change_outcome, log_change_proposal};
#[allow(unused_imports)]
pub use evolution::{
    current_trace, generate_experiment_id, generate_trace_id, with_trace, Actor,
    AnnotationPipeline, AnnotationReport, AnnotationSource, AnnotationUpdate, AsyncJsonlWriter,
    CandidatePriority, ChangeType, CircuitBreaker, CircuitBreakerState, ConfigEfficiencyIssue,
    CycleResult, DailyDigest, DataBasis, DataThresholds, DecisionLog, DecisionType,
    EngineCycleInput, EvolutionAnalyzer, EvolutionCandidate, EvolutionConfig, EvolutionEngine,
    EvolutionGate, EvolutionGateConfig, EvolutionLayer, EvolutionLog, EvolutionMode,
    EvolutionPipeline, EvolutionResult, EvolutionRetentionConfig, EvolutionRuntimeConfig,
    EvolutionRuntimeConfigManager, EvolutionScheduler, EvolutionTrigger, GateMetrics,
    GateRejection, GateResult, ImportSummary, JsonlRetentionPolicy, JsonlStoragePaths,
    JsonlToSqliteIndexer, JudgeConfig, JudgeDriftAlert, JudgeEngine, JudgeHealthMonitor,
    JudgeHealthReport, JudgeResult, JudgeScoringModel, MemoryAccessLog, MemoryAction,
    MemoryEvolutionConfig, MemoryEvolutionEngine, MetricShift, MockJudgeModel, NoiseMemoryPattern,
    Outcome, PipelineRunReport, PromptEvolutionConfig, PromptEvolutionEngine, PromptMutationType,
    RetrievalFusionWeights, RollbackConfig, RollbackManager, SchedulerRunSummary, SchedulerState,
    SearchHit, StrategyEvolutionConfig, StrategyEvolutionEngine, StructuredScores,
    TaskDailySummary, TaskType, TaskTypeDigest, TaskTypeWeakness, TestSplit, TestSuite, TestTask,
    TraceContext, TrendAnalysis, UserCorrectionCluster, VersionSnapshot,
};
#[allow(unused_imports)]
pub use experiment::{
    complete_experiment, rollback_experiment, start_experiment, ExperimentRecord, ExperimentStatus,
};
#[allow(unused_imports)]
pub use fitness::run_fitness_report;
#[allow(unused_imports)]
pub use orchestrator::{
    get_evolution_history, pause_evolution, resume_evolution, run_engine_cycle,
    run_evolution_cycle, ChangeOperation, ChangeTarget, CycleOutcome, EvolutionCycle,
    EvolutionProposal, EvolutionSignals, EvolutionState, EvolutionValidation, FitnessTrend,
    HealthSource, RiskLevel, RuntimeCronStore, RuntimeHealth, ValidationStatus,
};
#[allow(unused_imports)]
pub use policy_compiler::{
    compile_policy, compile_policy_from_sources, CompiledPolicy, FitnessWeightPolicy,
    SelfModifyPermission,
};
#[allow(unused_imports)]
pub use tools_alignment::{check_tools_alignment, AlignmentReport};
