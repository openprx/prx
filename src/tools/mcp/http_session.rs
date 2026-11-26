//! The persistent streamable-HTTP session of each HTTP MCP server.
//!
//! PRX keeps one client per HTTP server and uses it for both tool listing and
//! tool calls. The tools it lists fill the client's `x-mcp-header` cache, so
//! the calls that follow mirror those parameters into `Mcp-Param-*` headers.
//!
//! A session is reused while its URL, its configured headers and its
//! autonomy level (`full`, which selects the HTTP client's address resolver)
//! are unchanged and its transport is open. The headers are keyed by a
//! SHA-256 digest, so a credential change opens a new connection. The level is
//! part of the key, so a client built for Full is never used by a call below
//! Full.
//!
//! The slot lock is held only to get or create the client. Calls then run on
//! the shared `Arc` client concurrently. Header values stay in memory and in
//! the encrypted configuration; nothing in this module formats them.

use super::lifecycle::McpEra;
use super::{McpClient, McpTool};
use crate::config::McpServerConfig;
use parking_lot::Mutex;
use reqwest013::header::{HeaderName, HeaderValue};
use rmcp::model::Tool;
use rmcp::service::{ClientInitializeError, ServiceError};
use sha2::Digest;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// One live connection to an HTTP MCP server.
pub(super) struct HttpSession {
    pub(super) url: String,
    pub(super) headers_digest: [u8; 32],
    /// The autonomy level the client was built for; part of the reuse key.
    pub(super) full: bool,
    /// The protocol era the server answered with on connect; kept for the
    /// session's lifetime so a reconnect does not probe again.
    pub(super) era: McpEra,
    pub(super) client: Arc<McpClient>,
}

/// The session slot of one server. Its lock is held only to read or replace
/// the session, never across a call.
pub(super) type HttpSessionSlot = Arc<tokio::sync::Mutex<Option<HttpSession>>>;

/// The slots of every HTTP server of one `McpTool`, by server name.
pub(super) type HttpSessions = Mutex<HashMap<String, HttpSessionSlot>>;

/// Cancel a client when this is its last reference. A client still held by a
/// call in flight is released when that call ends.
async fn close_shared(client: Arc<McpClient>) {
    if let Ok(client) = Arc::try_unwrap(client) {
        let _ = client.cancel().await;
    }
}

/// The digest of the configured headers over their name-sorted pairs. Each
/// part is length-prefixed, so no two header sets share a digest input.
pub(super) fn headers_digest(headers: &BTreeMap<String, String>) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"prx.mcp.http-headers.v1\0");
    for (name, value) in headers {
        for part in [name.as_str(), value.as_str()] {
            hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(part.as_bytes());
        }
    }
    hasher.finalize().into()
}

/// The configured headers as transport headers. Every value is marked
/// sensitive, so the HTTP layer keeps it out of its own debug output. An
/// invalid name or value is refused with an error that names the header,
/// never its value.
fn transport_headers(
    server_name: &str,
    headers: &BTreeMap<String, String>,
) -> anyhow::Result<HashMap<HeaderName, HeaderValue>> {
    let server = server_name.escape_default();
    let mut out = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let header = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            anyhow::anyhow!(
                "MCP server '{server}' has an invalid header name '{}' in mcp.servers.{server}.headers",
                name.escape_default()
            )
        })?;
        let mut header_value = HeaderValue::from_str(value).map_err(|_| {
            anyhow::anyhow!(
                "MCP server '{server}': the value of header '{}' is not a valid HTTP header value",
                name.escape_default()
            )
        })?;
        header_value.set_sensitive(true);
        out.insert(header, header_value);
    }
    Ok(out)
}

/// The error for a 401 or 403 challenge: it names the server and the setting
/// to fill, never the challenge text.
pub(super) fn with_authorization_hint(server_name: &str, error: anyhow::Error) -> anyhow::Error {
    let challenged = error
        .downcast_ref::<ClientInitializeError>()
        .is_some_and(|error| error.auth_challenge().is_some())
        || error
            .downcast_ref::<ServiceError>()
            .is_some_and(|error| error.auth_challenge().is_some());
    if challenged {
        let server = server_name.escape_default();
        anyhow::anyhow!("MCP server '{server}' requires authorization: set mcp.servers.{server}.headers")
    } else {
        error
    }
}

