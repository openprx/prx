use super::{
    Config, MemoryConfig, MemoryEventsConfig, MemorySemanticConfig, Result, Spec, canonical_provider_name,
    check_onboarding_selection, default_memory_backend_key, default_model_for_provider, memory_backend_profile,
    selectable_memory_backends,
};
#[cfg(test)]
use super::{Path, PathBuf, default_user_config_dir, persist_workspace_selection_at};

// ── Quick setup (zero prompts) ───────────────────────────────────

/// Non-interactive setup: generates a sensible default config instantly.
/// Use `prx init` or `prx init --api-key sk-... --provider openrouter --memory sqlite|lucid`.
/// Use `prx init --interactive` for the full wizard.
pub(super) fn backend_key_from_choice(choice: usize) -> &'static str {
    selectable_memory_backends()
        .get(choice)
        .map_or(default_memory_backend_key(), |backend| backend.key)
}

pub(super) fn memory_config_defaults_for_backend(backend: &str) -> MemoryConfig {
    let profile = memory_backend_profile(backend);
    let embeddings_enabled = !matches!(backend, "markdown" | "none");

    MemoryConfig {
        backend: backend.to_string(),
        auto_save: profile.auto_save_default,
        events: MemoryEventsConfig::default(),
        semantic: MemorySemanticConfig::default(),
        acl_enabled: false,
        legacy_conversation_turns_visible: true,
        archive_after_days: 7,
        purge_after_days: 30,
        conversation_retention_days: 3,
        daily_retention_days: 7,
        embedding_provider: if embeddings_enabled { "local" } else { "none" }.to_string(),
        // Onboarding writes the provider into the generated configuration, so
        // this installation never depends on the serde default.
        embedding_provider_explicit: true,
        embedding_model: if embeddings_enabled {
            "prx-local-hash-v1"
        } else {
            "none"
        }
        .to_string(),
        embedding_dimensions: if embeddings_enabled { 384 } else { 0 },
        vector_weight: 0.7,
        keyword_weight: 0.3,
        min_relevance_score: 0.4,
        embedding_cache_size: if profile.uses_sqlite_hygiene { 10000 } else { 0 },
        vector_candidate_cap: MemoryConfig::default().vector_candidate_cap,
        auto_hydrate: true,
        sqlite_open_timeout_secs: None,
        sqlite_read_pool_size: None,
        lucid_cmd: None,
        lucid_budget: None,
        lucid_recall_timeout_ms: 5000,
        lucid_store_timeout_ms: 3000,
        lucid_local_hit_threshold: 3,
        lucid_failure_cooldown_ms: 60_000,
    }
}

/// Apply explicit quick-setup selections to the same staged init generation.
#[allow(clippy::too_many_arguments)]
pub async fn run_quick_setup(
    credential_override: Option<&str>,
    provider: Option<&str>,
    model_override: Option<&str>,
    native_tools: Option<bool>,
    memory_backend: Option<&str>,
    config_dir: Option<&str>,
    spec: Spec,
    force: bool,
) -> Result<Config> {
    let target = crate::config::resolve_init_target_dir(None, config_dir).await?;
    let customize = |config: &mut Config| -> Result<()> {
        if let Some(provider) = provider {
            if config.default_provider.as_deref() != Some(provider) {
                config.api_key = None;
                config.api_url = None;
                config.default_model = Some(default_model_for_provider(provider));
            }
            config.default_provider = Some(provider.to_owned());
        }
        if let Some(model) = model_override {
            config.default_model = Some(model.trim().to_owned());
        }
        if let Some(key) = credential_override {
            config.api_key = Some(key.to_owned());
        }
        if let Some(backend) = memory_backend {
            anyhow::ensure!(
                matches!(backend, "sqlite" | "lucid" | "markdown" | "none"),
                "Unknown memory backend: {backend}"
            );
            config.memory = memory_config_defaults_for_backend(backend);
        }
        let selected_provider = crate::providers::selection::default_provider(config).to_owned();
        let selected_model = crate::providers::selection::default_model(config).to_owned();
        let catalog = crate::providers::catalog::CatalogView::release_only();
        let released = catalog
            .release_catalog()?
            .model(&crate::providers::ProviderId::of(&selected_provider), &selected_model)
            .is_some();
        if !released {
            // An explicit selection outside the release catalog always needs
            // the operator's choice, even when repairing an existing tree.
            let tools = native_tools
                .ok_or_else(|| crate::providers::catalog::admission::ModelCapabilitiesUnknown::new(&selected_model))?;
            let declaration = crate::config::schema::ProviderModelConfig {
                provider: selected_provider.clone(),
                model: Some(selected_model.clone()),
                operations: Some(vec![crate::config::schema::ModelOperationSetting::Chat]),
                native_tools: Some(tools),
                vision_input: None,
                streaming: None,
                structured_output: None,
                tool_schema_dialect: None,
                tool_call_dialect: None,
                context_window: None,
            };
            let id = crate::providers::ProviderId::of(&selected_provider);
            if let Some(existing) = config.providers.models.iter_mut().find(|entry| {
                crate::providers::ProviderId::of(&entry.provider) == id
                    && entry
                        .model
                        .as_deref()
                        .is_some_and(|name| name.trim().eq_ignore_ascii_case(&selected_model))
            }) {
                existing.operations = declaration.operations;
                existing.native_tools = declaration.native_tools;
            } else {
                config.providers.models.push(declaration);
            }
        }
        check_onboarding_selection(
            config,
            crate::providers::selection::default_provider(config),
            crate::providers::selection::default_model(config),
        )
    };
    let report = spec.generate_customized(&target, force, Some(&customize), None).await?;
    report.print();
    report.into_result()?;
    if provider.is_some_and(|name| canonical_provider_name(name) == "xai")
        && credential_override.is_none_or(|key| key.trim().is_empty())
    {
        println!("Sign in to Grok (SuperGrok / X Premium) with:");
        println!("  prx auth login --provider xai");
        println!("  prx auth login --provider xai --device-code");
        println!("An xAI API key still works via --from-stdin or `api_key` in config.toml.");
    }
    Config::load_from_path(&target.join("config.toml"), target.join("workspace"))
}

#[cfg(test)]
pub(super) async fn run_quick_setup_with_home(
    credential: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    memory: Option<&str>,
    dir: Option<&str>,
    home: &Path,
) -> Result<Config> {
    let target = dir.map(PathBuf::from).unwrap_or_else(|| default_user_config_dir(home));
    let config = run_quick_setup(
        credential,
        provider,
        model,
        None,
        memory,
        target.to_str(),
        Spec::Minimal,
        false,
    )
    .await?;
    persist_workspace_selection_at(&config.config_path, &default_user_config_dir(home)).await?;
    Ok(config)
}
