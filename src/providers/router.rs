use super::Provider;
use super::traits::{ChatMessage, ChatRequest, ChatResponse, StreamChunk, StreamOptions, StreamResult};
use async_trait::async_trait;
use std::collections::HashMap;
#[cfg(any(test, feature = "test-mock"))]
use std::sync::Arc;
#[cfg(any(test, feature = "test-mock"))]
use std::sync::atomic::{AtomicUsize, Ordering};

/// A single route: maps a task hint to a provider + model combo.
#[derive(Debug, Clone)]
pub struct Route {
    pub provider_name: String,
    pub model: String,
}

/// Multi-model router — routes requests to different provider+model combos
/// based on a task hint encoded in the model parameter.
///
/// The model parameter can be:
/// - A regular model name (e.g. "anthropic/claude-sonnet-4") → uses default provider
/// - A hint-prefixed string (e.g. "hint:reasoning") → resolves via route table
///
/// This wraps multiple pre-created providers and selects the right one per request.
pub struct RouterProvider {
    routes: HashMap<String, (usize, String)>, // hint → (provider_index, model)
    providers: Vec<(String, Box<dyn Provider>)>,
    default_index: usize,
    _default_model: String,
}

impl RouterProvider {
    /// Create a new router with a default provider and optional routes.
    ///
    /// `providers` is a list of (name, provider) pairs. The first one is the default.
    /// `routes` maps hint names to Route structs containing provider_name and model.
    pub fn new(
        providers: Vec<(String, Box<dyn Provider>)>,
        routes: Vec<(String, Route)>,
        default_model: String,
    ) -> Self {
        // Build provider name → index lookup
        let name_to_index: HashMap<&str, usize> = providers
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (name.as_str(), i))
            .collect();

        // Resolve routes to provider indices
        let resolved_routes: HashMap<String, (usize, String)> = routes
            .into_iter()
            .filter_map(|(hint, route)| {
                let index = name_to_index.get(route.provider_name.as_str()).copied();
                match index {
                    Some(i) => Some((hint, (i, route.model))),
                    None => {
                        tracing::warn!(
                            hint = hint,
                            provider = route.provider_name,
                            "Route references unknown provider, skipping"
                        );
                        None
                    }
                }
            })
            .collect();

        Self {
            routes: resolved_routes,
            providers,
            default_index: 0,
            _default_model: default_model,
        }
    }

    /// Resolve a model parameter to a (provider, actual_model) pair.
    ///
    /// If the model starts with "hint:", look up the hint in the route table.
    /// Otherwise, use the default provider with the given model name.
    /// Resolve a model parameter to a (provider_index, actual_model) pair.
    fn resolve(&self, model: &str) -> (usize, String) {
        if let Some(hint) = model.strip_prefix("hint:") {
            if let Some((idx, resolved_model)) = self.routes.get(hint) {
                return (*idx, resolved_model.clone());
            }
            tracing::warn!(hint = hint, "Unknown route hint, falling back to default provider");
        }

        // Not a hint or hint not found — use default provider with the model as-is
        (self.default_index, model.to_string())
    }
}

#[async_trait]
impl Provider for RouterProvider {
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let (provider_idx, resolved_model) = self.resolve(model);

        let (provider_name, provider) = &self.providers[provider_idx];
        tracing::info!(
            provider = provider_name.as_str(),
            model = resolved_model.as_str(),
            "Router dispatching request"
        );

