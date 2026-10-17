//! Publication authorization and publication of an authorized proposal (SL08).
//!
//! ```text
//! Eligible --authorize_publication--> PublicationAuthorized --publish_authorized--> Committed
//! ```
//!
//! Authorization re-verifies the sealed gate decision with the policy and the
//! governing suite the *caller* supplies from operator-owned configuration
//! (never `decision.policy` or the proposal's own suite) and records the gate
//! digest it was authorized against. Publication goes through the store-level
//! [`SkillLifecycleStore::publish`], the sole writer of the active binding,
//! with the generation the proposal was prepared against, so a writer that
//! lost the race is rejected by the compare-and-swap.
//!
//! Crash behavior: the proposal record and the publication journal are two
//! durable records. A crash between them is repaired by calling
//! [`SkillLifecycleStore::publish_authorized`] again: the transaction id is
//! derived from the proposal and the attempt, so the retry finds the journal
//! entry, lets the store reconcile it, and finishes the proposal record. A
//! transaction that aborted is never re-run; the next attempt gets a new id and
//! re-verifies the gate.

use super::gate::{GateError, GatePolicy, gate_id_of};
use super::journal::{TransactionId, TransactionState};
use super::proposal::{
    ProposalFailure, ProposalFailureKind, ProposalLock, ProposalStatus, PublicationAuthorization, PublicationRecord,
    SkillProposal, bounded,
};
use super::publish::{PublishRequest, TransactionOutcome};
use super::store::SkillLifecycleStore;
use super::types::{LifecycleError, NewRevision, SourceKind};
use chrono::Utc;

/// Publication attempts one authorization allows (an aborted transaction
/// consumes one).
pub const MAX_PUBLICATION_ATTEMPTS: u32 = 3;

/// What publishing an authorized proposal did.
#[derive(Clone, Debug)]
pub struct PublicationOutcome {
    pub proposal: SkillProposal,
    pub transaction: TransactionOutcome,
}

/// Deterministic transaction id of a proposal's publication attempt.
fn publication_tx(proposal_id: &str, attempt: u32) -> Result<TransactionId, LifecycleError> {
    let digest = proposal_id.strip_prefix("prop-").unwrap_or(proposal_id);
    TransactionId::parse(&format!("pub-{digest}-{attempt}"))
}

impl SkillLifecycleStore {
    /// Moves an `Eligible` proposal to `PublicationAuthorized`.
    ///
    /// `gate_digest` is the digest pinned when the gate decided; `policy` and
    /// `suite_digest` come from the operator-owned source of the caller. The
    /// sealed decision is verified with them ([`Self::verify_gate`]); only an
    /// accept changes the proposal. A receipt or skill that went stale, or an
    /// accept past its expiry, makes the proposal `Stale`. New failing evidence
    /// and every other refusal leave the proposal untouched. Calling it again
    /// with the same digest on an authorized proposal re-verifies and returns it.
    ///
    /// Blocking: waits for locks and does file I/O.
    pub fn authorize_publication(
        &self,
        proposal_id: &str,
        gate_digest: &str,
        policy: &GatePolicy,
        suite_digest: &str,
    ) -> Result<SkillProposal, GateError> {
        let proposal = self
            .read_proposal(proposal_id)?
            .ok_or_else(|| GateError::Unsupported(format!("no such proposal {proposal_id}")))?;
        match proposal.status {
            ProposalStatus::Eligible => {}
            ProposalStatus::PublicationAuthorized => {
                if proposal.authorization.as_ref().map(|a| a.gate_digest.as_str()) != Some(gate_digest) {
                    return Err(GateError::Unsupported(
                        "the proposal is authorized against another gate decision".to_string(),
                    ));
                }
            }
            status => {
                return Err(GateError::NotAuthorized {
                    proposal_id: proposal.proposal_id,
                    status: status.as_str(),
                });
            }
        }
        let gate_id = gate_id_of(self.owner_scope(), &proposal.proposal_id, proposal.evaluation_epoch);
        let verified = self
            .verify_gate(&gate_id, gate_digest, policy, suite_digest)
            .map_err(|e| self.stale_after(&proposal.proposal_id, e))?;
        let candidate = self.candidate_record_digest(&proposal, &verified.decision.record_id, &verified)?;
        crash_point!("authorize_verified");

        let lock = self.lock_proposals()?;
        let mut current = self
            .read_proposal_record(&proposal.proposal_id)?
            .ok_or_else(|| GateError::Unsupported(format!("no such proposal {}", proposal.proposal_id)))?;
        if current.evaluation_epoch != proposal.evaluation_epoch {
            return Err(GateError::Unsupported(
                "the proposal moved to another evaluation epoch during authorization".to_string(),
            ));
        }
        match current.status {
            ProposalStatus::Eligible => {}
            ProposalStatus::PublicationAuthorized
                if current.authorization.as_ref().map(|a| a.gate_digest.as_str()) == Some(gate_digest) =>
            {
                return Ok(current);
            }
            status => {
                return Err(GateError::NotAuthorized {
                    proposal_id: current.proposal_id,
                    status: status.as_str(),
                });
            }
        }
        let now = Utc::now();
        current.authorization = Some(PublicationAuthorization {
            gate_id,
            gate_digest: gate_digest.to_string(),
            candidate_record_digest: candidate,
            authorized_at: now,
            attempt: 1,
        });
        self.move_locked(
            &lock,
            &mut current,
            ProposalStatus::PublicationAuthorized,
            "authorized against a verified gate accept",
        )?;
        crash_point!("authorize_written");
        Ok(current)
    }

