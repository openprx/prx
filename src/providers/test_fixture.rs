//! Test-only provider fixtures reached through the production provider factory.

use super::traits::{
    ChatMessage, ChatRequest, ChatResponse, ChatTrace, Provider, ProviderCapabilities, ProviderRequestMode,
    StreamChunk, StreamOptions, StreamResult, ToolsPayload,
};
use crate::llm::route_decision::{ProviderExecutionOutcome, RouteDecision};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

fn fixtures() -> &'static parking_lot::Mutex<HashMap<String, Weak<dyn Provider>>> {
    static FIXTURES: OnceLock<parking_lot::Mutex<HashMap<String, Weak<dyn Provider>>>> = OnceLock::new();
    FIXTURES.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

pub(crate) fn register(label: &str, provider: Arc<dyn Provider>) {
    fixtures().lock().insert(label.to_owned(), Arc::downgrade(&provider));
}

pub(crate) fn build(label: &str) -> Option<Box<dyn Provider>> {
    let provider = fixtures().lock().get(label).and_then(Weak::upgrade)?;
    Some(Box::new(ForwardProvider(provider)))
}

struct ForwardProvider(Arc<dyn Provider>);

#[async_trait]
impl Provider for ForwardProvider {
    fn accounting_targets(&self, model: &str) -> Option<super::AccountingTargets> {
        self.0.accounting_targets(model)
    }

    fn supports_attempt_accounting(&self) -> bool {
        self.0.supports_attempt_accounting()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.0.capabilities()
    }

    fn capabilities_for(&self, model: &str, mode: ProviderRequestMode) -> ProviderCapabilities {
        self.0.capabilities_for(model, mode)
    }

    fn tool_schema_dialect(&self) -> crate::tools::ToolSchemaDialect {
        self.0.tool_schema_dialect()
    }

    fn tool_schema_dialect_for(&self, model: &str) -> crate::tools::ToolSchemaDialect {
        self.0.tool_schema_dialect_for(model)
    }

    fn tool_schema_dialect_of_member(&self, member: &str, model: &str) -> Option<crate::tools::ToolSchemaDialect> {
        self.0.tool_schema_dialect_of_member(member, model)
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> anyhow::Result<ToolsPayload> {
        self.0.convert_tools(tools)
    }

    async fn simple_chat(&self, message: &str, model: &str, temperature: f64) -> anyhow::Result<String> {
        self.0.simple_chat(message, model, temperature).await
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.0
            .chat_with_system(system_prompt, message, model, temperature)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.0.chat_with_history(messages, model, temperature).await
    }

    async fn chat(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatResponse> {
        self.0.chat(request, model, temperature).await
    }

    async fn chat_traced(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatTrace> {
        self.0.chat_traced(request, model, temperature).await
    }

    async fn chat_with_decision(
        &self,
        decision: &RouteDecision,
        request: ChatRequest<'_>,
        temperature: f64,
    ) -> anyhow::Result<(ChatResponse, ProviderExecutionOutcome)> {
        self.0.chat_with_decision(decision, request, temperature).await
    }

    fn supports_native_tools(&self) -> bool {
        self.0.supports_native_tools()
    }

    fn supports_vision(&self) -> bool {
        self.0.supports_vision()
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        self.0.warmup().await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        self.0.chat_with_tools(messages, tools, model, temperature).await
    }

    fn supports_streaming(&self) -> bool {
        self.0.supports_streaming()
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        self.0
            .stream_chat_with_system(system_prompt, message, model, temperature, options)
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        self.0.stream_chat_with_history(messages, model, temperature, options)
    }
}
