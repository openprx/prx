//! Owner-authenticated history erasure. The store persists the fence before
//! cancellation; no successful response precedes verified payload removal.
use super::{ApiPrincipal, AppState, capabilities::coded_error};
use crate::memory::agent_erasure::{ErasePrepared, EraseWork, ErasureRefusal, ErasureStep, HistoryErase};
use anyhow::Context;
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::time::Duration;

pub(super) fn failure(error: &anyhow::Error, purge: bool) -> Response {
    let step: Option<&ErasureStep> = error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<ErasureStep>())
        .last();
    if step.is_some() || error.downcast_ref::<ErasureRefusal>().is_none() {
        tracing::warn!(step=?step,error_kind=%crate::agent::diagnostics::error_class(error),"agent erasure did not complete");
    }
    let (status, code) = match error.downcast_ref::<ErasureRefusal>() {
        Some(ErasureRefusal::IdempotencyConflict) => (StatusCode::CONFLICT, "idempotency_conflict"),
        Some(ErasureRefusal::NotRetired) => (StatusCode::CONFLICT, "agent_not_retired"),
        Some(ErasureRefusal::Unknown) => (StatusCode::NOT_FOUND, "agent_unknown"),
        Some(ErasureRefusal::Unsupported) => (StatusCode::CONFLICT, "agent_backend_unsupported"),
        Some(ErasureRefusal::RevisionConflict(current)) => {
            return super::capabilities::coded_error_with(
                StatusCode::CONFLICT,
                "revision_conflict",
                "the config revision changed",
                Some(("current_revision", current)),
            );
        }
        Some(ErasureRefusal::IncarnationMismatch(current)) => {
            return super::capabilities::coded_error_with(
                StatusCode::CONFLICT,
                "incarnation_mismatch",
                "the incarnation changed",
                Some(("current_incarnation", current)),
            );
        }
        Some(ErasureRefusal::Pending) => (
            StatusCode::SERVICE_UNAVAILABLE,
            if purge { "purge_pending" } else { "history_pending" },
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            if purge { "purge_pending" } else { "history_pending" },
        ),
    };
    let message = match code {
        "idempotency_conflict" => "this idempotency key belongs to another request",
        "agent_not_retired" => "retire the named agent before requesting purge",
        "agent_unknown" => "no such agent; reconcile the agent inventory",
        "agent_backend_unsupported" => "the memory backend does not support agent erasure",
        _ => "agent erasure is incomplete; retry with the same idempotency key",
    };
    coded_error(status, code, message)
}

