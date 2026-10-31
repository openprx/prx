//! Preregistered root-cluster analysis. Missing/invalid planned rows contribute
//! zero task score, remain in denominators, and prohibit a positive decision.
use super::{
    StudyAnalysis, StudyArm, StudyArmSummary, StudyAttemptState, StudyBenefitDecision, StudyManifest, StudyPhase,
    StudyRootDifference, StudyTrialRow,
};
use crate::skills::lifecycle::RunTerminal;
use anyhow::{Context, Result, ensure};

/// For independent root means in [-1, 1], Hoeffding's two-sided bound is
/// sqrt(2 ln(2/alpha)/n). Within-root repeats do not increase n.
pub(in crate::skills::lifecycle) fn interval_half_width(roots: usize, alpha_ppm: u32) -> Result<f64> {
    ensure!(
        roots > 0 && alpha_ppm > 0 && alpha_ppm < 1_000_000,
        "invalid study interval inputs"
    );
    let n = f64::from(u32::try_from(roots)?);
    let alpha = f64::from(alpha_ppm) / 1_000_000.0;
    Ok((2.0 * (2.0 / alpha).ln() / n).sqrt())
}

fn arm_summary(rows: &[StudyTrialRow], arm: StudyArm) -> Result<StudyArmSummary> {
    let mut summary = StudyArmSummary::default();
    for row in rows.iter().filter(|row| row.input.arm == arm) {
        summary.declared = summary.declared.checked_add(1).context("study count overflow")?;
        summary.reserved += u32::from(row.ever_reserved);
        summary.not_started += u32::from(row.state == StudyAttemptState::NotStarted);
        summary.proven_unsent += u32::from(row.state == StudyAttemptState::ProvenUnsent);
        summary.unknown += u32::from(matches!(
            row.state,
            StudyAttemptState::Reserved
                | StudyAttemptState::Dispatched
                | StudyAttemptState::Unknown
                | StudyAttemptState::SettledCleanupUnknown
        ));
        summary.settled_usage = summary.settled_usage.checked_add(row.settled_usage)?;
        summary.retained_liability = summary.retained_liability.checked_add(row.retained_liability)?;
        if row.state == StudyAttemptState::Settled {
            summary.settled_and_clean += 1;
            let receipt = row.receipt.as_ref().context("settled study row lacks owner receipt")?;
            summary.elapsed_ms = summary
                .elapsed_ms
                .checked_add(receipt.run.elapsed_ms)
                .context("study elapsed overflow")?;
            if receipt.measured_and_valid {
                summary.valid += 1;
                summary.successes += u32::from(receipt.success);
                summary.failures += u32::from(!receipt.success);
            } else {
                summary.invalid += 1;
            }
            summary.cancelled_runs += u32::from(receipt.run.terminal == RunTerminal::Cancelled);
        }
    }
    Ok(summary)
}

fn score(row: Option<&StudyTrialRow>, manifest: &StudyManifest) -> f64 {
    row.filter(|row| row.state == StudyAttemptState::Settled)
        .and_then(|row| row.receipt.as_ref())
        .filter(|receipt| receipt.measured_and_valid)
        .map_or(0.0, |receipt| {
            f64::from(receipt.metric_ppm(manifest.protocol.primary_metric)) / 1_000_000.0
        })
}

