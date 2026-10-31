use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage};
use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, ChatTrace, Provider,
    StreamChunk, StreamError, StreamOptions, StreamResult, ToolCall as ProviderToolCall,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde::{Deserialize, Serialize};

pub struct OpenRouterProvider {
    credential: Option<String>,
    #[cfg(test)]
    chat_endpoint: Option<String>,
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
struct ApiChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: String,
}

#[derive(Debug, Serialize)]
struct NativeChatRequest {
    model: String,
    messages: Vec<NativeMessage>,
    temperature: f64,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

#[derive(Debug, Serialize)]
struct UsageRequest {
    include: bool,
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

#[derive(Debug, Serialize)]
struct NativeToolSpec {
    #[serde(rename = "type")]
    kind: String,
    function: NativeToolFunctionSpec,
}

#[derive(Debug, Serialize)]
struct NativeToolFunctionSpec {
    name: String,
    description: String,
    parameters: serde_json::Value,
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
    usage: Option<OpenRouterUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
    prompt_tokens_details: Option<PromptTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokenDetails {
    cached_tokens: Option<u32>,
    cache_write_tokens: Option<u32>,
}

impl OpenRouterUsage {
    fn into_tokens(self) -> TokenUsage {
        TokenUsage::reported_with_cache(
            self.prompt_tokens,
            self.completion_tokens,
            self.total_tokens.or_else(|| {
                self.prompt_tokens
                    .zip(self.completion_tokens)
                    .map(|(input, output)| input.saturating_add(output))
            }),
            self.prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cache_write_tokens),
            self.prompt_tokens_details.and_then(|details| details.cached_tokens),
        )
    }
}

#[derive(Debug, Deserialize)]
struct NativeChoice {
    message: NativeResponseMessage,
}

#[derive(Debug, Deserialize)]
struct NativeResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<NativeToolCall>>,
}

impl OpenRouterProvider {
    pub fn new(credential: Option<&str>) -> Self {
        Self {
            credential: credential.map(ToString::to_string),
            #[cfg(test)]
            chat_endpoint: None,
        }
    }

    /// Lower every published tool schema to what this endpoint was declared to
    /// accept, or refuse the request naming the tool that cannot be offered.
    fn convert_tools(&self, tools: Option<&[ToolSpec]>, model: &str) -> anyhow::Result<Option<Vec<NativeToolSpec>>> {
        let items = tools.filter(|items| !items.is_empty());
        let Some(items) = items else {
            return Ok(None);
        };
        let dialect = self.tool_schema_dialect_for(model);
        let mut converted = Vec::with_capacity(items.len());
        for tool in items {
            let parameters =
                dialect
                    .wire_schema(&tool.name, &tool.parameters)
                    .map_err(|error| super::ProviderCapabilityError {
                        provider: "openrouter".to_string(),
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
            text: message.content,
            tool_calls,
            reasoning_content: None,
        }
    }

    const fn chat_endpoint(&self) -> &str {
        #[cfg(test)]
        if let Some(endpoint) = &self.chat_endpoint {
            return endpoint.as_str();
        }
        "https://openrouter.ai/api/v1/chat/completions"
    }

    async fn chat_metered(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<(ProviderChatResponse, TokenUsage)> {
        let credential = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!("OpenRouter API key not set. Set `api_key` in config.toml or run `prx auth paste-token --provider openrouter`.")
        })?;

        let tools = self.convert_tools(request.tools, model)?;
        let native_request = NativeChatRequest {
            model: model.to_string(),
            messages: Self::convert_messages(request.messages),
            temperature,
            stream: false,
            usage: None,
            tool_choice: tools.as_ref().map(|_| "auto".to_string()),
            tools,
        };

        let response = self
            .http_client()
            .post(self.chat_endpoint())
            .header("Authorization", format!("Bearer {credential}"))
            .header("HTTP-Referer", "https://github.com/openprx/prx")
            .header("X-Title", "OpenPRX")
            .json(&super::parameter_wire::request(
                "openrouter",
                super::parameter_wire::Format::OpenRouter,
                &native_request.model,
                &native_request,
            ))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(super::api_error("OpenRouter", response).await);
        }

        let native_response: NativeChatResponse = response.json().await?;
        let message = native_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| anyhow::anyhow!("No response from OpenRouter"))?;
        let tokens = native_response
            .usage
            .map(OpenRouterUsage::into_tokens)
            .unwrap_or_default();
        Ok((Self::parse_native_response(message), tokens))
    }

