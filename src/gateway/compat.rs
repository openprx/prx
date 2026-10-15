//! MCP/A2A compatibility discovery endpoints.
//!
//! Discovery endpoints are public metadata. Tool invocation is gated by the
//! MCP server enable flag, caller identity, exposed tool allowlist, the
//! owner-isolation-aware tool list, trusted scope injection, and the target
//! tool's own SideEffectGate checks.

use super::{AppState, jobs};
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
use ring::signature::{Ed25519KeyPair, KeyPair};
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
/// Request header carrying a caller-supplied idempotency key on
/// `POST /mcp/v1/tools/call`. The length cap is the gateway-wide one applied to
/// `/webhook`'s `X-Idempotency-Key`.
const MCP_IDEMPOTENCY_HEADER: &str = "Idempotency-Key";
/// Window for `mcp_server.rate_limit_rps`, which is documented as a per-caller
/// budget in requests per second.
const MCP_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(1);

/// Process-wide per-caller budget for `POST /mcp/v1/tools/call`.
///
/// Built once and then kept: a change to `mcp_server.rate_limit_rps` is adopted
/// **in place**, so a configuration reload cannot discard the windows callers
/// have already spent. Rebuilding it on every change handed every caller a fresh
/// window, which let a caller that had just been throttled burst again by doing
/// nothing but waiting for an unrelated reload.
static MCP_RATE_LIMITER: LazyLock<RwLock<Option<Arc<super::SlidingWindowRateLimiter>>>> =
    LazyLock::new(|| RwLock::new(None));

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
    workspace_id: String,
    prx_owner_id: String,
    prx_principal_id: String,
}

pub async fn mcp_list_servers(State(state): State<AppState>) -> Json<McpListServersResponse> {
    Json(build_mcp_list_servers_response(&state.config.load_full()))
}

pub async fn mcp_initialize(State(state): State<AppState>) -> Json<McpInitializeResponse> {
    let config = state.config.load_full();
    Json(McpInitializeResponse {
        protocol_version: MCP_PROTOCOL_VERSION,
        server_info: McpServerInfo {
            name: PRX_MCP_SERVER_NAME,
            version: env!("CARGO_PKG_VERSION"),
        },
        capabilities: McpCapabilities {
            tools: !config.mcp_server.admissible_exposed_tools().is_empty(),
            resources: true,
            prompts: false,
        },
    })
}

pub async fn mcp_tools_list(State(state): State<AppState>) -> Json<Vec<McpExposedTool>> {
    Json(exposed_tools(&state.config.load_full()))
}

/// Caller-scoped idempotency for one `POST /mcp/v1/tools/call`.
struct McpIdempotency {
    /// `{prx_owner_id}:{Idempotency-Key}`.
    ///
    /// The ledger's own reservation key already folds in workspace id, session
    /// key and semantic turn id, but for MCP both of the latter are
    /// `mcp:{external_subject}` — subject only, never issuer-qualified — so two
    /// callers that share a subject across issuers would collide on a shared key
    /// string and one could suppress the other's call. `prx_owner_id` is the
    /// authenticated composite `owner:{workspace}:agent:{issuer}:{subject}`,
    /// which is unique per caller.
    key: String,
    /// Durable `MessageEvent` ledger backing reservation and replay. `None` only
    /// when the workspace ledger cannot be opened; the execution service then
    /// refuses an Act-effect call rather than risk a duplicate side effect, and
    /// still runs a read-only one.
    ledger: Option<Arc<dyn crate::memory::Memory>>,
}

/// Resolve the caller-scoped idempotency of one `POST /mcp/v1/tools/call`.
///
/// `Ok(None)` means the caller sent no key: the call then behaves exactly as it
/// did before, so a transport retry is NOT deduplicated. A malformed or
/// over-long key is a client error rather than a silently different key.
fn mcp_idempotency_for_request(
    headers: &HeaderMap,
    identity: &ExternalAgentIdentity,
    memory: &Arc<dyn crate::memory::Memory>,
    workspace_dir: &Path,
) -> Result<Option<McpIdempotency>, (StatusCode, Json<Value>)> {
    let raw_key = super::idempotency_key_from_header(headers, MCP_IDEMPOTENCY_HEADER)
        .map_err(|message| json_error(StatusCode::BAD_REQUEST, message))?;
    Ok(raw_key.map(|raw_key| McpIdempotency {
        key: format!("{}:{raw_key}", identity.prx_owner_id),
        ledger: crate::memory::tool_execution_ledger(memory, workspace_dir),
    }))
}

