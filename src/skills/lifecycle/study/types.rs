//! Frozen study intent and independently pinned instruction revisions.
use super::{digest, interval_half_width, prompt_digest, require_digest};
use crate::skills::lifecycle::experiment::{
    ExperimentId, ExperimentPins, ExperimentResources, InstructionExperimentPlan, MAX_EXPERIMENT_STAGES,
};
use crate::skills::lifecycle::{
    EvalCase, EvalSplit, OwnerScope, RevisionId, SideEffects, SkillId, TransactionId, Validator,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const STUDY_SCHEMA_VERSION: u32 = 1;
pub const MAX_STUDY_PAIRS: usize = MAX_EXPERIMENT_STAGES / 2;
pub const STUDY_TASK_PROFILE: &str = "artifact_backed_instructional_workspace_v1";
const MAX_OPTIMIZATION_ROOTS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudyArm {
    Baseline,
    H2,
}
impl StudyArm {
    pub const fn other(self) -> Self {
        match self {
            Self::Baseline => Self::H2,
            Self::H2 => Self::Baseline,
        }
    }
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Baseline => "b",
            Self::H2 => "h",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyTrialInput {
    pub study_sha256: String,
    pub pair_id: String,
    pub arm: StudyArm,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyExperimentRef {
    pub id: ExperimentId,
    pub sha256: String,
}

/// Root tasks are statistical clusters. Cases/variants and repetitions within
/// one root are averaged; neither calls nor the budget root are sample units.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyRoot {
    pub root_task_id: String,
    pub cases: Vec<EvalCase>,
}
impl StudyRoot {
    pub fn provenance_sha256(&self, owner: &OwnerScope) -> Result<String> {
        let sources: BTreeSet<_> = self
            .cases
            .iter()
            .flat_map(crate::skills::lifecycle::suite::case_lesson_root_digests)
            .collect();
        digest(b"prx-v01-owner-root-content-v1\0", &(owner, sources))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyExcludedRoot {
    pub root_task_id: String,
    pub prompt_sha256: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyCalibration {
    pub study: StudyExperimentRef,
    pub report_sha256: String,
    pub excluded_roots: Vec<StudyExcludedRoot>,
    pub sample_and_uncertainty_rationale: String,
    /// Explicitly reviewed sampling premise, not inferred from unequal IDs.
    pub independent_root_sampling_basis: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudyMetric {
    Success,
    ArtifactQuality,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudyInterval {
    HoeffdingIndependentRootMeans,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudyPhase {
    Calibration,
    Confirmatory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudyStopRule {
    CompleteFrozenPlanOrSafetyAbort,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyProtocol {
    pub protocol_id: String,
    pub phase: StudyPhase,
    pub primary_metric: StudyMetric,
    pub interval: StudyInterval,
    pub stop_rule: StudyStopRule,
    /// Tail probability in millionths; no default threshold is approved.
    pub alpha_ppm: u32,
    pub practical_gain_ppm: u32,
    pub min_independent_roots: u32,
    pub max_interval_half_width_ppm: u32,
    pub max_arm_calls: u64,
    pub max_arm_tokens: u64,
    pub max_arm_micro_usd: u64,
    pub max_arm_elapsed_ms: u64,
    pub calibration: Option<StudyCalibration>,
}
impl StudyProtocol {
    pub fn validate(&self, roots: usize) -> Result<()> {
        TransactionId::parse(&self.protocol_id)?;
        ensure!(
            (1..=500_000).contains(&self.alpha_ppm),
            "invalid study tail probability"
        );
        ensure!(
            (1..=1_000_000).contains(&self.practical_gain_ppm),
            "study needs a positive practical gain"
        );
        ensure!(
            self.min_independent_roots >= 2 && roots >= usize::try_from(self.min_independent_roots)?,
            "insufficient independent root tasks"
        );
        ensure!(
            (1..=2_000_000).contains(&self.max_interval_half_width_ppm),
            "invalid planned uncertainty ceiling"
        );
        let width = interval_half_width(roots, self.alpha_ppm)?;
        ensure!(
            width * 1_000_000.0 <= f64::from(self.max_interval_half_width_ppm),
            "declared independent sample cannot meet preregistered uncertainty ceiling"
        );
        ensure!(
            self.max_arm_calls > 0 && self.max_arm_tokens > 0 && self.max_arm_elapsed_ms > 0,
            "missing study cost/latency ceiling"
        );
        if self.phase == StudyPhase::Calibration {
            ensure!(
                self.calibration.is_none(),
                "calibration cannot borrow a confirmatory calibration proof"
            );
            return Ok(());
        }
        let calibration = self
            .calibration
            .as_ref()
            .context("confirmatory study requires completed owner calibration")?;
        require_digest(&calibration.study.sha256)?;
        require_digest(&calibration.report_sha256)?;
        for text in [
            &calibration.sample_and_uncertainty_rationale,
            &calibration.independent_root_sampling_basis,
        ] {
            ensure!(
                !text.trim().is_empty() && text.len() <= 4096,
                "study calibration rationale is absent or oversized"
            );
        }
        ensure!(
            !calibration.excluded_roots.is_empty() && calibration.excluded_roots.len() <= 128,
            "calibration roots must be declared"
        );
        let mut seen = BTreeSet::new();
        for root in &calibration.excluded_roots {
            TransactionId::parse(&root.root_task_id)?;
            ensure!(seen.insert(&root.root_task_id), "duplicate calibration root");
            ensure!(
                !root.prompt_sha256.is_empty() && root.prompt_sha256.len() <= 32,
                "calibration inputs are absent or oversized"
            );
            root.prompt_sha256.iter().try_for_each(|pin| require_digest(pin))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyDraft {
    pub schema_version: u32,
    pub id: ExperimentId,
    pub skill_id: SkillId,
    pub origin: StudyExperimentRef,
    pub gate_stage: String,
    /// Additional, disclosed optimizer roots. The origin is always charged too.
    pub additional_optimizer_roots: Vec<StudyExperimentRef>,
    pub protocol: StudyProtocol,
    pub roots: Vec<StudyRoot>,
    pub repetitions: u32,
    pub order_seed: u64,
    pub required_tools: Vec<String>,
    pub case_tools: BTreeMap<String, Vec<String>>,
    pub side_effects: SideEffects,
    pub resources: ExperimentResources,
    pub per_trial: ExperimentResources,
    pub elapsed_ms: u64,
}
impl StudyDraft {
    pub(in crate::skills::lifecycle) fn validate_shape(&self) -> Result<()> {
        ensure!(
            !self.roots.is_empty() && self.roots.len() <= MAX_STUDY_PAIRS && (1..=5).contains(&self.repetitions),
            "invalid bounded study sample"
        );
        ensure!(
            self.additional_optimizer_roots.len() < MAX_OPTIMIZATION_ROOTS
                && self.required_tools.len() <= 3
                && self.case_tools.len() <= MAX_STUDY_PAIRS,
            "study intent exceeds supported bounds"
        );
        let mut total = 0_usize;
        for root in &self.roots {
            TransactionId::parse(&root.root_task_id)?;
            ensure!(
                !root.cases.is_empty() && root.cases.len() <= MAX_STUDY_PAIRS,
                "invalid root cluster"
            );
            for case in &root.cases {
                crate::skills::lifecycle::suite::validate_case(case)?;
                total = total
                    .checked_add(usize::try_from(self.repetitions)?)
                    .context("study sample overflow")?;
                ensure!(total <= MAX_STUDY_PAIRS, "study exceeds the unchanged 32-pair bound");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyRevisionPin {
    pub revision: RevisionId,
    pub record_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyOptimizerPin {
    pub experiment: StudyExperimentRef,
    pub head_sha256: String,
    pub actual: ExperimentResources,
    pub reserved_liability: ExperimentResources,
    pub pool_snapshot_sha256: String,
    /// Hash of actual parent physical observations, not caller cost numbers.
    pub physical_observations_sha256: String,
    pub physical_attempt_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyOriginPin {
    pub experiment: StudyExperimentRef,
    pub gate_stage: String,
    pub gate_pin_sha256: String,
    pub proposal_id: String,
    pub publication_tx: TransactionId,
    pub publication_sha256: String,
    pub baseline: StudyRevisionPin,
    pub h2: StudyRevisionPin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyPair {
    pub pair_id: String,
    pub root_task_id: String,
    pub case_id: String,
    pub repetition: u32,
    pub first_arm: StudyArm,
}
impl StudyPair {
    pub fn stage_id(&self, arm: StudyArm) -> String {
        format!("{}-{}", self.pair_id, arm.suffix())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyManifest {
    pub schema_version: u32,
    pub id: ExperimentId,
    pub owner_scope: OwnerScope,
    pub skill_id: SkillId,
    pub task_profile: String,
    /// Existing runtime pins; baseline here is the active store pin at planning,
    /// while the statistical baseline is explicitly origin.baseline.
    pub runtime: ExperimentPins,
    pub origin: StudyOriginPin,
    pub optimizer_roots: Vec<StudyOptimizerPin>,
    pub training_source_roots: Vec<String>,
    pub protocol: StudyProtocol,
    pub roots: Vec<StudyRoot>,
    pub repetitions: u32,
    pub order_seed: u64,
    pub pairs: Vec<StudyPair>,
    pub required_tools: Vec<String>,
    pub case_tools: BTreeMap<String, Vec<String>>,
    pub side_effects: SideEffects,
    pub resources: ExperimentResources,
    pub per_trial: ExperimentResources,
    pub elapsed_ms: u64,
}

impl StudyManifest {
    pub fn sha256(&self) -> Result<String> {
        digest(b"prx-v01-study-manifest-v1\0", self)
    }
    pub fn revision(&self, arm: StudyArm) -> &StudyRevisionPin {
        match arm {
            StudyArm::Baseline => &self.origin.baseline,
            StudyArm::H2 => &self.origin.h2,
        }
    }
    pub fn case(&self, pair: &StudyPair) -> Result<&EvalCase> {
        self.roots
            .iter()
            .find(|root| root.root_task_id == pair.root_task_id)
            .and_then(|root| root.cases.iter().find(|case| case.case_id == pair.case_id))
            .context("study pair has no frozen root/case")
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == STUDY_SCHEMA_VERSION
                && self.runtime.skill_id == self.skill_id
                && self.task_profile == STUDY_TASK_PROFILE,
            "invalid study schema/skill/profile"
        );
        ensure!(
            self.runtime.lesson_snapshot.is_none(),
            "holdout cannot deliver learning guidance"
        );
        ensure!(
            (1..=5).contains(&self.repetitions),
            "study repetition count is outside the bounded contract"
        );
        ensure!(
            !self.roots.is_empty() && self.roots.len() <= MAX_STUDY_PAIRS,
            "invalid study roots"
        );
        self.protocol.validate(self.roots.len())?;
        ensure!(
            !self.optimizer_roots.is_empty() && self.optimizer_roots.len() <= MAX_OPTIMIZATION_ROOTS,
            "invalid optimizer references"
        );
        require_digest(&self.origin.gate_pin_sha256)?;
        require_digest(&self.origin.publication_sha256)?;
        require_digest(&self.origin.baseline.record_sha256)?;
        require_digest(&self.origin.h2.record_sha256)?;
        ensure!(
            self.origin.baseline.revision != self.origin.h2.revision,
            "study revisions must differ"
        );
        ensure!(
            self.elapsed_ms > 0
                && self.per_trial.calls > 0
                && self.per_trial.tokens > 0
                && self.per_trial.output_bytes > 0,
            "study trial lacks admitted resources"
        );
        ensure!(self.required_tools.len() <= 3, "study tool surface is not admitted");
        let tools: BTreeSet<_> = self.required_tools.iter().map(String::as_str).collect();
        ensure!(
            tools.len() == self.required_tools.len()
                && tools
                    .iter()
                    .all(|name| matches!(*name, "file_read" | "file_write" | "file_edit")),
            "study tool surface is not admitted"
        );
        ensure!(
            self.side_effects == SideEffects::WorkspaceWrite
                || !tools.contains("file_write") && !tools.contains("file_edit"),
            "read-only study declares writes"
        );
        ensure!(
            self.side_effects == SideEffects::WorkspaceWrite,
            "initial V01 profile requires actual artifact-producing workspace effects"
        );
        let training: BTreeSet<_> = self.training_source_roots.iter().map(String::as_str).collect();
        ensure!(
            !training.is_empty() && training.len() == self.training_source_roots.len() && training.len() <= 2048,
            "training/source exclusions are absent or oversized"
        );
        self.training_source_roots
            .iter()
            .try_for_each(|root| require_digest(root))?;
        let excluded: BTreeSet<_> = self
            .protocol
            .calibration
            .iter()
            .flat_map(|calibration| calibration.excluded_roots.iter().map(|root| root.root_task_id.as_str()))
            .collect();
        let excluded_prompts: BTreeSet<_> = self
            .protocol
            .calibration
            .iter()
            .flat_map(|calibration| {
                calibration
                    .excluded_roots
                    .iter()
                    .flat_map(|root| root.prompt_sha256.iter().map(String::as_str))
            })
            .collect();
        let mut roots = BTreeSet::new();
        let mut root_contents = BTreeSet::new();
        let mut cases = BTreeSet::new();
        let mut inputs = BTreeSet::new();
        let mut expected_pairs = 0_usize;
        for root in &self.roots {
            TransactionId::parse(&root.root_task_id)?;
            ensure!(
                roots.insert(root.root_task_id.as_str()) && !excluded.contains(root.root_task_id.as_str()),
                "duplicate or calibration-overlapping root"
            );
            ensure!(
                root_contents.insert(root.provenance_sha256(&self.owner_scope)?),
                "root labels do not create independent content"
            );
            ensure!(
                !root.cases.is_empty() && root.cases.len() <= MAX_STUDY_PAIRS,
                "empty or oversized root cluster"
            );
            for case in &root.cases {
                crate::skills::lifecycle::suite::validate_case(case)?;
                validate_artifact_task(case)?;
                let case_tools = self.case_tools.get(&case.case_id).context("case tool ceiling absent")?;
                let distinct: BTreeSet<_> = case_tools.iter().map(String::as_str).collect();
                ensure!(
                    distinct.len() == case_tools.len()
                        && distinct.iter().all(|tool| tools.contains(tool))
                        && (distinct.contains("file_write") || distinct.contains("file_edit")),
                    "case tool ceiling is invalid or cannot produce its actual effect"
                );
                ensure!(
                    crate::skills::lifecycle::suite::case_lesson_root_digests(case)
                        .iter()
                        .all(|root| !training.contains(root.as_str())),
                    "study case overlaps actual frozen training/pool source"
                );
                let expected = match self.protocol.phase {
                    StudyPhase::Calibration => EvalSplit::Development,
                    StudyPhase::Confirmatory => EvalSplit::Holdout,
                };
                ensure!(case.split == expected, "study tasks use the wrong preregistered phase");
                let input = prompt_digest(&case.prompt);
                ensure!(
                    cases.insert(case.case_id.as_str())
                        && inputs.insert(input.clone())
                        && !excluded_prompts.contains(input.as_str()),
                    "duplicate or calibration-overlapping case"
                );
                expected_pairs = expected_pairs
                    .checked_add(usize::try_from(self.repetitions)?)
                    .context("study pair overflow")?;
            }
        }
        ensure!(self.case_tools.len() == cases.len(), "unexpected case-tool entry");
        ensure!(
            expected_pairs <= MAX_STUDY_PAIRS && self.pairs.len() == expected_pairs,
            "study exceeds the unchanged 64-stage/32-pair bound"
        );
        ensure!(
            self.pairs == compile_pairs(&self.roots, self.repetitions, self.order_seed)?,
            "study order/sample was changed"
        );
        let mut reserved = ExperimentResources::default();
        for _ in 0..self.pairs.len().saturating_mul(2) {
            reserved = reserved.checked_add(self.per_trial)?;
        }
        ensure!(
            reserved.within(self.resources),
            "study allocations exceed its single root budget"
        );
        for pin in &self.optimizer_roots {
            require_digest(&pin.experiment.sha256)?;
            require_digest(&pin.head_sha256)?;
            require_digest(&pin.pool_snapshot_sha256)?;
            require_digest(&pin.physical_observations_sha256)?;
        }
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            u64::try_from(bytes.len())? <= crate::skills::lifecycle::store::MAX_RECORD_BYTES / 2,
            "study manifest exceeds retained record bound"
        );
        Ok(())
    }
}

fn validate_artifact_task(case: &EvalCase) -> Result<()> {
    let mut positive = false;
    let mut initially_unsatisfied = false;
    for validator in &case.validators {
        match validator {
            Validator::ArtifactContains { path, text } => {
                positive = true;
                initially_unsatisfied |= case
                    .fixtures
                    .iter()
                    .find(|fixture| fixture.path == *path)
                    .is_none_or(|fixture| !fixture.content.contains(text));
            }
            Validator::ArtifactSha256 { path, sha256 } => {
                positive = true;
                initially_unsatisfied |= case
                    .fixtures
                    .iter()
                    .find(|fixture| fixture.path == *path)
                    .is_none_or(|fixture| hex::encode(Sha256::digest(fixture.content.as_bytes())) != *sha256);
            }
            _ => anyhow::bail!(
                "initial study profile admits only positive artifact oracles, not prose or unconfirmed absence"
            ),
        }
    }
    ensure!(
        positive && initially_unsatisfied,
        "artifact-backed study requires a positive real output not already satisfied by its fixture"
    );
    Ok(())
}

pub(in crate::skills::lifecycle) fn compile_pairs(
    roots: &[StudyRoot],
    repetitions: u32,
    seed: u64,
) -> Result<Vec<StudyPair>> {
    let mut pairs = Vec::new();
    for root in roots {
        for case in &root.cases {
            for repetition in 0..repetitions {
                ensure!(
                    pairs.len() < MAX_STUDY_PAIRS,
                    "study pair enumeration exceeds its bound"
                );
                let order = digest(
                    b"prx-v01-pair-order-v1\0",
                    &(seed, &root.root_task_id, &case.case_id, repetition),
                )?;
                let first = order.as_bytes().first().copied().context("empty order hash")?;
                pairs.push(StudyPair {
                    pair_id: format!("p{:04}", pairs.len()),
                    root_task_id: root.root_task_id.clone(),
                    case_id: case.case_id.clone(),
                    repetition,
                    first_arm: if first % 2 == 0 {
                        StudyArm::Baseline
                    } else {
                        StudyArm::H2
                    },
                });
            }
        }
    }
    Ok(pairs)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyPlan {
    pub manifest: StudyManifest,
    pub experiment: InstructionExperimentPlan,
}
impl StudyPlan {
    /// Structural checks grant nothing; only the service may seal/admit this
    /// plan after re-resolving actual owner/runtime/origin facts.
    pub fn validate(&self) -> Result<()> {
        self.manifest.validate()?;
        self.experiment.validate()?;
        let manifest = &self.manifest;
        let spec = &self.experiment.spec;
        ensure!(
            spec.controlled_admission.is_none(),
            "V01 study transport is local operator CLI, not a deserialized entrypoint delegation"
        );
        ensure!(
            spec.id == manifest.id && spec.owner_scope == manifest.owner_scope && spec.pins == manifest.runtime,
            "study plan identity/runtime mismatch"
        );
        ensure!(
            spec.resources == manifest.resources && spec.elapsed_ms == manifest.elapsed_ms && spec.max_concurrent == 1,
            "study resource or schedule mutation"
        );
        ensure!(
            spec.stages.len() == manifest.pairs.len().saturating_mul(2),
            "study plan omitted or added stages"
        );
        let sha256 = manifest.sha256()?;
        let mut previous: Option<String> = None;
        let mut stages = spec.stages.iter();
        for pair in &manifest.pairs {
            for arm in [pair.first_arm, pair.first_arm.other()] {
                let stage = stages.next().context("study stage absent")?;
                let id = pair.stage_id(arm);
                let operation = crate::skills::lifecycle::experiment::ExperimentOperation::StudyTrial {
                    input: StudyTrialInput {
                        study_sha256: sha256.clone(),
                        pair_id: pair.pair_id.clone(),
                        arm,
                    },
                };
                ensure!(
                    stage.id == id
                        && stage.kind == crate::skills::lifecycle::experiment::ExperimentStageKind::StudyTrial
                        && stage.dependencies == previous.iter().cloned().collect::<Vec<_>>()
                        && stage.allocation == manifest.per_trial
                        && stage.input_sha256 == operation.digest()?
                        && self.experiment.operations.get(&id) == Some(&operation),
                    "study stage/order/operation changed"
                );
                previous = Some(id);
            }
        }
        Ok(())
    }
}
