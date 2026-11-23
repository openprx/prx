#[allow(clippy::too_many_lines)]
#[allow(unsafe_code)]
async fn async_main(cli: Cli) -> Result<()> {
    // Install default crypto provider for Rustls TLS.
    // This prevents the error: "could not automatically determine the process-level CryptoProvider"
    // when both aws-lc-rs and ring features are available (or neither is explicitly selected).
    if let Err(e) = rustls::crypto::ring::default_provider().install_default() {
        eprintln!("Warning: Failed to install default crypto provider: {e:?}");
    }

    if let Some(warning) = argv_secret_warning(&cli.command) {
        eprintln!("{warning}");
    }

    if let Some(config_dir) = &cli.config_dir {
        if config_dir.trim().is_empty() {
            bail!("--config-dir cannot be empty");
        }
        // Publish the explicit directory before anything can lazily create
        // process-global state (witness key, session-worker secret) that has no
        // `Config` in hand and would otherwise anchor on `$HOME/.openprx`.
        config::set_process_config_dir(std::path::absolute(config_dir.trim()).context("resolve --config-dir")?);
    }

    #[cfg(feature = "test-mock")]
    if let Commands::MailboxHarness {
        workspace,
        worker,
        turns,
        ..
    } = &cli.command
    {
        if *worker {
            return session_worker::mailbox_harness::run(workspace, *turns).await;
        }
        return memory::sqlite::mailbox::harness::accept_adopt(workspace);
    }

    // session-worker must stay stdout-clean for IPC JSON.
    if let Commands::SessionWorker {
        task,
        workspace,
        memory_db,
        tools,
    } = &cli.command
    {
        return session_worker::runner::run_from_stdin(
            task.clone(),
            workspace.clone(),
            memory_db.clone(),
            tools.clone(),
            cli.config_dir.clone(),
        )
        .await;
    }

    // Completions must remain stdout-only and should not load config or initialize logging.
    // This avoids warnings/log lines corrupting sourced completion scripts.
    if let Commands::Completions { shell } = &cli.command {
        let mut stdout = std::io::stdout().lock();
        write_shell_completion(*shell, &mut stdout)?;
        return Ok(());
    }

    if let Commands::Token {
        command: TokenCommands::Issue { out, force },
    } = &cli.command
    {
        let config = Config::load_or_init_with_config_dir(cli.config_dir.as_deref()).await?;
        let owner = runtime::host_bootstrap::issue_token(config, out, *force).await?;
        println!("{}\n{}", out.display(), owner);
        return Ok(());
    }
    if matches!(&cli.command, Commands::Init { .. }) {
        return init_cli::run(&cli).await;
    }

    // Initialize logging. RUST_LOG always wins when set. Otherwise foreground
    // commands (chat, go, ...) default to warn on stderr so a scripted
    // `prx chat --message 'What is 2+2?'` prints only the reply on stdout. Long-running
    // services (daemon, gateway, channel start) keep info on the process
    // default writer. Chat still uses a reloadable stderr subscriber so
    // tracing can move to chat.log before configuration is loaded.
    let tracing_defaults = cli_tracing_command(&cli.command).defaults();
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(tracing_defaults.filter));
    if tracing_defaults.write_stderr {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;

        let stderr_writer = tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr);
        let fmt_layer: ChatFmtLayer = tracing_subscriber::fmt::Layer::default().with_writer(stderr_writer);
        let (reload_layer, reload_handle) = tracing_subscriber::reload::Layer::new(fmt_layer);
        if CHAT_TRACING_RELOAD.set(reload_handle).is_err() {
            eprintln!("BUG: CHAT_TRACING_RELOAD already initialized");
        }
        if let Err(e) = tracing_subscriber::registry()
            .with(env_filter)
            .with(reload_layer)
            .try_init()
        {
            eprintln!("failed to set default tracing subscriber: {e}");
        }
    } else {
        let subscriber = fmt::Subscriber::builder().with_env_filter(env_filter).finish();
        if let Err(e) = tracing::subscriber::set_global_default(subscriber) {
            eprintln!("failed to set default tracing subscriber: {e}");
        }
    }

    // Acquire this before Config::load_* and hold it through command dispatch.
    // Fatal startup errors are still returned through anyhow and printed by
    // main after this guard restores stderr; routine tracing stays in chat.log.
    // `prx chat --message` keeps warn-level tracing on stderr instead.
    let _chat_tracing_guard = if matches!(&cli.command, Commands::Chat { message: None, .. }) {
        match chat::setup_chat_tracing_to_file() {
            Ok(guard) => Some(guard),
            Err(error) => {
                tracing::warn!(%error, "keeping chat tracing on stderr because chat.log is unavailable");
                None
            }
        }
    } else {
        None
    };

    // `prx go` — zero-config instant start. No permanent files written.
    if let Commands::Go {
        api_key,
        from_env,
        from_stdin,
        provider,
        model,
        message,
    } = &cli.command
    {
        let api_key = manage_cli::secret_from(
            *from_stdin,
            from_env.as_deref(),
            &mut std::io::stdin(),
            &|name: &str| std::env::var(name).ok(),
        )?
        .or_else(|| api_key.clone());
        let (detected_provider, detected_key, detected_model) =
            auth::discovery::detect_credentials(api_key.as_deref(), provider.as_deref(), model.as_deref()).context(
                "No API key found. Run `prx auth paste-token --provider anthropic`, or use: prx go -k <your-api-key>",
            )?;

        let tmpdir = tempfile::tempdir().context("failed to create temporary workspace for `prx go`")?;

        let mut config = Config::default();
        config.default_provider = Some(detected_provider);
        config.default_model = Some(detected_model);
        config.api_key = Some(detected_key);
        config.workspace_dir = tmpdir.path().to_path_buf();
        config.config_path = tmpdir.path().join("config.toml");
        // `prx go` persists nothing, so the detected model is an invocation,
        // resolved once where each path builds its provider: the agent entry
        // for `--message`, chat start-up otherwise. An id-only model runs
        // with a warning; one that cannot chat is refused before any
        // provider exists.

        if let Some(msg) = message {
            // `prx go --message` is the chat one-shot (`prx chat --message`):
            // chat owns ctrl_c internally, so the token is never cancelled.
            let result = chat::run_message(
                config,
                None, // provider already set in config
                None, // model already set in config
                0.7,
                chat::ChatMessageRequest {
                    message: msg.clone(),
                    save: false,
                    session: None,
                },
                crate::runtime::shutdown::never_cancelled_shutdown(),
            )
            .await;
            // Keep tmpdir alive until the turn finishes, then drop
            drop(tmpdir);
            return result;
        }

        // Non-dispatch direct call (`prx go` interactive chat): chat owns ctrl_c
        // internally; no dispatch-level signal source. Use the explicitly-named
        // never-cancelled token so chat's internal handler stays the sole owner.
        let result = chat::run(
            config,
            None, // provider already set in config
            None, // model already set in config
            0.7,
            false, // plain mode off
            None,  // no session resume
            false, // don't list sessions
            crate::runtime::shutdown::never_cancelled_shutdown(),
        )
        .await;
        // Keep tmpdir alive until chat finishes, then drop
        drop(tmpdir);
        return result;
    }

    // Diagnostic/schema inspection must not initialize config/workspace state.
    // Baseline is routed through the same read-only loader because it is
    // retained only as an explicit compatibility error, never as a write.
    let read_only_config_command = matches!(
        &cli.command,
        Commands::Migrate {
            migrate_command: MigrateCommands::Status
                | MigrateCommands::Verify
                | MigrateCommands::DryRun
                | MigrateCommands::Plan { .. }
                | MigrateCommands::Baseline
        }
    ) || matches!(
        &cli.command,
        Commands::Doctor { .. }
            | Commands::Tasks { .. }
            | Commands::Causal { .. }
            | Commands::DecisionEngine {
                command: openprx::decision_engine_cli::DecisionEngineCommands::Status
                    | openprx::decision_engine_cli::DecisionEngineCommands::Analyses { .. }
                    | openprx::decision_engine_cli::DecisionEngineCommands::Backends,
                ..
            }
            | Commands::Cost { .. }
    );
    #[cfg(feature = "wasm-plugins")]
    let read_only_config_command = read_only_config_command || matches!(&cli.command, Commands::Plugin { .. });
    let mut config = if read_only_config_command {
        Config::load_existing_read_only_with_config_dir(cli.config_dir.as_deref()).await?
    } else if read_only_management_command(&cli.command) {
        // Without a configuration yet: the defaults, shown, nothing written
        // (D-101 F-5).
        let config = Config::load_existing_or_defaults_read_only(cli.config_dir.as_deref()).await?;
        if !config.config_path.exists() {
            eprintln!(
                "no config yet; showing defaults (nothing is written; {} is created by the first command that \
                 changes the configuration)",
                config.config_path.display()
            );
        }
        config
    } else {
        Config::load_or_init_with_config_dir(cli.config_dir.as_deref()).await?
    };

    if let Commands::Chat {
        workdir: Some(root), ..
    } = &cli.command
    {
        config.runtime.workdir = Some(root.clone());
        config.validate()?;
    }

    // Covers the resolutions the flag does not carry (`OPENPRX_CONFIG_DIR`,
    // `OPENPRX_WORKSPACE`, the active-workspace marker). No-op when the flag
    // already published the same directory.
    if let Some(resolved_config_dir) = config.config_path.parent() {
        config::set_process_config_dir(resolved_config_dir.to_path_buf());
    }

    // `dispatch` logs the inherited-embedding-default notice for every
    // long-running runtime, but chat's tracing has already been redirected into
    // chat.log by this point, so for the one interactive entry point the
    // warning went somewhere nobody looks. The process stderr is untouched by
    // that redirect and the TUI has not taken the screen yet.
    if matches!(&cli.command, Commands::Chat { message: None, .. }) {
        chat::print_embedding_upgrade_notice_to_stderr(&config);
    }

    mode::dispatch(cli.command, config).await
}