/// The per-caller limiter for the currently configured budget.
///
/// The same limiter is returned for the life of the process, with the
/// configured budget applied to it, so the windows it holds survive every
/// configuration reload — including one that changes `rate_limit_rps` and one
/// that changes it back.
fn mcp_rate_limiter_in(
    slot: &RwLock<Option<Arc<super::SlidingWindowRateLimiter>>>,
    rate_limit_rps: u32,
) -> Arc<super::SlidingWindowRateLimiter> {
    // The read guard is scoped explicitly: `parking_lot::RwLock` is not
    // reentrant, so it must be gone before the write lock below is taken.
    {
        let guard = slot.read();
        if let Some(limiter) = guard.as_ref() {
            limiter.set_limit_per_window(rate_limit_rps);
            return Arc::clone(limiter);
        }
    }
    let mut guard = slot.write();
    if let Some(limiter) = guard.as_ref() {
        limiter.set_limit_per_window(rate_limit_rps);
        return Arc::clone(limiter);
    }
    let limiter = Arc::new(super::SlidingWindowRateLimiter::new(
        rate_limit_rps,
        MCP_RATE_LIMIT_WINDOW,
        super::RATE_LIMIT_MAX_KEYS_DEFAULT,
    ));
    *guard = Some(Arc::clone(&limiter));
    limiter
}

/// The process-wide limiter, at the currently configured budget.
fn mcp_rate_limiter(rate_limit_rps: u32) -> Arc<super::SlidingWindowRateLimiter> {
    mcp_rate_limiter_in(&MCP_RATE_LIMITER, rate_limit_rps)
}

/// Enforce `mcp_server.rate_limit_rps` for one authenticated caller.
///
/// The key is the issuer-qualified owner id, so one bot token cannot spend
/// another's budget. `0` disables the budget, as it does for every other
/// gateway limiter.
fn mcp_rate_limit_allows(config: &crate::config::Config, caller: &str) -> bool {
    mcp_rate_limiter(config.mcp_server.rate_limit_rps).allow(caller)
}

