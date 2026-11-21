//! Replay, the restart rule and coded cancel of rf messages (AC-1b2a,
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

use super::super::ApiPrincipal;
use super::super::capabilities::{coded_error, rf_session_refusal};
use super::rf_message::{LiveRun, TurnSlot, live_run};
use crate::gateway::{AppState, jobs};
use crate::memory::{RfAdmission, RfAdmissionRefused, RfAttempt, RfAttemptLookup, RfMessageKey, RfRunTerminal};
use axum::{
    Json,
    extract::{Path, Request, State},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
};

/// The longest wait for the liveness claim on the admitting process's lock.
const LIVENESS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

const UNRECORDED: &str = "message idempotency needs user-message event recording (memory.events.record_user_messages)";

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

fn run_unknown() -> Response {
    coded_error(
        StatusCode::NOT_FOUND,
        "run_unknown",
        "this owner has no run with this id in this session",
    )
}

fn job_unknown() -> Response {
    coded_error(
        StatusCode::NOT_FOUND,
        "job_unknown",
        "the job is not known to this process; its outcome is unknown, not \"not run\"",
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
    if !matches!(
        live_run(workspace, &attempt.session_key, &attempt.agent_key, run_id, false),
        LiveRun::Absent
    ) {
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
async fn admitting_process_gone(state: &AppState, workspace: &str, attempt: &RfAttempt) -> Result<bool, Box<Response>> {
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
pub(super) async fn refused_context(
    state: &AppState,
    key: &RfMessageKey,
    request_hash: &str,
    error: &anyhow::Error,
) -> Response {
    if RfAdmissionRefused::of(error) == Some(RfAdmissionRefused::Unrecorded) {
        return storage_unavailable(UNRECORDED);
    }
    lost_admission(state, key, request_hash).await
}

/// Cancel an rf run: a run that ended answers its terminal status; a run
/// live here is killed (`cancelled` once its job ended, else
/// `cancelling`); a run whose admitting process is gone gets its terminal
/// `interrupted` (`failed`); a run held by another live process, or ended
/// here without a confirmed terminal, is `cancelling`.
async fn cancel_run(state: &AppState, workspace: &str, attempt: RfAttempt) -> Result<&'static str, Box<Response>> {
    let Some(run) = attempt.run.clone() else {
        return Err(Box::new(run_unknown()));
    };
    if let Some(terminal) = attempt.terminal {
        return Ok(terminal.status());
    }
    match live_run(workspace, &attempt.session_key, &attempt.agent_key, &run.run_id, true) {
        LiveRun::Job(job) => {
            if let Some(work) = jobs::work_id_of(job) {
                crate::runtime::registry::kill(work, true).await;
            }
            let ended = jobs::snapshot(job).is_some_and(|snapshot| snapshot.status == "cancelled");
            return Ok(if ended { "cancelled" } else { "cancelling" });
        }
        // The job starts after admission; it is killed as soon as it exists.
        LiveRun::Admitting => return Ok("cancelling"),
        LiveRun::Absent => {}
    }
    if !admitting_process_gone(state, workspace, &attempt).await? {
        return Ok("cancelling");
    }
    match state
        .mem
        .interrupt_rf_run(workspace, &attempt.owner_id, &run.run_id)
        .await
    {
        Ok(true) => Ok(RfRunTerminal::Interrupted.status()),
        // A terminal row landed first: answer it.
        Ok(false) => {
            let lookup = RfAttemptLookup::Run {
                owner_id: attempt.owner_id,
                session_key: attempt.session_key,
                run_id: run.run_id,
            };
            Ok(read(state, workspace, lookup)
                .await?
                .and_then(|attempt| attempt.terminal)
                .map_or(RfRunTerminal::Interrupted.status(), RfRunTerminal::status))
        }
        Err(error) => {
            tracing::warn!(
                error_kind = super::console_error_kind(&error),
                "rf run interruption failed"
            );
            Err(Box::new(storage_unavailable("the run's terminal could not be stored")))
        }
    }
}

fn workspace_of(state: &AppState) -> String {
    state
        .config
        .pin()
        .effective
        .workspace_dir
        .to_string_lossy()
        .into_owned()
}

/// `POST /api/sessions/{session_id}/runs/{run_id}/cancel`: answers
/// `200 {"run_id","status"}`; no such run of this owner in this session is
/// `404 run_unknown`. Behind the rf contract guard.
pub(in crate::gateway::api) async fn post_run_cancel(
    State(state): State<AppState>,
    Path((session_id, run_id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let owner_authenticated = super::super::owner_authenticated(&state, request.headers());
    if let Some(refusal) = rf_session_refusal(request.uri().query(), owner_authenticated, false) {
        return refusal;
    }
    let Some(principal) = request.extensions().get::<ApiPrincipal>() else {
        return coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a valid bearer token is required",
        );
    };
    let workspace = workspace_of(&state);
    let lookup = RfAttemptLookup::Run {
        owner_id: principal.owner_id.clone(),
        session_key: session_id,
        run_id: run_id.clone(),
    };
    let attempt = match read(&state, &workspace, lookup).await {
        Ok(Some(attempt)) => attempt,
        Ok(None) => return run_unknown(),
        Err(response) => return *response,
    };
    match cancel_run(&state, &workspace, attempt).await {
        Ok(status) => Json(serde_json::json!({ "run_id": run_id, "status": status })).into_response(),
        Err(response) => *response,
    }
}

/// The rf form of `POST /api/jobs/{job_id}/cancel`. A job is an rf run when
/// its in-memory record carries a run id, or, once it is gone, when an rf
/// attempt row names it. `None` leaves every other job to the Console path,
/// unchanged. An rf run answers `200 {"job_id","run_id","status"}` while its
/// job is known; an evicted job, one from before a restart or another
/// owner's is `404 job_unknown`.
pub(in crate::gateway::api) async fn rf_job_cancel(
    state: &AppState,
    raw_id: &str,
    request: &Parts,
) -> Option<Response> {
    let job = uuid::Uuid::parse_str(raw_id.trim()).ok()?;
    let known = match jobs::snapshot(job) {
        Some(snapshot) if snapshot.run_id.is_none() => return None,
        Some(_) => true,
        None => false,
    };
    let workspace = workspace_of(state);
    let attempt = match state
        .mem
        .rf_message_attempt(&workspace, RfAttemptLookup::Job(job.to_string()))
        .await
    {
        Ok(attempt) => attempt,
        // An unknown job whose record cannot be read stays a Console 404.
        Err(_) if !known => return None,
        Err(error) => {
            tracing::warn!(
                error_kind = super::console_error_kind(&error),
                "rf job record lookup failed"
            );
            return Some(storage_unavailable("the message record could not be read"));
        }
    };
    let Some(attempt) = attempt else {
        return known.then(job_unknown);
    };
    let owner_authenticated = super::super::owner_authenticated(state, &request.headers);
    if let Some(refusal) = rf_session_refusal(request.uri.query(), owner_authenticated, false) {
        return Some(refusal);
    }
    let owner = request
        .extensions
        .get::<ApiPrincipal>()
        .map(|principal| principal.owner_id.as_str());
    let run_id = attempt.run.as_ref().map(|run| run.run_id.clone());
    let Some(run_id) = run_id.filter(|_| known && owner == Some(attempt.owner_id.as_str())) else {
        return Some(job_unknown());
    };
    Some(match cancel_run(state, &workspace, attempt).await {
        Ok(status) => {
            Json(serde_json::json!({ "job_id": job.to_string(), "run_id": run_id, "status": status })).into_response()
        }
        Err(response) => *response,
    })
}
