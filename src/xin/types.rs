//! Core types for the xin (心) autonomous task engine.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Task lifecycle: Pending → Running → Completed | Failed | Stale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Stale,
}

impl TaskStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stale => "stale",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "stale" => Self::Stale,
            _ => Self::Pending,
        }
    }
}

/// Discriminates built-in system tasks from user-defined ones.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Built-in autonomous system task (evolution, fitness, hygiene).
    System,
    /// User/LLM-created task.
    User,
    /// Agent-spawned task.
    Agent,
}

impl TaskKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Agent => "agent",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "system" => Self::System,
            "agent" => Self::Agent,
            _ => Self::User,
        }
    }
}

/// Priority affects execution ordering within a tick.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum TaskPriority {
    Low = 0,
    Normal = 1,
    High = 2,
    Critical = 3,
}

impl TaskPriority {
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    pub const fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::Low,
            2 => Self::High,
            3 => Self::Critical,
            _ => Self::Normal,
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "low" => Self::Low,
            "high" => Self::High,
            "critical" => Self::Critical,
            _ => Self::Normal,
        }
    }
}

/// Execution mode: how the task runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Run a built-in Rust function (system tasks).
    Internal,
    /// Run as an isolated LLM agent session.
    AgentSession,
    /// Run as a shell command.
    Shell,
}

impl ExecutionMode {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::AgentSession => "agent_session",
            Self::Shell => "shell",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "internal" => Self::Internal,
            "shell" => Self::Shell,
            _ => Self::AgentSession,
        }
    }
}

/// Core task struct persisted in SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XinTask {
    pub id: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub execution_mode: ExecutionMode,
    /// For AgentSession: the prompt. For Shell: the command. For Internal: the handler key.
    pub payload: String,
    /// Whether this task repeats after completion.
    pub recurring: bool,
    /// Interval in seconds for recurring tasks.
    pub interval_secs: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub next_run_at: DateTime<Utc>,
    pub last_status: Option<String>,
    pub last_output: Option<String>,
    pub run_count: u64,
    pub fail_count: u64,
    /// Max consecutive failures before auto-disabling (0 = no limit).
    pub max_failures: u32,
    pub enabled: bool,
    /// Runtime-created approval grant for delayed shell execution.
    pub approval_grant_json: Option<String>,
}

/// Input for creating a new task.
#[derive(Debug, Clone)]
pub struct NewXinTask {
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub kind: TaskKind,
    pub priority: TaskPriority,
    pub execution_mode: ExecutionMode,
    pub payload: String,
    pub recurring: bool,
    pub interval_secs: u64,
    pub max_failures: u32,
    pub approval_grant_json: Option<String>,
}

/// Patch struct for updating tasks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct XinTaskPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub priority: Option<TaskPriority>,
    pub payload: Option<String>,
    pub interval_secs: Option<u64>,
    pub enabled: Option<bool>,
    pub max_failures: Option<u32>,
    #[serde(skip)]
    pub approval_grant_json: Option<String>,
}

/// Append-only execution and lifecycle event for a Xin task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XinTaskEvent {
    pub id: i64,
    pub event_id: String,
    pub task_id: String,
    pub workspace_id: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub event_type: String,
    pub status: Option<String>,
    pub payload_json: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Goal-level status: the user-visible progress of a multi-step intent.
///
/// FIX-P2-16 (d09): a `XinGoal` aggregates the state of its ordered `XinStep`s.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    /// No step has started yet.
    Pending,
    /// At least one step is claimed/running.
    Running,
    /// All steps completed successfully.
    Completed,
    /// A step exhausted its retries.
    Failed,
    /// The goal was cancelled by an operator/owner.
    Cancelled,
}

impl GoalStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }
}

/// Step-level status: the execution-engine-visible atomic state.
///
/// A step moves Pending → Claimed → Running → Completed | Failed. When a lease
/// expires while the step is Claimed/Running it is reset to `Stale` and may be
/// re-claimed by any worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    Claimed,
    Running,
    Completed,
    Failed,
    Stale,
}

impl StepStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stale => "stale",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "claimed" => Self::Claimed,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "stale" => Self::Stale,
            _ => Self::Pending,
        }
    }
}

/// A user-visible goal that owns N ordered execution steps.
///
/// Corresponds to the "top-level intent" semantics of a `XinTask`; introduced
/// in parallel with `xin_tasks` (zero-breakage migration, see store.rs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XinGoal {
    /// UUID v4.
    pub id: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    /// Supports nested goals / sub-goals.
    pub parent_task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub kind: TaskKind,
    pub status: GoalStatus,
    pub priority: TaskPriority,
    /// Expected completion time (SLA reference, not enforced).
    pub target_completion_at: Option<DateTime<Utc>>,
    /// Number of steps that have reached Completed.
    pub steps_completed: u32,
    /// Total number of steps (0 = unknown / dynamically appended).
    pub steps_total: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Final output summary on success.
    pub final_output: Option<String>,
    pub enabled: bool,
}

