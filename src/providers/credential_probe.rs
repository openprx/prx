//! Test-only loopback endpoint that records what a provider puts on the wire.
//!
//! Credential-containment tests drive the real factories, resolvers and tools
//! against this listener and then assert on the `Authorization` header each
//! request carried. It speaks just enough of the OpenAI-compatible surface for
//! chat completions, `/models` and `/embeddings`, answers every request from a
//! scripted status list (then `200`), and never leaves `127.0.0.1`.
//!
//! Every credential a test hands a provider is a sentinel literal; nothing here
//! reads a real credential, a real config directory or the environment.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;

/// The model id every probe response names.
pub(crate) const PROBE_MODEL: &str = "probe-model";

/// The assistant text every successful chat response carries.
pub(crate) const PROBE_REPLY: &str = "PROBE-LOOPBACK-OK";

/// One request as the probe received it.
#[derive(Debug, Clone)]
pub(crate) struct ProbeRequest {
    pub method: String,
    pub path: String,
    /// Header names lowercased; values verbatim.
    pub headers: Vec<(String, String)>,
    /// Only the policy-denial marker is retained; provider request bodies may
    /// carry sensitive data and must not be kept by this probe.
    pub tool_policy_denial_in_body: bool,
}

impl ProbeRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn authorization(&self) -> Option<&str> {
        self.header("authorization")
    }

    /// Whether any header value on this request contains `needle`.
    pub(crate) fn carries(&self, needle: &str) -> bool {
        self.headers.iter().any(|(_, value)| value.contains(needle))
    }
}

/// A loopback OpenAI-compatible endpoint owned by one test.
pub(crate) struct CredentialProbe {
    addr: SocketAddr,
    recorded: Arc<Mutex<Vec<ProbeRequest>>>,
    server: JoinHandle<()>,
}

impl CredentialProbe {
    /// Answer every request with `200`.
    pub(crate) async fn start() -> Self {
        Self::start_with_statuses(Vec::new()).await
    }

    /// Answer the first requests with `statuses`, in order, then `200`.
    pub(crate) async fn start_with_statuses(statuses: Vec<u16>) -> Self {
        Self::start_scripted(statuses, 200, PROBE_REPLY, None).await
    }

    /// Answer every chat completion with `reply` as the assistant text.
    pub(crate) async fn start_replying(reply: &str) -> Self {
        Self::start_scripted(Vec::new(), 200, reply, None).await
    }

    /// Return one tool-call reply and then a terminal assistant reply.
    pub(crate) async fn start_with_tool_reply(tool_reply: &str) -> Self {
        Self::start_scripted_with_followup(Vec::new(), 200, tool_reply, Some(PROBE_REPLY), None).await
    }

    /// Fail every request with `status` and an error body whose message is
    /// `message`, as an upstream that echoes a credential in its error would.
    pub(crate) async fn start_failing(status: u16, message: &str) -> Self {
        Self::start_scripted(Vec::new(), status, PROBE_REPLY, Some(message)).await
    }

    async fn start_scripted(statuses: Vec<u16>, default_status: u16, reply: &str, error_message: Option<&str>) -> Self {
        Self::start_scripted_with_followup(statuses, default_status, reply, None, error_message).await
    }

