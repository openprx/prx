use super::AppState;
use crate::config::Config;
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
    Json, Router,
};

mod channels;
mod config;
mod hooks;
mod logs;
mod mcp;
mod sessions;
mod skills;
mod status;

pub fn router(state: AppState) -> Router<AppState> {
    let protected_routes = Router::new()
        .route("/status", get(status::get_status))
        .route("/sessions", get(sessions::get_sessions))
        .route(
            "/sessions/{id}/messages",
            get(sessions::get_session_messages),
        )
        .route(
            "/sessions/{id}/message",
            post(sessions::post_session_message),
        )
        .route("/channels", get(channels::get_channels_status))
        // Phase 1: alias for frontend compatibility
        .route("/channels/status", get(channels::get_channels_status))
        .route("/config", get(config::get_config))
        // Phase 2: read-only endpoints
        .route("/hooks", get(hooks::get_hooks))
        .route("/mcp/servers", get(mcp::get_mcp_servers))
        .route("/skills", get(skills::get_skills))
        // Phase 3: CRUD endpoints
        .route("/hooks", post(hooks::create_hook))
        .route("/hooks/{id}", put(hooks::update_hook))
        .route("/hooks/{id}", delete(hooks::delete_hook))
        .route("/hooks/{id}/toggle", patch(hooks::toggle_hook))
        .route("/skills/{id}/toggle", patch(skills::toggle_skill))
        .route_layer(middleware::from_fn_with_state(state, auth_middleware));

    Router::new()
        .route("/logs", get(logs::ws_handler))
        // Phase 1: alias for frontend compatibility
        .route("/logs/stream", get(logs::ws_handler))
        .route("/sessions/media", get(sessions::get_session_media))
        .merge(protected_routes)
}

async fn auth_middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let provided_token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");

    if state.pairing.require_pairing() && !state.pairing.is_authenticated(provided_token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        )
            .into_response();
    }

    next.run(request).await
}

pub(super) fn configured_channel_names(config: &Config) -> Vec<String> {
    let mut channels = Vec::new();
    if config.channels_config.cli {
        channels.push("cli".to_string());
    }
    if config.channels_config.telegram.is_some() {
        channels.push("telegram".to_string());
    }
    if config.channels_config.discord.is_some() {
        channels.push("discord".to_string());
    }
    if config.channels_config.slack.is_some() {
        channels.push("slack".to_string());
    }
    if config.channels_config.mattermost.is_some() {
        channels.push("mattermost".to_string());
    }
    if config.channels_config.webhook.is_some() {
        channels.push("webhook".to_string());
    }
    if config.channels_config.imessage.is_some() {
        channels.push("imessage".to_string());
    }
    if config.channels_config.matrix.is_some() {
        channels.push("matrix".to_string());
    }
    if config.channels_config.signal.is_some() {
        channels.push("signal".to_string());
    }
    if config.channels_config.whatsapp.is_some() {
        channels.push("whatsapp".to_string());
    }
    if config.channels_config.wacli.is_some() {
        channels.push("wacli".to_string());
    }
    if config.channels_config.linq.is_some() {
        channels.push("linq".to_string());
    }
    if config.channels_config.nextcloud_talk.is_some() {
        channels.push("nextcloud_talk".to_string());
    }
    if config.channels_config.email.is_some() {
        channels.push("email".to_string());
    }
    if config.channels_config.irc.is_some() {
        channels.push("irc".to_string());
    }
    if config.channels_config.lark.is_some() {
        channels.push("lark".to_string());
    }
    if config.channels_config.dingtalk.is_some() {
        channels.push("dingtalk".to_string());
    }
    if config.channels_config.qq.is_some() {
        channels.push("qq".to_string());
    }
    channels
}

pub(super) fn resolve_memory_backend(config: &Config) -> String {
    let backend = config.memory.backend.trim().to_ascii_lowercase();
    if backend == "sqlite" || backend == "postgres" {
        return backend;
    }

    let storage_provider = config
        .storage
        .provider
        .config
        .provider
        .trim()
        .to_ascii_lowercase();
    if storage_provider == "sqlite" || storage_provider == "postgres" {
        return storage_provider;
    }

    backend
}
