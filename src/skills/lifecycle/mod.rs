//! Skill lifecycle: immutable, content-addressed skill revisions in an
//! owner-scoped store (SL01). Publication, runtime binding and evaluation
//! are separate packages and are not part of this module yet.

mod binding;
mod bundle;
mod manifest;
mod migrate;
mod store;
mod types;

#[cfg(test)]
mod tests;

pub use binding::{
    MAX_ACTIVE_BINDINGS, MAX_TASK_PINS, ManagedSkillRead, read_managed_resource, release_task_pins, task_pin,
};
pub use bundle::{
    BUNDLE_FORMAT_VERSION, Bundle, BundleEntry, BundleError, BundleFile, MAX_BUNDLE_COMPONENT_BYTES, MAX_BUNDLE_DEPTH,
    MAX_BUNDLE_FILE_BYTES, MAX_BUNDLE_FILES, MAX_BUNDLE_PATH_BYTES, MAX_BUNDLE_TOTAL_BYTES,
};
pub use manifest::{ManifestError, ManifestInfo, validate_manifest};
pub use migrate::{MigrationAction, MigrationEntry, MigrationPlan, MigrationReport, apply_migration, plan_migration};
pub use store::{SkillLifecycleStore, VerifiedRevision, WriteOutcome, record_digest};
pub use types::{
    ActiveSkillBinding, LifecycleError, NewRevision, OwnerScope, PinnedSkillRevision, RevisionId, SCHEMA_VERSION,
    SkillId, SkillRevision, SourceKind,
};
