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
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use parking_lot::RwLock;
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const PRX_MCP_SERVER_NAME: &str = "prx-runtime";
/// Agent Card schema version advertised at `/.well-known/agent.json`.
const A2A_CARD_SCHEMA_VERSION: &str = "1.0";
/// JOSE algorithm name for the Ed25519 Agent Card signature.
const A2A_CARD_JWS_ALG: &str = "EdDSA";
/// Default JWKS path served by this gateway when no explicit URI is configured.
const A2A_JWKS_PATH: &str = "/a2a/v1/.well-known/jwks.json";

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

/// Standard A2A Agent Card returned at `/.well-known/agent.json`, aligned with
/// the Linux Foundation A2A Protocol v1.0 discovery schema (RFC 8615 path).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct A2aAgentCard {
    /// Card schema version ("1.0").
    pub schema_version: String,
    /// Stable agent identifier (SPIFFE ID or URN).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Agent version.
    pub version: String,
    /// Human-readable description.
    pub description: String,
    /// Discovery / invocation endpoint advertised to peers.
    pub endpoint: String,
    /// Accepted authentication methods.
    pub authentication: Vec<A2aAuthMethod>,
    /// Declared capabilities.
    pub capabilities: A2aCardCapabilities,
    /// ISO-8601 issuance timestamp.
    pub issued_at: String,
    /// ISO-8601 expiry timestamp.
    pub expires_at: String,
    /// JWS signature over the unsigned card body. `None` only if signing failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<A2aCardSignature>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct A2aAuthMethod {
    #[serde(rename = "type")]
    pub kind: String,
    pub trusted_issuers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct A2aCardCapabilities {
    pub tools: Vec<String>,
    pub modalities: Vec<String>,
    pub skills: Vec<String>,
}

/// JWS (RFC 7515) compact-serialization signature over the canonical card body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct A2aCardSignature {
    /// JOSE algorithm ("EdDSA").
    pub algorithm: String,
    /// Key ID matching the `kid` in the published JWKS.
    pub key_id: String,
    /// JWS compact serialization: `base64url(header).base64url(payload).base64url(sig)`.
    pub jws: String,
    /// JWKS URI where the verification key is published.
    pub jwks_uri: String,
}

