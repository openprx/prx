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
//!
//! A live publication is never stranded: before a pre-commit terminal move
//! (stale, failed, cancelled) of an authorized proposal, the current attempt's
//! journal entry is settled; when it committed, the proposal is finalized as
//! `Committed` instead (see `settle_publication`, and
//! [`SkillLifecycleStore::finalize_published_proposals`] for a sweep to run
//! with or after `recover()`).
//!
//! Provenance of the candidate revision: it must already exist (evaluation
//! installs it and `verify_gate` re-reads it). It is accepted when it carries no
//! proposal id or this proposal's id, and refused (`RevisionConflict`) when it
//! names another proposal. This authorization's own provenance (gate id, gate
//! digest, pinned record digest) lives on the proposal record, not on the
//! shared, content-addressed revision.
//!
//! Known limits, deferred (SL09 must account for them):
//! - Failing evidence that lands between the final gate re-check and the
//!   binding swap is not seen: evaluation writes do not take the proposal lock.
//! - The PREPARED journal entry carries no gate or receipt hashes, and the
//!   candidate's capability requirements are not re-checked at publication.
//! - The proposal lock is held across the publish, up to the 10 s publication
//!   lock timeout; callers must handle `LockTimeout` and retry.

use super::gate::{GateError, GatePolicy, VerifiedGate, gate_id_of};
use super::journal::{TransactionId, TransactionState};
use super::proposal::{
    ProposalFailure, ProposalFailureKind, ProposalLock, ProposalStatus, PublicationAuthorization, PublicationRecord,
    SkillProposal, bounded,
};
use super::publish::{PublishRequest, TransactionOutcome};
use super::store::SkillLifecycleStore;
use super::types::{LifecycleError, RevisionId};
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

/// What the journal says about the current publication attempt of a proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Settlement {
    /// No journal entry for the attempt (or the proposal is not authorized).
    NotPublished,
    /// The attempt committed; the proposal is now `Committed`.
    Committed,
    /// The attempt's transaction aborted; the binding never carried it.
    Aborted,
}

