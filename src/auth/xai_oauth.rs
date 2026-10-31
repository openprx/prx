//! xAI Grok subscription OAuth (SuperGrok / X Premium).
//!
//! Verified against `https://auth.x.ai/.well-known/openid-configuration`:
//! authorization code + PKCE (`S256`), refresh tokens, and RFC 8628 device
//! code (`urn:ietf:params:oauth:grant-type:device_code`). The public client
//! id and loopback redirect are the Grok CLI registration that xAI allowlists
//! (OpenCode PR anomalyco/opencode#28557; <https://x.ai/news/grok-opencode>).
//! A different client id is rejected for loopback sign-in.
//!
//! Inference uses the resulting access token as `Authorization: Bearer` on
//! `https://api.x.ai` only. The token endpoint is `https://auth.x.ai`.

use crate::auth::profiles::TokenSet;
use crate::security::SecretStore;
use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Public Grok CLI OAuth client. xAI rejects loopback OAuth from clients that
/// are not on this registration.
pub const XAI_OAUTH_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const XAI_OAUTH_AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";
pub const XAI_OAUTH_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
pub const XAI_OAUTH_DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
pub const XAI_OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";
pub const XAI_OAUTH_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
pub const XAI_INFERENCE_ORIGIN: &str = "https://api.x.ai";
pub const XAI_INFERENCE_BASE: &str = "https://api.x.ai/v1";
pub const XAI_API_HOST: &str = "api.x.ai";
/// Refresh a little before expiry so a long turn does not fail mid-request.
pub const XAI_REFRESH_SKEW_SECS: u64 = 120;

const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const PENDING_FILE: &str = "auth-xai-pending.json";
const LOOPBACK_ADDR: &str = "127.0.0.1:56121";

#[derive(Debug, Clone)]
pub struct XaiPkce {
    pub code_verifier: String,
    pub code_challenge: String,
    pub state: String,
    pub nonce: String,
}

#[derive(Debug, Clone)]
pub struct DeviceCodeStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingXaiLogin {
    pub profile: String,
    pub code_verifier: String,
    pub state: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingXaiLoginFile {
    profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_verifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted_code_verifier: Option<String>,
    state: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// True when the access token should be refreshed before the next inference call.
pub fn access_needs_refresh(tokens: &TokenSet) -> bool {
    if tokens.access_token.trim().is_empty() || tokens.expires_at.is_none() {
        return true;
    }
    tokens.is_expiring_within(Duration::from_secs(XAI_REFRESH_SKEW_SECS))
}

/// Refuse to attach an OAuth bearer unless `url` is HTTPS on `api.x.ai`
/// with no userinfo and no non-default port.
pub fn ensure_oauth_bearer_target(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("xAI OAuth inference URL is not a URL")?;
    if parsed.scheme() != "https" {
        bail!("xAI OAuth bearer is refused for non-HTTPS URL");
    }
    if parsed.host_str() != Some(XAI_API_HOST) {
        let host = parsed.host_str().unwrap_or("unknown");
        bail!("xAI OAuth bearer is refused for host {host}");
    }
    if let Some(port) = parsed.port()
        && port != 443
    {
        bail!("xAI OAuth bearer is refused for non-default port {port}");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("xAI OAuth bearer is refused for a URL with userinfo");
    }
    Ok(())
}

/// The only inference base an OAuth bearer may be sent to.
///
/// A custom `api_url` on another host is an error. A URL that is already
/// `https://api.x.ai/...` is accepted and then ignored: chat still uses
/// [`XAI_INFERENCE_BASE`].
pub fn subscription_base_url(api_url: Option<&str>) -> Result<&'static str> {
    if let Some(custom) = api_url.map(str::trim).filter(|url| !url.is_empty()) {
        ensure_oauth_bearer_target(custom)?;
    }
    ensure_oauth_bearer_target(XAI_INFERENCE_BASE)?;
    Ok(XAI_INFERENCE_BASE)
}

pub fn generate_pkce() -> XaiPkce {
    let code_verifier = random_base64url(64);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    XaiPkce {
        code_verifier,
        code_challenge,
        state: random_base64url(24),
        nonce: random_base64url(24),
    }
}

pub fn build_authorize_url(pkce: &XaiPkce) -> String {
    let mut params = BTreeMap::new();
    params.insert("response_type", "code");
    params.insert("client_id", XAI_OAUTH_CLIENT_ID);
    params.insert("redirect_uri", XAI_OAUTH_REDIRECT_URI);
    params.insert("scope", XAI_OAUTH_SCOPE);
    params.insert("code_challenge", pkce.code_challenge.as_str());
    params.insert("code_challenge_method", "S256");
    params.insert("state", pkce.state.as_str());
    params.insert("nonce", pkce.nonce.as_str());
    // Required for the allowlisted Grok CLI loopback client. Without it,
    // accounts.x.ai rejects the redirect.
    params.insert("plan", "generic");
    params.insert("referrer", "prx");

    let mut encoded = Vec::with_capacity(params.len());
    for (key, value) in params {
        encoded.push(format!("{}={}", url_encode(key), url_encode(value)));
    }
    format!("{XAI_OAUTH_AUTHORIZE_URL}?{}", encoded.join("&"))
}

pub async fn exchange_code_for_tokens(client: &Client, code: &str, verifier: &str) -> Result<TokenSet> {
    exchange_code_for_tokens_at(client, &production_token_url(), code, verifier).await
}

pub async fn exchange_code_for_tokens_at(
    client: &Client,
    token_url: &str,
    code: &str,
    verifier: &str,
) -> Result<TokenSet> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", XAI_OAUTH_CLIENT_ID),
        ("redirect_uri", XAI_OAUTH_REDIRECT_URI),
        ("code_verifier", verifier),
    ];
    let response = client
        .post(token_url)
        .form(&form)
        .send()
        .await
        .context("Failed to exchange xAI OAuth authorization code")?;
    parse_token_response(response).await
}

