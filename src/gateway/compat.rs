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
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
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

pub async fn a2a_identity(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<A2aIdentityResponse>, (StatusCode, Json<Value>)> {
    let config = state.config.lock();
    if !peer_issuer_allowed(&headers, &config.a2a) {
        return Err(json_error(
            StatusCode::FORBIDDEN,
            "a2a peer issuer is not in the allow-list",
        ));
    }
    Ok(Json(build_a2a_identity_response(&config)))
}

pub async fn a2a_discover(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<A2aIdentityResponse>, (StatusCode, Json<Value>)> {
    let config = state.config.lock();
    if !peer_issuer_allowed(&headers, &config.a2a) {
        return Err(json_error(
            StatusCode::FORBIDDEN,
            "a2a peer issuer is not in the allow-list",
        ));
    }
    Ok(Json(build_a2a_identity_response(&config)))
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
        // A bearer token is only honored when it passes full JWT verification
        // (signature + iss/aud/exp + asymmetric algorithm whitelist). If the
        // server has no JWT verification configured, or the token is invalid,
        // the bearer is rejected — arbitrary tokens are never accepted.
        return match verify_bearer_jwt(token, &config.mcp_server) {
            Ok(claims) => Some(external_identity_for(
                &claims.issuer,
                "bearer-jwt",
                &claims.subject,
                workspace_id.as_ref(),
            )),
            Err(error) => {
                tracing::warn!(error = %error, "mcp bearer jwt rejected");
                None
            }
        };
    }
    (!config.mcp_server.require_auth)
        .then(|| external_identity_for("anonymous", "none", "anonymous:mcp", workspace_id.as_ref()))
}

/// Minimal verified subset of the JWT claim set used to derive caller identity.
#[derive(Debug)]
struct VerifiedBearerClaims {
    issuer: String,
    subject: String,
}

#[derive(Debug, Deserialize)]
struct BearerJwtClaims {
    #[serde(default)]
    iss: Option<String>,
    #[serde(default)]
    sub: Option<String>,
}

/// Error describing why a bearer JWT was not accepted. Carries no token
/// material so it is safe to log.
#[derive(Debug)]
enum BearerJwtError {
    NotConfigured,
    NoAcceptedAlgorithm,
    UntrustedAlgorithm(Algorithm),
    MissingKid,
    UnknownKid,
    KeyMaterial(String),
    InvalidJwks,
    Verification(String),
    MissingSubject,
}

impl std::fmt::Display for BearerJwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "bearer jwt verification is not configured (no issuer)"),
            Self::NoAcceptedAlgorithm => {
                write!(f, "no asymmetric algorithm is configured in jwt_algorithms")
            }
            Self::UntrustedAlgorithm(alg) => {
                write!(f, "token algorithm {alg:?} is not in the asymmetric whitelist")
            }
            Self::MissingKid => write!(f, "jwks is configured but the token header has no kid"),
            Self::UnknownKid => write!(f, "token kid does not match any configured jwk"),
            Self::KeyMaterial(reason) => write!(f, "signing key material is invalid: {reason}"),
            Self::InvalidJwks => write!(f, "configured jwks document is not valid json"),
            Self::Verification(reason) => write!(f, "jwt verification failed: {reason}"),
            Self::MissingSubject => write!(f, "verified jwt has no sub claim"),
        }
    }
}

impl std::error::Error for BearerJwtError {}

/// Only asymmetric algorithms are ever accepted. Symmetric (`HS*`) algorithms
/// and the `none` algorithm are rejected to prevent key-confusion and
/// unsigned-token attacks.
fn asymmetric_algorithm(name: &str) -> Option<Algorithm> {
    match name {
        "RS256" => Some(Algorithm::RS256),
        "RS384" => Some(Algorithm::RS384),
        "RS512" => Some(Algorithm::RS512),
        "PS256" => Some(Algorithm::PS256),
        "PS384" => Some(Algorithm::PS384),
        "PS512" => Some(Algorithm::PS512),
        "ES256" => Some(Algorithm::ES256),
        "ES384" => Some(Algorithm::ES384),
        "EdDSA" => Some(Algorithm::EdDSA),
        _ => None,
    }
}

