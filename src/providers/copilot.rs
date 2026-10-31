//! GitHub Copilot provider.
//!
//! Login through VS Code's OAuth client and the private Copilot token
//! endpoint is disabled. Saved cache files are left in place and are not sent.

use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, Provider, StreamChunk,
    StreamError, StreamOptions, StreamResult, ToolCall as ProviderToolCall,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

// ── Chat completions types ───────────────────────────────────────

#[derive(Debug, Serialize)]
struct ApiChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    temperature: f64,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiMessage {
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
    kind: &'static str,
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
struct ApiChatResponse {
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
    #[serde(default)]
    tool_calls: Option<Vec<NativeToolCall>>,
}

// ── Provider ─────────────────────────────────────────────────────

/// GitHub Copilot provider with automatic OAuth and token refresh.
///
/// On first use, prompts the user to visit github.com/login/device.
/// Tokens are cached to `~/.config/openprx/copilot/` and refreshed
/// automatically.
pub struct CopilotProvider {
    github_token: Option<String>,
    /// Mutex ensures only one caller refreshes tokens at a time,
    /// preventing duplicate device flow prompts or redundant API calls.
    refresh_lock: Arc<Mutex<Option<()>>>,
    token_dir: PathBuf,
}

impl CopilotProvider {
    pub fn new(github_token: Option<&str>) -> Self {
        let token_dir = crate::config::platform_config_dir()
            .map(|dir| dir.join("copilot"))
            .unwrap_or_else(|| {
                // Fall back to a user-specific temp directory to avoid
                // shared-directory symlink attacks.
                let user = std::env::var("USER")
                    .or_else(|_| std::env::var("USERNAME"))
                    .unwrap_or_else(|_| "unknown".to_string());
                std::env::temp_dir().join(format!("openprx-copilot-{user}"))
            });

        if let Err(err) = std::fs::create_dir_all(&token_dir) {
            warn!(
                "Failed to create Copilot token directory {:?}: {err}. Token caching is disabled.",
                token_dir
            );
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                if let Err(err) = std::fs::set_permissions(&token_dir, std::fs::Permissions::from_mode(0o700)) {
                    warn!(
                        "Failed to set Copilot token directory permissions on {:?}: {err}",
                        token_dir
                    );
                }
            }
        }

        Self {
            github_token: github_token.filter(|token| !token.is_empty()).map(String::from),
            refresh_lock: Arc::new(Mutex::new(None)),
            token_dir,
        }
    }

    fn http_client(&self) -> Client {
        crate::config::build_runtime_proxy_client_with_timeouts("provider.copilot", 120, 10)
            .map_err(|e| {
                tracing::error!("proxy build failed for provider.copilot, using direct: {e}");
                e
            })
            .unwrap_or_else(|_| crate::prx_http_client())
    }

    /// Headers that do not impersonate another editor. User-Agent comes from the client.
    const COPILOT_HEADERS: [(&str, &str); 1] = [("Accept", "application/json")];

    /// Lower every published tool schema to what this endpoint was declared to
    /// accept, or refuse the request naming the tool that cannot be offered.
    fn convert_tools(&self, tools: Option<&[ToolSpec]>) -> anyhow::Result<Option<Vec<NativeToolSpec>>> {
        let Some(items) = tools else {
            return Ok(None);
        };
        let dialect = self.tool_schema_dialect();
        let mut converted = Vec::with_capacity(items.len());
        for tool in items {
            let parameters =
                dialect
                    .wire_schema(&tool.name, &tool.parameters)
                    .map_err(|error| super::ProviderCapabilityError {
                        provider: "copilot".to_string(),
                        capability: "tool_schema_dialect".to_string(),
                        message: error.to_string(),
                    })?;
            converted.push(NativeToolSpec {
                kind: "function",
                function: NativeToolFunctionSpec {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: parameters.into_owned(),
                },
            });
        }
        Ok(Some(converted))
    }

