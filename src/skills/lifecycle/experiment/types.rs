//! Pure immutable experiment contracts. No transport, store or approval authority.
use super::ExperimentError;
use crate::skills::lifecycle::{EvalSplit, ModelBinding, OwnerScope, RevisionId, SkillId, TransactionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const EXPERIMENT_SCHEMA_VERSION: u32 = 1;
pub const MAX_EXPERIMENT_STAGES: usize = 64;

/// The storage identity is scoped by the existing lifecycle owner and directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExperimentId(TransactionId);
impl ExperimentId {
    pub fn parse(value: &str) -> Result<Self, ExperimentError> {
        Ok(Self(TransactionId::parse(value)?))
    }
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Cumulative resources; reservation and observed usage use integer units.
/// Unknown usage retains its full allocation until independently reconciled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentResources {
    pub calls: u64,
    pub tokens: u64,
    pub micro_usd: u64,
    pub tool_calls: u64,
    pub output_bytes: u64,
    pub scratch_bytes: u64,
}
impl ExperimentResources {
    pub fn checked_add(self, other: Self) -> Result<Self, ExperimentError> {
        fn add(left: u64, right: u64) -> Result<u64, ExperimentError> {
            left.checked_add(right).ok_or(ExperimentError::BudgetExceeded)
        }
        Ok(Self {
            calls: add(self.calls, other.calls)?,
            tokens: add(self.tokens, other.tokens)?,
            micro_usd: add(self.micro_usd, other.micro_usd)?,
            tool_calls: add(self.tool_calls, other.tool_calls)?,
            output_bytes: add(self.output_bytes, other.output_bytes)?,
            scratch_bytes: add(self.scratch_bytes, other.scratch_bytes)?,
        })
    }
    pub(super) fn component_max(self, other: Self) -> Self {
        Self {
            calls: self.calls.max(other.calls),
            tokens: self.tokens.max(other.tokens),
            micro_usd: self.micro_usd.max(other.micro_usd),
            tool_calls: self.tool_calls.max(other.tool_calls),
            output_bytes: self.output_bytes.max(other.output_bytes),
            scratch_bytes: self.scratch_bytes.max(other.scratch_bytes),
        }
    }
    pub const fn within(self, ceiling: Self) -> bool {
        self.calls <= ceiling.calls
            && self.tokens <= ceiling.tokens
            && self.micro_usd <= ceiling.micro_usd
            && self.tool_calls <= ceiling.tool_calls
            && self.output_bytes <= ceiling.output_bytes
            && self.scratch_bytes <= ceiling.scratch_bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStageKind {
    ModelProposal,
    SolverTrial,
    IndependentVerification,
    Selection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentStage {
    pub id: String,
    pub kind: ExperimentStageKind,
    pub dependencies: Vec<String>,
    pub input_sha256: String,
    pub allocation: ExperimentResources,
}

/// Operator intent before authoritative pins and operation hashes are resolved.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentDraft {
    pub id: ExperimentId,
    pub skill_id: SkillId,
    pub verification_split: EvalSplit,
    pub resources: ExperimentResources,
    pub elapsed_ms: u64,
    pub max_concurrent: u32,
    pub stages: Vec<ExperimentDraftStage>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentDraftStage {
    pub id: String,
    pub dependencies: Vec<String>,
    pub allocation: ExperimentResources,
    pub operation: super::ExperimentOperation,
}

/// All digests refer to admitted inputs, never model-declared approval or quality.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentPins {
    pub skill_id: SkillId,
    pub baseline_revision: RevisionId,
    pub baseline_record_digest: String,
    pub baseline_generation: u64,
    pub binary_sha256: String,
    pub config_sha256: String,
    pub policy_sha256: String,
    pub catalog_sha256: String,
    pub environment_sha256: String,
    pub edit_scope_sha256: String,
    pub model: ModelBinding,
    pub suite_digest: String,
    pub development_split_digest: String,
    pub verification_split: EvalSplit,
    pub verification_split_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentSpec {
    pub schema_version: u32,
    pub id: ExperimentId,
    pub owner_scope: OwnerScope,
    pub pins: ExperimentPins,
    pub resources: ExperimentResources,
    pub max_concurrent: u32,
    pub elapsed_ms: u64,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Topological order is frozen; ready stages may run only within concurrency limits.
    pub stages: Vec<ExperimentStage>,
}

/// Validation is not execution admission. This cannot grant a model or tool send.
#[derive(Clone, Debug)]
pub struct FrozenExperiment {
    spec: ExperimentSpec,
    digest: String,
}
impl FrozenExperiment {
    pub const fn spec(&self) -> &ExperimentSpec {
        &self.spec
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn ensure_live(&self, now: DateTime<Utc>) -> Result<(), ExperimentError> {
        if now < self.spec.created_at || now >= self.spec.expires_at {
            return Err(ExperimentError::Expired);
        }
        Ok(())
    }
}

pub(super) fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
impl ExperimentSpec {
    pub fn freeze(self) -> Result<FrozenExperiment, ExperimentError> {
        let refuse = |reason: &str| ExperimentError::InvalidSpec(reason.into());
        if self.schema_version != EXPERIMENT_SCHEMA_VERSION {
            return Err(refuse("unsupported experiment schema"));
        }
        if self.max_concurrent == 0
            || self.stages.is_empty()
            || self.stages.len() > MAX_EXPERIMENT_STAGES
            || u64::from(self.max_concurrent) > u64::try_from(self.stages.len()).unwrap_or(u64::MAX)
        {
            return Err(refuse("invalid stage count or concurrency"));
        }
        let duration = self
            .expires_at
            .signed_duration_since(self.created_at)
            .num_milliseconds();
        if duration <= 0 || u64::try_from(duration).map_or(true, |value| value > self.elapsed_ms) {
            return Err(refuse("invalid immutable experiment deadline"));
        }
        let pins = &self.pins;
        for digest in [
            &pins.baseline_record_digest,
            &pins.binary_sha256,
            &pins.config_sha256,
            &pins.policy_sha256,
            &pins.catalog_sha256,
            &pins.environment_sha256,
            &pins.edit_scope_sha256,
            &pins.suite_digest,
            &pins.development_split_digest,
            &pins.verification_split_digest,
        ] {
            if !is_digest(digest) {
                return Err(refuse("invalid pinned digest"));
            }
        }
        if pins.development_split_digest == pins.verification_split_digest
            || pins.verification_split == EvalSplit::Development
        {
            return Err(refuse(
                "independent verification must use a distinct validation or holdout split",
            ));
        }
        if pins.model.provider_label.is_empty() || pins.model.model.is_empty() {
            return Err(refuse("missing model binding"));
        }
        let mut seen = BTreeSet::new();
        for stage in &self.stages {
            TransactionId::parse(&stage.id)?;
            if stage.id.len() > 48 {
                return Err(refuse("stage identity exceeds 48 bytes"));
            }
            if !is_digest(&stage.input_sha256) || !stage.allocation.within(self.resources) {
                return Err(refuse("stage input or allocation is invalid"));
            }
            let mut dependencies = BTreeSet::new();
            for dependency in &stage.dependencies {
                if !seen.contains(dependency) || !dependencies.insert(dependency) {
                    return Err(refuse("stage dependencies are not a unique topological prefix"));
                }
            }
            if !seen.insert(stage.id.clone()) {
                return Err(refuse("duplicate stage identity"));
            }
            if matches!(
                stage.kind,
                ExperimentStageKind::ModelProposal | ExperimentStageKind::SolverTrial
            ) && (stage.allocation.calls == 0 || stage.allocation.tokens == 0 || stage.allocation.output_bytes == 0)
            {
                return Err(refuse("model stage has no admitted call/token allocation"));
            }
        }
        let mut hash = Sha256::new();
        hash.update(b"prx-instruction-experiment-spec-v1\0");
        let bytes = serde_json::to_vec(&self).map_err(|error| refuse(&error.to_string()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > super::super::store::MAX_RECORD_BYTES {
            return Err(refuse("experiment specification exceeds the record limit"));
        }
        hash.update(bytes);
        Ok(FrozenExperiment {
            spec: self,
            digest: hex::encode(hash.finalize()),
        })
    }
}

/// Optional binding on existing evaluation receipts. The receipt already binds
/// the exact candidate, frozen cases, run ids, artifacts and validator results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentRunBinding {
    pub experiment_sha256: String,
    pub stage: String,
    pub input_sha256: String,
}
