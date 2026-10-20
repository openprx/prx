//! Test harness of the management endpoints: a configuration saved to a
//! temp dir through the real save path, served by the real API router with
//! its auth and rate-limit layers on an ephemeral loopback port.

#![cfg(test)]

use super::AppState;
use crate::config::Config;
use crate::gateway::{GatewayRateLimiter, IdempotencyStore};
use crate::hooks::HookManager;
use crate::memory::SqliteMemory;
use crate::observability::NoopObserver;
use crate::providers::Provider;
use crate::security::pairing::PairingGuard;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::instrument::WithSubscriber as _;

/// The bearer token the served router accepts. Not a real credential.
pub(super) const TOKEN: &str = "zc_management_api_test_token";

/// Provider the gateway state needs but these tests never reach.
struct UnusedProvider;

#[async_trait]
impl Provider for UnusedProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("test: the provider is never called"))
    }

    async fn chat(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        Err(anyhow::anyhow!("test: the provider is never called"))
    }
}

fn app_state(config: Config) -> AppState {
    let workspace = config.workspace_dir.clone();
    std::fs::create_dir_all(&workspace).expect("test: workspace dir");
    let memory = SqliteMemory::new(&workspace).expect("test: sqlite memory");
    AppState {
        config: crate::config::new_shared(config),
        provider: Arc::new(UnusedProvider),
        model: "test-model".into(),
        temperature: 0.0,
        mem: Arc::new(memory),
        auto_save: false,
        tools_registry: Arc::new(vec![]),
        mcp_tool: None,
        turn_runtime: None,
        hooks: Arc::new(HookManager::new(workspace)),
        webhook_token_hash: None,
        webhook_signing_secret: None,
        pairing: Arc::new(PairingGuard::new(true, &[TOKEN.to_string()])),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(10_000, 10_000, 10_000, 10_000)),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_mins(5), 1000)),
        whatsapp: None,
        signal: None,
        whatsapp_app_secret: None,
        linq: None,
        linq_signing_secret: None,
        nextcloud_talk: None,
        nextcloud_talk_webhook_secret: None,
        observer: Arc::new(NoopObserver),
        start_time: Instant::now(),
        gateway_port: 0,
        logs_broadcast_tx: broadcast::channel(16).0,
        #[cfg(feature = "wasm-plugins")]
        plugin_runtime: None,
    }
}

/// A served configuration.
pub(super) struct Served {
    pub(super) dir: tempfile::TempDir,
    pub(super) state: AppState,
    pub(super) base: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Served {
    /// `config.toml` as stored.
    pub(super) fn disk(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.toml")).expect("test: read config.toml")
    }

    /// The configuration as the next load reads it.
    pub(super) fn loaded(&self) -> Config {
        Config::load_from_path(&self.dir.path().join("config.toml"), self.dir.path().join("workspace"))
            .expect("test: load")
    }

    /// A request with the bearer token; returns `(status, body)`.
    pub(super) async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        send(method, &format!("{}{path}", self.base), Some(TOKEN), None, body).await
    }

    /// A request with the bearer token and `If-Match: "<revision>"`.
    pub(super) async fn call_if_match(
        &self,
        method: reqwest::Method,
        path: &str,
        revision: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        send(
            method,
            &format!("{}{path}", self.base),
            Some(TOKEN),
            Some(revision),
            body,
        )
        .await
    }

    /// A request without any credential.
    pub(super) async fn call_anonymous(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        send(method, &format!("{}{path}", self.base), None, None, body).await
    }
}

/// Save `edit`ed defaults (default provider `openai`, model `gpt-4o`) to a
/// temp dir, load them back, and serve them.
pub(super) async fn serve_config(edit: impl FnOnce(&mut Config)) -> Served {
    let dir = tempfile::TempDir::new().expect("test: temp config dir");
    let mut config = Config {
        config_path: dir.path().join("config.toml"),
        workspace_dir: dir.path().join("workspace"),
        default_provider: Some("openai".into()),
        default_model: Some("gpt-4o".into()),
        ..Config::default()
    };
    edit(&mut config);
    config.save().await.expect("test: save initial config");
    let loaded =
        Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: load initial config");
    let state = app_state(loaded);
    // Axum serves connections on independent tasks. Bind the test dispatcher
    // on every request poll rather than relying on a thread-local default.
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let app = axum::Router::new()
        .nest("/api", super::router(state.clone()))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                next.run(request).with_subscriber(dispatch.clone())
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind ephemeral port");
    let port = listener.local_addr().expect("test: local addr").port();
    let server = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::warn!("test: management api server stopped: {error}");
        }
    });
    Served {
        dir,
        state,
        base: format!("http://127.0.0.1:{port}"),
        server,
    }
}

async fn send(
    method: reqwest::Method,
    url: &str,
    token: Option<&str>,
    if_match: Option<&str>,
    body: Option<serde_json::Value>,
) -> (u16, String) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("test: http client");
    let mut request = client.request(method, url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(revision) = if_match {
        request = request.header("If-Match", format!("\"{revision}\""));
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("test: request");
    let status = response.status().as_u16();
    let text = response.text().await.expect("test: response body");
    (status, text)
}

/// The JSON of a response body.
pub(super) fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|error| panic!("test: json body ({error}): {body}"))
}

/// The value at a JSON pointer (`/a/0/b`), `null` when absent.
pub(super) fn at(value: &serde_json::Value, pointer: &str) -> serde_json::Value {
    value.pointer(pointer).cloned().unwrap_or(serde_json::Value::Null)
}

/// A log sink for the current thread. `#[tokio::test]` runs the served
/// router on this same thread, so the handlers' events land here.
#[derive(Clone, Default)]
pub(super) struct LogSink(Arc<parking_lot::Mutex<Vec<u8>>>);

impl LogSink {
    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).to_string()
    }
}

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A loopback HTTP server that counts requests and answers each with a
/// one-model OpenAI-compatible list (`{"data":[{"id":"<model>"}]}`).
pub(super) struct CountingModelList {
    pub(super) port: u16,
    pub(super) requests: Arc<std::sync::atomic::AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for CountingModelList {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl CountingModelList {
    pub(super) async fn start(model: &str) -> Self {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind model list");
        let port = listener.local_addr().expect("test: local addr").port();
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let body = serde_json::json!({"data": [{"id": model}]}).to_string();
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buffer = [0_u8; 4096];
                let _ = stream.read(&mut buffer).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: \
                     close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            }
        });
        Self { port, requests, server }
    }

    pub(super) fn count(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }
}
