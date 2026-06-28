use crate::config::RouterModelConfig;

const MODERN_LONG_CONTEXT_TOKENS: usize = 1_000_000;
const ANTHROPIC_HAIKU_CONTEXT_TOKENS: usize = 200_000;
const GPT_4O_CONTEXT_TOKENS: usize = 128_000;
const KIMI_K2_5_CONTEXT_TOKENS: usize = 262_144;
const AGGREGATOR_STANDARD_CONTEXT_TOKENS: usize = 128_000;
const LEGACY_LOCAL_CONTEXT_TOKENS: usize = 32_000;

fn model(
    provider: &str,
    model_id: &str,
    cost_per_million_tokens: f32,
    max_context: usize,
    latency_ms: u32,
    categories: &[&str],
) -> RouterModelConfig {
    RouterModelConfig {
        model_id: model_id.to_string(),
        provider: provider.to_string(),
        cost_per_million_tokens,
        max_context,
        reserved_output_tokens: None,
        latency_ms,
        categories: categories.iter().map(|value| (*value).to_string()).collect(),
        elo_rating: 1_000.0,
    }
}

pub fn builtin_model_capabilities() -> Vec<RouterModelConfig> {
    vec![
        model(
            "anthropic",
            "claude-fable-5",
            10.0,
            MODERN_LONG_CONTEXT_TOKENS,
            3_500,
            &["conversation", "analysis", "long_doc", "code"],
        ),
        model(
            "anthropic",
            "claude-opus-4-8",
            5.0,
            MODERN_LONG_CONTEXT_TOKENS,
            3_000,
            &["conversation", "analysis", "long_doc", "code"],
        ),
        model(
            "anthropic",
            "claude-opus-4-7",
            5.0,
            MODERN_LONG_CONTEXT_TOKENS,
            3_000,
            &["conversation", "analysis", "long_doc", "code"],
        ),
        model(
            "anthropic",
            "claude-opus-4-6",
            15.0,
            MODERN_LONG_CONTEXT_TOKENS,
            3_000,
            &["conversation", "analysis", "long_doc", "code"],
        ),
        model(
            "anthropic",
            "claude-sonnet-4-6",
            3.0,
            MODERN_LONG_CONTEXT_TOKENS,
            2_000,
            &["conversation", "analysis", "code", "summary"],
        ),
        model(
            "anthropic",
            "claude-haiku-4-5-20251001",
            1.0,
            ANTHROPIC_HAIKU_CONTEXT_TOKENS,
            800,
            &["conversation", "summary", "translation"],
        ),
        model(
            "anthropic",
            "claude-haiku-4-5",
            1.0,
            ANTHROPIC_HAIKU_CONTEXT_TOKENS,
            800,
            &["conversation", "summary", "translation"],
        ),
        model(
            "anthropic",
            "claude-haiku-3",
            0.25,
            ANTHROPIC_HAIKU_CONTEXT_TOKENS,
            800,
            &["conversation", "summary", "translation"],
        ),
        model(
            "openai-codex",
            "gpt-code5.3",
            2.0,
            GPT_4O_CONTEXT_TOKENS,
            1_500,
            &["code", "debug", "refactor", "analysis"],
        ),
        model(
            "openai",
            "gpt-4o",
            5.0,
            GPT_4O_CONTEXT_TOKENS,
            2_000,
            &["conversation", "analysis", "code"],
        ),
        model(
            "openai",
            "gpt-4o-mini",
            0.15,
            GPT_4O_CONTEXT_TOKENS,
            800,
            &["conversation", "summary", "translation"],
        ),
        model(
            "openrouter",
            "moonshotai/kimi-k2.5",
            0.6,
            KIMI_K2_5_CONTEXT_TOKENS,
            2_000,
            &["conversation", "analysis", "long_doc"],
        ),
        model(
            "openrouter",
            "qwen/qwen3.5-plus",
            0.5,
            AGGREGATOR_STANDARD_CONTEXT_TOKENS,
            1_500,
            &["conversation", "code", "summary"],
        ),
        model(
            "openrouter",
            "thudm/glm-5",
            0.3,
            AGGREGATOR_STANDARD_CONTEXT_TOKENS,
            1_500,
            &["conversation", "summary", "translation"],
        ),
        model(
            "openrouter",
            "google/gemini-3-flash",
            0.1,
            MODERN_LONG_CONTEXT_TOKENS,
            1_000,
            &["conversation", "summary", "long_doc"],
        ),
        model(
            "openrouter",
            "x-ai/grok-4.1-fast",
            1.0,
            AGGREGATOR_STANDARD_CONTEXT_TOKENS,
            1_200,
            &["conversation", "analysis", "code"],
        ),
        model(
            "xai",
            "grok-3",
            3.0,
            AGGREGATOR_STANDARD_CONTEXT_TOKENS,
            2_000,
            &["conversation", "analysis", "code"],
        ),
        model(
            "gemini",
            "gemini-2.5-pro",
            1.25,
            MODERN_LONG_CONTEXT_TOKENS,
            2_000,
            &["conversation", "analysis", "long_doc", "code"],
        ),
        model(
            "gemini",
            "gemini-2.0-flash",
            0.1,
            MODERN_LONG_CONTEXT_TOKENS,
            800,
            &["conversation", "summary", "long_doc"],
        ),
        model(
            "ollama",
            "*",
            0.0,
            LEGACY_LOCAL_CONTEXT_TOKENS,
            500,
            &["conversation", "code"],
        ),
    ]
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
