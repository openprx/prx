#![cfg(test)]

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
    clippy::unwrap_used
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
    accepted: Arc<AtomicUsize>,
    closed: Arc<Mutex<Option<Instant>>>,
}

impl Stub {
    async fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("stub: bind");
        let addr = listener.local_addr().expect("stub: addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(Mutex::new(None));
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_by_listener = Arc::clone(&accepted);
        let (seen, gone) = (Arc::clone(&requests), Arc::clone(&closed));
        spawn_fixture(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                accepted_by_listener.fetch_add(1, Ordering::SeqCst);
                let (seen, gone, script) = (Arc::clone(&seen), Arc::clone(&gone), Arc::clone(&script));
                spawn_fixture(async move {
                    let Some(request) = read_request(&mut socket).await else {
                        return;
                    };
                    seen.lock().push(request.clone());
                    let reply = script.lock().pop_front().unwrap_or(Reply::Answer);
                    serve(&mut socket, &request, reply, &gone).await;
                });
            }
        });
        Self {
            addr,
            requests,
            accepted,
            closed,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    fn accepted_count(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
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
    let address = proxy_child_address().unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 0)));
    let listener = TcpListener::bind(address).await.expect("counter: bind");
    let addr = listener.local_addr().expect("counter: addr");
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    spawn_fixture(async move {
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
    home: RetainedHome,
    key: Arc<DecisionKey>,
    factory: AccountedFactory,
}

fn build_plane(endpoint: &str, max_response_bytes: u32) -> Plane {
    build_plane_with(endpoint, max_response_bytes, false)
}

fn build_plane_with(endpoint: &str, max_response_bytes: u32, remote: bool) -> Plane {
    let home = RetainedHome::new();
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
    config.config_path = home.path().join("config.toml");
    let catalog = CatalogView::from_config(&config).with_curated([CuratedModel {
        operations: Some(vec![ModelOperation::DecisionEndpoint]),
        ..CuratedModel::new(ProviderId::of(&format!("custom:{endpoint}")), Some(MODEL))
    }]);
    fixture_price(&mut config, &catalog);
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
    let factory = AccountedFactory {
        inner: factory,
        config,
        key: Arc::clone(&key),
    };
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
    run_fixture(async {
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
    })
    .await;
}

/// `localhost` is resolved to 127.0.0.1 by the client itself.
#[tokio::test]
#[serial_test::serial]
async fn a_localhost_endpoint_is_reached_without_dns() {
    run_fixture(async {
        let stub = Stub::start(vec![Reply::Answer]).await;
        let plane = build_plane(&format!("http://localhost:{}", stub.addr.port()), 262_144);
        let result = plane
            .factory
            .decide(&request(&plane.key, Duration::from_secs(5)), &CausalLineage::default())
            .await
            .expect("test: decide");
        assert_eq!(result.served_by, ServedBy::Primary, "{:?}", result.attempts);
        assert_eq!(stub.count(), 1);
    })
    .await;
}

// Only this original proxy oracle needs a fresh process. No environment in
// the parent LIB process is changed, including non-Unicode proxy values.
const PROXY_CHILD: &str = "PRX_PRIVATE_BACKEND_PROXY_CHILD";
const PROXY_CASE: &str = "causal_cli::backend_transport_tests::private_proxy_worker_executes_original_oracle";
const PROXY_NAMES: [&str; 6] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
];
const NO_PROXY_NAMES: [&str; 2] = ["NO_PROXY", "no_proxy"];
const PROXY_CAPTURE_LIMIT: u64 = 1_048_576;

