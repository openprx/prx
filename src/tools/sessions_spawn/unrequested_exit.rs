//! A worker that died on its own, with its owner alive, ends as failed.
//!
//! When nobody asked for a stop and the worker's leader has been reaped with
//! no result line on its pipes, post-exit recovery has already looked for a
//! signed completion record. Nothing is left that could still write one: the
//! only writer is the worker itself, it is reaped, and its output pipes are
//! drained. So the parent settles the run as failed instead of leaving it
//! unconfirmed (which clients show as `running` forever). An exit that follows
//! a requested stop, an owner exit or an owner loss is not converted here.

use super::{ProcessWorkerOutcome, describe_worker_exit};
use std::process::ExitStatus;

/// Convert an interrupted ending into `ExitedUnrequested` when the worker
/// exited with an empty stdout (`silent_exit` is its status) and no stop was
/// requested. Every other outcome passes through, so a completion that
/// recovery verified after the reap is never replaced.
pub(super) fn settle_unrequested_exit(
    run_id: &str,
    outcome: anyhow::Result<ProcessWorkerOutcome>,
    silent_exit: Option<ExitStatus>,
) -> anyhow::Result<ProcessWorkerOutcome> {
    let Some(status) = silent_exit else {
        return outcome;
    };
    let (usage, unfinished_segments) = match outcome {
        Ok(ProcessWorkerOutcome::OrdinaryInterrupted { usage, .. })
        | Ok(ProcessWorkerOutcome::OperatorInterrupted { usage, .. }) => (usage.records, usage.unfinished_segments),
        other => return other,
    };
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    tracing::info!(
        run_id,
        exit_code = status.code(),
        signal,
        unfinished_segments,
        usage_records = usage.len(),
        "unrequested worker exit settled as failed"
    );
    Ok(ProcessWorkerOutcome::ExitedUnrequested {
        // Status only: the worker's stderr is not copied into the record.
        detail: describe_worker_exit(status, ""),
        usage,
    })
}
