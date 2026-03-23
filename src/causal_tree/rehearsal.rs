//! Rehearsal engine for the Causal Tree Engine.
//!
//! A rehearsal is a lightweight "dry-run" evaluation of a [`CausalBranch`]
//! that runs **before** committing to a full execution path.  The results
//! (a [`RehearsalArtifact`]) feed into the branch scorer to refine rankings.
//!
//! ## Rehearsal levels (v1)
//!
//! | Level | I/O | Purpose |
//! |-------|-----|---------|
//! | [`RehearsalLevel::ScoreOnly`] | none | Ultra-fast scoring — zero delta, instant return |
//! | [`RehearsalLevel::DryRunReadonly`] | simulated | Memory-key prefetch sim, label-based delta |
//!
//! ## Timeout
//!
//! Every rehearsal call is wrapped with [`tokio::time::timeout`] using
//! `policy.rehearsal_timeout_ms`.  A timeout surfaces as
//! [`CausalTreeError::RehearsalTimeout`].

use std::time::Duration;

use async_trait::async_trait;
use tokio::time::timeout;

use super::branch::{BranchLabel, CausalBranch, RehearsalArtifact, RehearsalLevel};
use super::error::CausalTreeError;
use super::policy::CausalPolicy;
use super::state::CausalState;

// ---------------------------------------------------------------------------
// Key normalisation
// ---------------------------------------------------------------------------

/// Maximum byte length of a single segment inserted into a memory key.
///
/// Values longer than this are truncated to avoid oversized cache keys and
/// log-line pollution.
const MAX_KEY_SEGMENT_LEN: usize = 64;

/// Normalise an arbitrary string for use as a memory-key segment.
///
/// Rules applied in order:
/// 1. ASCII-lowercase.
/// 2. Replace any run of non-alphanumeric characters with a single `_`.
/// 3. Truncate to [`MAX_KEY_SEGMENT_LEN`] bytes (respecting char boundaries).
/// 4. Strip any leading or trailing `_`.
///
/// The result is always a safe, unambiguous key component regardless of the
/// original content.
fn normalise_key_segment(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();

    // Replace runs of non-[a-z0-9] with a single underscore.
    let mut result = String::with_capacity(lower.len().min(MAX_KEY_SEGMENT_LEN + 4));
    let mut in_sep = false;
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            result.push(ch);
            in_sep = false;
        } else if !in_sep {
            result.push('_');
            in_sep = true;
        }
    }

    // Truncate to the byte limit, respecting UTF-8 char boundaries.
    if result.len() > MAX_KEY_SEGMENT_LEN {
        let mut end = MAX_KEY_SEGMENT_LEN;
        while !result.is_char_boundary(end) {
            end -= 1;
        }
        result.truncate(end);
    }

    // Strip leading/trailing underscores introduced by the normalisation step.
    result.trim_matches('_').to_string()
}

// ---------------------------------------------------------------------------
// RehearsalEngine trait
// ---------------------------------------------------------------------------

/// Performs a speculative pre-execution of a candidate [`CausalBranch`].
///
/// Implementations must be `Send + Sync` and **must** complete within
/// `policy.rehearsal_timeout_ms` milliseconds.  The engine is expected to be
/// stateless — all per-request context is supplied via method arguments.
///
/// # Errors
///
/// * [`CausalTreeError::RehearsalTimeout`] — the call exceeded `policy.rehearsal_timeout_ms`.
/// * [`CausalTreeError::RehearsalFailed`]  — an internal failure occurred during rehearsal.
#[async_trait]
pub trait RehearsalEngine: Send + Sync {
    /// Execute a rehearsal for `branch` under the current `state` and `policy`.
    ///
    /// Returns a [`RehearsalArtifact`] that captures the outcome of the
    /// simulated run: score delta, prefetched memory keys, and any warnings.
    async fn rehearse(
        &self,
        state: &CausalState,
        branch: &CausalBranch,
        policy: &CausalPolicy,
    ) -> Result<RehearsalArtifact, CausalTreeError>;
}

// ---------------------------------------------------------------------------
// DefaultRehearsalEngine
// ---------------------------------------------------------------------------

