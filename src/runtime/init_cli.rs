//! One CLI entry for preset generation, full setup and channel repair.
use crate::{Cli, Commands, manage_cli};
use anyhow::{Context, Result, bail};
use openprx::{channels, initialization};

pub(crate) async fn run(cli: &Cli) -> Result<()> {
    if let Commands::Init {
        spec,
        dir,
        force,
        interactive,
        channels_only,
        api_key,
        from_stdin,
        from_env,
        provider,
        model,
        memory,
        decision_backend,
        decision_endpoint,
        decision_model,
        decision_enable,
        decision_credential_stdin,
    } = &cli.command
    {
        let target = openprx::config::resolve_init_target_dir(dir.as_deref(), cli.config_dir.as_deref()).await?;
        let target_text = target.to_str().context("Initialization target is not UTF-8")?;
        let explicit = api_key.is_some()
            || *from_stdin
            || from_env.is_some()
            || provider.is_some()
            || model.is_some()
            || memory.is_some();
        anyhow::ensure!(
            !(*interactive && (*channels_only || explicit || decision_backend.is_some() || *decision_credential_stdin)),
            "--interactive cannot be combined with quick setup or channel repair flags"
        );
        anyhow::ensure!(
            !(*channels_only
                && (explicit || spec.is_some() || *force || decision_backend.is_some() || *decision_credential_stdin)),
            "--channels-only accepts only the target directory"
        );
        if *channels_only {
            anyhow::ensure!(
                target.join("config.toml").is_file(),
                "Channel repair requires an existing configuration; run prx init first"
            );
        } else {
            anyhow::ensure!(
                *force || !target.join("config.toml").exists(),
                "Configuration already exists; use --force to repair and preserve explicit settings"
            );
        }
        let spec = spec.unwrap_or(openprx::config::init::Spec::Minimal);
        let interactive = *interactive;
        // The decision flags use the one setup service (CDP-14); they are
        // quick-mode flags, like --provider.
        let decision = decision_backend.map(|kind| openprx::causal_cli::DecisionSetup {
            kind: kind.into(),
            endpoint: decision_endpoint.clone(),
            model: decision_model.clone(),
            enable: *decision_enable,
        });
        if decision.is_some() && (interactive || *channels_only) {
            bail!("--decision-backend is a quick-setup flag; the interactive wizard asks for the decision backend");
        }
        let hosted = decision
            .as_ref()
            .is_some_and(|decision| openprx::causal_cli::hosted_owner(decision.kind).is_some());
        if hosted && !*decision_credential_stdin {
            bail!(
                "a hosted decision backend needs its credential: add --decision-credential-stdin (the value is read \
                 from standard input and stored as a credential reference), or use `prx causal config set backend`"
            );
        }
        if *decision_credential_stdin && !hosted {
            bail!("--decision-credential-stdin applies only to the hosted backends typesafe and openrouter");
        }
        let decision_secret = if *decision_credential_stdin {
            manage_cli::secret_from(true, None, &mut std::io::stdin(), &|name: &str| {
                std::env::var(name).ok()
            })?
            .map(openprx::config::manage::credentials::SecretString::new)
        } else {
            None
        };
        let channels_only = *channels_only;
        if channels_only && (*from_stdin || from_env.is_some()) {
            bail!("--channels-only does not accept --from-stdin or --from-env");
        }
        let api_key = manage_cli::secret_from(
            *from_stdin,
            from_env.as_deref(),
            &mut std::io::stdin(),
            &|name: &str| std::env::var(name).ok(),
        )?
        .or_else(|| api_key.clone());
        let provider = provider.clone();
        let model = model.clone();
        let memory = memory.clone();

        if interactive && channels_only {
            bail!("Use either --interactive or --channels-only, not both");
        }
        if channels_only && (api_key.is_some() || provider.is_some() || model.is_some() || memory.is_some()) {
            bail!("--channels-only does not accept --api-key, --provider, --model, or --memory");
        }
        let config_dir = Some(target_text);
        let autostart_config = if channels_only {
            let (config, autostart) = initialization::wizard::run_channels_repair_wizard(config_dir).await?;
            if autostart { Some(config) } else { None }
        } else if interactive {
            let (config, autostart) = initialization::wizard::run_wizard(config_dir, spec, *force).await?;
            if autostart { Some(config) } else { None }
        } else {
            // Checked before anything is written; applied to the written
            // configuration as its own transaction.
            if let Some(decision) = &decision {
                if hosted {
                    decision.check_hosted()?;
                    decision
                        .ensure_hosted_credential_free(&initialization::wizard::quick_setup_config_path(config_dir)?)?;
                    anyhow::ensure!(
                        decision_secret
                            .as_ref()
                            .is_some_and(|secret| !secret.expose().trim().is_empty()),
                        "the decision credential read from standard input is empty; nothing was written"
                    );
                } else {
                    decision.check()?;
                }
            }
            let config = if explicit {
                initialization::wizard::run_quick_setup(
                    api_key.as_deref(),
                    provider.as_deref(),
                    model.as_deref(),
                    memory.as_deref(),
                    config_dir,
                    spec,
                    *force,
                )
                .await?
            } else {
                spec.generate(&target, *force).await?;
                openprx::config::Config::load_or_init_with_config_dir(config_dir).await?
            };
            if let Some(decision) = &decision {
                let text = match decision_secret {
                    Some(secret) => decision
                        .apply_hosted(config, secret)
                        .await
                        .context("Base configuration committed; decision setup failed")?,
                    None => decision
                        .apply(config)
                        .await
                        .context("Base configuration committed; decision setup failed")?,
                };
                print!("{text}");
                println!("Decision backend configured ({}).", decision.kind.as_str());
            }
            initialization::select_workspace(&target)
                .await
                .context("Configuration committed; workspace selection failed")?;
            None
        };
        if let Some(config) = autostart_config {
            // Non-dispatch direct call (init autostart): no dispatch-level
            // signal task is wired here, so use the explicitly-named
            // never-cancelled token. Termination is via the process default.
            channels::start_channels(config, crate::runtime::shutdown::never_cancelled_shutdown()).await?;
        }
        return Ok(());
    }

    anyhow::bail!("init dispatch received another command")
}