fn proxy_child_address() -> Option<SocketAddr> {
    let marker = std::env::var_os(PROXY_CHILD)?;
    let expected = [PROXY_CASE, "--exact", "--ignored", "--nocapture"];
    assert!(
        std::env::args_os().skip(1).eq(expected.map(std::ffi::OsString::from)),
        "private proxy marker requires the exact child invocation"
    );
    let directory = std::path::PathBuf::from(marker);
    assert!(
        directory
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|name| name.starts_with("prx-proxy-worker-")),
        "private proxy handoff namespace"
    );
    assert_eq!(
        directory.parent(),
        Some(std::env::temp_dir().as_path()),
        "private proxy handoff root"
    );
    let metadata = std::fs::symlink_metadata(&directory).expect("owned proxy handoff directory");
    assert!(metadata.file_type().is_dir(), "proxy handoff must be a real directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }
    let path = directory.join("handoff.json");
    let metadata = std::fs::symlink_metadata(&path).expect("parent-issued proxy handoff");
    assert!(
        metadata.file_type().is_file() && metadata.len() <= 8192,
        "proxy handoff must be a bounded regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).expect("private proxy handoff");
    let opened = file.metadata().expect("opened proxy handoff");
    assert!(
        opened.is_file() && opened.len() <= 8192,
        "opened handoff must be bounded and regular"
    );
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, 8193), &mut bytes).expect("read private handoff");
    assert!(bytes.len() <= 8192, "private handoff grew beyond bound");
    let record: serde_json::Value = serde_json::from_slice(&bytes).expect("private proxy handoff JSON");
    let executable = std::env::current_exe().expect("child actual executable");
    assert_eq!(
        record.get("executable").and_then(serde_json::Value::as_str),
        executable.to_str()
    );
    let address: SocketAddr = record
        .get("address")
        .and_then(serde_json::Value::as_str)
        .expect("private child address")
        .parse()
        .expect("private child address");
    assert!(
        address.ip() == std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST) && address.port() != 0,
        "private proxy address must be owned loopback"
    );
    Some(address)
}

fn consume_proxy_handoff() {
    let directory = std::path::PathBuf::from(std::env::var_os(PROXY_CHILD).expect("child handoff"));
    save_proxy_capture(&directory.join("child-entered"), b"original proxy oracle\n")
        .expect("private proxy handoff must be consumed exactly once");
}

/// Validation only. Command.env installed the proxy before the child started.
/// Dropping this zero-sized guard never mutates any process environment.
struct ProxyEnv;
impl ProxyEnv {
    fn set(value: &str) -> Self {
        let address = proxy_child_address().expect("proxy oracle requires its private child");
        assert_eq!(value, format!("http://{address}"));
        for name in PROXY_NAMES {
            assert_eq!(
                std::env::var_os(name),
                Some(std::ffi::OsString::from(value)),
                "child proxy environment differs"
            );
        }
        for name in NO_PROXY_NAMES {
            assert!(std::env::var_os(name).is_none(), "child proxy bypass must be absent");
        }
        Self
    }
}

