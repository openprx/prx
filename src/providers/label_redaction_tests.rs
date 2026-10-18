//! URL userinfo in provider labels never reaches a log line, an error or a
//! record.
//!
//! A bring-your-own provider is named by its URL, and an operator may write
//! the credential into it (`custom:http://user:password@host/v1`). The label is
//! redacted once, where the failover chain is built; the real URL stays inside
//! the transport. Every test drives the production factory
//! (`create_provider_for`) against a loopback stub, with a
//! fixture credential that is not real, and asserts that the fixture never
//! appears in what an operator or a user can read — while the request still
//! reaches the endpoint the URL names.

#![allow(clippy::indexing_slicing)]

use super::traits::{ChatMessage, ChatRequest};
use super::{ProviderRuntimeOptions, create_provider_from_test_config};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The fixture password. Not a credential anywhere.
const SECRET: &str = "SECRET-fixture-pw-7c1e";

/// A loopback endpoint that answers every request with one fixed status and
/// counts the requests it received.
pub(super) struct FixedStatusStub {
    pub(super) addr: std::net::SocketAddr,
    pub(super) hits: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FixedStatusStub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub(super) async fn spawn_fixed_status_stub(status_line: &'static str, body: &'static str) -> FixedStatusStub {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind loopback");
    let addr = listener.local_addr().expect("test: loopback address");
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _peer)) = listener.accept().await else {
                return;
            };
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let mut buffer: Vec<u8> = Vec::with_capacity(8192);
                let mut chunk = [0_u8; 4096];
                let head_end = loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                    if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        break position + 4;
                    }
                    if buffer.len() > 1_048_576 {
                        return;
                    }
                };
                let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
                let content_length = head
                    .split("\r\n")
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while buffer.len() < head_end + content_length {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });
    FixedStatusStub { addr, hits, server }
}

/// A `tracing` writer that appends into a shared buffer.
#[derive(Clone)]
struct CaptureWriter(Arc<parking_lot::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Capture every event at `TRACE` and above on this thread until the guard
/// drops. The tests run on the current-thread runtime, so every `.await` in
/// the test body stays on this thread.
fn capture_logs() -> (Arc<parking_lot::Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    let captured = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
    let sink = CaptureWriter(Arc::clone(&captured));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    (captured, tracing::subscriber::set_default(subscriber))
}

fn logged(captured: &parking_lot::Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&captured.lock()).into_owned()
}

fn reliability(retries: u32, fallbacks: &[&str]) -> crate::config::ReliabilityConfig {
    crate::config::ReliabilityConfig {
        provider_retries: retries,
        provider_backoff_ms: 1,
        fallback_providers: fallbacks.iter().map(ToString::to_string).collect(),
        ..crate::config::ReliabilityConfig::default()
    }
}

fn userinfo_label(addr: std::net::SocketAddr) -> String {
    format!("custom:http://alice:{SECRET}@{addr}/v1")
}

