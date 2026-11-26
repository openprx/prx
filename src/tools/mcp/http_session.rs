//! The persistent streamable-HTTP session of each HTTP MCP server.
//!
//! PRX keeps one client per HTTP server and uses it for both tool listing and
//! tool calls. The tools it lists fill the client's `x-mcp-header` cache, so
//! the calls that follow mirror those parameters into `Mcp-Param-*` headers.
//!
//! A session is reused while its URL and its configured headers are unchanged
//! and its transport is open. The key holds a SHA-256 digest of the headers,
//! so a credential change opens a new connection and the old one is closed.
//! Header values stay in memory and in the encrypted configuration; nothing in
//! this module formats them.

use super::lifecycle::McpEra;
use super::{McpClient, McpTool};
use crate::config::McpServerConfig;
use anyhow::bail;
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
    /// The protocol era the server answered with on connect; kept for the
    /// session's lifetime so a reconnect does not probe again.
    pub(super) era: McpEra,
    pub(super) client: McpClient,
}

/// The session slot of one server. The lock is held for the whole of a
/// connect or a call, so calls to one server run one at a time.
pub(super) type HttpSessionSlot = Arc<tokio::sync::Mutex<Option<HttpSession>>>;

/// The slots of every HTTP server of one `McpTool`, by server name.
pub(super) type HttpSessions = Mutex<HashMap<String, HttpSessionSlot>>;

impl HttpSession {
    pub(super) async fn close(self) {
        let _ = self.client.cancel().await;
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

/// The configured headers as transport headers. An invalid name or value is
/// refused with an error that names the header, never its value.
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
        let header_value = HeaderValue::from_str(value).map_err(|_| {
            anyhow::anyhow!(
                "MCP server '{server}': the value of header '{}' is not a valid HTTP header value",
                name.escape_default()
            )
        })?;
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
    if let Some(slot) = previous
        && let Some(session) = slot.lock().await.take()
    {
        session.close().await;
    }
    slot_for(sessions, server_name)
}

/// Make `slot` hold a live session to `url` with `server`'s headers and return
/// it locked. A session that matches and whose transport is open is reused;
/// otherwise the old one is closed and a new one is connected, with its tools
/// listed once so the `x-mcp-header` cache is filled. The listed tools are
/// returned only for a new connection.
pub(super) async fn acquire<'a>(
    slot: &'a HttpSessionSlot,
    server_name: &str,
    url: &str,
    server: &McpServerConfig,
    full: bool,
    era: Option<McpEra>,
) -> anyhow::Result<(tokio::sync::MutexGuard<'a, Option<HttpSession>>, Option<Vec<Tool>>)> {
    let digest = headers_digest(&server.headers);
    let mut guard = slot.lock().await;
    let reusable = guard.as_ref().is_some_and(|session| {
        session.url == url
            && session.headers_digest == digest
            && !session.client.is_closed()
            && !session.client.is_transport_closed()
    });
    if reusable {
        return Ok((guard, None));
    }
    if let Some(stale) = guard.take() {
        stale.close().await;
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
    *guard = Some(HttpSession {
        url: url.to_string(),
        headers_digest: digest,
        era: learnt,
        client,
    });
    Ok((guard, Some(tools)))
}

/// Close and remove the sessions of `names` (every session when `None`).
/// Returns how many running sessions were closed. Waits for a call in flight
/// on a slot, so the caller holds the lifecycle lock that keeps calls out.
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
            session.close().await;
            closed += 1;
        }
    }
    closed
}

/// Refuse a transport-level failure of a call: the session is dropped so the
/// next call connects anew. Application-level errors keep the session.
pub(super) fn is_transport_failure(error: &ServiceError) -> bool {
    matches!(
        error,
        ServiceError::TransportSend(_)
            | ServiceError::TransportClosed
            | ServiceError::Cancelled { .. }
            | ServiceError::Timeout { .. }
    )
}

/// The session's kept era, or an error when the slot is empty.
pub(super) fn era_of(server_name: &str, session: Option<&HttpSession>) -> anyhow::Result<McpEra> {
    match session {
        Some(session) => Ok(session.era),
        None => bail!("MCP HTTP session for '{server_name}' was not initialized"),
    }
}
