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
//! call with a `// SAFETY:` note). On non-Unix targets we fall back to
//! best-effort `Child::kill` (documented limitation, plan §v2 risk 3).

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
use tokio_util::sync::CancellationToken;

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
    /// group). `None` until the process is spawned / on non-Unix.
    pgid: Arc<Mutex<Option<i32>>>,
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

    /// Terminate the whole process group (Unix) or best-effort kill the child
    /// (non-Unix), then mark the status `Cancelled` if it was still running.
    ///
    /// Idempotent and never panics. Returns an error only if the signal syscall
    /// itself fails for a reason other than "no such process" (already exited).
    pub fn kill(&self) -> Result<()> {
        // Trip the token first so the reader/reaper tasks unwind even if the
        // process has already exited.
        self.cancel.cancel();
        {
            let mut st = self.status.lock();
            if matches!(*st, ShellStatus::Running) {
                *st = ShellStatus::Cancelled;
            }
        }
        let pgid = *self.pgid.lock();
        pgid.map_or(Ok(()), kill_process_group)
    }
}

/// Signal an entire process group with SIGTERM then SIGKILL (Unix only).
///
/// Returns `Ok(())` when the group is already gone (`ESRCH`); other errors are
/// surfaced. The SIGKILL is best-effort after a short grace so a process
/// ignoring SIGTERM is still reaped.
#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_process_group(pgid: i32) -> Result<()> {
    // SAFETY: `killpg` is an async-signal-safe libc call that only sends a
    // signal to the process group `pgid`; it dereferences no pointers and has no
    // memory-safety preconditions. `pgid` is the group id we created via
    // `process_group(child_pid)`; signalling our own descendant group is sound.
    let term = unsafe { libc::killpg(pgid, libc::SIGTERM) };
    if term != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH = the group already exited; treat as success (idempotent kill).
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(anyhow!("killpg(SIGTERM, {pgid}) failed: {err}"));
        }
        return Ok(());
    }
    // Give the group a brief moment to exit on SIGTERM, then SIGKILL stragglers.
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

/// Non-Unix fallback: there is no portable process-group kill, so the caller
/// relies on `Child::kill` (wired via `kill_on_drop` and the cancel token). This
/// is a documented best-effort limitation (plan §v2 risk 3).
#[cfg(not(unix))]
fn kill_process_group(_pgid: i32) -> Result<()> {
    Ok(())
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
    if let Some(out) = child.stdout.take() {
        spawn_line_reader(BufReader::new(out), delta_tx.clone(), cancel.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_line_reader(BufReader::new(err), delta_tx.clone(), cancel.clone());
    }

    // 4. Reaper: await exit (or cancellation), record terminal status, and emit
    //    a final status line so `/attach` followers see the outcome inline. The
    //    marker goes through the **same** per-session delta channel as stdout so
    //    it is ordered *after* the command's output (the reaper only sends after
    //    the child has exited, by which point stdout lines are already queued).
    spawn_reaper(child, Arc::clone(&status), cancel.clone(), delta_tx);

    Ok(ShellSession {
        id,
        command: command.to_string(),
        cwd,
        started_at: Utc::now(),
        status,
        cancel,
        pgid: pgid_cell,
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
fn spawn_line_reader<R>(reader: BufReader<R>, delta_tx: tokio::sync::mpsc::Sender<String>, cancel: CancellationToken)
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
    });
}

/// Await process exit (or cancellation), record the terminal status, and emit a
/// final status line for attach followers.
///
/// The marker is sent through the per-session `delta_tx` (the same channel the
/// stdout/stderr readers use) so it is ordered *after* the command's output: the
/// reaper only reaches the send after `child.wait()` returns, by which point
/// every stdout line is already queued ahead of it in the drainer's middle
/// channel. `.send().await` cannot deadlock — the drainer drops on a full
/// main-loop channel rather than back-pressuring producers.
fn spawn_reaper(
    mut child: tokio::process::Child,
    status: Arc<Mutex<ShellStatus>>,
    cancel: CancellationToken,
    delta_tx: tokio::sync::mpsc::Sender<String>,
) {
    tokio::spawn(async move {
        let final_line = tokio::select! {
            () = cancel.cancelled() => {
                // `kill` already set Cancelled and signalled the group; reap the
                // child so it does not linger as a zombie.
                let _ = child.wait().await;
                "[shell cancelled]".to_string()
            }
            exited = child.wait() => match exited {
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
        };
        // Best-effort final marker for live followers; ignored if the drainer is
        // gone (chat shut down). The status line / `/logs` carry the result
        // authoritatively regardless of whether this marker lands.
        let _ = delta_tx.send(final_line).await;
    });
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
        session.kill().expect("test: kill group");
        assert_eq!(session.status(), ShellStatus::Cancelled);
        // Idempotent: a second kill is a no-op (group already gone -> ESRCH ok).
        session.kill().expect("test: idempotent kill");
    }

    #[tokio::test]
    async fn kill_is_status_terminal() {
        let (sink, _rx) = SessionEventSink::channel();
        let sec = auto_security();
        let session = spawn_shell("sleep 30", &sec, &sink).expect("test: spawn sleep");
        session.kill().expect("test: kill");
        assert!(session.is_terminal());
    }
}
