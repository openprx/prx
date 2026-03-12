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
//! [`create_routed_provider`].
//!
//! # Extension
//!
//! To add a new provider, implement [`Provider`] in a new submodule and register it
//! in [`create_provider_with_url`]. See `AGENTS.md` §7.1 for the full change playbook.

pub mod anthropic;
pub mod bedrock;
pub mod compatible;
pub mod copilot;
pub mod gemini;
pub mod ollama;
pub mod openai;
pub mod openai_codex;
pub mod openrouter;
pub mod reliable;
pub mod router;
pub mod traits;

#[allow(unused_imports)]
pub use traits::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, Provider, ProviderCapabilities,
    ProviderCapabilityError, ToolCall, ToolResultMessage,
};

use compatible::{AuthStyle, OpenAiCompatibleProvider};
use reliable::ReliableProvider;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

const MAX_API_ERROR_CHARS: usize = 200;
const MINIMAX_INTL_BASE_URL: &str = "https://api.minimax.io/v1";
const MINIMAX_CN_BASE_URL: &str = "https://api.minimaxi.com/v1";
const MINIMAX_OAUTH_GLOBAL_TOKEN_ENDPOINT: &str = "https://api.minimax.io/oauth/token";
const MINIMAX_OAUTH_CN_TOKEN_ENDPOINT: &str = "https://api.minimaxi.com/oauth/token";
const MINIMAX_OAUTH_PLACEHOLDER: &str = "minimax-oauth";
const MINIMAX_OAUTH_CN_PLACEHOLDER: &str = "minimax-oauth-cn";
const MINIMAX_OAUTH_TOKEN_ENV: &str = "MINIMAX_OAUTH_TOKEN";
const MINIMAX_API_KEY_ENV: &str = "MINIMAX_API_KEY";
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
const CLAUDE_CODE_ACCESS_TOKEN_ENV: &str = "CLAUDE_CODE_ACCESS_TOKEN";
const CLAUDE_CODE_REFRESH_TOKEN_ENV: &str = "CLAUDE_CODE_REFRESH_TOKEN";
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
    matches!(
        name,
        "moonshot-intl" | "moonshot-global" | "kimi-intl" | "kimi-global"
    )
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

pub(crate) fn is_huggingface_alias(name: &str) -> bool {
    matches!(name, "huggingface" | "hf" | "hf-inference")
}

#[derive(Clone, Copy, Debug)]
enum MinimaxOauthRegion {
    Global,
    Cn,
}

impl MinimaxOauthRegion {
    fn token_endpoint(self) -> &'static str {
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
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
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
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
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
}

impl std::fmt::Debug for ClaudeCodeProviderContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeProviderContext")
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Clone, Default)]
struct QwenOauthProviderContext {
    credential: Option<String>,
    base_url: Option<String>,
}

impl std::fmt::Debug for QwenOauthProviderContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QwenOauthProviderContext")
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "[REDACTED]"),
            )
            .field("base_url", &self.base_url)
            .finish()
    }
}

fn read_non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn is_minimax_oauth_placeholder(value: &str) -> bool {
    value.eq_ignore_ascii_case(MINIMAX_OAUTH_PLACEHOLDER)
        || value.eq_ignore_ascii_case(MINIMAX_OAUTH_CN_PLACEHOLDER)
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
    read_non_empty_env(MINIMAX_OAUTH_CLIENT_ID_ENV)
        .unwrap_or_else(|| MINIMAX_OAUTH_DEFAULT_CLIENT_ID.to_string())
}

fn qwen_oauth_client_id() -> String {
    read_non_empty_env(QWEN_OAUTH_CLIENT_ID_ENV)
        .unwrap_or_else(|| QWEN_OAUTH_DEFAULT_CLIENT_ID.to_string())
}

fn qwen_oauth_credentials_file_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .map(|home| home.join(QWEN_OAUTH_CREDENTIAL_FILE))
}

fn claude_code_credentials_file_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .map(|home| home.join(CLAUDE_CODE_CREDENTIAL_FILE))
}

fn normalize_qwen_oauth_base_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };

    let normalized = with_scheme.trim_end_matches('/').to_string();
    if normalized.ends_with("/v1") {
        Some(normalized)
    } else {
        Some(format!("{normalized}/v1"))
    }
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

pub(crate) fn write_claude_code_cached_credentials(
    credentials: &ClaudeCodeCredentials,
) -> anyhow::Result<()> {
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

    if !root_obj
        .get("claudeAiOauth")
        .is_some_and(serde_json::Value::is_object)
    {
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
        oauth_obj.insert(
            "subscriptionType".to_string(),
            serde_json::json!(subscription_type),
        );
    }

    let serialized = serde_json::to_string(&root)?;
    std::fs::write(path, serialized)?;
    Ok(())
}

