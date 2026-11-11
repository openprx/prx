//! Exercise /clear and /new through a real REPL process and its SQLite store.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::field_reassign_with_default
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const MODEL: &str = "prx-unpriced-fixture";
const REPLY: &str = "I can't complete this request under the current policy.";
const WAIT: Duration = Duration::from_secs(90);

struct Repl {
    child: Child,
    input: ChildStdin,
    output: Receiver<String>,
}

impl Repl {
    fn send(&mut self, input: &str) {
        writeln!(self.input, "{input}").expect("write REPL input");
        self.input.flush().expect("flush REPL input");
    }

    fn until(&self, marker: &str) {
        let deadline = Instant::now() + WAIT;
        let mut transcript = String::new();
        loop {
            let line = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("REPL did not emit {marker:?}: {error}; output: {transcript}"));
            transcript.push_str(&line);
            if line.contains(marker) {
                break;
            }
        }
    }
}

impl Drop for Repl {
    fn drop(&mut self) {
        // Also terminate the process if an assertion or bounded receive fails.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn recorded_turn(db: &rusqlite::Connection, text: &str) -> (String, String) {
    db.query_row(
        "SELECT session_key, run_id FROM message_events WHERE role = 'user' AND content = ?1",
        [text],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .expect("durable REPL source event")
}

fn serve_loopback(listener: TcpListener) {
    // The unpriced fixture deterministically closes each turn before inference.
    // Model discovery still crosses a real HTTP socket, with real JSON bytes.
    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else { continue };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("HTTP timeout");
            let mut buffer = [0_u8; 4096];
            let count = stream.read(&mut buffer).expect("HTTP request");
            let request = String::from_utf8_lossy(buffer.get(..count).expect("request bytes"));
            assert!(
                !request.contains("/chat/completions"),
                "unpriced model reached inference"
            );
            let body = serde_json::json!({
                "object": "list", "data": [{"id": MODEL, "object": "model", "created": 1, "owned_by": "prx"}]
            })
            .to_string();
            write!(stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ).expect("HTTP response");
        }
    });
}

fn start_repl(root: &std::path::Path, provider: &str) -> (Repl, std::path::PathBuf) {
    let home = root.join("home");
    let config_dir = root.join("config");
    let workspace = config_dir.join("workspace");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut config = openprx::config::Config::default();
    config.default_provider = Some(provider.to_string());
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
        toml::to_string_pretty(&config).expect("config serialization"),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_prx"))
        .arg("--config-dir")
        .arg(&config_dir)
        .arg("agent")
        .env("HOME", &home)
        .env("RUST_LOG", "error")
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn REPL");
    let input = child.stdin.take().expect("REPL stdin");
    let stdout = child.stdout.take().expect("REPL stdout");
    let stderr = child.stderr.take().expect("REPL stderr");
    let (tx, output) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.expect("REPL stdout line")).is_err() {
                break;
            }
        }
    });
    thread::spawn(move || {
        // Drain stderr so a diagnostic cannot block the process.
        std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink()).expect("drain stderr");
    });
    (Repl { child, input, output }, workspace)
}

#[test]
fn clear_and_new_preserve_all_memories_and_rotate_repl_session() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    let provider = format!("custom:http://{}/v1", listener.local_addr().expect("loopback address"));
    serve_loopback(listener);
    let root = tempfile::tempdir().expect("isolated root");
    let (mut repl, workspace) = start_repl(root.path(), &provider);
    repl.send("prx turn one");
    repl.until(REPLY);
    let db = rusqlite::Connection::open(workspace.join("memory/brain.db")).expect("independent SQLite connection");
    db.busy_timeout(Duration::from_secs(5)).expect("SQLite timeout");
    let first = recorded_turn(&db, "prx turn one");
    repl.send("prx turn two");
    repl.until(REPLY);
    let second = recorded_turn(&db, "prx turn two");
    assert_eq!(first.0, second.0, "turns before /clear must share a session");
    assert_ne!(first.1, second.1, "run IDs remain per-turn");

    let rows = [
        ("repl", "conversation", first.0.as_str(), "agent", "cli"),
        ("other", "daily", "agent:other-session", "agent", "cli"),
        (
            "channel",
            "conversation",
            "channel:other-session",
            "channel",
            "telegram",
        ),
        ("console", "daily", "console:other-session", "console", "console"),
        ("core", "core", first.0.as_str(), "agent", "cli"),
    ];
    for (key, category, session, source, channel) in rows {
        db.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at, session_id, workspace_id, source, channel)
             VALUES (?1, ?1, ?1, ?2, '2026-11-11T00:00:00Z', '2026-11-11T00:00:00Z', ?3, ?4, ?5, ?6)",
            rusqlite::params![key, category, session, workspace.to_string_lossy(), source, channel],
        ).expect("seed persisted memory");
    }
    let mut previous = second;
    for (command, text) in [("/clear", "prx after clear"), ("/new", "prx after new")] {
        repl.send(command);
        repl.until("New session started. Stored memories are preserved.");
        repl.send(text);
        repl.until(REPLY);
        let next = recorded_turn(&db, text);
        assert_ne!(previous.0, next.0, "{command} must rotate the durable session");
        assert_ne!(previous.1, next.1, "{command} must preserve per-turn run IDs");
        for (key, category, session, source, channel) in rows {
            let persisted: (String, String, String, String, String) = db
                .query_row(
                    "SELECT content, category, session_id, source, channel FROM memories WHERE key = ?1",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .unwrap_or_else(|error| panic!("{command} removed {key}: {error}"));
            assert_eq!(
                persisted,
                (
                    key.into(),
                    category.into(),
                    session.into(),
                    source.into(),
                    channel.into()
                )
            );
        }
        previous = next;
    }
    repl.send("/quit");
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = repl.child.try_wait().expect("REPL status") {
            assert!(status.success(), "REPL exit: {status}");
            break;
        }
        assert!(Instant::now() < deadline, "REPL did not exit");
        // Exit polling only; turn assertions synchronize on emitted replies.
        thread::sleep(Duration::from_millis(10));
    }
}
