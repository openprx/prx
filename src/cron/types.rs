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

/// The principal an `announce` delivery is authorized as.
///
/// A cron delivery is the one outbound message in this runtime with no live
/// caller behind it: `delivery.channel` and `delivery.to` are written by the
/// model when the job is created and fire hours or days later, long after that
/// turn ended. The authorization subject therefore has to be captured at
/// creation time and persisted with the job — that is what this is.
///
/// It is filled from the runtime-injected trusted scope (`_zc_scope`, guarded
/// by `_zc_scope_trusted`), which [`crate::tools::execution`] scrubs of any
/// model-supplied value before rewriting it, so a model cannot name a different
/// sender to widen the reach of a job it schedules.
///
/// Every field is optional because a job can reach the store without one: a row
/// added by `prx cron add` on the CLI, hand-inserted into `jobs.db`, written by
/// an external tool, or created by a PRX build that predates these columns.
/// Such a job is **not** waved through — it is authorized as the least
/// privileged caller there is (`unknown` on all three axes), the same
/// degradation `message_send` applies when it finds no trusted scope. Only
/// wildcard scope rules can then match it, and the cross-channel default still
/// applies, so the absence of an identity can never widen what a job may reach.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryPrincipal {
    #[serde(default)]
    pub sender: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub chat_type: Option<String>,
}

impl DeliveryPrincipal {
    /// What every unset axis of an identity-less job resolves to.
    ///
    /// MUTATION GUARD: this must stay a value no real channel, sender or chat
    /// type can equal. Replacing it with the destination channel, or with any
    /// value that makes `src == dst` hold by construction, turns the missing
    /// identity into a free pass through the same-channel default.
    pub const UNKNOWN: &'static str = "unknown";

    /// Build the principal a creating turn's trusted scope describes.
    #[must_use]
    pub const fn new(sender: Option<String>, channel: Option<String>, chat_type: Option<String>) -> Self {
        Self {
            sender,
            channel,
            chat_type,
        }
    }

    /// The authorizing sender, or [`Self::UNKNOWN`] when the job carries none.
    #[must_use]
    pub fn sender(&self) -> &str {
        Self::field(self.sender.as_deref())
    }

    /// The channel the creating turn was anchored to, or [`Self::UNKNOWN`].
    ///
    /// This is the `src_channel` of the outbound decision. A delivery whose
    /// `delivery.channel` differs from it is a genuine cross-channel send — the
    /// model named another channel — and is gated exactly like the `channel`
    /// argument of `message_send`.
    #[must_use]
    pub fn channel(&self) -> &str {
        Self::field(self.channel.as_deref())
    }

    /// The creating turn's chat type, or [`Self::UNKNOWN`].
    #[must_use]
    pub fn chat_type(&self) -> &str {
        Self::field(self.chat_type.as_deref())
    }

    /// Whether no axis of this principal is known.
    #[must_use]
    pub const fn is_anonymous(&self) -> bool {
        self.sender.is_none() && self.channel.is_none() && self.chat_type.is_none()
    }

    fn field(value: Option<&str>) -> &str {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(Self::UNKNOWN)
    }
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
    /// Who this job's `announce` delivery is authorized as. See
    /// [`DeliveryPrincipal`]; absent on rows that predate the columns or were
    /// written outside the tool path, which authorizes them as `unknown`.
    #[serde(default)]
    pub delivery_principal: DeliveryPrincipal,
}

#[derive(Debug, Clone, Default)]
pub struct CronJobLineage {
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub source_message_event_id: Option<String>,
    /// Creator identity carried alongside the lineage so every `add_*` entry
    /// point persists it without growing another argument.
    pub delivery_principal: DeliveryPrincipal,
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
    /// Re-anchor the delivery principal, set only by the runtime.
    ///
    /// `#[serde(skip)]` for the same reason `approval_grant_json` is: the patch
    /// body is model-written, and a model that could set this would be choosing
    /// the identity its own delivery is authorized as. `Some` replaces all
    /// three axes at once, so re-anchoring to a caller with no trusted scope
    /// clears the previous owner's identity instead of inheriting it.
    #[serde(skip)]
    pub delivery_principal: Option<DeliveryPrincipal>,
}