/// JWKS document exposing the Agent Card signing public key (RFC 7517).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct A2aJwks {
    pub keys: Vec<A2aJwk>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct A2aJwk {
    pub kty: String,
    pub crv: String,
    pub kid: String,
    #[serde(rename = "use")]
    pub use_: String,
    pub alg: String,
    /// base64url (no pad) of the raw 32-byte Ed25519 public key point.
    pub x: String,
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

    // When a remote JWKS endpoint is configured, refresh its cache off the
    // async runtime before the synchronous identity-derivation path verifies
    // any bearer JWT. This keeps `derive_external_agent_identity` free of
    // network I/O while preserving fail-closed semantics.
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("Bearer "))
    {
        prewarm_remote_jwks(&config.mcp_server).await;
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

/// `GET /.well-known/agent.json` — standard A2A discovery path (RFC 8615).
/// Returns the signed Agent Card. This is public discovery metadata, so it is
/// not gated by the peer-issuer allow-list. `Cache-Control: max-age` mirrors the
/// card TTL so peers honour the same lifetime as `expires_at`.
pub async fn well_known_agent_json(State(state): State<AppState>) -> Response {
    let (card, ttl) = {
        let config = state.config.lock();
        (build_signed_agent_card(&config), config.a2a.card_ttl_seconds)
    };
    let cache = format!("max-age={ttl}");
    ([(header::CACHE_CONTROL, cache)], Json(card)).into_response()
}

/// `GET /a2a/v1/.well-known/jwks.json` — publishes the Ed25519 verification key
/// for the Agent Card JWS so peers can verify card authenticity.
pub async fn a2a_jwks(State(state): State<AppState>) -> Result<Json<A2aJwks>, (StatusCode, Json<Value>)> {
    let config = state.config.lock();
    let signer = resolve_card_signer(&config.a2a)
        .ok_or_else(|| json_error(StatusCode::INTERNAL_SERVER_ERROR, "card signing key is unavailable"))?;
    Ok(Json(signer.jwks()))
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
        // When the caller also presents an X.509 SVID (PEM) and a trust bundle is
        // configured, the SVID is the source of truth: the SPIFFE ID is taken
        // from the verified certificate's SAN, not the asserted header. In strict
        // mode a verification failure rejects the request; otherwise it falls
        // back to header-asserted trust (trusted front-proxy / mTLS-terminating
        // gateway scenario) with a warning.
        let verified_spiffe = header_value(headers, "x-spiffe-svid")
            .or_else(|| header_value(headers, "x-client-cert-pem"))
            .map(|svid_pem| verify_svid(&svid_pem, &config.a2a));
        match verified_spiffe {
            Some(Ok(result)) => {
                return Some(external_identity_for(
                    "spiffe",
                    "spiffe",
                    &result.spiffe_id,
                    workspace_id.as_ref(),
                ));
            }
            Some(Err(error)) => {
                tracing::warn!(error = %error, "x509 svid verification failed");
                if config.a2a.spiffe_strict_validation {
                    return None;
                }
            }
            None => {
                if config.a2a.spiffe_strict_validation && config.a2a.spiffe_trust_bundle_pem.is_some() {
                    // Strict mode with a trust bundle requires a real SVID; a bare
                    // asserted header is not enough.
                    tracing::warn!("strict spiffe mode requires an x509 svid but none was presented");
                    return None;
                }
            }
        }
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
    /// The configured remote JWKS endpoint could not be fetched and no still
    /// valid cached copy exists. Fail-closed: the token is rejected.
    JwksUnavailable,
    /// The configured `jwt_jwks_uri` does not use HTTPS. Non-HTTPS endpoints
    /// are rejected unconditionally to prevent MITM key injection.
    InsecureJwksUri,
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
            Self::JwksUnavailable => write!(f, "remote jwks endpoint is unavailable and no valid cache exists"),
            Self::InsecureJwksUri => write!(
                f,
                "jwt_jwks_uri must use https:// (non-https endpoints are rejected to prevent MITM)"
            ),
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
///
/// Precedence: inline `jwt_jwks` > remote `jwt_jwks_uri` (cached) > PEM. Any
/// JWKS path requires the token to carry a `kid`. The remote path is
/// fail-closed: if the cache is empty/expired and a refresh is required, the
/// caller must have pre-warmed the cache via [`prewarm_remote_jwks`]; an
/// unreachable endpoint without a still-valid cache rejects the token.
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
    if let Some(raw_uri) = config.jwt_jwks_uri.as_deref().filter(|v| !v.trim().is_empty()) {
        // normalize_jwks_uri trims whitespace AND enforces https-only (fail-closed).
        // The normalised value is used as the cache key so write (prewarm) and
        // read (here) always address the same entry regardless of whitespace.
        let uri = normalize_jwks_uri(raw_uri)?;
        let kid = kid.ok_or(BearerJwtError::MissingKid)?;
        let jwks = cached_remote_jwks(uri, config.jwt_jwks_cache_ttl_secs).ok_or(BearerJwtError::JwksUnavailable)?;
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

/// Normalise a raw `jwt_jwks_uri` value:
///
/// 1. Strip leading/trailing whitespace so the caller never has to worry about
///    accidental whitespace in config values.
/// 2. Enforce that the scheme is `https` (case-insensitive). Any other scheme
///    is a configuration error and is returned as `Err(InsecureJwksUri)`.
///
/// The returned string is the canonical key used for both writing and reading
/// the [`REMOTE_JWKS_CACHE`], ensuring cache write/read are always consistent.
fn normalize_jwks_uri(raw: &str) -> Result<&str, BearerJwtError> {
    let trimmed = raw.trim();
    // Require the scheme portion to be exactly "https" (case-insensitive).
    // We compare the first 8 bytes to avoid a heap allocation from a full URL
    // parse; a 7-byte "http://" prefix that is NOT "https://" is always insecure.
    let scheme_end = trimmed.find("://").unwrap_or(0);
    let scheme = &trimmed[..scheme_end];
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(BearerJwtError::InsecureJwksUri);
    }
    Ok(trimmed)
}

/// Maximum time to wait for a remote JWKS fetch before giving up (fail-closed).
const JWKS_FETCH_TIMEOUT_SECS: u64 = 5;

/// A fetched remote JWKS document together with the instant it was retrieved.
#[derive(Clone)]
struct CachedJwks {
    jwks: Arc<JwkSet>,
    fetched_at: Instant,
}

impl CachedJwks {
    fn is_fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }
}

/// Process-global cache of remote JWKS documents, keyed by endpoint URI. Reads
/// are lock-light; refreshes happen on the async pre-warm path so the
/// synchronous verification path never performs network I/O.
static REMOTE_JWKS_CACHE: LazyLock<RwLock<HashMap<String, CachedJwks>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Return the cached JWKS for `uri` if a fresh (within `ttl_secs`) copy exists.
///
/// Synchronous and never performs network I/O — refreshing is the job of
/// [`prewarm_remote_jwks`]. Returns `None` when no entry exists or the cached
/// copy is stale, which the verification path translates into a fail-closed
/// rejection.
fn cached_remote_jwks(uri: &str, ttl_secs: u64) -> Option<Arc<JwkSet>> {
    let ttl = Duration::from_secs(ttl_secs.max(1));
    let guard = REMOTE_JWKS_CACHE.read();
    let entry = guard.get(uri)?;
    if entry.is_fresh(ttl) {
        Some(Arc::clone(&entry.jwks))
    } else {
        None
    }
}

/// Ensure the remote JWKS cache for `config` holds a fresh document before the
/// synchronous verification path runs. No-op unless a remote `jwt_jwks_uri` is
/// configured and no inline `jwt_jwks` overrides it.
///
/// Runs the blocking HTTP fetch on a blocking thread so the async runtime is
/// never stalled. On fetch failure the existing cache entry (if any) is left in
/// place — a still-fresh entry keeps working, a stale/absent one results in a
/// fail-closed rejection at verification time.
async fn prewarm_remote_jwks(config: &crate::config::McpServerRuntimeConfig) {
    if config.jwt_jwks.as_deref().is_some_and(|v| !v.trim().is_empty()) {
        // Inline JWKS wins; no remote fetch needed.
        return;
    }
    let raw_uri = match config.jwt_jwks_uri.as_deref().filter(|v| !v.trim().is_empty()) {
        Some(v) => v,
        None => return,
    };
    // Enforce HTTPS-only before issuing any network request (fail-closed).
    // normalize_jwks_uri also trims whitespace so the returned slice is the
    // canonical cache key — identical to what resolve_decoding_key uses.
    let uri = match normalize_jwks_uri(raw_uri) {
        Ok(u) => u.to_string(),
        Err(_) => {
            tracing::warn!(
                uri = %raw_uri.trim(),
                "jwt_jwks_uri does not use https:// — remote JWKS fetch refused (fail-closed)"
            );
            return;
        }
    };
    let ttl = Duration::from_secs(config.jwt_jwks_cache_ttl_secs.max(1));

    // Fast path: a fresh entry already satisfies this request.
    if REMOTE_JWKS_CACHE.read().get(&uri).is_some_and(|e| e.is_fresh(ttl)) {
        return;
    }

    let fetch_uri = uri.clone();
    let fetched = tokio::task::spawn_blocking(move || fetch_remote_jwks(&fetch_uri)).await;
    match fetched {
        Ok(Ok(jwks)) => {
            REMOTE_JWKS_CACHE.write().insert(
                uri,
                CachedJwks {
                    jwks: Arc::new(jwks),
                    fetched_at: Instant::now(),
                },
            );
        }
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "remote jwks refresh failed; relying on existing cache (fail-closed if stale)");
        }
        Err(error) => {
            tracing::warn!(error = %error, "remote jwks fetch task failed to join");
        }
    }
}

/// Blocking fetch + parse of a remote JWKS document. Runs only inside
/// `spawn_blocking`. Uses the rustls-backed blocking reqwest client with a
/// bounded timeout so a hung endpoint cannot stall a worker thread indefinitely.
fn fetch_remote_jwks(uri: &str) -> Result<JwkSet, anyhow::Error> {
    use anyhow::Context as _;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(JWKS_FETCH_TIMEOUT_SECS))
        .build()
        .context("failed to build jwks http client")?;
    let response = client
        .get(uri)
        .send()
        .context("jwks endpoint request failed")?
        .error_for_status()
        .context("jwks endpoint returned an error status")?;
    let body = response.text().context("failed to read jwks response body")?;
    serde_json::from_str::<JwkSet>(&body).context("jwks endpoint returned invalid json")
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

// ---------------------------------------------------------------------------
// Agent Card JWS signing (Ed25519) and JWKS publication.
// ---------------------------------------------------------------------------

/// Ed25519 signer for the published Agent Card. Holds the PKCS#8 private key and
/// the raw 32-byte public key point, plus a stable key id used as the JWKS
/// `kid` and the JWS protected-header `kid`.
#[derive(Debug, Clone)]
struct AgentCardSigner {
    kid: String,
    pkcs8: Vec<u8>,
    public_key: Vec<u8>,
}

/// On-disk persisted form of the card signing key (JSON, `0600`). Mirrors the
/// witness-key convention but is an independent key dedicated to card signing.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedCardKey {
    kid: String,
    alg: String,
    /// base64 (standard) PKCS#8 v2 DER private key.
    secret_b64: String,
    /// base64 (standard) raw 32-byte Ed25519 public key.
    public_b64: String,
    created_at: String,
}

/// Process-global ephemeral signer, used when no `card_signing_key_path` is set.
/// Generated once per process so the JWKS stays consistent for the run.
static EPHEMERAL_CARD_SIGNER: OnceLock<Option<AgentCardSigner>> = OnceLock::new();

