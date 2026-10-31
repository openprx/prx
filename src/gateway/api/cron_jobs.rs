//! Owner-scoped cron HTTP adapter.
//!
//! Calls the existing cron store and the scheduler's preauthorized manual-run
//! path. It does not start a second scheduler. An API token is never the
//! local-process operator: the owner comes only from
//! [`crate::cron::lineage_from_trusted_scope`] over a server-built scope whose
//! `owner_id` is [`ApiPrincipal::owner_id`].

use super::{ApiPrincipal, AppState};
use crate::cron::{self, CronJob, CronJobEvent, CronJobLineage, CronJobPatch, CronRun, Schedule};
use crate::security::policy::SecurityPolicy;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

const RUN_OUTPUT_CHARS: usize = 500;
const DEFAULT_RUN_LIMIT: usize = 50;
const MAX_RUN_LIMIT: usize = 200;

type ApiResponse = (StatusCode, Json<Value>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobWrite {
    #[serde(default)]
    expression: Option<String>,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    every_ms: Option<u64>,
    #[serde(default)]
    delay: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    tz: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RunsQuery {
    #[serde(default)]
    limit: Option<u32>,
}

enum WriteSchedule {
    Recurring(Schedule),
    Once(String),
}

fn error_response(status: StatusCode, error: impl Into<String>) -> ApiResponse {
    (status, Json(json!({ "error": error.into() })))
}

fn bad_request(error: impl Into<String>) -> ApiResponse {
    error_response(StatusCode::BAD_REQUEST, error)
}

fn not_found(id: &str) -> ApiResponse {
    error_response(StatusCode::NOT_FOUND, format!("Job '{id}' not found"))
}

fn unavailable(error: impl Into<String>) -> ApiResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "success": false, "error": error.into() })),
    )
}

fn scope_args(owner_id: &str) -> Value {
    json!({
        "_zc_scope_trusted": true,
        "_zc_scope": { "owner_id": owner_id }
    })
}

fn require_owner(config: &crate::config::Config, principal: &ApiPrincipal) -> Result<CronJobLineage, ApiResponse> {
    let lineage = cron::lineage_from_trusted_scope(config, &scope_args(&principal.owner_id));
    match lineage
        .owner_id
        .as_deref()
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
    {
        Some(_) => Ok(lineage),
        None => Err(error_response(
            StatusCode::FORBIDDEN,
            "cron owner cannot be derived for this API token",
        )),
    }
}

fn policy(config: &crate::config::Config) -> std::sync::Arc<SecurityPolicy> {
    crate::runtime::bootstrap::build_security_policy(config)
}

fn require_mutation(config: &crate::config::Config, action: &str) -> Result<(), ApiResponse> {
    if policy(config).can_act() {
        Ok(())
    } else {
        Err(error_response(
            StatusCode::FORBIDDEN,
            format!("Security policy: read-only mode, cannot perform '{action}'"),
        ))
    }
}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &Bytes) -> Result<T, ApiResponse> {
    if bytes.is_empty() {
        return Err(bad_request("request body is required"));
    }
    serde_json::from_slice(bytes).map_err(|error| bad_request(format!("malformed request: {error}")))
}

fn ensure_empty_body(bytes: &Bytes) -> Result<(), ApiResponse> {
    if bytes.is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| bad_request("malformed request"))?;
    if value.as_object().is_some_and(|object| object.is_empty()) {
        Ok(())
    } else {
        Err(bad_request("request body must be empty"))
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
}

