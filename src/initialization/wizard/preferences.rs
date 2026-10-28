use super::{
    ComposioConfig, Confirm, Input, MemoryConfig, Password, ProjectContext, Result, SecretsConfig, Select,
    backend_key_from_choice, memory_backend_profile, memory_config_defaults_for_backend, print_bullet,
    selectable_memory_backends, style,
};

// ── Step 5: Tool Mode & Security ────────────────────────────────

pub(super) fn setup_tool_mode() -> Result<(ComposioConfig, SecretsConfig)> {
    print_bullet("Choose how OpenPRX connects to external apps.");
    print_bullet("You can always change this later in config.toml.");
    println!();

    let options = vec![
        "Sovereign (local only) — you manage API keys, full privacy (default)",
        "Composio (managed OAuth) — 1000+ apps via OAuth, no raw keys shared",
    ];

    let choice = Select::new()
        .with_prompt("  Select tool mode")
        .items(&options)
        .default(0)
        .interact()?;

    let composio_config = if choice == 1 {
        println!();
        println!(
            "  {} {}",
            style("Composio Setup").white().bold(),
            style("— 1000+ OAuth integrations (Gmail, Notion, GitHub, Slack, ...)").dim()
        );
        print_bullet("Get your API key at: https://app.composio.dev/settings");
        print_bullet("OpenPRX uses Composio as a tool — your core agent stays local.");
        println!();

        let api_key: String = Input::new()
            .with_prompt("  Composio API key (or Enter to skip)")
            .allow_empty(true)
            .interact_text()?;

        if api_key.trim().is_empty() {
            println!(
                "  {} Skipped — set composio.api_key in config.toml later",
                style("→").dim()
            );
            ComposioConfig::default()
        } else {
            println!(
                "  {} Composio: {} (1000+ OAuth tools available)",
                style("✓").green().bold(),
                style("enabled").green()
            );
            ComposioConfig {
                api_key: Some(api_key),
                ..ComposioConfig::default()
            }
        }
    } else {
        println!(
            "  {} Tool mode: {} — full privacy, you own every key",
            style("✓").green().bold(),
            style("Sovereign (local only)").green()
        );
        ComposioConfig::default()
    };

    // ── Encrypted secrets ──
    println!();
    print_bullet("OpenPRX can encrypt API keys stored in config.toml.");
    print_bullet("A local key file protects against plaintext exposure and accidental leaks.");

    let encrypt = Confirm::new()
        .with_prompt("  Enable encrypted secret storage?")
        .default(true)
        .interact()?;

    let secrets_config = SecretsConfig { encrypt };

    if encrypt {
        println!(
            "  {} Secrets: {} — keys encrypted with local key file",
            style("✓").green().bold(),
            style("encrypted").green()
        );
    } else {
        println!(
            "  {} Secrets: {} — keys stored as plaintext (not recommended)",
            style("✓").green().bold(),
            style("plaintext").yellow()
        );
    }

    Ok((composio_config, secrets_config))
}

// ── Step 7: Project Context ─────────────────────────────────────

pub(super) fn setup_project_context() -> Result<ProjectContext> {
    print_bullet("Let's personalize your agent. You can always update these later.");
    print_bullet("Press Enter to accept defaults.");
    println!();

    let user_name: String = Input::new()
        .with_prompt("  Your name")
        .default("User".into())
        .interact_text()?;

    let tz_options = vec![
        "US/Eastern (EST/EDT)",
        "US/Central (CST/CDT)",
        "US/Mountain (MST/MDT)",
        "US/Pacific (PST/PDT)",
        "Europe/London (GMT/BST)",
        "Europe/Berlin (CET/CEST)",
        "Asia/Tokyo (JST)",
        "UTC",
        "Other (type manually)",
    ];

    let tz_idx = Select::new()
        .with_prompt("  Your timezone")
        .items(&tz_options)
        .default(0)
        .interact()?;

    let timezone = if tz_idx == tz_options.len() - 1 {
        Input::new()
            .with_prompt("  Enter timezone (e.g. America/New_York)")
            .default("UTC".into())
            .interact_text()?
    } else {
        // Extract the short label before the parenthetical
        // SAFETY: dialoguer Select guarantees tz_idx < tz_options.len()
        #[allow(clippy::indexing_slicing)]
        tz_options[tz_idx].split('(').next().unwrap_or("UTC").trim().to_string()
    };

    let agent_name: String = Input::new()
        .with_prompt("  Agent name")
        .default("OpenPRX".into())
        .interact_text()?;

    let style_options = vec![
        "Direct & concise — skip pleasantries, get to the point",
        "Friendly & casual — warm, human, and helpful",
        "Professional & polished — calm, confident, and clear",
        "Expressive & playful — more personality + natural emojis",
        "Technical & detailed — thorough explanations, code-first",
        "Balanced — adapt to the situation",
        "Custom — write your own style guide",
    ];

    let style_idx = Select::new()
        .with_prompt("  Communication style")
        .items(&style_options)
        .default(1)
        .interact()?;

    let communication_style = match style_idx {
        0 => "Be direct and concise. Skip pleasantries. Get to the point.".to_string(),
        1 => "Be friendly, human, and conversational. Show warmth and empathy while staying efficient. Use natural contractions.".to_string(),
        2 => "Be professional and polished. Stay calm, structured, and respectful. Use occasional tone-setting emojis only when appropriate.".to_string(),
        3 => "Be expressive and playful when appropriate. Use relevant emojis naturally (0-2 max), and keep serious topics emoji-light.".to_string(),
        4 => "Be technical and detailed. Thorough explanations, code-first.".to_string(),
        5 => "Adapt to the situation. Default to warm and clear communication; be concise when needed, thorough when it matters.".to_string(),
        _ => Input::new()
            .with_prompt("  Custom communication style")
            .default(
                "Be warm, natural, and clear. Use occasional relevant emojis (1-2 max) and avoid robotic phrasing.".into(),
            )
            .interact_text()?,
    };

    println!(
        "  {} Context: {} | {} | {} | {}",
        style("✓").green().bold(),
        style(&user_name).green(),
        style(&timezone).green(),
        style(&agent_name).green(),
        style(&communication_style).green().dim()
    );

    Ok(ProjectContext {
        user_name,
        timezone,
        agent_name,
        communication_style,
    })
}

