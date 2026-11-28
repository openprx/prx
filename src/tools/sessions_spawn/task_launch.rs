//! Task-mode launch of one sub-agent run: in-process driver construction, registry row, and the driver
//! task that runs the child loop to its terminal state.

use super::{
    SessionsSpawnTool, AllowedToolProxy, AnnounceOrigin, HistoryEntry, MemoryScope, OutboundNotice, SPAWN_EXECUTION_CONTEXT,
    STEER_CHANNEL_CAPACITY, SpawnExecutionContext, SubAgentRun, SubAgentStatus, TaskRunControl, TerminationCause,
    boxed_spawn_field, deliver_or_withhold_notice, format_announce_message, parse_memory_scope,
    record_spawn_result_event, resolve_tools_for_agent, run_sub_agent_task, spawn_started_text, task_failure_reason,
    task_terminal, termination_cause, watch_task_mode_monitor,
};
use super::spawn::{SpawnLaunch, SpawnOutcome, policy_at_level};
use crate::tools::traits::Tool;
use anyhow::Context as _;
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

impl SessionsSpawnTool {
    /// Launch the run as an in-process task once `execute_spawn_with_id` has resolved
    /// the request, provider route and admitted source into a [`SpawnLaunch`].
    pub(super) async fn launch_task_run(&self, launch: SpawnLaunch<'_>) -> anyhow::Result<SpawnOutcome> {
        let SpawnLaunch {
            operator_source,
            message_input,
            message_permit,
            caller,
            child_delegation,
            ordinary_publication,
            child_model_config,
            launch_security,
            child_level,
            task,
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
            resolved_provider_name,
            resolved_model,
            temperature_override,
            resolved_temperature,
            ordinary_binding,
            spawn_scope_for_event,
            run_lineage,
            tools,
            source_receipt,
            batch_id,
            parent_hooks,
            ..
        } = launch;
        // Create shared history and steer channel for this run
        let history_arc: Arc<RwLock<Vec<HistoryEntry>>> = Arc::new(RwLock::new(super::continuation::seed_history()));
        // The stored turns the first input replays through the shared opening.
        let replay_seed = super::continuation::seed_turns();
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
        let child_provider_binding = if let Some(source) = &operator_source {
            let binding = Arc::new(crate::runtime::child_source::ChildProviderBinding::build_independent(
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
        // Only approval suspension needs the optional restoration registry.
        // Every task still carries its admitted run identity, including mailbox
        // continuations without an approval surface.
        let restore_active_runs = run_approval_resolver.as_ref().map(|_| self.active_runs.clone());
        let restore_run_id = Some(run_id.clone());
        let task_owned = task.to_string();
        let identity_workspace = child_model_config.workspace_dir.clone();
        let security = Arc::clone(launch_security);
        // Second handle, for the announcement gate: `security` above is moved
        // into the sub-agent's own loop, and the announcement is authorized only
        // once that loop has returned.
        let announce_security = policy_at_level(launch_security, child_level);
        // The launching turn's trusted identity, captured atomically with
        // `recipient` / `run_channel_name` from the same scope snapshot.
        let announce_origin = AnnounceOrigin::from_scope(spawn_scope.as_ref());
        // Same handle the run writes its transcript through, so a refused
        // announcement is visible on the run itself and not only in the log.
        let announce_history = history_arc.clone();
        let mut terminal_usage = None;
        let mut task_execution_ctx = SpawnExecutionContext {
            task_input: parent_exec_ctx
                .as_ref()
                .map(|context| context.task_input.clone())
                .unwrap_or_default(),
            operator_authority: None,
            ordinary_authority: None,
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
        let task_mailbox_announce = super::mailbox_announce::MailboxAnnouncement::capture(
            caller,
            child_model_config,
            recipient.as_deref(),
            &session_scope_key,
            agent_label_for_registry.as_deref(),
        );
        let (system_prompt, filtered_tools) = if let Some((agent, cfg)) = selected_agent {
            let identity_path = cfg
                .identity_dir
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|directory| identity_workspace.join(directory));
            let prompt = crate::identity::delegated::compose(identity_path.as_deref(), cfg.system_prompt.as_deref());
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
                    if child_full || cfg.allowed_tools.is_empty() {
                        None
                    } else {
                        Some(&cfg.allowed_tools)
                    },
                    &inherited_channel_exclusions,
                )
            });
            (prompt, tools)
        } else {
            (crate::identity::delegated::DEFAULT_PROMPT.to_owned(), tools)
        };

        // Register the run.
        //
        // A background run outlives its spawning turn. Its independent observer
        // owns cleanup; parenting this to the caller would cancel valid work
        // when that caller returns. Register before publishing the active row.
        let task_control = TaskRunControl::new();
        let stop_token = task_control.cancellation().clone();
        let message_turn = super::message_lifetime::retain(
            child_model_config,
            message_input.as_ref(),
            &self.active_runs,
            Arc::new(move || stop_token.cancel()),
        )?;
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
                task_execution_ctx.ordinary_authority.as_deref(),
                task_execution_ctx.operator_authority.as_deref(),
            ) {
                (Some(authority), None) => LogicalTaskTerminalRef::ordinary(authority)?,
                (None, Some(authority)) => LogicalTaskTerminalRef::operator(authority)?,
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
            super::continuation::register(
                &mut runs,
                SubAgentRun {
                    agent_partition: spawn_scope_for_event.agent_partition.clone(),
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
                    // Recursive delegation status is issued by no parent (P2-G4).
                    delegation: None,
                    token_usage_records: Vec::new(),
                    progress: Arc::clone(&task_progress),
                    batch_id: batch_id.map(str::to_string),
                },
            )?;
        }

