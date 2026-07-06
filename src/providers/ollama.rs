use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, ProviderUsageAccumulator, TokenUsage};
use crate::multimodal;
use crate::providers::traits::{
    ChatMessage, ChatResponse, ChatTrace, Provider, ProviderCapabilities, StreamChunk, StreamError, StreamOptions,
    StreamResult, ToolCall, ToolCallChunk,
};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub struct OllamaProvider {
    base_url: String,
    api_key: Option<String>,
    reasoning_enabled: Option<bool>,
    num_ctx: Option<usize>,
    model_num_ctx: HashMap<String, usize>,
}

// ─── Request Structures ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    options: Options,
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
struct Message {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OutgoingToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct OutgoingToolCall {
    #[serde(rename = "type")]
    kind: String,
    function: OutgoingFunction,
}

#[derive(Debug, Serialize)]
struct OutgoingFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct Options {
    temperature: f64,
    num_ctx: usize,
}

// ─── Response Structures ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ApiChatResponse {
    message: ResponseMessage,
    #[serde(default)]
    prompt_eval_count: Option<serde_json::Value>,
    #[serde(default)]
    eval_count: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct StreamChatResponse {
    #[serde(default)]
    message: Option<ResponseMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    prompt_eval_count: Option<serde_json::Value>,
    #[serde(default)]
    eval_count: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
    /// Some models return a "thinking" field with internal reasoning
    #[serde(default)]
    thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCall {
    id: Option<String>,
    function: OllamaFunction,
}

#[derive(Debug, Deserialize)]
struct OllamaFunction {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

// ─── Implementation ───────────────────────────────────────────────────────────

impl OllamaProvider {
    pub fn new(base_url: Option<&str>, api_key: Option<&str>) -> Self {
        Self::new_with_reasoning(base_url, api_key, None)
    }

    pub fn new_with_reasoning(base_url: Option<&str>, api_key: Option<&str>, reasoning_enabled: Option<bool>) -> Self {
        Self::new_with_runtime_options(base_url, api_key, reasoning_enabled, None, Vec::new())
    }

    pub fn new_with_runtime_options(
        base_url: Option<&str>,
        api_key: Option<&str>,
        reasoning_enabled: Option<bool>,
        num_ctx: Option<usize>,
        model_num_ctx: Vec<(String, usize)>,
    ) -> Self {
        let api_key = api_key.and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });
        let model_num_ctx = model_num_ctx
            .into_iter()
            .filter(|(model, num_ctx)| !model.trim().is_empty() && *num_ctx > 0)
            .collect();

        Self {
            base_url: base_url
                .unwrap_or("http://localhost:11434")
                .trim_end_matches('/')
                .to_string(),
            api_key,
            reasoning_enabled,
            num_ctx,
            model_num_ctx,
        }
    }

    fn is_local_endpoint(&self) -> bool {
        reqwest::Url::parse(&self.base_url)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.to_string()))
            .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1"))
    }

    fn http_client(&self) -> Client {
        crate::config::build_runtime_proxy_client_with_timeouts("provider.ollama", 300, 10)
            .map_err(|e| {
                tracing::error!("proxy build failed for provider.ollama, using direct: {e}");
                e
            })
            .unwrap_or_else(|_| Client::new())
    }

    fn resolve_request_details(&self, model: &str) -> anyhow::Result<(String, bool)> {
        let requests_cloud = model.ends_with(":cloud");
        let normalized_model = model.strip_suffix(":cloud").unwrap_or(model).to_string();

        if requests_cloud && self.is_local_endpoint() {
            anyhow::bail!(
                "Model '{}' requested cloud routing, but Ollama endpoint is local. Configure api_url with a remote Ollama endpoint.",
                model
            );
        }

        if requests_cloud && self.api_key.is_none() {
            anyhow::bail!(
                "Model '{}' requested cloud routing, but no API key is configured. Set OLLAMA_API_KEY or config api_key.",
                model
            );
        }

        let should_auth = self.api_key.is_some() && !self.is_local_endpoint();

        Ok((normalized_model, should_auth))
    }

    fn parse_tool_arguments(arguments: &str) -> serde_json::Value {
        serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}))
    }

    fn resolve_num_ctx(&self, model: &str) -> usize {
        const DEFAULT_NUM_CTX: usize = 8192;
        let resolved = self
            .num_ctx
            .filter(|value| *value > 0)
            .or_else(|| self.model_num_ctx.get(model).copied())
            .or_else(|| {
                model
                    .rsplit_once('/')
                    .and_then(|(_, bare_model)| self.model_num_ctx.get(bare_model).copied())
            })
            .unwrap_or(DEFAULT_NUM_CTX);
        tracing::debug!(model = model, num_ctx = resolved, "applying Ollama num_ctx");
        resolved
    }

    fn build_chat_request(
        &self,
        messages: Vec<Message>,
        model: &str,
        temperature: f64,
        stream: bool,
        tools: Option<&[serde_json::Value]>,
    ) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages,
            stream,
            options: Options {
                temperature,
                num_ctx: self.resolve_num_ctx(model),
            },
            think: self.reasoning_enabled,
            tools: tools.map(|t| t.to_vec()),
        }
    }

    fn convert_stream_tools(tools: Option<&[crate::tools::ToolSpec]>) -> Option<Vec<serde_json::Value>> {
        let tools = tools?;
        if tools.is_empty() {
            return None;
        }
        Some(
            tools
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    })
                })
                .collect(),
        )
    }

    fn usage_from_eval_counts(
        prompt_eval_count: Option<&serde_json::Value>,
        eval_count: Option<&serde_json::Value>,
    ) -> Option<TokenUsage> {
        let prompt = prompt_eval_count
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        let completion = eval_count
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        let (Some(prompt), Some(completion)) = (prompt, completion) else {
            return None;
        };
        Some(TokenUsage::reported(
            Some(prompt),
            Some(completion),
            Some(prompt.saturating_add(completion)),
        ))
    }

    fn estimate_completion_usage(response: &ChatResponse) -> TokenUsage {
        let chars = response.text.as_deref().unwrap_or("").chars().count()
            + response.reasoning_content.as_deref().unwrap_or("").chars().count();
        let accumulator = ProviderUsageAccumulator::new();
        accumulator.finish_or_estimate_completion_chars(chars)
    }

    fn parse_api_chat_response(&self, response: ApiChatResponse) -> ChatResponse {
        // Route `thinking` to reasoning_content so it does NOT leak into the
        // visible text stream. The chat consumer can drop reasoning from the
        // live UI while history reconstruction still has access to it.
        let reasoning_content = response.message.thinking.as_ref().and_then(|t| {
            let trimmed = t.trim();
            if trimmed.is_empty() { None } else { Some(t.clone()) }
        });

        // Native tool calls returned by the model.
        if !response.message.tool_calls.is_empty() {
            let tool_calls: Vec<ToolCall> = response
                .message
                .tool_calls
                .iter()
                .map(|tc| {
                    let (name, args) = self.extract_tool_name_and_args(tc);
                    ToolCall {
                        id: tc.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        name,
                        arguments: serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string()),
                    }
                })
                .collect();
            let text = if response.message.content.is_empty() {
                None
            } else {
                Some(response.message.content)
            };
            return ChatResponse {
                text,
                tool_calls,
                reasoning_content,
            };
        }

        // Plain text response.
        let content = response.message.content;
        if content.is_empty() {
            if let Some(thinking) = &response.message.thinking {
                // Empty visible content + thinking-only: the model stopped after
                // its internal monologue. Log a warning and surface a polite
                // retry message in the visible text, while preserving the full
                // thinking content in reasoning_content for history fidelity.
                tracing::warn!(
                    "Ollama returned empty content with only thinking: '{}'. Model may have stopped prematurely.",
                    if thinking.len() > 100 {
                        &thinking[..100]
                    } else {
                        thinking
                    }
                );
                return ChatResponse {
                    text: Some(
                        "The model produced only internal reasoning without a final answer. Please try asking again."
                            .to_string(),
                    ),
                    tool_calls: vec![],
                    reasoning_content,
                };
            }
            tracing::warn!("Ollama returned empty content with no tool calls");
        }
        ChatResponse {
            text: Some(content),
            tool_calls: vec![],
            reasoning_content,
        }
    }

    async fn chat_metered(
        &self,
        request: crate::providers::traits::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<(ChatResponse, TokenUsage)> {
        let (normalized_model, should_auth) = self.resolve_request_details(model)?;
        let api_messages = self.convert_messages(request.messages);
        let tool_values = Self::convert_stream_tools(request.tools);
        let response = self
            .send_request(
                api_messages,
                &normalized_model,
                temperature,
                should_auth,
                tool_values.as_deref(),
            )
            .await?;
        let usage = Self::usage_from_eval_counts(response.prompt_eval_count.as_ref(), response.eval_count.as_ref());
        let response = self.parse_api_chat_response(response);
        let tokens_used = usage.unwrap_or_else(|| Self::estimate_completion_usage(&response));
        Ok((response, tokens_used))
    }

    fn convert_user_message_content(&self, content: &str) -> (Option<String>, Option<Vec<String>>) {
        let (cleaned, image_refs) = multimodal::parse_image_markers(content);
        if image_refs.is_empty() {
            return (Some(content.to_string()), None);
        }

        let images: Vec<String> = image_refs
            .iter()
            .filter_map(|reference| multimodal::extract_ollama_image_payload(reference))
            .collect();

        if images.is_empty() {
            return (Some(content.to_string()), None);
        }

        let cleaned = cleaned.trim();
        let content = if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.to_string())
        };

        (content, Some(images))
    }

    /// Convert internal chat history format to Ollama's native tool-call message schema.
    ///
    /// `run_tool_call_loop` stores native assistant/tool entries as JSON strings in
    /// `ChatMessage.content`. We decode those payloads here so follow-up requests send
    /// structured `assistant.tool_calls` and `tool.tool_name`, as expected by Ollama.
    fn convert_messages(&self, messages: &[ChatMessage]) -> Vec<Message> {
        let mut tool_name_by_id: HashMap<String, String> = HashMap::new();

        messages
            .iter()
            .map(|message| {
                if message.role == "assistant" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.content) {
                        if let Some(tool_calls_value) = value.get("tool_calls") {
                            if let Ok(parsed_calls) = serde_json::from_value::<Vec<ToolCall>>(tool_calls_value.clone())
                            {
                                let outgoing_calls: Vec<OutgoingToolCall> = parsed_calls
                                    .into_iter()
                                    .map(|call| {
                                        tool_name_by_id.insert(call.id.clone(), call.name.clone());
                                        OutgoingToolCall {
                                            kind: "function".to_string(),
                                            function: OutgoingFunction {
                                                name: call.name,
                                                arguments: Self::parse_tool_arguments(&call.arguments),
                                            },
                                        }
                                    })
                                    .collect();
                                let content = value
                                    .get("content")
                                    .and_then(serde_json::Value::as_str)
                                    .map(ToString::to_string);
                                return Message {
                                    role: "assistant".to_string(),
                                    content,
                                    images: None,
                                    tool_calls: Some(outgoing_calls),
                                    tool_name: None,
                                };
                            }
                        }
                    }
                }

                if message.role == "tool" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.content) {
                        let tool_name = value
                            .get("tool_name")
                            .and_then(serde_json::Value::as_str)
                            .map(ToString::to_string)
                            .or_else(|| {
                                value
                                    .get("tool_call_id")
                                    .and_then(serde_json::Value::as_str)
                                    .and_then(|id| tool_name_by_id.get(id))
                                    .cloned()
                            });
                        let content = value
                            .get("content")
                            .and_then(serde_json::Value::as_str)
                            .map(ToString::to_string)
                            .or_else(|| (!message.content.trim().is_empty()).then_some(message.content.clone()));

                        return Message {
                            role: "tool".to_string(),
                            content,
                            images: None,
                            tool_calls: None,
                            tool_name,
                        };
                    }
                }

                if message.role == "user" {
                    let (content, images) = self.convert_user_message_content(&message.content);
                    return Message {
                        role: "user".to_string(),
                        content,
                        images,
                        tool_calls: None,
                        tool_name: None,
                    };
                }

                Message {
                    role: message.role.clone(),
                    content: Some(message.content.clone()),
                    images: None,
                    tool_calls: None,
                    tool_name: None,
                }
            })
            .collect()
    }

    /// Send a request to Ollama and get the parsed response.
    /// Pass `tools` to enable native function-calling for models that support it.
    async fn send_request(
        &self,
        messages: Vec<Message>,
        model: &str,
        temperature: f64,
        should_auth: bool,
        tools: Option<&[serde_json::Value]>,
    ) -> anyhow::Result<ApiChatResponse> {
        let request = self.build_chat_request(messages, model, temperature, false, tools);

        let url = format!("{}/api/chat", self.base_url);

        tracing::debug!(
            "Ollama request: url={} model={} message_count={} temperature={} think={:?} tool_count={}",
            url,
            model,
            request.messages.len(),
            temperature,
            request.think,
            request.tools.as_ref().map_or(0, |t| t.len()),
        );

        let mut request_builder = self.http_client().post(&url).json(&request);

        if should_auth {
            if let Some(key) = self.api_key.as_ref() {
                request_builder = request_builder.bearer_auth(key);
            }
        }

        let response = request_builder.send().await?;
        let status = response.status();
        tracing::debug!("Ollama response status: {}", status);

        let body = response.bytes().await?;
        tracing::debug!("Ollama response body length: {} bytes", body.len());

        if !status.is_success() {
            let raw = String::from_utf8_lossy(&body);
            let sanitized = super::sanitize_api_error(&raw);
            tracing::error!("Ollama error response: status={} body_excerpt={}", status, sanitized);
            anyhow::bail!(
                "Ollama API error ({}): {}. Is Ollama running? (brew install ollama && ollama serve)",
                status,
                sanitized
            );
        }

        let chat_response: ApiChatResponse = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                let raw = String::from_utf8_lossy(&body);
                let sanitized = super::sanitize_api_error(&raw);
                tracing::error!(
                    "Ollama response deserialization failed: {e}. body_excerpt={}",
                    sanitized
                );
                anyhow::bail!("Failed to parse Ollama response: {e}");
            }
        };

        Ok(chat_response)
    }

    /// Convert Ollama tool calls to the JSON format expected by parse_tool_calls in loop_.rs
    ///
    /// Handles quirky model behavior where tool calls are wrapped:
    /// - `{"name": "tool_call", "arguments": {"name": "shell", "arguments": {...}}}`
    /// - `{"name": "tool.shell", "arguments": {...}}`
    fn format_tool_calls_for_loop(&self, tool_calls: &[OllamaToolCall]) -> String {
        let formatted_calls: Vec<serde_json::Value> = tool_calls
            .iter()
            .map(|tc| {
                let (tool_name, tool_args) = self.extract_tool_name_and_args(tc);

                // Arguments must be a JSON string for parse_tool_calls compatibility
                let args_str = serde_json::to_string(&tool_args).unwrap_or_else(|_| "{}".to_string());

                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tool_name,
                        "arguments": args_str
                    }
                })
            })
            .collect();

        serde_json::json!({
            "content": "",
            "tool_calls": formatted_calls
        })
        .to_string()
    }

    /// Extract the actual tool name and arguments from potentially nested structures
    fn extract_tool_name_and_args(&self, tc: &OllamaToolCall) -> (String, serde_json::Value) {
        let name = &tc.function.name;
        let args = &tc.function.arguments;

        // Pattern 1: Nested tool_call wrapper (various malformed versions)
        // {"name": "tool_call", "arguments": {"name": "shell", "arguments": {"command": "date"}}}
        // {"name": "tool_call><json", "arguments": {"name": "shell", ...}}
        // {"name": "tool.call", "arguments": {"name": "shell", ...}}
        if name == "tool_call"
            || name == "tool.call"
            || name.starts_with("tool_call>")
            || name.starts_with("tool_call<")
        {
            if let Some(nested_name) = args.get("name").and_then(|v| v.as_str()) {
                let nested_args = args.get("arguments").cloned().unwrap_or_else(|| serde_json::json!({}));
                tracing::debug!(
                    "Unwrapped nested tool call: {} -> {} with args {:?}",
                    name,
                    nested_name,
                    nested_args
                );
                return (nested_name.to_string(), nested_args);
            }
        }

        // Pattern 2: Prefixed tool name (tool.shell, tool.file_read, etc.)
        if let Some(stripped) = name.strip_prefix("tool.") {
            return (stripped.to_string(), args.clone());
        }

        // Pattern 3: Normal tool call
        (name.clone(), args.clone())
    }

    fn tool_call_chunks_from_ollama(tool_calls: Vec<OllamaToolCall>) -> Vec<ToolCallChunk> {
        let helper = Self::new(None, None);
        tool_calls
            .iter()
            .enumerate()
            .map(|(index, tc)| {
                let (name, args) = helper.extract_tool_name_and_args(tc);
                ToolCallChunk::new(
                    tc.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    name,
                    serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string()),
                    index,
                )
            })
            .collect()
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
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
        let (normalized_model, should_auth) = self.resolve_request_details(model)?;

        let mut messages = Vec::new();

        if let Some(sys) = system_prompt {
            messages.push(Message {
                role: "system".to_string(),
                content: Some(sys.to_string()),
                images: None,
                tool_calls: None,
                tool_name: None,
            });
        }

        let (user_content, user_images) = self.convert_user_message_content(message);
        messages.push(Message {
            role: "user".to_string(),
            content: user_content,
            images: user_images,
            tool_calls: None,
            tool_name: None,
        });

        let response = self
            .send_request(messages, &normalized_model, temperature, should_auth, None)
            .await?;

        // If model returned tool calls, format them for loop_.rs's parse_tool_calls
        if !response.message.tool_calls.is_empty() {
            tracing::debug!(
                "Ollama returned {} tool call(s), formatting for loop parser",
                response.message.tool_calls.len()
            );
            return Ok(self.format_tool_calls_for_loop(&response.message.tool_calls));
        }

        // Plain text response
        let content = response.message.content;

        // Handle edge case: model returned only "thinking" with no content or tool calls
        if content.is_empty() {
            if let Some(thinking) = &response.message.thinking {
                tracing::warn!(
                    "Ollama returned empty content with only thinking: '{}'. Model may have stopped prematurely.",
                    if thinking.len() > 100 {
                        &thinking[..100]
                    } else {
                        thinking
                    }
                );
                return Ok(format!(
                    "I was thinking about this: {}... but I didn't complete my response. Could you try asking again?",
                    if thinking.len() > 200 {
                        &thinking[..200]
                    } else {
                        thinking
                    }
                ));
            }
            tracing::warn!("Ollama returned empty content with no tool calls");
        }

        Ok(content)
    }

    async fn chat_with_history(
        &self,
        messages: &[crate::providers::ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let (normalized_model, should_auth) = self.resolve_request_details(model)?;

        let api_messages = self.convert_messages(messages);

        let response = self
            .send_request(api_messages, &normalized_model, temperature, should_auth, None)
            .await?;

        // If model returned tool calls, format them for loop_.rs's parse_tool_calls
        if !response.message.tool_calls.is_empty() {
            tracing::debug!(
                "Ollama returned {} tool call(s), formatting for loop parser",
                response.message.tool_calls.len()
            );
            return Ok(self.format_tool_calls_for_loop(&response.message.tool_calls));
        }

        // Plain text response
        let content = response.message.content;

        // Handle edge case: model returned only "thinking" with no content or tool calls
        // This is a model quirk - it stopped after reasoning without producing output
        if content.is_empty() {
            if let Some(thinking) = &response.message.thinking {
                tracing::warn!(
                    "Ollama returned empty content with only thinking: '{}'. Model may have stopped prematurely.",
                    if thinking.len() > 100 {
                        &thinking[..100]
                    } else {
                        thinking
                    }
                );
                // Return a message indicating the model's thought process but no action
                return Ok(format!(
                    "I was thinking about this: {}... but I didn't complete my response. Could you try asking again?",
                    if thinking.len() > 200 {
                        &thinking[..200]
                    } else {
                        thinking
                    }
                ));
            }
            tracing::warn!("Ollama returned empty content with no tool calls");
        }

        Ok(content)
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let (normalized_model, should_auth) = self.resolve_request_details(model)?;

        let api_messages = self.convert_messages(messages);

        // Tools arrive pre-formatted in OpenAI/Ollama-compatible JSON from
        // tools_to_openai_format() in loop_.rs — pass them through directly.
        let tools_opt = if tools.is_empty() { None } else { Some(tools) };

        let response = self
            .send_request(api_messages, &normalized_model, temperature, should_auth, tools_opt)
            .await?;

        // Route `thinking` to reasoning_content so it does NOT leak into the
        // visible text stream. The chat consumer can drop reasoning from the
        // live UI while history reconstruction still has access to it.
        let reasoning_content = response.message.thinking.as_ref().and_then(|t| {
            let trimmed = t.trim();
            if trimmed.is_empty() { None } else { Some(t.clone()) }
        });

        // Native tool calls returned by the model.
        if !response.message.tool_calls.is_empty() {
            let tool_calls: Vec<ToolCall> = response
                .message
                .tool_calls
                .iter()
                .map(|tc| {
                    let (name, args) = self.extract_tool_name_and_args(tc);
                    ToolCall {
                        id: tc.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        name,
                        arguments: serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string()),
                    }
                })
                .collect();
            let text = if response.message.content.is_empty() {
                None
            } else {
                Some(response.message.content)
            };
            return Ok(ChatResponse {
                text,
                tool_calls,
                reasoning_content,
            });
        }

        // Plain text response.
        let content = response.message.content;
        if content.is_empty() {
            if let Some(thinking) = &response.message.thinking {
                // Empty visible content + thinking-only: the model stopped after
                // its internal monologue. Log a warning and surface a polite
                // retry message in the visible text, while preserving the full
                // thinking content in reasoning_content for history fidelity.
                tracing::warn!(
                    "Ollama returned empty content with only thinking: '{}'. Model may have stopped prematurely.",
                    if thinking.len() > 100 {
                        &thinking[..100]
                    } else {
                        thinking
                    }
                );
                return Ok(ChatResponse {
                    text: Some(
                        "The model produced only internal reasoning without a final answer. Please try asking again."
                            .to_string(),
                    ),
                    tool_calls: vec![],
                    reasoning_content,
                });
            }
            tracing::warn!("Ollama returned empty content with no tool calls");
        }
        Ok(ChatResponse {
            text: Some(content),
            tool_calls: vec![],
            reasoning_content,
        })
    }

    async fn chat(
        &self,
        request: crate::providers::traits::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        Ok(self.chat_metered(request, model, temperature).await?.0)
    }

    async fn chat_traced(
        &self,
        request: crate::providers::traits::ChatRequest<'_>,
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
                provider: "ollama".to_string(),
                model: model.to_string(),
                started_at,
                finished_at,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "ollama".to_string(),
            final_model: model.to_string(),
            tokens_used,
        })
    }

    fn supports_native_tools(&self) -> bool {
        // Ollama's /api/chat supports native function-calling for capable models
        // (qwen2.5, llama3.1, mistral-nemo, etc.). chat_with_tools() sends tool
        // definitions in the request and returns structured ToolCall objects.
        true
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let (normalized_model, should_auth) = match self.resolve_request_details(model) {
            Ok(details) => details,
            Err(err) => {
                return stream::once(async move { Err(StreamError::Provider(err.to_string())) }).boxed();
            }
        };
        let request = self.build_chat_request(
            self.convert_messages(messages),
            &normalized_model,
            temperature,
            true,
            Self::convert_stream_tools(options.tools.as_deref()).as_deref(),
        );
        let url = format!("{}/api/chat", self.base_url);
        let client = self.http_client();
        let api_key = self.api_key.clone();
        let count_tokens = options.count_tokens;

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);
        tokio::spawn(async move {
            let mut req_builder = client.post(&url).json(&request);
            if should_auth {
                if let Some(key) = api_key.as_ref() {
                    req_builder = req_builder.bearer_auth(key);
                }
            }

            let response = match req_builder.send().await {
                Ok(response) => response,
                Err(err) => {
                    let _ = tx.send(Err(StreamError::Http(err))).await;
                    return;
                }
            };

            if !response.status().is_success() {
                let _ = tx.send(Err(super::stream_api_error("Ollama", response).await)).await;
                return;
            }

            let mut buffer = String::new();
            let mut bytes_stream = response.bytes_stream();
            while let Some(item) = bytes_stream.next().await {
                let bytes = match item {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        let _ = tx.send(Err(StreamError::Http(err))).await;
                        return;
                    }
                };
                let text = match String::from_utf8(bytes.to_vec()) {
                    Ok(text) => text,
                    Err(err) => {
                        let _ = tx
                            .send(Err(StreamError::InvalidSse(format!("Invalid UTF-8: {err}"))))
                            .await;
                        return;
                    }
                };
                buffer.push_str(&text);

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer.drain(..=pos).collect::<String>();
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    let parsed: StreamChatResponse = match serde_json::from_str(line) {
                        Ok(parsed) => parsed,
                        Err(err) => {
                            let _ = tx
                                .send(Err(StreamError::InvalidSse(format!(
                                    "Invalid Ollama stream JSON: {err}"
                                ))))
                                .await;
                            return;
                        }
                    };

                    if let Some(message) = parsed.message {
                        if !message.content.is_empty() {
                            let mut chunk = StreamChunk::delta(message.content);
                            if count_tokens {
                                chunk = chunk.with_token_estimate();
                            }
                            if tx.send(Ok(chunk)).await.is_err() {
                                return;
                            }
                        }
                        if let Some(thinking) = message.thinking.filter(|value| !value.is_empty()) {
                            let mut chunk = StreamChunk::reasoning_delta(thinking);
                            if count_tokens {
                                chunk = chunk.with_token_estimate();
                            }
                            if tx.send(Ok(chunk)).await.is_err() {
                                return;
                            }
                        }
                        if !message.tool_calls.is_empty() {
                            let chunk =
                                StreamChunk::tool_call_chunk(Self::tool_call_chunks_from_ollama(message.tool_calls));
                            if tx.send(Ok(chunk)).await.is_err() {
                                return;
                            }
                        }
                    }

                    if parsed.done {
                        if let Some(usage) =
                            Self::usage_from_eval_counts(parsed.prompt_eval_count.as_ref(), parsed.eval_count.as_ref())
                            && tx.send(Ok(StreamChunk::usage(usage))).await.is_err()
                        {
                            return;
                        }
                        let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
                        return;
                    }
                }
            }

            let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
        });

        stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let mut messages = Vec::new();
        if let Some(system) = system_prompt {
            messages.push(ChatMessage::system(system));
        }
        messages.push(ChatMessage::user(message));
        self.stream_chat_with_history(&messages, model, temperature, options)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_url() {
        let p = OllamaProvider::new(None, None);
        assert_eq!(p.base_url, "http://localhost:11434");
    }

    #[test]
    fn custom_url_trailing_slash() {
        let p = OllamaProvider::new(Some("http://192.168.1.100:11434/"), None);
        assert_eq!(p.base_url, "http://192.168.1.100:11434");
    }

    #[test]
    fn custom_url_no_trailing_slash() {
        let p = OllamaProvider::new(Some("http://myserver:11434"), None);
        assert_eq!(p.base_url, "http://myserver:11434");
    }

    #[test]
    fn empty_url_uses_empty() {
        let p = OllamaProvider::new(Some(""), None);
        assert_eq!(p.base_url, "");
    }

    #[test]
    fn cloud_suffix_strips_model_name() {
        let p = OllamaProvider::new(Some("https://ollama.com"), Some("ollama-key"));
        let (model, should_auth) = p.resolve_request_details("qwen3:cloud").unwrap();
        assert_eq!(model, "qwen3");
        assert!(should_auth);
    }

    #[test]
    fn cloud_suffix_with_local_endpoint_errors() {
        let p = OllamaProvider::new(None, Some("ollama-key"));
        let error = p
            .resolve_request_details("qwen3:cloud")
            .expect_err("cloud suffix should fail on local endpoint");
        assert!(
            error
                .to_string()
                .contains("requested cloud routing, but Ollama endpoint is local")
        );
    }

    #[test]
    fn cloud_suffix_without_api_key_errors() {
        let p = OllamaProvider::new(Some("https://ollama.com"), None);
        let error = p
            .resolve_request_details("qwen3:cloud")
            .expect_err("cloud suffix should require API key");
        assert!(
            error
                .to_string()
                .contains("requested cloud routing, but no API key is configured")
        );
    }

    #[test]
    fn remote_endpoint_auth_enabled_when_key_present() {
        let p = OllamaProvider::new(Some("https://ollama.com"), Some("ollama-key"));
        let (_model, should_auth) = p.resolve_request_details("qwen3").unwrap();
        assert!(should_auth);
    }

    #[test]
    fn local_endpoint_auth_disabled_even_with_key() {
        let p = OllamaProvider::new(None, Some("ollama-key"));
        let (_model, should_auth) = p.resolve_request_details("llama3").unwrap();
        assert!(!should_auth);
    }

    #[test]
    fn request_omits_think_when_reasoning_not_configured() {
        let provider = OllamaProvider::new(None, None);
        let request = provider.build_chat_request(
            vec![Message {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                images: None,
                tool_calls: None,
                tool_name: None,
            }],
            "llama3",
            0.7,
            false,
            None,
        );

        let json = serde_json::to_value(request).unwrap();
        assert!(json.get("think").is_none());
    }

    #[test]
    fn request_includes_think_when_reasoning_configured() {
        let provider = OllamaProvider::new_with_reasoning(None, None, Some(false));
        let request = provider.build_chat_request(
            vec![Message {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                images: None,
                tool_calls: None,
                tool_name: None,
            }],
            "llama3",
            0.7,
            false,
            None,
        );

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json.get("think"), Some(&serde_json::json!(false)));
    }

    fn user_message(content: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: Some(content.to_string()),
            images: None,
            tool_calls: None,
            tool_name: None,
        }
    }

    fn request_num_ctx(provider: &OllamaProvider, model: &str) -> usize {
        let request = provider.build_chat_request(vec![user_message("hello")], model, 0.7, false, None);
        let json = serde_json::to_value(request).unwrap();
        json.pointer("/options/num_ctx")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .expect("request serializes options.num_ctx")
    }

    #[test]
    fn request_num_ctx_defaults_to_8192() {
        let provider = OllamaProvider::new(None, None);

        assert_eq!(request_num_ctx(&provider, "llama3"), 8192);
    }

    #[test]
    fn request_num_ctx_uses_router_model_context() {
        let provider =
            OllamaProvider::new_with_runtime_options(None, None, None, None, vec![("llama3".to_string(), 32_768)]);

        assert_eq!(request_num_ctx(&provider, "llama3"), 32_768);
        assert_eq!(request_num_ctx(&provider, "ollama/llama3"), 32_768);
    }

    #[test]
    fn request_num_ctx_explicit_config_overrides_router_model_context() {
        let provider = OllamaProvider::new_with_runtime_options(
            None,
            None,
            None,
            Some(65_536),
            vec![("llama3".to_string(), 32_768)],
        );

        assert_eq!(request_num_ctx(&provider, "llama3"), 65_536);
    }

    #[test]
    fn final_eval_counts_map_to_reported_usage() {
        let response: ApiChatResponse = serde_json::from_str(
            r#"{
                "message": {"role": "assistant", "content": "done"},
                "prompt_eval_count": 31,
                "eval_count": 9
            }"#,
        )
        .unwrap();

        let usage =
            OllamaProvider::usage_from_eval_counts(response.prompt_eval_count.as_ref(), response.eval_count.as_ref())
                .unwrap();

        assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Reported);
        assert_eq!(usage.prompt_tokens, Some(31));
        assert_eq!(usage.completion_tokens, Some(9));
        assert_eq!(usage.total_tokens, Some(40));
    }

    #[test]
    fn malformed_or_partial_eval_counts_do_not_fabricate_reported_usage() {
        let response: StreamChatResponse = serde_json::from_str(
            r#"{
                "done": true,
                "prompt_eval_count": "bad",
                "eval_count": 9
            }"#,
        )
        .unwrap();

        assert!(
            OllamaProvider::usage_from_eval_counts(response.prompt_eval_count.as_ref(), response.eval_count.as_ref())
                .is_none(),
            "malformed/partial final stats must fall back to estimate outside the parser"
        );
    }

    #[test]
    fn request_includes_stream_when_enabled() {
        let provider = OllamaProvider::new(None, None);
        let request = provider.build_chat_request(
            vec![Message {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                images: None,
                tool_calls: None,
                tool_name: None,
            }],
            "llama3",
            0.7,
            true,
            None,
        );

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json.get("stream"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn supports_streaming_returns_true() {
        let provider = OllamaProvider::new(None, None);
        assert!(provider.supports_streaming());
    }

    #[test]
    fn response_deserializes() {
        let json = r#"{"message":{"role":"assistant","content":"Hello from Ollama!"}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message.content, "Hello from Ollama!");
    }

    #[test]
    fn response_with_empty_content() {
        let json = r#"{"message":{"role":"assistant","content":""}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.message.content.is_empty());
    }

    #[test]
    fn response_with_missing_content_defaults_to_empty() {
        let json = r#"{"message":{"role":"assistant"}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.message.content.is_empty());
    }

    #[test]
    fn response_with_thinking_field_extracts_content() {
        let json = r#"{"message":{"role":"assistant","content":"hello","thinking":"internal reasoning"}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message.content, "hello");
        // Thinking is parsed into its own field — never merged into content.
        assert_eq!(resp.message.thinking.as_deref(), Some("internal reasoning"));
    }

    /// Test helper mirroring the body of `chat_with_tools` that routes
    /// `thinking` into `reasoning_content` and leaves visible text clean.
    /// Lets us cover the routing logic without a real HTTP server.
    fn route_response_for_tools(resp: ApiChatResponse) -> ChatResponse {
        let reasoning_content = resp.message.thinking.as_ref().and_then(|t| {
            let trimmed = t.trim();
            if trimmed.is_empty() { None } else { Some(t.clone()) }
        });

        if !resp.message.tool_calls.is_empty() {
            let text = if resp.message.content.is_empty() {
                None
            } else {
                Some(resp.message.content)
            };
            return ChatResponse {
                text,
                tool_calls: vec![],
                reasoning_content,
            };
        }

        let content = resp.message.content;
        if content.is_empty() {
            if resp.message.thinking.is_some() {
                return ChatResponse {
                    text: Some(
                        "The model produced only internal reasoning without a final answer. Please try asking again."
                            .to_string(),
                    ),
                    tool_calls: vec![],
                    reasoning_content,
                };
            }
        }
        ChatResponse {
            text: Some(content),
            tool_calls: vec![],
            reasoning_content,
        }
    }

    #[test]
    fn ollama_response_routes_thinking_to_reasoning_content_field() {
        let json = r#"{"message":{"role":"assistant","content":"Visible answer.","thinking":"Step 1: parse... Step 2: respond."}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let routed = route_response_for_tools(resp);

        // Visible text stays clean of reasoning.
        assert_eq!(routed.text.as_deref(), Some("Visible answer."));
        assert_eq!(
            routed.reasoning_content.as_deref(),
            Some("Step 1: parse... Step 2: respond.")
        );
        assert!(routed.tool_calls.is_empty());
    }

    #[test]
    fn ollama_thinking_only_response_does_not_leak_thinking_into_text() {
        let json = r#"{"message":{"role":"assistant","content":"","thinking":"I considered the question but did not answer."}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let routed = route_response_for_tools(resp);

        // Visible text MUST NOT contain the internal thinking — only a neutral retry hint.
        let text = routed.text.as_deref().unwrap_or("");
        assert!(
            !text.contains("I considered the question"),
            "thinking content leaked into visible text: {text}"
        );
        // But reasoning_content preserves the original thinking for history.
        assert_eq!(
            routed.reasoning_content.as_deref(),
            Some("I considered the question but did not answer.")
        );
    }

    #[test]
    fn ollama_response_without_thinking_keeps_reasoning_none() {
        let json = r#"{"message":{"role":"assistant","content":"Just an answer."}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let routed = route_response_for_tools(resp);

        assert_eq!(routed.text.as_deref(), Some("Just an answer."));
        assert!(routed.reasoning_content.is_none());
    }

    #[test]
    fn response_with_tool_calls_parses_correctly() {
        let json = r#"{"message":{"role":"assistant","content":"","tool_calls":[{"id":"call_123","function":{"name":"shell","arguments":{"command":"date"}}}]}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.message.content.is_empty());
        assert_eq!(resp.message.tool_calls.len(), 1);
        assert_eq!(resp.message.tool_calls[0].function.name, "shell");
    }

    #[test]
    fn extract_tool_name_handles_nested_tool_call() {
        let provider = OllamaProvider::new(None, None);
        let tc = OllamaToolCall {
            id: Some("call_123".into()),
            function: OllamaFunction {
                name: "tool_call".into(),
                arguments: serde_json::json!({
                    "name": "shell",
                    "arguments": {"command": "date"}
                }),
            },
        };
        let (name, args) = provider.extract_tool_name_and_args(&tc);
        assert_eq!(name, "shell");
        assert_eq!(args.get("command").unwrap(), "date");
    }

    #[test]
    fn extract_tool_name_handles_prefixed_name() {
        let provider = OllamaProvider::new(None, None);
        let tc = OllamaToolCall {
            id: Some("call_123".into()),
            function: OllamaFunction {
                name: "tool.shell".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
        };
        let (name, args) = provider.extract_tool_name_and_args(&tc);
        assert_eq!(name, "shell");
        assert_eq!(args.get("command").unwrap(), "ls");
    }

    #[test]
    fn extract_tool_name_handles_normal_call() {
        let provider = OllamaProvider::new(None, None);
        let tc = OllamaToolCall {
            id: Some("call_123".into()),
            function: OllamaFunction {
                name: "file_read".into(),
                arguments: serde_json::json!({"path": "/tmp/test"}),
            },
        };
        let (name, args) = provider.extract_tool_name_and_args(&tc);
        assert_eq!(name, "file_read");
        assert_eq!(args.get("path").unwrap(), "/tmp/test");
    }

    #[test]
    fn format_tool_calls_produces_valid_json() {
        let provider = OllamaProvider::new(None, None);
        let tool_calls = vec![OllamaToolCall {
            id: Some("call_abc".into()),
            function: OllamaFunction {
                name: "shell".into(),
                arguments: serde_json::json!({"command": "date"}),
            },
        }];

        let formatted = provider.format_tool_calls_for_loop(&tool_calls);
        let parsed: serde_json::Value = serde_json::from_str(&formatted).unwrap();

        assert!(parsed.get("tool_calls").is_some());
        let calls = parsed.get("tool_calls").unwrap().as_array().unwrap();
        assert_eq!(calls.len(), 1);

        let func = calls[0].get("function").unwrap();
        assert_eq!(func.get("name").unwrap(), "shell");
        // arguments should be a string (JSON-encoded)
        assert!(func.get("arguments").unwrap().is_string());
    }

    #[test]
    fn convert_messages_parses_native_assistant_tool_calls() {
        let provider = OllamaProvider::new(None, None);
        let messages = vec![ChatMessage {
            role: "assistant".into(),
            content:
                r#"{"content":null,"tool_calls":[{"id":"call_1","name":"shell","arguments":"{\"command\":\"ls\"}"}]}"#
                    .into(),
        }];

        let converted = provider.convert_messages(&messages);

        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "assistant");
        assert!(converted[0].content.is_none());
        let calls = converted[0].tool_calls.as_ref().expect("tool calls expected");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "shell");
        assert_eq!(calls[0].function.arguments.get("command").unwrap(), "ls");
    }

    #[test]
    fn convert_messages_maps_tool_result_call_id_to_tool_name() {
        let provider = OllamaProvider::new(None, None);
        let messages = vec![
            ChatMessage {
                role: "assistant".into(),
                content: r#"{"content":null,"tool_calls":[{"id":"call_7","name":"file_read","arguments":"{\"path\":\"README.md\"}"}]}"#.into(),
            },
            ChatMessage {
                role: "tool".into(),
                content: r#"{"tool_call_id":"call_7","content":"ok"}"#.into(),
            },
        ];

        let converted = provider.convert_messages(&messages);

        assert_eq!(converted.len(), 2);
        assert_eq!(converted[1].role, "tool");
        assert_eq!(converted[1].tool_name.as_deref(), Some("file_read"));
        assert_eq!(converted[1].content.as_deref(), Some("ok"));
        assert!(converted[1].tool_calls.is_none());
    }

    #[test]
    fn convert_messages_extracts_images_from_user_marker() {
        let provider = OllamaProvider::new(None, None);
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "Inspect this screenshot [IMAGE:data:image/png;base64,abcd==]".into(),
        }];

        let converted = provider.convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
        assert_eq!(converted[0].content.as_deref(), Some("Inspect this screenshot"));
        let images = converted[0].images.as_ref().expect("images should be present");
        assert_eq!(images, &vec!["abcd==".to_string()]);
    }

    #[test]
    fn capabilities_include_native_tools_and_vision() {
        let provider = OllamaProvider::new(None, None);
        let caps = <OllamaProvider as Provider>::capabilities(&provider);
        assert!(caps.native_tool_calling);
        assert!(caps.vision);
    }
}
