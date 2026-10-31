//! Preregistered, nonqualifying V01 studies. These records own no execution,
//! publication, adoption, grant or accounting authority.
mod analysis;
mod grading;
mod record;
mod report;
#[cfg(test)]
mod tests;
mod types;

pub(in crate::skills::lifecycle) use analysis::{interval_half_width, summarize};
pub use report::*;
pub(in crate::skills::lifecycle) use types::compile_pairs;
pub use types::{
    MAX_STUDY_PAIRS, STUDY_SCHEMA_VERSION, STUDY_TASK_PROFILE, StudyArm, StudyCalibration, StudyDraft,
    StudyExcludedRoot, StudyExperimentRef, StudyInterval, StudyManifest, StudyMetric, StudyOptimizerPin,
    StudyOriginPin, StudyPair, StudyPhase, StudyPlan, StudyProtocol, StudyRevisionPin, StudyRoot, StudyStopRule,
    StudyTrialInput,
};

use anyhow::{Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub(in crate::skills::lifecycle) fn digest<T: Serialize>(domain: &[u8], value: &T) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(bytes);
    Ok(hex::encode(hash.finalize()))
}

pub(in crate::skills::lifecycle) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(in crate::skills::lifecycle) fn require_digest(value: &str) -> Result<()> {
    ensure!(valid_digest(value), "study digest must be lowercase SHA-256");
    Ok(())
}

pub(in crate::skills::lifecycle) fn prompt_digest(prompt: &str) -> String {
    let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    hex::encode(Sha256::digest(normalized.as_bytes()))
}
