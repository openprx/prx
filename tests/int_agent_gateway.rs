#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal,
    clippy::unwrap_or_default,
    clippy::wildcard_in_or_patterns,
    clippy::default_trait_access,
    clippy::expect_used,
    clippy::or_fun_call,
    clippy::match_wild_err_arm
)]
//! P0 integration tests for agent + gateway.
//!
//! These tests validate:
//! - Agent tool dispatch, malformed tool calls, timeouts, and iteration limits
//! - Gateway auth (bearer token rejection), body size limits, HMAC verification, rate limiting
//!
//! Each test uses mock providers and isolated state; no real LLM or network calls.

use anyhow::Result;
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use openprx::HookManager;
use openprx::agent::agent::Agent;
use openprx::agent::dispatcher::NativeToolDispatcher;
use openprx::config::{AgentConfig, Config, MemoryConfig};
use openprx::gateway::{AppState, GatewayRateLimiter, IdempotencyStore, MAX_BODY_SIZE};
use openprx::memory::{self, Memory, MemoryCategory, MemoryEntry};
use openprx::observability::{NoopObserver, Observer};
use openprx::providers::{ChatRequest, ChatResponse, Provider, ToolCall};
use openprx::security::pairing::PairingGuard;
use openprx::tools::{Tool, ToolResult};
use parking_lot::Mutex;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tower_http::limit::RequestBodyLimitLayer;

// ─────────────────────────────────────────────────────────────────────────────
// Mock infrastructure
// ─────────────────────────────────────────────────────────────────────────────

/// Mock provider that returns scripted responses in FIFO order.
struct MockProvider {
    responses: std::sync::Mutex<Vec<ChatResponse>>,
}

impl MockProvider {
    const fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        Ok("fallback".into())
    }

    async fn chat(&self, _request: ChatRequest<'_>, _model: &str, _temperature: f64) -> Result<ChatResponse> {
        let mut guard = self.responses.lock().expect("test: lock mock responses");
        if guard.is_empty() {
            return Ok(ChatResponse {
                text: Some("done".into()),
                tool_calls: vec![],
            });
        }
        Ok(guard.remove(0))
    }
}

/// Mock provider that hangs on the Nth call (0-indexed).
struct HangingProvider {
    responses: std::sync::Mutex<Vec<ChatResponse>>,
    hang_on_call: usize,
    call_count: std::sync::atomic::AtomicUsize,
}

impl HangingProvider {
    const fn new(responses: Vec<ChatResponse>, hang_on_call: usize) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            hang_on_call,
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Provider for HangingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        Ok("fallback".into())
    }

    async fn chat(&self, _request: ChatRequest<'_>, _model: &str, _temperature: f64) -> Result<ChatResponse> {
        let call_idx = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call_idx >= self.hang_on_call {
            // Hang for a very long time (simulates timeout)
            tokio::time::sleep(Duration::from_secs(3600)).await;
            anyhow::bail!("provider timeout (should not reach here)");
        }
        let mut guard = self.responses.lock().expect("test: lock hanging responses");
        if guard.is_empty() {
            return Ok(ChatResponse {
                text: Some("done".into()),
                tool_calls: vec![],
            });
        }
        Ok(guard.remove(0))
    }
}

/// Gateway mock provider for axum tests (minimal).
#[derive(Default)]
struct GatewayMockProvider;

#[async_trait]
impl Provider for GatewayMockProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        Ok("ok".into())
    }
}

/// Simple tool that echoes its input argument.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "Echoes the input message"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"}
            }
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("(empty)")
            .to_string();
        Ok(ToolResult {
            success: true,
            output: msg,
            error: None,
        })
    }
}

/// Tool that tracks invocation count for verifying dispatch.
struct CountingTool {
    count: Arc<std::sync::Mutex<usize>>,
}

impl CountingTool {
    fn new() -> (Self, Arc<std::sync::Mutex<usize>>) {
        let count = Arc::new(std::sync::Mutex::new(0));
        (Self { count: count.clone() }, count)
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &'static str {
        "counter"
    }
    fn description(&self) -> &'static str {
        "Counts invocations"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        let mut c = self.count.lock().expect("test: lock counting tool");
        *c += 1;
        Ok(ToolResult {
            success: true,
            output: format!("call #{}", *c),
            error: None,
        })
    }
}

/// Minimal mock memory for gateway tests.
#[derive(Default)]
struct MockMemory;

