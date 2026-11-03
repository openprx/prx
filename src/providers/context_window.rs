use crate::config::{
    AgentCompactionConfig, Config, ModelRouteConfig, RouterModelConfig, model_settings::ModelSettings,
};
use crate::providers::catalog::CatalogView;
use crate::security::redaction::redact_provider_text;
use std::collections::BTreeMap;
use std::sync::Once;

/// Operator windows and legacy router metadata borrowed from one configuration.
#[derive(Clone, Copy)]
pub struct ContextWindowSources<'a> {
    pub model_settings: &'a BTreeMap<String, ModelSettings>,
    pub legacy_router_models: &'a [RouterModelConfig],
}

impl<'a> ContextWindowSources<'a> {
    pub fn from_config(config: &'a Config) -> Self {
        Self {
            model_settings: &config.model_settings,
            legacy_router_models: &config.router.models,
        }
    }
}

/// Internal lookup key; render it only through the provider-text redactor.
pub fn model_settings_window_key(catalog: &CatalogView, provider: &str, model: &str) -> String {
    let identity = crate::providers::ProviderId::of(provider);
    if let Ok(release) = catalog.release_catalog()
        && let Some(row) = release.model(&identity, model)
    {
        return row.id.clone();
    }
    format!(
        "{}/{}",
        identity.catalog_name().unwrap_or_else(|| provider.trim()),
        model
    )
}

/// Migration guidance without changing legacy metadata or printing credentials.
pub fn legacy_router_window_warning(config: &Config) -> Option<String> {
    if config.router.models.is_empty() {
        return None;
    }
    let catalog = CatalogView::release_only();
    let rows = config
        .router
        .models
        .iter()
        .map(|row| {
            let label = redact_provider_text(&format!("{}/{}", row.provider, row.model_id));
            let key = redact_provider_text(&model_settings_window_key(&catalog, &row.provider, &row.model_id));
            format!("{label} -> [model_settings.\"{key}\"]")
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "router.models[].max_context/reserved_output_tokens are deprecated as context-window sources; move them to [model_settings.\"<key>\"] context_window/reserved_output_tokens (rows: {rows})"
    ))
}

static LEGACY_WINDOW_WARNING: Once = Once::new();

pub(crate) fn warn_legacy_router_windows(config: &Config) {
    emit_legacy_router_window_warning(config, &LEGACY_WINDOW_WARNING);
}

fn emit_legacy_router_window_warning(config: &Config, once: &Once) {
    if let Some(warning) = legacy_router_window_warning(config) {
        once.call_once(|| tracing::warn!(%warning, "legacy context-window configuration"));
    }
}

pub const KERNEL_SUPPORTED_CONTEXT_TOKENS: usize = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWindowSource {
    AgentCompactionOverride,
    ModelSettings,
    RouterModelConfig,
    Catalog,
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

impl EffectiveCompactionConfig {
    /// The selected provider for logs and traces, with userinfo, URL query
    /// values, fragments and recognised secrets redacted. Lookups use the raw
    /// `selected_provider` value.
    #[must_use]
    pub fn shown_provider(&self) -> std::borrow::Cow<'_, str> {
        let userinfo_free = crate::providers::redact_endpoint_userinfo(&self.selected_provider);
        std::borrow::Cow::Owned(redact_provider_text(&userinfo_free))
    }
}

pub fn resolve_effective_compaction_config(
    base: &AgentCompactionConfig,
    provider: &str,
    model: &str,
    sources: ContextWindowSources<'_>,
    model_routes: &[ModelRouteConfig],
) -> EffectiveCompactionConfig {
    resolve_effective_compaction_config_with_catalog(
        base,
        provider,
        model,
        sources,
        model_routes,
        &crate::providers::catalog::CatalogView::release_only(),
    )
}

