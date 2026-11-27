//! Tool-list freshness: the tools listener of each live connection, and the
//! per-server rediscovery that its notifications and `ttlMs` hints trigger.
//!
//! A connection to a 2026-07-28 server that declares `tools.listChanged` gets
//! one `toolsListChanged` subscription. A notification only marks the server
//! stale; the next catalog refresh lists that server again, over its live
//! connection when there is one and through discovery otherwise. A server
//! without the capability is listed again once its `ttlMs` has passed.
//!
//! A changed list is committed under the state lock with the server's
//! freshness generation, not the global epoch, so only the aliases of that
//! server are invalidated. An unchanged list commits nothing.

use super::client::discovered_tool_meta;
use super::lifecycle::McpEra;
use super::{DiscoveredToolMeta, McpClient, McpTool};
use crate::security::redaction::redact_provider_text;
use crate::tools::traits::Tool;
use rmcp::RoleClient;
use rmcp::model::{ServerNotification, SubscriptionFilter};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The shortest rediscovery interval a `ttlMs` hint can ask for.
const MIN_TTL: Duration = Duration::from_secs(60);

/// Servers marked stale by a listener, waiting for the next catalog refresh.
pub(super) type StaleSet = Arc<parking_lot::Mutex<HashSet<String>>>;

/// The deadline a `ttlMs` hint sets for the next rediscovery of a server, or
/// `None` when the server gave no hint or the deadline is not representable
/// (no TTL relist; a `list_changed` still works).
pub(super) fn ttl_deadline(ttl_ms: Option<u64>) -> Option<Instant> {
    ttl_ms.and_then(|ms| Instant::now().checked_add(Duration::from_millis(ms).max(MIN_TTL)))
}

/// Start the `toolsListChanged` subscription of one connection. Returns
/// `None` unless the connection speaks the 2026-07-28 era and the server
/// declares `tools.listChanged`. The subscription ends with the connection or
/// when the server ends it; a later connection opens a new one.
pub(super) fn spawn_tools_listener(
    server: &str,
    peer: rmcp::Peer<RoleClient>,
    era: McpEra,
    stale: &StaleSet,
) -> Option<tokio::task::AbortHandle> {
    if era != McpEra::Modern {
        return None;
    }
    let declared = peer
        .peer_info()
        .is_some_and(|info| info.capabilities.tools.as_ref().and_then(|tools| tools.list_changed) == Some(true));
    if !declared {
        return None;
    }
    let stale = Arc::clone(stale);
    let server = server.to_string();
    let task = tokio::spawn(async move {
        let filter = SubscriptionFilter::builder().tools_list_changed().build();
        let mut subscription = match peer.listen(filter).await {
            Ok(subscription) => subscription,
            Err(error) => {
                let error = redact_provider_text(&error.to_string());
                tracing::warn!(server = %server, error = %error, "MCP tools listener did not start");
                return;
            }
        };
        if subscription.acknowledged().tools_list_changed != Some(true) {
            // An empty acknowledgment (a token without push) keeps no stream;
            // dropping the subscription cancels it on the wire.
            tracing::debug!(server = %server, "MCP tools listener not acknowledged; closed");
            return;
        }
        while let Ok(Some(notification)) = subscription.next().await {
            if matches!(notification, ServerNotification::ToolListChangedNotification(_)) {
                stale.lock().insert(server.clone());
            }
        }
    });
    Some(task.abort_handle())
}

/// Whether a connection can still carry a call.
pub(super) fn is_live(client: &McpClient) -> bool {
    !client.is_closed() && !client.is_transport_closed()
}

fn tools_map(tools: Vec<rmcp::model::Tool>) -> HashMap<String, DiscoveredToolMeta> {
    tools
        .into_iter()
        .map(|tool| (tool.name.to_string(), discovered_tool_meta(tool)))
        .collect()
}

impl McpTool {
    /// Whether a stale mark or a passed `ttlMs` deadline asks for a pass.
    pub(super) fn freshness_due(&self) -> bool {
        if !self.stale.lock().is_empty() {
            return true;
        }
        let now = Instant::now();
        self.ttl_due.lock().values().any(|due| *due <= now)
    }

    /// Re-list the stale servers and the servers whose `ttlMs` has passed.
    /// The caller holds the lifecycle write lock, so no session is started
    /// or closed by another refresh meanwhile.
    pub(super) async fn refresh_stale_servers(&self) {
        let now = Instant::now();
        let mut due: HashSet<String> = std::mem::take(&mut *self.stale.lock());
        {
            let mut ttl = self.ttl_due.lock();
            let expired: Vec<String> = ttl
                .iter()
                .filter(|(_, deadline)| **deadline <= now)
                .map(|(name, _)| name.clone())
                .collect();
            for name in expired {
                ttl.remove(&name);
                due.insert(name);
            }
        }
        for server in due {
            self.refresh_one_server(&server).await;
        }
    }

