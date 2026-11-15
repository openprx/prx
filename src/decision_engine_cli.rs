//! `prx decision-engine`: the advisory engine's commands (P2-R8; strip
//! R43). `status` prints [`DecisionEngineStatus`] as the running daemon reports
//! it (process-local when no daemon answers), `analyses` the stored analyses,
//! `backends` the effective
//! engine backend plus the builtin, and `config set <key> <value>` changes one
//! allowlisted `[decision_engine]` key through the management service
//! (`config::manage::mutation::decision_engine`). No command takes or prints
//! a secret: credentials are set only through
//! `PUT /api/decision-engine/credential`. `prx causal` is retired and prints
//! one deprecation line.
//!
//! [`DecisionEngineStatus`]: crate::decision_engine::status::DecisionEngineStatus

use std::io::Write;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::config::manage::mutation::decision_engine::CausalMutation;
use crate::config::schema::decision_engine::{BackendKind, BackendLocality, CredentialRefSetting};

/// The one line `prx causal` prints (OV-7).
pub const CAUSAL_DEPRECATION: &str =
    "`prx causal` is retired: use `prx decision-engine` (status, backends, config set)";

/// `prx decision-engine` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum DecisionEngineCommands {
    /// Show whether the engine is enabled, its mode, backend and telemetry
    /// counters, as the running daemon reports them
    Status,
    /// List the newest analyses in the telemetry store
    Analyses {
        /// How many analyses to show (1 to 100)
        #[arg(long, default_value_t = DEFAULT_ANALYSES_LIMIT, value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
    },
    /// Show the effective engine backend and the builtin
    Backends,
    /// Change one allowlisted `[decision_engine]` key; the plan is shown and
    /// written only with --yes
    Config {
        #[command(subcommand)]
        command: DecisionEngineConfigCommands,
    },
}

/// `prx decision-engine config` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum DecisionEngineConfigCommands {
    /// Set `<key>` to `<value>`. Keys: enabled, mode, prompt_budget_tokens,
    /// candidates, timeout_ms, telemetry_cap, observe_queue, backend.kind,
    /// backend.endpoint, backend.model, backend.pinned_snapshot,
    /// backend.remote_enabled, backend.timeout_ms, backend.max_request_bytes,
    /// backend.max_response_bytes, backend.circuit_breaker.*
    Set {
        /// The `[decision_engine]` key
        key: String,
        /// Its new value
        value: String,
        #[command(flatten)]
        apply: DecisionEngineApplyArgs,
    },
}

/// How a `prx decision-engine config` change is applied: the same flags as
/// every `prx` management change.
#[derive(Debug, Clone, Args)]
pub struct DecisionEngineApplyArgs {
    /// Apply the plan (without it, the plan is shown and nothing is written)
    #[arg(long)]
    pub yes: bool,
    /// Confirm a high-risk plan; names the revision the plan reported
    #[arg(long, value_name = "REVISION", requires = "yes")]
    pub confirm_high_risk: Option<String>,
}

/// One backend as `prx decision-engine backends` and
/// `GET /api/decision-engine/backends` show it. No field carries a secret
/// or URL userinfo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendRow {
    pub kind: &'static str,
    /// `local` or `remote`.
    pub locality: &'static str,
    pub model: Option<String>,
    pub pinned_snapshot: Option<String>,
    pub credential_configured: bool,
    /// The breaker state a reader of the configuration sees: breakers are
    /// per engine process, so a fresh reader reports `closed`.
    pub circuit: &'static str,
}

/// The largest model or snapshot label shown.
const MAX_LABEL_CHARS: usize = 256;

fn bounded_label(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| {
        crate::providers::redact_endpoint_userinfo(text)
            .chars()
            .take(MAX_LABEL_CHARS)
            .collect()
    })
}

/// The effective engine backend first, then the builtin (at most two rows).
#[must_use]
pub fn backend_rows(config: &Config) -> Vec<BackendRow> {
    let backend = crate::config::schema::decision_engine::effective_engine_backend(config);
    let builtin = BackendRow {
        kind: BackendKind::Builtin.as_str(),
        locality: "local",
        model: Some(crate::tools::capability::turn_state::MODEL_VERSION.to_string()),
        pinned_snapshot: None,
        credential_configured: false,
        circuit: "closed",
    };
    let Some(selection) = crate::config::schema::decision_engine::configured_selection(backend) else {
        return vec![builtin];
    };
    let configured = BackendRow {
        kind: selection.kind.as_str(),
        locality: match selection.locality {
            BackendLocality::Builtin | BackendLocality::Loopback => "local",
            BackendLocality::Remote => "remote",
        },
        model: bounded_label(&backend.model),
        pinned_snapshot: bounded_label(&backend.pinned_snapshot),
        credential_configured: matches!(backend.credential_ref, CredentialRefSetting::Profile(_)),
        circuit: "closed",
    };
    vec![configured, builtin]
}

