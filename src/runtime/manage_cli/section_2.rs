pub(crate) async fn run_config(command: ConfigCommands, config: Config, io: &mut Io<'_>) -> Result<()> {
    match command {
        ConfigCommands::Autonomy { level, apply } => {
            let level = serde_json::from_value(serde_json::Value::String(level))?;
            let service = service(config);
            let plan = service.plan_autonomy(level, ConfigActor::Cli).await?;
            finish(&service, plan, &apply, io).await?;
            writeln!(io.out, "Restart running chats and daemons to use the selected autonomy level.")?;
            Ok(())
        }
        ConfigCommands::BraveKey { from_stdin: _, apply } => {
            // At Full the confirmation flag is accepted but not required.
            let at_full = config.autonomy.level == openprx::security::AutonomyLevel::Full;
            let service = service(config);
            let revision = service.revision().unwrap_or_else(|| "none".to_owned());
            if !apply.yes || (!at_full && apply.confirm_high_risk.is_none()) {
                if at_full {
                    writeln!(
                        io.out,
                        "Dry run: stdin was not read. Set web_search.brave_api_key with --from-stdin --yes"
                    )?;
                } else {
                    writeln!(
                        io.out,
                        "Dry run: stdin was not read. Set web_search.brave_api_key with --from-stdin --yes --confirm-high-risk {revision}"
                    )?;
                }
                return Ok(());
            }
            if apply
                .confirm_high_risk
                .as_deref()
                .is_some_and(|confirmed| confirmed != revision)
            {
                bail!("the configuration revision changed; rerun the dry run before supplying a credential");
            }
            let mut value = String::new();
            (&mut *io.stdin)
                .take(16_385)
                .read_to_string(&mut value)
                .context("failed to read the Brave credential")?;
            if value.len() > 16_384 {
                bail!("the Brave credential exceeds 16384 bytes; nothing was stored");
            }
            let secret = openprx::config::manage::credentials::SecretString::new(
                value.trim_end_matches(['\r', '\n']).to_owned(),
            );
            let plan = service.plan_brave_credential(&secret, ConfigActor::Cli).await?;
            finish(&service, plan, &apply, io).await
        }
        ConfigCommands::Plan {
            file,
            target,
            allow_credential_clear,
        } => {
            let (contents, scan) = read_posted_file(&file)?;
            let plan = service(config)
                .plan_file(target, contents, allow_credential_clear, ConfigActor::Cli)
                .await
                .map_err(|error| anyhow::anyhow!("{}", scan.scrub(&error.to_string())))?;
            write!(io.out, "{}", plan.text())?;
            writeln!(io.out, "Revision: {}", plan.revision().unwrap_or("none"))?;
            Ok(())
        }
        ConfigCommands::Apply {
            file,
            target,
            allow_credential_clear,
            apply,
        } => {
            let (contents, scan) = read_posted_file(&file)?;
            let service = service(config);
            let plan = service
                .plan_file(target, contents, allow_credential_clear, ConfigActor::Cli)
                .await
                .map_err(|error| anyhow::anyhow!("{}", scan.scrub(&error.to_string())))?;
            finish(&service, plan, &apply, io)
                .await
                .map_err(|error| anyhow::anyhow!("{}", scan.scrub(&format!("{error:#}"))))
        }
        ConfigCommands::History { history_command } => {
            run_history(history_command.unwrap_or(HistoryCommands::List), config, io).await
        }
        ConfigCommands::Rollback {
            id,
            last,
            yes,
            restore_credentials,
            disable_encryption,
        } => {
            let service = service(config);
            let history = service.transactions().history()?;
            let record = if last {
                history
                    .list()?
                    .into_iter()
                    .find(|record| {
                        record.is_restorable()
                            && matches!(
                                record.status,
                                RecordStatus::Activated
                                    | RecordStatus::ActivationDeferred
                                    | RecordStatus::Pending
                                    | RecordStatus::NotRestored
                            )
                    })
                    .context("no recorded change to roll back")?
            } else {
                let id = id.context("name the history record to roll back, or pass --last")?;
                history.get(id.trim())?
            };
            writeln!(
                io.out,
                "Rolling back to before record {} ({}).",
                record.id,
                record.status.as_str()
            )?;
            writeln!(io.out, "The recorded change that is undone:")?;
            write!(io.out, "{}", render_plan(&record.summary, &[]))?;
            let revision = match service.revision() {
                Some(revision) => revision,
                None => config::files::current_config_revision(&service.config().config_path)?,
            };
            // At Full the transaction asks for no confirmation; `--yes` still
            // gates the CLI write.
            if !yes && service.config().autonomy.level == openprx::security::AutonomyLevel::Full {
                writeln!(io.out, "Dry run: nothing was written. Roll back with --yes")?;
                return Ok(());
            }
            let outcome = service
                .transactions()
                .rollback(RollbackRequest {
                    record_id: &record.id,
                    expected_revision: &revision,
                    confirm: yes,
                    restore_credentials,
                    disable_encryption,
                })
                .await;
            match outcome {
                Ok(outcome) => {
                    writeln!(
                        io.out,
                        "Rolled back on disk (history record {}); activation deferred to the daemon.",
                        outcome
                            .record_id
                            .as_deref()
                            .unwrap_or("none: the files already held it")
                    )?;
                    Ok(())
                }
                Err(ConfigTransactionError::ConfirmationRequired {
                    required: "confirm", ..
                }) => {
                    writeln!(io.out, "Dry run: nothing was written. Roll back with --yes")?;
                    Ok(())
                }
                Err(ConfigTransactionError::ConfirmationRequired { required, .. }) => bail!(
                    "the rollback needs --{}: {}",
                    required.replace('_', "-"),
                    "it restores a credential or setting the current configuration no longer holds; nothing was written"
                ),
                Err(error) => bail!("{error}"),
            }
        }
        ConfigCommands::Status => {
            let service = service(config);
            let desired = service.config();
            writeln!(io.out, "Configuration: {}", desired.config_path.display())?;
            let revision = if desired.config_path.exists() {
                config::files::current_config_revision(&desired.config_path)?
            } else {
                "none (no configuration file)".to_string()
            };
            writeln!(io.out, "Revision: {revision}")?;
            match providers::selection::default_status(&desired) {
                Ok((_, line)) => writeln!(io.out, "Default: {line}")?,
                Err(refusal) => writeln!(io.out, "Default: refused: {refusal}")?,
            }
            let records = service.transactions().history()?.list()?;
            let pending = records
                .iter()
                .filter(|record| matches!(record.status, RecordStatus::Pending | RecordStatus::NotRestored))
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>();
            writeln!(io.out, "History: {} records", records.len())?;
            let deferred = records
                .iter()
                .filter(|record| record.status == RecordStatus::ActivationDeferred)
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>();
            if !deferred.is_empty() {
                writeln!(
                    io.out,
                    "Written by `prx`, activation deferred to the daemon (not confirmed here): {}",
                    deferred.join(", ")
                )?;
            }
            let superseded = records
                .iter()
                .filter(|record| record.status == RecordStatus::Superseded)
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>();
            if !superseded.is_empty() {
                writeln!(
                    io.out,
                    "Written by `prx` and replaced by a later write before a running PRX settled them: {}",
                    superseded.join(", ")
                )?;
            }
            if !pending.is_empty() {
                writeln!(
                    io.out,
                    "Unfinished changes (the process stopped, or a restore failed): {}. Inspect with `prx config \
                     history show <id>`; `prx config rollback <id>` restores the configuration from before it.",
                    pending.join(", ")
                )?;
            }
            Ok(())
        }
        ConfigCommands::Agents { agents_command } => match agents_command {
            AgentsCommands::New {
                agent,
                provider,
                model,
                apply,
            } => {
                let plan =
                    openprx::identity::roles::NewRolePlan::prepare(service(config), &agent, &provider, &model).await?;
                writeln!(io.out, "{}", plan.text())?;
                if !apply.yes {
                    writeln!(
                        io.out,
                        "Dry run: nothing was written. Apply with --yes; if required, use --confirm-high-risk {}",
                        plan.revision().unwrap_or("none")
                    )?;
                    return Ok(());
                }
                let outcome = plan.apply(apply.confirm_high_risk.as_deref()).await?;
                print_applied(&outcome, io)
            }
            AgentsCommands::List => {
                let mut agents = config.agents.iter().collect::<Vec<_>>();
                agents.sort_by(|left, right| left.0.cmp(right.0));
                if agents.is_empty() {
                    writeln!(io.out, "No delegate agents.")?;
                }
                for (name, agent) in agents {
                    writeln!(
                        io.out,
                        "{name}  {}  {}",
                        manage::redact_provider_text(&agent.provider),
                        agent.model
                    )?;
                }
                Ok(())
            }
            AgentsCommands::Set {
                agent,
                provider,
                model,
                apply,
            } => {
                change(
                    &service(config),
                    ManagedChange::BindAgent { agent, provider, model },
                    &apply,
                    io,
                )
                .await
            }
        },
        ConfigCommands::Show { .. } | ConfigCommands::Schema | ConfigCommands::Split { .. } | ConfigCommands::Merge => {
            bail!("BUG: `prx config show|schema|split|merge` is handled by the dispatcher")
        }
    }
}

