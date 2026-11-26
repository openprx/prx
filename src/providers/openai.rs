use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage};
use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, ChatTrace, Provider,
    StreamChunk, StreamError, StreamOptions, StreamResult, ToolCall as ProviderToolCall, ToolCallChunk,
};
use crate::providers::utf8_stream::SseTextDecoder;
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Provider name attached to byte-level UTF-8 decode failures on the
/// streaming path.
const STREAM_DECODER_LABEL: &str = "OpenAI";

/// Highest streamed `tool_calls[].index` accepted. The index sizes the
/// per-call buffer, and it is chosen by the server; one SSE line naming a huge
/// index must not allocate a buffer entry for every slot below it.
const MAX_STREAMED_TOOL_CALL_INDEX: usize = 128;

pub struct OpenAiProvider {
    parameter_provider: super::ProviderId,
    base_url: String,
    credential: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
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
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning/thinking models may return output in `reasoning_content`.
    /// Kept separate from `content` so callers can route it independently.
    #[serde(default)]
    reasoning_content: Option<String>,
}

impl ResponseMessage {
    /// Visible content only. Reasoning is intentionally not merged in.
    fn visible_content(&self) -> String {
        self.content.clone().unwrap_or_default()
    }

    /// Non-empty trimmed reasoning, if any.
    fn reasoning(&self) -> Option<String> {
        self.reasoning_content.as_ref().and_then(|r| {
            let trimmed = r.trim();
            if trimmed.is_empty() { None } else { Some(r.clone()) }
        })
    }
}

