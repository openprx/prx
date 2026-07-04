use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage};
use crate::multimodal;
use crate::onboard::auto_detect::is_claude_code_oauth_setup_token;
use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, ChatTrace, Provider,
    StreamChunk, StreamError, StreamOptions, StreamResult, ToolCall as ProviderToolCall, ToolCallChunk,
    ToolCallChunkStatus,
};
use crate::tools::ToolSpec;
use crate::tools::schema::SchemaCleanr;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// OAuth state for Claude Code token auto-refresh.
struct OAuthState {
    credential: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
}

pub struct AnthropicProvider {
    oauth: Mutex<OAuthState>,
    base_url: String,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<Message>,
    temperature: f64,
}

#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct NativeChatRequest<'a> {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<SystemPrompt>,
    messages: Vec<NativeMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec<'a>>>,
}

#[derive(Debug, Serialize)]
struct NativeMessage {
    role: String,
    content: Vec<NativeContentOut>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum NativeContentOut {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "image")]
    Image { source: NativeImageSource },
}

#[derive(Debug, Serialize)]
struct NativeImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Serialize)]
struct NativeToolSpec<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    cache_type: String,
}

impl CacheControl {
    fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SystemPrompt {
    String(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Deserialize)]
struct NativeChatResponse {
    #[serde(default)]
    content: Vec<NativeContentIn>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
}

impl AnthropicUsage {
    const fn into_reported(self) -> TokenUsage {
        let total = match (self.input_tokens, self.output_tokens) {
            (Some(input), Some(output)) => Some(input.saturating_add(output)),
            _ => None,
        };
        TokenUsage::reported(self.input_tokens, self.output_tokens, total)
    }
}

#[derive(Debug, Deserialize)]
struct NativeContentIn {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
    /// Anthropic "thinking" content blocks carry chain-of-thought in the
    /// `thinking` field (extended thinking mode). Surfaced as reasoning.
    #[serde(default)]
    thinking: Option<String>,
}

impl AnthropicProvider {
    const ANTHROPIC_BASE64_WARN_BYTES: usize = 20 * 1024 * 1024;

    pub fn new(credential: Option<&str>) -> Self {
        Self::with_base_url(credential, None)
    }

    pub fn with_base_url(credential: Option<&str>, base_url: Option<&str>) -> Self {
        let base_url = base_url
            .map(|u| u.trim_end_matches('/'))
            .unwrap_or("https://api.anthropic.com")
            .to_string();
        Self {
            oauth: Mutex::new(OAuthState {
                credential: credential
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .map(ToString::to_string),
                refresh_token: None,
                expires_at: None,
            }),
            base_url,
        }
    }

    pub fn with_oauth(credential: Option<&str>, refresh_token: Option<String>, expires_at: Option<i64>) -> Self {
        Self {
            oauth: Mutex::new(OAuthState {
                credential: credential
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .map(ToString::to_string),
                refresh_token,
                expires_at,
            }),
            base_url: "https://api.anthropic.com".to_string(),
        }
    }

    /// 90-second buffer before expiry to proactively refresh.
    const REFRESH_BUFFER_MS: i64 = 90_000;

