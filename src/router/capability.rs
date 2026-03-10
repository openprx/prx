use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use crate::config::{RouterConfig, RouterModelConfig};
use crate::memory::{Memory, MemoryCategory};
use crate::router::models::builtin_model_capabilities;
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
    pub async fn load_all(config: &RouterConfig, memory: &dyn Memory) -> Vec<Self> {
        let models = merged_router_models(config);
        let mut entries = Vec::with_capacity(models.len());
        for model in &models {
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

    pub async fn save_metrics(&self, memory: &dyn Memory, recent_successes: &[bool]) -> Result<()> {
        let metrics_payload = serde_json::to_string(&RouterMetricsSnapshot {
            dynamic_elo: self.dynamic_elo,
            success_rate: self.success_rate,
            recent_latency_ms: self.recent_latency_ms,
        })?;
        let success_rate_payload = serde_json::to_string(&RouterSuccessRateSnapshot {
            recent_successes: recent_successes.iter().copied().collect(),
            success_rate: self.success_rate,
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
                &format!("router/success_rate/{}", self.config.model_id),
                &success_rate_payload,
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

pub fn merged_router_models(config: &RouterConfig) -> Vec<RouterModelConfig> {
    let mut models = builtin_model_capabilities();

    for override_model in &config.models {
        if let Some(existing) = models.iter_mut().find(|entry| {
            entry.provider == override_model.provider && entry.model_id == override_model.model_id
        }) {
            *existing = override_model.clone();
        } else {
            models.push(override_model.clone());
        }
    }

    models
}

pub async fn load_recent_successes(memory: &dyn Memory, model_id: &str) -> VecDeque<bool> {
    load_success_rate_snapshot(memory, model_id)
        .await
        .map(|value| value.recent_successes)
        .unwrap_or_default()
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
