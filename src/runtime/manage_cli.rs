//! `prx providers`, `prx models`, `prx routes`, the management part of
//! `prx config` (PMM-E) and `prx causal config` (CDP-14).
//!
//! Every command calls the library's management service
//! (`config::manage::ManagementService`) in-process: the same candidate
//! building, the same validation and the same plan renderer the Gateway uses,
//! so an equal change prints the same plan and the same refusal on every
//! surface. A mutating command prints its plan and writes nothing unless
//! `--yes` is given; a high-risk plan also needs `--confirm-high-risk
//! <REVISION>`, naming the revision the plan reported (D-049 B).
//!
//! The CLI writes the files through the configuration transaction (revision
//! guard, history record, validation of the load); a running PRX picks the
//! change up through its configuration watcher.
//!
//! A credential never comes from an argument: `credential set` reads it from
//! a hidden prompt, standard input (`--from-stdin`) or an environment
//! variable (`--from-env VAR`), and nothing prints it.

use anyhow::{Context, Result, bail};
use openprx::config::manage::service::{self as manage, ProviderEntry};
use openprx::config::manage::{
    ConfigActor, ConfigTransactionError, ManageError, ManagedChange, ManagedPlan, ManagementService, RecordStatus,
    RollbackRequest, RootCredential, SelectionOperation, render_plan,
};
use openprx::config::{self, Config};
use openprx::providers;
use std::io::{Read, Write};

use crate::{
    AgentsCommands, ApplyArgs, ConfigCommands, CredentialCommands, DefaultCommands, HistoryCommands, ModelCommands,
    ModelDeclarationArgs, OperationArg, ProvidersCommands, RoutesCommands,
};

/// A management command, as the dispatcher hands it over.
pub(crate) enum ManageCommand {
    Providers(Option<ProvidersCommands>),
    Routes(RoutesCommands),
    Models(ModelCommands),
    Config(ConfigCommands),
    Causal(openprx::causal_cli::CausalConfigCommands),
}

/// Where a command writes and reads: the process's streams in production,
/// buffers in tests.
pub(crate) struct Io<'a> {
    pub(crate) out: &'a mut dyn Write,
    pub(crate) stdin: &'a mut dyn Read,
    /// Reads an environment variable (`--from-env`).
    pub(crate) env: &'a dyn Fn(&str) -> Option<String>,
    /// Asks for a secret without echoing it.
    pub(crate) prompt: &'a dyn Fn(&str) -> Result<String>,
}

/// Run `command` against the process's own streams and environment.
pub(crate) async fn run_process(command: ManageCommand, config: Config) -> Result<()> {
    let mut stdout = std::io::stdout();
    let mut stdin = std::io::stdin();
    let env = |name: &str| std::env::var(name).ok();
    let prompt = |label: &str| crate::read_auth_input(label);
    let mut io = Io {
        out: &mut stdout,
        stdin: &mut stdin,
        env: &env,
        prompt: &prompt,
    };
    run(command, config, &mut io).await
}

/// Run `command` with `io`.
pub(crate) async fn run(command: ManageCommand, config: Config, io: &mut Io<'_>) -> Result<()> {
    match command {
        ManageCommand::Providers(command) => {
            run_providers(command.unwrap_or(ProvidersCommands::List), config, io).await
        }
        ManageCommand::Routes(command) => run_routes(command, config, io).await,
        ManageCommand::Models(command) => run_models(command, config, io).await,
        ManageCommand::Config(command) => run_config(command, config, io).await,
        ManageCommand::Causal(command) => run_causal(command, config, io).await,
    }
}

/// The CLI does not run the configuration's runtime: what it applies is
/// activated by a running PRX's watcher, so it is recorded deferred.
fn service(config: Config) -> ManagementService {
    ManagementService::deferred(config::new_shared(config))
}

const fn operation(arg: OperationArg) -> SelectionOperation {
    match arg {
        OperationArg::Chat => SelectionOperation::Chat,
        OperationArg::Embedding => SelectionOperation::Embedding,
    }
}

/// Plan `change`, print the plan, and apply it when `apply.yes`.
async fn change(service: &ManagementService, change: ManagedChange, apply: &ApplyArgs, io: &mut Io<'_>) -> Result<()> {
    let plan = service
        .plan(&change, ConfigActor::Cli)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    finish(service, plan, apply, io).await
}