impl AgentCardSigner {
    fn generate() -> anyhow::Result<Self> {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| anyhow::anyhow!("generate Ed25519 card key"))?;
        let key_pair =
            Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| anyhow::anyhow!("load generated card key"))?;
        Ok(Self {
            kid: format!("a2a-card-{}", Uuid::new_v4()),
            pkcs8: pkcs8.as_ref().to_vec(),
            public_key: key_pair.public_key().as_ref().to_vec(),
        })
    }

    /// Load from disk if `path` exists, otherwise generate and persist (`0600`).
    fn load_or_generate(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            return Self::load_from(path);
        }
        let signer = Self::generate()?;
        signer.persist_to(path)?;
        tracing::info!(kid = %signer.kid, path = %path.display(), "generated new a2a card signing key");
        Ok(signer)
    }

    fn load_from(path: &Path) -> anyhow::Result<Self> {
        use base64::engine::general_purpose::STANDARD as BASE64_STD;
        let raw = std::fs::read_to_string(path)?;
        let persisted: PersistedCardKey = serde_json::from_str(&raw)?;
        if persisted.alg != "Ed25519" {
            anyhow::bail!("unsupported card key alg: {}", persisted.alg);
        }
        let pkcs8 = BASE64_STD.decode(persisted.secret_b64.as_bytes())?;
        let public_key = BASE64_STD.decode(persisted.public_b64.as_bytes())?;
        let key_pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| anyhow::anyhow!("load persisted card key"))?;
        if key_pair.public_key().as_ref() != public_key.as_slice() {
            anyhow::bail!("card key public/secret mismatch in {}", path.display());
        }
        Ok(Self {
            kid: persisted.kid,
            pkcs8,
            public_key,
        })
    }

    fn persist_to(&self, path: &Path) -> anyhow::Result<()> {
        use base64::engine::general_purpose::STANDARD as BASE64_STD;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let persisted = PersistedCardKey {
            kid: self.kid.clone(),
            alg: "Ed25519".to_string(),
            secret_b64: BASE64_STD.encode(&self.pkcs8),
            public_b64: BASE64_STD.encode(&self.public_key),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let json = serde_json::to_string_pretty(&persisted)?;
        write_private_key_file(path, json.as_bytes())?;
        Ok(())
    }

    fn key_pair(&self) -> anyhow::Result<Ed25519KeyPair> {
        Ed25519KeyPair::from_pkcs8(&self.pkcs8).map_err(|_| anyhow::anyhow!("load Ed25519 card keypair"))
    }

    /// Sign `payload_json` as a JWS compact serialization with an EdDSA header.
    fn sign_jws(&self, payload_json: &[u8]) -> anyhow::Result<String> {
        let header = serde_json::json!({
            "alg": A2A_CARD_JWS_ALG,
            "typ": "JWT",
            "kid": self.kid,
        });
        let header_b64 = BASE64_URL.encode(serde_json::to_vec(&header)?);
        let payload_b64 = BASE64_URL.encode(payload_json);
        let signing_input = format!("{header_b64}.{payload_b64}");
        let key_pair = self.key_pair()?;
        let signature = key_pair.sign(signing_input.as_bytes());
        let sig_b64 = BASE64_URL.encode(signature.as_ref());
        Ok(format!("{signing_input}.{sig_b64}"))
    }

    /// Public JWKS document advertising this signer's verification key.
    fn jwks(&self) -> A2aJwks {
        A2aJwks {
            keys: vec![A2aJwk {
                kty: "OKP".to_string(),
                crv: "Ed25519".to_string(),
                kid: self.kid.clone(),
                use_: "sig".to_string(),
                alg: A2A_CARD_JWS_ALG.to_string(),
                x: BASE64_URL.encode(&self.public_key),
            }],
        }
    }
}

/// Write `bytes` to `path` with owner-only (`0600`) permissions on Unix.
fn write_private_key_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    Ok(())
}

/// Resolve the active card signer: a persisted key when a path is configured,
/// otherwise the process-global ephemeral key. Returns `None` only if even
/// ephemeral key generation fails (extremely unlikely; logged).
fn resolve_card_signer(config: &crate::config::A2aConfig) -> Option<AgentCardSigner> {
    if let Some(path) = config.card_signing_key_path.as_deref().filter(|p| !p.trim().is_empty()) {
        match AgentCardSigner::load_or_generate(Path::new(path)) {
            Ok(signer) => return Some(signer),
            Err(error) => {
                tracing::warn!(error = %error, "a2a card signing key unavailable; falling back to ephemeral key");
            }
        }
    }
    EPHEMERAL_CARD_SIGNER
        .get_or_init(|| match AgentCardSigner::generate() {
            Ok(signer) => Some(signer),
            Err(error) => {
                tracing::error!(error = %error, "failed to generate ephemeral a2a card signing key");
                None
            }
        })
        .clone()
}

/// The JWKS URI advertised in card signatures: explicit config, else local path.
fn card_jwks_uri(config: &crate::config::A2aConfig) -> String {
    config
        .card_jwks_uri
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .map_or_else(|| A2A_JWKS_PATH.to_string(), ToString::to_string)
}

/// Build the unsigned card, then attach a JWS signature over its canonical body.
fn build_signed_agent_card(config: &crate::config::Config) -> A2aAgentCard {
    let now = chrono::Utc::now();
    let ttl = i64::try_from(config.a2a.card_ttl_seconds).unwrap_or(3_600);
    let expires = now + chrono::Duration::seconds(ttl);
    let mut card = A2aAgentCard {
        schema_version: A2A_CARD_SCHEMA_VERSION.to_string(),
        id: config.a2a.spiffe_id.clone(),
        name: config.a2a.agent_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        description: "PRX runtime agent — memory-grounded assistant exposing MCP tools over A2A".to_string(),
        endpoint: format!("http://{}/a2a/v1/identity", config.a2a.bind),
        authentication: vec![
            A2aAuthMethod {
                kind: "bearer-jwt".to_string(),
                trusted_issuers: config.a2a.allowed_peer_issuers.clone(),
            },
            A2aAuthMethod {
                kind: "spiffe".to_string(),
                trusted_issuers: config.a2a.trusted_trust_domains.clone(),
            },
            A2aAuthMethod {
                kind: "mtls".to_string(),
                trusted_issuers: config.a2a.allowed_peer_issuers.clone(),
            },
        ],
        capabilities: A2aCardCapabilities {
            tools: config.mcp_server.exposed_tools.clone(),
            modalities: vec!["text".to_string()],
            skills: vec!["memory_qa".to_string(), "document_grounding".to_string()],
        },
        issued_at: now.to_rfc3339(),
        expires_at: expires.to_rfc3339(),
        signature: None,
    };
    card.signature = sign_agent_card(&card, &config.a2a);
    card
}

/// Produce the JWS signature for `card` (its `signature` field must be `None`).
fn sign_agent_card(card: &A2aAgentCard, config: &crate::config::A2aConfig) -> Option<A2aCardSignature> {
    let signer = resolve_card_signer(config)?;
    let payload = match serde_json::to_vec(card) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(error = %error, "failed to serialize agent card for signing");
            return None;
        }
    };
    match signer.sign_jws(&payload) {
        Ok(jws) => Some(A2aCardSignature {
            algorithm: A2A_CARD_JWS_ALG.to_string(),
            key_id: signer.kid,
            jws,
            jwks_uri: card_jwks_uri(config),
        }),
        Err(error) => {
            tracing::warn!(error = %error, "failed to sign agent card");
            None
        }
    }
}