    /// Returns a fresh credential, refreshing the OAuth token if needed.
    /// For plain API keys (no refresh_token), returns the credential as-is.
    fn ensure_fresh_credential(&self) -> anyhow::Result<String> {
        let mut state = self.oauth.lock();

        // If no refresh_token, this is a plain API key — just return it.
        if state.refresh_token.is_none() {
            return state.credential.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "Anthropic credentials not set. Set ANTHROPIC_API_KEY or ANTHROPIC_OAUTH_TOKEN (setup-token)."
                )
            });
        }

        // Check whether the token is expired or within the buffer.
        let needs_refresh = state.expires_at.map_or_else(
            || state.credential.is_none(),
            |expires_at| {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|d| i64::try_from(d.as_millis()).ok())
                    .unwrap_or(i64::MAX);
                expires_at <= now_ms.saturating_add(Self::REFRESH_BUFFER_MS)
            },
        );

        if needs_refresh {
            let refresh_token = state
                .refresh_token
                .clone()
                .ok_or_else(|| anyhow::anyhow!("OAuth token expired but no refresh_token available"))?;
            tracing::info!("Proactively refreshing Claude Code OAuth token");
            let refreshed = super::refresh_claude_code_access_token(&refresh_token)?;

            state.credential = refreshed.access_token.clone();
            if let Some(new_expires) = refreshed.expires_at {
                state.expires_at = Some(new_expires);
            }
            if let Some(new_refresh) = refreshed.refresh_token.as_ref() {
                state.refresh_token = Some(new_refresh.clone());
            }

            // Persist refreshed credentials to the cached file.
            let write_creds = super::ClaudeCodeCredentials {
                access_token: refreshed.access_token,
                refresh_token: Some(refreshed.refresh_token.unwrap_or_else(|| refresh_token.clone())),
                expires_at: refreshed.expires_at,
                subscription_type: None,
            };
            if let Err(e) = super::write_claude_code_cached_credentials(&write_creds) {
                tracing::warn!(error = %e, "Failed to write refreshed Claude Code credentials");
            }
        }

        state.credential.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "Anthropic credentials not set. Set ANTHROPIC_API_KEY or ANTHROPIC_OAUTH_TOKEN (setup-token)."
            )
        })
    }

    /// Try to refresh the token after a 401 response. Returns Ok(new_credential) if
    /// refresh succeeded, Err if no refresh possible (plain API key or no refresh_token).
    fn try_refresh_after_401(&self) -> anyhow::Result<String> {
        let mut state = self.oauth.lock();

        let refresh_token = state
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No refresh_token available for 401 retry"))?;

        tracing::info!("Refreshing Claude Code OAuth token after 401");
        let refreshed = super::refresh_claude_code_access_token(&refresh_token)?;

        state.credential = refreshed.access_token.clone();
        if let Some(new_expires) = refreshed.expires_at {
            state.expires_at = Some(new_expires);
        }
        if let Some(new_refresh) = refreshed.refresh_token.as_ref() {
            state.refresh_token = Some(new_refresh.clone());
        }

        let write_creds = super::ClaudeCodeCredentials {
            access_token: refreshed.access_token.clone(),
            refresh_token: Some(refreshed.refresh_token.unwrap_or_else(|| refresh_token.clone())),
            expires_at: refreshed.expires_at,
            subscription_type: None,
        };
        if let Err(e) = super::write_claude_code_cached_credentials(&write_creds) {
            tracing::warn!(error = %e, "Failed to write refreshed Claude Code credentials");
        }

        refreshed
            .access_token
            .ok_or_else(|| anyhow::anyhow!("OAuth refresh succeeded but returned no access_token"))
    }

    fn is_setup_token(token: &str) -> bool {
        is_claude_code_oauth_setup_token(token)
    }

    fn apply_auth(&self, request: reqwest::RequestBuilder, credential: &str) -> reqwest::RequestBuilder {
        if Self::is_setup_token(credential) {
            request
                .header("Authorization", format!("Bearer {credential}"))
                .header("anthropic-beta", "oauth-2025-04-20")
        } else {
            request.header("x-api-key", credential)
        }
    }

    /// Cache system prompts larger than ~1024 tokens (3KB of text)
    const fn should_cache_system(text: &str) -> bool {
        text.len() > 3072
    }

    /// Cache conversations with more than 4 messages (excluding system)
    fn should_cache_conversation(messages: &[ChatMessage]) -> bool {
        messages.iter().filter(|m| m.role != "system").count() > 4
    }

    /// Apply cache control to the last message content block
    fn apply_cache_to_last_message(messages: &mut [NativeMessage]) {
        if let Some(last_msg) = messages.last_mut() {
            if let Some(last_content) = last_msg.content.last_mut() {
                match last_content {
                    NativeContentOut::Text { cache_control, .. }
                    | NativeContentOut::ToolResult { cache_control, .. } => {
                        *cache_control = Some(CacheControl::ephemeral());
                    }
                    NativeContentOut::ToolUse { .. } | NativeContentOut::Image { .. } => {}
                }
            }
        }
    }

    fn parse_anthropic_image_source(image_ref: &str) -> Option<NativeImageSource> {
        let rest = image_ref.strip_prefix("data:")?;
        let (meta, data) = rest.split_once(',')?;
        let media_type = meta.strip_suffix(";base64")?;
        let cleaned_data: String = data.chars().filter(|c| !c.is_whitespace()).collect();
        if media_type.is_empty() || cleaned_data.is_empty() {
            return None;
        }

        let normalized_media_type = media_type.trim().to_ascii_lowercase();
        let decoded_len = STANDARD.decode(&cleaned_data).ok().map(|bytes| bytes.len());
        if let Some(decoded_len) = decoded_len {
            if decoded_len > Self::ANTHROPIC_BASE64_WARN_BYTES {
                tracing::warn!(
                    media_type = normalized_media_type,
                    size_bytes = decoded_len,
                    limit_bytes = Self::ANTHROPIC_BASE64_WARN_BYTES,
                    "Anthropic image payload exceeds recommended size limit"
                );
            }
        } else {
            tracing::warn!(
                media_type = normalized_media_type,
                "Anthropic image payload could not be base64-decoded during validation"
            );
        }

        Some(NativeImageSource {
            source_type: "base64".to_string(),
            media_type: normalized_media_type,
            data: cleaned_data,
        })
    }

    fn push_text_block(blocks: &mut Vec<NativeContentOut>, text: &str) {
        if !text.is_empty() {
            blocks.push(NativeContentOut::Text {
                text: text.to_string(),
                cache_control: None,
            });
        }
    }

    fn convert_user_content(content: &str) -> Vec<NativeContentOut> {
        let (_, image_refs) = multimodal::parse_image_markers(content);
        if image_refs.is_empty() {
            return vec![NativeContentOut::Text {
                text: content.to_string(),
                cache_control: None,
            }];
        }

        let mut blocks = Vec::new();
        let mut cursor = 0usize;

        while let Some(rel_start) = content[cursor..].find("[IMAGE:") {
            let start = cursor + rel_start;
            let marker_start = start + "[IMAGE:".len();

            let Some(rel_end) = content[marker_start..].find(']') else {
                Self::push_text_block(&mut blocks, &content[cursor..]);
                return if blocks.is_empty() {
                    vec![NativeContentOut::Text {
                        text: content.to_string(),
                        cache_control: None,
                    }]
                } else {
                    blocks
                };
            };

            let end = marker_start + rel_end;
            let candidate = content[marker_start..end].trim();

            if let Some(source) = Self::parse_anthropic_image_source(candidate) {
                Self::push_text_block(&mut blocks, &content[cursor..start]);
                blocks.push(NativeContentOut::Image { source });
            } else {
                Self::push_text_block(&mut blocks, &content[cursor..=end]);
            }

            cursor = end + 1;
        }

        if cursor < content.len() {
            Self::push_text_block(&mut blocks, &content[cursor..]);
        }

        if blocks.is_empty() {
            vec![NativeContentOut::Text {
                text: content.to_string(),
                cache_control: None,
            }]
        } else {
            blocks
        }
    }

    fn convert_tools<'a>(tools: Option<&'a [ToolSpec]>) -> Option<Vec<NativeToolSpec<'a>>> {
        let items = tools?;
        if items.is_empty() {
            return None;
        }
        let mut native_tools: Vec<NativeToolSpec<'a>> = items
            .iter()
            .map(|tool| NativeToolSpec {
                name: &tool.name,
                description: &tool.description,
                input_schema: SchemaCleanr::clean_for_anthropic(tool.parameters.clone()),
                cache_control: None,
            })
            .collect();

        // Cache the last tool definition (caches all tools)
        if let Some(last_tool) = native_tools.last_mut() {
            last_tool.cache_control = Some(CacheControl::ephemeral());
        }

        Some(native_tools)
    }

    fn parse_assistant_tool_call_message(content: &str) -> Option<Vec<NativeContentOut>> {
        let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
        let tool_calls = value
            .get("tool_calls")
            .and_then(|v| serde_json::from_value::<Vec<ProviderToolCall>>(v.clone()).ok())?;

        let mut blocks = Vec::new();
        if let Some(text) = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            blocks.push(NativeContentOut::Text {
                text: text.to_string(),
                cache_control: None,
            });
        }
        for call in tool_calls {
            let input = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
            blocks.push(NativeContentOut::ToolUse {
                id: call.id,
                name: call.name,
                input,
                cache_control: None,
            });
        }
        Some(blocks)
    }

    fn parse_tool_result_message(content: &str) -> Option<NativeMessage> {
        let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
        let tool_use_id = value
            .get("tool_call_id")
            .and_then(serde_json::Value::as_str)?
            .to_string();
        let result = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        Some(NativeMessage {
            role: "user".to_string(),
            content: vec![NativeContentOut::ToolResult {
                tool_use_id,
                content: result,
                cache_control: None,
            }],
        })
    }

    fn convert_messages(messages: &[ChatMessage]) -> (Option<SystemPrompt>, Vec<NativeMessage>) {
        let mut system_text = None;
        let mut native_messages = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" => {
                    if system_text.is_none() {
                        system_text = Some(msg.content.clone());
                    }
                }
                "assistant" => {
                    if let Some(blocks) = Self::parse_assistant_tool_call_message(&msg.content) {
                        native_messages.push(NativeMessage {
                            role: "assistant".to_string(),
                            content: blocks,
                        });
                    } else {
                        native_messages.push(NativeMessage {
                            role: "assistant".to_string(),
                            content: vec![NativeContentOut::Text {
                                text: msg.content.clone(),
                                cache_control: None,
                            }],
                        });
                    }
                }
                "tool" => {
                    if let Some(tool_result) = Self::parse_tool_result_message(&msg.content) {
                        native_messages.push(tool_result);
                    } else {
                        native_messages.push(NativeMessage {
                            role: "user".to_string(),
                            content: vec![NativeContentOut::Text {
                                text: msg.content.clone(),
                                cache_control: None,
                            }],
                        });
                    }
                }
                _ => {
                    native_messages.push(NativeMessage {
                        role: "user".to_string(),
                        content: Self::convert_user_content(&msg.content),
                    });
                }
            }
        }

        // Convert system text to SystemPrompt with cache control if large
        let system_prompt = system_text.map(|text| {
            if Self::should_cache_system(&text) {
                SystemPrompt::Blocks(vec![SystemBlock {
                    block_type: "text".to_string(),
                    text,
                    cache_control: Some(CacheControl::ephemeral()),
                }])
            } else {
                SystemPrompt::String(text)
            }
        });

        (system_prompt, native_messages)
    }

    fn parse_text_response(response: ChatResponse) -> anyhow::Result<String> {
        response
            .content
            .into_iter()
            .find(|c| c.kind == "text")
            .and_then(|c| c.text)
            .ok_or_else(|| anyhow::anyhow!("No response from Anthropic"))
    }

    fn parse_native_response(response: NativeChatResponse) -> ProviderChatResponse {
        // Walk content blocks and route them by `type`:
        //   - "text"     -> visible text_parts
        //   - "thinking" -> reasoning_parts (extended thinking mode)
        //   - "tool_use" -> tool_calls
        // Thinking blocks are NEVER mixed into visible text — they travel back
        // on `reasoning_content` so the chat consumer can drop them from the
        // live stream while preserving them in history.
        let mut text_parts = Vec::new();
        let mut reasoning_parts = Vec::new();
        let mut tool_calls = Vec::new();

        for block in response.content {
            match block.kind.as_str() {
                "text" => {
                    if let Some(text) = block.text.map(|t| t.trim().to_string()) {
                        if !text.is_empty() {
                            text_parts.push(text);
                        }
                    }
                }
                "thinking" => {
                    // Anthropic extended-thinking blocks: prefer the dedicated
                    // `thinking` field, fall back to `text` for forward-compat.
                    let raw = block.thinking.or(block.text);
                    if let Some(t) = raw {
                        let trimmed = t.trim();
                        if !trimmed.is_empty() {
                            reasoning_parts.push(trimmed.to_string());
                        }
                    }
                }
                "tool_use" => {
                    let name = block.name.unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    let arguments = block
                        .input
                        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
                    tool_calls.push(ProviderToolCall {
                        id: block.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        name,
                        arguments: arguments.to_string(),
                    });
                }
                _ => {}
            }
        }

        ProviderChatResponse {
            text: if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join("\n"))
            },
            tool_calls,
            reasoning_content: if reasoning_parts.is_empty() {
                None
            } else {
                Some(reasoning_parts.join("\n"))
            },
        }
    }

    async fn chat_metered(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<(ProviderChatResponse, TokenUsage)> {
        let credential = self.ensure_fresh_credential()?;

        let (system_prompt, mut messages) = Self::convert_messages(request.messages);

        if Self::should_cache_conversation(request.messages) {
            Self::apply_cache_to_last_message(&mut messages);
        }

        let native_request = NativeChatRequest {
            model: model.to_string(),
            max_tokens: 4096,
            system: system_prompt,
            messages,
            temperature,
            tools: Self::convert_tools(request.tools),
        };

        let req = self
            .http_client()
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&native_request);

        let response = self.apply_auth(req, &credential).send().await?;

        let native_response = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Ok(new_credential) = self.try_refresh_after_401() {
                let retry_req = self
                    .http_client()
                    .post(format!("{}/v1/messages", self.base_url))
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(&native_request);
                let retry_response = self.apply_auth(retry_req, &new_credential).send().await?;
                if !retry_response.status().is_success() {
                    return Err(super::api_error("Anthropic", retry_response).await);
                }
                retry_response.json::<NativeChatResponse>().await?
            } else {
                return Err(super::api_error("Anthropic", response).await);
            }
        } else {
            if !response.status().is_success() {
                return Err(super::api_error("Anthropic", response).await);
            }
            response.json::<NativeChatResponse>().await?
        };

        let usage = native_response.usage.map(AnthropicUsage::into_reported);
        let response = Self::parse_native_response(native_response);
        let tokens_used = usage.unwrap_or_else(|| {
            let chars = response.text.as_deref().unwrap_or("").chars().count()
                + response.reasoning_content.as_deref().unwrap_or("").chars().count();
            let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
            accumulator.finish_or_estimate_completion_chars(chars)
        });
        Ok((response, tokens_used))
    }

    fn http_client(&self) -> Client {
        crate::config::build_runtime_proxy_client_with_timeouts("provider.anthropic", 120, 10)
            .map_err(|e| {
                tracing::error!("proxy build failed for provider.anthropic, using direct: {e}");
                e
            })
            .unwrap_or_else(|_| Client::new())
    }
}

