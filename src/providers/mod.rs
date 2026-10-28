//! Provider subsystem for model inference backends.
//!
//! This module implements the factory pattern for AI model providers. Each provider
//! implements the [`Provider`] trait defined in [`traits`], and is registered in the
//! factory function [`create_provider`] by its canonical string key (e.g., `"openai"`,
//! `"anthropic"`, `"ollama"`, `"gemini"`). Provider aliases are resolved internally
//! so that user-facing keys remain stable.
//!
//! The subsystem supports resilient multi-provider configurations through the
//! [`ReliableProvider`](reliable::ReliableProvider) wrapper, which handles fallback
//! chains and automatic retry. Model routing across providers is available via
//! [`create_routed_provider_for`].
//!
//! # Extension
//!
//! To add a new provider, implement [`Provider`] in a new submodule and register it
//! in [`create_provider_with_url`]. See `AGENTS.md` §7.1 for the full change playbook.

pub mod anthropic;
pub(crate) mod attempt_accounting;
pub mod bedrock;
pub mod catalog;
pub mod compatible;
pub mod connection_test;
pub mod copilot;
pub mod credentials;
pub mod gemini;
pub(crate) mod generation;
mod identity;
pub mod model_discovery;
pub(crate) mod model_settings;
pub mod ollama;
pub mod openai;
pub mod openai_codex;
pub mod openrouter;
pub(crate) mod parameter_capture;
pub(crate) mod parameter_wire;
pub mod rate_limit;
pub mod reliable;
pub mod router;
pub mod selection;
pub mod traits;
pub mod utf8_stream;

mod aliases;
mod credential_resolution;
mod errors;
mod info;
mod oauth;
mod redaction;
mod runtime_options;

#[cfg(test)]
pub(crate) mod credential_probe;
#[cfg(test)]
pub(crate) mod entrypoint_consistency_tests;
#[cfg(test)]
pub(crate) mod label_redaction_tests;
#[cfg(test)]
pub(crate) mod test_fixture;
#[cfg(test)]
mod tool_dialect_config_tests;
#[cfg(test)]
pub(crate) mod tool_schema_refusal_tests;

pub use attempt_accounting::AccountingTargets;
#[allow(unused_imports)]
pub use traits::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, Provider, ProviderCapabilities,
    ProviderCapabilityError, ToolCall, ToolResultMessage,
};

pub(crate) use identity::canonical_auth_profile_provider;
use identity::split_byo_provider;
pub(crate) use identity::{
    ByoDialect, ProviderConnection, ProviderIdentity, byo_dialect_of, canonical_provider_key, classify_provider,
    configured_connection_for, credential_for_provider_identity, scoped_reliability,
};
pub use identity::{ProviderId, ProviderNamespace, default_provider_for_unset};

pub(crate) use aliases::*;
pub(crate) use credential_resolution::*;
pub use errors::*;
pub use info::*;
pub(crate) use oauth::*;
pub use redaction::*;
pub use runtime_options::*;

use crate::auth::discovery::is_claude_code_oauth_setup_token;
use crate::auth::{profiles::AuthProfilesStore, select_profile_id};
use compatible::{AuthStyle, OpenAiCompatibleProvider};
use reliable::ReliableProvider;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const MAX_API_ERROR_CHARS: usize = 200;

/// How many characters of a refused, model-supplied provider string the
/// refusal warning prints.
const REFUSED_PROVIDER_LOG_CHARS: usize = 200;

/// The provider identities the operator configured.
///
/// A model may name a provider per call (`delegate`, `sessions_spawn`), and it
/// may name only one of these: the default provider, the fallback providers,
/// the `model_routes` providers and the named agents' providers. A
/// model-supplied `custom:<url>` that is none of them is refused before any
/// provider is built, so a prompt cannot direct a request, or a credential, to
/// a host the operator never chose. Membership is by provider identity
/// (`canonical_provider_key`), so every spelling of a configured provider is
/// that provider.
#[derive(Debug, Clone, Default)]
pub struct ConfiguredProviders {
    names: Vec<String>,
}

impl ConfiguredProviders {
    /// Every provider the operator configured in `config`.
    #[must_use]
    pub fn from_config(config: &crate::config::Config) -> Self {
        let default_provider = config
            .default_provider
            .as_deref()
            .unwrap_or_else(default_provider_for_unset);
        Self::default()
            .with_providers(std::iter::once(default_provider))
            .with_providers(config.reliability.fallback_providers.iter().map(String::as_str))
            .with_providers(config.model_routes.iter().map(|route| route.provider.as_str()))
            .with_providers(config.agents.values().map(|agent| agent.provider.as_str()))
    }

