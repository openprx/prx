//! `GET /api/owner` (HOST-1a, `owner_id_stable_v1`): the install owner id
//! every paired bearer acts for. It is an rf contract route behind
//! [`super::capabilities::guard`], so every error has the coded shape. Only
//! an owner-authenticated caller (pairing on and a paired bearer) gets the
//! id; with pairing off no caller is the install owner and the answer is
//! `401 auth_failed`. The id is random, not derived from any credential,
//! and the body carries nothing else.

use super::capabilities::coded_error;
use super::{ApiPrincipal, AppState, HeaderMap, Json, State, StatusCode};
use axum::response::{IntoResponse, Response};

/// `200 {"owner_id":"api:install:<32 hex>"}` for a paired bearer.
pub(super) async fn get_owner(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<ApiPrincipal>,
) -> Response {
    if !super::owner_authenticated(&state, &headers) {
        return coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a paired bearer token is required; the install owner is not served while pairing is off",
        );
    }
    Json(serde_json::json!({ "owner_id": principal.owner_id })).into_response()
}
