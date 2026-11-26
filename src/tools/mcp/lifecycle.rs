//! Lifecycle of one MCP client connection: which protocol era it speaks and
//! how it is started.
//!
//! PRX prefers protocol 2026-07-28 (`server/discover`, no `initialize`) and
//! falls back to 2025-11-25 (the legacy `initialize` handshake) for servers
//! that only speak the older revision. The era a server answered with is kept
//! by the caller so later connections to that server skip the probe.

use super::McpClient;
use anyhow::Context;
use rmcp::RoleClient;
use rmcp::model::{ClientConfig, Implementation, ProtocolVersion};
use rmcp::service::{ClientInitializeError, ClientLifecycleMode, ClientServiceExt};
use rmcp::transport::IntoTransport;
use std::time::Duration;

/// Time the automatic probe may take on top of the startup timeout. rmcp
/// waits up to this long for `server/discover` before it falls back to the
/// legacy handshake, so a legacy server that never answers the probe still
/// starts within the budget.
const AUTO_PROBE_ALLOWANCE: Duration = Duration::from_secs(10);

/// The protocol era a server speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum McpEra {
    Modern,
    Legacy,
}

/// The era to try on a second attempt after a first attempt with `first` failed
/// with `error`, or `None` when the failure is final.
///
/// - A kept era that fails is dropped and the server is probed afresh.
/// - A probe whose `server/discover` reply is not a valid `DiscoverResult`
///   (rmcp reports it as `ExpectedInitResult`) falls back to the legacy
///   handshake (2025-11-25), so a legacy server that answers unknown methods
///   with a plain result is still reachable.
///
/// Transport, timeout and authentication failures are never retried here.
pub(super) fn retry_era(first: Option<McpEra>, error: &anyhow::Error) -> Option<Option<McpEra>> {
    match first {
        Some(_) => Some(None),
        None if matches!(
            error.downcast_ref::<ClientInitializeError>(),
            Some(ClientInitializeError::ExpectedInitResult(_))
        ) =>
        {
            Some(Some(McpEra::Legacy))
        }
        None => None,
    }
}

/// Log a second attempt. Only the server name is logged, never the reply or URL.
pub(super) fn note_retry(server_name: &str, next: Option<McpEra>) {
    match next {
        Some(McpEra::Legacy) => tracing::info!(
            server = server_name,
            "MCP server did not answer server/discover with a modern result; using the legacy handshake"
        ),
        _ => tracing::warn!(server = server_name, "MCP cached protocol era failed; probing again"),
    }
}

/// The client identity PRX presents to every MCP server.
pub(super) fn client_config() -> ClientConfig {
    let mut info = ClientConfig::default();
    info.client_info = Implementation::new("openprx", env!("CARGO_PKG_VERSION"));
    info
}

/// Start a client over `transport`.
///
/// `era` is the era learnt for this server earlier, or `None` when unknown:
/// - `None` probes with `server/discover` and falls back to `initialize`
///   (requesting 2025-11-25) when the server is legacy;
/// - `Some(Modern)` sends `server/discover` only;
/// - `Some(Legacy)` sends `initialize` only.
///
/// Returns the running client and the era the server answered with.
pub(super) async fn start<T, E, A>(
    transport: T,
    era: Option<McpEra>,
    startup_timeout: Duration,
) -> anyhow::Result<(McpClient, McpEra)>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let preferred_versions = vec![ProtocolVersion::V_2026_07_28];
    let (lifecycle, budget) = match era {
        None => (
            ClientLifecycleMode::Auto {
                preferred_versions,
                legacy_version: Some(ProtocolVersion::V_2025_11_25),
            },
            startup_timeout.saturating_add(AUTO_PROBE_ALLOWANCE),
        ),
        Some(McpEra::Modern) => (ClientLifecycleMode::Discover { preferred_versions }, startup_timeout),
        Some(McpEra::Legacy) => (ClientLifecycleMode::Initialize, startup_timeout),
    };

    // The legacy `initialize` request carries the protocol version of the
    // client's own info, so a legacy start names 2025-11-25 there.
    let mut info = client_config();
    if era == Some(McpEra::Legacy) {
        info.protocol_version = ProtocolVersion::V_2025_11_25;
    }

    let client = tokio::time::timeout(budget, info.serve_with_lifecycle(transport, lifecycle))
        .await
        .map_err(|_| anyhow::anyhow!("MCP startup timed out after {} ms", budget.as_millis()))??;

    let server_info = client
        .peer()
        .peer_info()
        .context("MCP server completed startup without reporting its protocol version")?;
    let learnt = if server_info.protocol_version.has_initialize() {
        McpEra::Legacy
    } else {
        McpEra::Modern
    };
    Ok((client, learnt))
}
