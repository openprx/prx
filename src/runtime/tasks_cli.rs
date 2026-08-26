//! Client for the daemon's control API, backing `prx tasks` and the chat-side
//! `message_send`.
//!
//! The registry lives inside the process that owns the work, so inspecting it
//! means talking to that process's gateway rather than looking at local state:
//! a second `prx` invocation has an empty registry of its own and would report
//! nothing useful. The same is true of channels: only the process holding the
//! channel objects can send on them, which is why the outbound call below is a
//! request rather than a local send.
//!
//! This module owns transport and decoding only. Rendering lives in the binary,
//! where writing to stdout is the point.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::Config;

/// Where to reach the running process, and how to authenticate.
pub struct TasksEndpoint {
    base_url: String,
    token: Option<String>,
}

impl TasksEndpoint {
    /// Resolve from explicit flags, falling back to the configured gateway bind
    /// address.
    #[must_use]
    pub fn resolve(config: &Config, url: Option<String>, token: Option<String>) -> Self {
        let base_url = url
            .map(|url| url.trim().trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| {
                let host = if config.gateway.host.trim() == "0.0.0.0" {
                    // A wildcard bind is not a routable address; loopback is
                    // where a local operator can actually reach it.
                    "127.0.0.1"
                } else {
                    config.gateway.host.trim()
                };
                format!("http://{host}:{}", config.gateway.port)
            });
        Self {
            base_url,
            token: token
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty()),
        }
    }

    /// Base URL this client will talk to, for error messages and diagnostics.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// A request that authenticates as this *session* as well as this
    /// operator. Both credentials ride along: the bearer says the caller may
    /// talk to this daemon at all, the session token says which mailbox is its
    /// own.
    fn session_request(
        &self,
        client: &reqwest::Client,
        method: reqwest::Method,
        path: &str,
        session_token: &str,
    ) -> reqwest::RequestBuilder {
        self.request(client, method, path)
            .header(CHAT_SESSION_TOKEN_HEADER, session_token)
    }

    fn request(&self, client: &reqwest::Client, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = client.request(method, format!("{}{path}", self.base_url));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }
}

/// One registered work item as reported by the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    /// Fan-out this item belongs to, when it was started by `spawn_batch`.
    /// Absent on every other kind of work, and on daemons older than this
    /// field — hence `default` rather than a required key.
    #[serde(default)]
    pub batch_id: Option<String>,
    pub elapsed_secs: u64,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub pgid: Option<i32>,
    /// Whether `prx tasks message` can hand this item anything. `default`
    /// because a daemon older than this field omits it, and the safe reading of
    /// silence is "unknown, so do not advertise it" rather than a promise the
    /// listing cannot keep.
    #[serde(default)]
    pub steerable: bool,
}

/// Full listing: live work, plus children spawned but never reaped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TasksListing {
    pub running: Vec<WorkItem>,
    pub unreaped: Vec<WorkItem>,
    pub total: usize,
}

/// One item a kill was applied to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillTarget {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub outcome: String,
}

/// Result of a kill, covering the whole lineage when cascading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillReport {
    pub requested: String,
    pub cascade: bool,
    pub targets: Vec<KillTarget>,
}

/// Result of handing a message to a running task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageReport {
    /// The address the operator supplied.
    pub requested: String,
    /// Work item the address resolved to, in the target process's id space.
    pub id: String,
    /// Run id of the target, the address that is portable across processes.
    #[serde(default)]
    pub run_id: Option<String>,
    pub kind: String,
    pub name: String,
    /// The server's own stable tag for what happened. Rendered verbatim: the
    /// client is in no position to restate the guarantee the server made, and
    /// the tag it makes today (`queued`) is deliberately weaker than the word
    /// this field used to carry.
    pub outcome: String,
    /// The server's plain-language gloss on `outcome`, when it sends one.
    ///
    /// `None` covers a gateway older than the `queued` outcome; renderers must
    /// print it when present and invent nothing when it is absent.
    #[serde(default)]
    pub note: Option<String>,
}

