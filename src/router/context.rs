use crate::config::{AgentCompactionConfig, ModelRouteConfig, RouterConfig, RouterModelConfig};

pub const KERNEL_SUPPORTED_CONTEXT_TOKENS: usize = 10_000_000;

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
    pub requested_context_tokens: Option<usize>,
    pub kernel_supported_tokens: usize,
    pub kernel_capped: bool,
    pub selected_provider: String,
    pub selected_model: String,
}

#[derive(Debug, Clone)]
pub struct CompactionResolver {
    base: AgentCompactionConfig,
    router: RouterConfig,
    model_routes: Vec<ModelRouteConfig>,
}

impl CompactionResolver {
    pub const fn new(base: AgentCompactionConfig, router: RouterConfig, model_routes: Vec<ModelRouteConfig>) -> Self {
        Self {
            base,
            router,
            model_routes,
        }
    }

    pub fn from_base(base: AgentCompactionConfig) -> Self {
        Self::new(base, RouterConfig::default(), Vec::new())
    }

    pub fn resolve(&self, provider: &str, model: &str) -> EffectiveCompactionConfig {
        resolve_effective_compaction_config(&self.base, provider, model, &self.router, &self.model_routes)
    }
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
        let capped = cap_to_kernel(base.max_context_tokens);
        let mut config = base.clone();
        config.max_context_tokens = capped.effective;
        return resolved(
            config,
            ContextWindowSource::AgentCompactionOverride,
            Some(capped.effective),
            Some(capped.requested),
            capped.kernel_capped,
            selected_provider,
            selected_model,
        );
    }

    if let Some(model_config) = find_model_config(&router.models, &selected_provider, &selected_model) {
        return with_model_context(
            base,
            model_config.max_context,
            model_config.reserved_output_tokens,
            ContextWindowSource::RouterModelConfig,
            selected_provider,
            selected_model,
        );
    }

    #[cfg(feature = "llm-router")]
    {
        let builtin_models = crate::router::models::builtin_model_capabilities();
        if let Some(model_config) = find_model_config(&builtin_models, &selected_provider, &selected_model) {
            return with_model_context(
                base,
                model_config.max_context,
                model_config.reserved_output_tokens,
                ContextWindowSource::RouterBuiltin,
                selected_provider,
                selected_model,
            );
        }
    }

    let capped = cap_to_kernel(base.max_context_tokens);
    let mut config = base.clone();
    config.max_context_tokens = capped.effective;
    resolved(
        config,
        ContextWindowSource::FallbackDefault,
        None,
        Some(capped.requested),
        capped.kernel_capped,
        selected_provider,
        selected_model,
    )
}