fn write_schedule(body: &JobWrite, required: bool) -> Result<Option<WriteSchedule>, ApiResponse> {
    let chosen = [
        body.expression.is_some(),
        body.at.is_some(),
        body.every_ms.is_some(),
        body.delay.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if chosen == 0 {
        if required {
            return Err(bad_request(
                "exactly one of expression, at, every_ms, or delay is required",
            ));
        }
        if body.tz.is_some() {
            return Err(bad_request("tz is only valid with expression"));
        }
        return Ok(None);
    }
    if chosen != 1 {
        return Err(bad_request(
            "exactly one of expression, at, every_ms, or delay is required",
        ));
    }
    if body.tz.is_some() && body.expression.is_none() {
        return Err(bad_request("tz is only valid with expression"));
    }
    if let Some(expression) = &body.expression {
        let expression = expression.trim();
        if expression.is_empty() {
            return Err(bad_request("expression is empty"));
        }
        return Ok(Some(WriteSchedule::Recurring(Schedule::Cron {
            expr: expression.to_string(),
            tz: non_empty(body.tz.as_deref()),
        })));
    }
    if let Some(at) = &body.at {
        let at = DateTime::parse_from_rfc3339(at.trim())
            .map(|value| value.with_timezone(&Utc))
            .map_err(|_| bad_request("at must be an RFC3339 timestamp"))?;
        return Ok(Some(WriteSchedule::Recurring(Schedule::At { at })));
    }
    if let Some(every_ms) = body.every_ms {
        return Ok(Some(WriteSchedule::Recurring(Schedule::Every { every_ms })));
    }
    let delay = body.delay.as_deref().unwrap_or("").trim();
    if delay.is_empty() {
        return Err(bad_request("delay is empty"));
    }
    Ok(Some(WriteSchedule::Once(delay.to_string())))
}

fn schedule_client_error(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("invalid")
        || text.contains("cron")
        || text.contains("schedule")
        || text.contains("delay")
        || text.contains("timezone")
        || text.contains("every_ms")
}

fn store_client_error(error: &anyhow::Error) -> ApiResponse {
    if schedule_client_error(error) {
        bad_request(error.to_string())
    } else {
        tracing::warn!(%error, "cron store write failed");
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "cron store write failed")
    }
}

fn missing_job(error: &anyhow::Error, id: &str) -> bool {
    error.to_string() == format!("Cron job '{id}' not found")
}

fn redact_job(job: &CronJob) -> Result<Value, ApiResponse> {
    let mut value = serde_json::to_value(job).map_err(|error| {
        tracing::warn!(%error, "cron job could not be encoded");
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "cron job could not be encoded")
    })?;
    if let Some(object) = value.as_object_mut() {
        object.remove("approval_grant_json");
    }
    Ok(value)
}

fn truncate_output(output: Option<String>) -> Option<String> {
    output.map(|text| {
        if text.chars().count() <= RUN_OUTPUT_CHARS {
            text
        } else {
            let mut shortened: String = text.chars().take(RUN_OUTPUT_CHARS).collect();
            shortened.push_str("...");
            shortened
        }
    })
}

fn run_view(run: &CronRun) -> Value {
    json!({
        "id": run.id,
        "job_id": run.job_id,
        "started_at": run.started_at,
        "finished_at": run.finished_at,
        "status": run.status,
        "output": truncate_output(run.output.clone()),
        "duration_ms": run.duration_ms,
        "attempt_id": run.attempt_id,
        "worker_id": run.worker_id,
    })
}

fn event_view(event: &CronJobEvent) -> Value {
    json!({
        "id": event.id,
        "event_id": event.event_id,
        "event_type": event.event_type,
        "status": event.status,
        "created_at": event.created_at,
    })
}

fn owns(job: &CronJob, owner: &str) -> bool {
    job.owner_id.as_deref() == Some(owner)
}

fn visible_job(config: &crate::config::Config, owner: &str, id: &str) -> Result<CronJob, ApiResponse> {
    match cron::get_job(config, id) {
        Ok(job) if owns(&job, owner) => Ok(job),
        Ok(_) => Err(not_found(id)),
        Err(error) if missing_job(&error, id) => Err(not_found(id)),
        Err(error) => {
            tracing::warn!(job_id = %id, %error, "cron job unreadable");
            Err(unavailable("cron jobs are unavailable"))
        }
    }
}

fn owner_of<'a>(lineage: &'a CronJobLineage) -> Result<&'a str, ApiResponse> {
    lineage
        .owner_id
        .as_deref()
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
        .ok_or_else(|| error_response(StatusCode::FORBIDDEN, "cron owner cannot be derived for this API token"))
}

pub(super) async fn list_jobs(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
) -> ApiResponse {
    let config = state.config.load_full();
    let lineage = match require_owner(&config, &principal) {
        Ok(lineage) => lineage,
        Err(response) => return response,
    };
    let owner = match owner_of(&lineage) {
        Ok(owner) => owner,
        Err(response) => return response,
    };
    match cron::list_jobs(&config) {
        Ok(jobs) => {
            let mut visible = Vec::new();
            for job in jobs.into_iter().filter(|job| owns(job, owner)) {
                match redact_job(&job) {
                    Ok(value) => visible.push(value),
                    Err(response) => return response,
                }
            }
            (StatusCode::OK, Json(json!({ "jobs": visible })))
        }
        Err(error) => {
            tracing::warn!(%error, "cron job list failed");
            unavailable("cron jobs are unavailable")
        }
    }
}

