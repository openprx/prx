//! A run that was stopped on request ends as stopped, not as unconfirmed.
//!
//! When the stop was asked for (the tool `kill` action, or a registry kill of
//! the run or of its worker row) the worker group is killed and its leader
//! reaped, and the post-exit recovery has already looked for a signed
//! completion record. If it found none, no writer is left that could still
//! produce one, so the outcome is known: the run was stopped. Only the provider
//! usage that recovery found is partial. A stop nobody asked for (owner exit, a
//! crash) is not converted and stays unconfirmed.

use super::{KILLED_BY_USER_REASON, ProcessRunControl, ProcessWorkerOutcome};

/// The reason to record when the run's stop was requested, if it was.
///
/// The registry marks the worker row `kill_requested` before it signals the
/// group, while the control's termination reason is set only later by the
/// registry bridge, so the row flag covers a kill that has no reason yet.
pub(super) fn requested_stop_reason(control: &ProcessRunControl, worker_row_killed: bool) -> Option<String> {
    match control.termination_reason() {
        Some(reason) if reason == KILLED_BY_USER_REASON => Some(reason),
        Some(_) => None,
        None => worker_row_killed.then(|| KILLED_BY_USER_REASON.to_owned()),
    }
}

/// Convert an unconfirmed ending into `Terminated` when the stop was requested.
/// Every other outcome passes through; a completion that recovery verified
/// after the reap is never replaced.
pub(super) fn settle_requested_stop(
    run_id: &str,
    outcome: anyhow::Result<ProcessWorkerOutcome>,
    reason: Option<&str>,
) -> anyhow::Result<ProcessWorkerOutcome> {
    let Some(reason) = reason else {
        return outcome;
    };
    let (usage, unfinished_segments) = match outcome {
        Ok(ProcessWorkerOutcome::OrdinaryInterrupted { usage, .. })
        | Ok(ProcessWorkerOutcome::OperatorInterrupted { usage, .. }) => (usage.records, usage.unfinished_segments),
        Ok(ProcessWorkerOutcome::ExitedWithoutResult(_)) => (Vec::new(), 0),
        other => return other,
    };
    tracing::info!(
        run_id,
        unfinished_segments,
        usage_records = usage.len(),
        "requested stop settled as terminated"
    );
    Ok(ProcessWorkerOutcome::Terminated {
        reason: reason.to_owned(),
        usage,
    })
}
