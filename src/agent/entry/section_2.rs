/// The provider and model an agent entry runs on, resolved and owned.
///
/// The internal agent engine (cron and xin agent jobs, skill evaluation)
/// resolves their selection with
/// [`resolve_agent_model_choice`] before anything is constructed: before the
/// runtime bootstrap (memory hygiene, snapshots, the database, plugins,
/// tools), so a refused selection leaves no trace (D-065 M-1).
#[derive(Clone)]
pub(crate) struct AgentModelChoice {
    provider_name: String,
    model_name: String,
    model_binding: AgentModelBinding,
    resolved: crate::providers::catalog::ResolvedModel,
    catalog: crate::providers::catalog::CatalogView,
}

impl AgentModelChoice {
    pub(crate) const fn binding(&self) -> &AgentModelBinding {
        &self.model_binding
    }

    pub(crate) fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub(crate) fn model_name(&self) -> &str {
        &self.model_name
    }

    pub(crate) const fn catalog(&self) -> &crate::providers::catalog::CatalogView {
        &self.catalog
    }

    /// An injected provider may switch model IDs only within the connection
    /// identity already owned by that instance and pinned configuration.
    pub(crate) fn same_provider_connection(&self, other: &Self) -> bool {
        self.model_binding.provider == other.model_binding.provider
            && self.model_binding.provider_label == other.model_binding.provider_label
            && self.model_binding.credential_ref == other.model_binding.credential_ref
            && self.model_binding.credential_has_key == other.model_binding.credential_has_key
    }

    pub(crate) fn resolved_selection(&self) -> (&str, &str) {
        (&self.model_binding.provider_label, &self.model_binding.model)
    }

    /// Re-resolve the selected source against the pinned configuration and
    /// require the complete resolver release, including credential material,
    /// to be unchanged. Provider construction consumes `self.resolved`
    /// directly, so unrelated routes no longer affect this attestation.
    pub(crate) fn factory_credential_source_matches(&self, config: &Config) -> bool {
        if self.provider_name.trim() != self.provider_name {
            return false;
        }
        let route = match providers::selection::route_target(&self.catalog, &self.provider_name, &self.model_name) {
            Ok(route) => route.cloned(),
            Err(_) => return false,
        };
        if route != self.model_binding.route {
            return false;
        }
        providers::selection::check_with(
            config,
            &self.catalog,
            &self.provider_name,
            &self.model_name,
            self.resolved.credential().reference().clone(),
            &providers::catalog::Requirement::chat(),
            providers::catalog::UnknownPolicy::Invocation,
        )
        .is_ok_and(|fresh| fresh == self.resolved)
    }
}

/// Identity selected before provider construction. It deliberately excludes
/// credential material: the provider factory must receive its own pinned
/// material, while the decision service can compare route and owner facts.
#[derive(Clone)]
pub(crate) struct AgentModelBinding {
    provider: crate::providers::ProviderId,
    provider_label: Arc<str>,
    model: Arc<str>,
    credential_ref: crate::providers::credentials::CredentialRef,
    route: Option<crate::providers::catalog::RouteTarget>,
    verification: crate::providers::catalog::Verification,
    source: crate::providers::catalog::DescriptorSource,
    dialects: crate::providers::catalog::Dialects,
    credential_has_key: bool,
}

impl AgentModelBinding {
    /// Admit only the source spelling or the provider/model pinned at ingress.
    pub(crate) fn admits_execution(&self, source: (&str, &str), execution: (&str, &str)) -> bool {
        execution == source || execution == (self.provider_label.as_ref(), self.model.as_ref())
    }

    /// The model pinned at ingress, independent of any route spelling used to execute it.
    pub(crate) fn pinned_model(&self) -> &str {
        &self.model
    }

    /// The provider pinned at ingress: the one a route spelling (`hint:<name>`)
    /// resolved to, which may differ from the provider the selection named.
    pub(crate) fn pinned_provider_label(&self) -> &str {
        &self.provider_label
    }

    pub(crate) const fn credential_reference(&self) -> &crate::providers::credentials::CredentialRef {
        &self.credential_ref
    }

    pub(crate) fn from_resolved(
        resolved: &crate::providers::catalog::ResolvedModel,
        route: Option<crate::providers::catalog::RouteTarget>,
    ) -> Self {
        Self {
            provider: resolved.provider().clone(),
            provider_label: Arc::from(resolved.provider_label()),
            model: Arc::from(resolved.model()),
            credential_ref: resolved.credential().reference().clone(),
            route,
            verification: resolved.verification().clone(),
            source: resolved.source(),
            dialects: resolved.dialects(),
            credential_has_key: resolved.credential().api_key().is_some(),
        }
    }

    pub(crate) fn matches_identity(&self, resolved: &crate::providers::catalog::ResolvedModel) -> bool {
        self.provider == *resolved.provider()
            && self.provider_label.as_ref() == resolved.provider_label()
            && self.model.as_ref() == resolved.model()
            && self.credential_ref == *resolved.credential().reference()
    }

    pub(crate) fn matches(&self, resolved: &crate::providers::catalog::ResolvedModel) -> bool {
        self.matches_identity(resolved)
            && self.verification == *resolved.verification()
            && self.source == resolved.source()
            && self.dialects == resolved.dialects()
    }

    pub(crate) const fn route(&self) -> Option<&crate::providers::catalog::RouteTarget> {
        self.route.as_ref()
    }
}

