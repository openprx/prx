use super::{HistoryEntry, SPAWN_EXECUTION_CONTEXT, SubAgentRun, SubAgentStatus, steering_instruction};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_contracts::ScopeContext;
use crate::hooks::HookManager;
use crate::observability::NoopObserver;
use crate::providers::{ChatMessage, Provider};
use crate::security::SecurityPolicy;
use crate::tools::traits::Tool;
use anyhow::Context as _;
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Convert a slice of `ChatMessage` to `HistoryEntry` values.
/// Each entry is timestamped with the current wall-clock time (approximate).
fn chat_messages_to_history(messages: &[ChatMessage]) -> Vec<HistoryEntry> {
    let now = Utc::now();
    messages
        .iter()
        .map(|m| HistoryEntry {
            role: m.role.clone(),
            content: m.content.clone(),
            timestamp: now,
        })
        .collect()
}

/// Run an isolated sub-agent loop with steering and history support.
///
/// Supports:
/// - Agentic tool-call loop (when a tool registry is available)
/// - Steering: injected messages are added to the conversation and the loop restarts
/// - History: `history_out` is updated after each significant state change
///
/// Uses the admitted shared turn loop even when no tools are registered.
///
/// Deterministically restore a single run from a suspended approval back to
/// [`SubAgentStatus::Running`].
///
/// Downgrades **only** `AwaitingInput` -> `Running`; any terminal state
/// (`Completed` / `Failed` / `Cancelled`) — e.g. one set by a concurrent kill or
/// timeout — is left untouched, so a killed run that already moved to `Failed` is
/// never resurrected to `Running`. A no-op if the run id is absent. Idempotent.
pub(super) fn restore_run_to_running(runs: &mut [SubAgentRun], run_id: &str) {
    if let Some(run) = runs.iter_mut().find(|r| r.id == run_id)
        && matches!(run.status, SubAgentStatus::AwaitingInput { .. })
    {
        run.status = SubAgentStatus::Running;
        tracing::debug!(run_id = %run_id, "restored sub-agent to Running after approval suspension ended");
    }
}

/// Dropping an outer steering future must not detach its active inner loop.
struct AbortLoopOnDrop(pub(super) tokio::task::AbortHandle);

impl Drop for AbortLoopOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) struct SubAgentTaskResult {
    pub(super) segment_usage: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    pub(super) output: String,
    pub(super) history_commit_len: usize,
}

fn settle_sub_agent_task_result(
    mut settled_segment_usage: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    result: anyhow::Result<SubAgentTaskResult>,
) -> anyhow::Result<SubAgentTaskResult> {
    result.map(|mut result| {
        settled_segment_usage.append(&mut result.segment_usage);
        result.segment_usage = settled_segment_usage;
        if result.output.trim().is_empty() {
            result.output = "[Sub-agent produced no output]".to_string();
        }
        result
    })
}

