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
    /// Ended by the restart rule after the admitting process died;
    /// reported as `failed`.
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
    /// The run the restart rule would end got a terminal first.
    #[error("the previous rf attempt has a terminal; no new attempt starts")]
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
