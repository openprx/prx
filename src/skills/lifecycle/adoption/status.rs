//! Owner-local monotonic retirement/invalidation. This is not a pruner and
//! never changes immutable adopted history, expiry or publication state.
use super::context::LessonRef;
use super::record::{INDEX_CAP, RECORD_CAP, read_bounded_bytes, refusal};
use crate::skills::lifecycle::{LifecycleError, SkillLifecycleStore};
use std::time::Duration;

impl SkillLifecycleStore {
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