/// The default `prx decision-engine analyses --limit` (as the API).
pub const DEFAULT_ANALYSES_LIMIT: u16 = 20;

/// Where a `prx decision-engine status` row came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusSource {
    /// `GET /api/decision-engine/status` of the running daemon.
    Daemon,
    /// This process's own (empty) counters: no daemon answered.
    #[default]
    ProcessLocal,
}

/// `prx decision-engine status` as printed: the engine status fields plus
/// their source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusView {
    pub enabled: bool,
    pub mode: String,
    pub backend_kind: String,
    pub telemetry_cap: u32,
    pub queue_depth: usize,
    pub dropped: u64,
    pub store_errors: u64,
    pub analyses: u64,
    /// Never sent by the daemon; set by the reader.
    #[serde(skip_deserializing)]
    pub source: StatusSource,
}

impl From<crate::decision_engine::status::DecisionEngineStatus> for StatusView {
    fn from(status: crate::decision_engine::status::DecisionEngineStatus) -> Self {
        Self {
            enabled: status.enabled,
            mode: status.mode.to_string(),
            backend_kind: status.backend_kind.to_string(),
            telemetry_cap: status.telemetry_cap,
            queue_depth: status.queue_depth,
            dropped: status.dropped,
            store_errors: status.store_errors,
            analyses: status.analyses,
            source: StatusSource::ProcessLocal,
        }
    }
}

/// The engine status from the running daemon (endpoint and operator token as
/// `prx tasks` and chat resolve them); when no daemon answers, this process's
/// snapshot plus the stderr note saying so.
///
/// # Errors
///
/// The daemon refused the credential (401/403), answered another non-2xx, or
/// sent a body that is not a status.
pub async fn status_report(config: &Config) -> Result<(StatusView, Option<String>)> {
    use crate::runtime::tasks_cli::{DaemonRead, fetch_short};
    let endpoint = crate::chat::sessions::daemon::endpoint(config);
    match fetch_short::<StatusView>(&endpoint, "/api/decision-engine/status", "decision engine status").await? {
        DaemonRead::Answered(mut view) => {
            view.source = StatusSource::Daemon;
            Ok((view, None))
        }
        DaemonRead::Unreachable(reason) => {
            let note = format!(
                "daemon unreachable at {}: {reason}; showing process-local counters",
                endpoint.shown_url()
            );
            Ok((crate::decision_engine::status::snapshot(config).into(), Some(note)))
        }
    }
}

/// Print `prx decision-engine status`: the row on `out`, the fallback note
/// (if any) on `err`.
///
/// # Errors
///
/// As [`status_report`], or when the output cannot be written.
pub async fn run_status(config: &Config, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> Result<()> {
    let (status, note) = status_report(config).await?;
    if let Some(note) = note {
        writeln!(err, "{note}")?;
    }
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&status)?)?;
    } else {
        writeln!(
            out,
            "enabled: {}\nmode: {}\nbackend: {}\ntelemetry cap: {}\nqueue depth: {}\ndropped: {}\nstore errors: {}\nanalyses: {}\nsource: {}",
            status.enabled,
            status.mode,
            status.backend_kind,
            status.telemetry_cap,
            status.queue_depth,
            status.dropped,
            status.store_errors,
            status.analyses,
            match status.source {
                StatusSource::Daemon => "daemon",
                StatusSource::ProcessLocal => "process-local",
            }
        )?;
    }
    Ok(())
}

/// One stored analysis as `prx decision-engine analyses` and
/// `GET /api/decision-engine/analyses` show it: the redacted brief and the
/// typed values only.
#[derive(Debug, Serialize)]
pub struct AnalysisView {
    pub decision_id: String,
    pub created_at_ms: i64,
    pub brief_redacted: String,
    pub brief_sha256: String,
    pub global: serde_json::Value,
    pub candidates: serde_json::Value,
}

