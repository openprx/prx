pub mod decision_log;
pub mod experiment;
pub mod fitness;
pub mod orchestrator;
pub mod policy_compiler;
pub mod tools_alignment;

pub use decision_log::{log_change_outcome, log_change_proposal};
pub use experiment::{
    complete_experiment, rollback_experiment, start_experiment, ExperimentRecord, ExperimentStatus,
};
pub use fitness::run_fitness_report;
pub use orchestrator::{
    get_evolution_history, pause_evolution, resume_evolution, run_evolution_cycle, ChangeOperation,
    ChangeTarget, CycleOutcome, EvolutionCycle, EvolutionProposal, EvolutionSignals,
    EvolutionState, EvolutionValidation, FitnessTrend, HealthSource, RiskLevel, RuntimeCronStore,
    RuntimeHealth, ValidationStatus,
};
pub use policy_compiler::{
    compile_policy, compile_policy_from_sources, CompiledPolicy, FitnessWeightPolicy,
    SelfModifyPermission,
};
pub use tools_alignment::{check_tools_alignment, AlignmentReport};
