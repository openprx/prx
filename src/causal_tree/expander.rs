use async_trait::async_trait;

use super::branch::{BranchLabel, CausalBranch, CommitPolicy, CostEstimate, RehearsalLevel};
use super::error::CausalTreeError;
use super::policy::CausalPolicy;
use super::state::{CausalState, RiskLevel, SideEffectMode};

// ---------------------------------------------------------------------------
// Intent classification helpers
// ---------------------------------------------------------------------------

/// Keywords that indicate a simple / direct-answer intent.
///
/// Each keyword is matched as a whole word (preceded and followed by a
/// non-alphanumeric boundary) to avoid false positives such as "hi" matching
/// inside "this" or "while".
const SIMPLE_KEYWORDS: &[&str] = &["simple", "qa", "question", "hello", "hi"];

/// Keywords that indicate a retrieval-based intent.
const RETRIEVAL_KEYWORDS: &[&str] = &["audit", "review", "analyze", "search", "find"];

/// Returns `true` if `intent` (case-insensitive) contains any of the given
/// keywords as whole words.
///
/// Whole-word matching is performed by checking that each occurrence of the
/// keyword in the string is preceded and followed by a non-alphanumeric
/// character (or is at the start/end of the string).
fn intent_contains_any(intent: &str, keywords: &[&str]) -> bool {
    let lower = intent.to_lowercase();
    keywords.iter().any(|kw| {
        // Find every position where the keyword starts inside `lower`.
        let kw_lower = kw.to_lowercase();
        let kw_bytes = kw_lower.as_bytes();
        let text_bytes = lower.as_bytes();
        let klen = kw_bytes.len();

        if klen == 0 || klen > text_bytes.len() {
            return false;
        }

        text_bytes.windows(klen).enumerate().any(|(i, window)| {
            if window != kw_bytes {
                return false;
            }
            // Check left boundary: start of string or non-alphanumeric.
            let left_ok = i == 0
                || text_bytes
                    .get(i - 1)
                    .map(|b| !b.is_ascii_alphanumeric())
                    .unwrap_or(true);
            // Check right boundary: end of string or non-alphanumeric.
            let right_ok = i + klen == text_bytes.len()
                || text_bytes
                    .get(i + klen)
                    .map(|b| !b.is_ascii_alphanumeric())
                    .unwrap_or(true);
            left_ok && right_ok
        })
    })
}

// ---------------------------------------------------------------------------
// TreeExpander trait
// ---------------------------------------------------------------------------

/// Expands a [`CausalState`] into a ranked set of candidate [`CausalBranch`]es
/// according to the provided [`CausalPolicy`].
///
/// Implementations are expected to be stateless and cheap to clone — all
/// per-request context is passed via the arguments to [`expand`].
#[async_trait]
pub trait TreeExpander: Send + Sync {
    /// Produce up to `policy.max_branches` candidate branches for the given state.
    ///
    /// Returns [`CausalTreeError::ExpansionEmpty`] when no viable branch can be
    /// constructed, or [`CausalTreeError::ExpansionFailed`] on internal errors.
    async fn expand(&self, state: &CausalState, policy: &CausalPolicy) -> Result<Vec<CausalBranch>, CausalTreeError>;
}

// ---------------------------------------------------------------------------
// DefaultTreeExpander
// ---------------------------------------------------------------------------

/// Rule-based branch expander — no LLM calls, pure deterministic logic.
///
/// Decision priority (first match wins):
/// 1. Simple / conversational intent → [`BranchLabel::DirectAnswer`] (confidence 0.85)
/// 2. Retrieval intent → [`BranchLabel::RetrieveThenAnswer`] (confidence 0.80)
/// 3. Non-read-only side effects with High/Critical risk →
///    [`BranchLabel::AskApproval`] (confidence 0.75) + optional
///    [`BranchLabel::RetrieveThenAnswer`]
/// 4. Default → [`BranchLabel::DirectAnswer`] (0.60) +
///    [`BranchLabel::RetrieveThenAnswer`] (0.70)
///
/// Mutual-exclusion invariants enforced:
/// - `AskApproval` and `DirectAnswer` never appear together.
/// - When `AskApproval` is present, at most one `RetrieveThenAnswer` is added.
#[derive(Debug, Default)]
pub struct DefaultTreeExpander {
    /// Monotonically-increasing counter used to generate unique branch IDs.
    /// Wrapped in an atomic to keep the expander `Sync`.
    seq: std::sync::atomic::AtomicU64,
}