fn build(
    provider: &str,
    api_key: Option<&str>,
    reliability: crate::config::ReliabilityConfig,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn super::Provider>> {
    let mut config = crate::config::Config {
        default_provider: Some(provider.to_string()),
        default_model: Some("fixture-model".to_string()),
        api_key: api_key.map(str::to_string),
        reliability,
        ..crate::config::Config::default()
    };
    if let Some(openprx_dir) = &options.openprx_dir {
        config.workspace_dir = openprx_dir.join("workspace");
    }
    create_provider_from_test_config(&config, options)
}

/// R-1: every failed attempt, the all-failed summary and the retry and
/// exhaustion log lines name the provider — none of them may carry the
/// password, whatever its length. The request itself must still reach the
/// endpoint, which proves the transport kept the real URL.
#[tokio::test]
async fn a_failing_chain_names_its_provider_without_url_userinfo() {
    let stub = spawn_fixed_status_stub("500 Internal Server Error", r#"{"error":"upstream down"}"#).await;
    let label = userinfo_label(stub.addr);
    let provider = build(
        &label,
        Some("sk-fixture-not-real"),
        reliability(1, &[]),
        &ProviderRuntimeOptions::default(),
    )
    .expect("test: the userinfo-bearing endpoint constructs");

    let (captured, guard) = capture_logs();
    let messages = vec![ChatMessage::user("hello")];
    let result = provider
        .chat(
            ChatRequest {
                messages: &messages,
                tools: None,
            },
            "fixture-model",
            0.0,
        )
        .await;
    drop(guard);

    let Err(error) = result else {
        panic!("test: a 500 endpoint cannot serve the turn");
    };
    let rendered = format!("{error:#}");
    let logs = logged(&captured);
    assert!(
        stub.hits.load(Ordering::SeqCst) >= 1,
        "the request never reached the endpoint the URL names: {rendered}"
    );
    assert!(
        rendered.contains("All providers/models failed"),
        "not the aggregate: {rendered}"
    );
    assert!(
        rendered.contains(&format!("{}/v1", stub.addr)),
        "the aggregate no longer names the endpoint at all: {rendered}"
    );
    assert!(
        !rendered.contains(SECRET),
        "the aggregate leaked the password: {rendered}"
    );
    assert!(
        !rendered.contains("alice"),
        "the aggregate leaked the user name: {rendered}"
    );
    assert!(
        logs.contains("Exhausted retries"),
        "the capture saw none of the failover log lines: {logs}"
    );
    assert!(
        !logs.contains(SECRET),
        "a failover log line leaked the password: {logs}"
    );
}

/// R-1: `warmup` logs every chain member at INFO, and the channels call it on
/// every channel start, with no failure at all.
#[tokio::test]
async fn warmup_names_the_endpoint_without_url_userinfo() {
    let stub = spawn_fixed_status_stub("200 OK", r#"{"data":[]}"#).await;
    let label = userinfo_label(stub.addr);
    let provider = build(
        &label,
        Some("sk-fixture-not-real"),
        reliability(0, &[]),
        &ProviderRuntimeOptions::default(),
    )
    .expect("test: the userinfo-bearing endpoint constructs");

    let (captured, guard) = capture_logs();
    provider.warmup().await.expect("test: warmup never fails the chain");
    drop(guard);

    let logs = logged(&captured);
    assert!(
        logs.contains("Warming up provider connection pool"),
        "the capture saw no warmup line: {logs}"
    );
    assert!(
        !logs.contains(SECRET),
        "the warmup INFO line leaked the password: {logs}"
    );
}

/// R-1: a chain with no usable member is refused by the factory with every
/// member named. An unparseable URL is still named, and still without its
/// userinfo.
#[test]
fn a_chain_with_no_usable_member_is_refused_without_url_userinfo() {
    // Port 99999 does not parse, so the endpoint is unavailable.
    let label = format!("custom:http://alice:{SECRET}@127.0.0.1:99999/v1");
    let Err(error) = build(
        &label,
        Some("sk-fixture-not-real"),
        reliability(0, &[]),
        &ProviderRuntimeOptions::default(),
    ) else {
        panic!("test: an unparseable endpoint cannot build a chain");
    };
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("is not one PRX can construct"),
        "not the resolver refusal: {rendered}"
    );
    assert!(
        !rendered.contains(SECRET),
        "the refusal leaked the password: {rendered}"
    );
}

/// R-1: a fallback the resolver cannot construct is refused before transport;
/// neither the refusal nor its log may carry the fallback's userinfo.
#[tokio::test]
async fn an_unconstructible_fallback_is_refused_without_url_userinfo() {
    let stub = spawn_fixed_status_stub("500 Internal Server Error", r#"{"error":"upstream down"}"#).await;
    let fallback = format!("custom:http://bob:{SECRET}@127.0.0.1:99999/v1");
    let (captured, guard) = capture_logs();
    let error = match build(
        &format!("custom:http://{}/v1", stub.addr),
        Some("sk-fixture-not-real"),
        reliability(0, &[fallback.as_str()]),
        &ProviderRuntimeOptions::default(),
    ) {
        Ok(_) => panic!("test: the unconstructible fallback must be refused"),
        Err(error) => error,
    };
    drop(guard);

    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("fallback provider 'custom:http://[redacted]@127.0.0.1:99999/v1'"),
        "the fallback is not named in the refusal: {rendered}"
    );
    assert!(
        !rendered.contains(SECRET),
        "the summary leaked the password: {rendered}"
    );
    let logs = logged(&captured);
    assert!(!logs.contains(SECRET), "a log line leaked the password: {logs}");
    assert_eq!(stub.hits.load(Ordering::SeqCst), 0, "the refusal must be pre-transport");
}

/// S-1: a password containing a literal `/` ends the authority early, so the
/// URL does not parse and the userinfo was printed verbatim.
#[test]
fn a_password_with_a_slash_is_redacted_even_though_the_url_does_not_parse() {
    for label in [
        "custom:http://alice:pa/ss@h.invalid/v1",
        "custom:https://alice:pa/ss@h.invalid:8443/v1",
        "anthropic-custom:http://alice:a/b/c@h.invalid",
    ] {
        let redacted = super::redact_endpoint_userinfo(label);
        assert!(
            !redacted.contains("alice") && !redacted.contains("pa/ss") && !redacted.contains("a/b/c"),
            "{label} -> {redacted}"
        );
        assert!(redacted.contains("[redacted]@h.invalid"), "{label} -> {redacted}");
    }
    // A URL that parses, with an `@` only in its path, is left alone.
    assert_eq!(
        super::redact_endpoint_userinfo("custom:http://h.invalid/v1/@scope"),
        "custom:http://h.invalid/v1/@scope"
    );
}

/// The redacted label is what the chain keeps, so it must still classify as
/// the bring-your-own endpoint it names: model-prefix matching and the
/// member-name dialect lookup read it.
#[test]
fn a_redacted_label_is_still_the_same_kind_of_endpoint() {
    let redacted = super::redact_endpoint_userinfo("custom:http://alice:pw@127.0.0.1:8080/v1");
    assert!(matches!(
        super::classify_provider(&redacted),
        super::ProviderIdentity::ByoEndpoint { .. }
    ));
    assert!(super::provider_matches_model_prefix(&redacted, "org/model"));
    let redacted = super::redact_endpoint_userinfo("anthropic-custom:https://alice:pw@h.invalid");
    assert_eq!(
        super::byo_dialect_of(&redacted),
        Some(super::ByoDialect::AnthropicCompatible)
    );
}

/// The route decision names the configured label and the chain reports the
/// redacted one; a clean first-attempt success through a userinfo-bearing
/// endpoint must still be recorded as a clean success, not as a fallback, and
/// the recorded outcome carries no userinfo.
#[tokio::test]
async fn a_clean_success_through_a_userinfo_endpoint_is_not_a_fallback() {
    use crate::llm::route_decision::{ExecutionStatus, RouteDecision};

    let stub = spawn_fixed_status_stub(
        "200 OK",
        r#"{"id":"c","object":"chat.completion","created":1,"model":"fixture-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
    )
    .await;
    let label = userinfo_label(stub.addr);
    let provider = build(
        &label,
        Some("sk-fixture-not-real"),
        reliability(0, &[]),
        &ProviderRuntimeOptions::default(),
    )
    .expect("test: the userinfo-bearing endpoint constructs");
    let decision = RouteDecision::single_candidate(&label, "fixture-model");
    let messages = vec![ChatMessage::user("hello")];
    let (_, outcome) = provider
        .chat_with_decision(
            &decision,
            ChatRequest {
                messages: &messages,
                tools: None,
            },
            0.0,
        )
        .await
        .expect("test: the stub serves the turn");
    assert_eq!(outcome.status, ExecutionStatus::Success, "{outcome:?}");
    let recorded = serde_json::to_string(&outcome).expect("test: outcome serializes");
    assert!(
        !recorded.contains(SECRET),
        "the recorded outcome leaked the password: {recorded}"
    );
}

/// Error text that quotes several labels (an aggregate, an availability
/// reason) is scrubbed wherever it is sanitized: `sanitize_api_error` feeds
/// the channel failure replies and the gateway error bodies. Redaction is
/// idempotent, so text that passes through twice is unchanged the second
/// time.
#[test]
fn sanitized_error_text_carries_no_url_userinfo() {
    let text = format!(
        "No available providers: custom:http://alice:{SECRET}@h.invalid/v1: invalid provider; \
         [custom:https://bob:{SECRET}@k.invalid:8443/v1, openrouter] `anthropic-custom:http://c:{SECRET}@m.invalid`"
    );
    let sanitized = super::sanitize_api_error(&text);
    assert!(!sanitized.contains(SECRET), "{sanitized}");
    let redacted = super::redact_endpoint_userinfo(&text);
    assert!(!redacted.contains(SECRET), "{redacted}");
    for endpoint in [
        "custom:http://[redacted]@h.invalid/v1",
        "custom:https://[redacted]@k.invalid:8443/v1",
        "anthropic-custom:http://[redacted]@m.invalid",
    ] {
        assert!(redacted.contains(endpoint), "{endpoint} missing from {redacted}");
    }
    assert_eq!(super::redact_endpoint_userinfo(&redacted), redacted);
}

/// USERINFO-2: the agent loop, the chat surfaces and the gateway name the
/// sender or recipient of a conversation event `<provider>/<model>`. The
/// event is durable, so the label is redacted where it is written: every
/// writer that goes through `MemoryFabric` stores the endpoint without its
/// userinfo. Rows written before this change are not migrated.
#[tokio::test]
async fn a_conversation_event_names_its_provider_without_url_userinfo() {
    use crate::memory::{Memory, MemoryVisibility, MessageEventScope, SqliteMemory};
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).expect("test: sqlite"));
    let workspace = dir.path().to_string_lossy().to_string();
    let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.clone());
    let label = format!("custom:http://u:{SECRET}@127.0.0.1:1/v1/fixture-model");
    let scope = MessageEventScope::new("agent", MemoryVisibility::Workspace)
        .with_session_key("fixture-session")
        .with_sender(label.clone());
    let recorded = fabric
        .record_assistant_message(scope.clone().with_recipient(label.clone()), "reply")
        .await
        .expect("test: the event is written");
    assert_eq!(
        recorded.sender.as_deref(),
        Some("custom:http://[redacted]@127.0.0.1:1/v1/fixture-model")
    );
    assert!(!format!("{recorded:?}").contains(SECRET), "{recorded:?}");

    let stored = memory
        .list_message_events_recent(
            &crate::memory::MemoryPrincipal {
                require_owner_match: false,
                workspace_id: workspace,
                agent_id: None,
                persona_id: None,
                session_key: None,
                channel: None,
                sender: None,
                owner_id: None,
                legacy_session_key: None,
            },
            10,
        )
        .await
        .expect("test: events readable");
    assert!(!stored.is_empty());
    let stored = format!("{stored:?}");
    assert!(!stored.contains(SECRET), "the stored event kept the password: {stored}");
}

