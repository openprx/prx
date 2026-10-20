//! OAuth credential caches and token refresh for MiniMax, Qwen and Claude Code.

#[cfg(test)]
use super::credential_resolution_observation;
use super::{
    Deserialize, OauthRefresh, PathBuf, QWEN_CN_BASE_URL, Serialize, is_claude_code_alias, is_minimax_cn_alias,
};
pub(super) const MINIMAX_OAUTH_GLOBAL_TOKEN_ENDPOINT: &str = "https://api.minimax.io/oauth/token";
pub(super) const MINIMAX_OAUTH_CN_TOKEN_ENDPOINT: &str = "https://api.minimaxi.com/oauth/token";
pub(super) const MINIMAX_OAUTH_PLACEHOLDER: &str = "minimax-oauth";
pub(super) const MINIMAX_OAUTH_CN_PLACEHOLDER: &str = "minimax-oauth-cn";
pub(super) const MINIMAX_OAUTH_REFRESH_TOKEN_ENV: &str = "MINIMAX_OAUTH_REFRESH_TOKEN";
pub(super) const MINIMAX_OAUTH_REGION_ENV: &str = "MINIMAX_OAUTH_REGION";
pub(super) const MINIMAX_OAUTH_CLIENT_ID_ENV: &str = "MINIMAX_OAUTH_CLIENT_ID";
pub(super) const MINIMAX_OAUTH_DEFAULT_CLIENT_ID: &str = "78257093-7e40-4613-99e0-527b14b39113";
pub(super) const QWEN_OAUTH_BASE_FALLBACK_URL: &str = QWEN_CN_BASE_URL;
pub(super) const QWEN_OAUTH_TOKEN_ENDPOINT: &str = "https://chat.qwen.ai/api/v1/oauth2/token";
pub(super) const QWEN_OAUTH_PLACEHOLDER: &str = "qwen-oauth";
pub(super) const QWEN_OAUTH_TOKEN_ENV: &str = "QWEN_OAUTH_TOKEN";
pub(super) const QWEN_OAUTH_REFRESH_TOKEN_ENV: &str = "QWEN_OAUTH_REFRESH_TOKEN";
pub(super) const QWEN_OAUTH_RESOURCE_URL_ENV: &str = "QWEN_OAUTH_RESOURCE_URL";
pub(super) const QWEN_OAUTH_CLIENT_ID_ENV: &str = "QWEN_OAUTH_CLIENT_ID";
pub(super) const QWEN_OAUTH_DEFAULT_CLIENT_ID: &str = "f0304373b74a44d2b584a3fb70ca9e56";
pub(super) const QWEN_OAUTH_CREDENTIAL_FILE: &str = ".qwen/oauth_creds.json";
pub(super) const CLAUDE_CODE_OAUTH_TOKEN_ENDPOINT: &str = "https://console.anthropic.com/v1/oauth/token";
pub(super) const CLAUDE_CODE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub(super) const CLAUDE_CODE_CREDENTIAL_FILE: &str = ".claude/.credentials.json";

#[derive(Clone, Copy, Debug)]
pub(super) enum MinimaxOauthRegion {
    Global,
    Cn,
}