impl SkillLifecycleStore {
    /// Moves an `Eligible` proposal to `PublicationAuthorized`.
    ///
    /// `gate_digest` is the digest pinned when the gate decided; `policy` and
    /// `suite_digest` come from the operator-owned source of the caller. The
    /// sealed decision is verified with them ([`Self::verify_gate`]); only an
    /// accept changes the proposal. A receipt or skill that went stale, or an
    /// accept past its expiry, makes the proposal `Stale`. New failing evidence
    /// and every other refusal leave an `Eligible` proposal untouched.
    /// Calling it again with the same digest on an authorized proposal
    /// re-verifies and returns it; when the proposal's publication already went
    /// live it is finalized and returned as `Committed` without verifying.
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
                let lock = self.lock_proposals()?;
                let mut current = self
                    .read_proposal_record(proposal_id)?
                    .ok_or_else(|| GateError::Unsupported(format!("no such proposal {proposal_id}")))?;
                // An attempt with a journal entry is the publication's business;
                // verifying the gate again would see its own binding as stale.
                if self.settle_publication(&lock, &mut current)? != Settlement::NotPublished {
                    return Ok(current);
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
        check_bound(&proposal, &gate_id, &verified)?;
        let candidate = self.candidate_record_digest(&proposal)?;
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
        current.authorization = Some(PublicationAuthorization {
            gate_id,
            gate_digest: gate_digest.to_string(),
            candidate_record_digest: candidate,
            authorized_at: Utc::now(),
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
    /// with the caller's `policy` and `suite_digest`, and the authorization
    /// record must name this proposal's own gate. A generation that moved since
    /// authorization is a stale writer, and so is new failing evidence: the
    /// proposal becomes `Stale` and the active binding is untouched (the
    /// store's compare-and-swap enforces the generation under the skill lock).
    /// Failures that leave the outcome unknown (I/O, lock timeout, a crash) keep
    /// the proposal authorized; calling again resumes by transaction id. A
    /// proposal that is already `Committed` returns its recorded publication.
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
        let expected_gate = gate_id_of(self.owner_scope(), &proposal.proposal_id, proposal.evaluation_epoch);
        if auth.gate_id != expected_gate {
            return Err(GateError::Tampered(
                "the authorization names a gate decision of another proposal or epoch".to_string(),
            ));
        }
        let transaction_id = publication_tx(&proposal.proposal_id, auth.attempt)?;
        let request = publish_request(&proposal, &auth)?;
        if proposal.status == ProposalStatus::PublicationAuthorized && self.journal_entry(&transaction_id)?.is_none() {
            // Nothing was journaled for this attempt: the authorization must
            // still hold under the caller's own rules.
            match self.verify_gate(&auth.gate_id, &auth.gate_digest, policy, suite_digest) {
                Ok(verified) => check_bound(&proposal, &expected_gate, &verified)?,
                Err(e) => return Err(self.stale_locked(&lock, &mut proposal, e)),
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
            self.finish_commit(&lock, &mut proposal, &outcome)?;
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

    /// Finalizes every `PublicationAuthorized` proposal whose current attempt
    /// committed (a crash between the journal commit and the proposal record,
    /// or a repair-required failure). Run it with or after [`Self::recover`],
    /// which settles the journal entries it reads. Returns the finalized ids.
    ///
    /// Blocking, like [`Self::publish`].
    pub fn finalize_published_proposals(&self) -> Result<Vec<String>, GateError> {
        let lock = self.lock_proposals()?;
        let mut finalized = Vec::new();
        for mut proposal in self.list_proposals()? {
            if proposal.status == ProposalStatus::PublicationAuthorized
                && self.settle_publication(&lock, &mut proposal)? == Settlement::Committed
            {
                finalized.push(proposal.proposal_id);
            }
        }
        Ok(finalized)
    }

    /// Looks up the journal entry of the proposal's current publication
    /// attempt and, when it exists, lets the store settle it under the skill
    /// lock. A committed attempt finalizes the proposal as `Committed`. The
    /// proposal lock is held by the caller. Anything but `PublicationAuthorized`
    /// is `NotPublished`.
    pub(super) fn settle_publication(
        &self,
        lock: &ProposalLock,
        proposal: &mut SkillProposal,
    ) -> Result<Settlement, LifecycleError> {
        if proposal.status != ProposalStatus::PublicationAuthorized {
            return Ok(Settlement::NotPublished);
        }
        let Some(auth) = proposal.authorization.clone() else {
            return Ok(Settlement::NotPublished);
        };
        let transaction_id = publication_tx(&proposal.proposal_id, auth.attempt)?;
        if self.journal_entry(&transaction_id)?.is_none() {
            return Ok(Settlement::NotPublished);
        }
        match self.publish(&publish_request(proposal, &auth)?) {
            Ok(outcome) => {
                self.finish_commit(lock, proposal, &outcome)?;
                Ok(Settlement::Committed)
            }
            Err(LifecycleError::TransactionNotCommitted { state: "aborted", .. }) => Ok(Settlement::Aborted),
            Err(e) => Err(e),
        }
    }

    fn finish_commit(
        &self,
        lock: &ProposalLock,
        proposal: &mut SkillProposal,
        outcome: &TransactionOutcome,
    ) -> Result<(), LifecycleError> {
        proposal.publication = Some(PublicationRecord {
            transaction_id: outcome.entry.transaction_id.to_string(),
            revision_id: outcome.entry.candidate_revision_id.clone(),
            generation: outcome.entry.new_generation,
            committed_at: outcome.entry.finished_at.unwrap_or_else(Utc::now),
        });
        self.move_locked(
            lock,
            proposal,
            ProposalStatus::Committed,
            &format!("published by transaction {}", outcome.entry.transaction_id),
        )
    }

    /// Pinned record digest of the candidate revision, which must already be
    /// in the store: evaluation installs the candidate and `verify_gate`
    /// re-reads it, so a gated proposal whose revision is gone is refused
    /// (`RevisionNotFound`), never rebuilt from the staged bundle. An existing
    /// revision that names another proposal is refused (`RevisionConflict`).
    fn candidate_record_digest(&self, proposal: &SkillProposal) -> Result<String, GateError> {
        let Some(candidate) = proposal.candidate_revision.as_ref() else {
            return Err(GateError::Unsupported(
                "the proposal has no staged candidate".to_string(),
            ));
        };
        let existing = self.read_revision(&proposal.skill_id, candidate, None)?;
        match existing.revision.proposal_id.as_deref() {
            Some(other) if other != proposal.proposal_id => Err(LifecycleError::RevisionConflict {
                revision: format!("{}/{candidate}", proposal.skill_id),
                field: "proposal_id",
            }
            .into()),
            _ => Ok(existing.record_digest),
        }
    }

    /// Marks the proposal `Stale` when `error` calls for it, and hands the
    /// error back. A proposal whose publication is already live is finalized
    /// instead and keeps the error.
    fn stale_after(&self, proposal_id: &str, error: GateError) -> GateError {
        let lock = match self.lock_proposals() {
            Ok(lock) => lock,
            Err(e) => return e.into(),
        };
        match self.read_proposal_record(proposal_id) {
            Ok(Some(mut proposal)) => self.stale_locked(&lock, &mut proposal, error),
            Ok(None) => error,
            Err(e) => e.into(),
        }
    }

    /// Like [`Self::stale_after`] with the proposal lock already held.
    fn stale_locked(&self, lock: &ProposalLock, proposal: &mut SkillProposal, error: GateError) -> GateError {
        let calls_for_stale = match &error {
            GateError::Stale(_) | GateError::Expired(_) => true,
            // Evidence that arrived after authorization leaves nothing to retry.
            GateError::NewFailingEvidence(_) => proposal.status == ProposalStatus::PublicationAuthorized,
            _ => false,
        };
        if !calls_for_stale {
            return error;
        }
        match self.settle_publication(lock, proposal) {
            Ok(Settlement::Committed) => return error,
            Ok(_) => {}
            Err(e) => return e.into(),
        }
        if proposal.status.can_become(ProposalStatus::Stale)
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

/// The verified decision must be this proposal's own, for its own candidate.
fn check_bound(proposal: &SkillProposal, gate_id: &str, verified: &VerifiedGate) -> Result<(), GateError> {
    let decision = &verified.decision;
    if decision.record_id != gate_id
        || decision.proposal_id != proposal.proposal_id
        || proposal.candidate_revision.as_ref() != Some(&decision.candidate_revision)
    {
        return Err(GateError::Tampered(
            "the gate decision does not belong to this proposal and candidate".to_string(),
        ));
    }
    Ok(())
}

fn publish_request(
    proposal: &SkillProposal,
    auth: &PublicationAuthorization,
) -> Result<PublishRequest, LifecycleError> {
    let candidate: RevisionId = proposal
        .candidate_revision
        .clone()
        .ok_or_else(|| LifecycleError::ProposalRefused("the proposal has no staged candidate".to_string()))?;
    Ok(PublishRequest {
        transaction_id: publication_tx(&proposal.proposal_id, auth.attempt)?,
        skill_id: proposal.skill_id.clone(),
        candidate,
        candidate_record_digest: auth.candidate_record_digest.clone(),
        expected_generation: proposal.expected_generation,
        enabled: true,
    })
}
