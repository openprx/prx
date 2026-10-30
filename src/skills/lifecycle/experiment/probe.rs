//! Single-arm development trials and immutable evidence in the lifecycle store.
//! Reuses CaseExecutor and the evaluator's artifact/identity checks. This owner
//! cannot consume independent splits, install candidates, or publish revisions.
use super::{ExperimentError, ExperimentId, ExperimentJournal, ExperimentRunBinding};
use crate::skills::lifecycle::evaluate::{
    ArmResources, CaseContext, RunPlan, build_case_receipt, resource_digests, seal_digest,
};
use crate::skills::lifecycle::evaluator::{evaluator_invalidity, run_with_backstop};
use crate::skills::lifecycle::generation::ensure_dir_synced;
use crate::skills::lifecycle::store::{check_private, read_record, record_bytes};
use crate::skills::lifecycle::{
    Arm, ArmBinding, Bundle, CaseExecutor, CaseReceipt, CaseVerdict, EvalSplit, ExecutionRequest, FrozenSuite,
    InstallSkill, OwnerScope, RevisionId, SkillLifecycleStore,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub const MAX_DEVELOPMENT_PROBE_RUNS: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevelopmentProbeReceipt {
    pub schema_version: u32,
    pub binding: ExperimentRunBinding,
    pub owner_scope: OwnerScope,
    pub suite_digest: String,
    pub revision: RevisionId,
    pub proposal_stage: Option<String>,
    pub executor: String,
    pub cases: Vec<CaseReceipt>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}
impl DevelopmentProbeReceipt {
    pub fn digest(&self) -> Result<String, ExperimentError> {
        seal_digest(self).map_err(|error| ExperimentError::InvalidEvidence(error.to_string()))
    }
    /// A completed probe can report semantic failure. Invalid or absent runs
    /// are never eligible for candidate comparison.
    pub fn complete(&self) -> bool {
        !self.cases.is_empty()
            && self
                .cases
                .iter()
                .all(|case| matches!(case.verdict, CaseVerdict::Pass | CaseVerdict::Fail { .. }))
    }
    pub fn passed(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| case.verdict == CaseVerdict::Pass)
            .count()
    }
    pub(super) fn within_budget(&self, suite: &FrozenSuite) -> bool {
        let budget = &suite.suite().budget;
        let elapsed = self
            .cases
            .iter()
            .fold(0_u64, |sum, case| sum.saturating_add(case.run.elapsed_ms));
        let tokens = self
            .cases
            .iter()
            .try_fold(0_u64, |sum, case| sum.checked_add(case.run.tokens?));
        let cost = self
            .cases
            .iter()
            .try_fold(0_u64, |sum, case| sum.checked_add(case.run.cost_micros?));
        elapsed <= budget.max_total_elapsed_ms
            && tokens.is_some_and(|value| value <= budget.max_total_tokens)
            && cost.is_some_and(|value| value <= budget.max_total_cost_micros)
    }
}

fn refusal(message: &str) -> ExperimentError {
    ExperimentError::InvalidEvidence(message.into())
}

pub(in crate::skills::lifecycle) fn probe_run_count(suite: &FrozenSuite) -> Result<usize, ExperimentError> {
    let cases = suite.cases_of(EvalSplit::Development).count();
    let runs = cases
        .checked_mul(usize::try_from(suite.suite().trials).map_err(|_| refusal("trial count overflow"))?)
        .ok_or_else(|| refusal("probe run count overflow"))?;
    if cases < usize::try_from(suite.suite().min_cases).unwrap_or(usize::MAX)
        || runs > MAX_DEVELOPMENT_PROBE_RUNS
        || runs > usize::try_from(suite.suite().budget.max_runs).unwrap_or(0)
    {
        return Err(refusal(
            "development probe sample is empty, too small or exceeds its run limit",
        ));
    }
    Ok(runs)
}

