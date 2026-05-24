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
}

pub async fn get_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let config = state.config.lock().clone();
    let model = config.default_model.clone().unwrap_or_else(|| state.model.clone());
    let provider_runtime_options = crate::providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        openprx_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        codex_auth_json_path: Some(config.auth.codex_auth_json_path.clone()),
        codex_auth_json_auto_import: config.auth.codex_auth_json_auto_import,
        reasoning_enabled: config.runtime.reasoning_enabled,
        codex_stream_idle_timeout_secs: config.runtime.codex_stream_idle_timeout_secs,
        codex_reasoning_effort: config.runtime.codex_reasoning_effort.clone(),
    };

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
    })
}
