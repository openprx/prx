#[cfg(test)]
use super::TestWorkerProtocolDiagnostic;
use super::{
    HistoryEntry, PROCESS_OUTPUT_DRAIN_TIMEOUT_SECS, PROCESS_REAP_TIMEOUT_SECS, ProcessRunControl, process_terminal,
};
use crate::session_worker::protocol::WorkerResult;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug)]
pub(super) enum OwnedChildExit {
    Exited(std::process::ExitStatus),
    Terminated(String),
    TerminationFailed(String),
}

pub(super) async fn hold_unresolved_child_ownership<T>(
    child: &tokio::process::Child,
    error: impl std::fmt::Display,
) -> T {
    tracing::error!(
        error = %error,
        "session-worker reap state is unresolved; retaining Child ownership and run slot"
    );
    let owned_child = child;
    loop {
        std::future::pending::<()>().await;
        let _ = owned_child.id();
    }
}

pub(super) async fn start_kill_and_reap_direct_child(
    child: &mut tokio::process::Child,
) -> anyhow::Result<std::process::ExitStatus> {
    let start_kill_error = child.start_kill().err();
    let bounded_wait =
        tokio::time::timeout(std::time::Duration::from_secs(PROCESS_REAP_TIMEOUT_SECS), child.wait()).await;
    let mut bounded_wait_timed_out = false;
    let status = match bounded_wait {
        Ok(Ok(status)) => status,
        Ok(Err(wait_error)) => {
            let error = match start_kill_error {
                Some(start_error) => anyhow::anyhow!(
                    "failed to start direct-child kill: {start_error}; direct-child wait failed: {wait_error}"
                ),
                None => wait_error.into(),
            };
            return hold_unresolved_child_ownership(child, error).await;
        }
        Err(_) => {
            bounded_wait_timed_out = true;
            match child.wait().await {
                Ok(status) => status,
                Err(wait_error) => {
                    let error = start_kill_error.as_ref().map_or_else(
                        || {
                            anyhow::anyhow!(
                                "direct child was not reaped within {PROCESS_REAP_TIMEOUT_SECS}s and the continuing owner wait failed: {wait_error}"
                            )
                        },
                        |start_error| {
                            anyhow::anyhow!(
                                "failed to start direct-child kill: {start_error}; direct child was not reaped within {PROCESS_REAP_TIMEOUT_SECS}s and the continuing owner wait failed: {wait_error}"
                            )
                        },
                    );
                    return hold_unresolved_child_ownership(child, error).await;
                }
            }
        }
    };
    if let Some(start_error) = start_kill_error {
        if bounded_wait_timed_out {
            anyhow::bail!(
                "failed to start direct-child kill: {start_error}; direct child was not reaped within {PROCESS_REAP_TIMEOUT_SECS}s, but the continuing owner wait later confirmed reap"
            );
        }
        anyhow::bail!("failed to start direct-child kill before confirmed reap: {start_error}");
    }
    Ok(status)
}

pub(super) async fn wait_for_reap_after_group_termination(
    child: &mut tokio::process::Child,
    #[cfg(test)] inject_wait_error: bool,
) -> anyhow::Result<std::process::ExitStatus> {
    #[cfg(test)]
    if inject_wait_error {
        return hold_unresolved_child_ownership(child, "injected post-termination child.wait error").await;
    }
    match child.wait().await {
        Ok(status) => Ok(status),
        Err(error) => hold_unresolved_child_ownership(child, error).await,
    }
}

pub(super) async fn terminate_owned_child(
    child: &mut tokio::process::Child,
    process_group: &mut OwnedProcessGroup,
) -> anyhow::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        if let Err(group_error) = process_group.terminate() {
            process_group.disarm();
            let status = start_kill_and_reap_direct_child(child).await.map_err(|direct_error| {
                anyhow::anyhow!(
                    "failed to kill session-worker process group: {group_error}; direct-child fallback failed: {direct_error}"
                )
            })?;
            #[cfg(not(test))]
            let _ = status;
            #[cfg(test)]
            process_group.observe_reap(status);
            return Err(group_error);
        }
    }

    #[cfg(not(unix))]
    {
        process_group.disarm();
        return start_kill_and_reap_direct_child(child).await;
    }

    let status = wait_for_reap_after_group_termination(
        child,
        #[cfg(test)]
        false,
    )
    .await?;
    #[cfg(test)]
    process_group.observe_reap(status);
    Ok(status)
}

