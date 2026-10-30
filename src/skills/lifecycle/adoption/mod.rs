//! Adopted-history data contracts. Serializable pins are untrusted input;
//! only this owner's verified factory may construct prepared guidance.
#![allow(dead_code)]
mod context;
mod lookup;
mod profile;
mod proof;
mod record;
mod retention;
mod snapshot;
mod status;
#[allow(unused_imports)]
pub(crate) use context::{
    LessonCompatibility, LessonHandoffGuard, LessonLimits, LessonPlacement, LessonRef, LessonSnapshot,
    LessonSnapshotEntry, LessonSnapshotStats, PreparedLessonContext,
};
pub(crate) use lookup::LessonQuery;
pub(crate) use proof::RecordAdoptionRequest;

pub(in crate::skills::lifecycle) use snapshot::format_lesson_block;