/// Print `plan`, then apply it when `apply.yes`.
async fn finish(service: &ManagementService, plan: ManagedPlan, apply: &ApplyArgs, io: &mut Io<'_>) -> Result<()> {
    write!(io.out, "{}", plan.text())?;
    if plan.summary().is_empty() {
        writeln!(io.out, "Nothing to change.")?;
        return Ok(());
    }
    let revision = plan.revision().unwrap_or("none").to_string();
    let high_risk = !plan.summary().risk_reasons.is_empty();
    if !apply.yes {
        if high_risk {
            writeln!(
                io.out,
                "Dry run: nothing was written. This change is high risk: apply it with --yes --confirm-high-risk \
                 {revision}"
            )?;
        } else {
            writeln!(io.out, "Dry run: nothing was written. Apply it with --yes")?;
        }
        return Ok(());
    }
    match service
        .apply(plan, ConfigActor::Cli, apply.confirm_high_risk.as_deref())
        .await
    {
        Ok(outcome) => print_applied(&outcome, io),
        Err(error) => Err(refusal(error)),
    }
}

/// What an applied CLI change prints: written and validated, activation
/// deferred to the daemon (D-085 F-4).
fn print_applied(outcome: &openprx::config::manage::ConfigTransactionOutcome, io: &mut Io<'_>) -> Result<()> {
    match &outcome.record_id {
        Some(record) => writeln!(
            io.out,
            "Written and validated (history record {record}); activation deferred to the daemon: a running PRX \
             activates it through its configuration watcher, otherwise at its next start. This command cannot \
             confirm the activation; see `prx config status`. `prx config rollback {record}` undoes it."
        )?,
        None => writeln!(
            io.out,
            "The files already held this configuration; nothing was written."
        )?,
    }
    if !outcome.report.restart_required.is_empty() {
        writeln!(
            io.out,
            "Takes effect after a restart: {}",
            outcome.report.restart_required.join(", ")
        )?;
    }
    Ok(())
}

/// A refused CLI change as the error the command exits with; a needed
/// confirmation names the revision to confirm.
fn refusal(error: ManageError) -> anyhow::Error {
    match error {
        ManageError::Transaction(error @ ConfigTransactionError::ConfirmationRequired { .. }) => {
            let revision = match &error {
                ConfigTransactionError::ConfirmationRequired { revision, .. } => revision.clone().unwrap_or_default(),
                _ => String::new(),
            };
            anyhow::anyhow!(
                "{}. Review the plan above and confirm with --yes --confirm-high-risk {revision}",
                error.to_string().trim_end_matches('.')
            )
        }
        error => anyhow::anyhow!("{error}"),
    }
}

// ── prx causal config (CDP-14) ───────────────────────────────────────

fn causal_apply(apply: openprx::causal_cli::CausalApplyArgs) -> ApplyArgs {
    ApplyArgs {
        yes: apply.yes,
        confirm_high_risk: apply.confirm_high_risk,
    }
}

async fn run_causal(command: openprx::causal_cli::CausalConfigCommands, config: Config, io: &mut Io<'_>) -> Result<()> {
    use openprx::causal_cli::CausalConfigChange;
    let service = service(config);
    match openprx::causal_cli::config_change(command, &service.config())? {
        CausalConfigChange::Plan { mutations, apply } => {
            let plan = service
                .plan_causal(&mutations, ConfigActor::Cli)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            finish(&service, plan, &causal_apply(apply), io).await
        }
        CausalConfigChange::Secret {
            from_stdin,
            from_env,
            apply,
        } => store_decision_credential(&service, from_stdin, from_env.as_deref(), &causal_apply(apply), io).await,
    }
}