pub async fn mcp_tools_call(
    State(state): State<AppState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    Json(request): Json<McpToolCallRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let async_mode = jobs::wants_async(uri.query(), &headers);
    let config = state.config.load_full().clone();
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
        let message = if untrusted_forwarded_identity_header(&headers, &config).is_some() {
            "mcp tool call requires a bearer JWT; proxy-asserted identity headers are ignored unless \
             gateway.trust_forwarded_headers is true"
        } else {
            "mcp tool call requires bearer, spiffe, or mtls caller identity"
        };
        json_error(StatusCode::UNAUTHORIZED, message)
    })?;

    if !mcp_rate_limit_allows(&config, &identity.prx_owner_id) {
        return Err(json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "mcp per-caller request budget exceeded",
        ));
    }

    if !config.mcp_server.exposed_tools.iter().any(|tool| tool == &request.name) {
        return Err(json_error(StatusCode::FORBIDDEN, "tool is not exposed through mcp"));
    }

    // V02-R9 / OQ-21: every MCP caller runs owner-isolated, and an isolated
    // caller may run only a tool that enforces per-owner isolation. The
    // execution service is the authoritative gate; refusing here as well gives
    // the client a meaningful status and writes no idempotency reservation or
    // identity binding for a call that cannot run.
    if !crate::tools::execution::is_owner_isolation_aware_tool(&request.name) {
        return Err(json_error(
            StatusCode::FORBIDDEN,
            crate::tools::execution::owner_isolation_refusal(&request.name),
        ));
    }

    let idempotency = mcp_idempotency_for_request(&headers, &identity, &state.mem, &config.workspace_dir)?;

    if let Err(error) = upsert_agent_identity_binding(
        &config.workspace_dir,
        &identity,
        &config.mcp_server.admissible_exposed_tools(),
    ) {
        tracing::warn!(error = %error, "mcp agent identity binding upsert skipped");
    }

    // Resolve against the turn-runtime generation current at admission, which
    // a hot reload replaces — the same pin webhook turns take. The boot-time
    // `AppState::tools_registry` is never reassigned, so reading it would keep
    // serving a tool a reload removed or reconfigured and miss one it added.
    let turn_runtime = state.pin_turn_runtime();
    // Admission check only: the tool is resolved again inside the job, which is
    // where it actually runs, against this same pinned registry.
    if !turn_runtime
        .tools_registry
        .iter()
        .any(|tool| tool.supports_name(&request.name))
    {
        return Err(json_error(StatusCode::NOT_FOUND, "exposed tool is not available"));
    }
    let execution_context = bind_mcp_execution_context(
        Arc::clone(&turn_runtime.tools_registry),
        turn_runtime.mcp_tool.as_ref(),
        turn_runtime.config_generation.id.0.to_string(),
        &config,
        &request.name,
        &identity,
    )
    .map_err(|error| {
        tracing::warn!(tool = %request.name, error = %error, "mcp tool call admission binding failed");
        json_error(
            StatusCode::CONFLICT,
            "tool capability changed or became unavailable during admission",
        )
    })?;

    // An exposed tool can be a shell command, a build, or a sub-agent run —
    // none of which has a bound. Running it inside this handler would tie it to
    // the MCP client's connection; as a job it keeps going if the client drops,
    // and it shows up in `prx tasks list` with a kill path while it runs.
    let tool_name = request.name.clone();
    let job = jobs::submit(
        jobs::KIND_MCP_TOOL_CALL,
        format!("gateway:mcp_tool_call:{tool_name}"),
        run_mcp_tool_call_job(
            Arc::clone(&turn_runtime.tools_registry),
            config,
            request.name,
            request.arguments,
            identity,
            idempotency,
            execution_context,
        ),
    );

    if async_mode {
        return Ok((StatusCode::ACCEPTED, Json(job.accepted_body())));
    }
    Ok(job.wait().await.into_parts())
}

/// The unbounded half of `POST /mcp/v1/tools/call`, run as a job.
///
/// Runs through [`crate::tools::ToolExecutionService`], the same service every
/// other production entry point uses, so an external MCP client gets the
/// identical descriptor -> effect -> policy -> approval -> schema -> audit
/// pipeline as a model-issued call. Before 0.8.126 this called
/// `Tool::execute_named` directly, which meant a fully attacker-controlled
/// request body reached a tool with no autonomy policy, no approval gate, no
/// audit record and no published-schema enforcement.
///
/// Approval is [`crate::tools::DenyApprovalStrategy`]: a remote MCP client has
/// no way to answer an approval prompt, so a tool the autonomy level wants
/// confirmed is refused rather than silently auto-approved.
async fn run_mcp_tool_call_job(
    tools: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    config: Arc<crate::config::Config>,
    tool_name: String,
    args: Value,
    identity: ExternalAgentIdentity,
    idempotency: Option<McpIdempotency>,
    execution_context: crate::tools::ToolExecutionContext,
) -> Result<jobs::JobOutput, String> {
    if !tools.iter().any(|tool| tool.supports_name(&tool_name)) {
        return Err("exposed tool is not available".to_string());
    }
    let outcome = execute_bound_mcp_tool_call(tools, &config, &tool_name, args, idempotency, execution_context).await;
    let body = serde_json::to_value(mcp_tool_call_response(tool_name, identity, outcome))
        .map_err(|error| format!("failed to encode mcp tool result: {error}"))?;
    Ok(jobs::JobOutput::new(StatusCode::OK, body))
}

/// Drive one external MCP tool call through the shared execution service.
///
/// With an `Idempotency-Key` the call is reserved and replayed through the same
/// durable ledger the Chat and Agent entry points use, so a transport retry of
/// the identical request runs the side effect once and returns the first
/// outcome. Reusing the key for a different command is refused rather than
/// silently executed. Without a key nothing is deduplicated: a retry executes
/// again, which is the contract this route had before the key was accepted.
#[cfg(test)]
async fn execute_mcp_tool_call(
    tools: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    config: &crate::config::Config,
    tool_name: &str,
    args: Value,
    identity: &ExternalAgentIdentity,
    idempotency: Option<McpIdempotency>,
) -> crate::tools::ToolExecutionOutcome {
    execute_bound_mcp_tool_call(
        tools,
        config,
        tool_name,
        args,
        idempotency,
        mcp_execution_context(identity),
    )
    .await
}

