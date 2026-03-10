use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{RouterConfig, RouterModelConfig};
use crate::memory::{Memory, MemoryCategory};
use crate::self_system::SELF_SYSTEM_SESSION_ID;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouterMetricsSnapshot {
    dynamic_elo: f32,
    success_rate: f32,
    recent_latency_ms: u32,
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
        let mut entries = Vec::with_capacity(config.models.len());
        for model in &config.models {
            let key = format!("router/elo/{}", model.model_id);
            let snapshot = memory.get(&key).await.ok().flatten().and_then(|entry| {
                serde_json::from_str::<RouterMetricsSnapshot>(&entry.content).ok()
            });

            let dynamic_elo = snapshot
                .as_ref()
                .map_or(model.elo_rating, |metrics| metrics.dynamic_elo);

            entries.push(Self {
                config: model.clone(),
                dynamic_elo,
                success_rate: snapshot
                    .as_ref()
                    .map_or(1.0, |metrics| metrics.success_rate),
                recent_latency_ms: snapshot
                    .as_ref()
                    .map_or(model.latency_ms, |metrics| metrics.recent_latency_ms),
            });
        }
        entries
    }

    pub async fn save_metrics(&self, memory: &dyn Memory) -> Result<()> {
        let payload = serde_json::to_string(&RouterMetricsSnapshot {
            dynamic_elo: self.dynamic_elo,
            success_rate: self.success_rate,
            recent_latency_ms: self.recent_latency_ms,
        })?;

        memory
            .store(
                &format!("router/elo/{}", self.config.model_id),
                &payload,
                MemoryCategory::Custom("router".to_string()),
                Some(SELF_SYSTEM_SESSION_ID),
            )
            .await
    }
}