pub fn trace_effective_compaction_resolution(resolution: &EffectiveCompactionConfig) {
    tracing::debug!(
        provider = resolution.selected_provider.as_str(),
        model = resolution.selected_model.as_str(),
        max_context_tokens = resolution.config.max_context_tokens,
        requested_context_tokens = resolution.requested_context_tokens,
        kernel_supported_tokens = resolution.kernel_supported_tokens,
        kernel_capped = resolution.kernel_capped,
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
    reserved_output_tokens: Option<usize>,
    source: ContextWindowSource,
    selected_provider: String,
    selected_model: String,
) -> EffectiveCompactionConfig {
    let capped = cap_to_kernel(max_context_tokens);
    let mut config = base.clone();
    config.max_context_tokens = capped.effective;
    if let Some(reserved_output_tokens) = reserved_output_tokens {
        config.reserve_tokens = reserved_output_tokens;
    }
    config.max_context_tokens_explicit = false;
    resolved(
        config,
        source,
        Some(capped.effective),
        Some(capped.requested),
        capped.kernel_capped,
        selected_provider,
        selected_model,
    )
}

struct CappedContext {
    requested: usize,
    effective: usize,
    kernel_capped: bool,
}

const fn cap_to_kernel(tokens: usize) -> CappedContext {
    let effective = if tokens > KERNEL_SUPPORTED_CONTEXT_TOKENS {
        KERNEL_SUPPORTED_CONTEXT_TOKENS
    } else {
        tokens
    };
    CappedContext {
        requested: tokens,
        effective,
        kernel_capped: tokens > KERNEL_SUPPORTED_CONTEXT_TOKENS,
    }
}

const fn resolved(
    config: AgentCompactionConfig,
    max_context_source: ContextWindowSource,
    model_context_tokens: Option<usize>,
    requested_context_tokens: Option<usize>,
    kernel_capped: bool,
    selected_provider: String,
    selected_model: String,
) -> EffectiveCompactionConfig {
    EffectiveCompactionConfig {
        config,
        max_context_source,
        model_context_tokens,
        requested_context_tokens,
        kernel_supported_tokens: KERNEL_SUPPORTED_CONTEXT_TOKENS,
        kernel_capped,
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
        router_with_models(vec![RouterModelConfig {
            model_id: model_id.to_string(),
            provider: provider.to_string(),
            cost_per_million_tokens: 1.0,
            max_context,
            reserved_output_tokens: None,
            latency_ms: 1_000,
            categories: vec!["code".to_string()],
            elo_rating: 1_000.0,
        }])
    }

    fn router_with_models(models: Vec<RouterModelConfig>) -> RouterConfig {
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
            models,
        }
    }

    fn model(provider: &str, model_id: &str, max_context: usize) -> RouterModelConfig {
        RouterModelConfig {
            model_id: model_id.to_string(),
            provider: provider.to_string(),
            cost_per_million_tokens: 1.0,
            max_context,
            reserved_output_tokens: None,
            latency_ms: 1_000,
            categories: vec!["code".to_string()],
            elo_rating: 1_000.0,
        }
    }

    fn model_with_reserved_output(
        provider: &str,
        model_id: &str,
        max_context: usize,
        reserved_output_tokens: usize,
    ) -> RouterModelConfig {
        RouterModelConfig {
            reserved_output_tokens: Some(reserved_output_tokens),
            ..model(provider, model_id, max_context)
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
    fn per_model_reserved_output_tokens_override_global_reserve_literal() {
        let router = router_with_models(vec![model_with_reserved_output(
            "openrouter",
            "output-heavy",
            200_000,
            12_345,
        )]);
        let base = AgentCompactionConfig {
            reserve_tokens: 4_096,
            ..AgentCompactionConfig::default()
        };
        let result = resolve_effective_compaction_config(&base, "openrouter", "output-heavy", &router, &[]);
        assert_eq!(result.config.max_context_tokens, 200_000);
        assert_eq!(result.config.reserve_tokens, 12_345);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterModelConfig);
    }

    #[test]
    fn compaction_resolver_child_route_200k_beats_parent_1m() {
        let router = router_with_models(vec![
            model("anthropic", "claude-opus-4-8", 1_000_000),
            model("openrouter", "small-child", 200_000),
        ]);
        let resolver = CompactionResolver::new(AgentCompactionConfig::default(), router, Vec::new());

        let result = resolver.resolve("openrouter", "small-child");

        assert_eq!(result.config.max_context_tokens, 200_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterModelConfig);
        assert_eq!(result.selected_model, "small-child");
    }

    #[test]
    fn resolves_1m_configured_router_model() {
        let router = router_with_model("openrouter", "wide", 1_000_000);
        let result =
            resolve_effective_compaction_config(&AgentCompactionConfig::default(), "openrouter", "wide", &router, &[]);
        assert_eq!(result.config.max_context_tokens, 1_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterModelConfig);
        assert_eq!(result.model_context_tokens, Some(1_000_000));
        assert_eq!(result.requested_context_tokens, Some(1_000_000));
        assert_eq!(result.kernel_supported_tokens, 10_000_000);
        assert!(!result.kernel_capped);
    }

    #[test]
    fn resolves_10m_configured_router_model() {
        let router = router_with_model("openrouter", "ten-m", 10_000_000);
        let result =
            resolve_effective_compaction_config(&AgentCompactionConfig::default(), "openrouter", "ten-m", &router, &[]);
        assert_eq!(result.config.max_context_tokens, 10_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterModelConfig);
        assert_eq!(result.model_context_tokens, Some(10_000_000));
        assert_eq!(result.requested_context_tokens, Some(10_000_000));
        assert!(!result.kernel_capped);
    }

    #[test]
    fn caps_context_windows_at_kernel_limit() {
        let router = router_with_model("openrouter", "too-wide", 20_000_000);
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "openrouter",
            "too-wide",
            &router,
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 10_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterModelConfig);
        assert_eq!(result.model_context_tokens, Some(10_000_000));
        assert_eq!(result.requested_context_tokens, Some(20_000_000));
        assert_eq!(result.kernel_supported_tokens, 10_000_000);
        assert!(result.kernel_capped);
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
        assert_eq!(result.requested_context_tokens, Some(128_000));
        assert!(!result.kernel_capped);
    }

    #[test]
    fn caps_explicit_override_at_kernel_limit_preserving_source() {
        let router = router_with_model("openrouter", "small", 128_000);
        let mut base = AgentCompactionConfig {
            max_context_tokens: 20_000_000,
            ..AgentCompactionConfig::default()
        };
        base.max_context_tokens_explicit = true;
        let result = resolve_effective_compaction_config(&base, "openrouter", "small", &router, &[]);
        assert_eq!(result.config.max_context_tokens, 10_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::AgentCompactionOverride);
        assert_eq!(result.model_context_tokens, Some(10_000_000));
        assert_eq!(result.requested_context_tokens, Some(20_000_000));
        assert_eq!(result.kernel_supported_tokens, 10_000_000);
        assert!(result.kernel_capped);
    }

    #[test]
    fn compaction_resolver_explicit_override_wins_and_caps_at_literal_10m() {
        let router = router_with_model("openrouter", "small-child", 200_000);
        let mut base = AgentCompactionConfig {
            max_context_tokens: 20_000_000,
            ..AgentCompactionConfig::default()
        };
        base.max_context_tokens_explicit = true;
        let resolver = CompactionResolver::new(base, router, Vec::new());

        let result = resolver.resolve("openrouter", "small-child");

        assert_eq!(result.config.max_context_tokens, 10_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::AgentCompactionOverride);
        assert_eq!(result.requested_context_tokens, Some(20_000_000));
        assert!(result.kernel_capped);
    }

    #[test]
    fn compaction_resolver_unknown_child_falls_back_to_literal_128k_not_parent() {
        let router = router_with_models(vec![model("anthropic", "claude-opus-4-8", 1_000_000)]);
        let resolver = CompactionResolver::new(AgentCompactionConfig::default(), router, Vec::new());

        let result = resolver.resolve("unknown-provider", "unknown-child");

        assert_eq!(result.config.max_context_tokens, 128_000);
        assert_eq!(result.max_context_source, ContextWindowSource::FallbackDefault);
        assert_eq!(result.model_context_tokens, None);
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
            "claude-opus-4-8",
            &RouterConfig::default(),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 1_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterBuiltin);
    }

    #[test]
    fn resolves_current_haiku_builtin_conservatively() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "anthropic",
            "claude-haiku-4-5",
            &RouterConfig::default(),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 200_000);
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
        assert_eq!(result.config.max_context_tokens, 128_000);
        assert_eq!(result.max_context_source, ContextWindowSource::FallbackDefault);
        assert_eq!(result.model_context_tokens, None);
        assert_eq!(result.requested_context_tokens, Some(128_000));
        assert!(!result.kernel_capped);
    }

    #[test]
    fn fallback_default_still_stays_below_kernel_limit() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "unknown-provider",
            "unknown-model",
            &RouterConfig::default(),
            &[],
        );
        assert!(result.config.max_context_tokens <= KERNEL_SUPPORTED_CONTEXT_TOKENS);
    }

    #[test]
    fn caps_fallback_default_at_kernel_limit() {
        let base = AgentCompactionConfig {
            max_context_tokens: 20_000_000,
            max_context_tokens_explicit: false,
            ..AgentCompactionConfig::default()
        };
        let result = resolve_effective_compaction_config(
            &base,
            "unknown-provider",
            "unknown-model",
            &RouterConfig::default(),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 10_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::FallbackDefault);
        assert_eq!(result.model_context_tokens, None);
        assert_eq!(result.requested_context_tokens, Some(20_000_000));
        assert!(result.kernel_capped);
    }
}
