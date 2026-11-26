use crate::auth::discovery::is_claude_code_oauth_setup_token;
use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage};
use crate::multimodal;
use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, ChatTrace, Provider,
    StreamChunk, StreamError, StreamOptions, StreamResult, ToolCall as ProviderToolCall, ToolCallChunk,
    ToolCallChunkStatus,
};
use crate::providers::utf8_stream::SseTextDecoder;
use crate::tools::ToolSpec;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Provider name attached to byte-level UTF-8 decode failures on the
/// streaming path.
const STREAM_DECODER_LABEL: &str = "Anthropic";

/// OAuth state for Claude Code token auto-refresh.
struct OAuthState {
    credential: Option<String>,
    refresh_token: Option<String>,
}

pub struct AnthropicProvider {
    oauth: Mutex<OAuthState>,
    base_url: String,
    parameter_provider: super::ProviderId,
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

/// Indices, among the non-system messages, that carry a conversation cache
/// breakpoint. The last message always carries one. When a live user message
/// follows at least one earlier message, the message just before it (the end
/// of the replayed history) carries one too. At most two indices, so a request
/// with tools and a large system prompt stays within Anthropic's four markers.
fn conversation_breakpoint_indices(roles: &[&str]) -> Vec<usize> {
    let Some(last) = roles.len().checked_sub(1) else {
        return Vec::new();
    };
    let mut indices = Vec::with_capacity(2);
    if let Some(history_end) = roles
        .iter()
        .rposition(|role| *role == "user")
        .and_then(|live| live.checked_sub(1))
    {
        indices.push(history_end);
    }
    indices.push(last);
    indices
}

/// Set an ephemeral cache marker on the last content block of a message.
/// `Image` blocks carry no marker field and are left unmarked.
fn mark_last_block(message: &mut NativeMessage) {
    match message.content.last_mut() {
        Some(NativeContentOut::Text { cache_control, .. })
        | Some(NativeContentOut::ToolUse { cache_control, .. })
        | Some(NativeContentOut::ToolResult { cache_control, .. }) => {
            *cache_control = Some(CacheControl::ephemeral());
        }
        Some(NativeContentOut::Image { .. }) | None => {}
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
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

impl AnthropicUsage {
    const fn into_reported(self) -> TokenUsage {
        let prompt = sum_optional_u32(
            self.input_tokens,
            self.cache_creation_input_tokens,
            self.cache_read_input_tokens,
        );
        let total = match (prompt, self.output_tokens) {
            (Some(input), Some(output)) => Some(input.saturating_add(output)),
            _ => None,
        };
        TokenUsage::reported_with_cache(
            prompt,
            self.output_tokens,
            total,
            self.cache_creation_input_tokens,
            self.cache_read_input_tokens,
        )
    }

    const fn has_any_tokens(self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_creation_input_tokens.is_some()
            || self.cache_read_input_tokens.is_some()
    }
}

const fn sum_optional_u32(a: Option<u32>, b: Option<u32>, c: Option<u32>) -> Option<u32> {
    match (a, b, c) {
        (None, None, None) => None,
        (a, b, c) => {
            let mut total = 0u32;
            if let Some(value) = a {
                total = total.saturating_add(value);
            }
            if let Some(value) = b {
                total = total.saturating_add(value);
            }
            if let Some(value) = c {
                total = total.saturating_add(value);
            }
            Some(total)
        }
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
            }),
            base_url,
            parameter_provider: super::ProviderId::of("anthropic"),
        }
    }

    pub fn with_oauth(credential: Option<&str>, refresh_token: Option<String>, _expires_at: Option<i64>) -> Self {
        Self {
            oauth: Mutex::new(OAuthState {
                credential: credential
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .map(ToString::to_string),
                refresh_token,
            }),
            base_url: "https://api.anthropic.com".to_string(),
            parameter_provider: super::ProviderId::of("anthropic"),
        }
    }

    pub(crate) fn with_parameter_provider(mut self, provider: &str) -> Self {
        self.parameter_provider = super::ProviderId::of(provider);
        self
    }

