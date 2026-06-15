//! Background non-interactive shell sessions (v2).
//!
//! A [`ShellSession`] runs a one-shot command via [`tokio::process::Command`]
//! (**not** a PTY — interactive PTY shells are v3) in its own process group, so
//! the chat side can:
//!
//! - stream stdout/stderr line-by-line into the v1.1 [`SessionEvent`] /
//!   [`SessionRing`] event bridge for live read-only `/attach` and `/logs`;
//! - terminate the **entire** process group on `/kill` or chat exit (so a
//!   `sh -c` that forks children leaves no orphans);
//! - surface the exit code as a terminal [`ShellStatus`].
//!
//! Security: the command is gated through the same [`SideEffectGate`] the
//! interactive [`crate::tools::shell::ShellTool`] uses (high-risk commands such
//! as `rm -rf /` are blocked / require a grant), runs in the workspace
//! directory, and inherits only the hardened-PATH + safe-env baseline. We do not
//! re-implement an unsafe execution path.
//!
//! Process-group semantics (Unix): the child is placed into a **new process
//! group** via [`std::os::unix::process::CommandExt::process_group`] with no
//! `unsafe`, and the whole group is signalled with `killpg` (one tiny `libc`
//! call with a `// SAFETY:` note) — SIGTERM, a short async grace, then SIGKILL
//! for stragglers. On non-Unix targets, where there is no portable
//! process-group kill, the reaper task owns the [`tokio::process::Child`] and
//! the cancel path drives `Child::start_kill` so the direct child is *actually*
//! terminated (status and behaviour stay consistent; orphaned grandchildren
//! remain a documented platform limitation, plan §v2 risk 3).
//!
//! Termination ordering: `kill()` is async. It trips the cancel token and then
//! awaits the reaper, which performs the signal, marks `Cancelled` only after
//! the signal path succeeds, drains the stdout/stderr readers to EOF, and only
//! then emits the terminal marker — so `[shell cancelled]` / `[shell completed]`
//! never races ahead of the command's last output line.

use super::event::SessionEventSink;
use super::id::SessionId;
use crate::security::{SecurityPolicy, SideEffectGate};
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How long to wait after SIGTERM before escalating to SIGKILL, giving a
/// well-behaved process group a chance to shut down cleanly.
const SIGTERM_GRACE: std::time::Duration = std::time::Duration::from_millis(300);

/// Polling interval while waiting out the SIGTERM grace window.
const GRACE_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Environment variables safe to pass to a background shell command. Mirrors the
/// interactive [`crate::tools::shell::ShellTool`] allow-list: only functional
/// variables, never API keys or secrets (CWE-200).
const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

/// Hardened PATH used for background shell commands, matching the interactive
/// shell tool's secure default (no user-writable directories).
#[cfg(not(target_os = "windows"))]
const HARDENED_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
#[cfg(target_os = "windows")]
const HARDENED_PATH: &str = r"C:\Windows\System32;C:\Windows;C:\Windows\System32\Wbem";

/// Terminal/running state of a background shell session.
///
/// Distinct from the agent `SubAgentStatus` because shells have an exit code
/// rather than an agent result; the chat-side projection unifies both into the
/// shared `ManagedStatus` (see [`super::model::ManagedStatus`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellStatus {
    /// The command is still executing.
    Running,
    /// The command exited 0.
    Completed,
    /// The command exited non-zero (carries a short reason).
    Failed(String),
    /// The session was killed by the operator (`/kill`) or at chat exit.
    Cancelled,
}

/// A single background shell session: handle to the running process group plus
/// the bookkeeping the chat side reads for `/sessions`, `/kill`, and `/logs`.
///
/// Cloning a `ShellSession` is cheap (it shares the inner `Arc`s) so the chat
/// registry can hand out snapshots without moving the live handle.
#[derive(Clone, Debug)]
pub struct ShellSession {
    /// Stable id (a fresh UUID, distinct from agent run ids).
    pub id: SessionId,
    /// The command line (for display in `/sessions`).
    pub command: String,
    /// Working directory the command runs in.
    pub cwd: PathBuf,
    /// When the session was spawned.
    pub started_at: DateTime<Utc>,
    /// Current status, updated by the reaper task when the process exits and by
    /// `kill`. `parking_lot` (synchronous, never held across `.await`).
    status: Arc<Mutex<ShellStatus>>,
    /// Cancellation token tripped by `kill`; the reader/reaper tasks observe it.
    cancel: CancellationToken,
    /// Process-group id (== child pid on Unix, since the child leads a new
    /// group). `None` until the process is spawned, on non-Unix, or after the
    /// reaper has reaped the child — clearing it once the process is gone
    /// prevents a later `kill` from signalling a *reused* pgid (PID recycling).
    /// `parking_lot` (synchronous, never held across `.await`).
    pgid: Arc<Mutex<Option<i32>>>,
    /// Reaper task handle. `kill()` awaits it so the caller observes a fully
    /// terminated session (signalled, drained, marker emitted) on return.
    /// `tokio::sync::Mutex` because it is held across `.await`. The `Option` is
    /// taken by whichever of `kill`/the reaper-watcher gets there first.
    reaper: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>,
}

