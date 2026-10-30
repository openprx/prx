//! Sealed private gate-stage witness. Reading this record grants no current
//! gate or publication authority and never calls a model or repairs evidence.
use super::{ExperimentError, ExperimentId, ExperimentJournal, ExperimentStageKind};
use crate::skills::lifecycle::SkillLifecycleStore;
use crate::skills::lifecycle::gate::{GateExperimentBinding, GateReviewSubject};
use crate::skills::lifecycle::generation::ensure_dir_synced;
use crate::skills::lifecycle::store::{check_private, read_record, record_bytes};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Snapshot from the existing actual stage physical ledger, not a second balance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::skills::lifecycle) struct GateReviewPhysical {
    pub calls: u64,
    pub settled_calls: u64,
    pub tokens: u64,
    pub micro_usd: u64,
    pub attempt_ids: Vec<String>,
    pub models: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::skills::lifecycle) struct GateVerificationWindow {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::skills::lifecycle) struct GateReviewWitness {
    pub schema_version: u32,
    pub binding: GateExperimentBinding,
    pub subject: GateReviewSubject,
    pub proposal_stage: String,
    pub selection_stage: String,
    pub validation_stage: String,
    pub validation_verifier: String,
    pub holdout_stage: String,
    pub holdout_verifier: String,
    pub gate_id: String,
    pub gate_digest: String,
    pub policy_snapshot: serde_json::Value,
    pub current_verification: Option<GateVerificationWindow>,
    pub physical: Option<GateReviewPhysical>,
}
impl GateReviewWitness {
    pub fn digest(&self) -> Result<String, ExperimentError> {
        crate::skills::lifecycle::evaluate::seal_digest(self)
            .map_err(|error| ExperimentError::InvalidEvidence(error.to_string()))
    }
}
fn refusal(reason: &str) -> ExperimentError {
    ExperimentError::InvalidEvidence(reason.into())
}
impl SkillLifecycleStore {
    pub(in crate::skills::lifecycle) fn write_gate_review(
        &self,
        journal: &ExperimentJournal,
        witness: &GateReviewWitness,
        runtime: &crate::runtime::experiment_stage::ExperimentStageRuntime,
    ) -> Result<String, ExperimentError> {
        let actual = runtime
            .gate_record_binding(self.owner_scope())
            .map_err(|error| refusal(&error.to_string()))?;
        let stage = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|stage| stage.id == witness.binding.experiment.stage)
            .ok_or_else(|| refusal("gate witness stage is absent"))?;
        let physical = witness
            .physical
            .as_ref()
            .ok_or_else(|| refusal("gate witness has no post-cleanup physical completion"))?;
        if witness.schema_version != 1
            || witness.binding != actual
            || runtime.gate_subject().as_ref() != Some(&witness.subject)
            || witness.binding.experiment.experiment_sha256 != journal.plan.digest()
            || witness.binding.experiment.input_sha256 != stage.input_sha256
            || stage.kind != ExperimentStageKind::GateReview
            || journal.plan.spec().owner_scope != *self.owner_scope()
            || physical.calls != physical.settled_calls
            || (witness.current_verification.is_some() && physical.calls == 0)
            || physical.attempt_ids.len() != usize::try_from(physical.calls).unwrap_or(usize::MAX)
        {
            return Err(refusal(
                "gate witness differs from its actual stage or settled physical owner",
            ));
        }
        let _store = self.lock_store(true, Duration::from_secs(10))?;
        let _skill = self.lock_skill(&journal.plan.spec().pins.skill_id, Duration::from_secs(10))?;
        let root = self.root().join("experiment-evidence");
        ensure_dir_synced(self.root(), &root)?;
        let directory = root.join(journal.plan.spec().id.as_str());
        ensure_dir_synced(&root, &directory)?;
        let path = directory.join(format!("{}.gate-review.json", stage.id));
        let bytes = record_bytes(witness, &path)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > crate::skills::lifecycle::store::MAX_RECORD_BYTES {
            return Err(refusal("gate witness exceeds the retained record bound"));
        }
        let digest = witness.digest()?;
        if std::fs::symlink_metadata(&path).is_ok() {
            check_private(&path)?;
            let existing: GateReviewWitness = read_record(&path)?;
            if existing.digest()? != digest {
                return Err(ExperimentError::Conflict);
            }
        } else {
            self.write_atomic(&journal.plan.spec().pins.skill_id, &path, &bytes)?;
        }
        Ok(digest)
    }
    pub(in crate::skills::lifecycle) fn read_gate_review(
        &self,
        id: &ExperimentId,
        stage: &str,
        expected_digest: &str,
    ) -> Result<GateReviewWitness, ExperimentError> {
        crate::skills::lifecycle::TransactionId::parse(stage)?;
        let root = self.root().join("experiment-evidence");
        let directory = root.join(id.as_str());
        let path = directory.join(format!("{stage}.gate-review.json"));
        for entry in [&root, &directory, &path] {
            check_private(entry)?;
        }
        let witness: GateReviewWitness = read_record(&path)?;
        self.check_owner_scope(&witness.binding.owner_scope)?;
        if witness.schema_version != 1
            || witness.binding.experiment.stage != stage
            || witness.digest()? != expected_digest
        {
            return Err(refusal("gate witness seal or stage identity changed"));
        }
        Ok(witness)
    }
}