fn verify_bearer_jwt(
    token: &str,
    config: &crate::config::McpServerRuntimeConfig,
) -> Result<VerifiedBearerClaims, BearerJwtError> {
    // Verification requires at minimum a configured issuer. Without it we have
    // no trust anchor and must reject rather than accept arbitrary tokens.
    let Some(issuer) = config.jwt_issuer.as_deref().filter(|v| !v.is_empty()) else {
        return Err(BearerJwtError::NotConfigured);
    };

    // Build the asymmetric algorithm whitelist, silently dropping any
    // symmetric/none entries an operator may have placed in the list.
    let allowed: Vec<Algorithm> = config
        .jwt_algorithms
        .iter()
        .filter_map(|name| asymmetric_algorithm(name))
        .collect();
    if allowed.is_empty() {
        return Err(BearerJwtError::NoAcceptedAlgorithm);
    }

    let header = decode_header(token).map_err(|e| BearerJwtError::Verification(e.to_string()))?;
    if !allowed.contains(&header.alg) {
        return Err(BearerJwtError::UntrustedAlgorithm(header.alg));
    }

    let decoding_key = resolve_decoding_key(config, header.kid.as_deref())?;

    let mut validation = Validation::new(header.alg);
    validation.algorithms = allowed;
    validation.validate_exp = true;
    validation.set_issuer(&[issuer]);
    if let Some(audience) = config.jwt_audience.as_deref().filter(|v| !v.is_empty()) {
        validation.set_audience(&[audience]);
    } else {
        // No audience configured: do not require an `aud` claim to be present.
        validation.validate_aud = false;
    }

    let token_data = decode::<BearerJwtClaims>(token, &decoding_key, &validation)
        .map_err(|e| BearerJwtError::Verification(e.to_string()))?;

    let subject = token_data
        .claims
        .sub
        .filter(|s| !s.is_empty())
        .ok_or(BearerJwtError::MissingSubject)?;
    let issuer = token_data.claims.iss.unwrap_or_else(|| issuer.to_string());

    Ok(VerifiedBearerClaims { issuer, subject })
}

/// Resolve the signing key from a configured JWKS document (matched by `kid`)
/// or, failing that, a configured PEM public key.
fn resolve_decoding_key(
    config: &crate::config::McpServerRuntimeConfig,
    kid: Option<&str>,
) -> Result<DecodingKey, BearerJwtError> {
    if let Some(jwks_raw) = config.jwt_jwks.as_deref().filter(|v| !v.trim().is_empty()) {
        let jwks: JwkSet = serde_json::from_str(jwks_raw).map_err(|_| BearerJwtError::InvalidJwks)?;
        let kid = kid.ok_or(BearerJwtError::MissingKid)?;
        let jwk = jwks.find(kid).ok_or(BearerJwtError::UnknownKid)?;
        return DecodingKey::from_jwk(jwk).map_err(|e| BearerJwtError::KeyMaterial(e.to_string()));
    }
    if let Some(pem) = config.jwt_public_key_pem.as_deref().filter(|v| !v.trim().is_empty()) {
        // jsonwebtoken validates the PEM family against the token algorithm at
        // decode time, so any of RSA/EC/Ed PEMs are accepted here.
        return DecodingKey::from_rsa_pem(pem.as_bytes())
            .or_else(|_| DecodingKey::from_ec_pem(pem.as_bytes()))
            .or_else(|_| DecodingKey::from_ed_pem(pem.as_bytes()))
            .map_err(|e| BearerJwtError::KeyMaterial(e.to_string()));
    }
    Err(BearerJwtError::NotConfigured)
}

