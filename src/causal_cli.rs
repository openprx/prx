//! `prx causal`: the decision plane's commands (SP-CDP-A 5.13; CDP-11,
//! CDP-14).
//!
//! `status`, `backends`, `test`, `inspect` and `evaluate` run over the shared
//! admin service (`crate::causal_tree::admin`). `test` is local unless the
//! operator asks for one live call and confirms it (`--live --yes`,
//! PMM-D-7). The `config` commands ([`CausalConfigCommands`]) are run by the
//! management CLI of the `prx` binary over the shared management service:
//! the same plan text, the same `--yes`/`--confirm-high-risk` apply and the
//! same credential input as `prx providers`. No subcommand takes a secret
//! argument; every output is built from redacted DTOs.

use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};

use crate::causal_tree::admin::{DecisionAdmin, DecisionPlaneBuilder, TestMode, TestRequest, test_target};
use crate::causal_tree::config::CredentialRefSetting;
use crate::causal_tree::ids::DecisionId;
use crate::causal_tree::store::TimeWindow;
use crate::config::Config;
use crate::config::manage::mutation::causal::{CausalMutation, PolicyPatch};
use crate::decision::BackendKind;
use crate::decision::compatible_http::HttpDecisionTransports;
use crate::decision::factory::{DecisionBackendFactory, FactoryBuildError, FactoryDeps};
use crate::decision::telemetry::{CausalEventSink, ObserverCausalSink};
use crate::providers::catalog::CatalogView;

/// Which backend `prx causal test` asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TestBackend {
    /// The configured backend (the default).
    Configured,
    /// The builtin backend only.
    Builtin,
}

/// `prx causal` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum CausalCommands {
    #[command(flatten)]
    Read(CausalReadCommands),
    /// Change the decision plane's configuration: every change prints its
    /// plan and is written only with --yes (a high-risk one also needs
    /// `--confirm-high-risk <REVISION>`)
    Config {
        #[command(subcommand)]
        command: CausalConfigCommands,
    },
}

/// How a `prx causal config` change is applied: the same flags as every
/// `prx` management change.
#[derive(Debug, Clone, Args)]
pub struct CausalApplyArgs {
    /// Apply the plan (without it, the plan is shown and nothing is written)
    #[arg(long)]
    pub yes: bool,
    /// Confirm a high-risk plan; names the revision the plan reported
    #[arg(long, value_name = "REVISION", requires = "yes")]
    pub confirm_high_risk: Option<String>,
}

/// A backend kind `prx causal config set-backend` can choose.
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

/// `prx causal config` subcommands. None takes a secret: a credential is
/// read by `set-credential` from standard input, an environment variable or
/// a hidden prompt, or stored with `prx providers credential set` and named
/// by its `cred-` reference.
#[derive(Debug, Clone, Subcommand)]
pub enum CausalConfigCommands {
    /// Turn the decision plane on
    Enable {
        #[command(flatten)]
        apply: CausalApplyArgs,
    },
    /// Turn the decision plane off: the builtin serves every decision and
    /// nothing is sent or recorded (the rollback path)
    Disable {
        #[command(flatten)]
        apply: CausalApplyArgs,
    },
    /// Choose the decision backend and declare its model for the decision
    /// endpoint. Switching to the builtin also turns remote processing off
    Set {
        #[command(subcommand)]
        what: CausalSetCommands,
    },
    /// Allow decision state to leave this machine (high risk)
    EnableRemote {
        #[command(flatten)]
        apply: CausalApplyArgs,
    },
    /// Keep decision state on this machine
    DisableRemote {
        #[command(flatten)]
        apply: CausalApplyArgs,
    },
    /// Copy the thresholds an evaluation report proposes (it must have been
    /// measured on the configured, pinned backend)
    ApplyCalibration {
        /// The report id `prx causal evaluate` printed
        #[arg(long)]
        report: String,
        #[command(flatten)]
        apply: CausalApplyArgs,
    },
    /// Put every decision policy setting back to its default
    ResetPolicy {
        #[command(flatten)]
        apply: CausalApplyArgs,
    },
}

/// `prx causal config set …`.
#[derive(Debug, Clone, Subcommand)]
pub enum CausalSetCommands {
    /// Choose the backend kind, endpoint, model and timeout
    Backend {
        /// The backend kind
        #[arg(long, value_enum)]
        kind: BackendKindArg,
        /// The endpoint (compatible_http: required; hosted: optional base URL
        /// override, the backend stays remote)
        #[arg(long)]
        endpoint: Option<String>,
        /// The exact model version
        #[arg(long)]
        model: Option<String>,
        /// The exact snapshot a hosted service answers with (openrouter:
        /// required)
        #[arg(long)]
        pinned_snapshot: Option<String>,
        /// The transport timeout in milliseconds
        #[arg(long)]
        timeout_ms: Option<u32>,
        /// The backend's `cred-` reference (from `prx providers credential
        /// set`), or `none`; never a secret
        #[arg(long, value_name = "REFERENCE")]
        reference: Option<String>,
        #[command(flatten)]
        apply: CausalApplyArgs,
    },
    /// Store a new credential for the configured backend and reference it,
    /// in one high-risk change (the previous decision credential is removed)
    Secret {
        /// Read it from standard input
        #[arg(long, conflicts_with = "from_env")]
        from_stdin: bool,
        /// Read it from this environment variable
        #[arg(long, value_name = "VAR")]
        from_env: Option<String>,
        #[command(flatten)]
        apply: CausalApplyArgs,
    },
    /// Change explicit decision policy settings
    Policy {
        #[arg(long)]
        max_candidates: Option<u8>,
        #[arg(long)]
        max_decision_hops: Option<u8>,
        #[arg(long)]
        extra_latency_budget_ms: Option<u64>,
        #[arg(long)]
        commit_threshold: Option<f32>,
        #[arg(long)]
        minimum_choice_confidence: Option<f64>,
        #[arg(long)]
        minimum_top_margin: Option<f64>,
        /// `causal_tree.policy.extra_token_ratio_limit` (the flag avoids a
        /// secret-shaped name)
        #[arg(long)]
        extra_usage_ratio_limit: Option<f32>,
        #[command(flatten)]
        apply: CausalApplyArgs,
    },
}