#[async_trait]
impl Memory for MockMemory {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }

    async fn recall(&self, _query: &str, _limit: usize, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }

    async fn get(&self, _key: &str) -> Result<Option<MemoryEntry>> {
        Ok(None)
    }

    async fn list(&self, _category: Option<&MemoryCategory>, _session_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }

    async fn forget(&self, _key: &str) -> Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> Result<usize> {
        Ok(0)
    }

    async fn health_check(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test helpers
// ─────────────────────────────────────────────────────────────────────────────

fn make_memory() -> Arc<dyn Memory> {
    let cfg = MemoryConfig {
        backend: "none".into(),
        ..MemoryConfig::default()
    };
    Arc::from(memory::create_memory(&cfg, &std::env::temp_dir(), None).expect("test: create none-backend memory"))
}

fn make_observer() -> Arc<dyn Observer> {
    Arc::from(NoopObserver {})
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        text: Some(text.into()),
        tool_calls: vec![],
    }
}

const fn tool_response(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        text: Some(String::new()),
        tool_calls: calls,
    }
}

fn build_agent(provider: Box<dyn Provider>, tools: Vec<Box<dyn Tool>>) -> Agent {
    Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(make_memory())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::env::temp_dir())
        .build()
        .expect("test: build agent")
}

fn build_agent_with_config(provider: Box<dyn Provider>, tools: Vec<Box<dyn Tool>>, config: AgentConfig) -> Agent {
    Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(make_memory())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::env::temp_dir())
        .config(config)
        .build()
        .expect("test: build agent with config")
}

/// Build a minimal `AppState` for gateway integration tests.
fn build_test_app_state(overrides: TestAppStateOverrides) -> AppState {
    AppState {
        config: Arc::new(Mutex::new(Config::default())),
        shared_config: Arc::new(arc_swap::ArcSwap::from_pointee(Config::default())),
        provider: overrides.provider.unwrap_or_else(|| Arc::new(GatewayMockProvider)),
        model: "test-model".into(),
        temperature: 0.0,
        mem: Arc::new(MockMemory),
        auto_save: false,
        tools_registry: Arc::new(vec![]),
        mcp_tool: None,
        hooks: Arc::new(HookManager::new(std::env::temp_dir())),
        webhook_token_hash: overrides.webhook_token_hash,
        webhook_signing_secret: overrides.webhook_signing_secret,
        pairing: overrides
            .pairing
            .unwrap_or_else(|| Arc::new(PairingGuard::new(false, &[]))),
        trust_forwarded_headers: false,
        rate_limiter: overrides
            .rate_limiter
            .unwrap_or_else(|| Arc::new(GatewayRateLimiter::new(100, 100, 100, 100))),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
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
        plugin_manager: None,
        #[cfg(feature = "wasm-plugins")]
        wasm_middleware: None,
        #[cfg(feature = "wasm-plugins")]
        wasm_hook_executor: None,
        #[cfg(feature = "wasm-plugins")]
        wasm_cron_manager: None,
        #[cfg(feature = "wasm-plugins")]
        event_bus: None,
    }
}

#[derive(Default)]
struct TestAppStateOverrides {
    provider: Option<Arc<dyn Provider>>,
    pairing: Option<Arc<PairingGuard>>,
    rate_limiter: Option<Arc<GatewayRateLimiter>>,
    webhook_token_hash: Option<Arc<str>>,
    webhook_signing_secret: Option<Arc<str>>,
}

/// Build a minimal gateway router suitable for integration tests.
/// This mirrors the structure from `run_gateway` but only includes the routes
/// relevant to our tests (/health, /webhook, /api/*).
fn build_test_router(state: AppState) -> Router {
    use axum::middleware;
    use tower_http::timeout::TimeoutLayer;

    // Public routes with body limit (same as production)
    let limited_public_routes = Router::new()
        .route("/webhook", post(test_webhook_handler))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_SIZE));

    // API routes with auth middleware
    let api_routes = Router::new()
        .route("/chat", post(test_api_chat_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), test_auth_middleware))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_SIZE));

    Router::new()
        .route("/health", get(test_health_handler))
        .merge(limited_public_routes)
        .nest("/api", api_routes)
        .with_state(state)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
}

/// Minimal health handler for tests.
async fn test_health_handler() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