/// The deprecation warning for a secret given as a command-line argument
/// (D-018 Q3, SP-PMM E-4). The flag keeps working; the warning names the
/// alternative that keeps the secret out of the process list and the shell
/// history. Never contains the secret.
fn argv_secret_warning(command: &Commands) -> Option<String> {
    let (flag, alternative) = match command {
        Commands::Init { api_key: Some(_), .. } => (
            "--api-key",
            "--from-stdin (pipe the key in: `printf %s \"$KEY\" | prx init --from-stdin`) or --from-env VAR",
        ),
        Commands::Go { api_key: Some(_), .. } => ("-k/--key", "--from-stdin or --from-env VAR"),
        Commands::Auth {
            auth_command: AuthCommands::PasteToken { token: Some(_), .. },
        } => (
            "--token",
            "--from-stdin (pipe the token in), or leave the flag out to be prompted without echo",
        ),
        _ => return None,
    };
    Some(format!(
        "warning: a secret passed with {flag} is visible to other local users in the process list and stays in \
         your shell history; the flag is deprecated. Use {alternative} instead."
    ))
}

/// `prx tasks` — talks to a running process's gateway rather than inspecting
/// this one, because the work registry is per-process state.
async fn handle_tasks_command(command: TasksCommands, config: &Config) -> anyhow::Result<()> {
    use openprx::runtime::tasks_cli::{self, TasksEndpoint};

    match command {
        TasksCommands::List { json, url, token } => {
            let endpoint = TasksEndpoint::resolve(config, url, token);
            let listing = tasks_cli::fetch_tasks(&endpoint).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&listing)?);
                return Ok(());
            }
            if listing.running.is_empty() {
                println!("No work items are currently running.");
            } else {
                println!("Running work items ({}):", listing.running.len());
                print_work_items(&listing.running);
            }
            if !listing.unreaped.is_empty() {
                println!();
                println!(
                    "Spawned but not reaped ({}) — children that outlived their owner:",
                    listing.unreaped.len()
                );
                print_work_items(&listing.unreaped);
            }
        }
        TasksCommands::Kill {
            id,
            no_cascade,
            json,
            url,
            token,
        } => {
            let endpoint = TasksEndpoint::resolve(config, url, token);
            let report = tasks_cli::request_kill(&endpoint, &id, !no_cascade).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Killed {} (cascade: {}) — {} target(s):",
                    report.requested,
                    if report.cascade { "on" } else { "off" },
                    report.targets.len()
                );
                for target in &report.targets {
                    println!(
                        "  {:<8} {:<10} {:<14} {}",
                        target.id, target.kind, target.outcome, target.name
                    );
                }
                if report.targets.iter().any(|target| target.outcome == "requested") {
                    println!();
                    println!(
                        "Some targets had not gone away yet when this report was produced. Nothing \
                         was abandoned — re-run `prx tasks list` to see the current state."
                    );
                }
            }
            // A target with no termination handle is a partial result, not a success.
            if report.targets.iter().any(|target| target.outcome == "not_killable") {
                bail!("one or more targets had no termination handle and could not be killed");
            }
        }
        TasksCommands::Send {
            id,
            message,
            json,
            url,
            token,
        } => {
            let endpoint = TasksEndpoint::resolve(config, url, token);
            let report = tasks_cli::request_message(&endpoint, &id, &message).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", render_task_message(&report));
            }
        }
        TasksCommands::Pools { json, url, token } => {
            let endpoint = TasksEndpoint::resolve(config, url, token);
            let report = tasks_cli::fetch_pools(&endpoint).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            for pool in &report.pools {
                println!("[{}] {}", pool.kind, pool.name);
                if let Some(metrics) = pool.metrics.as_object() {
                    let mut keys: Vec<&String> = metrics.keys().collect();
                    keys.sort();
                    for key in keys {
                        if let Some(value) = metrics.get(key) {
                            println!("    {key:<32} {value}");
                        }
                    }
                }
                println!();
            }
        }
    }
    Ok(())
}