/// `prx causal config set secret`: a credential change is high risk, so
/// the secret is read only once the change is confirmed; the dry run names
/// the revision to confirm and reads nothing.
async fn store_decision_credential(
    service: &ManagementService,
    from_stdin: bool,
    from_env: Option<&str>,
    apply: &ApplyArgs,
    io: &mut Io<'_>,
) -> Result<()> {
    let config = service.config();
    let Some(selection) = openprx::decision::factory::configured_selection(&config.causal_tree) else {
        bail!(
            "no decision backend that takes a credential is configured; choose one with `prx causal config set backend`"
        );
    };
    let revision = service.revision().unwrap_or_else(|| "none".to_string());
    writeln!(
        io.out,
        "Store a new decision credential for {} and reference it from causal_tree.backend.credential_ref (high \
         risk: credential_changed). The previous decision credential of that provider is removed once the \
         change applies.",
        providers::redact_endpoint_userinfo(&selection.provider)
    )?;
    let Some(confirmed) = apply.confirm_high_risk.as_deref().filter(|_| apply.yes) else {
        writeln!(
            io.out,
            "Dry run: nothing was read or written. Apply it with --yes --confirm-high-risk {revision}"
        )?;
        return Ok(());
    };
    let secret = read_secret(from_stdin, from_env, io)?;
    let stored = service
        .store_decision_credential(
            openprx::config::manage::credentials::SecretString::new(secret),
            ConfigActor::Cli,
            Some(confirmed),
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    writeln!(
        io.out,
        "Stored decision credential {} (encrypted at rest when secrets.encrypt = true) and referenced it{}.",
        stored.id,
        stored
            .outcome
            .record_id
            .as_deref()
            .map(|record| format!(" (history record {record}; activation deferred to the daemon)"))
            .unwrap_or_default()
    )?;
    if let Some(previous) = &stored.removed_previous {
        writeln!(io.out, "Removed the previous decision credential {previous}.")?;
    }
    if let Some((previous, error)) = &stored.previous_left {
        writeln!(
            io.out,
            "The previous decision credential {previous} could not be removed ({error}); remove it with `prx \
             providers credential remove {previous}`."
        )?;
    }
    Ok(())
}

// ── prx providers ─────────────────────────────────────────────────────

async fn run_providers(command: ProvidersCommands, config: Config, io: &mut Io<'_>) -> Result<()> {
    match command {
        ProvidersCommands::List => {
            print_catalog(&config, io.out)?;
            print_configured(&config, io.out)
        }
        ProvidersCommands::Show { provider } => print_provider(&config, &provider, io.out),
        // `--fallback` is required by clap: it is the only role today.
        ProvidersCommands::Add {
            provider,
            fallback: _,
            apply,
        } => change(&service(config), ManagedChange::AddProvider { provider }, &apply, io).await,
        ProvidersCommands::Update {
            provider,
            api_url,
            clear_api_url,
            apply,
        } => {
            let api_url = if clear_api_url { None } else { api_url };
            change(
                &service(config),
                ManagedChange::SetProviderEndpoint { provider, api_url },
                &apply,
                io,
            )
            .await
        }
        ProvidersCommands::Remove { provider, apply } => {
            change(&service(config), ManagedChange::RemoveProvider { provider }, &apply, io).await
        }
        ProvidersCommands::Test { provider, model, live } => {
            let mode = if live {
                providers::connection_test::ProbeMode::Live
            } else {
                providers::connection_test::ProbeMode::Local
            };
            let report =
                providers::connection_test::test_connection(&config, provider.as_deref(), model.as_deref(), mode).await;
            writeln!(io.out, "{report}")?;
            let live_failed = report.live.as_ref().is_some_and(|live| live.error.is_some());
            if !report.local.resolved || live_failed {
                bail!("the connection test failed");
            }
            Ok(())
        }
        ProvidersCommands::Credential { credential_command } => run_credential(credential_command, config, io).await,
        ProvidersCommands::Migrate { apply, write } => run_migrate(config, apply, &write, io).await,
    }
}

// ── prx providers migrate ─────────────────────────────────────────────

/// `prx providers migrate`: the report (always), then with `--apply` the
/// plan, and with `--yes` the write (SP-PMM §2.7, PMM-H).
async fn run_migrate(config: Config, apply: bool, write: &ApplyArgs, io: &mut Io<'_>) -> Result<()> {
    let service = service(config);
    let report = service.migration_report().map_err(|error| anyhow::anyhow!("{error}"))?;
    write!(io.out, "{}", report.text())?;
    if !apply {
        if write.yes {
            bail!("--yes writes the migration only together with --apply; nothing was written");
        }
        if report.nothing_to_apply() {
            writeln!(io.out, "Nothing to migrate.")?;
        } else {
            writeln!(io.out, "Dry run: nothing was written. Plan the migration with --apply")?;
        }
        return Ok(());
    }
    let Some((report, plan)) = service
        .plan_migration()
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?
    else {
        writeln!(io.out, "Nothing to migrate.")?;
        return Ok(());
    };
    write!(io.out, "{}", plan.text())?;
    let references = report
        .inline_keys
        .iter()
        .map(openprx::config::manage::migrate::InlineKey::reference_id)
        .collect::<Vec<_>>();
    if !references.is_empty() {
        writeln!(
            io.out,
            "Credential references stored first (encrypted at rest when secrets.encrypt = true): {}",
            references.join(", ")
        )?;
    }
    if !write.yes {
        let revision = plan.revision().unwrap_or("none");
        if plan.summary().risk_reasons.is_empty() {
            writeln!(io.out, "Dry run: nothing was written. Apply it with --apply --yes")?;
        } else {
            writeln!(
                io.out,
                "Dry run: nothing was written. This change is high risk: apply it with --apply --yes \
                 --confirm-high-risk {revision}"
            )?;
        }
        return Ok(());
    }
    let outcome = service
        .apply_migration(&report, plan, ConfigActor::Cli, write.confirm_high_risk.as_deref())
        .await;
    match outcome {
        Ok(outcome) => {
            print_applied(&outcome, io)?;
            if outcome.record_id.is_some() && !references.is_empty() {
                writeln!(
                    io.out,
                    "Undoing it brings the inline keys back: `prx config rollback <record> --yes \
                     --restore-credentials`."
                )?;
            }
            Ok(())
        }
        Err(error) => Err(refusal(error)),
    }
}

/// The catalog of providers this build supports (the listing `prx
/// providers` always printed).
fn print_catalog(config: &Config, out: &mut dyn Write) -> Result<()> {
    let catalog = providers::list_providers();
    let current = providers::selection::default_provider(config)
        .trim()
        .to_ascii_lowercase();
    writeln!(out, "Supported providers ({} total):\n", catalog.len())?;
    writeln!(out, "  ID (use in config)  DESCRIPTION")?;
    writeln!(out, "  ─────────────────── ───────────")?;
    for entry in &catalog {
        let is_active = entry.name.eq_ignore_ascii_case(&current)
            || entry.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(&current));
        let marker = if is_active { " (active)" } else { "" };
        let local_tag = if entry.local { " [local]" } else { "" };
        let aliases = if entry.aliases.is_empty() {
            String::new()
        } else {
            format!("  (aliases: {})", entry.aliases.join(", "))
        };
        writeln!(
            out,
            "  {:<19} {}{}{}{}",
            entry.name, entry.display_name, local_tag, marker, aliases
        )?;
    }
    writeln!(out, "\n  custom:<URL>   Any OpenAI-compatible endpoint")?;
    writeln!(out, "  anthropic-custom:<URL>  Any Anthropic-compatible endpoint")?;
    Ok(())
}