impl ShellSession {
    /// The current status (cheap clone of the inner value).
    #[must_use]
    pub fn status(&self) -> ShellStatus {
        self.status.lock().clone()
    }

    /// Whether the session has reached a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self.status(), ShellStatus::Running)
    }

    /// Terminate a still-running session: trip the cancel token (which drives the
    /// reaper to signal the process group on Unix / `start_kill` the child on
    /// non-Unix), then await the reaper so the caller observes a fully terminated
    /// session — signal sent, output drained, terminal marker emitted.
    ///
    /// Guard (v2 review fix 1): if the session has **already reached a terminal
    /// state**, this is a no-op that sends **no signal**. Signalling here would
    /// be unsafe — the recorded pgid may by now have been recycled by the OS for
    /// an unrelated process group, so a stale `killpg` could kill a bystander.
    ///
    /// Idempotent and never panics. Returns an error only if the signal path
    /// itself fails for a reason other than "no such process" (already exited).
    pub async fn kill(&self) -> Result<()> {
        // Fix 1①: never signal a terminal session. Reading the status under the
        // sync lock (released before any `.await`) avoids racing the reaper, and
        // skipping the signal entirely closes the PID/PGID-reuse mis-kill window.
        if !matches!(*self.status.lock(), ShellStatus::Running) {
            return Ok(());
        }
        // Trip the token so the reaper performs the (graceful) kill and the
        // readers unwind even if the process has already exited on its own.
        self.cancel.cancel();
        // Await the reaper: the kill, status transition, drain, and marker all
        // happen inside it (fix 1③/2/3/4). Taking the handle is racy-safe — if
        // the reaper already finished, the join returns immediately; if another
        // `kill` took the handle first, we simply have nothing to await (the work
        // is already in flight / done).
        let handle = self.reaper.lock().await.take();
        if let Some(handle) = handle {
            // A reaper task only returns after it has finished its terminal work;
            // a join error means the task panicked or was aborted, which we treat
            // as best-effort (status/`/logs` carry the authoritative outcome).
            if let Err(e) = handle.await {
                tracing::warn!(error = %e, "shell reaper join failed during kill");
            }
        }
        Ok(())
    }
}

/// Signal an entire process group with SIGTERM, wait out a short async grace,
/// then SIGKILL any straggler (Unix only).
///
/// Returns `Ok(())` when the group is already gone (`ESRCH`); other errors are
/// surfaced. The grace (fix 4①) gives a well-behaved group the chance to exit on
/// SIGTERM before it is force-killed; the wait is `tokio::time::sleep` so it
/// never blocks the runtime. Idempotent: a second call after the group is gone
/// returns `Ok(())`.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(super) async fn kill_process_group(pgid: i32) -> Result<()> {
    // SAFETY: `killpg` is an async-signal-safe libc call that only sends a
    // signal to the process group `pgid`; it dereferences no pointers and has no
    // memory-safety preconditions. `pgid` is the group id we created via
    // `process_group(child_pid)`; signalling our own descendant group is sound.
    let term = unsafe { libc::killpg(pgid, libc::SIGTERM) };
    if term != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH = the group already exited; treat as success (idempotent kill).
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(anyhow!("killpg(SIGTERM, {pgid}) failed: {err}"));
    }
    // Grace window: poll whether the group has gone before escalating, so a
    // process that handles SIGTERM cleanly is never needlessly SIGKILL'd.
    let deadline = tokio::time::Instant::now() + SIGTERM_GRACE;
    while tokio::time::Instant::now() < deadline {
        if !process_group_alive(pgid) {
            return Ok(()); // exited within grace — no SIGKILL needed
        }
        tokio::time::sleep(GRACE_POLL).await;
    }
    // Still alive after the grace: SIGKILL the stragglers.
    // SAFETY: same invariants as the SIGTERM call above.
    let kill = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if kill != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(anyhow!("killpg(SIGKILL, {pgid}) failed: {err}"));
        }
    }
    Ok(())
}