impl DefaultTreeExpander {
    /// Create a new expander with its sequence counter reset to zero.
    pub const fn new() -> Self {
        Self {
            seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Allocate the next sequence number (relaxed ordering — ID uniqueness
    /// within a single request is sufficient; no cross-thread synchronisation
    /// of the counter value is required).
    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Build a branch ID from a label and the current sequence value.
    fn make_branch_id(&self, label: BranchLabel) -> String {
        format!("b-{}-{}", label.as_str(), self.next_seq())
    }

    /// Construct a [`CausalBranch`] for a direct answer.
    fn direct_answer_branch(&self, state: &CausalState, confidence: f32, explanation: Vec<String>) -> CausalBranch {
        let parent_ids = state.completed_steps.iter().map(|s| s.step_id.clone()).collect();

        CausalBranch {
            branch_id: self.make_branch_id(BranchLabel::DirectAnswer),
            label: BranchLabel::DirectAnswer,
            parent_step_ids: parent_ids,
            required_inputs: vec![],
            predicted_gain: confidence * 0.9,
            estimated_cost: CostEstimate {
                estimated_tokens: 512,
                estimated_cost_micro_usd: 10,
            },
            estimated_latency_ms: 80,
            confidence,
            rehearsal_level: RehearsalLevel::ScoreOnly,
            commit_policy: CommitPolicy::AutoCommit,
            explanation,
        }
    }

    /// Construct a [`CausalBranch`] for retrieve-then-answer.
    fn retrieve_then_answer_branch(
        &self,
        state: &CausalState,
        confidence: f32,
        explanation: Vec<String>,
    ) -> CausalBranch {
        let parent_ids = state.completed_steps.iter().map(|s| s.step_id.clone()).collect();

        CausalBranch {
            branch_id: self.make_branch_id(BranchLabel::RetrieveThenAnswer),
            label: BranchLabel::RetrieveThenAnswer,
            parent_step_ids: parent_ids,
            required_inputs: vec!["memory:recent".to_string(), "artifacts:known".to_string()],
            predicted_gain: confidence * 0.95,
            estimated_cost: CostEstimate {
                estimated_tokens: 1024,
                estimated_cost_micro_usd: 25,
            },
            estimated_latency_ms: 200,
            confidence,
            rehearsal_level: RehearsalLevel::DryRunReadonly,
            commit_policy: CommitPolicy::AutoCommit,
            explanation,
        }
    }

    /// Construct a [`CausalBranch`] for ask-approval.
    fn ask_approval_branch(&self, state: &CausalState, confidence: f32, explanation: Vec<String>) -> CausalBranch {
        let parent_ids = state.completed_steps.iter().map(|s| s.step_id.clone()).collect();

        CausalBranch {
            branch_id: self.make_branch_id(BranchLabel::AskApproval),
            label: BranchLabel::AskApproval,
            parent_step_ids: parent_ids,
            required_inputs: vec!["user:approval".to_string()],
            predicted_gain: confidence * 0.85,
            estimated_cost: CostEstimate {
                estimated_tokens: 256,
                estimated_cost_micro_usd: 5,
            },
            estimated_latency_ms: 50,
            confidence,
            rehearsal_level: RehearsalLevel::ScoreOnly,
            commit_policy: CommitPolicy::RequireApproval,
            explanation,
        }
    }

    /// Returns `true` when the state signals high/critical risk with non-readonly
    /// side effects — the gating condition for the `AskApproval` path.
    fn requires_approval(state: &CausalState) -> bool {
        if state.side_effect_mode == SideEffectMode::ReadOnly {
            return false;
        }
        matches!(state.max_risk_level(), Some(RiskLevel::High | RiskLevel::Critical))
    }
}

#[async_trait]
impl TreeExpander for DefaultTreeExpander {
    async fn expand(&self, state: &CausalState, policy: &CausalPolicy) -> Result<Vec<CausalBranch>, CausalTreeError> {
        let intent = state.user_intent.as_str();
        let mut branches: Vec<CausalBranch> = Vec::with_capacity(policy.max_branches);

        // Rule 3 is checked FIRST as a hard safety gate: even if the intent
        // matches a simple or retrieval pattern, a non-readonly session with
        // High/Critical risk must route through user approval.
        if Self::requires_approval(state) {
            // --- Rule 3: non-readonly + high/critical risk (highest priority) ---
            let risk_desc = state
                .max_risk_level()
                .map(|r| format!("{r:?}"))
                .unwrap_or_else(|| "unknown".to_string());

            let approval_branch = self.ask_approval_branch(
                state,
                0.75,
                vec![
                    format!(
                        "Side-effect mode is {:?} and max risk level is {}; \
                         user approval is mandatory before proceeding.",
                        state.side_effect_mode, risk_desc
                    ),
                    "Proceeding without approval would violate the safety policy.".to_string(),
                ],
            );
            branches.push(approval_branch);

            // Add at most one RetrieveThenAnswer companion (if budget allows).
            if policy.max_branches >= 2 {
                let rta_branch = self.retrieve_then_answer_branch(
                    state,
                    0.60,
                    vec![
                        "Retrieval branch added as a read-only companion to AskApproval.".to_string(),
                        "Will only be committed after the approval gate passes.".to_string(),
                    ],
                );
                branches.push(rta_branch);
            }
        } else if intent_contains_any(intent, SIMPLE_KEYWORDS) {
            // --- Rule 1: simple / conversational intent ---
            let branch = self.direct_answer_branch(
                state,
                0.85,
                vec![
                    format!(
                        "User intent '{}' matches simple/conversational keywords; \
                         direct answer is the most efficient path.",
                        intent
                    ),
                    "No retrieval or approval overhead required.".to_string(),
                ],
            );
            branches.push(branch);
        } else if intent_contains_any(intent, RETRIEVAL_KEYWORDS) {
            // --- Rule 2: retrieval / analysis intent ---
            let branch = self.retrieve_then_answer_branch(
                state,
                0.80,
                vec![
                    format!(
                        "User intent '{}' matches audit/review/search keywords; \
                         retrieval is required before answering.",
                        intent
                    ),
                    "Memory prefetch and artifact context will improve answer quality.".to_string(),
                ],
            );
            branches.push(branch);
        } else {
            // --- Rule 4: default — both DirectAnswer and RetrieveThenAnswer ---
            let da_branch = self.direct_answer_branch(
                state,
                0.60,
                vec![format!(
                    "No specific intent pattern detected for '{}'; \
                         DirectAnswer included as low-cost baseline.",
                    intent
                )],
            );
            let rta_branch = self.retrieve_then_answer_branch(
                state,
                0.70,
                vec![format!(
                    "No specific intent pattern detected for '{}'; \
                         RetrieveThenAnswer included for higher-quality fallback.",
                    intent
                )],
            );
            // Push higher-confidence branch first so truncation favours it.
            branches.push(rta_branch);
            branches.push(da_branch);
        }

        // Sort by descending confidence before enforcing the cap so that the
        // best candidates survive truncation.
        branches.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Enforce hard cap from policy.
        branches.truncate(policy.max_branches);

        if branches.is_empty() {
            return Err(CausalTreeError::ExpansionEmpty);
        }

        Ok(branches)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::needless_collect
)]
mod tests {
    use super::*;
    use crate::causal_tree::state::{BudgetState, RiskFlag};

    fn base_state(intent: &str, mode: SideEffectMode) -> CausalState {
        CausalState {
            session_id: "sess-test".into(),
            request_id: "req-test".into(),
            goal: "test goal".into(),
            user_intent: intent.into(),
            completed_steps: vec![],
            active_constraints: vec![],
            known_artifacts: vec![],
            unresolved_risks: vec![],
            side_effect_mode: mode,
            budget: BudgetState::default(),
            snapshot_ts: "2026-03-22T00:00:00Z".into(),
        }
    }

    fn state_with_risk(intent: &str, mode: SideEffectMode, severity: RiskLevel) -> CausalState {
        let mut s = base_state(intent, mode);
        s.unresolved_risks = vec![RiskFlag {
            code: "TEST_RISK".into(),
            severity,
            message: "test risk".into(),
        }];
        s
    }

    // -----------------------------------------------------------------------
    // Test 1: simple intent → single DirectAnswer branch, confidence 0.85
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_simple_intent_produces_direct_answer() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default();

        for intent in &["hello", "hi", "simple question", "qa check", "question about X"] {
            let state = base_state(intent, SideEffectMode::ReadOnly);
            let branches = expander
                .expand(&state, &policy)
                .await
                .expect("test: expand should succeed");

            assert_eq!(branches.len(), 1, "intent='{}' should yield 1 branch", intent);
            assert_eq!(
                branches[0].label,
                BranchLabel::DirectAnswer,
                "intent='{}' should yield DirectAnswer",
                intent
            );
            assert!(
                (branches[0].confidence - 0.85).abs() < f32::EPSILON,
                "confidence should be 0.85 for simple intent"
            );
            assert!(
                branches[0].commit_policy == CommitPolicy::AutoCommit,
                "DirectAnswer should auto-commit"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 2: retrieval intent → single RetrieveThenAnswer branch, confidence 0.80
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_retrieval_intent_produces_retrieve_then_answer() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default();

        for intent in &[
            "audit the repo",
            "review this PR",
            "analyze logs",
            "search for bugs",
            "find issues",
        ] {
            let state = base_state(intent, SideEffectMode::ReadOnly);
            let branches = expander
                .expand(&state, &policy)
                .await
                .expect("test: expand should succeed");

            assert_eq!(branches.len(), 1, "intent='{}' should yield 1 branch", intent);
            assert_eq!(
                branches[0].label,
                BranchLabel::RetrieveThenAnswer,
                "intent='{}' should yield RetrieveThenAnswer",
                intent
            );
            assert!(
                (branches[0].confidence - 0.80).abs() < f32::EPSILON,
                "confidence should be 0.80 for retrieval intent"
            );
            assert_eq!(branches[0].rehearsal_level, RehearsalLevel::DryRunReadonly);
        }
    }

    // -----------------------------------------------------------------------
    // Test 3: non-readonly + high risk → AskApproval + RetrieveThenAnswer
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_high_risk_non_readonly_produces_ask_approval() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default(); // max_branches = 3

        let state = state_with_risk("deploy service", SideEffectMode::GuardedWrite, RiskLevel::High);
        let branches = expander
            .expand(&state, &policy)
            .await
            .expect("test: expand should succeed");

        assert_eq!(branches.len(), 2, "should have AskApproval + RetrieveThenAnswer");

        let labels: Vec<BranchLabel> = branches.iter().map(|b| b.label).collect();
        assert!(
            labels.contains(&BranchLabel::AskApproval),
            "AskApproval must be present"
        );
        assert!(
            !labels.contains(&BranchLabel::DirectAnswer),
            "DirectAnswer must not coexist with AskApproval"
        );
        assert!(
            labels.contains(&BranchLabel::RetrieveThenAnswer),
            "RetrieveThenAnswer companion should be present"
        );

        let approval = branches
            .iter()
            .find(|b| b.label == BranchLabel::AskApproval)
            .expect("test: AskApproval branch must exist");
        assert!(
            (approval.confidence - 0.75).abs() < f32::EPSILON,
            "AskApproval confidence should be 0.75"
        );
        assert_eq!(approval.commit_policy, CommitPolicy::RequireApproval);
    }

    // -----------------------------------------------------------------------
    // Test 4: readonly mode + high risk → NO AskApproval (readonly blocks rule 3)
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_readonly_high_risk_does_not_trigger_approval() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default();

        let state = state_with_risk(
            "unknown task",
            SideEffectMode::ReadOnly, // ReadOnly → rule 3 skipped
            RiskLevel::Critical,
        );
        let branches = expander
            .expand(&state, &policy)
            .await
            .expect("test: expand should succeed");

        let labels: Vec<BranchLabel> = branches.iter().map(|b| b.label).collect();
        assert!(
            !labels.contains(&BranchLabel::AskApproval),
            "ReadOnly mode must not produce AskApproval"
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: default intent → DirectAnswer + RetrieveThenAnswer (sorted desc)
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_default_intent_produces_both_branches_sorted() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default();

        let state = base_state("write a poem", SideEffectMode::ReadOnly);
        let branches = expander
            .expand(&state, &policy)
            .await
            .expect("test: expand should succeed");

        assert_eq!(branches.len(), 2, "default path should yield 2 branches");
        let labels: Vec<BranchLabel> = branches.iter().map(|b| b.label).collect();
        assert!(labels.contains(&BranchLabel::DirectAnswer));
        assert!(labels.contains(&BranchLabel::RetrieveThenAnswer));

        let da = branches
            .iter()
            .find(|b| b.label == BranchLabel::DirectAnswer)
            .expect("test: DirectAnswer must exist");
        let rta = branches
            .iter()
            .find(|b| b.label == BranchLabel::RetrieveThenAnswer)
            .expect("test: RetrieveThenAnswer must exist");

        assert!((da.confidence - 0.60).abs() < f32::EPSILON);
        assert!((rta.confidence - 0.70).abs() < f32::EPSILON);

        // Sorted descending: RetrieveThenAnswer(0.70) must come before DirectAnswer(0.60).
        assert_eq!(
            branches[0].label,
            BranchLabel::RetrieveThenAnswer,
            "higher-confidence branch must be first after sort"
        );
        assert_eq!(branches[1].label, BranchLabel::DirectAnswer);
    }

    // -----------------------------------------------------------------------
    // Test 6: max_branches=1 cap keeps the highest-confidence branch
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_max_branches_cap_keeps_best() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy {
            max_branches: 1,
            ..CausalPolicy::default()
        };

        // Default intent would produce 2 (RTA:0.70 + DA:0.60), cap=1 must keep RTA.
        let state = base_state("write a poem", SideEffectMode::ReadOnly);
        let branches = expander
            .expand(&state, &policy)
            .await
            .expect("test: expand should succeed");

        assert_eq!(branches.len(), 1, "max_branches=1 cap must be enforced");
        assert_eq!(
            branches[0].label,
            BranchLabel::RetrieveThenAnswer,
            "max_branches=1 must keep the highest-confidence branch"
        );
    }

    // -----------------------------------------------------------------------
    // Test 7: branch IDs are unique across multiple expand calls
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_branch_ids_are_unique() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default();
        let state = base_state("write a poem", SideEffectMode::ReadOnly);

        let first = expander.expand(&state, &policy).await.expect("test: first expand");
        let second = expander.expand(&state, &policy).await.expect("test: second expand");

        let ids_first: Vec<&str> = first.iter().map(|b| b.branch_id.as_str()).collect();
        let ids_second: Vec<&str> = second.iter().map(|b| b.branch_id.as_str()).collect();

        for id in &ids_first {
            assert!(
                !ids_second.contains(id),
                "branch ID '{id}' should not repeat across expand calls"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 8: branch ID format matches "b-{label}-{seq}"
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_branch_id_format() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default();
        let state = base_state("hello world", SideEffectMode::ReadOnly);

        let branches = expander
            .expand(&state, &policy)
            .await
            .expect("test: expand should succeed");

        for branch in &branches {
            let id = &branch.branch_id;
            assert!(id.starts_with("b-"), "branch_id '{}' must start with 'b-'", id);
            // Format: b-<label>-<seq>  — at least two '-' separators
            let parts: Vec<&str> = id.splitn(3, '-').collect();
            assert_eq!(parts.len(), 3, "branch_id '{}' must have format b-label-seq", id);
            // Last segment must be a valid integer
            parts[2].parse::<u64>().expect("test: seq segment must be a number");
        }
    }

    // -----------------------------------------------------------------------
    // Test 9: critical risk with ApprovalRequired mode → AskApproval
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_critical_risk_approval_required_mode() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default();

        let state = state_with_risk(
            "execute deployment",
            SideEffectMode::ApprovalRequired,
            RiskLevel::Critical,
        );
        let branches = expander
            .expand(&state, &policy)
            .await
            .expect("test: expand should succeed");

        let labels: Vec<BranchLabel> = branches.iter().map(|b| b.label).collect();
        assert!(
            labels.contains(&BranchLabel::AskApproval),
            "Critical risk with ApprovalRequired must produce AskApproval"
        );
        assert!(
            !labels.contains(&BranchLabel::DirectAnswer),
            "DirectAnswer must not coexist with AskApproval"
        );
    }

    // -----------------------------------------------------------------------
    // Test 10: approval gate overrides simple/retrieval intent (safety priority)
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_approval_gate_overrides_simple_intent() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default();

        // "hello" would normally trigger DirectAnswer, but the approval gate
        // must take precedence when the session is non-readonly with High risk.
        let state = state_with_risk("hello deploy service", SideEffectMode::GuardedWrite, RiskLevel::High);
        let branches = expander
            .expand(&state, &policy)
            .await
            .expect("test: expand should succeed");

        let labels: Vec<BranchLabel> = branches.iter().map(|b| b.label).collect();
        assert!(
            labels.contains(&BranchLabel::AskApproval),
            "approval gate must override simple-intent rule"
        );
        assert!(
            !labels.contains(&BranchLabel::DirectAnswer),
            "DirectAnswer must not appear when AskApproval is active"
        );
    }

    // -----------------------------------------------------------------------
    // Test 11: approval gate overrides retrieval intent (safety priority)
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_approval_gate_overrides_retrieval_intent() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default();

        // "audit" would normally trigger RetrieveThenAnswer, but high-risk
        // non-readonly must still require approval first.
        let state = state_with_risk("audit and deploy", SideEffectMode::GuardedWrite, RiskLevel::Critical);
        let branches = expander
            .expand(&state, &policy)
            .await
            .expect("test: expand should succeed");

        let labels: Vec<BranchLabel> = branches.iter().map(|b| b.label).collect();
        assert!(
            labels.contains(&BranchLabel::AskApproval),
            "approval gate must override retrieval-intent rule"
        );
        assert!(
            !labels.contains(&BranchLabel::DirectAnswer),
            "DirectAnswer must not appear alongside AskApproval"
        );
    }

