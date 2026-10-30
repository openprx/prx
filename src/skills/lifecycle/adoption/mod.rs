//! Adopted-history data contracts. Serializable pins are untrusted input;
//! only this owner's later verified factory may construct prepared guidance.
// The factory is intentionally absent in the small A/B interface commit.
#![allow(dead_code)]
mod context;
#[allow(unused_imports)]
pub(crate) use context::{
    LessonCompatibility, LessonHandoffGuard, LessonLimits, LessonPlacement, LessonRef, LessonSnapshot,
    LessonSnapshotEntry, LessonSnapshotStats, PreparedLessonContext,
};
