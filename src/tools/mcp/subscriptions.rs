//! Process-wide resource subscription hub for `mcp_call` subscribe and
//! unsubscribe. Subscriptions live in memory for the life of the process.
//!
//! A hub is shared by every registry of one workspace directory in the
//! process. Each registry adopts the hub with a weak reference, so a replaced
//! registry is forgotten, and the newest live registry serves reconnects.
//! One `subscriptions/listen` stream serves every session that subscribed to
//! the same (server, URI); the stream task lives in `subscription_stream`.

use super::McpTool;
use super::subscription_stream::{self, OpenFailure};
use crate::config::Config;
use crate::security::redaction::redact_provider_text;
use crate::tools::traits::ToolResult;
use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rmcp::service::Subscription;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, SystemTime};
use tokio::task::AbortHandle;

/// The hubs of this process, keyed by the workspace directory of their registries.
fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Hub>>> {
    static HUBS: OnceLock<Mutex<HashMap<PathBuf, Arc<Hub>>>> = OnceLock::new();
    HUBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The session that subscribed: the owner, the session key and the agent of
/// the `mcp_call` receipt.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct SubscriberKey {
    owner: String,
    session: String,
    agent: String,
}

/// One subscriber of a stream, with the time it subscribed and the chain event
/// of its latest subscribe call (the wake-depth anchor of its notices).
pub(super) struct SubscriberState {
    since: SystemTime,
    chain_event: Option<String>,
}

/// The health of one (server, URI) stream as `status` shows it.
pub(super) enum StreamState {
    Active,
    Reconnecting {
        since: SystemTime,
        last_error: String,
    },
    /// The server did not acknowledge the URI on a reconnect. No retry happens
    /// until a new subscribe.
    Declined,
}

/// The stream of one (server, URI): its subscribers, its task and its counters.
pub(super) struct UriStream {
    subscribers: BTreeMap<SubscriberKey, SubscriberState>,
    /// The consumer task; `None` only while a first subscribe is being set up.
    task: Option<AbortHandle>,
    state: StreamState,
    updates: u64,
    last_update_at: Option<SystemTime>,
    /// The length of the last outage, from the end of the stream to the new
    /// acknowledgment (the value M6d reports on "resumed").
    last_outage: Option<Duration>,
}

impl UriStream {
    fn pending() -> Self {
        Self {
            subscribers: BTreeMap::new(),
            task: None,
            state: StreamState::Active,
            updates: 0,
            last_update_at: None,
            last_outage: None,
        }
    }

    fn is_declined(&self) -> bool {
        matches!(self.state, StreamState::Declined)
    }

    /// Add `caller`, or refresh its chain event; `true` when the stream now has
    /// more than one subscriber.
    fn add_subscriber(&mut self, caller: SubscriberKey, chain_event: Option<String>) -> bool {
        let subscriber = self.subscribers.entry(caller).or_insert_with(|| SubscriberState {
            since: SystemTime::now(),
            chain_event: None,
        });
        subscriber.chain_event = chain_event;
        self.subscribers.len() > 1
    }

    /// Install a freshly opened stream and its consumer task.
    fn start(&mut self, task: AbortHandle, caller: SubscriberKey, chain_event: Option<String>) {
        self.task = Some(task);
        self.state = StreamState::Active;
        self.last_outage = None;
        self.add_subscriber(caller, chain_event);
    }
}

/// A registry that adopted the hub, with the configuration it was built from.
struct Owner {
    tool: Weak<McpTool>,
    config: Arc<Config>,
}

/// One workspace's subscriptions and the registries that serve them.
#[derive(Default)]
pub(super) struct Hub {
    /// Registries that adopted this hub, oldest first. Dead ones are pruned on adopt.
    owners: Mutex<Vec<Owner>>,
    /// Streams keyed by (server, URI).
    streams: Mutex<BTreeMap<(String, String), UriStream>>,
}

impl Hub {
    /// The newest registry still alive and its configuration, if any.
    pub(super) fn newest_owner(&self) -> Option<(Arc<McpTool>, Arc<Config>)> {
        self.owners
            .lock()
            .iter()
            .rev()
            .find_map(|owner| Some((owner.tool.upgrade()?, Arc::clone(&owner.config))))
    }

