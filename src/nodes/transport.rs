use crate::nodes::protocol::{JsonRpcRequest, JsonRpcResponse};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use sha2::Sha256;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct TransportRequest {
    pub endpoint: String,
    pub bearer_token: String,
    pub hmac_secret: Option<String>,
    pub method: String,
    pub params: Value,
}

#[async_trait]
pub trait NodeTransport: Send + Sync {
    async fn call(&self, request: &TransportRequest) -> Result<Value>;
}

#[derive(Clone)]
pub struct H2Transport {
    client: reqwest::Client,
    max_retries: u8,
}

impl H2Transport {
    pub fn new(timeout: Duration, max_retries: u8) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .http2_prior_knowledge()
            .build()
            .context("failed to build HTTP/2 transport client")?;

        Ok(Self {
            client,
            max_retries,
        })
    }

    pub fn validate_endpoint(endpoint: &str) -> Result<()> {
        let url = reqwest::Url::parse(endpoint)
            .with_context(|| format!("invalid node endpoint URL: {endpoint}"))?;

        if url.scheme() == "https" {
            return Ok(());
        }

        if url.scheme() == "http" {
            let host = url
                .host_str()
                .ok_or_else(|| anyhow!("node endpoint missing host"))?
                .to_ascii_lowercase();
            if host == "localhost" || host == "127.0.0.1" {
                return Ok(());
            }
        }

        Err(anyhow!(
            "insecure node endpoint '{endpoint}': use https://, or loopback http://localhost / http://127.0.0.1 only"
        ))
    }

    fn rpc_url(endpoint: &str) -> String {
        format!("{}/rpc", endpoint.trim_end_matches('/'))
    }

    fn sign(secret: &str, timestamp: i64, body: &str) -> Result<String> {
        let payload = format!("{timestamp}.{body}");
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .map_err(|_| anyhow!("invalid hmac key length"))?;
        mac.update(payload.as_bytes());
        Ok(hex::encode(mac.finalize().into_bytes()))
    }
}

#[async_trait]
impl NodeTransport for H2Transport {
    async fn call(&self, request: &TransportRequest) -> Result<Value> {
        Self::validate_endpoint(&request.endpoint)?;
        let rpc = JsonRpcRequest::new(
            Uuid::new_v4().to_string(),
            &request.method,
            request.params.clone(),
        );
        let body = serde_json::to_string(&rpc).context("failed to encode JSON-RPC request")?;
        let url = Self::rpc_url(&request.endpoint);

        let mut last_err: Option<anyhow::Error> = None;

        for attempt in 0..=self.max_retries {
            let mut req = self
                .client
                .post(&url)
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {}", request.bearer_token))
                .body(body.clone());

            if let Some(secret) = &request.hmac_secret {
                let ts = Utc::now().timestamp();
                let signature = Self::sign(secret, ts, &body)?;
                req = req
                    .header("X-ZeroClaw-Timestamp", ts.to_string())
                    .header("X-ZeroClaw-Signature", signature);
            }

            match req.send().await {
                Ok(response) => {
                    let status = response.status();
                    let payload = response
                        .text()
                        .await
                        .context("failed reading JSON-RPC response body")?;

                    if !status.is_success() {
                        last_err = Some(anyhow!("remote status {status}: {payload}"));
                    } else {
                        let rpc_response: JsonRpcResponse =
                            serde_json::from_str(&payload).context("invalid JSON-RPC response")?;

                        if let Some(error) = rpc_response.error {
                            return Err(anyhow!(
                                "JSON-RPC error {}: {}",
                                error.code,
                                error.message
                            ));
                        }

                        return rpc_response
                            .result
                            .ok_or_else(|| anyhow!("JSON-RPC response missing result"));
                    }
                }
                Err(error) => {
                    last_err = Some(error.into());
                }
            }

            if attempt < self.max_retries {
                let backoff = 100_u64.saturating_mul(2_u64.pow(u32::from(attempt)));
                sleep(Duration::from_millis(backoff)).await;
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("remote transport request failed")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_url_appends_path() {
        assert_eq!(
            H2Transport::rpc_url("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/rpc"
        );
    }

    #[test]
    fn hmac_is_stable() {
        let sig = H2Transport::sign("k", 123, "{}{}").unwrap();
        assert_eq!(sig.len(), 64);
    }

    #[test]
    fn endpoint_validation_allows_https_or_loopback_http() {
        assert!(H2Transport::validate_endpoint("https://example.com").is_ok());
        assert!(H2Transport::validate_endpoint("http://127.0.0.1:8787").is_ok());
        assert!(H2Transport::validate_endpoint("http://localhost:8787").is_ok());
    }

    #[test]
    fn endpoint_validation_rejects_plain_remote_http() {
        assert!(H2Transport::validate_endpoint("http://10.0.0.2:8787").is_err());
    }
}