/// What a `prx causal config` command does.
#[derive(Debug)]
pub enum CausalConfigChange {
    /// Plan these mutations and apply them with these flags.
    Plan {
        mutations: Vec<CausalMutation>,
        apply: CausalApplyArgs,
    },
    /// Store a new decision credential (`set secret`), read only once the
    /// change is confirmed.
    Secret {
        from_stdin: bool,
        from_env: Option<String>,
        apply: CausalApplyArgs,
    },
}

/// The change `command` makes over `config`, through the one mutation
/// service's vocabulary. Switching to the builtin while remote processing is
/// on also turns it off, in the same plan (D-094 C-6): the decision check
/// refuses the pair otherwise.
///
/// # Errors
///
/// A `--reference` that is not `""`, `none` or a `cred-` id.
pub fn config_change(command: CausalConfigCommands, config: &Config) -> Result<CausalConfigChange> {
    use CausalConfigChange::Plan;
    let plan = |mutation: CausalMutation, apply: CausalApplyArgs| Plan {
        mutations: vec![mutation],
        apply,
    };
    Ok(match command {
        CausalConfigCommands::Enable { apply } => plan(CausalMutation::SetEnabled(true), apply),
        CausalConfigCommands::Disable { apply } => plan(CausalMutation::SetEnabled(false), apply),
        CausalConfigCommands::EnableRemote { apply } => plan(CausalMutation::SetRemote(true), apply),
        CausalConfigCommands::DisableRemote { apply } => plan(CausalMutation::SetRemote(false), apply),
        CausalConfigCommands::ApplyCalibration { report, apply } => {
            plan(CausalMutation::ApplyCalibration { report_id: report }, apply)
        }
        CausalConfigCommands::ResetPolicy { apply } => plan(CausalMutation::ResetPolicyToDefaults, apply),
        CausalConfigCommands::Set { what } => match what {
            CausalSetCommands::Backend {
                kind,
                endpoint,
                model,
                pinned_snapshot,
                timeout_ms,
                reference,
                apply,
            } => {
                let kind = BackendKind::from(kind);
                // A hosted service takes only its own `cred-` reference; the
                // first setup names it in the same change (D-040 OQ-A4).
                let referenced = reference.is_some()
                    || matches!(
                        config.causal_tree.backend.credential_ref,
                        CredentialRefSetting::Profile(_)
                    );
                if let Some(owner) = hosted_owner(kind)
                    && !referenced
                {
                    anyhow::bail!("{}", hosted_setup_steps(kind, owner, model.as_deref()));
                }
                let mut mutations = vec![CausalMutation::SetBackend {
                    kind,
                    endpoint,
                    model,
                    pinned_snapshot,
                    timeout_ms,
                }];
                if let Some(reference) = reference {
                    let setting = CredentialRefSetting::try_from(reference).map_err(|error| anyhow::anyhow!(error))?;
                    mutations.push(CausalMutation::SetCredentialRef(setting));
                }
                Plan {
                    mutations: crate::config::manage::mutation::causal::paired(config, mutations),
                    apply,
                }
            }
            CausalSetCommands::Policy {
                max_candidates,
                max_decision_hops,
                extra_latency_budget_ms,
                commit_threshold,
                minimum_choice_confidence,
                minimum_top_margin,
                extra_usage_ratio_limit,
                apply,
            } => plan(
                CausalMutation::SetPolicy(PolicyPatch {
                    max_candidates,
                    max_decision_hops,
                    extra_latency_budget_ms,
                    commit_threshold,
                    minimum_choice_confidence,
                    minimum_top_margin,
                    extra_token_ratio_limit: extra_usage_ratio_limit,
                }),
                apply,
            ),
            CausalSetCommands::Secret {
                from_stdin,
                from_env,
                apply,
            } => CausalConfigChange::Secret {
                from_stdin,
                from_env,
                apply,
            },
        },
    })
}

/// The admin service over the production decision plane (the HTTP
/// transports, the configuration's catalog) with `events` as its causal
/// event sink: what `prx causal` and the Gateway's `/api/causal` read and
/// test through.
pub(crate) fn http_admin(config: Config, events: Arc<dyn CausalEventSink>) -> DecisionAdmin {
    let plane = Arc::new(HttpPlane {
        events: Arc::clone(&events),
    });
    DecisionAdmin::new(config, plane, events)
}

/// Ask the configured backend the neutral fixture locally (loopback and
/// builtin only; a remote backend is not asked), as `prx causal test` does,
/// and return the outcome word (`passed`, `fell_back`, `requires_live`,
/// ...). The onboarding step runs it right after setup.
///
/// # Errors
///
/// When the fixture cannot be sealed.
pub async fn test_configured_locally(config: &Config) -> Result<&'static str> {
    let observer: Arc<dyn crate::observability::Observer> =
        Arc::from(crate::observability::create_observer(&config.observability));
    let events: Arc<dyn CausalEventSink> = Arc::new(ObserverCausalSink::new(Arc::clone(&observer)));
    let plane = Arc::new(HttpPlane {
        events: Arc::clone(&events),
    });
    let admin = DecisionAdmin::new(config.clone(), plane, events);
    let result = admin
        .test(TestRequest {
            target: test_target(None),
            mode: TestMode::Local,
        })
        .await;
    admin.close().await;
    observer.flush();
    Ok(result?.outcome)
}