    /// Add `caller` to a live stream of `key`; `None` when there is no live
    /// stream (none, or one declined by the server).
    fn join(&self, key: &(String, String), caller: &SubscriberKey, chain_event: &Option<String>) -> Option<bool> {
        let mut streams = self.streams.lock();
        let stream = streams.get_mut(key)?;
        if stream.task.is_none() || stream.is_declined() {
            return None;
        }
        Some(stream.add_subscriber(caller.clone(), chain_event.clone()))
    }

    /// Subscribe `caller` to (server, URI), opening the stream if none is live.
    pub(super) async fn subscribe(
        self: &Arc<Self>,
        tool: &McpTool,
        caller: SubscriberKey,
        chain_event: Option<String>,
        server: &str,
        uri: &str,
    ) -> anyhow::Result<bool> {
        let key = (server.to_string(), uri.to_string());
        if let Some(shared) = self.join(&key, &caller, &chain_event) {
            return Ok(shared);
        }
        let subscription: Subscription = match subscription_stream::open(tool, server, uri).await {
            Ok(subscription) => subscription,
            Err(OpenFailure::Declined) => bail!("server '{server}' declined the subscription to {uri}"),
            Err(OpenFailure::Gone(error) | OpenFailure::Failed { error, .. }) => return Err(error),
        };
        let mut streams = self.streams.lock();
        let stream = streams.entry(key.clone()).or_insert_with(UriStream::pending);
        if stream.task.is_some() && !stream.is_declined() {
            // A racing subscribe opened the stream first: share it and drop ours.
            let shared = stream.add_subscriber(caller, chain_event);
            drop(streams);
            drop(subscription);
            return Ok(shared);
        }
        let task = tokio::spawn(subscription_stream::run(Arc::clone(self), key, subscription));
        stream.start(task.abort_handle(), caller, chain_event);
        Ok(stream.subscribers.len() > 1)
    }

    /// Remove `caller` from (server, URI). The last subscriber ends the stream;
    /// the dropped subscription cancels the request on the wire.
    pub(super) fn unsubscribe(&self, caller: &SubscriberKey, server: &str, uri: &str) {
        let key = (server.to_string(), uri.to_string());
        let mut streams = self.streams.lock();
        let Some(stream) = streams.get_mut(&key) else {
            return;
        };
        if stream.subscribers.remove(caller).is_none() {
            return;
        }
        if stream.subscribers.is_empty()
            && let Some(ended) = streams.remove(&key)
            && let Some(task) = ended.task
        {
            task.abort();
        }
    }

    /// The streams `caller` subscribed to, as `status` shows them.
    pub(super) fn view(&self, caller: &SubscriberKey) -> Vec<Value> {
        let streams = self.streams.lock();
        streams
            .iter()
            .filter_map(|((server, uri), stream)| {
                let subscriber = stream.subscribers.get(caller)?;
                let (state, last_error) = match &stream.state {
                    StreamState::Active => ("active", None),
                    StreamState::Reconnecting { last_error, .. } => ("reconnecting", Some(last_error.as_str())),
                    StreamState::Declined => ("declined", None),
                };
                let mut entry = json!({
                    "server": server,
                    "uri": uri,
                    "state": state,
                    "since": rfc3339(subscriber.since),
                    "updates": stream.updates,
                    "last_update_at": stream.last_update_at.map(rfc3339),
                });
                if let Some(last_error) = last_error {
                    entry["last_error"] = json!(last_error);
                }
                if let Some(outage) = stream.last_outage {
                    entry["last_outage_secs"] = json!(outage.as_secs());
                }
                Some(entry)
            })
            .collect()
    }

    /// Count one update of a stream. Other URIs never reach here.
    pub(super) fn record_update(&self, key: &(String, String)) {
        if let Some(stream) = self.streams.lock().get_mut(key) {
            stream.updates += 1;
            stream.last_update_at = Some(SystemTime::now());
        }
    }

    /// Mark a stream down; keeps the start of an outage that is already running.
    pub(super) fn set_reconnecting(&self, key: &(String, String), last_error: String) {
        if let Some(stream) = self.streams.lock().get_mut(key) {
            let since = match stream.state {
                StreamState::Reconnecting { since, .. } => since,
                _ => SystemTime::now(),
            };
            stream.state = StreamState::Reconnecting { since, last_error };
        }
    }

