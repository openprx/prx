use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

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

            let dynamic_elo = metrics.as_ref().map_or(model.elo_rating, |metrics| metrics.dynamic_elo);

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

pub fn reachable_provider_names(default_provider: &str, model_routes: &[ModelRouteConfig]) -> HashSet<String> {
    let mut providers = HashSet::from([default_provider.to_string()]);
    for route in model_routes {
        providers.insert(route.provider.clone());
    }
    providers
}

#[allow(clippy::implicit_hasher)]
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

pub async fn append_success_event(memory: &dyn Memory, model_id: &str, success: bool) -> Result<()> {
    static SUCCESS_EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

    let timestamp = Utc::now().timestamp_millis();
    let seq = SUCCESS_EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
    let key = format!("router/success/{model_id}/{timestamp:013}-{seq:020}");
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

async fn load_metrics_snapshot(memory: &dyn Memory, model_id: &str) -> Option<RouterMetricsSnapshot> {
    let key = format!("router/elo/{model_id}");
    memory
        .get(&key)
        .await
        .ok()
        .flatten()
        .and_then(|entry| serde_json::from_str::<RouterMetricsSnapshot>(&entry.content).ok())
}

pub async fn load_success_rate_snapshot(memory: &dyn Memory, model_id: &str) -> Option<RouterSuccessRateSnapshot> {
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

async fn load_latency_snapshot(memory: &dyn Memory, model_id: &str) -> Option<RouterLatencySnapshot> {
    let key = format!("router/latency/{model_id}");
    memory
        .get(&key)
        .await
        .ok()
        .flatten()
        .and_then(|entry| serde_json::from_str::<RouterLatencySnapshot>(&entry.content).ok())
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouterModelConfig;
    use crate::memory::none::NoneMemory;

    fn model(id: &str, provider: &str, elo: f32, latency: u32) -> RouterModelConfig {
        RouterModelConfig {
            model_id: id.to_string(),
            provider: provider.to_string(),
            elo_rating: elo,
            latency_ms: latency,
            cost_per_million_tokens: 0.0,
            max_context: 4096,
            categories: vec![],
        }
    }

    fn route(provider: &str, model_name: &str) -> ModelRouteConfig {
        ModelRouteConfig {
            hint: "default".to_string(),
            provider: provider.to_string(),
            model: model_name.to_string(),
            api_key: None,
        }
    }

    // ── reachable_provider_names ─────────────────────────────────

    #[test]
    fn reachable_includes_default_provider() {
        let names = reachable_provider_names("openai", &[]);
        assert!(names.contains("openai"));
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn reachable_includes_route_providers() {
        let routes = vec![route("anthropic", "claude-3"), route("deepseek", "ds-v3")];
        let names = reachable_provider_names("openai", &routes);
        assert!(names.contains("openai"));
        assert!(names.contains("anthropic"));
        assert!(names.contains("deepseek"));
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn reachable_deduplicates_same_provider() {
        let routes = vec![route("openai", "gpt-4"), route("openai", "gpt-3.5")];
        let names = reachable_provider_names("openai", &routes);
        assert_eq!(names.len(), 1);
    }

    // ── filter_models_by_providers ──────────────────────────────

    #[test]
    fn filter_keeps_matching_providers() {
        let models = vec![
            model("gpt-4", "openai", 1200.0, 500),
            model("claude-3", "anthropic", 1100.0, 300),
            model("ds-v3", "deepseek", 1000.0, 200),
        ];
        let allowed: HashSet<String> = ["openai", "deepseek"].iter().map(|s| s.to_string()).collect();
        let filtered = filter_models_by_providers(models, &allowed);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|m| m.provider != "anthropic"));
    }

    #[test]
    fn filter_empty_allowed_returns_nothing() {
        let models = vec![model("gpt-4", "openai", 1200.0, 500)];
        let allowed = HashSet::new();
        let filtered = filter_models_by_providers(models, &allowed);
        assert!(filtered.is_empty());
    }

    // ── route_matches_model ─────────────────────────────────────

    #[test]
    fn route_matches_exact() {
        let r = route("openai", "gpt-4");
        assert!(route_matches_model(&r, "openai", "gpt-4"));
    }

    #[test]
    fn route_no_match_wrong_provider() {
        let r = route("openai", "gpt-4");
        assert!(!route_matches_model(&r, "anthropic", "gpt-4"));
    }

    #[test]
    fn route_no_match_wrong_model() {
        let r = route("openai", "gpt-4");
        assert!(!route_matches_model(&r, "openai", "gpt-3.5"));
    }

    // ── is_model_reachable ──────────────────────────────────────

    #[test]
    fn reachable_via_default_provider() {
        assert!(is_model_reachable("openai", &[], "openai", "gpt-4"));
    }

    #[test]
    fn reachable_via_route() {
        let routes = vec![route("anthropic", "claude-3")];
        assert!(is_model_reachable("openai", &routes, "anthropic", "claude-3"));
    }

    #[test]
    fn unreachable_unknown_provider() {
        assert!(!is_model_reachable("openai", &[], "anthropic", "claude-3"));
    }

    // ── load_all with empty memory (NoneMemory) ─────────────────

    #[tokio::test]
    async fn load_all_uses_config_defaults_when_memory_empty() {
        let models = vec![
            model("gpt-4", "openai", 1500.0, 400),
            model("claude-3", "anthropic", 1200.0, 300),
        ];
        let memory = NoneMemory;
        let entries = ModelCapabilityEntry::load_all(&models, &memory).await;

        assert_eq!(entries.len(), 2);
        assert!((entries[0].dynamic_elo - 1500.0).abs() < f32::EPSILON);
        assert_eq!(entries[0].recent_latency_ms, 400);
        assert!((entries[0].success_rate - 1.0).abs() < f32::EPSILON); // default
        assert!((entries[1].dynamic_elo - 1200.0).abs() < f32::EPSILON);
    }

    // ── default_success_rate ────────────────────────────────────

    #[test]
    fn default_success_rate_is_one() {
        assert!((default_success_rate() - 1.0).abs() < f32::EPSILON);
    }

    // ── RouterSuccessRateSnapshot defaults ───────────────────────

    #[test]
    fn success_rate_snapshot_default_values() {
        let snapshot = RouterSuccessRateSnapshot::default();
        assert!(snapshot.recent_successes.is_empty());
        // #[derive(Default)] yields f32 default (0.0);
        // serde(default = "default_success_rate") only applies during deserialization.
        assert!((snapshot.success_rate - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn success_rate_snapshot_serde_default() {
        // When deserialized from empty JSON, serde applies default_success_rate() = 1.0
        let snapshot: RouterSuccessRateSnapshot = serde_json::from_str("{}").expect("test: parse empty JSON");
        assert!((snapshot.success_rate - 1.0).abs() < f32::EPSILON);
    }

    // ── append_success_event + load_recent_successes with NoneMemory ─

    #[tokio::test]
    async fn append_success_event_with_none_memory_does_not_panic() {
        // NoneMemory silently discards stores — we just verify no panic
        let memory = NoneMemory;
        let result = append_success_event(&memory, "test-model", true).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn load_recent_successes_empty_memory_returns_empty() {
        let memory = NoneMemory;
        let successes = load_recent_successes(&memory, "test-model").await;
        assert!(successes.is_empty());
    }
}
