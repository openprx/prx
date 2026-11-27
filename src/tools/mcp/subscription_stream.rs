//! The `subscriptions/listen` stream of one (server, URI): it opens the stream,
//! counts its updates and reopens it with backoff when it ends. A reconnect
//! may run on a newer registry that serves the same workspace.

use super::McpTool;
use super::lifecycle::McpEra;
use super::resources::{request_error, reset_after_failure, reset_attached};
use super::subscriptions::Hub;
use crate::security::redaction::redact_provider_text;
use anyhow::anyhow;
use chrono::{DateTime, Utc};
use rmcp::RoleClient;
use rmcp::model::{ServerNotification, SubscriptionFilter};
use rmcp::service::Peer;
use rmcp::service::{ServiceError, Subscription};
use rmcp::transport::streamable_http_client::StreamableHttpError;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The first wait after a stream ends, doubled after each failed attempt.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A stream that lived this long counts as healthy: the next outage starts at 1 s again.
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// Why a stream could not be opened.
pub(super) enum OpenFailure {
    /// The server is no longer configured, retired, or no longer speaks a
    /// subscribable protocol. The message names the server only.
    Gone(anyhow::Error),
    /// The acknowledgment does not contain the URI.
    Declined,
    /// A transport, authorization or protocol failure that may clear. The
    /// `Retry-After` of a 429 or 5xx answer is the minimum wait before the next try.
    Failed {
        error: anyhow::Error,
        retry_after: Option<Duration>,
    },
}

/// Open a listen stream for `uri` on the live connection of `server`, after the
/// checks of a first subscribe. The subscription is dropped (and cancelled on the
/// wire) on every failure.
pub(super) async fn open(tool: &McpTool, server: &str, uri: &str) -> Result<Subscription, OpenFailure> {
    let timeout_ms = {
        let state = tool.state.read();
        McpTool::resolve_server(&state.effective_config, server).map(|config| config.request_timeout_ms)
    }
    .map_err(OpenFailure::Gone)?;
    if tool.retired.lock().contains(server) {
        return Err(OpenFailure::Gone(anyhow!(
            "MCP server '{}' was removed or narrowed by a configuration change; it is no longer started",
            server.escape_default()
        )));
    }
    let attached = match tool.attach_peer(server).await {
        Ok(attached) => attached,
        Err(error) => {
            let retry_after = error.chain().find_map(retry_after_of_one);
            return Err(OpenFailure::Failed { error, retry_after });
        }
    };
    let era = tool.state.read().server_eras.get(server).copied();
    if era != Some(McpEra::Modern) {
        return Err(OpenFailure::Gone(anyhow!(
            "server '{server}' speaks 2025-11-25; resource subscriptions need 2026-07-28 (legacy bridge M6e is planned)"
        )));
    }
    if !declares_subscribe(attached.peer()) {
        return Err(OpenFailure::Gone(anyhow!(
            "server '{server}' does not declare resources.subscribe; read the resource with action=read when needed"
        )));
    }
    let peer = attached.peer().clone();
    let filter = SubscriptionFilter::builder()
        .resource_subscriptions(vec![uri.to_string()])
        .build();
    let subscription = match tokio::time::timeout(Duration::from_millis(timeout_ms), peer.listen(filter)).await {
        Ok(Ok(subscription)) => subscription,
        Ok(Err(error)) => {
            let retry_after = retry_after_of_service(&error);
            reset_after_failure(&attached, &error).await;
            let error = request_error(server, &attached, error.into());
            return Err(OpenFailure::Failed { error, retry_after });
        }
        Err(_) => {
            reset_attached(&attached).await;
            return Err(OpenFailure::Failed {
                error: anyhow!("MCP subscription request timed out after {timeout_ms} ms; session was reset"),
                retry_after: None,
            });
        }
    };
    let acknowledged = subscription
        .acknowledged()
        .resource_subscriptions
        .as_ref()
        .is_some_and(|uris| uris.iter().any(|acked| acked == uri));
    if !acknowledged {
        // Dropping the subscription cancels the request on the wire.
        return Err(OpenFailure::Declined);
    }
    Ok(subscription)
}

