//! The rf contract's feature gate (AC-2): `GET /api/capabilities` and
//! `GET /api/agents/catalog`, and the guard every rf contract route shares
//! (the agent routes of AC-1a1 are in `agents.rs`).
//!
//! Both are reads. They change no configuration, write no store and are not
//! gated by autonomy. Neither body carries a credential, a key id or a URL
//! with userinfo: provider labels are the redacted ones every model listing
//! shows, and readiness is a boolean. Field names are a contract with rf; the
//! tokens' meanings live in `docs/rf-contract/capability-tokens.md`.
//!
//! Every error of these two routes uses the rf contract's coded shape
//! `{"error":{"code","message"}}` ([`guard`], [`method_not_allowed`]); the
//! shared bodies of the other `/api` routes are not changed. The shared 429
//! of the API rate limit still applies.

use super::AppState;
use crate::config::Config;
use crate::config::agent_name::RESERVED_AGENT_NAMES;
use crate::config::manage::service::{ModelFilter, model_catalog};
use crate::providers::ProviderId;
use crate::providers::catalog::ModelOperation;
use crate::providers::credential_readiness::{reference_ready, selection_references};
use crate::providers::credentials::CredentialRef;
use crate::security::AutonomyLevel;
use crate::tools::capability::contract::SideEffectClass;
use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::borrow::Cow;

/// The tokens effective in this running daemon, sorted ascending. A token is
/// listed only when its whole earliest usable form is on main.
const CAPABILITY_TOKENS: [&str; 16] = [
    "agent_catalog_v1",
    "agent_crud_v1",
    "agent_limits_v1",
    "agent_main_readback_v1",
    "agent_memory_purge_v1",
    "capabilities_v1",
    "history_erase_v1",
    "message_context_delivery_v1",
    "message_delete_v1",
    "message_forget_writes_v1",
    "message_idempotency_v1",
    "message_target_v1",
    "owner_id_stable_v1",
    "session_key_v1",
    "sync_snapshot_v1",
    "sync_ws_v1",
];

/// Tokens effective only while `gateway.targeted_agents` is on in this
/// running daemon; listed under `available` otherwise.
const TARGETED_AGENTS_TOKEN: &str = "targeted_agents_v1";

/// The effective token list of this running daemon, sorted ascending, and
/// the tokens present in this build but switched off or pending a restart.
fn advertised_tokens(state: &AppState) -> (Vec<&'static str>, Vec<&'static str>) {
    let mut capabilities = CAPABILITY_TOKENS.to_vec();
    let mut available = Vec::new();
    // `effective` exactly as `GET /api/gateway/targeted-agents` reports it.
    if state.config.load_full().gateway.targeted_agents {
        capabilities.push(TARGETED_AGENTS_TOKEN);
        capabilities.sort_unstable();
    } else {
        available.push(TARGETED_AGENTS_TOKEN);
    }
    if state.mem.sync_store().is_none() {
        capabilities.retain(|token| !matches!(*token, "sync_snapshot_v1" | "sync_ws_v1"));
        available.extend(["sync_snapshot_v1", "sync_ws_v1"]);
        available.sort_unstable();
    }
    (capabilities, available)
}

/// The version of this capabilities document.
const CAPABILITIES_API: u32 = 1;

/// Session-key prefixes PRX mints itself (`console-<uuid>`).
pub(super) const RESERVED_SESSION_KEY_PREFIXES: [&str; 1] = ["console-"];

/// Session-key prefixes reserved for callers: PRX never mints one.
const CALLER_RESERVED_SESSION_KEY_PREFIXES: [&str; 1] = ["rf-"];

