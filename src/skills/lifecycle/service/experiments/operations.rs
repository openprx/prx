//! Existing proposer, isolated evaluator and artifact-verifier adapters.
use super::{
    Arc, CancellationToken, Config, Context, ExperimentEvent, ExperimentJournal, ExperimentOperation, Result,
    SkillsLifecycleService, ensure,
};
use crate::skills::lifecycle::experiment::{ExperimentRunBinding, ExperimentStageKind};
use crate::skills::lifecycle::{
    AccountingOwner, AgentCaseExecutor, EvalSplit, ProposeOutcome, ProposeRequest, ProposerBudget, ProposerModel,
};

pub(super) struct StageEvidence {
    pub pending_gate: Option<Box<super::gate_review::GateReviewPending>>,
    pub record_id: String,
    pub digest: String,
    pub successful: bool,
    pub scratch_bytes: u64,
}
pub(super) fn proposal_request_id(journal: &ExperimentJournal, stage: &str) -> String {
    // Length-independent, immutable and owner-scoped by proposal_id_of.
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(journal.plan.digest());
    hash.update(b"\0");
    hash.update(stage);
    format!("experiment-{}", hex::encode(hash.finalize()))
}
pub(super) fn proposal_evidence(proposal: &crate::skills::lifecycle::SkillProposal) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut value = serde_json::json!({"proposal_id":proposal.proposal_id,
        "skill_id":proposal.skill_id, "baseline":proposal.baseline_revision,
        "generation":proposal.expected_generation, "candidate":proposal.candidate_revision,
        "author":proposal.author, "budget":proposal.budget, "patterns":proposal.requested_patterns});
    // Preserve historical M1 receipt identities while binding the exact diff
    // supplied as development evidence by the new role-specific proposer.
    if proposal.author.system_prompt_version == "h2-development-proposer-v1" {
        value
            .as_object_mut()
            .context("proposal evidence is not an object")?
            .insert("development_diff".into(), serde_json::to_value(&proposal.diff)?);
    }
    let bytes = serde_json::to_vec(&value)?;
    let mut hash = Sha256::new();
    hash.update(b"prx-experiment-proposal-evidence-v1\0");
    hash.update(bytes);
    Ok(hex::encode(hash.finalize()))
}
impl SkillsLifecycleService {
    pub(super) fn experiment_proposal(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
    ) -> Result<crate::skills::lifecycle::SkillProposal> {
        let id = crate::skills::lifecycle::proposal::proposal_id_of(
            self.owner_scope(),
            &proposal_request_id(journal, stage),
        );
        let proposal = self.proposal_of(&id)?;
        let evidence = journal
            .entries
            .iter()
            .find_map(|entry| match &entry.event {
                ExperimentEvent::Settled {
                    stage: name,
                    reference: Some(reference),
                    evidence_sha256,
                    successful: true,
                    ..
                } if name == stage && reference == &id => Some(evidence_sha256),
                _ => None,
            })
            .context("proposal stage has no bound successful evidence")?;
        ensure!(
            &proposal_evidence(&proposal)? == evidence,
            "proposal candidate binding changed"
        );
        let pins = &journal.plan.spec().pins;
        ensure!(
            proposal.skill_id == pins.skill_id
                && proposal.baseline_revision.as_ref() == Some(&pins.baseline_revision)
                && proposal.expected_generation == pins.baseline_generation
                && proposal.author.model == pins.model,
            "proposal does not match admitted baseline/model"
        );
        Ok(proposal)
    }
    pub(crate) fn experiment_qualified(&self, journal: &ExperimentJournal) -> Result<bool> {
        if !journal.state.qualified(&journal.plan) {
            return Ok(false);
        }
        let mut splits = std::collections::BTreeMap::new();
        for stage in &journal.plan.spec().stages {
            if stage.kind == ExperimentStageKind::GateReview {
                self.verify_gate_review_history(journal, &stage.id)?;
                continue;
            }
            if stage.kind != ExperimentStageKind::IndependentVerification {
                continue;
            }
            let (reference, digest) = journal
                .entries
                .iter()
                .find_map(|entry| match &entry.event {
                    ExperimentEvent::Settled {
                        stage: name,
                        reference: Some(reference),
                        evidence_sha256,
                        ..
                    } if name == &stage.id => Some((reference, evidence_sha256)),
                    _ => None,
                })
                .context("verifier stage has no evidence")?;
            let mut matched = false;
            for dependency in &stage.dependencies {
                if let Ok(verified) = self.verified_stage_evaluation(journal, dependency) {
                    if verified.receipt.record_id == *reference
                        && verified.receipt_digest == *digest
                        && verified.outcome.is_pass()
                    {
                        splits.insert(verified.receipt.split, verified.receipt.proposal_evaluation.clone());
                        matched = true;
                    }
                }
            }
            if !matched {
                return Ok(false);
            }
        }
        let pins = &journal.plan.spec().pins;
        if !splits.contains_key(&pins.verification_split) {
            return Ok(false);
        }
        if pins.holdout_split_digest.is_some() {
            let validation = splits.get(&EvalSplit::Validation).and_then(Option::as_ref);
            let holdout = splits.get(&EvalSplit::Holdout).and_then(Option::as_ref);
            if validation.is_none() || validation != holdout {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub(super) fn validate_stage_operation(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
        operation: &ExperimentOperation,
    ) -> Result<()> {
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == stage)
            .context("unknown stage")?;
        match operation {
            ExperimentOperation::Proposal {
                max_input_bytes,
                max_output_bytes,
                timeout_ms,
                ..
            }
            | ExperimentOperation::DevelopmentProposal {
                max_input_bytes,
                max_output_bytes,
                timeout_ms,
                ..
            } => {
                ProposerBudget::new(
                    *max_input_bytes,
                    *max_output_bytes,
                    std::time::Duration::from_millis(*timeout_ms),
                )?;
                ensure!(
                    u64::try_from(*max_output_bytes)? <= declared.allocation.output_bytes,
                    "proposal output exceeds stage allocation"
                );
                let pins = &journal.plan.spec().pins;
                let baseline = self.store.read_revision(
                    &pins.skill_id,
                    &pins.baseline_revision,
                    Some(&pins.baseline_record_digest),
                )?;
                let retained = baseline.bundle.files().iter().try_fold(0_u64, |total, file| {
                    total
                        .checked_add(u64::try_from(file.bytes().len())?)
                        .context("baseline bundle size overflow")
                })?;
                ensure!(
                    retained
                        .checked_add(u64::try_from(*max_output_bytes)?)
                        .is_some_and(|bytes| bytes <= declared.allocation.scratch_bytes),
                    "proposal candidate storage exceeds stage allowance"
                );
                if let ExperimentOperation::DevelopmentProposal {
                    role,
                    probe_stages,
                    parent_selection,
                    ..
                } = operation
                {
                    let mut expected_operation = operation.clone();
                    Self::pin_lesson_operation(
                        journal.plan.spec().pins.lesson_snapshot.as_ref(),
                        stage,
                        &mut expected_operation,
                    )?;
                    ensure!(expected_operation == *operation, "development lesson input pin changed");
                    self.development_context(journal, stage, *role, probe_stages, parent_selection.as_deref())?;
                }
            }
            ExperimentOperation::Evaluation { proposal_stage, split } => {
                ensure!(
                    declared.dependencies.contains(proposal_stage),
                    "evaluation proposal must be a direct dependency"
                );
                ensure!(
                    *split == EvalSplit::Development || *split == journal.plan.spec().pins.verification_split,
                    "evaluation split was not admitted"
                );
                self.experiment_proposal(journal, proposal_stage)?;
                let suite = self.store.read_suite(&journal.plan.spec().pins.suite_digest)?;
                ensure!(
                    suite
                        .suite()
                        .required_tools
                        .iter()
                        .all(|tool| matches!(tool.as_str(), "skill_read" | "file_read" | "file_write" | "file_edit")),
                    "experiment evaluation tool adapter is not admitted"
                );
            }
            ExperimentOperation::Verify { evaluation_stage } => {
                ensure!(
                    declared.dependencies.contains(evaluation_stage),
                    "verification evaluation must be a direct dependency"
                );
                self.verified_stage_evaluation(journal, evaluation_stage)?;
            }
            ExperimentOperation::DevelopmentProbe { proposal_stage } => {
                self.validate_development_probe(journal, stage, proposal_stage.as_deref())?;
            }
            ExperimentOperation::DevelopmentSelection {
                probe_stages,
                parent_selections,
            } => {
                self.choose_development_candidate(journal, stage, probe_stages, parent_selections)?;
            }
            ExperimentOperation::GateReview { .. } => self.validate_gate_review(journal, stage, operation)?,
            ExperimentOperation::SelectedEvaluation {
                selection_stage,
                split,
                validation_stage,
            } => {
                ensure!(
                    declared.dependencies.contains(selection_stage),
                    "selection is not a direct dependency"
                );
                ensure!(
                    *split == journal.plan.spec().pins.verification_split && validation_stage.is_none()
                        || *split == EvalSplit::Holdout && validation_stage.is_some(),
                    "final split was not admitted"
                );
                self.selected_development_proposal(journal, selection_stage)?;
                if let Some(validation) = validation_stage {
                    self.holdout_binding(journal, stage, selection_stage, validation)?;
                }
            }
        }
        Ok(())
    }
    pub(super) fn verified_stage_evaluation(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
    ) -> Result<crate::skills::lifecycle::VerifiedEvaluation> {
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == stage)
            .context("missing evaluation stage")?;
        ensure!(
            declared.kind == ExperimentStageKind::SolverTrial,
            "verifier dependency is not an evaluation"
        );
        let (reference, digest) = journal
            .entries
            .iter()
            .find_map(|entry| match &entry.event {
                ExperimentEvent::Settled {
                    stage: name,
                    reference: Some(reference),
                    evidence_sha256,
                    ..
                } if name == stage => Some((reference, evidence_sha256)),
                _ => None,
            })
            .context("evaluation stage has no persisted receipt")?;
        let verified = self.store.verify_evaluation(reference, Some(digest))?;
        let pins = &journal.plan.spec().pins;
        let expected = ExperimentRunBinding {
            experiment_sha256: journal.plan.digest().into(),
            stage: stage.into(),
            input_sha256: declared.input_sha256.clone(),
        };
        let receipt = &verified.receipt;
        ensure!(
            receipt.proposal_evaluation.as_ref().is_none_or(|binding| {
                binding.evaluation_epoch > 0 && binding.evaluating_since <= receipt.started_at
            }),
            "evaluation has an invalid sealed proposal window"
        );
        ensure!(
            receipt.experiment.as_ref() == Some(&expected),
            "evaluation is from another experiment, stage or input"
        );
        ensure!(
            receipt.owner_scope == *self.owner_scope()
                && receipt.suite_digest == pins.suite_digest
                && receipt.skill_id == pins.skill_id
                && receipt.observed_generation == pins.baseline_generation
                && receipt.end_generation == pins.baseline_generation
                && receipt.baseline.as_ref().map(|arm| &arm.revision_id) == Some(&pins.baseline_revision)
                && (receipt.split == pins.verification_split
                    || receipt.split == EvalSplit::Holdout && pins.holdout_split_digest.is_some()),
            "evaluation does not match admitted independent verification"
        );
        ensure!(
            receipt.started_at >= journal.plan.spec().created_at
                && receipt.finished_at <= journal.plan.spec().expires_at,
            "evaluation escaped immutable experiment deadline"
        );
        // Candidate identity must be one actually generated by an admitted proposal dependency.
        let mut proposal_stages = declared.dependencies.clone();
        for dependency in &declared.dependencies {
            if let Ok(Some(proposal)) = self.selected_development_proposal(journal, dependency) {
                proposal_stages.push(proposal);
            }
        }
        let matching_candidate = proposal_stages
            .iter()
            .filter_map(|dependency| self.experiment_proposal(journal, dependency).ok())
            .any(|proposal| {
                proposal.candidate_revision.as_ref() == Some(&receipt.candidate.revision_id)
                    && receipt.proposal_evaluation.as_ref().map_or_else(
                        || pins.holdout_split_digest.is_none(),
                        |binding| binding.proposal_id == proposal.proposal_id,
                    )
            });
        ensure!(
            matching_candidate,
            "evaluation candidate is not from the admitted proposal"
        );
        Ok(verified)
    }

    /// Additional holdout is never an entry point to reopen or review a proposal.
    fn holdout_binding(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
        selection: &str,
        validation: &str,
    ) -> Result<crate::skills::lifecycle::ProposalEvaluationBinding> {
        let pins = &journal.plan.spec().pins;
        ensure!(
            pins.verification_split == EvalSplit::Validation && pins.holdout_split_digest.is_some(),
            "additional holdout was not admitted"
        );
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == stage)
            .context("holdout stage missing")?;
        let primary = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == validation)
            .context("validation stage missing")?;
        ensure!(
            primary.input_sha256
                == (ExperimentOperation::SelectedEvaluation {
                    selection_stage: selection.into(),
                    split: EvalSplit::Validation,
                    validation_stage: None,
                })
                .digest()?,
            "validation belongs to another selection or operation"
        );
        let verified = self.verified_stage_evaluation(journal, validation)?;
        ensure!(
            verified.outcome.is_pass() && verified.receipt.split == EvalSplit::Validation,
            "holdout requires passing validation"
        );
        let verify_digest = (ExperimentOperation::Verify {
            evaluation_stage: validation.into(),
        })
        .digest()?;
        ensure!(
            declared.dependencies.iter().any(|dependency| {
                journal.plan.spec().stages.iter().any(|value| {
                    &value.id == dependency
                        && value.kind == ExperimentStageKind::IndependentVerification
                        && value.input_sha256 == verify_digest
                        && value.dependencies.contains(&validation.to_owned())
                        && journal
                            .state
                            .stages
                            .get(dependency)
                            .is_some_and(super::StageState::successful)
                        && journal.entries.iter().any(|entry| matches!(&entry.event,
                            ExperimentEvent::Settled { stage, reference: Some(reference), evidence_sha256, successful: true, .. }
                            if stage == dependency && reference == &verified.receipt.record_id && evidence_sha256 == &verified.receipt_digest))
                })
            }),
            "holdout has no successful direct validation verifier"
        );
        let selected = self
            .selected_development_proposal(journal, selection)?
            .context("holdout has no selected proposal")?;
        let proposal = self.experiment_proposal(journal, &selected)?;
        Ok(self.require_validation_epoch(&proposal.proposal_id, &verified)?)
    }
    pub(super) async fn execute_stage_operation(
        &self,
        config: &Config,
        journal: &ExperimentJournal,
        stage: &str,
        operation: &ExperimentOperation,
        cancel: &CancellationToken,
    ) -> Result<StageEvidence> {
        let runtime = crate::runtime::experiment_stage::current().context("stage has no accounting binding")?;
        runtime.ensure_active()?;
        runtime.watch()?;
        match operation {
            ExperimentOperation::Proposal {
                patterns,
                max_input_bytes,
                max_output_bytes,
                timeout_ms,
            } => {
                let model = crate::runtime::evolution_model::EvolutionModel::from_config(
                    config,
                    crate::runtime::evolution_model::EvolutionPurpose::Proposer,
                )?;
                ensure!(
                    model.binding() == &journal.plan.spec().pins.model,
                    "proposal model changed"
                );
                let request = ProposeRequest {
                    request_id: proposal_request_id(journal, stage),
                    skill_id: journal.plan.spec().pins.skill_id.clone(),
                    pattern_ids: patterns.clone(),
                    budget: ProposerBudget::new(
                        *max_input_bytes,
                        *max_output_bytes,
                        std::time::Duration::from_millis(*timeout_ms),
                    )?,
                };
                let model = Arc::new(super::guarded_proposer::GuardedProposer {
                    store: Arc::clone(&self.store),
                    model,
                    plan: journal.plan.clone(),
                    proposal_id: crate::skills::lifecycle::proposal::proposal_id_of(
                        self.owner_scope(),
                        &request.request_id,
                    ),
                });
                let outcome = self.owned_propose_revision(request, model, cancel).await?;
                let proposal = match outcome {
                    ProposeOutcome::Staged(proposal)
                    | ProposeOutcome::Reused(proposal)
                    | ProposeOutcome::Failed(proposal)
                    | ProposeOutcome::Cancelled(proposal)
                    | ProposeOutcome::Stale(proposal) => proposal,
                    ProposeOutcome::Suppressed { .. } => anyhow::bail!("proposal suppressed before execution"),
                };
                let bytes = serde_json::to_vec(&proposal)?;
                runtime.account_output(bytes.len())?;
                let digest = proposal_evidence(&proposal)?;
                let scratch_bytes = match proposal.candidate_revision.as_ref() {
                    Some(_) => self
                        .store
                        .read_candidate_bundle(&proposal.proposal_id)?
                        .files()
                        .iter()
                        .try_fold(0_u64, |sum, file| {
                            sum.checked_add(u64::try_from(file.bytes().len()).map_err(|_| ())?)
                                .ok_or(())
                        })
                        .map_err(|()| anyhow::anyhow!("candidate size overflow"))?,
                    None => 0,
                };
                Ok(StageEvidence {
                    pending_gate: None,
                    record_id: proposal.proposal_id,
                    digest,
                    successful: proposal.candidate_revision.is_some(),
                    scratch_bytes,
                })
            }
            ExperimentOperation::Evaluation { proposal_stage, split } => {
                self.execute_proposal_evaluation(config, journal, proposal_stage, *split, None, cancel)
                    .await
            }
            ExperimentOperation::Verify { evaluation_stage } => {
                let verified = self.verified_stage_evaluation(journal, evaluation_stage)?;
                Ok(StageEvidence {
                    pending_gate: None,
                    record_id: verified.receipt.record_id,
                    digest: verified.receipt_digest,
                    successful: verified.outcome.is_pass(),
                    scratch_bytes: 0,
                })
            }
            ExperimentOperation::DevelopmentProbe { proposal_stage } => {
                self.execute_development_probe(config, journal, stage, proposal_stage.as_deref(), cancel)
                    .await
            }
            ExperimentOperation::DevelopmentProposal {
                role,
                patterns,
                probe_stages,
                parent_selection,
                lesson_context_sha256: _,
                max_input_bytes,
                max_output_bytes,
                timeout_ms,
            } => {
                self.execute_development_proposal(
                    config,
                    journal,
                    stage,
                    operation,
                    *role,
                    patterns.clone(),
                    probe_stages,
                    parent_selection.as_deref(),
                    ProposerBudget::new(
                        *max_input_bytes,
                        *max_output_bytes,
                        std::time::Duration::from_millis(*timeout_ms),
                    )?,
                    cancel,
                )
                .await
            }
            ExperimentOperation::DevelopmentSelection {
                probe_stages,
                parent_selections,
            } => self.choose_development_candidate(journal, stage, probe_stages, parent_selections),
            ExperimentOperation::GateReview { .. } => {
                self.execute_gate_review(config, journal, stage, operation, cancel)
                    .await
            }
            ExperimentOperation::SelectedEvaluation {
                selection_stage,
                split,
                validation_stage,
            } => match self.selected_development_proposal(journal, selection_stage)? {
                Some(proposal_stage) => {
                    let expected = validation_stage
                        .as_deref()
                        .map(|validation| self.holdout_binding(journal, stage, selection_stage, validation))
                        .transpose()?;
                    self.execute_proposal_evaluation(
                        config,
                        journal,
                        &proposal_stage,
                        *split,
                        expected.as_ref(),
                        cancel,
                    )
                    .await
                }
                None => Ok(StageEvidence {
                    pending_gate: None,
                    record_id: "baseline".into(),
                    digest: journal.plan.spec().pins.baseline_record_digest.clone(),
                    successful: false,
                    scratch_bytes: 0,
                }),
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_proposal_evaluation(
        &self,
        config: &Config,
        journal: &ExperimentJournal,
        proposal_stage: &str,
        split: EvalSplit,
        expected: Option<&crate::skills::lifecycle::ProposalEvaluationBinding>,
        cancel: &CancellationToken,
    ) -> Result<StageEvidence> {
        let proposal = self.experiment_proposal(journal, proposal_stage)?;
        if expected.is_none() && proposal.status != crate::skills::lifecycle::ProposalStatus::Evaluating {
            self.review_source(&proposal.proposal_id, "PRXExperiment")?;
            self.begin_evaluation(&proposal.proposal_id, "admitted bounded experiment")?;
        }
        let scratch = tempfile::Builder::new()
            .prefix("prx-experiment-eval-")
            .tempdir_in(&config.workspace_dir)?;
        // The evaluator owns child workers that can outlive a dropped
        // waiter. Preserve their parent directory on every error.
        let scratch = scratch.keep();
        let executor = AgentCaseExecutor::new(
            config.clone(),
            scratch.clone(),
            0.0,
            AccountingOwner::new(config.workspace_dir.clone()),
        );
        let evaluation = self
            .evaluate_in_epoch(&proposal.proposal_id, split, expected, &executor, cancel)
            .await?;
        let runtime = crate::runtime::experiment_stage::current().context("stage has no accounting binding")?;
        runtime.account_output(serde_json::to_vec(&evaluation.receipt)?.len())?;
        ensure!(
            std::fs::read_dir(&scratch)?.next().is_none(),
            "trial workspaces remain quarantined; cleanup is unconfirmed"
        );
        std::fs::remove_dir(&scratch)?;
        Ok(StageEvidence {
            pending_gate: None,
            record_id: evaluation.receipt.record_id,
            digest: evaluation.receipt_digest,
            successful: true,
            scratch_bytes: 0,
        })
    }
}
