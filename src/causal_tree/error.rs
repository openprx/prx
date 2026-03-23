use std::fmt;

/// Unified error type for the Causal Tree Engine.
///
/// All public trait methods return `Result<T, CausalTreeError>` to provide
/// structured error handling with proper context.
#[derive(Debug)]
pub enum CausalTreeError {
    /// State snapshot construction failed.
    SnapshotFailed(String),
    /// Branch expansion produced no candidates.
    ExpansionEmpty,
    /// Branch expansion failed.
    ExpansionFailed(String),
    /// Rehearsal timed out for the given branch.
    RehearsalTimeout { branch_id: String, elapsed_ms: u64 },
    /// Rehearsal execution failed.
    RehearsalFailed { branch_id: String, reason: String },
    /// No branch met the commit threshold after scoring.
    NoBranchQualified { threshold: f32, best_score: f32 },
    /// The entire CTE pipeline exceeded its time budget.
    PipelineTimeout { budget_ms: u64, elapsed_ms: u64 },
    /// Circuit breaker is open — CTE is temporarily disabled.
    CircuitBreakerOpen { consecutive_failures: u32 },
    /// Policy violation prevented execution.
    PolicyViolation(String),
    /// Feedback write failed (non-fatal, logged).
    FeedbackWriteFailed(String),
    /// Configuration error.
    ConfigError(String),
    /// Wrapped upstream error.
    Internal(anyhow::Error),
}

impl fmt::Display for CausalTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotFailed(msg) => write!(f, "snapshot failed: {msg}"),
            Self::ExpansionEmpty => write!(f, "branch expansion produced no candidates"),
            Self::ExpansionFailed(msg) => write!(f, "branch expansion failed: {msg}"),
            Self::RehearsalTimeout { branch_id, elapsed_ms } => {
                write!(f, "rehearsal timeout for branch {branch_id} ({elapsed_ms}ms)")
            }
            Self::RehearsalFailed { branch_id, reason } => {
                write!(f, "rehearsal failed for branch {branch_id}: {reason}")
            }
            Self::NoBranchQualified { threshold, best_score } => write!(
                f,
                "no branch met commit threshold {threshold:.2} (best: {best_score:.2})"
            ),
            Self::PipelineTimeout { budget_ms, elapsed_ms } => {
                write!(f, "CTE pipeline timeout: budget {budget_ms}ms, elapsed {elapsed_ms}ms")
            }
            Self::CircuitBreakerOpen { consecutive_failures } => write!(
                f,
                "CTE circuit breaker open after {consecutive_failures} consecutive failures"
            ),
            Self::PolicyViolation(msg) => write!(f, "policy violation: {msg}"),
            Self::FeedbackWriteFailed(msg) => write!(f, "feedback write failed: {msg}"),
            Self::ConfigError(msg) => write!(f, "config error: {msg}"),
            Self::Internal(e) => write!(f, "internal error: {e}"),
        }
    }
}

impl std::error::Error for CausalTreeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for CausalTreeError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}
