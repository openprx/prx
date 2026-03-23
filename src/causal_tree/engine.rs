//! CausalTreeEngine — the top-level orchestrator for the CTE pipeline.
//!
//! This module wires together all CTE components into a single `run` method:
//!
//! ```text
//! snapshot → expand → rehearse → score → select → feedback
//! ```
//!
//! The engine uses **composition** (not a super-trait) to combine components,
//! following the audit recommendation to avoid a "God Object" trait.
//!
//! ## Degradation & Circuit Breaker
//!
//! - Each stage is subject to the pipeline timeout from [`CausalPolicy`].
//! - If the circuit breaker is open, `run` immediately returns
//!   [`CausalTreeError::CircuitBreakerOpen`].
//! - On any failure the circuit breaker state is updated; on success it resets.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use super::branch::{CausalBranch, PathCommitDecision, RehearsalArtifact};
use super::error::CausalTreeError;
use super::expander::TreeExpander;
use super::feedback::FeedbackWriter;
use super::metrics::{CausalTreeMetrics, RunObservation};
use super::policy::{CausalPolicy, CausalTreeConfig, CircuitBreakerState};
use super::rehearsal::RehearsalEngine;
use super::scorer::BranchScorer;
use super::selector::PathSelector;
use super::state::CausalState;
use crate::observability::{Observer, ObserverEvent, ObserverMetric};

/// The top-level CTE pipeline orchestrator.
///
/// All components are injected via `Arc<dyn Trait>` to support runtime
/// polymorphism and shared ownership across async tasks.
pub struct CausalTreeEngine {
    expander: Arc<dyn TreeExpander>,
    rehearsal: Arc<dyn RehearsalEngine>,
    scorer: Arc<dyn BranchScorer>,
    selector: Arc<dyn PathSelector>,
    feedback: Arc<dyn FeedbackWriter>,
    observer: Arc<dyn Observer>,
    config: CausalTreeConfig,
    circuit_breaker: Mutex<CircuitBreakerState>,
    metrics: Mutex<CausalTreeMetrics>,
}

impl CausalTreeEngine {
    /// Construct a new engine with all components injected.
    pub fn new(
        expander: Arc<dyn TreeExpander>,
        rehearsal: Arc<dyn RehearsalEngine>,
        scorer: Arc<dyn BranchScorer>,
        selector: Arc<dyn PathSelector>,
        feedback: Arc<dyn FeedbackWriter>,
        observer: Arc<dyn Observer>,
        config: CausalTreeConfig,
    ) -> Self {
        Self {
            expander,
            rehearsal,
            scorer,
            selector,
            feedback,
            observer,
            config,
            circuit_breaker: Mutex::new(CircuitBreakerState::default()),
            metrics: Mutex::new(CausalTreeMetrics::default()),
        }
    }

    /// Returns `true` if the CTE is enabled in the configuration.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Returns a reference to the engine's configuration.
    pub fn config(&self) -> &CausalTreeConfig {
        &self.config
    }

    /// Returns a snapshot of the current accumulated metrics.
    pub fn snapshot_metrics(&self) -> CausalTreeMetrics {
        self.metrics.lock().clone()
    }

    /// Run the full CTE pipeline for the given state.
    ///
    /// # Returns
    /// - `Ok((decision, chosen_branch))` on success.
    /// - `Err(CausalTreeError)` on any failure (including circuit breaker open,
    ///   pipeline timeout, no branch qualified, etc.).
    ///
    /// On failure the circuit breaker is updated. On success it resets.
    pub async fn run(
        &self,
        state: &CausalState,
    ) -> Result<(PathCommitDecision, CausalBranch), CausalTreeError> {
        let pipeline_start = Instant::now();
        let policy = &self.config.policy;

        // --- Circuit breaker check ---
        // Acquire and release cb lock before metrics lock to prevent
        // lock-order coupling (Codex audit finding).
        let cb_open_info = {
            let cb = self.circuit_breaker.lock();
            if cb.is_open(policy) {
                Some(cb.consecutive_failures)
            } else {
                None
            }
        };
        if let Some(consecutive_failures) = cb_open_info {
            self.metrics.lock().record_circuit_breaker_trip();
            let elapsed = pipeline_start.elapsed();
            self.observer.record_event(&ObserverEvent::CteRun {
                branch_count: 0,
                chosen_branch: String::new(),
                chosen_label: String::new(),
                extra_latency_ms: elapsed.as_millis() as u64,
                commit_succeeded: false,
                circuit_breaker_tripped: true,
            });
            self.observer
                .record_metric(&ObserverMetric::CteExtraLatency(elapsed));
            return Err(CausalTreeError::CircuitBreakerOpen {
                consecutive_failures,
            });
        }

        let result = self
            .run_pipeline(state, policy, pipeline_start)
            .await;

        // Update circuit breaker and metrics based on outcome.
        match &result {
            Ok(_) => {
                self.circuit_breaker.lock().record_success();
            }
            Err(_) => {
                let mut cb = self.circuit_breaker.lock();
                cb.record_failure();
                cb.maybe_open(policy);
            }
        }

        // Emit observer event and metric for the pipeline run.
        let elapsed = pipeline_start.elapsed();
        let elapsed_ms = elapsed.as_millis() as u64;
        match &result {
            Ok((decision, branch)) => {
                self.observer.record_event(&ObserverEvent::CteRun {
                    branch_count: decision.fallback_branch_ids.len()
                        + decision.rejected_branch_ids.len()
                        + 1,
                    chosen_branch: decision.chosen_branch_id.clone(),
                    chosen_label: branch.label.as_str().to_string(),
                    extra_latency_ms: elapsed_ms,
                    commit_succeeded: true,
                    circuit_breaker_tripped: false,
                });
            }
            Err(_) => {
                self.observer.record_event(&ObserverEvent::CteRun {
                    branch_count: 0,
                    chosen_branch: String::new(),
                    chosen_label: String::new(),
                    extra_latency_ms: elapsed_ms,
                    commit_succeeded: false,
                    circuit_breaker_tripped: false,
                });
            }
        }
        self.observer
            .record_metric(&ObserverMetric::CteExtraLatency(elapsed));

        result
    }

