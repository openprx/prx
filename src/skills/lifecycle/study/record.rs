//! Private authenticated study records under the existing lifecycle store.
//! No constructor here can admit work, refresh resources or create a grant.
use super::{StudyPlan, StudyReport, StudyTrialReceipt, digest, require_digest};
use crate::decision::{DecisionKey, KeyDomain};
use crate::skills::lifecycle::experiment::{ExperimentId, ExperimentJournal};
use crate::skills::lifecycle::generation::ensure_dir_synced;
use crate::skills::lifecycle::store::{MAX_RECORD_BYTES, check_private, read_record, record_bytes};
use crate::skills::lifecycle::{SkillId, SkillLifecycleStore, TransactionId};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PLAN_DOMAIN: &[u8] = b"prx-v01-owned-study-plan-v1";
const TRIAL_DOMAIN: &[u8] = b"prx-v01-owned-study-trial-v1";
const REPORT_DOMAIN: &[u8] = b"prx-v01-owned-study-report-v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sealed<T> {
    value: T,
    sha256: String,
    mac: String,
}

#[derive(Serialize)]
struct ExportValue<'a> {
    report: &'a StudyReport,
    evidence_sha256: String,
}

impl SkillLifecycleStore {
    fn require_study_runtime(
        &self,
        journal: &ExperimentJournal,
        binding: &crate::skills::lifecycle::experiment::ExperimentRunBinding,
    ) -> Result<std::sync::Arc<crate::runtime::experiment_stage::ExperimentStageRuntime>> {
        let runtime =
            crate::runtime::experiment_stage::current().context("study evidence has no actual stage owner")?;
        ensure!(
            runtime.kind() == crate::skills::lifecycle::experiment::ExperimentStageKind::StudyTrial
                && runtime.binding() == *binding
                && binding.experiment_sha256 == journal.plan.digest()
                && journal.plan.spec().owner_scope == *self.owner_scope(),
            "study evidence has no matching actual admitted owner"
        );
        let current = self.read_experiment(&journal.plan.spec().id, journal.plan.digest())?;
        ensure!(
            matches!(
                current.state.stages.get(&binding.stage),
                Some(crate::skills::lifecycle::experiment::StageState::Dispatched)
            ),
            "study evidence is not from a dispatched owned stage"
        );
        Ok(runtime)
    }
    fn study_key(&self) -> Result<std::sync::Arc<DecisionKey>> {
        let workspace = self
            .root()
            .parent()
            .and_then(Path::parent)
            .context("study store has no workspace")?;
        Ok(DecisionKey::load_existing(workspace)?)
    }
    fn study_directory(&self, id: &ExperimentId) -> PathBuf {
        self.root().join("studies").join(id.as_str())
    }
    fn study_path(&self, id: &ExperimentId, name: &str) -> PathBuf {
        self.study_directory(id).join(name)
    }
    fn study_seal<T: Serialize>(&self, value: &T, domain: &[u8], path: &Path) -> Result<Vec<u8>> {
        let payload = serde_json::to_vec(value)?;
        let sha256 = digest(domain, value)?;
        let mac = hex::encode(self.study_key()?.hmac(
            KeyDomain::ReportOwnership,
            &[domain, self.owner_scope().as_str().as_bytes(), &payload],
        ));
        let bytes = record_bytes(&Sealed { value, sha256, mac }, path)?;
        ensure!(
            u64::try_from(bytes.len())? <= MAX_RECORD_BYTES,
            "study record exceeds owner record bound"
        );
        Ok(bytes)
    }
    fn read_study_sealed<T: DeserializeOwned + Serialize>(
        &self,
        id: &ExperimentId,
        name: &str,
        domain: &[u8],
    ) -> Result<T> {
        for path in [
            self.root().join("studies"),
            self.study_directory(id),
            self.study_path(id, name),
        ] {
            check_private(&path)?;
        }
        let sealed: Sealed<T> = read_record(&self.study_path(id, name))?;
        let payload = serde_json::to_vec(&sealed.value)?;
        let tag = hex::decode(&sealed.mac).context("study MAC encoding changed")?;
        ensure!(
            sealed.sha256 == digest(domain, &sealed.value)?
                && self.study_key()?.verify_hmac(
                    KeyDomain::ReportOwnership,
                    &[domain, self.owner_scope().as_str().as_bytes(), &payload],
                    &tag
                ),
            "study evidence owner seal changed"
        );
        let path = self.study_path(id, name);
        ensure!(
            std::fs::metadata(&path)?.len() == u64::try_from(self.study_seal(&sealed.value, domain, &path)?.len())?,
            "study encoded evidence size changed"
        );
        Ok(sealed.value)
    }
    fn write_study_once<T: Serialize + DeserializeOwned>(
        &self,
        id: &ExperimentId,
        skill: &SkillId,
        name: &str,
        value: &T,
        domain: &[u8],
    ) -> Result<()> {
        let _store = self.lock_store(true, Duration::from_secs(10))?;
        let _skill = self.lock_skill(skill, Duration::from_secs(10))?;
        let root = self.root().join("studies");
        ensure_dir_synced(self.root(), &root)?;
        let directory = self.study_directory(id);
        ensure_dir_synced(&root, &directory)?;
        let path = self.study_path(id, name);
        let bytes = self.study_seal(value, domain, &path)?;
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                let old: T = self.read_study_sealed(id, name, domain)?;
                ensure!(
                    digest(domain, &old)? == digest(domain, value)?,
                    "immutable study record conflicts"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let runtime = crate::runtime::experiment_stage::current()
                    .context("study durable output lacks its original stage admission")?;
                let write = runtime.prepare_native_write(&directory, &path, bytes.len())?;
                let result = self.write_atomic(skill, &path, &bytes);
                write.finish();
                result?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    pub(in crate::skills::lifecycle) fn write_study_plan(&self, plan: &StudyPlan) -> Result<()> {
        plan.validate()?;
        ensure!(plan.manifest.owner_scope == *self.owner_scope(), "study owner mismatch");
        let _store = self.lock_store(true, Duration::from_secs(10))?;
        let _skill = self.lock_skill(&plan.manifest.skill_id, Duration::from_secs(10))?;
        let root = self.root().join("studies");
        ensure_dir_synced(self.root(), &root)?;
        // The immutable plan directory itself is the bounded attempt claim;
        // no second grant/claim/budget registry is constructed. An orphaned
        // admission remains claimed instead of silently reopening holdout.
        let new_roots: BTreeSet<_> = plan
            .manifest
            .roots
            .iter()
            .map(|root| root.root_task_id.as_str())
            .collect();
        let new_inputs: BTreeSet<_> = plan
            .manifest
            .roots
            .iter()
            .flat_map(|root| root.cases.iter().map(|case| super::prompt_digest(&case.prompt)))
            .collect();
        let mut examined = 0_u32;
        let mut existing_exact = false;
        for entry in std::fs::read_dir(&root)? {
            examined = examined
                .checked_add(1)
                .context("study admission enumeration overflow")?;
            ensure!(
                examined <= 64,
                "study history bound exhausted; cannot establish fresh holdout"
            );
            let entry = entry?;
            check_private(&entry.path())?;
            let id = ExperimentId::parse(entry.file_name().to_str().context("invalid study history identity")?)?;
            let old = self.read_study_plan(&id, None)?;
            if id == plan.manifest.id {
                ensure!(
                    digest(PLAN_DOMAIN, &old)? == digest(PLAN_DOMAIN, plan)?,
                    "study identity is already claimed by different inputs"
                );
                existing_exact = true;
                continue;
            }
            for old_root in &old.manifest.roots {
                ensure!(
                    !new_roots.contains(old_root.root_task_id.as_str()),
                    "root task was already exposed by an earlier study"
                );
                ensure!(
                    old_root
                        .cases
                        .iter()
                        .all(|case| !new_inputs.contains(&super::prompt_digest(&case.prompt))),
                    "case input was already exposed by an earlier study"
                );
            }
        }
        if existing_exact {
            return Ok(());
        }
        ensure!(
            examined < 64,
            "study history bound exhausted; new sample admission refused"
        );
        let directory = self.study_directory(&plan.manifest.id);
        ensure_dir_synced(&root, &directory)?;
        let path = self.study_path(&plan.manifest.id, "plan.json");
        let bytes = self.study_seal(plan, PLAN_DOMAIN, &path)?;
        self.check_study_metadata_budget(plan, bytes.len())?;
        self.write_atomic(&plan.manifest.skill_id, &path, &bytes)?;
        Ok(())
    }
    fn check_study_metadata_budget(&self, plan: &StudyPlan, bytes: usize) -> Result<()> {
        let mut reserved = crate::skills::lifecycle::experiment::ExperimentResources::default();
        for stage in &plan.experiment.spec.stages {
            reserved = reserved.checked_add(stage.allocation)?;
        }
        let metadata = crate::skills::lifecycle::experiment::ExperimentResources {
            output_bytes: u64::try_from(bytes)?,
            scratch_bytes: u64::try_from(bytes)?,
            ..Default::default()
        };
        ensure!(
            reserved.checked_add(metadata)?.within(plan.manifest.resources),
            "study root lacks explicit headroom for its actual encoded admission metadata"
        );
        Ok(())
    }
    pub(in crate::skills::lifecycle) fn study_plan_record_size(&self, plan: &StudyPlan) -> Result<usize> {
        let bytes = self.study_seal(plan, PLAN_DOMAIN, &self.study_path(&plan.manifest.id, "plan.json"))?;
        self.check_study_metadata_budget(plan, bytes.len())?;
        Ok(bytes.len())
    }
    pub(in crate::skills::lifecycle) fn study_trial_record_size(
        &self,
        receipt: &StudyTrialReceipt,
        id: &ExperimentId,
    ) -> Result<usize> {
        Ok(self
            .study_seal(
                receipt,
                TRIAL_DOMAIN,
                &self.study_path(id, &format!("{}.json", receipt.binding.stage)),
            )?
            .len())
    }
    pub(in crate::skills::lifecycle) fn study_output_directory(&self, id: &ExperimentId) -> Result<PathBuf> {
        let path = self.study_directory(id);
        check_private(&path)?;
        Ok(path)
    }
    pub(in crate::skills::lifecycle) fn read_study_plan(
        &self,
        id: &ExperimentId,
        expected_sha256: Option<&str>,
    ) -> Result<StudyPlan> {
        let plan: StudyPlan = self.read_study_sealed(id, "plan.json", PLAN_DOMAIN)?;
        plan.validate()?;
        self.study_plan_record_size(&plan)?;
        ensure!(
            plan.manifest.id == *id && plan.manifest.owner_scope == *self.owner_scope(),
            "study plan owner/identity changed"
        );
        if let Some(expected) = expected_sha256 {
            require_digest(expected)?;
            ensure!(plan.manifest.sha256()? == expected, "study manifest pin changed");
        }
        Ok(plan)
    }
    pub(in crate::skills::lifecycle) fn write_study_trial(
        &self,
        journal: &ExperimentJournal,
        receipt: &StudyTrialReceipt,
        config: &crate::config::Config,
    ) -> Result<String> {
        let runtime = self.require_study_runtime(journal, &receipt.binding)?;
        ensure!(
            runtime.scope_id() == receipt.physical.scope_id,
            "study receipt has another physical scope"
        );
        let workspace = self
            .root()
            .parent()
            .and_then(Path::parent)
            .context("study workspace unavailable")?;
        ensure!(
            workspace == config.workspace_dir.canonicalize()?,
            "study receipt accounting owner mismatch"
        );
        let physical = crate::cost::tracker::CostTracker::for_workspace(config.cost.clone(), workspace)?
            .physical_scope_usage(&runtime.scope_id())?;
        ensure!(
            physical.calls == receipt.physical.calls
                && physical.settled_calls == receipt.physical.settled_calls
                && physical.tokens == receipt.physical.tokens
                && physical.micro_usd == receipt.physical.micro_usd
                && physical.unresolved == receipt.physical.unresolved
                && physical.attempt_ids.iter().eq(receipt.physical.attempt_ids.iter())
                && physical.models == receipt.physical.models,
            "study receipt self-report differs from actual physical owner"
        );
        let plan = self.read_study_plan(&journal.plan.spec().id, Some(&receipt.input.study_sha256))?;
        ensure!(
            plan.experiment.spec.clone().freeze()?.digest() == journal.plan.digest()
                && receipt.binding.experiment_sha256 == journal.plan.digest()
                && receipt.owner_scope == *self.owner_scope(),
            "study trial root mismatch"
        );
        TransactionId::parse(&receipt.binding.stage)?;
        let declared = journal
            .plan
            .spec()
            .stages
            .iter()
            .find(|stage| stage.id == receipt.binding.stage)
            .context("study receipt stage absent")?;
        let operation = crate::skills::lifecycle::experiment::ExperimentOperation::StudyTrial {
            input: receipt.input.clone(),
        };
        let pair = plan
            .manifest
            .pairs
            .iter()
            .find(|pair| pair.pair_id == receipt.input.pair_id)
            .context("study receipt pair absent")?;
        ensure!(
            receipt.schema_version == super::STUDY_SCHEMA_VERSION
                && pair.stage_id(receipt.input.arm) == receipt.binding.stage
                && receipt.root_task_id == pair.root_task_id
                && receipt.case_id == pair.case_id
                && receipt.repetition == pair.repetition
                && declared.input_sha256 == receipt.binding.input_sha256
                && operation.digest()? == declared.input_sha256,
            "study receipt input changed"
        );
        let mut actual_artifacts = BTreeMap::new();
        for (name, fact) in &receipt.run.artifacts {
            actual_artifacts.insert(
                name.clone(),
                fact.as_ref()
                    .map(|fact| {
                        self.read_study_artifact(&plan.manifest.id, &receipt.binding.stage, &fact.sha256, fact.bytes)
                    })
                    .transpose()?,
            );
        }
        let revision = plan.manifest.revision(receipt.input.arm);
        let bundle = self
            .read_revision(
                &plan.manifest.skill_id,
                &revision.revision,
                Some(&revision.record_sha256),
            )?
            .bundle;
        let grade = receipt.compute_grade(&plan.manifest, &bundle, &actual_artifacts, &receipt.physical)?;
        ensure!(
            grade.validators == receipt.validators
                && grade.measured_and_valid == receipt.measured_and_valid
                && grade.invalid_reasons == receipt.invalid_reasons
                && grade.success == receipt.success
                && grade.quality_ppm == receipt.quality_ppm,
            "study owner refuses a self-reported grade inconsistent with actual effects"
        );
        self.write_study_once(
            &plan.manifest.id,
            &plan.manifest.skill_id,
            &format!("{}.json", receipt.binding.stage),
            receipt,
            TRIAL_DOMAIN,
        )?;
        receipt.sha256()
    }
    pub(in crate::skills::lifecycle) fn write_study_artifacts(
        &self,
        journal: &ExperimentJournal,
        binding: &crate::skills::lifecycle::experiment::ExperimentRunBinding,
        artifacts: &BTreeMap<String, Option<Vec<u8>>>,
    ) -> Result<()> {
        let _runtime = self.require_study_runtime(journal, binding)?;
        let _store = self.lock_store(true, Duration::from_secs(10))?;
        let _skill = self.lock_skill(&journal.plan.spec().pins.skill_id, Duration::from_secs(10))?;
        TransactionId::parse(&binding.stage)?;
        for bytes in artifacts.values().filter_map(Option::as_ref) {
            ensure!(
                u64::try_from(bytes.len())? <= MAX_RECORD_BYTES,
                "study effect evidence exceeds artifact bound"
            );
            let sha256 = hex::encode(Sha256::digest(bytes));
            let path = self.study_path(
                &journal.plan.spec().id,
                &format!("effect-{}-{sha256}.bin", binding.stage),
            );
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    let old = self.read_study_artifact(
                        &journal.plan.spec().id,
                        &binding.stage,
                        &sha256,
                        u64::try_from(bytes.len())?,
                    )?;
                    ensure!(old == *bytes, "immutable study effect evidence conflicts");
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let runtime = crate::runtime::experiment_stage::current()
                        .context("study artifact has no owned write admission")?;
                    let write = runtime.prepare_native_write(
                        &self.study_directory(&journal.plan.spec().id),
                        &path,
                        bytes.len(),
                    )?;
                    let result = self.write_atomic(&journal.plan.spec().pins.skill_id, &path, bytes);
                    write.finish();
                    result?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
    pub(in crate::skills::lifecycle) fn read_study_artifact(
        &self,
        id: &ExperimentId,
        stage: &str,
        sha256: &str,
        expected_bytes: u64,
    ) -> Result<Vec<u8>> {
        TransactionId::parse(stage)?;
        require_digest(sha256)?;
        ensure!(
            expected_bytes <= MAX_RECORD_BYTES,
            "study effect evidence bound changed"
        );
        let path = self.study_path(id, &format!("effect-{stage}-{sha256}.bin"));
        for component in [self.root().join("studies"), self.study_directory(id), path.clone()] {
            check_private(&component)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() == expected_bytes,
            "study effect evidence shape changed"
        );
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            u64::try_from(bytes.len())? == expected_bytes && hex::encode(Sha256::digest(&bytes)) == sha256,
            "study actual effect evidence digest changed"
        );
        Ok(bytes)
    }
    pub(in crate::skills::lifecycle) fn optional_study_trial(
        &self,
        id: &ExperimentId,
        stage: &str,
    ) -> Result<Option<StudyTrialReceipt>> {
        TransactionId::parse(stage)?;
        let name = format!("{stage}.json");
        match std::fs::symlink_metadata(self.study_path(id, &name)) {
            Ok(_) => Ok(Some(self.read_study_sealed(id, &name, TRIAL_DOMAIN)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub(in crate::skills::lifecycle) fn read_study_trial(
        &self,
        id: &ExperimentId,
        stage: &str,
        expected_sha256: &str,
    ) -> Result<StudyTrialReceipt> {
        TransactionId::parse(stage)?;
        require_digest(expected_sha256)?;
        let receipt: StudyTrialReceipt = self.read_study_sealed(id, &format!("{stage}.json"), TRIAL_DOMAIN)?;
        ensure!(
            receipt.binding.stage == stage
                && receipt.owner_scope == *self.owner_scope()
                && receipt.sha256()? == expected_sha256,
            "study trial evidence pin changed"
        );
        Ok(receipt)
    }
    /// Exports only a report just reconstructed by the crate's validating
    /// owner. No API imports a caller's serialized report as verified data.
    pub(in crate::skills::lifecycle) fn study_export_bytes(&self, report: &StudyReport) -> Result<Vec<u8>> {
        ensure!(
            report.manifest.owner_scope == *self.owner_scope(),
            "study export owner mismatch"
        );
        let value = ExportValue {
            report,
            evidence_sha256: report.sha256()?,
        };
        let payload = serde_json::to_vec(&value)?;
        let sha256 = digest(REPORT_DOMAIN, &value)?;
        let mac = hex::encode(self.study_key()?.hmac(
            KeyDomain::ReportOwnership,
            &[REPORT_DOMAIN, self.owner_scope().as_str().as_bytes(), &payload],
        ));
        let bytes = serde_json::to_vec(&Sealed { value, sha256, mac })?;
        // Export aggregates bounded per-stage records; it is not one store
        // record and does not enlarge any execution or retained-record limit.
        let bound = MAX_RECORD_BYTES
            .checked_mul(u64::try_from(
                crate::skills::lifecycle::experiment::MAX_EXPERIMENT_STAGES + 4,
            )?)
            .context("study export bound overflow")?;
        ensure!(u64::try_from(bytes.len())? <= bound, "study export bound exceeded");
        Ok(bytes)
    }
}
