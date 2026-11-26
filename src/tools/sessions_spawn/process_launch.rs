//! Process-mode launch of one sub-agent run: worker setup, registry row, and the monitor task that
//! owns the OS child until it ends.

use super::{
    SessionsSpawnTool, AnnounceOrigin, HistoryEntry, OutboundNotice, OwnerLossReason, ProcessFinalization, ProcessModeRunSeed,
    ProcessRunControl, ProcessWorkerOutcome, SPAWN_EXECUTION_CONTEXT, STEER_CHANNEL_CAPACITY, SpawnExecutionContext,
    SubAgentStatus, commit_process_terminal_state, deliver_or_withhold_notice, exited_without_result_reason,
    format_announce_message, new_process_mode_run, process_terminal, record_owner_loss_terminal,
    record_spawn_result_event_keyed, register_killable_process_run, resolve_worker_workspace_root,
    run_sub_agent_process, spawn_started_text, sub_agent_reported_failure, watch_process_mode_monitor,
};
use super::spawn::{SpawnLaunch, SpawnOutcome, policy_at_level};
use anyhow::Context as _;
use std::sync::Arc;
use tokio::sync::RwLock;

impl SessionsSpawnTool {
    /// Launch the run as an OS-process worker once `execute_spawn_with_id` has resolved
    /// the request, provider route and admitted source into a [`SpawnLaunch`].
    pub(super) async fn launch_process_run(&self, launch: SpawnLaunch<'_>) -> anyhow::Result<SpawnOutcome> {
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
            spawn_config,
            task,
            process_memory_strategy,
            spawn_scope,
            parent_exec_ctx,
            spawn_depth,
            parent_run_id,
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
            process_config_source,
            source_receipt,
            batch_id,
            process_compaction_config,
            ..
        } = launch;
        let process_parent_frames = caller.as_ref().and_then(|caller| {
            let context = caller.context();
            context
                .live_turn
                .as_ref()
                .map(|live| live.0.frames(&context.envelope, &std::collections::HashSet::new()))
        });
        let (process_config_dir, process_config_generation) =
            process_config_source.ok_or_else(|| anyhow::anyhow!("process config source was not resolved"))?;
        let process_provider_selection = if ordinary_publication.is_some() {
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
            .map(|launch| crate::runtime::operator_spawn::process::ProcessSourceLaunch::Ordinary(Box::new(launch)));
        let process_source_launch = match &operator_source {
            Some(source) => Some(crate::runtime::operator_spawn::process::ProcessSourceLaunch::Operator(
                Box::new(source.capture_process(
                    &spawn_scope_for_event,
                    task,
                    source_receipt.clone(),
                    process_config_generation.clone(),
                )?),
            )),
            None => process_source_launch,
        };
        let process_approval_relay = crate::session_worker::approval::relay_context::RelaySeedInputs {
            caller: caller.as_ref(),
            binding: ordinary_binding.as_deref(),
            resolvers: self.approval_resolver_factory.as_ref(),
            scope: &spawn_scope_for_event,
            task,
            receipt: source_receipt.as_deref(),
            security: launch_security,
            level: child_level,
            run_id: &run_id,
        }
        .seed()?;
        let temperature = resolved_temperature;
        let history_arc: Arc<RwLock<Vec<HistoryEntry>>> =
            Arc::new(RwLock::new(super::continuation::seed_history()));
        // Taken on the spawning task: task-locals do not cross `tokio::spawn`.
        let replay_seed = super::continuation::seed_turns();
        let process_control = ProcessRunControl::new();
        let stop_control = Arc::clone(&process_control);
        let runtime_handle = tokio::runtime::Handle::try_current()?;
        let message_turn = super::message_lifetime::retain(
            child_model_config,
            message_input.as_ref(),
            &self.active_runs,
            Arc::new(move || {
                let control = Arc::clone(&stop_control);
                runtime_handle.spawn(async move {
                    let _ = control.request_termination("history_erased").await;
                });
            }),
        )?;
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
            let mut run = new_process_mode_run(ProcessModeRunSeed {
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
                // Recursive delegation status is issued by no parent (P2-G4).
                delegation: None,
                progress: Arc::clone(&process_progress),
                batch_id: batch_id.map(str::to_string),
            });
            run.agent_partition = spawn_scope_for_event.agent_partition.clone();
            super::continuation::register(&mut runs, run)?;
        }