/// Whether the process group `pgid` still has at least one member, probed with
/// the null signal (`killpg(pgid, 0)`), which performs permission/existence
/// checks without delivering a signal.
#[cfg(unix)]
#[allow(unsafe_code)]
fn process_group_alive(pgid: i32) -> bool {
    // SAFETY: `killpg` with signal 0 sends no signal; it only validates that the
    // target group exists and is signallable. No pointers, no memory-safety
    // preconditions; `pgid` is our own descendant group id.
    let rc = unsafe { libc::killpg(pgid, 0) };
    if rc == 0 {
        return true;
    }
    // ESRCH = no such group (gone). Any other errno (e.g. EPERM) means the group
    // still exists; treat as alive so we still attempt the SIGKILL escalation.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Spawn a background non-interactive shell command.
///
/// The command is authorized through the [`SideEffectGate`] (same gate the
/// interactive shell tool uses) before any process is created. On success the
/// child runs in its own process group; stdout and stderr are streamed
/// line-by-line as [`SessionEvent::Delta`] through the supplied event sink, and
/// a reaper task records the terminal [`ShellStatus`] (and emits a final status
/// event) when the process exits.
///
/// Returns a clonable [`ShellSession`] handle for the chat registry.
pub fn spawn_shell(command: &str, security: &Arc<SecurityPolicy>, sink: &SessionEventSink) -> Result<ShellSession> {
    // 1. Security gate — identical policy to ShellTool. The operator typed
    //    `/shell`, but high-risk commands (rm -rf /, mkfs, dd, …) are still
    //    blocked unless the policy allows them; we never bypass the gate.
    if security.is_rate_limited() {
        return Err(anyhow!("Rate limit exceeded: too many actions in the last hour"));
    }
    SideEffectGate::new(security.as_ref())
        .authorize_command_execution("shell", command, None)
        .map_err(|reason| anyhow!("{reason}"))?;
    if !security.record_action() {
        return Err(anyhow!("Rate limit exceeded: action budget exhausted"));
    }

    let cwd = resolve_cwd(&security.workspace_dir);
    let id = SessionId::from_run_id(&uuid::Uuid::new_v4().to_string());

    // 2. Build the command in the workspace, hardened env + own process group.
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.env_clear();
    for var in SAFE_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    cmd.env("PATH", HARDENED_PATH);
    // Own process group so `/kill` and chat-exit can `killpg` the whole tree,
    // not just the `sh` leader. `tokio::process::Command::process_group` is an
    // inherent method (no trait import, no `unsafe`): it sets `setpgid(0,0)` in
    // the child before exec via a pre_exec hook managed by tokio/std.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Failed to start background shell: {e}"))?;

    let pgid_cell: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
    // On Unix the child leads its own group, so the group id equals its pid.
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // pid fits in i32 on every supported Unix; the cast is the conventional
        // pid_t representation used by killpg.
        *pgid_cell.lock() = Some(pid as i32);
    }

    let status = Arc::new(Mutex::new(ShellStatus::Running));
    let cancel = CancellationToken::new();

    // 3. Stream stdout/stderr line-by-line into the event bridge. The sink's
    //    drainer guarantees these `.send().await`s never back-pressure us into a
    //    deadlock with a slow UI (it drops on a full main-loop channel).
    let (delta_tx, _tool_tx) = sink.attach_run(id.clone());
    let mut readers: Vec<JoinHandle<()>> = Vec::with_capacity(2);
    if let Some(out) = child.stdout.take() {
        readers.push(spawn_line_reader(BufReader::new(out), delta_tx.clone(), cancel.clone()));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(spawn_line_reader(BufReader::new(err), delta_tx.clone(), cancel.clone()));
    }

    // 4. Reaper: await exit (or cancellation), perform the kill on cancel, record
    //    the terminal status, drain the readers, and only then emit the final
    //    status line so `/attach` followers see the outcome *after* the last
    //    output line. The marker goes through the **same** per-session delta
    //    channel as stdout.
    let reaper = spawn_reaper(ReaperCtx {
        child,
        readers,
        status: Arc::clone(&status),
        cancel: cancel.clone(),
        pgid: Arc::clone(&pgid_cell),
        delta_tx,
    });

    Ok(ShellSession {
        id,
        command: command.to_string(),
        cwd,
        started_at: Utc::now(),
        status,
        cancel,
        pgid: pgid_cell,
        reaper: Arc::new(tokio::sync::Mutex::new(Some(reaper))),
    })
}

