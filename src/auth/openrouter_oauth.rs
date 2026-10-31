//! OpenRouter OAuth PKCE. The exchange returns an API key billed to the user.
//!
//! Docs: <https://openrouter.ai/docs/guides/overview/auth/oauth>

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const AUTH_URL: &str = "https://openrouter.ai/auth";
const EXCHANGE_URL: &str = "https://openrouter.ai/api/v1/auth/keys";

#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

#[derive(Debug, Deserialize)]
struct KeyResponse {
    key: String,
}

pub fn generate_pkce() -> Pkce {
    use chacha20poly1305::aead::{OsRng, rand_core::RngCore};
    let mut bytes = vec![0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    Pkce { verifier, challenge }
}

pub fn browser_authorize_url(callback_url: &str, pkce: &Pkce) -> String {
    format!(
        "{AUTH_URL}?callback_url={}&code_challenge={}&code_challenge_method=S256&key_label=prx",
        url_encode(callback_url),
        url_encode(&pkce.challenge)
    )
}

pub fn headless_authorize_url(pkce: &Pkce) -> String {
    format!(
        "{AUTH_URL}?code_challenge={}&code_challenge_method=S256&key_label=prx",
        url_encode(&pkce.challenge)
    )
}

pub async fn receive_loopback_code(listener: TcpListener, timeout: Duration) -> Result<String> {
    let accepted = tokio::time::timeout(timeout, listener.accept())
        .await
        .context("Timed out waiting for the OpenRouter callback")?
        .context("Failed to accept the OpenRouter callback")?;
    let (mut stream, _) = accepted;
    let mut buffer = vec![0_u8; 8192];
    let read = stream
        .read(&mut buffer)
        .await
        .context("Failed to read the OpenRouter callback")?;
    #[allow(clippy::indexing_slicing)]
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("OpenRouter callback request missing path"))?;
    let code = path
        .split('?')
        .nth(1)
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == "code").then(|| url_decode(value))
            })
        })
        .filter(|code| !code.is_empty())
        .ok_or_else(|| anyhow::anyhow!("OpenRouter callback is missing code"))?;
    let body = "<html><body><h2>OpenPRX OpenRouter login complete</h2><p>You can close this tab.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    Ok(code)
}

pub async fn bind_loopback() -> Result<(TcpListener, String)> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("Failed to bind an OpenRouter loopback callback")?;
    let port = listener
        .local_addr()
        .context("OpenRouter callback listener has no address")?
        .port();
    let callback = format!("http://127.0.0.1:{port}/callback");
    Ok((listener, callback))
}

pub async fn exchange_code(client: &reqwest::Client, code: &str, verifier: &str) -> Result<String> {
    let response = client
        .post(crate::providers::token_host(EXCHANGE_URL))
        .header(reqwest::header::USER_AGENT, crate::PRX_USER_AGENT)
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256",
        }))
        .send()
        .await
        .context("Failed to exchange the OpenRouter authorization code")?;
    if !response.status().is_success() {
        let status = response.status();
        bail!("OpenRouter code exchange failed ({status})");
    }
    let parsed: KeyResponse = response
        .json()
        .await
        .context("OpenRouter code exchange response is not JSON")?;
    if parsed.key.trim().is_empty() {
        bail!("OpenRouter code exchange returned an empty key");
    }
    Ok(parsed.key)
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

#[allow(clippy::indexing_slicing)]
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hi = (bytes[index + 1] as char).to_digit(16);
            let lo = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (hi, lo)
                && let Ok(value) = u8::try_from(high * 16 + low)
            {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
