//! Generic OpenAI-compatible provider.
//! Most LLM APIs follow the same `/v1/chat/completions` format.
//! This module provides a single implementation that works for all of them.

use crate::llm::route_decision::{AttemptStatus, ProviderAttempt, TokenUsage};
use crate::multimodal;
use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, ChatTrace, Provider,
    StreamChunk, StreamError, StreamOptions, StreamResult, ToolCall as ProviderToolCall, ToolCallChunk,
    ToolCallChunkStatus,
};
use crate::providers::utf8_stream::SseTextDecoder;
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use serde::{Deserialize, Serialize};

/// Provider name attached to byte-level UTF-8 decode failures on the
/// streaming path.
const STREAM_DECODER_LABEL: &str = "OpenAI-compatible";

/// Highest streamed `tool_calls[].index` accepted. The index sizes the
/// per-call buffer, and it is chosen by the server; one SSE line naming a huge
/// index must not allocate a buffer entry for every slot below it.
const MAX_STREAMED_TOOL_CALL_INDEX: usize = 128;

/// A provider that speaks the OpenAI-compatible chat completions API.
///
/// Used by: Venice, Vercel AI Gateway, Cloudflare AI Gateway, Moonshot,
/// Synthetic, `OpenCode` Zen, `Z.AI`, `GLM`, `MiniMax`, Bedrock, Qianfan, Groq, Mistral, `xAI`, etc.
pub struct OpenAiCompatibleProvider {
    pub(crate) name: String,
    pub(crate) base_url: String,
    pub(crate) credential: Option<String>,
    pub(crate) auth_header: AuthStyle,
    /// When false, do not fall back to /v1/responses on chat completions 404.
    /// GLM/Zhipu does not support the responses API.
    supports_responses_fallback: bool,
    user_agent: Option<String>,
    /// When true, collect all `system` messages and prepend their content
    /// to the first `user` message, then drop the system messages.
    /// Required for providers that reject `role: system` (e.g. MiniMax).
    merge_system_into_user: bool,
    /// When true, a missing credential is a valid configuration: requests go
    /// out with no authorization header at all. Only bring-your-own endpoints
    /// set this; every named provider keeps the hard refusal.
    allows_anonymous: bool,
    /// What this endpoint accepts in a tool schema, as declared by an operator.
    ///
    /// Each entry is a model — or `None` for every model of this provider —
    /// and the dialect to lower tool schemas to. Empty means
    /// [`ToolSchemaDialect::Raw`](crate::tools::ToolSchemaDialect::Raw): PRX
    /// knows nothing about an arbitrary OpenAI-compatible endpoint, and a URL
    /// is not evidence, so the published document goes out unchanged and the
    /// validator holds the model to all of it.
    tool_schema_dialects: Vec<(Option<String>, crate::tools::ToolSchemaDialect)>,
    /// Which textual tool-call grammar this endpoint was declared to emit.
    ///
    /// Empty means [`ToolCallDialect::Native`](super::ToolCallDialect::Native):
    /// only the provider's `tool_calls` array is read, and prose stays prose.
    tool_call_dialects: Vec<(Option<String>, super::ToolCallDialect)>,
    /// The provider label this endpoint was constructed from, set when a
    /// `[[providers.tool_dialects]]` entry applies to it (the entry names the
    /// same provider identity). `name` is a display name (`"Custom"` for every
    /// bring-your-own endpoint), so a refusal names this instead: it is how the
    /// operator finds the entry to edit.
    declaration_provider: Option<String>,
    /// The provider label this endpoint was constructed from, with URL
    /// userinfo redacted, whether or not a declaration applies. A diagnostic
    /// that names the declaration to add prints it.
    configured_label: Option<String>,
}

/// An endpoint nobody declared a dialect for rejected a root combinator in a
/// tool schema, with the sentence naming the declaration that would fix it.
///
/// Typed so the failover summary can clip the upstream body, as it does every
/// upstream body, while keeping the hint whole: the hint holds only the
/// redacted provider label and the model.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{upstream}{hint}")]
pub struct RootCombinatorRejection {
    /// The upstream error, as every other non-2xx is rendered.
    pub upstream: String,
    /// The declaration hint, starting with a space.
    pub hint: String,
}

/// How the provider expects the API key to be sent.
#[derive(Debug, Clone)]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>` (used by some Chinese providers)
    XApiKey,
    /// Custom header name
    Custom(String),
}

impl OpenAiCompatibleProvider {
    fn requires_http1_only(&self) -> bool {
        let normalized = self.base_url.to_ascii_lowercase();
        normalized.contains("dashscope") || normalized.contains("coding-intl")
    }

    fn should_skip_warmup(&self) -> bool {
        self.requires_http1_only()
    }

    fn base_http_client_builder(&self) -> reqwest::ClientBuilder {
        let builder = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .user_agent("OpenPRX/0.1");
        let builder = if self.requires_http1_only() {
            builder.http1_only()
        } else {
            builder
        };

        crate::config::apply_runtime_proxy_to_builder(builder, "provider.compatible")
    }

    fn http_client_builder(&self) -> reqwest::ClientBuilder {
        // A buffered response can legitimately take longer than two minutes,
        // especially when a single-slot local server queues concurrent turns.
        // The agent's progress-aware idle guard owns hang recovery; a client
        // deadline here would turn slow-but-healthy work into a transport error.
        self.base_http_client_builder()
    }

    /// Streaming responses may legitimately run for many minutes. A reqwest
    /// client-level timeout covers the entire response body, not just the
    /// connection handshake, so applying the non-streaming two-minute limit
    /// here truncates healthy SSE streams while tokens are still arriving.
    fn streaming_http_client_builder(&self) -> reqwest::ClientBuilder {
        self.base_http_client_builder()
    }

    pub fn new(name: &str, base_url: &str, credential: Option<&str>, auth_style: AuthStyle) -> Self {
        Self::new_with_options(name, base_url, credential, auth_style, true, None, false)
    }

    /// Same as `new`, for an endpoint the operator runs that may legitimately
    /// require no authentication.
    ///
    /// With a credential the behaviour is identical to `new`. Without one, the
    /// request carries no authorization header — never a placeholder, which
    /// would be a credential-shaped value on the wire that no one issued.
    pub fn new_optional_credential(
        name: &str,
        base_url: &str,
        credential: Option<&str>,
        auth_style: AuthStyle,
    ) -> Self {
        Self {
            allows_anonymous: true,
            ..Self::new(name, base_url, credential, auth_style)
        }
    }

    /// Same as `new` but skips the /v1/responses fallback on 404.
    /// Use for providers (e.g. GLM) that only support chat completions.
    pub fn new_no_responses_fallback(
        name: &str,
        base_url: &str,
        credential: Option<&str>,
        auth_style: AuthStyle,
    ) -> Self {
        Self::new_with_options(name, base_url, credential, auth_style, false, None, false)
    }

    /// Create a provider with a custom User-Agent header.
    ///
    /// Some providers (for example Kimi Code) require a specific User-Agent
    /// for request routing and policy enforcement.
    pub fn new_with_user_agent(
        name: &str,
        base_url: &str,
        credential: Option<&str>,
        auth_style: AuthStyle,
        user_agent: &str,
    ) -> Self {
        Self::new_with_options(name, base_url, credential, auth_style, true, Some(user_agent), false)
    }

    /// For providers that do not support `role: system` (e.g. MiniMax).
    /// System prompt content is prepended to the first user message instead.
    pub fn new_merge_system_into_user(
        name: &str,
        base_url: &str,
        credential: Option<&str>,
        auth_style: AuthStyle,
    ) -> Self {
        Self::new_with_options(name, base_url, credential, auth_style, false, None, true)
    }

    fn new_with_options(
        name: &str,
        base_url: &str,
        credential: Option<&str>,
        auth_style: AuthStyle,
        supports_responses_fallback: bool,
        user_agent: Option<&str>,
        merge_system_into_user: bool,
    ) -> Self {
        Self {
            name: name.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            credential: credential.map(ToString::to_string),
            auth_header: auth_style,
            supports_responses_fallback,
            user_agent: user_agent.map(ToString::to_string),
            merge_system_into_user,
            allows_anonymous: false,
            tool_schema_dialects: Vec::new(),
            tool_call_dialects: Vec::new(),
            declaration_provider: None,
            configured_label: None,
        }
    }

    /// Bind catalog parameter lookup to the configured factory identity.
    #[must_use]
    pub(crate) fn with_parameter_provider(mut self, provider: &str) -> Self {
        self.configured_label = Some(super::redact_endpoint_userinfo(provider.trim()).into_owned());
        self
    }

    /// Attach every `[[providers.tool_dialects]]` declaration that applies to
    /// `provider`, the label this endpoint was constructed from.
    ///
    /// Declarations are matched by provider identity (see
    /// [`ProviderRuntimeOptions::declared_tool_schema_dialects`](super::ProviderRuntimeOptions::declared_tool_schema_dialects)),
    /// and a refusal names `provider` as the entry to edit.
    #[must_use]
    pub(crate) fn with_declared_tool_dialects(self, provider: &str, options: &super::ProviderRuntimeOptions) -> Self {
        // A `custom:` URL may carry `user:password@`, and this label is
        // printed into refusals and hints.
        let label = super::redact_endpoint_userinfo(provider.trim()).into_owned();
        let schema = options.declared_tool_schema_dialects(provider);
        let call = options.declared_tool_call_dialects(provider);
        if schema.is_empty() && call.is_empty() {
            return Self {
                configured_label: Some(label),
                ..self
            };
        }
        Self {
            declaration_provider: Some(label.clone()),
            configured_label: Some(label),
            ..self.with_tool_schema_dialects(schema).with_tool_call_dialects(call)
        }
    }

    /// Attach the operator's declaration of what this endpoint accepts in a
    /// tool schema.
    ///
    /// Each entry is `(model, dialect)`, with `None` standing for every model
    /// of this provider. The factory fills it from `[[providers.tool_dialects]]`
    /// through [`with_declared_tool_dialects`](Self::with_declared_tool_dialects).
    #[must_use]
    pub fn with_tool_schema_dialects(
        mut self,
        declarations: Vec<(Option<String>, crate::tools::ToolSchemaDialect)>,
    ) -> Self {
        self.tool_schema_dialects = declarations;
        self
    }

    /// Attach the operator's declaration of which textual tool-call grammar
    /// this endpoint emits.
    ///
    /// Each entry is `(model, dialect)`, with `None` standing for every model
    /// of this provider. Undeclared endpoints read only native `tool_calls`.
    #[must_use]
    pub fn with_tool_call_dialects(mut self, declarations: Vec<(Option<String>, super::ToolCallDialect)>) -> Self {
        self.tool_call_dialects = declarations;
        self
    }

    /// Which textual tool-call grammar this endpoint was declared to emit.
    ///
    /// The response parser has no model to key on, and every declared model of
    /// this endpoint could have produced the response, so a declaration on any
    /// of them enables the grammar for this provider.
    fn tool_call_dialect(&self) -> super::ToolCallDialect {
        if self
            .tool_call_dialects
            .iter()
            .any(|(_, dialect)| *dialect == super::ToolCallDialect::XmlFunctionTags)
        {
            super::ToolCallDialect::XmlFunctionTags
        } else {
            super::ToolCallDialect::Native
        }
    }

    /// The declaration that applies to `model`, most specific first, or `None`
    /// when nothing was declared for it.
    fn tool_schema_declaration_for_model(
        &self,
        model: &str,
    ) -> Option<&(Option<String>, crate::tools::ToolSchemaDialect)> {
        self.tool_schema_dialects
            .iter()
            .find(|(declared, _)| declared.as_deref().is_some_and(|name| name.eq_ignore_ascii_case(model)))
            .or_else(|| {
                self.tool_schema_dialects
                    .iter()
                    .find(|(declared, _)| declared.is_none())
            })
    }

    /// The dialect declared for `model`, most specific declaration first.
    fn tool_schema_dialect_for_model(&self, model: &str) -> crate::tools::ToolSchemaDialect {
        self.tool_schema_declaration_for_model(model)
            .map_or(crate::tools::ToolSchemaDialect::Raw, |(_, dialect)| *dialect)
    }

    /// Where the dialect a refusal was made under was declared, in the words
    /// of the line the operator would edit.
    fn tool_schema_dialect_source(&self, model: &str) -> String {
        let Some(provider) = self.declaration_provider.as_deref() else {
            return format!(
                "the tool-schema dialect declared for provider '{}' model '{model}'",
                self.name
            );
        };
        let scope = match self.tool_schema_declaration_for_model(model) {
            Some((Some(declared), _)) => format!("model '{declared}'"),
            _ => "every model".to_string(),
        };
        format!("`[[providers.tool_dialects]]` (provider '{provider}', {scope})")
    }

    /// The label a diagnostic names this endpoint by: the configured provider
    /// (redacted) when the factory built it, else its display name.
    fn diagnostic_label(&self) -> &str {
        self.declaration_provider
            .as_deref()
            .or(self.configured_label.as_deref())
            .unwrap_or(&self.name)
    }

    /// Whether an upstream error reads as a rejection of a root combinator in
    /// a tool schema: a 400/422 whose body names `allOf`, `anyOf` or `oneOf`
    /// next to "tool" or "schema".
    fn names_a_root_combinator_rejection(status: reqwest::StatusCode, body: &str) -> bool {
        if !matches!(
            status,
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY
        ) {
            return false;
        }
        let lower = body.to_ascii_lowercase();
        ["allof", "anyof", "oneof"]
            .iter()
            .any(|keyword| lower.contains(keyword))
            && (lower.contains("tool") || lower.contains("schema"))
    }

    /// The sentence naming the declaration that would stop `model` from being
    /// sent a root combinator, or `None` when one is already declared.
    ///
    /// Only an undeclared (`raw`) model can have sent a root combinator. The
    /// hint changes no behaviour and infers no capability: the operator
    /// decides whether to declare.
    fn root_combinator_declaration_hint(&self, model: &str) -> Option<String> {
        (self.tool_schema_dialect_for_model(model) == crate::tools::ToolSchemaDialect::Raw).then(|| {
            let (provider, credentialed) = super::pasteable_provider_label(self.diagnostic_label());
            format!(
                " If this endpoint rejects a root `allOf`, `anyOf` or `oneOf` in tool schemas, declare \
                 `[[providers.tool_dialects]]` provider = \"{provider}\" model = \"{model}\" \
                 tool_schema = \"no_root_combinators\" ({}).",
                if credentialed {
                    "write the provider exactly as in your configuration, replacing <user>:<password> with the \
                     credentials it carries"
                } else {
                    "spell the provider as in your configuration"
                }
            )
        })
    }

    /// Lower every published tool schema to what `model` accepts, or refuse.
    ///
    /// This replaces forwarding `tool.parameters` verbatim. A tool whose root
    /// combinator has no lossless lowering is named in the returned error and
    /// the request is not made: the alternative — quietly omitting the tool —
    /// would leave the model unable to do the work and nothing in the logs to
    /// say why.
    fn convert_tool_specs(
        &self,
        model: &str,
        tools: Option<&[crate::tools::ToolSpec]>,
    ) -> anyhow::Result<Option<Vec<serde_json::Value>>> {
        let Some(items) = tools else {
            return Ok(None);
        };
        let dialect = self.tool_schema_dialect_for_model(model);
        let lowered = dialect
            .lower_all(items.iter().map(|tool| (tool.name.as_str(), &tool.parameters)))
            .map_err(|refused| {
                let source = self.tool_schema_dialect_source(model);
                super::ToolSchemaRefusalError {
                    provider: self.name.clone(),
                    refusals: crate::tools::ToolSchemaRefusals {
                        dialect,
                        refusals: refused
                            .into_iter()
                            .map(|refusal| refusal.with_descriptor_source(source.clone()))
                            .collect(),
                        provider: self.diagnostic_label().to_string(),
                        model: model.to_string(),
                        descriptor_source: source,
                    },
                }
            })?;
        let converted = items
            .iter()
            .zip(lowered)
            .map(|(tool, parameters)| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": parameters,
                    }
                })
            })
            .collect();
        Ok(Some(converted))
    }

    /// Collect all `system` role messages, concatenate their content,
    /// and prepend to the first `user` message. Drop all system messages.
    /// Used for providers (e.g. MiniMax) that reject `role: system`.
    fn flatten_system_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
        let system_content: String = messages
            .iter()
            .filter(|m| m.role == "system")
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        if system_content.is_empty() {
            return messages.to_vec();
        }

        let mut result: Vec<ChatMessage> = messages.iter().filter(|m| m.role != "system").cloned().collect();

        if let Some(first_user) = result.iter_mut().find(|m| m.role == "user") {
            first_user.content = format!("{system_content}\n\n{}", first_user.content);
        } else {
            // No user message found: insert a synthetic user message with system content
            result.insert(0, ChatMessage::user(&system_content));
        }

        result
    }

    fn http_client(&self) -> Client {
        self.build_http_client(self.http_client_builder())
    }

    fn streaming_http_client(&self) -> Client {
        self.build_http_client(self.streaming_http_client_builder())
    }

    fn build_http_client(&self, builder: reqwest::ClientBuilder) -> Client {
        if let Some(ua) = self.user_agent.as_deref() {
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(ua) {
                headers.insert(USER_AGENT, value);
            }

            let builder = builder.default_headers(headers);

            return builder
                .build()
                .map_err(|error| {
                    tracing::error!("proxy build failed for provider.compatible (user-agent), using direct: {error}");
                    error
                })
                .unwrap_or_else(|_| Client::new());
        }

        builder
            .build()
            .map_err(|error| {
                tracing::error!("proxy build failed for provider.compatible, using direct: {error}");
                error
            })
            .unwrap_or_else(|_| Client::new())
    }

    /// Build the full URL for chat completions, detecting if base_url already includes the path.
    /// This allows custom providers with non-standard endpoints (e.g., VolcEngine ARK uses
    /// `/api/coding/v3/chat/completions` instead of `/v1/chat/completions`).
    fn chat_completions_url(&self) -> String {
        let has_full_endpoint = reqwest::Url::parse(&self.base_url)
            .map(|url| url.path().trim_end_matches('/').ends_with("/chat/completions"))
            .unwrap_or_else(|_| self.base_url.trim_end_matches('/').ends_with("/chat/completions"));

        if has_full_endpoint {
            self.base_url.clone()
        } else {
            format!("{}/chat/completions", self.base_url)
        }
    }

    fn path_ends_with(&self, suffix: &str) -> bool {
        if let Ok(url) = reqwest::Url::parse(&self.base_url) {
            return url.path().trim_end_matches('/').ends_with(suffix);
        }

        self.base_url.trim_end_matches('/').ends_with(suffix)
    }

    fn has_explicit_api_path(&self) -> bool {
        let Ok(url) = reqwest::Url::parse(&self.base_url) else {
            return false;
        };

        let path = url.path().trim_end_matches('/');
        !path.is_empty() && path != "/"
    }

    /// Build the full URL for responses API, detecting if base_url already includes the path.
    fn responses_url(&self) -> String {
        if self.path_ends_with("/responses") {
            return self.base_url.clone();
        }

        let normalized_base = self.base_url.trim_end_matches('/');

        // If chat endpoint is explicitly configured, derive sibling responses endpoint.
        if let Some(prefix) = normalized_base.strip_suffix("/chat/completions") {
            return format!("{prefix}/responses");
        }

        // If an explicit API path already exists (e.g. /v1, /openai, /api/coding/v3),
        // append responses directly to avoid duplicate /v1 segments.
        if self.has_explicit_api_path() {
            format!("{normalized_base}/responses")
        } else {
            format!("{normalized_base}/v1/responses")
        }
    }

    #[cfg(test)]
    fn tool_specs_to_openai_format(tools: &[crate::tools::ToolSpec]) -> Vec<serde_json::Value> {
        tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters
                    }
                })
            })
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct ApiChatRequest {
    model: String,
    messages: Vec<Message>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamUsageOptions>,
}