/// Render one work-item table, shared by the running and unreaped sections.
///
/// Members of one `spawn_batch` fan-out are pulled together under a header at
/// the position of the batch's first member: they are siblings rather than a
/// lineage, so nothing else in the table says they belong to one another. A
/// listing with no batches renders exactly as it always did.
/// Render `prx tasks send` for a human, saying only what the server said.
///
/// The server reports `outcome: "queued"` because accepting a message onto a
/// run's bounded steering queue is the whole of what it can observe: an mpsc
/// send proves the queue had room and the receiver still exists, never that
/// anybody polls it. A task-mode sub-agent with no tools registers a steering
/// sender and never reads the receiver, so its messages are accepted and
/// consumed by nobody.
///
/// This renderer therefore prints the server's tag verbatim, labelled as the
/// outcome, and appends the server's own note when it sent one. It states no
/// delivery of its own: the previous "Delivered to ..." headline was a claim
/// the CLI was in no position to make, and an operator who read it went on
/// waiting for a target that was never going to act.
///
/// MUTATION GUARD: reinstate a fixed "Delivered" headline and
/// `a_sent_message_is_reported_as_the_server_tagged_it` fails.
fn render_task_message(report: &openprx::runtime::tasks_cli::MessageReport) -> String {
    let run_id = report.run_id.as_deref().unwrap_or("-");
    let mut out = format!(
        "Message to {} [{}] {} (run_id: {}) — outcome: {}",
        report.id, report.kind, report.name, run_id, report.outcome
    );
    // Absent on a gateway older than the `queued` outcome. Nothing is
    // substituted for it: a note this CLI wrote would be a client-side guess at
    // a server-side guarantee.
    if let Some(note) = report.note.as_deref() {
        out.push('\n');
        out.push_str(note);
    }
    out
}

fn print_work_items(items: &[openprx::runtime::tasks_cli::WorkItem]) {
    println!(
        "{:<8} {:<10} {:>10}  {:<14} {:<}",
        "ID", "KIND", "ELAPSED", "STATE", "NAME"
    );
    for (batch_id, members) in openprx::runtime::tasks_cli::group_by_batch(items) {
        if let Some(batch_id) = batch_id {
            println!("  batch {batch_id} — {} member(s):", members.len());
        }
        for item in members {
            print_work_item(item, batch_id.is_some());
        }
    }
}

/// Render one row. `indented` marks a row as belonging to the batch header
/// above it.
fn print_work_item(item: &openprx::runtime::tasks_cli::WorkItem, indented: bool) {
    println!("{}", work_item_line(item, indented));
}

/// The text of one listing row, split out from the printing so what an operator
/// reads can be asserted rather than eyeballed.
fn work_item_line(item: &openprx::runtime::tasks_cli::WorkItem, indented: bool) -> String {
    let mut name = item.name.clone();
    if let Some(parent) = &item.parent {
        name.push_str(&format!("  (parent {parent})"));
    }
    if let Some(run_id) = &item.run_id {
        name.push_str(&format!("  (run {run_id})"));
    }
    // Marked, not columned: only a minority of rows can take a message, and a
    // column would spend width on the ones that cannot. The mark exists because
    // the KIND column reads like an answer and is not one — a detached gateway
    // job is listed `sub_agent` and has no steering channel — so without it the
    // only way to learn an item is unsteerable is to send it a message and read
    // the refusal.
    //
    // MUTATION GUARD: drop this and `a_steerable_row_is_marked_and_a_look_alike_is_not` fails.
    if item.steerable {
        name.push_str("  (steerable)");
    }
    if let Some(pid) = item.pid {
        name.push_str(&format!("  (pid {pid}"));
        match item.pgid {
            Some(pgid) => name.push_str(&format!(", pgid {pgid})")),
            None => name.push(')'),
        }
    }
    let id = if indented {
        format!("  {}", item.id)
    } else {
        item.id.clone()
    };
    format!(
        "{:<8} {:<10} {:>10}  {:<14} {name}",
        id,
        item.kind,
        openprx::runtime::tasks_cli::format_elapsed(item.elapsed_secs),
        item.state
    )
}

async fn handle_memory_command(command: MemoryCommands, config: &Config) -> anyhow::Result<()> {
    match command {
        MemoryCommands::Reindex { json } => {
            let memory = memory::create_memory_with_storage_and_routes_with_acl(
                &config.memory,
                &config.embedding_routes_with_references(),
                Some(&config.storage.provider.config),
                &config.workspace_dir,
                memory::EmbeddingRootCredential::from_config(&config),
                &config.identity_bindings,
                &config.user_policies,
            )?;
            let backend = memory.name().to_string();
            let repaired = memory.reindex().await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "backend": backend,
                        "reindexed": repaired,
                    })
                );
            } else {
                println!("Memory reindex complete for {backend}: {repaired} stale vectors rebuilt");
            }
            Ok(())
        }
    }
}

