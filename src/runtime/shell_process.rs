//! Shared, policy-free direct shell process execution.
//!
//! Authorization and action accounting deliberately stay at each caller. This
//! adapter owns only runtime construction, caller-selected environment setup,
//! bounded output capture, and process-tree lifecycle management. It deliberately
//! does not inspect command text or apply filesystem/OS isolation.

use super::{RuntimeAdapter, create_runtime};
use crate::config::Config;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Baseline environment keys retained by the remote-node boundary and the
/// filtered generic shell tool. Other adapter callers retain their existing
/// inheritance policy unless they explicitly select an allowlist.
pub(crate) const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

/// Maximum retained bytes for each output stream. Readers continue draining
/// after this limit so a noisy child can never block on a full pipe.
pub const MAX_SHELL_OUTPUT_BYTES: usize = 1_048_576;
/// Stable suffix appended when a captured stream exceeds its retention limit.
pub const SHELL_OUTPUT_TRUNCATED_MARKER: &str = "\n... [output truncated at 1MiB]";
const OUTPUT_DRAIN_KILL_GRACE: Duration = Duration::from_secs(1);

/// A single shell execution request. Policy decisions are intentionally absent.
/// How a shell job's git is treated (GIT-ENV, D-107).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellGitPolicy {
    /// A model-driven surface (the `shell` tool, workspace skill scripts,
    /// and cron and Xin jobs, whose stores cannot tell a tool-created job
    /// from an operator's): a command string that runs git is refused.
    RefuseGit,
    /// An operator-configured job: git runs, with the environment hardening
    /// of [`crate::security::git_launch::operator_git_environment`].
    HardenGit,
}

pub struct ShellProcessRequest<'a> {
    pub command: &'a str,
    pub workspace_dir: &'a Path,
    /// Optional operation deadline. Normal PRX work passes `None`; tests and
    /// explicitly bounded internal probes may opt into a deadline.
    pub timeout: Option<Duration>,
    pub cancellation: Option<CancellationToken>,
    /// Whether a command string that runs git is refused or hardened.
    pub git: ShellGitPolicy,
}