const PRX_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The git commit the binary was built from (`build.rs`), or `unknown`.
const PRX_COMMIT: &str = match option_env!("PRX_BUILD_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

/// Query keys that carry a credential; a credential never travels in a URL.
const CREDENTIAL_QUERY_KEYS: [&str; 6] = ["access_token", "api_key", "auth", "authorization", "bearer", "token"];

#[derive(Serialize)]
struct CapabilitiesBody {
    engine: &'static str,
    engine_instance_id: String,
    prx_version: &'static str,
    commit: &'static str,
    api: u32,
    capabilities: Vec<&'static str>,
    available: Vec<&'static str>,
}

#[derive(Serialize)]
struct CatalogModel {
    provider: String,
    model: String,
    /// Omitted when the credential lookup itself failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    credentials_ready: Option<bool>,
}

#[derive(Serialize)]
struct CatalogTool {
    name: String,
    read_only: bool,
}

#[derive(Serialize)]
struct CatalogDefaults {
    autonomy: AutonomyLevel,
}

/// The catalog body. `revision` is `None` only while the body is hashed.
#[derive(Serialize)]
struct CatalogBody {
    prx_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
    capabilities: Vec<&'static str>,
    models: Vec<CatalogModel>,
    tools: Vec<CatalogTool>,
    reserved_names: [&'static str; 2],
    defaults: CatalogDefaults,
    reserved_session_key_prefixes: [&'static str; 1],
    caller_reserved_session_key_prefixes: [&'static str; 1],
}

/// Why the catalog could not be assembled. Logged without payload.
#[derive(Debug, Clone, Copy, thiserror::Error)]
enum CatalogError {
    #[error("the gateway is draining")]
    Draining,
    #[error("the tool surface could not be published")]
    Surface,
    #[error("the model catalog reader failed")]
    Models,
    #[error("the catalog body could not be encoded")]
    Encode,
}

impl IntoResponse for CatalogError {
    fn into_response(self) -> Response {
        coded_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_unavailable",
            &self.to_string(),
        )
    }
}

/// The rf contract's coded error body.
pub(super) fn coded_error(status: StatusCode, code: &str, message: &str) -> Response {
    coded_error_with(status, code, message, None)
}

/// The coded error body with the one extra field its code carries
/// (`current_revision` of a 409, `revision` of a 428).
pub(super) fn coded_error_with(
    status: StatusCode,
    code: &str,
    message: &str,
    extra: Option<(&'static str, &str)>,
) -> Response {
    coded_error_value(
        status,
        code,
        message,
        extra.map(|(key, value)| (key, serde_json::Value::String(value.to_owned()))),
    )
}

/// The coded error body with one extra field of any JSON type (for example
/// the numeric `retry_after_ms` of a `429`).
pub(super) fn coded_error_value(
    status: StatusCode,
    code: &str,
    message: &str,
    extra: Option<(&'static str, serde_json::Value)>,
) -> Response {
    let mut error = serde_json::json!({ "code": code, "message": message });
    if let (Some((key, value)), Some(object)) = (extra, error.as_object_mut()) {
        object.insert(key.to_owned(), value);
    }
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

/// The one query parameter an rf contract route takes, on writes only.
const CONFIRM_QUERY_KEY: &str = "confirm_high_risk";

/// The authentication and query guard of the rf contract routes. A
/// credential in the query string is refused as `401 auth_failed` (even
/// beside a valid bearer); then the bearer is checked exactly as the shared
/// authentication does, and its principal is handed to the handler; then
/// any query parameter is `400 field_invalid`, except one
/// `confirm_high_risk=true|false` on a `PUT` or `DELETE`. Nothing of the URL
/// is echoed or logged.
pub(super) async fn guard(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let query = request.uri().query().unwrap_or_default();
    let pairs = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key).to_ascii_lowercase(), decode(value))
        })
        .collect::<Vec<_>>();
    if pairs
        .iter()
        .any(|(key, value)| CREDENTIAL_QUERY_KEYS.contains(&key.as_str()) || super::manage::looks_secret(value))
    {
        return coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a credential is never accepted in a URL; send the bearer token in the Authorization header",
        );
    }
    let Some(principal) = super::authenticated_principal(&state, request.headers()) else {
        return coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a valid bearer token is required",
        );
    };
    let write = matches!(*request.method(), Method::PUT | Method::DELETE);
    let confirm_only = write
        && pairs.len() == 1
        && pairs
            .iter()
            .all(|(key, value)| key == CONFIRM_QUERY_KEY && (value == "true" || value == "false"));
    let snapshot_query = request.method() == Method::GET
        && request.uri().path().ends_with("/sync/snapshot")
        && pairs
            .iter()
            .all(|(key, _)| matches!(key.as_str(), "page_token" | "limit"));
    if !pairs.is_empty() && !confirm_only && !snapshot_query {
        return coded_error(
            StatusCode::BAD_REQUEST,
            "field_invalid",
            "the only query parameter is confirm_high_risk=true|false, on PUT and DELETE",
        );
    }
    request.extensions_mut().insert(principal);
    next.run(request).await
}