    /// Inner pipeline — separated for cleaner circuit-breaker bookkeeping.
    async fn run_pipeline(
        &self,
        state: &CausalState,
        policy: &CausalPolicy,
        pipeline_start: Instant,
    ) -> Result<(PathCommitDecision, CausalBranch), CausalTreeError> {
        // --- 1. Expand ---
        self.check_timeout(policy, pipeline_start)?;
        let branches = self.expander.expand(state, policy).await?;

        // --- 2. Rehearse ---
        let mut artifacts: Vec<Option<RehearsalArtifact>> = Vec::with_capacity(branches.len());
        for branch in &branches {
            self.check_timeout(policy, pipeline_start)?;
            let artifact = self.rehearsal.rehearse(state, branch, policy).await;
            match artifact {
                Ok(a) => artifacts.push(Some(a)),
                Err(CausalTreeError::RehearsalTimeout { .. }) => {
                    // Non-fatal: score without artifact.
                    tracing::warn!(
                        branch_id = %branch.branch_id,
                        "rehearsal timed out, scoring without artifact"
                    );
                    artifacts.push(None);
                }
                Err(e) => {
                    tracing::warn!(
                        branch_id = %branch.branch_id,
                        error = %e,
                        "rehearsal failed, scoring without artifact"
                    );
                    artifacts.push(None);
                }
            }
        }

        // --- 3. Score ---
        self.check_timeout(policy, pipeline_start)?;
        let scored: Vec<(CausalBranch, f32, Option<RehearsalArtifact>)> = branches
            .into_iter()
            .zip(artifacts.into_iter())
            .filter_map(|(branch, artifact)| {
                let score = self.scorer.score(
                    state,
                    &branch,
                    artifact.as_ref(),
                    &self.config,
                )?;
                Some((branch, score, artifact))
            })
            .collect();

        // Sort by score descending.
        let mut sorted = scored;
        sorted.sort_by(|a, b| b.1.total_cmp(&a.1));

        // --- 4. Select ---
        self.check_timeout(policy, pipeline_start)?;
        let decision = self.selector.select(state, &sorted, policy).await?;

        // Find the chosen branch for the caller.
        let chosen_branch = sorted
            .iter()
            .find(|(b, _, _)| b.branch_id == decision.chosen_branch_id)
            .map(|(b, _, _)| b.clone())
            .ok_or_else(|| {
                CausalTreeError::ExpansionFailed(format!(
                    "chosen branch '{}' not found in scored set",
                    decision.chosen_branch_id,
                ))
            })?;

        // --- 5. Feedback ---
        let all_branches: Vec<CausalBranch> =
            sorted.iter().map(|(b, _, _)| b.clone()).collect();
        if let Err(e) = self
            .feedback
            .write_decision(state, &decision, &all_branches)
            .await
        {
            tracing::warn!(error = %e, "feedback write failed (non-fatal)");
        }

        // --- 6. Record metrics ---
        let elapsed_ms = pipeline_start.elapsed().as_millis() as u64;
        let rehearsals_performed = all_branches.len() as u64;
        let rehearsals_wasted = rehearsals_performed.saturating_sub(1); // at most 1 is used
        {
            let mut m = self.metrics.lock();
            m.record(&RunObservation {
                hit_at_1: true, // first version: assume hit (real tracking requires post-hoc)
                hit_at_3: true,
                rehearsals_performed,
                rehearsals_wasted,
                extra_latency_ms: elapsed_ms,
                extra_tokens: state.budget.tokens_used,
                commit_succeeded: true,
                no_qualified: false,
            });
        }

        Ok((decision, chosen_branch))
    }

