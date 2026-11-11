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

const MODEL: &str = "prx-reset-fixture";
const REPLY: &str = "PRX fixture observation recorded";
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
    // Serve actual model discovery and inference over HTTP. Priced turns reach
    // production semantic autosave, which an early policy refusal cannot prove.
    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else { continue };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("HTTP timeout");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            let head_end = loop {
                let count = stream.read(&mut buffer).expect("HTTP request");
                if count == 0 {
                    break None;
                }
                bytes.extend_from_slice(buffer.get(..count).expect("request bytes"));
                if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break Some(offset + 4);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(bytes.get(..head_end).expect("HTTP head")).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            while bytes.len().saturating_sub(head_end) < length {
                let count = stream.read(&mut buffer).expect("HTTP body");
                assert!(count > 0, "incomplete HTTP body");
                bytes.extend_from_slice(buffer.get(..count).expect("body bytes"));
            }
            let (content_type, body) = if head.contains("/chat/completions") {
                let request: serde_json::Value =
                    serde_json::from_slice(bytes.get(head_end..).expect("body")).expect("provider JSON");
                let request_text = request.to_string();
                let content = if request_text.contains("Classify the execution requirement of current_request") {
                    r#"{"requirement":"informational"}"#.to_string()
                } else if request_text.contains("Review the proposed final reply against") {
                    r#"{"decision":"accept","reason":"Fixture observation recorded","constraints":[]}"#.to_string()
                } else {
                    let input = request
                        .get("messages")
                        .and_then(serde_json::Value::as_array)
                        .and_then(|messages| {
                            messages
                                .iter()
                                .rev()
                                .find(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("user"))
                        })
                        .and_then(|message| message.get("content"))
                        .map(serde_json::Value::to_string)
                        .unwrap_or_default();
                    let phase = if input.contains("after starting a new session") {
                        "after-new"
                    } else if input.contains("after clearing the conversation") {
                        "after-clear"
                    } else if input.contains("another observation in the same session") {
                        "second"
                    } else {
                        "initial"
                    };
                    format!("{REPLY}: {phase}.")
                };
                if request.get("stream").and_then(serde_json::Value::as_bool) == Some(true) {
                    let chunk =
                        serde_json::json!({"choices":[{"index":0,"delta":{"content":content},"finish_reason":null}]});
                    let finish = serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
                    (
                        "text/event-stream",
                        format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
                    )
                } else {
                    ("application/json", serde_json::json!({
                        "id":"prx-fixture-response", "object":"chat.completion", "model":MODEL,
                        "choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":3,"completion_tokens":3,"total_tokens":6}
                    }).to_string())
                }
            } else {
                (
                    "application/json",
                    serde_json::json!({
                        "object":"list", "data":[{"id":MODEL,"object":"model","created":1,"owned_by":"prx"}]
                    })
                    .to_string(),
                )
            };
            write!(stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ).expect("HTTP response");
        }
    });
}

fn start_repl(root: &std::path::Path, provider: &str, chat: bool) -> (Repl, std::path::PathBuf) {
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
    config.memory.auto_save = true;
    config.memory.semantic.auto_promote_user_messages = true;
    config.memory.semantic.min_chars = 1;
    config.cost.prices.clear();
    config.cost.prices.insert(
        format!("{provider}/{MODEL}"),
        toml::from_str("input = 1.0\noutput = 1.0\n").expect("explicit fixture price"),
    );
    config.providers.models.push(
        toml::from_str(&format!(
            "provider = {provider:?}\nmodel = {MODEL:?}\noperations = [\"chat\"]\nnative_tools = true\n"
        ))
        .expect("model declaration"),
    );
    std::fs::write(
        config_dir.join("config.toml"),
        toml::to_string_pretty(&config).expect("config serialization"),
    )
    .expect("write config");

    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command.arg("--config-dir").arg(&config_dir);
    if chat {
        command.args(["chat", "--plain"]);
    } else {
        command.arg("agent");
    }
    let mut child = command
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("RUST_LOG", "error")
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn interactive process");
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
    check_session_reset(false);
}

#[test]
fn chat_clear_and_new_preserve_all_memories_and_start_new_sessions() {
    check_session_reset(true);
}

fn check_session_reset(chat: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    let provider = format!("custom:http://{}/v1", listener.local_addr().expect("loopback address"));
    serve_loopback(listener);
    let root = tempfile::tempdir().expect("isolated root");
    let (mut repl, workspace) = start_repl(root.path(), &provider, chat);
    repl.send("The PRX fixture records its initial conversation observation.");
    repl.until(&format!("{REPLY}: initial."));
    let db = rusqlite::Connection::open(workspace.join("memory/brain.db")).expect("independent SQLite connection");
    db.busy_timeout(Duration::from_secs(5)).expect("SQLite timeout");
    let first = recorded_turn(&db, "The PRX fixture records its initial conversation observation.");
    repl.send("The PRX fixture records another observation in the same session.");
    repl.until(&format!("{REPLY}: second."));
    let second = recorded_turn(&db, "The PRX fixture records another observation in the same session.");
    assert_eq!(first.0, second.0, "turns before /clear must share a session");
    assert_ne!(first.1, second.1, "run IDs remain per-turn");

    let rows = [
        (
            "current",
            "conversation",
            first.0.as_str(),
            if chat { "chat" } else { "agent" },
            if chat { "terminal" } else { "cli" },
        ),
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
    for (command, text) in [
        (
            "/clear",
            "The PRX fixture records an observation after clearing the conversation.",
        ),
        (
            "/new",
            "The PRX fixture records an observation after starting a new session.",
        ),
    ] {
        repl.send(command);
        repl.until(if chat {
            "Started new chat session "
        } else {
            "New session started. Stored memories are preserved."
        });
        repl.send(text);
        repl.until(&format!(
            "{REPLY}: {}.",
            if command == "/clear" {
                "after-clear"
            } else {
                "after-new"
            }
        ));
        let next = recorded_turn(&db, text);
        assert_ne!(previous.0, next.0, "{command} must rotate the durable session");
        assert_ne!(previous.1, next.1, "{command} must preserve per-turn run IDs");
        let memory: (String, Option<String>) = db
            .query_row(
                "SELECT content, session_id FROM memories WHERE key = ?1",
                [format!("user_msg:{}", next.1)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("production autosave for the post-reset turn");
        assert_eq!(memory.0, text, "post-reset autosave must contain the current input");
        assert_eq!(
            memory.1.as_deref(),
            Some(next.0.as_str()),
            "post-reset semantic memory must use the new session identity"
        );
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
