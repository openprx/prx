//! CF-R11: `prx decision-engine status` reports the running daemon's counters
//! (`source: "daemon"`), falls back to the process-local snapshot when no
//! daemon answers, refuses a wrong token; `prx decision-engine analyses` lists
//! the analyses an engine-on webhook turn stored.
//!
//! Real components: the built binary as the daemon and as the CLI, a temp
//! config dir with real SQLite, the real gateway on a loopback port, a
//! loopback OpenAI-compatible provider speaking real HTTP/SSE.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;

use plain_chat::{Running, Sandbox};
use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

const TOKEN: &str = "prx-decision-engine-status-token";
const MODEL: &str = "prx-engine-model";
/// Longest the daemon may take to answer its first request.
const STARTUP_DEADLINE: Duration = Duration::from_secs(90);
/// Longest the stored analysis may take to show up.
const ANALYSIS_DEADLINE: Duration = Duration::from_secs(30);
/// Read deadline of one gateway exchange (the webhook runs a whole turn).
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest HTTP response read from the gateway.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const POLL: Duration = Duration::from_millis(200);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("test: free loopback port")
}

/// One HTTP/1.1 exchange with the operator bearer token; `None` when the
/// daemon does not answer yet.
fn exchange(port: u16, method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(EXCHANGE_TIMEOUT)).ok()?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .ok()?;
    let mut bytes = Vec::new();
    stream.take(MAX_RESPONSE_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let body = text.split_once("\r\n\r\n").map(|(_, body)| body.to_owned())?;
    Some((status, body))
}

fn cli(sandbox: &Sandbox, args: &[&str]) -> plain_chat::Output {
    Running::spawn(sandbox.command(args)).wait()
}

fn json_of(output: &plain_chat::Output) -> serde_json::Value {
    serde_json::from_str(&output.stdout).unwrap_or_else(|error| panic!("{error}: {}", output.stdout))
}

#[test]
fn status_reads_the_daemon_and_analyses_list_the_stored_analysis() {
    let sandbox = Sandbox::new();
    let port = free_port();
    let provider = loopback_openai::LoopbackOpenAi::start(
        MODEL,
        Some((12, 3)),
        Box::new(|_, _| loopback_openai::Scripted::text("engine turn done")),
    );
    loopback_openai::write_config(&sandbox.config_dir, &provider.provider(), MODEL, |config| {
        config.secrets.encrypt = false;
        config.gateway.host = "127.0.0.1".to_owned();
        config.gateway.port = port;
        config.gateway.paired_tokens = vec![TOKEN.to_owned()];
        config.chat.daemon.token = TOKEN.to_owned();
        config.decision_engine.enabled = true;
    });

    let port_text = port.to_string();
    let daemon = Running::spawn(sandbox.command(&["daemon", "--host", "127.0.0.1", "--port", &port_text]));
    let deadline = Instant::now() + STARTUP_DEADLINE;
    while exchange(port, "GET", "/api/decision-engine/status", "").map(|(status, _)| status) != Some(200) {
        assert!(
            Instant::now() < deadline,
            "daemon did not answer within {STARTUP_DEADLINE:?}"
        );
        std::thread::sleep(POLL);
    }

    // One engine-on turn through the webhook.
    let (status, body) =
        exchange(port, "POST", "/webhook", r#"{"message":"Say hello"}"#).expect("test: webhook answered");
    assert_eq!(status, 200, "webhook: {body}");
    assert!(body.contains("engine turn done"), "webhook: {body}");

    let deadline = Instant::now() + ANALYSIS_DEADLINE;
    let analyses = loop {
        let output = cli(&sandbox, &["decision-engine", "analyses", "--json"]);
        assert!(output.status.success(), "analyses: {}", output.stderr);
        let rows = json_of(&output);
        if rows.as_array().is_some_and(|rows| !rows.is_empty()) {
            break rows;
        }
        assert!(Instant::now() < deadline, "no analysis stored: {}", output.stdout);
        std::thread::sleep(POLL);
    };
    assert_eq!(analyses.as_array().map(Vec::len), Some(1), "{analyses}");
    assert!(
        analyses[0]["decision_id"].as_str().is_some_and(|id| !id.is_empty()),
        "{analyses}"
    );

    let output = cli(&sandbox, &["decision-engine", "status", "--json"]);
    assert!(output.status.success(), "status: {}", output.stderr);
    let status = json_of(&output);
    assert_eq!(status["source"], "daemon", "{status}");
    assert_eq!(status["enabled"], true, "{status}");
    assert!(status["analyses"].as_u64().is_some_and(|count| count >= 1), "{status}");
    assert!(output.stderr.is_empty(), "status stderr: {}", output.stderr);

    // A config that names this daemon with a wrong token is refused.
    let stranger = Sandbox::new();
    loopback_openai::write_config(&stranger.config_dir, &provider.provider(), MODEL, |config| {
        config.chat.daemon.url = format!("http://127.0.0.1:{port}");
        config.chat.daemon.token = "prx-wrong-token".to_owned();
    });
    let output = cli(&stranger, &["decision-engine", "status", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "wrong token: {} {}",
        output.stdout,
        output.stderr
    );
    assert!(
        output.stderr.contains("401") || output.stderr.contains("403"),
        "{}",
        output.stderr
    );

    // With the daemon stopped, the process-local snapshot is shown.
    drop(daemon);
    let output = cli(&sandbox, &["decision-engine", "status", "--json"]);
    assert!(output.status.success(), "status without daemon: {}", output.stderr);
    let status = json_of(&output);
    assert_eq!(status["source"], "process-local", "{status}");
    assert_eq!(status["analyses"], 0, "{status}");
    assert!(
        output.stderr.contains("daemon unreachable at http://127.0.0.1:")
            && output.stderr.contains("showing process-local counters"),
        "{}",
        output.stderr
    );
}