pub(super) async fn run_sub_agent_task(
    task: &str,
    provider: Arc<dyn Provider>,
    provider_name: &str,
    model: &str,
    temperature: f64,
    tools: Option<Arc<Vec<Box<dyn Tool>>>>,
    system_prompt: &str,
    security: Arc<SecurityPolicy>,
    mut steer_rx: tokio::sync::mpsc::Receiver<String>,
    history_out: Arc<RwLock<Vec<HistoryEntry>>>,
    on_delta: Option<tokio::sync::mpsc::Sender<String>>,
    on_tool_call: Option<tokio::sync::mpsc::Sender<crate::agent::turn_contracts::ToolCallNotification>>,
    approval_resolver: Option<Arc<dyn crate::agent::turn_contracts::ApprovalResolver>>,
    // NeedsInput: the shared run registry + this run's id, used to
    // deterministically restore `AwaitingInput` -> `Running` whenever the loop
    // leaves a suspended approval to continue running (steer / cancel-and-resume).
    // The resolver's own `Drop` only does a best-effort `try_write` restore that
    // is skipped under lock contention, so this async path is the authoritative
    // guarantee that no run is left as a zombie `AwaitingInput` while it is in
    // fact running again. `None` when there is no approval resolver attached
    // (channels / gateway), where suspension can never happen.
    active_runs: Option<Arc<RwLock<Vec<SubAgentRun>>>>,
    run_id: Option<String>,
    // Liveness beat for this run, minted by the spawner and already parented on
    // the spawner's own beat. Installed as the ambient beat of every loop
    // iteration below, so the run's registry row observes exactly the events
    // `crate::agent::idle` already defines as progress.
    progress: Arc<crate::agent::idle::ProgressBeat>,
) -> anyhow::Result<SubAgentTaskResult> {
    let operator_authority = SPAWN_EXECUTION_CONTEXT
        .try_with(|context| context.operator_authority.clone())
        .ok()
        .flatten();
    if let Some(authority) = &operator_authority {
        authority.verify_execution(run_id.as_deref(), task, &provider, provider_name, model)?;
    }
    let mut operator_segments = operator_authority
        .as_ref()
        .map(|authority| authority.segments())
        .transpose()?;
    let ordinary_authority = SPAWN_EXECUTION_CONTEXT
        .try_with(|context| context.ordinary_authority.clone())
        .ok()
        .flatten();
    let mut ordinary_segments = ordinary_authority
        .as_ref()
        .map(|authority| -> anyhow::Result<_> {
            anyhow::ensure!(
                Arc::ptr_eq(&authority.binding().provider(), &provider)
                    && authority.binding().provider_name() == provider_name
                    && authority.binding().model() == model,
                "ordinary task provider differs from issued source"
            );
            authority.segments()
        })
        .transpose()?;
    let mut child_segments = SPAWN_EXECUTION_CONTEXT
        .try_with(|context| -> anyhow::Result<_> {
            let Some(grant) = context.incoming_grant.as_ref() else {
                return Ok(None);
            };
            let binding = context
                .provider_binding
                .as_ref()
                .context("child provider binding missing")?;
            let receipt = context.input_receipt.as_ref().context("child input receipt missing")?;
            anyhow::ensure!(
                binding.provider_name() == provider_name && binding.model() == model && receipt.input() == task,
                "child execution inputs differ from their admitted source"
            );
            binding.source_proof(grant, receipt, &provider)?;
            Ok(Some((
                Arc::clone(grant),
                Arc::clone(binding),
                crate::runtime::child_source::ChildSegmentSequence::new(Arc::clone(grant), Arc::clone(receipt))?,
            )))
        })
        .unwrap_or(Ok(None))?;
    anyhow::ensure!(
        usize::from(child_segments.is_some())
            + usize::from(ordinary_segments.is_some())
            + usize::from(operator_segments.is_some())
            == 1,
        "task execution requires exactly one admitted source authority"
    );
    let child_usage = SPAWN_EXECUTION_CONTEXT
        .try_with(|context| context.terminal_usage.clone())
        .ok()
        .flatten();
    anyhow::ensure!(
        (child_segments.is_none() && ordinary_segments.is_none() && operator_segments.is_none())
            || child_usage.is_some(),
        "child segment usage owner missing"
    );
    let child_fabric = child_segments
        .as_ref()
        .map(|(grant, _, _)| grant.source_fabric().clone())
        .or_else(|| {
            ordinary_authority
                .as_ref()
                .map(|authority| authority.publication().source_fabric())
        })
        .or_else(|| operator_authority.as_ref().map(|authority| authority.fabric().clone()));
    let mut child_source = if let Some((grant, binding, segments)) = child_segments.as_mut() {
        let receipt = segments
            .record(child_fabric.as_ref().context("child segment fabric missing")?, task)
            .await?;
        let proof = Arc::new(binding.source_proof(grant, &receipt, &provider)?);
        let execution_context = receipt.execution_context(&proof)?;
        Some((proof, execution_context))
    } else if let Some(segments) = ordinary_segments.as_mut() {
        Some(segments.record(task).await?)
    } else if let Some(segments) = operator_segments.as_mut() {
        Some(segments.record(task).await?)
    } else {
        None
    };
    // Empty tool registries still enter the same source-bound decision and
    // terminal lifecycle. No direct provider path can bypass admission.
    let tools_registry = tools.unwrap_or_else(|| Arc::new(Vec::new()));

    let child_config = child_segments
        .as_ref()
        .map(|(grant, _, _)| Arc::clone(&grant.generation().effective))
        .or_else(|| {
            ordinary_authority
                .as_ref()
                .map(|authority| Arc::clone(&authority.publication().generation().effective))
        })
        .or_else(|| {
            operator_authority
                .as_ref()
                .map(|authority| Arc::clone(&authority.generation().effective))
        });
    let mut settled_segment_usage = Vec::new();
    let child_service = if let Some((grant, _, _)) = child_segments.as_ref() {
        Some(
            crate::runtime::turn_publication::ChildDecisionPublication {
                allowed_tools: None,
                grant: Arc::clone(grant),
                registry: Arc::clone(&tools_registry),
                observer: Arc::new(NoopObserver),
            }
            .publish()
            .await?,
        )
    } else if let Some(authority) = &ordinary_authority {
        let owner = crate::runtime::turn_cleanup::current_tracker().context("ordinary task cleanup owner missing")?;
        let excluded = SPAWN_EXECUTION_CONTEXT
            .try_with(|context| context.channel_excluded_tools.clone())
            .context("ordinary task execution scope missing")?;
        let service = authority.publication().task_service(
            Arc::clone(authority),
            Arc::clone(&tools_registry),
            authority.allowed_tools(),
            &excluded,
        )?;
        owner.retain_child_service(Arc::clone(&service))?;
        Some(service)
    } else if let Some(authority) = &operator_authority {
        let owner = crate::runtime::turn_cleanup::current_tracker().context("operator task cleanup owner missing")?;
        let service = authority
            .publication()
            .task_service(Arc::clone(authority), Arc::clone(&tools_registry))?;
        owner.retain_child_service(Arc::clone(&service))?;
        Some(service)
    } else {
        None
    };
    let ordinary_runtime = ordinary_authority
        .as_ref()
        .map(|authority| authority.publication().loop_runtime().cloned())
        .or_else(|| {
            operator_authority
                .as_ref()
                .map(|authority| authority.publication().loop_runtime().cloned())
        })
        .transpose()?;

    // --- Agentic loop with steering support ---
    let mut history: Vec<ChatMessage> = Vec::new();

    // NeedsInput: deterministically restore a suspended run to `Running` before
    // re-entering the loop. Called on every path that *continues* running after a
    // possible approval suspension (steer-driven cancel-and-resume). Downgrades
    // `AwaitingInput` -> `Running` only; it never clobbers a terminal state
    // (Completed / Failed / Cancelled) set concurrently by a kill / timeout, so
    // a killed run that already moved to `Failed` is left untouched. This runs in
    // a proper async context (`.write().await`), so unlike the resolver's `Drop`
    // best-effort `try_write` it can never be skipped under lock contention.
    let restore_running = || async {
        let (Some(runs_arc), Some(rid)) = (active_runs.as_ref(), run_id.as_ref()) else {
            return;
        };
        let mut runs = runs_arc.write().await;
        restore_run_to_running(&mut runs, rid);
    };

    loop {
        if let Some(authority) = &operator_authority {
            authority.verify_execution(run_id.as_deref(), task, &provider, provider_name, model)?;
        }
        let cancel_token = CancellationToken::new();

        // Clone everything needed for the inner spawned task.
        // We move `history` into the task and get it back after completion.
        let initial_system = (child_source.is_some() || history.first().is_none_or(|message| message.role != "system"))
            .then(|| system_prompt.to_owned());
        let initial_task = history.is_empty().then(|| task.to_owned());
        let mut task_history = history;
        let provider_instance = provider.clone();
        let child_source_owned = child_source.clone().context("task segment source missing")?;
        let ordinary_runtime_owned = ordinary_runtime.clone();
        let child_service_owned = child_service.clone().context("task segment service missing")?;
        let child_config_owned = child_config.clone().context("task segment configuration missing")?;
        let child_fabric_owned = child_fabric.clone().context("task segment fabric missing")?;
        let child_usage_owned = child_usage.clone();
        let provider_name_owned = provider_name.to_string();
        let model_name = model.to_string();
        let temperature_value = temperature;
        let tools_registry_owned = tools_registry.clone();
        let workspace_dir_owned = child_config_owned.workspace_dir.clone();
        let multimodal_config_owned = child_config_owned.multimodal.clone();
        let cancel_token_owned = cancel_token.clone();
        let security = security.clone();
        // Clone the event-bridge senders per iteration so they survive across
        // steer-driven loop restarts (the inner spawn consumes its clones; the
        // originals stay owned by this outer loop). When `None`, the loop runs
        // silently exactly as before (channels/gateway behaviour unchanged).
        //
        // NOTE: the agent stays `silent = true` regardless. `silent` only gates
        // the loop's *direct* `print!` to stdout (turn_engine.rs); a background
        // sub-agent must never print to the chat's terminal (it would corrupt
        // the TUI). The `on_delta` / `on_tool_call` channel sends are NOT gated
        // by `silent`, so the event bridge streams to the drainer either way.
        let on_delta_iter = on_delta.clone();
        let on_tool_call_iter = on_tool_call.clone();
        // NeedsInput: clone the per-run resolver for this iteration. When present
        // (chat `/bg` only), a supervised `ApprovalManager` is built so the loop
        // consults the resolver at the approval decision point (suspend-on-gate).
        // When absent (channels/gateway), no manager + no resolver = the
        // historical auto-fail-on-gate path (zero behaviour change).
        let approval_resolver_iter = approval_resolver.clone();
        // Task-locals do not cross `tokio::spawn`, so this run's beat is
        // re-installed inside the child task. It was minted on the *spawning*
        // task and is parented on the spawner's beat, so a parent legitimately
        // blocked on a visibly-working sub-agent is not mistaken for wedged —
        // and the same events are what make this run's own silence visible.
        //
        // MUTATION GUARD: reading `current_beat()` here instead reads the beat
        // of the spawned monitor task, which has none, and both properties are
        // silently lost.
        let run_beat = Arc::clone(&progress);
        let cleanup_parent = crate::runtime::turn_cleanup::current_tracker();
        let (cleanup_ack, cleanup_confirmed) = tokio::sync::oneshot::channel();
        let (segment_admitted, mut segment_admission) = tokio::sync::oneshot::channel();
        let loop_cleanup =
            crate::runtime::turn_cleanup::CleanupScope::new(cleanup_parent.as_ref(), CancellationToken::new())?
                .with_acknowledgement(Some(cleanup_ack));
        let mut loop_handle = tokio::spawn(crate::agent::idle::scope_beat(Some(run_beat), async move {
            // Carry the segment proof across the spawn boundary explicitly.
            let child_source = child_source_owned;
            let child_service = child_service_owned;
            let (observer, hooks): (Arc<dyn crate::observability::Observer>, Arc<HookManager>) =
                match ordinary_runtime_owned {
                    Some(runtime) => (runtime.observer, runtime.hooks),
                    None => (
                        Arc::new(NoopObserver),
                        Arc::new(HookManager::new(workspace_dir_owned.clone())),
                    ),
                };
            let context = &child_source.1;
            let envelope = &context.envelope;
            let scope_ctx = ScopeContext::from_execution_context(
                &security,
                context,
                envelope.recipient.as_deref().unwrap_or_default(),
            );
            let memory_owned = child_fabric_owned.memory();
            // Only build an `ApprovalManager` when a resolver is attached. Under
            // permission-model Phase 1 the unified `SecurityPolicy::decide`
            // (supervised: act-tools → Ask) is what routes a call into the
            // resolver suspend path; the manager is just the UI/grant layer. Built
            // from the live policy's autonomy level so `Full` / `ReadOnly` never
            // suspend.
            let approval_manager = approval_resolver_iter
                .as_ref()
                .map(|_| crate::approval::ApprovalManager::new());
            let result = async {
                let child_turn = crate::runtime::child_turn::ChildSegmentTurn::admit(
                    child_service,
                    child_source.clone(),
                    child_fabric_owned.clone(),
                    child_config_owned,
                    cleanup_parent.as_ref().context("child segment cleanup owner missing")?,
                    &cancel_token_owned,
                    child_usage_owned,
                )
                .await?
                .with_child_cleanup(cleanup_confirmed);
                let _ = segment_admitted.send(());
                if child_turn.is_close()? {
                    loop_cleanup.run(async { Ok(()) }).await?;
                    let reply = child_turn
                        .close(&cancel_token_owned)
                        .await?
                        .context("child segment close disposition changed")?;
                    task_history.push(ChatMessage::assistant(reply.clone()));
                    if let Some(sender) = on_delta_iter.as_ref() {
                        let _ = sender.try_send(reply.clone());
                    }
                    return Ok(SubAgentTaskResult {
                        segment_usage: Vec::new(),
                        output: reply,
                        history_commit_len: task_history.len(),
                    });
                }
                if let Some(system) = initial_system {
                    let system = child_turn.prompt_with_skills(&system)?;
                    if let Some(previous) = task_history.first_mut().filter(|message| message.role == "system") {
                        *previous = ChatMessage::system(system);
                    } else {
                        task_history.insert(0, ChatMessage::system(system));
                    }
                }
                if let Some(task) = initial_task {
                    task_history.push(crate::agent::turn_contracts::inbound_user_message(&task));
                }
                child_turn.enrich_input(&mut task_history)?;
                let loop_outcome = loop_cleanup
                    .run(crate::agent::turn_engine::run_turn(
                        &mut task_history,
                        crate::agent::turn_request::TurnRequest {
                            channel_name: "sessions_spawn",
                            // chat event bridge: incremental loop output (v1.1a)
                            scope: Some(&scope_ctx),
                            mode: crate::agent::turn_contracts::ChatMode::default(),
                            silent: true,
                            expose_stay_silent: false,
                            multimodal: &multimodal_config_owned,
                            schedule: crate::agent::turn_request::TurnSchedule {
                                read_only_tool_concurrency_window: 2,
                                priority_scheduling_enabled: false,
                                low_priority_tool_names: vec![
                                    "sessions_spawn".to_string(),
                                    "delegate".to_string(),
                                    "cron".to_string(),
                                ],
                            },
                            compaction: Some(child_turn.compaction()),
                            decision: child_turn.routing(),
                        },
                        crate::agent::turn_request::TurnServices {
                            provider: provider_instance.as_ref(),
                            provider_name: &provider_name_owned,
                            model: &model_name,
                            temperature: temperature_value,
                            tools_registry: tools_registry_owned,
                            observer: observer.as_ref(),
                            hooks: hooks.as_ref(),
                            // silent — never print to the chat terminal (see note above)
                            approval: approval_manager.map(Arc::new),
                            approval_resolver: approval_resolver_iter,
                            runtime_adapter: Some(child_turn.adapter()),
                            memory: crate::agent::turn_request::ToolLoopMemory::new(
                                &memory_owned,
                                &workspace_dir_owned,
                                Some(
                                    DocumentIngestRuntime::from_scope(memory_owned.clone(), &scope_ctx)
                                        .into_nested_turn(),
                                ),
                            )
                            .with_event_fabric(child_fabric_owned),
                        },
                        crate::agent::turn_request::TurnDelivery {
                            cancellation: Some(cancel_token_owned.clone()),
                            on_delta: on_delta_iter,
                            on_tool_call: on_tool_call_iter,
                        },
                    ))
                    .await;
                let (outcome, trace) = match loop_outcome {
                    Ok(result) => result,
                    Err(error) => {
                        child_turn
                            .fail(
                                &error,
                                cancel_token_owned.is_cancelled()
                                    || crate::agent::turn_contracts::is_tool_loop_cancelled(&error),
                            )
                            .await?;
                        return Err(error);
                    }
                };
                let segment_usage = child_turn
                    .complete(&outcome, &trace)
                    .await?
                    .usage_settlement
                    .into_iter()
                    .collect();
                Ok(SubAgentTaskResult {
                    segment_usage,
                    output: outcome.into_text(),
                    history_commit_len: task_history.len(),
                })
            }
            .await;
            (task_history, result)
        }));
        let _loop_abort = AbortLoopOnDrop(loop_handle.abort_handle());

        // A queued steer may arrive immediately after the run is published.
        // Do not let it cancel this segment until terminal ownership exists:
        // otherwise admission can be dropped before a terminal guard is built,
        // and the next segment correctly refuses the unconfirmed predecessor.
        tokio::select! {
            loop_result = &mut loop_handle => {
                let (returned_history, result) = loop_result?;
                history = returned_history;
                *history_out.write().await = chat_messages_to_history(&history);
                return settle_sub_agent_task_result(settled_segment_usage, result);
            }
            admitted = &mut segment_admission => {
                if admitted.is_err() {
                    let (returned_history, result) = loop_handle.await?;
                    history = returned_history;
                    *history_out.write().await = chat_messages_to_history(&history);
                    return settle_sub_agent_task_result(settled_segment_usage, result);
                }
            }
        }

        // Race: loop completion vs steering message
        tokio::select! {
            loop_result = &mut loop_handle => {
                // Inner loop finished (naturally or via error)
                let (returned_history, result) = loop_result?;
                history = returned_history;
                // Write final history to shared store
                *history_out.write().await = chat_messages_to_history(&history);
                return settle_sub_agent_task_result(settled_segment_usage, result);
            },
            steer_opt = steer_rx.recv() => {
                match steer_opt {
                    Some(steer_msg) => {
                        // Cancel the running inner loop
                        cancel_token.cancel();
                        // Wait for the task to acknowledge cancellation and return history
                        let (returned_history, cancelled_result) = loop_handle.await?;
                        history = returned_history;
                        match cancelled_result {
                            Err(error) if error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>() => {
                                *history_out.write().await = chat_messages_to_history(&history);
                                return Err(error);
                            }
                            Ok(mut completed) => settled_segment_usage.append(&mut completed.segment_usage),
                            Err(_) => {}
                        }
                        // NeedsInput: the cancelled inner loop may have been parked
                        // on a suspended approval (`resolve()` future dropped on
                        // cancel). Its registry status can be a zombie
                        // `AwaitingInput` if the resolver's `Drop` `try_write`
                        // restore was skipped under contention. We are about to
                        // re-run the loop, so deterministically restore `Running`
                        // here (async, authoritative) — never clobbering a terminal
                        // state set by a concurrent kill / timeout.
                        restore_running().await;
                        let segment_input = steering_instruction(&steer_msg);
                        if let Some((grant, binding, segments)) = child_segments.as_mut() {
                            let receipt = segments.record(
                                child_fabric.as_ref().context("child segment fabric missing")?,
                                &segment_input,
                            ).await?;
                            let proof = Arc::new(binding.source_proof(grant, &receipt, &provider)?);
                            let execution_context = receipt.execution_context(&proof)?;
                            child_source = Some((proof, execution_context));
                        } else if let Some(segments) = ordinary_segments.as_mut() {
                            child_source = Some(segments.record(&segment_input).await?);
                        } else if let Some(segments) = operator_segments.as_mut() {
                            child_source = Some(segments.record(&segment_input).await?);
                        }
                        // Inject the steering message as a user turn
                        tracing::info!("Sub-agent steering: injecting message");
                        history.push(ChatMessage::user(segment_input));
                        // Update shared history so callers can see the injected message
                        *history_out.write().await = chat_messages_to_history(&history);
                        // Loop continues — will re-enter with updated history
                    }
                    None => {
                        // Steer channel closed — no more steering; wait for natural completion
                        let (returned_history, result) = loop_handle.await?;
                        history = returned_history;
                        *history_out.write().await = chat_messages_to_history(&history);
                        return settle_sub_agent_task_result(settled_segment_usage, result);
                    }
                }
            }
        }
    }
}
