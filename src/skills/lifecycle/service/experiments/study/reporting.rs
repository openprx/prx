//! Safe open-protocol status and closed-protocol owner reconstruction.
use super::{
    Config, Context, ExperimentId, ExperimentJournal, Result, SkillsLifecycleService, StageState, StudyPlan,
    StudyReport, StudyStatus, ensure,
};
use crate::skills::lifecycle::experiment::{ExperimentEvent, ExperimentResources, ExperimentStageKind};
use crate::skills::lifecycle::study::{
    STUDY_SCHEMA_VERSION, StudyAttemptState, StudyPhysicalObservation, StudyTrialInput, StudyTrialReceipt,
    StudyTrialRow, summarize,
};
use chrono::Utc;
use std::collections::{BTreeMap, BTreeSet};

fn projection(
    state: &StageState,
    allocation: ExperimentResources,
) -> (StudyAttemptState, ExperimentResources, ExperimentResources) {
    match state {
        StageState::Planned => (
            StudyAttemptState::NotStarted,
            ExperimentResources::default(),
            ExperimentResources::default(),
        ),
        StageState::Reserved => (StudyAttemptState::Reserved, ExperimentResources::default(), allocation),
        StageState::Dispatched => (
            StudyAttemptState::Dispatched,
            ExperimentResources::default(),
            allocation,
        ),
        StageState::DispatchUnknown => (StudyAttemptState::Unknown, ExperimentResources::default(), allocation),
        StageState::Settled {
            usage,
            cleanup_confirmed,
            ..
        } => {
            let liability = if *cleanup_confirmed {
                ExperimentResources::default()
            } else {
                ExperimentResources {
                    calls: allocation.calls.saturating_sub(usage.calls),
                    tokens: allocation.tokens.saturating_sub(usage.tokens),
                    micro_usd: allocation.micro_usd.saturating_sub(usage.micro_usd),
                    tool_calls: allocation.tool_calls.saturating_sub(usage.tool_calls),
                    output_bytes: allocation.output_bytes.saturating_sub(usage.output_bytes),
                    scratch_bytes: allocation.scratch_bytes.saturating_sub(usage.scratch_bytes),
                }
            };
            (
                if *cleanup_confirmed {
                    StudyAttemptState::Settled
                } else {
                    StudyAttemptState::SettledCleanupUnknown
                },
                *usage,
                liability,
            )
        }
    }
}

fn proven_unsent(journal: &ExperimentJournal, stage: &str, physical: &StudyPhysicalObservation) -> bool {
    // Zero ledger rows alone are never proof. The original journal must show
    // a reserved, never-dispatched stage cleaned by its existing owner, with
    // the exact original input digest and zero settled usage.
    let Some(declared) = journal.plan.spec().stages.iter().find(|value| value.id == stage) else {
        return false;
    };
    matches!(journal.state.stages.get(stage), Some(StageState::Settled { usage, evidence_sha256, successful: false, cleanup_confirmed: true })
        if *usage == ExperimentResources::default() && evidence_sha256 == &declared.input_sha256)
        && journal
            .entries
            .iter()
            .any(|entry| matches!(&entry.event, ExperimentEvent::Reserved { stage: name } if name == stage))
        && journal
            .entries
            .iter()
            .any(|entry| matches!(&entry.event, ExperimentEvent::CleanupConfirmed { stage: name } if name == stage))
        && !journal
            .entries
            .iter()
            .any(|entry| matches!(&entry.event, ExperimentEvent::Dispatched { stage: name } if name == stage))
        && physical.calls == 0
        && physical.attempt_ids.is_empty()
        && !physical.unresolved
}

