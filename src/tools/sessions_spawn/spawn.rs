use super::delegation;
use super::{
    SessionsSpawnTool, SpawnExecutionContext, SpawnLineage, SpawnRecordCodes, SpawnScope, boxed_spawn_field,
    current_spawn_execution_context, normalize_process_memory_strategy, parse_spawn_scope,
    record_spawn_request_event, refresh_tools_for_explicit_allowlist, running_run_count, spawn_event_scope,
    spawn_lineage, spawn_session_key,
};
use crate::channels::traits::Channel;
use crate::providers;
use crate::runtime::child_source::publication::ChildDelegation;
use crate::security::SideEffectGate;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::tools::traits::{Tool, ToolResult};
use anyhow::Context as _;
use std::sync::Arc;
use uuid::Uuid;

/// What one spawn attempt produced: the tool-visible result, plus the run id
/// when a run was actually registered.
pub(crate) struct SpawnOutcome {
    pub(super) result: ToolResult,
    pub(super) run_id: Option<String>,
}

impl SpawnOutcome {
    /// The request never became a run (bad argument, denied gate, unusable
    /// agent/provider). There is nothing to join.
    const fn rejected(error: String) -> Self {
        Self {
            result: ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            },
            run_id: None,
        }
    }

    /// A run is registered and its driver task is live.
    pub(super) const fn started(run_id: String, output: String) -> Self {
        Self {
            result: ToolResult {
                success: true,
                output,
                error: None,
            },
            run_id: Some(run_id),
        }
    }
}

pub(super) fn resolve_pinned_child_compaction(
    config: &crate::config::Config,
    catalog: &providers::catalog::CatalogView,
    provider: &str,
    model: &str,
) -> crate::providers::context_window::EffectiveCompactionConfig {
    crate::providers::context_window::resolve_effective_compaction_config_with_catalog(
        &config.agent.compaction,
        provider,
        model,
        crate::providers::context_window::ContextWindowSources::from_config(config),
        &config.model_routes,
        catalog,
    )
}

/// Everything `execute_spawn_with_id` resolved before the launch mode splits: the admitted source,
/// the pinned provider route, the run identity and its scope. Borrowed fields point into values the
/// caller keeps alive for the whole launch.
pub(super) struct SpawnLaunch<'a> {
    pub(super) operator_source: &'a Option<Arc<crate::runtime::operator_spawn::ChildLaunchSource>>,
    pub(super) message_input: Option<crate::messaging::runtime::MessageInput>,
    pub(super) message_permit: Option<Arc<crate::runtime::agent_admission::AgentTurnPermit>>,
    pub(super) caller: &'a Option<crate::tools::execution::ChildCallContext>,
    pub(super) child_delegation: Option<ChildDelegation>,
    pub(super) ordinary_publication: Option<&'a crate::runtime::child_source::publication::OrdinaryChildPublication>,
    pub(super) child_model_config: &'a crate::config::Config,
    pub(super) launch_security: &'a Arc<crate::security::SecurityPolicy>,
    pub(super) child_level: crate::security::AutonomyLevel,
    pub(super) spawn_config: &'a crate::config::schema::SessionsSpawnConfig,
    pub(super) task: &'a str,
    pub(super) process_memory_strategy: String,
    pub(super) spawn_scope: Option<SpawnScope>,
    pub(super) parent_exec_ctx: Option<SpawnExecutionContext>,
    pub(super) spawn_depth: usize,
    pub(super) parent_run_id: Option<String>,
    pub(super) child_full: bool,
    pub(super) inherited_channel_exclusions: Vec<String>,
    pub(super) run_id: String,
    pub(super) session_scope_key: String,
    pub(super) selected_agent: Option<(String, crate::config::DelegateAgentConfig)>,
    pub(super) recipient: Option<String>,
    pub(super) run_channel_name: Option<String>,
    pub(super) operator_channel: Option<Arc<dyn Channel>>,
    pub(super) announce_result: bool,
    pub(super) memory_fabric: crate::memory::MemoryFabric,
    pub(super) process_memory_backend: String,
    pub(super) resolved_provider_name: String,
    pub(super) resolved_model: String,
    pub(super) child_requested_model: Option<(String, String)>,
    pub(super) selected_route_hint: Option<String>,
    pub(super) route_source_provider: Option<String>,
    pub(super) child_credential: Option<providers::credentials::CredentialRef>,
    pub(super) route_credential: Option<providers::credentials::CredentialRef>,
    pub(super) route_uses_root_connection: bool,
    pub(super) direct_uses_root_connection: bool,
    pub(super) temperature_override: Option<f64>,
    pub(super) resolved_temperature: f64,
    pub(super) ordinary_binding: Option<Arc<crate::runtime::child_source::ChildProviderBinding>>,
    pub(super) spawn_scope_for_event: crate::memory::fabric::MessageEventScope,
    pub(super) run_lineage: SpawnLineage,
    pub(super) allowed_tools: Vec<String>,
    pub(super) tools: Option<Arc<Vec<Box<dyn Tool>>>>,
    pub(super) process_config_source: Option<(std::path::PathBuf, String)>,
    pub(super) source_receipt: Option<Arc<crate::runtime::child_source::ChildInputReceipt>>,
    pub(super) batch_id: Option<&'a str>,
    pub(super) process_compaction_config: crate::config::AgentCompactionConfig,
    pub(super) parent_hooks: Option<Arc<crate::hooks::HookManager>>,
}

