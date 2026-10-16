use crate::config::RouterModelConfig;

/// Router candidates derived entirely from the embedded release catalog.
/// The operator's explicit `[[router.models]]` entries still replace this
/// list at the router boundary.
pub fn builtin_model_capabilities() -> Vec<RouterModelConfig> {
    let Ok(catalog) = crate::providers::catalog::ReleaseCatalog::embedded_ref() else {
        return Vec::new();
    };
    catalog
        .models
        .iter()
        .filter_map(|model| {
            let router = model.router.as_ref()?;
            let context = model.limits.context_window?;
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
        let models = builtin_model_capabilities();
        assert!(!models.is_empty());
    }

    #[test]
    fn builtin_models_have_unique_provider_model_pairs() {
        let models = builtin_model_capabilities();
        let mut seen = HashSet::new();
        for m in &models {
            let key = format!("{}:{}", m.provider, m.model_id);
            assert!(seen.insert(key.clone()), "duplicate model entry: {key}");
        }
    }

    #[test]
    fn builtin_models_all_have_categories() {
        let models = builtin_model_capabilities();
        for m in &models {
            assert!(!m.categories.is_empty(), "model {} has no categories", m.model_id);
        }
    }

    #[test]
    fn builtin_models_have_positive_latency() {
        let models = builtin_model_capabilities();
        for m in &models {
            assert!(m.latency_ms > 0, "model {} has zero latency", m.model_id);
        }
    }

    #[test]
    fn builtin_models_include_major_providers() {
        let models = builtin_model_capabilities();
        let providers: HashSet<&str> = models.iter().map(|m| m.provider.as_str()).collect();
        assert!(providers.contains("anthropic"));
        assert!(providers.contains("openai"));
        assert!(providers.contains("gemini"));
        assert!(providers.contains("ollama"));
    }

    fn context_for(models: &[RouterModelConfig], provider: &str, model_id: &str) -> usize {
        models
            .iter()
            .find(|model| model.provider == provider && model.model_id == model_id)
            .unwrap_or_else(|| panic!("missing builtin model {provider}/{model_id}"))
            .max_context
    }

    #[test]
    fn builtin_context_windows_match_p4b_policy() {
        let models = builtin_model_capabilities();

        assert_eq!(
            context_for(&models, "anthropic", "claude-fable-5"),
            1_000_000,
            "Claude Fable 5 is a current 1M model and must not fall back to 128K"
        );
        assert_eq!(
            context_for(&models, "anthropic", "claude-opus-4-8"),
            1_000_000,
            "Claude Opus 4.8 is a current 1M model and must not fall back to 128K"
        );
        assert_eq!(
            context_for(&models, "anthropic", "claude-opus-4-7"),
            1_000_000,
            "Claude Opus 4.7 is a current 1M model and must not fall back to 128K"
        );
        assert_eq!(context_for(&models, "anthropic", "claude-opus-4-6"), 1_000_000);
        assert_eq!(context_for(&models, "anthropic", "claude-sonnet-4-6"), 1_000_000);
        assert_eq!(context_for(&models, "openrouter", "google/gemini-3-flash"), 1_000_000);
        assert_eq!(context_for(&models, "gemini", "gemini-2.5-pro"), 1_000_000);
        assert_eq!(context_for(&models, "gemini", "gemini-2.0-flash"), 1_000_000);

        assert_eq!(
            context_for(&models, "anthropic", "claude-haiku-4-5"),
            200_000,
            "Haiku 4.5 is a known-smaller Claude family and must not inherit 1M"
        );
        assert_eq!(
            context_for(&models, "anthropic", "claude-haiku-4-5-20251001"),
            200_000,
            "Haiku 4.5 dated ID is a known-smaller Claude family and must not inherit 1M"
        );
        assert_eq!(
            context_for(&models, "anthropic", "claude-haiku-3"),
            200_000,
            "Haiku is a known-smaller Claude family and must not inherit 1M"
        );
        assert_eq!(
            context_for(&models, "openai", "gpt-4o"),
            128_000,
            "gpt-4o is a known 128K model and must not inherit 1M"
        );
        assert_eq!(
            context_for(&models, "openai", "gpt-4o-mini"),
            128_000,
            "gpt-4o-mini is a known 128K model and must not inherit 1M"
        );
        assert_eq!(
            context_for(&models, "openrouter", "moonshotai/kimi-k2.5"),
            262_144,
            "Kimi K2.5 must match OpenRouter's published context length"
        );
        assert_eq!(
            context_for(&models, "ollama", "*"),
            32_000,
            "local legacy fallback must remain conservative"
        );
    }
}