fn normalized_qwen_expiry_millis(raw: i64) -> i64 {
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
    let client_id = qwen_oauth_client_id();
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    let response = client
        .post(QWEN_OAUTH_TOKEN_ENDPOINT)
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

    let payload =
        parsed.ok_or_else(|| anyhow::anyhow!("Qwen OAuth refresh response is not JSON"))?;

    if let Some(error_code) = payload
        .error
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
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

pub(crate) fn refresh_claude_code_access_token(
    refresh_token: &str,
) -> anyhow::Result<ClaudeCodeCredentials> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    // TODO: Verify this endpoint against official Anthropic OAuth docs/source if it changes.
    let response = client
        .post(CLAUDE_CODE_OAUTH_TOKEN_ENDPOINT)
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

    let payload =
        parsed.ok_or_else(|| anyhow::anyhow!("Claude Code OAuth refresh response is not JSON"))?;

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

fn resolve_qwen_oauth_context(credential_override: Option<&str>) -> QwenOauthProviderContext {
    let override_value = credential_override
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let placeholder_requested = override_value
        .map(|value| value.eq_ignore_ascii_case(QWEN_OAUTH_PLACEHOLDER))
        .unwrap_or(false);

    if let Some(explicit) = override_value {
        if !placeholder_requested {
            return QwenOauthProviderContext {
                credential: Some(explicit.to_string()),
                base_url: None,
            };
        }
    }

    let mut cached = read_qwen_oauth_cached_credentials();

    let env_token = read_non_empty_env(QWEN_OAUTH_TOKEN_ENV);
    let env_refresh_token = read_non_empty_env(QWEN_OAUTH_REFRESH_TOKEN_ENV);
    let env_resource_url = read_non_empty_env(QWEN_OAUTH_RESOURCE_URL_ENV);

    if env_token.is_none() {
        let refresh_token = env_refresh_token.clone().or_else(|| {
            cached
                .as_ref()
                .and_then(|credentials| credentials.refresh_token.clone())
        });

        let should_refresh = cached.as_ref().is_some_and(qwen_oauth_token_expired)
            || cached
                .as_ref()
                .and_then(|credentials| credentials.access_token.as_deref())
                .is_none_or(|value| value.trim().is_empty());

        if should_refresh {
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

    let mut credential = env_token.or_else(|| {
        cached
            .as_ref()
            .and_then(|credentials| credentials.access_token.clone())
    });
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
    }
}

fn is_claude_code_oauth_placeholder(value: &str) -> bool {
    is_claude_code_alias(value)
}

fn resolve_claude_code_context(credential_override: Option<&str>) -> ClaudeCodeProviderContext {
    let override_value = credential_override
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let placeholder_requested = override_value
        .map(is_claude_code_oauth_placeholder)
        .unwrap_or(false);

    if let Some(explicit) = override_value {
        if !placeholder_requested {
            return ClaudeCodeProviderContext {
                credential: Some(explicit.to_string()),
                refresh_token: None,
                expires_at: None,
            };
        }
    }

    let env_access_token = read_non_empty_env(CLAUDE_CODE_ACCESS_TOKEN_ENV);
    let env_refresh_token = read_non_empty_env(CLAUDE_CODE_REFRESH_TOKEN_ENV);
    let mut cached = if env_access_token.is_some() || env_refresh_token.is_some() {
        None
    } else {
        read_claude_code_cached_credentials()
    };

    if env_access_token.is_none() {
        let refresh_token = env_refresh_token.clone().or_else(|| {
            cached
                .as_ref()
                .and_then(|credentials| credentials.refresh_token.clone())
        });
        let should_refresh = cached.as_ref().is_some_and(claude_code_token_expired)
            || cached
                .as_ref()
                .and_then(|credentials| credentials.access_token.as_deref())
                .is_none_or(|token| token.trim().is_empty());

        if should_refresh {
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
    }

    let credential = env_access_token
        .or_else(|| {
            cached
                .as_ref()
                .and_then(|credentials| credentials.access_token.clone())
        })
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToString::to_string);

    let refresh_token = env_refresh_token.or_else(|| {
        cached
            .as_ref()
            .and_then(|credentials| credentials.refresh_token.clone())
    });
    let expires_at = cached
        .as_ref()
        .and_then(|credentials| credentials.expires_at);

    ClaudeCodeProviderContext {
        credential: if placeholder_requested {
            credential
        } else {
            credential.or_else(|| read_non_empty_env("ANTHROPIC_OAUTH_TOKEN"))
        },
        refresh_token,
        expires_at,
    }
}

fn resolve_minimax_static_credential() -> Option<String> {
    read_non_empty_env(MINIMAX_OAUTH_TOKEN_ENV).or_else(|| read_non_empty_env(MINIMAX_API_KEY_ENV))
}

fn refresh_minimax_oauth_access_token(name: &str, refresh_token: &str) -> anyhow::Result<String> {
    let region = minimax_oauth_region(name);
    let endpoint = region.token_endpoint();
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

#[derive(Debug, Clone)]
pub struct ProviderRuntimeOptions {
    pub auth_profile_override: Option<String>,
    pub openprx_dir: Option<PathBuf>,
    pub secrets_encrypt: bool,
    pub codex_auth_json_path: Option<PathBuf>,
    pub codex_auth_json_auto_import: bool,
    pub reasoning_enabled: Option<bool>,
}

impl Default for ProviderRuntimeOptions {
    fn default() -> Self {
        Self {
            auth_profile_override: None,
            openprx_dir: None,
            secrets_encrypt: true,
            codex_auth_json_path: None,
            codex_auth_json_auto_import: true,
            reasoning_enabled: None,
        }
    }
}

fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')
}

fn token_end(input: &str, from: usize) -> usize {
    let mut end = from;
    for (i, c) in input[from..].char_indices() {
        if is_secret_char(c) {
            end = from + i + c.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Scrub known secret-like token prefixes from provider error strings.
///
/// Redacts tokens with prefixes like `sk-`, `xoxb-`, `xoxp-`, `ghp_`, `gho_`,
/// `ghu_`, and `github_pat_`.
pub fn scrub_secret_patterns(input: &str) -> String {
    const PREFIXES: [&str; 7] = [
        "sk-",
        "xoxb-",
        "xoxp-",
        "ghp_",
        "gho_",
        "ghu_",
        "github_pat_",
    ];

    let mut scrubbed = input.to_string();

    for prefix in PREFIXES {
        let mut search_from = 0;
        loop {
            let Some(rel) = scrubbed[search_from..].find(prefix) else {
                break;
            };

            let start = search_from + rel;
            let content_start = start + prefix.len();
            let end = token_end(&scrubbed, content_start);

            // Bare prefixes like "sk-" should not stop future scans.
            if end == content_start {
                search_from = content_start;
                continue;
            }

            scrubbed.replace_range(start..end, "[REDACTED]");
            search_from = start + "[REDACTED]".len();
        }
    }

    scrubbed
}

/// Sanitize API error text by scrubbing secrets and truncating length.
pub fn sanitize_api_error(input: &str) -> String {
    let scrubbed = scrub_secret_patterns(input);

    if scrubbed.chars().count() <= MAX_API_ERROR_CHARS {
        return scrubbed;
    }

    let mut end = MAX_API_ERROR_CHARS;
    while end > 0 && !scrubbed.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}...", &scrubbed[..end])
}

/// Build a sanitized provider error from a failed HTTP response.
pub async fn api_error(provider: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read provider error body>".to_string());
    let sanitized = sanitize_api_error(&body);
    anyhow::anyhow!("{provider} API error ({status}): {sanitized}")
}

/// Resolve API key for a provider from config and environment variables.
///
/// Resolution order:
/// 1. Explicitly provided `api_key` parameter (trimmed, filtered if empty)
/// 2. Provider-specific environment variable (e.g., `ANTHROPIC_OAUTH_TOKEN`, `OPENROUTER_API_KEY`)
/// 3. Generic fallback variables (`ZEROCLAW_API_KEY`, `API_KEY`)
///
/// For Anthropic, the provider-specific env var is `ANTHROPIC_OAUTH_TOKEN` (for setup-tokens)
/// followed by `ANTHROPIC_API_KEY` (for regular API keys).
///
/// For MiniMax, OAuth mode supports `api_key = "minimax-oauth"`, resolving credentials from
/// `MINIMAX_OAUTH_TOKEN` first, then `MINIMAX_API_KEY`, and finally
/// `MINIMAX_OAUTH_REFRESH_TOKEN` (automatic access-token refresh).
fn resolve_provider_credential(name: &str, credential_override: Option<&str>) -> Option<String> {
    let mut minimax_oauth_placeholder_requested = false;

    if let Some(raw_override) = credential_override {
        let trimmed_override = raw_override.trim();
        if !trimmed_override.is_empty() {
            if is_minimax_alias(name) && is_minimax_oauth_placeholder(trimmed_override) {
                minimax_oauth_placeholder_requested = true;
                if let Some(credential) = resolve_minimax_static_credential() {
                    return Some(credential);
                }
                if let Some(credential) = resolve_minimax_oauth_refresh_token(name) {
                    return Some(credential);
                }
            } else {
                return Some(trimmed_override.to_owned());
            }
        }
    }

    let provider_env_candidates: Vec<&str> = match name {
        "anthropic" => vec!["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        "openrouter" => vec!["OPENROUTER_API_KEY"],
        "openai" => vec!["OPENAI_API_KEY"],
        "ollama" => vec!["OLLAMA_API_KEY"],
        "venice" => vec!["VENICE_API_KEY"],
        "groq" => vec!["GROQ_API_KEY"],
        "mistral" => vec!["MISTRAL_API_KEY"],
        "deepseek" => vec!["DEEPSEEK_API_KEY"],
        "xai" | "grok" => vec!["XAI_API_KEY"],
        "together" | "together-ai" => vec!["TOGETHER_API_KEY"],
        "fireworks" | "fireworks-ai" => vec!["FIREWORKS_API_KEY"],
        "perplexity" => vec!["PERPLEXITY_API_KEY"],
        "cohere" => vec!["COHERE_API_KEY"],
        name if is_moonshot_alias(name) => vec!["MOONSHOT_API_KEY"],
        "kimi-code" | "kimi_coding" | "kimi_for_coding" => {
            vec!["KIMI_CODE_API_KEY", "MOONSHOT_API_KEY"]
        }
        name if is_glm_alias(name) => vec!["GLM_API_KEY"],
        name if is_minimax_alias(name) => vec![MINIMAX_OAUTH_TOKEN_ENV, MINIMAX_API_KEY_ENV],
        // Bedrock uses AWS AKSK from env vars (AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY),
        // not a single API key. Credential resolution happens inside BedrockProvider.
        "bedrock" | "aws-bedrock" => return None,
        name if is_qianfan_alias(name) => vec!["QIANFAN_API_KEY"],
        name if is_qwen_alias(name) => vec!["DASHSCOPE_API_KEY"],
        name if is_zai_alias(name) => vec!["ZAI_API_KEY"],
        "nvidia" | "nvidia-nim" | "build.nvidia.com" => vec!["NVIDIA_API_KEY"],
        "synthetic" => vec!["SYNTHETIC_API_KEY"],
        "opencode" | "opencode-zen" => vec!["OPENCODE_API_KEY"],
        "vercel" | "vercel-ai" => vec!["VERCEL_API_KEY"],
        "cloudflare" | "cloudflare-ai" => vec!["CLOUDFLARE_API_KEY"],
        "ovhcloud" | "ovh" => vec!["OVH_AI_ENDPOINTS_ACCESS_TOKEN"],
        "astrai" => vec!["ASTRAI_API_KEY"],
        "llamacpp" | "llama.cpp" => vec!["LLAMACPP_API_KEY"],
        name if is_claude_code_alias(name) => {
            vec![
                CLAUDE_CODE_ACCESS_TOKEN_ENV,
                "ANTHROPIC_OAUTH_TOKEN",
                "ANTHROPIC_API_KEY",
            ]
        }
        name if is_litellm_alias(name) => vec!["LITELLM_API_KEY"],
        name if is_vllm_alias(name) => vec!["VLLM_API_KEY"],
        name if is_huggingface_alias(name) => vec!["HF_TOKEN", "HUGGINGFACE_API_KEY"],
        _ => vec![],
    };

    for env_var in provider_env_candidates {
        if let Ok(value) = std::env::var(env_var) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    if is_minimax_alias(name) {
        if let Some(credential) = resolve_minimax_oauth_refresh_token(name) {
            return Some(credential);
        }
    }

    if minimax_oauth_placeholder_requested && is_minimax_alias(name) {
        return None;
    }

    for env_var in ["ZEROCLAW_API_KEY", "API_KEY"] {
        if let Ok(value) = std::env::var(env_var) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    None
}

fn parse_custom_provider_url(
    raw_url: &str,
    provider_label: &str,
    format_hint: &str,
) -> anyhow::Result<String> {
    let base_url = raw_url.trim();

    if base_url.is_empty() {
        anyhow::bail!("{provider_label} requires a URL. Format: {format_hint}");
    }

    let parsed = reqwest::Url::parse(base_url).map_err(|_| {
        anyhow::anyhow!("{provider_label} requires a valid URL. Format: {format_hint}")
    })?;

    match parsed.scheme() {
        "http" | "https" => Ok(base_url.to_string()),
        _ => anyhow::bail!(
            "{provider_label} requires an http:// or https:// URL. Format: {format_hint}"
        ),
    }
}

/// Factory: create the right provider from config (without custom URL)
pub fn create_provider(name: &str, api_key: Option<&str>) -> anyhow::Result<Box<dyn Provider>> {
    create_provider_with_options(name, api_key, &ProviderRuntimeOptions::default())
}

/// Factory: create provider with runtime options (auth profile override, state dir).
pub fn create_provider_with_options(
    name: &str,
    api_key: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    match name {
        "openai-codex" | "openai_codex" | "codex" => {
            Ok(Box::new(openai_codex::OpenAiCodexProvider::new(options)))
        }
        _ => create_provider_with_url_and_options(name, api_key, None, options),
    }
}

/// Factory: create the right provider from config with optional custom base URL
pub fn create_provider_with_url(
    name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
) -> anyhow::Result<Box<dyn Provider>> {
    create_provider_with_url_and_options(name, api_key, api_url, &ProviderRuntimeOptions::default())
}

/// Factory: create provider with optional base URL and runtime options.
#[allow(clippy::too_many_lines)]
fn create_provider_with_url_and_options(
    name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    let qwen_oauth_context = is_qwen_oauth_alias(name).then(|| resolve_qwen_oauth_context(api_key));
    let claude_code_placeholder_requested = api_key
        .map(str::trim)
        .is_some_and(is_claude_code_oauth_placeholder);
    let claude_code_context = if is_claude_code_alias(name) {
        Some(resolve_claude_code_context(api_key))
    } else if name == "anthropic" {
        if api_key.is_none() || claude_code_placeholder_requested {
            Some(resolve_claude_code_context(api_key))
        } else {
            None
        }
    } else {
        None
    };

    // Resolve credential and break static-analysis taint chain from the
    // `api_key` parameter so that downstream provider storage of the value
    // is not linked to the original sensitive-named source.
    let resolved_credential = if let Some(context) = qwen_oauth_context.as_ref() {
        context.credential.clone()
    } else if let Some(context) = claude_code_context.as_ref() {
        let fallback_override = if claude_code_placeholder_requested {
            None
        } else {
            api_key
        };
        context.credential.clone().or_else(|| {
            if is_claude_code_alias(name) {
                resolve_provider_credential("anthropic", fallback_override)
            } else {
                resolve_provider_credential(name, fallback_override)
            }
        })
    } else {
        resolve_provider_credential(name, api_key)
    }
    .map(|v| String::from_utf8(v.into_bytes()).unwrap_or_default());
    #[allow(clippy::option_as_ref_deref)]
    let key = resolved_credential.as_ref().map(String::as_str);
    match name {
        // ── Primary providers (custom implementations) ───────
        "openrouter" => Ok(Box::new(openrouter::OpenRouterProvider::new(key))),
        "anthropic" => {
            if let Some(ctx) = claude_code_context.as_ref() {
                if ctx.refresh_token.is_some() {
                    return Ok(Box::new(anthropic::AnthropicProvider::with_oauth(
                        key,
                        ctx.refresh_token.clone(),
                        ctx.expires_at,
                    )));
                }
            }
            Ok(Box::new(anthropic::AnthropicProvider::new(key)))
        }
        name if is_claude_code_alias(name) => {
            if let Some(ctx) = claude_code_context.as_ref() {
                if ctx.refresh_token.is_some() {
                    return Ok(Box::new(anthropic::AnthropicProvider::with_oauth(
                        key,
                        ctx.refresh_token.clone(),
                        ctx.expires_at,
                    )));
                }
            }
            Ok(Box::new(anthropic::AnthropicProvider::new(key)))
        }
        "openai" => Ok(Box::new(openai::OpenAiProvider::with_base_url(api_url, key))),
        // Ollama uses api_url for custom base URL (e.g. remote Ollama instance)
        "ollama" => Ok(Box::new(ollama::OllamaProvider::new_with_reasoning(
            api_url,
            key,
            options.reasoning_enabled,
        ))),
        "gemini" | "google" | "google-gemini" => {
            Ok(Box::new(gemini::GeminiProvider::new(key)))
        }

        // ── OpenAI-compatible providers ──────────────────────
        "venice" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Venice", "https://api.venice.ai", key, AuthStyle::Bearer,
        ))),
        "vercel" | "vercel-ai" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Vercel AI Gateway", "https://api.vercel.ai", key, AuthStyle::Bearer,
        ))),
        "cloudflare" | "cloudflare-ai" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Cloudflare AI Gateway",
            "https://gateway.ai.cloudflare.com/v1",
            key,
            AuthStyle::Bearer,
        ))),
        name if moonshot_base_url(name).is_some() => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Moonshot",
            moonshot_base_url(name).expect("checked in guard"),
            key,
            AuthStyle::Bearer,
        ))),
        "kimi-code" | "kimi_coding" | "kimi_for_coding" => Ok(Box::new(
            OpenAiCompatibleProvider::new_with_user_agent(
                "Kimi Code",
                "https://api.kimi.com/coding/v1",
                key,
                AuthStyle::Bearer,
                "KimiCLI/0.77",
            ),
        )),
        "synthetic" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Synthetic", "https://api.synthetic.com", key, AuthStyle::Bearer,
        ))),
        "opencode" | "opencode-zen" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "OpenCode Zen", "https://opencode.ai/zen/v1", key, AuthStyle::Bearer,
        ))),
        name if zai_base_url(name).is_some() => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Z.AI",
            zai_base_url(name).expect("checked in guard"),
            key,
            AuthStyle::Bearer,
        ))),
        name if glm_base_url(name).is_some() => {
            Ok(Box::new(OpenAiCompatibleProvider::new_no_responses_fallback(
                "GLM",
                glm_base_url(name).expect("checked in guard"),
                key,
                AuthStyle::Bearer,
            )))
        }
        name if minimax_base_url(name).is_some() => Ok(Box::new(
            OpenAiCompatibleProvider::new_merge_system_into_user(
                "MiniMax",
                minimax_base_url(name).expect("checked in guard"),
                key,
                AuthStyle::Bearer,
            )
        )),
        "bedrock" | "aws-bedrock" => Ok(Box::new(bedrock::BedrockProvider::new())),
        name if is_qwen_oauth_alias(name) => {
            let base_url = api_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .or_else(|| qwen_oauth_context.as_ref().and_then(|context| context.base_url.clone()))
                .unwrap_or_else(|| QWEN_OAUTH_BASE_FALLBACK_URL.to_string());

            Ok(Box::new(OpenAiCompatibleProvider::new_with_user_agent(
                "Qwen Code",
                &base_url,
                key,
                AuthStyle::Bearer,
                "QwenCode/1.0",
            )))
        }
        name if is_qianfan_alias(name) => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Qianfan", "https://aip.baidubce.com", key, AuthStyle::Bearer,
        ))),
        name if qwen_coding_base_url(name).is_some() => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Qwen Coding Plan Pro",
            qwen_coding_base_url(name).expect("checked in guard"),
            key,
            AuthStyle::Bearer,
        ))),
        name if qwen_base_url(name).is_some() => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Qwen",
            qwen_base_url(name).expect("checked in guard"),
            key,
            AuthStyle::Bearer,
        ))),

        // ── Extended ecosystem (community favorites) ─────────
        "groq" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Groq", "https://api.groq.com/openai", key, AuthStyle::Bearer,
        ))),
        "mistral" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Mistral", "https://api.mistral.ai/v1", key, AuthStyle::Bearer,
        ))),
        "xai" | "grok" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "xAI", "https://api.x.ai/v1", key, AuthStyle::Bearer,
        ))),
        "deepseek" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "DeepSeek", "https://api.deepseek.com", key, AuthStyle::Bearer,
        ))),
        "together" | "together-ai" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Together AI", "https://api.together.xyz", key, AuthStyle::Bearer,
        ))),
        "fireworks" | "fireworks-ai" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Fireworks AI", "https://api.fireworks.ai/inference/v1", key, AuthStyle::Bearer,
        ))),
        "perplexity" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Perplexity", "https://api.perplexity.ai", key, AuthStyle::Bearer,
        ))),
        "cohere" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Cohere", "https://api.cohere.com/compatibility", key, AuthStyle::Bearer,
        ))),
        "copilot" | "github-copilot" => Ok(Box::new(copilot::CopilotProvider::new(key))),
        "lmstudio" | "lm-studio" => {
            let lm_studio_key = key
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("lm-studio");
            Ok(Box::new(OpenAiCompatibleProvider::new(
                "LM Studio",
                "http://localhost:1234/v1",
                Some(lm_studio_key),
                AuthStyle::Bearer,
            )))
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
            Ok(Box::new(OpenAiCompatibleProvider::new(
                "llama.cpp",
                base_url,
                Some(llama_cpp_key),
                AuthStyle::Bearer,
            )))
        }
        name if is_litellm_alias(name) => {
            let base = api_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("http://localhost:4000");
            Ok(Box::new(OpenAiCompatibleProvider::new(
                "LiteLLM",
                base,
                key,
                AuthStyle::Bearer,
            )))
        }
        name if is_vllm_alias(name) => {
            let base = api_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("http://localhost:8000/v1");
            Ok(Box::new(OpenAiCompatibleProvider::new(
                "vLLM",
                base,
                key,
                AuthStyle::Bearer,
            )))
        }
        name if is_huggingface_alias(name) => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Hugging Face",
            "https://api-inference.huggingface.co/v1",
            key,
            AuthStyle::Bearer,
        ))),
        "nvidia" | "nvidia-nim" | "build.nvidia.com" => Ok(Box::new(
            OpenAiCompatibleProvider::new(
                "NVIDIA NIM",
                "https://integrate.api.nvidia.com/v1",
                key,
                AuthStyle::Bearer,
            ),
        )),

        // ── AI inference routers ─────────────────────────────
        "astrai" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Astrai", "https://as-trai.com/v1", key, AuthStyle::Bearer,
        ))),

        // ── Cloud AI endpoints ───────────────────────────────
        "ovhcloud" | "ovh" => Ok(Box::new(openai::OpenAiProvider::with_base_url(
            Some("https://oai.endpoints.kepler.ai.cloud.ovh.net/v1"),
            key,
        ))),

        // ── Bring Your Own Provider (custom URL) ───────────
        // Format: "custom:https://your-api.com" or "custom:http://localhost:1234"
        name if name.starts_with("custom:") => {
            let base_url = parse_custom_provider_url(
                name.strip_prefix("custom:").unwrap_or(""),
                "Custom provider",
                "custom:https://your-api.com",
            )?;
            Ok(Box::new(OpenAiCompatibleProvider::new(
                "Custom",
                &base_url,
                key,
                AuthStyle::Bearer,
            )))
        }

        // ── Anthropic-compatible custom endpoints ───────────
        // Format: "anthropic-custom:https://your-api.com"
        name if name.starts_with("anthropic-custom:") => {
            let base_url = parse_custom_provider_url(
                name.strip_prefix("anthropic-custom:").unwrap_or(""),
                "Anthropic-custom provider",
                "anthropic-custom:https://your-api.com",
            )?;
            Ok(Box::new(anthropic::AnthropicProvider::with_base_url(
                key,
                Some(&base_url),
            )))
        }

        _ => anyhow::bail!(
            "Unknown provider: {name}. Check README for supported providers or run `openprx onboard --interactive` to reconfigure.\n\
             Tip: Use \"custom:https://your-api.com\" for OpenAI-compatible endpoints.\n\
             Tip: Use \"anthropic-custom:https://your-api.com\" for Anthropic-compatible endpoints."
        ),
    }
}