/// Completed shell execution with independently bounded output streams.
pub struct ShellProcessOutcome {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Captured bytes remain available even when process execution is interrupted.
#[derive(Debug)]
pub struct ShellPartialOutput {
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub complete: bool,
}

/// Failures owned by the process adapter rather than caller policy.
#[derive(Debug, thiserror::Error)]
pub enum ShellProcessError {
    #[error("{reason}")]
    Interrupted {
        #[source]
        reason: Box<Self>,
        output: Box<ShellPartialOutput>,
    },
    #[error("failed to build runtime command: {0}")]
    Runtime(#[source] anyhow::Error),
    #[error("failed to spawn command: {0}")]
    Spawn(#[source] io::Error),
    #[error("failed while waiting for command: {0}")]
    Wait(#[source] io::Error),
    #[error("failed while capturing command output: {0}")]
    Output(#[source] io::Error),
    #[error("command timed out after {0:?}")]
    Timeout(Duration),
    #[error("command execution cancelled")]
    Cancelled,
    /// The command string runs git (GIT-ENV, D-106): git runs only through
    /// the hardened launch, which the shell cannot apply.
    #[error(
        "refusing to run git through the shell: use the git_operations tool, which disables the programs a \
         repository's configuration names"
    )]
    GitRefused,
}

impl ShellProcessError {
    fn without_output(self) -> Self {
        match self {
            Self::Interrupted { reason, .. } => reason.without_output(),
            error => error,
        }
    }
}

/// Reusable process adapter shared by interactive shell, cron, and Xin.
pub struct ShellProcessAdapter {
    runtime: Arc<dyn RuntimeAdapter>,
    environment_allowlist: Option<Vec<String>>,
    #[cfg(test)]
    active_output_readers: Arc<AtomicUsize>,
}

impl ShellProcessAdapter {
    pub fn new(runtime: Arc<dyn RuntimeAdapter>) -> Self {
        Self {
            runtime,
            environment_allowlist: None,
            #[cfg(test)]
            active_output_readers: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Clear the shell's inherited environment, keeping the baseline and these
    /// exact operator-configured names. Joined skill runs keep their stricter
    /// environment independently of this setting.
    pub fn with_environment_allowlist(mut self, names: Vec<String>) -> Self {
        self.environment_allowlist = Some(names);
        self
    }

    /// Construct a direct host-shell adapter from the configured runtime.
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let runtime: Arc<dyn RuntimeAdapter> = Arc::from(create_runtime(&config.runtime)?);
        Ok(Self::new(runtime))
    }

    pub async fn execute(&self, request: ShellProcessRequest<'_>) -> Result<ShellProcessOutcome, ShellProcessError> {
        self.execute_with(request, false)
            .await
            .map_err(ShellProcessError::without_output)
    }

    /// Retain partial output on interruption for the execution-service result.
    pub async fn execute_observed(
        &self,
        request: ShellProcessRequest<'_>,
    ) -> Result<ShellProcessOutcome, ShellProcessError> {
        self.execute_with(request, false).await
    }

    /// Like [`Self::execute`], for a run that must be fully contained:
    ///
    /// * the whole process group ends the moment the command's leader exits,
    ///   so nothing the command started outlives the call (and a child holding
    ///   the output pipes open cannot keep the call waiting). Cancellation and
    ///   timeout already tear the group down in both forms;
    /// * on the native runtime the command does not inherit the PRX
    ///   environment: it gets `PATH`, `LANG`, `LC_*` and `TZ` from the parent,
    ///   `HOME` set to the request's working directory and `TMPDIR` to a
    ///   directory inside it. No provider key, token or other credential
    ///   reaches it. A container runtime isolates the environment itself.
    ///
    /// Used where a lifecycle-managed skill script runs.
    pub async fn execute_joined(
        &self,
        request: ShellProcessRequest<'_>,
    ) -> Result<ShellProcessOutcome, ShellProcessError> {
        self.execute_with(request, true)
            .await
            .map_err(ShellProcessError::without_output)
    }

    async fn execute_with(
        &self,
        request: ShellProcessRequest<'_>,
        end_group_on_exit: bool,
    ) -> Result<ShellProcessOutcome, ShellProcessError> {
        if request.git == ShellGitPolicy::RefuseGit
            && crate::security::git_launch::shell_command_runs_git(request.command)
        {
            return Err(ShellProcessError::GitRefused);
        }
        // An operator job's git (also inside a script it runs) gets the
        // fixed settings through the environment.
        let git_environment = if request.git == ShellGitPolicy::HardenGit {
            crate::security::git_launch::operator_git_environment()
                .await
                .map_err(ShellProcessError::Runtime)?
        } else {
            Vec::new()
        };
        if request
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(ShellProcessError::Cancelled);
        }
        let filtered_environment = self
            .environment_allowlist
            .as_deref()
            .filter(|_| !end_group_on_exit)
            .map(filtered_shell_environment)
            .transpose()
            .map_err(ShellProcessError::Runtime)?;
        let mut shell_environment = Vec::new();
        if let Some(environment) = &filtered_environment {
            // A container keeps its image's PATH/HOME and other defaults.
            // Only explicit operator additions cross into it; the Docker
            // client itself still gets the cleared host baseline below.
            shell_environment.extend(
                environment
                    .iter()
                    .filter(|(key, _)| {
                        self.runtime.name() != "docker"
                            || self
                                .environment_allowlist
                                .as_ref()
                                .is_some_and(|names| names.iter().any(|name| key == std::ffi::OsStr::new(name)))
                    })
                    .cloned(),
            );
        }
        // Hardened Git settings win even over an explicitly inherited name.
        shell_environment.extend(git_environment.iter().cloned());
        let mut command = self
            .runtime
            .build_shell_command_with_env(request.command, request.workspace_dir, &shell_environment)
            .map_err(ShellProcessError::Runtime)?;
        if let Some(environment) = filtered_environment {
            command.env_clear();
            command.envs(environment);
            command.envs(git_environment.iter().cloned());
        }
        if end_group_on_exit && self.runtime.name() == "native" {
            command.env_clear();
            command.envs(contained_environment(request.workspace_dir).map_err(ShellProcessError::Runtime)?);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if request
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(ShellProcessError::Cancelled);
        }

        let mut process = spawn_managed_shell_child(command).map_err(ShellProcessError::Spawn)?;
        let captures = OutputCaptures::default();
        let stdout_task = spawn_output_drain(
            process.take_stdout(),
            Arc::clone(&captures.stdout),
            #[cfg(test)]
            Arc::clone(&self.active_output_readers),
        );
        let stderr_task = spawn_output_drain(
            process.take_stderr(),
            Arc::clone(&captures.stderr),
            #[cfg(test)]
            Arc::clone(&self.active_output_readers),
        );
        let mut drains = tokio::spawn(OutputDrainTasks::new(stdout_task, stderr_task).join());
        let mut drain_guard = DrainAbortGuard::new(drains.abort_handle());
        let deadline = wait_for_timeout(request.timeout);
        tokio::pin!(deadline);
        let cancellation = wait_for_cancellation(request.cancellation.as_ref());
        tokio::pin!(cancellation);

        let status = tokio::select! {
            biased;
            () = &mut cancellation => {
                let reaped = process.terminate_and_reap().await;
                let complete = finish_drains_after_kill(&mut drains).await;
                drain_guard.disarm();
                if reaped {
                    process.mark_complete();
                }
                return Err(captures.interrupted(ShellProcessError::Cancelled, complete));
            }
            timeout = &mut deadline => {
                let reaped = process.terminate_and_reap().await;
                let complete = finish_drains_after_kill(&mut drains).await;
                drain_guard.disarm();
                if reaped {
                    process.mark_complete();
                }
                return Err(captures.interrupted(ShellProcessError::Timeout(timeout), complete));
            }
            status = process.wait() => status.map_err(ShellProcessError::Wait)?,
        };
        if end_group_on_exit {
            // The leader was reaped by `wait`; the signal reaches whatever it
            // left running in its group. Known window: once the leader is
            // reaped and every member has exited, the group id could in
            // principle be reused by an unrelated process before this
            // `killpg`. The same window exists in the timeout and cancel
            // paths; it is a few microseconds after `wait` returns, and the
            // signal goes to the group id this adapter created.
            process.kill_process_tree();
        }

        enum DrainResult {
            Complete(Result<io::Result<()>, tokio::task::JoinError>),
            Cancelled,
            TimedOut(Duration),
        }
        let drain_result = tokio::select! {
            biased;
            () = &mut cancellation => DrainResult::Cancelled,
            timeout = &mut deadline => DrainResult::TimedOut(timeout),
            output = &mut drains => DrainResult::Complete(output),
        };
        match drain_result {
            DrainResult::Complete(output) => {
                drain_guard.disarm();
                let output = output.map_err(|error| ShellProcessError::Output(io::Error::other(error)))?;
                output.map_err(ShellProcessError::Output)?
            }
            DrainResult::Cancelled => {
                let reaped = process.terminate_and_reap().await;
                let complete = finish_drains_after_kill(&mut drains).await;
                drain_guard.disarm();
                if reaped {
                    process.mark_complete();
                }
                return Err(captures.interrupted(ShellProcessError::Cancelled, complete));
            }
            DrainResult::TimedOut(timeout) => {
                let reaped = process.terminate_and_reap().await;
                let complete = finish_drains_after_kill(&mut drains).await;
                drain_guard.disarm();
                if reaped {
                    process.mark_complete();
                }
                return Err(captures.interrupted(ShellProcessError::Timeout(timeout), complete));
            }
        };
        process.mark_complete();

        let (stdout, stdout_truncated) = captures.take_stdout().into_string();
        let (stderr, stderr_truncated) = captures.take_stderr().into_string();
        Ok(ShellProcessOutcome {
            status,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }

    #[cfg(test)]
    fn active_output_reader_count(&self) -> usize {
        self.active_output_readers.load(Ordering::SeqCst)
    }
}

/// Baseline needed by local command-line tools, without credential, loader or
/// interpreter-startup variables. Locale names are exact: `LC_*_TOKEN` is not
/// a locale setting. Extra names are an explicit operator grant.
fn filtered_shell_environment(extra: &[String]) -> anyhow::Result<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    crate::config::RuntimeConfig::validate_shell_env_allowlist(extra)?;
    let mut environment: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| {
            key.to_str().is_some_and(|key| {
                SAFE_ENV_VARS.contains(&key)
                    || matches!(
                        key,
                        "TZ" | "LC_COLLATE" | "LC_MESSAGES" | "LC_MONETARY" | "LC_NUMERIC" | "LC_TIME"
                    )
                    || extra.iter().any(|name| name == key)
            })
        })
        .collect();
    if !environment.iter().any(|(key, _)| key == "PATH") {
        environment.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
    }
    Ok(environment)
}

/// The only environment a contained run gets (see
/// [`ShellProcessAdapter::execute_joined`]).
fn contained_environment(home: &Path) -> anyhow::Result<Vec<(String, std::ffi::OsString)>> {
    let tmp = home.join(".tmp");
    match std::fs::create_dir(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => anyhow::bail!("cannot create {}: {e}", tmp.display()),
    }
    let mut environment: Vec<(String, std::ffi::OsString)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            (matches!(key.as_str(), "PATH" | "LANG" | "TZ") || key.starts_with("LC_")).then_some((key, value))
        })
        .collect();
    if !environment.iter().any(|(key, _)| key == "PATH") {
        environment.push(("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".into()));
    }
    environment.push(("HOME".to_string(), home.as_os_str().to_owned()));
    environment.push(("TMPDIR".to_string(), tmp.into_os_string()));
    Ok(environment)
}

async fn wait_for_cancellation(cancellation: Option<&CancellationToken>) {
    match cancellation {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

async fn wait_for_timeout(timeout: Option<Duration>) -> Duration {
    match timeout {
        Some(timeout) => {
            tokio::time::sleep(timeout).await;
            timeout
        }
        None => std::future::pending().await,
    }
}

#[derive(Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CapturedOutput {
    fn into_string(self) -> (String, bool) {
        let mut output = String::from_utf8_lossy(&self.bytes).into_owned();
        let truncated = self.truncated || output.len() > MAX_SHELL_OUTPUT_BYTES;
        if truncated {
            let retained = MAX_SHELL_OUTPUT_BYTES.saturating_sub(SHELL_OUTPUT_TRUNCATED_MARKER.len());
            output.truncate(output.floor_char_boundary(retained));
            output.push_str(SHELL_OUTPUT_TRUNCATED_MARKER);
        }
        (output, truncated)
    }
}

#[derive(Default)]
struct OutputCaptures {
    stdout: Arc<parking_lot::Mutex<CapturedOutput>>,
    stderr: Arc<parking_lot::Mutex<CapturedOutput>>,
}

impl OutputCaptures {
    fn take_stdout(&self) -> CapturedOutput {
        std::mem::take(&mut *self.stdout.lock())
    }
    fn take_stderr(&self) -> CapturedOutput {
        std::mem::take(&mut *self.stderr.lock())
    }

    fn interrupted(&self, reason: ShellProcessError, complete: bool) -> ShellProcessError {
        let (stdout, stdout_truncated) = self.take_stdout().into_string();
        let (stderr, stderr_truncated) = self.take_stderr().into_string();
        ShellProcessError::Interrupted {
            reason: Box::new(reason),
            output: Box::new(ShellPartialOutput {
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                complete,
            }),
        }
    }
}

fn spawn_output_drain<R>(
    stream: Option<R>,
    captured: Arc<parking_lot::Mutex<CapturedOutput>>,
    #[cfg(test)] active_readers: Arc<AtomicUsize>,
) -> JoinHandle<io::Result<()>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut stream) = stream else {
            return Ok(());
        };
        #[cfg(test)]
        let _active_reader = ActiveOutputReaderGuard::new(active_readers);
        let mut buffer = [0_u8; 8192];
        loop {
            let count = stream.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            // Only bounded byte copying holds the lock; no await or decoding.
            let mut captured = captured.lock();
            let remaining = MAX_SHELL_OUTPUT_BYTES.saturating_sub(captured.bytes.len());
            let retained = remaining.min(count);
            let bytes = buffer
                .get(..retained)
                .ok_or_else(|| io::Error::other("captured output slice exceeded read buffer"))?;
            captured.bytes.extend_from_slice(bytes);
            captured.truncated |= retained < count;
        }
        Ok(())
    })
}

