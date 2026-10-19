use super::{
    KILLED_BY_USER_REASON, PANICKED_BEFORE_RESULT_SUFFIX, ProcessFinalization, ProcessRunControl, SubAgentRun,
    SubAgentStatus, TERMINATED_BEFORE_RESULT_SUFFIX, TaskFinalization, TaskRunControl, commit_process_terminal_state,
    commit_run_termination_if_unfinished, process_terminal, sub_agent_reported_failure, task_terminal,
};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Why a monitor task stopped without reporting, when it stopped that way.
pub(super) fn vanished_monitor_reason(error: &tokio::task::JoinError, kind: &str) -> String {
    if error.is_cancelled() {
        format!("{kind} {TERMINATED_BEFORE_RESULT_SUFFIX}")
    } else {
        format!("{kind} {PANICKED_BEFORE_RESULT_SUFFIX}")
    }
}

/// Watch a task-mode monitor from *outside* the run's registry subtree.
///
/// Awaiting the handle here neither aborts nor keeps the monitor alive; it only
/// observes how it ended. The watcher deliberately runs in a task that carries
/// no registry scope, so a cascade kill of the run cannot take the observer
/// down with the observed.
pub(super) fn watch_task_mode_monitor(
    handle: tokio::task::JoinHandle<()>,
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: String,
    cleanup: Option<Arc<crate::runtime::turn_cleanup::CleanupTracker>>,
    task_control: Option<Arc<TaskRunControl>>,
    mut work_guard: Option<crate::runtime::registry::WorkGuard>,
    terminal_journal: Option<Arc<task_terminal::TaskTerminalJournal>>,
) {
    tokio::spawn(async move {
        let joined = handle.await;
        if let Some(control) = &task_control {
            control.revoke_authority();
        }
        // The observer outlives abort of the run, and owns its cleanup ticket
        // chain independently of the turn that originally spawned the run.
        let drained = match cleanup {
            Some(owner) => {
                owner
                    .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(15))
                    .await
            }
            None => Ok(()),
        };
        let recovered = if drained.is_ok() {
            if let Some(journal) = terminal_journal.as_ref() {
                let interruption = joined.as_ref().err().map(|error| {
                    let status = if error.is_cancelled() {
                        crate::agent::terminal::TurnTerminalStatus::Cancelled
                    } else {
                        crate::agent::terminal::TurnTerminalStatus::Failed
                    };
                    let reason = task_control
                        .as_ref()
                        .and_then(|control| control.reason())
                        .unwrap_or_else(|| vanished_monitor_reason(error, "sub-agent run"));
                    (status, reason)
                });
                Some(
                    tokio::time::timeout(std::time::Duration::from_secs(15), journal.recover(interruption))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|result| result),
                )
            } else {
                None
            }
        } else {
            None
        };
        let (reason, mut finalization) = if drained.is_err() {
            tracing::error!(run_id = %run_id, "Sub-agent child cleanup remains unconfirmed");
            (
                Some("sub-agent child cleanup remains unconfirmed".to_owned()),
                TaskFinalization::Unconfirmed,
            )
        } else if let Some(recovered) = recovered {
            let reconciled = match recovered {
                Ok(committed) => {
                    let mut runs = active_runs.write().await;
                    runs.iter_mut().find(|run| run.id == run_id).map_or_else(
                        || {
                            Err(anyhow::anyhow!(
                                "task registry disappeared before terminal reconciliation"
                            ))
                        },
                        |run| committed.reconcile_run(run),
                    )
                }
                Err(error) => Err(error),
            };
            match reconciled {
                Ok(status) => (
                    None,
                    if status == crate::agent::terminal::TurnTerminalStatus::Cancelled {
                        TaskFinalization::Terminated
                    } else {
                        TaskFinalization::Natural
                    },
                ),
                Err(error) => {
                    tracing::error!(run_id = %run_id, error = %error, "task terminal recovery remains unconfirmed");
                    (
                        Some("task terminal recovery remains unconfirmed".to_owned()),
                        TaskFinalization::Unconfirmed,
                    )
                }
            }
        } else if let Err(error) = joined {
            if error.is_cancelled() {
                (
                    Some(
                        task_control
                            .as_ref()
                            .and_then(|control| control.reason())
                            .map(|reason| {
                                if reason == KILLED_BY_USER_REASON {
                                    reason
                                } else {
                                    format!("sub-agent run ({reason}) {TERMINATED_BEFORE_RESULT_SUFFIX}")
                                }
                            })
                            .unwrap_or_else(|| vanished_monitor_reason(&error, "sub-agent run")),
                    ),
                    TaskFinalization::Terminated,
                )
            } else {
                (
                    Some(vanished_monitor_reason(&error, "sub-agent run")),
                    TaskFinalization::Unconfirmed,
                )
            }
        } else {
            (None, TaskFinalization::Natural)
        };
        if let Some(reason) = reason {
            if commit_run_termination_if_unfinished(&active_runs, &run_id, &reason).await {
                tracing::warn!(run_id = %run_id, "{reason}");
            } else if finalization == TaskFinalization::Terminated {
                // A previously committed terminal result wins the abort race.
                finalization = TaskFinalization::Natural;
            }
        }
        if finalization != TaskFinalization::Unconfirmed {
            if let Some(guard) = work_guard.as_mut() {
                guard.mark_cleanup_confirmed();
            }
        }
        if let Some(control) = task_control {
            control.publish(finalization);
        }
    });
}