    // -----------------------------------------------------------------------
    // Test 12: "hi" as a whole word matches, but "this" does not
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_hi_word_boundary_matching() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy::default();

        // "hi" as a standalone word → should match SIMPLE_KEYWORDS.
        let hi_state = base_state("hi there", SideEffectMode::ReadOnly);
        let hi_branches = expander
            .expand(&hi_state, &policy)
            .await
            .expect("test: expand should succeed");
        assert_eq!(
            hi_branches[0].label,
            BranchLabel::DirectAnswer,
            "'hi' standalone should trigger DirectAnswer"
        );

        // "this" contains "hi" as a substring but NOT as a whole word →
        // must NOT match SIMPLE_KEYWORDS.
        let this_state = base_state("explain this concept", SideEffectMode::ReadOnly);
        let this_branches = expander
            .expand(&this_state, &policy)
            .await
            .expect("test: expand should succeed");
        let this_labels: Vec<BranchLabel> = this_branches.iter().map(|b| b.label).collect();
        // "explain this concept" has no matching keyword → default path
        assert!(
            this_labels.contains(&BranchLabel::RetrieveThenAnswer) || this_labels.contains(&BranchLabel::DirectAnswer),
            "'this' substring must not trigger simple-keyword DirectAnswer(0.85)"
        );
        // Confidence must NOT be 0.85 (that value is only for simple-keyword matches).
        assert!(
            this_branches.iter().all(|b| (b.confidence - 0.85).abs() > f32::EPSILON),
            "'this' must not produce confidence=0.85 (simple-keyword path)"
        );
    }

    // -----------------------------------------------------------------------
    // Test 13: AskApproval with max_branches=1 has no companion
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_approval_only_when_max_branches_is_one() {
        let expander = DefaultTreeExpander::new();
        let policy = CausalPolicy {
            max_branches: 1,
            ..CausalPolicy::default()
        };

        let state = state_with_risk("deploy now", SideEffectMode::GuardedWrite, RiskLevel::High);
        let branches = expander
            .expand(&state, &policy)
            .await
            .expect("test: expand should succeed");

        assert_eq!(
            branches.len(),
            1,
            "max_branches=1 must yield exactly 1 branch even for approval path"
        );
        assert_eq!(
            branches[0].label,
            BranchLabel::AskApproval,
            "the single branch must be AskApproval"
        );
    }
}
