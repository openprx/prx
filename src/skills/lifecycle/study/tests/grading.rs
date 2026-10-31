use super::*;

#[test]
fn measured_model_identity_requires_the_exact_ledger_provider_and_model_pair() -> Result<()> {
    let (mut plan, baseline, _) = plan_fixture(2, 1)?;
    plan.manifest.runtime.model.provider_label = "custom:http://127.0.0.1:18080/v1".into();
    plan.manifest.runtime.model.model = "prx/model".into();
    plan.experiment.spec.pins.model = plan.manifest.runtime.model.clone();
    let pair = plan.manifest.pairs.first().context("pair")?;
    let (receipt, artifacts) = observed_receipt(&plan, &baseline, pair, StudyArm::Baseline, Some(b"VERIFIED\n"))?;
    let valid = receipt.compute_grade(&plan.manifest, &baseline, &artifacts, &receipt.physical)?;
    assert!(valid.measured_and_valid && valid.success);
    for model in [
        "prx/model",
        "other-provider/prx/model",
        "custom:http://127.0.0.1:18081/v1/prx/model",
        "custom:http://127.0.0.1:18080/v1/other-model",
    ] {
        let mut changed = receipt.clone();
        changed.run.models_used = vec![model.into()];
        changed.physical.models = vec![model.into()];
        let grade = changed.compute_grade(&plan.manifest, &baseline, &artifacts, &changed.physical)?;
        assert!(
            !grade.measured_and_valid && !grade.success,
            "accepted model identity {model}"
        );
        assert_eq!(grade.invalid_reasons, ["model_or_environment_mismatch"]);
    }
    for change_run in [false, true] {
        let mut changed = receipt.clone();
        let models = if change_run {
            &mut changed.run.models_used
        } else {
            &mut changed.physical.models
        };
        *models = vec!["other-provider/prx/model".into()];
        let grade = changed.compute_grade(&plan.manifest, &baseline, &artifacts, &changed.physical)?;
        assert!(!grade.measured_and_valid && !grade.success);
        assert_eq!(grade.invalid_reasons, ["model_or_environment_mismatch"]);
    }
    Ok(())
}

#[test]
fn correct_prose_and_requested_arguments_without_an_actual_file_are_failure() -> Result<()> {
    let (plan, baseline, _) = plan_fixture(2, 1)?;
    let pair = plan.manifest.pairs.first().context("pair")?;
    let (mut receipt, artifacts) = observed_receipt(&plan, &baseline, pair, StudyArm::Baseline, None)?;
    receipt.run.response_sha256 = hash(b"VERIFIED file_write(path=result.txt,content=VERIFIED)");
    receipt.success = true;
    receipt.quality_ppm = 1_000_000;
    receipt.validators.iter_mut().for_each(|outcome| outcome.passed = true);
    let grade = receipt.compute_grade(&plan.manifest, &baseline, &artifacts, &receipt.physical)?;
    assert!(
        grade.measured_and_valid,
        "a measured semantic failure is not relabelled as missing instrumentation"
    );
    assert!(!grade.success);
    assert_eq!(grade.quality_ppm, 0);
    assert!(
        grade.validators.iter().all(|outcome| !outcome.passed),
        "grade derives from actual bytes, never stored success flags"
    );
    Ok(())
}

#[test]
fn a_tool_failure_or_wrong_artifact_cannot_be_repaired_by_a_success_sentence() -> Result<()> {
    let (plan, baseline, _) = plan_fixture(2, 1)?;
    let pair = plan.manifest.pairs.first().context("pair")?;
    let (mut receipt, artifacts) = observed_receipt(&plan, &baseline, pair, StudyArm::Baseline, Some(b"WRONG\n"))?;
    let wrong = receipt.compute_grade(&plan.manifest, &baseline, &artifacts, &receipt.physical)?;
    assert!(!wrong.success);
    assert_eq!(wrong.quality_ppm, 0);
    receipt.run.terminal = RunTerminal::Failed;
    let failed = receipt.compute_grade(&plan.manifest, &baseline, &artifacts, &receipt.physical)?;
    assert!(!failed.success);
    assert_eq!(failed.quality_ppm, 0);
    Ok(())
}

#[test]
fn actual_file_bytes_are_read_independently_of_the_claimed_artifact_digest() -> Result<()> {
    let (plan, baseline, _) = plan_fixture(2, 1)?;
    let pair = plan.manifest.pairs.first().context("pair")?;
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(file.path(), b"VERIFIED\n")?;
    let actual = std::fs::read(file.path())?;
    let (receipt, mut artifacts) = observed_receipt(&plan, &baseline, pair, StudyArm::Baseline, Some(&actual))?;
    assert!(
        receipt
            .compute_grade(&plan.manifest, &baseline, &artifacts, &receipt.physical)?
            .success
    );
    std::fs::write(file.path(), b"CHANGED\n")?;
    artifacts.insert("result.txt".into(), Some(std::fs::read(file.path())?));
    assert!(
        receipt
            .compute_grade(&plan.manifest, &baseline, &artifacts, &receipt.physical)
            .is_err()
    );
    Ok(())
}

#[test]
fn missing_meter_cleanup_or_real_hydration_prevents_a_positive_outcome() -> Result<()> {
    let (plan, baseline, _) = plan_fixture(2, 1)?;
    let pair = plan.manifest.pairs.first().context("pair")?;
    let (receipt, artifacts) = observed_receipt(&plan, &baseline, pair, StudyArm::Baseline, Some(b"VERIFIED\n"))?;
    for attack in 0..4 {
        let mut changed = receipt.clone();
        match attack {
            0 => changed.run.tokens = None,
            1 => changed.run.cleanup = CleanupState::Failed,
            2 => changed.run.hydrations.clear(),
            _ => changed.physical.unresolved = true,
        }
        let grade = changed.compute_grade(&plan.manifest, &baseline, &artifacts, &changed.physical)?;
        assert!(!grade.success);
        assert_eq!(grade.quality_ppm, 0);
    }
    Ok(())
}

#[test]
fn another_revision_or_usage_claim_cannot_substitute_for_the_actual_trial() -> Result<()> {
    let (plan, baseline, h2) = plan_fixture(2, 1)?;
    let pair = plan.manifest.pairs.first().context("pair")?;
    let (mut receipt, artifacts) = observed_receipt(&plan, &baseline, pair, StudyArm::Baseline, Some(b"VERIFIED\n"))?;
    assert!(
        receipt
            .compute_grade(&plan.manifest, &h2, &artifacts, &receipt.physical)
            .is_err()
    );
    receipt.run.cost_micros = Some(999);
    let grade = receipt.compute_grade(&plan.manifest, &baseline, &artifacts, &receipt.physical)?;
    assert!(!grade.measured_and_valid && !grade.success);
    Ok(())
}
