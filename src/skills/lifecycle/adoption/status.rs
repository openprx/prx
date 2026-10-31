//! Owner-local monotonic retirement/invalidation. This is not a pruner and
//! never changes immutable adopted history, expiry or publication state.
use super::context::LessonRef;
use super::record::{INDEX_CAP, RECORD_CAP, read_bounded_bytes, refusal};
use crate::skills::lifecycle::{LifecycleError, SkillLifecycleStore};
use std::time::Duration;

impl SkillLifecycleStore {
    /// Read historical source exclusions for a frozen optimizer pool. This does
    /// not re-query lessons, test present eligibility or construct guidance.
    /// Both source exclusions and parent experiment pins from the same bounded
    /// authenticated reads; callers must still verify each historical root.
    pub(in crate::skills::lifecycle) fn study_snapshot_provenance(
        &self,
        snapshot: &super::context::LessonSnapshot,
    ) -> Result<(Vec<String>, Vec<(String, String)>), LifecycleError> {
        let mut budget = super::lookup::LookupBudget::new(snapshot.limits)?;
        let root_cap =
            crate::skills::lifecycle::suite::MAX_SUITE_CASES * (crate::skills::lifecycle::suite::MAX_CASE_FIXTURES + 1);
        if snapshot.schema_version != 1
            || snapshot.owner_scope != *self.owner_scope()
            || snapshot.entries.len() > usize::try_from(snapshot.limits.lessons).unwrap_or(usize::MAX)
            || snapshot.exclusions.len() > root_cap
            || snapshot
                .exclusions
                .iter()
                .any(|root| !super::record::valid_digest(root))
            || snapshot.exclusions.windows(2).any(|pair| pair.first() >= pair.get(1))
        {
            return Err(refusal("study lesson snapshot owner or source bounds differ"));
        }
        let _store = self.lock_store(true, Duration::from_millis(snapshot.limits.elapsed_ms))?;
        let mut roots: std::collections::BTreeSet<_> = snapshot.exclusions.iter().cloned().collect();
        let mut experiments = std::collections::BTreeSet::new();
        for entry in &snapshot.entries {
            budget.examine()?;
            let path = self.adoption_record_path(&entry.reference.adoption_id)?;
            let bytes = read_bounded_bytes(&path, snapshot.limits.record_bytes.min(RECORD_CAP))?;
            budget.charge(u64::try_from(bytes.len()).map_err(|_| refusal("source record size overflow"))?)?;
            let record = self.decode_adoption(&bytes, &entry.reference)?;
            let barrier = read_bounded_bytes(&self.adoption_retention_barrier_path(&record)?, RECORD_CAP)?;
            budget.charge(u64::try_from(barrier.len()).map_err(|_| refusal("source barrier size overflow"))?)?;
            self.check_adoption_retention_barrier(&record, &barrier)?;
            if record.task_family != snapshot.task_family
                || record.evidence_pin_sha256 != entry.evidence_pin_sha256
                || record.excluded_roots.len() > root_cap
                || record
                    .excluded_roots
                    .iter()
                    .any(|root| !super::record::valid_digest(root))
            {
                return Err(refusal(
                    "study lesson source differs from authenticated adopted history",
                ));
            }
            crate::skills::lifecycle::experiment::ExperimentId::parse(&record.experiment_id)
                .map_err(|error| refusal(&error.to_string()))?;
            if !super::record::valid_digest(&record.experiment_sha256) {
                return Err(refusal("historical adoption source experiment digest is invalid"));
            }
            experiments.insert((record.experiment_id, record.experiment_sha256));
            roots.extend(record.excluded_roots);
        }
        budget.check()?;
        Ok((roots.into_iter().collect(), experiments.into_iter().collect()))
    }
    /// Inspect only the finite families of the immutable origin suite. One
    /// shared-store -> skill lock protects existing sealed indices and records;
    /// no directory scan, new registry, proof construction or status mutation.
    pub(in crate::skills::lifecycle) fn product_adoptions(
        &self,
        expected: &crate::skills::lifecycle::ExperimentFinalExpectation,
        transaction: &str,
    ) -> Result<Vec<crate::skills::lifecycle::ExperimentAdoptionStatusObservation>, LifecycleError> {
        let suite = self
            .read_suite(&expected.suite_sha256)
            .map_err(|error| refusal(&error.to_string()))?;
        if suite.suite().owner_scope != *self.owner_scope() || suite.suite().skill_id != expected.skill_id {
            return Err(refusal("adoption origin suite owner/skill mismatch"));
        }
        let families: std::collections::BTreeSet<_> = suite
            .suite()
            .cases
            .iter()
            .map(|case| case.task_family.as_str())
            .collect();
        if families.len() > crate::skills::lifecycle::suite::MAX_SUITE_CASES {
            return Err(refusal("adoption observation exceeds the origin family bound"));
        }
        let _store = self.lock_store(true, Duration::from_secs(5))?;
        let _skill = self.lock_skill(&expected.skill_id, Duration::from_secs(5))?;
        let mut records = Vec::new();
        for family in families {
            let (index, _) = self
                .read_adoption_index_locked(family, &expected.skill_id)?
                .ok_or_else(|| refusal("origin family has no authenticated index; adoption absence is unproved"))?;
            let id = self.exact_adoption_id(&expected.proposal_id, transaction, family)?;
            let Some(entry) = index.entries.iter().find(|entry| entry.reference.adoption_id == id) else {
                let path = self.adoption_record_path(&id)?;
                match std::fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(LifecycleError::io(&path, error)),
                    Ok(_) => {
                        return Err(refusal(
                            "unindexed adoption artifact requires original owner reconciliation",
                        ));
                    }
                }
            };
            let path = self.adoption_record_path(&id)?;
            let record = self.decode_adoption(&read_bounded_bytes(&path, RECORD_CAP)?, &entry.reference)?;
            let barrier = read_bounded_bytes(&self.adoption_retention_barrier_path(&record)?, RECORD_CAP)?;
            self.check_adoption_retention_barrier(&record, &barrier)?;
            if record.proposal_id != expected.proposal_id
                || record.experiment_id != expected.handle.experiment_id.as_str()
                || record.experiment_sha256 != expected.handle.plan_sha256
                || record.publication_tx != transaction
                || record.task_family != family
                || record.skill_id != expected.skill_id
            {
                return Err(refusal(
                    "indexed adoption differs from the exact experiment publication",
                ));
            }
            records.push(crate::skills::lifecycle::ExperimentAdoptionStatusObservation {
                reference: entry.reference.clone(),
                handle: expected.handle.clone(),
                owner_scope: self.owner_scope().to_string(),
                skill_id: record.skill_id,
                task_family: record.task_family,
                retired: entry.retired,
                invalidated: entry.invalidated,
                expires_at: record.expires_at,
            });
        }
        Ok(records)
    }
    /// Bounded read-only status of an authentic annotation. Pure observation,
    /// never a proof constructor, revival or publication operation.
    pub(in crate::skills::lifecycle) fn product_adoption_status(
        &self,
        reference: &LessonRef,
    ) -> Result<crate::skills::lifecycle::ExperimentAdoptionStatusObservation, LifecycleError> {
        let _store = self.lock_store(true, Duration::from_secs(5))?;
        let path = self.adoption_record_path(&reference.adoption_id)?;
        let record = self.decode_adoption(&read_bounded_bytes(&path, RECORD_CAP)?, reference)?;
        let barrier = read_bounded_bytes(&self.adoption_retention_barrier_path(&record)?, RECORD_CAP)?;
        self.check_adoption_retention_barrier(&record, &barrier)?;
        let _skill = self.lock_skill(&record.skill_id, Duration::from_secs(5))?;
        let (index, _) = self
            .read_adoption_index_locked(&record.task_family, &record.skill_id)?
            .ok_or_else(|| refusal("adoption has no authenticated owner index"))?;
        let descriptor = index
            .entries
            .iter()
            .find(|entry| entry.reference == *reference)
            .ok_or_else(|| refusal("the exact adoption reference is not indexed"))?;
        Ok(crate::skills::lifecycle::ExperimentAdoptionStatusObservation {
            reference: reference.clone(),
            handle: crate::skills::lifecycle::ExperimentProductHandle {
                experiment_id: crate::skills::lifecycle::experiment::ExperimentId::parse(&record.experiment_id)
                    .map_err(|error| refusal(&error.to_string()))?,
                plan_sha256: record.experiment_sha256,
            },
            owner_scope: self.owner_scope().to_string(),
            skill_id: record.skill_id,
            task_family: record.task_family,
            retired: descriptor.retired,
            invalidated: descriptor.invalidated,
            expires_at: record.expires_at,
        })
    }
    /// True flags add permanent markers; false leaves the existing flag alone.
    /// This standalone entry point must not run under a shared handoff guard:
    /// it acquires store EXCLUSIVE before the actual skill lock, with no upgrade.
    pub(crate) fn set_adoption_status(
        &self,
        reference: &LessonRef,
        retired: bool,
        invalidated: bool,
    ) -> Result<(), LifecycleError> {
        let _store = self.lock_store(false, Duration::from_secs(5))?;
        let record_path = self.adoption_record_path(&reference.adoption_id)?;
        let record = self.decode_adoption(&read_bounded_bytes(&record_path, RECORD_CAP)?, reference)?;
        let barrier = read_bounded_bytes(&self.adoption_retention_barrier_path(&record)?, RECORD_CAP)?;
        self.check_adoption_retention_barrier(&record, &barrier)?;
        let _skill = self.lock_skill(&record.skill_id, Duration::from_secs(5))?;
        let (mut index, _) = self
            .read_adoption_index_locked(&record.task_family, &record.skill_id)?
            .ok_or_else(|| refusal("adoption status has no exact owner index"))?;
        let entry = index
            .entries
            .iter_mut()
            .find(|entry| entry.reference.adoption_id == reference.adoption_id)
            .ok_or_else(|| refusal("adoption status reference is not indexed"))?;
        if entry.reference != *reference {
            return Err(refusal("adoption status reference differs from its sealed index"));
        }
        let next_retired = entry.retired || retired;
        let next_invalidated = entry.invalidated || invalidated;
        if next_retired == entry.retired && next_invalidated == entry.invalidated {
            return Ok(());
        }
        entry.retired = next_retired;
        entry.invalidated = next_invalidated;
        let index_path = self.adoption_index_path(&record.task_family, &record.skill_id)?;
        let bytes = self.adoption_sealed_bytes(&index, &index_path, b"prx-adoption-index-v1")?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > INDEX_CAP {
            return Err(refusal("adoption status exceeds the bounded index capacity"));
        }
        self.write_atomic(&record.skill_id, &index_path, &bytes)?;
        if read_bounded_bytes(&index_path, INDEX_CAP)? != bytes {
            return Err(refusal("adoption status durable readback differs from its update"));
        }
        Ok(())
    }
}
