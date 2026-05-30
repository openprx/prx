//! MCP/A2A compatibility discovery endpoints.
//!
//! Discovery endpoints are public metadata. Tool invocation is gated by the
//! MCP server enable flag, caller identity, exposed tool allowlist, trusted
//! scope injection, and the target tool's own SideEffectGate checks.

use super::AppState;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use uuid::Uuid;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const PRX_MCP_SERVER_NAME: &str = "prx-runtime";

#[derive(Debug, Serialize)]
pub struct McpListServersResponse {
    servers: Vec<McpServerCard>,
}

#[derive(Debug, Serialize)]
struct McpServerCard {
    name: &'static str,
    protocol_version: &'static str,
    enabled: bool,
    bind: String,
    tools: Vec<McpExposedTool>,
}

#[derive(Debug, Serialize)]
pub struct McpExposedTool {
    name: String,
    risk: &'static str,
    default_exposed: bool,
}

#[derive(Debug, Serialize)]
pub struct McpInitializeResponse {
    protocol_version: &'static str,
    server_info: McpServerInfo,
    capabilities: McpCapabilities,
}

#[derive(Debug, Serialize)]
struct McpServerInfo {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize)]
struct McpCapabilities {
    tools: bool,
    resources: bool,
    prompts: bool,
}

#[derive(Debug, Serialize)]
pub struct A2aIdentityResponse {
    agent_id: String,
    spiffe_id: String,
    enabled: bool,
    bind: String,
    capabilities: A2aCapabilities,
    auth: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct A2aCapabilities {
    tools: Vec<String>,
    modalities: Vec<&'static str>,
    skills: Vec<&'static str>,
}

#[derive(Debug, Deserialize)]
pub struct McpToolCallRequest {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Serialize)]
pub struct McpToolCallResponse {
    content: Vec<McpToolContent>,
    is_error: bool,
    tool_name: String,
    caller: ExternalAgentIdentity,
}