pub async fn refresh_access_token(client: &Client, refresh_token: &str) -> Result<TokenSet> {
    refresh_access_token_at(client, &production_token_url(), refresh_token).await
}

pub async fn refresh_access_token_at(client: &Client, token_url: &str, refresh_token: &str) -> Result<TokenSet> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", XAI_OAUTH_CLIENT_ID),
    ];
    let response = client
        .post(token_url)
        .form(&form)
        .send()
        .await
        .context("Failed to refresh xAI OAuth token")?;
    parse_token_response(response).await
}

pub async fn start_device_code_flow(client: &Client) -> Result<DeviceCodeStart> {
    start_device_code_flow_at(client, &production_device_url()).await
}

pub async fn start_device_code_flow_at(client: &Client, device_url: &str) -> Result<DeviceCodeStart> {
    let form = [
        ("client_id", XAI_OAUTH_CLIENT_ID),
        ("scope", XAI_OAUTH_SCOPE),
        ("referrer", "prx"),
    ];
    let response = client
        .post(device_url)
        .form(&form)
        .send()
        .await
        .context("Failed to start xAI device-code flow")?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = oauth_error_detail(response).await;
        bail!("xAI device-code start failed ({status}): {detail}");
    }
    let parsed: DeviceCodeResponse = response
        .json()
        .await
        .context("Failed to parse xAI device-code response")?;
    if parsed.device_code.is_empty() || parsed.user_code.is_empty() || parsed.verification_uri.is_empty() {
        bail!("xAI device-code response is missing device_code, user_code, or verification_uri");
    }
    Ok(DeviceCodeStart {
        device_code: parsed.device_code,
        user_code: parsed.user_code,
        verification_uri: parsed.verification_uri,
        verification_uri_complete: parsed.verification_uri_complete,
        expires_in: parsed.expires_in.max(1),
        interval: parsed.interval.unwrap_or(5).max(1),
    })
}

pub async fn poll_device_code_tokens(client: &Client, device: &DeviceCodeStart) -> Result<TokenSet> {
    poll_device_code_tokens_at(client, &production_token_url(), device, tokio::time::sleep).await
}

