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

use crate::auth::{profiles::AuthProfilesStore, select_profile_id};
use crate::onboard::auto_detect::is_claude_code_oauth_setup_token;
use compatible::{AuthStyle, OpenAiCompatibleProvider};
use reliable::ReliableProvider;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const MAX_API_ERROR_CHARS: usize = 200;

/// How many characters of a refused, model-supplied provider string the
/// refusal warning prints.
const REFUSED_PROVIDER_LOG_CHARS: usize = 200;
const MINIMAX_INTL_BASE_URL: &str = "https://api.minimax.io/v1";
const MINIMAX_CN_BASE_URL: &str = "https://api.minimaxi.com/v1";
const MINIMAX_OAUTH_GLOBAL_TOKEN_ENDPOINT: &str = "https://api.minimax.io/oauth/token";
const MINIMAX_OAUTH_CN_TOKEN_ENDPOINT: &str = "https://api.minimaxi.com/oauth/token";
const MINIMAX_OAUTH_PLACEHOLDER: &str = "minimax-oauth";
const MINIMAX_OAUTH_CN_PLACEHOLDER: &str = "minimax-oauth-cn";
const MINIMAX_OAUTH_REFRESH_TOKEN_ENV: &str = "MINIMAX_OAUTH_REFRESH_TOKEN";
const MINIMAX_OAUTH_REGION_ENV: &str = "MINIMAX_OAUTH_REGION";
const MINIMAX_OAUTH_CLIENT_ID_ENV: &str = "MINIMAX_OAUTH_CLIENT_ID";
const MINIMAX_OAUTH_DEFAULT_CLIENT_ID: &str = "78257093-7e40-4613-99e0-527b14b39113";
const GLM_GLOBAL_BASE_URL: &str = "https://api.z.ai/api/paas/v4";
const GLM_CN_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";
const MOONSHOT_INTL_BASE_URL: &str = "https://api.moonshot.ai/v1";
const MOONSHOT_CN_BASE_URL: &str = "https://api.moonshot.cn/v1";
const QWEN_CN_BASE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const QWEN_INTL_BASE_URL: &str = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1";
const QWEN_US_BASE_URL: &str = "https://dashscope-us.aliyuncs.com/compatible-mode/v1";
const QWEN_CODING_CN_BASE_URL: &str = "https://coding.dashscope.aliyuncs.com/v1";
const QWEN_CODING_INTL_BASE_URL: &str = "https://coding-intl.dashscope.aliyuncs.com/v1";
const QWEN_OAUTH_BASE_FALLBACK_URL: &str = QWEN_CN_BASE_URL;
const QWEN_OAUTH_TOKEN_ENDPOINT: &str = "https://chat.qwen.ai/api/v1/oauth2/token";
const QWEN_OAUTH_PLACEHOLDER: &str = "qwen-oauth";
const QWEN_OAUTH_TOKEN_ENV: &str = "QWEN_OAUTH_TOKEN";
const QWEN_OAUTH_REFRESH_TOKEN_ENV: &str = "QWEN_OAUTH_REFRESH_TOKEN";
const QWEN_OAUTH_RESOURCE_URL_ENV: &str = "QWEN_OAUTH_RESOURCE_URL";
const QWEN_OAUTH_CLIENT_ID_ENV: &str = "QWEN_OAUTH_CLIENT_ID";
const QWEN_OAUTH_DEFAULT_CLIENT_ID: &str = "f0304373b74a44d2b584a3fb70ca9e56";
const QWEN_OAUTH_CREDENTIAL_FILE: &str = ".qwen/oauth_creds.json";
const CLAUDE_CODE_OAUTH_TOKEN_ENDPOINT: &str = "https://console.anthropic.com/v1/oauth/token";
const CLAUDE_CODE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_CODE_CREDENTIAL_FILE: &str = ".claude/.credentials.json";
const ZAI_GLOBAL_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";
const ZAI_CN_BASE_URL: &str = "https://open.bigmodel.cn/api/coding/paas/v4";

pub(crate) fn is_minimax_intl_alias(name: &str) -> bool {
    matches!(
        name,
        "minimax"
            | "minimax-intl"
            | "minimax-io"
            | "minimax-global"
            | "minimax-oauth"
            | "minimax-portal"
            | "minimax-oauth-global"
            | "minimax-portal-global"
    )
}

pub(crate) fn is_minimax_cn_alias(name: &str) -> bool {
    matches!(
        name,
        "minimax-cn" | "minimaxi" | "minimax-oauth-cn" | "minimax-portal-cn"
    )
}

pub(crate) fn is_minimax_alias(name: &str) -> bool {
    is_minimax_intl_alias(name) || is_minimax_cn_alias(name)
}

pub(crate) fn is_glm_global_alias(name: &str) -> bool {
    matches!(name, "glm" | "zhipu" | "glm-global" | "zhipu-global")
}

pub(crate) fn is_glm_cn_alias(name: &str) -> bool {
    matches!(name, "glm-cn" | "zhipu-cn" | "bigmodel")
}

pub(crate) fn is_glm_alias(name: &str) -> bool {
    is_glm_global_alias(name) || is_glm_cn_alias(name)
}

pub(crate) fn is_moonshot_intl_alias(name: &str) -> bool {
    matches!(name, "moonshot-intl" | "moonshot-global" | "kimi-intl" | "kimi-global")
}

pub(crate) fn is_moonshot_cn_alias(name: &str) -> bool {
    matches!(name, "moonshot" | "kimi" | "moonshot-cn" | "kimi-cn")
}

pub(crate) fn is_moonshot_alias(name: &str) -> bool {
    is_moonshot_intl_alias(name) || is_moonshot_cn_alias(name)
}

pub(crate) fn is_qwen_cn_alias(name: &str) -> bool {
    matches!(name, "qwen" | "dashscope" | "qwen-cn" | "dashscope-cn")
}

pub(crate) fn is_qwen_intl_alias(name: &str) -> bool {
    matches!(
        name,
        "qwen-intl" | "dashscope-intl" | "qwen-international" | "dashscope-international"
    )
}

pub(crate) fn is_qwen_us_alias(name: &str) -> bool {
    matches!(name, "qwen-us" | "dashscope-us")
}

pub(crate) fn is_qwen_oauth_alias(name: &str) -> bool {
    matches!(name, "qwen-code" | "qwen-oauth" | "qwen_oauth")
}

pub(crate) fn is_qwen_coding_alias(name: &str) -> bool {
    matches!(
        name,
        "dashscope-coding" | "qwen-coding" | "dashscope-coding-intl" | "qwen-coding-intl"
    )
}

pub(crate) fn is_qwen_alias(name: &str) -> bool {
    is_qwen_cn_alias(name)
        || is_qwen_intl_alias(name)
        || is_qwen_us_alias(name)
        || is_qwen_coding_alias(name)
        || is_qwen_oauth_alias(name)
}

pub(crate) fn is_zai_global_alias(name: &str) -> bool {
    matches!(name, "zai" | "z.ai" | "zai-global" | "z.ai-global")
}

pub(crate) fn is_zai_cn_alias(name: &str) -> bool {
    matches!(name, "zai-cn" | "z.ai-cn")
}

pub(crate) fn is_zai_alias(name: &str) -> bool {
    is_zai_global_alias(name) || is_zai_cn_alias(name)
}

pub(crate) fn is_qianfan_alias(name: &str) -> bool {
    matches!(name, "qianfan" | "baidu")
}

pub(crate) fn is_claude_code_alias(name: &str) -> bool {
    matches!(name, "claude-code" | "claude-cli")
}

pub(crate) fn is_litellm_alias(name: &str) -> bool {
    matches!(name, "litellm" | "lite-llm")
}

pub(crate) fn is_vllm_alias(name: &str) -> bool {
    matches!(name, "vllm" | "v-llm")
}

/// The base URL provider construction addresses for the catalog provider
/// spelled `name` when its connection carries `api_url`: `Some` for the
/// adapters that honour a configured endpoint (`openai`, `ollama`,
/// llama.cpp, the Qwen OAuth aliases, LiteLLM, vLLM), `None` for the ones
/// that always use their own fixed endpoint. Everything that sends this
/// provider's key somewhere other than a generation — the live connection
/// test, `prx models refresh` — takes its host from here, so it addresses
/// exactly the host the provider itself does (D-095 M-1, D-078 T-7).
pub(crate) fn honoured_base_url<'a>(name: &str, api_url: Option<&'a str>) -> Option<&'a str> {
    let api_url = api_url.map(str::trim).filter(|url| !url.is_empty())?;
    let name = name.trim().to_ascii_lowercase();
    let honours = matches!(name.as_str(), "openai" | "ollama" | "llamacpp" | "llama.cpp")
        || is_qwen_oauth_alias(&name)
        || is_litellm_alias(&name)
        || is_vllm_alias(&name);
    honours.then_some(api_url)
}

/// `base` with `suffix` appended to its path, keeping its query and
/// dropping its fragment (`https://h/v1?sig=x` + `/models` =
/// `https://h/v1/models?sig=x`, D-095 L-4). A path already ending in
/// `suffix` is kept. `None` when `base` is not a URL.
pub(crate) fn endpoint_url(base: &str, suffix: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(base.trim()).ok()?;
    let path = url.path().trim_end_matches('/').to_string();
    if !path.ends_with(suffix) {
        url.set_path(&format!("{path}{suffix}"));
    }
    url.set_fragment(None);
    Some(url.to_string())
}

pub(crate) fn is_huggingface_alias(name: &str) -> bool {
    matches!(name, "huggingface" | "hf" | "hf-inference")
}

#[derive(Clone, Copy, Debug)]
enum MinimaxOauthRegion {
    Global,
    Cn,
}

impl MinimaxOauthRegion {
    const fn token_endpoint(self) -> &'static str {
        match self {
            Self::Global => MINIMAX_OAUTH_GLOBAL_TOKEN_ENDPOINT,
            Self::Cn => MINIMAX_OAUTH_CN_TOKEN_ENDPOINT,
        }
    }
}

#[derive(Debug, Deserialize)]
struct MinimaxOauthRefreshResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    base_resp: Option<MinimaxOauthBaseResponse>,
}

#[derive(Debug, Deserialize)]
struct MinimaxOauthBaseResponse {
    #[serde(default)]
    status_msg: Option<String>,
}

#[derive(Clone, Deserialize, Default)]
struct QwenOauthCredentials {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    resource_url: Option<String>,
    #[serde(default)]
    expiry_date: Option<i64>,
}

impl std::fmt::Debug for QwenOauthCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QwenOauthCredentials")
            .field("access_token", &self.access_token.as_ref().map(|_| "[REDACTED]"))
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "[REDACTED]"))
            .field("resource_url", &self.resource_url)
            .field("expiry_date", &self.expiry_date)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct QwenOauthTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    resource_url: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Clone, Deserialize, Serialize, Default)]
pub(crate) struct ClaudeCodeCredentials {
    #[serde(rename = "accessToken", default)]
    pub(crate) access_token: Option<String>,
    #[serde(rename = "refreshToken", default)]
    pub(crate) refresh_token: Option<String>,
    #[serde(rename = "expiresAt", default)]
    pub(crate) expires_at: Option<i64>,
    #[serde(rename = "subscriptionType", default)]
    pub(crate) subscription_type: Option<String>,
}

impl std::fmt::Debug for ClaudeCodeCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeCredentials")
            .field("access_token", &self.access_token.as_ref().map(|_| "[REDACTED]"))
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "[REDACTED]"))
            .field("expires_at", &self.expires_at)
            .field("subscription_type", &self.subscription_type)
            .finish()
    }
}

#[derive(Debug, Deserialize, Default)]
struct ClaudeCodeCredentialsFile {
    #[serde(rename = "claudeAiOauth", default)]
    claude_ai_oauth: Option<ClaudeCodeCredentials>,
}

#[derive(Debug, Deserialize)]
struct ClaudeCodeOauthTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Clone, Default)]
struct ClaudeCodeProviderContext {
    credential: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
    /// A refresh was due and [`OauthRefresh::Never`] withheld it.
    refresh_withheld: bool,
}

impl std::fmt::Debug for ClaudeCodeProviderContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeProviderContext")
            .field("credential", &self.credential.as_ref().map(|_| "[REDACTED]"))
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "[REDACTED]"))
            .field("expires_at", &self.expires_at)
            .field("refresh_withheld", &self.refresh_withheld)
            .finish()
    }
}

#[derive(Clone, Default)]
struct QwenOauthProviderContext {
    credential: Option<String>,
    base_url: Option<String>,
    /// A refresh was due and [`OauthRefresh::Never`] withheld it.
    refresh_withheld: bool,
}

impl std::fmt::Debug for QwenOauthProviderContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QwenOauthProviderContext")
            .field("credential", &self.credential.as_ref().map(|_| "[REDACTED]"))
            .field("base_url", &self.base_url)
            .field("refresh_withheld", &self.refresh_withheld)
            .finish()
    }
}

fn read_non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

const fn is_minimax_oauth_placeholder(value: &str) -> bool {
    value.eq_ignore_ascii_case(MINIMAX_OAUTH_PLACEHOLDER) || value.eq_ignore_ascii_case(MINIMAX_OAUTH_CN_PLACEHOLDER)
}

fn minimax_oauth_region(name: &str) -> MinimaxOauthRegion {
    if let Some(region) = read_non_empty_env(MINIMAX_OAUTH_REGION_ENV) {
        let normalized = region.to_ascii_lowercase();
        if matches!(normalized.as_str(), "cn" | "china") {
            return MinimaxOauthRegion::Cn;
        }
        if matches!(normalized.as_str(), "global" | "intl" | "international") {
            return MinimaxOauthRegion::Global;
        }
    }

    if is_minimax_cn_alias(name) {
        MinimaxOauthRegion::Cn
    } else {
        MinimaxOauthRegion::Global
    }
}

fn minimax_oauth_client_id() -> String {
    read_non_empty_env(MINIMAX_OAUTH_CLIENT_ID_ENV).unwrap_or_else(|| MINIMAX_OAUTH_DEFAULT_CLIENT_ID.to_string())
}

fn qwen_oauth_client_id() -> String {
    read_non_empty_env(QWEN_OAUTH_CLIENT_ID_ENV).unwrap_or_else(|| QWEN_OAUTH_DEFAULT_CLIENT_ID.to_string())
}

fn qwen_oauth_credentials_file_path() -> Option<PathBuf> {
    oauth_credentials_home().map(|home| home.join(QWEN_OAUTH_CREDENTIAL_FILE))
}

fn claude_code_credentials_file_path() -> Option<PathBuf> {
    oauth_credentials_home().map(|home| home.join(CLAUDE_CODE_CREDENTIAL_FILE))
}

fn normalize_qwen_oauth_base_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    // Reject plain HTTP — enforce HTTPS only
    if trimmed.starts_with("http://") {
        tracing::error!("Qwen OAuth base URL must use HTTPS, got http://");
        return None;
    }

    let with_scheme = if trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };

    // Extract host and reject private/loopback addresses (SSRF defense)
    let host = with_scheme
        .strip_prefix("https://")
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("");
    let host_no_port = host.split(':').next().unwrap_or(host);

    if is_private_or_loopback_host(host_no_port) {
        tracing::error!(host = %host, "Qwen OAuth base URL points to private/loopback address");
        return None;
    }

    let normalized = with_scheme.trim_end_matches('/').to_string();
    if normalized.ends_with("/v1") {
        Some(normalized)
    } else {
        Some(format!("{normalized}/v1"))
    }
}

/// Check whether a hostname or IP literal is a private or loopback address.
fn is_private_or_loopback_host(host: &str) -> bool {
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    let lower = bare.to_lowercase();

    if lower == "localhost" || lower.ends_with(".localhost") || lower == "::1" || lower == "0.0.0.0" {
        return true;
    }

    // Check .local TLD
    if lower.rsplit('.').next().is_some_and(|tld| tld == "local") {
        return true;
    }

    // Parse as IPv4 and check private/loopback ranges
    if let Ok(v4) = bare.parse::<std::net::Ipv4Addr>() {
        let [a, b, _, _] = v4.octets();
        return a == 0            // 0.0.0.0/8
            || a == 10           // 10.0.0.0/8
            || a == 127          // 127.0.0.0/8
            || (a == 169 && b == 254)  // 169.254.0.0/16
            || (a == 172 && (16..=31).contains(&b))  // 172.16.0.0/12
            || (a == 192 && b == 168)  // 192.168.0.0/16
            || (a == 100 && (64..=127).contains(&b))  // 100.64.0.0/10 (CGNAT)
            || a >= 240; // 240.0.0.0/4 (reserved/broadcast)
    }

    // Parse as IPv6 and check private/loopback ranges
    if let Ok(v6) = bare.parse::<std::net::Ipv6Addr>() {
        let segs = v6.segments();
        return v6.is_loopback()
            || v6.is_unspecified()
            || v6.is_multicast()
            || (segs[0] & 0xfe00) == 0xfc00  // unique local (fc00::/7)
            || (segs[0] & 0xffc0) == 0xfe80  // link-local (fe80::/10)
            || v6.to_ipv4_mapped().is_some_and(|v4| {
                let [a, b, _, _] = v4.octets();
                a == 0 || a == 10 || a == 127
                    || (a == 169 && b == 254)
                    || (a == 172 && (16..=31).contains(&b))
                    || (a == 192 && b == 168)
                    || (a == 100 && (64..=127).contains(&b))
                    || a >= 240
            });
    }

    false
}

fn read_qwen_oauth_cached_credentials() -> Option<QwenOauthCredentials> {
    let path = qwen_oauth_credentials_file_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<QwenOauthCredentials>(&content).ok()
}

fn read_claude_code_cached_credentials() -> Option<ClaudeCodeCredentials> {
    let path = claude_code_credentials_file_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    let parsed = serde_json::from_str::<ClaudeCodeCredentialsFile>(&content).ok()?;
    parsed.claude_ai_oauth
}

pub(crate) fn write_claude_code_cached_credentials(credentials: &ClaudeCodeCredentials) -> anyhow::Result<()> {
    let Some(path) = claude_code_credentials_file_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut root = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Claude credentials root must be an object"))?;

    if !root_obj.get("claudeAiOauth").is_some_and(serde_json::Value::is_object) {
        root_obj.insert("claudeAiOauth".to_string(), serde_json::json!({}));
    }
    let oauth_obj = root_obj
        .get_mut("claudeAiOauth")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("claudeAiOauth must be an object"))?;

    if let Some(access_token) = credentials
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        oauth_obj.insert("accessToken".to_string(), serde_json::json!(access_token));
    }
    if let Some(refresh_token) = credentials
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        oauth_obj.insert("refreshToken".to_string(), serde_json::json!(refresh_token));
    }
    if let Some(expires_at) = credentials.expires_at {
        oauth_obj.insert("expiresAt".to_string(), serde_json::json!(expires_at));
    }
    if let Some(subscription_type) = credentials
        .subscription_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        oauth_obj.insert("subscriptionType".to_string(), serde_json::json!(subscription_type));
    }

    let serialized = serde_json::to_string(&root)?;
    std::fs::write(path, serialized)?;
    Ok(())
}

const fn normalized_qwen_expiry_millis(raw: i64) -> i64 {
    if raw < 10_000_000_000 {
        raw.saturating_mul(1000)
    } else {
        raw
    }
}

fn qwen_oauth_token_expired(credentials: &QwenOauthCredentials) -> bool {
    let Some(expiry) = credentials.expiry_date else {
        return false;
    };

    let expiry_millis = normalized_qwen_expiry_millis(expiry);
    let now_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX);

    expiry_millis <= now_millis.saturating_add(30_000)
}

fn claude_code_token_expired(credentials: &ClaudeCodeCredentials) -> bool {
    let Some(expires_at) = credentials.expires_at else {
        return false;
    };

    let now_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX);

    expires_at <= now_millis.saturating_add(300_000)
}

fn refresh_qwen_oauth_access_token(refresh_token: &str) -> anyhow::Result<QwenOauthCredentials> {
    #[cfg(test)]
    credential_resolution_observation::record(2);
    let client_id = qwen_oauth_client_id();
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    let response = client
        .post(oauth_token_endpoint(OauthTokenEndpoint::Qwen))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id.as_str()),
        ])
        .send()
        .map_err(|error| anyhow::anyhow!("Qwen OAuth refresh request failed: {error}"))?;

    let status = response.status();
    let body = response
        .text()
        .unwrap_or_else(|_| "<failed to read Qwen OAuth response body>".to_string());

    let parsed = serde_json::from_str::<QwenOauthTokenResponse>(&body).ok();

    if !status.is_success() {
        let detail = parsed
            .as_ref()
            .and_then(|payload| payload.error_description.as_deref())
            .or_else(|| parsed.as_ref().and_then(|payload| payload.error.as_deref()))
            .filter(|msg| !msg.trim().is_empty())
            .unwrap_or(body.as_str());
        anyhow::bail!("Qwen OAuth refresh failed (HTTP {status}): {detail}");
    }

    let payload = parsed.ok_or_else(|| anyhow::anyhow!("Qwen OAuth refresh response is not JSON"))?;

    if let Some(error_code) = payload.error.as_deref().filter(|value| !value.trim().is_empty()) {
        let detail = payload.error_description.as_deref().unwrap_or(error_code);
        anyhow::bail!("Qwen OAuth refresh failed: {detail}");
    }

    let access_token = payload
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Qwen OAuth refresh response missing access_token"))?
        .to_string();

    let expiry_date = payload.expires_in.and_then(|seconds| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_secs()).ok())?;
        now_secs
            .checked_add(seconds)
            .and_then(|unix_secs| unix_secs.checked_mul(1000))
    });

    Ok(QwenOauthCredentials {
        access_token: Some(access_token),
        refresh_token: payload
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
        resource_url: payload
            .resource_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
        expiry_date,
    })
}

/// An external token endpoint as a request sees it.
#[cfg(not(test))]
pub(crate) const fn token_host(url: &'static str) -> &'static str {
    url
}

/// An external token endpoint as a request sees it in a test build: a closed
/// loopback port with the same path, so no test reaches the real host
/// (D-081).
#[cfg(test)]
pub(crate) fn token_host(url: &'static str) -> String {
    format!("http://127.0.0.1:9/{}", url.trim_start_matches("https://"))
}

/// An OAuth token endpoint PRX refreshes a provider credential at.
#[derive(Clone, Copy)]
enum OauthTokenEndpoint {
    ClaudeCode,
    Qwen,
    Minimax(MinimaxOauthRegion),
}

impl OauthTokenEndpoint {
    const fn real(self) -> &'static str {
        match self {
            Self::ClaudeCode => CLAUDE_CODE_OAUTH_TOKEN_ENDPOINT,
            Self::Qwen => QWEN_OAUTH_TOKEN_ENDPOINT,
            Self::Minimax(region) => region.token_endpoint(),
        }
    }
}

/// Where a credential is refreshed.
#[cfg(not(test))]
const fn oauth_token_endpoint(endpoint: OauthTokenEndpoint) -> &'static str {
    endpoint.real()
}

/// Where a credential is refreshed in a test build: the local listener a test
/// installed, else a closed loopback port. No test reaches a real token
/// endpoint (D-081).
#[cfg(test)]
fn oauth_token_endpoint(endpoint: OauthTokenEndpoint) -> String {
    let installed = OAUTH_TEST_OVERRIDES.read().token_endpoint.clone();
    installed.unwrap_or_else(|| token_host(endpoint.real()))
}

/// The home the Claude Code and Qwen credential files are under.
#[cfg(not(test))]
fn oauth_credentials_home() -> Option<PathBuf> {
    crate::config::user_home_dir()
}

/// The home the Claude Code and Qwen credential files are under: a test
/// installs its own, independent of the process-wide test home other modules
/// change concurrently.
#[cfg(test)]
fn oauth_credentials_home() -> Option<PathBuf> {
    let installed = OAUTH_TEST_OVERRIDES.read().home.clone();
    installed.or_else(crate::config::user_home_dir)
}

/// What a test installed for the OAuth credential lookups and refreshes.
#[cfg(test)]
#[derive(Default)]
struct OauthTestOverrides {
    home: Option<PathBuf>,
    token_endpoint: Option<String>,
}

#[cfg(test)]
static OAUTH_TEST_OVERRIDES: parking_lot::RwLock<OauthTestOverrides> = parking_lot::RwLock::new(OauthTestOverrides {
    home: None,
    token_endpoint: None,
});

pub(crate) fn refresh_claude_code_access_token(refresh_token: &str) -> anyhow::Result<ClaudeCodeCredentials> {
    #[cfg(test)]
    credential_resolution_observation::record(2);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    // TODO: Verify this endpoint against official Anthropic OAuth docs/source if it changes.
    let response = client
        .post(oauth_token_endpoint(OauthTokenEndpoint::ClaudeCode))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLAUDE_CODE_OAUTH_CLIENT_ID),
        ])
        .send()
        .map_err(|error| anyhow::anyhow!("Claude Code OAuth refresh request failed: {error}"))?;

    let status = response.status();
    let body = response
        .text()
        .unwrap_or_else(|_| "<failed to read Claude Code OAuth response body>".to_string());
    let parsed = serde_json::from_str::<ClaudeCodeOauthTokenResponse>(&body).ok();

    if !status.is_success() {
        let detail = parsed
            .as_ref()
            .and_then(|payload| payload.error_description.as_deref())
            .or_else(|| parsed.as_ref().and_then(|payload| payload.error.as_deref()))
            .filter(|msg| !msg.trim().is_empty())
            .unwrap_or(body.as_str());
        anyhow::bail!("Claude Code OAuth refresh failed (HTTP {status}): {detail}");
    }

    let payload = parsed.ok_or_else(|| anyhow::anyhow!("Claude Code OAuth refresh response is not JSON"))?;

    if let Some(error_code) = payload
        .error
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let detail = payload.error_description.as_deref().unwrap_or(error_code);
        anyhow::bail!("Claude Code OAuth refresh failed: {detail}");
    }

    let access_token = payload
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Claude Code OAuth refresh response missing access_token"))?
        .to_string();

    let expires_at = payload.expires_in.and_then(|seconds| {
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())?;
        seconds
            .checked_mul(1000)
            .and_then(|duration_millis| now_millis.checked_add(duration_millis))
    });

    Ok(ClaudeCodeCredentials {
        access_token: Some(access_token),
        refresh_token: payload
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(ToString::to_string),
        expires_at,
        subscription_type: None,
    })
}

fn resolve_qwen_oauth_context(credential_override: Option<&str>, refresh: OauthRefresh) -> QwenOauthProviderContext {
    let override_value = credential_override.map(str::trim).filter(|value| !value.is_empty());
    let placeholder_requested = override_value
        .map(|value| value.eq_ignore_ascii_case(QWEN_OAUTH_PLACEHOLDER))
        .unwrap_or(false);

    if let Some(explicit) = override_value {
        if !placeholder_requested {
            return QwenOauthProviderContext {
                credential: Some(explicit.to_string()),
                base_url: None,
                refresh_withheld: false,
            };
        }
    }

    let mut cached = read_qwen_oauth_cached_credentials();

    let env_token = read_non_empty_env(QWEN_OAUTH_TOKEN_ENV);
    let env_refresh_token = read_non_empty_env(QWEN_OAUTH_REFRESH_TOKEN_ENV);
    let env_resource_url = read_non_empty_env(QWEN_OAUTH_RESOURCE_URL_ENV);
    let mut refresh_withheld = false;

    if env_token.is_none() {
        let refresh_token = env_refresh_token.or_else(|| {
            cached
                .as_ref()
                .and_then(|credentials| credentials.refresh_token.clone())
        });

        let should_refresh = cached.as_ref().is_some_and(qwen_oauth_token_expired)
            || cached
                .as_ref()
                .and_then(|credentials| credentials.access_token.as_deref())
                .is_none_or(|value| value.trim().is_empty());

        refresh_withheld = should_refresh && refresh_token.is_some() && refresh == OauthRefresh::Never;
        if should_refresh && !refresh_withheld {
            if let Some(refresh_token) = refresh_token.as_deref() {
                match refresh_qwen_oauth_access_token(refresh_token) {
                    Ok(refreshed) => {
                        cached = Some(refreshed);
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "Qwen OAuth refresh failed");
                    }
                }
            }
        }
    }

    let mut credential = env_token.or_else(|| cached.as_ref().and_then(|credentials| credentials.access_token.clone()));
    credential = credential
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    if credential.is_none() && !placeholder_requested {
        credential = read_non_empty_env("DASHSCOPE_API_KEY");
    }

    let base_url = env_resource_url
        .as_deref()
        .and_then(normalize_qwen_oauth_base_url)
        .or_else(|| {
            cached
                .as_ref()
                .and_then(|credentials| credentials.resource_url.as_deref())
                .and_then(normalize_qwen_oauth_base_url)
        });

    QwenOauthProviderContext {
        credential,
        base_url,
        refresh_withheld,
    }
}

