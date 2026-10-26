//! Explicit authenticated reload of one plugin in an already-running gateway.
use crate::config::manage::credentials::SecretString;
use anyhow::{Context, Result};
use std::time::Duration;

fn endpoint(origin: &str, name: &str) -> Result<reqwest::Url> {
    anyhow::ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "plugin name must contain only ASCII letters, digits, underscores or hyphens"
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
    url.set_path(&format!("/api/plugins/{name}/reload"));
    Ok(url)
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
    let token = tokio::task::spawn_blocking(|| -> Result<SecretString> {
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
    .await??;
    send(url, token).await
}

async fn send(url: reqwest::Url, token: SecretString) -> Result<serde_json::Value> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut response = client
        .post(url)
        .bearer_auth(token.expose())
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("gateway reload request failed"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "gateway refused reload (HTTP {})",
        response.status().as_u16()
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("reading gateway reload response")? {
        anyhow::ensure!(
            body.len().saturating_add(chunk.len()) <= 16 * 1024,
            "gateway reload response exceeds limit"
        );
        body.extend_from_slice(&chunk);
    }
    let result: serde_json::Value = serde_json::from_slice(&body).context("invalid gateway reload response")?;
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
                let (mut stream, _) = listener.accept().await.unwrap();
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