#[derive(Debug, Serialize)]
pub struct McpToolContent {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ExternalAgentIdentity {
    external_subject: String,
    external_issuer: String,
    auth_method: String,
    prx_owner_id: String,
    prx_principal_id: String,
}

pub async fn mcp_list_servers(State(state): State<AppState>) -> Json<McpListServersResponse> {
    Json(build_mcp_list_servers_response(&state.config.lock()))
}

pub async fn mcp_initialize(State(state): State<AppState>) -> Json<McpInitializeResponse> {
    let config = state.config.lock();
    Json(McpInitializeResponse {
        protocol_version: MCP_PROTOCOL_VERSION,
        server_info: McpServerInfo {
            name: PRX_MCP_SERVER_NAME,
            version: env!("CARGO_PKG_VERSION"),
        },
        capabilities: McpCapabilities {
            tools: !config.mcp_server.exposed_tools.is_empty(),
            resources: true,
            prompts: false,
        },
    })
}

pub async fn mcp_tools_list(State(state): State<AppState>) -> Json<Vec<McpExposedTool>> {
    Json(exposed_tools(&state.config.lock()))
}

pub async fn mcp_tools_call(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<McpToolCallRequest>,
) -> Result<Json<McpToolCallResponse>, (StatusCode, Json<Value>)> {
    let config = state.config.lock().clone();
    if !(config.modules.mcp_server && config.mcp_server.enabled) {
        return Err(json_error(StatusCode::FORBIDDEN, "mcp server tool calls are disabled"));
    }

    let identity = derive_external_agent_identity(&headers, &config).ok_or_else(|| {
        json_error(
            StatusCode::UNAUTHORIZED,
            "mcp tool call requires bearer, spiffe, or mtls caller identity",
        )
    })?;

    if !config.mcp_server.exposed_tools.iter().any(|tool| tool == &request.name) {
        return Err(json_error(StatusCode::FORBIDDEN, "tool is not exposed through mcp"));
    }

    if let Err(error) =
        upsert_agent_identity_binding(&config.workspace_dir, &identity, &config.mcp_server.exposed_tools)
    {
        tracing::warn!(error = %error, "mcp agent identity binding upsert skipped");
    }

    let Some(tool) = state
        .tools_registry
        .iter()
        .find(|tool| tool.supports_name(&request.name))
    else {
        return Err(json_error(StatusCode::NOT_FOUND, "exposed tool is not available"));
    };

    let args = inject_trusted_scope(request.arguments, &identity);
    let result = tool
        .execute_named(&request.name, args)
        .await
        .map_err(|error| json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;

    Ok(Json(McpToolCallResponse {
        content: vec![McpToolContent {
            kind: "text",
            text: if result.success {
                result.output
            } else {
                result.error.clone().unwrap_or(result.output)
            },
        }],
        is_error: !result.success,
        tool_name: request.name,
        caller: identity,
    }))
}

pub async fn a2a_identity(State(state): State<AppState>) -> Json<A2aIdentityResponse> {
    Json(build_a2a_identity_response(&state.config.lock()))
}

pub async fn a2a_discover(State(state): State<AppState>) -> Json<A2aIdentityResponse> {
    Json(build_a2a_identity_response(&state.config.lock()))
}

fn build_mcp_list_servers_response(config: &crate::config::Config) -> McpListServersResponse {
    McpListServersResponse {
        servers: vec![McpServerCard {
            name: PRX_MCP_SERVER_NAME,
            protocol_version: MCP_PROTOCOL_VERSION,
            enabled: config.modules.mcp_server && config.mcp_server.enabled,
            bind: config.mcp_server.bind.clone(),
            tools: exposed_tools(config),
        }],
    }
}

fn build_a2a_identity_response(config: &crate::config::Config) -> A2aIdentityResponse {
    A2aIdentityResponse {
        agent_id: config.a2a.agent_id.clone(),
        spiffe_id: config.a2a.spiffe_id.clone(),
        enabled: config.modules.a2a && config.a2a.enabled,
        bind: config.a2a.bind.clone(),
        capabilities: A2aCapabilities {
            tools: config.mcp_server.exposed_tools.clone(),
            modalities: vec!["text"],
            skills: vec!["memory_qa", "document_grounding"],
        },
        auth: vec!["bearer-jwt", "mtls", "spiffe"],
    }
}

fn exposed_tools(config: &crate::config::Config) -> Vec<McpExposedTool> {
    config
        .mcp_server
        .exposed_tools
        .iter()
        .map(|name| McpExposedTool {
            name: name.clone(),
            risk: "readonly",
            default_exposed: true,
        })
        .collect()
}

fn derive_external_agent_identity(
    headers: &HeaderMap,
    config: &crate::config::Config,
) -> Option<ExternalAgentIdentity> {
    let workspace_id = config.workspace_dir.to_string_lossy();
    if let Some(spiffe_id) = header_value(headers, "x-spiffe-id") {
        return Some(external_identity_for(
            "spiffe",
            "spiffe",
            &spiffe_id,
            workspace_id.as_ref(),
        ));
    }
    if let Some(subject) =
        header_value(headers, "x-client-cert-subject").or_else(|| header_value(headers, "x-forwarded-client-cert"))
    {
        return Some(external_identity_for("mtls", "mtls", &subject, workspace_id.as_ref()));
    }
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let token_hash = short_sha256(token);
        return Some(external_identity_for(
            "bearer-jwt",
            "bearer-jwt",
            &format!("bearer:{token_hash}"),
            workspace_id.as_ref(),
        ));
    }
    (!config.mcp_server.require_auth)
        .then(|| external_identity_for("anonymous", "none", "anonymous:mcp", workspace_id.as_ref()))
}

fn external_identity_for(
    external_issuer: &str,
    auth_method: &str,
    external_subject: &str,
    workspace_id: &str,
) -> ExternalAgentIdentity {
    let principal_id = format!("agent:{external_issuer}:{external_subject}");
    ExternalAgentIdentity {
        external_subject: external_subject.to_string(),
        external_issuer: external_issuer.to_string(),
        auth_method: auth_method.to_string(),
        prx_owner_id: format!("owner:{workspace_id}:{principal_id}"),
        prx_principal_id: principal_id,
    }
}

fn header_value(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn short_sha256(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest.get(..8).unwrap_or(digest.as_slice()))
}

fn inject_trusted_scope(args: Value, identity: &ExternalAgentIdentity) -> Value {
    let mut args = match args {
        Value::Object(map) => Value::Object(map),
        _ => serde_json::json!({}),
    };
    if let Some(map) = args.as_object_mut() {
        map.insert("_zc_scope_trusted".to_string(), Value::Bool(true));
        map.insert(
            "_zc_scope".to_string(),
            serde_json::json!({
                "owner_id": identity.prx_owner_id,
                "principal_id": identity.prx_principal_id,
                "channel": "mcp",
                "sender": identity.external_subject,
                "session_key": format!("mcp:{}", identity.external_subject),
                "visibility": "workspace",
            }),
        );
    }
    args
}

fn upsert_agent_identity_binding(
    workspace_dir: &Path,
    identity: &ExternalAgentIdentity,
    capabilities: &[String],
) -> anyhow::Result<()> {
    let db_path = workspace_dir.join("memory").join("brain.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_identity_bindings (
            binding_id        TEXT PRIMARY KEY,
            external_subject  TEXT NOT NULL,
            external_issuer   TEXT NOT NULL,
            auth_method       TEXT NOT NULL,
            prx_owner_id      TEXT NOT NULL,
            prx_principal_id  TEXT NOT NULL,
            capabilities      TEXT NOT NULL,
            expires_at        TEXT,
            created_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            last_used_at      TEXT,
            UNIQUE (external_issuer, external_subject, auth_method)
        );
        CREATE INDEX IF NOT EXISTS idx_agent_bindings_lookup
            ON agent_identity_bindings(external_issuer, external_subject);
        CREATE INDEX IF NOT EXISTS idx_agent_bindings_owner
            ON agent_identity_bindings(prx_owner_id);",
    )?;
    let now = chrono::Utc::now().to_rfc3339();
    let capabilities = serde_json::to_string(capabilities)?;
    conn.execute(
        "INSERT INTO agent_identity_bindings (
            binding_id, external_subject, external_issuer, auth_method,
            prx_owner_id, prx_principal_id, capabilities, expires_at, created_at, last_used_at
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?8)
         ON CONFLICT(external_issuer, external_subject, auth_method) DO UPDATE SET
            prx_owner_id = excluded.prx_owner_id,
            prx_principal_id = excluded.prx_principal_id,
            capabilities = excluded.capabilities,
            last_used_at = excluded.last_used_at",
        params![
            Uuid::new_v4().to_string(),
            identity.external_subject,
            identity.external_issuer,
            identity.auth_method,
            identity.prx_owner_id,
            identity.prx_principal_id,
            capabilities,
            now
        ],
    )?;
    Ok(())
}

