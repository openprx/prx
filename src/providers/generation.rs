//! Provider construction bound to one immutable configuration generation.

use super::{Provider, ProviderRuntimeOptions};
use crate::config::{ConfigGeneration, ConfigGenerationId, SharedConfig};
use anyhow::Context as _;
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) type GenerationProviderCacheKey = (u64, String, String);
pub(crate) type GenerationProviderCache = Arc<Mutex<HashMap<GenerationProviderCacheKey, Arc<dyn Provider>>>>;
type GenerationCatalogCache = Arc<Mutex<HashMap<u64, Arc<super::catalog::CatalogView>>>>;

/// The default provider request facts owned by one immutable configuration
/// generation. Long-lived tools must use these together rather than combining
/// a reloaded connection with startup model settings.
pub(crate) struct PinnedDefaultProvider {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) model_name: String,
    pub(crate) temperature: f64,
    pub(crate) capabilities: crate::providers::ProviderCapabilities,
}

/// Narrow port used by tools that outlive the configuration generation of the
/// turn currently invoking them.
#[async_trait]
pub(crate) trait PinnedProviderResolver: Send + Sync {
    async fn resolve_default(&self, generation_id: u64) -> anyhow::Result<PinnedDefaultProvider>;
}

/// Resolve and cache providers by the generation that owns their connection.
pub(crate) struct ConfigGenerationProviderResolver {
    config: SharedConfig,
    cache: GenerationProviderCache,
    catalogs: GenerationCatalogCache,
    options: ProviderRuntimeOptions,
}

impl ConfigGenerationProviderResolver {
    pub(crate) fn with_cache(
        config: SharedConfig,
        options: ProviderRuntimeOptions,
        cache: GenerationProviderCache,
    ) -> Self {
        Self {
            config,
            cache,
            catalogs: Arc::new(Mutex::new(HashMap::new())),
            options,
        }
    }

    fn catalog_for_generation(&self, generation: &Arc<ConfigGeneration>) -> Arc<super::catalog::CatalogView> {
        let mut catalogs = self.catalogs.lock();
        catalogs.retain(|id, _| self.config.pin_generation(ConfigGenerationId(*id)).is_some());
        Arc::clone(
            catalogs
                .entry(generation.id.0)
                .or_insert_with(|| Arc::new(super::selection::catalog_for(&generation.effective))),
        )
    }
}

#[async_trait]
impl PinnedProviderResolver for ConfigGenerationProviderResolver {
    async fn resolve_default(&self, generation_id: u64) -> anyhow::Result<PinnedDefaultProvider> {
        let generation = self
            .config
            .pin_generation(ConfigGenerationId(generation_id))
            .with_context(|| format!("configuration generation {generation_id} is no longer pinned"))?;
        let config = &generation.effective;
        let provider_name = super::selection::default_provider(config).to_owned();
        let model_name = super::selection::default_model(config).to_owned();
        let catalog = self.catalog_for_generation(&generation);
        let provider = get_or_create_provider_for_generation_with_catalog(
            &self.config,
            &self.cache,
            &self.options,
            &provider_name,
            &model_name,
            &generation,
            &catalog,
        )
        .await?;
        let adapter =
            provider.capabilities_for(&model_name, crate::providers::traits::ProviderRequestMode::NonStreaming);
        let capabilities = catalog.effective_provider_capabilities(&provider_name, &model_name, adapter);
        Ok(PinnedDefaultProvider {
            provider,
            model_name,
            temperature: config.default_temperature,
            capabilities,
        })
    }
}

pub(crate) async fn get_or_create_provider_for_generation(
    config_owner: &SharedConfig,
    cache: &GenerationProviderCache,
    options: &ProviderRuntimeOptions,
    provider_name: &str,
    model_name: &str,
    generation: &Arc<ConfigGeneration>,
) -> anyhow::Result<Arc<dyn Provider>> {
    let catalog = super::selection::catalog_for(&generation.effective);
    get_or_create_provider_for_generation_with_catalog(
        config_owner,
        cache,
        options,
        provider_name,
        model_name,
        generation,
        &catalog,
    )
    .await
}

async fn get_or_create_provider_for_generation_with_catalog(
    config_owner: &SharedConfig,
    cache: &GenerationProviderCache,
    options: &ProviderRuntimeOptions,
    provider_name: &str,
    model_name: &str,
    generation: &Arc<ConfigGeneration>,
    catalog: &super::catalog::CatalogView,
) -> anyhow::Result<Arc<dyn Provider>> {
    let cache_key = (generation.id.0, provider_name.to_string(), model_name.to_string());
    let active_generation = config_owner.pin().id.0;
    {
        let mut cache = cache.lock();
        cache.retain(|(generation, _, _), _| *generation == active_generation);
        if generation.id.0 == active_generation
            && let Some(existing) = cache.get(&cache_key).cloned()
        {
            return Ok(existing);
        }
    }

    let config = &generation.effective;
    let resolved = super::selection::check_with(
        config,
        catalog,
        provider_name,
        model_name,
        super::selection::credential_for(config, provider_name),
        &super::catalog::Requirement::chat(),
        super::catalog::UnknownPolicy::Invocation,
    )?;
    let provider = super::create_provider_for(&resolved, catalog, &config.reliability, &config.cost, options)?;
    let provider: Arc<dyn Provider> = Arc::from(provider);

    if let Err(error) = provider.warmup().await {
        tracing::warn!(
            provider = %super::redact_endpoint_userinfo(provider_name),
            "Provider warmup failed: {}",
            super::redact_endpoint_userinfo(&error.to_string())
        );
    }

    let active_generation = config_owner.pin().id.0;
    let mut cache = cache.lock();
    cache.retain(|(generation, _, _), _| *generation == active_generation);
    if generation.id.0 == active_generation {
        cache.insert(cache_key, Arc::clone(&provider));
    }
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_resolver_reuses_one_catalog_snapshot() {
        let workspace = tempfile::tempdir().expect("test: workspace");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        crate::providers::catalog::discovery::record_discovered(
            workspace.path(),
            "custom:https://models.example/v1",
            &[crate::providers::catalog::discovery::DiscoveredModel {
                id: "prx-model".to_owned(),
                input_modalities: None,
                output_modalities: None,
                supported_parameters: Some(vec!["temperature".to_owned()]),
                generation_methods: None,
                context_window: Some(32_768),
            }],
            1,
        )
        .expect("test: cache discovered model");
        let shared = crate::config::new_shared(config);
        let generation = shared.pin();
        let resolver = ConfigGenerationProviderResolver::with_cache(
            shared,
            ProviderRuntimeOptions::default(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        let first = resolver.catalog_for_generation(&generation);
        std::fs::remove_file(crate::providers::catalog::discovery::model_cache_path(workspace.path()))
            .expect("test: remove source cache");
        let second = resolver.catalog_for_generation(&generation);
        assert!(Arc::ptr_eq(&first, &second));
        let capabilities = second.effective_provider_capabilities(
            "custom:https://models.example/v1",
            "prx-model",
            crate::providers::ProviderCapabilities {
                native_tool_calling: true,
                vision: true,
            },
        );
        assert!(!capabilities.native_tool_calling);
    }
}
