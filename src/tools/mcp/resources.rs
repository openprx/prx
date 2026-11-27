//! Resource access for the model: `mcp_call action=resources` lists a server's
//! resources and resource templates, and `action=read` returns one resource by
//! URI. Reads are live (no PRX cache). They skip the read-only autonomy check
//! and the `SideEffectGate` that `call` runs, because they run no tool.
//!
//! A failed request resets its session exactly as a failed `call` does: a
//! transport failure or a timeout drops the stdio session, or the HTTP
//! connection if it is still the current one.

use super::ATTACH_ONLY;
use super::McpClient;
use super::McpTool;
use super::StdioSessionSlot;
use super::http_session::{self, HttpSessionSlot};
use super::result::render_resource_contents;
use crate::config::McpTransport;
use crate::tools::traits::ToolResult;
use anyhow::bail;
use rmcp::RoleClient;
use rmcp::model::{ReadResourceRequestParams, ReadResourceResponse, Resource, ResourceTemplate};
use rmcp::service::{Peer, ServiceError};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLockReadGuard;

/// A live peer and the session it belongs to, so a failure resets exactly
/// that session and no newer replacement.
///
/// It also holds the lifecycle read guard until the request and any reset on
/// failure are done, as `validate_and_call` does for a tool call. A retirement
/// or refresh therefore waits for the request instead of racing it.
pub(super) struct AttachedPeer<'a> {
    peer: Peer<RoleClient>,
    attachment: Attachment,
    _lifecycle: RwLockReadGuard<'a, ()>,
}

enum Attachment {
    /// The stdio slot and the generation of the session attached to.
    Stdio { slot: StdioSessionSlot, generation: u64 },
    /// The HTTP slot and the connection attached to.
    Http {
        slot: HttpSessionSlot,
        client: Arc<McpClient>,
    },
}

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
        let attached = self.attach_peer(server).await?;
        if !declares_resources(&attached.peer) {
            bail!("server '{server}' declares no resources");
        }
        let request_timeout = {
            let state = self.state.read();
            Duration::from_millis(Self::resolve_server(&state.effective_config, server)?.request_timeout_ms)
        };
        if action == "read" {
            let uri = args
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Missing 'uri' parameter"))?;
            read_resource(&attached, server, uri, request_timeout).await
        } else {
            list_resources(&attached, server, request_timeout).await
        }
    }

    /// A live peer for `server`, starting its stdio session or HTTP connection
    /// when none is running. It never calls a tool. Refusals and SSRF checks
    /// match `validate_and_call` and `call_http`.
    pub(super) async fn attach_peer(&self, server: &str) -> anyhow::Result<AttachedPeer<'_>> {
        let lifecycle = self.lifecycle.read().await;
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
                let (peer, attachment) = self
                    .stdio_attachment(server)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("MCP stdio session for '{server}' is not running"))?;
                Ok(AttachedPeer {
                    peer,
                    attachment,
                    _lifecycle: lifecycle,
                })
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
                match http_session::acquire(&slot, server, url, &config, self.call_full(), era, &self.stale).await {
                    Ok((client, learnt, _)) => {
                        self.record_server_era(server, Some(learnt));
                        Ok(AttachedPeer {
                            peer: client.peer().clone(),
                            attachment: Attachment::Http { slot, client },
                            _lifecycle: lifecycle,
                        })
                    }
                    Err(error) => {
                        self.record_server_era(server, None);
                        Err(error)
                    }
                }
            }
        }
    }

    /// The peer and generation of the running stdio session of `server`, if any.
    async fn stdio_attachment(&self, server: &str) -> Option<(Peer<RoleClient>, Attachment)> {
        let slot = self.stdio_sessions.lock().get(server).cloned()?;
        let (peer, generation) = {
            let session = slot.lock().await;
            let active = session.as_ref()?;
            (active.client.peer().clone(), active.generation)
        };
        Some((peer, Attachment::Stdio { slot, generation }))
    }
}