fn save_proxy_capture(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

async fn run_proxy_fixture(work: impl std::future::Future<Output = ()>) {
    if proxy_child_address().is_some() {
        // The exact selector prevents another test from consuming the marker.
        // This branch runs the unchanged original oracle under the real owner.
        consume_proxy_handoff();
        run_fixture(work).await;
        return;
    }
    // Parent never polls or partially executes the original case.
    drop(work);
    let mut builder = tempfile::Builder::new();
    builder.prefix("prx-proxy-worker-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let evidence = builder.tempdir().expect("private proxy evidence").keep();
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "prx_proxy_evidence={}", evidence.display());
    let saved_parent: Vec<_> = PROXY_NAMES
        .into_iter()
        .chain(NO_PROXY_NAMES)
        .map(|name| (name, std::env::var_os(name)))
        .collect();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve private proxy address");
    let address = reservation.local_addr().expect("reserved proxy address");
    let proxy = format!("http://{address}");
    let executable = std::env::current_exe().expect("actual LIB test executable");
    let handoff = serde_json::json!({"executable":executable,"address":address.to_string()});
    save_proxy_capture(
        &evidence.join("handoff.json"),
        &serde_json::to_vec(&handoff).expect("private handoff JSON"),
    )
    .expect("retain parent-issued handoff");
    let mut command = tokio::process::Command::new(&executable);
    command
        .args([PROXY_CASE, "--exact", "--ignored", "--nocapture"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .env(PROXY_CHILD, &evidence);
    for name in PROXY_NAMES {
        command.env(name, &proxy);
    }
    for name in NO_PROXY_NAMES {
        command.env_remove(name);
    }
    // A handoff collision is a bind failure in counter(), never a fabricated
    // listener or accepted request. No retry or weakened original assertion.
    drop(reservation);
    let spawned = crate::runtime::shell_process::spawn_managed_shell_child(command);
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            let _ = save_proxy_capture(&evidence.join("receipt.json"), b"{\"spawned\":false}");
            panic!("private proxy worker could not spawn: {error}");
        }
    };
    let pipes = (child.take_stdout(), child.take_stderr());
    let (Some(stdout), Some(stderr)) = pipes else {
        let reaped = matches!(
            tokio::time::timeout(Duration::from_secs(5), child.terminate_and_reap()).await,
            Ok(true)
        );
        if reaped {
            child.mark_complete();
        }
        let receipt = format!("{{\"spawned\":true,\"pipes_missing\":true,\"reaped\":{reaped}}}");
        let _ = save_proxy_capture(&evidence.join("receipt.json"), receipt.as_bytes());
        panic!("private proxy worker pipes unavailable");
    };
    let mut stdout = stdout.take(PROXY_CAPTURE_LIMIT + 1);
    let mut stderr = stderr.take(PROXY_CAPTURE_LIMIT + 1);
    let mut output = Vec::new();
    let mut errors = Vec::new();
    // Supervisor bound only; original 400/2000ms and per-request deadlines stay unchanged.
    let observed = std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_mins(1), async {
        tokio::join!(
            child.wait(),
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut errors)
        )
    }))
    .catch_unwind()
    .await;
    let natural = matches!(&observed, Ok(Ok((Ok(_), Ok(_), Ok(_)))));
    let (killed, reaped) = if !natural {
        // Receipt bit means forced cleanup was requested, not signal success.
        let reaped = matches!(
            tokio::time::timeout(Duration::from_secs(5), child.terminate_and_reap()).await,
            Ok(true)
        );
        if reaped {
            child.mark_complete();
        }
        (true, reaped)
    } else {
        // Leader wait and both bounded pipe drains completed naturally.
        // Retire ownership before later receipt/assertion failures can unwind.
        child.mark_complete();
        (false, true)
    };
    // No reader tasks were spawned; all pipe futures above are already dropped.
    drop(stdout);
    drop(stderr);
    let bounded = output.len() <= PROXY_CAPTURE_LIMIT as usize && errors.len() <= PROXY_CAPTURE_LIMIT as usize;
    output.truncate(PROXY_CAPTURE_LIMIT as usize);
    errors.truncate(PROXY_CAPTURE_LIMIT as usize);
    let succeeded = matches!(&observed, Ok(Ok((Ok(status), Ok(_), Ok(_)))) if status.success());
    let output_text = std::str::from_utf8(&output).ok();
    let exact_body = output_text.is_some_and(|text| {
        text.lines().filter(|line| *line == "running 1 test").count() == 1
            && text
                .lines()
                .filter(|line| *line == format!("test {PROXY_CASE} ... ok"))
                .count()
                == 1
            && text.lines().filter(|line| line.starts_with("test result:")).count() == 1
            && text
                .lines()
                .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;"))
    });
    let parent_unchanged = saved_parent
        .iter()
        .all(|(name, value)| std::env::var_os(name) == *value);
    let retained = (|| -> std::io::Result<()> {
        save_proxy_capture(&evidence.join("stdout.log"), &output)?;
        save_proxy_capture(&evidence.join("stderr.log"), &errors)?;
        let receipt = serde_json::json!({"spawned":true,"natural_wait":natural,"killed":killed,
            "reaped":reaped,"capture_bounded":bounded,"exit_success":succeeded,
            "exact_original_body":exact_body,"parent_proxy_values_unchanged":parent_unchanged});
        save_proxy_capture(
            &evidence.join("receipt.json"),
            &serde_json::to_vec(&receipt).map_err(std::io::Error::other)?,
        )
    })()
    .is_ok();
    if let Err(panic) = observed {
        std::panic::resume_unwind(panic);
    }
    assert!(retained, "private proxy worker evidence must be retained");
    assert!(
        natural && reaped && !killed,
        "private proxy worker must exit and be reaped naturally"
    );
    assert!(
        bounded && succeeded && exact_body,
        "original proxy oracle child failed; private evidence retained"
    );
    assert!(
        parent_unchanged,
        "parent proxy environment changed during isolated oracle"
    );
}

