//! Owner-authenticated snapshot paging outside the API minute limiter.
use super::{ApiPrincipal, AppState, capabilities::coded_error};
use crate::memory::sync::SyncError;
use axum::{
    Extension, Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SnapshotQuery {
    page_token: Option<String>,
    limit: Option<usize>,
}

pub(super) async fn snapshot(
    State(state): State<AppState>,
    Extension(principal): Extension<ApiPrincipal>,
    headers: HeaderMap,
    query: Result<Query<SnapshotQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !super::owner_authenticated(&state, &headers) {
        return coded_error(StatusCode::UNAUTHORIZED, "auth_failed", "a paired bearer is required");
    }
    let Ok(Query(query)) = query else {
        return coded_error(StatusCode::BAD_REQUEST, "field_invalid", "invalid snapshot query");
    };
    let Some(store) = state.mem.sync_store() else {
        return coded_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "this backend has no transactional sync store",
        );
    };
    let result = crate::runtime::blocking::spawn_blocking(move || {
        store.snapshot(
            &principal.owner_id,
            query.page_token.as_deref().filter(|s| !s.is_empty()),
            query.limit.unwrap_or(500),
        )
    })
    .await;
    match result {
        Ok(Ok(page)) => Json(page).into_response(),
        Ok(Err(error)) => match error.downcast_ref::<SyncError>() {
            Some(SyncError::SnapshotExpired) => {
                coded_error(StatusCode::GONE, "snapshot_expired", "start a fresh snapshot")
            }
            Some(SyncError::RateLimited) => coded_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "two snapshots are already active",
            ),
            Some(SyncError::Invalid) => coded_error(
                StatusCode::BAD_REQUEST,
                "field_invalid",
                "invalid snapshot page token or limit",
            ),
            _ => {
                tracing::warn!(error_kind=%crate::agent::diagnostics::error_class(&error),"sync snapshot read failed");
                coded_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage_unavailable",
                    "snapshot storage could not be read",
                )
            }
        },
        Err(_) => coded_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_unavailable",
            "snapshot worker failed",
        ),
    }
}
