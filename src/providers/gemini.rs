//! Google Gemini provider with support for:
//! - Direct API key (from config or auth-profiles.json)
//! - Gemini CLI OAuth tokens (reuse existing ~/.gemini/ authentication)

use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, ProviderUsageAccumulator, TokenUsage};
use crate::multimodal;
use crate::providers::traits::{
    ChatMessage, ChatResponse, ChatTrace, Provider, StreamChunk, StreamError, StreamOptions, StreamResult, ToolCall,
    ToolCallChunk, ToolCallChunkStatus, ToolsPayload,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use directories::UserDirs;
use futures::stream::{self, BoxStream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Gemini provider supporting multiple authentication methods.
pub struct GeminiProvider {
    auth: Option<GeminiAuth>,
}

/// Resolved credential — the variant determines both the HTTP auth method
/// and the diagnostic label returned by `auth_source()`.
#[derive(Debug)]
enum GeminiAuth {
    /// Explicit API key from config: sent as `?key=` query parameter.
    ExplicitKey(String),
    /// OAuth access token from Gemini CLI: sent as `Authorization: Bearer`.
    OAuthToken(String),
}

impl GeminiAuth {
    /// Whether this credential is an API key (sent as `?key=` query param).
    const fn is_api_key(&self) -> bool {
        matches!(self, Self::ExplicitKey(_))
    }

    /// Whether this credential is an OAuth token from Gemini CLI.
    const fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuthToken(_))
    }

    /// The raw credential string.
    fn credential(&self) -> &str {
        match self {
            Self::ExplicitKey(s) | Self::OAuthToken(s) => s,
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// API REQUEST/RESPONSE TYPES
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Serialize, Clone)]
struct GenerateContentRequest {
    contents: Vec<Content>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiToolGroup>>,
}

/// Request envelope for the internal cloudcode-pa API.
/// OAuth tokens from Gemini CLI are scoped for this endpoint.
#[derive(Debug, Serialize)]
struct InternalGenerateContentEnvelope {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_prompt_id: Option<String>,
    request: InternalGenerateContentRequest,
}

/// Nested request payload for cloudcode-pa's code assist APIs.
#[derive(Debug, Serialize)]
struct InternalGenerateContentRequest {
    contents: Vec<Content>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiToolGroup>>,
}

#[derive(Debug, Serialize, Clone)]
struct Content {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<RequestPart>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(untagged)]
enum RequestPart {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    InlineData {
        inline_data: InlineData,
    },
    #[serde(rename_all = "camelCase")]
    FunctionCall {
        function_call: RequestFunctionCall,
        #[serde(skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    FunctionResponse {
        function_response: FunctionResponse,
    },
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct InlineData {
    mime_type: String,
    data: String,
}

#[derive(Debug, Serialize, Clone)]
struct RequestFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Debug, Serialize, Clone)]
struct FunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Debug, Serialize, Clone)]
struct GeminiToolGroup {
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Serialize, Clone)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize, Clone)]
struct GenerationConfig {
    temperature: f64,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct GenerateContentResponse {
    candidates: Option<Vec<Candidate>>,
    error: Option<ApiError>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<serde_json::Value>,
    #[serde(default)]
    response: Option<Box<Self>>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    content: CandidateContent,
}

#[derive(Debug, Deserialize)]
struct CandidateContent {
    parts: Vec<ResponsePart>,
}

#[derive(Debug, Deserialize)]
struct ResponsePart {
    text: Option<String>,
    #[serde(rename = "thoughtSignature", default)]
    thought_signature: Option<String>,
    /// **S3 T3-2-C**: Gemini API surfaces tool invocations as
    /// `functionCall { name, args }` parts. `args` is the parsed JSON object
    /// (Gemini emits the full object at once; it does not stream argument
    /// fragments today). See [`FunctionCall`] for the wire shape.
    #[serde(rename = "functionCall", default)]
    function_call: Option<FunctionCall>,
}

/// Gemini API function call payload (used in both non-streaming responses and
/// `streamGenerateContent` chunks).
///
/// `args` is the structured JSON arguments object — Gemini emits it complete
/// in a single part rather than streaming fragments, so the streaming path
/// surfaces this as a `Completed` [`ToolCallChunk`].
#[derive(Debug, Deserialize)]
struct FunctionCall {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

impl GenerateContentResponse {
    /// cloudcode-pa wraps the actual response under `response`.
    fn into_effective_response(self) -> Self {
        match self {
            Self {
                response: Some(inner), ..
            } => *inner,
            other => other,
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// GEMINI CLI TOKEN STRUCTURES
// ══════════════════════════════════════════════════════════════════════════════

/// OAuth token stored by Gemini CLI in `~/.gemini/oauth_creds.json`
#[derive(Debug, Deserialize)]
struct GeminiCliOAuthCreds {
    access_token: Option<String>,
    expiry: Option<String>,
}

/// Internal API endpoint used by Gemini CLI for OAuth users.
/// See: https://github.com/google-gemini/gemini-cli/issues/19200
const CLOUDCODE_PA_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com/v1internal";

/// Public API endpoint for API key users.
const PUBLIC_API_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";

impl GeminiProvider {
    fn tool_call_id(thought_signature: Option<&str>) -> String {
        thought_signature.map_or_else(
            || uuid::Uuid::new_v4().to_string(),
            |signature| format!("gemini:{signature}:{}", uuid::Uuid::new_v4()),
        )
    }

    fn thought_signature_from_tool_call_id(id: &str) -> Option<String> {
        id.strip_prefix("gemini:")
            .and_then(|encoded| encoded.rsplit_once(':'))
            .map(|(signature, _)| signature.to_string())
            .filter(|signature| !signature.is_empty())
    }

    /// Create a new Gemini provider.
    ///
    /// Authentication priority:
    /// 1. Explicit API key passed in (from config or auth-profiles.json)
    /// 2. `GEMINI_API_KEY` env var (standard Google env)
    /// 3. `GOOGLE_API_KEY` env var (standard Google env)
    /// 4. Gemini CLI OAuth tokens (`~/.gemini/oauth_creds.json`)
    pub fn new(api_key: Option<&str>) -> Self {
        let resolved_auth = api_key
            .and_then(Self::normalize_non_empty)
            .map(GeminiAuth::ExplicitKey)
            .or_else(Self::try_env_var_key)
            .or_else(|| Self::try_load_gemini_cli_token().map(GeminiAuth::OAuthToken));

        Self { auth: resolved_auth }
    }

    /// Try standard Google API env vars: `GEMINI_API_KEY`, then `GOOGLE_API_KEY`.
    fn try_env_var_key() -> Option<GeminiAuth> {
        std::env::var("GEMINI_API_KEY")
            .ok()
            .and_then(|v| Self::normalize_non_empty(&v))
            .or_else(|| {
                std::env::var("GOOGLE_API_KEY")
                    .ok()
                    .and_then(|v| Self::normalize_non_empty(&v))
            })
            .map(GeminiAuth::ExplicitKey)
    }

    fn normalize_non_empty(value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Try to load OAuth access token from Gemini CLI's cached credentials.
    /// Location: `~/.gemini/oauth_creds.json`
    fn try_load_gemini_cli_token() -> Option<String> {
        let gemini_dir = Self::gemini_cli_dir()?;
        let creds_path = gemini_dir.join("oauth_creds.json");

        if !creds_path.exists() {
            return None;
        }

        let content = std::fs::read_to_string(&creds_path).ok()?;
        let creds: GeminiCliOAuthCreds = serde_json::from_str(&content).ok()?;

        // Check if token is expired (basic check)
        if let Some(ref expiry) = creds.expiry {
            if let Ok(expiry_time) = chrono::DateTime::parse_from_rfc3339(expiry) {
                if expiry_time < chrono::Utc::now() {
                    tracing::warn!("Gemini CLI OAuth token expired — re-run `gemini` to refresh");
                    return None;
                }
            }
        }

        creds.access_token.and_then(|token| Self::normalize_non_empty(&token))
    }

    /// Get the Gemini CLI config directory (~/.gemini)
    fn gemini_cli_dir() -> Option<PathBuf> {
        UserDirs::new().map(|u| u.home_dir().join(".gemini"))
    }

    /// Check if Gemini CLI is configured and has valid credentials
    pub fn has_cli_credentials() -> bool {
        Self::try_load_gemini_cli_token().is_some()
    }

    /// Check if any Gemini authentication is available (env vars or file-based).
    pub fn has_any_auth() -> bool {
        Self::try_env_var_key().is_some() || Self::has_cli_credentials()
    }

    /// Get authentication source description for diagnostics.
    pub const fn auth_source(&self) -> &'static str {
        match self.auth.as_ref() {
            Some(GeminiAuth::ExplicitKey(_)) => "config/env",
            Some(GeminiAuth::OAuthToken(_)) => "Gemini CLI OAuth",
            None => "none",
        }
    }

    fn format_model_name(model: &str) -> String {
        if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{model}")
        }
    }

    fn format_internal_model_name(model: &str) -> String {
        model.strip_prefix("models/").unwrap_or(model).to_string()
    }

    /// Build the API URL based on auth type.
    ///
    /// - API key users → public `generativelanguage.googleapis.com/v1beta`
    /// - OAuth users → internal `cloudcode-pa.googleapis.com/v1internal`
    ///
    /// The Gemini CLI OAuth tokens are scoped for the internal Code Assist API,
    /// not the public API. Sending them to the public endpoint results in
    /// "400 Bad Request: API key not valid" errors.
    /// See: https://github.com/google-gemini/gemini-cli/issues/19200
    fn build_generate_content_url(model: &str, auth: &GeminiAuth) -> String {
        match auth {
            GeminiAuth::OAuthToken(_) => {
                // OAuth tokens from Gemini CLI are scoped for the internal
                // Code Assist API. The model is passed in the request body,
                // not the URL path.
                format!("{CLOUDCODE_PA_ENDPOINT}:generateContent")
            }
            _ => {
                let model_name = Self::format_model_name(model);
                let base_url = format!("{PUBLIC_API_ENDPOINT}/{model_name}:generateContent");

                if auth.is_api_key() {
                    format!("{base_url}?key={}", auth.credential())
                } else {
                    base_url
                }
            }
        }
    }

    /// **S3 T3-2-C**: Build the streaming URL using `streamGenerateContent`.
    ///
    /// We pass `?alt=sse` so Gemini emits standard `data: {...}\n\n` SSE
    /// records rather than a streamed JSON array; the SSE shape is much
    /// easier to parse incrementally and matches the OpenAI / Anthropic
    /// parsing approach already used in this crate.
    fn build_stream_url(model: &str, auth: &GeminiAuth) -> String {
        match auth {
            GeminiAuth::OAuthToken(_) => {
                // cloudcode-pa internal endpoint — model goes in the body.
                format!("{CLOUDCODE_PA_ENDPOINT}:streamGenerateContent?alt=sse")
            }
            _ => {
                let model_name = Self::format_model_name(model);
                let base = format!("{PUBLIC_API_ENDPOINT}/{model_name}:streamGenerateContent");
                if auth.is_api_key() {
                    format!("{base}?alt=sse&key={}", auth.credential())
                } else {
                    format!("{base}?alt=sse")
                }
            }
        }
    }

    fn http_client(&self) -> Client {
        crate::config::build_runtime_proxy_client_with_timeouts("provider.gemini", 120, 10)
            .map_err(|e| {
                tracing::error!("proxy build failed for provider.gemini, using direct: {e}");
                e
            })
            .unwrap_or_else(|_| Client::new())
    }

    fn build_generate_content_request(
        &self,
        auth: &GeminiAuth,
        url: &str,
        request: &GenerateContentRequest,
        model: &str,
    ) -> reqwest::RequestBuilder {
        let req = self.http_client().post(url).json(request);
        match auth {
            GeminiAuth::OAuthToken(token) => {
                // cloudcode-pa expects an outer envelope with `request`.
                let internal_request = InternalGenerateContentEnvelope {
                    model: Self::format_internal_model_name(model),
                    project: None,
                    user_prompt_id: None,
                    request: InternalGenerateContentRequest {
                        contents: request.contents.clone(),
                        system_instruction: request.system_instruction.clone(),
                        generation_config: request.generation_config.clone(),
                        tools: request.tools.clone(),
                    },
                };
                self.http_client().post(url).json(&internal_request).bearer_auth(token)
            }
            _ => req,
        }
    }
}

impl GeminiProvider {
    /// Convert generic `ChatMessage` history into Gemini-native `Content`
    /// values plus an optional combined `systemInstruction`. Used by both
    /// non-streaming `chat_with_history` and `stream_chat_with_history`.
    fn convert_messages(messages: &[ChatMessage]) -> (Option<Content>, Vec<Content>) {
        let mut system_parts: Vec<&str> = Vec::new();
        let mut contents: Vec<Content> = Vec::new();
        let mut tool_name_by_id = std::collections::HashMap::<String, String>::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" => system_parts.push(&msg.content),
                "user" => contents.push(Content {
                    role: Some("user".to_string()),
                    parts: Self::convert_user_parts(&msg.content),
                }),
                "assistant" => {
                    let parts = Self::convert_assistant_parts(&msg.content, &mut tool_name_by_id);
                    contents.push(Content {
                        // Gemini API uses "model" role instead of "assistant".
                        role: Some("model".to_string()),
                        parts,
                    });
                }
                "tool" => {
                    let part = Self::convert_tool_result_part(&msg.content, &tool_name_by_id);
                    contents.push(Content {
                        // Gemini function responses are sent as user turns.
                        role: Some("user".to_string()),
                        parts: vec![part],
                    });
                }
                _ => {}
            }
        }

        let system_instruction = if system_parts.is_empty() {
            None
        } else {
            Some(Content {
                role: None,
                parts: vec![RequestPart::Text {
                    text: system_parts.join("\n\n"),
                }],
            })
        };

        (system_instruction, contents)
    }

