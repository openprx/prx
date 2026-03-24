//! Feedback writer for the Causal Tree Engine.
//!
//! After the [`PathSelector`] commits a branch, the pipeline calls
//! [`FeedbackWriter::write_decision`] to record the outcome. This decouples
//! observability from the core selection logic and makes the feedback channel
//! replaceable (log sink today, SelfSystem sink in a later version).
//!
//! ## Implementations
//!
//! | Type | Behaviour |
//! |------|-----------|
//! | [`LogFeedbackWriter`] | Structured `tracing::info!` log; never blocks the caller. |
//! | `NoopFeedbackWriter` (test-only) | Silent no-op; intended for unit tests. |

use async_trait::async_trait;

use super::branch::{CausalBranch, PathCommitDecision};
use super::error::CausalTreeError;
use super::state::CausalState;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Records the outcome of a [`PathCommitDecision`] after branch selection.
///
/// Implementations are **non-blocking with respect to the main request path**.
/// A write failure should be logged and surfaced as
/// [`CausalTreeError::FeedbackWriteFailed`], but callers must decide whether
/// to treat it as fatal or advisory.
///
/// # Arguments (for [`write_decision`])
/// * `state`        – Immutable causal state snapshot (provides `session_id`, `request_id`, …).
/// * `decision`     – The commit decision produced by [`PathSelector::select`].
/// * `all_branches` – All candidate branches evaluated during this CTE run.
///   This is the authoritative source for branch metadata; IDs in `decision`
///   must be a subset of IDs in `all_branches`.
#[async_trait]
pub trait FeedbackWriter: Send + Sync {
    /// Persist or emit the commit decision for observability / learning.
    ///
    /// # Errors
    /// Returns [`CausalTreeError::FeedbackWriteFailed`] if the write could not
    /// be completed. Callers should log the error and continue — feedback
    /// failures are advisory by default.
    async fn write_decision(
        &self,
        state: &CausalState,
        decision: &PathCommitDecision,
        all_branches: &[CausalBranch],
    ) -> Result<(), CausalTreeError>;
}

// ---------------------------------------------------------------------------
// LogFeedbackWriter
// ---------------------------------------------------------------------------

/// Feedback writer that emits a structured `tracing::info!` log entry.
///
/// This is the default production implementation for v1.  A SelfSystem
/// integration (for online learning / adaptive policy tuning) is planned
/// for a future version.
///
/// ## Log fields
///
/// | Field | Source |
/// |-------|--------|
/// | `session_id` | `state.session_id` |
/// | `request_id` | `state.request_id` |
/// | `chosen_branch_id` | `decision.chosen_branch_id` |
/// | `fallback_count` | `decision.fallback_branch_ids.len()` |
/// | `rejected_count` | `decision.rejected_branch_ids.len()` |
/// | `total_branches` | `all_branches.len()` |
/// | `reasons` | `decision.reasons` joined with `"; "` |
#[derive(Debug, Default)]
pub struct LogFeedbackWriter;

