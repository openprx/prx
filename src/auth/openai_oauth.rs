use crate::auth::profiles::TokenSet;
use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// First-time Sign in with ChatGPT registration entrypoint. Not a client id to store.
pub const OPENAI_OAUTH_DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
pub const OPENAI_OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
pub const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
pub const OPENAI_OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
pub const OPENAI_OAUTH_SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub const OPENAI_OAUTH_RESOURCE: &str = "https://api.openai.com/v1";
pub const OPENAI_AGENT_NAME_HINT: &str = "prx";
pub const OPENAI_CLIENT_ID_METADATA: &str = "oai_client_id";

#[derive(Debug, Clone)]
pub struct PkceState {
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
    pub message: Option<String>,
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

pub fn generate_pkce_state() -> PkceState {
    let code_verifier = random_base64url(64);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);

    PkceState {
        code_verifier,
        code_challenge,
        state: random_base64url(24),
        nonce: random_base64url(24),
    }
}

pub fn build_authorize_url(
    pkce: &PkceState,
    host_id: &str,
    issued_client_id: Option<&str>,
    id_token_hint: Option<&str>,
) -> String {
    let client_id = issued_client_id
        .map(str::trim)
        .filter(|value| value.starts_with("oaiapp_"))
        .unwrap_or(OPENAI_OAUTH_DYNAMIC_CLIENT_ID);
    let mut params = BTreeMap::new();
    params.insert("response_type", "code");
    params.insert("client_id", client_id);
    params.insert("redirect_uri", OPENAI_OAUTH_REDIRECT_URI);
    params.insert("scope", OPENAI_OAUTH_SCOPE);
    params.insert("resource", OPENAI_OAUTH_RESOURCE);
    params.insert("code_challenge", pkce.code_challenge.as_str());
    params.insert("code_challenge_method", "S256");
    params.insert("state", pkce.state.as_str());
    params.insert("nonce", pkce.nonce.as_str());
    params.insert("ext_agent_host_id", host_id);
    if client_id == OPENAI_OAUTH_DYNAMIC_CLIENT_ID {
        params.insert("agent_name_hint", OPENAI_AGENT_NAME_HINT);
    }
    if let Some(hint) = id_token_hint.map(str::trim).filter(|value| !value.is_empty()) {
        params.insert("id_token_hint", hint);
    }

    let mut encoded: Vec<String> = Vec::with_capacity(params.len());
    for (k, v) in params {
        encoded.push(format!("{}={}", url_encode(k), url_encode(v)));
    }

    format!("{OPENAI_OAUTH_AUTHORIZE_URL}?{}", encoded.join("&"))
}

pub async fn exchange_code_for_tokens(
    client: &Client,
    code: &str,
    pkce: &PkceState,
    client_id: &str,
) -> Result<TokenSet> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id),
        ("redirect_uri", OPENAI_OAUTH_REDIRECT_URI),
        ("code_verifier", pkce.code_verifier.as_str()),
        ("resource", OPENAI_OAUTH_RESOURCE),
    ];

    let response = client
        .post(crate::providers::token_host(OPENAI_OAUTH_TOKEN_URL))
        .form(&form)
        .send()
        .await
        .context("Failed to exchange OpenAI OAuth authorization code")?;

    parse_token_response(response).await
}

pub async fn refresh_access_token(client: &Client, refresh_token: &str, client_id: &str) -> Result<TokenSet> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
        ("resource", OPENAI_OAUTH_RESOURCE),
    ];

    let response = client
        .post(crate::providers::token_host(OPENAI_OAUTH_TOKEN_URL))
        .form(&form)
        .send()
        .await
        .context("Failed to refresh OpenAI OAuth token")?;

    parse_token_response(response).await
}

pub async fn start_device_code_flow(_client: &Client) -> Result<DeviceCodeStart> {
    bail!("{}", crate::auth::terms::OPENAI_DEVICE_CODE_UNSUPPORTED)
}

