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
//! Provenance of the candidate revision: it must already be in the main store.
//! `propose_revision` only stages the bundle under `proposals/<id>/candidate`;
//! the single production writer is `install_candidate` (candidate.rs), which
//! the lifecycle service runs when a proposal enters evaluation (proposal id =
//! this proposal, parent = baseline, provenance Learned). Here the revision is
//! accepted when it carries no proposal id or this proposal's id, and refused
//! (`RevisionConflict`) when it names another proposal. This authorization's own
//! provenance (gate id, gate digest, pinned record digest) lives on the
//! proposal record, not on the shared, content-addressed revision.
//!
//! Spec 8.3: the PREPARED journal entry pins a digest of the authorization
//! (proposal, gate decision, gate digest and candidate record digest), and the
//! candidate's capability requirements are re-checked against its baseline
//! before the first journal entry of an attempt (`check_capabilities`).
//!
//! Known limits, deferred:
//! - Failing evidence that lands between the final gate re-check and the
//!   binding swap is not seen: evaluation writes do not take the proposal lock.
//! - The proposal lock is held across the publish, up to the 10 s publication
//!   lock timeout; callers must handle `LockTimeout` (the lifecycle service
//!   turns it into a retryable error).

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
use sha2::{Digest, Sha256};

const AUTHORIZATION_PIN_DOMAIN: &[u8] = b"prx-skill-authorization\0";

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
pub(super) fn publication_tx(proposal_id: &str, attempt: u32) -> Result<TransactionId, LifecycleError> {
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
    /// live it is finalized and returned as `Committed` without verifying (an
    /// aborted attempt is verified again like any other).
    ///
    /// Blocking: waits for locks and does file I/O.
    pub(in crate::skills) fn authorize_publication(
        &self,
        proposal_id: &str,
        gate_digest: &str,
        policy: &GatePolicy,
        suite_digest: &str,
    ) -> Result<SkillProposal, GateError> {
        self.authorize_publication_inner(proposal_id, gate_digest, policy, suite_digest, None)
    }

    pub(in crate::skills::lifecycle) fn authorize_experiment_publication(
        &self,
        expected: &super::ExperimentFinalExpectation,
        policy: &GatePolicy,
        suite_digest: &str,
    ) -> Result<SkillProposal, GateError> {
        self.authorize_publication_inner(
            &expected.proposal_id,
            &expected.gate_sha256,
            policy,
            suite_digest,
            Some(expected),
        )
    }

    fn authorize_publication_inner(
        &self,
        proposal_id: &str,
        gate_digest: &str,
        policy: &GatePolicy,
        suite_digest: &str,
        expected: Option<&super::ExperimentFinalExpectation>,
    ) -> Result<SkillProposal, GateError> {
        let proposal = self
            .read_proposal(proposal_id)?
            .ok_or_else(|| GateError::Unsupported(format!("no such proposal {proposal_id}")))?;
        self.require_experiment_publication_intent(&proposal, expected)?;
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
                self.require_experiment_publication_binding(&current, expected)?;
                // A committed attempt is the publication's business; verifying
                // the gate again would see its own binding as stale. An aborted
                // or unpublished one is verified like any other authorization.
                if self.settle_publication(&lock, &mut current)? == Settlement::Committed {
                    return Ok(current);
                }
            }
            status
                if expected.is_some()
                    && super::product::recorded_publication_status(status)
                    && proposal.publication.is_some() =>
            {
                let _lock = self.lock_proposals()?;
                let current = self
                    .read_proposal_record(proposal_id)?
                    .ok_or_else(|| GateError::Unbound("the expected proposal is missing".into()))?;
                self.require_experiment_publication_binding(&current, expected)?;
                // No authorization is minted. The following original publisher
                // validates the actual Committed transaction before returning it.
                return Ok(current);
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
            .map_err(|e| self.stale_after_expected(&proposal.proposal_id, e, expected))?;
        check_bound(&proposal, &gate_id, &verified)?;
        self.require_source_review(&proposal)?;
        let candidate = self.candidate_record_digest(&proposal)?;
        self.check_capabilities(&proposal, Some(&candidate))?;
        crash_point!("authorize_verified");

        let lock = self.lock_proposals()?;
        let mut current = self
            .read_proposal_record(&proposal.proposal_id)?
            .ok_or_else(|| GateError::Unsupported(format!("no such proposal {}", proposal.proposal_id)))?;
        self.require_experiment_publication_binding(&current, expected)?;
        if expected.is_some()
            && super::product::recorded_publication_status(current.status)
            && current.publication.is_some()
        {
            return Ok(current);
        }
        if let Some(expected) = expected {
            self.require_experiment_fresh_publication(expected)?;
        }
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
    pub(in crate::skills) fn publish_authorized(
        &self,
        proposal_id: &str,
        policy: &GatePolicy,
        suite_digest: &str,
    ) -> Result<PublicationOutcome, GateError> {
        self.publish_authorized_inner(proposal_id, policy, suite_digest, None)
    }

    pub(in crate::skills::lifecycle) fn publish_experiment_authorized(
        &self,
        expected: &super::ExperimentFinalExpectation,
        policy: &GatePolicy,
        suite_digest: &str,
    ) -> Result<PublicationOutcome, GateError> {
        self.publish_authorized_inner(&expected.proposal_id, policy, suite_digest, Some(expected))
    }

    fn publish_authorized_inner(
        &self,
        proposal_id: &str,
        policy: &GatePolicy,
        suite_digest: &str,
        expected: Option<&super::ExperimentFinalExpectation>,
    ) -> Result<PublicationOutcome, GateError> {
        let lock = self.lock_proposals()?;
        let mut proposal = self
            .read_proposal(proposal_id)?
            .ok_or_else(|| GateError::Unsupported(format!("no such proposal {proposal_id}")))?;
        self.require_experiment_publication_binding(&proposal, expected)?;
        let historical_product = expected.is_some()
            && super::product::recorded_publication_status(proposal.status)
            && proposal.publication.is_some();
        if !historical_product
            && !matches!(
                proposal.status,
                ProposalStatus::PublicationAuthorized | ProposalStatus::Committed
            )
        {
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
        let pin = authorization_pin(&proposal.proposal_id, &auth);
        if historical_product {
            // Product replay is historical observation only. In particular a
            // later rollback, gate expiry or projection does not reactivate or
            // repair this old revision and cannot create another transaction.
            let entry = self.journal_entry(&transaction_id)?.ok_or_else(|| {
                GateError::Unbound("committed experiment publication has no actual transaction".into())
            })?;
            let publication = proposal
                .publication
                .as_ref()
                .ok_or_else(|| GateError::Unbound("committed experiment proposal has no publication record".into()))?;
            if entry.state != TransactionState::Committed
                || entry.kind != super::TransactionKind::Publish
                || entry.skill_id != request.skill_id
                || entry.candidate_revision_id != request.candidate
                || entry.candidate_record_digest != request.candidate_record_digest
                || entry.expected_generation != request.expected_generation
                || entry.authorization_digest.as_deref() != Some(pin.as_str())
                || publication.transaction_id != transaction_id.as_str()
                || publication.revision_id != entry.candidate_revision_id
                || publication.generation != entry.new_generation
                || Some(publication.committed_at) != entry.finished_at
            {
                return Err(GateError::Unbound(
                    "committed experiment publication history is inconsistent".into(),
                ));
            }
            return Ok(PublicationOutcome {
                proposal,
                transaction: TransactionOutcome { entry, replayed: true },
            });
        }
        if proposal.status == ProposalStatus::PublicationAuthorized && self.journal_entry(&transaction_id)?.is_none() {
            if let Some(expected) = expected {
                self.require_experiment_fresh_publication(expected)?;
            }
            // Nothing was journaled for this attempt: the authorization must
            // still hold under the caller's own rules.
            match self.verify_gate(&auth.gate_id, &auth.gate_digest, policy, suite_digest) {
                Ok(verified) => check_bound(&proposal, &expected_gate, &verified)?,
                Err(e) => return Err(self.stale_locked(&lock, &mut proposal, e)),
            }
            self.require_source_review(&proposal)?;
            // Spec 8.3 step 3: the capabilities are checked again at publication.
            if let Err(e) = self.check_capabilities(&proposal, Some(&auth.candidate_record_digest)) {
                if matches!(e, LifecycleError::CapabilityExpansion { .. }) {
                    proposal.failure = Some(ProposalFailure {
                        kind: ProposalFailureKind::CapabilityExpansion,
                        detail: bounded(&e.to_string()),
                    });
                    self.move_locked(&lock, &mut proposal, ProposalStatus::Failed, &e.to_string())?;
                }
                return Err(e.into());
            }
        }
        crash_point!("publish_before_store");
        let outcome = match self.publish_pinned(&request, &pin) {
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

    /// Startup variant of [`Self::finalize_published_proposals`]: a proposal
    /// whose record cannot be read, or whose publication cannot be settled,
    /// is logged and reported in `skipped` instead of blocking the others.
    /// Only the proposal lock itself can fail the call.
    ///
    /// Blocking, like [`Self::publish`].
    pub fn finalize_published_proposals_lenient(&self) -> Result<Finalization, GateError> {
        let lock = self.lock_proposals()?;
        let (proposals, unreadable) = self.list_proposals_lenient()?;
        let mut report = Finalization {
            finalized: Vec::new(),
            skipped: unreadable.into_iter().map(|u| (u.proposal_id, u.reason)).collect(),
        };
        for mut proposal in proposals {
            if proposal.status != ProposalStatus::PublicationAuthorized {
                continue;
            }
            match self.settle_publication(&lock, &mut proposal) {
                Ok(Settlement::Committed) => report.finalized.push(proposal.proposal_id),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(proposal = %proposal.proposal_id, %error, "could not settle a skill publication at startup");
                    report.skipped.push((proposal.proposal_id, bounded(&error.to_string())));
                }
            }
        }
        Ok(report)
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
        let pin = authorization_pin(&proposal.proposal_id, &auth);
        match self.publish_pinned(&publish_request(proposal, &auth)?, &pin) {
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
    /// in the main store. No production code writes it there yet (see the module
    /// header: SL09 owns that writer); `verify_gate` re-reads it, so a gated
    /// proposal whose revision is missing is refused (`RevisionNotFound`) and
    /// never rebuilt from the staged bundle. An existing revision that names
    /// another proposal is refused (`RevisionConflict`).
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

    /// H2 error reconciliation is also guarded by the exact source-owned final.
    /// A caller-supplied stale or forged expectation cannot trigger settlement
    /// or a terminal proposal mutation through the authorization error path.
    fn stale_after_expected(
        &self,
        proposal_id: &str,
        error: GateError,
        expected: Option<&super::ExperimentFinalExpectation>,
    ) -> GateError {
        let Some(expected) = expected else {
            return self.stale_after(proposal_id, error);
        };
        let lock = match self.lock_proposals() {
            Ok(lock) => lock,
            Err(error) => return error.into(),
        };
        let mut proposal = match self.read_proposal_record(proposal_id) {
            Ok(Some(proposal)) => proposal,
            Ok(None) => return GateError::Unbound("the expected proposal is missing".into()),
            Err(error) => return error.into(),
        };
        if let Err(error) = self.require_experiment_publication_binding(&proposal, Some(expected)) {
            return error;
        }
        self.stale_locked(&lock, &mut proposal, error)
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

/// How an operator closes a stuck authorized proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForceClose {
    /// The proposal ends `Failed`.
    Failed,
    /// The proposal ends `Stale`: the world moved on, a new proposal may follow.
    Stale,
}

impl SkillLifecycleStore {
    /// Closes a `PublicationAuthorized` proposal without publishing (see
    /// `SkillsLifecycleService::force_close`). The binding is never touched.
    /// A `TransactionConflict` while settling the publication is the case this
    /// exists for, but it closes only when the publication is provably not
    /// live: the journal entry is aborted (or absent, or a PREPARED one that
    /// reconciles to aborted) and the binding does not carry the transaction.
    /// A publication that settles as committed, or that is live behind the
    /// conflict, is not closed: the proposal is recorded `Committed` from the
    /// journal entry (its transaction and generation) and returned.
    pub(in crate::skills) fn force_close_proposal(
        &self,
        id: &str,
        reason: &str,
        outcome: ForceClose,
    ) -> Result<SkillProposal, LifecycleError> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(LifecycleError::ProposalRefused(
                "a force-close needs a reason".to_string(),
            ));
        }
        let lock = self.lock_proposals()?;
        let mut proposal = self
            .read_proposal_record(id)?
            .ok_or_else(|| LifecycleError::ProposalRefused(format!("no such proposal {id}")))?;
        if proposal.status != ProposalStatus::PublicationAuthorized {
            return Err(LifecycleError::ProposalRefused(format!(
                "only a publication_authorized proposal can be force-closed; {id} is {}",
                proposal.status.as_str()
            )));
        }
        match self.settle_publication(&lock, &mut proposal) {
            // Settled as published: the proposal is Committed now.
            Ok(Settlement::Committed) => return Ok(proposal),
            Ok(Settlement::Aborted | Settlement::NotPublished) => {}
            Err(LifecycleError::TransactionConflict { transaction_id }) => {
                // The conflict says nothing about the candidate. Read the
                // journal entry and the binding: a live publication is never
                // closed over, whatever its pin says.
                let auth = proposal
                    .authorization
                    .as_ref()
                    .ok_or_else(|| LifecycleError::ProposalRefused(format!("{id} carries no authorization record")))?;
                let tx = publication_tx(&proposal.proposal_id, auth.attempt)?;
                if let Some(entry) = self.live_publication(&proposal.skill_id, &tx)? {
                    // The publication went live behind the conflict. Closing
                    // the proposal would strand it, so record it as what it is.
                    tracing::warn!(
                        proposal_id = %id,
                        transaction_id = %transaction_id,
                        "force-close found the publication live; recording the proposal committed"
                    );
                    self.finish_commit(&lock, &mut proposal, &TransactionOutcome { entry, replayed: true })?;
                    return Ok(proposal);
                }
                tracing::warn!(
                    proposal_id = %id,
                    transaction_id = %transaction_id,
                    "force-closing a proposal stuck on a transaction conflict whose publication is not live"
                );
            }
            Err(error) => return Err(error),
        }
        let (next, kind_note) = match outcome {
            ForceClose::Failed => (ProposalStatus::Failed, "failed"),
            ForceClose::Stale => (ProposalStatus::Stale, "stale"),
        };
        proposal.failure = Some(ProposalFailure {
            kind: ProposalFailureKind::OperatorClosed,
            detail: bounded(reason),
        });
        self.move_locked(
            &lock,
            &mut proposal,
            next,
            &format!("operator force-close ({kind_note}): {reason}"),
        )?;
        Ok(proposal)
    }
}

/// What [`SkillLifecycleStore::finalize_published_proposals_lenient`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Finalization {
    /// Proposals whose committed publication was recorded.
    pub finalized: Vec<String>,
    /// Proposal ids that could not be read or settled, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// Digest of the authorization a publication runs under; pinned in the
/// PREPARED journal entry.
pub(super) fn authorization_pin(proposal_id: &str, auth: &PublicationAuthorization) -> String {
    let mut hasher = Sha256::new();
    hasher.update(AUTHORIZATION_PIN_DOMAIN);
    for part in [
        proposal_id,
        auth.gate_id.as_str(),
        auth.gate_digest.as_str(),
        auth.candidate_record_digest.as_str(),
    ] {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hex::encode(hasher.finalize())
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
