//! Crate-controlled V01 plan/admit/run over the original experiment owners.
//! No stage, executor, store record or report grants publication eligibility.
mod origin;
mod reporting;
mod scratch;
mod trials;

use super::{
    Arc, CancellationToken, Config, Context, ExperimentJournal, Result, SkillsLifecycleService, StageState, ensure,
};
use crate::skills::lifecycle::experiment::{
    ExperimentId, ExperimentOperation, ExperimentSpec, ExperimentStage, ExperimentStageKind, InstructionExperimentPlan,
};
use crate::skills::lifecycle::study::{
    STUDY_SCHEMA_VERSION, StudyDraft, StudyExcludedRoot, StudyManifest, StudyPhase, StudyPlan, StudyReport,
    StudyStatus, StudyTrialInput,
};
use crate::skills::lifecycle::{EvalSplit, TransactionId};
use chrono::Utc;
use std::collections::BTreeMap;

impl SkillsLifecycleService {
    pub(crate) fn plan_v01_study(&self, mut draft: StudyDraft, config: &Config) -> Result<StudyPlan> {
        self.require_evaluation("planning a V01 study")?;
        ensure!(draft.schema_version == STUDY_SCHEMA_VERSION, "unsupported study schema");
        draft.validate_shape()?;
        TransactionId::parse(&draft.gate_stage)?;
        let origin = self.resolve_study_origin(&draft.origin, &draft.gate_stage, &draft.skill_id)?;
        let (optimizer_roots, mut training_source_roots) =
            self.resolve_study_optimizers(&draft.origin, &draft.additional_optimizer_roots, config)?;
        self.resolve_study_calibration(&mut draft, &origin, config)?;
        let runtime = self.experiment_pins(&draft.skill_id, config, EvalSplit::Holdout, false)?;
        let suite = self.store.read_suite(&runtime.suite_digest)?;
        training_source_roots.extend(suite.lesson_root_digests());
        training_source_roots.sort();
        training_source_roots.dedup();
        let pairs = crate::skills::lifecycle::study::compile_pairs(&draft.roots, draft.repetitions, draft.order_seed)?;
        let manifest = StudyManifest {
            schema_version: STUDY_SCHEMA_VERSION,
            id: draft.id,
            owner_scope: self.owner_scope().clone(),
            skill_id: draft.skill_id,
            task_profile: crate::skills::lifecycle::study::STUDY_TASK_PROFILE.into(),
            runtime,
            origin,
            optimizer_roots,
            training_source_roots,
            protocol: draft.protocol,
            roots: draft.roots,
            repetitions: draft.repetitions,
            order_seed: draft.order_seed,
            pairs,
            required_tools: draft.required_tools,
            case_tools: draft.case_tools,
            side_effects: draft.side_effects,
            resources: draft.resources,
            per_trial: draft.per_trial,
            elapsed_ms: draft.elapsed_ms,
        };
        manifest.validate()?;
        let sha256 = manifest.sha256()?;
        let created_at = Utc::now();
        let duration = chrono::Duration::try_milliseconds(i64::try_from(manifest.elapsed_ms)?)
            .context("study duration outside supported range")?;
        let expires_at = created_at
            .checked_add_signed(duration)
            .context("study deadline overflow")?;
        let mut stages = Vec::new();
        let mut operations = BTreeMap::new();
        let mut previous: Option<String> = None;
        for pair in &manifest.pairs {
            for arm in [pair.first_arm, pair.first_arm.other()] {
                let id = pair.stage_id(arm);
                let operation = ExperimentOperation::StudyTrial {
                    input: StudyTrialInput {
                        study_sha256: sha256.clone(),
                        pair_id: pair.pair_id.clone(),
                        arm,
                    },
                };
                stages.push(ExperimentStage {
                    id: id.clone(),
                    kind: ExperimentStageKind::StudyTrial,
                    dependencies: previous.iter().cloned().collect(),
                    input_sha256: operation.digest()?,
                    allocation: manifest.per_trial,
                });
                operations.insert(id.clone(), operation);
                previous = Some(id);
            }
        }
        let experiment = InstructionExperimentPlan {
            spec: ExperimentSpec {
                schema_version: crate::skills::lifecycle::experiment::EXPERIMENT_SCHEMA_VERSION,
                id: manifest.id.clone(),
                owner_scope: manifest.owner_scope.clone(),
                pins: manifest.runtime.clone(),
                resources: manifest.resources,
                max_concurrent: 1,
                elapsed_ms: manifest.elapsed_ms,
                created_at,
                expires_at,
                stages,
                controlled_admission: None,
            },
            operations,
        };
        let plan = StudyPlan { manifest, experiment };
        plan.validate()?;
        self.revalidate_study_manifest(&plan, config)?;
        self.store.study_plan_record_size(&plan)?;
        Ok(plan)
    }