// ── R-C2BU F-1: passwords the tokenising redactor split apart ──────────

/// Every label here is one the transport accepts or would accept, with a
/// password (or authority form) that the old redactor split on and passed
/// through verbatim. Each entry is `(label, secret)`: the secret must never
/// be printed by any sink.
pub(crate) const HOSTILE_USERINFO: &[(&str, &str)] = &[
    ("custom:http://alice:ab'cd-7f@127.0.0.1:1/v1", "ab'cd-7f"),
    ("custom:http://alice:ab\"cd-7f@127.0.0.1:1/v1", "ab\"cd-7f"),
    ("custom:http://alice:ab`cd-7f@127.0.0.1:1/v1", "ab`cd-7f"),
    ("custom:http://alice:ab cd-7f@127.0.0.1:1/v1", "ab cd-7f"),
    ("custom:http://alice:ab<cd-7f@127.0.0.1:1/v1", "ab<cd-7f"),
    ("custom:http://alice:ab>cd-7f@127.0.0.1:1/v1", "ab>cd-7f"),
    ("custom:http://alice:ab/cd-7f@127.0.0.1:1/v1", "ab/cd-7f"),
    ("custom:http://alice:ab@cd-7f@127.0.0.1:1/v1", "ab@cd-7f"),
    ("custom:http://alice:ab:cd-7f@127.0.0.1:1/v1", "ab:cd-7f"),
    ("custom:http://alice:ab%40cd-7f@127.0.0.1:1/v1", "ab%40cd-7f"),
    ("custom:http:\\\\alice:pw-back-7f@127.0.0.1:1/v1", "pw-back-7f"),
    ("custom:http:/\\alice:pw-mixed-7f@127.0.0.1:1/v1", "pw-mixed-7f"),
    ("custom:http://alice:ab://cd-7f@127.0.0.1:1/v1", "cd-7f"),
    (
        "custom:https://alice:a' b\"c<d>e`f-7f@[::1]:8443/v1",
        "a' b\"c<d>e`f-7f",
    ),
    ("anthropic-custom:http://alice:ab'cd-7f@127.0.0.1:1", "ab'cd-7f"),
    // R-C2BUf F-1b: the parser accepts any number of `/` or `\` (none
    // included) after a special scheme's colon, and ignores tab, LF and CR
    // anywhere in the input.
    ("custom:http:/alice:pw-one-slash-7f@127.0.0.1:1/v1", "pw-one-slash-7f"),
    ("custom:http:alice:pw-no-slash-7f@127.0.0.1:1/v1", "pw-no-slash-7f"),
    (
        "custom:http:\\alice:pw-one-backslash-7f@127.0.0.1:1/v1",
        "pw-one-backslash-7f",
    ),
    (
        "custom:http:///alice:pw-three-slash-7f@127.0.0.1:1/v1",
        "pw-three-slash-7f",
    ),
    (
        "custom:http\t://alice:pw-tab-scheme-7f@127.0.0.1:1/v1",
        "pw-tab-scheme-7f",
    ),
    (
        "custom:http:/\t/alice:pw-tab-separator-7f@127.0.0.1:1/v1",
        "pw-tab-separator-7f",
    ),
    ("custom:http:\n//alice:pw-newline-7f@127.0.0.1:1/v1", "pw-newline-7f"),
    (
        "custom:ht\r\ntp://alice:pw-crlf-scheme-7f@127.0.0.1:1/v1",
        "pw-crlf-scheme-7f",
    ),
];

