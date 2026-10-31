use super::*;

#[test]
fn study_model_minima_cannot_be_removed_from_a_spec_only_admission() -> Result<()> {
    let (plan, _, _) = plan_fixture(2, 1)?;
    for component in 0..3 {
        let mut spec = plan.experiment.spec.clone();
        let stage = spec.stages.first_mut().context("fixture stage absent")?;
        match component {
            0 => stage.allocation.calls = 0,
            1 => stage.allocation.tokens = 0,
            _ => stage.allocation.output_bytes = 0,
        }
        assert!(
            spec.freeze().is_err(),
            "StudyTrial must retain the original positive model-resource minima"
        );
    }
    Ok(())
}

#[test]
fn mixed_study_and_verification_or_a_downgraded_operation_map_are_invalid_before_execution() -> Result<()> {
    let (plan, _, _) = plan_fixture(2, 1)?;
    for kind in [
        ExperimentStageKind::ModelProposal,
        ExperimentStageKind::IndependentVerification,
        ExperimentStageKind::GateReview,
    ] {
        let mut spec = plan.experiment.spec.clone();
        spec.stages.last_mut().context("fixture stage absent")?.kind = kind;
        assert!(
            spec.freeze().is_err(),
            "a distinct study kind must expose mixed plans to the original spec owner"
        );
    }
    let mut downgraded = plan.experiment;
    for stage in &mut downgraded.spec.stages {
        stage.kind = ExperimentStageKind::SolverTrial;
    }
    assert!(
        downgraded.spec.clone().freeze().is_ok(),
        "ordinary structural spec is insufficient to establish operation identity"
    );
    assert!(
        downgraded.validate().is_err(),
        "full operation map must reject StudyTrial disguised as ordinary SolverTrial"
    );
    Ok(())
}

#[test]
fn unchanged_stage_limit_rejects_thirty_three_pairs_without_a_fresh_budget_root() -> Result<()> {
    let (mut plan, _, _) = plan_fixture(2, 1)?;
    let source = plan.manifest.roots.first().context("fixture root absent")?.clone();
    for number in 2..33 {
        let mut root = source.clone();
        root.root_task_id = format!("extra-root-{number}");
        let case = root.cases.first_mut().context("fixture case absent")?;
        case.case_id = format!("extra-case-{number}");
        case.prompt = format!("Different independent input {number}");
        plan.manifest.roots.push(root);
    }
    assert!(compile_pairs(&plan.manifest.roots, 1, 7).is_err());
    assert!(plan.manifest.validate().is_err());
    assert_eq!(
        MAX_STUDY_PAIRS * 2,
        crate::skills::lifecycle::experiment::MAX_EXPERIMENT_STAGES
    );
    Ok(())
}

#[test]
fn a_fixture_that_already_satisfies_the_positive_effect_cannot_score_a_noop() -> Result<()> {
    let (mut plan, _, _) = plan_fixture(2, 1)?;
    let case = plan
        .manifest
        .roots
        .first_mut()
        .context("root")?
        .cases
        .first_mut()
        .context("case")?;
    case.fixtures.push(crate::skills::lifecycle::FixtureFile {
        path: "result.txt".into(),
        content: "VERIFIED\n".into(),
    });
    assert!(plan.manifest.validate().is_err());
    Ok(())
}

#[test]
fn prose_absence_and_unlisted_tool_substitutions_are_not_an_artifact_study_profile() -> Result<()> {
    let (plan, _, _) = plan_fixture(2, 1)?;
    for oracle in [
        Validator::ResponseContains {
            text: "VERIFIED".into(),
        },
        Validator::ArtifactAbsent {
            path: "result.txt".into(),
        },
    ] {
        let mut altered = plan.manifest.clone();
        altered
            .roots
            .first_mut()
            .context("root")?
            .cases
            .first_mut()
            .context("case")?
            .validators = vec![oracle];
        assert!(altered.validate().is_err());
    }
    let mut altered = plan.manifest;
    altered.case_tools.insert("case-0".into(), vec!["file_edit".into()]);
    assert!(
        altered.validate().is_err(),
        "a case cannot invoke a replacement operation outside its frozen global ceiling"
    );
    Ok(())
}

#[test]
fn labels_and_case_partition_edits_do_not_change_root_source_identity() -> Result<()> {
    let (plan, _, _) = plan_fixture(2, 1)?;
    let original = plan.manifest.roots.first().context("root")?;
    let mut renamed = original.clone();
    renamed.root_task_id = "renamed-root".into();
    let case = renamed.cases.first_mut().context("case")?;
    case.case_id = "renamed-case".into();
    case.split = EvalSplit::Holdout;
    case.task_family = "renamed-family".into();
    assert_eq!(
        original.provenance_sha256(&plan.manifest.owner_scope)?,
        renamed.provenance_sha256(&plan.manifest.owner_scope)?
    );
    assert_ne!(
        original.provenance_sha256(&plan.manifest.owner_scope)?,
        original.provenance_sha256(&OwnerScope::new("PRXOtherOperator")?)?
    );
    Ok(())
}

#[test]
fn training_source_relabel_and_changed_pair_order_are_rejected() -> Result<()> {
    let (plan, _, _) = plan_fixture(2, 1)?;
    let mut overlapping = plan.manifest.clone();
    let case = overlapping
        .roots
        .first()
        .context("root")?
        .cases
        .first()
        .context("case")?;
    overlapping.training_source_roots = crate::skills::lifecycle::suite::case_lesson_root_digests(case);
    assert!(overlapping.validate().is_err());
    let mut reordered = plan.manifest;
    let pair = reordered.pairs.first_mut().context("pair")?;
    pair.first_arm = pair.first_arm.other();
    assert!(
        reordered.validate().is_err(),
        "preregistered paired order cannot be changed after freezing"
    );
    Ok(())
}