fn roles(entry: &ProviderEntry) -> String {
    let mut roles = Vec::new();
    if entry.default {
        roles.push("default");
    }
    if entry.fallback {
        roles.push("fallback");
    }
    if roles.is_empty() {
        "used".to_string()
    } else {
        roles.join(", ")
    }
}

fn print_configured(config: &Config, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "\nConfigured providers:")?;
    for entry in manage::configured_providers(config) {
        let used_by = if entry.used_by.is_empty() {
            "nothing".to_string()
        } else {
            entry.used_by.join("; ")
        };
        writeln!(out, "  {}  [{}]  used by: {used_by}", entry.provider, roles(&entry))?;
    }
    Ok(())
}

fn print_provider(config: &Config, provider: &str, out: &mut dyn Write) -> Result<()> {
    let entry = manage::provider_entry(config, provider);
    let credentials = manage::provider_credentials(config, provider).map_err(|error| anyhow::anyhow!("{error}"))?;
    writeln!(out, "Provider: {}", entry.provider)?;
    writeln!(out, "Identity: {}", entry.identity)?;
    writeln!(out, "Roles: {}", roles(&entry))?;
    if let Some(endpoint) = &entry.endpoint {
        writeln!(out, "Endpoint (api_url): {endpoint}")?;
    }
    writeln!(out, "Model declarations: {}", entry.declarations)?;
    if entry.used_by.is_empty() {
        writeln!(out, "Used by: nothing")?;
    } else {
        writeln!(out, "Used by:")?;
        for name in &entry.used_by {
            writeln!(out, "  - {name}")?;
        }
    }
    print_provider_credentials(&config, &credentials, out)
}