    /// Mark a stream up again after an outage of `outage`.
    pub(super) fn set_active(&self, key: &(String, String), outage: Duration) {
        if let Some(stream) = self.streams.lock().get_mut(key) {
            stream.state = StreamState::Active;
            stream.last_outage = Some(outage);
        }
    }

    /// Mark a stream declined; its subscribers stay so `status` shows it.
    pub(super) fn set_declined(&self, key: &(String, String)) {
        if let Some(stream) = self.streams.lock().get_mut(key) {
            stream.state = StreamState::Declined;
        }
    }

    /// Drop a stream and its subscribers: the server cannot serve it any more.
    pub(super) fn remove_stream(&self, key: &(String, String)) {
        self.streams.lock().remove(key);
    }
}

/// Register `tool` with the hub of `workspace_dir`, creating the hub on first use.
/// `root_config` is the configuration the registry was built from.
pub(crate) fn adopt(tool: &Arc<McpTool>, workspace_dir: &Path, root_config: &Config) {
    let hub = {
        let mut hubs = registry().lock();
        Arc::clone(hubs.entry(workspace_dir.to_path_buf()).or_default())
    };
    let mut owners = hub.owners.lock();
    owners.retain(|owner| owner.tool.strong_count() > 0);
    owners.push(Owner {
        tool: Arc::downgrade(tool),
        config: Arc::new(root_config.clone()),
    });
}

/// The hub that `tool` adopted, if any. A registry built outside the tool
/// setup (for example a diagnostics snapshot) has none.
pub(super) fn hub_for(tool: &McpTool) -> Option<Arc<Hub>> {
    let hubs = registry().lock();
    hubs.values()
        .find(|hub| {
            hub.owners
                .lock()
                .iter()
                .any(|owner| std::ptr::eq(owner.tool.as_ptr(), tool))
        })
        .cloned()
}

/// The subscriber of this `mcp_call` from its receipt, and the receipt's
/// source event (the chain its notices continue).
fn caller_key() -> anyhow::Result<(SubscriberKey, Option<String>)> {
    let caller = crate::tools::execution::current_child_call_context()
        .ok_or_else(|| anyhow!("subscribe needs a session context"))?;
    caller.verify_tool("mcp_call")?;
    let envelope = &caller.context().envelope;
    let key = SubscriberKey {
        owner: envelope.resolved_owner_id(),
        session: envelope.session_key.to_string(),
        agent: envelope.agent_id.as_deref().unwrap_or("main").to_string(),
    };
    Ok((key, envelope.source_message_event_id.clone()))
}

fn rfc3339(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339()
}

impl McpTool {
    /// Serve `subscribe` or `unsubscribe` for the root `mcp_call` tool. Neither
    /// runs a tool, so neither takes the read-only check or the `SideEffectGate`.
    pub(super) async fn execute_subscription_action(&self, action: &str, args: &Value) -> anyhow::Result<ToolResult> {
        Ok(match self.run_subscription_action(action, args).await {
            Ok(output) => ToolResult {
                success: true,
                output,
                error: None,
            },
            Err(err) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(redact_provider_text(&format!("{err:#}"))),
            },
        })
    }

    async fn run_subscription_action(&self, action: &str, args: &Value) -> anyhow::Result<String> {
        let (caller, chain_event) = caller_key()?;
        let server = args
            .get("server")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing 'server' parameter"))?;
        let uri = args
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing 'uri' parameter"))?;
        let hub = hub_for(self).ok_or_else(|| anyhow!("subscriptions are not available in this registry"))?;
        if action == "unsubscribe" {
            hub.unsubscribe(&caller, server, uri);
            return Ok(json!({"subscribed": false, "server": server, "uri": uri}).to_string());
        }
        self.refresh_runtime_state(false).await;
        let shared = hub.subscribe(self, caller, chain_event, server, uri).await?;
        Ok(json!({"subscribed": true, "server": server, "uri": uri, "shared": shared}).to_string())
    }

    /// The caller's subscriptions for `list` and `status`; `None` without a
    /// receipt, so the field is omitted.
    pub(super) fn subscriptions_view(&self) -> Option<Value> {
        // Without a receipt there is no subscriber to show; the field is omitted.
        let (caller, _) = caller_key().ok()?;
        let entries = hub_for(self).map(|hub| hub.view(&caller)).unwrap_or_default();
        Some(Value::Array(entries))
    }
}
