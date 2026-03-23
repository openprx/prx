pub mod automix;
pub mod capability;
pub mod elo;
pub mod history;
pub mod intent;
pub mod knn;
pub mod models;
pub mod scorer;

use anyhow::Result;
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;

use crate::agent::classifier::TaskIntent;
use crate::config::{ModelRouteConfig, RouterConfig};
use crate::memory::Memory;
use crate::memory::embeddings::EmbeddingProvider;

use self::capability::{
    ModelCapabilityEntry, append_success_event, filter_models_by_providers, is_model_reachable, load_recent_successes,
    reachable_provider_names,
};
use self::elo::update_elo;
use self::history::RouterHistory;
use self::intent::infer_router_intent;
use self::knn::KnnStore;
pub use self::scorer::RouterResult;
use self::scorer::rank_models;

pub struct RouterEngine {
    config: RouterConfig,
    default_provider: String,
    model_routes: Vec<ModelRouteConfig>,
    models: RwLock<Vec<ModelCapabilityEntry>>,
    memory: Arc<dyn Memory>,
    history: Option<RouterHistory>,
}

#[derive(Clone)]
struct OutcomePersistenceSnapshot {
    model_id: String,
    metrics_entry: ModelCapabilityEntry,
}

impl RouterEngine {
    pub async fn new(
        mut config: RouterConfig,
        default_provider: String,
        model_routes: Vec<ModelRouteConfig>,
        memory: Arc<dyn Memory>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Result<Self> {
        let reachable_providers = reachable_provider_names(&default_provider, &model_routes);
        if config.models.is_empty() {
            config.models =
                filter_models_by_providers(self::models::builtin_model_capabilities(), &reachable_providers);
        } else {
            config.models = filter_models_by_providers(config.models.clone(), &reachable_providers);
        }
        if config.enabled && config.models.is_empty() {
            tracing::warn!(
                default_provider = default_provider.as_str(),
                reachable_providers = ?reachable_providers,
                "Router enabled but no reachable models remain after provider filtering; disabling router"
            );
            config.enabled = false;
        }
        let mut models = ModelCapabilityEntry::load_all(&config.models, memory.as_ref()).await;
        models = filter_reachable_entries(models, &default_provider, &model_routes);
        if config.enabled && models.is_empty() {
            tracing::warn!(
                default_provider = default_provider.as_str(),
                "Router enabled but no reachable models remain after defensive post-load filtering; disabling router"
            );
            config.enabled = false;
        }
        let history = if config.knn_enabled {
            if let Some(embedder) = embedder.filter(|embedder| embedder.dimensions() > 0) {
                let store = KnnStore::new(Arc::clone(&memory))?;
                Some(RouterHistory::new(
                    store,
                    embedder,
                    config.knn_k,
                    config.knn_min_records,
                ))
            } else {
                None
            }
        } else {
            None
        };
        Ok(Self {
            config,
            default_provider,
            model_routes,
            models: RwLock::new(models),
            memory,
            history,
        })
    }

    #[allow(clippy::indexing_slicing)]
    #[cfg(test)]
    fn new_with_history(
        config: RouterConfig,
        default_provider: String,
        model_routes: Vec<ModelRouteConfig>,
        memory: Arc<dyn Memory>,
        history: Option<RouterHistory>,
    ) -> Self {
        let reachable_providers = reachable_provider_names(&default_provider, &model_routes);
        let filtered_config = RouterConfig {
            models: if config.models.is_empty() {
                filter_models_by_providers(self::models::builtin_model_capabilities(), &reachable_providers)
            } else {
                filter_models_by_providers(config.models.clone(), &reachable_providers)
            },
            ..config.clone()
        };
        let models =
            futures::executor::block_on(ModelCapabilityEntry::load_all(&filtered_config.models, memory.as_ref()));
        let models = filter_reachable_entries(models, &default_provider, &model_routes);
        Self {
            config: filtered_config,
            default_provider,
            model_routes,
            models: RwLock::new(models),
            memory,
            history,
        }
    }

    pub async fn select_model(&self, message: &str, task_intent: &TaskIntent) -> RouterResult {
        if !self.config.enabled {
            return RouterResult {
                chosen_model: None,
                chosen_provider: None,
                score: 0.0,
                candidates: Vec::new(),
                intent: infer_router_intent(*task_intent, message).category_name().to_string(),
                estimated_tokens: estimate_tokens(message),
            };
        }
        let intent = infer_router_intent(*task_intent, message);
        let estimated_tokens = estimate_tokens(message);
        let similarity_scores = if let Some(history) = &self.history {
            history.similarity_scores(message).await
        } else {
            std::collections::HashMap::new()
        };
        let models = self.models.read();
        let reachable_models: Vec<ModelCapabilityEntry> = models
            .iter()
            .filter(|model| {
                is_model_reachable(
                    &self.default_provider,
                    &self.model_routes,
                    &model.config.provider,
                    &model.config.model_id,
                )
            })
            .cloned()
            .collect();
        drop(models);
        if reachable_models.is_empty() {
            return RouterResult {
                chosen_model: None,
                chosen_provider: None,
                score: 0.0,
                candidates: Vec::new(),
                intent: intent.category_name().to_string(),
                estimated_tokens,
            };
        }
        let result = rank_models(
            &intent,
            estimated_tokens,
            &reachable_models,
            &self.config,
            Some(&similarity_scores),
        );
        let mut result = result;
        if let (Some(provider), Some(model)) = (result.chosen_provider.as_deref(), result.chosen_model.as_deref()) {
            if !is_model_reachable(&self.default_provider, &self.model_routes, provider, model) {
                tracing::warn!(
                    provider,
                    model,
                    "Router selected unreachable model; falling back to default model"
                );
                result.chosen_provider = None;
                result.chosen_model = None;
                result.score = 0.0;
            }
        }

        let candidates: Vec<_> = result
            .candidates
            .iter()
            .filter(|candidate| candidate.filtered_reason.is_none())
            .map(|candidate| {
                serde_json::json!({
                    "model": candidate.model_id,
                    "provider": candidate.provider,
                    "similarity": candidate.similarity_score,
                    "score": candidate.total_score,
                })
            })
            .collect();
        let filtered: Vec<_> = result
            .candidates
            .iter()
            .filter_map(|candidate| {
                candidate.filtered_reason.as_ref().map(|reason| {
                    serde_json::json!({
                        "model": candidate.model_id,
                        "reason": reason,
                        "detail": candidate.filtered_detail,
                    })
                })
            })
            .collect();

        tracing::info!(
            chosen = result.chosen_model.as_deref().unwrap_or(""),
            provider = result.chosen_provider.as_deref().unwrap_or(""),
            score = result.score,
            intent = result.intent.as_str(),
            estimated_tokens = result.estimated_tokens,
            candidates = ?candidates,
            filtered = ?filtered,
            "Router decision"
        );

        result
    }

    pub const fn automix_config(&self) -> Option<&crate::config::AutomixConfig> {
        Some(&self.config.automix)
    }

    pub fn model_cost_per_million_tokens(&self, model_id: &str) -> Option<f32> {
        self.models
            .read()
            .iter()
            .find(|entry| {
                entry.config.model_id == model_id
                    || format!("{}/{}", entry.config.provider, entry.config.model_id) == model_id
            })
            .map(|entry| entry.config.cost_per_million_tokens)
    }

    pub async fn record_outcome(&self, message: &str, model_id: &str, success: bool, latency_ms: u64) -> Result<()> {
        let mut recent_successes = load_recent_successes(self.memory.as_ref(), &normalize_model_id(model_id)).await;
        let snapshot = {
            let mut models = self.models.write();
            apply_outcome_update_locked(&mut models, model_id, success, latency_ms, &mut recent_successes)
        };
        let Some(snapshot) = snapshot else {
            tracing::warn!(model = model_id, "Router outcome ignored for unknown model");
            return Ok(());
        };

        if let Err(err) = append_success_event(self.memory.as_ref(), &snapshot.model_id, success).await {
            tracing::warn!("Router success event append failed: {err}");
        }
        if let Err(err) = snapshot.metrics_entry.save_metrics(self.memory.as_ref()).await {
            tracing::warn!("Router metrics persistence failed: {err}");
        }
        if let Some(history) = &self.history {
            if let Err(err) = history.record_query(message, &snapshot.model_id, success).await {
                tracing::warn!("Router history record failed: {err}");
            }
        }
        tracing::info!(model = model_id, success, latency_ms, "Router outcome recorded");
        Ok(())
    }
}

fn normalize_model_id(model_id: &str) -> String {
    model_id
        .rsplit_once('/')
        .map_or_else(|| model_id.to_string(), |(_, model)| model.to_string())
}

fn filter_reachable_entries(
    entries: Vec<ModelCapabilityEntry>,
    default_provider: &str,
    model_routes: &[ModelRouteConfig],
) -> Vec<ModelCapabilityEntry> {
    entries
        .into_iter()
        .filter(|entry| {
            is_model_reachable(
                default_provider,
                model_routes,
                &entry.config.provider,
                &entry.config.model_id,
            )
        })
        .collect()
}

// SAFETY: chosen_index and other_index are produced by .position()/.enumerate()
// on the same `models` slice, so they are always valid indices.
#[allow(clippy::indexing_slicing)]
fn apply_outcome_update_locked(
    models: &mut [ModelCapabilityEntry],
    model_id: &str,
    success: bool,
    latency_ms: u64,
    recent_successes: &mut VecDeque<bool>,
) -> Option<OutcomePersistenceSnapshot> {
    let chosen_index = models.iter().position(|model| {
        model.config.model_id == model_id || format!("{}/{}", model.config.provider, model.config.model_id) == model_id
    })?;
    let baseline = models
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != chosen_index)
        .max_by(|(_, left), (_, right)| {
            left.dynamic_elo
                .partial_cmp(&right.dynamic_elo)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(index, _)| index);

    recent_successes.push_back(success);
    while recent_successes.len() > 100 {
        recent_successes.pop_front();
    }

    let success_count = recent_successes.iter().filter(|value| **value).count() as f32;
    models[chosen_index].success_rate = if recent_successes.is_empty() {
        1.0
    } else {
        success_count / recent_successes.len() as f32
    };

    let old_latency = models[chosen_index].recent_latency_ms as f32;
    let new_latency = latency_ms.min(u64::from(u32::MAX)) as f32;
    models[chosen_index].recent_latency_ms = (old_latency.mul_add(0.7, new_latency * 0.3)).round() as u32;

    if let Some(other_index) = baseline {
        let baseline_elo = models[other_index].dynamic_elo;
        let updated_chosen_elo = if success {
            update_elo(models[chosen_index].dynamic_elo, baseline_elo).0
        } else {
            update_elo(baseline_elo, models[chosen_index].dynamic_elo).1
        };
        models[chosen_index].dynamic_elo = updated_chosen_elo;
    }

    let metrics_entry = models[chosen_index].clone();
    Some(OutcomePersistenceSnapshot {
        model_id: metrics_entry.config.model_id.clone(),
        metrics_entry,
    })
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 3 + 100
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RouterConfig, RouterModelConfig};
    use crate::memory::embeddings::EmbeddingProvider;
    use crate::memory::{Memory, MemoryCategory, MemoryEntry};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct TestMemory {
        entries: Mutex<HashMap<String, MemoryEntry>>,
    }