/// Verify a signed Agent Card against a JWKS. Returns `Ok(true)` only when the
/// JWS signs the exact canonical body of `card` (with `signature` stripped) and
/// the signing `kid` resolves to an Ed25519 key in `jwks`.
#[cfg_attr(not(test), allow(dead_code))]
fn verify_agent_card(card: &A2aAgentCard, jwks: &A2aJwks) -> anyhow::Result<bool> {
    let Some(signature) = card.signature.as_ref() else {
        anyhow::bail!("card has no signature");
    };
    let mut parts = signature.jws.split('.');
    let (Some(header_b64), Some(payload_b64), Some(sig_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        anyhow::bail!("malformed jws compact serialization");
    };

    // The signed payload must equal the canonical unsigned card body. This binds
    // the signature to the card content and defeats body tampering.
    let mut unsigned = card.clone();
    unsigned.signature = None;
    let expected_payload = serde_json::to_vec(&unsigned)?;
    let actual_payload = BASE64_URL.decode(payload_b64.as_bytes())?;
    if actual_payload != expected_payload {
        return Ok(false);
    }

    // Resolve the verification key by kid from the JWKS.
    let header: Value = serde_json::from_slice(&BASE64_URL.decode(header_b64.as_bytes())?)?;
    let kid = header.get("kid").and_then(Value::as_str).unwrap_or_default();
    let Some(jwk) = jwks
        .keys
        .iter()
        .find(|k| k.kid == kid && k.kty == "OKP" && k.crv == "Ed25519")
    else {
        return Ok(false);
    };
    let public_key = BASE64_URL.decode(jwk.x.as_bytes())?;

    let signing_input = format!("{header_b64}.{payload_b64}");
    let signature_bytes = BASE64_URL.decode(sig_b64.as_bytes())?;
    let verifier = UnparsedPublicKey::new(&ED25519, public_key.as_slice());
    Ok(verifier.verify(signing_input.as_bytes(), &signature_bytes).is_ok())
}

// ---------------------------------------------------------------------------
// SPIFFE SVID X.509 verification.
// ---------------------------------------------------------------------------

/// Outcome of verifying a peer-presented X.509 SVID against the configured
/// trust bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvidVerification {
    /// SPIFFE ID extracted from the leaf certificate's SAN URI.
    pub spiffe_id: String,
    /// Trust domain parsed from the SPIFFE ID.
    pub trust_domain: String,
    /// SHA-256 fingerprint of the leaf certificate DER (audit aid).
    pub cert_fingerprint: String,
    /// True when the leaf chains to a configured trust-bundle CA, is within its
    /// validity window, and its trust domain is accepted.
    pub trusted: bool,
}

/// Errors raised while verifying an X.509 SVID. Carries no key material.
#[derive(Debug, PartialEq, Eq)]
pub enum SvidError {
    /// PEM/DER could not be parsed into an X.509 certificate.
    ParseError(String),
    /// No SAN URI of the `spiffe://` form was present in the leaf.
    NoSpiffeSan,
    /// The SPIFFE ID is syntactically invalid.
    InvalidSpiffeId(String),
    /// The certificate is outside its validity window.
    Expired,
    /// No trust bundle was configured, so the chain cannot be verified.
    NoTrustBundle,
    /// The leaf's signature did not verify against any trust-bundle CA.
    UntrustedChain,
    /// The SPIFFE trust domain is not in the accepted set.
    UntrustedDomain(String),
}

impl std::fmt::Display for SvidError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseError(reason) => write!(f, "svid parse error: {reason}"),
            Self::NoSpiffeSan => write!(f, "leaf certificate has no spiffe:// SAN URI"),
            Self::InvalidSpiffeId(id) => write!(f, "invalid spiffe id: {id}"),
            Self::Expired => write!(f, "svid is outside its validity window"),
            Self::NoTrustBundle => write!(f, "no trust bundle configured for chain verification"),
            Self::UntrustedChain => write!(f, "svid does not chain to any trust-bundle ca"),
            Self::UntrustedDomain(domain) => write!(f, "trust domain {domain} is not accepted"),
        }
    }
}

impl std::error::Error for SvidError {}

/// Parse `spiffe://<trust-domain>/<path>` and return the trust domain. Enforces
/// the SPIFFE URI scheme and a non-empty authority component.
fn parse_spiffe_trust_domain(spiffe_id: &str) -> Result<String, SvidError> {
    let rest = spiffe_id
        .strip_prefix("spiffe://")
        .ok_or_else(|| SvidError::InvalidSpiffeId(spiffe_id.to_string()))?;
    let trust_domain = rest.split('/').next().unwrap_or_default();
    if trust_domain.is_empty() || trust_domain.contains(' ') {
        return Err(SvidError::InvalidSpiffeId(spiffe_id.to_string()));
    }
    Ok(trust_domain.to_string())
}

/// Normalize a PEM string that may have arrived via an HTTP header. HTTP header
/// values cannot contain raw newlines, so proxies typically transport a PEM SVID
/// either percent-encoded (`%0A`) or with literal escaped `\n`/`\t` sequences, or
/// space-joined. This restores real newlines so the PEM parser accepts it. A PEM
/// that already contains real newlines is returned essentially unchanged.
fn normalize_header_pem(raw: &str) -> std::borrow::Cow<'_, str> {
    if raw.contains('\n') {
        return std::borrow::Cow::Borrowed(raw);
    }
    let decoded = raw
        .replace("%0A", "\n")
        .replace("%0a", "\n")
        .replace("%20", " ")
        .replace("\\n", "\n")
        .replace("\\t", "\n");
    // Some proxies join the base64 body with single spaces; turn the inter-line
    // spaces back into newlines but keep the BEGIN/END marker words intact.
    let rebuilt = decoded
        .replace("-----BEGIN CERTIFICATE----- ", "-----BEGIN CERTIFICATE-----\n")
        .replace(" -----END CERTIFICATE-----", "\n-----END CERTIFICATE-----");
    std::borrow::Cow::Owned(rebuilt)
}

/// Decode one-or-more PEM certificate blocks into DER byte vectors. Accepts both
/// raw multi-line PEM and header-transported single-line PEM.
fn pem_certificates_to_der(pem: &str) -> Result<Vec<Vec<u8>>, SvidError> {
    let normalized = normalize_header_pem(pem);
    let mut ders = Vec::new();
    for block in x509_parser::pem::Pem::iter_from_buffer(normalized.as_bytes()) {
        let block = block.map_err(|e| SvidError::ParseError(e.to_string()))?;
        ders.push(block.contents);
    }
    if ders.is_empty() {
        return Err(SvidError::ParseError("no PEM certificate blocks found".to_string()));
    }
    Ok(ders)
}

