#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::auth::{profiles::AuthProfilesStore, select_profile_id};
#[cfg(feature = "channel-matrix")]
use crate::config::MatrixConfig;
use crate::config::init::Spec;
use crate::config::schema::{
    DingTalkConfig, IrcConfig, LarkReceiveMode, LinqConfig, QQConfig, StreamMode, WhatsAppConfig,
};
use crate::config::{
    AutonomyConfig, BrowserConfig, ChannelsConfig, ComposioConfig, Config, DiscordConfig, HeartbeatConfig,
    IMessageConfig, LarkConfig, MemoryConfig, MemoryEventsConfig, MemorySemanticConfig, ObservabilityConfig,
    RuntimeConfig, SecretsConfig, SlackConfig, StorageConfig, TelegramConfig, WebhookConfig,
};
use crate::memory::{default_memory_backend_key, memory_backend_profile, selectable_memory_backends};
use crate::onboard::auto_detect::is_claude_code_oauth_setup_token;
use crate::providers::catalog::discovery::{self, DiscoveredModel};
use crate::providers::{
    canonical_china_provider_name, is_glm_alias, is_glm_cn_alias, is_huggingface_alias, is_litellm_alias,
    is_minimax_alias, is_moonshot_alias, is_qianfan_alias, is_qwen_alias, is_qwen_oauth_alias, is_vllm_alias,
    is_zai_alias, is_zai_cn_alias,
};
use anyhow::{Context, Result, bail};
use console::style;
use dialoguer::{Confirm, Input, Password, Select};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ── Project context collected during wizard ──────────────────────

/// User-provided personalization baked into workspace MD files.
#[derive(Debug, Clone, Default)]
pub struct ProjectContext {
    pub user_name: String,
    pub timezone: String,
    pub agent_name: String,
    pub communication_style: String,
}

// ── Banner ───────────────────────────────────────────────────────

const BANNER: &str = r"
    ⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡

     ██████╗ ██████╗ ███████╗███╗   ██╗██████╗ ██████╗ ██╗  ██╗
    ██╔═══██╗██╔══██╗██╔════╝████╗  ██║██╔══██╗██╔══██╗╚██╗██╔╝
    ██║   ██║██████╔╝█████╗  ██╔██╗ ██║██████╔╝██████╔╝ ╚███╔╝
    ██║   ██║██╔═══╝ ██╔══╝  ██║╚██╗██║██╔═══╝ ██╔══██╗ ██╔██╗
    ╚██████╔╝██║     ███████╗██║ ╚████║██║     ██║  ██║██╔╝ ██╗
     ╚═════╝ ╚═╝     ╚══════╝╚═╝  ╚═══╝╚═╝     ╚═╝  ╚═╝╚═╝  ╚═╝

    100% Rust. 100% Agnostic. Your AI, your rules.

    ⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡⚡
";

const fn has_launchable_channels(channels: &ChannelsConfig) -> bool {
    let ChannelsConfig {
        cli: _,     // `cli` is always available and does not require channel server startup
        webhook: _, // webhook traffic is handled by gateway, not `prx channel start`
        telegram,
        discord,
        slack,
        mattermost,
        imessage,
        matrix: _,
        signal,
        whatsapp,
        email,
        irc,
        lark,
        dingtalk,
        linq,
        qq,
        ..
    } = channels;

    telegram.is_some()
        || discord.is_some()
        || slack.is_some()
        || mattermost.is_some()
        || imessage.is_some()
        || (cfg!(feature = "channel-matrix") && channels.matrix.is_some())
        || signal.is_some()
        || whatsapp.is_some()
        || email.is_some()
        || irc.is_some()
        || lark.is_some()
        || dingtalk.is_some()
        || linq.is_some()
        || qq.is_some()
}

// ── Main wizard entry point ──────────────────────────────────────

