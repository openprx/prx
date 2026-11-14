//! Loopback scripted chat-completions provider for real `prx` parents and
//! their process workers.
//!
//! Requests are routed by a fixture marker found in the request's own user
//! input. Every request is captured (bounded), the first request of each
//! worker marker also captures that worker's live, production-written
//! manifest, and one marker may be held until the test releases it. Every
//! bound breach is recorded as a failure the test asserts is absent.

use openprx::session_worker::protocol::WorkerManifest;
use parking_lot::{Condvar, Mutex, MutexGuard};
use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Largest accepted request head (request line and headers).
pub const MAX_REQUEST_HEAD_BYTES: usize = 64 * 1024;
/// Largest accepted request body, checked against `Content-Length` before
/// reading and against the bytes read.
pub const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
/// Read timeout of every accepted socket.
pub const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(3);
/// Most requests one server captures; later requests are answered `503`.
pub const MAX_CAPTURED_REQUESTS: usize = 256;
/// Longest a held request waits for its release.
pub const HOLD_WAIT: Duration = Duration::from_secs(30);
/// Poll interval of the nonblocking accept loop.
pub const ACCEPT_POLL: Duration = Duration::from_millis(5);
/// Longest wait for a worker artefact (here: its live manifest snapshot).
pub const WORKER_DEADLINE: Duration = Duration::from_secs(180);
/// Bounded join of the server thread during cleanup.
const SERVER_JOIN_BOUND: Duration = Duration::from_secs(5);
/// The configured fixture model every scripted reply names.
pub const PROVIDER_MODEL: &str = "probe-model";

#[derive(Clone, Debug)]
pub struct ProviderRequest {
    pub marker: String,
    pub head: String,
    pub body: String,
}

/// A worker manifest as the production parent wrote it: typed, and the raw
/// JSON so key-level absence can be asserted.
#[derive(Clone)]
pub struct CapturedManifest {
    pub manifest: WorkerManifest,
    pub raw: serde_json::Value,
}

/// A scripted answer: an assistant message wrapped in a chat completion (JSON,
/// or SSE for a streaming request), or an exact HTTP status and JSON body.
pub enum Reply {
    Assistant(serde_json::Value),
    Raw { status: u16, body: String },
}

/// One non-assessment request: its marker, its 1-based index among that
/// marker's non-assessment requests, and its body.
pub struct Call<'a> {
    pub marker: &'a str,
    pub index: usize,
    pub body: &'a str,
}

pub type Script = Box<dyn FnMut(&Call<'_>) -> Reply + Send>;

pub struct ProviderSpec {
    /// Markers in classification priority order.
    pub markers: Vec<&'static str>,
    /// Markers of real parent turns; their requests capture no manifest.
    pub parents: Vec<&'static str>,
    /// Markers whose requests are classified `execution_required`.
    pub execution: Vec<&'static str>,
    /// Root of the worker workspaces holding the live manifest snapshots.
    pub manifest_root: PathBuf,
    /// Worker task text for a marker whose spawned task differs from it.
    pub tasks: BTreeMap<&'static str, String>,
    /// A marker whose first request is held until `release_hold`.
    pub hold: Option<&'static str>,
    pub script: Script,
}

#[derive(Default)]
struct HoldState {
    reached: bool,
    released: bool,
}

#[derive(Default)]
struct Hold {
    state: Mutex<HoldState>,
    signal: Condvar,
}

impl Hold {
    fn release(&self) {
        self.state.lock().released = true;
        self.signal.notify_all();
    }
}

#[derive(Default)]
struct Shared {
    requests: Mutex<Vec<ProviderRequest>>,
    manifests: Mutex<BTreeMap<String, CapturedManifest>>,
    failures: Mutex<Vec<String>>,
    hold: Hold,
    stop: AtomicBool,
}

impl Shared {
    fn fail(&self, failure: String) {
        self.failures.lock().push(failure);
    }
}

pub struct OwnedProviderServer {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    destination: String,
}

impl OwnedProviderServer {
    pub fn start(spec: ProviderSpec) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test: loopback bind");
        listener.set_nonblocking(true).expect("test: nonblocking loopback");
        let destination = format!(
            "custom:http://{}/v1",
            listener.local_addr().expect("test: loopback address")
        );
        let shared = Arc::new(Shared::default());
        let server_shared = Arc::clone(&shared);
        let thread = std::thread::spawn(move || serve(&listener, spec, &server_shared));
        Self {
            shared,
            thread: Some(thread),
            destination,
        }
    }

    /// The `custom:<url>` provider name of this server.
    pub fn destination(&self) -> &str {
        &self.destination
    }

    pub fn requests(&self) -> MutexGuard<'_, Vec<ProviderRequest>> {
        self.shared.requests.lock()
    }

    pub fn manifest(&self, marker: &str) -> Option<CapturedManifest> {
        self.shared.manifests.lock().get(marker).cloned()
    }

    /// Wait until the held marker's request arrived, at most `timeout`.
    pub fn wait_for_hold(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.hold.state.lock();
        while !state.reached {
            if self.shared.hold.signal.wait_until(&mut state, deadline).timed_out() {
                return state.reached;
            }
        }
        true
    }

    pub fn release_hold(&self) {
        self.shared.hold.release();
    }

    /// Every bound breach or fixture fault the server recorded.
    pub fn assert_no_failures(&self) {
        let failures = self.shared.failures.lock();
        assert!(failures.is_empty(), "loopback provider failures: {failures:?}");
    }

    /// Stop accepting, release any hold and join the server thread (bounded;
    /// a thread that does not join in time is detached and reported).
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.hold.release();
        if let Some(thread) = self.thread.take() {
            join_bounded(thread, "loopback provider");
        }
    }
}

