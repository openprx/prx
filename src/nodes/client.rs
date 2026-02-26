use crate::config::RemoteNodeConfig;
use crate::nodes::protocol::{
    AsyncTaskAccepted, CancelParams, ExecShellParams, ExecShellResult, MetricsResult, PingResult,
    ReadFileParams, ReadFileResult, TaskListResult, TaskStatusParams, TaskStatusResult,
    WriteFileParams, WriteFileResult,
};
use crate::nodes::transport::{NodeTransport, TransportRequest};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const FAILURE_THRESHOLD: u8 = 3;
const UNHEALTHY_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct CircuitBreakerState {
    consecutive_failures: u8,
    unhealthy_until: Option<Instant>,
}

impl CircuitBreakerState {
    fn new() -> Self {
        Self {
            consecutive_failures: 0,
            unhealthy_until: None,
        }
    }

    fn allow_request(&self) -> Result<()> {
        if let Some(until) = self.unhealthy_until {
            if Instant::now() < until {
                bail!("node temporarily unhealthy, retry after cooldown")
            }
        }
        Ok(())
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.unhealthy_until = None;
    }

    fn record_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= FAILURE_THRESHOLD {
            self.unhealthy_until = Some(Instant::now() + UNHEALTHY_COOLDOWN);
            self.consecutive_failures = 0;
        }
    }
}

pub struct RemoteNodeClient {
    node: RemoteNodeConfig,
    transport: Arc<dyn NodeTransport>,
    circuit_breaker: Arc<Mutex<CircuitBreakerState>>,
}

impl RemoteNodeClient {
    pub fn new(node: RemoteNodeConfig, transport: Arc<dyn NodeTransport>) -> Self {
        Self {
            node,
            transport,
            circuit_breaker: Arc::new(Mutex::new(CircuitBreakerState::new())),
        }
    }

    pub fn node_id(&self) -> &str {
        &self.node.id
    }

    pub async fn is_healthy(&self) -> bool {
        let guard = self.circuit_breaker.lock().await;
        guard.allow_request().map(|_| true).unwrap_or(false)
    }

    async fn call_rpc<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        {
            let guard = self.circuit_breaker.lock().await;
            guard.allow_request()?;
        }

        let request = TransportRequest {
            endpoint: self.node.endpoint.clone(),
            bearer_token: self.node.bearer_token.clone(),
            hmac_secret: self.node.hmac_secret.clone(),
            method: method.to_string(),
            params,
        };

        let result = self.transport.call(&request).await;

        let mut guard = self.circuit_breaker.lock().await;
        match result {
            Ok(value) => {
                guard.record_success();
                serde_json::from_value(value).context("invalid JSON-RPC result payload")
            }
            Err(error) => {
                guard.record_failure();
                Err(error)
            }
        }
    }

    pub async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        let result: PingResult = self.call_rpc("node.ping", serde_json::json!({})).await?;
        if result.message != "pong" {
            bail!("unexpected ping response")
        }
        Ok(start.elapsed())
    }

    pub async fn exec_shell(
        &self,
        cmd: &str,
        timeout_ms: Option<u64>,
        cwd: Option<&str>,
    ) -> Result<ExecShellResult> {
        if cmd.trim().is_empty() {
            bail!("command cannot be empty")
        }

        self.call_rpc(
            "node.exec_shell",
            serde_json::to_value(ExecShellParams {
                cmd: cmd.to_string(),
                timeout_ms,
                cwd: cwd.map(ToOwned::to_owned),
                env: None,
                async_exec: None,
                callback_url: None,
            })?,
        )
        .await
    }

    pub async fn exec_shell_async(
        &self,
        cmd: &str,
        timeout_ms: Option<u64>,
        cwd: Option<&str>,
        env: Option<HashMap<String, String>>,
        callback_url: Option<&str>,
    ) -> Result<AsyncTaskAccepted> {
        if cmd.trim().is_empty() {
            bail!("command cannot be empty")
        }

        self.call_rpc(
            "node.exec_shell",
            serde_json::to_value(ExecShellParams {
                cmd: cmd.to_string(),
                timeout_ms,
                cwd: cwd.map(ToOwned::to_owned),
                env,
                async_exec: Some(true),
                callback_url: callback_url.map(ToOwned::to_owned),
            })?,
        )
        .await
    }

    pub async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<ReadFileResult> {
        self.call_rpc(
            "node.read_file",
            serde_json::to_value(ReadFileParams {
                path: path.to_string(),
                offset,
                limit,
            })?,
        )
        .await
    }

    pub async fn write_file(
        &self,
        path: &str,
        content: &str,
        create_dirs: bool,
    ) -> Result<WriteFileResult> {
        self.call_rpc(
            "node.write_file",
            serde_json::to_value(WriteFileParams {
                path: path.to_string(),
                content: content.to_string(),
                create_dirs,
            })?,
        )
        .await
    }

    pub async fn cancel(&self, task_id: &str) -> Result<()> {
        if task_id.trim().is_empty() {
            return Err(anyhow!("task_id cannot be empty"));
        }

        let _: serde_json::Value = self
            .call_rpc(
                "node.cancel",
                serde_json::to_value(CancelParams {
                    task_id: task_id.to_string(),
                })?,
            )
            .await?;
        Ok(())
    }

    pub async fn task_status(&self, task_id: &str) -> Result<TaskStatusResult> {
        if task_id.trim().is_empty() {
            return Err(anyhow!("task_id cannot be empty"));
        }

        self.call_rpc(
            "node.task_status",
            serde_json::to_value(TaskStatusParams {
                task_id: task_id.to_string(),
            })?,
        )
        .await
    }

    pub async fn task_list(&self) -> Result<TaskListResult> {
        self.call_rpc("node.task_list", serde_json::json!({})).await
    }

    pub async fn metrics(&self) -> Result<MetricsResult> {
        self.call_rpc("node.metrics", serde_json::json!({})).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockTransport {
        failures: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeTransport for MockTransport {
        async fn call(&self, _request: &TransportRequest) -> Result<serde_json::Value> {
            let current = self.failures.fetch_add(1, Ordering::SeqCst);
            if current < 3 {
                Err(anyhow!("simulated failure"))
            } else {
                Ok(serde_json::json!({"message": "pong", "timestamp": chrono::Utc::now()}))
            }
        }
    }

    fn mock_node() -> RemoteNodeConfig {
        RemoteNodeConfig {
            id: "n1".into(),
            endpoint: "http://127.0.0.1:7878".into(),
            bearer_token: "token".into(),
            hmac_secret: None,
            enabled: true,
            timeout_ms: None,
            retry_max: None,
        }
    }

    #[tokio::test]
    async fn circuit_breaker_blocks_after_threshold() {
        let transport = Arc::new(MockTransport {
            failures: Arc::new(AtomicUsize::new(0)),
        });
        let client = RemoteNodeClient::new(mock_node(), transport);

        assert!(client.ping().await.is_err());
        assert!(client.ping().await.is_err());
        assert!(client.ping().await.is_err());
        let blocked = client.ping().await;
        assert!(blocked.is_err());
        assert!(blocked
            .unwrap_err()
            .to_string()
            .contains("temporarily unhealthy"));
    }
}
