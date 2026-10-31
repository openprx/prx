//! Memory HTTP routes.
//!
//! The memory tools do not take [`ApiPrincipal`]. Their principal is a
//! service-authored `_zc_scope`, and this request has no session envelope to
//! author one. The routes therefore stay unavailable. They never execute the
//! tools and they never accept `owner_id` from the body.

use super::AppState;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

const UNAVAILABLE: &str = "memory tools cannot take the API principal; search, recall, store, and forget are unavailable until a service authors an owner scope for this request";

type ApiResponse = (StatusCode, Json<Value>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchBody {
    query: String,
    #[serde(default)]
    max_results: Option<u32>,
    #[serde(default, rename = "maxResults")]
    max_results_camel: Option<u32>,
    #[serde(default, rename = "minScore")]
    min_score: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecallBody {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreBody {
    key: String,
    content: String,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetBody {
    key: String,
}

fn unavailable() -> ApiResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "success": false, "error": UNAVAILABLE })),
    )
}

fn bad_request(error: impl Into<String>) -> ApiResponse {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error.into() })))
}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &Bytes) -> Result<T, ApiResponse> {
    if bytes.is_empty() {
        return Err(bad_request("request body is required"));
    }
    serde_json::from_slice(bytes).map_err(|error| bad_request(format!("malformed request: {error}")))
}

fn require_text(value: &str, field: &str) -> Result<(), ApiResponse> {
    if value.trim().is_empty() {
        Err(bad_request(format!("{field} is required")))
    } else {
        Ok(())
    }
}

pub(super) async fn search(State(_state): State<AppState>, body: Bytes) -> ApiResponse {
    let parsed: SearchBody = match parse_json(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    if let Err(response) = require_text(&parsed.query, "query") {
        return response;
    }
    let _ = (parsed.max_results, parsed.max_results_camel, parsed.min_score);
    unavailable()
}

pub(super) async fn recall(State(_state): State<AppState>, body: Bytes) -> ApiResponse {
    let parsed: RecallBody = match parse_json(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    if let Err(response) = require_text(&parsed.query, "query") {
        return response;
    }
    let _ = (parsed.limit, parsed.session_id);
    unavailable()
}

pub(super) async fn store(State(_state): State<AppState>, body: Bytes) -> ApiResponse {
    let parsed: StoreBody = match parse_json(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    if let Err(response) = require_text(&parsed.key, "key") {
        return response;
    }
    if let Err(response) = require_text(&parsed.content, "content") {
        return response;
    }
    let _ = parsed.category;
    unavailable()
}

pub(super) async fn forget(State(_state): State<AppState>, body: Bytes) -> ApiResponse {
    let parsed: ForgetBody = match parse_json(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    if let Err(response) = require_text(&parsed.key, "key") {
        return response;
    }
    unavailable()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{at, json, serve_config};

    #[tokio::test]
    async fn memory_routes_are_unavailable_and_reject_an_owner_override() {
        let served = serve_config(|_| {}).await;
        let (status, _) = served
            .call_anonymous(
                reqwest::Method::POST,
                "/api/memory/search",
                Some(serde_json::json!({ "query": "notes" })),
            )
            .await;
        assert_eq!(status, 401);

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/search",
                Some(serde_json::json!({ "query": "notes", "owner_id": "api:other" })),
            )
            .await;
        assert_eq!(status, 400, "{body}");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/search",
                Some(serde_json::json!({ "query": "notes" })),
            )
            .await;
        assert_eq!(status, 503, "{body}");
        let parsed = json(&body);
        assert_eq!(at(&parsed, "/success"), false);
        let unavailable_error = at(&parsed, "/error");

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/forget",
                Some(serde_json::json!({ "key": "foreign-key" })),
            )
            .await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(at(&json(&body), "/error"), unavailable_error);
        assert_ne!(status, 404);

        let (status, _) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/recall",
                Some(serde_json::json!({ "query": "" })),
            )
            .await;
        assert_eq!(status, 400);

        let (status, body) = served
            .call(
                reqwest::Method::POST,
                "/api/memory/store",
                Some(serde_json::json!({ "key": "lang", "content": "notes" })),
            )
            .await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(at(&json(&body), "/success"), false);
    }
}
