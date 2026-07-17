//! Shared, policy-free shell process execution.
//!
//! Authorization and action accounting deliberately stay at each caller. This
//! adapter owns only runtime/sandbox construction, hardened process setup,
//! bounded output capture, and process-tree lifecycle management.

use super::{RuntimeAdapter, create_runtime};
use crate::config::Config;
use crate::security::traits::Sandbox;
use crate::security::{create_sandbox_with_workspace_and_dirs, resolve_extra_path_dirs};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Maximum retained bytes for each output stream. Readers continue draining
/// after this limit so a noisy child can never block on a full pipe.
pub const MAX_SHELL_OUTPUT_BYTES: usize = 1_048_576;
/// Stable suffix appended when a captured stream exceeds its retention limit.
pub const SHELL_OUTPUT_TRUNCATED_MARKER: &str = "\n... [output truncated at 1MiB]";
const OUTPUT_DRAIN_KILL_GRACE: Duration = Duration::from_secs(1);

pub(crate) const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

/// A single shell execution request. Policy decisions are intentionally absent.
pub struct ShellProcessRequest<'a> {
    pub command: &'a str,
    pub workspace_dir: &'a Path,
    pub timeout: Duration,
    pub cancellation: Option<CancellationToken>,
}

/// Completed shell execution with independently bounded output streams.
pub struct ShellProcessOutcome {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Failures owned by the process adapter rather than caller policy.
#[derive(Debug, thiserror::Error)]
pub enum ShellProcessError {
    #[error("failed to build runtime command: {0}")]
    Runtime(#[source] anyhow::Error),
    #[error("sandbox failed to wrap command: {0}")]
    Sandbox(#[source] io::Error),
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
}

/// Reusable process adapter shared by interactive shell, cron, and Xin.
pub struct ShellProcessAdapter {
    runtime: Arc<dyn RuntimeAdapter>,
    sandbox: Arc<dyn Sandbox>,
    extra_path_dirs: Vec<PathBuf>,
    #[cfg(test)]
    active_output_readers: Arc<AtomicUsize>,
}

impl ShellProcessAdapter {
    pub fn new(runtime: Arc<dyn RuntimeAdapter>, sandbox: Arc<dyn Sandbox>, extra_path_dirs: Vec<PathBuf>) -> Self {
        Self {
            runtime,
            sandbox,
            extra_path_dirs,
            #[cfg(test)]
            active_output_readers: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Construct runtime, sandbox grants, and PATH from one resolution of the
    /// configured extra executable directories.
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let extra_path_dirs = resolve_extra_path_dirs(&config.autonomy.sandbox.extra_path_dirs);
        let sandbox = create_sandbox_with_workspace_and_dirs(
            &config.autonomy.sandbox,
            Some(&config.workspace_dir),
            &extra_path_dirs,
        );
        let runtime: Arc<dyn RuntimeAdapter> = Arc::from(create_runtime(&config.runtime)?);
        Ok(Self::new(runtime, sandbox, extra_path_dirs))
    }

    pub async fn execute(&self, request: ShellProcessRequest<'_>) -> Result<ShellProcessOutcome, ShellProcessError> {
        if request
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(ShellProcessError::Cancelled);
        }
        let mut command = self
            .runtime
            .build_shell_command(request.command, request.workspace_dir)
            .map_err(ShellProcessError::Runtime)?;
        command.env_clear();
        for variable in SAFE_ENV_VARS {
            if let Ok(value) = std::env::var(variable) {
                command.env(variable, value);
            }
        }
        command.env("PATH", build_shell_path(&self.extra_path_dirs));
        self.sandbox
            .wrap_command(command.as_std_mut())
            .map_err(ShellProcessError::Sandbox)?;
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
        let stdout_task = spawn_output_drain(
            process.take_stdout(),
            #[cfg(test)]
            Arc::clone(&self.active_output_readers),
        );
        let stderr_task = spawn_output_drain(
            process.take_stderr(),
            #[cfg(test)]
            Arc::clone(&self.active_output_readers),
        );
        let mut drains = tokio::spawn(OutputDrainTasks::new(stdout_task, stderr_task).join());
        let mut drain_guard = DrainAbortGuard::new(drains.abort_handle());
        let deadline = tokio::time::sleep(request.timeout);
        tokio::pin!(deadline);
        let cancellation = wait_for_cancellation(request.cancellation.as_ref());
        tokio::pin!(cancellation);

        let status = tokio::select! {
            biased;
            () = &mut cancellation => {
                let reaped = process.terminate_and_reap().await;
                finish_drains_after_kill(&mut drains).await;
                drain_guard.disarm();
                if reaped {
                    process.mark_complete();
                }
                return Err(ShellProcessError::Cancelled);
            }
            () = &mut deadline => {
                let reaped = process.terminate_and_reap().await;
                finish_drains_after_kill(&mut drains).await;
                drain_guard.disarm();
                if reaped {
                    process.mark_complete();
                }
                return Err(ShellProcessError::Timeout(request.timeout));
            }
            status = process.wait() => status.map_err(ShellProcessError::Wait)?,
        };

        enum DrainResult {
            Complete(Result<io::Result<(CapturedOutput, CapturedOutput)>, tokio::task::JoinError>),
            Cancelled,
            TimedOut,
        }
        let drain_result = tokio::select! {
            biased;
            () = &mut cancellation => DrainResult::Cancelled,
            () = &mut deadline => DrainResult::TimedOut,
            output = &mut drains => DrainResult::Complete(output),
        };
        let (stdout, stderr) = match drain_result {
            DrainResult::Complete(output) => {
                drain_guard.disarm();
                let output = output.map_err(|error| ShellProcessError::Output(io::Error::other(error)))?;
                output.map_err(ShellProcessError::Output)?
            }
            DrainResult::Cancelled => {
                let reaped = process.terminate_and_reap().await;
                finish_drains_after_kill(&mut drains).await;
                drain_guard.disarm();
                if reaped {
                    process.mark_complete();
                }
                return Err(ShellProcessError::Cancelled);
            }
            DrainResult::TimedOut => {
                let reaped = process.terminate_and_reap().await;
                finish_drains_after_kill(&mut drains).await;
                drain_guard.disarm();
                if reaped {
                    process.mark_complete();
                }
                return Err(ShellProcessError::Timeout(request.timeout));
            }
        };
        process.mark_complete();

        let (stdout, stdout_truncated) = stdout.into_string();
        let (stderr, stderr_truncated) = stderr.into_string();
        Ok(ShellProcessOutcome {
            status,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }

    #[cfg(test)]
    pub(crate) fn shell_path(&self) -> String {
        build_shell_path(&self.extra_path_dirs)
    }

    #[cfg(test)]
    fn active_output_reader_count(&self) -> usize {
        self.active_output_readers.load(Ordering::SeqCst)
    }
}

async fn wait_for_cancellation(cancellation: Option<&CancellationToken>) {
    match cancellation {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

fn build_shell_path(extra_path_dirs: &[PathBuf]) -> String {
    let base = if cfg!(target_os = "windows") {
        r"C:\Windows\System32;C:\Windows;C:\Windows\System32\Wbem"
    } else {
        "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    };
    if extra_path_dirs.is_empty() {
        return base.to_string();
    }
    let separator = if cfg!(target_os = "windows") { ';' } else { ':' };
    let mut path = String::new();
    for directory in extra_path_dirs {
        path.push_str(&directory.to_string_lossy());
        path.push(separator);
    }
    path.push_str(base);
    path
}

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

fn spawn_output_drain<R>(
    stream: Option<R>,
    #[cfg(test)] active_readers: Arc<AtomicUsize>,
) -> JoinHandle<io::Result<CapturedOutput>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut stream) = stream else {
            return Ok(CapturedOutput {
                bytes: Vec::new(),
                truncated: false,
            });
        };
        #[cfg(test)]
        let _active_reader = ActiveOutputReaderGuard::new(active_readers);
        let mut bytes = Vec::with_capacity(MAX_SHELL_OUTPUT_BYTES);
        let mut buffer = [0_u8; 8192];
        let mut truncated = false;
        loop {
            let count = stream.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            let remaining = MAX_SHELL_OUTPUT_BYTES.saturating_sub(bytes.len());
            let retained = remaining.min(count);
            let retained_bytes = buffer
                .get(..retained)
                .ok_or_else(|| io::Error::other("captured output slice exceeded read buffer"))?;
            bytes.extend_from_slice(retained_bytes);
            truncated |= retained < count;
        }
        Ok(CapturedOutput { bytes, truncated })
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
    stdout: Option<JoinHandle<io::Result<CapturedOutput>>>,
    stderr: Option<JoinHandle<io::Result<CapturedOutput>>>,
}

impl OutputDrainTasks {
    const fn new(
        stdout: JoinHandle<io::Result<CapturedOutput>>,
        stderr: JoinHandle<io::Result<CapturedOutput>>,
    ) -> Self {
        Self {
            stdout: Some(stdout),
            stderr: Some(stderr),
        }
    }

    async fn join(mut self) -> io::Result<(CapturedOutput, CapturedOutput)> {
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
        output
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

async fn join_output_drain(task: &mut JoinHandle<io::Result<CapturedOutput>>) -> io::Result<CapturedOutput> {
    task.await.map_err(io::Error::other)?
}

async fn finish_drains_after_kill(drains: &mut JoinHandle<io::Result<(CapturedOutput, CapturedOutput)>>) {
    if tokio::time::timeout(OUTPUT_DRAIN_KILL_GRACE, &mut *drains)
        .await
        .is_err()
    {
        drains.abort();
        let _ = drains.await;
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

pub(crate) fn spawn_managed_shell_child(mut cmd: tokio::process::Command) -> io::Result<ManagedShellChild> {
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let child = cmd.spawn()?;
    #[cfg(unix)]
    let pgid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0);

    Ok(ManagedShellChild {
        child: Some(child),
        #[cfg(unix)]
        pgid,
        leader_reaped: false,
        complete: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::NativeRuntime;
    use crate::security::traits::NoopSandbox;
    use std::process::Command;
    use tempfile::TempDir;

    struct MarkerSandbox;

    impl Sandbox for MarkerSandbox {
        fn wrap_command(&self, command: &mut Command) -> io::Result<()> {
            command.env("PRX_TEST_SANDBOX_APPLIED", "yes");
            Ok(())
        }

        fn is_available(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "marker"
        }

        fn description(&self) -> &str {
            "test sandbox marker"
        }
    }

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

    fn adapter(sandbox: Arc<dyn Sandbox>) -> ShellProcessAdapter {
        ShellProcessAdapter::new(Arc::new(NativeRuntime::new()), sandbox, Vec::new())
    }

    #[test]
    fn hardened_path_has_no_empty_component_and_preserves_order() {
        let extra = vec![PathBuf::from("/trusted/one"), PathBuf::from("/trusted/two")];
        let path = build_shell_path(&extra);
        if cfg!(target_os = "windows") {
            assert!(
                !path.contains(";;"),
                "PATH must not inject the current directory: {path}"
            );
        } else {
            assert!(
                !path.contains("::"),
                "PATH must not inject the current directory: {path}"
            );
            assert!(path.starts_with("/trusted/one:/trusted/two:"), "{path}");
            assert!(path.ends_with("/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"), "{path}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn adapter_applies_sandbox_and_clears_secret_environment() {
        let _secret = EnvGuard::secret("must-not-leak");
        let temp = TempDir::new().expect("temp dir");
        let outcome = adapter(Arc::new(MarkerSandbox))
            .execute(ShellProcessRequest {
                command: "printf '%s|%s' \"$PRX_TEST_SANDBOX_APPLIED\" \"$PRX_TEST_SECRET\"",
                workspace_dir: temp.path(),
                timeout: Duration::from_secs(5),
                cancellation: None,
            })
            .await
            .expect("shell execution");

        assert!(outcome.status.success());
        assert_eq!(outcome.stdout, "yes|");
    }

    #[tokio::test]
    async fn adapter_continuously_drains_but_bounds_each_output_stream() {
        let temp = TempDir::new().expect("temp dir");
        let outcome = adapter(Arc::new(NoopSandbox))
            .execute(ShellProcessRequest {
                command: "yes x | head -c 1100000; yes y | head -c 1100000 >&2",
                workspace_dir: temp.path(),
                timeout: Duration::from_secs(10),
                cancellation: None,
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
        let result = adapter(Arc::new(NoopSandbox))
            .execute(ShellProcessRequest {
                command: "touch pre-cancelled-marker",
                workspace_dir: temp.path(),
                timeout: Duration::from_secs(5),
                cancellation: Some(cancellation),
            })
            .await;

        assert!(matches!(result, Err(ShellProcessError::Cancelled)));
        assert!(!temp.path().join("pre-cancelled-marker").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_after_leader_exit_drains_descendant_stderr_without_double_poll() {
        let temp = TempDir::new().expect("temp dir");
        let result = adapter(Arc::new(NoopSandbox))
            .execute(ShellProcessRequest {
                command: "sleep 30 >&2 & exit 0",
                workspace_dir: temp.path(),
                timeout: Duration::from_millis(100),
                cancellation: None,
            })
            .await;

        assert!(matches!(result, Err(ShellProcessError::Timeout(_))));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn setsid_descendant_holding_stderr_cannot_exceed_drain_grace() {
        let temp = TempDir::new().expect("temp dir");
        let process = adapter(Arc::new(NoopSandbox));
        let started = tokio::time::Instant::now();
        let result = process
            .execute(ShellProcessRequest {
                command: "setsid sh -c 'echo $$ > setsid.pid; exec sleep 30' >&2 & while [ ! -s setsid.pid ]; do :; done; exit 0",
                workspace_dir: temp.path(),
                timeout: Duration::from_millis(100),
                cancellation: None,
            })
            .await;

        assert!(matches!(result, Err(ShellProcessError::Timeout(_))));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "drain grace must remain bounded"
        );
        assert_eq!(
            process.active_output_reader_count(),
            0,
            "bounded return must leave no detached output-reader tasks"
        );
        let pid: i32 = std::fs::read_to_string(temp.path().join("setsid.pid"))
            .expect("setsid pid")
            .trim()
            .parse()
            .expect("numeric pid");
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

        let result = adapter(Arc::new(NoopSandbox))
            .execute(ShellProcessRequest {
                command: &command,
                workspace_dir: temp.path(),
                timeout: Duration::from_secs(10),
                cancellation: Some(cancellation),
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

        let result = adapter(Arc::new(NoopSandbox))
            .execute(ShellProcessRequest {
                command,
                workspace_dir: temp.path(),
                timeout: Duration::from_secs(10),
                cancellation: Some(cancellation),
            })
            .await;
        let pid = canceller.await.expect("canceller task");

        assert!(matches!(result, Err(ShellProcessError::Cancelled)));
        wait_until_process_gone(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_execution_future_kills_and_reaps_the_process_group() {
        let temp = TempDir::new().expect("temp dir");
        let workspace = temp.path().to_path_buf();
        let pid_file = temp.path().join("dropped-descendant.pid");
        let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let process = Arc::new(adapter(Arc::new(NoopSandbox)));
        let execution = tokio::spawn(async move {
            process
                .execute(ShellProcessRequest {
                    command: &command,
                    workspace_dir: &workspace,
                    timeout: Duration::from_secs(60),
                    cancellation: None,
                })
                .await
        });

        let pid = wait_for_pid_file(&pid_file).await;
        execution.abort();
        let _ = execution.await;
        wait_until_process_gone(pid).await;
    }
}