/// Resolve the selection for an agent entry, check-only.
///
/// This is the one resolution of the entry. An operator's invocation runs an
/// id-only model with a warning and refuses one the evidence rules out
/// (D-034). The chain the entry builds is routed, so every `[[model_routes]]`
/// destination is resolved when the routed provider is built, and a `hint:`
/// model is checked as the destination it names (D-035).
pub(crate) fn resolve_agent_model_choice(
    config: &Config,
    provider_override: Option<&str>,
    model_override: Option<&str>,
    creator_agent: Option<&str>,
) -> Result<AgentModelChoice> {
    use crate::providers::credentials::{CredentialRef, LegacySlot};
    let named = creator_agent
        .map(|name| config.agents.get(name).ok_or_else(|| anyhow::anyhow!("creator_gone")))
        .transpose()?;
    let provider_name = named.map_or_else(
        || provider_override.unwrap_or_else(|| providers::selection::default_provider(config)),
        |agent| agent.provider.as_str(),
    );
    let model_name = named.map_or_else(
        || model_override.unwrap_or_else(|| providers::selection::default_model(config)),
        |agent| agent.model.as_str(),
    );
    let credential = named.map_or_else(
        || providers::selection::credential_for(config, provider_name),
        |agent| {
            agent.credential_ref.as_ref().map_or_else(
                || {
                    if agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
                        CredentialRef::LegacyInline(LegacySlot::Agent(creator_agent.unwrap_or_default().to_owned()))
                    } else {
                        providers::selection::credential_for(config, provider_name)
                    }
                },
                |id| CredentialRef::Profile { id: id.clone() },
            )
        },
    );
    let catalog = providers::selection::catalog_for(config);
    let route = providers::selection::route_target(&catalog, provider_name, model_name)?.cloned();
    let resolved = providers::selection::check_with(
        config,
        &catalog,
        provider_name,
        model_name,
        credential,
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )
    .map_err(|refusal| providers::selection::route_refusal(route.as_ref(), refusal))?;
    Ok(AgentModelChoice {
        provider_name: provider_name.to_string(),
        model_name: model_name.to_string(),
        model_binding: AgentModelBinding::from_resolved(&resolved, route),
        resolved,
        catalog,
    })
}

/// The provider an agent entry runs on, and the only connection it may use.
///
/// The root credential, base URL and rotation keys are released only when
/// the resolved selection is the provider identity they were configured for.
/// Anything else resolves its own credential (auth profile, or none for a
/// keyless `custom:` endpoint) or is refused at construction, before any
/// transport exists.
pub(crate) struct AgentProviderSelection<'a> {
    pub(crate) provider_name: &'a str,
    pub(crate) model_name: &'a str,
    resolved: &'a crate::providers::catalog::ResolvedModel,
    catalog: &'a crate::providers::catalog::CatalogView,
}

/// Scope the connection for a resolved selection. Resolution already ran in
/// [`resolve_agent_model_choice`]; this does not resolve again.
pub(crate) fn agent_provider_selection(choice: &AgentModelChoice) -> AgentProviderSelection<'_> {
    AgentProviderSelection {
        provider_name: &choice.provider_name,
        model_name: &choice.model_name,
        resolved: &choice.resolved,
        catalog: &choice.catalog,
    }
}

impl AgentProviderSelection<'_> {
    /// Build the routed provider chain from the resolver-owned selection.
    pub(crate) fn build_provider(
        &self,
        config: &Config,
        options: &providers::ProviderRuntimeOptions,
    ) -> Result<Box<dyn Provider>> {
        providers::create_routed_provider_for(self.resolved, self.catalog, config, options)
    }
}

/// P0-B: a provider override on the agent entry must give the override only its own
/// credential. Driven through the production seam, the real factory and a
/// loopback endpoint; every credential is a sentinel and the config directory
/// is a fresh temporary one with no auth profiles.
#[path = "../turn_tests/entry_provider_selection.rs"]
mod agent_provider_selection_tests;

