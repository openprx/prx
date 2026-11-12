//! Fixed-model proof through the Python profile and real PRX HTTP transport.
#![cfg(test)]

use serde_json::Value;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

const MODEL: &str = "prx-bench-fixture";
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_OUTPUT: u64 = 1024 * 1024;
const MAX_CONNECTIONS: usize = 16;
const CHILD_DEADLINE: Duration = Duration::from_secs(90);
const SOCKET_DEADLINE: Duration = Duration::from_secs(3);

type Request = (String, Value);

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn drain(mut reader: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (tx, rx) = sync_channel(1);
    thread::spawn(move || {
        let mut output = Vec::new();
        reader
            .by_ref()
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut output)
            .expect("test: bounded child output read");
        let _ = tx.send(output);
    });
    rx
}

fn run(command: &mut Command, deadline: Duration) -> (ExitStatus, String, String) {
    let mut child = ChildGuard(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("test: spawn bounded child"),
    );
    let stdout = drain(child.0.stdout.take().expect("test: stdout pipe"));
    let stderr = drain(child.0.stderr.take().expect("test: stderr pipe"));
    let end = Instant::now() + deadline;
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("test: poll child") {
            break status;
        }
        assert!(Instant::now() < end, "child exceeded its {deadline:?} deadline");
        thread::sleep(Duration::from_millis(10));
    };
    let out = stdout.recv_timeout(SOCKET_DEADLINE).expect("test: stdout completed");
    let err = stderr.recv_timeout(SOCKET_DEADLINE).expect("test: stderr completed");
    assert!(out.len() as u64 <= MAX_OUTPUT, "stdout overflow");
    assert!(err.len() as u64 <= MAX_OUTPUT, "stderr overflow");
    (
        status,
        String::from_utf8(out).expect("test: utf8 stdout"),
        String::from_utf8(err).expect("test: utf8 stderr"),
    )
}

fn read_request(stream: &mut TcpStream) -> Request {
    stream
        .set_read_timeout(Some(SOCKET_DEADLINE))
        .expect("test: socket read deadline");
    stream
        .set_write_timeout(Some(SOCKET_DEADLINE))
        .expect("test: socket write deadline");
    let end = Instant::now() + SOCKET_DEADLINE;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        assert!(head.len() < MAX_HEAD, "HTTP head overflow");
        assert!(Instant::now() < end, "HTTP head deadline exceeded");
        let mut byte = [0];
        stream.read_exact(&mut byte).expect("test: complete HTTP head");
        head.push(*byte.first().expect("test: one-byte buffer"));
    }
    let head = String::from_utf8(head).expect("test: ascii HTTP head");
    let first = head.lines().next().expect("test: request line").to_string();
    let length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| {
            value.trim().parse::<usize>().expect("test: content length")
        });
    assert!(length <= MAX_BODY, "HTTP body overflow: {length}");
    let mut body = Vec::new();
    while body.len() < length {
        let remaining_time = end
            .checked_duration_since(Instant::now())
            .expect("test: HTTP body deadline");
        stream
            .set_read_timeout(Some(remaining_time))
            .expect("test: body read deadline");
        let mut chunk = [0_u8; 8192];
        let remaining = length - body.len();
        let buffer = chunk.get_mut(..remaining.min(8192)).expect("test: bounded chunk");
        let count = stream.read(buffer).expect("test: bounded HTTP body");
        assert!(count > 0, "truncated HTTP body");
        body.extend_from_slice(buffer.get(..count).expect("test: bytes read"));
    }
    assert_eq!(body.len(), length, "truncated HTTP body");
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).expect("test: JSON request body")
    };
    (first, value)
}

