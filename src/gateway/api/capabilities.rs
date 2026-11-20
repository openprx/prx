//! The rf contract's feature gate (AC-2): `GET /api/capabilities` and
//! `GET /api/agents/catalog`.
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
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::borrow::Cow;

/// The tokens effective in this running daemon, sorted ascending. A token is
/// listed only when its whole earliest usable form is on main.
const CAPABILITY_TOKENS: [&str; 2] = ["agent_catalog_v1", "capabilities_v1"];

/// Tokens present in this build but switched off or pending a restart,
/// sorted ascending.
const AVAILABLE_TOKENS: &[&str] = &[];

/// The version of this capabilities document.
const CAPABILITIES_API: u32 = 1;

/// Session-key prefixes PRX mints itself (`console-<uuid>`).
const RESERVED_SESSION_KEY_PREFIXES: [&str; 1] = ["console-"];

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
    prx_version: &'static str,
    commit: &'static str,
    api: u32,
    capabilities: &'static [&'static str],
    available: &'static [&'static str],
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
    capabilities: [&'static str; 2],
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
fn coded_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

/// The authentication and query guard of the two routes. A credential in
/// the query string is refused as `401 auth_failed` (even beside a valid
/// bearer); then the bearer is checked exactly as the shared
/// authentication does; then any other query parameter is `400
/// field_invalid`, since neither route takes one. Nothing of the URL is
/// echoed or logged.
pub(super) async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
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
    if super::authenticated_principal(&state, request.headers()).is_none() {
        return coded_error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "a valid bearer token is required",
        );
    }
    if !pairs.is_empty() {
        return coded_error(
            StatusCode::BAD_REQUEST,
            "field_invalid",
            "this route takes no query parameters",
        );
    }
    next.run(request).await
}

fn decode(text: &str) -> String {
    let spaced = text.replace('+', " ");
    urlencoding::decode(&spaced).map_or_else(|_| spaced.clone(), Cow::into_owned)
}

/// Any method other than `GET` (and `HEAD`) on the two routes.
pub(super) async fn method_not_allowed() -> Response {
    let mut response = coded_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "this route accepts only GET",
    );
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("GET,HEAD"));
    response
}

/// `GET /api/capabilities`: the PRX version and commit, and the tokens
/// effective in this running daemon and those only available in this build.
pub(super) async fn get_capabilities() -> Response {
    Json(CapabilitiesBody {
        prx_version: PRX_VERSION,
        commit: PRX_COMMIT,
        api: CAPABILITIES_API,
        capabilities: &CAPABILITY_TOKENS,
        available: AVAILABLE_TOKENS,
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
    let runtime = state.pin_turn_runtime();
    let tools = published_tools(state, &runtime).await?;
    let config = std::sync::Arc::clone(&runtime.config_generation.effective);
    let autonomy = config.autonomy.level;
    let models = crate::runtime::blocking::spawn_blocking(move || catalog_models(&config))
        .await
        .map_err(|_| CatalogError::Models)?;
    let mut body = CatalogBody {
        prx_version: PRX_VERSION,
        revision: None,
        capabilities: CAPABILITY_TOKENS,
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

/// Main's published tool surface without the tools no target can reach,
/// sorted by name.
async fn published_tools(
    state: &AppState,
    runtime: &std::sync::Arc<crate::gateway::TurnRuntimeGeneration>,
) -> Result<Vec<CatalogTool>, CatalogError> {
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cleanup =
        crate::runtime::turn_cleanup::CleanupScope::new(Some(&runtime.publication_slot.cleanup), cancellation)
            .map_err(|_| CatalogError::Draining)?;
    let service = cleanup
        .run(Box::pin(async {
            Ok::<_, anyhow::Error>(std::sync::Arc::clone(
                runtime.publication_slot.publish(runtime, state).await?,
            ))
        }))
        .await
        .map_err(|_| CatalogError::Surface)?;
    let excluded = super::sessions::TARGET_EXCLUDED_TOOLS;
    let mut tools = service
        .publication_scope()
        .catalog_view()
        .iter_entries()
        .filter(|entry| !excluded.contains(&entry.public_name.as_str()) && !excluded.contains(&entry.root.as_str()))
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
