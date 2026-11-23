#[allow(clippy::too_many_lines)]
async fn handle_auth_command(auth_command: AuthCommands, config: &Config) -> Result<()> {
    let auth_service = auth::AuthService::from_config(config);

    match auth_command {
        AuthCommands::Login {
            provider,
            profile,
            device_code,
        } => {
            let provider = auth::normalize_provider(&provider)?;
            if provider == "xai" {
                return login_xai(&auth_service, config, &profile, device_code).await;
            }
            if provider == "openrouter" {
                return login_openrouter(&auth_service, &profile, device_code).await;
            }
            if provider != "openai-codex" {
                bail!("`auth login` supports --provider openai-codex, --provider openrouter, and --provider xai");
            }

            let client = openprx::prx_http_client();

            if device_code {
                auth::openai_oauth::start_device_code_flow(&client)?;
            }

            let state_dir = auth::state_dir_from_config(config);
            let host_id = auth::openai_oauth::load_or_create_host_id(&state_dir)?;
            let saved_client_id = auth_service
                .get_profile("openai-codex", Some(&profile))?
                .and_then(|profile| {
                    profile
                        .metadata
                        .get(auth::openai_oauth::OPENAI_CLIENT_ID_METADATA)
                        .cloned()
                });
            let pkce = auth::openai_oauth::generate_pkce_state();
            let client_id_sent = saved_client_id
                .as_deref()
                .filter(|value| value.starts_with("oaiapp_"))
                .unwrap_or(auth::openai_oauth::OPENAI_OAUTH_DYNAMIC_CLIENT_ID)
                .to_string();
            let pending = PendingOpenAiLogin {
                profile: profile.clone(),
                code_verifier: pkce.code_verifier.clone(),
                state: pkce.state.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                host_id: host_id.clone(),
                client_id: client_id_sent.clone(),
            };
            save_pending_openai_login(config, &pending)?;

            let authorize_url =
                auth::openai_oauth::build_authorize_url(&pkce, &host_id, saved_client_id.as_deref(), None);
            println!("Open this URL in your browser and authorize access:");
            println!("{authorize_url}");
            println!();
            println!(
                "Waiting for callback at {} ...",
                auth::openai_oauth::OPENAI_OAUTH_REDIRECT_URI
            );

            let callback =
                match auth::openai_oauth::receive_loopback_code(&pkce.state, std::time::Duration::from_mins(3)).await {
                    Ok(callback) => callback,
                    Err(e) => {
                        println!("Callback capture failed: {e}");
                        println!("Run `prx auth paste-redirect --provider openai-codex --profile {profile}`");
                        return Ok(());
                    }
                };
            let code = auth::openai_oauth::parse_code_from_redirect(&callback, Some(&pkce.state))?;

            let issued = auth::openai_oauth::issued_client_id_from_callback(&callback)
                .or_else(|| client_id_sent.starts_with("oaiapp_").then(|| client_id_sent.clone()));
            let Some(exchange_client_id) = issued else {
                bail!(
                    "Sign in with ChatGPT did not return an issued oaiapp_ client id. Run `prx auth login --provider openai-codex` again."
                );
            };
            let token_set =
                auth::openai_oauth::exchange_code_for_tokens(&client, &code, &pkce, &exchange_client_id).await?;
            let account_id =
                extract_openai_account_id_for_profile(&token_set.access_token, token_set.id_token.as_deref());

            auth_service.store_openai_siwc_tokens(&profile, token_set, account_id, &exchange_client_id, true)?;
            clear_pending_openai_login(config);

            println!("Saved profile {profile}");
            println!("Active profile for openai-codex: {profile}");
            Ok(())
        }

        AuthCommands::PasteRedirect {
            provider,
            profile,
            input,
        } => {
            let provider = auth::normalize_provider(&provider)?;
            if provider == "xai" {
                return paste_xai_redirect(&auth_service, config, &profile, input).await;
            }
            if provider != "openai-codex" {
                bail!("`auth paste-redirect` supports --provider openai-codex and --provider xai");
            }

            let pending = load_pending_openai_login(config)?.ok_or_else(|| {
                anyhow::anyhow!("No pending OpenAI login found. Run `prx auth login --provider openai-codex` first.")
            })?;

            if pending.profile != profile {
                bail!(
                    "Pending login profile mismatch: pending={}, requested={}",
                    pending.profile,
                    profile
                );
            }

            let redirect_input = match input {
                Some(value) => value,
                None => read_plain_input("Paste redirect URL or OAuth code")?,
            };

            let code = auth::openai_oauth::parse_code_from_redirect(&redirect_input, Some(&pending.state))?;
            let issued = auth::openai_oauth::issued_client_id_from_callback(&redirect_input).or_else(|| {
                pending
                    .client_id
                    .starts_with("oaiapp_")
                    .then(|| pending.client_id.clone())
            });
            let Some(exchange_client_id) = issued else {
                bail!(
                    "Sign in with ChatGPT did not return an issued oaiapp_ client id. Run `prx auth login --provider openai-codex` again."
                );
            };

            let pkce = auth::openai_oauth::PkceState {
                code_verifier: pending.code_verifier.clone(),
                code_challenge: String::new(),
                state: pending.state.clone(),
                nonce: String::new(),
            };

            let client = openprx::prx_http_client();
            let token_set =
                auth::openai_oauth::exchange_code_for_tokens(&client, &code, &pkce, &exchange_client_id).await?;
            let account_id =
                extract_openai_account_id_for_profile(&token_set.access_token, token_set.id_token.as_deref());

            auth_service.store_openai_siwc_tokens(&profile, token_set, account_id, &exchange_client_id, true)?;
            clear_pending_openai_login(config);

            println!("Saved profile {profile}");
            println!("Active profile for openai-codex: {profile}");
            Ok(())
        }

        AuthCommands::PasteToken {
            provider,
            profile,
            token,
            from_stdin,
            auth_kind,
        } => {
            let provider = auth::normalize_provider(&provider)?;
            let piped = manage_cli::secret_from(from_stdin, None, &mut std::io::stdin(), &|_: &str| None)?;
            let token = match (token, piped) {
                (_, Some(piped)) => piped.trim().to_string(),
                (Some(token), None) => token.trim().to_string(),
                (None, None) => read_auth_input("Paste token")?,
            };
            if token.is_empty() {
                bail!("Token cannot be empty");
            }
            if auth::discovery::is_claude_code_oauth_setup_token(&token) {
                bail!("{}", auth::terms::ANTHROPIC_SUBSCRIPTION_REMOVED);
            }

            let kind = auth::anthropic_token::detect_auth_kind(&token, auth_kind.as_deref());
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("auth_kind".to_string(), kind.as_metadata_value().to_string());

            auth_service.store_provider_token(&provider, &profile, &token, metadata, true)?;
            println!("Saved profile {profile}");
            println!("Active profile for {provider}: {profile}");
            Ok(())
        }

        AuthCommands::SetupToken { provider, profile } => {
            let provider = auth::normalize_provider(&provider)?;
            let token = read_auth_input("Paste token")?;
            if token.is_empty() {
                bail!("Token cannot be empty");
            }
            if auth::discovery::is_claude_code_oauth_setup_token(&token) {
                bail!("{}", auth::terms::ANTHROPIC_SUBSCRIPTION_REMOVED);
            }

            let kind = auth::anthropic_token::detect_auth_kind(&token, Some("authorization"));
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("auth_kind".to_string(), kind.as_metadata_value().to_string());

            auth_service.store_provider_token(&provider, &profile, &token, metadata, true)?;
            println!("Saved profile {profile}");
            println!("Active profile for {provider}: {profile}");
            Ok(())
        }

        AuthCommands::Refresh { provider, profile } => {
            let provider = auth::normalize_provider(&provider)?;
            if provider == "xai" {
                return match auth_service.get_valid_xai_access_token(profile.as_deref()).await? {
                    Some(_) => {
                        println!("xAI Grok token is valid (refresh completed if needed).");
                        Ok(())
                    }
                    None => {
                        bail!("No xAI auth profile found. Run `prx auth login --provider xai`.")
                    }
                };
            }
            if provider != "openai-codex" {
                bail!("`auth refresh` supports --provider openai-codex and --provider xai");
            }

            match auth_service.get_valid_openai_access_token(profile.as_deref()).await? {
                Some(_) => {
                    println!("OpenAI Codex token is valid (refresh completed if needed).");
                    Ok(())
                }
                None => {
                    bail!("No OpenAI Codex auth profile found. Run `prx auth login --provider openai-codex`.")
                }
            }
        }

        AuthCommands::Logout { provider, profile } => {
            let provider = auth::normalize_provider(&provider)?;
            let removed = auth_service.remove_profile(&provider, &profile)?;
            if removed {
                println!("Removed auth profile {provider}:{profile}");
            } else {
                println!("Auth profile not found: {provider}:{profile}");
            }
            Ok(())
        }

        AuthCommands::Use { provider, profile } => {
            let provider = auth::normalize_provider(&provider)?;
            auth_service.set_active_profile(&provider, &profile)?;
            println!("Active profile for {provider}: {profile}");
            Ok(())
        }

        AuthCommands::List => {
            let data = auth_service.load_profiles()?;
            if data.profiles.is_empty() {
                println!("No auth profiles configured.");
                return Ok(());
            }

            for (id, profile) in &data.profiles {
                let active = data
                    .active_profiles
                    .get(&profile.provider)
                    .is_some_and(|active_id| active_id == id);
                let marker = if active { "*" } else { " " };
                println!("{marker} {id}");
            }

            Ok(())
        }

        AuthCommands::Status => {
            let data = auth_service.load_profiles()?;
            if data.profiles.is_empty() {
                println!("No auth profiles configured.");
                return Ok(());
            }

            for (id, profile) in &data.profiles {
                let active = data
                    .active_profiles
                    .get(&profile.provider)
                    .is_some_and(|active_id| active_id == id);
                let marker = if active { "*" } else { " " };
                println!(
                    "{} {} kind={:?} account={} expires={}",
                    marker,
                    id,
                    profile.kind,
                    crate::security::redact(profile.account_id.as_deref().unwrap_or("unknown")),
                    format_expiry(profile)
                );
            }

            println!();
            println!("Active profiles:");
            for (provider, profile_id) in &data.active_profiles {
                println!("  {provider}: {profile_id}");
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_definition_has_no_flag_conflicts() {
        Cli::command().debug_assert();
    }

    /// Oracle is clap argv, not a copy of `cli_tracing_command`. Mapping
    /// `Commands::Gateway` to `CliCommand::Chat` fails the gateway row.
    fn commands_coverage_tag(command: &Commands) -> &'static str {
        match command {
            Commands::Init { .. } | Commands::Token { .. } => "Init",
            Commands::Go { .. } => "Go",
            Commands::Chat { message: Some(_), .. } => "ChatMessage",
            Commands::Chat { .. } => "Chat",
            Commands::Gateway { .. } => "Gateway",
            Commands::Daemon { .. } => "Daemon",
            Commands::Service { .. } => "Service",
            Commands::Doctor { .. } => "Doctor",
            Commands::A2a { .. } => "A2a",
            Commands::Status { .. } => "Status",
            Commands::Tasks { .. } => "Tasks",
            Commands::Memory { .. } => "Memory",
            Commands::Evolution { .. } => "Evolution",
            #[cfg(feature = "wasm-plugins")]
            Commands::Plugin { .. } => "Plugin",
            Commands::Cost { .. } => "Cost",
            Commands::Fitness { .. } => "Fitness",
            Commands::Cron { .. } => "Cron",
            Commands::Xin { .. } => "Xin",
            Commands::Causal { .. } => "Causal",
            Commands::DecisionEngine { .. } => "DecisionEngine",
            Commands::Models { .. } => "Models",
            Commands::Providers { .. } => "Providers",
            Commands::Routes { .. } => "Routes",
            Commands::Channel {
                channel_command: ChannelCommands::Start,
            } => "ChannelStart",
            Commands::Channel { .. } => "ChannelManage",
            Commands::Integrations { .. } => "Integrations",
            Commands::Skills { .. } => "Skills",
            Commands::Migrate { .. } => "Migrate",
            Commands::Auth { .. } => "Auth",
            Commands::Approval { .. } => "Approval",
            Commands::Audit { .. } => "Audit",
            Commands::Config { .. } => "Config",
            #[cfg(feature = "test-mock")]
            Commands::MailboxHarness { .. } => "MailboxHarness",
            Commands::SessionWorker { .. } => "SessionWorker",
            Commands::Completions { .. } => "Completions",
        }
    }

    #[test]
    fn cli_tracing_clap_argv_maps_every_command() {
        let cases: &[(&[&str], cli_tracing::CliCommand)] = &[
            (&["prx", "init"], cli_tracing::CliCommand::Init),
            (&["prx", "go"], cli_tracing::CliCommand::Go),
            (&["prx", "chat"], cli_tracing::CliCommand::Chat),
            (
                &["prx", "chat", "--message", "hi"],
                cli_tracing::CliCommand::ChatMessage,
            ),
            (&["prx", "gateway"], cli_tracing::CliCommand::Gateway),
            (&["prx", "daemon"], cli_tracing::CliCommand::Daemon),
            (&["prx", "service", "status"], cli_tracing::CliCommand::Service),
            (&["prx", "doctor"], cli_tracing::CliCommand::Doctor),
            (&["prx", "status"], cli_tracing::CliCommand::Status),
            (&["prx", "tasks", "list"], cli_tracing::CliCommand::Tasks),
            (&["prx", "memory", "reindex"], cli_tracing::CliCommand::Memory),
            (&["prx", "evolution", "status"], cli_tracing::CliCommand::Evolution),
            (&["prx", "cost"], cli_tracing::CliCommand::Cost),
            (&["prx", "fitness", "status"], cli_tracing::CliCommand::Fitness),
            (&["prx", "cron", "list"], cli_tracing::CliCommand::Cron),
            (&["prx", "xin", "status"], cli_tracing::CliCommand::Xin),
            (&["prx", "causal", "status"], cli_tracing::CliCommand::Causal),
            (
                &["prx", "decision-engine", "status"],
                cli_tracing::CliCommand::DecisionEngine,
            ),
            (&["prx", "models"], cli_tracing::CliCommand::Models),
            (&["prx", "providers"], cli_tracing::CliCommand::Providers),
            (&["prx", "routes", "list"], cli_tracing::CliCommand::Routes),
            (&["prx", "channel", "start"], cli_tracing::CliCommand::ChannelStart),
            (&["prx", "channel", "list"], cli_tracing::CliCommand::ChannelManage),
            (&["prx", "integrations"], cli_tracing::CliCommand::Integrations),
            (&["prx", "skills", "list"], cli_tracing::CliCommand::Skills),
            (&["prx", "migrate", "status"], cli_tracing::CliCommand::Migrate),
            (
                &["prx", "auth", "login", "--provider", "openai-codex"],
                cli_tracing::CliCommand::Auth,
            ),
            (&["prx", "approval", "list"], cli_tracing::CliCommand::Approval),
            (&["prx", "audit", "attest-eu-ai-act"], cli_tracing::CliCommand::Audit),
            (&["prx", "config", "show"], cli_tracing::CliCommand::Config),
            (&["prx", "session-worker"], cli_tracing::CliCommand::SessionWorker),
            (&["prx", "completions", "bash"], cli_tracing::CliCommand::Completions),
            #[cfg(feature = "test-mock")]
            (
                &[
                    "prx",
                    "__mailbox-harness",
                    "accept-adopt",
                    "--workspace",
                    "prx_workspace",
                ],
                cli_tracing::CliCommand::SessionWorker,
            ),
        ];
        #[cfg(feature = "wasm-plugins")]
        let plugin_case: &[(&[&str], cli_tracing::CliCommand)] =
            &[(&["prx", "plugin", "list"], cli_tracing::CliCommand::Plugin)];
        #[cfg(not(feature = "wasm-plugins"))]
        let plugin_case: &[(&[&str], cli_tracing::CliCommand)] = &[];

        let mut seen = Vec::new();
        for (argv, want) in cases.iter().chain(plugin_case.iter()) {
            let cli = Cli::try_parse_from(*argv).unwrap_or_else(|error| panic!("{argv:?}: {error}"));
            assert_eq!(cli_tracing_command(&cli.command), *want, "{argv:?}");
            seen.push(commands_coverage_tag(&cli.command));
        }
        let mut expected_tags = vec![
            "Init",
            "Go",
            "Chat",
            "ChatMessage",
            "Gateway",
            "Daemon",
            "Service",
            "Doctor",
            "Status",
            "Tasks",
            "Memory",
            "Evolution",
            "Cost",
            "Fitness",
            "Cron",
            "Xin",
            "Causal",
            "DecisionEngine",
            "Models",
            "Providers",
            "Routes",
            "ChannelStart",
            "ChannelManage",
            "Integrations",
            "Skills",
            "Migrate",
            "Auth",
            "Approval",
            "Audit",
            "Config",
            "SessionWorker",
            "Completions",
        ];
        #[cfg(feature = "wasm-plugins")]
        expected_tags.push("Plugin");
        #[cfg(feature = "test-mock")]
        expected_tags.push("MailboxHarness");
        expected_tags.sort_unstable();
        seen.sort_unstable();
        assert_eq!(seen, expected_tags);
    }

    /// E-4 (RED before PMM-E): a secret on the command line draws a warning
    /// that names the alternative and never repeats the secret; the stdin
    /// and environment alternatives draw none.
    #[test]
    fn a_secret_argument_draws_a_warning_naming_the_alternative() {
        const SENTINEL: &str = "sk-pmme-warning-sentinel";
        for (args, flag, alternative) in [
            (vec!["prx", "init", "--api-key", SENTINEL], "--api-key", "--from-stdin"),
            (vec!["prx", "go", "-k", SENTINEL], "-k/--key", "--from-stdin"),
            (
                vec![
                    "prx",
                    "auth",
                    "paste-token",
                    "--provider",
                    "anthropic",
                    "--token",
                    SENTINEL,
                ],
                "--token",
                "--from-stdin",
            ),
        ] {
            let cli = Cli::try_parse_from(&args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
            let warning = argv_secret_warning(&cli.command).unwrap_or_else(|| panic!("{args:?}: no warning"));
            assert!(warning.contains(flag) && warning.contains(alternative), "{warning}");
            assert!(!warning.contains(SENTINEL), "{warning}");
        }
        for args in [
            vec!["prx", "init", "--from-stdin"],
            vec!["prx", "init", "--from-env", "OPENROUTER_API_KEY"],
            vec!["prx", "go", "--from-env", "ANTHROPIC_API_KEY"],
            vec!["prx", "go", "--from-stdin", "--message", "hi"],
            vec!["prx", "auth", "paste-token", "--provider", "anthropic", "--from-stdin"],
            vec!["prx", "auth", "paste-token", "--provider", "anthropic"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
            assert_eq!(argv_secret_warning(&cli.command), None, "{args:?}");
        }
        // The argument and its alternative are exclusive.
        assert!(Cli::try_parse_from(["prx", "init", "--api-key", SENTINEL, "--from-stdin"]).is_err());
        assert!(Cli::try_parse_from(["prx", "go", "-k", SENTINEL, "--from-env", "X"]).is_err());
    }

    /// `prx tasks send` must repeat the server's outcome, not upgrade it.
    ///
    /// The endpoint tags a success `queued` because it can only see that the
    /// message reached the target's steering queue. This renderer used to print
    /// "Delivered to ..." unconditionally and never printed the outcome at all,
    /// so an operator steering a run that never drains its queue was told the
    /// instruction had landed.
    ///
    /// MUTATION GUARD: restore a fixed "Delivered" headline in
    /// `render_task_message` and the first two assertions fail.
    #[test]
    fn a_sent_message_is_reported_as_the_server_tagged_it() {
        let report = openprx::runtime::tasks_cli::MessageReport {
            requested: "w3".to_string(),
            id: "w3".to_string(),
            run_id: Some("d9671848-1111-2222-3333-444444444444".to_string()),
            kind: "sub_agent".to_string(),
            name: "sub-agent".to_string(),
            outcome: "queued".to_string(),
            note: Some("the message is on the target's steering queue".to_string()),
        };

        let rendered = render_task_message(&report);
        assert!(
            !rendered.to_lowercase().contains("deliver"),
            "the CLI must not claim a delivery the server did not report: {rendered}"
        );
        assert!(
            rendered.contains("outcome: queued"),
            "the server's own tag must be on screen: {rendered}"
        );
        assert!(rendered.contains("w3"), "{rendered}");
        assert!(
            rendered.contains("run_id: d9671848-1111-2222-3333-444444444444"),
            "{rendered}"
        );
        assert!(
            rendered.contains("the message is on the target's steering queue"),
            "the server's note explains why queued is not read: {rendered}"
        );
    }

    /// A gateway that sends no note gets no invented one.
    #[test]
    fn a_missing_note_is_not_replaced_by_a_client_side_guess() {
        let report = openprx::runtime::tasks_cli::MessageReport {
            requested: "w3".to_string(),
            id: "w3".to_string(),
            run_id: None,
            kind: "sub_agent".to_string(),
            name: "sub-agent".to_string(),
            outcome: "queued".to_string(),
            note: None,
        };

        let rendered = render_task_message(&report);
        assert_eq!(
            rendered, "Message to w3 [sub_agent] sub-agent (run_id: -) — outcome: queued",
            "with no note the renderer prints one line and nothing else"
        );
    }

    fn listed(kind: &str, name: &str, steerable: bool) -> openprx::runtime::tasks_cli::WorkItem {
        openprx::runtime::tasks_cli::WorkItem {
            id: "w2".to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            state: "running".to_string(),
            parent: None,
            run_id: None,
            batch_id: None,
            elapsed_secs: 3,
            pid: None,
            pgid: None,
            steerable,
        }
    }

    /// The listing has to separate the rows that accept a message from the ones
    /// that only look like they do.
    ///
    /// The KIND column cannot do it: a detached gateway job is registered as
    /// `sub_agent` and has no steering channel at all, so an operator reading
    /// the kind as permission sends a message, gets a 409, and has no way to
    /// tell a wrong address from an unsteerable target. Both rows below are
    /// `sub_agent`; only one of them can be steered.
    ///
    /// MUTATION GUARD: drop the `(steerable)` mark from `work_item_line` and
    /// the first assertion fails.
    #[test]
    fn a_steerable_row_is_marked_and_a_look_alike_is_not() {
        let run = work_item_line(&listed("sub_agent", "researcher", true), false);
        assert!(
            run.contains("(steerable)"),
            "a run that takes messages must say so: {run}"
        );

        let job = work_item_line(&listed("sub_agent", "gateway:webhook:anonymous", false), false);
        assert!(
            !job.contains("steerable"),
            "a gateway job is listed sub_agent but takes no messages, so it must not be marked: {job}"
        );
        assert!(job.contains("sub_agent"), "the kind is still reported as it is: {job}");
    }

    #[test]
    fn init_help_includes_model_flag() {
        let cmd = Cli::command();
        let init = cmd
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == "init")
            .expect("init subcommand must exist");

        let has_model_flag = init
            .get_arguments()
            .any(|arg| arg.get_id().as_str() == "model" && arg.get_long() == Some("model"));

        assert!(
            has_model_flag,
            "init help should include --model for quick setup overrides"
        );
    }

    #[test]
    fn init_cli_accepts_model_provider_and_api_key_in_quick_mode() {
        let cli = Cli::try_parse_from([
            "prx",
            "init",
            "--provider",
            "openrouter",
            "--model",
            "custom-model-946",
            "--api-key",
            "sk-issue946",
        ])
        .expect("quick init invocation should parse");

        match cli.command {
            Commands::Init {
                interactive,
                channels_only,
                api_key,
                provider,
                model,
                ..
            } => {
                assert!(!interactive);
                assert!(!channels_only);
                assert_eq!(provider.as_deref(), Some("openrouter"));
                assert_eq!(model.as_deref(), Some("custom-model-946"));
                assert_eq!(api_key.as_deref(), Some("sk-issue946"));
            }
            other => panic!("expected init command, got {other:?}"),
        }
    }

    #[test]
    fn chat_continue_flag_parses_as_resume_last_intent() {
        let cli = Cli::try_parse_from(["prx", "chat", "--continue"]).expect("chat --continue should parse");
        match cli.command {
            Commands::Chat {
                continue_last, session, ..
            } => {
                assert!(continue_last);
                assert!(session.is_none());
            }
            other => panic!("expected chat command, got {other:?}"),
        }
    }

    #[test]
    fn chat_explicit_session_can_coexist_with_continue() {
        let cli = Cli::try_parse_from(["prx", "chat", "--continue", "--session", "abc"])
            .expect("explicit --session with --continue should parse");
        match cli.command {
            Commands::Chat {
                continue_last, session, ..
            } => {
                assert!(continue_last);
                assert_eq!(session.as_deref(), Some("abc"));
            }
            other => panic!("expected chat command, got {other:?}"),
        }
    }

    #[test]
    fn chat_message_parses_short_flag_save_and_workdir() {
        let root = tempfile::tempdir().expect("test: root");
        let workdir = root.path().to_str().expect("test: root path");
        let cli = Cli::try_parse_from([
            "prx",
            "chat",
            "-q",
            "/compact",
            "--save",
            "--workdir",
            workdir,
            "-m",
            "m1",
        ])
        .expect("test: chat one-shot flags");
        match cli.command {
            Commands::Chat {
                message,
                save,
                workdir: Some(parsed),
                model,
                ..
            } => {
                assert_eq!(message.as_deref(), Some("/compact"));
                assert!(save);
                assert_eq!(model.as_deref(), Some("m1"));
                assert_eq!(parsed, root.path().canonicalize().expect("test: canonical root"));
            }
            other => panic!("expected chat command, got {other:?}"),
        }
        for argv in [
            &["prx", "chat", "--save"][..],
            &["prx", "chat", "--message", "x", "--list-sessions"],
            &["prx", "chat", "--workdir", "relative"],
        ] {
            let error = Cli::try_parse_from(argv).expect_err("test: invalid one-shot usage");
            assert_eq!(error.exit_code(), 2, "test: clap usage exit for {argv:?}");
        }
    }

    #[test]
    fn chat_workdir_is_canonical_and_requires_an_existing_absolute_directory() {
        let root = tempfile::tempdir().expect("test: root");
        let alias = root.path().join(".");
        let cli = Cli::try_parse_from(["prx", "chat", "--workdir", alias.to_str().expect("test: root path")])
            .expect("test: canonical workdir");
        assert!(
            matches!(cli.command, Commands::Chat { workdir: Some(ref workdir), .. } if *workdir == root.path().canonicalize().expect("test: canonical root")),
            "test: parsed root canonicalized"
        );
        for path in [
            "relative",
            root.path().join("missing").to_str().expect("test: missing path"),
        ] {
            let error = Cli::try_parse_from(["prx", "chat", "--workdir", path]).expect_err("test: invalid root");
            assert_eq!(error.exit_code(), 2, "test: clap usage exit");
        }
    }

    #[test]
    fn prx_causal_parses_any_arguments_and_decision_engine_replaces_it() {
        for args in [
            &["prx", "causal"][..],
            &["prx", "causal", "status", "--json"],
            &["prx", "causal", "config", "set", "backend", "--kind", "builtin"],
        ] {
            let cli = Cli::try_parse_from(args).expect("retired causal command still parses");
            assert!(matches!(cli.command, Commands::Causal { .. }), "{args:?}");
        }
        let cli = Cli::try_parse_from(["prx", "decision-engine", "status", "--json"]).expect("status parses");
        assert!(matches!(
            cli.command,
            Commands::DecisionEngine {
                json: true,
                command: openprx::decision_engine_cli::DecisionEngineCommands::Status,
            }
        ));
        let cli = Cli::try_parse_from(["prx", "decision-engine", "config", "set", "enabled", "true", "--yes"])
            .expect("config set parses");
        assert!(matches!(
            cli.command,
            Commands::DecisionEngine {
                command: openprx::decision_engine_cli::DecisionEngineCommands::Config { .. },
                ..
            }
        ));
        assert!(
            Cli::try_parse_from([
                "prx",
                "decision-engine",
                "config",
                "set",
                "enabled",
                "true",
                "--confirm-high-risk",
                "r"
            ])
            .is_err(),
            "a confirmation without --yes"
        );
    }

    /// No `prx decision-engine` subcommand takes a secret on the command line.
    #[test]
    fn no_decision_engine_subcommand_accepts_a_secret_argument() {
        fn check(command: &clap::Command, path: &str, violations: &mut Vec<String>) {
            for arg in command.get_arguments() {
                let names = std::iter::once(arg.get_id().as_str().to_string())
                    .chain(arg.get_long().map(str::to_string))
                    .map(|name| name.to_ascii_lowercase());
                for name in names {
                    if ["key", "token", "secret", "password", "credential", "bearer"]
                        .iter()
                        .any(|word| name.contains(word))
                        && name != "key"
                    {
                        violations.push(format!("{path} --{name}"));
                    }
                }
            }
            for sub in command.get_subcommands() {
                check(sub, &format!("{path} {}", sub.get_name()), violations);
            }
        }
        let root = Cli::command();
        let engine = root
            .find_subcommand("decision-engine")
            .expect("prx decision-engine exists");
        let mut violations = Vec::new();
        check(engine, "decision-engine", &mut violations);
        assert!(violations.is_empty(), "secret-shaped arguments: {violations:?}");
    }

    #[test]
    fn completions_cli_parses_supported_shells() {
        for shell in ["bash", "fish", "zsh", "powershell", "elvish"] {
            let cli = Cli::try_parse_from(["prx", "completions", shell]).expect("completions invocation should parse");
            match cli.command {
                Commands::Completions { .. } => {}
                other => panic!("expected completions command, got {other:?}"),
            }
        }
    }

    #[test]
    fn config_show_cli_parses_default_and_json_format() {
        let cli = Cli::try_parse_from(["prx", "config", "show"]).expect("config show should parse");
        match cli.command {
            Commands::Config {
                config_command: ConfigCommands::Show { format },
            } => assert_eq!(format, ConfigShowFormat::Toml),
            other => panic!("expected config show command, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["prx", "config", "show", "--format", "json"])
            .expect("config show --format json should parse");
        match cli.command {
            Commands::Config {
                config_command: ConfigCommands::Show { format },
            } => assert_eq!(format, ConfigShowFormat::Json),
            other => panic!("expected config show json command, got {other:?}"),
        }
    }

    #[test]
    fn memory_reindex_cli_parses() {
        let cli = Cli::try_parse_from(["prx", "memory", "reindex"]).expect("memory reindex should parse");
        match cli.command {
            Commands::Memory {
                memory_command: MemoryCommands::Reindex { json },
            } => assert!(!json),
            other => panic!("expected memory reindex command, got {other:?}"),
        }

        let cli =
            Cli::try_parse_from(["prx", "memory", "reindex", "--json"]).expect("memory reindex --json should parse");
        match cli.command {
            Commands::Memory {
                memory_command: MemoryCommands::Reindex { json },
            } => assert!(json),
            other => panic!("expected memory reindex command, got {other:?}"),
        }
    }

    #[test]
    fn approval_revoke_cli_parses() {
        let cli = Cli::try_parse_from([
            "prx",
            "approval",
            "revoke",
            "grant-test",
            "--reason",
            "operator requested",
        ])
        .expect("approval revoke should parse");
        match cli.command {
            Commands::Approval {
                approval_command: ApprovalCommands::Revoke { grant_id, reason },
            } => {
                assert_eq!(grant_id, "grant-test");
                assert_eq!(reason, "operator requested");
            }
            other => panic!("expected approval revoke command, got {other:?}"),
        }
    }

    #[test]
    fn eu_ai_act_audit_cli_parses_json_and_output() {
        let cli = Cli::try_parse_from([
            "prx",
            "audit",
            "attest-eu-ai-act",
            "--json",
            "--output",
            "/tmp/eu-ai-act-test.json",
        ])
        .expect("eu ai act attestation should parse");

        match cli.command {
            Commands::Audit {
                audit_command:
                    AuditCommands::AttestEuAiAct {
                        json,
                        format,
                        output,
                        verbose,
                        fail_on,
                    },
            } => {
                assert!(json);
                assert_eq!(format, AuditOutputFormat::Markdown);
                assert_eq!(
                    output.as_deref(),
                    Some(std::path::Path::new("/tmp/eu-ai-act-test.json"))
                );
                assert!(!verbose);
                assert_eq!(fail_on, None);
            }
            other => panic!("expected audit attest-eu-ai-act command, got {other:?}"),
        }
    }

    #[test]
    fn eu_declaration_cli_requires_explicit_signature_reference() {
        let cli = Cli::try_parse_from([
            "prx",
            "audit",
            "generate-eu-declaration",
            "--output",
            "declaration.json",
            "--signature-reference",
            "external:receipt-1",
        ])
        .expect("declaration generator should parse");
        match cli.command {
            Commands::Audit {
                audit_command:
                    AuditCommands::GenerateEuDeclaration {
                        output,
                        signature_reference,
                    },
            } => {
                assert_eq!(output.as_deref(), Some(std::path::Path::new("declaration.json")));
                assert_eq!(signature_reference, "external:receipt-1");
            }
            other => panic!("expected declaration generator, got {other:?}"),
        }
        assert!(
            Cli::try_parse_from([
                "prx",
                "audit",
                "generate-eu-declaration",
                "--output",
                "declaration.json",
            ])
            .is_err()
        );
    }

    #[test]
    fn incident_audit_cli_parses_mutating_and_export_commands() {
        let create = Cli::try_parse_from(["prx", "audit", "incident-create", "--input", "incident.json"])
            .expect("incident create should parse");
        assert!(matches!(
            create.command,
            Commands::Audit {
                audit_command: AuditCommands::IncidentCreate { input }
            } if input == std::path::Path::new("incident.json")
        ));

        let export = Cli::try_parse_from([
            "prx",
            "audit",
            "incident-export",
            "incident-7",
            "--output",
            "incident-7-export.json",
        ])
        .expect("incident export should parse");
        assert!(matches!(
            export.command,
            Commands::Audit {
                audit_command: AuditCommands::IncidentExport {
                    incident_id,
                    output,
                }
            } if incident_id == "incident-7" && output == std::path::Path::new("incident-7-export.json")
        ));
    }

    /// A default config whose files live in `dir`. The binary's tests link the
    /// production library, where the default paths name the real
    /// `~/.openprx`, and the attestation reads the workspace (D-070).
    fn scratch_config(dir: &std::path::Path) -> Config {
        Config {
            config_path: dir.join("config.toml"),
            workspace_dir: dir.join("workspace"),
            ..Config::default()
        }
    }

    #[test]
    fn declaration_attestation_respects_operator_classification() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut config = scratch_config(temp.path());
        let c02_status = |config: &Config| {
            build_eu_ai_act_attestation(config)
                .checks
                .into_iter()
                .find(|check| check.id == "C02")
                .expect("C02 control")
                .status
        };
        assert_eq!(c02_status(&config), ControlStatus::Unknown);

        config.compliance.eu_ai_act.classification.status = config::schema::EuAiActRiskClassification::NotHighRisk;
        config.compliance.eu_ai_act.classification.owner = Some("legal-owner".to_string());
        config.compliance.eu_ai_act.classification.assessed_at = Some("2026-07-18".to_string());
        assert_eq!(c02_status(&config), ControlStatus::NotApplicable);

        config.compliance.eu_ai_act.classification.status = config::schema::EuAiActRiskClassification::HighRisk;
        assert_eq!(c02_status(&config), ControlStatus::Fail);
    }

    #[test]
    fn incident_attestation_requires_and_verifies_durable_store_for_high_risk() {
        let temp = tempfile::TempDir::new().unwrap();
        let store_path = temp.path().join("article-73.sqlite3");
        let mut config = scratch_config(temp.path());
        config.compliance.eu_ai_act.classification.status = config::schema::EuAiActRiskClassification::HighRisk;

        let m04_status = |config: &Config| {
            build_eu_ai_act_attestation(config)
                .checks
                .into_iter()
                .find(|check| check.id == "M04")
                .expect("M04 control")
                .status
        };
        assert_eq!(m04_status(&config), ControlStatus::Fail);

        config.compliance.eu_ai_act.incident_store_path = Some(store_path.clone());
        openprx::compliance::incident::IncidentStore::open(&store_path).unwrap();
        assert_eq!(m04_status(&config), ControlStatus::Pass);
    }

    #[test]
    fn eu_ai_act_attestation_has_required_check_counts() {
        let temp = tempfile::TempDir::new().unwrap();
        let attestation = build_eu_ai_act_attestation(&scratch_config(temp.path()));

        assert_eq!(attestation.total_checks, 24);
        assert_eq!(attestation.checks.len(), 24);
        assert_eq!(
            attestation.total_checks,
            attestation.passed_count
                + attestation.warning_count
                + attestation.failed_count
                + attestation.unknown_count
                + attestation.not_applicable_count
        );

        let json = render_eu_ai_act_attestation_json(&attestation).expect("attestation should serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("attestation json should parse");
        assert_eq!(value.get("total_checks").and_then(serde_json::Value::as_u64), Some(24));
        assert_eq!(
            value.get("report_type").and_then(serde_json::Value::as_str),
            Some("EU AI Act implementation attestation")
        );
        let c02 = value
            .get("checks")
            .and_then(serde_json::Value::as_array)
            .and_then(|checks| {
                checks
                    .iter()
                    .find(|check| check.get("id").and_then(serde_json::Value::as_str) == Some("C02"))
            })
            .expect("C02 control should exist");
        assert_eq!(
            c02.get("framework_reference").and_then(serde_json::Value::as_str),
            Some("Article 47 and Annex V")
        );
    }

    #[test]
    fn eu_ai_act_audit_cli_parses_fail_on_and_gate_semantics() {
        let cli = Cli::try_parse_from(["prx", "audit", "attest-eu-ai-act", "--json", "--fail-on", "warning"])
            .expect("strict attestation gate should parse");
        let Commands::Audit {
            audit_command: AuditCommands::AttestEuAiAct { fail_on, .. },
        } = cli.command
        else {
            panic!("expected audit command");
        };
        assert_eq!(fail_on, Some(AuditFailOn::Warning));

        let temp = tempfile::TempDir::new().unwrap();
        let attestation = build_eu_ai_act_attestation(&scratch_config(temp.path()));
        assert!(!attestation_fails_gate(&attestation, AuditFailOn::Fail));
        assert!(attestation_fails_gate(&attestation, AuditFailOn::Warning));
    }

    #[test]
    fn doctor_memory_cli_parses() {
        let cli = Cli::try_parse_from(["prx", "doctor", "memory"]).expect("doctor memory should parse");
        match cli.command {
            Commands::Doctor {
                doctor_command: Some(DoctorCommands::Memory),
            } => {}
            other => panic!("expected doctor memory command, got {other:?}"),
        }
    }

    #[test]
    fn doctor_runtime_cli_parses() {
        let cli = Cli::try_parse_from(["prx", "doctor", "runtime"]).expect("doctor runtime should parse");
        match cli.command {
            Commands::Doctor {
                doctor_command: Some(DoctorCommands::Runtime),
            } => {}
            other => panic!("expected doctor runtime command, got {other:?}"),
        }
    }

    #[test]
    fn config_show_redacts_sensitive_toml_values() {
        let mut value = toml::Value::Table(toml::toml! {
            default_provider = "kimi-code"
            api_key = "enc2:encrypted-root"

            [storage.provider.config]
            db_url = "postgres://user:secret@example/prx"

            [agents.worker]
            api_key = "sk-agent"
            model = "kimi-k2"

            [channels.slack]
            bot_token = "xoxb-token"
            allowed_users = ["alice"]

            [nested]
            api_keys = ["sk-one", "sk-two"]
            credential = { username = "user", password = "pass" }
        });

        config::secret_paths::redact(&mut value);

        let root = value.as_table().expect("redacted value should remain a table");
        assert_eq!(root.get("api_key").and_then(toml::Value::as_str), Some("***"));
        assert_eq!(
            root.get("default_provider").and_then(toml::Value::as_str),
            Some("kimi-code")
        );

        let nested = root
            .get("nested")
            .and_then(toml::Value::as_table)
            .expect("nested table should remain");
        assert_eq!(
            nested
                .get("api_keys")
                .and_then(toml::Value::as_array)
                .expect("api_keys should remain an array")
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["***", "***"]
        );

        let rendered = toml::to_string_pretty(&value).expect("redacted config should serialize");
        assert!(rendered.contains("default_provider = \"kimi-code\""));
        assert!(rendered.contains("model = \"kimi-k2\""));
        assert!(rendered.contains("allowed_users = [\"alice\"]"));
        assert!(rendered.contains("api_key = \"***\""));
        assert!(rendered.contains("db_url = \"***\""));
        assert!(rendered.contains("bot_token = \"***\""));
        assert!(!rendered.contains("enc2:encrypted-root"));
        assert!(!rendered.contains("postgres://user:secret@example/prx"));
        assert!(!rendered.contains("sk-agent"));
        assert!(!rendered.contains("xoxb-token"));
        assert!(!rendered.contains("sk-one"));
        assert!(!rendered.contains("password = \"pass\""));
    }

    #[test]
    fn models_and_integrations_accept_bare_and_list_commands() {
        let cli = Cli::try_parse_from(["prx", "models"]).expect("bare models should parse");
        match cli.command {
            Commands::Models { model_command } => assert!(model_command.is_none()),
            other => panic!("expected bare models command, got {other:?}"),
        }

        let cli =
            Cli::try_parse_from(["prx", "models", "list", "--provider", "openai"]).expect("models list should parse");
        match cli.command {
            Commands::Models {
                model_command: Some(ModelCommands::List { provider, .. }),
            } => assert_eq!(provider.as_deref(), Some("openai")),
            other => panic!("expected models list command, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["prx", "integrations"]).expect("bare integrations should parse");
        match cli.command {
            Commands::Integrations { integration_command } => assert!(integration_command.is_none()),
            other => panic!("expected bare integrations command, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["prx", "integrations", "list"]).expect("integrations list should parse");
        match cli.command {
            Commands::Integrations {
                integration_command: Some(IntegrationCommands::List),
            } => {}
            other => panic!("expected integrations list command, got {other:?}"),
        }
    }

    #[test]
    fn migrate_plan_uses_the_library_owned_dto() {
        let cli = Cli::try_parse_from(["prx", "migrate", "plan", "--target-version", "7"])
            .expect("migrate plan should parse");
        match cli.command {
            Commands::Migrate {
                migrate_command: MigrateCommands::Plan { target_version },
            } => assert_eq!(target_version, "7"),
            other => panic!("expected migrate plan command, got {other:?}"),
        }
    }

    #[test]
    fn xin_task_and_goal_step_commands_parse() {
        let cli = Cli::try_parse_from([
            "prx",
            "xin",
            "add",
            "health-watch",
            "inspect health",
            "--recurring",
            "--interval-secs",
            "300",
        ])
        .expect("xin add should parse");
        assert!(matches!(
            cli.command,
            Commands::Xin {
                xin_command: XinCommands::Add {
                    recurring: true,
                    interval_secs: 300,
                    ..
                }
            }
        ));

        let cli = Cli::try_parse_from([
            "prx",
            "xin",
            "steps",
            "add",
            "goal-1",
            "verify",
            "verify artifacts",
            "--sequence",
            "2",
        ])
        .expect("xin steps add should parse");
        assert!(matches!(
            cli.command,
            Commands::Xin {
                xin_command: XinCommands::Steps {
                    step_command: openprx::XinStepCommands::Add { sequence: Some(2), .. }
                }
            }
        ));
    }

    #[test]
    fn completion_generation_mentions_binary_name() -> Result<()> {
        const SHELLS: [CompletionShell; 5] = [
            CompletionShell::Bash,
            CompletionShell::Fish,
            CompletionShell::Zsh,
            CompletionShell::PowerShell,
            CompletionShell::Elvish,
        ];
        for shell in SHELLS {
            let mut output = Vec::new();
            write_shell_completion(shell, &mut output)
                .with_context(|| format!("test: generate {shell:?} completion for the complete CLI tree"))?;
            let script = String::from_utf8(output).context("test: completion output must be UTF-8")?;
            assert!(script.contains("prx"), "{shell:?} completion lost the binary name");
        }
        Ok(())
    }
}