impl SkillsLifecycleService {
    fn study_read_bound(
        &self,
        id: &ExperimentId,
        experiment: &str,
        study: &str,
    ) -> Result<(StudyPlan, ExperimentJournal)> {
        self.require_enabled()?;
        let plan = self.store.read_study_plan(id, Some(study))?;
        let journal = self.inspect_experiment(id, experiment)?;
        ensure!(
            plan.experiment.spec.clone().freeze()?.digest() == journal.plan.digest()
                && journal
                    .plan
                    .spec()
                    .stages
                    .iter()
                    .all(|stage| stage.kind == ExperimentStageKind::StudyTrial),
            "study read does not match its original owner-sealed plan"
        );
        Ok((plan, journal))
    }

    pub(crate) fn inspect_v01_study(
        &self,
        config: &Config,
        id: &ExperimentId,
        experiment: &str,
        study: &str,
    ) -> Result<StudyStatus> {
        let (plan, journal) = self.study_read_bound(id, experiment, study)?;
        let metadata_bytes = u64::try_from(self.store.study_plan_record_size(&plan)?)?;
        let metadata = ExperimentResources {
            output_bytes: metadata_bytes,
            scratch_bytes: metadata_bytes,
            ..Default::default()
        };
        ensure!(
            journal
                .state
                .committed(&journal.plan)?
                .checked_add(metadata)?
                .within(journal.plan.spec().resources),
            "study metadata plus original committed resources exceed its frozen root"
        );
        let mut reserved = 0_u32;
        let mut settled = 0_u32;
        let mut unknown = 0_u32;
        let mut actual = ExperimentResources::default();
        let mut liability = ExperimentResources::default();
        for declared in &journal.plan.spec().stages {
            let state = journal
                .state
                .stages
                .get(&declared.id)
                .context("study stage state absent")?;
            let (mut phase, usage, retained) = projection(state, declared.allocation);
            actual = actual.checked_add(usage)?;
            liability = liability.checked_add(retained)?;
            reserved += u32::from(
                journal
                    .entries
                    .iter()
                    .any(|entry| matches!(&entry.event, ExperimentEvent::Reserved { stage } if stage == &declared.id)),
            );
            let physical = self.study_physical(config, journal.plan.digest(), &declared.id)?;
            if proven_unsent(&journal, &declared.id, &physical) {
                phase = StudyAttemptState::ProvenUnsent;
            }
            settled += u32::from(matches!(
                phase,
                StudyAttemptState::Settled | StudyAttemptState::ProvenUnsent
            ));
            unknown += u32::from(
                physical.unresolved
                    || matches!(
                        phase,
                        StudyAttemptState::Reserved
                            | StudyAttemptState::Dispatched
                            | StudyAttemptState::Unknown
                            | StudyAttemptState::SettledCleanupUnknown
                    ),
            );
        }
        let all_terminal = usize::try_from(settled)? == journal.plan.spec().stages.len();
        Ok(StudyStatus {
            study_sha256: study.into(),
            experiment_sha256: experiment.into(),
            journal_head_sha256: journal.head_sha256,
            cancelled: journal.state.cancelled,
            protocol_closed: journal.state.cancelled || all_terminal,
            declared_stages: u32::try_from(journal.plan.spec().stages.len())?,
            reserved_stages: reserved,
            settled_and_clean_stages: settled,
            unknown_stages: unknown,
            settled_usage: actual,
            retained_liability: liability,
            admission_metadata_usage: metadata,
            qualification: false,
        })
    }