fn is_claude_code_oauth_placeholder(value: &str) -> bool {
    is_claude_code_alias(value)
}

fn resolve_claude_code_context(credential_override: Option<&str>, refresh: OauthRefresh) -> ClaudeCodeProviderContext {
    let override_value = credential_override.map(str::trim).filter(|value| !value.is_empty());
    let placeholder_requested = override_value.map(is_claude_code_oauth_placeholder).unwrap_or(false);

    if let Some(explicit) = override_value {
        if !placeholder_requested {
            return ClaudeCodeProviderContext {
                credential: Some(explicit.to_string()),
                refresh_token: None,
                expires_at: None,
                refresh_withheld: false,
            };
        }
    }

    // Only file-based credential reading: ~/.claude/.credentials.json
    let mut cached = read_claude_code_cached_credentials();

    let refresh_token = cached
        .as_ref()
        .and_then(|credentials| credentials.refresh_token.clone());
    let should_refresh = cached.as_ref().is_some_and(claude_code_token_expired)
        || cached
            .as_ref()
            .and_then(|credentials| credentials.access_token.as_deref())
            .is_none_or(|token| token.trim().is_empty());
    let refresh_withheld = should_refresh && refresh_token.is_some() && refresh == OauthRefresh::Never;

    if should_refresh && !refresh_withheld {
        if let Some(refresh_token) = refresh_token.as_deref() {
            match refresh_claude_code_access_token(refresh_token) {
                Ok(mut refreshed) => {
                    if refreshed.refresh_token.is_none() {
                        refreshed.refresh_token = Some(refresh_token.to_string());
                    }
                    if refreshed.subscription_type.is_none() {
                        refreshed.subscription_type = cached
                            .as_ref()
                            .and_then(|credentials| credentials.subscription_type.clone());
                    }
                    if let Err(error) = write_claude_code_cached_credentials(&refreshed) {
                        tracing::warn!(error = %error, "Failed to write Claude Code credentials");
                    }
                    cached = Some(refreshed);
                }
                Err(error) => {
                    tracing::warn!(error = %error, "Claude Code OAuth refresh failed");
                }
            }
        }
    }

    let credential = cached
        .as_ref()
        .and_then(|credentials| credentials.access_token.clone())
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToString::to_string);

    let refresh_token = cached
        .as_ref()
        .and_then(|credentials| credentials.refresh_token.clone());
    let expires_at = cached.as_ref().and_then(|credentials| credentials.expires_at);

    ClaudeCodeProviderContext {
        credential,
        refresh_token,
        expires_at,
        refresh_withheld,
    }
}

fn refresh_minimax_oauth_access_token(name: &str, refresh_token: &str) -> anyhow::Result<String> {
    #[cfg(test)]
    credential_resolution_observation::record(2);
    let endpoint = oauth_token_endpoint(OauthTokenEndpoint::Minimax(minimax_oauth_region(name)));
    let client_id = minimax_oauth_client_id();
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    let response = client
        .post(endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id.as_str()),
        ])
        .send()
        .map_err(|error| anyhow::anyhow!("MiniMax OAuth refresh request failed: {error}"))?;

    let status = response.status();
    let body = response
        .text()
        .unwrap_or_else(|_| "<failed to read MiniMax OAuth response body>".to_string());

    let parsed = serde_json::from_str::<MinimaxOauthRefreshResponse>(&body).ok();

    if !status.is_success() {
        let detail = parsed
            .as_ref()
            .and_then(|payload| payload.base_resp.as_ref())
            .and_then(|base| base.status_msg.as_deref())
            .filter(|msg| !msg.trim().is_empty())
            .unwrap_or(body.as_str());
        anyhow::bail!("MiniMax OAuth refresh failed (HTTP {status}): {detail}");
    }

    if let Some(payload) = parsed {
        if let Some(status_text) = payload.status.as_deref() {
            if !status_text.eq_ignore_ascii_case("success") {
                let detail = payload
                    .base_resp
                    .as_ref()
                    .and_then(|base| base.status_msg.as_deref())
                    .unwrap_or(status_text);
                anyhow::bail!("MiniMax OAuth refresh failed: {detail}");
            }
        }

        if let Some(token) = payload
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            return Ok(token.to_string());
        }
    }

    anyhow::bail!("MiniMax OAuth refresh response missing access_token");
}

fn resolve_minimax_oauth_refresh_token(name: &str) -> Option<String> {
    let refresh_token = read_non_empty_env(MINIMAX_OAUTH_REFRESH_TOKEN_ENV)?;

    match refresh_minimax_oauth_access_token(name, &refresh_token) {
        Ok(token) => Some(token),
        Err(error) => {
            tracing::warn!(provider = name, error = %error, "MiniMax OAuth refresh failed");
            None
        }
    }
}

pub(crate) fn canonical_china_provider_name(name: &str) -> Option<&'static str> {
    if is_qwen_alias(name) {
        Some("qwen")
    } else if is_glm_alias(name) {
        Some("glm")
    } else if is_moonshot_alias(name) {
        Some("moonshot")
    } else if is_minimax_alias(name) {
        Some("minimax")
    } else if is_zai_alias(name) {
        Some("zai")
    } else if is_qianfan_alias(name) {
        Some("qianfan")
    } else {
        None
    }
}

fn minimax_base_url(name: &str) -> Option<&'static str> {
    if is_minimax_cn_alias(name) {
        Some(MINIMAX_CN_BASE_URL)
    } else if is_minimax_intl_alias(name) {
        Some(MINIMAX_INTL_BASE_URL)
    } else {
        None
    }
}

fn glm_base_url(name: &str) -> Option<&'static str> {
    if is_glm_cn_alias(name) {
        Some(GLM_CN_BASE_URL)
    } else if is_glm_global_alias(name) {
        Some(GLM_GLOBAL_BASE_URL)
    } else {
        None
    }
}

fn moonshot_base_url(name: &str) -> Option<&'static str> {
    if is_moonshot_intl_alias(name) {
        Some(MOONSHOT_INTL_BASE_URL)
    } else if is_moonshot_cn_alias(name) {
        Some(MOONSHOT_CN_BASE_URL)
    } else {
        None
    }
}

fn qwen_base_url(name: &str) -> Option<&'static str> {
    if is_qwen_cn_alias(name) || is_qwen_oauth_alias(name) {
        Some(QWEN_CN_BASE_URL)
    } else if is_qwen_intl_alias(name) {
        Some(QWEN_INTL_BASE_URL)
    } else if is_qwen_us_alias(name) {
        Some(QWEN_US_BASE_URL)
    } else {
        None
    }
}

fn qwen_coding_base_url(name: &str) -> Option<&'static str> {
    if matches!(name, "dashscope-coding" | "qwen-coding") {
        Some(QWEN_CODING_CN_BASE_URL)
    } else if matches!(name, "dashscope-coding-intl" | "qwen-coding-intl") {
        Some(QWEN_CODING_INTL_BASE_URL)
    } else {
        None
    }
}

fn zai_base_url(name: &str) -> Option<&'static str> {
    if is_zai_cn_alias(name) {
        Some(ZAI_CN_BASE_URL)
    } else if is_zai_global_alias(name) {
        Some(ZAI_GLOBAL_BASE_URL)
    } else {
        None
    }
}

/// The regional endpoint a provider name selects, for the vendors whose
/// aliases share one catalog row while addressing different servers.
///
/// A credential issued for one region is not a credential for another, so the
/// identity the credential resolvers compare has to carry the endpoint as well
/// as the vendor. These are the same pure selectors the factory dispatches on,
/// so the identity cannot disagree with the URL the provider is built with.
/// The argument is the trimmed, lowercased name.
fn regional_endpoint_for(name: &str) -> Option<&'static str> {
    qwen_base_url(name)
        .or_else(|| qwen_coding_base_url(name))
        .or_else(|| glm_base_url(name))
        .or_else(|| moonshot_base_url(name))
        .or_else(|| minimax_base_url(name))
        .or_else(|| zai_base_url(name))
}

/// One operator declaration of what an endpoint accepts in a tool schema.
///
/// PRX cannot know what an arbitrary `custom:<url>` endpoint will accept, and a
/// URL is not evidence, so an undeclared endpoint keeps
/// [`ToolSchemaDialect::Raw`](crate::tools::ToolSchemaDialect::Raw) — the
/// strongest enforcement contract — and the first transport rejection carries
/// the diagnostic that says which declaration would fix it.
///
/// Filled from `[[providers.tool_dialects]]` by
/// [`provider_runtime_options_from_config`]. The declaration belongs to the
/// per-(provider, model) capability descriptor, and that descriptor replaces
/// this struct when it lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSchemaDialectDeclaration {
    /// The provider it applies to. Compared through
    /// [`canonical_provider_key`], so every spelling of one provider identity
    /// shares the declaration.
    pub provider: String,
    /// The model the declaration applies to, or every model of that provider.
    pub model: Option<String>,
    /// What that endpoint accepts.
    pub dialect: crate::tools::ToolSchemaDialect,
}

/// Which tool-call grammars an endpoint was declared to emit.
///
/// A model served behind an OpenAI-compatible API answers a tool request in
/// the provider's native `tool_calls` array. Some chat templates *also* emit a
/// textual grammar in the content — `<function=NAME>…</function>` is the
/// Qwen/Llama spelling — and reading it is a capability, not a courtesy: it
/// decides whether prose an endpoint produced becomes an executed tool call.
/// So it is declared, and the default is that nothing textual is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ToolCallDialect {
    /// Only the provider's native `tool_calls` array is a tool call.
    #[default]
    Native,
    /// Additionally read `<function=NAME>{json}</function>` from the content.
    ///
    /// The tool name must be one this request registered and the body must
    /// parse as JSON; a block failing either is reported, never executed.
    XmlFunctionTags,
}

/// One operator declaration of a textual tool-call grammar an endpoint emits.
///
/// Like [`ToolSchemaDialectDeclaration`], this is filled from
/// `[[providers.tool_dialects]]` and is replaced by the per-(provider, model)
/// capability descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallDialectDeclaration {
    /// The provider it applies to, compared through [`canonical_provider_key`].
    pub provider: String,
    /// The model the declaration applies to, or every model of that provider.
    pub model: Option<String>,
    /// What that endpoint emits.
    pub dialect: ToolCallDialect,
}

#[derive(Debug, Clone)]
pub struct ProviderRuntimeOptions {
    /// The global baseline; invocation-specific temperatures remain higher precedence.
    pub default_temperature: Option<f64>,
    pub model_settings:
        Option<std::sync::Arc<std::collections::BTreeMap<String, crate::config::model_settings::ModelSettings>>>,
    pub auth_profile_override: Option<String>,
    pub openprx_dir: Option<PathBuf>,
    pub secrets_encrypt: bool,
    pub codex_auth_json_path: Option<PathBuf>,
    pub codex_auth_json_auto_import: bool,
    pub reasoning_enabled: Option<bool>,
    pub codex_stream_idle_timeout_secs: Option<u64>,
    pub codex_reasoning_effort: Option<String>,
    pub ollama_num_ctx: Option<usize>,
    pub ollama_model_num_ctx: Vec<(String, usize)>,
    /// What each declared endpoint accepts in a tool schema. Empty means every
    /// endpoint is offered the published document unchanged.
    pub tool_schema_dialects: Vec<ToolSchemaDialectDeclaration>,
    /// Which textual tool-call grammar each declared endpoint emits. Empty
    /// means only native `tool_calls` are read, anywhere.
    pub tool_call_dialects: Vec<ToolCallDialectDeclaration>,
}

impl ProviderRuntimeOptions {
    /// The `(model, dialect)` declarations that apply to `provider`.
    ///
    /// Matched by provider identity, not by spelling: a padded, differently
    /// cased or trailing-slash label is the provider C1's identity says it is,
    /// and a different URL path is a different endpoint.
    #[must_use]
    pub fn declared_tool_schema_dialects(
        &self,
        provider: &str,
    ) -> Vec<(Option<String>, crate::tools::ToolSchemaDialect)> {
        if self.tool_schema_dialects.is_empty() {
            return Vec::new();
        }
        let id = ProviderId::of(provider);
        self.tool_schema_dialects
            .iter()
            .filter(|declaration| ProviderId::of(&declaration.provider) == id)
            .map(|declaration| (declaration.model.clone(), declaration.dialect))
            .collect()
    }

    /// The `(model, dialect)` textual tool-call declarations for `provider`,
    /// matched by provider identity like
    /// [`declared_tool_schema_dialects`](Self::declared_tool_schema_dialects).
    #[must_use]
    pub fn declared_tool_call_dialects(&self, provider: &str) -> Vec<(Option<String>, ToolCallDialect)> {
        if self.tool_call_dialects.is_empty() {
            return Vec::new();
        }
        let id = ProviderId::of(provider);
        self.tool_call_dialects
            .iter()
            .filter(|declaration| ProviderId::of(&declaration.provider) == id)
            .map(|declaration| (declaration.model.clone(), declaration.dialect))
            .collect()
    }
}

impl Default for ProviderRuntimeOptions {
    fn default() -> Self {
        Self {
            default_temperature: None,
            model_settings: Default::default(),
            auth_profile_override: None,
            openprx_dir: None,
            secrets_encrypt: true,
            codex_auth_json_path: None,
            codex_auth_json_auto_import: true,
            reasoning_enabled: None,
            codex_stream_idle_timeout_secs: None,
            codex_reasoning_effort: None,
            ollama_num_ctx: None,
            ollama_model_num_ctx: Vec::new(),
            tool_schema_dialects: Vec::new(),
            tool_call_dialects: Vec::new(),
        }
    }
}

