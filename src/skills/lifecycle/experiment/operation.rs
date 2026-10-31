//! Frozen bounded-stage inputs. Scheduling belongs to the separate cycle compiler.
use super::{ExperimentError, ExperimentStageKind};
use crate::skills::lifecycle::{EvalSplit, PatternId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevelopmentRole {
    General,
    Domain,
    Compose,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExperimentOperation {
    StudyTrial {
        input: crate::skills::lifecycle::study::StudyTrialInput,
    },
    Proposal {
        patterns: Vec<PatternId>,
        max_input_bytes: usize,
        max_output_bytes: usize,
        timeout_ms: u64,
    },
    Evaluation {
        proposal_stage: String,
        split: EvalSplit,
    },
    Verify {
        evaluation_stage: String,
    },
    /// One development arm, without rerunning a paired baseline or consuming
    /// validation/holdout attempts. None selects the immutable active baseline.
    DevelopmentProbe {
        proposal_stage: Option<String>,
    },
    /// A bounded role-specific revision grounded only in completed development
    /// probes. Its source review and publication requirements stay unchanged.
    DevelopmentProposal {
        role: DevelopmentRole,
        patterns: Vec<PatternId>,
        probe_stages: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_selection: Option<String>,
        /// Final literal guidance block digest, resolved by the service.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lesson_context_sha256: Option<String>,
        max_input_bytes: usize,
        max_output_bytes: usize,
        timeout_ms: u64,
    },
    DevelopmentSelection {
        probe_stages: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        parent_selections: Vec<String>,
    },
    SelectedEvaluation {
        selection_stage: String,
        split: EvalSplit,
        /// The verified primary validation required before one final holdout.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validation_stage: Option<String>,
    },
    /// Private gate creation after both exact independent verifiers succeeded.
    GateReview {
        selection_stage: String,
        validation_stage: String,
        validation_verifier: String,
        holdout_stage: String,
        holdout_verifier: String,
    },
}
impl ExperimentOperation {
    pub fn digest(&self) -> Result<String, ExperimentError> {
        let bytes = serde_json::to_vec(self).map_err(|error| ExperimentError::InvalidSpec(error.to_string()))?;
        let mut hash = Sha256::new();
        hash.update(b"prx-instruction-experiment-input-v1\0");
        hash.update(bytes);
        Ok(hex::encode(hash.finalize()))
    }
    pub const fn kind(&self) -> ExperimentStageKind {
        match self {
            Self::StudyTrial { .. } => ExperimentStageKind::StudyTrial,
            Self::Proposal { .. } | Self::DevelopmentProposal { .. } => ExperimentStageKind::ModelProposal,
            Self::Evaluation { .. } | Self::DevelopmentProbe { .. } | Self::SelectedEvaluation { .. } => {
                ExperimentStageKind::SolverTrial
            }
            Self::Verify { .. } => ExperimentStageKind::IndependentVerification,
            Self::DevelopmentSelection { .. } => ExperimentStageKind::Selection,
            Self::GateReview { .. } => ExperimentStageKind::GateReview,
        }
    }
}
