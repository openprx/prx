//! Decision evaluation (SP-CDP-A 5.12, CDP 15; CDP-PLAN row CDP-10).
//!
//! Evaluation reads the store (read-only connections) and compares recorded
//! predictions with the outcomes the runtime observed. The predictions are
//! predictions, not causal effects:
//!
//! - accuracy is **agreement with the observed successful path**;
//! - a counterfactual or policy-effect estimate is reported only with
//!   exploration data that has overlap: a minimum propensity of at least
//!   0.01 for every compared action and an effective sample size of at least
//!   100. Otherwise it is labeled unavailable.
//!
//! Imported v1 decisions are left out, and decisions without a complete
//! outcome count against coverage.
//!
//! A report is versioned and written `0600` to
//! `<workspace>/self/causal_tree/reports/<id>.json`. **Nothing in this module
//! writes configuration:** thresholds change only through the operator's
//! `ApplyCalibration`/`SetPolicy` mutations (CDP-12), which record the report
//! id.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use rand::{RngExt, SeedableRng};
use serde::Serialize;

use super::store::{DecisionScope, DecisionStoreReader, DecisionView, EvaluationRows, StoreError, TimeWindow};

/// The report format version.
pub(crate) const REPORT_VERSION: u32 = 2;

/// The smallest propensity a compared action may have.
const MIN_PROPENSITY: f64 = 0.01;

/// The smallest effective sample size of a counterfactual estimate.
const MIN_ESS: f64 = 100.0;

/// Bootstrap resamples of the confidence interval.
const BOOTSTRAP_RESAMPLES: usize = 1000;

/// Equal-width bins of the expected calibration error.
const ECE_BINS: usize = 10;

/// The agreement a proposed threshold must reach (D-092).
const TARGET_AGREEMENT: f64 = 0.9;

/// The smallest number of compared decisions a proposal is made from.
const MIN_CALIBRATION_DECISIONS: u64 = 100;

/// What the accuracy figures mean (CDP 15).
const ACCURACY_MEANING: &str = "agreement with the observed successful path, not causal accuracy";

/// Why a figure is not reported.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Availability<T> {
    /// The figure.
    Available { value: T },
    /// It cannot be computed honestly from this data.
    Unavailable { reason: &'static str },
}

/// Thresholds a report proposes, for `ApplyCalibration` (D-092).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CalibrationProposal {
    /// The one pinned backend the window ran on: `"<kind>:<model>"`.
    pub(crate) profile: String,
    /// The endpoint that backend was configured with (D-102 H-2): a
    /// calibration applies only to the same endpoint.
    pub(crate) endpoint: String,
    /// Successful decisions with a next-action distribution it is made from.
    pub(crate) decisions: u64,
    /// The lowest top-probability at and above which the next-action
    /// prediction agreed with the observed path at least 90% of the time.
    pub(crate) minimum_choice_confidence: f64,
    /// The lowest top-1/top-2 margin at and above which decisions succeeded
    /// at least 90% of the time (at most 0.5).
    pub(crate) minimum_top_margin: f64,
}

/// Per-class agreement of the argmax prediction with the observed path.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct ClassMetrics {
    /// Decisions compared (a successful, complete outcome with an executed
    /// value and a recorded distribution).
    pub(crate) compared: u64,
    pub(crate) accuracy: Option<f64>,
    /// Predicted class → observed class → count.
    pub(crate) confusion: BTreeMap<String, BTreeMap<String, u64>>,
    pub(crate) precision: BTreeMap<String, Option<f64>>,
    pub(crate) recall: BTreeMap<String, Option<f64>>,
}

/// Brier score and expected calibration error.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct Calibration {
    pub(crate) samples: u64,
    pub(crate) brier: f64,
    pub(crate) ece: f64,
}

/// Success by top-1/top-2 margin of the next-action distribution.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct MarginBucket {
    pub(crate) from: f64,
    pub(crate) to: f64,
    pub(crate) decisions: u64,
    pub(crate) successes: u64,
}

/// The builtin shadow compared with the selected path.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ShadowComparison {
    /// Decisions with a shadow choice and a complete outcome.
    pub(crate) compared: u64,
    /// Of those, where the selected action differed from the shadow's.
    pub(crate) disagreements: u64,
    pub(crate) success_rate_when_agreeing: Option<f64>,
    pub(crate) success_rate_when_disagreeing: Option<f64>,
    /// Estimated reward of always following the shadow's action minus the
    /// observed mean reward: only with exploration overlap.
    pub(crate) regret: Availability<f64>,
    /// The same against the recorded fallback path.
    pub(crate) fallback_regret: Availability<f64>,
}

/// Rates over decisions with an outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub(crate) struct OutcomeRates {
    pub(crate) outcomes: u64,
    pub(crate) success_rate: Option<f64>,
    pub(crate) fallback_rate: Option<f64>,
    /// Outcomes with more than one provider attempt.
    pub(crate) retry_rate: Option<f64>,
}

/// Prompt and tool-definition bytes before and after narrowing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ByteTotals {
    pub(crate) outcomes: u64,
    pub(crate) prompt_before: i64,
    pub(crate) prompt_after: i64,
    pub(crate) tool_before: i64,
    pub(crate) tool_after: i64,
}

/// Capability narrowing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Narrowing {
    /// Tool attempts the narrowed set denied (false narrowing).
    pub(crate) denied_tool_attempts: i64,
    pub(crate) outcomes_with_denials: u64,
    /// Whether every used capability was initially allowed: the capability
    /// sets are recorded with the outcomes from CDP-22.
    pub(crate) capability_recall: Availability<f64>,
}

/// Decision latency percentiles (nearest rank), in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Latency {
    pub(crate) p50: Option<i64>,
    pub(crate) p95: Option<i64>,
    pub(crate) p99: Option<i64>,
}

/// Backend health over the window's decisions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub(crate) struct BackendRates {
    /// Decisions with an error class.
    pub(crate) failure_rate: Option<f64>,
    /// Decisions with an attempt skipped by an open breaker.
    pub(crate) circuit_rate: Option<f64>,
    /// Decisions the builtin served.
    pub(crate) builtin_rate: Option<f64>,
}

/// The counterfactual estimate of always taking one action and target.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct PolicyEstimate {
    pub(crate) action: String,
    pub(crate) target: String,
    /// Explored decisions that took it.
    pub(crate) taken: u64,
    pub(crate) min_propensity: f64,
    pub(crate) ess: f64,
    pub(crate) ips: f64,
    pub(crate) doubly_robust: f64,
    /// The 95% bootstrap interval of the doubly robust estimate.
    pub(crate) ci95: (f64, f64),
}

/// Coverage of the window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub(crate) struct Coverage {
    pub(crate) decisions: u64,
    pub(crate) complete_outcomes: u64,
    pub(crate) incomplete_outcomes: u64,
    pub(crate) without_outcome: u64,
    pub(crate) legacy_excluded: u64,
    /// Decisions with a complete outcome over all decisions.
    pub(crate) coverage: Option<f64>,
}