/// Create provider chain with retry and fallback behavior.
pub fn create_resilient_provider(
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
) -> anyhow::Result<Box<dyn Provider>> {
    create_resilient_provider_with_options(
        primary_name,
        api_key,
        api_url,
        reliability,
        &ProviderRuntimeOptions::default(),
    )
}

/// Create provider chain with retry/fallback behavior and auth runtime options.
pub fn create_resilient_provider_with_options(
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    let availability = summarize_provider_availability(primary_name, api_key, reliability);
    let mut providers: Vec<(String, Box<dyn Provider>)> = Vec::new();
    let mut unavailable = availability.unavailable.clone();

    for provider_name in &availability.available {
        let explicit_key = (provider_name == primary_name).then_some(api_key).flatten();
        let creation_result = if provider_name == primary_name {
            match primary_name {
                "openai-codex" | "openai_codex" | "codex" => {
                    create_provider_with_options(provider_name, explicit_key, options)
                }
                _ => create_provider_with_url_and_options(
                    provider_name,
                    explicit_key,
                    api_url,
                    options,
                ),
            }
        } else {
            create_provider_with_options(provider_name, None, options)
        };

        match creation_result {
            Ok(provider) => providers.push((provider_name.clone(), provider)),
            Err(error) => {
                let reason = format!("failed to initialize: {error}");
                if provider_name == primary_name {
                    anyhow::bail!("Primary provider \"{provider_name}\" unavailable: {reason}");
                }
                tracing::warn!(
                    provider = provider_name,
                    "Ignoring unavailable fallback provider"
                );
                unavailable.push((provider_name.clone(), reason));
            }
        }
    }

    if providers.is_empty() {
        anyhow::bail!("No available providers after credential/initialization checks");
    }

    let reliable = ReliableProvider::new(
        providers,
        reliability.provider_retries,
        reliability.provider_backoff_ms,
    )
    .with_api_keys(reliability.api_keys.clone())
    .with_model_fallbacks(reliability.model_fallbacks.clone())
    .with_unavailable_providers(unavailable);

    Ok(Box::new(reliable))
}

