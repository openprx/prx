use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::config::{HookAction, bounded_timeout};
use super::context::MAX_CONTEXT_STDOUT_BYTES;
use super::envelope::identity_variables;
use super::hook_core::HookCore;
use super::event::{HookEvent, redacted_hook_payload};
use super::{MAX_PAYLOAD_BYTES, MAX_STDERR_BYTES, MAX_STDERR_CHARS};

/// A hook run that failed, with the class the context path reports
/// (`timeout`, `exit_status`, `stdout_over_limit`). Any other failure of a
/// run is an ordinary error and is classed as `spawn`.
#[derive(Debug)]
pub(super) struct HookRunError {
    class: &'static str,
    message: String,
}

impl HookRunError {
    fn fail(class: &'static str, message: String) -> anyhow::Error {
        Self { class, message }.into()
    }

    pub(super) const fn class(&self) -> &'static str {
        self.class
    }
}

impl std::fmt::Display for HookRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HookRunError {}

impl HookCore {
    /// Run a command action. Notify callers pass `capture_stdout: false`:
    /// nothing is read from its stdout. With `true` its stdout is read
    /// concurrently, at most `MAX_CONTEXT_STDOUT_BYTES + 1` bytes, and
    /// returned; over the cap is an error.
    pub(super) async fn run_action(
        &self,
        event: HookEvent,
        payload: &serde_json::Value,
        default_timeout_ms: u64,
        action: &HookAction,
        capture_stdout: bool,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let command = action.command.as_deref().unwrap_or_default().trim();
        if command.is_empty() {
            anyhow::bail!("hook command is empty");
        }
        let timeout_ms = bounded_timeout(action.timeout_ms.unwrap_or(default_timeout_ms))?;

        // `emit` redacted the payload; this redacts again for direct callers
        // and is a borrow when there is nothing left to redact.
        let payload_json = redacted_hook_payload(payload).to_string();
        if payload_json.len() > MAX_PAYLOAD_BYTES {
            anyhow::bail!(
                "hook payload is {} bytes; limit is {MAX_PAYLOAD_BYTES}",
                payload_json.len()
            );
        }
        // The directory the hook starts in: the `cwd` the action names (a
        // relative one is taken from the workspace), else the dedicated
        // directory beside config.toml, never PRX's own working directory.
        let working_dir = if let Some(cwd) = action.cwd.as_deref().map(str::trim).filter(|cwd| !cwd.is_empty()) {
            if Path::new(cwd).is_absolute() {
                PathBuf::from(cwd)
            } else {
                self.workspace_dir.join(cwd)
            }
        } else {
            let (_, launch_root) = self.workspace_policy();
            let Some(launch_root) = launch_root else {
                anyhow::bail!(
                    "workspace hooks have no launch directory: PRX starts them in `hooks/` beside config.toml, and \
                     this process has no configuration directory"
                );
            };
            let working_dir = crate::config::mcp_launch::launch_directory(&launch_root, "workspace-hooks", None)
                .map_err(|error| anyhow::anyhow!(error))?;
            crate::config::mcp_launch::prepare_launch_directory(&launch_root, &working_dir).await?;
            working_dir
        };
        // A command with a path separator is workspace content (a script in
        // the workspace): resolve it against the workspace, since the hook
        // no longer starts there. A bare name is looked up in `PATH`.
        let program = if command.contains(['/', '\\']) && !Path::new(command).is_absolute() {
            self.workspace_dir.join(command)
        } else {
            PathBuf::from(command)
        };

        // A cleared environment: the launch allowlist of PRX's environment
        // (so no provider key of PRX reaches the hook) plus the hook's own
        // `env`, which is passed as written.
        let (environment, dropped) = crate::config::mcp_launch::hook_launch_environment(&action.env);
        if dropped > 0 {
            tracing::warn!(
                event = event.as_str(),
                dropped,
                "hook environment entries holding a NUL byte were dropped"
            );
        }
        let mut cmd = Command::new(&program);
        cmd.args(&action.args);
        cmd.env_clear();
        cmd.envs(environment);
        cmd.current_dir(&working_dir);
        // Each event variable is set under its `PRX_HOOK_*` name and its
        // legacy `ZERO_HOOK_*` name, which stays for compatibility.
        cmd.env("ZERO_HOOK_EVENT", event.as_str());
        cmd.env("PRX_HOOK_EVENT", event.as_str());
        for (name, value) in identity_variables(payload) {
            cmd.env(name, value);
        }

        // Avoid execve argv+env size overflow for large payloads.
        const MAX_ENV_PAYLOAD_BYTES: usize = 8 * 1024;
        if payload_json.len() <= MAX_ENV_PAYLOAD_BYTES {
            cmd.env("ZERO_HOOK_PAYLOAD", &payload_json);
            cmd.env("PRX_HOOK_PAYLOAD", &payload_json);
        } else {
            cmd.env("ZERO_HOOK_PAYLOAD", "");
            cmd.env("ZERO_HOOK_PAYLOAD_TRUNCATED", "1");
            cmd.env("PRX_HOOK_PAYLOAD", "");
            cmd.env("PRX_HOOK_PAYLOAD_TRUNCATED", "1");
        }

        // NamedTempFile is created with restrictive permissions and removes itself on every exit.
        let mut payload_file = tempfile::Builder::new()
            .prefix("openprx-hook-payload-")
            .suffix(".json")
            .tempfile()?;
        payload_file.write_all(payload_json.as_bytes())?;
        payload_file.flush()?;
        cmd.env("ZERO_HOOK_PAYLOAD_FILE", payload_file.path());
        cmd.env("PRX_HOOK_PAYLOAD_FILE", payload_file.path());

        if action.stdin_json {
            cmd.stdin(std::process::Stdio::piped());
        }
        if capture_stdout {
            cmd.stdout(std::process::Stdio::piped());
        } else {
            cmd.stdout(std::process::Stdio::null());
        }
        cmd.stderr(std::process::Stdio::piped());

        let mut child = crate::runtime::shell_process::spawn_managed_shell_child(cmd)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

        // Stdout is `null` unless captured, so its reader then reads nothing.
        // A reader that reads past the cap signals the wait below, which ends
        // the group at once.
        let (overflow_tx, overflow_rx) = tokio::sync::oneshot::channel::<()>();
        let stdout = child.take_stdout();
        let mut stdout_reader = tokio::spawn(async move {
            let Some(stdout) = stdout else {
                return std::io::Result::Ok(Vec::new());
            };
            let mut bytes = Vec::new();
            stdout
                .take(MAX_CONTEXT_STDOUT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > MAX_CONTEXT_STDOUT_BYTES {
                let _ = overflow_tx.send(());
            }
            Ok(bytes)
        });
        // Resolves only on overflow; without one it stays pending.
        let overflow = async move {
            if overflow_rx.await.is_err() {
                std::future::pending::<()>().await;
            }
        };

        let stderr = child.take_stderr();
        let mut stderr_reader = tokio::spawn(async move {
            let Some(stderr) = stderr else {
                return std::io::Result::Ok(Vec::new());
            };
            let mut bytes = Vec::new();
            stderr.take(MAX_STDERR_BYTES + 1).read_to_end(&mut bytes).await?;
            Ok(bytes)
        });

        if action.stdin_json {
            if let Some(mut stdin) = child.take_stdin() {
                let write_result = tokio::time::timeout_at(deadline, stdin.write_all(payload_json.as_bytes())).await;
                match write_result {
                    Err(_) => {
                        let _ = child.terminate_and_reap().await;
                        settle_readers(&mut stdout_reader, &mut stderr_reader).await;
                        return Err(HookRunError::fail(
                            "timeout",
                            "hook timed out while writing stdin".to_string(),
                        ));
                    }
                    Ok(Err(error)) if error.kind() != std::io::ErrorKind::BrokenPipe => {
                        let _ = child.terminate_and_reap().await;
                        settle_readers(&mut stdout_reader, &mut stderr_reader).await;
                        return Err(error.into());
                    }
                    Ok(_) => {}
                }
            }
        }

        let waited = tokio::select! {
            biased;
            () = overflow => None,
            result = tokio::time::timeout_at(deadline, child.wait()) => Some(result),
        };
        let status = match waited {
            None => {
                // More than the cap was written: end the group now, not at the timeout.
                let _ = child.terminate_and_reap().await;
                settle_readers(&mut stdout_reader, &mut stderr_reader).await;
                return Err(over_limit());
            }
            Some(Ok(Ok(status))) => status,
            Some(Ok(Err(error))) => {
                let _ = child.terminate_and_reap().await;
                settle_readers(&mut stdout_reader, &mut stderr_reader).await;
                return Err(error.into());
            }
            Some(Err(_)) => {
                let _ = child.terminate_and_reap().await;
                settle_readers(&mut stdout_reader, &mut stderr_reader).await;
                return Err(HookRunError::fail(
                    "timeout",
                    format!("hook timed out after {timeout_ms} ms"),
                ));
            }
        };
        let stdout = match settle_reader(&mut stdout_reader).await {
            Some(bytes) => bytes?,
            None => {
                let _ = child.terminate_and_reap().await;
                settle_readers(&mut stdout_reader, &mut stderr_reader).await;
                return Err(HookRunError::fail(
                    "timeout",
                    format!("hook output did not close within {READER_SETTLE_MS} ms"),
                ));
            }
        };
        // Over the cap takes precedence over the exit status.
        if stdout.len() > MAX_CONTEXT_STDOUT_BYTES {
            return Err(over_limit());
        }
        let mut stderr = if capture_stdout {
            match settle_reader(&mut stderr_reader).await {
                Some(bytes) => bytes?,
                None => {
                    // A grandchild holds stderr open after the hook exited: end its
                    // group and go on without the excerpt.
                    let _ = child.terminate_and_reap().await;
                    Vec::new()
                }
            }
        } else {
            stderr_reader.await??
        };
        child.mark_complete();
        let stderr_truncated = stderr.len() > MAX_STDERR_BYTES as usize;
        stderr.truncate(MAX_STDERR_BYTES as usize);

        if !status.success() {
            let stderr = String::from_utf8_lossy(&stderr);
            let mut stderr = truncate_for_log(stderr.as_ref(), MAX_STDERR_CHARS);
            if stderr_truncated {
                stderr.push_str(" [truncated]");
            }
            return Err(HookRunError::fail(
                "exit_status",
                format!(
                    "hook exited with status {}{}",
                    status,
                    if stderr.is_empty() {
                        String::new()
                    } else {
                        format!(", stderr: {stderr}")
                    }
                ),
            ));
        }
        if !capture_stdout {
            return Ok(None);
        }
        Ok(Some(stdout))
    }
}

/// How long the output readers may take to finish once the hook has ended
/// or been killed. A grandchild that keeps a pipe open cannot hold a turn
/// past this.
const READER_SETTLE_MS: u64 = 1_000;

type OutputReader = tokio::task::JoinHandle<std::io::Result<Vec<u8>>>;

/// Wait up to `READER_SETTLE_MS` for one output reader. On expiry the
/// reader is aborted and `None` is returned.
async fn settle_reader(reader: &mut OutputReader) -> Option<std::io::Result<Vec<u8>>> {
    match tokio::time::timeout(Duration::from_millis(READER_SETTLE_MS), &mut *reader).await {
        Ok(joined) => Some(joined.unwrap_or_else(|error| Err(std::io::Error::other(error)))),
        Err(_) => {
            reader.abort();
            None
        }
    }
}

/// The error of a context run whose stdout passed the cap.
fn over_limit() -> anyhow::Error {
    HookRunError::fail(
        "stdout_over_limit",
        format!("hook stdout is over {MAX_CONTEXT_STDOUT_BYTES} bytes"),
    )
}

/// Settle both readers after a kill; their results are discarded.
async fn settle_readers(stdout: &mut OutputReader, stderr: &mut OutputReader) {
    let _ = settle_reader(stdout).await;
    let _ = settle_reader(stderr).await;
}

pub(super) fn truncate_for_log(input: &str, max_chars: usize) -> String {
    let char_count = input.chars().count();
    if char_count <= max_chars {
        return input.to_string();
    }
    let mut out = String::new();
    for c in input.chars().take(max_chars) {
        out.push(c);
    }
    out.push_str("...");
    out
}
