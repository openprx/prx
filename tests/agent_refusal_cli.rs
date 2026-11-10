//! Real-process proofs for a refused single-shot `prx agent` turn.
//!
//! Spawns the built `prx` binary against an isolated config dir and a
//! loopback OpenAI-compatible listener. The listener speaks HTTP; the agent
//! must refuse before `/chat/completions`.

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

#[test]
fn unpriced_single_shot_prints_reason_and_exits_two() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    let addr = listener.local_addr().expect("loopback address");
    let provider = format!("custom:http://{addr}/v1");
    let chat_hits = Arc::new(AtomicUsize::new(0));
    serve_loopback(listener, Arc::clone(&chat_hits));

    let root = tempfile::tempdir().expect("isolated root");
    let home = root.path().join("home");
    let config_dir = root.path().join("config");
    let workspace = config_dir.join("workspace");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&workspace).expect("workspace");

    let mut config = openprx::config::Config::default();
    config.default_provider = Some(provider.clone());
    config.default_model = Some(MODEL.to_string());
    config.api_key = Some("prx-test-credential".to_string());
    config.auth.codex_auth_json_auto_import = false;
    config.memory.embedding_provider = "none".to_string();
    config.reliability.provider_retries = 0;
    config.reliability.fallback_providers.clear();
    config.cost.prices.clear();
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

    let child = Command::new(env!("CARGO_BIN_EXE_prx"))
        .args(["--config-dir"])
        .arg(&config_dir)
        .args(["agent", "What is 2 + 2?"])
        .env("HOME", &home)
        .env("RUST_LOG", "error")
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn prx agent");
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = rx
        .recv_timeout(Duration::from_secs(90))
        .unwrap_or_else(|_| {
            let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
            panic!("prx agent timed out after 90s");
        })
        .expect("wait prx agent");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let price_key = format!("{provider}/{MODEL}");
    let reason = format!("Reason: model {MODEL} has no price; add [cost.prices.{price_key:?}]");
    assert_eq!(output.status.code(), Some(2), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains(GENERIC), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains(&reason), "stdout: {stdout}\nstderr: {stderr}");
    assert!(!stderr.contains("agent turn refused"), "stderr: {stderr}");
    assert!(!stdout.contains("HIDDEN-CAP"), "stdout: {stdout}");
    assert_eq!(chat_hits.load(Ordering::SeqCst), 0, "provider chat was called");
}