    fn convert_user_parts(content: &str) -> Vec<RequestPart> {
        let (cleaned, image_refs) = multimodal::parse_image_markers(content);
        if image_refs.is_empty() {
            return vec![RequestPart::Text {
                text: content.to_string(),
            }];
        }

        let mut parts = Vec::new();
        if !cleaned.trim().is_empty() {
            parts.push(RequestPart::Text {
                text: cleaned.trim().to_string(),
            });
        }
        for image_ref in image_refs {
            let Some(payload) = image_ref.strip_prefix("data:") else {
                parts.push(RequestPart::Text {
                    text: format!("[IMAGE:{image_ref}]"),
                });
                continue;
            };
            let Some((mime_type, data)) = payload.split_once(";base64,") else {
                parts.push(RequestPart::Text {
                    text: format!("[IMAGE:{image_ref}]"),
                });
                continue;
            };
            parts.push(RequestPart::InlineData {
                inline_data: InlineData {
                    mime_type: mime_type.to_string(),
                    data: data
                        .chars()
                        .filter(|character| !character.is_ascii_whitespace())
                        .collect(),
                },
            });
        }

        if parts.is_empty() {
            vec![RequestPart::Text {
                text: content.to_string(),
            }]
        } else {
            parts
        }
    }

    fn convert_assistant_parts(
        content: &str,
        tool_name_by_id: &mut std::collections::HashMap<String, String>,
    ) -> Vec<RequestPart> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
            return vec![RequestPart::Text {
                text: content.to_string(),
            }];
        };
        let mut parts = Vec::new();
        if let Some(text) = value.get("content").and_then(serde_json::Value::as_str) {
            if !text.is_empty() {
                parts.push(RequestPart::Text { text: text.to_string() });
            }
        }
        if let Some(tool_calls) = value.get("tool_calls") {
            if let Ok(tool_calls) = serde_json::from_value::<Vec<ToolCall>>(tool_calls.clone()) {
                for call in tool_calls {
                    let thought_signature = Self::thought_signature_from_tool_call_id(&call.id);
                    tool_name_by_id.insert(call.id, call.name.clone());
                    let args = serde_json::from_str(&call.arguments)
                        .unwrap_or_else(|_| serde_json::Value::String(call.arguments));
                    parts.push(RequestPart::FunctionCall {
                        function_call: RequestFunctionCall { name: call.name, args },
                        thought_signature,
                    });
                }
            }
        }
        if parts.is_empty() {
            parts.push(RequestPart::Text {
                text: content.to_string(),
            });
        }
        parts
    }

