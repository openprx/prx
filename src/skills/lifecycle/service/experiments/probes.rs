//! Admission and runtime construction for development-only, single-arm probes.
//! Source review and execution reuse their existing owners; no candidate install.
use super::operations::StageEvidence;
use super::{CancellationToken, Config, Context, ExperimentJournal, Result, SkillsLifecycleService, ensure};
use crate::skills::lifecycle::{AccountingOwner, AgentCaseExecutor, Bundle, EvalSplit};

impl SkillsLifecycleService {
    pub(crate) fn experiment_development_probes(&self, journal: &ExperimentJournal) -> Result<serde_json::Value> {
        ensure!(
            journal.plan.spec().owner_scope == *self.owner_scope(),
            "probe owner mismatch"
        );
        let mut probes = serde_json::Map::new();
        for entry in &journal.entries {
            if let super::ExperimentEvent::Settled {
                stage,
                reference: Some(reference),
                evidence_sha256,
                ..
            } = &entry.event
            {
                if reference != &format!("probe:{stage}") {
                    continue;
                }
                let receipt = self
                    .store
                    .read_development_probe(&journal.plan.spec().id, stage, evidence_sha256)?;
                ensure!(
                    receipt.binding.experiment_sha256 == journal.plan.digest(),
                    "probe belongs to another experiment"
                );
                probes.insert(stage.clone(), serde_json::to_value(receipt)?);
            }
        }
        Ok(serde_json::Value::Object(probes))
    }

    pub(super) fn validate_development_probe(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
        proposal_stage: Option<&str>,
    ) -> Result<()> {
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == stage)
            .context("unknown stage")?;
        if let Some(proposal) = proposal_stage {
            ensure!(
                declared.dependencies.iter().any(|value| value == proposal),
                "probe proposal is not a direct dependency"
            );
            self.experiment_proposal(journal, proposal)?;
        }
        let suite = self.store.read_suite(&journal.plan.spec().pins.suite_digest)?;
        crate::skills::lifecycle::experiment::probe_run_count(&suite)?;
        ensure!(
            suite
                .suite()
                .required_tools
                .iter()
                .all(|tool| matches!(tool.as_str(), "skill_read" | "file_read" | "file_write" | "file_edit")),
            "development tool adapter is not admitted"
        );
        Ok(())
    }

    fn development_bundle(&self, journal: &ExperimentJournal, proposal_stage: Option<&str>) -> Result<Bundle> {
        if let Some(stage) = proposal_stage {
            let proposal = self.experiment_proposal(journal, stage)?;
            self.review_source(&proposal.proposal_id, "PRXExperiment")?;
            Ok(self.store.read_candidate_bundle(&proposal.proposal_id)?)
        } else {
            let pins = &journal.plan.spec().pins;
            Ok(self
                .store
                .read_revision(
                    &pins.skill_id,
                    &pins.baseline_revision,
                    Some(&pins.baseline_record_digest),
                )?
                .bundle)
        }
    }

    pub(super) async fn execute_development_probe(
        &self,
        config: &Config,
        journal: &ExperimentJournal,
        stage: &str,
        proposal_stage: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<StageEvidence> {
        let bundle = self.development_bundle(journal, proposal_stage)?;
        let scratch = tempfile::Builder::new()
            .prefix("prx-development-probe-")
            .tempdir_in(&config.workspace_dir)?
            .keep();
        let executor = AgentCaseExecutor::new(
            config.clone(),
            scratch.clone(),
            0.0,
            AccountingOwner::new(config.workspace_dir.clone()),
        );
        let receipt = self
            .store
            .run_development_probe(
                journal,
                stage,
                proposal_stage.map(str::to_string),
                &bundle,
                &executor,
                cancel,
            )
            .await?;
        let runtime = crate::runtime::experiment_stage::current().context("probe runtime missing")?;
        let retained_bytes = serde_json::to_vec_pretty(&receipt)?
            .len()
            .checked_add(1)
            .context("probe evidence size overflow")?;
        runtime.account_output(retained_bytes)?;
        runtime.account_scratch(u64::try_from(retained_bytes)?)?;
        let digest = self.store.write_development_probe(journal, &receipt)?;
        ensure!(
            std::fs::read_dir(&scratch)?.next().is_none(),
            "probe cleanup is unconfirmed; scratch remains quarantined"
        );
        std::fs::remove_dir(&scratch)?;
        let suite = self.store.read_suite(&journal.plan.spec().pins.suite_digest)?;
        let successful = receipt.complete()
            && crate::skills::lifecycle::experiment::probe_within_budget(&receipt, &suite)
            && receipt.cases.len() == crate::skills::lifecycle::experiment::probe_run_count(&suite)?
            && !cancel.is_cancelled()
            && receipt.cases.iter().all(|case| case.split == EvalSplit::Development);
        Ok(StageEvidence {
            pending_gate: None,
            record_id: format!("probe:{stage}"),
            digest,
            successful,
            scratch_bytes: 0,
        })
    }
}
