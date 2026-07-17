use super::{AppState, configured_channel_names, resolve_memory_backend};
use axum::{Json, extract::State};
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
    active_config_generation: u64,
    active_config_source_revision: String,
    desired_config_source_revision: String,
    config_reload_status: String,
    config_reload_in_progress: bool,
    config_generation_participants: Vec<String>,
    restart_required: Vec<String>,
    last_config_reload_failure: Option<String>,
}

pub async fn get_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let generation = state.config.pin();
    let desired = state.config.desired();
    let generation_status = state.config.status();
    let config = generation.effective.as_ref().clone();
    let model = config.default_model.clone().unwrap_or_else(|| state.model.clone());
    let provider_runtime_options = crate::providers::provider_runtime_options_from_config(&config);

    let availability = crate::providers::summarize_provider_availability(
        config.default_provider.as_deref().unwrap_or("openrouter"),
        config.api_key.as_deref(),
        &config.reliability,
        &provider_runtime_options,
    );

    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
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
        active_config_generation: generation.id.0,
        active_config_source_revision: generation.source_revision.fingerprint_sha256.clone(),
        desired_config_source_revision: desired.source_revision.fingerprint_sha256.clone(),
        config_reload_status: generation_status.last_failure.as_ref().map_or_else(
            || {
                generation_status
                    .last_report
                    .as_ref()
                    .map_or("idle", crate::config::ConfigApplyReport::status)
                    .to_string()
            },
            |_| "failed".to_string(),
        ),
        config_reload_in_progress: generation_status.reload_in_progress,
        config_generation_participants: generation_status.registered_participants,
        restart_required: generation_status
            .last_report
            .as_ref()
            .map_or_else(Vec::new, |report| report.restart_required.clone()),
        last_config_reload_failure: generation_status.last_failure.map(|failure| failure.error),
    })
}
