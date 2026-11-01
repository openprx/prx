//! Skill lifecycle HTTP adapter.
//!
//! Calls [`crate::tools::skills::SkillsManageTool`] through `Tool::execute`
//! after installing a trusted scope whose `owner_id` is only the paired
//! [`ApiPrincipal`]. The tool's owner check and approval gate stay in place.
//! The browser cannot send `owner_id` or a grant.

use super::{ApiPrincipal, AppState};
use crate::tools::SkillsManageTool;
use crate::tools::traits::Tool;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Map, Value, json};

type ApiResponse = (StatusCode, Json<Value>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProposalQuery {
    #[serde(default)]
    skill_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelBody {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackBody {
    skill_id: String,
    revision_id: String,
    expected_generation: u64,
}

fn error_response(status: StatusCode, error: impl Into<String>) -> ApiResponse {
    (status, Json(json!({ "error": error.into() })))
}

fn bad_request(error: impl Into<String>) -> ApiResponse {
    error_response(StatusCode::BAD_REQUEST, error)
}

fn unavailable() -> ApiResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "success": false, "error": "skill lifecycle is unavailable" })),
    )
}

/// The lifecycle store is offered only when `[skills_lifecycle]` is enabled and
/// opens. A disabled section, or one that cannot start, is unavailable. The
/// tool schema then omits every lifecycle action, and schema rejection is not
/// a malformed request.
fn lifecycle_ready(config: &crate::config::Config) -> bool {
    match crate::skills::lifecycle_runtime::open_service(&config.workspace_dir, &config.skills_lifecycle) {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(%error, "skill lifecycle is unavailable");
            false
        }
    }
}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &Bytes) -> Result<T, ApiResponse> {
    if bytes.is_empty() {
        return Err(bad_request("request body is required"));
    }
    serde_json::from_slice(bytes).map_err(|error| bad_request(format!("malformed request: {error}")))
}

fn scoped_args(owner_id: &str, mut fields: Map<String, Value>) -> Value {
    fields.retain(|key, _| !crate::tools::execution::is_runtime_only_arg(key) && key != "owner_id" && key != "owner");
    fields.insert("_zc_scope_trusted".to_string(), json!(true));
    fields.insert(
        "_zc_scope".to_string(),
        json!({ "owner_id": owner_id, "owner_isolated": true }),
    );
    Value::Object(fields)
}

fn lifecycle_code(output: &str) -> Option<(String, Option<String>)> {
    let value: Value = serde_json::from_str(output).ok()?;
    let code = value.get("lifecycle_code")?.as_str()?.to_string();
    let transaction_id = value
        .get("transaction_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    Some((code, transaction_id))
}

fn classify_failure(output: &str, error: &str) -> ApiResponse {
    if let Some((code, transaction_id)) = lifecycle_code(output)
        && let Some(response) = classify_code(&code, transaction_id.as_deref())
    {
        return response;
    }
    classify_text(error)
}

fn classify_code(code: &str, transaction_id: Option<&str>) -> Option<ApiResponse> {
    Some(match code {
        "committed_repair_required" => {
            let mut body = serde_json::Map::new();
            body.insert("error".to_string(), json!("committed with repair required"));
            body.insert("committed_repair_required".to_string(), json!(true));
            if let Some(id) = transaction_id {
                body.insert("transaction_id".to_string(), json!(id));
            }
            (StatusCode::CONFLICT, Json(Value::Object(body)))
        }
        "owner_isolated_write" => error_response(
            StatusCode::FORBIDDEN,
            "publish and rollback need the operator's full session",
        ),
        "not_found" => error_response(StatusCode::NOT_FOUND, "not found"),
        "wrong_owner" => error_response(
            StatusCode::FORBIDDEN,
            "the skill lifecycle store belongs to another owner",
        ),
        "unknown_owner" => error_response(
            StatusCode::FORBIDDEN,
            "the skill lifecycle needs a caller with a known owner scope",
        ),
        "invalid" => bad_request("lifecycle request is invalid"),
        "unavailable" => unavailable(),
        "busy" => error_response(StatusCode::CONFLICT, "skill lifecycle is busy"),
        "stale_generation" | "conflict" => error_response(StatusCode::CONFLICT, "lifecycle generation conflict"),
        "approval_required" => error_response(StatusCode::FORBIDDEN, "explicit approval grant required"),
        "read_only" => error_response(
            StatusCode::FORBIDDEN,
            "read-only mode cannot change the skill lifecycle",
        ),
        "policy_denied" => error_response(StatusCode::FORBIDDEN, "the skill lifecycle change was refused"),
        _ => return None,
    })
}

fn classify_text(error: &str) -> ApiResponse {
    let lower = error.to_ascii_lowercase();
    if lower.contains("committed but needs repair") {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "committed with repair required",
                "committed_repair_required": true
            })),
        );
    }
    if lower.contains("another owner") {
        return error_response(
            StatusCode::FORBIDDEN,
            "the skill lifecycle store belongs to another owner",
        );
    }
    if lower.contains("known owner scope") || lower.contains("approval grant") || lower.contains("read-only") {
        let message = if lower.contains("approval grant") {
            "explicit approval grant required"
        } else if lower.contains("read-only") {
            "read-only mode cannot change the skill lifecycle"
        } else {
            "the skill lifecycle needs a caller with a known owner scope"
        };
        return error_response(StatusCode::FORBIDDEN, message);
    }
    if lower.contains("no such proposal")
        || lower.contains("revision not found")
        || lower.contains("rollback target")
        || lower.contains("was never published")
    {
        return error_response(StatusCode::NOT_FOUND, "not found");
    }
    if lower.contains("stale generation") || lower.contains("conflicts with") || lower.contains("conflict") {
        return error_response(StatusCode::CONFLICT, "lifecycle generation conflict");
    }
    if lower.contains("disabled") || lower.contains("unsupported") || lower.contains("unavailable") {
        return unavailable();
    }
    if lower.contains("missing") || lower.contains("invalid") {
        return bad_request("lifecycle request is invalid");
    }
    tracing::warn!(error = error, "skill lifecycle call failed");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "skill lifecycle call failed")
}