fn write_shell_completion<W: Write>(shell: CompletionShell, writer: &mut W) -> Result<()> {
    use clap_complete::generate;
    use clap_complete::shells;

    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();

    match shell {
        CompletionShell::Bash => generate(shells::Bash, &mut cmd, bin_name, writer),
        CompletionShell::Fish => generate(shells::Fish, &mut cmd, bin_name, writer),
        CompletionShell::Zsh => generate(shells::Zsh, &mut cmd, bin_name, writer),
        CompletionShell::PowerShell => {
            generate(shells::PowerShell, &mut cmd, bin_name, writer);
        }
        CompletionShell::Elvish => generate(shells::Elvish, &mut cmd, bin_name, writer),
    }

    writer.flush()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingOpenAiLogin {
    profile: String,
    code_verifier: String,
    state: String,
    created_at: String,
    host_id: String,
    client_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingOpenAiLoginFile {
    profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_verifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted_code_verifier: Option<String>,
    state: String,
    created_at: String,
    #[serde(default)]
    host_id: String,
    #[serde(default)]
    client_id: String,
}

fn pending_openai_login_path(config: &Config) -> std::path::PathBuf {
    auth::state_dir_from_config(config).join("auth-openai-pending.json")
}

fn pending_openai_secret_store(config: &Config) -> security::secrets::SecretStore {
    security::secrets::SecretStore::new(&auth::state_dir_from_config(config), config.secrets.encrypt)
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn save_pending_openai_login(config: &Config, pending: &PendingOpenAiLogin) -> Result<()> {
    let path = pending_openai_login_path(config);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let secret_store = pending_openai_secret_store(config);
    let encrypted_code_verifier = secret_store.encrypt(&pending.code_verifier)?;
    let persisted = PendingOpenAiLoginFile {
        profile: pending.profile.clone(),
        code_verifier: None,
        encrypted_code_verifier: Some(encrypted_code_verifier),
        state: pending.state.clone(),
        created_at: pending.created_at.clone(),
        host_id: pending.host_id.clone(),
        client_id: pending.client_id.clone(),
    };
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let json = serde_json::to_vec_pretty(&persisted)?;
    std::fs::write(&tmp, json)?;
    set_owner_only_permissions(&tmp)?;
    std::fs::rename(tmp, &path)?;
    set_owner_only_permissions(&path)?;
    Ok(())
}

fn load_pending_openai_login(config: &Config) -> Result<Option<PendingOpenAiLogin>> {
    let path = pending_openai_login_path(config);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let persisted: PendingOpenAiLoginFile = serde_json::from_slice(&bytes)?;
    let secret_store = pending_openai_secret_store(config);
    let code_verifier = if let Some(encrypted) = persisted.encrypted_code_verifier {
        secret_store.decrypt(&encrypted)?
    } else if let Some(plaintext) = persisted.code_verifier {
        plaintext
    } else {
        bail!("Pending OpenAI login is missing code verifier");
    };
    Ok(Some(PendingOpenAiLogin {
        profile: persisted.profile,
        code_verifier,
        state: persisted.state,
        created_at: persisted.created_at,
        host_id: persisted.host_id,
        client_id: persisted.client_id,
    }))
}

fn clear_pending_openai_login(config: &Config) {
    let path = pending_openai_login_path(config);
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
        let _ = file.set_len(0);
        let _ = file.sync_all();
    }
    let _ = std::fs::remove_file(path);
}

fn read_auth_input(prompt: &str) -> Result<String> {
    let input = Password::new()
        .with_prompt(prompt)
        .allow_empty_password(false)
        .interact()?;
    Ok(input.trim().to_string())
}

fn read_plain_input(prompt: &str) -> Result<String> {
    let input: String = Input::new().with_prompt(prompt).interact_text()?;
    Ok(input.trim().to_string())
}

fn extract_openai_account_id_for_profile(access_token: &str, id_token: Option<&str>) -> Option<String> {
    let account_id = auth::openai_oauth::extract_account_id(access_token, id_token);
    if account_id.is_none() {
        warn!(
            "Could not extract OpenAI account id from OAuth access token; \
             requests may fail until re-authentication."
        );
    }
    account_id
}

fn format_expiry(profile: &auth::profiles::AuthProfile) -> String {
    profile
        .token_set
        .as_ref()
        .and_then(|token_set| token_set.expires_at)
        .map_or_else(
            || "n/a".to_string(),
            |ts| {
                let now = chrono::Utc::now();
                if ts <= now {
                    format!("expired at {}", ts.to_rfc3339())
                } else {
                    let mins = (ts - now).num_minutes();
                    format!("expires in {mins}m ({})", ts.to_rfc3339())
                }
            },
        )
}

fn attestation_check(
    observed_at: &str,
    id: &'static str,
    framework_reference: &'static str,
    control: &'static str,
    status: ControlStatus,
    evidence_kind: &'static str,
    evidence_reference: impl Into<String>,
    explanation: impl Into<String>,
    remediation: impl Into<String>,
) -> EuAiActCheck {
    EuAiActCheck {
        id,
        framework_reference,
        control,
        applicability: "operator classification required",
        severity: if matches!(status, ControlStatus::Fail) {
            "release_blocking"
        } else {
            "advisory"
        },
        status,
        observed_at: observed_at.to_string(),
        evidence: vec![EvidenceReference {
            kind: evidence_kind,
            reference: evidence_reference.into(),
        }],
        explanation: explanation.into(),
        remediation: remediation.into(),
    }
}

fn config_source_revision(config: &Config) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(&config.config_path).ok()?;
    Some(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn config_generation(config: &Config) -> Option<u64> {
    let generation_path = config.config_path.parent()?.join(".config-generation");
    fs::read_to_string(generation_path).ok()?.trim().parse().ok()
}

fn declaration_artifact_path(config: &Config) -> Option<PathBuf> {
    config
        .compliance
        .eu_ai_act
        .declaration
        .artifact_path
        .as_ref()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                config.workspace_dir.join(path)
            }
        })
}

fn incident_store_path(config: &Config) -> Option<PathBuf> {
    config.compliance.eu_ai_act.incident_store_path.as_ref().map(|path| {
        if path.is_absolute() {
            path.clone()
        } else {
            config.workspace_dir.join(path)
        }
    })
}

fn verified_declaration_control(config: &Config) -> (ControlStatus, String, String) {
    let Some(path) = declaration_artifact_path(config) else {
        return (
            ControlStatus::Fail,
            "declaration-of-conformity:path-missing".to_string(),
            "High-risk classification is recorded, but no declaration artifact path is configured.".to_string(),
        );
    };
    openprx::compliance::declaration::verify_declaration_artifact(&path).map_or_else(
        |_| {
            (
                ControlStatus::Fail,
                "declaration-of-conformity:verification-failed".to_string(),
                "The configured declaration artifact is missing, incomplete, or has a mismatched hash.".to_string(),
            )
        },
        |evidence| {
            (
                ControlStatus::Pass,
                evidence,
                "Artifact structure/hash and an operator-supplied external signature reference were verified; PRX did not validate signature authenticity or submission."
                    .to_string(),
            )
        },
    )
}

fn verified_incident_control(config: &Config) -> (ControlStatus, String, String) {
    let Some(path) = incident_store_path(config) else {
        return (
            ControlStatus::Fail,
            "article-73-incidents:path-missing".to_string(),
            "High-risk classification is recorded, but no durable incident store path is configured.".to_string(),
        );
    };
    openprx::compliance::incident::verify_incident_store(&path).map_or_else(
        |_| {
            (
                ControlStatus::Fail,
                "article-73-incidents:verification-failed".to_string(),
                "The configured incident store is missing, incompatible, or contains an overdue open incident without an initial report."
                    .to_string(),
            )
        },
        |evidence| {
            (
                ControlStatus::Pass,
                evidence,
                "The durable store schema and open-incident initial-report deadlines were verified read-only."
                    .to_string(),
            )
        },
    )
}