pub(super) async fn wait_for_owned_process(
    child: &mut tokio::process::Child,
    process_group: &mut OwnedProcessGroup,
    process_control: &ProcessRunControl,
    #[cfg(test)] inject_wait_error: bool,
) -> anyhow::Result<OwnedChildExit> {
    #[cfg(test)]
    if inject_wait_error {
        relinquish_process_group_after_leader_exit(process_group);
        return hold_unresolved_child_ownership(child, "injected child.wait error").await;
    }
    tokio::select! {
        biased;
        status = child.wait() => match status {
            Ok(status) => {
                #[cfg(test)]
                process_group.observe_reap(status);
                Ok(OwnedChildExit::Exited(status))
            },
            Err(error) => {
                relinquish_process_group_after_leader_exit(process_group);
                hold_unresolved_child_ownership(child, error).await
            }
        },
        reason = process_control.termination_requested() => {
            match terminate_owned_child(child, process_group).await {
                Ok(_) => Ok(OwnedChildExit::Terminated(reason)),
                Err(error) => Ok(OwnedChildExit::TerminationFailed(error.to_string())),
            }
        }
    }
}

pub(super) enum ProcessWorkerOutcome {
    UnverifiedStartupFailure(crate::session_worker::startup_failure::WorkerOutput),
    VerifiedStartupFailure(WorkerResult),
    OperatorVerifiedFinished(process_terminal::operator::VerifiedOperatorWorkerCompletion),
    OperatorInterrupted {
        detail: String,
        usage: process_terminal::RecoveredWorkerUsage,
    },
    Finished(WorkerResult),
    VerifiedFinished(process_terminal::VerifiedWorkerCompletion),
    OrdinaryVerifiedFinished(process_terminal::ordinary::VerifiedOrdinaryWorkerCompletion),
    OrdinaryInterrupted {
        detail: String,
        usage: process_terminal::RecoveredWorkerUsage,
    },
    Terminated(String),
    TerminationFailed(String),
    /// The worker's stdout reached EOF without a `WorkerResult` line.
    ///
    /// This is the process-mode half of "the run died where the parent cannot
    /// see it": an OOM kill, a `SIGKILL`, or a segfault leaves the parent with
    /// nothing but a closed pipe. It is a distinct variant rather than a
    /// generic decode failure so the terminal status names the actual fault,
    /// and so that a run in this state can never be left `Running`.
    ExitedWithoutResult(String),
}

/// Human description of how a worker process ended, for the terminal status of
/// a run that produced no result.
pub(super) fn describe_worker_exit(status: std::process::ExitStatus, stderr: &str) -> String {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let how = match (status.code(), signal) {
        (_, Some(signal)) => format!("killed by signal {signal}"),
        (Some(code), None) => format!("exit status {code}"),
        (None, None) => "exit status unknown".to_string(),
    };
    if stderr.is_empty() {
        how
    } else {
        let preview = stderr.chars().rev().take(400).collect::<String>();
        let preview = preview.chars().rev().collect::<String>();
        format!("{how}; stderr: {preview}")
    }
}

#[derive(Debug)]
pub(super) struct OwnedProcessGroup {
    #[cfg(unix)]
    pub(super) pgid: i32,
    pub(super) armed: bool,
    #[cfg(test)]
    pub(super) termination_calls: Option<Arc<std::sync::atomic::AtomicUsize>>,
    #[cfg(test)]
    skip_signal: bool,
    #[cfg(test)]
    pub(super) protocol_diagnostic: Option<Arc<parking_lot::Mutex<TestWorkerProtocolDiagnostic>>>,
}

impl OwnedProcessGroup {
    pub(super) fn from_child(child: &tokio::process::Child) -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            let pgid = child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .filter(|pid| *pid > 0)
                .ok_or_else(|| anyhow::anyhow!("owned session-worker has no valid process group"))?;
            Ok(Self {
                pgid,
                armed: true,
                #[cfg(test)]
                termination_calls: None,
                #[cfg(test)]
                skip_signal: false,
                #[cfg(test)]
                protocol_diagnostic: None,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = child;
            Ok(Self {
                armed: true,
                #[cfg(test)]
                termination_calls: None,
                #[cfg(test)]
                skip_signal: false,
                #[cfg(test)]
                protocol_diagnostic: None,
            })
        }
    }