    fn http_client(&self) -> Client {
        crate::config::build_runtime_proxy_client_with_timeouts("provider.openrouter", 120, 10)
            .map_err(|e| {
                tracing::error!("proxy build failed for provider.openrouter, using direct: {e}");
                e
            })
            .unwrap_or_else(|_| crate::prx_http_client())
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    fn tool_schema_dialect_for(&self, model: &str) -> crate::tools::ToolSchemaDialect {
        super::catalog::builtin_tool_schema_dialect(&super::ProviderId::of("openrouter"), model)
    }

    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        // Hit a lightweight endpoint to establish TLS + HTTP/2 connection pool.
        // This prevents the first real chat request from timing out on cold start.
        if let Some(credential) = self.credential.as_ref() {
            self.http_client()
                .get("https://openrouter.ai/api/v1/auth/key")
                .header("Authorization", format!("Bearer {credential}"))
                .send()
                .await?
                .error_for_status()?;
        }
        Ok(())
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let credential = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!("OpenRouter API key not set. Set `api_key` in config.toml or run `prx auth paste-token --provider openrouter`.")
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
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header("Authorization", format!("Bearer {credential}"))
            .header("HTTP-Referer", "https://github.com/openprx/prx")
            .header("X-Title", "OpenPRX")
            .json(&super::parameter_wire::request(
                "openrouter",
                super::parameter_wire::Format::OpenRouter,
                &request.model,
                &request,
            ))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(super::api_error("OpenRouter", response).await);
        }

        let chat_response: ApiChatResponse = response.json().await?;

