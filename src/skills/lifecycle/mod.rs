//! Skill lifecycle: immutable, content-addressed skill revisions in an
//! owner-scoped store (SL01). Publication, runtime binding and evaluation
//! are separate packages and are not part of this module yet. SL08 store level
//! adds the publication journal, generation CAS, rollback and recovery. SL06
//! adds frozen evaluation suites and the baseline-versus-candidate evaluator. SL07
//! adds the deterministic gate, its receipt and the optional judge. SL09 adds
//! the source review, the single candidate writer and the lifecycle service,
//! the policy owner that every mutation goes through.

/// Test-only crash injection point; expands to nothing outside `cfg(test)`.
macro_rules! crash_point {
    ($name:expr) => {
        #[cfg(test)]
        {
            $crate::skills::lifecycle::publish::crash_hooks::check($name)?;
        }
    };
}

mod agent_executor;
mod authorize;
mod binding;
mod bundle;
mod candidate;
mod capability;
mod capture;
mod consolidate;
mod evaluate;
mod evaluator;
mod facts;
mod gate;
mod gate_judge;
mod generation;
mod journal;
mod knowledge;
mod manifest;
mod migrate;
mod proposal;
mod propose;
mod propose_new;
mod proposer_model;
mod publish;
mod receipts;
mod rejection;
mod review;
mod service;
mod store;
mod suite;
#[cfg(test)]
pub(crate) mod test_support;
mod types;

#[cfg(test)]
mod tests;