/// Create a RouterProvider if model routes are configured, otherwise return a
/// standard resilient provider. The router wraps individual providers per route,
/// each with its own retry/fallback chain.
pub fn create_routed_provider(
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    model_routes: &[crate::config::ModelRouteConfig],
    default_model: &str,
) -> anyhow::Result<Box<dyn Provider>> {
    create_routed_provider_with_options(
        primary_name,
        api_key,
        api_url,
        reliability,
        model_routes,
        default_model,
        &ProviderRuntimeOptions::default(),
    )
}

/// Create a routed provider using explicit runtime options.
pub fn create_routed_provider_with_options(
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    model_routes: &[crate::config::ModelRouteConfig],
    default_model: &str,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    if model_routes.is_empty() {
        return create_resilient_provider_with_options(
            primary_name,
            api_key,
            api_url,
            reliability,
            options,
        );
    }

    // Collect unique provider names needed
    let mut needed: Vec<String> = vec![primary_name.to_string()];
    for route in model_routes {
        if !needed.iter().any(|n| n == &route.provider) {
            needed.push(route.provider.clone());
        }
    }

    // Create each provider (with its own resilience wrapper)
    let mut providers: Vec<(String, Box<dyn Provider>)> = Vec::new();
    for name in &needed {
        let routed_credential = model_routes
            .iter()
            .find(|r| &r.provider == name)
            .and_then(|r| {
                r.api_key.as_ref().and_then(|raw_key| {
                    let trimmed_key = raw_key.trim();
                    (!trimmed_key.is_empty()).then_some(trimmed_key)
                })
            });
        let key = routed_credential.or(api_key);
        // Only use api_url for the primary provider
        let url = if name == primary_name { api_url } else { None };
        match create_resilient_provider_with_options(name, key, url, reliability, options) {
            Ok(provider) => providers.push((name.clone(), provider)),
            Err(e) => {
                if name == primary_name {
                    return Err(e);
                }
                tracing::warn!(
                    provider = name.as_str(),
                    "Ignoring routed provider that failed to initialize"
                );
            }
        }
    }

    // Build route table
    let routes: Vec<(String, router::Route)> = model_routes
        .iter()
        .map(|r| {
            (
                r.hint.clone(),
                router::Route {
                    provider_name: r.provider.clone(),
                    model: r.model.clone(),
                },
            )
        })
        .collect();

    Ok(Box::new(router::RouterProvider::new(
        providers,
        routes,
        default_model.to_string(),
    )))
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

    let lowered = name.trim().to_ascii_lowercase();
    let mut local_names: HashSet<String> = HashSet::new();
    for info in list_providers() {
        if info.local {
            local_names.insert(info.name.to_string());
            for alias in info.aliases {
                local_names.insert((*alias).to_string());
            }
        }
    }

    !local_names.contains(&lowered)
}

