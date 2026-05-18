//! Compatibility re-exports for memory-write safety filtering.
//!
//! The filter lives in `memory::filter` because it now protects the primary
//! memory write path. Evolution still re-exports these types for historical
//! callers and for offline cleanup workflows.

#[allow(unused_imports)]
pub use crate::memory::filter::{
    ConflictChecker, MemorySafetyFilter, SafetyCheckResult, SafetyIssue, SafetyIssueKind, SourceMetadata,
};
