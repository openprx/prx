//! Message and reply deletes on caller-keyed sessions (AC-1b2b,
//! `message_delete_v1`).
//!
//! A delete tombstones the stored rows of a message (every attempt's input,
//! the replies and the tool and working rows of their runs) or of one run's
//! assistant side, in one transaction with the deleted mark on the attempt
//! record (`memory/sqlite/rf_delete.rs`). A run whose job, or a run it
//! spawned, is still live here keeps its event rows until the last of them
//! ends: its conversation rows are tombstoned at once, the job's in-memory
//! result is erased now and when it finishes, and [`settle_deleted`] rewrites
//! the event rows once nothing of the family is live (one waiter per session
//! re-settles each time a run ends; the first settle of a workspace in this
//! process also settles what a restart left pending). Nothing is cancelled
//! and what tools did is not undone.
//!
//! With `"forget_writes": true` (`message_forget_writes_v1`) the same
//! transaction also deletes the memories and documents the deleted runs and
//! their descendants wrote, and each answer reports what was forgotten.

use super::super::ApiPrincipal;
use super::super::capabilities::{coded_error, rf_session_refusal};
use super::rf_message::{is_message_id, live_runs, require_session_owner, runs_in_flight};
use super::target::{AgentField, TurnTarget};
use crate::gateway::{AppState, jobs};
use crate::memory::{RfDeletedAgent, RfForget, RfForgetCounts, RfForgetRefused, RfMessageDelete, RfReplyDelete};
use axum::{
    Json,
    body::{Bytes, to_bytes},
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::LazyLock;

/// The most ids one batch delete takes.
const BATCH_MAX: usize = 100;
/// The largest delete body read: 100 ids of 128 bytes fit with room.
const BODY_MAX_BYTES: usize = 64 * 1024;

const AGENT_RULE: &str = "agent must be \"main\" or an agent name";
const BODY_RULE: &str =
    "the body must be {\"agent\"?,\"forget_writes\"?} with agent a string and forget_writes a boolean";
const REPLY_BODY_RULE: &str = "forget_writes must be a boolean";
const BATCH_RULE: &str = "the body must be {\"ids\":[1 to 100 client_message_id strings],\"agent\"?,\"forget_writes\"?} with forget_writes a boolean";

fn field_invalid(message: &str) -> Response {
    coded_error(StatusCode::BAD_REQUEST, "field_invalid", message)
}

fn storage_unavailable(message: &str) -> Response {
    coded_error(StatusCode::INTERNAL_SERVER_ERROR, "storage_unavailable", message)
}

const MESSAGE_UNKNOWN: &str = "this session has no record of this message for this owner";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteBody {
    #[serde(default, deserialize_with = "super::present_value")]
    agent: Option<serde_json::Value>,
    #[serde(default)]
    forget_writes: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchBody {
    ids: Vec<String>,
    #[serde(default, deserialize_with = "super::present_value")]
    agent: Option<serde_json::Value>,
    #[serde(default)]
    forget_writes: bool,
}

#[derive(Serialize)]
struct DeletedAgent {
    agent: String,
    runs: Vec<String>,
    /// Present only when the request asked to forget writes.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    forget: Option<WireForget>,
}

#[derive(Serialize)]
struct WireCounts {
    created: i64,
    touched: i64,
}

#[derive(Serialize)]
struct WireForgotten {
    memories: WireCounts,
    documents: WireCounts,
}

/// `forgotten` is `null` while a live run's forget waits for its end;
/// `forget_pending` appears only then.
#[derive(Serialize)]
struct WireForget {
    forgotten: Option<WireForgotten>,
    writes_traceable: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    forget_pending: bool,
}

impl From<RfForget> for WireForget {
    fn from(forget: RfForget) -> Self {
        Self {
            forgotten: forget.counts.map(|counts: RfForgetCounts| WireForgotten {
                memories: WireCounts {
                    created: counts.memories_created,
                    touched: counts.memories_touched,
                },
                documents: WireCounts {
                    created: counts.documents_created,
                    touched: counts.documents_touched,
                },
            }),
            writes_traceable: forget.writes_traceable,
            forget_pending: forget.counts.is_none(),
        }
    }
}

/// A failed delete: the route's coded error.
#[derive(Debug, Clone, Copy)]
enum DeleteFailure {
    Storage,
    FamilyTooLarge,
}

impl DeleteFailure {
    fn of(error: &anyhow::Error) -> Self {
        match RfForgetRefused::of(error) {
            Some(RfForgetRefused::FamilyTooLarge) => Self::FamilyTooLarge,
            None => Self::Storage,
        }
    }

    const fn code(self) -> &'static str {
        match self {
            Self::Storage => "storage_unavailable",
            Self::FamilyTooLarge => RfForgetRefused::FamilyTooLarge.code(),
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::Storage => "the message could not be deleted",
            Self::FamilyTooLarge => FAMILY_TOO_LARGE,
        }
    }

    fn response(self) -> Response {
        match self {
            Self::Storage => storage_unavailable(self.message()),
            Self::FamilyTooLarge => coded_error(StatusCode::UNPROCESSABLE_ENTITY, self.code(), self.message()),
        }
    }
}

const FAMILY_TOO_LARGE: &str = "the deleted run family wrote more than one delete can forget; nothing was deleted";

#[derive(Serialize)]
struct Deleted<'a> {
    message_id: &'a str,
    status: &'static str,
    agents: Vec<DeletedAgent>,
}

