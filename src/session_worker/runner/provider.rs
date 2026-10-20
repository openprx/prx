use super::tools::WorkerChannel;
use crate::config::Config;
use crate::providers::Provider;
use crate::security::SecurityPolicy;
use crate::session_worker::protocol::WorkerManifest;
use anyhow::{Context, Result};
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
    /// The worker's own provider connection.
    pub(super) provider: crate::providers::ProviderConnection<'a>,
    /// Reliability for the worker's own chain; rotation keys follow the root.
    pub(super) reliability: std::borrow::Cow<'a, crate::config::ReliabilityConfig>,
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
        reliability: crate::providers::scoped_reliability(&config.reliability, &provider),
        spawn_fallback: if manifest.route_hint.is_some() {
            config.api_key.as_deref()
        } else {
            provider.api_key
        },
        provider,
        root_api_key: config.api_key.as_deref(),
    }
}

/// A process child accepts only the exact route its parent sealed. In
/// particular, a route key that happens to equal the root key is still not
/// permission to attach the root URL or rotation keys.
pub(super) fn validate_worker_route(manifest: &WorkerManifest, config: &Config) -> Result<()> {
    let credentials = worker_credentials(manifest, config);
    let Some(hint) = manifest.route_hint.as_deref() else {
        if manifest.route_uses_root_connection || manifest.model.trim().starts_with("hint:") {
            anyhow::bail!("session-worker manifest has an unresolved or inconsistent model route");
        }
        if manifest.direct_uses_root_connection
            && (crate::providers::ProviderId::of(&manifest.provider_name)
                != crate::providers::ProviderId::of(crate::providers::selection::default_provider(config))
                || credentials.sealed_key != credentials.root_api_key)
        {
            anyhow::bail!("session-worker direct root connection does not match sealed config");
        }
        if let Some(agent) = manifest.agent_id.as_deref().and_then(|name| config.agents.get(name))
            && crate::providers::ProviderId::of(&agent.provider)
                == crate::providers::ProviderId::of(&manifest.provider_name)
        {
            if let Some(id) = agent.credential_ref.as_deref() {
                if manifest.direct_uses_root_connection {
                    anyhow::bail!("session-worker agent credential cannot use the root connection");
                }
                let selected = crate::providers::selection::check_with(
                    config,
                    &crate::providers::selection::catalog_for(config),
                    &manifest.provider_name,
                    &manifest.model,
                    crate::providers::credentials::CredentialRef::Profile { id: id.to_string() },
                    &crate::providers::catalog::Requirement::chat(),
                    crate::providers::catalog::UnknownPolicy::Invocation,
                )?;
                if selected.credential().api_key() != credentials.sealed_key {
                    anyhow::bail!("session-worker agent credential does not match sealed config");
                }
            } else if let Some(key) = agent.api_key.as_deref().filter(|key| !key.trim().is_empty()) {
                if manifest.direct_uses_root_connection || credentials.sealed_key != Some(key) {
                    anyhow::bail!("session-worker agent credential does not match sealed config");
                }
            }
        }
        return Ok(());
    };
    if manifest.direct_uses_root_connection {
        anyhow::bail!("session-worker routed connection has direct root provenance");
    }
    let catalog = crate::providers::selection::catalog_for(config);
    let selection = format!("hint:{hint}");
    let route = crate::providers::selection::route_target(&catalog, &manifest.provider_name, &selection)?
        .context("session-worker manifest route is missing")?;
    if route.provider != manifest.provider_name {
        anyhow::bail!("session-worker manifest route destination does not match sealed config");
    }
    if route.model != manifest.model {
        anyhow::bail!("session-worker manifest route destination does not match sealed config");
    }
    if manifest.route_uses_root_connection
        && (route.has_own_key
            || route.credential_ref.is_some()
            || crate::providers::ProviderId::of(&route.provider)
                != crate::providers::ProviderId::of(crate::providers::selection::default_provider(config)))
    {
        anyhow::bail!("session-worker manifest route cannot use the root connection");
    }
    if manifest.route_uses_root_connection && credentials.sealed_key != credentials.root_api_key {
        anyhow::bail!("session-worker manifest root connection does not match sealed config");
    }
    let source_provider = manifest
        .route_source_provider
        .as_deref()
        .context("session-worker manifest route source is missing")?;
    let credential = manifest
        .route_credential
        .as_ref()
        .context("session-worker manifest route credential provenance is missing")?;
    let resolved = crate::providers::selection::check_with(
        config,
        &catalog,
        source_provider,
        &selection,
        credential.clone(),
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    if resolved.credential().api_key() != credentials.sealed_key {
        anyhow::bail!("session-worker manifest route credential does not match sealed config");
    }
    if resolved.credential().reference() != credential {
        anyhow::bail!("session-worker manifest route credential provenance does not match sealed config");
    }
    let resolved_uses_root = matches!(
        resolved.credential().reference(),
        crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
    );
    if manifest.route_uses_root_connection != resolved_uses_root {
        anyhow::bail!("session-worker manifest route connection provenance does not match sealed config");
    }
    Ok(())
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

/// Build the worker's own provider chain from its scoped credentials, after
/// resolving the manifest's provider and model for chat: a model that cannot
/// chat builds nothing (an id-only model runs, with a warning).
pub(super) fn worker_provider(
    manifest: &WorkerManifest,
    config: &Config,
    credentials: &WorkerCredentials<'_>,
    options: &crate::providers::ProviderRuntimeOptions,
) -> Result<Box<dyn Provider>> {
    if manifest.model.trim().starts_with("hint:") {
        anyhow::bail!("session-worker received an unresolved model route");
    }
    let credential = if manifest.route_hint.is_some() {
        manifest
            .route_credential
            .clone()
            .context("session-worker manifest route credential provenance is missing")?
    } else if let Some((name, agent)) = manifest
        .agent_id
        .as_deref()
        .and_then(|name| config.agents.get(name).map(|agent| (name, agent)))
        .filter(|(_, agent)| {
            crate::providers::ProviderId::of(&agent.provider)
                == crate::providers::ProviderId::of(&manifest.provider_name)
        })
    {
        match agent
            .credential_ref
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(id) => crate::providers::credentials::CredentialRef::Profile { id: id.to_string() },
            None if agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) => {
                crate::providers::credentials::CredentialRef::LegacyInline(
                    crate::providers::credentials::LegacySlot::Agent(name.to_string()),
                )
            }
            None => crate::providers::selection::credential_for(config, &manifest.provider_name),
        }
    } else {
        crate::providers::selection::credential_for(config, &manifest.provider_name)
    };
    build_worker_provider_selection(
        config,
        &manifest.provider_name,
        &manifest.model,
        credential,
        &credentials.provider,
        &credentials.reliability,
        options,
    )
}

/// Resolve and construct the exact provider/model connection a worker was
/// sealed to. This is the worker's sole provider-construction seam.
pub(crate) fn build_worker_provider_selection(
    config: &Config,
    provider_name: &str,
    model: &str,
    credential: crate::providers::credentials::CredentialRef,
    sealed_connection: &crate::providers::ProviderConnection<'_>,
    reliability: &crate::config::ReliabilityConfig,
    options: &crate::providers::ProviderRuntimeOptions,
) -> Result<Box<dyn Provider>> {
    let catalog = crate::providers::selection::catalog_for(config);
    let resolved = crate::providers::selection::check_with(
        config,
        &catalog,
        provider_name,
        model,
        credential,
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    anyhow::ensure!(
        resolved.credential().api_key() == sealed_connection.api_key
            && resolved.credential().api_url() == sealed_connection.api_url,
        "session-worker resolved credential differs from its sealed connection"
    );
    crate::providers::create_provider_for(&resolved, &catalog, reliability, &config.cost, options)
}