pub async fn poll_device_code_tokens_at<S, F>(
    client: &Client,
    token_url: &str,
    device: &DeviceCodeStart,
    mut sleep: S,
) -> Result<TokenSet>
where
    S: FnMut(Duration) -> F,
    F: Future<Output = ()>,
{
    let started = Instant::now();
    let mut interval_secs = device.interval.max(1);
    let deadline = Duration::from_secs(device.expires_in.max(1));

    loop {
        if started.elapsed() > deadline {
            bail!("xAI device-code flow timed out before authorization completed");
        }
        sleep(Duration::from_secs(interval_secs)).await;

        let form = [
            ("grant_type", DEVICE_CODE_GRANT),
            ("device_code", device.device_code.as_str()),
            ("client_id", XAI_OAUTH_CLIENT_ID),
        ];
        let response = client
            .post(token_url)
            .form(&form)
            .send()
            .await
            .context("Failed polling xAI device-code token endpoint")?;

        if response.status().is_success() {
            return parse_token_response(response).await;
        }

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<OAuthErrorResponse>(&text) {
            match err.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval_secs = interval_secs.saturating_add(5);
                    continue;
                }
                "access_denied" | "authorization_denied" => {
                    bail!("xAI device authorization was denied");
                }
                "expired_token" => bail!("xAI device code expired"),
                _ => {
                    let detail = err.error_description.unwrap_or(err.error);
                    bail!("xAI device-code polling failed ({status}): {detail}");
                }
            }
        }
        bail!("xAI device-code polling failed ({status})");
    }
}

pub async fn receive_loopback_code(expected_state: &str, timeout: Duration) -> Result<String> {
    let listener = TcpListener::bind(LOOPBACK_ADDR).await.with_context(|| {
        format!(
            "Failed to bind {XAI_OAUTH_REDIRECT_URI}. That port is part of xAI's registered redirect; close anything already listening there, or use `prx auth login --provider xai --device-code`."
        )
    })?;

    let accepted = tokio::time::timeout(timeout, listener.accept())
        .await
        .context("Timed out waiting for the xAI browser callback")?
        .context("Failed to accept the xAI callback connection")?;
    let (mut stream, _) = accepted;
    let mut buffer = vec![0_u8; 8192];
    let bytes_read = stream
        .read(&mut buffer)
        .await
        .context("Failed to read the xAI callback request")?;
    // SAFETY: bytes_read <= buffer.len() because it is the return of stream.read.
    #[allow(clippy::indexing_slicing)]
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let first_line = request
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Malformed xAI callback request"))?;
    let path = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("xAI callback request missing path"))?;
    let code = parse_code_from_redirect(path, Some(expected_state))?;

    let body = "<html><body><h2>OpenPRX xAI login complete</h2><p>You can close this tab.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    Ok(code)
}

pub fn parse_code_from_redirect(input: &str, expected_state: Option<&str>) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("No OAuth code provided");
    }
    let query = trimmed.split_once('?').map_or(trimmed, |(_, right)| right);
    let params = parse_query_params(query);
    let is_callback = trimmed.contains('?')
        || params.contains_key("code")
        || params.contains_key("state")
        || params.contains_key("error");

    if let Some(err) = params.get("error") {
        let desc = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| "OAuth authorization failed".to_string());
        bail!("xAI OAuth error: {err} ({desc})");
    }
    if let Some(expected_state) = expected_state {
        if let Some(got) = params.get("state") {
            if !crate::security::pairing::constant_time_eq(got, expected_state) {
                bail!("OAuth state mismatch");
            }
        } else if is_callback {
            bail!("Missing OAuth state in callback");
        }
    }
    if let Some(code) = params.get("code").cloned() {
        return Ok(code);
    }
    if !is_callback {
        return Ok(trimmed.to_string());
    }
    bail!("Missing OAuth code in callback")
}