fn build_eu_ai_act_attestation(config: &Config) -> EuAiActAttestation {
    let observed_at = chrono::Utc::now().to_rfc3339();
    let config_revision = config_source_revision(config);
    let config_evidence = config_revision
        .clone()
        .unwrap_or_else(|| "config:unavailable".to_string());
    let audit_status = ControlStatus::Pass;
    let events_status = ControlStatus::Pass;
    let postgres = config.memory.backend == "postgres" || config.storage.provider.config.provider == "postgres";
    let (vector_isolation_status, vector_isolation_evidence, vector_isolation_explanation) = if postgres {
        let storage = &config.storage.provider.config;
        match storage.db_url.as_deref().map(|db_url| {
            memory::postgres::PostgresMemory::verify_vector_isolation(
                db_url,
                &storage.schema,
                &storage.table,
                storage.connect_timeout_secs,
            )
        }) {
            Some(Ok(evidence)) => (
                ControlStatus::Pass,
                evidence,
                "Migrations 21, 23 and 24 and the exact set of forced, permissive RLS policies were verified read-only on all PostgreSQL vector tables."
                    .to_string(),
            ),
            Some(Err(_)) => (
                ControlStatus::Fail,
                "postgres:vector-isolation:verification-failed".to_string(),
                "PostgreSQL is configured, but live migration/RLS evidence could not be verified.".to_string(),
            ),
            None => (
                ControlStatus::Fail,
                "postgres:vector-isolation:db-url-missing".to_string(),
                "PostgreSQL is configured without the connection data required for read-only evidence verification."
                    .to_string(),
            ),
        }
    } else {
        (
            ControlStatus::NotApplicable,
            "backend:not-postgres".to_string(),
            "PostgreSQL vector RLS is not applicable to this backend; application owner isolation is evaluated separately."
                .to_string(),
        )
    };
    let notice = &config.compliance.interaction_notice;
    let (notice_status, notice_explanation) = match notice.applicability {
        config::schema::InteractionNoticeApplicability::NotApplicable => (
            ControlStatus::NotApplicable,
            format!(
                "Operator exception owner={} reviewed_at={}",
                notice.exception_owner.as_deref().unwrap_or("missing"),
                notice.exception_reviewed_at.as_deref().unwrap_or("missing")
            ),
        ),
        config::schema::InteractionNoticeApplicability::Required => (
            ControlStatus::Pass,
            format!(
                "versioned notice {} is enabled before channel response routing",
                notice.version
            ),
        ),
    };
    let eu_classification = &config.compliance.eu_ai_act.classification;
    let (declaration_status, declaration_evidence, declaration_explanation) = match eu_classification.status {
        config::schema::EuAiActRiskClassification::Unclassified => (
            ControlStatus::Unknown,
            "classification:unclassified".to_string(),
            "No named owner has classified this deployment for the high-risk declaration requirement.".to_string(),
        ),
        config::schema::EuAiActRiskClassification::NotHighRisk => (
            ControlStatus::NotApplicable,
            format!(
                "classification:owner={}:assessed_at={}",
                eu_classification.owner.as_deref().unwrap_or("missing"),
                eu_classification.assessed_at.as_deref().unwrap_or("missing")
            ),
            "The recorded operator/legal classification says this deployment is not a high-risk AI system.".to_string(),
        ),
        config::schema::EuAiActRiskClassification::HighRisk => verified_declaration_control(config),
    };
    let (incident_status, incident_evidence, incident_explanation) = match eu_classification.status {
        config::schema::EuAiActRiskClassification::Unclassified => (
            ControlStatus::Unknown,
            "classification:unclassified".to_string(),
            "The incident workflow cannot be evaluated until a named owner classifies the deployment.".to_string(),
        ),
        config::schema::EuAiActRiskClassification::NotHighRisk => (
            ControlStatus::NotApplicable,
            format!(
                "classification:owner={}:assessed_at={}",
                eu_classification.owner.as_deref().unwrap_or("missing"),
                eu_classification.assessed_at.as_deref().unwrap_or("missing")
            ),
            "The recorded operator/legal classification says Article 73 is not applicable to this deployment."
                .to_string(),
        ),
        config::schema::EuAiActRiskClassification::HighRisk => verified_incident_control(config),
    };

    let mut checks = vec![
        attestation_check(
            &observed_at,
            "L01",
            "Article 12",
            "Side-effect audit logging enabled",
            audit_status,
            "config_revision",
            &config_evidence,
            "security.audit=always_on".to_string(),
            "Retain signed audit evidence.",
        ),
        attestation_check(
            &observed_at,
            "L02",
            "Article 12",
            "Audit retention floor",
            ControlStatus::Warning,
            "config_revision",
            &config_evidence,
            format!(
                "audit rotation size is {} MiB; no time-based floor is enforced",
                config.security.audit.max_size_mb
            ),
            "Define and enforce an operator-approved retention period.",
        ),
        attestation_check(
            &observed_at,
            "L03",
            "Article 12",
            "Control-ladder trace rotation",
            ControlStatus::Unknown,
            "runtime_diagnostic",
            "doctor:runtime/control_ladder_traces",
            "No runtime trace-archive evidence was evaluated by this command.",
            "Attach trace rotation and archival evidence.",
        ),
        attestation_check(
            &observed_at,
            "L04",
            "Article 12",
            "Message timeline recording",
            events_status,
            "config_revision",
            &config_evidence,
            "memory.events=always_on".to_string(),
            "Attach retention evidence.",
        ),
        attestation_check(
            &observed_at,
            "T01",
            "Article 13",
            "Provider routing transparency",
            ControlStatus::Warning,
            "config_revision",
            &config_evidence,
            format!(
                "default provider is {:?}; runtime route evidence was not queried",
                config.default_provider
            ),
            "Attach route-decision and final-outcome event references.",
        ),
        attestation_check(
            &observed_at,
            "T02",
            "Article 13",
            "Third-party provider data-policy disclosure",
            ControlStatus::Unknown,
            "operator_assertion",
            "provider-policy-owner:missing",
            "Provider policy URL and review ownership were not supplied.",
            "Record policy URLs, owner, and review expiry for each provider.",
        ),
        attestation_check(
            &observed_at,
            "T04",
            "Article 50",
            "AI interaction notice",
            notice_status,
            "config_revision",
            &config_evidence,
            notice_explanation,
            "Emit and minimally acknowledge a versioned AI identity notice before the first response.",
        ),
        attestation_check(
            &observed_at,
            "H01",
            "Article 14",
            "Approval grant cryptographic witness",
            ControlStatus::Unknown,
            "runtime_diagnostic",
            "approval-ledger:not_evaluated",
            "Source presence alone is not execution evidence.",
            "Attach a verified ApprovalGrant v2 ledger receipt.",
        ),
        attestation_check(
            &observed_at,
            "H02",
            "Article 14",
            "Side-effect authorization coverage",
            ControlStatus::Unknown,
            "runtime_diagnostic",
            "tool-gate-coverage:not_evaluated",
            "This report did not execute tool-gate coverage tests.",
            "Attach release test receipts for every side-effect tool.",
        ),
        attestation_check(
            &observed_at,
            "H03",
            "Article 14",
            "High-risk operation supervision",
            ControlStatus::Warning,
            "config_revision",
            &config_evidence,
            format!("autonomy level is {:?}", config.autonomy.level),
            "Use supervised policy and attach approval receipts where applicable.",
        ),
        attestation_check(
            &observed_at,
            "H04",
            "Article 14",
            "Inbound listener human oversight",
            ControlStatus::Unknown,
            "runtime_diagnostic",
            "channel-oversight:not_evaluated",
            "No live inbound-channel oversight evidence was evaluated.",
            "Run adapter-level supervised-mode conformance tests.",
        ),
        attestation_check(
            &observed_at,
            "A01",
            "Article 15",
            "Retrieval traceability",
            ControlStatus::Unknown,
            "runtime_diagnostic",
            "retrieval-traces:not_evaluated",
            "No retrieval trace was sampled.",
            "Attach immutable retrieval-trace identifiers.",
        ),
        attestation_check(
            &observed_at,
            "A02",
            "Article 15",
            "Compaction provenance",
            ControlStatus::Unknown,
            "runtime_diagnostic",
            "compaction-runs:not_evaluated",
            "No compaction run was sampled.",
            "Attach completed compaction-run identifiers and fidelity evidence.",
        ),
        attestation_check(
            &observed_at,
            "A03",
            "Article 15",
            "Prompt-injection content boundaries",
            ControlStatus::Unknown,
            "runtime_diagnostic",
            "content-boundary:not_evaluated",
            "Static implementation claims are insufficient evidence.",
            "Attach adversarial content-boundary test results.",
        ),
        attestation_check(
            &observed_at,
            "A04",
            "Article 15 internal security control",
            "Vector owner/tenant isolation",
            vector_isolation_status,
            "postgres_runtime_evidence",
            vector_isolation_evidence,
            vector_isolation_explanation,
            "Install authoritative RLS policies and attach cross-owner negative-test evidence.",
        ),
        attestation_check(
            &observed_at,
            "Q01",
            "Article 16",
            "Dependency vulnerability gate",
            ControlStatus::Unknown,
            "release_receipt",
            "cargo-audit:missing",
            "No cargo-audit receipt was supplied to this invocation.",
            "Attach a timestamped cargo-audit release receipt.",
        ),
        attestation_check(
            &observed_at,
            "Q02",
            "Article 16",
            "Rust engineering gate",
            ControlStatus::Unknown,
            "release_receipt",
            "engineering-gate:missing",
            "No fmt/clippy/check/test receipt was supplied.",
            "Attach source-identity-bound engineering gate results.",
        ),
        attestation_check(
            &observed_at,
            "Q03",
            "Article 16",
            "Quality management documentation",
            ControlStatus::Unknown,
            "operator_assertion",
            "qms-owner:missing",
            "No QMS owner or reviewed document set was supplied.",
            "Record the QMS owner, document versions, and review date.",
        ),
        attestation_check(
            &observed_at,
            "C01",
            "Article 17",
            "Quality management system",
            ControlStatus::Unknown,
            "operator_assertion",
            "qms-classification:missing",
            "Applicability and operator evidence are unresolved.",
            "Complete legal classification and QMS evidence review.",
        ),
        attestation_check(
            &observed_at,
            "C02",
            "Article 47 and Annex V",
            "EU declaration of conformity artifact",
            declaration_status,
            "artifact",
            declaration_evidence,
            declaration_explanation,
            "Generate a versioned artifact only from explicit operator declarations.",
        ),
        attestation_check(
            &observed_at,
            "M01",
            "Article 72",
            "Post-market monitoring inputs",
            ControlStatus::Unknown,
            "runtime_diagnostic",
            "monitoring-evidence:not_evaluated",
            "Runtime health and trace data were not sampled.",
            "Attach monitoring evidence and an operator-approved plan.",
        ),
        attestation_check(
            &observed_at,
            "M02",
            "Article 73",
            "Incident response ownership",
            ControlStatus::Unknown,
            "operator_assertion",
            "incident-owner:missing",
            "No incident owner or jurisdiction matrix was supplied.",
            "Record the incident owner, jurisdictions, and escalation runbook.",
        ),
        attestation_check(
            &observed_at,
            "M03",
            "Article 72",
            "Post-market report cadence",
            ControlStatus::Unknown,
            "operator_assertion",
            "monitoring-cadence:missing",
            "No reviewed monitoring cadence was supplied.",
            "Record cadence, owner, review date, and report references.",
        ),
        attestation_check(
            &observed_at,
            "M04",
            "Article 73",
            "Serious-incident workflow",
            incident_status,
            "incident_store",
            incident_evidence,
            incident_explanation,
            "Configure the durable incident store and use the incident CLI to record awareness, classification, reports, supplements, and closure.",
        ),
    ];
    checks.sort_by_key(|check| check.id);

    let passed_count = checks
        .iter()
        .filter(|check| check.status == ControlStatus::Pass)
        .count();
    let warning_count = checks
        .iter()
        .filter(|check| check.status == ControlStatus::Warning)
        .count();
    let failed_count = checks
        .iter()
        .filter(|check| check.status == ControlStatus::Fail)
        .count();
    let unknown_count = checks
        .iter()
        .filter(|check| check.status == ControlStatus::Unknown)
        .count();
    let not_applicable_count = checks
        .iter()
        .filter(|check| check.status == ControlStatus::NotApplicable)
        .count();

    EuAiActAttestation {
        report_type: "EU AI Act implementation attestation",
        evaluator_version: "prx-eu-ai-act-controls/v2",
        product_version: env!("CARGO_PKG_VERSION"),
        source_identity: option_env!("PRX_BUILD_SOURCE_REVISION").map(str::to_string),
        config_generation: config_generation(config),
        config_source_revision: config_revision,
        generated_at: observed_at,
        applicability: ApplicabilityDecision {
            status: ControlStatus::Unknown,
            classification: "unclassified; operator and legal review required".to_string(),
            owner: None,
            reviewed_at: None,
        },
        default_provider: config.default_provider.clone(),
        total_checks: checks.len(),
        passed_count,
        warning_count,
        failed_count,
        unknown_count,
        not_applicable_count,
        checks,
    }
}

