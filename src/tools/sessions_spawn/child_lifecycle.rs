use super::{
    HistoryEntry, MAX_SUBPROCESS_OUTPUT, OwnedProcessGroup, OwnedProcessPhase, ProcessRunControl, ProcessWorkerOutcome,
    WorkerSteerPipe, hold_unresolved_child_ownership, process_terminal, relinquish_process_group_after_leader_exit,
    run_owned_child_phase, steering_instruction, terminate_owned_child,
};
#[cfg(test)]
use super::{TestWorkerProtocolCase, TestWorkerProtocolControl, TestWorkerProtocolTerminal};
use crate::memory::MemoryFabric;
use crate::session_worker::protocol::WorkerControlFrame;
#[cfg(test)]
use anyhow::Context as _;
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Aborts a background task when the owning scope unwinds.
///
/// The steer pump owns the child's `ChildStdin`; dropping this guard closes that
/// pipe, which is the worker's EOF signal that no further steering can arrive.
pub(super) struct AbortOnDrop(pub(super) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Forward steer messages to a running session-worker as line-delimited control
/// frames on its stdin, and mirror each delivered message into the run history.
///
/// Back-pressure, not loss, and deliberately without any wall-clock deadline: a
/// worker that is too busy to drain its stdin blocks this write, which fills the
/// bounded steer channel, which parks `sessions_send` — the same contract task
/// mode already has. A stuck target is the idle detector's problem, not a
/// timeout's.
pub(super) async fn pump_worker_steer_frames(
    mut stdin: tokio::process::ChildStdin,
    mut steer_rx: tokio::sync::mpsc::Receiver<String>,
    history: Arc<RwLock<Vec<HistoryEntry>>>,
    run_id: String,
) {
    use tokio::io::AsyncWriteExt;

    while let Some(message) = steer_rx.recv().await {
        let frame = match serde_json::to_string(&WorkerControlFrame::Steer {
            message: message.clone(),
        }) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!(run_id = %run_id, "failed to encode sub-agent steer frame: {error}");
                continue;
            }
        };
        let write = async {
            stdin.write_all(frame.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(error) = write.await {
            // Broken pipe: the worker exited. Stop pumping; the run's terminal
            // commit drops the sender so no caller can park on it afterwards.
            tracing::debug!(run_id = %run_id, "sub-agent steer pipe closed: {error}");
            return;
        }
        // Recorded only after the frame is on the wire, so history reflects what
        // was actually delivered rather than what was merely queued.
        history.write().await.push(HistoryEntry {
            role: "user".to_string(),
            content: steering_instruction(&message),
            timestamp: Utc::now(),
        });
    }
}

/// Read one of the worker's output pipes to EOF, stamping the run's progress
/// beat on every arrival.
///
/// Bytes from the worker are the only liveness signal that crosses the process
/// boundary, which is why this replaced a plain `read_to_end`: the buffered
/// result is byte-identical, but the parent now learns *while* the worker is
/// working rather than only once it is gone. The event is recorded as
/// [`crate::agent::idle::ProgressKind::ChannelOutput`] — output handed to a
/// sink — rather than as a new kind, so there is exactly one definition of what
/// counts as progress in this runtime.
pub(super) async fn drain_worker_stream<R>(
    stream: R,
    pipe: &'static str,
    progress: Option<Arc<crate::agent::idle::ProgressBeat>>,
) -> anyhow::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let mut limited = stream.take(MAX_SUBPROCESS_OUTPUT);
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut total: u64 = 0;
    loop {
        let read = limited.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if let Some(beat) = progress.as_ref() {
            beat.record(crate::agent::idle::ProgressKind::ChannelOutput);
        }
        match chunk.get(..read) {
            Some(bytes) => buffer.extend_from_slice(bytes),
            None => anyhow::bail!("session-worker {pipe} read reported more bytes than the buffer holds"),
        }
    }
    if total >= MAX_SUBPROCESS_OUTPUT {
        tracing::warn!(
            limit_bytes = MAX_SUBPROCESS_OUTPUT,
            pipe,
            "session-worker output reached size limit; output truncated"
        );
        buffer.extend_from_slice(b"\n[output truncated at 10MB]");
    }
    Ok(buffer)
}

