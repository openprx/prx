//! MCP client helpers shared by the stdio and streamable-HTTP paths:
//! discovered-tool metadata, call parameters, HTTP connect, HTTP discovery and
//! HTTP calls. Result rendering lives in `result.rs`.

use super::http_session::{self, HttpSessionSlot, HttpSessions};
use super::lifecycle::{self, McpEra};
use super::{DiscoveredToolMeta, McpClient, McpTool, mcp_http_client};
use crate::config::McpServerConfig;
use anyhow::bail;
use reqwest013::header::{HeaderName, HeaderValue};
use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use std::collections::HashMap;
use serde::Serialize;
use std::time::Duration;

/// The discovered metadata of one tool: description, input schema and the
/// display fields (title, annotations, output-schema presence, icon count).
/// Display fields are never used to gate or route a call.
pub(super) fn discovered_tool_meta(tool: Tool) -> DiscoveredToolMeta {
    DiscoveredToolMeta {
        description: tool.description.map(|v| v.to_string()),
        input_schema: serde_json::to_value(tool.input_schema).ok(),
        title: tool.title,
        annotations: tool.annotations.and_then(|a| serde_json::to_value(a).ok()),
        has_output_schema: tool.output_schema.is_some(),
        icon_count: tool.icons.map_or(0, |icons| icons.len()),
    }
}

impl DiscoveredToolMeta {
    pub(super) fn detail(&self, name: &str) -> McpToolDetail {
        McpToolDetail {
            name: name.to_string(),
            title: self.title.clone(),
            annotations: self.annotations.clone(),
            has_output_schema: self.has_output_schema,
            icon_count: self.icon_count,
        }
    }
}

/// Per-tool metadata shown on the status API.
#[derive(Debug, Clone, Serialize)]
pub struct McpToolDetail {
    pub name: String,
    pub title: Option<String>,
    pub annotations: Option<serde_json::Value>,
    pub has_output_schema: bool,
    pub icon_count: usize,
}

/// The per-tool details of one server, sorted by tool name.
pub(super) fn detail_list(tools: &HashMap<String, DiscoveredToolMeta>) -> Vec<McpToolDetail> {
    let mut details = tools
        .iter()
        .map(|(tool, meta)| meta.detail(tool))
        .collect::<Vec<_>>();
    details.sort_by(|left, right| left.name.cmp(&right.name));
    details
}

impl McpTool {
    pub(super) fn call_tool_params(
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> CallToolRequestParams {
        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        params
    }

    /// Open a streamable-HTTP MCP session to `url`, which the caller has
    /// already passed through [`Self::validate_http_url`], sending `headers`
    /// with every request.
    ///
    /// Both HTTP entry points (discovery and `call_http`) connect here, so
    /// the transport's HTTP client is chosen in one place.
    pub(super) async fn connect_http(
        url: &str,
        headers: HashMap<HeaderName, HeaderValue>,
        startup_timeout_ms: u64,
        full: bool,
        era: Option<McpEra>,
    ) -> anyhow::Result<(McpClient, McpEra)> {
        let transport = StreamableHttpClientTransport::with_client(
            mcp_http_client(full)?,
            StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers),
        );
        lifecycle::start(transport, era, Duration::from_millis(startup_timeout_ms)).await
    }

    /// `connect_http` with at most one retry, as decided by [`lifecycle::retry_era`].
    pub(super) async fn connect_http_retrying(
        server_name: &str,
        url: &str,
        headers: HashMap<HeaderName, HeaderValue>,
        startup_timeout_ms: u64,
        full: bool,
        era: Option<McpEra>,
    ) -> anyhow::Result<(McpClient, McpEra)> {
        match Self::connect_http(url, headers.clone(), startup_timeout_ms, full, era).await {
            Ok(connected) => Ok(connected),
            Err(error) => {
                let Some(next) = lifecycle::retry_era(era, &error) else {
                    return Err(error);
                };
                lifecycle::note_retry(server_name, next);
                Self::connect_http(url, headers, startup_timeout_ms, full, next).await
            }
        }
    }

    /// Discovery opens a fresh session and keeps it: the calls that follow use
    /// the connection that listed the tools.
    pub(super) async fn discover_server_tools_http(
        server_name: &str,
        server: &McpServerConfig,
        full: bool,
        era: Option<McpEra>,
        sessions: &HttpSessions,
    ) -> anyhow::Result<(HashMap<String, DiscoveredToolMeta>, McpEra)> {
        let url = server
            .url
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' uses http but url is missing"))?;

        // ── SSRF protection: block private/local addresses below Full ──
        Self::validate_http_target(url, full)?;

        let slot = http_session::fresh_slot(sessions, server_name).await;
        let (_client, learnt, listed) = http_session::acquire(&slot, server_name, url, server, full, era).await?;
        let Some(listed) = listed else {
            bail!("MCP HTTP session for '{server_name}' listed no tools on connect");
        };

        let tools = listed
            .into_iter()
            .map(|tool| (tool.name.to_string(), discovered_tool_meta(tool)))
            .collect::<HashMap<_, _>>();

        Ok((tools, learnt))
    }

    /// A call uses the server's persistent session. A connect that fails
    /// (after its one retry) drops the kept era, so the next call probes the
    /// server again; the caller writes `era` back. A transport failure or a
    /// timeout drops the session, so the next call connects anew; the timed
    /// out call is not replayed.
    pub(super) async fn call_http(
        server_name: &str,
        server: &McpServerConfig,
        slot: &HttpSessionSlot,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
        full: bool,
        era: &mut Option<McpEra>,
    ) -> anyhow::Result<(bool, String)> {
        let url = server
            .url
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' uses http but url is missing"))?;

        // ── SSRF protection: block private/local addresses below Full ──
        Self::validate_http_target(url, full)?;

        let request_timeout = Duration::from_millis(server.request_timeout_ms);
        // The slot lock is released inside `acquire`; the call runs on the
        // shared client, so concurrent calls to one server do not queue.
        let (client, learnt, _) = match http_session::acquire(slot, server_name, url, server, full, *era).await {
            Ok(connected) => connected,
            Err(error) => {
                *era = None;
                return Err(error);
            }
        };
        *era = Some(learnt);

        let outcome = tokio::time::timeout(
            request_timeout,
            client.call_tool(Self::call_tool_params(tool_name, arguments)),
        )
        .await;
        let result = match outcome {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                if http_session::is_transport_failure(&error) {
                    http_session::reset_if_current(slot, &client).await;
                }
                return Err(http_session::with_authorization_hint(server_name, error.into()));
            }
            Err(_) => {
                http_session::reset_if_current(slot, &client).await;
                bail!(
                    "MCP call timed out after {} ms; HTTP session was reset and the call was not replayed",
                    server.request_timeout_ms
                );
            }
        };

        let value = serde_json::to_value(result)?;
        Ok(Self::extract_call_success_and_output(&value))
    }
}