fn render_eu_ai_act_attestation_markdown(attestation: &EuAiActAttestation, verbose: bool) -> String {
    let mut output = String::new();
    output.push_str("# PRX EU AI Act Implementation Attestation\n\n");
    output.push_str("This is an implementation attestation, not a regulator-issued certification.\n\n");
    output.push_str(&format!("- Product version: {}\n", attestation.product_version));
    output.push_str(&format!(
        "- Source identity: {}\n",
        attestation.source_identity.as_deref().unwrap_or("unavailable")
    ));
    output.push_str(&format!("- Generated at: {}\n", attestation.generated_at));
    output.push_str(&format!(
        "- Classification: {}\n",
        attestation.applicability.classification
    ));
    output.push_str(&format!(
        "- Checks: {} total, {} pass, {} warning, {} fail, {} unknown, {} not applicable\n\n",
        attestation.total_checks,
        attestation.passed_count,
        attestation.warning_count,
        attestation.failed_count,
        attestation.unknown_count,
        attestation.not_applicable_count
    ));
    output.push_str("| ID | Article | Status | Control |\n");
    output.push_str("|----|---------|--------|---------|\n");
    for check in &attestation.checks {
        output.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            check.id,
            check.framework_reference,
            check.status.as_str(),
            check.control
        ));
        if verbose {
            output.push_str(&format!(
                "\n{} evidence: {}\n\n{} explanation: {}\n\n{} remediation: {}\n\n",
                check.id,
                check
                    .evidence
                    .iter()
                    .map(|evidence| format!("{}={}", evidence.kind, evidence.reference))
                    .collect::<Vec<_>>()
                    .join(", "),
                check.id,
                check.explanation,
                check.id,
                check.remediation
            ));
        }
    }
    output
}

