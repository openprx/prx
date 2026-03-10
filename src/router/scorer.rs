use crate::config::RouterConfig;
use crate::router::capability::ModelCapabilityEntry;
use crate::router::intent::RouterIntent;

#[derive(Debug, Clone)]
pub struct ModelScore {
    pub model_id: String,
    pub provider: String,
    pub capability_score: f32,
    pub elo_score: f32,
    pub cost_penalty: f32,
    pub latency_penalty: f32,
    pub total_score: f32,
    pub filtered_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RouterResult {
    pub chosen_model: Option<String>,
    pub chosen_provider: Option<String>,
    pub score: f32,
    pub candidates: Vec<ModelScore>,
}

fn normalize_elo(elo: f32) -> f32 {
    ((elo - 800.0) / 400.0).clamp(0.0, 1.0)
}

fn capability_score(intent: &RouterIntent, model: &ModelCapabilityEntry) -> f32 {
    if model.config.categories.is_empty() {
        return 0.7;
    }

    if model
        .config
        .categories
        .iter()
        .any(|category| category.eq_ignore_ascii_case(intent.category_name()))
    {
        1.0
    } else {
        0.5
    }
}

pub fn compute_score(
    intent: &RouterIntent,
    estimated_tokens: usize,
    model: &ModelCapabilityEntry,
    config: &RouterConfig,
) -> ModelScore {
    if estimated_tokens > model.config.max_context {
        return ModelScore {
            model_id: model.config.model_id.clone(),
            provider: model.config.provider.clone(),
            capability_score: 0.0,
            elo_score: 0.0,
            cost_penalty: 0.0,
            latency_penalty: 0.0,
            total_score: f32::MIN,
            filtered_reason: Some(format!(
                "estimated tokens {estimated_tokens} exceed max_context {}",
                model.config.max_context
            )),
        };
    }

    let capability = capability_score(intent, model);
    let elo_score = normalize_elo(model.dynamic_elo);
    let cost_penalty = model.config.cost_per_million_tokens * estimated_tokens as f32 / 1_000_000.0;
    let latency_penalty = model.recent_latency_ms.min(5_000) as f32 / 5_000.0;
    let total_score = config.beta * capability + config.gamma * elo_score
        - config.delta * cost_penalty
        - config.epsilon * latency_penalty;

    ModelScore {
        model_id: model.config.model_id.clone(),
        provider: model.config.provider.clone(),
        capability_score: capability,
        elo_score,
        cost_penalty,
        latency_penalty,
        total_score,
        filtered_reason: None,
    }
}

pub fn rank_models(
    intent: &RouterIntent,
    estimated_tokens: usize,
    models: &[ModelCapabilityEntry],
    config: &RouterConfig,
) -> RouterResult {
    let mut candidates: Vec<ModelScore> = models
        .iter()
        .map(|model| compute_score(intent, estimated_tokens, model, config))
        .collect();

    candidates.sort_by(|a, b| {
        b.total_score
            .partial_cmp(&a.total_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let best = candidates
        .iter()
        .find(|candidate| candidate.filtered_reason.is_none());

    RouterResult {
        chosen_model: best.map(|candidate| candidate.model_id.clone()),
        chosen_provider: best.map(|candidate| candidate.provider.clone()),
        score: best.map_or(0.0, |candidate| candidate.total_score),
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RouterConfig, RouterModelConfig};

    fn make_router_config() -> RouterConfig {
        RouterConfig {
            enabled: true,
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 0.1,
            epsilon: 0.1,
            models: Vec::new(),
        }
    }

    fn model(
        model_id: &str,
        provider: &str,
        categories: &[&str],
        cost: f32,
        max_context: usize,
    ) -> ModelCapabilityEntry {
        ModelCapabilityEntry {
            config: RouterModelConfig {
                model_id: model_id.to_string(),
                provider: provider.to_string(),
                cost_per_million_tokens: cost,
                max_context,
                latency_ms: 2_000,
                categories: categories
                    .iter()
                    .map(|value| (*value).to_string())
                    .collect(),
                elo_rating: 1_000.0,
            },
            dynamic_elo: 1_000.0,
            success_rate: 1.0,
            recent_latency_ms: 2_000,
        }
    }

    #[test]
    fn test_capability_score() {
        let config = make_router_config();
        let models = vec![
            model("general", "openrouter", &["conversation"], 1.0, 128_000),
            model("coder", "openrouter", &["code"], 1.0, 128_000),
        ];

        let result = rank_models(&RouterIntent::Code, 500, &models, &config);
        assert_eq!(result.chosen_model.as_deref(), Some("coder"));
    }

    #[test]
    fn test_context_filter() {
        let config = make_router_config();
        let models = vec![
            model("tiny", "openrouter", &["code"], 1.0, 100),
            model("wide", "openrouter", &["code"], 1.0, 10_000),
        ];

        let result = rank_models(&RouterIntent::Code, 500, &models, &config);
        assert_eq!(result.chosen_model.as_deref(), Some("wide"));
        assert!(result.candidates.iter().any(|candidate| {
            candidate.model_id == "tiny" && candidate.filtered_reason.is_some()
        }));
    }

    #[test]
    fn test_cost_preference() {
        let config = make_router_config();
        let models = vec![
            model("expensive", "openrouter", &["analysis"], 50.0, 128_000),
            model("cheap", "openrouter", &["analysis"], 1.0, 128_000),
        ];

        let result = rank_models(&RouterIntent::Analysis, 8_000, &models, &config);
        assert_eq!(result.chosen_model.as_deref(), Some("cheap"));
    }

    #[test]
    fn test_no_models_fallback() {
        let config = make_router_config();
        let result = rank_models(&RouterIntent::Conversation, 100, &[], &config);
        assert!(result.chosen_model.is_none());
        assert!(result.chosen_provider.is_none());
        assert!(result.candidates.is_empty());
    }
}
