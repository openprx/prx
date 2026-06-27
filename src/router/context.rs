use crate::config::{AgentCompactionConfig, ModelRouteConfig, RouterConfig, RouterModelConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWindowSource {
    AgentCompactionOverride,
    RouterModelConfig,
    RouterBuiltin,
    FallbackDefault,
}

#[derive(Debug, Clone)]
pub struct EffectiveCompactionConfig {
    pub config: AgentCompactionConfig,
    pub max_context_source: ContextWindowSource,
    pub model_context_tokens: Option<usize>,
    pub selected_provider: String,
    pub selected_model: String,
}

pub fn resolve_effective_compaction_config(
    base: &AgentCompactionConfig,
    provider: &str,
    model: &str,
    router: &RouterConfig,
    model_routes: &[ModelRouteConfig],
) -> EffectiveCompactionConfig {
    let (selected_provider, selected_model) = resolve_selected_model(provider, model, model_routes);

    if base.max_context_tokens_explicit {
        return resolved(
            base.clone(),
            ContextWindowSource::AgentCompactionOverride,
            Some(base.max_context_tokens),
            selected_provider,
            selected_model,
        );
    }

    if let Some(model_config) = find_model_config(&router.models, &selected_provider, &selected_model) {
        return with_model_context(
            base,
            model_config.max_context,
            ContextWindowSource::RouterModelConfig,
            selected_provider,
            selected_model,
        );
    }

    let builtin_models = crate::router::models::builtin_model_capabilities();
    if let Some(model_config) = find_model_config(&builtin_models, &selected_provider, &selected_model) {
        return with_model_context(
            base,
            model_config.max_context,
            ContextWindowSource::RouterBuiltin,
            selected_provider,
            selected_model,
        );
    }

    resolved(
        base.clone(),
        ContextWindowSource::FallbackDefault,
        None,
        selected_provider,
        selected_model,
    )
}

pub fn trace_effective_compaction_resolution(resolution: &EffectiveCompactionConfig) {
    tracing::debug!(
        provider = resolution.selected_provider.as_str(),
        model = resolution.selected_model.as_str(),
        max_context_tokens = resolution.config.max_context_tokens,
        source = ?resolution.max_context_source,
        override_honored = matches!(
            resolution.max_context_source,
            ContextWindowSource::AgentCompactionOverride
        ),
        "resolved effective compaction context window"
    );
}

fn with_model_context(
    base: &AgentCompactionConfig,
    max_context_tokens: usize,
    source: ContextWindowSource,
    selected_provider: String,
    selected_model: String,
) -> EffectiveCompactionConfig {
    let mut config = base.clone();
    config.max_context_tokens = max_context_tokens;
    config.max_context_tokens_explicit = false;
    resolved(
        config,
        source,
        Some(max_context_tokens),
        selected_provider,
        selected_model,
    )
}

const fn resolved(
    config: AgentCompactionConfig,
    max_context_source: ContextWindowSource,
    model_context_tokens: Option<usize>,
    selected_provider: String,
    selected_model: String,
) -> EffectiveCompactionConfig {
    EffectiveCompactionConfig {
        config,
        max_context_source,
        model_context_tokens,
        selected_provider,
        selected_model,
    }
}

fn resolve_selected_model(provider: &str, model: &str, model_routes: &[ModelRouteConfig]) -> (String, String) {
    if let Some(hint) = model.strip_prefix("hint:")
        && let Some(route) = model_routes.iter().find(|route| route.hint == hint)
    {
        return (route.provider.clone(), route.model.clone());
    }
    (provider.to_string(), model.to_string())
}

fn find_model_config<'a>(
    models: &'a [RouterModelConfig],
    selected_provider: &str,
    selected_model: &str,
) -> Option<&'a RouterModelConfig> {
    models.iter().find(|model| {
        provider_matches(&model.provider, selected_provider) && model_matches(&model.model_id, selected_model)
    })
}

