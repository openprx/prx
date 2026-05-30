use crate::llm::route_decision::{ProviderExecutionOutcome, RouteDecision};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use std::fmt::Write;

/// A single message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
        }
    }
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// An LLM response that may contain text, tool calls, or both.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// Text content of the response (may be empty if only tool calls).
    pub text: Option<String>,
    /// Tool calls requested by the LLM.
    pub tool_calls: Vec<ToolCall>,
    /// Reasoning/thinking content from thinking-mode models (e.g. Kimi Code).
    /// Preserved across turns so the provider can re-attach it to history messages.
    pub reasoning_content: Option<String>,
}

impl ChatResponse {
    /// True when the LLM wants to invoke at least one tool.
    pub const fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Convenience: return text content or empty string.
    pub fn text_or_empty(&self) -> &str {
        self.text.as_deref().unwrap_or("")
    }
}

/// Request payload for provider chat calls.
#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub tools: Option<&'a [ToolSpec]>,
}

/// A tool result to feed back to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub content: String,
}

/// A message in a multi-turn conversation, including tool interactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ConversationMessage {
    /// Regular chat message (system, user, assistant).
    Chat(ChatMessage),
    /// Tool calls from the assistant (stored for history fidelity).
    AssistantToolCalls {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    /// Results of tool executions, fed back to the LLM.
    ToolResults(Vec<ToolResultMessage>),
}

/// Lifecycle status of a [`ToolCallChunk`] in the two-phase streaming protocol.
///
/// **S3 T3-0**: providers may emit tool-call information either as a single
/// fully-buffered `Completed` chunk (legacy "completion protocol") or as a
/// sequence of `Streaming` chunks carrying argument deltas followed by a final
/// `Completed` chunk with full args (incremental protocol).
///
/// Driver consumers MUST treat both protocols as semantically equivalent at
/// turn boundaries: aggregating `Streaming` deltas and reading `Completed.args`
/// MUST yield the same JSON string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ToolCallChunkStatus {
    /// Partial tool-call fragment carrying an `arguments_delta`. `args` SHOULD
    /// be empty in this state; consumers MUST NOT rely on `args` until a
    /// `Completed` chunk arrives.
    Streaming,
    /// Final tool-call chunk. `args` contains the full JSON argument string
    /// the provider has buffered (or aggregated from preceding `Streaming`
    /// deltas). `arguments_delta` MUST be `None`.
    #[default]
    Completed,
}

/// A tool call surfaced through a streaming response.
///
/// **5a-5 协议层**: 当 provider 在 SSE 流中识别到 LLM 要调用工具，emit 一个
/// 携带 `tool_calls` 的 `StreamChunk`。`index` 用来区分单轮内并发 tool call。
///
/// **S3 T3-0 协议扩展（双阶段）**: 引入 `status` + `arguments_delta` 两个字段，
/// 在保持向后兼容的前提下支持增量（incremental）发布工具参数：
///
/// 1. **Legacy / Completion 协议**（OpenAI/Anthropic 当前实现）: provider 在
///    SSE 累积完成后 emit 单个 `status = Completed`、`args = 完整 JSON`、
///    `arguments_delta = None` 的 chunk。driver 直接读取 `args` 执行。
/// 2. **Streaming / Incremental 协议**（S3 T3-2 才有 provider 真实启用）:
///    provider 在解析到第一个 `input_json_delta` 时 emit 一个或多个
///    `status = Streaming`、`args = ""`、`arguments_delta = Some(片段)` 的 chunk，
///    最后一个 chunk 必须是 `status = Completed`、`args = 完整 JSON`、
///    `arguments_delta = None`。driver 聚合所有 deltas 应等于 Completed.args。
///
/// **不变量**:
/// - `id` 与 `name` 在同一 tool call 的整个 chunk 序列中必须保持一致；
/// - 任意时刻，`Streaming` chunk 与对应 `Completed` chunk 共享相同 `index`；
/// - `Completed.arguments_delta` 必须为 `None`；
/// - `Streaming.args` 应为 `""`（消费端不依赖该字段）；
/// - 旧 provider 不需要任何改动（构造器 `ToolCallChunk::new` 默认 `Completed`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ToolCallChunk {
    /// LLM 给出的 tool_call_id（用作 result 回填的关联键）.
    pub id: String,
    /// 工具名（必须匹配 tools_registry 注册的 ToolSpec.name）.
    pub name: String,
    /// 完整参数 JSON 字符串（provider 已经把 SSE delta 拼成完整 JSON）.
    ///
    /// 在 `Streaming` 状态下应为空串；在 `Completed` 状态下为完整 JSON。
    pub args: String,
    /// 同一轮内并发 tool call 的序号（0..N）.
    pub index: usize,
    /// **S3 T3-0**: 增量协议参数片段。仅在 `status = Streaming` 时为 `Some`，
    /// 携带本次 SSE 帧的 `arguments` 增量子串；`Completed` 时必须为 `None`。
    pub arguments_delta: Option<String>,
    /// **S3 T3-0**: 双阶段协议生命周期标记。
    /// 默认 `Completed`，保证旧 provider 与 `ToolCallChunk::new` 路径
    /// 无需改动即继续工作。
    pub status: ToolCallChunkStatus,
}