    fn convert_tool_result_part(
        content: &str,
        tool_name_by_id: &std::collections::HashMap<String, String>,
    ) -> RequestPart {
        let parsed = serde_json::from_str::<serde_json::Value>(content).unwrap_or_else(|_| {
            serde_json::json!({
                "content": content,
                "tool_name": "tool"
            })
        });
        let name = parsed
            .get("tool_name")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                parsed
                    .get("tool_call_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|id| tool_name_by_id.get(id))
                    .cloned()
            })
            .unwrap_or_else(|| "tool".to_string());
        let response = parsed
            .get("content")
            .cloned()
            .map(|value| match value {
                serde_json::Value::String(text) => {
                    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
                }
                other => other,
            })
            .unwrap_or(parsed);
        // Gemini declares FunctionResponse.response as a protobuf Struct, so
        // scalar/array tool outputs must be wrapped in an object.
        let response = if response.is_object() {
            response
        } else {
            serde_json::json!({ "result": response })
        };
        RequestPart::FunctionResponse {
            function_response: FunctionResponse { name, response },
        }
    }

    fn native_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<GeminiToolGroup>> {
        let declarations = tools?
            .iter()
            .map(|tool| GeminiFunctionDeclaration {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            })
            .collect::<Vec<_>>();
        (!declarations.is_empty()).then_some(vec![GeminiToolGroup {
            function_declarations: declarations,
        }])
    }

    fn parse_response(result: GenerateContentResponse) -> anyhow::Result<ChatResponse> {
        if let Some(err) = result.error {
            anyhow::bail!("Gemini API error: {}", err.message);
        }
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for candidate in result.candidates.unwrap_or_default() {
            for part in candidate.content.parts {
                if let Some(delta) = part.text {
                    text.push_str(&delta);
                }
                if let Some(call) = part.function_call {
                    let Some(name) = call.name.filter(|name| !name.is_empty()) else {
                        continue;
                    };
                    tool_calls.push(ToolCall {
                        id: Self::tool_call_id(part.thought_signature.as_deref()),
                        name,
                        arguments: serde_json::to_string(&call.args.unwrap_or_else(|| serde_json::json!({})))?,
                    });
                }
            }
        }
        if text.is_empty() && tool_calls.is_empty() {
            anyhow::bail!("No response from Gemini");
        }
        Ok(ChatResponse {
            text: (!text.is_empty()).then_some(text),
            tool_calls,
            reasoning_content: None,
        })
    }

    fn usage_metadata_to_reported(metadata: Option<&serde_json::Value>) -> Option<TokenUsage> {
        let metadata = metadata?;
        let field = |name: &str| -> Option<u32> {
            metadata
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
        };
        let prompt = field("promptTokenCount");
        let completion = field("candidatesTokenCount");
        let total = field("totalTokenCount").or_else(|| prompt.zip(completion).map(|(p, c)| p.saturating_add(c)));

        if total.is_some() || prompt.zip(completion).is_some() {
            Some(TokenUsage::reported(prompt, completion, total))
        } else {
            None
        }
    }

    fn estimate_completion_usage(text: &str) -> TokenUsage {
        let accumulator = ProviderUsageAccumulator::new();
        accumulator.finish_or_estimate_completion_chars(text.chars().count())
    }

    async fn send_generate_content_metered(
        &self,
        contents: Vec<Content>,
        system_instruction: Option<Content>,
        model: &str,
        temperature: f64,
        tools: Option<&[ToolSpec]>,
    ) -> anyhow::Result<(ChatResponse, TokenUsage)> {
        let auth = self.auth.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Gemini API key not found. Options:\n\
                 1. Add api_key to your Gemini provider config\n\
                 2. Set GEMINI_API_KEY or GOOGLE_API_KEY env var\n\
                 3. Run `gemini` CLI to authenticate (tokens will be reused)\n\
                 4. Get an API key from https://aistudio.google.com/app/apikey\n\
                 5. Run `prx onboard` to configure"
            )
        })?;

        let request = GenerateContentRequest {
            contents,
            system_instruction,
            generation_config: GenerationConfig {
                temperature,
                max_output_tokens: 8192,
            },
            tools: Self::native_tools(tools),
        };

        let url = Self::build_generate_content_url(model, auth);

        let response = self
            .build_generate_content_request(auth, &url, &request, model)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API error ({status}): {error_text}");
        }

        let result: GenerateContentResponse = response.json().await?;
        let result = result.into_effective_response();
        let usage = Self::usage_metadata_to_reported(result.usage_metadata.as_ref());
        let parsed = Self::parse_response(result)?;
        let tokens_used =
            usage.unwrap_or_else(|| Self::estimate_completion_usage(parsed.text.as_deref().unwrap_or("")));
        Ok((parsed, tokens_used))
    }

    async fn send_generate_content(
        &self,
        contents: Vec<Content>,
        system_instruction: Option<Content>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.send_generate_content_metered(contents, system_instruction, model, temperature, None)
            .await?
            .0
            .text
            .ok_or_else(|| anyhow::anyhow!("Gemini returned tool calls where text was required"))
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// STREAMING (S3 T3-2-C)
// ══════════════════════════════════════════════════════════════════════════════
//
// Gemini's `:streamGenerateContent` returns either:
//   - a JSON array streamed incrementally (the default), or
//   - standard SSE (`data: {json}\n\n`) when called with `?alt=sse`.
//
// We use `?alt=sse` to match the framing already handled by Anthropic /
// OpenAI parsers and avoid building an ad-hoc JSON-array-stream parser.
//
// Each SSE event payload is a fully-typed `GenerateContentResponse` with one
// or more `candidates[].content.parts[]`. Parts may carry:
//   - `text` deltas (we emit `StreamChunk::delta`), or
//   - a `functionCall { name, args }` object (we emit one terminal
//     `ToolCallChunk { status: Completed, args: <serialized JSON>, .. }`).
//
// **Gemini specifics**: Gemini does NOT stream function-call argument
// fragments today — the `args` object arrives complete in a single part.
// We therefore use the legacy "completion protocol" (single `Completed`
// chunk) for tool calls, mirroring the current Anthropic / OpenAI emission
// shape pre-T3-2. If/when Google adds incremental function-call streaming
// we can upgrade to the dual-phase `Streaming` + `Completed` protocol
// defined in T3-0.

/// Parse a single Gemini SSE record (`data: {...}` lines plus an optional
/// blank-line terminator). Returns `Ok(None)` for empty / comment-only
/// records, `Ok(Some(resp))` for a successfully decoded payload, and
/// `Err(StreamError::InvalidSse)` if the `data:` line is not valid JSON.
fn parse_gemini_sse_record(record: &str) -> StreamResult<Option<GenerateContentResponse>> {
    let mut data_lines: Vec<&str> = Vec::new();
    for line in record.split('\n') {
        let line = line.trim_end_matches('\r');
        // SSE comments start with ":" and must be ignored.
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start());
        }
        // Other SSE fields (`event:`, `id:`, `retry:`) are not used by Gemini.
    }
    if data_lines.is_empty() {
        return Ok(None);
    }
    let payload = data_lines.join("\n");
    if payload == "[DONE]" {
        // Gemini does not actually emit `[DONE]` today, but be defensive.
        return Ok(None);
    }
    let parsed: GenerateContentResponse =
        serde_json::from_str(&payload).map_err(|e| StreamError::InvalidSse(format!("gemini SSE json: {e}")))?;
    Ok(Some(parsed))
}