/// Drop the session behind `attached` if the slot still holds it. For HTTP
/// that is the connection (`reset_if_current`); for stdio it is the session
/// of the same generation. A newer replacement in the slot is left alone.
/// The slot lock is held only for the check and the take, never across a read.
async fn reset_attached(attached: &AttachedPeer<'_>) {
    match &attached.attachment {
        Attachment::Http { slot, client } => http_session::reset_if_current(slot, client).await,
        Attachment::Stdio { slot, generation } => {
            let failed = {
                let mut guard = slot.lock().await;
                match guard.as_ref() {
                    Some(session) if session.generation == *generation => guard.take(),
                    _ => None,
                }
            };
            if let Some(failed) = failed {
                failed.close().await;
            }
        }
    }
}

/// Reset after a failed request if the failure means the transport is gone
/// or stuck. Application-level errors keep the session.
async fn reset_after_failure(attached: &AttachedPeer<'_>, error: &ServiceError) {
    if http_session::is_transport_failure(error) {
        reset_attached(attached).await;
    }
}

/// Resources and templates as compact JSON, under the request timeout.
async fn list_resources(
    attached: &AttachedPeer<'_>,
    server: &str,
    request_timeout: Duration,
) -> anyhow::Result<String> {
    let (resources, templates) = match tokio::time::timeout(request_timeout, list_items(&attached.peer)).await {
        Ok(Ok(items)) => items,
        Ok(Err(error)) => {
            reset_after_failure(attached, &error).await;
            return Err(request_error(server, attached, error.into()));
        }
        Err(_) => {
            reset_attached(attached).await;
            bail!(
                "MCP resource list timed out after {} ms; session was reset",
                request_timeout.as_millis()
            );
        }
    };
    let resources = resources.iter().map(listed_resource).collect::<Vec<_>>();
    let templates = templates.iter().map(listed_template).collect::<Vec<_>>();
    Ok(serde_json::to_string(&json!({
        "resources": resources,
        "resourceTemplates": templates,
    }))?)
}

/// Read one resource under the request timeout. Text is returned in full;
/// a blob is a placeholder with its size and no base64 payload. A not-found
/// answer (`-32002`, or `-32602` for a read of an unknown URI) names the URI
/// and keeps the session.
async fn read_resource(
    attached: &AttachedPeer<'_>,
    server: &str,
    uri: &str,
    request_timeout: Duration,
) -> anyhow::Result<String> {
    let request = attached.peer.read_resource_once(ReadResourceRequestParams::new(uri));
    let response = match tokio::time::timeout(request_timeout, request).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            reset_after_failure(attached, &error).await;
            return Err(match error {
                ServiceError::McpError(ref data) if matches!(data.code.0, -32002 | -32602) => {
                    anyhow::anyhow!("resource not found: {uri}")
                }
                other => request_error(server, attached, other.into()),
            });
        }
        Err(_) => {
            reset_attached(attached).await;
            bail!(
                "MCP resource read timed out after {} ms; session was reset",
                request_timeout.as_millis()
            );
        }
    };
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

fn declares_resources(peer: &Peer<RoleClient>) -> bool {
    peer.peer_info()
        .is_some_and(|info| info.capabilities.resources.is_some())
}

/// One page-complete listing of resources and templates.
async fn list_items(peer: &Peer<RoleClient>) -> Result<(Vec<Resource>, Vec<ResourceTemplate>), ServiceError> {
    let resources = peer.list_all_resources_with_cache_hint().await?.items;
    let templates = peer.list_all_resource_templates_with_cache_hint().await?.items;
    Ok((resources, templates))
}

/// The error returned to the model for a failed request. HTTP failures get the
/// same authorization hint as the call path.
fn request_error(server: &str, attached: &AttachedPeer<'_>, error: anyhow::Error) -> anyhow::Error {
    match attached.attachment {
        Attachment::Http { .. } => http_session::with_authorization_hint(server, error),
        Attachment::Stdio { .. } => error,
    }
}

/// Drop the null fields of one listed item so the output stays compact.
fn compact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.into_iter().filter(|(_, field)| !field.is_null()).collect()),
        other => other,
    }
}

fn listed_resource(item: &Resource) -> Value {
    compact(json!({
        "uri": item.uri,
        "name": item.name,
        "title": item.title,
        "mimeType": item.mime_type,
        "description": item.description,
    }))
}

fn listed_template(item: &ResourceTemplate) -> Value {
    compact(json!({
        "uriTemplate": item.uri_template,
        "name": item.name,
        "title": item.title,
        "mimeType": item.mime_type,
        "description": item.description,
    }))
}