pub async fn run_wizard(config_dir: Option<&str>) -> Result<(Config, bool)> {
    println!("{}", style(BANNER).cyan().bold());

    println!(
        "  {}",
        style("Welcome to OpenPRX — the fastest, smallest AI assistant.")
            .white()
            .bold()
    );
    println!(
        "  {}",
        style("This wizard will configure your agent in under 60 seconds.").dim()
    );
    println!();

    print_step(1, 9, "Workspace Setup");
    let (workspace_dir, config_path) = setup_workspace(config_dir)?;

    print_step(2, 9, "AI Provider & API Key");
    let (provider, api_key, model, provider_api_url) = setup_provider(&workspace_dir).await?;
    let model_settings = choose_model_level(&provider, &model)?;

    print_step(3, 9, "Channels (How You Talk to OpenPRX)");
    let channels_config = setup_channels()?;

    print_step(4, 9, "Tunnel (Expose to Internet)");
    let tunnel_config = setup_tunnel()?;

    print_step(5, 9, "Tool Mode & Security");
    let (composio_config, secrets_config) = setup_tool_mode()?;

    print_step(6, 9, "Memory Configuration");
    let memory_config = setup_memory()?;

    print_step(7, 9, "Project Context (Personalize Your Agent)");
    let project_ctx = setup_project_context()?;

    print_step(8, 9, "Decision Plane");
    let decision = setup_decision_plane()?;

    print_step(9, 9, "Workspace Files");
    scaffold_workspace(&workspace_dir, &project_ctx)?;

    // ── Build config ──
    // Defaults: SQLite memory, supervised autonomy, workspace-scoped, native runtime
    let config = Config {
        workspace_dir: workspace_dir.clone(),
        config_path: config_path.clone(),
        api_key: if api_key.is_empty() { None } else { Some(api_key) },
        api_url: provider_api_url,
        default_provider: Some(provider),
        // Stored as the resolver reads it (R-PMMCh C-2).
        default_model: Some(model.trim().to_string()),
        default_temperature: 0.7,
        model_settings,
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
        channels_config,
        memory: memory_config, // User-selected memory backend
        identity_bindings: Vec::new(),
        user_policies: Vec::new(),
        storage: StorageConfig::default(),
        tunnel: tunnel_config,
        gateway: crate::config::GatewayConfig::default(),
        webhook: crate::config::MemoryWebhookConfig::default(),
        composio: composio_config,
        mcp: crate::config::McpConfig::default(),
        mcp_server: crate::config::McpServerRuntimeConfig::default(),
        a2a: crate::config::A2aConfig::default(),
        auth: crate::config::AuthConfig::default(),
        secrets: secrets_config,
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

    // ── Detect Spec from wizard choices ──────────────────────────
    // Full  → user configured messaging channels (Telegram, Discord, Slack, …)
    // Server → user configured a tunnel (non-local gateway exposure)
    // Minimal → local CLI only
    let spec = if has_launchable_channels(&config.channels_config) {
        Spec::Full
    } else if config.tunnel.provider != "none" {
        Spec::Server
    } else {
        Spec::Minimal
    };

    println!(
        "  {} Security: {} | workspace-scoped",
        style("✓").green().bold(),
        style("Supervised").green()
    );
    println!(
        "  {} Memory: {} (auto-save: {})",
        style("✓").green().bold(),
        style(&config.memory.backend).green(),
        if config.memory.auto_save { "on" } else { "off" }
    );
    println!(
        "  {} Config layout: {} ({})",
        style("✓").green().bold(),
        style("config.d/ split").green(),
        style(spec.name()).cyan()
    );

    if let Some((decision, Some(_))) = &decision {
        decision.ensure_hosted_credential_free(&config.config_path)?;
    }

    // The chosen default is an operator selection (D-034): a model the
    // evidence rules out is refused before anything is written; an
    // unverified one is accepted with a warning.
    check_onboarding_selection(
        &config,
        crate::providers::selection::default_provider(&config),
        crate::providers::selection::default_model(&config),
    )?;

    // Write config.toml (main) + config.d/ fragment files.
    // This replaces the flat config.save() call so that the user's
    // workspace is set up with the split layout from the start.
    persist_onboarding_config(&config, true).await?;
    persist_workspace_selection(&config.config_path).await?;

    // ── Decision plane: the shared setup service, then a local test ──
    if let Some((decision, secret)) = decision {
        match secret {
            Some(secret) => print!("{}", decision.apply_hosted(config.clone(), secret).await?),
            None => {
                decision.apply(config.clone()).await?;
            }
        }
        let written = Config::load_from_path(&config.config_path, config.workspace_dir.clone())?;
        let outcome = crate::causal_cli::test_configured_locally(&written).await?;
        println!(
            "  {} Decision backend: {} (local test: {outcome})",
            style("✓").green().bold(),
            decision.kind.as_str()
        );
    }

    // ── Final summary ────────────────────────────────────────────
    print_summary(&config);

    // ── Offer to launch channels immediately ─────────────────────
    let has_channels = has_launchable_channels(&config.channels_config);
    let mut autostart = false;

    if has_channels && config.api_key.is_some() {
        let launch: bool = Confirm::new()
            .with_prompt(format!(
                "  {} Launch channels now? (connected channels → AI → reply)",
                style("🚀").cyan()
            ))
            .default(true)
            .interact()?;

        if launch {
            println!();
            println!(
                "  {} {}",
                style("⚡").cyan(),
                style("Starting channel server...").white().bold()
            );
            println!();
            autostart = true;
        }
    }

    Ok((config, autostart))
}

/// Interactive repair flow: rerun channel setup only without redoing full onboarding.
pub async fn run_channels_repair_wizard(config_dir: Option<&str>) -> Result<(Config, bool)> {
    println!("{}", style(BANNER).cyan().bold());
    println!(
        "  {}",
        style("Channels Repair — update channel tokens and allowlists only")
            .white()
            .bold()
    );
    println!();

    let mut config = Config::load_or_init_with_config_dir(config_dir).await?;

    print_step(1, 1, "Channels (How You Talk to OpenPRX)");
    config.channels_config = setup_channels()?;

    let config_d_dir = config
        .config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config.d");
    if config_d_dir.is_dir() {
        crate::config::files::write_split_config(&config, false).await?;
    } else {
        config.save().await?;
    }
    persist_workspace_selection(&config.config_path).await?;

    println!();
    println!(
        "  {} Channel config saved: {}",
        style("✓").green().bold(),
        style(config.config_path.display()).green()
    );

    let has_channels = has_launchable_channels(&config.channels_config);
    let mut autostart = false;

    if has_channels && config.api_key.is_some() {
        let launch: bool = Confirm::new()
            .with_prompt(format!(
                "  {} Launch channels now? (connected channels → AI → reply)",
                style("🚀").cyan()
            ))
            .default(true)
            .interact()?;

        if launch {
            println!();
            println!(
                "  {} {}",
                style("⚡").cyan(),
                style("Starting channel server...").white().bold()
            );
            println!();
            autostart = true;
        }
    }

    Ok((config, autostart))
}
mod quick;
pub use quick::*;

mod models;
pub use models::*;

mod workspace;
pub use workspace::*;

mod provider;
use provider::{
    credential_setup_hint, provider_credential_env_var, provider_supports_keyless_local_usage,
    quick_summary_provider_line, setup_provider, setup_provider_line,
};
#[cfg(test)]
use provider::{keyless_provider_hint, selected_provider_line};

mod preferences;
use preferences::{setup_decision_plane, setup_memory, setup_project_context, setup_tool_mode};

mod channels;
use channels::setup_channels;

mod tunnel;
use tunnel::setup_tunnel;

mod summary;
use summary::{print_bullet, print_step, print_summary};

mod persistence;
use persistence::persist_onboarding_config;

#[cfg(test)]
mod tests;