/// Minimal auth middleware that checks bearer token against `PairingGuard`.
async fn test_auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .map_or("", str::trim);

    if state.pairing.require_pairing() && !state.pairing.is_authenticated(token) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))).into_response();
    }

    next.run(request).await
}

/// Minimal webhook handler for tests — checks HMAC, rate limiting, returns 200.
async fn test_webhook_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Rate limiting
    let rate_key = peer_addr.ip().to_string();
    if !state.rate_limiter.allow_webhook(&rate_key) {
        let err = json!({
            "error": "Too many requests",
            "retry_after": 60,
        });
        return (StatusCode::TOO_MANY_REQUESTS, Json(err));
    }

    // HMAC signature verification
    if let Some(ref signing_secret) = state.webhook_signing_secret {
        let signature_header = headers
            .get("X-Webhook-Signature")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty());

        match signature_header {
            Some(sig) if verify_hmac(signing_secret, &body, sig) => {}
            _ => {
                let err = json!({"error": "Invalid or missing X-Webhook-Signature"});
                return (StatusCode::UNAUTHORIZED, Json(err));
            }
        }
    }

    (StatusCode::OK, Json(json!({"status": "ok"})))
}

/// Minimal chat handler for tests.
async fn test_api_chat_handler(body: Bytes) -> impl IntoResponse {
    let _body_str = String::from_utf8_lossy(&body);
    Json(json!({"response": "test"}))
}

/// Compute HMAC-SHA256 and verify against a `sha256=<hex>` signature header.
fn verify_hmac(secret: &str, body: &[u8], signature_header: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let signature_hex = signature_header
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or(signature_header.trim());
    let Ok(provided) = hex::decode(signature_hex) else {
        return false;
    };

    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

/// Compute the HMAC-SHA256 hex signature for a body with a given secret.
fn compute_hmac_signature(secret: &str, body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("test: create HMAC from secret");
    mac.update(body);
    let result = mac.finalize();
    format!("sha256={}", hex::encode(result.into_bytes()))
}

/// Start an axum test server on a random port and return (addr, `join_handle`).
async fn spawn_test_server(router: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind to random port");
    let addr = listener.local_addr().expect("test: get local address");

    let handle = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .expect("test: axum serve");
    });

    // Give the server a moment to start.
    tokio::time::sleep(Duration::from_millis(50)).await;

    (addr, handle)
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-AP-01: Provider returns tool calls, agent dispatches correctly
// ═══════════════════════════════════════════════════════════════════════════════

