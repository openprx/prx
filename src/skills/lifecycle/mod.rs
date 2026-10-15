//! Skill lifecycle: immutable, content-addressed skill revisions in an
//! owner-scoped store (SL01). Publication, runtime binding and evaluation
//! are separate packages and are not part of this module yet. SL08 store level
//! adds the publication journal, generation CAS, rollback and recovery.

/// Test-only crash injection point; expands to nothing outside `cfg(test)`.
macro_rules! crash_point {
    ($name:expr) => {
        #[cfg(test)]
        {
            $crate::skills::lifecycle::publish::crash_hooks::check($name)?;
        }
    };
}

mod bundle;
mod generation;
mod journal;
mod manifest;
mod publish;
mod store;
mod types;

#[cfg(test)]
mod tests;

pub use bundle::{
    BUNDLE_FORMAT_VERSION, Bundle, BundleEntry, BundleError, BundleFile, MAX_BUNDLE_COMPONENT_BYTES, MAX_BUNDLE_DEPTH,
    MAX_BUNDLE_FILE_BYTES, MAX_BUNDLE_FILES, MAX_BUNDLE_PATH_BYTES, MAX_BUNDLE_TOTAL_BYTES,
};
pub use generation::ActiveBinding;
pub use journal::{JournalEntry, TransactionId, TransactionKind, TransactionState};
pub use manifest::{ManifestError, ManifestInfo, validate_manifest};
pub use publish::{PUBLICATION_LOCK_TIMEOUT, PublishRequest, RecoveryReport, RollbackRequest, TransactionOutcome};
pub use store::{SkillLifecycleStore, VerifiedRevision, WriteOutcome, record_digest};
pub use types::{
    LifecycleError, NewRevision, OwnerScope, RevisionId, SCHEMA_VERSION, SkillId, SkillRevision, SourceKind,
};
