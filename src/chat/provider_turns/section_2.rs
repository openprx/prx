#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_ready_per_turn_contexts(
    per_turn_contexts: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, PerTurnContext>,
    pending_ordered_provider_turn_commits: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    >,
    pending_provider_completion_events: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionEvent,
    >,
    turn_signal: &dispatcher::TurnCompletionSignal,
    provider_turn_completion_contexts: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionContext,
    >,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    provider_turn_finalizer_events: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    history_commit_coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    plain_mode: bool,
    plain_mode_turn_failed: &mut bool,
) -> bool {
    let ready_task_ids: Vec<_> = per_turn_contexts
        .keys()
        .copied()
        .filter(|task_id| pending_provider_completion_events.contains_key(task_id))
        .collect();
    if ready_task_ids.is_empty() {
        return false;
    }
    for task_id in ready_task_ids {
        if turn_signal.ensure_history_commits_healthy().is_err() {
            break;
        }
        let Some(pending) = per_turn_contexts.remove(&task_id) else {
            continue;
        };
        let completion = pending_provider_completion_events.remove(&task_id);
        finalize_per_turn_context(
            pending,
            completion,
            pending_ordered_provider_turn_commits,
            turn_signal,
            provider_turn_completion_contexts,
            history,
            tools_registry,
            provider_turn_finalizer_events,
            turn_scheduler,
            history_commit_coordinator,
            provider_turn_workers,
            chat_dispatcher,
            terminal,
            memory_fabric,
            chat_session_key,
            config,
            sessions_redraw_handle,
            redraw_tx_for_main,
            plain_mode,
            plain_mode_turn_failed,
        )
        .await;
    }
    true
}

#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_all_per_turn_contexts_as_cancelled(
    per_turn_contexts: &mut std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, PerTurnContext>,
    pending_ordered_provider_turn_commits: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        PendingOrderedProviderTurnCommit,
    >,
    turn_signal: &dispatcher::TurnCompletionSignal,
    provider_turn_completion_contexts: &mut std::collections::HashMap<
        crate::chat::turn_scheduler::TurnTaskId,
        ProviderTurnCompletionContext,
    >,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    provider_turn_finalizer_events: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    history_commit_coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    provider_turn_workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    terminal: &Arc<TerminalChannel>,
    memory_fabric: &MemoryFabric,
    chat_session_key: &str,
    config: &Config,
    sessions_redraw_handle: Option<&mpsc::Sender<()>>,
    redraw_tx_for_main: Option<&mpsc::Sender<()>>,
    plain_mode: bool,
    plain_mode_turn_failed: &mut bool,
) {
    let pending_turns: Vec<_> = per_turn_contexts.drain().map(|(_, pending)| pending).collect();
    for pending in pending_turns {
        finalize_per_turn_context(
            pending,
            None,
            pending_ordered_provider_turn_commits,
            turn_signal,
            provider_turn_completion_contexts,
            history,
            tools_registry,
            provider_turn_finalizer_events,
            turn_scheduler,
            history_commit_coordinator,
            provider_turn_workers,
            chat_dispatcher,
            terminal,
            memory_fabric,
            chat_session_key,
            config,
            sessions_redraw_handle,
            redraw_tx_for_main,
            plain_mode,
            plain_mode_turn_failed,
        )
        .await;
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn drain_provider_turn_finalizer_events(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    queue: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
) -> Vec<ProviderTurnFinalizerResult> {
    let mut results = Vec::new();
    while let Some(event) = queue.pop_front() {
        results.extend(finalize_provider_turn_from_event(
            scheduler,
            coordinator,
            workers,
            event,
        ));
    }
    results
}

#[cfg(feature = "terminal-tui")]
pub(super) fn drain_provider_turn_finalizer_events_and_publish(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    queue: &mut std::collections::VecDeque<ProviderTurnFinalizerEvent>,
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
) -> Vec<ProviderTurnFinalizerResult> {
    let results = drain_provider_turn_finalizer_events(scheduler, coordinator, workers, queue);
    if !results.is_empty() {
        publish_main_queue_status(chat_dispatcher, scheduler);
        publish_provider_worker_status(chat_dispatcher, workers);
    }
    results
}

pub(super) fn start_provider_turn_task(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    existing_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    input: &str,
    history_base_len: usize,
) -> Option<crate::chat::turn_scheduler::TurnTaskId> {
    let id = existing_task_id.unwrap_or_else(|| {
        scheduler.enqueue(
            input.to_string(),
            crate::chat::turn_scheduler::TurnPriority::Normal,
            history_base_len,
        )
    });
    if let Err(error) = scheduler.start_task(id) {
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            reused_queued_task = existing_task_id.is_some(),
            error = %crate::agent::diagnostics::error_class(&error),
            "TurnScheduler provider task start failed"
        );
        return None;
    }
    Some(id)
}

pub(super) fn register_provider_history_commit_task(
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
) {
    if let Some(id) = id
        && let Some(task) = scheduler.task(id)
        && let Err(error) = coordinator.register_task(task)
    {
        let task_seq: u64 = task.sequence;
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            task_seq,
            error = %crate::agent::diagnostics::error_class(&error),
            "HistoryCommitCoordinator provider task registration failed"
        );
    }
}

