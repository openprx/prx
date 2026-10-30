//! Finite owner/family/skill lookup. Eligibility precedes output limiting;
//! exhausted count/bytes/deadline never returns a partial usable snapshot.
use super::context::{LessonCompatibility, LessonLimits, LessonSnapshot, LessonSnapshotEntry, LessonSnapshotStats};
use super::record::{AdoptionRecord, INDEX_CAP, digest, read_bounded_bytes, refusal, valid_digest};
use crate::skills::lifecycle::{LifecycleError, OwnerScope, RevisionId, SkillId, SkillLifecycleStore};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LessonQuery {
    pub owner_scope: OwnerScope,
    pub task_family: String,
    pub skill_id: SkillId,
    pub baseline_revision: RevisionId,
    pub baseline_record_sha256: String,
    pub compatibility: LessonCompatibility,
    pub excluded_roots: Vec<String>,
    pub limits: LessonLimits,
    pub planned_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}
impl LessonLimits {
    pub(super) fn validate(&self) -> Result<(), LifecycleError> {
        let cap = Self::default();
        if self.examined == 0
            || self.examined > cap.examined
            || self.index_bytes == 0
            || self.index_bytes > cap.index_bytes
            || self.record_bytes == 0
            || self.record_bytes > cap.record_bytes
            || self.decoded_bytes == 0
            || self.decoded_bytes > cap.decoded_bytes
            || self.lessons == 0
            || self.lessons > cap.lessons
            || self.item_bytes == 0
            || self.item_bytes > cap.item_bytes
            || self.context_bytes == 0
            || self.context_bytes > cap.context_bytes
            || self.context_token_upper_bound == 0
            || self.context_token_upper_bound > cap.context_token_upper_bound
            || self.elapsed_ms == 0
            || self.elapsed_ms > cap.elapsed_ms
        {
            return Err(refusal("lesson query limits exceed the frozen supported bounds"));
        }
        Ok(())
    }
}

pub(super) struct LookupBudget {
    started: Instant,
    limits: LessonLimits,
    pub stats: LessonSnapshotStats,
}
impl LookupBudget {
    pub fn new(limits: LessonLimits) -> Result<Self, LifecycleError> {
        limits.validate()?;
        Ok(Self {
            started: Instant::now(),
            limits,
            stats: LessonSnapshotStats::default(),
        })
    }
    pub fn check(&mut self) -> Result<(), LifecycleError> {
        self.stats.elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if self.started.elapsed() > Duration::from_millis(self.limits.elapsed_ms) {
            return Err(refusal(
                "lesson lookup deadline exhausted; no partial snapshot is usable",
            ));
        }
        Ok(())
    }
    pub fn charge(&mut self, bytes: u64) -> Result<(), LifecycleError> {
        self.check()?;
        self.stats.decoded_bytes = self
            .stats
            .decoded_bytes
            .checked_add(bytes)
            .ok_or_else(|| refusal("lesson decode budget overflow"))?;
        if self.stats.decoded_bytes > self.limits.decoded_bytes {
            return Err(refusal("lesson decode budget exhausted"));
        }
        self.check()
    }
    pub fn examine(&mut self) -> Result<(), LifecycleError> {
        self.check()?;
        self.stats.examined = self
            .stats
            .examined
            .checked_add(1)
            .ok_or_else(|| refusal("lesson examined budget overflow"))?;
        if self.stats.examined > self.limits.examined {
            return Err(refusal("lesson examined budget exhausted"));
        }
        Ok(())
    }
}

