use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::state::SideEffectMode;

/// Runtime policy governing the CTE pipeline.
///
/// All limits are enforced as hard caps — exceeding any limit triggers
/// immediate degradation to Router-direct mode.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CausalPolicy {
    /// Maximum number of candidate branches to expand.
    pub max_branches: usize,
    /// Minimum score required to commit a branch as the primary path.
    pub commit_threshold: f32,
    /// Maximum ratio of extra tokens (CTE overhead / baseline request tokens).
    pub extra_token_ratio_limit: f32,
    /// Maximum additional latency budget for the entire CTE pipeline (ms).
    pub extra_latency_budget_ms: u64,
    /// Timeout for a single rehearsal run (ms).
    pub rehearsal_timeout_ms: u64,
    /// Default side-effect mode for rehearsals.
    pub default_side_effect_mode: SideEffectMode,
    /// Number of consecutive failures before the circuit breaker opens.
    pub circuit_breaker_threshold: u32,
    /// Duration (seconds) the circuit breaker stays open before a retry.
    pub circuit_breaker_cooldown_secs: u64,
}

impl Default for CausalPolicy {
    fn default() -> Self {
        Self {
            max_branches: 3,
            commit_threshold: 0.62,
            extra_token_ratio_limit: 0.35,
            extra_latency_budget_ms: 300,
            rehearsal_timeout_ms: 5000,
            default_side_effect_mode: SideEffectMode::ReadOnly,
            circuit_breaker_threshold: 5,
            circuit_breaker_cooldown_secs: 60,
        }
    }
}

/// Top-level configuration section for the causal tree engine.
///
/// Deserialized from the `[causal_tree]` section in the PRX config file.
/// Uses `#[serde(default)]` so all fields are optional in the config.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CausalTreeConfig {
    /// Master switch — when `false`, the CTE is completely bypassed.
    #[serde(default)]
    pub enabled: bool,

    /// Policy parameters.
    #[serde(default)]
    pub policy: CausalPolicy,

    /// Weight for the *confidence* dimension in branch scoring.
    #[serde(default = "default_w_confidence")]
    pub w_confidence: f32,
    /// Weight for the *cost* dimension (penalty).
    #[serde(default = "default_w_cost")]
    pub w_cost: f32,
    /// Weight for the *latency* dimension (penalty).
    #[serde(default = "default_w_latency")]
    pub w_latency: f32,

    /// Enable decision-log output for every CTE run.
    #[serde(default = "default_true")]
    pub write_decision_log: bool,
    /// Enable metrics collection.
    #[serde(default = "default_true")]
    pub write_metrics: bool,
}

const fn default_w_confidence() -> f32 {
    0.50
}
const fn default_w_cost() -> f32 {
    0.25
}
const fn default_w_latency() -> f32 {
    0.25
}
const fn default_true() -> bool {
    true
}

impl Default for CausalTreeConfig {
    fn default() -> Self {
        Self {
            enabled: false, // default OFF — opt-in via config
            policy: CausalPolicy::default(),
            w_confidence: default_w_confidence(),
            w_cost: default_w_cost(),
            w_latency: default_w_latency(),
            write_decision_log: true,
            write_metrics: true,
        }
    }
}

/// Circuit breaker state — tracked at runtime, not serialized.
#[derive(Debug)]
pub struct CircuitBreakerState {
    pub consecutive_failures: u32,
    pub open_since: Option<std::time::Instant>,
}

impl Default for CircuitBreakerState {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            open_since: None,
        }
    }
}

impl CircuitBreakerState {
    /// Record a successful CTE run — resets the failure counter.
    pub const fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.open_since = None;
    }

    /// Record a failed CTE run — increments the failure counter.
    pub const fn record_failure(&mut self) {
        self.consecutive_failures += 1;
    }

    /// Returns `true` if the circuit breaker is currently open (tripped).
    pub fn is_open(&self, policy: &CausalPolicy) -> bool {
        if self.consecutive_failures < policy.circuit_breaker_threshold {
            return false;
        }
        // If open, check whether the cooldown has elapsed.
        self.open_since
            .map_or(true, |opened_at| opened_at.elapsed().as_secs() < policy.circuit_breaker_cooldown_secs)
    }

    /// Transition to the open state if the threshold has been reached.
    pub fn maybe_open(&mut self, policy: &CausalPolicy) {
        if self.consecutive_failures >= policy.circuit_breaker_threshold && self.open_since.is_none() {
            self.open_since = Some(std::time::Instant::now());
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_default_policy() {
        let p = CausalPolicy::default();
        assert_eq!(p.max_branches, 3);
        assert!((p.commit_threshold - 0.62).abs() < f32::EPSILON);
        assert_eq!(p.circuit_breaker_threshold, 5);
    }

    #[test]
    fn test_default_config_disabled() {
        let cfg = CausalTreeConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.w_confidence - 0.50).abs() < f32::EPSILON);
        assert!((cfg.w_cost - 0.25).abs() < f32::EPSILON);
        assert!((cfg.w_latency - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn test_circuit_breaker_lifecycle() {
        let policy = CausalPolicy {
            circuit_breaker_threshold: 3,
            circuit_breaker_cooldown_secs: 60,
            ..CausalPolicy::default()
        };
        let mut cb = CircuitBreakerState::default();

        // Not open initially
        assert!(!cb.is_open(&policy));

        // 2 failures — still closed
        cb.record_failure();
        cb.record_failure();
        assert!(!cb.is_open(&policy));

        // 3rd failure — threshold reached
        cb.record_failure();
        cb.maybe_open(&policy);
        assert!(cb.is_open(&policy));

        // Success resets
        cb.record_success();
        assert!(!cb.is_open(&policy));
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let cfg = CausalTreeConfig::default();
        let json = serde_json::to_string(&cfg).expect("test: serialize");
        let restored: CausalTreeConfig = serde_json::from_str(&json).expect("test: deserialize");
        assert!(!restored.enabled);
        assert_eq!(restored.policy.max_branches, 3);
    }
}
