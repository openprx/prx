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
        let detail = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .unwrap_or(body);
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            anyhow::bail!("{}", auth_hint(endpoint, status, what, &detail));
        }
        anyhow::bail!("{what} failed with HTTP {status}: {detail}");
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
