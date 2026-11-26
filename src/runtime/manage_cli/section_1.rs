// `prx providers`, `prx models`, `prx routes`, the management part of
// `prx config` (PMM-E) and `prx decision-engine config` (P2-R8).
//
// Every command calls the library's management service
// (`config::manage::ManagementService`) in-process: the same candidate
// building, the same validation and the same plan renderer the Gateway uses,
// so an equal change prints the same plan and the same refusal on every
// surface. A mutating command prints its plan and writes nothing unless
// `--yes` is given; a high-risk plan also needs `--confirm-high-risk
// <REVISION>`, naming the revision the plan reported (D-049 B).
//
// The CLI writes the files through the configuration transaction (revision
// guard, history record, validation of the load); a running PRX picks the
// change up through its configuration watcher.
//
// A credential never comes from an argument: `credential set` reads it from
// a hidden prompt, standard input (`--from-stdin`) or an environment
// variable (`--from-env VAR`), and nothing prints it.

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
    DecisionEngine(openprx::decision_engine_cli::DecisionEngineConfigCommands),
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
        ManageCommand::DecisionEngine(command) => run_decision_engine(command, config, io).await,
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
    let high_risk = !plan.summary().risk_reasons.is_empty() && !plan.applies_at_full();
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

// ── prx decision-engine config (P2-R8) ──────────────────────────────

/// `prx decision-engine config set <key> <value>`: one allowlisted key,
/// planned and applied like every management change. A refused key is one
/// typed error (exit ≠ 0).
async fn run_decision_engine(
    command: openprx::decision_engine_cli::DecisionEngineConfigCommands,
    config: Config,
    io: &mut Io<'_>,
) -> Result<()> {
    use openprx::config::manage::mutation::decision_engine::{CausalMutation, engine_setting};
    use openprx::decision_engine_cli::DecisionEngineConfigCommands;
    let DecisionEngineConfigCommands::Set { key, value, apply } = command;
    let setting = engine_setting(&key, &value)?;
    let service = service(config);
    let plan = service
        .plan_causal(&[CausalMutation::SetEngine(setting)], ConfigActor::Cli)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let apply = ApplyArgs {
        yes: apply.yes,
        confirm_high_risk: apply.confirm_high_risk,
    };
    finish(&service, plan, &apply, io).await
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
            model,
            apply,
        } => {
            let mutation = match model {
                Some(model) => ManagedChange::AddProviderWithModel { provider, model },
                None => ManagedChange::AddProvider { provider },
            };
            change(&service(config), mutation, &apply, io).await
        }
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
        if plan.summary().risk_reasons.is_empty() || plan.applies_at_full() {
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
/// argument flags (`init --api-key`, `go -k`, `auth paste-token --token`).
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
        ModelCommands::Catalog {
            catalog_command: crate::CatalogCommands::Status,
        } => {
            let release = providers::catalog::ReleaseCatalog::embedded_ref()?;
            writeln!(
                io.out,
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "source": "embedded", "revision": release.revision,
                    "content_sha256": release.content_sha256(), "providers": release.providers.len(),
                    "models": release.models.len(),
                    "active": release.models.iter().filter(|row| row.lifecycle.status == providers::catalog::ModelLifecycleStatus::Active).count(),
                    "deprecated": release.models.iter().filter(|row| row.lifecycle.status == providers::catalog::ModelLifecycleStatus::Deprecated).count(),
                    "retired": release.models.iter().filter(|row| row.lifecycle.status == providers::catalog::ModelLifecycleStatus::Retired).count(),
                    "updates": "releases only"
                }))?
            )?;
            Ok(())
        }
        ModelCommands::MigrateRetired { apply } => {
            for warning in openprx::config::manage::lifecycle::warnings(&config)? {
                writeln!(io.out, "{warning}")?;
            }
            change(&service(config), ManagedChange::MigrateRetired, &apply, io).await
        }
        ModelCommands::Levels { model } => {
            let release = providers::catalog::ReleaseCatalog::embedded_ref()?;
            let row = release
                .models
                .iter()
                .find(|row| row.id == model)
                .ok_or_else(|| anyhow::anyhow!("use a canonical release catalog model ID"))?;
            writeln!(
                io.out,
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "model": row.id, "levels": row.levels, "parameters": row.parameters,
                    "default_level": row.default_level
                }))?
            )?;
            Ok(())
        }
        ModelCommands::Settings {
            model,
            level,
            overrides,
            apply,
        } => {
            if level.is_none() && overrides.is_empty() {
                let release = providers::catalog::ReleaseCatalog::embedded_ref()?;
                if release.models.iter().any(|row| row.id == model && row.is_listed())
                    && let Some(settings) = config.model_settings.get(&model)
                {
                    settings.validate(&model)?;
                }
                writeln!(
                    io.out,
                    "{}",
                    serde_json::to_string_pretty(&config.model_settings.get(&model))?
                )?;
                return Ok(());
            }
            let settings = parse_model_settings(level, overrides)?;
            change(
                &service(config),
                ManagedChange::SetModelSettings { model, settings },
                &apply,
                io,
            )
            .await
        }

        ModelCommands::List {
            provider,
            operation: None,
            executable: false,
        } => openprx::providers::model_discovery::run_models_list(&config, provider.as_deref()),
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
            openprx::providers::model_discovery::run_models_refresh(&config, provider.as_deref(), force)
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
                level,
                overrides,
                provider,
                model,
                operation: op,
                keep_root_credential,
                clear_root_credential,
                apply,
            } => {
                let settings = if level.is_some() || !overrides.is_empty() {
                    Some(parse_model_settings(level, overrides)?)
                } else {
                    None
                };
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
                        settings,
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
    let warning = entry
        .lifecycle_warning
        .as_deref()
        .map(|warning| format!("  [{warning}]"))
        .unwrap_or_default();
    writeln!(
        out,
        "{}  {}  [{supported}]  ({}){warning}",
        entry.provider, entry.model, entry.source
    )?;
    Ok(())
}

fn print_model(entry: &manage::ModelEntry, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "Provider: {} ({})", entry.provider, entry.identity)?;
    writeln!(out, "Model: {}", entry.model)?;
    writeln!(out, "Evidence: {}", entry.source)?;
    if let Some(warning) = &entry.lifecycle_warning {
        writeln!(out, "Lifecycle: {warning}")?;
    }
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