pub(super) fn register_provider_turn_worker(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    kind: crate::chat::turn_worker::ProviderTurnWorkerKind,
) {
    if let Some(id) = id
        && let Some(task) = scheduler.task(id)
        && let Err(error) = workers.start_from_task(task, kind)
    {
        let task_seq: u64 = task.sequence;
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            task_seq,
            kind = ?kind,
            error = %crate::agent::diagnostics::error_class(&error),
            "ProviderTurnWorkerRegistry provider worker registration failed"
        );
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn spawn_provider_turn_completion_waiter(
    turn_signal: dispatcher::TurnCompletionSignal,
    task_id: crate::chat::turn_scheduler::TurnTaskId,
    tx: mpsc::Sender<ProviderTurnCompletionEvent>,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let notified = turn_signal.notified_for(task_id);
        match notified {
            Some(notified) => {
                tokio::select! {
                    () = notified => {}
                    () = shutdown.cancelled() => {}
                }
            }
            None => {
                tokio::select! {
                    () = turn_signal.notified() => {}
                    () = shutdown.cancelled() => {}
                }
            }
        }
        let event = ProviderTurnCompletionEvent {
            task_id,
            outcome: turn_signal.consume_turn_outcome(task_id),
            usage: turn_signal.consume_turn_usage(task_id),
        };
        if tx.send(event).await.is_err() {
            tracing::debug!(
                task_id = crate::chat::turn_scheduler::TurnTaskId::get(task_id),
                "provider turn completion waiter could not deliver event"
            );
        }
    });
}

pub(super) fn request_provider_turn_cancel(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    reason: &'static str,
) {
    if let Some(id) = id {
        if let Err(error) = scheduler.request_cancel(id) {
            tracing::warn!(
                task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
                reason,
                error = %crate::agent::diagnostics::error_class(&error),
                "TurnScheduler provider task cancel request failed"
            );
        }
        if let Err(error) = workers.request_cancel(id) {
            tracing::warn!(
                task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
                reason,
                error = %crate::agent::diagnostics::error_class(&error),
                "ProviderTurnWorkerRegistry provider worker cancel request failed"
            );
        }
    }
}

/// Record the user's request when the turn is admitted (the non-TUI path;
/// its completion commits with `record_user: false`). The send is bounded and
/// awaited on the ordered action channel
/// ([`crate::chat::dispatcher::ChatDispatcher::dispatch_bounded`]): a full
/// channel delays the turn instead of dropping the request, so the later
/// commit always includes it. A send that is not accepted latches the
/// unconfirmed-commit stop once and fails the turn before it runs.
///
/// Returns the admission's identity: a failed or cancelled turn settles by
/// it ([`settle_aborted_turn`]), so only this request is ever removed.
pub(super) async fn record_user_turn_at_admission(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    commits: &crate::chat::dispatcher::TurnCompletionSignal,
    user_input: &str,
    user_event_id: Option<&str>,
) -> anyhow::Result<crate::chat::action::AdmissionId> {
    let admission = crate::chat::action::AdmissionId::next();
    chat_dispatcher
        .dispatch_bounded(
            crate::chat::action::Action::UserTurnAdmitted {
                admission,
                content: user_input.to_owned(),
                event_id: user_event_id.map(str::to_owned),
            },
            commits,
        )
        .await
        .map_err(|error| anyhow::anyhow!("Chat stopped after an unconfirmed history commit: {error}"))?;
    Ok(admission)
}

/// The acknowledged session as of every action sent before this call: an
/// awaited no-op (`RedrawRequested`) on the same ordered channel. A turn's
/// admission reads its start state through it, so a compaction another turn
/// already sent is never missed by a racing snapshot read. A failed
/// acknowledgement latches `commits` and is the unconfirmed-commit stop.
pub(super) async fn acknowledged_session_barrier(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    commits: &crate::chat::dispatcher::TurnCompletionSignal,
) -> anyhow::Result<crate::chat::acked_session::AckedSession> {
    match chat_dispatcher
        .dispatch_awaited(crate::chat::action::Action::RedrawRequested, commits)
        .await
        .map_err(|error| anyhow::anyhow!("Chat stopped after an unconfirmed history commit: {error}"))?
    {
        crate::chat::acked_session::Ack::Published(acked) | crate::chat::acked_session::Ack::Unchanged(acked) => {
            Ok(acked)
        }
        crate::chat::acked_session::Ack::Rejected(_) => Ok(chat_dispatcher.session_reader().acked()),
    }
}

