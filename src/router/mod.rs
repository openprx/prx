pub mod capability;
pub mod elo;
pub mod intent;
pub mod models;
pub mod scorer;

use anyhow::Result;
use parking_lot::RwLock;
use std::sync::Arc;

use crate::agent::classifier::TaskIntent;
use crate::config::RouterConfig;
use crate::memory::Memory;

use self::capability::{load_recent_successes, ModelCapabilityEntry};
use self::elo::update_elo;
use self::intent::infer_router_intent;
use self::scorer::rank_models;
pub use self::scorer::RouterResult;

pub struct RouterEngine {
    config: RouterConfig,
    models: RwLock<Vec<ModelCapabilityEntry>>,
    memory: Arc<dyn Memory>,
}

impl RouterEngine {
    pub async fn new(config: RouterConfig, memory: Arc<dyn Memory>) -> Result<Self> {
        let models = ModelCapabilityEntry::load_all(&config, memory.as_ref()).await;
        Ok(Self {
            config,
            models: RwLock::new(models),
            memory,
        })
    }

    pub async fn select_model(&self, message: &str, task_intent: &TaskIntent) -> RouterResult {
        let intent = infer_router_intent(task_intent, message);
        let estimated_tokens = estimate_tokens(message);
        let models = self.models.read();
        let result = rank_models(&intent, estimated_tokens, &models, &self.config);

        let candidates: Vec<_> = result
            .candidates
            .iter()
            .filter(|candidate| candidate.filtered_reason.is_none())
            .map(|candidate| {
                serde_json::json!({
                    "model": candidate.model_id,
                    "provider": candidate.provider,
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
                        "provider": candidate.provider,
                        "reason": reason,
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

    pub async fn record_outcome(
        &self,
        model_id: &str,
        success: bool,
        latency_ms: u64,
    ) -> Result<()> {
        let mut models = self.models.write();
        let Some(chosen_index) = models.iter().position(|model| {
            model.config.model_id == model_id
                || format!("{}/{}", model.config.provider, model.config.model_id) == model_id
        }) else {
            return Ok(());
        };

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

        let mut recent_successes =
            load_recent_successes(self.memory.as_ref(), &models[chosen_index].config.model_id)
                .await;
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
        models[chosen_index].recent_latency_ms =
            (old_latency.mul_add(0.7, new_latency * 0.3)).round() as u32;

        if let Some(other_index) = baseline {
            let (winner_elo, loser_elo) = if success {
                update_elo(
                    models[chosen_index].dynamic_elo,
                    models[other_index].dynamic_elo,
                )
            } else {
                let (other, chosen) = update_elo(
                    models[other_index].dynamic_elo,
                    models[chosen_index].dynamic_elo,
                );
                (chosen, other)
            };
            models[chosen_index].dynamic_elo = winner_elo;
            models[other_index].dynamic_elo = loser_elo;
            let mut other_recent_successes =
                load_recent_successes(self.memory.as_ref(), &models[other_index].config.model_id)
                    .await;
            models[other_index]
                .save_metrics(
                    self.memory.as_ref(),
                    other_recent_successes.make_contiguous(),
                )
                .await?;
        }

        models[chosen_index]
            .save_metrics(self.memory.as_ref(), recent_successes.make_contiguous())
            .await?;
        tracing::info!(
            model = model_id,
            success,
            latency_ms,
            "Router outcome recorded"
        );
        Ok(())
    }
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 3 + 100
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RouterConfig, RouterModelConfig};
    use crate::memory::{Memory, MemoryCategory, MemoryEntry};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

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

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
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

    #[tokio::test]
    async fn test_record_outcome_updates_elo() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let router = RouterEngine::new(router_config(), Arc::clone(&memory))
            .await
            .unwrap();

        router.record_outcome("model-a", true, 1_600).await.unwrap();

        let elo = memory
            .get("router/elo/model-a")
            .await
            .unwrap()
            .expect("elo persisted");
        let elo_snapshot: serde_json::Value = serde_json::from_str(&elo.content).unwrap();
        let updated_elo = elo_snapshot["dynamic_elo"].as_f64().unwrap() as f32;
        assert!(updated_elo > 1_000.0);

        let stats = memory
            .get("router/success_rate/model-a")
            .await
            .unwrap()
            .expect("success_rate persisted");
        let stats_snapshot: serde_json::Value =
            serde_json::from_str(&stats.content).unwrap();
        assert_eq!(
            stats_snapshot["recent_successes"]
                .as_array()
                .expect("success window")
                .len(),
            1
        );
        assert_eq!(stats_snapshot["success_rate"].as_f64().unwrap(), 1.0);

        let latency = memory
            .get("router/latency/model-a")
            .await
            .unwrap()
            .expect("latency persisted");
        let latency_snapshot: serde_json::Value = serde_json::from_str(&latency.content).unwrap();
        assert_eq!(
            latency_snapshot["recent_latency_ms"].as_u64().unwrap(),
            1_180
        );
    }

    #[tokio::test]
    async fn test_success_rate_window() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let router = RouterEngine::new(router_config(), Arc::clone(&memory))
            .await
            .unwrap();

        for _ in 0..100 {
            router.record_outcome("model-a", true, 1_000).await.unwrap();
        }
        router.record_outcome("model-a", false, 1_000).await.unwrap();

        let stats = memory
            .get("router/success_rate/model-a")
            .await
            .unwrap()
            .expect("success_rate persisted");
        let stats_snapshot: serde_json::Value = serde_json::from_str(&stats.content).unwrap();
        let recent_successes = stats_snapshot["recent_successes"]
            .as_array()
            .expect("success window");

        assert_eq!(recent_successes.len(), 100);
        assert_eq!(recent_successes[0].as_bool(), Some(true));
        assert_eq!(recent_successes[99].as_bool(), Some(false));
        assert_eq!(stats_snapshot["success_rate"].as_f64().unwrap(), 0.99);
    }

    #[tokio::test]
    async fn test_latency_ema() {
        let memory: Arc<dyn Memory> = Arc::new(TestMemory::default());
        let router = RouterEngine::new(router_config(), Arc::clone(&memory))
            .await
            .unwrap();

        router.record_outcome("model-a", true, 1_600).await.unwrap();
        router.record_outcome("model-a", true, 2_000).await.unwrap();

        let latency = memory
            .get("router/latency/model-a")
            .await
            .unwrap()
            .expect("latency persisted");
        let latency_snapshot: serde_json::Value = serde_json::from_str(&latency.content).unwrap();
        assert_eq!(
            latency_snapshot["recent_latency_ms"].as_u64().unwrap(),
            1_426
        );
    }
}