/// Verify a peer X.509 SVID (PEM) against the configured trust bundle.
///
/// Steps: parse leaf, extract SPIFFE ID from a SAN URI, check the leaf validity
/// window, verify the leaf signature against each trust-bundle CA, then confirm
/// the trust domain is accepted. Each failure is a typed error; nothing panics.
fn verify_svid(leaf_pem: &str, config: &crate::config::A2aConfig) -> Result<SvidVerification, SvidError> {
    use sha2::{Digest, Sha256};
    use x509_parser::prelude::*;

    let leaf_ders = pem_certificates_to_der(leaf_pem)?;
    let leaf_der = leaf_ders.first().ok_or(SvidError::NoSpiffeSan)?;
    let (_, leaf) = X509Certificate::from_der(leaf_der).map_err(|e| SvidError::ParseError(e.to_string()))?;

    // Extract the SPIFFE ID from a SAN URI entry.
    let spiffe_id = leaf
        .extensions()
        .iter()
        .find_map(|ext| match ext.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(san) => san.general_names.iter().find_map(|name| {
                if let GeneralName::URI(uri) = name {
                    uri.starts_with("spiffe://").then(|| (*uri).to_string())
                } else {
                    None
                }
            }),
            _ => None,
        })
        .ok_or(SvidError::NoSpiffeSan)?;
    let trust_domain = parse_spiffe_trust_domain(&spiffe_id)?;

    let cert_fingerprint = {
        let digest = Sha256::digest(leaf_der);
        digest.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };

    // Validity window check.
    if !leaf.validity().is_valid() {
        return Err(SvidError::Expired);
    }

    // Chain verification against the configured trust bundle CA(s).
    let Some(bundle_pem) = config
        .spiffe_trust_bundle_pem
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    else {
        return Err(SvidError::NoTrustBundle);
    };
    let ca_ders = pem_certificates_to_der(bundle_pem)?;
    let mut chained = false;
    for ca_der in &ca_ders {
        let Ok((_, ca)) = X509Certificate::from_der(ca_der) else {
            continue;
        };
        if leaf.verify_signature(Some(ca.public_key())).is_ok() {
            chained = true;
            break;
        }
    }
    if !chained {
        return Err(SvidError::UntrustedChain);
    }

    // Trust-domain allow-list. An empty list defers to the issuer allow-list and
    // is treated as "accept any chained domain" at this layer.
    let domain_ok = config.trusted_trust_domains.is_empty()
        || config
            .trusted_trust_domains
            .iter()
            .any(|d| d == &trust_domain || d == &format!("spiffe://{trust_domain}"));
    if !domain_ok {
        return Err(SvidError::UntrustedDomain(trust_domain));
    }

    Ok(SvidVerification {
        spiffe_id,
        trust_domain,
        cert_fingerprint,
        trusted: true,
    })
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

    fn test_remote_jwks_doc() -> String {
        format!(
            r#"{{"keys":[{{"kty":"RSA","use":"sig","kid":"remote-1","alg":"RS256","n":"{TEST_RSA_N}","e":"AQAB"}}]}}"#
        )
    }

    fn seed_remote_jwks_cache(uri: &str, fetched_at: Instant) {
        let jwks: JwkSet = serde_json::from_str(&test_remote_jwks_doc()).expect("test: parse jwks");
        REMOTE_JWKS_CACHE.write().insert(
            uri.to_string(),
            CachedJwks {
                jwks: Arc::new(jwks),
                fetched_at,
            },
        );
    }

    fn remote_jwks_config(uri: &str) -> crate::config::McpServerRuntimeConfig {
        crate::config::McpServerRuntimeConfig {
            jwt_issuer: Some("https://issuer.example".to_string()),
            jwt_audience: Some("prx-mcp".to_string()),
            jwt_jwks_uri: Some(uri.to_string()),
            jwt_jwks_cache_ttl_secs: 300,
            ..Default::default()
        }
    }

    #[test]
    fn remote_jwks_fresh_cache_verifies_token() {
        let uri = "https://issuer.example/.well-known/jwks-fresh.json";
        seed_remote_jwks_cache(uri, Instant::now());
        let config = remote_jwks_config(uri);
        let token = sign_rs256(
            &TestClaims {
                iss: "https://issuer.example".to_string(),
                sub: "agent-remote".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: future_exp(),
            },
            Some("remote-1"),
        );
        let claims = verify_bearer_jwt(&token, &config).expect("test: remote jwks verify");
        assert_eq!(claims.subject, "agent-remote");
    }

    #[test]
    fn remote_jwks_missing_cache_fails_closed() {
        // No cache entry seeded -> the remote endpoint is treated as unavailable
        // and the token MUST be rejected rather than accepted.
        let uri = "https://issuer.example/.well-known/jwks-absent.json";
        REMOTE_JWKS_CACHE.write().remove(uri);
        let config = remote_jwks_config(uri);
        let token = sign_rs256(
            &TestClaims {
                iss: "https://issuer.example".to_string(),
                sub: "agent-remote".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: future_exp(),
            },
            Some("remote-1"),
        );
        let err = verify_bearer_jwt(&token, &config).expect_err("test: missing remote jwks");
        assert!(matches!(err, BearerJwtError::JwksUnavailable));
    }

    #[test]
    fn remote_jwks_expired_cache_fails_closed() {
        // A cache entry older than the TTL is treated as stale: fail-closed.
        let uri = "https://issuer.example/.well-known/jwks-stale.json";
        let stale_at = Instant::now()
            .checked_sub(Duration::from_secs(10_000))
            .expect("test: stale instant");
        seed_remote_jwks_cache(uri, stale_at);
        let mut config = remote_jwks_config(uri);
        config.jwt_jwks_cache_ttl_secs = 1;
        let token = sign_rs256(
            &TestClaims {
                iss: "https://issuer.example".to_string(),
                sub: "agent-remote".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: future_exp(),
            },
            Some("remote-1"),
        );
        let err = verify_bearer_jwt(&token, &config).expect_err("test: stale remote jwks");
        assert!(matches!(err, BearerJwtError::JwksUnavailable));
    }

    #[test]
    fn remote_jwks_requires_kid() {
        let uri = "https://issuer.example/.well-known/jwks-nokid.json";
        seed_remote_jwks_cache(uri, Instant::now());
        let config = remote_jwks_config(uri);
        let token = sign_rs256(
            &TestClaims {
                iss: "https://issuer.example".to_string(),
                sub: "agent-remote".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: future_exp(),
            },
            None,
        );
        let err = verify_bearer_jwt(&token, &config).expect_err("test: remote jwks needs kid");
        assert!(matches!(err, BearerJwtError::MissingKid));
    }

    #[test]
    fn inline_jwks_takes_precedence_over_remote_uri() {
        // When both inline and remote are set, the inline document is used and
        // the (deliberately absent) remote cache is never consulted.
        let uri = "https://issuer.example/.well-known/jwks-precedence.json";
        REMOTE_JWKS_CACHE.write().remove(uri);
        let mut config = remote_jwks_config(uri);
        config.jwt_jwks = Some(test_remote_jwks_doc());
        let token = sign_rs256(
            &TestClaims {
                iss: "https://issuer.example".to_string(),
                sub: "agent-inline".to_string(),
                aud: Some("prx-mcp".to_string()),
                exp: future_exp(),
            },
            Some("remote-1"),
        );
        let claims = verify_bearer_jwt(&token, &config).expect("test: inline precedence");
        assert_eq!(claims.subject, "agent-inline");
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

    // -- Agent Card / JWS / JWKS ------------------------------------------

    #[test]
    fn well_known_card_is_well_formed() {
        let card = build_signed_agent_card(&crate::config::Config::default());
        assert_eq!(card.schema_version, A2A_CARD_SCHEMA_VERSION);
        assert_eq!(card.id, "spiffe://prx-local/agent/prx-default");
        assert_eq!(card.name, "prx-default");
        assert_eq!(card.version, env!("CARGO_PKG_VERSION"));
        // Three auth methods advertised: bearer-jwt, spiffe, mtls.
        assert_eq!(card.authentication.len(), 3);
        assert!(card.authentication.iter().any(|m| m.kind == "spiffe"));
        // expires_at is strictly after issued_at.
        assert!(card.expires_at > card.issued_at);
        // A signature must be present (ephemeral key generation succeeds).
        assert!(card.signature.is_some());
    }

    #[test]
    fn signed_card_verifies_against_published_jwks() {
        let config = crate::config::A2aConfig::default();
        let card = {
            let mut full = crate::config::Config::default();
            full.a2a = config.clone();
            build_signed_agent_card(&full)
        };
        let signer = resolve_card_signer(&config).expect("test: ephemeral signer");
        let jwks = signer.jwks();
        assert!(verify_agent_card(&card, &jwks).expect("test: verify"));
    }

    #[test]
    fn tampered_card_fails_verification() {
        let config = crate::config::A2aConfig::default();
        let mut card = {
            let mut full = crate::config::Config::default();
            full.a2a = config.clone();
            build_signed_agent_card(&full)
        };
        let signer = resolve_card_signer(&config).expect("test: ephemeral signer");
        let jwks = signer.jwks();
        // Mutate a signed field without re-signing -> verification must fail.
        card.name = "impersonator".to_string();
        assert!(!verify_agent_card(&card, &jwks).expect("test: verify tampered"));
    }

    #[test]
    fn card_verification_rejects_foreign_jwks() {
        let card = build_signed_agent_card(&crate::config::Config::default());
        // A JWKS from a different key (different kid + key material) must not
        // verify the card.
        let other = AgentCardSigner::generate().expect("test: other signer");
        let foreign_jwks = other.jwks();
        assert!(!verify_agent_card(&card, &foreign_jwks).expect("test: foreign jwks"));
    }

    #[test]
    fn persisted_card_key_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("card.key");
        let first = AgentCardSigner::load_or_generate(&path).expect("test: generate");
        let second = AgentCardSigner::load_or_generate(&path).expect("test: reload");
        // Reload yields the same kid and public key (stable across restarts).
        assert_eq!(first.kid, second.kid);
        assert_eq!(first.public_key, second.public_key);
    }

    // -- SPIFFE SVID X.509 verification -----------------------------------

    const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDEzCCAfugAwIBAgIUDhSoIjROzqwbTnjzWsE0ruuM+OEwDQYJKoZIhvcNAQEL\n\
