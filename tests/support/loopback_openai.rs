//! A loopback OpenAI-compatible provider speaking real HTTP/SSE bytes.
//!
//! The listener binds `127.0.0.1:0`, answers `GET */models` with one model
//! and every `POST */chat/completions` from a test script. A request with
//! `"stream":true` gets an SSE stream (each scripted text chunk is its own
//! `data:` frame); any other request gets one JSON completion. Every request
//! body is recorded, bounded by `MAX_REQUESTS` and `MAX_BODY_BYTES`.
//! Dropping the server stops the accept loop and joins it (bounded).
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_types,
    clippy::disallowed_methods
)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Most requests recorded; one more fails the request with 503.
pub const MAX_REQUESTS: usize = 64;
/// Largest accepted request body.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Per-connection read deadline.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Accept poll interval of the nonblocking listener.
const ACCEPT_POLL: Duration = Duration::from_millis(5);
/// Bound on joining the accept thread when the server is dropped.
const JOIN_BOUND: Duration = Duration::from_secs(5);
/// Longest a [`Scripted::Stall`] request is held.
pub const STALL_BOUND: Duration = Duration::from_secs(60);
/// Poll interval while a stalled request watches for the client's close.
const STALL_POLL: Duration = Duration::from_millis(20);

/// One scripted provider answer.
#[derive(Clone, Debug)]
pub enum Scripted {
    /// Assistant text, streamed as one SSE frame per chunk.
    Text(Vec<String>),
    /// One native tool call.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// An HTTP error answer with `status` and the JSON error `message`.
    Error { status: u16, message: String },
    /// No answer: the request is held until the client closes the connection
    /// (a cancelled turn), or `STALL_BOUND` passes and it fails with 503.
    Stall,
}

impl Scripted {
    pub fn text(text: &str) -> Self {
        Self::Text(vec![text.to_owned()])
    }

    pub fn tool(id: &str, name: &str, arguments: &serde_json::Value) -> Self {
        Self::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_string(),
        }
    }
}

/// Usage reported on every completion (`prompt`, `completion`); total is the sum.
pub type Usage = Option<(u64, u64)>;

/// The script: called with the 0-based request index and the request JSON.
pub type Script = Box<dyn FnMut(usize, &serde_json::Value) -> Scripted + Send>;

pub struct LoopbackOpenAi {
    pub base_url: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    stalls_closed: Arc<std::sync::atomic::AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LoopbackOpenAi {
    pub fn start(model: &str, usage: Usage, script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test: loopback bind");
        listener.set_nonblocking(true).expect("test: nonblocking loopback");
        let base_url = format!("http://{}/v1", listener.local_addr().expect("test: loopback address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stalls_closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let state = Serve {
            model: model.to_owned(),
            usage,
            script: Mutex::new(script),
            requests: Arc::clone(&requests),
            stalls_closed: Arc::clone(&stalls_closed),
        };
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => state.handle(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
                    Err(_) => std::thread::sleep(ACCEPT_POLL),
                }
            }
        });
        Self {
            base_url,
            requests,
            stalls_closed,
            stop,
            thread: Some(thread),
        }
    }

    /// How many [`Scripted::Stall`] requests the client closed (cancelled).
    pub fn stalls_closed_by_client(&self) -> usize {
        self.stalls_closed.load(Ordering::SeqCst)
    }

    /// `custom:<base_url>`, the provider string for a PRX config.
    pub fn provider(&self) -> String {
        format!("custom:{}", self.base_url)
    }

    /// Every recorded `/chat/completions` request body, in arrival order.
    pub fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().expect("test: request log").clone()
    }
}

impl Drop for LoopbackOpenAi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + JOIN_BOUND;
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(ACCEPT_POLL);
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

struct Serve {
    model: String,
    usage: Usage,
    script: Mutex<Script>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    stalls_closed: Arc<std::sync::atomic::AtomicUsize>,
}

impl Serve {
    fn handle(&self, mut stream: TcpStream) {
        if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(READ_TIMEOUT)).is_err() {
            return;
        }
        let Some((head, body)) = read_request(&mut stream) else {
            return;
        };
        let request_line = head.lines().next().unwrap_or_default().to_owned();
        if request_line.starts_with("GET") && request_line.contains("/models") {
            let body = serde_json::json!({
                "object": "list",
                "data": [{"id": self.model, "object": "model", "created": 1, "owned_by": "prx"}]
            })
            .to_string();
            respond(&mut stream, 200, "application/json", &body);
            return;
        }
        if !request_line.contains("/chat/completions") {
            respond(&mut stream, 404, "application/json", r#"{"error":"not_found"}"#);
            return;
        }
        let Ok(request) = serde_json::from_slice::<serde_json::Value>(&body) else {
            respond(&mut stream, 400, "application/json", r#"{"error":"bad json"}"#);
            return;
        };
        let index = {
            let mut requests = self.requests.lock().expect("test: request log");
            if requests.len() >= MAX_REQUESTS {
                drop(requests);
                respond(&mut stream, 503, "application/json", r#"{"error":"request cap"}"#);
                return;
            }
            requests.push(request.clone());
            requests.len() - 1
        };
        let reply = (self.script.lock().expect("test: script"))(index, &request);
        match &reply {
            Scripted::Error { status, message } => {
                let body = serde_json::json!({
                    "error": {"message": message, "type": "invalid_request_error", "code": null}
                })
                .to_string();
                respond(&mut stream, *status, "application/json", &body);
                return;
            }
            Scripted::Stall => {
                if hold_until_closed(&mut stream) {
                    self.stalls_closed.fetch_add(1, Ordering::SeqCst);
                }
                return;
            }
            Scripted::Text(_) | Scripted::ToolCall { .. } => {}
        }
        if request["stream"] == true {
            respond(&mut stream, 200, "text/event-stream", &self.sse(&reply));
        } else {
            respond(&mut stream, 200, "application/json", &self.json(&reply));
        }
    }

    fn usage_json(&self) -> serde_json::Value {
        self.usage.map_or(serde_json::Value::Null, |(prompt, completion)| {
            serde_json::json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "total_tokens": prompt + completion
            })
        })
    }

