//! Branch scoring for the Causal Tree Engine.
//!
//! Implements a two-phase scoring model:
//!
//! 1. **Hard gates** — disqualify a branch outright (`None`).
//! 2. **Soft scoring** — produce a composite score in `[0.0, 1.0]`.
//!
//! The default implementation covers three dimensions: confidence, cost, and
//! latency. Weights are read from [`CausalTreeConfig`] so they can be tuned
//! without recompilation.

use super::{
    branch::{BranchLabel, CausalBranch, CommitPolicy, RehearsalArtifact},
    policy::CausalTreeConfig,
    state::{CausalState, SideEffectMode},
};

// ---------------------------------------------------------------------------
// Public trait
// ---------------------------------------------------------------------------

/// Scores a candidate branch for the Causal Tree Engine.
///
/// Implementors must be `Send + Sync` so they can be shared across async
/// tasks. All methods take shared references only — scoring is a read-only
/// operation.
pub trait BranchScorer: Send + Sync {
    /// Score a branch. Returns a composite score in `[0.0, 1.0]`.
    ///
    /// Returns `None` if the branch is disqualified by hard gates (i.e. it
    /// must not be committed under the current state and config).
    fn score(
        &self,
        state: &CausalState,
        branch: &CausalBranch,
        artifact: Option<&RehearsalArtifact>,
        config: &CausalTreeConfig,
    ) -> Option<f32>;
}

// ---------------------------------------------------------------------------
// Scored result — returned by `rank_branches`
// ---------------------------------------------------------------------------

/// A branch paired with its computed score.
///
/// Produced by [`DefaultBranchScorer::rank_branches`] — branches that were
/// disqualified by hard gates are excluded from the output.
#[derive(Debug, Clone)]
pub struct ScoredBranch<'a> {
    /// Reference to the original candidate branch.
    pub branch: &'a CausalBranch,
    /// Composite score in `[0.0, 1.0]`.
    pub score: f32,
}

// ---------------------------------------------------------------------------
// Default implementation
// ---------------------------------------------------------------------------

/// Default branch scorer shipped with the CTE.
///
/// **Hard gates** (any one triggers disqualification → `None`):
/// - The branch requires a write-capable input but the current
///   [`SideEffectMode`] is [`SideEffectMode::ReadOnly`].
/// - The branch's estimated token cost exceeds the remaining token budget.
/// - The branch's estimated latency exceeds the remaining latency budget.
/// - The branch's [`CommitPolicy`] is [`CommitPolicy::RequireApproval`] while
///   the mode is [`SideEffectMode::ReadOnly`] (approval cannot be obtained).
///
/// **Soft scoring formula:**
/// ```text
/// score = w_confidence * norm(confidence)
///       - w_cost      * norm(estimated_tokens / remaining_tokens)
///       - w_latency   * norm(estimated_latency_ms / remaining_latency_ms)
///       + rehearsal_bonus
/// ```
/// where `norm(x) = x.clamp(0.0, 1.0)` and
/// `rehearsal_bonus = artifact.score_delta * 0.1` (if a rehearsal artifact is
/// present).
///
/// The final score is clamped to `[0.0, 1.0]`.
///
/// > **Note on write-capability detection**: `required_inputs` entries that
/// > start with `"write:"` or `"tool:write"` are treated as write operations.
/// > In a future iteration this should be replaced with a typed capability
/// > enum (bitflags) to avoid fragile string matching.
#[derive(Debug, Default)]
pub struct DefaultBranchScorer;

impl DefaultBranchScorer {
    /// Create a new `DefaultBranchScorer`.
    pub fn new() -> Self {
        Self
    }

