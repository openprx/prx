//! Frozen inputs for individual M1 bridges; scheduling belongs to M2.
use super::{ExperimentError, ExperimentStageKind};
use crate::skills::lifecycle::{EvalSplit, PatternId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExperimentOperation {
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
            Self::Proposal { .. } => ExperimentStageKind::ModelProposal,
            Self::Evaluation { .. } => ExperimentStageKind::SolverTrial,
            Self::Verify { .. } => ExperimentStageKind::IndependentVerification,
        }
    }
}
