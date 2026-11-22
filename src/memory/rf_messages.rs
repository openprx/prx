//! Durable rf message attempts (AC-1b2a, `message_idempotency_v1`).
//!
//! One row per admitted attempt of an rf message key (install owner,
//! session, normalised agent, incarnation, `client_message_id`). The row is
//! written in the transaction that writes the attempt's input event, so an
//! input is never admitted without its row and a row never names an input
//! that was not admitted. A key gets a second attempt only through the
//! restart rule, which ends the previous attempt (`interrupted`) in the same
//! transaction. The row carries the input event's agent scope stamp and no
//! message content: only the digest of the canonical request.

/// The idempotency key of an rf message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfMessageKey {
    pub workspace_id: String,
    pub owner_id: String,
    pub session_key: String,
    /// `main` for Main (`agent` omitted or `"main"`), else the agent name.
    pub agent_key: String,
    /// The named agent's incarnation; empty for Main.
    pub incarnation: String,
    pub client_message_id: String,
}

/// The run of a `turn` attempt; a `context` attempt has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfRun {
    pub run_id: String,
    pub job_id: String,
}

/// The attempt row an admission writes with its input event.
#[derive(Debug, Clone)]
pub struct RfAdmission {
    pub key: RfMessageKey,
    /// 1 for the first admission; the previous attempt + 1 for a restart.
    pub attempt: i64,
    /// Hex SHA-256 of the canonical validated request.
    pub request_hash: String,
    pub run: Option<RfRun>,
    /// Restart rule: the run of the previous attempt, given its terminal
    /// `interrupted` in the same transaction, only while it has no terminal.
    pub interrupts: Option<String>,
}

/// The durable terminal of an attempt's run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RfRunTerminal {
    Succeeded,
    Failed,
    Cancelled,
    /// Ended by the restart rule or a cancel after the admitting process
    /// died; reported as `failed`.
    Interrupted,
}

impl RfRunTerminal {
    /// The rf status of the terminal.
    pub const fn status(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed | Self::Interrupted => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The closed code next to a `failed` status: `interrupted` for a run
    /// ended by interruption, none for every other terminal.
    pub const fn code(self) -> Option<&'static str> {
        match self {
            Self::Interrupted => Some("interrupted"),
            Self::Succeeded | Self::Failed | Self::Cancelled => None,
        }
    }
}

/// One stored attempt, with its run's terminal read in the same statement.
#[derive(Debug, Clone)]
pub struct RfAttempt {
    pub owner_id: String,
    pub session_key: String,
    pub agent_key: String,
    pub attempt: i64,
    pub request_hash: String,
    pub run: Option<RfRun>,
    /// The attempt's input event (the `message_id` of a `context`).
    pub message_event_id: String,
    /// The ingress lifetime of the process that admitted the attempt, when
    /// that process recorded ingress.
    pub lifetime: Option<String>,
    pub terminal: Option<RfRunTerminal>,
    /// The message was deleted (AC-1b2b): a replay of the key is refused.
    pub deleted: bool,
    /// The assistant side of the attempt's run was deleted.
    pub reply_deleted: bool,
}

/// Delete one rf message (AC-1b2b, `message_delete_v1`): every attempt of
/// `client_message_id` in the session, for one agent or for every agent.
#[derive(Debug, Clone)]
pub struct RfMessageDelete {
    pub workspace_id: String,
    pub owner_id: String,
    pub session_key: String,
    pub client_message_id: String,
    /// `main` or an agent name; `None` is every agent the message reached.
    pub agent_key: Option<String>,
    /// Runs whose job has not ended in this process: their stored rows are
    /// tombstoned when the job ends (see [`RfDeletedAgent::deferred`]).
    pub live_runs: Vec<String>,
    /// Also forget the memories and documents the deleted runs (and their
    /// descendants) wrote (`message_forget_writes_v1`).
    pub forget_writes: bool,
}

/// How many targets one forget deleted, by table and by whether the run
/// family created them (`created`) or only updated them (`touched`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RfForgetCounts {
    pub memories_created: i64,
    pub memories_touched: i64,
    pub documents_created: i64,
    pub documents_touched: i64,
}

impl RfForgetCounts {
    /// The per-table sums of two counts.
    #[must_use]
    pub const fn plus(self, other: Self) -> Self {
        Self {
            memories_created: self.memories_created.saturating_add(other.memories_created),
            memories_touched: self.memories_touched.saturating_add(other.memories_touched),
            documents_created: self.documents_created.saturating_add(other.documents_created),
            documents_touched: self.documents_touched.saturating_add(other.documents_touched),
        }
    }
}