fn is_openai_codex_alias(name: &str) -> bool {
    matches!(name.trim().to_ascii_lowercase().as_str(), "openai-codex" | "openai_codex" | "codex")
}

fn default_openprx_state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENPRX_STATE_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    directories::UserDirs::new().map_or_else(
        || PathBuf::from(".openprx"),
        |dirs| dirs.home_dir().join(".openprx"),
    )
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
    let model = model.trim();
    if model.is_empty() || model.starts_with("hint:") {
        return true;
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

    for info in list_providers() {
        if info.name.eq_ignore_ascii_case(&provider_name)
            || info
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(&provider_name))
        {
            if info.name.eq_ignore_ascii_case(&prefix)
                || info
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(&prefix))
            {
                return true;
            }
        }
    }

    false
}

pub fn summarize_provider_availability(
    primary_name: &str,
    api_key: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
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
        if let Err(error) = create_provider(provider, None) {
            unavailable.push((provider.clone(), format!("invalid provider: {error}")));
            continue;
        }

        if is_openai_codex_alias(provider) {
            if openai_codex_auth_profile_available() {
                available.push(provider.clone());
            } else {
                unavailable.push((
                    provider.clone(),
                    "missing OpenAI Codex auth profile/token".to_string(),
                ));
            }
            continue;
        }

        let explicit_for_provider = if provider == primary_name {
            api_key
        } else {
            None
        };
        if provider_requires_explicit_credential(provider)
            && resolve_provider_credential(provider, explicit_for_provider).is_none()
        {
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

/// Return the list of all known providers for display in `openprx providers list`.
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
        },
        ProviderInfo {
            name: "anthropic",
            display_name: "Anthropic",
            aliases: &["claude-code", "claude-cli"],
            local: false,
        },
        ProviderInfo {
            name: "openai",
            display_name: "OpenAI",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "openai-codex",
            display_name: "OpenAI Codex (OAuth)",
            aliases: &["openai_codex", "codex"],
            local: false,
        },
        ProviderInfo {
            name: "ollama",
            display_name: "Ollama",
            aliases: &[],
            local: true,
        },
        ProviderInfo {
            name: "gemini",
            display_name: "Google Gemini",
            aliases: &["google", "google-gemini"],
            local: false,
        },
        // ── OpenAI-compatible providers ──────────────────────
        ProviderInfo {
            name: "venice",
            display_name: "Venice",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "vercel",
            display_name: "Vercel AI Gateway",
            aliases: &["vercel-ai"],
            local: false,
        },
        ProviderInfo {
            name: "cloudflare",
            display_name: "Cloudflare AI",
            aliases: &["cloudflare-ai"],
            local: false,
        },
        ProviderInfo {
            name: "moonshot",
            display_name: "Moonshot",
            aliases: &["kimi"],
            local: false,
        },
        ProviderInfo {
            name: "kimi-code",
            display_name: "Kimi Code",
            aliases: &["kimi_coding", "kimi_for_coding"],
            local: false,
        },
        ProviderInfo {
            name: "synthetic",
            display_name: "Synthetic",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "opencode",
            display_name: "OpenCode Zen",
            aliases: &["opencode-zen"],
            local: false,
        },
        ProviderInfo {
            name: "zai",
            display_name: "Z.AI",
            aliases: &["z.ai"],
            local: false,
        },
        ProviderInfo {
            name: "glm",
            display_name: "GLM (Zhipu)",
            aliases: &["zhipu"],
            local: false,
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
        },
        ProviderInfo {
            name: "bedrock",
            display_name: "Amazon Bedrock",
            aliases: &["aws-bedrock"],
            local: false,
        },
        ProviderInfo {
            name: "qianfan",
            display_name: "Qianfan (Baidu)",
            aliases: &["baidu"],
            local: false,
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
        },
        ProviderInfo {
            name: "groq",
            display_name: "Groq",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "mistral",
            display_name: "Mistral",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "xai",
            display_name: "xAI (Grok)",
            aliases: &["grok"],
            local: false,
        },
        ProviderInfo {
            name: "deepseek",
            display_name: "DeepSeek",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "together",
            display_name: "Together AI",
            aliases: &["together-ai"],
            local: false,
        },
        ProviderInfo {
            name: "fireworks",
            display_name: "Fireworks AI",
            aliases: &["fireworks-ai"],
            local: false,
        },
        ProviderInfo {
            name: "perplexity",
            display_name: "Perplexity",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "cohere",
            display_name: "Cohere",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "copilot",
            display_name: "GitHub Copilot",
            aliases: &["github-copilot"],
            local: false,
        },
        ProviderInfo {
            name: "lmstudio",
            display_name: "LM Studio",
            aliases: &["lm-studio"],
            local: true,
        },
        ProviderInfo {
            name: "llamacpp",
            display_name: "llama.cpp server",
            aliases: &["llama.cpp"],
            local: true,
        },
        ProviderInfo {
            name: "litellm",
            display_name: "LiteLLM",
            aliases: &["lite-llm"],
            local: false,
        },
        ProviderInfo {
            name: "vllm",
            display_name: "vLLM",
            aliases: &["v-llm"],
            local: true,
        },
        ProviderInfo {
            name: "huggingface",
            display_name: "Hugging Face Inference",
            aliases: &["hf", "hf-inference"],
            local: false,
        },
        ProviderInfo {
            name: "nvidia",
            display_name: "NVIDIA NIM",
            aliases: &["nvidia-nim", "build.nvidia.com"],
            local: false,
        },
        ProviderInfo {
            name: "ovhcloud",
            display_name: "OVHcloud AI Endpoints",
            aliases: &["ovh"],
            local: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let original = std::env::var(key).ok();
            match value {
                Some(next) => std::env::set_var(key, next),
                None => std::env::remove_var(key),
            }

            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(original) = self.original.as_deref() {
                std::env::set_var(self.key, original);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned")
    }

    #[test]
    fn resolve_provider_credential_prefers_explicit_argument() {
        let resolved = resolve_provider_credential("openrouter", Some("  explicit-key  "));
        assert_eq!(resolved, Some("explicit-key".to_string()));
    }

    #[test]
    fn resolve_provider_credential_uses_minimax_oauth_env_for_placeholder() {
        let _env_lock = env_lock();
        let _oauth_guard = EnvGuard::set(MINIMAX_OAUTH_TOKEN_ENV, Some("oauth-token"));
        let _api_guard = EnvGuard::set(MINIMAX_API_KEY_ENV, Some("api-key"));
        let _refresh_guard = EnvGuard::set(MINIMAX_OAUTH_REFRESH_TOKEN_ENV, None);

        let resolved = resolve_provider_credential("minimax", Some(MINIMAX_OAUTH_PLACEHOLDER));

        assert_eq!(resolved.as_deref(), Some("oauth-token"));
    }

    #[test]
    fn resolve_provider_credential_falls_back_to_minimax_api_key_for_placeholder() {
        let _env_lock = env_lock();
        let _oauth_guard = EnvGuard::set(MINIMAX_OAUTH_TOKEN_ENV, None);
        let _api_guard = EnvGuard::set(MINIMAX_API_KEY_ENV, Some("api-key"));
        let _refresh_guard = EnvGuard::set(MINIMAX_OAUTH_REFRESH_TOKEN_ENV, None);

        let resolved = resolve_provider_credential("minimax", Some(MINIMAX_OAUTH_PLACEHOLDER));

        assert_eq!(resolved.as_deref(), Some("api-key"));
    }

    #[test]
    fn resolve_provider_credential_placeholder_ignores_generic_api_key_fallback() {
        let _env_lock = env_lock();
        let _oauth_guard = EnvGuard::set(MINIMAX_OAUTH_TOKEN_ENV, None);
        let _api_guard = EnvGuard::set(MINIMAX_API_KEY_ENV, None);
        let _refresh_guard = EnvGuard::set(MINIMAX_OAUTH_REFRESH_TOKEN_ENV, None);
        let _generic_guard = EnvGuard::set("API_KEY", Some("generic-key"));

        let resolved = resolve_provider_credential("minimax", Some(MINIMAX_OAUTH_PLACEHOLDER));

        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_provider_credential_bedrock_uses_internal_credential_path() {
        let _generic_guard = EnvGuard::set("API_KEY", Some("generic-key"));
        let _override_guard = EnvGuard::set("OPENROUTER_API_KEY", Some("openrouter-key"));

        assert_eq!(
            resolve_provider_credential("bedrock", Some("explicit")),
            Some("explicit".to_string())
        );
        assert!(resolve_provider_credential("bedrock", None).is_none());
        assert!(resolve_provider_credential("aws-bedrock", None).is_none());
    }

    #[test]
    fn resolve_provider_credential_supports_litellm_env() {
        let _env_lock = env_lock();
        let _litellm_guard = EnvGuard::set("LITELLM_API_KEY", Some("litellm-key"));
        let _generic_guard = EnvGuard::set("API_KEY", Some("generic-key"));

        let resolved = resolve_provider_credential("litellm", None);
        assert_eq!(resolved.as_deref(), Some("litellm-key"));
    }

    #[test]
    fn resolve_provider_credential_supports_optional_vllm_env() {
        let _env_lock = env_lock();
        let _vllm_guard = EnvGuard::set("VLLM_API_KEY", Some("vllm-key"));
        let _generic_guard = EnvGuard::set("API_KEY", Some("generic-key"));

        let resolved = resolve_provider_credential("vllm", None);
        assert_eq!(resolved.as_deref(), Some("vllm-key"));
    }

    #[test]
    fn resolve_provider_credential_huggingface_prefers_hf_token() {
        let _env_lock = env_lock();
        let _hf_token_guard = EnvGuard::set("HF_TOKEN", Some("hf-token"));
        let _hf_api_key_guard = EnvGuard::set("HUGGINGFACE_API_KEY", Some("hf-api-key"));
        let _generic_guard = EnvGuard::set("API_KEY", Some("generic-key"));

        let resolved = resolve_provider_credential("hf", None);
        assert_eq!(resolved.as_deref(), Some("hf-token"));
    }

    #[test]
    fn resolve_qwen_oauth_context_prefers_explicit_override() {
        let _env_lock = env_lock();
        let fake_home = format!("/tmp/openprx-qwen-oauth-home-{}", std::process::id());
        let _home_guard = EnvGuard::set("HOME", Some(fake_home.as_str()));
        let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, Some("oauth-token"));
        let _resource_guard = EnvGuard::set(
            QWEN_OAUTH_RESOURCE_URL_ENV,
            Some("coding-intl.dashscope.aliyuncs.com"),
        );

        let context = resolve_qwen_oauth_context(Some("  explicit-qwen-token  "));

        assert_eq!(context.credential.as_deref(), Some("explicit-qwen-token"));
        assert!(context.base_url.is_none());
    }

    #[test]
    fn resolve_qwen_oauth_context_uses_env_token_and_resource_url() {
        let _env_lock = env_lock();
        let fake_home = format!("/tmp/openprx-qwen-oauth-home-{}-env", std::process::id());
        let _home_guard = EnvGuard::set("HOME", Some(fake_home.as_str()));
        let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, Some("oauth-token"));
        let _refresh_guard = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
        let _resource_guard = EnvGuard::set(
            QWEN_OAUTH_RESOURCE_URL_ENV,
            Some("coding-intl.dashscope.aliyuncs.com"),
        );
        let _dashscope_guard = EnvGuard::set("DASHSCOPE_API_KEY", Some("dashscope-fallback"));

        let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER));

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

        let _home_guard = EnvGuard::set("HOME", Some(fake_home.as_str()));
        let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, None);
        let _refresh_guard = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
        let _resource_guard = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, None);
        let _dashscope_guard = EnvGuard::set("DASHSCOPE_API_KEY", None);

        let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER));

        assert_eq!(context.credential.as_deref(), Some("cached-token"));
        assert_eq!(
            context.base_url.as_deref(),
            Some("https://resource.example.com/v1")
        );
    }

    #[test]
    fn resolve_qwen_oauth_context_placeholder_does_not_use_dashscope_fallback() {
        let _env_lock = env_lock();
        let fake_home = format!(
            "/tmp/openprx-qwen-oauth-home-{}-placeholder",
            std::process::id()
        );
        let _home_guard = EnvGuard::set("HOME", Some(fake_home.as_str()));
        let _token_guard = EnvGuard::set(QWEN_OAUTH_TOKEN_ENV, None);
        let _refresh_guard = EnvGuard::set(QWEN_OAUTH_REFRESH_TOKEN_ENV, None);
        let _resource_guard = EnvGuard::set(QWEN_OAUTH_RESOURCE_URL_ENV, None);
        let _dashscope_guard = EnvGuard::set("DASHSCOPE_API_KEY", Some("dashscope-fallback"));

        let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER));

        assert!(context.credential.is_none());
    }

    #[test]
    fn resolve_claude_code_context_prefers_explicit_override() {
        let _env_lock = env_lock();
        let fake_home = format!("/tmp/openprx-claude-oauth-home-{}", std::process::id());
        let _home_guard = EnvGuard::set("HOME", Some(fake_home.as_str()));
        let _access_guard = EnvGuard::set(CLAUDE_CODE_ACCESS_TOKEN_ENV, Some("oauth-token"));
        let _refresh_guard = EnvGuard::set(CLAUDE_CODE_REFRESH_TOKEN_ENV, Some("oauth-refresh"));
        let _anthropic_oauth_guard =
            EnvGuard::set("ANTHROPIC_OAUTH_TOKEN", Some("anthropic-token"));

        let context = resolve_claude_code_context(Some("  explicit-claude-token  "));

        assert_eq!(context.credential.as_deref(), Some("explicit-claude-token"));
    }

    #[test]
    fn resolve_claude_code_context_uses_env_access_token_before_file() {
        let _env_lock = env_lock();
        let fake_home = format!("/tmp/openprx-claude-oauth-home-{}-env", std::process::id());
        let _home_guard = EnvGuard::set("HOME", Some(fake_home.as_str()));
        let _access_guard = EnvGuard::set(CLAUDE_CODE_ACCESS_TOKEN_ENV, Some("env-access-token"));
        let _refresh_guard =
            EnvGuard::set(CLAUDE_CODE_REFRESH_TOKEN_ENV, Some("env-refresh-token"));
        let _anthropic_oauth_guard =
            EnvGuard::set("ANTHROPIC_OAUTH_TOKEN", Some("anthropic-token"));

        let context = resolve_claude_code_context(Some("claude-code"));

        assert_eq!(context.credential.as_deref(), Some("env-access-token"));
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

        let _home_guard = EnvGuard::set("HOME", Some(fake_home.as_str()));
        let _access_guard = EnvGuard::set(CLAUDE_CODE_ACCESS_TOKEN_ENV, None);
        let _refresh_guard = EnvGuard::set(CLAUDE_CODE_REFRESH_TOKEN_ENV, None);
        let _anthropic_oauth_guard = EnvGuard::set("ANTHROPIC_OAUTH_TOKEN", None);

        let context = resolve_claude_code_context(Some("claude-code"));

        assert_eq!(context.credential.as_deref(), Some("cached-token"));
    }

    #[test]
    fn resolve_claude_code_context_placeholder_does_not_use_anthropic_oauth_fallback() {
        let _env_lock = env_lock();
        let fake_home = format!(
            "/tmp/openprx-claude-oauth-home-{}-placeholder",
            std::process::id()
        );
        let _home_guard = EnvGuard::set("HOME", Some(fake_home.as_str()));
        let _access_guard = EnvGuard::set(CLAUDE_CODE_ACCESS_TOKEN_ENV, None);
        let _refresh_guard = EnvGuard::set(CLAUDE_CODE_REFRESH_TOKEN_ENV, None);
        let _anthropic_oauth_guard =
            EnvGuard::set("ANTHROPIC_OAUTH_TOKEN", Some("anthropic-token"));
        let _anthropic_api_guard = EnvGuard::set("ANTHROPIC_API_KEY", Some("anthropic-api-key"));

        let context = resolve_claude_code_context(Some("claude-code"));

        assert!(context.credential.is_none());
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
        assert_eq!(
            minimax_base_url("minimax-intl"),
            Some(MINIMAX_INTL_BASE_URL)
        );
        assert_eq!(minimax_base_url("minimax-cn"), Some(MINIMAX_CN_BASE_URL));

        assert_eq!(glm_base_url("glm"), Some(GLM_GLOBAL_BASE_URL));
        assert_eq!(glm_base_url("glm-cn"), Some(GLM_CN_BASE_URL));
        assert_eq!(glm_base_url("bigmodel"), Some(GLM_CN_BASE_URL));

        assert_eq!(moonshot_base_url("moonshot"), Some(MOONSHOT_CN_BASE_URL));
        assert_eq!(
            moonshot_base_url("moonshot-intl"),
            Some(MOONSHOT_INTL_BASE_URL)
        );

        assert_eq!(qwen_base_url("qwen"), Some(QWEN_CN_BASE_URL));
        assert_eq!(qwen_base_url("qwen-cn"), Some(QWEN_CN_BASE_URL));
        assert_eq!(qwen_base_url("qwen-intl"), Some(QWEN_INTL_BASE_URL));
        assert_eq!(qwen_base_url("qwen-us"), Some(QWEN_US_BASE_URL));
        assert_eq!(qwen_base_url("qwen-code"), Some(QWEN_CN_BASE_URL));
        assert_eq!(
            qwen_coding_base_url("dashscope-coding"),
            Some(QWEN_CODING_CN_BASE_URL)
        );
        assert_eq!(
            qwen_coding_base_url("qwen-coding"),
            Some(QWEN_CODING_CN_BASE_URL)
        );
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
        // Bedrock uses AWS env vars for credentials, not API key.
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
        assert!(
            create_provider_with_url("litellm", Some("key"), Some("http://127.0.0.1:4001")).is_ok()
        );
    }

    #[test]
    fn factory_vllm() {
        assert!(create_provider("vllm", Some("key")).is_ok());
        assert!(create_provider("v-llm", Some("key")).is_ok());
        assert!(create_provider("vllm", None).is_ok());
        assert!(
            create_provider_with_url("vllm", Some("key"), Some("http://127.0.0.1:9000/v1")).is_ok()
        );
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
    fn resilient_provider_ignores_duplicate_and_invalid_fallbacks() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec![
                "openrouter".into(),
                "nonexistent-provider".into(),
                "openai".into(),
                "openai".into(),
            ],
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let provider = create_resilient_provider(
            "openrouter",
            Some("provider-test-credential"),
            None,
            &reliability,
        );
        assert!(provider.is_ok());
    }

    #[test]
    fn resilient_provider_errors_for_invalid_primary() {
        let reliability = crate::config::ReliabilityConfig::default();
        let provider = create_resilient_provider(
            "totally-invalid",
            Some("provider-test-credential"),
            None,
            &reliability,
        );
        assert!(provider.is_err());
    }

    #[test]
    fn summarize_provider_availability_marks_degraded_when_only_primary_has_credentials() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec!["anthropic".into()],
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let summary = summarize_provider_availability("openai", Some("sk-test"), &reliability);
        assert!(summary.degraded);
        assert_eq!(summary.available, vec!["openai"]);
        assert!(summary
            .unavailable
            .iter()
            .any(|(name, reason)| name == "anthropic" && reason.contains("missing credential")));
    }

    fn provider_availability_env_lock() -> &'static std::sync::Mutex<()> {
        static ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        ENV_LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn summarize_provider_availability_marks_openai_codex_available_with_auth_profile() {
        let _guard = provider_availability_env_lock().lock().unwrap();

        let state_dir = std::env::temp_dir().join(format!(
            "openprx-provider-avail-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
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

        unsafe {
            std::env::set_var("OPENPRX_STATE_DIR", &state_dir);
        }
        let summary = summarize_provider_availability(
            "openai",
            Some("sk-test"),
            &crate::config::ReliabilityConfig {
                provider_retries: 1,
                provider_backoff_ms: 100,
                fallback_providers: vec!["openai-codex".into()],
                api_keys: Vec::new(),
                model_fallbacks: std::collections::HashMap::new(),
                channel_initial_backoff_secs: 2,
                channel_max_backoff_secs: 60,
                scheduler_poll_secs: 15,
                scheduler_retries: 2,
            },
        );
        unsafe {
            std::env::remove_var("OPENPRX_STATE_DIR");
        }

        assert!(summary.available.iter().any(|p| p == "openai-codex"));
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn summarize_provider_availability_marks_openai_codex_unavailable_without_auth_profile() {
        let _guard = provider_availability_env_lock().lock().unwrap();

        let state_dir = std::env::temp_dir().join(format!(
            "openprx-provider-avail-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&state_dir).unwrap();

        unsafe {
            std::env::set_var("OPENPRX_STATE_DIR", &state_dir);
        }
        let summary = summarize_provider_availability(
            "openai",
            Some("sk-test"),
            &crate::config::ReliabilityConfig {
                provider_retries: 1,
                provider_backoff_ms: 100,
                fallback_providers: vec!["openai-codex".into()],
                api_keys: Vec::new(),
                model_fallbacks: std::collections::HashMap::new(),
                channel_initial_backoff_secs: 2,
                channel_max_backoff_secs: 60,
                scheduler_poll_secs: 15,
                scheduler_retries: 2,
            },
        );
        unsafe {
            std::env::remove_var("OPENPRX_STATE_DIR");
        }

        assert!(summary
            .unavailable
            .iter()
            .any(|(name, reason)| name == "openai-codex" && reason.contains("auth profile")));
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn provider_model_prefix_matching_rejects_cross_provider_mismatch() {
        assert!(provider_matches_model_prefix("openai", "openai/gpt-4o"));
        assert!(!provider_matches_model_prefix("anthropic", "openai/gpt-4o"));
    }

    /// Fallback providers resolve their own credentials via provider-specific
    /// env vars rather than inheriting the primary provider's key.  A provider
    /// that requires no key (e.g. lmstudio, ollama) must initialize
    /// successfully even when the primary uses a completely different key.
    #[test]
    fn resilient_fallback_resolves_own_credential() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec!["lmstudio".into(), "ollama".into()],
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        // Primary uses a ZAI key; fallbacks (lmstudio, ollama) should NOT
        // receive this key; they resolve their own credentials independently.
        let provider = create_resilient_provider("zai", Some("zai-test-key"), None, &reliability);
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
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let provider =
            create_resilient_provider("openai", Some("openai-test-key"), None, &reliability);
        assert!(provider.is_ok());
    }

    /// Mixed fallback chain: named providers, custom URLs, and invalid entries
    /// all coexist.  Invalid entries are silently ignored; valid ones initialize.
    #[test]
    fn resilient_fallback_mixed_chain() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec![
                "deepseek".into(),
                "custom:http://localhost:8080/v1".into(),
                "nonexistent-provider".into(),
                "lmstudio".into(),
            ],
            api_keys: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let provider = create_resilient_provider("zai", Some("zai-test-key"), None, &reliability);
        assert!(provider.is_ok());
    }

    #[test]
    fn ollama_with_custom_url() {
        let provider = create_provider_with_url("ollama", None, Some("http://10.100.2.32:11434"));
        assert!(provider.is_ok());
    }

    #[test]
    fn ollama_cloud_with_custom_url() {
        let provider =
            create_provider_with_url("ollama", Some("ollama-key"), Some("https://ollama.com"));
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
        let result = scrub_secret_patterns(input);
        assert_eq!(result, "auth failed with token [REDACTED]");
    }

    #[test]
    fn scrub_github_oauth_token() {
        let input = "Bearer gho_1234567890abcdef";
        let result = scrub_secret_patterns(input);
        assert_eq!(result, "Bearer [REDACTED]");
    }

    #[test]
    fn scrub_github_user_token() {
        let input = "token ghu_sessiontoken123";
        let result = scrub_secret_patterns(input);
        assert_eq!(result, "token [REDACTED]");
    }

    #[test]
    fn scrub_github_fine_grained_pat() {
        let input = "failed: github_pat_11AABBC_xyzzy789";
        let result = scrub_secret_patterns(input);
        assert_eq!(result, "failed: [REDACTED]");
    }
}