#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ApiChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<OpenAiCompatibleUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

/// Remove `<think>...</think>` blocks from model output.
/// Some reasoning models (e.g. MiniMax) embed their chain-of-thought inline
/// in the `content` field rather than a separate `reasoning_content` field.
/// The resulting `<think>` tags must be stripped before returning to the user.
fn strip_think_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        if let Some(start) = rest.find("<think>") {
            result.push_str(&rest[..start]);
            if let Some(end) = rest[start..].find("</think>") {
                rest = &rest[start + end + "</think>".len()..];
            } else {
                // Unclosed tag: drop the rest to avoid leaking partial reasoning.
                break;
            }
        } else {
            result.push_str(rest);
            break;
        }
    }
    result.trim().to_string()
}

/// Extract the concatenated text inside `<think>...</think>` blocks. Inverse of
/// [`strip_think_tags`]: returns `None` when no closed `<think>` block exists.
/// Used to route inline reasoning (MiniMax-style) to the `reasoning_content`
/// field instead of silently dropping it.
fn extract_think_tags(s: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("<think>") {
        let after_open = &rest[start + "<think>".len()..];
        if let Some(end) = after_open.find("</think>") {
            parts.push(&after_open[..end]);
            rest = &after_open[end + "</think>".len()..];
        } else {
            // Unclosed tag — drop trailing partial reasoning.
            break;
        }
    }
    if parts.is_empty() {
        None
    } else {
        let joined = parts
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if joined.is_empty() { None } else { Some(joined) }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning/thinking models (e.g. Qwen3, GLM-4) may return their output
    /// in `reasoning_content` instead of `content`. Used as automatic fallback.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

impl ResponseMessage {
    /// Visible text content only. `<think>...</think>` inline blocks (MiniMax
    /// and similar) are stripped — they are not user-facing output. The
    /// dedicated `reasoning_content` field is NOT merged in; it is exposed
    /// separately via [`Self::reasoning`].
    fn visible_content(&self) -> String {
        self.content
            .as_ref()
            .filter(|c| !c.is_empty())
            .map(|content| strip_think_tags(content))
            .filter(|c| !c.is_empty())
            .unwrap_or_default()
    }

    /// Visible content as Option (`None` when truly empty). Same semantics as
    /// [`Self::visible_content`] — reasoning is not merged in.
    fn visible_content_optional(&self) -> Option<String> {
        self.content
            .as_ref()
            .filter(|c| !c.is_empty())
            .map(|content| strip_think_tags(content))
            .filter(|c| !c.is_empty())
    }

    /// Reasoning/thinking content.
    ///
    /// Combines:
    ///   1. Dedicated `reasoning_content` field (Qwen3, GLM-4 family).
    ///   2. `<think>...</think>` blocks lifted from inline `content` (MiniMax).
    ///
    /// Returns `None` when neither source yields non-empty reasoning.
    fn reasoning(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(inline) = self.content.as_ref().and_then(|c| extract_think_tags(c)) {
            let trimmed = inline.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
        if let Some(rc) = self.reasoning_content.as_ref() {
            let trimmed = rc.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
        if parts.is_empty() { None } else { Some(parts.join("\n")) }
    }

    /// Legacy fallback for paths that must return a single string and cannot
    /// surface reasoning separately (`chat_with_system`,
    /// `chat_with_history`). Prefers visible content; logs and drops reasoning
    /// if only reasoning is available.
    fn fallback_text_for_legacy_api(&self) -> String {
        let visible = self.visible_content();
        if !visible.is_empty() {
            return visible;
        }
        if let Some(r) = self.reasoning() {
            tracing::warn!(
                reasoning_chars = r.chars().count(),
                "Provider returned only reasoning_content; legacy String-returning API drops reasoning"
            );
        }
        String::new()
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<Function>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Function {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Serialize)]
struct NativeChatRequest {
    model: String,
    messages: Vec<NativeMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamUsageOptions>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct OpenAiCompatibleUsage {
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    prompt_tokens: Option<u32>,
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    completion_tokens: Option<u32>,
    #[serde(default, deserialize_with = "optional_u32_from_any")]
    total_tokens: Option<u32>,
}

impl OpenAiCompatibleUsage {
    fn into_reported(self) -> Option<TokenUsage> {
        let total = self.total_tokens.or_else(|| {
            self.prompt_tokens
                .zip(self.completion_tokens)
                .map(|(p, c)| p.saturating_add(c))
        });
        if total.is_some() || self.prompt_tokens.zip(self.completion_tokens).is_some() {
            Some(TokenUsage::reported(self.prompt_tokens, self.completion_tokens, total))
        } else {
            None
        }
    }
}

fn optional_u32_from_any<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok()))
}

#[derive(Debug, Clone, Copy, Serialize)]
struct StreamUsageOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct NativeMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<NativeMessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum NativeMessageContent {
    Text(String),
    Parts(Vec<NativeContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum NativeContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: NativeImageUrl },
}

#[derive(Debug, Serialize)]
struct NativeImageUrl {
    url: String,
}

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<ResponsesInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ResponsesInput {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    output: Vec<ResponsesOutput>,
    #[serde(default)]
    output_text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesOutput {
    #[serde(default)]
    content: Vec<ResponsesContent>,
}

#[derive(Debug, Deserialize)]
struct ResponsesContent {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

// ---------------------------------------------------------------
// Streaming support (SSE parser)
// ---------------------------------------------------------------

/// Server-Sent Event stream chunk for OpenAI-compatible streaming.
#[derive(Debug, Deserialize)]
struct StreamChunkResponse {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<OpenAiCompatibleUsage>,
    /// A failure reported inside a stream that already answered 200.
    #[serde(default)]
    error: Option<StreamErrorObject>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamErrorObject {
    #[serde(default)]
    message: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    code: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// Per-choice delta. Mirrors the OpenAI Chat Completions streaming format used
/// by Groq, Mistral, Cohere (via `/compatibility` endpoint), MiniMax, GLM, etc.
#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    /// Visible content token fragment.
    #[serde(default)]
    content: Option<String>,
    /// Reasoning/thinking models may stream output via `reasoning_content`.
    #[serde(default)]
    reasoning_content: Option<String>,
    /// **S3 T3-2-D**: incremental tool-call deltas. Each entry is keyed by
    /// `index` and may carry an `id`, a `function.name`, and/or a
    /// `function.arguments` fragment to be concatenated with prior fragments
    /// for the same index.
    #[serde(default)]
    tool_calls: Option<Vec<StreamSseToolCall>>,
}

/// A single tool-call delta within `choices[].delta.tool_calls[]`.
///
/// OpenAI-compatible providers stream the leading delta with `id` + `type` +
/// `function.name` and then a sequence of deltas carrying only
/// `function.arguments` fragments keyed by the same `index`.
#[derive(Debug, Deserialize)]
struct StreamSseToolCall {
    /// Concurrent-call slot (0..N).
    #[serde(default)]
    index: usize,
    /// Provider-assigned call id. Usually present only on the first delta.
    #[serde(default)]
    id: Option<String>,
    /// Function payload (`name` + incremental `arguments`).
    #[serde(default)]
    function: Option<StreamSseFunction>,
}

/// Function payload inside a streaming tool-call delta.
#[derive(Debug, Deserialize)]
struct StreamSseFunction {
    /// Tool name (only present on the leading delta).
    #[serde(default)]
    name: Option<String>,
    /// Incremental fragment of the JSON arguments string.
    #[serde(default)]
    arguments: Option<String>,
}

/// Outcome of parsing a single SSE line from an OpenAI-compatible provider.
#[derive(Debug, Default)]
struct SseEvent {
    /// Visible content delta (`choices[].delta.content`).
    content: Option<String>,
    /// Reasoning/thinking delta (`choices[].delta.reasoning_content`).
    /// Streamed separately so the consumer can route it independently.
    reasoning: Option<String>,
    /// Incremental tool-call deltas (`choices[].delta.tool_calls[]`).
    /// Empty for ordinary content/reasoning chunks.
    tool_call_deltas: Vec<StreamSseToolCall>,
    /// `choices[].finish_reason`; `Some("tool_calls")` is the signal to flush
    /// the accumulated tool-call buffer.
    finish_reason: Option<String>,
    /// Provider-reported usage, usually delivered on a final usage-only chunk.
    usage: Option<TokenUsage>,
    /// `true` when the chunk was the `[DONE]` sentinel.
    done_sentinel: bool,
}

impl SseEvent {
    /// `true` when the event carries no payload at all (no content, no
    /// reasoning, no tool deltas, no finish reason and not the DONE sentinel).
    const fn is_noop(&self) -> bool {
        self.content.is_none()
            && self.reasoning.is_none()
            && self.tool_call_deltas.is_empty()
            && self.finish_reason.is_none()
            && self.usage.is_none()
            && !self.done_sentinel
    }
}

/// Buffered state for a single in-flight tool call across SSE chunks.
///
/// OpenAI-compatible providers stream `function.arguments` as incremental JSON
/// fragments keyed by `index`. We accumulate them per-slot and emit a
/// terminal `Completed` [`ToolCallChunk`] once `finish_reason == "tool_calls"`
/// arrives. The same buffer also tracks whether the leading "open" Streaming
/// chunk has already been forwarded so we don't re-emit it on subsequent
/// argument fragments.
#[derive(Debug, Default, Clone)]
struct ToolCallBuffer {
    /// Provider-assigned call id, defaulted lazily to a UUID when missing.
    id: String,
    /// Function name (must match a registered `ToolSpec.name`).
    name: String,
    /// Concatenation of every `function.arguments` fragment observed so far.
    arguments: String,
    /// `true` once the leading `Streaming` chunk (open marker) has been
    /// forwarded — prevents duplicate empty-delta chunks when a provider
    /// emits `id` + `name` across multiple frames.
    opened: bool,
}

/// Parse SSE (Server-Sent Events) stream from OpenAI-compatible providers.
/// Handles the `data: {...}` format and `[DONE]` sentinel.
///
/// Returns `None` on blank/comment lines (no-op). Returns `Some(SseEvent)`
/// for every `data:` payload, including the `[DONE]` sentinel and chunks
/// whose only meaningful field is `finish_reason` — the caller needs those
/// to flush the tool-call buffer.
fn parse_sse_line(line: &str) -> StreamResult<Option<SseEvent>> {
    let line = line.trim();

    // Skip empty lines and comments
    if line.is_empty() || line.starts_with(':') {
        return Ok(None);
    }

    // SSE format: "data: {...}"
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(None);
    };
    let data = data.trim();

    // Check for [DONE] sentinel
    if data == "[DONE]" {
        return Ok(Some(SseEvent {
            done_sentinel: true,
            ..SseEvent::default()
        }));
    }

    // Parse JSON delta
    let chunk: StreamChunkResponse = serde_json::from_str(data).map_err(StreamError::Json)?;
    if let Some(error) = chunk.error {
        // An in-stream error ends the turn. Parsed as an empty chunk, it used
        // to be skipped, and the end-of-stream flush then ran a half call.
        return Err(super::reliable::in_stream_error_object(
            "OpenAI-compatible",
            error.kind,
            error.code,
            error.message,
        ));
    }

    let usage = chunk.usage.and_then(OpenAiCompatibleUsage::into_reported);
    let Some(choice) = chunk.choices.into_iter().next() else {
        return Ok(usage.map(|usage| SseEvent {
            usage: Some(usage),
            ..SseEvent::default()
        }));
    };

    let content = choice.delta.content.filter(|c| !c.is_empty());
    let reasoning = choice.delta.reasoning_content.filter(|r| !r.is_empty());
    let tool_call_deltas = choice.delta.tool_calls.unwrap_or_default();
    let finish_reason = choice.finish_reason;

    let event = SseEvent {
        content,
        reasoning,
        tool_call_deltas,
        finish_reason,
        usage,
        done_sentinel: false,
    };

    if event.is_noop() {
        return Ok(None);
    }

    Ok(Some(event))
}

/// Apply a vec of tool_call deltas onto the index-keyed buffer.
///
/// **S3 T3-2-D**: in addition to accumulating fragments into the buffer, this
/// returns a vec of `Streaming`-status [`ToolCallChunk`]s in observation order
/// so the caller can emit them incrementally. The streaming protocol mirrors
/// the OpenAI provider's behaviour (`T3-2-B`):
///
/// - The first delta for a given index carrying `id`/`name` emits a leading
///   `Streaming` chunk with `arguments_delta = Some("")` so the driver sees
///   the call has been opened, even before any argument fragment arrives.
/// - Each subsequent delta carrying a `function.arguments` fragment emits a
///   `Streaming` chunk with `arguments_delta = Some(fragment)`.
///
/// `id` is sourced from whichever delta first carried it; when no provider id
/// ever arrives, a UUID is minted lazily on the flush path and recorded back
/// into the buffer so the matching `Completed` chunk shares the same id.
///
/// The buffer is indexed by the server-supplied `index`, so an index above
/// [`MAX_STREAMED_TOOL_CALL_INDEX`] is refused as malformed SSE instead of
/// growing the buffer to whatever the upstream names.
fn apply_tool_call_deltas(
    buf: &mut Vec<ToolCallBuffer>,
    deltas: Vec<StreamSseToolCall>,
) -> StreamResult<Vec<ToolCallChunk>> {
    let mut streaming = Vec::with_capacity(deltas.len());

    for delta in deltas {
        let slot = delta.index;
        if slot > MAX_STREAMED_TOOL_CALL_INDEX {
            return Err(super::reliable::tool_call_index_over_limit(
                "OpenAI-compatible",
                slot,
                MAX_STREAMED_TOOL_CALL_INDEX,
            ));
        }
        while buf.len() <= slot {
            buf.push(ToolCallBuffer::default());
        }
        let Some(entry) = buf.get_mut(slot) else {
            continue;
        };

        let mut saw_open_signal = false;
        if let Some(id) = delta.id {
            if !id.is_empty() {
                entry.id = id;
                saw_open_signal = true;
            }
        }
        let mut arg_fragment: Option<String> = None;
        if let Some(func) = delta.function {
            if let Some(name) = func.name {
                if !name.is_empty() {
                    entry.name = name;
                    saw_open_signal = true;
                }
            }
            if let Some(args) = func.arguments {
                if !args.is_empty() {
                    entry.arguments.push_str(&args);
                    arg_fragment = Some(args);
                }
            }
        }

        // Defensive: skip deltas that carried neither identifying info nor an
        // argument fragment (should not happen on well-formed OpenAI streams).
        if !saw_open_signal && arg_fragment.is_none() {
            continue;
        }

        // Hold the streaming chunk until at least the function name is known —
        // consumers key on `name` to route the call.
        if entry.name.is_empty() {
            continue;
        }

        if entry.id.is_empty() {
            entry.id = uuid::Uuid::new_v4().to_string();
        }

        // Emit a leading "open" chunk exactly once per index, then one chunk
        // per non-empty argument fragment.
        if !entry.opened {
            entry.opened = true;
            streaming.push(ToolCallChunk {
                id: entry.id.clone(),
                name: entry.name.clone(),
                args: String::new(),
                index: slot,
                arguments_delta: Some(String::new()),
                status: ToolCallChunkStatus::Streaming,
            });
        }

        if let Some(fragment) = arg_fragment {
            streaming.push(ToolCallChunk {
                id: entry.id.clone(),
                name: entry.name.clone(),
                args: String::new(),
                index: slot,
                arguments_delta: Some(fragment),
                status: ToolCallChunkStatus::Streaming,
            });
        }
    }

    Ok(streaming)
}

/// Flush the tool_call buffer into terminal `Completed` chunks.
///
/// **S3 T3-2-D**: invoked when `finish_reason == "tool_calls"` arrives. The
/// driver-side aggregator treats `Completed.args` as authoritative, so this
/// must always return the full concatenated argument JSON (falling back to
/// `"{}"` when no fragments were observed).
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
                arguments_delta: None,
                status: ToolCallChunkStatus::Completed,
            }
        })
        .collect()
}

/// Convert SSE byte stream to text chunks.
///
/// **S3 T3-2-D**: in addition to forwarding visible-content and reasoning
/// deltas, this maintains an index-keyed [`ToolCallBuffer`] across SSE frames
/// so each `delta.tool_calls[]` fragment is surfaced as an incremental
/// `Streaming` [`ToolCallChunk`]. When `finish_reason == "tool_calls"` is
/// observed, the buffer is flushed into a single terminal `tool_call_chunk`
/// carrying one `Completed` entry per accumulated index.
pub(crate) fn sse_bytes_to_chunks(
    response: reqwest::Response,
    count_tokens: bool,
) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
    // Create a channel to send chunks
    let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);

    crate::providers::compatible::spawn_until_closed(tx.clone(), async move {
        // Buffer for incomplete lines
        let mut buffer = String::new();
        // Byte-level decoder: HTTP chunk boundaries do not respect UTF-8
        // character boundaries, so a split character is carried over instead of
        // aborting the stream. `buffer` above stays the SSE *line* accumulator.
        let mut decoder = SseTextDecoder::new(STREAM_DECODER_LABEL);
        let mut saw_sse_event = false;
        let mut saw_non_sse_line = false;
        let mut saw_usage = false;
        let mut completion_chars: usize = 0;
        // Per-index tool-call accumulator. Carried across all SSE frames so
        // argument fragments split across multiple `data:` lines aggregate
        // correctly before flush.
        let mut tool_buf: Vec<ToolCallBuffer> = Vec::new();
        let mut saw_done = false;

        // Get response body as bytes stream
        match response.error_for_status_ref() {
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(Err(StreamError::Http(e))).await;
                return;
            }
        }

        let mut bytes_stream = response.bytes_stream();

        loop {
            // Stop reading as soon as nobody consumes the stream (the turn
            // ended, for example by the idle guard). The streaming client has
            // no whole-request deadline, so otherwise this task and its
            // connection would wait on a silent server indefinitely.
            let next = tokio::select! {
                () = tx.closed() => return,
                next = bytes_stream.next() => next,
            };
            let Some(item) = next else {
                break;
            };
            match item {
                Ok(bytes) => {
                    // Decode bytes into the line buffer, carrying any character
                    // that straddles this chunk and the next.
                    if let Err(e) = decoder.push(&bytes, &mut buffer) {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }

                    // Process complete lines. `drain(..=pos)` already removes the
                    // terminator from `buffer`; the previous re-slice into
                    // `buffer[pos+1..]` was a dangling index relative to the
                    // already-shortened vector and would panic (e.g. `buffer == "\n"`
                    // → `drain` yields `""` and the empty buffer has no `[1..]`).
                    while let Some(pos) = buffer.find('\n') {
                        let line = buffer.drain(..=pos).collect::<String>();
                        let trimmed = line.trim();
                        if !trimmed.is_empty() && !trimmed.starts_with(':') && !trimmed.starts_with("data:") {
                            saw_non_sse_line = true;
                        }

                        match parse_sse_line(&line) {
                            Ok(Some(event)) => {
                                saw_sse_event = true;
                                // `[DONE]` carries no payload; nothing else to emit.
                                if event.done_sentinel {
                                    saw_done = true;
                                    continue;
                                }
                                if let Some(usage) = event.usage {
                                    saw_usage = true;
                                    if tx.send(Ok(StreamChunk::usage(usage))).await.is_err() {
                                        return;
                                    }
                                }
                                // Emit a separate StreamChunk for visible content
                                // and reasoning so consumers can route them
                                // independently. They are never merged.
                                if let Some(content) = event.content {
                                    completion_chars = completion_chars.saturating_add(content.chars().count());
                                    let mut chunk = StreamChunk::delta(content);
                                    if count_tokens {
                                        chunk = chunk.with_token_estimate();
                                    }
                                    if tx.send(Ok(chunk)).await.is_err() {
                                        return; // Receiver dropped
                                    }
                                }
                                if let Some(reasoning) = event.reasoning {
                                    completion_chars = completion_chars.saturating_add(reasoning.chars().count());
                                    let mut chunk = StreamChunk::reasoning_delta(reasoning);
                                    if count_tokens {
                                        chunk = chunk.with_token_estimate();
                                    }
                                    if tx.send(Ok(chunk)).await.is_err() {
                                        return; // Receiver dropped
                                    }
                                }
                                if !event.tool_call_deltas.is_empty() {
                                    let streaming = match apply_tool_call_deltas(&mut tool_buf, event.tool_call_deltas)
                                    {
                                        Ok(streaming) => streaming,
                                        Err(e) => {
                                            let _ = tx.send(Err(e)).await;
                                            return;
                                        }
                                    };
                                    if !streaming.is_empty()
                                        && tx.send(Ok(StreamChunk::tool_call_chunk(streaming))).await.is_err()
                                    {
                                        return; // Receiver dropped
                                    }
                                }
                                if let Some(finish) = event.finish_reason {
                                    // Cut by the output cap or a filter while a
                                    // call is buffered: the call is unfinished,
                                    // even if `[DONE]` follows. Never flush it.
                                    if finish == "content_filter" && !tool_buf.is_empty() {
                                        let _ = tx
                                            .send(Err(super::reliable::content_filter_stop(
                                                "OpenAI-compatible",
                                                "finish_reason content_filter inside a tool call; it was not run",
                                            )))
                                            .await;
                                        return;
                                    }
                                    if finish == "length" && !tool_buf.is_empty() {
                                        let _ = tx
                                            .send(Err(StreamError::Provider(
                                                "OpenAI-compatible stream ended (finish_reason length) inside a tool \
                                                 call; it was not run"
                                                    .to_string(),
                                            )))
                                            .await;
                                        return;
                                    }
                                    // Any other finish with a call that got its
                                    // name but not one argument byte: downstream
                                    // it cannot be told apart from a no-argument
                                    // call and would run as `{}`. Mirrors the
                                    // direct OpenAI rule.
                                    if finish != "tool_calls"
                                        && tool_buf
                                            .iter()
                                            .any(|entry| !entry.name.is_empty() && entry.arguments.trim().is_empty())
                                    {
                                        let _ = tx
                                            .send(Err(StreamError::Provider(format!(
                                                "OpenAI-compatible stream ended (finish_reason {finish}) with an \
                                                 incomplete tool call; it was not run"
                                            ))))
                                            .await;
                                        return;
                                    }
                                    if finish == "tool_calls" && !tool_buf.is_empty() {
                                        let calls = flush_tool_call_buffer(&tool_buf);
                                        tool_buf.clear();
                                        if !calls.is_empty()
                                            && tx.send(Ok(StreamChunk::tool_call_chunk(calls))).await.is_err()
                                        {
                                            return; // Receiver dropped
                                        }
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                let _ = tx.send(Err(e)).await;
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
                    break;
                }
            }
        }

        // A character left half-decoded when the body ended is real truncation.
        if let Err(e) = decoder.finish() {
            let _ = tx.send(Err(e)).await;
            return;
        }

        let trailing = buffer.trim();
        if !trailing.is_empty() {
            let preview = trailing.chars().take(200).collect::<String>();
            let _ = tx
                .send(Err(StreamError::InvalidSse(format!(
                    "OpenAI-compatible SSE stream ended with incomplete trailing data: {preview}"
                ))))
                .await;
            return;
        }

        if !saw_sse_event && saw_non_sse_line {
            let _ = tx
                .send(Err(StreamError::InvalidSse(
                    "OpenAI-compatible streaming response did not contain any SSE data events".to_string(),
                )))
                .await;
            return;
        }

        // A body that ended without `[DONE]` while calls were still buffered
        // was cut, possibly in the middle of their arguments: fail instead of
        // flushing half a call.
        if !saw_done && !tool_buf.is_empty() {
            let _ = tx
                .send(Err(StreamError::InvalidSse(
                    "OpenAI-compatible stream ended before [DONE] with a tool call still open".to_string(),
                )))
                .await;
            return;
        }

        // Calls still buffered here never got a `finish_reason`. A call that
        // got its name but not one argument byte cannot be told apart from a
        // no-argument call downstream and would run as `{}`: fail the stream
        // (parity with the direct OpenAI `[DONE]` rule).
        if tool_buf
            .iter()
            .any(|entry| !entry.name.is_empty() && entry.arguments.trim().is_empty())
        {
            let _ = tx
                .send(Err(StreamError::Provider(
                    "OpenAI-compatible stream ended ([DONE] without finish_reason) with an incomplete tool call; \
                     it was not run"
                        .to_string(),
                )))
                .await;
            return;
        }

        // Defensive flush: if the stream ended without an explicit
        // `finish_reason == "tool_calls"` marker (some providers omit it for
        // single-call turns), surface the buffered tool calls anyway so the
        // driver does not lose them.
        if !tool_buf.is_empty() {
            let calls = flush_tool_call_buffer(&tool_buf);
            if !calls.is_empty() {
                let _ = tx.send(Ok(StreamChunk::tool_call_chunk(calls))).await;
            }
        }

        if !saw_usage && completion_chars > 0 {
            let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
            let usage = accumulator.finish_or_estimate_completion_chars(completion_chars);
            if tx.send(Ok(StreamChunk::usage(usage))).await.is_err() {
                return;
            }
        }

        // Send final chunk
        let _ = tx.send(Ok(StreamChunk::final_chunk())).await;
    });

    // Convert channel receiver to stream
    stream::unfold(rx, |mut rx| async { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
}

/// Spawn a provider streaming task that is dropped as soon as nobody reads
/// its output.
///
/// The task waits in places that are not chunk reads: the HTTP send until the
/// server answers with headers, reading an error body, rate-limit and backoff
/// waits. A streaming client has no whole-request deadline, so a server that
/// accepts a request and never answers held the task and its connection
/// forever once the turn was abandoned. Racing the whole task against
/// `tx.closed()` drops it, and with it the in-flight request and connection,
/// the moment the last receiver is gone. While the consumer still holds the
/// stream `closed()` cannot fire, so no output is lost.
pub(crate) fn spawn_until_closed<F>(tx: tokio::sync::mpsc::Sender<StreamResult<StreamChunk>>, task: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    super::parameter_capture::spawn(async move {
        tokio::select! {
            () = tx.closed() => {}
            () = task => {}
        }
    });
}

/// Forward `chunks` into `tx` until the stream ends or nobody reads `tx`.
///
/// A forwarding task that only notices a dropped receiver on its next send
/// keeps the inner stream, and with it the provider's connection, alive for
/// as long as a silent server stays silent. Racing the next chunk against
/// `tx.closed()` drops the inner stream as soon as the consumer is gone.
pub(crate) async fn forward_stream_until_closed(
    mut chunks: stream::BoxStream<'static, StreamResult<StreamChunk>>,
    tx: &tokio::sync::mpsc::Sender<StreamResult<StreamChunk>>,
) {
    loop {
        let next = tokio::select! {
            () = tx.closed() => return,
            next = chunks.next() => next,
        };
        let Some(chunk) = next else {
            return;
        };
        if tx.send(chunk).await.is_err() {
            return;
        }
    }
}

fn first_nonempty(text: Option<&str>) -> Option<String> {
    text.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_responses_role(role: &str) -> &'static str {
    match role {
        "assistant" => "assistant",
        "tool" => "assistant",
        _ => "user",
    }
}

fn build_responses_prompt(messages: &[ChatMessage]) -> (Option<String>, Vec<ResponsesInput>) {
    let mut instructions_parts = Vec::new();
    let mut input = Vec::new();

    for message in messages {
        if message.content.trim().is_empty() {
            continue;
        }

        if message.role == "system" {
            instructions_parts.push(message.content.clone());
            continue;
        }

        input.push(ResponsesInput {
            role: normalize_responses_role(&message.role).to_string(),
            content: message.content.clone(),
        });
    }

    let instructions = if instructions_parts.is_empty() {
        None
    } else {
        Some(instructions_parts.join("\n\n"))
    };

    (instructions, input)
}

fn extract_responses_text(response: ResponsesResponse) -> Option<String> {
    if let Some(text) = first_nonempty(response.output_text.as_deref()) {
        return Some(text);
    }

    for item in &response.output {
        for content in &item.content {
            if content.kind.as_deref() == Some("output_text") {
                if let Some(text) = first_nonempty(content.text.as_deref()) {
                    return Some(text);
                }
            }
        }
    }

    for item in &response.output {
        for content in &item.content {
            if let Some(text) = first_nonempty(content.text.as_deref()) {
                return Some(text);
            }
        }
    }

    None
}

fn compact_sanitized_body_snippet(body: &str) -> String {
    super::sanitize_api_error(body)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_chat_response_body(provider_name: &str, body: &str) -> anyhow::Result<ApiChatResponse> {
    serde_json::from_str::<ApiChatResponse>(body).map_err(|error| {
        let snippet = compact_sanitized_body_snippet(body);
        anyhow::anyhow!("{provider_name} API returned an unexpected chat-completions payload: {error}; body={snippet}")
    })
}

fn parse_responses_response_body(provider_name: &str, body: &str) -> anyhow::Result<ResponsesResponse> {
    serde_json::from_str::<ResponsesResponse>(body).map_err(|error| {
        let snippet = compact_sanitized_body_snippet(body);
        anyhow::anyhow!("{provider_name} Responses API returned an unexpected payload: {error}; body={snippet}")
    })
}

impl OpenAiCompatibleProvider {
    /// The credential to present on an outbound request.
    ///
    /// `Ok(None)` means the endpoint was configured without one and accepts
    /// anonymous requests. Every named provider still refuses here, before any
    /// socket is opened.
    fn outbound_credential(&self) -> anyhow::Result<Option<&str>> {
        match self.credential.as_deref() {
            Some(credential) => Ok(Some(credential)),
            None if self.allows_anonymous => Ok(None),
            None => anyhow::bail!(
                "{} API key not set. Set `api_key` in config.toml or run `prx auth paste-token --provider <provider>`.",
                self.name
            ),
        }
    }

    /// The streaming variant of [`Self::outbound_credential`]. The stream
    /// surfaces report this condition as a `StreamError::Provider` and own the
    /// credential, because the request is built inside a spawned task.
    fn outbound_credential_for_stream(&self) -> Result<Option<String>, String> {
        match self.credential.as_deref() {
            Some(credential) => Ok(Some(credential.to_string())),
            None if self.allows_anonymous => Ok(None),
            None => Err(format!("{} API key not set", self.name)),
        }
    }

    fn apply_auth_header(&self, req: reqwest::RequestBuilder, credential: Option<&str>) -> reqwest::RequestBuilder {
        Self::apply_auth_header_with_style(req, &self.auth_header, credential)
    }

    /// Apply the configured auth style, or nothing at all when the endpoint is
    /// running without a credential.
    fn apply_auth_header_with_style(
        req: reqwest::RequestBuilder,
        auth_header: &AuthStyle,
        credential: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let Some(credential) = credential else {
            return req;
        };
        match auth_header {
            AuthStyle::Bearer => req.header("Authorization", format!("Bearer {credential}")),
            AuthStyle::XApiKey => req.header("x-api-key", credential),
            AuthStyle::Custom(header) => req.header(header, credential),
        }
    }

    async fn chat_via_responses(
        &self,
        credential: Option<&str>,
        messages: &[ChatMessage],
        model: &str,
    ) -> anyhow::Result<String> {
        let (instructions, input) = build_responses_prompt(messages);
        if input.is_empty() {
            anyhow::bail!(
                "{} Responses API fallback requires at least one non-system message",
                self.name
            );
        }

        let request = ResponsesRequest {
            model: model.to_string(),
            input,
            instructions,
            stream: Some(false),
        };

        let url = self.responses_url();

        let response = super::attempt_accounting::send_json(
            self.apply_auth_header(self.http_client().post(&url), credential),
            model,
            &url,
            credential,
            &super::parameter_wire::request(
                self.configured_label.as_deref().unwrap_or(&self.name),
                super::parameter_wire::Format::Responses,
                &request.model,
                &request,
            ),
            super::attempt_accounting::PhysicalKind::Chat,
        )
        .await?;

        if !response.status().is_success() {
            let error = super::sanitize_api_error(&response.error_body().await);
            anyhow::bail!("{} Responses API error: {error}", self.name);
        }

        let body = response.text().await?;
        let responses = parse_responses_response_body(&self.name, &body)?;

        extract_responses_text(responses).ok_or_else(|| anyhow::anyhow!("No response from {} Responses API", self.name))
    }

    fn parse_assistant_tool_calls(value: &serde_json::Value) -> Option<Vec<ToolCall>> {
        let tool_calls_value = value.get("tool_calls")?;

        if let Ok(parsed_calls) = serde_json::from_value::<Vec<ToolCall>>(tool_calls_value.clone()) {
            let tool_calls = parsed_calls
                .into_iter()
                .filter_map(|tc| {
                    let function = tc.function?;
                    let name = function.name?;
                    let arguments = function.arguments.unwrap_or_else(|| "{}".to_string());
                    Some(ToolCall {
                        id: tc.id,
                        kind: tc.kind.or_else(|| Some("function".to_string())),
                        function: Some(Function {
                            name: Some(name),
                            arguments: Some(arguments),
                        }),
                    })
                })
                .collect::<Vec<_>>();

            if !tool_calls.is_empty() {
                return Some(tool_calls);
            }
        }

        serde_json::from_value::<Vec<ProviderToolCall>>(tool_calls_value.clone())
            .ok()
            .map(|parsed_calls| {
                parsed_calls
                    .into_iter()
                    .map(|tc| ToolCall {
                        id: Some(tc.id),
                        kind: Some("function".to_string()),
                        function: Some(Function {
                            name: Some(tc.name),
                            arguments: Some(tc.arguments),
                        }),
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|tool_calls| !tool_calls.is_empty())
    }

    fn convert_messages_for_native(messages: &[ChatMessage]) -> Vec<NativeMessage> {
        messages
            .iter()
            .map(|message| {
                if message.role == "assistant" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.content) {
                        if let Some(tool_calls) = Self::parse_assistant_tool_calls(&value) {
                            let content = value
                                .get("content")
                                .and_then(serde_json::Value::as_str)
                                .map(|content| NativeMessageContent::Text(content.to_string()));

                            // Thinking-mode providers (e.g. Kimi Code) require
                            // `reasoning_content` on every assistant message that
                            // carries tool_calls. Preserve the actual value from
                            // history so it can be re-sent to the provider.
                            let reasoning_content = value
                                .get("reasoning_content")
                                .and_then(serde_json::Value::as_str)
                                .map(|s| s.to_string());

                            return NativeMessage {
                                role: "assistant".to_string(),
                                content,
                                reasoning_content,
                                tool_call_id: None,
                                tool_calls: Some(tool_calls),
                            };
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
                            .map(|content| NativeMessageContent::Text(content.to_string()))
                            .or_else(|| Some(NativeMessageContent::Text(message.content.clone())));

                        return NativeMessage {
                            role: "tool".to_string(),
                            content,
                            reasoning_content: None,
                            tool_call_id,
                            tool_calls: None,
                        };
                    }
                }

                if message.role == "user" {
                    let content = Self::convert_native_user_content(&message.content);
                    return NativeMessage {
                        role: "user".to_string(),
                        content: Some(content),
                        reasoning_content: None,
                        tool_call_id: None,
                        tool_calls: None,
                    };
                }

                NativeMessage {
                    role: message.role.clone(),
                    content: Some(NativeMessageContent::Text(message.content.clone())),
                    reasoning_content: None,
                    tool_call_id: None,
                    tool_calls: None,
                }
            })
            .collect()
    }

    fn convert_native_user_content(content: &str) -> NativeMessageContent {
        let (cleaned_text, image_refs) = multimodal::parse_image_markers(content);
        if image_refs.is_empty() {
            return NativeMessageContent::Text(content.to_string());
        }

        let mut parts = Vec::new();
        let trimmed_text = cleaned_text.trim();
        if !trimmed_text.is_empty() {
            parts.push(NativeContentPart::Text {
                text: trimmed_text.to_string(),
            });
        }

        for image_ref in image_refs {
            parts.push(NativeContentPart::ImageUrl {
                image_url: NativeImageUrl { url: image_ref },
            });
        }

        if parts.is_empty() {
            NativeMessageContent::Text(content.to_string())
        } else {
            NativeMessageContent::Parts(parts)
        }
    }

    fn with_prompt_guided_tool_instructions(
        messages: &[ChatMessage],
        tools: Option<&[crate::tools::ToolSpec]>,
    ) -> Vec<ChatMessage> {
        let Some(tools) = tools else {
            return messages.to_vec();
        };

        if tools.is_empty() {
            return messages.to_vec();
        }

        let instructions = crate::providers::traits::build_tool_instructions_text(tools);
        let mut modified_messages = messages.to_vec();

        if let Some(system_message) = modified_messages.iter_mut().find(|m| m.role == "system") {
            if !system_message.content.is_empty() {
                system_message.content.push_str("\n\n");
            }
            system_message.content.push_str(&instructions);
        } else {
            modified_messages.insert(0, ChatMessage::system(instructions));
        }

        modified_messages
    }

    /// Read the tool calls out of one response.
    ///
    /// The native `tool_calls` array is always read. The textual
    /// `<function=NAME>…</function>` grammar is read only where an operator
    /// declared that this endpoint emits it: it is a chat-template dialect, and
    /// turning prose into an executed tool call on every OpenAI-compatible
    /// endpoint is a capability PRX never declared. A declared endpoint whose
    /// block fails the registered-name or JSON check is reported once, redacted
    /// — dropping it silently is why the behaviour took a live session to see.
    fn parse_native_response(
        &self,
        message: ResponseMessage,
        registered_tools: Option<&[crate::tools::ToolSpec]>,
    ) -> ProviderChatResponse {
        let mut tool_calls = message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .filter_map(|tc| {
                let function = tc.function?;
                let name = function.name?;
                let arguments = function.arguments.unwrap_or_else(|| "{}".to_string());
                Some(ProviderToolCall {
                    id: tc.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    name,
                    arguments,
                })
            })
            .collect::<Vec<_>>();

        if tool_calls.is_empty()
            && self.tool_call_dialect() == super::ToolCallDialect::XmlFunctionTags
            && let Some(content) = message.content.as_deref()
        {
            // A request that registered no tools has no name a textual block
            // could legitimately carry, so "no registry" is "nothing is
            // registered" and every block is rejected. Reading the absence as
            // "every name is allowed" would turn any `<function=NAME>` in prose
            // into a call with an arbitrary name.
            let allowed = registered_tools
                .unwrap_or(&[])
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<std::collections::HashSet<_>>();
            let mut rest = content;
            while let Some(start) = rest.find("<function=") {
                let after = &rest[start + "<function=".len()..];
                let Some(name_end) = after.find('>') else { break };
                let name = &after[..name_end];
                let body = &after[name_end + 1..];
                let Some(end) = body.find("</function>") else { break };
                let arguments = &body[..end];
                if !allowed.contains(name) {
                    tracing::warn!(
                        provider = %self.name,
                        tool = %sanitize_tool_call_log_preview(name),
                        reason = "tool name is not registered for this request",
                        "declared textual tool-call block rejected"
                    );
                } else if serde_json::from_str::<serde_json::Value>(arguments).is_ok() {
                    tool_calls.push(ProviderToolCall {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: name.to_string(),
                        arguments: arguments.to_string(),
                    });
                } else {
                    tracing::warn!(
                        provider = %self.name,
                        tool = %sanitize_tool_call_log_preview(name),
                        reason = "body is not JSON",
                        body = %sanitize_tool_call_log_preview(arguments),
                        "declared textual tool-call block rejected"
                    );
                }
                rest = &body[end + "</function>".len()..];
            }
        }

        ProviderChatResponse {
            text: message.content,
            tool_calls,
            reasoning_content: message.reasoning_content,
        }
    }

    fn build_streaming_history_request(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> anyhow::Result<NativeChatRequest> {
        let effective_messages = if self.merge_system_into_user {
            Self::flatten_system_messages(messages)
        } else {
            messages.to_vec()
        };
        let tools = self.convert_tool_specs(model, options.tools.as_deref())?;
        let tool_choice = tools.as_ref().map(|_| "auto".to_string());
        Ok(NativeChatRequest {
            model: model.to_string(),
            messages: Self::convert_messages_for_native(&effective_messages),
            temperature,
            stream: Some(options.enabled),
            tools,
            tool_choice,
            stream_options: if options.enabled {
                Some(StreamUsageOptions { include_usage: true })
            } else {
                None
            },
        })
    }

    async fn chat_metered(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<(ProviderChatResponse, TokenUsage)> {
        let credential = self.outbound_credential()?;

        let tools = self.convert_tool_specs(model, request.tools)?;
        let effective_messages = if self.merge_system_into_user {
            Self::flatten_system_messages(request.messages)
        } else {
            request.messages.to_vec()
        };
        let native_messages = Self::convert_messages_for_native(&effective_messages);

        let native_request = NativeChatRequest {
            model: model.to_string(),
            messages: native_messages,
            temperature,
            stream: Some(false),
            tool_choice: tools.as_ref().map(|_| "auto".to_string()),
            tools,
            stream_options: None,
        };

        let url = self.chat_completions_url();
        let response = match super::attempt_accounting::send_json(
            self.apply_auth_header(self.http_client().post(&url), credential),
            model,
            &url,
            credential,
            &super::parameter_wire::request(
                self.configured_label.as_deref().unwrap_or(&self.name),
                super::parameter_wire::Format::Compatible,
                &native_request.model,
                &native_request,
            ),
            super::attempt_accounting::PhysicalKind::Chat,
        )
        .await
        {
            Ok(response) => response,
            // A transport failure is not endpoint discovery. The upstream may
            // already have processed the request, so replaying it through the
            // Responses API risks duplicate model work and tool side effects.
            // Only an explicit HTTP 404 below is safe evidence for fallback.
            Err(chat_error) => return Err(chat_error),
        };

        if !response.status().is_success() {
            let status = response.status();
            let error = response.error_body().await;
            let sanitized = super::sanitize_api_error(&error);

            tracing::warn!(
                provider = %self.name,
                http_status = %status,
                error_preview = %&sanitized[..sanitized.len().min(300)],
                "OpenAiCompatibleProvider.chat() got non-200 response"
            );

            if Self::is_native_tool_schema_unsupported(status, &sanitized) {
                tracing::warn!("is_native_tool_schema_unsupported triggered — falling back to prompt-guided XML tools");
                let fallback_messages = Self::with_prompt_guided_tool_instructions(request.messages, request.tools);
                let text = self.chat_with_history(&fallback_messages, model, temperature).await?;
                let mut accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
                accumulator.record_estimated_completion_chars(text.chars().count());
                return Ok((
                    ProviderChatResponse {
                        text: Some(text),
                        tool_calls: vec![],
                        reasoning_content: None,
                    },
                    accumulator.finish(),
                ));
            }

            if status == reqwest::StatusCode::NOT_FOUND && self.supports_responses_fallback {
                return self
                    .chat_via_responses(credential, &effective_messages, model)
                    .await
                    .map(|text| {
                        let mut accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
                        accumulator.record_estimated_completion_chars(text.chars().count());
                        (
                            ProviderChatResponse {
                                text: Some(text),
                                tool_calls: vec![],
                                reasoning_content: None,
                            },
                            accumulator.finish(),
                        )
                    })
                    .map_err(|responses_err| {
                        if super::attempt_accounting::is_local_refusal(&responses_err) {
                            return responses_err;
                        }
                        anyhow::anyhow!(
                            "{} API error ({status}): {sanitized} (chat completions unavailable; responses fallback failed: {responses_err})",
                            self.name
                        )
                    });
            }

            let upstream = format!("{} API error ({status}): {sanitized}", self.name);
            if Self::names_a_root_combinator_rejection(status, &sanitized)
                && let Some(hint) = self.root_combinator_declaration_hint(model)
            {
                return Err(RootCombinatorRejection { upstream, hint }.into());
            }
            anyhow::bail!("{upstream}");
        }

        let native_response: ApiChatResponse = response.parse_native().await?;
        let usage = native_response.usage.and_then(OpenAiCompatibleUsage::into_reported);
        let message = native_response
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .ok_or_else(|| anyhow::anyhow!("No response from {}", self.name))?;

        let response = self.parse_native_response(message, request.tools);
        let tokens_used = usage.unwrap_or_else(|| {
            let chars = response.text.as_deref().unwrap_or("").chars().count()
                + response.reasoning_content.as_deref().unwrap_or("").chars().count();
            let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
            accumulator.finish_or_estimate_completion_chars(chars)
        });
        Ok((response, tokens_used))
    }

    fn is_native_tool_schema_unsupported(status: reqwest::StatusCode, error: &str) -> bool {
        if !matches!(
            status,
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY
        ) {
            return false;
        }

        let lower = error.to_lowercase();
        [
            "unknown parameter: tools",
            "unsupported parameter: tools",
            "unrecognized field `tools`",
            "does not support tools",
            "function calling is not supported",
            "tool_choice",
        ]
        .iter()
        .any(|hint| lower.contains(hint))
    }

    const fn stream_options_retry_status(status: reqwest::StatusCode) -> bool {
        matches!(
            status,
            reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::NOT_FOUND
                | reqwest::StatusCode::UNPROCESSABLE_ENTITY
        )
    }
}

/// How much of a rejected tool-call block reaches the log.
const TOOL_CALL_LOG_PREVIEW_CHARS: usize = 200;

/// Redact and truncate text from a model response before it is logged.
///
/// A rejected block is model output: it can carry anything the model was shown
/// or invented, so known secret shapes are scrubbed and the remainder is capped
/// at a preview. The point of the log is to name what was rejected and why, not
/// to reproduce it.
fn sanitize_tool_call_log_preview(input: &str) -> String {
    let scrubbed = crate::security::redaction::redact_secrets(input);
    if scrubbed.chars().count() <= TOOL_CALL_LOG_PREVIEW_CHARS {
        return scrubbed;
    }
    scrubbed.chars().take(TOOL_CALL_LOG_PREVIEW_CHARS).collect()
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn accounting_targets(&self, model: &str) -> Option<super::AccountingTargets> {
        let mut endpoints = vec![self.chat_completions_url()];
        if self.supports_responses_fallback {
            endpoints.push(self.responses_url());
        }
        Some(super::AccountingTargets::chat(
            model,
            endpoints,
            self.outbound_credential().ok()?,
        ))
    }

    fn supports_attempt_accounting(&self) -> bool {
        true
    }

    /// The dialect this endpoint lowers every model's tool schemas to, when
    /// they all agree.
    ///
    /// The hook carries no model. Every model this endpoint could serve is
    /// covered: the declared ones, and any undeclared model at the
    /// provider-wide declaration (or `Raw` when there is none). If they agree
    /// the answer is exact; if they differ it is `Raw`, the published
    /// document, because a per-model rewrite is not what the other models
    /// were shown. A caller that knows the model asks
    /// [`tool_schema_dialect_for`](Self::tool_schema_dialect_for).
    fn tool_schema_dialect(&self) -> crate::tools::ToolSchemaDialect {
        let undeclared_models = self
            .tool_schema_dialects
            .iter()
            .find(|(model, _)| model.is_none())
            .map_or(crate::tools::ToolSchemaDialect::Raw, |(_, dialect)| *dialect);
        super::traits::unanimous_dialect(
            std::iter::once(undeclared_models).chain(self.tool_schema_dialects.iter().map(|(_, dialect)| *dialect)),
        )
    }

    /// The dialect declared for exactly `model`, most specific first.
    fn tool_schema_dialect_for(&self, model: &str) -> crate::tools::ToolSchemaDialect {
        self.tool_schema_dialect_for_model(model)
    }

    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        // The adapter can encode native tools and image parts. Whether the
        // selected model accepts them is catalog data applied by the runtime.
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
        let credential = self.outbound_credential()?;

        let mut messages = Vec::new();

        if self.merge_system_into_user {
            let content = system_prompt.map_or_else(|| message.to_string(), |sys| format!("{sys}\n\n{message}"));
            messages.push(Message {
                role: "user".to_string(),
                content,
            });
        } else {
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
        }

        let request = ApiChatRequest {
            model: model.to_string(),
            messages,
            temperature,
            stream: Some(false),
            tools: None,
            tool_choice: None,
            stream_options: None,
        };

        let url = self.chat_completions_url();

        let mut fallback_messages = Vec::new();
        if let Some(system_prompt) = system_prompt {
            fallback_messages.push(ChatMessage::system(system_prompt));
        }
        fallback_messages.push(ChatMessage::user(message));
        let fallback_messages = if self.merge_system_into_user {
            Self::flatten_system_messages(&fallback_messages)
        } else {
            fallback_messages
        };

        let response = match self
            .apply_auth_header(
                self.http_client().post(&url).json(&super::parameter_wire::request(
                    self.configured_label.as_deref().unwrap_or(&self.name),
                    super::parameter_wire::Format::Compatible,
                    &request.model,
                    &request,
                )),
                credential,
            )
            .send()
            .await
        {
            Ok(response) => response,
            Err(chat_error) => return Err(chat_error.into()),
        };

        if !response.status().is_success() {
            let status = response.status();
            let error = super::read_error_body(response).await;
            let sanitized = super::sanitize_api_error(&error);

            if status == reqwest::StatusCode::NOT_FOUND && self.supports_responses_fallback {
                return self
                    .chat_via_responses(credential, &fallback_messages, model)
                    .await
                    .map_err(|responses_err| {
                        anyhow::anyhow!(
                            "{} API error ({status}): {sanitized} (chat completions unavailable; responses fallback failed: {responses_err})",
                            self.name
                        )
                    });
            }

            anyhow::bail!("{} API error ({status}): {sanitized}", self.name);
        }

        let body = response.text().await?;
        let chat_response = parse_chat_response_body(&self.name, &body)?;

        chat_response
            .choices
            .into_iter()
            .next()
            .map(|c| {
                // If tool_calls are present, serialize the full message as JSON
                // so parse_tool_calls can handle the OpenAI-style format
                if c.message.tool_calls.is_some() && c.message.tool_calls.as_ref().map_or(false, |t| !t.is_empty()) {
                    serde_json::to_string(&c.message).unwrap_or_else(|_| c.message.fallback_text_for_legacy_api())
                } else {
                    // No tool calls — return visible content only. Reasoning is
                    // intentionally not concatenated; the String-returning
                    // legacy API cannot carry it back to history.
                    c.message.fallback_text_for_legacy_api()
                }
            })
            .ok_or_else(|| anyhow::anyhow!("No response from {}", self.name))
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let credential = self.outbound_credential()?;

        let effective_messages = if self.merge_system_into_user {
            Self::flatten_system_messages(messages)
        } else {
            messages.to_vec()
        };
        let api_messages: Vec<Message> = effective_messages
            .iter()
            .map(|m| Message {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        let request = ApiChatRequest {
            model: model.to_string(),
            messages: api_messages,
            temperature,
            stream: Some(false),
            tools: None,
            tool_choice: None,
            stream_options: None,
        };

        let url = self.chat_completions_url();
        let response = match super::attempt_accounting::send_json(
            self.apply_auth_header(self.http_client().post(&url), credential),
            model,
            &url,
            credential,
            &super::parameter_wire::request(
                self.configured_label.as_deref().unwrap_or(&self.name),
                super::parameter_wire::Format::Compatible,
                &request.model,
                &request,
            ),
            super::attempt_accounting::PhysicalKind::Chat,
        )
        .await
        {
            Ok(response) => response,
            Err(chat_error) => return Err(chat_error),
        };

        if !response.status().is_success() {
            let status = response.status();

            // Mirror chat_with_system: 404 may mean this provider uses the Responses API
            if status == reqwest::StatusCode::NOT_FOUND && self.supports_responses_fallback {
                return self
                    .chat_via_responses(credential, &effective_messages, model)
                    .await
                    .map_err(|responses_err| {
                        if super::attempt_accounting::is_local_refusal(&responses_err) {
                            return responses_err;
                        }
                        anyhow::anyhow!(
                            "{} API error (chat completions unavailable; responses fallback failed: {responses_err})",
                            self.name
                        )
                    });
            }

            return Err(response.api_error(&self.name).await);
        }

        let body = response.text().await?;
        let chat_response = parse_chat_response_body(&self.name, &body)?;

        chat_response
            .choices
            .into_iter()
            .next()
            .map(|c| {
                // If tool_calls are present, serialize the full message as JSON
                // so parse_tool_calls can handle the OpenAI-style format
                if c.message.tool_calls.is_some() && c.message.tool_calls.as_ref().map_or(false, |t| !t.is_empty()) {
                    serde_json::to_string(&c.message).unwrap_or_else(|_| c.message.fallback_text_for_legacy_api())
                } else {
                    // No tool calls — return visible content only.
                    c.message.fallback_text_for_legacy_api()
                }
            })
            .ok_or_else(|| anyhow::anyhow!("No response from {}", self.name))
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let credential = self.outbound_credential()?;

        let effective_messages = if self.merge_system_into_user {
            Self::flatten_system_messages(messages)
        } else {
            messages.to_vec()
        };
        let api_messages: Vec<Message> = effective_messages
            .iter()
            .map(|m| Message {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        let request = ApiChatRequest {
            model: model.to_string(),
            messages: api_messages,
            temperature,
            stream: Some(false),
            tools: if tools.is_empty() { None } else { Some(tools.to_vec()) },
            tool_choice: if tools.is_empty() {
                None
            } else {
                Some("auto".to_string())
            },
            stream_options: None,
        };

        let url = self.chat_completions_url();
        let response = match self
            .apply_auth_header(
                self.http_client().post(&url).json(&super::parameter_wire::request(
                    self.configured_label.as_deref().unwrap_or(&self.name),
                    super::parameter_wire::Format::Compatible,
                    &request.model,
                    &request,
                )),
                credential,
            )
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(
                    "{} native tool call transport failed: {error}; falling back to history path",
                    self.name
                );
                let text = self.chat_with_history(messages, model, temperature).await?;
                return Ok(ProviderChatResponse {
                    text: Some(text),
                    tool_calls: vec![],
                    reasoning_content: None,
                });
            }
        };

        if !response.status().is_success() {
            return Err(super::api_error(&self.name, response).await);
        }

        let body = response.text().await?;
        let chat_response = parse_chat_response_body(&self.name, &body)?;
        let choice = chat_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No response from {}", self.name))?;

        // Split visible text from reasoning at the provider parsing layer so
        // chat consumers can drop reasoning from the live stream while history
        // reconstruction still has it available via `reasoning_content`.
        let text = choice.message.visible_content_optional();
        let reasoning_content = choice.message.reasoning();
        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .filter_map(|tc| {
                let function = tc.function?;
                let name = function.name?;
                let arguments = function.arguments.unwrap_or_else(|| "{}".to_string());
                Some(ProviderToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    arguments,
                })
            })
            .collect::<Vec<_>>();

        Ok(ProviderChatResponse {
            text,
            tool_calls,
            reasoning_content,
        })
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
        let finished_at = chrono::Utc::now();
        Ok(ChatTrace {
            response,
            attempts: vec![ProviderAttempt {
                seq: 1,
                provider: self.name.clone(),
                model: model.to_string(),
                started_at,
                finished_at,
                status: AttemptStatus::Success,
                error_class: None,
                error_message: None,
            }],
            final_provider: self.name.clone(),
            final_model: model.to_string(),
            served_member: None,
            tokens_used,
        })
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let credential = match self.outbound_credential_for_stream() {
            Ok(credential) => credential,
            Err(message) => {
                return stream::once(async move { Err(StreamError::Provider(message)) }).boxed();
            }
        };

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

        let request = ApiChatRequest {
            model: model.to_string(),
            messages,
            temperature,
            stream: Some(options.enabled),
            tools: None,
            tool_choice: None,
            stream_options: if options.enabled {
                Some(StreamUsageOptions { include_usage: true })
            } else {
                None
            },
        };

        let url = self.chat_completions_url();
        let client = self.streaming_http_client();
        let auth_header = self.auth_header.clone();
        let provider_name = self.name.clone();
        let parameter_provider = self.configured_label.clone().unwrap_or_else(|| self.name.clone());

        // Use a channel to bridge the async HTTP response to the stream
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);

        crate::providers::compatible::spawn_until_closed(tx.clone(), async move {
            // Build request with auth
            let mut req_builder = client.post(&url).json(&super::parameter_wire::request(
                &parameter_provider,
                super::parameter_wire::Format::Compatible,
                &request.model,
                &request,
            ));

            // Apply auth header
            req_builder = Self::apply_auth_header_with_style(req_builder, &auth_header, credential.as_deref());

            // Set accept header for streaming
            req_builder = req_builder.header("Accept", "text/event-stream");

            // Send request
            let mut response = match req_builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
                    return;
                }
            };

            if !response.status().is_success()
                && request.stream_options.is_some()
                && Self::stream_options_retry_status(response.status())
            {
                let status = response.status();
                tracing::debug!(
                    provider = %provider_name,
                    %status,
                    "stream_options.include_usage rejected; retrying stream without usage request"
                );
                let mut fallback_request = request;
                fallback_request.stream_options = None;
                let mut fallback_builder = client.post(&url).json(&super::parameter_wire::retry_request(
                    &parameter_provider,
                    super::parameter_wire::Format::Compatible,
                    &fallback_request.model,
                    &fallback_request,
                ));
                fallback_builder =
                    Self::apply_auth_header_with_style(fallback_builder, &auth_header, credential.as_deref());
                fallback_builder = fallback_builder.header("Accept", "text/event-stream");
                response = match fallback_builder.send().await {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(StreamError::Http(e))).await;
                        return;
                    }
                };
            }

            // Check status. FIX-P0-33: 429/503 carry a structured Retry-After.
            if !response.status().is_success() {
                let _ = tx
                    .send(Err(super::stream_api_error(&provider_name, response).await))
                    .await;
                return;
            }

            // Convert to chunk stream and forward to channel
            forward_stream_until_closed(sse_bytes_to_chunks(response, options.count_tokens), &tx).await;
        });

        // Convert channel receiver to stream
        stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let credential = match self.outbound_credential_for_stream() {
            Ok(credential) => credential,
            Err(message) => {
                return stream::once(async move { Err(StreamError::Provider(message)) }).boxed();
            }
        };

        let count_tokens = options.count_tokens;
        // Built before the connection is opened, so a tool this endpoint cannot
        // be offered ends the turn with the reason rather than an upstream 400.
        let request = match self.build_streaming_history_request(messages, model, temperature, options) {
            Ok(request) => request,
            Err(error) => {
                let message = error.to_string();
                return stream::once(async move { Err(StreamError::Provider(message)) }).boxed();
            }
        };
        let url = self.chat_completions_url();
        let client = self.streaming_http_client();
        let auth_header = self.auth_header.clone();
        let provider_name = self.name.clone();
        let parameter_provider = self.configured_label.clone().unwrap_or_else(|| self.name.clone());
        // Only a request that offered tools can be rejected for their schema.
        let declaration_hint = request
            .tools
            .is_some()
            .then(|| self.root_combinator_declaration_hint(model))
            .flatten();

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);

        crate::providers::compatible::spawn_until_closed(tx.clone(), async move {
            let mut req_builder = client.post(&url).json(&super::parameter_wire::request(
                &parameter_provider,
                super::parameter_wire::Format::Compatible,
                &request.model,
                &request,
            ));
            req_builder = Self::apply_auth_header_with_style(req_builder, &auth_header, credential.as_deref());
            req_builder = req_builder.header("Accept", "text/event-stream");

            let mut response = match req_builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
                    return;
                }
            };

            if !response.status().is_success()
                && request.stream_options.is_some()
                && Self::stream_options_retry_status(response.status())
            {
                let status = response.status();
                tracing::debug!(
                    provider = %provider_name,
                    %status,
                    "stream_options.include_usage rejected; retrying stream without usage request"
                );
                let mut fallback_request = request;
                fallback_request.stream_options = None;
                let mut fallback_builder = client.post(&url).json(&super::parameter_wire::retry_request(
                    &parameter_provider,
                    super::parameter_wire::Format::Compatible,
                    &fallback_request.model,
                    &fallback_request,
                ));
                fallback_builder =
                    Self::apply_auth_header_with_style(fallback_builder, &auth_header, credential.as_deref());
                fallback_builder = fallback_builder.header("Accept", "text/event-stream");
                response = match fallback_builder.send().await {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(StreamError::Http(e))).await;
                        return;
                    }
                };
            }

            // FIX-P0-33: 429/503 carry a structured Retry-After.
            if !response.status().is_success() {
                let status = response.status();
                let error = match super::stream_api_error(&provider_name, response).await {
                    StreamError::Provider(message) => match declaration_hint.as_deref() {
                        Some(hint) if Self::names_a_root_combinator_rejection(status, &message) => {
                            StreamError::Provider(format!("{message}{hint}"))
                        }
                        _ => StreamError::Provider(message),
                    },
                    other => other,
                };
                let _ = tx.send(Err(error)).await;
                return;
            }

            forward_stream_until_closed(sse_bytes_to_chunks(response, count_tokens), &tx).await;
        });

        stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) }).boxed()
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        if self.should_skip_warmup() {
            return Ok(());
        }

        if let Some(credential) = self.credential.as_ref() {
            // Hit the chat completions URL with a GET to establish the connection pool.
            // The server will likely return 405 Method Not Allowed, which is fine -
            // the goal is TLS handshake and HTTP/2 negotiation.
            let url = self.chat_completions_url();
            let _ = self
                .apply_auth_header(self.http_client().get(&url), Some(credential.as_str()))
                .send()
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "compatible/parameter_tests.rs"]
mod parameter_tests;

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
    use super::*;

    /// The `<function=NAME>...</function>` grammar is a chat-template dialect,
    /// not a declared provider capability. An endpoint nobody declared it for
    /// must not have it parsed out of its prose: only the native `tool_calls`
    /// array is a tool call there.
    #[test]
    fn a_textual_function_call_is_not_executed_on_an_undeclared_endpoint() {
        let provider = OpenAiCompatibleProvider::new("Fixture", "http://127.0.0.1:1/v1", None, AuthStyle::Bearer);
        let tools = vec![crate::tools::ToolSpec {
            name: "shell".to_string(),
            description: "Run a command".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];
        let message = ResponseMessage {
            content: Some(r#"<function=shell>{"command":"date"}</function>"#.to_string()),
            tool_calls: None,
            reasoning_content: None,
        };
        let parsed = provider.parse_native_response(message, Some(&tools));
        assert!(
            parsed.tool_calls.is_empty(),
            "an undeclared endpoint had a textual tool call executed: {:?}",
            parsed.tool_calls
        );
    }

    /// The same body on an endpoint that was declared to emit the grammar is
    /// still parsed, so declaring it is the whole of the behaviour change.
    #[test]
    fn a_textual_function_call_is_executed_on_a_declared_endpoint() {
        let provider = OpenAiCompatibleProvider::new("Fixture", "http://127.0.0.1:1/v1", None, AuthStyle::Bearer)
            .with_tool_call_dialects(vec![(None, super::super::ToolCallDialect::XmlFunctionTags)]);
        let tools = vec![crate::tools::ToolSpec {
            name: "shell".to_string(),
            description: "Run a command".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];
        let message = ResponseMessage {
            content: Some(r#"<function=shell>{"command":"date"}</function>"#.to_string()),
            tool_calls: None,
            reasoning_content: None,
        };
        let parsed = provider.parse_native_response(message, Some(&tools));
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "shell");
        assert_eq!(parsed.tool_calls[0].arguments, r#"{"command":"date"}"#);
    }

    /// A declared endpoint whose request registered no tool set has nothing a
    /// textual block could legitimately name. Treating "no registry" as "every
    /// name is allowed" turns any `<function=anything>{json}</function>` in
    /// prose into a tool call with an arbitrary name; the check must be that a
    /// registered name was matched, not that no registry contradicted it.
    #[test]
    fn a_declared_endpoint_with_no_registered_tools_refuses_every_textual_call() {
        let provider = OpenAiCompatibleProvider::new("Fixture", "http://127.0.0.1:1/v1", None, AuthStyle::Bearer)
            .with_tool_call_dialects(vec![(None, super::super::ToolCallDialect::XmlFunctionTags)]);
        let message = ResponseMessage {
            content: Some(r#"<function=anything>{"command":"date"}</function>"#.to_string()),
            tool_calls: None,
            reasoning_content: None,
        };

        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let sink = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || CaptureWriter(sink.clone()))
            .finish();
        let parsed = tracing::subscriber::with_default(subscriber, || provider.parse_native_response(message, None));

        assert!(
            parsed.tool_calls.is_empty(),
            "a request with no registered tools executed a textual call: {:?}",
            parsed.tool_calls
        );
        let logged = String::from_utf8(captured.lock().clone()).expect("test: log is utf-8");
        assert_eq!(
            logged.matches("textual tool-call block").count(),
            1,
            "the refusal must be reported exactly once: {logged}"
        );

        // An empty registry is the same statement as no registry at all.
        let provider = OpenAiCompatibleProvider::new("Fixture", "http://127.0.0.1:1/v1", None, AuthStyle::Bearer)
            .with_tool_call_dialects(vec![(None, super::super::ToolCallDialect::XmlFunctionTags)]);
        let message = ResponseMessage {
            content: Some(r#"<function=anything>{"command":"date"}</function>"#.to_string()),
            tool_calls: None,
            reasoning_content: None,
        };
        let parsed = provider.parse_native_response(message, Some(&[]));
        assert!(
            parsed.tool_calls.is_empty(),
            "an empty registry executed a textual call: {:?}",
            parsed.tool_calls
        );
    }

    /// A body the declared grammar cannot parse must say so once, redacted.
    /// Silently skipping it is why F6 needed a live session to notice.
    #[test]
    fn an_unparseable_textual_function_call_is_reported_not_dropped() {
        let provider = OpenAiCompatibleProvider::new("Fixture", "http://127.0.0.1:1/v1", None, AuthStyle::Bearer)
            .with_tool_call_dialects(vec![(None, super::super::ToolCallDialect::XmlFunctionTags)]);
        let tools = vec![crate::tools::ToolSpec {
            name: "shell".to_string(),
            description: "Run a command".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];
        let message = ResponseMessage {
            // The Qwen chat-template spelling: parameters as nested tags, not JSON.
            content: Some(
                "<function=shell><parameter=command>date sk-0123456789abcdef</parameter></function>".to_string(),
            ),
            tool_calls: None,
            reasoning_content: None,
        };

        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let sink = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || CaptureWriter(sink.clone()))
            .finish();
        let parsed =
            tracing::subscriber::with_default(subscriber, || provider.parse_native_response(message, Some(&tools)));

        assert!(parsed.tool_calls.is_empty(), "an unparseable body must not execute");
        let logged = String::from_utf8(captured.lock().clone()).expect("test: log is utf-8");
        assert!(
            logged.contains("shell") && logged.contains("body is not JSON"),
            "the rejected body was not reported: {logged}"
        );
        assert_eq!(
            logged.matches("textual tool-call block").count(),
            1,
            "the rejection must be reported exactly once: {logged}"
        );
        assert!(
            !logged.contains("sk-0123456789abcdef"),
            "the rejected body was logged unredacted: {logged}"
        );
    }

    /// A `tracing` writer that appends into a shared buffer.
    struct CaptureWriter(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A tool schema this endpoint cannot accept must stop the request here,
    /// not at the endpoint.
    ///
    /// The provider is pointed at a port nothing listens on, so a request that
    /// were actually sent would fail with a transport error. The assertion is
    /// that the error is the lowering diagnostic instead: the tool is named,
    /// the keyword is named, and no connection was attempted.
    #[tokio::test]
    async fn a_tool_that_cannot_be_lowered_fails_before_the_request_is_sent() {
        let provider = OpenAiCompatibleProvider::new(
            "Fixture",
            // Port 1 is reserved and refuses connections, so reaching the
            // transport is observable as a different error.
            "http://127.0.0.1:1/v1",
            Some("fixture-key"),
            AuthStyle::Bearer,
        )
        .with_tool_schema_dialects(vec![(None, crate::tools::ToolSchemaDialect::NoRootCombinators)]);

        let tools = vec![crate::tools::ToolSpec {
            name: "fixture_alternatives".to_string(),
            description: "Requires one of two argument sets".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "key": {"type": "string"}},
                "anyOf": [{"required": ["path"]}, {"required": ["key"]}]
            }),
        }];
        let messages = vec![ChatMessage::user("go")];
        let error = provider
            .chat(
                ProviderChatRequest {
                    messages: &messages,
                    tools: Some(&tools),
                },
                "fixture-model",
                0.0,
            )
            .await
            .expect_err("test: an unlowerable tool must fail closed");
        let rendered = error.to_string();
        for fragment in [
            "fixture_alternatives",
            "anyOf",
            "no_root_combinators",
            "tool_schema_dialect",
        ] {
            assert!(
                rendered.contains(fragment),
                "the pre-transport refusal does not name '{fragment}': {rendered}"
            );
        }
        assert!(
            !rendered.contains("127.0.0.1"),
            "the request reached the transport instead of failing closed: {rendered}"
        );
    }

    /// The same endpoint with no declaration keeps every tool and every byte.
    #[test]
    fn an_undeclared_endpoint_keeps_the_published_document() {
        let provider = OpenAiCompatibleProvider::new("Fixture", "http://127.0.0.1:1/v1", None, AuthStyle::Bearer);
        assert_eq!(
            provider.tool_schema_dialect(),
            crate::tools::ToolSchemaDialect::Raw,
            "an endpoint nobody declared anything about must keep the strongest contract"
        );
        let tools = vec![crate::tools::ToolSpec {
            name: "fixture_alternatives".to_string(),
            description: "Requires one of two argument sets".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "key": {"type": "string"}},
                "anyOf": [{"required": ["path"]}, {"required": ["key"]}]
            }),
        }];
        let converted = provider
            .convert_tool_specs("fixture-model", Some(&tools))
            .expect("test: an undeclared endpoint refuses nothing")
            .expect("test: tools present");
        assert_eq!(converted[0]["function"]["parameters"], tools[0].parameters);
    }

    /// A declaration for one model does not reach another model of the same
    /// endpoint.
    #[test]
    fn a_model_scoped_declaration_applies_only_to_that_model() {
        let provider = OpenAiCompatibleProvider::new("Fixture", "http://127.0.0.1:1/v1", None, AuthStyle::Bearer)
            .with_tool_schema_dialects(vec![(
                Some("small-context".to_string()),
                crate::tools::ToolSchemaDialect::NoRootCombinators,
            )]);
        let tools = vec![crate::tools::ToolSpec {
            name: "fixture_alternatives".to_string(),
            description: "Requires one of two argument sets".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "anyOf": [{"required": ["path"]}]
            }),
        }];
        assert!(
            provider.convert_tool_specs("small-context", Some(&tools)).is_err(),
            "the declared model must get the declared dialect"
        );
        assert!(
            provider.convert_tool_specs("other-model", Some(&tools)).is_ok(),
            "an undeclared model of the same endpoint must keep the published document"
        );
    }

    /// **UTF-8 streaming regression** — the byte-level decoder used by this
    /// provider's stream loop must carry a character that the transport split
    /// across two chunks, while still rejecting genuinely malformed bytes.
    #[test]
    fn stream_decoder_survives_split_multibyte_characters() {
        let mut decoder = SseTextDecoder::new(STREAM_DECODER_LABEL);
        let mut buf = String::new();
        let bytes = "\u{20ac}".as_bytes();
        assert_eq!(bytes.len(), 3);

        decoder
            .push(&bytes[..2], &mut buf)
            .expect("test: an incomplete character is not a stream error");
        assert!(buf.is_empty(), "test: nothing decodable yet");
        assert_eq!(decoder.pending_len(), 2, "test: the split tail is carried, not dropped");

        decoder.push(&bytes[2..], &mut buf).expect("test: character completes");
        assert_eq!(buf, "\u{20ac}");
        decoder.finish().expect("test: stream ends on a character boundary");

        // Genuinely invalid input must still fail, tagged with this provider.
        let mut decoder = SseTextDecoder::new(STREAM_DECODER_LABEL);
        let err = decoder
            .push(&[0x80], &mut buf)
            .expect_err("test: a stray continuation byte must be reported");
        assert!(err.to_string().contains(STREAM_DECODER_LABEL), "got {err}");

        // A body that stops mid-character is truncation, not success.
        let mut decoder = SseTextDecoder::new(STREAM_DECODER_LABEL);
        let mut sink = String::new();
        decoder
            .push(&bytes[..1], &mut sink)
            .expect("test: partial tail buffered");
        assert!(decoder.finish().is_err(), "test: truncated tail must surface");
    }

    fn make_provider(name: &str, url: &str, key: Option<&str>) -> OpenAiCompatibleProvider {
        OpenAiCompatibleProvider::new(name, url, key, AuthStyle::Bearer)
    }

    #[test]
    fn creates_with_key() {
        let p = make_provider("venice", "https://api.venice.ai", Some("venice-test-credential"));
        assert_eq!(p.name, "venice");
        assert_eq!(p.base_url, "https://api.venice.ai");
        assert_eq!(p.credential.as_deref(), Some("venice-test-credential"));
    }

    #[test]
    fn creates_without_key() {
        let p = make_provider("test", "https://example.com", None);
        assert!(p.credential.is_none());
    }

    #[test]
    fn strips_trailing_slash() {
        let p = make_provider("test", "https://example.com/", None);
        assert_eq!(p.base_url, "https://example.com");
    }

    #[tokio::test]
    async fn chat_fails_without_key() {
        let p = make_provider("Venice", "https://api.venice.ai", None);
        let result = p.chat_with_system(None, "hello", "llama-3.3-70b", 0.7).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Venice API key not set"));
    }

    #[tokio::test]
    async fn transport_failure_is_not_replayed_through_responses_api() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("test: accept chat request");
            // Drop before sending HTTP headers. The provider cannot know
            // whether the upstream processed the request, so this ambiguous
            // failure must not cause a second request through another API.
            drop(socket);
            drop(listener);
        });

        let provider = make_provider("custom", &format!("http://{addr}"), Some("test-key"));
        let error = provider
            .chat_with_system(None, "hello", "test-model", 0.0)
            .await
            .expect_err("test: closed connection must fail");
        server.await.expect("test: join server");

        let message = error.to_string();
        assert!(!message.contains("responses fallback"), "got: {message}");
    }

    /// A buffered (non-streaming) response that arrives after the two-minute
    /// whole-response deadline this client used to carry still reaches the
    /// caller.
    ///
    /// The server runs on its own thread and runtime and holds the response
    /// until the test releases it. The client connects and sends on the real
    /// clock; once the server has the request, the client's clock is paused and
    /// advanced 121 s before the response is released, so any client-side
    /// deadline of 121 s or less has expired by the time the response can
    /// arrive, and fails the request — without the test waiting two minutes.
    ///
    /// It used to run as `#[tokio::test(start_paused = true)]` with the server
    /// on the same runtime and waited for the server-side rendezvous with no
    /// bound and without watching the request itself. Any request that ended
    /// before reaching the server therefore hung the test — and the suite —
    /// forever: nothing was left to wake the paused runtime. The client picks
    /// up the process-wide runtime proxy (`apply_runtime_proxy_to_builder`),
    /// and config-generation tests running in the same process publish
    /// `all_proxy = http://127.0.0.1:8080` for a moment; a request built in
    /// that window is refused by the absent proxy (reproduced: the old body
    /// under that proxy hangs until killed). A paused clock can also
    /// auto-advance to the 10 s `connect_timeout` while the loopback connect
    /// is still in flight, which ends the request the same way. Now the
    /// request is watched alongside the rendezvous, every wait is bounded in
    /// real time, and the test is `serial` with the other tests that publish
    /// a proxy, so a failure is reported, never a hang.
    ///
    /// MUTATION GUARD: give the buffered client a whole-response
    /// `.timeout(Duration::from_secs(120))` again and this goes red.
    #[test]
    #[serial_test::serial]
    fn buffered_response_survives_past_the_old_two_minute_deadline() {
        use axum::Router;
        use axum::routing::post;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::Notify;

        /// Real-time bound on the whole test, enforced from outside its
        /// runtimes so that no paused clock can stretch or skip it.
        const WHOLE_TEST: Duration = Duration::from_mins(1);
        /// Real-time bound on the request reaching the server.
        const REQUEST_ARRIVES: Duration = Duration::from_secs(30);
        /// One second past the whole-response deadline the client used to set.
        const PAST_THE_OLD_DEADLINE: Duration = Duration::from_secs(121);

        let request_started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("test: bind");
        listener.set_nonblocking(true).expect("test: nonblocking listener");
        let addr = listener.local_addr().expect("test: addr");
        {
            let request_started = Arc::clone(&request_started);
            let release = Arc::clone(&release);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("test: server runtime");
                runtime.block_on(async move {
                    let app = Router::new().route(
                        "/chat/completions",
                        post(move || {
                            let request_started = Arc::clone(&request_started);
                            let release = Arc::clone(&release);
                            async move {
                                request_started.notify_one();
                                release.notified().await;
                                axum::Json(serde_json::json!({
                                    "choices": [{"message": {"content": "slow but healthy"}}]
                                }))
                            }
                        }),
                    );
                    let listener = tokio::net::TcpListener::from_std(listener).expect("test: adopt listener");
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async move { stop.notified().await })
                        .await
                        .expect("test: serve");
                });
            });
        }

        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test: client runtime");
            let outcome = runtime.block_on(async move {
                let provider = make_provider("custom", &format!("http://{addr}"), Some("test-key"));
                let mut request =
                    tokio::spawn(async move { provider.chat_with_system(None, "hello", "test-model", 0.0).await });
                tokio::select! {
                    arrived = tokio::time::timeout(REQUEST_ARRIVES, request_started.notified()) => {
                        if arrived.is_err() {
                            return Err(format!("the request did not reach the server within {REQUEST_ARRIVES:?}"));
                        }
                    }
                    early = &mut request => {
                        return Err(format!("the request ended before the server saw it: {early:?}"));
                    }
                }
                tokio::time::pause();
                tokio::time::advance(PAST_THE_OLD_DEADLINE).await;
                release.notify_one();
                request
                    .await
                    .map_err(|error| format!("the request task failed: {error}"))?
                    .map_err(|error| format!("a healthy buffered response has no whole-response deadline: {error:#}"))
            });
            // The receiver is gone only if the test already failed on time.
            let _ = done.send(outcome);
        });

        let outcome = finished.recv_timeout(WHOLE_TEST);
        stop.notify_one();
        let response = outcome.unwrap_or_else(|timed_out| {
            panic!("the buffered request did not finish within {WHOLE_TEST:?} of real time ({timed_out})")
        });
        assert_eq!(response.as_deref(), Ok("slow but healthy"));
    }

    #[test]
    fn request_serializes_correctly() {
        let req = ApiChatRequest {
            model: "llama-3.3-70b".to_string(),
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
            temperature: 0.4,
            stream: Some(false),
            tools: None,
            tool_choice: None,
            stream_options: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("llama-3.3-70b"));
        assert!(json.contains("system"));
        assert!(json.contains("user"));
        // tools/tool_choice should be omitted when None
        assert!(!json.contains("tools"));
        assert!(!json.contains("tool_choice"));
    }

    #[test]
    fn response_deserializes() {
        let json = r#"{"choices":[{"message":{"content":"Hello from Venice!"}}]}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content, Some("Hello from Venice!".to_string()));
    }

    #[test]
    fn compatible_non_streaming_response_usage_maps_reported() {
        let json = r#"{
            "choices":[{"message":{"content":"Hello from Venice!"}}],
            "usage":{"prompt_tokens":44,"completion_tokens":9,"total_tokens":53}
        }"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let usage = resp
            .usage
            .expect("usage expected")
            .into_reported()
            .expect("complete usage should report");

        assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Reported);
        assert_eq!(usage.prompt_tokens, Some(44));
        assert_eq!(usage.completion_tokens, Some(9));
        assert_eq!(usage.total_tokens, Some(53));
    }

    #[test]
    fn response_empty_choices() {
        let json = r#"{"choices":[]}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.choices.is_empty());
    }

    #[test]
    fn parse_chat_response_body_reports_sanitized_snippet() {
        let body = r#"{"choices":"invalid","api_key":"sk-test-secret-value"}"#;
        let err = parse_chat_response_body("custom", body).expect_err("payload should fail");
        let msg = err.to_string();

        assert!(msg.contains("custom API returned an unexpected chat-completions payload"));
        assert!(msg.contains("body="));
        assert!(msg.contains("[REDACTED]"));
        assert!(!msg.contains("sk-test-secret-value"));
    }

    #[test]
    fn parse_responses_response_body_reports_sanitized_snippet() {
        let body = r#"{"output_text":123,"api_key":"sk-another-secret"}"#;
        let err = parse_responses_response_body("custom", body).expect_err("payload should fail");
        let msg = err.to_string();

        assert!(msg.contains("custom Responses API returned an unexpected payload"));
        assert!(msg.contains("body="));
        assert!(msg.contains("[REDACTED]"));
        assert!(!msg.contains("sk-another-secret"));
    }

    #[test]
    fn x_api_key_auth_style() {
        let p = OpenAiCompatibleProvider::new(
            "moonshot",
            "https://api.moonshot.cn",
            Some("ms-key"),
            AuthStyle::XApiKey,
        );
        assert!(matches!(p.auth_header, AuthStyle::XApiKey));
    }

    #[test]
    fn custom_auth_style() {
        let p = OpenAiCompatibleProvider::new(
            "custom",
            "https://api.example.com",
            Some("key"),
            AuthStyle::Custom("X-Custom-Key".into()),
        );
        assert!(matches!(p.auth_header, AuthStyle::Custom(_)));
    }

    #[tokio::test]
    async fn all_compatible_providers_fail_without_key() {
        let providers = vec![
            make_provider("Venice", "https://api.venice.ai", None),
            make_provider("Moonshot", "https://api.moonshot.cn", None),
            make_provider("GLM", "https://open.bigmodel.cn", None),
            make_provider("MiniMax", "https://api.minimaxi.com/v1", None),
            make_provider("Groq", "https://api.groq.com/openai", None),
            make_provider("Mistral", "https://api.mistral.ai", None),
            make_provider("xAI", "https://api.x.ai/v1", None),
            make_provider("Astrai", "https://as-trai.com/v1", None),
        ];

        for p in providers {
            let result = p.chat_with_system(None, "test", "model", 0.7).await;
            assert!(result.is_err(), "{} should fail without key", p.name);
            assert!(
                result.unwrap_err().to_string().contains("API key not set"),
                "{} error should mention key",
                p.name
            );
        }
    }

    #[test]
    fn responses_extracts_top_level_output_text() {
        let json = r#"{"output_text":"Hello from top-level","output":[]}"#;
        let response: ResponsesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            extract_responses_text(response).as_deref(),
            Some("Hello from top-level")
        );
    }

    #[test]
    fn responses_extracts_nested_output_text() {
        let json = r#"{"output":[{"content":[{"type":"output_text","text":"Hello from nested"}]}]}"#;
        let response: ResponsesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(extract_responses_text(response).as_deref(), Some("Hello from nested"));
    }

    #[test]
    fn responses_extracts_any_text_as_fallback() {
        let json = r#"{"output":[{"content":[{"type":"message","text":"Fallback text"}]}]}"#;
        let response: ResponsesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(extract_responses_text(response).as_deref(), Some("Fallback text"));
    }

    #[test]
    fn build_responses_prompt_preserves_multi_turn_history() {
        let messages = vec![
            ChatMessage::system("policy"),
            ChatMessage::user("step 1"),
            ChatMessage::assistant("ack 1"),
            ChatMessage::tool("{\"result\":\"ok\"}"),
            ChatMessage::user("step 2"),
        ];

        let (instructions, input) = build_responses_prompt(&messages);

        assert_eq!(instructions.as_deref(), Some("policy"));
        assert_eq!(input.len(), 4);
        assert_eq!(input[0].role, "user");
        assert_eq!(input[0].content, "step 1");
        assert_eq!(input[1].role, "assistant");
        assert_eq!(input[1].content, "ack 1");
        assert_eq!(input[2].role, "assistant");
        assert_eq!(input[2].content, "{\"result\":\"ok\"}");
        assert_eq!(input[3].role, "user");
        assert_eq!(input[3].content, "step 2");
    }

    /// DC-6: the Responses API fallback's error is the sanitized upstream
    /// body, like every other provider error; it quoted the raw body.
    #[tokio::test]
    async fn a_responses_api_error_body_is_sanitized() {
        let stub = crate::providers::label_redaction_tests::spawn_fixed_status_stub(
            "401 Unauthorized",
            r#"{"error":{"message":"bad key sk-proj-FIXTURE0SECRET0responses1 rejected"}}"#,
        )
        .await;
        let provider = make_provider("custom", &format!("http://{}", stub.addr), Some("test-key"));
        let error = provider
            .chat_via_responses(Some("test-key"), &[ChatMessage::user("hi")], "gpt-test")
            .await
            .expect_err("a 401 is an error");
        let text = error.to_string();
        assert!(text.contains("Responses API error"), "{text}");
        assert!(
            !text.contains("FIXTURE0SECRET0responses1"),
            "the raw body reached the error: {text}"
        );
    }

    #[tokio::test]
    async fn chat_via_responses_requires_non_system_message() {
        let provider = make_provider("custom", "https://api.example.com", Some("test-key"));
        let err = provider
            .chat_via_responses(Some("test-key"), &[ChatMessage::system("policy")], "gpt-test")
            .await
            .expect_err("system-only fallback payload should fail");

        assert!(err.to_string().contains("requires at least one non-system message"));
    }

    // ----------------------------------------------------------
    // Custom endpoint path tests (Issue #114)
    // ----------------------------------------------------------

    #[test]
    fn chat_completions_url_standard_openai() {
        // Standard OpenAI-compatible providers get /chat/completions appended
        let p = make_provider("openai", "https://api.openai.com/v1", None);
        assert_eq!(p.chat_completions_url(), "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn chat_completions_url_trailing_slash() {
        // Trailing slash is stripped, then /chat/completions appended
        let p = make_provider("test", "https://api.example.com/v1/", None);
        assert_eq!(p.chat_completions_url(), "https://api.example.com/v1/chat/completions");
    }

    #[test]
    fn chat_completions_url_volcengine_ark() {
        // VolcEngine ARK uses custom path - should use as-is
        let p = make_provider(
            "volcengine",
            "https://ark.cn-beijing.volces.com/api/coding/v3/chat/completions",
            None,
        );
        assert_eq!(
            p.chat_completions_url(),
            "https://ark.cn-beijing.volces.com/api/coding/v3/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_custom_full_endpoint() {
        // Custom provider with full endpoint path
        let p = make_provider("custom", "https://my-api.example.com/v2/llm/chat/completions", None);
        assert_eq!(
            p.chat_completions_url(),
            "https://my-api.example.com/v2/llm/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_requires_exact_suffix_match() {
        let p = make_provider(
            "custom",
            "https://my-api.example.com/v2/llm/chat/completions-proxy",
            None,
        );
        assert_eq!(
            p.chat_completions_url(),
            "https://my-api.example.com/v2/llm/chat/completions-proxy/chat/completions"
        );
    }

    #[test]
    fn responses_url_standard() {
        // Standard providers get /v1/responses appended
        let p = make_provider("test", "https://api.example.com", None);
        assert_eq!(p.responses_url(), "https://api.example.com/v1/responses");
    }

    #[test]
    fn responses_url_custom_full_endpoint() {
        // Custom provider with full responses endpoint
        let p = make_provider("custom", "https://my-api.example.com/api/v2/responses", None);
        assert_eq!(p.responses_url(), "https://my-api.example.com/api/v2/responses");
    }

    #[test]
    fn responses_url_requires_exact_suffix_match() {
        let p = make_provider("custom", "https://my-api.example.com/api/v2/responses-proxy", None);
        assert_eq!(
            p.responses_url(),
            "https://my-api.example.com/api/v2/responses-proxy/responses"
        );
    }

    #[test]
    fn responses_url_derives_from_chat_endpoint() {
        let p = make_provider("custom", "https://my-api.example.com/api/v2/chat/completions", None);
        assert_eq!(p.responses_url(), "https://my-api.example.com/api/v2/responses");
    }

    #[test]
    fn responses_url_base_with_v1_no_duplicate() {
        let p = make_provider("test", "https://api.example.com/v1", None);
        assert_eq!(p.responses_url(), "https://api.example.com/v1/responses");
    }

    #[test]
    fn responses_url_non_v1_api_path_uses_raw_suffix() {
        let p = make_provider("test", "https://api.example.com/api/coding/v3", None);
        assert_eq!(p.responses_url(), "https://api.example.com/api/coding/v3/responses");
    }

    #[test]
    fn chat_completions_url_without_v1() {
        // Provider configured without /v1 in base URL
        let p = make_provider("test", "https://api.example.com", None);
        assert_eq!(p.chat_completions_url(), "https://api.example.com/chat/completions");
    }

    #[test]
    fn chat_completions_url_base_with_v1() {
        // Provider configured with /v1 in base URL
        let p = make_provider("test", "https://api.example.com/v1", None);
        assert_eq!(p.chat_completions_url(), "https://api.example.com/v1/chat/completions");
    }

    // ----------------------------------------------------------
    // Provider-specific endpoint tests (Issue #167)
    // ----------------------------------------------------------

    #[test]
    fn chat_completions_url_zai() {
        // Z.AI uses /api/paas/v4 base path
        let p = make_provider("zai", "https://api.z.ai/api/paas/v4", None);
        assert_eq!(
            p.chat_completions_url(),
            "https://api.z.ai/api/paas/v4/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_minimax() {
        // MiniMax OpenAI-compatible endpoint requires /v1 base path.
        let p = make_provider("minimax", "https://api.minimaxi.com/v1", None);
        assert_eq!(p.chat_completions_url(), "https://api.minimaxi.com/v1/chat/completions");
    }

    #[test]
    fn chat_completions_url_glm() {
        // GLM (BigModel) uses /api/paas/v4 base path
        let p = make_provider("glm", "https://open.bigmodel.cn/api/paas/v4", None);
        assert_eq!(
            p.chat_completions_url(),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_opencode() {
        // OpenCode Zen uses /zen/v1 base path
        let p = make_provider("opencode", "https://opencode.ai/zen/v1", None);
        assert_eq!(p.chat_completions_url(), "https://opencode.ai/zen/v1/chat/completions");
    }

    #[test]
    fn parse_native_response_preserves_tool_call_id() {
        let message = ResponseMessage {
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: Some("call_123".to_string()),
                kind: Some("function".to_string()),
                function: Some(Function {
                    name: Some("shell".to_string()),
                    arguments: Some(r#"{"command":"pwd"}"#.to_string()),
                }),
            }]),
            reasoning_content: None,
        };

        let parsed = OpenAiCompatibleProvider::new("Fixture", "http://127.0.0.1:1/v1", None, AuthStyle::Bearer)
            .parse_native_response(message, None);
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "call_123");
        assert_eq!(parsed.tool_calls[0].name, "shell");
        assert!(parsed.reasoning_content.is_none());
    }

    #[test]
    fn convert_messages_for_native_maps_tool_result_payload() {
        let input = vec![ChatMessage::tool(r#"{"tool_call_id":"call_abc","content":"done"}"#)];

        let converted = OpenAiCompatibleProvider::convert_messages_for_native(&input);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "tool");
        assert_eq!(converted[0].tool_call_id.as_deref(), Some("call_abc"));
        match converted[0].content.as_ref() {
            Some(NativeMessageContent::Text(text)) => assert_eq!(text, "done"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn compatible_parses_openai_nested_tool_calls_assistant_history() {
        let input = vec![ChatMessage::assistant(
            serde_json::json!({
                "content": "",
                "reasoning_content": "Need to inspect the system.",
                "tool_calls": [{
                    "id": "call_nested",
                    "type": "function",
                    "function": {
                        "name": "shell",
                        "arguments": "{\"command\":\"pwd\"}"
                    }
                }]
            })
            .to_string(),
        )];

        let converted = OpenAiCompatibleProvider::convert_messages_for_native(&input);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(
            converted[0].reasoning_content.as_deref(),
            Some("Need to inspect the system.")
        );
        let tool_calls = converted[0].tool_calls.as_ref().expect("tool_calls expected");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id.as_deref(), Some("call_nested"));
        assert_eq!(tool_calls[0].kind.as_deref(), Some("function"));
        let function = tool_calls[0].function.as_ref().expect("function expected");
        assert_eq!(function.name.as_deref(), Some("shell"));
        assert_eq!(function.arguments.as_deref(), Some(r#"{"command":"pwd"}"#));
    }

    #[test]
    fn compatible_falls_back_to_provider_tool_call_schema() {
        let input = vec![ChatMessage::assistant(
            serde_json::json!({
                "content": "checking",
                "tool_calls": [{
                    "id": "call_flat",
                    "name": "file_read",
                    "arguments": "{\"path\":\"README.md\"}"
                }]
            })
            .to_string(),
        )];

        let converted = OpenAiCompatibleProvider::convert_messages_for_native(&input);
        let tool_calls = converted[0].tool_calls.as_ref().expect("tool_calls expected");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id.as_deref(), Some("call_flat"));
        let function = tool_calls[0].function.as_ref().expect("function expected");
        assert_eq!(function.name.as_deref(), Some("file_read"));
        assert_eq!(function.arguments.as_deref(), Some(r#"{"path":"README.md"}"#));
        match converted[0].content.as_ref() {
            Some(NativeMessageContent::Text(text)) => assert_eq!(text, "checking"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn convert_messages_for_native_maps_user_images_to_content_parts() {
        let input = vec![ChatMessage::user("describe this [IMAGE:data:image/png;base64,abcd==]")];

        let converted = OpenAiCompatibleProvider::convert_messages_for_native(&input);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
        match converted[0].content.as_ref() {
            Some(NativeMessageContent::Parts(parts)) => {
                assert_eq!(parts.len(), 2);
                match &parts[0] {
                    NativeContentPart::Text { text } => assert_eq!(text, "describe this"),
                    other => panic!("expected text content part, got {other:?}"),
                }
                match &parts[1] {
                    NativeContentPart::ImageUrl { image_url } => {
                        assert_eq!(image_url.url, "data:image/png;base64,abcd==");
                    }
                    other => panic!("expected image content part, got {other:?}"),
                }
            }
            other => panic!("expected multipart content, got {other:?}"),
        }
    }

    #[test]
    fn flatten_system_messages_merges_into_first_user() {
        let input = vec![
            ChatMessage::system("core policy"),
            ChatMessage::assistant("ack"),
            ChatMessage::system("delivery rules"),
            ChatMessage::user("hello"),
            ChatMessage::assistant("post-user"),
        ];

        let output = OpenAiCompatibleProvider::flatten_system_messages(&input);
        assert_eq!(output.len(), 3);
        assert_eq!(output[0].role, "assistant");
        assert_eq!(output[0].content, "ack");
        assert_eq!(output[1].role, "user");
        assert_eq!(output[1].content, "core policy\n\ndelivery rules\n\nhello");
        assert_eq!(output[2].role, "assistant");
        assert_eq!(output[2].content, "post-user");
        assert!(output.iter().all(|m| m.role != "system"));
    }

    #[test]
    fn flatten_system_messages_inserts_user_when_missing() {
        let input = vec![ChatMessage::system("core policy"), ChatMessage::assistant("ack")];

        let output = OpenAiCompatibleProvider::flatten_system_messages(&input);
        assert_eq!(output.len(), 2);
        assert_eq!(output[0].role, "user");
        assert_eq!(output[0].content, "core policy");
        assert_eq!(output[1].role, "assistant");
        assert_eq!(output[1].content, "ack");
    }

    #[test]
    fn streaming_history_request_preserves_full_message_history() {
        let provider = make_provider("kimi-code", "https://api.kimi.com/coding/v1", Some("test-key"));
        let messages = vec![
            ChatMessage::system("system policy"),
            ChatMessage::user("first user"),
            ChatMessage::assistant("first answer"),
            ChatMessage::user("second user"),
        ];

        let request = provider
            .build_streaming_history_request(&messages, "kimi-k2", 1.0, StreamOptions::new(true))
            .expect("test: streaming request builds");
        let value = serde_json::to_value(super::super::parameter_wire::request(
            "kimi-code",
            super::super::parameter_wire::Format::Compatible,
            &request.model,
            &request,
        ))
        .unwrap();
        let serialized_messages = value["messages"].as_array().unwrap();

        assert_eq!(value["model"], "kimi-k2");
        // This historical model ID is unverified. Use its explicit base
        // temperature; real declared Kimi clamp coverage is separate.
        assert_eq!(value["temperature"], 1.0);
        assert_eq!(value["stream"], true);
        assert_eq!(serialized_messages.len(), 4);
        assert_eq!(serialized_messages[0]["role"], "system");
        assert_eq!(serialized_messages[0]["content"], "system policy");
        assert_eq!(serialized_messages[1]["role"], "user");
        assert_eq!(serialized_messages[1]["content"], "first user");
        assert_eq!(serialized_messages[2]["role"], "assistant");
        assert_eq!(serialized_messages[2]["content"], "first answer");
        assert_eq!(serialized_messages[3]["role"], "user");
        assert_eq!(serialized_messages[3]["content"], "second user");
    }

    #[test]
    fn streaming_history_request_includes_tools_when_provided() {
        let provider = make_provider("kimi-code", "https://api.kimi.com/coding/v1", Some("test-key"));
        let messages = vec![ChatMessage::user("inspect local system")];
        let tools = vec![crate::tools::ToolSpec {
            name: "shell".to_string(),
            description: "Execute terminal commands.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }),
        }];

        let request = provider
            .build_streaming_history_request(&messages, "kimi-k2", 0.3, StreamOptions::new(true).with_tools(tools))
            .expect("test: streaming request builds");
        let value = serde_json::to_value(&request).unwrap();
        let tools = value["tools"].as_array().unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(value["tool_choice"], "auto");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "shell");
        assert_eq!(tools[0]["function"]["parameters"]["required"][0], "command");
    }

    #[test]
    fn streaming_history_request_omits_tools_when_none() {
        let provider = make_provider("kimi-code", "https://api.kimi.com/coding/v1", Some("test-key"));
        let messages = vec![ChatMessage::user("hello")];

        let request = provider
            .build_streaming_history_request(&messages, "kimi-k2", 0.3, StreamOptions::new(true))
            .expect("test: streaming request builds");
        let value = serde_json::to_value(&request).unwrap();

        assert!(value["tools"].is_null());
        assert!(value["tool_choice"].is_null());
    }

    #[test]
    fn streaming_history_request_honors_merge_system_into_user() {
        let provider = OpenAiCompatibleProvider::new_merge_system_into_user(
            "minimax",
            "https://api.minimax.example/v1",
            Some("test-key"),
            AuthStyle::Bearer,
        );
        let messages = vec![ChatMessage::system("core policy"), ChatMessage::user("hello")];

        let request = provider
            .build_streaming_history_request(&messages, "abab", 0.7, StreamOptions::new(true))
            .expect("test: streaming request builds");
        let value = serde_json::to_value(&request).unwrap();
        let serialized_messages = value["messages"].as_array().unwrap();

        assert_eq!(serialized_messages.len(), 1);
        assert_eq!(serialized_messages[0]["role"], "user");
        assert_eq!(serialized_messages[0]["content"], "core policy\n\nhello");
    }

    #[test]
    fn strip_think_tags_drops_unclosed_block_suffix() {
        let input = "visible<think>hidden";
        assert_eq!(strip_think_tags(input), "visible");
    }

    #[test]
    fn native_tool_schema_unsupported_detection_is_precise() {
        assert!(OpenAiCompatibleProvider::is_native_tool_schema_unsupported(
            reqwest::StatusCode::BAD_REQUEST,
            "unknown parameter: tools"
        ));
        assert!(!OpenAiCompatibleProvider::is_native_tool_schema_unsupported(
            reqwest::StatusCode::UNAUTHORIZED,
            "unknown parameter: tools"
        ));
    }

    #[test]
    fn prompt_guided_tool_fallback_injects_system_instruction() {
        let input = vec![ChatMessage::user("check status")];
        let tools = vec![crate::tools::ToolSpec {
            name: "shell_exec".to_string(),
            description: "Execute shell command".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }),
        }];

        let output = OpenAiCompatibleProvider::with_prompt_guided_tool_instructions(&input, Some(&tools));
        assert!(!output.is_empty());
        assert_eq!(output[0].role, "system");
        assert!(output[0].content.contains("Available Tools"));
        assert!(output[0].content.contains("shell_exec"));
    }

    #[tokio::test]
    async fn warmup_without_key_is_noop() {
        let provider = make_provider("test", "https://example.com", None);
        let result = provider.warmup().await;
        assert!(result.is_ok());
    }

    // ══════════════════════════════════════════════════════════
    // Native tool calling tests
    // ══════════════════════════════════════════════════════════

    #[test]
    fn capabilities_reports_native_tool_calling() {
        let p = make_provider("test", "https://example.com", None);
        let caps = <OpenAiCompatibleProvider as Provider>::capabilities(&p);
        assert!(caps.native_tool_calling);
    }

    #[test]
    fn tool_specs_convert_to_openai_format() {
        let specs = vec![crate::tools::ToolSpec {
            name: "shell".to_string(),
            description: "Run shell command".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }),
        }];

        let tools = OpenAiCompatibleProvider::tool_specs_to_openai_format(&specs);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "shell");
        assert_eq!(tools[0]["function"]["description"], "Run shell command");
        assert_eq!(tools[0]["function"]["parameters"]["required"][0], "command");
    }

    #[test]
    fn request_serializes_with_tools() {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather for a location",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    }
                }
            }
        })];

        let req = ApiChatRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: "What is the weather?".to_string(),
            }],
            temperature: 0.7,
            stream: Some(false),
            tools: Some(tools),
            tool_choice: Some("auto".to_string()),
            stream_options: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"tools\""));
        assert!(json.contains("get_weather"));
        assert!(json.contains("\"tool_choice\":\"auto\""));
    }

    #[test]
    fn response_with_tool_calls_deserializes() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"location\":\"London\"}"
                        }
                    }]
                }
            }]
        }"#;

        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert!(msg.content.is_none());
        let tool_calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(
            tool_calls[0].function.as_ref().unwrap().name.as_deref(),
            Some("get_weather")
        );
        assert_eq!(
            tool_calls[0].function.as_ref().unwrap().arguments.as_deref(),
            Some("{\"location\":\"London\"}")
        );
    }

    #[test]
    fn response_with_multiple_tool_calls() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": "I'll check both.",
                    "tool_calls": [
                        {
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"location\":\"London\"}"
                            }
                        },
                        {
                            "type": "function",
                            "function": {
                                "name": "get_time",
                                "arguments": "{\"timezone\":\"UTC\"}"
                            }
                        }
                    ]
                }
            }]
        }"#;

        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some("I'll check both."));
        let tool_calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(
            tool_calls[0].function.as_ref().unwrap().name.as_deref(),
            Some("get_weather")
        );
        assert_eq!(
            tool_calls[1].function.as_ref().unwrap().name.as_deref(),
            Some("get_time")
        );
    }

    #[tokio::test]
    async fn chat_with_tools_fails_without_key() {
        let p = make_provider("TestProvider", "https://example.com", None);
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }];
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "test_tool",
                "description": "A test tool",
                "parameters": {}
            }
        })];

        let result = p.chat_with_tools(&messages, &tools, "model", 0.7).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("TestProvider API key not set"));
    }

    #[test]
    fn response_with_no_tool_calls_has_empty_vec() {
        let json = r#"{"choices":[{"message":{"content":"Just text, no tools."}}]}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some("Just text, no tools."));
        assert!(msg.tool_calls.is_none());
    }

    #[test]
    fn flatten_system_messages_merges_into_first_user_and_removes_system_roles() {
        let messages = vec![
            ChatMessage::system("System A"),
            ChatMessage::assistant("Earlier assistant turn"),
            ChatMessage::system("System B"),
            ChatMessage::user("User turn"),
            ChatMessage::tool(r#"{"ok":true}"#),
        ];

        let flattened = OpenAiCompatibleProvider::flatten_system_messages(&messages);
        assert_eq!(flattened.len(), 3);
        assert_eq!(flattened[0].role, "assistant");
        assert_eq!(flattened[1].content, "System A\n\nSystem B\n\nUser turn".to_string());
        assert_eq!(flattened[1].role, "user");
        assert_eq!(flattened[2].role, "tool");
        assert!(!flattened.iter().any(|m| m.role == "system"));
    }

    #[test]
    fn flatten_system_messages_inserts_synthetic_user_when_no_user_exists() {
        let messages = vec![
            ChatMessage::assistant("Assistant only"),
            ChatMessage::system("Synthetic system"),
        ];

        let flattened = OpenAiCompatibleProvider::flatten_system_messages(&messages);
        assert_eq!(flattened.len(), 2);
        assert_eq!(flattened[0].role, "user");
        assert_eq!(flattened[0].content, "Synthetic system");
        assert_eq!(flattened[1].role, "assistant");
    }

    #[test]
    fn strip_think_tags_removes_multiple_blocks_with_surrounding_text() {
        let input = "Answer A <think>hidden 1</think> and B <think>hidden 2</think> done";
        let output = strip_think_tags(input);
        assert_eq!(output, "Answer A  and B  done");
    }

    #[test]
    fn strip_think_tags_drops_tail_for_unclosed_block() {
        let input = "Visible<think>hidden tail";
        let output = strip_think_tags(input);
        assert_eq!(output, "Visible");
    }

    // ----------------------------------------------------------
    // Reasoning vs visible-content separation tests
    // ----------------------------------------------------------

    #[test]
    fn reasoning_kept_separate_when_content_empty() {
        // Reasoning models (Qwen3, GLM-4) return content: "" with reasoning_content populated.
        // The two streams MUST stay independent — never merged into visible_content.
        let json = r#"{"choices":[{"message":{"content":"","reasoning_content":"Thinking output here"}}]}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.visible_content(), "");
        assert_eq!(msg.reasoning().as_deref(), Some("Thinking output here"));
    }

    #[test]
    fn reasoning_kept_separate_when_content_null() {
        let json = r#"{"choices":[{"message":{"content":null,"reasoning_content":"Fallback text"}}]}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.visible_content(), "");
        assert_eq!(msg.reasoning().as_deref(), Some("Fallback text"));
    }

    #[test]
    fn reasoning_kept_separate_when_content_missing() {
        let json = r#"{"choices":[{"message":{"reasoning_content":"Only reasoning"}}]}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.visible_content(), "");
        assert_eq!(msg.reasoning().as_deref(), Some("Only reasoning"));
    }

    #[test]
    fn visible_content_preferred_when_present_reasoning_still_exposed() {
        let json = r#"{"choices":[{"message":{"content":"Normal response","reasoning_content":"Sidebar"}}]}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        // visible_content returns just the visible bit.
        assert_eq!(msg.visible_content(), "Normal response");
        // reasoning() still exposes the sidebar for history reconstruction.
        assert_eq!(msg.reasoning().as_deref(), Some("Sidebar"));
    }

    #[test]
    fn inline_think_tags_extracted_to_reasoning_not_visible() {
        // MiniMax-style: <think>...</think> embedded in content. The tag content
        // must be lifted into reasoning(), and visible_content must NOT contain it.
        let json =
            r#"{"choices":[{"message":{"content":"<think>secret</think>","reasoning_content":"Fallback text"}}]}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.visible_content(), "");
        assert_eq!(msg.visible_content_optional(), None);
        // reasoning() joins inline <think> body with dedicated reasoning_content.
        let reasoning = msg.reasoning().expect("test: reasoning must be present");
        assert!(
            reasoning.contains("secret"),
            "missing inline <think> body in reasoning: {reasoning}"
        );
        assert!(
            reasoning.contains("Fallback text"),
            "missing reasoning_content in reasoning: {reasoning}"
        );
    }

    #[test]
    fn both_absent_returns_empty_visible_and_no_reasoning() {
        let json = r#"{"choices":[{"message":{}}]}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.visible_content(), "");
        assert!(msg.reasoning().is_none());
    }

    #[test]
    fn reasoning_none_when_normal_model_response() {
        let json = r#"{"choices":[{"message":{"content":"Hello from Venice!"}}]}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        let msg = &resp.choices[0].message;
        assert!(msg.reasoning_content.is_none());
        assert_eq!(msg.visible_content(), "Hello from Venice!");
        assert!(msg.reasoning().is_none());
    }

    #[test]
    fn extract_think_tags_returns_inline_reasoning_body() {
        let extracted = extract_think_tags("visible<think>hidden reasoning</think> after");
        assert_eq!(extracted.as_deref(), Some("hidden reasoning"));
    }

    #[test]
    fn extract_think_tags_concats_multiple_blocks() {
        let extracted = extract_think_tags("a<think>one</think>b<think>two</think>c");
        assert_eq!(extracted.as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn extract_think_tags_returns_none_when_no_blocks() {
        assert!(extract_think_tags("no tags here").is_none());
    }

    // ----------------------------------------------------------
    // SSE streaming reasoning_content fallback tests
    // ----------------------------------------------------------

    #[test]
    fn parse_sse_line_with_content() {
        let line = r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#;
        let result = parse_sse_line(line)
            .expect("test: parse should succeed")
            .expect("test: should produce delta");
        assert_eq!(result.content.as_deref(), Some("hello"));
        assert_eq!(result.reasoning, None);
    }

    #[test]
    fn compatible_sse_line_parses_final_usage_chunk() {
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":21,"completion_tokens":8,"total_tokens":29}}"#;
        let event = parse_sse_line(line)
            .expect("test: parse should succeed")
            .expect("test: usage-only chunk should produce event");
        let usage = event.usage.expect("usage expected");

        assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Reported);
        assert_eq!(usage.prompt_tokens, Some(21));
        assert_eq!(usage.completion_tokens, Some(8));
        assert_eq!(usage.total_tokens, Some(29));
        assert!(event.content.is_none());
        assert!(event.tool_call_deltas.is_empty());
    }

    #[test]
    fn compatible_malformed_usage_is_ignored_without_breaking_content() {
        let line = r#"data: {"choices":[{"delta":{"content":"still streams"}}],"usage":{"prompt_tokens":"bad","completion_tokens":8}}"#;
        let event = parse_sse_line(line)
            .expect("malformed usage fields should not fail SSE parsing")
            .expect("content event expected");

        assert_eq!(event.content.as_deref(), Some("still streams"));
        assert!(
            event.usage.is_none(),
            "partial/malformed usage must fall back to estimate outside the parser"
        );
    }

    #[test]
    fn compatible_stream_options_rejection_statuses_retry_without_usage_request() {
        assert!(OpenAiCompatibleProvider::stream_options_retry_status(
            reqwest::StatusCode::BAD_REQUEST
        ));
        assert!(OpenAiCompatibleProvider::stream_options_retry_status(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        ));
        assert!(OpenAiCompatibleProvider::stream_options_retry_status(
            reqwest::StatusCode::NOT_FOUND
        ));
        assert!(!OpenAiCompatibleProvider::stream_options_retry_status(
            reqwest::StatusCode::UNAUTHORIZED
        ));
        assert!(!OpenAiCompatibleProvider::stream_options_retry_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
    }

    #[test]
    fn compatible_sse_without_usage_stays_estimated_fallback() {
        let line = r#"data: {"choices":[{"delta":{"content":"fallback estimate"}}]}"#;
        let event = parse_sse_line(line)
            .expect("test: parse should succeed")
            .expect("test: should produce content event");
        assert!(
            event.usage.is_none(),
            "content chunks must not fabricate reported usage"
        );

        let accumulator = crate::llm::route_decision::ProviderUsageAccumulator::new();
        let usage =
            accumulator.finish_or_estimate_completion_chars(event.content.as_deref().unwrap_or("").chars().count());
        assert_eq!(usage.source, crate::llm::route_decision::TokenUsageSource::Estimated);
        assert_eq!(usage.prompt_tokens, None);
        assert!(usage.completion_tokens.unwrap_or_default() > 0);
    }

    #[test]
    fn parse_sse_line_with_reasoning_content_goes_to_reasoning_field_only() {
        let line = r#"data: {"choices":[{"delta":{"reasoning_content":"thinking..."}}]}"#;
        let result = parse_sse_line(line)
            .expect("test: parse should succeed")
            .expect("test: should produce delta");
        // Reasoning must NOT be merged into content: prevents thinking leaking into chat output.
        assert_eq!(result.content, None);
        assert_eq!(result.reasoning.as_deref(), Some("thinking..."));
    }

    #[test]
    fn parse_sse_line_with_both_keeps_them_separate() {
        let line = r#"data: {"choices":[{"delta":{"content":"real answer","reasoning_content":"thinking..."}}]}"#;
        let result = parse_sse_line(line)
            .expect("test: parse should succeed")
            .expect("test: should produce delta");
        // Both fields should be surfaced independently — consumer chooses.
        assert_eq!(result.content.as_deref(), Some("real answer"));
        assert_eq!(result.reasoning.as_deref(), Some("thinking..."));
    }

    #[test]
    fn parse_sse_line_with_empty_content_still_surfaces_reasoning_separately() {
        let line = r#"data: {"choices":[{"delta":{"content":"","reasoning_content":"thinking..."}}]}"#;
        let result = parse_sse_line(line)
            .expect("test: parse should succeed")
            .expect("test: should produce delta");
        // Empty content is treated as None, reasoning travels in its own field.
        assert_eq!(result.content, None);
        assert_eq!(result.reasoning.as_deref(), Some("thinking..."));
    }

    #[test]
    fn parse_sse_line_done_sentinel() {
        let line = "data: [DONE]";
        let event = parse_sse_line(line)
            .expect("test: parse should succeed")
            .expect("test: [DONE] should produce a sentinel event");
        assert!(event.done_sentinel);
        assert!(event.content.is_none());
        assert!(event.reasoning.is_none());
        assert!(event.tool_call_deltas.is_empty());
        assert!(event.finish_reason.is_none());
    }

    #[test]
    fn stream_chunk_reasoning_does_not_pollute_delta_text() {
        // Direct verification: building a reasoning_delta chunk leaves visible delta empty.
        let chunk = StreamChunk::reasoning_delta("internal monologue");
        assert!(
            chunk.delta.is_empty(),
            "visible delta must stay empty for reasoning-only chunk"
        );
        assert_eq!(chunk.reasoning.as_deref(), Some("internal monologue"));
        assert!(chunk.is_reasoning_only());
    }

    // ----------------------------------------------------------
    // S3 T3-2-D: tool_calls SSE streaming (Cohere / Groq / Mistral
    // share this parser via OpenAiCompatibleProvider).
    // ----------------------------------------------------------

    /// Mock SSE sequence for a single-tool call: open frame + 3 argument
    /// fragments + terminal `finish_reason = "tool_calls"`. Verifies the
    /// parser emits one leading `Streaming` open chunk (empty delta), one
    /// `Streaming` chunk per argument fragment, and one terminal `Completed`
    /// chunk carrying the concatenated JSON.
    #[test]
    fn test_compatible_tool_calls_streaming_emits_incremental_chunks() {
        let mut buf: Vec<ToolCallBuffer> = Vec::new();
        let mut emitted: Vec<ToolCallChunk> = Vec::new();

        // Frame 1: id + name, no arguments yet
        let frame1 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"search","arguments":""}}]}}]}"#;
        let event = parse_sse_line(frame1)
            .expect("frame1 parses")
            .expect("frame1 yields event");
        emitted.extend(apply_tool_call_deltas(&mut buf, event.tool_call_deltas).expect("test: in-range index"));

        // Frame 2-4: argument fragments. The fragments are the raw decoded
        // strings the parser will surface (each piece concatenates into
        // `{"q":"hello"}`). The on-the-wire JSON encodes the inner quotes as
        // `\"` — `serde_json::to_string` handles that for us.
        let fragments = [r#"{"q"#, r#"":"hello"#, r#""}"#];
        for fragment in fragments {
            let arguments_json = serde_json::to_string(fragment).expect("test: arg json encode");
            let line = format!(
                r#"data: {{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"function":{{"arguments":{arguments_json}}}}}]}}}}]}}"#,
            );
            let ev = parse_sse_line(&line)
                .expect("fragment parses")
                .expect("fragment yields event");
            emitted.extend(apply_tool_call_deltas(&mut buf, ev.tool_call_deltas).expect("test: in-range index"));
        }

        // Frame 5: finish_reason flushes Completed
        let finish = r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        let ev = parse_sse_line(finish)
            .expect("finish parses")
            .expect("finish yields event");
        assert!(ev.tool_call_deltas.is_empty());
        assert_eq!(ev.finish_reason.as_deref(), Some("tool_calls"));
        let completed = flush_tool_call_buffer(&buf);
        emitted.extend(completed);

        // Expect: 1 open Streaming + 3 fragment Streaming + 1 Completed = 5
        assert_eq!(emitted.len(), 5, "should emit 1 open + 3 fragments + 1 completed");

        // Leading open chunk: empty arguments_delta, Streaming status.
        assert_eq!(emitted[0].status, ToolCallChunkStatus::Streaming);
        assert_eq!(emitted[0].id, "call_abc");
        assert_eq!(emitted[0].name, "search");
        assert_eq!(emitted[0].arguments_delta.as_deref(), Some(""));
        assert_eq!(emitted[0].args, "");
        assert_eq!(emitted[0].index, 0);

        // Argument fragments
        assert_eq!(emitted[1].status, ToolCallChunkStatus::Streaming);
        assert_eq!(emitted[1].arguments_delta.as_deref(), Some(fragments[0]));
        assert_eq!(emitted[2].arguments_delta.as_deref(), Some(fragments[1]));
        assert_eq!(emitted[3].arguments_delta.as_deref(), Some(fragments[2]));

        // Terminal Completed chunk
        let last = &emitted[4];
        assert_eq!(last.status, ToolCallChunkStatus::Completed);
        assert_eq!(last.id, "call_abc");
        assert_eq!(last.name, "search");
        assert_eq!(last.index, 0);
        assert!(last.arguments_delta.is_none());
        // Concatenated arguments equal the join of all fragments.
        assert_eq!(last.args, r#"{"q":"hello"}"#);
    }

    /// Two concurrent tool calls (index 0 + 1) interleaving their argument
    /// fragments across SSE frames. Verifies the index-keyed buffer keeps
    /// them isolated and emits a separate `Completed` per index.
    #[test]
    fn test_compatible_tool_calls_concurrent_indices() {
        let mut buf: Vec<ToolCallBuffer> = Vec::new();
        let mut emitted: Vec<ToolCallChunk> = Vec::new();

        // Frame 1: open call #0 (search)
        let f1 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_0","type":"function","function":{"name":"search","arguments":""}}]}}]}"#;
        let ev = parse_sse_line(f1).expect("ok").expect("event");
        emitted.extend(apply_tool_call_deltas(&mut buf, ev.tool_call_deltas).expect("test: in-range index"));

        // Frame 2: open call #1 (lookup) + fragment for #0 in same frame
        let f2 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_1","type":"function","function":{"name":"lookup","arguments":""}},{"index":0,"function":{"arguments":"{\"q\":1}"}}]}}]}"#;
        let ev = parse_sse_line(f2).expect("ok").expect("event");
        emitted.extend(apply_tool_call_deltas(&mut buf, ev.tool_call_deltas).expect("test: in-range index"));

        // Frame 3: fragment for #1
        let f3 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"k\":2}"}}]}}]}"#;
        let ev = parse_sse_line(f3).expect("ok").expect("event");
        emitted.extend(apply_tool_call_deltas(&mut buf, ev.tool_call_deltas).expect("test: in-range index"));

        // Flush
        let finish = r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        let ev = parse_sse_line(finish).expect("ok").expect("event");
        assert_eq!(ev.finish_reason.as_deref(), Some("tool_calls"));
        let completed = flush_tool_call_buffer(&buf);

        // Streaming emissions so far: open #0, open #1, fragment #0, fragment #1 = 4.
        assert_eq!(emitted.len(), 4);
        assert!(emitted.iter().all(|c| c.status == ToolCallChunkStatus::Streaming));

        // Indices preserved through interleaving.
        assert_eq!(emitted[0].index, 0);
        assert_eq!(emitted[0].name, "search");
        assert_eq!(emitted[1].index, 1);
        assert_eq!(emitted[1].name, "lookup");
        assert_eq!(emitted[2].index, 0);
        assert_eq!(emitted[2].arguments_delta.as_deref(), Some(r#"{"q":1}"#));
        assert_eq!(emitted[3].index, 1);
        assert_eq!(emitted[3].arguments_delta.as_deref(), Some(r#"{"k":2}"#));

        // Two terminal Completed chunks, one per index, with isolated args.
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0].index, 0);
        assert_eq!(completed[0].id, "call_0");
        assert_eq!(completed[0].name, "search");
        assert_eq!(completed[0].args, r#"{"q":1}"#);
        assert_eq!(completed[0].status, ToolCallChunkStatus::Completed);
        assert_eq!(completed[1].index, 1);
        assert_eq!(completed[1].id, "call_1");
        assert_eq!(completed[1].name, "lookup");
        assert_eq!(completed[1].args, r#"{"k":2}"#);
        assert_eq!(completed[1].status, ToolCallChunkStatus::Completed);
    }

    /// Regression: the previous SSE line splitter ran `buffer.drain(..=pos)`
    /// followed by `buffer = buffer[pos+1..].to_string()`. After the drain the
    /// buffer is already shortened to `len - (pos+1)`, so the re-slice was
    /// indexing into a dangling range and would panic. The minimal trigger is
    /// a buffer that contains exactly one byte — the newline terminator: after
    /// `drain` the buffer is empty, and `buffer[1..]` is out of bounds.
    ///
    /// This test replays that splitter contract directly (no provider, no I/O)
    /// to guarantee the fix stays in place: drain alone is sufficient.
    #[test]
    fn test_compatible_sse_line_splitter_no_panic_on_lone_newline() {
        // Replay the exact splitter logic used in `stream_chat_with_system`.
        fn split_lines(buffer: &mut String) -> Vec<String> {
            let mut out = Vec::new();
            while let Some(pos) = buffer.find('\n') {
                let line: String = buffer.drain(..=pos).collect();
                out.push(line);
            }
            out
        }

        // Case 1: lone newline — previously panicked because `buffer[1..]` on
        // an empty buffer is out of bounds.
        let mut b1 = String::from("\n");
        let lines = split_lines(&mut b1);
        assert_eq!(lines, vec!["\n".to_string()]);
        assert!(b1.is_empty(), "lone newline must fully drain the buffer");

        // Case 2: two complete lines back-to-back. The bug would corrupt the
        // remainder between iterations even when no panic was triggered.
        let mut b2 = String::from("a\nb\n");
        let lines = split_lines(&mut b2);
        assert_eq!(lines, vec!["a\n".to_string(), "b\n".to_string()]);
        assert!(b2.is_empty(), "two complete lines must fully drain");

        // Case 3: one complete line + partial remainder. The partial must be
        // preserved verbatim for the next chunk to concatenate.
        let mut b3 = String::from("first\npartial-without-newline");
        let lines = split_lines(&mut b3);
        assert_eq!(lines, vec!["first\n".to_string()]);
        assert_eq!(b3, "partial-without-newline");

        // Case 4: multi-byte UTF-8 right before the newline. `drain(..=pos)`
        // is byte-indexed; the splitter must remain panic-free even when the
        // last code point uses 3 bytes (e.g. `\u{20ac}`).
        let mut b4 = String::from("\u{20ac}\n\u{20ac}\n");
        let lines = split_lines(&mut b4);
        assert_eq!(lines, vec!["\u{20ac}\n".to_string(), "\u{20ac}\n".to_string()]);
        assert!(b4.is_empty(), "expected an empty value");
    }

    #[tokio::test]
    async fn compatible_streaming_rejects_malformed_non_sse_success_body() {
        use axum::Router;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn malformed() -> &'static str {
            "{not valid json"
        }

        let app = Router::new().route("/chat/completions", post(malformed));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let provider = make_provider("custom", &format!("http://{addr}"), Some("test-key"));
        let messages = vec![ChatMessage::user("hi".to_string())];
        let mut stream = provider.stream_chat_with_history(&messages, "test-model", 0.0, StreamOptions::new(false));
        let first = stream.next().await.expect("chunk");
        let err = first.expect_err("malformed 200 response must not become empty success");
        let msg = err.to_string();
        assert!(msg.contains("Invalid SSE format"), "got: {msg}");
        assert!(msg.contains("incomplete trailing data"), "got: {msg}");
    }

    /// Defensive: a tool-call delta arriving with only `id` (no `name`) must
    /// not emit any Streaming chunk until the name is observed. Mirrors
    /// permissive parsing of out-of-order Cohere/Groq frames.
    #[test]
    fn test_compatible_tool_call_delta_without_name_holds_streaming_chunk() {
        let mut buf: Vec<ToolCallBuffer> = Vec::new();

        let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x"}]}}]}"#;
        let ev = parse_sse_line(line).expect("ok").expect("event");
        let emitted = apply_tool_call_deltas(&mut buf, ev.tool_call_deltas).expect("test: in-range index");
        assert!(emitted.is_empty(), "no name => no streaming chunk yet");
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0].id, "call_x");
        assert!(buf[0].name.is_empty(), "expected an empty value");
        assert!(!buf[0].opened);

        // Now name arrives; expect the leading open chunk.
        let line2 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"fetch"}}]}}]}"#;
        let ev2 = parse_sse_line(line2).expect("ok").expect("event");
        let emitted2 = apply_tool_call_deltas(&mut buf, ev2.tool_call_deltas).expect("test: in-range index");
        assert_eq!(emitted2.len(), 1);
        assert_eq!(emitted2[0].name, "fetch");
        assert_eq!(emitted2[0].id, "call_x");
        assert_eq!(emitted2[0].status, ToolCallChunkStatus::Streaming);
        assert_eq!(emitted2[0].arguments_delta.as_deref(), Some(""));
    }

    /// STREAM-f: loopback stub answering `/chat/completions` with `body` as SSE.
    async fn compatible_stream_results(body: &'static str) -> Vec<StreamResult<StreamChunk>> {
        use axum::{Router, http::header, routing::post};
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/chat/completions",
            post(move || async move { ([(header::CONTENT_TYPE, "text/event-stream")], body) }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let provider = make_provider("custom", &format!("http://{addr}"), Some("test-key"));
        let messages = vec![ChatMessage::user("write x".to_string())];
        let results = provider
            .stream_chat_with_history(&messages, "test-model", 0.0, StreamOptions::new(true))
            .collect()
            .await;
        server.abort();
        results
    }

    fn compatible_completed_calls(results: &[StreamResult<StreamChunk>]) -> Vec<&ToolCallChunk> {
        results
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .flat_map(|c| c.tool_calls.iter())
            .filter(|c| c.status == ToolCallChunkStatus::Completed)
            .collect()
    }

    /// STREAM-f: a body cut in the middle of a call's arguments, with no
    /// `[DONE]`, must fail the stream instead of flushing the half call.
    #[tokio::test]
    async fn compatible_stream_cut_mid_tool_arguments_ends_in_an_error() {
        let results = compatible_stream_results(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_s1\",",
            "\"function\":{\"name\":\"fixture_write\",\"arguments\":\"{\\\"value\\\":\"}}]}}]}\n\n",
        ))
        .await;
        let error = results
            .last()
            .and_then(|r| r.as_ref().err())
            .expect("test: the stream must end in an error");
        assert!(matches!(error, StreamError::InvalidSse(_)), "got {error:?}");
        assert!(error.to_string().contains("[DONE]"), "got {error}");
        assert!(
            compatible_completed_calls(&results).is_empty(),
            "the half call must not be flushed"
        );
        assert!(!results.iter().filter_map(|r| r.as_ref().ok()).any(|c| c.is_final));
    }

    /// STREAM-f guard: an endpoint that omits `finish_reason` but ends the
    /// stream properly with `[DONE]` still has its calls flushed.
    #[tokio::test]
    async fn compatible_stream_done_without_finish_reason_still_flushes_calls() {
        let results = compatible_stream_results(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_s1\",",
            "\"function\":{\"name\":\"fixture_write\",\"arguments\":\"{\\\"value\\\":\\\"x\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        assert!(results.iter().all(Result::is_ok), "no error on a complete stream");
        let completed = compatible_completed_calls(&results);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].args, r#"{"value":"x"}"#);
    }

    /// STREAM-g (F-2): `length` or `content_filter` while a call is buffered
    /// means the call is unfinished, even when `[DONE]` follows; flushing it
    /// would run a name-only call with `{}`.
    #[tokio::test]
    async fn compatible_stream_length_or_filter_with_a_buffered_call_ends_in_an_error() {
        for body in [
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_s1\",",
                "\"function\":{\"name\":\"fixture_write\",\"arguments\":\"\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
                "data: [DONE]\n\n",
            ),
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_s1\",",
                "\"function\":{\"name\":\"fixture_write\",\"arguments\":\"\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n",
                "data: [DONE]\n\n",
            ),
        ] {
            let results = compatible_stream_results(body).await;
            let error = results
                .iter()
                .find_map(|r| r.as_ref().err())
                .expect("test: an unfinished call must fail the stream");
            assert!(matches!(error, StreamError::Provider(_)), "got {error:?}");
            assert!(
                compatible_completed_calls(&results).is_empty(),
                "the call must not be flushed"
            );
        }
    }

    /// COMPAT-STREAM (R-2): an in-stream `{"error":…}` object is a stream error,
    /// even when `[DONE]` follows. It used to parse as an empty chunk, and the
    /// defensive end-of-stream flush then ran the name-only call with `{}`.
    #[tokio::test]
    async fn compatible_stream_error_object_mid_call_ends_in_a_provider_error() {
        let results = compatible_stream_results(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_s1\",",
            "\"function\":{\"name\":\"fixture_write\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"error\":{\"message\":\"The server is overloaded\",\"type\":\"server_error\"}}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let error = results
            .iter()
            .find_map(|r| r.as_ref().err())
            .expect("test: the upstream error must surface");
        assert!(matches!(error, StreamError::Provider(_)), "got {error:?}");
        let message = error.to_string();
        assert!(message.contains("server_error"), "got {message}");
        assert!(message.contains("The server is overloaded"), "got {message}");
        assert!(
            compatible_completed_calls(&results).is_empty(),
            "the call must not be flushed"
        );
    }

    /// COMPAT-STREAM (R-2): a call that got its name but no argument bytes, then
    /// `finish_reason: stop` and `[DONE]`, must not be flushed as `{}`. Mirrors
    /// the direct OpenAI rule.
    #[tokio::test]
    async fn compatible_stream_stop_with_a_name_only_call_ends_in_an_error() {
        let results = compatible_stream_results(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_s1\",",
            "\"function\":{\"name\":\"fixture_write\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let error = results
            .iter()
            .find_map(|r| r.as_ref().err())
            .expect("test: a name-only call ended by stop must fail the stream");
        assert!(error.to_string().contains("incomplete"), "got {error}");
        assert!(
            compatible_completed_calls(&results).is_empty(),
            "expected an empty value"
        );
    }

    /// Guard: a complete call ended by `stop` (a server quirk) still runs.
    #[tokio::test]
    async fn compatible_stream_stop_with_a_complete_call_still_flushes_it() {
        let results = compatible_stream_results(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_s1\",",
            "\"function\":{\"name\":\"fixture_write\",\"arguments\":\"{\\\"value\\\":\\\"x\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        assert!(results.iter().all(Result::is_ok));
        let completed = compatible_completed_calls(&results);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].args, r#"{"value":"x"}"#);
    }

    /// COMPAT-STREAM: the server-controlled tool-call index sizes the buffer,
    /// so an index past the cap ends the stream instead of allocating up to it.
    #[tokio::test]
    async fn compatible_stream_rejects_an_out_of_range_tool_call_index() {
        let results = compatible_stream_results(concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1000,\"id\":\"call_far\",",
            "\"function\":{\"name\":\"fixture_write\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let error = results
            .iter()
            .find_map(|r| r.as_ref().err())
            .expect("test: an out-of-range index must end the stream");
        assert!(matches!(error, StreamError::InvalidSse(_)), "got {error:?}");
        assert!(error.to_string().contains("1000"), "got {error}");
        assert!(
            compatible_completed_calls(&results).is_empty(),
            "expected an empty value"
        );
    }

    /// COMPAT-STREAM: once the consumer drops the stream, the background task
    /// lets go of the connection instead of waiting on a silent server.
    #[tokio::test]
    async fn compatible_stream_releases_the_connection_when_the_consumer_goes_away() {
        let (base, released, server) =
            crate::providers::reliable::stream_release_probe::spawn_silent_after_first_record_stub(
                "/chat/completions",
                "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            )
            .await;
        let provider = make_provider("custom", &base, Some("test-key"));
        let messages = vec![ChatMessage::user("hi".to_string())];
        let mut stream = provider.stream_chat_with_history(&messages, "test-model", 0.0, StreamOptions::new(true));
        let first = stream.next().await.expect("test: first chunk");
        assert_eq!(first.expect("test: first chunk is ok").delta, "hel");
        drop(stream);
        let was_released = crate::providers::reliable::stream_release_probe::wait_released(&released).await;
        server.abort();
        assert!(
            was_released,
            "the provider task must close the connection once nobody reads the stream"
        );
    }

    /// COMPATf (N-1): a name-only call with no `finish_reason`, then `[DONE]`,
    /// must fail the stream, alone or next to a complete call. The defensive
    /// flush used to run it as `{}`. Parity with direct OpenAI.
    #[tokio::test]
    async fn compatible_stream_done_without_finish_reason_refuses_a_name_only_call() {
        for body in [
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_s1\",",
                "\"function\":{\"name\":\"fixture_write\",\"arguments\":\"\"}}]}}]}\n\n",
                "data: [DONE]\n\n",
            ),
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
                "{\"index\":0,\"id\":\"call_ok\",\"function\":{\"name\":\"fixture_write\",\"arguments\":\"{\\\"value\\\":\\\"x\\\"}\"}},",
                "{\"index\":1,\"id\":\"call_s1\",\"function\":{\"name\":\"fixture_write\",\"arguments\":\"\"}}",
                "]}}]}\n\n",
                "data: [DONE]\n\n",
            ),
        ] {
            let results = compatible_stream_results(body).await;
            let error = results
                .iter()
                .find_map(|r| r.as_ref().err())
                .expect("test: a name-only call without finish_reason must fail the stream");
            assert!(error.to_string().contains("incomplete"), "got {error}");
            assert!(
                compatible_completed_calls(&results).is_empty(),
                "no call may be flushed"
            );
        }
    }
}