// ─── Streaming SSE (5a-7a) ────────────────────────────────────────────────
//
// Anthropic Messages API streaming uses an event-driven SSE protocol:
//   event: message_start
//   data: {...}
//
//   event: content_block_start
//   data: {"index":0,"content_block":{"type":"text"|"tool_use",...}}
//
//   event: content_block_delta
//   data: {"index":0,"delta":{"type":"text_delta"|"input_json_delta"|"thinking_delta",...}}
//
//   event: content_block_stop
//   data: {"index":0}
//
//   event: message_delta / message_stop
//
// Tool calls arrive as: content_block_start (tool_use with id/name) + a series
// of content_block_delta (input_json_delta partial_json fragments) +
// content_block_stop. **S3 T3-2-A**: we emit chunks incrementally —
//   - `content_block_start(tool_use)` → emit `ToolCallChunk { status: Streaming,
//     arguments_delta: Some(""), .. }` so the driver registers the new tool
//     call immediately;
//   - each `input_json_delta` → emit `ToolCallChunk { status: Streaming,
//     arguments_delta: Some(partial_json), .. }`;
//   - `content_block_stop(tool_use)` → emit a terminal
//     `ToolCallChunk { status: Completed, args: full JSON, .. }`.
// All chunks for one tool_use share the same stable `index` allocated at
// `content_block_start` time. Driver-side `ToolCallAggregator` (T3-1) handles
// both legacy (single Completed) and incremental (Streaming…Completed) shapes.

#[derive(Debug, Serialize)]
struct OwnedNativeToolSpec {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Serialize)]
struct StreamingChatRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<SystemPrompt>,
    messages: Vec<NativeMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OwnedNativeToolSpec>>,
    stream: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct AnthropicSseContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    /// For text blocks, initial text. Usually empty for tool_use start events.
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct AnthropicSseDelta {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}

#[derive(Debug, Default)]
struct AnthropicStreamState {
    /// Per-content-block accumulators keyed by Anthropic `index`.
    blocks: std::collections::HashMap<usize, AnthropicBlockState>,
    /// Stable emission order for tool calls (chunk `index` field).
    tool_call_order: usize,
}

#[derive(Debug, Default)]
struct AnthropicBlockState {
    kind: String,
    tool_id: String,
    tool_name: String,
    /// Buffer of all `input_json_delta` fragments accumulated for this tool_use
    /// content block. The buffered value is replayed verbatim in the terminal
    /// `Completed` chunk so the driver's aggregator can validate that
    /// `Σ Streaming.arguments_delta == Completed.args` (T3-0 invariant).
    tool_args: String,
    /// Stable chunk `index` for this tool_use, reserved at
    /// `content_block_start` time so every chunk in the Streaming … Completed
    /// sequence shares the same value. Unused for non-tool_use blocks.
    tool_order: usize,
}

impl AnthropicStreamState {
    /// Allocate a stable tool-call ordinal. Returns the previous counter value
    /// and bumps it by one (saturating, so a pathological stream never panics).
    const fn next_tool_order(&mut self) -> usize {
        let order = self.tool_call_order;
        self.tool_call_order = self.tool_call_order.saturating_add(1);
        order
    }

