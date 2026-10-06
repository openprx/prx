//! CDP-07 and CDP-13: the compatible System One backend and the hosted
//! TypeSafe and OpenRouter adapters through the real factory and
//! transports, against loopback stubs only (SP-CDP-A 5.8, 5.9; SP-CDP-C
//! V2.01, V2.02, V2.05, V2.06, V6.08). A hosted backend reaches a stub
//! through its endpoint override (COORD-6). No test reaches a real network.
//!
//! Each stub is a raw TCP listener on 127.0.0.1 that records every request
//! and answers from a script, so a test can count requests, read headers
//! and see a connection close.

#![allow(
    clippy::doc_markdown,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    unsafe_code
)]

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use openprx::config::Config;
use openprx::decision::compatible_http::HttpDecisionTransports;
use openprx::decision::factory::{AttemptOutcome, DecisionBackendFactory, FactoryDeps, ServedBy};
use openprx::decision::telemetry::{CausalEvent, CausalEventSink, CausalLineage};
use openprx::decision::{
    DecisionError, DecisionKey, DecisionQuestion, DecisionRequest, GlobalQuestion, QuestionId, RequestLimits,
    RequestParts, StructuredValue, ValidationRule,
};
use openprx::providers::ProviderId;
use openprx::providers::catalog::{CatalogView, CuratedModel, ModelOperation};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const MODEL: &str = "jev-1.13.0";

/// What a stub answers to one request.
#[derive(Clone)]
enum Reply {
    /// A valid System One answer to every question of the request.
    Answer,
    /// A valid answer naming this model, with these top-level fields and
    /// these `usage` fields added.
    AnswerAs(
        &'static str,
        Vec<(&'static str, serde_json::Value)>,
        Vec<(&'static str, serde_json::Value)>,
    ),
    /// This status, headers and body.
    Status(u16, Vec<(&'static str, String)>, Vec<u8>),
    /// Wait, then reply.
    After(Duration, Box<Self>),
    /// Never answer; record when the client closes the connection.
    Hang,
    /// This status with a declared body of this many bytes, sent in small
    /// chunks slowly until the client closes the connection.
    Trickle(u16, usize),
    /// This status and body with no Content-Length: the body ends when the
    /// connection closes.
    Unsized(u16, Vec<u8>),
}

/// A loopback stub.
struct Stub {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    closed: Arc<Mutex<Option<Instant>>>,
}

impl Stub {
    async fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("stub: bind");
        let addr = listener.local_addr().expect("stub: addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(Mutex::new(None));
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let (seen, gone) = (Arc::clone(&requests), Arc::clone(&closed));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (seen, gone, script) = (Arc::clone(&seen), Arc::clone(&gone), Arc::clone(&script));
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut socket).await else {
                        return;
                    };
                    seen.lock().push(request.clone());
                    let reply = script.lock().pop_front().unwrap_or(Reply::Answer);
                    serve(&mut socket, &request, reply, &gone).await;
                });
            }
        });
        Self { addr, requests, closed }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    fn count(&self) -> usize {
        self.requests.lock().len()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&buffer).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            let length = text[..end]
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if buffer.len() >= end + 4 + length {
                return Some(text);
            }
        }
    }
}

fn answer_body(request: &str) -> Vec<u8> {
    answer_body_as(request, MODEL, &[], &[])
}