/// Drain one `GenerateContentResponse` chunk into zero or more `StreamChunk`
/// values, threading the running `tool_call_order` counter so successive
/// function-call chunks get monotonically increasing `index`es.
///
/// Returns `Err(StreamError::Provider)` if the chunk carries an API-level
/// error (`error.message` populated).
fn drain_gemini_chunk(
    chunk: GenerateContentResponse,
    tool_call_order: &mut usize,
    count_tokens: bool,
) -> StreamResult<Vec<StreamChunk>> {
    let effective = chunk.into_effective_response();
    if let Some(err) = effective.error {
        return Err(StreamError::Provider(err.message));
    }
    let usage = GeminiProvider::usage_metadata_to_reported(effective.usage_metadata.as_ref());
    let mut out: Vec<StreamChunk> = Vec::new();
    if let Some(candidates) = effective.candidates {
        for candidate in candidates {
            for part in candidate.content.parts {
                if let Some(text) = part.text {
                    if !text.is_empty() {
                        let mut sc = StreamChunk::delta(text);
                        if count_tokens {
                            sc = sc.with_token_estimate();
                        }
                        out.push(sc);
                    }
                }
                if let Some(call) = part.function_call {
                    let name = call.name.unwrap_or_default();
                    if name.is_empty() {
                        // Defensive: skip malformed function_call entries that
                        // are missing a name — surfacing them would break the
                        // driver's tool dispatch which keys on the registered
                        // tool name.
                        continue;
                    }
                    // `serde_json::to_string` on a parsed `Value` cannot
                    // realistically fail (no non-string map keys, no NaN at this
                    // layer); fall back to `{}` defensively so a malformed
                    // payload degrades gracefully rather than killing the
                    // streaming task mid-tool-call.
                    let args_str = call.args.map_or_else(
                        || "{}".to_string(),
                        |v| serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string()),
                    );
                    // Gemini does not provide stable tool-call IDs in its API
                    // surface (function calls are correlated by name + position).
                    // Generate a UUID to align with OpenAI / Anthropic semantics
                    // expected by the driver-side aggregator.
                    let id = GeminiProvider::tool_call_id(part.thought_signature.as_deref());
                    let index = *tool_call_order;
                    *tool_call_order = tool_call_order.saturating_add(1);
                    let tc = ToolCallChunk {
                        id,
                        name,
                        args: args_str,
                        index,
                        arguments_delta: None,
                        status: ToolCallChunkStatus::Completed,
                    };
                    out.push(StreamChunk::tool_call_chunk(vec![tc]));
                }
            }
        }
    }
    if let Some(usage) = usage {
        out.push(StreamChunk::usage(usage));
    }
    Ok(out)
}

#[async_trait]
impl Provider for GeminiProvider {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        let function_declarations = tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect();
        ToolsPayload::Gemini { function_declarations }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let system_instruction = system_prompt.map(|sys| Content {
            role: None,
            parts: vec![RequestPart::Text { text: sys.to_string() }],
        });

        let contents = vec![Content {
            role: Some("user".to_string()),
            parts: vec![RequestPart::Text {
                text: message.to_string(),
            }],
        }];

