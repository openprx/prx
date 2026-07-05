use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage};
use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, ChatTrace, Provider,
    StreamChunk, StreamError, StreamOptions, StreamResult, ToolCall as ProviderToolCall, ToolCallChunk,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};

pub struct OpenAiProvider {
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
        }
    }

    fn convert_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<NativeToolSpec>> {
        tools.map(|items| {
            items
                .iter()
                .map(|tool| NativeToolSpec {
                    kind: "function".to_string(),
                    function: NativeToolFunctionSpec {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        parameters: tool.parameters.clone(),
                    },
                })
                .collect()
        })
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
        let credential = self
            .credential
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OpenAI API key not set. Set OPENAI_API_KEY or edit config.toml."))?;

        let tools = Self::convert_tools(request.tools);
        let native_request = NativeChatRequest {
            model: model.to_string(),
            messages: Self::convert_messages(request.messages),
            temperature,
            tool_choice: tools.as_ref().map(|_| "auto".to_string()),
            tools,
        };

        let response = self
            .http_client()
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {credential}"))
            .json(&native_request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(super::api_error("OpenAI", response).await);
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
            .unwrap_or_else(|_| Client::new())
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
fn apply_tool_call_deltas(buf: &mut Vec<ToolCallBuffer>, deltas: Vec<StreamSseToolCall>) -> Vec<ToolCallChunk> {
    let mut streaming = Vec::with_capacity(deltas.len());
    for delta in deltas {
        let slot = delta.index;
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
    streaming
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
        let credential = self
            .credential
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OpenAI API key not set. Set OPENAI_API_KEY or edit config.toml."))?;

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
            .json(&request)
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
        let credential = self
            .credential
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OpenAI API key not set. Set OPENAI_API_KEY or edit config.toml."))?;

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
            .json(&native_request)
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

    /// **5a-7a**: Native streaming with full history + tool_calls.
    ///
    /// Sends `stream: true` to `/v1/chat/completions`, parses SSE chunks via
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

        let native_messages = Self::convert_messages(messages);
        let url = format!("{}/chat/completions", self.base_url);
        let client = self.http_client();

        let request_body = StreamingChatRequest {
            model: model.to_string(),
            messages: native_messages,
            temperature,
            stream: true,
            stream_options: StreamUsageOptions { include_usage: true },
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(64);

        tokio::spawn(async move {
            let response = match client
                .post(&url)
                .header("Authorization", format!("Bearer {credential}"))
                .header("Accept", "text/event-stream")
                .json(&request_body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
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
            let mut completion_chars: usize = 0;
            let mut sent_final = false;
            let mut saw_sse_event = false;
            let mut saw_non_sse_line = false;
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
                                "non-utf8 byte in OpenAI SSE: {e}"
                            ))))
                            .await;
                        return;
                    }
                };
                text_buf.push_str(&text);

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
                        let streaming = apply_tool_call_deltas(&mut tool_buf, event.tool_call_deltas);
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
                        if finish == "tool_calls" && !tool_buf.is_empty() {
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

                // Defensive tail flush: some upstream gateways close the byte
                // stream without ever delivering `finish_reason == "tool_calls"`
                // (and without `[DONE]`). Surface any buffered tool calls so the
                // driver does not silently lose them on EOF.
                if !tool_buf.is_empty() {
                    let calls = flush_tool_call_buffer(&tool_buf);
                    tool_buf.clear();
                    if !calls.is_empty() {
                        let _ = tx.send(Ok(StreamChunk::tool_call_chunk(calls))).await;
                    }
                }
                if !usage_seen && completion_chars > 0 {
                    let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
                    let usage = accumulator.finish_or_estimate_completion_chars(completion_chars);
                    let _ = tx.send(Ok(StreamChunk::usage(usage))).await;
                }
                let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
            }
        });

        stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_with_key() {
        let p = OpenAiProvider::new(Some("openai-test-credential"));
        assert_eq!(p.credential.as_deref(), Some("openai-test-credential"));
    }

    #[test]
    fn creates_without_key() {
        let p = OpenAiProvider::new(None);
        assert!(p.credential.is_none());
    }

    #[test]
    fn creates_with_empty_key() {
        let p = OpenAiProvider::new(Some(""));
        assert_eq!(p.credential.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn chat_fails_without_key() {
        let p = OpenAiProvider::new(None);
        let result = p.chat_with_system(None, "hello", "gpt-4o", 0.7).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("API key not set"));
    }

    #[tokio::test]
    async fn chat_with_system_fails_without_key() {
        let p = OpenAiProvider::new(None);
        let result = p.chat_with_system(Some("You are OpenPRX"), "test", "gpt-4o", 0.5).await;
        assert!(result.is_err());
    }

    #[test]
    fn request_serializes_with_system_message() {
        let req = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: "You are OpenPRX".to_string(),
                },
                Message {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                },
            ],
            temperature: 0.7,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"role\":\"system\""));
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("gpt-4o"));
    }

    #[test]
    fn request_serializes_without_system() {
        let req = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            temperature: 0.0,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("system"));
        assert!(json.contains("\"temperature\":0.0"));
    }

    #[test]
    fn response_deserializes_single_choice() {
        let json = r#"{"choices":[{"message":{"content":"Hi!"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.visible_content(), "Hi!");
    }

    #[test]
    fn response_deserializes_empty_choices() {
        let json = r#"{"choices":[]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.choices.is_empty());
    }

    #[test]
    fn response_deserializes_multiple_choices() {
        let json = r#"{"choices":[{"message":{"content":"A"}},{"message":{"content":"B"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 2);
        assert_eq!(resp.choices[0].message.visible_content(), "A");
    }

    #[test]
    fn response_with_unicode() {
        let json = r#"{"choices":[{"message":{"content":"Hello \u03A9"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.visible_content(), "Hello \u{03A9}");
    }

    #[test]
    fn response_with_long_content() {
        let long = "x".repeat(100_000);
        let json = format!(r#"{{"choices":[{{"message":{{"content":"{long}"}}}}]}}"#);
        let resp: ChatResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp.choices[0].message.content.as_ref().unwrap().len(), 100_000);
    }

    #[tokio::test]
    async fn warmup_without_key_is_noop() {
        let provider = OpenAiProvider::new(None);
        let result = provider.warmup().await;
        assert!(result.is_ok());
    }

    // ----------------------------------------------------------
    // Reasoning model fallback tests (reasoning_content)
    // ----------------------------------------------------------

    #[test]
    fn reasoning_content_not_merged_into_visible_when_content_empty() {
        let json = r#"{"choices":[{"message":{"content":"","reasoning_content":"Thinking..."}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        // Reasoning must stay in its own field — visible content remains empty.
        assert_eq!(msg.visible_content(), "");
        assert_eq!(msg.reasoning().as_deref(), Some("Thinking..."));
    }

    #[test]
    fn reasoning_content_not_merged_when_content_null() {
        let json = r#"{"choices":[{"message":{"content":null,"reasoning_content":"Thinking..."}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.visible_content(), "");
        assert_eq!(msg.reasoning().as_deref(), Some("Thinking..."));
    }

    #[test]
    fn visible_content_preferred_when_present() {
        let json = r#"{"choices":[{"message":{"content":"Hello","reasoning_content":"Sidebar"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        // Both fields are separate; visible_content stays just the visible bit.
        assert_eq!(msg.visible_content(), "Hello");
        assert_eq!(msg.reasoning().as_deref(), Some("Sidebar"));
    }

    #[test]
    fn native_response_surfaces_reasoning_separately_when_content_empty() {
        let json = r#"{"choices":[{"message":{"content":"","reasoning_content":"Native thinking"}}]}"#;
        let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        // Visible content stays empty; reasoning lives on its own.
        assert_eq!(msg.visible_content(), None);
        assert_eq!(msg.reasoning().as_deref(), Some("Native thinking"));
    }

    #[test]
    fn native_response_keeps_visible_and_reasoning_apart() {
        let json = r#"{"choices":[{"message":{"content":"Real answer","reasoning_content":"Sidebar"}}]}"#;
        let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.visible_content(), Some("Real answer".to_string()));
        assert_eq!(msg.reasoning().as_deref(), Some("Sidebar"));
    }

    #[test]
    fn parse_native_response_populates_reasoning_content_field() {
        let json = r#"{"choices":[{"message":{"content":"Visible","reasoning_content":"Internal"}}]}"#;
        let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
        let msg = resp.choices.into_iter().next().expect("test: choice").message;
        let chat_resp = OpenAiProvider::parse_native_response(msg);
        // Visible text -> ProviderChatResponse.text. Reasoning -> reasoning_content.
        assert_eq!(chat_resp.text.as_deref(), Some("Visible"));
        assert_eq!(chat_resp.reasoning_content.as_deref(), Some("Internal"));
    }

    #[test]
    fn parse_native_response_no_reasoning_content_when_absent() {
        let json = r#"{"choices":[{"message":{"content":"Visible only"}}]}"#;
        let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
        let msg = resp.choices.into_iter().next().expect("test: choice").message;
        let chat_resp = OpenAiProvider::parse_native_response(msg);
        assert_eq!(chat_resp.text.as_deref(), Some("Visible only"));
        assert_eq!(chat_resp.reasoning_content, None);
    }

    #[test]
    fn openai_non_streaming_response_usage_maps_reported() {
        let json = r#"{
            "choices":[{"message":{"content":"Real answer"}}],
            "usage":{"prompt_tokens":30,"completion_tokens":7,"total_tokens":37}
        }"#;
        let resp: NativeChatResponse = serde_json::from_str(json).unwrap();
        let usage = resp
            .usage
            .expect("usage expected")
            .into_reported()
            .expect("complete usage should report");

        assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Reported);
        assert_eq!(usage.prompt_tokens, Some(30));
        assert_eq!(usage.completion_tokens, Some(7));
        assert_eq!(usage.total_tokens, Some(37));
    }

    #[test]
    fn openai_malformed_usage_is_ignored_without_breaking_content() {
        let line = r#"data: {"choices":[{"delta":{"content":"still streams"}}],"usage":{"prompt_tokens":"bad","completion_tokens":4}}"#;
        let event = parse_openai_sse_line(line)
            .expect("malformed usage fields should not fail SSE parsing")
            .expect("content event expected");

        assert_eq!(event.content.as_deref(), Some("still streams"));
        assert!(
            event.usage.is_none(),
            "partial/malformed usage must fall back to estimate outside the parser"
        );
    }

    #[tokio::test]
    async fn chat_with_tools_fails_without_key() {
        let p = OpenAiProvider::new(None);
        let messages = vec![ChatMessage::user("hello".to_string())];
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a shell command",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    },
                    "required": ["command"]
                }
            }
        })];
        let result = p.chat_with_tools(&messages, &tools, "gpt-4o", 0.7).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("API key not set"));
    }

    #[tokio::test]
    async fn chat_with_tools_rejects_invalid_tool_shape() {
        let p = OpenAiProvider::new(Some("openai-test-credential"));
        let messages = vec![ChatMessage::user("hello".to_string())];
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    },
                    "required": ["command"]
                }
            }
        })];

        let result = p.chat_with_tools(&messages, &tools, "gpt-4o", 0.7).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid OpenAI tool specification")
        );
    }

    #[test]
    fn native_tool_spec_deserializes_from_openai_format() {
        let json = serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a shell command",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    },
                    "required": ["command"]
                }
            }
        });
        let spec = parse_native_tool_spec(json).unwrap();
        assert_eq!(spec.kind, "function");
        assert_eq!(spec.function.name, "shell");
    }

    // ─── 5a-7a Streaming SSE tests ──────────────────────────────────────

    #[test]
    fn openai_sse_line_done_sentinel() {
        let ev = parse_openai_sse_line("data: [DONE]").unwrap().expect("event");
        assert!(ev.done_sentinel);
        assert!(ev.content.is_none());
    }

    #[test]
    fn openai_sse_line_blank_and_comment_skipped() {
        assert!(parse_openai_sse_line("").unwrap().is_none());
        assert!(parse_openai_sse_line(":heartbeat").unwrap().is_none());
        assert!(parse_openai_sse_line("\n").unwrap().is_none());
    }

    #[test]
    fn openai_sse_line_parses_content_delta() {
        let line = r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#;
        let ev = parse_openai_sse_line(line).unwrap().expect("event");
        assert_eq!(ev.content.as_deref(), Some("hello"));
        assert!(ev.tool_call_deltas.is_empty());
    }

    #[test]
    fn openai_sse_line_parses_final_usage_chunk() {
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17}}"#;
        let ev = parse_openai_sse_line(line).unwrap().expect("usage event");
        let usage = ev.usage.expect("usage must be surfaced");

        assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Reported);
        assert_eq!(usage.prompt_tokens, Some(12));
        assert_eq!(usage.completion_tokens, Some(5));
        assert_eq!(usage.total_tokens, Some(17));
        assert!(ev.content.is_none());
        assert!(ev.tool_call_deltas.is_empty());
    }

    #[test]
    fn openai_sse_line_parses_reasoning_content_separately() {
        let line = r#"data: {"choices":[{"delta":{"reasoning_content":"thinking"}}]}"#;
        let ev = parse_openai_sse_line(line).unwrap().expect("event");
        assert!(ev.content.is_none());
        assert_eq!(ev.reasoning.as_deref(), Some("thinking"));
    }

    #[test]
    fn openai_sse_line_parses_tool_call_delta() {
        let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_42","function":{"name":"shell","arguments":"{\"cmd\":"}}]}}]}"#;
        let ev = parse_openai_sse_line(line).unwrap().expect("event");
        assert_eq!(ev.tool_call_deltas.len(), 1);
        let tc = &ev.tool_call_deltas[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("call_42"));
        let f = tc.function.as_ref().unwrap();
        assert_eq!(f.name.as_deref(), Some("shell"));
        assert_eq!(f.arguments.as_deref(), Some("{\"cmd\":"));
    }

    #[test]
    fn openai_sse_finish_reason_propagates() {
        let line = r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        let ev = parse_openai_sse_line(line).unwrap().expect("event");
        assert_eq!(ev.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn tool_call_buffer_accumulates_partial_arguments() {
        let mut buf: Vec<ToolCallBuffer> = Vec::new();
        let s1 = apply_tool_call_deltas(
            &mut buf,
            vec![StreamSseToolCall {
                index: 0,
                id: Some("call_1".into()),
                function: Some(StreamSseFunction {
                    name: Some("shell".into()),
                    arguments: Some(r#"{"a""#.into()),
                }),
            }],
        );
        // First delta carries id+name+partial args → one Streaming chunk.
        assert_eq!(s1.len(), 1);
        assert_eq!(s1[0].status, crate::providers::traits::ToolCallChunkStatus::Streaming);
        assert_eq!(s1[0].arguments_delta.as_deref(), Some(r#"{"a""#));

        let s2 = apply_tool_call_deltas(
            &mut buf,
            vec![StreamSseToolCall {
                index: 0,
                id: None,
                function: Some(StreamSseFunction {
                    name: None,
                    arguments: Some(r#": 1}"#.into()),
                }),
            }],
        );
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].arguments_delta.as_deref(), Some(r#": 1}"#));

        let flushed = flush_tool_call_buffer(&buf);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].id, "call_1");
        assert_eq!(flushed[0].name, "shell");
        assert_eq!(flushed[0].args, r#"{"a": 1}"#);
        assert_eq!(flushed[0].index, 0);
        assert_eq!(
            flushed[0].status,
            crate::providers::traits::ToolCallChunkStatus::Completed
        );
        assert!(flushed[0].arguments_delta.is_none());
    }

    #[test]
    fn tool_call_buffer_supports_parallel_calls_by_index() {
        let mut buf: Vec<ToolCallBuffer> = Vec::new();
        let streaming = apply_tool_call_deltas(
            &mut buf,
            vec![
                StreamSseToolCall {
                    index: 0,
                    id: Some("call_a".into()),
                    function: Some(StreamSseFunction {
                        name: Some("ls".into()),
                        arguments: Some("{}".into()),
                    }),
                },
                StreamSseToolCall {
                    index: 1,
                    id: Some("call_b".into()),
                    function: Some(StreamSseFunction {
                        name: Some("pwd".into()),
                        arguments: Some("{}".into()),
                    }),
                },
            ],
        );
        assert_eq!(streaming.len(), 2);
        assert_eq!(streaming[0].index, 0);
        assert_eq!(streaming[1].index, 1);

        let flushed = flush_tool_call_buffer(&buf);
        assert_eq!(flushed.len(), 2);
        assert_eq!(flushed[0].name, "ls");
        assert_eq!(flushed[1].name, "pwd");
        assert_eq!(flushed[1].index, 1);
    }

    /// Regression for the OpenAI EOF tail-flush path.
    ///
    /// Some OpenAI-compatible gateways close the byte stream after emitting
    /// `tool_calls` deltas but before sending `finish_reason == "tool_calls"`
    /// or `[DONE]`. Without an EOF tail flush the buffered tool call would be
    /// silently dropped. We replay the exact splitter + post-loop flush that
    /// `stream_chat_with_system` runs and verify the buffer would still flush
    /// to a `Completed` chunk on EOF.
    #[test]
    fn tool_call_buffer_flushes_on_eof_without_finish_reason() {
        use crate::providers::traits::ToolCallChunkStatus;

        // Replay: 1 SSE line with id+name+partial args, 1 SSE line with the
        // remainder of args, then the byte stream ends WITHOUT any
        // `finish_reason` or `[DONE]` line. The post-loop branch in
        // `stream_chat_with_system` must surface the buffered tool call.
        let lines = [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_eof","function":{"name":"shell","arguments":"{\"cmd\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
        ];

        let mut tool_buf: Vec<ToolCallBuffer> = Vec::new();
        let mut streaming_emitted: Vec<ToolCallChunk> = Vec::new();
        let mut saw_finish = false;
        for line in lines {
            let event = match parse_openai_sse_line(line).expect("test: line parses") {
                Some(e) => e,
                None => continue,
            };
            if event.done_sentinel {
                saw_finish = true;
                break;
            }
            if !event.tool_call_deltas.is_empty() {
                streaming_emitted.extend(apply_tool_call_deltas(&mut tool_buf, event.tool_call_deltas));
            }
            if event.finish_reason.is_some() {
                saw_finish = true;
                break;
            }
        }

        // Precondition: nothing closed the turn — neither [DONE] nor a
        // finish_reason landed before the byte stream ended.
        assert!(!saw_finish, "test setup: no terminator must arrive before EOF");
        assert!(!tool_buf.is_empty(), "test setup: tool buffer must hold the call");
        assert_eq!(streaming_emitted.len(), 2, "two Streaming deltas (open + arg fragment)");

        // Drive the exact post-loop branch from `stream_chat_with_system`:
        // when no `final_chunk` has been sent, flush the buffered tool calls
        // before sending the synthetic final chunk.
        let calls = flush_tool_call_buffer(&tool_buf);
        tool_buf.clear();

        assert_eq!(calls.len(), 1, "EOF tail flush must surface the buffered call");
        assert_eq!(calls[0].id, "call_eof");
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].args, r#"{"cmd":"ls"}"#);
        assert_eq!(calls[0].index, 0);
        assert_eq!(calls[0].status, ToolCallChunkStatus::Completed);
        assert!(calls[0].arguments_delta.is_none());
    }

    #[test]
    fn tool_call_buffer_skips_unnamed_entries() {
        // Some servers send empty `arguments` chunks before name appears; we
        // must not emit unnamed/empty placeholder calls.
        let mut buf: Vec<ToolCallBuffer> = Vec::new();
        let streaming = apply_tool_call_deltas(
            &mut buf,
            vec![StreamSseToolCall {
                index: 0,
                id: Some("call_x".into()),
                function: Some(StreamSseFunction {
                    name: None,
                    arguments: Some(String::new()),
                }),
            }],
        );
        // No name yet → no Streaming chunk emitted to the driver.
        assert!(streaming.is_empty());
        assert!(flush_tool_call_buffer(&buf).is_empty());
    }

    /// Test helper: replay a sequence of SSE lines through the parser to
    /// assert the driver-facing chunk shape (delta / tool_call / final).
    ///
    /// Returns `(text, all_emitted_calls, saw_final, had_finish_reason)` where
    /// `all_emitted_calls` interleaves Streaming chunks (in arrival order) and
    /// the terminal Completed chunks emitted at `finish_reason == "tool_calls"`.
    fn replay_openai_sse(lines: &[&str]) -> (String, Vec<ToolCallChunk>, bool, bool) {
        let mut tool_buf: Vec<ToolCallBuffer> = Vec::new();
        let mut text = String::new();
        let mut emitted_calls: Vec<ToolCallChunk> = Vec::new();
        let mut saw_final = false;
        let mut had_finish_reason = false;
        for line in lines {
            let event = match parse_openai_sse_line(line).expect("test: line parses") {
                Some(e) => e,
                None => continue,
            };
            if event.done_sentinel {
                saw_final = true;
                break;
            }
            if !event.tool_call_deltas.is_empty() {
                let streaming = apply_tool_call_deltas(&mut tool_buf, event.tool_call_deltas);
                emitted_calls.extend(streaming);
            }
            if let Some(c) = event.content {
                text.push_str(&c);
            }
            if let Some(finish) = event.finish_reason.as_deref() {
                had_finish_reason = true;
                if finish == "tool_calls" {
                    emitted_calls.extend(flush_tool_call_buffer(&tool_buf));
                    tool_buf.clear();
                }
                saw_final = true;
                break;
            }
        }
        (text, emitted_calls, saw_final, had_finish_reason)
    }

    #[test]
    fn openai_streaming_native_tool_call_via_fixture() {
        use crate::providers::traits::ToolCallChunkStatus;
        // Replay a realistic SSE stream: content delta, then two `tool_calls`
        // arguments fragments, then `finish_reason=tool_calls`, then [DONE].
        let lines = [
            r#"data: {"choices":[{"delta":{"content":"checking"}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":"{\"cmd\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ];
        let (text, calls, final_seen, had_finish) = replay_openai_sse(&lines);
        assert_eq!(text, "checking");
        assert!(had_finish, "stream must include finish_reason");
        assert!(final_seen, "must reach final / done sentinel");
        // 2 Streaming + 1 Completed.
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].status, ToolCallChunkStatus::Streaming);
        assert_eq!(calls[1].status, ToolCallChunkStatus::Streaming);
        assert_eq!(calls[2].status, ToolCallChunkStatus::Completed);
        assert_eq!(calls[2].name, "shell");
        assert_eq!(calls[2].id, "call_1");
        assert_eq!(calls[2].args, r#"{"cmd":"ls"}"#);
        assert_eq!(calls[2].index, 0);
    }

    #[test]
    fn openai_streaming_delta_then_final_stop() {
        let lines = [
            r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":" world"}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ];
        let (text, calls, final_seen, had_finish) = replay_openai_sse(&lines);
        assert_eq!(text, "hello world");
        assert!(had_finish);
        assert!(final_seen);
        assert!(calls.is_empty());
    }

    #[test]
    fn openai_streaming_handles_partial_tool_args_across_three_chunks() {
        use crate::providers::traits::ToolCallChunkStatus;
        let lines = [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_z","function":{"name":"edit"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"p\""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"a.rs\","}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"l\":1}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let (_text, calls, _final, _finish) = replay_openai_sse(&lines);
        // 4 Streaming (id/name + 3 args fragments) + 1 Completed = 5.
        assert_eq!(calls.len(), 5);
        let completed = calls.last().expect("test: completed chunk");
        assert_eq!(completed.status, ToolCallChunkStatus::Completed);
        assert_eq!(completed.name, "edit");
        assert_eq!(completed.args, r#"{"p":"a.rs","l":1}"#);
    }

    /// **S3 T3-2-B**: incremental Streaming protocol — provider must emit a
    /// `Streaming` `ToolCallChunk` for each SSE delta carrying a tool-call
    /// fragment, then a single final `Completed` chunk whose `args` equals the
    /// concatenation of all preceding `arguments_delta` values.
    #[test]
    fn test_tool_calls_streaming_emits_incremental_chunks() {
        use crate::providers::traits::ToolCallChunkStatus;
        let lines = [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"search","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"uery\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"hello\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let (_text, calls, final_seen, had_finish) = replay_openai_sse(&lines);
        assert!(had_finish);
        assert!(final_seen);

        // 4 Streaming (1 opener with empty args + 3 fragments) + 1 Completed.
        assert_eq!(calls.len(), 5, "expected 4 Streaming + 1 Completed, got {calls:?}");
        let (streaming_chunks, completed_chunks): (Vec<_>, Vec<_>) =
            calls.iter().partition(|c| c.status == ToolCallChunkStatus::Streaming);
        assert_eq!(streaming_chunks.len(), 4);
        assert_eq!(completed_chunks.len(), 1);

        // Every Streaming chunk must carry an `arguments_delta` (possibly empty
        // on the opener) and an empty `args` field.
        for chunk in &streaming_chunks {
            assert_eq!(chunk.id, "call_abc");
            assert_eq!(chunk.name, "search");
            assert_eq!(chunk.index, 0);
            assert_eq!(chunk.args, "");
            assert!(chunk.arguments_delta.is_some());
        }

        // Aggregated streaming deltas must equal Completed.args.
        let aggregated: String = streaming_chunks
            .iter()
            .filter_map(|c| c.arguments_delta.as_deref())
            .collect();
        let completed = completed_chunks[0];
        assert_eq!(completed.status, ToolCallChunkStatus::Completed);
        assert_eq!(completed.id, "call_abc");
        assert_eq!(completed.name, "search");
        assert_eq!(completed.index, 0);
        assert!(completed.arguments_delta.is_none());
        assert_eq!(completed.args, r#"{"query":"hello"}"#);
        assert_eq!(
            aggregated, completed.args,
            "streaming deltas aggregated must equal Completed.args"
        );
    }

    /// **S3 T3-2-B**: concurrent tool calls — buffer must be isolated by
    /// `index` and emit independent `Completed` chunks for each call.
    #[test]
    fn test_tool_calls_concurrent_indices() {
        use crate::providers::traits::ToolCallChunkStatus;
        // Interleaved deltas for two concurrent tool calls (index 0 + 1).
        let lines = [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"search","arguments":""}},{"index":1,"id":"call_b","function":{"name":"fetch","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":\"rust\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"url\":\"x\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let (_text, calls, _final, _finish) = replay_openai_sse(&lines);

        let streaming_chunks: Vec<&ToolCallChunk> = calls
            .iter()
            .filter(|c| c.status == ToolCallChunkStatus::Streaming)
            .collect();
        let completed_chunks: Vec<&ToolCallChunk> = calls
            .iter()
            .filter(|c| c.status == ToolCallChunkStatus::Completed)
            .collect();

        // 2 openers + 1 fragment for index 0 + 1 fragment for index 1 = 4 Streaming.
        assert_eq!(streaming_chunks.len(), 4);
        assert_eq!(completed_chunks.len(), 2, "expected 2 independent Completed chunks");

        // Find Completed by index.
        let c0 = completed_chunks
            .iter()
            .find(|c| c.index == 0)
            .expect("test: completed index 0");
        let c1 = completed_chunks
            .iter()
            .find(|c| c.index == 1)
            .expect("test: completed index 1");

        assert_eq!(c0.id, "call_a");
        assert_eq!(c0.name, "search");
        assert_eq!(c0.args, r#"{"q":"rust"}"#);
        assert!(c0.arguments_delta.is_none());

        assert_eq!(c1.id, "call_b");
        assert_eq!(c1.name, "fetch");
        assert_eq!(c1.args, r#"{"url":"x"}"#);
        assert!(c1.arguments_delta.is_none());

        // Per-index aggregation invariant.
        let agg0: String = streaming_chunks
            .iter()
            .filter(|c| c.index == 0)
            .filter_map(|c| c.arguments_delta.as_deref())
            .collect();
        let agg1: String = streaming_chunks
            .iter()
            .filter(|c| c.index == 1)
            .filter_map(|c| c.arguments_delta.as_deref())
            .collect();
        assert_eq!(agg0, c0.args);
        assert_eq!(agg1, c1.args);
    }

    #[tokio::test]
    async fn openai_streaming_fails_without_key() {
        use crate::providers::traits::StreamOptions;
        let provider = OpenAiProvider::new(None);
        let messages = vec![ChatMessage::user("hi".to_string())];
        let mut stream = provider.stream_chat_with_history(&messages, "gpt-4o", 0.0, StreamOptions::new(false));
        let first = stream.next().await.expect("chunk");
        let err = first.expect_err("no key → error");
        assert!(err.to_string().contains("API key not set"));
    }

    #[tokio::test]
    async fn openai_streaming_propagates_http_error() {
        use crate::providers::traits::StreamOptions;
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn unauthorized() -> (StatusCode, &'static str) {
            (StatusCode::UNAUTHORIZED, "Unauthorized")
        }
        let app = Router::new().route("/chat/completions", post(unauthorized));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let provider = OpenAiProvider::with_base_url(Some(&format!("http://{addr}")), Some("bad-key"));
        let messages = vec![ChatMessage::user("hi".to_string())];
        let mut stream = provider.stream_chat_with_history(&messages, "gpt-4o", 0.0, StreamOptions::new(false));
        let first = stream.next().await.expect("chunk");
        let err = first.expect_err("must surface 401 as StreamError");
        let msg = err.to_string();
        assert!(msg.contains("401"), "got: {msg}");
    }

    #[tokio::test]
    async fn openai_streaming_rejects_malformed_non_sse_success_body() {
        use crate::providers::traits::StreamOptions;
        use axum::Router;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn malformed() -> &'static str {
            "{not valid json"
        }
        let app = Router::new().route("/chat/completions", post(malformed));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let provider = OpenAiProvider::with_base_url(Some(&format!("http://{addr}")), Some("test-key"));
        let messages = vec![ChatMessage::user("hi".to_string())];
        let mut stream = provider.stream_chat_with_history(&messages, "gpt-4o", 0.0, StreamOptions::new(false));
        let first = stream.next().await.expect("chunk");
        let err = first.expect_err("malformed 200 response must not become empty success");
        let msg = err.to_string();
        assert!(msg.contains("Invalid SSE format"), "got: {msg}");
        assert!(msg.contains("incomplete trailing data"), "got: {msg}");
    }
}