/// Run the Agent entry with a caller-owned runtime envelope. This preserves
/// owner/topic/task/source lineage for scheduler-driven single-shot turns while
/// keeping the public CLI entrypoint's existing defaults.
///
/// `scheduled_tool_scope` is the tool context and policy a cron job or xin
/// task runs under when its `JobOrigin` is not the local operator
/// (CRON-ORIGIN); `None` keeps the local-operator CLI context and policy. It
/// governs a single-shot turn only.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_with_runtime_envelope(
    config_source: impl Into<crate::runtime::bootstrap::AgentRuntimeConfig>,
    message: String,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
    shutdown: CancellationToken,
    runtime_envelope_override: Option<RuntimeEnvelope>,
    scheduled_tool_scope: Option<crate::tools::execution::ScheduledToolScope>,
) -> Result<String> {
    let config_source = config_source.into();
    let config = config_source.effective();
    let named_job = super::automation::select(config, runtime_envelope_override.as_ref())?;
    let mut runtime_envelope_override = runtime_envelope_override;
    let mut scheduled_tool_scope = scheduled_tool_scope;
    if let (Some(job), Some(envelope)) = (&named_job, runtime_envelope_override.as_mut()) {
        envelope.agent_partition = Some(job.partition(envelope));
        if let Some(scope) = scheduled_tool_scope.as_mut() {
            scope.context.envelope.agent_partition = envelope.agent_partition.clone();
        }
    }
    let scheduled_card = super::scheduled_source::current_card();
    let mut scheduled_admission = super::scheduled_source::validate_current(
        runtime_envelope_override.as_ref(),
        scheduled_tool_scope.as_ref(),
        &shutdown,
    )
    .await?;
    anyhow::ensure!(
        runtime_envelope_override.is_none() || scheduled_admission.is_some(),
        "agent runtime override requires an admitted scheduled source"
    );
    if let (Some(job), Some(admission), Some(envelope)) = (
        &named_job,
        scheduled_admission.as_mut(),
        runtime_envelope_override.as_ref(),
    ) {
        admission.job_metadata = Some(job.metadata(envelope));
    }
    // Validate scoped identity before bootstrap can initialize persistence.
    // Scheduled sources retain their attempt/lease run id through terminal
    // recording and child lineage instead of acquiring a second CLI run id.
    let scoped_run_id = runtime_envelope_override
        .as_ref()
        .map(|envelope| {
            anyhow::ensure!(
                envelope.workspace_id == config.workspace_dir.to_string_lossy()
                    && !envelope.session_key.trim().is_empty(),
                "scoped runtime workspace or session does not match this invocation"
            );
            let run_id = envelope
                .run_id
                .as_deref()
                .filter(|run_id| !run_id.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("a scoped runtime run requires its source-owned run identity"))?;
            Ok::<_, anyhow::Error>(run_id.to_owned())
        })
        .transpose()?;
    // Manual scheduled tool calls inherit this owner in ordinary single-shot
    // execution. Let the entry's existing shutdown branch close its own turn;
    // cancellation of this child scope must not race and drop that branch.
    let caller_cleanup = if scheduled_admission.is_none() {
        let parent = crate::runtime::turn_cleanup::current_tracker();
        Some(crate::runtime::turn_cleanup::CleanupScope::new(
            parent.as_ref(),
            CancellationToken::new(),
        )?)
    } else {
        None
    };

    // D-065 M-1: resolve before the bootstrap, so a refused selection runs
    // no memory hygiene, snapshot, database, plugin or tool setup.
    let temperature = named_job
        .as_ref()
        .and_then(|job| config.agents.get(job.name.as_str()))
        .and_then(|agent| agent.temperature)
        .unwrap_or(temperature);
    let choice = resolve_agent_model_choice(
        config,
        provider_override.as_deref(),
        model_override.as_deref(),
        named_job.as_ref().map(|job| job.name.as_str()),
    )?;
    // ── Wire up subsystems via RuntimeBootstrap (D1 step 2) ──────
    // The internal agent engine is always a full single-shot run with no
    // memory-only early-exit (unlike chat's `--list-sessions`), so it always
    // needs memory + tools. It uses the dedicated `AgentLoop` profile: identical
    // core to `Interactive` (memory + tools, no router), with the published
    // source-bound turn decision service. Bootstrap constructs subsystems in
    // observer/security/memory/runtime/tools order.
    let ctx = crate::runtime::bootstrap::RuntimeBootstrap::build_agent(
        config_source,
        runtime_envelope_override.as_ref(),
        scheduled_tool_scope.as_ref(),
    )
    .await?;

    let _agent_permit = if let (Some(job), Some(envelope)) = (&named_job, runtime_envelope_override.as_ref()) {
        let memory = ctx
            .memory
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("agent_backend_unsupported"))?;
        job.validate(memory.as_ref(), envelope)?;
        let result =
            crate::runtime::agent_admission::wait(&ctx.config, &envelope.resolved_owner_id(), job.name.as_str()).await;
        // Retirement while waiting must refuse before any provider access.
        job.validate(memory.as_ref(), envelope)?;
        match result {
            Ok(permit) => permit,
            Err(error) => {
                super::scheduled_source::begin_decision()?;
                let fabric = MemoryFabric::new(Arc::clone(memory), envelope.workspace_id.clone())
                    .with_event_recording(ctx.config.memory.event_recording_config());
                let payload = serde_json::json!({"job_id":envelope.task_id,"job_version":job.version,"reason":error.to_string(),"retryable":false});
                fabric
                    .record_runtime_event_idempotent(
                        envelope.message_scope(),
                        "scheduled.skipped",
                        "scheduled run skipped",
                        Some(serde_json::to_string(&payload)?),
                        format!("{}:skipped", envelope.run_id.as_deref().unwrap_or_default()),
                    )
                    .await?;
                return Err(error.into());
            }
        }
    } else {
        None
    };

    // Scheduled cleanup owns the published writer even if this entry future is
    // dropped before a decision or terminal proposal is available.
    if scheduled_admission.is_some() {
        let cleanup = crate::runtime::turn_cleanup::current_tracker()
            .ok_or_else(|| anyhow::anyhow!("scheduled cleanup owner is missing"))?;
        if let Some(service) = &ctx.turn_service {
            cleanup.retain_service(Arc::clone(service));
        }
    }

    // Capture invocation state without constructing the large async work yet.
    let work = CliWork {
        ctx: &ctx,
        choice: &choice,
        message,
        temperature,
        shutdown: &shutdown,
        runtime_envelope_override,
        scheduled_tool_scope,
        scoped_run_id,
        scheduled_admission: &scheduled_admission,
    };
    // The synchronous factory invokes this closure in its own frame before
    // returning the boxed future. The actual engine work remains owned here.
    let work = boxed_cli_work(move || async move {
        let CliWork {
            ctx,
            choice,
            message,
            temperature,
            shutdown,
            runtime_envelope_override,
            scheduled_tool_scope,
            scoped_run_id,
            scheduled_admission,
        } = work;
        // `build` took ownership of `config`; reclaim a shared `Arc<Config>` for the
        // rest of this function. `Arc<Config>` deref-coerces to `&Config`, so all the
        // `config.xxx` accesses below are unchanged.
        let config = Arc::clone(&ctx.config);

        let observer = Arc::clone(&ctx.observer);
        let hooks = Arc::clone(&ctx.hooks);
        // security is consumed only by tool construction, which now happens inside the
        // bootstrap; it is not re-bound here on purpose (no downstream use in `run`).
        // The local runtime adapter is likewise built and consumed inside the
        // bootstrap, so it is no longer constructed here.

        // ── Memory (the brain) ────────────────────────────────────────
        // The Interactive profile always builds memory, so it is Some here; take it
        // explicitly without panicking (iron rules 1/6).
        let mem: Arc<dyn Memory> = ctx
            .memory
            .clone()
            .ok_or_else(|| anyhow::anyhow!("bootstrap did not build a memory backend for the agent CLI"))?;
        let memory_fabric = MemoryFabric::new(mem.clone(), config.workspace_dir.to_string_lossy())
            .with_event_recording(config.memory.event_recording_config());
        // One cost outbox pass before the turn (one-shot entry; D§5.2).
        crate::runtime::cost_recovery::warn_legacy_files_once(&config.workspace_dir);
        crate::runtime::cost_recovery::CostOutboxProjector::run_once(
            Arc::clone(&mem),
            config.workspace_dir.clone(),
            config.cost.clone(),
        )
        .await;
        // D8-3: run_id is per-turn (generated inside each turn branch as `turn_run_id`),
        // no longer a single per-run id held here.
        let mut fabric_turn_seq: u64 = 0;

        // ── Tools ────────────────────────────────────────────────────
        // The Interactive profile built the tool registry inside the bootstrap
        // (security + runtime + memory all ready) using the identical
        // `all_tools_with_runtime` call with the same arguments; take it explicitly.
        let tools_registry: Arc<Vec<Box<dyn Tool>>> = ctx
            .tools
            .clone()
            .ok_or_else(|| anyhow::anyhow!("bootstrap did not build the tool registry for the agent CLI"))?;

        // ── Provider (resolved before the bootstrap) ─────────────────
        let selection = agent_provider_selection(choice);
        let provider_name = selection.provider_name;
        let model_name = selection.model_name;

        let provider_runtime_options = providers::provider_runtime_options_from_config(&config);

        let provider: Box<dyn Provider> = selection.build_provider(&config, &provider_runtime_options)?;
        let effective_compaction = crate::providers::context_window::resolve_effective_compaction_config_with_catalog(
            &config.agent.compaction,
            provider_name,
            model_name,
            crate::providers::context_window::ContextWindowSources::from_config(&config),
            &config.model_routes,
            choice.catalog(),
        );
        crate::providers::context_window::trace_effective_compaction_resolution(&effective_compaction);

        observer.record_event(&ObserverEvent::AgentStart {
            provider: provider_name.to_string(),
            model: model_name.to_string(),
        });
        hooks
            .emit(
                HookEvent::AgentStart,
                serde_json::json!({
                    "provider": provider_name,
                    "model": model_name,
                }),
            )
            .await;

        // ── Build system prompt from the canonical prompt context ──
        let native_tools = provider
            .capabilities_for(model_name, crate::providers::traits::ProviderRequestMode::NonStreaming)
            .native_tool_calling;

        // ── Approval manager (supervised mode) ───────────────────────
        let approval_manager = Arc::new(ApprovalManager::new());

        // ── Execute ──────────────────────────────────────────────────
        let start = Instant::now();

        let mut final_output = String::new();

        {
            let msg = message;
            // ── Single-shot turn (D5/D9 step 2) ──────────────────────────
            // Race the work against the external shutdown token so a root
            // cancel aborts the in-flight turn instead of waiting it out.
            fabric_turn_seq += 1;
            // Keep the turn identity outside the raced future so a shutdown that
            // drops it can still close the shared terminal boundary as Cancelled.
            let turn_run_id = scoped_run_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let mut runtime_envelope = scheduled_admission.as_ref().map_or_else(
                || {
                    resolve_agent_turn_envelope(
                        runtime_envelope_override.as_ref(),
                        memory_fabric.workspace_id(),
                        &turn_run_id,
                    )
                },
                |admission| admission.envelope().clone(),
            );
            if runtime_envelope.config_source_revision.is_none() {
                runtime_envelope = runtime_envelope.with_config_generation(&ctx.config_generation);
            }
            let mut fabric_scope = runtime_envelope
                .message_scope()
                .with_recipient(format!("{provider_name}/{model_name}"));
            let cancel_turn_id = turn_run_id.clone();
            let cancel_scope = parking_lot::Mutex::new(runtime_envelope.message_scope());
            let turn_started_at = chrono::Utc::now();
            let terminal_binding = Arc::new(parking_lot::Mutex::new(None::<crate::agent::terminal::TerminalBinding>));
            let admission_proof = std::sync::OnceLock::<AgentTurnProof>::new();
            let terminal_guard = std::sync::OnceLock::<crate::runtime::turn_terminal::TurnTerminalGuard>::new();
            let mut cli_admission_guard = None;
            let mut scheduled_input_guard = None;
            let retained_provider_trace = Arc::new(crate::agent::turn_contracts::RetainedProviderTrace::default());
            let single_shot = boxed_cli_single_shot(async {
                let agent_user_event = if scheduled_admission.is_none() {
                    let owner = crate::runtime::turn_cleanup::current_tracker()
                        .ok_or_else(|| anyhow::anyhow!("CLI input cleanup owner missing"))?;
                    let (event, guard) = CliPendingInput {
                        fabric: memory_fabric.clone(),
                        generation: Arc::clone(&ctx.config_generation),
                        envelope: runtime_envelope.clone(),
                        expected_session_key: format!("agent:{turn_run_id}"),
                        scope: fabric_scope.clone(),
                        choice: choice.clone(),
                        input: msg.clone(),
                        key: format!("agent:{turn_run_id}:{fabric_turn_seq}:user"),
                        ticket: owner.register(shutdown.clone())?,
                    }
                    .record()
                    .await?;
                    cli_admission_guard = Some(guard);
                    event
                } else {
                    let admission = scheduled_admission
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("scheduled admission source missing"))?;
                    let owner = crate::runtime::turn_cleanup::current_tracker()
                        .ok_or_else(|| anyhow::anyhow!("scheduled input cleanup owner missing"))?;
                    let (event, guard) = admission
                        .record_guarded(
                            memory_fabric.clone(),
                            Arc::clone(&ctx.config_generation),
                            (
                                msg.clone(),
                                format!("agent:{turn_run_id}:{fabric_turn_seq}:user"),
                                choice.clone(),
                            ),
                            owner.register(shutdown.clone())?,
                        )
                        .await?;
                    scheduled_input_guard = Some(guard);
                    event
                };

                let decision_turn = {
                    let service = ctx
                        .turn_service
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("agent decision service is missing"))?;
                    let proof = if let Some(admission) = &scheduled_admission {
                        let source = admission
                            .bind_input(
                                mem.as_ref(),
                                &ctx.config_generation,
                                &runtime_envelope,
                                (&msg, choice),
                                agent_user_event.as_ref(),
                                shutdown,
                            )
                            .await?;
                        AgentTurnProof {
                            workspace_id: Arc::from(source.envelope.workspace_id.as_str()),
                            owner_id: Arc::from(source.envelope.resolved_owner_id()),
                            session_key: Arc::from(source.envelope.session_key.as_str()),
                            run_id: Arc::from(turn_run_id.as_str()),
                            source_event_id: source.envelope.source_message_event_id.as_deref().map(Arc::from),
                            routing_input: Arc::from(msg.as_str()),
                            memory_scope: source
                                .envelope
                                .memory_write_context(source.execution.chat_type.as_str()),
                            provider_name: Arc::from(choice.provider_name()),
                            model_name: Arc::from(choice.model_name()),
                            model_binding: choice.binding().clone(),
                            config_generation_id: ctx.config_generation.id.0,
                            config_source_revision: Arc::from(
                                ctx.config_generation.source_revision.fingerprint_sha256.as_str(),
                            ),
                            retrieval_principal: source.envelope.memory_principal(),
                            scheduled: Some(Box::new(source)),
                        }
                    } else {
                        issue_cli_agent_turn_proof(
                            None,
                            &runtime_envelope,
                            &ctx.config_generation,
                            &turn_run_id,
                            &msg,
                            agent_user_event.as_ref(),
                            choice,
                        )?
                    };
                    if let Some(source) = proof.scheduled.as_ref() {
                        runtime_envelope = source.envelope.clone();
                    }
                    // Admission owns the durable input identity. Carry it into
                    // ordinary completion and cancellation, not just tool calls.
                    runtime_envelope.source_message_event_id = proof.source_event_id.as_deref().map(str::to_owned);
                    fabric_scope = runtime_envelope
                        .message_scope()
                        .with_recipient(format!("{provider_name}/{model_name}"));
                    *cancel_scope.lock() = runtime_envelope.message_scope();
                    let proof = admission_proof.get_or_init(|| proof);
                    if scheduled_admission.is_some() {
                        // Mark before polling decision work: a failed or interrupted
                        // admission can already have a durable decision identity.
                        super::scheduled_source::begin_decision()?;
                    }
                    let decision = service
                        .open_issued_turn_context(
                            proof,
                            crate::runtime::turn_context::AgentTurnKind::SingleShot,
                            shutdown,
                        )
                        .await?;
                    let advice = service.advisory().analyze_turn(decision.runtime(), shutdown).await;
                    *terminal_binding.lock() = Some(service.terminal_binding(&decision)?);
                    let cleanup_ticket = if scheduled_admission.is_some() {
                        scheduled_input_guard
                            .take()
                            .ok_or_else(|| anyhow::anyhow!("scheduled input cleanup guard missing"))?
                            .into_terminal_ticket()?
                    } else {
                        cli_admission_guard
                            .take()
                            .ok_or_else(|| anyhow::anyhow!("CLI input cleanup guard missing"))?
                            .into_terminal_ticket()?
                    };
                    let route_decision = crate::llm::route_decision::RouteDecision::single_candidate_for_context(
                        provider_name,
                        model_name.to_string(),
                        runtime_envelope.resolved_owner_id(),
                        runtime_envelope.session_key.clone(),
                        runtime_envelope.source_message_event_id.clone(),
                        None,
                        "agent_cli_single",
                        u32::try_from(msg.chars().count() / 4).unwrap_or(u32::MAX),
                        !tools_registry.is_empty(),
                        false,
                    );
                    let guard = crate::runtime::turn_terminal::TurnTerminalGuard::new(
                        cleanup_ticket,
                        Arc::clone(service),
                        Arc::clone(&decision),
                        memory_fabric.clone(),
                        Arc::clone(&config),
                        runtime_envelope.clone(),
                        None,
                    )?
                    .with_provider_trace(Arc::clone(&retained_provider_trace), route_decision.clone());
                    terminal_guard
                        .set(guard)
                        .map_err(|_| anyhow::anyhow!("agent terminal owner already exists"))?;

                    (service, proof, decision, route_decision, advice)
                };

                let (decision_service, decision_proof, decision_context, route_decision, advice) = &decision_turn;
                let decision_proof = *decision_proof;
                let selected_skills = decision_service.agent_prompt_skills(decision_proof, decision_context)?;
                let system_prompt = match &named_job {
                    Some(job) => job.prompt(&config, model_name, &selected_skills, native_tools)?,
                    None => build_runtime_system_prompt(&config, model_name, &selected_skills, native_tools),
                };

                // Auto-save user message to memory (skip short/trivial messages)
                if config.memory.should_auto_promote_user_message(&msg) {
                    let _ = save_current_input(
                        &memory_fabric,
                        &turn_run_id,
                        &runtime_envelope.session_key,
                        &msg,
                        agent_user_event.as_ref().map(|event| event.event_id.as_str()),
                        runtime_envelope.agent_partition.clone(),
                    )
                    .await;
                }

                // Inject memory context into user message. The routing input is
                // pinned from `msg` here, ahead of the enrichment below.
                let memory_runtime = cli_turn_memory_runtime(
                    &mem,
                    &config.workspace_dir,
                    &runtime_envelope,
                    &memory_fabric,
                    &fabric_scope,
                    agent_user_event.as_ref().map(|event| event.event_id.as_str()),
                );
                let mem_context = crate::agent::context_recall::scoped_recall(
                    mem.as_ref(),
                    &crate::agent::context_recall::RecallScope::from_envelope(
                        &runtime_envelope,
                        &config.memory,
                        "private",
                    ),
                    &msg,
                    crate::agent::context_recall::RECALL_CONTEXT_BUDGET,
                )
                .await;
                // A scheduled run opens from its session's earlier runs; a plain run
                // keeps the single-message request it always had.
                let scheduled_turn = scheduled_admission.is_some();
                let (mut history, history_event_ids) = if scheduled_turn {
                    let previous_turns = scheduled_previous_turns(
                        &mem,
                        &runtime_envelope,
                        agent_user_event.as_ref().map(|event| event.event_id.as_str()),
                    )
                    .await;
                    let opening = crate::context::api::open::open_turn(crate::context::api::open::OpenInput {
                        memory: &mem,
                        envelope: &runtime_envelope,
                        memory_config: &config.memory,
                        chat_type: "private",
                        visible_message: &msg,
                        card: scheduled_card.as_deref(),
                        previous_turns,
                        replay: crate::context::api::open::ReplayMode::ClearAll,
                        system_prompt,
                        service: decision_service,
                        context: decision_context,
                        provider_label: provider_name,
                        model: model_name,
                    })
                    .await;
                    (opening.history, opening.history_event_ids)
                } else {
                    let context = mem_context.preamble.clone();
                    let card = scheduled_card.as_deref().unwrap_or_default();
                    let mut enriched = if context.is_empty() && card.is_empty() {
                        msg.clone()
                    } else {
                        format!("{context}{card}{msg}")
                    };
                    crate::agent::turn_contracts::push_current_time_tail(&mut enriched);
                    (
                        vec![
                            ChatMessage::system(&system_prompt),
                            crate::agent::turn_contracts::inbound_user_message(&enriched),
                        ],
                        Vec::new(),
                    )
                };
                let memory_runtime = if scheduled_turn {
                    memory_runtime
                        .with_history_event_ids(history_event_ids)
                        .with_rendered_stored_handoffs()
                } else {
                    memory_runtime
                };

                // D8-4: seed a turn-root spawn context so a sub-agent spawned from this
                // single-shot agent turn inherits parent_run_id = the per-turn run_id.
                let turn_spawn_ctx = crate::tools::sessions_spawn::SpawnExecutionContext::seed_turn_context(
                    turn_run_id.clone(),
                    runtime_envelope.session_key.clone(),
                );
                let provider_started_at = chrono::Utc::now();
                let runtime_adapter = Some({
                    let mut adapter =
                        configured_cli_tool_adapter(&ctx.security, &tools_registry, &approval_manager, &memory_runtime);
                    {
                        let proof = decision_proof;
                        let mut envelope = runtime_envelope.clone();
                        if let Some(event_id) = proof.source_event_id() {
                            envelope = envelope.with_source_message_event_id(event_id);
                        }
                        adapter.tool_execution_context = proof.scheduled.as_ref().map_or_else(
                            || cli_tool_execution_context(envelope),
                            |source| source.execution.clone(),
                        );
                    }
                    if let Some(case) = &ctx.controlled_case {
                        case.apply(&mut adapter.tool_execution_context)?;
                    }
                    adapter.provider_trace = Some(Arc::clone(&retained_provider_trace));
                    adapter
                });
                let turn_result = crate::tools::execution::with_scheduled_job_tool_scope(
                    scheduled_tool_scope,
                    crate::tools::sessions_spawn::SPAWN_EXECUTION_CONTEXT.scope(turn_spawn_ctx, async {
                        Box::pin(run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                advice: advice.clone(),
                                channel_name: "cli",
                                scope: None,
                                mode: ChatMode::default(),
                                silent: false,
                                expose_stay_silent: false,
                                multimodal: &config.multimodal,
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: config.agent.read_only_tool_concurrency_window,
                                    priority_scheduling_enabled: config.agent.priority_scheduling_enabled,
                                    low_priority_tool_names: config.agent.low_priority_tools.clone(),
                                },
                                compaction: Some(decision_context.main_compaction()),
                                decision: crate::agent::turn_request::TurnDecision {
                                    service: decision_service,
                                    context: decision_context,
                                    runtime: decision_context.runtime(),
                                    proof: decision_proof,
                                },
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: provider.as_ref(),
                                provider_name,
                                model: model_name,
                                temperature,
                                tools_registry: Arc::clone(&tools_registry),
                                observer: observer.as_ref(),
                                hooks: &hooks,
                                approval: Some(Arc::clone(&approval_manager)),
                                approval_resolver: None,
                                runtime_adapter,
                                memory: memory_runtime,
                            },
                            crate::agent::turn_request::TurnDelivery {
                                cancellation: None,
                                on_delta: None,
                                on_tool_call: None,
                            },
                        ))
                        .await
                        .map(|(outcome, trace)| (outcome.into_text(), trace))
                    }),
                )
                .await;
                let (response, turn_trace) = match turn_result {
                    Ok(result) => result,
                    Err(error) => {
                        let provider_outcome = if error.is::<crate::agent::send_admission::ProviderSendRefused>() {
                            // No provider attempt belongs to a refused send. The
                            // terminal guard retains any earlier observed rounds.
                            None
                        } else {
                            Some(
                                crate::llm::route_decision::ProviderExecutionOutcome::failed_for_decision(
                                    &route_decision,
                                    provider_started_at,
                                    &error,
                                ),
                            )
                        };
                        if let Err(finalize_error) = crate::runtime::turn_terminal::commit_terminal(
                            terminal_guard.get(),
                            &memory_fabric,
                            crate::agent::terminal::TurnTerminalDraft {
                                binding: terminal_binding_snapshot(&terminal_binding),
                                terminal_id: turn_run_id.clone(),
                                scope: runtime_envelope.message_scope(),
                                status: crate::agent::terminal::TurnTerminalStatus::Failed,
                                history: None,
                                history_scope: None,
                                provider_outcome,
                                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                                    summary: error.to_string(),
                                    started_at: provider_started_at,
                                    finished_at: chrono::Utc::now(),
                                },
                                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                            },
                            &config,
                        )
                        .await
                        {
                            tracing::warn!(error = %crate::agent::diagnostics::error_class(&finalize_error), "Failed to commit failed single-shot agent terminal event");
                        }
                        if !QUIET_SINGLE_SHOT.try_with(|quiet| *quiet).unwrap_or(false)
                            && let Some(refusal) =
                                error.downcast_ref::<crate::agent::send_admission::ProviderSendRefused>()
                        {
                            println!("{}", refusal.reply());
                        }
                        return Err(error);
                    }
                };
                {
                    let decision = decision_context;
                    anyhow::ensure!(
                        turn_trace.execution_trace.as_ref() == Some(&decision.terminal_execution_trace()),
                        "tool loop trace differs from the owning decision's execution facts"
                    );
                }
                let provider_outcome = crate::agent::terminal::provider_outcome_from_trace(
                    &route_decision,
                    provider_started_at,
                    turn_trace,
                );
                if let Err(error) = crate::runtime::turn_terminal::commit_terminal(
                    terminal_guard.get(),
                    &memory_fabric,
                    crate::agent::terminal::TurnTerminalDraft {
                        binding: terminal_binding_snapshot(&terminal_binding),
                        terminal_id: turn_run_id.clone(),
                        scope: runtime_envelope.message_scope(),
                        status: crate::agent::terminal::TurnTerminalStatus::Completed,
                        history: Some(crate::agent::terminal::TurnHistoryProjection {
                            assistant_content: response.clone(),
                            history_commit_len: history.len(),
                        }),
                        history_scope: Some(
                            fabric_scope
                                .clone()
                                .with_sender(format!("{provider_name}/{model_name}"))
                                .with_recipient("local-user"),
                        ),
                        provider_outcome: Some(provider_outcome),
                        telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                            summary: "single-shot agent turn completed".to_string(),
                            started_at: provider_started_at,
                            finished_at: chrono::Utc::now(),
                        },
                        delivery_intent: crate::agent::terminal::TurnDeliveryIntent::ReturnToCaller,
                    },
                    &config,
                )
                .await
                {
                    return Err(error);
                }
                increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;
                final_output = response.clone();
                if !QUIET_SINGLE_SHOT.try_with(|quiet| *quiet).unwrap_or(false) {
                    println!("{response}");
                }
                observer.record_event(&ObserverEvent::TurnComplete);
                Ok::<(), anyhow::Error>(())
            });
            let single_result = tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    if terminal_guard.get().is_none()
                        && terminal_binding_snapshot(&terminal_binding).is_some()
                    {
                    let cancelled_scope = cancel_scope.lock().clone();
                    if let Err(error) = crate::agent::terminal::finalize_turn(
                        &memory_fabric,
                        crate::agent::terminal::TurnTerminalDraft {
                            binding: terminal_binding_snapshot(&terminal_binding),
                            terminal_id: cancel_turn_id,
                            scope: cancelled_scope,
                            status: crate::agent::terminal::TurnTerminalStatus::Cancelled,
                            history: None,
                            history_scope: None,
                            provider_outcome: None,
                            telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                                summary: "single-shot agent cancelled by shutdown signal".to_string(),
                                started_at: turn_started_at,
                                finished_at: chrono::Utc::now(),
                            },
                            delivery_intent: crate::agent::terminal::TurnDeliveryIntent::None,
                        },
                        &config.cost,
                        &config.workspace_dir,
                    ).await {
                        tracing::warn!(error = %crate::agent::diagnostics::error_class(&error), "Failed to commit cancelled single-shot agent terminal event");
                    }
                    }
                    Err(anyhow::anyhow!("agent single-shot run cancelled by shutdown signal"))
                }
                result = single_shot => result,
            };
            if let (Err(error), Some(guard)) = (&single_result, terminal_guard.get()) {
                guard.finish_failure(error, shutdown.is_cancelled()).await?;
            }
            if let Err(error) = &single_result
                && let Some(mut guard) = cli_admission_guard.take()
            {
                guard
                    .finish_failure(shutdown.is_cancelled(), &error.to_string())
                    .await
                    .map_err(|record_error| {
                        anyhow::anyhow!(
                            "CLI admission failed: {error}; failure persistence is unconfirmed: {record_error}"
                        )
                    })?;
            }
            if single_result.is_err()
                && let Some(mut guard) = scheduled_input_guard.take()
            {
                guard
                    .finish_failure(shutdown.is_cancelled())
                    .await
                    .map_err(|_| anyhow::anyhow!("scheduled admission failure persistence is unconfirmed"))?;
            }
            single_result?;
        }

        let duration = start.elapsed();
        observer.record_event(&ObserverEvent::AgentEnd {
            provider: provider_name.to_string(),
            model: model_name.to_string(),
            duration,
            tokens_used: None,
            cost_usd: None,
        });
        hooks
            .emit(
                HookEvent::AgentEnd,
                serde_json::json!({
                    "duration_ms": duration.as_millis(),
                    "tokens_used": serde_json::Value::Null,
                }),
            )
            .await;

        Ok::<String, anyhow::Error>(final_output)
    });
    let result = match caller_cleanup {
        Some(cleanup) => cleanup.run(work).await,
        None => work.await,
    };
    if scheduled_admission.is_none() {
        if let Some(service) = &ctx.turn_service {
            service
                .shutdown(Instant::now() + std::time::Duration::from_secs(2))
                .await;
        }
    }
    // The process may exit right after this run: deliver the queued hook events first.
    ctx.hooks.drain_for_shutdown().await;
    result
}

