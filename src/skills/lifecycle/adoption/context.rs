//! Small shared A/B contract; no public preparation constructor or model send.
use crate::skills::lifecycle::experiment::ExperimentRunBinding;
use crate::skills::lifecycle::generation::StoreLock;
use crate::skills::lifecycle::{LifecycleError, ModelBinding, OwnerScope, RevisionId, SkillId, SkillLifecycleStore};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Pure data. Deserialization or a matching hash is never adoption authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LessonRef {
    pub adoption_id: String,
    pub adoption_sha256: String,
    pub publication_tx: String,
    pub adopted_revision: RevisionId,
    pub adopted_record_sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LessonLimits {
    pub examined: u32,
    pub index_bytes: u64,
    pub record_bytes: u64,
    pub decoded_bytes: u64,
    pub lessons: u32,
    pub item_bytes: u64,
    pub context_bytes: u64,
    pub context_token_upper_bound: u64,
    pub elapsed_ms: u64,
}
impl Default for LessonLimits {
    fn default() -> Self {
        Self {
            examined: 64,
            index_bytes: 32 * 1024,
            record_bytes: 16 * 1024,
            decoded_bytes: 1024 * 1024,
            lessons: 2,
            item_bytes: 512,
            context_bytes: 1536,
            context_token_upper_bound: 1536,
            elapsed_ms: 500,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LessonCompatibility {
    pub model_sha256: String,
    pub policy_sha256: String,
    pub source_profile_sha256: String,
    pub tokenizer_profile_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LessonSnapshotEntry {
    pub reference: LessonRef,
    pub evidence_pin_sha256: String,
    pub sanitized_guidance: String,
    pub guidance_sha256: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LessonSnapshotStats {
    pub examined: u32,
    pub index_bytes: u64,
    pub decoded_bytes: u64,
    pub excluded: u32,
    pub elapsed_ms: u64,
}

/// Frozen data only. Plan/admission/runtime must resolve and verify every pin
/// and deterministically rebuild guidance from supported verified source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LessonSnapshot {
    pub schema_version: u32,
    pub owner_scope: OwnerScope,
    pub task_family: String,
    pub query_sha256: String,
    pub index_sha256: String,
    pub compatibility: LessonCompatibility,
    pub exclusions: Vec<String>,
    pub limits: LessonLimits,
    pub stats: LessonSnapshotStats,
    pub entries: Vec<LessonSnapshotEntry>,
    pub formatter_version: String,
    pub planned_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LessonPlacement {
    FirstSoleUserText,
}

/// Opaque runtime data, not serializable authority. This interface commit
/// deliberately exposes no factory: unverified snapshots cannot create one.
pub(crate) struct PreparedLessonContext {
    store: Arc<SkillLifecycleStore>,
    skill_id: SkillId,
    binding: ExperimentRunBinding,
    refs: Vec<LessonRef>,
    snapshot_sha256: String,
    block: String,
    block_sha256: String,
    expected_user_slot_sha256: String,
    expected_user_slot_bytes: u64,
    placement: LessonPlacement,
}
impl PreparedLessonContext {
    pub(crate) fn block(&self) -> &str {
        &self.block
    }
    pub(crate) fn block_sha256(&self) -> &str {
        &self.block_sha256
    }
    pub(crate) fn expected_user_slot_sha256(&self) -> &str {
        &self.expected_user_slot_sha256
    }
    pub(crate) const fn expected_user_slot_bytes(&self) -> u64 {
        self.expected_user_slot_bytes
    }
    pub(crate) fn snapshot_sha256(&self) -> &str {
        &self.snapshot_sha256
    }
    pub(crate) fn refs(&self) -> &[LessonRef] {
        &self.refs
    }
    pub(crate) const fn binding(&self) -> &ExperimentRunBinding {
        &self.binding
    }
    pub(crate) const fn placement(&self) -> LessonPlacement {
        self.placement
    }

    /// Checks the COMPLETE decoded logical slot. The actual wire parser and
    /// duplicate/placement checks remain B's existing-transport responsibility.
    pub(crate) fn compare_actual_user_slot(&self, actual: &str) -> Result<String, LifecycleError> {
        let digest = hex::encode(Sha256::digest(actual.as_bytes()));
        if u64::try_from(actual.len()).unwrap_or(u64::MAX) != self.expected_user_slot_bytes
            || digest != self.expected_user_slot_sha256
            || self.block.is_empty()
            || actual.matches(&self.block).count() != 1
        {
            return Err(LifecycleError::ProposalRefused(
                "lesson user slot was clipped, changed or duplicated".into(),
            ));
        }
        Ok(digest)
    }

    /// The small contract commit has no verified adoption/profile factory.
    /// Until A installs that proof and runtime revalidation, no transport can
    /// acquire a handoff guard; callers must preserve admitted liability.
    pub(crate) fn lock_handoff(
        &self,
        _actual_model: &ModelBinding,
        _now: DateTime<Utc>,
    ) -> Result<LessonHandoffGuard<'_>, LifecycleError> {
        Err(LifecycleError::ProposalRefused(
            "verified lesson preparation is unavailable".into(),
        ))
    }
}

/// Owns only the pre-existing shared store lock. B may take the actual skill
/// lock inside its bounded blocking-I/O closure, then must release before wire.
pub(crate) struct LessonHandoffGuard<'a> {
    _store_lock: StoreLock,
    prepared: &'a PreparedLessonContext,
}
impl LessonHandoffGuard<'_> {
    pub(in crate::skills::lifecycle) fn store(&self) -> &SkillLifecycleStore {
        &self.prepared.store
    }
    pub(in crate::skills::lifecycle) const fn skill_id(&self) -> &SkillId {
        &self.prepared.skill_id
    }
    pub(in crate::skills::lifecycle) const fn prepared(&self) -> &PreparedLessonContext {
        self.prepared
    }
}
