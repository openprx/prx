use super::tools::WorkerChannel;
use crate::config::Config;
use crate::security::SecurityPolicy;
use crate::session_worker::protocol::WorkerManifest;
use std::sync::Arc;

/// Every credential a worker hands out, each paired with the provider it was
/// issued for.
///
/// A worker runs on `manifest.provider_name`, which the parent may have taken
/// from a model's per-call `provider` argument, and it carries at most the
/// credential the parent released for that provider (`manifest.api_key`). The
/// root key, base URL and rotation keys belong to the configured default
/// provider. Each consumer below receives only the credential for the provider
/// it will present it to.
pub(super) struct WorkerCredentials<'a> {
    /// Sealed credential material, retained only for manifest/config comparison.
    pub(super) sealed_key: Option<&'a str>,
    /// Fallback for the worker's `sessions_spawn`, which pairs it with
    /// `manifest.provider_name`.
    pub(super) spawn_fallback: Option<&'a str>,
    /// Root key for the base tool factory, whose `delegate` scopes it to the
    /// configured default provider.
    pub(super) root_api_key: Option<&'a str>,
}

pub(super) fn worker_credentials<'a>(manifest: &'a WorkerManifest, config: &'a Config) -> WorkerCredentials<'a> {
    let configured_provider = crate::providers::selection::default_provider(config);
    let uses_root = if manifest.route_hint.is_some() {
        manifest.route_uses_root_connection
    } else {
        manifest.direct_uses_root_connection
    };
    let provider = if uses_root {
        crate::providers::configured_connection_for(
            &manifest.provider_name,
            configured_provider,
            config.api_key.as_deref(),
            config.api_url.as_deref(),
        )
    } else {
        crate::providers::ProviderConnection {
            api_key: manifest.api_key.as_deref().map(str::trim).filter(|key| !key.is_empty()),
            api_url: None,
            released: false,
        }
    };
    WorkerCredentials {
        sealed_key: manifest.api_key.as_deref(),
        spawn_fallback: if manifest.route_hint.is_some() {
            config.api_key.as_deref()
        } else {
            provider.api_key
        },
        root_api_key: config.api_key.as_deref(),
    }
}

/// The worker's own `sessions_spawn`, which pairs `credentials.spawn_fallback`
/// with `manifest.provider_name` and accepts a per-call provider only among
/// the providers the operator configured (the same set every other surface
/// uses).
pub(super) fn worker_spawn_tool(
    manifest: &WorkerManifest,
    config: &Config,
    credentials: &WorkerCredentials<'_>,
    security: Arc<SecurityPolicy>,
    options: &crate::providers::ProviderRuntimeOptions,
    active_runs: Arc<tokio::sync::RwLock<Vec<crate::tools::sessions_spawn::SubAgentRun>>>,
) -> crate::tools::SessionsSpawnTool {
    let source_provider = manifest
        .route_source_provider
        .as_deref()
        .unwrap_or(manifest.provider_name.as_str());
    let spawn_provider_name = if manifest.route_hint.is_some() {
        source_provider
    } else {
        manifest.provider_name.as_str()
    };
    let spawn_model = manifest
        .route_hint
        .as_ref()
        .map_or_else(|| manifest.model.clone(), |hint| format!("hint:{hint}"));
    let tool = crate::tools::SessionsSpawnTool::new_with_registry(
        Arc::new(WorkerChannel),
        spawn_provider_name,
        spawn_model,
        manifest.temperature,
        security,
        manifest.workspace_dir.clone(),
        config.multimodal.clone(),
        config.agents_with_references().into_owned(),
        credentials.spawn_fallback.map(str::to_string),
        options.clone(),
        config.sessions_spawn.clone(),
        active_runs,
    )
    .with_cost_config(config.cost.clone())
    .with_configured_providers(&crate::providers::ConfiguredProviders::from_config(config))
    .with_model_catalog(crate::providers::selection::catalog_for(config))
    .with_hint_config(config)
    .with_fallback_api_key_owner(if manifest.route_hint.is_some() {
        crate::providers::selection::default_provider(config)
    } else {
        manifest.provider_name.as_str()
    })
    .with_fallback_root_connection(manifest.route_hint.is_some() || manifest.direct_uses_root_connection);
    if let (Some(source), Some(credential)) = (&manifest.route_source_provider, &manifest.route_credential) {
        tool.with_default_route_source(source, credential.clone())
    } else {
        tool
    }
}