impl SkillLifecycleStore {
    /// Store-level data query. The service resolves actual target pins and
    /// registered task roots before calling it; no model/tool send occurs.
    pub(crate) fn query_adopted_lessons(&self, query: &LessonQuery) -> Result<LessonSnapshot, LifecycleError> {
        let mut budget = LookupBudget::new(query.limits)?;
        if query.owner_scope != *self.owner_scope()
            || query.expires_at <= query.planned_at
            || !valid_digest(&query.baseline_record_sha256)
            || query.excluded_roots.iter().any(|root| !valid_digest(root))
        {
            return Err(refusal("lesson query has invalid actual target pins"));
        }
        let _store = self.lock_store(true, Duration::from_millis(query.limits.elapsed_ms))?;
        budget.check()?;
        let index =
            self.read_adoption_index_bounded_locked(&query.task_family, &query.skill_id, query.limits.index_bytes)?;
        budget.check()?;
        let (entries, index_sha256) = if let Some((index, bytes)) = index {
            let bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if bytes > query.limits.index_bytes.min(INDEX_CAP) {
                return Err(refusal("lesson index decode budget exhausted"));
            }
            budget.stats.index_bytes = bytes;
            budget.charge(bytes)?;
            let hash = digest(&serde_json::to_vec(&index).map_err(|_| refusal("lesson index encoding failed"))?);
            (index.entries, hash)
        } else {
            (Vec::new(), digest(b"empty-adoption-index-v1"))
        };
        let mut selected = Vec::new();
        let mut expiry = query.expires_at;
        for descriptor in entries {
            if selected.len() >= usize::try_from(query.limits.lessons).unwrap_or(usize::MAX) {
                break;
            }
            budget.examine()?;
            if descriptor.retired || descriptor.invalidated {
                budget.stats.excluded += 1;
                continue;
            }
            let path = self.adoption_record_path(&descriptor.reference.adoption_id)?;
            budget.check()?;
            let bytes = read_bounded_bytes(&path, query.limits.record_bytes)?;
            budget.charge(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
            let record = self.decode_adoption(&bytes, &descriptor.reference)?;
            budget.check()?;
            let barrier = read_bounded_bytes(
                &self.adoption_retention_barrier_path(&record)?,
                query.limits.record_bytes,
            )?;
            budget.charge(u64::try_from(barrier.len()).unwrap_or(u64::MAX))?;
            self.check_adoption_retention_barrier(&record, &barrier)?;
            budget.check()?;
            if !eligible(&record, query) {
                budget.stats.excluded += 1;
                continue;
            }
            let guidance = record.source.render()?;
            if u64::try_from(guidance.len()).unwrap_or(u64::MAX) > query.limits.item_bytes {
                return Err(refusal("lesson guidance item budget exhausted"));
            }
            // Charge the known immutable bundle before the existing verifier
            // reads it; a mismatch is corruption, never an estimate or free I/O.
            let revision_metadata = self
                .root()
                .join("revisions")
                .join(record.skill_id.as_str())
                .join(record.adopted_revision.as_str())
                .join("revision.json");
            budget.check()?;
            let metadata_bytes = read_bounded_bytes(&revision_metadata, query.limits.record_bytes)?;
            budget.charge(u64::try_from(metadata_bytes.len()).unwrap_or(u64::MAX))?;
            let actual_bytes = super::snapshot::bounded_bundle_metadata(
                &self.bundle_dir(&record.skill_id, &record.adopted_revision),
                record.adopted_bundle_bytes,
                &mut budget,
            )?;
            if actual_bytes != record.adopted_bundle_bytes {
                return Err(refusal("adopted source size changed"));
            }
            budget.charge(actual_bytes)?;
            let revision = self.read_revision(
                &record.skill_id,
                &record.adopted_revision,
                Some(&record.adopted_record_sha256),
            )?;
            let observed = revision
                .bundle
                .files()
                .iter()
                .try_fold(0_u64, |sum, file| {
                    sum.checked_add(u64::try_from(file.bytes().len()).ok()?)
                })
                .ok_or_else(|| refusal("adopted bundle size overflow"))?;
            budget.check()?;
            if observed != record.adopted_bundle_bytes {
                return Err(refusal("adopted source size changed"));
            }
            expiry = expiry.min(record.expires_at);
            selected.push(LessonSnapshotEntry {
                reference: descriptor.reference,
                evidence_pin_sha256: record.evidence_pin_sha256,
                guidance_sha256: digest(guidance.as_bytes()),
                sanitized_guidance: guidance,
                expires_at: record.expires_at,
            });
        }
        budget.check()?;
        let query_sha256 = digest(&serde_json::to_vec(query).map_err(|_| refusal("lesson query encoding failed"))?);
        Ok(LessonSnapshot {
            schema_version: 1,
            owner_scope: self.owner_scope().clone(),
            task_family: query.task_family.clone(),
            query_sha256,
            index_sha256,
            compatibility: query.compatibility.clone(),
            exclusions: query.excluded_roots.clone(),
            limits: query.limits,
            stats: budget.stats,
            entries: selected,
            formatter_version: "adopted-skill-ascii-v1".into(),
            planned_at: query.planned_at,
            expires_at: expiry,
        })
    }
}

fn eligible(record: &AdoptionRecord, query: &LessonQuery) -> bool {
    let adopted_source = (&record.adopted_revision, &record.adopted_record_sha256);
    let target_baseline = (&query.baseline_revision, &query.baseline_record_sha256);
    record.owner_scope == query.owner_scope
        && record.skill_id == query.skill_id
        && record.task_family == query.task_family
        && adopted_source == target_baseline
        && record.compatibility == query.compatibility
        && record.expires_at > query.planned_at
        && !record
            .excluded_roots
            .iter()
            .any(|root| query.excluded_roots.contains(root))
}