impl From<crate::decision_engine::AnalysisRecord> for AnalysisView {
    fn from(row: crate::decision_engine::AnalysisRecord) -> Self {
        // The store wrote `values_json` itself under its CHECK bound; a row
        // that does not parse shows empty values rather than failing the
        // whole read.
        let values: serde_json::Value = serde_json::from_str(row.values_json()).unwrap_or(serde_json::Value::Null);
        Self {
            decision_id: row.decision_id().to_string(),
            created_at_ms: row.created_at_ms(),
            brief_redacted: row.brief_redacted().to_string(),
            brief_sha256: row.brief_sha256().to_string(),
            global: values.get("global").cloned().unwrap_or_else(|| serde_json::json!({})),
            candidates: values
                .get("candidates")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        }
    }
}

/// The newest `limit` analyses of the workspace telemetry store, newest
/// first. A missing store reads as empty and nothing is created.
///
/// # Errors
///
/// When the store cannot be read.
pub fn read_analyses(config: &Config, limit: u16) -> Result<Vec<AnalysisView>> {
    use crate::decision_engine::observe::{SqliteTelemetryWriter, TELEMETRY_FILE};
    let path = config.workspace_dir.join(TELEMETRY_FILE);
    let Some(store) = SqliteTelemetryWriter::open_existing(&path)? else {
        return Ok(Vec::new());
    };
    Ok(store
        .analyses(usize::from(limit))?
        .into_iter()
        .map(AnalysisView::from)
        .collect())
}

/// Run a read-only local `prx decision-engine` command (`analyses`,
/// `backends`) over `config`, writing to `out`.
///
/// # Errors
///
/// When the output cannot be written.
pub fn run_read(config: &Config, json: bool, command: &DecisionEngineCommands, out: &mut dyn Write) -> Result<()> {
    match command {
        DecisionEngineCommands::Status => {
            anyhow::bail!("`prx decision-engine status` asks the daemon first: use `run_status`")
        }
        DecisionEngineCommands::Analyses { limit } => {
            let rows = read_analyses(config, *limit)?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
            } else if rows.is_empty() {
                writeln!(out, "no analyses recorded")?;
            } else {
                for row in rows {
                    writeln!(out, "{} {} global={}", row.created_at_ms, row.decision_id, row.global)?;
                }
            }
        }
        DecisionEngineCommands::Backends => {
            let rows = backend_rows(config);
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
            } else {
                for row in rows {
                    writeln!(
                        out,
                        "{} ({}) model={} snapshot={} credential={} circuit={}",
                        row.kind,
                        row.locality,
                        row.model.as_deref().unwrap_or("-"),
                        row.pinned_snapshot.as_deref().unwrap_or("-"),
                        if row.credential_configured {
                            "configured"
                        } else {
                            "none"
                        },
                        row.circuit
                    )?;
                }
            }
        }
        DecisionEngineCommands::Config { .. } => {
            anyhow::bail!("`prx decision-engine config` runs through the management service")
        }
    }
    Ok(())
}

/// A backend kind setup (`prx init --decision-backend`) can choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackendKindArg {
    /// The local deterministic backend: nothing leaves PRX
    Builtin,
    /// A System One compatible endpoint (loopback, or remote with
    /// enable-remote)
    #[value(name = "compatible_http", alias = "compatible-http")]
    CompatibleHttp,
    /// The hosted TypeSafe service (remote; needs a `cred-` reference)
    #[value(name = "typesafe")]
    TypeSafe,
    /// The hosted OpenRouter Decisions API (remote; needs a `cred-`
    /// reference and a pinned snapshot)
    #[value(name = "openrouter")]
    OpenRouter,
}

impl From<BackendKindArg> for BackendKind {
    fn from(kind: BackendKindArg) -> Self {
        match kind {
            BackendKindArg::Builtin => Self::Builtin,
            BackendKindArg::CompatibleHttp => Self::CompatibleHttp,
            BackendKindArg::TypeSafe => Self::TypeSafe,
            BackendKindArg::OpenRouter => Self::OpenRouter,
        }
    }
}

/// A decision backend chosen during setup (`prx init --decision-*`, the
/// onboarding step and its quick-setup flags; CDP-14). A hosted service is
/// refused here: it needs a credential, which the Gateway credential route or
/// the interactive onboarding stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionSetup {
    pub kind: BackendKind,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    /// Turn the advisory decision engine on too (`[decision_engine] enabled`).
    pub enable: bool,
}

