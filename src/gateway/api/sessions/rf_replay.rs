//! Replay and the restart rule of rf messages (AC-1b2a,
//! `message_idempotency_v1`).
//!
//! An rf message key (install owner, session, normalised agent,
//! incarnation, `client_message_id`) has one attempt row per admitted
//! attempt, written in the transaction of the attempt's input event. A
//! resend with the same request answers from that row and never admits
//! anything, with one exception, the restart rule: a `turn` attempt with no
//! terminal row, not live in this process, whose admitting process is
//! proven gone. The proof is the ingress lifetime lock the admitting process
//! held for its whole life: a lock that can be claimed has no live holder.
//! The status of a replay is read from the run's durable terminal rows in the
//! same statement as the attempt row, or is `running` while the turn is live
//! here; it never comes from the in-memory job store.

use super::super::capabilities::coded_error;
use super::rf_message::{TurnSlot, live_run};
use crate::gateway::AppState;
use crate::memory::{RfAdmission, RfAdmissionRefused, RfAttempt, RfAttemptLookup, RfMessageKey};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// The longest wait for the liveness claim on the admitting process's lock.
const LIVENESS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

const UNRECORDED: &str =
    "message idempotency needs user-message event recording (memory.events.record_user_messages)";

fn storage_unavailable(message: &str) -> Response {
    coded_error(StatusCode::INTERNAL_SERVER_ERROR, "storage_unavailable", message)
}

pub(super) fn session_busy() -> Response {
    coded_error(
        StatusCode::CONFLICT,
        "session_busy",
        "a turn of this agent in this session is still running",
    )
}

/// An rf `turn` on its way from admission to its job.
pub(super) struct RfTurn {
    /// The (session, agent) slot, held until the job's work ends.
    pub(super) slot: TurnSlot,
    pub(super) run_id: String,
    pub(super) job_id: uuid::Uuid,
    /// The attempt row the input's transaction writes.
    pub(super) admission: RfAdmission,
}

impl RfTurn {
    /// `202 {"run_id","job_id"}`; a restart adds `attempt` and
    /// `previous_run_id`.
    pub(super) fn accepted(&self) -> Response {
        let mut body = serde_json::json!({ "run_id": self.run_id, "job_id": self.job_id.to_string() });
        if let (Some(previous), Some(object)) = (&self.admission.interrupts, body.as_object_mut()) {
            object.insert("attempt".to_owned(), self.admission.attempt.into());
            object.insert("previous_run_id".to_owned(), previous.clone().into());
        }
        (StatusCode::ACCEPTED, Json(body)).into_response()
    }
}

/// What the stored attempt of a key says about a request.
pub(super) enum Lookup {
    /// No attempt: admit the first one.
    First,
    /// The restart rule holds for this attempt: admit the next one.
    Restart(RfAttempt),
    /// The answer, a replay or a refusal; nothing is admitted.
    Answer(Response),
}

async fn read(state: &AppState, workspace: &str, lookup: RfAttemptLookup) -> Result<Option<RfAttempt>, Box<Response>> {
    state.mem.rf_message_attempt(workspace, lookup).await.map_err(|error| {
        tracing::warn!(
            error_kind = super::console_error_kind(&error),
            "rf message record lookup failed"
        );
        Box::new(storage_unavailable("the message record could not be read"))
    })
}

/// The replay lookup, after the agent checks and before `session_busy`.
pub(super) async fn lookup(state: &AppState, key: &RfMessageKey, request_hash: &str) -> Lookup {
    let attempt = match read(state, &key.workspace_id, RfAttemptLookup::Latest(key.clone())).await {
        Ok(Some(attempt)) => attempt,
        Ok(None) => return Lookup::First,
        Err(response) => return Lookup::Answer(*response),
    };
    if attempt.request_hash != request_hash {
        return Lookup::Answer(coded_error(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "this client_message_id was already used with a different request",
        ));
    }
    let Some(run) = attempt.run.clone() else {
        return Lookup::Answer(
            Json(serde_json::json!({
                "message_id": attempt.message_event_id,
                "status": "recorded",
                "replayed": true,
            }))
            .into_response(),
        );
    };
    match turn_status(state, &key.workspace_id, &attempt, &run.run_id).await {
        Ok(Some(status)) => Lookup::Answer(
            Json(serde_json::json!({
                "run_id": run.run_id,
                "job_id": run.job_id,
                "status": status,
                "replayed": true,
            }))
            .into_response(),
        ),
        Ok(None) => Lookup::Restart(attempt),
        Err(response) => Lookup::Answer(*response),
    }
}