impl SessionsSpawnTool {
    /// Launch exactly one sub-agent run and return as soon as it is registered
    /// and its driver task is running.
    ///
    /// Split out of [`Tool::execute`] so `spawn_batch` starts its members
    /// through the *same* path a single `spawn` takes — same approval gate,
    /// same registry row, same announce routing — rather than a parallel
    /// implementation that would drift. `batch_id` is the only thing that
    /// changes behaviour, and only by flipping the default of the `announce`
    /// argument.
    ///
    /// Must be called from the task that owns the requesting turn:
    /// [`crate::agent::idle::child_beat`] reads a task-local, so a run started
    /// from a detached task would silently lose the parent link that keeps a
    /// joining caller alive.
    ///
    /// `batch_id` stamps the run's registry row so `join`, `sessions_list`, and
    /// the control plane can address the whole fan-out as one unit.
    pub(super) async fn execute_spawn(
        &self,
        args: &serde_json::Value,
        batch_id: Option<&str>,
    ) -> anyhow::Result<SpawnOutcome> {
        boxed_spawn_field(|| self.execute_spawn_with_id(args, batch_id, None)).await
    }

    /// Whether the child this spawn issues could run `sessions_spawn` itself:
    /// a task child runs on this tool's registry, every child keeps the
    /// inherited surface exclusions, and a named agent's allowance bounds its
    /// tools. A recursive scope is derived only for a child that can serve it.
    /// A Full child (`full`) has neither inherited exclusions nor an allowance.
    fn child_serves_delegator(
        &self,
        args: &serde_json::Value,
        config: &crate::config::Config,
        full: bool,
    ) -> anyhow::Result<bool> {
        let name = self.name();
        let mode = args
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .unwrap_or(config.sessions_spawn.default_mode.as_str());
        if !mode.eq_ignore_ascii_case("process")
            && !self
                .tools
                .get()?
                .is_some_and(|registry| registry.iter().any(|tool| tool.name() == name))
        {
            return Ok(false);
        }
        if full {
            return Ok(true);
        }
        if current_spawn_execution_context()
            .is_some_and(|context| context.channel_excluded_tools.iter().any(|tool| tool == name))
        {
            return Ok(false);
        }
        let agent = requested_agent_name(args).and_then(|agent| config.agents.get(agent));
        Ok(agent.is_none_or(|agent| {
            agent.allowed_tools.is_empty()
                || agent
                    .allowed_tools
                    .iter()
                    .map(|tool| tool.trim())
                    .any(|tool| tool == "*" || tool == name)
        }))
    }