fn answer_body_as(
    request: &str,
    model: &str,
    fields: &[(&'static str, serde_json::Value)],
    usage: &[(&'static str, serde_json::Value)],
) -> Vec<u8> {
    let body = &request[request.find("\r\n\r\n").expect("stub: body") + 4..];
    let parsed: serde_json::Value = serde_json::from_str(body).expect("stub: request json");
    let answers: serde_json::Map<String, serde_json::Value> = parsed["questions"]
        .as_object()
        .expect("stub: questions")
        .keys()
        .map(|id| (id.clone(), serde_json::json!({"type": "noul", "noul": 0.25})))
        .collect();
    let mut body = serde_json::json!({
        "model": model,
        "answers": answers,
        "usage": {"input_tokens": 10, "output_tokens": 2}
    });
    for (name, value) in fields {
        body[*name] = value.clone();
    }
    for (name, value) in usage {
        body["usage"][*name] = value.clone();
    }
    serde_json::to_vec(&body).expect("stub: response json")
}

async fn serve(socket: &mut tokio::net::TcpStream, request: &str, reply: Reply, closed: &Mutex<Option<Instant>>) {
    let (status, headers, body) = match reply {
        Reply::Answer => (200, Vec::new(), answer_body(request)),
        Reply::AnswerAs(model, fields, usage) => (200, Vec::new(), answer_body_as(request, model, &fields, &usage)),
        Reply::Status(status, headers, body) => (status, headers, body),
        Reply::After(delay, reply) => {
            tokio::time::sleep(delay).await;
            return Box::pin(serve(socket, request, *reply, closed)).await;
        }
        Reply::Hang => {
            let mut sink = [0_u8; 64];
            while socket.read(&mut sink).await.is_ok_and(|read| read > 0) {}
            *closed.lock() = Some(Instant::now());
            return;
        }
        Reply::Unsized(status, body) => {
            let head = format!("HTTP/1.1 {status} STUB\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n");
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
            return;
        }
        Reply::Trickle(status, declared) => {
            let head = format!(
                "HTTP/1.1 {status} STUB\r\ncontent-type: application/json\r\ncontent-length: {declared}\r\n\r\n"
            );
            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            let chunk = [b' '; 512];
            let mut sent = 0;
            while sent < declared && socket.write_all(&chunk).await.is_ok() {
                sent += chunk.len();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            return;
        }
    };
    let mut head = format!(
        "HTTP/1.1 {status} STUB\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("\r\n");
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(&body).await;
    let _ = socket.shutdown().await;
}

/// A counting listener that answers nothing useful: a proxy or a redirect
/// target that must never see a request.
async fn counter() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("counter: bind");
    let addr = listener.local_addr().expect("counter: addr");
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            seen.fetch_add(1, Ordering::SeqCst);
            let _ = socket
                .write_all(b"HTTP/1.1 502 COUNTER\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
        }
    });
    (addr, count)
}

/// A sink that drops every causal event.
struct NoEvents;

impl CausalEventSink for NoEvents {
    fn emit(&self, _event: CausalEvent) {}
}

struct Plane {
    home: tempfile::TempDir,
    key: Arc<DecisionKey>,
    factory: DecisionBackendFactory,
}

fn build_plane(endpoint: &str, max_response_bytes: u32) -> Plane {
    build_plane_with(endpoint, max_response_bytes, false)
}

fn build_plane_with(endpoint: &str, max_response_bytes: u32, remote: bool) -> Plane {
    let home = tempfile::TempDir::new().expect("test: temp dir");
    let section: toml::Value = toml::from_str(&format!(
        "schema_version = 1\nenabled = true\n\n[backend]\nkind = \"compatible_http\"\n\
         endpoint = \"{endpoint}\"\nmodel = \"{MODEL}\"\ntimeout_ms = 400\nmax_response_bytes = {max_response_bytes}\n\
         remote_enabled = {remote}\n\n[policy]\nextra_latency_budget_ms = 2000\n"
    ))
    .expect("test: section");
    let mut tree = toml::Value::try_from(Config::default()).expect("test: default tree");
    tree.as_table_mut()
        .expect("test: table")
        .insert("causal_tree".to_string(), section);
    let mut config: Config = tree.try_into().expect("test: config");
    config.workspace_dir = home.path().to_path_buf();
    let catalog = CatalogView::from_config(&config).with_curated([CuratedModel {
        operations: Some(vec![ModelOperation::DecisionEndpoint]),
        ..CuratedModel::new(ProviderId::of(&format!("custom:{endpoint}")), Some(MODEL))
    }]);
    let key = FactoryDeps::load_key(&config).expect("test: key");
    let factory = DecisionBackendFactory::from_config(
        &config,
        FactoryDeps {
            catalog: &catalog,
            key: Some(Arc::clone(&key)),
            transports: &HttpDecisionTransports,
            config_generation: 1,
            events: Arc::new(NoEvents),
        },
    )
    .expect("test: factory");
    Plane { home, key, factory }
}

fn request(key: &DecisionKey, expires_in: Duration) -> DecisionRequest {
    let mut questions = BTreeMap::new();
    questions.insert(
        QuestionId::global(GlobalQuestion::EvidenceSufficient),
        DecisionQuestion::Noul {
            instructions: StructuredValue::try_from(serde_json::Value::from("Is the evidence sufficient?"))
                .expect("test: text"),
            criteria: None,
        },
    );
    DecisionRequest::seal(
        RequestParts {
            state: StructuredValue::try_from(serde_json::json!({"goal": "list files"})).expect("test: state"),
            questions,
            model: Some(MODEL.to_string()),
            state_fingerprint: "state".to_string(),
            candidate_set_hash: "candidates".to_string(),
            policy_version: Arc::from("policy"),
            catalog_generation: "catalog".to_string(),
            omitted_questions: Vec::new(),
            approval_pending: true,
        },
        key,
        Instant::now() + expires_in,
        &RequestLimits::default(),
    )
    .expect("test: seal")
}

/// V2.01 over loopback: the compatible backend serves a valid decision
/// and sends no `authorization` header without a credential.
#[tokio::test]
#[serial_test::serial]
async fn loopback_compatible_backend_sends_no_authorization() {
    let stub = Stub::start(vec![Reply::Answer]).await;
    let plane = build_plane(&stub.endpoint(), 262_144);
    let result = plane
        .factory
        .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Primary, "{:?}", result.attempts);
    let sent = stub.requests.lock()[0].clone();
    assert!(sent.starts_with("POST /v1/systemone HTTP/1.1"), "{sent}");
    assert!(!sent.to_ascii_lowercase().contains("authorization:"), "{sent}");
    assert!(
        sent.to_ascii_lowercase().contains("content-type: application/json"),
        "{sent}"
    );
}

/// `localhost` is resolved to 127.0.0.1 by the client itself.
#[tokio::test]
#[serial_test::serial]
async fn a_localhost_endpoint_is_reached_without_dns() {
    let stub = Stub::start(vec![Reply::Answer]).await;
    let plane = build_plane(&format!("http://localhost:{}", stub.addr.port()), 262_144);
    let result = plane
        .factory
        .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Primary, "{:?}", result.attempts);
    assert_eq!(stub.count(), 1);
}

/// Sets proxy environment variables for one test and restores them.
struct ProxyEnv(Vec<(&'static str, Option<String>)>);

impl ProxyEnv {
    fn set(value: &str) -> Self {
        let names = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];
        let saved = names.iter().map(|name| (*name, std::env::var(name).ok())).collect();
        for name in names {
            // SAFETY: test-only, serialized by `serial_test` across this
            // binary, restored on drop; the loopback clients under test never
            // read the environment.
            unsafe { std::env::set_var(name, value) };
        }
        let no_proxy = ["NO_PROXY", "no_proxy"];
        for name in no_proxy {
            // SAFETY: as above.
            unsafe { std::env::remove_var(name) };
        }
        Self(saved)
    }
}

impl Drop for ProxyEnv {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            // SAFETY: restores what `set` saved, under the same serialization.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

/// SP-CDP-A 5.8: a loopback request never goes through a proxy from the
/// environment. The control shows the environment is live: a default
/// client sends through the proxy.
#[tokio::test]
#[serial_test::serial]
async fn a_loopback_request_never_goes_through_a_proxy_env() {
    let stub = Stub::start(vec![Reply::Answer]).await;
    let (proxy, proxied) = counter().await;
    let _env = ProxyEnv::set(&format!("http://{proxy}"));
    let control = reqwest::Client::new()
        .get(format!("{}/control", stub.endpoint()))
        .send()
        .await;
    assert!(control.is_ok() || control.is_err());
    assert_eq!(
        proxied.load(Ordering::SeqCst),
        1,
        "control: a default client uses the proxy environment"
    );

    let plane = build_plane(&stub.endpoint(), 262_144);
    let result = plane
        .factory
        .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Primary, "{:?}", result.attempts);
    assert_eq!(
        proxied.load(Ordering::SeqCst),
        1,
        "the decision client never used the proxy"
    );
    assert_eq!(stub.count(), 1);
}

/// SP-CDP-A 5.8: a redirect is never followed. The control shows a
/// default client follows it.
#[tokio::test]
#[serial_test::serial]
async fn a_redirect_is_never_followed() {
    let (target, followed) = counter().await;
    let redirect = Reply::Status(
        302,
        vec![("location", format!("http://{target}/v1/systemone"))],
        Vec::new(),
    );
    let stub = Stub::start(vec![redirect.clone(), redirect.clone(), redirect]).await;
    let control = reqwest::Client::builder().no_proxy().build().expect("test: client");
    let _ = control
        .post(format!("{}/v1/systemone", stub.endpoint()))
        .body("{}")
        .send()
        .await;
    assert_eq!(
        followed.load(Ordering::SeqCst),
        1,
        "control: a default client follows the redirect"
    );

    let plane = build_plane(&stub.endpoint(), 262_144);
    let result = plane
        .factory
        .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(
        followed.load(Ordering::SeqCst),
        1,
        "the decision client never followed it"
    );
    assert_eq!(result.served_by, ServedBy::Builtin);
    assert!(matches!(
        result.attempts[0].outcome,
        AttemptOutcome::Failed(DecisionError::Unavailable)
    ));
}

/// D-086 F-1, F-7: the body of a refusal is never read, however large it
/// is declared to be or however slowly it arrives; the refusal fails at
/// once as its class.
#[tokio::test]
#[serial_test::serial]
async fn a_refusal_body_is_never_read() {
    let stub = Stub::start(vec![Reply::Trickle(500, 256 << 20), Reply::Trickle(500, 256 << 20)]).await;
    let plane = build_plane(&stub.endpoint(), 262_144);
    let started = Instant::now();
    let result = plane
        .factory
        .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
        .await
        .expect("test: decide");
    // Waiting for the body would take the whole 400 ms slot timeout per
    // attempt, twice (the loopback retry).
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "the refusal waited for its body: {:?}",
        started.elapsed()
    );
    assert_eq!(result.served_by, ServedBy::Builtin);
    assert!(
        matches!(
            &result.attempts[0].outcome,
            AttemptOutcome::Failed(DecisionError::Unavailable)
        ),
        "{:?}",
        result.attempts
    );
}

/// D-086 C-1: a success with no declared length is capped while it streams:
/// past the limit it is refused as oversize, never decoded.
#[tokio::test]
#[serial_test::serial]
async fn an_undeclared_oversize_body_is_capped_while_streaming() {
    let stub = Stub::start(vec![Reply::Unsized(200, vec![b' '; 5000])]).await;
    let plane = build_plane(&stub.endpoint(), 4096);
    let result = plane
        .factory
        .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert!(
        matches!(
            &result.attempts[0].outcome,
            AttemptOutcome::Failed(DecisionError::InvalidResponse(ValidationRule::Oversize))
        ),
        "{:?}",
        result.attempts
    );
}

/// D-086 F-8: a success whose declared length is over the limit is refused
/// before its body is read.
#[tokio::test]
#[serial_test::serial]
async fn an_oversize_declared_length_is_refused_before_reading() {
    let stub = Stub::start(vec![Reply::Trickle(200, 10 << 20)]).await;
    let plane = build_plane(&stub.endpoint(), 262_144);
    let started = Instant::now();
    let result = plane
        .factory
        .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the body was read: {:?}",
        started.elapsed()
    );
    assert!(
        matches!(
            &result.attempts[0].outcome,
            AttemptOutcome::Failed(DecisionError::InvalidResponse(ValidationRule::Oversize))
        ),
        "{:?}",
        result.attempts
    );
}