        self.send_generate_content(contents, system_instruction, model, temperature)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let (system_instruction, contents) = Self::convert_messages(messages);
        self.send_generate_content(contents, system_instruction, model, temperature)
            .await
    }

    async fn chat(
        &self,
        request: crate::providers::traits::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        Ok(self.chat_traced(request, model, temperature).await?.response)
    }

    async fn chat_traced(
        &self,
        request: crate::providers::traits::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatTrace> {
        let started_at = chrono::Utc::now();
        let (system_instruction, contents) = Self::convert_messages(request.messages);
        let (response, tokens_used) = self
            .send_generate_content_metered(contents, system_instruction, model, temperature, request.tools)
            .await?;
        let finished_at = chrono::Utc::now();
        Ok(ChatTrace {
            response,
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: "gemini".to_string(),
                model: model.to_string(),
                started_at,
                finished_at,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: "gemini".to_string(),
            final_model: model.to_string(),
            tokens_used,
        })
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let tool_specs = tools
            .iter()
            .map(|tool| {
                let function = tool
                    .get("function")
                    .ok_or_else(|| anyhow::anyhow!("Gemini tool definition is missing 'function'"))?;
                let name = function
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Gemini tool definition is missing function.name"))?;
                Ok(ToolSpec {
                    name: name.to_string(),
                    description: function
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    parameters: function
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let (system_instruction, contents) = Self::convert_messages(messages);
        Ok(self
            .send_generate_content_metered(contents, system_instruction, model, temperature, Some(&tool_specs))
            .await?
            .0)
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        if let Some(auth) = self.auth.as_ref() {
            // cloudcode-pa does not expose a lightweight model-list probe like the public API.
            // Avoid false negatives for valid Gemini CLI OAuth credentials.
            if auth.is_oauth() {
                return Ok(());
            }

            let url = if auth.is_api_key() {
                format!(
                    "https://generativelanguage.googleapis.com/v1beta/models?key={}",
                    auth.credential()
                )
            } else {
                "https://generativelanguage.googleapis.com/v1beta/models".to_string()
            };

            self.http_client().get(&url).send().await?.error_for_status()?;
        }
        Ok(())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    /// **S3 T3-2-C**: Native Gemini `:streamGenerateContent` (SSE mode).
    ///
    /// Each `data:` record decodes to a partial [`GenerateContentResponse`];
    /// text parts are emitted as [`StreamChunk::delta`] and any `functionCall`
    /// part is emitted as a single terminal `Completed` [`ToolCallChunk`]
    /// (Gemini does not stream function-call argument fragments today —
    /// see TODO at the top of the streaming section).
    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        let Some(auth) = self.auth.as_ref() else {
            return stream::once(async {
                Err(StreamError::Provider(
                    "Gemini API key not found — set GEMINI_API_KEY / GOOGLE_API_KEY or run `gemini` CLI".into(),
                ))
            })
            .boxed();
        };

        let (system_instruction, contents) = Self::convert_messages(messages);
        let request_body = GenerateContentRequest {
            contents,
            system_instruction,
            generation_config: GenerationConfig {
                temperature,
                max_output_tokens: 8192,
            },
            tools: Self::native_tools(options.tools.as_deref()),
        };

        let url = Self::build_stream_url(model, auth);
        let client = self.http_client();
        let model_owned = model.to_string();
        let auth_kind = match auth {
            GeminiAuth::OAuthToken(token) => OwnedGeminiAuth::OAuthToken(token.clone()),
            // The explicit API key is already embedded in `url` as the
            // `?key=` query parameter, so we don't need to carry the
            // value across the await boundary — we only need the variant
            // to choose the right request shape.
            GeminiAuth::ExplicitKey(_) => OwnedGeminiAuth::ExplicitKey,
        };
        let count_tokens = options.count_tokens;

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(64);

        tokio::spawn(async move {
            let request_builder = match &auth_kind {
                OwnedGeminiAuth::OAuthToken(token) => {
                    let envelope = InternalGenerateContentEnvelope {
                        model: Self::format_internal_model_name(&model_owned),
                        project: None,
                        user_prompt_id: None,
                        request: InternalGenerateContentRequest {
                            contents: request_body.contents.clone(),
                            system_instruction: request_body.system_instruction.clone(),
                            generation_config: request_body.generation_config.clone(),
                            tools: request_body.tools.clone(),
                        },
                    };
                    client
                        .post(&url)
                        .header("accept", "text/event-stream")
                        .json(&envelope)
                        .bearer_auth(token)
                }
                OwnedGeminiAuth::ExplicitKey => client
                    .post(&url)
                    .header("accept", "text/event-stream")
                    .json(&request_body),
            };

            let response = match request_builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
                    return;
                }
            };

            if !response.status().is_success() {
                let _ = tx.send(Err(super::stream_api_error("Gemini", response).await)).await;
                return;
            }

            let mut tool_call_order: usize = 0;
            let mut byte_stream = response.bytes_stream();
            let mut buf = String::new();

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
                                "non-utf8 byte in Gemini SSE: {e}"
                            ))))
                            .await;
                        return;
                    }
                };
                buf.push_str(&text);

                // SSE records are separated by blank lines (`\n\n`).
                while let Some(end) = buf.find("\n\n") {
                    let record: String = buf.drain(..end + 2).collect();
                    let parsed = match parse_gemini_sse_record(&record) {
                        Ok(Some(p)) => p,
                        Ok(None) => continue,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    };
                    let chunks = match drain_gemini_chunk(parsed, &mut tool_call_order, count_tokens) {
                        Ok(cs) => cs,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    };
                    for chunk in chunks {
                        if tx.send(Ok(chunk)).await.is_err() {
                            break 'outer;
                        }
                    }
                }
            }

            // Gemini's SSE stream simply closes when complete (no
            // explicit `[DONE]` marker), so we always emit a synthesised
            // final chunk after the byte stream is drained.
            let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
        });

        stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
    }
}

/// Owned variant of [`GeminiAuth`] usable from a `'static` `tokio::spawn`
/// future. Mirrors the borrowed enum so we can take ownership of the
/// credential before crossing the await boundary.
///
/// `ExplicitKey` carries no payload because the API key is already encoded
/// into the URL `?key=` query parameter before the future is spawned —
/// the variant exists only to select the request-shape branch.
#[derive(Debug)]
enum OwnedGeminiAuth {
    ExplicitKey,
    OAuthToken(String),
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::AUTHORIZATION;

    #[test]
    fn normalize_non_empty_trims_and_filters() {
        assert_eq!(GeminiProvider::normalize_non_empty(" value "), Some("value".into()));
        assert_eq!(GeminiProvider::normalize_non_empty(""), None);
        assert_eq!(GeminiProvider::normalize_non_empty(" \t\n"), None);
    }

    #[test]
    fn provider_creates_without_key() {
        let provider = GeminiProvider::new(None);
        // May pick up env vars; just verify it doesn't panic
        let _ = provider.auth_source();
    }

    #[test]
    fn provider_creates_with_key() {
        let provider = GeminiProvider::new(Some("test-api-key"));
        assert!(matches!(
            provider.auth,
            Some(GeminiAuth::ExplicitKey(ref key)) if key == "test-api-key"
        ));
    }

    #[test]
    fn provider_rejects_empty_key() {
        let provider = GeminiProvider::new(Some(""));
        assert!(!matches!(provider.auth, Some(GeminiAuth::ExplicitKey(_))));
    }

    #[test]
    fn capabilities_match_sent_native_tools_and_vision_parts() {
        let provider = GeminiProvider::new(Some("test-api-key"));
        assert!(provider.capabilities().native_tool_calling);
        assert!(provider.capabilities().vision);
    }