impl DecisionSetup {
    /// The mutations of this choice, through the one mutation service.
    ///
    /// # Errors
    ///
    /// A hosted kind.
    pub fn mutations(&self) -> Result<Vec<CausalMutation>> {
        if matches!(self.kind, BackendKind::TypeSafe | BackendKind::OpenRouter) {
            anyhow::bail!(
                "the decision backend `{}` is a hosted service, which needs a credential: choose it with \
                 `prx decision-engine config set backend.kind` (with the credential stored through \
                 `PUT /api/decision-engine/credential`) or `prx init --interactive`",
                self.kind.as_str()
            );
        }
        let mut mutations = vec![CausalMutation::SetBackend {
            kind: self.kind,
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            pinned_snapshot: None,
            timeout_ms: None,
        }];
        if self.enable {
            mutations.push(CausalMutation::SetEngine(
                crate::config::manage::mutation::decision_engine::EngineSetting::Enabled(true),
            ));
        }
        Ok(mutations)
    }

    /// Check this choice before anything is written: the section rules and
    /// the decision backend check, over a default configuration. Nothing
    /// is read but the configuration.
    ///
    /// # Errors
    ///
    /// A hosted kind, or the refusal of the rules or the check.
    pub fn check(&self) -> Result<()> {
        let defaults = Config::default();
        let mutations = crate::config::manage::mutation::decision_engine::with_recommended_latency_budget(
            &defaults,
            self.mutations()?,
        );
        let candidate =
            crate::config::manage::mutation::decision_engine::CausalConfigService::mutate(&defaults, &mutations)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
        crate::config::manage::validate::check_decision_plane(None, &candidate)
            .map_err(|refusal| anyhow::anyhow!(refusal))?;
        Ok(())
    }

    fn ensure_hosted_credential_free_in(
        &self,
        references: &crate::config::manage::credentials::CredentialReferences,
    ) -> Result<String> {
        use crate::config::manage::credentials::CredentialReferences;
        let id = CredentialReferences::id_for(&format!("decision-{}", self.kind.as_str()))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        // `set` creates or replaces. Setup must never overwrite (and, on a
        // refusal, then delete) a credential an operator already stored under
        // the documented name, so an existing one stops setup.
        if references
            .status(&id)
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .is_some()
        {
            anyhow::bail!(
                "the credential reference {id} already exists and setup never replaces it: keep it by setting \
                 `backend.kind {kind}` and `backend.model {model}` with `prx decision-engine config set` and \
                 referencing {id}, rotate it with `PUT /api/decision-engine/credential`, or remove it first with \
                 `prx providers credential remove {id}`",
                kind = self.kind.as_str(),
                model = self.model.as_deref().unwrap_or("<exact version>"),
            );
        }
        Ok(id)
    }

    /// Refuse a hosted setup whose credential reference already exists in the
    /// store next to `config_path`, before the base configuration is written.
    ///
    /// # Errors
    ///
    /// A kind that is not hosted, an unreadable store, or an existing reference.
    pub fn ensure_hosted_credential_free(&self, config_path: &std::path::Path) -> Result<()> {
        anyhow::ensure!(
            hosted_owner(self.kind).is_some(),
            "the decision backend `{}` is not a hosted service",
            self.kind.as_str()
        );
        let config = Config {
            config_path: config_path.to_path_buf(),
            ..Config::default()
        };
        let references = crate::config::manage::credentials::CredentialReferences::for_config(&config)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        self.ensure_hosted_credential_free_in(&references).map(|_| ())
    }

    /// Check a hosted choice before anything is written: the kind is hosted, a
    /// model is given, and the section rules accept the backend with a
    /// credential reference (an openrouter model absent from the release
    /// catalog still needs a pinned snapshot, so it is refused here).
    ///
    /// # Errors
    ///
    /// A kind that is not hosted, a missing model, or the refusal of the rules.
    pub fn check_hosted(&self) -> Result<()> {
        anyhow::ensure!(
            hosted_owner(self.kind).is_some(),
            "the decision backend `{}` is not a hosted service",
            self.kind.as_str()
        );
        let model = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .context("a hosted decision backend needs --decision-model, one exact version")?;
        let mutations = vec![
            CausalMutation::SetBackend {
                kind: self.kind,
                endpoint: self.endpoint.clone(),
                model: Some(model.to_string()),
                pinned_snapshot: None,
                timeout_ms: None,
            },
            CausalMutation::SetCredentialRef(CredentialRefSetting::Profile(format!(
                "cred-decision-{}",
                self.kind.as_str()
            ))),
        ];
        let defaults = Config::default();
        let mutations =
            crate::config::manage::mutation::decision_engine::with_recommended_latency_budget(&defaults, mutations);
        crate::config::manage::mutation::decision_engine::CausalConfigService::mutate(&defaults, &mutations)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(())
    }