/// Settle a failed or cancelled turn with the session authority, in order
/// after its terminal action and every in-turn compaction the turn sent:
/// `TurnAbortSettled` removes the request admitted as `admission` (when the
/// turn recorded one and that admission is still open) and saves, and the
/// acknowledged revision is returned. Then the run loop's provider view
/// follows that revision ([`provider_view_after_settled_abort`]).
///
/// A failed acknowledgement latches `commits` and is returned as the
/// unconfirmed-commit stop; a refusal (which this action never produces for
/// a live session) is latched the same way.
pub(super) async fn settle_aborted_turn(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    commits: &crate::chat::dispatcher::TurnCompletionSignal,
    admission: Option<crate::chat::action::AdmissionId>,
) -> anyhow::Result<crate::chat::acked_session::AckedSession> {
    let ack = chat_dispatcher
        .dispatch_awaited(crate::chat::action::Action::TurnAbortSettled { admission }, commits)
        .await
        .map_err(|error| anyhow::anyhow!("Chat stopped after an unconfirmed history commit: {error}"))?;
    match ack {
        crate::chat::acked_session::Ack::Published(acked) | crate::chat::acked_session::Ack::Unchanged(acked) => {
            Ok(acked)
        }
        crate::chat::acked_session::Ack::Rejected(_) => {
            let reason = "the failed or cancelled turn could not be settled";
            commits.fail_history_commit(reason.to_owned());
            anyhow::bail!("Chat stopped after an unconfirmed history commit: {reason}")
        }
    }
}

/// The run loop's provider view after a settled failed or cancelled turn.
/// The aborted request is never kept: the settled snapshot already excludes it
/// (admission identity, `TurnAbortSettled`), so no text is matched to remove
/// anything. When the session was compacted or the view was changed at any
/// point since before the turn's preflight (`settled` carries a compaction
/// epoch other than `compaction_epoch_before_preflight`, or the turn itself
/// reported one), the view is rebuilt from the settled snapshot; otherwise it
/// is cut back to `len_before_user_turn`, its length before the request
/// (`None` for a view the request was never pushed to). Returns whether it
/// rebuilt the view.
pub(super) fn provider_view_after_settled_abort(
    history: &mut Vec<crate::providers::ChatMessage>,
    settled: &crate::chat::acked_session::AckedSession,
    compaction_epoch_before_preflight: u64,
    compacted_in_turn: bool,
    len_before_user_turn: Option<usize>,
) -> bool {
    if settled.compaction_epoch == compaction_epoch_before_preflight && !compacted_in_turn {
        if let Some(len) = len_before_user_turn {
            rollback_cancelled_turn_history(history, len);
        }
        return false;
    }
    let view = super::compaction::provider_view_from_acked(history, settled);
    tracing::info!(
        messages_before = history.len(),
        messages_after = view.len(),
        "provider view rebuilt from the settled snapshot after a compaction in a failed or cancelled turn"
    );
    *history = view;
    true
}

/// Dispatch a turn's final metered usage (`FinalAggregate`) unconditionally,
/// before the turn's awaited commit; the reducer deduplicates per task. The
/// scheduler bookkeeping uses the same terminal usage.
///
/// The send waits for channel capacity within the acknowledgement bound
/// ([`crate::chat::dispatcher::ChatDispatcher::dispatch_bounded`]); when the
/// usage is not accepted the history commit is latched as failed and the
/// error is returned, and the caller must not commit the turn.
pub(super) async fn dispatch_final_usage(
    chat_dispatcher: &crate::chat::dispatcher::ChatDispatcher,
    commits: &crate::chat::dispatcher::TurnCompletionSignal,
    turn_scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    terminal_usage: Option<crate::chat::session::MainSessionTokenUsageRecord>,
    site: &'static str,
) -> Result<(), crate::chat::acked_session::AckError> {
    let Some(record) = terminal_usage else {
        return Ok(());
    };
    record_provider_turn_usage(turn_scheduler, task_id, &record);
    chat_dispatcher
        .dispatch_bounded(
            crate::chat::action::Action::ProviderUsageRecorded {
                task_id,
                usage_kind: crate::chat::action::ProviderUsageRecordKind::FinalAggregate,
                record,
            },
            commits,
        )
        .await
        .inspect_err(|error| tracing::error!(site, error_kind = error.kind(), "final provider usage was not accepted"))
}

pub(super) fn record_provider_turn_usage(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    record: &crate::chat::session::MainSessionTokenUsageRecord,
) {
    if let Some(id) = id
        && let Err(error) = scheduler.record_usage(id, record)
    {
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            error = %crate::agent::diagnostics::error_class(&error),
            "TurnScheduler provider usage ledger record failed"
        );
    }
}

