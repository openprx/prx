use serde::{Deserialize, Serialize};

/// Branch semantic labels — first version supports only three types.
///
/// Each label defines a distinct execution strategy. The `TreeExpander`
/// assigns labels based on the `CausalState` and produces scored candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BranchLabel {
    /// Answer directly without additional retrieval or tool use.
    DirectAnswer,
    /// Retrieve from memory / docs / artifacts, then answer.
    RetrieveThenAnswer,
    /// Require explicit user approval before proceeding (high-risk actions).
    AskApproval,
}

impl BranchLabel {
    /// Human-readable short name for logging / tracing.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DirectAnswer => "direct_answer",
            Self::RetrieveThenAnswer => "retrieve_then_answer",
            Self::AskApproval => "ask_approval",
        }
    }
}

impl std::fmt::Display for BranchLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Rehearsal depth — first version supports only two levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RehearsalLevel {
    /// Only compute a score — no I/O, no LLM calls.
    ScoreOnly,
    /// Run a read-only dry-run (memory prefetch, readonly worker, etc.).
    DryRunReadonly,
}

impl Default for RehearsalLevel {
    fn default() -> Self {
        Self::ScoreOnly
    }
}

/// Policy for committing a branch to the real execution path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitPolicy {
    /// Can be committed automatically if score is above threshold.
    AutoCommit,
    /// Requires explicit user confirmation before committing.
    RequireApproval,
}

impl Default for CommitPolicy {
    fn default() -> Self {
        Self::AutoCommit
    }
}

/// Estimated cost for a branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostEstimate {
    /// Estimated token consumption.
    pub estimated_tokens: u64,
    /// Estimated monetary cost in USD (micro-cents for precision).
    pub estimated_cost_micro_usd: u64,
}

impl Default for CostEstimate {
    fn default() -> Self {
        Self {
            estimated_tokens: 0,
            estimated_cost_micro_usd: 0,
        }
    }
}

/// A candidate branch produced by the `TreeExpander`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalBranch {
    /// Unique identifier for this branch within the current CTE run.
    pub branch_id: String,
    /// Semantic label defining the execution strategy.
    pub label: BranchLabel,
    /// IDs of parent steps that this branch follows from.
    pub parent_step_ids: Vec<String>,
    /// Inputs required by this branch (e.g. "memory:recent", "tool:shell").
    pub required_inputs: Vec<String>,
    /// Predicted quality gain in [0.0, 1.0].
    pub predicted_gain: f32,
    /// Estimated resource cost.
    pub estimated_cost: CostEstimate,
    /// Estimated execution latency in milliseconds.
    pub estimated_latency_ms: u32,
    /// Confidence that this branch is the right path, in [0.0, 1.0].
    pub confidence: f32,
    /// Maximum rehearsal depth permitted for this branch.
    pub rehearsal_level: RehearsalLevel,
    /// Commit policy for this branch.
    pub commit_policy: CommitPolicy,
    /// Human-readable explanation for why this branch was proposed.
    pub explanation: Vec<String>,
}

/// Artifact produced by a rehearsal run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RehearsalArtifact {
    /// Which branch this artifact belongs to.
    pub branch_id: String,
    /// Preview output (partial answer, draft, etc.) — may be `None` for `ScoreOnly`.
    pub preview_output: Option<String>,
    /// Memory keys retrieved during prefetch.
    pub retrieved_memory_keys: Vec<String>,
    /// Model selected by the Router for this branch (if applicable).
    pub selected_model: Option<String>,
    /// Score delta from the rehearsal (positive = better than pre-rehearsal estimate).
    pub score_delta: f32,
    /// Warnings generated during rehearsal.
    pub warnings: Vec<String>,
}

/// The final decision on which branch to commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathCommitDecision {
    /// The branch selected as the primary execution path.
    pub chosen_branch_id: String,
    /// Branches explicitly rejected.
    pub rejected_branch_ids: Vec<String>,
    /// Branches kept as fallback (ordered by preference).
    pub fallback_branch_ids: Vec<String>,
    /// Reasons for the selection (human-readable).
    pub reasons: Vec<String>,
    /// Cache TTL for rehearsal artifacts (seconds).
    pub cache_ttl_seconds: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_branch_label_display() {
        assert_eq!(BranchLabel::DirectAnswer.as_str(), "direct_answer");
        assert_eq!(
            BranchLabel::RetrieveThenAnswer.as_str(),
            "retrieve_then_answer"
        );
        assert_eq!(BranchLabel::AskApproval.as_str(), "ask_approval");
    }

    #[test]
    fn test_rehearsal_level_ordering() {
        assert!(RehearsalLevel::ScoreOnly < RehearsalLevel::DryRunReadonly);
    }

    #[test]
    fn test_cost_estimate_default() {
        let cost = CostEstimate::default();
        assert_eq!(cost.estimated_tokens, 0);
        assert_eq!(cost.estimated_cost_micro_usd, 0);
    }

    #[test]
    fn test_branch_serde_roundtrip() {
        let branch = CausalBranch {
            branch_id: "b-1".into(),
            label: BranchLabel::DirectAnswer,
            parent_step_ids: vec!["s1".into()],
            required_inputs: vec![],
            predicted_gain: 0.7,
            estimated_cost: CostEstimate::default(),
            estimated_latency_ms: 100,
            confidence: 0.85,
            rehearsal_level: RehearsalLevel::ScoreOnly,
            commit_policy: CommitPolicy::AutoCommit,
            explanation: vec!["simple question".into()],
        };
        let json = serde_json::to_string(&branch).expect("test: serialize");
        let restored: CausalBranch =
            serde_json::from_str(&json).expect("test: deserialize");
        assert_eq!(restored.branch_id, "b-1");
        assert_eq!(restored.label, BranchLabel::DirectAnswer);
    }
}
