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
    pub fn as_str(&self) -> &'static str {
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
    pub fn as_str(&self) -> &'static str {
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
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    pub fn from_i32(v: i32) -> Self {
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
    pub fn as_str(&self) -> &'static str {
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
}

/// Input for creating a new task.
#[derive(Debug, Clone)]
pub struct NewXinTask {
    pub name: String,
    pub description: Option<String>,
    pub kind: TaskKind,
    pub priority: TaskPriority,
    pub execution_mode: ExecutionMode,
    pub payload: String,
    pub recurring: bool,
    pub interval_secs: u64,
    pub max_failures: u32,
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
}
