//! Delivery of MCP resource notices into the mailbox of the subscribing
//! session. A notice names the server, the URI and a count (or a reason); it
//! never carries resource contents.

use super::subscriptions::SubscriberKey;
use crate::config::Config;
use crate::messaging::SessionAddr;
use crate::messaging::mailbox::MailboxError;
use crate::messaging::runtime::{MessagingRuntime, RuntimeSender};
use parking_lot::Mutex;
use serde_json::{Value, json};
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
    /// The model-facing text of the notice. The server and the URI are shown
    /// as JSON strings and the suggested calls are serialized JSON, so no
    /// quote, newline or line separator chosen by a server reaches the text
    /// unescaped (a URI cannot forge another call or a message header).
    pub(super) fn text(&self) -> String {
        match self {
            Self::Updated { server, uri, count } => {
                let read = json_text(&json!({"action": "read", "server": server, "uri": uri}));
                let stop = json_text(&json!({"action": "unsubscribe", "server": server, "uri": uri}));
                format!(
                    "[MCP resource updated] server {}, uri {}: {count} update(s) since the last notice. Read it with mcp_call {read}; stop with mcp_call {stop}.",
                    json_str(server),
                    json_str(uri)
                )
            }
            Self::Resumed {
                server,
                uri,
                outage_secs,
            } => format!(
                "[MCP subscription resumed] server {}, uri {}: the stream was down for {outage_secs} s; updates may have been missed. Re-read to catch up.",
                json_str(server),
                json_str(uri)
            ),
            Self::Ended { server, uri, reason } => format!(
                "[MCP subscription ended] server {}, uri {}: {}. Subscribe again to retry.",
                json_str(server),
                json_str(uri),
                one_line(reason)
            ),
        }
    }
}

/// `value` as a JSON string literal.
fn json_str(value: &str) -> String {
    json_text(&Value::from(value))
}

/// `value` serialized as JSON, with the line and paragraph separators that
/// JSON leaves raw (U+0085, U+2028, U+2029) escaped too; the result is still
/// valid JSON with the same value.
fn json_text(value: &Value) -> String {
    let text = value.to_string();
    if !text.contains(['\u{85}', '\u{2028}', '\u{2029}']) {
        return text;
    }
    text.replace('\u{85}', "\\u0085")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// `text` on one line: every control character and line separator becomes a space.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                character
            }
        })
        .collect()
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
    // The running runtime keeps the live configuration of its owner; the
    // registry's adopt-time snapshot only seeds a runtime when none runs yet.
    let runtime = match MessagingRuntime::running(&config.workspace_dir) {
        Some(runtime) => runtime,
        None => {
            MessagingRuntime::for_config(config).map_err(|_| Delivery::Undelivered("messaging runtime unavailable"))?
        }
    };
    let target = runtime.address(&subscriber.owner, &subscriber.session, &subscriber.agent);
    if !runtime.inbound(&target) {
        return Err(Delivery::Undelivered("inbound disabled"));
    }
    match runtime.resolve_scope(&target) {
        Ok(_) => {}
        // Only an unknown or unauthorized receiver counts as gone; a storage
        // failure leaves the subscriber in place.
        Err(MailboxError::Unauthorized) => return Err(Delivery::SessionGone),
        Err(MailboxError::InvalidAddress) => return Err(Delivery::Undelivered("invalid receiver address")),
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