fn assert_hidden(sink: &str, label: &str, secret: &str, printed: &str) {
    assert!(
        !printed.contains(secret),
        "{sink} printed the password of {label:?}: {printed}"
    );
    assert!(
        !printed.contains("alice"),
        "{sink} printed the user of {label:?}: {printed}"
    );
}

/// The helper itself, bare and quoted inside free text, and every text sink
/// that goes through it.
#[test]
fn hostile_passwords_are_redacted_by_the_helper_and_the_error_sanitizer() {
    for (label, secret) in HOSTILE_USERINFO {
        assert_hidden(
            "redact_endpoint_userinfo",
            label,
            secret,
            &super::redact_endpoint_userinfo(label),
        );
        let quoted = format!("Primary provider \"{label}\" unavailable; [{label}, openrouter] `{label}` <{label}>");
        assert_hidden("free text", label, secret, &super::redact_endpoint_userinfo(&quoted));
        assert_hidden("sanitize_api_error", label, secret, &super::sanitize_api_error(&quoted));
        let mut config = toml::Value::Table(toml::map::Map::new());
        if let toml::Value::Table(table) = &mut config {
            table.insert("default_provider".into(), toml::Value::String((*label).to_string()));
        }
        crate::config::secret_paths::redact(&mut config);
        assert_hidden("config show", label, secret, &config.to_string());
        let mut scanned = toml::map::Map::new();
        scanned.insert(
            "default_provider".to_string(),
            toml::Value::String((*label).to_string()),
        );
        let scan = crate::config::secret_paths::scan(&toml::Value::Table(scanned));
        assert_hidden("scan scrub", label, secret, &scan.scrub(&format!("rejected {label}")));
    }
}

/// The failover chain built by the factory: member names, the all-failed
/// summary and every log line, for every hostile form the factory accepts.
#[tokio::test]
async fn hostile_passwords_never_reach_the_failover_chain_output() {
    let stub = spawn_fixed_status_stub("500 Internal Server Error", r#"{"error":"down"}"#).await;
    for (label, secret) in HOSTILE_USERINFO {
        let label = label.replace("127.0.0.1:1", &stub.addr.to_string());
        let (captured, guard) = capture_logs();
        let built = build(
            &label,
            Some("sk-fixture-not-real"),
            reliability(0, &[]),
            &ProviderRuntimeOptions::default(),
        );
        let rendered = match built {
            Ok(provider) => {
                provider.warmup().await.ok();
                let messages = vec![ChatMessage::user("hi")];
                match provider
                    .chat(
                        ChatRequest {
                            messages: &messages,
                            tools: None,
                        },
                        "m",
                        0.0,
                    )
                    .await
                {
                    Ok(_) => String::new(),
                    Err(error) => format!("{error:#}"),
                }
            }
            Err(error) => format!("{error:#}"),
        };
        drop(guard);
        assert_hidden("failover chain", &label, secret, &rendered);
        assert_hidden("chain logs", &label, secret, &logged(&captured));
    }
}

/// R-C2BU F-4: the fabric redacts provider labels only. A sender that is an
/// identity (a webhook `reply_target`, a platform subject) is the row's
/// identity column and is stored as given, so its owner still reads its own
/// private rows by it.
#[tokio::test]
async fn an_identity_sender_is_stored_as_given_and_its_owner_reads_it_back() {
    use crate::memory::{Memory, MemoryVisibility, MessageEventScope, SqliteMemory};
    let dir = tempfile::TempDir::new().expect("test: temp dir");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(dir.path()).expect("test: sqlite"));
    let workspace = dir.path().to_string_lossy().to_string();
    let fabric = crate::memory::MemoryFabric::new(Arc::clone(&memory), workspace.clone());
    let sender = "https://tok@hooks.example/cb";
    let scope = MessageEventScope::new("webhook", MemoryVisibility::Private)
        .with_owner_id("owner:webhook-caller")
        .with_session_key("webhook:caller")
        .with_sender(sender);
    let recorded = fabric
        .record_inbound_user_message(scope, "hello", None, None)
        .await
        .expect("test: the event is written");
    assert_eq!(recorded.sender.as_deref(), Some(sender));

    let own = memory
        .list_message_events_since(
            &crate::memory::MemoryPrincipal {
                require_owner_match: true,
                workspace_id: workspace,
                agent_id: None,
                persona_id: None,
                session_key: Some("webhook:caller".to_string()),
                channel: None,
                sender: Some(sender.to_string()),
                owner_id: Some("owner:webhook-caller".to_string()),
                legacy_session_key: None,
            },
            0,
            10,
        )
        .await
        .expect("test: events readable");
    assert_eq!(own.len(), 1, "the owner no longer reads its own private row: {own:?}");
}

// ── R-C2BUg G-1: free text that begins with a URL ─────────────────────

/// Free text holding several credentialed URLs, the first at the very start
/// (so the whole text parses as one URL), the others bare, quoted or after
/// `=`. Every secret must be redacted, not only the first.
pub(crate) const MULTI_URL_TEXTS: &[(&str, &[&str])] = &[
    (
        "https://a:pw1-g1@h/x\nDATABASE_URL=postgres://u:pw3-g1@db/app",
        &["pw1-g1", "pw3-g1"],
    ),
    (
        "custom:http://alice:pw1-g1@h/v1 failed; see http://bob:pw2-g1@h2/v1",
        &["pw1-g1", "pw2-g1"],
    ),
    (
        "http://alice:pw1-g1@h/v1: error sending request for url (http://alice:pw2-g1@h/v1/chat/completions)",
        &["pw1-g1", "pw2-g1"],
    ),
    (
        "https://a:pw1-g1@h/x {\"k\": \"http://b:pw2-g1@h\"} then 'ws://c:pw3-g1@h3/s' and URL=ftp://d:pw4-g1@f/p",
        &["pw1-g1", "pw2-g1", "pw3-g1", "pw4-g1"],
    ),
];