pub(in crate::skills::lifecycle) fn summarize(
    manifest: &StudyManifest,
    rows: &[StudyTrialRow],
    cancelled: bool,
) -> Result<(StudyArmSummary, StudyArmSummary, StudyAnalysis)> {
    ensure!(
        rows.len() == manifest.pairs.len().saturating_mul(2),
        "study report omitted planned rows"
    );
    let baseline = arm_summary(rows, StudyArm::Baseline)?;
    let h2 = arm_summary(rows, StudyArm::H2)?;
    let mut roots = Vec::new();
    let mut environment_mismatches = 0_u32;
    let mut critical_h2_failures = 0_u32;
    for root in &manifest.roots {
        let pairs: Vec<_> = manifest
            .pairs
            .iter()
            .filter(|pair| pair.root_task_id == root.root_task_id)
            .collect();
        ensure!(!pairs.is_empty(), "statistical root has no pairs");
        let mut baseline_total = 0.0;
        let mut h2_total = 0.0;
        for pair in &pairs {
            let find = |arm| {
                rows.iter()
                    .find(|row| row.input.pair_id == pair.pair_id && row.input.arm == arm)
            };
            let b = find(StudyArm::Baseline);
            let h = find(StudyArm::H2);
            baseline_total += score(b, manifest);
            h2_total += score(h, manifest);
            if let (Some(b), Some(h)) = (
                b.and_then(|row| row.receipt.as_ref()),
                h.and_then(|row| row.receipt.as_ref()),
            ) {
                if b.run.environment_hash != h.run.environment_hash {
                    environment_mismatches += 1;
                }
            }
            if manifest.case(pair)?.critical
                && !h
                    .and_then(|row| row.receipt.as_ref())
                    .is_some_and(|receipt| receipt.measured_and_valid && receipt.success)
            {
                critical_h2_failures += 1;
            }
        }
        let count = u32::try_from(pairs.len())?;
        let divisor = f64::from(count);
        roots.push(StudyRootDifference {
            root_task_id: root.root_task_id.clone(),
            root_content_sha256: root.provenance_sha256(&manifest.owner_scope)?,
            paired_trials: count,
            baseline_mean: baseline_total / divisor,
            h2_mean: h2_total / divisor,
            difference: (h2_total - baseline_total) / divisor,
        });
    }
    let n = u32::try_from(roots.len())?;
    let difference = roots.iter().map(|root| root.difference).sum::<f64>() / f64::from(n);
    let half_width = interval_half_width(roots.len(), manifest.protocol.alpha_ppm)?;
    let interval_low = (difference - half_width).max(-1.0);
    let interval_high = (difference + half_width).min(1.0);
    let complete = !cancelled
        && environment_mismatches == 0
        && rows.iter().all(|row| {
            row.state == StudyAttemptState::Settled
                && !row.physical.unresolved
                && row.receipt.as_ref().is_some_and(|receipt| receipt.measured_and_valid)
        });
    let within = [&baseline, &h2].iter().all(|arm| {
        arm.settled_usage.calls <= manifest.protocol.max_arm_calls
            && arm.settled_usage.tokens <= manifest.protocol.max_arm_tokens
            && arm.settled_usage.micro_usd <= manifest.protocol.max_arm_micro_usd
            && arm.elapsed_ms <= manifest.protocol.max_arm_elapsed_ms
    });
    let practical = f64::from(manifest.protocol.practical_gain_ppm) / 1_000_000.0;
    let success_non_regression = h2.successes > 0 && h2.successes >= baseline.successes;
    let benefit = if !complete {
        StudyBenefitDecision::MechanismIncomplete
    } else if manifest.protocol.phase == StudyPhase::Calibration {
        StudyBenefitDecision::Inconclusive
    } else if critical_h2_failures == 0 && success_non_regression && within && interval_low >= practical {
        StudyBenefitDecision::GainSupported
    } else if interval_high < practical || critical_h2_failures > 0 || !within || !success_non_regression {
        StudyBenefitDecision::NoPracticalGain
    } else {
        StudyBenefitDecision::Inconclusive
    };
    Ok((
        baseline,
        h2,
        StudyAnalysis {
            primary_metric: manifest.protocol.primary_metric,
            statistical_unit:
                "independent root task; variants/repetitions averaged within root, not calls or accounting roots".into(),
            independent_roots: n,
            declared_pairs: u32::try_from(manifest.pairs.len())?,
            roots,
            difference,
            interval_low,
            interval_high,
            interval_half_width: half_width,
            critical_h2_failures,
            task_success_non_regression: success_non_regression,
            environment_mismatches,
            cost_latency_within_preregistered_ceilings: within,
            mechanism_complete: complete,
            benefit,
            retain_baseline_or_shadow: benefit != StudyBenefitDecision::GainSupported,
            qualification: false,
        },
    ))
}