    /// Generate a synthetic id when the upstream `content_block_start` did
    /// not include one. Anthropic always sends `id`, but defensive code keeps
    /// the driver-side invariants (non-empty `id` per chunk) intact.
    fn id_or_synthetic(raw: String) -> String {
        if raw.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            raw
        }
    }

    /// **S3 T3-2-A**: also returns an initial `Streaming` chunk (with empty
    /// `arguments_delta`) for `tool_use` blocks so the driver can register the
    /// new tool call before any argument bytes arrive. Returns `None` for text
    /// and thinking blocks.
    fn on_content_block_start(&mut self, idx: usize, block: AnthropicSseContentBlock) -> Option<ToolCallChunk> {
        let kind = block.kind.clone();
        let is_tool = kind == "tool_use";
        let tool_id_raw = if is_tool {
            block.id.unwrap_or_default()
        } else {
            String::new()
        };
        let tool_name = if is_tool {
            block.name.unwrap_or_default()
        } else {
            String::new()
        };
        let tool_order = if is_tool { self.next_tool_order() } else { 0 };

        let entry = AnthropicBlockState {
            kind,
            tool_id: if is_tool {
                Self::id_or_synthetic(tool_id_raw)
            } else {
                String::new()
            },
            tool_name: tool_name.clone(),
            tool_args: String::new(),
            tool_order,
        };
        self.blocks.insert(idx, entry);
        let _ = block.text; // text in start is unused

        if !is_tool || tool_name.is_empty() {
            return None;
        }
        // SAFETY: we just inserted this entry above; the lookup is for the
        // freshly normalised id (post-synthesis).
        let id = self.blocks.get(&idx).map(|e| e.tool_id.clone()).unwrap_or_default();
        Some(ToolCallChunk::streaming_delta(id, tool_name, "", tool_order))
    }

    fn on_content_block_delta(&mut self, idx: usize, delta: AnthropicSseDelta) -> Option<DeltaOutcome> {
        match delta.kind.as_str() {
            "text_delta" => {
                let text = delta.text.unwrap_or_default();
                if !text.is_empty() {
                    return Some(DeltaOutcome::Text(text));
                }
            }
            "thinking_delta" => {
                let text = delta.thinking.or(delta.text).unwrap_or_default();
                if !text.is_empty() {
                    return Some(DeltaOutcome::Reasoning(text));
                }
            }
            "input_json_delta" => {
                let frag = delta.partial_json.unwrap_or_default();
                let entry = self.blocks.get_mut(&idx)?;
                if entry.kind != "tool_use" || entry.tool_name.is_empty() {
                    return None;
                }
                entry.tool_args.push_str(&frag);
                // S3 T3-2-A: emit incremental Streaming chunk carrying the
                // raw partial_json fragment. `args` stays empty per T3-0
                // protocol invariants.
                let chunk = ToolCallChunk {
                    id: entry.tool_id.clone(),
                    name: entry.tool_name.clone(),
                    args: String::new(),
                    index: entry.tool_order,
                    arguments_delta: Some(frag),
                    status: ToolCallChunkStatus::Streaming,
                };
                return Some(DeltaOutcome::ToolDelta(chunk));
            }
            _ => {}
        }
        None
    }

    fn on_content_block_stop(&mut self, idx: usize) -> Option<ToolCallChunk> {
        let entry = self.blocks.remove(&idx)?;
        if entry.kind != "tool_use" {
            return None;
        }
        if entry.tool_name.is_empty() {
            return None;
        }
        let args = if entry.tool_args.is_empty() {
            "{}".to_string()
        } else {
            entry.tool_args
        };
        // S3 T3-2-A: terminal `Completed` chunk. The driver aggregator uses
        // this to flush — and to validate that the accumulated Streaming
        // deltas concatenate to the same JSON string.
        Some(ToolCallChunk {
            id: entry.tool_id,
            name: entry.tool_name,
            args,
            index: entry.tool_order,
            arguments_delta: None,
            status: ToolCallChunkStatus::Completed,
        })
    }

    /// Defensive tail flush invoked at stream EOF when no `content_block_stop`
    /// (and no `message_stop`) ever arrived for one or more in-flight tool_use
    /// blocks. Drains every remaining block and returns a terminal `Completed`
    /// chunk for each well-formed tool_use entry so the driver does not silently
    /// lose tool calls on truncated streams. Non-tool / unnamed blocks are
    /// discarded. The returned chunks preserve their original `index` so the
    /// driver can still reconcile them with the prior Streaming deltas.
    fn drain_pending_tool_calls(&mut self) -> Vec<ToolCallChunk> {
        let mut completed: Vec<ToolCallChunk> = self
            .blocks
            .drain()
            .filter_map(|(_, entry)| {
                if entry.kind != "tool_use" || entry.tool_name.is_empty() {
                    return None;
                }
                let args = if entry.tool_args.is_empty() {
                    "{}".to_string()
                } else {
                    entry.tool_args
                };
                Some(ToolCallChunk {
                    id: entry.tool_id,
                    name: entry.tool_name,
                    args,
                    index: entry.tool_order,
                    arguments_delta: None,
                    status: ToolCallChunkStatus::Completed,
                })
            })
            .collect();
        // HashMap iteration order is non-deterministic; sort by the stable
        // `tool_order` so callers and tests observe a predictable sequence.
        completed.sort_by_key(|c| c.index);
        completed
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DeltaOutcome {
    Text(String),
    Reasoning(String),
    /// **S3 T3-2-A**: a `Streaming` tool-call chunk carrying an
    /// `arguments_delta` fragment.
    ToolDelta(ToolCallChunk),
}

#[derive(Debug, PartialEq, Eq)]
enum AnthropicEvent {
    /// `event: message_start` with optional initial usage.
    MessageStart(AnthropicUsage),
    /// `event: message_delta` with optional cumulative output usage.
    MessageDelta(AnthropicUsage),
    /// `event: content_block_start` with parsed payload index + block.
    BlockStart(usize, AnthropicSseContentBlock),
    /// `event: content_block_delta` index + delta.
    BlockDelta(usize, AnthropicSseDelta),
    /// `event: content_block_stop` index.
    BlockStop(usize),
    /// `event: message_stop` — terminates the turn.
    MessageStop,
    /// Any other event (ping, etc.) — ignored.
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct MessageStartPayload {
    #[serde(default)]
    message: MessageStartBody,
}

#[derive(Debug, Default, Deserialize)]
struct MessageStartBody {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct MessageDeltaPayload {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct BlockStartPayload {
    index: usize,
    content_block: AnthropicSseContentBlock,
}

#[derive(Debug, Deserialize)]
struct BlockDeltaPayload {
    index: usize,
    delta: AnthropicSseDelta,
}

#[derive(Debug, Deserialize)]
struct BlockStopPayload {
    index: usize,
}

/// Parse a single Anthropic SSE record into a structured event. The record
/// consists of one or more `event:`/`data:` lines (no blank-line separator
/// inside the record). Returns `Ok(None)` when the record has no `data:` line.
fn parse_anthropic_sse_record(record: &str) -> StreamResult<Option<AnthropicEvent>> {
    let mut event_name: Option<&str> = None;
    let mut data_buf = String::new();
    for line in record.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = Some(rest.trim());
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(rest.trim());
        }
    }
    if data_buf.is_empty() {
        return Ok(None);
    }
    let event_name = event_name.unwrap_or("");
    match event_name {
        "message_start" => {
            let payload: MessageStartPayload = serde_json::from_str(&data_buf).unwrap_or_default();
            Ok(Some(AnthropicEvent::MessageStart(
                payload.message.usage.unwrap_or_default(),
            )))
        }
        "message_delta" => {
            let payload: MessageDeltaPayload = serde_json::from_str(&data_buf).unwrap_or_default();
            Ok(Some(AnthropicEvent::MessageDelta(payload.usage.unwrap_or_default())))
        }
        "content_block_start" => {
            let payload: BlockStartPayload = serde_json::from_str(&data_buf).map_err(StreamError::Json)?;
            Ok(Some(AnthropicEvent::BlockStart(payload.index, payload.content_block)))
        }
        "content_block_delta" => {
            let payload: BlockDeltaPayload = serde_json::from_str(&data_buf).map_err(StreamError::Json)?;
            Ok(Some(AnthropicEvent::BlockDelta(payload.index, payload.delta)))
        }
        "content_block_stop" => {
            let payload: BlockStopPayload = serde_json::from_str(&data_buf).map_err(StreamError::Json)?;
            Ok(Some(AnthropicEvent::BlockStop(payload.index)))
        }
        "message_stop" => Ok(Some(AnthropicEvent::MessageStop)),
        _ => Ok(Some(AnthropicEvent::Other)),
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let credential = self.ensure_fresh_credential()?;

        let body = ChatRequest {
            model: model.to_string(),
            max_tokens: 4096,
            system: system_prompt.map(ToString::to_string),
            messages: vec![Message {
                role: "user".to_string(),
                content: message.to_string(),
            }],
            temperature,
        };

        let req = self
            .http_client()
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body);

        let response = self.apply_auth(req, &credential).send().await?;

        // Handle 401: try one refresh + retry before failing.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Ok(new_credential) = self.try_refresh_after_401() {
                let retry_req = self
                    .http_client()
                    .post(format!("{}/v1/messages", self.base_url))
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(&body);
                let retry_response = self.apply_auth(retry_req, &new_credential).send().await?;
                if !retry_response.status().is_success() {
                    return Err(super::api_error("Anthropic", retry_response).await);
                }
                let chat_response: ChatResponse = retry_response.json().await?;
                return Self::parse_text_response(chat_response);
            }
            return Err(super::api_error("Anthropic", response).await);
        }

        if !response.status().is_success() {
            return Err(super::api_error("Anthropic", response).await);
        }

        let chat_response: ChatResponse = response.json().await?;
        Self::parse_text_response(chat_response)
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        self.chat_metered(request, model, temperature)
            .await
            .map(|(response, _)| response)
    }

    async fn chat_traced(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatTrace> {
        let started_at = chrono::Utc::now();
        let (response, tokens_used) = self.chat_metered(request, model, temperature).await?;
        let finished_at = chrono::Utc::now();
        Ok(ChatTrace {
            response,
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: "anthropic".to_string(),
                model: model.to_string(),
                started_at,
                finished_at,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "anthropic".to_string(),
            final_model: model.to_string(),
            tokens_used,
        })
    }

    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            ..Default::default()
        }
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        // Convert OpenAI-format tool JSON to ToolSpec so we can reuse the
        // existing `chat()` method which handles full message history,
        // system prompt extraction, caching, and Anthropic native formatting.
        let tool_specs: Vec<ToolSpec> = tools
            .iter()
            .filter_map(|t| {
                let func = t.get("function").or_else(|| {
                    tracing::warn!("Skipping malformed tool definition (missing 'function' key)");
                    None
                })?;
                let name = func.get("name").and_then(|n| n.as_str()).or_else(|| {
                    tracing::warn!("Skipping tool with missing or non-string 'name'");
                    None
                })?;
                Some(ToolSpec {
                    name: name.to_string(),
                    description: func
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    parameters: func
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                })
            })
            .collect();

        let request = ProviderChatRequest {
            messages,
            tools: if tool_specs.is_empty() { None } else { Some(&tool_specs) },
        };
        self.chat(request, model, temperature).await
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        let credential = self.oauth.lock().credential.clone();
        if let Some(credential) = credential {
            let mut request = self
                .http_client()
                .post(format!("{}/v1/messages", self.base_url))
                .header("anthropic-version", "2023-06-01");
            request = self.apply_auth(request, &credential);
            // Send a minimal request; the goal is TLS + HTTP/2 setup, not a valid response.
            // Anthropic has no lightweight GET endpoint, so we accept any non-network error.
            let _ = request.send().await?;
        }
        Ok(())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    /// **5a-7a**: Native Anthropic Messages API streaming.
    ///
    /// Sends `stream: true` to `/v1/messages`, parses the event-driven SSE
    /// stream via [`parse_anthropic_sse_record`], aggregates `input_json_delta`
    /// fragments per content_block, and emits a [`ToolCallChunk`] on each
    /// `content_block_stop` whose block is `tool_use`.
    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        let credential = match self.ensure_fresh_credential() {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                return stream::once(async move { Err(StreamError::Provider(msg)) }).boxed();
            }
        };

        let (system_prompt, native_messages) = Self::convert_messages(messages);
        // Anthropic tools API is not surfaced over the legacy
        // `stream_chat_with_history(messages, model, ..)` signature; the
        // driver passes tools through `EffectDeps.tools_registry` and we
        // serialise assistant tool_call / tool_result back into history. So we
        // *also* need to send the tool catalogue on every streaming call —
        // resolve it from the global registry hook below. Until tools are
        // threaded through the trait, we omit tools here; chat::run still
        // injects native tool schema through the legacy chat_with_tools path
        // when needed. driver path will operate without native schema until
        // step 5a-7b adds tools to the streaming signature.
        let tools: Option<Vec<OwnedNativeToolSpec>> = None;

        let url = format!("{}/v1/messages", self.base_url);
        let client = self.http_client();
        let use_bearer = Self::is_setup_token(&credential);

        let request_body = StreamingChatRequest {
            model: model.to_string(),
            max_tokens: 4096,
            system: system_prompt,
            messages: native_messages,
            temperature,
            tools,
            stream: true,
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(64);

        tokio::spawn(async move {
            let mut req = client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .json(&request_body);
            req = if use_bearer {
                req.header("Authorization", format!("Bearer {credential}"))
                    .header("anthropic-beta", "oauth-2025-04-20")
            } else {
                req.header("x-api-key", &credential)
            };

            let response = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
                    return;
                }
            };

            if !response.status().is_success() {
                let _ = tx.send(Err(super::stream_api_error("Anthropic", response).await)).await;
                return;
            }

            let mut state = AnthropicStreamState::default();
            let mut usage_state = AnthropicUsage::default();
            let mut usage_seen = false;
            let mut byte_stream = response.bytes_stream();
            let mut buf = String::new();
            let mut completion_chars: usize = 0;
            let mut sent_final = false;

            'outer: while let Some(bytes_res) = byte_stream.next().await {
                let bytes = match bytes_res {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(StreamError::Http(e))).await;
                        return;
                    }
                };
                let text = match std::str::from_utf8(&bytes) {
                    Ok(t) => t.to_string(),
                    Err(e) => {
                        let _ = tx
                            .send(Err(StreamError::InvalidSse(format!(
                                "non-utf8 byte in Anthropic SSE: {e}"
                            ))))
                            .await;
                        return;
                    }
                };
                buf.push_str(&text);

                // SSE records are separated by blank lines (`\n\n`).
                while let Some(end) = buf.find("\n\n") {
                    let record: String = buf.drain(..end + 2).collect();
                    let event = match parse_anthropic_sse_record(&record) {
                        Ok(Some(ev)) => ev,
                        Ok(None) => continue,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    };
                    match event {
                        AnthropicEvent::MessageStart(usage) => {
                            if usage.input_tokens.is_some() || usage.output_tokens.is_some() {
                                usage_seen = true;
                                if usage.input_tokens.is_some() {
                                    usage_state.input_tokens = usage.input_tokens;
                                }
                                if usage.output_tokens.is_some() {
                                    usage_state.output_tokens = usage.output_tokens;
                                }
                            }
                        }
                        AnthropicEvent::MessageDelta(usage) => {
                            if usage.input_tokens.is_some() || usage.output_tokens.is_some() {
                                usage_seen = true;
                                if usage.input_tokens.is_some() {
                                    usage_state.input_tokens = usage.input_tokens;
                                }
                                if usage.output_tokens.is_some() {
                                    usage_state.output_tokens = usage.output_tokens;
                                }
                            }
                        }
                        AnthropicEvent::BlockStart(idx, block) => {
                            if let Some(initial) = state.on_content_block_start(idx, block) {
                                if tx.send(Ok(StreamChunk::tool_call_chunk(vec![initial]))).await.is_err() {
                                    return;
                                }
                            }
                        }
                        AnthropicEvent::BlockDelta(idx, delta) => {
                            if let Some(outcome) = state.on_content_block_delta(idx, delta) {
                                let chunk = match outcome {
                                    DeltaOutcome::Text(t) => {
                                        completion_chars = completion_chars.saturating_add(t.chars().count());
                                        let mut c = StreamChunk::delta(t);
                                        if options.count_tokens {
                                            c = c.with_token_estimate();
                                        }
                                        c
                                    }
                                    DeltaOutcome::Reasoning(r) => {
                                        completion_chars = completion_chars.saturating_add(r.chars().count());
                                        let mut c = StreamChunk::reasoning_delta(r);
                                        if options.count_tokens {
                                            c = c.with_token_estimate();
                                        }
                                        c
                                    }
                                    DeltaOutcome::ToolDelta(tool_chunk) => {
                                        // Tool-call chunks are never token-counted —
                                        // they don't carry user-visible text.
                                        StreamChunk::tool_call_chunk(vec![tool_chunk])
                                    }
                                };
                                if tx.send(Ok(chunk)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        AnthropicEvent::BlockStop(idx) => {
                            if let Some(call) = state.on_content_block_stop(idx) {
                                if tx.send(Ok(StreamChunk::tool_call_chunk(vec![call]))).await.is_err() {
                                    return;
                                }
                            }
                        }
                        AnthropicEvent::MessageStop => {
                            if usage_seen {
                                if tx
                                    .send(Ok(StreamChunk::usage(usage_state.into_reported())))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            } else if completion_chars > 0 {
                                let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
                                let usage = accumulator.finish_or_estimate_completion_chars(completion_chars);
                                if tx.send(Ok(StreamChunk::usage(usage))).await.is_err() {
                                    return;
                                }
                            }
                            let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
                            sent_final = true;
                            break 'outer;
                        }
                        AnthropicEvent::Other => {}
                    }
                }
            }

            if !sent_final {
                // Defensive tail flush: if the upstream closed the byte stream
                // without ever sending `content_block_stop` (and `message_stop`)
                // for in-flight tool_use blocks, drain them now so the driver
                // gets the terminal `Completed` chunks it needs to finalise the
                // turn. Without this, partial tool calls would be lost on EOF.
                for call in state.drain_pending_tool_calls() {
                    if tx.send(Ok(StreamChunk::tool_call_chunk(vec![call]))).await.is_err() {
                        return;
                    }
                }
                if usage_seen {
                    if tx
                        .send(Ok(StreamChunk::usage(usage_state.into_reported())))
                        .await
                        .is_err()
                    {
                        return;
                    }
                } else if completion_chars > 0 {
                    let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
                    let usage = accumulator.finish_or_estimate_completion_chars(completion_chars);
                    if tx.send(Ok(StreamChunk::usage(usage))).await.is_err() {
                        return;
                    }
                }
                let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
            }
        });

        stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
