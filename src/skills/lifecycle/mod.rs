//! Skill lifecycle: immutable, content-addressed skill revisions in an
//! owner-scoped store (SL01). Publication, runtime binding and evaluation
//! are separate packages and are not part of this module yet. SL08 store level
//! adds the publication journal, generation CAS, rollback and recovery. SL06
//! adds frozen evaluation suites and the baseline-versus-candidate evaluator. SL07
//! adds the deterministic gate, its receipt and the optional judge.

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
mod capture;
mod consolidate;
mod evaluate;
mod evaluator;
mod gate;
mod gate_judge;
mod generation;
mod journal;
mod knowledge;
mod manifest;
mod migrate;
mod proposal;
mod propose;
mod proposer_model;
mod publish;
mod rejection;
mod store;
mod suite;
mod types;

#[cfg(test)]
mod tests;

#[cfg(test)]
use agent_executor::read_usage_for_test as agent_executor_read_usage_for_test;
pub use agent_executor::{AccountingOwner, AgentCaseExecutor};
pub use authorize::{MAX_PUBLICATION_ATTEMPTS, PublicationOutcome};
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
    TerminalJoinOutcome, experience_id, on_task_terminal, task_key,
};
pub use consolidate::{ConsolidationOutcome, ConsolidationRequest};
pub use evaluate::{
    Arm, ArmBinding, ArtifactDigest, CaseExecutor, CaseReceipt, CaseVerdict, CleanupState, EVALUATION_SCHEMA_VERSION,
    EvaluationError, EvaluationOutcome, EvaluationReceipt, EvaluationSummary, ExecutionRequest, ExperienceTerminalFact,
    HydrationFact, IncompleteReason, InstallSkill, InstalledSkill, InvalidReason, RUNNER_IDENTITY, RunObservation,
    RunReceipt, RunTerminal, ValidatorOutcome, VerifiedEvaluation,
};
pub use evaluator::{EvaluationRequest, RecordedEvaluation};
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
pub use manifest::{ManifestError, ManifestInfo, validate_manifest};
pub use migrate::{MigrationAction, MigrationEntry, MigrationPlan, MigrationReport, apply_migration, plan_migration};
pub use proposal::{
    AppliedEdit, AttemptOutcome, AttemptRecord, AuthorBinding, BoundedDiff, BudgetRecord, DescriptionChange, EditOp,
    MAX_PROPOSAL_ATTEMPTS, MAX_PROPOSAL_LISTING, ProposalBasis, ProposalFailure, ProposalFailureKind, ProposalStatus,
    PublicationAuthorization, PublicationRecord, SkillProposal,
};
pub use propose::{
    MAX_EDIT_PERCENT, MAX_EDIT_TOKENS, MAX_EDITS, MAX_PROPOSAL_PATTERNS, ProposeOutcome, ProposeRequest,
    ProposerBudget, SYSTEM_PROMPT_VERSION, SuppressionStage, propose_revision,
};
pub use proposer_model::{
    ManagedProviderModel, ModelBinding, ModelCallError, ModelReply, ProposerModel, ReportedUsage,
};
pub use publish::{PUBLICATION_LOCK_TIMEOUT, PublishRequest, RecoveryReport, RollbackRequest, TransactionOutcome};
pub use rejection::{NewRejection, ProposalOperation, RejectionRecord, material_digest};
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