    pub(crate) fn report_v01_study(
        &self,
        config: &Config,
        id: &ExperimentId,
        experiment: &str,
        study: &str,
    ) -> Result<StudyReport> {
        let status = self.inspect_v01_study(config, id, experiment, study)?;
        ensure!(
            status.protocol_closed,
            "study protocol is open; use status without oracle feedback"
        );
        let (plan, journal) = self.study_read_bound(id, experiment, study)?;
        let manifest = &plan.manifest;
        // Historical source facts are authenticated independently of today's
        // active generation/config. Changing a current baseline does not turn
        // an old report into a new execution permission.
        ensure!(
            self.resolve_study_origin(
                &manifest.origin.experiment,
                &manifest.origin.gate_stage,
                &manifest.skill_id
            )? == manifest.origin,
            "study original publication evidence changed"
        );
        let extra: Vec<_> = manifest
            .optimizer_roots
            .iter()
            .filter(|pin| pin.experiment.id != manifest.origin.experiment.id)
            .map(|pin| pin.experiment.clone())
            .collect();
        let (optimizer, mut training) = self.resolve_study_optimizers(&manifest.origin.experiment, &extra, config)?;
        training.extend(
            self.store
                .read_suite(&manifest.runtime.suite_digest)?
                .lesson_root_digests(),
        );
        training.sort();
        training.dedup();
        ensure!(
            optimizer == manifest.optimizer_roots && training == manifest.training_source_roots,
            "study optimizer/source records changed"
        );
        let mut rows = Vec::new();
        let mut all_attempts = BTreeSet::new();
        for pin in &manifest.optimizer_roots {
            for attempt in &pin.physical_attempt_ids {
                ensure!(
                    all_attempts.insert(attempt.clone()),
                    "optimizer physical call counted twice"
                );
            }
        }
        let mut physical_at_read = BTreeMap::new();
        for pair in &manifest.pairs {
            for arm in [pair.first_arm, pair.first_arm.other()] {
                let stage = pair.stage_id(arm);
                let declared = journal
                    .plan
                    .spec()
                    .stages
                    .iter()
                    .find(|declared| declared.id == stage)
                    .context("frozen study stage absent")?;
                let state = journal.state.stages.get(&stage).context("study stage state absent")?;
                let (mut phase, settled_usage, retained_liability) = projection(state, declared.allocation);
                let input = StudyTrialInput {
                    study_sha256: study.into(),
                    pair_id: pair.pair_id.clone(),
                    arm,
                };
                let physical = self.study_physical(config, journal.plan.digest(), &stage)?;
                if proven_unsent(&journal, &stage, &physical) {
                    phase = StudyAttemptState::ProvenUnsent;
                }
                for attempt in &physical.attempt_ids {
                    ensure!(
                        all_attempts.insert(attempt.clone()),
                        "physical attempt appears in multiple study stages"
                    );
                }
                let terminal_reference = journal.entries.iter().find_map(|entry| match &entry.event {
                    ExperimentEvent::Settled {
                        stage: name,
                        reference,
                        evidence_sha256,
                        ..
                    } if name == &stage => Some((reference, evidence_sha256)),
                    _ => None,
                });
                let receipt = if let Some((reference, sha256)) = terminal_reference {
                    ensure!(
                        reference.as_deref() == Some(format!("study:{stage}").as_str()),
                        "study journal has a nonstudy evidence reference"
                    );
                    Some(self.store.read_study_trial(id, &stage, sha256)?)
                } else {
                    self.store.optional_study_trial(id, &stage)?
                };
                ensure!(
                    phase != StudyAttemptState::ProvenUnsent || receipt.is_none(),
                    "never-dispatched stage unexpectedly has execution evidence"
                );
                if let Some(receipt) = &receipt {
                    self.verify_study_receipt(
                        &plan,
                        &journal,
                        receipt,
                        &input,
                        &physical,
                        phase == StudyAttemptState::Settled,
                    )?;
                }
                if phase == StudyAttemptState::Settled {
                    ensure!(
                        receipt.is_some()
                            && !physical.unresolved
                            && physical.calls == physical.settled_calls
                            && physical.calls == settled_usage.calls
                            && physical.tokens == settled_usage.tokens
                            && physical.micro_usd == settled_usage.micro_usd,
                        "study settlement/cleanup differs from original physical owner"
                    );
                }
                physical_at_read.insert(stage.clone(), physical.clone());
                rows.push(StudyTrialRow {
                    stage: stage.clone(),
                    input,
                    root_task_id: pair.root_task_id.clone(),
                    case_id: pair.case_id.clone(),
                    state: phase,
                    ever_reserved: journal.entries.iter().any(
                        |entry| matches!(&entry.event, ExperimentEvent::Reserved { stage: name } if name == &stage),
                    ),
                    settled_usage,
                    retained_liability,
                    physical,
                    receipt,
                });
            }
        }
        let (baseline, h2, analysis) = summarize(manifest, &rows, journal.state.cancelled)?;
        ensure!(
            !self.experiment_qualified(&journal)?,
            "study unexpectedly acquired qualification authority"
        );
        let mut optimizer_actual = ExperimentResources::default();
        let mut optimizer_liability = ExperimentResources::default();
        for pin in &manifest.optimizer_roots {
            optimizer_actual = optimizer_actual.checked_add(pin.actual)?;
            optimizer_liability = optimizer_liability.checked_add(pin.reserved_liability)?;
        }
        let (calibration_settled_usage, calibration_retained_liability, calibration_physical_attempt_ids) =
            if let Some(calibration) = &manifest.protocol.calibration {
                let prior = self.store.read_study_plan(&calibration.study.id, None)?;
                let report = self.report_v01_study(
                    config,
                    &calibration.study.id,
                    &calibration.study.sha256,
                    &prior.manifest.sha256()?,
                )?;
                ensure!(
                    report.manifest.protocol.phase == crate::skills::lifecycle::study::StudyPhase::Calibration
                        && report.analysis.mechanism_complete
                        && report.sha256()? == calibration.report_sha256
                        && report.manifest.runtime == manifest.runtime
                        && report.manifest.origin.baseline == manifest.origin.baseline
                        && report.manifest.origin.h2 == manifest.origin.h2,
                    "study calibration historical proof is unavailable or changed"
                );
                let actual_roots: Vec<_> = report
                    .manifest
                    .roots
                    .iter()
                    .map(|root| crate::skills::lifecycle::study::StudyExcludedRoot {
                        root_task_id: root.root_task_id.clone(),
                        prompt_sha256: root
                            .cases
                            .iter()
                            .map(|case| crate::skills::lifecycle::study::prompt_digest(&case.prompt))
                            .collect(),
                    })
                    .collect();
                ensure!(
                    actual_roots == calibration.excluded_roots,
                    "study calibration source exclusions changed"
                );
                let mut calls = BTreeSet::new();
                for row in &report.rows {
                    for attempt in &row.physical.attempt_ids {
                        ensure!(
                            calls.insert(attempt.clone()) && all_attempts.insert(attempt.clone()),
                            "calibration cost duplicates optimizer or holdout work"
                        );
                    }
                }
                (
                    Some(
                        report
                            .baseline
                            .settled_usage
                            .checked_add(report.h2.settled_usage)?
                            .checked_add(report.admission_metadata_usage)?,
                    ),
                    Some(
                        report
                            .baseline
                            .retained_liability
                            .checked_add(report.h2.retained_liability)?,
                    ),
                    calls.into_iter().collect(),
                )
            } else {
                (None, None, Vec::new())
            };
        let again = self.inspect_experiment(id, experiment)?;
        ensure!(
            again.head_sha256 == journal.head_sha256,
            "study changed during report; retry read only"
        );
        for (stage, before) in physical_at_read {
            ensure!(
                self.study_physical(config, experiment, &stage)? == before,
                "physical accounting changed during report; retry read only"
            );
        }
        Ok(StudyReport {
            schema_version: STUDY_SCHEMA_VERSION, study_sha256: study.into(), experiment_sha256: experiment.into(),
            journal_head_sha256: journal.head_sha256, manifest: manifest.clone(), cancelled: journal.state.cancelled,
            admission_metadata_usage: status.admission_metadata_usage,
            rows, baseline, h2, analysis, optimizer_settled_usage: optimizer_actual,
            optimizer_retained_liability: optimizer_liability,
            optimizer_scope: "unique original optimizer roots plus authenticated frozen-pool ancestry; separate from adopted steady-state study and outer interactive overhead".into(),
            calibration_settled_usage, calibration_retained_liability, calibration_physical_attempt_ids,
            preparation_overhead_measurement: "metadata/library/analyzer CPU and energy are not separately measured; this is unavailable, not zero compute. Model preparation calls belong to the disclosed original optimizer/pool roots.".into(),
            generated_at: Utc::now(),
        })
    }