    /// Apply this choice to the configuration written at `config`'s path,
    /// through the management service, as one recorded transaction, and
    /// return the plan text.
    ///
    /// The plan is checked against that written configuration (D-105 K-4).
    /// The operator's setup flag confirms exactly one high-risk reason: the
    /// new decision endpoint, when it is a literal loopback address (the
    /// endpoint and its model declaration). Any other high-risk reason is
    /// refused, naming the `prx decision-engine config set` command that confirms it
    /// (D-105 K-1): setup never confirms more than a local endpoint.
    ///
    /// # Errors
    ///
    /// A hosted kind, a high-risk reason setup does not confirm, or the
    /// refusal of the plan or the transaction. Every error says that the base
    /// configuration was written and the decision backend was not.
    pub async fn apply(&self, config: Config) -> Result<String> {
        self.apply_written(config).await.map_err(|error| {
            anyhow::anyhow!(
                "the base configuration was written, but the decision backend was not configured: {error:#}; \
                 configure it with `prx decision-engine config set backend.kind {}`",
                self.kind.as_str()
            )
        })
    }

    /// Apply a hosted choice (`typesafe` or `openrouter`) together with its
    /// credential, as setup: store `secret` as the credential reference
    /// `cred-decision-<kind>` of the hosted provider, then plan the backend
    /// and that reference as one recorded transaction, like [`Self::apply`].
    /// Remote processing stays off (decision state does not leave this machine
    /// until `backend.remote_enabled` is set, which setup never does); the
    /// returned text says so. A refusal removes the credential this call
    /// stored, so nothing is left behind unreferenced.
    ///
    /// # Errors
    ///
    /// A kind that is not hosted, a missing model, a store failure, a
    /// high-risk reason setup does not confirm, or the refusal of the plan or
    /// the transaction. Every error says that the base configuration was
    /// written and the decision backend was not.
    pub async fn apply_hosted(
        &self,
        config: Config,
        secret: crate::config::manage::credentials::SecretString,
    ) -> Result<String> {
        self.apply_hosted_written(config, secret).await.map_err(|error| {
            anyhow::anyhow!(
                "the base configuration was written, but the hosted decision backend was not configured: \
                 {error:#}; configure it with `prx decision-engine config set backend.kind {}`",
                self.kind.as_str()
            )
        })
    }

    async fn apply_hosted_written(
        &self,
        config: Config,
        secret: crate::config::manage::credentials::SecretString,
    ) -> Result<String> {
        use crate::config::manage::credentials::CredentialReferences;
        let owner = hosted_owner(self.kind)
            .with_context(|| format!("the decision backend `{}` is not a hosted service", self.kind.as_str()))?;
        let model = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .context("a hosted decision backend needs --decision-model, one exact version")?;
        anyhow::ensure!(
            !secret.expose().trim().is_empty(),
            "the decision credential is empty; nothing was stored"
        );
        let references = CredentialReferences::for_config(&config).map_err(|error| anyhow::anyhow!("{error}"))?;
        let name = format!("decision-{}", self.kind.as_str());
        let id = self.ensure_hosted_credential_free_in(&references)?;
        references
            .set(owner, &name, secret.expose())
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        drop(secret);
        let applied = async {
            let mut mutations = vec![
                CausalMutation::SetBackend {
                    kind: self.kind,
                    endpoint: self.endpoint.clone(),
                    model: Some(model.to_string()),
                    pinned_snapshot: None,
                    timeout_ms: None,
                },
                CausalMutation::SetCredentialRef(CredentialRefSetting::Profile(id.clone())),
            ];
            if self.enable {
                mutations.push(CausalMutation::SetEngine(
                    crate::config::manage::mutation::decision_engine::EngineSetting::Enabled(true),
                ));
            }
            self.apply_mutations(config, mutations, true).await
        }
        .await;
        match applied {
            Ok(text) => Ok(format!(
                "{text}\nDecision state stays on this machine until you run `prx decision-engine config set \
                 backend.remote_enabled true`."
            )),
            Err(error) => {
                if let Err(removal) = references.remove(&id) {
                    tracing::warn!(reference = %id, "the decision credential stored for a refused setup could not be removed: {removal}");
                }
                Err(error)
            }
        }
    }

    async fn apply_written(&self, config: Config) -> Result<String> {
        let mutations = self.mutations()?;
        self.apply_mutations(config, mutations, false).await
    }