/// The window and scope a report covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReportScope {
    /// `local_operator` or `owner`.
    pub(crate) kind: &'static str,
    pub(crate) from_ms: i64,
    pub(crate) to_ms: i64,
}

/// An evaluation report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct EvaluationReport {
    pub(crate) report_version: u32,
    pub(crate) report_id: String,
    pub(crate) created_at_ms: i64,
    pub(crate) store_schema: i64,
    pub(crate) policy_versions: BTreeSet<String>,
    pub(crate) scorer_versions: BTreeSet<String>,
    pub(crate) builtin_version: &'static str,
    pub(crate) scope: ReportScope,
    pub(crate) accuracy_meaning: &'static str,
    pub(crate) coverage: Coverage,
    pub(crate) next_action: ClassMetrics,
    pub(crate) capability_target: ClassMetrics,
    pub(crate) next_action_calibration: Option<Calibration>,
    pub(crate) capability_target_calibration: Option<Calibration>,
    pub(crate) feature_calibration: BTreeMap<String, Calibration>,
    pub(crate) margin: Vec<MarginBucket>,
    pub(crate) shadow: ShadowComparison,
    pub(crate) narrowing: Narrowing,
    pub(crate) rates: OutcomeRates,
    pub(crate) bytes: ByteTotals,
    pub(crate) latency_ms: Latency,
    pub(crate) backend: BackendRates,
    /// Priced cost per successful turn (PMM price policy, D-027).
    pub(crate) cost_per_successful_turn_micro_usd: Option<f64>,
    /// Proposed thresholds: only when the whole window ran on one pinned
    /// backend (D-092).
    pub(crate) calibration: Availability<CalibrationProposal>,
    /// What the counterfactual section estimates.
    pub(crate) counterfactual_estimand: &'static str,
    /// IPS and doubly robust estimates per (action, target), only with
    /// exploration overlap.
    pub(crate) counterfactual: Availability<Vec<PolicyEstimate>>,
}

/// What a report is computed with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvaluationOptions {
    pub(crate) report_id: uuid::Uuid,
    pub(crate) created_at_ms: i64,
}

