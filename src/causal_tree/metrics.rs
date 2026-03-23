//! Metrics for the Causal Tree Engine.
//!
//! Tracks key performance indicators across CTE runs. First-version metrics
//! are simple counters / accumulators that can be periodically flushed to
//! a tracing span or an external metrics sink.

use serde::{Deserialize, Serialize};

/// Accumulated metrics for a batch of CTE runs.
///
/// All counters start at zero and are incremented by
/// [`CausalTreeMetrics::record`]. Ratios / averages are computed on read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CausalTreeMetrics {
    /// Total number of CTE runs.
    pub total_runs: u64,
    /// Runs where the first-ranked branch was ultimately used.
    pub hits_at_1: u64,
    /// Runs where the correct branch was within the top-3 candidates.
    pub hits_at_3: u64,
    /// Runs where no branch met the commit threshold.
    pub no_branch_qualified: u64,
    /// Total rehearsal runs that were performed but not used.
    pub wasted_rehearsals: u64,
    /// Total rehearsals performed.
    pub total_rehearsals: u64,
    /// Cumulative extra latency introduced by CTE (ms).
    pub cumulative_extra_latency_ms: u64,
    /// Cumulative extra tokens consumed by CTE.
    pub cumulative_extra_tokens: u64,
    /// Successful commit count.
    pub commit_successes: u64,
    /// Circuit breaker trip count.
    pub circuit_breaker_trips: u64,
}

/// A single CTE run observation used to update the metrics.
#[derive(Debug, Clone)]
pub struct RunObservation {
    /// Whether the first-ranked branch was ultimately correct.
    pub hit_at_1: bool,
    /// Whether the correct branch was within the top-3.
    pub hit_at_3: bool,
    /// Number of rehearsals performed.
    pub rehearsals_performed: u64,
    /// Number of rehearsals whose results were not used.
    pub rehearsals_wasted: u64,
    /// Extra latency introduced by CTE in this run (ms).
    pub extra_latency_ms: u64,
    /// Extra tokens consumed by CTE in this run.
    pub extra_tokens: u64,
    /// Whether the commit was successful.
    pub commit_succeeded: bool,
    /// Whether the commit threshold was not met.
    pub no_qualified: bool,
}

impl CausalTreeMetrics {
    /// Record a single CTE run observation.
    pub fn record(&mut self, obs: &RunObservation) {
        self.total_runs += 1;
        if obs.hit_at_1 {
            self.hits_at_1 += 1;
        }
        if obs.hit_at_3 {
            self.hits_at_3 += 1;
        }
        if obs.no_qualified {
            self.no_branch_qualified += 1;
        }
        self.wasted_rehearsals += obs.rehearsals_wasted;
        self.total_rehearsals += obs.rehearsals_performed;
        self.cumulative_extra_latency_ms += obs.extra_latency_ms;
        self.cumulative_extra_tokens += obs.extra_tokens;
        if obs.commit_succeeded {
            self.commit_successes += 1;
        }
    }

    /// Record a circuit breaker trip event.
    pub fn record_circuit_breaker_trip(&mut self) {
        self.circuit_breaker_trips += 1;
    }

    /// Hit-at-1 ratio: fraction of runs where the first pick was correct.
    pub fn hit_at_1_ratio(&self) -> f64 {
        if self.total_runs == 0 {
            return 0.0;
        }
        self.hits_at_1 as f64 / self.total_runs as f64
    }

    /// Hit-at-3 ratio: fraction of runs where the correct branch was in top-3.
    pub fn hit_at_3_ratio(&self) -> f64 {
        if self.total_runs == 0 {
            return 0.0;
        }
        self.hits_at_3 as f64 / self.total_runs as f64
    }

    /// Wasted speculation ratio: fraction of rehearsals that were not used.
    pub fn wasted_speculation_ratio(&self) -> f64 {
        if self.total_rehearsals == 0 {
            return 0.0;
        }
        self.wasted_rehearsals as f64 / self.total_rehearsals as f64
    }