async fn execute(state: &AppState, principal: &ApiPrincipal, fields: Map<String, Value>) -> ApiResponse {
    let config = state.config.load_full();
    if !lifecycle_ready(config.as_ref()) {
        return unavailable();
    }
    let security = crate::runtime::bootstrap::build_security_policy(&config);
    let tool = SkillsManageTool::from_config(config.workspace_dir.clone(), config.as_ref().clone(), security);
    let args = scoped_args(&principal.owner_id, fields);
    match tool.execute(args).await {
        Ok(result) if result.success => match serde_json::from_str::<Value>(&result.output) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(error) => {
                tracing::warn!(%error, "skill lifecycle result was not json");
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "skill lifecycle result was not json")
            }
        },
        Ok(result) => classify_failure(
            &result.output,
            result.error.as_deref().unwrap_or("skill lifecycle call failed"),
        ),
        Err(error) => classify_failure("", &error.to_string()),
    }
}

pub(super) async fn status(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
) -> ApiResponse {
    let mut fields = Map::new();
    fields.insert("action".to_string(), json!("lifecycle_status"));
    execute(&state, &principal, fields).await
}

pub(super) async fn list_proposals(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Query(query): Query<ProposalQuery>,
) -> ApiResponse {
    let mut fields = Map::new();
    fields.insert("action".to_string(), json!("lifecycle_list"));
    if let Some(skill_id) = query
        .skill_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        fields.insert("skill_id".to_string(), json!(skill_id));
    }
    execute(&state, &principal, fields).await
}

pub(super) async fn get_proposal(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
) -> ApiResponse {
    let id = id.trim();
    if id.is_empty() {
        return bad_request("proposal id is required");
    }
    let mut fields = Map::new();
    fields.insert("action".to_string(), json!("lifecycle_inspect"));
    fields.insert("proposal_id".to_string(), json!(id));
    execute(&state, &principal, fields).await
}

pub(super) async fn publish(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResponse {
    if !body.is_empty() {
        let value: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => return bad_request("malformed request"),
        };
        if value.as_object().is_none_or(|object| !object.is_empty()) {
            return bad_request("publish body must be empty");
        }
    }
    let id = id.trim();
    if id.is_empty() {
        return bad_request("proposal id is required");
    }
    let mut fields = Map::new();
    fields.insert("action".to_string(), json!("lifecycle_publish"));
    fields.insert("proposal_id".to_string(), json!(id));
    execute(&state, &principal, fields).await
}

pub(super) async fn cancel(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResponse {
    let reason = if body.is_empty() {
        None
    } else {
        let parsed: CancelBody = match parse_json(&body) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        parsed.reason
    };
    let id = id.trim();
    if id.is_empty() {
        return bad_request("proposal id is required");
    }
    let mut fields = Map::new();
    fields.insert("action".to_string(), json!("lifecycle_cancel"));
    fields.insert("proposal_id".to_string(), json!(id));
    if let Some(reason) = reason
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        fields.insert("reason".to_string(), json!(reason));
    }
    execute(&state, &principal, fields).await
}

