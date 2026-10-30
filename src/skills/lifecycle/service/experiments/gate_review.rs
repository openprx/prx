//! Private gate review borrows the admitted stage and existing gate/epoch owners.
//! Completed history is a sealed witness, never current publication authority.
use super::operations::StageEvidence;
use super::{
    CancellationToken, Config, Context, ExperimentEvent, ExperimentJournal, ExperimentOperation, Result,
    SkillsLifecycleService, ensure,
};
use crate::skills::lifecycle::experiment::{
    ExperimentRunBinding, GateReviewPhysical, GateReviewWitness, GateVerificationWindow,
};
use crate::skills::lifecycle::{
    EvalSplit, EvaluationPin, GatePolicy, GateReviewSubject, ModelJudge, ProposalEvaluationBinding, ProposerModel,
    SkillProposal,
};
use chrono::Utc;

/// Untrusted exact pins supplied by the historical adoption owner.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::skills::lifecycle) struct GateReviewPin {
    pub stage: String,
    pub witness_sha256: String,
    pub gate_id: String,
    pub gate_sha256: String,
}
/// Private constructor and no Deserialize/VerifiedGate conversion. These facts
/// describe past completion only, never current publication authorization.
#[derive(Clone, Debug)]
pub(in crate::skills::lifecycle) struct VerifiedGateReviewHistory {
    witness: GateReviewWitness,
    witness_sha256: String,
    checked: GateVerificationWindow,
    physical: GateReviewPhysical,
    gate_decided_at: chrono::DateTime<Utc>,
    gate_expires_at: chrono::DateTime<Utc>,
}
// The immediately following adoption package consumes this explicit internal
// contract; the current qualifier already constructs the verified result.
#[allow(dead_code)]
impl VerifiedGateReviewHistory {
    pub const fn binding(&self) -> &crate::skills::lifecycle::GateExperimentBinding {
        &self.witness.binding
    }
    pub const fn subject(&self) -> &GateReviewSubject {
        &self.witness.subject
    }
    pub const fn physical(&self) -> &GateReviewPhysical {
        &self.physical
    }
    pub const fn verification_window(&self) -> &GateVerificationWindow {
        &self.checked
    }
    pub const fn gate_decided_at(&self) -> chrono::DateTime<Utc> {
        self.gate_decided_at
    }
    pub const fn gate_expires_at(&self) -> chrono::DateTime<Utc> {
        self.gate_expires_at
    }
    pub fn pin(&self) -> GateReviewPin {
        GateReviewPin {
            stage: self.witness.binding.experiment.stage.clone(),
            witness_sha256: self.witness_sha256.clone(),
            gate_id: self.witness.gate_id.clone(),
            gate_sha256: self.witness.gate_digest.clone(),
        }
    }
}

/// The gate usage is a lower bound from consult_judge's per-call physical
/// TallyCounters (merged with reported usage), not proof of its actual owner.
/// Only the drained physical ledger below establishes owner and completion.
pub(super) struct GateReviewPending {
    witness: GateReviewWitness,
    judge_usage: Option<crate::skills::lifecycle::ReportedUsage>,
}