/// One connection pool's counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolReport {
    pub kind: String,
    pub name: String,
    pub metrics: serde_json::Value,
}

/// Every pool the process is holding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolsReport {
    pub pools: Vec<PoolReport>,
}

/// HTTP client for the control API.
///
/// No overall request timeout is configured: this CLI talks to a runtime whose
/// whole premise is that work is not cut off on a clock, and a kill
/// verification legitimately takes a couple of seconds. The connect timeout
/// stays bounded so an unreachable address is reported promptly instead of
/// hanging.
fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .context("failed to build HTTP client for the runtime control API")
}

/// A refusal the control API stated, kept in the form it was stated in.
///
/// # Why this is a type and not a sentence
///
/// A caller that has to *act* on a particular refusal — the chat assignment
/// poller re-registering when the daemon says it has no such session — used to
/// have nothing but the rendered message to go on, and matching on prose is a
/// bug waiting for the day somebody improves the wording. The status code and
/// the API's own machine tag ride along here so a decision can be made on them,
/// while [`std::fmt::Display`] still renders exactly the sentence this client
/// has always produced.
#[derive(Debug, Clone)]
pub struct ControlApiRefusal {
    /// HTTP status the daemon answered with.
    status: u16,
    /// The API's stable machine tag for the refusal (`code` in the body), when
    /// it sent one. A daemon older than the tag sends none, so `None` means
    /// "not stated" and never "some other reason".
    code: Option<String>,
    /// The full operator-facing message, including any hint appended to it.
    message: String,
}

impl ControlApiRefusal {
    /// The tag the daemon uses for "no session with that id is registered".
    ///
    /// Must stay in step with `ChatSessionError::code` on the daemon side; the
    /// pair is what keeps this decision off the prose.
    pub const UNKNOWN_SESSION: &'static str = "unknown_session";

    /// HTTP status the daemon answered with.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// The API's machine tag for this refusal, when it sent one.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// Whether this refusal is the daemon saying it holds no such chat session.
    ///
    /// A `404` with the tag is unambiguous. A `404` with **no** tag is accepted
    /// as the same thing because that is what an older daemon — or a router
    /// that no longer has the route — answers, and on the mailbox endpoints
    /// there is nothing else a `404` can mean: the pull path returns it only
    /// for an unregistered session, and an unknown acknowledgement id is
    /// ignored rather than refused. A tag that says something *else* is
    /// therefore the only case this rejects, which is exactly the case a newer
    /// daemon would use to mean something new.
    #[must_use]
    pub fn says_the_session_is_unregistered(&self) -> bool {
        self.status == reqwest::StatusCode::NOT_FOUND.as_u16()
            && matches!(self.code.as_deref(), None | Some(Self::UNKNOWN_SESSION))
    }
}

impl std::fmt::Display for ControlApiRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ControlApiRefusal {}

/// Whether an error from this client is the daemon saying it has no such chat
/// session.
///
/// The one supported way to ask. Callers must not match on the message: see
/// [`ControlApiRefusal`].
#[must_use]
pub fn session_is_unregistered(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ControlApiRefusal>()
        .is_some_and(ControlApiRefusal::says_the_session_is_unregistered)
}

/// The `code` and `error` fields the API sends with a refusal, when it sends
/// them. One reader, so a test double and the live client cannot disagree about
/// what a refusal body means.
fn refusal_fields(body: &str) -> (Option<String>, Option<String>) {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|value| value.get(name))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
    };
    (field("code"), field("error"))
}

/// Rebuild the error a refusal body would have produced, for tests that need a
/// client-side failure in the exact shape the daemon sends one.
#[cfg(test)]
#[must_use]
pub fn refusal_for_tests(status: u16, body: &str, what: &str) -> anyhow::Error {
    let (code, detail) = refusal_fields(body);
    let detail = detail.unwrap_or_else(|| body.to_string());
    anyhow::Error::new(ControlApiRefusal {
        status,
        code,
        message: format!("{what} failed with HTTP {status}: {detail}"),
    })
}