#[cfg(test)]
struct ActiveOutputReaderGuard(Arc<AtomicUsize>);

#[cfg(test)]
impl ActiveOutputReaderGuard {
    fn new(active_readers: Arc<AtomicUsize>) -> Self {
        active_readers.fetch_add(1, Ordering::SeqCst);
        Self(active_readers)
    }
}

#[cfg(test)]
impl Drop for ActiveOutputReaderGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct OutputDrainTasks {
    stdout: Option<JoinHandle<io::Result<()>>>,
    stderr: Option<JoinHandle<io::Result<()>>>,
}

impl OutputDrainTasks {
    const fn new(stdout: JoinHandle<io::Result<()>>, stderr: JoinHandle<io::Result<()>>) -> Self {
        Self {
            stdout: Some(stdout),
            stderr: Some(stderr),
        }
    }

    async fn join(mut self) -> io::Result<()> {
        let stdout = self
            .stdout
            .as_mut()
            .ok_or_else(|| io::Error::other("stdout drain task already taken"))?;
        let stderr = self
            .stderr
            .as_mut()
            .ok_or_else(|| io::Error::other("stderr drain task already taken"))?;
        let output = tokio::try_join!(join_output_drain(stdout), join_output_drain(stderr));
        if output.is_ok() {
            self.stdout.take();
            self.stderr.take();
        }
        output.map(|_| ())
    }
}

