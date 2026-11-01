//! Explicit authenticated reload of one plugin in an already-running gateway.
use crate::config::manage::credentials::SecretString;
use anyhow::{Context, Result};
use std::time::Duration;

fn endpoint(origin: &str, name: &str) -> Result<reqwest::Url> {
    plugin_endpoint(origin, name, "reload")
}

pub(super) fn plugin_endpoint(origin: &str, name: &str, action: &str) -> Result<reqwest::Url> {
    anyhow::ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "plugin name must contain only ASCII letters, digits, underscores or hyphens"
    );
    anyhow::ensure!(
        matches!(action, "reload" | "enable"),
        "unsupported plugin gateway action"
    );
    let mut url = reqwest::Url::parse(origin).context("invalid gateway origin")?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    anyhow::ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "gateway requires HTTPS, or HTTP with a numeric loopback address"
    );
    anyhow::ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "gateway must be an origin without credentials, path, query or fragment"
    );
    url.set_path(&format!("/api/plugins/{name}/{action}"));
    Ok(url)
}

fn gateway_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .context("gateway client")
}

async fn read_gateway_token() -> Result<SecretString> {
    tokio::task::spawn_blocking(|| -> Result<SecretString> {
        use std::io::{IsTerminal, Read};
        anyhow::ensure!(
            !std::io::stdin().is_terminal(),
            "pipe a paired token to stdin; interactive token input is refused"
        );
        let mut bytes = zeroize::Zeroizing::new(String::new());
        std::io::stdin().lock().take(4097).read_to_string(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 4096, "gateway token exceeds 4096 bytes");
        let trimmed = bytes.trim();
        anyhow::ensure!(
            !trimmed.is_empty() && !trimmed.chars().any(char::is_whitespace),
            "invalid gateway token"
        );
        Ok(SecretString::new(trimmed.to_owned()))
    })
    .await?
}

pub(super) async fn reload(
    config: &crate::config::Config,
    name: &str,
    gateway_url: &str,
    token_from_stdin: bool,
    yes: bool,
) -> Result<serde_json::Value> {
    let url = endpoint(gateway_url, name)?;
    if !yes {
        return Ok(serde_json::json!({"applied":false,"name":name,"gateway":gateway_url,"action":"reload"}));
    }
    anyhow::ensure!(
        config.autonomy.level != crate::security::AutonomyLevel::ReadOnly,
        "read-only mode refuses plugin reload"
    );
    anyhow::ensure!(
        token_from_stdin,
        "reload requires --token-from-stdin and a paired gateway token"
    );
    let token = read_gateway_token().await?;
    send(url, token).await
}

/// Move one disabled plugin back into service on the running gateway.
///
/// Dry-run validates the origin and does not connect. `--yes` posts
/// `POST /api/plugins/{name}/enable` and returns that gateway's generation.
/// This command does not build a local runtime, write trust, or change
/// `[plugins] enabled`.
pub(super) async fn enable(
    config: &crate::config::Config,
    name: &str,
    gateway_url: &str,
    token_from_stdin: bool,
    yes: bool,
) -> Result<serde_json::Value> {
    let url = plugin_endpoint(gateway_url, name, "enable")?;
    if !yes {
        return Ok(serde_json::json!({
            "applied": false,
            "name": name,
            "enabled": false,
            "gateway": gateway_url,
            "action": "enable",
            "activation": "pass --token-from-stdin --yes to post enable to the gateway; this command does not refresh a local runtime"
        }));
    }
    anyhow::ensure!(
        config.autonomy.level != crate::security::AutonomyLevel::ReadOnly,
        "read-only mode refuses plugin enable"
    );
    anyhow::ensure!(
        token_from_stdin,
        "enable requires --token-from-stdin and a paired gateway token"
    );
    let token = read_gateway_token().await?;
    submit_enable(url, token).await
}

pub(super) async fn submit_enable(url: reqwest::Url, token: SecretString) -> Result<serde_json::Value> {
    let mut result = post(url, token, Some("{}"), "enable").await?;
    let object = result
        .as_object_mut()
        .context("gateway enable response must be a JSON object")?;
    object.insert("applied".to_string(), serde_json::Value::Bool(true));
    Ok(result)
}

async fn send(url: reqwest::Url, token: SecretString) -> Result<serde_json::Value> {
    let result = post(url, token, None, "reload").await?;
    anyhow::ensure!(
        result.get("success").and_then(serde_json::Value::as_bool) == Some(true),
        "gateway did not confirm reload"
    );
    let generation = result
        .get("generation")
        .and_then(serde_json::Value::as_u64)
        .context("gateway reload generation missing")?;
    Ok(serde_json::json!({"applied":true,"generation":generation}))
}

async fn post(url: reqwest::Url, token: SecretString, body: Option<&str>, action: &str) -> Result<serde_json::Value> {
    let client = gateway_client()?;
    let mut request = client.post(url).bearer_auth(token.expose());
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_owned());
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("gateway {action} request failed"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "gateway refused {action} (HTTP {})",
        response.status().as_u16()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("reading gateway {action} response"))?
    {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= 16 * 1024,
            "gateway {action} response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).with_context(|| format!("invalid gateway {action} response"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reload_destination_rejects_credentials_paths_and_insecure_remote_origins() {
        for url in [
            "http://example.com",
            "http://localhost",
            "https://token@example.com",
            "https://example.com/api",
            "https://example.com/?secret=x",
            "https://example.com/#fragment",
            "file:///tmp",
        ] {
            assert!(endpoint(url, "prx_fixture").is_err(), "{url}");
        }
        for name in ["", "..", "a/b", "a?b", "a%b"] {
            assert!(endpoint("http://127.0.0.1:1234", name).is_err());
        }
        assert_eq!(
            endpoint("http://[::1]:1234", "prx_fixture").unwrap().path(),
            "/api/plugins/prx_fixture/reload"
        );
    }

    #[tokio::test]
    async fn reload_dry_run_needs_no_token_or_connection_and_readonly_refuses_apply() {
        let mut config = crate::config::Config::default();
        let result = reload(&config, "prx_fixture", "http://127.0.0.1:1", false, false)
            .await
            .unwrap();
        assert_eq!(result.get("applied"), Some(&serde_json::json!(false)));
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        assert!(
            reload(&config, "prx_fixture", "http://127.0.0.1:1", true, true)
                .await
                .unwrap_err()
                .to_string()
                .contains("read-only")
        );
    }

    #[tokio::test]
    async fn reload_posts_exact_authenticated_route_and_never_follows_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for redirect in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .expect("test: gateway client did not connect")
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let byte = stream.read_u8().await.unwrap();
                    request.push(byte);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                    assert!(request.len() < 8192);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.starts_with("post /api/plugins/prx_fixture/reload http/1.1\r\n"));
                assert!(request.contains("authorization: bearer prx-test-token\r\n"));
                let response = if redirect {
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://192.0.2.1/token-sink\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                } else {
                    let body = r#"{"success":true,"generation":9,"message":"not echoed"}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let result = send(
                endpoint(&format!("http://{address}"), "prx_fixture").unwrap(),
                SecretString::new("prx-test-token".into()),
            )
            .await;
            if redirect {
                assert!(result.unwrap_err().to_string().contains("307"));
            } else {
                assert_eq!(result.unwrap(), serde_json::json!({"applied":true,"generation":9}));
            }
            server.await.unwrap();
        }
    }
}