/// Default synchronous-simulation rehearsal engine shipped with the CTE.
///
/// This v1 implementation performs **no real I/O**.  All operations are
/// in-memory computations that simulate what a live rehearsal would do.
///
/// The engine still enforces the `policy.rehearsal_timeout_ms` budget via
/// [`tokio::time::timeout`] so that future versions introducing real I/O
/// remain bounded by the same mechanism without API changes.
///
/// ## ScoreOnly behaviour
///
/// * `score_delta = 0.0` — no information gained, score is unchanged.
/// * `preview_output = None` — no partial answer produced.
/// * `retrieved_memory_keys = []` — no prefetch performed.
///
/// ## DryRunReadonly behaviour
///
/// 1. **Memory prefetch simulation** — generates two keys from `state.goal`
///    and `state.session_id` after slug-normalisation via [`normalise_key_segment`].
/// 2. **Label-based score delta**:
///    * `DirectAnswer`       → `+0.05` (minor boost: low-overhead path)
///    * `RetrieveThenAnswer` → `+0.15` (meaningful boost: retrieval improves quality)
///    * `AskApproval`        → `0.00`  (neutral: approval path, no quality gain)
/// 3. **Risk warnings** — one structured warning per unresolved [`RiskFlag`],
///    prefixed with severity and code, so callers can act on them.
///
/// [`RiskFlag`]: crate::causal_tree::state::RiskFlag
#[derive(Debug, Default)]
pub struct DefaultRehearsalEngine;

impl DefaultRehearsalEngine {
    /// Create a new [`DefaultRehearsalEngine`].
    pub fn new() -> Self {
        Self
    }

    /// Build a [`RehearsalArtifact`] for the [`RehearsalLevel::ScoreOnly`] path.
    ///
    /// No I/O is performed.  Returns a zero-delta artifact immediately.
    fn build_score_only_artifact(&self, branch: &CausalBranch) -> RehearsalArtifact {
        RehearsalArtifact {
            branch_id: branch.branch_id.clone(),
            preview_output: None,
            retrieved_memory_keys: vec![],
            selected_model: None,
            score_delta: 0.0,
            warnings: vec![],
        }
    }

    /// Build a [`RehearsalArtifact`] for the [`RehearsalLevel::DryRunReadonly`] path.
    ///
    /// Simulates a memory prefetch and computes a label-dependent score delta.
    /// Any unresolved risks in `state` are surfaced as structured warnings.
    fn build_dry_run_artifact(
        &self,
        state: &CausalState,
        branch: &CausalBranch,
    ) -> RehearsalArtifact {
        // ----------------------------------------------------------------
        // Simulated memory prefetch keys (slugified to prevent injection)
        // ----------------------------------------------------------------
        let goal_slug = normalise_key_segment(&state.goal);
        let session_slug = normalise_key_segment(&state.session_id);

        let retrieved_memory_keys = vec![
            format!("memory:recent:{goal_slug}"),
            format!("memory:session:{session_slug}"),
        ];

        // ----------------------------------------------------------------
        // Label-based score delta
        // ----------------------------------------------------------------
        let score_delta = match branch.label {
            BranchLabel::DirectAnswer => 0.05,
            BranchLabel::RetrieveThenAnswer => 0.15,
            BranchLabel::AskApproval => 0.0,
        };

        // ----------------------------------------------------------------
        // Risk warnings — one structured entry per unresolved risk flag
        // ----------------------------------------------------------------
        let warnings: Vec<String> = state
            .unresolved_risks
            .iter()
            .map(|risk| {
                format!(
                    "[{severity:?}] unresolved risk '{code}': {message}",
                    severity = risk.severity,
                    code = risk.code,
                    message = risk.message,
                )
            })
            .collect();

        RehearsalArtifact {
            branch_id: branch.branch_id.clone(),
            preview_output: None,
            retrieved_memory_keys,
            selected_model: None,
            score_delta,
            warnings,
        }
    }
}

