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
    provider_degraded: bool,
    provider_available: Vec<String>,
    provider_unavailable: Vec<String>,
}

pub async fn get_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let config = state.config.lock().clone();
    let model = config
        .default_model
        .clone()
        .unwrap_or_else(|| state.model.clone());

    let availability = crate::providers::summarize_provider_availability(
        config.default_provider.as_deref().unwrap_or("openrouter"),
        config.api_key.as_deref(),
        &config.reliability,
    );

    Json(StatusResponse {
        version: std::env::var("OPENPRX_VERSION")
            .or_else(|_| std::env::var("ZEROCLAW_VERSION"))
            .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string()),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        model,
        memory_backend: resolve_memory_backend(&config),
        channels: configured_channel_names(&config),
        gateway_port: state.gateway_port,
        provider_degraded: availability.degraded,
        provider_available: availability.available,
        provider_unavailable: availability
            .unavailable
            .into_iter()
            .map(|(name, reason)| format!("{name}: {reason}"))
            .collect(),
    })
}