    /// Add more configured providers; blank names are ignored.
    #[must_use]
    pub fn with_providers<'a>(mut self, names: impl IntoIterator<Item = &'a str>) -> Self {
        for name in names {
            let trimmed = name.trim();
            if !trimmed.is_empty() && !self.permits(trimmed) {
                self.names.push(trimmed.to_string());
            }
        }
        self
    }

    /// The union of two configured sets.
    #[must_use]
    pub fn union(self, other: &Self) -> Self {
        self.with_providers(other.names.iter().map(String::as_str))
    }

    /// Whether `name` is one of the configured provider identities.
    #[must_use]
    pub fn permits(&self, name: &str) -> bool {
        let key = canonical_provider_key(name);
        self.names
            .iter()
            .any(|configured| canonical_provider_key(configured) == key)
    }

    /// Refuse a per-call provider the operator did not configure.
    ///
    /// The message is returned to the model, so it names provider identities
    /// only: catalog names, and a count of the operator's own endpoints. No
    /// URL, host or port of a configured endpoint, or of the refused one,
    /// reaches the model. The full configured list (userinfo redacted) is
    /// logged for the operator.
    pub fn ensure_permits(&self, requested: &str) -> anyhow::Result<()> {
        use std::fmt::Write as _;
        if self.permits(requested) {
            return Ok(());
        }
        let mut catalog: Vec<&str> = Vec::new();
        let mut endpoints = 0usize;
        for name in &self.names {
            match classify_provider(name) {
                ProviderIdentity::Catalog(canonical) => {
                    if !catalog.contains(&canonical) {
                        catalog.push(canonical);
                    }
                }
                _ => endpoints += 1,
            }
        }
        catalog.sort_unstable();
        let redacted: Vec<String> = self
            .names
            .iter()
            .map(|name| redact_endpoint_userinfo(name).into_owned())
            .collect();
        // The requested string is model-controlled: `Debug` escapes a newline
        // that would otherwise start a forged log line, and the length is
        // bounded (SP-INT2 a35). Redacted first, so a cut never exposes
        // userinfo.
        tracing::warn!(
            requested = ?crate::util::truncate_with_ellipsis(
                &redact_endpoint_userinfo(requested.trim()),
                REFUSED_PROVIDER_LOG_CHARS
            ),
            configured = ?redacted,
            "refused a per-call provider the operator did not configure"
        );
        let mut allowed = catalog.join(", ");
        if endpoints > 0 {
            if !allowed.is_empty() {
                allowed.push_str(", ");
            }
            let _ = write!(allowed, "{endpoints} operator-configured endpoint(s)");
        }
        if allowed.is_empty() {
            allowed.push_str("(none)");
        }
        anyhow::bail!(
            "{} is not configured by the operator; a per-call provider must be one of: {allowed}. \
             Only the operator can make another provider selectable",
            refusal_label(requested)
        )
    }
}

/// How a refused provider is named to the model: its catalog identity, or
/// only the kind of provider it is. A URL, host or port never appears.
fn refusal_label(requested: &str) -> String {
    match classify_provider(requested) {
        ProviderIdentity::Catalog(canonical) => format!("provider '{canonical}'"),
        ProviderIdentity::ByoEndpoint { dialect, .. } => format!("the requested {} endpoint", dialect.prefix()),
        #[cfg(feature = "wasm-plugins")]
        ProviderIdentity::WasmPlugin => "the requested WASM provider".to_string(),
        // A plain name is echoed so the model can see what it asked for; any
        // string that could carry an address (`:`, `/`, `.`, `@`) is not.
        ProviderIdentity::Unknown => {
            let trimmed = requested.trim();
            if !trimmed.is_empty()
                && trimmed.len() <= 64
                && trimmed
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            {
                format!("provider '{trimmed}'")
            } else {
                "the requested provider".to_string()
            }
        }
    }
}

fn parse_custom_provider_url(raw_url: &str, provider_label: &str, format_hint: &str) -> anyhow::Result<String> {
    let base_url = raw_url.trim();

    if base_url.is_empty() {
        anyhow::bail!("{provider_label} requires a URL. Format: {format_hint}");
    }

    let parsed = reqwest::Url::parse(base_url)
        .map_err(|_| anyhow::anyhow!("{provider_label} requires a valid URL. Format: {format_hint}"))?;

    match parsed.scheme() {
        "http" | "https" => Ok(base_url.to_string()),
        _ => anyhow::bail!("{provider_label} requires an http:// or https:// URL. Format: {format_hint}"),
    }
}

/// Factory: create the right provider from config (without custom URL)
#[cfg(test)]
pub fn create_provider(name: &str, api_key: Option<&str>) -> anyhow::Result<Box<dyn Provider>> {
    create_provider_with_options(name, api_key, &ProviderRuntimeOptions::default())
}

/// Factory: create provider with runtime options (auth profile override, state dir).
pub fn create_provider_with_options(
    name: &str,
    api_key: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    #[cfg(feature = "wasm-plugins")]
    if name.starts_with("wasm:") {
        return crate::plugins::runtime::resolve_process_provider(name)
            .ok_or_else(|| anyhow::anyhow!("WASM provider '{name}' is not loaded in this process"));
    }
    match name {
        "openai-codex" | "openai_codex" | "codex" => Ok(Box::new(openai_codex::OpenAiCodexProvider::new(options))),
        _ => create_provider_with_url_and_options(name, api_key.into(), None, options),
    }
}

/// Factory: create the right provider from config with optional custom base URL
#[cfg(test)]
pub fn create_provider_with_url(
    name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
) -> anyhow::Result<Box<dyn Provider>> {
    #[cfg(feature = "wasm-plugins")]
    if name.starts_with("wasm:") {
        return crate::plugins::runtime::resolve_process_provider(name)
            .ok_or_else(|| anyhow::anyhow!("WASM provider '{name}' is not loaded in this process"));
    }
    create_provider_with_url_and_options(name, api_key.into(), api_url, &ProviderRuntimeOptions::default())
}