#[derive(Serialize)]
struct ItemError<'a> {
    code: &'static str,
    message: &'a str,
}

#[derive(Serialize)]
#[serde(untagged)]
enum BatchItem<'a> {
    Deleted(Deleted<'a>),
    Refused {
        message_id: &'a str,
        error: ItemError<'static>,
    },
}

/// The workspace, owner and session one delete acts in.
struct Scope {
    workspace_id: String,
    owner_id: String,
    session_key: String,
}

/// Bearer, URL and session checks shared by the three routes; the parts of
/// the request the route still needs.
async fn admit(state: &AppState, session_key: String, request: Request) -> Result<(Scope, Bytes), Box<Response>> {
    let owner_authenticated = super::super::owner_authenticated(state, request.headers());
    if let Some(refusal) = rf_session_refusal(request.uri().query(), owner_authenticated, false) {
        return Err(Box::new(refusal));
    }
    let Some(principal) = request.extensions().get::<ApiPrincipal>().cloned() else {
        return Err(Box::new(coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a valid bearer token is required",
        )));
    };
    require_session_owner(state, &session_key, &principal.owner_id, true).await?;
    let body = to_bytes(request.into_body(), BODY_MAX_BYTES)
        .await
        .map_err(|_| Box::new(field_invalid("the body could not be read within 64 KiB")))?;
    let workspace_id = state
        .config
        .pin()
        .effective
        .workspace_dir
        .to_string_lossy()
        .into_owned();
    sweep_workspace_once(state, &workspace_id).await;
    Ok((
        Scope {
            workspace_id,
            owner_id: principal.owner_id,
            session_key,
        },
        body,
    ))
}

/// The agent key a delete body names: `main`, a name, or every agent.
fn agent_key(agent: Option<serde_json::Value>) -> Result<Option<String>, Box<Response>> {
    let Some(agent) = agent else {
        return Ok(None);
    };
    match TurnTarget::from_field(AgentField::from_json(agent)) {
        Ok(TurnTarget::Main) => Ok(Some("main".to_owned())),
        Ok(TurnTarget::Named(name)) => Ok(Some(name.as_str().to_owned())),
        Err(_) => Err(Box::new(field_invalid(AGENT_RULE))),
    }
}