pub(super) fn idempotency_key(headers: &HeaderMap) -> Result<String, Box<Response>> {
    match headers.get("Idempotency-Key").and_then(|value| value.to_str().ok()) {
        Some(value) if (16..=128).contains(&value.len()) && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) => {
            Ok(value.to_owned())
        }
        _ => Err(Box::new(coded_error(
            StatusCode::BAD_REQUEST,
            "idempotency_key_required",
            "Idempotency-Key must be 16 to 128 printable ASCII bytes",
        ))),
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct EraseBody {
    #[serde(default)]
    forget_writes: bool,
}

pub(super) async fn delete_history(
    State(state): State<AppState>,
    Path((session, agent)): Path<(String, String)>,
    Extension(principal): Extension<ApiPrincipal>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    history(state, (session, agent), principal, headers, uri, body, false).await
}

/// Read the exact incarnation needed by the paired terminal erase operation.
pub(super) async fn get_terminal_history(
    State(state): State<AppState>,
    Path((session, agent)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !super::owner_authenticated(&state, &headers) {
        return coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a paired owner bearer is required",
        );
    }
    let config = state.config.load_full();
    match crate::runtime::blocking::spawn_blocking(move || {
        let owner =
            crate::chat::chat_runtime_envelope(&config.workspace_dir.to_string_lossy(), &session).resolved_owner_id();
        let runtime = crate::messaging::runtime::MessagingRuntime::for_config(&config)?;
        let target = runtime.address(&owner, &session, &agent);
        runtime.resolve_scope(&target).map_err(anyhow::Error::from)
    })
    .await
    {
        Ok(Ok(scope)) => {
            Json(serde_json::json!({"incarnation":scope.incarnation,"generation":scope.generation})).into_response()
        }
        _ => coded_error(
            StatusCode::NOT_FOUND,
            "session_unknown",
            "no terminal session of this installation has this key",
        ),
    }
}

/// Paired local operators may erase their terminal sessions without rebinding
/// them to the Console owner or trusting an owner supplied in request JSON.
pub(super) async fn delete_terminal_history(
    State(state): State<AppState>,
    Path(path): Path<(String, String)>,
    Extension(principal): Extension<ApiPrincipal>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    history(state, path, principal, headers, uri, body, true).await
}

async fn history(
    state: AppState,
    (session, agent): (String, String),
    mut principal: ApiPrincipal,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
    terminal: bool,
) -> Response {
    if !super::owner_authenticated(&state, &headers) {
        return coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a paired owner bearer is required",
        );
    }
    if uri.query().is_some() {
        return coded_error(
            StatusCode::BAD_REQUEST,
            "field_invalid",
            "this route takes no query parameters",
        );
    }
    let name = match crate::config::agent_name::AgentName::parse(&agent) {
        Ok(name) if name.is_main() || !name.is_reserved() => name,
        _ => return coded_error(StatusCode::BAD_REQUEST, "agent_invalid", "invalid agent name"),
    };
    // The terminal owner is derived exclusively from this installation's workspace.
    if terminal {
        let config = state.config.load_full();
        let owner =
            crate::chat::chat_runtime_envelope(&config.workspace_dir.to_string_lossy(), &session).resolved_owner_id();
        let target_session = session.clone();
        let target_agent = agent.clone();
        let selected_owner = owner.clone();
        let known = crate::runtime::blocking::spawn_blocking(move || {
            let runtime = crate::messaging::runtime::MessagingRuntime::for_config(&config)?;
            let target = runtime.address(&selected_owner, &target_session, &target_agent);
            runtime.resolve_scope(&target).map_err(anyhow::Error::from)
        })
        .await;
        if !matches!(known, Ok(Ok(_))) {
            return coded_error(
                StatusCode::NOT_FOUND,
                "session_unknown",
                "no terminal session of this installation has this key",
            );
        }
        principal.owner_id = owner;
    } else {
        // Ownership is checked before idempotency or incarnation disclosure.
        match state.mem.conversation_session_owner(&session).await {
            Ok(Some(row))
                if row.channel == crate::memory::CALLER_KEYED_SESSION_CHANNEL
                    && row.owner_id.as_deref() == Some(principal.owner_id.as_str()) => {}
            Ok(_) => {
                return coded_error(
                    StatusCode::NOT_FOUND,
                    "session_unknown",
                    "no caller-keyed session of this owner has this id",
                );
            }
            Err(error) => return failure(&error, false),
        }
    }
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(response) => return *response,
    };
    let incarnation = match headers
        .get(header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_matches('"'))
    {
        Some(value) if uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value) => value.to_owned(),
        _ => {
            return coded_error(
                StatusCode::BAD_REQUEST,
                "field_invalid",
                "If-Match must name a non-null incarnation UUID",
            );
        }
    };
    let body = if body.is_empty() {
        EraseBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(body) => body,
            Err(_) => {
                return coded_error(
                    StatusCode::BAD_REQUEST,
                    "field_invalid",
                    "the body must be an object with optional boolean forget_writes",
                );
            }
        }
    };
    let config = state.config.load_full();
    let request = HistoryErase {
        workspace: config.workspace_dir.to_string_lossy().into_owned(),
        owner: principal.owner_id,
        session: session.clone(),
        agent: agent.clone(),
        incarnation,
        key,
        forget_writes: body.forget_writes,
        configured: name.is_main() || config.agents.contains_key(&agent),
    };
    let memory = std::sync::Arc::clone(&state.mem);
    let prepared = crate::runtime::blocking::spawn_blocking(move || {
        let registry = memory.agent_registry().ok_or(ErasureRefusal::Unsupported)?;
        registry
            .begin_history_erase(&request)
            .context(ErasureStep::PrepareHistory)
    })
    .await;
    let work = match prepared {
        Ok(Ok(ErasePrepared::Replay(receipt))) => return Json(receipt).into_response(),
        Ok(Ok(ErasePrepared::Work(work))) => work,
        Ok(Err(error)) => return failure(&error, false),
        Err(error) => return failure(&error.into(), false),
    };
    if let Err(error) = drain(&work).await.context(ErasureStep::DrainJobs) {
        return failure(&error, false);
    }
    let memory = std::sync::Arc::clone(&state.mem);
    match crate::runtime::blocking::spawn_blocking(move || {
        memory
            .agent_registry()
            .ok_or(ErasureRefusal::Unsupported)?
            .finish_history_erase(&work)
            .context(ErasureStep::FinishHistory)
    })
    .await
    {
        Ok(Ok(receipt)) => {
            // The erased session starts without usage anchors if it is created again.
            crate::context::budget::session_reserves::forget_session_tool_surface_reserve(&session);
            Json(receipt).into_response()
        }
        Ok(Err(error)) => failure(&error, false),
        Err(error) => failure(&error.into(), false),
    }
}