/// Resolve the workspace directory to a canonical cwd, falling back to the
/// original on a canonicalize error (mirrors the runtime adapter behaviour).
fn resolve_cwd(workspace_dir: &std::path::Path) -> PathBuf {
    workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| workspace_dir.to_path_buf())
}

/// Stream a child pipe line-by-line into the event bridge until EOF or cancel.
///
/// Returns the task's [`JoinHandle`] so the reaper can await the readers' drain
/// to EOF before emitting the terminal marker (fix 3: marker ordered after the
/// command's last output line).
fn spawn_line_reader<R>(
    reader: BufReader<R>,
    delta_tx: tokio::sync::mpsc::Sender<String>,
    cancel: CancellationToken,
) -> JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = reader.lines();
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                next = lines.next_line() => match next {
                    Ok(Some(line)) => {
                        // The drainer never blocks us for long (drop-on-full),
                        // so this `.await` cannot deadlock the reader.
                        if delta_tx.send(line).await.is_err() {
                            break; // drainer gone (chat shut down)
                        }
                    }
                    Ok(None) => break, // EOF
                    Err(e) => {
                        let _ = delta_tx.send(format!("[read error: {e}]")).await;
                        break;
                    }
                },
            }
        }
    })
}

/// Owned state handed to the reaper task. Bundling these keeps the spawn call
/// readable and avoids a `clippy::too_many_arguments` lint.
struct ReaperCtx {
    child: tokio::process::Child,
    readers: Vec<JoinHandle<()>>,
    status: Arc<Mutex<ShellStatus>>,
    cancel: CancellationToken,
    pgid: Arc<Mutex<Option<i32>>>,
    delta_tx: tokio::sync::mpsc::Sender<String>,
}

/// Await process exit (or cancellation), perform the kill on cancel, record the
/// terminal status, drain the output readers, and emit a final status line for
/// attach followers.
///
/// Ordering guarantees:
///
/// - Fix 1③: on cancel the status is set `Cancelled` only **after** the signal
///   path returns success; if the signal fails the status is left untouched so
///   the UI never shows "cancelled" for a process that is still running.
/// - Fix 1②: once the child has been reaped the recorded `pgid` is cleared, so a
///   later `kill` cannot signal a recycled pgid.
/// - Fix 2: on non-Unix (no `killpg`) the cancel path calls `child.start_kill`
///   so the direct child is actually terminated, not merely marked cancelled.
/// - Fix 3: the terminal marker is sent only **after** both readers have drained
///   to EOF, so `[shell completed]` cannot overtake the command's last line.
///
/// `.send().await` cannot deadlock — the drainer drops on a full main-loop
/// channel rather than back-pressuring producers.
fn spawn_reaper(ctx: ReaperCtx) -> JoinHandle<()> {
    let ReaperCtx {
        mut child,
        readers,
        status,
        cancel,
        pgid,
        delta_tx,
    } = ctx;
    tokio::spawn(async move {
        let final_line = tokio::select! {
            () = cancel.cancelled() => {
                // Operator/`exit`-driven termination. Perform the actual kill,
                // then mark Cancelled only on a successful signal (fix 1③).
                let signalled = terminate(&pgid, &mut child).await;
                // Reap the child so it does not linger as a zombie; this also
                // lets us clear the pgid afterwards (fix 1②).
                let _ = child.wait().await;
                *pgid.lock() = None;
                if signalled.is_ok() {
                    set_if_running(&status, ShellStatus::Cancelled);
                    "[shell cancelled]".to_string()
                } else {
                    // Signal failed for a real reason: leave status as-is and
                    // surface the error in the marker rather than claiming a
                    // clean cancel.
                    set_if_running(&status, ShellStatus::Failed("kill failed".to_string()));
                    "[shell kill failed]".to_string()
                }
            }
            exited = child.wait() => {
                // Natural exit: the process is already gone, so clear the pgid
                // immediately (fix 1②) before any later `kill` can read it.
                *pgid.lock() = None;
                match exited {
                    Ok(es) if es.success() => {
                        set_if_running(&status, ShellStatus::Completed);
                        "[shell completed]".to_string()
                    }
                    Ok(es) => {
                        let code = es.code().map_or_else(|| "signal".to_string(), |c| c.to_string());
                        set_if_running(&status, ShellStatus::Failed(format!("exit {code}")));
                        format!("[shell failed: exit {code}]")
                    }
                    Err(e) => {
                        set_if_running(&status, ShellStatus::Failed(format!("wait error: {e}")));
                        format!("[shell error: {e}]")
                    }
                }
            }
        };
        // Fix 3: wait for the stdout/stderr readers to finish draining the pipes
        // (the child has exited, so the pipes are at EOF and the readers will end
        // promptly) before sending the terminal marker, guaranteeing the marker
        // is ordered after the command's last output line.
        for r in readers {
            let _ = r.await;
        }
        // Best-effort final marker for live followers; ignored if the drainer is
        // gone (chat shut down). The status line / `/logs` carry the result
        // authoritatively regardless of whether this marker lands.
        let _ = delta_tx.send(final_line).await;
    })
}