const fn provider_matches(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn model_matches(model_id: &str, selected_model: &str) -> bool {
    model_id == selected_model
        || selected_model
            .strip_prefix(model_id)
            .is_some_and(|rest| rest.starts_with('/') || rest.starts_with(':'))
        || selected_model
            .rsplit_once('/')
            .is_some_and(|(_, suffix)| suffix == model_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentCompactionConfig, AutomixConfig, RouterModelConfig};

    fn router_with_model(provider: &str, model_id: &str, max_context: usize) -> RouterConfig {
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
            automix: AutomixConfig::default(),
            models: vec![RouterModelConfig {
                model_id: model_id.to_string(),
                provider: provider.to_string(),
                cost_per_million_tokens: 1.0,
                max_context,
                latency_ms: 1_000,
                categories: vec!["code".to_string()],
                elo_rating: 1_000.0,
            }],
        }
    }

    #[test]
    fn resolves_128k_configured_router_model() {
        let router = router_with_model("openrouter", "small", 128_000);
        let result =
            resolve_effective_compaction_config(&AgentCompactionConfig::default(), "openrouter", "small", &router, &[]);
        assert_eq!(result.config.max_context_tokens, 128_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterModelConfig);
        assert_eq!(result.model_context_tokens, Some(128_000));
    }

    #[test]
    fn resolves_1m_configured_router_model() {
        let router = router_with_model("openrouter", "wide", 1_000_000);
        let result =
            resolve_effective_compaction_config(&AgentCompactionConfig::default(), "openrouter", "wide", &router, &[]);
        assert_eq!(result.config.max_context_tokens, 1_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterModelConfig);
        assert_eq!(result.model_context_tokens, Some(1_000_000));
    }

    #[test]
    fn explicit_override_wins_over_1m_model_metadata() {
        let router = router_with_model("openrouter", "wide", 1_000_000);
        let mut base = AgentCompactionConfig {
            max_context_tokens: 128_000,
            ..AgentCompactionConfig::default()
        };
        base.max_context_tokens_explicit = true;
        let result = resolve_effective_compaction_config(&base, "openrouter", "wide", &router, &[]);
        assert_eq!(result.config.max_context_tokens, 128_000);
        assert_eq!(result.max_context_source, ContextWindowSource::AgentCompactionOverride);
        assert_eq!(result.model_context_tokens, Some(128_000));
    }

    #[test]
    fn resolves_hint_route_before_model_lookup() {
        let router = router_with_model("openrouter", "wide", 1_000_000);
        let routes = vec![ModelRouteConfig {
            hint: "reasoning".to_string(),
            provider: "openrouter".to_string(),
            model: "wide".to_string(),
            api_key: None,
        }];
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "openai",
            "hint:reasoning",
            &router,
            &routes,
        );
        assert_eq!(result.selected_provider, "openrouter");
        assert_eq!(result.selected_model, "wide");
        assert_eq!(result.config.max_context_tokens, 1_000_000);
    }

    #[test]
    fn resolves_builtin_model_when_config_models_are_empty() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "openrouter",
            "google/gemini-3-flash",
            &RouterConfig::default(),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 1_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterBuiltin);
    }

    #[test]
    fn resolves_modern_claude_builtin_to_1m() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "anthropic",
            "claude-sonnet-4-6",
            &RouterConfig::default(),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 1_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterBuiltin);
    }

    #[test]
    fn resolves_local_legacy_builtin_conservatively() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "ollama",
            "*",
            &RouterConfig::default(),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 32_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterBuiltin);
    }

    #[test]
    fn falls_back_when_no_model_metadata_matches() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "unknown-provider",
            "unknown-model",
            &RouterConfig::default(),
            &[],
        );
        assert_eq!(
            result.config.max_context_tokens,
            AgentCompactionConfig::default().max_context_tokens
        );
        assert_eq!(result.max_context_source, ContextWindowSource::FallbackDefault);
        assert_eq!(result.model_context_tokens, None);
    }
}