impl AnthropicProvider {
    fn credential(&self) -> Option<String> {
        self.oauth.lock().credential.clone()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
    use super::*;
    use crate::auth::anthropic_token::{AnthropicAuthKind, detect_auth_kind};

    #[test]
    fn creates_with_key() {
        let p = AnthropicProvider::new(Some("anthropic-test-credential"));
        assert!(p.credential().is_some());
        assert_eq!(p.credential().as_deref(), Some("anthropic-test-credential"));
        assert_eq!(p.base_url, "https://api.anthropic.com");
    }

    #[test]
    fn creates_without_key() {
        let p = AnthropicProvider::new(None);
        assert!(p.credential().is_none());
        assert_eq!(p.base_url, "https://api.anthropic.com");
    }

    #[test]
    fn creates_with_empty_key() {
        let p = AnthropicProvider::new(Some(""));
        assert!(p.credential().is_none());
    }

    #[test]
    fn creates_with_whitespace_key() {
        let p = AnthropicProvider::new(Some("  anthropic-test-credential  "));
        assert!(p.credential().is_some());
        assert_eq!(p.credential().as_deref(), Some("anthropic-test-credential"));
    }

    #[test]
    fn creates_with_custom_base_url() {
        let p = AnthropicProvider::with_base_url(Some("anthropic-credential"), Some("https://api.example.com"));
        assert_eq!(p.base_url, "https://api.example.com");
        assert_eq!(p.credential().as_deref(), Some("anthropic-credential"));
    }

    #[test]
    fn custom_base_url_trims_trailing_slash() {
        let p = AnthropicProvider::with_base_url(None, Some("https://api.example.com/"));
        assert_eq!(p.base_url, "https://api.example.com");
    }

    #[test]
    fn default_base_url_when_none_provided() {
        let p = AnthropicProvider::with_base_url(None, None);
        assert_eq!(p.base_url, "https://api.anthropic.com");
    }

    #[tokio::test]
    async fn chat_fails_without_key() {
        let p = AnthropicProvider::new(None);
        let result = p.chat_with_system(None, "hello", "claude-3-opus", 0.7).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("credentials not set"), "Expected key error, got: {err}");
    }

    #[test]
    fn setup_token_detection_works() {
        assert!(AnthropicProvider::is_setup_token("sk-ant-oat01-abcdef"));
        assert!(!AnthropicProvider::is_setup_token("sk-ant-api-key"));
    }

    #[test]
    fn apply_auth_uses_bearer_and_beta_for_setup_tokens() {
        let provider = AnthropicProvider::new(None);
        let request = provider
            .apply_auth(
                provider.http_client().get("https://api.anthropic.com/v1/models"),
                "sk-ant-oat01-test-token",
            )
            .build()
            .expect("request should build");

        assert_eq!(
            request.headers().get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer sk-ant-oat01-test-token")
        );
        assert_eq!(
            request.headers().get("anthropic-beta").and_then(|v| v.to_str().ok()),
            Some("oauth-2025-04-20")
        );
        assert!(request.headers().get("x-api-key").is_none());
    }

    #[test]
    fn apply_auth_uses_x_api_key_for_regular_tokens() {
        let provider = AnthropicProvider::new(None);
        let request = provider
            .apply_auth(
                provider.http_client().get("https://api.anthropic.com/v1/models"),
                "sk-ant-api-key",
            )
            .build()
            .expect("request should build");

        assert_eq!(
            request.headers().get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("sk-ant-api-key")
        );
        assert!(request.headers().get("authorization").is_none());
        assert!(request.headers().get("anthropic-beta").is_none());
    }