impl ToolCallChunk {
    /// Construct a fully-buffered (`Completed`) tool call chunk.
    ///
    /// This is the legacy / "completion protocol" path used by OpenAI and
    /// Anthropic today; `arguments_delta` is set to `None` and `status` to
    /// [`ToolCallChunkStatus::Completed`].
    pub fn new(id: impl Into<String>, name: impl Into<String>, args: impl Into<String>, index: usize) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            args: args.into(),
            index,
            arguments_delta: None,
            status: ToolCallChunkStatus::Completed,
        }
    }

    /// **S3 T3-0**: Construct a partial (`Streaming`) tool-call chunk carrying
    /// an argument-fragment delta. `args` is intentionally left empty — only
    /// the matching `Completed` chunk (or aggregation of all `Streaming` deltas)
    /// carries the final JSON.
    pub fn streaming_delta(
        id: impl Into<String>,
        name: impl Into<String>,
        delta: impl Into<String>,
        index: usize,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            args: String::new(),
            index,
            arguments_delta: Some(delta.into()),
            status: ToolCallChunkStatus::Streaming,
        }
    }
}

/// A chunk of content from a streaming response.
///
/// Reasoning/thinking content (Anthropic `thinking` blocks, OpenAI
/// `reasoning_content`, Ollama `thinking`) is intentionally separated from the
/// visible `delta` text so chat consumers can choose whether to render it.
/// The default chat consumer drops reasoning from the live stream and only
/// preserves the final aggregated text + reasoning in conversation history.
///
/// **5a-5 扩展**：新增 `tool_calls` 字段（默认空 vec，向后兼容）。当 provider
/// 在 streaming 中识别 LLM 要调用工具，emit 一个携带 tool_calls 的 chunk；
/// driver 检测到非空 tool_calls 时进入工具回合循环。
#[derive(Debug, Clone, Default)]
pub struct StreamChunk {
    /// Visible text delta for this chunk (assistant's "spoken" output).
    pub delta: String,
    /// Reasoning/thinking delta for this chunk (model's internal monologue).
    /// `None` for ordinary text chunks; `Some` only when the provider emits a
    /// dedicated reasoning event.
    pub reasoning: Option<String>,
    /// Whether this is the final chunk.
    pub is_final: bool,
    /// Approximate token count for this chunk (estimated).
    pub token_count: usize,
    /// **5a-5**: Tool calls surfaced in this chunk. Empty for ordinary text;
    /// non-empty when the provider has parsed a tool_use block from the stream.
    ///
    /// Provider 负责把所有 SSE event 累积成完整 tool_call (id + name + args)
    /// 再 emit 单个 chunk；driver 不做增量解析。
    pub tool_calls: Vec<ToolCallChunk>,
}

impl StreamChunk {
    /// Create a new non-final chunk carrying visible text.
    pub fn delta(text: impl Into<String>) -> Self {
        Self {
            delta: text.into(),
            reasoning: None,
            is_final: false,
            token_count: 0,
            tool_calls: Vec::new(),
        }
    }

    /// Create a non-final chunk carrying reasoning/thinking content only.
    /// The visible `delta` is left empty; consumers should not display this
    /// as primary output.
    pub fn reasoning_delta(text: impl Into<String>) -> Self {
        Self {
            delta: String::new(),
            reasoning: Some(text.into()),
            is_final: false,
            token_count: 0,
            tool_calls: Vec::new(),
        }
    }

    /// **5a-5**: Create a non-final chunk carrying tool calls only.
    ///
    /// Provider 在 streaming 中识别 LLM 要调用工具时，emit 此变体。`delta` 留空
    /// 让 UI 不把 tool_call 当文本 token 渲染。driver 检测到 `tool_calls.is_empty()
    /// == false` 时进入工具执行回合。
    pub const fn tool_call_chunk(calls: Vec<ToolCallChunk>) -> Self {
        Self {
            delta: String::new(),
            reasoning: None,
            is_final: false,
            token_count: 0,
            tool_calls: calls,
        }
    }

