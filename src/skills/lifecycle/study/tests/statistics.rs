//! Synthetic rows test arithmetic/decision boundaries only, never owner proof.
use super::*;

#[test]
fn repeated_trials_and_accounting_calls_do_not_become_independent_roots() -> Result<()> {
    let (plan, baseline, h2) = plan_fixture(2, 5)?;
    let rows = unverified_rows_for_pure_analysis(&plan, &baseline, &h2)?;
    let (_, _, analysis) = summarize(&plan.manifest, &rows, false)?;
    assert_eq!(analysis.independent_roots, 2);
    assert_eq!(analysis.declared_pairs, 10);
    assert!(
        analysis.interval_low < 0.0,
        "ten correlated repeats cannot give the precision of ten independent tasks"
    );
    assert_eq!(analysis.benefit, StudyBenefitDecision::Inconclusive);
    assert!(!analysis.qualification);
    Ok(())
}

fn confirmatory_fixture() -> Result<(StudyPlan, Vec<StudyTrialRow>)> {
    let (mut plan, baseline, h2) = plan_fixture(32, 1)?;
    plan.manifest.protocol = protocol(StudyPhase::Confirmatory)?;
    for root in &mut plan.manifest.roots {
        for case in &mut root.cases {
            case.split = EvalSplit::Holdout;
        }
    }
    rebind_plan(&mut plan)?;
    // These remain plain data. No owner admission/report can consume this as
    // calibrated proof; the actual owner integration exercises that boundary.
    let rows = unverified_rows_for_pure_analysis(&plan, &baseline, &h2)?;
    Ok((plan, rows))
}

#[test]
fn calibration_never_mints_gain_even_at_an_otherwise_positive_statistical_boundary() -> Result<()> {
    let (plan, baseline, h2) = plan_fixture(32, 1)?;
    let rows = unverified_rows_for_pure_analysis(&plan, &baseline, &h2)?;
    let (_, _, analysis) = summarize(&plan.manifest, &rows, false)?;
    assert!(analysis.interval_low >= 0.1);
    assert!(analysis.mechanism_complete);
    assert_eq!(analysis.benefit, StudyBenefitDecision::Inconclusive);
    assert!(analysis.retain_baseline_or_shadow && !analysis.qualification);
    Ok(())
}

#[test]
fn complete_independent_data_can_meet_the_frozen_rule_without_acquiring_qualification() -> Result<()> {
    let (plan, rows) = confirmatory_fixture()?;
    let (_, h2, analysis) = summarize(&plan.manifest, &rows, false)?;
    assert_eq!(h2.successes, 32);
    assert_eq!(analysis.benefit, StudyBenefitDecision::GainSupported);
    assert!(
        !analysis.qualification,
        "statistical outcome is never a deterministic gate or publication authority"
    );
    Ok(())
}

#[test]
fn one_unknown_or_unclean_row_cannot_be_dropped_from_a_favorable_remainder() -> Result<()> {
    let (plan, rows) = confirmatory_fixture()?;
    for phase in [
        StudyAttemptState::Unknown,
        StudyAttemptState::SettledCleanupUnknown,
        StudyAttemptState::NotStarted,
    ] {
        let mut incomplete = rows.clone();
        let row = incomplete
            .iter_mut()
            .find(|row| row.input.arm == StudyArm::H2)
            .context("H2 row")?;
        row.state = phase;
        row.retained_liability = plan.manifest.per_trial;
        row.settled_usage = ExperimentResources::default();
        if phase != StudyAttemptState::SettledCleanupUnknown {
            row.receipt = None;
        }
        let (_, arm, analysis) = summarize(&plan.manifest, &incomplete, false)?;
        assert_eq!(arm.declared, 32);
        assert!(arm.valid < arm.declared);
        assert_eq!(analysis.declared_pairs, 32);
        assert_eq!(analysis.benefit, StudyBenefitDecision::MechanismIncomplete);
        assert!(arm.retained_liability.calls >= plan.manifest.per_trial.calls);
        assert!(analysis.retain_baseline_or_shadow);
    }
    Ok(())
}

#[test]
fn physical_prefix_observation_is_not_added_again_as_settlement_or_called_free() -> Result<()> {
    let (plan, mut rows) = confirmatory_fixture()?;
    let row = rows
        .iter_mut()
        .find(|row| row.input.arm == StudyArm::H2)
        .context("H2 row")?;
    row.state = StudyAttemptState::Unknown;
    row.settled_usage = ExperimentResources::default();
    row.retained_liability = plan.manifest.per_trial;
    row.receipt = None;
    row.physical.unresolved = true;
    assert_eq!(
        row.physical.calls, 1,
        "known physical prefix is retained, not replaced with zero"
    );
    let (_, h2, analysis) = summarize(&plan.manifest, &rows, false)?;
    assert_eq!(h2.settled_usage.calls, 31);
    assert_eq!(h2.retained_liability.calls, plan.manifest.per_trial.calls);
    assert_eq!(h2.unknown, 1);
    assert_eq!(analysis.benefit, StudyBenefitDecision::MechanismIncomplete);
    Ok(())
}

#[test]
fn cost_ceiling_environment_or_critical_failure_cannot_be_overruled_by_quality() -> Result<()> {
    let (plan, rows) = confirmatory_fixture()?;
    let mut boundary = plan.manifest.clone();
    boundary.protocol.max_arm_calls = 32;
    assert_eq!(
        summarize(&boundary, &rows, false)?.2.benefit,
        StudyBenefitDecision::GainSupported
    );
    boundary.protocol.max_arm_calls = 31;
    assert_eq!(
        summarize(&boundary, &rows, false)?.2.benefit,
        StudyBenefitDecision::NoPracticalGain
    );
    let mut mismatch = rows.clone();
    mismatch
        .iter_mut()
        .find(|row| row.input.arm == StudyArm::H2)
        .context("H2 row")?
        .receipt
        .as_mut()
        .context("receipt")?
        .run
        .environment_hash = hash(b"different served environment");
    assert_eq!(
        summarize(&plan.manifest, &mismatch, false)?.2.benefit,
        StudyBenefitDecision::MechanismIncomplete
    );
    let mut critical = rows.clone();
    let receipt = critical
        .iter_mut()
        .find(|row| row.input.arm == StudyArm::H2)
        .context("H2 row")?
        .receipt
        .as_mut()
        .context("receipt")?;
    receipt.success = false;
    receipt.quality_ppm = 0;
    assert_eq!(
        summarize(&plan.manifest, &critical, false)?.2.benefit,
        StudyBenefitDecision::NoPracticalGain
    );
    assert_eq!(
        summarize(&plan.manifest, &rows, true)?.2.benefit,
        StudyBenefitDecision::MechanismIncomplete
    );
    Ok(())
}

#[test]
fn insufficient_root_precision_is_rejected_before_opening_the_protocol() -> Result<()> {
    let (mut plan, _, _) = plan_fixture(2, 1)?;
    plan.manifest.protocol.max_interval_half_width_ppm = 100_000;
    assert!(
        plan.manifest.validate().is_err(),
        "a caller cannot claim narrow uncertainty from two root tasks"
    );
    Ok(())
}

#[test]
fn omitted_planned_row_is_an_error_instead_of_a_smaller_success_denominator() -> Result<()> {
    let (plan, mut rows) = confirmatory_fixture()?;
    rows.pop().context("last frozen row")?;
    assert!(summarize(&plan.manifest, &rows, false).is_err());
    Ok(())
}