pub(super) async fn rollback(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    body: Bytes,
) -> ApiResponse {
    let parsed: RollbackBody = match parse_json(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    if parsed.skill_id.trim().is_empty() || parsed.revision_id.trim().is_empty() {
        return bad_request("skill_id and revision_id are required");
    }
    let mut fields = Map::new();
    fields.insert("action".to_string(), json!("lifecycle_rollback"));
    fields.insert("skill_id".to_string(), json!(parsed.skill_id.trim()));
    fields.insert("revision_id".to_string(), json!(parsed.revision_id.trim()));
    fields.insert("expected_generation".to_string(), json!(parsed.expected_generation));
    execute(&state, &principal, fields).await
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{TOKEN, at, json, serve_config};
    use super::*;

    fn enable(owner: String) -> impl FnOnce(&mut crate::config::Config) {
        move |config: &mut crate::config::Config| {
            config.skills_lifecycle.enabled = true;
            config.skills_lifecycle.owner_scope = Some(owner);
        }
    }

    #[test]
    fn lifecycle_errors_keep_repair_conflict_and_not_found_distinct() {
        let repair = classify_failure(
            r#"{"lifecycle_code":"committed_repair_required","transaction_id":"tx-1"}"#,
            "transaction tx-1 is committed but needs repair or recovery: disk",
        );
        assert_eq!(repair.0, StatusCode::CONFLICT);
        assert_eq!(at(&repair.1.0, "/committed_repair_required"), true);
        assert_eq!(at(&repair.1.0, "/transaction_id"), "tx-1");
        assert_eq!(at(&repair.1.0, "/error"), "committed with repair required");
        assert!(!at(&repair.1.0, "/error").as_str().unwrap_or("").contains("rolled back"));

        let conflict = classify_failure("", "stale generation for skill demo: expected 1, active is 2");
        assert_eq!(conflict.0, StatusCode::CONFLICT);

        let missing = classify_failure("", "no such proposal proposal-1");
        let foreign = classify_failure("", "revision not found: r1-other");
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
        assert_eq!(foreign.0, StatusCode::NOT_FOUND);
        assert_eq!(at(&missing.1.0, "/error"), at(&foreign.1.0, "/error"));

        let denied = classify_failure(
            "",
            "the skill lifecycle store belongs to another owner scope (configured owner_scope: owner:ws:cli:other; this caller's owner id: api:token)",
        );
        assert_eq!(denied.0, StatusCode::FORBIDDEN);
        let error = at(&denied.1.0, "/error");
        let text = error.as_str().unwrap_or("");
        assert!(!text.contains("owner:ws"));
        assert!(!text.contains("api:token"));
    }

    #[tokio::test]
    async fn lifecycle_routes_honor_owner_scope_and_stay_unavailable_when_disabled() {
        let disabled = serve_config(|_| {}).await;
        let (status, _) = disabled
            .call_anonymous(reqwest::Method::GET, "/api/skills/lifecycle/status", None)
            .await;
        assert_eq!(status, 401);
        let (status, body) = disabled
            .call(reqwest::Method::GET, "/api/skills/lifecycle/status", None)
            .await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(at(&json(&body), "/success"), false);

        let owner = ApiPrincipal::of_token(TOKEN).owner_id;
        let served = serve_config(enable(owner)).await;
        let (status, body) = served
            .call(reqwest::Method::GET, "/api/skills/lifecycle/status", None)
            .await;
        assert_eq!(status, 200, "{body}");

        let missing = format!("prop-{}", "ab".repeat(16));
        let other_id = format!("prop-{}", "cd".repeat(16));
        let (status, body) = served
            .call(
                reqwest::Method::GET,
                &format!("/api/skills/lifecycle/proposals/{missing}"),
                None,
            )
            .await;
        assert_eq!(status, 404, "{body}");
        let (status_other, body_other) = served
            .call(
                reqwest::Method::GET,
                &format!("/api/skills/lifecycle/proposals/{other_id}"),
                None,
            )
            .await;
        assert_eq!(status_other, 404, "{body_other}");
        assert_eq!(at(&json(&body), "/error"), at(&json(&body_other), "/error"));

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/skills/lifecycle/proposals/missing-proposal/publish",
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(
            at(&json(&body), "/error"),
            "publish and rollback need the operator's full session"
        );

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                &format!("/api/skills/lifecycle/proposals/{missing}/cancel"),
                None,
            )
            .await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(at(&json(&body), "/error"), "not found");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/skills/lifecycle/rollback",
                Some(serde_json::json!({
                    "skill_id": "demo",
                    "revision_id": "r1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "expected_generation": 1
                })),
            )
            .await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(
            at(&json(&body), "/error"),
            "publish and rollback need the operator's full session"
        );

        let (status, _) = served
            .call(
                reqwest::Method::POST,
                "/api/skills/lifecycle/rollback",
                Some(serde_json::json!({
                    "skill_id": "demo",
                    "revision_id": "r1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "expected_generation": 1,
                    "owner_id": "api:other"
                })),
            )
            .await;
        assert_eq!(status, 400);

        let (status, _) = served
            .call(
                reqwest::Method::POST,
                "/api/skills/lifecycle/rollback",
                Some(serde_json::json!({ "skill_id": "demo" })),
            )
            .await;
        assert_eq!(status, 400);

        let other = serve_config(enable("owner:ws:cli:other".to_string())).await;
        let (status, body) = other
            .call(reqwest::Method::GET, "/api/skills/lifecycle/status", None)
            .await;
        assert_eq!(status, 403, "{body}");
        assert!(!at(&json(&body), "/error").as_str().unwrap_or("").contains("owner:ws"));
    }
}