    /// Create a final chunk.
    pub const fn final_chunk() -> Self {
        Self {
            delta: String::new(),
            reasoning: None,
            is_final: true,
            token_count: 0,
            tool_calls: Vec::new(),
        }
    }

    /// Create an error chunk.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            delta: message.into(),
            reasoning: None,
            is_final: true,
            token_count: 0,
            tool_calls: Vec::new(),
        }
    }

    /// True when this chunk carries only reasoning (no visible delta).
    pub const fn is_reasoning_only(&self) -> bool {
        self.delta.is_empty() && self.reasoning.is_some()
    }

    /// **5a-5**: True when this chunk carries tool calls (regardless of delta).
    pub const fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Estimate tokens (rough approximation: ~4 chars per token).
    /// Counts both visible delta and reasoning content.
    pub const fn with_token_estimate(mut self) -> Self {
        let reasoning_len = match &self.reasoning {
            Some(r) => r.len(),
            None => 0,
        };
        self.token_count = (self.delta.len() + reasoning_len).div_ceil(4);
        self
    }
}

/// Options for streaming chat requests.
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    /// Whether to enable streaming (default: true).
    pub enabled: bool,
    /// Whether to include token counts in chunks.
    pub count_tokens: bool,
    /// Tool specs to register for native streaming tool calling.
    ///
    /// `None` means do not send tools. `Some(empty)` is normalized to `None`
    /// by [`Self::with_tools`] for the current providers.
    pub tools: Option<Vec<crate::tools::ToolSpec>>,
}

impl StreamOptions {
    /// Create new streaming options with enabled flag.
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            count_tokens: false,
            tools: None,
        }
    }

    /// Enable token counting.
    pub const fn with_token_count(mut self) -> Self {
        self.count_tokens = true;
        self
    }

    /// Attach native tool specs to a streaming request.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<crate::tools::ToolSpec>) -> Self {
        self.tools = if tools.is_empty() { None } else { Some(tools) };
        self
    }
}

/// Result type for streaming operations.
pub type StreamResult<T> = std::result::Result<T, StreamError>;

/// Errors that can occur during streaming.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("HTTP error: {0}")]
    Http(reqwest::Error),

    #[error("JSON parse error: {0}")]
    Json(serde_json::Error),

    #[error("Invalid SSE format: {0}")]
    InvalidSse(String),

    #[error("Provider error: {0}")]
    Provider(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Structured error returned when a requested capability is not supported.
#[derive(Debug, Clone, thiserror::Error)]
#[error("provider_capability_error provider={provider} capability={capability} message={message}")]
pub struct ProviderCapabilityError {
    pub provider: String,
    pub capability: String,
    pub message: String,
}

/// Provider capabilities declaration.
///
/// Describes what features a provider supports, enabling intelligent
/// adaptation of tool calling modes and request formatting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Whether the provider supports native tool calling via API primitives.
    ///
    /// When `true`, the provider can convert tool definitions to API-native
    /// formats (e.g., Gemini's functionDeclarations, Anthropic's input_schema).
    ///
    /// When `false`, tools must be injected via system prompt as text.
    pub native_tool_calling: bool,
    /// Whether the provider supports vision / image inputs.
    pub vision: bool,
}

