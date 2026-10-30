//! Actual stage provenance for private gate creation. None preserves legacy seals.
//! This binding records accounting ownership and grants no publication authority.
use super::{GateError, OwnerScope, SkillLifecycleStore};
use crate::skills::lifecycle::experiment::ExperimentRunBinding;
use serde::{Deserialize, Serialize};

/// The actual runtime that created the sealed decision, not a caller's claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateExperimentBinding {
    pub(crate) owner_scope: OwnerScope,
    pub(crate) experiment: ExperimentRunBinding,
    pub(crate) physical_scope_id: String,
}

impl SkillLifecycleStore {
    pub(super) fn current_gate_review_subject(&self) -> Result<Option<GateReviewSubject>, GateError> {
        crate::runtime::experiment_stage::current()
            .map(|runtime| {
                runtime
                    .gate_subject()
                    .ok_or_else(|| GateError::Unbound("GateReview has no immutable expected subject".into()))
            })
            .transpose()
    }
    pub(super) fn require_expected_gate_subject(
        &self,
        proposal: &super::SkillProposal,
        suite: &str,
        pins: &[super::EvaluationPin],
        expected: Option<&GateReviewSubject>,
    ) -> Result<(), GateError> {
        if let Some(expected) = expected {
            let mut given: Vec<_> = pins
                .iter()
                .map(|pin| (pin.evaluation_id.clone(), pin.receipt_digest.clone()))
                .collect();
            let mut required = expected.evaluations.clone();
            given.sort();
            required.sort();
            if given != required
                || proposal.proposal_id != expected.proposal_id
                || proposal.operation != expected.operation
                || proposal.skill_id != expected.skill_id
                || proposal.baseline_revision != expected.baseline_revision
                || proposal.candidate_revision.as_ref() != Some(&expected.candidate_revision)
                || proposal.expected_generation != expected.expected_generation
                || proposal.evaluation_epoch != expected.evaluation_epoch
                || proposal.evaluating_since != Some(expected.evaluating_since)
                || proposal.required_suite.as_deref() != Some(expected.suite_digest.as_str())
                || suite != expected.suite_digest
            {
                return Err(GateError::Unbound(
                    "actual gate capture differs from the stage's immutable expected subject".into(),
                ));
            }
        }
        Ok(())
    }
    pub(super) fn current_gate_experiment_binding(&self) -> Result<Option<GateExperimentBinding>, GateError> {
        crate::runtime::experiment_stage::current()
            .map(|runtime| {
                runtime
                    .gate_record_binding(self.owner_scope())
                    .map_err(|error| GateError::Unbound(error.to_string()))
            })
            .transpose()
    }
}

/// Immutable subject reconstructed from the admitted proposal and the two
/// sealed same-proposal evaluation bindings, never today's mutable epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GateReviewSubject {
    pub proposal_id: String,
    pub suite_digest: String,
    pub evaluations: Vec<(String, String)>,
    pub operation: super::ProposalOperation,
    pub skill_id: super::SkillId,
    pub baseline_revision: Option<super::RevisionId>,
    pub candidate_revision: super::RevisionId,
    pub expected_generation: u64,
    pub evaluation_epoch: u32,
    pub evaluating_since: chrono::DateTime<chrono::Utc>,
}