/// Delete one message; `None` when no attempt matches.
async fn delete_one(
    state: &AppState,
    scope: &Scope,
    client_message_id: &str,
    agent: Option<&str>,
    forget_writes: bool,
) -> Result<Option<Vec<DeletedAgent>>, DeleteFailure> {
    if !is_message_id(client_message_id) {
        return Ok(None);
    }
    let request = RfMessageDelete {
        workspace_id: scope.workspace_id.clone(),
        owner_id: scope.owner_id.clone(),
        session_key: scope.session_key.clone(),
        client_message_id: client_message_id.to_owned(),
        agent_key: agent.map(str::to_owned),
        live_runs: live_runs(&scope.workspace_id),
        forget_writes,
    };
    let agents = state.mem.rf_delete_message(request).await.map_err(|error| {
        tracing::warn!(
            error_kind = super::console_error_kind(&error),
            "rf message delete failed"
        );
        DeleteFailure::of(&error)
    })?;
    if agents.is_empty() {
        return Ok(None);
    }
    erase_jobs(agents.iter().flat_map(|agent| agent.jobs.iter()));
    // A family whose last run ended while the delete committed missed its
    // end signal; settle it now, else arm the session's waiter.
    if agents.iter().any(|agent| !agent.deferred.is_empty()) {
        settle_deleted(state, &scope.workspace_id, &scope.owner_id, &scope.session_key).await;
    }
    Ok(Some(agents.into_iter().map(wire_agent).collect()))
}

fn wire_agent(agent: RfDeletedAgent) -> DeletedAgent {
    DeletedAgent {
        agent: agent.agent_key,
        runs: agent.runs,
        forget: agent.forget.map(WireForget::from),
    }
}

fn erase_jobs<'a>(ids: impl Iterator<Item = &'a String>) {
    for id in ids {
        if let Ok(id) = uuid::Uuid::parse_str(id) {
            jobs::erase(id);
        }
    }
}

/// Sessions with a settle waiter, keyed `workspace\0owner\0session`.
static WAITERS: LazyLock<parking_lot::Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Workspaces whose pending deletes this process has swept once.
static SWEPT: LazyLock<parking_lot::Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// One settle of the session against the runs live now; `true` while an
/// attempt stays pending. A failure counts as pending (the rows stay for the
/// next call) and is logged without payload.
async fn settle_once(state: &AppState, workspace_id: &str, owner_id: &str, session_key: &str) -> bool {
    match state
        .mem
        .rf_settle_deleted(workspace_id, owner_id, session_key, live_runs(workspace_id))
        .await
    {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(
                error_kind = super::console_error_kind(&error),
                "Deleted rf rows could not be settled"
            );
            true
        }
    }
}

/// Tombstone the event rows of the session's deleted runs whose run families
/// have ended. Called when an rf job reaches its terminal phase, after a
/// delete or reply delete that deferred and before a history read or a turn.
/// While an attempt stays pending, one waiter per session settles again each
/// time a run ends (no timer). The first call for a workspace in this process
/// also settles every session a restart left pending.
pub(super) async fn settle_deleted(state: &AppState, workspace_id: &str, owner_id: &str, session_key: &str) {
    sweep_workspace_once(state, workspace_id).await;
    if settle_once(state, workspace_id, owner_id, session_key).await {
        arm_waiter(state, workspace_id, owner_id, session_key);
    }
}

/// Settle, once per workspace in this process, every session whose delete a
/// restart left pending (nothing of an earlier process is live). A failed
/// sweep is tried again by the next call.
async fn sweep_workspace_once(state: &AppState, workspace_id: &str) {
    let first_in_workspace = SWEPT.lock().insert(workspace_id.to_owned());
    if first_in_workspace
        && let Err(error) = state
            .mem
            .rf_settle_deleted_workspace(workspace_id, live_runs(workspace_id))
            .await
    {
        tracing::warn!(
            error_kind = super::console_error_kind(&error),
            "Pending rf deletes of the workspace could not be settled"
        );
        SWEPT.lock().remove(workspace_id);
    }
}