        chat_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow::anyhow!("No response from OpenRouter"))
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let credential = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!("OpenRouter API key not set. Set `api_key` in config.toml or run `prx auth paste-token --provider openrouter`.")
        })?;

        let api_messages: Vec<Message> = messages
            .iter()
            .map(|m| Message {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        let request = ChatRequest {
            model: model.to_string(),
            messages: api_messages,
            temperature,
        };

        let response = self
            .http_client()
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header("Authorization", format!("Bearer {credential}"))
            .header("HTTP-Referer", "https://github.com/openprx/prx")
            .header("X-Title", "OpenPRX")
            .json(&super::parameter_wire::request(
                "openrouter",
                super::parameter_wire::Format::OpenRouter,
                &request.model,
                &request,
            ))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(super::api_error("OpenRouter", response).await);
        }

        let chat_response: ApiChatResponse = response.json().await?;

        chat_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow::anyhow!("No response from OpenRouter"))
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
        Ok(ChatTrace {
            response,
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: "openrouter".to_string(),
                model: model.to_string(),
                started_at,
                finished_at: chrono::Utc::now(),
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "openrouter".to_string(),
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
        let specs: Vec<ToolSpec> = tools
            .iter()
            .filter_map(|tool| {
                let function = tool.get("function")?;
                Some(ToolSpec {
                    name: function.get("name")?.as_str()?.to_owned(),
                    description: function
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    parameters: function
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                })
            })
            .collect();
        self.chat(
            ProviderChatRequest {
                messages,
                tools: Some(&specs),
            },
            model,
            temperature,
        )
        .await
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
        let credential = match self.credential.as_ref() {
            Some(value) => value.clone(),
            None => {
                return stream::once(async {
                    Err(StreamError::Provider(
                        "OpenRouter API key not set. Set `api_key` in config.toml or run `prx auth paste-token --provider openrouter`.".to_string(),
                    ))
                })
                .boxed();
            }
        };

        // Lowered before the connection is opened, so a tool this endpoint
        // cannot be offered ends the turn with the reason, not an upstream 400.
        let tools = match self.convert_tools(options.tools.as_deref(), model) {
            Ok(tools) => tools,
            Err(error) => {
                let message = error.to_string();
                return stream::once(async move { Err(StreamError::Provider(message)) }).boxed();
            }
        };
        let request = NativeChatRequest {
            model: model.to_string(),
            messages: Self::convert_messages(messages),
            temperature,
            stream: true,
            usage: Some(UsageRequest { include: true }),
            tool_choice: options.tools.as_ref().map(|_| "auto".to_string()),
            tools,
        };
        let client = self.http_client();
        let count_tokens = options.count_tokens;
        let endpoint = self.chat_endpoint().to_owned();

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);
        crate::providers::compatible::spawn_until_closed(tx.clone(), async move {
            let response = match client
                .post(endpoint)
                .header("Authorization", format!("Bearer {credential}"))
                .header("HTTP-Referer", "https://github.com/openprx/prx")
                .header("X-Title", "OpenPRX")
                .header("Accept", "text/event-stream")
                .json(&super::parameter_wire::request(
                    "openrouter",
                    super::parameter_wire::Format::OpenRouter,
                    &request.model,
                    &request,
                ))
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    let _ = tx.send(Err(StreamError::Http(err))).await;
                    return;
                }
            };

            if !response.status().is_success() {
                let _ = tx
                    .send(Err(super::stream_api_error("OpenRouter", response).await))
                    .await;
                return;
            }

            crate::providers::compatible::forward_stream_until_closed(
                crate::providers::compatible::sse_bytes_to_chunks(response, count_tokens),
                &tx,
            )
            .await;
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

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::{ChatMessage, Provider};

    #[test]
    fn usage_preserves_cache_counts_and_missing_fields() {
        let response: NativeChatResponse = serde_json::from_value(serde_json::json!({
            "choices": [],
            "usage": {"prompt_tokens":100,"completion_tokens":20,
                "prompt_tokens_details":{"cached_tokens":30,"cache_write_tokens":10}}
        }))
        .unwrap();
        let usage = response.usage.unwrap().into_tokens();
        assert_eq!(
            usage,
            TokenUsage::reported_with_cache(Some(100), Some(20), Some(120), Some(10), Some(30))
        );
        for value in [serde_json::json!({}), serde_json::json!({"total_tokens":12})] {
            let usage: OpenRouterUsage = serde_json::from_value(value).unwrap();
            let tokens = usage.into_tokens();
            assert_eq!(tokens.prompt_tokens, None);
            assert_eq!(tokens.completion_tokens, None);
            assert_eq!(tokens.cache_read_input_tokens, None);
            assert_eq!(tokens.cache_creation_input_tokens, None);
        }
        let response: NativeChatResponse = serde_json::from_str(r#"{"choices":[]}"#).unwrap();
        assert!(response.usage.is_none());
    }

    #[tokio::test]
    async fn anthropic_schema_rejection_precedes_streaming_and_chat_transport() {
        let probe = crate::providers::credential_probe::CredentialProbe::start().await;
        let mut provider = OpenRouterProvider::new(Some("prx-test-credential"));
        provider.chat_endpoint = Some(format!("{}/chat/completions", probe.base_url()));
        let tools = vec![ToolSpec {
            name: "mcp_fixture".into(),
            description: "fixture".into(),
            parameters: serde_json::json!({"type":"object", "anyOf":[{"required":["a"]},{"required":["b"]}]}),
        }];
        let messages = vec![ChatMessage::user("fixture")];
        let error = provider
            .chat(
                ProviderChatRequest {
                    messages: &messages,
                    tools: Some(&tools),
                },
                "anthropic/fixture",
                0.0,
            )
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<super::super::ProviderCapabilityError>().is_some());
        for expected in ["mcp_fixture", "anyOf", "no_root_combinators", "tool_tiering"] {
            assert!(error.to_string().contains(expected), "{error}");
        }
        let result = provider
            .stream_chat_with_history(
                &messages,
                "anthropic/fixture",
                0.0,
                StreamOptions {
                    tools: Some(tools.clone()),
                    ..StreamOptions::default()
                },
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(result.len(), 1);
        assert!(
            result[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("no_root_combinators")
        );
        let raw = vec![
            serde_json::json!({"type":"function", "function":{"name":"mcp_fixture", "parameters":tools[0].parameters}}),
        ];
        let error = provider
            .chat_with_tools(&messages, &raw, "anthropic/fixture", 0.0)
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<super::super::ProviderCapabilityError>().is_some());
        assert!(probe.requests().is_empty());
        assert!(provider.convert_tools(Some(&tools), "openai/fixture").is_ok());
        let flat = vec![ToolSpec {
            name: "flat_fixture".into(),
            description: "fixture".into(),
            parameters: serde_json::json!({"type":"object","properties":{"action":{"type":"string","enum":["list"]}},"required":["action"]}),
        }];
        let response = provider
            .chat(
                ProviderChatRequest {
                    messages: &messages,
                    tools: Some(&flat),
                },
                "anthropic/fixture",
                0.0,
            )
            .await
            .unwrap();
        assert_eq!(
            response.text.as_deref(),
            Some(crate::providers::credential_probe::PROBE_REPLY)
        );
        assert_eq!(probe.requests().len(), 1);
    }

    #[test]
    fn catalog_and_transport_agree_on_anthropic_family_dialects() {
        use crate::tools::ToolSchemaDialect::{NoRootCombinators, Raw};
        let provider = OpenRouterProvider::new(None);
        for (model, expected) in [
            ("anthropic/fixture", NoRootCombinators),
            ("ANTHROPIC/fixture", NoRootCombinators),
            ("openai/fixture", Raw),
            ("anthropic-fixture", Raw),
        ] {
            let descriptor = super::super::catalog::CatalogView::default()
                .describe(&super::super::ProviderId::of("openrouter"), model);
            assert_eq!(descriptor.dialects.tool_schema, expected);
            assert_eq!(provider.tool_schema_dialect_for(model), expected);
        }
    }

    #[tokio::test]
    async fn priced_openrouter_turn_writes_cost_ledger() -> anyhow::Result<()> {
        use crate::agent::agent::Agent;
        use crate::agent::dispatcher::XmlToolDispatcher;
        use crate::providers::credential_probe::{CredentialProbe, PROBE_REPLY};
        use std::sync::Arc;
        let workspace = tempfile::tempdir()?;
        let probe = CredentialProbe::start().await;
        let mut provider = OpenRouterProvider::new(Some("prx-test-credential"));
        provider.chat_endpoint = Some(format!("{}/chat/completions", probe.base_url()));
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.config_path = workspace.path().join("config.toml");
        config.default_provider = Some("openrouter".into());
        config.default_model = Some("anthropic/claude-sonnet-5".into());
        config.api_key = Some("prx-test-credential".into());
        config.auth.codex_auth_json_auto_import = false;
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
        let mut agent = Agent::builder()
            .runtime_config(config, "openrouter")
            .provider(Box::new(provider))
            .tools(Vec::new())
            .memory(memory)
            .observer(Arc::new(crate::observability::NoopObserver {}))
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .build()?;
        assert_eq!(agent.turn("What is 2 + 2?").await?, PROBE_REPLY);
        assert_eq!(probe.requests().len(), 1);
        let ledger = std::fs::read_to_string(workspace.path().join("state/costs.jsonl"))?;
        let rows: Vec<crate::cost::types::CostRecord> =
            ledger.lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
        assert_eq!(rows.len(), 1);
        let row = rows.first().unwrap();
        assert_eq!(row.settlement_provider.as_deref(), Some("openrouter"));
        assert_eq!(row.usage.model, "openrouter/anthropic/claude-sonnet-5");
        assert_eq!(row.usage.input_tokens, 1);
        assert_eq!(row.usage.output_tokens, 1);
        assert!(row.usage.cost_usd > 0.0);
        assert!(row.settlement_id.is_some());
        Ok(())
    }

    #[test]
    fn creates_with_key() {
        let provider = OpenRouterProvider::new(Some("openrouter-test-credential"));
        assert_eq!(provider.credential.as_deref(), Some("openrouter-test-credential"));
    }

    #[test]
    fn creates_without_key() {
        let provider = OpenRouterProvider::new(None);
        assert!(provider.credential.is_none());
    }

    #[tokio::test]
    async fn warmup_without_key_is_noop() {
        let provider = OpenRouterProvider::new(None);
        let result = provider.warmup().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn chat_with_system_fails_without_key() {
        let provider = OpenRouterProvider::new(None);
        let result = provider
            .chat_with_system(Some("system"), "hello", "openai/gpt-4o", 0.2)
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("API key not set"));
    }

    #[tokio::test]
    async fn chat_with_history_fails_without_key() {
        let provider = OpenRouterProvider::new(None);
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: "be concise".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            },
        ];

        let result = provider
            .chat_with_history(&messages, "anthropic/claude-sonnet-4", 0.7)
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("API key not set"));
    }

    #[test]
    fn chat_request_serializes_with_system_and_user() {
        let request = ChatRequest {
            model: "anthropic/claude-sonnet-4".into(),
            messages: vec![
                Message {
                    role: "system".into(),
                    content: "You are helpful".into(),
                },
                Message {
                    role: "user".into(),
                    content: "Summarize this".into(),
                },
            ],
            temperature: 0.5,
        };

        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("anthropic/claude-sonnet-4"));
        assert!(json.contains("\"role\":\"system\""));
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"temperature\":0.5"));
    }

    #[test]
    fn chat_request_serializes_history_messages() {
        let messages = [
            ChatMessage {
                role: "assistant".into(),
                content: "Previous answer".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "Follow-up".into(),
            },
        ];

        let request = ChatRequest {
            model: "google/gemini-2.5-pro".into(),
            messages: messages
                .iter()
                .map(|msg| Message {
                    role: msg.role.clone(),
                    content: msg.content.clone(),
                })
                .collect(),
            temperature: 0.0,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"role\":\"assistant\""));
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("google/gemini-2.5-pro"));
    }

    #[test]
    fn response_deserializes_single_choice() {
        let json = r#"{"choices":[{"message":{"content":"Hi from OpenRouter"}}]}"#;

        let response: ApiChatResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.choices.len(), 1);
        assert_eq!(response.choices[0].message.content, "Hi from OpenRouter");
    }

    #[test]
    fn response_deserializes_empty_choices() {
        let json = r#"{"choices":[]}"#;

        let response: ApiChatResponse = serde_json::from_str(json).unwrap();

        assert!(response.choices.is_empty());
    }

    #[tokio::test]
    async fn chat_with_tools_fails_without_key() {
        let provider = OpenRouterProvider::new(None);
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "What is the date?".into(),
        }];
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a shell command",
                "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}
            }
        })];

        let result = provider
            .chat_with_tools(&messages, &tools, "deepseek/deepseek-chat", 0.5)
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("API key not set"));
    }

    #[test]
    fn native_response_deserializes_with_tool_calls() {
        let json = r#"{
            "choices":[{
                "message":{
                    "content":null,
                    "tool_calls":[
                        {"id":"call_123","type":"function","function":{"name":"get_price","arguments":"{\"symbol\":\"BTC\"}"}}
                    ]
                }
            }]
        }"#;

        let response: NativeChatResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.choices.len(), 1);
        let message = &response.choices[0].message;
        assert!(message.content.is_none());
        let tool_calls = message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id.as_deref(), Some("call_123"));
        assert_eq!(tool_calls[0].function.name, "get_price");
        assert_eq!(tool_calls[0].function.arguments, "{\"symbol\":\"BTC\"}");
    }

    #[test]
    fn native_response_deserializes_with_text_and_tool_calls() {
        let json = r#"{
            "choices":[{
                "message":{
                    "content":"I'll get that for you.",
                    "tool_calls":[
                        {"id":"call_456","type":"function","function":{"name":"shell","arguments":"{\"command\":\"date\"}"}}
                    ]
                }
            }]
        }"#;

        let response: NativeChatResponse = serde_json::from_str(json).unwrap();

        assert_eq!(response.choices.len(), 1);
        let message = &response.choices[0].message;
        assert_eq!(message.content.as_deref(), Some("I'll get that for you."));
        let tool_calls = message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "shell");
    }

    #[test]
    fn parse_native_response_converts_to_chat_response() {
        let message = NativeResponseMessage {
            content: Some("Here you go.".into()),
            tool_calls: Some(vec![NativeToolCall {
                id: Some("call_789".into()),
                kind: Some("function".into()),
                function: NativeFunctionCall {
                    name: "file_read".into(),
                    arguments: r#"{"path":"test.txt"}"#.into(),
                },
            }]),
        };

        let response = OpenRouterProvider::parse_native_response(message);

        assert_eq!(response.text.as_deref(), Some("Here you go."));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_789");
        assert_eq!(response.tool_calls[0].name, "file_read");
    }

    #[test]
    fn convert_messages_parses_assistant_tool_call_payload() {
        let messages = vec![ChatMessage {
            role: "assistant".into(),
            content: r#"{"content":"Using tool","tool_calls":[{"id":"call_abc","name":"shell","arguments":"{\"command\":\"pwd\"}"}]}"#
                .into(),
        }];

        let converted = OpenRouterProvider::convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[0].content.as_deref(), Some("Using tool"));

        let tool_calls = converted[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id.as_deref(), Some("call_abc"));
        assert_eq!(tool_calls[0].function.name, "shell");
        assert_eq!(tool_calls[0].function.arguments, r#"{"command":"pwd"}"#);
    }

    #[test]
    fn convert_messages_parses_tool_result_payload() {
        let messages = vec![ChatMessage {
            role: "tool".into(),
            content: r#"{"tool_call_id":"call_xyz","content":"done"}"#.into(),
        }];

        let converted = OpenRouterProvider::convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "tool");
        assert_eq!(converted[0].tool_call_id.as_deref(), Some("call_xyz"));
        assert_eq!(converted[0].content.as_deref(), Some("done"));
        assert!(converted[0].tool_calls.is_none());
    }

    #[test]
    fn native_stream_request_serializes_stream_true() {
        let request = NativeChatRequest {
            model: "openai/gpt-4o".to_string(),
            messages: vec![NativeMessage {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature: 0.2,
            stream: true,
            usage: Some(UsageRequest { include: true }),
            tools: None,
            tool_choice: None,
        };

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json.get("stream"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn supports_streaming_returns_true() {
        let provider = OpenRouterProvider::new(Some("openrouter-test-credential"));
        assert!(provider.supports_streaming());
    }
}

#[cfg(test)]
#[path = "openrouter/metering_tests.rs"]
mod metering_tests;