        provider
            .chat_with_system(system_prompt, message, &resolved_model, temperature)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let (provider_idx, resolved_model) = self.resolve(model);
        let (_, provider) = &self.providers[provider_idx];
        provider.chat_with_history(messages, &resolved_model, temperature).await
    }

    async fn chat(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatResponse> {
        let (provider_idx, resolved_model) = self.resolve(model);
        let (_, provider) = &self.providers[provider_idx];
        provider.chat(request, &resolved_model, temperature).await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let (provider_idx, resolved_model) = self.resolve(model);
        let (_, provider) = &self.providers[provider_idx];
        provider
            .chat_with_tools(messages, tools, &resolved_model, temperature)
            .await
    }

    fn supports_native_tools(&self) -> bool {
        self.providers
            .iter()
            .any(|(_, provider)| provider.supports_native_tools())
    }

    fn supports_vision(&self) -> bool {
        self.providers.iter().any(|(_, provider)| provider.supports_vision())
    }

    fn supports_streaming(&self) -> bool {
        self.providers.iter().any(|(_, provider)| provider.supports_streaming())
    }

    /// 把 `stream_chat_with_history` 转发到被路由的具体 provider，
    /// 与 `chat_with_history` 的行为对齐。否则默认 trait 实现会回退到
    /// "unknown does not support streaming" 错误 chunk，
    /// 让 Step 5a-4 dispatcher driver 路径（依赖 streaming）无法工作。
    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> futures::stream::BoxStream<'static, StreamResult<StreamChunk>> {
        use futures::StreamExt as _;
        let (provider_idx, resolved_model) = self.resolve(model);
        let Some((_, provider)) = self.providers.get(provider_idx) else {
            // resolve() 永远返回有效 idx；防御性兜底返回错误 chunk 而非 panic.
            return futures::stream::once(async move {
                Err(super::traits::StreamError::Provider(format!(
                    "RouterProvider: resolved provider index {provider_idx} out of bounds"
                )))
            })
            .boxed();
        };
        provider.stream_chat_with_history(messages, &resolved_model, temperature, options)
    }

    /// Keep system-prompt streaming behavior symmetric with history streaming by
    /// converting the system prompt and user message into chat history, then
    /// routing through `stream_chat_with_history`.
    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> futures::stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let mut messages = Vec::with_capacity(usize::from(system_prompt.is_some()) + 1);
        if let Some(system) = system_prompt {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: system.to_string(),
            });
        }
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: message.to_string(),
        });
        self.stream_chat_with_history(&messages, model, temperature, options)
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        for (name, provider) in &self.providers {
            tracing::info!(provider = name, "Warming up routed provider");
            if let Err(e) = provider.warmup().await {
                tracing::warn!(provider = name, "Warmup failed (non-fatal): {e}");
            }
        }
        Ok(())
    }
}

/// Deterministic in-process provider for tests and the PTY E2E harness.
///
/// Enabled by the `test-mock` Cargo feature (and unconditionally in `#[cfg(test)]`).
/// Reads the response text from the `OPENPRX_MOCK_RESPONSE` environment variable
/// at construction time so PTY tests can pin a unique sentinel per scenario,
/// e.g. `OPENPRX_MOCK_RESPONSE=[MOCK-END-A1B2]`. Falls back to `[MOCK-DEFAULT]`
/// when the env var is unset so the binary still runs deterministically without
/// any extra setup.
///
/// Intentionally bypasses prompt-guided tool injection by reporting
/// `supports_native_tools() = true` — the chat tool loop then treats `Provider::chat`
/// as native, and our impl just returns the canned text with no tool calls.
#[cfg(any(test, feature = "test-mock"))]
pub(crate) struct MockEnvProvider {
    response: String,
    /// 5a-6: 当设置 `OPENPRX_MOCK_TOOL_CALL=name:args_json` 时，第一次 streaming
    /// 调用产生 `ToolCallChunk`(name, args)，后续调用返回 `response` 文本。
    /// 让 PTY E2E 能验证 driver 完整 tool turn 闭环（call → execute → continue → final）。
    /// `None` 时维持原 5a-2 行为（直接返回 response 文本）.
    tool_call_spec: Option<MockToolCallSpec>,
    /// 流式调用计数器，决定本次 emit tool_call 还是 final text。
    call_counter: Arc<AtomicUsize>,
    /// S5 P0-1: 完整流式脚本（JSON 序列化 chunks 列表）。设置后 stream 按
    /// 脚本逐 chunk emit，绕过 response / tool_call_spec 路径。
    script: Option<MockScript>,
    /// S5 P0-1: provider flavor hint — 仅用于日志识别，不改变 chunk 内容
    /// (anthropic / openai / gemini)，方便 PTY 测试覆盖不同协议路径的回归。
    flavor: Option<String>,
    /// S5 P0-1: 每个 chunk 间的延迟（ms），cancel-mid-stream 测试窗口用。
    delay_ms_per_chunk: u64,
}