impl Drop for OwnedProviderServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Join a helper thread within `SERVER_JOIN_BOUND`; otherwise detach it and
/// report, so cleanup itself stays bounded.
pub fn join_bounded<T>(thread: std::thread::JoinHandle<T>, what: &str) -> Option<T> {
    let deadline = Instant::now() + SERVER_JOIN_BOUND;
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            let _ = writeln!(std::io::stderr(), "{what} thread did not join within bound; detached");
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    thread.join().ok()
}

fn serve(listener: &TcpListener, mut spec: ProviderSpec, shared: &Shared) {
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => handle(stream, &mut spec, shared),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
            Err(error) => {
                shared.fail(format!("loopback accept: {error}"));
                return;
            }
        }
    }
}

enum RequestFault {
    /// The peer closed without sending anything: not a request.
    Empty,
    TooLarge(String),
    Dropped(String),
}

fn read_request(stream: &mut TcpStream) -> Result<(String, String), RequestFault> {
    let mut buffer = Vec::with_capacity(8 * 1024);
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(offset) = buffer.windows(4).position(|part| part == b"\r\n\r\n") {
            break offset + 4;
        }
        if buffer.len() > MAX_REQUEST_HEAD_BYTES {
            return Err(RequestFault::TooLarge(
                "request head exceeds MAX_REQUEST_HEAD_BYTES".into(),
            ));
        }
        let read = stream
            .read(&mut chunk)
            .map_err(|error| RequestFault::Dropped(format!("request head read: {error}")))?;
        if read == 0 {
            return Err(if buffer.is_empty() {
                RequestFault::Empty
            } else {
                RequestFault::Dropped("connection closed inside the request head".into())
            });
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    if head_end > MAX_REQUEST_HEAD_BYTES {
        return Err(RequestFault::TooLarge(
            "request head exceeds MAX_REQUEST_HEAD_BYTES".into(),
        ));
    }
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    if content_length > MAX_REQUEST_BODY_BYTES {
        return Err(RequestFault::TooLarge(format!(
            "Content-Length {content_length} exceeds MAX_REQUEST_BODY_BYTES"
        )));
    }
    let mut body = buffer.split_off(head_end);
    if body.len() > content_length {
        return Err(RequestFault::TooLarge("request body exceeds its Content-Length".into()));
    }
    let remaining = u64::try_from(content_length - body.len()).unwrap_or(u64::MAX);
    std::io::Read::by_ref(stream)
        .take(remaining)
        .read_to_end(&mut body)
        .map_err(|error| RequestFault::Dropped(format!("request body read: {error}")))?;
    if body.len() != content_length {
        return Err(RequestFault::Dropped(
            "connection closed inside the request body".into(),
        ));
    }
    Ok((head, String::from_utf8_lossy(&body).into_owned()))
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // A peer that already exited (a killed parent) cannot read the reply; that
    // is not a harness bound breach.
    let _ = stream.write_all(response.as_bytes());
}

fn handle(mut stream: TcpStream, spec: &mut ProviderSpec, shared: &Shared) {
    if let Err(error) = stream
        .set_nonblocking(false)
        .and_then(|()| stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT)))
    {
        shared.fail(format!("loopback socket setup: {error}"));
        return;
    }
    let (head, body) = match read_request(&mut stream) {
        Ok(request) => request,
        Err(RequestFault::Empty) => return,
        Err(RequestFault::TooLarge(reason)) => {
            respond(&mut stream, 413, "text/plain", "too large");
            shared.fail(reason);
            return;
        }
        Err(RequestFault::Dropped(reason)) => {
            shared.fail(reason);
            return;
        }
    };
    let own_input = request_own_input(&body);
    let Some(marker) = spec.markers.iter().copied().find(|marker| own_input.contains(marker)) else {
        respond(&mut stream, 500, "text/plain", "no fixture marker");
        shared.fail(format!("fixture request marker missing in own input: {own_input:?}"));
        return;
    };
    if !spec.parents.contains(&marker) && !shared.manifests.lock().contains_key(marker) {
        let task = spec.tasks.get(marker).cloned().unwrap_or_else(|| marker.to_owned());
        match capture_live_worker_manifest(&spec.manifest_root, &task, &shared.stop) {
            Some(captured) => {
                shared.manifests.lock().insert(marker.to_owned(), captured);
            }
            None => shared.fail(format!("live production manifest for {task} was not found")),
        }
    }
    let index = {
        let mut requests = shared.requests.lock();
        if requests.len() >= MAX_CAPTURED_REQUESTS {
            drop(requests);
            respond(&mut stream, 503, "text/plain", "request cap");
            shared.fail("MAX_CAPTURED_REQUESTS exceeded".into());
            return;
        }
        requests.push(ProviderRequest {
            marker: marker.to_owned(),
            head,
            body: body.clone(),
        });
        requests
            .iter()
            .filter(|request| request.marker == marker && !is_assessment_body(&request.body))
            .count()
    };
    if spec.hold == Some(marker) && !hold_released(shared) {
        respond(&mut stream, 503, "text/plain", "hold expired");
        shared.fail(format!("HOLD_WAIT expired for {marker}"));
        return;
    }
    let reply = if body.contains("Review the proposed final reply against the CURRENT request") {
        review_reply(&body, shared)
    } else if body.contains("Classify the execution requirement of current_request") {
        let requirement = if spec.execution.contains(&marker) {
            "execution_required"
        } else {
            "informational"
        };
        Reply::Assistant(serde_json::json!({
            "role": "assistant",
            "content": serde_json::json!({"requirement": requirement}).to_string()
        }))
    } else {
        (spec.script)(&Call {
            marker,
            index,
            body: &body,
        })
    };
    write_reply(&mut stream, &reply, &body);
}