/// A decision backend chosen during setup (`prx init --decision-*`, the
/// onboarding step and its quick-setup flags; CDP-14). A hosted service is
/// refused here: it needs a credential, which `prx causal config` or the
/// interactive onboarding stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionSetup {
    pub kind: BackendKind,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    /// Turn the decision plane on too.
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
                 `prx causal config set backend` (with a `cred-` reference from `prx providers credential set`) \
                 or `prx onboard --interactive`",
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
            mutations.push(CausalMutation::SetEnabled(true));
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
        let mutations = self.mutations()?;
        let candidate =
            crate::config::manage::mutation::causal::CausalConfigService::mutate(&Config::default(), &mutations)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
        crate::config::manage::validate::check_decision_plane(None, &candidate)
            .map_err(|refusal| anyhow::anyhow!(refusal))?;
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
    /// refused, naming the `prx causal config` command that confirms it
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
                 configure it with `prx causal config set backend --kind {}`",
                self.kind.as_str()
            )
        })
    }

    async fn apply_written(&self, config: Config) -> Result<String> {
        let mutations = self.mutations()?;
        let service = crate::config::manage::ManagementService::deferred(crate::config::new_shared(config));
        let plan = service
            .plan_causal(&mutations, crate::config::manage::ConfigActor::Cli)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let text = plan.text();
        let revision = plan.revision().map(str::to_string);
        if !self.confirms(&plan.summary().risk_reasons) {
            anyhow::bail!(
                "the change is high risk beyond a local decision endpoint ({}), and setup never confirms it: review \
                 it with `prx causal config set backend --kind {}{}{}` and apply it with `--yes \
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
                    .map(|endpoint| format!(" --endpoint {endpoint}"))
                    .unwrap_or_default(),
                self.model
                    .as_deref()
                    .map(|model| format!(" --model {model}"))
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
    /// endpoint is a literal loopback address.
    fn confirms(&self, reasons: &[crate::config::manage::RiskReason]) -> bool {
        let loopback = self
            .endpoint
            .as_deref()
            .is_some_and(crate::decision::loopback::is_literal_loopback);
        reasons.iter().all(|reason| match reason {
            crate::config::manage::RiskReason::EndpointChanged { path } => {
                loopback
                    && (path == "causal_tree.backend.endpoint"
                        || (path.starts_with("providers.models[") && path.ends_with("].provider")))
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
    let snapshot = if kind == BackendKind::OpenRouter {
        " --pinned-snapshot <exact dated snapshot>"
    } else {
        ""
    };
    format!(
        "the hosted decision backend `{kind}` needs its own credential reference: first `prx providers credential \
         set {owner} --name {name}` (the value is read hidden, or with --from-stdin/--from-env), then `prx causal \
         config set backend --kind {kind} --model {model}{snapshot} --reference cred-{name}`",
        kind = kind.as_str(),
        model = model.unwrap_or("<exact version>"),
    )
}

/// The read-only `prx causal` subcommands, run by [`run`].
#[derive(Debug, Clone, Subcommand)]
pub enum CausalReadCommands {
    /// Show the decision plane: the backend order with health and circuit
    /// state, exclusions, the decision store and legacy configuration keys
    Status,
    /// Show the computed backend order with the reason each left-out backend
    /// is excluded
    Backends,
    /// Ask a backend the neutral test fixture (one question of each kind, no
    /// user state). Loopback and builtin backends run locally; a remote
    /// backend only with --live --yes (one call, which may be billed). Never
    /// writes the decision store or the configuration
    Test {
        /// The backend to ask
        #[arg(long, value_enum, default_value = "configured")]
        backend: TestBackend,
        /// Ask a remote backend once, live
        #[arg(long)]
        live: bool,
        /// Confirm the live call
        #[arg(long, requires = "live")]
        yes: bool,
    },
    /// Show one recorded decision and whether its outcome has joined
    Inspect {
        /// The decision id
        #[arg(long)]
        decision_id: String,
    },
    /// Evaluate recorded decisions against their outcomes and write a
    /// versioned report (never changes the configuration)
    Evaluate {
        /// First day (YYYY-MM-DD, UTC) or RFC 3339 time
        #[arg(long)]
        from: String,
        /// Last day (YYYY-MM-DD, UTC, inclusive) or RFC 3339 time (exclusive)
        #[arg(long)]
        to: String,
        /// Only this owner's decisions
        #[arg(long)]
        owner: Option<String>,
    },
}

/// Run `command` over `config`, writing to standard output.
///
/// # Errors
///
/// When the command fails; the message carries no secret.
pub async fn run(config: &Config, json: bool, command: CausalReadCommands) -> Result<()> {
    let observer: Arc<dyn crate::observability::Observer> =
        Arc::from(crate::observability::create_observer(&config.observability));
    let events: Arc<dyn CausalEventSink> = Arc::new(ObserverCausalSink::new(Arc::clone(&observer)));
    let plane = Arc::new(HttpPlane {
        events: Arc::clone(&events),
    });
    let admin = DecisionAdmin::new(config.clone(), plane, events);
    let mut stdout = std::io::stdout().lock();
    let result = execute(&admin, json, command, &mut stdout).await;
    admin.close().await;
    observer.flush();
    result
}

/// The production decision plane: the HTTP transports, the configuration's
/// catalog and the observer's causal event sink.
struct HttpPlane {
    events: Arc<dyn CausalEventSink>,
}

fn admin_prediction(result: crate::decision::factory::PredictionResult) -> crate::causal_tree::admin::AdminPrediction {
    crate::causal_tree::admin::AdminPrediction::from_validated_parts(
        result.served,
        result.served_by,
        result
            .attempts
            .into_iter()
            .map(|attempt| (attempt.identity.kind, attempt.outcome, attempt.latency)),
    )
}

impl DecisionPlaneBuilder for HttpPlane {
    fn check_health<'a>(
        &'a self,
        factory: &'a DecisionBackendFactory,
        config: &'a Config,
        key: Option<Arc<crate::decision::DecisionKey>>,
    ) -> crate::causal_tree::admin::AdminPlaneFuture<'a, bool> {
        Box::pin(async move {
            if !factory
                .status()
                .order
                .iter()
                .any(|slot| slot.locality == crate::decision::BackendLocality::Loopback)
            {
                factory.check_health().await;
                return Ok(true);
            }
            let key = key.ok_or_else(|| admin_accounting_error("probe accounting key unavailable"))?;
            let parent = crate::runtime::turn_cleanup::current_tracker();
            let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(
                parent.as_ref(),
                tokio_util::sync::CancellationToken::new(),
            )
            .map_err(|_| admin_accounting_error("probe cleanup owner unavailable"))?;
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_millis(config.causal_tree.policy.extra_latency_budget_ms);
            cleanup
                .run(async {
                    let accounting = admin_accounting_scope(config, factory, key, b"health-loopback", deadline).await?;
                    Ok(factory.check_health_with_accounting(Some(&accounting)).await)
                })
                .await
                .map_err(|_| admin_accounting_error("probe accounting or cleanup was not confirmed"))
        })
    }

    fn decide<'a>(
        &'a self,
        factory: &'a DecisionBackendFactory,
        config: &'a Config,
        key: Arc<crate::decision::DecisionKey>,
        request: &'a crate::decision::DecisionRequest,
        lineage: &'a crate::decision::telemetry::CausalLineage,
    ) -> crate::causal_tree::admin::AdminPlaneFuture<'a, crate::causal_tree::admin::AdminPrediction> {
        Box::pin(async move {
            if factory.status().order.is_empty() {
                return factory
                    .decide(request, lineage)
                    .await
                    .map(admin_prediction)
                    .map_err(|failure| admin_accounting_error(failure.0.class()));
            }
            let parent = crate::runtime::turn_cleanup::current_tracker();
            let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(
                parent.as_ref(),
                tokio_util::sync::CancellationToken::new(),
            )
            .map_err(|_| admin_accounting_error("test cleanup owner unavailable"))?;
            let deadline = request.manifest().expires_at().min(
                std::time::Instant::now()
                    + std::time::Duration::from_millis(config.causal_tree.policy.extra_latency_budget_ms),
            );
            cleanup
                .run(async {
                    let accounting = admin_accounting_scope(config, factory, key, b"configured-test", deadline).await?;
                    factory
                        .decide_with_accounting(request, lineage, Some(&accounting))
                        .await
                        .map_err(|failure| anyhow::anyhow!("{}", failure.0.class()))
                })
                .await
                .map(admin_prediction)
                .map_err(|_| admin_accounting_error("test accounting or cleanup was not confirmed"))
        })
    }

    fn build(
        &self,
        config: &Config,
        key: Option<Arc<crate::decision::DecisionKey>>,
    ) -> Result<DecisionBackendFactory, FactoryBuildError> {
        let catalog = CatalogView::from_config(config);
        DecisionBackendFactory::from_config(
            config,
            FactoryDeps {
                catalog: &catalog,
                key,
                transports: &HttpDecisionTransports,
                config_generation: 0,
                events: Arc::clone(&self.events),
            },
        )
    }
}

fn admin_accounting_error(reason: &str) -> crate::causal_tree::admin::AdminError {
    crate::causal_tree::admin::AdminError::Fixture(reason.to_owned())
}

/// Called only inside the authorized admin request's cleanup scope. No turn
/// source proof is minted and no request metadata can expand the budget.
async fn admin_accounting_scope(
    config: &Config,
    factory: &DecisionBackendFactory,
    key: Arc<crate::decision::DecisionKey>,
    operation: &[u8],
    deadline: std::time::Instant,
) -> Result<crate::runtime::decision_accounting::AccountingScope> {
    let cleanup =
        crate::runtime::turn_cleanup::current_tracker().context("admin accounting requires a request cleanup scope")?;
    let mut tokens = u64::MAX;
    for slot in factory.status().order {
        if operation == b"health-loopback" && slot.locality != crate::decision::BackendLocality::Loopback {
            continue;
        }
        let provider = slot
            .identity
            .provider
            .as_ref()
            .context("admin backend provider unavailable")?;
        let effective = crate::router::resolve_effective_compaction_config(
            &config.agent.compaction,
            provider.as_str(),
            &slot.identity.configured_model,
            &config.router,
            &config.model_routes,
        );
        let reservation = crate::runtime::turn_decision::DecisionTokenReservation::from_effective_compaction(
            &effective.config,
            config.causal_tree.policy.extra_token_ratio_limit,
        )?;
        tokens = tokens.min(u64::try_from(reservation.decision_tokens)?);
    }
    anyhow::ensure!(tokens != u64::MAX && tokens > 0, "admin decision allowance unavailable");
    let config = Arc::new(config.clone());
    let cost_config = Arc::clone(&config);
    let ticket = cleanup.register(tokio_util::sync::CancellationToken::new())?;
    let micro_usd = crate::runtime::blocking::spawn_blocking(move || {
        let result = crate::cost::tracker::remaining_micro_usd_read_only_with_security_ceiling(
            &cost_config.cost,
            &cost_config.workspace_dir,
            cost_config.autonomy.max_cost_per_day_cents,
        );
        // A failed read changes no accounting state; the actual operation has
        // finished even when it cannot provide an admission snapshot.
        ticket.finish(true);
        result
    })
    .await??;
    let scope_id = key.hmac(
        crate::decision::KeyDomain::TerminalClaim,
        &[
            b"admin-accounting-v1",
            uuid::Uuid::new_v4().as_bytes(),
            operation,
            &serde_json::to_vec(&config.causal_tree.backend)?,
        ],
    );
    let budgets = crate::causal_tree::state::TurnBudgets {
        tokens,
        latency_ms: config.causal_tree.policy.extra_latency_budget_ms,
        micro_usd,
    };
    Ok(crate::runtime::decision_accounting::AccountingScope::new(
        config, key, cleanup, scope_id, budgets, deadline,
    ))
}

/// What `prx causal test --live` prints before the confirmation.
#[derive(Debug, serde::Serialize)]
struct LiveNotice {
    outcome: &'static str,
    sent: bool,
    /// The provider the call would reach, URL userinfo redacted.
    target: String,
}

/// A time given as a day (`YYYY-MM-DD`, UTC) or an RFC 3339 instant, in
/// milliseconds; a day given as the end is inclusive.
pub(crate) fn parse_time(text: &str, end: bool) -> Result<i64> {
    let text = text.trim();
    if let Ok(day) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        let day = if end { day.succ_opt().unwrap_or(day) } else { day };
        return Ok(day.and_time(chrono::NaiveTime::MIN).and_utc().timestamp_millis());
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|time| time.timestamp_millis())
        .with_context(|| format!("`{text}` is not a day (YYYY-MM-DD) or an RFC 3339 time"))
}

fn emit<T: serde::Serialize>(
    out: &mut dyn Write,
    json: bool,
    value: &T,
    human: impl FnOnce(&mut dyn Write) -> Result<()>,
) -> Result<()> {
    if json {
        serde_json::to_writer_pretty(&mut *out, value)?;
        writeln!(out)?;
        Ok(())
    } else {
        human(out)
    }
}

async fn execute(admin: &DecisionAdmin, json: bool, command: CausalReadCommands, out: &mut dyn Write) -> Result<()> {
    match command {
        CausalReadCommands::Status => {
            let status = admin.status().await;
            emit(out, json, &status, |out| {
                writeln!(
                    out,
                    "Decision plane: {}",
                    if status.enabled { "enabled" } else { "disabled" }
                )?;
                writeln!(
                    out,
                    "Workspace key: {}",
                    if status.key_available {
                        "available"
                    } else {
                        "unavailable"
                    }
                )?;
                writeln!(out, "Order:")?;
                for (position, backend) in status.order.iter().enumerate() {
                    writeln!(
                        out,
                        "  {}. {} ({}) model={} circuit={} health={}{}{}",
                        position + 1,
                        backend.kind,
                        backend.locality,
                        backend.model,
                        backend.circuit,
                        backend.health,
                        backend
                            .last_failure
                            .map(|class| format!(" last_failure={class}"))
                            .unwrap_or_default(),
                        backend
                            .provider
                            .as_deref()
                            .map(|provider| format!(" provider={provider}"))
                            .unwrap_or_default()
                    )?;
                }
                for excluded in &status.excluded {
                    writeln!(
                        out,
                        "  excluded: {} ({}): {}",
                        excluded.kind, excluded.locality, excluded.reason
                    )?;
                }
                if let Some(error) = &status.factory_error {
                    writeln!(out, "  backend error: {error}")?;
                }
                let store = &status.store;
                writeln!(
                    out,
                    "Store: {} [{}] schema={} decisions={} (legacy {}) outcomes={}",
                    store.path,
                    store.mode,
                    store.schema_version,
                    store.decisions,
                    store.legacy_decisions,
                    store.outcomes
                )?;
                if let Some(import) = &store.v1_import {
                    writeln!(
                        out,
                        "  v1 import: files={} unchanged={} imported={} malformed={} dropped={}",
                        import.files, import.unchanged, import.imported, import.malformed, import.dropped
                    )?;
                }
                if let Some(error) = &store.error {
                    writeln!(out, "  store error: {error}")?;
                }
                let legacy = &status.legacy_keys;
                if !legacy.moved.is_empty() || !legacy.dropped.is_empty() {
                    writeln!(
                        out,
                        "Legacy keys: moved [{}], dropped [{}] (the next save writes the new form)",
                        legacy.moved.join(", "),
                        legacy.dropped.join(", ")
                    )?;
                }
                if let Some(error) = &legacy.error {
                    writeln!(out, "Legacy keys: {error}")?;
                }
                Ok(())
            })
        }
        CausalReadCommands::Backends => {
            let backends = admin.backends();
            emit(out, json, &backends, |out| {
                for (position, slot) in backends.order.iter().enumerate() {
                    writeln!(
                        out,
                        "{}. {} ({}){}",
                        position + 1,
                        slot.kind,
                        slot.locality,
                        slot.excluded
                            .map(|reason| format!(": excluded, {reason}"))
                            .unwrap_or_default()
                    )?;
                }
                Ok(())
            })
        }
        CausalReadCommands::Test { backend, live, yes } => {
            let kind = match backend {
                TestBackend::Configured => None,
                TestBackend::Builtin => Some(BackendKind::Builtin),
            };
            // PMM-D-7: a live call to a remote backend is asked for and
            // confirmed; without the confirmation nothing is sent.
            if live
                && !yes
                && backend == TestBackend::Configured
                && let Some(target) = admin.live_target()
            {
                let notice = LiveNotice {
                    outcome: "confirmation_required",
                    sent: false,
                    target,
                };
                return emit(out, json, &notice, |out| {
                    writeln!(
                        out,
                        "A live test sends one decision request with the neutral fixture (no user state) to {}; \
                         it may be billed. Nothing was sent. Confirm with --live --yes",
                        notice.target
                    )?;
                    Ok(())
                });
            }
            let view = admin
                .test(TestRequest {
                    target: test_target(kind),
                    mode: if live && yes { TestMode::Live } else { TestMode::Local },
                })
                .await?;
            emit(out, json, &view, |out| {
                writeln!(
                    out,
                    "Test: {}{}",
                    view.outcome,
                    view.served_by
                        .map(|served| format!(" (served by {served})"))
                        .unwrap_or_default()
                )?;
                for attempt in &view.attempts {
                    writeln!(
                        out,
                        "  {}: {}{} in {} ms",
                        attempt.backend,
                        attempt.outcome,
                        attempt
                            .error_class
                            .map(|class| format!(" ({class})"))
                            .unwrap_or_default(),
                        attempt.latency_ms
                    )?;
                }
                if !view.answered.is_empty() {
                    writeln!(out, "  answered: {}", view.answered.join(", "))?;
                }
                if let Some(note) = &view.note {
                    writeln!(out, "  {note}")?;
                }
                Ok(())
            })
        }
        CausalReadCommands::Inspect { decision_id } => {
            let id = DecisionId::parse(&decision_id).context("`--decision-id` is not a decision id")?;
            // An unknown id and one outside the scope read the same.
            let view = admin
                .inspect(&id)
                .await?
                .context("no such decision in this workspace")?;
            emit(out, json, &view, |out| {
                writeln!(
                    out,
                    "Decision {}{}",
                    view.decision_id,
                    if view.legacy_v1 { " (imported v1)" } else { "" }
                )?;
                writeln!(
                    out,
                    "  backend: {} ({}) model={}",
                    view.backend_kind, view.backend_locality, view.configured_model
                )?;
                writeln!(out, "  directive: {}", view.directive_kind)?;
                if let Some(selected) = &view.selected {
                    writeln!(
                        out,
                        "  selected: {} ({} / {})",
                        selected.id, selected.action, selected.target
                    )?;
                }
                writeln!(out, "  candidates: {}", view.candidates.len())?;
                if let Some(reason) = &view.abstention_reason {
                    writeln!(out, "  abstained: {reason}")?;
                }
                if let Some(class) = &view.error_class {
                    writeln!(out, "  error: {class}")?;
                }
                writeln!(out, "  outcome: {}", view.outcome_join)?;
                for outcome in &view.outcomes {
                    writeln!(
                        out,
                        "    {}: {} ({})",
                        outcome.settlement_id, outcome.classification, outcome.completeness
                    )?;
                }
                Ok(())
            })
        }
        CausalReadCommands::Evaluate { from, to, owner } => {
            let window = TimeWindow {
                from_ms: parse_time(&from, false)?,
                to_ms: parse_time(&to, true)?,
            };
            anyhow::ensure!(window.from_ms < window.to_ms, "`--from` must be before `--to`");
            let view = admin.evaluate(owner.as_deref(), window).await?;
            emit(out, json, &view, |out| {
                let report = &view.report;
                match &view.report_path {
                    Some(path) => writeln!(out, "Report {} written to {path}", report.report_id)?,
                    None => writeln!(
                        out,
                        "Report {} (the decision plane is disabled, so it is not written; it follows in full)",
                        report.report_id
                    )?,
                }
                writeln!(
                    out,
                    "  decisions={} complete outcomes={} coverage={}",
                    report.coverage.decisions,
                    report.coverage.complete_outcomes,
                    report
                        .coverage
                        .coverage
                        .map_or_else(|| "n/a".to_string(), |coverage| format!("{:.1}%", coverage * 100.0))
                )?;
                writeln!(out, "  accuracy is {}", report.accuracy_meaning)?;
                match &report.counterfactual {
                    crate::causal_tree::evaluate::Availability::Available { value } => {
                        writeln!(out, "  counterfactual: {} policy estimates", value.len())?;
                    }
                    crate::causal_tree::evaluate::Availability::Unavailable { reason } => {
                        writeln!(out, "  counterfactual: unavailable ({reason})")?;
                    }
                }
                if view.report_path.is_none() {
                    serde_json::to_writer_pretty(&mut *out, report)?;
                    writeln!(out)?;
                }
                Ok(())
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // Exercise the production admin accounting entry under its real cleanup owner.
    #[tokio::test]
    async fn admin_accounting_accepts_real_configured_physical_slot() {
        let workspace = tempfile::tempdir().expect("private admin accounting workspace");
        let mut config = Config {
            workspace_dir: workspace.path().to_owned(),
            config_path: workspace.path().join("config.toml"),
            ..Config::default()
        };
        config.causal_tree.enabled = true;
        config.causal_tree.backend.kind =
            crate::causal_tree::config::BackendSelection::Fixed(BackendKind::CompatibleHttp);
        config.causal_tree.backend.endpoint = "http://127.0.0.1:12345".to_owned();
        config.causal_tree.backend.model = "prx-admin-accounting-model".to_owned();
        config.providers.models.push(
            serde_json::from_value(serde_json::json!({
                "provider":"custom:http://127.0.0.1:12345",
                "model":"prx-admin-accounting-model", "operations":["decision_endpoint"]
            }))
            .expect("actual decision model declaration"),
        );
        config.cost.prices.insert(
            "custom:http://127.0.0.1:12345/prx-admin-accounting-model".to_owned(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
        let key = FactoryDeps::load_key(&config).expect("actual private DecisionKey");
        let plane = HttpPlane {
            events: Arc::new(crate::decision::telemetry::ObserverCausalSink::new(Arc::new(
                crate::observability::NoopObserver,
            ))),
        };
        use crate::causal_tree::admin::DecisionPlaneBuilder as _;
        let factory = plane
            .build(&config, Some(Arc::clone(&key)))
            .expect("actual configured factory");
        let slots = factory.status().order;
        assert!(
            slots
                .iter()
                .any(|slot| slot.identity.kind == BackendKind::CompatibleHttp && slot.identity.provider.is_some()),
            "factory must contain the physical provider"
        );
        assert_eq!(slots.len(), 1, "configured factory status contains one physical slot");
        assert!(
            slots
                .iter()
                .all(|slot| slot.identity.kind != BackendKind::Builtin && slot.identity.provider.is_some()),
            "builtin fallback is not a physical status slot"
        );
        let provider = slots
            .first()
            .expect("actual physical slot")
            .identity
            .provider
            .as_ref()
            .expect("actual physical provider");
        let effective = crate::router::resolve_effective_compaction_config(
            &config.agent.compaction,
            provider.as_str(),
            &slots.first().expect("actual physical slot").identity.configured_model,
            &config.router,
            &config.model_routes,
        );
        let expected_tokens = u64::try_from(
            crate::runtime::turn_decision::DecisionTokenReservation::from_effective_compaction(
                &effective.config,
                config.causal_tree.policy.extra_token_ratio_limit,
            )
            .expect("actual effective decision reservation")
            .decision_tokens,
        )
        .expect("bounded decision reservation");
        let expected_cost = crate::cost::tracker::remaining_micro_usd_read_only_with_security_ceiling(
            &config.cost,
            &config.workspace_dir,
            config.autonomy.max_cost_per_day_cents,
        )
        .expect("actual remaining cost ceiling");
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
            .expect("actual admin cleanup owner");
        cleanup
            .run(async {
                let scope = admin_accounting_scope(
                    &config,
                    &factory,
                    key,
                    b"configured-test",
                    std::time::Instant::now()
                        + std::time::Duration::from_millis(config.causal_tree.policy.extra_latency_budget_ms),
                )
                .await?;
                let allowance = scope.remaining_budgets();
                assert_eq!(
                    allowance.tokens, expected_tokens,
                    "exact effective decision token allowance"
                );
                assert!(allowance.tokens > 0, "physical decision retains token allowance");
                assert_eq!(allowance.micro_usd, expected_cost, "actual remaining cost ceiling");
                assert!(allowance.micro_usd > 0, "actual remaining-cost ceiling is available");
                assert!(
                    allowance.latency_ms <= config.causal_tree.policy.extra_latency_budget_ms,
                    "elapsed time cannot widen the original latency ceiling"
                );
                Ok(())
            })
            .await
            .expect("accounting and cleanup must complete");
    }

    #[tokio::test]
    async fn admin_physical_accounting_rejects_builtin_only_factory_without_allowance() {
        let workspace = tempfile::tempdir().expect("private builtin accounting workspace");
        let mut config = Config {
            workspace_dir: workspace.path().to_owned(),
            config_path: workspace.path().join("config.toml"),
            ..Config::default()
        };
        config.causal_tree.enabled = true;
        config.causal_tree.backend.kind = crate::causal_tree::config::BackendSelection::Fixed(BackendKind::Builtin);
        let key = FactoryDeps::load_key(&config).expect("actual private DecisionKey");
        let plane = HttpPlane {
            events: Arc::new(crate::decision::telemetry::ObserverCausalSink::new(Arc::new(
                crate::observability::NoopObserver,
            ))),
        };
        use crate::causal_tree::admin::DecisionPlaneBuilder as _;
        let factory = plane
            .build(&config, Some(Arc::clone(&key)))
            .expect("actual builtin factory");
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
            .expect("actual admin cleanup owner");
        cleanup
            .run(async {
                let result = admin_accounting_scope(
                    &config,
                    &factory,
                    key,
                    b"configured-test",
                    std::time::Instant::now()
                        + std::time::Duration::from_millis(config.causal_tree.policy.extra_latency_budget_ms),
                )
                .await;
                let error = match result {
                    Ok(_) => panic!("builtin cannot supply physical allowance"),
                    Err(error) => error,
                };
                assert_eq!(error.to_string(), "admin decision allowance unavailable");
                Ok(())
            })
            .await
            .expect("rejection must still drain cleanup");
    }

    #[test]
    fn admin_factory_rejects_undeclared_physical_model_before_accounting() {
        let workspace = tempfile::tempdir().expect("private selection workspace");
        let mut config = Config {
            workspace_dir: workspace.path().to_owned(),
            config_path: workspace.path().join("config.toml"),
            ..Config::default()
        };
        config.causal_tree.enabled = true;
        config.causal_tree.backend.kind =
            crate::causal_tree::config::BackendSelection::Fixed(BackendKind::CompatibleHttp);
        config.causal_tree.backend.endpoint = "http://127.0.0.1:12345".to_owned();
        config.causal_tree.backend.model = "prx-undeclared-admin-model".to_owned();
        config.providers.models.clear();
        let key = FactoryDeps::load_key(&config).expect("actual private DecisionKey");
        let plane = HttpPlane {
            events: Arc::new(crate::decision::telemetry::ObserverCausalSink::new(Arc::new(
                crate::observability::NoopObserver,
            ))),
        };
        use crate::causal_tree::admin::DecisionPlaneBuilder as _;
        assert!(
            matches!(plane.build(&config, Some(key)), Err(FactoryBuildError::Selection(_))),
            "unconfigured physical model must fail genuine strict selection"
        );
    }

    fn apply() -> CausalApplyArgs {
        CausalApplyArgs {
            yes: false,
            confirm_high_risk: None,
        }
    }

    /// D-094 C-6: switching to the builtin while remote processing is on
    /// turns it off in the same plan; otherwise it is left alone.
    #[test]
    fn switching_to_the_builtin_pairs_disable_remote() {
        let builtin = || CausalConfigCommands::Set {
            what: CausalSetCommands::Backend {
                kind: BackendKindArg::Builtin,
                endpoint: None,
                model: None,
                pinned_snapshot: None,
                timeout_ms: None,
                reference: None,
                apply: apply(),
            },
        };
        let mut config = Config::default();
        config.causal_tree.backend.remote_enabled = true;
        let CausalConfigChange::Plan { mutations, .. } = config_change(builtin(), &config).expect("test: change")
        else {
            panic!("a plan");
        };
        assert_eq!(mutations.first(), Some(&CausalMutation::SetRemote(false)));
        config.causal_tree.backend.remote_enabled = false;
        let CausalConfigChange::Plan { mutations, .. } = config_change(builtin(), &config).expect("test: change")
        else {
            panic!("a plan");
        };
        assert!(!mutations.contains(&CausalMutation::SetRemote(false)), "{mutations:?}");
    }

    /// Coordinator ruling on CDP-14 (1): a hosted backend without a
    /// reference is refused with the exact two commands, and the owner the
    /// first one stores is the identity the decision check requires.
    #[test]
    fn a_hosted_backend_without_a_reference_names_the_two_commands() {
        for (arg, kind) in [
            (BackendKindArg::TypeSafe, BackendKind::TypeSafe),
            (BackendKindArg::OpenRouter, BackendKind::OpenRouter),
        ] {
            let error = config_change(
                CausalConfigCommands::Set {
                    what: CausalSetCommands::Backend {
                        kind: arg,
                        endpoint: None,
                        model: Some("jev-1.13.0".into()),
                        pinned_snapshot: None,
                        timeout_ms: None,
                        reference: None,
                        apply: apply(),
                    },
                },
                &Config::default(),
            )
            .expect_err("test: refused")
            .to_string();
            let owner = hosted_owner(kind).expect("test: hosted");
            assert!(
                error.contains(&format!(
                    "prx providers credential set {owner} --name decision-{}",
                    kind.as_str()
                )) && error.contains(&format!("--reference cred-decision-{}", kind.as_str())),
                "{error}"
            );
            let mut section = crate::causal_tree::config::CausalTreeConfig::default();
            section.backend.kind = crate::causal_tree::config::BackendSelection::Fixed(kind);
            let selection = crate::decision::factory::configured_selection(&section).expect("test: selection");
            assert_eq!(
                crate::providers::ProviderId::of(owner),
                crate::providers::ProviderId::of(&selection.provider),
                "the stored owner is the identity the check resolves"
            );
        }
    }

    /// A reference is a `cred-` id or `none`, never a secret: anything else
    /// is refused without echoing it.
    #[test]
    fn a_reference_is_never_a_secret() {
        let with = |reference: &str| {
            config_change(
                CausalConfigCommands::Set {
                    what: CausalSetCommands::Backend {
                        kind: BackendKindArg::CompatibleHttp,
                        endpoint: Some("http://127.0.0.1:8900/v1".into()),
                        model: Some("decider-1".into()),
                        pinned_snapshot: None,
                        timeout_ms: None,
                        reference: Some(reference.into()),
                        apply: apply(),
                    },
                },
                &Config::default(),
            )
        };
        assert!(with("cred-decider").is_ok());
        assert!(with("none").is_ok());
        let error = with("sk-live-PASTED-SENTINEL").expect_err("test: refused").to_string();
        assert!(!error.contains("PASTED-SENTINEL"), "{error}");
    }

    /// CDP-14: setup refuses a hosted kind before anything is written, and
    /// checks a local choice with the one decision check.
    #[test]
    fn setup_refuses_hosted_kinds_and_checks_local_ones() {
        for kind in [BackendKind::TypeSafe, BackendKind::OpenRouter] {
            let setup = DecisionSetup {
                kind,
                endpoint: None,
                model: Some("jev-1.13.0".to_string()),
                enable: false,
            };
            let error = setup.check().expect_err("test: hosted refused").to_string();
            assert!(error.contains("prx causal config set backend"), "{error}");
        }
        let local = DecisionSetup {
            kind: BackendKind::CompatibleHttp,
            endpoint: Some("http://127.0.0.1:8900/v1".to_string()),
            model: Some("decider-1".to_string()),
            enable: true,
        };
        local.check().expect("test: a declared loopback backend passes");
        let remote = DecisionSetup {
            endpoint: Some("https://decisions.example.com/v1".to_string()),
            ..local
        };
        assert!(remote.check().is_err(), "a remote endpoint needs remote_enabled");
        // Only the decision check refuses a route hint as the decision
        // model: the section rules accept the spelling.
        let hint = DecisionSetup {
            endpoint: Some("http://127.0.0.1:8900/v1".to_string()),
            model: Some("hint:fast".to_string()),
            ..remote
        };
        let error = hint
            .check()
            .expect_err("test: the decision check refuses it")
            .to_string();
        assert!(error.contains("causal_tree.backend"), "{error}");
    }

    /// A configuration written to `dir` with `edit`, loaded back.
    async fn written(dir: &std::path::Path, edit: impl FnOnce(&mut Config)) -> Config {
        let mut config = Config {
            config_path: dir.join("config.toml"),
            workspace_dir: dir.join("workspace"),
            ..Config::default()
        };
        edit(&mut config);
        config.save().await.expect("test: save");
        Config::load_from_path(&config.config_path, config.workspace_dir.clone()).expect("test: load")
    }

    /// D-105 K-1: setup confirms a loopback decision endpoint itself, and
    /// nothing else: on a configuration with `remote_enabled` on, a remote
    /// endpoint is refused with the confirming command, and nothing is
    /// written.
    #[tokio::test]
    async fn setup_confirms_only_a_loopback_endpoint() {
        let temp = tempfile::TempDir::new().expect("test: temp");
        let local = DecisionSetup {
            kind: BackendKind::CompatibleHttp,
            endpoint: Some("http://127.0.0.1:8900/v1".to_string()),
            model: Some("decider-1".to_string()),
            enable: false,
        };
        let config = written(&temp.path().join("local"), |_| {}).await;
        local
            .apply(config)
            .await
            .expect("test: a loopback endpoint is confirmed by setup");

        let remote_dir = temp.path().join("remote");
        let config = written(&remote_dir, |config| config.causal_tree.backend.remote_enabled = true).await;
        let before = std::fs::read(remote_dir.join("config.toml")).expect("test: read");
        let remote = DecisionSetup {
            endpoint: Some("https://decisions.example.com/v1".to_string()),
            ..local
        };
        let error = remote.apply(config).await.expect_err("test: refused").to_string();
        assert!(error.contains("--confirm-high-risk"), "{error}");
        assert!(error.contains("the base configuration was written"), "{error}");
        assert_eq!(
            std::fs::read(remote_dir.join("config.toml")).expect("test: read"),
            before,
            "nothing was written"
        );
    }

    /// D-105 K-4: a choice the written configuration refuses (here a
    /// calibration pinned to another model) fails after the base
    /// configuration was written, and says so.
    #[tokio::test]
    async fn setup_checks_the_written_configuration() {
        let temp = tempfile::TempDir::new().expect("test: temp");
        let config = written(temp.path(), |config| {
            config.causal_tree.backend.kind =
                crate::causal_tree::config::BackendSelection::Fixed(BackendKind::CompatibleHttp);
            config.causal_tree.backend.endpoint = "http://127.0.0.1:8900/v1".to_string();
            config.causal_tree.backend.model = "decider-2026-09".to_string();
            config.causal_tree.policy.calibration_profile = Some(crate::causal_tree::config::CalibrationProfile {
                kind: BackendKind::CompatibleHttp,
                endpoint: "http://127.0.0.1:8900/v1".to_string(),
                model: "decider-2026-09".to_string(),
            });
        })
        .await;
        let setup = DecisionSetup {
            kind: BackendKind::CompatibleHttp,
            endpoint: Some("http://127.0.0.1:8900/v1".to_string()),
            model: Some("decider-1".to_string()),
            enable: false,
        };
        setup
            .check()
            .expect("test: the default configuration accepts the choice");
        let error = setup.apply(config).await.expect_err("test: refused").to_string();
        assert!(
            error.contains("the base configuration was written, but the decision backend was not configured"),
            "{error}"
        );
    }

    #[test]
    fn times_parse_as_inclusive_days_or_instants() {
        let from = parse_time("2026-09-01", false).expect("test: day");
        let to = parse_time("2026-09-01", true).expect("test: day end");
        assert_eq!(to - from, 86_400_000);
        assert_eq!(parse_time("2026-09-01T00:00:00Z", false).expect("test: instant"), from);
        assert!(parse_time("yesterday", false).is_err());
    }
}

#[cfg(test)]
#[path = "causal_cli/backend_transport_tests.rs"]
mod backend_transport_tests;