impl EvaluationOptions {
    /// A new report id (UUIDv7) at the current time.
    pub(crate) fn now() -> Self {
        Self {
            report_id: uuid::Uuid::now_v7(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// Why a report could not be produced.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EvaluateError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the evaluation report could not be written: {0}")]
    Io(#[from] std::io::Error),
    #[error("the evaluation report could not be encoded: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Evaluate `scope`'s decisions in `window`.
///
/// # Errors
///
/// [`StoreError`] when the store cannot be read.
pub(crate) fn evaluate(
    reader: &DecisionStoreReader,
    scope: &DecisionScope,
    window: TimeWindow,
    options: &EvaluationOptions,
) -> Result<EvaluationReport, StoreError> {
    let rows = reader.evaluation_rows(scope, window)?;
    let schema = reader.counts()?.schema_version;
    let kind = match scope {
        DecisionScope::Owner { .. } => "owner",
        DecisionScope::LocalOperator { .. } => "local_operator",
    };
    Ok(report(
        &rows,
        ReportScope {
            kind,
            from_ms: window.from_ms,
            to_ms: window.to_ms,
        },
        schema,
        options,
    ))
}

/// Write `report` to `<workspace>/self/causal_tree/reports/<id>.json`
/// (`0600`, never overwriting).
///
/// # Errors
///
/// [`EvaluateError`] when the file cannot be created or written.
pub(crate) fn write_report(workspace_dir: &Path, report: &EvaluationReport) -> Result<PathBuf, EvaluateError> {
    // The directories are created 0700 and never through a symlink
    // (D-091 F-6).
    let dir = crate::decision::key::prepare_decision_dir(workspace_dir)
        .map_err(|reason| EvaluateError::Io(std::io::Error::other(reason)))?
        .join("reports");
    match std::fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(EvaluateError::Io(std::io::Error::other(
                "the reports path is not a plain directory",
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            builder.create(&dir)?;
        }
        Err(error) => return Err(EvaluateError::Io(error)),
    }
    let path = dir.join(format!("{}.json", report.report_id));
    let mut options = std::fs::OpenOptions::new();
    // Exclusive and never through a symlink (D-093 N-8).
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&path)?;
    file.write_all(&serde_json::to_vec_pretty(report)?)?;
    file.sync_all()?;
    Ok(path)
}

fn is_success(classification: &str) -> bool {
    matches!(classification, "success" | "fallback_success")
}

/// The complete outcome of a decision, if it has one.
fn complete_outcome(decision: &DecisionView) -> Option<&super::store::OutcomeView> {
    decision
        .outcomes
        .iter()
        .find(|outcome| outcome.completeness == "complete")
}

/// The reward of an outcome: its recorded `reward`, else 1 for a success
/// and 0 otherwise.
fn reward_of(outcome: &super::store::OutcomeView) -> f64 {
    outcome
        .reward
        .as_ref()
        .and_then(|reward| reward.get("reward"))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| if is_success(&outcome.classification) { 1.0 } else { 0.0 })
}

/// The class with the highest probability (ties: the first in code order).
fn argmax(distribution: &BTreeMap<String, f64>) -> Option<(&str, f64)> {
    distribution.iter().fold(None, |best, (class, probability)| match best {
        Some((_, top)) if top >= *probability => best,
        _ => Some((class.as_str(), *probability)),
    })
}

/// A count as a float. Counts here stay far below 2^53, where the
/// conversion is exact.
#[allow(clippy::cast_precision_loss)]
const fn as_f64(count: u64) -> f64 {
    count as f64
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| as_f64(numerator) / as_f64(denominator))
}

/// Agreement of the argmax of `distribution_of` with `observed_of`.
fn class_metrics<'a>(
    decisions: impl Iterator<Item = (&'a BTreeMap<String, f64>, &'a str)>,
) -> (ClassMetrics, Option<Calibration>) {
    let mut metrics = ClassMetrics::default();
    let mut calibration = CalibrationBins::default();
    let mut correct = 0_u64;
    for (distribution, observed) in decisions {
        let Some((predicted, confidence)) = argmax(distribution) else {
            continue;
        };
        metrics.compared += 1;
        let hit = predicted == observed;
        correct += u64::from(hit);
        *metrics
            .confusion
            .entry(predicted.to_string())
            .or_default()
            .entry(observed.to_string())
            .or_default() += 1;
        let brier: f64 = distribution
            .iter()
            .map(|(class, probability)| {
                let truth = if class == observed { 1.0 } else { 0.0 };
                (probability - truth).powi(2)
            })
            .sum::<f64>()
            + if distribution.contains_key(observed) { 0.0 } else { 1.0 };
        calibration.add(confidence, if hit { 1.0 } else { 0.0 }, brier);
    }
    metrics.accuracy = ratio(correct, metrics.compared);
    let classes: BTreeSet<String> = metrics
        .confusion
        .iter()
        .flat_map(|(predicted, row)| std::iter::once(predicted.clone()).chain(row.keys().cloned()))
        .collect();
    for class in classes {
        let true_positive = metrics
            .confusion
            .get(&class)
            .and_then(|row| row.get(&class))
            .copied()
            .unwrap_or(0);
        let predicted: u64 = metrics.confusion.get(&class).map_or(0, |row| row.values().sum());
        let observed: u64 = metrics.confusion.values().filter_map(|row| row.get(&class)).sum();
        metrics.precision.insert(class.clone(), ratio(true_positive, predicted));
        metrics.recall.insert(class, ratio(true_positive, observed));
    }
    (metrics, calibration.finish())
}

/// Accumulates the Brier score and the equal-width calibration bins.
#[derive(Default)]
struct CalibrationBins {
    samples: u64,
    brier: f64,
    /// Per bin: count, summed confidence, summed outcome.
    bins: [(u64, f64, f64); ECE_BINS],
}

impl CalibrationBins {
    fn add(&mut self, confidence: f64, outcome: f64, brier: f64) {
        let confidence = confidence.clamp(0.0, 1.0);
        let index = ((confidence * as_f64(ECE_BINS as u64)).floor() as usize).min(ECE_BINS - 1);
        if let Some(bin) = self.bins.get_mut(index) {
            bin.0 += 1;
            bin.1 += confidence;
            bin.2 += outcome;
        }
        self.samples += 1;
        self.brier += brier;
    }

    fn finish(&self) -> Option<Calibration> {
        (self.samples > 0).then(|| {
            let total = as_f64(self.samples);
            let ece = self
                .bins
                .iter()
                .filter(|(count, _, _)| *count > 0)
                .map(|(count, confidence, outcome)| {
                    let count = as_f64(*count);
                    (count / total) * (outcome / count - confidence / count).abs()
                })
                .sum();
            Calibration {
                samples: self.samples,
                brier: self.brier / total,
                ece,
            }
        })
    }
}

/// The observed truth of a Noul feature: a boolean (or 0/1) under the
/// feature's name in the outcome's reward components.
fn observed_feature(outcome: &super::store::OutcomeView, feature: &str) -> Option<f64> {
    let value = outcome.reward.as_ref()?.get(feature)?;
    value
        .as_bool()
        .map(|flag| if flag { 1.0 } else { 0.0 })
        .or_else(|| value.as_f64().filter(|number| *number == 0.0 || *number == 1.0))
}

fn percentile(sorted: &[i64], percent: usize) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (percent * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

/// What the counterfactual section estimates (D-091 F-3).
const ESTIMAND: &str = "the mean reward of always taking one action and target, over the explored decisions only \
                        (the contexts exploration covered); each propensity is the recorded probability the \
                        selector gave that candidate given exploration";

/// One explored decision with a known outcome, for the counterfactual.
struct Explored {
    action: String,
    target: String,
    reward: f64,
    /// The recorded propensity of each eligible (action, target), summed
    /// over candidates; `None` when the decision did not record them.
    propensities: Option<BTreeMap<(String, String), f64>>,
    shadow_action: Option<String>,
}

impl Explored {
    /// The propensity of `(action, target)` in this decision; 0 when no
    /// eligible candidate had it.
    fn propensity(&self, action: &str, target: &str) -> f64 {
        self.propensities
            .as_ref()
            .and_then(|propensities| propensities.get(&(action.to_string(), target.to_string())))
            .copied()
            .unwrap_or(0.0)
    }

    /// The propensity of `action`, over every target.
    fn action_propensity(&self, action: &str) -> f64 {
        self.propensities.as_ref().map_or(0.0, |propensities| {
            propensities
                .iter()
                .filter(|((candidate, _), _)| candidate == action)
                .map(|(_, propensity)| propensity)
                .sum()
        })
    }
}

/// Whether `propensity` supports an estimate (a NaN never does).
fn supported(propensity: f64) -> bool {
    propensity
        .partial_cmp(&MIN_PROPENSITY)
        .is_some_and(std::cmp::Ordering::is_ge)
}

/// The effective sample size of `weights`.
fn ess(weights: impl Iterator<Item = f64>) -> f64 {
    let (sum, squares) = weights.fold((0.0, 0.0), |(sum, squares), weight| {
        (sum + weight, weight.mul_add(weight, squares))
    });
    if squares > 0.0 { sum * sum / squares } else { 0.0 }
}

/// IPS and doubly robust values of always taking `(action, target)`, with
/// the direct model `direct`.
// Plain `a * b + c` is kept on purpose: `mul_add` changes the rounding of the estimates.
#[allow(clippy::suboptimal_flops)]
fn doubly_robust(sample: &[&Explored], action: &str, target: &str, direct: f64) -> (f64, f64) {
    let n = as_f64(sample.len() as u64);
    let (mut ips, mut dr) = (0.0, 0.0);
    for decision in sample {
        let weight = if decision.action == action && decision.target == target {
            1.0 / decision.propensity(action, target)
        } else {
            0.0
        };
        ips += weight * decision.reward;
        dr += direct + weight * (decision.reward - direct);
    }
    (ips / n, dr / n)
}

fn counterfactual(explored: &[Explored], report_id: uuid::Uuid) -> Availability<Vec<PolicyEstimate>> {
    if explored.is_empty() {
        return Availability::Unavailable {
            reason: "no exploration data",
        };
    }
    if explored.iter().any(|decision| decision.propensities.is_none()) {
        return Availability::Unavailable {
            reason: "an explored decision did not record the propensity of every eligible candidate",
        };
    }
    let mut groups: BTreeMap<(&str, &str), Vec<&Explored>> = BTreeMap::new();
    for decision in explored {
        groups
            .entry((decision.action.as_str(), decision.target.as_str()))
            .or_default()
            .push(decision);
    }
    let everyone: Vec<&Explored> = explored.iter().collect();
    let mut estimates = Vec::new();
    for ((action, target), taken) in &groups {
        // Positivity over every explored context, not only those that took
        // it: a context that gave it no chance has propensity 0.
        let min_propensity = explored
            .iter()
            .map(|decision| decision.propensity(action, target))
            .fold(f64::INFINITY, f64::min);
        let ess = ess(taken.iter().map(|decision| 1.0 / decision.propensity(action, target)));
        if !supported(min_propensity) || ess < MIN_ESS {
            return Availability::Unavailable {
                reason: "insufficient overlap: a compared action has a propensity under 0.01 in some explored \
                         context, or an effective sample size under 100",
            };
        }
        let direct = taken.iter().map(|decision| decision.reward).sum::<f64>() / as_f64(taken.len() as u64);
        let (ips, dr) = doubly_robust(&everyone, action, target, direct);
        estimates.push(PolicyEstimate {
            action: (*action).to_string(),
            target: (*target).to_string(),
            taken: taken.len() as u64,
            min_propensity,
            ess,
            ips,
            doubly_robust: dr,
            ci95: bootstrap(&everyone, action, target, report_id),
        });
    }
    Availability::Available { value: estimates }
}

/// The 95% bootstrap interval of the doubly robust estimate, over
/// [`BOOTSTRAP_RESAMPLES`] resamples seeded by the report id.
fn bootstrap(sample: &[&Explored], action: &str, target: &str, report_id: uuid::Uuid) -> (f64, f64) {
    let (high, low) = report_id.as_u64_pair();
    let mut rng = rand::rngs::StdRng::seed_from_u64(high ^ low);
    let mut estimates = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    let mut resample: Vec<&Explored> = Vec::with_capacity(sample.len());
    for _ in 0..BOOTSTRAP_RESAMPLES {
        resample.clear();
        for _ in 0..sample.len() {
            if let Some(decision) = sample.get(rng.random_range(0..sample.len())) {
                resample.push(decision);
            }
        }
        let taken: Vec<f64> = resample
            .iter()
            .filter(|decision| decision.action == action && decision.target == target)
            .map(|decision| decision.reward)
            .collect();
        let direct = if taken.is_empty() {
            0.0
        } else {
            taken.iter().sum::<f64>() / as_f64(taken.len() as u64)
        };
        estimates.push(doubly_robust(&resample, action, target, direct).1);
    }
    estimates.sort_by(f64::total_cmp);
    let at = |quantile: f64| {
        let index = (as_f64((estimates.len() - 1) as u64) * quantile).round() as usize;
        estimates.get(index).copied().unwrap_or(f64::NAN)
    };
    (at(0.025), at(0.975))
}

/// The shadow's regret at action level, since the shadow records an action
/// only (D-091 F-4): the doubly robust value of taking the shadow's action
/// in each explored context, minus the observed mean reward. The propensity
/// of an action is the sum over its targets; the direct model is the mean
/// reward of the explored decisions that took the action.
// Plain `a * b + c` is kept on purpose: `mul_add` changes the rounding of the estimates.
#[allow(clippy::suboptimal_flops)]
fn shadow_regret(explored: &[Explored]) -> Availability<f64> {
    let with_shadow: Vec<&Explored> = explored
        .iter()
        .filter(|decision| decision.shadow_action.is_some())
        .collect();
    if with_shadow.is_empty() {
        return Availability::Unavailable {
            reason: "no explored decision recorded a shadow choice; the shadow's path was not executed",
        };
    }
    if with_shadow.iter().any(|decision| decision.propensities.is_none()) {
        return Availability::Unavailable {
            reason: "an explored decision did not record the propensity of every eligible candidate",
        };
    }
    let mut direct: BTreeMap<&str, (f64, u64)> = BTreeMap::new();
    for decision in explored {
        let entry = direct.entry(decision.action.as_str()).or_default();
        entry.0 += decision.reward;
        entry.1 += 1;
    }
    let mut value = 0.0;
    let mut weights = Vec::with_capacity(with_shadow.len());
    let mut min_propensity = f64::INFINITY;
    for decision in &with_shadow {
        let shadow = decision.shadow_action.as_deref().unwrap_or_default();
        let Some(&(sum, count)) = direct.get(shadow) else {
            return Availability::Unavailable {
                reason: "a shadow action was never explored",
            };
        };
        let direct = sum / as_f64(count);
        let propensity = decision.action_propensity(shadow);
        min_propensity = min_propensity.min(propensity);
        let weight = if decision.action == shadow {
            1.0 / propensity
        } else {
            0.0
        };
        if weight > 0.0 {
            weights.push(weight);
        }
        value += direct + weight * (decision.reward - direct);
    }
    if !supported(min_propensity) || ess(weights.into_iter()) < MIN_ESS {
        return Availability::Unavailable {
            reason: "insufficient overlap for the shadow's actions",
        };
    }
    let n = as_f64(with_shadow.len() as u64);
    let observed = with_shadow.iter().map(|decision| decision.reward).sum::<f64>() / n;
    Availability::Available {
        value: value / n - observed,
    }
}

/// The smallest candidate `threshold` such that the samples at or above it
/// all together reach [`TARGET_AGREEMENT`]; `samples` are (value, hit).
fn lowest_threshold(samples: &[(f64, bool)], candidates: impl Iterator<Item = f64>) -> Option<f64> {
    candidates
        .filter(|threshold| {
            let above: Vec<bool> = samples
                .iter()
                .filter(|(value, _)| *value >= *threshold)
                .map(|(_, hit)| *hit)
                .collect();
            let hits = above.iter().filter(|hit| **hit).count() as u64;
            !above.is_empty() && ratio(hits, above.len() as u64).is_some_and(|rate| rate >= TARGET_AGREEMENT)
        })
        .reduce(f64::min)
}

/// The calibration proposal of `decisions` (D-092): available only when
/// every decision ran on one backend kind, one endpoint (D-102 H-2) and one
/// pinned model, with enough successful, complete outcomes.
fn calibration(decisions: &[DecisionView]) -> Availability<CalibrationProposal> {
    let mut backends = decisions.iter().map(|decision| {
        (
            decision.backend_kind.as_str(),
            decision.backend_endpoint.as_deref(),
            decision.configured_model.as_str(),
        )
    });
    let Some((kind, endpoint, model)) = backends.next() else {
        return Availability::Unavailable {
            reason: "no decisions in the window",
        };
    };
    if backends.any(|other| other != (kind, endpoint, model)) {
        return Availability::Unavailable {
            reason: "the window ran on more than one backend, endpoint or model",
        };
    }
    let Some(endpoint) = endpoint else {
        return Availability::Unavailable {
            reason: "the decisions do not record the endpoint they were measured on",
        };
    };
    if crate::decision::model_pin::is_unpinned_model(model) {
        return Availability::Unavailable {
            reason: "the backend's model is not pinned to an exact version",
        };
    }
    let mut confidence = Vec::new();
    let mut margins = Vec::new();
    for decision in decisions {
        let Some(outcome) = complete_outcome(decision) else {
            continue;
        };
        let distribution = &decision.distributions.next_action;
        let Some((predicted, top)) = argmax(distribution) else {
            continue;
        };
        let success = is_success(&outcome.classification);
        if let (true, Some(observed)) = (success, outcome.executed_action.as_deref()) {
            confidence.push((top, predicted == observed));
        }
        let mut sorted: Vec<f64> = distribution.values().copied().collect();
        sorted.sort_by(|a, b| b.total_cmp(a));
        margins.push((top - sorted.get(1).copied().unwrap_or(0.0), success));
    }
    if (confidence.len() as u64) < MIN_CALIBRATION_DECISIONS {
        return Availability::Unavailable {
            reason: "fewer than 100 successful decisions with a next-action prediction",
        };
    }
    let bins = (0..ECE_BINS).map(|bin| as_f64(bin as u64) / as_f64(ECE_BINS as u64));
    let Some(minimum_choice_confidence) = lowest_threshold(&confidence, bins) else {
        return Availability::Unavailable {
            reason: "no confidence level reaches 90% agreement",
        };
    };
    let steps = (0..=10).map(|step| as_f64(step) * 0.05);
    let Some(minimum_top_margin) = lowest_threshold(&margins, steps) else {
        return Availability::Unavailable {
            reason: "no margin reaches a 90% success rate",
        };
    };
    Availability::Available {
        value: CalibrationProposal {
            profile: format!("{kind}:{model}"),
            endpoint: endpoint.to_string(),
            decisions: confidence.len() as u64,
            minimum_choice_confidence,
            minimum_top_margin,
        },
    }
}

/// The report of `rows`.
fn report(
    rows: &EvaluationRows,
    scope: ReportScope,
    store_schema: i64,
    options: &EvaluationOptions,
) -> EvaluationReport {
    let decisions = &rows.decisions;
    let mut coverage = Coverage {
        decisions: decisions.len() as u64,
        legacy_excluded: rows.legacy_excluded,
        ..Coverage::default()
    };
    let mut rates = OutcomeRates::default();
    let mut bytes = ByteTotals::default();
    let mut narrowing = Narrowing {
        denied_tool_attempts: 0,
        outcomes_with_denials: 0,
        capability_recall: Availability::Unavailable {
            reason: "capability sets are recorded with outcomes from CDP-22",
        },
    };
    let (mut successes, mut fallbacks, mut retries, mut cost) = (0_u64, 0_u64, 0_u64, None::<i64>);
    let mut margin = [(0.0, 0.1), (0.1, 0.2), (0.2, 0.4), (0.4, 1.0)].map(|(from, to)| MarginBucket {
        from,
        to,
        decisions: 0,
        successes: 0,
    });
    let mut shadow_agree = (0_u64, 0_u64);
    let mut shadow_disagree = (0_u64, 0_u64);
    let mut features: BTreeMap<String, CalibrationBins> = BTreeMap::new();
    let mut explored = Vec::new();
    let mut action_pairs = Vec::new();
    let mut target_pairs = Vec::new();
    for decision in decisions {
        if decision.outcomes.is_empty() {
            coverage.without_outcome += 1;
        }
        let Some(outcome) = complete_outcome(decision) else {
            if !decision.outcomes.is_empty() {
                coverage.incomplete_outcomes += 1;
            }
            continue;
        };
        coverage.complete_outcomes += 1;
        rates.outcomes += 1;
        let success = is_success(&outcome.classification);
        successes += u64::from(success);
        fallbacks += u64::from(outcome.classification == "fallback_success");
        retries += u64::from(outcome.provider_attempts > 1);
        if let Some(micro_usd) = outcome.cost_micro_usd {
            cost = Some(cost.unwrap_or(0).saturating_add(micro_usd));
        }
        if let (Some(a), Some(b), Some(c), Some(d)) = (
            outcome.prompt_bytes_before,
            outcome.prompt_bytes_after,
            outcome.tool_bytes_before,
            outcome.tool_bytes_after,
        ) {
            bytes.outcomes += 1;
            bytes.prompt_before += a;
            bytes.prompt_after += b;
            bytes.tool_before += c;
            bytes.tool_after += d;
        }
        if let Some(denied) = outcome.denied_tool_attempts.filter(|denied| *denied > 0) {
            narrowing.denied_tool_attempts += denied;
            narrowing.outcomes_with_denials += 1;
        }
        let next_action = &decision.distributions.next_action;
        if let (Some(observed), true) = (outcome.executed_action.as_deref(), success) {
            action_pairs.push((next_action, observed));
        }
        if let (Some(observed), true) = (outcome.executed_target.as_deref(), success) {
            target_pairs.push((&decision.distributions.capability_target, observed));
        }
        let mut sorted: Vec<f64> = next_action.values().copied().collect();
        sorted.sort_by(|a, b| b.total_cmp(a));
        if let Some(top) = sorted.first() {
            let gap = top - sorted.get(1).copied().unwrap_or(0.0);
            let position = margin
                .iter()
                .position(|bucket| gap < bucket.to)
                .unwrap_or(margin.len() - 1);
            if let Some(bucket) = margin.get_mut(position) {
                bucket.decisions += 1;
                bucket.successes += u64::from(success);
            }
        }
        for (feature, probability) in &decision.distributions.features {
            if let Some(truth) = observed_feature(outcome, feature) {
                features
                    .entry(feature.clone())
                    .or_default()
                    .add(*probability, truth, (probability - truth).powi(2));
            }
        }
        let selected = decision.selected();
        if let (Some(shadow), Some(selected)) = (decision.builtin_shadow.next_action.as_deref(), selected) {
            let tally = if shadow == selected.action {
                &mut shadow_agree
            } else {
                &mut shadow_disagree
            };
            tally.0 += 1;
            tally.1 += u64::from(success);
        }
        if let (true, Some(selected)) = (decision.explored, selected) {
            let recorded = decision
                .candidates
                .iter()
                .all(|candidate| candidate.propensity.is_some());
            let propensities = recorded.then(|| {
                let mut propensities: BTreeMap<(String, String), f64> = BTreeMap::new();
                for candidate in &decision.candidates {
                    *propensities
                        .entry((candidate.action.clone(), candidate.target.clone()))
                        .or_default() += candidate.propensity.unwrap_or(0.0);
                }
                propensities
            });
            explored.push(Explored {
                action: selected.action.clone(),
                target: selected.target.clone(),
                reward: reward_of(outcome),
                propensities,
                shadow_action: decision.builtin_shadow.next_action.clone(),
            });
        }
    }
    coverage.coverage = ratio(coverage.complete_outcomes, coverage.decisions);
    rates.success_rate = ratio(successes, rates.outcomes);
    rates.fallback_rate = ratio(fallbacks, rates.outcomes);
    rates.retry_rate = ratio(retries, rates.outcomes);
    let (next_action, next_action_calibration) = class_metrics(action_pairs.into_iter());
    let (capability_target, capability_target_calibration) = class_metrics(target_pairs.into_iter());
    let mut latencies: Vec<i64> = decisions.iter().map(|decision| decision.latency_ms).collect();
    latencies.sort_unstable();
    let total = decisions.len() as u64;
    let counterfactual = counterfactual(&explored, options.report_id);
    EvaluationReport {
        report_version: REPORT_VERSION,
        report_id: options.report_id.to_string(),
        created_at_ms: options.created_at_ms,
        store_schema,
        policy_versions: decisions
            .iter()
            .map(|decision| decision.policy_version.clone())
            .collect(),
        scorer_versions: decisions
            .iter()
            .map(|decision| decision.scorer_version.clone())
            .collect(),
        builtin_version: crate::decision::builtin::weights::MODEL_VERSION,
        scope,
        accuracy_meaning: ACCURACY_MEANING,
        coverage,
        next_action,
        capability_target,
        next_action_calibration,
        capability_target_calibration,
        feature_calibration: features
            .into_iter()
            .filter_map(|(feature, bins)| bins.finish().map(|calibration| (feature, calibration)))
            .collect(),
        margin: margin.to_vec(),
        shadow: ShadowComparison {
            compared: shadow_agree.0 + shadow_disagree.0,
            disagreements: shadow_disagree.0,
            success_rate_when_agreeing: ratio(shadow_agree.1, shadow_agree.0),
            success_rate_when_disagreeing: ratio(shadow_disagree.1, shadow_disagree.0),
            regret: shadow_regret(&explored),
            fallback_regret: Availability::Unavailable {
                reason: "the fallback path's outcome is observed only when the primary fails; no exploration \
                         covers it",
            },
        },
        narrowing,
        rates,
        bytes,
        latency_ms: Latency {
            p50: percentile(&latencies, 50),
            p95: percentile(&latencies, 95),
            p99: percentile(&latencies, 99),
        },
        backend: BackendRates {
            failure_rate: ratio(
                decisions
                    .iter()
                    .filter(|decision| decision.error_class.is_some())
                    .count() as u64,
                total,
            ),
            circuit_rate: ratio(
                decisions
                    .iter()
                    .filter(|decision| {
                        decision
                            .attempts
                            .iter()
                            .any(|attempt| attempt.error_class.as_deref() == Some("circuit_open"))
                    })
                    .count() as u64,
                total,
            ),
            builtin_rate: ratio(
                decisions
                    .iter()
                    .filter(|decision| decision.backend_kind == "builtin")
                    .count() as u64,
                total,
            ),
        },
        cost_per_successful_turn_micro_usd: cost
            .and_then(|cost| u64::try_from(cost).ok())
            .and_then(|cost| ratio(cost, successes)),
        calibration: calibration(decisions),
        counterfactual_estimand: ESTIMAND,
        counterfactual,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::causal_tree::ids::DecisionId;
    use crate::causal_tree::store::testing::{OutcomeFixture, open_private, record, write_outcome};
    use crate::causal_tree::store::{CausalDecisionStore, RecordedCandidate, RecordedDistributions, RecordedShadow};
    use crate::decision::telemetry::testing::RecordingSink;

    const WS: &str = "ws";

    fn id(n: u32) -> DecisionId {
        let mut digest = [0_u8; 32];
        digest[..4].copy_from_slice(&n.to_be_bytes());
        DecisionId::legacy(&digest)
    }

    /// The local operator's scope of `store`.
    fn scope(store: &CausalDecisionStore) -> DecisionScope {
        DecisionScope::LocalOperator {
            workspace_id: store.workspace_id().expect("test: id"),
        }
    }

    const ALL: TimeWindow = TimeWindow {
        from_ms: 0,
        to_ms: i64::MAX,
    };

    fn options() -> EvaluationOptions {
        EvaluationOptions {
            report_id: uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            created_at_ms: 1,
        }
    }

    fn distribution(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(class, p)| ((*class).to_string(), *p)).collect()
    }

    async fn flushed(store: &CausalDecisionStore) {
        store
            .writer()
            .unwrap()
            .drain(Instant::now() + Duration::from_secs(10))
            .await;
    }

    /// V8.05: accuracy, the multiclass Brier score and the 10-bin ECE match
    /// a hand-computed reference, for Choice and for a Noul feature.
    #[tokio::test]
    async fn ece_and_brier_on_fixture_match_reference() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = open_private(temp.path(), Arc::new(RecordingSink::default()));
        // (answer, retrieve), observed, p(needs_tools), needs_tools observed.
        let fixture = [
            (0.8, 0.2, "answer", 0.9, true),
            (0.6, 0.4, "retrieve", 0.2, false),
            (0.3, 0.7, "retrieve", 0.6, false),
            (0.9, 0.1, "answer", 0.7, true),
        ];
        let writer = store.writer().unwrap();
        for (n, (answer, retrieve, _, feature, _)) in fixture.iter().enumerate() {
            let mut decision = record(id(n as u32), WS, Some("a"), 10);
            decision.distributions = RecordedDistributions {
                next_action: distribution(&[("answer", *answer), ("retrieve", *retrieve)]),
                features: distribution(&[("needs_tools", *feature)]),
                ..RecordedDistributions::default()
            };
            writer.record_decision(decision);
        }
        flushed(&store).await;
        for (n, (_, _, observed, _, truth)) in fixture.iter().enumerate() {
            write_outcome(
                temp.path(),
                id(n as u32),
                &OutcomeFixture {
                    executed_action: Some(observed),
                    reward: Some(serde_json::json!({"needs_tools": truth})),
                    ..OutcomeFixture::default()
                },
            );
        }
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert_eq!(report.next_action.compared, 4);
        assert!((report.next_action.accuracy.unwrap() - 0.75).abs() < 1e-12);
        let choice = report.next_action_calibration.unwrap();
        assert!((choice.brier - 0.25).abs() < 1e-12, "{choice:?}");
        assert!((choice.ece - 0.3).abs() < 1e-12, "{choice:?}");
        let feature = report.feature_calibration["needs_tools"];
        assert!((feature.brier - 0.125).abs() < 1e-12, "{feature:?}");
        assert!((feature.ece - 0.3).abs() < 1e-12, "{feature:?}");
        assert_eq!(report.next_action.confusion["answer"]["retrieve"], 1);
        assert_eq!(report.next_action.precision["answer"], Some(2.0 / 3.0));
        assert_eq!(report.next_action.recall["retrieve"], Some(0.5));
        assert_eq!(report.accuracy_meaning, ACCURACY_MEANING);
        assert_eq!(report.coverage.coverage, Some(1.0));
    }

    /// An explored decision that chose `chosen` among `candidates` (each an
    /// (action, target) with its recorded propensity), with `reward`.
    fn explored_among(chosen: (&str, &str), candidates: &[((&str, &str), f64)], reward: f64) -> (DecisionRecord, f64) {
        let mut decision = record(DecisionId::legacy(&[0; 32]), WS, Some("a"), 10);
        decision.candidates = candidates
            .iter()
            .enumerate()
            .map(|(index, ((action, target), propensity))| RecordedCandidate {
                id: format!("c{index}"),
                action: (*action).to_string(),
                target: (*target).to_string(),
                propensity: Some(*propensity),
            })
            .collect();
        let chosen = candidates
            .iter()
            .position(|(candidate, _)| *candidate == chosen)
            .expect("test: chosen is a candidate");
        decision.selected_candidate_id = Some(format!("c{chosen}"));
        decision.explored = true;
        decision.propensity = candidates[chosen].1;
        decision.builtin_shadow = RecordedShadow {
            next_action: Some("retrieve".to_string()),
            capability_target: None,
        };
        (decision, reward)
    }

    /// An explored choice between answering and retrieving: the chosen one
    /// had `propensity`, the other the rest.
    fn explored(action: &str, target: &str, propensity: f64, reward: f64) -> (DecisionRecord, f64) {
        let other = if action == "answer" {
            ("retrieve", "memory")
        } else {
            ("answer", "none")
        };
        explored_among(
            (action, target),
            &[((action, target), propensity), (other, 1.0 - propensity)],
            reward,
        )
    }

    use super::super::store::DecisionRecord;

    async fn store_with(temp: &tempfile::TempDir, rows: Vec<(DecisionRecord, f64)>) -> CausalDecisionStore {
        let store = open_private(temp.path(), Arc::new(RecordingSink::default()));
        let writer = store.writer().unwrap();
        for (n, (mut decision, _)) in rows.iter().cloned().enumerate() {
            decision.decision_id = id(n as u32);
            writer.record_decision(decision);
        }
        flushed(&store).await;
        for (n, (_, reward)) in rows.iter().enumerate() {
            write_outcome(
                temp.path(),
                id(n as u32),
                &OutcomeFixture {
                    reward: Some(serde_json::json!({ "reward": reward })),
                    ..OutcomeFixture::default()
                },
            );
        }
        store
    }

    /// V8.05: with known propensities the IPS and doubly robust estimates
    /// and the effective sample size match a hand-computed reference.
    #[tokio::test]
    async fn ips_dr_with_known_propensities_match_reference_and_report_ess() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut rows = Vec::new();
        rows.extend((0..100).map(|_| explored("answer", "none", 0.5, 1.0)));
        rows.extend((0..50).map(|_| explored("answer", "none", 0.25, 0.0)));
        rows.extend((0..150).map(|_| explored("retrieve", "memory", 0.5, 0.5)));
        let store = store_with(&temp, rows).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        let Availability::Available { value: estimates } = &report.counterfactual else {
            panic!("expected an estimate: {:?}", report.counterfactual);
        };
        let answer = estimates.iter().find(|estimate| estimate.action == "answer").unwrap();
        // IPS = (100·2·1 + 50·4·0) / 300; q̂ = 100/150;
        // DR = q̂ + (100·2·(1 − q̂) + 50·4·(0 − q̂)) / 300; ESS = 400² / (100·4 + 50·16).
        assert!((answer.ips - 2.0 / 3.0).abs() < 1e-12, "{answer:?}");
        assert!((answer.doubly_robust - 4.0 / 9.0).abs() < 1e-12, "{answer:?}");
        assert!((answer.ess - 400.0 / 3.0).abs() < 1e-9, "{answer:?}");
        assert_eq!((answer.taken, answer.min_propensity), (150, 0.25));
        assert!(answer.ci95.0 <= answer.doubly_robust && answer.doubly_robust <= answer.ci95.1);
        let retrieve = estimates.iter().find(|estimate| estimate.action == "retrieve").unwrap();
        assert!((retrieve.ips - 0.5).abs() < 1e-12 && (retrieve.doubly_robust - 0.5).abs() < 1e-12);
        assert!((retrieve.ess - 150.0).abs() < 1e-9);
        // The same report id gives the same interval.
        let again = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert_eq!(again.counterfactual, report.counterfactual);
        assert!(matches!(report.shadow.regret, Availability::Available { .. }));
    }

    /// V8.05: without exploration, or with too little overlap, the
    /// counterfactual section says unavailable.
    #[tokio::test]
    async fn evaluate_report_labels_counterfactual_unavailable_without_overlap() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = store_with(&temp, vec![(record(id(0), WS, Some("a"), 10), 1.0)]).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert_eq!(
            report.counterfactual,
            Availability::Unavailable {
                reason: "no exploration data"
            }
        );
        assert!(matches!(report.shadow.regret, Availability::Unavailable { .. }));

        let few = tempfile::TempDir::new().unwrap();
        let rows = (0..40).map(|_| explored("answer", "none", 0.5, 1.0)).collect();
        let store = store_with(&few, rows).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert!(
            matches!(report.counterfactual, Availability::Unavailable { reason } if reason.starts_with("insufficient overlap")),
            "an effective sample size of 40 is not enough"
        );

        let rare = tempfile::TempDir::new().unwrap();
        let mut rows: Vec<_> = (0..300).map(|_| explored("answer", "none", 0.5, 1.0)).collect();
        rows.push(explored("retrieve", "memory", 0.005, 1.0));
        let store = store_with(&rare, rows).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert!(
            matches!(report.counterfactual, Availability::Unavailable { .. }),
            "a propensity under 0.01"
        );

        // Enough effective samples, but every propensity is under 0.01.
        let small = tempfile::TempDir::new().unwrap();
        let rows = (0..150).map(|_| explored("answer", "none", 0.009, 1.0)).collect();
        let store = store_with(&small, rows).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert!(
            matches!(report.counterfactual, Availability::Unavailable { .. }),
            "an effective sample size of 150 with propensities of 0.009 is not supported"
        );
        let json = serde_json::to_value(&report.counterfactual).unwrap();
        assert_eq!(json["status"], "unavailable");
    }