/// Assert that `render` hides every secret of every multi-URL text.
pub(crate) fn assert_every_url_redacted(sink: &str, render: &dyn Fn(&str) -> String) {
    for (text, secrets) in MULTI_URL_TEXTS {
        let rendered = render(text);
        for secret in *secrets {
            assert!(
                !rendered.contains(secret),
                "{sink} printed {secret} from {text:?}: {rendered}"
            );
        }
    }
}

#[test]
fn every_userinfo_in_url_first_free_text_is_redacted() {
    assert_every_url_redacted("redact_endpoint_userinfo", &|text| {
        super::redact_endpoint_userinfo(text).into_owned()
    });
    assert_every_url_redacted("sanitize_api_error (error bodies)", &super::sanitize_api_error);
    assert_every_url_redacted(
        "security::redaction::redact_secrets",
        &crate::security::redaction::redact_secrets,
    );
}

// ── R-C2BUh H-1: a URL right after a tab, LF or CR ─────────────────────

/// The token before a tab, LF or CR must not be read as part of the next
/// URL's scheme.
pub(crate) const BREAK_BEFORE_URL_TEXTS: &[(&str, &[&str])] = &[
    ("HTTP 502\nhttps://u:pw-h1a@h/v1", &["pw-h1a"]),
    ("id\t1\thttps://u:pw-h1b@h/x", &["pw-h1b"]),
    ("error: bad gateway\nhttp:/u:pw-h1c@h/v1", &["pw-h1c"]),
    ("upstream proxy:8080\nhttp://alice:pw-h1d@h/v1", &["pw-h1d"]),
    ("Step 1.\nhttps://alice:pw-h1e@h/v1", &["pw-h1e"]),
    ("---\r\nhttps://alice:pw-h1f@h/v1", &["pw-h1f"]),
];

#[test]
fn a_url_after_a_tab_or_line_break_is_redacted() {
    for (text, secrets) in BREAK_BEFORE_URL_TEXTS {
        for (sink, rendered) in [
            (
                "redact_endpoint_userinfo",
                super::redact_endpoint_userinfo(text).into_owned(),
            ),
            ("sanitize_api_error", super::sanitize_api_error(text)),
        ] {
            for secret in *secrets {
                assert!(
                    !rendered.contains(secret),
                    "{sink} printed {secret} from {text:?}: {rendered}"
                );
            }
        }
    }
}