#[derive(Debug, Serialize)]
struct NativeChatRequest {
    model: String,
    messages: Vec<NativeMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

#[derive(Debug, Serialize)]
struct NativeMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<NativeToolCall>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct NativeToolSpec {
    #[serde(rename = "type")]
    kind: String,
    function: NativeToolFunctionSpec,
}

#[derive(Debug, Serialize, Deserialize)]
struct NativeToolFunctionSpec {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

fn parse_native_tool_spec(value: serde_json::Value) -> anyhow::Result<NativeToolSpec> {
    let spec: NativeToolSpec =
        serde_json::from_value(value).map_err(|e| anyhow::anyhow!("Invalid OpenAI tool specification: {e}"))?;

    if spec.kind != "function" {
        anyhow::bail!(
            "Invalid OpenAI tool specification: unsupported tool type '{}', expected 'function'",
            spec.kind
        );
    }

    Ok(spec)
}

#[derive(Debug, Serialize, Deserialize)]
struct NativeToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    function: NativeFunctionCall,
}

#[derive(Debug, Serialize, Deserialize)]
struct NativeFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct NativeChatResponse {
    choices: Vec<NativeChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct OpenAiUsage {
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    prompt_tokens: Option<u32>,
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    completion_tokens: Option<u32>,
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    total_tokens: Option<u32>,
}

impl OpenAiUsage {
    fn into_reported(self) -> Option<TokenUsage> {
        let total = self.total_tokens.or_else(|| {
            self.prompt_tokens
                .zip(self.completion_tokens)
                .map(|(p, c)| p.saturating_add(c))
        });
        if total.is_some() || self.prompt_tokens.zip(self.completion_tokens).is_some() {
            Some(TokenUsage::reported(self.prompt_tokens, self.completion_tokens, total))
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

#[derive(Debug, Serialize)]
struct StreamUsageOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct StreamingChatRequest {
    model: String,
    messages: Vec<NativeMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    stream: bool,
    stream_options: StreamUsageOptions,
}

#[derive(Debug, Deserialize)]
struct NativeChoice {
    message: NativeResponseMessage,
}

#[derive(Debug, Deserialize)]
struct NativeResponseMessage {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning/thinking models may return output in `reasoning_content`.
    /// Surfaced separately via `reasoning()`; never merged into `content`.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<NativeToolCall>>,
}

impl NativeResponseMessage {
    /// Visible content only. Reasoning is exposed via [`Self::reasoning`].
    fn visible_content(&self) -> Option<String> {
        self.content
            .as_ref()
            .and_then(|c| if c.is_empty() { None } else { Some(c.clone()) })
    }

    /// Non-empty trimmed reasoning_content, if any.
    fn reasoning(&self) -> Option<String> {
        self.reasoning_content.as_ref().and_then(|r| {
            let trimmed = r.trim();
            if trimmed.is_empty() { None } else { Some(r.clone()) }
        })
    }
}

impl OpenAiProvider {
    pub fn new(credential: Option<&str>) -> Self {
        Self::with_base_url(None, credential)
    }

    /// Create a provider with an optional custom base URL.
    /// Defaults to `https://api.openai.com/v1` when `base_url` is `None`.
    pub fn with_base_url(base_url: Option<&str>, credential: Option<&str>) -> Self {
        Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            credential: credential.map(ToString::to_string),
            parameter_provider: super::ProviderId::of("openai"),
        }
    }

    pub(crate) fn with_parameter_provider(mut self, provider: &str) -> Self {
        self.parameter_provider = super::ProviderId::of(provider);
        self
    }

    /// Lower every published tool schema to what this endpoint was declared to
    /// accept, or refuse the request naming the tool that cannot be offered.
    ///
    /// An empty list is no offer, on both the buffered and the streaming path:
    /// it must not become `tools: []` beside `tool_choice`, which the endpoint
    /// rejects.
    fn convert_tools(&self, tools: Option<&[ToolSpec]>) -> anyhow::Result<Option<Vec<NativeToolSpec>>> {
        let Some(items) = tools.filter(|items| !items.is_empty()) else {
            return Ok(None);
        };
        let dialect = self.tool_schema_dialect();
        let mut converted = Vec::with_capacity(items.len());
        for tool in items {
            let parameters =
                dialect
                    .wire_schema(&tool.name, &tool.parameters)
                    .map_err(|error| super::ProviderCapabilityError {
                        provider: "openai".to_string(),
                        capability: "tool_schema_dialect".to_string(),
                        message: error.to_string(),
                    })?;
            converted.push(NativeToolSpec {
                kind: "function".to_string(),
                function: NativeToolFunctionSpec {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: parameters.into_owned(),
                },
            });
        }
        Ok(Some(converted))
    }

    fn convert_messages(messages: &[ChatMessage]) -> Vec<NativeMessage> {
        messages
            .iter()
            .map(|m| {
                if m.role == "assistant" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&m.content) {
                        if let Some(tool_calls_value) = value.get("tool_calls") {
                            if let Ok(parsed_calls) =
                                serde_json::from_value::<Vec<ProviderToolCall>>(tool_calls_value.clone())
                            {
                                let tool_calls = parsed_calls
                                    .into_iter()
                                    .map(|tc| NativeToolCall {
                                        id: Some(tc.id),
                                        kind: Some("function".to_string()),
                                        function: NativeFunctionCall {
                                            name: tc.name,
                                            arguments: tc.arguments,
                                        },
                                    })
                                    .collect::<Vec<_>>();
                                let content = value
                                    .get("content")
                                    .and_then(serde_json::Value::as_str)
                                    .map(ToString::to_string);
                                return NativeMessage {
                                    role: "assistant".to_string(),
                                    content,
                                    tool_call_id: None,
                                    tool_calls: Some(tool_calls),
                                };
                            }
                        }
                    }
                }

                if m.role == "tool" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&m.content) {
                        let tool_call_id = value
                            .get("tool_call_id")
                            .and_then(serde_json::Value::as_str)
                            .map(ToString::to_string);
                        let content = value
                            .get("content")
                            .and_then(serde_json::Value::as_str)
                            .map(ToString::to_string);
                        return NativeMessage {
                            role: "tool".to_string(),
                            content,
                            tool_call_id,
                            tool_calls: None,
                        };
                    }
                }

                NativeMessage {
                    role: m.role.clone(),
                    content: Some(m.content.clone()),
                    tool_call_id: None,
                    tool_calls: None,
                }
            })
            .collect()
    }