async fn decode<T: serde::de::DeserializeOwned>(
    endpoint: &TasksEndpoint,
    response: reqwest::Response,
    what: &str,
) -> Result<T> {
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("failed to read {what} response body"))?;
    if !status.is_success() {
        let (code, detail) = refusal_fields(&body);
        let detail = detail.unwrap_or(body);
        let message = if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            auth_hint(endpoint, status, what, &detail)
        } else {
            format!("{what} failed with HTTP {status}: {detail}")
        };
        return Err(anyhow::Error::new(ControlApiRefusal {
            status: status.as_u16(),
            code,
            message,
        }));
    }
    serde_json::from_str::<T>(&body).with_context(|| format!("failed to parse {what} response"))
}

/// Turn a rejected credential into something the operator can act on.
///
/// An unreachable gateway already says what to do about it; a 401 said only
/// "HTTP 401" and left the operator to guess. It is the *first* thing anyone
/// meets, too: `gateway.require_pairing` defaults to on and `chat.daemon.token`
/// defaults to empty, so an out-of-the-box `/sessions --daemon` is a 401 by
/// construction. The two causes need different advice — no credential was sent
/// at all, or one was sent and refused — so they are separated here rather than
/// merged into one hedged sentence.
#[must_use]
fn auth_hint(endpoint: &TasksEndpoint, status: reqwest::StatusCode, what: &str, detail: &str) -> String {
    let cause = if endpoint.token.is_some() {
        "A bearer token was sent and the daemon refused it. Note that `gateway.paired_tokens` \
         stores SHA-256 hashes of paired tokens: only the plaintext token authenticates, and \
         handing back a stored hash never will."
    } else {
        "No bearer token was sent, and the daemon requires one \
         (`gateway.require_pairing` is on by default)."
    };
    format!(
        "{what} failed with HTTP {status}: {detail}\n\
         {cause}\n\
         Set `chat.daemon.token` (under `[chat.daemon]` in the PRX config) to a token the daemon \
         at {} accepts — or pass `--token` to `prx tasks`, or put the token in \
         `gateway.paired_tokens` when chat and the daemon share a config dir. A token is minted \
         by pairing: `POST /pair` with header `X-Pairing-Code: <code>`, using the one-time code \
         the daemon prints at startup.",
        endpoint.base_url()
    )
}

fn unreachable_hint(endpoint: &TasksEndpoint, error: &reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "cannot reach a running PRX process at {}: {error}\n\
         The work registry lives inside the process that owns the work, so this command needs a \
         live gateway. Start one with `prx daemon`, or point `--url` at the process you want to \
         inspect.",
        endpoint.base_url()
    )
}

/// Fetch the current work listing.
pub async fn fetch_tasks(endpoint: &TasksEndpoint) -> Result<TasksListing> {
    let client = client()?;
    let response = endpoint
        .request(&client, reqwest::Method::GET, "/api/runtime/tasks")
        .send()
        .await
        .map_err(|error| unreachable_hint(endpoint, &error))?;
    decode(endpoint, response, "runtime task listing").await
}

/// Ask the running process to terminate one work item.
pub async fn request_kill(endpoint: &TasksEndpoint, id: &str, cascade: bool) -> Result<KillReport> {
    let client = client()?;
    // Ids reach here straight from an operator's shell; percent-encode so an
    // address that is not a bare uuid cannot alter the request path.
    let path = format!("/api/runtime/tasks/{}/kill?cascade={cascade}", urlencoding::encode(id));
    let response = endpoint
        .request(&client, reqwest::Method::POST, &path)
        .send()
        .await
        .map_err(|error| unreachable_hint(endpoint, &error))?;
    decode(endpoint, response, "runtime task kill").await
}

