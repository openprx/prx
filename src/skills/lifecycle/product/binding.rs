//! Exact H2 result checks inside the existing proposal/publication owner.
use super::{ExperimentFinalExpectation, ExperimentProductHandle};
use crate::skills::lifecycle::experiment::{ExperimentEvent, ExperimentStageKind, StageState};
use crate::skills::lifecycle::{GateError, SkillLifecycleStore, SkillProposal};
use sha2::{Digest, Sha256};

pub(in crate::skills::lifecycle) fn experiment_proposal_request_id(
    plan: &str,
    stage: &str,
    controlled: bool,
) -> String {
    let mut hash = Sha256::new();
    hash.update(plan);
    hash.update(b"\0");
    hash.update(stage);
    let prefix = if controlled { "h2c-experiment-" } else { "experiment-" };
    format!("{prefix}{}", hex::encode(hash.finalize()))
}
fn require(condition: bool, message: &str) -> Result<(), GateError> {
    if condition {
        Ok(())
    } else {
        Err(GateError::Unbound(message.into()))
    }
}
impl SkillLifecycleStore {
    /// Called under the original proposal owner. Serialized pins do not supply
    /// the live source; only a real admitted stage or typed exact command does.
    pub(in crate::skills::lifecycle) fn require_controlled_proposal_source(
        &self,
        proposal: &SkillProposal,
        expected: Option<&ExperimentFinalExpectation>,
    ) -> Result<Option<crate::runtime::h2_entry::RunContinuation>, crate::skills::lifecycle::LifecycleError> {
        use crate::skills::lifecycle::LifecycleError;
        let refusal = |message: &str| LifecycleError::ProposalRefused(message.into());
        if !proposal.request_id.starts_with("h2c-experiment-") {
            return Ok(None);
        }
        let (id, digest) = if let Some(expected) = expected {
            self.require_experiment_publication_binding(proposal, Some(expected))
                .map_err(|error| refusal(&error.to_string()))?;
            (
                expected.handle.experiment_id.clone(),
                expected.handle.plan_sha256.clone(),
            )
        } else {
            crate::runtime::experiment_stage::current()
                .ok_or_else(|| refusal("controlled proposal review requires its real admitted stage or exact final"))?
                .verified_experiment_origin(self.owner_scope())
                .map_err(|error| refusal(&error.to_string()))?
        };
        let journal = self
            .read_experiment(&id, &digest)
            .map_err(|error| refusal(&error.to_string()))?;
        let envelope = journal
            .plan
            .spec()
            .controlled_admission
            .as_ref()
            .ok_or_else(|| refusal("controlled proposal has no authenticated original admission"))?;
        if proposal.owner_scope != *self.owner_scope()
            || proposal.skill_id != journal.plan.spec().pins.skill_id
            || !journal.plan.spec().stages.iter().any(|stage| {
                stage.kind == ExperimentStageKind::ModelProposal
                    && experiment_proposal_request_id(journal.plan.digest(), &stage.id, true) == proposal.request_id
            })
        {
            return Err(refusal(
                "controlled proposal is not an original stage of this exact root",
            ));
        }
        let current = crate::runtime::h2_entry::current_run_continuation()
            .ok_or_else(|| refusal("controlled proposal requires a live original-root command"))?;
        let bound = current
            .for_store_plan(
                &journal.plan,
                &envelope.pin,
                self.admission_workspace()
                    .map_err(|error| refusal(&error.to_string()))?,
                self.owner_scope().as_str(),
            )
            .map_err(|error| refusal(&error.to_string()))?;
        bound
            .ensure_store_admission(
                &envelope.pin,
                &journal.plan,
                self.admission_workspace()
                    .map_err(|error| refusal(&error.to_string()))?,
                self.owner_scope().as_str(),
            )
            .map_err(|error| refusal(&error.to_string()))?;
        Ok(Some(bound))
    }
    pub(in crate::skills::lifecycle) fn require_experiment_fresh_publication(
        &self,
        expected: &ExperimentFinalExpectation,
    ) -> Result<(), GateError> {
        let journal = self
            .read_experiment(&expected.handle.experiment_id, &expected.handle.plan_sha256)
            .map_err(|error| GateError::Unbound(error.to_string()))?;
        require(
            journal.state.qualified(&journal.plan),
            "experiment was cancelled or is no longer qualified",
        )
    }
    /// request_id is already immutable under proposal_id_of(owner, request_id).
    /// Any known marker tightens admission; a missing gate binding never falls
    /// back to ordinary proposal publication.
    pub(in crate::skills::lifecycle) fn require_experiment_publication_intent(
        &self,
        proposal: &SkillProposal,
        expected: Option<&ExperimentFinalExpectation>,
    ) -> Result<(), GateError> {
        let gate = self.recorded_gate_for(proposal)?;
        let marked = proposal.request_id.starts_with("experiment-")
            || proposal.request_id.starts_with("h2c-experiment-")
            || proposal.author.system_prompt_version == "h2-development-proposer-v1"
            || gate.as_ref().is_some_and(|gate| gate.decision.experiment.is_some());
        if marked && expected.is_none() {
            return Err(GateError::Unbound(
                "experiment-bound proposals require exact product publication; use skills lifecycle experiment product publish"
                    .into(),
            ));
        }
        if expected.is_some() {
            require(
                marked,
                "the expected experiment result does not name an experiment proposal",
            )?;
        }
        Ok(())
    }