#[must_use]
pub fn provider_runtime_options_from_config(config: &crate::config::Config) -> ProviderRuntimeOptions {
    ProviderRuntimeOptions {
        default_temperature: Some(config.default_temperature),
        model_settings: (!config.model_settings.is_empty()).then(|| std::sync::Arc::new(config.model_settings.clone())),
        auth_profile_override: None,
        openprx_dir: config.config_path.parent().map(PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        codex_auth_json_path: Some(config.auth.codex_auth_json_path.clone()),
        codex_auth_json_auto_import: config.auth.codex_auth_json_auto_import,
        reasoning_enabled: config.runtime.reasoning_enabled,
        codex_stream_idle_timeout_secs: config.runtime.codex_stream_idle_timeout_secs,
        codex_reasoning_effort: config.runtime.codex_reasoning_effort.clone(),
        ollama_num_ctx: config.providers.ollama.num_ctx,
        ollama_model_num_ctx: ollama_model_num_ctx_from_router(&config.router),
        // `[[providers.tool_dialects]]`. An endpoint with no entry keeps the
        // published document (`Raw`) and native tool calls only; nothing is
        // inferred from its URL.
        tool_schema_dialects: tool_schema_dialects_from_config(&config.providers),
        tool_call_dialects: tool_call_dialects_from_config(&config.providers),
    }
}

/// The schema dialects declared in `[[providers.tool_dialects]]` and
/// `[[providers.models]]`. The loader refuses a `(provider, model)` that
/// declares one in both tables, so the two lists never disagree.
fn tool_schema_dialects_from_config(providers: &crate::config::ProvidersConfig) -> Vec<ToolSchemaDialectDeclaration> {
    let from_key = providers
        .tool_dialects
        .iter()
        .filter_map(|entry| Some((entry.provider.as_str(), entry.model.as_deref(), entry.tool_schema?)));
    let from_models = providers.models.iter().filter_map(|entry| {
        Some((
            entry.provider.as_str(),
            entry.model.as_deref(),
            entry.tool_schema_dialect?,
        ))
    });
    from_key
        .chain(from_models)
        .map(|(provider, model, setting)| ToolSchemaDialectDeclaration {
            provider: provider.trim().to_string(),
            model: declared_model(model),
            dialect: tool_schema_dialect_setting(setting),
        })
        .collect()
}

/// The textual tool-call grammars declared in `[[providers.tool_dialects]]`
/// and `[[providers.models]]`.
fn tool_call_dialects_from_config(providers: &crate::config::ProvidersConfig) -> Vec<ToolCallDialectDeclaration> {
    let from_key = providers
        .tool_dialects
        .iter()
        .filter_map(|entry| Some((entry.provider.as_str(), entry.model.as_deref(), entry.tool_call?)));
    let from_models = providers.models.iter().filter_map(|entry| {
        Some((
            entry.provider.as_str(),
            entry.model.as_deref(),
            entry.tool_call_dialect?,
        ))
    });
    from_key
        .chain(from_models)
        .map(|(provider, model, setting)| ToolCallDialectDeclaration {
            provider: provider.trim().to_string(),
            model: declared_model(model),
            dialect: tool_call_dialect_setting(setting),
        })
        .collect()
}

/// The tool-schema dialect a configuration setting names.
pub(crate) const fn tool_schema_dialect_setting(
    setting: crate::config::schema::ToolSchemaDialectSetting,
) -> crate::tools::ToolSchemaDialect {
    match setting {
        crate::config::schema::ToolSchemaDialectSetting::Raw => crate::tools::ToolSchemaDialect::Raw,
        crate::config::schema::ToolSchemaDialectSetting::NoRootCombinators => {
            crate::tools::ToolSchemaDialect::NoRootCombinators
        }
    }
}

/// The tool-call grammar a configuration setting names.
pub(crate) const fn tool_call_dialect_setting(
    setting: crate::config::schema::ToolCallDialectSetting,
) -> ToolCallDialect {
    match setting {
        crate::config::schema::ToolCallDialectSetting::Native => ToolCallDialect::Native,
        crate::config::schema::ToolCallDialectSetting::XmlFunctionTags => ToolCallDialect::XmlFunctionTags,
    }
}

fn declared_model(model: Option<&str>) -> Option<String> {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
}

/// The indices of `[[providers.tool_dialects]]` entries whose provider is none
/// of the providers the configuration uses: the default, the fallbacks, the
/// model routes, the agents and the router's candidate models.
///
/// Such an entry is harmless, but it is usually a typo'd port or path, and the
/// endpoint the operator meant then keeps failing with its own HTTP 400.
pub(crate) fn unused_tool_dialect_entries(config: &crate::config::Config) -> Vec<usize> {
    if config.providers.tool_dialects.is_empty() {
        return Vec::new();
    }
    let used = ConfiguredProviders::from_config(config)
        .with_providers(config.router.models.iter().map(|model| model.provider.as_str()));
    config
        .providers
        .tool_dialects
        .iter()
        .enumerate()
        .filter(|(_, entry)| !used.permits(&entry.provider))
        .map(|(index, _)| index)
        .collect()
}

/// The catalog entries whose constructor builds an `OpenAiCompatibleProvider`
/// against a server the operator runs, and therefore reads
/// `[[providers.tool_dialects]]` (D-020 Q3). Canonical catalog names.
pub(crate) const TOOL_DIALECT_DECLARABLE_CATALOG: [&str; 4] = ["vllm", "llamacpp", "lmstudio", "litellm"];

/// Whether the constructor `name` selects reads a tool-dialect declaration.
///
/// A `custom:<url>` endpoint with a URL that parses, and the catalog entries
/// in [`TOOL_DIALECT_DECLARABLE_CATALOG`] under any of their spellings. Every
/// other provider either has a fixed dialect (`gemini`, `anthropic`,
/// `anthropic-custom:`) or is a hosted catalog vendor whose constructor
/// ignores declarations, so accepting one would be a silently ignored key.
pub(crate) fn accepts_tool_dialect_declarations(name: &str) -> bool {
    match classify_provider(name) {
        ProviderIdentity::ByoEndpoint {
            dialect: ByoDialect::OpenAiCompatible,
            ..
        } => true,
        ProviderIdentity::Catalog(canonical) => TOOL_DIALECT_DECLARABLE_CATALOG.contains(&canonical),
        _ => false,
    }
}

fn ollama_model_num_ctx_from_router(router: &crate::config::RouterConfig) -> Vec<(String, usize)> {
    const ROUTER_DEFAULT_MAX_CONTEXT_SENTINEL: usize = 1_000_000;
    router
        .models
        .iter()
        .filter(|model| {
            model.provider.eq_ignore_ascii_case("ollama")
                && model.max_context > 0
                && model.max_context < ROUTER_DEFAULT_MAX_CONTEXT_SENTINEL
        })
        .map(|model| (model.model_id.clone(), model.max_context))
        .collect()
}

/// How much of an error text [`sanitize_api_error`] scrubs. It prints only
/// [`MAX_API_ERROR_CHARS`] characters, and no credential that starts within
/// them is this long, so scrubbing more would only cost time on a large or
/// crafted upstream body (R-C2BUj J-1).
const MAX_SCRUBBED_ERROR_BYTES: usize = 16 * 1024;

/// How much of an upstream error response body PRX reads into memory. Error
/// bodies are shown sanitized and truncated, so the rest is never needed.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// How long PRX waits for an upstream error body. The streaming client has no
/// whole-request deadline, so without this bound an upstream that sends an
/// error status and then drips (or withholds) its body held the turn until
/// the body reached [`MAX_ERROR_BODY_BYTES`] or ended (R-C2BUk K-7).
pub(crate) const ERROR_BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Read at most [`MAX_ERROR_BODY_BYTES`] of an error response body, lossily
/// decoded, for at most [`ERROR_BODY_READ_TIMEOUT`]: when the time is up, what
/// was read is used. A body that cannot be read at all yields a fixed
/// placeholder.
pub(crate) async fn read_error_body(response: reqwest::Response) -> String {
    read_error_body_within(response, ERROR_BODY_READ_TIMEOUT).await
}

/// [`read_error_body`] with its time bound given: a provider that carries its
/// own bound (the default is [`ERROR_BODY_READ_TIMEOUT`]) passes it here.
pub(crate) async fn read_error_body_within(mut response: reqwest::Response, bound: std::time::Duration) -> String {
    let mut body: Vec<u8> = Vec::new();
    let mut failed = false;
    let read = async {
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let room = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
                    body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
                    if body.len() >= MAX_ERROR_BODY_BYTES {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
    };
    if tokio::time::timeout(bound, read).await.is_err() {
        tracing::warn!(
            timeout_ms = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX),
            "upstream error body did not finish in time; using what was read"
        );
    }
    if failed && body.is_empty() {
        return "<failed to read provider error body>".to_string();
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// Sanitize API error text by scrubbing secrets and truncating length.
///
/// Only the first [`MAX_SCRUBBED_ERROR_BYTES`] are scrubbed; the result is
/// truncated to [`MAX_API_ERROR_CHARS`] characters anyway.
pub fn sanitize_api_error(input: &str) -> String {
    let mut prefix_end = input.len().min(MAX_SCRUBBED_ERROR_BYTES);
    while !input.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let scrubbed = crate::security::redaction::redact_secrets(input.get(..prefix_end).unwrap_or_default());

    if scrubbed.chars().count() <= MAX_API_ERROR_CHARS {
        // A scrubbed prefix of a longer text is still marked as cut.
        return if prefix_end < input.len() {
            format!("{scrubbed}...")
        } else {
            scrubbed
        };
    }

    let mut end = MAX_API_ERROR_CHARS;
    while end > 0 && !scrubbed.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}...", &scrubbed[..end])
}

/// Build a sanitized provider error from a failed HTTP response.
///
/// The real `Retry-After` header is read *before* `response.text()` consumes the
/// body and, when present, attached as a structured
/// [`RetryAfterHint`](rate_limit::RetryAfterHint) source on the returned error.
/// The reliability layer recovers it with
/// [`retry_after_hint_ms`](rate_limit::retry_after_hint_ms) and honors the
/// upstream delay instead of falling back to its own exponential guess. The
/// `Display` form is unchanged, so every existing caller and log line keeps
/// exactly the message it had before.
pub async fn api_error(provider: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let retry_after_ms = traits::retry_after_ms_from_headers(response.headers());
    let body = read_error_body(response).await;
    let sanitized = sanitize_api_error(&body);
    let message = format!("{provider} API error ({status}): {sanitized}");
    match retry_after_ms {
        Some(millis) => anyhow::Error::new(rate_limit::RetryAfterHint { millis }).context(message),
        None => anyhow::anyhow!("{message}"),
    }
}

/// Build a [`StreamError`](traits::StreamError) from a failed streaming HTTP
/// response, reading the real `Retry-After` header before the body is consumed.
///
/// FIX-P0-33: for `429` (Too Many Requests) and `503` (Service Unavailable) the
/// header is parsed into a structured [`StreamError::RateLimited`] so the
/// reliability layer can honor the upstream backoff hint without relying on the
/// error's textual form (which previously dropped the header entirely). All
/// other non-2xx statuses map to [`StreamError::Provider`] as before.
///
/// The `Retry-After` header is captured *before* `response.text()` consumes the
/// response, since reading the body takes ownership.
pub async fn stream_api_error(provider: &str, response: reqwest::Response) -> traits::StreamError {
    use traits::StreamError;
    let status = response.status();
    let code = status.as_u16();
    let retry_after_ms = traits::retry_after_ms_from_headers(response.headers());
    let body = read_error_body(response).await;
    let sanitized = sanitize_api_error(&body);
    if code == 429 || code == 503 {
        StreamError::RateLimited {
            status: code,
            retry_after_ms,
            message: format!("{provider} streaming HTTP {status}: {sanitized}"),
        }
    } else {
        StreamError::Provider(format!("{provider} streaming HTTP {status}: {sanitized}"))
    }
}

/// Resolve API key for a provider from explicit config only.
///
/// Resolution order:
/// 1. Explicitly provided `api_key` parameter (trimmed, filtered if empty)
/// 2. For MiniMax OAuth placeholders: MiniMax OAuth refresh-token flow
///
/// Environment variables are NOT read here. All credentials must come from
/// the config file or auth-profiles.json.
#[cfg(test)]
fn resolve_provider_credential(name: &str, credential_override: Option<&str>) -> Option<String> {
    resolve_provider_credential_with_options(name, credential_override, &ProviderRuntimeOptions::default())
}

fn resolve_provider_credential_with_options(
    name: &str,
    credential_override: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> Option<String> {
    if let Some(raw_override) = credential_override {
        let trimmed_override = raw_override.trim();
        if !trimmed_override.is_empty() {
            if is_minimax_alias(name) && is_minimax_oauth_placeholder(trimmed_override) {
                if let Some(credential) = resolve_minimax_oauth_refresh_token(name) {
                    return Some(credential);
                }
                return None;
            }
            return Some(trimmed_override.to_owned());
        }
    }

    // Bedrock uses AWS AKSK credentials, not a single API key.
    // Credential resolution happens inside BedrockProvider.
    if matches!(name, "bedrock" | "aws-bedrock") {
        return None;
    }

    if let Some(credential) = resolve_auth_profile_credential(name, options) {
        return Some(credential);
    }

    None
}

#[derive(Clone, Default)]
struct ResolvedProviderCredentials {
    credential: Option<String>,
    qwen_base_url: Option<String>,
    claude_refresh_token: Option<String>,
    claude_expires_at: Option<i64>,
    /// An OAuth refresh was due and [`OauthRefresh::Never`] withheld it.
    refresh_withheld: bool,
}

impl ResolvedProviderCredentials {
    fn is_usable_for(&self, provider: &str) -> bool {
        if matches!(provider, "bedrock" | "aws-bedrock") {
            return true;
        }
        if matches!(provider, "gemini" | "google" | "google-gemini") {
            return self.credential.is_some() || gemini::GeminiProvider::has_any_auth();
        }
        self.credential.as_deref().is_some_and(|credential| {
            !is_claude_code_oauth_setup_token(credential) || self.claude_refresh_token.is_some()
        })
    }
}

/// Whether a credential lookup may refresh a due OAuth token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OauthRefresh {
    /// Construction: a due token is refreshed (a request to the provider's
    /// token endpoint; for Claude Code, a rewrite of its credential file).
    Allowed,
    /// A probe: nothing is refreshed and nothing is written; a due refresh
    /// is reported instead (D-080 T-7b).
    Never,
}

/// Sole provider credential resolver used by construction and availability.
/// It preserves provider-specific OAuth metadata while applying the common
/// explicit/config-profile precedence exactly once.
fn resolve_provider_credentials(
    name: &str,
    credential_override: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> ResolvedProviderCredentials {
    resolve_provider_credentials_with(name, credential_override, options, OauthRefresh::Allowed)
}

/// [`resolve_provider_credentials`] under an explicit refresh policy.
fn resolve_provider_credentials_with(
    name: &str,
    credential_override: Option<&str>,
    options: &ProviderRuntimeOptions,
    refresh: OauthRefresh,
) -> ResolvedProviderCredentials {
    #[cfg(test)]
    credential_resolution_observation::record(0);
    if is_qwen_oauth_alias(name) {
        let context = resolve_qwen_oauth_context(credential_override, refresh);
        return ResolvedProviderCredentials {
            credential: context.credential,
            qwen_base_url: context.base_url,
            refresh_withheld: context.refresh_withheld,
            ..ResolvedProviderCredentials::default()
        };
    }

    // A MiniMax OAuth placeholder is exchanged for an access token on every
    // construction: that exchange is a refresh.
    if refresh == OauthRefresh::Never
        && is_minimax_alias(name)
        && credential_override
            .map(str::trim)
            .is_some_and(is_minimax_oauth_placeholder)
    {
        return ResolvedProviderCredentials {
            refresh_withheld: read_non_empty_env(MINIMAX_OAUTH_REFRESH_TOKEN_ENV).is_some(),
            ..ResolvedProviderCredentials::default()
        };
    }

    let placeholder_requested = credential_override
        .map(str::trim)
        .is_some_and(is_claude_code_oauth_placeholder);
    if is_claude_code_alias(name) || (name == "anthropic" && (credential_override.is_none() || placeholder_requested)) {
        let context = resolve_claude_code_context(credential_override, refresh);
        let fallback_override = (!placeholder_requested).then_some(credential_override).flatten();
        let fallback_name = if is_claude_code_alias(name) { "anthropic" } else { name };
        return ResolvedProviderCredentials {
            credential: context
                .credential
                .or_else(|| resolve_provider_credential_with_options(fallback_name, fallback_override, options)),
            claude_refresh_token: context.refresh_token,
            claude_expires_at: context.expires_at,
            refresh_withheld: context.refresh_withheld,
            ..ResolvedProviderCredentials::default()
        };
    }

    ResolvedProviderCredentials {
        credential: resolve_provider_credential_with_options(name, credential_override, options),
        ..ResolvedProviderCredentials::default()
    }
}

/// What a connection test may present for a provider.
pub(crate) enum ProbeCredential {
    /// The credential construction would present (`None`: it has none).
    Ready(Option<String>),
    /// Only an OAuth refresh would produce the credential construction
    /// presents, and a probe does not refresh.
    RefreshDue,
}

/// The credential provider construction presents for `name` (the explicit,
/// resolver-released credential, else the provider's own store lookup, with
/// every provider-specific OAuth context), looked up without side effects:
/// no OAuth token is refreshed and no credential file is written (D-078 T-7,
/// D-080 T-7b). It reads files and takes the auth-profile lock, so it
/// blocks: call it off the async runtime.
pub(crate) fn probe_credential(
    name: &str,
    credential_override: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> ProbeCredential {
    let resolved = resolve_provider_credentials_with(name, credential_override, options, OauthRefresh::Never);
    if resolved.refresh_withheld {
        ProbeCredential::RefreshDue
    } else {
        ProbeCredential::Ready(resolved.credential)
    }
}

fn resolve_auth_profile_credential(name: &str, options: &ProviderRuntimeOptions) -> Option<String> {
    #[cfg(test)]
    credential_resolution_observation::record(1);
    let openprx_dir = options.openprx_dir.as_ref()?;
    let store = AuthProfilesStore::new(openprx_dir, options.secrets_encrypt);
    let data = store.load_read_only().ok()?;
    let profile_id = auth_profile_keys(name)
        .iter()
        .find_map(|provider| select_profile_id(&data, provider, options.auth_profile_override.as_deref()))?;
    let profile = data.profiles.get(&profile_id)?;
    profile
        .token
        .as_deref()
        .or_else(|| profile.token_set.as_ref().map(|tokens| tokens.access_token.as_str()))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
}

/// The auth-profile provider keys a provider's own credential is read under,
/// in order. A `custom:`, `anthropic-custom:` or `wasm:` identity is
/// case-sensitive, so its token is stored under the `ProviderId` key
/// (`auth::normalize_provider`); a token stored lowercased before that is
/// still read, but only when lowercasing the spelling does not change its
/// identity, so another path's or plugin's token is never read.
fn auth_profile_keys(name: &str) -> Vec<String> {
    let identity = ProviderId::of(name);
    match identity.namespace() {
        ProviderNamespace::Custom | ProviderNamespace::AnthropicCustom | ProviderNamespace::Wasm => {
            let legacy = name.trim().to_ascii_lowercase();
            let mut keys = vec![identity.as_str().to_string()];
            if ProviderId::of(&legacy) == identity && legacy != identity.as_str() {
                keys.push(legacy);
            }
            keys
        }
        ProviderNamespace::DecisionService => vec![identity.as_str().to_string()],
        ProviderNamespace::Catalog | ProviderNamespace::Unknown => vec![canonical_auth_profile_provider(name)],
    }
}

/// A request refused before transport because the endpoint's declared
/// tool-schema dialect cannot carry some of the tools it offered.
///
/// The rendering keeps the `provider_capability_error … capability=tool_schema_dialect`
/// shape every capability refusal has, so a text-only consumer (a stream
/// error) still recognises it by its start ([`is_tool_schema_refusal_text`]);
/// typed consumers downcast to this type instead of matching text. It is
/// deterministic: the same request is refused the same way every time, so the
/// reliability layer never retries it against the same provider.
#[derive(Debug, Clone, thiserror::Error)]
#[error("provider_capability_error provider={provider} capability=tool_schema_dialect message={refusals}")]
pub struct ToolSchemaRefusalError {
    /// The provider implementation that refused (its display name).
    pub provider: String,
    /// Every refused tool and why.
    pub refusals: crate::tools::ToolSchemaRefusals,
}

/// How every rendered capability refusal starts (see [`ToolSchemaRefusalError`]).
const CAPABILITY_REFUSAL_PREFIX: &str = "provider_capability_error provider=";

/// How a stream error renders a provider message (`StreamError::Provider`),
/// the one form in which a refusal crosses the streaming boundary.
const STREAM_PROVIDER_ERROR_PREFIX: &str = "Provider error: ";

/// Whether `text` is a rendered [`ToolSchemaRefusalError`], as the streaming
/// boundary carries it: the text *starts with* the refusal's own header
/// (optionally inside a stream error's `Provider error: ` rendering), and that
/// header names the tool-schema capability.
///
/// Text that merely contains `capability=tool_schema_dialect` is not a refusal:
/// an upstream error body is rendered after `… API error (NNN): `, and a
/// hostile or confused upstream can quote the marker there (SP-INT2 a51).
#[must_use]
pub fn is_tool_schema_refusal_text(text: &str) -> bool {
    let text = text.strip_prefix(STREAM_PROVIDER_ERROR_PREFIX).unwrap_or(text);
    text.strip_prefix(CAPABILITY_REFUSAL_PREFIX)
        .and_then(|rest| rest.split_once(" capability="))
        .is_some_and(|(_, capability)| capability.starts_with("tool_schema_dialect message="))
}

/// The refused tools, if a tool-schema refusal ended the request `error`
/// reports.
///
/// Typed first: the leaf refusal, or a failover chain whose last candidate
/// was refused, anywhere in the cause chain. A boundary that carries errors
/// as text (the streaming path) loses the type, so a cause whose text *is* a
/// rendered refusal ([`is_tool_schema_refusal_text`]) is the fallback; text
/// that only quotes one is not.
#[must_use]
pub fn tool_schema_refused_tools(error: &anyhow::Error) -> Option<Vec<String>> {
    for cause in error.chain() {
        if let Some(refusal) = cause.downcast_ref::<ToolSchemaRefusalError>() {
            return Some(refusal.refusals.tool_names().map(str::to_string).collect());
        }
        if let Some(refusal) = cause
            .downcast_ref::<reliable::ProviderChainFailure>()
            .and_then(reliable::ProviderChainFailure::tool_schema_refusal)
        {
            return Some(refusal.refusals.tool_names().map(str::to_string).collect());
        }
    }
    error
        .chain()
        .map(ToString::to_string)
        .find(|text| is_tool_schema_refusal_text(text))
        .and_then(|text| crate::tools::ToolSchemaRefusals::tool_names_in(&text))
}

/// A provider string as it is safe to echo back.
///
/// A bring-your-own endpoint is named by its URL, and a URL may carry
/// userinfo, which is a credential. The scheme, host and port are what an
/// operator needs to recognise the endpoint; the userinfo must not reach
/// terminal output or an error chain.
///
/// The input may be a bare label or free text that quotes labels (an error
/// chain, a failover summary): every URL authority in it that carries
/// userinfo is rewritten to `scheme://[redacted]@host`. The rewrite is
/// idempotent, so text redacted once can pass through here again unchanged.
///
/// The authority is delimited the way the WHATWG URL parser behind
/// `reqwest::Url` delimits it, so whatever the transport would send as
/// userinfo is what is redacted: a special scheme's separator may be written
/// with any number of `/` or `\`; the authority ends at the first `/`, `?`
/// or `#` (and `\` for a special scheme); the userinfo ends at the last `@`
/// inside it, but never reaches into a later URL that carries its own
/// userinfo. Quotes, spaces, backticks, angle brackets and non-ASCII
/// characters do not end it: the parser accepts them in a password. A `//`
/// authority that does not parse (a password holding a literal `/`) is
/// redacted conservatively, through the last `@` before the next URL that
/// carries its own userinfo. No input can make this function panic: every
/// slice goes through `get` at a char boundary. It runs in time linear in
/// the text (see [`TextIndex`]), so an adversarial upstream body cannot
/// stall the caller.
///
/// `docs/providers.md` lists what the tests prove and the known limits.
#[must_use]
pub fn redact_endpoint_userinfo(provider: &str) -> std::borrow::Cow<'_, str> {
    if !provider.contains('@') {
        return std::borrow::Cow::Borrowed(provider);
    }
    // First pass: a bare label is decided by the parser itself, so the two
    // can never disagree about whether the transport would send userinfo.
    // It is only a first pass: text that merely *begins* with a URL also
    // parses as one URL (the rest becomes its path), so the free-text scan
    // always runs over the result and redacts every later userinfo. The scan
    // leaves `[redacted]@` alone, so running it again is safe.
    match redact_parsed_label(provider) {
        Some(std::borrow::Cow::Owned(first)) => match redact_free_text(&first) {
            std::borrow::Cow::Owned(both) => std::borrow::Cow::Owned(both),
            std::borrow::Cow::Borrowed(_) => std::borrow::Cow::Owned(first),
        },
        Some(std::borrow::Cow::Borrowed(_)) | None => redact_free_text(provider),
    }
}

/// Redact the userinfo of every URL authority in free text (see
/// [`redact_endpoint_userinfo`]).
///
/// Every index below is a char boundary: it is either a position `find` or
/// `rfind` returned for an ASCII character (`:`, `@`, a separator or a
/// terminator), or a position advanced past a whole character with
/// `len_utf8`. Slices go through `get`, so even a broken invariant ends the
/// scan instead of panicking.
///
/// The cost is linear in the text (R-C2BUj J-1): the positions of `@` and of
/// every authority terminator are indexed once ([`TextIndex`]), so an
/// authority's end, whether it holds an `@`, and the next credentialed URL
/// are each a binary search, never a rescan of the rest of the text.
fn redact_free_text(provider: &str) -> std::borrow::Cow<'_, str> {
    let index = TextIndex::new(provider);
    let mut output = String::new();
    let mut copied = 0;
    let mut search_from = 0;
    while let Some(found) = next_url_authority(provider, search_from) {
        let authority_end = index.authority_end(found.authority_start, found.special);
        let authority_len = authority_end - found.authority_start;
        // The userinfo ends at the last `@` of the authority, but never
        // reaches into a later URL that carries its own userinfo: a
        // non-special authority does not end at `\`, so without this bound it
        // could swallow `ws:\\x@h` on one pass but not on the next. The bound
        // can only matter when the authority holds an `@` at all.
        let userinfo_end = if index.has_at(found.authority_start, authority_end) {
            let bound = found.authority_start
                + authority_len.min(index.userinfo_bound_after(found.authority_start, found.authority_start));
            index.last_at(found.authority_start, bound)
        } else if found.separators >= 2 {
            // The conservative fallback is for a `//` authority that a
            // literal `/` in the password cut short; a spelling with fewer
            // separators is matched only where the parser itself reads it as
            // an authority.
            conservative_userinfo_end(provider, &index, &found, authority_end)
        } else {
            None
        };
        let Some(at) = userinfo_end else {
            search_from = found.authority_start;
            continue;
        };
        let userinfo = provider.get(found.authority_start..at).unwrap_or_default();
        // Already redacted, or the placeholder a pasteable declaration prints.
        if !matches!(userinfo, REDACTED_USERINFO | USERINFO_PLACEHOLDER) {
            output.push_str(provider.get(copied..found.authority_start).unwrap_or_default());
            output.push_str(REDACTED_USERINFO);
            copied = at;
        }
        search_from = at + 1;
    }
    if copied == 0 {
        return std::borrow::Cow::Borrowed(provider);
    }
    output.push_str(provider.get(copied..).unwrap_or_default());
    std::borrow::Cow::Owned(output)
}

// Bytes the userinfo redactor examined on this thread: a deterministic work
// count, so a test can prove the scan linear without a clock (R-C2BUk K-4).
// Every scan step records what it reads; a test resets it, redacts, and
// compares the count across input sizes.
#[cfg(test)]
thread_local! {
    static USERINFO_SCAN_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_userinfo_scan_work(bytes: usize) {
    USERINFO_SCAN_WORK.with(|work| work.set(work.get().saturating_add(bytes)));
}

/// The bytes [`redact_endpoint_userinfo`] examines for `text`, counted on this
/// thread.
#[cfg(test)]
pub(crate) fn userinfo_scan_work(text: &str) -> usize {
    USERINFO_SCAN_WORK.with(|work| work.set(0));
    std::hint::black_box(redact_endpoint_userinfo(text).len());
    USERINFO_SCAN_WORK.with(std::cell::Cell::get)
}

/// The positions the scan asks about, found once per text, so every question
/// is a binary search: where an authority ends, whether a span holds an `@`,
/// the last `@` in a span, and the next URL whose own authority carries an
/// `@`.
struct TextIndex<'a> {
    text: &'a str,
    /// Positions of every `@`.
    ats: Vec<usize>,
    /// Positions of `/`, `\`, `?` and `#`: where a special authority ends.
    special_ends: Vec<usize>,
    /// Positions of `/`, `?` and `#`: where a non-special authority ends
    /// (the parser keeps `\` in a non-special userinfo).
    plain_ends: Vec<usize>,
    /// Every URL written with at least two separators whose authority holds
    /// an `@`, as `(colon, scheme start)`, in text order. Built lazily.
    credentialed: std::cell::OnceCell<Vec<(usize, usize)>>,
    text_len: usize,
}

impl<'a> TextIndex<'a> {
    fn new(text: &'a str) -> Self {
        #[cfg(test)]
        record_userinfo_scan_work(text.len());
        let mut ats = Vec::new();
        let mut special_ends = Vec::new();
        let mut plain_ends = Vec::new();
        for (position, byte) in text.bytes().enumerate() {
            match byte {
                b'@' => ats.push(position),
                b'\\' => special_ends.push(position),
                b'/' | b'?' | b'#' => {
                    special_ends.push(position);
                    plain_ends.push(position);
                }
                _ => {}
            }
        }
        Self {
            text,
            ats,
            special_ends,
            plain_ends,
            credentialed: std::cell::OnceCell::new(),
            text_len: text.len(),
        }
    }

    /// Every URL written with at least two separators whose own authority
    /// holds an `@`, as `(colon, scheme start)`, from one pass over the text.
    fn scan_credentialed(&self) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let mut from = 0;
        while let Some(url) = next_url_authority(self.text, from) {
            if url.separators >= 2
                && self.has_at(
                    url.authority_start,
                    self.authority_end(url.authority_start, url.special),
                )
            {
                found.push((url.colon, url.scheme_start));
            }
            from = url.authority_start;
        }
        found
    }

    /// Where the authority starting at `start` ends: the first `/`, `?` or
    /// `#` at or after it (and `\` for a special scheme), or the end.
    fn authority_end(&self, start: usize, special: bool) -> usize {
        let ends = if special { &self.special_ends } else { &self.plain_ends };
        let index = ends.partition_point(|&position| position < start);
        ends.get(index).copied().unwrap_or(self.text_len)
    }

    /// Whether `[start, end)` holds an `@`.
    fn has_at(&self, start: usize, end: usize) -> bool {
        let index = self.ats.partition_point(|&position| position < start);
        self.ats.get(index).is_some_and(|&position| position < end)
    }

    /// The last `@` in `[start, end)`.
    fn last_at(&self, start: usize, end: usize) -> Option<usize> {
        let index = self.ats.partition_point(|&position| position < end);
        index
            .checked_sub(1)
            .and_then(|last| self.ats.get(last).copied())
            .filter(|&position| position >= start)
    }

    /// The offset from `authority_start` of the first URL whose colon is at
    /// or after `from`, written with at least two separators, whose own
    /// authority holds an `@`; or the rest of the text. A userinfo never
    /// reaches into such a URL: that `@` belongs to it.
    ///
    /// Only a `//` (or, for a special scheme, `\\`) URL counts: a special
    /// scheme word with a colon and no separators (`k9-wss:Q`) inside a
    /// password is part of that password to the parser (R-C2BUj J-2).
    fn userinfo_bound_after(&self, authority_start: usize, from: usize) -> usize {
        let whole = self.text_len.saturating_sub(authority_start);
        let credentialed = self.credentialed.get_or_init(|| self.scan_credentialed());
        let index = credentialed.partition_point(|&(colon, _)| colon < from);
        credentialed.get(index).map_or(whole, |&(_, scheme_start)| {
            scheme_start.max(from).saturating_sub(authority_start).min(whole)
        })
    }
}

/// A redacted provider label as it can be pasted into a declaration: the
/// `[redacted]` userinfo is replaced by `<user>:<password>` placeholders, and
/// the second value says whether that happened, so the caller can tell the
/// operator to fill them in. A declaration matches by provider identity,
/// which keeps userinfo, so a pasted `[redacted]` would match nothing (and
/// the configuration API reads it as a mask).
#[must_use]
pub fn pasteable_provider_label(label: &str) -> (std::borrow::Cow<'_, str>, bool) {
    // After whatever separator the label was spelled with (`//`, `\\`, `/`,
    // none): the marker is what the redactor leaves in place of userinfo.
    const MARKER: &str = "[redacted]@";
    if label.contains(MARKER) {
        (
            std::borrow::Cow::Owned(label.replace(MARKER, "<user>:<password>@")),
            true,
        )
    } else {
        (std::borrow::Cow::Borrowed(label), false)
    }
}

/// The marker a redacted userinfo leaves behind, without its `@`.
const REDACTED_USERINFO: &str = "[redacted]";

/// The userinfo placeholder of [`pasteable_provider_label`]; it names no
/// credential, so it is printed as is.
const USERINFO_PLACEHOLDER: &str = "<user>:<password>";

/// Schemes the URL parser treats as special: after the colon it accepts any
/// number of `/` or `\`, none included, before the authority.
const SPECIAL_URL_SCHEMES: [&str; 5] = ["http", "https", "ws", "wss", "ftp"];

/// Whether `c` is one of the characters the URL parser removes from its whole
/// input before parsing (ASCII tab, LF, CR).
const fn is_url_ignored(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r')
}

/// The scanner's view of one URL.
struct UrlAuthority {
    scheme_start: usize,
    colon: usize,
    authority_start: usize,
    separators: usize,
    special: bool,
}

/// How far back from a colon the scheme search looks. A special scheme has
/// at most five characters; a longer non-special scheme is still found, only
/// reported with a shorter scheme. Without this cap, a long run of scheme
/// characters (`1a1a…:`, `a\ta\t…:`) makes every colon cost the whole run
/// (R-C2BUj J-1).
const MAX_SCHEME_SPAN: usize = 64;

/// The next URL authority at or after `from`.
///
/// The scheme is `[A-Za-z][A-Za-z0-9+.-]*` before a colon; tab, LF and CR
/// anywhere in the scheme or the separator are skipped, as the parser skips
/// them. A special scheme (`http`, `https`, `ws`, `wss`, `ftp`) takes any
/// number of `/` or `\` after the colon, none included, and whatever follows
/// (whitespace too: the parser reads `http: alice:pw@h` as userinfo, so prose
/// such as `https: see a@b` is redacted, the safe direction). Any other
/// scheme needs `//`: the parser gives it an authority only after two `/`,
/// and keeps a `\` as part of that authority.
///
/// Candidate scheme starts, tried in order: the widest run of scheme
/// characters (plus tab, LF, CR) before the colon, at most
/// [`MAX_SCHEME_SPAN`] bytes back, as the parser reads `ht\ntp://`; then each
/// start just after a tab, LF or CR inside it (a URL at the start of a line
/// or a cell); then each letter that follows a digit, `+`, `-` or `.` (a
/// scheme glued to `502`, `1.`, `--`). Nothing before `from` is ever part of
/// a candidate.
fn next_url_authority(text: &str, from: usize) -> Option<UrlAuthority> {
    let mut index = from;
    while index < text.len() {
        let colon = index + text.get(index..)?.find(':')?;
        #[cfg(test)]
        record_userinfo_scan_work(colon + 1 - index);
        index = colon + 1;
        let rest = text.get(colon + 1..)?;
        // Special schemes read any mix of `/` and `\`; others only `/`.
        let special_run = rest
            .find(|c: char| !matches!(c, '/' | '\\') && !is_url_ignored(c))
            .unwrap_or(rest.len());
        let special_separators = rest
            .get(..special_run)?
            .chars()
            .filter(|c| matches!(c, '/' | '\\'))
            .count();
        let plain_run = rest
            .find(|c: char| c != '/' && !is_url_ignored(c))
            .unwrap_or(rest.len());
        let plain_separators = rest.get(..plain_run)?.chars().filter(|c| *c == '/').count();
        #[cfg(test)]
        record_userinfo_scan_work(special_run + plain_run + 2);
        let before = text.get(from..colon)?;
        // Advance past the whole character the search stopped at: it may be
        // multibyte (`é:`, `€:`, NBSP, East-Asian text), and one byte past its
        // start is not a char boundary (R-C2BUi I-1).
        let widest = from
            + before
                .char_indices()
                .rev()
                .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.') || is_url_ignored(*c)))
                .map_or(0, |(position, c)| position + c.len_utf8());
        #[cfg(test)]
        record_userinfo_scan_work(colon - widest);
        // Every character between `widest` and the colon is ASCII, so any
        // byte offset in that span is a char boundary.
        let widest = widest.max(colon.saturating_sub(MAX_SCHEME_SPAN));
        let span = text.get(widest..colon)?;
        #[cfg(test)]
        record_userinfo_scan_work(span.len());
        let mut previous: Option<char> = None;
        let mut inner = Vec::new();
        for (offset, c) in span.char_indices() {
            if is_url_ignored(c) {
                inner.push(widest + offset + c.len_utf8());
            }
            if c.is_ascii_alphabetic() && previous.is_some_and(|p| p.is_ascii_digit() || matches!(p, '+' | '-' | '.')) {
                inner.push(widest + offset);
            }
            if !is_url_ignored(c) {
                previous = Some(c);
            }
        }
        for scheme_start in std::iter::once(widest).chain(inner) {
            #[cfg(test)]
            record_userinfo_scan_work(colon - scheme_start);
            let scheme: String = text
                .get(scheme_start..colon)?
                .chars()
                .filter(|c| !is_url_ignored(*c))
                .collect();
            if !scheme.chars().next().is_some_and(|first| first.is_ascii_alphabetic()) {
                continue;
            }
            let special = SPECIAL_URL_SCHEMES.iter().any(|name| scheme.eq_ignore_ascii_case(name));
            let (run, separators) = if special {
                (special_run, special_separators)
            } else {
                (plain_run, plain_separators)
            };
            if separators >= 2 || special {
                return Some(UrlAuthority {
                    scheme_start,
                    colon,
                    authority_start: colon + 1 + run,
                    separators,
                    special,
                });
            }
        }
        // Every candidate read as a non-special scheme without two `/`. A
        // special scheme glued to a preceding letter (`xhttps:\\u:pw@h`,
        // `completionshTTp\nS:\\u:pw@h`) is such a longer word, for which `\`
        // is no separator, but the URL it ends is what a reader copies (and
        // what the parser reads once the glued prefix is gone). When the
        // separators include two `/` or `\`, that trailing special scheme is
        // taken as the start (R-C2BUk K-1).
        if special_separators >= 2
            && let Some(scheme_start) = glued_special_scheme_start(span, widest)
        {
            return Some(UrlAuthority {
                scheme_start,
                colon,
                authority_start: colon + 1 + special_run,
                separators: special_separators,
                special: true,
            });
        }
    }
    None
}

/// Where a special scheme glued to the end of `span` starts, as a byte offset
/// into the text (`span` starts at `span_start`): `https` in `xhttps`, and in
/// `hTTp\nS`, since tab, LF and CR inside a scheme are skipped, as the parser
/// skips them. The longest special name wins (`wss` over `ss`). `None` when
/// the span does not end with one.
fn glued_special_scheme_start(span: &str, span_start: usize) -> Option<usize> {
    /// The longest special scheme name.
    const MAX_SPECIAL_SCHEME_LEN: usize = 5;
    #[cfg(test)]
    record_userinfo_scan_work(span.len());
    let mut letters = 0usize;
    let mut start = None;
    for (offset, c) in span.char_indices().rev() {
        if is_url_ignored(c) {
            continue;
        }
        letters += 1;
        if letters > MAX_SPECIAL_SCHEME_LEN {
            break;
        }
        let name: String = span.get(offset..)?.chars().filter(|c| !is_url_ignored(*c)).collect();
        if SPECIAL_URL_SCHEMES
            .iter()
            .any(|special| name.eq_ignore_ascii_case(special))
        {
            start = Some(span_start + offset);
        }
    }
    start
}

/// For a bare provider label (optionally prefixed `custom:` or
/// `anthropic-custom:`) that the URL parser reads with a user name or a
/// password, the label with that userinfo redacted, measured on the raw
/// string: after the scheme colon and every separator (`/`, `\`, tab, LF,
/// CR), through the last `@` before the first `/`, `?` or `#` (and `\` for a
/// special scheme). `None` when the input is not such a label.
fn redact_parsed_label(label: &str) -> Option<std::borrow::Cow<'_, str>> {
    let trimmed = label.trim_end();
    let url_start = split_byo_provider(trimmed).map_or_else(
        || trimmed.len() - trimmed.trim_start().len(),
        |(_, raw)| trimmed.len() - raw.len(),
    );
    let raw = trimmed.get(url_start..)?;
    #[cfg(test)]
    record_userinfo_scan_work(raw.len());
    let parsed = reqwest::Url::parse(raw).ok()?;
    if parsed.username().is_empty() && parsed.password().is_none() {
        return None;
    }
    let special = SPECIAL_URL_SCHEMES.contains(&parsed.scheme());
    let colon = raw.find(':')?;
    let after = raw.get(colon + 1..)?;
    let run = after
        .find(|c: char| !(if special { matches!(c, '/' | '\\') } else { c == '/' }) && !is_url_ignored(c))
        .unwrap_or(after.len());
    let authority_start = url_start + colon + 1 + run;
    let index = TextIndex::new(label);
    // Bounded like the free-text scan, so the two passes agree.
    let authority_end = index.authority_end(authority_start, special);
    let bound = authority_start
        + (authority_end - authority_start).min(index.userinfo_bound_after(authority_start, authority_start));
    let at = index.last_at(authority_start, bound)?;
    if matches!(
        label.get(authority_start..at)?,
        REDACTED_USERINFO | USERINFO_PLACEHOLDER
    ) {
        return Some(std::borrow::Cow::Borrowed(label));
    }
    Some(std::borrow::Cow::Owned(format!(
        "{}{REDACTED_USERINFO}{}",
        label.get(..authority_start)?,
        label.get(at..)?
    )))
}

