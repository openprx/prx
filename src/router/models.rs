use crate::config::RouterModelConfig;

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
        latency_ms,
        categories: categories
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        elo_rating: 1_000.0,
    }
}

pub fn builtin_model_capabilities() -> Vec<RouterModelConfig> {
    vec![
        model(
            "anthropic",
            "claude-opus-4-6",
            15.0,
            200_000,
            3_000,
            &["conversation", "analysis", "long_doc", "code"],
        ),
        model(
            "anthropic",
            "claude-sonnet-4-6",
            3.0,
            200_000,
            2_000,
            &["conversation", "analysis", "code", "summary"],
        ),
        model(
            "anthropic",
            "claude-haiku-3",
            0.25,
            200_000,
            800,
            &["conversation", "summary", "translation"],
        ),
        model(
            "openai-codex",
            "gpt-code5.3",
            2.0,
            128_000,
            1_500,
            &["code", "debug", "refactor", "analysis"],
        ),
        model(
            "openai",
            "gpt-4o",
            5.0,
            128_000,
            2_000,
            &["conversation", "analysis", "code"],
        ),
        model(
            "openai",
            "gpt-4o-mini",
            0.15,
            128_000,
            800,
            &["conversation", "summary", "translation"],
        ),
        model(
            "openrouter",
            "moonshotai/kimi-k2.5",
            0.6,
            128_000,
            2_000,
            &["conversation", "analysis", "long_doc"],
        ),
        model(
            "openrouter",
            "qwen/qwen3.5-plus",
            0.5,
            128_000,
            1_500,
            &["conversation", "code", "summary"],
        ),
        model(
            "openrouter",
            "thudm/glm-5",
            0.3,
            128_000,
            1_500,
            &["conversation", "summary", "translation"],
        ),
        model(
            "openrouter",
            "google/gemini-3-flash",
            0.1,
            1_000_000,
            1_000,
            &["conversation", "summary", "long_doc"],
        ),
        model(
            "openrouter",
            "x-ai/grok-4.1-fast",
            1.0,
            128_000,
            1_200,
            &["conversation", "analysis", "code"],
        ),
        model(
            "xai",
            "grok-3",
            3.0,
            128_000,
            2_000,
            &["conversation", "analysis", "code"],
        ),
        model(
            "gemini",
            "gemini-2.5-pro",
            1.25,
            1_000_000,
            2_000,
            &["conversation", "analysis", "long_doc", "code"],
        ),
        model(
            "gemini",
            "gemini-2.0-flash",
            0.1,
            1_000_000,
            800,
            &["conversation", "summary", "long_doc"],
        ),
        model("ollama", "*", 0.0, 32_000, 500, &["conversation", "code"]),
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
            assert!(
                !m.categories.is_empty(),
                "model {} has no categories",
                m.model_id
            );
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
}
