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
    async fn resolve(
        &self,
        generation_id: u64,
        provider_name: &str,
        model_name: &str,
    ) -> anyhow::Result<Arc<dyn Provider>>;

    async fn resolve_default(&self, generation_id: u64) -> anyhow::Result<PinnedDefaultProvider>;
}

/// Resolve and cache providers by the generation that owns their connection.
pub(crate) struct ConfigGenerationProviderResolver {
    config: SharedConfig,
    cache: GenerationProviderCache,
    options: ProviderRuntimeOptions,
}

impl ConfigGenerationProviderResolver {
    pub(crate) fn new(config: SharedConfig, options: ProviderRuntimeOptions) -> Self {
        Self::with_cache(config, options, Arc::new(Mutex::new(HashMap::new())))
    }

    pub(crate) fn with_cache(
        config: SharedConfig,
        options: ProviderRuntimeOptions,
        cache: GenerationProviderCache,
    ) -> Self {
        Self { config, cache, options }
    }
}

#[async_trait]
impl PinnedProviderResolver for ConfigGenerationProviderResolver {
    async fn resolve(
        &self,
        generation_id: u64,
        provider_name: &str,
        model_name: &str,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        let generation = self
            .config
            .pin_generation(ConfigGenerationId(generation_id))
            .with_context(|| format!("configuration generation {generation_id} is no longer pinned"))?;
        let provider_name = if provider_name.trim().is_empty() {
            super::selection::default_provider(&generation.effective)
        } else {
            provider_name
        };
        get_or_create_provider_for_generation(
            &self.config,
            &self.cache,
            &self.options,
            provider_name,
            model_name,
            &generation,
        )
        .await
    }

    async fn resolve_default(&self, generation_id: u64) -> anyhow::Result<PinnedDefaultProvider> {
        let generation = self
            .config
            .pin_generation(ConfigGenerationId(generation_id))
            .with_context(|| format!("configuration generation {generation_id} is no longer pinned"))?;
        let config = &generation.effective;
        let provider_name = super::selection::default_provider(config).to_owned();
        let model_name = super::selection::default_model(config).to_owned();
        let provider = get_or_create_provider_for_generation(
            &self.config,
            &self.cache,
            &self.options,
            &provider_name,
            &model_name,
            &generation,
        )
        .await?;
        let adapter =
            provider.capabilities_for(&model_name, crate::providers::traits::ProviderRequestMode::NonStreaming);
        let capabilities =
            super::selection::catalog_for(config).effective_provider_capabilities(&provider_name, &model_name, adapter);
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
    let catalog = super::selection::catalog_for(config);
    let resolved = super::selection::check_with(
        config,
        &catalog,
        provider_name,
        model_name,
        super::selection::credential_for(config, provider_name),
        &super::catalog::Requirement::chat(),
        super::catalog::UnknownPolicy::Invocation,
    )?;
    let provider = super::create_provider_for(&resolved, &catalog, &config.reliability, &config.cost, options)?;
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