    /// Publishes an authorized proposal as the active revision of its skill,
    /// from the generation it was prepared against, and moves it to
    /// `Committed`.
    ///
    /// Before the first journal entry of an attempt the gate is verified again
    /// with the caller's `policy` and `suite_digest`. A generation that moved
    /// since authorization is a stale writer: the proposal becomes `Stale` and
    /// the active binding is untouched (the store's compare-and-swap enforces
    /// the same under the skill lock). Failures that leave the outcome unknown
    /// (I/O, lock timeout, a crash) keep the proposal authorized; calling again
    /// resumes by transaction id. A proposal that is already `Committed`
    /// returns its recorded publication.
    ///
    /// Blocking, like [`Self::publish`].
    pub fn publish_authorized(
        &self,
        proposal_id: &str,
        policy: &GatePolicy,
        suite_digest: &str,
    ) -> Result<PublicationOutcome, GateError> {
        let lock = self.lock_proposals()?;
        let mut proposal = self
            .read_proposal(proposal_id)?
            .ok_or_else(|| GateError::Unsupported(format!("no such proposal {proposal_id}")))?;
        if !matches!(
            proposal.status,
            ProposalStatus::PublicationAuthorized | ProposalStatus::Committed
        ) {
            return Err(GateError::NotAuthorized {
                proposal_id: proposal.proposal_id,
                status: proposal.status.as_str(),
            });
        }
        let mut auth = proposal
            .authorization
            .clone()
            .ok_or_else(|| GateError::Tampered("an authorized proposal carries no authorization record".to_string()))?;
        let candidate = proposal
            .candidate_revision
            .clone()
            .ok_or_else(|| GateError::Unsupported("the proposal has no staged candidate".to_string()))?;
        let transaction_id = publication_tx(&proposal.proposal_id, auth.attempt)?;
        let request = PublishRequest {
            transaction_id: transaction_id.clone(),
            skill_id: proposal.skill_id.clone(),
            candidate,
            candidate_record_digest: auth.candidate_record_digest.clone(),
            expected_generation: proposal.expected_generation,
            enabled: true,
        };
        if proposal.status == ProposalStatus::PublicationAuthorized && self.journal_entry(&transaction_id)?.is_none() {
            // Nothing was journaled for this attempt: the authorization must
            // still hold under the caller's own rules.
            if let Err(e) = self.verify_gate(&auth.gate_id, &auth.gate_digest, policy, suite_digest) {
                return Err(self.stale_locked(&lock, &mut proposal, e));
            }
        }
        crash_point!("publish_before_store");
        let outcome = match self.publish(&request) {
            Ok(outcome) => outcome,
            Err(e @ (LifecycleError::StaleGeneration { .. } | LifecycleError::RevisionAlreadyActive(_))) => {
                if proposal.status == ProposalStatus::PublicationAuthorized {
                    self.move_locked(&lock, &mut proposal, ProposalStatus::Stale, &e.to_string())?;
                }
                return Err(e.into());
            }
            Err(LifecycleError::TransactionNotCommitted {
                transaction_id: aborted,
                state: "aborted",
            }) => {
                auth.attempt += 1;
                if auth.attempt > MAX_PUBLICATION_ATTEMPTS {
                    proposal.failure = Some(ProposalFailure {
                        kind: ProposalFailureKind::PublicationAborted,
                        detail: bounded("every publication attempt aborted"),
                    });
                    self.move_locked(
                        &lock,
                        &mut proposal,
                        ProposalStatus::Failed,
                        "every publication attempt aborted",
                    )?;
                } else {
                    proposal.authorization = Some(auth);
                    self.move_locked(
                        &lock,
                        &mut proposal,
                        ProposalStatus::PublicationAuthorized,
                        "a publication attempt aborted; the next one uses a new transaction",
                    )?;
                }
                return Err(GateError::PublicationAborted {
                    proposal_id: proposal.proposal_id,
                    transaction_id: aborted,
                });
            }
            Err(e) => return Err(e.into()),
        };
        crash_point!("publish_before_finalize");
        if proposal.status == ProposalStatus::PublicationAuthorized {
            proposal.publication = Some(PublicationRecord {
                transaction_id: outcome.entry.transaction_id.to_string(),
                revision_id: outcome.entry.candidate_revision_id.clone(),
                generation: outcome.entry.new_generation,
                committed_at: outcome.entry.finished_at.unwrap_or_else(Utc::now),
            });
            self.move_locked(
                &lock,
                &mut proposal,
                ProposalStatus::Committed,
                &format!("published by transaction {}", outcome.entry.transaction_id),
            )?;
        } else if outcome.entry.state != TransactionState::Committed {
            return Err(GateError::Tampered(
                "a committed proposal points at a publication that is not committed".to_string(),
            ));
        }
        Ok(PublicationOutcome {
            proposal,
            transaction: outcome,
        })
    }