/// Hand a message to one running task, addressed by run id or work id.
///
/// No request timeout is imposed here for the same reason the rest of this
/// client has none, and one more: the target's message queue is bounded, so a
/// busy run legitimately parks the send until it drains. That is backpressure
/// working, not a stall to abandon.
pub async fn request_message(endpoint: &TasksEndpoint, id: &str, message: &str) -> Result<MessageReport> {
    let client = client()?;
    let path = format!("/api/runtime/tasks/{}/message", urlencoding::encode(id));
    let response = endpoint
        .request(&client, reqwest::Method::POST, &path)
        .json(&serde_json::json!({ "message": message }))
        .send()
        .await
        .map_err(|error| unreachable_hint(endpoint, &error))?;
    decode(endpoint, response, "runtime task message").await
}

/// Outcome of an outbound send performed by the daemon on the caller's behalf.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSendReport {
    /// Channel the daemon delivered on.
    pub channel: String,
    pub delivered: bool,
    /// The daemon's own account of the delivery.
    pub detail: String,
}

/// Ask the daemon to send one message on one of its channels.
///
/// The caller supplies a destination and a body; **every policy decision is the
/// daemon's**. Nothing here pre-approves the send, and the daemon does not
/// trust this client to have checked anything — a refusal comes back as a
/// non-success status whose body carries the reason, which is surfaced verbatim
/// rather than reinterpreted.
///
/// As everywhere else in this client, there is no overall request timeout: a
/// channel that takes its time to accept a message is working, not stalled.
pub async fn request_channel_send(
    endpoint: &TasksEndpoint,
    channel: &str,
    recipient: &str,
    message: &str,
    as_voice: bool,
) -> Result<ChannelSendReport> {
    let client = client()?;
    // The channel name reaches here from a model or an operator; percent-encode
    // it so a name that is not a bare identifier cannot alter the request path.
    let path = format!("/api/channels/{}/send", urlencoding::encode(channel));
    let response = endpoint
        .request(&client, reqwest::Method::POST, &path)
        .json(&serde_json::json!({
            "recipient": recipient,
            "message": message,
            "as_voice": as_voice,
        }))
        .send()
        .await
        .map_err(|error| unreachable_hint(endpoint, &error))?;
    decode(endpoint, response, "channel send").await
}

/// Header the daemon's chat-session mailbox authenticates a *session* with.
///
/// The bearer token authenticates a local **operator**, and every process on
/// this host that can read the config is one. That is the right level for
/// looking at work and killing it, and the wrong level for a mailbox: pulling
/// another chat's assignments would take its work away from it, and reporting
/// on its behalf would put words in its mouth. The per-session token minted at
/// registration is what tells the two apart, so the mailbox calls carry both.
pub const CHAT_SESSION_TOKEN_HEADER: &str = "X-Prx-Session-Token";

/// What registering a chat session with a daemon yields.
///
/// `token` is returned exactly once — the daemon keeps only its hash — so a
/// caller that drops it has to register again under a new id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSessionRegistration {
    pub session_id: String,
    /// Redacted handle the daemon uses for this session in refusals and rows.
    pub session_ref: String,
    pub token: String,
    pub label: String,
    pub registered_at_unix_ms: u64,
}

/// One assignment handed to this chat session by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PulledAssignment {
    pub assignment_id: String,
    pub session_id: String,
    pub task: String,
    /// `queue`, `steer` or `interrupt`. Kept as the server's own string: the
    /// client maps it to behaviour and must be able to see a word it does not
    /// know rather than have it silently become a known one.
    pub disposition: String,
    /// Greater than one means this is a redelivery. The mailbox is
    /// at-least-once, so `assignment_id` is the idempotency key.
    #[serde(default = "one")]
    pub deliveries: u32,
    #[serde(default)]
    pub created_at_unix_ms: u64,
    #[serde(default)]
    pub work_id: String,
    /// Channel the assignment came in on, in plaintext. Non-sensitive, and
    /// without it nobody can say where a task came from.
    #[serde(default)]
    pub origin_channel: String,
    /// Redacted origin fingerprint; absent for the operator plane, which has no
    /// correspondent to protect.
    #[serde(default)]
    pub origin_ref: Option<String>,
}

const fn one() -> u32 {
    1
}