/// A single execution step inside a `XinGoal`. Supports lease + checkpoint +
/// heartbeat for crash-safe, long-running work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XinStep {
    /// UUID v4.
    pub id: String,
    /// FK → xin_goals.id.
    pub goal_id: String,
    /// 1-based execution order within the goal.
    pub sequence: u32,
    pub name: String,
    pub description: Option<String>,
    pub status: StepStatus,
    pub execution_mode: ExecutionMode,
    /// AgentSession prompt / Shell command / Internal handler key.
    pub payload: String,
    /// Worker holding the current lease (format: `prx:{pid}:{host_hash}`).
    pub lease_owner: Option<String>,
    /// Lease expiry; once past, any worker may re-claim.
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// Most recent heartbeat (liveness signal).
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Structured progress snapshot (JSON); replayed on crash recovery.
    pub checkpoint_json: Option<String>,
    /// Per-step lease TTL override in seconds (0 = per-mode default).
    pub lease_ttl_secs: u64,
    pub retry_count: u32,
    pub max_retries: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub last_output: Option<String>,
    pub approval_grant_json: Option<String>,
}

/// Input for creating a new step (see [`NewXinGoal`]).
#[derive(Debug, Clone)]
pub struct NewXinStep {
    pub sequence: u32,
    pub name: String,
    pub description: Option<String>,
    pub execution_mode: ExecutionMode,
    pub payload: String,
    pub max_retries: u32,
    pub approval_grant_json: Option<String>,
    /// Lease TTL in seconds; 0 = use the per-mode default.
    pub lease_ttl_secs: u64,
}

/// Input for creating a new goal with an optional initial set of steps.
#[derive(Debug, Clone)]
pub struct NewXinGoal {
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub kind: TaskKind,
    pub priority: TaskPriority,
    pub target_completion_at: Option<DateTime<Utc>>,
    /// Initial steps; more may be appended later via `add_step`.
    pub initial_steps: Vec<NewXinStep>,
}

/// Default lease TTL (seconds) by execution mode (d09 §7).
pub const fn default_lease_ttl_secs(mode: &ExecutionMode) -> u64 {
    match mode {
        ExecutionMode::AgentSession => 1800,
        ExecutionMode::Shell => 300,
        ExecutionMode::Internal => 60,
    }
}

/// Summary of a single xin tick.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct XinTickSummary {
    pub tasks_checked: usize,
    pub tasks_executed: usize,
    pub tasks_completed: usize,
    pub tasks_failed: usize,
    pub tasks_cleaned: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_roundtrip() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Stale,
        ] {
            assert_eq!(TaskStatus::from_str_lossy(status.as_str()), status);
        }
    }

    #[test]
    fn task_priority_ordering() {
        assert!(TaskPriority::Low < TaskPriority::Normal);
        assert!(TaskPriority::Normal < TaskPriority::High);
        assert!(TaskPriority::High < TaskPriority::Critical);
    }

    #[test]
    fn task_priority_roundtrip() {
        for p in [
            TaskPriority::Low,
            TaskPriority::Normal,
            TaskPriority::High,
            TaskPriority::Critical,
        ] {
            assert_eq!(TaskPriority::from_i32(p.as_i32()), p);
        }
    }

    #[test]
    fn execution_mode_roundtrip() {
        for mode in [
            ExecutionMode::Internal,
            ExecutionMode::AgentSession,
            ExecutionMode::Shell,
        ] {
            assert_eq!(ExecutionMode::from_str_lossy(mode.as_str()), mode);
        }
    }

    #[test]
    fn task_kind_roundtrip() {
        for kind in [TaskKind::System, TaskKind::User, TaskKind::Agent] {
            assert_eq!(TaskKind::from_str_lossy(kind.as_str()), kind);
        }
    }

    #[test]
    fn goal_status_roundtrip() {
        for status in [
            GoalStatus::Pending,
            GoalStatus::Running,
            GoalStatus::Completed,
            GoalStatus::Failed,
            GoalStatus::Cancelled,
        ] {
            assert_eq!(GoalStatus::from_str_lossy(status.as_str()), status);
        }
    }

    #[test]
    fn step_status_roundtrip() {
        for status in [
            StepStatus::Pending,
            StepStatus::Claimed,
            StepStatus::Running,
            StepStatus::Completed,
            StepStatus::Failed,
            StepStatus::Stale,
        ] {
            assert_eq!(StepStatus::from_str_lossy(status.as_str()), status);
        }
    }

    #[test]
    fn default_lease_ttl_by_mode() {
        assert_eq!(default_lease_ttl_secs(&ExecutionMode::AgentSession), 1800);
        assert_eq!(default_lease_ttl_secs(&ExecutionMode::Shell), 300);
        assert_eq!(default_lease_ttl_secs(&ExecutionMode::Internal), 60);
    }
}
