use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum JobType {
    #[default]
    Shell,
    Agent,
}

impl JobType {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Agent => "agent",
        }
    }

    pub(crate) const fn parse(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("agent") {
            Self::Agent
        } else {
            Self::Shell
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SessionTarget {
    #[default]
    Isolated,
    Main,
}

impl SessionTarget {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Isolated => "isolated",
            Self::Main => "main",
        }
    }

    pub(crate) const fn parse(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("main") {
            Self::Main
        } else {
            Self::Isolated
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Schedule {
    Cron {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
    At {
        at: DateTime<Utc>,
    },
    Every {
        every_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CronJobTerminalState {
    Succeeded,
    Failed,
}

impl CronJobTerminalState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw {
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            other => anyhow::bail!("Invalid cron terminal state: {other}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryConfig {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default = "default_true")]
    pub best_effort: bool,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            mode: "none".to_string(),
            channel: None,
            to: None,
            best_effort: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// Fencing handle for one scheduler execution attempt.
///
/// A handle is authoritative only while the matching database lease is still
/// unexpired. Callers must pass the complete handle back when renewing or
/// finishing a run; `last_status` is deliberately not part of lease authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronClaim {
    pub worker_id: String,
    pub attempt_id: String,
    pub claimed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub expression: String,
    pub schedule: Schedule,
    pub command: String,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub job_type: JobType,
    pub session_target: SessionTarget,
    pub model: Option<String>,
    pub enabled: bool,
    pub delivery: DeliveryConfig,
    pub delete_after_run: bool,
    pub created_at: DateTime<Utc>,
    pub next_run: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_status: Option<String>,
    pub last_output: Option<String>,
    #[serde(default)]
    pub claim: Option<CronClaim>,
    #[serde(default)]
    pub terminal_state: Option<CronJobTerminalState>,
    pub approval_grant_json: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CronJobLineage {
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub source_message_event_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronRun {
    pub id: i64,
    pub job_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: String,
    pub output: Option<String>,
    pub duration_ms: Option<i64>,
    pub attempt_id: Option<String>,
    pub worker_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJobEvent {
    pub id: i64,
    pub event_id: String,
    pub job_id: String,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CronJobPatch {
    pub schedule: Option<Schedule>,
    pub command: Option<String>,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub delivery: Option<DeliveryConfig>,
    pub model: Option<String>,
    pub session_target: Option<SessionTarget>,
    pub delete_after_run: Option<bool>,
    #[serde(skip)]
    pub approval_grant_json: Option<String>,
}
