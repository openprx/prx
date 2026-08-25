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
    pub outcome: String,
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

async fn decode<T: serde::de::DeserializeOwned>(response: reqwest::Response, what: &str) -> Result<T> {
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
        anyhow::bail!("{what} failed with HTTP {status}: {detail}");
    }
    serde_json::from_str::<T>(&body).with_context(|| format!("failed to parse {what} response"))
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
    decode(response, "runtime task listing").await
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
    decode(response, "runtime task kill").await
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
    decode(response, "runtime task message").await
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
    decode(response, "channel send").await
}

/// Fetch connection-pool occupancy and saturation counters.
pub async fn fetch_pools(endpoint: &TasksEndpoint) -> Result<PoolsReport> {
    let client = client()?;
    let response = endpoint
        .request(&client, reqwest::Method::GET, "/api/runtime/pools")
        .send()
        .await
        .map_err(|error| unreachable_hint(endpoint, &error))?;
    decode(response, "runtime pool metrics").await
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
mod tests {
    use super::*;

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