/// Hold the first request of the held marker: signal that it arrived, then
/// wait for the release at most `HOLD_WAIT`. Later requests pass through.
fn hold_released(shared: &Shared) -> bool {
    let mut state = shared.hold.state.lock();
    if state.reached {
        return true;
    }
    state.reached = true;
    shared.hold.signal.notify_all();
    let deadline = Instant::now() + HOLD_WAIT;
    while !state.released {
        if shared.hold.signal.wait_until(&mut state, deadline).timed_out() {
            state.released = true;
            return false;
        }
    }
    true
}

fn review_reply(body: &str, shared: &Shared) -> Reply {
    let tools_empty = serde_json::from_str::<serde_json::Value>(body)
        .is_ok_and(|request| request["tools"].as_array().is_none_or(Vec::is_empty));
    if !tools_empty || !(body.contains("observed_tools") && body.contains("proposed_final_reply")) {
        shared.fail("final-reply review request changed shape".into());
    }
    Reply::Assistant(serde_json::json!({
        "role": "assistant",
        "content": r#"{"decision":"accept","reason":"fixture evidence accepted","constraints":[]}"#
    }))
}

fn write_reply(stream: &mut TcpStream, reply: &Reply, request_body: &str) {
    let assistant = match reply {
        Reply::Raw { status, body } => {
            respond(stream, *status, "application/json", body);
            return;
        }
        Reply::Assistant(assistant) => assistant,
    };
    if request_body.contains("\"stream\":true") {
        let (delta, finish_reason) = assistant.get("tool_calls").map_or_else(
            || (serde_json::json!({"content": assistant["content"]}), "stop"),
            |calls| {
                let mut calls = calls.as_array().cloned().unwrap_or_default();
                for (index, call) in calls.iter_mut().enumerate() {
                    call["index"] = serde_json::json!(index);
                }
                (serde_json::json!({"tool_calls": calls}), "tool_calls")
            },
        );
        let chunk = serde_json::json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]});
        let finish = serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]});
        respond(
            stream,
            200,
            "text/event-stream",
            &format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        );
    } else {
        respond(stream, 200, "application/json", &completion_body(assistant));
    }
}

