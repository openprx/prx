//! OAuth credential caches and token refresh for MiniMax, Qwen and Claude Code.

#[cfg(test)]
use super::credential_resolution_observation;
use super::{Deserialize, OauthRefresh, PathBuf, is_claude_code_alias, is_minimax_cn_alias};
pub(super) const MINIMAX_OAUTH_GLOBAL_TOKEN_ENDPOINT: &str = "https://account.minimax.io/oauth2/token";
pub(super) const MINIMAX_OAUTH_CN_TOKEN_ENDPOINT: &str = "https://account.minimaxi.com/oauth2/token";
pub(super) const MINIMAX_OAUTH_PLACEHOLDER: &str = "minimax-oauth";
pub(super) const MINIMAX_OAUTH_CN_PLACEHOLDER: &str = "minimax-oauth-cn";
pub(super) const MINIMAX_OAUTH_REFRESH_TOKEN_ENV: &str = "MINIMAX_OAUTH_REFRESH_TOKEN";
pub(super) const MINIMAX_OAUTH_REGION_ENV: &str = "MINIMAX_OAUTH_REGION";
pub(super) const MINIMAX_OAUTH_CLIENT_ID_ENV: &str = "MINIMAX_OAUTH_CLIENT_ID";
pub(super) const MINIMAX_OAUTH_DEFAULT_CLIENT_ID: &str = "78257093-7e40-4613-99e0-527b14b39113";
pub(super) const QWEN_OAUTH_PLACEHOLDER: &str = "qwen-oauth";
pub(super) const QWEN_OAUTH_TOKEN_ENV: &str = "QWEN_OAUTH_TOKEN";
pub(super) const QWEN_OAUTH_REFRESH_TOKEN_ENV: &str = "QWEN_OAUTH_REFRESH_TOKEN";
pub(super) const QWEN_OAUTH_RESOURCE_URL_ENV: &str = "QWEN_OAUTH_RESOURCE_URL";

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
    if let Some(client_id) = read_non_empty_env(MINIMAX_OAUTH_CLIENT_ID_ENV) {
        return client_id;
    }
    tracing::warn!("{}", crate::auth::terms::MINIMAX_OAUTH_WARNING);
    MINIMAX_OAUTH_DEFAULT_CLIENT_ID.to_string()
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
    Minimax(MinimaxOauthRegion),
}

impl OauthTokenEndpoint {
    pub(super) const fn real(self) -> &'static str {
        match self {
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

    // Qwen Code OAuth, including ~/.qwen/oauth_creds.json, is discontinued.
    // Do not read the cache, refresh it, or send it.
    let _ = (refresh, oauth_credentials_home());
    QwenOauthProviderContext {
        credential: None,
        base_url: None,
        refresh_withheld: false,
    }
}

pub(super) fn is_claude_code_oauth_placeholder(value: &str) -> bool {
    is_claude_code_alias(value)
}

pub(super) fn resolve_claude_code_context(
    credential_override: Option<&str>,
    _refresh: OauthRefresh,
) -> ClaudeCodeProviderContext {
    let override_value = credential_override.map(str::trim).filter(|value| !value.is_empty());
    let placeholder_requested = override_value.map(is_claude_code_oauth_placeholder).unwrap_or(false);

    if let Some(explicit) = override_value {
        if !placeholder_requested && !crate::auth::discovery::is_claude_code_oauth_setup_token(explicit) {
            return ClaudeCodeProviderContext {
                credential: Some(explicit.to_string()),
                refresh_token: None,
                expires_at: None,
                refresh_withheld: false,
            };
        }
    }

    // Do not read or write ~/.claude/.credentials.json.
    ClaudeCodeProviderContext {
        credential: None,
        refresh_token: None,
        expires_at: None,
        refresh_withheld: false,
    }
}

pub(super) fn refresh_minimax_oauth_access_token(name: &str, refresh_token: &str) -> anyhow::Result<String> {
    #[cfg(test)]
    credential_resolution_observation::record(2);
    let endpoint = oauth_token_endpoint(OauthTokenEndpoint::Minimax(minimax_oauth_region(name)));
    let client_id = minimax_oauth_client_id();
    let client = reqwest::blocking::Client::builder()
        .user_agent(crate::PRX_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| crate::prx_blocking_http_client());

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