struct Server {
    address: String,
    stop: Arc<AtomicBool>,
    requests: Receiver<Vec<Request>>,
}
impl Server {
    fn start(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test: loopback listener");
        let address = listener.local_addr().expect("test: bound address").to_string();
        listener.set_nonblocking(true).expect("test: nonblocking accept");
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let (tx, requests) = sync_channel(1);
        thread::spawn(move || {
            let deadline = Instant::now() + CHILD_DEADLINE + SOCKET_DEADLINE;
            let mut seen = Vec::new();
            while !stopped.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        assert!(seen.len() < MAX_CONNECTIONS, "accepted connection cap exceeded");
                        let request = read_request(&mut stream);
                        let chat = request.0.starts_with("POST ") && request.0.contains("/chat/completions ");
                        let body = if chat && status == 200 {
                            serde_json::json!({"id":"prx-fixture", "object":"chat.completion",
                                "model":MODEL,"choices":[{"index":0,"finish_reason":"stop",
                                "message":{"role":"assistant","content":"4"}}],
                                "usage":{"prompt_tokens":20,"completion_tokens":1,"total_tokens":21,
                                "prompt_tokens_details":{"cached_tokens":0}}})
                        } else {
                            serde_json::json!({"error":{"message":"PRX fixture HTTP 503"}})
                        }
                        .to_string();
                        let code = if chat { status } else { 404 };
                        write!(stream, "HTTP/1.1 {code} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                            .expect("test: write HTTP response");
                        seen.push(request);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("test: accept failed: {error}"),
                }
            }
            let _ = tx.send(seen);
        });
        Self {
            address,
            stop,
            requests,
        }
    }

    fn finish(&self) -> Vec<Request> {
        self.stop.store(true, Ordering::SeqCst);
        self.requests
            .recv_timeout(SOCKET_DEADLINE + Duration::from_secs(1))
            .expect("test: server stopped")
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn bounded_file(path: &Path) -> String {
    let file = std::fs::File::open(path).expect("test: open artifact");
    let mut bytes = Vec::new();
    file.take(MAX_OUTPUT + 1)
        .read_to_end(&mut bytes)
        .expect("test: bounded artifact read");
    assert!(
        bytes.len() as u64 <= MAX_OUTPUT,
        "artifact overflow: {}",
        path.display()
    );
    String::from_utf8(bytes).expect("test: utf8 artifact")
}

fn exercise(status: u16, refusal: bool) {
    let server = Server::start(status);
    let trap = Server::start(200);
    let root = tempfile::tempdir().expect("test: isolated filesystem");
    let config_dir = root.path().join("config");
    std::fs::create_dir(&config_dir).expect("test: config directory");
    let config_path = config_dir.join("config.toml");
    let provider = format!("custom:http://{}/v1", server.address);
    let mut renderer = Command::new("python3");
    renderer
        .args(["scripts/bench/harbor/profile.py", "render", "--out"])
        .arg(&config_path)
        .args([
            "--model",
            &format!("prx/{MODEL}"),
            "--provider",
            &provider,
            "--price-input",
            "1",
            "--price-output",
            "1",
            "--price-cache-read",
            "1",
            "--price-cache-write",
            "1",
        ])
        .env("PRX_BENCH_API_KEY", "prx-fixture-credential");
    let (render_status, out, err) = run(&mut renderer, Duration::from_secs(30));
    assert!(render_status.success(), "profile render failed: {out} {err}");
    let mut config = bounded_file(&config_path);
    if refusal {
        let (prefix, rest) = config.split_once("[cost.prices.").expect("test: price table");
        let (_, declaration) = rest
            .split_once("[[providers.models]]")
            .expect("test: model declaration");
        config = format!("{prefix}[[providers.models]]{declaration}");
    }
    // Runtime break-the-fix: change the already-rendered file, bypassing static check.
    if status == 503 && std::env::var_os("PRX_BENCH_BREAK_FALLBACK").is_some() {
        config = config.replace(
            "model_fallbacks = {}",
            &format!("model_fallbacks = {{\"{MODEL}\" = [\"prx-other-fixture\"]}}"),
        );
        // A fallback must pass the shipped cost/native-tool admission before
        // it can reach transport (providers::fallback_admission_for). These
        // declarations exist only in the deliberately broken fixture.
        let alternate_price_key = format!("{provider}/prx-other-fixture");
        write!(
            config,
            "\n[cost.prices.{alternate_price_key:?}]\ninput = 1.0\noutput = 1.0\ncache_read = 1.0\ncache_write = 1.0\n\n[[providers.models]]\nprovider = {provider:?}\nmodel = \"prx-other-fixture\"\noperations = [\"chat\"]\nnative_tools = true\n",
        )
        .expect("test: append eligible fallback fixture");
    }
    std::fs::write(&config_path, config).expect("test: write fixture mutation");
    if status == 503 && std::env::var_os("PRX_BENCH_BREAK_FALLBACK").is_some() {
        let mut checker = Command::new("python3");
        checker
            .args(["scripts/bench/harbor/profile.py", "check"])
            .arg(&config_path);
        let (validation_exit, out, err) = run(&mut checker, Duration::from_secs(30));
        assert_eq!(
            validation_exit.code(),
            Some(2),
            "static check accepted injected fallback: {out} {err}"
        );
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_prx"));
    command
        .args(["--config-dir"])
        .arg(&config_dir)
        .args(["agent", "-t", "0.25", "-m", "What is 2 + 2?"])
        .env("HOME", root.path())
        .env("RUST_LOG", "error")
        .env_remove("OPENPRX_CONFIG_DIR")
        .env_remove("OPENPRX_WORKSPACE")
        .env_remove("PRX_BENCH_API_KEY");
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(key, format!("http://{}", trap.address));
    }
    command
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    let (exit, stdout, stderr) = run(&mut command, CHILD_DEADLINE);
    let requests = server.finish();
    assert!(trap.finish().is_empty(), "proxy trap received a connection");
    for (path, body) in &requests {
        assert!(
            path.starts_with("POST ") && path.contains("/chat/completions "),
            "unexpected requests: {requests:?}"
        );
        assert_eq!(
            body.get("model").and_then(Value::as_str),
            Some(MODEL),
            "model substitution observed: {requests:?}"
        );
        assert_eq!(
            body.get("temperature").and_then(Value::as_f64),
            Some(0.25),
            "sampling changed: {requests:?}"
        );
    }
    let expected_exit = if refusal { 2 } else { i32::from(status == 503) };
    assert_eq!(
        exit.code(),
        Some(expected_exit),
        "stdout={stdout}\nstderr={stderr}\nrequests={requests:?}"
    );
    assert_eq!(
        requests.len(),
        if refusal {
            0
        } else if status == 503 {
            2
        } else {
            1
        },
        "unexpected request count: {requests:?}; stdout={stdout}; stderr={stderr}"
    );
    if !refusal && status == 200 {
        let ledger = bounded_file(&config_dir.join("workspace/state/costs.jsonl"));
        let rows: Vec<Value> = ledger
            .lines()
            .map(|line| serde_json::from_str(line).expect("test: cost row"))
            .collect();
        assert_eq!(rows.len(), 1, "one settled turn expected: {rows:?}");
        let row = rows.first().expect("test: one settled row");
        assert_eq!(
            row.pointer("/usage/model").and_then(Value::as_str),
            Some(format!("{provider}/{MODEL}").as_str()),
            "ledger model changed"
        );
    }
}

#[test]
fn rendered_profile_sends_one_requested_model_and_temperature() {
    exercise(200, false);
}

#[test]
fn unavailable_provider_retries_only_requested_model() {
    exercise(503, false);
}

#[test]
fn unpriced_profile_refuses_before_any_request() {
    exercise(200, true);
}