#[cfg(test)]
use agent_executor::read_usage_for_test as agent_executor_read_usage_for_test;
pub use agent_executor::{AccountingOwner, AgentCaseExecutor};
pub use authorize::{Finalization, ForceClose, MAX_PUBLICATION_ATTEMPTS, PublicationOutcome};
pub use binding::{
    MAX_ACTIVE_BINDINGS, MAX_TASK_PINS, ManagedReadRequest, ManagedSkillRead, ORPHAN_PIN_AGE, read_managed_resource,
    release_task_pins, task_pin,
};
pub use bundle::{
    BUNDLE_FORMAT_VERSION, Bundle, BundleEntry, BundleError, BundleFile, MAX_BUNDLE_COMPONENT_BYTES, MAX_BUNDLE_DEPTH,
    MAX_BUNDLE_FILE_BYTES, MAX_BUNDLE_FILES, MAX_BUNDLE_PATH_BYTES, MAX_BUNDLE_TOTAL_BYTES,
};
pub use capture::{
    CAPTURE_LOCK_TIMEOUT, Correlation, ExperienceOutcome, ExperienceRetention, HydrationReceipt,
    MAX_HYDRATION_RECEIPTS, MAX_SELECTION_RECEIPTS, SelectionReceipt, SkillExperience, TaskTerminal, TerminalJoin,
    TerminalJoinOutcome, UNCOVERED_RECORDING_LIMIT, experience_id, on_task_terminal, task_key,
};
pub use consolidate::{ConsolidationOutcome, ConsolidationRequest};
pub use evaluate::{
    Arm, ArmBinding, ArtifactDigest, CaseExecutor, CaseReceipt, CaseVerdict, CleanupState, EVALUATION_SCHEMA_VERSION,
    EvaluationError, EvaluationOutcome, EvaluationReceipt, EvaluationSummary, ExecutionRequest, ExperienceTerminalFact,
    HydrationFact, IncompleteReason, InstallSkill, InstalledSkill, InvalidReason, RUNNER_IDENTITY, RunObservation,
    RunReceipt, RunTerminal, ValidatorOutcome, VerifiedEvaluation,
};
pub use evaluator::{EvaluationRequest, RecordedEvaluation};
pub use facts::{
    Adherence, Exclusion, FactState, MAX_ADHERENCE_NAMES, MAX_ADHERENCE_SKILLS, ObservedCallStatus, ObservedToolCall,
    ProviderEnd, SkillAdherence, TerminalFacts, TerminalObservations,
};
pub use gate::{
    EvaluationPin, GATE_IDENTITY, GateDecision, GateError, GateEvaluationEntry, GateOutcome, GatePolicy, GateReason,
    GateRequest, JudgeRecord, JudgeResult, MAX_GATE_EPOCHS, RecordedGate, SkillGateDecision, VerifiedGate,
};
pub use gate_judge::{
    GateJudge, JUDGE_PROMPT_VERSION, JudgeBrief, JudgeError, JudgeOpinion, JudgeReply, JudgedEvaluation,
    MAX_JUDGE_REASON_BYTES, ModelJudge,
};
pub use generation::ActiveBinding;
pub use journal::{JournalEntry, TransactionId, TransactionKind, TransactionState};
pub use knowledge::{
    ConfidenceBasis, FailureClass, FailureFacts, KnowledgePattern, MAX_PATTERN_EVIDENCE,
    MIN_DISTINCT_TASKS_FOR_PROPOSAL, PatternId, PatternIndex, PatternIndexEntry, PatternKind, PatternScope,
    classify_failure,
};
pub use manifest::{ManifestError, ManifestInfo, validate_manifest, validate_manifest_strict};
pub use migrate::{MigrationAction, MigrationEntry, MigrationPlan, MigrationReport, apply_migration, plan_migration};
pub use proposal::{
    AppliedEdit, AttemptOutcome, AttemptRecord, AuthorBinding, BoundedDiff, BudgetRecord, DescriptionChange, EditOp,
    MAX_PROPOSAL_ATTEMPTS, MAX_PROPOSAL_LISTING, ProposalBasis, ProposalFailure, ProposalFailureKind, ProposalStatus,
    PublicationAuthorization, PublicationRecord, SkillProposal, UnreadableProposal,
};
pub use propose::{
    MAX_EDIT_PERCENT, MAX_EDIT_TOKENS, MAX_EDITS, MAX_PROPOSAL_PATTERNS, ProposeOutcome, ProposeRequest,
    ProposerBudget, SYSTEM_PROMPT_VERSION, SuppressionStage, propose_revision,
};
pub use propose_new::{NEW_SKILL_PROMPT_VERSION, NewSkillRequest, propose_new_skill};
pub use proposer_model::{
    ManagedProviderModel, ModelBinding, ModelCallError, ModelReply, ProposerModel, ReportedUsage,
};
pub use publish::{PUBLICATION_LOCK_TIMEOUT, PublishRequest, RecoveryReport, RollbackRequest, TransactionOutcome};
#[cfg(test)]
pub(crate) use receipts::drain_background_for_test;
pub use receipts::{
    SelectionRequest, TerminalKind, TerminalObservation, record_selected_skills, schedule_selection_receipts,
    settle_task_terminal, spawn_settle_task_terminal,
};
pub use rejection::{NewRejection, ProposalOperation, RejectionRecord, material_digest};
pub use review::{ReviewCheck, ReviewFinding, SOURCE_REVIEW_IDENTITY, SourceReviewOutcome, SourceReviewRecord};
pub use service::{
    LifecycleLimits, LifecycleMode, LifecycleStatus, MIN_PUBLICATION_CASES_FLOOR, ProposalDetail, ProposalSummary,
    ReviewSummary, ServiceError, SkillsLifecycleService, SkillsLifecycleSettings, SkippedProposal, StartupReport,
    StartupSummary, SuiteRegistry, default_gate_policy,
};
pub use store::{SkillLifecycleStore, VerifiedRevision, WriteOutcome, record_digest};
pub use suite::{
    EvalCase, EvalSplit, EvalSuite, FixtureFile, FrozenSuite, MAX_ALLOWED_REGRESSIONS, MAX_HOLDOUT_ATTEMPTS,
    MAX_SUITE_CASES, MAX_TRIALS, MAX_VALIDATION_ATTEMPTS, MIN_SAMPLE_CASES, READ_ONLY_REQUIRED_TOOLS,
    SUITE_SCHEMA_VERSION, SideEffects, SuiteBudget, Validator, WRITE_REQUIRED_TOOLS,
};
pub use types::{
    LifecycleError, NewRevision, OwnerScope, PinnedSkillRevision, RevisionId, SCHEMA_VERSION, SkillId, SkillRevision,
    SourceKind,
};