BQAwGTEXMBUGA1UEAwwOdGVzdC1zcGlmZmUtY2EwHhcNMjYwNTMxMjA0NzExWhcN\n\
MzYwNTI4MjA0NzExWjAZMRcwFQYDVQQDDA50ZXN0LXNwaWZmZS1jYTCCASIwDQYJ\n\
KoZIhvcNAQEBBQADggEPADCCAQoCggEBAJzwAHQX7gZCZq5xwYhEPXr8tn2hWWtj\n\
M6dq3KjJ+44FklT4dGO9EC7WvzJlfPokppuUYLvuRPT/Hsdcru4lL4CzYbECN7Ci\n\
aLxN+5rHKIrt25O53bu2878TsJufX1pQqusGx4hJNOQIOq9cjfbOdCJ9HKzI0E/x\n\
uGsieEfm5HSeh1h8ZVT3j6I1yvR69A/k/a/p1yon19qx+iQ4Xgf5Fa7ntKlX9xdt\n\
pKWsjRQEmpb6aeUcfcyyIzJmm9EceJaE5okAr+8B2O9ds1v3PxzKtAHaXsqjpFGz\n\
ANme7GfMf00qgIB2NH9zC+c2btE8Au3oRYXkmwr5Dd6EWAXvMAou2lkCAwEAAaNT\n\
MFEwHQYDVR0OBBYEFJU1LDUF0KECh/i1zlQ3FIhSYU0GMB8GA1UdIwQYMBaAFJU1\n\
LDUF0KECh/i1zlQ3FIhSYU0GMA8GA1UdEwEB/wQFMAMBAf8wDQYJKoZIhvcNAQEL\n\
BQADggEBAHUurEvLru/d2gYIEtwBoBxEbgzct45k+hASehG1kF4B5Mbl+ZEqI5UN\n\
5gvtlRQSqmbkO1VabYwk9SLK6uD9zUb08KiKJJQedrMjxs/g0kbVHdYolQneY51V\n\
4cMDYOi1/AIfonK+I6G97DgouRGWMRtg0dmmqj2KwF4O3F4AWlfgSFUJfrAk/BN+\n\
D1UJLeOuPwnWYJmwgJTYkMi1gzWHSCwjMw8/K1B/gK5CVjfWc5Qq5RpmditgtZPi\n\
/yOiuWQsBixthIxfCqspj5mRoGu8ZOTiQSzfbKQpbwQZtKYjkVqOt+EcdxD3Vo5m\n\
rfMSOTLgmwS0fvgZn1UiBJm/hiCMyno=\n\
-----END CERTIFICATE-----\n";

    const TEST_LEAF_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDLjCCAhagAwIBAgIUcfUQUBXgovzvmrcNl4cwB5pVH6EwDQYJKoZIhvcNAQEL\n\
