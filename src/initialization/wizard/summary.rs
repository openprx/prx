use super::{
    Config, credential_setup_hint, has_launchable_channels, provider_supports_keyless_local_usage,
    quick_summary_provider_line, style,
};

// ── Step helpers ─────────────────────────────────────────────────

pub(super) fn print_step(current: u8, total: u8, title: &str) {
    println!();
    println!(
        "  {} {}",
        style(format!("[{current}/{total}]")).cyan().bold(),
        style(title).white().bold()
    );
    println!("  {}", style("─".repeat(50)).dim());
}

pub(super) fn print_bullet(text: &str) {
    println!("  {} {}", style("›").cyan(), text);
}

fn xai_subscription_saved(config: &Config) -> bool {
    let auth = crate::auth::AuthService::from_config(config);
    auth.get_profile("xai", None).ok().flatten().is_some_and(|profile| {
        profile.kind == crate::auth::profiles::AuthProfileKind::OAuth
            && profile.token_set.as_ref().is_some_and(|tokens| {
                tokens
                    .refresh_token
                    .as_deref()
                    .is_some_and(|token| !token.trim().is_empty())
            })
    })
}

// ── Final summary ────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
pub(super) fn print_summary(config: &Config) {
    let has_channels = has_launchable_channels(&config.channels_config);

    println!();
    println!(
        "  {}",
        style("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━").cyan()
    );
    println!(
        "  {}  {}",
        style("⚡").cyan(),
        style("OpenPRX is ready!").white().bold()
    );
    println!(
        "  {}",
        style("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━").cyan()
    );
    println!();

    println!("  {}", style("Configuration saved to:").dim());
    println!("    {}", style(config.config_path.display()).green());
    println!();

    println!("  {}", style("Quick summary:").white().bold());
    println!("{}", quick_summary_provider_line(config));
    println!(
        "    {} Model:         {}",
        style("🧠").cyan(),
        config.default_model.as_deref().unwrap_or("(default)")
    );
    println!("    {} Autonomy:      {:?}", style("🛡️").cyan(), config.autonomy.level);
    println!(
        "    {} Memory:        {} (auto-save: {})",
        style("🧠").cyan(),
        config.memory.backend,
        if config.memory.auto_save { "on" } else { "off" }
    );

    // Channels summary
    let mut channels: Vec<&str> = vec!["CLI"];
    if config.channels_config.telegram.is_some() {
        channels.push("Telegram");
    }
    if config.channels_config.discord.is_some() {
        channels.push("Discord");
    }
    if config.channels_config.slack.is_some() {
        channels.push("Slack");
    }
    if config.channels_config.imessage.is_some() {
        channels.push("iMessage");
    }
    #[cfg(feature = "channel-matrix")]
    if config.channels_config.matrix.is_some() {
        channels.push("Matrix");
    }
    if config.channels_config.email.is_some() {
        channels.push("Email");
    }
    if config.channels_config.webhook.is_some() {
        channels.push("Webhook");
    }
    if config.channels_config.lark.is_some() {
        channels.push("Lark");
    }
    println!("    {} Channels:      {}", style("📡").cyan(), channels.join(", "));

    println!(
        "    {} API Key:       {}",
        style("🔑").cyan(),
        if config.api_key.is_some() {
            style("configured").green().to_string()
        } else {
            style("not set (set via env var or config)").yellow().to_string()
        }
    );

    // Tunnel
    println!(
        "    {} Tunnel:        {}",
        style("🌐").cyan(),
        if config.tunnel.provider == "none" || config.tunnel.provider.is_empty() {
            "none (local only)".to_string()
        } else {
            config.tunnel.provider.clone()
        }
    );

    // Composio
    println!(
        "    {} Composio:      {}",
        style("🔗").cyan(),
        if config.composio.configured() {
            style("enabled (1000+ OAuth apps)").green().to_string()
        } else {
            "disabled (sovereign mode)".to_string()
        }
    );

    // Secrets
    println!("    {} Secrets:       configured", style("🔒").cyan());

    // Gateway
    println!(
        "    {} Gateway:       {}",
        style("🚪").cyan(),
        if config.gateway.require_pairing {
            "pairing required (secure)"
        } else {
            "pairing disabled"
        }
    );

    println!();
    println!("  {}", style("Next steps:").white().bold());
    println!();

    let mut step = 1u8;

    let provider = crate::providers::selection::default_provider(config);
    if config.api_key.is_none() && !provider_supports_keyless_local_usage(provider) {
        if provider == "openai-codex" {
            println!(
                "    {} Authenticate OpenAI Codex:",
                style(format!("{step}.")).cyan().bold()
            );
            println!(
                "       {}",
                style("prx auth login --provider openai-codex --device-code").yellow()
            );
        } else if matches!(provider, "xai" | "grok") {
            if xai_subscription_saved(config) {
                println!(
                    "    {} Grok subscription is saved. Pick a model with `prx models` if you want a different one.",
                    style(format!("{step}.")).cyan().bold()
                );
            } else {
                println!(
                    "    {} Sign in to Grok (SuperGrok / X Premium), or add an API key:",
                    style(format!("{step}.")).cyan().bold()
                );
                println!("       {}", style("prx auth login --provider xai").yellow());
                println!(
                    "       {}",
                    style("prx auth login --provider xai --device-code").yellow()
                );
                println!("       {}", style(credential_setup_hint(provider)).yellow());
            }
        } else if provider == "anthropic" {
            println!(
                "    {} Configure Anthropic auth:",
                style(format!("{step}.")).cyan().bold()
            );
            println!("       {}", style(credential_setup_hint(provider)).yellow());
            println!(
                "       {}",
                style("or, for a setup-token: prx auth paste-token --provider anthropic --auth-kind authorization")
                    .yellow()
            );
        } else {
            println!("    {} Add your API key:", style(format!("{step}.")).cyan().bold());
            println!("       {}", style(credential_setup_hint(provider)).yellow());
        }
        println!();
        step += 1;
    }

    // If channels are configured, show channel start as the primary next step
    if has_channels {
        println!(
            "    {} {} (connected channels → AI → reply):",
            style(format!("{step}.")).cyan().bold(),
            style("Launch your channels").white().bold()
        );
        println!("       {}", style("prx channel start").yellow());
        println!();
        step += 1;
    }

    println!("    {} Send a quick message:", style(format!("{step}.")).cyan().bold());
    println!("       {}", style("prx agent -m \"Hello, OpenPRX!\"").yellow());
    println!();
    step += 1;

    println!(
        "    {} Start interactive CLI mode:",
        style(format!("{step}.")).cyan().bold()
    );
    println!("       {}", style("prx agent").yellow());
    println!();
    step += 1;

    println!("    {} Check full status:", style(format!("{step}.")).cyan().bold());
    println!("       {}", style("prx status").yellow());
    println!();
    println!(
        "    {} Legacy alias (optional):",
        style(format!("{}.", step + 1)).cyan().bold()
    );
    println!(
        "       {}",
        style("ln -s \"$(command -v openprx)\" \"$HOME/.local/bin/openprx\"").yellow()
    );

    println!();
    println!("  {} {}", style("⚡").cyan(), style("Happy hacking! 🦀").white().bold());
    println!();
}