/// Settle every pending rf delete of the workspace when the gateway starts,
/// before any request and before woken runs are recovered (nothing of an
/// earlier process is live). A failure is logged by the sweep and the gateway
/// starts; the next rf request retries it.
pub(in crate::gateway) async fn settle_pending_at_startup(state: &AppState) {
    let workspace_id = state
        .config
        .pin()
        .effective
        .workspace_dir
        .to_string_lossy()
        .into_owned();
    sweep_workspace_once(state, &workspace_id).await;
}

/// Start the session's settle waiter unless it has one. It subscribes to the
/// run-end signal, settles, and on each signal settles again; it ends when
/// the session has nothing pending.
fn arm_waiter(state: &AppState, workspace_id: &str, owner_id: &str, session_key: &str) {
    let key = format!("{workspace_id}\0{owner_id}\0{session_key}");
    if !WAITERS.lock().insert(key.clone()) {
        return;
    }
    let state = state.clone();
    let (workspace_id, owner_id, session_key) = (workspace_id.to_owned(), owner_id.to_owned(), session_key.to_owned());
    tokio::spawn(async move {
        let mut ended = crate::runtime::registry::subscribe_ended();
        ended.mark_unchanged();
        loop {
            if !settle_once(&state, &workspace_id, &owner_id, &session_key).await {
                WAITERS.lock().remove(&key);
                // A delete may have deferred between the settle and the
                // removal and seen this waiter: settle once more.
                if !settle_once(&state, &workspace_id, &owner_id, &session_key).await
                    || !WAITERS.lock().insert(key.clone())
                {
                    return;
                }
            }
            if ended.changed().await.is_err() {
                // The signal lives for the process; a closed one ends the
                // wait and leaves the rows to the next settle call.
                WAITERS.lock().remove(&key);
                return;
            }
        }
    });
}