/// The slot of `server_name` in `sessions`, created empty on first use.
pub(super) fn slot_for(sessions: &HttpSessions, server_name: &str) -> HttpSessionSlot {
    Arc::clone(
        sessions
            .lock()
            .entry(server_name.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))),
    )
}

/// The slot of `server_name` with any session it holds closed first, so the
/// next [`acquire`] connects anew (discovery starts from a fresh connection).
pub(super) async fn fresh_slot(sessions: &HttpSessions, server_name: &str) -> HttpSessionSlot {
    let previous = sessions.lock().remove(server_name);
    if let Some(slot) = previous {
        let taken = slot.lock().await.take();
        if let Some(session) = taken {
            close_shared(session.client).await;
        }
    }
    slot_for(sessions, server_name)
}

/// The client to talk to `url` with `server`'s headers at autonomy `full`.
///
/// A session that matches on URL, headers digest and level, and whose
/// transport is open, is reused. Otherwise the old session is replaced by a
/// new connection whose tools are listed once, so the `x-mcp-header` cache is
/// filled; those tools are returned only for a new connection. The slot lock
/// is held while connecting, so concurrent first calls share one connection.
/// Returns the client, the era it answered with and the listed tools.
pub(super) async fn acquire(
    slot: &HttpSessionSlot,
    server_name: &str,
    url: &str,
    server: &McpServerConfig,
    full: bool,
    era: Option<McpEra>,
) -> anyhow::Result<(Arc<McpClient>, McpEra, Option<Vec<Tool>>)> {
    let digest = headers_digest(&server.headers);
    let mut guard = slot.lock().await;
    if let Some(session) = guard.as_ref()
        && session.url == url
        && session.headers_digest == digest
        && session.full == full
        && !session.client.is_closed()
        && !session.client.is_transport_closed()
    {
        return Ok((Arc::clone(&session.client), session.era, None));
    }
    if let Some(stale) = guard.take() {
        close_shared(stale.client).await;
    }
    let headers = transport_headers(server_name, &server.headers)?;
    let (client, learnt) =
        McpTool::connect_http_retrying(server_name, url, headers, server.startup_timeout_ms, full, era)
            .await
            .map_err(|error| with_authorization_hint(server_name, error))?;
    let tools = match client.peer().list_all_tools().await {
        Ok(tools) => tools,
        Err(error) => {
            let _ = client.cancel().await;
            return Err(with_authorization_hint(server_name, error.into()));
        }
    };
    let client = Arc::new(client);
    *guard = Some(HttpSession {
        url: url.to_string(),
        headers_digest: digest,
        full,
        era: learnt,
        client: Arc::clone(&client),
    });
    Ok((client, learnt, Some(tools)))
}

/// Drop the session of `slot` only if it still holds `client`. A transport
/// failure of one call must not tear down a fresh replacement made by another.
pub(super) async fn reset_if_current(slot: &HttpSessionSlot, client: &Arc<McpClient>) {
    let taken = {
        let mut guard = slot.lock().await;
        match guard.as_ref() {
            Some(session) if Arc::ptr_eq(&session.client, client) => guard.take(),
            _ => None,
        }
    };
    if let Some(session) = taken {
        close_shared(session.client).await;
    }
}

/// Close and remove the sessions of `names` (every session when `None`).
/// Returns how many running sessions were closed. The caller holds the
/// lifecycle lock, so no call is in flight through this tool.
pub(super) async fn close_sessions(sessions: &HttpSessions, names: Option<&[String]>) -> usize {
    let slots: Vec<HttpSessionSlot> = {
        let mut map = sessions.lock();
        match names {
            None => map.drain().map(|(_, slot)| slot).collect(),
            Some(names) => names.iter().filter_map(|name| map.remove(name)).collect(),
        }
    };
    let mut closed = 0;
    for slot in slots {
        let session = slot.lock().await.take();
        if let Some(session) = session {
            close_shared(session.client).await;
            closed += 1;
        }
    }
    closed
}

/// Whether a failed call means the transport is gone or stuck, so its session
/// must be reset. Application-level errors keep the session.
pub(super) fn is_transport_failure(error: &ServiceError) -> bool {
    matches!(
        error,
        ServiceError::TransportSend(_)
            | ServiceError::TransportClosed
            | ServiceError::Cancelled { .. }
            | ServiceError::Timeout { .. }
    )
}
