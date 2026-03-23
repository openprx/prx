use crate::self_system::evolution::analyzer::EvolutionCandidate;
use crate::self_system::evolution::config::{EvolutionConfig, EvolutionGateConfig};
use serde::{Deserialize, Serialize};

/// Gate runtime check inputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateMetrics {
    /// Average improvement ratio (e.g. 0.03 == +3%).
    pub average_improvement: f64,
    /// Holdout regression ratio, usually <= 0.0 when regressed.
    pub holdout_regression: f64,
    /// Token degradation ratio (e.g. 0.1 == +10%).
    pub token_degradation: f64,
}

/// Rejection details when gate blocks evolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateRejection {
    pub reason: String,
    pub details: String,
}

/// Gate decision output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "rejection")]
pub enum GateResult {
    Passed,
    Rejected(GateRejection),
}

/// Evolution gate based on `evolution_config.gate`.
#[derive(Debug, Clone)]
pub struct EvolutionGate {
    config: EvolutionGateConfig,
}

impl EvolutionGate {
    pub fn from_evolution_config(config: &EvolutionConfig) -> Self {
        Self {
            config: config.gate.clone(),
        }
    }

    pub const fn from_gate_config(config: EvolutionGateConfig) -> Self {
        Self { config }
    }

    pub fn evaluate_metrics(&self, metrics: &GateMetrics) -> GateResult {
        if metrics.average_improvement < self.config.min_improvement {
            return GateResult::Rejected(GateRejection {
                reason: "insufficient_improvement".to_string(),
                details: format!(
                    "average_improvement {:.4} < min_improvement {:.4}",
                    metrics.average_improvement, self.config.min_improvement
                ),
            });
        }

        if metrics.holdout_regression < self.config.max_regression {
            return GateResult::Rejected(GateRejection {
                reason: "holdout_regression_exceeded".to_string(),
                details: format!(
                    "holdout_regression {:.4} < max_regression {:.4}",
                    metrics.holdout_regression, self.config.max_regression
                ),
            });
        }

        if metrics.token_degradation > self.config.max_token_degradation {
            return GateResult::Rejected(GateRejection {
                reason: "token_degradation_exceeded".to_string(),
                details: format!(
                    "token_degradation {:.4} > max_token_degradation {:.4}",
                    metrics.token_degradation, self.config.max_token_degradation
                ),
            });
        }

        GateResult::Passed
    }

    /// Analyze -> Evolve guard checks.
    pub fn validate_candidate(&self, candidate: &EvolutionCandidate) -> GateResult {
        if candidate.evidence_ids.is_empty() {
            return GateResult::Rejected(GateRejection {
                reason: "missing_evidence".to_string(),
                details: "candidate.evidence_ids must be non-empty".to_string(),
            });
        }

        if candidate.target.len() != 1 {
            return GateResult::Rejected(GateRejection {
                reason: "invalid_target_scope".to_string(),
                details: format!(
                    "candidate.target must contain exactly one field, got {}",
                    candidate.target.len()
                ),
            });
        }

        if candidate.backfill_after_days == 0 {
            return GateResult::Rejected(GateRejection {
                reason: "missing_backfill_plan".to_string(),
                details: "backfill_after_days must be > 0".to_string(),
            });
        }

        GateResult::Passed
    }

    pub fn evaluate(&self, candidate: &EvolutionCandidate, metrics: &GateMetrics) -> GateResult {
        match self.validate_candidate(candidate) {
            GateResult::Passed => self.evaluate_metrics(metrics),
            rejected => rejected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::analyzer::{CandidatePriority, EvolutionCandidate};
    use std::collections::BTreeMap;

    fn sample_candidate() -> EvolutionCandidate {
        let mut target = BTreeMap::new();
        target.insert("task_type".to_string(), "tool_call".to_string());
        EvolutionCandidate {
            target,
            current_value: "failure_rate=0.4".to_string(),
            suggested_value: "increase_validation".to_string(),
            evidence_ids: vec!["trace-a".to_string()],
            priority: CandidatePriority::High,
            backfill_after_days: 3,
        }
    }

    #[test]
    fn metric_gate_passes_when_all_thresholds_met() {
        let gate = EvolutionGate::from_gate_config(EvolutionGateConfig::default());
        let result = gate.evaluate_metrics(&GateMetrics {
            average_improvement: 0.05,
            holdout_regression: -0.01,
            token_degradation: 0.05,
        });
        assert!(matches!(result, GateResult::Passed));
    }

    #[test]
    fn metric_gate_rejects_low_improvement() {
        let gate = EvolutionGate::from_gate_config(EvolutionGateConfig::default());
        let result = gate.evaluate_metrics(&GateMetrics {
            average_improvement: 0.01,
            holdout_regression: 0.0,
            token_degradation: 0.01,
        });
        assert!(matches!(
            result,
            GateResult::Rejected(GateRejection { reason, .. }) if reason == "insufficient_improvement"
        ));
    }

    #[test]
    fn candidate_guard_rejects_missing_evidence() {
        let gate = EvolutionGate::from_gate_config(EvolutionGateConfig::default());
        let mut candidate = sample_candidate();
        candidate.evidence_ids.clear();

        let result = gate.validate_candidate(&candidate);
        assert!(matches!(
            result,
            GateResult::Rejected(GateRejection { reason, .. }) if reason == "missing_evidence"
        ));
    }

    #[test]
    fn candidate_guard_rejects_multi_target() {
        let gate = EvolutionGate::from_gate_config(EvolutionGateConfig::default());
        let mut candidate = sample_candidate();
        candidate
            .target
            .insert("config_snapshot_hash".to_string(), "cfg-x".to_string());

        let result = gate.validate_candidate(&candidate);
        assert!(matches!(
            result,
            GateResult::Rejected(GateRejection { reason, .. }) if reason == "invalid_target_scope"
        ));
    }

    #[test]
    fn candidate_guard_rejects_missing_backfill_plan() {
        let gate = EvolutionGate::from_gate_config(EvolutionGateConfig::default());
        let mut candidate = sample_candidate();
        candidate.backfill_after_days = 0;

        let result = gate.validate_candidate(&candidate);
        assert!(matches!(
            result,
            GateResult::Rejected(GateRejection { reason, .. }) if reason == "missing_backfill_plan"
        ));
    }
}