    fn verify_study_receipt(
        &self,
        plan: &StudyPlan,
        journal: &ExperimentJournal,
        receipt: &StudyTrialReceipt,
        input: &StudyTrialInput,
        physical: &StudyPhysicalObservation,
        eligible: bool,
    ) -> Result<()> {
        let manifest = &plan.manifest;
        let pair = manifest
            .pairs
            .iter()
            .find(|pair| pair.pair_id == input.pair_id)
            .context("study receipt pair absent")?;
        let stage = pair.stage_id(input.arm);
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|declared| declared.id == stage)
            .context("study receipt stage absent")?;
        ensure!(
            receipt.schema_version == STUDY_SCHEMA_VERSION
                && receipt.owner_scope == *self.owner_scope()
                && receipt.input == *input
                && receipt.binding.experiment_sha256 == journal.plan.digest()
                && receipt.binding.stage == stage
                && receipt.binding.input_sha256 == declared.input_sha256
                && receipt.physical.scope_id == physical.scope_id
                && receipt.root_task_id == pair.root_task_id
                && receipt.case_id == pair.case_id
                && receipt.repetition == pair.repetition
                && receipt.recorded_at >= journal.plan.spec().created_at
                && receipt.recorded_at <= journal.plan.spec().expires_at,
            "study receipt is from another original owner/input/stage"
        );
        let mut artifacts = BTreeMap::new();
        for (path, fact) in &receipt.run.artifacts {
            let bytes = fact
                .as_ref()
                .map(|fact| {
                    self.store
                        .read_study_artifact(&manifest.id, &stage, &fact.sha256, fact.bytes)
                })
                .transpose()?;
            artifacts.insert(path.clone(), bytes);
        }
        let revision = manifest.revision(input.arm);
        let bundle = self
            .store
            .read_revision(&manifest.skill_id, &revision.revision, Some(&revision.record_sha256))?
            .bundle;
        // Recompute the actual-effect oracle using retained, hashed owner bytes,
        // not only a self-labelled score or a saved validator bool.
        let captured_grade = receipt.compute_grade(manifest, &bundle, &artifacts, &receipt.physical)?;
        ensure!(
            captured_grade.validators == receipt.validators
                && captured_grade.measured_and_valid == receipt.measured_and_valid
                && captured_grade.invalid_reasons == receipt.invalid_reasons
                && captured_grade.success == receipt.success
                && captured_grade.quality_ppm == receipt.quality_ppm,
            "study grade changed from its actual effect evidence"
        );
        if eligible {
            ensure!(
                receipt.physical == *physical,
                "study receipt usage/call IDs differ from actual final owner records"
            );
            let final_grade = receipt.compute_grade(manifest, &bundle, &artifacts, physical)?;
            ensure!(
                final_grade.measured_and_valid == receipt.measured_and_valid
                    && final_grade.success == receipt.success
                    && final_grade.quality_ppm == receipt.quality_ppm,
                "study final physical settlement changed its grade"
            );
        }
        Ok(())
    }

    pub(crate) fn export_v01_study(
        &self,
        config: &Config,
        id: &ExperimentId,
        experiment: &str,
        study: &str,
    ) -> Result<Vec<u8>> {
        let report = self.report_v01_study(config, id, experiment, study)?;
        self.store.study_export_bytes(&report)
    }
}