    fn convert_messages(messages: &[ChatMessage]) -> Vec<ApiMessage> {
        messages
            .iter()
            .map(|message| {
                if message.role == "assistant" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.content) {
                        if let Some(tool_calls_value) = value.get("tool_calls") {
                            if let Ok(parsed_calls) =
                                serde_json::from_value::<Vec<ProviderToolCall>>(tool_calls_value.clone())
                            {
                                let tool_calls = parsed_calls
                                    .into_iter()
                                    .map(|tool_call| NativeToolCall {
                                        id: Some(tool_call.id),
                                        kind: Some("function".to_string()),
                                        function: NativeFunctionCall {
                                            name: tool_call.name,
                                            arguments: tool_call.arguments,
                                        },
                                    })
                                    .collect::<Vec<_>>();

                                let content = value
                                    .get("content")
                                    .and_then(serde_json::Value::as_str)
                                    .map(ToString::to_string);

                                return ApiMessage {
                                    role: "assistant".to_string(),
                                    content,
                                    tool_call_id: None,
                                    tool_calls: Some(tool_calls),
                                };
                            }
                        }
                    }
                }

                if message.role == "tool" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.content) {
                        let tool_call_id = value
                            .get("tool_call_id")
                            .and_then(serde_json::Value::as_str)
                            .map(ToString::to_string);
                        let content = value
                            .get("content")
                            .and_then(serde_json::Value::as_str)
                            .map(ToString::to_string);

                        return ApiMessage {
                            role: "tool".to_string(),
                            content,
                            tool_call_id,
                            tool_calls: None,
                        };
                    }
                }

                ApiMessage {
                    role: message.role.clone(),
                    content: Some(message.content.clone()),
                    tool_call_id: None,
                    tool_calls: None,
                }
            })
            .collect()
    }

    /// Send a chat completions request with required Copilot headers.
    async fn send_chat_request(
        &self,
        messages: Vec<ApiMessage>,
        tools: Option<&[ToolSpec]>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let (token, endpoint) = self.get_api_key().await?;
        let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));

        let native_tools = self.convert_tools(tools)?;
        let request = ApiChatRequest {
            model: model.to_string(),
            messages,
            temperature,
            stream: false,
            tool_choice: native_tools.as_ref().map(|_| "auto".to_string()),
            tools: native_tools,
        };

        let mut req = self
            .http_client()
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&request);

        for (header, value) in &Self::COPILOT_HEADERS {
            req = req.header(*header, *value);
        }

        let response = req.send().await?;

        if !response.status().is_success() {
            return Err(super::api_error("GitHub Copilot", response).await);
        }

        let api_response: ApiChatResponse = response.json().await?;
        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No response from GitHub Copilot"))?;

        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tool_call| ProviderToolCall {
                id: tool_call.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: tool_call.function.name,
                arguments: tool_call.function.arguments,
            })
            .collect();

        Ok(ProviderChatResponse {
            text: choice.message.content,
            tool_calls,
            reasoning_content: None,
        })
    }

    /// Copilot login is disabled. Cached tokens are not read or sent.
    async fn get_api_key(&self) -> anyhow::Result<(String, String)> {
        let _ = (&self.github_token, &self.token_dir, &self.refresh_lock);
        anyhow::bail!("{}", crate::auth::terms::COPILOT_LOGIN_DISABLED)
    }
}

#[async_trait]
impl Provider for CopilotProvider {
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
        let mut messages = Vec::new();
        if let Some(system) = system_prompt {
            messages.push(ApiMessage {
                role: "system".to_string(),
                content: Some(system.to_string()),
                tool_call_id: None,
                tool_calls: None,
            });
        }
        messages.push(ApiMessage {
            role: "user".to_string(),
            content: Some(message.to_string()),
            tool_call_id: None,
            tool_calls: None,
        });