    async fn apply_mutations(&self, config: Config, mutations: Vec<CausalMutation>, hosted: bool) -> Result<String> {
        // Setup writes the catalog's latency recommendation explicitly, in the
        // same transaction, when the written budget is lower.
        let mutations =
            crate::config::manage::mutation::decision_engine::with_recommended_latency_budget(&config, mutations);
        let service = crate::config::manage::ManagementService::deferred(crate::config::new_shared(config));
        let plan = service
            .plan_causal(&mutations, crate::config::manage::ConfigActor::Cli)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let text = plan.text();
        let revision = plan.revision().map(str::to_string);
        if !self.confirms(&plan.summary().risk_reasons, hosted) {
            anyhow::bail!(
                "the change is high risk beyond a local decision endpoint ({}), and setup never confirms it: review \
                 it with `prx decision-engine config set backend.kind {}`{}{} and apply it with `--yes \
                 --confirm-high-risk {}`",
                plan.summary()
                    .risk_reasons
                    .iter()
                    .map(crate::config::manage::RiskReason::describe)
                    .collect::<Vec<_>>()
                    .join("; "),
                self.kind.as_str(),
                self.endpoint
                    .as_deref()
                    .map(|endpoint| format!(", `backend.endpoint {endpoint}`"))
                    .unwrap_or_default(),
                self.model
                    .as_deref()
                    .map(|model| format!(", `backend.model {model}`"))
                    .unwrap_or_default(),
                revision.as_deref().unwrap_or("<REVISION>")
            );
        }
        let confirmed = (!plan.summary().risk_reasons.is_empty())
            .then_some(revision.as_deref())
            .flatten();
        service
            .apply(plan, crate::config::manage::ConfigActor::Cli, confirmed)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(text)
    }

    /// Whether setup may confirm `reasons` itself: none, or only the new
    /// decision endpoint (and the model declaration naming it) when that
    /// endpoint is a literal loopback address, plus (hosted setup only) the
    /// reference to the credential setup just stored.
    fn confirms(&self, reasons: &[crate::config::manage::RiskReason], hosted: bool) -> bool {
        let loopback = self
            .endpoint
            .as_deref()
            .is_some_and(crate::providers::loopback::is_literal_loopback);
        reasons.iter().all(|reason| match reason {
            crate::config::manage::RiskReason::EndpointChanged { path } => {
                loopback
                    && (path == "decision_engine.backend.endpoint"
                        || (path.starts_with("providers.models[") && path.ends_with("].provider")))
            }
            // The hosted flow stored this credential itself, one step before.
            crate::config::manage::RiskReason::CredentialChanged { path } => {
                hosted && path == "decision_engine.backend.credential_ref"
            }
            _ => false,
        })
    }
}

/// The provider a hosted kind's credential must be issued for: the owner
/// the decision check's resolver requires (`typesafe` is
/// `decision:typesafe`); `None` for the other kinds.
#[must_use]
pub const fn hosted_owner(kind: BackendKind) -> Option<&'static str> {
    match kind {
        BackendKind::TypeSafe => Some("typesafe"),
        BackendKind::OpenRouter => Some("openrouter"),
        _ => None,
    }
}

/// The two commands that set up the hosted `kind`: store its credential,
/// owned by `owner`, then choose the backend with that reference.
#[must_use]
pub fn hosted_setup_steps(kind: BackendKind, owner: &str, model: Option<&str>) -> String {
    let name = format!("decision-{}", kind.as_str());
    let catalog_supplies_snapshot = model.is_some_and(|model| {
        crate::providers::catalog::ReleaseCatalog::embedded_ref()
            .ok()
            .and_then(|catalog| catalog.model(&crate::providers::ProviderId::of(owner), model))
            .and_then(|entry| entry.pinned_snapshot.as_deref())
            .is_some()
    });
    let snapshot = if kind == BackendKind::OpenRouter && !catalog_supplies_snapshot {
        ", `backend.pinned_snapshot <exact dated snapshot>`"
    } else {
        ""
    };
    format!(
        "the hosted decision backend `{kind}` needs its own credential reference: first `prx providers credential \
         set {owner} --name {name}` (the value is read hidden, or with --from-stdin/--from-env), then set \
         `backend.kind {kind}`, `backend.model {model}`{snapshot} with `prx decision-engine config set` and \
         reference cred-{name}",
        kind = kind.as_str(),
        model = model.unwrap_or("<exact version>"),
    )
}

#[cfg(test)]
#[path = "decision_engine_cli/tests.rs"]
mod tests;