async fn run_history(command: HistoryCommands, config: Config, io: &mut Io<'_>) -> Result<()> {
    let service = service(config);
    match command {
        HistoryCommands::List => {
            let records = service.transactions().history()?.list()?;
            if records.is_empty() {
                writeln!(io.out, "No recorded changes.")?;
            }
            for record in records {
                writeln!(
                    io.out,
                    "{}  {}  {:?}  {:?}  {} changes, risk {:?}",
                    record.id,
                    record.created_at.to_rfc3339(),
                    record.actor,
                    record.status,
                    record.summary.changes.len(),
                    record.summary.risk
                )?;
            }
        }
        HistoryCommands::Show { id } => {
            let record = service.transactions().history()?.get(id.trim())?;
            writeln!(io.out, "Record: {}", record.id)?;
            writeln!(io.out, "Created: {}", record.created_at.to_rfc3339())?;
            writeln!(io.out, "Actor: {:?}", record.actor)?;
            writeln!(io.out, "Status: {:?}", record.status)?;
            if let Some(restores) = &record.restores {
                writeln!(io.out, "Restores: before record {restores}")?;
            }
            if let Some(reason) = &record.reason {
                writeln!(io.out, "Reason: {reason}")?;
            }
            write!(io.out, "{}", render_plan(&record.summary, &[]))?;
        }
        HistoryCommands::Purge { yes } => {
            if !yes {
                bail!("purging removes every history record and cannot be undone; confirm with --yes");
            }
            let removed = service.transactions().purge_history().await?;
            writeln!(io.out, "Removed {removed} history records.")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Commands};
    use clap::{CommandFactory, Parser};
    use openprx::config::manage::ManagementService;

    /// A credential-shaped sentinel. Not a real secret.
    const SECRET: &str = "pmme-cli-sentinel-value";

    #[test]
    fn deprecated_model_warning_is_visible_in_cli_and_json_surfaces() {
        let entry = manage::ModelEntry {
            provider: "openai".to_owned(),
            identity: "openai".to_owned(),
            model: "gpt-legacy".to_owned(),
            operations: std::collections::BTreeMap::from([("chat".to_owned(), "supported".to_owned())]),
            features: std::collections::BTreeMap::new(),
            context_window: None,
            source: "release catalog".to_owned(),
            fetched_at_unix: None,
            lifecycle_warning: Some("deprecated; use openai/gpt-current".to_owned()),
        };
        let mut listed = Vec::new();
        print_model_line(&entry, &mut listed).expect("test: list line");
        let listed = String::from_utf8(listed).expect("test: utf8 list line");
        assert!(listed.contains("deprecated; use openai/gpt-current"));
        let mut shown = Vec::new();
        print_model(&entry, &mut shown).expect("test: model detail");
        let shown = String::from_utf8(shown).expect("test: utf8 model detail");
        assert!(shown.contains("Lifecycle: deprecated; use openai/gpt-current"));
        let gateway_json = serde_json::to_value(&entry).expect("test: gateway JSON");
        assert_eq!(
            gateway_json
                .get("lifecycle_warning")
                .and_then(serde_json::Value::as_str),
            Some("deprecated; use openai/gpt-current")
        );
    }

    async fn fixture(edit: impl FnOnce(&mut Config)) -> (tempfile::TempDir, Config) {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let mut config = Config {
            config_path: dir.path().join("config.toml"),
            workspace_dir: dir.path().join("workspace"),
            default_provider: Some("openai".into()),
            default_model: Some("gpt-4o".into()),
            ..Config::default()
        };
        // The confirmation tests exercise the rule below Full (at Full none is demanded).
        config.autonomy.level = openprx::security::AutonomyLevel::Supervised;
        edit(&mut config);
        config.save().await.expect("test: save");
        let loaded = reload(&dir).await;
        (dir, loaded)
    }

    /// What the next `prx` invocation loads.
    async fn reload(dir: &tempfile::TempDir) -> Config {
        Config::load_or_init_with_config_dir(dir.path().to_str())
            .await
            .expect("test: load")
    }

    fn manage_command(args: &[&str]) -> ManageCommand {
        let cli = Cli::try_parse_from(std::iter::once("prx").chain(args.iter().copied()))
            .unwrap_or_else(|error| panic!("test: parse {args:?}: {error}"));
        match cli.command {
            Commands::Providers { providers_command } => ManageCommand::Providers(providers_command),
            Commands::Routes { routes_command } => ManageCommand::Routes(routes_command),
            Commands::Models { model_command } => ManageCommand::Models(model_command.unwrap_or(ModelCommands::List {
                provider: None,
                operation: None,
                executable: false,
            })),
            Commands::Config { config_command } => ManageCommand::Config(config_command),
            Commands::DecisionEngine {
                command: openprx::decision_engine_cli::DecisionEngineCommands::Config { command },
                ..
            } => ManageCommand::DecisionEngine(command),
            other => panic!("test: not a management command: {other:?}"),
        }
    }

    /// Run `prx <args>` against `config` with `stdin` and `env`; returns the
    /// result and everything written to stdout.
    async fn run_cli(args: &[&str], config: Config, stdin: &str, env: &[(&str, &str)]) -> (Result<()>, String) {
        let mut out = Vec::new();
        let mut input = stdin.as_bytes();
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        };
        let prompt = |_: &str| -> Result<String> { bail!("test: no terminal") };
        let result = {
            let mut io = Io {
                out: &mut out,
                stdin: &mut input,
                env: &lookup,
                prompt: &prompt,
            };
            Box::pin(run(manage_command(args), config, &mut io)).await
        };
        (result, String::from_utf8(out).expect("test: utf-8"))
    }

    fn stored(dir: &tempfile::TempDir) -> Vec<u8> {
        std::fs::read(dir.path().join("config.toml")).expect("test: read config.toml")
    }

    /// Standard input that records whether anything read it.
    struct Watched<'a> {
        bytes: &'a [u8],
        read: &'a std::cell::Cell<bool>,
    }

    impl std::io::Read for Watched<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.read.set(true);
            self.bytes.read(buf)
        }
    }

    #[tokio::test]
    async fn brave_stdin_intake_waits_for_confirmation_and_never_echoes_the_value() {
        let (dir, config) = fixture(|_| {}).await;
        let original = stored(&dir);
        let read = std::cell::Cell::new(false);
        let mut stdin = Watched {
            bytes: SECRET.as_bytes(),
            read: &read,
        };
        let mut out = Vec::new();
        let mut io = Io {
            out: &mut out,
            stdin: &mut stdin,
            env: &|_| None,
            prompt: &|_| bail!("no prompt"),
        };
        run(
            manage_command(&["config", "brave-key", "--from-stdin"]),
            config.clone(),
            &mut io,
        )
        .await
        .unwrap();
        assert!(!read.get());
        assert_eq!(stored(&dir), original);
        let revision = service(config.clone()).revision().unwrap();
        let (result, output) = run_cli(
            &[
                "config",
                "brave-key",
                "--from-stdin",
                "--yes",
                "--confirm-high-risk",
                &revision,
            ],
            config,
            &format!("{SECRET}\n"),
            &[],
        )
        .await;
        result.unwrap();
        assert!(!output.contains(SECRET));
        assert!(!String::from_utf8(stored(&dir)).unwrap().contains(SECRET));
        assert_eq!(reload(&dir).await.web_search.brave_api_key.as_deref(), Some(SECRET));
    }

    #[tokio::test]
    async fn fallback_model_flag_updates_the_default_chain_without_duplicate_provider_membership() {
        let (dir, config) = fixture(|_| {}).await;
        let revision = service(config.clone()).revision().unwrap();
        let original = stored(&dir);
        let (result, plan) = run_cli(
            &["providers", "add", "openai", "--fallback", "--model", "gpt-5.6"],
            config.clone(),
            "",
            &[],
        )
        .await;
        result.unwrap();
        assert!(plan.contains("model_fallbacks"));
        assert_eq!(stored(&dir), original);
        let (result, _) = run_cli(
            &[
                "providers",
                "add",
                "openai",
                "--fallback",
                "--model",
                "gpt-5.6",
                "--yes",
                "--confirm-high-risk",
                &revision,
            ],
            config,
            "",
            &[],
        )
        .await;
        result.unwrap();
        let config = reload(&dir).await;
        assert_eq!(
            config.reliability.model_fallbacks.get("gpt-4o").unwrap(),
            &vec!["gpt-5.6".to_owned()]
        );
        assert_eq!(config.reliability.fallback_providers, Vec::<String>::new());
    }

    /// E-1 guard: `prx providers` with no subcommand still lists the
    /// providers.
    #[tokio::test]
    async fn providers_without_a_subcommand_still_lists() {
        let cli = Cli::try_parse_from(["prx", "providers"]).expect("test: parse");
        assert!(matches!(
            cli.command,
            Commands::Providers {
                providers_command: None
            }
        ));
        let (_dir, config) = fixture(|config| config.reliability.fallback_providers = vec!["openrouter".into()]).await;
        let (result, out) = run_cli(&["providers"], config, "", &[]).await;
        result.expect("test: list");
        assert!(out.starts_with("Supported providers ("), "{out}");
        assert!(out.contains("  custom:<URL>   Any OpenAI-compatible endpoint"), "{out}");
        assert!(
            out.contains("Configured providers:\n  openai  [default]  used by: default model"),
            "{out}"
        );
        assert!(
            out.contains("  openrouter  [fallback]  used by: fallback provider 'openrouter'"),
            "{out}"
        );
    }

    /// E-1 guard over the clap tree: no management command takes a secret as
    /// an argument; `credential set` reads it from a prompt, stdin or an
    /// environment variable.
    #[test]
    fn no_management_command_takes_a_secret_argument() {
        fn walk(command: &clap::Command, path: &str, found: &mut Vec<String>) {
            for arg in command.get_arguments() {
                let names = std::iter::once(arg.get_id().as_str().to_string())
                    .chain(arg.get_long().map(str::to_string))
                    .chain(arg.get_short().map(|short| short.to_string()))
                    .collect::<Vec<_>>();
                for name in names {
                    let lowered = name.to_ascii_lowercase();
                    if ["key", "token", "secret", "password"]
                        .iter()
                        .any(|word| lowered.contains(word))
                    {
                        found.push(format!("{path} --{name}"));
                    }
                }
            }
            for sub in command.get_subcommands() {
                walk(sub, &format!("{path} {}", sub.get_name()), found);
            }
        }
        let root = Cli::command();
        let mut found = Vec::new();
        for name in ["providers", "models", "routes", "config"] {
            let command = root
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("test: `prx {name}` exists"));
            walk(command, &format!("prx {name}"), &mut found);
        }
        assert!(found.is_empty(), "secret-shaped arguments: {found:?}");

        let set = root
            .find_subcommand("providers")
            .and_then(|providers| providers.find_subcommand("credential"))
            .and_then(|credential| credential.find_subcommand("set"))
            .expect("test: `prx providers credential set` exists");
        let longs = set.get_arguments().filter_map(clap::Arg::get_long).collect::<Vec<_>>();
        assert!(
            longs.contains(&"from-stdin") && longs.contains(&"from-env"),
            "{longs:?}"
        );
        let positionals = set
            .get_arguments()
            .filter(|arg| arg.is_positional())
            .map(|arg| arg.get_id().as_str().to_string())
            .collect::<Vec<_>>();
        assert_eq!(positionals, ["provider"]);
    }

    /// D-101 F-5 ruling: read-only management commands without a
    /// configuration run on the defaults and write nothing.
    #[tokio::test]
    async fn read_only_commands_without_a_config_write_nothing() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_dir = dir.path().join("not-yet");
        let config = Config::load_existing_or_defaults_read_only(config_dir.to_str())
            .await
            .expect("test: defaults");
        for args in [
            &["providers", "list"][..],
            &["providers", "migrate"][..],
            &["config", "status"][..],
            &["routes", "list"][..],
        ] {
            let (result, _) = run_cli(args, config.clone(), "", &[]).await;
            result.unwrap_or_else(|error| panic!("test: {args:?}: {error}"));
            assert!(!config_dir.exists(), "{args:?} created the configuration directory");
        }
    }

    /// RED before PMMH-g (D-104 C-2): `models list/show`, `config agents
    /// list`, `config plan` and `config schema` loaded through the
    /// initializing loader (creating `config.toml` on a new machine, running
    /// the legacy upgrade otherwise). Every command that only reads loads
    /// read-only now; a writing command does not.
    #[test]
    fn every_command_that_only_reads_loads_read_only() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("prx").chain(args.iter().copied()))
                .unwrap_or_else(|error| panic!("test: parse {args:?}: {error}"))
        };
        for args in [
            &["models", "list"][..],
            &["models", "show", "openai", "gpt-4o"][..],
            &["config", "agents", "list"][..],
            &["config", "plan", "--file", "candidate.toml"][..],
            &["config", "schema"][..],
            &["config", "status"][..],
            &["providers", "migrate", "--apply"][..],
            &["routes", "list"][..],
        ] {
            assert!(
                crate::read_only_management_command(&parse(args).command),
                "{args:?} loads through the initializing loader"
            );
        }
        for args in [
            &["providers", "migrate", "--apply", "--yes"][..],
            &["config", "apply", "--file", "candidate.toml", "--yes"][..],
            &["routes", "add", "fast", "openai", "gpt-4o-mini"][..],
        ] {
            assert!(
                !crate::read_only_management_command(&parse(args).command),
                "{args:?} writes"
            );
        }
    }

    /// RED before PMMH-f (D-101 F-2): replacing a credential reference
    /// recorded `activated`, and `config rollback --last` offered to undo it
    /// although the previous value is not kept. The record is
    /// `credential_replaced` now, `--last` skips it, and the output says the
    /// new value takes effect after a reload.
    #[tokio::test]
    async fn a_replaced_credential_is_not_offered_for_rollback() {
        let (dir, config) = fixture(|_| {}).await;
        let (result, _) = run_cli(
            &["providers", "credential", "set", "openai", "--from-stdin"],
            config,
            &format!("{SECRET}\n"),
            &[],
        )
        .await;
        result.expect("test: create");
        let (result, out) = run_cli(
            &["providers", "credential", "set", "openai", "--from-stdin"],
            reload(&dir).await,
            "pmmh-cli-rotated-sentinel\n",
            &[],
        )
        .await;
        result.expect("test: replace");
        assert!(
            out.contains("when the configuration next changes (Gateway) or at the next restart"),
            "{out}"
        );

        let (result, out) = run_cli(&["config", "rollback", "--last"], reload(&dir).await, "", &[]).await;
        let error = result.expect_err("test: nothing restorable to roll back");
        assert!(
            error.to_string().contains("no recorded change to roll back"),
            "{error} {out}"
        );
    }

    /// SP-PMM H-1/H-2 through the CLI: `providers migrate` reports and
    /// writes nothing; `--apply` shows the plan and the revision to confirm;
    /// with `--yes --confirm-high-risk` it writes the migration; a second
    /// run has nothing to migrate. No output carries the key.
    #[tokio::test]
    async fn providers_migrate_reports_plans_and_applies() {
        let (dir, config) = fixture(|config| {
            config.model_routes.push(openprx::config::ModelRouteConfig {
                settings: Default::default(),
                hint: "fast".into(),
                provider: "openai".into(),
                model: "gpt-4o-mini".into(),
                api_key: Some(SECRET.into()),
                credential_ref: None,
            });
        })
        .await;
        let before = std::fs::read(dir.path().join("config.toml")).expect("test: read");

        let (result, out) = run_cli(&["providers", "migrate"], config, "", &[]).await;
        result.expect("test: report");
        assert!(out.contains("model_routes[0].api_key"), "{out}");
        assert!(out.contains("Plan the migration with --apply"), "{out}");
        assert!(!out.contains(SECRET), "{out}");
        assert_eq!(
            std::fs::read(dir.path().join("config.toml")).expect("test: read"),
            before
        );

        let (result, out) = run_cli(&["providers", "migrate", "--apply"], reload(&dir).await, "", &[]).await;
        result.expect("test: plan");
        assert!(!out.contains(SECRET), "{out}");
        let revision = out
            .lines()
            .find_map(|line| line.split("--confirm-high-risk ").nth(1))
            .map(|revision| revision.trim().to_string())
            .unwrap_or_else(|| panic!("test: the plan names the revision to confirm: {out}"));
        assert_eq!(
            std::fs::read(dir.path().join("config.toml")).expect("test: read"),
            before
        );

        let (result, out) = run_cli(
            &[
                "providers",
                "migrate",
                "--apply",
                "--yes",
                "--confirm-high-risk",
                &revision,
            ],
            reload(&dir).await,
            "",
            &[],
        )
        .await;
        result.expect("test: apply");
        assert!(out.contains("Written and validated"), "{out}");
        assert!(out.contains("--restore-credentials"), "{out}");
        assert!(!out.contains(SECRET), "{out}");
        let migrated = reload(&dir).await;
        let route = migrated.model_routes.first().expect("test: the route");
        assert!(route.api_key.is_none());
        assert!(route.credential_ref.is_some());
        assert_eq!(
            migrated
                .model_routes_with_references()
                .first()
                .and_then(|route| route.api_key.as_deref()),
            Some(SECRET)
        );

        let (result, out) = run_cli(&["providers", "migrate", "--apply"], migrated, "", &[]).await;
        result.expect("test: nothing left");
        assert!(out.ends_with("Nothing to migrate.\n"), "{out}");
    }

    #[tokio::test]
    async fn a_credential_is_read_from_stdin_or_env_and_never_printed_or_stored_in_plaintext() {
        let (dir, config) = fixture(|_| {}).await;
        let (result, out) = run_cli(
            &["providers", "credential", "set", "openai", "--from-stdin"],
            config,
            &format!("{SECRET}\n"),
            &[],
        )
        .await;
        result.expect("test: set from stdin");
        assert!(
            out.contains("Stored credential reference cred-openai for provider catalog:openai"),
            "{out}"
        );
        // D-085 F-5, PMM-H: the output says how a configuration uses the
        // reference, and which key PRX sends when nothing names one.
        assert!(out.contains("`credential_ref = \"cred-openai\"`"), "{out}");
        assert!(
            out.contains("PRX sends 'openai' its own auth profile today")
                && out.contains("prx auth paste-token --provider openai --from-stdin"),
            "{out}"
        );
        let (result, out) = run_cli(
            &[
                "providers",
                "credential",
                "set",
                "openai",
                "--name",
                "spare",
                "--from-env",
                "PMME_TEST_KEY",
            ],
            reload(&dir).await,
            "",
            &[("PMME_TEST_KEY", SECRET)],
        )
        .await;
        result.expect("test: set from env");
        assert!(out.contains("cred-spare"), "{out}");
        let store = std::fs::read_to_string(dir.path().join("auth-profiles.json")).expect("test: store");
        assert!(!store.contains(SECRET), "stored in plaintext");

        let (result, out) = run_cli(&["providers", "credential", "status"], reload(&dir).await, "", &[]).await;
        result.expect("test: status");
        assert!(out.contains("cred-openai  provider catalog:openai"), "{out}");
        let (result, out) = run_cli(&["providers", "show", "openai"], reload(&dir).await, "", &[]).await;
        result.expect("test: show");
        assert!(out.contains("  - credential reference cred-spare"), "{out}");
        assert!(out.contains("not referenced"), "{out}");
        assert!(out.contains("In use: PRX sends 'openai'"), "{out}");
        assert!(!out.contains(SECRET), "{out}");

        // A missing environment variable and an empty input are refused.
        let (result, _) = run_cli(
            &["providers", "credential", "set", "openai", "--from-env", "PMME_UNSET"],
            reload(&dir).await,
            "",
            &[],
        )
        .await;
        assert!(
            result
                .expect_err("test: unset")
                .to_string()
                .contains("PMME_UNSET is not set")
        );
        let (result, _) = run_cli(
            &["providers", "credential", "set", "openai", "--from-stdin"],
            reload(&dir).await,
            "\n",
            &[],
        )
        .await;
        assert!(result.is_err());

        let (result, out) = run_cli(
            &["providers", "credential", "remove", "spare"],
            reload(&dir).await,
            "",
            &[],
        )
        .await;
        result.expect("test: remove");
        assert_eq!(out, "Removed credential reference cred-spare.\n");
    }

    /// E-2 (shared with PMM-F-3): the CLI prints exactly the service's plan
    /// text for the same change, and the same refusal; a dry run writes
    /// nothing.
    #[tokio::test]
    async fn the_cli_prints_the_services_plan_and_refusal_text_byte_for_byte() {
        let (dir, config) = fixture(|config| {
            config.agents.insert(
                "coder".into(),
                serde_json::from_value(serde_json::json!({ "provider": "openai", "model": "gpt-4o" }))
                    .expect("test: agent"),
            );
        })
        .await;
        let before = stored(&dir);
        let cases: Vec<(Vec<&str>, ManagedChange)> = vec![
            (
                vec!["routes", "add", "fast", "openai", "gpt-5.6"],
                ManagedChange::AddRoute {
                    operation: SelectionOperation::Chat,
                    hint: "fast".into(),
                    provider: "openai".into(),
                    model: "gpt-5.6".into(),
                },
            ),
            (
                vec!["models", "default", "set", "openai", "gpt-5.6"],
                ManagedChange::SetDefault {
                    settings: None,
                    operation: SelectionOperation::Chat,
                    provider: "openai".into(),
                    model: "gpt-5.6".into(),
                    root_credential: RootCredential::Refuse,
                },
            ),
            (
                vec!["providers", "add", "openrouter", "--fallback"],
                ManagedChange::AddProvider {
                    provider: "openrouter".into(),
                },
            ),
            (
                vec!["config", "agents", "set", "coder", "openai", "gpt-5.6"],
                ManagedChange::BindAgent {
                    agent: "coder".into(),
                    provider: "openai".into(),
                    model: "gpt-5.6".into(),
                },
            ),
            (
                vec![
                    "models",
                    "add",
                    "--provider",
                    "openai",
                    "--model",
                    "gpt-5.6",
                    "--operations",
                    "chat",
                    "--native-tools",
                    "true",
                ],
                ManagedChange::AddModel(config::schema::ProviderModelConfig {
                    provider: "openai".into(),
                    model: Some("gpt-5.6".into()),
                    operations: Some(vec![config::schema::ModelOperationSetting::Chat]),
                    native_tools: Some(true),
                    vision_input: None,
                    streaming: None,
                    structured_output: None,
                    tool_schema_dialect: None,
                    tool_call_dialect: None,
                    context_window: None,
                }),
            ),
        ];
        for (args, change) in cases {
            let service = ManagementService::new(config::new_shared(config.clone()));
            let expected = service
                .plan(&change, ConfigActor::Cli)
                .await
                .unwrap_or_else(|error| panic!("test: service plan {args:?}: {error}"))
                .text();
            let (result, out) = run_cli(&args, config.clone(), "", &[]).await;
            result.unwrap_or_else(|error| panic!("test: cli {args:?}: {error}"));
            assert!(
                out.starts_with(&expected),
                "{args:?}: CLI printed\n{out}\nservice rendered\n{expected}"
            );
            assert_eq!(
                &out[expected.len()..],
                "Dry run: nothing was written. Apply it with --yes\n",
                "{args:?}"
            );
            assert_eq!(stored(&dir), before, "{args:?}: a dry run wrote");
        }

        // The same refusal.
        let service = ManagementService::new(config::new_shared(config.clone()));
        let expected = service
            .plan(
                &ManagedChange::RemoveProvider {
                    provider: "openai".into(),
                },
                ConfigActor::Cli,
            )
            .await
            .expect_err("test: refused")
            .to_string();
        let (result, out) = run_cli(&["providers", "remove", "openai", "--yes"], config, "", &[]).await;
        assert_eq!(result.expect_err("test: refused").to_string(), expected);
        assert!(out.is_empty(), "{out}");
        assert_eq!(stored(&dir), before);
    }

    /// D-085 F-7: `providers add` names its role; without `--fallback` it
    /// does not parse. F-6: `config apply --file` writes only with `--yes`.
    #[tokio::test]
    async fn adding_a_provider_names_its_role_and_a_file_apply_needs_yes() {
        assert!(Cli::try_parse_from(["prx", "providers", "add", "anthropic"]).is_err());
        assert!(Cli::try_parse_from(["prx", "providers", "add", "anthropic", "--fallback"]).is_ok());

        let (dir, config) = fixture(|_| {}).await;
        let before = stored(&dir);
        let posted = dir.path().join("posted.toml");
        let text = String::from_utf8(before.clone()).expect("test: utf-8").replacen(
            "default_temperature = 0.7",
            "default_temperature = 0.3",
            1,
        );
        assert_ne!(text.as_bytes(), before.as_slice(), "test: the edit changes the file");
        std::fs::write(&posted, text).expect("test: write");
        let path = posted.to_str().expect("test: path");
        let (result, out) = run_cli(&["config", "apply", "--file", path], config.clone(), "", &[]).await;
        result.expect("test: dry run");
        assert!(
            out.ends_with("Dry run: nothing was written. Apply it with --yes\n"),
            "{out}"
        );
        assert_eq!(
            stored(&dir),
            before,
            "RED before PMME-f: `config apply` wrote without --yes"
        );
        let (result, out) = run_cli(&["config", "apply", "--file", path, "--yes"], config, "", &[]).await;
        result.expect("test: apply");
        assert!(out.contains("Written and validated"), "{out}");
        assert_ne!(stored(&dir), before);
    }

    #[tokio::test]
    async fn raw_config_secret_removal_requires_the_clear_flag_on_plan_and_apply() {
        for encrypt in [false, true] {
            for target in ["config.toml", "tools.toml"] {
                let (dir, mut config) = fixture(|config| {
                    config.secrets.encrypt = encrypt;
                    config.web_search.brave_api_key = Some(SECRET.into());
                })
                .await;
                if target == "tools.toml" {
                    config::files::write_split_config(&config, false).await.unwrap();
                    config = reload(&dir).await;
                }
                let target_path = if target == "config.toml" {
                    dir.path().join(target)
                } else {
                    dir.path().join("config.d").join(target)
                };
                let original = std::fs::read_to_string(&target_path).unwrap();
                let before: toml::Value = toml::from_str(&original).unwrap();
                let mut omitted = before.clone();
                omitted
                    .get_mut("web_search")
                    .unwrap()
                    .as_table_mut()
                    .unwrap()
                    .remove("brave_api_key");
                let file = dir.path().join("posted.toml");
                let path = file.to_str().unwrap();
                let mut empty = before.clone();
                *empty.get_mut("web_search").unwrap().get_mut("brave_api_key").unwrap() =
                    toml::Value::String(String::new());
                for posted in [&omitted, &empty] {
                    std::fs::write(&file, toml::to_string(posted).unwrap()).unwrap();
                    for command in ["plan", "apply"] {
                        let mut args = vec!["config", command, "--file", path, "--target", target];
                        if command == "apply" {
                            args.push("--yes");
                        }
                        let (result, out) = run_cli(&args, config.clone(), "", &[]).await;
                        let error = result.unwrap_err().to_string();
                        for expected in [
                            "web_search.brave_api_key",
                            "PUT /api/config/secrets",
                            "--allow-credential-clear",
                        ] {
                            assert!(error.contains(expected), "{error}");
                        }
                        assert!(!error.contains(SECRET), "{error}");
                        assert!(!out.contains(SECRET), "{out}");
                        assert_eq!(std::fs::read_to_string(&target_path).unwrap(), original);
                        assert_eq!(reload(&dir).await.web_search.brave_api_key.as_deref(), Some(SECRET));
                    }
                }
                // Exact stored ciphertext/plaintext is preserved without a clear flag.
                std::fs::write(&file, &original).unwrap();
                let (result, _) = run_cli(
                    &["config", "plan", "--file", path, "--target", target],
                    config.clone(),
                    "",
                    &[],
                )
                .await;
                result.unwrap();
                // Even explicit clear permission cannot turn a masked read into a write.
                let mut masked = before.clone();
                *masked.get_mut("web_search").unwrap().get_mut("brave_api_key").unwrap() =
                    toml::Value::String("***".into());
                std::fs::write(&file, toml::to_string(&masked).unwrap()).unwrap();
                let (result, _) = run_cli(
                    &[
                        "config",
                        "apply",
                        "--file",
                        path,
                        "--target",
                        target,
                        "--allow-credential-clear",
                        "--yes",
                    ],
                    config.clone(),
                    "",
                    &[],
                )
                .await;
                assert!(result.unwrap_err().to_string().contains("redaction mask"));
                assert_eq!(std::fs::read_to_string(&target_path).unwrap(), original);
                if encrypt {
                    *masked.get_mut("web_search").unwrap().get_mut("brave_api_key").unwrap() =
                        toml::Value::String("prx-new-plaintext-fixture".into());
                    std::fs::write(&file, toml::to_string(&masked).unwrap()).unwrap();
                    let (result, _) = run_cli(
                        &[
                            "config",
                            "apply",
                            "--file",
                            path,
                            "--target",
                            target,
                            "--allow-credential-clear",
                            "--yes",
                        ],
                        config.clone(),
                        "",
                        &[],
                    )
                    .await;
                    assert!(result.unwrap_err().to_string().contains("plaintext credentials"));
                    assert_eq!(std::fs::read_to_string(&target_path).unwrap(), original);
                }
                std::fs::write(&file, toml::to_string(&omitted).unwrap()).unwrap();
                let (result, out) = run_cli(
                    &[
                        "config",
                        "plan",
                        "--file",
                        path,
                        "--target",
                        target,
                        "--allow-credential-clear",
                    ],
                    config.clone(),
                    "",
                    &[],
                )
                .await;
                result.unwrap();
                assert!(!out.contains(SECRET), "{out}");
                assert_eq!(std::fs::read_to_string(&target_path).unwrap(), original);
                let revision = out
                    .lines()
                    .find_map(|line| line.strip_prefix("Revision: "))
                    .unwrap()
                    .to_owned();
                let (result, _) = run_cli(
                    &[
                        "config",
                        "apply",
                        "--file",
                        path,
                        "--target",
                        target,
                        "--allow-credential-clear",
                        "--yes",
                    ],
                    config.clone(),
                    "",
                    &[],
                )
                .await;
                assert!(result.unwrap_err().to_string().contains("confirm-high-risk"));
                assert_eq!(std::fs::read_to_string(&target_path).unwrap(), original);
                let (result, out) = run_cli(
                    &[
                        "config",
                        "apply",
                        "--file",
                        path,
                        "--target",
                        target,
                        "--allow-credential-clear",
                        "--yes",
                        "--confirm-high-risk",
                        &revision,
                    ],
                    config,
                    "",
                    &[],
                )
                .await;
                result.unwrap();
                assert!(out.contains("Written and validated"), "{out}");
                assert!(!out.contains(SECRET), "{out}");
                assert!(reload(&dir).await.web_search.brave_api_key.is_none());
            }
        }
    }

    #[tokio::test]
    async fn raw_secret_clear_permission_does_not_confirm_access_changes() {
        let (dir, config) = fixture(|config| config.gateway.paired_tokens = vec![SECRET.into()]).await;
        let before = stored(&dir);
        let mut posted: toml::Value = toml::from_str(std::str::from_utf8(&before).unwrap()).unwrap();
        *posted.get_mut("gateway").unwrap().get_mut("paired_tokens").unwrap() = toml::Value::Array(Vec::new());
        let file = dir.path().join("posted.toml");
        std::fs::write(&file, toml::to_string(&posted).unwrap()).unwrap();
        let (result, out) = run_cli(
            &[
                "config",
                "apply",
                "--file",
                file.to_str().unwrap(),
                "--allow-credential-clear",
                "--yes",
            ],
            config,
            "",
            &[],
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("confirm-high-risk"));
        assert!(!out.contains(SECRET), "{out}");
        assert_eq!(stored(&dir), before);
    }

    #[tokio::test]
    async fn config_apply_names_the_shadowing_fragment_and_target_repairs_it() {
        let (dir, config) = fixture(|_| {}).await;
        let before = stored(&dir);
        let fragments = dir.path().join("config.d");
        std::fs::create_dir_all(&fragments).unwrap();
        let fragment = fragments.join("tools.toml");
        let original_fragment = "[web_search]\nprovider = 'duckduckgo'\n";
        std::fs::write(&fragment, original_fragment).unwrap();
        let mut posted: toml::Value = toml::from_str(std::str::from_utf8(&before).unwrap()).unwrap();
        *posted.get_mut("web_search").unwrap().get_mut("provider").unwrap() = toml::Value::String("brave".into());
        let file = dir.path().join("posted.toml");
        std::fs::write(&file, toml::to_string(&posted).unwrap()).unwrap();
        let path = file.to_str().unwrap();
        for args in [
            vec!["config", "plan", "--file", path],
            vec!["config", "apply", "--file", path, "--yes"],
        ] {
            let (result, out) = run_cli(&args, config.clone(), "", &[]).await;
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("web_search.provider is shadowed by config.d/tools.toml; use --target tools.toml"),
                "{error}"
            );
            assert!(!out.contains("no change"), "{out}");
            assert_eq!(stored(&dir), before);
            assert_eq!(std::fs::read_to_string(&fragment).unwrap(), original_fragment);
        }
        // A valid unrelated edit must not hide a shadowed field or be partly applied.
        *posted.get_mut("default_temperature").unwrap() = toml::Value::Float(0.4);
        std::fs::write(&file, toml::to_string(&posted).unwrap()).unwrap();
        let (result, _) = run_cli(&["config", "apply", "--file", path, "--yes"], config.clone(), "", &[]).await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("shadowed by config.d/tools.toml")
        );
        assert_eq!(stored(&dir), before);
        assert_eq!(std::fs::read_to_string(&fragment).unwrap(), original_fragment);
        // Posting an unchanged base is still legal even though the fragment wins.
        std::fs::write(&file, &before).unwrap();
        let (result, _) = run_cli(&["config", "plan", "--file", path], config.clone(), "", &[]).await;
        result.unwrap();
        // Follow the exact guidance: the target is a basename and other files stay intact.
        std::fs::write(&file, "[web_search]\nprovider = 'brave'\n").unwrap();
        let (result, out) = run_cli(
            &["config", "apply", "--file", path, "--target", "tools.toml", "--yes"],
            config,
            "",
            &[],
        )
        .await;
        result.unwrap();
        assert!(out.contains("Written and validated"), "{out}");
        assert_eq!(stored(&dir), before);
        assert_eq!(reload(&dir).await.web_search.provider, "brave");
    }

    #[tokio::test]
    async fn stored_deleted_model_settings_can_be_inspected_without_selecting_them() {
        let (dir, config) = fixture(|config| {
            config.model_settings.insert(
                "openai/prx-deleted-model".into(),
                config::model_settings::ModelSettings {
                    level: Some("high".into()),
                    ..Default::default()
                },
            );
        })
        .await;
        let before = stored(&dir);
        let (result, text) = run_cli(&["models", "settings", "openai/prx-deleted-model"], config, "", &[]).await;
        result.unwrap();
        let settings: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(settings.get("level").and_then(serde_json::Value::as_str), Some("high"));
        assert_eq!(stored(&dir), before);
    }

    #[tokio::test]
    async fn catalog_lifecycle_status_and_migration_use_the_cli_transaction() {
        let (dir, config) = fixture(|config| {
            config.default_provider = Some("openai".into());
            config.default_model = Some("gpt-4o".into());
        })
        .await;
        let before = stored(&dir);
        let (result, text) = run_cli(&["models", "catalog", "status"], config.clone(), "", &[]).await;
        result.unwrap();
        let status: serde_json::Value = serde_json::from_str(&text).unwrap();
        let release = providers::catalog::ReleaseCatalog::embedded_ref().unwrap();
        assert_eq!(
            status.get("revision").and_then(serde_json::Value::as_str),
            Some(release.revision.as_str())
        );
        assert_eq!(
            status.get("models").and_then(serde_json::Value::as_u64),
            Some(u64::try_from(release.models.len()).unwrap())
        );
        assert_eq!(
            status.get("content_sha256").and_then(serde_json::Value::as_str),
            Some(release.content_sha256())
        );
        assert_eq!(
            status.get("updates").and_then(serde_json::Value::as_str),
            Some("releases only")
        );
        let (result, text) = run_cli(&["models", "migrate-retired"], config.clone(), "", &[]).await;
        result.unwrap();
        assert!(text.contains("gpt-5.6-sol"), "{text}");
        assert_eq!(stored(&dir), before);
        let (result, text) = run_cli(&["models", "migrate-retired", "--yes"], config, "", &[]).await;
        result.unwrap();
        assert!(text.contains("history record"), "{text}");
        assert_ne!(stored(&dir), before);
        assert!(String::from_utf8(stored(&dir)).unwrap().contains("gpt-5.6-sol"));
    }

    /// D-085 F-6: `prx models list --operation` and `prx models show` print
    /// the service's model read model.
    #[tokio::test]
    async fn models_list_by_operation_and_show_use_the_service_read_model() {
        let (_dir, config) = fixture(|config| {
            config.providers.models.push(
                toml::from_str(
                    "provider = \"openrouter\"\nmodel = \"jev-decision-test\"\noperations = [\"decision_endpoint\"]\n",
                )
                .expect("test: declaration"),
            );
            config.providers.models.push(
                toml::from_str("provider = \"openai\"\nmodel = \"gpt-4o\"\noperations = [\"chat\"]\n")
                    .expect("test: declaration"),
            );
        })
        .await;
        let (result, out) = run_cli(
            &["models", "list", "--operation", "chat", "--executable"],
            config.clone(),
            "",
            &[],
        )
        .await;
        result.expect("test: list");
        assert!(out.contains("openai  gpt-4o  [chat]  (operator declaration)"), "{out}");
        assert!(
            out.contains("openai  gpt-5.6  [chat]  (embedded release catalog)"),
            "the real CLI listing must include release-catalog models: {out}"
        );
        assert!(!out.contains("jev-decision-test"), "{out}");
        let (result, out) = run_cli(&["models", "show", "openrouter", "jev-decision-test"], config, "", &[]).await;
        result.expect("test: show");
        assert!(out.contains("Evidence: operator declaration\n"), "{out}");
        assert!(out.contains("  chat: unsupported"), "{out}");
    }

    /// RED before PMME-g (D-088 C-6): `routes list` and `config agents list`
    /// redacted only credential-named query parameters, so `?sig=` came
    /// back verbatim.
    #[tokio::test]
    async fn listings_redact_every_url_query_value() {
        const PROVIDER: &str = "custom:https://llm.example/v1?sig=sentinel-listing-sig";
        let (_dir, config) = fixture(|config| {
            config.model_routes = vec![config::ModelRouteConfig {
                settings: Default::default(),
                hint: "fast".into(),
                provider: PROVIDER.into(),
                model: "m".into(),
                api_key: None,
                credential_ref: None,
            }];
            config.agents.insert(
                "coder".into(),
                serde_json::from_value(serde_json::json!({ "provider": PROVIDER, "model": "m" })).expect("test: agent"),
            );
        })
        .await;
        for args in [&["routes", "list"][..], &["config", "agents", "list"][..]] {
            let (result, out) = run_cli(args, config.clone(), "", &[]).await;
            result.expect("test: list");
            assert!(
                out.contains("custom:https://llm.example/v1?sig=[redacted]"),
                "{args:?}: {out}"
            );
            assert!(!out.contains("sentinel-listing-sig"), "{args:?}: {out}");
        }
    }

    /// E-3: `prx config rollback --last` restores the before-image.
    #[tokio::test]
    async fn rollback_last_restores_the_before_image() {
        let (dir, config) = fixture(|_| {}).await;
        let before = stored(&dir);
        let (result, out) = run_cli(
            &["routes", "add", "fast", "openai", "gpt-5.6", "--yes"],
            config,
            "",
            &[],
        )
        .await;
        result.expect("test: apply");
        assert!(out.contains("Written and validated (history record "), "{out}");
        assert!(out.contains("activation deferred to the daemon"), "{out}");
        assert!(!out.contains("Applied"), "{out}");
        assert_ne!(stored(&dir), before);

        let (result, out) = run_cli(&["config", "history"], reload(&dir).await, "", &[]).await;
        result.expect("test: history");
        assert_eq!(out.lines().count(), 1, "{out}");
        // RED before PMME-f (D-085 F-4): recorded `Activated`, although the
        // CLI cannot confirm what a running daemon does with the change.
        assert!(out.contains("Cli  ActivationDeferred"), "{out}");

        // Without --yes nothing is written.
        let changed = stored(&dir);
        let (result, out) = run_cli(&["config", "rollback", "--last"], reload(&dir).await, "", &[]).await;
        result.expect("test: dry rollback");
        assert!(out.contains("  + model_routes[0].hint = \"fast\"\n"), "{out}");
        assert!(
            out.ends_with("Dry run: nothing was written. Roll back with --yes\n"),
            "{out}"
        );
        assert_eq!(stored(&dir), changed);

        let (result, out) = run_cli(&["config", "rollback", "--last", "--yes"], reload(&dir).await, "", &[]).await;
        result.expect("test: rollback");
        assert!(out.contains("Rolled back on disk (history record "), "{out}");
        assert_eq!(stored(&dir), before, "the before-image is restored byte for byte");

        let (result, out) = run_cli(&["config", "status"], reload(&dir).await, "", &[]).await;
        result.expect("test: status");
        assert!(out.contains("History: 2 records"), "{out}");
        assert!(
            out.contains("activation deferred to the daemon (not confirmed here)"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn a_confirmed_history_rollback_retains_the_existing_retired_default() {
        let (dir, config) = fixture(|config| {
            config.default_provider = Some("openai".into());
            config.default_model = Some("gpt-4o".into());
        })
        .await;
        let before = stored(&dir);
        let (result, _) = run_cli(
            &["models", "default", "set", "openai", "gpt-5.6", "--yes"],
            config,
            "",
            &[],
        )
        .await;
        result.unwrap();
        assert_ne!(stored(&dir), before);
        let (result, _) = run_cli(&["config", "rollback", "--last", "--yes"], reload(&dir).await, "", &[]).await;
        result.unwrap();
        assert_eq!(stored(&dir), before);
        let restored = reload(&dir).await;
        assert_eq!(restored.default_model.as_deref(), Some("gpt-4o"));
        assert!(
            openprx::config::manage::lifecycle::warnings(&restored)
                .unwrap()
                .iter()
                .any(|warning| warning.contains("retired"))
        );
    }

    /// A high-risk change applies only with a confirmation naming the
    /// revision the plan reported (D-049 B).
    #[tokio::test]
    async fn a_high_risk_change_needs_the_confirmed_revision() {
        let (dir, config) = fixture(|_| {}).await;
        let before = stored(&dir);
        let args = [
            "providers",
            "update",
            "openai",
            "--api-url",
            "https://proxy.example/v1",
            "--yes",
        ];
        let (result, out) = run_cli(&args, config.clone(), "", &[]).await;
        let error = result.expect_err("test: confirmation required").to_string();
        assert!(
            out.contains("High risk because the change:\n  - changes the provider endpoint at api_url"),
            "{out}"
        );
        let revision = error
            .rsplit("--confirm-high-risk ")
            .next()
            .expect("test: the refusal names the revision")
            .trim()
            .to_string();
        assert!(!revision.is_empty(), "{error}");
        assert_eq!(stored(&dir), before);

        let mut wrong = args.to_vec();
        wrong.extend(["--confirm-high-risk", "0000"]);
        let (result, _) = run_cli(&wrong, config.clone(), "", &[]).await;
        assert!(result.is_err());
        assert_eq!(stored(&dir), before);

        let mut confirmed = args.to_vec();
        confirmed.extend(["--confirm-high-risk", revision.as_str()]);
        let (result, out) = run_cli(&confirmed, config, "", &[]).await;
        result.unwrap_or_else(|error| panic!("test: confirmed: {error}"));
        assert!(out.contains("Written and validated (history record "), "{out}");
        assert!(out.contains("activation deferred to the daemon"), "{out}");
        assert!(!out.contains("Applied"), "{out}");
        assert!(
            String::from_utf8_lossy(&stored(&dir)).contains("https://proxy.example/v1"),
            "the endpoint is written"
        );
    }

    /// `prx config plan --file` goes through the same transaction as the raw
    /// editor: a plaintext key at an encrypted slot is refused, and neither
    /// the refusal nor a parse error quotes the posted secret.
    #[tokio::test]
    async fn a_raw_file_plan_never_echoes_a_posted_secret() {
        let (dir, config) = fixture(|_| {}).await;
        let posted = dir.path().join("posted.toml");
        let mut text = String::from_utf8(stored(&dir)).expect("test: utf-8");
        text = text.replacen(
            "default_model = \"gpt-4o\"",
            &format!("api_key = \"{SECRET}\"\ndefault_model = \"gpt-4o\""),
            1,
        );
        std::fs::write(&posted, &text).expect("test: write");
        let path = posted.to_str().expect("test: path");
        let (result, out) = run_cli(&["config", "plan", "--file", path], config.clone(), "", &[]).await;
        let error = result.expect_err("test: plaintext refused").to_string();
        assert!(error.contains("api_key"), "the refusal names the path: {error}");
        assert!(!error.contains(SECRET) && !out.contains(SECRET), "{error}\n{out}");

        std::fs::write(&posted, format!("api_key = \"{SECRET}\n")).expect("test: write");
        let (result, _) = run_cli(&["config", "plan", "--file", path], config, "", &[]).await;
        let error = result.expect_err("test: malformed").to_string();
        assert!(error.starts_with("Invalid TOML content at line 1"), "{error}");
        assert!(!error.contains(SECRET), "{error}");
    }
    #[tokio::test]
    async fn cli_model_settings_plan_inspect_and_atomic_default_selection_use_pmm() {
        let (dir, config) = fixture(|_| {}).await;
        let before = stored(&dir);
        let (result, out) = run_cli(&["models", "levels", "openai/gpt-5.6"], config.clone(), "", &[]).await;
        result.unwrap();
        assert!(out.contains("xhigh"));
        assert_eq!(stored(&dir), before);
        let (result, out) = run_cli(
            &["models", "settings", "openai/gpt-5.6", "--level", "high"],
            config.clone(),
            "",
            &[],
        )
        .await;
        result.unwrap();
        assert!(out.contains("model_settings"));
        assert_eq!(stored(&dir), before);
        let (result, out) = run_cli(
            &[
                "models",
                "default",
                "set",
                "openai",
                "gpt-5.6",
                "--level",
                "high",
                "--set",
                "reasoning_effort=low",
                "--yes",
            ],
            config,
            "",
            &[],
        )
        .await;
        result.unwrap();
        assert!(out.contains("history record"));
        let config = reload(&dir).await;
        assert_eq!(config.default_model.as_deref(), Some("gpt-5.6"));
        let settings = config.model_settings.get("openai/gpt-5.6").unwrap();
        assert_eq!(settings.level.as_deref(), Some("high"));
        assert_eq!(
            settings.overrides.get("reasoning_effort"),
            Some(&serde_json::json!("low"))
        );
        let committed = stored(&dir);
        let (result, _) = run_cli(
            &[
                "models",
                "settings",
                "openai/gpt-5.6",
                "--set",
                "max_output_tokens=0",
                "--yes",
            ],
            config,
            "",
            &[],
        )
        .await;
        assert!(result.is_err());
        assert_eq!(stored(&dir), committed);
    }
}

fn parse_model_settings(
    level: Option<String>,
    overrides: Vec<String>,
) -> Result<config::model_settings::ModelSettings> {
    let mut settings = config::model_settings::ModelSettings {
        level,
        ..Default::default()
    };
    for entry in overrides {
        let (name, value) = entry
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("use --set name=value"))?;
        if providers::catalog::ModelParameter::parse(name).is_none() {
            anyhow::bail!("unknown model parameter");
        }
        let value = serde_json::from_str(value).unwrap_or_else(|_| serde_json::Value::String(value.to_owned()));
        if settings.overrides.insert(name.to_owned(), value).is_some() {
            anyhow::bail!("duplicate model parameter");
        }
    }
    Ok(settings)
}