#[cfg(any(test, feature = "test-mock"))]
#[derive(Clone, Debug)]
struct MockToolCallSpec {
    name: String,
    args: String,
}

/// S5 P0-1: 完整脚本，描述一次 stream 中按顺序 emit 的 chunks.
///
/// JSON 序列化形态：
/// ```json
/// {"chunks":[
///   {"delta":"Hello "},
///   {"reasoning":"thinking..."},
///   {"tool":{"id":"t1","name":"shell","args":"{\"cmd\":\"ls\"}"}},
///   {"delta":"done"},
///   {"is_final":true}
/// ]}
/// ```
#[cfg(any(test, feature = "test-mock"))]
#[derive(Clone, Debug, serde::Deserialize)]
struct MockScript {
    chunks: Vec<MockChunk>,
}

#[cfg(any(test, feature = "test-mock"))]
#[derive(Clone, Debug, serde::Deserialize)]
struct MockChunk {
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool: Option<MockScriptTool>,
    #[serde(default)]
    is_final: bool,
}

#[cfg(any(test, feature = "test-mock"))]
#[derive(Clone, Debug, serde::Deserialize)]
struct MockScriptTool {
    id: String,
    name: String,
    args: String,
}

/// S5 P0-1: 把单个 `MockChunk` 映射到 `StreamChunk`（按优先级：tool > final > reasoning > delta）.
#[cfg(any(test, feature = "test-mock"))]
fn script_chunk_to_stream(mc: &MockChunk, idx: usize) -> StreamChunk {
    use crate::providers::traits::ToolCallChunk;
    if let Some(tool) = mc.tool.as_ref() {
        return StreamChunk::tool_call_chunk(vec![ToolCallChunk::new(
            tool.id.clone(),
            tool.name.clone(),
            tool.args.clone(),
            idx,
        )]);
    }
    if mc.is_final {
        return StreamChunk::final_chunk();
    }
    if let Some(r) = mc.reasoning.as_ref() {
        return StreamChunk::reasoning_delta(r.clone());
    }
    StreamChunk::delta(mc.delta.clone().unwrap_or_default())
}

#[cfg(any(test, feature = "test-mock"))]
impl MockEnvProvider {
    /// Read response sentinel from `OPENPRX_MOCK_RESPONSE` (default
    /// `[MOCK-DEFAULT-RESPONSE][MOCK-END]`).
    ///
    /// Empty-string is treated as "unset" and falls back to the default
    /// sentinel — an empty mock response would never satisfy any PTY
    /// `expect` matcher and would silently turn into a hang.
    ///
    /// 5a-6: 读取 `OPENPRX_MOCK_TOOL_CALL` 控制 streaming 是否在首次返回 tool_call.
    /// 格式: `name:args_json` (e.g. `shell:{"cmd":"ls"}`). 解析失败回退为无 tool_call.
    pub(crate) fn from_env() -> Self {
        const DEFAULT_SENTINEL: &str = "[MOCK-DEFAULT-RESPONSE][MOCK-END]";
        let response = match std::env::var("OPENPRX_MOCK_RESPONSE") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => DEFAULT_SENTINEL.to_string(),
        };
        let tool_call_spec = std::env::var("OPENPRX_MOCK_TOOL_CALL").ok().and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }
            let (name, args) = trimmed.split_once(':')?;
            let name = name.trim();
            let args = args.trim();
            if name.is_empty() || args.is_empty() {
                return None;
            }
            Some(MockToolCallSpec {
                name: name.to_string(),
                args: args.to_string(),
            })
        });
        // S5 P0-1: 完整流式脚本，优先级最高（绕过 response / tool_call_spec）
        let script = std::env::var("OPENPRX_MOCK_SCRIPT").ok().and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }
            match serde_json::from_str::<MockScript>(trimmed) {
                Ok(s) if !s.chunks.is_empty() => Some(s),
                Ok(_) => {
                    tracing::warn!("OPENPRX_MOCK_SCRIPT 解析成功但 chunks 为空，忽略");
                    None
                }
                Err(e) => {
                    tracing::warn!(error = %e, "OPENPRX_MOCK_SCRIPT 解析失败，回退默认路径");
                    None
                }
            }
        });
        let flavor = std::env::var("OPENPRX_MOCK_PROVIDER_FLAVOR")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| matches!(v.as_str(), "anthropic" | "openai" | "gemini"));
        let delay_ms_per_chunk = std::env::var("OPENPRX_MOCK_DELAY_MS_PER_CHUNK")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        Self {
            response,
            tool_call_spec,
            call_counter: Arc::new(AtomicUsize::new(0)),
            script,
            flavor,
            delay_ms_per_chunk,
        }
    }
}

