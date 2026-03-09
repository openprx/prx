use super::{configured_channel_names, resolve_memory_backend, AppState};
use axum::{extract::State, Json};
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct StatusResponse {
    version: String,
    uptime_seconds: u64,
    model: String,
    memory_backend: String,
    channels: Vec<String>,
    gateway_port: u16,
}

pub async fn get_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let config = state.config.lock().clone();
    let model = config
        .default_model
        .clone()
        .unwrap_or_else(|| state.model.clone());

    Json(StatusResponse {
        version: std::env::var("OPENPRX_VERSION")
            .or_else(|_| std::env::var("ZEROCLAW_VERSION"))
            .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string()),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        model,
        memory_backend: resolve_memory_backend(&config),
        channels: configured_channel_names(&config),
        gateway_port: state.gateway_port,
    })
}
