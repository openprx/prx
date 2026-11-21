//! Job endpoints: what long work was submitted, how it ended, and how to stop it.
//!
//! The gateway has no request timeout, so anything that can run long is
//! submitted as a detached job instead of being awaited inside the request
//! future (see [`crate::gateway::jobs`]). These endpoints are the collection
//! side of that split: a caller that submitted with `?mode=async` polls here for
//! status and result, and can cancel through the same runtime registry that
//! backs `prx tasks kill`.

use super::{AppState, authorize_resource_mutation};
use crate::gateway::jobs;
use crate::security::policy::ResourceRiskLevel;
use axum::{
    Json,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize)]
pub(super) struct JobsResponse {
    jobs: Vec<jobs::JobSnapshot>,
    total: usize,
}

pub async fn get_jobs() -> Json<JobsResponse> {
    let jobs = jobs::snapshot_all();
    let total = jobs.len();
    Json(JobsResponse { jobs, total })
}

fn parse_job_id(raw: &str) -> Result<Uuid, (StatusCode, Json<serde_json::Value>)> {
    Uuid::parse_str(raw.trim()).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid job id '{raw}'") })),
        )
    })
}

fn job_not_found(raw: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": format!("no job with id '{raw}'") })),
    )
}

pub async fn get_job(Path(id): Path<String>) -> Result<Json<jobs::JobSnapshot>, (StatusCode, Json<serde_json::Value>)> {
    let job_id = parse_job_id(&id)?;
    jobs::snapshot(job_id).map(Json).ok_or_else(|| job_not_found(&id))
}

#[derive(Serialize)]
pub(super) struct JobCancelResponse {
    job_id: String,
    work_id: String,
    /// Per-target results from the runtime registry, so a cancel that could not
    /// confirm termination says so rather than reporting success.
    targets: Vec<String>,
}

/// `POST /api/jobs/{id}/cancel`. The job of an rf run answers in the rf
/// contract's coded shape (AC-1b2a, [`super::sessions::rf_job_cancel`]);
/// every other job keeps the Console behaviour and body below.
pub async fn post_job_cancel(State(state): State<AppState>, Path(id): Path<String>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if let Some(response) = super::sessions::rf_job_cancel(&state, &id, &parts).await {
        return response;
    }
    console_job_cancel(&state, &parts.headers, &id).await.into_response()
}

/// A caller authenticated by a paired bearer cancels without Main's level
/// being consulted; with pairing off the pre-A4 gate applies.
async fn console_job_cancel(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
) -> Result<Json<JobCancelResponse>, (StatusCode, Json<serde_json::Value>)> {
    if !super::owner_authenticated(state, headers) {
        // Ending live work is a high-risk side effect, same gate as the registry
        // kill endpoint this delegates to.
        authorize_resource_mutation(state, "gateway_api:jobs:cancel", ResourceRiskLevel::High)?;
    }

    let job_id = parse_job_id(id)?;
    let Some(work_id) = jobs::work_id_of(job_id) else {
        return Err(job_not_found(id));
    };

    // Cascade: a job's tool calls and child processes have no reason to outlive
    // the job that started them.
    let results = crate::runtime::registry::kill(work_id, true).await;
    Ok(Json(JobCancelResponse {
        job_id: job_id.to_string(),
        work_id: work_id.to_string(),
        targets: results
            .into_iter()
            .map(|result| format!("{}:{}", result.id, result.outcome.as_str()))
            .collect(),
    }))
}
