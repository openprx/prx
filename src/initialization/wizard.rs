#![allow(clippy::print_stdout, clippy::print_stderr)]

#[cfg(test)]
use crate::auth::profiles::AuthProfilesStore;
#[cfg(feature = "channel-matrix")]
use crate::config::MatrixConfig;
use crate::config::init::Spec;
use crate::config::schema::{
    DingTalkConfig, IrcConfig, LarkReceiveMode, LinqConfig, QQConfig, StreamMode, WhatsAppConfig,
};
use crate::config::{
    ChannelsConfig, ComposioConfig, Config, DiscordConfig, IMessageConfig, LarkConfig, MemoryConfig,
    MemoryEventsConfig, MemorySemanticConfig, SecretsConfig, SlackConfig, TelegramConfig, WebhookConfig,
};
use crate::memory::{default_memory_backend_key, memory_backend_profile, selectable_memory_backends};
#[cfg(test)]
use crate::providers::catalog::discovery::{self, DiscoveredModel};
use crate::providers::{
    is_glm_alias, is_glm_cn_alias, is_minimax_alias, is_moonshot_alias, is_qianfan_alias, is_qwen_alias, is_zai_alias,
    is_zai_cn_alias,
};
use anyhow::{Context, Result};
use console::style;
use dialoguer::{Confirm, Input, Password, Select};
use serde_json::Value;
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ── Project context collected during wizard ──────────────────────

pub use crate::identity::scaffold::ProjectContext;

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

pub async fn run_wizard(config_dir: Option<&str>, spec: Spec, force: bool) -> Result<(Config, bool)> {
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
    let state_dir = config_path.parent().context("Config path has no parent")?;

    print_step(2, 9, "AI Provider & API Key");
    let (provider, api_key, model, provider_api_url) = setup_provider(&workspace_dir, state_dir).await?;
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

    let customize = |config: &mut Config| -> Result<()> {
        if !api_key.trim().is_empty() {
            config.api_key = Some(api_key.clone());
        } else if config.default_provider.as_deref() != Some(provider.as_str()) {
            config.api_key = None;
        }
        config.default_provider = Some(provider.clone());
        config.default_model = Some(model.trim().to_string());
        config.api_url = provider_api_url.clone();
        config.model_settings.extend(model_settings.clone());
        config.channels_config = channels_config.clone();
        config.tunnel = tunnel_config.clone();
        config.composio = composio_config.clone();
        config.secrets = secrets_config.clone();
        config.memory = memory_config.clone();
        check_onboarding_selection(config, &provider, model.trim())
    };
    if let Some((decision, secret)) = &decision {
        if secret.is_some() {
            decision.ensure_hosted_credential_free(&config_path)?;
        }
    }
    let target = config_path.parent().context("Config path has no parent")?;
    let report = spec
        .generate_customized(target, force, Some(&customize), Some(&project_ctx))
        .await?;
    report.print();
    report.into_result()?;
    let mut config = Config::load_from_path(&config_path, workspace_dir.clone())?;
    if let Some((decision, secret)) = decision {
        match secret {
            Some(secret) => {
                print!(
                    "{}",
                    decision
                        .apply_hosted(config.clone(), secret)
                        .await
                        .context("Base configuration committed; decision setup failed")?
                );
            }
            None => {
                decision
                    .apply(config.clone())
                    .await
                    .context("Base configuration committed; decision setup failed")?;
            }
        }
        config = Config::load_from_path(&config_path, workspace_dir.clone())?;
        let outcome = crate::causal_cli::test_configured_locally(&config)
            .await
            .context("Configuration committed; local decision diagnostic failed")?;
        println!("Decision backend local test: {outcome}");
    }
    persist_workspace_selection(&config_path)
        .await
        .context("Configuration committed; active workspace update failed")?;

    // ── Final summary ────────────────────────────────────────────
    print_summary(&config);

    // ── Offer to launch channels immediately ─────────────────────
    let has_channels = has_launchable_channels(&config.channels_config);
    let mut autostart = false;

    if has_channels {
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

    let target = crate::config::resolve_init_target_dir(None, config_dir).await?;
    anyhow::ensure!(
        target.join("config.toml").is_file(),
        "Channel repair requires an existing configuration; run prx init first"
    );
    let mut config = Config::load_from_path(&target.join("config.toml"), target.join("workspace"))?;

    print_step(1, 1, "Channels (How You Talk to OpenPRX)");
    config.channels_config = setup_channels()?;

    persistence::persist_onboarding_config(&config, true).await?;
    persist_workspace_selection(&config.config_path).await?;

    println!();
    println!(
        "  {} Channel config saved: {}",
        style("✓").green().bold(),
        style(config.config_path.display()).green()
    );

    let has_channels = has_launchable_channels(&config.channels_config);
    let mut autostart = false;

    if has_channels {
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

pub(crate) use crate::providers::model_discovery::*;

mod workspace;
pub use workspace::*;

mod provider;
use provider::{
    choose_model_level, credential_setup_hint, provider_supports_keyless_local_usage, quick_summary_provider_line,
    setup_provider,
};
#[cfg(test)]
use provider::{keyless_provider_hint, selected_provider_line, setup_provider_line};

mod preferences;
use preferences::{setup_decision_plane, setup_memory, setup_project_context, setup_tool_mode};

mod channels;
use channels::setup_channels;

mod tunnel;
use tunnel::setup_tunnel;

mod summary;
use summary::{print_bullet, print_step, print_summary};

mod persistence;
#[cfg(test)]
use persistence::persist_onboarding_config;

#[cfg(test)]
mod tests;
