//! Web Console API routes for the WASM plugin system.
//!
//! - `GET /api/plugins` — list all loaded plugins
//! - `POST /api/plugins/{name}/reload` — reload a plugin by name

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use super::super::AppState;

/// GET /api/plugins — list all loaded plugins with status.
pub async fn list_plugins(State(state): State<AppState>) -> impl IntoResponse {
    #[cfg(feature = "wasm-plugins")]
    {
        if let Some(ref pm) = state.plugin_manager {
            let plugins = pm.list_plugins().await;
            return (StatusCode::OK, Json(serde_json::json!({
                "plugins": plugins,
                "count": plugins.len(),
            })));
        }
    }

    // Feature not enabled or no plugin manager
    let _ = state; // suppress unused warning when feature is off
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "plugins": [],
            "count": 0,
            "note": "WASM plugin system not enabled (compile with --features wasm-plugins)",
        })),
    )
}

/// POST /api/plugins/{name}/reload — reload a specific plugin.
pub async fn reload_plugin(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    #[cfg(feature = "wasm-plugins")]
    {
        if let Some(ref pm) = state.plugin_manager {
            return match pm.reload_plugin(&name).await {
                Ok(()) => (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true,
                        "message": format!("plugin '{name}' reloaded"),
                    })),
                ),
                Err(e) => (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "success": false,
                        "error": e.to_string(),
                    })),
                ),
            };
        }
    }

    let _ = (state, name);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "success": false,
            "error": "WASM plugin system not available",
        })),
    )
}