/// SP-CDP-A 5.8 failure classes: slow is a timeout; 429 with Retry-After is
/// rate limited and not retried; 401 is authentication and not retried; 500
/// is unavailable and retried once on loopback; an oversize body is an
/// invalid response.
#[tokio::test]
#[serial_test::serial]
async fn compatible_backend_failure_classes() {
    let cases: Vec<(Vec<Reply>, u32, DecisionError, usize)> = vec![
        (
            vec![Reply::After(Duration::from_secs(2), Box::new(Reply::Answer))],
            262_144,
            DecisionError::Timeout,
            1,
        ),
        (
            vec![Reply::Status(429, vec![("retry-after", "30".to_string())], Vec::new())],
            262_144,
            DecisionError::RateLimited {
                retry_after_ms: Some(30_000),
            },
            1,
        ),
        (
            vec![Reply::Status(401, Vec::new(), b"{\"error\":\"bad key\"}".to_vec())],
            262_144,
            DecisionError::Authentication,
            1,
        ),
        (
            vec![
                Reply::Status(500, Vec::new(), Vec::new()),
                Reply::Status(500, Vec::new(), Vec::new()),
            ],
            262_144,
            DecisionError::Unavailable,
            2,
        ),
        (
            vec![Reply::Status(200, Vec::new(), vec![b' '; 5000])],
            4096,
            DecisionError::InvalidResponse(ValidationRule::Oversize),
            1,
        ),
    ];
    for (script, max_response_bytes, expected, requests) in cases {
        let stub = Stub::start(script).await;
        let plane = build_plane(&stub.endpoint(), max_response_bytes);
        let result = plane
            .factory
            .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Failed(expected.clone()),
            "{expected}"
        );
        assert_eq!(result.served_by, ServedBy::Builtin, "{expected}");
        assert_eq!(stub.count(), requests, "{expected}: requests sent");
    }
    // A 500 then a valid answer: the one loopback retry serves.
    let stub = Stub::start(vec![Reply::Status(500, Vec::new(), Vec::new()), Reply::Answer]).await;
    let plane = build_plane(&stub.endpoint(), 262_144);
    let result = plane
        .factory
        .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Primary);
    assert_eq!(stub.count(), 2);
}