/// A non-streaming chat completion carrying `assistant`.
pub fn completion_body(assistant: &serde_json::Value) -> String {
    serde_json::json!({
        "id": "chatcmpl-prx-worker",
        "object": "chat.completion",
        "created": 1,
        "model": PROVIDER_MODEL,
        "choices": [{"index": 0, "message": assistant, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
    .to_string()
}

/// A plain assistant text reply.
pub fn text_reply(text: &str) -> Reply {
    Reply::Assistant(serde_json::json!({"role": "assistant", "content": text}))
}

/// Find the live manifest snapshot the production parent wrote for `task`
/// under `<root>/<run>/.prx-worker-*/manifest.json`, within `WORKER_DEADLINE`.
fn capture_live_worker_manifest(root: &Path, task: &str, stop: &AtomicBool) -> Option<CapturedManifest> {
    let deadline = Instant::now() + WORKER_DEADLINE;
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        if let Some(found) = find_manifest(root, task) {
            return Some(found);
        }
        std::thread::sleep(ACCEPT_POLL);
    }
    None
}

fn find_manifest(root: &Path, task: &str) -> Option<CapturedManifest> {
    for run in std::fs::read_dir(root).ok()?.flatten() {
        if !run.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(run.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with(".prx-worker-") {
                continue;
            }
            let Ok(file) = std::fs::File::open(entry.path().join("manifest.json")) else {
                continue;
            };
            let mut bytes = Vec::new();
            let limit = u64::try_from(MAX_REQUEST_BODY_BYTES * 16).unwrap_or(u64::MAX);
            if file.take(limit).read_to_end(&mut bytes).is_err() {
                continue;
            }
            let Ok(raw) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_value::<WorkerManifest>(raw.clone()) else {
                continue;
            };
            if manifest.task == task {
                return Some(CapturedManifest { manifest, raw });
            }
        }
    }
    None
}

/// Recall preamble headers the runtime prepends to a turn's input
/// (`src/agent/context_recall.rs`). They quote other sessions' events, so a
/// marker inside them belongs to another request.
const RECALL_PREAMBLE_HEADERS: [&str; 4] = [
    "[Recent shared workspace events]\n",
    "[Current session context]\n",
    "[Memory context]\n",
    "[Document evidence]\n",
];

/// Drops leading recall preamble blocks (header through the blank line that
/// ends the block), leaving the request's own input.
fn strip_recall_preambles(text: &str) -> &str {
    let mut rest = text.trim_start();
    while let Some(header) = RECALL_PREAMBLE_HEADERS.iter().find(|header| rest.starts_with(**header)) {
        rest = match rest[header.len()..].split_once("\n\n") {
            Some((_, tail)) => tail.trim_start(),
            None => "",
        };
    }
    rest
}

/// The text a fixture request is classified on: its user messages (or the
/// assessment's `current_request`), minus recall preambles. Never the whole
/// body, which also carries recalled events quoting other sessions' markers,
/// and never assistant/tool messages, which echo other tasks' text.
pub fn request_own_input(request_body: &str) -> String {
    let Ok(request) = serde_json::from_str::<serde_json::Value>(request_body) else {
        return String::new();
    };
    request["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "user")
                .filter_map(|message| message["content"].as_str())
                .map(|content| {
                    let current = serde_json::from_str::<serde_json::Value>(content)
                        .ok()
                        .and_then(|value| value["current_request"].as_str().map(str::to_string))
                        .unwrap_or_else(|| content.to_string());
                    strip_recall_preambles(&current).to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn is_assessment_body(body: &str) -> bool {
    body.contains("Classify the execution requirement of current_request")
        || body.contains("Review the proposed final reply against the CURRENT request")
}

pub fn is_assessment_request(request: &ProviderRequest) -> bool {
    is_assessment_body(&request.body)
}

/// The last tool result of a request, parsed as JSON.
pub fn last_tool_summary(request: &ProviderRequest) -> Option<serde_json::Value> {
    let body: serde_json::Value = serde_json::from_str(&request.body).ok()?;
    body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .and_then(|content| serde_json::from_str::<serde_json::Value>(content).ok())
}

/// The parent's post-join request: the first request of `marker` whose last
/// tool result is a join summary.
pub fn post_join_request<'a>(requests: &'a [ProviderRequest], marker: &str) -> &'a ProviderRequest {
    requests
        .iter()
        .filter(|request| request.marker == marker && !is_assessment_request(request))
        .find(|request| last_tool_summary(request).is_some_and(|summary| summary.get("total").is_some()))
        .expect("test: parent post-join request carrying a join summary")
}

pub fn assert_parent_join(requests: &[ProviderRequest], marker: &str, expected_total: usize) {
    assert_join_summary(post_join_request(requests, marker), expected_total);
}

pub fn assert_join_summary(post_join: &ProviderRequest, expected_total: usize) {
    let summary = last_tool_summary(post_join).expect("test: parent join summary");
    assert_eq!(summary["total"], expected_total, "join summary total: {summary}");
    assert_eq!(
        summary["completed"].as_array().map(Vec::len),
        Some(expected_total),
        "{summary}"
    );
    for empty in ["failed", "killed", "no_result"] {
        assert_eq!(summary[empty], serde_json::json!([]), "join summary {empty}: {summary}");
    }
}

pub fn scripted_spawn_call(arguments: &serde_json::Value) -> Reply {
    static NEXT_CALL_ID: AtomicUsize = AtomicUsize::new(1);
    let call_id = format!("call-prx-spawn-{}", NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed));
    Reply::Assistant(serde_json::json!({
        "role": "assistant", "content": null,
        "tool_calls": [{
            "id": call_id, "type": "function",
            "function": {"name": "sessions_spawn", "arguments": arguments.to_string()}
        }]
    }))
}

/// The batch id of the most recent `spawn_batch` result in a request.
pub fn batch_id_in(request_body: &str) -> Option<String> {
    let request: serde_json::Value = serde_json::from_str(request_body).ok()?;
    request["messages"]
        .as_array()?
        .iter()
        .rev()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .filter_map(|content| serde_json::from_str::<serde_json::Value>(content).ok())
        .find_map(|result| result["batch_id"].as_str().map(str::to_owned))
}

/// Join the most recent batch; without one, answer text that fails the test's
/// join assertion instead of panicking inside the server thread.
pub fn scripted_join_call(request_body: &str) -> Reply {
    batch_id_in(request_body).map_or_else(
        || text_reply("PRX-FIXTURE-NO-BATCH-RESULT"),
        |batch_id| scripted_spawn_call(&serde_json::json!({"action": "join", "batch_id": batch_id})),
    )
}

/// Zero-price pricing and a chat model entry for a fixture model.
pub fn add_provider_fixture_model(config: &mut openprx::config::Config, provider: &str, model: &str) {
    config.cost.prices.insert(
        format!("{provider}/{model}"),
        openprx::config::schema::ModelPricing {
            input: 0.0,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    if !config
        .providers
        .models
        .iter()
        .any(|entry| entry.provider == provider && entry.model.as_deref() == Some(model))
    {
        config
            .providers
            .models
            .push(openprx::config::schema::ProviderModelConfig {
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
            });
    }
}

/// The manifest the production parent wrote carries exactly the ordinary
/// source and no legacy decision keys.
pub fn assert_ordinary_manifest(captured: &CapturedManifest) {
    assert!(
        captured
            .raw
            .get("ordinary_source")
            .is_some_and(|source| !source.is_null()),
        "worker manifest carries an ordinary source"
    );
    for legacy in ["capability_manifest", "child_source"] {
        assert!(captured.raw.get(legacy).is_none(), "worker manifest carries `{legacy}`");
    }
    assert!(
        captured.manifest.operator_source.is_none(),
        "ordinary child has no operator source"
    );
}