    #[test]
    fn native_tool_request_serializes_function_declarations() {
        let tools = vec![ToolSpec {
            name: "get_weather".into(),
            description: "Get weather".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        }];
        let request = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".into()),
                parts: vec![RequestPart::Text {
                    text: "Weather?".into(),
                }],
            }],
            system_instruction: None,
            generation_config: GenerationConfig {
                temperature: 0.0,
                max_output_tokens: 8192,
            },
            tools: GeminiProvider::native_tools(Some(&tools)),
        };

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["tools"][0]["functionDeclarations"][0]["name"], "get_weather");
        assert_eq!(
            json["tools"][0]["functionDeclarations"][0]["parameters"]["required"][0],
            "city"
        );
    }

    #[test]
    fn user_image_marker_serializes_as_inline_data() {
        let parts = GeminiProvider::convert_user_parts("Inspect [IMAGE:data:image/png;base64,aGVs\n bG8=] carefully");
        let json = serde_json::to_value(parts).unwrap();

        assert_eq!(json[0]["text"], "Inspect  carefully");
        assert_eq!(json[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(json[1]["inlineData"]["data"], "aGVsbG8=");
    }

    #[test]
    fn function_call_and_result_round_trip_use_gemini_native_parts() {
        let messages = vec![
            ChatMessage::assistant(
                serde_json::json!({
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "name": "get_weather",
                        "arguments": "{\"city\":\"Paris\"}"
                    }]
                })
                .to_string(),
            ),
            ChatMessage::tool(
                serde_json::json!({
                    "tool_call_id": "call-1",
                    "content": "{\"temperature\":20}"
                })
                .to_string(),
            ),
        ];
        let (_, contents) = GeminiProvider::convert_messages(&messages);
        let json = serde_json::to_value(contents).unwrap();

        assert_eq!(json[0]["role"], "model");
        assert_eq!(json[0]["parts"][0]["functionCall"]["name"], "get_weather");
        assert_eq!(json[1]["role"], "user");
        assert_eq!(json[1]["parts"][0]["functionResponse"]["name"], "get_weather");
        assert_eq!(json[1]["parts"][0]["functionResponse"]["response"]["temperature"], 20);
    }

    #[test]
    fn scalar_tool_result_is_wrapped_as_gemini_response_struct() {
        let messages = vec![
            ChatMessage::assistant(
                serde_json::json!({
                    "tool_calls": [{
                        "id": "call-1",
                        "name": "lookup",
                        "arguments": "{}"
                    }]
                })
                .to_string(),
            ),
            ChatMessage::tool(
                serde_json::json!({
                    "tool_call_id": "call-1",
                    "content": "plain text"
                })
                .to_string(),
            ),
        ];
        let (_, contents) = GeminiProvider::convert_messages(&messages);
        let json = serde_json::to_value(contents).unwrap();

        assert_eq!(
            json[1]["parts"][0]["functionResponse"]["response"]["result"],
            "plain text"
        );
    }

    #[test]
    fn non_streaming_function_call_parses_into_provider_tool_call() {
        let response: GenerateContentResponse = serde_json::from_value(serde_json::json!({
            "candidates": [{
                "content": {"parts": [{
                    "functionCall": {"name": "get_weather", "args": {"city": "Paris"}},
                    "thoughtSignature": "c2lnbmF0dXJl"
                }]}
            }]
        }))
        .unwrap();
        let parsed = GeminiProvider::parse_response(response).unwrap();

        assert!(parsed.text.is_none());
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "get_weather");
        assert_eq!(parsed.tool_calls[0].arguments, "{\"city\":\"Paris\"}");
        assert!(parsed.tool_calls[0].id.starts_with("gemini:c2lnbmF0dXJl:"));

        let history = vec![ChatMessage::assistant(
            serde_json::json!({
                "tool_calls": [parsed.tool_calls[0].clone()]
            })
            .to_string(),
        )];
        let (_, contents) = GeminiProvider::convert_messages(&history);
        let json = serde_json::to_value(contents).unwrap();
        assert_eq!(json[0]["parts"][0]["thoughtSignature"], "c2lnbmF0dXJl");
    }

    #[test]
    fn gemini_cli_dir_returns_path() {
        let dir = GeminiProvider::gemini_cli_dir();
        // Should return Some on systems with home dir
        if UserDirs::new().is_some() {
            assert!(dir.is_some());
            assert!(dir.unwrap().ends_with(".gemini"));
        }
    }

    #[test]
    fn auth_source_explicit_key() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::ExplicitKey("key".into())),
        };
        assert_eq!(provider.auth_source(), "config/env");
    }

    #[test]
    fn auth_source_none_without_credentials() {
        let provider = GeminiProvider { auth: None };
        assert_eq!(provider.auth_source(), "none");
    }

    #[test]
    fn auth_source_oauth() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::OAuthToken("ya29.mock".into())),
        };
        assert_eq!(provider.auth_source(), "Gemini CLI OAuth");
    }

    #[test]
    fn model_name_formatting() {
        assert_eq!(
            GeminiProvider::format_model_name("gemini-2.0-flash"),
            "models/gemini-2.0-flash"
        );
        assert_eq!(
            GeminiProvider::format_model_name("models/gemini-1.5-pro"),
            "models/gemini-1.5-pro"
        );
        assert_eq!(
            GeminiProvider::format_internal_model_name("models/gemini-2.5-flash"),
            "gemini-2.5-flash"
        );
        assert_eq!(
            GeminiProvider::format_internal_model_name("gemini-2.5-flash"),
            "gemini-2.5-flash"
        );
    }

    #[test]
    fn api_key_url_includes_key_query_param() {
        let auth = GeminiAuth::ExplicitKey("api-key-123".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        assert!(url.contains(":generateContent?key=api-key-123"));
    }

    #[test]
    fn oauth_url_uses_internal_endpoint() {
        let auth = GeminiAuth::OAuthToken("ya29.test-token".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        assert!(url.starts_with("https://cloudcode-pa.googleapis.com/v1internal"));
        assert!(url.ends_with(":generateContent"));
        assert!(!url.contains("generativelanguage.googleapis.com"));
        assert!(!url.contains("?key="));
    }

    #[test]
    fn api_key_url_uses_public_endpoint() {
        let auth = GeminiAuth::ExplicitKey("api-key-123".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        assert!(url.contains("generativelanguage.googleapis.com/v1beta"));
        assert!(url.contains("models/gemini-2.0-flash"));
    }

    #[test]
    fn oauth_request_uses_bearer_auth_header() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::OAuthToken("ya29.mock-token".into())),
        };
        let auth = GeminiAuth::OAuthToken("ya29.mock-token".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        let body = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".into()),
                parts: vec![RequestPart::Text { text: "hello".into() }],
            }],
            system_instruction: None,
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
            tools: None,
        };

        let request = provider
            .build_generate_content_request(&auth, &url, &body, "gemini-2.0-flash")
            .build()
            .unwrap();

        assert_eq!(
            request.headers().get(AUTHORIZATION).and_then(|h| h.to_str().ok()),
            Some("Bearer ya29.mock-token")
        );
    }

    #[test]
    fn oauth_request_wraps_payload_in_request_envelope() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::OAuthToken("ya29.mock-token".into())),
        };
        let auth = GeminiAuth::OAuthToken("ya29.mock-token".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        let body = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".into()),
                parts: vec![RequestPart::Text { text: "hello".into() }],
            }],
            system_instruction: None,
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
            tools: None,
        };

        let request = provider
            .build_generate_content_request(&auth, &url, &body, "models/gemini-2.0-flash")
            .build()
            .unwrap();

        let payload = request
            .body()
            .and_then(|b| b.as_bytes())
            .expect("json request body should be bytes");
        let json: serde_json::Value = serde_json::from_slice(payload).unwrap();

        assert_eq!(json["model"], "gemini-2.0-flash");
        assert!(json.get("generationConfig").is_none());
        assert!(json.get("request").is_some());
        assert!(json["request"].get("generationConfig").is_some());
    }

    #[test]
    fn api_key_request_does_not_set_bearer_header() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::ExplicitKey("api-key-123".into())),
        };
        let auth = GeminiAuth::ExplicitKey("api-key-123".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        let body = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".into()),
                parts: vec![RequestPart::Text { text: "hello".into() }],
            }],
            system_instruction: None,
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
            tools: None,
        };

        let request = provider
            .build_generate_content_request(&auth, &url, &body, "gemini-2.0-flash")
            .build()
            .unwrap();

        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    #[test]
    fn request_serialization() {
        let request = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".to_string()),
                parts: vec![RequestPart::Text {
                    text: "Hello".to_string(),
                }],
            }],
            system_instruction: Some(Content {
                role: None,
                parts: vec![RequestPart::Text {
                    text: "You are helpful".to_string(),
                }],
            }),
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
            tools: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"text\":\"Hello\""));
        assert!(json.contains("\"systemInstruction\""));
        assert!(!json.contains("\"system_instruction\""));
        assert!(json.contains("\"temperature\":0.7"));
        assert!(json.contains("\"maxOutputTokens\":8192"));
    }

    #[test]
    fn internal_request_includes_model() {
        let request = InternalGenerateContentEnvelope {
            model: "gemini-test-model".to_string(),
            project: None,
            user_prompt_id: None,
            request: InternalGenerateContentRequest {
                contents: vec![Content {
                    role: Some("user".to_string()),
                    parts: vec![RequestPart::Text {
                        text: "Hello".to_string(),
                    }],
                }],
                system_instruction: None,
                generation_config: GenerationConfig {
                    temperature: 0.7,
                    max_output_tokens: 8192,
                },
                tools: None,
            },
        };

        let json: serde_json::Value = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "gemini-test-model");
        assert!(json.get("generationConfig").is_none());
        assert!(json["request"].get("generationConfig").is_some());
        assert_eq!(json["request"]["contents"][0]["role"], "user");
    }

    #[test]
    fn response_deserialization() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello there!"}]
                }
            }]
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        assert!(response.candidates.is_some());
        let text = response
            .candidates
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .content
            .parts
            .into_iter()
            .next()
            .unwrap()
            .text;
        assert_eq!(text, Some("Hello there!".to_string()));
    }

    #[test]
    fn usage_metadata_maps_to_reported_tokens() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello there!"}]
                }
            }],
            "usageMetadata": {
                "promptTokenCount": 12,
                "candidatesTokenCount": 7,
                "totalTokenCount": 19
            }
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        let usage = GeminiProvider::usage_metadata_to_reported(response.usage_metadata.as_ref()).unwrap();

        assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Reported);
        assert_eq!(usage.prompt_tokens, Some(12));
        assert_eq!(usage.completion_tokens, Some(7));
        assert_eq!(usage.total_tokens, Some(19));
    }

    #[test]
    fn malformed_or_partial_usage_metadata_does_not_fabricate_reported_usage() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello there!"}]
                }
            }],
            "usageMetadata": {
                "promptTokenCount": "bad"
            }
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        let mut order = 0;
        let chunks = drain_gemini_chunk(response, &mut order, true).unwrap();

        assert!(chunks.iter().any(|chunk| chunk.delta == "Hello there!"));
        assert!(
            chunks.iter().all(|chunk| chunk.usage.is_none()),
            "malformed/partial usage must fall back to estimate outside the parser"
        );
    }

    #[test]
    fn error_response_deserialization() {
        let json = r#"{
            "error": {
                "message": "Invalid API key"
            }
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().message, "Invalid API key");
    }

    #[test]
    fn internal_response_deserialization() {
        let json = r#"{
            "response": {
                "candidates": [{
                    "content": {
                        "parts": [{"text": "Hello from internal"}]
                    }
                }]
            }
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        let text = response
            .into_effective_response()
            .candidates
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .content
            .parts
            .into_iter()
            .next()
            .unwrap()
            .text;
        assert_eq!(text, Some("Hello from internal".to_string()));
    }

    #[tokio::test]
    async fn warmup_without_key_is_noop() {
        let provider = GeminiProvider { auth: None };
        let result = provider.warmup().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn warmup_oauth_is_noop() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::OAuthToken("ya29.mock-token".into())),
        };
        let result = provider.warmup().await;
        assert!(result.is_ok());
    }

    // ─── S3 T3-2-C streaming protocol tests ────────────────────────────────

    #[test]
    fn stream_url_for_api_key_uses_sse_and_public_endpoint() {
        let auth = GeminiAuth::ExplicitKey("api-key-123".into());
        let url = GeminiProvider::build_stream_url("gemini-2.0-flash", &auth);
        assert!(url.contains("generativelanguage.googleapis.com/v1beta"));
        assert!(url.contains(":streamGenerateContent"));
        assert!(url.contains("alt=sse"));
        assert!(url.contains("key=api-key-123"));
    }

    #[test]
    fn stream_url_for_oauth_uses_internal_endpoint() {
        let auth = GeminiAuth::OAuthToken("ya29.mock".into());
        let url = GeminiProvider::build_stream_url("gemini-2.5-pro", &auth);
        assert!(url.starts_with("https://cloudcode-pa.googleapis.com/v1internal"));
        assert!(url.contains(":streamGenerateContent"));
        assert!(url.contains("alt=sse"));
        assert!(!url.contains("?key="));
    }

    #[test]
    fn parse_sse_record_skips_blank_and_comments() {
        // Empty record -> None.
        let empty = parse_gemini_sse_record("\n\n").expect("empty record is valid");
        assert!(empty.is_none());

        // Comment-only record (`:` lines) -> None.
        let only_comments = parse_gemini_sse_record(": keepalive\n: heartbeat\n\n").expect("comments valid");
        assert!(only_comments.is_none());
    }

    #[test]
    fn parse_sse_record_decodes_text_payload() {
        let record = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}]}\n\n";
        let parsed = parse_gemini_sse_record(record).expect("ok").expect("payload present");
        let text = parsed
            .candidates
            .expect("candidates")
            .into_iter()
            .next()
            .expect("first candidate")
            .content
            .parts
            .into_iter()
            .next()
            .expect("first part")
            .text;
        assert_eq!(text.as_deref(), Some("Hello"));
    }

    #[test]
    fn parse_sse_record_rejects_invalid_json() {
        let err = parse_gemini_sse_record("data: {not-json\n\n").expect_err("invalid json");
        match err {
            StreamError::InvalidSse(msg) => assert!(msg.contains("gemini SSE json")),
            other => panic!("expected InvalidSse, got {other:?}"),
        }
    }

    /// **T3-2-C required**: mock a Gemini SSE stream of multiple text deltas
    /// and verify the parsed `StreamChunk` sequence: order is preserved and
    /// concatenation reproduces the full assistant text.
    #[test]
    fn test_gemini_streaming_text_chunks() {
        let records = [
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello, \"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"world\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}]}}]}\n\n",
        ];

        let mut tool_order: usize = 0;
        let mut collected: Vec<StreamChunk> = Vec::new();
        for rec in records {
            let parsed = parse_gemini_sse_record(rec)
                .expect("parse ok")
                .expect("payload present");
            let chunks = drain_gemini_chunk(parsed, &mut tool_order, false).expect("drain ok");
            collected.extend(chunks);
        }

        // Three pure-text chunks, no tool_calls.
        assert_eq!(collected.len(), 3, "expected 3 text deltas");
        assert!(collected.iter().all(|c| c.tool_calls.is_empty()));
        let combined: String = collected.iter().map(|c| c.delta.clone()).collect();
        assert_eq!(combined, "Hello, world!");
        // Order counter not advanced when there are no function calls.
        assert_eq!(tool_order, 0);
    }

    /// **T3-2-C required**: a Gemini stream carrying a `functionCall` part
    /// must surface as a single terminal `Completed` `ToolCallChunk` with
    /// the full JSON arguments and `arguments_delta = None`. Gemini does not
    /// stream argument fragments, so the legacy completion protocol is the
    /// only valid shape today.
    #[test]
    fn test_gemini_streaming_function_call_completed() {
        let record = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"shell","args":{"command":"ls -la"}}}]}}]}