/// The forget part of one agent's (or one reply's) delete answer, present
/// only when the request asked to forget writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RfForget {
    /// The counts deleted (or recorded at the first execution); `None` while
    /// a run of the delete is still live and its forget waits for settle.
    pub counts: Option<RfForgetCounts>,
    /// `false` when a deleted run started before this store recorded write
    /// provenance, so its writes cannot all be attributed.
    pub writes_traceable: bool,
}

impl RfForget {
    /// The answer of an agent with several attempts: summed counts, pending
    /// when any attempt is pending, traceable only when every attempt is.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            counts: self.counts.zip(other.counts).map(|(left, right)| left.plus(right)),
            writes_traceable: self.writes_traceable && other.writes_traceable,
        }
    }
}

/// A forget the store refused; nothing of the delete was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RfForgetRefused {
    /// The run family has more write-log rows than one delete forgets.
    #[error("forget_family_too_large: the deleted run family wrote more than one delete can forget")]
    FamilyTooLarge,
}

impl RfForgetRefused {
    /// The closed wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::FamilyTooLarge => "forget_family_too_large",
        }
    }

    /// The refusal anywhere in the error chain.
    pub fn of(error: &anyhow::Error) -> Option<Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>().copied())
    }
}

/// What one delete did for one agent key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfDeletedAgent {
    /// `main` or the agent name: the wire label.
    pub agent_key: String,
    /// The runs of the deleted attempts, in attempt order (`[]` for a
    /// `context` message).
    pub runs: Vec<String>,
    /// The jobs of those runs, whose in-memory results are erased.
    pub jobs: Vec<String>,
    /// Runs still live here: their message rows wait for the job's end.
    pub deferred: Vec<String>,
    /// The forget answer; `None` when the request did not ask to forget.
    pub forget: Option<RfForget>,
}

/// The outcome of a reply delete by run id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RfReplyDelete {
    /// The assistant side is deleted (now or by an earlier delete); the
    /// run's job, whose in-memory result is erased, and the forget answer
    /// when the request asked to forget.
    Deleted { job_id: String, forget: Option<RfForget> },
    /// No attempt of this owner's session has this run.
    Unknown,
    /// The run has no terminal yet.
    Active,
}

/// One row of a caller-keyed session's history, a tombstone included.
#[derive(Debug, Clone)]
pub struct RfHistoryTurn {
    pub turn: super::ConversationTurn,
    pub deleted: bool,
}

/// Which attempt to read.
#[derive(Debug, Clone)]
pub enum RfAttemptLookup {
    /// The latest attempt of a key.
    Latest(RfMessageKey),
    /// The attempt of a run, within its owner's session.
    Run {
        owner_id: String,
        session_key: String,
        run_id: String,
    },
    /// The attempt whose job has this id, of any owner.
    Job(String),
}

/// An admission whose transaction rolled back: nothing of it was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RfAdmissionRefused {
    /// Another admission of the same key and attempt committed first.
    #[error("another admission of this rf message key committed first")]
    Taken,
    /// The run the restart rule would end got a terminal first, or the
    /// message was deleted (AC-1b2b).
    #[error("the previous rf attempt has a terminal or was deleted; no new attempt starts")]
    Stale,
    /// User-message event recording is off, so no input event can hold it.
    #[error("rf message admission needs user-message event recording")]
    Unrecorded,
}

impl RfAdmissionRefused {
    /// The refusal anywhere in the error chain.
    pub fn of(error: &anyhow::Error) -> Option<Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>().copied())
    }
}

/// The run's terminal from its durable rows: the `turn:{run}:final` payload
/// first, then the `turn:{run}:admission-failed` payload, then the
/// attempt's own `interrupted` mark.
pub(crate) fn run_terminal(
    final_payload: Option<&str>,
    admission_failure: Option<&str>,
    interrupted: bool,
) -> anyhow::Result<Option<RfRunTerminal>> {
    fn status(payload: &str) -> anyhow::Result<String> {
        let value: serde_json::Value = serde_json::from_str(payload)?;
        value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("a terminal row carries no status"))
    }
    if let Some(payload) = final_payload {
        return match status(payload)?.as_str() {
            "completed" | "silent" => Ok(Some(RfRunTerminal::Succeeded)),
            "failed" => Ok(Some(RfRunTerminal::Failed)),
            "cancelled" => Ok(Some(RfRunTerminal::Cancelled)),
            _ => Err(anyhow::anyhow!("a terminal row carries an unknown status")),
        };
    }
    if let Some(payload) = admission_failure {
        return Ok(Some(if status(payload)? == "cancelled" {
            RfRunTerminal::Cancelled
        } else {
            RfRunTerminal::Failed
        }));
    }
    Ok(interrupted.then_some(RfRunTerminal::Interrupted))
}