fn mcp_tool_execution_service(
    tools: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    config: &crate::config::Config,
) -> crate::tools::ToolExecutionService {
    crate::tools::ToolExecutionService::from_shared_boxed_registry(
        tools,
        Arc::new(crate::tools::SecurityEffectPolicy::new(
            crate::runtime::bootstrap::build_security_policy(config),
        )),
        Arc::new(crate::tools::DenyApprovalStrategy),
        Arc::new(crate::tools::AdapterOwnedPreparation),
        Arc::new(crate::tools::TracingToolExecutionAudit),
    )
}

/// Pin the exact executable capability admitted by the external MCP route.
///
/// This binding carries identity only: execution still applies owner
/// isolation, effect policy, approval, schema and idempotency gates. Capturing
/// it before the detached job is submitted makes any schema, generation or
/// dynamic backend change fail closed when the job starts.
fn bind_mcp_execution_context(
    tools: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    mcp_tool: Option<&Arc<crate::tools::McpTool>>,
    config_generation_id: String,
    config: &crate::config::Config,
    tool_name: &str,
    identity: &ExternalAgentIdentity,
) -> anyhow::Result<crate::tools::ToolExecutionContext> {
    let service = mcp_tool_execution_service(Arc::clone(&tools), config);
    let sources = crate::tools::capability::catalog::CatalogSources {
        config_generation_id,
        mcp: mcp_tool.map_or_else(Vec::new, |tool| tool.capability_snapshot()),
        ..Default::default()
    };
    let catalog = Arc::new(crate::tools::capability::catalog::CapabilityCatalog::build(
        tools.as_ref(),
        &sources,
        &crate::tools::capability::catalog::SurfacePolicy::default(),
    )?);
    catalog.check_parity(&service)?;
    let entry = catalog
        .entry_for_public_name(tool_name)
        .ok_or_else(|| anyhow::anyhow!("admitted tool has no capability catalog entry"))?;
    let mut context = mcp_execution_context(identity);
    let binding = crate::tools::capability::execution_binding::ExecutionBinding::issued(
        crate::causal_tree::ids::DecisionId::new_runtime(),
        entry,
        Arc::clone(&catalog),
        context.envelope.clone(),
        true,
    );
    context.execution_bindings = Some(Arc::new(std::collections::BTreeMap::from([(
        tool_name.to_string(),
        binding,
    )])));
    context.execution_trace = Some(crate::tools::capability::execution_trace::ExecutionTrace::default());
    Ok(context)
}

async fn execute_bound_mcp_tool_call(
    tools: Arc<Vec<Box<dyn crate::tools::Tool>>>,
    config: &crate::config::Config,
    tool_name: &str,
    args: Value,
    idempotency: Option<McpIdempotency>,
    execution_context: crate::tools::ToolExecutionContext,
) -> crate::tools::ToolExecutionOutcome {
    let mut service = mcp_tool_execution_service(tools, config);
    // Runtime-only keys in the request body are dropped and re-authored by the
    // service from this context, so a forged `_zc_scope` / `_zc_approval_grant`
    // in the body cannot survive into execution.
    let mut command = crate::tools::ToolExecutionCommand::new(tool_name, normalize_mcp_arguments(args));
    if let Some(idempotency) = idempotency {
        command = command.with_idempotency_key(idempotency.key);
        // No ledger means no reservation. The service then fails an Act-effect
        // call closed — "durable tool idempotency ledger is unavailable" reaches
        // the client as the error text — and lets a read-only one through.
        if let Some(ledger) = idempotency.ledger {
            service = service.with_idempotency_memory(ledger);
        }
    }
    service.execute(command, execution_context, None).await
}

