//! Content-free, owner-validated study evidence. Serializable report fields are
//! data, never verification authority or publication eligibility.
use super::{StudyArm, StudyManifest, StudyMetric, StudyTrialInput, digest};
use crate::skills::lifecycle::experiment::{ExperimentResources, ExperimentRunBinding};
use crate::skills::lifecycle::{OwnerScope, RunReceipt, ValidatorOutcome};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyPhysicalObservation {
    pub scope_id: String,
    pub attempt_ids: Vec<String>,
    pub calls: u64,
    pub settled_calls: u64,
    pub tokens: u64,
    pub micro_usd: u64,
    pub unresolved: bool,
    pub models: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyTrialReceipt {
    pub schema_version: u32,
    pub owner_scope: OwnerScope,
    pub input: StudyTrialInput,
    pub binding: ExperimentRunBinding,
    pub root_task_id: String,
    pub case_id: String,
    pub repetition: u32,
    pub run: RunReceipt,
    pub validators: Vec<ValidatorOutcome>,
    pub measured_and_valid: bool,
    pub invalid_reasons: Vec<String>,
    pub success: bool,
    pub quality_ppm: u32,
    pub physical: StudyPhysicalObservation,
    pub recorded_at: DateTime<Utc>,
}
impl StudyTrialReceipt {
    pub fn sha256(&self) -> Result<String> {
        digest(b"prx-v01-study-trial-v1\0", self)
    }
    pub const fn metric_ppm(&self, metric: StudyMetric) -> u32 {
        match metric {
            StudyMetric::Success => {
                if self.success {
                    1_000_000
                } else {
                    0
                }
            }
            StudyMetric::ArtifactQuality => self.quality_ppm,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudyAttemptState {
    NotStarted,
    ProvenUnsent,
    Reserved,
    Dispatched,
    Unknown,
    SettledCleanupUnknown,
    Settled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyTrialRow {
    pub stage: String,
    pub input: StudyTrialInput,
    pub root_task_id: String,
    pub case_id: String,
    pub state: StudyAttemptState,
    pub ever_reserved: bool,
    /// Only actual journal settlement, not estimated or missing usage.
    pub settled_usage: ExperimentResources,
    /// Unsettled retained allocation. Do not sum it into observed spend.
    pub retained_liability: ExperimentResources,
    /// The original ledger's independent projection, including partial/unknown
    /// physical work. It is not another charge or a substitute settlement.
    pub physical: StudyPhysicalObservation,
    pub receipt: Option<StudyTrialReceipt>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyArmSummary {
    pub declared: u32,
    pub reserved: u32,
    pub settled_and_clean: u32,
    pub valid: u32,
    pub successes: u32,
    pub failures: u32,
    pub invalid: u32,
    pub unknown: u32,
    pub not_started: u32,
    pub proven_unsent: u32,
    pub cancelled_runs: u32,
    pub elapsed_ms: u64,
    pub settled_usage: ExperimentResources,
    pub retained_liability: ExperimentResources,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyRootDifference {
    pub root_task_id: String,
    pub root_content_sha256: String,
    pub paired_trials: u32,
    pub baseline_mean: f64,
    pub h2_mean: f64,
    pub difference: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudyBenefitDecision {
    GainSupported,
    NoPracticalGain,
    Inconclusive,
    MechanismIncomplete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyAnalysis {
    pub primary_metric: StudyMetric,
    pub statistical_unit: String,
    pub independent_roots: u32,
    pub declared_pairs: u32,
    pub roots: Vec<StudyRootDifference>,
    pub difference: f64,
    pub interval_low: f64,
    pub interval_high: f64,
    pub interval_half_width: f64,
    pub critical_h2_failures: u32,
    pub task_success_non_regression: bool,
    pub environment_mismatches: u32,
    pub cost_latency_within_preregistered_ceilings: bool,
    pub mechanism_complete: bool,
    pub benefit: StudyBenefitDecision,
    pub retain_baseline_or_shadow: bool,
    /// Always false: a V01 study never qualifies an experiment or mints a gate.
    pub qualification: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyReport {
    pub schema_version: u32,
    pub study_sha256: String,
    pub experiment_sha256: String,
    pub journal_head_sha256: String,
    pub manifest: StudyManifest,
    pub cancelled: bool,
    pub admission_metadata_usage: ExperimentResources,
    pub rows: Vec<StudyTrialRow>,
    pub baseline: StudyArmSummary,
    pub h2: StudyArmSummary,
    pub analysis: StudyAnalysis,
    /// Costs of exactly these disclosed original optimizer roots, separate
    /// from the paired adopted-revision steady-state runs above.
    pub optimizer_settled_usage: ExperimentResources,
    pub optimizer_retained_liability: ExperimentResources,
    pub optimizer_scope: String,
    pub calibration_settled_usage: Option<ExperimentResources>,
    pub calibration_retained_liability: Option<ExperimentResources>,
    pub calibration_physical_attempt_ids: Vec<String>,
    /// Metadata/library preparation has no separately instrumented CPU/energy
    /// meter. Never report this absence as free computation.
    pub preparation_overhead_measurement: String,
    pub generated_at: DateTime<Utc>,
}
impl StudyReport {
    /// Stable evidence pin excludes only the report-generation wall clock;
    /// the original trial timestamps, journal head and all metrics remain.
    pub fn sha256(&self) -> Result<String> {
        let mut value = serde_json::to_value(self)?;
        if let Some(object) = value.as_object_mut() {
            object.remove("generated_at");
        }
        digest(b"prx-v01-validated-report-v1\0", &value)
    }
    pub fn arm(&self, arm: StudyArm) -> &StudyArmSummary {
        match arm {
            StudyArm::Baseline => &self.baseline,
            StudyArm::H2 => &self.h2,
        }
    }
}

/// Safe while the protocol is open: no task text, oracle, arm grades, model
/// output, per-arm success or treatment comparison leaks into status.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudyStatus {
    pub study_sha256: String,
    pub experiment_sha256: String,
    pub journal_head_sha256: String,
    pub cancelled: bool,
    pub protocol_closed: bool,
    pub declared_stages: u32,
    pub reserved_stages: u32,
    pub settled_and_clean_stages: u32,
    pub unknown_stages: u32,
    pub settled_usage: ExperimentResources,
    pub retained_liability: ExperimentResources,
    pub admission_metadata_usage: ExperimentResources,
    pub qualification: bool,
}