pub(super) async fn drain(work: &EraseWork) -> anyhow::Result<()> {
    let work_to_drain = async {
        // Erase cached replies before signalling, so a concurrent poll cannot
        // read the result while cancellation is being joined.
        for job in &work.jobs {
            let id = uuid::Uuid::parse_str(job)?;
            crate::gateway::jobs::erase(id);
            if let Some(id) = crate::gateway::jobs::work_id_of(id) {
                if crate::runtime::registry::kill(id, true).await.iter().any(|result| {
                    matches!(
                        result.outcome,
                        crate::runtime::registry::KillOutcome::Requested
                            | crate::runtime::registry::KillOutcome::NotKillable
                    )
                }) {
                    return Err(ErasureRefusal::Pending.into());
                }
            }
        }
        for item in crate::runtime::registry::snapshot_all() {
            if item
                .run_id
                .as_deref()
                .is_some_and(|run| work.runs.iter().any(|id| id == run))
            {
                if crate::runtime::registry::kill(item.id, true)
                    .await
                    .iter()
                    .any(|result| {
                        matches!(
                            result.outcome,
                            crate::runtime::registry::KillOutcome::Requested
                                | crate::runtime::registry::KillOutcome::NotKillable
                        )
                    })
                {
                    return Err(ErasureRefusal::Pending.into());
                }
            }
        }
        for job in &work.jobs {
            if crate::gateway::jobs::snapshot(uuid::Uuid::parse_str(job)?).is_some_and(|job| job.status == "running") {
                return Err(ErasureRefusal::Pending.into());
            }
        }
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(10), work_to_drain)
        .await
        .map_err(|_| ErasureRefusal::Pending)?
}

pub(super) async fn post_purge(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Extension(principal): Extension<ApiPrincipal>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if !super::owner_authenticated(&state, &headers) {
        return coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a paired owner bearer is required",
        );
    }
    if uri.query().is_some()
        || (!body.is_empty() && serde_json::from_slice::<serde_json::Value>(&body).ok() != Some(serde_json::json!({})))
    {
        return coded_error(
            StatusCode::BAD_REQUEST,
            "field_invalid",
            "purge takes no query parameters or body fields",
        );
    }
    match crate::config::agent_name::AgentName::parse(&agent) {
        Ok(name) if !name.is_reserved() => {}
        _ => return coded_error(StatusCode::BAD_REQUEST, "agent_invalid", "purge requires a named agent"),
    }
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(response) => return *response,
    };
    let expected = match super::config::if_match_revision(&headers) {
        Ok(Some(revision)) => revision,
        _ => {
            return coded_error(
                StatusCode::BAD_REQUEST,
                "field_invalid",
                "If-Match must name a configuration revision",
            );
        }
    };
    let desired = state.config.desired();
    let Some(current) = crate::config::manage::ConfigTransactions::base_revision(&desired) else {
        return coded_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_unavailable",
            "no config revision is available",
        );
    };
    let request = crate::memory::agent_erasure::AgentPurge {
        workspace: desired.config.workspace_dir.to_string_lossy().into_owned(),
        owner: principal.owner_id,
        agent: agent.clone(),
        key,
        expected_revision: expected,
        current_revision: current,
    };
    let memory = std::sync::Arc::clone(&state.mem);
    let prepared = crate::runtime::blocking::spawn_blocking(move || {
        memory
            .agent_registry()
            .ok_or(ErasureRefusal::Unsupported)?
            .begin_agent_purge(&request)
            .context(ErasureStep::PreparePurge)
    })
    .await;
    let work = match prepared {
        Ok(Ok(ErasePrepared::Replay(receipt))) => return Json(receipt).into_response(),
        Ok(Ok(ErasePrepared::Work(work))) => work,
        Ok(Err(error)) => return failure(&error, true),
        Err(error) => return failure(&error.into(), true),
    };
    if let Err(error) = drain(&work).await.context(ErasureStep::DrainJobs) {
        return failure(&error, true);
    }
    let memory = std::sync::Arc::clone(&state.mem);
    let erased_work = work.clone();
    match crate::runtime::blocking::spawn_blocking(move || {
        memory
            .agent_registry()
            .ok_or(ErasureRefusal::Unsupported)?
            .finish_history_erase(&erased_work)
            .context(ErasureStep::FinishHistory)
    })
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return failure(&error, true),
        Err(error) => return failure(&error.into(), true),
    }
    if let Err(response) = super::agents::purge_config(&state, &agent, &headers).await {
        return *response;
    }
    let memory = std::sync::Arc::clone(&state.mem);
    match crate::runtime::blocking::spawn_blocking(move || {
        memory
            .agent_registry()
            .ok_or(ErasureRefusal::Unsupported)?
            .finish_agent_purge(&work)
            .context(ErasureStep::FinishPurge)
    })
    .await
    {
        Ok(Ok(receipt)) => Json(receipt).into_response(),
        Ok(Err(error)) => failure(&error, true),
        Err(error) => failure(&error.into(), true),
    }
}