/// A small deterministic PRNG (xorshift64*), so the property test needs no
/// new dependency and every run sees the same texts.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % (bound as u64)).unwrap_or(0)
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// Property test for the free-text redactor, which regressed three times
/// across fix-forwards (R-C2BUg G-1, R-C2BUh H-1, R-C2BUi I-1).
///
/// It generates free texts that mix credentialed URLs with words, digits,
/// punctuation, quotes, tab/LF/CR and non-ASCII text (three-byte Hangul,
/// emoji, accented letters, typographic punctuation, NBSP) and asserts for every text:
/// (a) no password the URL parser reads survives; (b) redacting the output
/// again changes nothing; (c) no doubled marker appears. Redaction must also
/// not panic (a panic fails the test).
///
/// URLs use special schemes (`http`, `https`, `ws`, `wss`, `ftp`, in mixed
/// case, with 0 to 3 `/` or `\`, optionally behind `custom:` or
/// `anthropic-custom:`) and non-special ones (`postgres`, `redis`,
/// `mongodb`, `amqp`, `socks5`, with `//`), empty and non-empty users
/// (including `CORP\svc`), and passwords holding quotes, spaces, `@`, `:`,
/// `/`, `\`, `?`, `#` and non-ASCII characters. A password counts as read
/// when the URL on its own parses with that password (so `/`, `?`, `#`, and
/// `\` after a special scheme, which end the authority, are generated but
/// only asserted where the parser reads them). Fixed seed, so a failure
/// reproduces exactly.
#[test]
fn free_text_redaction_holds_for_generated_texts() {
    const CASES: usize = 40_000;
    let mut rng = Xorshift(0x5EED_C2B0_1234_ABCD);
    let special_schemes = ["http", "https", "HTTP", "Https", "ws", "wss", "ftp"];
    let other_schemes = ["postgres", "redis", "mongodb", "amqp", "socks5", "Postgres"];
    let prefixes = ["", "", "", "custom:", "anthropic-custom:", "CUSTOM:"];
    let slashes = ["//", "//", "/", "", "\\", "\\\\", "/\\", "///"];
    let users = ["alice", "u", "", "a.b-c", "CORP\\svc", "\u{ac0d}\u{ac4d}"];
    let password_extras = [
        "",
        "",
        "'",
        "\"",
        " ",
        "<",
        ">",
        "@",
        ":",
        "`",
        "!",
        "$",
        "&",
        "*",
        "+",
        ",",
        ";",
        "=",
        "~",
        "%40",
        "/",
        "\\",
        "?",
        "#",
        "é",
        "\u{acc6}\u{ac01}",
        "😀",
        // R-C2BUj J-2: scheme words inside a password.
        "ws:",
        "wss:Q",
        "ftp:",
        "http:",
        "-https:",
        "1.ws:",
        "x:\\\\y",
    ];
    let hosts = ["h", "h.example", "127.0.0.1:8080", "[::1]:8443", "db", "bücher.example"];
    let tails = ["/v1", "/v1/chat/completions", "/x?q=1", "#frag", "", "/app"];
    let words = [
        "HTTP",
        "502",
        "error",
        "gateway",
        "id",
        "1",
        "Step",
        "1.",
        "---",
        "8080",
        "proxy:8080",
        "x=y",
        "=>",
        "DATABASE_URL=",
        "see",
        "(",
        ")",
        "'",
        "\"",
        "[",
        "]",
        "{",
        "}",
        "http",
        "ht",
        "tp",
        "\u{ac19}\u{acef}:",
        "\u{ac0d}\u{acf0}:",
        "é:",
        "résumé",
        "→",
        "😀",
        "\u{a0}",
        "\u{2022}",
        "\u{2236}",
        "\u{ac31}\u{ac25}",
        "€5:",
    ];
    let separators = [
        " ", " ", "\n", "\t", "\r\n", "\r", ", ", "; ", "=", "(", "'", "\"", ": ", "\u{a0}", "\u{2022}", "\u{2236}",
        "→",
    ];
    let mut failures = Vec::new();
    for case in 0..CASES {
        let mut text = String::new();
        let mut secrets = Vec::new();
        for piece in 0..(2 + rng.below(8)) {
            if piece > 0 {
                text.push_str(rng.pick(&separators));
            }
            if rng.below(3) != 0 {
                text.push_str(rng.pick(&words));
                continue;
            }
            let special = rng.below(4) != 0;
            let secret = format!("Pw{case:05}x{piece}{}{}z", rng.pick(&password_extras), rng.below(1000));
            let mut url = String::new();
            if special {
                text.push_str(rng.pick(&prefixes));
                url.push_str(rng.pick(&special_schemes));
                url.push(':');
                url.push_str(rng.pick(&slashes));
            } else {
                url.push_str(rng.pick(&other_schemes));
                url.push_str("://");
            }
            url.push_str(rng.pick(&users));
            url.push(':');
            url.push_str(&secret);
            url.push('@');
            url.push_str(rng.pick(&hosts));
            url.push_str(rng.pick(&tails));
            // Assert only a password the parser itself reads from this URL.
            if reqwest::Url::parse(&url).is_ok_and(|parsed| parsed.password().is_some_and(|read| !read.is_empty())) {
                secrets.push(secret);
            }
            text.push_str(&url);
        }
        let once = super::redact_endpoint_userinfo(&text).into_owned();
        let twice = super::redact_endpoint_userinfo(&once).into_owned();
        let leaked = secrets.iter().any(|secret| once.contains(secret.as_str()));
        if leaked || twice != once || once.contains("[redacted]@[redacted]") || once.contains("[redacted][redacted]") {
            failures.push(format!("{text:?}\n  -> {once:?}\n  -> {twice:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {CASES} generated texts failed; first 5:\n{}",
        failures.len(),
        failures.iter().take(5).cloned().collect::<Vec<_>>().join("\n")
    );
}

// ── R-C2BUi I-1: non-ASCII text must never panic ──────────────────────

/// Non-ASCII text around `:` and `@` (three-byte Hangul syllables standing in
/// for East-Asian text, emoji, accented letters, typographic punctuation,
/// NBSP; the source tree carries no CJK code points). The redactor used to slice
/// one byte after a multibyte character and panic; release builds abort.
pub(crate) const NON_ASCII_TEXTS: &[&str] = &[
    "\u{ac19}\u{acef}:alice@example.com",
    "upstream: \u{ac19}\u{acef}:alice@example.com",
    "upstream said \u{ac0d}\u{acf0}:x a@b",
    "\u{a0}https://u:pw-i1a@h/",
    "é:https://u:pw-i1b@h/v1",
    "résumé: see 😀:a@b and →:c@d",
    "\u{2022}https://u:pw-i1c@h/x\u{204f}\u{ac8d}\u{acd5}",
    "🔥🔥:🔥@🔥 http://u:pw-i1d@h/x 🔥",
    "€:@",
    "\u{ac2d}\u{ac87}http:/u:pw-i1e@h/v1",
];

#[test]
fn non_ascii_text_never_panics_and_is_still_redacted() {
    for text in NON_ASCII_TEXTS {
        let sanitized = super::sanitize_api_error(text);
        let redacted = super::redact_endpoint_userinfo(text).into_owned();
        for secret in ["pw-i1a", "pw-i1b", "pw-i1c", "pw-i1d", "pw-i1e"] {
            assert!(!sanitized.contains(secret), "{text:?} -> {sanitized}");
            assert!(!redacted.contains(secret), "{text:?} -> {redacted}");
        }
    }
}

// ── R-C2BUi I-2 and the glued-scheme limit ─────────────────────────────

/// A `\` in the userinfo of a non-special scheme is part of the userinfo
/// (the parser ends that authority only at `/`, `?` or `#`), and a scheme
/// glued to digits, `.`, `+` or `-` is still found.
pub(crate) const SCHEME_EDGE_TEXTS: &[(&str, &[&str])] = &[
    (
        "error: DATABASE_URL=postgres://CORP\\svc:secret-i2a@db:5432/app",
        &["secret-i2a", "CORP\\svc"],
    ),
    ("redis://CORP\\svc:secret-i2b@h/0", &["secret-i2b"]),
    ("redis://u:12\\secret-i2c@h/0", &["12\\secret-i2c"]),
    ("1.https://alice:secret-i2d@h/v1", &["secret-i2d"]),
    ("502https://alice:secret-i2e@h/v1", &["secret-i2e"]),
    ("--https://alice:secret-i2f@h/v1", &["secret-i2f"]),
    ("[::1]:8443https://alice:secret-i2g@h/v1", &["secret-i2g"]),
    ("failed: see https: alice:secret-i2h@h/v1", &["secret-i2h"]),
    ("http://a:p?1@h ws://b:@secret-i3\n@h2/", &["@secret-i3"]),
    // R-C2BUk K-1: a special scheme glued to a letter, written with `\`.
    ("xhttps:\\\\u:pw2-k1@h/", &["pw2-k1"]),
    ("xhttps:/\\u:pw3-k1@h/", &["pw3-k1"]),
    ("completionshTTp\nS:\\\\bZ:pw6-k1@10.0.0.1#f", &["pw6-k1"]),
];

#[test]
fn backslash_userinfo_glued_schemes_and_whitespace_spellings_are_redacted() {
    for (text, secrets) in SCHEME_EDGE_TEXTS {
        let once = super::redact_endpoint_userinfo(text).into_owned();
        for secret in *secrets {
            assert!(!once.contains(secret), "{text:?} -> {once}");
        }
        assert_eq!(
            super::redact_endpoint_userinfo(&once),
            once,
            "not idempotent for {text:?}"
        );
    }
}

/// R-C2BUk K-1: the glued special scheme is taken only with two separators,
/// so prose that merely ends a word in a scheme name is left alone.
#[test]
fn a_glued_special_scheme_needs_two_separators() {
    for text in [
        "Reviews: contact bob@example.com",
        "news:alice@example.com",
        "xhttp:/u:pw-k1n@h",
    ] {
        assert_eq!(super::redact_endpoint_userinfo(text), text, "{text:?}");
    }
}

/// Never panics, over arbitrary UTF-8 built from the characters the scanner
/// branches on (`:`, `@`, `/`, `\`, `?`, `#`, tab, LF, CR, letters, digits)
/// and multibyte characters of every width. Fixed seed.
#[test]
fn the_redactor_never_panics_on_arbitrary_utf8() {
    const CASES: usize = 30_000;
    let pool: Vec<char> =
        "ab:@/\\?#\t\n\r .+-hHtTpPsS0189[]%'\"\u{ac19}\u{acef}\u{ac0d}\u{acf0}é€😀→\u{2022}\u{a0}\u{2028}ÿ𝄞"
            .chars()
            .collect();
    let mut rng = Xorshift(0x000C_2B01_0000_0001);
    for _ in 0..CASES {
        let length = rng.below(40);
        let text: String = (0..length).map(|_| pool[rng.below(pool.len())]).collect();
        let outcome = std::panic::catch_unwind(|| {
            let once = super::redact_endpoint_userinfo(&text).into_owned();
            let _ = super::sanitize_api_error(&text);
            let _ = super::redact_endpoint_userinfo(&once);
        });
        assert!(outcome.is_ok(), "the redactor panicked on {text:?}");
    }
}

// ── R-C2BUj J-2 / J-3: scheme words inside a password ─────────────────

/// A password that holds a special scheme word with a colon (`wss:`, `ftp:`,
/// `http:`) or `x:\\` is one password to the parser; it must be redacted
/// whole, as a bare label and in free text.
pub(crate) const SCHEME_WORD_IN_PASSWORD: &[(&str, &[&str])] = &[
    (
        "custom:https://svc:k9-wss:Qsecret-j2a@h/v1",
        &["k9-wss", "Qsecret-j2a", "svc:"],
    ),
    ("redis://u:secret-j2b-ftp:tail@h/0", &["secret-j2b", "tail"]),
    ("http://u:Pa55.http:x-j2c@h/", &["Pa55", "x-j2c"]),
    ("socks5://u:x:\\\\y-j2d@h", &["y-j2d"]),
    (
        "error: upstream custom:https://svc:k9-wss:Qsecret-j2e@h/v1 refused",
        &["k9-wss", "Qsecret-j2e"],
    ),
    // J-3: a URL whose scheme starts inside the previous authority.
    (
        "HTTP 502\n://, gateway\n:// \tm\nongodb+sr\tv://-!:Secret-j3,%2FZ@db:5432/p?q#f",
        &["Secret-j3"],
    ),
];

#[test]
fn a_scheme_word_inside_a_password_does_not_cut_it_short() {
    for (text, secrets) in SCHEME_WORD_IN_PASSWORD {
        for (sink, rendered) in [
            (
                "redact_endpoint_userinfo",
                super::redact_endpoint_userinfo(text).into_owned(),
            ),
            ("sanitize_api_error", super::sanitize_api_error(text)),
        ] {
            for secret in *secrets {
                assert!(
                    !rendered.contains(secret),
                    "{sink} printed {secret} from {text:?}: {rendered}"
                );
            }
        }
    }
}

// ── R-C2BUj J-1: redaction time must stay linear ───────────────────────

/// The adversarial shapes the review measured, each repeated to `size` bytes
/// with a single `@` somewhere so the early return does not apply.
fn adversarial_body(shape: &str, size: usize) -> String {
    let repeat = |unit: &str| {
        let mut text = String::with_capacity(size + unit.len());
        while text.len() < size {
            text.push_str(unit);
        }
        text
    };
    match shape {
        "http_no_at_then_at" => repeat("http:x/ ") + "@",
        "https_no_at_then_at" => repeat("https://h/ ") + "@",
        "non_special_no_at_then_at" => repeat("redis://h/ ") + "@",
        "html_links" => repeat("<a href=\"https://example.com/docs/page\">docs</a>\n") + "support@example.com",
        "digit_letter_run" => repeat("1a") + ":x@h",
        "tab_letter_run" => repeat("a\t") + ":x@h",
        "at_first_http_no_delimiter" => format!("@ {}", repeat("http:x ")),
        "at_first_https_no_delimiter" => format!("@ {}", repeat("https://x ")),
        "unparseable_then_at" => repeat("http://a b/ ") + "@",
        "long_authorities_then_at" => repeat("https://x ") + "/@",
        "credentialed_each" => repeat("http://u:p@h/ "),
        _ => String::new(),
    }
}

/// Run `work` on its own thread and give up after `limit`: a quadratic
/// redactor must fail this test, not hang the suite.
fn timed(limit: std::time::Duration, work: impl FnOnce() + Send + 'static) -> Option<std::time::Duration> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        work();
        let _ = sender.send(started.elapsed());
    });
    receiver.recv_timeout(limit).ok()
}

const J1_SHAPES: &[&str] = &[
    "http_no_at_then_at",
    "https_no_at_then_at",
    "non_special_no_at_then_at",
    "html_links",
    "digit_letter_run",
    "tab_letter_run",
    "at_first_http_no_delimiter",
    "at_first_https_no_delimiter",
    "unparseable_then_at",
    "long_authorities_then_at",
    "credentialed_each",
];

/// The redactor's work grows linearly on every adversarial shape (R-C2BUj
/// J-1), measured as a deterministic count of the bytes it examines
/// (`userinfo_scan_work`), not by a clock (R-C2BUk K-4): the wall-clock
/// version failed under load and its timer floor let a 15x regression pass.
///
/// Quadrupling the input may at most quadruple the work, plus a small
/// constant for what does not scale with the input; a quadratic scan gives
/// about 16x. The absolute bound (a fixed number of bytes examined per input
/// byte) catches a constant-factor regression the ratio alone would miss.
#[test]
fn redaction_work_grows_linearly_on_adversarial_bodies() {
    const SMALL: usize = 8 * 1024;
    const LARGE: usize = 4 * SMALL;
    const SLACK: usize = 4 * 1024;
    const WORK_PER_BYTE: usize = 16;
    for shape in J1_SHAPES {
        let small_text = adversarial_body(shape, SMALL);
        let large_text = adversarial_body(shape, LARGE);
        let small = super::userinfo_scan_work(&small_text);
        let large = super::userinfo_scan_work(&large_text);
        assert!(
            large <= 4 * small + SLACK,
            "{shape}: {} bytes cost {small}, {} bytes cost {large} (more than 4x)",
            small_text.len(),
            large_text.len()
        );
        assert!(
            large <= WORK_PER_BYTE * large_text.len(),
            "{shape}: {} bytes cost {large}, more than {WORK_PER_BYTE} per byte",
            large_text.len()
        );
    }
}

/// A 100 KB and a 1 MB upstream error body of every adversarial shape is
/// sanitized promptly (the sanitizer only ever prints 200 characters).
#[test]
fn large_adversarial_error_bodies_are_sanitized_promptly() {
    let limit = std::time::Duration::from_secs(10);
    for size in [100 * 1024, 1024 * 1024] {
        for shape in J1_SHAPES {
            let body = adversarial_body(shape, size);
            let took = timed(limit, move || {
                std::hint::black_box(super::sanitize_api_error(&body).len());
            });
            assert!(
                took.is_some(),
                "sanitizing a {size}-byte {shape} body took longer than {limit:?}"
            );
        }
    }
}

/// The sanitizer scrubs a bounded prefix and still marks a cut text, and an
/// error body read from an upstream is capped in memory.
#[tokio::test]
async fn the_error_sanitizer_and_the_error_body_reader_are_bounded() {
    let long = format!("custom:http://u:{SECRET}@h/v1 {}", "x".repeat(40 * 1024));
    let sanitized = super::sanitize_api_error(&long);
    assert!(!sanitized.contains(SECRET), "{sanitized}");
    assert!(sanitized.ends_with("..."), "{sanitized}");

    let huge = "e".repeat(super::MAX_ERROR_BODY_BYTES * 4);
    let body: &'static str = Box::leak(huge.into_boxed_str());
    let stub = spawn_fixed_status_stub("500 Internal Server Error", body).await;
    let response = reqwest::Client::new()
        .get(format!("http://{}/", stub.addr))
        .send()
        .await
        .expect("test: loopback responds");
    let read = super::read_error_body(response).await;
    assert_eq!(read.len(), super::MAX_ERROR_BODY_BYTES, "the error body was not capped");
}

/// The one remaining cut shape, documented as a limit, redacts everything
/// from the inner URL on: the part of the password after the inner scheme is
/// never printed, and the output is stable.
#[test]
fn the_documented_inner_url_limit_is_stable() {
    let text = "redis://u:ws:\\\\inner-secret@b@h/0";
    let once = super::redact_endpoint_userinfo(text).into_owned();
    assert!(!once.contains("inner-secret"), "{once}");
    assert_eq!(super::redact_endpoint_userinfo(&once), once);
}

/// SP-INT2 a35: the refused-provider WARN prints the model-supplied provider
/// escaped and bounded. It used `Display`, so a `\n` in the requested string
/// started a forged log line and 10 KB of it reached the log whole.
#[test]
fn a_refused_provider_is_logged_escaped_and_bounded() {
    let configured = super::ConfiguredProviders::default().with_providers(["openai"]);
    let requested = format!(
        "custom:http://u:{SECRET}@attacker.example/v1?x\nFAKE LINE level=ERROR {}",
        "a".repeat(10 * 1024)
    );
    let (captured, guard) = capture_logs();
    let refused = configured.ensure_permits(&requested);
    drop(guard);
    assert!(refused.is_err(), "an unconfigured endpoint is refused");
    let log = logged(&captured);
    assert!(log.contains("refused a per-call provider"), "{log}");
    assert!(
        !log.contains("\nFAKE LINE"),
        "a newline in the requested provider forged a log line: {log}"
    );
    assert!(
        log.len() < 2 * 1024,
        "the requested provider must be bounded in the log; {} bytes were logged",
        log.len()
    );
    assert!(!log.contains(SECRET), "{log}");
}

/// USERINFO-3 (providers): an unknown provider label is named in the
/// factory's error without the URL userinfo it may carry.
#[test]
fn an_unknown_provider_is_named_without_its_userinfo() {
    for label in [
        format!("custom-ish:http://u:{SECRET}@h/v1"),
        format!("http://alice:{SECRET}@127.0.0.1:9/v1"),
    ] {
        let error = super::create_provider(&label, None)
            .err()
            .unwrap_or_else(|| panic!("test: '{label}' is not a provider"))
            .to_string();
        assert!(error.contains("Unknown provider"), "{error}");
        assert!(!error.contains(SECRET), "the factory error echoed userinfo: {error}");
        assert!(
            error.contains("[redacted]@"),
            "the label is still recognisable: {error}"
        );
    }
}

/// R-C2BUk K-7: an upstream error body is read for a bounded time. An
/// upstream that answers 500, sends 10 bytes and then stalls held the reader
/// until 64 KiB or EOF, which never came; now the bytes read so far are
/// returned when `ERROR_BODY_READ_TIMEOUT` elapses (on a paused clock here).
#[tokio::test]
async fn an_error_body_that_stalls_is_read_for_a_bounded_time() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: bind loopback");
    let addr = listener.local_addr().expect("test: loopback address");
    let (release, hold) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let Ok((mut socket, _peer)) = listener.accept().await else {
            return;
        };
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await;
        let _ = socket
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 1000\r\n\r\n0123456789")
            .await;
        let _ = socket.flush().await;
        // Stall with the connection open until the test is done.
        let _ = hold.await;
    });
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("test: loopback responds");
    // The stub wrote its status line, headers and ten body bytes in one
    // write, so everything it will ever send is in the socket by the time the
    // headers are here; the clock is stopped now, and from here only the
    // read's own timeout can end the wait. No wall-clock wait is involved.
    tokio::time::pause();
    let read = tokio::time::timeout(std::time::Duration::from_mins(1), super::read_error_body(response)).await;
    tokio::time::resume();
    let _ = release.send(());
    server.abort();
    let body = read.expect("the error body read must end within its own bound");
    assert_eq!(body, "0123456789");
}