pub async fn poll_device_code_tokens(_client: &Client, _device: &DeviceCodeStart) -> Result<TokenSet> {
    bail!("{}", crate::auth::terms::OPENAI_DEVICE_CODE_UNSUPPORTED)
}

pub async fn receive_loopback_code(expected_state: &str, timeout: Duration) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:1455")
        .await
        .context("Failed to bind callback listener at 127.0.0.1:1455")?;

    let accepted = tokio::time::timeout(timeout, listener.accept())
        .await
        .context("Timed out waiting for browser callback")?
        .context("Failed to accept callback connection")?;

    let (mut stream, _) = accepted;
    let mut buffer = vec![0_u8; 8192];
    let bytes_read = stream
        .read(&mut buffer)
        .await
        .context("Failed to read callback request")?;

    // SAFETY: bytes_read <= buffer.len() because it is the return value of stream.read(&mut buffer)
    #[allow(clippy::indexing_slicing)]
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let first_line = request
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Malformed callback request"))?;

    let path = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Callback request missing path"))?;

    let path = path.to_string();
    let _code = parse_code_from_redirect(&path, Some(expected_state))?;

    let body = "<html><body><h2>OpenPRX login complete</h2><p>You can close this tab.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;

    Ok(path)
}

pub fn load_or_create_host_id(state_dir: &std::path::Path) -> Result<String> {
    let path = state_dir.join("openai-host-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let host_id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    std::fs::write(&path, &host_id)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(host_id)
}

pub fn issued_client_id_from_callback(input: &str) -> Option<String> {
    let query = input.split_once('?').map_or(input, |(_, right)| right);
    parse_query_params(query)
        .get("client_id")
        .map(|value| value.trim().to_string())
        .filter(|value| value.starts_with("oaiapp_"))
}

pub fn parse_code_from_redirect(input: &str, expected_state: Option<&str>) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("No OAuth code provided");
    }

    let query = if let Some((_, right)) = trimmed.split_once('?') {
        right
    } else {
        trimmed
    };

    let params = parse_query_params(query);
    let is_callback_payload = trimmed.contains('?')
        || params.contains_key("code")
        || params.contains_key("state")
        || params.contains_key("error");

    if let Some(err) = params.get("error") {
        let desc = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| "OAuth authorization failed".to_string());
        anyhow::bail!("OpenAI OAuth error: {err} ({desc})");
    }

    if let Some(expected_state) = expected_state {
        if let Some(got) = params.get("state") {
            // Use constant-time comparison to prevent timing-based CSRF attacks
            // that could infer valid state values.
            if !crate::security::pairing::constant_time_eq(got, expected_state) {
                anyhow::bail!("OAuth state mismatch");
            }
        } else if is_callback_payload {
            anyhow::bail!("Missing OAuth state in callback");
        }
    }

    if let Some(code) = params.get("code").cloned() {
        return Ok(code);
    }

    if !is_callback_payload {
        return Ok(trimmed.to_string());
    }

    anyhow::bail!("Missing OAuth code in callback")
}

pub fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let claims = decode_jwt_claims(token)?;

    for key in [
        "account_id",
        "accountId",
        "acct",
        "sub",
        "https://api.openai.com/account_id",
    ] {
        if let Some(value) = claims.get(key).and_then(|v| v.as_str()) {
            if !value.trim().is_empty() {
                return Some(value.to_string());
            }
        }
    }

    // Current Codex CLI access tokens nest the ChatGPT account id here.
    if let Some(value) = claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(|value| value.as_str())
        && !value.trim().is_empty()
    {
        return Some(value.to_string());
    }

    None
}

/// Account id from the access token, then the id token.
pub fn extract_account_id(access_token: &str, id_token: Option<&str>) -> Option<String> {
    extract_account_id_from_jwt(access_token).or_else(|| id_token.and_then(extract_account_id_from_jwt))
}

