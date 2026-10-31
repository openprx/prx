//! Controlled lifecycle-experiment HTTP adapter.
//!
//! The handler validates the envelope and refuses study actions. It does not
//! mint an [`crate::runtime::h2_entry::EntryInvocation`]: the only mint needs a
//! verified decision call, which an API token does not carry. Run is not
//! submitted as a session-message job, because that job kind does not own the
//! lifecycle continuation.

use super::AppState;
use crate::runtime::h2_entry::EntryAction;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

const FORBIDDEN_REQUEST_KEYS: &[&str] = &[
    "owner",
    "owner_id",
    "workspace",
    "workspace_id",
    "mac",
    "pins",
    "grants",
    "grant",
    "proof",
    "approval_grant",
];

type ApiResponse = (StatusCode, Json<Value>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum H2Disposition {
    Denied,
    Rejected,
    Unavailable,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    action: String,
    request: Value,
}

fn error_response(status: StatusCode, error: impl Into<String>) -> ApiResponse {
    (status, Json(json!({ "error": error.into() })))
}

fn request_forges_authority(request: &Value) -> bool {
    let Some(object) = request.as_object() else {
        return false;
    };
    object
        .keys()
        .any(|key| FORBIDDEN_REQUEST_KEYS.contains(&key.as_str()) || crate::tools::execution::is_runtime_only_arg(key))
}

pub(super) fn disposition(profile_attached: bool, action: &str) -> H2Disposition {
    if EntryAction::parse(action.trim()).is_err() {
        return H2Disposition::Rejected;
    }
    if !profile_attached {
        H2Disposition::Denied
    } else {
        H2Disposition::Unavailable
    }
}

fn profile_attached(state: &AppState) -> bool {
    state
        .turn_runtime
        .as_ref()
        .is_some_and(|runtime| runtime.has_controlled_profile())
}

pub(super) async fn post_h2(State(state): State<AppState>, body: Bytes) -> ApiResponse {
    if body.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "request body is required");
    }
    let envelope: Envelope = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, format!("malformed request: {error}")),
    };
    if !envelope.request.is_object() {
        return error_response(StatusCode::BAD_REQUEST, "request must be an object");
    }
    if request_forges_authority(&envelope.request) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "owner, workspace, MAC, pins, grants, and proof are not accepted",
        );
    }
    match disposition(profile_attached(&state), &envelope.action) {
        H2Disposition::Rejected => {
            error_response(StatusCode::CONFLICT, "action is outside the controlled experiment set")
        }
        H2Disposition::Denied => error_response(StatusCode::FORBIDDEN, "controlled profile is not attached"),
        H2Disposition::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "success": false,
                "error": "the gateway cannot mint an H2 invocation from the API principal, and lifecycle_experiment_run does not fit an existing job cleanup owner"
            })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{at, json, serve_config};
    use super::*;

    #[test]
    fn catalogue_without_a_profile_is_forbidden_and_a_profile_cannot_mint() {
        assert_eq!(
            disposition(false, "lifecycle_experiment_catalog"),
            H2Disposition::Denied
        );
        assert_eq!(
            disposition(true, "lifecycle_experiment_run"),
            H2Disposition::Unavailable
        );
        assert_eq!(disposition(true, "lifecycle_experiment_study"), H2Disposition::Rejected);
        assert_eq!(disposition(false, "study"), H2Disposition::Rejected);
    }

    #[tokio::test]
    async fn h2_route_rejects_study_and_forged_authority_and_refuses_without_a_profile() {
        let served = serve_config(|_| {}).await;
        let (status, _) = served
            .call_anonymous(
                reqwest::Method::POST,
                "/api/skills/lifecycle/h2",
                Some(serde_json::json!({
                    "action": "lifecycle_experiment_catalog",
                    "request": {}
                })),
            )
            .await;
        assert_eq!(status, 401);

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/skills/lifecycle/h2",
                Some(serde_json::json!({
                    "action": "lifecycle_experiment_catalog",
                    "request": {},
                    "owner": "api:other"
                })),
            )
            .await;
        assert_eq!(status, 400, "{body}");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/skills/lifecycle/h2",
                Some(serde_json::json!({
                    "action": "lifecycle_experiment_catalog",
                    "request": { "owner_id": "api:other" }
                })),
            )
            .await;
        assert_eq!(status, 400, "{body}");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/skills/lifecycle/h2",
                Some(serde_json::json!({
                    "action": "lifecycle_experiment_study",
                    "request": {}
                })),
            )
            .await;
        assert_eq!(status, 409, "{body}");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/skills/lifecycle/h2",
                Some(serde_json::json!({
                    "action": "lifecycle_experiment_catalog",
                    "request": {}
                })),
            )
            .await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(at(&json(&body), "/error"), "controlled profile is not attached");
    }
}