BQAwGTEXMBUGA1UEAwwOdGVzdC1zcGlmZmUtY2EwHhcNMjYwNTMxMjA0NzExWhcN\n\
MzYwNTI4MjA0NzExWjATMREwDwYDVQQDDAh3b3JrbG9hZDCCASIwDQYJKoZIhvcN\n\
AQEBBQADggEPADCCAQoCggEBAL496gM4R3YYIquFKEKzJS9hXpWrStLVVmiCGBxK\n\
keGCpZ1C80myFo1GV+QKRP5bNOryBkPQNmUhCMfe/pD0CN4pdXRLliEoy6bX/Qnv\n\
osQlrRanINTPOkM/CAw462pOHuu8MSRODrLmd96udRacez6nhrgfm9wWxLCeB4qn\n\
zhtT+utm1VFkZhthF+M5x/xZDYLVdZbft6Aj+feRyTXoYpnYfwD7an+UgBWAAuA3\n\
AP9uue3GaGbQcgKB9WOIoRxTlkcFATWbO/07mNXv/OM+2/mI3rCtNTU/Iyk/5MyS\n\
3kzaGnj631pP/5v+EZAWPxD5lgW7bfQP05lgug14L2RAFdECAwEAAaN0MHIwMAYD\n\
VR0RBCkwJ4Ylc3BpZmZlOi8vdHJ1c3RlZC5leGFtcGxlL2FnZW50L3dvcmtlcjAd\n\
BgNVHQ4EFgQUfZIt57LabHcu+3ZwUvH3kQgjK9UwHwYDVR0jBBgwFoAUlTUsNQXQ\n\
oQKH+LXOVDcUiFJhTQYwDQYJKoZIhvcNAQELBQADggEBAECx5q+dY8WANqkWT9aL\n\
llsPhDcSegsWNR9EAos1b0RZFgsbBnXJd/p4WFyp4R2IKmNS83kQMI3h8KaElrrq\n\
IKbVir8bTB2w3KJJ+ZZ2ZBwDyQ86t/lYl7O6VsfZyRiu/nwXUddx2wYLmHSNqTRj\n\
yoYOfb4yDo5Ezi4ttGXBmcZkBcbpB5zsM74CgA7puPp6trgCYRohE44DlLHGZN2c\n\
dkMK/xsOTz1kOowyahZDADPgATW+SoXQ5tnR4gkunfPdowUyuqOkWohgy+WmdUM8\n\
HkRJBc3+5CQr6Za4zjfNTMrMMTE8LHLp08uZGdOkaMMhAftF1IR+UMWRdJ3wkCzm\n\
KIo=\n\
-----END CERTIFICATE-----\n";

    // A leaf carrying the same SPIFFE SAN but signed by an UNTRUSTED CA.
    const TEST_ROGUE_LEAF_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDJTCCAg2gAwIBAgIUE9uuBu6Z8BKxQdYG7hR/1Lx+WZgwDQYJKoZIhvcNAQEL\n\