    #[async_trait]
    impl Memory for TestMemory {
        fn name(&self) -> &str {
            "test"
        }

        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> Result<()> {
            self.entries.lock().unwrap().insert(
                key.to_string(),
                MemoryEntry {
                    id: key.to_string(),
                    key: key.to_string(),
                    content: content.to_string(),
                    category,
                    timestamp: "2026-03-10T00:00:00Z".to_string(),
                    session_id: session_id.map(str::to_string),
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: Some(0),
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                },
            );
            Ok(())
        }

        async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
            Ok(self.entries.lock().unwrap().get(key).cloned())
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            Ok(self.entries.lock().unwrap().values().cloned().collect())
        }

        async fn forget(&self, key: &str) -> Result<bool> {
            Ok(self.entries.lock().unwrap().remove(key).is_some())
        }

        async fn count(&self) -> Result<usize> {
            Ok(self.entries.lock().unwrap().len())
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    fn router_config() -> RouterConfig {
        RouterConfig {
            enabled: true,
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 0.1,
            epsilon: 0.1,
            knn_enabled: false,
            knn_min_records: 10,
            knn_k: 7,
            automix: crate::config::AutomixConfig::default(),
            models: vec![
                RouterModelConfig {
                    model_id: "model-a".to_string(),
                    provider: "openai".to_string(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    latency_ms: 1_000,
                    categories: vec!["analysis".to_string()],
                    elo_rating: 1_000.0,
                },
                RouterModelConfig {
                    model_id: "model-b".to_string(),
                    provider: "openai".to_string(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    latency_ms: 1_000,
                    categories: vec!["analysis".to_string()],
                    elo_rating: 1_000.0,
                },
            ],
        }
    }

    #[derive(Clone)]
    struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_record_outcome_updates_elo() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let router = RouterEngine::new(router_config(), "openai".into(), Vec::new(), Arc::clone(&memory), None)
            .await
            .unwrap();

        router
            .record_outcome("hello analysis", "model-a", true, 1_600)
            .await
            .unwrap();

        let elo = memory.get("router/elo/model-a").await.unwrap().expect("elo persisted");
        assert_eq!(
            elo.session_id.as_deref(),
            Some(crate::self_system::SELF_SYSTEM_SESSION_ID)
        );
        let elo_snapshot: serde_json::Value = serde_json::from_str(&elo.content).unwrap();
        let updated_elo = elo_snapshot["dynamic_elo"].as_f64().unwrap() as f32;
        assert!(updated_elo > 1_000.0);

        let success_events: Vec<_> = memory
            .list(
                Some(&MemoryCategory::Custom("router".into())),
                Some(crate::self_system::SELF_SYSTEM_SESSION_ID),
            )
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| entry.key.starts_with("router/success/model-a/"))
            .collect();
        assert_eq!(success_events.len(), 1);

        let latency = memory
            .get("router/latency/model-a")
            .await
            .unwrap()
            .expect("latency persisted");
        assert_eq!(
            latency.session_id.as_deref(),
            Some(crate::self_system::SELF_SYSTEM_SESSION_ID)
        );
        let latency_snapshot: serde_json::Value = serde_json::from_str(&latency.content).unwrap();
        assert_eq!(latency_snapshot["recent_latency_ms"].as_u64().unwrap(), 1_180);
    }

    #[test]
    fn test_record_outcome_no_await_under_write_lock() {
        let mut models = vec![
            ModelCapabilityEntry {
                config: RouterModelConfig {
                    model_id: "model-a".to_string(),
                    provider: "openai".to_string(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    latency_ms: 1_000,
                    categories: vec!["analysis".to_string()],
                    elo_rating: 1_000.0,
                },
                dynamic_elo: 1_000.0,
                success_rate: 0.5,
                recent_latency_ms: 1_000,
            },
            ModelCapabilityEntry {
                config: RouterModelConfig {
                    model_id: "model-b".to_string(),
                    provider: "openai".to_string(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    latency_ms: 1_000,
                    categories: vec!["analysis".to_string()],
                    elo_rating: 1_050.0,
                },
                dynamic_elo: 1_050.0,
                success_rate: 0.8,
                recent_latency_ms: 900,
            },
        ];
        let mut recent_successes = VecDeque::from([true, false, true]);

        let snapshot = apply_outcome_update_locked(&mut models, "model-a", true, 1_600, &mut recent_successes);

        assert!(snapshot.is_some());
        let snapshot = snapshot.unwrap();
        assert_eq!(snapshot.model_id, "model-a");
        assert_eq!(recent_successes.len(), 4);
        assert!(models[0].dynamic_elo > 1_000.0);
        assert_eq!(models[0].recent_latency_ms, 1_180);
    }

    #[tokio::test]
    async fn test_success_rate_window() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let router = RouterEngine::new(router_config(), "openai".into(), Vec::new(), Arc::clone(&memory), None)
            .await
            .unwrap();

        for _ in 0..100 {
            router
                .record_outcome("hello analysis", "model-a", true, 1_000)
                .await
                .unwrap();
        }
        router
            .record_outcome("hello analysis", "model-a", false, 1_000)
            .await
            .unwrap();

        let recent_successes = load_recent_successes(memory.as_ref(), "model-a").await;
        assert_eq!(recent_successes.len(), 100);
        assert_eq!(recent_successes.front().copied(), Some(true));
        assert_eq!(recent_successes.back().copied(), Some(false));
    }

    #[tokio::test]
    async fn test_latency_ema() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let router = RouterEngine::new(router_config(), "openai".into(), Vec::new(), Arc::clone(&memory), None)
            .await
            .unwrap();

        router
            .record_outcome("hello analysis", "model-a", true, 1_600)
            .await
            .unwrap();
        router
            .record_outcome("hello analysis", "model-a", true, 2_000)
            .await
            .unwrap();

        let latency = memory
            .get("router/latency/model-a")
            .await
            .unwrap()
            .expect("latency persisted");
        let latency_snapshot: serde_json::Value = serde_json::from_str(&latency.content).unwrap();
        assert_eq!(latency_snapshot["recent_latency_ms"].as_u64().unwrap(), 1_426);
    }

    struct FixedEmbeddingProvider;

    #[async_trait]
    impl EmbeddingProvider for FixedEmbeddingProvider {
        fn name(&self) -> &str {
            "test-fixed"
        }

        fn dimensions(&self) -> usize {
            3
        }

        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0_f32, 0.0_f32, 0.0_f32]).collect())
        }
    }

    struct SlowEmbeddingProvider;

    #[async_trait]
    impl EmbeddingProvider for SlowEmbeddingProvider {
        fn name(&self) -> &str {
            "test-slow"
        }

        fn dimensions(&self) -> usize {
            3
        }

        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(texts.iter().map(|_| vec![1.0_f32, 0.0_f32, 0.0_f32]).collect())
        }
    }

    #[tokio::test]
    async fn test_knn_cold_start() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let store = KnnStore::new(Arc::clone(&memory)).unwrap();
        let history = RouterHistory::new(store, Arc::new(FixedEmbeddingProvider), 7, 10);

        for index in 0..9 {
            history
                .record_query("same query", &format!("model-{index}"), true)
                .await
                .unwrap();
        }

        assert_eq!(history.similarity_score("same query", "model-1").await, 0.0);
    }

    #[test]
    fn test_majority_vote() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let store = KnnStore::new(memory).unwrap();
        let voted = store.majority_vote(&[
            ("model-a".to_string(), 0.1),
            ("model-a".to_string(), 0.2),
            ("model-b".to_string(), 0.3),
        ]);

        assert_eq!(voted.map(|value| value.0), Some("model-a".to_string()));
    }

    #[test]
    fn test_distance_weighted_vote() {
        let neighbors = vec![
            ("model-a".to_string(), 0.01),
            ("model-b".to_string(), 0.20),
            ("model-b".to_string(), 0.30),
        ];

        let model_a_score = super::knn::weighted_model_score(&neighbors, "model-a");
        let model_b_score = super::knn::weighted_model_score(&neighbors, "model-b");

        assert!(model_a_score > model_b_score);
    }

    #[tokio::test]
    async fn test_knn_timeout_fallback() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let store = KnnStore::new(Arc::clone(&memory)).unwrap();
        let history =
            RouterHistory::new(store, Arc::new(SlowEmbeddingProvider), 7, 10).with_timeout(Duration::from_millis(10));
        let mut config = router_config();
        config.alpha = 1.0;
        config.knn_enabled = true;
        let router =
            RouterEngine::new_with_history(config, "openai".into(), Vec::new(), Arc::clone(&memory), Some(history));

        let result = router.select_model("same query", &TaskIntent::Simple).await;

        assert!(result.chosen_model.is_some());
        assert!(
            result
                .candidates
                .iter()
                .all(|candidate| candidate.similarity_score == 0.0)
        );
    }

    #[tokio::test]
    async fn test_load_all_keeps_only_reachable_models() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let config = RouterConfig {
            enabled: true,
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 0.1,
            epsilon: 0.1,
            knn_enabled: false,
            knn_min_records: 10,
            knn_k: 7,
            automix: crate::config::AutomixConfig::default(),
            models: vec![
                RouterModelConfig {
                    model_id: "claude-sonnet".into(),
                    provider: "anthropic".into(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    latency_ms: 1_000,
                    categories: vec!["analysis".into()],
                    elo_rating: 1_100.0,
                },
                RouterModelConfig {
                    model_id: "gpt-4.1".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    latency_ms: 1_000,
                    categories: vec!["analysis".into()],
                    elo_rating: 1_500.0,
                },
            ],
        };
        let router = RouterEngine::new(config, "anthropic".into(), Vec::new(), Arc::clone(&memory), None)
            .await
            .unwrap();

        let models = router.models.read();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].config.provider, "anthropic");
    }

    #[tokio::test]
    async fn test_router_auto_disable_when_no_reachable_models() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let mut config = router_config();
        config.models = vec![RouterModelConfig {
            model_id: "gpt-4.1".into(),
            provider: "openai".into(),
            cost_per_million_tokens: 1.0,
            max_context: 128_000,
            latency_ms: 1_000,
            categories: vec!["analysis".into()],
            elo_rating: 1_000.0,
        }];

        let logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer({
                let logs = Arc::clone(&logs);
                move || SharedLogWriter(Arc::clone(&logs))
            })
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let router = RouterEngine::new(config, "anthropic".into(), Vec::new(), Arc::clone(&memory), None)
            .await
            .unwrap();

        assert!(!router.config.enabled);

        let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(output.contains("disabling router"));
        assert!(output.contains("no reachable models remain"));
    }

    #[tokio::test]
    async fn test_select_model_returns_none_for_unreachable() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let config = RouterConfig {
            enabled: true,
            models: vec![RouterModelConfig {
                model_id: "gpt-4.1".into(),
                provider: "openai".into(),
                cost_per_million_tokens: 0.1,
                max_context: 128_000,
                latency_ms: 500,
                categories: vec!["conversation".into()],
                elo_rating: 1_500.0,
            }],
            ..RouterConfig::default()
        };
        let router = RouterEngine {
            config,
            default_provider: "anthropic".into(),
            model_routes: Vec::new(),
            models: RwLock::new(vec![ModelCapabilityEntry {
                config: RouterModelConfig {
                    model_id: "gpt-4.1".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 0.1,
                    max_context: 128_000,
                    latency_ms: 500,
                    categories: vec!["conversation".into()],
                    elo_rating: 1_500.0,
                },
                dynamic_elo: 1_500.0,
                success_rate: 1.0,
                recent_latency_ms: 500,
            }]),
            memory,
            history: None,
        };

        let result = router.select_model("hello", &TaskIntent::Simple).await;

        assert!(result.chosen_model.is_none());
        assert!(result.chosen_provider.is_none());
        assert_eq!(result.score, 0.0);
    }

    #[tokio::test]
    async fn test_select_model_prefers_reachable_candidates() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let config = RouterConfig {
            enabled: true,
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 0.1,
            epsilon: 0.1,
            knn_enabled: false,
            knn_min_records: 10,
            knn_k: 7,
            automix: crate::config::AutomixConfig::default(),
            models: vec![
                RouterModelConfig {
                    model_id: "claude-sonnet".into(),
                    provider: "anthropic".into(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    latency_ms: 1_000,
                    categories: vec!["analysis".into()],
                    elo_rating: 1_000.0,
                },
                RouterModelConfig {
                    model_id: "gpt-4.1".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 1.0,
                    max_context: 128_000,
                    latency_ms: 1_000,
                    categories: vec!["analysis".into()],
                    elo_rating: 2_000.0,
                },
            ],
        };
        let router = RouterEngine::new(config, "anthropic".into(), Vec::new(), Arc::clone(&memory), None)
            .await
            .unwrap();

        let result = router.select_model("hello analysis", &TaskIntent::Simple).await;
        assert_eq!(result.chosen_provider.as_deref(), Some("anthropic"));
        assert_eq!(result.chosen_model.as_deref(), Some("claude-sonnet"));
        assert!(
            result
                .candidates
                .iter()
                .all(|candidate| candidate.provider == "anthropic")
        );
    }

    #[tokio::test]
    async fn test_record_outcome_updates_only_chosen_model_elo() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let config = RouterConfig {
            enabled: true,
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 1.0,
            epsilon: 0.3,
            knn_enabled: false,
            knn_min_records: 10,
            knn_k: 7,
            automix: crate::config::AutomixConfig {
                enabled: true,
                confidence_threshold: 0.7,
                cheap_model_tiers: vec!["cheap".into()],
                premium_model_id: "openai/model-premium".into(),
            },
            models: vec![
                RouterModelConfig {
                    model_id: "model-cheap".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 0.1,
                    max_context: 128_000,
                    latency_ms: 500,
                    categories: vec!["conversation".into()],
                    elo_rating: 1_000.0,
                },
                RouterModelConfig {
                    model_id: "model-premium".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 10.0,
                    max_context: 128_000,
                    latency_ms: 2_000,
                    categories: vec!["conversation".into()],
                    elo_rating: 1_000.0,
                },
            ],
        };
        let router = RouterEngine::new(config, "openai".into(), Vec::new(), Arc::clone(&memory), None)
            .await
            .unwrap();

        router.record_outcome("hello", "model-cheap", true, 500).await.unwrap();

        let cheap_elo = memory
            .get("router/elo/model-cheap")
            .await
            .unwrap()
            .expect("cheap elo persisted");
        let cheap_snapshot: serde_json::Value = serde_json::from_str(&cheap_elo.content).unwrap();
        assert!(cheap_snapshot["dynamic_elo"].as_f64().unwrap() > 1_000.0);

        assert!(memory.get("router/elo/model-premium").await.unwrap().is_none());

        let router_entries = memory
            .list(
                Some(&MemoryCategory::Custom("router".into())),
                Some(crate::self_system::SELF_SYSTEM_SESSION_ID),
            )
            .await
            .unwrap();
        assert!(
            router_entries
                .iter()
                .any(|entry| entry.key.starts_with("router/success/model-cheap/"))
        );
        assert!(
            router_entries
                .iter()
                .all(|entry| !entry.key.starts_with("router/success/model-premium/"))
        );
    }
}
