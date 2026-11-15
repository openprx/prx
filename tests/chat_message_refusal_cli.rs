//! Real-process proofs for a refused `prx chat --message` turn.
//!
//! Spawns the built `prx` binary against an isolated config dir and a
//! loopback OpenAI-compatible listener. The listener speaks HTTP; the turn
//! must refuse before `/chat/completions`, print only the refusal on stdout
//! and exit 2.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::indexing_slicing,
    clippy::field_reassign_with_default
)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

const MODEL: &str = "prx-unpriced-fixture";
const GENERIC: &str = "I can't complete this request under the current policy.";

fn serve_loopback(listener: TcpListener, chat_hits: Arc<AtomicUsize>) {
    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else {
                continue;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0_u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(buf.get(..n).unwrap_or_default());
            if request.contains("/chat/completions") {
                chat_hits.fetch_add(1, Ordering::SeqCst);
            }
            let body = serde_json::json!({
                "object": "list",
                "data": [{
                    "id": MODEL,
                    "object": "model",
                    "created": 1,
                    "owned_by": "prx"
                }]
            })
            .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
}

/// An isolated `config/` (with `workspace/`) and `home/`; `priced` adds a
/// zero tariff for the model so the send is admitted.
fn sandbox(provider: &str, priced: bool) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("isolated root");
    let home = root.path().join("home");
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(config_dir.join("workspace")).expect("workspace");

    let mut config = openprx::config::Config::default();
    config.default_provider = Some(provider.to_owned());
    config.default_model = Some(MODEL.to_string());
    config.api_key = Some("prx-test-credential".to_string());
    config.auth.codex_auth_json_auto_import = false;
    config.memory.embedding_provider = "none".to_string();
    config.reliability.provider_retries = 0;
    config.reliability.fallback_providers.clear();
    config.cost.prices.clear();
    if priced {
        config.cost.prices.insert(
            format!("{provider}/{MODEL}"),
            openprx::config::schema::ModelPricing {
                input: 0.0,
                output: 0.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
    }
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = {provider:?}\nmodel = {MODEL:?}\noperations = [\"chat\"]\n"
        ))
        .expect("model declaration"),
    );
    std::fs::write(
        config_dir.join("config.toml"),
        toml::to_string_pretty(&config).expect("serialize config"),
    )
    .expect("write config");
    (root, config_dir, home)
}

/// `prx chat --message <message>`, bounded by a 90 s deadline.
fn run_message(config_dir: &std::path::Path, home: &std::path::Path, message: &str) -> std::process::Output {
    let child = Command::new(env!("CARGO_BIN_EXE_prx"))
        .args(["--config-dir"])
        .arg(config_dir)
        .args(["chat", "--message", message])
        .env("HOME", home)
        .env("RUST_LOG", "error")
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn prx chat --message");
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    rx.recv_timeout(Duration::from_secs(90))
        .unwrap_or_else(|_| {
            let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
            panic!("prx chat --message timed out after 90s");
        })
        .expect("wait prx chat --message")
}

#[test]
fn unpriced_chat_message_prints_reason_and_exits_two() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    let addr = listener.local_addr().expect("loopback address");
    let provider = format!("custom:http://{addr}/v1");
    let chat_hits = Arc::new(AtomicUsize::new(0));
    serve_loopback(listener, Arc::clone(&chat_hits));

    let (_root, config_dir, home) = sandbox(&provider, false);
    let workspace = config_dir.join("workspace");
    let output = run_message(&config_dir, &home, "What is 2 + 2?");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let price_key = format!("{provider}/{MODEL}");
    let reason = format!("Reason: model {MODEL} has no price; add [cost.prices.{price_key:?}]");
    assert_eq!(output.status.code(), Some(2), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.starts_with(GENERIC), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains(&reason), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.is_empty(), "stderr must be empty on a refusal: {stderr}");
    assert!(!stdout.contains("HIDDEN-CAP"), "stdout: {stdout}");
    assert_eq!(chat_hits.load(Ordering::SeqCst), 0, "provider chat was called");

    let database = rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("test: read actual refused-turn store");
    let markers = database
        .prepare("SELECT raw_payload_json FROM message_events WHERE event_type='turn.finalized'")
        .expect("test: terminal marker query")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("test: terminal markers")
        .collect::<Result<Vec<_>, _>>()
        .expect("test: durable terminal payloads");
    assert_eq!(markers.len(), 1, "one real terminal for the refused send");
    let payload: serde_json::Value =
        serde_json::from_str(markers.first().expect("test: one marker")).expect("test: terminal JSON");
    if let Some(outcome) = payload.get("provider_outcome").filter(|value| !value.is_null()) {
        assert!(
            outcome
                .get("attempts")
                .and_then(serde_json::Value::as_array)
                .expect("test: reported provider attempts")
                .is_empty(),
            "a pre-send refusal cannot fabricate a provider attempt",
        );
    }
    // P2-R3 (D§4.2): the repository terminal carries no execution trace;
    // the zero physical requests are evidenced by the provider spy above and
    // by the empty provider attempts.
    assert!(
        payload.get("execution_trace").is_none_or(serde_json::Value::is_null),
        "the terminal payload carries no execution trace: {payload}",
    );
}

#[test]
fn unreachable_provider_exits_one_with_error_on_stderr() {
    // A bound-then-closed port: nothing listens, so every send fails.
    let addr = TcpListener::bind("127.0.0.1:0")
        .expect("loopback bind")
        .local_addr()
        .expect("loopback address");
    let provider = format!("custom:http://{addr}/v1");
    let (_root, config_dir, home) = sandbox(&provider, true);
    let output = run_message(&config_dir, &home, "What is 2 + 2?");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.is_empty(), "stdout must be empty on a failure: {stdout}");
    assert!(stderr.contains("Error: "), "stderr: {stderr}");
}