    /// Check whether the pipeline has exceeded its total time budget.
    fn check_timeout(
        &self,
        policy: &CausalPolicy,
        start: Instant,
    ) -> Result<(), CausalTreeError> {
        let elapsed_ms = start.elapsed().as_millis() as u64;
        if elapsed_ms > policy.extra_latency_budget_ms {
            return Err(CausalTreeError::PipelineTimeout {
                budget_ms: policy.extra_latency_budget_ms,
                elapsed_ms,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_tree::branch::BranchLabel;
    use crate::causal_tree::expander::DefaultTreeExpander;
    use crate::causal_tree::feedback::NoopFeedbackWriter;
    use crate::causal_tree::rehearsal::DefaultRehearsalEngine;
    use crate::causal_tree::scorer::DefaultBranchScorer;
    use crate::causal_tree::selector::DefaultPathSelector;
    use crate::causal_tree::state::{BudgetState, SideEffectMode};
    use crate::observability::noop::NoopObserver;

    fn make_state(intent: &str) -> CausalState {
        CausalState {
            session_id: "sess-engine-test".into(),
            request_id: "req-engine-test".into(),
            goal: "test goal".into(),
            user_intent: intent.into(),
            completed_steps: vec![],
            active_constraints: vec![],
            known_artifacts: vec![],
            unresolved_risks: vec![],
            side_effect_mode: SideEffectMode::ReadOnly,
            budget: BudgetState {
                extra_token_limit: 4096,
                tokens_used: 0,
                extra_latency_budget_ms: 5000,
                latency_used_ms: 0,
            },
            snapshot_ts: "2026-03-22T00:00:00Z".into(),
        }
    }

    fn make_engine(enabled: bool) -> CausalTreeEngine {
        let config = CausalTreeConfig {
            enabled,
            policy: CausalPolicy {
                commit_threshold: 0.30, // low threshold for tests
                extra_latency_budget_ms: 5000,
                ..CausalPolicy::default()
            },
            ..CausalTreeConfig::default()
        };
        CausalTreeEngine::new(
            Arc::new(DefaultTreeExpander::new()),
            Arc::new(DefaultRehearsalEngine::new()),
            Arc::new(DefaultBranchScorer::new()),
            Arc::new(DefaultPathSelector::new()),
            Arc::new(NoopFeedbackWriter::new()),
            Arc::new(NoopObserver),
            config,
        )
    }

    #[tokio::test]
    async fn test_engine_simple_intent() {
        let engine = make_engine(true);
        let state = make_state("hello");

        let result = engine.run(&state).await;
        assert!(result.is_ok(), "simple intent should succeed: {result:?}");

        let (decision, chosen) = result.expect("test: unwrap ok");
        assert_eq!(chosen.label, BranchLabel::DirectAnswer);
        assert!(!decision.chosen_branch_id.is_empty());
    }

    #[tokio::test]
    async fn test_engine_retrieval_intent() {
        let engine = make_engine(true);
        let state = make_state("audit the repository");

        let result = engine.run(&state).await;
        assert!(result.is_ok(), "retrieval intent should succeed: {result:?}");

        let (_decision, chosen) = result.expect("test: unwrap ok");
        assert_eq!(chosen.label, BranchLabel::RetrieveThenAnswer);
    }

    #[tokio::test]
    async fn test_engine_circuit_breaker() {
        let config = CausalTreeConfig {
            enabled: true,
            policy: CausalPolicy {
                commit_threshold: 0.99, // impossibly high → triggers failures
                circuit_breaker_threshold: 2,
                circuit_breaker_cooldown_secs: 3600,
                extra_latency_budget_ms: 5000,
                ..CausalPolicy::default()
            },
            ..CausalTreeConfig::default()
        };
        let engine = CausalTreeEngine::new(
            Arc::new(DefaultTreeExpander::new()),
            Arc::new(DefaultRehearsalEngine::new()),
            Arc::new(DefaultBranchScorer::new()),
            Arc::new(DefaultPathSelector::new()),
            Arc::new(NoopFeedbackWriter::new()),
            Arc::new(NoopObserver),
            config,
        );
        let state = make_state("hello");

        // First two failures
        let _ = engine.run(&state).await;
        let _ = engine.run(&state).await;

        // Third attempt should hit circuit breaker
        let result = engine.run(&state).await;
        assert!(
            matches!(result, Err(CausalTreeError::CircuitBreakerOpen { .. })),
            "circuit breaker should be open: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_engine_metrics_accumulate() {
        let engine = make_engine(true);
        let state = make_state("hello");

        let _ = engine.run(&state).await;
        let _ = engine.run(&state).await;

        let metrics = engine.snapshot_metrics();
        assert!(metrics.total_runs >= 2);
    }
}
