//! PTY-driven end-to-end tests for `prx chat`.
//!
//! These tests spawn the real `prx` binary inside a pseudo-terminal and assert
//! on the observable stdout/stderr stream. They cover regressions that pure
//! unit tests cannot catch (banner ordering, slash-command dispatch under TUI,
//! double-Ctrl-C semantics, ANSI escape elision under `--plain`).
//!
//! ## How it works
//!
//! - `test-mock` feature is required at build time (CI must pass
//!   `--features test-mock`). It adds the `mock` provider to the factory and
//!   to `list_providers()` so the availability check accepts it without an
//!   API key.
//! - Each test allocates a fresh `tempfile::TempDir` and points
//!   `OPENPRX_CONFIG_DIR` / `OPENPRX_WORKSPACE` / `HOME` / `XDG_DATA_HOME`
//!   into it, so the suite is fully hermetic — no `~/.openprx/` writes, no
//!   reedline history pollution.
//! - `PRX_TUI=0` forces the legacy reedline + `BufRead` fallback so the banner
//!   reliably lands on stdout (the ratatui path renders into the alt-screen
//!   and cannot be scraped by line-oriented matchers).
//! - reedline (and crossterm under the hood) probes cursor position via the
//!   DSR escape sequence `ESC [ 6 n` and blocks waiting for a `ESC [ <r> ;
//!   <c> R` reply that only a real terminal emulator generates. The
//!   `read_until_with_dsr` helper below scans the PTY stream and writes a
//!   synthetic `\x1b[1;1R` reply every time it sees a query, so the chat
//!   loop unblocks under expectrl.
//! - `serial_test::serial` keeps PTY tests from racing for controlling-tty
//!   resources without forcing `--test-threads=1` globally.
//!
//! ## Failure handling
//!
//! When the host has no usable PTY (rare on Linux but possible in some
//! containers), `expectrl::spawn` returns an `Err`. We treat that as a real
//! test failure (`panic!`) rather than silently passing — a silent skip
//! would mask "the test never actually ran" as "the test passed". Mark
//! these tests `#[ignore]` instead if you want to opt out per-environment.

#![cfg(feature = "test-mock")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::missing_const_for_fn,
    clippy::match_same_arms,
    // The fake daemon below shares its recorded state between the test thread
    // and the thread serving HTTP. `parking_lot` is a library dependency and is
    // not reachable from an integration test, and the project's ban on
    // `std::sync::Mutex` exempts test code for exactly this reason: there is no
    // production path here to poison.
    clippy::disallowed_types,
    clippy::disallowed_methods
)]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use expectrl::session::Session;
use serial_test::serial;
use tempfile::TempDir;

#[path = "chat_pty_e2e/decision_resume.rs"]
mod decision_resume;

// D-071: every wait below ends on an event (the expected output, the process
// exiting, the peer recording a call). These limits only turn a hang into a
// failure: they are far above anything a loaded machine needs, and no test
// asserts that something happens *quickly*. A wait for output also fails at
// once when the process has exited.
const STARTUP_TIMEOUT: Duration = Duration::from_mins(3);
const TURN_TIMEOUT: Duration = Duration::from_mins(2);
const EXIT_TIMEOUT: Duration = Duration::from_mins(1);

/// Synthetic reply to ESC[6n (Device Status Report → Report Cursor Position).
/// Real terminals respond with `ESC [ row ; col R`; we always answer 1;1.
const DSR_REPLY: &[u8] = b"\x1b[1;1R";
/// Query reedline / crossterm emits when probing the cursor.
const DSR_QUERY: &[u8] = b"\x1b[6n";

/// Holds the lifetime of the tempdir alongside the spawned session so any
/// late writes by the chat process land inside the sandbox (and clean up
/// when the test ends).
struct HarnessGuard {
    tempdir: Option<TempDir>,
    config_dir: PathBuf,
    workspace_dir: PathBuf,
    home_dir: PathBuf,
    xdg_data: PathBuf,
    xdg_cache: PathBuf,
    xdg_state: PathBuf,
    xdg_config: PathBuf,
}

/// Locate the test-built `prx` binary. Cargo provides this via
/// `CARGO_BIN_EXE_<bin-name>` during `cargo test`.
fn prx_binary() -> &'static str {
    env!("CARGO_BIN_EXE_prx")
}