/// An authority longer than this is not handed to the URL parser by the
/// conservative fallback: no real host is that long, and parsing a long
/// authority for every URL of a crafted text would cost quadratic time. It is
/// treated as unparseable, which only ever redacts more.
const MAX_PARSED_AUTHORITY: usize = 1024;

/// For an authority with no `@` of its own: when `scheme://authority` does not
/// parse, the password may hold a literal `/` that ended the authority early,
/// so everything through the last `@` before the next URL that carries its
/// own userinfo (or the end of the text) is treated as userinfo. Tab, LF and
/// CR do not end it, since the parser ignores them. `None` when the authority
/// parses, or there is no such `@`.
fn conservative_userinfo_end(
    text: &str,
    index: &TextIndex<'_>,
    found: &UrlAuthority,
    authority_end: usize,
) -> Option<usize> {
    // No `@` after the authority: nothing to find, and nothing to parse.
    index.last_at(authority_end, text.len())?;
    #[cfg(test)]
    if authority_end - found.authority_start <= MAX_PARSED_AUTHORITY {
        record_userinfo_scan_work(authority_end - found.scheme_start);
    }
    if authority_end - found.authority_start <= MAX_PARSED_AUTHORITY
        && reqwest::Url::parse(text.get(found.scheme_start..authority_end)?).is_ok()
    {
        return None;
    }
    // Stop at the next URL whose own authority holds an `@`: that `@` belongs
    // to it (R-C2BUi I-3). A URL nested in this password (`a:b://c@h`) starts
    // before this authority ends and is not searched for.
    let bound = found.authority_start + index.userinfo_bound_after(found.authority_start, authority_end);
    index.last_at(found.authority_start, bound)
}

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
            "Unknown provider: {}. Check README for supported providers or run `prx onboard --interactive` to reconfigure.\n\
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

/// Information about a supported provider for display purposes.
pub struct ProviderInfo {
    /// Canonical name used in config (e.g. `"openrouter"`)
    pub name: &'static str,
    /// Human-readable display name
    pub display_name: &'static str,
    /// Alternative names accepted in config
    pub aliases: &'static [&'static str],
    /// Whether the provider runs locally (no API key required)
    pub local: bool,
    /// Whether this provider is a model aggregator that accepts any model ID
    /// regardless of the org-prefix in `org/model` format.
    /// Example: OpenRouter routes `mistralai/mistral-7b` even though its own
    /// canonical name is `"openrouter"`, not `"mistralai"`.
    pub accepts_any_model: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderAvailabilitySummary {
    pub configured: Vec<String>,
    pub available: Vec<String>,
    pub unavailable: Vec<(String, String)>,
    pub degraded: bool,
}

fn provider_requires_explicit_credential(name: &str) -> bool {
    if matches!(name, "bedrock" | "aws-bedrock") {
        return false;
    }

    match classify_provider(name) {
        // A `custom:<url>` endpoint is an OpenAI-compatible server the
        // operator runs. Many need no credential at all, exactly like the
        // `local: true` catalog entries, and they cannot carry that flag
        // because they are not catalog rows. The exemption stops at that
        // dialect: `AnthropicProvider` has no keyless mode, so exempting
        // `anthropic-custom:<url>` would only move the refusal from this gate
        // to the first turn, and would advertise a fallback that can never
        // serve one. An unparseable BYO string classifies `Unknown` and keeps
        // the requirement.
        ProviderIdentity::ByoEndpoint {
            dialect: ByoDialect::OpenAiCompatible,
            ..
        } => false,
        // A catalog provider needs a credential unless its row is a local
        // runtime. Aliases fold to the row, so one pass over the catalog
        // answers for every spelling.
        ProviderIdentity::Catalog(canonical) => {
            !list_providers().iter().any(|info| info.local && info.name == canonical)
        }
        _ => true,
    }
}

fn is_openai_codex_alias(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "openai-codex" | "openai_codex" | "codex"
    )
}

fn default_openprx_state_dir() -> PathBuf {
    crate::config::default_openprx_dir().unwrap_or_else(|| PathBuf::from(".openprx"))
}

fn openai_codex_auth_profile_available_for_state_dir(state_dir: &std::path::Path) -> bool {
    let auth = crate::auth::AuthService::new_with_codex_import(
        state_dir,
        false,
        Some(crate::auth::codex_auth::default_codex_auth_json_path()),
        true,
    );

    auth.get_provider_bearer_token("openai-codex", None)
        .ok()
        .flatten()
        .is_some_and(|token| !token.trim().is_empty())
}

fn openai_codex_auth_profile_available() -> bool {
    openai_codex_auth_profile_available_for_state_dir(&default_openprx_state_dir())
}

pub fn provider_matches_model_prefix(provider_name: &str, model: &str) -> bool {
    #[cfg(feature = "wasm-plugins")]
    if provider_name.starts_with("wasm:") {
        return true;
    }
    let model = model.trim();
    if model.is_empty() {
        // An empty model means "use the provider's configured default", which is
        // always compatible with that provider.
        return true;
    }
    if model.starts_with("hint:") {
        // d04 §9.2 hint-injection hardening: a raw "hint:<name>" prefix is an
        // UNRESOLVED routing hint. It MUST be resolved upstream by the
        // RouterProvider into a concrete "<provider>:<model>" / "<provider>/<model>"
        // form before reaching provider matching. The previous wildcard
        // early-return (`return true`) let a crafted, unresolved hint match ANY
        // provider, bypassing the prefix check and allowing a model to be routed
        // to the wrong API. An unresolved hint reaching here is a non-match.
        return false;
    }

    let Some((prefix, _rest)) = model.split_once('/') else {
        return true;
    };

    let prefix = prefix.trim().to_ascii_lowercase();
    if prefix.is_empty() {
        return true;
    }

    let provider_name = provider_name.trim().to_ascii_lowercase();
    if provider_name == prefix {
        return true;
    }

    // A bring-your-own endpoint publishes its own model catalog, so PRX has
    // nothing to check an `org/model` id against and the endpoint is the
    // authority. This arm sits behind the `hint:` guard and the slash guard,
    // so neither of those is widened, and behind the catalog comparison for
    // every other identity, which is unchanged.
    if matches!(classify_provider(&provider_name), ProviderIdentity::ByoEndpoint { .. }) {
        return true;
    }
    // A decision service publishes its own model ids, like a BYO endpoint.
    if ProviderId::of(&provider_name).namespace() == ProviderNamespace::DecisionService {
        return true;
    }

    for info in list_providers() {
        if info.name.eq_ignore_ascii_case(&provider_name)
            || info
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(&provider_name))
        {
            // Aggregator providers accept any model regardless of the org-prefix.
            // OpenRouter routes to hundreds of models whose IDs use `org/model`
            // format (e.g. "mistralai/mistral-7b", "anthropic/claude-3-opus").
            // The prefix is the upstream org, not "openrouter" itself.
            if info.accepts_any_model {
                return true;
            }

            if info.name.eq_ignore_ascii_case(&prefix)
                || info.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(&prefix))
            {
                return true;
            }
        }
    }

    false
}

/// Providers that serve whatever model the operator loaded into them, named by
/// its Hugging Face `org/model` repository (`llama-server -hf …`). The
/// organisation says nothing about which provider serves it
/// (`google/gemma-…-GGUF` is a llama.cpp repository).
const SELF_HOSTED_MODEL_SERVERS: &[&str] = &["llamacpp"];

/// Whether `provider_name` is a self-hosted model server (llama.cpp),
/// under its name or an alias.
pub fn is_self_hosted_model_server(provider_name: &str) -> bool {
    canonical_provider_info(provider_name).is_some_and(|info| SELF_HOSTED_MODEL_SERVERS.contains(&info.name))
}

/// Whether `provider_name` can serve `model` when the model is configured for
/// it (the default model, an onboarding choice, `/model`). A self-hosted model
/// server serves any repository it is configured for; every other provider
/// keeps [`provider_matches_model_prefix`]. Which provider answers a route is
/// decided by the route's provider identity, not by the model's organisation
/// (see `ReliableProvider`, D-075).
pub fn provider_serves_configured_model(provider_name: &str, model: &str) -> bool {
    if is_self_hosted_model_server(provider_name) {
        return !model.trim().starts_with("hint:");
    }
    provider_matches_model_prefix(provider_name, model)
}

/// Whether `model` explicitly names a model repository (`org/repository`)
/// that is not routed to another provider: bare ids go to the route's own
/// provider, and an organisation written as a PRX provider name or alias
/// (`anthropic/…`, `openai/…`) is that provider's model by PRX's
/// `<provider>/<model>` convention. A self-hosted model server serves such a
/// `model_fallbacks` entry as a fallback member (D-076 N-1).
pub fn names_model_repository(model: &str) -> bool {
    let model = model.trim();
    if model.starts_with("hint:") {
        return false;
    }
    let Some((organisation, repository)) = model.split_once('/') else {
        return false;
    };
    // Normalised as provider matching does elsewhere: trimmed, any case
    // (`OpenAI/gpt-4o` and `openai /gpt-4o` are OpenAI's; D-076 N-10).
    let organisation = organisation.trim();
    if organisation.is_empty() || repository.trim().is_empty() {
        return false;
    }
    !list_providers().iter().any(|provider| {
        provider.name.eq_ignore_ascii_case(organisation)
            || provider
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(organisation))
    })
}

fn canonical_provider_info(provider_name: &str) -> Option<ProviderInfo> {
    let provider_name = provider_name.trim();
    list_providers().into_iter().find(|info| {
        info.name.eq_ignore_ascii_case(provider_name)
            || info
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(provider_name))
    })
}

const KIMI_CODE_SUPPORTED_MODELS: &[&str] = &["k3", "kimi-for-coding", "kimi-for-coding-highspeed"];

/// Validate model IDs that PRX can safely validate locally.
///
/// Most OpenAI-compatible providers own model catalog changes, so PRX does not
/// generally reject bare model IDs. Kimi Code is an exception in practice: the
/// coding endpoint can return success even for an invalid model string, which
/// makes CLI validation evidence misleading.
pub fn validate_provider_model(provider_name: &str, model: &str) -> anyhow::Result<()> {
    let model = model.trim();
    if model.is_empty() || model.starts_with("hint:") {
        return Ok(());
    }

    if !provider_serves_configured_model(provider_name, model) {
        anyhow::bail!("model '{model}' is not compatible with provider '{provider_name}'");
    }

    if canonical_provider_info(provider_name).is_some_and(|info| info.name == "kimi-code")
        && !KIMI_CODE_SUPPORTED_MODELS.iter().any(|known| *known == model)
    {
        anyhow::bail!(
            "invalid model '{model}' for provider 'kimi-code'. Supported models: {}",
            KIMI_CODE_SUPPORTED_MODELS.join(", ")
        );
    }

    Ok(())
}

pub fn summarize_provider_availability(
    primary_name: &str,
    api_key: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    options: &ProviderRuntimeOptions,
) -> ProviderAvailabilitySummary {
    summarize_provider_availability_inner(primary_name, api_key, reliability, options, None)
}

fn summarize_provider_availability_inner(
    primary_name: &str,
    api_key: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    options: &ProviderRuntimeOptions,
    validated_primary: Option<ResolvedPrimary<'_>>,
) -> ProviderAvailabilitySummary {
    let mut configured = vec![primary_name.to_string()];
    for fallback in &reliability.fallback_providers {
        if !configured.iter().any(|name| name == fallback) {
            configured.push(fallback.clone());
        }
    }

    let mut available = Vec::new();
    let mut unavailable = Vec::new();

    for provider in &configured {
        if provider == primary_name && validated_primary.is_some() {
            // The same resolver-owned primary has already passed its real constructor.
            available.push(provider.clone());
            continue;
        }
        if let Err(error) = create_provider_with_options(provider, None, options) {
            unavailable.push((provider.clone(), format!("invalid provider: {error}")));
            continue;
        }

        if is_openai_codex_alias(provider) {
            if openai_codex_auth_profile_available() {
                available.push(provider.clone());
            } else {
                unavailable.push((provider.clone(), "missing OpenAI Codex auth profile/token".to_string()));
            }
            continue;
        }

        let explicit_for_provider = (provider == primary_name).then_some(api_key).flatten();
        let resolved = resolve_provider_credentials(provider, explicit_for_provider, options);
        if provider_requires_explicit_credential(provider) && !resolved.is_usable_for(provider) {
            unavailable.push((provider.clone(), "missing credential/api key".to_string()));
            continue;
        }

        available.push(provider.clone());
    }

    ProviderAvailabilitySummary {
        configured,
        degraded: available.len() < 2,
        available,
        unavailable,
    }
}