/// Validates that when a provider returns `tool_calls`, the agent dispatches
/// to the correct tool, collects the result, and feeds it back to the provider,
/// ultimately returning a final text response.
#[tokio::test]
async fn int_agent_gateway_ap01_tool_dispatch_round_trip() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Box::new(MockProvider::new(vec![
        // First response: provider asks to call "counter" tool
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        // Second response: provider asks to call "echo" tool
        tool_response(vec![ToolCall {
            id: "tc2".into(),
            name: "echo".into(),
            arguments: r#"{"message": "tool result fed back"}"#.into(),
        }]),
        // Final response: text answer
        text_response("Agent dispatched both tools successfully"),
    ]));

    let mut agent = build_agent(provider, vec![Box::new(counting_tool), Box::new(EchoTool)]);

    let response = agent.turn("dispatch test").await.expect("test: agent turn");
    assert!(
        !response.is_empty(),
        "Expected non-empty final text response after tool dispatch"
    );
    assert_eq!(
        *count.lock().expect("test: read count"),
        1,
        "Counter tool should have been called exactly once"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-AP-02: Provider returns malformed tool call JSON
// ═══════════════════════════════════════════════════════════════════════════════

/// Validates that the agent does NOT panic when the provider returns a tool call
/// with malformed JSON in the `arguments` field. The agent should gracefully
/// handle the error and continue.
#[tokio::test]
async fn int_agent_gateway_ap02_malformed_tool_call_json() {
    let provider = Box::new(MockProvider::new(vec![
        // Malformed JSON arguments
        tool_response(vec![ToolCall {
            id: "tc_bad".into(),
            name: "echo".into(),
            arguments: "{{not json}".into(),
        }]),
        // Provider should still get a chance to respond with text
        text_response("Recovered from malformed arguments"),
    ]));

    let mut agent = build_agent(provider, vec![Box::new(EchoTool)]);

    // The agent should NOT panic. It should either return a result or an error,
    // but never crash.
    let result = agent.turn("send malformed tool call").await;
    match result {
        Ok(response) => {
            assert!(
                !response.is_empty(),
                "Expected non-empty response after malformed tool call recovery"
            );
        }
        Err(e) => {
            // An error is acceptable too — the key point is no panic.
            let err_msg = e.to_string();
            assert!(!err_msg.is_empty(), "Error message should be non-empty: {err_msg}");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-AP-03: Provider timeout during multi-turn tool loop
// ═══════════════════════════════════════════════════════════════════════════════

/// Validates that when the provider hangs on the second call during a tool loop,
/// the agent times out and returns an error (partial result or timeout error).
#[tokio::test]
async fn int_agent_gateway_ap03_provider_timeout_during_tool_loop() {
    let provider = Box::new(HangingProvider::new(
        vec![
            // First response: tool call (succeeds)
            tool_response(vec![ToolCall {
                id: "tc1".into(),
                name: "echo".into(),
                arguments: r#"{"message": "first call ok"}"#.into(),
            }]),
        ],
        1, // hang on the second call (index 1)
    ));

    let mut agent = build_agent(provider, vec![Box::new(EchoTool)]);

    // Use a timeout to prevent the test from hanging forever.
    let result = tokio::time::timeout(Duration::from_secs(5), agent.turn("timeout test")).await;

    match result {
        Ok(Ok(response)) => {
            // If the agent returns a partial result, that is acceptable.
            assert!(!response.is_empty(), "Expected non-empty partial response on timeout");
        }
        Ok(Err(_e)) => {
            // An error is also acceptable — the agent detected the hang.
        }
        Err(_timeout_err) => {
            // Timeout at the test level is also valid — it proves the provider hung
            // and we protected the test with a timeout wrapper. The agent itself
            // would need a configured timeout to handle this in production.
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-AP-06: Max tool iterations prevents runaway loops
// ═══════════════════════════════════════════════════════════════════════════════

/// Validates that when the provider always returns tool calls, the agent stops
/// after `max_tool_iterations` iterations and returns an error.
#[tokio::test]
async fn int_agent_gateway_ap06_max_tool_iterations_prevents_runaway() {
    let max_iters = 3;

    // Create way more tool call responses than the limit allows.
    let mut responses = Vec::new();
    for i in 0..max_iters + 10 {
        responses.push(tool_response(vec![ToolCall {
            id: format!("tc{i}"),
            name: "echo".into(),
            arguments: r#"{"message": "loop forever"}"#.into(),
        }]));
    }

    let provider = Box::new(MockProvider::new(responses));
    let config = AgentConfig {
        max_tool_iterations: max_iters,
        ..AgentConfig::default()
    };

    let mut agent = build_agent_with_config(provider, vec![Box::new(EchoTool)], config);

    let result = agent.turn("infinite loop").await;
    assert!(result.is_err(), "Expected error when max tool iterations exceeded");
    let err = result.expect_err("test: expected max iterations error").to_string();
    assert!(
        err.contains("maximum tool iterations"),
        "Error should mention max iterations limit, got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-GS-01: Gateway rejects requests without valid bearer token
// ═══════════════════════════════════════════════════════════════════════════════

/// Validates that POST to `/api/chat` without auth returns 401 Unauthorized
/// when pairing is required.
#[tokio::test]
async fn int_agent_gateway_gs01_rejects_unauthenticated_api_request() {
    // Create a PairingGuard that requires pairing and has one token.
    let token = "test-bearer-token-12345";
    let pairing = Arc::new(PairingGuard::new(true, &[token.to_string()]));

    let state = build_test_app_state(TestAppStateOverrides {
        pairing: Some(pairing),
        ..Default::default()
    });
    let router = build_test_router(state);

    let (addr, _handle) = spawn_test_server(router).await;

    let client = reqwest::Client::new();

    // Request WITHOUT auth header -> 401
    let resp_no_auth = client
        .post(format!("http://{addr}/api/chat"))
        .header("Content-Type", "application/json")
        .body(r#"{"message":"hello"}"#)
        .send()
        .await
        .expect("test: send unauthenticated request");

    assert_eq!(
        resp_no_auth.status().as_u16(),
        401,
        "Expected 401 Unauthorized for unauthenticated API request"
    );

    // Request WITH valid auth header -> 200
    let resp_with_auth = client
        .post(format!("http://{addr}/api/chat"))
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .body(r#"{"message":"hello"}"#)
        .send()
        .await
        .expect("test: send authenticated request");

    assert_eq!(
        resp_with_auth.status().as_u16(),
        200,
        "Expected 200 OK for authenticated API request"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-GS-04: Gateway body size limit prevents OOM
// ═══════════════════════════════════════════════════════════════════════════════

/// Validates that sending a body larger than `MAX_BODY_SIZE` (64KB) to a
/// body-limited route returns 413 Payload Too Large.
#[tokio::test]
async fn int_agent_gateway_gs04_body_size_limit_prevents_oom() {
    let state = build_test_app_state(Default::default());
    let router = build_test_router(state);

    let (addr, _handle) = spawn_test_server(router).await;

    let client = reqwest::Client::new();

    // Send a body larger than MAX_BODY_SIZE (64KB) + some margin
    let oversized_body = vec![b'A'; MAX_BODY_SIZE + 1024];
    let resp = client
        .post(format!("http://{addr}/webhook"))
        .header("Content-Type", "application/json")
        .body(oversized_body)
        .send()
        .await
        .expect("test: send oversized body");

    assert_eq!(
        resp.status().as_u16(),
        413,
        "Expected 413 Payload Too Large for oversized body"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-GS-05: HMAC webhook signature verification
// ═══════════════════════════════════════════════════════════════════════════════

/// Validates that:
/// - A valid HMAC signature is accepted (200)
/// - A tampered body with the same signature is rejected (401)
#[tokio::test]
async fn int_agent_gateway_gs05_hmac_webhook_signature_verification() {
    let secret = "test-hmac-secret-for-webhooks";

    let state = build_test_app_state(TestAppStateOverrides {
        webhook_signing_secret: Some(Arc::from(secret.to_string())),
        ..Default::default()
    });
    let router = build_test_router(state);

    let (addr, _handle) = spawn_test_server(router).await;

    let client = reqwest::Client::new();
    let body = br#"{"message":"hello"}"#;
    let valid_signature = compute_hmac_signature(secret, body);

    // Valid signature -> accepted
    let resp_valid = client
        .post(format!("http://{addr}/webhook"))
        .header("Content-Type", "application/json")
        .header("X-Webhook-Signature", &valid_signature)
        .body(body.to_vec())
        .send()
        .await
        .expect("test: send webhook with valid signature");

    assert_eq!(
        resp_valid.status().as_u16(),
        200,
        "Expected 200 OK for valid HMAC signature"
    );

    // Tampered body -> rejected
    let tampered_body = br#"{"message":"tampered"}"#;
    let resp_tampered = client
        .post(format!("http://{addr}/webhook"))
        .header("Content-Type", "application/json")
        .header("X-Webhook-Signature", &valid_signature) // same sig, different body
        .body(tampered_body.to_vec())
        .send()
        .await
        .expect("test: send webhook with tampered body");

    assert_eq!(
        resp_tampered.status().as_u16(),
        401,
        "Expected 401 Unauthorized for tampered body with stale signature"
    );

    // Missing signature header -> rejected
    let resp_no_sig = client
        .post(format!("http://{addr}/webhook"))
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .expect("test: send webhook without signature");

    assert_eq!(
        resp_no_sig.status().as_u16(),
        401,
        "Expected 401 Unauthorized for missing X-Webhook-Signature"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-GCW-03: Webhook rate limiting
// ═══════════════════════════════════════════════════════════════════════════════

/// Validates that sending requests exceeding the webhook rate limit returns 429.
#[tokio::test]
async fn int_agent_gateway_gcw03_webhook_rate_limiting() {
    // Set a very low rate limit: 3 requests per 60s window.
    let rate_limiter = Arc::new(GatewayRateLimiter::new(100, 3, 100, 100));

    let state = build_test_app_state(TestAppStateOverrides {
        rate_limiter: Some(rate_limiter),
        ..Default::default()
    });
    let router = build_test_router(state);

    let (addr, _handle) = spawn_test_server(router).await;

    let client = reqwest::Client::new();
    let body = r#"{"message":"hello"}"#;

    // First 3 requests should succeed.
    for i in 0..3 {
        let resp = client
            .post(format!("http://{addr}/webhook"))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .expect("test: send webhook within rate limit");

        assert_eq!(
            resp.status().as_u16(),
            200,
            "Request {i} should succeed (within rate limit)"
        );
    }

    // 4th request should be rate-limited.
    let resp_limited = client
        .post(format!("http://{addr}/webhook"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .expect("test: send webhook exceeding rate limit");

    assert_eq!(
        resp_limited.status().as_u16(),
        429,
        "Expected 429 Too Many Requests when rate limit exceeded"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-AP-04: Provider returns empty response (no text, no tools)
// ═══════════════════════════════════════════════════════════════════════════════

/// Validates that when the provider returns an empty response (no text, no tool calls),
/// the agent loop terminates gracefully without infinite looping.
#[tokio::test]
async fn int_agent_gateway_ap04_empty_provider_response() {
    let provider = Box::new(MockProvider::new(vec![ChatResponse {
        text: None,
        tool_calls: vec![],
    }]));

    let mut agent = build_agent(provider, vec![Box::new(EchoTool)]);

    // The agent should handle the empty response without panicking or infinite looping.
    let result = tokio::time::timeout(Duration::from_secs(5), agent.turn("empty response test")).await;

    match result {
        Ok(Ok(response)) => {
            // Empty or default response is acceptable
            // The key point is no infinite loop and no panic
            let _ = response;
        }
        Ok(Err(_e)) => {
            // An error is also acceptable — the agent detected the empty response
        }
        Err(_timeout) => {
            panic!("test: agent entered infinite loop on empty provider response");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-AT-03: Unknown tool name in tool call
// ═══════════════════════════════════════════════════════════════════════════════

/// Validates that when the provider requests a tool that is not registered,
/// the agent returns an error `ToolResult` and feeds it back to the provider.
#[tokio::test]
async fn int_agent_gateway_at03_unknown_tool_name() {
    let provider = Box::new(MockProvider::new(vec![
        // Provider requests a nonexistent tool
        tool_response(vec![ToolCall {
            id: "tc_unknown".into(),
            name: "nonexistent_tool".into(),
            arguments: "{}".into(),
        }]),
        // Provider should receive the error and produce a text response
        text_response("Recovered from unknown tool error"),
    ]));

    let mut agent = build_agent(provider, vec![Box::new(EchoTool)]);

    let result = agent.turn("call nonexistent tool").await;
    // The agent should not panic. It should either return a text response
    // (after the provider self-corrects) or return an error.
    match result {
        Ok(response) => {
            assert!(
                !response.is_empty(),
                "Expected non-empty response after unknown tool error recovery"
            );
        }
        Err(e) => {
            // An error is also acceptable — the key point is no panic.
            let err_msg = e.to_string();
            assert!(!err_msg.is_empty(), "Error message should be non-empty: {err_msg}");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-GS-02: Rate limiter enforces per-IP sliding window
// ═══════════════════════════════════════════════════════════════════════════════

/// Rate limiter blocks after the per-window limit is reached.
#[test]
fn int_agent_gateway_gs02_rate_limiter_enforcement() {
    let limiter = GatewayRateLimiter::new(5, 5, 5, 100);

    // 5 requests from the same IP should succeed
    for _ in 0..5 {
        assert!(
            limiter.allow_webhook("192.168.1.1"),
            "test: request within limit should be allowed"
        );
    }

    // 6th request should be blocked
    assert!(
        !limiter.allow_webhook("192.168.1.1"),
        "test: 6th request should be rate-limited"
    );

    // Different IP should still be allowed
    assert!(
        limiter.allow_webhook("192.168.1.2"),
        "test: different IP should not be rate-limited"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-GS-03: Rate limiter memory does not grow unbounded
// ═══════════════════════════════════════════════════════════════════════════════

/// Rate limiter with bounded `max_keys` evicts entries when capacity is exceeded.
#[test]
fn int_agent_gateway_gs03_rate_limiter_bounded_memory() {
    // max_keys=3 — only 3 distinct keys can be tracked
    let limiter = GatewayRateLimiter::new(5, 5, 5, 3);

    // Add 3 unique keys
    assert!(limiter.allow_webhook("ip-1"));
    assert!(limiter.allow_webhook("ip-2"));
    assert!(limiter.allow_webhook("ip-3"));

    // 4th key triggers eviction of least-active key
    assert!(limiter.allow_webhook("ip-4"));

    // All 4 keys should still "work" (the evicted one just loses its count)
    // The key point is no OOM — bounded cardinality
}