    #[allow(unsafe_code)]
    pub(super) fn terminate(&mut self) -> anyhow::Result<()> {
        if !self.armed {
            return Ok(());
        }
        #[cfg(test)]
        if let Some(calls) = &self.termination_calls {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.skip_signal {
                self.armed = false;
                return Ok(());
            }
        }
        #[cfg(unix)]
        {
            // SAFETY: `pgid` is captured from a child spawned with
            // `process_group(0)` and is never exposed outside its owner.
            let rc = unsafe { libc::killpg(self.pgid, libc::SIGKILL) };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(anyhow::anyhow!(
                        "failed to kill session-worker process group {}: {error}",
                        self.pgid
                    ));
                }
            }
        }
        self.armed = false;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn observe_reap(&self, status: std::process::ExitStatus) {
        if let Some(diagnostic) = &self.protocol_diagnostic {
            let mut diagnostic = diagnostic.lock();
            diagnostic.reap_confirmed = true;
            diagnostic.exit_code = status.code();
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt as _;
                diagnostic.exit_signal = status.signal();
            }
        }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }

    #[cfg(test)]
    pub(super) const fn test_stub(termination_calls: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Self {
            #[cfg(unix)]
            pgid: 1,
            armed: true,
            termination_calls: Some(termination_calls),
            skip_signal: true,
            protocol_diagnostic: None,
        }
    }

    #[cfg(test)]
    pub(super) fn observe_termination_calls(&mut self, termination_calls: Arc<std::sync::atomic::AtomicUsize>) {
        self.termination_calls = Some(termination_calls);
    }
}

/// Steering plumbing handed to the owner of a session-worker child process.
pub(super) struct WorkerSteerPipe {
    pub(super) steer_rx: tokio::sync::mpsc::Receiver<String>,
    pub(super) history: Arc<RwLock<Vec<HistoryEntry>>>,
    pub(super) run_id: String,
}

pub(super) enum ProcessOutputDrain {
    Finished { stdout: Vec<u8>, stderr: Vec<u8> },
    TerminationFailed(String),
}

pub(super) enum OwnedProcessPhase {
    Exited {
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Terminated(String),
    TerminationFailed(String),
}

pub(super) async fn drain_process_output_after_leader_exit(
    stdout_task: tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>,
    stderr_task: tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>,
    process_control: &ProcessRunControl,
    drain_timeout: std::time::Duration,
) -> anyhow::Result<ProcessOutputDrain> {
    let stdout_abort = stdout_task.abort_handle();
    let stderr_abort = stderr_task.abort_handle();
    let drain = async move {
        let stdout = stdout_task.await??;
        let stderr = stderr_task.await??;
        Ok::<_, anyhow::Error>(ProcessOutputDrain::Finished { stdout, stderr })
    };
    tokio::pin!(drain);

    tokio::select! {
        biased;
        output = &mut drain => output,
        reason = process_control.termination_requested() => {
            stdout_abort.abort();
            stderr_abort.abort();
            Ok(ProcessOutputDrain::TerminationFailed(format!(
                "termination requested after session-worker leader exit ({reason}); process group ownership was already relinquished"
            )))
        }
        () = tokio::time::sleep(drain_timeout) => {
            stdout_abort.abort();
            stderr_abort.abort();
            anyhow::bail!(
                "session-worker descendants held output pipes open after leader exit for {}s",
                drain_timeout.as_secs_f64()
            )
        }
    }
}

pub(super) const fn relinquish_process_group_after_leader_exit(process_group: &mut OwnedProcessGroup) {
    // Once wait() has reaped the leader, the numeric PGID is no longer a safe
    // capability: the group may be empty and the id may be reused immediately.
    process_group.disarm();
}

pub(super) async fn run_owned_child_phase(
    child: &mut tokio::process::Child,
    process_group: &mut OwnedProcessGroup,
    stdout_task: tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>,
    stderr_task: tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>,
    process_control: &ProcessRunControl,
) -> anyhow::Result<OwnedProcessPhase> {
    let process_exit = match wait_for_owned_process(
        child,
        process_group,
        process_control,
        #[cfg(test)]
        false,
    )
    .await
    {
        Ok(exit) => exit,
        Err(error) => {
            stdout_task.abort();
            stderr_task.abort();
            return Err(error);
        }
    };

    match process_exit {
        OwnedChildExit::Exited(status) => {
            relinquish_process_group_after_leader_exit(process_group);
            match drain_process_output_after_leader_exit(
                stdout_task,
                stderr_task,
                process_control,
                std::time::Duration::from_secs(PROCESS_OUTPUT_DRAIN_TIMEOUT_SECS),
            )
            .await?
            {
                ProcessOutputDrain::Finished { stdout, stderr } => {
                    Ok(OwnedProcessPhase::Exited { status, stdout, stderr })
                }
                ProcessOutputDrain::TerminationFailed(error) => Ok(OwnedProcessPhase::TerminationFailed(error)),
            }
        }
        OwnedChildExit::Terminated(reason) => {
            stdout_task.abort();
            stderr_task.abort();
            Ok(OwnedProcessPhase::Terminated(reason))
        }
        OwnedChildExit::TerminationFailed(error) => {
            stdout_task.abort();
            stderr_task.abort();
            Ok(OwnedProcessPhase::TerminationFailed(error))
        }
    }
}