pub(super) fn mark_provider_turn_completed(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_commit_len: usize,
    summary: &'static str,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    if let Some(id) = id {
        match scheduler.mark_completed(id, history_commit_len, summary) {
            Ok(()) => {
                if record_provider_worker_completed(workers, id) {
                    return record_provider_history_commit_outcome(coordinator, workers, scheduler, id);
                }
            }
            Err(error) => {
                tracing::warn!(
                    task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
                    error = %crate::agent::diagnostics::error_class(&error),
                    "TurnScheduler provider task completion failed"
                );
            }
        }
    }
    Vec::new()
}

fn mark_provider_turn_failed(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    history_commit_len: usize,
    summary: impl Into<String>,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    if let Some(id) = id {
        match scheduler.mark_failed(id, history_commit_len, summary) {
            Ok(()) => {
                if record_provider_worker_failed(workers, id) {
                    return record_provider_history_commit_outcome(coordinator, workers, scheduler, id);
                }
            }
            Err(error) => {
                tracing::warn!(
                    task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
                    error = %crate::agent::diagnostics::error_class(&error),
                    "TurnScheduler provider task failure failed"
                );
            }
        }
    }
    Vec::new()
}

fn mark_provider_turn_cancelled(
    scheduler: &mut crate::chat::turn_scheduler::TurnScheduler,
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    summary: &'static str,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    if let Some(id) = id {
        match scheduler.mark_cancelled(id, summary) {
            Ok(()) => {
                if record_provider_worker_cancelled(workers, id) {
                    return record_provider_history_commit_outcome(coordinator, workers, scheduler, id);
                }
            }
            Err(error) => {
                tracing::warn!(
                    task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
                    error = %crate::agent::diagnostics::error_class(&error),
                    "TurnScheduler provider task cancellation failed"
                );
            }
        }
    }
    Vec::new()
}

fn record_provider_worker_completed(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> bool {
    if let Err(error) = workers.record_completed(id) {
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            error = %crate::agent::diagnostics::error_class(&error),
            "ProviderTurnWorkerRegistry provider worker completion failed"
        );
        return false;
    }
    true
}

fn record_provider_worker_failed(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> bool {
    if let Err(error) = workers.record_failed(id) {
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            error = %crate::agent::diagnostics::error_class(&error),
            "ProviderTurnWorkerRegistry provider worker failure failed"
        );
        return false;
    }
    true
}

fn record_provider_worker_cancelled(
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> bool {
    if let Err(error) = workers.record_cancelled(id) {
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            error = %crate::agent::diagnostics::error_class(&error),
            "ProviderTurnWorkerRegistry provider worker cancellation failed"
        );
        return false;
    }
    true
}

fn record_provider_history_commit_outcome(
    coordinator: &mut crate::chat::history_commit::HistoryCommitCoordinator,
    workers: &mut crate::chat::turn_worker::ProviderTurnWorkerRegistry,
    scheduler: &crate::chat::turn_scheduler::TurnScheduler,
    id: crate::chat::turn_scheduler::TurnTaskId,
) -> Vec<crate::chat::history_commit::HistoryCommitDecision> {
    let Some(task) = scheduler.task(id) else {
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            "HistoryCommitCoordinator missing scheduler task for provider outcome"
        );
        return Vec::new();
    };
    let Some(outcome) = crate::chat::history_commit::HistoryCommitOutcome::from_terminal_task(task) else {
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            state = task.state.kind(),
            "HistoryCommitCoordinator ignored non-terminal provider outcome"
        );
        return Vec::new();
    };
    if let Err(error) = coordinator.record_outcome(outcome) {
        let task_seq: u64 = task.sequence;
        tracing::warn!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            task_seq,
            error = %crate::agent::diagnostics::error_class(&error),
            "HistoryCommitCoordinator provider outcome record failed"
        );
        return Vec::new();
    }
    let mut decisions = Vec::new();
    for decision in coordinator.drain_ready() {
        if let Err(error) = workers.apply_commit_decision(&decision) {
            tracing::warn!(
                task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
                decision_kind = decision.kind(),
                error = %crate::agent::diagnostics::error_class(&error),
                "ProviderTurnWorkerRegistry provider worker commit decision failed"
            );
        }
        tracing::trace!(
            task_id = crate::chat::turn_scheduler::TurnTaskId::get(id),
            decision_kind = decision.kind(),
            pending_tasks = coordinator.pending_tasks(),
            pending_outcomes = coordinator.pending_outcomes(),
            "HistoryCommitCoordinator provider outcome became ready"
        );
        decisions.push(decision);
    }
    decisions
}
