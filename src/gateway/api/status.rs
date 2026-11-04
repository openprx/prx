use super::{AppState, configured_channel_names, resolve_memory_backend};
use axum::{Json, extract::State};
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct StatusResponse {
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
        // Labels and reasons may quote a `custom:` URL carrying
        // `user:password@`; this response leaves the host.
        provider_available: availability
            .available
            .iter()
            .map(|name| crate::providers::redact_endpoint_userinfo(name).into_owned())
            .collect(),
        provider_unavailable: availability
            .unavailable
            .into_iter()
            .map(|(name, reason)| {
                format!(
                    "{}: {}",
                    crate::providers::redact_endpoint_userinfo(&name),
                    crate::providers::redact_endpoint_userinfo(&reason)
                )
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
mod tests {
    use super::*;
    #[tokio::test]
    async fn get_status_exposes_redacted_prompt_cache_row() {
        use crate::agent::prompt_cache_metrics::{RoundTrip, Series, record_round_trip};
        let provider = [
            "custom:https://user:",
            "sk-live-",
            "SECRET@example.test/v1?key=",
            "QUERYSECRET",
        ]
        .concat();
        record_round_trip(RoundTrip {
            series: Series::for_turn(None, &crate::agent::turn_request::ToolLoopMemory::none()),
            provider: &provider,
            model: "status-cache-fixture",
            system_prompt_sha256: None,
            tools_sha256: "tools",
            usage: &crate::llm::route_decision::TokenUsage::reported_with_cache(
                Some(1000),
                Some(10),
                Some(1010),
                Some(0),
                Some(800),
            ),
        });
        let workspace = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            workspace_dir: workspace.path().into(),
            config_path: workspace.path().join("config.toml"),
            ..crate::config::Config::default()
        };
        let state = super::super::test_support::app_state(config);
        let body = serde_json::to_value(get_status(State(state)).await.0).unwrap();
        let rows = body.get("prompt_cache").unwrap().as_array().unwrap();
        let row = rows
            .iter()
            .find(|row| {
                row.get("provider_model")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .ends_with("/status-cache-fixture")
            })
            .unwrap();
        assert_eq!(row.get("requests_total"), Some(&serde_json::json!(1)));
        assert_eq!(row.get("cache_reported_total"), Some(&serde_json::json!(1)));
        assert_eq!(row.get("cache_hit_requests_total"), Some(&serde_json::json!(1)));
        assert_eq!(row.get("cache_read_tokens_total"), Some(&serde_json::json!(800)));
        let text = body.to_string();
        for secret in [["sk-live-", "SECRET"].concat(), "QUERYSECRET".into(), "user:".into()] {
            assert!(!text.contains(&secret), "credential leaked");
        }
    }
}