    pub(super) async fn execute_spawn_with_id(
        &self,
        args: &serde_json::Value,
        batch_id: Option<&str>,
        reserved_run_id: Option<&str>,
    ) -> anyhow::Result<SpawnOutcome> {
        let operator_source = crate::runtime::operator_spawn::claim(args)?;
        if let Some(source) = &operator_source {
            source.verify_live()?;
        }
        let message_input = crate::runtime::operator_spawn::current().and_then(|source| source.message_input());
        let message_permit = operator_source.as_ref().and_then(|source| source.message_permit());
        let caller = crate::tools::execution::current_child_call_context();
        if let Some(caller) = &caller {
            caller.verify_tool("sessions_spawn")?;
        }
        // Recursive delegation (the default; an omitted flag falls back to a
        // non-delegating child at a bound, an explicit `false` opts out)
        // re-scopes the caller onto a publication that shares its
        // authenticated parent, so task issuance, process launch and funding
        // all read that one object. An operator source opens its children as
        // roots, so the flag changes nothing there.
        let recursive = delegation::requested(args)?;
        // Whether the issued child can spawn; operator and unpublished
        // callers have no ordinary child publication and state nothing.
        let mut child_delegation = None;
        let caller = match caller {
            Some(caller) => match caller.context().ordinary_child_publication.as_ref() {
                Some(publication) => {
                    let config = &publication.generation().effective;
                    // Full parents keep every child Full; below Full a named
                    // agent retains its configured level.
                    let named = crate::security::agent_level::named_child_level(
                        config,
                        requested_agent_name(args),
                        publication.level(),
                    );
                    let full = named.unwrap_or_else(|| publication.level()) == crate::security::AutonomyLevel::Full;
                    let served = self.child_serves_delegator(args, config, full)?;
                    let (selected, delegation) = publication.select_for_spawn(caller.context(), recursive, served)?;
                    child_delegation = Some(delegation);
                    let level = crate::tools::capability::child::ChildLevel::from_named(named);
                    Some(caller.with_ordinary_publication(selected.with_child_level(level)?)?)
                }
                None => Some(caller),
            },
            None => None,
        };
        let ordinary_publication = caller
            .as_ref()
            .and_then(|caller| caller.context().ordinary_child_publication.as_ref());
        if let (Some(publication), Some(caller)) = (ordinary_publication, caller.as_ref()) {
            publication.verify_caller(caller.context())?;
        }
        anyhow::ensure!(
            ordinary_publication.is_some() || operator_source.is_some(),
            "session spawning requires an admitted parent or operator source"
        );
        anyhow::ensure!(
            operator_source.is_none() || (caller.is_none() && current_spawn_execution_context().is_none()),
            "operator spawn cannot inherit model or child authority"
        );
        let child_model_config = ordinary_publication
            .map(|publication| publication.generation().effective.as_ref())
            .or_else(|| {
                operator_source
                    .as_ref()
                    .map(|source| source.generation().effective.as_ref())
            })
            .context("spawn admitted configuration missing")?;
        let launch_security = ordinary_publication
            .map(|publication| publication.security())
            .or_else(|| operator_source.as_ref().map(|source| source.security()))
            .ok_or_else(|| anyhow::anyhow!("spawn admitted security policy missing"))?;
        // The level the child runs at, recorded on `task.spawned`.
        let child_level = ordinary_publication.map_or_else(
            || {
                crate::security::agent_level::named_child_level(
                    child_model_config,
                    requested_agent_name(args),
                    launch_security.autonomy,
                )
                .unwrap_or(launch_security.autonomy)
            },
            crate::runtime::child_source::publication::OrdinaryChildPublication::level,
        );
        let child_level = if operator_source
            .as_ref()
            .is_some_and(|source| source.is_message_continuation())
        {
            launch_security.autonomy
        } else {
            child_level
        };
        let provider_options = &providers::provider_runtime_options_from_config(child_model_config);
        let spawn_config = &child_model_config.sessions_spawn;
        let agents = &child_model_config.agents;
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("Missing 'task' parameter"))?;

        if task.is_empty() {
            return Ok(SpawnOutcome::rejected("'task' parameter must not be empty".into()));
        }
        // The brief and references are rendered after the task, so the child's
        // recorded input and the process manifest both carry them. Absent or
        // blank keys leave the task untouched.
        let rendered_task = match super::brief::render_task(task, args) {
            Ok(rendered) => rendered,
            Err(message) => return Ok(SpawnOutcome::rejected(message)),
        };
        let task = rendered_task.as_deref().unwrap_or(task);

        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(spawn_config.default_mode.as_str())
            .to_ascii_lowercase();
        if mode != "task" && mode != "process" {
            return Ok(SpawnOutcome::rejected(format!(
                "Invalid 'mode' value '{mode}'. Expected 'task' or 'process'."
            )));
        }
        let process_memory_strategy = if mode == "process" {
            normalize_process_memory_strategy(&spawn_config.process_memory_strategy)?.to_string()
        } else {
            String::new()
        };