// Additional dispatch helper, not an ignored original oracle. The original
// active test supervises this exact helper in a fresh process and checks its
// one-body footer; invoking it normally without the parent handoff fails.
#[test]
#[ignore = "private process worker invoked by the active original proxy test"]
fn private_proxy_worker_executes_original_oracle() {
    assert!(
        proxy_child_address().is_some(),
        "private worker requires parent handoff"
    );
    a_loopback_request_never_goes_through_a_proxy_env();
}

/// SP-CDP-A 5.8: a loopback request never goes through a proxy from the
/// environment. The control shows the environment is live: a default
/// client sends through the proxy.
#[tokio::test]
#[serial_test::serial]
async fn a_loopback_request_never_goes_through_a_proxy_env() {
    run_proxy_fixture(async {
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
    })
    .await;
}

/// SP-CDP-A 5.8: a redirect is never followed. The control shows a
/// default client follows it.
#[tokio::test]
#[serial_test::serial]
async fn a_redirect_is_never_followed() {
    run_fixture(async {
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
    })
    .await;
}

/// D-086 F-1, F-7: the body of a refusal is never read, however large it
/// is declared to be or however slowly it arrives; the refusal fails at
/// once as its class.
#[tokio::test]
#[serial_test::serial]
async fn a_refusal_body_is_never_read() {
    run_fixture(async {
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
    })
    .await;
}

/// D-086 C-1: a success with no declared length is capped while it streams:
/// past the limit it is refused as oversize, never decoded.
#[tokio::test]
#[serial_test::serial]
async fn an_undeclared_oversize_body_is_capped_while_streaming() {
    run_fixture(async {
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
    })
    .await;
}

/// D-086 F-8: a success whose declared length is over the limit is refused
/// before its body is read.
#[tokio::test]
#[serial_test::serial]
async fn an_oversize_declared_length_is_refused_before_reading() {
    run_fixture(async {
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
    })
    .await;
}

/// SP-CDP-A 5.8 failure classes: slow is a timeout; 429 with Retry-After is
/// rate limited and not retried; 401 is authentication and not retried; 500
/// is unavailable and retried once on loopback; an oversize body is an
/// invalid response.
#[tokio::test]
#[serial_test::serial]
async fn compatible_backend_failure_classes() {
    run_fixture(async {
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
    })
    .await;
}

/// SP-CDP-A 5.8 cancellation: dropping a decision closes the connection
/// promptly (within 100 ms) and records no success.
#[tokio::test]
#[serial_test::serial]
async fn dropping_decide_closes_the_socket_and_records_no_success() {
    run_fixture(async {
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
    })
    .await;
}

/// The health probe of a loopback backend reaches the endpoint with a
/// neutral request; a failing endpoint leaves the backend out.
#[tokio::test]
#[serial_test::serial]
async fn the_loopback_health_probe_decides_whether_the_backend_serves() {
    run_fixture(async {
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
    })
    .await;
}

/// SP-CDP-A 5.6 "Health": a remote backend reports configuration readiness
/// only; its health probe opens no connection (a `.invalid` host could not be
/// reached, so any attempt would fail the probe).
#[tokio::test]
#[serial_test::serial]
async fn a_remote_backend_health_probe_does_no_io() {
    run_fixture(async {
        let plane = build_plane_with("https://decisions.invalid/v1", 262_144, true);
        plane.factory.check_health().await;
        assert!(plane.factory.status().order[0].healthy);
    })
    .await;
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
    let home = RetainedHome::new();
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
    fixture_price(&mut config, &catalog);
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
    let factory = AccountedFactory {
        inner: factory,
        config,
        key: Arc::clone(&key),
    };
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
    run_fixture(async {
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
    })
    .await;
}

/// COORD-6: a hosted endpoint override stays remote. With
/// `remote_enabled = false` a loopback stub gets no request although the
/// key is stored; with it, exactly one, never retried after a failure; and
/// its health probe does no I/O.
#[tokio::test]
#[serial_test::serial]
async fn hosted_endpoint_override_stays_remote_and_needs_remote_enabled() {
    run_fixture(async {
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
    })
    .await;
}

