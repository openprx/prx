use super::{
    AllowedToolProxy, AnnounceOrigin, DEFAULT_SUB_AGENT_SYSTEM_PROMPT, HistoryEntry, MemoryScope, OutboundNotice,
    PROCESS_OWNER_PANICKED_PREFIX, ProcessFinalization, ProcessModeRunSeed, ProcessRunControl, ProcessWorkerOutcome,
    SPAWN_EXECUTION_CONTEXT, STEER_CHANNEL_CAPACITY, SessionsSpawnTool, SpawnExecutionContext, SpawnScope, SubAgentRun,
    SubAgentStatus, TaskRunControl, TerminationCause, boxed_spawn_field, commit_process_owner_failure_if_unfinalized,
    commit_process_terminal_state, current_spawn_execution_context, deliver_or_withhold_notice,
    exited_without_result_reason, format_announce_message, new_process_mode_run, normalize_process_memory_strategy,
    parse_memory_scope, parse_spawn_scope, process_terminal, record_spawn_request_event, record_spawn_result_event,
    refresh_tools_for_explicit_allowlist, register_killable_process_run, resolve_child_provider_api_key,
    resolve_tools_for_agent, run_sub_agent_process, run_sub_agent_task, running_run_count, spawn_completion_note,
    spawn_event_scope, spawn_lineage, spawn_session_scope_key, sub_agent_reported_failure, task_failure_reason,
    task_terminal, termination_cause, watch_process_mode_monitor, watch_task_mode_monitor,
};
use crate::channels::build_identity_prompt;
use crate::providers;
use crate::security::SideEffectGate;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::tools::traits::{Tool, ToolResult};
use anyhow::Context as _;
use chrono::Utc;
use futures_util::FutureExt;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
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
    const fn started(run_id: String, output: String) -> Self {
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
) -> crate::router::context::EffectiveCompactionConfig {
    crate::router::context::resolve_effective_compaction_config_with_catalog(
        &config.agent.compaction,
        provider,
        model,
        &config.router,
        &config.model_routes,
        catalog,
    )
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
        let caller = crate::tools::execution::current_child_call_context();
        if let Some(caller) = &caller {
            caller.verify_tool("sessions_spawn")?;
        }
        let ordinary_publication = caller
            .as_ref()
            .and_then(|caller| caller.context().ordinary_child_publication.as_ref());
        if let (Some(publication), Some(caller)) = (ordinary_publication, caller.as_ref()) {
            publication.verify_caller(caller.context())?;
        }
        let child_grant = crate::runtime::child_source::outgoing_grant();
        anyhow::ensure!(
            child_grant.is_some() || ordinary_publication.is_some() || operator_source.is_some(),
            "session spawning requires an admitted parent or operator source"
        );
        anyhow::ensure!(
            operator_source.is_none()
                || (caller.is_none() && child_grant.is_none() && current_spawn_execution_context().is_none()),
            "operator spawn cannot inherit model or child authority"
        );
        let child_model_config = child_grant
            .as_ref()
            .map(|grant| grant.generation().effective.as_ref())
            .or_else(|| ordinary_publication.map(|publication| publication.generation().effective.as_ref()))
            .or_else(|| {
                operator_source
                    .as_ref()
                    .map(|source| source.generation().effective.as_ref())
            })
            .context("spawn admitted configuration missing")?;
        let launch_security = child_grant
            .as_ref()
            .map(|grant| grant.security())
            .or_else(|| ordinary_publication.map(|publication| publication.security()))
            .or_else(|| operator_source.as_ref().map(|source| source.security()))
            .ok_or_else(|| anyhow::anyhow!("spawn admitted security policy missing"))?;
        let provider_options = providers::provider_runtime_options_from_config(child_model_config);
        let provider_options = &provider_options;
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
        // this spawn produces, however deeply it nests.
        let inherited_channel_exclusions = parent_exec_ctx
            .as_ref()
            .map(|ctx| ctx.channel_excluded_tools.clone())
            .unwrap_or_default();
        let session_scope_key = operator_source.as_ref().map_or_else(
            || spawn_session_scope_key(parent_exec_ctx.as_ref(), spawn_scope.as_ref()),
            |source| {
                use crate::runtime::turn_source::TurnSourceProof;
                source.proof().session_key().to_owned()
            },
        );

        // This runtime caps neither how many sub-agents run at once, how deeply
        // they nest, nor how many one session may own: an operator sees every
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

        // FIX-P0-37: spawning a child session creates a new process that
        // consumes resources and carries a potential sandbox-escape surface,
        // so it is a Medium-risk side effect (requires an approval grant under
        // supervised autonomy; denied outright under read-only) rather than Low.
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), args);
        if let Err(error) = SideEffectGate::new(launch_security.as_ref()).authorize_resource_operation(
            self.name(),
            "sessions_spawn:spawn",
            ResourceRiskLevel::Medium,
            approval_grant.as_ref(),
        ) {
            return Ok(SpawnOutcome::rejected(error));
        }

        let run_id = reserved_run_id.map_or_else(|| Uuid::new_v4().to_string(), str::to_owned);
        // A native issued child owns a distinct session before its durable input
        // is issued. Ordinary and operator spawning retain their shared scope.
        let session_scope_key = child_grant.as_ref().map_or(session_scope_key, |grant| {
            format!("{}:child:{run_id}", grant.parent().session_key())
        });

        let selected_agent = match agent_name.as_deref() {
            Some(name) => match agents.get(name) {
                Some(cfg) => {
                    if !cfg.spawn_enabled.unwrap_or(true) {
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
        let operator_channel = if operator_source.is_some() {
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
            && args
                .get("announce")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);

        anyhow::ensure!(
            !child_model_config.causal_tree.enabled || child_grant.is_some() || operator_source.is_some(),
            "decision-enabled spawning requires a child grant or typed operator source"
        );
        let memory_fabric = child_grant
            .as_ref()
            .map(|grant| grant.source_fabric().clone())
            .or_else(|| ordinary_publication.map(|publication| publication.source_fabric()))
            .or_else(|| operator_source.as_ref().map(|source| source.fabric().clone()))
            .context("spawn admitted memory fabric missing")?;
        let process_memory_backend = memory_fabric.memory().name().to_owned();
        let child_model_catalog = child_grant
            .as_ref()
            .map(|grant| grant.provider_catalog())
            .or_else(|| ordinary_publication.map(|publication| publication.provider_catalog()))
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
        let inherited_model = if let Some(grant) = &child_grant {
            grant.model_source()?
        } else if let Some(publication) = ordinary_publication {
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
        let mut child_requested_model = (child_grant.is_some() || ordinary_publication.is_some())
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
        let resolved_temperature = selected_agent
            .as_ref()
            .and_then(|(_, cfg)| cfg.temperature)
            .unwrap_or(child_model_config.default_temperature);
        // A gateway key is only meaningful for the gateway provider, and a named
        // agent's key only for that agent's own provider. Passing either to a
        // different provider hands one vendor's secret to another (and, for a
        // model-chosen provider, to a host the model picked).
        let resolved_api_key = selected_hint.as_ref().map_or_else(
            || {
                resolve_child_provider_api_key(
                    selected_agent.as_ref().map(|(_, cfg)| cfg),
                    self.fallback_api_key.as_deref(),
                    &resolved_provider_name,
                    &self.fallback_api_key_owner,
                )
            },
            |resolved| resolved.credential().api_key().map(str::to_string),
        );
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
        let process_child_grant = (mode == "process")
            .then(|| {
                child_grant
                    .as_deref()
                    .map(crate::runtime::child_source::IssuedChildGrant::for_process_worker)
                    .transpose()
                    .map(|grant| grant.map(Arc::new))
            })
            .transpose()?
            .flatten();
        let event_workspace = memory_fabric.workspace_id();
        let mut spawn_scope_for_event = spawn_event_scope(
            event_workspace,
            &run_id,
            &session_scope_key,
            parent_run_id.as_deref(),
            agent_name.as_deref(),
            spawn_scope.as_ref(),
        );
        if let Some(grant) = process_child_grant.as_ref().or(child_grant.as_ref()) {
            grant.bind_event_scope(&mut spawn_scope_for_event)?;
        }
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
        let allowed_tools = selected_agent
            .as_ref()
            .map(|(_, cfg)| {
                cfg.allowed_tools
                    .iter()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let tools = self.tools.get()?;
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
            &resolved_provider_name,
            &resolved_model,
            &run_lineage,
        )
        .await?;

        if mode == "process" {
            let (process_config_dir, process_config_generation) =
                process_config_source.ok_or_else(|| anyhow::anyhow!("process config source was not resolved"))?;
            let process_provider_selection = if child_grant.is_some() || ordinary_publication.is_some() {
                let (provider, model) = child_requested_model
                    .as_ref()
                    .context("process child selection missing")?;
                Some(crate::session_worker::protocol::WorkerProviderSelection {
                    provider: provider.clone(),
                    model: model.clone(),
                    credential: child_credential.clone().context("process child credential missing")?,
                })
            } else {
                None
            };
            let process_source_launch = ordinary_publication
                .map(|publication| {
                    publication.capture_process_launch(
                        caller.as_ref().context("ordinary process caller missing")?,
                        ordinary_binding
                            .as_deref()
                            .context("ordinary process binding missing")?,
                        &spawn_scope_for_event,
                        task,
                        source_receipt.clone(),
                        process_provider_selection
                            .clone()
                            .context("ordinary process selection missing")?,
                        process_config_generation.clone(),
                    )
                })
                .transpose()?
                .map(crate::runtime::operator_spawn::process::ProcessSourceLaunch::Ordinary);
            let process_source_launch = match &operator_source {
                Some(source) => Some(crate::runtime::operator_spawn::process::ProcessSourceLaunch::Operator(
                    source.capture_process(
                        &spawn_scope_for_event,
                        task,
                        source_receipt.clone(),
                        process_config_generation.clone(),
                    )?,
                )),
                None => process_source_launch,
            };
            let temperature = resolved_temperature;
            let history_arc: Arc<RwLock<Vec<HistoryEntry>>> = Arc::new(RwLock::new(Vec::new()));
            let process_control = ProcessRunControl::new();
            // Process-mode steering: the sender lives in the registry exactly as
            // it does for task mode, so `sessions_send` / `sessions_spawn:steer`
            // reach an OS-process sub-agent through the same code path. The
            // receiver is handed to the child owner, which forwards each message
            // as a line-delimited control frame on the worker's stdin.
            let (process_steer_tx, process_steer_rx) = tokio::sync::mpsc::channel::<String>(STEER_CHANNEL_CAPACITY);
            // Minted here, on the *spawning* task, so the run's beat is parented
            // on the caller's: a parent legitimately blocked on a working child
            // must not look silent to its own hang detector. A beat minted
            // inside the spawned monitor would have no parent at all, because
            // task-locals do not cross `tokio::spawn`.
            let process_progress = crate::agent::idle::child_beat();

            {
                let mut runs = self.active_runs.write().await;
                runs.push(new_process_mode_run(ProcessModeRunSeed {
                    id: run_id.clone(),
                    task: task.to_string(),
                    lineage: &run_lineage,
                    recipient: recipient.clone(),
                    channel_name: run_channel_name.clone(),
                    process_control: process_control.clone(),
                    history: history_arc.clone(),
                    steer_tx: process_steer_tx.clone(),
                    parent_run_id: parent_run_id.clone(),
                    session_scope_key: session_scope_key.clone(),
                    spawn_depth,
                    progress: Arc::clone(&process_progress),
                    batch_id: batch_id.map(str::to_string),
                }));
            }

            let model = resolved_model;
            let provider_name = resolved_provider_name;
            let api_key = resolved_api_key;
            let route_hint = selected_route_hint;
            let workspace_root = child_model_config.workspace_dir.clone();
            let worker_workspace_root = spawn_config
                .worker_workspace_root
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| {
                    let path = std::path::PathBuf::from(value);
                    if path.is_absolute() {
                        path
                    } else {
                        workspace_root.join(path)
                    }
                })
                .unwrap_or_else(|| workspace_root.join("workers"));
            let active_runs = self.active_runs.clone();
            // Resolve the announce channel from the *per-turn* channel name bound
            // to this run (the launching message's scope), not the shared active
            // channel — so a concurrently-processed message cannot mis-route this
            // run's result. Falls back to the active channel when no scope name.
            let channel = match &operator_channel {
                Some(channel) => Arc::clone(channel),
                None => self.resolve_announce_channel(run_channel_name.as_deref()).await,
            };
            let keep_workspace = !spawn_config.cleanup_on_complete;
            let process_agent_id = selected_agent.as_ref().map(|(name, _)| name.clone());
            let identity_dir = selected_agent.as_ref().and_then(|(_, cfg)| {
                cfg.identity_dir
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            });
            // Process workers need the same explicit agent contract as task-mode
            // children.  Without carrying this prompt into the signed manifest,
            // a named agent's tool-use and completion instructions silently vanish
            // at the OS-process boundary.
            let process_system_prompt = selected_agent.as_ref().and_then(|(_, cfg)| {
                cfg.system_prompt
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            });
            let task_owned = task.to_string();
            let rid = run_id.clone();
            let process_scope = spawn_scope.clone();
            // Announcement gate inputs for the process-mode monitor. Same three
            // values the task-mode branch below captures, handed to the same
            // `deliver_or_withhold_notice`.
            let process_announce_security = Arc::clone(launch_security);
            let process_announce_origin = AnnounceOrigin::from_scope(spawn_scope.as_ref());
            let process_announce_history = history_arc.clone();
            let process_parent_run_id = parent_run_id.clone();
            let process_session_scope_key = session_scope_key.clone();
            let process_spawn_depth = spawn_depth;
            let process_channel_exclusions = inherited_channel_exclusions.clone();
            let process_compaction_config = resolved_compaction.config.clone();
            let process_event_recording = child_model_config.memory.event_recording_config();
            let process_execution_ctx = SpawnExecutionContext {
                operator_authority: None,
                ordinary_authority: None,
                incoming_grant: process_child_grant.clone(),
                provider_binding: None,
                terminal_usage: None,
                input_receipt: source_receipt.clone(),
                run_id: rid.clone(),
                session_scope_key: session_scope_key.clone(),
                spawn_depth,
                owner_id: run_lineage.owner_id.clone(),
                owner_isolated: run_lineage.owner_isolated,
                topic_id: run_lineage.topic_id.clone(),
                source_message_event_id: run_lineage.source_message_event_id.clone(),
                // A spawn-run context (this run may itself spawn): its children
                // compute spawn_depth + 1.
                is_turn_root: false,
                channel_excluded_tools: inherited_channel_exclusions.clone(),
            };
            let process_memory_fabric = Some(memory_fabric.clone());
            let process_result_scope = spawn_scope_for_event.clone();
            let process_lineage = run_lineage.clone();
            let process_cost_config = child_model_config.cost.clone();
            let monitor_process_control = process_control.clone();

            // Registry row for this run. The parent is captured *here*, on the
            // spawning task, because a freshly spawned task inherits no
            // task-locals. No abort handle is attached: this monitor is the sole
            // owner responsible for signalling and reaping the OS child, so
            // aborting it would strand the very process a kill is meant to end.
            // The row's cooperative token is how a kill reaches it instead —
            // see `bridge_registry_kill_to_process_control`, which asks that
            // same owner to stop. A cascade additionally signals the worker's
            // process group through the child row `run_sub_agent_process`
            // registers underneath.
            let sub_agent_parent = crate::runtime::registry::current_work_id();
            let sub_agent_label = process_agent_id
                .clone()
                .unwrap_or_else(|| "sub-agent (process)".to_string());
            let mut sub_agent_work =
                register_killable_process_run(&sub_agent_label, &rid, sub_agent_parent, batch_id, &process_control);
            // Publish this run's steering channel on its registry row so the
            // control plane can address it by run id from another entry point.
            // Same sender, same bounded queue as `sessions_send` — only the
            // lookup is new; see `registry::attach_steer_sender`.
            crate::runtime::registry::attach_steer_sender(sub_agent_work.id(), process_steer_tx.clone());
            sub_agent_work.require_cleanup_acknowledgement();
            let process_work_id = sub_agent_work.id();
            let process_recovery = process_child_grant.as_ref().map(|grant| {
                Arc::new(process_terminal::ProcessRecovery::new(
                    process_memory_fabric.clone(),
                    Arc::clone(grant),
                    source_receipt.clone(),
                    batch_id.map(str::to_owned),
                ))
            });
            let is_ordinary_process = process_source_launch
                .as_ref()
                .is_some_and(|launch| launch.is_ordinary());
            let ordinary_recovery = if is_ordinary_process {
                Some(process_terminal::ordinary::OrdinaryProcessRecovery::new(
                    process_memory_fabric
                        .clone()
                        .context("ordinary recovery fabric missing")?,
                    workspace_root.clone(),
                    run_lineage
                        .owner_id
                        .clone()
                        .context("ordinary recovery owner missing")?,
                    run_id.clone(),
                )?)
            } else {
                None
            };
            let is_operator_process = process_source_launch
                .as_ref()
                .is_some_and(|launch| !launch.is_ordinary());
            let operator_recovery = if is_operator_process {
                Some(process_terminal::operator::OperatorProcessRecovery::new(
                    process_memory_fabric
                        .clone()
                        .context("operator recovery fabric missing")?,
                    workspace_root.clone(),
                    run_lineage
                        .owner_id
                        .clone()
                        .context("operator recovery owner missing")?,
                    run_id.clone(),
                )?)
            } else {
                None
            };
            let process_batch_id = batch_id.map(str::to_owned);
            #[cfg(test)]
            let test_worker_executable = self.test_worker_executable.clone();
            let monitor_recovery = process_recovery.clone();
            let verified_terminal = Arc::new(parking_lot::Mutex::new(None));
            let monitor_verified_terminal = Arc::clone(&verified_terminal);
            let ordinary_terminal = Arc::new(parking_lot::Mutex::new(None));
            let monitor_ordinary_terminal = Arc::clone(&ordinary_terminal);
            let operator_terminal = Arc::new(parking_lot::Mutex::new(None));
            let monitor_operator_terminal = Arc::clone(&operator_terminal);
            let monitor_active_runs = active_runs.clone();
            let monitor_progress = Arc::clone(&process_progress);
            let jh = tokio::spawn(crate::runtime::registry::scope_current(
                process_work_id,
                SPAWN_EXECUTION_CONTEXT.scope(
                    process_execution_ctx,
                    crate::agent::idle::scope_beat(Some(Arc::clone(&monitor_progress)), async move {
                        let failure_active_runs = active_runs.clone();
                        let failure_run_id = rid.clone();
                        let monitor_result = std::panic::AssertUnwindSafe(async {
                            tracing::info!(run_id = %rid, "Sub-agent process starting");

                            let mut worker_result = run_sub_agent_process(
                                &rid,
                                &task_owned,
                                &provider_name,
                                &model,
                                api_key.as_deref(),
                                route_hint.as_deref(),
                                route_source_provider.as_deref(),
                                route_credential.as_ref(),
                                route_uses_root_connection,
                                direct_uses_root_connection,
                                temperature,
                                &workspace_root,
                                &worker_workspace_root,
                                identity_dir.as_deref(),
                                process_system_prompt.as_deref(),
                                &allowed_tools,
                                keep_workspace,
                                process_scope.as_ref(),
                                process_spawn_depth,
                                &process_session_scope_key,
                                process_parent_run_id.as_deref(),
                                process_agent_id.as_deref(),
                                &process_lineage,
                                &process_memory_strategy,
                                &process_memory_backend,
                                &process_config_dir,
                                &process_config_generation,
                                process_event_recording,
                                &process_compaction_config,
                                &process_channel_exclusions,
                                monitor_process_control.as_ref(),
                                process_steer_rx,
                                history_arc,
                                Arc::clone(&monitor_progress),
                                process_provider_selection.as_ref(),
                                process_memory_fabric.as_ref(),
                                monitor_recovery.as_deref(),
                                process_source_launch,
                                process_batch_id.as_deref(),
                                #[cfg(test)]
                                test_worker_executable.as_ref(),
                            )
                            .await;

                            let recovered_usage = if matches!(
                                &worker_result,
                                Ok(ProcessWorkerOutcome::Terminated(_) | ProcessWorkerOutcome::ExitedWithoutResult(_))
                            ) {
                                let context = current_spawn_execution_context();
                                if let Some(grant) = context.as_ref().and_then(|value| value.incoming_grant.as_ref()) {
                                    let recovery = async {
                                        let fabric = process_memory_fabric.as_ref().context("worker recovery storage missing")?;
                                        let input = context.as_ref().and_then(|value| value.input_receipt.as_deref())
                                            .context("worker recovery input missing")?;
                                        if let Some(completion) = process_terminal::recover_completion(fabric, grant, input).await? {
                                            Ok::<_, anyhow::Error>((Some(completion), None))
                                        } else {
                                            let usage = process_terminal::recover_usage(fabric, grant, input).await?;
                                            Ok((None, Some(usage)))
                                        }
                                    }.await;
                                    match recovery {
                                        Ok((completion, usage)) => {
                                            if let Some(completion) = completion {
                                                worker_result = Ok(ProcessWorkerOutcome::VerifiedFinished(completion));
                                            }
                                            usage
                                        }
                                        Err(error) => {
                                            tracing::error!(run_id = %rid, error = %error,
                                                "worker interruption settlement remains unconfirmed");
                                            return;
                                        }
                                    }
                                } else { None }
                            } else { None };
                            let (status, mut result_text, mut token_usage_records, finalization) = match worker_result {
                                Ok(ProcessWorkerOutcome::VerifiedFinished(completion)) => {
                                    *monitor_verified_terminal.lock() = Some(completion.clone());
                                    let (result, usage) = completion.into_parts();
                                    let (status, text) = if result.success {
                                        (SubAgentStatus::Completed(result.output.clone()), result.output)
                                    } else {
                                        let error = result.error.unwrap_or_else(|| "worker failed".to_owned());
                                        (
                                            SubAgentStatus::Failed(sub_agent_reported_failure(&error)),
                                            format!("Sub-agent error: {error}"),
                                        )
                                    };
                                    (status, text, usage, ProcessFinalization::Natural)
                                }
                                Ok(ProcessWorkerOutcome::OrdinaryVerifiedFinished(completion)) => {
                                    *monitor_ordinary_terminal.lock() = Some(completion.clone());
                                    let result = completion.result;
                                    let (status, text) = if result.success {
                                        (SubAgentStatus::Completed(result.output.clone()), result.output)
                                    } else {
                                        let error = result.error.unwrap_or_else(|| "worker failed".to_owned());
                                        (SubAgentStatus::Failed(sub_agent_reported_failure(&error)), format!("Sub-agent error: {error}"))
                                    };
                                    (status, text, completion.usage, ProcessFinalization::Natural)
                                }
                                Ok(ProcessWorkerOutcome::OperatorVerifiedFinished(completion)) => {
                                    *monitor_operator_terminal.lock() = Some(completion.clone());
                                    let result = completion.result;
                                    let (status, text) = if result.success {
                                        (SubAgentStatus::Completed(result.output.clone()), result.output)
                                    } else {
                                        let error = result.error.unwrap_or_else(|| "worker failed".to_owned());
                                        (SubAgentStatus::Failed(sub_agent_reported_failure(&error)), format!("Sub-agent error: {error}"))
                                    };
                                    (status, text, completion.usage, ProcessFinalization::Natural)
                                }
                                Ok(ProcessWorkerOutcome::VerifiedStartupFailure(result)) => {
                                    let error = result.error.context("verified startup failure detail missing");
                                    let error = match error {
                                        Ok(error) => error,
                                        Err(error) => {
                                            tracing::error!(run_id = %rid, error = %error, "verified startup result invalid");
                                            return;
                                        }
                                    };
                                    (
                                        SubAgentStatus::Failed(sub_agent_reported_failure(&error)),
                                        format!("Sub-agent startup error: {error}"),
                                        Vec::new(),
                                        ProcessFinalization::Natural,
                                    )
                                }
                                Ok(ProcessWorkerOutcome::UnverifiedStartupFailure(_)) => {
                                    tracing::error!(run_id = %rid, "unverified startup failure escaped process owner");
                                    return;
                                }
                                Ok(ProcessWorkerOutcome::Finished(result)) if result.success => {
                                    let token_usage_records = result
                                        .tokens_used
                                        .as_ref()
                                        .and_then(|usage| {
                                            crate::llm::route_decision::MeteredTokenUsageRecord::from_parts(
                                                &provider_name,
                                                &model,
                                                usage,
                                                &process_cost_config,
                                            )
                                        })
                                        .into_iter()
                                        .collect::<Vec<_>>();
                                    (
                                        SubAgentStatus::Completed(result.output.clone()),
                                        result.output,
                                        token_usage_records,
                                        ProcessFinalization::Natural,
                                    )
                                }
                                Ok(ProcessWorkerOutcome::Finished(result)) => {
                                    let error = result.error.unwrap_or_else(|| "worker failed".to_string());
                                    let msg = format!("Sub-agent error: {error}");
                                    // `result.error` is a field of the JSON the
                                    // worker itself printed, so it is the run's
                                    // account of the run — quarantined here so
                                    // it cannot pass for a runtime wording in
                                    // `termination_cause`. The announcement
                                    // above still shows the raw account.
                                    (
                                        SubAgentStatus::Failed(sub_agent_reported_failure(&error)),
                                        msg,
                                        Vec::new(),
                                        ProcessFinalization::Natural,
                                    )
                                }
                                Ok(ProcessWorkerOutcome::Terminated(reason)) => {
                                    let msg = format!("Sub-agent process terminated: {reason}");
                                    (
                                        SubAgentStatus::Failed(reason),
                                        msg,
                                        Vec::new(),
                                        ProcessFinalization::Terminated,
                                    )
                                }
                                Ok(ProcessWorkerOutcome::OrdinaryInterrupted { detail, usage }) => {
                                    let error = format!("{}; cleanup and provider usage remain unconfirmed; {} visible segments lack a terminal",
                                        exited_without_result_reason(&detail), usage.unfinished_segments);
                                    (SubAgentStatus::Failed(error.clone()), format!("Sub-agent process error: {error}"),
                                        usage.records, ProcessFinalization::Unconfirmed)
                                }
                                Ok(ProcessWorkerOutcome::OperatorInterrupted { detail, usage }) => {
                                    let error = format!("{}; cleanup and provider usage remain unconfirmed; {} visible segments lack a terminal",
                                        exited_without_result_reason(&detail), usage.unfinished_segments);
                                    (SubAgentStatus::Failed(error.clone()), format!("Sub-agent process error: {error}"),
                                        usage.records, ProcessFinalization::Unconfirmed)
                                }
                                Ok(ProcessWorkerOutcome::ExitedWithoutResult(detail)) => {
                                    // MUTATION GUARD: folding this back into the
                                    // generic parse-error path loses the only
                                    // wording an operator can act on, and folding it
                                    // into "keep waiting" would leave the run
                                    // `Running` for as long as the process lives.
                                    let error = exited_without_result_reason(&detail);
                                    let msg = format!("Sub-agent process error: {error}");
                                    (
                                        SubAgentStatus::Failed(error),
                                        msg,
                                        Vec::new(),
                                        ProcessFinalization::Natural,
                                    )
                                }
                                Ok(ProcessWorkerOutcome::TerminationFailed(error)) => {
                                    let msg = format!("Sub-agent process termination failed: {error}");
                                    (
                                        SubAgentStatus::Failed(error),
                                        msg,
                                        Vec::new(),
                                        ProcessFinalization::TerminationFailed,
                                    )
                                }
                                Err(error) => {
                                    let msg = format!("Sub-agent process error: {error}");
                                    (
                                        SubAgentStatus::Failed(error.to_string()),
                                        msg,
                                        Vec::new(),
                                        if is_ordinary_process || is_operator_process || current_spawn_execution_context().as_ref()
                                            .is_some_and(|context| context.incoming_grant.is_some()) {
                                            ProcessFinalization::Unconfirmed
                                        } else { ProcessFinalization::Natural },
                                    )
                                }
                            };

                            if let Some(recovered) = recovered_usage {
                                token_usage_records.extend(recovered.records);
                                result_text.push_str(&format!(
                                    "; provider usage may be incomplete ({} recorded segments lack terminals)",
                                    recovered.unfinished_segments,
                                ));
                            }

                            let announce = format_announce_message(&rid, &status, &result_text);
                            if let Err(error) = commit_process_terminal_state(
                                &active_runs,
                                &rid,
                                status.clone(),
                                token_usage_records,
                                monitor_process_control.as_ref(),
                                finalization,
                                None,
                                #[cfg(test)]
                                None,
                            )
                            .await {
                                tracing::error!(run_id = %rid, error = %error, "process terminal registry commit remains unconfirmed");
                                return;
                            }
                            if finalization == ProcessFinalization::Unconfirmed {
                                return;
                            }
                            record_spawn_result_event(
                                process_memory_fabric.as_ref(),
                                process_result_scope,
                                &result_text,
                                &status,
                                &process_lineage,
                                (finalization == ProcessFinalization::Terminated).then_some("task.killed"),
                            )
                            .await;

                            if !announce_result {
                                tracing::debug!(
                                    run_id = %rid,
                                    "Sub-agent process result withheld from the channel (announce=false); \
                                     it is delivered to whoever joins this run's batch"
                                );
                            } else if let Some(target) = recipient {
                                // Not a mirror of the task-mode block any more:
                                // the same function, so the gate this path gets
                                // is literally the gate the task-mode tests
                                // exercise. Driving a real process-mode
                                // announcement from a unit test is impossible
                                // (the worker re-execs `current_exe`, which under
                                // `cargo test` is the test binary), so sharing the
                                // implementation is what covers it.
                                deliver_or_withhold_notice(
                                    &process_announce_security,
                                    process_announce_origin.as_ref(),
                                    channel.as_ref(),
                                    &process_announce_history,
                                    &rid,
                                    &target,
                                    &announce,
                                    OutboundNotice::ProcessRunResult,
                                )
                                .await;
                            }

                            tracing::info!(run_id = %rid, "Sub-agent process finished");
                        })
                        .catch_unwind()
                        .await;

                        if monitor_result.is_err() {
                            commit_process_owner_failure_if_unfinalized(
                                &failure_active_runs,
                                &failure_run_id,
                                monitor_process_control.as_ref(),
                                PROCESS_OWNER_PANICKED_PREFIX.to_string(),
                            )
                            .await;
                        }
                    }),
                ),
            ));

            // Process-mode callers must never abort this monitor: it is the
            // sole owner responsible for signalling and reaping the OS child.
            // Awaiting the handle from a *separate* task does not abort it, and
            // is the only way a cancellation of the monitor (runtime shutdown,
            // a future registry abort) can still reach the tool-plane row.
            watch_process_mode_monitor(
                jh,
                monitor_active_runs,
                run_id.clone(),
                process_control.clone(),
                sub_agent_work,
                verified_terminal,
                process_recovery,
                ordinary_terminal,
                ordinary_recovery,
                operator_terminal,
                operator_recovery,
            );

            return Ok(SpawnOutcome::started(
                run_id.clone(),
                format!(
                    "Sub-agent spawned in process mode (run_id: {run_id}). {}",
                    spawn_completion_note(announce_result)
                ),
            ));
        }

        // Create shared history and steer channel for this run
        let history_arc: Arc<RwLock<Vec<HistoryEntry>>> = Arc::new(RwLock::new(Vec::new()));
        // Class B — bounded. Steering messages are produced by operators and by
        // model tool calls, neither of which is rate-limited now that sub-agent
        // concurrency is uncapped, while the consumer only polls between tool
        // iterations. An unbounded queue therefore grows for the whole lifetime
        // of a long-running sub-agent.
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(STEER_CHANNEL_CAPACITY);
        // Minted on the spawning task so the run's beat is parented on the
        // caller's (see `crate::agent::idle::child_beat`). The sub-agent's own
        // guarded turn parents *its* beat on this one, so every ProgressKind
        // event it produces refreshes this row and the caller's window alike.
        let task_progress = crate::agent::idle::child_beat();

        // Clone everything the spawned task needs.
        // Resolve the announce channel from the *per-turn* channel name bound to
        // this run (the launching message's scope), not the shared active channel
        // — so a concurrently-processed message cannot mis-route this run's
        // result. Falls back to the active channel when no scope name is present.
        let channel = match &operator_channel {
            Some(channel) => Arc::clone(channel),
            None => self.resolve_announce_channel(run_channel_name.as_deref()).await,
        };
        let mut provider_name = resolved_provider_name;
        let mut model = resolved_model;
        let child_provider_binding = if let Some(grant) = child_grant.as_ref() {
            let credential = child_credential.clone().context("child credential selection missing")?;
            let (requested_child_provider, requested_child_model) =
                child_requested_model.as_ref().context("child model request missing")?;
            let binding = Arc::new(crate::runtime::child_source::ChildProviderBinding::build(
                Arc::clone(grant),
                requested_child_provider,
                requested_child_model,
                credential,
            )?);
            provider_name = binding.provider_name().to_owned();
            model = binding.model().to_owned();
            Some(binding)
        } else if let Some(source) = &operator_source {
            let binding = Arc::new(crate::runtime::child_source::ChildProviderBinding::build_operator(
                source,
            )?);
            provider_name = binding.provider_name().to_owned();
            model = binding.model().to_owned();
            Some(binding)
        } else if let Some(binding) = ordinary_binding {
            provider_name = binding.provider_name().to_owned();
            model = binding.model().to_owned();
            Some(binding)
        } else {
            None
        };
        let temperature = resolved_temperature;
        // An explicit hint always builds its exact selected chain, even when
        // it names the gateway provider: the gateway may hold another route's
        // credential. Direct models keep their existing provider plan.
        if let Some(source) = &operator_source {
            source.verify_live()?;
            let binding = child_provider_binding
                .as_ref()
                .context("operator child model binding missing")?;
            anyhow::ensure!(
                Arc::ptr_eq(&binding.provider(), source.provider()),
                "operator child provider differs from captured ingress"
            );
        }
        let provider = child_provider_binding
            .as_ref()
            .context("admitted sessions_spawn child model binding missing")?
            .provider();
        let active_runs = self.active_runs.clone();
        let rid = run_id.clone();
        // Event bridge (v1.1a): if a chat-side sink is attached, create this
        // session's middle channels + drainer up front (run_id is already
        // minted, so the drainer is tagged with the correct id — no race). The
        // background agent only ever `.send().await`s onto these; the drainer
        // continuously empties them, so the agent never back-pressures.
        let run_event_streams = self.event_sink.as_ref().map(|sink| sink.streams_for(&run_id));
        // NeedsInput (chat `/bg` only): mint this run's approval resolver so a
        // supervised gate hit suspends awaiting an operator decision instead of
        // auto-failing. `None` everywhere else (channels/gateway) preserves the
        // historical auto-fail-on-gate semantics.
        let run_approval_resolver = self
            .approval_resolver_factory
            .as_ref()
            .map(|factory| factory.resolver_for(&run_id));
        // NeedsInput: when (and only when) an approval resolver is attached, hand
        // the loop the run registry + id so it can deterministically restore
        // `AwaitingInput` -> `Running` on cancel-and-resume (steer). `None`
        // elsewhere — without a resolver no run can ever suspend.
        let (restore_active_runs, restore_run_id) = if run_approval_resolver.is_some() {
            (Some(self.active_runs.clone()), Some(run_id.clone()))
        } else {
            (None, None)
        };
        let task_owned = task.to_string();
        let identity_workspace = child_model_config.workspace_dir.clone();
        let security = Arc::clone(launch_security);
        // Second handle, for the announcement gate: `security` above is moved
        // into the sub-agent's own loop, and the announcement is authorized only
        // once that loop has returned.
        let announce_security = Arc::clone(launch_security);
        // The launching turn's trusted identity, captured atomically with
        // `recipient` / `run_channel_name` from the same scope snapshot.
        let announce_origin = AnnounceOrigin::from_scope(spawn_scope.as_ref());
        // Same handle the run writes its transcript through, so a refused
        // announcement is visible on the run itself and not only in the log.
        let announce_history = history_arc.clone();
        let mut terminal_usage = child_grant.as_ref().map(|grant| {
            Arc::new(crate::runtime::turn_terminal::TerminalUsageCollector::new(
                grant
                    .generation()
                    .effective
                    .workspace_dir
                    .to_string_lossy()
                    .into_owned(),
                grant.scope().gate_inputs().0.authority.owner.clone(),
                rid.clone(),
            ))
        });
        let mut task_execution_ctx = SpawnExecutionContext {
            operator_authority: None,
            ordinary_authority: None,
            incoming_grant: child_grant,
            provider_binding: child_provider_binding,
            terminal_usage: terminal_usage.clone(),
            input_receipt: source_receipt,
            run_id: rid.clone(),
            session_scope_key: session_scope_key.clone(),
            spawn_depth,
            owner_id: run_lineage.owner_id.clone(),
            owner_isolated: run_lineage.owner_isolated,
            topic_id: run_lineage.topic_id.clone(),
            source_message_event_id: run_lineage.source_message_event_id.clone(),
            // A spawn-run context (this run may itself spawn): its children
            // compute spawn_depth + 1.
            is_turn_root: false,
            channel_excluded_tools: inherited_channel_exclusions.clone(),
        };
        let task_memory_fabric = memory_fabric.clone();
        let task_result_scope = spawn_scope_for_event.clone();
        let task_lineage = run_lineage.clone();
        let agent_label_for_registry = selected_agent.as_ref().map(|(name, _)| name.clone());
        let (system_prompt, filtered_tools) = if let Some((agent, cfg)) = selected_agent {
            let identity_prompt = cfg
                .identity_dir
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|identity_dir| build_identity_prompt(&identity_workspace.join(identity_dir)))
                .unwrap_or_default();
            let prompt = if identity_prompt.trim().is_empty() {
                DEFAULT_SUB_AGENT_SYSTEM_PROMPT.to_string()
            } else {
                identity_prompt
            };
            let memory_scope = parse_memory_scope(cfg.memory_scope.as_deref())?;
            let tools = tools.map(|registry| {
                if ordinary_publication.is_some() {
                    // Keep the backend root identity and dynamic metadata for
                    // the sealed catalog. The child service enforces the exact
                    // manifest, agent allowance and inherited exclusions.
                    let memory_prefix = (memory_scope == MemoryScope::Isolated).then(|| agent.clone());
                    return Arc::new(
                        registry
                            .iter()
                            .map(|tool| {
                                Box::new(AllowedToolProxy {
                                    source: Arc::clone(&registry),
                                    public_name: tool.name().to_owned(),
                                    memory_prefix: memory_prefix.clone(),
                                }) as Box<dyn Tool>
                            })
                            .collect(),
                    );
                }
                resolve_tools_for_agent(
                    registry,
                    &agent,
                    memory_scope,
                    if cfg.allowed_tools.is_empty() {
                        None
                    } else {
                        Some(&cfg.allowed_tools)
                    },
                    &inherited_channel_exclusions,
                )
            });
            (prompt, tools)
        } else {
            (DEFAULT_SUB_AGENT_SYSTEM_PROMPT.to_string(), tools)
        };

        // Register the run.
        //
        // A background run outlives its spawning turn. Its independent observer
        // owns cleanup; parenting this to the caller would cancel valid work
        // when that caller returns. Register before publishing the active row.
        let task_control = TaskRunControl::new();
        if let Some(source) = &operator_source {
            let (authority, lifetime) = source.issue_task(
                Arc::clone(
                    task_execution_ctx
                        .provider_binding
                        .as_ref()
                        .context("operator model binding missing")?,
                ),
                &task_result_scope,
                task,
                task_execution_ctx.input_receipt.as_deref(),
                task_control.cancellation(),
            )?;
            task_control.retain_operator_lifetime(lifetime)?;
            use crate::runtime::turn_source::TurnSourceProof;
            let usage = Arc::new(crate::runtime::turn_terminal::TerminalUsageCollector::new_ordinary(
                authority.workspace_id().to_owned(),
                authority.owner_id().to_owned(),
                rid.clone(),
                authority.generation().effective.memory.event_recording_config(),
            ));
            terminal_usage = Some(Arc::clone(&usage));
            task_execution_ctx.terminal_usage = Some(usage);
            task_execution_ctx.operator_authority = Some(authority);
        }
        if ordinary_publication.is_some() {
            let binding = task_execution_ctx
                .provider_binding
                .as_ref()
                .context("ordinary task provider binding missing")?;
            let caller = caller.as_ref().context("ordinary task caller missing")?;
            let (authority, lifetime) = binding.issue_ordinary_task(
                caller,
                &task_result_scope,
                task,
                task_execution_ctx.input_receipt.as_deref(),
                task_control.cancellation(),
            )?;
            task_control.retain_ordinary_lifetime(lifetime)?;
            use crate::runtime::turn_source::TurnSourceProof;
            let (proof, _) = authority.source()?;
            let usage = Arc::new(crate::runtime::turn_terminal::TerminalUsageCollector::new_ordinary(
                proof.workspace_id().to_owned(),
                proof.owner_id().to_owned(),
                rid.clone(),
                authority
                    .publication()
                    .generation()
                    .effective
                    .memory
                    .event_recording_config(),
            ));
            terminal_usage = Some(Arc::clone(&usage));
            task_execution_ctx.terminal_usage = Some(usage);
            task_execution_ctx.ordinary_authority = Some(authority);
        }
        let logical_terminal = {
            use crate::agent::terminal::logical::LogicalTaskTerminalRef;
            match (
                task_execution_ctx.incoming_grant.as_deref(),
                task_execution_ctx.ordinary_authority.as_deref(),
                task_execution_ctx.operator_authority.as_deref(),
            ) {
                (Some(grant), None, None) => LogicalTaskTerminalRef::granted(
                    grant,
                    task_execution_ctx
                        .input_receipt
                        .as_deref()
                        .context("logical task input missing")?,
                    &task_result_scope,
                )?,
                (None, Some(authority), None) => LogicalTaskTerminalRef::ordinary(authority)?,
                (None, None, Some(authority)) => LogicalTaskTerminalRef::operator(authority)?,
                _ => anyhow::bail!("logical task completion requires one issued source authority"),
            }
        };
        let run_cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let run_cleanup_scope =
            crate::runtime::turn_cleanup::CleanupScope::new(Some(&run_cleanup), CancellationToken::new())?;

        // Deliberately the *last* fallible-free step before the driver task is
        // spawned, and never earlier: everything between this block and
        // `tokio::spawn` below is infallible, so the presence of a row in
        // `active_runs` implies a driver task that will publish a terminal status
        // for it. Registering before the fallible preparation above (agent prompt
        // and tool resolution) left a `Running` row behind on every early return —
        // no abort handle, no driver task, no observer — and `join` polls such a
        // row forever, with no wall-clock fallback in this runtime to break the
        // tie. Keep any new fallible step above this block.
        //
        // `abort_handle` is filled in right after `tokio::spawn` returns.
        {
            let mut runs = self.active_runs.write().await;
            runs.push(SubAgentRun {
                id: run_id.clone(),
                task: task.to_string(),
                owner_id: run_lineage.owner_id.clone(),
                topic_id: run_lineage.topic_id.clone(),
                source_message_event_id: run_lineage.source_message_event_id.clone(),
                started_at: Utc::now(),
                finished_at: None,
                status: SubAgentStatus::Running,
                recipient: recipient.clone(),
                channel_name: run_channel_name.clone(),
                abort_handle: None,
                process_control: None,
                task_control: Some(Arc::clone(&task_control)),
                history: history_arc.clone(),
                steer_tx: Some(steer_tx.clone()),
                parent_run_id: parent_run_id.clone(),
                session_scope_key: session_scope_key.clone(),
                spawn_depth,
                token_usage_records: Vec::new(),
                progress: Arc::clone(&task_progress),
                batch_id: batch_id.map(str::to_string),
            });
        }

        // Registry row for this run. Parent captured on the spawning task, which
        // is the tool call that requested the spawn; a spawned task inherits no
        // task-locals of its own.
        let sub_agent_parent = crate::runtime::registry::current_work_id();
        let sub_agent_label = agent_label_for_registry.unwrap_or_else(|| "sub-agent".to_string());
        let mut sub_agent_work =
            crate::runtime::registry::register_sub_agent(&sub_agent_label, &rid, sub_agent_parent, batch_id, None);
        let sub_agent_work_id = sub_agent_work.id();
        sub_agent_work.require_cleanup_acknowledgement();
        // As in the process branch: the control plane resolves `run_id` through
        // the process-wide work registry, so the run's existing steer sender is
        // published there. `active_runs` stays the tool-plane source of truth.
        crate::runtime::registry::attach_steer_sender(sub_agent_work_id, steer_tx.clone());

        let terminal_journal = Arc::new(task_terminal::TaskTerminalJournal::new(
            task_memory_fabric.clone(),
            task_result_scope.clone(),
            logical_terminal,
            true,
            terminal_usage,
        ));
        let driver_terminal_journal = Arc::clone(&terminal_journal);
        // Spawn async task (fire-and-forget); capture handle to support kill
        let jh = tokio::spawn(crate::runtime::registry::scope_current(
            sub_agent_work_id,
            SPAWN_EXECUTION_CONTEXT.scope(task_execution_ctx, async move {
                tracing::info!(run_id = %rid, "Sub-agent task starting");
                let provider_started_at = Utc::now();
                let (run_on_delta, run_on_tool) = match run_event_streams {
                    Some((delta_tx, tool_tx)) => (Some(delta_tx), Some(tool_tx)),
                    None => (None, None),
                };
                let run_future = run_sub_agent_task(
                    &task_owned,
                    provider,
                    &provider_name,
                    &model,
                    temperature,
                    filtered_tools,
                    &system_prompt,
                    security,
                    steer_rx,
                    history_arc,
                    run_on_delta,
                    run_on_tool,
                    run_approval_resolver,
                    restore_active_runs,
                    restore_run_id,
                    Arc::clone(&task_progress),
                );
                let result = run_cleanup_scope.run(run_future).await;
                tracing::info!(run_id = %rid, success = result.is_ok(), "Sub-agent task finished");

                let (status, result_text, terminal_status, history_projection, token_usage_records) = match result {
                    Ok(task_result) => (
                        SubAgentStatus::Completed(task_result.output.clone()),
                        task_result.output.clone(),
                        crate::agent::terminal::TurnTerminalStatus::Completed,
                        Some(crate::agent::terminal::TurnHistoryProjection {
                            assistant_content: task_result.output,
                            history_commit_len: task_result.history_commit_len,
                        }),
                        task_result.segment_usage,
                    ),
                    Err(error) => {
                        // Persist the same runtime-classified reason used by the
                        // live registry. Recovery after commit must not lose the
                        // quarantine tag on provider/tool-authored failure text.
                        let reason = task_failure_reason(&error);
                        let terminal_status = if matches!(termination_cause(&reason), TerminationCause::Killed) {
                            crate::agent::terminal::TurnTerminalStatus::Cancelled
                        } else {
                            crate::agent::terminal::TurnTerminalStatus::Failed
                        };
                        (
                            SubAgentStatus::Failed(reason.clone()),
                            reason,
                            terminal_status,
                            None,
                            Vec::new(),
                        )
                    }
                };

                let delivery_intent = recipient.as_ref().map_or_else(
                    || crate::agent::terminal::TurnDeliveryIntent::Deferred {
                        route: "sessions_registry".to_string(),
                    },
                    |target| crate::agent::terminal::TurnDeliveryIntent::Reply { target: target.clone() },
                );
                match driver_terminal_journal
                    .commit(crate::agent::terminal::TurnTerminalDraft {
                        decision: None,
                        terminal_id: rid.clone(),
                        scope: task_result_scope.clone(),
                        status: terminal_status,
                        history: history_projection,
                        history_scope: None,
                        provider_outcome: None,
                        telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                            summary: result_text.clone(),
                            started_at: provider_started_at,
                            finished_at: Utc::now(),
                        },
                        delivery_intent,
                    })
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(run_id = %rid, error = %error, "task terminal persistence remains unconfirmed");
                        // The external observer retries this exact proposal.
                        // Do not publish a result or a substitute usage receipt.
                        return;
                    }
                }
                let announce = format_announce_message(&rid, &status, &result_text);
                record_spawn_result_event(
                    Some(&task_memory_fabric),
                    task_result_scope,
                    &result_text,
                    &status,
                    &task_lineage,
                    None,
                )
                .await;

                // Update run status
                {
                    let mut runs = active_runs.write().await;
                    if let Some(run) = runs.iter_mut().find(|r| r.id == rid) {
                        run.finished_at = Some(Utc::now());
                        run.status = status;
                        run.token_usage_records.extend(token_usage_records);
                        run.steer_tx = None; // drop sender — no more steering possible
                    }
                }

                // Announce result back to channel if we have a recipient
                if !announce_result {
                    tracing::debug!(
                        run_id = %rid,
                        "Sub-agent result withheld from the channel (announce=false); \
                         it is delivered to whoever joins this run's batch"
                    );
                } else if let Some(target) = recipient {
                    // MUTATION GUARD: route this straight to `channel.send` and the
                    // model-supplied `recipient` on the spawn call reaches the
                    // channel with no authorization at all — the hole
                    // `message_send`'s own outbound gate closes on its side.
                    deliver_or_withhold_notice(
                        &announce_security,
                        announce_origin.as_ref(),
                        channel.as_ref(),
                        &announce_history,
                        &rid,
                        &target,
                        &announce,
                        OutboundNotice::RunResult,
                    )
                    .await;
                } else {
                    tracing::warn!(
                        run_id = %rid,
                        "Sub-agent completed but no recipient configured for announcement"
                    );
                }
            }),
        ));

        // Install the external owner before the first suspension after spawn.
        let abort_handle = jh.abort_handle();
        task_control.attach(abort_handle.clone());
        crate::runtime::registry::attach_abort_handle(sub_agent_work_id, abort_handle.clone());
        watch_task_mode_monitor(
            jh,
            self.active_runs.clone(),
            run_id.clone(),
            Some(run_cleanup),
            Some(task_control),
            Some(sub_agent_work),
            Some(terminal_journal),
        );
        {
            let mut runs = self.active_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
                run.abort_handle = Some(abort_handle);
            }
        }

        Ok(SpawnOutcome::started(
            run_id.clone(),
            format!(
                "Sub-agent spawned (run_id: {run_id}). {}",
                spawn_completion_note(announce_result)
            ),
        ))
    }
}
