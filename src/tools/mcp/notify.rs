//! Delivery of MCP resource notices into the mailbox of the subscribing
//! session. A notice names the server, the URI and a count (or a reason); it
//! never carries resource contents.

use super::subscriptions::SubscriberKey;
use crate::config::Config;
use crate::messaging::SessionAddr;
use crate::messaging::mailbox::MailboxError;
use crate::messaging::runtime::{MessagingRuntime, RuntimeSender};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::OnceLock;
use uuid::Uuid;

/// One notice for a subscriber.
pub(super) enum Notice<'a> {
    /// Updates of one (server, URI) since the previous notice.
    Updated { server: &'a str, uri: &'a str, count: u64 },
    /// The stream is back after an outage of `outage_secs`.
    Resumed {
        server: &'a str,
        uri: &'a str,
        outage_secs: u64,
    },
    /// The subscription was terminated by PRX.
    Ended {
        server: &'a str,
        uri: &'a str,
        reason: &'a str,
    },
}

impl Notice<'_> {
    /// The model-facing text of the notice.
    pub(super) fn text(&self) -> String {
        match self {
            Self::Updated { server, uri, count } => format!(
                "[MCP resource updated] server '{server}', uri {uri}: {count} update(s) since the last notice. Read it with mcp_call {{\"action\":\"read\",\"server\":\"{server}\",\"uri\":\"{uri}\"}}; stop with {{\"action\":\"unsubscribe\",\"server\":\"{server}\",\"uri\":\"{uri}\"}}."
            ),
            Self::Resumed {
                server,
                uri,
                outage_secs,
            } => format!(
                "[MCP subscription resumed] server '{server}', uri {uri}: the stream was down for {outage_secs} s; updates may have been missed. Re-read to catch up."
            ),
            Self::Ended { server, uri, reason } => {
                format!("[MCP subscription ended] server '{server}', uri {uri}: {reason}. Subscribe again to retry.")
            }
        }
    }
}

/// The outcome of one delivery attempt.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Delivery {
    Delivered,
    /// Not delivered; the reason is fixed text, never content or identifiers.
    Undelivered(&'static str),
    /// The subscribing session no longer exists for the mailbox.
    SessionGone,
}

/// Send `text` to the subscribing session of `subscriber`. The sender is the
/// notifier session of the owner and agent, and the message continues the
/// chain of `chain_event` (the subscribe call's source event).
pub(super) async fn deliver(
    config: std::sync::Arc<Config>,
    subscriber: SubscriberKey,
    chain_event: Option<String>,
    text: String,
) -> Delivery {
    let prepared =
        crate::runtime::blocking::spawn_blocking(move || prepare(&config, &subscriber, chain_event.as_deref())).await;
    let (sender, target) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(outcome)) => return outcome,
        Err(_) => return Delivery::Undelivered("messaging task failed"),
    };
    match sender
        .send_with_id(target, text, "text/plain".to_owned(), Uuid::new_v4())
        .await
    {
        Ok(_) => Delivery::Delivered,
        Err(_) => Delivery::Undelivered("mailbox refused the notice"),
    }
}

/// The checks that decide the outcome before anything is sent.
fn prepare(
    config: &Config,
    subscriber: &SubscriberKey,
    chain_event: Option<&str>,
) -> Result<(RuntimeSender, SessionAddr), Delivery> {
    if config.memory.backend != "sqlite" {
        return Err(Delivery::Undelivered("messaging needs the SQLite backend"));
    }
    let runtime =
        MessagingRuntime::for_config(config).map_err(|_| Delivery::Undelivered("messaging runtime unavailable"))?;
    let target = runtime.address(&subscriber.owner, &subscriber.session, &subscriber.agent);
    if !runtime.inbound(&target) {
        return Err(Delivery::Undelivered("inbound disabled"));
    }
    match runtime.resolve_scope(&target) {
        Ok(_) => {}
        // Only an unknown or unauthorized receiver counts as gone; a storage
        // failure leaves the subscriber in place.
        Err(MailboxError::Unauthorized) => return Err(Delivery::SessionGone),
        Err(_) => return Err(Delivery::Undelivered("mailbox storage unavailable")),
    }
    let notifier = format!("mcp:notify:{}", subscriber.agent);
    let source = runtime.address(&subscriber.owner, &notifier, &subscriber.agent);
    let sender = runtime
        .bind(source, chain_event)
        .map_err(|_| Delivery::Undelivered("mailbox storage unavailable"))?;
    Ok((sender, target))
}

/// Warn once per (server, reason) and process that a notice was not delivered.
pub(super) fn warn_undelivered(server: &str, reason: &'static str) {
    static WARNED: OnceLock<Mutex<HashSet<(String, &'static str)>>> = OnceLock::new();
    let first = WARNED
        .get_or_init(Mutex::default)
        .lock()
        .insert((server.to_string(), reason));
    if first {
        tracing::warn!(server = %server, reason = reason, "MCP resource notice not delivered");
    }
}