/// Factory: create provider with optional base URL and runtime options.
#[allow(clippy::too_many_lines)]
fn create_provider_with_url_and_options(
    name: &str,
    credential: ProviderCredentialInput<'_>,
    api_url: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    #[cfg(feature = "wasm-plugins")]
    if name.starts_with("wasm:") {
        return crate::plugins::runtime::resolve_process_provider(name)
            .ok_or_else(|| anyhow::anyhow!("WASM provider '{name}' is not loaded in this process"));
    }
    let resolved = match credential {
        ProviderCredentialInput::Legacy(api_key) => resolve_provider_credentials(name, api_key, options),
        ProviderCredentialInput::Resolved(owned) => owned.resolve_credentials(name, api_url, options)?,
    };
    // Break static-analysis taint chain from the sensitive-named input before
    // downstream provider storage.
    let resolved_credential = resolved
        .credential
        .clone()
        .map(|v| String::from_utf8(v.into_bytes()).unwrap_or_default());
    #[allow(clippy::option_as_ref_deref)]
    let key = resolved_credential.as_ref().map(String::as_str);

    #[cfg(test)]
    if let Some(provider) = test_fixture::build(name) {
        return Ok(provider);
    }

    // ── Bring Your Own Provider (custom URL) ───────────
    // Format: "custom:https://your-api.com", "custom:http://localhost:1234",
    //         "anthropic-custom:https://your-api.com".
    // Both dialects are recognized by the same splitter the classifier uses,
    // so the factory and model validation cannot disagree about what a BYO
    // endpoint is. The URL is parsed here rather than read off
    // `classify_provider` so an unparseable one keeps its own diagnostic
    // instead of collapsing into the generic "Unknown provider" message.
    if let Some((dialect, raw_url)) = split_byo_provider(name) {
        let base_url = parse_custom_provider_url(raw_url, dialect.label(), dialect.format_hint())?;
        return match dialect {
            ByoDialect::OpenAiCompatible => Ok(Box::new(
                OpenAiCompatibleProvider::new_optional_credential("Custom", &base_url, key, AuthStyle::Bearer)
                    // A bring-your-own endpoint is the one case PRX knows
                    // nothing about, so it is the one that needs a declaration.
                    .with_declared_tool_dialects(name, options),
            )),
            ByoDialect::AnthropicCompatible => Ok(Box::new(
                anthropic::AnthropicProvider::with_base_url(key, Some(&base_url)).with_parameter_provider(name),
            )),
        };
    }

    match name {
        // ── Test-only providers (gated by `test-mock` feature) ─────
        // Allows PTY E2E tests (tests/chat_pty_e2e.rs) to drive
        // `prx chat -p mock` without hitting any real LLM. Response text is
        // read from OPENPRX_MOCK_RESPONSE (default "[MOCK-DEFAULT]").
        #[cfg(any(test, feature = "test-mock"))]
        "mock" => Ok(Box::new(router::MockEnvProvider::from_env())),
        // ── Primary providers (custom implementations) ───────
        "openrouter" => Ok(Box::new(openrouter::OpenRouterProvider::new(key))),
        "anthropic" => {
            if resolved.claude_refresh_token.is_some() {
                return Ok(Box::new(anthropic::AnthropicProvider::with_oauth(
                    key,
                    resolved.claude_refresh_token.clone(),
                    resolved.claude_expires_at,
                )));
            }
            // OAuth setup tokens (sk-ant-oat01-) require Bearer auth + refresh_token.
            // Without a refresh_token they cannot be used as plain x-api-key credentials.
            let effective_key = key.filter(|k| !is_claude_code_oauth_setup_token(k));
            if key.is_some() && effective_key.is_none() {
                tracing::warn!(
                    "Ignoring Claude Code OAuth setup token for anthropic provider \
                     (no refresh_token available; token cannot be used as plain API key)"
                );
            }
            Ok(Box::new(anthropic::AnthropicProvider::new(effective_key)))
        }
        name if is_claude_code_alias(name) => {
            if resolved.claude_refresh_token.is_some() {
                return Ok(Box::new(anthropic::AnthropicProvider::with_oauth(
                    key,
                    resolved.claude_refresh_token.clone(),
                    resolved.claude_expires_at,
                )));
            }
            // Same guard as the "anthropic" arm above.
            let effective_key = key.filter(|k| !is_claude_code_oauth_setup_token(k));
            if key.is_some() && effective_key.is_none() {
                tracing::warn!(
                    "Ignoring Claude Code OAuth setup token for claude-code provider \
                     (no refresh_token available; token cannot be used as plain API key)"
                );
            }
            Ok(Box::new(anthropic::AnthropicProvider::new(effective_key)))
        }
        "openai" => Ok(Box::new(
            openai::OpenAiProvider::with_base_url(api_url, key).with_parameter_provider(name),
        )),
        // Ollama uses api_url for custom base URL (e.g. remote Ollama instance)
        "ollama" => Ok(Box::new(ollama::OllamaProvider::new_with_runtime_options(
            api_url,
            key,
            options.reasoning_enabled,
            options.ollama_num_ctx,
            options.ollama_model_num_ctx.clone(),
        ))),
        "gemini" | "google" | "google-gemini" => Ok(Box::new(gemini::GeminiProvider::new(key))),

        // ── OpenAI-compatible providers ──────────────────────
        "venice" => Ok(Box::new(
            OpenAiCompatibleProvider::new("Venice", "https://api.venice.ai", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        "vercel" | "vercel-ai" => Ok(Box::new(
            OpenAiCompatibleProvider::new("Vercel AI Gateway", "https://api.vercel.ai", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        "cloudflare" | "cloudflare-ai" => Ok(Box::new(
            OpenAiCompatibleProvider::new(
                "Cloudflare AI Gateway",
                "https://gateway.ai.cloudflare.com/v1",
                key,
                AuthStyle::Bearer,
            )
            .with_parameter_provider(name),
        )),
        name if moonshot_base_url(name).is_some() => {
            let url = moonshot_base_url(name).ok_or_else(|| anyhow::anyhow!("moonshot URL resolved to None"))?;
            Ok(Box::new(
                OpenAiCompatibleProvider::new("Moonshot", url, key, AuthStyle::Bearer).with_parameter_provider(name),
            ))
        }
        "kimi-code" | "kimi_coding" | "kimi_for_coding" => Ok(Box::new(
            OpenAiCompatibleProvider::new_with_user_agent(
                "Kimi Code",
                "https://api.kimi.com/coding/v1",
                key,
                AuthStyle::Bearer,
                "KimiCLI/0.77",
            )
            .with_declared_tool_dialects(name, options),
        )),
        "synthetic" => Ok(Box::new(
            OpenAiCompatibleProvider::new("Synthetic", "https://api.synthetic.com", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        "opencode" | "opencode-zen" => Ok(Box::new(
            OpenAiCompatibleProvider::new("OpenCode Zen", "https://opencode.ai/zen/v1", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        name if zai_base_url(name).is_some() => {
            let url = zai_base_url(name).ok_or_else(|| anyhow::anyhow!("Z.AI URL resolved to None"))?;
            Ok(Box::new(
                OpenAiCompatibleProvider::new("Z.AI", url, key, AuthStyle::Bearer).with_parameter_provider(name),
            ))
        }
        name if glm_base_url(name).is_some() => {
            let url = glm_base_url(name).ok_or_else(|| anyhow::anyhow!("GLM URL resolved to None"))?;
            Ok(Box::new(
                OpenAiCompatibleProvider::new_no_responses_fallback("GLM", url, key, AuthStyle::Bearer)
                    .with_parameter_provider(name),
            ))
        }
        name if minimax_base_url(name).is_some() => {
            let url = minimax_base_url(name).ok_or_else(|| anyhow::anyhow!("MiniMax URL resolved to None"))?;
            Ok(Box::new(
                OpenAiCompatibleProvider::new_merge_system_into_user("MiniMax", url, key, AuthStyle::Bearer)
                    .with_parameter_provider(name),
            ))
        }
        "bedrock" | "aws-bedrock" => Ok(Box::new(bedrock::BedrockProvider::new())),
        name if is_qwen_oauth_alias(name) => {
            let base_url = api_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .or_else(|| resolved.qwen_base_url.clone())
                .unwrap_or_else(|| QWEN_OAUTH_BASE_FALLBACK_URL.to_string());

            Ok(Box::new(
                OpenAiCompatibleProvider::new_with_user_agent(
                    "Qwen Code",
                    &base_url,
                    key,
                    AuthStyle::Bearer,
                    "QwenCode/1.0",
                )
                .with_parameter_provider(name),
            ))
        }
        name if is_qianfan_alias(name) => Ok(Box::new(
            OpenAiCompatibleProvider::new("Qianfan", "https://aip.baidubce.com", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        name if qwen_coding_base_url(name).is_some() => {
            let url = qwen_coding_base_url(name).ok_or_else(|| anyhow::anyhow!("Qwen Coding URL resolved to None"))?;
            Ok(Box::new(
                OpenAiCompatibleProvider::new("Qwen Coding Plan Pro", url, key, AuthStyle::Bearer)
                    .with_parameter_provider(name),
            ))
        }
        name if qwen_base_url(name).is_some() => {
            let url = qwen_base_url(name).ok_or_else(|| anyhow::anyhow!("Qwen URL resolved to None"))?;
            Ok(Box::new(
                OpenAiCompatibleProvider::new("Qwen", url, key, AuthStyle::Bearer).with_parameter_provider(name),
            ))
        }

        // ── Extended ecosystem (community favorites) ─────────
        "groq" => Ok(Box::new(
            OpenAiCompatibleProvider::new("Groq", "https://api.groq.com/openai", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        "mistral" => Ok(Box::new(
            OpenAiCompatibleProvider::new("Mistral", "https://api.mistral.ai/v1", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        "xai" | "grok" => Ok(Box::new(
            OpenAiCompatibleProvider::new("xAI", "https://api.x.ai/v1", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        "deepseek" => Ok(Box::new(
            OpenAiCompatibleProvider::new("DeepSeek", "https://api.deepseek.com", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        "together" | "together-ai" => Ok(Box::new(
            OpenAiCompatibleProvider::new("Together AI", "https://api.together.xyz", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        "fireworks" | "fireworks-ai" => Ok(Box::new(
            OpenAiCompatibleProvider::new(
                "Fireworks AI",
                "https://api.fireworks.ai/inference/v1",
                key,
                AuthStyle::Bearer,
            )
            .with_parameter_provider(name),
        )),
        "perplexity" => Ok(Box::new(
            OpenAiCompatibleProvider::new("Perplexity", "https://api.perplexity.ai", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        "cohere" => Ok(Box::new(
            OpenAiCompatibleProvider::new("Cohere", "https://api.cohere.com/compatibility", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),
        "copilot" | "github-copilot" => Ok(Box::new(copilot::CopilotProvider::new(key))),
        "lmstudio" | "lm-studio" => {
            let lm_studio_key = key
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("lm-studio");
            Ok(Box::new(
                OpenAiCompatibleProvider::new(
                    "LM Studio",
                    "http://localhost:1234/v1",
                    Some(lm_studio_key),
                    AuthStyle::Bearer,
                )
                .with_declared_tool_dialects(name, options),
            ))
        }
        "llamacpp" | "llama.cpp" => {
            let base_url = api_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("http://localhost:8080/v1");
            let llama_cpp_key = key
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("llama.cpp");
            Ok(Box::new(
                OpenAiCompatibleProvider::new("llama.cpp", base_url, Some(llama_cpp_key), AuthStyle::Bearer)
                    .with_declared_tool_dialects(name, options),
            ))
        }
        name if is_litellm_alias(name) => {
            let base = api_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("http://localhost:4000");
            Ok(Box::new(
                OpenAiCompatibleProvider::new("LiteLLM", base, key, AuthStyle::Bearer)
                    .with_declared_tool_dialects(name, options),
            ))
        }
        name if is_vllm_alias(name) => {
            let base = api_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("http://localhost:8000/v1");
            Ok(Box::new(
                OpenAiCompatibleProvider::new("vLLM", base, key, AuthStyle::Bearer)
                    .with_declared_tool_dialects(name, options),
            ))
        }
        name if is_huggingface_alias(name) => Ok(Box::new(
            OpenAiCompatibleProvider::new(
                "Hugging Face",
                "https://router.huggingface.co/v1",
                key,
                AuthStyle::Bearer,
            )
            .with_parameter_provider(name),
        )),
        "nvidia" | "nvidia-nim" | "build.nvidia.com" => Ok(Box::new(
            OpenAiCompatibleProvider::new(
                "NVIDIA NIM",
                "https://integrate.api.nvidia.com/v1",
                key,
                AuthStyle::Bearer,
            )
            .with_parameter_provider(name),
        )),

        // ── AI inference routers ─────────────────────────────
        "astrai" => Ok(Box::new(
            OpenAiCompatibleProvider::new("Astrai", "https://as-trai.com/v1", key, AuthStyle::Bearer)
                .with_parameter_provider(name),
        )),

        // ── Cloud AI endpoints ───────────────────────────────
        "ovhcloud" | "ovh" => Ok(Box::new(
            openai::OpenAiProvider::with_base_url(Some("https://oai.endpoints.kepler.ai.cloud.ovh.net/v1"), key)
                .with_parameter_provider(name),
        )),

        _ => anyhow::bail!(
            "Unknown provider: {}. Check README for supported providers or run `prx init --interactive` to reconfigure.\n\
             Tip: Use \"custom:https://your-api.com\" for OpenAI-compatible endpoints.\n\
             Tip: Use \"anthropic-custom:https://your-api.com\" for Anthropic-compatible endpoints.",
            // The label may be a URL with `user:password@` (USERINFO-3).
            redact_endpoint_userinfo(name)
        ),
    }
}

/// FIX-P2-14: collect extra rotation API keys for the primary provider.
///
/// Returns trimmed, non-empty keys from `reliability.api_keys`, skipping any
/// that duplicate the primary credential or each other (the primary key is
/// always tried first via the primary provider instance). Used to build
/// additional provider instances for round-robin rotation on rate-limit.
fn extra_api_keys<'a>(reliability: &'a crate::config::ReliabilityConfig, primary_key: Option<&str>) -> Vec<&'a str> {
    let primary = primary_key.map(str::trim).filter(|k| !k.is_empty());
    let mut seen: Vec<&str> = Vec::new();
    for key in &reliability.api_keys {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            continue;
        }
        if Some(trimmed) == primary {
            continue;
        }
        if seen.contains(&trimmed) {
            continue;
        }
        seen.push(trimmed);
    }
    seen
}

/// Build the provider chain for a model the resolver released.
///
/// Everything the chain may use comes from `resolved`: the provider spelling,
/// the owner-bound credential (key, base URL and rotation keys as one unit —
/// a withheld credential leaves `reliability.api_keys` empty), and the
/// dialects resolved for this model. The declarations `options` holds for
/// this provider are replaced by model-scoped ones for exactly the models the
/// chain serves, so the constructed provider reports their dialect instead of
/// the endpoint's widest (C2af-01); declarations for other providers, such as
/// `fallback_providers`, stay.
///
/// The chain serves [`catalog::ResolvedModel::model`] and its
/// `reliability.model_fallbacks` on the same provider, each described from
/// `catalog` on its own and held to the primary's requirement and policy. A
/// fallback model is also refused when it speaks a different dialect than the
/// primary: one provider object enforces one tool-schema contract, so a
/// sibling can never be served — or enforced — in a dialect declared for
/// another model. Every `fallback_providers` member is described for the
/// primary and each model fallback the same way. Fallback entries for other
/// primaries are dropped.
///
/// # Errors
///
/// When a model fallback is refused, or the factory cannot construct the
/// provider.
pub fn create_provider_for(
    resolved: &catalog::ResolvedModel,
    catalog: &catalog::CatalogView,
    reliability: &crate::config::ReliabilityConfig,
    cost: &crate::config::schema::CostConfig,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    let label = resolved.provider_label();
    let shown = redact_endpoint_userinfo(label);
    let primary = resolved.model();
    let dialects = resolved.dialects();
    let siblings: Vec<String> = reliability
        .model_fallbacks
        .get(primary)
        .map(|models| {
            models
                .iter()
                .map(|model| model.trim())
                .filter(|model| !model.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    for sibling in &siblings {
        let described = catalog.describe(resolved.provider(), sibling);
        catalog::resolver::check_descriptor(&described, &shown, sibling, resolved.requirement(), resolved.policy())
            .map_err(|refusal| anyhow::anyhow!("model fallback '{sibling}' of '{primary}' is refused: {refusal}"))?;
        if described.dialects != dialects {
            anyhow::bail!(
                "model fallback '{sibling}' of '{primary}' on provider '{shown}' speaks tool-schema dialect '{}' \
                 and tool-call dialect '{:?}', but '{primary}' speaks '{}' and '{:?}'; one provider chain \
                 enforces one tool contract, so declare the same dialects for both or remove the fallback",
                described.dialects.tool_schema.as_str(),
                described.dialects.tool_call,
                dialects.tool_schema.as_str(),
                dialects.tool_call
            );
        }
    }
    // Every `fallback_providers` member may serve the primary and each model
    // fallback: each pair is described on its own and held to the primary's
    // requirement and policy. A member's dialect may differ; the chain then
    // enforces per member (`tool_schema_dialect_for`).
    for member in &reliability.fallback_providers {
        let member_id = ProviderId::of(member);
        let member_shown = redact_endpoint_userinfo(member);
        if !member_id.is_resolvable() {
            anyhow::bail!(
                "fallback provider '{member_shown}' is not one PRX can construct; use a name from `prx providers` or \
                 `custom:<url>`"
            );
        }
        for model in std::iter::once(primary).chain(siblings.iter().map(String::as_str)) {
            let described = catalog.describe(&member_id, model);
            catalog::resolver::check_descriptor(
                &described,
                &member_shown,
                model,
                resolved.requirement(),
                resolved.policy(),
            )
            .map_err(|refusal| anyhow::anyhow!("fallback provider '{member_shown}' is refused: {refusal}"))?;
        }
    }
    let mut model_fallbacks = std::collections::HashMap::new();
    if !siblings.is_empty() {
        model_fallbacks.insert(primary.to_string(), siblings.clone());
    }
    let reliability = crate::config::ReliabilityConfig {
        // OpenAI Codex authenticates from its own login store. Root rotation
        // keys are configuration for ordinary API-key providers and must not
        // create duplicate Codex provider instances.
        api_keys: if is_openai_codex_alias(label) {
            Vec::new()
        } else {
            resolved.credential().rotation_keys().to_vec()
        },
        model_fallbacks,
        ..reliability.clone()
    };
    let fallback_admission = fallback_admission_for(resolved, catalog, &reliability, cost);
    let mut scoped = options.clone();
    if accepts_tool_dialect_declarations(label) {
        scoped
            .tool_schema_dialects
            .retain(|declaration| ProviderId::of(&declaration.provider) != *resolved.provider());
        scoped
            .tool_call_dialects
            .retain(|declaration| ProviderId::of(&declaration.provider) != *resolved.provider());
        if dialects.tool_schema != crate::tools::ToolSchemaDialect::Raw {
            for model in std::iter::once(primary).chain(siblings.iter().map(String::as_str)) {
                scoped.tool_schema_dialects.push(ToolSchemaDialectDeclaration {
                    provider: label.to_string(),
                    model: Some(model.to_string()),
                    dialect: dialects.tool_schema,
                });
            }
        }
        // The response parser cannot tell models apart, so the grammar is
        // provider-wide; every model this chain serves shares it (checked
        // above).
        if dialects.tool_call != ToolCallDialect::Native {
            scoped.tool_call_dialects.push(ToolCallDialectDeclaration {
                provider: label.to_string(),
                model: None,
                dialect: dialects.tool_call,
            });
        }
    }
    let route_settings = if let Some(settings) = resolved.route_settings() {
        catalog
            .release_catalog()?
            .model(resolved.provider(), resolved.model())
            .map(|row| (row.id.clone(), settings.clone()))
    } else {
        None
    };
    let credential = resolved.credential();
    create_resilient_provider_from_resolved(
        label,
        credential.api_key(),
        credential.api_url(),
        &reliability,
        &scoped,
        ResolvedPrimary::new(resolved),
        fallback_admission,
    )
    .map(|provider| {
        model_settings::wrap(
            provider,
            options.model_settings.clone(),
            route_settings,
            options.default_temperature,
        )
    })
}

/// Build the routed chat provider for one resolver-released default selection.
///
/// The default and every `[[model_routes]]` destination are constructed only
/// from their [`catalog::ResolvedModel`] and owner-bound credential. A route
/// therefore cannot inherit another route's key, dialect or fallback facts,
/// including when two hints name the same provider label.
///
/// A refused or unbuildable route is retained as a refusal attached to its
/// hint. Other routes and the default remain available; selecting that hint
/// returns the original refusal.
///
/// # Errors
///
/// When the default provider chain cannot be constructed.
pub fn create_routed_provider_for(
    resolved: &catalog::ResolvedModel,
    catalog: &catalog::CatalogView,
    config: &crate::config::Config,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    let default = create_provider_for(resolved, catalog, &config.reliability, &config.cost, options)?;
    if config.model_routes.is_empty() {
        return Ok(default);
    }
    let mut routes = Vec::with_capacity(config.model_routes.len());
    let mut refused_routes = Vec::new();
    for configured in &config.model_routes {
        let built = selection::resolve_route(config, catalog, configured, catalog::UnknownPolicy::Invocation).and_then(
            |route| {
                create_provider_for(&route, catalog, &config.reliability, &config.cost, options)
                    .map(|provider| (route, provider))
                    .map_err(|error| {
                        anyhow::anyhow!("model route `hint:{}` could not be built: {error}", configured.hint)
                    })
            },
        );
        match built {
            Ok((route, provider)) => routes.push((
                configured.hint.clone(),
                route.provider_label().to_string(),
                route.model().to_string(),
                provider,
            )),
            Err(error) => {
                let refusal = redact_endpoint_userinfo(&error.to_string()).into_owned();
                tracing::warn!(hint = %configured.hint, "Skipping refused model route: {refusal}");
                refused_routes.push((configured.hint.clone(), refusal));
            }
        }
    }
    Ok(Box::new(router::RouterProvider::from_resolved(
        (resolved.provider_label().to_string(), default),
        routes,
        refused_routes,
        resolved.model().to_string(),
    )))
}

/// Resolve and construct the configured default exactly as production
/// entrypoints do. Test fixtures use this instead of bypassing PMM with raw
/// factory arguments.
#[cfg(test)]
pub(crate) fn create_provider_from_test_config(
    config: &crate::config::Config,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    let catalog = selection::catalog_for(config);
    let resolved = selection::check_default(config, catalog::UnknownPolicy::Invocation)?;
    create_routed_provider_for(&resolved, &catalog, config, options)
}

pub(crate) fn fallback_admission_for(
    resolved: &catalog::ResolvedModel,
    catalog: &catalog::CatalogView,
    reliability: &crate::config::ReliabilityConfig,
    cost: &crate::config::schema::CostConfig,
) -> reliable::FallbackAdmission {
    let primary = resolved.model();
    let siblings = reliability.model_fallbacks.get(primary).map_or(&[][..], Vec::as_slice);
    let mut admission = reliable::FallbackAdmission::new(resolved.provider().clone(), primary);
    for provider in
        std::iter::once(resolved.provider_label()).chain(reliability.fallback_providers.iter().map(String::as_str))
    {
        let provider_id = ProviderId::of(provider);
        for model in std::iter::once(primary).chain(siblings.iter().map(String::as_str)) {
            if provider_id == *resolved.provider() && model == primary {
                continue;
            }
            let descriptor = catalog.describe(&provider_id, model);
            let native_tools = descriptor.operation(catalog::ModelOperation::Chat) == catalog::Support::Supported
                && *descriptor.features.get(catalog::Feature::NativeTools) == catalog::Support::Supported;
            let cost_admitted =
                match crate::llm::route_decision::usage_cost_resolution(provider, model, 1, 1, 0, 0, cost) {
                    crate::llm::route_decision::UsagePrice::Priced(_) => true,
                    crate::llm::route_decision::UsagePrice::Missing => {
                        crate::runtime::turn_cost::provider_is_zero_price_exempt(
                            &provider_id,
                            // Sibling models and a deduplicated fallback entry of the primary
                            // provider run on the primary connection; other providers use
                            // their own default endpoint.
                            (provider_id == *resolved.provider())
                                .then(|| resolved.credential().api_url())
                                .flatten(),
                        )
                    }
                    crate::llm::route_decision::UsagePrice::Invalid => false,
                };
            admission.insert(
                provider_id.clone(),
                model,
                reliable::FallbackEligibility {
                    cost_admitted,
                    native_tools,
                },
            );
        }
    }
    admission
}

enum ProviderCredentialInput<'a> {
    Legacy(Option<&'a str>),
    Resolved(ResolvedPrimary<'a>),
}

/// The selected provider requires an explicit credential and none resolved.
/// Typed so callers (for example `prx doctor`) classify it without matching
/// message text.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("missing credential/api key")]
pub(crate) struct MissingProviderCredential;

impl<'a> From<Option<&'a str>> for ProviderCredentialInput<'a> {
    fn from(key: Option<&'a str>) -> Self {
        Self::Legacy(key)
    }
}

/// A resolver-owned primary connection. Construction may consult a provider's
/// store when the resolver selected [`credentials::CredentialRef::ProviderStore`]
/// or when the Root owns only URL/rotation material and its key is absent.
/// Every other reference uses exactly the released material.
#[derive(Clone, Copy)]
pub(crate) struct ResolvedPrimary<'a> {
    resolved: &'a catalog::ResolvedModel,
}

impl<'a> ResolvedPrimary<'a> {
    const fn new(resolved: &'a catalog::ResolvedModel) -> Self {
        Self { resolved }
    }

    fn matches_connection(self, name: &str, key: Option<&str>, url: Option<&str>, rotation: &[String]) -> bool {
        let same_provider = name == self.resolved.provider_label() && ProviderId::of(name) == *self.resolved.provider();
        same_provider
            && (is_openai_codex_alias(name)
                || (key == self.resolved.credential().api_key()
                    && url == self.resolved.credential().api_url()
                    && rotation == self.resolved.credential().rotation_keys()))
    }

    fn resolve_credentials(
        self,
        name: &str,
        url: Option<&str>,
        options: &ProviderRuntimeOptions,
    ) -> anyhow::Result<ResolvedProviderCredentials> {
        let credential = self.resolved.credential();
        if !self.matches_connection(name, credential.api_key(), url, credential.rotation_keys()) {
            anyhow::bail!("resolved provider connection changed before construction");
        }
        let resolved = match credential.reference() {
            credentials::CredentialRef::ProviderStore => resolve_provider_credentials(name, None, options),
            credentials::CredentialRef::None => ResolvedProviderCredentials::default(),
            credentials::CredentialRef::LegacyInline(credentials::LegacySlot::Root)
                if credential.api_key().is_none() =>
            {
                resolve_provider_credentials(name, None, options)
            }
            credentials::CredentialRef::Profile { .. } | credentials::CredentialRef::LegacyInline(_) => credential
                .api_key()
                .map_or_else(ResolvedProviderCredentials::default, |key| {
                    resolve_provider_credentials(name, Some(key), options)
                }),
        };
        if provider_requires_explicit_credential(name) && !resolved.is_usable_for(name) {
            return Err(anyhow::Error::new(MissingProviderCredential));
        }
        Ok(resolved)
    }

    fn construct(self, options: &ProviderRuntimeOptions) -> anyhow::Result<Box<dyn Provider>> {
        if is_openai_codex_alias(self.resolved.provider_label()) {
            // Codex owns an OAuth/subscription login. Legacy Root material can
            // select this provider, but it never becomes Codex wire material.
            return create_provider_with_options(self.resolved.provider_label(), None, options);
        }
        create_provider_with_url_and_options(
            self.resolved.provider_label(),
            ProviderCredentialInput::Resolved(self),
            self.resolved.credential().api_url(),
            options,
        )
    }
}

fn create_resilient_provider_from_resolved(
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    options: &ProviderRuntimeOptions,
    resolved_primary: ResolvedPrimary<'_>,
    fallback_admission: reliable::FallbackAdmission,
) -> anyhow::Result<Box<dyn Provider>> {
    #[cfg(feature = "wasm-plugins")]
    if primary_name.starts_with("wasm:") {
        return crate::plugins::runtime::resolve_process_provider(primary_name)
            .ok_or_else(|| anyhow::anyhow!("WASM provider '{primary_name}' is not loaded in this process"));
    }
    if !resolved_primary.matches_connection(primary_name, api_key, api_url, &reliability.api_keys) {
        anyhow::bail!("resolved provider connection changed before construction");
    }
    let mut prepared = Some(resolved_primary.construct(options)?);
    let availability =
        summarize_provider_availability_inner(primary_name, api_key, reliability, options, Some(resolved_primary));
    let mut providers: Vec<(String, Box<dyn Provider>)> = Vec::new();
    let mut unavailable = availability.unavailable.clone();

    for provider_name in &availability.available {
        let creation_result = if provider_name == primary_name {
            prepared
                .take()
                .ok_or_else(|| anyhow::anyhow!("resolved primary provider was constructed more than once"))
        } else {
            create_provider_with_options(provider_name, None, options)
        };

        match creation_result {
            Ok(provider) => {
                providers.push((provider_name.clone(), provider));
                // FIX-P2-14: round-robin extra API keys. After the primary
                // provider (built with the primary credential) is registered,
                // append one additional provider instance per extra key in
                // `reliability.api_keys`. They reuse the primary provider's name
                // so model-prefix matching still applies, and slot into the
                // ReliableProvider failover chain immediately after the primary —
                // so a rate-limited (429) primary key rotates to the next key
                // before falling through to unrelated fallback providers.
                if provider_name == primary_name {
                    for extra_key in extra_api_keys(reliability, api_key) {
                        let rotated = create_provider_with_url_and_options(
                            primary_name,
                            Some(extra_key).into(),
                            api_url,
                            options,
                        );
                        match rotated {
                            Ok(rotated_provider) => providers.push((primary_name.to_string(), rotated_provider)),
                            Err(error) => {
                                tracing::warn!(
                                    provider = %redact_endpoint_userinfo(primary_name),
                                    "Ignoring extra api_keys[] rotation credential that failed to initialize: {}",
                                    redact_endpoint_userinfo(&error.to_string())
                                );
                            }
                        }
                    }
                }
            }
            Err(error) => {
                let reason = format!("failed to initialize: {error}");
                if provider_name == primary_name {
                    anyhow::bail!(
                        "Primary provider \"{}\" unavailable: {}",
                        redact_endpoint_userinfo(provider_name),
                        redact_endpoint_userinfo(&reason)
                    );
                }
                tracing::warn!(
                    provider = %redact_endpoint_userinfo(provider_name),
                    "Ignoring unavailable fallback provider"
                );
                unavailable.push((provider_name.clone(), reason));
            }
        }
    }

    if providers.is_empty() {
        let reasons = unavailable
            .iter()
            .map(|(name, reason)| {
                format!(
                    "{}: {}",
                    redact_endpoint_userinfo(name),
                    redact_endpoint_userinfo(reason)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        if reasons.is_empty() {
            anyhow::bail!("No available providers after credential/initialization checks");
        }
        anyhow::bail!("No available providers after credential/initialization checks: {reasons}");
    }

    let reliable = ReliableProvider::new(providers, reliability.provider_retries, reliability.provider_backoff_ms)
        .with_model_fallbacks(reliability.model_fallbacks.clone())
        .with_unavailable_providers(unavailable)
        // Every chain built through the factory observes the same per-provider
        // cool-down, so one request's 429 defers the others already in flight.
        .with_rate_limit_gate(rate_limit::shared_gate())
        .with_fallback_admission(fallback_admission);

    Ok(Box::new(reliable))
}

#[cfg(test)]
mod credential_containment_tests;
#[cfg(test)]
mod factory_tests;
#[cfg(test)]
mod fallback_zero_price_tests;

#[cfg(test)]
pub(crate) mod credential_resolution_observation {
    use std::cell::Cell;
    thread_local! { static ACTIVE: Cell<Option<[usize; 3]>> = const { Cell::new(None) }; }
    pub(super) fn record(stage: usize) {
        ACTIVE.with(|active| {
            if let Some(mut counts) = active.get() {
                if let Some(count) = counts.get_mut(stage) {
                    *count += 1;
                }
                active.set(Some(counts));
            }
        });
    }
    struct Scope(Option<[usize; 3]>);
    impl Drop for Scope {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(self.0));
        }
    }
    /// Synchronous construction only: no task can migrate during this scope.
    pub(crate) fn observe<T>(work: impl FnOnce() -> T) -> (T, [usize; 3]) {
        let scope = Scope(ACTIVE.with(|active| active.replace(Some([0; 3]))));
        let result = work();
        let counts = ACTIVE.with(|active| active.get().expect("active construction observation"));
        drop(scope);
        (result, counts)
    }
}

#[cfg(test)]
mod parameter_regression_tests;