/// Consume a stream until it ends, then reopen it with backoff. Ends when the
/// URI is declined, the server is gone, or the subscription is aborted by the
/// last unsubscribe.
pub(super) async fn run(hub: Arc<Hub>, key: (String, String), mut subscription: Subscription) {
    let mut backoff = BACKOFF_FIRST;
    loop {
        let started = Instant::now();
        let reason = consume(&hub, &key, &mut subscription).await;
        drop(subscription);
        if started.elapsed() >= STABLE_AFTER {
            backoff = BACKOFF_FIRST;
        }
        let outage_started = Instant::now();
        hub.set_reconnecting(&key, reason);
        let mut retry_after = None;
        subscription = loop {
            tokio::time::sleep(backoff.max(retry_after.unwrap_or_default())).await;
            match reconnect(&hub, &key).await {
                Ok(next) => break next,
                Err(OpenFailure::Declined) => {
                    tracing::debug!(server = %key.0, "MCP resource subscription declined on reconnect");
                    hub.set_declined(&key);
                    return;
                }
                Err(OpenFailure::Gone(error)) => {
                    tracing::warn!(server = %key.0, reason = %redact_provider_text(&format!("{error:#}")), "MCP resource subscription removed");
                    hub.remove_stream(&key);
                    return;
                }
                Err(OpenFailure::Failed {
                    error,
                    retry_after: hint,
                }) => {
                    tracing::debug!(server = %key.0, "MCP resource subscription reconnect failed; retrying");
                    hub.set_reconnecting(&key, redact_provider_text(&format!("{error:#}")));
                    retry_after = hint;
                    backoff = backoff.saturating_mul(2).min(BACKOFF_MAX);
                }
            }
        };
        hub.set_active(&key, outage_started.elapsed());
    }
}

/// Read notifications until the stream ends. Counts updates of the stream's URI
/// and returns the reason the stream ended, for `status`.
async fn consume(hub: &Hub, key: &(String, String), subscription: &mut Subscription) -> String {
    loop {
        match subscription.next().await {
            Ok(Some(ServerNotification::ResourceUpdatedNotification(notification))) => {
                if notification.params.uri == key.1 {
                    hub.record_update(key);
                } else {
                    tracing::debug!(server = %key.0, "MCP resource update for another URI ignored");
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => return "the stream ended".to_string(),
            Err(error) => return redact_provider_text(&format!("the stream failed: {error}")),
        }
    }
}

/// Reopen the stream of `key` on the newest live registry.
async fn reconnect(hub: &Hub, key: &(String, String)) -> Result<Subscription, OpenFailure> {
    let Some(tool) = hub.newest_owner() else {
        return Err(OpenFailure::Failed {
            error: anyhow!("no live MCP registry serves server '{}'", key.0),
            retry_after: None,
        });
    };
    open(&tool, &key.0, &key.1).await
}

/// Whether the server of `peer` declares `resources.subscribe`.
fn declares_subscribe(peer: &Peer<RoleClient>) -> bool {
    peer.peer_info().is_some_and(|info| {
        info.capabilities
            .resources
            .as_ref()
            .and_then(|resources| resources.subscribe)
            == Some(true)
    })
}

/// The `Retry-After` of a failed listen answer, read from the typed HTTP status
/// of the transport error, if the answer carried one.
fn retry_after_of_service(error: &ServiceError) -> Option<Duration> {
    let ServiceError::TransportSend(dynamic) = error else {
        return None;
    };
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(dynamic.error.as_ref());
    while let Some(current) = source {
        if let Some(delay) = retry_after_of_one(current) {
            return Some(delay);
        }
        source = current.source();
    }
    None
}

/// The `Retry-After` delay of one error node, when it is an HTTP status answer.
fn retry_after_of_one(error: &(dyn std::error::Error + 'static)) -> Option<Duration> {
    match error.downcast_ref::<StreamableHttpError<reqwest013::Error>>() {
        Some(StreamableHttpError::HttpStatus(status)) => status.retry_after.as_deref().and_then(parse_retry_after),
        _ => None,
    }
}

/// A `Retry-After` value: delay seconds, or an HTTP date. A date already past
/// is no delay.
fn parse_retry_after(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let when = DateTime::parse_from_rfc2822(raw).ok()?.with_timezone(&Utc);
    Some((when - Utc::now()).to_std().unwrap_or_default())
}