/// V6.08: a hosted backend answering with any model but its pin (an alias
/// that drifted, or another snapshot) is a model-version mismatch and falls
/// back.
#[tokio::test]
#[serial_test::serial]
async fn hosted_alias_drift_after_pin_falls_back() {
    run_fixture(async {
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
    })
    .await;
}

/// DIFFS D-17 over a stub: the OpenRouter backend posts the Decisions route
/// with the model and nothing OpenRouter-specific, accepts its pinned
/// snapshot, and records the body `id` and `usage.cost`.
#[tokio::test]
#[serial_test::serial]
async fn openrouter_backend_posts_the_decisions_route_and_reads_id_and_cost() {
    run_fixture(async {
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
    })
    .await;
}

/// DIFFS D-17 and D-043 C-9 over a stub: OpenRouter's statuses map to their
/// classes, and a 200 carrying only an `error` object is a transport
/// failure; each falls back to the builtin after exactly one request.
#[tokio::test]
#[serial_test::serial]
async fn openrouter_failures_map_to_their_classes() {
    run_fixture(async {
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
    })
    .await;
}

// Private test carrier: uses the real operator accounting producer, never exports an owner.
use crate as openprx;
use futures::FutureExt as _;

tokio::task_local! {
    static FIXTURE_TASKS: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>;
}

fn spawn_fixture(future: impl std::future::Future<Output = ()> + Send + 'static) {
    let tasks = FIXTURE_TASKS.with(Arc::clone);
    let child_tasks = Arc::clone(&tasks);
    let handle = tokio::spawn(FIXTURE_TASKS.scope(child_tasks, future));
    tasks.lock().push(handle);
}

struct TaskOwner(Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>);
impl Drop for TaskOwner {
    fn drop(&mut self) {
        for handle in self.0.lock().iter() {
            handle.abort();
        }
    }
}

async fn run_fixture(work: impl std::future::Future<Output = ()>) {
    let tasks = Arc::new(Mutex::new(Vec::new()));
    let _tasks_owner = TaskOwner(Arc::clone(&tasks));
    let evidence = tempfile::tempdir().expect("private lifecycle evidence").keep();
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
        .expect("private backend cleanup owner");
    let mut outcome = None;
    let drained = FIXTURE_TASKS
        .scope(
            Arc::clone(&tasks),
            cleanup.run(async {
                outcome = Some(std::panic::AssertUnwindSafe(work).catch_unwind().await);
                Ok(())
            }),
        )
        .await;
    // All listener and connection tasks are registered, including nested spawns.
    // Abort without yielding before taking the complete current task list.
    let handles = {
        let mut handles = tasks.lock();
        for handle in handles.iter() {
            handle.abort();
        }
        std::mem::take(&mut *handles)
    };
    let mut joined = true;
    for mut handle in handles {
        match tokio::time::timeout(Duration::from_secs(5), &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) if error.is_cancelled() => {}
            _ => joined = false,
        }
    }
    // Private fixed-field receipt only; no error strings, credentials or payload.
    let evidence_saved = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let file = evidence.join("cleanup.json");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(file)?;
        write!(
            file,
            "{{\"accounting_drained\":{},\"listeners_joined\":{}}}",
            drained.is_ok(),
            joined
        )?;
        file.sync_all()
    })()
    .is_ok();
    match outcome {
        Some(Err(panic)) => std::panic::resume_unwind(panic),
        Some(Ok(())) => {
            assert!(evidence_saved, "private cleanup evidence retained");
            drained.expect("private backend accounting drained");
            assert!(joined, "private backend listener tasks joined");
        }
        None => panic!("private backend work did not enter its cleanup scope"),
    }
}