/// One mailbox pull: the batch handed over, plus what the daemon did with the
/// acknowledgements that rode along.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentPull {
    pub assignments: Vec<PulledAssignment>,
    #[serde(default)]
    pub acked: usize,
    /// Assignments delivered last time, never acknowledged, and now put back at
    /// the head of the queue.
    #[serde(default)]
    pub requeued: usize,
    #[serde(default)]
    pub queued_remaining: usize,
}

/// The daemon's receipt for one reported assignment result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentReceipt {
    pub assignment_id: String,
    pub status: String,
    pub seq: u64,
}

/// What deregistering a chat session took down with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSessionDeregistration {
    pub session_id: String,
    /// Outstanding assignments the daemon cancelled because nobody is left to
    /// run them.
    #[serde(default)]
    pub discarded: usize,
}

/// Enrol a `prx chat` session so the daemon can hand it work.
pub async fn register_chat_session(
    endpoint: &TasksEndpoint,
    label: &str,
    pid: Option<u32>,
) -> Result<ChatSessionRegistration> {
    let client = client()?;
    let response = endpoint
        .request(&client, reqwest::Method::POST, "/api/chat-sessions/register")
        .json(&serde_json::json!({ "label": label, "pid": pid }))
        .send()
        .await
        .map_err(|error| unreachable_hint(endpoint, &error))?;
    decode(endpoint, response, "chat session registration").await
}

/// Take this session's queued work, acknowledging the previous batch.
///
/// `ack` carries the ids handed over by the *previous* call and now safely in
/// the caller's hands. Acknowledging on the next pull rather than on receipt is
/// what makes a client that dies between the two lose nothing: the daemon puts
/// anything unacknowledged back at the head of the queue.
///
/// As everywhere else in this client there is no request timeout — the poll
/// interval is a sampling rate, not a deadline, and an answer that takes its
/// time is an answer.
pub async fn pull_chat_assignments(
    endpoint: &TasksEndpoint,
    session_id: &str,
    session_token: &str,
    ack: &[String],
    max: Option<usize>,
) -> Result<AssignmentPull> {
    let client = client()?;
    let path = format!("/api/chat-sessions/{}/inbox/pull", urlencoding::encode(session_id));
    let response = endpoint
        .session_request(&client, reqwest::Method::POST, &path, session_token)
        .json(&serde_json::json!({ "ack": ack, "max": max }))
        .send()
        .await
        .map_err(|error| unreachable_hint(endpoint, &error))?;
    decode(endpoint, response, "chat assignment pull").await
}

/// Tell the daemon what this session made of one assignment.
///
/// `status` is one of `completed`, `failed`, `rejected`. `cancelled` is the
/// daemon's own verdict about work *it* ended and is refused here, so an
/// operator kill can never be confused with a client giving up.
pub async fn report_chat_assignment(
    endpoint: &TasksEndpoint,
    session_id: &str,
    session_token: &str,
    assignment_id: &str,
    status: &str,
    summary: &str,
) -> Result<AssignmentReceipt> {
    let client = client()?;
    let path = format!("/api/chat-sessions/{}/result", urlencoding::encode(session_id));
    let response = endpoint
        .session_request(&client, reqwest::Method::POST, &path, session_token)
        .json(&serde_json::json!({
            "assignment_id": assignment_id,
            "status": status,
            "summary": summary,
        }))
        .send()
        .await
        .map_err(|error| unreachable_hint(endpoint, &error))?;
    decode(endpoint, response, "chat assignment result").await
}

/// Withdraw this chat session from the daemon's registry.
///
/// Nothing on the daemon expires a session on a clock, by design: a session
/// that stopped pulling is *reported* as silent, never evicted. Deregistering
/// is therefore the only thing that removes the row, and skipping it on exit
/// leaves a permanent zombie that assignments can still be addressed to.
pub async fn deregister_chat_session(endpoint: &TasksEndpoint, session_id: &str) -> Result<ChatSessionDeregistration> {
    let client = client()?;
    let path = format!("/api/chat-sessions/{}", urlencoding::encode(session_id));
    let response = endpoint
        .request(&client, reqwest::Method::DELETE, &path)
        .send()
        .await
        .map_err(|error| unreachable_hint(endpoint, &error))?;
    decode(endpoint, response, "chat session deregistration").await
}

