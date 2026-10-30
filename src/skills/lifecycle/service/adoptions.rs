//! Production annotation entry point. Exact historical proof only; this method
//! never calls publication, recovery, projection repair, a model or a tool.
use super::SkillsLifecycleService;
use crate::skills::lifecycle::LessonRef;
use crate::skills::lifecycle::adoption::RecordAdoptionRequest;
use anyhow::{Result, ensure};

impl SkillsLifecycleService {
    /// Record one already-Committed adoption using only trusted owner evidence.
    /// No current relaxed policy or caller-supplied compatibility is accepted.
    pub fn record_adoption(
        &self,
        request: &RecordAdoptionRequest,
        config: &crate::config::Config,
    ) -> Result<LessonRef> {
        self.require_enabled()?;
        let experiment = request.experiment()?;
        let pin = request.gate_pin()?;
        // ProposalLock already borrows the existing shared store retention
        // barrier. Downstream helpers acquire no recursive store lock.
        let owner = self.store.lock_proposals()?;
        let journal = self.store.read_experiment(&experiment, &request.experiment_sha256)?;
        let history = self.verify_gate_review_witness(&experiment, &request.experiment_sha256, &pin)?;
        let origin = &journal.plan.spec().pins;
        let compatibility =
            self.store
                .lesson_profile_compatibility(&origin.model, &origin.policy_sha256, &origin.binary_sha256)?;
        // D alone revalidates its full sealed historical witness. These opaque
        // values cannot be reconstructed from a raw gate boolean or JSON blob.
        let repeated = self.verify_gate_review_witness(&experiment, &request.experiment_sha256, &pin)?;
        ensure!(
            history.pin() == repeated.pin()
                && history.binding() == repeated.binding()
                && history.subject() == repeated.subject()
                && history.physical() == repeated.physical()
                && history.verification_window() == repeated.verification_window()
                && history.gate_decided_at() == repeated.gate_decided_at()
                && history.gate_expires_at() == repeated.gate_expires_at(),
            "gate history changed during adoption proof"
        );
        ensure!(
            self.experiment_qualified(&journal)?,
            "origin experiment is not historically qualified"
        );
        self.store
            .record_committed_adoption_locked(&owner, request, &journal, &repeated, &compatibility, config)
    }
}