impl MinimaxOauthRegion {
    pub(super) const fn token_endpoint(self) -> &'static str {
        match self {
            Self::Global => MINIMAX_OAUTH_GLOBAL_TOKEN_ENDPOINT,
            Self::Cn => MINIMAX_OAUTH_CN_TOKEN_ENDPOINT,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct MinimaxOauthRefreshResponse {
    #[serde(default)]
    pub(super) status: Option<String>,
    #[serde(default)]
    pub(super) access_token: Option<String>,
    #[serde(default)]
    pub(super) base_resp: Option<MinimaxOauthBaseResponse>,
}

#[derive(Debug, Deserialize)]
pub(super) struct MinimaxOauthBaseResponse {
    #[serde(default)]
    pub(super) status_msg: Option<String>,
}

#[derive(Clone, Deserialize, Default)]
pub(super) struct QwenOauthCredentials {
    #[serde(default)]
    pub(super) access_token: Option<String>,
    #[serde(default)]
    pub(super) refresh_token: Option<String>,
    #[serde(default)]
    pub(super) resource_url: Option<String>,
    #[serde(default)]
    pub(super) expiry_date: Option<i64>,
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
pub(super) struct QwenOauthTokenResponse {
    #[serde(default)]
    pub(super) access_token: Option<String>,
    #[serde(default)]
    pub(super) refresh_token: Option<String>,
    #[serde(default)]
    pub(super) expires_in: Option<i64>,
    #[serde(default)]
    pub(super) resource_url: Option<String>,
    #[serde(default)]
    pub(super) error: Option<String>,
    #[serde(default)]
    pub(super) error_description: Option<String>,
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
pub(super) struct ClaudeCodeCredentialsFile {
    #[serde(rename = "claudeAiOauth", default)]
    pub(super) claude_ai_oauth: Option<ClaudeCodeCredentials>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ClaudeCodeOauthTokenResponse {
    #[serde(default)]
    pub(super) access_token: Option<String>,
    #[serde(default)]
    pub(super) refresh_token: Option<String>,
    #[serde(default)]
    pub(super) expires_in: Option<i64>,
    #[serde(default)]
    pub(super) error: Option<String>,
    #[serde(default)]
    pub(super) error_description: Option<String>,
}

#[derive(Clone, Default)]
pub(super) struct ClaudeCodeProviderContext {
    pub(super) credential: Option<String>,
    pub(super) refresh_token: Option<String>,
    pub(super) expires_at: Option<i64>,
    /// A refresh was due and [`OauthRefresh::Never`] withheld it.
    pub(super) refresh_withheld: bool,
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
pub(super) struct QwenOauthProviderContext {
    pub(super) credential: Option<String>,
    pub(super) base_url: Option<String>,
    /// A refresh was due and [`OauthRefresh::Never`] withheld it.
    pub(super) refresh_withheld: bool,
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

pub(super) fn read_non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(super) const fn is_minimax_oauth_placeholder(value: &str) -> bool {
    value.eq_ignore_ascii_case(MINIMAX_OAUTH_PLACEHOLDER) || value.eq_ignore_ascii_case(MINIMAX_OAUTH_CN_PLACEHOLDER)
}

pub(super) fn minimax_oauth_region(name: &str) -> MinimaxOauthRegion {
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

pub(super) fn minimax_oauth_client_id() -> String {
    read_non_empty_env(MINIMAX_OAUTH_CLIENT_ID_ENV).unwrap_or_else(|| MINIMAX_OAUTH_DEFAULT_CLIENT_ID.to_string())
}

pub(super) fn qwen_oauth_client_id() -> String {
    read_non_empty_env(QWEN_OAUTH_CLIENT_ID_ENV).unwrap_or_else(|| QWEN_OAUTH_DEFAULT_CLIENT_ID.to_string())
}

pub(super) fn qwen_oauth_credentials_file_path() -> Option<PathBuf> {
    oauth_credentials_home().map(|home| home.join(QWEN_OAUTH_CREDENTIAL_FILE))
}

pub(super) fn claude_code_credentials_file_path() -> Option<PathBuf> {
    oauth_credentials_home().map(|home| home.join(CLAUDE_CODE_CREDENTIAL_FILE))
}

pub(super) fn normalize_qwen_oauth_base_url(raw: &str) -> Option<String> {
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
pub(super) fn is_private_or_loopback_host(host: &str) -> bool {
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

pub(super) fn read_qwen_oauth_cached_credentials() -> Option<QwenOauthCredentials> {
    let path = qwen_oauth_credentials_file_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<QwenOauthCredentials>(&content).ok()
}

pub(super) fn read_claude_code_cached_credentials() -> Option<ClaudeCodeCredentials> {
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

pub(super) const fn normalized_qwen_expiry_millis(raw: i64) -> i64 {
    if raw < 10_000_000_000 {
        raw.saturating_mul(1000)
    } else {
        raw
    }
}

pub(super) fn qwen_oauth_token_expired(credentials: &QwenOauthCredentials) -> bool {
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

pub(super) fn claude_code_token_expired(credentials: &ClaudeCodeCredentials) -> bool {
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

pub(super) fn refresh_qwen_oauth_access_token(refresh_token: &str) -> anyhow::Result<QwenOauthCredentials> {
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
pub(super) enum OauthTokenEndpoint {
    ClaudeCode,
    Qwen,
    Minimax(MinimaxOauthRegion),
}

impl OauthTokenEndpoint {
    pub(super) const fn real(self) -> &'static str {
        match self {
            Self::ClaudeCode => CLAUDE_CODE_OAUTH_TOKEN_ENDPOINT,
            Self::Qwen => QWEN_OAUTH_TOKEN_ENDPOINT,
            Self::Minimax(region) => region.token_endpoint(),
        }
    }
}

/// Where a credential is refreshed.
#[cfg(not(test))]
pub(super) const fn oauth_token_endpoint(endpoint: OauthTokenEndpoint) -> &'static str {
    endpoint.real()
}

/// Where a credential is refreshed in a test build: the local listener a test
/// installed, else a closed loopback port. No test reaches a real token
/// endpoint (D-081).
#[cfg(test)]
pub(super) fn oauth_token_endpoint(endpoint: OauthTokenEndpoint) -> String {
    let installed = OAUTH_TEST_OVERRIDES.read().token_endpoint.clone();
    installed.unwrap_or_else(|| token_host(endpoint.real()))
}

/// The home the Claude Code and Qwen credential files are under.
#[cfg(not(test))]
pub(super) fn oauth_credentials_home() -> Option<PathBuf> {
    crate::config::user_home_dir()
}

/// The home the Claude Code and Qwen credential files are under: a test
/// installs its own, independent of the process-wide test home other modules
/// change concurrently.
#[cfg(test)]
pub(super) fn oauth_credentials_home() -> Option<PathBuf> {
    let installed = OAUTH_TEST_OVERRIDES.read().home.clone();
    installed.or_else(crate::config::user_home_dir)
}

/// What a test installed for the OAuth credential lookups and refreshes.
#[cfg(test)]
#[derive(Default)]
pub(super) struct OauthTestOverrides {
    pub(super) home: Option<PathBuf>,
    pub(super) token_endpoint: Option<String>,
}

#[cfg(test)]
pub(super) static OAUTH_TEST_OVERRIDES: parking_lot::RwLock<OauthTestOverrides> =
    parking_lot::RwLock::new(OauthTestOverrides {
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

pub(super) fn resolve_qwen_oauth_context(
    credential_override: Option<&str>,
    refresh: OauthRefresh,
) -> QwenOauthProviderContext {
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

pub(super) fn is_claude_code_oauth_placeholder(value: &str) -> bool {
    is_claude_code_alias(value)
}

pub(super) fn resolve_claude_code_context(
    credential_override: Option<&str>,
    refresh: OauthRefresh,
) -> ClaudeCodeProviderContext {
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

pub(super) fn refresh_minimax_oauth_access_token(name: &str, refresh_token: &str) -> anyhow::Result<String> {
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

pub(super) fn resolve_minimax_oauth_refresh_token(name: &str) -> Option<String> {
    let refresh_token = read_non_empty_env(MINIMAX_OAUTH_REFRESH_TOKEN_ENV)?;

    match refresh_minimax_oauth_access_token(name, &refresh_token) {
        Ok(token) => Some(token),
        Err(error) => {
            tracing::warn!(provider = name, error = %error, "MiniMax OAuth refresh failed");
            None
        }
    }
}