    fn parse_native_response(message: NativeResponseMessage) -> ProviderChatResponse {
        // Visible text and reasoning are split into separate fields. The
        // chat consumer only renders `text`; `reasoning_content` is preserved
        // for history reconstruction (see build_native_assistant_history).
        let reasoning = message.reasoning();
        let text = message.visible_content();
        let tool_calls = message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| ProviderToolCall {
                id: tc.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: tc.function.name,
                arguments: tc.function.arguments,
            })
            .collect::<Vec<_>>();

        ProviderChatResponse {
            text,
            tool_calls,
            reasoning_content: reasoning,
        }
    }

    async fn chat_metered(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<(ProviderChatResponse, TokenUsage)> {
        let credential = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "OpenAI API key not set. Set `api_key` in config.toml or run `prx auth paste-token --provider openai`."
            )
        })?;

        let tools = self.convert_tools(request.tools)?;
        let native_request = NativeChatRequest {
            model: model.to_string(),
            messages: Self::convert_messages(request.messages),
            temperature,
            tool_choice: tools.as_ref().map(|_| "auto".to_string()),
            tools,
        };

        let endpoint = format!("{}/chat/completions", self.base_url);
        let response = super::attempt_accounting::send_json(
            self.http_client()
                .post(&endpoint)
                .header("Authorization", format!("Bearer {credential}")),
            model,
            &endpoint,
            Some(credential),
            &super::parameter_wire::request_for(
                &self.parameter_provider,
                super::parameter_wire::Format::OpenAiChat,
                &native_request.model,
                &native_request,
            ),
            super::attempt_accounting::PhysicalKind::Chat,
        )
        .await?;

        if !response.status().is_success() {
            return Err(response.api_error("OpenAI").await);
        }

        let native_response: NativeChatResponse = response.json().await?;
        let usage = native_response.usage.and_then(OpenAiUsage::into_reported);
        let message = native_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| anyhow::anyhow!("No response from OpenAI"))?;
        let response = Self::parse_native_response(message);
        let tokens_used = usage.unwrap_or_else(|| {
            let chars = response.text.as_deref().unwrap_or("").chars().count()
                + response.reasoning_content.as_deref().unwrap_or("").chars().count();
            let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
            accumulator.finish_or_estimate_completion_chars(chars)
        });
        Ok((response, tokens_used))
    }

    fn http_client(&self) -> Client {
        crate::config::build_runtime_proxy_client_with_timeouts("provider.openai", 120, 10)
            .map_err(|e| {
                tracing::error!("proxy build failed for provider.openai, using direct: {e}");
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
        crate::config::apply_runtime_proxy_to_builder(builder, "provider.openai")
            .build()
            .map_err(|e| {
                tracing::error!("streaming client build failed for provider.openai, using direct: {e}");
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
// OpenAI Chat Completions native streaming with `stream: true` + tool_calls.
// SSE events have the shape:
//   data: {"choices":[{"delta":{"content":"...","tool_calls":[...]},
//                      "finish_reason": null | "stop" | "tool_calls"}]}
//   data: [DONE]
//
// Tool call deltas stream `index`-keyed fragments — provider must accumulate
// `function.arguments` strings until `finish_reason == "tool_calls"` then emit
// a single [`ToolCallChunk`] per accumulated call.

#[derive(Debug, Deserialize)]
struct StreamSseResponse {
    #[serde(default)]
    choices: Vec<StreamSseChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    /// A failure reported inside a stream that already answered 200.
    #[serde(default)]
    error: Option<StreamSseError>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamSseError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    code: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct StreamSseChoice {
    delta: StreamSseDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamSseDelta {
    #[serde(default)]
    content: Option<String>,
    /// DeepSeek / Kimi / GLM extension: reasoning tokens are emitted in a
    /// dedicated field, intentionally split from `content` so the consumer
    /// can route the chain-of-thought independently.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamSseToolCall>>,
}

#[derive(Debug, Deserialize)]
struct StreamSseToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamSseFunction>,
}

#[derive(Debug, Deserialize)]
struct StreamSseFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Buffered state for a single in-flight tool call across SSE chunks.
/// OpenAI streams `function.arguments` as incremental JSON fragments keyed by
/// `index`; we accumulate then emit a complete [`ToolCallChunk`] on
/// `finish_reason == "tool_calls"`.
#[derive(Debug, Default, Clone)]
struct ToolCallBuffer {
    id: String,
    name: String,
    arguments: String,
}

/// Outcome of feeding a single SSE `data:` payload into the parser.
#[derive(Debug, Default)]
struct OpenAiSseEvent {
    content: Option<String>,
    reasoning: Option<String>,
    tool_call_deltas: Vec<StreamSseToolCall>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
    done_sentinel: bool,
}

/// Parse a single SSE line into a structured event. Returns `Ok(None)` for
/// blank lines, comments, or non-data events.
fn parse_openai_sse_line(line: &str) -> StreamResult<Option<OpenAiSseEvent>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return Ok(None);
    }
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(None);
    };
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(Some(OpenAiSseEvent {
            done_sentinel: true,
            ..OpenAiSseEvent::default()
        }));
    }

    let parsed: StreamSseResponse = serde_json::from_str(data).map_err(StreamError::Json)?;
    if let Some(error) = parsed.error {
        // An in-stream error ends the turn; it must not parse as an empty,
        // successful chunk and be skipped.
        return Err(super::reliable::in_stream_error_object(
            "OpenAI",
            error.kind,
            error.code,
            error.message,
        ));
    }
    let usage = parsed.usage.and_then(OpenAiUsage::into_reported);
    let Some(choice) = parsed.choices.into_iter().next() else {
        return Ok(usage.map(|usage| OpenAiSseEvent {
            usage: Some(usage),
            ..OpenAiSseEvent::default()
        }));
    };

    let mut event = OpenAiSseEvent {
        finish_reason: choice.finish_reason,
        usage,
        ..OpenAiSseEvent::default()
    };
    event.content = choice.delta.content.filter(|c| !c.is_empty());
    event.reasoning = choice.delta.reasoning_content.filter(|r| !r.is_empty());
    if let Some(tcs) = choice.delta.tool_calls {
        event.tool_call_deltas = tcs;
    }
    Ok(Some(event))
}

/// Apply a vec of tool_call deltas onto the index-keyed buffer.
///
/// **S3 T3-2-B**: in addition to accumulating into the buffer, this returns a
/// vec of [`ToolCallChunk`]s with `status = Streaming` so the caller can emit
/// them as the SSE stream progresses. One streaming chunk is produced per input
/// delta to preserve the wire-order observed by the provider:
/// - First delta (carrying `id` + `name`): emits a `Streaming` chunk with
///   `arguments_delta = Some("")` even when no `arguments` fragment is present,
///   so the driver sees the call has been opened.
/// - Subsequent delta (with `function.arguments` fragment): emits a `Streaming`
///   chunk with `arguments_delta = Some(fragment)`.
///
/// `id` is sourced from whichever delta first carried it; if absent, a UUID is
/// minted lazily and recorded back into the buffer so the matching `Completed`
/// chunk shares the same id.
///
/// The buffer is indexed by the server-supplied `index`, so an index above
/// [`MAX_STREAMED_TOOL_CALL_INDEX`] is refused as malformed SSE instead of
/// growing the buffer to whatever the upstream names.
fn apply_tool_call_deltas(
    buf: &mut Vec<ToolCallBuffer>,
    deltas: Vec<StreamSseToolCall>,
) -> StreamResult<Vec<ToolCallChunk>> {
    let mut streaming = Vec::with_capacity(deltas.len());
    for delta in deltas {
        let slot = delta.index;
        if slot > MAX_STREAMED_TOOL_CALL_INDEX {
            return Err(super::reliable::tool_call_index_over_limit(
                "OpenAI",
                slot,
                MAX_STREAMED_TOOL_CALL_INDEX,
            ));
        }
        while buf.len() <= slot {
            buf.push(ToolCallBuffer::default());
        }
        let entry = match buf.get_mut(slot) {
            Some(e) => e,
            None => continue,
        };

        let mut got_id_or_name = false;
        if let Some(id) = delta.id {
            if !id.is_empty() {
                entry.id = id;
                got_id_or_name = true;
            }
        }
        let mut arg_fragment: Option<String> = None;
        if let Some(func) = delta.function {
            if let Some(name) = func.name {
                if !name.is_empty() {
                    entry.name = name;
                    got_id_or_name = true;
                }
            }
            if let Some(args) = func.arguments {
                entry.arguments.push_str(&args);
                arg_fragment = Some(args);
            }
        }

        // Skip producing a streaming chunk when neither identifying info nor
        // an argument fragment arrived (defensive — should not happen in
        // well-formed OpenAI streams, but the parser is permissive).
        if !got_id_or_name && arg_fragment.is_none() {
            continue;
        }

        // We can only emit a useful Streaming chunk once we know the function
        // name (driver consumers key on `name`). If the very first delta for
        // an index is malformed and carries only `id`, we still buffer it but
        // hold the streaming chunk until name arrives.
        if entry.name.is_empty() {
            continue;
        }

        if entry.id.is_empty() {
            entry.id = uuid::Uuid::new_v4().to_string();
        }
        let delta_text = arg_fragment.unwrap_or_default();
        streaming.push(ToolCallChunk {
            id: entry.id.clone(),
            name: entry.name.clone(),
            args: String::new(),
            index: slot,
            arguments_delta: Some(delta_text),
            status: crate::providers::traits::ToolCallChunkStatus::Streaming,
        });
    }
    Ok(streaming)
}

/// Flush the tool_call buffer into chunks suitable for [`StreamChunk::tool_call_chunk`].
///
/// **S3 T3-2-B**: emits the terminal `Completed` chunk for each buffered tool
/// call after preceding `Streaming` chunks have already been forwarded. The
/// driver-side aggregator treats `Completed.args` as authoritative, so this
/// must always return the full concatenated argument JSON.
fn flush_tool_call_buffer(buf: &[ToolCallBuffer]) -> Vec<ToolCallChunk> {
    buf.iter()
        .enumerate()
        .filter(|(_, entry)| !entry.name.is_empty())
        .map(|(idx, entry)| {
            let id = if entry.id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                entry.id.clone()
            };
            let args = if entry.arguments.is_empty() {
                "{}".to_string()
            } else {
                entry.arguments.clone()
            };
            ToolCallChunk {
                id,
                name: entry.name.clone(),
                args,
                index: idx,
                arguments_delta: None,
                status: crate::providers::traits::ToolCallChunkStatus::Completed,
            }
        })
        .collect()
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn accounting_targets(&self, model: &str) -> Option<super::AccountingTargets> {
        Some(super::AccountingTargets::chat(
            model,
            vec![format!("{}/chat/completions", self.base_url)],
            self.credential.as_deref(),
        ))
    }

    fn supports_attempt_accounting(&self) -> bool {
        true
    }

    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let credential = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "OpenAI API key not set. Set `api_key` in config.toml or run `prx auth paste-token --provider openai`."
            )
        })?;

        let mut messages = Vec::new();

        if let Some(sys) = system_prompt {
            messages.push(Message {
                role: "system".to_string(),
                content: sys.to_string(),
            });
        }

        messages.push(Message {
            role: "user".to_string(),
            content: message.to_string(),
        });

        let request = ChatRequest {
            model: model.to_string(),
            messages,
            temperature,
        };

        let response = self
            .http_client()
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {credential}"))
            .json(&super::parameter_wire::request_for(
                &self.parameter_provider,
                super::parameter_wire::Format::OpenAiChat,
                &request.model,
                &request,
            ))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(super::api_error("OpenAI", response).await);
        }

        let chat_response: ChatResponse = response.json().await?;

        let message = chat_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| anyhow::anyhow!("No response from OpenAI"))?;

        let visible = message.visible_content();
        if visible.is_empty() {
            if let Some(reasoning) = message.reasoning() {
                // Reasoning-only response with no visible output: log and surface
                // an empty string. The legacy chat_with_system path returns String
                // only, so reasoning cannot be carried back to history here — the
                // caller should prefer chat()/chat_with_tools() for full fidelity.
                tracing::warn!(
                    reasoning_chars = reasoning.chars().count(),
                    "OpenAI returned reasoning_content only with no visible content; chat_with_system drops reasoning"
                );
            }
        }
        Ok(visible)
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
                provider: "openai".to_string(),
                model: model.to_string(),
                started_at,
                finished_at,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "openai".to_string(),
            final_model: model.to_string(),
            served_member: None,
            tokens_used,
        })
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
        let credential = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "OpenAI API key not set. Set `api_key` in config.toml or run `prx auth paste-token --provider openai`."
            )
        })?;

        let native_tools: Option<Vec<NativeToolSpec>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .cloned()
                    .map(parse_native_tool_spec)
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };

        let native_request = NativeChatRequest {
            model: model.to_string(),
            messages: Self::convert_messages(messages),
            temperature,
            tool_choice: native_tools.as_ref().map(|_| "auto".to_string()),
            tools: native_tools,
        };

        let response = self
            .http_client()
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {credential}"))
            .json(&super::parameter_wire::request_for(
                &self.parameter_provider,
                super::parameter_wire::Format::OpenAiChat,
                &native_request.model,
                &native_request,
            ))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(super::api_error("OpenAI", response).await);
        }

        let native_response: NativeChatResponse = response.json().await?;
        let message = native_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| anyhow::anyhow!("No response from OpenAI"))?;
        Ok(Self::parse_native_response(message))
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        if let Some(credential) = self.credential.as_ref() {
            self.http_client()
                .get(format!("{}/models", self.base_url))
                .header("Authorization", format!("Bearer {credential}"))
                .send()
                .await?
                .error_for_status()?;
        }
        Ok(())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        let mut messages = Vec::with_capacity(if system_prompt.is_some() { 2 } else { 1 });
        if let Some(system) = system_prompt {
            messages.push(ChatMessage::system(system));
        }
        messages.push(ChatMessage::user(message));
        self.stream_chat_with_history(&messages, model, temperature, options)
    }

    /// **5a-7a**: Native streaming with full history + tool_calls.
    ///
    /// Sends `stream: true`, the native tools in `options.tools` and
    /// `tool_choice: "auto"` (only when tools are offered) to
    /// `/v1/chat/completions`, parses SSE chunks via
    /// [`parse_openai_sse_line`].
    ///
    /// **S3 T3-2-B**: emits the dual-phase tool-call protocol:
    /// - For each in-flight `tool_calls` SSE delta, emits a `Streaming`
    ///   [`ToolCallChunk`] carrying the `arguments_delta` fragment (or an
    ///   empty delta on the opening fragment that only carries `id`/`name`).
    /// - On `finish_reason == "tool_calls"`, emits one terminal `Completed`
    ///   [`ToolCallChunk`] per buffered index with the full JSON `args`.
    /// The driver-side aggregator reconciles both phases; aggregating all
    /// `Streaming.arguments_delta` MUST equal the terminal `Completed.args`.
    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        let Some(credential) = self.credential.clone() else {
            return stream::once(async { Err(StreamError::Provider("OpenAI API key not set".to_string())) }).boxed();
        };

        // Offer the caller's tools exactly as the buffered path does, with
        // `tool_choice: "auto"` whenever any are offered. A refusal fails the
        // turn before any connection is opened.
        let tools = match self.convert_tools(options.tools.as_deref()) {
            Ok(tools) => tools,
            Err(e) => {
                let msg = e.to_string();
                return stream::once(async move { Err(StreamError::Provider(msg)) }).boxed();
            }
        };
        let native_messages = Self::convert_messages(messages);
        let url = format!("{}/chat/completions", self.base_url);
        let client = self.streaming_http_client();

        let request_body = StreamingChatRequest {
            model: model.to_string(),
            messages: native_messages,
            temperature,
            tool_choice: tools.as_ref().map(|_| "auto".to_string()),
            tools,
            stream: true,
            stream_options: StreamUsageOptions { include_usage: true },
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(64);

        let parameter_provider = self.parameter_provider.clone();
        crate::providers::compatible::spawn_until_closed(tx.clone(), async move {
            let response = match client
                .post(&url)
                .header("Authorization", format!("Bearer {credential}"))
                .header("Accept", "text/event-stream")
                .json(&super::parameter_wire::request_for(
                    &parameter_provider,
                    super::parameter_wire::Format::OpenAiChat,
                    &request_body.model,
                    &request_body,
                ))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(StreamError::from(e))).await;
                    return;
                }
            };

            if !response.status().is_success() {
                let _ = tx.send(Err(super::stream_api_error("OpenAI", response).await)).await;
                return;
            }

            let mut tool_buf: Vec<ToolCallBuffer> = Vec::new();
            let mut usage_seen = false;
            let mut byte_stream = response.bytes_stream();
            let mut text_buf = String::new();
            // Byte-level decoder; `text_buf` above stays the SSE line accumulator.
            let mut decoder = SseTextDecoder::new(STREAM_DECODER_LABEL);
            let mut completion_chars: usize = 0;
            let mut sent_final = false;
            let mut saw_sse_event = false;
            let mut saw_non_sse_line = false;
            let mut saw_finish_reason = false;
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
                if let Err(e) = decoder.push(&bytes, &mut text_buf) {
                    let _ = tx.send(Err(e)).await;
                    return;
                }

                while let Some(pos) = text_buf.find('\n') {
                    let line: String = text_buf.drain(..=pos).collect();
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && !trimmed.starts_with(':') && !trimmed.starts_with("data:") {
                        saw_non_sse_line = true;
                    }
                    let event = match parse_openai_sse_line(&line) {
                        Ok(Some(ev)) => ev,
                        Ok(None) => continue,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    };
                    saw_sse_event = true;

                    if event.done_sentinel {
                        // Calls still buffered at `[DONE]` never got a
                        // `finish_reason` (every finish flushes or fails
                        // them). Apply the `stop` rule instead of dropping
                        // them: a name-only call fails the stream, and the
                        // rest are handed over for the loop to check.
                        if !tool_buf.is_empty() {
                            if tool_buf
                                .iter()
                                .any(|entry| !entry.name.is_empty() && entry.arguments.trim().is_empty())
                            {
                                let _ = tx
                                    .send(Err(StreamError::Provider(
                                        "OpenAI ended the message ([DONE] without finish_reason) with an incomplete \
                                         tool call; it was not run"
                                            .to_string(),
                                    )))
                                    .await;
                                return;
                            }
                            let calls = flush_tool_call_buffer(&tool_buf);
                            tool_buf.clear();
                            if !calls.is_empty() && tx.send(Ok(StreamChunk::tool_call_chunk(calls))).await.is_err() {
                                return;
                            }
                        }
                        if !usage_seen && completion_chars > 0 {
                            let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
                            let usage = accumulator.finish_or_estimate_completion_chars(completion_chars);
                            let _ = tx.send(Ok(StreamChunk::usage(usage))).await;
                        }
                        let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
                        sent_final = true;
                        break 'outer;
                    }

                    if let Some(usage) = event.usage {
                        usage_seen = true;
                        if tx.send(Ok(StreamChunk::usage(usage))).await.is_err() {
                            return;
                        }
                    }

                    if !event.tool_call_deltas.is_empty() {
                        let streaming = match apply_tool_call_deltas(&mut tool_buf, event.tool_call_deltas) {
                            Ok(streaming) => streaming,
                            Err(e) => {
                                let _ = tx.send(Err(e)).await;
                                return;
                            }
                        };
                        if !streaming.is_empty() && tx.send(Ok(StreamChunk::tool_call_chunk(streaming))).await.is_err()
                        {
                            return;
                        }
                    }

                    if let Some(content) = event.content {
                        completion_chars = completion_chars.saturating_add(content.chars().count());
                        let mut chunk = StreamChunk::delta(content);
                        if options.count_tokens {
                            chunk = chunk.with_token_estimate();
                        }
                        if tx.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }
                    if let Some(reasoning) = event.reasoning {
                        completion_chars = completion_chars.saturating_add(reasoning.chars().count());
                        let mut chunk = StreamChunk::reasoning_delta(reasoning);
                        if options.count_tokens {
                            chunk = chunk.with_token_estimate();
                        }
                        if tx.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }

                    if let Some(finish) = event.finish_reason.as_deref() {
                        saw_finish_reason = true;
                        // A filtered response is cut wherever the filter hit:
                        // fail visibly instead of passing it off as an answer.
                        if finish == "content_filter" {
                            let _ = tx
                                .send(Err(super::reliable::content_filter_stop(
                                    "OpenAI",
                                    "finish_reason content_filter",
                                )))
                                .await;
                            return;
                        }
                        // The output limit ended the message inside a call:
                        // its arguments are truncated, so fail the stream
                        // rather than drop or run the call.
                        if finish == "length" && !tool_buf.is_empty() {
                            let _ = tx
                                .send(Err(StreamError::Provider(
                                    "OpenAI stopped at the output length limit inside a tool call; its arguments \
                                     are truncated and the call was not run"
                                        .to_string(),
                                )))
                                .await;
                            return;
                        }
                        if finish != "tool_calls" && !tool_buf.is_empty() {
                            // Some servers end a complete call with `stop`
                            // (for example a forced tool choice). A call that
                            // got its name but not one argument byte cannot be
                            // told apart downstream from a no-argument call,
                            // so it fails the stream. Every other call is
                            // handed over as received: the loop runs the ones
                            // whose arguments parse and refuses the rest on
                            // their call id.
                            if tool_buf
                                .iter()
                                .any(|entry| !entry.name.is_empty() && entry.arguments.trim().is_empty())
                            {
                                let _ = tx
                                    .send(Err(StreamError::Provider(format!(
                                        "OpenAI ended the message (finish_reason {finish}) with an incomplete tool \
                                         call; it was not run"
                                    ))))
                                    .await;
                                return;
                            }
                        }
                        if !tool_buf.is_empty() {
                            let calls = flush_tool_call_buffer(&tool_buf);
                            tool_buf.clear();
                            if !calls.is_empty() && tx.send(Ok(StreamChunk::tool_call_chunk(calls))).await.is_err() {
                                return;
                            }
                        }
                        // With stream_options.include_usage, OpenAI sends a
                        // final usage-only chunk after finish_reason and before
                        // [DONE]. Keep reading so that chunk is not skipped.
                    }
                }
            }

            if !sent_final {
                // No terminal sentinel arrived, so the body simply stopped. A
                // character left half-decoded at that point is real truncation.
                if let Err(e) = decoder.finish() {
                    let _ = tx.send(Err(e)).await;
                    return;
                }

                let trailing = text_buf.trim();
                if !trailing.is_empty() {
                    let preview = trailing.chars().take(200).collect::<String>();
                    let _ = tx
                        .send(Err(StreamError::InvalidSse(format!(
                            "OpenAI SSE stream ended with incomplete trailing data: {preview}"
                        ))))
                        .await;
                    return;
                }

                if !saw_sse_event && saw_non_sse_line {
                    let _ = tx
                        .send(Err(StreamError::InvalidSse(
                            "OpenAI streaming response did not contain any SSE data events".to_string(),
                        )))
                        .await;
                    return;
                }

                // No `[DONE]`. A message that already ended with a
                // `finish_reason` and has no call still buffered is complete;
                // accept it. Otherwise the body was cut: buffered tool calls
                // may hold half of their arguments and a text answer is cut
                // short, so fail the stream instead of reporting success.
                if saw_finish_reason && tool_buf.is_empty() {
                    if !usage_seen && completion_chars > 0 {
                        let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
                        let usage = accumulator.finish_or_estimate_completion_chars(completion_chars);
                        let _ = tx.send(Ok(StreamChunk::usage(usage))).await;
                    }
                    let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
                    return;
                }
                let _ = tx
                    .send(Err(StreamError::InvalidSse(
                        "OpenAI stream ended before [DONE]; the response is incomplete".to_string(),
                    )))
                    .await;
            }
        });

        stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests;