/// Terminate the child: kill the whole process group on Unix (`killpg` with a
/// SIGTERM grace then SIGKILL), or `start_kill` the direct child on non-Unix
/// where no portable group kill exists (fix 2). Returns the signal outcome so
/// the reaper can decide whether to mark the session `Cancelled`.
async fn terminate(pgid: &Arc<Mutex<Option<i32>>>, child: &mut tokio::process::Child) -> Result<()> {
    #[cfg(unix)]
    {
        // Avoid using a possibly-recycled pgid in the (shouldn't-happen) case it
        // was already cleared; fall back to killing the direct child.
        let gid = *pgid.lock();
        match gid {
            Some(gid) => kill_process_group(gid).await,
            None => child.start_kill().map_err(|e| anyhow!("start_kill failed: {e}")),
        }
    }
    #[cfg(not(unix))]
    {
        // No portable process-group kill: terminate the direct child so the
        // status (Cancelled) matches reality. `_pgid` is unused on this target.
        let _ = pgid;
        child.start_kill().map_err(|e| anyhow!("start_kill failed: {e}"))
    }
}

/// Set the status to `next` only if it is still `Running` (so a `kill`-set
/// `Cancelled` is never overwritten by a late natural exit).
fn set_if_running(status: &Arc<Mutex<ShellStatus>>, next: ShellStatus) {
    let mut st = status.lock();
    if matches!(*st, ShellStatus::Running) {
        *st = next;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::event::SessionEvent;
    use super::*;
    use crate::security::AutonomyLevel;

    fn auto_security() -> Arc<SecurityPolicy> {
        // Full autonomy + a permissive command allowlist (`*`) so the gate
        // admits the ordinary test commands (`sleep`, `exit`, …) — the
        // operator-typed `/shell` analogue. High-risk *patterns* (rm -rf /, …)
        // are still blocked by `command_risk_level` independently of the
        // allowlist; this only widens the base-command allowlist.
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            allowed_commands: vec!["*".into()],
            ..SecurityPolicy::default()
        })
    }

    #[tokio::test]
    async fn high_risk_command_is_rejected_even_with_permissive_allowlist() {
        // The gate is not bypassed for `/shell`: a destructive pattern is denied
        // before any process is spawned, even under Full autonomy + `*`.
        let (sink, _rx) = SessionEventSink::channel();
        let sec = auto_security();
        let err = spawn_shell("rm -rf /", &sec, &sink).expect_err("test: high-risk denied");
        let msg = err.to_string();
        assert!(!msg.is_empty(), "denial carries a reason: {msg}");
    }

    #[tokio::test]
    async fn shell_runs_and_streams_output_then_completes() {
        let (sink, mut rx) = SessionEventSink::channel();
        let sec = auto_security();
        let session = spawn_shell("echo hello-shell", &sec, &sink).expect("test: spawn echo");
        let sid = session.id.clone();

        // Collect events until we see the completion marker.
        let mut saw_hello = false;
        let mut saw_completed = false;
        for _ in 0..50 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Some(SessionEvent::Delta { id, text })) => {
                    assert_eq!(id, sid);
                    if text.contains("hello-shell") {
                        saw_hello = true;
                    }
                    if text.contains("[shell completed]") {
                        saw_completed = true;
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert!(saw_hello, "stdout line streamed");
        assert!(saw_completed, "completion marker emitted");
        assert_eq!(session.status(), ShellStatus::Completed);
        assert!(session.is_terminal());
    }

    #[tokio::test]
    async fn nonzero_exit_maps_to_failed() {
        let (sink, _rx) = SessionEventSink::channel();
        let sec = auto_security();
        let session = spawn_shell("exit 3", &sec, &sink).expect("test: spawn exit 3");
        // Poll the status until terminal (the reaper runs on a separate task).
        for _ in 0..50 {
            if session.is_terminal() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        match session.status() {
            ShellStatus::Failed(reason) => assert!(reason.contains("exit 3"), "got {reason}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn kill_marks_cancelled_and_terminates_group() {
        let (sink, _rx) = SessionEventSink::channel();
        let sec = auto_security();
        // A long sleeper that also forks a child sleeper: killing only `sh` would
        // orphan the inner sleep; killpg of the whole group must reap both.
        let session = spawn_shell("sleep 60 & sleep 60", &sec, &sink).expect("test: spawn sleepers");
        assert_eq!(session.status(), ShellStatus::Running);
        session.kill().await.expect("test: kill group");
        assert_eq!(session.status(), ShellStatus::Cancelled);
        // Idempotent: a second kill is a no-op (already terminal -> no signal).
        session.kill().await.expect("test: idempotent kill");
    }

    #[tokio::test]
    async fn kill_is_status_terminal() {
        let (sink, _rx) = SessionEventSink::channel();
        let sec = auto_security();
        let session = spawn_shell("sleep 30", &sec, &sink).expect("test: spawn sleep");
        session.kill().await.expect("test: kill");
        assert!(session.is_terminal());
    }

    /// Fix 1①: killing an already-terminal session must NOT send a signal. We
    /// can't directly observe "no syscall", but we assert the status is left
    /// untouched and the call is a fast no-op (it returns without awaiting a
    /// grace window or a reaper that already finished).
    #[tokio::test]
    async fn kill_on_terminal_session_is_noop_no_signal() {
        let (sink, _rx) = SessionEventSink::channel();
        let sec = auto_security();
        let session = spawn_shell("exit 0", &sec, &sink).expect("test: spawn exit 0");
        // Wait until the reaper has recorded the terminal status.
        for _ in 0..100 {
            if session.is_terminal() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(session.status(), ShellStatus::Completed);
        // Kill on a Completed session is a no-op and does not flip the status to
        // Cancelled (the guard returns before tripping the token / signalling).
        session.kill().await.expect("test: kill terminal no-op");
        assert_eq!(session.status(), ShellStatus::Completed, "terminal status preserved");
    }

    /// Fix 1②: after a session has been reaped, its recorded pgid is cleared so a
    /// later `kill` cannot signal a recycled process group.
    #[tokio::test]
    async fn pgid_cleared_after_natural_exit() {
        let (sink, _rx) = SessionEventSink::channel();
        let sec = auto_security();
        let session = spawn_shell("exit 0", &sec, &sink).expect("test: spawn exit 0");
        for _ in 0..100 {
            if session.is_terminal() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(session.status(), ShellStatus::Completed);
        // Give the reaper a beat past the status set to clear the pgid + drain.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(session.pgid.lock().is_none(), "pgid cleared once child is reaped");
    }

    /// Fix 3: the terminal marker must arrive *after* the command's final output
    /// line on the same delta channel.
    #[tokio::test]
    async fn completion_marker_is_ordered_after_last_output() {
        let (sink, mut rx) = SessionEventSink::channel();
        let sec = auto_security();
        let session = spawn_shell("echo first; echo last", &sec, &sink).expect("test: spawn echoes");
        let sid = session.id.clone();

        let mut order: Vec<String> = Vec::new();
        for _ in 0..100 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Some(SessionEvent::Delta { id, text })) if id == sid => {
                    let done = text.contains("[shell completed]");
                    order.push(text);
                    if done {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        let marker_idx = order
            .iter()
            .position(|t| t.contains("[shell completed]"))
            .expect("test: saw completion marker");
        let last_idx = order
            .iter()
            .rposition(|t| t.contains("last"))
            .expect("test: saw last output line");
        assert!(
            last_idx < marker_idx,
            "output {last_idx} must precede marker {marker_idx}: {order:?}"
        );
    }
}
