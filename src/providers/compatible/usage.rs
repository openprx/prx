//! Usage fields shared by compatible responses and OpenRouter SSE frames.
use crate::llm::route_decision::TokenUsage;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(super) struct OpenAiCompatibleUsage {
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    prompt_tokens: Option<u32>,
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    completion_tokens: Option<u32>,
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    total_tokens: Option<u32>,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptTokenDetails>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct OpenAiPromptTokenDetails {
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    cached_tokens: Option<u32>,
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    cache_write_tokens: Option<u32>,
}

impl OpenAiCompatibleUsage {
    pub(super) fn into_reported(self) -> Option<TokenUsage> {
        let total = self.total_tokens.or_else(|| {
            self.prompt_tokens
                .zip(self.completion_tokens)
                .map(|(p, c)| p.saturating_add(c))
        });
        if total.is_some() || self.prompt_tokens.zip(self.completion_tokens).is_some() {
            Some(TokenUsage::reported_with_cache(
                self.prompt_tokens,
                self.completion_tokens,
                total,
                self.prompt_tokens_details
                    .and_then(|details| details.cache_write_tokens),
                self.prompt_tokens_details.and_then(|details| details.cached_tokens),
            ))
        } else {
            None
        }
    }
}

fn optional_u32_from_any<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn openrouter_stream_usage_preserves_cache_read_and_write_tokens() {
        let usage: super::OpenAiCompatibleUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens":100, "completion_tokens":20,
            "prompt_tokens_details":{"cached_tokens":30,"cache_write_tokens":10}
        }))
        .unwrap();
        assert_eq!(
            usage.into_reported(),
            Some(crate::llm::route_decision::TokenUsage::reported_with_cache(
                Some(100),
                Some(20),
                Some(120),
                Some(10),
                Some(30)
            ))
        );
    }
}