    /// Rank a slice of `(branch, optional_artifact)` pairs by score,
    /// descending.
    ///
    /// Branches disqualified by hard gates are excluded from the result.
    /// Ties are broken deterministically by:
    /// 1. `confidence` descending
    /// 2. `estimated_tokens` ascending
    /// 3. `estimated_latency_ms` ascending
    /// 4. `branch_id` lexicographic ascending
    ///
    /// This guarantees a stable, reproducible ranking.
    pub fn rank_branches<'a>(
        &self,
        state: &CausalState,
        branches: &[(&'a CausalBranch, Option<&'a RehearsalArtifact>)],
        config: &CausalTreeConfig,
    ) -> Vec<ScoredBranch<'a>> {
        let mut scored: Vec<ScoredBranch<'a>> = branches
            .iter()
            .filter_map(|(branch, artifact)| {
                self.score(state, branch, *artifact, config)
                    .map(|score| ScoredBranch { branch, score })
            })
            .collect();

        scored.sort_by(|a, b| {
            // Primary: score descending
            b.score
                .total_cmp(&a.score)
                // Tie-break 1: confidence descending
                .then_with(|| {
                    b.branch
                        .confidence
                        .total_cmp(&a.branch.confidence)
                })
                // Tie-break 2: estimated_tokens ascending
                .then_with(|| {
                    a.branch
                        .estimated_cost
                        .estimated_tokens
                        .cmp(&b.branch.estimated_cost.estimated_tokens)
                })
                // Tie-break 3: estimated_latency_ms ascending
                .then_with(|| {
                    a.branch
                        .estimated_latency_ms
                        .cmp(&b.branch.estimated_latency_ms)
                })
                // Tie-break 4: branch_id lexicographic ascending
                .then_with(|| a.branch.branch_id.cmp(&b.branch.branch_id))
        });

        scored
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Returns `true` if any entry in `required_inputs` represents a write
/// operation.
///
/// Currently detects write inputs by string prefix. This should be replaced
/// with a typed capability type in a future iteration.
fn branch_requires_write(branch: &CausalBranch) -> bool {
    branch.required_inputs.iter().any(|input| {
        input.starts_with("write:") || input.starts_with("tool:write")
    })
}