/// Fetch connection-pool occupancy and saturation counters.
pub async fn fetch_pools(endpoint: &TasksEndpoint) -> Result<PoolsReport> {
    let client = client()?;
    let response = endpoint
        .request(&client, reqwest::Method::GET, "/api/runtime/pools")
        .send()
        .await
        .map_err(|error| unreachable_hint(endpoint, &error))?;
    decode(endpoint, response, "runtime pool metrics").await
}

/// Lay a work listing out so the members of one `spawn_batch` fan-out read as
/// one unit.
///
/// Returns the items regrouped, never filtered: each entry is a batch id (or
/// `None` for work that belongs to no batch) together with the items under it.
/// A batch takes the position of its *first* member, so a listing without any
/// batches comes back in exactly the order the daemon sent it and renders
/// byte-for-byte as it always did.
///
/// Grouping is needed because batch members are *siblings*, not a lineage: the
/// tool call that launched them is deregistered as soon as it returns, so
/// without this they are scattered through the table by spawn order with
/// nothing tying them together.
///
/// This lives here rather than in the binary because it is an ordering
/// decision, not a rendering one — and an ordering decision is testable.
#[must_use]
pub fn group_by_batch(items: &[WorkItem]) -> Vec<(Option<&str>, Vec<&WorkItem>)> {
    let mut groups: Vec<(Option<&str>, Vec<&WorkItem>)> = Vec::new();
    for item in items {
        match item.batch_id.as_deref() {
            None => groups.push((None, vec![item])),
            Some(batch_id) => match groups.iter_mut().find(|(existing, _)| *existing == Some(batch_id)) {
                Some((_, members)) => members.push(item),
                None => groups.push((Some(batch_id), vec![item])),
            },
        }
    }
    groups
}

