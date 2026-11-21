//! Message and reply deletes on caller-keyed sessions (AC-1b2b,
//! `message_delete_v1`).
//!
//! A delete tombstones the stored rows of a message (every attempt's input,
//! the replies and the tool and working rows of their runs) or of one run's
//! assistant side, in one transaction with the deleted mark on the attempt
//! record (`memory/sqlite/rf_delete.rs`). A run whose job is still live here
//! keeps its event rows until the job ends: its conversation rows are
//! tombstoned at once, the job's in-memory result is erased now and when it
//! finishes, and [`settle_deleted`] rewrites the event rows after the job's
//! terminal phase. Nothing is cancelled and what tools did is not undone.

use super::super::ApiPrincipal;
use super::super::capabilities::{coded_error, rf_session_refusal};
use super::rf_message::{is_message_id, require_session_owner, runs_in_flight};
use super::target::{AgentField, TurnTarget};
use crate::gateway::{AppState, jobs};
use crate::memory::{RfDeletedAgent, RfMessageDelete, RfReplyDelete};
use axum::{
    Json,
    body::{Bytes, to_bytes},
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

/// The most ids one batch delete takes.
const BATCH_MAX: usize = 100;
/// The largest delete body read: 100 ids of 128 bytes fit with room.
const BODY_MAX_BYTES: usize = 64 * 1024;

const AGENT_RULE: &str = "agent must be \"main\" or an agent name";
const BODY_RULE: &str = "the body must be {\"agent\"?} with agent a string";
const BATCH_RULE: &str = "the body must be {\"ids\":[1 to 100 client_message_id strings],\"agent\"?}";

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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchBody {
    ids: Vec<String>,
    #[serde(default, deserialize_with = "super::present_value")]
    agent: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct DeletedAgent {
    agent: String,
    runs: Vec<String>,
}

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
) -> Result<Option<Vec<DeletedAgent>>, Box<Response>> {
    if !is_message_id(client_message_id) {
        return Ok(None);
    }
    let request = RfMessageDelete {
        workspace_id: scope.workspace_id.clone(),
        owner_id: scope.owner_id.clone(),
        session_key: scope.session_key.clone(),
        client_message_id: client_message_id.to_owned(),
        agent_key: agent.map(str::to_owned),
        live_runs: runs_in_flight(&scope.workspace_id),
    };
    let agents = state.mem.rf_delete_message(request).await.map_err(|error| {
        tracing::warn!(
            error_kind = super::console_error_kind(&error),
            "rf message delete failed"
        );
        Box::new(storage_unavailable("the message could not be deleted"))
    })?;
    if agents.is_empty() {
        return Ok(None);
    }
    erase_jobs(agents.iter().flat_map(|agent| agent.jobs.iter()));
    // A deferred run whose job ended while the delete committed missed the
    // job-end settle; settle it now (a no-op when nothing is pending).
    let in_flight = runs_in_flight(&scope.workspace_id);
    if agents
        .iter()
        .flat_map(|agent| agent.deferred.iter())
        .any(|run| !in_flight.contains(run))
    {
        settle_deleted(state, &scope.workspace_id, &scope.owner_id, &scope.session_key).await;
    }
    Ok(Some(agents.into_iter().map(wire_agent).collect()))
}

fn wire_agent(agent: RfDeletedAgent) -> DeletedAgent {
    DeletedAgent {
        agent: agent.agent_key,
        runs: agent.runs,
    }
}

fn erase_jobs<'a>(ids: impl Iterator<Item = &'a String>) {
    for id in ids {
        if let Ok(id) = uuid::Uuid::parse_str(id) {
            jobs::erase(id);
        }
    }
}

/// Tombstone the event rows of the session's deleted runs whose jobs have
/// ended. Called when an rf job reaches its terminal phase, after a delete
/// and before a history read. A failure leaves the rows pending for the
/// next call and is logged without payload.
pub(super) async fn settle_deleted(state: &AppState, workspace_id: &str, owner_id: &str, session_key: &str) {
    if let Err(error) = state
        .mem
        .rf_settle_deleted(workspace_id, owner_id, session_key, runs_in_flight(workspace_id))
        .await
    {
        tracing::warn!(
            error_kind = super::console_error_kind(&error),
            "Deleted rf rows could not be settled"
        );
    }
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
    let agent = if body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        match serde_json::from_slice::<DeleteBody>(&body) {
            Ok(body) => body.agent,
            Err(_) => return field_invalid(BODY_RULE),
        }
    };
    let agent = match agent_key(agent) {
        Ok(agent) => agent,
        Err(response) => return *response,
    };
    match delete_one(&state, &scope, &client_message_id, agent.as_deref()).await {
        Ok(Some(agents)) => Json(Deleted {
            message_id: &client_message_id,
            status: "deleted",
            agents,
        })
        .into_response(),
        Ok(None) => coded_error(StatusCode::NOT_FOUND, "message_unknown", MESSAGE_UNKNOWN),
        Err(response) => *response,
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
        results.push(match delete_one(&state, &scope, id, agent.as_deref()).await {
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
            Err(_) => BatchItem::Refused {
                message_id: id,
                error: ItemError {
                    code: "storage_unavailable",
                    message: "the message could not be deleted",
                },
            },
        });
    }
    Json(serde_json::json!({ "results": results })).into_response()
}

/// `POST /api/sessions/{session_id}/runs/{run_id}/reply/delete`: deletes
/// the assistant side of one run; `200 {"run_id","status":"deleted"}`, the
/// same on a repeat; a run still running is `409 run_active`; no such run of
/// this owner in this session is `404 run_unknown`.
pub(in crate::gateway::api) async fn post_reply_delete(
    State(state): State<AppState>,
    Path((session_id, run_id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (scope, _) = match admit(&state, session_id, request).await {
        Ok(admitted) => admitted,
        Err(response) => return *response,
    };
    if runs_in_flight(&scope.workspace_id).contains(&run_id) {
        return run_active();
    }
    match state
        .mem
        .rf_delete_reply(&scope.workspace_id, &scope.owner_id, &scope.session_key, &run_id)
        .await
    {
        Ok(RfReplyDelete::Deleted { job_id }) => {
            erase_jobs(std::iter::once(&job_id));
            Json(serde_json::json!({ "run_id": run_id, "status": "deleted" })).into_response()
        }
        Ok(RfReplyDelete::Active) => run_active(),
        Ok(RfReplyDelete::Unknown) => coded_error(
            StatusCode::NOT_FOUND,
            "run_unknown",
            "this owner has no run with this id in this session",
        ),
        Err(error) => {
            tracing::warn!(error_kind = super::console_error_kind(&error), "rf reply delete failed");
            storage_unavailable("the reply could not be deleted")
        }
    }
}

fn run_active() -> Response {
    coded_error(
        StatusCode::CONFLICT,
        "run_active",
        "the run is still running; cancel it first",
    )
}