struct RetainedHome(std::path::PathBuf);
impl RetainedHome {
    fn new() -> Self {
        Self(tempfile::tempdir().expect("private retained backend evidence").keep())
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

struct AccountedFactory {
    inner: DecisionBackendFactory,
    config: Config,
    key: Arc<DecisionKey>,
}
impl AccountedFactory {
    async fn decide(
        &self,
        request: &DecisionRequest,
        lineage: &CausalLineage,
    ) -> anyhow::Result<openprx::decision::factory::PredictionResult> {
        if self.inner.status().order.is_empty() {
            return Ok(self.inner.decide(request, lineage).await?);
        }
        let deadline = request
            .manifest()
            .expires_at()
            .min(Instant::now() + Duration::from_millis(self.config.causal_tree.policy.extra_latency_budget_ms));
        let owner = super::admin_accounting_scope(
            &self.config,
            &self.inner,
            Arc::clone(&self.key),
            b"configured-test",
            deadline,
        )
        .await?;
        Ok(self
            .inner
            .decide_with_accounting(request, lineage, Some(&owner))
            .await?)
    }
    async fn check_health(&self) {
        if !self
            .inner
            .status()
            .order
            .iter()
            .any(|slot| slot.locality == openprx::decision::BackendLocality::Loopback)
        {
            self.inner.check_health().await;
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(self.config.causal_tree.policy.extra_latency_budget_ms);
        let owner = super::admin_accounting_scope(
            &self.config,
            &self.inner,
            Arc::clone(&self.key),
            b"health-loopback",
            deadline,
        )
        .await
        .expect("actual private health accounting");
        assert!(
            self.inner.check_health_with_accounting(Some(&owner)).await,
            "health actually observed"
        );
    }
    fn status(&self) -> openprx::decision::factory::BackendPlaneStatus {
        self.inner.status()
    }
}

fn fixture_price(config: &mut Config, catalog: &CatalogView) {
    let selection = openprx::decision::factory::configured_selection(&config.causal_tree)
        .expect("test: configured decision selection");
    let resolved = openprx::providers::selection::check_decision_backend(
        config,
        catalog,
        &selection.provider,
        config.causal_tree.backend.model.trim(),
        selection.credential,
    )
    .expect("test: actual resolved pricing identity");
    let model = resolved.model();
    let pricing_key = if model.contains('/') {
        model.to_owned()
    } else {
        format!("{}/{model}", resolved.provider_label())
    };
    config.cost.prices.insert(
        pricing_key,
        openprx::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
}

fn decision_tranche_micro_usd(config: &Config, tokens: u64) -> u64 {
    use crate::llm::route_decision::{UsagePrice, usage_cost_resolution};
    let selection =
        crate::decision::factory::configured_selection(&config.causal_tree).expect("configured decision selection");
    let catalog = CatalogView::from_config(config).with_curated([CuratedModel {
        operations: Some(vec![ModelOperation::DecisionEndpoint]),
        ..CuratedModel::new(
            ProviderId::of(&selection.provider),
            Some(config.causal_tree.backend.model.trim()),
        )
    }]);
    let resolved = crate::providers::selection::check_decision_backend(
        config,
        &catalog,
        &selection.provider,
        config.causal_tree.backend.model.trim(),
        selection.credential,
    )
    .expect("actual PMM pricing identity");
    let mut maximum = 0.0_f64;
    for (input, output, write, read) in [
        (tokens, 0, 0, 0),
        (0, tokens, 0, 0),
        (tokens, 0, tokens, 0),
        (tokens, 0, 0, tokens),
    ] {
        let UsagePrice::Priced(cost) = usage_cost_resolution(
            resolved.provider_label(),
            resolved.model(),
            input,
            output,
            write,
            read,
            &config.cost,
        ) else {
            panic!("fixture tranche must have valid PMM pricing");
        };
        maximum = maximum.max(cost);
    }
    u64::try_from(crate::cost::tracker::ceil_micro_usd(maximum).expect("production estimate rounding"))
        .expect("bounded tranche estimate")
}

// Reads only this fixture's reservation journal. Exact row count and event
// sequence exclude settlement, release, and extra reservation events.
fn assert_pending_decision_journal(
    config: &Config,
    scope_id: &str,
    token_limit: u64,
    expected_tranches: &[u64],
) -> (String, u64) {
    use std::io::Read as _;

    let path = config.workspace_dir.join("state/costs.reservations.jsonl");
    let file = std::fs::File::open(&path).expect("owned reservation journal");
    assert!(file.metadata().expect("journal metadata").is_file());

    let mut bytes = Vec::new();
    file.take(65_537)
        .read_to_end(&mut bytes)
        .expect("bounded owned journal read");
    assert!(bytes.len() <= 65_536, "fixture journal exceeds bound");
    let raw = String::from_utf8(bytes).expect("journal UTF-8");
    let rows: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("reservation event"))
        .collect();

    assert_eq!(rows.len(), expected_tranches.len() * 2);
    let mut ids = std::collections::BTreeSet::new();
    let mut bindings = std::collections::BTreeSet::new();
    let mut held_money = 0_u64;

    for (pair, expected_tokens) in rows.as_chunks::<2>().0.iter().zip(expected_tranches) {
        assert_eq!(pair[0]["event"], "estimated_reserved_v1");
        assert_eq!(pair[1]["event"], "dispatched");

        let estimate = &pair[0]["request"];
        let reservation = &estimate["reservation"];
        let id = reservation["attempt_id"].as_str().expect("attempt ID");
        assert!(id.starts_with("decision:"));
        assert!(ids.insert(id.to_owned()), "distinct physical attempt IDs");
        assert_eq!(pair[1]["attempt_id"].as_str(), Some(id));
        assert_eq!(reservation["turn_id"].as_str(), Some(scope_id));
        assert_eq!(estimate["turn_token_limit"].as_u64(), Some(token_limit));
        assert_eq!(estimate["reserved_tokens"].as_u64(), Some(*expected_tokens));

        let binding = reservation["binding_sha256"].as_str().expect("estimate binding");
        assert!(bindings.insert(binding.to_owned()), "distinct plan bindings");

        let maximum = reservation["maximum_micro_usd"].as_u64().expect("monetary hold");
        assert!(maximum > 0);
        held_money = held_money.checked_add(maximum).expect("bounded total hold");
    }

    (raw, held_money)
}

#[tokio::test]
#[serial_test::serial]
async fn loopback_retry_retains_two_unknown_decision_liabilities() {
    run_fixture(async {
        let stub = Stub::start(vec![
            Reply::Status(500, Vec::new(), Vec::new()),
            Reply::Status(500, Vec::new(), Vec::new()),
        ])
        .await;
        let plane = build_plane(&stub.endpoint(), 262_144);
        let config = &plane.factory.config;
        let request = request(&plane.key, Duration::from_secs(5));
        let deadline = request
            .manifest()
            .expires_at()
            .min(Instant::now() + Duration::from_millis(config.causal_tree.policy.extra_latency_budget_ms));
        let owner = super::admin_accounting_scope(
            config,
            &plane.factory.inner,
            Arc::clone(&plane.key),
            b"configured-test",
            deadline,
        )
        .await
        .expect("real operator accounting");

        let initial = owner.remaining_budgets();
        assert!(initial.tokens >= 2);
        let first = initial.tokens / 2;
        let second = initial.tokens - first;

        let result = plane
            .factory
            .inner
            .decide_with_accounting(&request, &CausalLineage::default(), Some(&owner))
            .await
            .expect("real configured decision");

        assert_eq!(result.served_by, ServedBy::Builtin);
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Failed(DecisionError::Unavailable),
        );
        assert_eq!(stub.count(), 2, "initial send plus exactly one retry");
        assert_eq!(stub.accepted_count(), 2, "two physical connections");

        let (raw, held) =
            assert_pending_decision_journal(config, &owner.scope_id_for_tests(), initial.tokens, &[first, second]);
        assert_eq!(owner.remaining_budgets().tokens, 0);
        assert_eq!(
            owner.remaining_budgets().micro_usd,
            initial.micro_usd.checked_sub(held).expect("admitted aggregate hold"),
        );

        // Reopen through the actual tracker; never release/reconcile a hold.
        let reopened = crate::cost::tracker::CostTracker::new(config.cost.clone(), &config.workspace_dir)
            .expect("owned tracker reopen");
        assert_eq!(
            reopened.get_summary().expect("reopened summary").daily_cost_usd,
            0.0,
            "unknown usage is not a fabricated settled debit",
        );
        let remaining = crate::cost::tracker::remaining_micro_usd_read_only_with_security_ceiling(
            &config.cost,
            &config.workspace_dir,
            config.autonomy.max_cost_per_day_cents,
        )
        .expect("reopened liability snapshot");
        assert_eq!(remaining, initial.micro_usd - held);
        assert_eq!(
            std::fs::read_to_string(config.workspace_dir.join("state/costs.reservations.jsonl"),)
                .expect("journal after reopen"),
            raw,
            "reopen cannot release or settle either unknown liability",
        );
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn insufficient_retry_money_refuses_second_decision_socket() {
    run_fixture(async {
        let stub = Stub::start(vec![Reply::Status(500, Vec::new(), Vec::new()), Reply::Answer]).await;
        let mut plane = build_plane(&stub.endpoint(), 262_144);

        // Derive the same token allowance used by the real admin issuer.
        let status = plane.factory.inner.status();
        let slot = status.order.first().expect("configured loopback slot");
        let provider = slot.identity.provider.as_ref().expect("resolved provider");
        let effective = crate::router::resolve_effective_compaction_config(
            &plane.factory.config.agent.compaction,
            provider.as_str(),
            &slot.identity.configured_model,
            &plane.factory.config.router,
            &plane.factory.config.model_routes,
        );
        let reserve = crate::runtime::turn_decision::DecisionTokenReservation::from_effective_compaction(
            &effective.config,
            plane.factory.config.causal_tree.policy.extra_token_ratio_limit,
        )
        .expect("real configured token reservation");
        let total = u64::try_from(reserve.decision_tokens).expect("token allowance");
        assert!(total >= 2);
        let first = total / 2;
        let second = total - first;

        let first_money = decision_tranche_micro_usd(&plane.factory.config, first);
        let second_money = decision_tranche_micro_usd(&plane.factory.config, second);

        // Strictly between one and two tranches. Snapshot assertions below
        // verify production binary64 rounding instead of assuming it.
        let limit = (first_money as f64 + second_money as f64 / 2.0) / 1_000_000.0;
        plane.factory.config.cost.daily_limit_usd = limit;
        plane.factory.config.cost.monthly_limit_usd = limit;

        let config = &plane.factory.config;
        let request = request(&plane.key, Duration::from_secs(5));
        let deadline = request
            .manifest()
            .expires_at()
            .min(Instant::now() + Duration::from_millis(config.causal_tree.policy.extra_latency_budget_ms));
        let owner = super::admin_accounting_scope(
            config,
            &plane.factory.inner,
            Arc::clone(&plane.key),
            b"configured-test",
            deadline,
        )
        .await
        .expect("real money-constrained accounting");
        let initial = owner.remaining_budgets();
        assert_eq!(initial.tokens, total);
        assert!(initial.micro_usd >= first_money);
        assert!(initial.micro_usd - first_money < second_money);

        let result = plane
            .factory
            .inner
            .decide_with_accounting(&request, &CausalLineage::default(), Some(&owner))
            .await
            .expect("real configured decision");

        assert_eq!(result.served_by, ServedBy::Builtin);
        assert_eq!(
            result.attempts[0].outcome,
            AttemptOutcome::Failed(DecisionError::BudgetExceeded),
        );
        assert_eq!(stub.count(), 1, "unfunded retry never sends its request");
        assert_eq!(stub.accepted_count(), 1, "retry refused before second socket");

        let (raw, held) = assert_pending_decision_journal(config, &owner.scope_id_for_tests(), total, &[first]);
        assert_eq!(held, first_money);
        assert_eq!(
            owner.remaining_budgets().tokens,
            second,
            "failed monetary admission restores only the unsent retry tranche",
        );
        assert_eq!(owner.remaining_budgets().micro_usd, initial.micro_usd - held);

        let reopened = crate::cost::tracker::CostTracker::new(config.cost.clone(), &config.workspace_dir)
            .expect("owned tracker reopen");
        assert!(matches!(
            reopened
                .check_budget(second_money as f64 / 1_000_000.0)
                .expect("reopened retry budget"),
            crate::cost::types::BudgetCheck::Exceeded { .. },
        ));
        assert_eq!(
            crate::cost::tracker::remaining_micro_usd_read_only_with_security_ceiling(
                &config.cost,
                &config.workspace_dir,
                config.autonomy.max_cost_per_day_cents,
            )
            .expect("reopened liability snapshot"),
            initial.micro_usd - held,
        );
        assert_eq!(
            std::fs::read_to_string(config.workspace_dir.join("state/costs.reservations.jsonl"),)
                .expect("journal after reopen"),
            raw,
        );
    })
    .await;
}
