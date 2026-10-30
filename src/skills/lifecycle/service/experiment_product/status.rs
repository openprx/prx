//! Observation only. Never infer live termination from a durable cancel flag.
use super::{Config, ProductResult, SkillsLifecycleService};
use crate::skills::lifecycle::experiment::{ExperimentJournal, ExperimentResources, ExperimentStageKind, StageState};
use crate::skills::lifecycle::*;

impl SkillsLifecycleService {
    pub(super) fn product_snapshot(
        &self,
        journal: &ExperimentJournal,
        config: &Config,
    ) -> ProductResult<ExperimentProductSnapshot> {
        let mut issues = Vec::new();
        let qualification = match self.experiment_qualified(journal) {
            Ok(true) => ExperimentQualification::QualifiedHistorical,
            Ok(false) => ExperimentQualification::Nonqualifying,
            Err(error) => {
                issues.push(error.to_string());
                ExperimentQualification::EvidenceUnavailable
            }
        };
        let (final_selection, final_unavailable) = match self.resolve_product_final(journal) {
            Ok(value) => (value, false),
            Err(error) => {
                issues.push(error.to_string());
                (None, true)
            }
        };
        let publication = match final_selection.as_ref() {
            Some(expected) => match self.product_publication_observation(expected) {
                Ok(value) => value,
                Err(error) => {
                    issues.push(error.to_string());
                    unavailable_publication()
                }
            },
            None if final_unavailable => unavailable_publication(),
            None => ExperimentPublicationObservation {
                status: ExperimentPublicationStatus::NotPublished,
                transaction: None,
                proposal_committed: false,
                authorized_attempt: None,
                attempts: Vec::new(),
                provenance: "no_qualified_final_publication_observed; not_proof_of_unrelated_effect_absence",
            },
        };
        let adoption = match (final_selection.as_ref(), publication.transaction.as_ref()) {
            (Some(expected), Some(tx)) if publication.status == ExperimentPublicationStatus::Committed => {
                match self.store.product_adoptions(expected, tx.transaction_id.as_str()) {
                    Ok(records) => ExperimentAdoptionObservation {
                        status: if records.is_empty() {
                            ExperimentAdoptionStatus::NotRecorded
                        } else {
                            ExperimentAdoptionStatus::Recorded
                        },
                        records,
                        provenance: "exact_origin_suite_families_and_existing_authenticated_owner_indices",
                    },
                    Err(error) => {
                        issues.push(error.to_string());
                        ExperimentAdoptionObservation {
                            status: ExperimentAdoptionStatus::ObservationUnavailable,
                            records: Vec::new(),
                            provenance: "authenticated_adoption_observation_failed",
                        }
                    }
                }
            }
            _ => ExperimentAdoptionObservation {
                status: ExperimentAdoptionStatus::ObservationUnavailable,
                records: Vec::new(),
                provenance: "no_exact_committed_publication_observed; absence_not_proved",
            },
        };
        let physical_observations = match self.experiment_physical_observations(journal, config) {
            Ok(value) => Some(value),
            Err(error) => {
                issues.push(error.to_string());
                None
            }
        };
        let mut settled_usage = ExperimentResources::default();
        let mut stages = Vec::new();
        let mut started = false;
        let mut unknown = false;
        let mut active = false;
        let mut failed = false;
        let mut cleaned = true;
        for declared in &journal.plan.spec().stages {
            let state = journal.state.stages.get(&declared.id).ok_or_else(|| {
                ExperimentProductError::refused(
                    ExperimentProductFailureKind::EvidenceUnavailable,
                    "missing stage state",
                )
            })?;
            let (label, usage, cleanup_confirmed) = match state {
                StageState::Planned => (ExperimentStageStatus::Planned, None, false),
                StageState::Reserved => {
                    started = true;
                    active = true;
                    cleaned = false;
                    (ExperimentStageStatus::Reserved, None, false)
                }
                StageState::Dispatched => {
                    started = true;
                    active = true;
                    cleaned = false;
                    (ExperimentStageStatus::Dispatched, None, false)
                }
                StageState::DispatchUnknown => {
                    started = true;
                    unknown = true;
                    cleaned = false;
                    (ExperimentStageStatus::DispatchUnknown, None, false)
                }
                StageState::Settled {
                    usage,
                    successful,
                    cleanup_confirmed,
                    ..
                } => {
                    started = true;
                    failed |= !successful;
                    cleaned &= *cleanup_confirmed;
                    settled_usage = settled_usage.checked_add(*usage)?;
                    (ExperimentStageStatus::Settled, Some(*usage), *cleanup_confirmed)
                }
            };
            stages.push(ExperimentStageObservation {
                stage: declared.id.clone(),
                state: label,
                settled_usage: usage,
                cleanup_confirmed,
                allocation: declared.allocation,
            });
        }
        let physical_uncertain = physical_observations.as_ref().is_none_or(|value| {
            value.as_object().is_none_or(|stages| {
                stages.values().any(|stage| {
                    stage
                        .get("needs_reconciliation")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true)
                })
            })
        });
        let cleanup = if !started {
            ExperimentCleanupStatus::NotStarted
        } else if unknown || physical_uncertain {
            ExperimentCleanupStatus::Unknown
        } else if cleaned {
            ExperimentCleanupStatus::Confirmed
        } else if journal.state.cancelled {
            ExperimentCleanupStatus::RequestedUnconfirmed
        } else {
            ExperimentCleanupStatus::Unconfirmed
        };
        let mut baseline_retained = false;
        if let Some(selection) = journal.plan.spec().stages.iter().rev().find(|stage| {
            stage.kind == ExperimentStageKind::Selection
                && journal.state.stages.get(&stage.id).is_some_and(StageState::successful)
        }) {
            match self.selected_development_proposal(journal, &selection.id) {
                Ok(None) => baseline_retained = true,
                Ok(Some(_)) => {}
                Err(error) => issues.push(error.to_string()),
            }
        }
        let execution = if unknown {
            ExperimentExecutionStatus::Unknown
        } else if journal.state.cancelled {
            ExperimentExecutionStatus::CancelRequested
        } else if active {
            ExperimentExecutionStatus::ActiveUnconfirmed
        } else if failed {
            ExperimentExecutionStatus::Failed
        } else if baseline_retained {
            ExperimentExecutionStatus::BaselineRetained
        } else if journal.state.stages.values().all(StageState::successful) {
            ExperimentExecutionStatus::Completed
        } else {
            ExperimentExecutionStatus::Planned
        };
        Ok(ExperimentProductSnapshot {
            handle: ExperimentProductHandle {
                experiment_id: journal.plan.spec().id.clone(),
                plan_sha256: journal.plan.digest().into(),
            },
            owner_scope: self.owner_scope().to_string(),
            journal_head_sha256: journal.head_sha256.clone(),
            cancel_requested: journal.state.cancelled,
            execution,
            qualification,
            final_selection,
            publication,
            adoption,
            stages,
            committed_or_reserved: journal.state.committed(&journal.plan)?,
            settled_usage,
            cleanup,
            termination_confirmed: started && cleaned && !unknown && !active && !physical_uncertain,
            observation_provenance: "durable_owner_events_and_read_only_physical_observations; no_live_draining_claim",
            physical_observations,
            evidence_issues: issues,
        })
    }

    fn product_publication_observation(
        &self,
        expected: &ExperimentFinalExpectation,
    ) -> ProductResult<ExperimentPublicationObservation> {
        let _owner = self.store.lock_proposals()?;
        let proposal = self.proposal_of(&expected.proposal_id)?;
        self.store
            .require_experiment_publication_binding(&proposal, Some(expected))?;
        let Some(auth) = proposal.authorization.as_ref() else {
            return Ok(ExperimentPublicationObservation {
                status: ExperimentPublicationStatus::NotPublished,
                transaction: None,
                authorized_attempt: None,
                attempts: Vec::new(),
                proposal_committed: false,
                provenance: "exact_proposal_has_no_authorization",
            });
        };
        let id = crate::skills::lifecycle::authorize::publication_tx(&proposal.proposal_id, auth.attempt)?;
        let transaction = self.store.journal_entry(&id)?;
        let mut attempts = Vec::new();
        for attempt in 1..=auth.attempt {
            let attempt_id = crate::skills::lifecycle::authorize::publication_tx(&proposal.proposal_id, attempt)?;
            if let Some(entry) = self.store.journal_entry(&attempt_id)? {
                if entry.kind != TransactionKind::Publish
                    || entry.skill_id != expected.skill_id
                    || entry.candidate_revision_id != expected.candidate_revision
                    || entry.candidate_record_digest != expected.candidate_record_sha256
                    || entry.expected_generation != expected.baseline_generation
                    || entry.authorization_digest.as_deref()
                        != Some(
                            crate::skills::lifecycle::authorize::authorization_pin(&proposal.proposal_id, auth)
                                .as_str(),
                        )
                {
                    return Err(ExperimentProductError::refused(
                        ExperimentProductFailureKind::EvidenceUnavailable,
                        "publication attempt differs from its exact authorization",
                    ));
                }
                attempts.push(entry);
            }
        }
        let status = match transaction.as_ref() {
            None if proposal.status == ProposalStatus::Committed => ExperimentPublicationStatus::ObservationUnavailable,
            None => ExperimentPublicationStatus::Authorized,
            Some(entry) => {
                if entry.kind != TransactionKind::Publish
                    || entry.skill_id != expected.skill_id
                    || entry.candidate_revision_id != expected.candidate_revision
                    || entry.candidate_record_digest != expected.candidate_record_sha256
                    || entry.expected_generation != expected.baseline_generation
                    || entry.authorization_digest.as_deref()
                        != Some(
                            crate::skills::lifecycle::authorize::authorization_pin(&proposal.proposal_id, auth)
                                .as_str(),
                        )
                {
                    return Err(ExperimentProductError::refused(
                        ExperimentProductFailureKind::EvidenceUnavailable,
                        "publication observation differs from the exact authorization",
                    ));
                }
                match entry.state {
                    TransactionState::Prepared => ExperimentPublicationStatus::PreparedUncertain,
                    TransactionState::Aborted => ExperimentPublicationStatus::Aborted,
                    TransactionState::Committed
                        if crate::skills::lifecycle::product::recorded_publication_status(proposal.status)
                            && proposal.publication.is_some() =>
                    {
                        let publication = proposal.publication.as_ref().ok_or_else(|| {
                            ExperimentProductError::refused(
                                ExperimentProductFailureKind::EvidenceUnavailable,
                                "committed proposal has no actual publication record",
                            )
                        })?;
                        if publication.transaction_id != id.as_str()
                            || publication.revision_id != entry.candidate_revision_id
                            || publication.generation != entry.new_generation
                            || Some(publication.committed_at) != entry.finished_at
                        {
                            return Err(ExperimentProductError::refused(
                                ExperimentProductFailureKind::EvidenceUnavailable,
                                "committed publication record differs from its journal",
                            ));
                        }
                        ExperimentPublicationStatus::Committed
                    }
                    TransactionState::Committed => ExperimentPublicationStatus::RepairRequired,
                }
            }
        };
        Ok(ExperimentPublicationObservation {
            status,
            transaction,
            authorized_attempt: Some(auth.attempt),
            attempts,
            proposal_committed: proposal.status == ProposalStatus::Committed,
            provenance: "exact_sealed_subject_authorization_and_publication_journal; current_active_binding_not_implied",
        })
    }
}
fn unavailable_publication() -> ExperimentPublicationObservation {
    ExperimentPublicationObservation {
        status: ExperimentPublicationStatus::ObservationUnavailable,
        transaction: None,
        authorized_attempt: None,
        attempts: Vec::new(),
        proposal_committed: false,
        provenance: "missing_or_invalid_exact_publication_evidence",
    }
}