    fn sse(&self, reply: &Scripted) -> String {
        let mut frames = Vec::new();
        let finish = match reply {
            Scripted::Text(chunks) => {
                for chunk in chunks {
                    frames.push(serde_json::json!({
                        "choices": [{"index": 0, "delta": {"content": chunk}, "finish_reason": null}]
                    }));
                }
                "stop"
            }
            Scripted::Error { .. } | Scripted::Stall => "stop",
            Scripted::ToolCall { id, name, arguments } => {
                frames.push(serde_json::json!({
                    "choices": [{"index": 0, "delta": {"tool_calls": [{
                        "index": 0, "id": id, "type": "function",
                        "function": {"name": name, "arguments": arguments}
                    }]}, "finish_reason": null}]
                }));
                "tool_calls"
            }
        };
        let mut last = serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]});
        if self.usage.is_some() {
            last["usage"] = self.usage_json();
        }
        frames.push(last);
        let mut out = String::new();
        for frame in frames {
            out.push_str("data: ");
            out.push_str(&frame.to_string());
            out.push_str("\n\n");
        }
        out.push_str("data: [DONE]\n\n");
        out
    }

    fn json(&self, reply: &Scripted) -> String {
        let (message, finish) = match reply {
            Scripted::Text(chunks) => (
                serde_json::json!({"role": "assistant", "content": chunks.concat()}),
                "stop",
            ),
            Scripted::Error { .. } | Scripted::Stall => {
                (serde_json::json!({"role": "assistant", "content": ""}), "stop")
            }
            Scripted::ToolCall { id, name, arguments } => (
                serde_json::json!({"role": "assistant", "content": null, "tool_calls": [{
                    "id": id, "type": "function", "function": {"name": name, "arguments": arguments}
                }]}),
                "tool_calls",
            ),
        };
        let mut body = serde_json::json!({
            "id": "loopback", "object": "chat.completion", "model": self.model,
            "choices": [{"index": 0, "message": message, "finish_reason": finish}]
        });
        if self.usage.is_some() {
            body["usage"] = self.usage_json();
        }
        body.to_string()
    }
}

/// Hold a request unanswered until the client closes the connection
/// (returns `true`), or `STALL_BOUND` passes (answers 503, returns `false`).
fn hold_until_closed(stream: &mut TcpStream) -> bool {
    if stream.set_read_timeout(Some(STALL_POLL)).is_err() {
        return false;
    }
    let started = Instant::now();
    let mut byte = [0_u8; 1];
    while started.elapsed() < STALL_BOUND {
        match stream.read(&mut byte) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return true,
        }
    }
    respond(stream, 503, "application/json", r#"{"error":"stall bound"}"#);
    false
}

fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    let head_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break offset + 4;
        }
        if bytes.len() > MAX_BODY_BYTES {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return None;
    }
    while bytes.len() - head_end < length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = bytes[head_end..head_end + length].to_vec();
    Some((head, body))
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Service Unavailable",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// Write a minimal PRX config for `provider`/`model` with a zero tariff and a
/// native-tools declaration, after letting `edit` adjust it.
pub fn write_config(
    config_dir: &std::path::Path,
    provider: &str,
    model: &str,
    edit: impl FnOnce(&mut openprx::config::Config),
) {
    let mut config = openprx::config::Config {
        default_provider: Some(provider.to_owned()),
        default_model: Some(model.to_owned()),
        api_key: Some("prx-test-credential".to_owned()),
        ..openprx::config::Config::default()
    };
    config.auth.codex_auth_json_auto_import = false;
    "none".clone_into(&mut config.memory.embedding_provider);
    config.reliability.provider_retries = 0;
    config.reliability.fallback_providers.clear();
    config.cost.prices.insert(
        format!("{provider}/{model}"),
        openprx::config::schema::ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    config
        .providers
        .models
        .push(openprx::config::schema::ProviderModelConfig {
            provider: provider.to_owned(),
            model: Some(model.to_owned()),
            operations: Some(vec![openprx::config::schema::ModelOperationSetting::Chat]),
            native_tools: Some(true),
            vision_input: None,
            streaming: None,
            structured_output: None,
            tool_schema_dialect: None,
            tool_call_dialect: None,
            context_window: None,
        });
    edit(&mut config);
    std::fs::create_dir_all(config_dir).expect("test: config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        toml::to_string_pretty(&config).expect("test: serialize config"),
    )
    .expect("test: write config");
}

/// One persisted message event: type, role and content.
pub struct StoredEvent {
    pub event_type: String,
    pub role: String,
    pub content: String,
}

/// Most message events [`message_events`] reads; one more fails the test.
pub const MAX_EVENT_ROWS: usize = 4096;
/// Most bytes kept from one stored event field; one more fails the test.
pub const MAX_EVENT_FIELD_BYTES: usize = 1024 * 1024;
/// Most bytes [`message_events`] retains in total; more fails the test.
pub const MAX_EVENT_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// Every message event in the workspace's real SQLite store, in insert order.
/// The read is bounded before collection: at most [`MAX_EVENT_ROWS`] rows, each
/// field cut at [`MAX_EVENT_FIELD_BYTES`] + 1 bytes in the query, and
/// [`MAX_EVENT_TOTAL_BYTES`] retained; any overflow fails the test.
pub fn message_events(workspace: &std::path::Path) -> Vec<StoredEvent> {
    let database = rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("test: open real store");
    let field_cap = i64::try_from(MAX_EVENT_FIELD_BYTES + 1).expect("test: field cap fits i64");
    let row_cap = i64::try_from(MAX_EVENT_ROWS + 1).expect("test: row cap fits i64");
    let mut statement = database
        .prepare(
            "SELECT substr(CAST(event_type AS BLOB), 1, ?1), substr(CAST(role AS BLOB), 1, ?1),
                    substr(CAST(content AS BLOB), 1, ?1)
             FROM message_events ORDER BY id LIMIT ?2",
        )
        .expect("test: event query");
    let mut rows = statement
        .query(rusqlite::params![field_cap, row_cap])
        .expect("test: events");
    let mut events = Vec::new();
    let mut retained = 0_usize;
    while let Some(row) = rows.next().expect("test: event row") {
        assert!(
            events.len() < MAX_EVENT_ROWS,
            "more than {MAX_EVENT_ROWS} message events"
        );
        let mut field = |index: usize| -> String {
            let bytes: Vec<u8> = row.get(index).expect("test: event field");
            assert!(
                bytes.len() <= MAX_EVENT_FIELD_BYTES,
                "a message event field exceeds {MAX_EVENT_FIELD_BYTES} bytes"
            );
            retained += bytes.len();
            assert!(
                retained <= MAX_EVENT_TOTAL_BYTES,
                "message events exceed {MAX_EVENT_TOTAL_BYTES} bytes"
            );
            String::from_utf8(bytes).expect("test: UTF-8 event field")
        };
        let event_type = field(0);
        let role = field(1);
        let content = field(2);
        events.push(StoredEvent {
            event_type,
            role,
            content,
        });
    }
    events
}

/// Content of every `tool.approval.resolved` row, in insert order, e.g.
/// `operation_id=<id> capability=<tool> verdict=denied cause=user_rejected`.
pub fn approval_rows(workspace: &std::path::Path) -> Vec<String> {
    message_events(workspace)
        .into_iter()
        .filter(|event| event.event_type == "tool.approval.resolved")
        .map(|event| event.content)
        .collect()
}

/// Content of every `tool.execution.reserved` row, in insert order, e.g.
/// `operation_id=<id> capability=<tool> attempt=0`.
pub fn reserved_rows(workspace: &std::path::Path) -> Vec<String> {
    message_events(workspace)
        .into_iter()
        .filter(|event| event.event_type == "tool.execution.reserved")
        .map(|event| event.content)
        .collect()
}

/// Index of the first event of `event_type` whose content starts with
/// `prefix`, in insert order.
pub fn event_position(workspace: &std::path::Path, event_type: &str, prefix: &str) -> Option<usize> {
    message_events(workspace)
        .iter()
        .position(|event| event.event_type == event_type && event.content.starts_with(prefix))
}

/// The content of the `role:"tool"` message answering `tool_call_id` in one
/// recorded provider request, if it holds one.
pub fn tool_result_text(request: &serde_json::Value, tool_call_id: &str) -> Option<String> {
    request["messages"].as_array()?.iter().find_map(|message| {
        (message["role"] == "tool" && message["tool_call_id"] == tool_call_id)
            .then(|| message["content"].as_str().map(str::to_owned))
            .flatten()
    })
}

/// Contents of the persisted assistant messages, oldest first.
pub fn assistant_messages(workspace: &std::path::Path) -> Vec<String> {
    message_events(workspace)
        .into_iter()
        .filter(|event| event.role == "assistant" && event.event_type == "message.created")
        .map(|event| event.content)
        .collect()
}