/// `POST /api/sessions/{session_id}/messages/{client_message_id}/delete`,
/// body `{"agent"?}`: `200 {"message_id","status":"deleted","agents"}`,
/// the same on a repeat; no record is `404 message_unknown`.
pub(in crate::gateway::api) async fn post_message_delete(
    State(state): State<AppState>,
    Path((session_id, client_message_id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (scope, body) = match admit(&state, session_id, request).await {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    let (agent, forget_writes) = if body.iter().all(u8::is_ascii_whitespace) {
        (None, false)
    } else {
        match serde_json::from_slice::<DeleteBody>(&body) {
            Ok(body) => (body.agent, body.forget_writes),
            Err(_) => return field_invalid(BODY_RULE),
        }
    };
    let agent = match agent_key(agent) {
        Ok(agent) => agent,
        Err(response) => return *response,
    };
    match delete_one(&state, &scope, &client_message_id, agent.as_deref(), forget_writes).await {
        Ok(Some(agents)) => Json(Deleted {
            message_id: &client_message_id,
            status: "deleted",
            agents,
        })
        .into_response(),
        Ok(None) => coded_error(StatusCode::NOT_FOUND, "message_unknown", MESSAGE_UNKNOWN),
        Err(failure) => failure.response(),
    }
}

/// `POST /api/sessions/{session_id}/messages/delete`, body
/// `{"ids":[…≤100],"agent"?}`: per-item results in request order, not
/// all-or-nothing; empty or more than 100 ids is `400 field_invalid`.
pub(in crate::gateway::api) async fn post_batch_delete(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    request: Request,
) -> Response {
    let (scope, body) = match admit(&state, session_id, request).await {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    let Ok(batch) = serde_json::from_slice::<BatchBody>(&body) else {
        return field_invalid(BATCH_RULE);
    };
    if batch.ids.is_empty() || batch.ids.len() > BATCH_MAX {
        return field_invalid(BATCH_RULE);
    }
    let agent = match agent_key(batch.agent) {
        Ok(agent) => agent,
        Err(response) => return *response,
    };
    let mut results = Vec::with_capacity(batch.ids.len());
    for id in &batch.ids {
        results.push(
            match delete_one(&state, &scope, id, agent.as_deref(), batch.forget_writes).await {
                Ok(Some(agents)) => BatchItem::Deleted(Deleted {
                    message_id: id,
                    status: "deleted",
                    agents,
                }),
                Ok(None) => BatchItem::Refused {
                    message_id: id,
                    error: ItemError {
                        code: "message_unknown",
                        message: MESSAGE_UNKNOWN,
                    },
                },
                Err(failure) => BatchItem::Refused {
                    message_id: id,
                    error: ItemError {
                        code: failure.code(),
                        message: failure.message(),
                    },
                },
            },
        );
    }
    Json(serde_json::json!({ "results": results })).into_response()
}

/// `POST /api/sessions/{session_id}/runs/{run_id}/reply/delete`: deletes
/// the assistant side of one run; `200 {"run_id","status":"deleted"}`, the
/// same on a repeat; a run still running is `409 run_active`, a run whose
/// spawned runs are still running is deleted with its rows deferred until
/// they end; no such run of this owner in this session is `404 run_unknown`.
pub(in crate::gateway::api) async fn post_reply_delete(
    State(state): State<AppState>,
    Path((session_id, run_id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (scope, body) = match admit(&state, session_id, request).await {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    let forget_writes = match reply_forget_writes(&body) {
        Ok(forget_writes) => forget_writes,
        Err(response) => return *response,
    };
    if runs_in_flight(&scope.workspace_id).contains(&run_id) {
        return run_active();
    }
    match state
        .mem
        .rf_delete_reply(
            &scope.workspace_id,
            &scope.owner_id,
            &scope.session_key,
            &run_id,
            forget_writes,
            live_runs(&scope.workspace_id),
        )
        .await
    {
        Ok(RfReplyDelete::Deleted { job_id, forget }) => {
            erase_jobs(std::iter::once(&job_id));
            // A run it spawned still running defers the rows: settle now (the
            // last one may have ended meanwhile) or arm the waiter.
            settle_deleted(&state, &scope.workspace_id, &scope.owner_id, &scope.session_key).await;
            Json(DeletedReply {
                run_id: &run_id,
                status: "deleted",
                forget: forget.map(WireForget::from),
            })
            .into_response()
        }
        Ok(RfReplyDelete::Active) => run_active(),
        Ok(RfReplyDelete::Unknown) => coded_error(
            StatusCode::NOT_FOUND,
            "run_unknown",
            "this owner has no run with this id in this session",
        ),
        Err(error) => {
            tracing::warn!(error_kind = super::console_error_kind(&error), "rf reply delete failed");
            match DeleteFailure::of(&error) {
                DeleteFailure::Storage => storage_unavailable("the reply could not be deleted"),
                failure @ DeleteFailure::FamilyTooLarge => failure.response(),
            }
        }
    }
}

/// The reply delete's answer; the forget fields sit beside `run_id` (the
/// reply of one run is one agent's entry).
#[derive(Serialize)]
struct DeletedReply<'a> {
    run_id: &'a str,
    status: &'static str,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    forget: Option<WireForget>,
}

/// `forget_writes` of a reply delete body. The route took no body before, so
/// anything but an object naming `forget_writes` is ignored as it was; a
/// non-boolean `forget_writes` is `400 field_invalid`.
fn reply_forget_writes(body: &[u8]) -> Result<bool, Box<Response>> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(false);
    }
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Ok(false);
    };
    match fields.get("forget_writes") {
        None => Ok(false),
        Some(serde_json::Value::Bool(forget_writes)) => Ok(*forget_writes),
        Some(_) => Err(Box::new(field_invalid(REPLY_BODY_RULE))),
    }
}

fn run_active() -> Response {
    coded_error(
        StatusCode::CONFLICT,
        "run_active",
        "the run is still running; cancel it first",
    )
}