        // Registry row for this run. Parent captured on the spawning task, which
        // is the enclosing turn (not the launching tool call, which is gone once
        // it returns); a spawned task inherits no task-locals of its own.
        let sub_agent_parent = crate::runtime::registry::current_owner_work_id();
        let sub_agent_label = agent_label_for_registry.unwrap_or_else(|| "sub-agent".to_string());
        let mut sub_agent_work =
            crate::runtime::registry::register_sub_agent(&sub_agent_label, &rid, sub_agent_parent, batch_id, None);
        sub_agent_work.mark_spawned();
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
        // The launching turn's hook identity, read on the spawning task: the
        // spawned task has no task-locals, so the child's `child_started` and
        // `child_finished` events would otherwise be lost (`notify` needs the
        // identity). `run_sub_agent_task` reads it again on this task and
        // re-installs it across its own spawn.
        let launch_hook_identity = crate::hooks::current_identity();
        // Spawn async task (fire-and-forget); capture handle to support kill
        let jh = crate::agent::channel_approval::spawn(crate::runtime::registry::scope_current(
            sub_agent_work_id,
            crate::hooks::identity_scope_optional(launch_hook_identity, SPAWN_EXECUTION_CONTEXT.scope(task_execution_ctx, async move {
                let _message_permit = message_permit;
                let _message_input = message_input;
                let _message_turn = message_turn;
                tracing::info!(run_id = %rid, "Sub-agent task starting");
                let provider_started_at = Utc::now();
                let (run_on_delta, run_on_tool) = match run_event_streams {
                    Some((delta_tx, tool_tx)) => (Some(delta_tx), Some(tool_tx)),
                    None => (None, None),
                };
                // Construct the large child loop behind the existing allocation
                // boundary before adding task-local temperature ownership.
                let run_future = boxed_spawn_field(|| {
                    run_sub_agent_task(
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
                        parent_hooks,
                        replay_seed,
                    )
                });
                let result = run_cleanup_scope
                    .run(crate::providers::model_settings::with_temperature(
                        temperature_override,
                        run_future,
                    ))
                    .await;
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
                        binding: None,
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
                let task_completed = matches!(status, SubAgentStatus::Completed(_));
                // Read before the result row is written: the scope moves into it.
                let task_documents = match &task_mailbox_announce {
                    Some(_) if announce_result => {
                        super::mailbox_announce::run_document_refs(&task_memory_fabric, &task_result_scope, &rid).await
                    }
                    _ => Vec::new(),
                };
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

                // A gateway parent session receives the completion through its mailbox.
                let task_mailbox_delivered = match &task_mailbox_announce {
                    Some(mailbox) if announce_result => {
                        mailbox.deliver(&rid, task_completed, &result_text, &task_documents).await
                    }
                    _ => false,
                };
                // Announce result back to channel if we have a recipient
                if !announce_result {
                    tracing::debug!(
                        run_id = %rid,
                        "Sub-agent result withheld from the channel (announce=false); \
                         it is delivered to whoever joins this run's batch"
                    );
                } else if task_mailbox_delivered {
                    // The parent session received it as an inbound message.
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
            })),
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
            spawn_started_text("Sub-agent spawned", &run_id, child_delegation, announce_result),
        ))
    }
}
