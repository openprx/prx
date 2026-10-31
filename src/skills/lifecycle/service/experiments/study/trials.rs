//! Actual controlled case execution and private, artifact-backed grading.
use super::{
    CancellationToken, Config, Context, ExperimentJournal, Result, SkillsLifecycleService, StudyTrialInput, ensure,
};
use crate::skills::lifecycle::experiment::{ExperimentOperation, ExperimentStageKind};
use crate::skills::lifecycle::study::{STUDY_SCHEMA_VERSION, StudyTrialReceipt};
use crate::skills::lifecycle::{
    AccountingOwner, AgentCaseExecutor, CaseExecutor, ExecutionRequest, InstallSkill, RunReceipt,
};
use chrono::Utc;
use std::collections::BTreeSet;
use std::time::Duration;

impl SkillsLifecycleService {
    pub(in crate::skills::lifecycle::service::experiments) fn validate_study_trial(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
        input: &StudyTrialInput,
    ) -> Result<()> {
        ensure!(
            journal
                .plan
                .spec()
                .stages
                .iter()
                .all(|declared| declared.kind == ExperimentStageKind::StudyTrial),
            "study is mixed with qualification or ordinary stages"
        );
        let plan = self
            .store
            .read_study_plan(&journal.plan.spec().id, Some(&input.study_sha256))?;
        ensure!(
            plan.experiment.spec.clone().freeze()?.digest() == journal.plan.digest(),
            "study trial does not belong to its owned plan"
        );
        let pair = plan
            .manifest
            .pairs
            .iter()
            .find(|pair| pair.pair_id == input.pair_id)
            .context("study pair is not frozen")?;
        let expected = ExperimentOperation::StudyTrial { input: input.clone() };
        ensure!(
            pair.stage_id(input.arm) == stage && plan.experiment.operations.get(stage) == Some(&expected),
            "study arm/stage/input changed"
        );
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|declared| declared.id == stage)
            .context("study stage absent")?;
        ensure!(
            declared.kind == ExperimentStageKind::StudyTrial && declared.input_sha256 == expected.digest()?,
            "study operation kind/digest mismatch"
        );
        Ok(())
    }

    pub(in crate::skills::lifecycle::service::experiments) async fn execute_study_trial(
        &self,
        config: &Config,
        journal: &ExperimentJournal,
        stage: &str,
        input: &StudyTrialInput,
        cancel: &CancellationToken,
    ) -> Result<super::super::operations::StageEvidence> {
        self.validate_study_trial(journal, stage, input)?;
        let runtime = crate::runtime::experiment_stage::current().context("study has no admitted stage owner")?;
        ensure!(
            runtime.kind() == ExperimentStageKind::StudyTrial
                && runtime.binding().experiment_sha256 == journal.plan.digest()
                && runtime.binding().stage == stage,
            "study execution is not owned by its actual admitted stage"
        );
        runtime.ensure_active()?;
        let before = self.study_physical(config, journal.plan.digest(), stage)?;
        ensure!(
            before.calls == 0 && before.attempt_ids.is_empty() && !before.unresolved,
            "study stage already has physical work; inspect without replay"
        );
        let plan = self
            .store
            .read_study_plan(&journal.plan.spec().id, Some(&input.study_sha256))?;
        let manifest = &plan.manifest;
        let pair = manifest
            .pairs
            .iter()
            .find(|pair| pair.pair_id == input.pair_id)
            .context("study pair absent")?;
        let case = manifest.case(pair)?;
        let revision = manifest.revision(input.arm);
        let bundle = self
            .store
            .read_revision(&manifest.skill_id, &revision.revision, Some(&revision.record_sha256))?
            .bundle;
        let artifacts: Vec<_> = case
            .validators
            .iter()
            .filter_map(crate::skills::lifecycle::Validator::artifact_path)
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let case_tools = manifest
            .case_tools
            .get(&case.case_id)
            .context("study case tools absent")?;
        let run_id = format!("v01-{}-{stage}", manifest.id.as_str());
        let denied = [self.store.root().to_path_buf()];
        let mut scratch = super::scratch::OwnedStudyScratch::new(&config.workspace_dir, cancel)?;
        let executor = AgentCaseExecutor::new_controlled(
            config.clone(),
            scratch.path().to_path_buf(),
            f64::from(manifest.runtime.model.temperature_milli) / 1000.0,
            AccountingOwner::new(config.workspace_dir.clone()),
        )
        .map_err(|_| anyhow::anyhow!("controlled study case profile is unavailable"))?;
        let request = ExecutionRequest {
            run_id: &run_id,
            prompt: &case.prompt,
            fixtures: &case.fixtures,
            collect_artifacts: &artifacts,
            skill: Some(InstallSkill {
                skill_id: &manifest.skill_id,
                bundle: &bundle,
            }),
            deadline: Duration::from_millis(case.max_elapsed_ms),
            required_tools: case_tools,
            side_effects: manifest.side_effects,
            denied_paths: &denied,
        };
        // Oracle and other-arm observations are deliberately absent from the
        // request. Error text can echo provider/tool data, so keep only class.
        scratch.start()?;
        let observation = executor
            .run_case(&request, cancel)
            .await
            .map_err(|_| anyhow::anyhow!("controlled study case execution or cleanup was not confirmed"))?;
        // This removes only a confirmed quiescent empty parent. Every later
        // flush/grade/retained-write error therefore leaves no orphan parent.
        scratch.confirm_case_removed(observation.cleanup)?;
        runtime.flush().await?;
        // No case DATA/runtime-state tree is concurrently retained while the
        // private result copy is made. Native-write admission below remains
        // conservative/nonrefundable and charges both work and durable copy.

        let _durable_output = runtime.register_scratch(&self.store.study_output_directory(&manifest.id)?, 0)?;
        let physical = self.study_physical(config, journal.plan.digest(), stage)?;
        let mut receipt = StudyTrialReceipt {
            schema_version: STUDY_SCHEMA_VERSION,
            owner_scope: self.owner_scope().clone(),
            input: input.clone(),
            binding: runtime.binding(),
            root_task_id: pair.root_task_id.clone(),
            case_id: case.case_id.clone(),
            repetition: pair.repetition,
            run: RunReceipt::from_observation(&observation),
            validators: Vec::new(),
            measured_and_valid: false,
            invalid_reasons: Vec::new(),
            success: false,
            quality_ppm: 0,
            physical,
            recorded_at: Utc::now(),
        };
        let grade = receipt.compute_grade(manifest, &bundle, &observation.artifacts, &receipt.physical)?;
        receipt.validators = grade.validators;
        receipt.measured_and_valid = grade.measured_and_valid;
        receipt.invalid_reasons = grade.invalid_reasons;
        receipt.success = grade.success;
        receipt.quality_ppm = grade.quality_ppm;
        let mut unique_effects = std::collections::BTreeMap::new();
        for bytes in observation.artifacts.values().filter_map(Option::as_ref) {
            use sha2::{Digest, Sha256};
            unique_effects.insert(hex::encode(Sha256::digest(bytes)), bytes.len());
        }
        let effect_bytes = unique_effects.values().try_fold(0_usize, |total, bytes| {
            total.checked_add(*bytes).context("study evidence size overflow")
        })?;
        let retained_bytes = self
            .store
            .study_trial_record_size(&receipt, &manifest.id)?
            .checked_add(effect_bytes)
            .context("study evidence size overflow")?;
        runtime.account_output(retained_bytes)?;
        runtime.account_scratch(u64::try_from(retained_bytes)?)?;
        self.store
            .write_study_artifacts(journal, &receipt.binding, &observation.artifacts)?;
        let sha256 = self.store.write_study_trial(journal, &receipt, config)?;

        Ok(super::super::operations::StageEvidence {
            pending_gate: None,
            record_id: format!("study:{stage}"),
            digest: sha256,
            // This is execution completion, NEVER the semantic grade. Task
            // failures continue the preregistered complete paired schedule.
            successful: receipt.measured_and_valid,
            scratch_bytes: 0,
        })
    }
}
