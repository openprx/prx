//! Workspace cost report for the console.
//!
//! [`crate::cost::tracker::workspace_cost_report`] reads the workspace ledger.
//! It does not take a token owner, so the response scope is `workspace`.
//! Unpriced requests are omitted from that ledger; they are not reported as zero.

use super::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

type ApiResponse = (StatusCode, Json<Value>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CostQuery {
    window: String,
}

fn bad_request(error: impl Into<String>) -> ApiResponse {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error.into() })))
}

fn window_field(window: &str) -> Option<&'static str> {
    match window {
        "today" => Some("today"),
        "month" => Some("month"),
        "all" => Some("all_time"),
        _ => None,
    }
}

pub(super) async fn get_cost(State(state): State<AppState>, Query(query): Query<CostQuery>) -> ApiResponse {
    let Some(field) = window_field(query.window.trim()) else {
        return bad_request("window must be today, month, or all");
    };
    let config = state.config.load_full();
    let report = match crate::cost::tracker::workspace_cost_report(&config.workspace_dir) {
        Ok(report) => report,
        Err(error) => {
            tracing::warn!(%error, "workspace cost report failed");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "success": false,
                    "error": "workspace cost report is unavailable"
                })),
            );
        }
    };
    let mut value = match serde_json::to_value(&report) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "workspace cost report could not be encoded");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "workspace cost report could not be encoded" })),
            );
        }
    };
    let selected = value.get(field).cloned().unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert("scope".to_string(), json!("workspace"));
        object.insert("window".to_string(), json!(query.window.trim()));
        object.insert("estimates".to_string(), json!(true));
        object.insert(
            "estimate_note".to_string(),
            json!("reservation amounts include estimates and are not settled charges"),
        );
        object.insert("unpriced_usage".to_string(), json!("omitted"));
        object.insert("selected".to_string(), selected);
    }
    (StatusCode::OK, Json(value))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{at, json, serve_config};

    #[tokio::test]
    async fn cost_report_is_workspace_scoped_and_does_not_zero_fill_unknown_prices() {
        let served = serve_config(|_| {}).await;
        let (status, _) = served
            .call_anonymous(reqwest::Method::GET, "/api/cost?window=today", None)
            .await;
        assert_eq!(status, 401);

        let (status, _) = served.call(reqwest::Method::GET, "/api/cost?window=week", None).await;
        assert_eq!(status, 400);

        let (status, body) = served.call(reqwest::Method::GET, "/api/cost?window=today", None).await;
        assert_eq!(status, 200, "{body}");
        let report = json(&body);
        assert_eq!(at(&report, "/scope"), "workspace");
        assert_eq!(at(&report, "/estimates"), true);
        assert_eq!(at(&report, "/unpriced_usage"), "omitted");
        assert!(report.get("unknown_cost_usd").is_none());
        assert!(report.get("selected").is_some());
        assert!(report.get("today").is_some());
        assert!(report.get("outstanding_reservation_micro_usd").is_some());

        let blocked = serve_config(|_| {}).await;
        let workspace = blocked.state.config.load_full().workspace_dir.clone();
        std::fs::remove_dir_all(&workspace).expect("test: remove workspace");
        std::fs::write(&workspace, b"blocked").expect("test: block workspace");
        let (status, body) = blocked.call(reqwest::Method::GET, "/api/cost?window=all", None).await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(at(&json(&body), "/success"), false);
    }
}