impl SkillLifecycleStore {
    /// Only development cases are reachable here; validators remain outside
    /// ExecutionRequest, and every run receives a new private workspace.
    pub(in crate::skills) async fn run_development_probe(
        &self,
        journal: &ExperimentJournal,
        stage: &str,
        proposal_stage: Option<String>,
        bundle: &Bundle,
        executor: &dyn CaseExecutor,
        cancel: &CancellationToken,
    ) -> Result<DevelopmentProbeReceipt, ExperimentError> {
        self.check_owner_scope(&journal.plan.spec().owner_scope)?;
        let pins = &journal.plan.spec().pins;
        let suite = self
            .read_suite(&pins.suite_digest)
            .map_err(|error| refusal(&error.to_string()))?;
        let planned = probe_run_count(&suite)?;
        let binding = ArmBinding {
            revision_id: bundle.revision_id().clone(),
            bundle_manifest_hash: bundle.revision_id().digest_hex().into(),
        };
        let resources = ArmResources {
            baseline: None,
            candidate: resource_digests(bundle),
        };
        let evaluation_id = format!("probe-{}-{stage}", journal.plan.spec().id.as_str());
        let context = CaseContext {
            owner: self.owner_scope(),
            evaluation_id: &evaluation_id,
            suite: &suite,
            observed_generation: pins.baseline_generation,
            resources: &resources,
        };
        let stage_binding = crate::runtime::experiment_stage::current()
            .ok_or_else(|| refusal("development probe has no admitted runtime"))?
            .binding();
        let denied_paths = [self.root().to_path_buf()];
        let mut result = DevelopmentProbeReceipt {
            schema_version: 1,
            binding: stage_binding,
            owner_scope: self.owner_scope().clone(),
            suite_digest: suite.digest().into(),
            revision: binding.revision_id.clone(),
            proposal_stage,
            executor: executor.identity(),
            cases: Vec::with_capacity(planned),
            started_at: Utc::now(),
            finished_at: Utc::now(),
        };
        let mut run_ids = BTreeSet::new();
        let mut environment = None;
        let mut elapsed = 0_u64;
        let mut tokens = 0_u64;
        let mut micro_usd = 0_u64;
        let budget = &suite.suite().budget;
        'cases: for case in suite.cases_of(EvalSplit::Development) {
            let mut paths: Vec<_> = case
                .validators
                .iter()
                .filter_map(|item| item.artifact_path())
                .map(str::to_string)
                .collect();
            paths.sort();
            paths.dedup();
            for trial in 0..suite.suite().trials {
                if cancel.is_cancelled()
                    || elapsed >= budget.max_total_elapsed_ms
                    || tokens >= budget.max_total_tokens
                    || micro_usd >= budget.max_total_cost_micros
                {
                    break 'cases;
                }
                let run_id = format!("evalrun-{}", uuid::Uuid::new_v4().simple());
                let request = ExecutionRequest {
                    run_id: &run_id,
                    prompt: &case.prompt,
                    fixtures: &case.fixtures,
                    collect_artifacts: &paths,
                    skill: Some(InstallSkill {
                        skill_id: &pins.skill_id,
                        bundle,
                    }),
                    deadline: Duration::from_millis(case.max_elapsed_ms),
                    required_tools: &suite.suite().required_tools,
                    side_effects: suite.suite().side_effects,
                    denied_paths: &denied_paths,
                };
                let started = Instant::now();
                let observed = run_with_backstop(executor, &request, cancel).await;
                let wall = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let (observation, forced) = match observed {
                    Ok(mut value) => {
                        value.elapsed_ms = value.elapsed_ms.max(wall);
                        let forced = evaluator_invalidity(&value, &mut run_ids, &mut environment);
                        (Some(value), forced)
                    }
                    Err(_) => (None, None),
                };
                elapsed = elapsed.saturating_add(observation.as_ref().map_or(wall, |value| value.elapsed_ms));
                tokens = tokens.saturating_add(
                    observation
                        .as_ref()
                        .and_then(|value| value.tokens)
                        .unwrap_or_else(|| budget.unmeasured_tokens()),
                );
                micro_usd = micro_usd.saturating_add(
                    observation
                        .as_ref()
                        .and_then(|value| value.cost_micros)
                        .unwrap_or_else(|| budget.unmeasured_cost_micros()),
                );
                let run = RunPlan {
                    case,
                    trial,
                    arm: Arm::Candidate,
                    order: u32::try_from(result.cases.len()).map_err(|_| refusal("probe order overflow"))?,
                    arm_binding: Some(&binding),
                };
                result.cases.push(
                    build_case_receipt(&context, &run, &run_id, observation.as_ref(), wall, forced)
                        .map_err(|error| refusal(&error.to_string()))?,
                );
            }
        }
        result.finished_at = Utc::now();
        // Retain failures and partial evidence. The stage adapter independently
        // checks completeness and budget; a stored receipt is not success.
        Ok(result)
    }

    pub(in crate::skills) fn write_development_probe(
        &self,
        journal: &ExperimentJournal,
        receipt: &DevelopmentProbeReceipt,
    ) -> Result<String, ExperimentError> {
        let spec = journal.plan.spec();
        self.check_owner_scope(&receipt.owner_scope)?;
        let stage = spec
            .stages
            .iter()
            .find(|item| item.id == receipt.binding.stage)
            .ok_or_else(|| refusal("probe stage is not admitted"))?;
        if receipt.binding.experiment_sha256 != journal.plan.digest()
            || receipt.binding.input_sha256 != stage.input_sha256
            || receipt.suite_digest != spec.pins.suite_digest
        {
            return Err(refusal("probe bindings do not match the experiment"));
        }
        let _store = self.lock_store(true, Duration::from_secs(10))?;
        let _skill = self.lock_skill(&spec.pins.skill_id, Duration::from_secs(10))?;
        let root = self.root().join("experiment-evidence");
        ensure_dir_synced(self.root(), &root)?;
        let directory = root.join(spec.id.as_str());
        ensure_dir_synced(&root, &directory)?;
        let path = directory.join(format!("{}.json", stage.id));
        let bytes = record_bytes(receipt, &path)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > crate::skills::lifecycle::store::MAX_RECORD_BYTES {
            return Err(refusal("development evidence exceeds the retained record bound"));
        }
        let digest = receipt.digest()?;
        if std::fs::symlink_metadata(&path).is_ok() {
            check_private(&path)?;
            let existing: DevelopmentProbeReceipt = read_record(&path)?;
            if existing.digest()? != digest {
                return Err(ExperimentError::Conflict);
            }
        } else {
            self.write_atomic(&spec.pins.skill_id, &path, &bytes)?;
        }
        Ok(digest)
    }

    pub(in crate::skills) fn read_development_probe(
        &self,
        id: &ExperimentId,
        stage: &str,
        expected_digest: &str,
    ) -> Result<DevelopmentProbeReceipt, ExperimentError> {
        crate::skills::lifecycle::TransactionId::parse(stage)?;
        let root = self.root().join("experiment-evidence");
        let directory = root.join(id.as_str());
        let path = directory.join(format!("{stage}.json"));
        for entry in [&root, &directory, &path] {
            check_private(entry)?;
        }
        let receipt: DevelopmentProbeReceipt = read_record(&path)?;
        self.check_owner_scope(&receipt.owner_scope)?;
        if receipt.schema_version != 1 || receipt.binding.stage != stage || receipt.digest()? != expected_digest {
            return Err(refusal("development evidence digest or identity changed"));
        }
        Ok(receipt)
    }
}