/// SP-CDP-A 5.8 cancellation: dropping a decision closes the connection
/// promptly (within 100 ms) and records no success.
#[tokio::test]
#[serial_test::serial]
async fn dropping_decide_closes_the_socket_and_records_no_success() {
    let stub = Stub::start(vec![Reply::Hang]).await;
    let plane = build_plane(&stub.endpoint(), 262_144);
    let request = request(&plane.key, Duration::from_secs(5));
    let lineage = CausalLineage::default();
    let decision = plane.factory.decide(&request, &lineage);
    let dropped = tokio::time::timeout(Duration::from_millis(150), decision).await;
    assert!(dropped.is_err(), "the decision was still waiting");
    let dropped_at = Instant::now();
    for _ in 0..50 {
        if stub.closed.lock().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let closed = stub.closed.lock().expect("test: the stub saw the connection close");
    assert!(closed.saturating_duration_since(dropped_at) < Duration::from_millis(100));
    assert_eq!(plane.factory.status().cached, 0);
}

/// The health probe of a loopback backend reaches the endpoint with a
/// neutral request; a failing endpoint leaves the backend out.
#[tokio::test]
#[serial_test::serial]
async fn the_loopback_health_probe_decides_whether_the_backend_serves() {
    let stub = Stub::start(vec![Reply::Answer, Reply::Answer]).await;
    let plane = build_plane(&stub.endpoint(), 262_144);
    plane.factory.check_health().await;
    assert!(plane.factory.status().order[0].healthy);
    let probe = stub.requests.lock()[0].clone();
    assert!(probe.contains("health_check"), "{probe}");
    assert!(!probe.contains("list files"), "the probe carries no user state");

    let refused = Stub::start(vec![Reply::Status(503, Vec::new(), Vec::new()); 2]).await;
    let plane = build_plane(&refused.endpoint(), 262_144);
    plane.factory.check_health().await;
    assert!(!plane.factory.status().order[0].healthy);
    let result = plane
        .factory
        .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(
        result.attempts[0].outcome,
        AttemptOutcome::Skipped(DecisionError::Unavailable)
    );
    assert_eq!(
        refused.count(),
        2,
        "the probe and its one retry; the decision sent nothing"
    );
}

/// SP-CDP-A 5.6 "Health": a remote backend reports configuration readiness
/// only; its health probe opens no connection (a `.invalid` host could not be
/// reached, so any attempt would fail the probe).
#[tokio::test]
#[serial_test::serial]
async fn a_remote_backend_health_probe_does_no_io() {
    let plane = build_plane_with("https://decisions.invalid/v1", 262_144, true);
    plane.factory.check_health().await;
    assert!(plane.factory.status().order[0].healthy);
}

// ---------------------------------------------------------------------------
// CDP-13: the hosted adapters
// ---------------------------------------------------------------------------

/// The pinned OpenRouter model and the dated snapshot it answers with.
const OR_MODEL: &str = "typesafe/jev-1.13";
const OR_SNAPSHOT: &str = "typesafe/jev-1.13-20260917";

/// A hosted backend of `kind` whose endpoint override is `endpoint`, with
/// `secret` stored (encrypted) as its auth-profile credential.
struct Hosted {
    kind: &'static str,
    endpoint: String,
    model: &'static str,
    snapshot: &'static str,
    remote: bool,
    secret: &'static str,
}

fn build_hosted(hosted: &Hosted) -> Plane {
    let home = tempfile::TempDir::new().expect("test: temp dir");
    let provider = if hosted.kind == "typesafe" {
        "decision:typesafe"
    } else {
        "openrouter"
    };
    let section: toml::Value = toml::from_str(&format!(
        "schema_version = 1\nenabled = true\n\n[backend]\nkind = \"{}\"\nendpoint = \"{}\"\nmodel = \"{}\"\n\
         pinned_snapshot = \"{}\"\ncredential_ref = \"cred-decision-hosted\"\ntimeout_ms = 400\n\
         remote_enabled = {}\n\n[policy]\nextra_latency_budget_ms = 2000\n",
        hosted.kind, hosted.endpoint, hosted.model, hosted.snapshot, hosted.remote
    ))
    .expect("test: section");
    let mut tree = toml::Value::try_from(Config::default()).expect("test: default tree");
    tree.as_table_mut()
        .expect("test: table")
        .insert("causal_tree".to_string(), section);
    let mut config: Config = tree.try_into().expect("test: config");
    config.workspace_dir = home.path().join("workspace");
    config.config_path = home.path().join("config.toml");
    config.secrets.encrypt = true;
    config.causal_tree.validate().expect("test: a valid hosted section");
    openprx::config::manage::credentials::CredentialReferences::for_config(&config)
        .expect("test: references")
        .set(provider, "decision-hosted", hosted.secret)
        .expect("test: stored credential");
    let catalog = CatalogView::from_config(&config).with_curated([CuratedModel {
        operations: Some(vec![ModelOperation::DecisionEndpoint]),
        ..CuratedModel::new(ProviderId::of(provider), Some(hosted.model))
    }]);
    let key = FactoryDeps::load_key(&config).expect("test: key");
    let factory = DecisionBackendFactory::from_config(
        &config,
        FactoryDeps {
            catalog: &catalog,
            key: Some(Arc::clone(&key)),
            transports: &HttpDecisionTransports,
            config_generation: 1,
            events: Arc::new(NoEvents),
        },
    )
    .expect("test: factory");
    Plane { home, key, factory }
}

fn request_with_model(key: &DecisionKey, model: &str) -> DecisionRequest {
    let mut questions = BTreeMap::new();
    questions.insert(
        QuestionId::global(GlobalQuestion::EvidenceSufficient),
        DecisionQuestion::Noul {
            instructions: StructuredValue::try_from(serde_json::Value::from("Is the evidence sufficient?"))
                .expect("test: text"),
            criteria: None,
        },
    );
    DecisionRequest::seal(
        RequestParts {
            state: StructuredValue::try_from(serde_json::json!({"goal": "list files"})).expect("test: state"),
            questions,
            model: Some(model.to_string()),
            state_fingerprint: "state".to_string(),
            candidate_set_hash: "candidates".to_string(),
            policy_version: Arc::from("policy"),
            catalog_generation: "catalog".to_string(),
            omitted_questions: Vec::new(),
            approval_pending: true,
        },
        key,
        Instant::now() + Duration::from_secs(5),
        &RequestLimits::default(),
    )
    .expect("test: seal")
}

/// V2.02 (resolution half): the TypeSafe backend's credential is the
/// auth-profile reference, stored encrypted and released to its owner only;
/// the stub sees it as the bearer token on the System One route of the
/// override, and the store never holds it in plain text.
#[tokio::test]
#[serial_test::serial]
async fn typesafe_backend_resolves_credential_ref_through_secret_store() {
    let stub = Stub::start(vec![Reply::Answer]).await;
    let plane = build_hosted(&Hosted {
        kind: "typesafe",
        endpoint: format!("{}/ts", stub.endpoint()),
        model: MODEL,
        snapshot: "",
        remote: true,
        secret: "TS-KEY-SENTINEL-6W2x9Q",
    });
    let store = std::fs::read_to_string(plane.home.path().join("auth-profiles.json")).expect("test: store");
    assert!(!store.contains("TS-KEY-SENTINEL-6W2x9Q"), "stored encrypted");
    assert!(store.contains("enc2:"), "stored encrypted");
    let result = plane
        .factory
        .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Primary, "{:?}", result.attempts);
    assert_eq!(result.served.identity().kind, openprx::decision::BackendKind::TypeSafe);
    assert_eq!(result.served.claimed_backend(), "typesafe");
    let sent = stub.requests.lock()[0].clone();
    assert!(sent.starts_with("POST /ts/v1/systemone HTTP/1.1"), "{sent}");
    assert!(
        sent.lines()
            .any(|line| line.eq_ignore_ascii_case("authorization: Bearer TS-KEY-SENTINEL-6W2x9Q")),
        "{sent}"
    );
    let status = plane.factory.status();
    assert_eq!(status.order[0].locality, openprx::decision::BackendLocality::Remote);
}