// ── Step 6: Memory Configuration ───────────────────────────────

pub(super) fn setup_memory() -> Result<MemoryConfig> {
    print_bullet("Choose how OpenPRX stores and searches memories.");
    print_bullet("You can always change this later in config.toml.");
    println!();

    let options: Vec<&str> = selectable_memory_backends()
        .iter()
        .map(|backend| backend.label)
        .collect();

    let choice = Select::new()
        .with_prompt("  Select memory backend")
        .items(&options)
        .default(0)
        .interact()?;

    let backend = backend_key_from_choice(choice);
    let profile = memory_backend_profile(backend);

    let auto_save = profile.auto_save_default
        && Confirm::new()
            .with_prompt("  Auto-save conversations to memory?")
            .default(true)
            .interact()?;

    println!(
        "  {} Memory: {} (auto-save: {})",
        style("✓").green().bold(),
        style(backend).green(),
        if auto_save { "on" } else { "off" }
    );

    let mut config = memory_config_defaults_for_backend(backend);
    config.auto_save = auto_save;
    Ok(config)
}

// ── Step 8: Decision plane (CDP-14) ─────────────────────────────

/// Choose the decision backend. The builtin (the default) keeps every
/// decision on this machine and changes nothing. A local System One
/// endpoint is checked by the one decision check before anything is
/// written, and tested locally once the configuration is written. A hosted
/// service needs a credential: it is read hidden, stored as a credential
/// reference and referenced in the same setup; remote processing stays off
/// until `prx causal config enable-remote`.
pub(super) fn setup_decision_plane() -> Result<
    Option<(
        crate::causal_cli::DecisionSetup,
        Option<crate::config::manage::credentials::SecretString>,
    )>,
> {
    print_bullet("The decision plane chooses between candidate actions before the model answers.");
    print_bullet("The builtin decides locally; nothing is sent anywhere.");
    println!();
    let options = [
        "Builtin (default): decisions stay on this machine",
        "A local System One endpoint (loopback)",
        "A hosted service (TypeSafe or OpenRouter): needs a credential; remote stays off until enabled",
    ];
    let choice = Select::new()
        .with_prompt("  Decision backend")
        .items(&options)
        .default(0)
        .interact()?;
    match choice {
        1 => {
            let endpoint: String = Input::new()
                .with_prompt("  Endpoint (http://127.0.0.1:<port>/v1)")
                .interact_text()?;
            let model: String = Input::new().with_prompt("  Model, one exact version").interact_text()?;
            let enable = Confirm::new()
                .with_prompt("  Turn the decision plane on now?")
                .default(true)
                .interact()?;
            let setup = crate::causal_cli::DecisionSetup {
                kind: crate::decision::BackendKind::CompatibleHttp,
                endpoint: Some(endpoint.trim().to_string()),
                model: Some(model.trim().to_string()),
                enable,
            };
            setup.check()?;
            Ok(Some((setup, None)))
        }
        2 => {
            print_bullet("A hosted service needs a credential. Decision state is sent to it only after you run");
            print_bullet("`prx causal config enable-remote`; until then the builtin decides locally.");
            let kinds = [
                crate::decision::BackendKind::TypeSafe,
                crate::decision::BackendKind::OpenRouter,
            ];
            let kind_labels = ["TypeSafe", "OpenRouter (Decisions API)"];
            let kind = Select::new()
                .with_prompt("  Hosted service")
                .items(&kind_labels)
                .default(0)
                .interact()?;
            let kind = kinds
                .get(kind)
                .copied()
                .unwrap_or(crate::decision::BackendKind::TypeSafe);
            let model: String = Input::new().with_prompt("  Model, one exact version").interact_text()?;
            let enable = Confirm::new()
                .with_prompt("  Turn the decision plane on now?")
                .default(true)
                .interact()?;
            let secret = crate::config::manage::credentials::SecretString::new(
                Password::new()
                    .with_prompt("  Service credential (input hidden)")
                    .interact()?,
            );
            if secret.expose().trim().is_empty() {
                println!(
                    "  {} No credential entered; the hosted backend is not set up.",
                    style("→").dim()
                );
                return Ok(None);
            }
            let setup = crate::causal_cli::DecisionSetup {
                kind,
                endpoint: None,
                model: Some(model.trim().to_string()),
                enable,
            };
            setup.check_hosted()?;
            Ok(Some((setup, Some(secret))))
        }
        _ => Ok(None),
    }
}
