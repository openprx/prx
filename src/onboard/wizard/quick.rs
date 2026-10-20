use super::{
    AutonomyConfig, BANNER, BrowserConfig, ChannelsConfig, ComposioConfig, Config, Context, HeartbeatConfig,
    MemoryConfig, MemoryEventsConfig, MemorySemanticConfig, ObservabilityConfig, Path, PathBuf, ProjectContext, Result,
    RuntimeConfig, SecretsConfig, StorageConfig, check_onboarding_selection, credential_setup_hint,
    default_memory_backend_key, default_model_for_provider, default_user_config_dir, fs, memory_backend_profile,
    persist_onboarding_config, persist_workspace_selection_at, scaffold_workspace, selectable_memory_backends,
    setup_provider_line, style,
};

// ── Quick setup (zero prompts) ───────────────────────────────────

/// Non-interactive setup: generates a sensible default config instantly.
/// Use `prx onboard` or `prx onboard --api-key sk-... --provider openrouter --memory sqlite|lucid`.
/// Use `prx onboard --interactive` for the full wizard.
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

#[allow(clippy::too_many_lines)]
pub async fn run_quick_setup(
    credential_override: Option<&str>,
    provider: Option<&str>,
    model_override: Option<&str>,
    memory_backend: Option<&str>,
    config_dir: Option<&str>,
) -> Result<Config> {
    let home = crate::config::user_home_dir().context("Could not find home directory")?;

    run_quick_setup_with_home(
        credential_override,
        provider,
        model_override,
        memory_backend,
        config_dir,
        &home,
    )
    .await
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_quick_setup_with_home(
    credential_override: Option<&str>,
    provider: Option<&str>,
    model_override: Option<&str>,
    memory_backend: Option<&str>,
    config_dir: Option<&str>,
    home: &Path,
) -> Result<Config> {
    println!("{}", style(BANNER).cyan().bold());
    println!(
        "  {}",
        style("Quick Setup — generating config with sensible defaults...")
            .white()
            .bold()
    );
    println!();

    let openprx_dir = config_dir.map_or_else(|| default_user_config_dir(home), PathBuf::from);
    let workspace_dir = openprx_dir.join("workspace");
    let config_path = openprx_dir.join("config.toml");

    fs::create_dir_all(&workspace_dir).context("Failed to create workspace directory")?;

    let provider_name = provider.unwrap_or("openrouter").to_string();
    // Stored as the resolver reads it (R-PMMCh C-2).
    let model = model_override
        .map(|model| model.trim().to_string())
        .unwrap_or_else(|| default_model_for_provider(&provider_name));
    let memory_backend_name = memory_backend.unwrap_or(default_memory_backend_key()).to_string();

    // Create memory config based on backend choice
    let memory_config = memory_config_defaults_for_backend(&memory_backend_name);

    let config = Config {
        workspace_dir: workspace_dir.clone(),
        config_path: config_path.clone(),
        api_key: credential_override.map(|c| {
            let mut s = String::with_capacity(c.len());
            s.push_str(c);
            s
        }),
        api_url: None,
        default_provider: Some(provider_name.clone()),
        default_model: Some(model.clone()),
        default_temperature: 0.7,
        model_settings: Default::default(),
        providers: crate::config::ProvidersConfig::default(),
        observability: ObservabilityConfig::default(),
        autonomy: AutonomyConfig::default(),
        runtime: RuntimeConfig::default(),
        reliability: crate::config::ReliabilityConfig::default(),
        scheduler: crate::config::schema::SchedulerConfig::default(),
        agent: crate::config::schema::AgentConfig::default(),
        sessions_spawn: crate::config::SessionsSpawnConfig::default(),
        chat: crate::config::ChatConfig::default(),
        self_system: crate::config::SelfSystemConfig::default(),
        skills: crate::config::SkillsConfig::default(),
        skill_rag: crate::config::schema::SkillRagConfig::default(),
        plugins: crate::config::plugins::PluginsConfig::default(),
        skills_lifecycle: crate::config::skills_lifecycle::SkillsLifecycleConfig::default(),
        model_routes: Vec::new(),
        embedding_routes: Vec::new(),
        query_classification: crate::config::QueryClassificationConfig::default(),
        task_routing: crate::config::TaskRoutingConfig::default(),
        router: crate::config::RouterConfig::default(),
        smart_group: crate::config::SmartGroupConfig::default(),
        heartbeat: HeartbeatConfig::default(),
        cron: crate::config::CronConfig::default(),
        xin: crate::xin::XinConfig::default(),
        channels_config: ChannelsConfig::default(),
        memory: memory_config,
        identity_bindings: Vec::new(),
        user_policies: Vec::new(),
        storage: StorageConfig::default(),
        tunnel: crate::config::TunnelConfig::default(),
        gateway: crate::config::GatewayConfig::default(),
        webhook: crate::config::MemoryWebhookConfig::default(),
        composio: ComposioConfig::default(),
        mcp: crate::config::McpConfig::default(),
        mcp_server: crate::config::McpServerRuntimeConfig::default(),
        a2a: crate::config::A2aConfig::default(),
        auth: crate::config::AuthConfig::default(),
        secrets: SecretsConfig::default(),
        browser: BrowserConfig::default(),
        http_request: crate::config::HttpRequestConfig::default(),
        multimodal: crate::config::MultimodalConfig::default(),
        web_search: crate::config::WebSearchConfig::default(),
        proxy: crate::config::ProxyConfig::default(),
        identity: crate::config::IdentityConfig::default(),
        cost: crate::config::CostConfig::default(),
        nodes: crate::config::NodesConfig::default(),
        agents: std::collections::HashMap::new(),
        media: crate::config::MediaConfig::default(),
        causal_tree: crate::causal_tree::CausalTreeConfig::default(),
        security: crate::config::SecurityConfig::default(),
        compliance: crate::config::schema::ComplianceConfig::default(),
        tool_tiering: crate::config::ToolTieringConfig::default(),
    };

    // A new durable default (D-018 Q1): the model must be one PRX has
    // evidence for, or nothing is written.
    check_onboarding_selection(&config, &provider_name, &model)?;

    let split = config
        .config_path
        .parent()
        .is_some_and(|parent| parent.join("config.d").is_dir());
    persist_onboarding_config(&config, split).await?;
    persist_workspace_selection_at(&config.config_path, &default_user_config_dir(home)).await?;

    // Scaffold minimal workspace files
    let default_ctx = ProjectContext {
        user_name: std::env::var("USER").unwrap_or_else(|_| "User".into()),
        timezone: "UTC".into(),
        agent_name: "OpenPRX".into(),
        communication_style:
            "Be warm, natural, and clear. Use occasional relevant emojis (1-2 max) and avoid robotic phrasing.".into(),
    };
    scaffold_workspace(&workspace_dir, &default_ctx)?;

    println!(
        "  {} Workspace:  {}",
        style("✓").green().bold(),
        style(workspace_dir.display()).green()
    );
    println!("{}", setup_provider_line(&provider_name));
    println!("  {} Model:      {}", style("✓").green().bold(), style(&model).green());
    println!(
        "  {} API Key:    {}",
        style("✓").green().bold(),
        if credential_override.is_some() {
            style("set").green()
        } else {
            style("not set (use --api-key or edit config.toml)").yellow()
        }
    );
    println!(
        "  {} Security:   {}",
        style("✓").green().bold(),
        style("Supervised (workspace-scoped)").green()
    );
    println!(
        "  {} Memory:     {} (auto-save: {})",
        style("✓").green().bold(),
        style(&memory_backend_name).green(),
        if memory_backend_name == "none" { "off" } else { "on" }
    );
    println!(
        "  {} Secrets:    {}",
        style("✓").green().bold(),
        style("encrypted").green()
    );
    println!(
        "  {} Gateway:    {}",
        style("✓").green().bold(),
        style("pairing required (127.0.0.1:8080)").green()
    );
    println!(
        "  {} Tunnel:     {}",
        style("✓").green().bold(),
        style("none (local only)").dim()
    );
    println!(
        "  {} Composio:   {}",
        style("✓").green().bold(),
        style("disabled (sovereign mode)").dim()
    );
    println!();
    println!(
        "  {} {}",
        style("Config saved:").white().bold(),
        style(config_path.display()).green()
    );
    println!();
    println!("  {}", style("Next steps:").white().bold());
    if credential_override.is_none() {
        println!("    1. Add your API key:  {}", credential_setup_hint(&provider_name));
        println!("    2. Chat:              prx agent -m \"Hello!\"");
        println!("    3. Gateway:           prx gateway");
    } else {
        println!("    1. Chat:     prx agent -m \"Hello!\"");
        println!("    2. Gateway:  prx gateway");
        println!("    3. Status:   prx status");
    }
    println!();

    Ok(config)
}