        let model = resolved_model;
        let provider_name = resolved_provider_name;
        let route_hint = selected_route_hint;
        let workspace_root = child_model_config.workspace_dir.clone();
        let worker_workspace_root =
            resolve_worker_workspace_root(spawn_config.worker_workspace_root.as_deref(), &workspace_root);
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
        let process_announce_security = policy_at_level(launch_security, child_level);
        let process_announce_origin = AnnounceOrigin::from_scope(spawn_scope.as_ref());
        let process_announce_history = history_arc.clone();
        // The worker's parent is the run its bound input names: the
        // publication's spawn run, or the operator source run. A gateway
        // Console turn seeds no spawn context, so `parent_run_id` alone is
        // None there and the manifest would differ from its issued input.
        let process_parent_run_id = spawn_scope_for_event
            .parent_run_id
            .clone()
            .or_else(|| parent_run_id.clone());
        let process_session_scope_key = session_scope_key.clone();
        let process_mailbox_announce = super::mailbox_announce::MailboxAnnouncement::capture(
            caller,
            child_model_config,
            recipient.as_deref(),
            &session_scope_key,
            process_agent_id.as_deref(),
        );
        let process_spawn_depth = spawn_depth;
        let process_channel_exclusions = inherited_channel_exclusions.clone();
        let process_event_recording = child_model_config.memory.event_recording_config();
        let process_execution_ctx = SpawnExecutionContext {
            task_input: parent_exec_ctx
                .as_ref()
                .map(|context| context.task_input.clone())
                .unwrap_or_default(),
            operator_authority: None,
            ordinary_authority: None,
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
        // Parented to the enclosing turn, not the launching tool call: that row
        // is gone once the call returns, which would cut this run out of a
        // cascading cancel of the root.
        let sub_agent_parent = crate::runtime::registry::current_owner_work_id();
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
        let process_worker_mailbox = if let Some(factory) = &self.worker_mailbox {
            Some(factory.clone())
        } else if let Some(config) = self
            .hint_config
            .as_ref()
            .filter(|config| config.memory.backend == "sqlite")
        {
            Some(crate::messaging::worker::WorkerMailboxes::for_config(config)?
                as Arc<dyn crate::session_worker::mailbox_binding::WorkerMailboxFactory>)
        } else {
            None
        };
        let process_batch_id = batch_id.map(str::to_owned);
        #[cfg(test)]
        let test_worker_executable = self.test_worker_executable.clone();
        let ordinary_terminal = Arc::new(parking_lot::Mutex::new(None));
        let monitor_ordinary_terminal = Arc::clone(&ordinary_terminal);
        let operator_terminal = Arc::new(parking_lot::Mutex::new(None));
        let monitor_operator_terminal = Arc::clone(&operator_terminal);
        let monitor_active_runs = active_runs.clone();
        let monitor_progress = Arc::clone(&process_progress);
        let jh = crate::agent::channel_approval::spawn(crate::runtime::registry::scope_current(
            process_work_id,
            SPAWN_EXECUTION_CONTEXT.scope(
                process_execution_ctx,
                crate::agent::idle::scope_beat(Some(Arc::clone(&monitor_progress)), async move {
                    let _message_permit=message_permit;
                    let _message_input=message_input;
                    let _message_turn=message_turn;
                    async {
                        tracing::info!(run_id = %rid, "Sub-agent process starting");

                        let worker_result = run_sub_agent_process(
                            &rid,
                            &task_owned,
                            &provider_name,
                            &model,
                            route_hint.as_deref(),
                            route_source_provider.as_deref(),
                            route_credential.as_ref(),
                            route_uses_root_connection,
                            direct_uses_root_connection,
                            temperature,
                            temperature_override,
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
                            replay_seed,
                            Arc::clone(&monitor_progress),
                            process_memory_fabric.as_ref(),
                            process_source_launch,
                            process_batch_id.as_deref(),
                            process_worker_mailbox.as_ref(),
                            process_parent_frames,
                            process_approval_relay,
                            #[cfg(test)]
                            test_worker_executable.as_ref(),
                        )
                        .await;

                        // The worker's own account that its work was cancelled; the run's
                        // status stays `Failed`, only the recorded task event differs.
                        let mut worker_cancelled = false;
                        let (status, result_text, token_usage_records, finalization) = match worker_result {
                            Ok(ProcessWorkerOutcome::OrdinaryVerifiedFinished(completion)) => {
                                *monitor_ordinary_terminal.lock() = Some(completion.clone());
                                let result = completion.result;
                                worker_cancelled = result.cancelled;
                                let (status, text) = if result.success {
                                    (SubAgentStatus::Completed(result.output.clone()), result.output)
                                } else {
                                    let error = super::failure::llm_visible_error(&result.error.unwrap_or_else(|| "worker failed".to_owned()));
                                    (SubAgentStatus::Failed(sub_agent_reported_failure(&error)), format!("Sub-agent error: {error}"))
                                };
                                (status, text, completion.usage, ProcessFinalization::Natural)
                            }
                            Ok(ProcessWorkerOutcome::OperatorVerifiedFinished(completion)) => {
                                *monitor_operator_terminal.lock() = Some(completion.clone());
                                let result = completion.result;
                                worker_cancelled = result.cancelled;
                                let (status, text) = if result.success {
                                    (SubAgentStatus::Completed(result.output.clone()), result.output)
                                } else {
                                    let error = super::failure::llm_visible_error(&result.error.unwrap_or_else(|| "worker failed".to_owned()));
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
                                worker_cancelled = result.cancelled;
                                let error = result.error.unwrap_or_else(|| "worker failed".to_string());
                                if let Some(reason) = crate::session_worker::refusal::reason_of_report(&error) {
                                    tracing::warn!(
                                        run_id = %rid,
                                        reason = %reason,
                                        "sub-agent process worker refused to start"
                                    );
                                }
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
                            Ok(ProcessWorkerOutcome::Terminated { reason, usage }) => {
                                let msg = format!("Sub-agent process terminated: {reason}");
                                (
                                    SubAgentStatus::Failed(reason),
                                    msg,
                                    usage,
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
                            Ok(ProcessWorkerOutcome::ExitedUnrequested { detail, usage }) => {
                                let error = exited_without_result_reason(&detail);
                                (SubAgentStatus::Failed(error.clone()), format!("Sub-agent process error: {error}"),
                                    usage, ProcessFinalization::Natural)
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
                                // Nothing was spawned for a refused launch, so its
                                // failure is final rather than left to recovery.
                                let launch_refused =
                                    error.downcast_ref::<super::process_mode::WorkerLaunchRefused>().is_some();
                                let reason: crate::session_worker::refusal::WorkerRefusalReason =
                                    crate::session_worker::refusal::classify(&error);
                                tracing::warn!(
                                    run_id = %rid,
                                    launch_refused,
                                    reason = %reason,
                                    error = %crate::agent::diagnostics::error_class(&error),
                                    "sub-agent process ended with an error"
                                );
                                let detail = super::failure::llm_visible_error(
                                    &crate::session_worker::refusal::describe(&error),
                                );
                                let msg = format!("Sub-agent process error: {detail}");
                                (
                                    SubAgentStatus::Failed(detail),
                                    msg,
                                    Vec::new(),
                                    if (is_ordinary_process || is_operator_process) && !launch_refused {
                                        ProcessFinalization::Unconfirmed
                                    } else { ProcessFinalization::Natural },
                                )
                            }
                        };

                        let announce = format_announce_message(&rid, &status, &result_text);
                        // An exiting owner waits only for finalization, so the run's
                        // durable terminal row is written before the commit publishes it.
                        let owner_exiting = monitor_process_control.termination_reason().as_deref()
                            == Some(OwnerLossReason::OwnerExited.code());
                        if owner_exiting {
                            if matches!(finalization, ProcessFinalization::Terminated | ProcessFinalization::Unconfirmed) {
                                if let Some(fabric) = process_memory_fabric.as_ref() {
                                    if let Err(error) = record_owner_loss_terminal(
                                        fabric,
                                        process_result_scope.clone(),
                                        &rid,
                                        OwnerLossReason::OwnerExited,
                                    )
                                    .await
                                    {
                                        tracing::error!(
                                            run_id = %rid,
                                            error = %crate::agent::diagnostics::error_class(&error),
                                            "owner-exit terminal row remains unconfirmed"
                                        );
                                    }
                                }
                            } else if finalization == ProcessFinalization::Natural {
                                // Finished on its own within the bound: its real outcome.
                                record_spawn_result_event_keyed(
                                    process_memory_fabric.as_ref(),
                                    process_result_scope.clone(),
                                    &result_text,
                                    &status,
                                    &process_lineage,
                                    worker_cancelled.then_some("task.cancelled"),
                                    Some(crate::session_worker::protocol::worker_result_key(&rid)),
                                )
                                .await;
                            }
                        }
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
                        if finalization == ProcessFinalization::Unconfirmed || owner_exiting {
                            return;
                        }
                        // One result row per run: the worker's own row, if it wrote one,
                        // already holds the run's key and this write adds nothing.
                        record_spawn_result_event_keyed(
                            process_memory_fabric.as_ref(),
                            process_result_scope,
                            &result_text,
                            &status,
                            &process_lineage,
                            // A `Terminated` run is a requested stop: recorded as cancelled.
                            (finalization == ProcessFinalization::Terminated || worker_cancelled)
                                .then_some("task.cancelled"),
                            Some(crate::session_worker::protocol::worker_result_key(&rid)),
                        )
                        .await;

                        let process_mailbox_delivered = match &process_mailbox_announce {
                            Some(mailbox) if announce_result => {
                                mailbox
                                    .deliver(&rid, matches!(status, SubAgentStatus::Completed(_)), &result_text)
                                    .await
                            }
                            _ => false,
                        };
                        if !announce_result {
                            tracing::debug!(
                                run_id = %rid,
                                "Sub-agent process result withheld from the channel (announce=false); \
                                 it is delivered to whoever joins this run's batch"
                            );
                        } else if process_mailbox_delivered {
                            // The parent session received it as an inbound message.
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
                    }
                    .await;
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
            ordinary_terminal,
            ordinary_recovery,
            operator_terminal,
            operator_recovery,
        );

        return Ok(SpawnOutcome::started(
            run_id.clone(),
            spawn_started_text(
                "Sub-agent spawned in process mode",
                &run_id,
                child_delegation,
                announce_result,
            ),
        ));
    }
}