pub(super) async fn run_spawned_child_lifecycle(
    child: &mut tokio::process::Child,
    process_group: &mut OwnedProcessGroup,
    payload: &str,
    process_control: &ProcessRunControl,
    steer: Option<WorkerSteerPipe>,
    progress: Option<Arc<crate::agent::idle::ProgressBeat>>,
    #[cfg(test)] panic_before_wait: bool,
    #[cfg(test)] protocol: Option<&TestWorkerProtocolControl>,
) -> anyhow::Result<OwnedProcessPhase> {
    use tokio::io::AsyncWriteExt;

    // The manifest is line 1 of the worker's stdin. Unlike before, the pipe is
    // then kept open for the life of the run so steer frames can follow it.
    #[cfg(test)]
    let mut held_stdin = None;
    #[cfg(test)]
    let _steer_pump = if let Some(control) = protocol {
        assert!(payload.len() < 3072, "fixture manifest line exceeds stdin precondition");
        let mut pipe = child.stdin.take().context("worker protocol stdin missing")?;
        pipe.write_all(payload.as_bytes()).await?;
        pipe.write_all(b"\n").await?;
        pipe.flush().await?;
        match control.case {
            TestWorkerProtocolCase::HoldAfterThreeSteers => {
                for message in ["first steer", "second steer", "third steer"] {
                    let frame = serde_json::to_string(&WorkerControlFrame::Steer {
                        message: message.to_owned(),
                    })?;
                    assert!(frame.len() < 256, "fixture steer line exceeds stdin precondition");
                    pipe.write_all(frame.as_bytes()).await?;
                    pipe.write_all(b"\n").await?;
                }
                pipe.flush().await?;
                held_stdin = Some(pipe);
            }
            TestWorkerProtocolCase::CloseAfterManifest => drop(pipe),
        }
        None
    } else if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        steer.map(|steer| {
            AbortOnDrop(tokio::spawn(pump_worker_steer_frames(
                stdin,
                steer.steer_rx,
                steer.history,
                steer.run_id,
            )))
        })
    } else {
        None
    };
    #[cfg(not(test))]
    let _steer_pump = if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        steer.map(|steer| {
            AbortOnDrop(tokio::spawn(pump_worker_steer_frames(
                stdin,
                steer.steer_rx,
                steer.history,
                steer.run_id,
            )))
        })
    } else {
        None
    };

    let stdout_stream = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("session-worker stdout pipe was not configured"))?;
    let stderr_stream = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("session-worker stderr pipe was not configured"))?;

    let stdout_task = tokio::spawn(drain_worker_stream(
        stdout_stream,
        "stdout",
        progress.as_ref().map(Arc::clone),
    ));
    let stderr_task = tokio::spawn(drain_worker_stream(
        stderr_stream,
        "stderr",
        progress.as_ref().map(Arc::clone),
    ));

    #[cfg(test)]
    assert!(!panic_before_wait, "injected owned-child lifecycle panic");

    #[cfg(not(test))]
    {
        run_owned_child_phase(child, process_group, stdout_task, stderr_task, process_control).await
    }
    #[cfg(test)]
    {
        let phase = run_owned_child_phase(child, process_group, stdout_task, stderr_task, process_control).await?;
        #[cfg(test)]
        if let (Some(control), OwnedProcessPhase::Exited { status, stdout, stderr }) = (protocol, &phase) {
            let mut terminal = control.terminal.lock();
            anyhow::ensure!(terminal.is_none(), "duplicate worker terminal observation");
            *terminal = Some(TestWorkerProtocolTerminal {
                status: *status,
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                stdin_held_through_reap: held_stdin.is_some(),
            });
        }
        #[cfg(test)]
        drop(held_stdin);
        Ok(phase)
    }
}

pub(super) async fn cleanup_owned_child_after_panic(
    child: &mut tokio::process::Child,
    process_group: &mut OwnedProcessGroup,
    #[cfg(test)] inject_try_wait_error: bool,
) -> anyhow::Result<()> {
    #[cfg(test)]
    if inject_try_wait_error {
        relinquish_process_group_after_leader_exit(process_group);
        return hold_unresolved_child_ownership(child, "injected panic-cleanup try_wait error").await;
    }
    match child.try_wait() {
        Ok(Some(status)) => {
            #[cfg(not(test))]
            let _ = status;
            #[cfg(test)]
            process_group.observe_reap(status);
            relinquish_process_group_after_leader_exit(process_group);
            Ok(())
        }
        Ok(None) => {
            terminate_owned_child(child, process_group).await?;
            Ok(())
        }
        Err(error) => {
            relinquish_process_group_after_leader_exit(process_group);
            hold_unresolved_child_ownership(child, error).await
        }
    }
}

pub(super) fn cleanup_process_worker_workspace(run_id: &str, worker_workspace: &std::path::Path, keep_workspace: bool) {
    if !keep_workspace {
        if let Err(error) = std::fs::remove_dir_all(worker_workspace) {
            tracing::warn!(
                run_id,
                "Failed to cleanup worker workspace {}: {error}",
                worker_workspace.display()
            );
        }
    }
}

pub(super) async fn recover_ordinary_interruption(
    fabric: &MemoryFabric,
    manifest: &crate::session_worker::protocol::WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    transport_key: &ring::hmac::Key,
    detail: String,
) -> anyhow::Result<ProcessWorkerOutcome> {
    if let Some(mut completion) =
        process_terminal::ordinary::recover_completion(fabric, manifest, generation, transport_key).await?
    {
        completion.confirm_reaped(&manifest.run_id)?;
        return Ok(ProcessWorkerOutcome::OrdinaryVerifiedFinished(completion));
    }
    let usage = process_terminal::ordinary::recover_partial_usage(fabric, manifest, generation, transport_key).await?;
    Ok(ProcessWorkerOutcome::OrdinaryInterrupted { detail, usage })
}

pub(super) async fn recover_operator_interruption(
    fabric: &MemoryFabric,
    manifest: &crate::session_worker::protocol::WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    transport_key: &ring::hmac::Key,
    detail: String,
) -> anyhow::Result<ProcessWorkerOutcome> {
    if let Some(mut completion) =
        process_terminal::operator::recover_completion(fabric, manifest, generation, transport_key).await?
    {
        completion.confirm_reaped(&manifest.run_id)?;
        return Ok(ProcessWorkerOutcome::OperatorVerifiedFinished(completion));
    }
    let usage = process_terminal::operator::recover_partial_usage(fabric, manifest, generation, transport_key).await?;
    Ok(ProcessWorkerOutcome::OperatorInterrupted { detail, usage })
}