#[cfg(any(test, feature = "test-mock"))]
#[async_trait]
impl Provider for MockEnvProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(self.response.clone())
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(self.response.clone())
    }

    // Native-tools = true so the agent loop calls Provider::chat directly and
    // doesn't inject the prompt-guided tool instructions (which would otherwise
    // pollute the output and make sentinel matching brittle).
    fn supports_native_tools(&self) -> bool {
        true
    }

    async fn chat(&self, _request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        Ok(ChatResponse {
            text: Some(self.response.clone()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }

    async fn chat_with_tools(
        &self,
        _messages: &[ChatMessage],
        _tools: &[serde_json::Value],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        Ok(ChatResponse {
            text: Some(self.response.clone()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// 显式实现 streaming：把 `response` 作为单个 delta 推送 + final 标记。
    ///
    /// 仅在 `test-mock` 启用下编译。PRX_CHAT_REDUX_DRIVER 路径的 PTY 验证依赖这里
    /// 真返回 chunk（默认 trait 实现返回错误 chunk "unknown does not support streaming"）。
    ///
    /// 5a-6: 当 `OPENPRX_MOCK_TOOL_CALL=name:args` 设置且 `call_counter == 0` 时，首次
    /// 调用返回单个 `ToolCallChunk` (无 delta 文本) — driver 收到后执行 tool，把
    /// tool_result 喂回 history，再次调 stream_chat_with_history（此时 counter == 1），
    /// 返回 `response` 文本作为最终答复，turn 闭环。
    fn stream_chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
        _options: StreamOptions,
    ) -> futures::stream::BoxStream<'static, StreamResult<StreamChunk>> {
        use crate::providers::traits::ToolCallChunk;
        use futures::StreamExt as _;

        let counter_val = self.call_counter.fetch_add(1, Ordering::SeqCst);
        let response = self.response.clone();

        // S5 P0-1: SCRIPT 路径优先 — 按脚本顺序 emit chunks，支持 reasoning / tool / delta /
        // is_final 混搭。flavor 仅记日志（实际协议适配由 driver 与各 provider impl 负责）。
        //
        // 关键：第一次调用按脚本 emit，**第二次及以后**只输出 final（避免 tool_call 无限重放
        // 导致 driver max_tool_iterations 触发）。脚本含 tool_call 时尤其重要：driver
        // 执行 tool → 喂回 tool_result → 再次调 stream_chat_with_history，本次不该再发 tool_call.
        if let Some(script) = self.script.as_ref() {
            if let Some(flavor) = &self.flavor {
                tracing::debug!(flavor = %flavor, call = counter_val, "mock SCRIPT stream start");
            }
            let chunks: Vec<StreamResult<StreamChunk>> = if counter_val == 0 {
                script
                    .chunks
                    .iter()
                    .enumerate()
                    .map(|(idx, mc)| Ok(script_chunk_to_stream(mc, idx)))
                    .collect()
            } else {
                // 续轮：emit 脚本里的所有 delta（拼回 sentinel）+ final，不重放 tool.
                let mut out: Vec<StreamResult<StreamChunk>> = script
                    .chunks
                    .iter()
                    .filter_map(|mc| mc.delta.as_ref().map(|d| Ok(StreamChunk::delta(d.clone()))))
                    .collect();
                out.push(Ok(StreamChunk::final_chunk()));
                out
            };
            let delay = self.delay_ms_per_chunk;
            return futures::stream::iter(chunks)
                .then(move |c| async move {
                    if delay > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    }
                    c
                })
                .boxed();
        }

        // First call + tool_call_spec configured → emit ToolCallChunk.
        if counter_val == 0
            && let Some(spec) = self.tool_call_spec.as_ref()
        {
            let calls = vec![ToolCallChunk::new(
                format!("mock-call-{counter_val}"),
                spec.name.clone(),
                spec.args.clone(),
                0,
            )];
            return futures::stream::iter(vec![
                Ok(StreamChunk::tool_call_chunk(calls)),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed();
        }

        // Default path / subsequent call → text response.
        futures::stream::iter(vec![
            Ok(StreamChunk {
                delta: response,
                reasoning: None,
                is_final: false,
                token_count: 0,
                tool_calls: Vec::new(),
            }),
            Ok(StreamChunk::final_chunk()),
        ])
        .boxed()
    }

    fn supports_streaming(&self) -> bool {
        true
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    struct MockProvider {
        calls: Arc<AtomicUsize>,
        response: &'static str,
        last_model: parking_lot::Mutex<String>,
    }

    impl MockProvider {
        fn new(response: &'static str) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                response,
                last_model: parking_lot::Mutex::new(String::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn last_model(&self) -> String {
            self.last_model.lock().clone()
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_model.lock() = model.to_string();
            Ok(self.response.to_string())
        }
    }

    struct NativeCapabilityMock {
        native_tools: bool,
    }

    #[async_trait]
    impl Provider for NativeCapabilityMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("ok".to_string())
        }

        fn supports_native_tools(&self) -> bool {
            self.native_tools
        }
    }

    struct StreamingCaptureProvider {
        seen_messages: Arc<parking_lot::Mutex<Vec<ChatMessage>>>,
        seen_model: Arc<parking_lot::Mutex<String>>,
    }

    #[async_trait]
    impl Provider for StreamingCaptureProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("ok".to_string())
        }

        fn stream_chat_with_history(
            &self,
            messages: &[ChatMessage],
            model: &str,
            _temperature: f64,
            _options: StreamOptions,
        ) -> futures::stream::BoxStream<'static, StreamResult<StreamChunk>> {
            use futures::StreamExt as _;
            *self.seen_messages.lock() = messages.to_vec();
            *self.seen_model.lock() = model.to_string();
            futures::stream::iter(vec![Ok(StreamChunk::delta("ok")), Ok(StreamChunk::final_chunk())]).boxed()
        }

        fn supports_streaming(&self) -> bool {
            true
        }
    }

    fn make_router(
        providers: Vec<(&'static str, &'static str)>,
        routes: Vec<(&str, &str, &str)>,
    ) -> (RouterProvider, Vec<Arc<MockProvider>>) {
        let mocks: Vec<Arc<MockProvider>> = providers
            .iter()
            .map(|(_, response)| Arc::new(MockProvider::new(response)))
            .collect();

        let provider_list: Vec<(String, Box<dyn Provider>)> = providers
            .iter()
            .zip(mocks.iter())
            .map(|((name, _), mock)| (name.to_string(), Box::new(Arc::clone(mock)) as Box<dyn Provider>))
            .collect();

        let route_list: Vec<(String, Route)> = routes
            .iter()
            .map(|(hint, provider_name, model)| {
                (
                    hint.to_string(),
                    Route {
                        provider_name: provider_name.to_string(),
                        model: model.to_string(),
                    },
                )
            })
            .collect();

        let router = RouterProvider::new(provider_list, route_list, "default-model".to_string());

        (router, mocks)
    }

    // Arc<MockProvider> should also be a Provider
    #[async_trait]
    impl Provider for Arc<MockProvider> {
        async fn chat_with_system(
            &self,
            system_prompt: Option<&str>,
            message: &str,
            model: &str,
            temperature: f64,
        ) -> anyhow::Result<String> {
            self.as_ref()
                .chat_with_system(system_prompt, message, model, temperature)
                .await
        }
    }

    #[tokio::test]
    async fn routes_hint_to_correct_provider() {
        let (router, mocks) = make_router(
            vec![("fast", "fast-response"), ("smart", "smart-response")],
            vec![("fast", "fast", "llama-3-70b"), ("reasoning", "smart", "claude-opus")],
        );

        let result = router.simple_chat("hello", "hint:reasoning", 0.5).await.unwrap();
        assert_eq!(result, "smart-response");
        assert_eq!(mocks[1].call_count(), 1);
        assert_eq!(mocks[1].last_model(), "claude-opus");
        assert_eq!(mocks[0].call_count(), 0);
    }

    #[tokio::test]
    async fn routes_fast_hint() {
        let (router, mocks) = make_router(
            vec![("fast", "fast-response"), ("smart", "smart-response")],
            vec![("fast", "fast", "llama-3-70b")],
        );

        let result = router.simple_chat("hello", "hint:fast", 0.5).await.unwrap();
        assert_eq!(result, "fast-response");
        assert_eq!(mocks[0].call_count(), 1);
        assert_eq!(mocks[0].last_model(), "llama-3-70b");
    }

    #[tokio::test]
    async fn unknown_hint_falls_back_to_default() {
        let (router, mocks) = make_router(
            vec![("default", "default-response"), ("other", "other-response")],
            vec![],
        );

        let result = router.simple_chat("hello", "hint:nonexistent", 0.5).await.unwrap();
        assert_eq!(result, "default-response");
        assert_eq!(mocks[0].call_count(), 1);
        // Falls back to default with the hint as model name
        assert_eq!(mocks[0].last_model(), "hint:nonexistent");
    }

    #[tokio::test]
    async fn non_hint_model_uses_default_provider() {
        let (router, mocks) = make_router(
            vec![("primary", "primary-response"), ("secondary", "secondary-response")],
            vec![("code", "secondary", "codellama")],
        );

        let result = router
            .simple_chat("hello", "anthropic/claude-sonnet-4-20250514", 0.5)
            .await
            .unwrap();
        assert_eq!(result, "primary-response");
        assert_eq!(mocks[0].call_count(), 1);
        assert_eq!(mocks[0].last_model(), "anthropic/claude-sonnet-4-20250514");
    }

    #[test]
    fn resolve_preserves_model_for_non_hints() {
        let (router, _) = make_router(vec![("default", "ok")], vec![]);

        let (idx, model) = router.resolve("gpt-4o");
        assert_eq!(idx, 0);
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn resolve_strips_hint_prefix() {
        let (router, _) = make_router(
            vec![("fast", "ok"), ("smart", "ok")],
            vec![("reasoning", "smart", "claude-opus")],
        );

        let (idx, model) = router.resolve("hint:reasoning");
        assert_eq!(idx, 1);
        assert_eq!(model, "claude-opus");
    }

    #[test]
    fn skips_routes_with_unknown_provider() {
        let (router, _) = make_router(vec![("default", "ok")], vec![("broken", "nonexistent", "model")]);

        // Route should not exist
        assert!(!router.routes.contains_key("broken"));
    }

    #[tokio::test]
    async fn warmup_calls_all_providers() {
        let (router, _) = make_router(vec![("a", "ok"), ("b", "ok")], vec![]);

        // Warmup should not error
        assert!(router.warmup().await.is_ok());
    }

    #[tokio::test]
    async fn chat_with_system_passes_system_prompt() {
        let mock = Arc::new(MockProvider::new("response"));
        let router = RouterProvider::new(
            vec![("default".into(), Box::new(Arc::clone(&mock)) as Box<dyn Provider>)],
            vec![],
            "model".into(),
        );

        let result = router
            .chat_with_system(Some("system"), "hello", "model", 0.5)
            .await
            .unwrap();
        assert_eq!(result, "response");
        assert_eq!(mock.call_count(), 1);
    }

    #[tokio::test]
    async fn chat_with_tools_delegates_to_resolved_provider() {
        let mock = Arc::new(MockProvider::new("tool-response"));
        let router = RouterProvider::new(
            vec![("default".into(), Box::new(Arc::clone(&mock)) as Box<dyn Provider>)],
            vec![],
            "model".into(),
        );

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "use tools".to_string(),
        }];
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run shell command",
                "parameters": {}
            }
        })];

        // chat_with_tools should delegate through the router to the mock.
        // MockProvider's default chat_with_tools calls chat_with_history -> chat_with_system.
        let result = router.chat_with_tools(&messages, &tools, "model", 0.7).await.unwrap();
        assert_eq!(result.text.as_deref(), Some("tool-response"));
        assert_eq!(mock.call_count(), 1);
        assert_eq!(mock.last_model(), "model");
    }

    #[tokio::test]
    async fn chat_with_tools_routes_hint_correctly() {
        let (router, mocks) = make_router(
            vec![("fast", "fast-tool"), ("smart", "smart-tool")],
            vec![("reasoning", "smart", "claude-opus")],
        );

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "reason about this".to_string(),
        }];
        let tools = vec![serde_json::json!({"type": "function", "function": {"name": "test"}})];

        let result = router
            .chat_with_tools(&messages, &tools, "hint:reasoning", 0.5)
            .await
            .unwrap();
        assert_eq!(result.text.as_deref(), Some("smart-tool"));
        assert_eq!(mocks[1].call_count(), 1);
        assert_eq!(mocks[1].last_model(), "claude-opus");
        assert_eq!(mocks[0].call_count(), 0);
    }

    #[tokio::test]
    async fn stream_chat_with_system_routes_through_history() {
        use futures::StreamExt as _;

        let seen_messages = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen_model = Arc::new(parking_lot::Mutex::new(String::new()));
        let provider = StreamingCaptureProvider {
            seen_messages: Arc::clone(&seen_messages),
            seen_model: Arc::clone(&seen_model),
        };
        let router = RouterProvider::new(
            vec![("default".into(), Box::new(provider) as Box<dyn Provider>)],
            vec![(
                "stream".into(),
                Route {
                    provider_name: "default".into(),
                    model: "routed-model".into(),
                },
            )],
            "default-model".into(),
        );

        let chunks: Vec<_> = router
            .stream_chat_with_system(
                Some("system prompt"),
                "hello",
                "hint:stream",
                0.5,
                StreamOptions::new(false),
            )
            .collect()
            .await;

        assert_eq!(chunks.len(), 2);
        assert_eq!(*seen_model.lock(), "routed-model");
        let messages = seen_messages.lock();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, "system prompt");
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, "hello");
    }

    #[test]
    fn supports_native_tools_is_true_if_any_routed_provider_supports_it() {
        let router = RouterProvider::new(
            vec![
                (
                    "default".into(),
                    Box::new(NativeCapabilityMock { native_tools: false }) as Box<dyn Provider>,
                ),
                (
                    "alternate".into(),
                    Box::new(NativeCapabilityMock { native_tools: true }) as Box<dyn Provider>,
                ),
            ],
            vec![],
            "model".into(),
        );

        assert!(router.supports_native_tools());
    }
}
