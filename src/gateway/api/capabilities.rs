//! The rf contract's feature gate (AC-2): `GET /api/capabilities` and
//! `GET /api/agents/catalog`.
//!
//! Both are reads. They change no configuration, write no store and are not
//! gated by autonomy. Neither body carries a credential, a key id or a URL
//! with userinfo: provider labels are the redacted ones every model listing
//! shows, and readiness is a boolean. Field names are a contract with rf; the
//! tokens' meanings live in `docs/rf-contract/capability-tokens.md`.

use super::AppState;
use crate::config::Config;
use crate::config::agent_name::RESERVED_AGENT_NAMES;
use crate::config::manage::service::{ModelFilter, model_catalog};
use crate::providers::catalog::ModelOperation;
use crate::security::AutonomyLevel;
use crate::tools::capability::contract::SideEffectClass;
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The advertised tokens, sorted ascending. A token is listed only when its
/// whole earliest usable form is on main.
const CAPABILITY_TOKENS: [&str; 2] = ["agent_catalog_v1", "capabilities_v1"];

/// Session-key prefixes PRX mints itself (`console-<uuid>`).
const RESERVED_SESSION_KEY_PREFIXES: [&str; 1] = ["console-"];

const PRX_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize)]
struct CapabilitiesBody {
    prx_version: &'static str,
    capabilities: [&'static str; 2],
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
        let status = match self {
            Self::Draining | Self::Surface | Self::Models => StatusCode::SERVICE_UNAVAILABLE,
            Self::Encode => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(serde_json::json!({ "error": self.to_string() }))).into_response()
    }
}

/// `GET /api/capabilities`: the PRX version and the advertised tokens.
pub(super) async fn get_capabilities() -> Response {
    Json(CapabilitiesBody {
        prx_version: PRX_VERSION,
        capabilities: CAPABILITY_TOKENS,
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
/// model, each with its provider's credential readiness (resolved once per
/// provider).
fn catalog_models(config: &Config) -> Vec<CatalogModel> {
    let filter = ModelFilter {
        provider: None,
        operation: Some(ModelOperation::Chat),
        executable: false,
    };
    let mut readiness: BTreeMap<String, Option<bool>> = BTreeMap::new();
    let mut lookup_failed = false;
    let mut models = Vec::new();
    for entry in model_catalog(config, &filter) {
        let ready = *readiness.entry(entry.provider.clone()).or_insert_with(|| {
            let ready = crate::providers::credential_readiness::credential_ready(config, &entry.provider).ok();
            lookup_failed |= ready.is_none();
            ready
        });
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