/// The query and authentication rule of an rf-shaped session request (a
/// `POST /api/sessions` with `session_key`, a message with
/// `client_message_id`), in [`guard`]'s order: a credential in the query is
/// `401 auth_failed`; a caller that is not owner-authenticated (pairing off)
/// is `401 auth_failed`; any query parameter is `400 field_invalid`, except
/// one `mode=async` when `admit_async`. `None` admits the request.
pub(super) fn rf_session_refusal(
    query: Option<&str>,
    owner_authenticated: bool,
    admit_async: bool,
) -> Option<Response> {
    let pairs = query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key).to_ascii_lowercase(), decode(value))
        })
        .collect::<Vec<_>>();
    if pairs
        .iter()
        .any(|(key, value)| CREDENTIAL_QUERY_KEYS.contains(&key.as_str()) || super::manage::looks_secret(value))
    {
        return Some(coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a credential is never accepted in a URL; send the bearer token in the Authorization header",
        ));
    }
    if !owner_authenticated {
        return Some(coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a paired bearer token is required; these requests are refused while pairing is off",
        ));
    }
    let async_only = admit_async
        && pairs.len() == 1
        && pairs
            .iter()
            .all(|(key, value)| key == "mode" && value.eq_ignore_ascii_case("async"));
    if !pairs.is_empty() && !async_only {
        return Some(coded_error(
            StatusCode::BAD_REQUEST,
            "field_invalid",
            if admit_async {
                "the only query parameter is mode=async"
            } else {
                "this request takes no query parameter"
            },
        ));
    }
    None
}

/// Whether the request confirms a high-risk change
/// (`?confirm_high_risk=true`, as [`guard`] admitted it).
pub(super) fn confirms_high_risk(uri: &Uri) -> bool {
    uri.query().is_some_and(|query| {
        query.split('&').any(|pair| {
            pair.split_once('=').is_some_and(|(key, value)| {
                decode(key).eq_ignore_ascii_case(CONFIRM_QUERY_KEY) && decode(value) == "true"
            })
        })
    })
}

fn decode(text: &str) -> String {
    let spaced = text.replace('+', " ");
    urlencoding::decode(&spaced).map_or_else(|_| spaced.clone(), Cow::into_owned)
}

/// Any method other than `GET` (and `HEAD`) on a read-only route.
pub(super) async fn method_not_allowed() -> Response {
    method_not_allowed_allowing("GET,HEAD")
}

/// `405 method_not_allowed` naming the methods the route takes.
pub(super) fn method_not_allowed_allowing(allow: &'static str) -> Response {
    let mut response = coded_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        &format!("this route accepts only {allow}"),
    );
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(allow));
    response
}

/// `GET /api/capabilities`: the PRX version and commit, and the tokens
/// effective in this running daemon and those only available in this build.
pub(super) async fn get_capabilities(State(state): State<AppState>) -> Response {
    let Some(owner) = state.pairing.install_owner() else {
        return coded_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_unavailable",
            "install owner identity unavailable",
        );
    };
    let (capabilities, available) = advertised_tokens(&state);
    Json(CapabilitiesBody {
        engine: "prx",
        engine_instance_id: owner.as_str().to_owned(),
        prx_version: PRX_VERSION,
        commit: PRX_COMMIT,
        api: CAPABILITIES_API,
        capabilities,
        available,
    })
    .into_response()
}

/// `GET /api/agents/catalog`: what an rf agent can be configured with.
pub(super) async fn get_catalog(State(state): State<AppState>) -> Response {
    match assemble_catalog(&state).await {
        Ok(body) => Json(body).into_response(),
        Err(error) => {
            tracing::warn!(reason = %error, "agent catalog unavailable");
            error.into_response()
        }
    }
}

async fn assemble_catalog(state: &AppState) -> Result<CatalogBody, CatalogError> {
    let runtime = state.pin_turn_state();
    let tools = published_tools(state, &runtime).await?;
    let config = std::sync::Arc::clone(&runtime.config_generation().effective);
    let autonomy = config.autonomy.level;
    let models = crate::runtime::blocking::spawn_blocking(move || catalog_models(&config))
        .await
        .map_err(|_| CatalogError::Models)?;
    let mut body = CatalogBody {
        prx_version: PRX_VERSION,
        revision: None,
        capabilities: advertised_tokens(state).0,
        models,
        tools,
        reserved_names: RESERVED_AGENT_NAMES,
        defaults: CatalogDefaults { autonomy },
        reserved_session_key_prefixes: RESERVED_SESSION_KEY_PREFIXES,
        caller_reserved_session_key_prefixes: CALLER_RESERVED_SESSION_KEY_PREFIXES,
    };
    // Canonical JSON: fixed field order, sorted arrays, no `revision`.
    let canonical = serde_json::to_vec(&body).map_err(|_| CatalogError::Encode)?;
    body.revision = Some(hex::encode(Sha256::digest(&canonical)));
    Ok(body)
}