fn print_provider_credentials(
    config: &Config,
    credentials: &manage::ProviderCredentials,
    out: &mut dyn Write,
) -> Result<()> {
    writeln!(out, "Credentials (never shown):")?;
    let mut any = false;
    if credentials.root_connection {
        any = true;
        writeln!(
            out,
            "  - root connection (api_key / api_url / reliability.api_keys): set"
        )?;
    }
    for slot in &credentials.inline_slots {
        any = true;
        writeln!(out, "  - inline key at {slot}: set")?;
    }
    for profile in &credentials.store_profiles {
        any = true;
        writeln!(out, "  - auth profile {profile}")?;
    }
    for reference in &credentials.references {
        any = true;
        let uses = manage::reference_use(config, &reference.id).map_err(|error| anyhow::anyhow!("{error}"))?;
        writeln!(
            out,
            "  - credential reference {} (updated {}): {uses}",
            reference.id,
            reference.updated_at.to_rfc3339(),
        )?;
    }
    if !any {
        writeln!(out, "  none")?;
    }
    writeln!(out, "In use: {}", credentials.guidance)?;
    Ok(())
}

// ── prx providers credential ──────────────────────────────────────────

/// The reference id a `remove` argument names: `cred-<name>` or `<name>`.
fn reference_id(reference: &str) -> Result<String> {
    let reference = reference.trim();
    if reference.starts_with(openprx::config::manage::credentials::REFERENCE_PREFIX) {
        Ok(reference.to_string())
    } else {
        openprx::config::manage::credentials::CredentialReferences::id_for(reference)
            .map_err(|error| anyhow::anyhow!("{error}"))
    }
}