/// COORD-6: a hosted endpoint override stays remote. With
/// `remote_enabled = false` a loopback stub gets no request although the
/// key is stored; with it, exactly one, never retried after a failure; and
/// its health probe does no I/O.
#[tokio::test]
#[serial_test::serial]
async fn hosted_endpoint_override_stays_remote_and_needs_remote_enabled() {
    for kind in ["typesafe", "openrouter"] {
        let (model, snapshot) = if kind == "typesafe" {
            (MODEL, "")
        } else {
            (OR_MODEL, OR_SNAPSHOT)
        };
        let stub = Stub::start(vec![Reply::Status(503, Vec::new(), Vec::new()), Reply::Answer]).await;
        let hosted = |remote| Hosted {
            kind,
            endpoint: format!("{}/{kind}", stub.endpoint()),
            model,
            snapshot,
            remote,
            secret: "HOSTED-KEY-SENTINEL-51xQ",
        };
        let off = build_hosted(&hosted(false));
        let result = off
            .factory
            .decide(&request_with_model(&off.key, model), &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(result.served_by, ServedBy::Builtin, "{kind}");
        let status = off.factory.status();
        assert!(status.order.is_empty(), "{kind}: {status:?}");
        assert_eq!(status.excluded[0].2.as_str(), "remote_disabled", "{kind}: {status:?}");
        off.factory.check_health().await;
        assert_eq!(stub.count(), 0, "{kind}: nothing is sent while remote is off");

        let on = build_hosted(&hosted(true));
        on.factory.check_health().await;
        assert_eq!(stub.count(), 0, "{kind}: a remote health probe does no I/O");
        let result = on
            .factory
            .decide(&request_with_model(&on.key, model), &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Failed(DecisionError::Unavailable),
            "{kind}"
        );
        assert_eq!(result.served_by, ServedBy::Builtin, "{kind}");
        assert_eq!(stub.count(), 1, "{kind}: a remote backend is never retried");
    }
}

/// V6.08: a hosted backend answering with any model but its pin (an alias
/// that drifted, or another snapshot) is a model-version mismatch and falls
/// back.
#[tokio::test]
#[serial_test::serial]
async fn hosted_alias_drift_after_pin_falls_back() {
    let cases = [
        ("typesafe", MODEL, "", "jev-latest"),
        ("typesafe", MODEL, "", "jev-1.14.0"),
        ("openrouter", OR_MODEL, OR_SNAPSHOT, OR_MODEL),
        ("openrouter", OR_MODEL, OR_SNAPSHOT, "typesafe/jev-1.13-20261001"),
    ];
    for (kind, model, snapshot, answered) in cases {
        let stub = Stub::start(vec![Reply::AnswerAs(answered, Vec::new(), Vec::new())]).await;
        let plane = build_hosted(&Hosted {
            kind,
            endpoint: stub.endpoint(),
            model,
            snapshot,
            remote: true,
            secret: "DRIFT-KEY-SENTINEL-77aZ",
        });
        let result = plane
            .factory
            .decide(&request_with_model(&plane.key, model), &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Failed(DecisionError::ModelVersionMismatch),
            "{kind} answered {answered}"
        );
        assert_eq!(result.served_by, ServedBy::Builtin);
    }
}

/// DIFFS D-17 over a stub: the OpenRouter backend posts the Decisions route
/// with the model and nothing OpenRouter-specific, accepts its pinned
/// snapshot, and records the body `id` and `usage.cost`.
#[tokio::test]
#[serial_test::serial]
async fn openrouter_backend_posts_the_decisions_route_and_reads_id_and_cost() {
    let stub = Stub::start(vec![Reply::AnswerAs(
        OR_SNAPSHOT,
        vec![
            ("id", serde_json::json!("gen-dec-1789738314-X5e5eKGQdvR9rblyX250")),
            ("provider", serde_json::json!("TypeSafe")),
        ],
        vec![("cost", serde_json::json!(1.9992e-05))],
    )])
    .await;
    let plane = build_hosted(&Hosted {
        kind: "openrouter",
        endpoint: stub.endpoint(),
        model: OR_MODEL,
        snapshot: OR_SNAPSHOT,
        remote: true,
        secret: "OR-KEY-SENTINEL-3kP0",
    });
    let result = plane
        .factory
        .decide(&request_with_model(&plane.key, OR_MODEL), &CausalLineage::default())
        .await
        .expect("test: decide");
    assert_eq!(result.served_by, ServedBy::Primary, "{:?}", result.attempts);
    assert_eq!(result.served.claimed_model(), OR_SNAPSHOT);
    assert_eq!(
        result.served.provider_request_id(),
        Some("gen-dec-1789738314-X5e5eKGQdvR9rblyX250")
    );
    assert_eq!(result.served.usage().cost_micro_usd, Some(20));
    let sent = stub.requests.lock()[0].clone();
    assert!(sent.starts_with("POST /api/alpha/decisions HTTP/1.1"), "{sent}");
    assert!(
        sent.lines()
            .any(|line| line.eq_ignore_ascii_case("authorization: Bearer OR-KEY-SENTINEL-3kP0")),
        "{sent}"
    );
    assert!(!sent.to_ascii_lowercase().contains("x-session-id"), "{sent}");
    let body: serde_json::Value =
        serde_json::from_str(&sent[sent.find("\r\n\r\n").expect("test: body") + 4..]).expect("test: body json");
    let mut fields: Vec<&str> = body
        .as_object()
        .expect("test: object")
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["model", "questions", "state"]);
    assert_eq!(body["model"], OR_MODEL);
}