pub fn resolve_effective_compaction_config_with_catalog(
    base: &AgentCompactionConfig,
    provider: &str,
    model: &str,
    sources: ContextWindowSources<'_>,
    model_routes: &[ModelRouteConfig],
    catalog: &crate::providers::catalog::CatalogView,
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

    let key = model_settings_window_key(catalog, &selected_provider, &selected_model);
    if let Some(settings) = sources.model_settings.get(&key)
        && let Some(window) = settings.context_window
    {
        return with_model_context(
            base,
            usize::try_from(window).unwrap_or(usize::MAX),
            settings
                .reserved_output_tokens
                .map(|tokens| usize::try_from(tokens).unwrap_or(usize::MAX)),
            ContextWindowSource::ModelSettings,
            selected_provider,
            selected_model,
        );
    }

    if let Some(model_config) = find_model_config(sources.legacy_router_models, &selected_provider, &selected_model) {
        return with_model_context(
            base,
            model_config.max_context,
            model_config.reserved_output_tokens,
            ContextWindowSource::RouterModelConfig,
            selected_provider,
            selected_model,
        );
    }

    let descriptor = catalog.describe(&crate::providers::ProviderId::of(&selected_provider), &selected_model);
    // Retired models retain their published limits for existing selections.
    // Views without release evidence keep the existing conservative fallback.
    let context_window = descriptor.context_window.or_else(|| {
        catalog
            .release_catalog()
            .ok()
            .and_then(|release| release.model(&crate::providers::ProviderId::of(&selected_provider), &selected_model))
            .and_then(|model| model.limits.context_window)
    });
    if let Some(context_window) = context_window {
        return with_model_context(
            base,
            usize::try_from(context_window).unwrap_or(usize::MAX),
            None,
            ContextWindowSource::Catalog,
            selected_provider,
            selected_model,
        );
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
        provider = %resolution.shown_provider(),
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
    use crate::config::{AgentCompactionConfig, RouterConfig, RouterModelConfig};

    fn sources(router: &RouterConfig) -> ContextWindowSources<'_> {
        static EMPTY: BTreeMap<String, ModelSettings> = BTreeMap::new();
        ContextWindowSources {
            model_settings: &EMPTY,
            legacy_router_models: &router.models,
        }
    }

    /// INT-2B B4f M-1: the value the per-turn compaction line prints for the
    /// selected provider carries no URL userinfo; the provider itself is kept.
    #[test]
    fn the_shown_provider_redacts_url_userinfo() {
        let label = "custom:http://u:CONTEXT-SECRET-pw@127.0.0.1:1/v1";
        let resolution = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            label,
            "m",
            sources(&RouterConfig::default()),
            &[],
        );
        assert_eq!(resolution.selected_provider, label);
        assert_eq!(resolution.shown_provider(), "custom:http://[redacted]@127.0.0.1:1/v1");
    }

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
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 0.1,
            epsilon: 0.1,
            knn_min_records: 10,
            knn_k: 7,
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
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "openrouter",
            "small",
            sources(&router),
            &[],
        );
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
        let result = resolve_effective_compaction_config(&base, "openrouter", "output-heavy", sources(&router), &[]);
        assert_eq!(result.config.max_context_tokens, 200_000);
        assert_eq!(result.config.reserve_tokens, 12_345);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterModelConfig);
    }

    #[test]
    fn effective_compaction_child_route_200k_beats_parent_1m() {
        let router = router_with_models(vec![
            model("anthropic", "claude-opus-4-8", 1_000_000),
            model("openrouter", "small-child", 200_000),
        ]);
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "openrouter",
            "small-child",
            sources(&router),
            &[],
        );

        assert_eq!(result.config.max_context_tokens, 200_000);
        assert_eq!(result.max_context_source, ContextWindowSource::RouterModelConfig);
        assert_eq!(result.selected_model, "small-child");
    }

    #[test]
    fn resolves_1m_configured_router_model() {
        let router = router_with_model("openrouter", "wide", 1_000_000);
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "openrouter",
            "wide",
            sources(&router),
            &[],
        );
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
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "openrouter",
            "ten-m",
            sources(&router),
            &[],
        );
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
            sources(&router),
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
        let result = resolve_effective_compaction_config(&base, "openrouter", "wide", sources(&router), &[]);
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
        let result = resolve_effective_compaction_config(&base, "openrouter", "small", sources(&router), &[]);
        assert_eq!(result.config.max_context_tokens, 10_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::AgentCompactionOverride);
        assert_eq!(result.model_context_tokens, Some(10_000_000));
        assert_eq!(result.requested_context_tokens, Some(20_000_000));
        assert_eq!(result.kernel_supported_tokens, 10_000_000);
        assert!(result.kernel_capped);
    }

    #[test]
    fn effective_compaction_explicit_override_wins_and_caps_at_literal_10m() {
        let router = router_with_model("openrouter", "small-child", 200_000);
        let mut base = AgentCompactionConfig {
            max_context_tokens: 20_000_000,
            ..AgentCompactionConfig::default()
        };
        base.max_context_tokens_explicit = true;
        let result = resolve_effective_compaction_config(&base, "openrouter", "small-child", sources(&router), &[]);

        assert_eq!(result.config.max_context_tokens, 10_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::AgentCompactionOverride);
        assert_eq!(result.requested_context_tokens, Some(20_000_000));
        assert!(result.kernel_capped);
    }

    #[test]
    fn effective_compaction_unknown_child_falls_back_to_literal_128k_not_parent() {
        let router = router_with_models(vec![model("anthropic", "claude-opus-4-8", 1_000_000)]);
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "unknown-provider",
            "unknown-child",
            sources(&router),
            &[],
        );

        assert_eq!(result.config.max_context_tokens, 128_000);
        assert_eq!(result.max_context_source, ContextWindowSource::FallbackDefault);
        assert_eq!(result.model_context_tokens, None);
    }

    #[test]
    fn resolves_hint_route_before_model_lookup() {
        let router = router_with_model("openrouter", "wide", 1_000_000);
        let routes = vec![ModelRouteConfig {
            settings: Default::default(),
            hint: "reasoning".to_string(),
            provider: "openrouter".to_string(),
            model: "wide".to_string(),
            api_key: None,
            credential_ref: None,
        }];
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "openai",
            "hint:reasoning",
            sources(&router),
            &routes,
        );
        assert_eq!(result.selected_provider, "openrouter");
        assert_eq!(result.selected_model, "wide");
        assert_eq!(result.config.max_context_tokens, 1_000_000);
    }

    #[test]
    fn compaction_window_comes_from_release_catalog_without_router_rows() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "openrouter",
            "google/gemini-3-flash",
            sources(&RouterConfig::default()),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 1_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::Catalog);
    }

    #[test]
    fn resolves_modern_claude_builtin_to_1m() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "anthropic",
            "claude-opus-4-8",
            sources(&RouterConfig::default()),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 1_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::Catalog);
    }

    #[test]
    fn resolves_current_haiku_builtin_conservatively() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "anthropic",
            "claude-haiku-4-5",
            sources(&RouterConfig::default()),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 200_000);
        assert_eq!(result.max_context_source, ContextWindowSource::Catalog);
    }

    #[test]
    fn resolves_local_legacy_builtin_conservatively() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "ollama",
            "*",
            sources(&RouterConfig::default()),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 32_000);
        assert_eq!(result.max_context_source, ContextWindowSource::Catalog);
    }

    #[test]
    fn catalog_context_matches_aggregator_suffix_without_matching_ollama_wildcard() {
        let variant = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "openrouter",
            "moonshotai/kimi-k2.5:free",
            sources(&RouterConfig::default()),
            &[],
        );
        assert_eq!(variant.config.max_context_tokens, 262_144);
        assert_eq!(variant.max_context_source, ContextWindowSource::Catalog);

        let unknown_local = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "ollama",
            "prx-unknown-local-model",
            sources(&RouterConfig::default()),
            &[],
        );
        assert_eq!(unknown_local.config.max_context_tokens, 128_000);
        assert_eq!(unknown_local.max_context_source, ContextWindowSource::FallbackDefault);
    }

    #[test]
    fn falls_back_when_no_model_metadata_matches() {
        let result = resolve_effective_compaction_config(
            &AgentCompactionConfig::default(),
            "unknown-provider",
            "unknown-model",
            sources(&RouterConfig::default()),
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
            sources(&RouterConfig::default()),
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
            sources(&RouterConfig::default()),
            &[],
        );
        assert_eq!(result.config.max_context_tokens, 10_000_000);
        assert_eq!(result.max_context_source, ContextWindowSource::FallbackDefault);
        assert_eq!(result.model_context_tokens, None);
        assert_eq!(result.requested_context_tokens, Some(20_000_000));
        assert!(result.kernel_capped);
    }
    fn settings_config(text: &str) -> Config {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        Config::load_from_path(&path, dir.path().join("workspace")).unwrap()
    }

    fn settings_resolution(config: &Config, provider: &str, model: &str) -> EffectiveCompactionConfig {
        resolve_effective_compaction_config(
            &config.agent.compaction,
            provider,
            model,
            ContextWindowSources::from_config(config),
            &config.model_routes,
        )
    }

    #[test]
    fn model_settings_window_and_reserved_output_override_catalog() {
        let config = settings_config(
            r#"[model_settings."openai/gpt-5.6"]
context_window = 200000
reserved_output_tokens = 8000"#,
        );
        let result = settings_resolution(&config, "openai", "gpt-5.6");
        assert_eq!(result.config.max_context_tokens, 200_000);
        assert_eq!(result.config.reserve_tokens, 8_000);
        assert_eq!(result.max_context_source, ContextWindowSource::ModelSettings);
    }

    #[test]
    fn window_only_local_model_loads_validates_and_resolves_without_catalog_warning() {
        let config = settings_config(
            r#"[model_settings."ollama/my-local-model:latest"]
context_window = 16384"#,
        );
        crate::config::model_settings::validate_changes(None, &config).unwrap();
        assert!(crate::config::model_settings::warnings(&config).unwrap().is_empty());
        let result = settings_resolution(&config, "ollama", "my-local-model:latest");
        assert_eq!(result.config.max_context_tokens, 16_384);
        assert_eq!(result.max_context_source, ContextWindowSource::ModelSettings);
    }

    #[test]
    fn model_settings_window_beats_legacy_router_metadata() {
        let mut config = settings_config(
            r#"[model_settings."openai/gpt-5.6"]
context_window = 300000"#,
        );
        config.router.models.push(model("openai", "gpt-5.6", 100_000));
        let result = settings_resolution(&config, "openai", "gpt-5.6");
        assert_eq!(result.config.max_context_tokens, 300_000);
        assert_eq!(result.max_context_source, ContextWindowSource::ModelSettings);
    }

    #[test]
    fn explicit_agent_window_beats_model_settings_window() {
        let mut config = settings_config(
            r#"[model_settings."openai/gpt-5.6"]
context_window = 300000"#,
        );
        config.agent.compaction.max_context_tokens = 123_456;
        config.agent.compaction.max_context_tokens_explicit = true;
        let result = settings_resolution(&config, "openai", "gpt-5.6");
        assert_eq!(result.config.max_context_tokens, 123_456);
        assert_eq!(result.max_context_source, ContextWindowSource::AgentCompactionOverride);
    }

    #[derive(Clone)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn captured_warnings(config: &Config) -> String {
        let captured = Capture(Default::default());
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let once = Once::new();
        tracing::subscriber::with_default(subscriber, || {
            emit_legacy_router_window_warning(config, &once);
            emit_legacy_router_window_warning(config, &once);
        });
        let bytes = captured.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn legacy_router_window_warns_once_and_preserves_the_base_window() {
        let mut config = Config::default();
        assert_eq!(legacy_router_window_warning(&config), None);
        config
            .router
            .models
            .push(model("ollama", "my-local-model:latest", 16_384));
        let result = settings_resolution(&config, "ollama", "my-local-model:latest");
        assert_eq!(result.config.max_context_tokens, 16_384, "base legacy window");
        assert_eq!(result.max_context_source, ContextWindowSource::RouterModelConfig);
        let captured = captured_warnings(&config);
        assert_eq!(
            captured.matches("legacy context-window configuration").count(),
            1,
            "{captured}"
        );
        assert!(captured.contains("WARN"), "{captured}");
        assert!(
            legacy_router_window_warning(&config)
                .unwrap()
                .contains("ollama/my-local-model:latest")
        );
    }

    #[test]
    fn legacy_router_window_warning_redacts_userinfo_query_and_fragment() {
        let mut config = Config::default();
        for provider in [
            "custom:https://user:USERSECRET@host.test/v1",
            "custom:https://host.test/v1?key=QUERYSECRET&x=1",
            "custom:https://host.test/v1#FRAGSECRET",
        ] {
            config.router.models.push(model(provider, "prx_model", 16_384));
        }
        let warning = legacy_router_window_warning(&config).unwrap();
        let captured = captured_warnings(&config);
        for text in [&warning, &captured] {
            for secret in ["USERSECRET", "QUERYSECRET", "FRAGSECRET", "user:"] {
                assert!(!text.contains(secret), "warning leaked {secret}: {text}");
            }
            for row in &config.router.models {
                let label = redact_provider_text(&format!("{}/{}", row.provider, row.model_id));
                let key = redact_provider_text(&model_settings_window_key(
                    &CatalogView::release_only(),
                    &row.provider,
                    &row.model_id,
                ));
                assert!(text.contains(&label), "missing redacted row: {text}");
                assert!(text.contains(&key), "missing redacted key: {text}");
            }
        }
        assert_eq!(
            captured.matches("legacy context-window configuration").count(),
            1,
            "{captured}"
        );
    }
    #[test]
    fn shown_provider_redacts_query_and_fragment_values() {
        for (provider, secret) in [
            ("custom:https://host.test/v1?key=QUERYSECRET", "QUERYSECRET"),
            ("custom:https://host.test/v1#FRAGSECRET", "FRAGSECRET"),
        ] {
            let result = resolve_effective_compaction_config(
                &AgentCompactionConfig::default(),
                provider,
                "prx_model",
                sources(&RouterConfig::default()),
                &[],
            );
            assert_eq!(result.selected_provider, provider);
            let shown = result.shown_provider();
            assert!(!shown.contains(secret), "{shown}");
            assert!(shown.contains("[redacted]"), "{shown}");
        }
    }
}