    /// Coverage counts missing and incomplete outcomes; legacy rows are
    /// left out; the operational figures come from the recorded columns.
    #[tokio::test]
    async fn coverage_rates_latency_and_cost() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = open_private(temp.path(), Arc::new(RecordingSink::default()));
        let writer = store.writer().unwrap();
        for n in 0..4 {
            let mut decision = record(id(n), WS, Some("a"), 10);
            decision.latency_ms = u64::from(n + 1) * 10;
            if n == 3 {
                decision.error_class = Some("timeout".to_string());
            }
            writer.record_decision(decision);
        }
        let mut legacy = record(id(9), WS, None, 10);
        legacy.legacy_v1 = true;
        writer.record_decision(legacy);
        flushed(&store).await;
        write_outcome(
            temp.path(),
            id(0),
            &OutcomeFixture {
                cost_micro_usd: Some(300),
                provider_attempts: 2,
                bytes: Some((1000, 400, 800, 200)),
                denied_tool_attempts: Some(2),
                ..OutcomeFixture::default()
            },
        );
        write_outcome(
            temp.path(),
            id(1),
            &OutcomeFixture {
                classification: "fallback_success",
                cost_micro_usd: Some(100),
                ..OutcomeFixture::default()
            },
        );
        write_outcome(
            temp.path(),
            id(2),
            &OutcomeFixture {
                completeness: "partial",
                ..OutcomeFixture::default()
            },
        );
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        let coverage = report.coverage;
        assert_eq!(
            (
                coverage.decisions,
                coverage.complete_outcomes,
                coverage.incomplete_outcomes
            ),
            (4, 2, 1)
        );
        assert_eq!((coverage.without_outcome, coverage.legacy_excluded), (1, 1));
        assert_eq!(coverage.coverage, Some(0.5));
        assert_eq!(report.rates.fallback_rate, Some(0.5));
        assert_eq!(report.rates.retry_rate, Some(0.5));
        assert_eq!(report.cost_per_successful_turn_micro_usd, Some(200.0));
        assert_eq!(report.bytes.prompt_after, 400);
        assert_eq!(report.narrowing.denied_tool_attempts, 2);
        assert_eq!(report.latency_ms.p50, Some(20));
        assert_eq!(report.latency_ms.p99, Some(40));
        assert_eq!(report.backend.failure_rate, Some(0.25));
        assert_eq!(report.backend.builtin_rate, Some(1.0));
    }

    /// V8.05 `runtime_never_rewrites_thresholds`: evaluating and writing a
    /// report leaves the configuration file and its history untouched; the
    /// report is private and never overwritten.
    #[tokio::test]
    async fn runtime_never_rewrites_thresholds() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(&config, "[causal_tree.policy]\ncommit_threshold = 0.61\n").unwrap();
        let history = temp.path().join("config-history");
        std::fs::create_dir_all(&history).unwrap();
        let before = (
            std::fs::read(&config).unwrap(),
            std::fs::read_dir(&history).unwrap().count(),
        );
        let store = store_with(&temp, vec![(record(id(0), WS, Some("a"), 10), 1.0)]).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        let path = write_report(temp.path(), &report).unwrap();
        assert_eq!(
            (
                std::fs::read(&config).unwrap(),
                std::fs::read_dir(&history).unwrap().count()
            ),
            before
        );
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["report_version"], REPORT_VERSION);
        assert_eq!(written["builtin_version"], "builtin-v1");
        assert!(
            write_report(temp.path(), &report).is_err(),
            "a report is never overwritten"
        );
    }

    /// D-091 F-3: positivity is checked over every explored context: a
    /// context that gave a compared action no chance makes the estimate
    /// unavailable, however often it was taken elsewhere.
    #[tokio::test]
    async fn a_context_without_a_compared_action_blocks_the_estimate() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut rows: Vec<_> = (0..200).map(|_| explored("answer", "none", 0.5, 1.0)).collect();
        rows.extend((0..200).map(|_| explored("retrieve", "memory", 0.5, 0.0)));
        // One explored context offered only answering.
        rows.push(explored_among(("answer", "none"), &[(("answer", "none"), 1.0)], 1.0));
        let store = store_with(&temp, rows).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert!(
            matches!(report.counterfactual, Availability::Unavailable { reason } if reason.starts_with("insufficient overlap")),
            "{:?}",
            report.counterfactual
        );
        assert!(report.counterfactual_estimand.contains("explored decisions only"));

        // Without the propensities of every eligible candidate there is no
        // estimate at all.
        let unrecorded = tempfile::TempDir::new().unwrap();
        let mut row = explored("answer", "none", 0.5, 1.0);
        row.0.candidates[1].propensity = None;
        let store = store_with(&unrecorded, vec![row]).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert!(matches!(report.counterfactual, Availability::Unavailable { .. }));
        assert!(matches!(
            report.shadow.fallback_regret,
            Availability::Unavailable { .. }
        ));
    }

    /// D-091 F-4: the shadow's regret is evaluated at action level, the
    /// action's propensity summing its targets; a hand-computed reference.
    #[tokio::test]
    async fn shadow_regret_is_action_level_and_matches_reference() {
        let temp = tempfile::TempDir::new().unwrap();
        let spread = |answer_none: f64, answer_web: f64| {
            [
                (("answer", "none"), answer_none),
                (("answer", "web"), answer_web),
                (("retrieve", "memory"), 1.0 - answer_none - answer_web),
            ]
        };
        let mut rows = Vec::new();
        rows.extend((0..120).map(|_| explored_among(("answer", "none"), &spread(0.25, 0.25), 1.0)));
        rows.extend((0..80).map(|_| explored_among(("answer", "none"), &spread(0.4, 0.4), 0.0)));
        rows.extend((0..200).map(|_| explored_among(("retrieve", "memory"), &spread(0.25, 0.25), 0.5)));
        for row in &mut rows {
            row.0.builtin_shadow.next_action = Some("answer".to_string());
        }
        let store = store_with(&temp, rows).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        // q̂(answer) = 120/200 = 0.6; weights 1/0.5 and 1/0.8 on the
        // decisions that answered; V = (120·1.4 + 80·(-0.15) + 200·0.6)/400
        // = 0.69; the observed mean is 220/400 = 0.55.
        let Availability::Available { value } = report.shadow.regret else {
            panic!("expected a regret: {:?}", report.shadow.regret);
        };
        assert!((value - 0.14).abs() < 1e-12, "{value}");
    }

    /// D-092: a window on one pinned backend proposes the lowest thresholds
    /// that reach 90%; a mixed or unpinned window proposes nothing.
    #[tokio::test]
    async fn the_calibration_is_proposed_only_for_one_pinned_backend() {
        let fill = |temp: &tempfile::TempDir, model: &'static str| {
            let temp = temp.path().to_path_buf();
            async move {
                let store = open_private(&temp, Arc::new(RecordingSink::default()));
                let writer = store.writer().unwrap();
                // 100 confident decisions that agree, 20 unsure ones that do
                // not: agreement reaches 90% from a confidence of 0.8.
                for n in 0..120_u32 {
                    let confident = n < 100;
                    let mut decision = record(id(n), WS, Some("a"), 10);
                    decision.backend_kind = "compatible_http".to_string();
                    decision.configured_model = model.to_string();
                    decision.backend_endpoint = Some("http://127.0.0.1:8900/v1".to_string());
                    decision.distributions = RecordedDistributions {
                        next_action: if confident {
                            distribution(&[("answer", 0.85), ("retrieve", 0.15)])
                        } else {
                            distribution(&[("answer", 0.55), ("retrieve", 0.45)])
                        },
                        ..RecordedDistributions::default()
                    };
                    writer.record_decision(decision);
                }
                flushed(&store).await;
                for n in 0..120_u32 {
                    write_outcome(
                        &temp,
                        id(n),
                        &OutcomeFixture {
                            executed_action: Some(if n < 100 { "answer" } else { "retrieve" }),
                            classification: if n < 100 { "success" } else { "failed" },
                            ..OutcomeFixture::default()
                        },
                    );
                }
                store
            }
        };
        let temp = tempfile::TempDir::new().unwrap();
        let store = fill(&temp, "decider-2026-09").await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        let Availability::Available { value } = &report.calibration else {
            panic!("expected a proposal: {:?}", report.calibration);
        };
        assert_eq!(value.profile, "compatible_http:decider-2026-09");
        assert_eq!(value.endpoint, "http://127.0.0.1:8900/v1");
        assert_eq!(value.decisions, 100);
        assert!((value.minimum_choice_confidence - 0.0).abs() < 1e-12, "{value:?}");
        assert!((value.minimum_top_margin - 0.15).abs() < 1e-9, "{value:?}");

        // A second backend in the window: no proposal.
        let mixed = tempfile::TempDir::new().unwrap();
        let store = fill(&mixed, "decider-2026-09").await;
        let mut other = record(id(500), WS, Some("a"), 10);
        other.backend_kind = "builtin".to_string();
        store.writer().unwrap().record_decision(other);
        flushed(&store).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert!(
            matches!(report.calibration, Availability::Unavailable { reason } if reason.contains("more than one")),
            "{:?}",
            report.calibration
        );

        // D-102 H-2: the same kind and model on a second endpoint in the
        // window: no proposal.
        let moved = tempfile::TempDir::new().unwrap();
        let store = fill(&moved, "decider-2026-09").await;
        let mut other = record(id(501), WS, Some("a"), 10);
        other.backend_kind = "compatible_http".to_string();
        other.configured_model = "decider-2026-09".to_string();
        other.backend_endpoint = Some("http://127.0.0.2:8900/v1".to_string());
        store.writer().unwrap().record_decision(other);
        flushed(&store).await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert!(
            matches!(report.calibration, Availability::Unavailable { reason } if reason.contains("endpoint")),
            "{:?}",
            report.calibration
        );

        let unpinned = tempfile::TempDir::new().unwrap();
        let store = fill(&unpinned, "decider-latest").await;
        let report = evaluate(&store.reader(), &scope(&store), ALL, &options()).unwrap();
        assert!(matches!(report.calibration, Availability::Unavailable { reason } if reason.contains("not pinned")));
    }
}