/// Provider-specific tool payload formats.
///
/// Different LLM providers require different formats for tool definitions.
/// This enum encapsulates those variations, enabling providers to convert
/// from the unified `ToolSpec` format to their native API requirements.
#[derive(Debug, Clone)]
pub enum ToolsPayload {
    /// Gemini API format (functionDeclarations).
    Gemini {
        function_declarations: Vec<serde_json::Value>,
    },
    /// Anthropic Messages API format (tools with input_schema).
    Anthropic { tools: Vec<serde_json::Value> },
    /// OpenAI Chat Completions API format (tools with function).
    OpenAI { tools: Vec<serde_json::Value> },
    /// Prompt-guided fallback (tools injected as text in system prompt).
    PromptGuided { instructions: String },
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Query provider capabilities.
    ///
    /// Default implementation returns minimal capabilities (no native tool calling).
    /// Providers should override this to declare their actual capabilities.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Convert tool specifications to provider-native format.
    ///
    /// Default implementation returns `PromptGuided` payload, which injects
    /// tool documentation into the system prompt as text. Providers with
    /// native tool calling support should override this to return their
    /// specific format (Gemini, Anthropic, OpenAI).
    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        ToolsPayload::PromptGuided {
            instructions: build_tool_instructions_text(tools),
        }
    }

    /// Simple one-shot chat (single user message, no explicit system prompt).
    ///
    /// This is the preferred API for non-agentic direct interactions.
    async fn simple_chat(&self, message: &str, model: &str, temperature: f64) -> anyhow::Result<String> {
        self.chat_with_system(None, message, model, temperature).await
    }

    /// One-shot chat with optional system prompt.
    ///
    /// Kept for compatibility and advanced one-shot prompting.
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String>;

    /// Multi-turn conversation. Default implementation extracts the last user
    /// message and delegates to `chat_with_system`.
    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let system = messages.iter().find(|m| m.role == "system").map(|m| m.content.as_str());
        let last_user = messages
            .iter()
            .rfind(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");
        self.chat_with_system(system, last_user, model, temperature).await
    }

    /// Structured chat API for agent loop callers.
    async fn chat(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatResponse> {
        // If tools are provided but provider doesn't support native tools,
        // inject tool instructions into system prompt as fallback.
        if let Some(tools) = request.tools {
            if !tools.is_empty() && !self.supports_native_tools() {
                let tool_instructions = match self.convert_tools(tools) {
                    ToolsPayload::PromptGuided { instructions } => instructions,
                    payload => {
                        anyhow::bail!(
                            "Provider returned non-prompt-guided tools payload ({payload:?}) while supports_native_tools() is false"
                        )
                    }
                };
                let mut modified_messages = request.messages.to_vec();

                // Inject tool instructions into an existing system message.
                // If none exists, prepend one to the conversation.
                if let Some(system_message) = modified_messages.iter_mut().find(|m| m.role == "system") {
                    if !system_message.content.is_empty() {
                        system_message.content.push_str("\n\n");
                    }
                    system_message.content.push_str(&tool_instructions);
                } else {
                    modified_messages.insert(0, ChatMessage::system(tool_instructions));
                }

                let text = self.chat_with_history(&modified_messages, model, temperature).await?;
                return Ok(ChatResponse {
                    text: Some(text),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                });
            }
        }

        let text = self.chat_with_history(request.messages, model, temperature).await?;
        Ok(ChatResponse {
            text: Some(text),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }

    async fn chat_with_decision(
        &self,
        decision: &RouteDecision,
        request: ChatRequest<'_>,
        temperature: f64,
    ) -> anyhow::Result<(ChatResponse, ProviderExecutionOutcome)> {
        let started_at = chrono::Utc::now();
        match self.chat(request, decision.effective_model(), temperature).await {
            Ok(response) => Ok((
                response,
                ProviderExecutionOutcome::success_for_decision(decision, started_at),
            )),
            Err(error) => Err(error),
        }
    }

    /// Whether provider supports native tool calls over API.
    fn supports_native_tools(&self) -> bool {
        self.capabilities().native_tool_calling
    }

    /// Whether provider supports multimodal vision input.
    fn supports_vision(&self) -> bool {
        self.capabilities().vision
    }

    /// Warm up the HTTP connection pool (TLS handshake, DNS, HTTP/2 setup).
    /// Default implementation is a no-op; providers with HTTP clients should override.
    async fn warmup(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Chat with tool definitions for native function calling support.
    /// The default implementation falls back to chat_with_history and returns
    /// an empty tool_calls vector (prompt-based tool use only).
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        _tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let text = self.chat_with_history(messages, model, temperature).await?;
        Ok(ChatResponse {
            text: Some(text),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }

    /// Whether provider supports streaming responses.
    /// Default implementation returns false.
    fn supports_streaming(&self) -> bool {
        false
    }

    /// Streaming chat with optional system prompt.
    /// Returns an async stream of text chunks.
    /// Default implementation falls back to non-streaming chat.
    fn stream_chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
        _options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        // Default: return an empty stream (not supported)
        stream::empty().boxed()
    }

    /// Streaming chat with history.
    /// Default implementation falls back to stream_chat_with_system with last user message.
    fn stream_chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
        _options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        // For default implementation, we need to convert to owned strings
        // This is a limitation of the default implementation
        let provider_name = "unknown".to_string();

        // Create a single empty chunk to indicate not supported
        let chunk = StreamChunk::error(format!("{} does not support streaming", provider_name));
        stream::once(async move { Ok(chunk) }).boxed()
    }
}

/// Build tool instructions text for prompt-guided tool calling.
///
/// Generates a formatted text block describing available tools and how to
/// invoke them using XML-style tags. This is used as a fallback when the
/// provider doesn't support native tool calling.
pub fn build_tool_instructions_text(tools: &[ToolSpec]) -> String {
    let mut instructions = String::new();

    instructions.push_str("## Tool Use Protocol\n\n");
    instructions.push_str("To use a tool, wrap a JSON object in <tool_call></tool_call> tags:\n\n");
    instructions.push_str("<tool_call>\n");
    instructions.push_str(r#"{"name": "tool_name", "arguments": {"param": "value"}}"#);
    instructions.push_str("\n</tool_call>\n\n");
    instructions.push_str("You may use multiple tool calls in a single response. ");
    instructions.push_str("After tool execution, results appear in <tool_result> tags. ");
    instructions.push_str("Continue reasoning with the results until you can give a final answer.\n\n");
    instructions.push_str("### Available Tools\n\n");

    for tool in tools {
        let _ = writeln!(&mut instructions, "**{}**: {}", tool.name, tool.description);

        let parameters = serde_json::to_string(&tool.parameters).unwrap_or_else(|_| "{}".to_string());
        let _ = writeln!(&mut instructions, "Parameters: `{parameters}`");
        instructions.push('\n');
    }

    instructions
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CapabilityMockProvider;

    #[async_trait]
    impl Provider for CapabilityMockProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: true,
                vision: true,
            }
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("ok".into())
        }
    }

    #[test]
    fn chat_message_constructors() {
        let sys = ChatMessage::system("Be helpful");
        assert_eq!(sys.role, "system");
        assert_eq!(sys.content, "Be helpful");

        let user = ChatMessage::user("Hello");
        assert_eq!(user.role, "user");

        let asst = ChatMessage::assistant("Hi there");
        assert_eq!(asst.role, "assistant");

        let tool = ChatMessage::tool("{}");
        assert_eq!(tool.role, "tool");
    }

    #[test]
    fn chat_response_helpers() {
        let empty = ChatResponse {
            text: None,
            tool_calls: vec![],
            reasoning_content: None,
        };
        assert!(!empty.has_tool_calls());
        assert_eq!(empty.text_or_empty(), "");

        let with_tools = ChatResponse {
            text: Some("Let me check".into()),
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
            reasoning_content: None,
        };
        assert!(with_tools.has_tool_calls());
        assert_eq!(with_tools.text_or_empty(), "Let me check");
    }

    #[test]
    fn tool_call_serialization() {
        let tc = ToolCall {
            id: "call_123".into(),
            name: "file_read".into(),
            arguments: r#"{"path":"test.txt"}"#.into(),
        };
        let json = serde_json::to_string(&tc).unwrap();
        assert!(json.contains("call_123"));
        assert!(json.contains("file_read"));
    }

    #[test]
    fn conversation_message_variants() {
        let chat = ConversationMessage::Chat(ChatMessage::user("hi"));
        let json = serde_json::to_string(&chat).unwrap();
        assert!(json.contains("\"type\":\"Chat\""));

        let tool_result = ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "1".into(),
            content: "done".into(),
        }]);
        let json = serde_json::to_string(&tool_result).unwrap();
        assert!(json.contains("\"type\":\"ToolResults\""));
    }

    #[test]
    fn provider_capabilities_default() {
        let caps = ProviderCapabilities::default();
        assert!(!caps.native_tool_calling);
        assert!(!caps.vision);
    }

    #[test]
    fn provider_capabilities_equality() {
        let caps1 = ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        };
        let caps2 = ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        };
        let caps3 = ProviderCapabilities {
            native_tool_calling: false,
            vision: false,
        };

        assert_eq!(caps1, caps2);
        assert_ne!(caps1, caps3);
    }

    #[test]
    fn supports_native_tools_reflects_capabilities_default_mapping() {
        let provider = CapabilityMockProvider;
        assert!(provider.supports_native_tools());
    }

    #[test]
    fn supports_vision_reflects_capabilities_default_mapping() {
        let provider = CapabilityMockProvider;
        assert!(provider.supports_vision());
    }

    #[test]
    fn tools_payload_variants() {
        // Test Gemini variant
        let gemini = ToolsPayload::Gemini {
            function_declarations: vec![serde_json::json!({"name": "test"})],
        };
        assert!(matches!(gemini, ToolsPayload::Gemini { .. }));

        // Test Anthropic variant
        let anthropic = ToolsPayload::Anthropic {
            tools: vec![serde_json::json!({"name": "test"})],
        };
        assert!(matches!(anthropic, ToolsPayload::Anthropic { .. }));

        // Test OpenAI variant
        let openai = ToolsPayload::OpenAI {
            tools: vec![serde_json::json!({"type": "function"})],
        };
        assert!(matches!(openai, ToolsPayload::OpenAI { .. }));

        // Test PromptGuided variant
        let prompt_guided = ToolsPayload::PromptGuided {
            instructions: "Use tools...".to_string(),
        };
        assert!(matches!(prompt_guided, ToolsPayload::PromptGuided { .. }));
    }

    #[test]
    fn build_tool_instructions_text_format() {
        let tools = vec![
            ToolSpec {
                name: "shell".to_string(),
                description: "Execute commands".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"}
                    }
                }),
            },
            ToolSpec {
                name: "file_read".to_string(),
                description: "Read files".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"}
                    }
                }),
            },
        ];

        let instructions = build_tool_instructions_text(&tools);

        // Check for protocol description
        assert!(instructions.contains("Tool Use Protocol"));
        assert!(instructions.contains("<tool_call>"));
        assert!(instructions.contains("</tool_call>"));

        // Check for tool listings
        assert!(instructions.contains("**shell**"));
        assert!(instructions.contains("Execute commands"));
        assert!(instructions.contains("**file_read**"));
        assert!(instructions.contains("Read files"));

        // Check for parameters
        assert!(instructions.contains("Parameters:"));
        assert!(instructions.contains(r#""type":"object""#));
    }

    #[test]
    fn build_tool_instructions_text_empty() {
        let instructions = build_tool_instructions_text(&[]);

        // Should still have protocol description
        assert!(instructions.contains("Tool Use Protocol"));

        // Should have empty tools section
        assert!(instructions.contains("Available Tools"));
    }

    // Mock provider for testing.
    struct MockProvider {
        supports_native: bool,
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn supports_native_tools(&self) -> bool {
            self.supports_native
        }

        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("response".to_string())
        }
    }

    #[test]
    fn provider_convert_tools_default() {
        let provider = MockProvider { supports_native: false };

        let tools = vec![ToolSpec {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let payload = provider.convert_tools(&tools);

        // Default implementation should return PromptGuided.
        assert!(matches!(payload, ToolsPayload::PromptGuided { .. }));

        if let ToolsPayload::PromptGuided { instructions } = payload {
            assert!(instructions.contains("test_tool"));
            assert!(instructions.contains("A test tool"));
        }
    }

    #[tokio::test]
    async fn provider_chat_prompt_guided_fallback() {
        let provider = MockProvider { supports_native: false };

        let tools = vec![ToolSpec {
            name: "shell".to_string(),
            description: "Run commands".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let request = ChatRequest {
            messages: &[ChatMessage::user("Hello")],
            tools: Some(&tools),
        };

        let response = provider.chat(request, "model", 0.7).await.unwrap();

        // Should return a response (default impl calls chat_with_history).
        assert!(response.text.is_some());
    }

    #[tokio::test]
    async fn provider_chat_without_tools() {
        let provider = MockProvider { supports_native: true };

        let request = ChatRequest {
            messages: &[ChatMessage::user("Hello")],
            tools: None,
        };

        let response = provider.chat(request, "model", 0.7).await.unwrap();

        // Should work normally without tools.
        assert!(response.text.is_some());
    }

    // Provider that echoes the system prompt for assertions.
    struct EchoSystemProvider {
        supports_native: bool,
    }

    #[async_trait]
    impl Provider for EchoSystemProvider {
        fn supports_native_tools(&self) -> bool {
            self.supports_native
        }

        async fn chat_with_system(
            &self,
            system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(system.unwrap_or_default().to_string())
        }
    }

    // Provider with custom prompt-guided conversion.
    struct CustomConvertProvider;

    #[async_trait]
    impl Provider for CustomConvertProvider {
        fn supports_native_tools(&self) -> bool {
            false
        }

        fn convert_tools(&self, _tools: &[ToolSpec]) -> ToolsPayload {
            ToolsPayload::PromptGuided {
                instructions: "CUSTOM_TOOL_INSTRUCTIONS".to_string(),
            }
        }

        async fn chat_with_system(
            &self,
            system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(system.unwrap_or_default().to_string())
        }
    }

    // Provider returning an invalid payload for non-native mode.
    struct InvalidConvertProvider;

    #[async_trait]
    impl Provider for InvalidConvertProvider {
        fn supports_native_tools(&self) -> bool {
            false
        }

        fn convert_tools(&self, _tools: &[ToolSpec]) -> ToolsPayload {
            ToolsPayload::OpenAI {
                tools: vec![serde_json::json!({"type": "function"})],
            }
        }

        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("should_not_reach".to_string())
        }
    }

    #[tokio::test]
    async fn provider_chat_prompt_guided_preserves_existing_system_not_first() {
        let provider = EchoSystemProvider { supports_native: false };

        let tools = vec![ToolSpec {
            name: "shell".to_string(),
            description: "Run commands".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let request = ChatRequest {
            messages: &[ChatMessage::user("Hello"), ChatMessage::system("BASE_SYSTEM_PROMPT")],
            tools: Some(&tools),
        };

        let response = provider.chat(request, "model", 0.7).await.unwrap();
        let text = response.text.unwrap_or_default();

        assert!(text.contains("BASE_SYSTEM_PROMPT"));
        assert!(text.contains("Tool Use Protocol"));
    }

    #[tokio::test]
    async fn provider_chat_prompt_guided_uses_convert_tools_override() {
        let provider = CustomConvertProvider;

        let tools = vec![ToolSpec {
            name: "shell".to_string(),
            description: "Run commands".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let request = ChatRequest {
            messages: &[ChatMessage::system("BASE"), ChatMessage::user("Hello")],
            tools: Some(&tools),
        };

        let response = provider.chat(request, "model", 0.7).await.unwrap();
        let text = response.text.unwrap_or_default();

        assert!(text.contains("BASE"));
        assert!(text.contains("CUSTOM_TOOL_INSTRUCTIONS"));
    }

    #[tokio::test]
    async fn provider_chat_prompt_guided_rejects_non_prompt_payload() {
        let provider = InvalidConvertProvider;

        let tools = vec![ToolSpec {
            name: "shell".to_string(),
            description: "Run commands".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let request = ChatRequest {
            messages: &[ChatMessage::user("Hello")],
            tools: Some(&tools),
        };

        let err = provider.chat(request, "model", 0.7).await.unwrap_err();
        let message = err.to_string();

        assert!(message.contains("non-prompt-guided"));
    }

    // ─── 5a-5 StreamChunk + ToolCallChunk 协议层契约测试 ────────────────────

    #[test]
    fn tool_call_chunk_new_constructs_fields() {
        let c = ToolCallChunk::new("call_42", "shell", r#"{"cmd":"ls"}"#, 0);
        assert_eq!(c.id, "call_42");
        assert_eq!(c.name, "shell");
        assert_eq!(c.args, r#"{"cmd":"ls"}"#);
        assert_eq!(c.index, 0);
    }

    #[test]
    fn tool_call_chunk_serde_roundtrip() {
        let original = ToolCallChunk::new("call_1", "edit", r#"{"path":"a.rs"}"#, 2);
        let json = serde_json::to_string(&original).expect("serialize ToolCallChunk");
        let parsed: ToolCallChunk = serde_json::from_str(&json).expect("deserialize ToolCallChunk");
        assert_eq!(parsed, original);
    }

    #[test]
    fn stream_chunk_delta_has_empty_tool_calls() {
        let c = StreamChunk::delta("hello");
        assert_eq!(c.delta, "hello");
        assert!(c.tool_calls.is_empty());
        assert!(!c.has_tool_calls());
    }

    #[test]
    fn stream_chunk_reasoning_has_empty_tool_calls() {
        let c = StreamChunk::reasoning_delta("thinking...");
        assert!(c.delta.is_empty());
        assert!(c.reasoning.is_some());
        assert!(c.tool_calls.is_empty());
        assert!(c.is_reasoning_only());
        assert!(!c.has_tool_calls());
    }

    #[test]
    fn stream_chunk_tool_call_chunk_constructor() {
        let calls = vec![
            ToolCallChunk::new("call_a", "shell", "{}", 0),
            ToolCallChunk::new("call_b", "edit", "{}", 1),
        ];
        let c = StreamChunk::tool_call_chunk(calls.clone());
        assert!(c.delta.is_empty(), "tool_call_chunk has no visible delta");
        assert!(c.reasoning.is_none());
        assert!(!c.is_final);
        assert_eq!(c.tool_calls, calls);
        assert!(c.has_tool_calls());
    }

    #[test]
    fn stream_chunk_final_and_error_have_no_tool_calls() {
        assert!(StreamChunk::final_chunk().tool_calls.is_empty());
        assert!(StreamChunk::error("boom").tool_calls.is_empty());
    }

    #[test]
    fn stream_chunk_default_is_backwards_compatible() {
        // Default impl 是协议向后兼容的保证：所有 provider 即便不感知 5a-5
        // 也能编译 — 字段被默认补 Vec::new()，has_tool_calls() == false.
        let c = StreamChunk::default();
        assert!(c.delta.is_empty());
        assert!(c.tool_calls.is_empty());
        assert!(!c.has_tool_calls());
    }

    // ─── S3 T3-0 双阶段（Streaming / Completed）协议契约测试 ────────────────

    /// 协议自洽：Streaming chunks 的 `arguments_delta` 累加值必须等于最后
    /// `Completed` chunk 的 `args`；`id` 与 `name` 在序列内贯穿一致。
    #[test]
    fn streaming_to_completed_aggregates_arguments() {
        let chunk1 = ToolCallChunk {
            id: "tool_1".into(),
            name: "search".into(),
            args: String::new(),
            index: 0,
            arguments_delta: Some(r#"{"query":"#.into()),
            status: ToolCallChunkStatus::Streaming,
        };
        let chunk2 = ToolCallChunk {
            id: "tool_1".into(),
            name: "search".into(),
            args: String::new(),
            index: 0,
            arguments_delta: Some(r#""rust"}"#.into()),
            status: ToolCallChunkStatus::Streaming,
        };
        let chunk_final = ToolCallChunk {
            id: "tool_1".into(),
            name: "search".into(),
            args: r#"{"query":"rust"}"#.into(),
            index: 0,
            arguments_delta: None,
            status: ToolCallChunkStatus::Completed,
        };

        // 聚合：driver 端会实现，这里先验证协议自洽。
        let mut aggregated = String::new();
        for c in [&chunk1, &chunk2] {
            if let Some(delta) = c.arguments_delta.as_ref() {
                aggregated.push_str(delta);
            }
        }
        assert_eq!(aggregated, chunk_final.args, "Streaming 累积应等于 Completed 的 args",);
        assert_eq!(chunk1.id, chunk_final.id, "tool id 必须贯穿全 chunk 序列");
        assert_eq!(chunk1.name, chunk_final.name, "tool name 必须贯穿全 chunk 序列");
        assert_eq!(
            chunk1.index, chunk_final.index,
            "tool index 在 Streaming/Completed 间必须一致",
        );
        assert_eq!(chunk1.status, ToolCallChunkStatus::Streaming);
        assert_eq!(chunk_final.status, ToolCallChunkStatus::Completed);
        assert!(
            chunk_final.arguments_delta.is_none(),
            "Completed.arguments_delta MUST be None"
        );
    }

    /// 旧 provider 风格（只发 `Completed`）继续合法 —— `ToolCallChunk::new`
    /// 默认即 `Completed` + `arguments_delta = None`。
    #[test]
    fn legacy_completed_only_still_valid() {
        let chunk = ToolCallChunk::new("tool_1", "search", r#"{"query":"hello"}"#, 0);
        assert_eq!(chunk.status, ToolCallChunkStatus::Completed);
        assert!(chunk.arguments_delta.is_none());
        assert_eq!(chunk.args, r#"{"query":"hello"}"#);

        // 序列化兼容：旧 provider emit 的 chunk 仍可正常 round-trip。
        let json = serde_json::to_string(&chunk).expect("serialize legacy chunk");
        let parsed: ToolCallChunk = serde_json::from_str(&json).expect("deserialize legacy chunk");
        assert_eq!(parsed, chunk);
    }

    /// `ToolCallChunkStatus::default()` 必须是 `Completed`，否则 derive(Default)
    /// 派生的 `ToolCallChunk::default()` 会破坏 5a-5 已有测试的不变量。
    #[test]
    fn default_status_is_completed() {
        let status = ToolCallChunkStatus::default();
        assert_eq!(status, ToolCallChunkStatus::Completed);

        let chunk = ToolCallChunk::default();
        assert_eq!(chunk.status, ToolCallChunkStatus::Completed);
        assert!(chunk.arguments_delta.is_none());
        assert!(chunk.args.is_empty());
    }

    /// 便捷构造器：`streaming_delta` 应产生 `Streaming` chunk，`args` 为空，
    /// `arguments_delta = Some(...)`，保留 `id` / `name` / `index`。
    #[test]
    fn streaming_delta_constructor_shape() {
        let chunk = ToolCallChunk::streaming_delta("call_x", "edit", r#"{"path":"#, 3);
        assert_eq!(chunk.id, "call_x");
        assert_eq!(chunk.name, "edit");
        assert_eq!(chunk.index, 3);
        assert!(chunk.args.is_empty(), "Streaming chunk MUST have empty args");
        assert_eq!(chunk.arguments_delta.as_deref(), Some(r#"{"path":"#));
        assert_eq!(chunk.status, ToolCallChunkStatus::Streaming);
    }
}
