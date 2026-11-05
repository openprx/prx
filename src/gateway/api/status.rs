use super::{AppState, configured_channel_names, resolve_memory_backend};
use crate::security::redaction::redact_provider_text;
use axum::{Json, extract::State};
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct StatusResponse {
    decision_engine: crate::decision_engine::status::DecisionEngineStatus,
    prompt_cache: Vec<PromptCacheStatus>,
    version: String,
    uptime_seconds: u64,
    model: String,
    memory_backend: String,
    channels: Vec<String>,
    gateway_port: u16,
    provider_degraded: bool,
    provider_available: Vec<String>,
    provider_unavailable: Vec<String>,
    provider_circuits: Vec<ProviderCircuitStatus>,
    active_config_generation: u64,
    active_config_source_revision: String,
    desired_config_source_revision: String,
    config_reload_status: String,
    config_reload_in_progress: bool,
    config_generation_participants: Vec<String>,
    restart_required: Vec<String>,
    last_config_reload_failure: Option<String>,
}

#[derive(Serialize)]
struct PromptCacheStatus {
    provider_model: String,
    requests_total: u64,
    cache_reported_total: u64,
    cache_hit_requests_total: u64,
    cache_read_tokens_total: u64,
    series_continuations_total: u64,
    system_prefix_changed_total: u64,
    tools_prefix_changed_total: u64,
    series_unkeyed_total: u64,
}

#[derive(Serialize)]
struct ProviderCircuitStatus {
    provider: String,
    state: &'static str,
    consecutive_failures: u32,
    opened_total: u64,
    retry_in_ms: Option<u64>,
}

pub async fn get_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let generation = state.config.pin();
    let desired = state.config.desired();
    let generation_status = state.config.status();
    let config = generation.effective.as_ref().clone();
    let model = config.default_model.clone().unwrap_or_else(|| state.model.clone());
    let provider_runtime_options = crate::providers::provider_runtime_options_from_config(&config);

    let availability = crate::providers::summarize_provider_availability(
        crate::providers::selection::default_provider(&config),
        config.api_key.as_deref(),
        &config.reliability,
        &provider_runtime_options,
    );

    Json(StatusResponse {
        decision_engine: crate::decision_engine::status::snapshot(&config),
        prompt_cache: crate::agent::prompt_cache_metrics::snapshot()
            .into_iter()
            .map(|(provider_model, stats)| PromptCacheStatus {
                provider_model,
                requests_total: stats.requests_total,
                cache_reported_total: stats.cache_reported_total,
                cache_hit_requests_total: stats.cache_hit_requests_total,
                cache_read_tokens_total: stats.cache_read_tokens_total,
                series_continuations_total: stats.series_continuations_total,
                system_prefix_changed_total: stats.system_prefix_changed_total,
                tools_prefix_changed_total: stats.tools_prefix_changed_total,
                series_unkeyed_total: stats.series_unkeyed_total,
            })
            .collect(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        model,
        memory_backend: resolve_memory_backend(&config),
        channels: configured_channel_names(&config),
        gateway_port: state.gateway_port,
        provider_degraded: availability.degraded,
        // Labels and reasons may quote a `custom:` URL carrying userinfo
        // or query credentials; every field in this response leaves the host.
        provider_available: availability
            .available
            .iter()
            .map(|name| redact_provider_text(&crate::providers::redact_endpoint_userinfo(name)))
            .collect(),
        provider_unavailable: availability
            .unavailable
            .into_iter()
            .map(|(name, reason)| {
                format!(
                    "{}: {}",
                    redact_provider_text(&crate::providers::redact_endpoint_userinfo(&name)),
                    redact_provider_text(&crate::providers::redact_endpoint_userinfo(&reason))
                )
            })
            .collect(),
        provider_circuits: crate::providers::circuit_breaker::snapshot()
            .into_iter()
            .map(|(provider, row)| ProviderCircuitStatus {
                provider,
                state: row.state,
                consecutive_failures: row.consecutive_failures,
                opened_total: row.opened_total,
                retry_in_ms: row.retry_in_ms,
            })
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

#[cfg(test)]
mod tests;
