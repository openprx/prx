//! Pure-data fixtures do not construct admitted execution or verified owner
//! evidence. Actual owner/HTTP/artifact integration lives in v01_study tests.
use super::*;
use crate::skills::lifecycle::experiment::{
    ExperimentId, ExperimentOperation, ExperimentResources, ExperimentRunBinding, ExperimentStage, ExperimentStageKind,
    InstructionExperimentPlan,
};
use crate::skills::lifecycle::{
    Bundle, BundleEntry, CleanupState, EvalCase, EvalSplit, HydrationFact, InstalledSkill, ModelBinding, OwnerScope,
    RunObservation, RunReceipt, RunTerminal, SideEffects, SkillId, TransactionId, Validator,
};
use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::BTreeMap;

mod contracts;
mod grading;
mod statistics;
mod storage;

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn bundle(label: &str) -> Result<Bundle> {
    Ok(Bundle::from_entries(vec![BundleEntry {
        path: "SKILL.md".into(),
        bytes: format!(
            "---\nname: PRX study helper\ndescription: Bounded fixture instructions\n---\n\n# Instructions\n{label}\n"
        )
        .into_bytes(),
        executable: false,
    }])?)
}
fn protocol(phase: StudyPhase) -> Result<StudyProtocol> {
    Ok(StudyProtocol {
        protocol_id: "fixture-protocol".into(),
        phase,
        primary_metric: StudyMetric::Success,
        interval: StudyInterval::HoeffdingIndependentRootMeans,
        stop_rule: StudyStopRule::CompleteFrozenPlanOrSafetyAbort,
        alpha_ppm: 50_000,
        practical_gain_ppm: 100_000,
        min_independent_roots: 2,
        max_interval_half_width_ppm: 2_000_000,
        max_arm_calls: 10_000,
        max_arm_tokens: 1_000_000,
        max_arm_micro_usd: 100_000,
        max_arm_elapsed_ms: 1_000_000,
        calibration: if phase == StudyPhase::Confirmatory {
            Some(StudyCalibration {
                study: StudyExperimentRef {
                    id: ExperimentId::parse("unverified-calibration-fixture")?,
                    sha256: hash(b"calibration spec"),
                },
                report_sha256: hash(b"not an owner proof"),
                excluded_roots: vec![StudyExcludedRoot {
                    root_task_id: "development-only".into(),
                    prompt_sha256: vec![hash(b"other development input")],
                }],
                sample_and_uncertainty_rationale: "Synthetic decision-boundary fixture; not a live protocol.".into(),
                independent_root_sampling_basis: "Unverified fixture rows; owner admission is tested separately."
                    .into(),
            })
        } else {
            None
        },
    })
}