pub(super) async fn get_job(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
) -> ApiResponse {
    let config = state.config.load_full();
    let lineage = match require_owner(&config, &principal) {
        Ok(lineage) => lineage,
        Err(response) => return response,
    };
    let owner = match owner_of(&lineage) {
        Ok(owner) => owner,
        Err(response) => return response,
    };
    let job = match visible_job(&config, owner, id.trim()) {
        Ok(job) => job,
        Err(response) => return response,
    };
    let events = match cron::list_job_events(&config, &job.id) {
        Ok(events) => events.into_iter().map(|event| event_view(&event)).collect::<Vec<_>>(),
        Err(error) => {
            tracing::warn!(job_id = %job.id, %error, "cron job events failed");
            return unavailable("cron job events are unavailable");
        }
    };
    let mut body = match redact_job(&job) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Some(object) = body.as_object_mut() {
        object.insert("events".to_string(), Value::Array(events));
    }
    (StatusCode::OK, Json(body))
}

pub(super) async fn list_runs(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
    Query(query): Query<RunsQuery>,
) -> ApiResponse {
    let config = state.config.load_full();
    let lineage = match require_owner(&config, &principal) {
        Ok(lineage) => lineage,
        Err(response) => return response,
    };
    let owner = match owner_of(&lineage) {
        Ok(owner) => owner,
        Err(response) => return response,
    };
    let id = id.trim();
    if let Err(response) = visible_job(&config, owner, id) {
        return response;
    }
    let limit = usize::try_from(
        query
            .limit
            .unwrap_or(DEFAULT_RUN_LIMIT as u32)
            .clamp(1, MAX_RUN_LIMIT as u32),
    )
    .unwrap_or(DEFAULT_RUN_LIMIT);
    match cron::list_runs(&config, id, limit) {
        Ok(runs) => (
            StatusCode::OK,
            Json(json!({ "runs": runs.iter().map(run_view).collect::<Vec<_>>() })),
        ),
        Err(error) => {
            tracing::warn!(job_id = %id, %error, "cron runs failed");
            unavailable("cron runs are unavailable")
        }
    }
}

pub(super) async fn status(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
) -> ApiResponse {
    let config = state.config.load_full();
    let lineage = match require_owner(&config, &principal) {
        Ok(lineage) => lineage,
        Err(response) => return response,
    };
    let owner = match owner_of(&lineage) {
        Ok(owner) => owner,
        Err(response) => return response,
    };
    let jobs = match cron::list_jobs(&config) {
        Ok(jobs) => jobs.into_iter().filter(|job| owns(job, owner)).collect::<Vec<_>>(),
        Err(error) => {
            tracing::warn!(%error, "cron status failed");
            return unavailable("cron jobs are unavailable");
        }
    };
    let terminal = jobs.iter().filter(|job| job.terminal_state.is_some()).count();
    let active = jobs
        .iter()
        .filter(|job| job.enabled && job.terminal_state.is_none())
        .count();
    let paused = jobs.len().saturating_sub(active).saturating_sub(terminal);
    let claimed = jobs.iter().filter(|job| job.claim.is_some()).count();
    (
        StatusCode::OK,
        Json(json!({
            "enabled": true,
            "disabled_state": "not_configured",
            "jobs": jobs.len(),
            "active": active,
            "paused": paused,
            "terminal": terminal,
            "claimed": claimed,
        })),
    )
}

