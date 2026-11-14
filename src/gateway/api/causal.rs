//! The retired `/api/causal/*` routes (OV-7; P2-R9): each of the ten former
//! routes answers `410 Gone` and points at `/api/decision-engine`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// The ten retired paths, relative to `/api`.
pub(super) const RETIRED_ROUTES: [&str; 10] = [
    "/causal/status",
    "/causal/backends",
    "/causal/test",
    "/causal/decisions",
    "/causal/decisions/{id}",
    "/causal/reports",
    "/causal/reports/saved",
    "/causal/config/plans",
    "/causal/config/apply",
    "/causal/credential",
];

/// The one answer of every retired route.
pub(super) async fn gone() -> Response {
    (
        StatusCode::GONE,
        Json(json!({"error": "gone", "see": "/api/decision-engine"})),
    )
        .into_response()
}
