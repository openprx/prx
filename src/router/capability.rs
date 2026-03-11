use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use uuid::Uuid;

use crate::config::{ModelRouteConfig, RouterModelConfig};
use crate::memory::{Memory, MemoryCategory};
use crate::self_system::SELF_SYSTEM_SESSION_ID;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouterMetricsSnapshot {
    dynamic_elo: f32,
    success_rate: f32,
    recent_latency_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouterSuccessRateSnapshot {
    #[serde(default)]
    recent_successes: VecDeque<bool>,
    #[serde(default = "default_success_rate")]
    success_rate: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouterLatencySnapshot {
    recent_latency_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouterSuccessEvent {
    success: bool,
}

fn default_success_rate() -> f32 {
    1.0
}

#[derive(Debug, Clone)]
pub struct ModelCapabilityEntry {
    pub config: RouterModelConfig,
    pub dynamic_elo: f32,
    pub success_rate: f32,
    pub recent_latency_ms: u32,
}

impl ModelCapabilityEntry {
    pub async fn load_all(models: &[RouterModelConfig], memory: &dyn Memory) -> Vec<Self> {
        let mut entries = Vec::with_capacity(models.len());
        for model in models {
            let metrics = load_metrics_snapshot(memory, &model.model_id).await;
            let stats = load_success_rate_snapshot(memory, &model.model_id).await;
            let latency = load_latency_snapshot(memory, &model.model_id).await;

            let dynamic_elo = metrics
                .as_ref()
                .map_or(model.elo_rating, |metrics| metrics.dynamic_elo);

            entries.push(Self {
                config: model.clone(),
                dynamic_elo,
                success_rate: stats.as_ref().map_or_else(
                    || {
                        metrics
                            .as_ref()
                            .map_or(default_success_rate(), |value| value.success_rate)
                    },
                    |value| value.success_rate,
                ),
                recent_latency_ms: latency.as_ref().map_or_else(
                    || {
                        metrics
                            .as_ref()
                            .map_or(model.latency_ms, |value| value.recent_latency_ms)
                    },
                    |value| value.recent_latency_ms,
                ),
            });
        }
        entries
    }

    pub async fn save_metrics(&self, memory: &dyn Memory) -> Result<()> {
        let metrics_payload = serde_json::to_string(&RouterMetricsSnapshot {
            dynamic_elo: self.dynamic_elo,
            success_rate: self.success_rate,
            recent_latency_ms: self.recent_latency_ms,
        })?;
        let latency_payload = serde_json::to_string(&RouterLatencySnapshot {
            recent_latency_ms: self.recent_latency_ms,
        })?;

        memory
            .store(
                &format!("router/elo/{}", self.config.model_id),
                &metrics_payload,
                MemoryCategory::Custom("router".to_string()),
                Some(SELF_SYSTEM_SESSION_ID),
            )
            .await?;
        memory
            .store(
                &format!("router/latency/{}", self.config.model_id),
                &latency_payload,
                MemoryCategory::Custom("router".to_string()),
                Some(SELF_SYSTEM_SESSION_ID),
            )
            .await
    }
}

pub fn reachable_provider_names(
    default_provider: &str,
    model_routes: &[ModelRouteConfig],
) -> HashSet<String> {
    let mut providers = HashSet::from([default_provider.to_string()]);
    for route in model_routes {
        providers.insert(route.provider.clone());
    }
    providers
}

pub fn filter_models_by_providers(
    models: Vec<RouterModelConfig>,
    allowed_providers: &HashSet<String>,
) -> Vec<RouterModelConfig> {
    models
        .into_iter()
        .filter(|model| allowed_providers.contains(&model.provider))
        .collect()
}

pub fn route_matches_model(route: &ModelRouteConfig, provider: &str, model_id: &str) -> bool {
    route.provider == provider && route.model == model_id
}

pub fn is_model_reachable(
    default_provider: &str,
    model_routes: &[ModelRouteConfig],
    provider: &str,
    model_id: &str,
) -> bool {
    provider == default_provider
        || model_routes
            .iter()
            .any(|route| route_matches_model(route, provider, model_id))
}

pub async fn append_success_event(
    memory: &dyn Memory,
    model_id: &str,
    success: bool,
) -> Result<()> {
    let timestamp = Utc::now().timestamp_millis();
    let key = format!("router/success/{model_id}/{timestamp}-{}", Uuid::new_v4());
    let payload = serde_json::to_string(&RouterSuccessEvent { success })?;
    memory
        .store(
            &key,
            &payload,
            MemoryCategory::Custom("router".to_string()),
            Some(SELF_SYSTEM_SESSION_ID),
        )
        .await
}

pub async fn load_recent_successes(memory: &dyn Memory, model_id: &str) -> VecDeque<bool> {
    let prefix = format!("router/success/{model_id}/");
    let mut events: Vec<_> = memory
        .list(
            Some(&MemoryCategory::Custom("router".to_string())),
            Some(SELF_SYSTEM_SESSION_ID),
        )
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.key.starts_with(&prefix))
        .collect();
    events.sort_by(|left, right| left.key.cmp(&right.key));

    let successes: VecDeque<bool> = events
        .into_iter()
        .filter_map(|entry| serde_json::from_str::<RouterSuccessEvent>(&entry.content).ok())
        .map(|entry| entry.success)
        .collect();

    if successes.is_empty() {
        load_success_rate_snapshot(memory, model_id)
            .await
            .map(|value| value.recent_successes)
            .unwrap_or_default()
    } else {
        successes
            .into_iter()
            .rev()
            .take(100)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

async fn load_metrics_snapshot(
    memory: &dyn Memory,
    model_id: &str,
) -> Option<RouterMetricsSnapshot> {
    let key = format!("router/elo/{model_id}");
    memory
        .get(&key)
        .await
        .ok()
        .flatten()
        .and_then(|entry| serde_json::from_str::<RouterMetricsSnapshot>(&entry.content).ok())
}

pub async fn load_success_rate_snapshot(
    memory: &dyn Memory,
    model_id: &str,
) -> Option<RouterSuccessRateSnapshot> {
    for key in [
        format!("router/success_rate/{model_id}"),
        format!("router/stats/{model_id}"),
    ] {
        if let Some(snapshot) = memory
            .get(&key)
            .await
            .ok()
            .flatten()
            .and_then(|entry| serde_json::from_str::<RouterSuccessRateSnapshot>(&entry.content).ok())
        {
            return Some(snapshot);
        }
    }

    None
}

async fn load_latency_snapshot(
    memory: &dyn Memory,
    model_id: &str,
) -> Option<RouterLatencySnapshot> {
    let key = format!("router/latency/{model_id}");
    memory
        .get(&key)
        .await
        .ok()
        .flatten()
        .and_then(|entry| serde_json::from_str::<RouterLatencySnapshot>(&entry.content).ok())
}
