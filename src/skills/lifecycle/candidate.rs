//! The single writer of a proposal's candidate revision into the main store
//! (SL08 hard gate 1).
//!
//! `propose_revision` only stages the bundle under `proposals/<id>/candidate`.
//! Evaluation (SL06) and publication (SL08) read the candidate from the main
//! store, so something has to put it there, and exactly one function does:
//! [`SkillLifecycleStore::install_candidate`], called when a proposal enters
//! evaluation. The revision it writes names this proposal, its baseline as
//! parent and `Learned` provenance, so a revision of that content can never
//! be claimed by two proposals. Before any evaluation spend it refuses
//! content that another live proposal already carries and a revision that
//! already exists with other provenance.
//!
//! Rejection memory: a revision is content-addressed and immutable, and it
//! names the proposal that wrote it. Content that a proposal installed once is
//! therefore refused for every later proposal, also after the first was
//! cancelled, rejected or failed. That is intended: it is the same permanent
//! memory the rejection history keeps of a material change, and a new attempt
//! must propose different content.

use super::proposal::{ProposalStatus, SkillProposal};
use super::store::{SkillLifecycleStore, WriteOutcome};
use super::types::{LifecycleError, NewRevision, SourceKind};

impl SkillLifecycleStore {
    /// Writes the candidate of `proposal_id` into the main store with
    /// `proposal_id = this proposal`, `parent = baseline` and `Learned`
    /// provenance, after verifying the staged bundle against the proposal's
    /// candidate id. Idempotent for the same proposal.
    ///
    /// Requires a `SourceReviewed` (or, for a retry, `Inconclusive`) proposal
    /// with a passing source review. Refuses with
    /// [`LifecycleError::DuplicateCandidate`] when another live proposal of the
    /// same skill carries the identical candidate, and with
    /// [`LifecycleError::CandidateRevisionConflict`] when the revision exists
    /// with any other proposal id, parent or source kind.
    ///
    /// Blocking: takes the proposal lock and writes the store.
    pub(in crate::skills) fn install_candidate(&self, proposal_id: &str) -> Result<WriteOutcome, LifecycleError> {
        let _lock = self.lock_proposals()?;
        let proposal = self
            .read_proposal(proposal_id)?
            .ok_or_else(|| LifecycleError::ProposalRefused(format!("no such proposal {proposal_id}")))?;
        if !matches!(
            proposal.status,
            ProposalStatus::SourceReviewed | ProposalStatus::Inconclusive
        ) {
            return Err(LifecycleError::InvalidProposalTransition {
                proposal_id: proposal.proposal_id,
                from: proposal.status.as_str(),
                to: ProposalStatus::Evaluating.as_str(),
            });
        }
        let review = self.require_source_review(&proposal)?;
        let candidate = proposal
            .candidate_revision
            .clone()
            .ok_or_else(|| LifecycleError::ProposalRefused("the proposal has no staged candidate".to_string()))?;
        let bundle = self.read_candidate_bundle(&proposal.proposal_id)?;
        if bundle.revision_id() != &candidate {
            return Err(LifecycleError::Tampered {
                path: self.root().to_path_buf(),
                reason: "staged candidate does not hash to the proposal's candidate".to_string(),
            });
        }
        // The lock is held: nobody else can install or move a proposal now.
        let (proposals, _) = self.list_proposals_lenient()?;
        if let Some(other) = proposals.iter().find(|other| competes(&proposal, other)) {
            return Err(LifecycleError::DuplicateCandidate {
                proposal_id: proposal.proposal_id,
                other: other.proposal_id.clone(),
            });
        }
        let baseline = self.read_revision(&proposal.skill_id, &proposal.baseline_revision, None)?;
        let mut new = NewRevision::new(proposal.skill_id.clone(), SourceKind::Learned);
        new.parent_revision_id = Some(proposal.baseline_revision.clone());
        new.proposal_id = Some(proposal.proposal_id.clone());
        new.pattern_ids = proposal.pattern_ids.iter().map(ToString::to_string).collect();
        // The candidate may use what its baseline used and nothing more.
        new.capability_requirements = baseline.revision.capability_requirements;
        new.review_receipt_ids = vec![review.record_id];
        self.write_revision(&new, &bundle).map_err(|error| match error {
            LifecycleError::RevisionConflict { revision, field } => LifecycleError::CandidateRevisionConflict {
                reason: format!("the revision exists with other {field}"),
                revision,
            },
            other => other,
        })
    }
}

/// Whether `other` is a different live proposal of the same skill with the
/// identical candidate content that already entered evaluation (it has opened
/// an evaluation epoch). Two drafts of the same content do not compete until
/// one of them has claimed the revision; the revision's own proposal id
/// settles a race between two installs.
fn competes(proposal: &SkillProposal, other: &SkillProposal) -> bool {
    other.proposal_id != proposal.proposal_id
        && other.skill_id == proposal.skill_id
        && other.status.is_live()
        && other.evaluation_epoch > 0
        && other.candidate_revision.is_some()
        && other.candidate_revision == proposal.candidate_revision
}