fn render_eu_ai_act_attestation_json(attestation: &EuAiActAttestation) -> Result<String> {
    serde_json::to_string_pretty(attestation).context("Failed to serialize EU AI Act attestation")
}

fn attestation_fails_gate(attestation: &EuAiActAttestation, fail_on: AuditFailOn) -> bool {
    attestation.checks.iter().any(|check| match fail_on {
        AuditFailOn::Fail => check.status == ControlStatus::Fail,
        AuditFailOn::Warning => matches!(
            check.status,
            ControlStatus::Warning | ControlStatus::Unknown | ControlStatus::Fail
        ),
    })
}

fn handle_audit_command(audit_command: AuditCommands, config: &Config) -> Result<()> {
    match audit_command {
        AuditCommands::AttestEuAiAct {
            json,
            format,
            output,
            verbose,
            fail_on,
        } => {
            let attestation = build_eu_ai_act_attestation(config);
            let stdout_json = json || format == AuditOutputFormat::Json;
            let stdout_rendered = if stdout_json {
                render_eu_ai_act_attestation_json(&attestation)?
            } else {
                render_eu_ai_act_attestation_markdown(&attestation, verbose)
            };

            if let Some(path) = output {
                let write_json = path.extension().and_then(|ext| ext.to_str()) == Some("json") || stdout_json;
                let file_rendered = if write_json {
                    render_eu_ai_act_attestation_json(&attestation)?
                } else {
                    render_eu_ai_act_attestation_markdown(&attestation, verbose)
                };
                fs::write(&path, file_rendered)
                    .with_context(|| format!("Failed to write EU AI Act attestation to {}", path.display()))?;
            }

            println!("{stdout_rendered}");
            if fail_on.is_some_and(|threshold| attestation_fails_gate(&attestation, threshold)) {
                bail!("EU AI Act implementation attestation did not satisfy --fail-on threshold");
            }
            Ok(())
        }
        AuditCommands::GenerateEuDeclaration {
            output,
            signature_reference,
        } => {
            let configured_path = declaration_artifact_path(config);
            let output = output
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        config.workspace_dir.join(path)
                    }
                })
                .or_else(|| configured_path.clone())
                .context(
                    "declaration output is required via --output or compliance.eu_ai_act.declaration.artifact_path",
                )?;
            if let Some(configured_path) = configured_path {
                anyhow::ensure!(
                    output == configured_path,
                    "--output must match compliance.eu_ai_act.declaration.artifact_path so attestation can verify it"
                );
            }
            let artifact = openprx::compliance::declaration::build_declaration_artifact(
                config.compliance.eu_ai_act.classification.status,
                &config.compliance.eu_ai_act.declaration,
                &signature_reference,
            )?;
            openprx::compliance::declaration::write_declaration_artifact(&output, &artifact)?;
            println!(
                "{}",
                serde_json::json!({
                    "artifact": output,
                    "payload_sha256": artifact.payload_sha256,
                    "signature_status": artifact.signature.status,
                    "submitted": artifact.signature.submitted,
                })
            );
            Ok(())
        }
        AuditCommands::IncidentCreate { input } => {
            let path = incident_store_path(config).context("compliance.eu_ai_act.incident_store_path is required")?;
            let bytes =
                fs::read(&input).with_context(|| format!("Failed to read incident input {}", input.display()))?;
            let request = serde_json::from_slice(&bytes).context("Failed to parse incident input JSON")?;
            let mut store = openprx::compliance::incident::IncidentStore::open(&path)?;
            let record = store.create(request)?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            Ok(())
        }
        AuditCommands::IncidentInitialReport { incident_id, input } => {
            let path = incident_store_path(config).context("compliance.eu_ai_act.incident_store_path is required")?;
            let report = fs::read_to_string(&input)
                .with_context(|| format!("Failed to read initial report {}", input.display()))?;
            let mut store = openprx::compliance::incident::IncidentStore::open(&path)?;
            let record = store.set_initial_report(&incident_id, &report)?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            Ok(())
        }
        AuditCommands::IncidentSupplement { incident_id, input } => {
            let path = incident_store_path(config).context("compliance.eu_ai_act.incident_store_path is required")?;
            let content = fs::read_to_string(&input)
                .with_context(|| format!("Failed to read incident supplement {}", input.display()))?;
            let mut store = openprx::compliance::incident::IncidentStore::open(&path)?;
            let supplement = store.add_supplement(&incident_id, &content)?;
            println!("{}", serde_json::to_string_pretty(&supplement)?);
            Ok(())
        }
        AuditCommands::IncidentClose {
            incident_id,
            closed_at,
            summary,
        } => {
            let path = incident_store_path(config).context("compliance.eu_ai_act.incident_store_path is required")?;
            let closed_at = chrono::DateTime::parse_from_rfc3339(&closed_at)
                .context("--closed-at must be RFC 3339")?
                .with_timezone(&Utc);
            let summary = fs::read_to_string(&summary)
                .with_context(|| format!("Failed to read closure summary {}", summary.display()))?;
            let mut store = openprx::compliance::incident::IncidentStore::open(&path)?;
            let record = store.close(&incident_id, closed_at, &summary)?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            Ok(())
        }
        AuditCommands::IncidentExport { incident_id, output } => {
            let path = incident_store_path(config).context("compliance.eu_ai_act.incident_store_path is required")?;
            let output = if output.is_absolute() {
                output
            } else {
                config.workspace_dir.join(output)
            };
            let store = openprx::compliance::incident::IncidentStore::open(&path)?;
            let export = store.export(&incident_id)?;
            openprx::compliance::incident::write_incident_export(&output, &export)?;
            println!(
                "{}",
                serde_json::json!({
                    "artifact": output,
                    "incident_id": incident_id,
                    "automatically_submitted": export.automatically_submitted,
                })
            );
            Ok(())
        }
    }
}