impl LogFeedbackWriter {
    /// Create a new [`LogFeedbackWriter`].
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FeedbackWriter for LogFeedbackWriter {
    async fn write_decision(
        &self,
        state: &CausalState,
        decision: &PathCommitDecision,
        all_branches: &[CausalBranch],
    ) -> Result<(), CausalTreeError> {
        let reasons_joined = decision.reasons.join("; ");

        tracing::info!(
            session_id = %state.session_id,
            request_id = %state.request_id,
            chosen_branch_id = %decision.chosen_branch_id,
            fallback_count = decision.fallback_branch_ids.len(),
            rejected_count = decision.rejected_branch_ids.len(),
            total_branches = all_branches.len(),
            cache_ttl_seconds = decision.cache_ttl_seconds,
            reasons = %reasons_joined,
            "CTE path decision committed",
        );

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// NoopFeedbackWriter
// ---------------------------------------------------------------------------

/// No-op feedback writer for unit tests.
///
/// Silently discards the decision without any I/O. Used in tests that
/// need a `FeedbackWriter` implementation but do not care about feedback
/// side-effects.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct NoopFeedbackWriter;

#[cfg(test)]
impl NoopFeedbackWriter {
    /// Create a new [`NoopFeedbackWriter`].
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(test)]
#[async_trait]
impl FeedbackWriter for NoopFeedbackWriter {
    async fn write_decision(
        &self,
        _state: &CausalState,
        _decision: &PathCommitDecision,
        _all_branches: &[CausalBranch],
    ) -> Result<(), CausalTreeError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::causal_tree::branch::{BranchLabel, CausalBranch, CommitPolicy, CostEstimate, RehearsalLevel};
    use crate::causal_tree::state::{BudgetState, CausalState, SideEffectMode};

    fn make_branch(id: &str) -> CausalBranch {
        CausalBranch {
            branch_id: id.to_owned(),
            label: BranchLabel::DirectAnswer,
            parent_step_ids: vec![],
            required_inputs: vec![],
            predicted_gain: 0.6,
            estimated_cost: CostEstimate::default(),
            estimated_latency_ms: 80,
            confidence: 0.75,
            rehearsal_level: RehearsalLevel::ScoreOnly,
            commit_policy: CommitPolicy::AutoCommit,
            explanation: vec!["feedback test branch".to_owned()],
        }
    }

    fn make_state() -> CausalState {
        CausalState {
            session_id: "sess-fb-test".to_owned(),
            request_id: "req-fb-test".to_owned(),
            goal: "test feedback".to_owned(),
            user_intent: "simple_qa".to_owned(),
            completed_steps: vec![],
            active_constraints: vec![],
            known_artifacts: vec![],
            unresolved_risks: vec![],
            side_effect_mode: SideEffectMode::ReadOnly,
            budget: BudgetState::default(),
            snapshot_ts: "2026-03-22T00:00:00Z".to_owned(),
        }
    }

    fn make_decision(chosen: &str) -> PathCommitDecision {
        PathCommitDecision {
            chosen_branch_id: chosen.to_owned(),
            rejected_branch_ids: vec!["b2".to_owned()],
            fallback_branch_ids: vec![],
            reasons: vec![format!("branch '{chosen}' selected: score 0.800 >= threshold 0.620")],
            cache_ttl_seconds: 60,
        }
    }

    // -----------------------------------------------------------------------
    // Test 1: LogFeedbackWriter returns Ok without panicking
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_log_writer_returns_ok() {
        let writer = LogFeedbackWriter::new();
        let state = make_state();
        let decision = make_decision("b1");
        let branches = vec![make_branch("b1"), make_branch("b2")];

        let result = writer.write_decision(&state, &decision, &branches).await;
        assert!(result.is_ok(), "test: log writer must succeed: {result:?}");
    }

    // -----------------------------------------------------------------------
    // Test 2: NoopFeedbackWriter always returns Ok
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_noop_writer_returns_ok() {
        let writer = NoopFeedbackWriter::new();
        let state = make_state();
        let decision = make_decision("b1");

        let result = writer.write_decision(&state, &decision, &[]).await;
        assert!(result.is_ok(), "test: noop writer must always succeed");
    }

    // -----------------------------------------------------------------------
    // Test 3: LogFeedbackWriter handles empty branch list without error
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_log_writer_empty_branches() {
        let writer = LogFeedbackWriter::new();
        let state = make_state();
        let decision = PathCommitDecision {
            chosen_branch_id: "b1".to_owned(),
            rejected_branch_ids: vec![],
            fallback_branch_ids: vec![],
            reasons: vec!["only branch, auto-selected".to_owned()],
            cache_ttl_seconds: 60,
        };

        let result = writer.write_decision(&state, &decision, &[]).await;
        assert!(result.is_ok(), "test: must handle empty all_branches slice");
    }

    // -----------------------------------------------------------------------
    // Test 4: FeedbackWriter trait object dispatch works for both impls
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_trait_object_dispatch() {
        let writers: Vec<Box<dyn FeedbackWriter>> =
            vec![Box::new(LogFeedbackWriter::new()), Box::new(NoopFeedbackWriter::new())];
        let state = make_state();
        let decision = make_decision("b1");
        let branches = vec![make_branch("b1")];

        for writer in &writers {
            let result = writer.write_decision(&state, &decision, &branches).await;
            assert!(result.is_ok(), "test: trait object dispatch must succeed");
        }
    }
}