pub fn save_pending_login(state_dir: &Path, encrypt: bool, pending: &PendingXaiLogin) -> Result<()> {
    let path = pending_path(state_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let secret_store = SecretStore::new(state_dir, encrypt);
    let encrypted_code_verifier = secret_store.encrypt(&pending.code_verifier)?;
    let persisted = PendingXaiLoginFile {
        profile: pending.profile.clone(),
        code_verifier: None,
        encrypted_code_verifier: Some(encrypted_code_verifier),
        state: pending.state.clone(),
        created_at: pending.created_at.clone(),
    };
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    fs::write(&tmp, serde_json::to_vec_pretty(&persisted)?)?;
    set_owner_only_permissions(&tmp)?;
    fs::rename(&tmp, &path)?;
    set_owner_only_permissions(&path)?;
    Ok(())
}

pub fn load_pending_login(state_dir: &Path, encrypt: bool) -> Result<Option<PendingXaiLogin>> {
    let path = pending_path(state_dir);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let persisted: PendingXaiLoginFile = serde_json::from_slice(&bytes)?;
    let secret_store = SecretStore::new(state_dir, encrypt);
    let code_verifier = if let Some(encrypted) = persisted.encrypted_code_verifier {
        secret_store.decrypt(&encrypted)?
    } else if let Some(plaintext) = persisted.code_verifier {
        plaintext
    } else {
        bail!("Pending xAI login is missing code verifier");
    };
    Ok(Some(PendingXaiLogin {
        profile: persisted.profile,
        code_verifier,
        state: persisted.state,
        created_at: persisted.created_at,
    }))
}

pub fn clear_pending_login(state_dir: &Path) {
    let path = pending_path(state_dir);
    if let Ok(file) = fs::OpenOptions::new().write(true).open(&path) {
        let _ = file.set_len(0);
        let _ = file.sync_all();
    }
    let _ = fs::remove_file(path);
}

fn pending_path(state_dir: &Path) -> PathBuf {
    state_dir.join(PENDING_FILE)
}

/// Token endpoint as this build may call it. Tests rewrite it to a closed
/// loopback port so a test cannot reach `auth.x.ai` unless it passes an
/// explicit URL (D-081).
pub fn production_token_url() -> String {
    token_endpoint(XAI_OAUTH_TOKEN_URL)
}

pub fn production_device_url() -> String {
    token_endpoint(XAI_OAUTH_DEVICE_CODE_URL)
}

fn token_endpoint(url: &'static str) -> String {
    #[cfg(test)]
    {
        crate::providers::token_host(url)
    }
    #[cfg(not(test))]
    {
        crate::providers::token_host(url).to_string()
    }
}

async fn parse_token_response(response: reqwest::Response) -> Result<TokenSet> {
    if !response.status().is_success() {
        let status = response.status();
        let detail = oauth_error_detail(response).await;
        bail!("xAI OAuth token request failed ({status}): {detail}");
    }
    let token: TokenResponse = response.json().await.context("Failed to parse xAI token response")?;
    if token.access_token.trim().is_empty() {
        bail!("xAI token response missing access_token");
    }
    let expires_at = token
        .expires_in
        .and_then(|seconds| (seconds > 0).then(|| Utc::now() + chrono::Duration::seconds(seconds)))
        .or_else(|| jwt_expiry(&token.access_token));
    if token.refresh_token.as_deref().map(str::trim).is_none_or(str::is_empty) {
        tracing::warn!("xAI token response omitted refresh_token; the session cannot be refreshed later");
    }
    Ok(TokenSet {
        access_token: token.access_token,
        refresh_token: token
            .refresh_token
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        id_token: token.id_token,
        expires_at,
        token_type: token.token_type,
        scope: token.scope,
    })
}

async fn oauth_error_detail(response: reqwest::Response) -> String {
    let text = response.text().await.unwrap_or_default();
    serde_json::from_str::<OAuthErrorResponse>(&text)
        .ok()
        .map(|err| err.error_description.unwrap_or(err.error))
        .filter(|detail| !detail.trim().is_empty())
        .unwrap_or_else(|| "token endpoint rejected the request".to_string())
}

fn jwt_expiry(token: &str) -> Option<chrono::DateTime<Utc>> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let exp = claims.get("exp")?.as_i64()?;
    chrono::DateTime::<Utc>::from_timestamp(exp, 0)
}

fn parse_query_params(input: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in input.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(url_decode(key), url_decode(value));
    }
    out
}