    /// Re-resolve every owner fact before sealing. The immutable plan is also
    /// the retained attempt claim if admission later becomes uncertain.
    pub(crate) fn admit_v01_study(&self, plan: StudyPlan, config: &Config) -> Result<ExperimentJournal> {
        self.require_evaluation("admitting a V01 study")?;
        plan.validate()?;
        self.revalidate_study_manifest(&plan, config)?;
        self.store.write_study_plan(&plan)?;
        self.admit_experiment(plan.experiment.spec, config)
    }

    /// Called by the original spec-only admission and per-stage owner before
    /// reservations. A caller-supplied kind or digest is never a study proof.
    pub(super) fn validate_study_spec(&self, spec: &ExperimentSpec, config: &Config) -> Result<()> {
        if !spec
            .stages
            .iter()
            .any(|stage| stage.kind == ExperimentStageKind::StudyTrial)
        {
            return Ok(());
        }
        ensure!(
            spec.stages
                .iter()
                .all(|stage| stage.kind == ExperimentStageKind::StudyTrial),
            "study cannot mix ordinary/qualification stages"
        );
        let owned = self.store.read_study_plan(&spec.id, None)?;
        ensure!(
            owned.experiment.spec.clone().freeze()?.digest() == spec.clone().freeze()?.digest(),
            "study spec differs from its owner-sealed plan"
        );
        self.revalidate_study_manifest(&owned, config)
    }

    /// Every generic full-plan runner checks the ENTIRE input map before its
    /// first effect, including a StudyTrial disguised under another kind.
    pub(super) fn validate_study_instruction_plan(
        &self,
        plan: &InstructionExperimentPlan,
        config: &Config,
    ) -> Result<()> {
        let contains = plan
            .operations
            .values()
            .any(|operation| matches!(operation, ExperimentOperation::StudyTrial { .. }))
            || plan
                .spec
                .stages
                .iter()
                .any(|stage| stage.kind == ExperimentStageKind::StudyTrial);
        if !contains {
            return Ok(());
        }
        plan.validate()?;
        ensure!(
            plan.operations
                .values()
                .all(|operation| matches!(operation, ExperimentOperation::StudyTrial { .. })),
            "study operation map mixes ordinary/qualification work"
        );
        self.validate_study_spec(&plan.spec, config)?;
        let owned = self.store.read_study_plan(&plan.spec.id, None)?;
        ensure!(
            owned.experiment.operations == plan.operations,
            "study operation map changed"
        );
        Ok(())
    }

    pub(crate) async fn run_v01_study(
        self: &Arc<Self>,
        config: &Config,
        id: ExperimentId,
        digest: String,
        study_sha256: String,
        cancel: &CancellationToken,
    ) -> Result<StudyStatus> {
        let plan = self.store.read_study_plan(&id, Some(&study_sha256))?;
        ensure!(
            plan.experiment.spec.clone().freeze()?.digest() == digest,
            "study experiment pin changed"
        );
        self.validate_study_instruction_plan(&plan.experiment, config)?;
        let mut journal = self.inspect_experiment(&id, &digest)?;
        for stage in &plan.experiment.spec.stages {
            if journal.state.cancelled || cancel.is_cancelled() {
                if !journal.state.cancelled {
                    self.cancel_experiment(&id, &digest)?;
                }
                break;
            }
            match journal
                .state
                .stages
                .get(&stage.id)
                .context("study stage state absent")?
            {
                StageState::Settled {
                    cleanup_confirmed: true,
                    ..
                } => continue,
                StageState::Planned => {}
                _ => {
                    // No replay of reservations/Unknown work. Close new
                    // admission durably and retain original liability.
                    self.cancel_experiment(&id, &digest)?;
                    break;
                }
            }
            let operation = plan
                .experiment
                .operations
                .get(&stage.id)
                .context("study operation absent")?
                .clone();
            match self
                .run_experiment_stage(config, id.clone(), digest.clone(), stage.id.clone(), operation, cancel)
                .await
            {
                Ok(current) => journal = current,
                Err(error) => {
                    // Preserve the original durable Unknown/failed record;
                    // this cancellation is only the stop rule, not repair.
                    if let Err(cancel_error) = self.cancel_experiment(&id, &digest) {
                        return Err(error.context(format!("study stop record was not confirmed: {cancel_error}")));
                    }
                    break;
                }
            }
        }
        self.inspect_v01_study(config, &id, &digest, &study_sha256)
    }

