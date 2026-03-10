pub mod capability;
pub mod elo;
pub mod intent;
pub mod scorer;

use anyhow::Result;
use parking_lot::RwLock;
use std::sync::Arc;

use crate::agent::classifier::TaskIntent;
use crate::config::RouterConfig;
use crate::memory::Memory;

use self::capability::ModelCapabilityEntry;
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
        rank_models(&intent, estimated_tokens, &models, &self.config)
    }

    pub async fn record_outcome(
        &self,
        chosen_model: &str,
        success: bool,
        latency_ms: u64,
    ) -> Result<()> {
        let mut models = self.models.write();
        let Some(chosen_index) = models.iter().position(|model| {
            model.config.model_id == chosen_model
                || format!("{}/{}", model.config.provider, model.config.model_id) == chosen_model
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

        let old_latency = models[chosen_index].recent_latency_ms;
        models[chosen_index].recent_latency_ms =
            ((u64::from(old_latency) + latency_ms) / 2).min(u64::from(u32::MAX)) as u32;
        models[chosen_index].success_rate = if success {
            (models[chosen_index].success_rate * 99.0 + 1.0) / 100.0
        } else {
            (models[chosen_index].success_rate * 99.0) / 100.0
        };

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
            models[other_index]
                .save_metrics(self.memory.as_ref())
                .await?;
        }

        models[chosen_index]
            .save_metrics(self.memory.as_ref())
            .await?;
        Ok(())
    }
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 3 + 100
}