impl SkillLifecycleStore {
    /// Historical private completion only. Never consults current proposal
    /// status/epoch/baseline, rescans current siblings, calls a judge, or grants
    /// a current VerifiedGate/publication authority.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::skills::lifecycle) fn verify_historical_gate_review(
        &self,
        gate_id: &str,
        digest: &str,
        binding: &GateExperimentBinding,
        subject: &GateReviewSubject,
        policy: &super::GatePolicy,
        suite_digest: &str,
        pins: &[super::EvaluationPin],
        judge_identity: &str,
        physical: &crate::skills::lifecycle::experiment::GateReviewPhysical,
        dispatched_at: chrono::DateTime<chrono::Utc>,
        verified_started_at: chrono::DateTime<chrono::Utc>,
        verified_finished_at: chrono::DateTime<chrono::Utc>,
        settled_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>), GateError> {
        use super::{EvalSplit, GATE_IDENTITY, GateDecision, SCHEMA_VERSION};
        let refuse = |message: &str| GateError::Tampered(message.into());
        policy.validate()?;
        let mut given: Vec<_> = pins
            .iter()
            .map(|pin| (pin.evaluation_id.clone(), pin.receipt_digest.clone()))
            .collect();
        let mut expected_pins = subject.evaluations.clone();
        given.sort();
        expected_pins.sort();
        if given != expected_pins {
            return Err(refuse(
                "historical gate evaluation inputs differ from its admitted subject",
            ));
        }
        let stored = self
            .read_gate(gate_id)?
            .ok_or_else(|| refuse("historical gate is missing"))?;
        let gate = &stored.decision;
        if stored.digest != digest
            || gate.experiment.as_ref() != Some(binding)
            || binding.owner_scope != *self.owner_scope()
            || gate.owner_scope != binding.owner_scope
            || gate.record_id != super::gate_id_of(self.owner_scope(), &subject.proposal_id, subject.evaluation_epoch)
            || gate.schema_version != SCHEMA_VERSION
            || gate.review_identity != GATE_IDENTITY
            || gate.proposal_id != subject.proposal_id
            || gate.operation != subject.operation
            || gate.skill_id != subject.skill_id
            || gate.baseline_revision != subject.baseline_revision
            || gate.candidate_revision != subject.candidate_revision
            || gate.expected_generation != subject.expected_generation
            || gate.epoch != subject.evaluation_epoch
            || gate.policy != *policy
            || gate.suite_digest != suite_digest
            || subject.suite_digest != suite_digest
            || subject.evaluation_epoch == 0
        {
            return Err(refuse(
                "historical gate subject, owner, policy or actual stage binding changed",
            ));
        }
        // The sealed complete owner snapshot must cover the judge's actual
        // per-call physical tally, including attempts absent from a reply.
        let judge = gate
            .judge
            .as_ref()
            .ok_or_else(|| refuse("historical gate has no physical judge tally"))?;
        if physical.calls == 0
            || physical.calls != u64::from(judge.usage.physical_requests)
            || physical.settled_calls != physical.calls
            || physical.settled_calls < u64::from(judge.usage.settled_requests)
            || physical
                .attempt_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != usize::try_from(physical.calls).unwrap_or(usize::MAX)
        {
            return Err(refuse(
                "historical stage owner does not cover the judge's physical tally",
            ));
        }
        if gate.expires_at != super::expiry_of(gate.decided_at, policy) {
            return Err(refuse("historical gate expiry differs from its recorded policy"));
        }
        // The witness brackets the actual current verify_gate call. A later
        // stage settlement may follow expiry; it is not a new authorization.
        if dispatched_at > gate.decided_at
            || gate.decided_at > verified_started_at
            || verified_started_at > verified_finished_at
            || verified_finished_at > settled_at
            || verified_finished_at > gate.expires_at
        {
            return Err(GateError::InsufficientTemporalEvidence(
                "the sealed actual gate-verification window is missing or inconsistent".into(),
            ));
        }
        let suite = self.read_suite(suite_digest)?;
        if suite.suite().skill_id != subject.skill_id
            || suite.suite().owner_scope != *self.owner_scope()
            || suite.suite().min_cases < policy.min_cases_floor
            || pins.len() != 2
        {
            return Err(refuse("historical gate suite or two-split floor changed"));
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut entries = Vec::new();
        for pin in pins {
            let evaluation = self.verify_evaluation(&pin.evaluation_id, Some(&pin.receipt_digest))?;
            let receipt = &evaluation.receipt;
            let expected_epoch = crate::skills::lifecycle::ProposalEvaluationBinding {
                proposal_id: subject.proposal_id.clone(),
                evaluation_epoch: subject.evaluation_epoch,
                evaluating_since: subject.evaluating_since,
            };
            if receipt.proposal_evaluation.as_ref() != Some(&expected_epoch) {
                return Err(refuse(
                    "historical gate requires both sealed same-proposal evaluation triples",
                ));
            }
            let started_in_epoch = receipt.started_at >= subject.evaluating_since;
            let finished_in_order = receipt.finished_at >= receipt.started_at;
            let finished_before_gate = receipt.finished_at <= dispatched_at;
            if !matches!(receipt.split, EvalSplit::Validation | EvalSplit::Holdout)
                || !seen.insert(receipt.split)
                || receipt.owner_scope != *self.owner_scope()
                || receipt.skill_id != subject.skill_id
                || receipt.suite_digest != suite_digest
                || receipt.candidate.revision_id != subject.candidate_revision
                || receipt.baseline.as_ref().map(|arm| &arm.revision_id) != subject.baseline_revision.as_ref()
                || receipt.observed_generation != subject.expected_generation
                || receipt.end_generation != subject.expected_generation
                || receipt.stale
                || !started_in_epoch
                || !finished_in_order
                || !finished_before_gate
                || !super::evaluation_reasons(suite.suite(), policy, &evaluation).is_empty()
            {
                return Err(refuse("historical gate evaluation subject, window or verdict changed"));
            }
            entries.push(super::entry_of(&evaluation));
        }
        entries.sort_by_key(|entry| super::split_order(entry.split));
        if !seen.contains(&EvalSplit::Validation)
            || !seen.contains(&EvalSplit::Holdout)
            || entries != gate.evaluations
            || !gate.deterministic_reasons.is_empty()
            || gate.judge.as_ref().is_none_or(|judge| judge.identity != judge_identity)
            || policy
                .judge_identity
                .as_ref()
                .is_some_and(|identity| identity != judge_identity)
        {
            return Err(refuse(
                "historical gate pins, deterministic result or pinned judge changed",
            ));
        }
        let (decision, reasons) = super::final_decision(&gate.deterministic_reasons, gate.judge.as_ref(), policy)?;
        if decision != GateDecision::Accept
            || gate.decision != decision
            || !reasons.is_empty()
            || gate.reasons != reasons
        {
            return Err(refuse("historical gate is not the sealed accepting decision"));
        }
        Ok((gate.decided_at, gate.expires_at))
    }
}