pub(super) async fn create_job(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    body: Bytes,
) -> ApiResponse {
    let config = state.config.load_full();
    if let Err(response) = require_mutation(&config, "add") {
        return response;
    }
    let lineage = match require_owner(&config, &principal) {
        Ok(lineage) => lineage,
        Err(response) => return response,
    };
    let write: JobWrite = match parse_json(&body) {
        Ok(write) => write,
        Err(response) => return response,
    };
    let command = match non_empty(write.command.as_deref()) {
        Some(command) => command,
        None => return bad_request("command is required"),
    };
    let schedule = match write_schedule(&write, true) {
        Ok(Some(schedule)) => schedule,
        Ok(None) => return bad_request("exactly one of expression, at, every_ms, or delay is required"),
        Err(response) => return response,
    };
    let name = non_empty(write.name.as_deref());
    let created = match schedule {
        WriteSchedule::Once(delay) => {
            cron::add_once_with_lineage_and_approval_grant(&config, &delay, &command, None, lineage)
        }
        WriteSchedule::Recurring(schedule) => {
            if let Err(error) = cron::validate_schedule(&schedule, Utc::now()) {
                return if schedule_client_error(&error) {
                    bad_request(error.to_string())
                } else {
                    store_client_error(&error)
                };
            }
            cron::add_shell_job_with_lineage_and_approval_grant(&config, name, schedule, &command, None, lineage)
        }
    };
    match created {
        Ok(job) => match redact_job(&job) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(response) => response,
        },
        Err(error) => store_client_error(&error),
    }
}

pub(super) async fn patch_job(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResponse {
    let config = state.config.load_full();
    if let Err(response) = require_mutation(&config, "update") {
        return response;
    }
    let lineage = match require_owner(&config, &principal) {
        Ok(lineage) => lineage,
        Err(response) => return response,
    };
    let owner = match owner_of(&lineage) {
        Ok(owner) => owner,
        Err(response) => return response,
    };
    let id = id.trim();
    if let Err(response) = visible_job(&config, owner, id) {
        return response;
    }
    let write: JobWrite = match parse_json(&body) {
        Ok(write) => write,
        Err(response) => return response,
    };
    let schedule = match write_schedule(&write, false) {
        Ok(schedule) => schedule,
        Err(response) => return response,
    };
    let command = match write.command.as_deref() {
        Some(command) => match non_empty(Some(command)) {
            Some(command) => Some(command),
            None => return bad_request("command is empty"),
        },
        None => None,
    };
    let name = write.name.as_deref().map(|name| name.trim().to_string());
    if schedule.is_none() && command.is_none() && name.is_none() {
        return bad_request("update requires a schedule, command, or name");
    }
    let patch_schedule = match schedule {
        Some(WriteSchedule::Once(_)) => {
            return bad_request("delay creates a job; an update uses expression, at, or every_ms");
        }
        Some(WriteSchedule::Recurring(schedule)) => {
            if let Err(error) = cron::validate_schedule(&schedule, Utc::now()) {
                return if schedule_client_error(&error) {
                    bad_request(error.to_string())
                } else {
                    store_client_error(&error)
                };
            }
            Some(schedule)
        }
        None => None,
    };
    let patch = CronJobPatch {
        schedule: patch_schedule,
        command: command.clone(),
        name,
        created_via: command.is_some().then_some(crate::cron::JobCreator::AgentTool),
        ..CronJobPatch::default()
    };
    match cron::update_job(&config, id, patch) {
        Ok(job) => match redact_job(&job) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(response) => response,
        },
        Err(error) if missing_job(&error, id) => not_found(id),
        Err(error) => store_client_error(&error),
    }
}

fn set_enabled(state: &AppState, principal: &ApiPrincipal, id: &str, enabled: bool, body: &Bytes) -> ApiResponse {
    let config = state.config.load_full();
    let action = if enabled { "resume" } else { "pause" };
    if let Err(response) = require_mutation(&config, action) {
        return response;
    }
    if let Err(response) = ensure_empty_body(body) {
        return response;
    }
    let lineage = match require_owner(&config, principal) {
        Ok(lineage) => lineage,
        Err(response) => return response,
    };
    let owner = match owner_of(&lineage) {
        Ok(owner) => owner,
        Err(response) => return response,
    };
    let id = id.trim();
    if let Err(response) = visible_job(&config, owner, id) {
        return response;
    }
    let updated = if enabled {
        cron::resume_job(&config, id)
    } else {
        cron::pause_job(&config, id)
    };
    match updated {
        Ok(job) => match redact_job(&job) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(response) => response,
        },
        Err(error) if missing_job(&error, id) => not_found(id),
        Err(error) => store_client_error(&error),
    }
}