/// Install the same externally-owned task monitor as a production task-mode
/// spawn on a test registry row.
///
/// Tests that exercise kill through a public tool must give a `Running` row
/// the owner production always installs. The watcher, rather than the caller,
/// remains responsible for joining the aborted task, committing its terminal
/// status, and publishing finalization to the kill request.
#[cfg(test)]
pub(crate) async fn attach_test_task_mode_owner(
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: &str,
) -> anyhow::Result<()> {
    let task_control = TaskRunControl::new();
    let handle = tokio::spawn(std::future::pending::<()>());
    let abort_handle = handle.abort_handle();
    task_control.attach(abort_handle.clone());
    {
        let mut runs = active_runs.write().await;
        let run = runs
            .iter_mut()
            .find(|run| run.id == run_id)
            .ok_or_else(|| anyhow::anyhow!("test task row `{run_id}` is unavailable"))?;
        anyhow::ensure!(
            run.task_control.is_none() && run.abort_handle.is_none(),
            "test task row `{run_id}` already has a termination owner"
        );
        run.task_control = Some(Arc::clone(&task_control));
        run.abort_handle = Some(abort_handle);
    }
    watch_task_mode_monitor(
        handle,
        active_runs,
        run_id.to_owned(),
        None,
        Some(task_control),
        None,
        None,
    );
    Ok(())
}

