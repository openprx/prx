//! Path selector for the Causal Tree Engine.
//!
//! The [`PathSelector`] trait abstracts the final decision step: given a list
//! of scored candidate branches, pick the one to commit, keep a fallback, and
//! reject the rest.
//!
//! The primary implementation is [`DefaultPathSelector`], which applies a simple
//! threshold rule: the highest-scored branch must meet `policy.commit_threshold`
//! to become the chosen path.

use async_trait::async_trait;

use super::branch::{CausalBranch, PathCommitDecision, RehearsalArtifact};
use super::error::CausalTreeError;
use super::policy::CausalPolicy;
use super::state::CausalState;

/// Default cache TTL (seconds) for rehearsal artifacts stored in the decision.
const DEFAULT_CACHE_TTL_SECONDS: u32 = 60;

/// Selects the branch to commit from a ranked list of scored candidates.
///
/// Implementations receive an **already score-sorted** (descending) slice of
/// `(branch, score, rehearsal_artifact)` triples and must return a
/// [`PathCommitDecision`] that partitions all branches into:
/// - one **chosen** branch (primary execution path), or
/// - an error if no branch meets the threshold.
///
/// # Contract
/// - `scored_branches` **must** be sorted by score in descending order.
/// - Every branch in `scored_branches` must appear exactly once in the
///   returned `PathCommitDecision` (chosen, fallback, or rejected).
#[async_trait]
pub trait PathSelector: Send + Sync {
    /// Select the primary execution path from a ranked list of candidate branches.
    ///
    /// # Arguments
    /// * `state`           – Immutable causal state snapshot for the current request.
    /// * `scored_branches` – Candidate branches with their scores, sorted **descending**.
    ///   Each element is `(branch, score, optional_rehearsal_artifact)`.
    /// * `policy`          – Runtime policy, including `commit_threshold`.
    ///
    /// # Errors
    /// Returns [`CausalTreeError::NoBranchQualified`] if no branch meets the threshold.
    async fn select(
        &self,
        state: &CausalState,
        scored_branches: &[(CausalBranch, f32, Option<RehearsalArtifact>)],
        policy: &CausalPolicy,
    ) -> Result<PathCommitDecision, CausalTreeError>;
}

/// Default threshold-based path selector.
///
/// Selection rules (applied in order to the score-sorted input):
///
/// 1. **Chosen** — First branch with `score >= policy.commit_threshold`.
/// 2. **Fallback** — Second branch in the list, if it exists (regardless of score).
/// 3. **Rejected** — All remaining branches.
///
/// If no branch meets the threshold,
/// [`CausalTreeError::NoBranchQualified`] is returned with the threshold and
/// best available score (or `0.0` if the input was empty).
#[derive(Debug, Default)]
pub struct DefaultPathSelector;