    /// Returns a fresh credential. Subscription setup-tokens and refresh tokens are refused.
    /// For plain API keys (no refresh_token), returns the credential as-is.
    fn ensure_fresh_credential(&self) -> anyhow::Result<String> {
        let state = self.oauth.lock();

        if state.refresh_token.is_some() || state.credential.as_deref().is_some_and(Self::is_setup_token) {
            anyhow::bail!("{}", crate::auth::terms::ANTHROPIC_SUBSCRIPTION_REMOVED);
        }

        state.credential.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "Anthropic credentials not set. Set `api_key` in config.toml or store a Console API key with `prx auth paste-token --provider anthropic`."
            )
        })
    }

    /// Try to refresh the token after a 401 response. Returns Ok(new_credential) if
    /// refresh succeeded, Err if no refresh possible (plain API key or no refresh_token).
    fn try_refresh_after_401(&self) -> anyhow::Result<String> {
        anyhow::bail!("{}", crate::auth::terms::ANTHROPIC_SUBSCRIPTION_REMOVED)
    }

    fn is_setup_token(token: &str) -> bool {
        is_claude_code_oauth_setup_token(token)
    }

    fn apply_auth(&self, request: reqwest::RequestBuilder, credential: &str) -> reqwest::RequestBuilder {
        if Self::is_setup_token(credential) {
            request
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

    /// Mark the conversation breakpoints from the layout: the end of the
    /// replayed history and the last message. Native messages map one-to-one
    /// onto the non-system chat messages, in order (`convert_messages`).
    fn apply_conversation_breakpoints(messages: &[ChatMessage], native: &mut [NativeMessage]) {
        if !Self::should_cache_conversation(messages) {
            return;
        }
        let roles: Vec<&str> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| m.role.as_str())
            .collect();
        for index in conversation_breakpoint_indices(&roles) {
            if let Some(message) = native.get_mut(index) {
                mark_last_block(message);
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

    /// Lower every published tool schema to what this endpoint was declared to
    /// accept, or refuse the request naming the tool that cannot be offered.
    fn convert_tools<'a>(&self, tools: Option<&'a [ToolSpec]>) -> anyhow::Result<Option<Vec<NativeToolSpec<'a>>>> {
        let items = tools.filter(|items| !items.is_empty());
        let Some(items) = items else {
            return Ok(None);
        };
        let dialect = self.tool_schema_dialect();
        let mut native_tools: Vec<NativeToolSpec<'a>> = Vec::with_capacity(items.len());
        for tool in items {
            let input_schema =
                dialect
                    .wire_schema(&tool.name, &tool.parameters)
                    .map_err(|error| super::ProviderCapabilityError {
                        provider: "anthropic".to_string(),
                        capability: "tool_schema_dialect".to_string(),
                        message: error.to_string(),
                    })?;
            let input_schema = crate::tools::schema::SchemaCleanr::clean_for_anthropic(input_schema.into_owned());
            // Resolving a root reference can expose a combinator too.
            let input_schema = match dialect.wire_schema(&tool.name, &input_schema).map_err(|error| {
                super::ProviderCapabilityError {
                    provider: "anthropic".to_string(),
                    capability: "tool_schema_dialect".to_string(),
                    message: error.to_string(),
                }
            })? {
                std::borrow::Cow::Borrowed(_) => input_schema,
                std::borrow::Cow::Owned(lowered) => lowered,
            };
            native_tools.push(NativeToolSpec {
                name: &tool.name,
                description: &tool.description,
                input_schema,
                cache_control: None,
            });
        }

        // Cache the last tool definition (caches all tools)
        if let Some(last_tool) = native_tools.last_mut() {
            last_tool.cache_control = Some(CacheControl::ephemeral());
        }

        Ok(Some(native_tools))
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

        Self::apply_conversation_breakpoints(request.messages, &mut messages);

        let native_request = NativeChatRequest {
            model: model.to_string(),
            max_tokens: 4096,
            system: system_prompt,
            messages,
            temperature,
            tools: self.convert_tools(request.tools)?,
        };

        let req = self
            .http_client()
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(
                &super::parameter_wire::request_for(
                    &self.parameter_provider,
                    super::parameter_wire::Format::Anthropic,
                    &native_request.model,
                    &native_request,
                )
                .with_default_output_limit(),
            );

        let response = self.apply_auth(req, &credential).send().await?;

        let native_response = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Ok(new_credential) = self.try_refresh_after_401() {
                let retry_req = self
                    .http_client()
                    .post(format!("{}/v1/messages", self.base_url))
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(
                        &super::parameter_wire::request_for(
                            &self.parameter_provider,
                            super::parameter_wire::Format::Anthropic,
                            &native_request.model,
                            &native_request,
                        )
                        .with_default_output_limit()
                        .without_recording(),
                    );
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
            .unwrap_or_else(|_| crate::prx_http_client())
    }

    /// Client for streaming requests: a connect timeout, but no whole-request
    /// deadline. A reqwest client timeout covers the entire response body, so
    /// the buffered client's two-minute limit cut healthy streams while tokens
    /// were still arriving. A stalled stream is ended by the agent's
    /// progress-aware idle guard, which every received chunk resets.
    fn streaming_http_client(&self) -> Client {
        let builder = Client::builder().connect_timeout(std::time::Duration::from_secs(10));
        crate::config::apply_runtime_proxy_to_builder(builder, "provider.anthropic")
            .build()
            .map_err(|e| {
                tracing::error!("streaming client build failed for provider.anthropic, using direct: {e}");
                e
            })
            .unwrap_or_else(|_| {
                Client::builder()
                    .user_agent(crate::PRX_USER_AGENT)
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .build()
                    .unwrap_or_else(|_| crate::prx_http_client())
            })
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
// `content_block_start` time. The engine's streaming tool-call aggregator
// (`agent::provider_round`) handles both legacy (single Completed) and
// incremental (Streaming…Completed) shapes.

#[derive(Debug, Serialize)]
struct OwnedNativeToolSpec {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl From<NativeToolSpec<'_>> for OwnedNativeToolSpec {
    fn from(spec: NativeToolSpec<'_>) -> Self {
        Self {
            name: spec.name.to_string(),
            description: spec.description.to_string(),
            input_schema: spec.input_schema,
            cache_control: spec.cache_control,
        }
    }
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
    /// Whether the most recently started content block is a `tool_use` block.
    /// A `max_tokens` stop then cut that call's arguments short.
    last_block_is_tool_use: bool,
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
        self.last_block_is_tool_use = is_tool;

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
    /// `event: message_delta` with optional cumulative output usage and the
    /// message's `stop_reason`.
    MessageDelta(AnthropicUsage, Option<String>),
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
    delta: MessageDeltaBody,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct MessageDeltaBody {
    #[serde(default)]
    stop_reason: Option<String>,
}

/// `event: error`: `{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}`.
#[derive(Debug, Default, Deserialize)]
struct StreamErrorPayload {
    #[serde(default)]
    error: StreamErrorBody,
}

#[derive(Debug, Default, Deserialize)]
struct StreamErrorBody {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
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

/// Why a message that ended with `stop_reason` while its last block was a
/// `tool_use` left that call unfinished, or `None` for the stops that end a
/// message normally.
///
/// The wording never names the raw `max_tokens` / context-window reasons:
/// the loop's context-overflow classifier matches those words, and a
/// truncated call is not a request that compaction could fix.
fn unfinished_tool_call_stop(stop_reason: &str) -> Option<String> {
    match stop_reason {
        "tool_use" | "end_turn" | "stop_sequence" => None,
        "max_tokens" => Some("the output cap was reached".to_string()),
        "model_context_window_exceeded" => Some("the model ran out of room".to_string()),
        "refusal" => Some("the model refused".to_string()),
        other => {
            let reason: String = other
                .chars()
                .filter(|c| c.is_ascii_lowercase() || *c == '_')
                .take(40)
                .collect();
            Some(format!("stop reason {reason}"))
        }
    }
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
            Ok(Some(AnthropicEvent::MessageDelta(
                payload.usage.unwrap_or_default(),
                payload.delta.stop_reason,
            )))
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
        // The API reports a failure that happens after the 200 response (for
        // example `overloaded_error`) as an in-stream event and then closes
        // the stream. It ends the turn; it is never skipped as noise.
        "error" => {
            let payload: StreamErrorPayload = serde_json::from_str(&data_buf).unwrap_or_default();
            let kind = if payload.error.kind.is_empty() {
                "unknown_error"
            } else {
                payload.error.kind.as_str()
            };
            let message: String = payload.error.message.chars().take(300).collect();
            Err(StreamError::Provider(format!(
                "Anthropic stream error ({kind}): {message}"
            )))
        }
        _ => Ok(Some(AnthropicEvent::Other)),
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    /// Anthropic does not resolve `$ref`, so tool schemas go out through
    /// `SchemaCleanr::clean_for_anthropic`, which inlines the definitions
    /// instead. The model answers against the inlined document.
    fn tool_schema_dialect(&self) -> crate::tools::ToolSchemaDialect {
        crate::tools::ToolSchemaDialect::NoRootCombinators
    }

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
            .json(
                &super::parameter_wire::request_for(
                    &self.parameter_provider,
                    super::parameter_wire::Format::Anthropic,
                    &body.model,
                    &body,
                )
                .with_default_output_limit(),
            );

        let response = self.apply_auth(req, &credential).send().await?;

        // Handle 401: try one refresh + retry before failing.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Ok(new_credential) = self.try_refresh_after_401() {
                let retry_req = self
                    .http_client()
                    .post(format!("{}/v1/messages", self.base_url))
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(
                        &super::parameter_wire::request_for(
                            &self.parameter_provider,
                            super::parameter_wire::Format::Anthropic,
                            &body.model,
                            &body,
                        )
                        .with_default_output_limit()
                        .without_recording(),
                    );
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
            served_member: None,
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
    /// Sends `stream: true` and the native tools in `options.tools` to
    /// `/v1/messages`, parses the event-driven SSE
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

        // Offer the caller's tools exactly as the buffered path does: lowered
        // through this endpoint's dialect, with the cache marker on the last
        // one. A refusal fails the turn before any connection is opened.
        let tools = match self.convert_tools(options.tools.as_deref()) {
            Ok(tools) => tools.map(|tools| tools.into_iter().map(OwnedNativeToolSpec::from).collect::<Vec<_>>()),
            Err(e) => {
                let msg = e.to_string();
                return stream::once(async move { Err(StreamError::Provider(msg)) }).boxed();
            }
        };
        let (system_prompt, mut native_messages) = Self::convert_messages(messages);
        // The same breakpoints the buffered path sets: history end and last
        // message. With the last tool and the system prompt, at most four.
        Self::apply_conversation_breakpoints(messages, &mut native_messages);

        let url = format!("{}/v1/messages", self.base_url);
        let client = self.streaming_http_client();
        let parameter_provider = self.parameter_provider.clone();
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

        crate::providers::compatible::spawn_until_closed(tx.clone(), async move {
            let mut req = client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .json(
                    &super::parameter_wire::request_for(
                        &parameter_provider,
                        super::parameter_wire::Format::Anthropic,
                        &request_body.model,
                        &request_body,
                    )
                    .with_default_output_limit(),
                );
            req = if use_bearer {
                req.header("Authorization", format!("Bearer {credential}"))
                    .header("anthropic-beta", "oauth-2025-04-20")
            } else {
                req.header("x-api-key", &credential)
            };

            let response = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(StreamError::from(e))).await;
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
            let mut stop_reason_seen = false;
            let mut byte_stream = response.bytes_stream();
            let mut buf = String::new();
            // Byte-level decoder; `buf` above stays the SSE record accumulator.
            let mut decoder = SseTextDecoder::new(STREAM_DECODER_LABEL);
            let mut completion_chars: usize = 0;
            let mut sent_final = false;

            'outer: loop {
                // Stop reading as soon as nobody consumes the stream (the
                // turn ended, for example by the idle guard): otherwise this
                // task and its connection wait on the server indefinitely,
                // since the streaming client has no whole-request deadline.
                let next = tokio::select! {
                    () = tx.closed() => return,
                    next = byte_stream.next() => next,
                };
                let Some(bytes_res) = next else {
                    break 'outer;
                };
                let bytes = match bytes_res {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(StreamError::from(e))).await;
                        return;
                    }
                };
                // A character split across chunk boundaries is carried over,
                // not treated as corruption.
                if let Err(e) = decoder.push(&bytes, &mut buf) {
                    let _ = tx.send(Err(e)).await;
                    return;
                }

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
                            if usage.has_any_tokens() {
                                usage_seen = true;
                                if usage.input_tokens.is_some() {
                                    usage_state.input_tokens = usage.input_tokens;
                                }
                                if usage.output_tokens.is_some() {
                                    usage_state.output_tokens = usage.output_tokens;
                                }
                                if usage.cache_creation_input_tokens.is_some() {
                                    usage_state.cache_creation_input_tokens = usage.cache_creation_input_tokens;
                                }
                                if usage.cache_read_input_tokens.is_some() {
                                    usage_state.cache_read_input_tokens = usage.cache_read_input_tokens;
                                }
                            }
                        }
                        AnthropicEvent::MessageDelta(usage, stop_reason) => {
                            stop_reason_seen |= stop_reason.is_some();
                            // Any stop other than a normal end while the last
                            // block is a `tool_use` left that call unfinished,
                            // possibly with no argument bytes at all. The block
                            // was already closed, so fail the whole message: a
                            // failed stream's calls are never executed.
                            if state.last_block_is_tool_use
                                && let Some(why) = stop_reason.as_deref().and_then(unfinished_tool_call_stop)
                            {
                                let _ = tx
                                    .send(Err(StreamError::Provider(format!(
                                        "Anthropic ended the message inside a tool call ({why}); the call's \
                                         arguments are incomplete and it was not run"
                                    ))))
                                    .await;
                                return;
                            }
                            if usage.has_any_tokens() {
                                usage_seen = true;
                                if usage.input_tokens.is_some() {
                                    usage_state.input_tokens = usage.input_tokens;
                                }
                                if usage.output_tokens.is_some() {
                                    usage_state.output_tokens = usage.output_tokens;
                                }
                                if usage.cache_creation_input_tokens.is_some() {
                                    usage_state.cache_creation_input_tokens = usage.cache_creation_input_tokens;
                                }
                                if usage.cache_read_input_tokens.is_some() {
                                    usage_state.cache_read_input_tokens = usage.cache_read_input_tokens;
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
                            // No `stop_reason` ever confirmed how the message
                            // ended, and its last block is a `tool_use`: that
                            // call is unconfirmed, and a name-only one would
                            // run as `{}`.
                            if !stop_reason_seen && state.last_block_is_tool_use {
                                let _ = tx
                                    .send(Err(StreamError::Provider(
                                        "Anthropic ended the message inside a tool call (no stop reason); the call's \
                                         arguments are incomplete and it was not run"
                                            .to_string(),
                                    )))
                                    .await;
                                return;
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
                            sent_final = true;
                            break 'outer;
                        }
                        AnthropicEvent::Other => {}
                    }
                }
            }

            if !sent_final {
                // No `message_stop` arrived, so the body simply stopped. A
                // character left half-decoded at that point is real truncation.
                if let Err(e) = decoder.finish() {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
                // Either way the message is incomplete: a `tool_use` block may
                // hold half of its arguments, and a text answer is cut short.
                // Fail the stream instead of passing it off as a finished one.
                let _ = tx
                    .send(Err(StreamError::InvalidSse(
                        "Anthropic stream ended before message_stop; the response is incomplete".to_string(),
                    )))
                    .await;
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
mod tests;