/// Return the list of all known providers for display in `prx providers list`.
///
/// This is intentionally separate from the factory match in `create_provider`
/// (display concern vs. construction concern).
pub fn list_providers() -> Vec<ProviderInfo> {
    vec![
        // ── Primary providers ────────────────────────────────
        ProviderInfo {
            name: "openrouter",
            display_name: "OpenRouter",
            aliases: &[],
            local: false,
            // OpenRouter is a model aggregator: it routes requests to hundreds of
            // upstream models whose IDs use `org/model` format (e.g.
            // "mistralai/mistral-7b", "anthropic/claude-3-opus").  The prefix is
            // the upstream vendor, not "openrouter", so any model name is valid.
            accepts_any_model: true,
        },
        ProviderInfo {
            name: "anthropic",
            display_name: "Anthropic",
            aliases: &["claude-code", "claude-cli"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "openai",
            display_name: "OpenAI",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "openai-codex",
            display_name: "OpenAI Codex (OAuth)",
            aliases: &["openai_codex", "codex"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "ollama",
            display_name: "Ollama",
            aliases: &[],
            local: true,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "gemini",
            display_name: "Google Gemini",
            aliases: &["google", "google-gemini"],
            local: false,
            accepts_any_model: false,
        },
        // ── OpenAI-compatible providers ──────────────────────
        ProviderInfo {
            name: "venice",
            display_name: "Venice",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "vercel",
            display_name: "Vercel AI Gateway",
            aliases: &["vercel-ai"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "cloudflare",
            display_name: "Cloudflare AI",
            aliases: &["cloudflare-ai"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "moonshot",
            display_name: "Moonshot",
            aliases: &["kimi"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "kimi-code",
            display_name: "Kimi Code",
            aliases: &["kimi_coding", "kimi_for_coding"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "synthetic",
            display_name: "Synthetic",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "opencode",
            display_name: "OpenCode Zen",
            aliases: &["opencode-zen"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "zai",
            display_name: "Z.AI",
            aliases: &["z.ai"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "glm",
            display_name: "GLM (Zhipu)",
            aliases: &["zhipu"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "minimax",
            display_name: "MiniMax",
            aliases: &[
                "minimax-intl",
                "minimax-io",
                "minimax-global",
                "minimax-cn",
                "minimaxi",
                "minimax-oauth",
                "minimax-oauth-cn",
                "minimax-portal",
                "minimax-portal-cn",
            ],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "bedrock",
            display_name: "Amazon Bedrock",
            aliases: &["aws-bedrock"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "qianfan",
            display_name: "Qianfan (Baidu)",
            aliases: &["baidu"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "qwen",
            display_name: "Qwen (DashScope / Qwen Code OAuth)",
            aliases: &[
                "dashscope",
                "qwen-intl",
                "dashscope-intl",
                "qwen-us",
                "dashscope-us",
                "qwen-code",
                "qwen-oauth",
                "qwen_oauth",
            ],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "groq",
            display_name: "Groq",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "mistral",
            display_name: "Mistral",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "xai",
            display_name: "xAI (Grok)",
            aliases: &["grok"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "deepseek",
            display_name: "DeepSeek",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "together",
            display_name: "Together AI",
            aliases: &["together-ai"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "fireworks",
            display_name: "Fireworks AI",
            aliases: &["fireworks-ai"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "perplexity",
            display_name: "Perplexity",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "cohere",
            display_name: "Cohere",
            aliases: &[],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "copilot",
            display_name: "GitHub Copilot",
            aliases: &["github-copilot"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "lmstudio",
            display_name: "LM Studio",
            aliases: &["lm-studio"],
            local: true,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "llamacpp",
            display_name: "llama.cpp server",
            aliases: &["llama.cpp"],
            local: true,
            // Not an aggregator: a fallback llama.cpp member never answers for
            // another provider family's model. The model it is configured for
            // is accepted by `provider_serves_configured_model` (D-072 L-9).
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "litellm",
            display_name: "LiteLLM",
            aliases: &["lite-llm"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "vllm",
            display_name: "vLLM",
            aliases: &["v-llm"],
            local: true,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "huggingface",
            display_name: "Hugging Face Inference",
            aliases: &["hf", "hf-inference"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "nvidia",
            display_name: "NVIDIA NIM",
            aliases: &["nvidia-nim", "build.nvidia.com"],
            local: false,
            accepts_any_model: false,
        },
        ProviderInfo {
            name: "ovhcloud",
            display_name: "OVHcloud AI Endpoints",
            aliases: &["ovh"],
            local: false,
            accepts_any_model: false,
        },
        // Test-only mock provider. `local: true` ensures
        // `provider_requires_explicit_credential` returns false so the
        // availability check passes without any API key.
        // Compiled-in only when `test-mock` feature is enabled (or under
        // `#[cfg(test)]` for the in-crate provider tests).
        #[cfg(any(test, feature = "test-mock"))]
        ProviderInfo {
            name: "mock",
            display_name: "Mock (test)",
            aliases: &[],
            local: true,
            accepts_any_model: true,
        },
    ]
}

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
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    fn factory_config(
        provider: &str,
        model: &str,
        api_key: Option<&str>,
        api_url: Option<&str>,
        reliability: crate::config::ReliabilityConfig,
    ) -> crate::config::Config {
        crate::config::Config {
            default_provider: Some(provider.to_string()),
            default_model: Some(model.to_string()),
            api_key: api_key.map(str::to_string),
            api_url: api_url.map(str::to_string),
            reliability,
            ..crate::config::Config::default()
        }
    }

    // ── Retry-After capture on the non-streaming error path ──────

    /// Answer exactly one request with the given raw HTTP response and return
    /// the address to point a client at.
    async fn serve_one_raw_response(raw: &'static str) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind loopback");
        let addr = listener.local_addr().expect("test: local addr");
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut discard = [0_u8; 2048];
                let _ = socket.read(&mut discard).await;
                let _ = socket.write_all(raw.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        addr
    }

    /// The non-streaming error path must carry the real `Retry-After` response
    /// header. It never did: `api_error` only read the status and the body, so
    /// the reliability layer's Retry-After branch was unreachable for every
    /// non-streaming provider and each 429 fell back to a guessed backoff.
    #[tokio::test]
    async fn api_error_captures_the_retry_after_response_header() {
        const RAW: &str = "HTTP/1.1 429 Too Many Requests\r\n\
Content-Type: application/json\r\n\
Retry-After: 60\r\n\
Content-Length: 24\r\n\
Connection: close\r\n\
\r\n\
{\"type\":\"rate_limit\"}\r\n\r\n";
        let addr = serve_one_raw_response(RAW).await;
        let response = reqwest::Client::new()
            .get(format!("http://{addr}/v1/messages"))
            .send()
            .await
            .expect("test: request the fake upstream");
        assert_eq!(response.status().as_u16(), 429);

        let error = api_error("anthropic", response).await;

        assert_eq!(
            rate_limit::retry_after_hint_ms(&error),
            Some(60_000),
            "the upstream Retry-After must survive into the error"
        );
        assert!(
            error
                .to_string()
                .starts_with("anthropic API error (429 Too Many Requests)"),
            "the user-visible message must be unchanged: {error}"
        );
    }

    #[tokio::test]
    async fn api_error_without_retry_after_header_carries_no_hint() {
        const RAW: &str = "HTTP/1.1 429 Too Many Requests\r\n\
Content-Type: application/json\r\n\
Content-Length: 24\r\n\
Connection: close\r\n\
\r\n\
{\"type\":\"rate_limit\"}\r\n\r\n";
        let addr = serve_one_raw_response(RAW).await;
        let response = reqwest::Client::new()
            .get(format!("http://{addr}/v1/messages"))
            .send()
            .await
            .expect("test: request the fake upstream");

        let error = api_error("anthropic", response).await;

        assert_eq!(rate_limit::retry_after_hint_ms(&error), None);
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        #[allow(unsafe_code)]
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let original = std::env::var(key).ok();
            if key == "HOME" {
                crate::config::schema::follow_test_home_env(value.map(std::ffi::OsStr::new));
            }
            // SAFETY: test-only, single-threaded test runner
            unsafe {
                match value {
                    Some(next) => std::env::set_var(key, next),
                    None => std::env::remove_var(key),
                }
            }

            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            if self.key == "HOME" {
                crate::config::schema::follow_test_home_env(self.original.as_deref().map(std::ffi::OsStr::new));
            }
            // SAFETY: test-only, single-threaded test runner
            unsafe {
                if let Some(original) = self.original.as_deref() {
                    std::env::set_var(self.key, original);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().expect("env lock poisoned")
    }

    #[test]
    fn resolve_provider_credential_prefers_explicit_argument() {
        let resolved = resolve_provider_credential("openrouter", Some("  explicit-key  "));
        assert_eq!(resolved, Some("explicit-key".to_string()));
    }

    #[test]
    fn resolved_routes_never_inherit_the_primary_credential() {
        let mut config = factory_config(
            "openai",
            "gpt-4o",
            Some("openai-secret"),
            None,
            crate::config::ReliabilityConfig::default(),
        );
        config.model_routes = vec![
            crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "foreign".to_string(),
                provider: "anthropic".to_string(),
                model: "claude-3-5-sonnet-20241022".to_string(),
                api_key: None,
                credential_ref: None,
            },
            crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "owned".to_string(),
                provider: "anthropic".to_string(),
                model: "claude-3-5-sonnet-20241022".to_string(),
                api_key: Some("anthropic-secret".to_string()),
                credential_ref: None,
            },
        ];
        let catalog = selection::catalog_for(&config);
        let default = selection::check_default(&config, catalog::UnknownPolicy::Invocation)
            .expect("test: the configured default resolves");
        let foreign = selection::resolve_route(
            &config,
            &catalog,
            &config.model_routes[0],
            catalog::UnknownPolicy::Invocation,
        )
        .expect("test: the keyless route resolves");
        let owned = selection::resolve_route(
            &config,
            &catalog,
            &config.model_routes[1],
            catalog::UnknownPolicy::Invocation,
        )
        .expect("test: the keyed route resolves");

        assert_eq!(foreign.credential().api_key(), None);
        assert_eq!(default.credential().api_key(), Some("openai-secret"));
        assert_eq!(owned.credential().api_key(), Some("anthropic-secret"));
    }

    #[test]
    fn resolve_provider_credential_minimax_placeholder_without_refresh_returns_none() {
        let _env_lock = env_lock();
        let _refresh_guard = EnvGuard::set(MINIMAX_OAUTH_REFRESH_TOKEN_ENV, None);

        let resolved = resolve_provider_credential("minimax", Some(MINIMAX_OAUTH_PLACEHOLDER));

        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_provider_credential_bedrock_returns_none_without_explicit() {
        assert!(resolve_provider_credential("bedrock", None).is_none());
        assert!(resolve_provider_credential("aws-bedrock", None).is_none());
    }

    #[test]
    fn resolve_provider_credential_returns_none_without_explicit_key() {
        // Without explicit api_key, resolve_provider_credential returns None
        // (env vars are no longer read).
        assert!(resolve_provider_credential("litellm", None).is_none());
        assert!(resolve_provider_credential("vllm", None).is_none());
        assert!(resolve_provider_credential("hf", None).is_none());
    }

    #[test]
    fn resolve_provider_credential_reads_active_auth_profiles_for_kimi_providers() {
        let tmp = TempDir::new().unwrap();
        let store = AuthProfilesStore::new(tmp.path(), false);
        store
            .upsert_profile(
                crate::auth::profiles::AuthProfile::new_token(
                    "moonshot",
                    "default",
                    "moonshot-profile-key".to_string(),
                ),
                true,
            )
            .unwrap();
        store
            .upsert_profile(
                crate::auth::profiles::AuthProfile::new_token(
                    "kimi-code",
                    "default",
                    "kimi-code-profile-key".to_string(),
                ),
                true,
            )
            .unwrap();

        let options = ProviderRuntimeOptions {
            openprx_dir: Some(tmp.path().to_path_buf()),
            secrets_encrypt: false,
            ..ProviderRuntimeOptions::default()
        };

        assert_eq!(
            resolve_provider_credential_with_options("moonshot", None, &options).as_deref(),
            Some("moonshot-profile-key")
        );
        assert_eq!(
            resolve_provider_credential_with_options("kimi", None, &options).as_deref(),
            Some("moonshot-profile-key")
        );
        assert_eq!(
            resolve_provider_credential_with_options("moonshot-intl", None, &options).as_deref(),
            Some("moonshot-profile-key")
        );
        assert_eq!(
            resolve_provider_credential_with_options("kimi-code", None, &options).as_deref(),
            Some("kimi-code-profile-key")
        );
    }

    #[test]
    fn resolve_qwen_oauth_context_prefers_explicit_override() {
        let _env_lock = env_lock();
        let fake_home = format!("/tmp/openprx-qwen-oauth-home-{}", std::process::id());
        let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");
        let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, Some("oauth-token"));
        let _resource_guard = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, Some("coding-intl.dashscope.aliyuncs.com"));

        let context = resolve_qwen_oauth_context(Some("  explicit-qwen-token  "), OauthRefresh::Allowed);

        assert_eq!(context.credential.as_deref(), Some("explicit-qwen-token"));
        assert!(context.base_url.is_none());
    }

    #[test]
    fn resolve_qwen_oauth_context_uses_env_token_and_resource_url() {
        let _env_lock = env_lock();
        let fake_home = format!("/tmp/openprx-qwen-oauth-home-{}-env", std::process::id());
        let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");
        let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, Some("oauth-token"));
        let _refresh_guard = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
        let _resource_guard = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, Some("coding-intl.dashscope.aliyuncs.com"));
        let _dashscope_guard = EnvGuard::set("DASHSCOPE_API_KEY", Some("dashscope-fallback"));

        let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER), OauthRefresh::Allowed);

        assert_eq!(context.credential.as_deref(), Some("oauth-token"));
        assert_eq!(
            context.base_url.as_deref(),
            Some("https://coding-intl.dashscope.aliyuncs.com/v1")
        );
    }

    #[test]
    fn resolve_qwen_oauth_context_reads_cached_credentials_file() {
        let _env_lock = env_lock();
        let fake_home = format!("/tmp/openprx-qwen-oauth-home-{}-file", std::process::id());
        let creds_dir = PathBuf::from(&fake_home).join(".qwen");
        std::fs::create_dir_all(&creds_dir).unwrap();
        let creds_path = creds_dir.join("oauth_creds.json");
        std::fs::write(
            &creds_path,
            r#"{"access_token":"cached-token","refresh_token":"cached-refresh","resource_url":"https://resource.example.com","expiry_date":4102444800000}"#,
        )
        .unwrap();

        let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");
        let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, None);
        let _refresh_guard = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
        let _resource_guard = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, None);
        let _dashscope_guard = EnvGuard::set("DASHSCOPE_API_KEY", None);

        let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER), OauthRefresh::Allowed);

        assert_eq!(context.credential.as_deref(), Some("cached-token"));
        assert_eq!(context.base_url.as_deref(), Some("https://resource.example.com/v1"));
    }

    #[test]
    fn resolve_qwen_oauth_context_placeholder_does_not_use_dashscope_fallback() {
        let _env_lock = env_lock();
        let fake_home = format!("/tmp/openprx-qwen-oauth-home-{}-placeholder", std::process::id());
        let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");
        let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, None);
        let _refresh_guard = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
        let _resource_guard = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, None);
        let _dashscope_guard = EnvGuard::set("DASHSCOPE_API_KEY", Some("dashscope-fallback"));

        let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER), OauthRefresh::Allowed);

        assert!(context.credential.is_none());
    }

    #[test]
    fn resolve_claude_code_context_prefers_explicit_override() {
        let context = resolve_claude_code_context(Some("  explicit-claude-token  "), OauthRefresh::Allowed);
        assert_eq!(context.credential.as_deref(), Some("explicit-claude-token"));
    }

    #[test]
    fn resolve_claude_code_context_reads_cached_credentials_file() {
        let _env_lock = env_lock();
        let fake_home = format!("/tmp/openprx-claude-oauth-home-{}-file", std::process::id());
        let creds_dir = PathBuf::from(&fake_home).join(".claude");
        std::fs::create_dir_all(&creds_dir).unwrap();
        let creds_path = creds_dir.join(".credentials.json");
        std::fs::write(
            &creds_path,
            r#"{"claudeAiOauth":{"accessToken":"cached-token","refreshToken":"cached-refresh","expiresAt":4102444800000,"subscriptionType":"max","scopes":["org:create_api_key"]}}"#,
        )
        .unwrap();

        let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");

        let context = resolve_claude_code_context(Some("claude-code"), OauthRefresh::Allowed);

        assert_eq!(context.credential.as_deref(), Some("cached-token"));
    }

    #[test]
    fn resolve_claude_code_context_placeholder_without_file_returns_none() {
        let _env_lock = env_lock();
        let fake_home = format!("/tmp/openprx-claude-oauth-home-{}-placeholder", std::process::id());
        let _home_guard = OauthTestHome::set(std::path::Path::new(&fake_home), "http://127.0.0.1:9/unused");

        let context = resolve_claude_code_context(Some("claude-code"), OauthRefresh::Allowed);

        assert!(context.credential.is_none());
    }

    /// Installs a home and a refresh endpoint for the OAuth credential
    /// lookups for one test; hold [`env_lock`] while it lives.
    struct OauthTestHome;

    impl OauthTestHome {
        fn set(home: &std::path::Path, token_endpoint: &str) -> Self {
            *OAUTH_TEST_OVERRIDES.write() = OauthTestOverrides {
                home: Some(home.to_path_buf()),
                token_endpoint: Some(token_endpoint.to_string()),
            };
            Self
        }
    }

    impl Drop for OauthTestHome {
        fn drop(&mut self) {
            *OAUTH_TEST_OVERRIDES.write() = OauthTestOverrides::default();
        }
    }

    /// A loopback listener that counts the connections it accepts and
    /// answers each with a 400.
    struct LoopbackCounter {
        addr: std::net::SocketAddr,
        connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl LoopbackCounter {
        fn start() -> Self {
            use std::io::{Read as _, Write as _};
            use std::sync::atomic::Ordering;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("test: bind");
            listener.set_nonblocking(true).expect("test: non-blocking");
            let addr = listener.local_addr().expect("test: addr");
            let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (counted, stopped) = (std::sync::Arc::clone(&connections), std::sync::Arc::clone(&stop));
            let thread = std::thread::spawn(move || {
                while !stopped.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut socket, _)) => {
                            counted.fetch_add(1, Ordering::SeqCst);
                            let _ = socket.set_nonblocking(false);
                            let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(1)));
                            let mut buffer = [0_u8; 4096];
                            let _ = socket.read(&mut buffer);
                            let _ = socket.write_all(
                                b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            );
                        }
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                    }
                }
            });
            Self {
                addr,
                connections,
                stop,
                thread: Some(thread),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/v1/oauth/token", self.addr)
        }

        fn connections(&self) -> usize {
            self.connections.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Drop for LoopbackCounter {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// RED before PMMDtail-g (D-080 T-7b): the live probe looked the
    /// credential up on the async task with construction's lookup, which
    /// refreshes an expired Claude Code token: a blocking HTTP client inside
    /// the runtime (a panic), a second request to another host and a rewrite
    /// of the operator's credential file. The probe now looks up off the
    /// runtime with a lookup that never refreshes and never writes; it
    /// reports that the token needs a refresh and sends nothing.
    ///
    /// Loopback only: the refresh endpoint is a local listener, the lookup
    /// is first checked to see the expired token (so the probe stops at the
    /// credential and the anthropic model-list request is never sent), and
    /// the test home is the OAuth lookups' own override, not the process-wide
    /// test home other modules change concurrently.
    #[test]
    fn the_live_probe_never_refreshes_an_oauth_token() {
        let _env_lock = env_lock();
        let home = tempfile::TempDir::new().expect("test: temp home");
        let creds_dir = home.path().join(".claude");
        std::fs::create_dir_all(&creds_dir).expect("test: .claude");
        let creds_path = creds_dir.join(".credentials.json");
        let creds: &[u8] = br#"{"claudeAiOauth":{"accessToken":"sentinel-expired-access","refreshToken":"sentinel-refresh","expiresAt":1000,"subscriptionType":"max","scopes":["user:inference"]}}"#;
        std::fs::write(&creds_path, creds).expect("test: credentials");
        let token_endpoint = LoopbackCounter::start();
        let _home = OauthTestHome::set(home.path(), &token_endpoint.url());
        let config = crate::config::Config {
            config_path: home.path().join("prx").join("config.toml"),
            workspace_dir: home.path().join("prx").join("workspace"),
            default_provider: Some("anthropic".into()),
            default_model: Some("claude-sonnet-4".into()),
            api_key: None,
            ..crate::config::Config::default()
        };
        // Loopback only: nothing is sent unless the lookup finds the expired
        // token, so check that first, without the runtime.
        assert!(
            matches!(
                probe_credential("anthropic", None, &provider_runtime_options_from_config(&config)),
                ProbeCredential::RefreshDue
            ),
            "precondition: the lookup must see the expired token in the test home"
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test: runtime");

        let outcome = runtime.block_on(async move {
            tokio::spawn(async move {
                super::connection_test::test_connection(&config, None, None, super::connection_test::ProbeMode::Live)
                    .await
            })
            .await
        });

        let report = outcome.expect("the live probe must not panic");
        assert!(report.local.resolved, "{report}");
        let live = report.live.expect("test: a live result");
        assert!(!live.reachable, "nothing may be sent: {live:?}");
        assert!(
            live.error.as_deref().is_some_and(|error| error.contains("refresh")),
            "{live:?}"
        );
        assert_eq!(token_endpoint.connections(), 0, "the probe must not refresh the token");
        assert_eq!(
            std::fs::read(&creds_path).expect("test: read back"),
            creds,
            "the credential file must be untouched"
        );
        // The listener does stand in for the refresh host: construction's
        // own lookup refreshes through it (and, refused, writes nothing).
        let _ = resolve_claude_code_context(Some("claude-code"), OauthRefresh::Allowed);
        assert_eq!(token_endpoint.connections(), 1, "the loopback endpoint was not used");
        assert_eq!(std::fs::read(&creds_path).expect("test: read back"), creds);
    }

    /// D-081: the connection test's lookup never refreshes a Qwen OAuth
    /// token. An expired cached token with a refresh token is reported as
    /// due, with no request, and the cache file is untouched; construction's
    /// own lookup does refresh, through the same loopback endpoint.
    #[test]
    fn the_probe_lookup_never_refreshes_a_qwen_token() {
        let _env_lock = env_lock();
        let home = tempfile::TempDir::new().expect("test: temp home");
        let creds_dir = home.path().join(".qwen");
        std::fs::create_dir_all(&creds_dir).expect("test: .qwen");
        let creds_path = creds_dir.join("oauth_creds.json");
        let creds: &[u8] =
            br#"{"access_token":"sentinel-expired-qwen","refresh_token":"sentinel-qwen-refresh","expiry_date":1000}"#;
        std::fs::write(&creds_path, creds).expect("test: credentials");
        let _token = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, None);
        let _refresh = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
        let _resource = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, None);
        let _dashscope = EnvGuard::set("DASHSCOPE_API_KEY", None);
        let token_endpoint = LoopbackCounter::start();
        let _home = OauthTestHome::set(home.path(), &token_endpoint.url());

        let probed = probe_credential(
            "qwen-code",
            Some(QWEN_OAUTH_PLACEHOLDER),
            &ProviderRuntimeOptions::default(),
        );

        assert!(
            matches!(probed, ProbeCredential::RefreshDue),
            "a due refresh must be reported"
        );
        assert_eq!(token_endpoint.connections(), 0, "the probe must not refresh the token");
        assert_eq!(std::fs::read(&creds_path).expect("test: read back"), creds);
        let _ = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER), OauthRefresh::Allowed);
        assert_eq!(token_endpoint.connections(), 1, "the loopback endpoint was not used");
    }

    /// D-081: the connection test's lookup never exchanges a MiniMax OAuth
    /// refresh token (every construction does); it reports the exchange as
    /// due and sends nothing.
    #[test]
    fn the_probe_lookup_never_refreshes_a_minimax_token() {
        let _env_lock = env_lock();
        let _refresh = EnvGuard::set(MINIMAX_OAUTH_REFRESH_TOKEN_ENV, Some("sentinel-minimax-refresh"));
        let _region = EnvGuard::set(MINIMAX_OAUTH_REGION_ENV, None);
        let home = tempfile::TempDir::new().expect("test: temp home");
        let token_endpoint = LoopbackCounter::start();
        let _home = OauthTestHome::set(home.path(), &token_endpoint.url());
        let options = ProviderRuntimeOptions::default();

        let probed = probe_credential("minimax", Some(MINIMAX_OAUTH_PLACEHOLDER), &options);

        assert!(
            matches!(probed, ProbeCredential::RefreshDue),
            "a due exchange must be reported"
        );
        assert_eq!(token_endpoint.connections(), 0, "the probe must not exchange the token");
        assert!(
            resolve_provider_credential_with_options("minimax", Some(MINIMAX_OAUTH_PLACEHOLDER), &options).is_none()
        );
        assert_eq!(token_endpoint.connections(), 1, "the loopback endpoint was not used");
    }

    /// D-081: in a test build every OAuth token endpoint is local unless a
    /// test installs one, so no test can reach a real token host.
    #[test]
    fn a_test_build_never_targets_a_real_token_endpoint() {
        let _env_lock = env_lock();
        for endpoint in [
            OauthTokenEndpoint::ClaudeCode,
            OauthTokenEndpoint::Qwen,
            OauthTokenEndpoint::Minimax(MinimaxOauthRegion::Global),
            OauthTokenEndpoint::Minimax(MinimaxOauthRegion::Cn),
        ] {
            let url = oauth_token_endpoint(endpoint);
            assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        }
    }

    /// RED before PMMDtail-i (D-083 C-4): the connection test's credential
    /// lookup went through a profile load that migrates a legacy `enc:`
    /// value and rewrites auth-profiles.json. A probe writes no credential
    /// file.
    #[test]
    fn the_probe_lookup_never_rewrites_the_profiles_file() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let store = crate::auth::profiles::AuthProfilesStore::new(dir.path(), true);
        store
            .upsert_profile(
                crate::auth::profiles::AuthProfile::new_token("moonshot", "default", "sentinel-moonshot".into()),
                true,
            )
            .expect("test: profile");
        let legacy = crate::security::secrets::SecretStore::new(dir.path(), true)
            .legacy_xor_value("sentinel-moonshot")
            .expect("test: legacy value");
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store.path()).expect("test: read")).expect("test: json");
        for profile in raw["profiles"].as_object_mut().expect("test: profiles").values_mut() {
            profile["token"] = serde_json::json!(legacy);
        }
        std::fs::write(store.path(), serde_json::to_vec_pretty(&raw).expect("test: json")).expect("test: write");
        let before = std::fs::read_to_string(store.path()).expect("test: read back");
        let options = ProviderRuntimeOptions {
            openprx_dir: Some(dir.path().to_path_buf()),
            secrets_encrypt: true,
            ..ProviderRuntimeOptions::default()
        };

        let probed = probe_credential("moonshot", None, &options);

        assert!(
            matches!(&probed, ProbeCredential::Ready(Some(key)) if key == "sentinel-moonshot"),
            "the legacy value is still read"
        );
        assert_eq!(
            std::fs::read_to_string(store.path()).expect("test: read back"),
            before,
            "the probe rewrote the file"
        );
    }

    #[test]
    fn regional_alias_predicates_cover_expected_variants() {
        assert!(is_moonshot_alias("moonshot"));
        assert!(is_moonshot_alias("kimi-global"));
        assert!(is_glm_alias("glm"));
        assert!(is_glm_alias("bigmodel"));
        assert!(is_minimax_alias("minimax-io"));
        assert!(is_minimax_alias("minimaxi"));
        assert!(is_minimax_alias("minimax-oauth"));
        assert!(is_minimax_alias("minimax-portal-cn"));
        assert!(is_qwen_alias("dashscope"));
        assert!(is_qwen_alias("qwen-us"));
        assert!(is_qwen_alias("qwen-code"));
        assert!(is_qwen_alias("qwen-coding"));
        assert!(is_qwen_alias("dashscope-coding-intl"));
        assert!(is_qwen_coding_alias("dashscope-coding"));
        assert!(is_qwen_coding_alias("qwen-coding-intl"));
        assert!(is_qwen_oauth_alias("qwen-code"));
        assert!(is_qwen_oauth_alias("qwen_oauth"));
        assert!(is_claude_code_alias("claude-code"));
        assert!(is_claude_code_alias("claude-cli"));
        assert!(is_zai_alias("z.ai"));
        assert!(is_zai_alias("zai-cn"));
        assert!(is_qianfan_alias("qianfan"));
        assert!(is_qianfan_alias("baidu"));
        assert!(is_litellm_alias("litellm"));
        assert!(is_litellm_alias("lite-llm"));
        assert!(is_vllm_alias("vllm"));
        assert!(is_vllm_alias("v-llm"));
        assert!(is_huggingface_alias("huggingface"));
        assert!(is_huggingface_alias("hf"));
        assert!(is_huggingface_alias("hf-inference"));

        assert!(!is_moonshot_alias("openrouter"));
        assert!(!is_glm_alias("openai"));
        assert!(!is_qwen_alias("gemini"));
        assert!(!is_claude_code_alias("anthropic"));
        assert!(!is_zai_alias("anthropic"));
        assert!(!is_qianfan_alias("cohere"));
        assert!(!is_litellm_alias("lite"));
        assert!(!is_vllm_alias("vllm-local"));
        assert!(!is_huggingface_alias("hugging-face"));
    }

    #[test]
    fn canonical_china_provider_name_maps_regional_aliases() {
        assert_eq!(canonical_china_provider_name("moonshot"), Some("moonshot"));
        assert_eq!(canonical_china_provider_name("kimi-intl"), Some("moonshot"));
        assert_eq!(canonical_china_provider_name("glm"), Some("glm"));
        assert_eq!(canonical_china_provider_name("zhipu-cn"), Some("glm"));
        assert_eq!(canonical_china_provider_name("minimax"), Some("minimax"));
        assert_eq!(canonical_china_provider_name("minimax-cn"), Some("minimax"));
        assert_eq!(canonical_china_provider_name("qwen"), Some("qwen"));
        assert_eq!(canonical_china_provider_name("dashscope-us"), Some("qwen"));
        assert_eq!(canonical_china_provider_name("qwen-code"), Some("qwen"));
        assert_eq!(canonical_china_provider_name("zai"), Some("zai"));
        assert_eq!(canonical_china_provider_name("z.ai-cn"), Some("zai"));
        assert_eq!(canonical_china_provider_name("qianfan"), Some("qianfan"));
        assert_eq!(canonical_china_provider_name("baidu"), Some("qianfan"));
        assert_eq!(canonical_china_provider_name("openai"), None);
    }

    #[test]
    fn regional_endpoint_aliases_map_to_expected_urls() {
        assert_eq!(minimax_base_url("minimax"), Some(MINIMAX_INTL_BASE_URL));
        assert_eq!(minimax_base_url("minimax-intl"), Some(MINIMAX_INTL_BASE_URL));
        assert_eq!(minimax_base_url("minimax-cn"), Some(MINIMAX_CN_BASE_URL));

        assert_eq!(glm_base_url("glm"), Some(GLM_GLOBAL_BASE_URL));
        assert_eq!(glm_base_url("glm-cn"), Some(GLM_CN_BASE_URL));
        assert_eq!(glm_base_url("bigmodel"), Some(GLM_CN_BASE_URL));

        assert_eq!(moonshot_base_url("moonshot"), Some(MOONSHOT_CN_BASE_URL));
        assert_eq!(moonshot_base_url("moonshot-intl"), Some(MOONSHOT_INTL_BASE_URL));

        assert_eq!(qwen_base_url("qwen"), Some(QWEN_CN_BASE_URL));
        assert_eq!(qwen_base_url("qwen-cn"), Some(QWEN_CN_BASE_URL));
        assert_eq!(qwen_base_url("qwen-intl"), Some(QWEN_INTL_BASE_URL));
        assert_eq!(qwen_base_url("qwen-us"), Some(QWEN_US_BASE_URL));
        assert_eq!(qwen_base_url("qwen-code"), Some(QWEN_CN_BASE_URL));
        assert_eq!(qwen_coding_base_url("dashscope-coding"), Some(QWEN_CODING_CN_BASE_URL));
        assert_eq!(qwen_coding_base_url("qwen-coding"), Some(QWEN_CODING_CN_BASE_URL));
        assert_eq!(
            qwen_coding_base_url("dashscope-coding-intl"),
            Some(QWEN_CODING_INTL_BASE_URL)
        );
        assert_eq!(
            qwen_coding_base_url("qwen-coding-intl"),
            Some(QWEN_CODING_INTL_BASE_URL)
        );

        assert_eq!(zai_base_url("zai"), Some(ZAI_GLOBAL_BASE_URL));
        assert_eq!(zai_base_url("z.ai"), Some(ZAI_GLOBAL_BASE_URL));
        assert_eq!(zai_base_url("zai-global"), Some(ZAI_GLOBAL_BASE_URL));
        assert_eq!(zai_base_url("z.ai-global"), Some(ZAI_GLOBAL_BASE_URL));
        assert_eq!(zai_base_url("zai-cn"), Some(ZAI_CN_BASE_URL));
        assert_eq!(zai_base_url("z.ai-cn"), Some(ZAI_CN_BASE_URL));
    }

    // ── Primary providers ────────────────────────────────────

    #[test]
    fn factory_openrouter() {
        assert!(create_provider("openrouter", Some("provider-test-credential")).is_ok());
        assert!(create_provider("openrouter", None).is_ok());
    }

    #[test]
    fn factory_anthropic() {
        assert!(create_provider("anthropic", Some("provider-test-credential")).is_ok());
    }

    #[test]
    fn factory_openai() {
        assert!(create_provider("openai", Some("provider-test-credential")).is_ok());
    }

    #[test]
    fn factory_openai_codex() {
        let options = ProviderRuntimeOptions::default();
        assert!(create_provider_with_options("openai-codex", None, &options).is_ok());
    }

    #[test]
    fn factory_ollama() {
        assert!(create_provider("ollama", None).is_ok());
        // Ollama may use API key when a remote endpoint is configured.
        assert!(create_provider("ollama", Some("dummy")).is_ok());
        assert!(create_provider("ollama", Some("any-value-here")).is_ok());
    }

    #[test]
    fn factory_gemini() {
        assert!(create_provider("gemini", Some("test-key")).is_ok());
        assert!(create_provider("google", Some("test-key")).is_ok());
        assert!(create_provider("google-gemini", Some("test-key")).is_ok());
        // Should also work without key (will try CLI auth)
        assert!(create_provider("gemini", None).is_ok());
    }

    /// The validator asks the constructed provider which schema document it
    /// put on the wire (`tool_schema_dialect`/`tool_schema_dialect_for`); the
    /// rewriting happens inside each provider implementation. This pins what
    /// each label builds, so a provider that starts (or stops) cleaning its
    /// schemas is a visible change. (PMM-C-L deleted the label-keyed map this
    /// test used to compare against.)
    #[test]
    fn provider_aliases_agree_with_the_declared_schema_dialect() {
        for alias in [
            "anthropic",
            "claude-code",
            "claude-cli",
            "gemini",
            "google",
            "google-gemini",
            "openai",
            "ollama",
            "openrouter",
            "venice",
            "groq",
            // Bring-your-own endpoints. The label carries a URL, so a map keyed
            // on the whole string cannot match it; `anthropic-custom:` builds an
            // `AnthropicProvider` that strips `$ref`/`$defs` on the wire, and a
            // label resolving to `Raw` would put the validator back on the
            // unstripped document.
            "custom:https://endpoint.invalid/v1",
            "anthropic-custom:https://endpoint.invalid/v1",
        ] {
            let provider = create_provider(alias, Some("test-key"))
                .unwrap_or_else(|error| panic!("test: provider {alias} constructs: {error}"));
            let expected = match alias {
                "gemini" | "google" | "google-gemini" => crate::tools::ToolSchemaDialect::Gemini,
                "anthropic" | "claude-code" | "claude-cli" | "anthropic-custom:https://endpoint.invalid/v1" => {
                    crate::tools::ToolSchemaDialect::Anthropic
                }
                _ => crate::tools::ToolSchemaDialect::Raw,
            };
            assert_eq!(
                provider.tool_schema_dialect(),
                expected,
                "provider label '{alias}' builds a provider with a different schema dialect"
            );
        }
    }

    /// The provider factory recognizes a bring-your-own label through one
    /// splitter, so every spelling it accepts — any ASCII case, surrounding
    /// whitespace — builds the provider that splitter's dialect names, and a
    /// label the splitter rejects is not bring-your-own.
    #[test]
    fn byo_label_spellings_resolve_the_dialect_the_constructed_provider_declares() {
        use ByoDialect::{AnthropicCompatible, OpenAiCompatible};
        for (label, dialect) in [
            ("custom:https://endpoint.invalid/v1", OpenAiCompatible),
            ("CUSTOM:https://endpoint.invalid/v1", OpenAiCompatible),
            ("  Custom:https://endpoint.invalid/v1  ", OpenAiCompatible),
            ("anthropic-custom:https://endpoint.invalid/v1", AnthropicCompatible),
            ("Anthropic-Custom:https://endpoint.invalid/v1", AnthropicCompatible),
            ("  ANTHROPIC-CUSTOM:https://endpoint.invalid/v1 ", AnthropicCompatible),
            ("\tAnthropic-Custom:https://endpoint.invalid/v1", AnthropicCompatible),
        ] {
            // The splitter is asserted on its own, not only through the
            // provider it leads to (SP-INT2 a31).
            assert_eq!(
                byo_dialect_of(label),
                Some(dialect),
                "test: '{label}' is a bring-your-own label of that dialect"
            );
            assert!(
                matches!(classify_provider(label), ProviderIdentity::ByoEndpoint { .. }),
                "test: '{label}' is a bring-your-own label"
            );
            let provider = create_provider(label, Some("test-key"))
                .unwrap_or_else(|error| panic!("test: provider {label} constructs: {error}"));
            let expected = match byo_dialect_of(label) {
                Some(ByoDialect::AnthropicCompatible) => crate::tools::ToolSchemaDialect::Anthropic,
                Some(ByoDialect::OpenAiCompatible) => crate::tools::ToolSchemaDialect::Raw,
                None => panic!("test: '{label}' is a bring-your-own label"),
            };
            assert_eq!(
                provider.tool_schema_dialect(),
                expected,
                "the splitter and the constructed provider disagree on '{label}'"
            );
        }
        // A prefix with no URL, or a URL the factory refuses, is still that
        // dialect's label: the splitter decides the prefix, the URL check
        // comes after it.
        for (label, dialect) in [
            ("custom:", OpenAiCompatible),
            ("anthropic-custom:", AnthropicCompatible),
            ("\tAnthropic-Custom:x", AnthropicCompatible),
        ] {
            assert_eq!(
                byo_dialect_of(label),
                Some(dialect),
                "test: '{label:?}' is a bring-your-own label of that dialect"
            );
        }
        for label in ["custom", "xcustom:https://endpoint.invalid/v1", "anthropic-customs"] {
            assert_eq!(
                byo_dialect_of(label),
                None,
                "test: '{label}' is not a bring-your-own label"
            );
        }
    }

    /// A wrapper is what the dispatcher holds, so the dialect it declares is the
    /// one the validator uses. `ReliableProvider` and `RouterProvider` wrap a
    /// concrete provider; without a forwarding override they inherit the trait
    /// default `Raw`, and every Gemini turn behind a failover chain is then
    /// validated against keywords Gemini never saw.
    #[test]
    fn wrapper_providers_forward_the_wrapped_schema_dialect() {
        let wrapped = create_provider("gemini", Some("test-key")).expect("test: gemini constructs");
        let reliable = reliable::ReliableProvider::new(vec![("gemini".to_string(), wrapped)], 1, 50);
        assert_eq!(
            reliable.tool_schema_dialect(),
            crate::tools::ToolSchemaDialect::Gemini,
            "ReliableProvider must declare the dialect of the provider it wraps"
        );

        let wrapped = create_provider("gemini", Some("test-key")).expect("test: gemini constructs");
        let router = router::RouterProvider::from_test_parts(
            vec![("gemini".to_string(), wrapped)],
            Vec::new(),
            "gemini-2.0-flash".to_string(),
        );
        assert_eq!(
            router.tool_schema_dialect(),
            crate::tools::ToolSchemaDialect::Gemini,
            "RouterProvider must declare the dialect of the provider it routes to"
        );
    }

    /// PMM-C-L: a mixed chain answers `Raw` model-less, but the member that
    /// served a turn (`ChatTrace::final_provider`) answers its own dialect,
    /// through a router too, so the agent loop validates a fallback against
    /// the document that fallback put on the wire.
    #[test]
    fn a_chain_answers_the_dialect_of_the_member_that_served() {
        let chain = || {
            reliable::ReliableProvider::new(
                vec![
                    (
                        "gemini".to_string(),
                        create_provider("gemini", Some("test-key")).expect("test: gemini constructs"),
                    ),
                    (
                        "openai".to_string(),
                        create_provider("openai", Some("test-key")).expect("test: openai constructs"),
                    ),
                ],
                1,
                50,
            )
        };
        let reliable = chain();
        assert_eq!(reliable.tool_schema_dialect(), crate::tools::ToolSchemaDialect::Raw);
        assert_eq!(
            reliable.tool_schema_dialect_of_member("gemini", "gemini-2.0-flash"),
            Some(crate::tools::ToolSchemaDialect::Gemini)
        );
        assert_eq!(
            reliable.tool_schema_dialect_of_member("openai", "gpt-4o"),
            Some(crate::tools::ToolSchemaDialect::Raw)
        );
        assert_eq!(reliable.tool_schema_dialect_of_member("anthropic", "x"), None);

        let router = router::RouterProvider::from_test_parts(
            vec![("gemini".to_string(), Box::new(chain()) as Box<dyn Provider>)],
            Vec::new(),
            "gemini-2.0-flash".to_string(),
        );
        assert_eq!(
            router.tool_schema_dialect_of_member("gemini", "gemini-2.0-flash"),
            Some(crate::tools::ToolSchemaDialect::Gemini)
        );
        assert_eq!(router.tool_schema_dialect_of_member("mistral", "x"), None);
    }

    /// A chain whose members all belong to one family resolves to that family:
    /// whichever member serves the turn produced the same wire document, so
    /// enforcing that family's document is exact.
    #[test]
    fn a_single_family_chain_declares_that_family() {
        let chain = reliable::ReliableProvider::new(
            vec![
                (
                    "gemini".to_string(),
                    create_provider("gemini", Some("test-key")).expect("test: gemini constructs"),
                ),
                (
                    "google".to_string(),
                    create_provider("google", Some("test-key")).expect("test: google constructs"),
                ),
            ],
            1,
            50,
        );
        assert_eq!(
            chain.tool_schema_dialect(),
            crate::tools::ToolSchemaDialect::Gemini,
            "a chain of one family must declare that family"
        );
    }

    /// A chain that can land on either of two *different* families has no one
    /// answer, and the rewrites are not comparable: `Gemini` strips keywords,
    /// `NoRootCombinators` restructures the root, and neither subsumes the
    /// other. Ranking them and picking the "widest" makes enforcement looser
    /// than the wire on every turn the other member serves — the model is shown
    /// `additionalProperties: false` and then not held to it. A mixed chain
    /// therefore resolves to `Raw`: the published document, which is the
    /// strongest contract and the one no member can be shown less than.
    #[test]
    fn a_mixed_family_chain_declares_raw_rather_than_the_widest_rewrite() {
        let chain = reliable::ReliableProvider::new(
            vec![
                (
                    "gemini".to_string(),
                    create_provider("gemini", Some("test-key")).expect("test: gemini constructs"),
                ),
                (
                    "openai".to_string(),
                    create_provider("openai", Some("test-key")).expect("test: openai constructs"),
                ),
            ],
            1,
            50,
        );
        assert_eq!(
            chain.tool_schema_dialect(),
            crate::tools::ToolSchemaDialect::Raw,
            "a mixed-family chain must not enforce one member's rewrite on the other member's turns"
        );

        let routed = router::RouterProvider::from_test_parts(
            vec![
                (
                    "gemini".to_string(),
                    create_provider("gemini", Some("test-key")).expect("test: gemini constructs"),
                ),
                (
                    "openai".to_string(),
                    create_provider("openai", Some("test-key")).expect("test: openai constructs"),
                ),
            ],
            Vec::new(),
            "gemini-2.0-flash".to_string(),
        );
        assert_eq!(
            routed.tool_schema_dialect(),
            crate::tools::ToolSchemaDialect::Raw,
            "a mixed-family router must not enforce one destination's rewrite on another's turns"
        );
    }

    /// `Anthropic` and `NoRootCombinators` are the pair the rank ordering got
    /// most wrong: it declared the structural rewrite for a chain that may be
    /// served by a provider which never restructures anything. Resolving to
    /// `Raw` must not manufacture a refusal either — `Raw` puts the published
    /// document on the wire and enforces the same document.
    #[test]
    fn a_mixed_chain_of_anthropic_and_no_root_combinators_resolves_raw_without_refusing() {
        let declared = "custom:https://endpoint.invalid/v1";
        let options = ProviderRuntimeOptions {
            tool_schema_dialects: vec![ToolSchemaDialectDeclaration {
                provider: declared.to_string(),
                model: None,
                dialect: crate::tools::ToolSchemaDialect::NoRootCombinators,
            }],
            ..ProviderRuntimeOptions::default()
        };
        let lowering = create_provider_with_options(declared, Some("test-key"), &options)
            .expect("test: a declared custom endpoint constructs");
        assert_eq!(
            lowering.tool_schema_dialect(),
            crate::tools::ToolSchemaDialect::NoRootCombinators,
            "the declaration did not reach the constructed provider"
        );

        let chain = reliable::ReliableProvider::new(
            vec![
                (
                    "anthropic".to_string(),
                    create_provider("anthropic", Some("test-key")).expect("test: anthropic constructs"),
                ),
                (declared.to_string(), lowering),
            ],
            1,
            50,
        );
        let resolved = chain.tool_schema_dialect();
        assert_eq!(
            resolved,
            crate::tools::ToolSchemaDialect::Raw,
            "a chain mixing a keyword-stripping and a structural rewrite must fall back to the published document"
        );

        // A contract the structural rewrite refuses must still be offered under
        // the resolved dialect: resolution may not turn an available tool into
        // a refused one.
        let conditional = serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "key": {"type": "string"}},
            "anyOf": [{"required": ["path"]}, {"required": ["key"]}]
        });
        let wire = resolved
            .wire_schema("fixture_alternatives", &conditional)
            .expect("test: resolving a mixed chain must not manufacture a refusal");
        assert_eq!(
            &*wire, &conditional,
            "the resolved dialect changed the published document"
        );
        assert_eq!(&*resolved.enforceable_schema(&conditional), &conditional);
    }

    /// The two labels that do rewrite their schemas must not decay to `Raw`,
    /// which would put the validator back on the published document.
    #[test]
    fn cleaning_providers_declare_a_non_raw_dialect() {
        let dialect = |label: &str| {
            create_provider(label, Some("test-key"))
                .unwrap_or_else(|error| panic!("test: provider {label} constructs: {error}"))
                .tool_schema_dialect()
        };
        assert_eq!(dialect("gemini"), crate::tools::ToolSchemaDialect::Gemini);
        assert_eq!(dialect("anthropic"), crate::tools::ToolSchemaDialect::Anthropic);
        assert_eq!(dialect("openai"), crate::tools::ToolSchemaDialect::Raw);
    }

    // ── OpenAI-compatible providers ──────────────────────────

    #[test]
    fn factory_venice() {
        assert!(create_provider("venice", Some("vn-key")).is_ok());
    }

    #[test]
    fn factory_vercel() {
        assert!(create_provider("vercel", Some("key")).is_ok());
        assert!(create_provider("vercel-ai", Some("key")).is_ok());
    }

    #[test]
    fn factory_cloudflare() {
        assert!(create_provider("cloudflare", Some("key")).is_ok());
        assert!(create_provider("cloudflare-ai", Some("key")).is_ok());
    }

    #[test]
    fn factory_moonshot() {
        assert!(create_provider("moonshot", Some("key")).is_ok());
        assert!(create_provider("kimi", Some("key")).is_ok());
        assert!(create_provider("moonshot-intl", Some("key")).is_ok());
        assert!(create_provider("moonshot-cn", Some("key")).is_ok());
        assert!(create_provider("kimi-intl", Some("key")).is_ok());
        assert!(create_provider("kimi-cn", Some("key")).is_ok());
    }

    #[test]
    fn factory_kimi_code() {
        assert!(create_provider("kimi-code", Some("key")).is_ok());
        assert!(create_provider("kimi_coding", Some("key")).is_ok());
        assert!(create_provider("kimi_for_coding", Some("key")).is_ok());
    }

    #[test]
    fn factory_synthetic() {
        assert!(create_provider("synthetic", Some("key")).is_ok());
    }

    #[test]
    fn factory_opencode() {
        assert!(create_provider("opencode", Some("key")).is_ok());
        assert!(create_provider("opencode-zen", Some("key")).is_ok());
    }

    #[test]
    fn factory_zai() {
        assert!(create_provider("zai", Some("key")).is_ok());
        assert!(create_provider("z.ai", Some("key")).is_ok());
        assert!(create_provider("zai-global", Some("key")).is_ok());
        assert!(create_provider("z.ai-global", Some("key")).is_ok());
        assert!(create_provider("zai-cn", Some("key")).is_ok());
        assert!(create_provider("z.ai-cn", Some("key")).is_ok());
    }

    #[test]
    fn factory_glm() {
        assert!(create_provider("glm", Some("key")).is_ok());
        assert!(create_provider("zhipu", Some("key")).is_ok());
        assert!(create_provider("glm-cn", Some("key")).is_ok());
        assert!(create_provider("zhipu-cn", Some("key")).is_ok());
        assert!(create_provider("glm-global", Some("key")).is_ok());
        assert!(create_provider("bigmodel", Some("key")).is_ok());
    }

    #[test]
    fn factory_glm_routes_to_streaming_compatible_provider() {
        let provider = create_provider("glm", Some("key")).unwrap();
        assert!(provider.supports_streaming());
    }

    #[test]
    fn factory_minimax() {
        assert!(create_provider("minimax", Some("key")).is_ok());
        assert!(create_provider("minimax-intl", Some("key")).is_ok());
        assert!(create_provider("minimax-io", Some("key")).is_ok());
        assert!(create_provider("minimax-global", Some("key")).is_ok());
        assert!(create_provider("minimax-cn", Some("key")).is_ok());
        assert!(create_provider("minimaxi", Some("key")).is_ok());
        assert!(create_provider("minimax-oauth", Some("key")).is_ok());
        assert!(create_provider("minimax-oauth-cn", Some("key")).is_ok());
        assert!(create_provider("minimax-portal", Some("key")).is_ok());
        assert!(create_provider("minimax-portal-cn", Some("key")).is_ok());
    }

    #[test]
    fn factory_bedrock() {
        // Bedrock uses AWS config files for credentials, not API key.
        assert!(create_provider("bedrock", None).is_ok());
        assert!(create_provider("aws-bedrock", None).is_ok());
        // Passing an api_key is harmless (ignored).
        assert!(create_provider("bedrock", Some("ignored")).is_ok());
    }

    #[test]
    fn factory_qianfan() {
        assert!(create_provider("qianfan", Some("key")).is_ok());
        assert!(create_provider("baidu", Some("key")).is_ok());
    }

    #[test]
    fn factory_qwen() {
        assert!(create_provider("qwen", Some("key")).is_ok());
        assert!(create_provider("dashscope", Some("key")).is_ok());
        assert!(create_provider("qwen-cn", Some("key")).is_ok());
        assert!(create_provider("dashscope-cn", Some("key")).is_ok());
        assert!(create_provider("qwen-intl", Some("key")).is_ok());
        assert!(create_provider("dashscope-intl", Some("key")).is_ok());
        assert!(create_provider("qwen-international", Some("key")).is_ok());
        assert!(create_provider("dashscope-international", Some("key")).is_ok());
        assert!(create_provider("qwen-us", Some("key")).is_ok());
        assert!(create_provider("dashscope-us", Some("key")).is_ok());
        assert!(create_provider("dashscope-coding", Some("key")).is_ok());
        assert!(create_provider("qwen-coding", Some("key")).is_ok());
        assert!(create_provider("dashscope-coding-intl", Some("key")).is_ok());
        assert!(create_provider("qwen-coding-intl", Some("key")).is_ok());
        assert!(create_provider("qwen-code", Some("key")).is_ok());
        assert!(create_provider("qwen-oauth", Some("key")).is_ok());
    }

    #[test]
    fn factory_lmstudio() {
        assert!(create_provider("lmstudio", Some("key")).is_ok());
        assert!(create_provider("lm-studio", Some("key")).is_ok());
        assert!(create_provider("lmstudio", None).is_ok());
    }

    #[test]
    fn factory_llamacpp() {
        assert!(create_provider("llamacpp", Some("key")).is_ok());
        assert!(create_provider("llama.cpp", Some("key")).is_ok());
        assert!(create_provider("llamacpp", None).is_ok());
    }

    #[test]
    fn factory_litellm() {
        assert!(create_provider("litellm", Some("key")).is_ok());
        assert!(create_provider("lite-llm", Some("key")).is_ok());
        assert!(create_provider("litellm", None).is_ok());
        assert!(create_provider_with_url("litellm", Some("key"), Some("http://127.0.0.1:4001")).is_ok());
    }

    #[test]
    fn factory_vllm() {
        assert!(create_provider("vllm", Some("key")).is_ok());
        assert!(create_provider("v-llm", Some("key")).is_ok());
        assert!(create_provider("vllm", None).is_ok());
        assert!(create_provider_with_url("vllm", Some("key"), Some("http://127.0.0.1:9000/v1")).is_ok());
    }

    #[test]
    fn factory_huggingface() {
        assert!(create_provider("huggingface", Some("key")).is_ok());
        assert!(create_provider("hf", Some("key")).is_ok());
        assert!(create_provider("hf-inference", Some("key")).is_ok());
    }

    // ── Extended ecosystem ───────────────────────────────────

    #[test]
    fn factory_groq() {
        assert!(create_provider("groq", Some("key")).is_ok());
    }

    #[test]
    fn factory_mistral() {
        assert!(create_provider("mistral", Some("key")).is_ok());
    }

    #[test]
    fn factory_xai() {
        assert!(create_provider("xai", Some("key")).is_ok());
        assert!(create_provider("grok", Some("key")).is_ok());
    }

    #[test]
    fn factory_deepseek() {
        assert!(create_provider("deepseek", Some("key")).is_ok());
    }

    #[test]
    fn factory_together() {
        assert!(create_provider("together", Some("key")).is_ok());
        assert!(create_provider("together-ai", Some("key")).is_ok());
    }

    #[test]
    fn factory_fireworks() {
        assert!(create_provider("fireworks", Some("key")).is_ok());
        assert!(create_provider("fireworks-ai", Some("key")).is_ok());
    }

    #[test]
    fn factory_perplexity() {
        assert!(create_provider("perplexity", Some("key")).is_ok());
    }

    #[test]
    fn factory_cohere() {
        assert!(create_provider("cohere", Some("key")).is_ok());
    }

    #[test]
    fn factory_copilot() {
        assert!(create_provider("copilot", Some("key")).is_ok());
        assert!(create_provider("github-copilot", Some("key")).is_ok());
    }

    #[test]
    fn factory_copilot_supports_streaming() {
        let provider = create_provider("copilot", Some("key")).unwrap();
        assert!(provider.supports_streaming());
    }

    #[test]
    fn factory_nvidia() {
        assert!(create_provider("nvidia", Some("nvapi-test")).is_ok());
        assert!(create_provider("nvidia-nim", Some("nvapi-test")).is_ok());
        assert!(create_provider("build.nvidia.com", Some("nvapi-test")).is_ok());
    }

    // ── AI inference routers ─────────────────────────────────

    #[test]
    fn factory_astrai() {
        assert!(create_provider("astrai", Some("sk-astrai-test")).is_ok());
    }

    // ── Custom / BYOP provider ─────────────────────────────

    #[test]
    fn factory_custom_url() {
        let p = create_provider("custom:https://my-llm.example.com", Some("key"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_custom_localhost() {
        let p = create_provider("custom:http://localhost:1234", Some("key"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_custom_no_key() {
        let p = create_provider("custom:https://my-llm.example.com", None);
        assert!(p.is_ok());
    }

    #[test]
    fn factory_custom_empty_url_errors() {
        match create_provider("custom:", None) {
            Err(e) => assert!(
                e.to_string().contains("requires a URL"),
                "Expected 'requires a URL', got: {e}"
            ),
            Ok(_) => panic!("Expected error for empty custom URL"),
        }
    }

    #[test]
    fn factory_custom_invalid_url_errors() {
        match create_provider("custom:not-a-url", None) {
            Err(e) => assert!(
                e.to_string().contains("requires a valid URL"),
                "Expected 'requires a valid URL', got: {e}"
            ),
            Ok(_) => panic!("Expected error for invalid custom URL"),
        }
    }

    #[test]
    fn factory_custom_unsupported_scheme_errors() {
        match create_provider("custom:ftp://example.com", None) {
            Err(e) => assert!(
                e.to_string().contains("http:// or https://"),
                "Expected scheme validation error, got: {e}"
            ),
            Ok(_) => panic!("Expected error for unsupported custom URL scheme"),
        }
    }

    #[test]
    fn factory_custom_trims_whitespace() {
        let p = create_provider("custom:  https://my-llm.example.com  ", Some("key"));
        assert!(p.is_ok());
    }

    // ── Anthropic-compatible custom endpoints ─────────────────

    #[test]
    fn factory_anthropic_custom_url() {
        let p = create_provider("anthropic-custom:https://api.example.com", Some("key"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_anthropic_custom_trailing_slash() {
        let p = create_provider("anthropic-custom:https://api.example.com/", Some("key"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_anthropic_custom_no_key() {
        let p = create_provider("anthropic-custom:https://api.example.com", None);
        assert!(p.is_ok());
    }

    #[test]
    fn factory_anthropic_custom_empty_url_errors() {
        match create_provider("anthropic-custom:", None) {
            Err(e) => assert!(
                e.to_string().contains("requires a URL"),
                "Expected 'requires a URL', got: {e}"
            ),
            Ok(_) => panic!("Expected error for empty anthropic-custom URL"),
        }
    }

    #[test]
    fn factory_anthropic_custom_invalid_url_errors() {
        match create_provider("anthropic-custom:not-a-url", None) {
            Err(e) => assert!(
                e.to_string().contains("requires a valid URL"),
                "Expected 'requires a valid URL', got: {e}"
            ),
            Ok(_) => panic!("Expected error for invalid anthropic-custom URL"),
        }
    }

    #[test]
    fn factory_anthropic_custom_unsupported_scheme_errors() {
        match create_provider("anthropic-custom:ftp://example.com", None) {
            Err(e) => assert!(
                e.to_string().contains("http:// or https://"),
                "Expected scheme validation error, got: {e}"
            ),
            Ok(_) => panic!("Expected error for unsupported anthropic-custom URL scheme"),
        }
    }

    // ── Error cases ──────────────────────────────────────────

    #[test]
    fn factory_unknown_provider_errors() {
        let p = create_provider("nonexistent", None);
        assert!(p.is_err());
        let msg = p.err().unwrap().to_string();
        assert!(msg.contains("Unknown provider"));
        assert!(msg.contains("nonexistent"));
    }

    #[test]
    fn factory_empty_name_errors() {
        assert!(create_provider("", None).is_err());
    }

    #[test]
    fn resolved_provider_refuses_an_invalid_fallback() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec![
                "openrouter".into(),
                "nonexistent-provider".into(),
                "openai".into(),
                "openai".into(),
            ],
            acknowledge_single_provider_risk: false,
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let config = factory_config(
            "openrouter",
            "anthropic/claude-sonnet-4",
            Some("provider-test-credential"),
            None,
            reliability,
        );
        let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
        assert!(provider.is_err());
    }

    #[test]
    fn resilient_provider_errors_for_invalid_primary() {
        let config = factory_config(
            "totally-invalid",
            "fixture-model",
            Some("provider-test-credential"),
            None,
            crate::config::ReliabilityConfig::default(),
        );
        let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
        assert!(provider.is_err());
    }

    #[test]
    fn summarize_provider_availability_marks_degraded_when_only_primary_has_credentials() {
        let _guard = env_lock();

        // Isolate from host credentials: override HOME to an empty temp dir so
        // resolve_claude_code_context cannot find ~/.claude/.credentials.json,
        // and clear any ambient Anthropic env vars.
        let iso_home = std::env::temp_dir().join(format!("openprx-provider-degrade-test-{}", std::process::id()));
        std::fs::create_dir_all(&iso_home).unwrap();
        let _home_guard = EnvGuard::set("HOME", Some(iso_home.to_str().unwrap()));
        let _anthropic_key_guard = EnvGuard::set("ANTHROPIC_API_KEY", None);
        let _anthropic_oauth_guard = EnvGuard::set("ANTHROPIC_OAUTH_TOKEN", None);

        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec!["anthropic".into()],
            acknowledge_single_provider_risk: false,
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let summary = summarize_provider_availability(
            "openai",
            Some("sk-test"),
            &reliability,
            &ProviderRuntimeOptions::default(),
        );
        let _ = std::fs::remove_dir_all(&iso_home);
        assert!(summary.degraded);
        assert_eq!(summary.available, vec!["openai"]);
        assert!(
            summary
                .unavailable
                .iter()
                .any(|(name, reason)| name == "anthropic" && reason.contains("missing credential"))
        );
    }

    #[test]
    fn summarize_provider_availability_marks_openai_codex_available_with_auth_profile() {
        let _guard = env_lock();

        let fake_home = std::env::temp_dir().join(format!(
            "openprx-provider-avail-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let state_dir = fake_home.join(".openprx");
        std::fs::create_dir_all(&state_dir).unwrap();

        let auth = crate::auth::AuthService::new(&state_dir, false);
        auth.store_openai_tokens(
            "default",
            crate::auth::profiles::TokenSet {
                access_token: "test-access-token".into(),
                refresh_token: None,
                id_token: None,
                expires_at: None,
                token_type: Some("Bearer".into()),
                scope: None,
            },
            Some("acct_test".into()),
            true,
        )
        .unwrap();

        let home_guard = EnvGuard::set("HOME", Some(&fake_home.to_string_lossy()));
        let summary = summarize_provider_availability(
            "openai",
            Some("sk-test"),
            &crate::config::ReliabilityConfig {
                provider_retries: 1,
                provider_backoff_ms: 100,
                fallback_providers: vec!["openai-codex".into()],
                acknowledge_single_provider_risk: false,
                api_keys: Vec::new(),
                model_fallbacks: std::collections::HashMap::new(),
                channel_initial_backoff_secs: 2,
                channel_max_backoff_secs: 60,
                scheduler_poll_secs: 15,
                scheduler_retries: 2,
            },
            &ProviderRuntimeOptions::default(),
        );

        assert!(summary.available.iter().any(|p| p == "openai-codex"));
        drop(home_guard);
        let _ = std::fs::remove_dir_all(&fake_home);
    }

    #[test]
    fn summarize_provider_availability_marks_openai_codex_unavailable_without_auth_profile() {
        let _guard = env_lock();

        let fake_home = std::env::temp_dir().join(format!(
            "openprx-provider-avail-noauth-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&fake_home).unwrap();

        let home_guard = EnvGuard::set("HOME", Some(&fake_home.to_string_lossy()));
        let summary = summarize_provider_availability(
            "openai",
            Some("sk-test"),
            &crate::config::ReliabilityConfig {
                provider_retries: 1,
                provider_backoff_ms: 100,
                fallback_providers: vec!["openai-codex".into()],
                acknowledge_single_provider_risk: false,
                api_keys: Vec::new(),
                model_fallbacks: std::collections::HashMap::new(),
                channel_initial_backoff_secs: 2,
                channel_max_backoff_secs: 60,
                scheduler_poll_secs: 15,
                scheduler_retries: 2,
            },
            &ProviderRuntimeOptions::default(),
        );

        assert!(
            summary
                .unavailable
                .iter()
                .any(|(name, reason)| name == "openai-codex" && reason.contains("auth profile"))
        );
        drop(home_guard);
        let _ = std::fs::remove_dir_all(&fake_home);
    }

    #[test]
    fn provider_model_prefix_matching_rejects_cross_provider_mismatch() {
        assert!(provider_matches_model_prefix("openai", "openai/gpt-4o"));
        assert!(!provider_matches_model_prefix("anthropic", "openai/gpt-4o"));
    }

    /// A decision service publishes its own model ids, like a BYO endpoint,
    /// so no model id is refused as foreign to it (CDP-02).
    #[test]
    fn a_decision_service_accepts_its_own_model_ids() {
        for provider in ["typesafe", "decision:typesafe"] {
            for model in ["jev-1.13.0", "vendor/decider-2026-09"] {
                assert!(provider_matches_model_prefix(provider, model), "{provider} {model}");
                validate_provider_model(provider, model).expect("test: compatible");
            }
        }
    }

    // d04 §9.2 hint-injection hardening regression: a raw "hint:<name>" prefix is
    // an UNRESOLVED routing hint and MUST NOT bypass the provider prefix check.
    // The previous wildcard early-return (`if model.starts_with("hint:") { return
    // true; }`) let a crafted unresolved hint match ANY provider, allowing a model
    // to be routed to the wrong API. After hardening it must be a non-match.
    #[test]
    fn test_unresolved_hint_does_not_bypass_provider_match() {
        assert!(
            !provider_matches_model_prefix("openai", "hint:anything"),
            "unresolved hint must not match an unrelated provider"
        );
        assert!(
            !provider_matches_model_prefix("anthropic", "hint:fast"),
            "unresolved hint must not bypass the provider prefix check"
        );
        // A hint crafted to look like a provider name must still not match: the
        // literal "hint:" prefix means it is unresolved.
        assert!(
            !provider_matches_model_prefix("openai", "hint:openai"),
            "a 'hint:'-prefixed model is unresolved and must not match by coincidence"
        );
        // Even an aggregator (accepts_any_model) must reject an unresolved hint.
        assert!(
            !provider_matches_model_prefix("openrouter", "hint:whatever"),
            "aggregators must not accept an unresolved hint either"
        );
        // Regression: legitimate resolved provider-prefixed models still match.
        assert!(provider_matches_model_prefix("openai", "openai/gpt-4o"));
        assert!(provider_matches_model_prefix("anthropic", "anthropic/claude-3-opus"));
        // Empty model still means "provider default" and matches.
        assert!(provider_matches_model_prefix("openai", ""));
    }

    #[test]
    fn a_self_hosted_server_serves_any_repository_it_is_configured_for() {
        for repository in [
            "ggml-org/gpt-oss-20b-GGUF",
            "bartowski/Llama-3.3-70B-Instruct-GGUF",
            "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF",
            "google/gemma-3-27b-it-GGUF",
            "plain-model-name",
        ] {
            assert!(provider_serves_configured_model("llamacpp", repository), "{repository}");
            assert!(validate_provider_model("llama.cpp", repository).is_ok(), "{repository}");
        }
        assert!(is_self_hosted_model_server("llama.cpp"));
        assert!(!is_self_hosted_model_server("anthropic"));
        // An unresolved routing hint is never served.
        assert!(!provider_serves_configured_model("llamacpp", "hint:fast"));
        // Other providers are unchanged.
        assert!(!provider_serves_configured_model("anthropic", "openai/gpt-5"));
        assert!(provider_serves_configured_model("openrouter", "openai/gpt-5"));
    }

    #[test]
    fn a_model_repository_is_named_explicitly() {
        for repository in [
            "ggml-org/gpt-oss-20b-GGUF",
            "unsloth/gemma-3-27b-it-GGUF",
            "bartowski/Llama-3.3-70B-Instruct-GGUF",
            " ggml-org /gpt-oss-20b-GGUF",
        ] {
            assert!(names_model_repository(repository), "{repository}");
        }
        for routed_elsewhere in [
            "gpt-5",
            "anthropic/claude-sonnet-5",
            "openai/gpt-4o",
            "kimi/k2",
            // D-076 N-10: any case, surrounding space ignored.
            "OpenAI/gpt-4o",
            "Anthropic/claude-sonnet-5",
            "openai /gpt-4o",
            " OPENAI/gpt-4o",
            "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF",
            "google/gemma-3-27b-it-GGUF",
            "hint:fast",
            "org/",
            "/repo",
            " /repo",
        ] {
            assert!(!names_model_repository(routed_elsewhere), "{routed_elsewhere}");
        }
    }

    #[test]
    fn validate_provider_model_rejects_unknown_kimi_code_model() {
        assert!(validate_provider_model("kimi-code", "k3").is_ok());
        assert!(validate_provider_model("kimi_coding", "kimi-for-coding").is_ok());
        assert!(validate_provider_model("kimi-code", "kimi-for-coding-highspeed").is_ok());
        assert!(validate_provider_model("kimi-code", "kimi-k2.7-code").is_err());

        let err = validate_provider_model("kimi-code", "definitely-not-a-real-model")
            .expect_err("unknown Kimi Code model should fail local validation")
            .to_string();

        assert!(err.contains("invalid model 'definitely-not-a-real-model'"));
        assert!(err.contains("k3"));
    }

    /// OpenRouter is a model aggregator: it must accept any `org/model` style
    /// model ID even when the prefix does not match "openrouter".
    /// Regression test for: `prx go -p openrouter -m "mistralai/mistral-7b"`
    /// previously returning "not compatible with provider".
    #[test]
    fn openrouter_accepts_any_org_prefixed_model() {
        // Standard org/model format used by most OpenRouter models
        assert!(provider_matches_model_prefix("openrouter", "mistralai/mistral-7b"));
        assert!(provider_matches_model_prefix("openrouter", "anthropic/claude-3-opus"));
        assert!(provider_matches_model_prefix(
            "openrouter",
            "meta-llama/llama-3-70b-instruct"
        ));
        assert!(provider_matches_model_prefix("openrouter", "google/gemma-2-9b-it"));
        // Mixed-case provider name must also work
        assert!(provider_matches_model_prefix("OpenRouter", "mistralai/mistral-7b"));
        // Models without a slash (plain names) already returned true for everyone
        assert!(provider_matches_model_prefix("openrouter", "gpt-4o"));
        // Other providers must NOT accept cross-vendor prefixed models
        assert!(!provider_matches_model_prefix("anthropic", "mistralai/mistral-7b"));
        assert!(!provider_matches_model_prefix("openai", "mistralai/mistral-7b"));
    }

    /// Fallback providers resolve their own credentials independently
    /// rather than inheriting the primary provider's key.  A provider
    /// that requires no key (e.g. lmstudio, ollama) must initialize
    /// successfully even when the primary uses a completely different key.
    #[test]
    fn resilient_fallback_resolves_own_credential() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec!["lmstudio".into(), "ollama".into()],
            acknowledge_single_provider_risk: false,
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        // Primary uses a ZAI key; fallbacks (lmstudio, ollama) should NOT
        // receive this key; they resolve their own credentials independently.
        let config = factory_config("zai", "glm-4.7", Some("zai-test-key"), None, reliability);
        let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
        assert!(provider.is_ok());
    }

    /// `custom:` URL entries work as fallback providers, enabling arbitrary
    /// OpenAI-compatible endpoints (e.g. local LM Studio on a Docker host).
    #[test]
    fn resilient_fallback_supports_custom_url() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec!["custom:http://host.docker.internal:1234/v1".into()],
            acknowledge_single_provider_risk: false,
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let config = factory_config("openai", "gpt-4o", Some("openai-test-key"), None, reliability);
        let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
        assert!(provider.is_ok());
    }

    /// Mixed fallback chain: named providers, custom URLs, and invalid entries
    /// all coexist.  Invalid entries are silently ignored; valid ones initialize.
    #[test]
    fn resolved_provider_refuses_a_mixed_chain_with_an_invalid_member() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec![
                "deepseek".into(),
                "custom:http://localhost:8080/v1".into(),
                "nonexistent-provider".into(),
                "lmstudio".into(),
            ],
            acknowledge_single_provider_risk: false,
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let config = factory_config("zai", "glm-4.7", Some("zai-test-key"), None, reliability);
        let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
        assert!(provider.is_err());
    }

    #[test]
    fn ollama_with_custom_url() {
        let provider = create_provider_with_url("ollama", None, Some("http://10.100.2.32:11434"));
        assert!(provider.is_ok());
    }

    #[test]
    fn ollama_cloud_with_custom_url() {
        let provider = create_provider_with_url("ollama", Some("ollama-key"), Some("https://ollama.com"));
        assert!(provider.is_ok());
    }

    #[test]
    fn factory_all_providers_create_successfully() {
        let providers = [
            "openrouter",
            "anthropic",
            "openai",
            "ollama",
            "gemini",
            "venice",
            "vercel",
            "cloudflare",
            "moonshot",
            "moonshot-intl",
            "kimi-code",
            "moonshot-cn",
            "kimi-code",
            "synthetic",
            "opencode",
            "zai",
            "zai-cn",
            "glm",
            "glm-cn",
            "minimax",
            "minimax-cn",
            "bedrock",
            "qianfan",
            "qwen",
            "qwen-intl",
            "qwen-cn",
            "qwen-us",
            "qwen-code",
            "lmstudio",
            "llamacpp",
            "litellm",
            "vllm",
            "huggingface",
            "groq",
            "mistral",
            "xai",
            "deepseek",
            "together",
            "fireworks",
            "perplexity",
            "cohere",
            "copilot",
            "nvidia",
            "astrai",
            "ovhcloud",
        ];
        for name in providers {
            assert!(
                create_provider(name, Some("test-key")).is_ok(),
                "Provider '{name}' should create successfully"
            );
        }
    }

    #[test]
    fn listed_providers_have_unique_ids_and_aliases() {
        let providers = list_providers();
        let mut canonical_ids = std::collections::HashSet::new();
        let mut aliases = std::collections::HashSet::new();

        for provider in providers {
            assert!(
                canonical_ids.insert(provider.name),
                "Duplicate canonical provider id: {}",
                provider.name
            );

            for alias in provider.aliases {
                assert_ne!(
                    *alias, provider.name,
                    "Alias must differ from canonical id: {}",
                    provider.name
                );
                assert!(
                    !canonical_ids.contains(alias),
                    "Alias conflicts with canonical provider id: {}",
                    alias
                );
                assert!(aliases.insert(alias), "Duplicate provider alias: {}", alias);
            }
        }
    }

    #[test]
    fn listed_providers_and_aliases_are_constructible() {
        for provider in list_providers() {
            assert!(
                create_provider(provider.name, Some("provider-test-credential")).is_ok(),
                "Canonical provider id should be constructible: {}",
                provider.name
            );

            for alias in provider.aliases {
                assert!(
                    create_provider(alias, Some("provider-test-credential")).is_ok(),
                    "Provider alias should be constructible: {} (for {})",
                    alias,
                    provider.name
                );
            }
        }
    }

    // ── API error sanitization ───────────────────────────────

    #[test]
    fn sanitize_scrubs_sk_prefix() {
        let input = "request failed: sk-1234567890abcdef";
        let out = sanitize_api_error(input);
        assert!(!out.contains("sk-1234567890abcdef"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_scrubs_multiple_prefixes() {
        let input = "keys sk-abcdef xoxb-12345 xoxp-67890";
        let out = sanitize_api_error(input);
        assert!(!out.contains("sk-abcdef"));
        assert!(!out.contains("xoxb-12345"));
        assert!(!out.contains("xoxp-67890"));
    }

    #[test]
    fn sanitize_short_prefix_then_real_key() {
        let input = "error with sk- prefix and key sk-1234567890";
        let result = sanitize_api_error(input);
        assert!(!result.contains("sk-1234567890"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_sk_proj_comment_then_real_key() {
        let input = "note: sk- then sk-proj-abc123def456";
        let result = sanitize_api_error(input);
        assert!(!result.contains("sk-proj-abc123def456"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_keeps_bare_prefix() {
        let input = "only prefix sk- present";
        let result = sanitize_api_error(input);
        assert!(result.contains("sk-"));
    }

    #[test]
    fn sanitize_handles_json_wrapped_key() {
        let input = r#"{"error":"invalid key sk-abc123xyz"}"#;
        let result = sanitize_api_error(input);
        assert!(!result.contains("sk-abc123xyz"));
    }

    #[test]
    fn sanitize_handles_delimiter_boundaries() {
        let input = "bad token xoxb-abc123}; next";
        let result = sanitize_api_error(input);
        assert!(!result.contains("xoxb-abc123"));
        assert!(result.contains("};"));
    }

    #[test]
    fn sanitize_truncates_long_error() {
        let long = "a".repeat(400);
        let result = sanitize_api_error(&long);
        assert!(result.len() <= 203);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn sanitize_truncates_after_scrub() {
        let input = format!("{} sk-abcdef123456 {}", "a".repeat(190), "b".repeat(190));
        let result = sanitize_api_error(&input);
        assert!(!result.contains("sk-abcdef123456"));
        assert!(result.len() <= 203);
    }

    #[test]
    fn sanitize_preserves_unicode_boundaries() {
        let input = format!("{} sk-abcdef123", "hello🙂".repeat(80));
        let result = sanitize_api_error(&input);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
        assert!(!result.contains("sk-abcdef123"));
    }

    #[test]
    fn sanitize_no_secret_no_change() {
        let input = "simple upstream timeout";
        let result = sanitize_api_error(input);
        assert_eq!(result, input);
    }

    #[test]
    fn scrub_github_personal_access_token() {
        let input = "auth failed with token ghp_abc123def456";
        let result = crate::security::redaction::redact_secrets(input);
        assert_eq!(result, "auth failed with token [REDACTED]");
    }

    #[test]
    fn scrub_github_oauth_token() {
        let input = "Bearer gho_1234567890abcdef";
        let result = crate::security::redaction::redact_secrets(input);
        assert_eq!(result, "Bearer [REDACTED]");
    }

    #[test]
    fn scrub_github_user_token() {
        let input = "token ghu_sessiontoken123";
        let result = crate::security::redaction::redact_secrets(input);
        assert_eq!(result, "token [REDACTED]");
    }

    #[test]
    fn scrub_github_fine_grained_pat() {
        let input = "failed: github_pat_11AABBC_xyzzy789";
        let result = crate::security::redaction::redact_secrets(input);
        assert_eq!(result, "failed: [REDACTED]");
    }

    // ── FIX-P2-14: extra api_keys rotation ──

    fn reliability_with_keys(keys: Vec<String>) -> crate::config::ReliabilityConfig {
        crate::config::ReliabilityConfig {
            api_keys: keys,
            ..crate::config::ReliabilityConfig::default()
        }
    }

    #[test]
    fn extra_api_keys_filters_empty_dupes_and_primary() {
        let reliability = reliability_with_keys(vec![
            "  ".into(),         // empty after trim → skipped
            "sk-primary".into(), // equals primary → skipped
            " sk-two ".into(),   // trimmed and kept
            "sk-two".into(),     // duplicate of the above → skipped
            "sk-three".into(),
        ]);
        let extras = extra_api_keys(&reliability, Some("sk-primary"));
        assert_eq!(extras, vec!["sk-two", "sk-three"]);
    }

    #[test]
    fn extra_api_keys_handles_no_primary() {
        let reliability = reliability_with_keys(vec!["sk-a".into(), "sk-a".into(), "sk-b".into()]);
        let extras = extra_api_keys(&reliability, None);
        assert_eq!(extras, vec!["sk-a", "sk-b"]);
    }

    #[test]
    fn extra_api_keys_empty_when_unset() {
        let reliability = crate::config::ReliabilityConfig::default();
        assert!(
            extra_api_keys(&reliability, Some("sk-primary")).is_empty(),
            "expected an empty value"
        );
    }

    #[test]
    fn resilient_provider_builds_rotation_instances_for_extra_keys() {
        // With two distinct extra keys, the primary provider chain should
        // include the primary instance plus one instance per distinct extra key.
        // We assert construction succeeds (the rotation instances are built from
        // the same provider factory as the primary).
        let reliability = reliability_with_keys(vec![
            "openai-extra-1".into(),
            "openai-extra-2".into(),
            "openai-extra-2".into(),
        ]);
        let config = factory_config("openai", "gpt-4o", Some("openai-primary"), None, reliability);
        let provider = create_provider_from_test_config(&config, &ProviderRuntimeOptions::default());
        assert!(
            provider.is_ok(),
            "resilient provider with rotation keys should build: {:?}",
            provider.err()
        );
    }

    // ── Bring-your-own endpoints: identity, model validation, credentials ──
    //
    // A `custom:<url>` / `anthropic-custom:<url>` selection names an endpoint
    // the operator runs, not a catalog vendor. It therefore owns its own model
    // catalog and may legitimately be configured without a credential. Every
    // catalog identity keeps the behaviour it has today.

    /// A credential-shaped sentinel. Not a real secret.
    const BYO_SENTINEL_KEY: &str = "byo-secret";
    /// A BYO endpoint name with a fixed port, used only for string-level
    /// identity assertions: nothing connects to it.
    const BYO_ENDPOINT_NAME: &str = "custom:http://127.0.0.1:18086/v1";
    /// A bare model alias, the shape a BYO endpoint advertises.
    const BYO_BARE_MODEL: &str = "byo-test-model";
    /// A slash-qualified model id whose org prefix is not the provider name.
    const BYO_SLASH_MODEL: &str = "exampleorg/byo-test-model";
    /// The canned assistant reply the loopback endpoint returns.
    const BYO_LOOPBACK_REPLY: &str = "byo-loopback-ok";

    fn byo_reliability_without_fallbacks() -> crate::config::ReliabilityConfig {
        crate::config::ReliabilityConfig {
            provider_retries: 0,
            provider_backoff_ms: 0,
            fallback_providers: Vec::new(),
            acknowledge_single_provider_risk: false,
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        }
    }

    /// Runtime options rooted in a fresh temp directory, so credential
    /// resolution reads an empty auth-profile store and never touches the
    /// operator's real configuration.
    fn byo_isolated_options(temp: &TempDir) -> ProviderRuntimeOptions {
        ProviderRuntimeOptions {
            default_temperature: None,
            model_settings: Default::default(),
            auth_profile_override: None,
            openprx_dir: Some(temp.path().to_path_buf()),
            secrets_encrypt: false,
            codex_auth_json_path: Some(temp.path().join("absent-codex-auth.json")),
            codex_auth_json_auto_import: false,
            reasoning_enabled: None,
            codex_stream_idle_timeout_secs: None,
            codex_reasoning_effort: None,
            ollama_num_ctx: None,
            ollama_model_num_ctx: Vec::new(),
            // No declarations: a BYO endpoint keeps the undeclared defaults
            // (`Raw` schemas, native tool calls only) these tests assume.
            tool_schema_dialects: Vec::new(),
            tool_call_dialects: Vec::new(),
        }
    }

    /// A loopback OpenAI-compatible endpoint owned by the test. It requires no
    /// authentication and records the headers of every request it answers.
    struct ByoLoopbackEndpoint {
        addr: std::net::SocketAddr,
        recorded: std::sync::Arc<Mutex<Vec<Vec<(String, String)>>>>,
        server: tokio::task::JoinHandle<()>,
    }

    impl ByoLoopbackEndpoint {
        fn provider_name(&self) -> String {
            format!("custom:http://{}/v1", self.addr)
        }

        fn requests(&self) -> Vec<Vec<(String, String)>> {
            self.recorded
                .lock()
                .map_or_else(|poisoned| poisoned.into_inner().clone(), |guard| guard.clone())
        }

        fn shutdown(self) {
            self.server.abort();
        }
    }

    async fn spawn_byo_loopback() -> ByoLoopbackEndpoint {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let body = format!(
            r#"{{"id":"chatcmpl-byo","object":"chat.completion","created":1,"model":"{BYO_BARE_MODEL}","choices":[{{"index":0,"message":{{"role":"assistant","content":"{BYO_LOOPBACK_REPLY}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}}}"#
        );
        let sse_body = format!(
            "data: {{\"id\":\"chatcmpl-byo\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"{BYO_BARE_MODEL}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{BYO_LOOPBACK_REPLY}\"}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n"
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind loopback");
        let addr = listener.local_addr().expect("test: loopback address");
        let recorded: std::sync::Arc<Mutex<Vec<Vec<(String, String)>>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&recorded);

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _peer)) = listener.accept().await else {
                    return;
                };
                let sink = std::sync::Arc::clone(&sink);
                let body = body.clone();
                let sse_body = sse_body.clone();
                tokio::spawn(async move {
                    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
                    let mut chunk = [0_u8; 2048];
                    let head_end = loop {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                            break position + 4;
                        }
                        if buffer.len() > 262_144 {
                            return;
                        }
                    };

                    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                    let mut headers: Vec<(String, String)> = Vec::new();
                    let mut content_length: usize = 0;
                    for line in head.split("\r\n").skip(1) {
                        if line.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            let name = name.trim().to_ascii_lowercase();
                            let value = value.trim().to_string();
                            if name == "content-length" {
                                content_length = value.parse().unwrap_or(0);
                            }
                            headers.push((name, value));
                        }
                    }

                    let mut body_read = buffer.len().saturating_sub(head_end);
                    while body_read < content_length {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => body_read += read,
                        }
                    }

                    let streaming = headers
                        .iter()
                        .any(|(name, value)| name == "accept" && value.contains("text/event-stream"));

                    if let Ok(mut guard) = sink.lock() {
                        guard.push(headers);
                    }

                    let (content_type, payload) = if streaming {
                        ("text/event-stream", sse_body.as_str())
                    } else {
                        ("application/json", body.as_str())
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });

        ByoLoopbackEndpoint { addr, recorded, server }
    }

    // ── Root connection containment (PMM P0-A) ───────────────────────────

    /// A credential-shaped sentinel configured for one provider. Not a real
    /// secret.
    const ROOT_SENTINEL_KEY: &str = "root-secret";

    /// The root connection is configured for one provider. Another provider
    /// gets nothing at all — neither the credential nor the base URL — so one
    /// vendor's secret can never be presented to another.
    #[test]
    fn root_connection_is_not_released_to_a_different_provider() {
        let connection = configured_connection_for(
            "anthropic",
            "openai",
            Some(ROOT_SENTINEL_KEY),
            Some("https://api.openai.com/v1"),
        );
        assert_eq!(connection.api_key, None);
        assert_eq!(connection.api_url, None);
    }

    #[test]
    fn default_provider_keeps_its_connection() {
        let connection = configured_connection_for(
            "openai",
            "openai",
            Some(ROOT_SENTINEL_KEY),
            Some("https://api.openai.com/v1"),
        );
        assert_eq!(connection.api_key, Some(ROOT_SENTINEL_KEY));
        assert_eq!(connection.api_url, Some("https://api.openai.com/v1"));
    }

    /// Alias canonicalization is the reason this resolver exists rather than a
    /// plain case-insensitive string comparison: `claude-code` and `anthropic`
    /// are one provider, and withholding a credential from its own alias is a
    /// usability defect the four existing precedents all carry.
    #[test]
    fn an_alias_of_the_default_provider_keeps_its_connection() {
        let same = configured_connection_for("claude-code", "anthropic", Some(ROOT_SENTINEL_KEY), None);
        assert_eq!(same.api_key, Some(ROOT_SENTINEL_KEY));

        let other = configured_connection_for("claude-code", "openai", Some(ROOT_SENTINEL_KEY), None);
        assert_eq!(other.api_key, None);
    }

    /// A bring-your-own endpoint is identified by the endpoint it addresses,
    /// so a different port is a different provider and gets nothing.
    #[test]
    fn byo_endpoints_compare_by_endpoint_identity() {
        let same = configured_connection_for(
            "CUSTOM:http://127.0.0.1:18086/v1",
            BYO_ENDPOINT_NAME,
            Some(BYO_SENTINEL_KEY),
            None,
        );
        assert_eq!(same.api_key, Some(BYO_SENTINEL_KEY));

        let other = configured_connection_for(
            "custom:http://127.0.0.1:19999/v1",
            BYO_ENDPOINT_NAME,
            Some(BYO_SENTINEL_KEY),
            None,
        );
        assert_eq!(other.api_key, None, "a different endpoint is a different provider");

        // The two dialects are not interchangeable either, even on one URL.
        let crossed = configured_connection_for(
            "anthropic-custom:http://127.0.0.1:18086/v1",
            BYO_ENDPOINT_NAME,
            Some(BYO_SENTINEL_KEY),
            None,
        );
        assert_eq!(crossed.api_key, None);
    }

    /// The classifier is the single source of provider identity. Aliases
    /// collapse to the canonical catalog name, both BYO dialects resolve to
    /// their endpoint, and anything unresolvable — including a BYO prefix
    /// whose URL is rejected — is `Unknown`.
    #[test]
    fn classify_provider_separates_catalog_entries_from_byo_endpoints() {
        assert_eq!(classify_provider("openai"), ProviderIdentity::Catalog("openai"));
        assert_eq!(classify_provider("claude-code"), ProviderIdentity::Catalog("anthropic"));
        assert_eq!(
            classify_provider("  OpenRouter "),
            ProviderIdentity::Catalog("openrouter")
        );
        assert_eq!(
            classify_provider(BYO_ENDPOINT_NAME),
            ProviderIdentity::ByoEndpoint {
                dialect: ByoDialect::OpenAiCompatible,
                base_url: "http://127.0.0.1:18086/v1".to_string(),
            }
        );
        assert_eq!(
            classify_provider("anthropic-custom:https://api.example.com"),
            ProviderIdentity::ByoEndpoint {
                dialect: ByoDialect::AnthropicCompatible,
                base_url: "https://api.example.com".to_string(),
            }
        );
        assert_eq!(classify_provider("custom:not-a-url"), ProviderIdentity::Unknown);
        assert_eq!(classify_provider("custom:ftp://example.com"), ProviderIdentity::Unknown);
        assert_eq!(classify_provider("custom:"), ProviderIdentity::Unknown);
        assert_eq!(classify_provider("nonexistent"), ProviderIdentity::Unknown);
    }

    /// SPLASH F1. A BYO endpoint owns its own model catalog, so PRX cannot
    /// reject an `org/model` id the endpoint advertises. Every other identity
    /// keeps the catalog comparison it has today, including the `hint:`
    /// hardening and the Kimi Code allowlist.
    ///
    /// RED at `ae3ca634`: the first three assertions fail with
    /// `model '<id>' is not compatible with provider '<name>'`.
    #[test]
    fn custom_endpoint_accepts_slash_qualified_model_id() {
        assert!(validate_provider_model(BYO_ENDPOINT_NAME, BYO_SLASH_MODEL).is_ok());
        assert!(validate_provider_model("anthropic-custom:https://api.example.com", BYO_SLASH_MODEL).is_ok());
        // A bare alias was already accepted and must stay accepted.
        assert!(validate_provider_model(BYO_ENDPOINT_NAME, BYO_BARE_MODEL).is_ok());

        // Known constrained providers keep strict validation.
        assert!(validate_provider_model("anthropic", BYO_SLASH_MODEL).is_err());
        assert!(validate_provider_model("openai", "mistralai/mistral-7b").is_err());
        assert!(validate_provider_model("kimi-code", "k3").is_ok());
        assert!(validate_provider_model("kimi_coding", "kimi-for-coding").is_ok());
        assert!(validate_provider_model("kimi-code", "kimi-for-coding-highspeed").is_ok());
        assert!(validate_provider_model("kimi-code", "kimi-k2.7-code").is_err());
        assert!(validate_provider_model("kimi-code", "definitely-not-a-real-model").is_err());

        // d04 §9.2: an unresolved hint stays a non-match in front of the new
        // BYO arm, not behind it.
        assert!(!provider_matches_model_prefix(BYO_ENDPOINT_NAME, "hint:anything"));

        // An unparseable BYO string is not silently promoted to "accepts
        // anything": it has no endpoint identity at all.
        assert!(validate_provider_model("custom:not-a-url", BYO_SLASH_MODEL).is_err());
    }

    /// SPLASH F5, S4 case A. A keyless BYO endpoint configured as the default
    /// provider must start. The refusal it replaces was pre-transport, so the
    /// endpoint must still see no request before the first turn.
    ///
    /// RED at `ae3ca634`: `Err(... missing credential/api key)`.
    #[tokio::test]
    async fn keyless_byo_endpoint_starts_without_a_credential() {
        let endpoint = spawn_byo_loopback().await;
        let temp = TempDir::new().expect("test: temp config dir");
        let options = byo_isolated_options(&temp);
        let reliability = byo_reliability_without_fallbacks();
        let provider_name = endpoint.provider_name();
        let config = factory_config(&provider_name, BYO_BARE_MODEL, None, None, reliability.clone());
        let outcome = create_provider_from_test_config(&config, &options);

        assert!(
            reliability.fallback_providers.is_empty(),
            "an available fallback would mask the primary's refusal"
        );
        assert!(
            outcome.is_ok(),
            "a keyless BYO endpoint must start: {:?}",
            outcome.err().map(|error| format!("{error:#}"))
        );
        assert_eq!(
            endpoint.requests().len(),
            0,
            "provider construction must not touch the endpoint"
        );
        endpoint.shutdown();
    }

    /// SPLASH F5 second order, decision D-012 (S1 Q3). The relaxation is
    /// symmetric with `ollama`: a keyless BYO endpoint listed as a fallback
    /// becomes available too. The credential gate itself must stay intact for
    /// catalog providers, which the `mistral` control pins.
    ///
    /// RED at `ae3ca634`: the BYO fallback is reported
    /// `missing credential/api key`.
    #[tokio::test]
    async fn keyless_byo_fallback_becomes_available_while_the_gate_stays_intact() {
        let endpoint = spawn_byo_loopback().await;
        let temp = TempDir::new().expect("test: temp config dir");
        let options = byo_isolated_options(&temp);
        let provider_name = endpoint.provider_name();

        let mut reliability = byo_reliability_without_fallbacks();
        reliability.fallback_providers = vec![provider_name.clone()];

        let summary = summarize_provider_availability("ollama", None, &reliability, &options);
        assert!(
            summary.available.iter().any(|name| name == &provider_name),
            "keyless BYO fallback must be available: {:?}",
            summary.unavailable
        );

        // Control: a catalog provider with no credential is still refused, and
        // an empty fallback list cannot mask that refusal.
        let gate = byo_reliability_without_fallbacks();
        let catalog = summarize_provider_availability("mistral", None, &gate, &options);
        assert!(catalog.available.is_empty(), "{:?}", catalog.available);
        assert!(
            catalog
                .unavailable
                .iter()
                .any(|(name, reason)| name == "mistral" && reason == "missing credential/api key"),
            "{:?}",
            catalog.unavailable
        );
        endpoint.shutdown();
    }

    /// R-C1 F-1. `AnthropicProvider` has no keyless mode, so an
    /// `anthropic-custom:<url>` configured without a credential can never
    /// serve a turn. The credential exemption therefore covers the
    /// OpenAI-compatible dialect only: the Anthropic dialect is refused at the
    /// startup gate, where the diagnostic is accurate and no transport exists,
    /// instead of at the first turn — and it does not join a fallback chain it
    /// could never serve.
    ///
    /// RED at `c7d3fead`: the keyless Anthropic-dialect endpoint is reported
    /// available, as primary and as fallback.
    #[tokio::test]
    async fn keyless_anthropic_dialect_endpoint_is_refused_at_the_credential_gate() {
        const ANTHROPIC_DIALECT_ENDPOINT: &str = "anthropic-custom:http://127.0.0.1:18086/v1";
        let temp = TempDir::new().expect("test: temp config dir");
        let options = byo_isolated_options(&temp);
        let reliability = byo_reliability_without_fallbacks();

        assert!(
            provider_requires_explicit_credential(ANTHROPIC_DIALECT_ENDPOINT),
            "the Anthropic dialect has no keyless mode and must keep the requirement"
        );
        assert!(
            !provider_requires_explicit_credential(BYO_ENDPOINT_NAME),
            "the OpenAI-compatible dialect keeps its exemption"
        );

        let summary = summarize_provider_availability(ANTHROPIC_DIALECT_ENDPOINT, None, &reliability, &options);
        assert!(summary.available.is_empty(), "{:?}", summary.available);
        assert!(
            summary
                .unavailable
                .iter()
                .any(|(name, reason)| name == ANTHROPIC_DIALECT_ENDPOINT && reason == "missing credential/api key"),
            "{:?}",
            summary.unavailable
        );

        // With a credential of its own it is available, exactly as before.
        let keyed = summarize_provider_availability(
            ANTHROPIC_DIALECT_ENDPOINT,
            Some(BYO_SENTINEL_KEY),
            &reliability,
            &options,
        );
        assert!(
            keyed.available.iter().any(|name| name == ANTHROPIC_DIALECT_ENDPOINT),
            "{:?}",
            keyed.unavailable
        );

        // And a keyless one is not marked available as a fallback either.
        let mut with_fallback = byo_reliability_without_fallbacks();
        with_fallback.fallback_providers = vec![ANTHROPIC_DIALECT_ENDPOINT.to_string()];
        let chained = summarize_provider_availability("ollama", None, &with_fallback, &options);
        assert!(
            !chained.available.iter().any(|name| name == ANTHROPIC_DIALECT_ENDPOINT),
            "{:?}",
            chained.available
        );
    }

    /// R-C1 F-4. This module folds a provider name in two places: the
    /// classifier, which decides identity for credential release, and the
    /// auth-profile canonicalizer, which decides where the credential is
    /// stored. A name that folds one way in one and another way in the other
    /// withholds a key its own store says is shared. Guard test: it pins the
    /// agreement across both tables rather than a behavior of its own.
    ///
    /// RED at `c7d3fead`: the regional aliases the catalog does not list as
    /// rows (`bigmodel`, `glm-cn`, `kimi-intl`, `zai-cn`, …) classify
    /// `Unknown` while the credential store folds them onto their vendor.
    #[test]
    fn provider_identity_agrees_with_auth_profile_canonicalization() {
        /// Every alias the regional tables recognise but `list_providers()`
        /// does not carry as a name or alias.
        const REGIONAL_ALIASES: &[&str] = &[
            "minimax-oauth-global",
            "minimax-portal-global",
            "glm-cn",
            "zhipu-cn",
            "bigmodel",
            "glm-global",
            "zhipu-global",
            "moonshot-intl",
            "moonshot-global",
            "kimi-intl",
            "kimi-global",
            "moonshot-cn",
            "kimi-cn",
            "qwen-cn",
            "dashscope-cn",
            "qwen-international",
            "dashscope-international",
            "dashscope-coding",
            "qwen-coding",
            "dashscope-coding-intl",
            "qwen-coding-intl",
            "zai-global",
            "z.ai-global",
            "zai-cn",
            "z.ai-cn",
        ];

        let mut names: Vec<String> = Vec::new();
        for info in list_providers() {
            names.push(info.name.to_string());
            names.extend(info.aliases.iter().map(|alias| (*alias).to_string()));
        }
        names.extend(REGIONAL_ALIASES.iter().map(|alias| (*alias).to_string()));

        for name in names {
            match classify_provider(&name) {
                ProviderIdentity::Catalog(canonical) => assert_eq!(
                    canonical_auth_profile_provider(&name),
                    canonical,
                    "`{name}` folds to two different providers"
                ),
                other => panic!("test: `{name}` must classify as a catalog provider, got {other:?}"),
            }
        }
    }

    /// R-C1 F-3. `qwen`, `qwen-intl` and `qwen-us` share one catalog row but
    /// address three different servers, and so do `minimax` / `minimax-cn`,
    /// `glm` / `bigmodel`, `moonshot` / `kimi-intl` and `zai` / `zai-cn`. A
    /// credential issued for one region is not a credential for another, so
    /// the identity the resolvers compare carries the endpoint the name
    /// selects. Spellings of one region stay one identity.
    ///
    /// RED at `c7d3fead`: the regional spellings collapse onto the catalog row
    /// and a key bound to one region is released to the other.
    #[test]
    fn regional_aliases_of_one_catalog_row_keep_their_own_endpoint_identity() {
        assert_ne!(canonical_provider_key("qwen"), canonical_provider_key("qwen-us"));
        assert_ne!(canonical_provider_key("qwen"), canonical_provider_key("qwen-intl"));
        assert_ne!(canonical_provider_key("qwen"), canonical_provider_key("qwen-coding"));
        assert_ne!(canonical_provider_key("minimax"), canonical_provider_key("minimax-cn"));
        assert_ne!(canonical_provider_key("glm"), canonical_provider_key("bigmodel"));
        assert_ne!(canonical_provider_key("moonshot"), canonical_provider_key("kimi-intl"));
        assert_ne!(canonical_provider_key("zai"), canonical_provider_key("zai-cn"));

        assert_eq!(canonical_provider_key("qwen"), canonical_provider_key("dashscope"));
        assert_eq!(
            canonical_provider_key("qwen-us"),
            canonical_provider_key("dashscope-us")
        );
        assert_eq!(canonical_provider_key("minimax-cn"), canonical_provider_key("minimaxi"));
        assert_eq!(canonical_provider_key("glm"), canonical_provider_key("zhipu"));
        assert_eq!(canonical_provider_key("zai"), canonical_provider_key("z.ai"));

        // The root connection follows that identity: the same region keeps it,
        // another region gets nothing.
        assert_eq!(
            configured_connection_for("dashscope", "qwen", Some(ROOT_SENTINEL_KEY), None).api_key,
            Some(ROOT_SENTINEL_KEY)
        );
        assert_eq!(
            configured_connection_for("qwen-us", "qwen", Some(ROOT_SENTINEL_KEY), None).api_key,
            None,
            "a key bound to one region must not be released to another"
        );
    }

    /// S4 case E. An unparseable BYO string has no endpoint identity: it is
    /// refused with the message `parse_custom_provider_url` already produces,
    /// before any transport exists, and it is not treated as credential-free.
    #[tokio::test]
    async fn unparseable_byo_endpoint_is_refused_before_any_transport() {
        let endpoint = spawn_byo_loopback().await;
        let temp = TempDir::new().expect("test: temp config dir");
        let options = byo_isolated_options(&temp);
        let reliability = byo_reliability_without_fallbacks();

        let config = factory_config("custom:not-a-url", BYO_BARE_MODEL, None, None, reliability);
        let error = create_provider_from_test_config(&config, &options)
            .err()
            .map(|error| format!("{error:#}"))
            .expect("test: an unparseable BYO URL must be refused");

        assert!(
            error.contains("is not one PRX can construct"),
            "unexpected error: {error}"
        );
        assert!(
            provider_requires_explicit_credential("custom:not-a-url"),
            "an unparseable BYO string must not inherit the credential-free arm"
        );
        assert_eq!(endpoint.requests().len(), 0, "a refusal must be pre-transport");
        endpoint.shutdown();
    }

    /// SPLASH F5 second refusal layer, S4 case P6 inverted. A keyless BYO
    /// endpoint must reach its endpoint with no `Authorization` header at all
    /// — never with a placeholder credential — on every request surface, not
    /// only the one the first turn happens to use.
    ///
    /// RED at `ae3ca634`: every surface fails with `Custom API key not set`
    /// and zero requests reach the endpoint.
    #[tokio::test]
    async fn keyless_byo_endpoint_sends_no_authorization_header_on_any_surface() {
        use futures::StreamExt;

        let endpoint = spawn_byo_loopback().await;
        let provider =
            create_provider(&endpoint.provider_name(), None).expect("test: keyless BYO construction is valid");
        let history = [ChatMessage::user("ping")];
        let stream_options = traits::StreamOptions::new(true);

        let metered = provider
            .chat(
                ChatRequest {
                    messages: &history,
                    tools: None,
                },
                BYO_BARE_MODEL,
                0.0,
            )
            .await;
        let with_system = provider.chat_with_system(None, "ping", BYO_BARE_MODEL, 0.0).await;
        let with_history = provider.chat_with_history(&history, BYO_BARE_MODEL, 0.0).await;
        let with_tools = provider.chat_with_tools(&history, &[], BYO_BARE_MODEL, 0.0).await;
        let streamed_system: Vec<_> = provider
            .stream_chat_with_system(None, "ping", BYO_BARE_MODEL, 0.0, stream_options.clone())
            .collect()
            .await;
        let streamed_history: Vec<_> = provider
            .stream_chat_with_history(&history, BYO_BARE_MODEL, 0.0, stream_options)
            .collect()
            .await;

        assert_eq!(
            metered.map(|response| response.text).ok().flatten().as_deref(),
            Some(BYO_LOOPBACK_REPLY)
        );
        assert_eq!(with_system.as_deref().ok(), Some(BYO_LOOPBACK_REPLY));
        assert_eq!(with_history.as_deref().ok(), Some(BYO_LOOPBACK_REPLY));
        assert_eq!(
            with_tools.map(|response| response.text).ok().flatten().as_deref(),
            Some(BYO_LOOPBACK_REPLY)
        );
        for chunks in [&streamed_system, &streamed_history] {
            assert!(
                chunks.iter().all(std::result::Result::is_ok),
                "a keyless stream must not be refused: {:?}",
                chunks
                    .iter()
                    .filter_map(|chunk| chunk.as_ref().err())
                    .collect::<Vec<_>>()
            );
        }

        let requests = endpoint.requests();
        assert_eq!(requests.len(), 6, "every surface must reach the endpoint");
        for headers in &requests {
            assert!(
                !headers.iter().any(|(name, _)| name == "authorization"),
                "a keyless endpoint must receive no authorization header: {:?}",
                headers.iter().map(|(name, _)| name).collect::<Vec<_>>()
            );
        }
        endpoint.shutdown();
    }

    /// The keyless relaxation is scoped to the absence of a credential: a BYO
    /// endpoint that does have one still presents it.
    #[tokio::test]
    async fn keyed_byo_endpoint_still_presents_its_own_credential() {
        let endpoint = spawn_byo_loopback().await;
        let provider = create_provider(&endpoint.provider_name(), Some(BYO_SENTINEL_KEY))
            .expect("test: keyed BYO construction is valid");

        let reply = provider.simple_chat("ping", BYO_BARE_MODEL, 0.0).await;

        let requests = endpoint.requests();
        assert_eq!(reply.as_deref().ok(), Some(BYO_LOOPBACK_REPLY));
        assert_eq!(requests.len(), 1);
        let authorization = requests
            .first()
            .and_then(|headers| {
                headers
                    .iter()
                    .find(|(name, _)| name == "authorization")
                    .map(|(_, value)| value.clone())
            })
            .expect("test: a keyed endpoint receives an authorization header");
        assert_eq!(authorization, format!("Bearer {BYO_SENTINEL_KEY}"));
        endpoint.shutdown();
    }
}

#[cfg(test)]
mod fallback_zero_price_tests {
    use super::*;
    use crate::providers::catalog::{Requirement, Selection, UnknownPolicy, resolve};
    use crate::providers::credentials::{ConfigCredentialSource, CredentialRef, LegacySlot};

    fn sibling_is_cost_admitted(api_url: Option<&str>) -> bool {
        let mut config = crate::config::Config::default();
        config.default_provider = Some("ollama".to_string());
        config.api_url = api_url.map(str::to_string);
        config.api_key = Some("test-ollama-root-key".to_string());
        let catalog = catalog::CatalogView::from_config(&config);
        let resolved = resolve(
            &Selection {
                provider: "ollama",
                model: "prx-local-model",
                credential: CredentialRef::LegacyInline(LegacySlot::Root),
            },
            &Requirement::chat(),
            &catalog,
            &ConfigCredentialSource::new(&config),
            UnknownPolicy::Invocation,
        )
        .expect("test: ollama primary resolves");
        let mut reliability = config.reliability.clone();
        reliability
            .model_fallbacks
            .insert("prx-local-model".to_string(), vec!["prx-unpriced-sibling".to_string()]);
        let admission = fallback_admission_for(&resolved, &catalog, &reliability, &config.cost);
        admission
            .eligibility(&ProviderId::of("ollama"), "prx-unpriced-sibling")
            .expect("test: sibling is described")
            .cost_admitted
    }

    #[test]
    fn an_unpriced_sibling_is_free_only_on_a_local_ollama_primary() {
        assert!(sibling_is_cost_admitted(None));
        assert!(sibling_is_cost_admitted(Some("http://localhost:11434")));
        assert!(!sibling_is_cost_admitted(Some("https://ollama.com")));
    }
}

/// Credential containment across provider identities: the root connection and
/// its rotation keys (`reliability.api_keys`) reach only the provider they were
/// configured for, whatever path builds the chain.
#[cfg(test)]
mod credential_containment_tests {
    use super::credential_probe::{CredentialProbe, PROBE_MODEL};
    use super::*;

    /// Sentinels only; none of these is a real secret.
    const ROOT_SENTINEL: &str = "C0-ROOT-SENTINEL";
    const ROTATION_SENTINEL: &str = "C0-ROTATION-SENTINEL";

    fn isolated_options(temp: &tempfile::TempDir) -> ProviderRuntimeOptions {
        ProviderRuntimeOptions {
            openprx_dir: Some(temp.path().to_path_buf()),
            codex_auth_json_path: Some(temp.path().join("no-codex-auth.json")),
            codex_auth_json_auto_import: false,
            ..ProviderRuntimeOptions::default()
        }
    }

    fn rotating_reliability() -> crate::config::ReliabilityConfig {
        crate::config::ReliabilityConfig {
            provider_retries: 0,
            provider_backoff_ms: 0,
            fallback_providers: Vec::new(),
            api_keys: vec![ROTATION_SENTINEL.to_string()],
            ..crate::config::ReliabilityConfig::default()
        }
    }

    fn routed_config(
        provider: &str,
        model: &str,
        api_key: Option<&str>,
        api_url: Option<&str>,
        routes: Vec<crate::config::ModelRouteConfig>,
    ) -> crate::config::Config {
        crate::config::Config {
            default_provider: Some(provider.to_string()),
            default_model: Some(model.to_string()),
            api_key: api_key.map(str::to_string),
            api_url: api_url.map(str::to_string),
            reliability: rotating_reliability(),
            model_routes: routes,
            ..crate::config::Config::default()
        }
    }

    /// T7: a `model_routes` entry for another provider must not inherit the
    /// root's rotation keys. The loopback answers 429 first, which is exactly
    /// what makes the chain rotate to its next member.
    #[tokio::test]
    async fn a_routed_provider_never_receives_the_root_rotation_keys() {
        let temp = tempfile::tempdir().expect("test: temp config dir");
        let probe = CredentialProbe::start_with_statuses(vec![429]).await;
        let routes = vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "probe".to_string(),
            provider: probe.custom_provider(),
            model: PROBE_MODEL.to_string(),
            api_key: None,
            credential_ref: None,
        }];

        let config = routed_config("openai", "gpt-4o", Some(ROOT_SENTINEL), None, routes);
        let provider =
            create_provider_from_test_config(&config, &isolated_options(&temp)).expect("test: routed chain builds");

        let _reply = provider.simple_chat("ping", "hint:probe", 0.0).await;

        let requests = probe.requests();
        assert!(!requests.is_empty(), "the route must reach the loopback");
        assert!(
            !probe.saw(ROTATION_SENTINEL),
            "the root rotation key reached a routed provider it was not issued for"
        );
        assert!(!probe.saw(ROOT_SENTINEL), "the root key reached a routed provider");
        assert!(
            requests.iter().all(|request| request.authorization().is_none()),
            "a keyless routed endpoint must receive no Authorization header"
        );
    }

    /// Guard: the primary provider keeps its rotation keys when routing is on.
    /// Its first member is throttled, so the rotated member presents the key.
    #[tokio::test]
    async fn the_routed_primary_keeps_its_rotation_keys() {
        let temp = tempfile::tempdir().expect("test: temp config dir");
        let probe = CredentialProbe::start_with_statuses(vec![429]).await;
        let routes = vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "probe".to_string(),
            provider: probe.custom_provider(),
            model: PROBE_MODEL.to_string(),
            api_key: None,
            credential_ref: None,
        }];

        let config = routed_config(&probe.custom_provider(), PROBE_MODEL, Some(ROOT_SENTINEL), None, routes);
        let provider =
            create_provider_from_test_config(&config, &isolated_options(&temp)).expect("test: routed chain builds");

        let reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

        assert!(reply.is_ok(), "the rotated member must answer: {reply:?}");
        let requests = probe.requests();
        assert_eq!(requests.len(), 2, "one throttled attempt, one rotated attempt");
        assert!(
            requests
                .iter()
                .all(|request| request.method == "POST" && request.path.ends_with("/chat/completions"))
        );
        assert_eq!(
            requests.first().and_then(|request| request.authorization()),
            Some(format!("Bearer {ROOT_SENTINEL}").as_str())
        );
        assert_eq!(
            requests.get(1).and_then(|request| request.authorization()),
            Some(format!("Bearer {ROTATION_SENTINEL}").as_str())
        );
    }

    /// A route's own key is that route's credential and is still presented.
    #[tokio::test]
    async fn a_route_with_its_own_key_still_presents_it() {
        const ROUTE_SENTINEL: &str = "C0-ROUTE-SENTINEL";
        let temp = tempfile::tempdir().expect("test: temp config dir");
        let probe = CredentialProbe::start().await;
        let routes = vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "probe".to_string(),
            provider: probe.custom_provider(),
            model: PROBE_MODEL.to_string(),
            api_key: Some(ROUTE_SENTINEL.to_string()),
            credential_ref: None,
        }];

        let config = routed_config("openai", "gpt-4o", Some(ROOT_SENTINEL), None, routes);
        let provider =
            create_provider_from_test_config(&config, &isolated_options(&temp)).expect("test: routed chain builds");

        let reply = provider.simple_chat("ping", "hint:probe", 0.0).await;

        assert!(reply.is_ok(), "the route answers with its own key: {reply:?}");
        assert_eq!(probe.wait_for_requests(1, std::time::Duration::from_secs(5)).await, 1);
        assert!(probe.saw(ROUTE_SENTINEL));
        assert!(!probe.saw(ROOT_SENTINEL) && !probe.saw(ROTATION_SENTINEL));
    }

    #[test]
    fn rotation_keys_follow_the_root_connection() {
        let reliability = rotating_reliability();

        let released = configured_connection_for("OpenAI", "openai", Some(ROOT_SENTINEL), None);
        let scoped = scoped_reliability(&reliability, &released);
        assert!(released.released);
        assert!(matches!(scoped, std::borrow::Cow::Borrowed(_)));
        assert_eq!(scoped.api_keys, vec![ROTATION_SENTINEL.to_string()]);

        let withheld = configured_connection_for("custom:http://127.0.0.1:9/v1", "openai", Some(ROOT_SENTINEL), None);
        let scoped = scoped_reliability(&reliability, &withheld);
        assert!(!withheld.released);
        assert!(
            scoped.api_keys.is_empty(),
            "a withheld connection carries no rotation keys"
        );
        assert_eq!(scoped.provider_retries, reliability.provider_retries);
        assert_eq!(scoped.provider_backoff_ms, reliability.provider_backoff_ms);
        assert_eq!(scoped.fallback_providers, reliability.fallback_providers);
    }

    /// The factory-level shape of P0-B: a withheld connection plus the scoped
    /// reliability sends neither the root key nor a rotation key.
    #[tokio::test]
    async fn a_withheld_connection_builds_a_chain_without_rotation_keys() {
        let temp = tempfile::tempdir().expect("test: temp config dir");
        let probe = CredentialProbe::start_with_statuses(vec![429]).await;
        let selected = probe.custom_provider();
        let config = routed_config("openai", "gpt-4o", Some(ROOT_SENTINEL), None, Vec::new());
        let catalog = selection::catalog_for(&config);
        let resolved = selection::check_with(
            &config,
            &catalog,
            &selected,
            PROBE_MODEL,
            selection::credential_for(&config, &selected),
            &catalog::Requirement::chat(),
            catalog::UnknownPolicy::Invocation,
        )
        .expect("test: keyless BYO selection resolves");
        let provider = create_provider_for(
            &resolved,
            &catalog,
            &config.reliability,
            &config.cost,
            &isolated_options(&temp),
        )
        .expect("test: keyless BYO chain builds");
        let _reply = provider.simple_chat("ping", PROBE_MODEL, 0.0).await;

        assert_eq!(probe.requests().len(), 1, "one member, no rotated member");
        assert!(!probe.saw(ROOT_SENTINEL) && !probe.saw(ROTATION_SENTINEL));
    }

    #[test]
    fn configured_providers_are_every_provider_the_operator_named() {
        let mut config = crate::config::Config::default();
        config.default_provider = None;
        config.reliability.fallback_providers = vec!["anthropic".to_string()];
        config.model_routes = vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "fast".to_string(),
            provider: "custom:http://127.0.0.1:8080/v1".to_string(),
            model: "m".to_string(),
            api_key: None,
            credential_ref: None,
        }];
        config.agents = std::collections::HashMap::from([(
            "coder".to_string(),
            crate::config::DelegateAgentConfig {
                provider: "groq".to_string(),
                model: "m".to_string(),
                system_prompt: None,
                api_key: None,
                credential_ref: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        )]);

        let configured = ConfiguredProviders::from_config(&config);

        for permitted in [
            "openrouter",
            "anthropic",
            "claude-code",
            "groq",
            "custom:http://127.0.0.1:8080/v1/",
            "CUSTOM:http://127.0.0.1:8080/v1",
        ] {
            assert!(configured.permits(permitted), "{permitted}");
        }
        for refused in [
            "openai",
            "custom:http://127.0.0.1:8081/v1",
            "custom:https://attacker.example/v1",
        ] {
            assert!(!configured.permits(refused), "{refused}");
        }
    }

    #[test]
    fn a_refused_provider_is_named_without_its_userinfo() {
        let configured = ConfiguredProviders::default()
            .with_providers(["custom:http://operator-secret@127.0.0.1:8080/v1", "openai"]);

        let error = configured
            .ensure_permits("custom:https://model-secret@attacker.example/v1")
            .expect_err("an unconfigured endpoint is refused")
            .to_string();

        assert!(error.contains("not configured"), "{error}");
        assert!(
            !error.contains("model-secret") && !error.contains("operator-secret"),
            "{error}"
        );
        assert!(configured.ensure_permits("OpenAI").is_ok());
    }

    /// C0-f F-3: the refusal goes back to the model, so it names provider
    /// identities only: never a configured endpoint's URL, host or port, and
    /// never the refused one's.
    #[test]
    fn a_refusal_names_identities_but_no_endpoint() {
        let configured = ConfiguredProviders::default().with_providers([
            "custom:http://internal-llm.corp.example:8443/v1",
            "anthropic-custom:https://10.1.2.3/anthropic",
            "openai",
        ]);

        let error = configured
            .ensure_permits("custom:https://attacker.example:9999/v1")
            .expect_err("an unconfigured endpoint is refused")
            .to_string();

        for leaked in [
            "internal-llm",
            "corp.example",
            "8443",
            "10.1.2.3",
            "attacker.example",
            "9999",
            "http",
        ] {
            assert!(!error.contains(leaked), "the refusal leaked `{leaked}`: {error}");
        }
        assert!(error.contains("not configured"), "{error}");
        assert!(error.contains("openai"), "catalog identities stay nameable: {error}");
    }

    #[test]
    fn a_credential_is_released_only_to_its_own_provider_identity() {
        let owned = [
            (Some("groq"), Some(" agent-key ")),
            (Some("custom:http://127.0.0.1:8080/v1"), Some("root-key")),
            (None, Some("ownerless-key")),
        ];
        assert_eq!(credential_for_provider_identity("GROQ", &owned), Some("agent-key"));
        assert_eq!(
            credential_for_provider_identity("custom:http://127.0.0.1:8080/v1/", &owned),
            Some("root-key")
        );
        assert_eq!(
            credential_for_provider_identity("custom:https://attacker.example/v1", &owned),
            None
        );
        assert_eq!(credential_for_provider_identity("openai", &owned), None);
        assert_eq!(
            credential_for_provider_identity("groq", &[(Some("groq"), Some("  "))]),
            None
        );
    }

    // ── C0-f F-4: a route gets key, URL and rotation keys as one unit ─────

    #[test]
    fn a_respelled_primary_route_gets_the_root_connection_as_a_unit() {
        let config = routed_config(
            "openai",
            "gpt-4o",
            Some(ROOT_SENTINEL),
            Some("https://gateway.corp.example/v1"),
            vec![crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "primary".to_string(),
                provider: "OpenAI".to_string(),
                model: "gpt-4o".to_string(),
                api_key: None,
                credential_ref: None,
            }],
        );
        let catalog = selection::catalog_for(&config);
        let route = config.model_routes.first().expect("test: primary route");
        let resolved = selection::resolve_route(&config, &catalog, route, catalog::UnknownPolicy::Invocation)
            .expect("test: respelled primary resolves");
        assert_eq!(resolved.credential().api_key(), Some(ROOT_SENTINEL));
        assert_eq!(resolved.credential().api_url(), Some("https://gateway.corp.example/v1"));
        assert_eq!(resolved.credential().rotation_keys(), &[ROTATION_SENTINEL.to_string()]);
    }

    #[test]
    fn a_route_with_its_own_key_gets_neither_root_url_nor_rotation_keys() {
        let config = routed_config(
            "openai",
            "gpt-4o",
            Some(ROOT_SENTINEL),
            Some("https://gateway.corp.example/v1"),
            vec![crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "owned".to_string(),
                provider: "OpenAI".to_string(),
                model: "gpt-4o".to_string(),
                api_key: Some("route-own-key".to_string()),
                credential_ref: None,
            }],
        );
        let catalog = selection::catalog_for(&config);
        let route = config.model_routes.first().expect("test: owned route");
        let resolved = selection::resolve_route(&config, &catalog, route, catalog::UnknownPolicy::Invocation)
            .expect("test: owned route resolves");
        assert_eq!(resolved.credential().api_key(), Some("route-own-key"));
        assert_eq!(resolved.credential().api_url(), None);
        assert!(
            resolved.credential().rotation_keys().is_empty(),
            "the root's rotation keys are not the route's"
        );
    }

    #[test]
    fn the_exact_primary_and_foreign_routes_are_unchanged() {
        let config = routed_config(
            "openai",
            "gpt-4o",
            Some(ROOT_SENTINEL),
            Some("https://gw/v1"),
            vec![crate::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "foreign".to_string(),
                provider: "groq".to_string(),
                model: "llama-3.3-70b-versatile".to_string(),
                api_key: None,
                credential_ref: None,
            }],
        );
        let catalog = selection::catalog_for(&config);
        let primary =
            selection::check_default(&config, catalog::UnknownPolicy::Invocation).expect("test: primary resolves");
        assert_eq!(
            (primary.credential().api_key(), primary.credential().api_url()),
            (Some(ROOT_SENTINEL), Some("https://gw/v1"))
        );
        assert_eq!(primary.credential().rotation_keys().len(), 1);

        let route = config.model_routes.first().expect("test: foreign route");
        let foreign = selection::resolve_route(&config, &catalog, route, catalog::UnknownPolicy::Invocation)
            .expect("test: foreign route resolves");
        assert_eq!(
            (foreign.credential().api_key(), foreign.credential().api_url()),
            (None, None)
        );
        assert!(
            foreign.credential().rotation_keys().is_empty(),
            "expected an empty value"
        );
    }

    /// On the wire: a route spelling the primary endpoint differently is
    /// served with the root key first, not only with a rotation key.
    #[tokio::test]
    async fn a_respelled_primary_route_presents_the_root_key_with_its_rotation_keys() {
        let temp = tempfile::tempdir().expect("test: temp config dir");
        let probe = CredentialProbe::start_with_statuses(vec![429]).await;
        let primary = probe.custom_provider();
        let respelled = format!("{primary}/");
        let routes = vec![crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "probe".to_string(),
            provider: respelled,
            model: PROBE_MODEL.to_string(),
            api_key: None,
            credential_ref: None,
        }];

        let config = routed_config(&primary, PROBE_MODEL, Some(ROOT_SENTINEL), None, routes);
        let provider =
            create_provider_from_test_config(&config, &isolated_options(&temp)).expect("test: routed chain builds");
        let _reply = provider.simple_chat("ping", "hint:probe", 0.0).await;

        let requests = probe.requests();
        assert_eq!(
            requests.first().and_then(|request| request.authorization()),
            Some(format!("Bearer {ROOT_SENTINEL}").as_str()),
            "the respelled route must present the root key, not a rotation key alone"
        );
    }
}

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
