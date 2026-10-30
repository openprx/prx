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
pub struct LessonRef {
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

/// Opaque runtime data, not serializable authority. Only the actual bound
/// editor factory below may construct it after signed-source revalidation.
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
    plan: crate::skills::lifecycle::experiment::FrozenExperiment,
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

    /// Hold the existing store shared owner through B's durable handoff write.
    /// Status invalidation and retention take this same owner exclusively.
    pub(crate) fn lock_handoff(
        &self,
        actual_model: &ModelBinding,
        now: DateTime<Utc>,
    ) -> Result<LessonHandoffGuard<'_>, LifecycleError> {
        let snapshot = self
            .plan
            .spec()
            .pins
            .lesson_snapshot
            .as_ref()
            .ok_or_else(|| super::record::refusal("prepared lesson snapshot missing"))?;
        let store_lock = self
            .store
            .lock_store(true, std::time::Duration::from_millis(snapshot.limits.elapsed_ms))?;
        self.store
            .validate_prepared_lesson_locked(&self.plan, &self.binding, actual_model, now)?;
        Ok(LessonHandoffGuard {
            _store_lock: store_lock,
            prepared: self,
        })
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
    /// B calls only after taking this target skill lock, before its first
    /// durable handoff write. Publication cannot change the active target in
    /// the shared-store to actual-skill acquisition interval unnoticed.
    pub(in crate::skills::lifecycle) fn recheck_target_locked(&self, now: DateTime<Utc>) -> Result<(), LifecycleError> {
        self.prepared.store.validate_prepared_lesson_locked(
            &self.prepared.plan,
            &self.prepared.binding,
            &self.prepared.plan.spec().pins.model,
            now,
        )
    }
}

impl SkillLifecycleStore {
    pub(in crate::skills::lifecycle) fn prepare_verified_lesson_context(
        self: &Arc<Self>,
        plan: &crate::skills::lifecycle::experiment::FrozenExperiment,
        binding: ExperimentRunBinding,
        final_user: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Arc<PreparedLessonContext>>, LifecycleError> {
        let Some(snapshot) = &plan.spec().pins.lesson_snapshot else {
            return Ok(None);
        };
        let _owner = self.lock_store(true, std::time::Duration::from_millis(snapshot.limits.elapsed_ms))?;
        self.validate_prepared_lesson_locked(plan, &binding, &plan.spec().pins.model, now)?;
        if snapshot.entries.is_empty() {
            return Ok(None);
        }
        // NFC is identity only for this supported ASCII editor surface. The
        // frozen profile's byte alphabet and boundary reserve cover its block.
        if !final_user.is_ascii() {
            return Err(super::record::refusal(
                "verified lesson profile requires an ASCII logical user slot",
            ));
        }
        let block = super::snapshot::format_lesson_block(snapshot, &binding.stage)?;
        if final_user.matches(&block).count() != 1 {
            return Err(super::record::refusal(
                "final editor user slot has no unique complete frozen lesson block",
            ));
        }
        let prepared = PreparedLessonContext {
            store: Arc::clone(self),
            skill_id: plan.spec().pins.skill_id.clone(),
            binding,
            refs: snapshot.entries.iter().map(|entry| entry.reference.clone()).collect(),
            snapshot_sha256: super::record::digest(
                &serde_json::to_vec(snapshot).map_err(|_| super::record::refusal("lesson snapshot encoding failed"))?,
            ),
            block_sha256: super::record::digest(block.as_bytes()),
            block,
            expected_user_slot_sha256: super::record::digest(final_user.as_bytes()),
            expected_user_slot_bytes: u64::try_from(final_user.len())
                .map_err(|_| super::record::refusal("lesson user size overflow"))?,
            placement: LessonPlacement::FirstSoleUserText,
            plan: plan.clone(),
        };
        Ok(Some(Arc::new(prepared)))
    }

    fn validate_prepared_lesson_locked(
        &self,
        plan: &crate::skills::lifecycle::experiment::FrozenExperiment,
        binding: &ExperimentRunBinding,
        actual_model: &ModelBinding,
        now: DateTime<Utc>,
    ) -> Result<(), LifecycleError> {
        use crate::skills::lifecycle::experiment::{ExperimentStageKind, StageState};
        let refuse = super::record::refusal;
        plan.ensure_live(now).map_err(|_| refuse("lesson experiment expired"))?;
        let pins = &plan.spec().pins;
        if self.owner_scope() != &plan.spec().owner_scope
            || actual_model != &pins.model
            || binding.experiment_sha256 != plan.digest()
            || !plan.spec().stages.iter().any(|stage| {
                stage.id == binding.stage
                    && stage.input_sha256 == binding.input_sha256
                    && stage.kind == ExperimentStageKind::ModelProposal
            })
        {
            return Err(refuse("lesson actual owner, model or editor binding changed"));
        }
        let journal = self
            .read_experiment(&plan.spec().id, plan.digest())
            .map_err(|_| refuse("lesson admitted experiment cannot be read"))?;
        if journal.state.cancelled || !matches!(journal.state.stages.get(&binding.stage), Some(StageState::Dispatched))
        {
            return Err(refuse("lesson editor is not currently dispatched"));
        }
        let baseline = self
            .active_binding(&pins.skill_id)?
            .ok_or_else(|| refuse("lesson baseline missing"))?;
        if !baseline.enabled
            || baseline.revision_id != pins.baseline_revision
            || baseline.record_digest != pins.baseline_record_digest
            || baseline.generation != pins.baseline_generation
        {
            return Err(refuse("lesson active baseline changed"));
        }
        let snapshot = pins
            .lesson_snapshot
            .as_ref()
            .ok_or_else(|| refuse("lesson snapshot missing"))?;
        let suite = self
            .read_suite(&pins.suite_digest)
            .map_err(|_| refuse("lesson suite cannot be verified"))?;
        if suite
            .suite()
            .cases
            .iter()
            .any(|case| case.task_family != snapshot.task_family)
        {
            return Err(refuse("lesson task family changed"));
        }
        let compatibility =
            self.lesson_profile_compatibility(actual_model, &pins.policy_sha256, &pins.binary_sha256)?;
        self.validate_lesson_snapshot_locked(
            snapshot,
            &pins.skill_id,
            &pins.baseline_revision,
            &pins.baseline_record_digest,
            &compatibility,
            &suite.lesson_root_digests(),
            now,
        )
    }
}
