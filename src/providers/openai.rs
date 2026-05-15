use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, Provider, StreamChunk,
    StreamError, StreamOptions, StreamResult, ToolCall as ProviderToolCall, ToolCallChunk,
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
    let Some(choice) = parsed.choices.into_iter().next() else {
        return Ok(None);
    };

    let mut event = OpenAiSseEvent {
        finish_reason: choice.finish_reason,
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
fn apply_tool_call_deltas(buf: &mut Vec<ToolCallBuffer>, deltas: Vec<StreamSseToolCall>) {
    for delta in deltas {
        let slot = delta.index;
        while buf.len() <= slot {
            buf.push(ToolCallBuffer::default());
        }
        // Safety: just expanded so slot is in-bounds.
        let entry = match buf.get_mut(slot) {
            Some(e) => e,
            None => continue,
        };
        if let Some(id) = delta.id {
            if !id.is_empty() {
                entry.id = id;
            }
        }
        if let Some(func) = delta.function {
            if let Some(name) = func.name {
                if !name.is_empty() {
                    entry.name = name;
                }
            }
            if let Some(args) = func.arguments {
                entry.arguments.push_str(&args);
            }
        }
    }
}

/// Flush the tool_call buffer into chunks suitable for [`StreamChunk::tool_call_chunk`].
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
        let message = native_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| anyhow::anyhow!("No response from OpenAI"))?;
        Ok(Self::parse_native_response(message))
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
    /// [`parse_openai_sse_line`], and emits a single [`ToolCallChunk`] per
    /// in-flight tool call when `finish_reason == "tool_calls"` arrives (so
    /// driver can execute and feed `tool_results` back to history).
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

        #[derive(Serialize)]
        struct StreamingChatRequest {
            model: String,
            messages: Vec<NativeMessage>,
            temperature: f64,
            stream: bool,
        }

        let request_body = StreamingChatRequest {
            model: model.to_string(),
            messages: native_messages,
            temperature,
            stream: true,
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
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let preview = body.chars().take(300).collect::<String>();
                let _ = tx
                    .send(Err(StreamError::Provider(format!(
                        "OpenAI streaming HTTP {status}: {preview}"
                    ))))
                    .await;
                return;
            }

            let mut tool_buf: Vec<ToolCallBuffer> = Vec::new();
            let mut byte_stream = response.bytes_stream();
            let mut text_buf = String::new();
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
                                "non-utf8 byte in OpenAI SSE: {e}"
                            ))))
                            .await;
                        return;
                    }
                };
                text_buf.push_str(&text);

                while let Some(pos) = text_buf.find('\n') {
                    let line: String = text_buf.drain(..=pos).collect();
                    let event = match parse_openai_sse_line(&line) {
                        Ok(Some(ev)) => ev,
                        Ok(None) => continue,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    };

                    if event.done_sentinel {
                        let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
                        sent_final = true;
                        break 'outer;
                    }

                    if !event.tool_call_deltas.is_empty() {
                        apply_tool_call_deltas(&mut tool_buf, event.tool_call_deltas);
                    }

                    if let Some(content) = event.content {
                        let mut chunk = StreamChunk::delta(content);
                        if options.count_tokens {
                            chunk = chunk.with_token_estimate();
                        }
                        if tx.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }
                    if let Some(reasoning) = event.reasoning {
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
                        // finish_reason==stop|length|content_filter|tool_calls all
                        // close the turn. We emit `final_chunk` to be safe even
                        // when the server forgets `[DONE]`.
                        let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
                        sent_final = true;
                        break 'outer;
                    }
                }
            }

            if !sent_final {
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
        apply_tool_call_deltas(
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
        apply_tool_call_deltas(
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
        let flushed = flush_tool_call_buffer(&buf);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].id, "call_1");
        assert_eq!(flushed[0].name, "shell");
        assert_eq!(flushed[0].args, r#"{"a": 1}"#);
        assert_eq!(flushed[0].index, 0);
    }

    #[test]
    fn tool_call_buffer_supports_parallel_calls_by_index() {
        let mut buf: Vec<ToolCallBuffer> = Vec::new();
        apply_tool_call_deltas(
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
        let flushed = flush_tool_call_buffer(&buf);
        assert_eq!(flushed.len(), 2);
        assert_eq!(flushed[0].name, "ls");
        assert_eq!(flushed[1].name, "pwd");
        assert_eq!(flushed[1].index, 1);
    }

    #[test]
    fn tool_call_buffer_skips_unnamed_entries() {
        // Some servers send empty `arguments` chunks before name appears; we
        // must not emit unnamed/empty placeholder calls.
        let mut buf: Vec<ToolCallBuffer> = Vec::new();
        apply_tool_call_deltas(
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
        assert!(flush_tool_call_buffer(&buf).is_empty());
    }

    /// Test helper: replay a sequence of SSE lines through the parser to
    /// assert the driver-facing chunk shape (delta / tool_call / final).
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
                apply_tool_call_deltas(&mut tool_buf, event.tool_call_deltas);
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
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].args, r#"{"cmd":"ls"}"#);
        assert_eq!(calls[0].index, 0);
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
        let lines = [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_z","function":{"name":"edit"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"p\""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"a.rs\","}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"l\":1}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let (_text, calls, _final, _finish) = replay_openai_sse(&lines);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "edit");
        assert_eq!(calls[0].args, r#"{"p":"a.rs","l":1}"#);
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
}
