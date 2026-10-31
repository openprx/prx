//! xAI provider.
//!
//! An API key keeps the OpenAI-compatible client constructed by the factory.
//! A Grok subscription (SuperGrok / X Premium) is this type: it refreshes the
//! OAuth access token on use and will only attach that bearer to
//! `https://api.x.ai`.

use crate::auth::AuthService;
use crate::auth::xai_oauth::{self, XAI_INFERENCE_BASE};
use crate::providers::ProviderRuntimeOptions;
use crate::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
use crate::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, ChatTrace, Provider, StreamChunk, StreamError, StreamOptions, StreamResult,
};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};

pub struct XaiSubscriptionProvider {
    auth: AuthService,
    auth_profile_override: Option<String>,
    parameter_provider: String,
}

impl XaiSubscriptionProvider {
    pub(crate) fn from_runtime(options: &ProviderRuntimeOptions, parameter_provider: &str) -> Self {
        let state_dir = options
            .openprx_dir
            .clone()
            .unwrap_or_else(super::info::default_openprx_state_dir);
        Self {
            auth: AuthService::new(&state_dir, options.secrets_encrypt),
            auth_profile_override: options.auth_profile_override.clone(),
            parameter_provider: parameter_provider.to_string(),
        }
    }

    async fn delegate(&self) -> anyhow::Result<OpenAiCompatibleProvider> {
        let access_token = self
            .auth
            .get_valid_xai_access_token(self.auth_profile_override.as_deref())
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "xAI Grok subscription is not signed in. Run `prx auth login --provider xai` or `prx auth login --provider xai --device-code`."
                )
            })?;
        let base = xai_oauth::subscription_base_url(Some(XAI_INFERENCE_BASE))?;
        Ok(
            OpenAiCompatibleProvider::new("xAI", base, Some(&access_token), AuthStyle::Bearer)
                .with_parameter_provider(&self.parameter_provider),
        )
    }
}

#[async_trait]
impl Provider for XaiSubscriptionProvider {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.delegate()
            .await?
            .chat_with_system(system_prompt, message, model, temperature)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.delegate()
            .await?
            .chat_with_history(messages, model, temperature)
            .await
    }

    async fn chat(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatResponse> {
        self.delegate().await?.chat(request, model, temperature).await
    }

    async fn chat_traced(&self, request: ChatRequest<'_>, model: &str, temperature: f64) -> anyhow::Result<ChatTrace> {
        self.delegate().await?.chat_traced(request, model, temperature).await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        self.delegate()
            .await?
            .chat_with_tools(messages, tools, model, temperature)
            .await
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let system_prompt = system_prompt.map(str::to_string);
        let message = message.to_string();
        let model = model.to_string();
        let auth = self.auth.clone();
        let profile = self.auth_profile_override.clone();
        let parameter_provider = self.parameter_provider.clone();
        stream::once(async move {
            let provider = match delegate_with(auth, profile, &parameter_provider).await {
                Ok(provider) => provider,
                Err(error) => {
                    return stream::once(async move { Err(StreamError::Provider(error.to_string())) }).boxed();
                }
            };
            provider.stream_chat_with_system(system_prompt.as_deref(), &message, &model, temperature, options)
        })
        .flatten()
        .boxed()
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let messages = messages.to_vec();
        let model = model.to_string();
        let auth = self.auth.clone();
        let profile = self.auth_profile_override.clone();
        let parameter_provider = self.parameter_provider.clone();
        stream::once(async move {
            let provider = match delegate_with(auth, profile, &parameter_provider).await {
                Ok(provider) => provider,
                Err(error) => {
                    return stream::once(async move { Err(StreamError::Provider(error.to_string())) }).boxed();
                }
            };
            provider.stream_chat_with_history(&messages, &model, temperature, options)
        })
        .flatten()
        .boxed()
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        self.delegate().await?.warmup().await
    }
}

async fn delegate_with(
    auth: AuthService,
    auth_profile_override: Option<String>,
    parameter_provider: &str,
) -> anyhow::Result<OpenAiCompatibleProvider> {
    let provider = XaiSubscriptionProvider {
        auth,
        auth_profile_override,
        parameter_provider: parameter_provider.to_string(),
    };
    provider.delegate().await
}
