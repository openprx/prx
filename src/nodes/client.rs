use crate::config::RemoteNodeConfig;
use crate::nodes::protocol::{
    AsyncTaskAccepted, CancelParams, ExecShellParams, ExecShellResult, MetricsResult, PingResult, ReadFileParams,
    ReadFileResult, TaskListResult, TaskStatusParams, TaskStatusResult, WriteFileParams, WriteFileResult,
};
use crate::nodes::transport::{NodeTransport, TransportRequest};
use anyhow::{Context, Result, anyhow, bail};
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

    async fn call_rpc<T: serde::de::DeserializeOwned>(&self, method: &str, params: serde_json::Value) -> Result<T> {
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

    pub async fn exec_shell(&self, cmd: &str, timeout_ms: Option<u64>, cwd: Option<&str>) -> Result<ExecShellResult> {
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

    pub async fn read_file(&self, path: &str, offset: Option<u64>, limit: Option<u64>) -> Result<ReadFileResult> {
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

    pub async fn write_file(&self, path: &str, content: &str, create_dirs: bool) -> Result<WriteFileResult> {
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
    use parking_lot::Mutex as ParkingMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Mock transports ─────────────────────────────────────────

    /// Always fails for the first N calls, then succeeds with a pong.
    struct FailThenSucceedTransport {
        call_count: Arc<AtomicUsize>,
        fail_until: usize,
    }

    #[async_trait]
    impl NodeTransport for FailThenSucceedTransport {
        async fn call(&self, _request: &TransportRequest) -> Result<serde_json::Value> {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_until {
                Err(anyhow!("simulated failure"))
            } else {
                Ok(serde_json::json!({"message": "pong", "timestamp": chrono::Utc::now()}))
            }
        }
    }

    /// Always succeeds, echoing back the method and params.
    struct EchoTransport {
        calls: Arc<ParkingMutex<Vec<String>>>,
    }

    impl EchoTransport {
        fn new() -> (Arc<Self>, Arc<ParkingMutex<Vec<String>>>) {
            let calls = Arc::new(ParkingMutex::new(Vec::new()));
            (Arc::new(Self { calls: calls.clone() }), calls)
        }
    }

    #[async_trait]
    impl NodeTransport for EchoTransport {
        async fn call(&self, request: &TransportRequest) -> Result<serde_json::Value> {
            self.calls.lock().push(request.method.clone());
            // Return a generic success response matching common result types
            Ok(serde_json::json!({
                "message": "pong",
                "timestamp": chrono::Utc::now(),
                "task_id": "t1",
                "exit_code": 0,
                "stdout": "ok",
                "stderr": "",
                "duration_ms": 10,
                "timed_out": false,
                "cancelled": false,
                "path": "/tmp/f",
                "content": "data",
                "bytes_read": 4,
                "bytes_written": 4,
                "offset": 0,
                "eof": true,
                "created_dirs": false,
                "cpu_cores": 4,
                "tasks": []
            }))
        }
    }

    /// Always fails.
    struct AlwaysFailTransport;

    #[async_trait]
    impl NodeTransport for AlwaysFailTransport {
        async fn call(&self, _request: &TransportRequest) -> Result<serde_json::Value> {
            Err(anyhow!("network error"))
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

    // ── node_id ─────────────────────────────────────────────────

    #[test]
    fn node_id_matches_config() {
        let (transport, _) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        assert_eq!(client.node_id(), "n1");
    }

    // ── is_healthy ──────────────────────────────────────────────

    #[tokio::test]
    async fn healthy_initially() {
        let (transport, _) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        assert!(client.is_healthy().await);
    }

    // ── circuit breaker ─────────────────────────────────────────

    #[tokio::test]
    async fn circuit_breaker_blocks_after_threshold() {
        let transport = Arc::new(FailThenSucceedTransport {
            call_count: Arc::new(AtomicUsize::new(0)),
            fail_until: 100, // always fail
        });
        let client = RemoteNodeClient::new(mock_node(), transport);

        // 3 failures → circuit opens
        for _ in 0..FAILURE_THRESHOLD {
            let _ = client.ping().await;
        }
        let blocked = client.ping().await;
        assert!(blocked.is_err());
        assert!(blocked.unwrap_err().to_string().contains("temporarily unhealthy"));
    }

    #[tokio::test]
    async fn circuit_breaker_resets_on_success() {
        let transport = Arc::new(FailThenSucceedTransport {
            call_count: Arc::new(AtomicUsize::new(0)),
            fail_until: 1, // fail once, then succeed
        });
        let client = RemoteNodeClient::new(mock_node(), transport);

        // First call fails, records 1 failure
        assert!(client.ping().await.is_err());
        // Second call succeeds, resets counter
        assert!(client.ping().await.is_ok());
        // Should still be healthy
        assert!(client.is_healthy().await);
    }

    // ── ping ────────────────────────────────────────────────────

    #[tokio::test]
    async fn ping_success_returns_duration() {
        let (transport, _) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        let duration = client.ping().await.unwrap();
        assert!(duration.as_millis() < 1000);
    }

    #[tokio::test]
    async fn ping_failure_propagates() {
        let client = RemoteNodeClient::new(mock_node(), Arc::new(AlwaysFailTransport));
        assert!(client.ping().await.is_err());
    }

    // ── exec_shell ──────────────────────────────────────────────

    #[tokio::test]
    async fn exec_shell_empty_command_fails() {
        let (transport, _) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        let err = client.exec_shell("", None, None).await.unwrap_err();
        assert!(err.to_string().contains("cannot be empty"));
    }

    #[tokio::test]
    async fn exec_shell_whitespace_command_fails() {
        let (transport, _) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        assert!(client.exec_shell("   ", None, None).await.is_err());
    }

    #[tokio::test]
    async fn exec_shell_calls_correct_method() {
        let (transport, calls) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        let _ = client.exec_shell("ls", None, None).await;
        assert!(calls.lock().contains(&"node.exec_shell".to_string()));
    }

    // ── cancel ──────────────────────────────────────────────────

    #[tokio::test]
    async fn cancel_empty_task_id_fails() {
        let (transport, _) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        let err = client.cancel("").await.unwrap_err();
        assert!(err.to_string().contains("task_id cannot be empty"));
    }

    // ── task_status ─────────────────────────────────────────────

    #[tokio::test]
    async fn task_status_empty_id_fails() {
        let (transport, _) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        assert!(client.task_status("").await.is_err());
    }

    // ── read_file / write_file ──────────────────────────────────

    #[tokio::test]
    async fn read_file_calls_correct_method() {
        let (transport, calls) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        let _ = client.read_file("/etc/hosts", None, None).await;
        assert!(calls.lock().contains(&"node.read_file".to_string()));
    }

    #[tokio::test]
    async fn write_file_calls_correct_method() {
        let (transport, calls) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        let _ = client.write_file("/tmp/f", "data", false).await;
        assert!(calls.lock().contains(&"node.write_file".to_string()));
    }

    // ── metrics / task_list ─────────────────────────────────────

    #[tokio::test]
    async fn metrics_calls_correct_method() {
        let (transport, calls) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        let _ = client.metrics().await;
        assert!(calls.lock().contains(&"node.metrics".to_string()));
    }

    #[tokio::test]
    async fn task_list_calls_correct_method() {
        let (transport, calls) = EchoTransport::new();
        let client = RemoteNodeClient::new(mock_node(), transport);
        let _ = client.task_list().await;
        assert!(calls.lock().contains(&"node.task_list".to_string()));
    }

    // ── CircuitBreakerState unit tests ──────────────────────────

    #[test]
    fn circuit_breaker_new_allows_requests() {
        let cb = CircuitBreakerState::new();
        assert!(cb.allow_request().is_ok());
    }

    #[test]
    fn circuit_breaker_success_resets_counter() {
        let mut cb = CircuitBreakerState::new();
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        assert_eq!(cb.consecutive_failures, 0);
        assert!(cb.unhealthy_until.is_none());
    }

    #[test]
    fn circuit_breaker_opens_at_threshold() {
        let mut cb = CircuitBreakerState::new();
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_failure();
        }
        // After threshold, unhealthy_until is set
        assert!(cb.allow_request().is_err());
    }
}
