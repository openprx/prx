//! Owner-bearer Xin inventory and control. Creation stays on the agent tool.
use super::{ApiPrincipal, AppState, capabilities::coded_error};
use crate::{
    config::Config,
    cron::agent_jobs::{self, Kind},
    xin::{
        store,
        types::{XinGoal, XinTask},
    },
};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter {
    agent: Option<String>,
}

fn unknown(kind: Kind) -> Response {
    coded_error(
        StatusCode::NOT_FOUND,
        if matches!(kind, Kind::XinGoal) {
            "xin_goal_unknown"
        } else {
            "xin_task_unknown"
        },
        "no such Xin job",
    )
}
fn failure(error: &anyhow::Error, kind: Kind) -> Response {
    if error.downcast_ref::<store::JobLookupError>().is_some()
        || error
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|error| matches!(error, rusqlite::Error::QueryReturnedNoRows))
    {
        return unknown(kind);
    }
    coded_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "storage_unavailable",
        "Xin jobs are unavailable",
    )
}
fn task_view(config: &Config, task: &XinTask) -> anyhow::Result<Value> {
    let (creator, version) = agent_jobs::metadata(config, &task.id, Kind::XinTask)?;
    Ok(
        json!({"id":task.id,"kind":"xin_task","creator_agent":agent_jobs::creator_name(&creator),"schedule":if task.recurring {Some(format!("every {}s",task.interval_secs))} else {None},"next_run_at":task.next_run_at.to_rfc3339_opts(chrono::SecondsFormat::Secs,true),"enabled":task.enabled,"version":version,"context":task.context.as_str(),"deliver":task.deliver.as_str()}),
    )
}
fn goal_view(config: &Config, goal: &XinGoal) -> anyhow::Result<Value> {
    let (creator, version) = agent_jobs::metadata(config, &goal.id, Kind::XinGoal)?;
    Ok(
        json!({"id":goal.id,"kind":"xin_goal","creator_agent":agent_jobs::creator_name(&creator),"schedule":null,"next_run_at":null,"enabled":goal.enabled,"version":version}),
    )
}
fn visible(config: &Config, owner: &str, kind: Kind, id: &str) -> anyhow::Result<Option<Value>> {
    match kind {
        Kind::XinTask => {
            let task = store::get_task(config, id)?;
            if task.owner_id.as_deref() != Some(owner) {
                return Ok(None);
            }
            Ok(Some(task_view(config, &task)?))
        }
        Kind::XinGoal => {
            let goal = store::get_goal(config, id)?;
            if goal.owner_id.as_deref() != Some(owner) {
                return Ok(None);
            }
            Ok(Some(goal_view(config, &goal)?))
        }
        Kind::Cron => anyhow::bail!("invalid Xin kind"),
    }
}
async fn list(state: AppState, principal: ApiPrincipal, filter: Filter, kind: Kind) -> Response {
    if filter
        .agent
        .as_deref()
        .is_some_and(|name| crate::config::agent_name::AgentName::parse(name).is_err())
    {
        return coded_error(StatusCode::BAD_REQUEST, "field_invalid", "agent must be a valid name");
    }
    let config = state.config.load_full();
    let result = crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Value> {
        let mut jobs = Vec::new();
        match kind {
            Kind::XinTask => {
                for task in store::list_tasks(&config)? {
                    if task.owner_id.as_deref() == Some(principal.owner_id.as_str()) {
                        jobs.push(task_view(&config, &task)?);
                    }
                }
            }
            Kind::XinGoal => {
                for goal in store::list_goals(&config)? {
                    if goal.owner_id.as_deref() == Some(principal.owner_id.as_str()) {
                        jobs.push(goal_view(&config, &goal)?);
                    }
                }
            }
            Kind::Cron => anyhow::bail!("invalid Xin kind"),
        }
        jobs.retain(|job| {
            filter
                .agent
                .as_deref()
                .is_none_or(|name| job.get("creator_agent").and_then(Value::as_str) == Some(name))
        });
        Ok(if matches!(kind, Kind::XinGoal) {
            json!({"goals":jobs})
        } else {
            json!({"tasks":jobs})
        })
    })
    .await;
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => failure(&error, kind),
        Err(_) => coded_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "Xin jobs are unavailable",
        ),
    }
}
async fn item(state: AppState, principal: ApiPrincipal, id: String, kind: Kind) -> Response {
    let config = state.config.load_full();
    match crate::runtime::blocking::spawn_blocking(move || visible(&config, &principal.owner_id, kind, &id)).await {
        Ok(Ok(Some(job))) => Json(job).into_response(),
        Ok(Ok(None)) => unknown(kind),
        Ok(Err(error)) => failure(&error, kind),
        Err(_) => coded_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "Xin jobs are unavailable",
        ),
    }
}
#[derive(Clone, Copy)]
enum Control {
    Pause,
    Resume,
    Remove,
}
async fn control(
    state: AppState,
    principal: ApiPrincipal,
    id: String,
    kind: Kind,
    operation: Control,
    body: Bytes,
) -> Response {
    if !body.is_empty()
        && !serde_json::from_slice::<Value>(&body).is_ok_and(|v| v.as_object().is_some_and(|o| o.is_empty()))
    {
        return coded_error(StatusCode::BAD_REQUEST, "field_invalid", "request body must be empty");
    }
    if let Err(response) = super::authorize_resource_mutation(
        &state,
        match operation {
            Control::Pause => "gateway_api:xin:pause",
            Control::Resume => "gateway_api:xin:resume",
            Control::Remove => "gateway_api:xin:remove",
        },
        crate::security::policy::ResourceRiskLevel::Medium,
    ) {
        return response.into_response();
    }
    let config = state.config.load_full();
    match crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        if visible(&config, &principal.owner_id, kind, &id)?.is_none() {
            return Ok(None);
        }
        match (kind, operation) {
            (Kind::XinTask, Control::Pause) => {
                store::pause_task(&config, &id)?;
            }
            (Kind::XinTask, Control::Resume) => {
                store::resume_task(&config, &id)?;
            }
            (Kind::XinTask, Control::Remove) => store::remove_task(&config, &id)?,
            (Kind::XinGoal, Control::Pause) => {
                store::pause_goal(&config, &id)?;
            }
            (Kind::XinGoal, Control::Resume) => {
                store::resume_goal(&config, &id)?;
            }
            (Kind::XinGoal, Control::Remove) => store::remove_goal(&config, &id)?,
            (Kind::Cron, _) => anyhow::bail!("invalid Xin kind"),
        }
        if matches!(operation, Control::Remove) {
            Ok(Some(json!({"id":id,"removed":true})))
        } else {
            visible(&config, &principal.owner_id, kind, &id)
        }
    })
    .await
    {
        Ok(Ok(Some(value))) => Json(value).into_response(),
        Ok(Ok(None)) => unknown(kind),
        Ok(Err(error)) => failure(&error, kind),
        Err(_) => coded_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "Xin jobs are unavailable",
        ),
    }
}
pub(super) fn router(state: AppState) -> Router<AppState> {
    let mut routes = Router::new();
    for (kind, path) in [(Kind::XinTask, "/xin/tasks"), (Kind::XinGoal, "/xin/goals")] {
        routes = routes
            .route(
                path,
                get(
                    move |State(state): State<AppState>,
                          Extension(principal): Extension<ApiPrincipal>,
                          Query(filter): Query<Filter>| async move {
                        list(state, principal, filter, kind).await
                    },
                ),
            )
            .route(
                &format!("{path}/{{id}}"),
                get(
                    move |State(state): State<AppState>,
                          Extension(principal): Extension<ApiPrincipal>,
                          Path(id): Path<String>| async move { item(state, principal, id, kind).await },
                )
                .delete(
                    move |State(state): State<AppState>,
                          Extension(principal): Extension<ApiPrincipal>,
                          Path(id): Path<String>,
                          body: Bytes| async move {
                        control(state, principal, id, kind, Control::Remove, body).await
                    },
                ),
            );
        for (operation, suffix) in [(Control::Pause, "pause"), (Control::Resume, "resume")] {
            routes = routes.route(
                &format!("{path}/{{id}}/{suffix}"),
                post(
                    move |State(state): State<AppState>,
                          Extension(principal): Extension<ApiPrincipal>,
                          Path(id): Path<String>,
                          body: Bytes| async move {
                        control(state, principal, id, kind, operation, body).await
                    },
                ),
            );
        }
    }
    routes.route_layer(axum::middleware::from_fn_with_state(state, super::capabilities::guard))
}
