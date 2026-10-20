//! Generation-owned settings around a resolved provider chain.
use super::Provider;
use super::traits::{
    ChatMessage, ChatRequest, ChatResponse, ChatTrace, ProviderCapabilities, ProviderRequestMode, StreamChunk,
    StreamOptions, StreamResult, ToolsPayload,
};
use crate::config::model_settings::ModelSettings;
use crate::llm::route_decision::{ProviderExecutionOutcome, RouteDecision};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Default)]
pub(crate) struct Settings {
    models: Option<Arc<BTreeMap<String, ModelSettings>>>,
    route: Option<(String, ModelSettings)>,
    default_temperature: Option<f64>,
}

tokio::task_local! { static CURRENT: Settings; static TEMPERATURE: Option<f64>; }

pub(crate) fn default_temperature() -> Option<f64> {
    CURRENT.try_with(|settings| settings.default_temperature).ok().flatten()
}

pub(crate) fn caller_temperature() -> Option<f64> {
    TEMPERATURE.try_with(|value| *value).ok().flatten()
}

// Return the scope directly: an extra async wrapper would store the large
// turn future again in its state machine and grow every caller's poll frame.
pub(crate) fn with_temperature<F: std::future::Future>(
    temperature: Option<f64>,
    work: F,
) -> impl std::future::Future<Output = F::Output> {
    TEMPERATURE.scope(temperature, work)
}

pub(crate) fn selected(id: &str) -> Vec<ModelSettings> {
    CURRENT
        .try_with(|settings| {
            let mut layers = Vec::new();
            if let Some(model) = settings.models.as_ref().and_then(|models| models.get(id)) {
                layers.push(model.clone());
            }
            if let Some((model, route)) = &settings.route
                && model == id
            {
                layers.push(route.clone());
            }
            layers
        })
        .unwrap_or_default()
}

pub(crate) fn inherited() -> (Settings, Option<f64>) {
    (CURRENT.try_with(Clone::clone).unwrap_or_default(), caller_temperature())
}

pub(crate) async fn scope<F: std::future::Future>(settings: (Settings, Option<f64>), work: F) -> F::Output {
    TEMPERATURE.scope(settings.1, CURRENT.scope(settings.0, work)).await
}

pub(crate) fn wrap(
    inner: Box<dyn Provider>,
    models: Option<Arc<BTreeMap<String, ModelSettings>>>,
    route: Option<(String, ModelSettings)>,
    default_temperature: Option<f64>,
) -> Box<dyn Provider> {
    if models.as_ref().is_none_or(|models| models.is_empty()) && route.is_none() {
        inner
    } else {
        Box::new(SettingsProvider {
            inner,
            settings: Settings {
                models,
                route,
                default_temperature,
            },
        })
    }
}

struct SettingsProvider {
    inner: Box<dyn Provider>,
    settings: Settings,
}

impl SettingsProvider {
    fn stream(
        &self,
        mut stream: stream::BoxStream<'static, StreamResult<StreamChunk>>,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let settings = self.settings.clone();
        let temperature = caller_temperature();
        stream::poll_fn(move |cx| {
            TEMPERATURE.sync_scope(temperature, || {
                CURRENT.sync_scope(settings.clone(), || stream.as_mut().poll_next(cx))
            })
        })
        .boxed()
    }
}

#[async_trait]
impl Provider for SettingsProvider {
    fn accounting_targets(&self, model: &str) -> Option<super::AccountingTargets> {
        self.inner.accounting_targets(model)
    }

    fn supports_attempt_accounting(&self) -> bool {
        self.inner.supports_attempt_accounting()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    fn capabilities_for(&self, model: &str, mode: ProviderRequestMode) -> ProviderCapabilities {
        self.inner.capabilities_for(model, mode)
    }

    fn tool_schema_dialect(&self) -> crate::tools::ToolSchemaDialect {
        self.inner.tool_schema_dialect()
    }

    fn tool_schema_dialect_for(&self, model: &str) -> crate::tools::ToolSchemaDialect {
        self.inner.tool_schema_dialect_for(model)
    }

    fn tool_schema_dialect_of_member(&self, member: &str, model: &str) -> Option<crate::tools::ToolSchemaDialect> {
        self.inner.tool_schema_dialect_of_member(member, model)
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> anyhow::Result<ToolsPayload> {
        self.inner.convert_tools(tools)
    }

    async fn simple_chat(&self, message: &str, model: &str, temperature: f64) -> anyhow::Result<String> {
        CURRENT
            .scope(
                self.settings.clone(),
                self.inner.simple_chat(message, model, temperature),
            )
            .await
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        CURRENT
            .scope(
                self.settings.clone(),
                self.inner.chat_with_system(system_prompt, message, model, temperature),
            )
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        CURRENT
            .scope(
                self.settings.clone(),
                self.inner.chat_with_history(messages, model, temperature),
            )
            .await
    }

    async fn chat(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatResponse> {
        CURRENT
            .scope(self.settings.clone(), self.inner.chat(request, model, temperature))
            .await
    }

    async fn chat_traced(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatTrace> {
        CURRENT
            .scope(
                self.settings.clone(),
                self.inner.chat_traced(request, model, temperature),
            )
            .await
    }

    async fn chat_with_decision(
        &self,
        decision: &RouteDecision,
        request: ChatRequest<'_>,
        temperature: f64,
    ) -> anyhow::Result<(ChatResponse, ProviderExecutionOutcome)> {
        CURRENT
            .scope(
                self.settings.clone(),
                self.inner.chat_with_decision(decision, request, temperature),
            )
            .await
    }

    fn supports_native_tools(&self) -> bool {
        self.inner.supports_native_tools()
    }

    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        CURRENT.scope(self.settings.clone(), self.inner.warmup()).await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        CURRENT
            .scope(
                self.settings.clone(),
                self.inner.chat_with_tools(messages, tools, model, temperature),
            )
            .await
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        self.stream(CURRENT.sync_scope(self.settings.clone(), || {
            self.inner
                .stream_chat_with_system(system_prompt, message, model, temperature, options)
        }))
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        self.stream(CURRENT.sync_scope(self.settings.clone(), || {
            self.inner
                .stream_chat_with_history(messages, model, temperature, options)
        }))
    }
}

#[cfg(test)]
mod tests;