    async fn refresh_one_server(&self, server: &str) {
        if self.retired.lock().contains(server) {
            return;
        }
        let (observed_epoch, config) = {
            let state = self.state.read();
            (
                state.mcp_state_epoch,
                state.effective_config.servers.get(server).cloned(),
            )
        };
        let Some(config) = config else {
            return;
        };

        let listed = match self.live_peer(server).await {
            Some(peer) => {
                let timeout = Duration::from_millis(config.request_timeout_ms);
                match tokio::time::timeout(timeout, peer.list_all_tools_with_cache_hint()).await {
                    Ok(Ok(list)) => Ok((tools_map(list.items), list.ttl_ms, None)),
                    Ok(Err(error)) => Err(anyhow::Error::from(error)),
                    Err(_) => Err(anyhow::anyhow!(
                        "MCP tool listing timed out after {} ms",
                        config.request_timeout_ms
                    )),
                }
            }
            None => Self::discover_server_tools(
                server,
                &config,
                self.launch_root.as_deref(),
                self.full(),
                None,
                &self.http_sessions,
                &self.stale,
            )
            .await
            .map(|(tools, era, ttl_ms)| (tools, ttl_ms, Some(era))),
        };
        match listed {
            Ok((tools, ttl_ms, era)) => self.commit_listing(server, observed_epoch, tools, ttl_ms, era),
            Err(error) => {
                let error = redact_provider_text(&error.to_string());
                tracing::warn!(server, error = %error, "MCP tool freshness refresh failed; the previous list is kept");
                // Back off instead of re-marking: a failing server is retried
                // no sooner than MIN_TTL, while a later list_changed still
                // marks it stale at once.
                let mut ttl = self.ttl_due.lock();
                match Instant::now().checked_add(MIN_TTL) {
                    Some(retry_at) => {
                        ttl.insert(server.to_string(), retry_at);
                    }
                    None => {
                        ttl.remove(server);
                    }
                }
            }
        }
    }

    /// The peer of a live stdio call session or HTTP connection of `server`.
    async fn live_peer(&self, server: &str) -> Option<rmcp::Peer<RoleClient>> {
        let stdio = self.stdio_sessions.lock().get(server).cloned();
        if let Some(slot) = stdio {
            let guard = slot.lock().await;
            if let Some(session) = guard.as_ref().filter(|session| is_live(&session.client)) {
                return Some(session.client.peer().clone());
            }
        }
        let http = self.http_sessions.lock().get(server).cloned();
        if let Some(slot) = http {
            let guard = slot.lock().await;
            if let Some(session) = guard.as_ref().filter(|session| is_live(&session.client)) {
                return Some(session.client.peer().clone());
            }
        }
        None
    }

    /// Commit one server's fresh list. The result is discarded when a
    /// configuration reload ran meanwhile (the epoch moved), when the server
    /// is gone, or when it is retired. A list equal to the held one changes
    /// nothing; a changed list bumps only this server's generation.
    fn commit_listing(
        &self,
        server: &str,
        observed_epoch: u64,
        tools: HashMap<String, DiscoveredToolMeta>,
        ttl_ms: Option<u64>,
        era: Option<McpEra>,
    ) {
        let retired = self.retired.lock().contains(server);
        let mut state = self.state.write();
        if state.mcp_state_epoch != observed_epoch || retired || !state.effective_config.servers.contains_key(server) {
            return;
        }
        if let Some(era) = era {
            state.server_eras.insert(server.to_string(), era);
        }
        {
            let mut ttl = self.ttl_due.lock();
            match ttl_deadline(ttl_ms) {
                Some(deadline) => {
                    ttl.insert(server.to_string(), deadline);
                }
                None => {
                    ttl.remove(server);
                }
            }
        }
        if state.discovered_tools.get(server) == Some(&tools) {
            return;
        }
        let tool_count = tools.len();
        state.discovered_tools.insert(server.to_string(), tools);
        let generation = state.server_generations.entry(server.to_string()).or_insert(0);
        *generation = generation.saturating_add(1);
        state.discovery_errors.remove(server);
        tracing::debug!(server, tools = tool_count, "MCP tool list changed; catalog republished");
        Self::publish_catalog_snapshot(&mut state, self.spec());
    }
}
