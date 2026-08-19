//! Runtime observability endpoints: what is running, and how to end it.
//!
//! PRX removes timeouts on purpose, so nothing in the process expires on its
//! own. These endpoints are the operator's replacement for that: a live listing
//! of every registered work item with its lineage and elapsed time, a by-id kill
//! that terminates process groups rather than lone pids, and the connection-pool
//! saturation counters that until now had a `snapshot()` but no way out of the
//! process.

use super::{AppState, authorize_resource_mutation};
use crate::runtime::registry;
use crate::security::policy::ResourceRiskLevel;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub(super) struct WorkItemResponse {
    id: String,
    kind: &'static str,
    name: String,
    state: &'static str,
    parent: Option<String>,
    run_id: Option<String>,
    started_at_unix_ms: u64,
    elapsed_secs: u64,
    elapsed_ms: u128,
    pid: Option<u32>,
    pgid: Option<i32>,
}

impl From<registry::WorkSnapshot> for WorkItemResponse {
    fn from(snapshot: registry::WorkSnapshot) -> Self {
        Self {
            id: snapshot.id.to_string(),
            kind: snapshot.kind.as_str(),
            name: snapshot.name.to_string(),
            state: snapshot.state.as_str(),
            parent: snapshot.parent.map(|parent| parent.to_string()),
            run_id: snapshot.run_id.map(|run_id| run_id.to_string()),
            started_at_unix_ms: snapshot.started_at_unix_ms,
            elapsed_secs: snapshot.elapsed.as_secs(),
            elapsed_ms: snapshot.elapsed.as_millis(),
            pid: snapshot.pid,
            pgid: snapshot.pgid,
        }
    }
}

#[derive(Serialize)]
pub(super) struct TasksResponse {
    /// Work items whose owner is still alive.
    running: Vec<WorkItemResponse>,
    /// Children spawned but never reaped: still alive after their owner ended
    /// (`orphaned`), exited but unreaped (`zombie`), or of unknown liveness
    /// (`abandoned`). This is the orphan-hunting view.
    unreaped: Vec<WorkItemResponse>,
    total: usize,
}

pub async fn get_tasks() -> Json<TasksResponse> {
    let (unreaped, running): (Vec<_>, Vec<_>) = registry::snapshot_all().into_iter().partition(|snapshot| {
        matches!(
            snapshot.state,
            registry::WorkState::Abandoned | registry::WorkState::Orphaned | registry::WorkState::Zombie
        )
    });
    let total = running.len().saturating_add(unreaped.len());
    Json(TasksResponse {
        running: running.into_iter().map(WorkItemResponse::from).collect(),
        unreaped: unreaped.into_iter().map(WorkItemResponse::from).collect(),
        total,
    })
}

#[derive(Deserialize)]
pub(super) struct KillQuery {
    /// Take the item's descendants with it. Defaults to true; see
    /// [`registry::kill`] for why.
    cascade: Option<bool>,
}

#[derive(Serialize)]
pub(super) struct KillTargetResponse {
    id: String,
    kind: &'static str,
    name: String,
    outcome: &'static str,
}

#[derive(Serialize)]
pub(super) struct KillResponse {
    requested: String,
    cascade: bool,
    targets: Vec<KillTargetResponse>,
}

pub async fn post_task_kill(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<KillQuery>,
) -> Result<Json<KillResponse>, (StatusCode, Json<serde_json::Value>)> {
    // Ending live work is a high-risk side effect, so it goes through the same
    // gate as every other mutating gateway operation.
    authorize_resource_mutation(&state, "runtime_task_kill", ResourceRiskLevel::High)?;

    let Some(work_id) = registry::WorkId::parse(&id) else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("invalid work id '{id}'")})),
        ));
    };
    let cascade = query.cascade.unwrap_or(true);
    let results = registry::kill(work_id, cascade).await;
    if results
        .iter()
        .all(|result| result.outcome == registry::KillOutcome::Unknown)
    {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("no running work item with id '{id}'")})),
        ));
    }

    Ok(Json(KillResponse {
        requested: work_id.to_string(),
        cascade,
        targets: results
            .into_iter()
            .map(|result| KillTargetResponse {
                id: result.id.to_string(),
                kind: result.kind.as_str(),
                name: result.name.to_string(),
                outcome: result.outcome.as_str(),
            })
            .collect(),
    }))
}

#[derive(Serialize)]
pub(super) struct PoolResponse {
    kind: &'static str,
    name: String,
    metrics: serde_json::Value,
}

#[derive(Serialize)]
pub(super) struct PoolsResponse {
    pools: Vec<PoolResponse>,
}

pub async fn get_pools() -> Json<PoolsResponse> {
    Json(PoolsResponse {
        pools: registry::pool_snapshots()
            .into_iter()
            .map(|snapshot| PoolResponse {
                kind: snapshot.kind,
                name: snapshot.name,
                metrics: snapshot.metrics,
            })
            .collect(),
    })
}
