use crate::config::RouterModelConfig;

/// Router candidates derived entirely from the embedded release catalog.
/// The operator's explicit `[[router.models]]` entries still replace this
/// list at the router boundary.
pub fn builtin_model_capabilities(cost: &crate::config::schema::CostConfig) -> Vec<RouterModelConfig> {
    let Ok(catalog) = crate::providers::catalog::ReleaseCatalog::embedded_ref() else {
        return Vec::new();
    };
    model_capabilities_from_catalog(catalog, cost)
}

fn model_capabilities_from_catalog(
    catalog: &crate::providers::catalog::ReleaseCatalog,
    cost: &crate::config::schema::CostConfig,
) -> Vec<RouterModelConfig> {
    catalog
        .models
        .iter()
        .filter_map(|model| {
            if model.lifecycle.status != crate::providers::catalog::ModelLifecycleStatus::Active {
                return None;
            }
            let router = model.router.as_ref()?;
            let context = model.limits.context_window?;
            if !matches!(
                crate::llm::route_decision::usage_cost_resolution(&model.provider, &model.api_model, 1, 1, 0, 0, cost),
                crate::llm::route_decision::UsagePrice::Priced(_)
            ) {
                return None;
            }
            Some(RouterModelConfig {
                model_id: model.api_model.clone(),
                provider: model.provider.clone(),
                cost_per_million_tokens: router.cost_per_million_tokens as f32,
                max_context: usize::try_from(context).unwrap_or(usize::MAX),
                reserved_output_tokens: model
                    .limits
                    .max_output_tokens
                    .and_then(|tokens| usize::try_from(tokens).ok()),
                latency_ms: router.latency_ms,
                categories: router.categories.clone(),
                elo_rating: router.quality_rank as f32,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn builtin_models_not_empty() {
        let models = builtin_model_capabilities(&crate::config::schema::CostConfig::default());
        assert!(!models.is_empty());
    }

    #[test]
    fn builtin_models_have_unique_provider_model_pairs() {
        let models = builtin_model_capabilities(&crate::config::schema::CostConfig::default());
        let mut seen = HashSet::new();
        for m in &models {
            let key = format!("{}:{}", m.provider, m.model_id);
            assert!(seen.insert(key.clone()), "duplicate model entry: {key}");
        }
    }

    #[test]
    fn builtin_models_all_have_categories() {
        let models = builtin_model_capabilities(&crate::config::schema::CostConfig::default());
        for m in &models {
            assert!(!m.categories.is_empty(), "model {} has no categories", m.model_id);
        }
    }

    #[test]
    fn builtin_models_have_positive_latency() {
        let models = builtin_model_capabilities(&crate::config::schema::CostConfig::default());
        for m in &models {
            assert!(m.latency_ms > 0, "model {} has zero latency", m.model_id);
        }
    }

    #[test]
    fn builtin_models_exclude_retired_and_unpriced_catalog_rows() {
        let models = builtin_model_capabilities(&crate::config::schema::CostConfig::default());
        let catalog = crate::providers::catalog::ReleaseCatalog::embedded_ref().expect("release catalog");
        for model in &models {
            let release = catalog
                .model(&crate::providers::ProviderId::of(&model.provider), &model.model_id)
                .expect("every builtin router row comes from the release catalog");
            assert_eq!(
                release.lifecycle.status,
                crate::providers::catalog::ModelLifecycleStatus::Active,
                "non-active router candidate: {}",
                release.id
            );
            assert!(
                matches!(
                    crate::llm::route_decision::usage_cost_resolution(
                        &model.provider,
                        &model.model_id,
                        1,
                        1,
                        0,
                        0,
                        &crate::config::schema::CostConfig::default(),
                    ),
                    crate::llm::route_decision::UsagePrice::Priced(_)
                ),
                "router candidate has no effective billing tariff: {}",
                release.id
            );
        }
        let providers: HashSet<&str> = models.iter().map(|model| model.provider.as_str()).collect();
        assert!(providers.contains("anthropic"));
        assert!(providers.contains("ollama"));
        for provider in ["openrouter", "gemini"] {
            assert!(
                providers.contains(provider),
                "missing active priced candidates for {provider}"
            );
        }
        assert!(
            !providers.contains("openai"),
            "automatic direct OpenAI candidates require the C2 parameter wire mapping"
        );
    }

    fn context_for(models: &[RouterModelConfig], provider: &str, model_id: &str) -> usize {
        models
            .iter()
            .find(|model| model.provider == provider && model.model_id == model_id)
            .unwrap_or_else(|| panic!("missing builtin model {provider}/{model_id}"))
            .max_context
    }

    #[test]
    fn responses_only_codex_is_not_an_automatic_chat_candidate() {
        assert!(
            !builtin_model_capabilities(&crate::config::schema::CostConfig::default())
                .iter()
                .any(|model| model.provider == "openai" && model.model_id == "gpt-5.2-codex"),
            "the direct OpenAI adapter has no Responses wire path"
        );
    }

    #[test]
    fn router_pricing_filter_uses_upstream_tariffs_and_rejects_missing_tariffs() {
        let mut catalog = crate::providers::catalog::ReleaseCatalog::embedded_ref()
            .expect("test: embedded catalog")
            .clone();
        let template = catalog
            .models
            .iter()
            .find(|model| model.id == "openrouter/anthropic/claude-sonnet-5")
            .expect("test: priced Router metadata");
        let router = template.router.clone();
        let limits = template.limits.clone();
        // Only metadata needed to exercise the filter is added. This is not
        // evidence that either endpoint is available or transport-admitted.
        for id in ["openrouter/openai/gpt-5.6", "openrouter/google/gemini-3.5-flash"] {
            let row = catalog
                .models
                .iter_mut()
                .find(|model| model.id == id)
                .expect("test: existing release row");
            assert!(row.pricing.is_none(), "test: neither row has its own tariff");
            row.router = router.clone();
            row.limits = limits.clone();
        }
        let models = model_capabilities_from_catalog(&catalog, &crate::config::schema::CostConfig::default());
        assert!(
            models
                .iter()
                .any(|model| model.provider == "openrouter" && model.model_id == "openai/gpt-5.6"),
            "a published upstream tariff must satisfy the same pricing rule as onboarding"
        );
        assert!(
            !models
                .iter()
                .any(|model| model.provider == "openrouter" && model.model_id == "google/gemini-3.5-flash"),
            "a ranking cost must not turn an unpriced model into a billing-priced candidate"
        );
    }

    #[test]
    fn builtin_context_windows_match_the_active_priced_router_rows() {
        let models = builtin_model_capabilities(&crate::config::schema::CostConfig::default());
        let catalog = crate::providers::catalog::ReleaseCatalog::embedded_ref().expect("release catalog");

        for (provider, model_id, expected_context) in [
            ("anthropic", "claude-fable-5", 1_000_000),
            ("anthropic", "claude-opus-4-7", 1_000_000),
            ("anthropic", "claude-opus-4-6", 1_000_000),
            ("anthropic", "claude-sonnet-4-6", 1_000_000),
            ("openrouter", "google/gemini-3-flash", 1_000_000),
            ("gemini", "gemini-2.5-pro", 1_000_000),
            ("gemini", "gemini-2.0-flash", 1_000_000),
            ("anthropic", "claude-haiku-4-5", 200_000),
            ("anthropic", "claude-haiku-3", 200_000),
            ("openai", "gpt-4o", 128_000),
            ("openai", "gpt-4o-mini", 128_000),
            ("openrouter", "moonshotai/kimi-k2.5", 262_144),
        ] {
            let release = catalog
                .model(&crate::providers::ProviderId::of(provider), model_id)
                .unwrap_or_else(|| panic!("missing release model {provider}/{model_id}"));
            assert_eq!(
                release.limits.context_window,
                Some(expected_context),
                "release context changed for {provider}/{model_id}"
            );
            assert!(
                !models
                    .iter()
                    .any(|model| model.provider == provider && model.model_id == model_id),
                "retired or unpriced model remained an automatic candidate: {provider}/{model_id}"
            );
        }

        assert_eq!(
            context_for(&models, "anthropic", "claude-opus-4-8"),
            1_000_000,
            "Claude Opus 4.8 is a current 1M model and must not fall back to 128K"
        );
        assert_eq!(
            context_for(&models, "anthropic", "claude-haiku-4-5-20251001"),
            200_000,
            "Haiku 4.5 dated ID is a known-smaller Claude family and must not inherit 1M"
        );
        assert_eq!(
            context_for(&models, "ollama", "*"),
            32_000,
            "local legacy fallback must remain conservative"
        );
    }
}

#[cfg(test)]
mod operator_prices;