pub fn extract_expiry_from_jwt(token: &str) -> Option<chrono::DateTime<Utc>> {
    let claims = decode_jwt_claims(token)?;
    let exp = claims.get("exp")?.as_i64()?;
    chrono::DateTime::<Utc>::from_timestamp(exp, 0)
}

fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

async fn parse_token_response(response: reqwest::Response) -> Result<TokenSet> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI OAuth token request failed ({status}): {body}");
    }

    let token: TokenResponse = response.json().await.context("Failed to parse OpenAI token response")?;

    let expires_at = token.expires_in.and_then(|seconds| {
        if seconds <= 0 {
            None
        } else {
            Some(Utc::now() + chrono::Duration::seconds(seconds))
        }
    });

    Ok(TokenSet {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        id_token: token.id_token,
        expires_at,
        token_type: token.token_type,
        scope: token.scope,
    })
}

fn parse_query_params(input: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in input.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
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
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect::<String>()
}

// SAFETY: All byte accesses in url_decode are guarded:
// - bytes[i] is under `while i < bytes.len()`
// - bytes[i+1] and bytes[i+2] are under `i + 2 < bytes.len()`
#[allow(clippy::indexing_slicing)]
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = bytes[i + 1] as char;
                let lo = bytes[i + 2] as char;
                if let (Some(h), Some(l)) = (hi.to_digit(16), lo.to_digit(16)) {
                    if let Ok(value) = u8::try_from(h * 16 + l) {
                        out.push(value);
                        i += 3;
                        continue;
                    }
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_generation_is_valid() {
        let pkce = generate_pkce_state();
        assert!(pkce.code_verifier.len() >= 43);
        assert!(!pkce.code_challenge.is_empty(), "expected a non-empty value");
        assert!(!pkce.state.is_empty(), "expected a non-empty value");
    }

    #[test]
    fn parse_redirect_url_extracts_code() {
        let code =
            parse_code_from_redirect("http://127.0.0.1:1455/auth/callback?code=abc123&state=xyz", Some("xyz")).unwrap();
        assert_eq!(code, "abc123");
    }

    #[test]
    fn parse_redirect_accepts_raw_code() {
        let code = parse_code_from_redirect("raw-code", None).unwrap();
        assert_eq!(code, "raw-code");
    }

    #[test]
    fn parse_redirect_rejects_state_mismatch() {
        let err = parse_code_from_redirect("/auth/callback?code=x&state=a", Some("b")).unwrap_err();
        assert!(err.to_string().contains("state mismatch"));
    }

    #[test]
    fn parse_redirect_rejects_error_without_code() {
        let err = parse_code_from_redirect(
            "/auth/callback?error=access_denied&error_description=user+cancelled",
            Some("xyz"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("OpenAI OAuth error: access_denied"));
    }

    #[test]
    fn extract_account_id_from_jwt_payload() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{\"account_id\":\"acct_123\"}");
        let token = format!("{header}.{payload}.sig");

        let account = extract_account_id_from_jwt(&token);
        assert_eq!(account.as_deref(), Some("acct_123"));
    }

    #[test]
    fn extract_account_id_reads_the_nested_chatgpt_claim_and_the_id_token() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        let access_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_nested"}}"#);
        let access = format!("{header}.{access_payload}.sig");
        assert_eq!(extract_account_id_from_jwt(&access).as_deref(), Some("acct_nested"));

        let id_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"account_id":"acct_from_id"}"#);
        let id_token = format!("{header}.{id_payload}.sig");
        let bare = format!(
            "{header}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}")
        );
        let bare = format!("{bare}.sig");
        assert_eq!(
            extract_account_id(&bare, Some(&id_token)).as_deref(),
            Some("acct_from_id")
        );
    }

    #[test]
    fn extract_expiry_from_jwt_payload() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{\"exp\":1900000000}");
        let token = format!("{header}.{payload}.sig");

        let expiry = extract_expiry_from_jwt(&token);
        assert_eq!(expiry.map(|ts| ts.timestamp()), Some(1_900_000_000));
    }
}