/// The status of a turn attempt; `None` when the restart rule holds.
async fn turn_status(
    state: &AppState,
    workspace: &str,
    attempt: &RfAttempt,
    run_id: &str,
) -> Result<Option<&'static str>, Box<Response>> {
    if let Some(terminal) = attempt.terminal {
        return Ok(Some(terminal.status()));
    }
    if live_run(workspace, &attempt.session_key, &attempt.agent_key, run_id) {
        return Ok(Some("running"));
    }
    // Not finished by any durable row and not live here: either its process
    // is gone (restart), or it is still held by a live process, or its job
    // ended here without a confirmed terminal, which keeps this runtime's
    // admission closed until a restart settles it. Both read as `running`.
    Ok(if admitting_process_gone(state, workspace, attempt).await? {
        None
    } else {
        Some("running")
    })
}

/// Whether the process that admitted the attempt has ended: its ingress
/// lifetime lock can be claimed. A held lock, a missing lock or an attempt
/// admitted without a lifetime proves nothing, so the answer is `false`.
async fn admitting_process_gone(
    state: &AppState,
    workspace: &str,
    attempt: &RfAttempt,
) -> Result<bool, Box<Response>> {
    let Some(lifetime) = attempt.lifetime.as_deref() else {
        return Ok(false);
    };
    let deadline = std::time::Instant::now() + LIVENESS_DEADLINE;
    match state.mem.claim_ingress_recovery(workspace, lifetime, deadline).await {
        // The claim is released when it drops at the end of this arm.
        Ok(claim) => Ok(claim.is_some()),
        Err(error) if error.is::<crate::memory::ingress_recovery::IngressLockMissing>() => Ok(false),
        Err(error) => {
            tracing::warn!(
                error_kind = super::console_error_kind(&error),
                "rf attempt liveness check failed"
            );
            Err(Box::new(storage_unavailable("the run's process could not be checked")))
        }
    }
}

/// The answer when the admission transaction rolled back: user-message
/// recording is off (`storage_unavailable`), or another admission of the
/// key committed first, which is answered like a replay.
pub(super) async fn refused_admission(state: &AppState, rf: Option<RfTurn>, error: &anyhow::Error) -> Response {
    let refusal = RfAdmissionRefused::of(error);
    match rf {
        Some(RfTurn { slot, admission, .. }) if refusal != Some(RfAdmissionRefused::Unrecorded) => {
            drop(slot);
            lost_admission(state, &admission.key, &admission.request_hash).await
        }
        _ => storage_unavailable(UNRECORDED),
    }
}

/// The answer after another admission of the key committed first: the
/// replay answer of the winning attempt (`session_busy` if it is restartable
/// again).
pub(super) async fn lost_admission(state: &AppState, key: &RfMessageKey, request_hash: &str) -> Response {
    match lookup(state, key, request_hash).await {
        Lookup::Answer(response) => response,
        Lookup::First | Lookup::Restart(_) => session_busy(),
    }
}

/// The answer of a refused context admission.
pub(super) async fn refused_context(state: &AppState, key: &RfMessageKey, request_hash: &str, error: &anyhow::Error) -> Response {
    if RfAdmissionRefused::of(error) == Some(RfAdmissionRefused::Unrecorded) {
        return storage_unavailable(UNRECORDED);
    }
    lost_admission(state, key, request_hash).await
}