"#;
        let parsed = parse_gemini_sse_record(record)
            .expect("parse ok")
            .expect("payload present");

        let mut tool_order: usize = 0;
        let chunks = drain_gemini_chunk(parsed, &mut tool_order, false).expect("drain ok");

        assert_eq!(chunks.len(), 1, "expected single tool_call chunk");
        let chunk = &chunks[0];
        assert!(chunk.delta.is_empty(), "tool_call chunk has no visible delta");
        assert_eq!(chunk.tool_calls.len(), 1);

        let tc = &chunk.tool_calls[0];
        assert_eq!(tc.name, "shell");
        assert_eq!(tc.index, 0);
        assert_eq!(
            tc.status,
            ToolCallChunkStatus::Completed,
            "Gemini emits Completed-only (no streaming fragments today)"
        );
        assert!(tc.arguments_delta.is_none(), "Completed.arguments_delta MUST be None");
        assert!(!tc.id.is_empty(), "fresh UUID assigned for missing API id");

        // Args round-trip back to a JSON object containing the expected command.
        let args_val: serde_json::Value = serde_json::from_str(&tc.args).expect("args is JSON");
        assert_eq!(args_val["command"], "ls -la");

        // Successive function_calls in the same stream get monotonic indexes.
        assert_eq!(tool_order, 1);
    }

    /// Mixed text + functionCall in a single SSE record: each part becomes
    /// its own `StreamChunk`, and order is preserved.
    #[test]
    fn drain_gemini_chunk_mixes_text_and_function_call_in_order() {
        let record = r#"data: {"candidates":[{"content":{"parts":[{"text":"calling tool now"},{"functionCall":{"name":"search","args":{"q":"rust"}}}]}}]}

"#;
        let parsed = parse_gemini_sse_record(record)
            .expect("parse ok")
            .expect("payload present");

        let mut tool_order: usize = 0;
        let chunks = drain_gemini_chunk(parsed, &mut tool_order, false).expect("drain ok");

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].delta, "calling tool now");
        assert!(chunks[0].tool_calls.is_empty());

        assert!(chunks[1].delta.is_empty());
        assert_eq!(chunks[1].tool_calls.len(), 1);
        assert_eq!(chunks[1].tool_calls[0].name, "search");
        assert_eq!(chunks[1].tool_calls[0].status, ToolCallChunkStatus::Completed);
    }

    /// An empty-name `functionCall` part is defensively skipped (the driver
    /// keys tool dispatch on `name`, so a nameless call is unactionable).
    #[test]
    fn drain_gemini_chunk_skips_function_call_without_name() {
        let record = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"args\":{}}}]}}]}\n\n";
        let parsed = parse_gemini_sse_record(record)
            .expect("parse ok")
            .expect("payload present");

        let mut tool_order: usize = 0;
        let chunks = drain_gemini_chunk(parsed, &mut tool_order, false).expect("drain ok");

        assert!(chunks.is_empty(), "malformed function_call must be skipped");
        assert_eq!(tool_order, 0, "skipped call must not consume index");
    }

    /// Chunk-level API error (`error.message` populated) is surfaced as a
    /// `StreamError::Provider` so the streaming task can fail fast.
    #[test]
    fn drain_gemini_chunk_surfaces_api_error() {
        let record = "data: {\"error\":{\"message\":\"quota exceeded\"}}\n\n";
        let parsed = parse_gemini_sse_record(record)
            .expect("parse ok")
            .expect("payload present");

        let mut tool_order: usize = 0;
        let err = drain_gemini_chunk(parsed, &mut tool_order, false).expect_err("error chunk");
        match err {
            StreamError::Provider(msg) => assert!(msg.contains("quota exceeded")),
            other => panic!("expected Provider error, got {other:?}"),
        }
    }

    /// `into_effective_response` unwrapping: cloudcode-pa wraps payloads in
    /// `{ "response": {...} }`. The drain helper must look through that
    /// envelope so streaming parses identically for OAuth & API-key paths.
    #[test]
    fn drain_gemini_chunk_unwraps_cloudcode_pa_envelope() {
        let record = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}}\n\n";
        let parsed = parse_gemini_sse_record(record)
            .expect("parse ok")
            .expect("payload present");

        let mut tool_order: usize = 0;
        let chunks = drain_gemini_chunk(parsed, &mut tool_order, false).expect("drain ok");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].delta, "hi");
    }

    /// `supports_streaming()` must be `true` for the trait-object dispatch
    /// path (router uses this to choose between streaming and one-shot).
    #[test]
    fn supports_streaming_returns_true() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::ExplicitKey("k".into())),
        };
        assert!(provider.supports_streaming());
    }
}
