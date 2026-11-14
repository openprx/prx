//! CF-R4: a running `prx daemon` applies a CLI change to a gateway-prepared
//! field through its file watcher (inside the runtime), instead of rolling it
//! back with "there is no reactor running".
//!
//! Real components: the built binary as the daemon and as the CLI, a temp
//! config dir, the real gateway on a loopback port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

#[path = "support/loopback_openai.rs"]
mod loopback_openai;
#[path = "support/plain_chat.rs"]
mod plain_chat;

use plain_chat::{Running, Sandbox};
use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

const TOKEN: &str = "prx-daemon-reload-token";
/// Longest the daemon may take to answer its first request.
const STARTUP_DEADLINE: Duration = Duration::from_secs(90);
/// Longest a written change may take to become active.
const APPLY_DEADLINE: Duration = Duration::from_secs(30);
/// Largest HTTP response read from the gateway.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const POLL: Duration = Duration::from_millis(200);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("test: free loopback port")
}

/// `GET path` with the operator bearer token; `None` when the daemon does not
/// answer yet.
fn get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut bytes = Vec::new();
    stream.take(MAX_RESPONSE_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let body = text.split_once("\r\n\r\n").map(|(_, body)| body.to_owned())?;
    Some((status, body))
}

/// The engine's `enabled` flag as the daemon reports it.
fn daemon_enabled(port: u16) -> Option<bool> {
    let (status, body) = get(port, "/api/decision-engine/status")?;
    if status != 200 {
        return None;
    }
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    let value: serde_json::Value = serde_json::from_str(&body[start..=end]).ok()?;
    value["enabled"].as_bool()
}

fn cli(sandbox: &Sandbox, args: &[&str]) -> plain_chat::Output {
    let output = Running::spawn(sandbox.command(args)).wait();
    assert!(output.status.success(), "prx {args:?}: {}", output.stderr);
    output
}

/// Set `enabled` through the CLI and require the daemon to apply it.
fn set_and_observe(sandbox: &Sandbox, port: u16, enabled: bool) {
    let value = if enabled { "true" } else { "false" };
    cli(
        sandbox,
        &["decision-engine", "config", "set", "enabled", value, "--yes"],
    );
    let deadline = Instant::now() + APPLY_DEADLINE;
    loop {
        let history = cli(sandbox, &["config", "history"]).stdout;
        let settled = !history.contains("ActivationDeferred") && !history.contains("Pending");
        assert!(!history.contains("RolledBack"), "the reload was rolled back: {history}");
        if settled && history.contains("Activated") && daemon_enabled(port) == Some(enabled) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "enabled={value} did not become active within {APPLY_DEADLINE:?}: {history}"
        );
        std::thread::sleep(POLL);
    }
}

#[test]
fn daemon_applies_decision_engine_toggle_through_hot_reload() {
    let sandbox = Sandbox::new();
    let port = free_port();
    // The gateway needs a configured provider credential to start; the
    // loopback provider is never called by this test.
    let provider = loopback_openai::LoopbackOpenAi::start(
        "prx-daemon-model",
        None,
        Box::new(|_, _| loopback_openai::Scripted::text("unused")),
    );
    loopback_openai::write_config(
        &sandbox.config_dir,
        &provider.provider(),
        "prx-daemon-model",
        |config| {
            config.secrets.encrypt = false;
            config.gateway.host = "127.0.0.1".to_owned();
            config.gateway.port = port;
            config.gateway.paired_tokens = vec![TOKEN.to_owned()];
        },
    );

    let port_text = port.to_string();
    let daemon = Running::spawn(sandbox.command(&["daemon", "--host", "127.0.0.1", "--port", &port_text]));
    let deadline = Instant::now() + STARTUP_DEADLINE;
    while daemon_enabled(port).is_none() {
        assert!(
            Instant::now() < deadline,
            "daemon did not answer within {STARTUP_DEADLINE:?}"
        );
        std::thread::sleep(POLL);
    }
    assert_eq!(daemon_enabled(port), Some(false), "the engine starts disabled");

    set_and_observe(&sandbox, port, true);
    set_and_observe(&sandbox, port, false);
    drop(daemon);
}
