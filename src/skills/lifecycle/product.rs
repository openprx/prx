//! Product intents and truthful observations. Deserializable pins grant no authority.
mod binding;
mod errors;
pub(in crate::skills::lifecycle) use binding::experiment_proposal_request_id;

use super::experiment::{ExperimentId, ExperimentResources, InstructionExperimentPlan};
use super::{JournalEntry, LessonRef, RevisionId, SkillId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentProductHandle {
    pub experiment_id: ExperimentId,
    pub plan_sha256: String,
}
impl ExperimentProductHandle {
    pub fn of(plan: &InstructionExperimentPlan) -> Result<Self, super::experiment::ExperimentError> {
        plan.validate()?;
        Ok(Self {
            experiment_id: plan.spec.id.clone(),
            plan_sha256: plan.spec.clone().freeze()?.digest().into(),
        })
    }
}

/// Expected immutable result, not a VerifiedGate or a publication grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentFinalExpectation {
    pub handle: ExperimentProductHandle,
    pub proposal_id: String,
    pub proposal_stage: String,
    pub skill_id: SkillId,
    pub baseline_revision: RevisionId,
    pub baseline_record_sha256: String,
    pub baseline_generation: u64,
    pub candidate_revision: RevisionId,
    pub candidate_record_sha256: String,
    pub evaluation_epoch: u32,
    pub evaluating_since: DateTime<Utc>,
    pub suite_sha256: String,
    pub gate_stage: String,
    pub gate_witness_sha256: String,
    pub gate_id: String,
    pub gate_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentPublishRequest {
    pub expected: ExperimentFinalExpectation,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentAnnotationRequest {
    pub expected: ExperimentFinalExpectation,
    pub publication_tx: String,
    pub task_family: String,
    pub expires_at: DateTime<Utc>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentRollbackRequest {
    pub handle: ExperimentProductHandle,
    pub prior_revision: RevisionId,
    pub observed_generation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentAdoptionStatusRequest {
    pub handle: ExperimentProductHandle,
    pub reference: LessonRef,
    pub retired: bool,
    pub invalidated: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentLessonQueryRequest {
    pub handle: ExperimentProductHandle,
    pub task_family: String,
    /// Query lifetime only, bounded by the service; never refreshes an experiment.
    pub lifetime_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentExecutionStatus {
    Planned,
    ActiveUnconfirmed,
    CancelRequested,
    Unknown,
    Failed,
    BaselineRetained,
    Completed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentQualification {
    QualifiedHistorical,
    Nonqualifying,
    EvidenceUnavailable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentCleanupStatus {
    NotStarted,
    RequestedUnconfirmed,
    Unconfirmed,
    Confirmed,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentPublicationStatus {
    NotPublished,
    Authorized,
    PreparedUncertain,
    Committed,
    RepairRequired,
    Aborted,
    ObservationUnavailable,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentPublicationObservation {
    pub status: ExperimentPublicationStatus,
    pub transaction: Option<JournalEntry>,
    pub authorized_attempt: Option<u32>,
    pub attempts: Vec<JournalEntry>,
    pub proposal_committed: bool,
    /// Durable publication facts do not prove current active binding or benefit.
    pub provenance: &'static str,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStageStatus {
    Planned,
    Reserved,
    Dispatched,
    DispatchUnknown,
    Settled,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentStageObservation {
    pub stage: String,
    pub state: ExperimentStageStatus,
    pub settled_usage: Option<ExperimentResources>,
    pub cleanup_confirmed: bool,
    pub allocation: ExperimentResources,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentProductSnapshot {
    pub handle: ExperimentProductHandle,
    pub owner_scope: String,
    pub journal_head_sha256: String,
    pub cancel_requested: bool,
    pub execution: ExperimentExecutionStatus,
    pub qualification: ExperimentQualification,
    pub final_selection: Option<ExperimentFinalExpectation>,
    pub publication: ExperimentPublicationObservation,
    pub adoption: ExperimentAdoptionObservation,
    pub stages: Vec<ExperimentStageObservation>,
    /// Existing state-owner total: settled spend or full unresolved allocation, once.
    pub committed_or_reserved: ExperimentResources,
    pub settled_usage: ExperimentResources,
    pub cleanup: ExperimentCleanupStatus,
    /// No live-process claim is inferred from a journal row or watcher interval.
    pub termination_confirmed: bool,
    pub observation_provenance: &'static str,
    pub physical_observations: Option<serde_json::Value>,
    pub evidence_issues: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentProductPublication {
    pub expected: ExperimentFinalExpectation,
    pub transaction: JournalEntry,
    pub replayed: bool,
    pub proposal_committed: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentLessonQueryResult {
    pub owner_scope: String,
    pub task_family: String,
    pub query_sha256: String,
    pub index_sha256: String,
    pub references: Vec<LessonRef>,
    pub expires_at: DateTime<Utc>,
    pub guidance_delivery_or_utility_proved: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentAdoptionStatus {
    NotRecorded,
    Recorded,
    ObservationUnavailable,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentAdoptionObservation {
    pub status: ExperimentAdoptionStatus,
    pub records: Vec<ExperimentAdoptionStatusObservation>,
    pub provenance: &'static str,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentAdoptionStatusObservation {
    pub reference: LessonRef,
    pub handle: ExperimentProductHandle,
    pub owner_scope: String,
    pub skill_id: SkillId,
    pub task_family: String,
    pub retired: bool,
    pub invalidated: bool,
    pub expires_at: DateTime<Utc>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentProductFailureKind {
    Disabled,
    Unsupported,
    WrongOwner,
    InvalidInput,
    MissingAdmission,
    Stale,
    Conflict,
    Busy,
    SourceReviewRequired,
    NotEligible,
    Uncertain,
    RepairRequired,
    AnnotationFailed,
    EvidenceUnavailable,
}
#[derive(Debug, thiserror::Error, Serialize)]
#[error("{kind:?}: {message}")]
pub struct ExperimentProductError {
    pub kind: ExperimentProductFailureKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Box<ExperimentProductSnapshot>>,
}
impl ExperimentProductError {
    pub(in crate::skills::lifecycle) fn refused(
        kind: ExperimentProductFailureKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            snapshot: None,
        }
    }
}

/// Only identifies candidate historical lineage; actual journal/authentication
/// must still be checked by the original publisher under its proposal lock.
pub(in crate::skills::lifecycle) fn recorded_publication_status(status: super::ProposalStatus) -> bool {
    matches!(
        status,
        super::ProposalStatus::Committed
            | super::ProposalStatus::Monitored
            | super::ProposalStatus::Retained
            | super::ProposalStatus::Disabled
            | super::ProposalStatus::RolledBack
    )
}