fn plan_fixture(roots: usize, repetitions: u32) -> Result<(StudyPlan, Bundle, Bundle)> {
    let baseline = bundle("Use the baseline procedure.")?;
    let h2 = bundle("Use the revised procedure and verify the artifact.")?;
    let skill = SkillId::parse("study-helper")?;
    let owner = OwnerScope::new("PRXOperator")?;
    let mut spec = crate::skills::lifecycle::experiment::test_specification();
    spec.id = ExperimentId::parse("unadmitted-study-fixture")?;
    spec.owner_scope = owner.clone();
    spec.pins.skill_id = skill.clone();
    spec.pins.baseline_revision = h2.revision_id().clone();
    spec.pins.baseline_record_digest = hash(b"h2 record");
    spec.pins.model = ModelBinding {
        provider_label: "fixture-provider".into(),
        model: "fixture-model".into(),
        temperature_milli: 100,
    };
    spec.pins.lesson_snapshot = None;
    spec.controlled_admission = None;
    spec.created_at = Utc::now();
    spec.elapsed_ms = 600_000;
    spec.expires_at = spec.created_at + chrono::Duration::milliseconds(600_000);
    spec.max_concurrent = 1;
    spec.resources = ExperimentResources {
        calls: 2048,
        tokens: 10_000_000,
        micro_usd: 1_000_000,
        tool_calls: 2048,
        output_bytes: 64 * 1024 * 1024,
        scratch_bytes: 256 * 1024 * 1024,
    };
    let per_trial = ExperimentResources {
        calls: 4,
        tokens: 4096,
        micro_usd: 100,
        tool_calls: 8,
        output_bytes: 64 * 1024,
        scratch_bytes: 2 * 1024 * 1024,
    };
    let mut case_tools = BTreeMap::new();
    let roots: Vec<_> = (0..roots)
        .map(|number| {
            let case_id = format!("case-{number}");
            case_tools.insert(case_id.clone(), vec!["file_write".into()]);
            StudyRoot {
                root_task_id: format!("root-{number}"),
                cases: vec![EvalCase {
                    case_id,
                    split: EvalSplit::Development,
                    task_family: "study-contract".into(),
                    prompt: format!(
                        "Independent fixture input {number}: write and verify result.txt using the instruction skill."
                    ),
                    fixtures: Vec::new(),
                    validators: vec![Validator::ArtifactSha256 {
                        path: "result.txt".into(),
                        sha256: hash(b"VERIFIED\n"),
                    }],
                    expect_skill_read: true,
                    critical: true,
                    max_elapsed_ms: 30_000,
                }],
            }
        })
        .collect();
    let source = StudyExperimentRef {
        id: ExperimentId::parse("optimizer-fixture")?,
        sha256: hash(b"optimizer spec"),
    };
    let manifest = StudyManifest {
        schema_version: STUDY_SCHEMA_VERSION,
        id: spec.id.clone(),
        owner_scope: owner,
        skill_id: skill,
        task_profile: STUDY_TASK_PROFILE.into(),
        runtime: spec.pins.clone(),
        origin: StudyOriginPin {
            experiment: source.clone(),
            gate_stage: "gate-review".into(),
            gate_pin_sha256: hash(b"gate pin"),
            proposal_id: "prop-fixture".into(),
            publication_tx: TransactionId::parse("publication-fixture")?,
            publication_sha256: hash(b"publication"),
            baseline: StudyRevisionPin {
                revision: baseline.revision_id().clone(),
                record_sha256: hash(b"baseline record"),
            },
            h2: StudyRevisionPin {
                revision: h2.revision_id().clone(),
                record_sha256: hash(b"h2 record"),
            },
        },
        optimizer_roots: vec![StudyOptimizerPin {
            experiment: source,
            head_sha256: hash(b"optimizer head"),
            actual: ExperimentResources::default(),
            reserved_liability: ExperimentResources::default(),
            pool_snapshot_sha256: hash(b"pool"),
            physical_observations_sha256: hash(b"physical"),
            physical_attempt_ids: Vec::new(),
        }],
        training_source_roots: vec![hash(b"unrelated source")],
        protocol: protocol(StudyPhase::Calibration)?,
        pairs: compile_pairs(&roots, repetitions, 7)?,
        roots,
        repetitions,
        order_seed: 7,
        required_tools: vec!["file_write".into()],
        case_tools,
        side_effects: SideEffects::WorkspaceWrite,
        resources: spec.resources,
        per_trial,
        elapsed_ms: spec.elapsed_ms,
    };
    let sha = manifest.sha256()?;
    let mut previous: Option<String> = None;
    let mut operations = BTreeMap::new();
    spec.stages.clear();
    for pair in &manifest.pairs {
        for arm in [pair.first_arm, pair.first_arm.other()] {
            let id = pair.stage_id(arm);
            let operation = ExperimentOperation::StudyTrial {
                input: StudyTrialInput {
                    study_sha256: sha.clone(),
                    pair_id: pair.pair_id.clone(),
                    arm,
                },
            };
            spec.stages.push(ExperimentStage {
                id: id.clone(),
                kind: ExperimentStageKind::StudyTrial,
                dependencies: previous.iter().cloned().collect(),
                input_sha256: operation.digest()?,
                allocation: per_trial,
            });
            operations.insert(id.clone(), operation);
            previous = Some(id);
        }
    }
    let plan = StudyPlan {
        manifest,
        experiment: InstructionExperimentPlan { spec, operations },
    };
    plan.validate()?;
    Ok((plan, baseline, h2))
}