    /// Must be called while the existing proposal owner is held. This verifies
    /// source-owned records, never the caller's claim of a verified final.
    pub(in crate::skills::lifecycle) fn require_experiment_publication_binding(
        &self,
        proposal: &SkillProposal,
        expected: Option<&ExperimentFinalExpectation>,
    ) -> Result<(), GateError> {
        self.require_experiment_publication_intent(proposal, expected)?;
        let Some(expected) = expected else { return Ok(()) };
        let actual = self.resolve_experiment_final(&expected.handle, &expected.gate_stage)?;
        require(actual == *expected, "the exact sealed experiment final result changed")?;
        require(
            proposal.owner_scope == *self.owner_scope()
                && proposal.proposal_id == expected.proposal_id
                && proposal.request_id
                    == experiment_proposal_request_id(
                        &expected.handle.plan_sha256,
                        &expected.proposal_stage,
                        proposal.request_id.starts_with("h2c-experiment-"),
                    )
                && proposal.skill_id == expected.skill_id
                && proposal.baseline_revision.as_ref() == Some(&expected.baseline_revision)
                && proposal.expected_generation == expected.baseline_generation
                && proposal.candidate_revision.as_ref() == Some(&expected.candidate_revision)
                && proposal.evaluation_epoch == expected.evaluation_epoch
                && proposal.evaluating_since == Some(expected.evaluating_since)
                && proposal.required_suite.as_deref() == Some(expected.suite_sha256.as_str()),
            "publication proposal no longer matches the sealed final subject",
        )?;
        if let Some(auth) = &proposal.authorization {
            require(
                (1..=crate::skills::lifecycle::MAX_PUBLICATION_ATTEMPTS).contains(&auth.attempt)
                    && auth.gate_id == expected.gate_id
                    && auth.gate_digest == expected.gate_sha256
                    && auth.candidate_record_digest == expected.candidate_record_sha256,
                "publication authorization differs from the exact experiment result",
            )?;
        }
        Ok(())
    }