/// Process-mode counterpart of [`watch_task_mode_monitor`].
///
/// Routes through [`commit_process_owner_failure_if_unfinalized`] so the run's
/// `ProcessRunControl` is finalized together with the status — a caller parked
/// on finalization must not be stranded by the very cancellation that ended the
/// monitor.
pub(super) fn watch_process_mode_monitor(
    handle: tokio::task::JoinHandle<()>,
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: String,
    process_control: Arc<ProcessRunControl>,
    mut work_guard: crate::runtime::registry::WorkGuard,
    verified_terminal: Arc<parking_lot::Mutex<Option<process_terminal::VerifiedWorkerCompletion>>>,
    recovery: Option<Arc<process_terminal::ProcessRecovery>>,
    ordinary_terminal: Arc<parking_lot::Mutex<Option<process_terminal::ordinary::VerifiedOrdinaryWorkerCompletion>>>,
    ordinary_recovery: Option<process_terminal::ordinary::OrdinaryProcessRecovery>,
    operator_terminal: Arc<parking_lot::Mutex<Option<process_terminal::operator::VerifiedOperatorWorkerCompletion>>>,
    operator_recovery: Option<process_terminal::operator::OperatorProcessRecovery>,
) {
    tokio::spawn(async move {
        let joined = handle.await;
        if !matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            let mut retained = verified_terminal.lock().clone();
            if retained.is_none() {
                if let Some(recovery) = recovery.as_ref() {
                    match recovery.recover().await {
                        Ok(completion) => retained = completion,
                        Err(error) => tracing::error!(run_id = %run_id, error = %error,
                            "process observer journal recovery remains unconfirmed"),
                    }
                }
            }
            if let Some(completion) = retained {
                if completion.run_id() == run_id {
                    let repair = recovery
                        .as_ref()
                        .map(|owner| owner.repair(completion.clone()))
                        .transpose();
                    let repair = match repair {
                        Ok(repair) => repair,
                        Err(error) => {
                            tracing::error!(run_id = %run_id, error = %error, "process repair authority unavailable");
                            None
                        }
                    };
                    let (result, usage) = completion.into_parts();
                    let status = if result.success {
                        SubAgentStatus::Completed(result.output)
                    } else {
                        SubAgentStatus::Failed(sub_agent_reported_failure(
                            result.error.as_deref().unwrap_or("worker failed"),
                        ))
                    };
                    if let Some(repair) = repair {
                        if let Err(error) = commit_process_terminal_state(
                            &active_runs,
                            &run_id,
                            status,
                            usage,
                            process_control.as_ref(),
                            ProcessFinalization::Natural,
                            Some(&repair),
                            #[cfg(test)]
                            None,
                        )
                        .await
                        {
                            tracing::error!(run_id = %run_id, error = %error,
                            "retained worker completion could not be reconciled");
                        }
                    }
                } else {
                    tracing::error!(run_id = %run_id, "retained worker completion belongs to another run");
                }
            }
        }
        if !matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            let mut completion = ordinary_terminal.lock().clone();
            if completion.is_none() {
                if let Some(recovery) = ordinary_recovery.as_ref() {
                    match recovery.recover().await {
                        Ok(recovered) => completion = recovered,
                        Err(error) => tracing::error!(run_id = %run_id, error = %error,
                            "ordinary monitor journal recovery remains unconfirmed"),
                    }
                }
            }
            if let Some(completion) = completion {
                let repair = completion.repair();
                match repair {
                    Ok(repair) => {
                        let status = if completion.result.success {
                            SubAgentStatus::Completed(completion.result.output)
                        } else {
                            SubAgentStatus::Failed(sub_agent_reported_failure(
                                completion.result.error.as_deref().unwrap_or("worker failed"),
                            ))
                        };
                        if let Err(error) = commit_process_terminal_state(
                            &active_runs,
                            &run_id,
                            status,
                            completion.usage,
                            process_control.as_ref(),
                            ProcessFinalization::Natural,
                            Some(&repair),
                            #[cfg(test)]
                            None,
                        )
                        .await
                        {
                            tracing::error!(run_id = %run_id, error = %error, "ordinary completion registry repair remains unconfirmed");
                        }
                    }
                    Err(error) => {
                        tracing::error!(run_id = %run_id, error = %error, "ordinary process exit remains unconfirmed")
                    }
                }
            }
        }
        if !matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            if let Some(recovery) = ordinary_recovery.as_ref() {
                if let Err(error) = recovery.retain_partial(&active_runs).await {
                    tracing::error!(run_id = %run_id, error = %error, "ordinary partial usage remains unconfirmed");
                }
            }
        }
        if !matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            let mut completion = operator_terminal.lock().clone();
            if completion.is_none() {
                if let Some(recovery) = operator_recovery.as_ref() {
                    match recovery.recover().await {
                        Ok(recovered) => completion = recovered,
                        Err(error) => tracing::error!(run_id = %run_id, error = %error,
                            "operator monitor journal recovery remains unconfirmed"),
                    }
                }
            }
            if let Some(completion) = completion {
                let repair = completion.repair();
                match repair {
                    Ok(repair) => {
                        let status = if completion.result.success {
                            SubAgentStatus::Completed(completion.result.output)
                        } else {
                            SubAgentStatus::Failed(sub_agent_reported_failure(
                                completion.result.error.as_deref().unwrap_or("worker failed"),
                            ))
                        };
                        if let Err(error) = commit_process_terminal_state(
                            &active_runs,
                            &run_id,
                            status,
                            completion.usage,
                            process_control.as_ref(),
                            ProcessFinalization::Natural,
                            Some(&repair),
                            #[cfg(test)]
                            None,
                        )
                        .await
                        {
                            tracing::error!(run_id = %run_id, error = %error, "operator completion registry repair remains unconfirmed");
                        }
                    }
                    Err(error) => {
                        tracing::error!(run_id = %run_id, error = %error, "operator process exit remains unconfirmed")
                    }
                }
            }
        }
        if !matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            if let Some(recovery) = operator_recovery.as_ref() {
                if let Err(error) = recovery.retain_partial(&active_runs).await {
                    tracing::error!(run_id = %run_id, error = %error, "operator partial usage remains unconfirmed");
                }
            }
        }
        if process_control.finalization().is_none() {
            let reason = joined.err().map_or_else(
                || "process monitor ended with unconfirmed settlement".to_owned(),
                |error| vanished_monitor_reason(&error, "sub-agent process monitor"),
            );
            let mut runs = active_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|run| run.id == run_id) {
                run.status = SubAgentStatus::Failed(format!("{reason}; cleanup and provider usage remain unconfirmed"));
                run.steer_tx = None;
            }
            process_control.finalize(ProcessFinalization::Unconfirmed);
            tracing::error!(run_id = %run_id, "process observer could not confirm cleanup and settlement");
        }
        if matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            work_guard.mark_cleanup_confirmed();
        }
        // Unconfirmed and failed termination retain an abandoned registry row.
    });
}
