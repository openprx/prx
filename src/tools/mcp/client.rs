//! MCP client helpers shared by the stdio and streamable-HTTP paths:
//! tool-result extraction, call parameters, HTTP connect, HTTP discovery and
//! HTTP calls.

use super::{DiscoveredToolMeta, McpClient, McpTool};
use crate::config::McpServerConfig;
use anyhow::bail;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use std::collections::HashMap;
use std::time::Duration;

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
    pub(super) async fn connect_http(url: &str, startup_timeout_ms: u64, full: bool) -> anyhow::Result<McpClient> {
        let transport = StreamableHttpClientTransport::with_client(
            mcp_http_client(full)?,
            StreamableHttpClientTransportConfig::with_uri(url),
        );
        tokio::time::timeout(Duration::from_millis(startup_timeout_ms), ().serve(transport))
            .await
            .map_err(|_| anyhow::anyhow!("MCP startup timed out after {startup_timeout_ms} ms"))?
            .map_err(Into::into)
    }

    pub(super) async fn discover_server_tools_http(
        server_name: &str,
        server: &McpServerConfig,
        full: bool,
    ) -> anyhow::Result<HashMap<String, DiscoveredToolMeta>> {
        let url = server
            .url
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' uses http but url is missing"))?;

        // ── SSRF protection: block private/local addresses below Full ──
        Self::validate_http_target(url, full)?;

        let client = Self::connect_http(url, server.startup_timeout_ms, full).await?;

        let list = client.peer().list_all_tools().await?;
        let _ = client.cancel().await;

        let tools = list
            .into_iter()
            .map(|tool| {
                let meta = DiscoveredToolMeta {
                    description: tool.description.map(|v| v.to_string()),
                    input_schema: serde_json::to_value(tool.input_schema).ok(),
                };
                (tool.name.to_string(), meta)
            })
            .collect::<HashMap<_, _>>();

        Ok(tools)
    }

    pub(super) async fn call_http(
        server_name: &str,
        server: &McpServerConfig,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
        full: bool,
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
        let client = Self::connect_http(url, server.startup_timeout_ms, full).await?;

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
