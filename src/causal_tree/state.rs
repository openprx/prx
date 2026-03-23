use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The side-effect mode governing what the CTE pipeline is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum SideEffectMode {
    /// No writes, no tool execution — pure analysis.
    ReadOnly,
    /// Writes require explicit user approval.
    ApprovalRequired,
    /// Writes allowed under policy guard (first-version default is ReadOnly).
    GuardedWrite,
}

impl Default for SideEffectMode {
    fn default() -> Self {
        Self::ReadOnly
    }
}

/// Current token / latency / cost budget for the CTE pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetState {
    /// Maximum additional tokens the CTE may consume (across all branches).
    pub extra_token_limit: u64,
    /// Tokens already consumed by CTE in this request.
    pub tokens_used: u64,
    /// Maximum additional latency budget in milliseconds.
    pub extra_latency_budget_ms: u64,
    /// Latency already consumed by CTE in this request.
    pub latency_used_ms: u64,
}

impl Default for BudgetState {
    fn default() -> Self {
        Self {
            extra_token_limit: 4096,
            tokens_used: 0,
            extra_latency_budget_ms: 300,
            latency_used_ms: 0,
        }
    }
}

impl BudgetState {
    /// Returns `true` if the token budget has been exhausted.
    pub const fn tokens_exhausted(&self) -> bool {
        self.tokens_used >= self.extra_token_limit
    }

    /// Returns `true` if the latency budget has been exhausted.
    pub const fn latency_exhausted(&self) -> bool {
        self.latency_used_ms >= self.extra_latency_budget_ms
    }

    /// Remaining token headroom.
    pub const fn remaining_tokens(&self) -> u64 {
        self.extra_token_limit.saturating_sub(self.tokens_used)
    }

    /// Remaining latency headroom in milliseconds.
    pub const fn remaining_latency_ms(&self) -> u64 {
        self.extra_latency_budget_ms.saturating_sub(self.latency_used_ms)
    }
}

/// Severity levels for risk flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

/// A risk flag attached to the current session state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskFlag {
    pub code: String,
    pub severity: RiskLevel,
    pub message: String,
}

/// Status of a completed or in-progress step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

/// A record of a step that has already occurred in the current session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub step_id: String,
    pub label: String,
    pub status: StepStatus,
    pub started_at: String,
    pub ended_at: Option<String>,
    /// Evidence or references produced by this step.
    pub evidence: Vec<String>,
}

/// Type of artifact produced during a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactType {
    TextSummary,
    CodeSnippet,
    FileReference,
    ToolOutput,
    MemoryEntry,
}

/// Source of an artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactSource {
    UserInput,
    LlmOutput,
    ToolExecution,
    MemoryRetrieval,
}

/// A reference to an artifact produced during the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub artifact_type: ArtifactType,
    pub summary: String,
    pub source: ArtifactSource,
    /// Importance score in [0.0, 1.0].
    pub importance: f32,
}

/// The full causal state snapshot for the current request.
///
/// This is an immutable value type (`Clone`) that captures everything the CTE
/// needs to make branching decisions. It is constructed once per request and
/// passed by reference to all downstream components.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalState {
    pub session_id: String,
    pub request_id: String,
    /// High-level goal for the current task.
    pub goal: String,
    /// Classified user intent (e.g. "code_audit", "simple_qa", "tool_chain").
    pub user_intent: String,
    /// Steps already completed in this session.
    pub completed_steps: Vec<StepRecord>,
    /// Active constraints or rules (e.g. "no_write", "budget_limited").
    pub active_constraints: Vec<String>,
    /// Artifacts produced so far.
    pub known_artifacts: Vec<ArtifactRef>,
    /// Unresolved risks flagged by previous steps.
    pub unresolved_risks: Vec<RiskFlag>,
    /// Current side-effect permission mode.
    pub side_effect_mode: SideEffectMode,
    /// Token / latency budget.
    pub budget: BudgetState,
    /// Timestamp when this snapshot was created (ISO-8601).
    pub snapshot_ts: String,
}

impl CausalState {
    /// Returns the highest risk severity among unresolved risks, if any.
    pub fn max_risk_level(&self) -> Option<RiskLevel> {
        self.unresolved_risks.iter().map(|r| r.severity).max()
    }

    /// Returns `true` if any step has failed.
    pub fn has_failed_steps(&self) -> bool {
        self.completed_steps.iter().any(|s| s.status == StepStatus::Failed)
    }

    /// Convenience: number of completed (succeeded) steps.
    pub fn succeeded_step_count(&self) -> usize {
        self.completed_steps
            .iter()
            .filter(|s| s.status == StepStatus::Succeeded)
            .count()
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_state() -> CausalState {
        CausalState {
            session_id: "sess-1".into(),
            request_id: "req-1".into(),
            goal: "audit repository".into(),
            user_intent: "code_audit".into(),
            completed_steps: vec![
                StepRecord {
                    step_id: "s1".into(),
                    label: "read repo".into(),
                    status: StepStatus::Succeeded,
                    started_at: "2026-03-22T10:00:00Z".into(),
                    ended_at: Some("2026-03-22T10:00:05Z".into()),
                    evidence: vec!["found 10 files".into()],
                },
                StepRecord {
                    step_id: "s2".into(),
                    label: "parse config".into(),
                    status: StepStatus::Failed,
                    started_at: "2026-03-22T10:00:06Z".into(),
                    ended_at: Some("2026-03-22T10:00:07Z".into()),
                    evidence: vec![],
                },
            ],
            active_constraints: vec!["no_write".into()],
            known_artifacts: vec![ArtifactRef {
                artifact_id: "a1".into(),
                artifact_type: ArtifactType::TextSummary,
                summary: "repo overview".into(),
                source: ArtifactSource::LlmOutput,
                importance: 0.8,
            }],
            unresolved_risks: vec![RiskFlag {
                code: "UNWRAP_IN_PROD".into(),
                severity: RiskLevel::High,
                message: "found unwrap in production code".into(),
            }],
            side_effect_mode: SideEffectMode::ReadOnly,
            budget: BudgetState::default(),
            snapshot_ts: "2026-03-22T10:00:10Z".into(),
        }
    }

    #[test]
    fn test_max_risk_level() {
        let state = sample_state();
        assert_eq!(state.max_risk_level(), Some(RiskLevel::High));
    }

    #[test]
    fn test_has_failed_steps() {
        let state = sample_state();
        assert!(state.has_failed_steps());
    }

    #[test]
    fn test_succeeded_step_count() {
        let state = sample_state();
        assert_eq!(state.succeeded_step_count(), 1);
    }

    #[test]
    fn test_budget_helpers() {
        let mut budget = BudgetState::default();
        assert!(!budget.tokens_exhausted());
        assert!(!budget.latency_exhausted());
        assert_eq!(budget.remaining_tokens(), 4096);
        assert_eq!(budget.remaining_latency_ms(), 300);

        budget.tokens_used = 5000;
        assert!(budget.tokens_exhausted());
        assert_eq!(budget.remaining_tokens(), 0);
    }

    #[test]
    fn test_serde_roundtrip() {
        let state = sample_state();
        let json = serde_json::to_string(&state).expect("test: serialize");
        let restored: CausalState = serde_json::from_str(&json).expect("test: deserialize");
        assert_eq!(restored.session_id, "sess-1");
        assert_eq!(restored.completed_steps.len(), 2);
    }
}