        let response = self.send_chat_request(messages, None, model, temperature).await?;
        Ok(response.text.unwrap_or_default())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let response = self
            .send_chat_request(Self::convert_messages(messages), None, model, temperature)
            .await?;
        Ok(response.text.unwrap_or_default())
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        self.send_chat_request(
            Self::convert_messages(request.messages),
            request.tools,
            model,
            temperature,
        )
        .await
    }

    fn supports_native_tools(&self) -> bool {
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
        let messages = Self::convert_messages(messages);
        // Lowered before the connection is opened, so a tool this endpoint
        // cannot be offered ends the turn with the reason, not an upstream 400.
        let native_tools = match self.convert_tools(options.tools.as_deref()) {
            Ok(tools) => tools,
            Err(error) => {
                let message = error.to_string();
                return stream::once(async move { Err(StreamError::Provider(message)) }).boxed();
            }
        };
        let request = ApiChatRequest {
            model: model.to_string(),
            messages,
            temperature,
            stream: true,
            tool_choice: native_tools.as_ref().map(|_| "auto".to_string()),
            tools: native_tools,
        };
        let count_tokens = options.count_tokens;
        let this = self.clone_for_stream();

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);
        crate::providers::compatible::spawn_until_closed(tx.clone(), async move {
            let (token, endpoint) = match this.get_api_key().await {
                Ok(value) => value,
                Err(err) => {
                    let _ = tx.send(Err(StreamError::Provider(err.to_string()))).await;
                    return;
                }
            };
            let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
            let mut req = this
                .http_client()
                .post(&url)
                .header("Authorization", format!("Bearer {token}"))
                .header("Accept", "text/event-stream")
                .json(&request);

            for (header, value) in &Self::COPILOT_HEADERS {
                req = req.header(*header, *value);
            }

            let response = match req.send().await {
                Ok(response) => response,
                Err(err) => {
                    let _ = tx.send(Err(StreamError::Http(err))).await;
                    return;
                }
            };

            if !response.status().is_success() {
                let _ = tx
                    .send(Err(super::stream_api_error("GitHub Copilot", response).await))
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

    async fn warmup(&self) -> anyhow::Result<()> {
        let _ = self.get_api_key().await?;
        Ok(())
    }
}

impl CopilotProvider {
    fn clone_for_stream(&self) -> Self {
        Self {
            github_token: self.github_token.clone(),
            refresh_lock: Arc::clone(&self.refresh_lock),
            token_dir: self.token_dir.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_without_token() {
        let provider = CopilotProvider::new(None);
        assert!(provider.github_token.is_none());
    }

    #[test]
    fn new_with_token() {
        let provider = CopilotProvider::new(Some("ghp_test"));
        assert_eq!(provider.github_token.as_deref(), Some("ghp_test"));
    }

    #[test]
    fn empty_token_treated_as_none() {
        let provider = CopilotProvider::new(Some(""));
        assert!(provider.github_token.is_none());
    }

    #[tokio::test]
    async fn cache_starts_empty() {
        let provider = CopilotProvider::new(None);
        let cached = provider.refresh_lock.lock().await;
        assert!(cached.is_none());
    }

    #[test]
    fn copilot_headers_include_required_fields() {
        let headers = CopilotProvider::COPILOT_HEADERS;
        assert!(headers.iter().any(|(header, _)| *header == "Accept"));
        assert!(!headers.iter().any(|(header, _)| *header == "Editor-Version"));
        assert!(!headers.iter().any(|(header, _)| *header == "User-Agent"));
    }

    #[test]
    fn supports_native_tools() {
        let provider = CopilotProvider::new(None);
        assert!(provider.supports_native_tools());
    }

    #[test]
    fn api_chat_request_serializes_stream_true() {
        let request = ApiChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![ApiMessage {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature: 0.2,
            stream: true,
            tools: None,
            tool_choice: None,
        };

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json.get("stream"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn supports_streaming_returns_true() {
        let provider = CopilotProvider::new(None);
        assert!(provider.supports_streaming());
    }
}