    #[tokio::test]
    async fn chat_with_system_fails_without_key() {
        let p = AnthropicProvider::new(None);
        let result = p
            .chat_with_system(Some("You are OpenPRX"), "hello", "claude-3-opus", 0.7)
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn chat_request_serializes_without_system() {
        let req = ChatRequest {
            model: "claude-3-opus".to_string(),
            max_tokens: 4096,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            temperature: 0.7,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("system"), "system field should be skipped when None");
        assert!(json.contains("claude-3-opus"));
        assert!(json.contains("hello"));
    }

    #[test]
    fn chat_request_serializes_with_system() {
        let req = ChatRequest {
            model: "claude-3-opus".to_string(),
            max_tokens: 4096,
            system: Some("You are OpenPRX".to_string()),
            messages: vec![Message {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            temperature: 0.7,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"system\":\"You are OpenPRX\""));
    }

    #[test]
    fn chat_response_deserializes() {
        let json = r#"{"content":[{"type":"text","text":"Hello there!"}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content.len(), 1);
        assert_eq!(resp.content[0].kind, "text");
        assert_eq!(resp.content[0].text.as_deref(), Some("Hello there!"));
    }

    #[test]
    fn chat_response_empty_content() {
        let json = r#"{"content":[]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.content.is_empty());
    }

    #[test]
    fn chat_response_multiple_blocks() {
        let json = r#"{"content":[{"type":"text","text":"First"},{"type":"text","text":"Second"}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert_eq!(resp.content[0].text.as_deref(), Some("First"));
        assert_eq!(resp.content[1].text.as_deref(), Some("Second"));
    }

    #[test]
    fn temperature_range_serializes() {
        for temp in [0.0, 0.5, 1.0, 2.0] {
            let req = ChatRequest {
                model: "claude-3-opus".to_string(),
                max_tokens: 4096,
                system: None,
                messages: vec![],
                temperature: temp,
            };
            let json = serde_json::to_string(&req).unwrap();
            assert!(json.contains(&format!("{temp}")));
        }
    }

    #[test]
    fn detects_auth_from_jwt_shape() {
        let kind = detect_auth_kind("a.b.c", None);
        assert_eq!(kind, AnthropicAuthKind::Authorization);
    }

    #[test]
    fn cache_control_serializes_correctly() {
        let cache = CacheControl::ephemeral();
        let json = serde_json::to_string(&cache).unwrap();
        assert_eq!(json, r#"{"type":"ephemeral"}"#);
    }

    #[test]
    fn system_prompt_string_variant_serializes() {
        let prompt = SystemPrompt::String("You are a helpful assistant".to_string());
        let json = serde_json::to_string(&prompt).unwrap();
        assert_eq!(json, r#""You are a helpful assistant""#);
    }

    #[test]
    fn system_prompt_blocks_variant_serializes() {
        let prompt = SystemPrompt::Blocks(vec![SystemBlock {
            block_type: "text".to_string(),
            text: "You are a helpful assistant".to_string(),
            cache_control: Some(CacheControl::ephemeral()),
        }]);
        let json = serde_json::to_string(&prompt).unwrap();
        assert!(json.contains(r#""type":"text""#));
        assert!(json.contains("You are a helpful assistant"));
        assert!(json.contains(r#""type":"ephemeral""#));
    }

    #[test]
    fn system_prompt_blocks_without_cache_control() {
        let prompt = SystemPrompt::Blocks(vec![SystemBlock {
            block_type: "text".to_string(),
            text: "Short prompt".to_string(),
            cache_control: None,
        }]);
        let json = serde_json::to_string(&prompt).unwrap();
        assert!(json.contains("Short prompt"));
        assert!(!json.contains("cache_control"));
    }

    #[test]
    fn native_content_text_without_cache_control() {
        let content = NativeContentOut::Text {
            text: "Hello".to_string(),
            cache_control: None,
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains(r#""type":"text""#));
        assert!(json.contains("Hello"));
        assert!(!json.contains("cache_control"));
    }

    #[test]
    fn native_content_text_with_cache_control() {
        let content = NativeContentOut::Text {
            text: "Hello".to_string(),
            cache_control: Some(CacheControl::ephemeral()),
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains(r#""type":"text""#));
        assert!(json.contains("Hello"));
        assert!(json.contains(r#""cache_control":{"type":"ephemeral"}"#));
    }

    #[test]
    fn native_content_tool_use_without_cache_control() {
        let content = NativeContentOut::ToolUse {
            id: "tool_123".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"location": "San Francisco"}),
            cache_control: None,
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains(r#""type":"tool_use""#));
        assert!(json.contains("tool_123"));
        assert!(json.contains("get_weather"));
        assert!(!json.contains("cache_control"));
    }

    #[test]
    fn native_content_tool_result_with_cache_control() {
        let content = NativeContentOut::ToolResult {
            tool_use_id: "tool_123".to_string(),
            content: "Result data".to_string(),
            cache_control: Some(CacheControl::ephemeral()),
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains(r#""type":"tool_result""#));
        assert!(json.contains("tool_123"));
        assert!(json.contains("Result data"));
        assert!(json.contains(r#""cache_control":{"type":"ephemeral"}"#));
    }

    #[test]
    fn native_tool_spec_without_cache_control() {
        let schema = serde_json::json!({"type": "object"});
        let tool = NativeToolSpec {
            name: "get_weather",
            description: "Get weather info",
            input_schema: schema,
            cache_control: None,
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("get_weather"));
        assert!(!json.contains("cache_control"));
    }

    #[test]
    fn native_tool_spec_with_cache_control() {
        let schema = serde_json::json!({"type": "object"});
        let tool = NativeToolSpec {
            name: "get_weather",
            description: "Get weather info",
            input_schema: schema,
            cache_control: Some(CacheControl::ephemeral()),
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("get_weather"));
        assert!(json.contains(r#""cache_control":{"type":"ephemeral"}"#));
    }

    #[test]
    fn should_cache_system_small_prompt() {
        let small_prompt = "You are a helpful assistant.";
        assert!(!AnthropicProvider::should_cache_system(small_prompt));
    }

    #[test]
    fn should_cache_system_large_prompt() {
        let large_prompt = "a".repeat(3073); // Just over 3072 bytes
        assert!(AnthropicProvider::should_cache_system(&large_prompt));
    }

    #[test]
    fn should_cache_system_boundary() {
        let boundary_prompt = "a".repeat(3072); // Exactly 3072 bytes
        assert!(!AnthropicProvider::should_cache_system(&boundary_prompt));

        let over_boundary = "a".repeat(3073);
        assert!(AnthropicProvider::should_cache_system(&over_boundary));
    }

    #[test]
    fn should_cache_conversation_short() {
        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: "System prompt".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "Hello".to_string(),
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: "Hi".to_string(),
            },
        ];
        // Only 2 non-system messages
        assert!(!AnthropicProvider::should_cache_conversation(&messages));
    }

    #[test]
    fn should_cache_conversation_long() {
        let mut messages = vec![ChatMessage {
            role: "system".to_string(),
            content: "System prompt".to_string(),
        }];
        // Add 5 non-system messages
        for i in 0..5 {
            messages.push(ChatMessage {
                role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                content: format!("Message {i}"),
            });
        }
        assert!(AnthropicProvider::should_cache_conversation(&messages));
    }

    #[test]
    fn should_cache_conversation_boundary() {
        let mut messages = vec![];
        // Add exactly 4 non-system messages
        for i in 0..4 {
            messages.push(ChatMessage {
                role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                content: format!("Message {i}"),
            });
        }
        assert!(!AnthropicProvider::should_cache_conversation(&messages));

        // Add one more to cross boundary
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: "One more".to_string(),
        });
        assert!(AnthropicProvider::should_cache_conversation(&messages));
    }

    #[test]
    fn apply_cache_to_last_message_text() {
        let mut messages = vec![NativeMessage {
            role: "user".to_string(),
            content: vec![NativeContentOut::Text {
                text: "Hello".to_string(),
                cache_control: None,
            }],
        }];

        AnthropicProvider::apply_cache_to_last_message(&mut messages);

        match &messages[0].content[0] {
            NativeContentOut::Text { cache_control, .. } => {
                assert!(cache_control.is_some());
            }
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn apply_cache_to_last_message_tool_result() {
        let mut messages = vec![NativeMessage {
            role: "user".to_string(),
            content: vec![NativeContentOut::ToolResult {
                tool_use_id: "tool_123".to_string(),
                content: "Result".to_string(),
                cache_control: None,
            }],
        }];

        AnthropicProvider::apply_cache_to_last_message(&mut messages);

        match &messages[0].content[0] {
            NativeContentOut::ToolResult { cache_control, .. } => {
                assert!(cache_control.is_some());
            }
            _ => panic!("Expected ToolResult variant"),
        }
    }

    #[test]
    fn apply_cache_to_last_message_does_not_affect_tool_use() {
        let mut messages = vec![NativeMessage {
            role: "assistant".to_string(),
            content: vec![NativeContentOut::ToolUse {
                id: "tool_123".to_string(),
                name: "get_weather".to_string(),
                input: serde_json::json!({}),
                cache_control: None,
            }],
        }];

        AnthropicProvider::apply_cache_to_last_message(&mut messages);

        // ToolUse should not be affected
        match &messages[0].content[0] {
            NativeContentOut::ToolUse { cache_control, .. } => {
                assert!(cache_control.is_none());
            }
            _ => panic!("Expected ToolUse variant"),
        }
    }

    #[test]
    fn apply_cache_empty_messages() {
        let mut messages = vec![];
        AnthropicProvider::apply_cache_to_last_message(&mut messages);
        // Should not panic
        assert!(messages.is_empty());
    }

    #[test]
    fn convert_tools_adds_cache_to_last_tool() {
        let tools = vec![
            ToolSpec {
                name: "tool1".to_string(),
                description: "First tool".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolSpec {
                name: "tool2".to_string(),
                description: "Second tool".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ];

        let native_tools = AnthropicProvider::convert_tools(Some(&tools)).unwrap();

        assert_eq!(native_tools.len(), 2);
        assert!(native_tools[0].cache_control.is_none());
        assert!(native_tools[1].cache_control.is_some());
    }

    #[test]
    fn convert_tools_single_tool_gets_cache() {
        let tools = vec![ToolSpec {
            name: "tool1".to_string(),
            description: "Only tool".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let native_tools = AnthropicProvider::convert_tools(Some(&tools)).unwrap();

        assert_eq!(native_tools.len(), 1);
        assert!(native_tools[0].cache_control.is_some());
    }

    #[test]
    fn convert_messages_small_system_prompt() {
        let messages = vec![ChatMessage {
            role: "system".to_string(),
            content: "Short system prompt".to_string(),
        }];

        let (system_prompt, _) = AnthropicProvider::convert_messages(&messages);

        match system_prompt.unwrap() {
            SystemPrompt::String(s) => {
                assert_eq!(s, "Short system prompt");
            }
            SystemPrompt::Blocks(_) => panic!("Expected String variant for small prompt"),
        }
    }

    #[test]
    fn convert_messages_large_system_prompt() {
        let large_content = "a".repeat(3073);
        let messages = vec![ChatMessage {
            role: "system".to_string(),
            content: large_content.clone(),
        }];

        let (system_prompt, _) = AnthropicProvider::convert_messages(&messages);

        match system_prompt.unwrap() {
            SystemPrompt::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].text, large_content);
                assert!(blocks[0].cache_control.is_some());
            }
            SystemPrompt::String(_) => panic!("Expected Blocks variant for large prompt"),
        }
    }

    #[test]
    fn backward_compatibility_native_chat_request() {
        // Test that requests without cache_control serialize identically to old format
        let req = NativeChatRequest {
            model: "claude-3-opus".to_string(),
            max_tokens: 4096,
            system: Some(SystemPrompt::String("System".to_string())),
            messages: vec![NativeMessage {
                role: "user".to_string(),
                content: vec![NativeContentOut::Text {
                    text: "Hello".to_string(),
                    cache_control: None,
                }],
            }],
            temperature: 0.7,
            tools: None,
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("cache_control"));
        assert!(json.contains(r#""system":"System""#));
    }

    #[tokio::test]
    async fn warmup_without_key_is_noop() {
        let provider = AnthropicProvider::new(None);
        let result = provider.warmup().await;
        assert!(result.is_ok());
    }

    #[test]
    fn convert_messages_preserves_multi_turn_history() {
        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: "You are helpful.".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "gen a 2 sum in golang".to_string(),
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: "```go\nfunc twoSum(nums []int) {}\n```".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "what's meaning of make here?".to_string(),
            },
        ];

        let (system, native_msgs) = AnthropicProvider::convert_messages(&messages);

        // System prompt extracted
        assert!(system.is_some());
        // All 3 non-system messages preserved in order
        assert_eq!(native_msgs.len(), 3);
        assert_eq!(native_msgs[0].role, "user");
        assert_eq!(native_msgs[1].role, "assistant");
        assert_eq!(native_msgs[2].role, "user");
    }

    #[test]
    fn convert_messages_splits_user_text_and_image_blocks() {
        let messages = vec![ChatMessage::user("Look [IMAGE:data:image/png;base64,abcd==] now")];

        let (_, native_msgs) = AnthropicProvider::convert_messages(&messages);

        assert_eq!(native_msgs.len(), 1);
        assert_eq!(native_msgs[0].role, "user");
        assert_eq!(native_msgs[0].content.len(), 3);
        match &native_msgs[0].content[0] {
            NativeContentOut::Text { text, .. } => assert_eq!(text, "Look "),
            other => panic!("expected text block, got {other:?}"),
        }
        match &native_msgs[0].content[1] {
            NativeContentOut::Image { source } => {
                assert_eq!(source.source_type, "base64");
                assert_eq!(source.media_type, "image/png");
                assert_eq!(source.data, "abcd==");
            }
            other => panic!("expected image block, got {other:?}"),
        }
        match &native_msgs[0].content[2] {
            NativeContentOut::Text { text, .. } => assert_eq!(text, " now"),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn parse_anthropic_image_source_strips_whitespace_from_base64_payload() {
        let source = AnthropicProvider::parse_anthropic_image_source("data:image/png;base64,Zm9v\n YmFy\t")
            .expect("expected valid image source");

        assert_eq!(source.media_type, "image/png");
        assert_eq!(source.data, "Zm9vYmFy");
    }

    /// Integration test: spin up a mock Anthropic API server, call chat_with_tools
    /// with a multi-turn conversation + tools, and verify the request body contains
    /// ALL conversation turns and native tool definitions.
    #[tokio::test]
    async fn chat_with_tools_sends_full_history_and_native_tools() {
        use axum::{Json, Router, routing::post};
        use std::sync::{Arc, Mutex};
        use tokio::net::TcpListener;

        // Captured request body for assertion
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();

        let app = Router::new().route(
            "/v1/messages",
            post(move |Json(body): Json<serde_json::Value>| {
                let cap = captured_clone.clone();
                async move {
                    *cap.lock().unwrap() = Some(body);
                    // Return a minimal valid Anthropic response
                    Json(serde_json::json!({
                        "id": "msg_test",
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "The make function creates a map."}],
                        "model": "claude-opus-4-6",
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 100, "output_tokens": 20}
                    }))
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Create provider pointing at mock server
        let provider = AnthropicProvider::with_base_url(Some("test-key"), Some(&format!("http://{addr}")));

        // Multi-turn conversation: system → user (Go code) → assistant (code response) → user (follow-up)
        let messages = vec![
            ChatMessage::system("You are a helpful assistant."),
            ChatMessage::user("gen a 2 sum in golang"),
            ChatMessage::assistant(
                "```go\nfunc twoSum(nums []int, target int) []int {\n    m := make(map[int]int)\n    for i, n := range nums {\n        if j, ok := m[target-n]; ok {\n            return []int{j, i}\n        }\n        m[n] = i\n    }\n    return nil\n}\n```",
            ),
            ChatMessage::user("what's meaning of make here?"),
        ];

        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a shell command",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"}
                    },
                    "required": ["command"]
                }
            }
        })];

        let result = provider
            .chat_with_tools(&messages, &tools, "claude-opus-4-6", 0.7)
            .await;
        assert!(result.is_ok(), "chat_with_tools failed: {:?}", result.err());

        let body = captured.lock().unwrap().take().expect("No request captured");

        // Verify system prompt extracted to top-level field
        let system = &body["system"];
        assert!(
            system.to_string().contains("helpful assistant"),
            "System prompt missing: {system}"
        );

        // Verify ALL conversation turns present in messages array
        let msgs = body["messages"].as_array().expect("messages not an array");
        assert_eq!(
            msgs.len(),
            3,
            "Expected 3 messages (2 user + 1 assistant), got {}",
            msgs.len()
        );

        // Turn 1: user with Go request
        assert_eq!(msgs[0]["role"], "user");
        let turn1_text = msgs[0]["content"].to_string();
        assert!(turn1_text.contains("2 sum"), "Turn 1 missing Go request: {turn1_text}");

        // Turn 2: assistant with Go code
        assert_eq!(msgs[1]["role"], "assistant");
        let turn2_text = msgs[1]["content"].to_string();
        assert!(
            turn2_text.contains("make(map[int]int)"),
            "Turn 2 missing Go code: {turn2_text}"
        );

        // Turn 3: user follow-up
        assert_eq!(msgs[2]["role"], "user");
        let turn3_text = msgs[2]["content"].to_string();
        assert!(
            turn3_text.contains("meaning of make"),
            "Turn 3 missing follow-up: {turn3_text}"
        );

        // Verify native tools are present
        let api_tools = body["tools"].as_array().expect("tools not an array");
        assert_eq!(api_tools.len(), 1);
        assert_eq!(api_tools[0]["name"], "shell");
        assert!(api_tools[0]["input_schema"].is_object(), "Missing input_schema");

        server_handle.abort();
    }

    // -----------------------------------------------------------------
    // Reasoning/thinking block separation tests
    // -----------------------------------------------------------------

    #[test]
    fn parse_native_response_routes_thinking_block_to_reasoning_field() {
        // Anthropic extended-thinking returns a content block of type "thinking"
        // with the chain-of-thought in the `thinking` field. It must NOT be
        // mixed into the visible `text` payload.
        let json = r#"{
            "content": [
                {"type": "thinking", "thinking": "Let me reason about this..."},
                {"type": "text", "text": "Final answer."}
            ]
        }"#;
        let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
        let parsed = AnthropicProvider::parse_native_response(resp);

        // Visible text only — no thinking content leaks in.
        assert_eq!(parsed.text.as_deref(), Some("Final answer."));
        assert_eq!(parsed.reasoning_content.as_deref(), Some("Let me reason about this..."));
        assert!(parsed.tool_calls.is_empty());
    }

    #[test]
    fn parse_native_response_handles_thinking_only_without_text() {
        let json = r#"{
            "content": [
                {"type": "thinking", "thinking": "Internal monologue only."}
            ]
        }"#;
        let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
        let parsed = AnthropicProvider::parse_native_response(resp);

        // No visible text at all.
        assert_eq!(parsed.text, None);
        assert_eq!(parsed.reasoning_content.as_deref(), Some("Internal monologue only."));
    }

    #[test]
    fn parse_native_response_with_thinking_and_tool_use() {
        let json = r#"{
            "content": [
                {"type": "thinking", "thinking": "I should call shell."},
                {"type": "tool_use", "id": "tu_1", "name": "shell", "input": {"command": "ls"}}
            ]
        }"#;
        let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
        let parsed = AnthropicProvider::parse_native_response(resp);

        assert_eq!(parsed.text, None);
        assert_eq!(parsed.reasoning_content.as_deref(), Some("I should call shell."));
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "shell");
    }

    #[test]
    fn parse_native_response_no_thinking_keeps_reasoning_none() {
        let json = r#"{
            "content": [
                {"type": "text", "text": "Just an answer."}
            ]
        }"#;
        let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
        let parsed = AnthropicProvider::parse_native_response(resp);

        assert_eq!(parsed.text.as_deref(), Some("Just an answer."));
        assert_eq!(parsed.reasoning_content, None);
    }

    #[test]
    fn anthropic_non_streaming_response_usage_maps_reported() {
        let json = r#"{
            "content": [{"type": "text", "text": "Just an answer."}],
            "usage": {"input_tokens": 64, "output_tokens": 11}
        }"#;
        let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.expect("usage expected").into_reported();

        assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Reported);
        assert_eq!(usage.prompt_tokens, Some(64));
        assert_eq!(usage.completion_tokens, Some(11));
        assert_eq!(usage.total_tokens, Some(75));
    }

    #[test]
    fn anthropic_sse_usage_events_parse_reported_tokens() {
        let start = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":1}}}\n\n"
        );
        let delta = concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":23}}\n\n"
        );

        let start_usage = match parse_anthropic_sse_record(start).unwrap().expect("message_start event") {
            AnthropicEvent::MessageStart(usage) => usage,
            other => panic!("expected message_start usage, got {other:?}"),
        };
        let delta_usage = match parse_anthropic_sse_record(delta).unwrap().expect("message_delta event") {
            AnthropicEvent::MessageDelta(usage) => usage,
            other => panic!("expected message_delta usage, got {other:?}"),
        };

        assert_eq!(start_usage.input_tokens, Some(100));
        assert_eq!(start_usage.output_tokens, Some(1));
        assert_eq!(delta_usage.input_tokens, None);
        assert_eq!(delta_usage.output_tokens, Some(23));

        let reported = AnthropicUsage {
            input_tokens: start_usage.input_tokens,
            output_tokens: delta_usage.output_tokens,
        }
        .into_reported();
        assert_eq!(reported.source, crate::llm::route_decision::TokenUsageSource::Reported);
        assert_eq!(reported.prompt_tokens, Some(100));
        assert_eq!(reported.completion_tokens, Some(23));
        assert_eq!(reported.total_tokens, Some(123));
    }

    // ─── S3 T3-2-A: SSE tool_use incremental streaming ──────────────────────

    /// Build a `tool_use` `content_block_start` payload.
    fn tool_use_start_block(id: &str, name: &str) -> AnthropicSseContentBlock {
        AnthropicSseContentBlock {
            kind: "tool_use".into(),
            id: Some(id.into()),
            name: Some(name.into()),
            text: None,
        }
    }

    /// Build an `input_json_delta` SSE delta.
    fn input_json_delta(partial: &str) -> AnthropicSseDelta {
        AnthropicSseDelta {
            kind: "input_json_delta".into(),
            text: None,
            thinking: None,
            partial_json: Some(partial.into()),
        }
    }

    /// Mock the full SSE sequence for a single tool_use block (start +
    /// multiple input_json_delta + stop) and verify provider emits:
    /// 1) an initial `Streaming { arguments_delta: Some("") }` chunk,
    /// 2) one `Streaming` chunk per partial_json with the verbatim fragment,
    /// 3) a final `Completed` chunk whose `args` equals Σ fragments.
    #[test]
    fn test_tool_use_streaming_emits_incremental_chunks() {
        let mut state = AnthropicStreamState::default();

        // BlockStart → initial Streaming chunk
        let initial = state
            .on_content_block_start(0, tool_use_start_block("tu_42", "shell"))
            .expect("BlockStart for tool_use must emit an initial Streaming chunk");
        assert_eq!(initial.id, "tu_42");
        assert_eq!(initial.name, "shell");
        assert_eq!(initial.index, 0);
        assert_eq!(initial.status, ToolCallChunkStatus::Streaming);
        assert_eq!(initial.arguments_delta.as_deref(), Some(""));
        assert!(initial.args.is_empty(), "Streaming.args MUST be empty");

        // Three input_json_delta events → three Streaming chunks
        let fragments = [r#"{"command":"#, r#" "ls -"#, r#"la"}"#];
        let mut streamed_chunks: Vec<ToolCallChunk> = Vec::new();
        for frag in &fragments {
            let outcome = state
                .on_content_block_delta(0, input_json_delta(frag))
                .expect("input_json_delta must produce a ToolDelta outcome");
            match outcome {
                DeltaOutcome::ToolDelta(chunk) => {
                    assert_eq!(chunk.id, "tu_42");
                    assert_eq!(chunk.name, "shell");
                    assert_eq!(chunk.index, 0);
                    assert_eq!(chunk.status, ToolCallChunkStatus::Streaming);
                    assert!(chunk.args.is_empty(), "Streaming.args MUST be empty");
                    assert_eq!(chunk.arguments_delta.as_deref(), Some(*frag));
                    streamed_chunks.push(chunk);
                }
                other => panic!("expected ToolDelta, got {other:?}"),
            }
        }
        assert_eq!(streamed_chunks.len(), 3);

        // BlockStop → terminal Completed chunk with full JSON
        let completed = state
            .on_content_block_stop(0)
            .expect("BlockStop for tool_use must emit a Completed chunk");
        assert_eq!(completed.id, "tu_42");
        assert_eq!(completed.name, "shell");
        assert_eq!(completed.index, 0);
        assert_eq!(completed.status, ToolCallChunkStatus::Completed);
        assert!(
            completed.arguments_delta.is_none(),
            "Completed.arguments_delta MUST be None"
        );

        // T3-0 invariant: Σ Streaming.arguments_delta == Completed.args.
        let aggregated: String = streamed_chunks
            .iter()
            .filter_map(|c| c.arguments_delta.as_deref())
            .collect();
        assert_eq!(aggregated, completed.args);
        assert_eq!(completed.args, r#"{"command": "ls -la"}"#);

        // The block-level entry must be drained at BlockStop.
        assert!(state.blocks.is_empty(), "blocks map must drain at BlockStop");
    }

    /// Stream interruption: BlockStop never arrives. Provider must not panic;
    /// the per-block entry stays in the map, no terminal Completed is emitted,
    /// and a follow-up second tool_use still gets a fresh stable `index`.
    #[test]
    fn test_tool_use_streaming_interrupted_handles_partial_json() {
        let mut state = AnthropicStreamState::default();

        // Tool 1: start + one partial fragment, then "stream ends" — no stop.
        let initial1 = state
            .on_content_block_start(0, tool_use_start_block("tu_a", "search"))
            .expect("initial Streaming chunk");
        assert_eq!(initial1.index, 0);

        let frag_out = state
            .on_content_block_delta(0, input_json_delta(r#"{"query":"#))
            .expect("partial fragment must surface");
        match frag_out {
            DeltaOutcome::ToolDelta(chunk) => {
                assert_eq!(chunk.index, 0);
                assert_eq!(chunk.arguments_delta.as_deref(), Some(r#"{"query":"#));
            }
            other => panic!("expected ToolDelta, got {other:?}"),
        }

        // Buffer is still alive (no Completed flushed yet).
        assert_eq!(state.blocks.len(), 1, "interrupted tool buffer must persist");

        // A second tool_use must still allocate a *fresh* index (1) — the
        // ordinal counter is independent of orphan buffers.
        let initial2 = state
            .on_content_block_start(1, tool_use_start_block("tu_b", "edit"))
            .expect("second initial Streaming chunk");
        assert_eq!(initial2.index, 1);
        assert_eq!(initial2.id, "tu_b");

        // Calling stop on a content_block that was never started must not panic
        // and must return None.
        assert!(state.on_content_block_stop(99).is_none());

        // Calling stop on the second tool flushes it cleanly with empty-args
        // fallback (`{}`) even though no input_json_delta arrived for it.
        let completed2 = state.on_content_block_stop(1).expect("Completed for tool 2");
        assert_eq!(completed2.index, 1);
        assert_eq!(completed2.args, "{}");
        assert_eq!(completed2.status, ToolCallChunkStatus::Completed);
    }

    /// Regression for the Anthropic EOF tail-flush path.
    ///
    /// If the upstream byte stream closes after `input_json_delta` events but
    /// before `content_block_stop` (and `message_stop`) arrive, the provider
    /// must still emit the terminal `Completed` chunks for every in-flight
    /// tool_use block so the driver does not silently lose the tool call.
    /// `drain_pending_tool_calls` is the canonical entry point for that flush.
    #[test]
    fn test_tool_use_eof_tail_flush_drains_pending_calls() {
        let mut state = AnthropicStreamState::default();

        // Two in-flight tool_use blocks, both received their fragments but
        // neither got a `content_block_stop` before EOF.
        let _ = state
            .on_content_block_start(0, tool_use_start_block("tu_x", "search"))
            .expect("initial Streaming chunk for tool 0");
        let _ = state
            .on_content_block_delta(0, input_json_delta(r#"{"q":"abc"}"#))
            .expect("partial fragment must surface for tool 0");

        let _ = state
            .on_content_block_start(1, tool_use_start_block("tu_y", "edit"))
            .expect("initial Streaming chunk for tool 1");
        // Tool 1 received no fragments — `args` must fall back to `{}`.

        assert_eq!(state.blocks.len(), 2, "two in-flight blocks before EOF flush");

        // Simulate EOF: the byte stream ended without BlockStop / MessageStop.
        let flushed = state.drain_pending_tool_calls();

        // Both pending tool_use blocks must surface as terminal Completed
        // chunks. Order is sorted by stable `index` for determinism.
        assert_eq!(
            flushed.len(),
            2,
            "EOF flush must emit one Completed per pending tool_use"
        );
        assert!(state.blocks.is_empty(), "drain must empty the per-block map");

        assert_eq!(flushed[0].index, 0);
        assert_eq!(flushed[0].id, "tu_x");
        assert_eq!(flushed[0].name, "search");
        assert_eq!(flushed[0].args, r#"{"q":"abc"}"#);
        assert_eq!(flushed[0].status, ToolCallChunkStatus::Completed);
        assert!(flushed[0].arguments_delta.is_none());

        assert_eq!(flushed[1].index, 1);
        assert_eq!(flushed[1].id, "tu_y");
        assert_eq!(flushed[1].name, "edit");
        assert_eq!(flushed[1].args, "{}", "no fragments => fallback {{}}");
        assert_eq!(flushed[1].status, ToolCallChunkStatus::Completed);

        // Re-draining an empty state must be a safe no-op.
        assert!(state.drain_pending_tool_calls().is_empty());
    }
}