struct GateInputs {
    proposal_stage: String,
    proposal: SkillProposal,
    epoch: ProposalEvaluationBinding,
    evaluations: Vec<EvaluationPin>,
}
impl GateInputs {
    fn subject(&self, suite: &str) -> Result<GateReviewSubject> {
        Ok(GateReviewSubject {
            proposal_id: self.proposal.proposal_id.clone(),
            suite_digest: suite.into(),
            evaluations: self
                .evaluations
                .iter()
                .map(|pin| (pin.evaluation_id.clone(), pin.receipt_digest.clone()))
                .collect(),
            operation: self.proposal.operation,
            skill_id: self.proposal.skill_id.clone(),
            baseline_revision: self.proposal.baseline_revision.clone(),
            candidate_revision: self
                .proposal
                .candidate_revision
                .clone()
                .context("gate candidate is missing")?,
            expected_generation: self.proposal.expected_generation,
            evaluation_epoch: self.epoch.evaluation_epoch,
            evaluating_since: self.epoch.evaluating_since,
        })
    }
}
fn declared_input(journal: &ExperimentJournal, name: &str, operation: &ExperimentOperation) -> Result<()> {
    let stage = journal
        .plan
        .spec()
        .stages
        .iter()
        .find(|stage| stage.id == name)
        .context("gate dependency is absent")?;
    ensure!(
        stage.kind == operation.kind() && stage.input_sha256 == operation.digest()?,
        "gate dependency operation changed"
    );
    Ok(())
}
fn verifier_evidence(journal: &ExperimentJournal, verifier: &str, evaluation: &str, pin: &EvaluationPin) -> Result<()> {
    declared_input(
        journal,
        verifier,
        &ExperimentOperation::Verify {
            evaluation_stage: evaluation.into(),
        },
    )?;
    let stage = journal
        .plan
        .spec()
        .stages
        .iter()
        .find(|stage| stage.id == verifier)
        .context("gate verifier missing")?;
    ensure!(
        stage.dependencies.contains(&evaluation.to_owned())
            && journal
                .state
                .stages
                .get(verifier)
                .is_some_and(|stage| stage.successful()),
        "gate verifier is not a completed direct verifier"
    );
    ensure!(
        journal.entries.iter().any(|entry| matches!(&entry.event,
        ExperimentEvent::Settled { stage, reference:Some(reference), evidence_sha256, successful:true, .. }
        if stage == verifier && reference == &pin.evaluation_id && evidence_sha256 == &pin.receipt_digest)),
        "gate verifier does not pin the exact independent receipt"
    );
    Ok(())
}
impl SkillsLifecycleService {
    /// Exact source-owned historical proof, not a caller-created witness.
    #[allow(dead_code)] // Cross-branch adoption API, independently tested here.
    pub(in crate::skills::lifecycle) fn verify_gate_review_witness(
        &self,
        experiment_id: &crate::skills::lifecycle::experiment::ExperimentId,
        experiment_sha256: &str,
        pin: &crate::skills::lifecycle::GateReviewPin,
    ) -> Result<crate::skills::lifecycle::VerifiedGateReviewHistory> {
        let journal = self.inspect_experiment(experiment_id, experiment_sha256)?;
        let verified = self.verify_gate_review_history(&journal, &pin.stage)?;
        ensure!(verified.pin() == *pin, "historical gate confirmation pins changed");
        Ok(verified)
    }
    fn gate_review_inputs(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
        operation: &ExperimentOperation,
    ) -> Result<GateInputs> {
        let ExperimentOperation::GateReview {
            selection_stage,
            validation_stage,
            validation_verifier,
            holdout_stage,
            holdout_verifier,
        } = operation
        else {
            anyhow::bail!("gate stage has another operation");
        };
        let pins = &journal.plan.spec().pins;
        ensure!(
            pins.verification_split == EvalSplit::Validation && pins.holdout_split_digest.is_some(),
            "gate requires admitted dual-independent verification"
        );
        declared_input(journal, stage, operation)?;
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == stage)
            .context("gate stage missing")?;
        for dependency in [
            selection_stage,
            validation_stage,
            validation_verifier,
            holdout_stage,
            holdout_verifier,
        ] {
            ensure!(
                declared.dependencies.contains(dependency),
                "gate input is not a direct frozen dependency"
            );
        }
        declared_input(
            journal,
            validation_stage,
            &ExperimentOperation::SelectedEvaluation {
                selection_stage: selection_stage.clone(),
                split: EvalSplit::Validation,
                validation_stage: None,
            },
        )?;
        declared_input(
            journal,
            holdout_stage,
            &ExperimentOperation::SelectedEvaluation {
                selection_stage: selection_stage.clone(),
                split: EvalSplit::Holdout,
                validation_stage: Some(validation_stage.clone()),
            },
        )?;
        let proposal_stage = self
            .selected_development_proposal(journal, selection_stage)?
            .context("baseline cannot enter gate review")?;
        let proposal = self.experiment_proposal(journal, &proposal_stage)?;
        let validation = self.verified_stage_evaluation(journal, validation_stage)?;
        let holdout = self.verified_stage_evaluation(journal, holdout_stage)?;
        ensure!(
            validation.receipt.split == EvalSplit::Validation
                && holdout.receipt.split == EvalSplit::Holdout
                && validation.outcome.is_pass()
                && holdout.outcome.is_pass(),
            "gate requires both passing independent splits"
        );
        let epoch = validation
            .receipt
            .proposal_evaluation
            .clone()
            .context("gate validation has no sealed proposal epoch")?;
        ensure!(
            holdout.receipt.proposal_evaluation.as_ref() == Some(&epoch)
                && epoch.proposal_id == proposal.proposal_id
                && epoch.evaluation_epoch > 0,
            "gate split proposal/epoch/window changed"
        );
        let validation_pin = EvaluationPin {
            evaluation_id: validation.receipt.record_id,
            receipt_digest: validation.receipt_digest,
        };
        let holdout_pin = EvaluationPin {
            evaluation_id: holdout.receipt.record_id,
            receipt_digest: holdout.receipt_digest,
        };
        verifier_evidence(journal, validation_verifier, validation_stage, &validation_pin)?;
        verifier_evidence(journal, holdout_verifier, holdout_stage, &holdout_pin)?;
        let evaluations = vec![validation_pin, holdout_pin];
        Ok(GateInputs {
            proposal_stage,
            proposal,
            epoch,
            evaluations,
        })
    }
    pub(super) fn validate_gate_review(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
        operation: &ExperimentOperation,
    ) -> Result<()> {
        let inputs = self.gate_review_inputs(journal, stage, operation)?;
        ensure!(
            inputs.proposal.evaluation_epoch == inputs.epoch.evaluation_epoch
                && inputs.proposal.evaluating_since == Some(inputs.epoch.evaluating_since),
            "gate's current epoch no longer matches its independent evidence"
        );
        self.store.require_bound_attempts_terminal(&inputs.proposal)?;
        Ok(())
    }
    pub(super) async fn execute_gate_review(
        &self,
        config: &Config,
        journal: &ExperimentJournal,
        stage: &str,
        operation: &ExperimentOperation,
        cancel: &CancellationToken,
    ) -> Result<StageEvidence> {
        let inputs = self.gate_review_inputs(journal, stage, operation)?;
        let runtime = crate::runtime::experiment_stage::current().context("gate has no admitted accounting owner")?;
        runtime.bind_gate_subject(inputs.subject(&journal.plan.spec().pins.suite_digest)?)?;
        let actual = runtime.gate_record_binding(self.owner_scope())?;
        let existing = self.store.recorded_gate_for(&inputs.proposal)?;
        if let Some(existing) = existing {
            ensure!(
                existing.decision.experiment.as_ref() == Some(&actual)
                    && existing.decision.epoch == inputs.epoch.evaluation_epoch,
                "cached gate is not this actual root/stage/subject"
            );
            self.store.require_bound_attempts_terminal(&inputs.proposal)?;
        } else {
            // C's existing proposal lock owns exact triple checking, terminal
            // attempt references and idempotence. No epoch is reopened here.
            let finished = self.finish_evaluation_bound(&inputs.epoch, "both admitted independent splits completed")?;
            ensure!(
                finished.candidate_revision == inputs.proposal.candidate_revision
                    && finished.required_suite.as_deref() == Some(journal.plan.spec().pins.suite_digest.as_str()),
                "bound finish changed the gate subject"
            );
        }
        let model = crate::runtime::evolution_model::EvolutionModel::from_config(
            config,
            crate::runtime::evolution_model::EvolutionPurpose::Judge,
        )?;
        ensure!(
            model.binding() == &journal.plan.spec().pins.model,
            "gate judge model changed"
        );
        let judge = ModelJudge::new(model);
        let outcome = self
            .gate(&inputs.epoch.proposal_id, &inputs.evaluations, Some(&judge), cancel)
            .await?;
        ensure!(
            outcome.gate.decision.experiment.as_ref() == Some(&actual),
            "gate winner has another actual owner"
        );
        let policy_snapshot = self.experiment_policy_snapshot(config);
        ensure!(
            Self::experiment_policy_digest(&policy_snapshot)? == journal.plan.spec().pins.policy_sha256,
            "gate policy snapshot changed"
        );
        let current_verification = if outcome.gate.decision.decision == crate::skills::lifecycle::GateDecision::Accept {
            let started_at = Utc::now();
            self.store.verify_gate(
                &outcome.gate.decision.record_id,
                &outcome.gate.digest,
                &self.settings.gate_policy,
                &journal.plan.spec().pins.suite_digest,
            )?;
            Some(GateVerificationWindow {
                started_at,
                finished_at: Utc::now(),
            })
        } else {
            None
        };
        runtime.ensure_active()?;
        let ExperimentOperation::GateReview {
            selection_stage,
            validation_stage,
            validation_verifier,
            holdout_stage,
            holdout_verifier,
        } = operation
        else {
            anyhow::bail!("gate operation changed before witness construction")
        };
        let witness = GateReviewWitness {
            schema_version: 1,
            binding: actual,
            subject: inputs.subject(&journal.plan.spec().pins.suite_digest)?,
            proposal_stage: inputs.proposal_stage,
            selection_stage: selection_stage.clone(),
            validation_stage: validation_stage.clone(),
            validation_verifier: validation_verifier.clone(),
            holdout_stage: holdout_stage.clone(),
            holdout_verifier: holdout_verifier.clone(),
            gate_id: outcome.gate.decision.record_id.clone(),
            gate_digest: outcome.gate.digest.clone(),
            policy_snapshot,
            current_verification,
            physical: None,
        };
        let gate_bytes = serde_json::to_vec(&outcome.gate.decision)?;
        runtime.account_output(gate_bytes.len())?;
        runtime.account_scratch(u64::try_from(gate_bytes.len())?)?;
        Ok(StageEvidence {
            record_id: witness.gate_id.clone(),
            digest: String::new(),
            successful: outcome.gate.decision.decision == crate::skills::lifecycle::GateDecision::Accept,
            scratch_bytes: u64::try_from(gate_bytes.len())?,
            pending_gate: Some(Box::new(GateReviewPending {
                witness,
                judge_usage: outcome.gate.decision.judge.as_ref().map(|judge| judge.usage),
            })),
        })
    }
    /// The original stage owner calls this only after CleanupScope has drained.
    /// The same ScopeUsage value is subsequently put in StageSettled.
    pub(super) fn finalize_gate_review_after_cleanup(
        &self,
        journal: &ExperimentJournal,
        runtime: &crate::runtime::experiment_stage::ExperimentStageRuntime,
        physical: &crate::cost::reservation::ScopeUsage,
        mut evidence: StageEvidence,
    ) -> Result<StageEvidence> {
        let pending = *evidence
            .pending_gate
            .take()
            .context("gate completion material missing")?;
        ensure!(
            !physical.unresolved && physical.calls == physical.settled_calls,
            "gate completion has unresolved physical work"
        );
        let mut witness = pending.witness;
        let judge_usage = pending.judge_usage.unwrap_or_default();
        ensure!(
            physical.calls == u64::from(judge_usage.physical_requests)
                && physical.settled_calls >= u64::from(judge_usage.settled_requests)
                && physical.attempt_ids.len() == usize::try_from(physical.calls)?,
            "actual stage ledger does not cover the judge's physical tally"
        );
        ensure!(
            !evidence.successful || (physical.calls > 0 && judge_usage.physical_requests > 0),
            "accepted gate has no actual stage-owned judge attempts"
        );
        runtime.ensure_active()?;
        witness.physical = Some(GateReviewPhysical {
            calls: physical.calls,
            settled_calls: physical.settled_calls,
            tokens: physical.tokens,
            micro_usd: physical.micro_usd,
            attempt_ids: physical.attempt_ids.iter().cloned().collect(),
            models: physical.models.clone(),
        });
        let bytes = serde_json::to_vec(&witness)?;
        runtime.account_output(bytes.len())?;
        evidence.scratch_bytes = evidence
            .scratch_bytes
            .checked_add(u64::try_from(bytes.len())?)
            .context("gate retained size overflow")?;
        runtime.account_scratch(evidence.scratch_bytes)?;
        evidence.digest = self.store.write_gate_review(journal, &witness, runtime)?;
        Ok(evidence)
    }
    pub(super) fn verify_gate_review_history(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
    ) -> Result<crate::skills::lifecycle::VerifiedGateReviewHistory> {
        let (gate_id, digest, usage, settled_at) = journal
            .entries
            .iter()
            .find_map(|entry| match &entry.event {
                ExperimentEvent::Settled {
                    stage: name,
                    reference: Some(reference),
                    evidence_sha256,
                    usage,
                    successful: true,
                } if name == stage => Some((reference, evidence_sha256, usage, entry.recorded_at)),
                _ => None,
            })
            .context("gate stage has no successful sealed settlement")?;
        let witness = self.store.read_gate_review(&journal.plan.spec().id, stage, digest)?;
        let operation = ExperimentOperation::GateReview {
            selection_stage: witness.selection_stage.clone(),
            validation_stage: witness.validation_stage.clone(),
            validation_verifier: witness.validation_verifier.clone(),
            holdout_stage: witness.holdout_stage.clone(),
            holdout_verifier: witness.holdout_verifier.clone(),
        };
        let inputs = self.gate_review_inputs(journal, stage, &operation)?;
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == stage)
            .context("gate declaration missing")?;
        let expected = ExperimentRunBinding {
            experiment_sha256: journal.plan.digest().into(),
            stage: stage.into(),
            input_sha256: declared.input_sha256.clone(),
        };
        ensure!(
            witness.binding.owner_scope == *self.owner_scope()
                && witness.binding.experiment == expected
                && witness.subject == inputs.subject(&journal.plan.spec().pins.suite_digest)?
                && witness.proposal_stage == inputs.proposal_stage
                && witness.gate_id == *gate_id,
            "sealed gate witness subject or root/stage/input changed"
        );
        ensure!(
            Self::experiment_policy_digest(&witness.policy_snapshot)? == journal.plan.spec().pins.policy_sha256,
            "historical policy snapshot differs from original root pin"
        );
        let policy: GatePolicy = serde_json::from_value(
            witness
                .policy_snapshot
                .get("gate")
                .cloned()
                .context("historical policy snapshot has no gate policy")?,
        )?;
        let physical = witness
            .physical
            .as_ref()
            .context("historical gate has no post-cleanup physical snapshot")?
            .clone();
        ensure!(
            physical.calls > 0
                && physical.calls == physical.settled_calls
                && physical.calls == usage.calls
                && physical.tokens == usage.tokens
                && physical.micro_usd == usage.micro_usd
                && physical
                    .attempt_ids
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == usize::try_from(physical.calls)?
                && witness.binding.physical_scope_id.len() == 64
                && witness
                    .binding
                    .physical_scope_id
                    .bytes()
                    .all(|value| value.is_ascii_hexdigit()),
            "historical gate lacks its original settled physical owner"
        );
        let dispatched_at = journal
            .entries
            .iter()
            .find_map(|entry| match &entry.event {
                ExperimentEvent::Dispatched { stage: name } if name == stage => Some(entry.recorded_at),
                _ => None,
            })
            .context("gate has no original dispatch")?;
        let checked = witness.current_verification.clone().ok_or_else(|| {
            crate::skills::lifecycle::GateError::InsufficientTemporalEvidence(
                "gate has no sealed actual verification bracket".into(),
            )
        })?;
        if checked.started_at < journal.plan.spec().created_at || checked.finished_at > journal.plan.spec().expires_at {
            return Err(crate::skills::lifecycle::GateError::InsufficientTemporalEvidence(
                "gate verification escaped the original experiment window".into(),
            )
            .into());
        }
        let (gate_decided_at, gate_expires_at) = self.store.verify_historical_gate_review(
            &witness.gate_id,
            &witness.gate_digest,
            &witness.binding,
            &witness.subject,
            &policy,
            &journal.plan.spec().pins.suite_digest,
            &inputs.evaluations,
            &ModelJudge::identity_for(&journal.plan.spec().pins.model),
            &physical,
            dispatched_at,
            checked.started_at,
            checked.finished_at,
            settled_at,
        )?;
        Ok(VerifiedGateReviewHistory {
            witness,
            witness_sha256: digest.clone(),
            checked,
            physical,
            gate_decided_at,
            gate_expires_at,
        })
    }
}