    /// Commit success rate.
    pub fn commit_success_rate(&self) -> f64 {
        if self.total_runs == 0 {
            return 0.0;
        }
        self.commit_successes as f64 / self.total_runs as f64
    }

    /// Average extra latency per run (ms).
    pub fn avg_extra_latency_ms(&self) -> f64 {
        if self.total_runs == 0 {
            return 0.0;
        }
        self.cumulative_extra_latency_ms as f64 / self.total_runs as f64
    }

    /// Emit the current metrics as a tracing info event.
    pub fn emit_tracing_summary(&self) {
        tracing::info!(
            total_runs = self.total_runs,
            hit_at_1_ratio = format_args!("{:.3}", self.hit_at_1_ratio()),
            hit_at_3_ratio = format_args!("{:.3}", self.hit_at_3_ratio()),
            wasted_speculation_ratio = format_args!("{:.3}", self.wasted_speculation_ratio()),
            commit_success_rate = format_args!("{:.3}", self.commit_success_rate()),
            avg_extra_latency_ms = format_args!("{:.1}", self.avg_extra_latency_ms()),
            circuit_breaker_trips = self.circuit_breaker_trips,
            "CTE metrics summary",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_metrics_ratios() {
        let m = CausalTreeMetrics::default();
        assert!((m.hit_at_1_ratio() - 0.0).abs() < f64::EPSILON);
        assert!((m.hit_at_3_ratio() - 0.0).abs() < f64::EPSILON);
        assert!((m.wasted_speculation_ratio() - 0.0).abs() < f64::EPSILON);
        assert!((m.commit_success_rate() - 0.0).abs() < f64::EPSILON);
        assert!((m.avg_extra_latency_ms() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_record_and_ratios() {
        let mut m = CausalTreeMetrics::default();
        m.record(&RunObservation {
            hit_at_1: true,
            hit_at_3: true,
            rehearsals_performed: 2,
            rehearsals_wasted: 1,
            extra_latency_ms: 100,
            extra_tokens: 500,
            commit_succeeded: true,
            no_qualified: false,
        });
        m.record(&RunObservation {
            hit_at_1: false,
            hit_at_3: true,
            rehearsals_performed: 2,
            rehearsals_wasted: 0,
            extra_latency_ms: 200,
            extra_tokens: 800,
            commit_succeeded: true,
            no_qualified: false,
        });

        assert_eq!(m.total_runs, 2);
        assert_eq!(m.hits_at_1, 1);
        assert_eq!(m.hits_at_3, 2);
        assert!((m.hit_at_1_ratio() - 0.5).abs() < f64::EPSILON);
        assert!((m.hit_at_3_ratio() - 1.0).abs() < f64::EPSILON);
        assert!((m.wasted_speculation_ratio() - 0.25).abs() < f64::EPSILON);
        assert!((m.commit_success_rate() - 1.0).abs() < f64::EPSILON);
        assert!((m.avg_extra_latency_ms() - 150.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_circuit_breaker_trip_counter() {
        let mut m = CausalTreeMetrics::default();
        m.record_circuit_breaker_trip();
        m.record_circuit_breaker_trip();
        assert_eq!(m.circuit_breaker_trips, 2);
    }

    #[test]
    fn test_serde_roundtrip() {
        let mut m = CausalTreeMetrics::default();
        m.record(&RunObservation {
            hit_at_1: true,
            hit_at_3: true,
            rehearsals_performed: 1,
            rehearsals_wasted: 0,
            extra_latency_ms: 50,
            extra_tokens: 100,
            commit_succeeded: true,
            no_qualified: false,
        });
        let json = serde_json::to_string(&m).expect("test: serialize");
        let restored: CausalTreeMetrics =
            serde_json::from_str(&json).expect("test: deserialize");
        assert_eq!(restored.total_runs, 1);
        assert_eq!(restored.hits_at_1, 1);
    }
}