/// A chat `-p` override owns a non-root connection. A process child inherits
/// that source and completes instead of claiming the configured root URL/key.
#[test]
#[serial(prx_chat_pty)]
fn chat_provider_override_process_child_keeps_non_root_connection() {
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let guard = new_harness_guard().expect("isolated chat paths");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    listener.set_nonblocking(true).expect("nonblocking loopback");
    let address = listener.local_addr().expect("loopback address");
    let root_provider = format!("custom:http://{address}/root/v1");
    let override_provider = format!("custom:http://{address}/override/v1");
    let mut config = openprx::config::Config {
        config_path: guard.config_dir.join("config.toml"),
        workspace_dir: guard.workspace_dir.clone(),
        default_provider: Some(root_provider),
        default_model: Some("probe-model".to_string()),
        api_key: Some("PRX-CHAT-ROOT-KEY".to_string()),
        ..openprx::config::Config::default()
    };
    config.sessions_spawn.process_memory_strategy = "isolated_private".to_string();
    config.tool_tiering.always_include.push("sessions_spawn".to_string());
    std::fs::write(
        &config.config_path,
        toml::to_string_pretty(&config).expect("serialize chat config"),
    )
    .expect("write chat config");
    prepare_owned_chat_fixture(&guard, &override_provider, "probe-model");

    let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let server_seen = Arc::clone(&seen);
    let server_stop = Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        while !server_stop.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("loopback accept: {error}"),
            };
            stream.set_nonblocking(false).expect("blocking loopback connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("read timeout");
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            let head_end = loop {
                let read = stream.read(&mut chunk).expect("request read");
                if read == 0 {
                    break None;
                }
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break Some(offset + 4);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&bytes[..head_end]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            while bytes.len().saturating_sub(head_end) < length {
                let read = stream.read(&mut chunk).expect("body read");
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body = String::from_utf8_lossy(&bytes[head_end..]).to_string();
            server_seen
                .lock()
                .expect("request sink")
                .push((head.clone(), body.clone()));
            let parent_count = server_seen
                .lock()
                .expect("request sink")
                .iter()
                .filter(|(_, body)| body.contains("PRX_CHAT_PARENT"))
                .count();
            let message = if body.contains("PRX_CHAT_PARENT") && parent_count == 1 {
                serde_json::json!({"role":"assistant","content":null,"tool_calls":[{
                    "id":"call-prx-chat-spawn","type":"function","function":{
                        "name":"sessions_spawn","arguments":serde_json::json!({
                            "action":"spawn_batch","mode":"process","tasks":["PRX_CHAT_CHILD"]
                        }).to_string()
                    }
                }]})
            } else if body.contains("PRX_CHAT_PARENT") && parent_count == 2 {
                let start = body.find("batch-").expect("batch id in parent request");
                let batch_id = &body[start..start + 42];
                serde_json::json!({"role":"assistant","content":null,"tool_calls":[{
                    "id":"call-prx-chat-join","type":"function","function":{
                        "name":"sessions_spawn","arguments":serde_json::json!({"action":"join","batch_id":batch_id}).to_string()
                    }
                }]})
            } else {
                serde_json::json!({"role":"assistant","content":"PRX-CHAT-OVERRIDE-OK"})
            };
            let payload = serde_json::json!({
                "id":"chatcmpl-prx-override","object":"chat.completion","created":1,
                "model":"probe-model","choices":[{"index":0,"message":message,"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
            })
            .to_string();
            let (content_type, payload) = if body.contains("\"stream\":true") {
                let (delta, finish_reason) = message.get("tool_calls").map_or_else(
                    || (serde_json::json!({"content":message["content"]}), "stop"),
                    |calls| {
                        let mut calls = calls.as_array().expect("tool calls array").clone();
                        for (index, call) in calls.iter_mut().enumerate() {
                            call["index"] = serde_json::json!(index);
                        }
                        (serde_json::json!({"tool_calls":calls}), "tool_calls")
                    },
                );
                let chunk = serde_json::json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                let finish = serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":finish_reason}]});
                (
                    "text/event-stream",
                    format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
                )
            } else {
                ("application/json", payload)
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            stream.write_all(response.as_bytes()).expect("loopback reply");
        }
    });

    let mut command = Command::new(prx_binary());
    command
        .arg("--config-dir")
        .arg(&guard.config_dir)
        .arg("chat")
        .arg("--plain")
        .arg("-p")
        .arg(&override_provider)
        .arg("--model")
        .arg("probe-model")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &guard.home_dir)
        .env("XDG_DATA_HOME", &guard.xdg_data)
        .env("XDG_CACHE_HOME", &guard.xdg_cache)
        .env("XDG_STATE_HOME", &guard.xdg_state)
        .env("XDG_CONFIG_HOME", &guard.xdg_config)
        .env("PRX_TUI", "0")
        .env("NO_COLOR", "1")
        .env("TERM", "xterm-256color")
        .env("RUST_LOG", "off")
        .env("SESSION_WORKER_SECRET", "PRX-CHAT-ISOLATED-SECRET")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("chat binary");
    child
        .stdin
        .take()
        .expect("chat stdin")
        .write_all(b"PRX_CHAT_PARENT\n/exit\n")
        .expect("chat script");
    let output = child.wait_with_output().expect("chat output");
    stop.store(true, Ordering::Relaxed);
    server.join().expect("loopback server");
    assert!(
        output.status.success(),
        "chat stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = seen.lock().expect("request sink");
    let observed_results = requests
        .iter()
        .flat_map(|(_, body)| serde_json::from_str::<serde_json::Value>(body))
        .flat_map(|request| request["messages"].as_array().cloned().unwrap_or_default())
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let joined = requests.iter().any(|(_, body)| {
        let Ok(request) = serde_json::from_str::<serde_json::Value>(body) else {
            return false;
        };
        request["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| message["content"].as_str())
            .filter_map(|content| serde_json::from_str::<serde_json::Value>(content).ok())
            .any(|result| {
                result["total"] == 1
                    && result["completed"].as_array().is_some_and(|items| items.len() == 1)
                    && result["failed"] == serde_json::json!([])
                    && result["killed"] == serde_json::json!([])
                    && result["no_result"] == serde_json::json!([])
            })
    });
    assert!(
        joined,
        "parent did not observe one completed child; parent requests={}, total requests={}, tool results={observed_results:?}, stdout={}",
        requests
            .iter()
            .filter(|(_, body)| body.contains("PRX_CHAT_PARENT"))
            .count(),
        requests.len(),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        requests.iter().any(|(_, body)| body.contains("PRX_CHAT_CHILD")),
        "process child never called the override"
    );
    assert!(
        requests
            .iter()
            .all(|(head, _)| head.contains("/override/v1") && !head.contains("PRX-CHAT-ROOT-KEY"))
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("PRX-CHAT-OVERRIDE-OK"));
}

/// Build a fully-isolated `Command` for `prx chat`, redirecting every path the
/// chat session might write to into a fresh tempdir.
///
/// Caller is responsible for keeping the returned `HarnessGuard` alive for
/// the duration of the session (the `TempDir` is dropped together with it).
fn new_harness_guard() -> std::io::Result<HarnessGuard> {
    let tempdir = TempDir::new()?;
    let base = tempdir.path();
    let config_dir = base.join("config");
    let workspace_dir = base.join("workspace");
    let home_dir = base.join("home");
    let xdg_data = base.join("xdg_data");
    let xdg_cache = base.join("xdg_cache");
    let xdg_state = base.join("xdg_state");
    let xdg_config = base.join("xdg_config");
    for dir in [
        &config_dir,
        &workspace_dir,
        &home_dir,
        &xdg_data,
        &xdg_cache,
        &xdg_state,
        &xdg_config,
    ] {
        std::fs::create_dir_all(dir)?;
    }

    Ok(HarnessGuard {
        tempdir: Some(tempdir),
        config_dir,
        workspace_dir,
        home_dir,
        xdg_data,
        xdg_cache,
        xdg_state,
        xdg_config,
    })
}

/// Every owned PTY config supplies zero tariffs and an exact native-tools
/// declaration for its fixture models. Existing configuration values
/// (including daemon and security settings) survive.
fn prepare_owned_chat_fixture(guard: &HarnessGuard, provider: &str, model: &str) {
    let path = guard.config_dir.join("config.toml");
    let mut document = if path.exists() {
        toml::from_str::<toml::Value>(&std::fs::read_to_string(&path).expect("read owned chat config"))
            .expect("parse owned chat config")
    } else {
        // The normal missing-file loader serializes this same default. Its
        // computed workspace/config paths are serde-skipped and stay unchanged.
        toml::Value::try_from(openprx::config::Config::default()).expect("serialize chat defaults")
    };
    let default_cost = toml::Value::try_from(openprx::config::CostConfig::default()).expect("serialize cost defaults");
    let default_prices = default_cost.get("prices").expect("default cost prices").clone();
    let cost = document
        .as_table_mut()
        .expect("chat config table")
        .entry("cost")
        .or_insert(default_cost)
        .as_table_mut()
        .expect("cost table");
    let prices = cost
        .entry("prices")
        .or_insert(default_prices)
        .as_table_mut()
        .expect("prices table");
    let price = toml::Value::try_from(openprx::config::schema::ModelPricing {
        input: 0.0,
        output: 0.0,
        cache_write: 0.0,
        cache_read: 0.0,
    })
    .expect("serialize zero fixture price");
    let mut changed = false;
    for key in ["mock/mock".to_string(), format!("{provider}/{model}")] {
        if let Some(existing) = prices.get(&key) {
            assert_eq!(existing, &price, "owned fixture tariff must remain zero");
        } else {
            prices.insert(key, price.clone());
            changed = true;
        }
    }
    let declaration = toml::Value::try_from(openprx::config::schema::ProviderModelConfig {
        provider: provider.to_string(),
        model: Some(model.to_string()),
        operations: Some(vec![openprx::config::schema::ModelOperationSetting::Chat]),
        native_tools: Some(true),
        vision_input: None,
        streaming: None,
        structured_output: None,
        tool_schema_dialect: None,
        tool_call_dialect: None,
        context_window: None,
    })
    .expect("serialize fixture model declaration");
    let providers = document
        .as_table_mut()
        .expect("chat config table")
        .entry("providers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .expect("providers table");
    let models = providers
        .entry("models")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .expect("provider model array");
    if !models.iter().any(|entry| entry == &declaration) {
        models.push(declaration);
        changed = true;
    }
    // Repeated commands on one harness must not rewrite a live daemon's config.
    if !changed {
        return;
    }
    let encoded = toml::to_string_pretty(&document).expect("serialize priced chat config");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .expect("open owned chat config")
        .write_all(encoded.as_bytes())
        .expect("write fixture tariff");
}

#[test]
fn owned_chat_fixture_tariff_preserves_preexisting_config_keys() {
    let guard = new_harness_guard().expect("create owned chat config harness");
    let path = guard.config_dir.join("config.toml");
    let original_text = r#"
default_provider = "mock"
default_model = "mock"

[autonomy]
level = "readonly"
workspace_only = true

[chat.daemon]
url = "http://127.0.0.1:1"
token = ""

[fixture_metadata]
label = "prx_fixture"
values = [1, 2, 3]
"#;
    std::fs::write(&path, original_text).expect("write preexisting chat config");
    let original = original_text.parse::<toml::Table>().expect("parse original document");
    prepare_owned_chat_fixture(&guard, "mock", "mock");
    let encoded = std::fs::read_to_string(&path).expect("read priced document");
    let round_trip = encoded.parse::<toml::Table>().expect("parse priced document");
    for (key, value) in &original {
        assert_eq!(round_trip.get(key), Some(value), "preexisting config key changed");
    }
    let price = round_trip
        .get("cost")
        .and_then(|cost| cost.get("prices"))
        .and_then(|prices| prices.get("mock/mock"))
        .and_then(toml::Value::as_table)
        .expect("qualified mock fixture price");
    for field in ["input", "output", "cache_write", "cache_read"] {
        assert_eq!(price.get(field).and_then(toml::Value::as_float), Some(0.0));
    }
    let modified = std::fs::metadata(&path)
        .expect("priced config metadata")
        .modified()
        .expect("priced config mtime");
    prepare_owned_chat_fixture(&guard, "mock", "mock");
    assert_eq!(std::fs::read_to_string(&path).expect("reread priced config"), encoded);
    assert_eq!(
        std::fs::metadata(&path)
            .expect("unchanged config metadata")
            .modified()
            .expect("unchanged config mtime"),
        modified
    );
}

fn build_chat_command_in(guard: &HarnessGuard, extra_args: &[&str], extra_env: &[(&str, &str)]) -> Command {
    prepare_owned_chat_fixture(guard, "mock", "mock");
    build_unprepared_chat_command_in(guard, extra_args, extra_env)
}

fn build_unprepared_chat_command_in(guard: &HarnessGuard, extra_args: &[&str], extra_env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(prx_binary());
    cmd.arg("chat");
    cmd.arg("-p").arg("mock");
    cmd.arg("--model").arg("mock");
    for a in extra_args {
        cmd.arg(a);
    }
    // Wipe inherited env so the test doesn't accidentally pick up real
    // OpenPRX state from the developer's shell. Then layer the hermetic
    // env on top.
    cmd.env_clear();
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    cmd.env("OPENPRX_CONFIG_DIR", &guard.config_dir);
    cmd.env("OPENPRX_WORKSPACE", &guard.workspace_dir);
    cmd.env("HOME", &guard.home_dir);
    cmd.env("XDG_DATA_HOME", &guard.xdg_data);
    cmd.env("XDG_CACHE_HOME", &guard.xdg_cache);
    cmd.env("XDG_STATE_HOME", &guard.xdg_state);
    cmd.env("XDG_CONFIG_HOME", &guard.xdg_config);
    // Force reedline + BufRead fallback so banner lands on stdout.
    cmd.env("PRX_TUI", "0");
    cmd.env("NO_COLOR", "1");
    cmd.env("TERM", "xterm-256color");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");
    // Silence tracing so test output isn't polluted.
    cmd.env("RUST_LOG", "off");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    cmd
}

fn build_chat_command(extra_args: &[&str], extra_env: &[(&str, &str)]) -> std::io::Result<(Command, HarnessGuard)> {
    let guard = new_harness_guard()?;
    let cmd = build_chat_command_in(&guard, extra_args, extra_env);
    Ok((cmd, guard))
}

/// Spawn a `prx chat` PTY session, set a fixed 120x40 window, and configure
/// per-call expect timeouts. Panics when PTY is unavailable in the current
/// environment — a silent skip would mask "the regression test never
/// actually exercised the binary".
fn spawn_chat(extra_args: &[&str], extra_env: &[(&str, &str)]) -> (SessionGuard, HarnessGuard) {
    let (cmd, guard) = build_chat_command(extra_args, extra_env).expect("build chat command");
    let session = spawn_chat_command(cmd);
    (SessionGuard::new(session), guard)
}

#[allow(dead_code)]
fn spawn_chat_in(guard: &HarnessGuard, extra_args: &[&str], extra_env: &[(&str, &str)]) -> SessionGuard {
    let cmd = build_chat_command_in(guard, extra_args, extra_env);
    SessionGuard::new(spawn_chat_command(cmd))
}

fn spawn_chat_command(cmd: Command) -> Session {
    let mut session = match Session::spawn(cmd) {
        Ok(s) => s,
        Err(e) => {
            // Distinguish "no PTY device available" (ENXIO/ENOTTY, expected in some
            // containers) from real failures (binary not found, permission denied, etc.).
            let hint = if e.to_string().contains("No such device")
                || e.to_string().contains("not a tty")
                || e.to_string().contains("ENXIO")
                || e.to_string().contains("ENOTTY")
            {
                "This looks like a missing PTY device (ENXIO/ENOTTY). \
                 Mark the test `#[ignore = \"needs TTY\"]` if this host cannot \
                 allocate a pseudo-terminal."
            } else {
                "This does NOT look like a missing PTY device — check that the \
                 `prx` binary exists, is executable, and has the correct permissions."
            };
            panic!(
                "Failed to spawn PTY session: {e:?}\n\
                 Hint: {hint}"
            )
        }
    };
    // Fixed PTY geometry avoids width-dependent wrapping noise.
    if let Err(e) = session.get_process_mut().set_window_size(120, 40) {
        let _ = writeln!(
            std::io::stderr(),
            "warning: set_window_size failed: {e}; continuing with default size"
        );
    }
    session.set_expect_timeout(Some(TURN_TIMEOUT));
    session
}

/// Own the actual PTY child through status polling and bounded cleanup.
/// Unknown cleanup retains the session and fixture paths and fails the test.
struct TrackedSession {
    inner: Session,
    reaped: Option<expectrl::WaitStatus>,
}

impl TrackedSession {
    fn confirmed_exit(&mut self) -> std::io::Result<bool> {
        if self.reaped.is_some() {
            return Ok(true);
        }
        match self.inner.get_process().status() {
            Ok(status @ (expectrl::WaitStatus::Exited(_, _) | expectrl::WaitStatus::Signaled(_, _, _))) => {
                self.reaped = Some(status);
                Ok(true)
            }
            Ok(_) => Ok(false),
            // In particular, first ECHILD/ESRCH is unknown, never evidence.
            Err(error) => Err(std::io::Error::other(error)),
        }
    }
}

impl std::ops::Deref for TrackedSession {
    type Target = Session;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for TrackedSession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

struct SessionGuard {
    inner: Option<TrackedSession>,
}

impl SessionGuard {
    fn new(session: Session) -> Self {
        Self {
            inner: Some(TrackedSession {
                inner: session,
                reaped: None,
            }),
        }
    }

    fn session(&mut self) -> &mut TrackedSession {
        // Invariant: `inner` is only `None` after explicit `into_session()` (none here)
        // or after `Drop` runs (no further access possible).
        self.inner.as_mut().expect("SessionGuard accessed after drop")
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let Some(mut session) = self.inner.take() else { return };
        let still_owned = match session.confirmed_exit() {
            Ok(true) => return,
            Ok(false) => true,
            Err(_) => false,
        };
        // First ECHILD/ESRCH is unknown: do not signal a potentially reused PID.
        if still_owned {
            let _ = session.get_process_mut().signal(expectrl::Signal::SIGKILL);
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                match session.confirmed_exit() {
                    Ok(true) => return,
                    Ok(false) => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
        }
        // Dependency Drop has its own signal/sleep retries and may panic.
        // Retain unknown ownership instead of entering that implicit path.
        std::mem::forget(session);
        let _ = writeln!(
            std::io::stderr(),
            "PTY cleanup UNKNOWN; scratch retained; no descendant-exit claim"
        );
        assert!(
            std::thread::panicking(),
            "PTY cleanup did not confirm direct-child reap"
        );
    }
}

impl Drop for HarnessGuard {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(root) = self.tempdir.take()
        {
            let _ = writeln!(
                std::io::stderr(),
                "PTY failure scratch retained: {}",
                root.keep().display()
            );
        }
    }
}

/// Read from the PTY until `needle` appears in the accumulated buffer or
/// `deadline` is hit. Whenever a `ESC[6n` DSR cursor-position query is seen
/// in the stream, write back a synthetic `ESC[1;1R` reply so reedline /
/// crossterm don't block on a non-existent terminal emulator.
///
/// Returns the complete accumulated buffer on success. On timeout panics
/// with the captured buffer so the failure message is actionable.
fn read_until_with_dsr(session: &mut TrackedSession, needle: &str, total: Duration) -> String {
    let deadline = Instant::now() + total;
    let mut accum: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    // Set once the process has exited; the next empty read then ends the wait.
    let mut exited = false;
    loop {
        // 1. drain any pending bytes
        match session.try_read(&mut buf) {
            Ok(0) => {
                // EOF
                let s = String::from_utf8_lossy(&accum).into_owned();
                if s.contains(needle) {
                    return s;
                }
                panic!("PTY hit EOF before `{needle}` appeared. Captured:\n----\n{s}\n----");
            }
            Ok(n) => {
                accum.extend_from_slice(&buf[..n]);
                // Reply to every DSR query we've seen but not yet acked.
                while let Some(pos) = window(&accum, DSR_QUERY) {
                    // Replace the query in-place with empty bytes so we don't
                    // re-detect on the next pass.
                    for b in &mut accum[pos..pos + DSR_QUERY.len()] {
                        *b = 0;
                    }
                    if let Err(e) = session.send(DSR_REPLY) {
                        eprintln!("warning: failed to send DSR reply: {e}");
                    }
                }
                let view = String::from_utf8_lossy(&accum);
                if view.contains(needle) {
                    return view.into_owned();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if exited {
                    let s = String::from_utf8_lossy(&accum).into_owned();
                    panic!("the chat process exited before `{needle}` appeared. Captured:\n----\n{s}\n----");
                }
                // No data right now: check the process is still there (one
                // more read drains what it wrote before exiting), then yield.
                exited = matches!(session.confirmed_exit(), Ok(true));
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let s = String::from_utf8_lossy(&accum).into_owned();
                panic!("PTY read error before `{needle}`: {e}. Captured:\n----\n{s}\n----");
            }
        }
        if Instant::now() >= deadline {
            let s = String::from_utf8_lossy(&accum).into_owned();
            panic!(
                "timeout waiting for `{needle}` after {total:?}. Captured ({} bytes):\n----\n{s}\n----",
                accum.len()
            );
        }
    }
}

/// Reply once per `ESC[6n` occurrence seen in `chunk`, using a cross-chunk
/// accumulation buffer so queries that straddle read boundaries are never
/// missed. The caller passes `pending` by &mut so state persists across
/// multiple calls within the same polling loop.
///
/// A single read can return several DSR queries back-to-back (reedline prints
/// a few during init); replying only once leaves the others unanswered and
/// reedline parks forever.
fn ack_all_dsr_with_pending(session: &mut TrackedSession, chunk: &[u8], pending: &mut String) {
    // Append fresh bytes to the cross-chunk accumulation buffer.
    pending.push_str(&String::from_utf8_lossy(chunk));

    // Scan and consume all complete DSR queries from the front of the buffer.
    // We drain bytes as we go so the buffer never grows unboundedly.
    let dsr_query_str = "\x1b[6n";
    while let Some(idx) = pending.find(dsr_query_str) {
        // Discard everything before the query, then discard the query itself.
        pending.drain(..idx + dsr_query_str.len());
        if let Err(e) = session.send(DSR_REPLY) {
            eprintln!("warning: failed to send DSR reply: {e}");
            return;
        }
    }

    // After consuming all complete queries, keep only any trailing bytes that
    // might be the start of an incomplete ESC sequence (at most 3 bytes: an
    // `\x1b` that has not yet been followed by `[6n`). Everything else is
    // already-processed output that we don't need to hold on to.
    //
    // `\x1b[6n` is 4 bytes, so a tail shorter than 4 bytes starting with `\x1b`
    // is the only possible "half-sequence" we need to preserve.
    let drain_to = pending
        .rfind('\x1b')
        .filter(|&i| pending.len() - i < dsr_query_str.len())
        .unwrap_or(pending.len());
    // Ensure we drain only up to a valid UTF-8 char boundary.
    let safe_drain_to = (0..=drain_to).rev().find(|&i| pending.is_char_boundary(i)).unwrap_or(0);
    pending.drain(..safe_drain_to);
}

/// Drain any pending bytes from the PTY without blocking, replying to any
/// DSR queries encountered. Used between commands to keep reedline happy.
///
/// Uses a persistent `pending` buffer across chunks so that DSR queries
/// straddling read boundaries are never missed.
fn drain_with_dsr(session: &mut TrackedSession, total: Duration) {
    let deadline = Instant::now() + total;
    let mut buf = [0u8; 4096];
    let mut pending = String::new();
    while Instant::now() < deadline {
        match session.try_read(&mut buf) {
            Ok(0) => return,
            Ok(n) => ack_all_dsr_with_pending(session, &buf[..n], &mut pending),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return,
        }
    }
}

/// First-occurrence substring search byte-wise. Returned index is into the
/// haystack. None when not found.
fn window(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Wait up to `total` for the child process to exit. Returns true only when
/// the owner has recorded Exited/Signaled, not merely because the PTY fd
/// produced a read error or a wait returned ECHILD without recorded evidence.
///
/// Previously, any non-WouldBlock read error was treated as "process exited",
/// which could hide hang regressions: a live process that triggers a transient
/// I/O error (e.g. EPIPE on the slave PTY side) would falsely look like it
/// exited. We now keep draining for DSR purposes but authoritative exit
/// detection uses owner-recorded Exited/Signaled status.
fn wait_for_exit(session: &mut TrackedSession, total: Duration) -> bool {
    let deadline = Instant::now() + total;
    let mut pending_dsr = String::new();
    while Instant::now() < deadline {
        // Keep draining + replying to DSR while we wait so a still-alive
        // reedline doesn't sit blocked on a cursor query.
        let mut buf = [0u8; 4096];
        match session.try_read(&mut buf) {
            Ok(0) => {
                // EOF on the PTY stream — still need to confirm via owner status.
            }
            Ok(n) => ack_all_dsr_with_pending(session, &buf[..n], &mut pending_dsr),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                // Log the error but do NOT treat it as proof of exit — the
                // process may still be alive with a broken PTY stream.
                eprintln!("wait_for_exit: PTY read error ({e}); continuing owner status check");
            }
        }
        // Authoritative exit check: ask the OS whether the child is still alive.
        match session.confirmed_exit() {
            Ok(true) => return true,
            Ok(false) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => {
                // A transient waitpid/PTY race here is not proof of a clean
                // process exit. Keep polling until the deadline so SessionGuard
                // does not kill a still-flushing chat process before session
                // persistence reaches disk.
                eprintln!("wait_for_exit: status error ({e}); continuing until deadline");
                std::thread::sleep(Duration::from_millis(40));
            }
        }
    }
    false
}

fn wait_for_session_list_containing(guard: &HarnessGuard, needle: &str, total: Duration) -> String {
    let deadline = Instant::now() + total;
    let mut last_stdout = String::new();
    let mut last_stderr = String::new();
    while Instant::now() < deadline {
        let output = build_chat_command_in(guard, &["--list-sessions"], &[])
            .output()
            .expect("list sessions");
        last_stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        last_stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "list sessions failed: status={:?}, stderr={last_stderr}",
            output.status
        );
        if last_stdout.contains(needle) {
            return last_stdout;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("session list did not contain `{needle}` within {total:?}. stdout:\n{last_stdout}\nstderr:\n{last_stderr}");
}

fn run_chat_script_in(guard: &HarnessGuard, extra_args: &[&str], extra_env: &[(&str, &str)], script: &str) -> String {
    let mut child = build_chat_command_in(guard, extra_args, extra_env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn scripted chat");
    {
        let stdin = child.stdin.as_mut().expect("scripted chat stdin");
        stdin.write_all(script.as_bytes()).expect("write scripted chat input");
        stdin.flush().expect("flush scripted chat input");
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait scripted chat");
    assert!(
        output.status.success(),
        "scripted chat failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn list_sessions_in(guard: &HarnessGuard) -> String {
    let output = build_chat_command_in(guard, &["--list-sessions"], &[])
        .output()
        .expect("list sessions");
    assert!(
        output.status.success(),
        "list sessions failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn first_saved_session_id(list_output: &str) -> String {
    list_output
        .lines()
        .find_map(|line| line.trim().split_once(" | ").map(|(id, _)| id.to_string()))
        .expect("saved session id in list output")
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// 1. Banner is visible on startup.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_banner_visible() {
    let (mut sg, _guard) = spawn_chat(&[], &[]);
    let session = sg.session();

    // Banner format: "prx <ver> · mock/mock" (src/chat/mod.rs:634).
    let captured = read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    assert!(
        captured.contains("prx "),
        "banner should start with `prx `, got:\n{captured}"
    );
    assert!(
        captured.contains("mock/mock"),
        "banner should include `mock/mock`, got:\n{captured}"
    );

    // Clean exit. reedline raw mode treats CR as Enter.
    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
    // SessionGuard drop runs cleanup automatically (including on panic).
}

/// 2. A user message receives a mock response carrying our sentinel.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_mock_response_with_sentinel() {
    let sentinel = "[MOCK-END-A1B2]";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let session = sg.session();

    // Wait for banner first so reedline is ready to consume our input.
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    // Send a user line. reedline expects CR for Enter under raw mode.
    session.send("hello\r").expect("send hello");

    // Mock provider returns our sentinel verbatim.
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(captured.contains(sentinel), "mock response should contain `{sentinel}`");

    // Clean exit.
    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

/// 3. `/exit` slash command terminates the session cleanly.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_exit_command_clean() {
    let (mut sg, _guard) = spawn_chat(&[], &[]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "process did not exit within {EXIT_TIMEOUT:?} after /exit"
    );

    // We accept any non-signal exit because the surface differs across
    // platforms; the regression we care about is the process not exiting
    // at all on /exit.
    if let Some(status) = session.reaped {
        match status {
            expectrl::WaitStatus::Exited(_, code) => {
                assert_eq!(code, 0, "non-zero exit code after /exit: {code}");
            }
            other => panic!("unexpected wait status after /exit: {other:?}"),
        }
    }
}

/// Chat tracing must move to `chat.log` before configuration is loaded. If it
/// stays on stderr until the TUI starts, those startup lines sit behind the
/// alternate screen and are revealed as stale terminal output on exit.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_redirects_startup_tracing_before_tui() {
    let guard = new_harness_guard().expect("build chat harness");
    let cmd = build_chat_command_in(&guard, &[], &[("PRX_TUI", "1"), ("RUST_LOG", "info")]);
    let mut sg = SessionGuard::new(spawn_chat_command(cmd));
    let session = sg.session();

    let captured = read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    assert!(
        !captured.contains("Config loaded") && !captured.contains("openprx::config::schema"),
        "startup tracing must not be written to the chat terminal:\n{captured}"
    );

    session.send("\x04").expect("send Ctrl-D");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "full-screen chat did not exit after Ctrl-D"
    );

    let log_path = guard.home_dir.join(".openprx/chat.log");
    let log = std::fs::read_to_string(&log_path).expect("read redirected chat.log");
    assert!(
        log.contains("Config loaded"),
        "startup tracing should be retained in chat.log, got:\n{log}"
    );
}

/// 4. Double Ctrl-C exits the session — *both* the user-visible
/// `Exiting...` banner AND a fully-terminated process within the agreed
/// timeout. No fallback escalation (Ctrl-D / SIGHUP) is allowed; this
/// test is the regression detector for the runtime-drop hang bug.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_double_ctrl_c_exit() {
    let (mut sg, _guard) = spawn_chat(&[], &[]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    // First SIGINT — at idle this just stamps last_ctrlc_ms (see
    // src/chat/mod.rs:804). Within DOUBLE_CTRLC_WINDOW_MS a second SIGINT
    // must trigger graceful shutdown.
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");

    let captured = read_until_with_dsr(session, "Exiting...", EXIT_TIMEOUT);
    assert!(
        captured.contains("Exiting..."),
        "expected `Exiting...` after double Ctrl-C, captured:\n{captured}"
    );

    // ── Real exit contract: process must terminate ─────────────────────
    // The full double-Ctrl-C contract is: print `Exiting...` AND exit.
    // Previously the suite escalated via Ctrl-D / SIGHUP because reedline
    // parks in `spawn_blocking` on stdin; the runtime drop in `main` now
    // bounds that wait via `runtime.shutdown_timeout` (see main.rs's
    // `RUNTIME_SHUTDOWN_TIMEOUT`). If this assertion fails, the hang bug
    // has regressed.
    let exit_deadline = EXIT_TIMEOUT;
    assert!(
        wait_for_exit(session, exit_deadline),
        "process did NOT exit within {exit_deadline:?} after double Ctrl-C \
         even though `Exiting...` was emitted. This is the runtime-drop hang \
         bug — see main.rs RUNTIME_SHUTDOWN_TIMEOUT."
    );
}

/// 5. `--plain` mode means the mock response is emitted without ANSI color
/// escapes wrapping it. Cursor-positioning / DSR replies from reedline are
/// tolerated; what we assert is the absence of SGR colour codes in the
/// window between sending input and seeing the mock sentinel.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_mode_no_ansi() {
    let sentinel = "[MOCK-PLAIN-C3D4]";
    let (mut sg, _guard) = spawn_chat(&["--plain"], &[("OPENPRX_MOCK_RESPONSE", sentinel), ("PRX_TUI", "1")]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);

    // Inspect only the contiguous run of bytes immediately surrounding the
    // sentinel — i.e. the chat renderer's output for that line. reedline's
    // prompt elsewhere in the stream is allowed to emit SGR codes
    // (reedline's responsibility, not the chat plain-mode contract).
    //
    // The sentinel arrives on its own logical line: scan back to the
    // nearest newline or CSI cursor-positioning sequence, scan forward
    // similarly, and assert that slice has no SGR-coloured runs.
    let sentinel_pos = captured.rfind(sentinel).expect("sentinel was found above");
    // Scan backwards up to 256 bytes or to a newline / CR.
    let lookback_start = sentinel_pos.saturating_sub(256);
    let lookback = &captured[lookback_start..sentinel_pos];
    let start = lookback
        .rfind(['\n', '\r'])
        .map_or(lookback_start, |i| lookback_start + i + 1);
    // Scan forwards up to 256 bytes or to a newline / CR.
    let end_window = sentinel_pos + sentinel.len();
    let lookahead_end = (end_window + 256).min(captured.len());
    let lookahead = &captured[end_window..lookahead_end];
    let end = lookahead.find(['\n', '\r']).map_or(lookahead_end, |i| end_window + i);
    let line_window = &captured[start..end];

    let sgr_color = regex::Regex::new(r"\x1b\[[0-9;]*m").expect("sgr regex");
    let matches: Vec<&str> = sgr_color.find_iter(line_window).map(|m| m.as_str()).collect();
    assert!(
        matches.is_empty(),
        "SGR colour codes wrap the mock response under --plain: {matches:?}\nresponse line: {line_window:?}"
    );
    assert!(
        !captured.contains("\x1b[?2004h"),
        "--plain must bypass TerminalGuard/TUI even when PRX_TUI=1; captured bracketed-paste enable in:\n{captured}"
    );
    assert!(
        !captured.contains("PRX Chat |")
            && !captured.contains("Ctrl+G sessions")
            && !captured.contains("Ctrl+O transcript")
            && !captured.contains("Ctrl+R reverse-search")
            && !captured.contains("Ctrl+X Ctrl+E edit")
            && !captured.contains("Shift+Tab mode")
            && !captured.contains("mode:")
            && !captured.contains("auth:")
            && !captured.contains("attached #")
            && !captured.contains("transcript "),
        "--plain must not render TUI chrome or child viewport headers; captured:\n{captured}"
    );
    assert!(
        !captured.contains("/ 1M tok") && !captured.contains("/ 10M tok"),
        "--plain must not render status-bar context window chrome; captured:\n{captured}"
    );

    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_diff_no_tui_chrome() {
    let (mut sg, _guard) = spawn_chat(&["--plain"], &[("PRX_TUI", "1")]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session.send("/diff\r").expect("send /diff");
    let captured = read_until_with_dsr(session, "diff unavailable:", TURN_TIMEOUT);
    assert!(
        !captured.contains("PRX Chat |")
            && !captured.contains("Ctrl+G sessions")
            && !captured.contains("diff workspace diff")
            && !captured.contains("attached #0"),
        "--plain /diff must emit plain text only, not TUI chrome; captured:\n{captured}"
    );

    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_attach_no_tui_chrome() {
    let guard = new_harness_guard().expect("harness guard");
    let output = run_chat_script_in(
        &guard,
        &["--plain"],
        &[("PRX_TUI", "1"), ("OPENPRX_MOCK_RESPONSE", "ATTACH_UNUSED")],
        "/attach 1\n/exit\n",
    );
    assert!(
        output.contains("Attach failed"),
        "plain /attach should run the attach handler and fail visibly for a missing child session; output:\n{output}"
    );
    assert!(
        !output.contains("PRX Chat |")
            && !output.contains("Ctrl+G sessions")
            && !output.contains("attached #")
            && !output.contains("child viewport")
            && !output.contains("diff workspace diff"),
        "--plain /attach must not render TUI chrome; output:\n{output}"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_file_mention_no_tui_chrome() {
    let sentinel = "[MOCK-FILE-MENTION-OK]";
    let guard = new_harness_guard().expect("harness guard");
    std::fs::write(guard.workspace_dir.join("note.txt"), "file mention body").expect("write mention file");
    let mut sg = spawn_chat_in(
        &guard,
        &["--plain"],
        &[("PRX_TUI", "1"), ("OPENPRX_MOCK_RESPONSE", sentinel)],
    );
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session.send("please read @note.txt\r").expect("send mention");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(captured.contains(sentinel), "mock response should contain `{sentinel}`");
    assert!(
        !captured.contains("PRX Chat |")
            && !captured.contains("Ctrl+G sessions")
            && !captured.contains("attached #")
            && !captured.contains("diff workspace diff"),
        "--plain @path must not render TUI chrome; captured:\n{captured}"
    );

    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_resume_list_no_tui_chrome() {
    let guard = new_harness_guard().expect("harness guard");

    let _ = run_chat_script_in(
        &guard,
        &["--plain"],
        &[("OPENPRX_MOCK_RESPONSE", "LIST_REPLY")],
        "list me\n/exit\n",
    );

    let output = run_chat_script_in(&guard, &["--plain"], &[], "/resume\n/exit\n");
    assert!(
        output.contains("Saved chat sessions:"),
        "plain /resume should emit the text session list; output:\n{output}"
    );
    assert!(
        output.contains("Resume with: /resume <ID> or /resume last"),
        "plain /resume should include text resume instructions; output:\n{output}"
    );
    assert!(
        !output.contains("PRX Chat |")
            && !output.contains("Saved chat sessions (/resume)")
            && !output.contains("Ctrl+G sessions")
            && !output.contains("attached #")
            && !output.contains("diff workspace diff"),
        "--plain /resume list must not render TUI chrome; output:\n{output}"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_resume_command_switches_saved_session_no_tui_chrome() {
    let guard = new_harness_guard().expect("harness guard");

    let _ = run_chat_script_in(
        &guard,
        &["--plain"],
        &[("OPENPRX_MOCK_RESPONSE", "ALPHA_REPLY")],
        "alpha question\n/exit\n",
    );
    let alpha_id = first_saved_session_id(&list_sessions_in(&guard));

    let output = run_chat_script_in(
        &guard,
        &["--plain"],
        &[("PRX_TUI", "1"), ("OPENPRX_MOCK_RESPONSE", "BETA_REPLY")],
        &format!("beta question\n/resume {alpha_id}\n/cost\n/exit\n"),
    );

    assert!(
        output.contains("Resumed saved chat session"),
        "resume confirmation missing; output:\n{output}"
    );
    assert!(
        output.contains("Turns:        2"),
        "cost after /resume should reflect alpha session's exact two persisted turns; output:\n{output}"
    );
    assert!(
        !output.contains("PRX Chat |")
            && !output.contains("Ctrl+G sessions")
            && !output.contains("attached #")
            && !output.contains("diff workspace diff"),
        "--plain /resume must not render TUI chrome; output:\n{output}"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_continue_resumes_last_session_no_tui_chrome() {
    let guard = new_harness_guard().expect("harness guard");

    let _ = run_chat_script_in(
        &guard,
        &["--plain"],
        &[("OPENPRX_MOCK_RESPONSE", "CONTINUE_REPLY")],
        "continue question\n/exit\n",
    );
    let output = run_chat_script_in(
        &guard,
        &["--plain", "--continue"],
        &[("PRX_TUI", "1"), ("OPENPRX_MOCK_RESPONSE", "CONTINUE_SECOND")],
        "/cost\n/exit\n",
    );

    assert!(
        output.contains("Turns:        2"),
        "--continue should resume last saved chat session; output:\n{output}"
    );
    assert!(
        !output.contains("PRX Chat |")
            && !output.contains("Ctrl+G sessions")
            && !output.contains("attached #")
            && !output.contains("diff workspace diff"),
        "--plain --continue must not render TUI chrome; output:\n{output}"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_branch_forks_prefix_no_tui_chrome() {
    let guard = new_harness_guard().expect("harness guard");

    let _ = run_chat_script_in(
        &guard,
        &["--plain"],
        &[("OPENPRX_MOCK_RESPONSE", "BRANCH_SOURCE_REPLY")],
        "branch source\n/exit\n",
    );
    let source_id = first_saved_session_id(&list_sessions_in(&guard));

    let output = run_chat_script_in(
        &guard,
        &["--plain"],
        &[("PRX_TUI", "1"), ("OPENPRX_MOCK_RESPONSE", "BRANCH_UNUSED_REPLY")],
        &format!("/resume {source_id}\n/branch 1\n/cost\n/exit\n"),
    );

    assert!(
        output.contains("Created branch"),
        "branch confirmation missing; output:\n{output}"
    );
    assert!(
        output.contains("Turns:        1"),
        "cost after /branch 1 should reflect the forked one-turn prefix; output:\n{output}"
    );
    assert!(
        !output.contains("PRX Chat |")
            && !output.contains("Ctrl+G sessions")
            && !output.contains("attached #")
            && !output.contains("diff workspace diff"),
        "--plain /branch must not render TUI chrome; output:\n{output}"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_rewind_env_override_cannot_bypass_interactive_confirmation() {
    let guard = new_harness_guard().expect("harness guard");

    let _ = run_chat_script_in(
        &guard,
        &["--plain"],
        &[("OPENPRX_MOCK_RESPONSE", "REWIND_SOURCE_REPLY")],
        "rewind source\n/exit\n",
    );
    let source_id = first_saved_session_id(&list_sessions_in(&guard));

    let output = run_chat_script_in(
        &guard,
        &["--plain"],
        &[
            ("PRX_TUI", "1"),
            ("OPENPRX_APPROVAL_OVERRIDE", "allow"),
            ("OPENPRX_MOCK_RESPONSE", "REWIND_UNUSED_REPLY"),
        ],
        &format!("/resume {source_id}\n/rewind 0\n/cost\n/exit\n"),
    );

    assert!(
        output.contains("Rewind requires interactive confirmation"),
        "plain rewind must fail closed before any env override path; output:\n{output}"
    );
    assert!(
        output.contains("Turns:        2"),
        "plain rewind must leave the resumed two-turn session unchanged; output:\n{output}"
    );
    assert!(
        !output.contains("PRX Chat |")
            && !output.contains("Ctrl+G sessions")
            && !output.contains("attached #")
            && !output.contains("diff workspace diff"),
        "--plain /rewind fail-closed path must not render TUI chrome; output:\n{output}"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_apply_env_override_cannot_bypass_interactive_confirmation() {
    let guard = new_harness_guard().expect("harness guard");
    let target = guard.workspace_dir.join("apply.txt");
    std::fs::write(&target, "old\n").expect("seed apply target");
    let assistant_patch = "```diff\n--- a/apply.txt\n+++ b/apply.txt\n@@ -1 +1 @@\n-old\n+new\n```";

    let output = run_chat_script_in(
        &guard,
        &["--plain"],
        &[
            ("PRX_TUI", "1"),
            ("OPENPRX_APPROVAL_OVERRIDE", "allow"),
            ("OPENPRX_MOCK_RESPONSE", assistant_patch),
        ],
        "please propose a patch\n/apply\n/exit\n",
    );

    assert!(
        output.contains("Diff apply requires interactive TUI approval"),
        "plain /apply must fail closed through the real sessions_redraw_handle gate; output:\n{output}"
    );
    assert!(
        output.contains("Workspace unchanged"),
        "plain /apply must visibly report unchanged workspace; output:\n{output}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("read target"),
        "old\n",
        "OPENPRX_APPROVAL_OVERRIDE=allow must not let plain /apply mutate files"
    );
    assert!(
        !output.contains("PRX Chat |")
            && !output.contains("Ctrl+G sessions")
            && !output.contains("attached #")
            && !output.contains("diff workspace diff"),
        "--plain /apply fail-closed path must not render TUI chrome; output:\n{output}"
    );
}

/// 6. East Asian wide (CJK) characters in the mock response must appear contiguously
/// — no phantom space between each character (regression guard for the
/// `insert_before` wide-char bug fixed by enabling the `scrolling-regions`
/// ratatui feature).
///
/// We use `--plain` mode (reedline + `BufRead`) so the response lands on
/// stdout verbatim, which lets the PTY scraper check the raw byte sequence.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_wide_characters_response_no_extra_spaces() {
    // The mock response: 6 wide CJK chars with no ASCII filler in between.
    // If the wide-char bug is present, the captured output will contain one
    // space after each glyph.
    let response = "\u{ac00}\u{ac01}\u{ac02}\u{ac03}\u{ac04}\u{ac05}PRX";
    let (mut sg, _guard) = spawn_chat(&["--plain"], &[("OPENPRX_MOCK_RESPONSE", response)]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session.send("hi\r").expect("send hi");
    // Wait until the first wide CJK character appears in the output.
    let captured = read_until_with_dsr(session, "\u{ac00}", TURN_TIMEOUT);

    // Strip ANSI escape sequences from the captured buffer so we compare
    // plain text only. The DSR reply (`ESC[1;1R`) and reedline SGR codes
    // would otherwise pollute the search.
    let ansi_re = regex::Regex::new(r"\x1b\[[^a-zA-Z]*[a-zA-Z]").expect("ansi regex");
    let plain = ansi_re.replace_all(&captured, "").to_string();

    // The wide-char substring must appear as a contiguous run (no spaces).
    assert!(
        plain.contains("\u{ac00}\u{ac01}\u{ac02}\u{ac03}\u{ac04}\u{ac05}PRX"),
        "wide-char response should contain the six wide chars contiguously (no inter-character \
         spaces). Plain output: {plain:?}"
    );
    // Explicit negative: spaces between consecutive CJK chars would indicate
    // the phantom-space bug has regressed.
    assert!(
        !plain.contains("\u{ac00} \u{ac01}") && !plain.contains("\u{ac01} \u{ac02}"),
        "phantom spaces detected between CJK chars in response. Plain output: {plain:?}"
    );

    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

// ─── Pure-mode PTY E2E coverage ─────────────────────────────────────────────
//
// The single reducer/driver route is the default path, and session persistence is
// taken over by the reducer's `Effect::SaveSession`.
//
// These two tests are the key regression guards for the Pure-only path:
//   - mock_response_works: an end-to-end Pure-mode conversation runs (input -> driver -> render)
//   - tool_call_completes: the driver attaches tools_registry automatically

#[test]
#[serial(prx_chat_pty)]
fn test_chat_pure_mock_response_works() {
    // Pure mode + mock provider: user input -> driver streams ->
    // reducer StreamCompleted -> SaveSession Effect -> memory.store (this test only
    // asserts the sentinel renders to the PTY; persistence is covered by unit tests).
    let sentinel = "[MOCK-PURE]";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "the mock reply must render in Pure mode. captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "/exit must exit cleanly in Pure mode"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_pure_tool_call_completes() {
    // Pure mode + the real tools_registry: the driver must attach tools_registry by
    // default for the tool turn to close the loop.
    let sentinel = "[MOCK-PURE-TOOL-ROUND-2]";
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_RESPONSE", sentinel),
            ("OPENPRX_MOCK_TOOL_CALL", "memory_recall:{\"query\":\"hi\",\"limit\":3}"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "the Pure-mode driver tool-call loop must return the final sentinel. captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "/exit must exit cleanly after a Pure-mode tool turn"
    );
}

/// chat::run level integration test — runs a full Pure-mode turn plus /exit and checks the
/// reducer's single-source persistence path. Reducer unit tests on their own cannot reach the
/// exit branch of the chat::run main loop; this test walks the full path through a real PTY.
#[test]
#[serial(prx_chat_pty)]
fn s4_a_p1_pure_exit_after_turn_chat_run_level() {
    let sentinel = "[MOCK-PURE-EXIT]";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    // turn 1: full user -> assistant final
    session.send("hi\r").expect("send hi");
    let captured1 = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured1.contains(sentinel),
        "Pure chat::run must render the turn 1 sentinel. captured:\n{captured1}"
    );

    // /exit must go through the chat::run main-loop break + legacy_exit_save_enabled=false path
    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "after a full Pure chat::run turn, /exit must exit cleanly within {EXIT_TIMEOUT:?}"
    );
}

/// P4a: detached Redux turns are finalized by the outer event pump while
/// visible admission remains N=1. Two sequential turns must complete and exit
/// cleanly through the same PTY transcript path as the P3c runtime.
#[test]
#[serial(prx_chat_pty)]
fn p4a_event_pump_two_sequential_turns_complete_and_exit() {
    let sentinel = "[MOCK-P4A-SEQUENTIAL]";
    let first_message = "p4a first visible turn";
    let second_message = "p4a second visible turn";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session
        .send(format!("{first_message}\r").as_str())
        .expect("send first turn");
    let first = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        first.contains(sentinel),
        "P4a first turn should complete. captured:\n{first}"
    );

    session
        .send(format!("{second_message}\r").as_str())
        .expect("send second turn");
    let second = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        second.contains(sentinel),
        "P4a second sequential turn should complete. captured:\n{second}"
    );

    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "P4a sequential turn session should exit"
    );
}

/// A later ordinary conversation turn has provider history but no external
/// target or failed work. The real Chat path must serve it through the model
/// instead of turning the presence of history into a Clarify terminal.
#[test]
#[serial(prx_chat_pty)]
fn ordinary_plain_second_turn_reaches_the_serving_model() {
    let sentinel = "[MOCK-PLAIN-CONVERSATION]";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session.send("hello\r").expect("send first plain turn");
    let first = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        first.contains(sentinel),
        "first plain turn should reach the model: {first}"
    );

    session.send("thanks\r").expect("send second plain turn");
    let second = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        second.contains(sentinel),
        "second plain turn with conversation history should reach the model: {second}"
    );

    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "plain conversation should exit cleanly"
    );
}

/// The default decision policy must serve a plain turn without an operator
/// capability declaration for the selected model. The mock's zero tariff is
/// test accounting only; the causal tree and provider capability catalog stay
/// exactly at their defaults.
#[test]
#[serial(prx_chat_pty)]
fn default_config_plain_turn_reaches_the_model_without_a_model_declaration() {
    let sentinel = "[MOCK-DEFAULT-CONFIG]";
    let guard = new_harness_guard().expect("build default-config chat harness");
    let defaults = openprx::config::Config::default();
    let mut config = defaults.clone();
    config.cost.prices.insert(
        "mock/mock".to_string(),
        openprx::config::schema::ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    assert_eq!(config.causal_tree, defaults.causal_tree);
    assert_eq!(config.providers.models, defaults.providers.models);
    assert!(
        config.providers.models.is_empty(),
        "default config has no model declarations"
    );
    std::fs::write(
        guard.config_dir.join("config.toml"),
        toml::to_string_pretty(&config).expect("serialize default decision fixture"),
    )
    .expect("write default decision fixture");

    let command = build_unprepared_chat_command_in(&guard, &[], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let mut session_guard = SessionGuard::new(spawn_chat_command(command));
    let session = session_guard.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session.send("hello\r").expect("send default-config plain turn");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "a default-config plain turn should reach the model instead of Clarify: {captured}"
    );

    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "default-config plain conversation should exit cleanly"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_session_resume_last_restores_saved_turns() {
    let first_response = "[MOCK-RESUME-FIRST]";
    let second_response = "[MOCK-RESUME-SECOND]";
    let first_message = format!("resume-check-{}", std::process::id());

    let guard = new_harness_guard().expect("build chat harness");
    let captured = run_chat_script_in(
        &guard,
        &[],
        &[("OPENPRX_MOCK_RESPONSE", first_response)],
        &format!("{first_message}\n/exit\n"),
    );
    assert!(
        captured.contains(first_response),
        "first scripted chat process should complete turn before resume test. captured:\n{captured}"
    );
    let before_stdout = wait_for_session_list_containing(&guard, &first_message, TURN_TIMEOUT);
    assert!(
        before_stdout.contains(&first_message) && before_stdout.contains("2 turns"),
        "first session should be saved with one user/assistant pair. stdout:\n{before_stdout}"
    );

    let captured2 = run_chat_script_in(
        &guard,
        &["--session", "last"],
        &[("OPENPRX_MOCK_RESPONSE", second_response)],
        "resume followup\n/exit\n",
    );
    assert!(
        captured2.contains(second_response),
        "resumed scripted chat process should complete follow-up turn. captured:\n{captured2}"
    );

    let after_stdout = wait_for_session_list_containing(&guard, &first_message, TURN_TIMEOUT);
    assert!(
        after_stdout.contains(&first_message) && after_stdout.contains("4 turns"),
        "`--session last` should append the follow-up to the original saved session. stdout:\n{after_stdout}"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_pure_double_ctrl_c_exits_cleanly() {
    // A double Ctrl+C must not hang round 2 in Pure mode.
    let (mut sg, _guard) = spawn_chat(&[], &[]);
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");

    let captured = read_until_with_dsr(session, "Exiting...", EXIT_TIMEOUT);
    assert!(
        captured.contains("Exiting..."),
        "a double Ctrl+C must print Exiting... in Pure mode; captured:\n{captured}"
    );
    let exit_deadline = EXIT_TIMEOUT;
    assert!(
        wait_for_exit(session, exit_deadline),
        "a double Ctrl+C must exit within {exit_deadline:?} in Pure mode"
    );
}

// ─── S4-A Commit 0: minimal E2E over the real ratatui path ───────────────────
//
// All 14 existing PTY tests inject `PRX_TUI=0` into the child process and land on the
// reedline + BufRead fallback, leaving the real ratatui path (`run_tui_unified_loop`)
// with zero regression coverage. S4-A must have regression coverage of the real path
// before switching the render source, hence these 3 added tests:
//   - banner rendering (visible at startup)
//   - streaming mock response rendering
//   - double Ctrl+C exit
//
// Injecting `PRX_TUI=1` through `extra_env` overrides the default `PRX_TUI=0` and makes
// chat::run take `TerminalGuard::enter()` + `spawn_tui_unified_loop`. ratatui uses
// `Viewport::Inline` and does not enter the alt-screen, so the bytes still go to the main
// buffer where the PTY scraper can capture them. The banner goes out through
// `chat_mirror.lock().push_system_message(&banner)` followed by `terminal.insert_before`
// writing to stdout (mod.rs:1084 + 2590).

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_0_ratatui_banner_visible_via_real_path() {
    // PRX_TUI=1 forces the real ratatui path. The banner is written into the main-screen
    // scrollback through insert_before, so the PTY scraper can pick up "mock/mock".
    let (mut sg, _guard) = spawn_chat(&[], &[("PRX_TUI", "1")]);
    let session = sg.session();

    let captured = read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    assert!(
        captured.contains("prx "),
        "banner must start with `prx `, got:\n{captured}"
    );
    assert!(
        captured.contains("mock/mock"),
        "banner must contain `mock/mock`, got:\n{captured}"
    );

    // Exit with a double Ctrl+C (/exit also works on the ratatui path, but after sending
    // \r ratatui's raw-mode line discipline differs from reedline's, so SIGINT*2 is more
    // reliable).
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_0_ratatui_mock_response_via_real_path() {
    // On the real ratatui path: user input -> the mock provider streams back the sentinel
    // -> ratatui insert_before's the ConversationLine::Assistant onto the main screen. The
    // PTY scraper must see the sentinel on the main screen.
    let sentinel = "[S4A0-RATATUI-MOCK]";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", sentinel), ("PRX_TUI", "1")]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(300));

    // In ratatui raw mode Enter is still \r (crossterm KeyCode::Enter).
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "the mock reply must render on the real ratatui path. captured:\n{captured}"
    );

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_ratatui_resume_picker_selects_saved_session() {
    let guard = new_harness_guard().expect("harness guard");

    let _ = run_chat_script_in(
        &guard,
        &["--plain"],
        &[("OPENPRX_MOCK_RESPONSE", "ALPHA_REPLY")],
        "alpha one\nalpha two\n/exit\n",
    );
    let _ = run_chat_script_in(
        &guard,
        &["--plain"],
        &[("OPENPRX_MOCK_RESPONSE", "BETA_REPLY")],
        "beta one\n/exit\n",
    );

    let mut sg = spawn_chat_in(&guard, &[], &[("PRX_TUI", "1"), ("OPENPRX_MOCK_RESPONSE", "UNUSED")]);
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(300));

    session.send("/resume\r").expect("send /resume");
    let picker = read_until_with_dsr(session, "Saved chat sessions (/resume)", TURN_TIMEOUT);
    assert!(
        picker.contains("Saved chat sessions (/resume)"),
        "TUI /resume should open saved-session picker overlay; captured:\n{picker}"
    );

    session.send("\x1b[B\r").expect("select older saved session");
    let resumed = read_until_with_dsr(session, "Resumed saved chat session", TURN_TIMEOUT);
    assert!(
        resumed.contains("Resumed saved chat session"),
        "picker Enter should resume the selected saved session; captured:\n{resumed}"
    );

    session.send("/cost\r").expect("send /cost");
    let cost = read_until_with_dsr(session, "Turns:", TURN_TIMEOUT);
    assert!(
        cost.contains("Turns:        4"),
        "selected alpha session should have four persisted turns; captured:\n{cost}"
    );

    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_0_ratatui_double_ctrl_c_exit_via_real_path() {
    // Double Ctrl+C exit semantics on the real ratatui path — the
    // `KeyDispatch::InterruptTurn` branch inside `run_tui_unified_loop` plus the
    // shutdown.cancel() path.
    let (mut sg, _guard) = spawn_chat(&[], &[("PRX_TUI", "1")]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");

    let exit_deadline = EXIT_TIMEOUT;
    assert!(
        wait_for_exit(session, exit_deadline),
        "a double Ctrl+C on the real ratatui path must exit within {exit_deadline:?}"
    );
}

// ─── Real ratatui path + Pure-mode snapshot, end to end ─────────────────────
//
// Checks that:
//   - the reducer as single source drives ratatui rendering (UiSnapshot watch path)
//   - banner / mock response / tool call / wide characters all render correctly

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_6_pure_snapshot_renders_banner_via_real_path() {
    // Real ratatui path + Pure mode. The banner is written into ui.conversation_lines by
    // the reducer's SystemMessageAdded, the dispatcher pushes the snapshot to the watch,
    // and run_tui_unified_loop reads it through RenderSource::Snapshot and insert_before's
    // it onto the main screen — the PTY scraper must pick up the "mock/mock" string.
    let (mut sg, _guard) = spawn_chat(&[], &[("PRX_TUI", "1")]);
    let session = sg.session();

    let captured = read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    assert!(
        captured.contains("prx "),
        "banner must start with `prx `, got:\n{captured}"
    );
    assert!(
        captured.contains("mock/mock"),
        "the banner must contain `mock/mock` on the Pure-mode real ratatui path, got:\n{captured}"
    );

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_6_pure_snapshot_mock_response_via_real_path() {
    // Streaming mock reply on the Pure mode + real ratatui path:
    // user input -> drive_start_turn_stream dispatches StreamChunkReceived /
    // StreamCompleted -> the reducer pushes ConversationLine::Assistant ->
    // the dispatcher pushes a UiSnapshot -> run_tui_unified_loop insert_before's the
    // main screen.
    let sentinel = "[S4A6-PURE-RATATUI]";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", sentinel), ("PRX_TUI", "1")]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(300));

    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "the mock reply must render on the Pure ratatui real path. captured:\n{captured}"
    );

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_6_pure_snapshot_tool_call_via_real_path() {
    // Tool turn loop on the Pure mode + real ratatui path:
    // round one, the LLM dispatches ToolStarted -> the reducer pushes ToolResult Running ->
    // the driver runs the tool -> ToolFinished -> the reducer updates ToolResult to Done ->
    // round two, the LLM emits the final sentinel.
    let sentinel = "[S4A6-PURE-TOOL]";
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_RESPONSE", sentinel),
            ("OPENPRX_MOCK_TOOL_CALL", "memory_recall:{\"query\":\"hi\",\"limit\":3}"),
            ("PRX_TUI", "1"),
        ],
    );
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(300));

    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "the Pure ratatui real-path tool-call loop must return the final sentinel. captured:\n{captured}"
    );

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_6_pure_snapshot_wide_characters_no_extra_spaces_via_real_path() {
    // On the Pure + real ratatui path a wide-char (CJK) response must be byte-accurate,
    // with no phantom space.
    let response = "\u{ac00}\u{ac01}\u{ac02}\u{ac03}S4A6";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", response), ("PRX_TUI", "1")]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(300));

    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, "\u{ac00}", TURN_TIMEOUT);
    // Strip ANSI escapes, then assert the wide chars are contiguous.
    let ansi_re = regex::Regex::new(r"\x1b\[[^a-zA-Z]*[a-zA-Z]").expect("ansi regex");
    let plain = ansi_re.replace_all(&captured, "").to_string();
    assert!(
        plain.contains("\u{ac00}\u{ac01}\u{ac02}\u{ac03}S4A6"),
        "wide chars must stay contiguous on the Pure ratatui real path, plain:\n{plain}"
    );
    assert!(
        !plain.contains("\u{ac00} \u{ac01}") && !plain.contains("\u{ac01} \u{ac02}"),
        "no phantom space may be present, plain:\n{plain}"
    );

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

// ─── S5 P0-1: protocol-level PTY regression (anthropic / openai / gemini flavor) ─
//
// Real LLMs are unreachable without an API key, so this group emits a complete script
// (delta + reasoning + tool + final) on the streaming path through MockEnvProvider's
// OPENPRX_MOCK_SCRIPT, covering the driver's protocol-layer regressions across provider
// flavors. See docs/release-notes-0.4.0.md for details.

/// S5 P0-1: a full anthropic-flavor turn — several deltas + reasoning + final.
#[test]
#[serial(prx_chat_pty)]
fn s5_release_p0_1_anthropic_full_turn_via_real_path() {
    let sentinel = "[MOCK-S5-P0-1-ANTHROPIC]";
    let script = format!(
        r#"{{"chunks":[{{"delta":"Hello "}},{{"delta":"from "}},{{"delta":"anthropic "}},{{"delta":"flavor "}},{{"delta":"{sentinel}"}},{{"is_final":true}}]}}"#
    );
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_SCRIPT", script.as_str()),
            ("OPENPRX_MOCK_PROVIDER_FLAVOR", "anthropic"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "the anthropic flavor must emit the sentinel. captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "/exit must exit cleanly on the anthropic flavor"
    );
}

/// S5 P0-1: an openai-flavor turn containing a tool_call — the driver must run the tool
/// and then continue the text.
#[test]
#[serial(prx_chat_pty)]
fn s5_release_p0_1_openai_tool_call_turn_via_real_path() {
    let sentinel = "[MOCK-S5-P0-1-OPENAI-DONE]";
    let script = format!(
        r#"{{"chunks":[{{"tool":{{"id":"t1","name":"memory_recall","args":"{{\"query\":\"x\",\"limit\":1}}"}}}},{{"delta":"{sentinel}"}},{{"is_final":true}}]}}"#
    );
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_SCRIPT", script.as_str()),
            ("OPENPRX_MOCK_PROVIDER_FLAVOR", "openai"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "the openai-flavor tool turn must eventually emit the sentinel. captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "/exit must exit cleanly on the openai flavor"
    );
}

/// b2 core evidence 1, through the real binary: a single model tool call fans
/// out into three sub-agents, and all three land in the session registry.
///
/// `OPENPRX_MOCK_TOOL_CALL` (rather than `OPENPRX_MOCK_SCRIPT`) is what makes
/// this safe to run: the mock emits the tool call only on the *first* stream
/// call in the process, so the three sub-agents — which share the same provider
/// instance and therefore the same counter — get plain text and cannot fan out
/// again.
#[test]
#[serial(prx_chat_pty)]
fn b2_sessions_spawn_batch_fans_out_three_subtasks_via_real_path() {
    let sentinel = "[MOCK-B2-FANOUT-DONE]";
    let tool_call =
        r#"sessions_spawn:{"action":"spawn_batch","tasks":["b2 fan-out one","b2 fan-out two","b2 fan-out three"]}"#;
    let (mut sg, _guard) = spawn_chat(
        &["--plain"],
        &[
            ("OPENPRX_MOCK_TOOL_CALL", tool_call),
            ("OPENPRX_MOCK_RESPONSE", sentinel),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("fan this out\r").expect("send prompt");
    read_until_with_dsr(session, sentinel, TURN_TIMEOUT);

    session.send("/sessions\r").expect("send /sessions");
    let listed = read_until_with_dsr(session, "b2 fan-out three", TURN_TIMEOUT);
    for task in ["b2 fan-out one", "b2 fan-out two", "b2 fan-out three"] {
        assert!(
            listed.contains(task),
            "every member of the fan-out must be registered as its own session ({task}); captured:\n{listed}"
        );
    }

    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "fan-out session should exit cleanly"
    );
}

/// S5 P0-1: gemini-flavor cancel-mid-stream — the partial turn reaches the
/// cancelled terminal outcome and remains usable for a following command.
#[test]
#[serial(prx_chat_pty)]
fn s5_release_p0_1_gemini_cancel_midstream_via_real_path() {
    // 10 chunks * 100ms = 1s total, long enough for Ctrl+C to land mid-stream.
    let script = r#"{"chunks":[{"reasoning":"CANCEL_STREAM_READY"},{"delta":"g2 "},{"delta":"g3 "},{"delta":"g4 "},{"delta":"g5 "},{"delta":"g6 "},{"delta":"g7 "},{"delta":"g8 "},{"delta":"g9 "},{"delta":"g10"},{"is_final":true}]}"#;
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_SCRIPT", script),
            ("OPENPRX_MOCK_PROVIDER_FLAVOR", "gemini"),
            ("OPENPRX_MOCK_DELAY_MS_PER_CHUNK", "100"),
            ("PRX_TUI", "1"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    // Reasoning progress proves the stream started. Final-answer text is
    // buffered until the no-result check accepts it, so it is not a cancel signal.
    read_until_with_dsr(session, "CANCEL_STREAM_READY", TURN_TIMEOUT);
    session
        .send(expectrl::ControlCode::EndOfText)
        .expect("mid-stream Ctrl+C");
    std::thread::sleep(Duration::from_millis(250));

    session.send("/cost\r").expect("send /cost after cancellation");
    let cancelled = read_until_with_dsr(session, "Turns:", TURN_TIMEOUT);
    assert!(
        cancelled.contains("Turns:        0"),
        "the cancelled partial turn must not be committed as a completed user/assistant pair; captured:\n{cancelled}"
    );
    assert!(
        !cancelled.contains("g10"),
        "the provider stream must stop before its final chunk after cancellation; captured:\n{cancelled}"
    );

    session.send("/exit\r").expect("send /exit after cancellation");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "gemini-flavor chat must remain responsive and exit after the cancelled turn"
    );
}

// ── Daemon-assigned work (c2) ────────────────────────────────────────────────
//
// The loop this covers is the one a person actually asks for: another process
// hands `prx chat` a task, chat runs it in its ordinary turn machinery, and the
// answer goes back. Only a real binary against a real socket can show that,
// because every interesting part of it — enrolling before a prompt appears,
// pulling off the input loop, acknowledging on the *next* pull, reporting when
// the turn ends, and withdrawing on exit — is about ordering between a process
// and a peer, not about a function's return value.

/// Minimal HTTP/1.1 stand-in for the daemon's chat-session mailbox.
///
/// It speaks exactly the five calls chat makes and records what arrived, which
/// is what lets the test assert on ordering (`ack` on the second pull, never the
/// first) rather than merely on the happy path.
struct FakeDaemon {
    base_url: String,
    state: std::sync::Arc<std::sync::Mutex<FakeDaemonState>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(server) = self.server.take() {
            let failed = server.join().is_err();
            if !std::thread::panicking() {
                assert!(!failed, "fake daemon thread failed before cleanup");
            }
        }
    }
}

#[derive(Default)]
struct FakeDaemonState {
    registered_label: Option<String>,
    /// How many times a chat registered. A daemon restart is not a new process
    /// here — it is this counter going up a second time.
    registrations: usize,
    /// The session id handed out by the most recent registration.
    session_id: String,
    /// One assignment, handed out on the first pull and never again.
    pending: Option<(String, String, String)>,
    /// Set to model a restart: the registry is process memory, so a restarted
    /// daemon refuses every pull from a session registered before it.
    forgot_registrations: bool,
    /// The `ack` array of every pull, in order.
    pull_acks: Vec<Vec<String>>,
    /// Every reported result: (assignment id, status, summary).
    results: Vec<(String, String, String)>,
    deregistered: bool,
}

impl FakeDaemon {
    /// Start a daemon that will hand out one assignment with `disposition`.
    fn start(assignment_id: &str, task: &str, disposition: &str) -> Self {
        Self::with_pending(Some((
            assignment_id.to_string(),
            task.to_string(),
            disposition.to_string(),
        )))
    }

    /// Start a daemon holding no work, for tests that queue it later.
    fn start_idle() -> Self {
        Self::with_pending(None)
    }

    fn with_pending(pending: Option<(String, String, String)>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
        let addr = listener.local_addr().expect("fake daemon addr");
        let state = std::sync::Arc::new(std::sync::Mutex::new(FakeDaemonState {
            pending,
            ..FakeDaemonState::default()
        }));
        let shared_state = std::sync::Arc::clone(&state);
        listener.set_nonblocking(true).expect("nonblocking fake daemon");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = std::sync::Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            while !server_stop.load(std::sync::atomic::Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).expect("blocking fake daemon connection");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .expect("daemon read timeout");
                        stream
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .expect("daemon write timeout");
                        serve_one(stream, &shared_state, &server_stop);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fake daemon accept failed: {error}"),
                }
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{}", addr.port()),
            state,
            stop,
            server: Some(server),
        }
    }

    /// Restart the daemon, as far as a chat session can tell: every
    /// registration is forgotten, and a task is waiting for whoever registers
    /// next. Nothing about this is a reconnect the *daemon* performs — it
    /// cannot, which is the whole point.
    fn restart_holding(&self, assignment_id: &str, task: &str) {
        let mut state = self.state.lock().expect("fake daemon state");
        state.forgot_registrations = true;
        state.pending = Some((assignment_id.to_string(), task.to_string(), "queue".to_string()));
    }

    fn snapshot<T>(&self, read: impl FnOnce(&FakeDaemonState) -> T) -> T {
        let guard = self.state.lock().expect("fake daemon state");
        read(&guard)
    }

    /// Wait until `ready` is satisfied, or fail with what the daemon did see.
    fn wait_for(&self, what: &str, total: Duration, ready: impl Fn(&FakeDaemonState) -> bool) {
        let deadline = Instant::now() + total;
        while Instant::now() < deadline {
            if self.snapshot(&ready) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let dump = self.snapshot(|state| {
            format!(
                "registered={:?} pulls={:?} results={:?} deregistered={}",
                state.registered_label, state.pull_acks, state.results, state.deregistered
            )
        });
        panic!("timed out waiting for {what}; fake daemon saw: {dump}");
    }
}

/// Read one request, answer it, close. Keep-alive is deliberately not offered:
/// one exchange per connection keeps this parser trivial and correct.
fn serve_one(
    mut stream: std::net::TcpStream,
    state: &std::sync::Mutex<FakeDaemonState>,
    stop: &std::sync::atomic::AtomicBool,
) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if let Some(at) = window(&raw, b"\r\n\r\n") {
                    break at + 4;
                }
            }
            Err(_) => return,
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while raw.len() < head_end + content_length {
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let body = String::from_utf8_lossy(&raw[head_end..]).to_string();
    let request_line = head.lines().next().unwrap_or_default().to_string();
    let (status, body) = answer(&request_line, &body, state);
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn answer(request_line: &str, body: &str, state: &std::sync::Mutex<FakeDaemonState>) -> (u16, String) {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let mut state = state.lock().expect("fake daemon state");
    if method == "POST" && path == "/api/chat-sessions/register" {
        state.registered_label = Some(json_string(body, "label"));
        state.registrations += 1;
        state.forgot_registrations = false;
        let id = format!("sess-{}", state.registrations);
        state.session_id.clone_from(&id);
        return (
            200,
            format!(
                r#"{{"session_id":"{id}","session_ref":"9b2e0f7c","token":"tok-1","label":"workspace","registered_at_unix_ms":1}}"#
            ),
        );
    }
    // A restarted daemon knows nothing about a session registered before it,
    // and says so in the shape the real one does: `404`, with the registry's
    // own machine tag next to the sentence.
    if state.forgot_registrations && path.starts_with("/api/chat-sessions/") {
        return (
            404,
            r#"{"error":"no chat session with that id is registered; `GET /api/chat-sessions` lists the live ones","code":"unknown_session"}"#
                .to_string(),
        );
    }
    if method == "POST" && path.ends_with("/inbox/pull") {
        state.pull_acks.push(json_string_array(body, "ack"));
        let session_id = state.session_id.clone();
        return match state.pending.take() {
            Some((id, task, disposition)) => (
                200,
                format!(
                    r#"{{"assignments":[{{"assignment_id":"{id}","session_id":"{session_id}","task":"{task}","disposition":"{disposition}","deliveries":1,"created_at_unix_ms":1,"work_id":"w1","origin_channel":"wacli","origin_ref":"3d1a4f5b"}}],"acked":0,"requeued":0,"queued_remaining":0}}"#
                ),
            ),
            None => (
                200,
                r#"{"assignments":[],"acked":0,"requeued":0,"queued_remaining":0}"#.to_string(),
            ),
        };
    }
    if method == "POST" && path.ends_with("/result") {
        let id = json_string(body, "assignment_id");
        let status = json_string(body, "status");
        let summary = json_string(body, "summary");
        state.results.push((id.clone(), status.clone(), summary));
        return (
            200,
            format!(r#"{{"assignment_id":"{id}","status":"{status}","seq":1}}"#),
        );
    }
    if method == "DELETE" && path.starts_with("/api/chat-sessions/") {
        state.deregistered = true;
        let id = state.session_id.clone();
        return (200, format!(r#"{{"session_id":"{id}","discarded":0}}"#));
    }
    (200, r#"{"error":"unexpected request"}"#.to_string())
}

/// Pull one JSON string field out of a flat body. Enough for the four keys this
/// test reads, and it unescapes the two sequences chat can actually emit.
fn json_string(body: &str, key: &str) -> String {
    let needle = format!("\"{key}\":\"");
    let Some(start) = body.find(&needle) else {
        return String::new();
    };
    let rest = &body[start + needle.len()..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}

fn json_string_array(body: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\":[");
    let Some(start) = body.find(&needle) else {
        return Vec::new();
    };
    let rest = &body[start + needle.len()..];
    let Some(end) = rest.find(']') else {
        return Vec::new();
    };
    rest[..end]
        .split(',')
        .map(|item| item.trim().trim_matches('"').to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Point a harness at a daemon by writing the config chat reads at startup.
fn write_daemon_config(guard: &HarnessGuard, base_url: &str) {
    // The five top-level keys have no serde default, so a config file that omits
    // them fails to deserialize before anything in it is read.
    let config = format!(
        "api_key = \"test-key\"\n\
         api_url = \"https://example.invalid/v1\"\n\
         default_provider = \"mock\"\n\
         default_model = \"mock\"\n\
         default_temperature = 0.0\n\
         \n[chat.daemon]\nurl = \"{base_url}\"\ntoken = \"operator-token\"\n\
         \n[gateway]\nrequire_pairing = false\n"
    );
    std::fs::write(guard.config_dir.join("config.toml"), config).expect("write chat config");
}

/// The whole loop: assigned in, executed here, answered back, withdrawn on exit.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_runs_daemon_assigned_work_and_reports_the_answer_back() {
    let sentinel = "ASSIGNED_REPLY_SENTINEL";
    let daemon = FakeDaemon::start("assign-1", "count the crates", "queue");
    let guard = new_harness_guard().expect("harness");
    write_daemon_config(&guard, &daemon.base_url);
    let mut sg = spawn_chat_in(&guard, &[], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let session = sg.session();
    // Both captures are kept: enrolment and delivery race the banner, so a
    // notice can land in either one and asserting on only the later capture
    // would make this test flaky about the very thing it is checking.
    let mut transcript = read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);

    // Enrolment happens off the startup path, but it does happen.
    daemon.wait_for("registration", STARTUP_TIMEOUT, |state| {
        state.registered_label.is_some()
    });
    assert_eq!(
        daemon.snapshot(|state| state.registered_label.clone()),
        Some("workspace".to_string()),
        "chat registers under its workspace name, which is what an assign_allow rule matches"
    );

    // The assignment arrives without anybody typing, is announced, and runs.
    transcript.push_str(&read_until_with_dsr(session, sentinel, TURN_TIMEOUT));
    assert!(
        transcript.contains("Assigned by wacli"),
        "assigned work must never be silent; captured:\n{transcript}"
    );
    assert!(
        transcript.contains("count the crates"),
        "the announcement must say what was assigned; captured:\n{transcript}"
    );

    // The answer goes back, and it is the reply the turn produced.
    daemon.wait_for("the result", TURN_TIMEOUT, |state| !state.results.is_empty());
    let (id, status, summary) = daemon.snapshot(|state| state.results[0].clone());
    assert_eq!(id, "assign-1");
    assert_eq!(status, "completed");
    assert!(
        summary.contains(sentinel),
        "the reported summary must be the reply this chat produced, got: {summary}"
    );

    // Acknowledgement rides the *next* pull, never the one that delivered it:
    // a client that acknowledged on receipt and then died would destroy the work.
    daemon.wait_for("the acknowledging pull", TURN_TIMEOUT, |state| {
        state.pull_acks.iter().any(|ack| ack.contains(&"assign-1".to_string()))
    });
    let acks = daemon.snapshot(|state| state.pull_acks.clone());
    assert!(acks.len() >= 2, "expected at least two pulls, saw: {acks:?}");
    assert!(
        acks[0].is_empty(),
        "the first pull cannot acknowledge work it has not received yet: {acks:?}"
    );
    assert!(
        acks.iter().any(|ack| ack.contains(&"assign-1".to_string())),
        "the assignment must be acknowledged by a later pull: {acks:?}"
    );

    // Exiting withdraws the session: nothing on the daemon reaps it on a clock.
    session.send("/exit\r").expect("send /exit");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "chat should exit cleanly");
    daemon.wait_for("deregistration", TURN_TIMEOUT, |state| state.deregistered);
}

/// A chat that no daemon will accept still starts, and says why it cannot be
/// assigned work rather than failing or going quiet about it.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_starts_normally_when_it_cannot_enrol_with_a_daemon() {
    let sentinel = "STILL_WORKS_SENTINEL";
    // A port nothing is listening on: enrolment cannot succeed.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = closed.local_addr().expect("addr").port();
    drop(closed);

    let guard = new_harness_guard().expect("harness");
    write_daemon_config(&guard, &format!("http://127.0.0.1:{port}"));
    let mut sg = spawn_chat_in(&guard, &[], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let session = sg.session();
    let mut transcript = read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);

    // The chat is fully usable: a failed enrolment costs one capability.
    session.send("hello\r").expect("send prompt");
    transcript.push_str(&read_until_with_dsr(session, sentinel, TURN_TIMEOUT));
    assert!(
        transcript.contains("cannot be assigned work"),
        "a failed enrolment must be stated, not swallowed; captured:\n{transcript}"
    );

    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "a chat that could not enrol must still exit cleanly"
    );
}

/// A daemon restart must not cost the operator a chat restart.
///
/// The daemon keeps its chat-session registry in memory, so every upgrade,
/// config change or crash recovery leaves a live `prx chat` holding a session
/// id nothing answers to. Before this, the chat said so once and then stayed
/// unassignable until somebody restarted it by hand — which, since daemon
/// restarts are routine, meant assignment quietly stopped working most of the
/// time. Here the restart happens under a chat that nobody touches, and the
/// chat has to come back on its own and run work handed to the *new*
/// registration.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_re_enrols_by_itself_after_the_daemon_forgets_its_session() {
    let sentinel = "RECONNECTED_REPLY_SENTINEL";
    let daemon = FakeDaemon::start_idle();
    let guard = new_harness_guard().expect("harness");
    write_daemon_config(&guard, &daemon.base_url);
    let mut sg = spawn_chat_in(&guard, &[], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let session = sg.session();
    let mut transcript = read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);

    daemon.wait_for("the first registration", STARTUP_TIMEOUT, |state| {
        state.registrations == 1
    });
    // Wait until this chat is actually polling, so the restart lands on a
    // session the daemon really had rather than on one still being set up.
    daemon.wait_for("the first pull", TURN_TIMEOUT, |state| !state.pull_acks.is_empty());

    // The daemon restarts, and has work waiting for whoever registers next.
    // Nobody touches the chat from here on.
    daemon.restart_holding("assign-after-restart", "count the crates");

    daemon.wait_for("the chat to register again", TURN_TIMEOUT, |state| {
        state.registrations == 2
    });

    // Being registered again is not the claim; being able to take work is.
    transcript.push_str(&read_until_with_dsr(session, sentinel, TURN_TIMEOUT));
    assert!(
        transcript.contains("registered again"),
        "a chat that re-enrolled must say so; captured:\n{transcript}"
    );
    assert!(
        transcript.contains("count the crates"),
        "the assignment handed to the new registration must arrive; captured:\n{transcript}"
    );
    daemon.wait_for("the result", TURN_TIMEOUT, |state| !state.results.is_empty());
    let (id, status, summary) = daemon.snapshot(|state| state.results[0].clone());
    assert_eq!(id, "assign-after-restart");
    assert_eq!(status, "completed");
    assert!(
        summary.contains(sentinel),
        "the reply must come from the turn this chat ran, got: {summary}"
    );

    session.send("/exit\r").expect("send /exit");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "chat should exit cleanly");
}

/// A fabricated completion must not leak through either terminal delivery path.
/// The fixture supplies a structured execution-required verdict and repeats the
/// same final draft; the production loop must exhaust one correction and fail.
#[test]
#[serial(prx_chat_pty)]
fn unexecuted_completion_is_not_delivered_in_plain_or_fullscreen_chat() {
    for tui in ["0", "1"] {
        let sentinel = "UNEXECUTED_COMPLETION_MUST_NOT_APPEAR";
        let (mut sg, _guard) = spawn_chat(
            &[],
            &[
                ("PRX_TUI", tui),
                ("OPENPRX_MOCK_RESPONSE", sentinel),
                (
                    "OPENPRX_MOCK_EXECUTION_ASSESSMENT",
                    r#"{"requirement":"execution_required"}"#,
                ),
            ],
        );
        let session = sg.session();
        read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
        drain_with_dsr(session, Duration::from_millis(200));
        session
            .send("Use shell to run exactly printf ACTUAL_EXECUTION_REQUIRED and report its output.\r")
            .expect("send actual execution request");
        let captured = read_until_with_dsr(session, "unverified", TURN_TIMEOUT);
        assert!(
            !captured.contains(sentinel),
            "rejected draft reached the terminal: {captured}"
        );
        session.send("/exit\r").expect("send /exit");
        assert!(wait_for_exit(session, EXIT_TIMEOUT), "rejected turn must exit cleanly");
    }
}
