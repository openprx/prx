//! MCP client helpers shared by the stdio and streamable-HTTP paths:
//! tool-result extraction, call parameters, HTTP connect, HTTP discovery and
//! HTTP calls.

use super::lifecycle::{self, McpEra};
use super::{DiscoveredToolMeta, McpClient, McpTool, mcp_http_client};
use crate::config::McpServerConfig;
use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use std::collections::HashMap;
use std::time::Duration;

/// The discovered metadata of one tool. Only the description and the input
/// schema are kept; the rest of the definition is not used by PRX.
pub(super) fn discovered_tool_meta(tool: Tool) -> DiscoveredToolMeta {
    DiscoveredToolMeta {
        description: tool.description.map(|v| v.to_string()),
        input_schema: serde_json::to_value(tool.input_schema).ok(),
    }
}

impl McpTool {
    pub(super) fn extract_call_success_and_output(value: &serde_json::Value) -> (bool, String) {
        let is_error = value
            .get("isError")
            .or_else(|| value.get("is_error"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let content = value
            .get("content")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let text = item.get("text").and_then(serde_json::Value::as_str);
                        if text.is_some() {
                            return text.map(ToString::to_string);
                        }

                        if item
                            .get("type")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|t| t.eq_ignore_ascii_case("text"))
                        {
                            return Some(item.to_string());
                        }

                        None
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        let output = if content.is_empty() { value.to_string() } else { content };

        (!is_error, output)
    }

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
    /// already passed through [`Self::validate_http_url`].
    ///
    /// Both HTTP entry points (discovery and `call_http`) connect here, so
    /// the transport's HTTP client is chosen in one place.
    pub(super) async fn connect_http(
        url: &str,
        startup_timeout_ms: u64,
        full: bool,
        era: Option<McpEra>,
    ) -> anyhow::Result<(McpClient, McpEra)> {
        let transport = StreamableHttpClientTransport::with_client(
            mcp_http_client(full)?,
            StreamableHttpClientTransportConfig::with_uri(url),
        );
        lifecycle::start(transport, era, Duration::from_millis(startup_timeout_ms)).await
    }

    pub(super) async fn discover_server_tools_http(
        server_name: &str,
        server: &McpServerConfig,
        full: bool,
        era: Option<McpEra>,
    ) -> anyhow::Result<(HashMap<String, DiscoveredToolMeta>, McpEra)> {
        let url = server
            .url
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' uses http but url is missing"))?;

        // ── SSRF protection: block private/local addresses below Full ──
        Self::validate_http_target(url, full)?;

        let (client, learnt) = Self::connect_http(url, server.startup_timeout_ms, full, era).await?;

        let list = client.peer().list_all_tools().await?;
        let _ = client.cancel().await;

        let tools = list
            .into_iter()
            .map(|tool| (tool.name.to_string(), discovered_tool_meta(tool)))
            .collect::<HashMap<_, _>>();

        Ok((tools, learnt))
    }

    pub(super) async fn call_http(
        server_name: &str,
        server: &McpServerConfig,
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
        // A cached era that no longer fits the server (for example after the
        // server was replaced) is dropped, and the connection is probed again.
        // The error text is not logged: it can quote the URL with userinfo.
        let (client, learnt) = match Self::connect_http(url, server.startup_timeout_ms, full, *era).await {
            Ok(connected) => connected,
            Err(_) if era.is_some() => {
                tracing::warn!(server = server_name, "MCP cached protocol era failed; probing again");
                *era = None;
                Self::connect_http(url, server.startup_timeout_ms, full, None).await?
            }
            Err(error) => return Err(error),
        };
        *era = Some(learnt);

        let result = tokio::time::timeout(
            request_timeout,
            client.call_tool(Self::call_tool_params(tool_name, arguments)),
        )
        .await
        .map_err(|_| anyhow::anyhow!("MCP call timed out after {} ms", server.request_timeout_ms))??;

        let _ = client.cancel().await;
        let value = serde_json::to_value(result)?;
        Ok(Self::extract_call_success_and_output(&value))
    }
}
