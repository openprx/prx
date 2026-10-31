//! Pure grading of real effect bytes. Used at owner capture and independently
//! again during authenticated report reconstruction; no model self-grade.
use super::{StudyManifest, StudyPhysicalObservation, StudyTrialReceipt};
use crate::skills::lifecycle::{Bundle, CleanupState, RunTerminal, ValidatorOutcome};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(in crate::skills::lifecycle) struct StudyGrade {
    pub validators: Vec<ValidatorOutcome>,
    pub measured_and_valid: bool,
    pub invalid_reasons: Vec<String>,
    pub success: bool,
    pub quality_ppm: u32,
}

impl StudyTrialReceipt {
    pub(in crate::skills::lifecycle) fn compute_grade(
        &self,
        manifest: &StudyManifest,
        bundle: &Bundle,
        artifacts: &BTreeMap<String, Option<Vec<u8>>>,
        physical: &StudyPhysicalObservation,
    ) -> Result<StudyGrade> {
        let pair = manifest
            .pairs
            .iter()
            .find(|pair| pair.pair_id == self.input.pair_id)
            .context("study grade pair absent")?;
        let case = manifest.case(pair)?;
        let revision = manifest.revision(self.input.arm);
        ensure!(
            bundle.revision_id() == &revision.revision
                && self.root_task_id == pair.root_task_id
                && self.case_id == pair.case_id
                && self.repetition == pair.repetition,
            "study grade source mismatch"
        );
        let expected_paths: BTreeSet<_> = case
            .validators
            .iter()
            .filter_map(crate::skills::lifecycle::Validator::artifact_path)
            .collect();
        ensure!(
            artifacts.len() == expected_paths.len()
                && artifacts.keys().all(|path| expected_paths.contains(path.as_str()))
                && self.run.artifacts.len() == artifacts.len(),
            "study actual artifact set changed"
        );
        for (path, content) in artifacts {
            let recorded = self.run.artifacts.get(path).context("study artifact receipt absent")?;
            match (content, recorded) {
                (None, None) => {}
                (Some(bytes), Some(fact)) => ensure!(
                    fact.sha256 == hex::encode(Sha256::digest(bytes)) && fact.bytes == u64::try_from(bytes.len())?,
                    "study effect bytes differ from actual capture"
                ),
                _ => anyhow::bail!("study artifact presence changed"),
            }
        }
        let run = &self.run;
        let mut invalid = Vec::new();
        let expected_run = format!("v01-{}-{}", manifest.id.as_str(), pair.stage_id(self.input.arm));
        if run.run_id != expected_run {
            invalid.push("wrong_run_identity".into());
        }
        if run.cleanup != CleanupState::Removed {
            invalid.push("cleanup_unconfirmed".into());
        }
        if physical.unresolved
            || physical.calls == 0
            || physical.calls != physical.settled_calls
            || run.tokens != Some(physical.tokens)
            || run.cost_micros != Some(physical.micro_usd)
        {
            invalid.push("physical_usage_unresolved_or_mismatched".into());
        }
        if !super::valid_digest(&run.environment_hash)
            || run.models_used.is_empty()
            || run
                .models_used
                .iter()
                .any(|model| model != &manifest.runtime.model.model)
            || physical
                .models
                .iter()
                .any(|model| model != &manifest.runtime.model.model)
            || run.models_used != physical.models
        {
            invalid.push("model_or_environment_mismatch".into());
        }
        if run.terminal == RunTerminal::Completed && run.elapsed_ms > case.max_elapsed_ms {
            invalid.push("case_deadline_exceeded".into());
        }
        let installed = run.installed.as_ref();
        if !installed.is_some_and(|installed| {
            installed.skill_id == manifest.skill_id
                && installed.revision_id == revision.revision
                && installed.bundle_manifest_hash == revision.revision.digest_hex()
        }) {
            invalid.push("actual_installed_revision_mismatch".into());
        }
        let mut valid_reads = 0_u32;
        for read in &run.hydrations {
            let file = bundle.files().iter().find(|file| file.path() == read.resource_path);
            if read.skill_id != manifest.skill_id
                || read.revision_id != revision.revision
                || installed.is_none_or(|installed| read.generation != installed.generation)
                || file.is_none_or(|file| hex::encode(Sha256::digest(file.bytes())) != read.resource_sha256)
            {
                invalid.push("actual_hydration_mismatch".into());
            } else {
                valid_reads = valid_reads.saturating_add(1);
            }
        }
        let trigger_correct = if case.expect_skill_read {
            valid_reads > 0
        } else {
            run.hydrations.is_empty()
        };
        // StudyManifest::validate admits only positive artifact validators.
        // An empty response here cannot affect grading or become proof.
        let validators = crate::skills::lifecycle::evaluate::run_validators(case, "", artifacts);
        ensure!(!validators.is_empty(), "study has no actual-effect oracle");
        let measured_and_valid = invalid.is_empty();
        let completed = run.terminal == RunTerminal::Completed;
        let success =
            measured_and_valid && completed && trigger_correct && validators.iter().all(|outcome| outcome.passed);
        let passed = u64::try_from(validators.iter().filter(|outcome| outcome.passed).count())?;
        let total = u64::try_from(validators.len())?;
        let quality_ppm = if measured_and_valid && completed && trigger_correct {
            u32::try_from(passed.checked_mul(1_000_000).context("artifact quality overflow")? / total)?
        } else {
            0
        };
        Ok(StudyGrade {
            validators,
            measured_and_valid,
            invalid_reasons: invalid,
            success,
            quality_ppm,
        })
    }
}