/// Read a secret from standard input or from an environment variable, when
/// one of them is named (`None` otherwise). The trailing newline of piped
/// input is dropped; an empty secret is refused. Shared by `credential set`
/// and the `--from-stdin`/`--from-env` alternatives of the deprecated
/// argument flags (`onboard --api-key`, `go -k`, `auth paste-token --token`).
pub(crate) fn secret_from(
    from_stdin: bool,
    from_env: Option<&str>,
    stdin: &mut dyn Read,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>> {
    let secret = if let Some(variable) = from_env {
        env(variable).with_context(|| format!("environment variable {variable} is not set"))?
    } else if from_stdin {
        let mut text = String::new();
        stdin
            .read_to_string(&mut text)
            .context("failed to read the secret from standard input")?;
        text
    } else {
        return Ok(None);
    };
    let secret = secret.trim_end_matches(['\r', '\n']).to_string();
    if secret.trim().is_empty() {
        bail!("the secret is empty; nothing was stored");
    }
    Ok(Some(secret))
}

/// Read a credential from exactly one source; never from an argument.
fn read_secret(from_stdin: bool, from_env: Option<&str>, io: &mut Io<'_>) -> Result<String> {
    if let Some(secret) = secret_from(from_stdin, from_env, io.stdin, io.env)? {
        return Ok(secret);
    }
    let secret = (io.prompt)("Credential (input hidden)")?;
    if secret.trim().is_empty() {
        bail!("the credential is empty; nothing was stored");
    }
    Ok(secret)
}

async fn run_credential(command: CredentialCommands, config: Config, io: &mut Io<'_>) -> Result<()> {
    match command {
        CredentialCommands::Set {
            provider,
            name,
            from_stdin,
            from_env,
        } => {
            let id = providers::ProviderId::of(&provider);
            let name = match name {
                Some(name) => name,
                None => id.catalog_name().map(str::to_string).context(
                    "name the credential with --name (a provider without a catalog name has no default name)",
                )?,
            };
            let secret = read_secret(from_stdin, from_env.as_deref(), io)?;
            let guidance = manage::provider_credentials(&config, &provider)
                .map_err(|error| anyhow::anyhow!("{error}"))?
                .guidance;
            let status = service(config)
                .set_credential(provider, name, secret, ConfigActor::Cli)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            writeln!(
                io.out,
                "Stored credential reference {id} for provider {owner} (encrypted at rest when secrets.encrypt = \
                 true). A model route, embedding route or agent of {owner} uses it once its configuration names \
                 it: `credential_ref = \"{id}\"`; an enabled decision backend of {owner} can use it when configured \
                 with that reference. In a running PRX a replaced value takes effect when the \
                 configuration next changes (Gateway) or at the next restart (channels and daemon components); \
                 restart to apply it now. The replacement is recorded in the configuration history and cannot be \
                 rolled back (the previous value is not kept).\n{guidance}.",
                id = status.id,
                owner = status.owner,
            )?;
            Ok(())
        }
        CredentialCommands::Remove { reference } => {
            let id = reference_id(&reference)?;
            let removed = service(config)
                .remove_credential(&id)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            if removed {
                writeln!(io.out, "Removed credential reference {id}.")?;
            } else {
                writeln!(io.out, "Credential reference {id} does not exist; nothing was removed.")?;
            }
            Ok(())
        }
        CredentialCommands::Status {
            provider: Some(provider),
        } => {
            let entry = manage::provider_entry(&config, &provider);
            let credentials =
                manage::provider_credentials(&config, &provider).map_err(|error| anyhow::anyhow!("{error}"))?;
            writeln!(io.out, "Provider: {}", entry.provider)?;
            print_provider_credentials(&config, &credentials, io.out)
        }
        CredentialCommands::Status { provider: None } => {
            let references = service(config.clone())
                .credentials()
                .and_then(|references| references.list().map_err(ManageError::from))
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            if references.is_empty() {
                writeln!(io.out, "No credential references.")?;
            }
            for reference in references {
                let used = manage::reference_use(&config, &reference.id).map_err(|error| anyhow::anyhow!("{error}"))?;
                writeln!(
                    io.out,
                    "{}  provider {}  updated {}  {used}",
                    reference.id,
                    reference.owner,
                    reference.updated_at.to_rfc3339()
                )?;
            }
            Ok(())
        }
    }
}

// ── prx routes ────────────────────────────────────────────────────────

async fn run_routes(command: RoutesCommands, config: Config, io: &mut Io<'_>) -> Result<()> {
    match command {
        RoutesCommands::List { operation: op } => {
            let routes: Vec<(String, String, String)> = match operation(op) {
                SelectionOperation::Chat => config
                    .model_routes
                    .iter()
                    .map(|route| (route.hint.clone(), route.provider.clone(), route.model.clone()))
                    .collect(),
                SelectionOperation::Embedding => config
                    .embedding_routes
                    .iter()
                    .map(|route| (route.hint.clone(), route.provider.clone(), route.model.clone()))
                    .collect(),
            };
            if routes.is_empty() {
                writeln!(io.out, "No routes.")?;
            }
            for (hint, provider, model) in routes {
                writeln!(
                    io.out,
                    "hint:{hint}  {}  {model}",
                    manage::redact_provider_text(&provider)
                )?;
            }
            Ok(())
        }
        RoutesCommands::Add {
            hint,
            provider,
            model,
            operation: op,
            apply,
        } => {
            change(
                &service(config),
                ManagedChange::AddRoute {
                    operation: operation(op),
                    hint,
                    provider,
                    model,
                },
                &apply,
                io,
            )
            .await
        }
        RoutesCommands::Update {
            hint,
            provider,
            model,
            operation: op,
            apply,
        } => {
            change(
                &service(config),
                ManagedChange::UpdateRoute {
                    operation: operation(op),
                    hint,
                    provider,
                    model,
                },
                &apply,
                io,
            )
            .await
        }
        RoutesCommands::Remove {
            hint,
            operation: op,
            apply,
        } => {
            change(
                &service(config),
                ManagedChange::RemoveRoute {
                    operation: operation(op),
                    hint,
                },
                &apply,
                io,
            )
            .await
        }
    }
}

// ── prx models ────────────────────────────────────────────────────────

fn declaration(args: ModelDeclarationArgs) -> (config::schema::ProviderModelConfig, ApplyArgs) {
    let ModelDeclarationArgs {
        provider,
        model,
        operations,
        native_tools,
        vision_input,
        streaming,
        structured_output,
        tool_schema_dialect,
        tool_call_dialect,
        context_window,
        apply,
    } = args;
    (
        config::schema::ProviderModelConfig {
            provider,
            model,
            operations,
            native_tools,
            vision_input,
            streaming,
            structured_output,
            tool_schema_dialect,
            tool_call_dialect,
            context_window,
        },
        apply,
    )
}

async fn run_models(command: ModelCommands, config: Config, io: &mut Io<'_>) -> Result<()> {
    match command {
        ModelCommands::List {
            provider,
            operation: None,
            executable: false,
        } => openprx::onboard::run_models_list(&config, provider.as_deref()),
        ModelCommands::List {
            provider,
            operation,
            executable,
        } => {
            let operation = operation
                .as_deref()
                .map(manage::parse_operation)
                .transpose()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let entries = manage::model_catalog(
                &config,
                &manage::ModelFilter {
                    provider,
                    operation,
                    executable,
                },
            );
            if entries.is_empty() {
                writeln!(io.out, "No models match.")?;
            }
            for entry in &entries {
                print_model_line(entry, io.out)?;
            }
            Ok(())
        }
        ModelCommands::Show { provider, model } => {
            let entry = manage::model_entry(&config, &provider, &model).map_err(|error| anyhow::anyhow!("{error}"))?;
            print_model(&entry, io.out)
        }
        ModelCommands::Refresh { provider, force } => openprx::runtime::blocking::spawn_blocking(move || {
            openprx::onboard::run_models_refresh(&config, provider.as_deref(), force)
        })
        .await
        .map_err(|error| anyhow::anyhow!("models refresh task failed: {error}"))?,
        ModelCommands::Add(args) => {
            let (declaration, apply) = declaration(args);
            change(&service(config), ManagedChange::AddModel(declaration), &apply, io).await
        }
        ModelCommands::Update(args) => {
            let (declaration, apply) = declaration(args);
            change(&service(config), ManagedChange::UpdateModel(declaration), &apply, io).await
        }
        ModelCommands::Remove { provider, model, apply } => {
            change(
                &service(config),
                ManagedChange::RemoveModel { provider, model },
                &apply,
                io,
            )
            .await
        }
        ModelCommands::Default { default_command } => match default_command {
            DefaultCommands::Show => {
                match providers::selection::default_status(&config) {
                    Ok((_, line)) => writeln!(io.out, "Chat: {line}")?,
                    Err(refusal) => writeln!(io.out, "Chat: refused: {refusal}")?,
                }
                writeln!(
                    io.out,
                    "Embedding: provider '{}' model '{}'",
                    manage::redact_provider_text(&config.memory.embedding_provider),
                    config.memory.embedding_model
                )?;
                Ok(())
            }
            DefaultCommands::Set {
                provider,
                model,
                operation: op,
                keep_root_credential,
                clear_root_credential,
                apply,
            } => {
                let root_credential = if keep_root_credential {
                    RootCredential::Keep
                } else if clear_root_credential {
                    RootCredential::Clear
                } else {
                    RootCredential::Refuse
                };
                change(
                    &service(config),
                    ManagedChange::SetDefault {
                        operation: operation(op),
                        provider,
                        model,
                        root_credential,
                    },
                    &apply,
                    io,
                )
                .await
            }
        },
    }
}

/// One model on one line: provider, model, the operations the evidence
/// proves, and the evidence's source.
fn print_model_line(entry: &manage::ModelEntry, out: &mut dyn Write) -> Result<()> {
    let supported = entry
        .operations
        .iter()
        .filter(|(_, support)| support.as_str() == "supported")
        .map(|(operation, _)| operation.as_str())
        .collect::<Vec<_>>();
    let supported = if supported.is_empty() {
        "unverified".to_string()
    } else {
        supported.join(",")
    };
    writeln!(
        out,
        "{}  {}  [{supported}]  ({})",
        entry.provider, entry.model, entry.source
    )?;
    Ok(())
}

fn print_model(entry: &manage::ModelEntry, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "Provider: {} ({})", entry.provider, entry.identity)?;
    writeln!(out, "Model: {}", entry.model)?;
    writeln!(out, "Evidence: {}", entry.source)?;
    if let Some(fetched) = entry.fetched_at_unix {
        writeln!(out, "Catalog fetched at (unix): {fetched}")?;
    }
    if let Some(window) = entry.context_window {
        writeln!(out, "Context window: {window}")?;
    }
    writeln!(out, "Operations:")?;
    for (operation, support) in &entry.operations {
        writeln!(out, "  {operation}: {support}")?;
    }
    writeln!(out, "Chat features:")?;
    for (feature, support) in &entry.features {
        writeln!(out, "  {feature}: {support}")?;
    }
    Ok(())
}