/// Render an elapsed duration the way an operator scans it.
#[must_use]
pub fn format_elapsed(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// A gateway that answers exactly one request with the given status line
    /// and JSON body, then closes. Enough to exercise the client's decoding of
    /// a refusal without standing up the real gateway.
    async fn refusing_gateway(status_line: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test: bind ephemeral port");
        let addr = listener.local_addr().expect("test: local addr");
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0_u8; 2048];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        format!("http://{addr}")
    }

    /// `gateway.require_pairing` is on by default and `chat.daemon.token` is
    /// empty by default, so the very first `/sessions --daemon` an operator
    /// runs is a 401. It has to say which key to set, the way the unreachable
    /// case already says which process to start.
    #[tokio::test]
    async fn a_401_names_the_config_key_that_fixes_it() {
        let url = refusing_gateway("HTTP/1.1 401 Unauthorized", r#"{"error":"unauthorized"}"#).await;
        let endpoint = TasksEndpoint::resolve(&Config::default(), Some(url), None);

        let error = fetch_tasks(&endpoint).await.expect_err("test: a 401 is an error");
        let text = format!("{error:#}");

        assert!(text.contains("401"), "the status is still reported: {text}");
        assert!(text.contains("chat.daemon.token"), "no key to set: {text}");
        assert!(text.contains("gateway.require_pairing"), "no cause named: {text}");
        assert!(text.contains("No bearer token was sent"), "wrong cause: {text}");
    }

    /// A token that was sent and refused is a different problem from no token
    /// at all — most often a stored hash pasted back as a bearer — so it gets
    /// its own advice rather than being told to set a key it already set.
    #[tokio::test]
    async fn a_rejected_token_is_told_apart_from_a_missing_one() {
        let url = refusing_gateway("HTTP/1.1 403 Forbidden", r#"{"error":"forbidden"}"#).await;
        let endpoint = TasksEndpoint::resolve(&Config::default(), Some(url), Some("zc_wrong".to_string()));

        let error = fetch_tasks(&endpoint).await.expect_err("test: a 403 is an error");
        let text = format!("{error:#}");

        assert!(text.contains("refused it"), "{text}");
        assert!(text.contains("gateway.paired_tokens"), "{text}");
        assert!(text.contains("chat.daemon.token"), "{text}");
        assert!(!text.contains("No bearer token was sent"), "{text}");
    }

    #[test]
    fn endpoint_defaults_to_the_configured_gateway() {
        let mut config = Config::default();
        config.gateway.host = "127.0.0.1".to_string();
        config.gateway.port = 16830;
        let endpoint = TasksEndpoint::resolve(&config, None, None);
        assert_eq!(endpoint.base_url(), "http://127.0.0.1:16830");
    }

    #[test]
    fn wildcard_bind_resolves_to_loopback() {
        let mut config = Config::default();
        config.gateway.host = "0.0.0.0".to_string();
        config.gateway.port = 8080;
        let endpoint = TasksEndpoint::resolve(&config, None, None);
        assert_eq!(endpoint.base_url(), "http://127.0.0.1:8080");
    }

    #[test]
    fn explicit_url_wins_and_is_normalized() {
        let config = Config::default();
        let endpoint = TasksEndpoint::resolve(&config, Some("http://box:9000/".to_string()), None);
        assert_eq!(endpoint.base_url(), "http://box:9000");
    }

    #[test]
    fn blank_token_is_treated_as_absent() {
        let config = Config::default();
        let endpoint = TasksEndpoint::resolve(&config, None, Some("   ".to_string()));
        assert!(endpoint.token.is_none());
    }

    fn item(id: &str, batch_id: Option<&str>) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            kind: "sub_agent".to_string(),
            name: format!("run {id}"),
            state: "running".to_string(),
            parent: None,
            run_id: None,
            batch_id: batch_id.map(ToString::to_string),
            elapsed_secs: 3,
            pid: None,
            pgid: None,
            steerable: false,
        }
    }

    /// A listing with no batches must come back exactly as it went in: one
    /// group per item, in the daemon's order.
    #[test]
    fn a_listing_without_batches_is_left_alone() {
        let items = vec![item("w1", None), item("w2", None)];
        let groups = group_by_batch(&items);
        assert_eq!(groups.len(), 2);
        assert!(
            groups
                .iter()
                .all(|(batch, members)| batch.is_none() && members.len() == 1)
        );
        let ids: Vec<&str> = groups
            .iter()
            .flat_map(|(_, members)| members.iter().map(|item| item.id.as_str()))
            .collect();
        assert_eq!(ids, vec!["w1", "w2"]);
    }

    /// Members of one fan-out are pulled together at the position of the first
    /// of them, and unrelated work keeps its place rather than being swallowed.
    #[test]
    fn batch_members_are_gathered_under_one_group() {
        let items = vec![
            item("w1", None),
            item("w2", Some("batch-a")),
            item("w3", None),
            item("w4", Some("batch-a")),
            item("w5", Some("batch-b")),
        ];
        let groups = group_by_batch(&items);
        let shape: Vec<(Option<&str>, Vec<&str>)> = groups
            .iter()
            .map(|(batch, members)| (*batch, members.iter().map(|item| item.id.as_str()).collect()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (None, vec!["w1"]),
                (Some("batch-a"), vec!["w2", "w4"]),
                (None, vec!["w3"]),
                (Some("batch-b"), vec!["w5"]),
            ]
        );
    }

    /// Grouping regroups, it never drops: every item appears exactly once.
    #[test]
    fn grouping_never_loses_an_item() {
        let items = vec![
            item("w1", Some("batch-a")),
            item("w2", None),
            item("w3", Some("batch-a")),
            item("w4", Some("batch-b")),
        ];
        let grouped: usize = group_by_batch(&items).iter().map(|(_, members)| members.len()).sum();
        assert_eq!(grouped, items.len());
    }

    #[test]
    fn elapsed_formatting_is_readable_at_every_scale() {
        assert_eq!(format_elapsed(9), "9s");
        assert_eq!(format_elapsed(75), "1m15s");
        assert_eq!(format_elapsed(3725), "1h02m05s");
    }
}