fn json_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use tempfile::TempDir;

    #[test]
    fn mcp_list_servers_returns_single_prx_server_by_default() {
        let response = build_mcp_list_servers_response(&crate::config::Config::default());

        assert_eq!(response.servers.len(), 1);
        let server = response.servers.first().expect("server card should exist");
        assert_eq!(server.name, PRX_MCP_SERVER_NAME);
        assert!(!server.enabled);
        assert_eq!(server.tools.len(), 7);
    }

    #[test]
    fn a2a_identity_uses_default_agent_identity() {
        let response = build_a2a_identity_response(&crate::config::Config::default());

        assert_eq!(response.agent_id, "prx-default");
        assert_eq!(response.spiffe_id, "spiffe://prx-local/agent/prx-default");
        assert!(!response.enabled);
        assert_eq!(response.capabilities.modalities, vec!["text"]);
    }

    #[test]
    fn mcp_identity_prefers_spiffe_and_hashes_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer secret-token"));
        let config = crate::config::Config::default();
        let bearer = derive_external_agent_identity(&headers, &config).unwrap();
        assert_eq!(bearer.external_issuer, "bearer-jwt");
        assert!(bearer.external_subject.starts_with("bearer:"));
        assert!(!bearer.external_subject.contains("secret-token"));

        headers.insert("x-spiffe-id", HeaderValue::from_static("spiffe://issuer/agent/a"));
        let spiffe = derive_external_agent_identity(&headers, &config).unwrap();
        assert_eq!(spiffe.external_issuer, "spiffe");
        assert_eq!(spiffe.external_subject, "spiffe://issuer/agent/a");
    }

    #[test]
    fn mcp_tool_args_receive_trusted_owner_scope() {
        let identity = external_identity_for("spiffe", "spiffe", "spiffe://issuer/agent/a", "/tmp/workspace");
        let args = inject_trusted_scope(serde_json::json!({"query": "hello"}), &identity);
        let scope = args
            .get("_zc_scope")
            .and_then(Value::as_object)
            .expect("trusted scope object");

        assert_eq!(args.get("_zc_scope_trusted").and_then(Value::as_bool), Some(true));
        assert_eq!(
            scope.get("owner_id").and_then(Value::as_str),
            Some(identity.prx_owner_id.as_str())
        );
        assert_eq!(scope.get("channel").and_then(Value::as_str), Some("mcp"));
    }

    #[test]
    fn mcp_agent_identity_binding_upserts_sqlite_row() {
        let tmp = TempDir::new().unwrap();
        let identity = external_identity_for("spiffe", "spiffe", "spiffe://issuer/agent/a", "/tmp/workspace");
        upsert_agent_identity_binding(tmp.path(), &identity, &["memory_search".to_string()]).unwrap();
        upsert_agent_identity_binding(tmp.path(), &identity, &["memory_search".to_string()]).unwrap();

        let conn = Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_identity_bindings", [], |row| row.get(0))
            .unwrap();
        let owner_id: String = conn
            .query_row(
                "SELECT prx_owner_id FROM agent_identity_bindings WHERE external_issuer = 'spiffe'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(owner_id, identity.prx_owner_id);
    }
}