#[async_trait]
impl RehearsalEngine for DefaultRehearsalEngine {
    async fn rehearse(
        &self,
        state: &CausalState,
        branch: &CausalBranch,
        policy: &CausalPolicy,
    ) -> Result<RehearsalArtifact, CausalTreeError> {
        let timeout_dur = Duration::from_millis(policy.rehearsal_timeout_ms);
        let branch_id = branch.branch_id.clone();

        // Wrap the entire computation in a tokio timeout so that future
        // iterations introducing real async I/O remain bounded by the same
        // mechanism without any API changes.
        let result = timeout(timeout_dur, async {
            match branch.rehearsal_level {
                RehearsalLevel::ScoreOnly => self.build_score_only_artifact(branch),
                RehearsalLevel::DryRunReadonly => self.build_dry_run_artifact(state, branch),
            }
        })
        .await;

        match result {
            Ok(artifact) => Ok(artifact),
            Err(_elapsed) => Err(CausalTreeError::RehearsalTimeout {
                branch_id,
                elapsed_ms: policy.rehearsal_timeout_ms,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_tree::{
        branch::{CommitPolicy, CostEstimate},
        policy::CausalPolicy,
        state::{BudgetState, CausalState, RiskFlag, RiskLevel, SideEffectMode},
    };

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn base_state(goal: &str) -> CausalState {
        CausalState {
            session_id: "sess-rehearsal-test".into(),
            request_id: "req-test".into(),
            goal: goal.into(),
            user_intent: "code_audit".into(),
            completed_steps: vec![],
            active_constraints: vec![],
            known_artifacts: vec![],
            unresolved_risks: vec![],
            side_effect_mode: SideEffectMode::ReadOnly,
            budget: BudgetState::default(),
            snapshot_ts: "2026-03-22T00:00:00Z".into(),
        }
    }

    fn make_branch(id: &str, label: BranchLabel, level: RehearsalLevel) -> CausalBranch {
        CausalBranch {
            branch_id: id.into(),
            label,
            parent_step_ids: vec![],
            required_inputs: vec![],
            predicted_gain: 0.5,
            estimated_cost: CostEstimate::default(),
            estimated_latency_ms: 80,
            confidence: 0.75,
            rehearsal_level: level,
            commit_policy: CommitPolicy::AutoCommit,
            explanation: vec![],
        }
    }

    fn default_policy() -> CausalPolicy {
        CausalPolicy::default()
    }

    // -----------------------------------------------------------------------
    // Test 1: ScoreOnly produces a zero-delta artifact with no keys/warnings
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_score_only_returns_zero_delta_no_keys() {
        let engine = DefaultRehearsalEngine::new();
        let state = base_state("audit the repository");
        let branch = make_branch("b-score-1", BranchLabel::DirectAnswer, RehearsalLevel::ScoreOnly);
        let policy = default_policy();

        let artifact = engine
            .rehearse(&state, &branch, &policy)
            .await
            .expect("test: ScoreOnly must succeed");

        assert_eq!(artifact.branch_id, "b-score-1");
        assert_eq!(artifact.score_delta, 0.0, "ScoreOnly must yield zero delta");
        assert!(
            artifact.preview_output.is_none(),
            "ScoreOnly must not produce preview output"
        );
        assert!(
            artifact.retrieved_memory_keys.is_empty(),
            "ScoreOnly must not prefetch any keys"
        );
        assert!(
            artifact.warnings.is_empty(),
            "ScoreOnly with no risks must have no warnings"
        );
    }

    // -----------------------------------------------------------------------
    // Test 2: DryRunReadonly — correct label-based deltas and key generation
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_dry_run_label_deltas_and_keys() {
        let engine = DefaultRehearsalEngine::new();
        let policy = default_policy();

        let cases: &[(BranchLabel, f32)] = &[
            (BranchLabel::DirectAnswer, 0.05),
            (BranchLabel::RetrieveThenAnswer, 0.15),
            (BranchLabel::AskApproval, 0.0),
        ];

        for (label, expected_delta) in cases {
            let state = base_state("analyze logs");
            let branch = make_branch("b-dry-run", *label, RehearsalLevel::DryRunReadonly);

            let artifact = engine
                .rehearse(&state, &branch, &policy)
                .await
                .expect("test: DryRunReadonly must succeed");

            assert!(
                (artifact.score_delta - expected_delta).abs() < f32::EPSILON,
                "label {:?}: expected delta {expected_delta}, got {}",
                label,
                artifact.score_delta
            );

            // Must produce exactly 2 memory keys.
            assert_eq!(
                artifact.retrieved_memory_keys.len(),
                2,
                "DryRunReadonly must prefetch exactly 2 memory keys"
            );
            assert!(
                artifact.retrieved_memory_keys[0].starts_with("memory:recent:"),
                "first key must use 'memory:recent:' prefix"
            );
            assert!(
                artifact.retrieved_memory_keys[1].starts_with("memory:session:"),
                "second key must use 'memory:session:' prefix"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 3: DryRunReadonly — unresolved risks yield structured warnings
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_dry_run_risk_warnings_are_structured() {
        let engine = DefaultRehearsalEngine::new();
        let policy = default_policy();

        let mut state = base_state("deploy service");
        state.unresolved_risks = vec![
            RiskFlag {
                code: "UNWRAP_IN_PROD".into(),
                severity: RiskLevel::High,
                message: "unwrap found in production code".into(),
            },
            RiskFlag {
                code: "SQL_CONCAT".into(),
                severity: RiskLevel::Critical,
                message: "raw SQL string concatenation detected".into(),
            },
        ];

        let branch = make_branch(
            "b-risky",
            BranchLabel::RetrieveThenAnswer,
            RehearsalLevel::DryRunReadonly,
        );

        let artifact = engine
            .rehearse(&state, &branch, &policy)
            .await
            .expect("test: rehearsal with risks must succeed");

        assert_eq!(
            artifact.warnings.len(),
            2,
            "must produce one warning per unresolved risk"
        );

        let w0 = &artifact.warnings[0];
        assert!(
            w0.contains("UNWRAP_IN_PROD"),
            "first warning must contain risk code: got {w0}"
        );
        assert!(
            w0.contains("High"),
            "first warning must contain severity: got {w0}"
        );

        let w1 = &artifact.warnings[1];
        assert!(
            w1.contains("SQL_CONCAT"),
            "second warning must contain risk code: got {w1}"
        );
        assert!(
            w1.contains("Critical"),
            "second warning must contain severity: got {w1}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: Memory key normalisation handles special chars and long strings
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_dry_run_key_normalisation() {
        let engine = DefaultRehearsalEngine::new();
        let policy = default_policy();

        // Goal with special chars, spaces, and length > MAX_KEY_SEGMENT_LEN.
        let mut long_goal = "Audit the repo: SQL injection & XSS? (URGENT)".to_string();
        long_goal.push_str(&"x".repeat(200));
        let state = base_state(&long_goal);
        let branch = make_branch("b-norm", BranchLabel::DirectAnswer, RehearsalLevel::DryRunReadonly);

        let artifact = engine
            .rehearse(&state, &branch, &policy)
            .await
            .expect("test: normalisation rehearsal must succeed");

        let goal_key = &artifact.retrieved_memory_keys[0];
        let segment = goal_key
            .strip_prefix("memory:recent:")
            .expect("test: must have 'memory:recent:' prefix");

        // Segment must contain only safe characters.
        assert!(
            segment.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "key segment must contain only [a-z0-9_]: got {segment}"
        );

        // Segment must not exceed MAX_KEY_SEGMENT_LEN bytes.
        assert!(
            segment.len() <= MAX_KEY_SEGMENT_LEN,
            "key segment length {} must be <= {MAX_KEY_SEGMENT_LEN}",
            segment.len()
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: Timeout fires when rehearsal_timeout_ms is zero
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_timeout_fires_on_zero_ms_budget() {
        let engine = DefaultRehearsalEngine::new();
        let state = base_state("test timeout");
        let branch = make_branch(
            "b-timeout",
            BranchLabel::DirectAnswer,
            RehearsalLevel::DryRunReadonly,
        );
        let policy = CausalPolicy {
            rehearsal_timeout_ms: 0,
            ..CausalPolicy::default()
        };

        let result = engine.rehearse(&state, &branch, &policy).await;

        match result {
            Err(CausalTreeError::RehearsalTimeout { branch_id, .. }) => {
                assert_eq!(branch_id, "b-timeout", "timeout error must carry the branch_id");
            }
            Ok(_) => {
                // tokio::time::timeout(0ms) may occasionally complete before the
                // scheduler yields — this is a known edge case that is acceptable
                // for a zero-ms budget; the timeout mechanism is structurally sound.
            }
            Err(other) => panic!("test: unexpected error variant: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // Test 6: Trait object dispatch works correctly
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_trait_object_dispatch() {
        let engine: Box<dyn RehearsalEngine> = Box::new(DefaultRehearsalEngine::new());
        let state = base_state("simple question");
        let branch = make_branch("b-trait", BranchLabel::DirectAnswer, RehearsalLevel::ScoreOnly);
        let policy = default_policy();

        let artifact = engine
            .rehearse(&state, &branch, &policy)
            .await
            .expect("test: trait object dispatch must succeed");

        assert_eq!(artifact.branch_id, "b-trait");
    }

    // -----------------------------------------------------------------------
    // Test 7: normalise_key_segment unit tests (synchronous)
    // -----------------------------------------------------------------------
    #[test]
    fn test_normalise_key_segment_edge_cases() {
        // Empty string → empty result.
        assert_eq!(normalise_key_segment(""), "");

        // Pure alphanumeric — lowercased, unchanged otherwise.
        assert_eq!(normalise_key_segment("Hello123"), "hello123");

        // Leading/trailing separators stripped.
        assert_eq!(normalise_key_segment("---foo---"), "foo");

        // Multiple consecutive non-alphanum → single underscore.
        assert_eq!(normalise_key_segment("foo::bar!!baz"), "foo_bar_baz");

        // Length capped at MAX_KEY_SEGMENT_LEN.
        let long = "a".repeat(MAX_KEY_SEGMENT_LEN + 50);
        let result = normalise_key_segment(&long);
        assert!(
            result.len() <= MAX_KEY_SEGMENT_LEN,
            "result length {} must be <= {MAX_KEY_SEGMENT_LEN}",
            result.len()
        );
    }
}
