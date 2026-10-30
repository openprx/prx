//! Service policy entry for monotonic owner-local adoption state markers.
use super::{ServiceError, SkillsLifecycleService};
use crate::skills::lifecycle::LessonRef;

impl SkillsLifecycleService {
    /// True adds a retired/invalidated marker; false does not revive history.
    pub fn set_adoption_status(
        &self,
        reference: &LessonRef,
        retired: bool,
        invalidated: bool,
    ) -> Result<(), ServiceError> {
        self.require_enabled()?;
        Ok(self.store.set_adoption_status(reference, retired, invalidated)?)
    }
}