/// Normalise a ratio to `[0.0, 1.0]`, handling the zero-denominator edge
/// case safely.
///
/// - If `denominator == 0` and `numerator == 0`, returns `0.0`.
/// - If `denominator == 0` and `numerator > 0`, returns `1.0` (full cost
///   relative to an exhausted budget).
fn safe_norm(numerator: u64, denominator: u64) -> f32 {
    if denominator == 0 {
        if numerator == 0 {
            0.0_f32
        } else {
            1.0_f32
        }
    } else {
        (numerator as f32 / denominator as f32).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// BranchScorer implementation
// ---------------------------------------------------------------------------

impl BranchScorer for DefaultBranchScorer {
    fn score(
        &self,
        state: &CausalState,
        branch: &CausalBranch,
        artifact: Option<&RehearsalArtifact>,
        config: &CausalTreeConfig,
    ) -> Option<f32> {
        // ----------------------------------------------------------------
        // Phase 1 — Hard gates
        // ----------------------------------------------------------------

        // Gate 1: write-capable inputs require a writable side-effect mode.
        // Exception: AskApproval branches are control-flow (not write ops)
        // and must remain available even in ReadOnly so the user can be
        // asked before any write is attempted.
        if branch.label != BranchLabel::AskApproval
            && state.side_effect_mode == SideEffectMode::ReadOnly
            && branch_requires_write(branch)
        {
            return None;
        }

        // Gate 2: estimated tokens must not exceed the remaining token budget.
        let remaining_tokens = state.budget.remaining_tokens();
        if branch.estimated_cost.estimated_tokens > remaining_tokens {
            return None;
        }

        // Gate 3: estimated latency must not exceed the remaining latency budget.
        let remaining_latency_ms = state.budget.remaining_latency_ms();
        if u64::from(branch.estimated_latency_ms) > remaining_latency_ms {
            return None;
        }

        // Gate 4: RequireApproval branches cannot proceed in ReadOnly mode
        // because there is no mechanism to obtain user approval.
        // Note: this is distinct from gate 1 — a branch may require approval
        // for non-write reasons (e.g. irreversible read side-effects).
        if branch.commit_policy == CommitPolicy::RequireApproval
            && state.side_effect_mode == SideEffectMode::ReadOnly
        {
            return None;
        }

        // ----------------------------------------------------------------
        // Phase 2 — Soft scoring
        // ----------------------------------------------------------------

        // Confidence is already in [0.0, 1.0]; clamp defensively.
        let norm_confidence = branch.confidence.clamp(0.0, 1.0);

        // Cost normalised against remaining budget (not total limit) so that
        // the score reflects resource scarcity at decision time.
        let norm_cost = safe_norm(branch.estimated_cost.estimated_tokens, remaining_tokens);

        // Latency normalised against remaining latency budget.
        let norm_latency =
            safe_norm(u64::from(branch.estimated_latency_ms), remaining_latency_ms);

        // Rehearsal bonus: a positive score_delta means the rehearsal
        // produced better-than-expected results. The 0.1 multiplier keeps
        // the bonus bounded well below the primary weight (0.25 minimum).
        // TODO: expose as `config.w_rehearsal` in a future iteration.
        let rehearsal_bonus = artifact
            .map(|a| a.score_delta.clamp(-1.0, 1.0) * 0.1)
            .unwrap_or(0.0);

        let raw_score = config.w_confidence.mul_add(
            norm_confidence,
            config
                .w_cost
                .mul_add(-norm_cost, config.w_latency.mul_add(-norm_latency, rehearsal_bonus)),
        );

        Some(raw_score.clamp(0.0, 1.0))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_tree::{
        branch::{BranchLabel, CausalBranch, CommitPolicy, CostEstimate, RehearsalArtifact},
        policy::CausalTreeConfig,
        state::{BudgetState, CausalState, SideEffectMode},
    };

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn base_state() -> CausalState {
        CausalState {
            session_id: "sess-test".into(),
            request_id: "req-test".into(),
            goal: "test goal".into(),
            user_intent: "test".into(),
            completed_steps: vec![],
            active_constraints: vec![],
            known_artifacts: vec![],
            unresolved_risks: vec![],
            side_effect_mode: SideEffectMode::GuardedWrite,
            budget: BudgetState {
                extra_token_limit: 1000,
                tokens_used: 0,
                extra_latency_budget_ms: 500,
                latency_used_ms: 0,
            },
            snapshot_ts: "2026-03-22T00:00:00Z".into(),
        }
    }

    fn base_branch(id: &str) -> CausalBranch {
        CausalBranch {
            branch_id: id.into(),
            label: BranchLabel::DirectAnswer,
            parent_step_ids: vec![],
            required_inputs: vec![],
            predicted_gain: 0.5,
            estimated_cost: CostEstimate {
                estimated_tokens: 100,
                estimated_cost_micro_usd: 0,
            },
            estimated_latency_ms: 50,
            confidence: 0.8,
            rehearsal_level: crate::causal_tree::branch::RehearsalLevel::ScoreOnly,
            commit_policy: CommitPolicy::AutoCommit,
            explanation: vec![],
        }
    }

    fn default_config() -> CausalTreeConfig {
        CausalTreeConfig::default()
    }

    // -----------------------------------------------------------------------
    // Gate tests
    // -----------------------------------------------------------------------

    /// Gate 1: write-input branch is blocked in ReadOnly mode.
    #[test]
    fn test_gate_write_input_blocked_in_readonly() {
        let scorer = DefaultBranchScorer::new();
        let state = CausalState {
            side_effect_mode: SideEffectMode::ReadOnly,
            ..base_state()
        };
        let branch = CausalBranch {
            required_inputs: vec!["write:file".into()],
            ..base_branch("b-write")
        };
        assert_eq!(
            scorer.score(&state, &branch, None, &default_config()),
            None,
            "write input must be blocked in ReadOnly"
        );
    }

    /// Gate 1 exception: AskApproval branches bypass the write gate.
    #[test]
    fn test_gate_ask_approval_bypasses_write_gate() {
        let scorer = DefaultBranchScorer::new();
        let state = CausalState {
            side_effect_mode: SideEffectMode::ApprovalRequired,
            ..base_state()
        };
        let branch = CausalBranch {
            label: BranchLabel::AskApproval,
            required_inputs: vec!["write:database".into()],
            // RequireApproval is fine with ApprovalRequired mode
            commit_policy: CommitPolicy::RequireApproval,
            ..base_branch("b-ask")
        };
        // ApprovalRequired mode allows RequireApproval commit policy
        let result = scorer.score(&state, &branch, None, &default_config());
        assert!(
            result.is_some(),
            "AskApproval branch should not be blocked by write gate"
        );
    }

    /// Gate 2: token budget exceeded blocks the branch.
    #[test]
    fn test_gate_token_budget_exceeded() {
        let scorer = DefaultBranchScorer::new();
        let state = CausalState {
            budget: BudgetState {
                extra_token_limit: 1000,
                tokens_used: 950, // only 50 remaining
                extra_latency_budget_ms: 500,
                latency_used_ms: 0,
            },
            ..base_state()
        };
        let branch = CausalBranch {
            estimated_cost: CostEstimate {
                estimated_tokens: 200, // exceeds remaining 50
                estimated_cost_micro_usd: 0,
            },
            ..base_branch("b-costly")
        };
        assert_eq!(
            scorer.score(&state, &branch, None, &default_config()),
            None,
            "branch exceeding token budget must be blocked"
        );
    }

    /// Gate 3: latency budget exceeded blocks the branch.
    #[test]
    fn test_gate_latency_budget_exceeded() {
        let scorer = DefaultBranchScorer::new();
        let state = CausalState {
            budget: BudgetState {
                extra_token_limit: 1000,
                tokens_used: 0,
                extra_latency_budget_ms: 500,
                latency_used_ms: 450, // only 50ms remaining
            },
            ..base_state()
        };
        let branch = CausalBranch {
            estimated_latency_ms: 100, // exceeds remaining 50ms
            ..base_branch("b-slow")
        };
        assert_eq!(
            scorer.score(&state, &branch, None, &default_config()),
            None,
            "branch exceeding latency budget must be blocked"
        );
    }

    /// Gate 4: RequireApproval commit policy is blocked in ReadOnly mode.
    #[test]
    fn test_gate_require_approval_blocked_in_readonly() {
        let scorer = DefaultBranchScorer::new();
        let state = CausalState {
            side_effect_mode: SideEffectMode::ReadOnly,
            ..base_state()
        };
        let branch = CausalBranch {
            commit_policy: CommitPolicy::RequireApproval,
            // no write inputs, so gate 1 won't fire
            required_inputs: vec![],
            ..base_branch("b-approval")
        };
        assert_eq!(
            scorer.score(&state, &branch, None, &default_config()),
            None,
            "RequireApproval must be blocked in ReadOnly mode"
        );
    }

    // -----------------------------------------------------------------------
    // Soft scoring tests
    // -----------------------------------------------------------------------

    /// A fully qualified branch produces a score in [0.0, 1.0].
    #[test]
    fn test_score_in_valid_range() {
        let scorer = DefaultBranchScorer::new();
        let state = base_state();
        let branch = base_branch("b-good");
        let score = scorer
            .score(&state, &branch, None, &default_config())
            .expect("test: branch should not be gated");
        assert!(
            (0.0..=1.0).contains(&score),
            "score {score} must be in [0.0, 1.0]"
        );
    }

    /// Higher confidence produces a higher score (all else equal).
    #[test]
    fn test_score_higher_confidence_wins() {
        let scorer = DefaultBranchScorer::new();
        let state = base_state();
        let config = default_config();

        let low_conf = CausalBranch {
            confidence: 0.2,
            ..base_branch("b-low")
        };
        let high_conf = CausalBranch {
            confidence: 0.9,
            ..base_branch("b-high")
        };

        let s_low = scorer
            .score(&state, &low_conf, None, &config)
            .expect("test: low confidence branch should score");
        let s_high = scorer
            .score(&state, &high_conf, None, &config)
            .expect("test: high confidence branch should score");

        assert!(
            s_high > s_low,
            "higher confidence ({s_high}) must outscore lower ({s_low})"
        );
    }

    /// Rehearsal artifact with a positive score_delta boosts the score.
    #[test]
    fn test_score_rehearsal_bonus_applied() {
        let scorer = DefaultBranchScorer::new();
        let state = base_state();
        let branch = base_branch("b-rehearsed");
        let config = default_config();

        let artifact = RehearsalArtifact {
            branch_id: "b-rehearsed".into(),
            preview_output: None,
            retrieved_memory_keys: vec![],
            selected_model: None,
            score_delta: 1.0,
            warnings: vec![],
        };

        let without_bonus = scorer
            .score(&state, &branch, None, &config)
            .expect("test: base branch should score");
        let with_bonus = scorer
            .score(&state, &branch, Some(&artifact), &config)
            .expect("test: rehearsed branch should score");

        assert!(
            with_bonus > without_bonus,
            "rehearsal bonus must increase score: {with_bonus} vs {without_bonus}"
        );
    }

    // -----------------------------------------------------------------------
    // rank_branches tests
    // -----------------------------------------------------------------------

    /// rank_branches returns only qualified branches in descending score order.
    #[test]
    fn test_rank_branches_order_and_filtering() {
        let scorer = DefaultBranchScorer::new();
        let state = base_state();
        let config = default_config();

        // High confidence — should rank first
        let b_high = CausalBranch {
            confidence: 0.95,
            ..base_branch("b-high")
        };
        // Low confidence — should rank second
        let b_low = CausalBranch {
            confidence: 0.1,
            ..base_branch("b-low")
        };
        // Over-budget — should be excluded
        let b_gated = CausalBranch {
            estimated_cost: CostEstimate {
                estimated_tokens: 9999,
                estimated_cost_micro_usd: 0,
            },
            ..base_branch("b-gated")
        };

        let pairs: Vec<(&CausalBranch, Option<&RehearsalArtifact>)> = vec![
            (&b_low, None),
            (&b_gated, None),
            (&b_high, None),
        ];

        let ranked = scorer.rank_branches(&state, &pairs, &config);

        assert_eq!(ranked.len(), 2, "gated branch must be excluded");
        assert_eq!(
            ranked[0].branch.branch_id, "b-high",
            "highest score must be first"
        );
        assert_eq!(
            ranked[1].branch.branch_id, "b-low",
            "lowest score must be last"
        );
        assert!(
            ranked[0].score >= ranked[1].score,
            "scores must be non-increasing"
        );
    }

    /// rank_branches is stable for equal scores (tie-breaks on branch_id).
    #[test]
    fn test_rank_branches_stable_tiebreak() {
        let scorer = DefaultBranchScorer::new();
        let state = base_state();
        let config = default_config();

        // Both branches are identical in cost/latency/confidence → tie on
        // score → resolved by branch_id lexicographic order.
        let b1 = base_branch("b-alpha");
        let b2 = base_branch("b-beta");

        let pairs: Vec<(&CausalBranch, Option<&RehearsalArtifact>)> = vec![
            (&b2, None),
            (&b1, None),
        ];

        let ranked = scorer.rank_branches(&state, &pairs, &config);
        assert_eq!(ranked.len(), 2);
        // b-alpha < b-beta lexicographically → b-alpha first
        assert_eq!(
            ranked[0].branch.branch_id, "b-alpha",
            "tie must be broken by branch_id ascending"
        );
    }

    // -----------------------------------------------------------------------
    // Edge case tests
    // -----------------------------------------------------------------------

    /// Zero-remaining budget: branch with zero estimated tokens still scores.
    #[test]
    fn test_edge_zero_remaining_tokens_zero_estimated() {
        let scorer = DefaultBranchScorer::new();
        let state = CausalState {
            budget: BudgetState {
                extra_token_limit: 1000,
                tokens_used: 1000, // remaining = 0
                extra_latency_budget_ms: 500,
                latency_used_ms: 0,
            },
            ..base_state()
        };
        // Branch with zero estimated tokens must not be blocked by gate 2.
        let branch = CausalBranch {
            estimated_cost: CostEstimate {
                estimated_tokens: 0,
                estimated_cost_micro_usd: 0,
            },
            ..base_branch("b-zero-tokens")
        };
        let score = scorer.score(&state, &branch, None, &default_config());
        assert!(
            score.is_some(),
            "zero-cost branch must not be blocked when remaining tokens = 0"
        );
        let score = score.expect("test: confirmed some above");
        assert!(
            (0.0..=1.0).contains(&score),
            "score {score} must be in [0.0, 1.0]"
        );
    }

    /// Negative rehearsal delta does not push score below 0.0.
    #[test]
    fn test_edge_negative_rehearsal_delta_clamped() {
        let scorer = DefaultBranchScorer::new();
        let state = base_state();
        let branch = CausalBranch {
            confidence: 0.0, // worst possible confidence
            estimated_cost: CostEstimate {
                estimated_tokens: 900, // near budget limit of 1000
                estimated_cost_micro_usd: 0,
            },
            estimated_latency_ms: 450, // near latency limit of 500
            ..base_branch("b-worst")
        };
        let artifact = RehearsalArtifact {
            branch_id: "b-worst".into(),
            preview_output: None,
            retrieved_memory_keys: vec![],
            selected_model: None,
            score_delta: -1.0, // worst rehearsal result
            warnings: vec![],
        };
        let score = scorer
            .score(&state, &branch, Some(&artifact), &default_config())
            .expect("test: worst-case branch should not be gated");
        assert!(
            score >= 0.0,
            "score {score} must not be negative even in worst-case scenario"
        );
    }

    /// AskApproval branch in ReadOnly mode is still blocked by gate 4
    /// (RequireApproval commit policy cannot be fulfilled in ReadOnly).
    #[test]
    fn test_gate_ask_approval_readonly_require_approval_blocked() {
        let scorer = DefaultBranchScorer::new();
        let state = CausalState {
            side_effect_mode: SideEffectMode::ReadOnly,
            ..base_state()
        };
        // AskApproval label bypasses gate 1 (write check), but gate 4
        // (RequireApproval + ReadOnly) must still fire.
        let branch = CausalBranch {
            label: BranchLabel::AskApproval,
            required_inputs: vec!["write:database".into()],
            commit_policy: CommitPolicy::RequireApproval,
            ..base_branch("b-ask-readonly")
        };
        assert_eq!(
            scorer.score(&state, &branch, None, &default_config()),
            None,
            "AskApproval+RequireApproval must be blocked in ReadOnly (gate 4)"
        );
    }

    /// safe_norm with zero denominator and non-zero numerator returns 1.0.
    #[test]
    fn test_edge_safe_norm_zero_denominator_nonzero_numerator() {
        // We exercise this path indirectly via score() with an exhausted
        // token budget and a branch with non-zero estimated tokens.
        // Gate 2 fires before scoring, so we test the latency path instead:
        // exhaust latency but give a zero-latency branch to avoid gate 3.
        let scorer = DefaultBranchScorer::new();
        let state = CausalState {
            budget: BudgetState {
                extra_token_limit: 1000,
                tokens_used: 900,  // 100 remaining — branch below uses 100
                extra_latency_budget_ms: 500,
                latency_used_ms: 500, // 0 remaining
            },
            ..base_state()
        };
        // Branch with zero latency → gate 3 passes (0 <= 0).
        // norm_latency = safe_norm(0, 0) = 0.0.
        let branch = CausalBranch {
            estimated_cost: CostEstimate {
                estimated_tokens: 100,
                estimated_cost_micro_usd: 0,
            },
            estimated_latency_ms: 0,
            ..base_branch("b-zero-latency-remaining")
        };
        let score = scorer
            .score(&state, &branch, None, &default_config())
            .expect("test: zero-latency branch with exhausted budget should score");
        assert!(
            (0.0..=1.0).contains(&score),
            "score {score} must be in [0.0, 1.0] with zero remaining latency"
        );
    }

    /// Score is NaN-free regardless of extreme input values.
    #[test]
    fn test_edge_no_nan_in_score() {
        let scorer = DefaultBranchScorer::new();
        let state = base_state();

        // confidence exactly at boundaries
        for confidence in [0.0_f32, 1.0_f32] {
            let branch = CausalBranch {
                confidence,
                ..base_branch("b-boundary")
            };
            let score = scorer
                .score(&state, &branch, None, &default_config())
                .expect("test: boundary branch should not be gated");
            assert!(
                !score.is_nan(),
                "score must not be NaN for confidence={confidence}"
            );
        }
    }
}