/// Validate that an inbound A2A peer's asserted issuer is on the configured
/// allow-list. An empty allow-list means no external peer issuer is trusted,
/// so any presented peer issuer is rejected.
fn peer_issuer_allowed(headers: &HeaderMap, config: &crate::config::A2aConfig) -> bool {
    let Some(issuer) = header_value(headers, "x-a2a-peer-issuer")
        .or_else(|| header_value(headers, "x-peer-issuer"))
        .or_else(|| header_value(headers, "x-spiffe-id"))
    else {
        // No peer issuer asserted: this is discovery-only metadata access, not
        // a peer handoff, so it is allowed.
        return true;
    };
    let allow: HashSet<&str> = config.allowed_peer_issuers.iter().map(String::as_str).collect();
    allow.contains(issuer.as_str())
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

    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde::Serialize;

    const TEST_RSA_PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC2XB5X630ehklg\n\
cK2tKCnNj1BhEaeNNU3+ElKnQK2v8r4ctb28JtgiTIe5SmGpp2CXSwuePEJp/KJ5\n\
tbL2wlcOfBEgV1R/zJ9dpR5OgZHRSdwcpHQ1WnGwyU2PA7XDAczHUM7bDjFZee02\n\
N7lSvZKJFRgInPm2i6CKZpD9eFYfACl0fFkZvlWe4cH5Qu4L1e6A551vaFzE7nAC\n\
5BVeTy+4glWzxqriXoBvfDwvA0fkP93b4kuDfjHQIl5UYTIKJldCSgX9fKcyeQQK\n\
mQM0uD7jLBNup3z52Le8SOqUC2+porAx17NbglVNSSTFo4mUlcKtAos/ZZKkG3Cw\n\
t4XmItGrAgMBAAECggEABD+7OUM0ZByu4v9dHJa9+psnPUvDAqhGS/AzWtBH+7yF\n\
L3gQlqy1Bn+P/gI8qk5LEttdxu8SgROw2XMhW1yF2MGNSJqgTmX/JJhvT1b3hc/Z\n\
wBUY3BFiasMV+ooUlxmYouBAIZyAC9yqaUP9Hm9qVaVXLQQzUUyJjg/7utwt7YTy\n\
Xyh9uGKWeE/LG+7m8SR1fXq5Lwf/1SgPBb2oVoO8Dpn2DnAJ/JpxCbulSu5GAzSs\n\
AyjEuLW9Zalqh+3hTvWXtUzT3KHuX4elDhmv9vu5UbZGrjMPC9IEI6WdQu4qO3fw\n\
D99RkuLZanhqw9FgJDQhDghKUJTuf5MIShJyfHP36QKBgQDfwt+HHcYSeomcXJLm\n\
Us6t5bSvv6ZMaODyr4LEK+r6d8BfxFF4g0B0RIBotT5krEyFv4OY8cCenNMmFFXu\n\
FDK5XNXZJQdpX2u18PxhY4eYK7JPaW76F4rUZRB77NavZNwh8GOYZhWF6z5E9EOW\n\
E9VLs31T3OJSKiiQsWpIjfozVQKBgQDQojY9TOSBBXPn9our1/nm7KP6+NKELcy/\n\
PtAetKR9SSMWOYrrhBiTQ4NzJgRvoMi5iw4UD5FMN7MbMGI8Rpt9OQVVARBQ2+8y\n\
pforWDMdgbppk+8KMm4VW0kLezXlt0BY+aLN+TXFRTwfpxqNI9Q0OStMXMV1tDOc\n\
P+QHtqLw/wKBgQCP7p4yH6jFQiU6eyHTHfjsSxHK6xBhniT0dok6/rULn/QSpglx\n\
55uSLm4a7FrSDzK55dMUko1AecgoenQ7zKpEKb81CmiWE1cJlZYCXy9dZt4vzrYg\n\
EPywWsIbtODzuYEQI70szp2RoxxO5oCDDQbqxu4a/75k89FcIYMoYItMcQKBgCN4\n\
UIxfdEHTMX9wVRKkJ2JxPPfAMdozBypEfZGa2JRMSODQa6Pa02rGAaUkA4EO7tFM\n\
qNoUQ3mXxqWKtkjVID5L4XIwOhvlKGeN/Fg+KIKNuamVcwBizoBnAqYnDmS9oPz/\n\
hARMqC2ftbcT69mvC7bNOWVEKHX4awXXfucoz871AoGASv++umarnx2XAOPR+4pd\n\
Dv3qazMWKWp+P6Lh4BxhJqKnmcicyaEWf7ermG29tjLFvTyCTWBz/wtRKyS+PNSg\n\
LKi6p/oD7FKDLwRlNR14bCfuo57HplU0N3/xVkwOCBq7+u/tj8aUxQQa25Tgs1BY\n\
2EPW+JVwoGPd+e3y/0uZ90o=\n\
-----END PRIVATE KEY-----\n";

    const TEST_RSA_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAtlweV+t9HoZJYHCtrSgp\n\
zY9QYRGnjTVN/hJSp0Ctr/K+HLW9vCbYIkyHuUphqadgl0sLnjxCafyiebWy9sJX\n\
DnwRIFdUf8yfXaUeToGR0UncHKR0NVpxsMlNjwO1wwHMx1DO2w4xWXntNje5Ur2S\n\
iRUYCJz5tougimaQ/XhWHwApdHxZGb5VnuHB+ULuC9XugOedb2hcxO5wAuQVXk8v\n\
uIJVs8aq4l6Ab3w8LwNH5D/d2+JLg34x0CJeVGEyCiZXQkoF/XynMnkECpkDNLg+\n\
4ywTbqd8+di3vEjqlAtvqaKwMdezW4JVTUkkxaOJlJXCrQKLP2WSpBtwsLeF5iLR\n\
qwIDAQAB\n\
-----END PUBLIC KEY-----\n";

    const TEST_RSA_N: &str = "tlweV-t9HoZJYHCtrSgpzY9QYRGnjTVN_hJSp0Ctr_K-HLW9vCbYIkyHuUphqadgl0sLnjxCafyiebWy9sJXDnwRIFdUf8yfXaUeToGR0UncHKR0NVpxsMlNjwO1wwHMx1DO2w4xWXntNje5Ur2SiRUYCJz5tougimaQ_XhWHwApdHxZGb5VnuHB-ULuC9XugOedb2hcxO5wAuQVXk8vuIJVs8aq4l6Ab3w8LwNH5D_d2-JLg34x0CJeVGEyCiZXQkoF_XynMnkECpkDNLg-4ywTbqd8-di3vEjqlAtvqaKwMdezW4JVTUkkxaOJlJXCrQKLP2WSpBtwsLeF5iLRqw";

    #[derive(Serialize)]
    struct TestClaims {
        iss: String,
        sub: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        aud: Option<String>,
        exp: usize,
    }

    fn future_exp() -> usize {
        usize::try_from(chrono::Utc::now().timestamp() + 3_600).unwrap_or(usize::MAX)
    }

    fn past_exp() -> usize {
        usize::try_from(chrono::Utc::now().timestamp() - 3_600).unwrap_or(0)
    }

    fn sign_rs256(claims: &TestClaims, kid: Option<&str>) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(ToString::to_string);
        let key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_PEM.as_bytes()).expect("test: rsa private pem");
        encode(&header, claims, &key).expect("test: encode rs256")
    }

    fn pem_config() -> crate::config::McpServerRuntimeConfig {
        crate::config::McpServerRuntimeConfig {
            jwt_issuer: Some("https://issuer.example".to_string()),
            jwt_audience: Some("prx-mcp".to_string()),
            jwt_public_key_pem: Some(TEST_RSA_PUBLIC_PEM.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn bearer_without_jwt_config_is_rejected() {
        // Default config has no jwt_issuer: arbitrary bearer tokens MUST NOT
        // produce an identity, even though require_auth is true.
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer arbitrary-token"));
        let config = crate::config::Config::default();
        assert!(derive_external_agent_identity(&headers, &config).is_none());
    }

    #[test]
    fn spiffe_header_still_takes_precedence() {
        let mut headers = HeaderMap::new();
        headers.insert("x-spiffe-id", HeaderValue::from_static("spiffe://issuer/agent/a"));
        let config = crate::config::Config::default();
        let spiffe = derive_external_agent_identity(&headers, &config).expect("test: spiffe identity");
        assert_eq!(spiffe.external_issuer, "spiffe");
        assert_eq!(spiffe.external_subject, "spiffe://issuer/agent/a");
    }

    #[test]
    fn valid_rs256_bearer_pem_is_accepted() {
        let token = sign_rs256(
            &TestClaims {
                iss: "https://issuer.example".to_string(),
                sub: "agent-007".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: future_exp(),
            },
            None,
        );
        let claims = verify_bearer_jwt(&token, &pem_config()).expect("test: valid jwt");
        assert_eq!(claims.issuer, "https://issuer.example");
        assert_eq!(claims.subject, "agent-007");
    }

    #[test]
    fn expired_bearer_is_rejected() {
        let token = sign_rs256(
            &TestClaims {
                iss: "https://issuer.example".to_string(),
                sub: "agent-007".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: past_exp(),
            },
            None,
        );
        let err = verify_bearer_jwt(&token, &pem_config()).expect_err("test: expired rejected");
        assert!(matches!(err, BearerJwtError::Verification(_)));
    }

    #[test]
    fn wrong_issuer_bearer_is_rejected() {
        let token = sign_rs256(
            &TestClaims {
                iss: "https://evil.example".to_string(),
                sub: "agent-007".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: future_exp(),
            },
            None,
        );
        let err = verify_bearer_jwt(&token, &pem_config()).expect_err("test: issuer rejected");
        assert!(matches!(err, BearerJwtError::Verification(_)));
    }

    #[test]
    fn alg_none_token_is_rejected() {
        // Hand-craft an unsigned alg:none token: header.payload. with empty sig.
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = b64.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = b64.encode(br#"{"iss":"https://issuer.example","sub":"agent-007","exp":9999999999}"#);
        let token = format!("{header}.{payload}.");
        let err = verify_bearer_jwt(&token, &pem_config()).expect_err("test: none rejected");
        // alg:none does not parse as a known algorithm header, so it fails at
        // header decode or algorithm whitelist — either way it is rejected.
        assert!(matches!(
            err,
            BearerJwtError::Verification(_) | BearerJwtError::UntrustedAlgorithm(_)
        ));
    }

    #[test]
    fn symmetric_alg_not_in_whitelist() {
        // Even if an operator lists HS256, it is dropped from the allow-list.
        let mut config = pem_config();
        config.jwt_algorithms = vec!["HS256".to_string()];
        let token = sign_rs256(
            &TestClaims {
                iss: "https://issuer.example".to_string(),
                sub: "agent-007".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: future_exp(),
            },
            None,
        );
        let err = verify_bearer_jwt(&token, &config).expect_err("test: hs256 dropped");
        assert!(matches!(err, BearerJwtError::NoAcceptedAlgorithm));
    }

    #[test]
    fn jwks_kid_lookup_verifies_token() {
        let jwks = format!(
            r#"{{"keys":[{{"kty":"RSA","use":"sig","kid":"key-1","alg":"RS256","n":"{TEST_RSA_N}","e":"AQAB"}}]}}"#
        );
        let mut config = pem_config();
        config.jwt_public_key_pem = None;
        config.jwt_jwks = Some(jwks);
        let token = sign_rs256(
            &TestClaims {
                iss: "https://issuer.example".to_string(),
                sub: "agent-jwks".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: future_exp(),
            },
            Some("key-1"),
        );
        let claims = verify_bearer_jwt(&token, &config).expect("test: jwks verify");
        assert_eq!(claims.subject, "agent-jwks");
    }

    #[test]
    fn jwks_unknown_kid_is_rejected() {
        let jwks = format!(
            r#"{{"keys":[{{"kty":"RSA","use":"sig","kid":"key-1","alg":"RS256","n":"{TEST_RSA_N}","e":"AQAB"}}]}}"#
        );
        let mut config = pem_config();
        config.jwt_public_key_pem = None;
        config.jwt_jwks = Some(jwks);
        let token = sign_rs256(
            &TestClaims {
                iss: "https://issuer.example".to_string(),
                sub: "agent-jwks".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: future_exp(),
            },
            Some("key-unknown"),
        );
        let err = verify_bearer_jwt(&token, &config).expect_err("test: unknown kid");
        assert!(matches!(err, BearerJwtError::UnknownKid));
    }

    #[test]
    fn a2a_peer_issuer_allow_list_enforced() {
        let mut config = crate::config::A2aConfig::default();
        config.allowed_peer_issuers = vec!["spiffe://trusted/peer".to_string()];

        // No issuer header -> discovery-only metadata access is allowed.
        let empty = HeaderMap::new();
        assert!(peer_issuer_allowed(&empty, &config));

        // Allowed issuer passes.
        let mut ok = HeaderMap::new();
        ok.insert("x-a2a-peer-issuer", HeaderValue::from_static("spiffe://trusted/peer"));
        assert!(peer_issuer_allowed(&ok, &config));

        // Untrusted issuer is rejected.
        let mut bad = HeaderMap::new();
        bad.insert("x-a2a-peer-issuer", HeaderValue::from_static("spiffe://rogue/peer"));
        assert!(!peer_issuer_allowed(&bad, &config));
    }

    #[test]
    fn a2a_empty_allow_list_rejects_any_asserted_peer() {
        let config = crate::config::A2aConfig::default();
        assert!(config.allowed_peer_issuers.is_empty());
        let mut headers = HeaderMap::new();
        headers.insert("x-spiffe-id", HeaderValue::from_static("spiffe://any/peer"));
        assert!(!peer_issuer_allowed(&headers, &config));
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