    /// Pure immutable result resolution. Current eligibility and publication
    /// remain the existing authorizer/publisher's responsibility.
    pub(in crate::skills::lifecycle) fn resolve_experiment_final(
        &self,
        handle: &ExperimentProductHandle,
        gate_stage: &str,
    ) -> Result<ExperimentFinalExpectation, GateError> {
        let fail = |error: crate::skills::lifecycle::experiment::ExperimentError| GateError::Unbound(error.to_string());
        let journal = self
            .read_experiment(&handle.experiment_id, &handle.plan_sha256)
            .map_err(fail)?;
        require(
            journal.plan.spec().owner_scope == *self.owner_scope(),
            "experiment belongs to another owner",
        )?;
        let gates: Vec<_> = journal
            .plan
            .spec()
            .stages
            .iter()
            .filter(|stage| stage.kind == ExperimentStageKind::GateReview)
            .collect();
        let [gate] = gates.as_slice() else {
            return Err(GateError::Unbound("experiment has no unique final gate stage".into()));
        };
        require(gate.id == gate_stage, "experiment final gate stage differs")?;
        require(
            journal.state.stages.values().all(StageState::successful),
            "the experiment has incomplete, failed or uncleaned stages",
        )?;
        let (gate_id, digest) = journal
            .entries
            .iter()
            .find_map(|entry| match &entry.event {
                ExperimentEvent::Settled {
                    stage,
                    reference: Some(reference),
                    evidence_sha256,
                    successful: true,
                    ..
                } if stage == gate_stage => Some((reference.clone(), evidence_sha256.clone())),
                _ => None,
            })
            .ok_or_else(|| GateError::Unbound("the final gate has no sealed successful settlement".into()))?;
        let witness = self
            .read_gate_review(&handle.experiment_id, gate_stage, &digest)
            .map_err(fail)?;
        let pins = &journal.plan.spec().pins;
        require(
            witness.binding.owner_scope == *self.owner_scope()
                && witness.binding.experiment.experiment_sha256 == handle.plan_sha256
                && witness.binding.experiment.stage == gate_stage
                && witness.binding.experiment.input_sha256 == gate.input_sha256
                && witness.gate_id == gate_id
                && witness.subject.skill_id == pins.skill_id
                && witness.subject.baseline_revision.as_ref() == Some(&pins.baseline_revision)
                && witness.subject.expected_generation == pins.baseline_generation
                && witness.subject.suite_digest == pins.suite_digest,
            "final witness differs from the admitted owner, baseline or gate stage",
        )?;
        let proposal = self
            .read_proposal(&witness.subject.proposal_id)?
            .ok_or_else(|| GateError::Unbound("final proposal is missing".into()))?;
        require(
            proposal.request_id
                == experiment_proposal_request_id(
                    &handle.plan_sha256,
                    &witness.proposal_stage,
                    journal.plan.spec().controlled_admission.is_some(),
                ),
            "the final proposal is not the immutable admitted proposal stage",
        )?;
        let gate = self
            .recorded_gate_for(&proposal)?
            .ok_or_else(|| GateError::Unbound("the final gate decision is missing".into()))?;
        require(
            gate.decision.experiment.as_ref() == Some(&witness.binding)
                && gate.decision.record_id == witness.gate_id
                && gate.digest == witness.gate_digest
                && gate.decision.proposal_id == witness.subject.proposal_id
                && gate.decision.candidate_revision == witness.subject.candidate_revision
                && gate.decision.epoch == witness.subject.evaluation_epoch
                && gate.decision.expected_generation == witness.subject.expected_generation,
            "the final gate lost its immutable experiment/subject binding",
        )?;
        let candidate = self.read_revision(&pins.skill_id, &witness.subject.candidate_revision, None)?;
        require(
            candidate
                .revision
                .proposal_id
                .as_deref()
                .is_none_or(|id| id == witness.subject.proposal_id),
            "candidate revision belongs to another proposal",
        )?;
        Ok(ExperimentFinalExpectation {
            handle: handle.clone(),
            proposal_id: witness.subject.proposal_id,
            proposal_stage: witness.proposal_stage,
            skill_id: pins.skill_id.clone(),
            baseline_revision: pins.baseline_revision.clone(),
            baseline_record_sha256: pins.baseline_record_digest.clone(),
            baseline_generation: pins.baseline_generation,
            candidate_revision: witness.subject.candidate_revision,
            candidate_record_sha256: candidate.record_digest,
            evaluation_epoch: witness.subject.evaluation_epoch,
            evaluating_since: witness.subject.evaluating_since,
            suite_sha256: pins.suite_digest.clone(),
            gate_stage: gate_stage.into(),
            gate_witness_sha256: digest,
            gate_id,
            gate_sha256: witness.gate_digest,
        })
    }
}