    fn resolve_study_calibration(
        &self,
        draft: &mut StudyDraft,
        origin: &crate::skills::lifecycle::study::StudyOriginPin,
        config: &Config,
    ) -> Result<()> {
        if draft.protocol.phase == StudyPhase::Calibration {
            ensure!(
                draft.protocol.calibration.is_none(),
                "calibration cannot reference itself"
            );
            return Ok(());
        }
        let calibration = draft
            .protocol
            .calibration
            .as_mut()
            .context("confirmatory study requires a calibration reference")?;
        let report = self.report_v01_study(
            config,
            &calibration.study.id,
            &calibration.study.sha256,
            &self
                .store
                .read_study_plan(&calibration.study.id, None)?
                .manifest
                .sha256()?,
        )?;
        ensure!(
            report.manifest.protocol.phase == StudyPhase::Calibration
                && report.analysis.mechanism_complete
                && report.sha256()? == calibration.report_sha256
                && report.manifest.origin.baseline == origin.baseline
                && report.manifest.origin.h2 == origin.h2,
            "calibration is incomplete, changed, or from different frozen revisions"
        );
        calibration.excluded_roots = report
            .manifest
            .roots
            .iter()
            .map(|root| StudyExcludedRoot {
                root_task_id: root.root_task_id.clone(),
                prompt_sha256: root
                    .cases
                    .iter()
                    .map(|case| crate::skills::lifecycle::study::prompt_digest(&case.prompt))
                    .collect(),
            })
            .collect();
        Ok(())
    }

    fn revalidate_study_manifest(&self, plan: &StudyPlan, config: &Config) -> Result<()> {
        plan.validate()?;
        let manifest = &plan.manifest;
        ensure!(manifest.owner_scope == *self.owner_scope(), "study owner changed");
        ensure!(
            manifest.runtime == self.experiment_pins(&manifest.skill_id, config, EvalSplit::Holdout, false)?,
            "study runtime/model/policy pins changed"
        );
        let origin = self.resolve_study_origin(
            &manifest.origin.experiment,
            &manifest.origin.gate_stage,
            &manifest.skill_id,
        )?;
        ensure!(origin == manifest.origin, "study publication/origin facts changed");
        let extras: Vec<_> = manifest
            .optimizer_roots
            .iter()
            .filter(|pin| pin.experiment.id != manifest.origin.experiment.id)
            .map(|pin| pin.experiment.clone())
            .collect();
        let (optimizer, mut training) = self.resolve_study_optimizers(&manifest.origin.experiment, &extras, config)?;
        training.extend(
            self.store
                .read_suite(&manifest.runtime.suite_digest)?
                .lesson_root_digests(),
        );
        training.sort();
        training.dedup();
        ensure!(
            optimizer == manifest.optimizer_roots && training == manifest.training_source_roots,
            "optimizer cost/pool/source evidence changed"
        );
        if let Some(calibration) = &manifest.protocol.calibration {
            let prior = self.store.read_study_plan(&calibration.study.id, None)?;
            let report = self.report_v01_study(
                config,
                &calibration.study.id,
                &calibration.study.sha256,
                &prior.manifest.sha256()?,
            )?;
            let actual: Vec<_> = report
                .manifest
                .roots
                .iter()
                .map(|root| StudyExcludedRoot {
                    root_task_id: root.root_task_id.clone(),
                    prompt_sha256: root
                        .cases
                        .iter()
                        .map(|case| crate::skills::lifecycle::study::prompt_digest(&case.prompt))
                        .collect(),
                })
                .collect();
            ensure!(
                report.manifest.protocol.phase == StudyPhase::Calibration
                    && report.analysis.mechanism_complete
                    && report.sha256()? == calibration.report_sha256
                    && actual == calibration.excluded_roots
                    && report.manifest.origin.baseline == manifest.origin.baseline
                    && report.manifest.origin.h2 == manifest.origin.h2
                    && report.manifest.runtime == manifest.runtime,
                "confirmatory calibration proof changed"
            );
        }
        Ok(())
    }
}