/// How many stored turns of a scheduled run's session are replayed at most.
const SCHEDULED_REPLAY_TURN_LIMIT: usize = 500;

/// The earlier runs of a scheduled run's session, oldest first. The run's own
/// input is already stored, so that row is left out. A fresh run has none, and
/// a failed read starts the run without them.
async fn scheduled_previous_turns(
    memory: &std::sync::Arc<dyn crate::memory::Memory>,
    envelope: &RuntimeEnvelope,
    input_event_id: Option<&str>,
) -> Vec<crate::memory::ConversationTurn> {
    if super::scheduled_source::current_fresh() {
        return Vec::new();
    }
    let transcript = crate::context::transcript::Transcript::new(memory.as_ref(), envelope.memory_principal());
    match transcript
        .load_model_turns(&envelope.session_key, SCHEDULED_REPLAY_TURN_LIMIT)
        .await
    {
        Ok(mut turns) => {
            if let Some(input_event_id) = input_event_id {
                turns.retain(|turn| turn.message_event_id.as_deref() != Some(input_event_id));
            }
            turns
        }
        Err(error) => {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::warn!(
                error_len,
                error_sha256,
                "scheduled run could not read its session's earlier runs; starting without them"
            );
            Vec::new()
        }
    }
}

// This invocation state preserves all captures across the boxed work boundary.
struct CliWork<'a> {
    ctx: &'a crate::runtime::bootstrap::AppContext,
    choice: &'a AgentModelChoice,
    message: String,
    temperature: f64,
    shutdown: &'a CancellationToken,
    runtime_envelope_override: Option<RuntimeEnvelope>,
    scheduled_tool_scope: Option<crate::tools::execution::ScheduledToolScope>,
    scoped_run_id: Option<String>,
    scheduled_admission: &'a Option<super::scheduled_source::ScheduledAdmission>,
}