fn random_base64url(byte_len: usize) -> String {
    use chacha20poly1305::aead::{OsRng, rand_core::RngCore};
    let mut bytes = vec![0_u8; byte_len];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn url_encode(input: &str) -> String {
    input
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

// SAFETY: every index is bounded by `bytes.len()` or by `i + 2 < bytes.len()`.
#[allow(clippy::indexing_slicing)]
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hi = (bytes[index + 1] as char).to_digit(16);
                let lo = (bytes[index + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (hi, lo)
                    && let Ok(value) = u8::try_from(high * 16 + low)
                {
                    out.push(value);
                    index += 3;
                    continue;
                }
                out.push(bytes[index]);
                index += 1;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::profiles::AuthProfileKind;
    use crate::auth::{AuthService, profiles::TokenSet};
    use chrono::Duration as ChronoDuration;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    #[test]
    fn authorize_url_uses_the_allowlisted_client_and_loopback() {
        let pkce = generate_pkce();
        let url = build_authorize_url(&pkce);
        assert!(url.starts_with("https://auth.x.ai/oauth2/authorize?"));
        assert!(url.contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A56121%2Fcallback"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("plan=generic"));
        assert!(url.contains("referrer=prx"));
        assert!(url.contains("grok-cli%3Aaccess"));
        assert!(url.contains("api%3Aaccess"));
        assert!(url.contains("offline_access"));
        assert!(!url.contains("client_secret"));
    }

    #[test]
    fn oauth_bearer_is_pinned_to_api_x_ai() {
        assert!(ensure_oauth_bearer_target("https://api.x.ai/v1/chat/completions").is_ok());
        assert!(ensure_oauth_bearer_target("https://api.x.ai/v1/models").is_ok());
        assert!(ensure_oauth_bearer_target(XAI_INFERENCE_BASE).is_ok());
        for refused in [
            "http://api.x.ai/v1/chat/completions",
            "https://api.x.ai:8443/v1/chat/completions",
            "https://evil.example/v1/chat/completions",
            "https://api.x.ai.evil/v1/chat/completions",
            "https://user:secret@api.x.ai/v1/chat/completions",
            "https://auth.x.ai/oauth2/token",
            "https://api.x.ai.attacker.com/v1/chat/completions",
        ] {
            assert!(ensure_oauth_bearer_target(refused).is_err(), "{refused}");
        }
        assert!(subscription_base_url(Some("https://evil.example/v1")).is_err());
        assert_eq!(
            subscription_base_url(Some("https://api.x.ai/v1")).expect("same host"),
            XAI_INFERENCE_BASE
        );
        assert_eq!(subscription_base_url(None).expect("default"), XAI_INFERENCE_BASE);
    }

    #[test]
    fn test_builds_do_not_target_the_real_xai_token_host() {
        for url in [production_token_url(), production_device_url()] {
            assert!(url.starts_with("http://127.0.0.1:"), "{url}");
            assert!(!url.contains("auth.x.ai"), "{url}");
            assert!(!url.contains("api.x.ai"), "{url}");
        }
    }

    #[test]
    fn callback_parser_checks_state() {
        let code =
            parse_code_from_redirect("http://127.0.0.1:56121/callback?code=abc&state=xyz", Some("xyz")).expect("code");
        assert_eq!(code, "abc");
        let err = parse_code_from_redirect("/callback?code=abc&state=nope", Some("xyz")).unwrap_err();
        assert!(err.to_string().contains("state mismatch"));
    }

    #[tokio::test]
    async fn device_code_start_posts_the_public_client() {
        let (url, requests) = spawn_mock(vec![status_body(
            200,
            r#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"https://auth.x.ai/device","verification_uri_complete":"https://auth.x.ai/device?user_code=ABCD-EFGH","expires_in":180,"interval":5}"#,
        )])
        .await;
        let started = start_device_code_flow_at(&Client::new(), &url)
            .await
            .expect("device start");
        assert_eq!(started.user_code, "ABCD-EFGH");
        assert_eq!(started.interval, 5);
        let body = requests.lock().await.join("\n");
        assert!(body.contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"));
        assert!(body.contains("grok-cli"));
        assert!(body.contains("referrer=prx"));
        assert!(!body.contains("api.x.ai"));
    }

    #[tokio::test]
    async fn device_poll_waits_through_authorization_pending() {
        let (url, requests) = spawn_mock(vec![
            status_body(400, r#"{"error":"authorization_pending"}"#),
            status_body(
                200,
                r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#,
            ),
        ])
        .await;
        let sleeps = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let record = Arc::clone(&sleeps);
        let tokens = poll_device_code_tokens_at(&Client::new(), &url, &sample_device(), move |delay| {
            record.lock().push(delay);
            std::future::ready(())
        })
        .await
        .expect("pending then success");
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(
            sleeps.lock().clone(),
            vec![Duration::from_secs(5), Duration::from_secs(5)]
        );
        assert_eq!(requests.lock().await.len(), 2);
        let body = requests.lock().await.join("\n");
        assert!(body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
        assert!(body.contains("device_code=device-1"));
    }

    #[tokio::test]
    async fn device_poll_backs_off_on_slow_down() {
        let (url, _) = spawn_mock(vec![
            status_body(400, r#"{"error":"slow_down"}"#),
            status_body(
                200,
                r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":1800}"#,
            ),
        ])
        .await;
        let sleeps = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let record = Arc::clone(&sleeps);
        let tokens = poll_device_code_tokens_at(&Client::new(), &url, &sample_device(), move |delay| {
            record.lock().push(delay);
            std::future::ready(())
        })
        .await
        .expect("slow_down then success");
        assert_eq!(tokens.access_token, "access-2");
        assert_eq!(
            sleeps.lock().clone(),
            vec![Duration::from_secs(5), Duration::from_secs(10)]
        );
    }

    #[tokio::test]
    async fn device_poll_stops_on_expired_token() {
        let (url, requests) = spawn_mock(vec![status_body(400, r#"{"error":"expired_token"}"#)]).await;
        let error = poll_device_code_tokens_at(&Client::new(), &url, &sample_device(), |_| std::future::ready(()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("expired"), "{error}");
        assert_eq!(requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn refresh_posts_only_to_the_token_endpoint_and_keeps_the_rotated_refresh_token() {
        let (url, requests) = spawn_mock(vec![status_body(
            200,
            r#"{"access_token":"new-access","refresh_token":"rt-new","expires_in":3600,"token_type":"Bearer"}"#,
        )])
        .await;
        assert!(url.starts_with("http://127.0.0.1:"));
        let dir = tempfile::tempdir().expect("temp dir");
        let auth = AuthService::new(dir.path(), false).with_xai_token_endpoint(url.clone());
        auth.store_xai_tokens(
            "default",
            TokenSet {
                access_token: "old-access".into(),
                refresh_token: Some("rt-old".into()),
                id_token: None,
                expires_at: Some(Utc::now() - ChronoDuration::minutes(5)),
                token_type: Some("Bearer".into()),
                scope: None,
            },
            true,
        )
        .expect("store");

        let access = auth
            .get_valid_xai_access_token(None)
            .await
            .expect("refresh")
            .expect("token");
        assert_eq!(access, "new-access");
        let profile = auth.get_profile("xai", None).expect("profile").expect("saved");
        assert_eq!(profile.kind, AuthProfileKind::OAuth);
        let saved = profile.token_set.expect("token set");
        assert_eq!(saved.access_token, "new-access");
        assert_eq!(saved.refresh_token.as_deref(), Some("rt-new"));
        let body = requests.lock().await.join("\n");
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=rt-old"));
        assert!(body.contains(&format!("client_id={XAI_OAUTH_CLIENT_ID}")));
        assert!(!body.contains("api.x.ai"));
        assert!(!body.contains("new-access"));
    }

    #[tokio::test]
    async fn a_fresh_access_token_is_reused_without_a_refresh_request() {
        let (url, requests) = spawn_mock(Vec::new()).await;
        let dir = tempfile::tempdir().expect("temp dir");
        let auth = AuthService::new(dir.path(), false).with_xai_token_endpoint(url);
        auth.store_xai_tokens(
            "default",
            TokenSet {
                access_token: "still-fresh".into(),
                refresh_token: Some("rt".into()),
                id_token: None,
                expires_at: Some(Utc::now() + ChronoDuration::hours(1)),
                token_type: Some("Bearer".into()),
                scope: None,
            },
            true,
        )
        .expect("store");
        let access = auth
            .get_valid_xai_access_token(None)
            .await
            .expect("read")
            .expect("token");
        assert_eq!(access, "still-fresh");
        assert!(requests.lock().await.is_empty());
    }

    fn sample_device() -> DeviceCodeStart {
        DeviceCodeStart {
            device_code: "device-1".into(),
            user_code: "CODE".into(),
            verification_uri: "https://auth.x.ai/device".into(),
            verification_uri_complete: None,
            expires_in: 120,
            interval: 5,
        }
    }

    struct MockReply {
        status: u16,
        body: String,
    }

    fn status_body(status: u16, body: &str) -> MockReply {
        MockReply {
            status,
            body: body.to_string(),
        }
    }

    async fn spawn_mock(replies: Vec<MockReply>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let url = format!("http://{address}/oauth2/token");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = vec![0_u8; 8192];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                // SAFETY: read <= buffer.len().
                #[allow(clippy::indexing_slicing)]
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                recorded.lock().await.push(request);
                let raw = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                let _ = stream.write_all(raw.as_bytes()).await;
            }
        });
        (url, seen)
    }
}