/// Project a service outcome onto the MCP wire response.
///
/// The success and tool-failure shapes are byte-identical to the pre-0.8.126
/// direct-execution path. Service-level refusals (policy deny, approval
/// required, schema violation, unknown tool) reuse the same envelope with
/// `is_error: true` so a client sees one rejection structure regardless of
/// which gate refused.
fn mcp_tool_call_response(
    tool_name: String,
    identity: ExternalAgentIdentity,
    outcome: crate::tools::ToolExecutionOutcome,
) -> McpToolCallResponse {
    let (text, is_error) = match outcome.result {
        Some(result) if result.success => (result.output, false),
        Some(result) => (result.error.unwrap_or(result.output), true),
        None => (outcome.error.unwrap_or_else(|| outcome.model_content.clone()), true),
    };
    McpToolCallResponse {
        content: vec![McpToolContent { kind: "text", text }],
        is_error,
        tool_name,
        caller: identity,
    }
}

pub async fn a2a_identity(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<A2aIdentityResponse>, (StatusCode, Json<Value>)> {
    let config = state.config.load_full();
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
    let config = state.config.load_full();
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
        let config = state.config.load_full();
        (build_signed_agent_card(&config), config.a2a.card_ttl_seconds)
    };
    let cache = format!("max-age={ttl}");
    ([(header::CACHE_CONTROL, cache)], Json(card)).into_response()
}

/// `GET /a2a/v1/.well-known/jwks.json` — publishes the Ed25519 verification key
/// for the Agent Card JWS so peers can verify card authenticity.
pub async fn a2a_jwks(State(state): State<AppState>) -> Result<Json<A2aJwks>, (StatusCode, Json<Value>)> {
    let config = state.config.load_full();
    let signer = resolve_card_signer(&config.a2a)
        .ok_or_else(|| json_error(StatusCode::INTERNAL_SERVER_ERROR, "card signing key is unavailable"))?;
    Ok(Json(signer.jwks()))
}

fn build_mcp_list_servers_response(config: &crate::config::Config) -> McpListServersResponse {
    McpListServersResponse {
        servers: vec![McpServerCard {
            name: PRX_MCP_SERVER_NAME,
            protocol_version: MCP_PROTOCOL_VERSION,
            enabled: true,
            bind: config.mcp_server.bind.clone(),
            tools: exposed_tools(config),
        }],
    }
}

fn build_a2a_identity_response(config: &crate::config::Config) -> A2aIdentityResponse {
    A2aIdentityResponse {
        agent_id: config.a2a.agent_id.clone(),
        spiffe_id: config.a2a.spiffe_id.clone(),
        enabled: true,
        bind: config.a2a.bind.clone(),
        capabilities: A2aCapabilities {
            tools: admissible_exposed_tool_names(config),
            modalities: vec!["text"],
            skills: vec!["memory_qa", "document_grounding"],
        },
        auth: vec!["bearer-jwt", "mtls", "spiffe"],
    }
}

/// The exposed tools an external caller can actually run: `exposed_tools`
/// narrowed to the owner-isolation-aware list. A name outside it always
/// answers 403, so discovery must not advertise it.
fn admissible_exposed_tool_names(config: &crate::config::Config) -> Vec<String> {
    config
        .mcp_server
        .admissible_exposed_tools()
        .into_iter()
        .map(ToString::to_string)
        .collect()
}

fn exposed_tools(config: &crate::config::Config) -> Vec<McpExposedTool> {
    config
        .mcp_server
        .admissible_exposed_tools()
        .into_iter()
        .map(|name| McpExposedTool {
            risk: mcp_tool_risk(name),
            name: name.to_string(),
            default_exposed: true,
        })
        .collect()
}

/// The declared side effect of an exposed tool.
///
/// This is the predicate `ToolDescriptor::effect` is derived from, so discovery
/// and execution agree. Reporting a fixed `readonly` would advertise a mutating
/// tool an operator added to `exposed_tools` as harmless.
fn mcp_tool_risk(name: &str) -> &'static str {
    if crate::security::policy::is_read_only_tool(name) {
        "readonly"
    } else {
        "act"
    }
}

/// Request headers through which a reverse proxy asserts the caller identity
/// of an MCP tool call.
///
/// Every one of them is an ordinary request header: any client can send any
/// value. They are therefore honored only when `gateway.trust_forwarded_headers`
/// declares that a proxy in front of the gateway removes or overwrites them. A
/// certificate carried in `x-spiffe-svid` / `x-client-cert-pem` is no exception:
/// verifying its chain proves nothing about possession of its private key, and
/// a certificate is public, so a copy of a peer's SVID replays as that peer.
const FORWARDED_IDENTITY_HEADERS: [&str; 5] = [
    "x-spiffe-id",
    "x-spiffe-svid",
    "x-client-cert-pem",
    "x-client-cert-subject",
    "x-forwarded-client-cert",
];