// Invoke the small capture closure here so the large future is constructed
// outside the canonical entry poll frame, then preserve the existing heap seam.
#[inline(never)]
fn boxed_cli_work<'a, F, Fut>(
    make_work: F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String>> + Send + 'a,
{
    Box::pin(make_work())
}

// Keep the single-turn poll state out of the enclosing CLI select frame.
// The synchronous boundary also keeps Box construction in its own stack frame.
#[inline(never)]
fn boxed_cli_single_shot<'a>(
    work: impl std::future::Future<Output = Result<()>> + Send + 'a,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(work)
}

fn resolve_agent_turn_envelope(
    override_envelope: Option<&RuntimeEnvelope>,
    workspace_id: &str,
    turn_run_id: &str,
) -> RuntimeEnvelope {
    let mut envelope = override_envelope
        .cloned()
        .unwrap_or_else(|| RuntimeEnvelope::agent(workspace_id.to_string(), turn_run_id.to_string()));
    if envelope.recipient.is_none() {
        envelope = envelope.with_recipient("cli:local-user");
    }
    envelope
}

#[path = "../turn_tests/entry.rs"]
mod tests;

#[cfg(test)]
mod cli_admission_failure_tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_admission_retries_without_model_terminal_or_secret() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.workspace_dir = dir.path().to_path_buf();
        config.config_path = dir.path().join("config.toml");
        let generation = crate::config::new_shared(config.clone()).pin();
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(dir.path()).unwrap());
        let proof =
            test_recorded_cli_agent_turn_proof_with_input(&generation, memory.clone(), "prx input", "prx_run").await;
        let fabric = MemoryFabric::new(memory.clone(), dir.path().to_string_lossy());
        let summary = format!("cancelled {}", crate::agent::terminal::persistence_probe::PROBE_SECRET);
        for _ in 0..2 {
            proof
                .record_cli_admission_failure(&fabric, &config, true, &summary)
                .await
                .unwrap();
        }
        assert!(
            proof
                .record_cli_admission_failure(&fabric, &config, false, &summary)
                .await
                .is_err()
        );
        let event = memory
            .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:admission-failed")
            .await
            .unwrap()
            .unwrap();
        assert!(event.id > 0);
        assert_eq!(event.causation_event_id.as_deref(), proof.source_event_id.as_deref());
        assert!(
            memory
                .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_run:final",)
                .await
                .unwrap()
                .is_none()
        );
        crate::agent::terminal::persistence_probe::assert_nothing_under_persists_the_probe_secret(dir.path());
    }
}

pub(crate) fn cli_tool_execution_context(envelope: RuntimeEnvelope) -> crate::tools::ToolExecutionContext {
    crate::tools::ToolExecutionContext::new(envelope, "private")
        .with_dispatch_origin(crate::tools::DispatchOrigin::Operator)
}