fn observed_receipt(
    plan: &StudyPlan,
    bundle: &Bundle,
    pair: &StudyPair,
    arm: StudyArm,
    bytes: Option<&[u8]>,
) -> Result<(StudyTrialReceipt, BTreeMap<String, Option<Vec<u8>>>)> {
    let artifacts = BTreeMap::from([("result.txt".into(), bytes.map(<[u8]>::to_vec))]);
    let stage = pair.stage_id(arm);
    let model = format!(
        "{}/{}",
        plan.manifest.runtime.model.provider_label, plan.manifest.runtime.model.model
    );
    let physical = StudyPhysicalObservation {
        scope_id: hash(format!("{stage}-scope").as_bytes()),
        attempt_ids: vec![format!("unverified-fixture-{stage}")],
        calls: 1,
        settled_calls: 1,
        tokens: 10,
        micro_usd: 0,
        unresolved: false,
        models: vec![model.clone()],
    };
    let file = bundle.files().first().context("fixture instruction file missing")?;
    let observation = RunObservation {
        run_id: format!("v01-{}-{stage}", plan.manifest.id.as_str()),
        terminal: RunTerminal::Completed,
        response: "The requested file_write arguments were correct and the file was verified.".into(),
        elapsed_ms: 1,
        environment_hash: hash(b"same actual environment"),
        installed: Some(InstalledSkill {
            skill_id: plan.manifest.skill_id.clone(),
            revision_id: bundle.revision_id().clone(),
            bundle_manifest_hash: bundle.revision_id().digest_hex().into(),
            generation: 1,
        }),
        hydrations: vec![HydrationFact {
            skill_id: plan.manifest.skill_id.clone(),
            revision_id: bundle.revision_id().clone(),
            generation: 1,
            resource_path: file.path().into(),
            resource_sha256: hash(file.bytes()),
        }],
        experience_terminal: None,
        artifacts: artifacts.clone(),
        cleanup: CleanupState::Removed,
        tokens: Some(10),
        cost_micros: Some(0),
        models_used: vec![model],
        failure_detail: String::new(),
    };
    let operation = ExperimentOperation::StudyTrial {
        input: StudyTrialInput {
            study_sha256: plan.manifest.sha256()?,
            pair_id: pair.pair_id.clone(),
            arm,
        },
    };
    let mut receipt = StudyTrialReceipt {
        schema_version: STUDY_SCHEMA_VERSION,
        owner_scope: plan.manifest.owner_scope.clone(),
        input: StudyTrialInput {
            study_sha256: plan.manifest.sha256()?,
            pair_id: pair.pair_id.clone(),
            arm,
        },
        binding: ExperimentRunBinding {
            experiment_sha256: plan.experiment.spec.clone().freeze()?.digest().into(),
            stage,
            input_sha256: operation.digest()?,
        },
        root_task_id: pair.root_task_id.clone(),
        case_id: pair.case_id.clone(),
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
    let grade = receipt.compute_grade(&plan.manifest, bundle, &artifacts, &receipt.physical)?;
    receipt.validators = grade.validators;
    receipt.measured_and_valid = grade.measured_and_valid;
    receipt.invalid_reasons = grade.invalid_reasons;
    receipt.success = grade.success;
    receipt.quality_ppm = grade.quality_ppm;
    Ok((receipt, artifacts))
}

fn unverified_rows_for_pure_analysis(plan: &StudyPlan, baseline: &Bundle, h2: &Bundle) -> Result<Vec<StudyTrialRow>> {
    let mut rows = Vec::new();
    for pair in &plan.manifest.pairs {
        for arm in [pair.first_arm, pair.first_arm.other()] {
            let bundle = if arm == StudyArm::Baseline { baseline } else { h2 };
            let bytes: &[u8] = if arm == StudyArm::Baseline {
                b"WRONG\n"
            } else {
                b"VERIFIED\n"
            };
            let (receipt, _) = observed_receipt(plan, bundle, pair, arm, Some(bytes))?;
            rows.push(StudyTrialRow {
                stage: pair.stage_id(arm),
                input: receipt.input.clone(),
                root_task_id: pair.root_task_id.clone(),
                case_id: pair.case_id.clone(),
                state: StudyAttemptState::Settled,
                ever_reserved: true,
                settled_usage: ExperimentResources {
                    calls: 1,
                    tokens: 10,
                    tool_calls: 1,
                    ..Default::default()
                },
                retained_liability: ExperimentResources::default(),
                physical: receipt.physical.clone(),
                receipt: Some(receipt),
            });
        }
    }
    Ok(rows)
}

fn rebind_plan(plan: &mut StudyPlan) -> Result<()> {
    plan.manifest.pairs = compile_pairs(
        &plan.manifest.roots,
        plan.manifest.repetitions,
        plan.manifest.order_seed,
    )?;
    let spec = &mut plan.experiment.spec;
    spec.id = plan.manifest.id.clone();
    spec.owner_scope = plan.manifest.owner_scope.clone();
    spec.pins = plan.manifest.runtime.clone();
    spec.resources = plan.manifest.resources;
    spec.stages.clear();
    plan.experiment.operations.clear();
    let mut previous: Option<String> = None;
    let sha = plan.manifest.sha256()?;
    for pair in &plan.manifest.pairs {
        for arm in [pair.first_arm, pair.first_arm.other()] {
            let id = pair.stage_id(arm);
            let operation = ExperimentOperation::StudyTrial {
                input: StudyTrialInput {
                    study_sha256: sha.clone(),
                    pair_id: pair.pair_id.clone(),
                    arm,
                },
            };
            spec.stages.push(ExperimentStage {
                id: id.clone(),
                kind: ExperimentStageKind::StudyTrial,
                dependencies: previous.iter().cloned().collect(),
                allocation: plan.manifest.per_trial,
                input_sha256: operation.digest()?,
            });
            plan.experiment.operations.insert(id.clone(), operation);
            previous = Some(id);
        }
    }
    plan.validate()?;
    Ok(())
}