/// Whether each entry of [`FORWARDED_IDENTITY_HEADERS`] has already been
/// warned about on an untrusted request in this process.
static UNTRUSTED_IDENTITY_HEADER_WARNED: [std::sync::atomic::AtomicBool; FORWARDED_IDENTITY_HEADERS.len()] =
    [const { std::sync::atomic::AtomicBool::new(false) }; FORWARDED_IDENTITY_HEADERS.len()];

/// Log that an untrusted proxy-asserted identity header was ignored.
///
/// The line comes before any per-caller budget (there is no identity yet), so
/// logging every sighting at `warn` let any client — or a sidecar that injects
/// the header on every call — flood the log. The first sighting of each header
/// name in a process is a warning; every later one is `debug`. Only the header
/// name is ever logged: its value is attacker-controlled.
fn log_untrusted_forwarded_identity_header(header: &'static str) {
    let first_sighting = FORWARDED_IDENTITY_HEADERS
        .iter()
        .position(|name| *name == header)
        .and_then(|index| UNTRUSTED_IDENTITY_HEADER_WARNED.get(index))
        .is_some_and(|warned| !warned.swap(true, std::sync::atomic::Ordering::Relaxed));
    if first_sighting {
        tracing::warn!(
            header,
            "ignoring a proxy-asserted caller identity header because gateway.trust_forwarded_headers is false; \
             later requests carrying this header are logged at debug level"
        );
    } else {
        tracing::debug!(
            header,
            "ignoring a proxy-asserted caller identity header because gateway.trust_forwarded_headers is false"
        );
    }
}

/// The first proxy-asserted identity header present on a request that the
/// configuration does not trust, if any.
fn untrusted_forwarded_identity_header(headers: &HeaderMap, config: &crate::config::Config) -> Option<&'static str> {
    if config.gateway.trust_forwarded_headers {
        return None;
    }
    FORWARDED_IDENTITY_HEADERS
        .into_iter()
        .find(|name| headers.contains_key(*name))
}

