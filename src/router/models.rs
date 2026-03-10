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