pub(super) async fn pause_job(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResponse {
    set_enabled(&state, &principal, &id, false, &body)
}

pub(super) async fn resume_job(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResponse {
    set_enabled(&state, &principal, &id, true, &body)
}

pub(super) async fn delete_job(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResponse {
    let config = state.config.load_full();
    if let Err(response) = require_mutation(&config, "remove") {
        return response;
    }
    if let Err(response) = ensure_empty_body(&body) {
        return response;
    }
    let lineage = match require_owner(&config, &principal) {
        Ok(lineage) => lineage,
        Err(response) => return response,
    };
    let owner = match owner_of(&lineage) {
        Ok(owner) => owner,
        Err(response) => return response,
    };
    let id = id.trim();
    if let Err(response) = visible_job(&config, owner, id) {
        return response;
    }
    match cron::remove_job(&config, id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "id": id, "removed": true }))),
        Err(error) if missing_job(&error, id) => not_found(id),
        Err(error) => store_client_error(&error),
    }
}

pub(super) async fn run_job(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResponse {
    let config = state.config.load_full();
    if let Err(response) = require_mutation(&config, "run") {
        return response;
    }
    if let Err(response) = ensure_empty_body(&body) {
        return response;
    }
    let lineage = match require_owner(&config, &principal) {
        Ok(lineage) => lineage,
        Err(response) => return response,
    };
    let owner = match owner_of(&lineage) {
        Ok(owner) => owner,
        Err(response) => return response,
    };
    let id = id.trim();
    let job = match visible_job(&config, owner, id) {
        Ok(job) => job,
        Err(response) => return response,
    };
    let Ok(lease_secs) = i64::try_from(config.scheduler.claim_lease_secs) else {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "scheduler lease does not fit");
    };
    let worker_id = format!("cron-manual-{}", uuid::Uuid::new_v4());
    let lease = chrono::Duration::seconds(lease_secs);
    let now = Utc::now();
    let claimed = if job.terminal_state.is_some() {
        cron::claim_terminal_job_for_manual_rerun(&config, &job, &worker_id, now, lease)
    } else {
        cron::claim_job_if_current_for_manual_run(&config, &job, &worker_id, now, lease)
    };
    let claim = match claimed {
        Ok(Some(claim)) => claim,
        Ok(None) => {
            return error_response(
                StatusCode::CONFLICT,
                format!("cron job '{}' changed or is already running", job.id),
            );
        }
        Err(error) => {
            tracing::warn!(job_id = %job.id, %error, "cron manual claim failed");
            return error_response(StatusCode::CONFLICT, "cron job could not be claimed");
        }
    };
    let started = Utc::now();
    let (success, output) =
        cron::scheduler::execute_claimed_job_preauthorized_for_tool(&config, &job, claim, "cron").await;
    let duration_ms = (Utc::now() - started).num_milliseconds();
    let status = if success { "ok" } else { "error" };
    (
        StatusCode::OK,
        Json(json!({
            "job_id": job.id,
            "status": status,
            "duration_ms": duration_ms,
            "output": output,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{at, json, serve_config};
    use super::*;
    use crate::security::AutonomyLevel;

    fn sample_command() -> &'static str {
        "true"
    }

    #[test]
    fn empty_api_owner_fails_closed() {
        let config = crate::config::Config::default();
        let principal = ApiPrincipal {
            owner_id: "   ".to_string(),
        };
        let error = require_owner(&config, &principal).expect_err("test: empty owner");
        assert_eq!(error.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn cron_routes_cover_owner_isolation_and_validation() {
        let served = serve_config(|_| {}).await;
        let (status, _) = served
            .call_anonymous(reqwest::Method::GET, "/api/cron/jobs", None)
            .await;
        assert_eq!(status, 401);

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/cron/jobs",
                Some(serde_json::json!({
                    "every_ms": 60_000,
                    "command": sample_command(),
                    "name": "owned",
                    "owner_id": "api:other"
                })),
            )
            .await;
        assert_eq!(status, 400, "{body}");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/cron/jobs",
                Some(serde_json::json!({ "expression": "not a cron", "command": sample_command() })),
            )
            .await;
        assert_eq!(status, 400, "{body}");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/cron/jobs",
                Some(serde_json::json!({
                    "every_ms": 60_000,
                    "command": sample_command(),
                    "name": "owned"
                })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let created = json(&body);
        let id = at_str(&created, "/id");
        assert_ne!(id, "");
        assert!(created.get("approval_grant_json").is_none());
        let owner = ApiPrincipal::of_token(super::super::test_support::TOKEN).owner_id;
        assert_eq!(at(&created, "/owner_id").as_str(), Some(owner.as_str()));

        let config = served.state.config.load_full();
        let foreign = cron::add_shell_job_with_lineage_and_approval_grant(
            &config,
            Some("foreign".to_string()),
            Schedule::Every { every_ms: 60_000 },
            sample_command(),
            None,
            CronJobLineage {
                owner_id: Some("api:foreign".to_string()),
                ..CronJobLineage::default()
            },
        )
        .expect("test: foreign job");

        let (missing_status, missing_body) = served
            .call(reqwest::Method::GET, "/api/cron/jobs/missing-job", None)
            .await;
        let (foreign_status, foreign_body) = served
            .call(reqwest::Method::GET, &format!("/api/cron/jobs/{}", foreign.id), None)
            .await;
        assert_eq!(missing_status, 404);
        assert_eq!(foreign_status, 404);
        let missing_error = at(&json(&missing_body), "/error").as_str().unwrap_or("").to_string();
        let foreign_error = at(&json(&foreign_body), "/error").as_str().unwrap_or("").to_string();
        assert_eq!(missing_error, "Job 'missing-job' not found");
        assert_eq!(foreign_error, format!("Job '{}' not found", foreign.id));
        assert!(!missing_error.contains("another principal"));
        assert!(!foreign_error.contains("another principal"));

        let (status, body) = served.call(reqwest::Method::GET, "/api/cron/jobs", None).await;
        assert_eq!(status, 200, "{body}");
        let parsed = json(&body);
        let jobs = at(&parsed, "/jobs").as_array().cloned().unwrap_or_default();
        assert!(jobs.iter().any(|job| at(job, "/id") == at(&created, "/id")));
        assert!(
            jobs.iter()
                .all(|job| at(job, "/id").as_str() != Some(foreign.id.as_str()))
        );

        let (status, body) = served.call(reqwest::Method::GET, "/api/cron/status", None).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(at(&json(&body), "/disabled_state"), "not_configured");
        assert_eq!(at(&json(&body), "/enabled"), true);

        let (status, _) = served
            .call(
                reqwest::Method::POST,
                &format!("/api/cron/jobs/{id}/pause"),
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(status, 200);
        let (status, body) = served
            .call(reqwest::Method::GET, &format!("/api/cron/jobs/{id}"), None)
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(at(&json(&body), "/enabled"), false);
        assert!(json(&body).get("events").and_then(Value::as_array).is_some());
        assert!(json(&body).get("approval_grant_json").is_none());

        let (status, _) = served
            .call(reqwest::Method::POST, &format!("/api/cron/jobs/{id}/resume"), None)
            .await;
        assert_eq!(status, 200);

        let (status, body) = served
            .call(reqwest::Method::POST, &format!("/api/cron/jobs/{id}/run"), None)
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(at(&json(&body), "/job_id"), at(&created, "/id"));
        assert!(json(&body).get("run_id").is_none());

        let (status, body) = served
            .call(reqwest::Method::GET, &format!("/api/cron/jobs/{id}/runs"), None)
            .await;
        assert_eq!(status, 200, "{body}");

        let (status, _) = served
            .call(reqwest::Method::DELETE, &format!("/api/cron/jobs/{id}"), None)
            .await;
        assert_eq!(status, 200);
        let (status, _) = served
            .call(reqwest::Method::GET, &format!("/api/cron/jobs/{id}"), None)
            .await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn read_only_cron_mutation_is_forbidden_and_a_blocked_store_is_unavailable() {
        let served = serve_config(|config| {
            config.autonomy.level = AutonomyLevel::ReadOnly;
        })
        .await;
        let (status, _) = served
            .call(
                reqwest::Method::POST,
                "/api/cron/jobs",
                Some(serde_json::json!({ "every_ms": 60_000, "command": "true" })),
            )
            .await;
        assert_eq!(status, 403);
        let (status, _) = served.call(reqwest::Method::GET, "/api/cron/jobs", None).await;
        assert_eq!(status, 200);

        let blocked = serve_config(|_| {}).await;
        let workspace = blocked.state.config.load_full().workspace_dir.clone();
        std::fs::remove_dir_all(&workspace).expect("test: remove workspace");
        std::fs::write(&workspace, b"blocked").expect("test: block workspace");
        let (status, body) = blocked.call(reqwest::Method::GET, "/api/cron/jobs", None).await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(at(&json(&body), "/success"), false);
    }

    fn at_str(value: &Value, pointer: &str) -> String {
        value.pointer(pointer).and_then(Value::as_str).unwrap_or("").to_string()
    }
}