/// The names of the catalog's `tools`, sorted; `None` when the surface
/// could not be published.
pub(super) async fn catalog_tool_names(state: &AppState) -> Option<Vec<String>> {
    let runtime = state.pin_turn_state();
    let tools = published_tools(state, &runtime).await.ok()?;
    Some(tools.into_iter().map(|tool| tool.name).collect())
}

/// Turn controls the runtime adds or withholds by itself, never as a member
/// of an agent's list: `capability_search` is offered only while something
/// is left to discover (an explicit list publishes every member directly),
/// and a Console turn never exposes `stay_silent`.
const TURN_CONTROL_TOOLS: [&str; 2] = ["capability_search", "stay_silent"];

/// Main's published tool surface without the tools no target can reach or
/// list, sorted by name.
async fn published_tools(
    state: &AppState,
    runtime: &crate::gateway::GatewayTurnState,
) -> Result<Vec<CatalogTool>, CatalogError> {
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cleanup =
        crate::runtime::turn_cleanup::CleanupScope::new(Some(&runtime.publication_slot().cleanup), cancellation)
            .map_err(|_| CatalogError::Draining)?;
    let service = cleanup
        .run(Box::pin(async {
            Ok::<_, anyhow::Error>(std::sync::Arc::clone(runtime.publish(state).await?))
        }))
        .await
        .map_err(|_| CatalogError::Surface)?;
    let hidden =
        |name: &str| super::sessions::TARGET_EXCLUDED_TOOLS.contains(&name) || TURN_CONTROL_TOOLS.contains(&name);
    let mut tools = service
        .publication_scope()
        .catalog_view()
        .iter_entries()
        .filter(|entry| !hidden(&entry.public_name) && !hidden(&entry.root))
        .map(|entry| CatalogTool {
            name: entry.public_name.clone(),
            read_only: entry.effect == SideEffectClass::ReadOnly,
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools.dedup_by(|left, right| left.name == right.name);
    Ok(tools)
}

/// The chat-capable rows of the model catalog, sorted by provider then
/// model. A row is ready when any way a turn can select it (the provider's own
/// selection, a model route, a named agent) yields a usable credential, not
/// ready when none does, and its readiness is omitted when a lookup failed
/// and none succeeded.
fn catalog_models(config: &Config) -> Vec<CatalogModel> {
    let filter = ModelFilter {
        provider: None,
        operation: Some(ModelOperation::Chat),
        executable: false,
    };
    // One store lookup per (provider identity, reference): most rows share
    // their provider's own selection.
    let mut answers: Vec<(ProviderId, CredentialRef, Option<bool>)> = Vec::new();
    let mut lookup_failed = false;
    let mut models = Vec::new();
    for entry in model_catalog(config, &filter) {
        let identity = ProviderId::of(&entry.provider);
        let mut ready = Some(false);
        for reference in selection_references(config, &entry.provider, &entry.model) {
            let known = answers
                .iter()
                .find(|(id, known, _)| *id == identity && *known == reference)
                .map(|(_, _, answer)| *answer);
            let answer = known.unwrap_or_else(|| {
                let answer = reference_ready(config, &entry.provider, &reference).ok();
                lookup_failed |= answer.is_none();
                answers.push((identity.clone(), reference, answer));
                answer
            });
            ready = match (ready, answer) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (None, _) | (_, None) => None,
                (Some(false), Some(false)) => Some(false),
            };
            if ready == Some(true) {
                break;
            }
        }
        models.push(CatalogModel {
            provider: entry.provider,
            model: entry.model,
            credentials_ready: ready,
        });
    }
    if lookup_failed {
        tracing::warn!("agent catalog: a provider credential lookup failed; its readiness is omitted");
    }
    models.sort_by(|left, right| (&left.provider, &left.model).cmp(&(&right.provider, &right.model)));
    models.dedup_by(|left, right| left.provider == right.provider && left.model == right.model);
    models
}
