//! Adopted-history data contracts. Serializable pins are untrusted input;
//! only this owner's verified factory may construct prepared guidance.
mod context;
mod lookup;
mod profile;
mod proof;
mod record;
mod retention;
mod snapshot;
mod status;
pub use context::LessonRef;
#[cfg(test)]
pub(crate) use context::{LessonCompatibility, LessonSnapshotEntry};
pub(crate) use context::{LessonLimits, LessonPlacement, LessonSnapshot, PreparedLessonContext};
pub(crate) use lookup::LessonQuery;
pub use proof::RecordAdoptionRequest;

pub(in crate::skills::lifecycle) use snapshot::format_lesson_block;

#[cfg(test)]
pub(crate) use lookup::arm_lookup_wall_stall;
#[cfg(test)]
pub(crate) use lookup::worker_clock;
#[cfg(test)]
pub(crate) use profile::LessonFixtureProfileGuard;