        let model_override = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let provider_override = args
            .get("provider")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let agent_name = args
            .get("agent")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let explicit_recipient = args
            .get("recipient")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let spawn_scope = if let Some(source) = &operator_source {
            use crate::runtime::turn_source::TurnSourceProof;
            let proof = source.proof();
            Some(SpawnScope {
                sender: proof.policy_sender().to_owned(),
                channel: proof.policy_channel().to_owned(),
                chat_type: proof.policy_chat_type().to_owned(),
                chat_id: proof
                    .memory_scope()
                    .chat_id
                    .clone()
                    .context("operator spawn recipient missing")?,
                owner_id: Some(proof.owner_id().to_owned()),
                owner_isolated: proof.owner_isolated(),
                topic_id: None,
                parent_task_id: Some(proof.run_id().to_owned()),
                source_message_event_id: proof.source_event_id().map(str::to_owned),
                config_generation_id: Some(proof.config_generation_id()),
                config_source_revision: Some(proof.config_source_revision().to_owned()),
            })
        } else {
            parse_spawn_scope(args)
        };
        let parent_exec_ctx = current_spawn_execution_context();
        // D8-4: a turn-root context represents the turn itself (zero spawn
        // nesting so far), so its first child reports depth 0 — identical to the
        // no-context case. A real spawn-run context's child reports +1. The depth
        // is lineage only: nothing rejects a spawn for being deeply nested.
        let spawn_depth = parent_exec_ctx.as_ref().map_or(0, |ctx| {
            if ctx.is_turn_root {
                ctx.spawn_depth
            } else {
                ctx.spawn_depth.saturating_add(1)
            }
        });
        let parent_run_id = parent_exec_ctx.as_ref().map(|ctx| ctx.run_id.clone()).or_else(|| {
            use crate::runtime::turn_source::TurnSourceProof;
            operator_source
                .as_ref()
                .map(|source| source.proof().run_id().to_owned())
        });
        // Whatever the parent turn's surface hides stays hidden for everything
        // this spawn produces, however deeply it nests. A Full child has no
        // such exclusions: every tool its level allows is published.
        let child_full = child_level == crate::security::AutonomyLevel::Full;
        let inherited_channel_exclusions = parent_exec_ctx
            .as_ref()
            .filter(|_| !child_full)
            .map(|ctx| ctx.channel_excluded_tools.clone())
            .unwrap_or_default();