    async fn start_scripted_with_followup(
        statuses: Vec<u16>,
        default_status: u16,
        reply: &str,
        followup: Option<&str>,
        error_message: Option<&str>,
    ) -> Self {
        let reply: Arc<str> = Arc::from(reply);
        let followup: Option<Arc<str>> = followup.map(Arc::from);
        let replied = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error_message: Option<Arc<str>> = error_message.map(Arc::from);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind loopback probe");
        let addr = listener.local_addr().expect("test: loopback probe address");
        let recorded: Arc<Mutex<Vec<ProbeRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let script: Arc<Mutex<VecDeque<u16>>> = Arc::new(Mutex::new(statuses.into()));
        let sink = Arc::clone(&recorded);

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _peer)) = listener.accept().await else {
                    return;
                };
                let sink = Arc::clone(&sink);
                let script = Arc::clone(&script);
                let reply = Arc::clone(&reply);
                let followup = followup.clone();
                let replied = Arc::clone(&replied);
                let error_message = error_message.clone();
                tokio::spawn(async move {
                    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
                    let mut chunk = [0_u8; 4096];
                    let head_end = loop {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(chunk.get(..read).unwrap_or_default()),
                        }
                        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                            break position + 4;
                        }
                        if buffer.len() > 1_048_576 {
                            return;
                        }
                    };

                    let head = String::from_utf8_lossy(buffer.get(..head_end).unwrap_or_default()).to_string();
                    let mut lines = head.split("\r\n");
                    let request_line = lines.next().unwrap_or_default();
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let path = parts.next().unwrap_or_default().to_string();
                    let mut headers: Vec<(String, String)> = Vec::new();
                    let mut content_length: usize = 0;
                    for line in lines {
                        if line.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            let name = name.trim().to_ascii_lowercase();
                            let value = value.trim().to_string();
                            if name == "content-length" {
                                content_length = value.parse().unwrap_or(0);
                            }
                            headers.push((name, value));
                        }
                    }

                    let denial_marker = b"not permitted under the current execution policy";
                    let mut marker_tail = Vec::new();
                    let mut tool_policy_denial_in_body = scan_marker(
                        &mut marker_tail,
                        buffer.get(head_end..).unwrap_or_default(),
                        denial_marker,
                    );
                    let mut body_read = buffer.len().saturating_sub(head_end);
                    while body_read < content_length {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => {
                                body_read += read;
                                tool_policy_denial_in_body |=
                                    scan_marker(&mut marker_tail, chunk.get(..read).unwrap_or_default(), denial_marker);
                            }
                        }
                    }

                    let status = script.lock().pop_front().unwrap_or(default_status);
                    let selected_reply = if path.contains("chat/completions")
                        && replied.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        followup.as_deref().unwrap_or(&reply)
                    } else {
                        &reply
                    };
                    let body = response_body(&path, status, selected_reply, error_message.as_deref());
                    sink.lock().push(ProbeRequest {
                        method,
                        path,
                        headers,
                        tool_policy_denial_in_body,
                    });

                    let reason = match status {
                        200 => "OK",
                        401 => "Unauthorized",
                        429 => "Too Many Requests",
                        _ => "Error",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });

        Self { addr, recorded, server }
    }

    /// `http://127.0.0.1:<port>/v1`
    pub(crate) fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// `custom:http://127.0.0.1:<port>/v1`
    pub(crate) fn custom_provider(&self) -> String {
        format!("custom:{}", self.base_url())
    }

    pub(crate) fn requests(&self) -> Vec<ProbeRequest> {
        self.recorded.lock().clone()
    }

    /// Whether any request so far carried `needle` in any header value.
    pub(crate) fn saw(&self, needle: &str) -> bool {
        self.recorded.lock().iter().any(|request| request.carries(needle))
    }

    /// Wait until at least `count` requests arrived, or `timeout` elapsed.
    pub(crate) async fn wait_for_requests(&self, count: usize, timeout: std::time::Duration) -> usize {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let seen = self.recorded.lock().len();
            if seen >= count || tokio::time::Instant::now() >= deadline {
                return seen;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// Search a request body across socket reads while retaining at most one
/// marker-length tail. The probe never stores the prompt or tool payload.
fn scan_marker(tail: &mut Vec<u8>, segment: &[u8], marker: &[u8]) -> bool {
    let mut scan = Vec::with_capacity(tail.len() + segment.len());
    scan.extend_from_slice(tail);
    scan.extend_from_slice(segment);
    let found = scan.windows(marker.len()).any(|window| window == marker);
    let keep = marker.len().saturating_sub(1).min(scan.len());
    tail.clear();
    tail.extend_from_slice(scan.get(scan.len() - keep..).unwrap_or_default());
    found
}

#[test]
fn denial_marker_scan_finds_split_boundary_without_retaining_body() {
    let marker = b"policy denied";
    let mut tail = Vec::new();
    assert!(!scan_marker(&mut tail, b"unrelated policy de", marker));
    assert!(tail.len() < marker.len());
    assert!(scan_marker(&mut tail, b"nied and unrelated", marker));
    assert!(tail.len() < marker.len());
}

impl Drop for CredentialProbe {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn response_body(path: &str, status: u16, reply: &str, error_message: Option<&str>) -> String {
    if status != 200 {
        let message = error_message.map_or_else(|| format!("probe scripted status {status}"), str::to_string);
        return serde_json::json!({ "error": { "message": message, "type": "probe_error" } }).to_string();
    }
    if path.contains("chat/completions") {
        serde_json::json!({
            "id": "chatcmpl-probe",
            "object": "chat.completion",
            "created": 1,
            "model": PROBE_MODEL,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": reply },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
        .to_string()
    } else if path.contains("embeddings") {
        r#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.25,0.5,0.75]}],"model":"probe-embed"}"#
            .to_string()
    } else {
        format!(
            r#"{{"object":"list","data":[{{"id":"{PROBE_MODEL}","object":"model","created":1,"owned_by":"probe"}}]}}"#
        )
    }
}
