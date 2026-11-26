//! Resource access for the model: `mcp_call action=resources` lists a server's
//! resources and resource templates, and `action=read` returns one resource by
//! URI. Reads are live (no PRX cache). They skip the read-only autonomy check
//! and the `SideEffectGate` that `call` runs, because they run no tool.

use super::ATTACH_ONLY;
use super::McpTool;
use super::http_session;
use super::result::render_resource_contents;
use crate::config::McpTransport;
use crate::tools::traits::ToolResult;
use anyhow::bail;
use rmcp::RoleClient;
use rmcp::model::{ReadResourceRequestParams, ReadResourceResponse};
use rmcp::service::{Peer, ServiceError};
use serde_json::{Value, json};
use std::time::Duration;

impl McpTool {
    /// Serve `resources` or `read` for the root `mcp_call` tool.
    pub(super) async fn execute_resource_action(&self, action: &str, args: &Value) -> anyhow::Result<ToolResult> {
        Ok(match self.run_resource_action(action, args).await {
            Ok(output) => ToolResult {
                success: true,
                output,
                error: None,
            },
            // The error may quote a server URL or a transport detail; redact it
            // the same way the call path does.
            Err(err) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(crate::security::redaction::redact_provider_text(&format!("{err:#}"))),
            },
        })
    }

    async fn run_resource_action(&self, action: &str, args: &Value) -> anyhow::Result<String> {
        let server = args
            .get("server")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'server' parameter"))?;
        self.refresh_runtime_state(false).await;
        let peer = self.attach_peer(server).await?;
        if !declares_resources(&peer) {
            bail!("server '{server}' declares no resources");
        }
        if action == "read" {
            let uri = args
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Missing 'uri' parameter"))?;
            let request_timeout = {
                let state = self.state.read();
                Duration::from_millis(Self::resolve_server(&state.effective_config, server)?.request_timeout_ms)
            };
            read_resource(&peer, server, uri, request_timeout).await
        } else {
            list_resources(&peer).await
        }
    }

    /// A live peer for `server`, starting its stdio session or HTTP connection
    /// when none is running. It never calls a tool. Refusals and SSRF checks
    /// match `validate_and_call` and `call_http`.
    pub(super) async fn attach_peer(&self, server: &str) -> anyhow::Result<Peer<RoleClient>> {
        let _lifecycle = self.lifecycle.read().await;
        if self.retired.lock().contains(server) {
            bail!(
                "MCP server '{}' was removed or narrowed by a configuration change; it is no longer started",
                server.escape_default()
            );
        }
        let (config, era) = {
            let state = self.state.read();
            let config = Self::resolve_server(&state.effective_config, server)?.clone();
            (config, state.server_eras.get(server).copied())
        };
        match config.transport {
            McpTransport::Stdio => {
                self.call_stdio(server, &config, ATTACH_ONLY, None).await?;
                self.stdio_peer(server)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("MCP stdio session for '{server}' is not running"))
            }
            McpTransport::Http => {
                let url = config
                    .url
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("MCP server '{server}' uses http but url is missing"))?;
                // SSRF protection: the same target check as `call_http`.
                Self::validate_http_target(url, self.call_full())?;
                let slot = http_session::slot_for(&self.http_sessions, server);
                match http_session::acquire(&slot, server, url, &config, self.call_full(), era).await {
                    Ok((client, learnt, _)) => {
                        self.record_server_era(server, Some(learnt));
                        Ok(client.peer().clone())
                    }
                    Err(error) => {
                        self.record_server_era(server, None);
                        Err(error)
                    }
                }
            }
        }
    }

    /// The peer of the running stdio session of `server`, if any.
    async fn stdio_peer(&self, server: &str) -> Option<Peer<RoleClient>> {
        let slot = self.stdio_sessions.lock().get(server).cloned()?;
        let session = slot.lock().await;
        session.as_ref().map(|active| active.client.peer().clone())
    }
}

fn declares_resources(peer: &Peer<RoleClient>) -> bool {
    peer.peer_info()
        .is_some_and(|info| info.capabilities.resources.is_some())
}

/// Resources and templates as compact JSON. `ttlMs` and `cacheScope` are not
/// shown; reads are live.
async fn list_resources(peer: &Peer<RoleClient>) -> anyhow::Result<String> {
    let resources = peer.list_all_resources_with_cache_hint().await?.items;
    let templates = peer.list_all_resource_templates_with_cache_hint().await?.items;
    let resources = resources
        .iter()
        .map(|item| {
            compact(json!({
                "uri": item.uri,
                "name": item.name,
                "title": item.title,
                "mimeType": item.mime_type,
                "description": item.description,
            }))
        })
        .collect::<Vec<_>>();
    let templates = templates
        .iter()
        .map(|item| {
            compact(json!({
                "uriTemplate": item.uri_template,
                "name": item.name,
                "title": item.title,
                "mimeType": item.mime_type,
                "description": item.description,
            }))
        })
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&json!({
        "resources": resources,
        "resourceTemplates": templates,
    }))?)
}

/// Drop the null fields of one listed item so the output stays compact.
fn compact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.into_iter().filter(|(_, field)| !field.is_null()).collect()),
        other => other,
    }
}

/// Read one resource. Text is returned in full; a blob is a placeholder with
/// its size and no base64 payload. A not-found answer names the URI and does
/// not reset the session.
async fn read_resource(
    peer: &Peer<RoleClient>,
    server: &str,
    uri: &str,
    request_timeout: Duration,
) -> anyhow::Result<String> {
    let response = tokio::time::timeout(
        request_timeout,
        peer.read_resource_once(ReadResourceRequestParams::new(uri)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("MCP resource read timed out after {} ms", request_timeout.as_millis()))?
    .map_err(|error| read_error(uri, error))?;
    match response {
        ReadResourceResponse::Complete(result) if result.contents.is_empty() => Ok("(empty resource)".to_string()),
        ReadResourceResponse::Complete(result) => Ok(result
            .contents
            .iter()
            .map(render_resource_contents)
            .collect::<Vec<_>>()
            .join("\n")),
        ReadResourceResponse::InputRequired(_) => bail!(
            "MCP server '{server}' asked for client input (elicitation/sampling/roots); PRX does not support MCP input requests yet"
        ),
        _ => bail!("MCP server returned a response type PRX does not understand"),
    }
}

/// JSON-RPC "invalid params" (-32602) and the MCP "resource not found" code
/// (-32002) both mean the URI is unknown to the server.
fn read_error(uri: &str, error: ServiceError) -> anyhow::Error {
    match error {
        ServiceError::McpError(ref data) if data.code.0 == -32602 || data.code.0 == -32002 => {
            anyhow::anyhow!("resource not found: {uri}")
        }
        other => other.into(),
    }
}