    /// Pinned record digest of the candidate revision. The revision normally
    /// exists already (evaluation installed it); otherwise it is written from
    /// the staged candidate with the proposal's provenance.
    fn candidate_record_digest(
        &self,
        proposal: &SkillProposal,
        gate_id: &str,
        verified: &super::gate::VerifiedGate,
    ) -> Result<String, GateError> {
        let Some(candidate) = proposal.candidate_revision.as_ref() else {
            return Err(GateError::Unsupported(
                "the proposal has no staged candidate".to_string(),
            ));
        };
        match self.read_revision(&proposal.skill_id, candidate, None) {
            Ok(existing) => Ok(existing.record_digest),
            Err(LifecycleError::RevisionNotFound(_)) => {
                let bundle = self.read_candidate_bundle(&proposal.proposal_id)?;
                let mut new = NewRevision::new(proposal.skill_id.clone(), SourceKind::Learned);
                new.parent_revision_id = Some(proposal.baseline_revision.clone());
                new.proposal_id = Some(proposal.proposal_id.clone());
                new.pattern_ids = proposal.pattern_ids.iter().map(ToString::to_string).collect();
                new.review_receipt_ids = vec![gate_id.to_string()];
                new.validation_receipt_ids = verified
                    .decision
                    .evaluations
                    .iter()
                    .map(|entry| entry.evaluation_id.clone())
                    .collect();
                Ok(self.write_revision(&new, &bundle)?.record_digest)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Marks the proposal `Stale` when `error` says the receipt or the skill
    /// moved or the accept expired, and hands the error back.
    fn stale_after(&self, proposal_id: &str, error: GateError) -> GateError {
        if matches!(error, GateError::Stale(_) | GateError::Expired(_)) {
            let lock = match self.lock_proposals() {
                Ok(lock) => lock,
                Err(e) => return e.into(),
            };
            match self.read_proposal_record(proposal_id) {
                Ok(Some(mut proposal)) => {
                    if let Err(e) = self.move_locked(&lock, &mut proposal, ProposalStatus::Stale, &error.to_string()) {
                        return e.into();
                    }
                }
                Ok(None) => {}
                Err(e) => return e.into(),
            }
        }
        error
    }

    /// Like [`Self::stale_after`] with the proposal lock already held.
    fn stale_locked(&self, lock: &ProposalLock, proposal: &mut SkillProposal, error: GateError) -> GateError {
        if matches!(error, GateError::Stale(_) | GateError::Expired(_))
            && let Err(e) = self.move_locked(lock, proposal, ProposalStatus::Stale, &error.to_string())
        {
            return e.into();
        }
        error
    }

    /// Moves `proposal` to `next` and stores it; the proposal lock is held.
    fn move_locked(
        &self,
        lock: &ProposalLock,
        proposal: &mut SkillProposal,
        next: ProposalStatus,
        reason: &str,
    ) -> Result<(), LifecycleError> {
        if proposal.status != next && !proposal.status.can_become(next) {
            return Err(LifecycleError::InvalidProposalTransition {
                proposal_id: proposal.proposal_id.clone(),
                from: proposal.status.as_str(),
                to: next.as_str(),
            });
        }
        proposal.status = next;
        proposal.status_reason = Some(bounded(reason));
        proposal.updated_at = Utc::now();
        self.write_proposal(lock, proposal)
    }
}
