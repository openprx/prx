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
    pub record_id: String,
    pub digest: String,
    pub successful: bool,
    pub scratch_bytes: u64,
}
fn proposal_request_id(journal: &ExperimentJournal, stage: &str) -> String {
    // Length-independent, immutable and owner-scoped by proposal_id_of.
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(journal.plan.digest());
    hash.update(b"\0");
    hash.update(stage);
    format!("experiment-{}", hex::encode(hash.finalize()))
}
fn proposal_evidence(proposal: &crate::skills::lifecycle::SkillProposal) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&serde_json::json!({"proposal_id":proposal.proposal_id,
        "skill_id":proposal.skill_id, "baseline":proposal.baseline_revision,
        "generation":proposal.expected_generation, "candidate":proposal.candidate_revision,
        "author":proposal.author, "budget":proposal.budget, "patterns":proposal.requested_patterns}))?;
    let mut hash = Sha256::new();
    hash.update(b"prx-experiment-proposal-evidence-v1\0");
    hash.update(bytes);
    Ok(hex::encode(hash.finalize()))
}
impl SkillsLifecycleService {
    fn experiment_proposal(
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
        for stage in &journal.plan.spec().stages {
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
                        matched = true;
                    }
                }
            }
            if !matched {
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
        }
        Ok(())
    }
    fn verified_stage_evaluation(
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
                && receipt.split == pins.verification_split,
            "evaluation does not match admitted independent verification"
        );
        ensure!(
            receipt.started_at >= journal.plan.spec().created_at
                && receipt.finished_at <= journal.plan.spec().expires_at,
            "evaluation escaped immutable experiment deadline"
        );
        // Candidate identity must be one actually generated by an admitted proposal dependency.
        let matching_candidate = declared
            .dependencies
            .iter()
            .filter_map(|dependency| self.experiment_proposal(journal, dependency).ok())
            .any(|proposal| proposal.candidate_revision.as_ref() == Some(&receipt.candidate.revision_id));
        ensure!(
            matching_candidate,
            "evaluation candidate is not from the admitted proposal"
        );
        Ok(verified)
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
                    record_id: proposal.proposal_id,
                    digest,
                    successful: proposal.candidate_revision.is_some(),
                    scratch_bytes,
                })
            }
            ExperimentOperation::Evaluation { proposal_stage, split } => {
                let proposal = self.experiment_proposal(journal, proposal_stage)?;
                if proposal.status != crate::skills::lifecycle::ProposalStatus::Evaluating {
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
                let evaluation = self.evaluate(&proposal.proposal_id, *split, &executor, cancel).await?;
                runtime.account_output(serde_json::to_vec(&evaluation.receipt)?.len())?;
                ensure!(
                    std::fs::read_dir(&scratch)?.next().is_none(),
                    "trial workspaces remain quarantined; cleanup is unconfirmed"
                );
                std::fs::remove_dir(&scratch)?;
                Ok(StageEvidence {
                    record_id: evaluation.receipt.record_id,
                    digest: evaluation.receipt_digest,
                    successful: true,
                    scratch_bytes: 0,
                })
            }
            ExperimentOperation::Verify { evaluation_stage } => {
                let verified = self.verified_stage_evaluation(journal, evaluation_stage)?;
                Ok(StageEvidence {
                    record_id: verified.receipt.record_id,
                    digest: verified.receipt_digest,
                    successful: verified.outcome.is_pass(),
                    scratch_bytes: 0,
                })
            }
        }
    }
}