fn handle_approval_command(approval_command: ApprovalCommands, config: &Config) -> Result<()> {
    let conn = open_approval_ledger(config)?;
    match approval_command {
        ApprovalCommands::List { version } => {
            let version = version.unwrap_or(2);
            if version == acl::approval_grant::ApprovalGrantV2::VERSION {
                let mut stmt = conn.prepare(
                    "SELECT grant_id, owner_id, principal_id, capability_op_id, expires_at, revoked_at
                     FROM approval_grants
                     WHERE version = ?1
                     ORDER BY issued_at DESC
                     LIMIT 50",
                )?;
                let rows = stmt.query_map([i64::from(version)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                })?;
                let grants = rows.collect::<rusqlite::Result<Vec<_>>>()?;
                if grants.is_empty() {
                    println!("no approval grants found (version: 2)");
                } else {
                    for (grant_id, owner_id, principal_id, op_id, expires_at, revoked_at) in grants {
                        let status = if revoked_at.is_some() { "revoked" } else { "active" };
                        println!(
                            "{grant_id}\t{status}\towner={owner_id}\tprincipal={principal_id}\top={op_id}\texpires={expires_at}"
                        );
                    }
                }
                Ok(())
            } else {
                bail!("unsupported approval grant version: {version}")
            }
        }
        ApprovalCommands::Verify { all, grant_id } => {
            if all {
                let total: i64 = conn.query_row("SELECT COUNT(*) FROM approval_grants", [], |row| row.get(0))?;
                let malformed: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM approval_grants
                     WHERE grant_json = '' OR signature_alg = '' OR signed_payload_sha256 = ''",
                    [],
                    |row| row.get(0),
                )?;
                if total == 0 {
                    println!("no approval grants found to verify");
                } else if malformed == 0 {
                    println!("approval grant ledger metadata verified: {total} grants");
                } else {
                    bail!("approval grant ledger metadata verification failed: {malformed}/{total} malformed grants");
                }
                return Ok(());
            }
            let Some(grant_id) = grant_id else {
                bail!("provide a grant id or --all");
            };
            let found: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT grant_id, signature_alg, signed_payload_sha256
                     FROM approval_grants
                     WHERE grant_id = ?1",
                    [grant_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((grant_id, signature_alg, payload_sha)) = found else {
                bail!("approval grant not found: {grant_id}");
            };
            if signature_alg.trim().is_empty() || payload_sha.trim().is_empty() {
                bail!("approval grant metadata verification failed: {grant_id}");
            }
            println!("approval grant metadata verified: {grant_id} ({signature_alg})");
            Ok(())
        }
        ApprovalCommands::Revoke { grant_id, reason } => {
            let now = Utc::now().to_rfc3339();
            let updated = conn.execute(
                "UPDATE approval_grants
                 SET revoked_at = COALESCE(revoked_at, ?2),
                     revocation_reason = COALESCE(revocation_reason, ?3),
                     updated_at = ?2
                 WHERE grant_id = ?1",
                params![grant_id, now, reason],
            )?;
            if updated == 0 {
                bail!("approval grant not found: {grant_id}");
            }
            conn.execute(
                "INSERT INTO approval_grant_events (event_id, grant_id, event_type, actor, occurred_at, payload_json)
                 VALUES (?1, ?2, 'grant.revoked', 'cli', ?3, ?4)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    grant_id,
                    now,
                    serde_json::json!({ "reason": reason }).to_string()
                ],
            )?;
            println!("approval grant revoked: {grant_id}");
            Ok(())
        }
    }
}

fn open_approval_ledger(config: &Config) -> Result<Connection> {
    let db_path = config.workspace_dir.join("memory").join("brain.db");
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create approval ledger directory: {}", parent.display()))?;
    }
    let conn =
        Connection::open(&db_path).with_context(|| format!("Failed to open approval ledger: {}", db_path.display()))?;
    memory::sqlite::init_approval_grant_schema(&conn)?;
    Ok(conn)
}

async fn login_openrouter(auth_service: &auth::AuthService, profile: &str, headless: bool) -> Result<()> {
    let pkce = auth::openrouter_oauth::generate_pkce();
    let client = openprx::prx_http_client();
    let code = if headless {
        println!("Open this URL. It shows a code to paste back (no browser callback):");
        println!("{}", auth::openrouter_oauth::headless_authorize_url(&pkce));
        read_plain_input("Paste OpenRouter code")?
    } else {
        let (listener, callback) = auth::openrouter_oauth::bind_loopback().await?;
        println!("Open this URL in your browser and authorize access:");
        println!("{}", auth::openrouter_oauth::browser_authorize_url(&callback, &pkce));
        println!("Waiting for the loopback callback...");
        auth::openrouter_oauth::receive_loopback_code(listener, std::time::Duration::from_mins(3)).await?
    };
    let key = auth::openrouter_oauth::exchange_code(&client, &code, &pkce.verifier).await?;
    auth_service.store_provider_token("openrouter", profile, &key, std::collections::HashMap::new(), true)?;
    println!("Saved profile {profile}");
    println!("Active profile for openrouter: {profile}");
    Ok(())
}

async fn login_xai(auth_service: &auth::AuthService, config: &Config, profile: &str, device_code: bool) -> Result<()> {
    let client = openprx::prx_http_client();
    let state_dir = auth::state_dir_from_config(config);

    if device_code {
        let device = auth::xai_oauth::start_device_code_flow(&client).await?;
        println!("xAI device-code login started.");
        println!("Visit: {}", device.verification_uri);
        println!("Code:  {}", device.user_code);
        if let Some(uri_complete) = &device.verification_uri_complete {
            println!("Fast link: {uri_complete}");
        }
        let token_set = auth::xai_oauth::poll_device_code_tokens(&client, &device).await?;
        auth_service.store_xai_tokens(profile, token_set, true)?;
        println!("Saved profile {profile}");
        println!("Active profile for xai: {profile}");
        return Ok(());
    }

    let pkce = auth::xai_oauth::generate_pkce();
    let pending = auth::xai_oauth::PendingXaiLogin {
        profile: profile.to_string(),
        code_verifier: pkce.code_verifier.clone(),
        state: pkce.state.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    auth::xai_oauth::save_pending_login(&state_dir, config.secrets.encrypt, &pending)?;
    let authorize_url = auth::xai_oauth::build_authorize_url(&pkce);
    println!("Open this URL in your browser and authorize access:");
    println!("{authorize_url}");
    println!();
    println!(
        "Waiting for callback at {} ...",
        auth::xai_oauth::XAI_OAUTH_REDIRECT_URI
    );

    let code = match auth::xai_oauth::receive_loopback_code(&pkce.state, std::time::Duration::from_secs(180)).await {
        Ok(code) => code,
        Err(error) => {
            println!("Callback capture failed: {error}");
            println!("Run `prx auth paste-redirect --provider xai --profile {profile}`");
            return Ok(());
        }
    };
    let token_set = auth::xai_oauth::exchange_code_for_tokens(&client, &code, &pkce.code_verifier).await?;
    auth_service.store_xai_tokens(profile, token_set, true)?;
    auth::xai_oauth::clear_pending_login(&state_dir);
    println!("Saved profile {profile}");
    println!("Active profile for xai: {profile}");
    Ok(())
}

async fn paste_xai_redirect(
    auth_service: &auth::AuthService,
    config: &Config,
    profile: &str,
    input: Option<String>,
) -> Result<()> {
    let state_dir = auth::state_dir_from_config(config);
    let pending = auth::xai_oauth::load_pending_login(&state_dir, config.secrets.encrypt)?
        .ok_or_else(|| anyhow::anyhow!("No pending xAI login found. Run `prx auth login --provider xai` first."))?;
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
    let code = auth::xai_oauth::parse_code_from_redirect(&redirect_input, Some(&pending.state))?;
    let client = openprx::prx_http_client();
    let token_set = auth::xai_oauth::exchange_code_for_tokens(&client, &code, &pending.code_verifier).await?;
    auth_service.store_xai_tokens(profile, token_set, true)?;
    auth::xai_oauth::clear_pending_login(&state_dir);
    println!("Saved profile {profile}");
    println!("Active profile for xai: {profile}");
    Ok(())
}