fn derive_external_agent_identity(
    headers: &HeaderMap,
    config: &crate::config::Config,
) -> Option<ExternalAgentIdentity> {
    let workspace_id = config.workspace_dir.to_string_lossy();
    // Proxy-asserted identities (branches 1 and 2 below) exist only behind a
    // trusted proxy. Without one they never mint an identity and never pre-empt
    // a bearer: the request authenticates by bearer, as anonymous when auth is
    // not required, or not at all. Only the header name is logged; its value is
    // attacker-controlled.
    let trust_forwarded_identity = config.gateway.trust_forwarded_headers;
    if let Some(header) = untrusted_forwarded_identity_header(headers, config) {
        log_untrusted_forwarded_identity_header(header);
    }
    if trust_forwarded_identity && let Some(spiffe_id) = header_value(headers, "x-spiffe-id") {
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
    if trust_forwarded_identity
        && let Some(subject) =
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

    match fetch_remote_jwks(&uri).await {
        Ok(jwks) => {
            REMOTE_JWKS_CACHE.write().insert(
                uri,
                CachedJwks {
                    jwks: Arc::new(jwks),
                    fetched_at: Instant::now(),
                },
            );
        }
        Err(error) => {
            tracing::warn!(error = %error, "remote jwks refresh failed; relying on existing cache (fail-closed if stale)");
        }
    }
}

/// Fetch + parse a remote JWKS document.
///
/// Deliberately async rather than `reqwest::blocking` inside `spawn_blocking`:
/// a blocking HTTP call holds one of the process's finite blocking-pool threads
/// for the whole round trip (up to `JWKS_FETCH_TIMEOUT_SECS`), while the async
/// client holds none. With unbounded concurrent gateway traffic, a slow or
/// unreachable JWKS endpoint could otherwise drain the pool on its own.
/// The bounded timeout is retained so a hung endpoint cannot stall the caller.
async fn fetch_remote_jwks(uri: &str) -> Result<JwkSet, anyhow::Error> {
    use anyhow::Context as _;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(JWKS_FETCH_TIMEOUT_SECS))
        .build()
        .context("failed to build jwks http client")?;
    let response = client
        .get(uri)
        .send()
        .await
        .context("jwks endpoint request failed")?
        .error_for_status()
        .context("jwks endpoint returned an error status")?;
    let body = response.text().await.context("failed to read jwks response body")?;
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

/// The fixed issuer component every non-bearer MCP origin identifies itself by.
///
/// `prx_principal_id` is `agent:{issuer}:{subject}` and `prx_owner_id` wraps it
/// again, and after C03 that string is an authorization boundary rather than
/// just a label. The components are joined with `:` and are NOT escaped, because
/// real issuers and subjects contain `:` routinely — JWT `iss` URLs, `spiffe://`
/// subjects, the literal `anonymous:mcp` — and escaping them would move every
/// real caller's owner id, orphaning its history under the owner predicate and
/// resetting its idempotency scope.
///
/// Ambiguity is excluded by construction instead. The issuer component is only
/// ever one of these three fixed names or the single configured
/// `mcp_server.jwt_issuer` (the JWT `iss` claim is validated against exactly
/// that value before it reaches here), and the three fixed names share no
/// prefix with one another. Two different `(issuer, subject)` pairs can
/// therefore only compose to the same principal id if the configured issuer is
/// one of these names or begins with one of them followed by `:`, which
/// [`crate::config::Config::validate`] refuses at config load.
pub const FIXED_EXTERNAL_ISSUERS: [&str; 3] = ["spiffe", "mtls", "anonymous"];

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
        workspace_id: workspace_id.to_string(),
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

/// Coerce an MCP request body into the JSON object the execution service
/// expects, dropping every caller-supplied runtime-only key up front.
///
/// The service strips these again in `normalize_arguments` before it authors
/// the authoritative ones, so this is defence in depth rather than the only
/// scrub: it keeps a forged `_zc_*` / `_prx_*` key out of the audited input
/// hash and out of the schema-validation candidate as well.
fn normalize_mcp_arguments(args: Value) -> Value {
    let mut args = match args {
        Value::Object(map) => Value::Object(map),
        _ => serde_json::json!({}),
    };
    if let Some(map) = args.as_object_mut() {
        crate::tools::execution::strip_runtime_only_args(map);
    }
    args
}

/// The execution context an authenticated external MCP caller runs under.
///
/// The service re-authors `_zc_scope` from this envelope, so the scope a tool
/// sees is the gateway-authenticated identity and nothing else. `source` is
/// `Gateway` and the channel is `mcp`, which makes
/// `ToolExecutionContext::is_local_operator` false by construction — a remote
/// MCP client can never reach the destructive `gateway` actions that marker
/// guards.
fn mcp_execution_context(identity: &ExternalAgentIdentity) -> crate::tools::ToolExecutionContext {
    let envelope = crate::runtime::envelope::RuntimeEnvelope::gateway(
        identity.workspace_id.clone(),
        format!("mcp:{}", identity.external_subject),
        "mcp",
        identity.external_subject.clone(),
        identity.external_subject.clone(),
        crate::memory::MemoryVisibility::Workspace,
    )
    .with_owner_id(identity.prx_owner_id.clone())
    // C03 / V02-R4: the session key and sender above are the external *subject*
    // only, so two identities presenting the same subject under different
    // issuers share both. Mark the envelope owner-isolated: every `session`- and
    // `private`-visibility read made under it must additionally match
    // `prx_owner_id`, which is the only part of this identity that carries the
    // issuer.
    .with_owner_isolation();
    crate::tools::ToolExecutionContext::new(envelope, "gateway")
}

fn upsert_agent_identity_binding(
    workspace_dir: &Path,
    identity: &ExternalAgentIdentity,
    capabilities: &[&str],
) -> anyhow::Result<()> {
    let db_path = workspace_dir.join("memory").join("brain.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db_path)?;
    // This writes the same brain.db the pooled memory store owns, so it must
    // agree with the pool on what a held lock means: wait for it (warning
    // periodically), never fail the binding upsert with `SQLITE_BUSY`.
    crate::runtime::sqlite_pool::install_busy_handler(&conn)?;
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
            tools: admissible_exposed_tool_names(config),
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
mod tests;