        // FIX-P0-37: spawning a child session creates a new process that
        // consumes resources and carries a potential sandbox-escape surface,
        // so it is a Medium-risk side effect (requires an approval grant under
        // supervised autonomy; denied outright under read-only) rather than Low.
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), args);
        if let Err(error) = if operator_source
            .as_ref()
            .is_some_and(|source| source.is_message_continuation())
        {
            Ok(ResourceRiskLevel::Medium)
        } else {
            SideEffectGate::new(launch_security.as_ref()).authorize_resource_operation(
                self.name(),
                "sessions_spawn:spawn",
                ResourceRiskLevel::Medium,
                approval_grant.as_ref(),
            )
        } {
            return Ok(SpawnOutcome::rejected(error));
        }

        let run_id = reserved_run_id.map_or_else(|| Uuid::new_v4().to_string(), str::to_owned);
        // Every ordinary spawned run records under its own key; the caller
        // link is `parent_session_key` and `parent_run_id`. An operator
        // source keeps its authenticated session.
        let session_scope_key =
            super::continuation::current().map_or_else(|| spawn_session_key(&run_id), |seed| seed.record.session_key);
        let _new_session_gate = if super::continuation::current().is_none() {
            let owner = spawn_scope
                .as_ref()
                .and_then(|scope| scope.owner_id.as_deref())
                .context("child owner missing")?;
            let address = crate::messaging::SessionAddr::new(
                child_model_config.workspace_dir.to_string_lossy(),
                owner,
                &session_scope_key,
                None,
            );
            Some(crate::messaging::receiver::gate(&address).lock_owned().await)
        } else {
            None
        };

        // The registry imposes no global fan-out or nesting limit. A child's
        // sealed delegation grant independently bounds its depth. An operator sees every
        // live run in the runtime registry and ends it with `prx tasks kill`,
        // which is what makes an uncapped fan-out answerable rather than
        // unstoppable. Counting the live runs on the way in keeps that fan-out
        // visible in the log while it grows, instead of only after it hurts.
        let live_runs = running_run_count(&self.active_runs.read().await);
        tracing::debug!(
            live_runs,
            spawn_depth,
            session_scope_key = %session_scope_key,
            "sessions_spawn accepted"
        );

        let selected_agent = match agent_name.as_deref() {
            Some(name) => match agents.get(name) {
                Some(cfg) => {
                    // At Full `spawn_enabled` does not limit the caller.
                    let caller_full = crate::security::agent_level::call_level(
                        crate::tools::effective_autonomy::current(),
                        launch_security.autonomy,
                    ) == crate::security::AutonomyLevel::Full;
                    if !caller_full && !cfg.spawn_enabled.unwrap_or(true) {
                        return Ok(SpawnOutcome::rejected(format!(
                            "Agent '{name}' is not allowed for sessions_spawn (spawn_enabled=false)."
                        )));
                    }
                    let mut selected = cfg.clone();
                    {
                        let config = child_model_config;
                        selected.api_key = providers::credentials::configured_key(
                            &providers::credentials::ConfigCredentialSource::new(config),
                            &cfg.provider,
                            cfg.api_key.as_deref(),
                            cfg.credential_ref.as_deref(),
                        );
                    }
                    Some((name.to_string(), selected))
                }
                None => {
                    let mut available = agents
                        .iter()
                        .filter(|(_, cfg)| cfg.spawn_enabled.unwrap_or(true))
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>();
                    available.sort_unstable();
                    return Ok(SpawnOutcome::rejected(format!(
                        "Unknown agent '{name}'. Available agents: {}",
                        if available.is_empty() {
                            "(none configured)".to_string()
                        } else {
                            available.join(", ")
                        }
                    )));
                }
            },
            None => None,
        };

        // Resolve the announce recipient + channel atomically from this turn.
        //
        // Precedence: explicit `recipient` arg > per-turn scope `chat_id` >
        // shared `default_recipient`. The per-turn scope (parsed from the trusted
        // `_zc_scope` injected for *this* execution) is atomic — it travels with
        // the launching message — whereas `default_recipient`/`channel` are shared
        // and a concurrently-processed message can overwrite them between this
        // turn entering the LLM loop and the spawn actually executing. Binding
        // both `recipient` and `channel_name` from the same scope eliminates the
        // A-channel + B-recipient cross-wiring (cross-channel privacy leak).
        let recipient = match explicit_recipient {
            Some(r) => Some(r),
            None => match spawn_scope
                .as_ref()
                .map(|scope| scope.chat_id.trim())
                .filter(|chat_id| !chat_id.is_empty())
                .map(str::to_owned)
            {
                Some(chat_id) => Some(chat_id),
                None => self.default_recipient.read().await.clone(),
            },
        };
        // Channel name bound to the *originating* message (atomic with recipient).
        // `None` (no trusted scope) falls back at announce time to the shared
        // active channel, preserving single-channel / legacy behaviour.
        let run_channel_name = spawn_scope.as_ref().map(|scope| scope.channel.clone());
        let operator_channel = if operator_source
            .as_ref()
            .is_some_and(|source| !source.is_message_continuation())
        {
            let channel = self.resolve_announce_channel(run_channel_name.as_deref()).await;
            anyhow::ensure!(
                Some(channel.name()) == run_channel_name.as_deref(),
                "operator spawn announcement channel differs from its source"
            );
            Some(channel)
        } else {
            None
        };

        // Whether this run posts its own result to the channel when it ends.
        //
        // Default by construction: a standalone `spawn` is fire-and-forget, so
        // its announcement is the only way its result is ever seen and it stays
        // on. A `spawn_batch` member's result is collected by `join`, so
        // announcing it as well would put the fan-out's raw intermediate output
        // on the channel *and* the parent's summary after it — N+1 messages for
        // one request. Which of those two a run is, is exactly `batch_id`.
        //
        // For a batch member this is not a default but a **property**: the
        // argument is not consulted at all. "One request, one message" is the
        // whole point of `spawn_batch` + `join`, and a default the model can
        // flip is not that promise — it is a suggestion. `announce` is
        // therefore no longer a per-member parameter of the entry schema
        // either, so an entry that names it is refused where the caller can
        // see it (see `batch_member_overridable_keys`), rather than silently
        // ignored here. A run that really should report for itself is a plain
        // `spawn`, which is unchanged.
        //
        // This deliberately changes nothing else about the announce wiring: the
        // run still captures `channel_name` + `recipient` atomically from the
        // launching message's scope (below), so a later kill notification still
        // routes to the right channel.
        //
        // MUTATION GUARD: drop the `batch_id.is_none() &&` and a batch launched
        // with `announce: true` at the top level delivers one message per member
        // on top of the parent's summary.
        let announce_result = batch_id.is_none()
            && !operator_source
                .as_ref()
                .is_some_and(|source| source.is_message_continuation())
            && args
                .get("announce")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);

        let memory_fabric = ordinary_publication
            .map(|publication| publication.source_fabric())
            .or_else(|| operator_source.as_ref().map(|source| source.fabric().clone()))
            .context("spawn admitted memory fabric missing")?;
        if mode == "process" && memory_fabric.memory().requires_scoped_execution() {
            return Err(
                crate::memory::erasure::Unsupported("process worker cannot preserve scoped memory erasure").into(),
            );
        }
        let process_memory_backend = memory_fabric.memory().name().to_owned();
        let child_model_catalog = ordinary_publication
            .map(|publication| publication.provider_catalog())
            .or_else(|| operator_source.as_ref().map(|source| source.catalog()))
            .context("spawn admitted provider catalog missing")?;
        // A per-call provider is accepted only among the providers the operator
        // configured: a prompt must not be able to direct this run, or a
        // credential, to a host the operator never chose.
        if let Some(requested) = provider_override.as_deref() {
            let configured = providers::ConfiguredProviders::from_config(child_model_config);
            if let Err(error) = configured.ensure_permits(requested) {
                return Ok(SpawnOutcome::rejected(error.to_string()));
            }
        }
        let inherited_model = if let Some(publication) = ordinary_publication {
            publication.model_source()
        } else {
            use crate::runtime::turn_source::TurnSourceProof;
            let source = operator_source.as_ref().context("spawn model source missing")?;
            (
                source.proof().provider_name(),
                source.proof().model_name(),
                source.resolved().credential().reference(),
            )
        };
        let inherits_default_route =
            provider_override.is_none() && model_override.is_none() && selected_agent.is_none();
        let mut resolved_provider_name = provider_override.unwrap_or_else(|| {
            selected_agent
                .as_ref()
                .map(|(_, cfg)| cfg.provider.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| inherited_model.0.to_owned())
        });
        let mut resolved_model = model_override.unwrap_or_else(|| {
            selected_agent
                .as_ref()
                .map(|(_, cfg)| cfg.model.trim().to_string())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| inherited_model.1.to_owned())
        });
        // Retain the request spelling: a resolved hint alone cannot prove which
        // route and credential source selected the actual child provider.
        let mut child_requested_model = ordinary_publication
            .is_some()
            .then(|| (resolved_provider_name.clone(), resolved_model.clone()));
        let selected_route_hint = resolved_model.trim().strip_prefix("hint:").map(str::to_string);
        let route_source_provider = selected_route_hint.as_ref().map(|_| resolved_provider_name.clone());
        // Resolve the child's model for chat before any run or provider
        // exists: a model that cannot chat is refused and nothing is sent. The
        // gateway's own provider was resolved when it was built; a provider
        // this build cannot name fails construction with its own message.
        if providers::ProviderId::of(&resolved_provider_name).is_resolvable()
            && let Err(refusal) = providers::selection::check_model(
                child_model_catalog,
                &resolved_provider_name,
                &resolved_model,
                &providers::catalog::Requirement::chat(),
                providers::catalog::UnknownPolicy::Invocation,
            )
        {
            return Ok(SpawnOutcome::rejected(format!("Cannot spawn this run: {refusal}")));
        }
        let child_credential = operator_source.as_ref().map_or_else(
            || {
                let config = child_model_config;
                let credential = if inherits_default_route {
                    (providers::ProviderId::of(inherited_model.0) == providers::ProviderId::of(&resolved_provider_name))
                        .then(|| inherited_model.2.clone())
                } else {
                    None
                };
                Some(credential.unwrap_or_else(|| {
                    selected_agent
                        .as_ref()
                        .filter(|(_, agent)| {
                            providers::ProviderId::of(&agent.provider)
                                == providers::ProviderId::of(&resolved_provider_name)
                        })
                        .map_or_else(
                            || providers::selection::credential_for(config, &resolved_provider_name),
                            |(name, agent)| {
                                agent.credential_ref.as_deref().map_or_else(
                                    || {
                                        if agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
                                            providers::credentials::CredentialRef::LegacyInline(
                                                providers::credentials::LegacySlot::Agent(name.clone()),
                                            )
                                        } else {
                                            providers::selection::credential_for(config, &resolved_provider_name)
                                        }
                                    },
                                    |id| providers::credentials::CredentialRef::Profile { id: id.to_string() },
                                )
                            },
                        )
                }))
            },
            |source| Some(source.resolved().credential().reference().clone()),
        );
        // An admitted ordinary child also needs an exact PMM/credential
        // selection for direct model names, rather than reusing the gateway.
        let selected_hint = if let Some(source) = &operator_source {
            source.verify_live()?;
            let resolved = source.resolved().clone();
            resolved_provider_name = resolved.provider_label().to_owned();
            resolved_model = resolved.model().to_owned();
            Some(resolved)
        } else if ordinary_publication.is_some() || resolved_model.trim().starts_with("hint:") {
            let config = child_model_config;
            let credential = child_credential.clone().context("child credential selection missing")?;
            let resolved = match providers::selection::check_with(
                config,
                child_model_catalog,
                &resolved_provider_name,
                &resolved_model,
                credential,
                &providers::catalog::Requirement::chat(),
                providers::catalog::UnknownPolicy::Invocation,
            ) {
                Ok(resolved) => resolved,
                Err(refusal) => return Ok(SpawnOutcome::rejected(format!("Cannot spawn this run: {refusal}"))),
            };
            resolved_provider_name = resolved.provider_label().to_string();
            resolved_model = resolved.model().to_string();
            Some(resolved)
        } else {
            None
        };
        if selected_hint.is_none()
            && let Some((_, agent)) = selected_agent.as_ref()
            && let Some(id) = agent.credential_ref.as_deref()
        {
            let config = child_model_config;
            let checked = match providers::selection::check_with(
                config,
                child_model_catalog,
                &resolved_provider_name,
                &resolved_model,
                providers::credentials::CredentialRef::Profile { id: id.to_string() },
                &providers::catalog::Requirement::chat(),
                providers::catalog::UnknownPolicy::Invocation,
            ) {
                Ok(checked) => checked,
                Err(refusal) => return Ok(SpawnOutcome::rejected(format!("Cannot spawn this run: {refusal}"))),
            };
            if checked.credential().api_key() != agent.api_key.as_deref() {
                return Ok(SpawnOutcome::rejected(
                    "Cannot spawn this run: agent credential is unavailable".to_string(),
                ));
            }
        }
        let route_credential = selected_hint
            .as_ref()
            .filter(|_| operator_source.as_ref().is_none_or(|source| source.route().is_some()))
            .map(|resolved| resolved.credential().reference().clone());
        let route_uses_root_connection = operator_source.as_ref().is_none_or(|source| source.route().is_some())
            && selected_hint.as_ref().is_some_and(|resolved| {
                matches!(
                    resolved.credential().reference(),
                    providers::credentials::CredentialRef::LegacyInline(providers::credentials::LegacySlot::Root)
                )
            });
        let direct_uses_root_connection = operator_source.as_ref().map_or_else(
            || {
                selected_hint.is_none()
                    && self.fallback_uses_root_connection
                    && providers::ProviderId::of(&resolved_provider_name)
                        == providers::ProviderId::of(&self.fallback_api_key_owner)
                    && selected_agent.as_ref().is_none_or(|(_, agent)| {
                        agent.credential_ref.is_none()
                            && agent.api_key.as_deref().is_none_or(|key| key.trim().is_empty())
                    })
            },
            |source| {
                source.route().is_none()
                    && matches!(
                        source.resolved().credential().reference(),
                        providers::credentials::CredentialRef::LegacyInline(providers::credentials::LegacySlot::Root)
                    )
            },
        );
        let resolved_compaction = resolve_pinned_child_compaction(
            child_model_config,
            child_model_catalog,
            &resolved_provider_name,
            &resolved_model,
        );
        tracing::debug!(
            mode = mode.as_str(),
            provider = %resolved_compaction.shown_provider(),
            model = resolved_compaction.selected_model.as_str(),
            max_context_tokens = resolved_compaction.config.max_context_tokens,
            source = ?resolved_compaction.max_context_source,
            kernel_capped = resolved_compaction.kernel_capped,
            "sessions_spawn resolved child compaction window"
        );
        let temperature_override = selected_agent.as_ref().and_then(|(_, cfg)| cfg.temperature);
        let resolved_temperature = selected_agent
            .as_ref()
            .and_then(|(_, cfg)| cfg.temperature)
            .unwrap_or(child_model_config.default_temperature);
        let reuses_parent_provider =
            self.reuses_parent_provider_identity(selected_agent.as_ref().map(|(_, cfg)| cfg), &resolved_provider_name);
        // A spelling alias of the already-configured gateway is the same
        // connection, credential, and catalog identity. Issue its ordinary
        // child proof under the configured spelling so the decision service
        // sees the catalog row it actually published. Route hints retain their
        // requested source spelling because that spelling selects the route.
        if reuses_parent_provider
            && selected_route_hint.is_none()
            && let Some((provider, _)) = child_requested_model.as_mut()
        {
            provider.clone_from(&self.provider_name);
        }
        // Bind ordinary input to the pinned selection before recording.
        // Tasks retain this provider instance; processes export its verified
        // selection and construct their own instance from signed snapshots.
        let ordinary_binding = ordinary_publication
            .map(|publication| -> anyhow::Result<_> {
                let (provider, model) = child_requested_model
                    .as_ref()
                    .context("ordinary task model request missing")?;
                Ok(Arc::new(
                    crate::runtime::child_source::ChildProviderBinding::build_ordinary(
                        &publication.generation().effective,
                        publication.provider_catalog(),
                        provider,
                        model,
                        child_credential.clone().context("ordinary task credential missing")?,
                        Some(publication),
                    )?,
                ))
            })
            .transpose()?;
        let event_workspace = memory_fabric.workspace_id();
        let mut spawn_scope_for_event = spawn_event_scope(
            event_workspace,
            &run_id,
            &session_scope_key,
            parent_run_id.as_deref(),
            agent_name.as_deref(),
            spawn_scope.as_ref(),
        );
        if let Some(binding) = &ordinary_binding {
            binding.bind_ordinary_input_scope(
                &binding.provider(),
                event_workspace,
                &mut spawn_scope_for_event,
                caller.as_ref(),
            )?;
        }
        if let Some(source) = &operator_source {
            source.bind_input_scope(&mut spawn_scope_for_event)?;
        }
        let mut run_lineage = spawn_lineage(&spawn_scope_for_event, parent_exec_ctx.as_ref(), spawn_scope.as_ref());
        if let Some(source) = &operator_source {
            use crate::runtime::turn_source::TurnSourceProof;
            run_lineage.parent_task_id = Some(source.proof().run_id().to_owned());
            run_lineage.source_message_event_id = source.proof().source_event_id().map(str::to_owned);
        }
        // At Full a named agent's `allowed_tools` is ignored.
        let allowed_tools = selected_agent
            .as_ref()
            .filter(|_| !child_full)
            .map(|(_, cfg)| {
                cfg.allowed_tools
                    .iter()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let tools = self.tools.get()?;
        let parent_hooks = self.parent_hooks.clone();
        if let Some(registry) = &tools {
            refresh_tools_for_explicit_allowlist(registry, &allowed_tools).await?;
        } else if !allowed_tools.is_empty() {
            anyhow::bail!("sessions_spawn tool registry is not initialized");
        }
        let process_config_source = if mode == "process" {
            Some(Self::resolved_process_config_source(
                provider_options,
                Some(child_model_config),
            )?)
        } else {
            None
        };
        let source_receipt = record_spawn_request_event(
            Some(&memory_fabric),
            spawn_scope_for_event.clone(),
            task,
            &mode,
            (&resolved_provider_name, &resolved_model),
            &run_lineage,
            SpawnRecordCodes {
                child_delegation: child_delegation.map(ChildDelegation::record_code).as_deref(),
                child_level,
            },
        )
        .await?;

        if let Some(publication) = ordinary_publication {
            publication.watch_mailbox_children()?;
        }
        let process_compaction_config = resolved_compaction.config.clone();
        let launch = SpawnLaunch {
            operator_source: &operator_source,
            message_input,
            message_permit,
            caller: &caller,
            child_delegation,
            ordinary_publication,
            child_model_config,
            launch_security,
            child_level,
            spawn_config,
            task,
            process_memory_strategy,
            spawn_scope,
            parent_exec_ctx,
            spawn_depth,
            parent_run_id,
            child_full,
            inherited_channel_exclusions,
            run_id,
            session_scope_key,
            selected_agent,
            recipient,
            run_channel_name,
            operator_channel,
            announce_result,
            memory_fabric,
            process_memory_backend,
            resolved_provider_name,
            resolved_model,
            child_requested_model,
            selected_route_hint,
            route_source_provider,
            child_credential,
            route_credential,
            route_uses_root_connection,
            direct_uses_root_connection,
            temperature_override,
            resolved_temperature,
            ordinary_binding,
            spawn_scope_for_event,
            run_lineage,
            allowed_tools,
            tools,
            process_config_source,
            source_receipt,
            batch_id,
            process_compaction_config,
            parent_hooks,
        };
        if mode == "process" {
            return self.launch_process_run(launch).await;
        }
        self.launch_task_run(launch).await
    }
}

/// The trimmed, non-empty `agent` argument of a spawn call.
fn requested_agent_name(args: &serde_json::Value) -> Option<&str> {
    args.get("agent")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
}

/// `policy` at the level of the run that sends an announcement: the gate
/// applies the outbound scope at that run's level.
pub(super) fn policy_at_level(
    policy: &Arc<crate::security::SecurityPolicy>,
    level: crate::security::AutonomyLevel,
) -> Arc<crate::security::SecurityPolicy> {
    if policy.autonomy == level {
        Arc::clone(policy)
    } else {
        Arc::new(crate::security::SecurityPolicy {
            autonomy: level,
            ..policy.as_ref().clone()
        })
    }
}