BQAwEzERMA8GA1UEAwwIcm9ndWUtY2EwHhcNMjYwNTMxMjA0NzExWhcNMzYwNTI4\n\
MjA0NzExWjAQMQ4wDAYDVQQDDAVyb2d1ZTCCASIwDQYJKoZIhvcNAQEBBQADggEP\n\
ADCCAQoCggEBANK5BOkp7KvPV1aBSCSL9u68QiWdFPdypNx12Y/EmHyTJwkRQ8Cz\n\
/DcT4Txsab0tzerwdPptfkIqX8hk/1YTsC4jsOAL9kxsIWNEsO8n8sClMgNnOaW7\n\
tcsjJ3GeJFFsI/pz6X9cbTEcI20v+migBrKtvppmZlvoAbNAxfz78i2HZVM9IGgR\n\
Tsulpbwq1uegZbkeZJGsNXjbQdfhXW6ykKBq69AEfpimBubVZcH7IOrko23Z4/SN\n\
BDgfJRWbbdbqQy7PGxHjqVPl1+gGO8MSWDjK3Ht9+VZKVIu80T1Xm2BWoEcLKJVc\n\
psoJX0LtIZRCzFsOIlQfzeWmh1IZJVekn4UCAwEAAaN0MHIwMAYDVR0RBCkwJ4Yl\n\
c3BpZmZlOi8vdHJ1c3RlZC5leGFtcGxlL2FnZW50L3dvcmtlcjAdBgNVHQ4EFgQU\n\
VtrnJ0c5c9373mu7BT7qLjG1sjIwHwYDVR0jBBgwFoAUKwgA2ADDQPrrz0HSPZ0G\n\
5Ncvp3QwDQYJKoZIhvcNAQELBQADggEBAF4FtdodA+1JXPEQQHppu/AcvoYevMB9\n\
49WRnv8plcKbsUxaNh8ji5aWrq1NZ2JTGbs7NFD63t1L9lMY3wlXH8jGmgyx+XgQ\n\
p7+XY0xJ3sljbxmjxym5HNn6L2uOQV77zOwumAsjcTm2YtUTMoBqDxFDQB50nin9\n\
+jK6Y34CtczVEFzLVajfEXJNoYjqPw2C02YAfuFs4S0WALQsWG27sC/UI4amXYuJ\n\
f/+LwM1E2HpwH8ZFVfFQr0acepQEMs9gkymArUuqdWObmM5v2xAw1WY8rH6UX6Cm\n\
2nXfrUbRSIN6SXEctFoy2APvsTy5T3UpzNTHCRW+B81QBEy/cJ4yDrU=\n\
-----END CERTIFICATE-----\n";

    fn svid_config() -> crate::config::A2aConfig {
        crate::config::A2aConfig {
            spiffe_trust_bundle_pem: Some(TEST_CA_PEM.to_string()),
            trusted_trust_domains: vec!["trusted.example".to_string()],
            ..Default::default()
        }
    }

    /// HTTP header values cannot carry raw newlines; percent-encode the PEM the
    /// way a front proxy would when transporting an SVID via a header.
    fn header_safe_pem(pem: &str) -> String {
        pem.replace('\n', "%0A")
    }

    #[test]
    fn parse_trust_domain_extracts_authority() {
        assert_eq!(
            parse_spiffe_trust_domain("spiffe://trusted.example/agent/x").unwrap(),
            "trusted.example"
        );
        assert!(parse_spiffe_trust_domain("https://not-spiffe/x").is_err());
        assert!(parse_spiffe_trust_domain("spiffe:///empty-authority").is_err());
    }

    #[test]
    fn valid_svid_chains_to_trust_bundle() {
        let result = verify_svid(TEST_LEAF_PEM, &svid_config()).expect("test: valid svid");
        assert_eq!(result.spiffe_id, "spiffe://trusted.example/agent/worker");
        assert_eq!(result.trust_domain, "trusted.example");
        assert!(result.trusted);
        assert_eq!(result.cert_fingerprint.len(), 64);
    }

    #[test]
    fn svid_with_untrusted_chain_is_rejected() {
        // Same SPIFFE SAN, but signed by a CA not in our trust bundle.
        let err = verify_svid(TEST_ROGUE_LEAF_PEM, &svid_config()).expect_err("test: untrusted chain");
        assert_eq!(err, SvidError::UntrustedChain);
    }

    #[test]
    fn svid_without_trust_bundle_is_rejected() {
        let mut config = svid_config();
        config.spiffe_trust_bundle_pem = None;
        let err = verify_svid(TEST_LEAF_PEM, &config).expect_err("test: no bundle");
        assert_eq!(err, SvidError::NoTrustBundle);
    }

    #[test]
    fn svid_with_untrusted_domain_is_rejected() {
        let mut config = svid_config();
        config.trusted_trust_domains = vec!["other.example".to_string()];
        let err = verify_svid(TEST_LEAF_PEM, &config).expect_err("test: untrusted domain");
        assert_eq!(err, SvidError::UntrustedDomain("trusted.example".to_string()));
    }

    #[test]
    fn svid_empty_domain_list_accepts_any_chained_domain() {
        let mut config = svid_config();
        config.trusted_trust_domains.clear();
        let result = verify_svid(TEST_LEAF_PEM, &config).expect("test: empty domain list");
        assert!(result.trusted);
    }

    #[test]
    fn garbage_svid_pem_does_not_panic() {
        let err = verify_svid("not a certificate", &svid_config()).expect_err("test: garbage");
        assert!(matches!(err, SvidError::ParseError(_)));
    }

    #[test]
    fn strict_mode_rejects_svid_failure() {
        let mut config = crate::config::Config::default();
        config.a2a = svid_config();
        config.a2a.spiffe_strict_validation = true;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-spiffe-id",
            HeaderValue::from_static("spiffe://trusted.example/agent/worker"),
        );
        // Rogue SVID -> strict mode must reject (no identity derived).
        let mut value = HeaderValue::from_str(&header_safe_pem(TEST_ROGUE_LEAF_PEM)).expect("test: header value");
        value.set_sensitive(true);
        headers.insert("x-spiffe-svid", value);
        assert!(derive_external_agent_identity(&headers, &config).is_none());
    }

    #[test]
    fn verified_svid_overrides_asserted_header() {
        let mut config = crate::config::Config::default();
        config.a2a = svid_config();
        let mut headers = HeaderMap::new();
        // Caller asserts a DIFFERENT spiffe id in the header...
        headers.insert(
            "x-spiffe-id",
            HeaderValue::from_static("spiffe://attacker.example/agent/evil"),
        );
        // ...but presents a valid SVID. The SVID SAN must win.
        let value = HeaderValue::from_str(&header_safe_pem(TEST_LEAF_PEM)).expect("test: header value");
        headers.insert("x-spiffe-svid", value);
        let identity = derive_external_agent_identity(&headers, &config).expect("test: svid identity");
        assert_eq!(identity.external_subject, "spiffe://trusted.example/agent/worker");
    }

    #[test]
    fn non_strict_mode_falls_back_to_header_on_svid_failure() {
        let mut config = crate::config::Config::default();
        config.a2a = svid_config();
        config.a2a.spiffe_strict_validation = false;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-spiffe-id",
            HeaderValue::from_static("spiffe://trusted.example/agent/worker"),
        );
        let value = HeaderValue::from_str(&header_safe_pem(TEST_ROGUE_LEAF_PEM)).expect("test: header value");
        headers.insert("x-spiffe-svid", value);
        // Non-strict: rogue SVID fails, but header-asserted id is still honored.
        let identity = derive_external_agent_identity(&headers, &config).expect("test: fallback identity");
        assert_eq!(identity.external_subject, "spiffe://trusted.example/agent/worker");
    }

    // ── JWKS URI security tests ───────────────────────────────────────────────

    /// `normalize_jwks_uri` must reject any non-https scheme immediately so
    /// that `resolve_decoding_key` propagates `InsecureJwksUri` and the token
    /// is denied without any network I/O (fail-closed).
    #[test]
    fn http_jwks_uri_is_rejected_by_normalize() {
        // Plain http — MITM risk.
        assert!(matches!(
            normalize_jwks_uri("http://example.com/.well-known/jwks.json"),
            Err(BearerJwtError::InsecureJwksUri)
        ));
        // Leading whitespace + http — whitespace must not bypass the check.
        assert!(matches!(
            normalize_jwks_uri("  http://example.com/jwks.json  "),
            Err(BearerJwtError::InsecureJwksUri)
        ));
        // No scheme at all.
        assert!(matches!(
            normalize_jwks_uri("example.com/jwks.json"),
            Err(BearerJwtError::InsecureJwksUri)
        ));
        // Loopback http.
        assert!(matches!(
            normalize_jwks_uri("http://127.0.0.1/jwks.json"),
            Err(BearerJwtError::InsecureJwksUri)
        ));
    }

    /// `normalize_jwks_uri` must accept a valid https URI and return the
    /// trimmed slice as the canonical cache key.
    #[test]
    fn https_jwks_uri_is_accepted_and_trimmed() {
        let raw = "  https://auth.example.com/.well-known/jwks.json  ";
        let result = normalize_jwks_uri(raw).expect("test: https uri should be accepted");
        assert_eq!(result, "https://auth.example.com/.well-known/jwks.json");

        // Upper-case HTTPS scheme must also be accepted (case-insensitive).
        let upper = "HTTPS://auth.example.com/jwks.json";
        let result2 = normalize_jwks_uri(upper).expect("test: upper-case HTTPS");
        assert_eq!(result2, upper);
    }

    /// `resolve_decoding_key` must return `InsecureJwksUri` (not attempt a
    /// network fetch) when `jwt_jwks_uri` is configured with an http:// URI,
    /// even without a pre-warmed cache.
    #[test]
    fn resolve_decoding_key_rejects_http_jwks_uri() {
        let mut mcp_cfg = crate::config::McpServerRuntimeConfig::default();
        mcp_cfg.jwt_jwks_uri = Some("http://evil-mitm.example.com/jwks.json".to_string());
        mcp_cfg.jwt_issuer = Some("https://auth.example.com".to_string());
        // The cache is empty; resolution must fail-closed with InsecureJwksUri,
        // never with JwksUnavailable (which would imply a fetch was attempted).
        // DecodingKey doesn't implement Debug so we cannot use expect_err;
        // use a match guard instead.
        let result = resolve_decoding_key(&mcp_cfg, Some("key1"));
        match result {
            Err(BearerJwtError::InsecureJwksUri) => { /* expected */ }
            Err(other) => panic!("expected InsecureJwksUri, got: {other}"),
            Ok(_) => panic!("expected error but resolve_decoding_key succeeded"),
        }
    }

    /// Cache key consistency: the canonical (trimmed) URI written by
    /// `prewarm_remote_jwks` must equal the key used by `cached_remote_jwks`
    /// inside `resolve_decoding_key`. If both normalise via `normalize_jwks_uri`
    /// there is never a mismatch regardless of whitespace in the config.
    #[test]
    fn cached_remote_jwks_key_is_trimmed_uri() {
        // Directly seed the cache with a trimmed key (simulating a successful
        // pre-warm) and then verify that a lookup with a whitespace-padded URI
        // still finds the entry because resolve_decoding_key trims first.
        let canonical = "https://auth.example.com/.well-known/jwks.json";
        let jwks_json = r#"{"keys":[]}"#;
        let jwks: JwkSet = serde_json::from_str(jwks_json).expect("test: parse empty jwks");
        REMOTE_JWKS_CACHE.write().insert(
            canonical.to_string(),
            CachedJwks {
                jwks: Arc::new(jwks),
                fetched_at: std::time::Instant::now(),
            },
        );
        // A config value with surrounding whitespace must still hit the cache.
        let padded = format!("  {canonical}  ");
        let result = cached_remote_jwks(
            normalize_jwks_uri(&padded).expect("test: padded https uri normalises ok"),
            300,
        );
        assert!(
            result.is_some(),
            "cache lookup must succeed when using the normalised (trimmed) key"
        );
    }
}
