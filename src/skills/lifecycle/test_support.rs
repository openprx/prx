//! Fixtures for tests outside `src/skills` that need an operator revision
//! published. The raw store writes are `pub(in crate::skills)`, so the
//! compiler keeps every other module away from them; this is the one named
//! door, and it exists only in test builds.

use super::bundle::Bundle;
use super::journal::TransactionId;
use super::publish::PublishRequest;
use super::store::SkillLifecycleStore;
use super::types::{NewRevision, RevisionId, SkillId, SourceKind};

/// Writes `bundle` as a revision of `skill` and publishes it from
/// `expected_generation`, enabled. Returns the revision id.
///
/// # Panics
/// Panics when the store refuses either step; test fixtures only.
pub(crate) fn publish_revision(
    store: &SkillLifecycleStore,
    skill: &SkillId,
    kind: SourceKind,
    bundle: &Bundle,
    expected_generation: u64,
) -> RevisionId {
    let written = store
        .write_revision(&NewRevision::new(skill.clone(), kind), bundle)
        .expect("test: write revision");
    store
        .publish(&PublishRequest {
            transaction_id: TransactionId::generate(),
            skill_id: skill.clone(),
            candidate: written.revision.revision_id.clone(),
            candidate_record_digest: written.record_digest,
            expected_generation,
            enabled: true,
        })
        .expect("test: publish revision");
    written.revision.revision_id
}