impl Drop for OutputDrainTasks {
    fn drop(&mut self) {
        if let Some(task) = self.stdout.take() {
            task.abort();
        }
        if let Some(task) = self.stderr.take() {
            task.abort();
        }
    }
}

async fn join_output_drain(task: &mut JoinHandle<io::Result<()>>) -> io::Result<()> {
    task.await.map_err(io::Error::other)?
}

async fn finish_drains_after_kill(drains: &mut JoinHandle<io::Result<()>>) -> bool {
    match tokio::time::timeout(OUTPUT_DRAIN_KILL_GRACE, &mut *drains).await {
        Ok(Ok(Ok(()))) => true,
        Ok(_) => false,
        Err(_) => {
            drains.abort();
            let _ = drains.await;
            false
        }
    }
}

struct DrainAbortGuard {
    abort: tokio::task::AbortHandle,
    armed: bool,
}

impl DrainAbortGuard {
    const fn new(abort: tokio::task::AbortHandle) -> Self {
        Self { abort, armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DrainAbortGuard {
    fn drop(&mut self) {
        if self.armed {
            self.abort.abort();
        }
    }
}

pub(crate) struct ManagedShellChild {
    child: Option<Child>,
    #[cfg(unix)]
    pgid: Option<i32>,
    leader_reaped: bool,
    complete: bool,
    /// Registry row for this child. Dropped with the struct, so a child whose
    /// owner disappears without reaping stays visible as an orphan instead of
    /// silently vanishing from the ledger.
    registration: crate::runtime::registry::WorkGuard,
}

impl ManagedShellChild {
    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut().and_then(|child| child.stdout.take())
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut().and_then(|child| child.stdin.take())
    }

    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut().and_then(|child| child.stderr.take())
    }

    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("shell child already taken"))?;
        let status = child.wait().await?;
        self.leader_reaped = true;
        Ok(status)
    }

    pub(crate) async fn terminate_and_reap(&mut self) -> bool {
        self.kill_process_tree();
        if !self.leader_reaped {
            if let Some(child) = &mut self.child {
                let _ = child.start_kill();
                if child.wait().await.is_ok() {
                    self.leader_reaped = true;
                }
            }
        }
        self.leader_reaped
    }

    #[allow(unsafe_code)]
    fn kill_process_tree(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // SAFETY: `pgid` is the positive group id created for this child.
            // `killpg` sends a signal and does not dereference any pointer.
            let _ = unsafe { libc::killpg(pgid, libc::SIGKILL) };
            return;
        }
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }

    pub(crate) const fn mark_complete(&mut self) {
        self.complete = true;
        // The leader was waited on and the group torn down, so the ledger row
        // is retired rather than reported as an unreaped child.
        self.registration.mark_reaped();
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }
}

impl Drop for ManagedShellChild {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        self.kill_process_tree();
        let Some(mut child) = self.child.take() else {
            return;
        };
        if self.leader_reaped {
            return;
        }
        let _ = child.start_kill();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

/// Human-readable label for a command, used as the registry row's name.
///
/// Borrowed from the `Command` itself rather than passed in by callers, so
/// every managed spawn is labelled without any call site having to remember.
fn command_label(cmd: &tokio::process::Command) -> String {
    let std_cmd = cmd.as_std();
    let mut label = std_cmd.get_program().to_string_lossy().into_owned();
    for arg in std_cmd.get_args() {
        label.push(' ');
        label.push_str(&arg.to_string_lossy());
    }
    // Shell one-liners can be arbitrarily long; the registry is a listing, not
    // an audit log, so a bounded label keeps `prx tasks list` readable.
    const MAX_LABEL_BYTES: usize = 200;
    if label.len() > MAX_LABEL_BYTES {
        label.truncate(label.floor_char_boundary(MAX_LABEL_BYTES));
        label.push('…');
    }
    label
}

pub(crate) fn spawn_managed_shell_child(mut cmd: tokio::process::Command) -> io::Result<ManagedShellChild> {
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let label = command_label(&cmd);
    let child = cmd.spawn()?;
    #[cfg(unix)]
    let pgid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0);
    #[cfg(not(unix))]
    let pgid: Option<i32> = None;
    // Registered with the group id, not just the pid: a child that forks its
    // own workers can only be ended as a whole through `killpg`.
    let registration = crate::runtime::registry::register_process(&label, child.id(), pgid);

