//! Publication of an operator-reviewed executable bundle (SL11).
//!
//! The only way a revision with scripts or executable tools enters the store.
//! Migration only plans a legacy executable directory
//! (`ExecutableNeedsReview`); the operator publishes it through this function
//! like any other reviewed directory, and execution refuses a revision whose
//! lineage has no such review. The caller names the complete bundle hash the
//! operator reviewed; the directory must hash to exactly that, pass
//! [`review_executable_bundle`], and the revision is written with `Operator`
//! publication provenance (model authorship remains in the hashed proposal record)
//! and published through the ordinary generation-checked
//! transaction, so a concurrent writer is rejected and rollback, pinning and
//! the compatibility projection behave as for any other revision.

use super::bundle::Bundle;
use super::executable::{ExecutableError, review_executable_bundle, review_receipt_id};
use super::manifest::validate_manifest_strict;
use super::publish::PublishRequest;
use super::store::SkillLifecycleStore;
use super::types::{NewRevision, RevisionId, SkillId, SourceKind};
use super::{LifecycleError, TransactionId};
use std::path::Path;

/// What an executable import published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutableImport {
    pub skill_id: SkillId,
    pub revision_id: RevisionId,
    pub generation: u64,
    /// The revision this one replaced as active, when the skill was managed.
    pub replaced: Option<RevisionId>,
}

impl SkillLifecycleStore {
    /// Publishes the executable bundle in `source`, which the operator
    /// reviewed as `reviewed`. A skill whose display name is already bound is
    /// revised (from its current generation, keeping its enabled state);
    /// otherwise a new skill is created under the id of the directory name.
    ///
    /// Raw store write with no lifecycle mode or grant; `pub(in crate::skills)`
    /// and reached only through `SkillsLifecycleService::import_executable`.
    ///
    /// Blocking: waits for locks and does file I/O.
    pub(in crate::skills) fn import_reviewed_executable(
        &self,
        source: &Path,
        reviewed: &RevisionId,
    ) -> Result<ExecutableImport, ExecutableError> {
        let bundle = Bundle::from_dir(source).map_err(LifecycleError::from)?;
        if bundle.revision_id() != reviewed {
            return Err(ExecutableError::NotReviewed {
                reviewed: reviewed.to_string(),
                found: bundle.revision_id().to_string(),
            });
        }
        review_executable_bundle(&bundle)?;
        let info = validate_manifest_strict(&bundle).map_err(LifecycleError::from)?;
        let (skill_id, parent, expected_generation, enabled) = match self.find_binding_by_name(&info.name)? {
            Some((binding, _)) => (
                binding.skill_id,
                Some(binding.revision_id),
                binding.generation,
                binding.enabled,
            ),
            None => {
                let directory = source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| ExecutableError::Stage("the source directory has no UTF-8 name".to_string()))?;
                (SkillId::from_legacy_dir(directory)?, None, 0, true)
            }
        };
        let mut new = NewRevision::new(skill_id.clone(), SourceKind::Operator);
        new.parent_revision_id.clone_from(&parent);
        new.review_receipt_ids = vec![review_receipt_id(bundle.revision_id())];
        let written = self.write_revision(&new, &bundle)?;
        let outcome = self.publish(&PublishRequest {
            transaction_id: TransactionId::generate(),
            skill_id: skill_id.clone(),
            candidate: written.revision.revision_id.clone(),
            candidate_record_digest: written.record_digest,
            expected_generation,
            enabled,
        })?;
        Ok(ExecutableImport {
            skill_id,
            revision_id: written.revision.revision_id,
            generation: outcome.entry.new_generation,
            replaced: parent,
        })
    }
}