// ── prx config (management part) ──────────────────────────────────────

/// Read the file a raw write posts, and refuse text that does not parse
/// without echoing it (the parser's message quotes the offending line).
fn read_posted_file(path: &std::path::Path) -> Result<(String, config::secret_paths::SecretScan)> {
    let contents = std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed = toml::from_str::<toml::Table>(&contents)
        .map_err(|error| anyhow::anyhow!("{}", config::manage::render::toml_error_location(&contents, &error)))?;
    let scan = config::secret_paths::scan(&toml::Value::Table(parsed));
    Ok((contents, scan))
}

pub(crate) async fn run_config(command: ConfigCommands, config: Config, io: &mut Io<'_>) -> Result<()> {
    match command {
        ConfigCommands::Plan { file, target } => {
            let (contents, scan) = read_posted_file(&file)?;
            let plan = service(config)
                .plan_file(target, contents, ConfigActor::Cli)
                .await
                .map_err(|error| anyhow::anyhow!("{}", scan.scrub(&error.to_string())))?;
            write!(io.out, "{}", plan.text())?;
            writeln!(io.out, "Revision: {}", plan.revision().unwrap_or("none"))?;
            Ok(())
        }
        ConfigCommands::Apply { file, target, apply } => {
            let (contents, scan) = read_posted_file(&file)?;
            let service = service(config);
            let plan = service
                .plan_file(target, contents, ConfigActor::Cli)
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

    async fn fixture(edit: impl FnOnce(&mut Config)) -> (tempfile::TempDir, Config) {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let mut config = Config {
            config_path: dir.path().join("config.toml"),
            workspace_dir: dir.path().join("workspace"),
            default_provider: Some("openai".into()),
            default_model: Some("gpt-4o".into()),
            ..Config::default()
        };
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
            Commands::Causal {
                causal_command: openprx::causal_cli::CausalCommands::Config { command },
                ..
            } => ManageCommand::Causal(command),
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

    /// CDP-14: `prx causal config set secret` reads the credential only once
    /// the high-risk change is confirmed: the dry run names the revision and
    /// reads nothing, from standard input, the environment or a prompt.
    #[tokio::test]
    async fn a_decision_secret_is_read_only_after_confirmation() {
        let (_dir, config) = fixture(|config| {
            config.causal_tree.backend.kind = String::from("compatible_http").try_into().expect("test: kind");
            config.causal_tree.backend.endpoint = "http://127.0.0.1:8900/v1".into();
            config.causal_tree.backend.model = "decider-1".into();
            config.providers.models.push(
                serde_json::from_value(serde_json::json!({
                    "provider": "custom:http://127.0.0.1:8900/v1",
                    "model": "decider-1",
                    "operations": ["decision_endpoint"],
                }))
                .expect("test: declaration"),
            );
        })
        .await;
        let read = std::cell::Cell::new(false);
        let asked = std::cell::Cell::new(false);
        let mut out = Vec::new();
        let mut stdin = Watched {
            bytes: b"sk-DECISION-READ-SENTINEL-01\n",
            read: &read,
        };
        let env = |_: &str| {
            asked.set(true);
            None
        };
        let prompt = |_: &str| -> Result<String> {
            asked.set(true);
            bail!("test: no terminal")
        };
        let result = {
            let mut io = Io {
                out: &mut out,
                stdin: &mut stdin,
                env: &env,
                prompt: &prompt,
            };
            run(
                manage_command(&["causal", "config", "set", "secret", "--from-stdin"]),
                config,
                &mut io,
            )
            .await
        };
        let text = String::from_utf8(out).expect("test: utf-8");
        result.expect("test: the dry run succeeds");
        assert!(text.contains("Dry run: nothing was read or written"), "{text}");
        assert!(!read.get() && !asked.get(), "nothing was read before the confirmation");
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
                vec!["routes", "add", "fast", "openai", "gpt-4o-mini"],
                ManagedChange::AddRoute {
                    operation: SelectionOperation::Chat,
                    hint: "fast".into(),
                    provider: "openai".into(),
                    model: "gpt-4o-mini".into(),
                },
            ),
            (
                vec!["models", "default", "set", "openai", "gpt-4o-mini"],
                ManagedChange::SetDefault {
                    operation: SelectionOperation::Chat,
                    provider: "openai".into(),
                    model: "gpt-4o-mini".into(),
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
                vec!["config", "agents", "set", "coder", "openai", "gpt-4o-mini"],
                ManagedChange::BindAgent {
                    agent: "coder".into(),
                    provider: "openai".into(),
                    model: "gpt-4o-mini".into(),
                },
            ),
            (
                vec![
                    "models",
                    "add",
                    "--provider",
                    "openai",
                    "--model",
                    "gpt-4o-mini",
                    "--operations",
                    "chat",
                    "--native-tools",
                    "true",
                ],
                ManagedChange::AddModel(config::schema::ProviderModelConfig {
                    provider: "openai".into(),
                    model: Some("gpt-4o-mini".into()),
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
            &["routes", "add", "fast", "openai", "gpt-4o-mini", "--yes"],
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
}