/// DIFFS D-17 and D-043 C-9 over a stub: OpenRouter's statuses map to their
/// classes, and a 200 carrying only an `error` object is a transport
/// failure; each falls back to the builtin after exactly one request.
#[tokio::test]
#[serial_test::serial]
async fn openrouter_failures_map_to_their_classes() {
    let cases: Vec<(Reply, DecisionError)> = vec![
        (Reply::Status(402, Vec::new(), Vec::new()), DecisionError::Unavailable),
        (
            Reply::Status(402, vec![("retry-after", "5".to_string())], Vec::new()),
            DecisionError::RateLimited {
                retry_after_ms: Some(5_000),
            },
        ),
        (
            Reply::Status(413, Vec::new(), Vec::new()),
            DecisionError::InvalidRequest(openprx::decision::RequestRule::RejectedByBackend),
        ),
        (Reply::Status(524, Vec::new(), Vec::new()), DecisionError::Timeout),
        (Reply::Status(529, Vec::new(), Vec::new()), DecisionError::Unavailable),
        (
            Reply::Status(403, Vec::new(), Vec::new()),
            DecisionError::Authentication,
        ),
        (
            Reply::Status(
                200,
                Vec::new(),
                br#"{"error":{"code":502,"message":"Provider returned error"}}"#.to_vec(),
            ),
            DecisionError::Transport,
        ),
    ];
    for (reply, expected) in cases {
        let stub = Stub::start(vec![reply]).await;
        let plane = build_hosted(&Hosted {
            kind: "openrouter",
            endpoint: stub.endpoint(),
            model: OR_MODEL,
            snapshot: OR_SNAPSHOT,
            remote: true,
            secret: "OR-KEY-SENTINEL-3kP0",
        });
        let result = plane
            .factory
            .decide(&request_with_model(&plane.key, OR_MODEL), &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Failed(expected.clone()),
            "{expected}"
        );
        assert_eq!(result.served_by, ServedBy::Builtin, "{expected}");
        assert_eq!(stub.count(), 1, "{expected}");
    }
}