impl DefaultPathSelector {
    /// Create a new [`DefaultPathSelector`].
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PathSelector for DefaultPathSelector {
    async fn select(
        &self,
        _state: &CausalState,
        scored_branches: &[(CausalBranch, f32, Option<RehearsalArtifact>)],
        policy: &CausalPolicy,
    ) -> Result<PathCommitDecision, CausalTreeError> {
        // Reject NaN/infinite scores up-front so comparisons below are sound.
        for (branch, score, _) in scored_branches {
            if !score.is_finite() {
                return Err(CausalTreeError::ExpansionFailed(format!(
                    "branch {} has non-finite score: {score}",
                    branch.branch_id
                )));
            }
        }

        // Best score for error reporting; 0.0 when input is empty.
        let best_score = scored_branches.first().map(|(_, s, _)| *s).unwrap_or(0.0_f32);

        // First entry must meet the threshold to become the chosen branch.
        let (chosen, _chosen_score, _chosen_artifact) = match scored_branches.first() {
            Some(entry) if entry.1 >= policy.commit_threshold => entry,
            _ => {
                return Err(CausalTreeError::NoBranchQualified {
                    threshold: policy.commit_threshold,
                    best_score,
                });
            }
        };

        let mut reasons: Vec<String> = Vec::new();
        reasons.push(format!(
            "branch '{}' selected: score {:.3} >= threshold {:.3}",
            chosen.branch_id, _chosen_score, policy.commit_threshold,
        ));

        // Second entry (if present) becomes the fallback regardless of score.
        let fallback_branch_ids: Vec<String> = scored_branches
            .get(1)
            .map(|(b, score, _)| {
                reasons.push(format!(
                    "branch '{}' kept as fallback (score {:.3})",
                    b.branch_id, score,
                ));
                vec![b.branch_id.clone()]
            })
            .unwrap_or_default();

        // Everything from index 2 onward is rejected (superseded by a better-ranked branch).
        let rejected_branch_ids: Vec<String> = scored_branches
            .iter()
            .skip(2)
            .map(|(b, score, _)| {
                reasons.push(format!(
                    "branch '{}' rejected: superseded (rank > 2, score {:.3})",
                    b.branch_id, score,
                ));
                b.branch_id.clone()
            })
            .collect();

        Ok(PathCommitDecision {
            chosen_branch_id: chosen.branch_id.clone(),
            rejected_branch_ids,
            fallback_branch_ids,
            reasons,
            cache_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
        })
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
    use crate::causal_tree::policy::CausalPolicy;
    use crate::causal_tree::state::{BudgetState, CausalState, SideEffectMode};

    fn make_branch(id: &str) -> CausalBranch {
        CausalBranch {
            branch_id: id.to_owned(),
            label: BranchLabel::DirectAnswer,
            parent_step_ids: vec![],
            required_inputs: vec![],
            predicted_gain: 0.5,
            estimated_cost: CostEstimate::default(),
            estimated_latency_ms: 100,
            confidence: 0.8,
            rehearsal_level: RehearsalLevel::ScoreOnly,
            commit_policy: CommitPolicy::AutoCommit,
            explanation: vec!["test branch".to_owned()],
        }
    }

    fn make_state() -> CausalState {
        CausalState {
            session_id: "sess-test".to_owned(),
            request_id: "req-test".to_owned(),
            goal: "test goal".to_owned(),
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

    fn default_policy() -> CausalPolicy {
        CausalPolicy {
            commit_threshold: 0.62,
            ..CausalPolicy::default()
        }
    }

    // -----------------------------------------------------------------------
    // Test 1: single branch above threshold → chosen, no fallback, no rejected
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_single_branch_above_threshold() {
        let selector = DefaultPathSelector::new();
        let state = make_state();
        let policy = default_policy();
        let branches = vec![(make_branch("b1"), 0.80_f32, None)];

        let decision = selector
            .select(&state, &branches, &policy)
            .await
            .expect("test: should select");

        assert_eq!(decision.chosen_branch_id, "b1");
        assert!(decision.fallback_branch_ids.is_empty());
        assert!(decision.rejected_branch_ids.is_empty());
        assert_eq!(decision.cache_ttl_seconds, DEFAULT_CACHE_TTL_SECONDS);
        assert!(!decision.reasons.is_empty());
    }

    // -----------------------------------------------------------------------
    // Test 2: three branches — first chosen, second fallback, third rejected
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_three_branches_full_partition() {
        let selector = DefaultPathSelector::new();
        let state = make_state();
        let policy = default_policy();
        // Already sorted descending: b1(0.90) > b2(0.70) > b3(0.50)
        let branches = vec![
            (make_branch("b1"), 0.90_f32, None),
            (make_branch("b2"), 0.70_f32, None),
            (make_branch("b3"), 0.50_f32, None),
        ];

        let decision = selector
            .select(&state, &branches, &policy)
            .await
            .expect("test: should select");

        assert_eq!(decision.chosen_branch_id, "b1");
        assert_eq!(decision.fallback_branch_ids, vec!["b2"]);
        assert_eq!(decision.rejected_branch_ids, vec!["b3"]);
        // reasons must mention all three branches
        let all_reasons = decision.reasons.join(" ");
        assert!(all_reasons.contains("b1"));
        assert!(all_reasons.contains("b2"));
        assert!(all_reasons.contains("b3"));
    }

    // -----------------------------------------------------------------------
    // Test 3: best score below threshold → NoBranchQualified error
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_no_branch_meets_threshold() {
        let selector = DefaultPathSelector::new();
        let state = make_state();
        let policy = default_policy(); // threshold = 0.62
        let branches = vec![(make_branch("b1"), 0.61_f32, None), (make_branch("b2"), 0.40_f32, None)];

        let err = selector
            .select(&state, &branches, &policy)
            .await
            .expect_err("test: should fail with NoBranchQualified");

        match err {
            CausalTreeError::NoBranchQualified { threshold, best_score } => {
                assert!((threshold - 0.62_f32).abs() < f32::EPSILON);
                assert!((best_score - 0.61_f32).abs() < f32::EPSILON);
            }
            other => panic!("test: unexpected error variant: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // Test 4: empty branch list → NoBranchQualified with best_score = 0.0
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_empty_branches_returns_no_branch_qualified() {
        let selector = DefaultPathSelector::new();
        let state = make_state();
        let policy = default_policy();

        let err = selector
            .select(&state, &[], &policy)
            .await
            .expect_err("test: empty list should fail");

        match err {
            CausalTreeError::NoBranchQualified { best_score, .. } => {
                assert!((best_score - 0.0_f32).abs() < f32::EPSILON);
            }
            other => panic!("test: unexpected error: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // Test 5: score exactly equal to threshold → chosen (boundary condition)
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_score_exactly_at_threshold() {
        let selector = DefaultPathSelector::new();
        let state = make_state();
        let policy = CausalPolicy {
            commit_threshold: 0.75,
            ..CausalPolicy::default()
        };
        let branches = vec![(make_branch("b1"), 0.75_f32, None)];

        let decision = selector
            .select(&state, &branches, &policy)
            .await
            .expect("test: score == threshold should qualify");

        assert_eq!(decision.chosen_branch_id, "b1");
    }

    // -----------------------------------------------------------------------
    // Test 6: NaN score → ExpansionFailed error
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_nan_score_rejected() {
        let selector = DefaultPathSelector::new();
        let state = make_state();
        let policy = default_policy();
        let branches = vec![(make_branch("b1"), f32::NAN, None)];

        let err = selector
            .select(&state, &branches, &policy)
            .await
            .expect_err("test: NaN score must be rejected");

        assert!(
            matches!(err, CausalTreeError::ExpansionFailed(_)),
            "test: expected ExpansionFailed, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 7: infinite score → ExpansionFailed error
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_infinite_score_rejected() {
        let selector = DefaultPathSelector::new();
        let state = make_state();
        let policy = default_policy();

        for score in [f32::INFINITY, f32::NEG_INFINITY] {
            let branches = vec![(make_branch("b1"), score, None)];
            let err = selector
                .select(&state, &branches, &policy)
                .await
                .expect_err("test: infinite score must be rejected");

            assert!(
                matches!(err, CausalTreeError::ExpansionFailed(_)),
                "test: expected ExpansionFailed for score={score}, got: {err}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 8: rejected reason text does not claim "< threshold" for rank>2
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_rejected_reason_text() {
        let selector = DefaultPathSelector::new();
        let state = make_state();
        let policy = CausalPolicy {
            commit_threshold: 0.50,
            ..CausalPolicy::default()
        };
        // All three branches qualify in score, but only first is chosen,
        // second is fallback, third is rejected.
        let branches = vec![
            (make_branch("b1"), 0.90_f32, None),
            (make_branch("b2"), 0.80_f32, None),
            (make_branch("b3"), 0.70_f32, None),
        ];

        let decision = selector
            .select(&state, &branches, &policy)
            .await
            .expect("test: all qualify, should select");

        // b3 is rejected because it's rank>2, not because score < threshold
        let rejected_reason = decision
            .reasons
            .iter()
            .find(|r| r.contains("b3"))
            .expect("test: must have reason for b3");
        assert!(
            rejected_reason.contains("superseded"),
            "test: rejected reason must say 'superseded', got: {rejected_reason}"
        );
        assert!(
            !rejected_reason.contains("< threshold"),
            "test: must not claim '< threshold' for high-scoring rejected branch"
        );
    }
}