    Ok(ManagedShellChild {
        child: Some(child),
        #[cfg(unix)]
        pgid,
        leader_reaped: false,
        complete: false,
        registration,
    })
}

/// Run a command to completion and capture its output, with full process-tree
/// cleanup.
///
/// Semantically equivalent to `cmd.output().await`, but the child leads its own
/// process group and the whole group is killed when the returned future is
/// dropped. `tokio::process::Command::kill_on_drop` only reaches the direct
/// child, which is not enough for helpers that fork their own workers (`git`
/// spawning `git-remote-https`, `npm` spawning `node`, ...). Callers must not
/// preconfigure stdio: stdin is closed and both output streams are captured.
pub(crate) async fn run_managed_output(mut cmd: tokio::process::Command) -> io::Result<std::process::Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = spawn_managed_shell_child(cmd)?;
    let mut stdout_pipe = child.take_stdout();
    let mut stderr_pipe = child.take_stderr();

    let read_stdout = async {
        let mut buffer = Vec::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            pipe.read_to_end(&mut buffer).await?;
        }
        io::Result::Ok(buffer)
    };
    let read_stderr = async {
        let mut buffer = Vec::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            pipe.read_to_end(&mut buffer).await?;
        }
        io::Result::Ok(buffer)
    };

    let (stdout, stderr, status) = tokio::join!(read_stdout, read_stderr, child.wait());
    let status = status?;
    let stdout = stdout?;
    let stderr = stderr?;
    child.mark_complete();

    Ok(std::process::Output { status, stdout, stderr })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::NativeRuntime;
    use std::path::PathBuf;
    use tempfile::TempDir;

    struct EnvGuard {
        original: Option<String>,
    }

    impl EnvGuard {
        #[allow(unsafe_code)]
        fn secret(value: &str) -> Self {
            let original = std::env::var("PRX_TEST_SECRET").ok();
            // SAFETY: this test runs on a current-thread runtime and restores
            // the variable before returning.
            unsafe { std::env::set_var("PRX_TEST_SECRET", value) };
            Self { original }
        }
    }

    impl Drop for EnvGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: paired with the current-thread test mutation above.
            unsafe {
                if let Some(original) = &self.original {
                    std::env::set_var("PRX_TEST_SECRET", original);
                } else {
                    std::env::remove_var("PRX_TEST_SECRET");
                }
            }
        }
    }

    fn adapter() -> ShellProcessAdapter {
        ShellProcessAdapter::new(Arc::new(NativeRuntime::new()))
    }

    #[tokio::test]
    async fn filtered_docker_launch_clears_client_and_forwards_only_explicit_names() {
        struct InspectDocker {
            forwarded: parking_lot::Mutex<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
        }
        impl RuntimeAdapter for InspectDocker {
            fn name(&self) -> &str {
                "docker"
            }
            fn has_shell_access(&self) -> bool {
                true
            }
            fn has_filesystem_access(&self) -> bool {
                true
            }
            fn storage_path(&self) -> std::path::PathBuf {
                std::path::PathBuf::new()
            }
            fn supports_long_running(&self) -> bool {
                false
            }
            fn build_shell_command(&self, command: &str, dir: &Path) -> anyhow::Result<tokio::process::Command> {
                let mut command = NativeRuntime::new().build_shell_command(command, dir)?;
                command.env("PRX_RUNTIME_INTERNAL_TOKEN", "synthetic-secret");
                Ok(command)
            }
            fn build_shell_command_with_env(
                &self,
                command: &str,
                dir: &Path,
                env: &[(std::ffi::OsString, std::ffi::OsString)],
            ) -> anyhow::Result<tokio::process::Command> {
                *self.forwarded.lock() = env.to_vec();
                self.build_shell_command(command, dir)
            }
        }
        let temp = TempDir::new().unwrap();
        for names in [Vec::new(), vec!["PATH".to_string()]] {
            let runtime = Arc::new(InspectDocker {
                forwarded: parking_lot::Mutex::new(Vec::new()),
            });
            let outcome = ShellProcessAdapter::new(runtime.clone()).with_environment_allowlist(names.clone())
                .execute(ShellProcessRequest {
                    command: "test -n \"$PATH\" && test -z \"${PRX_RUNTIME_INTERNAL_TOKEN+x}\" && printf CLIENT_FILTERED",
                    workspace_dir: temp.path(), timeout: None, cancellation: None, git: ShellGitPolicy::RefuseGit,
                }).await.unwrap();
            assert!(outcome.status.success());
            assert_eq!(outcome.stdout, "CLIENT_FILTERED");
            let forwarded = runtime.forwarded.lock();
            assert_eq!(forwarded.len(), names.len());
            for (key, _) in forwarded.iter() {
                assert_eq!(key, "PATH", "only the explicitly allowed PATH is forwarded");
            }
        }
    }

    /// D-107: a model-driven surface's command string that runs git is
    /// refused before anything starts.
    #[tokio::test]
    async fn a_model_driven_shell_command_that_runs_git_is_refused() {
        let temp = TempDir::new().expect("temp dir");
        let marker = temp.path().join("started");
        for command in [
            format!("touch '{}'; git status", marker.display()),
            "cd . && GIT_DIR=x git log".to_string(),
            "sh -c 'git status'".to_string(),
        ] {
            let result = adapter()
                .execute(ShellProcessRequest {
                    command: &command,
                    workspace_dir: temp.path(),
                    timeout: None,
                    cancellation: None,
                    git: ShellGitPolicy::RefuseGit,
                })
                .await;
            assert!(matches!(result, Err(ShellProcessError::GitRefused)), "{command}");
        }
        assert!(!marker.exists(), "part of a refused command ran");
    }

    /// D-107: an operator-configured job's git runs, with the fixed settings
    /// and `GIT_NO_LAZY_FETCH` in its environment (a repository fsmonitor
    /// does not run).
    #[cfg(unix)]
    #[tokio::test]
    async fn an_operator_shell_job_runs_git_with_the_hardened_environment() {
        use std::os::unix::fs::PermissionsExt as _;
        crate::security::git_launch::hermetic_git_for_tests();
        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let marker = temp.path().join("fsmonitor-ran");
        let fsmonitor = temp.path().join("fsmonitor.sh");
        std::fs::write(&fsmonitor, format!("#!/bin/sh\necho ran > '{}'\n", marker.display())).expect("script");
        std::fs::set_permissions(&fsmonitor, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let setup = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?}");
        };
        setup(&["init", "-q"]);
        setup(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "one",
        ]);
        setup(&["config", "core.fsmonitor", &fsmonitor.display().to_string()]);
        let outcome = adapter()
            .execute(ShellProcessRequest {
                command: "git status --porcelain && git config --get core.hooksPath && printf \"$GIT_NO_LAZY_FETCH\"",
                workspace_dir: &repo,
                timeout: None,
                cancellation: None,
                git: ShellGitPolicy::HardenGit,
            })
            .await
            .expect("operator git runs");
        assert!(outcome.status.success(), "{}", outcome.stderr);
        assert!(outcome.stdout.contains("/dev/null"), "{}", outcome.stdout);
        assert!(outcome.stdout.trim_end().ends_with('1'), "{}", outcome.stdout);
        assert!(!marker.exists(), "the repository's fsmonitor ran in an operator job");
    }

    #[tokio::test]
    async fn a_joined_run_gets_only_the_contained_environment() {
        let temp = TempDir::new().expect("temp dir");
        fn request<'a>(command: &'a str, dir: &'a Path) -> ShellProcessRequest<'a> {
            ShellProcessRequest {
                command,
                workspace_dir: dir,
                timeout: None,
                cancellation: None,
                git: ShellGitPolicy::RefuseGit,
            }
        }
        // A plain run inherits the test process's variables; a joined one must
        // see none that is not on the allowlist.
        let plain = adapter().execute(request("env", temp.path())).await.expect("plain run");
        let foreign: Vec<String> = std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .filter(|key| {
                !(matches!(
                    key.as_str(),
                    "PATH" | "LANG" | "TZ" | "HOME" | "TMPDIR" | "PWD" | "OLDPWD" | "SHLVL" | "_"
                ) || key.starts_with("LC_"))
            })
            .collect();
        assert!(!foreign.is_empty(), "the test process has variables to leak");
        assert!(
            foreign.iter().any(|key| plain.stdout.contains(&format!("{key}="))),
            "the parent environment is inherited"
        );
        let joined = adapter()
            .execute_joined(request("env", temp.path()))
            .await
            .expect("joined run");
        let dir = std::fs::canonicalize(temp.path()).expect("canonical");
        for line in joined.stdout.lines() {
            let key = line.split('=').next().unwrap_or_default();
            assert!(
                matches!(
                    key,
                    "PATH" | "LANG" | "TZ" | "HOME" | "TMPDIR" | "PWD" | "OLDPWD" | "SHLVL" | "_"
                ) || key.starts_with("LC_"),
                "unexpected variable in a contained run: {key}"
            );
        }
        for key in &foreign {
            assert!(!joined.stdout.contains(&format!("{key}=")), "{key} leaked");
        }
        assert!(
            joined.stdout.contains(&format!("HOME={}", temp.path().display())),
            "{}",
            joined.stdout
        );
        assert!(
            joined
                .stdout
                .contains(&format!("TMPDIR={}", temp.path().join(".tmp").display()))
        );
        assert!(dir.join(".tmp").is_dir());
    }

    #[tokio::test]
    async fn request_without_deadline_runs_to_natural_completion() {
        let temp = TempDir::new().expect("temp dir");
        let outcome = adapter()
            .execute(ShellProcessRequest {
                command: "sleep 0.05; printf done",
                workspace_dir: temp.path(),
                timeout: None,
                cancellation: None,
                git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
            })
            .await
            .expect("unbounded shell execution");

        assert!(outcome.status.success());
        assert_eq!(outcome.stdout, "done");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn adapter_inherits_complete_parent_environment() {
        let _secret = EnvGuard::secret("must-not-leak");
        let temp = TempDir::new().expect("temp dir");
        let outcome = adapter()
            .execute(ShellProcessRequest {
                command: "printf '%s' \"$PRX_TEST_SECRET\"",
                workspace_dir: temp.path(),
                timeout: Some(Duration::from_secs(5)),
                cancellation: None,
                git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
            })
            .await
            .expect("shell execution");

        assert!(outcome.status.success());
        assert_eq!(outcome.stdout, "must-not-leak");
    }

    #[tokio::test]
    async fn adapter_continuously_drains_but_bounds_each_output_stream() {
        let temp = TempDir::new().expect("temp dir");
        let outcome = adapter()
            .execute(ShellProcessRequest {
                command: "yes x | head -c 1100000; yes y | head -c 1100000 >&2",
                workspace_dir: temp.path(),
                timeout: Some(Duration::from_secs(10)),
                cancellation: None,
                git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
            })
            .await
            .expect("large-output command");

        assert!(outcome.status.success());
        assert!(outcome.stdout.len() <= MAX_SHELL_OUTPUT_BYTES);
        assert!(outcome.stderr.len() <= MAX_SHELL_OUTPUT_BYTES);
        assert!(outcome.stdout.ends_with(SHELL_OUTPUT_TRUNCATED_MARKER));
        assert!(outcome.stderr.ends_with(SHELL_OUTPUT_TRUNCATED_MARKER));
    }

    #[tokio::test]
    async fn pre_cancelled_request_does_not_execute_marker_command() {
        let temp = TempDir::new().expect("temp dir");
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = adapter()
            .execute(ShellProcessRequest {
                command: "touch pre-cancelled-marker",
                workspace_dir: temp.path(),
                timeout: Some(Duration::from_secs(5)),
                cancellation: Some(cancellation),
                git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
            })
            .await;

        assert!(matches!(result, Err(ShellProcessError::Cancelled)));
        assert!(!temp.path().join("pre-cancelled-marker").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_after_leader_exit_drains_descendant_stderr_without_double_poll() {
        let temp = TempDir::new().expect("temp dir");
        let result = adapter()
            .execute(ShellProcessRequest {
                command: "sleep 30 >&2 & exit 0",
                workspace_dir: temp.path(),
                timeout: Some(Duration::from_millis(100)),
                cancellation: None,
                git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
            })
            .await;

        assert!(matches!(result, Err(ShellProcessError::Timeout(_))));
    }

    /// A detached (`setsid`) descendant that keeps stderr open must not keep
    /// the call waiting for EOF: once the call is ended, the output drains
    /// get the bounded kill grace and are then aborted.
    ///
    /// D-063: this used a 100 ms timeout started at spawn, so under load the
    /// deadline could fire before the child had called `setsid` and written
    /// its pid file (`setsid.pid: NotFound`). The call is now ended by
    /// cancellation, and only after the pid file exists, so the descendant
    /// is always detached first. The timeout and cancellation arms share the
    /// same kill-and-drain code. That the call did not wait for the
    /// descendant's EOF is shown by the descendant still being alive (and
    /// holding stderr) when the call returns, not by a wall-clock budget.
    #[cfg(unix)]
    #[tokio::test]
    async fn setsid_descendant_holding_stderr_cannot_exceed_drain_grace() {
        let temp = TempDir::new().expect("temp dir");
        let process = adapter();
        // macOS does not ship the util-linux `setsid` executable.  Its system
        // Python exposes the same POSIX syscall, so keep the test's detached
        // descendant contract without making the suite Linux-command dependent.
        // The pid file is written after `setsid`, atomically (rename), so a
        // reader never sees a partial pid.
        let detached = if cfg!(target_os = "macos") {
            "python3 -c 'import os; os.setsid(); open(\"setsid.tmp\", \"w\").write(str(os.getpid())); os.rename(\"setsid.tmp\", \"setsid.pid\"); os.execvp(\"sleep\", [\"sleep\", \"30\"])'"
        } else {
            "setsid sh -c 'echo $$ > setsid.tmp; mv setsid.tmp setsid.pid; exec sleep 30'"
        };
        let command = format!("{detached} >&2 & while [ ! -s setsid.pid ]; do :; done; exit 0");
        let cancellation = CancellationToken::new();
        let cancellation_watcher = cancellation.clone();
        let pid_file = temp.path().join("setsid.pid");
        let canceller = tokio::spawn(async move {
            let pid = wait_for_pid_file(&pid_file).await;
            cancellation_watcher.cancel();
            pid
        });
        let result = process
            .execute(ShellProcessRequest {
                command: &command,
                workspace_dir: temp.path(),
                timeout: None,
                cancellation: Some(cancellation),
                git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
            })
            .await;
        let pid = canceller.await.expect("canceller task");
        let pid = i32::try_from(pid).expect("pid fits pid_t");

        assert!(
            matches!(result, Err(ShellProcessError::Cancelled)),
            "{:?}",
            result.as_ref().err()
        );
        #[allow(unsafe_code)]
        // SAFETY: signal 0 only checks that the process exists; `pid` is the
        // positive pid the test's own detached child wrote.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(
            alive,
            "the call returned while the detached descendant still held stderr"
        );
        assert_eq!(
            process.active_output_reader_count(),
            0,
            "bounded return must leave no detached output-reader tasks"
        );
        #[allow(unsafe_code)]
        // SAFETY: the positive pgid was written by the test's freshly-created
        // setsid leader; killpg does not dereference pointers.
        unsafe {
            let _ = libc::killpg(pid, libc::SIGKILL);
        }
    }

    #[cfg(unix)]
    async fn wait_for_pid_file(path: &Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(raw) = tokio::fs::read_to_string(path).await {
                    if let Ok(pid) = raw.trim().parse() {
                        return pid;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("child pid file should appear")
    }

    #[cfg(unix)]
    async fn wait_until_process_gone(pid: u32) {
        let proc_path = PathBuf::from(format!("/proc/{pid}"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while proc_path.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("descendant process should be killed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_the_entire_process_group() {
        let temp = TempDir::new().expect("temp dir");
        let pid_file = temp.path().join("descendant.pid");
        let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let cancellation = CancellationToken::new();
        let cancellation_watcher = cancellation.clone();
        let pid_file_watcher = pid_file.clone();
        let canceller = tokio::spawn(async move {
            let pid = wait_for_pid_file(&pid_file_watcher).await;
            cancellation_watcher.cancel();
            pid
        });

        let result = adapter()
            .execute(ShellProcessRequest {
                command: &command,
                workspace_dir: temp.path(),
                timeout: Some(Duration::from_secs(10)),
                cancellation: Some(cancellation),
                git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
            })
            .await;
        let pid = canceller.await.expect("canceller task");

        assert!(matches!(result, Err(ShellProcessError::Cancelled)));
        wait_until_process_gone(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_after_leader_exit_drains_descendant_stderr_without_double_poll() {
        let temp = TempDir::new().expect("temp dir");
        let pid_file = temp.path().join("stderr-descendant.pid");
        let command = "sleep 30 >&2 & echo $! > stderr-descendant.pid; exit 0";
        let cancellation = CancellationToken::new();
        let cancellation_watcher = cancellation.clone();
        let pid_file_watcher = pid_file.clone();
        let canceller = tokio::spawn(async move {
            let pid = wait_for_pid_file(&pid_file_watcher).await;
            cancellation_watcher.cancel();
            pid
        });

        let result = adapter()
            .execute(ShellProcessRequest {
                command,
                workspace_dir: temp.path(),
                timeout: Some(Duration::from_secs(10)),
                cancellation: Some(cancellation),
                git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
            })
            .await;
        let pid = canceller.await.expect("canceller task");

        assert!(matches!(result, Err(ShellProcessError::Cancelled)));
        wait_until_process_gone(pid).await;
    }

    #[tokio::test]
    async fn run_managed_output_reports_stdout_stderr_and_status() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "echo out; echo err 1>&2; exit 3"]);

        let output = run_managed_output(command).await.expect("managed output");

        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "out");
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "err");
        assert_eq!(output.status.code(), Some(3));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_run_managed_output_kills_the_whole_process_group() {
        let temp = TempDir::new().expect("temp dir");
        let pid_file = temp.path().join("managed-output-descendant.pid");
        let script = format!("sleep 300 & echo $! > '{}'; wait", pid_file.display());
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", &script]);

        // The timeout is a test device for dropping the future, not a runtime
        // policy: it is what a cancelled task or an exiting parent would do.
        let execution = tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_millis(200), run_managed_output(command)).await;
        });
        let pid = wait_for_pid_file(&pid_file).await;
        execution.await.expect("execution task");

        wait_until_process_gone(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_on_drop_reaches_the_direct_child_when_the_future_is_dropped() {
        let temp = TempDir::new().expect("temp dir");
        let pid_file = temp.path().join("direct-child.pid");
        let script = format!("echo $$ > '{}'; sleep 300", pid_file.display());
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let execution = tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_millis(200), command.output()).await;
        });
        let pid = wait_for_pid_file(&pid_file).await;
        execution.await.expect("execution task");

        wait_until_process_gone(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_execution_future_kills_and_reaps_the_process_group() {
        let temp = TempDir::new().expect("temp dir");
        let workspace = temp.path().to_path_buf();
        let pid_file = temp.path().join("dropped-descendant.pid");
        let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let process = Arc::new(adapter());
        let execution = tokio::spawn(async move {
            process
                .execute(ShellProcessRequest {
                    command: &command,
                    workspace_dir: &workspace,
                    timeout: Some(Duration::from_mins(1)),
                    cancellation: None,
                    git: crate::runtime::shell_process::ShellGitPolicy::RefuseGit,
                })
                .await
        });

        let pid = wait_for_pid_file(&pid_file).await;
        execution.abort();
        let _ = execution.await;
        wait_until_process_gone(pid).await;
    }
    #[tokio::test]
    async fn observed_timeout_preserves_both_streams_and_marks_process_interruption() {
        let temp = tempfile::tempdir().unwrap();
        let result = adapter()
            .execute_observed(ShellProcessRequest {
                command: "printf timeout_stdout; printf timeout_stderr >&2; sleep 30",
                workspace_dir: temp.path(),
                timeout: Some(Duration::from_secs(1)),
                cancellation: None,
                git: ShellGitPolicy::RefuseGit,
            })
            .await;
        let Err(ShellProcessError::Interrupted { reason, output }) = result else {
            panic!("expected an observed process interruption");
        };
        assert!(matches!(*reason, ShellProcessError::Timeout(_)));
        assert_eq!(output.stdout, "timeout_stdout");
        assert_eq!(output.stderr, "timeout_stderr");
        assert!(output.complete);
        assert!(!output.stdout_truncated);
        assert!(!output.stderr_truncated);
    }
}
