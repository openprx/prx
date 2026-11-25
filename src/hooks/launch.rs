use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::config::{HookAction, bounded_timeout};
use super::hook_core::HookCore;
use super::event::{HookEvent, redacted_hook_payload};
use super::{MAX_PAYLOAD_BYTES, MAX_STDERR_BYTES, MAX_STDERR_CHARS};

impl HookCore {
    pub(super) async fn run_action(
        &self,
        event: HookEvent,
        payload: &serde_json::Value,
        default_timeout_ms: u64,
        action: &HookAction,
    ) -> anyhow::Result<()> {
        let command = action.command.trim();
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
        // The dedicated directory the hook starts in: beside config.toml,
        // never the workspace (which the agent's file tools can write) and
        // never PRX's own working directory (HOOKS-ENV). A `cwd` in
        // hooks.json is workspace-controlled and is not used.
        let (_, launch_root) = self.workspace_policy();
        let Some(launch_root) = launch_root else {
            anyhow::bail!(
                "workspace hooks have no launch directory: PRX starts them in `hooks/` beside config.toml, and this \
                 process has no configuration directory"
            );
        };
        let working_dir = crate::config::mcp_launch::launch_directory(&launch_root, "workspace-hooks", None)
            .map_err(|error| anyhow::anyhow!(error))?;
        crate::config::mcp_launch::prepare_launch_directory(&launch_root, &working_dir).await?;
        // A command with a path separator is workspace content (a script in
        // the workspace): resolve it against the workspace, since the hook
        // no longer starts there. A bare name is looked up in `PATH`.
        let program = if command.contains(['/', '\\']) && !Path::new(command).is_absolute() {
            self.workspace_dir.join(command)
        } else {
            PathBuf::from(command)
        };

        // A cleared environment through the shared launch builder: the
        // launch allowlist of PRX's environment plus the hook's own `env`,
        // without the variables a workspace may not set (HOOKS-ENV, D-045).
        let (environment, dropped) = crate::config::mcp_launch::workspace_launch_environment(&action.env);
        if !dropped.is_empty() {
            tracing::warn!(
                event = event.as_str(),
                dropped = ?dropped,
                "hook environment variables a workspace may not set were dropped"
            );
        }
        let mut cmd = Command::new(&program);
        cmd.args(&action.args);
        cmd.env_clear();
        cmd.envs(environment);
        cmd.current_dir(&working_dir);
        cmd.env("ZERO_HOOK_EVENT", event.as_str());

        // Avoid execve argv+env size overflow for large payloads.
        const MAX_ENV_PAYLOAD_BYTES: usize = 8 * 1024;
        if payload_json.len() <= MAX_ENV_PAYLOAD_BYTES {
            cmd.env("ZERO_HOOK_PAYLOAD", &payload_json);
        } else {
            cmd.env("ZERO_HOOK_PAYLOAD", "");
            cmd.env("ZERO_HOOK_PAYLOAD_TRUNCATED", "1");
        }

        // NamedTempFile is created with restrictive permissions and removes itself on every exit.
        let mut payload_file = tempfile::Builder::new()
            .prefix("openprx-hook-payload-")
            .suffix(".json")
            .tempfile()?;
        payload_file.write_all(payload_json.as_bytes())?;
        payload_file.flush()?;
        cmd.env("ZERO_HOOK_PAYLOAD_FILE", payload_file.path());

        if action.stdin_json {
            cmd.stdin(std::process::Stdio::piped());
        }
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = crate::runtime::shell_process::spawn_managed_shell_child(cmd)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

        let stderr = child.take_stderr();
        let stderr_reader = tokio::spawn(async move {
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
                        let _ = stderr_reader.await;
                        anyhow::bail!("hook timed out while writing stdin");
                    }
                    Ok(Err(error)) if error.kind() != std::io::ErrorKind::BrokenPipe => {
                        let _ = child.terminate_and_reap().await;
                        let _ = stderr_reader.await;
                        return Err(error.into());
                    }
                    Ok(_) => {}
                }
            }
        }

        let status = match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                let _ = child.terminate_and_reap().await;
                let _ = stderr_reader.await;
                return Err(error.into());
            }
            Err(_) => {
                let _ = child.terminate_and_reap().await;
                let _ = stderr_reader.await;
                anyhow::bail!("hook timed out after {timeout_ms} ms");
            }
        };
        let mut stderr = stderr_reader.await??;
        child.mark_complete();
        let stderr_truncated = stderr.len() > MAX_STDERR_BYTES as usize;
        stderr.truncate(MAX_STDERR_BYTES as usize);

        if status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&stderr);
        let mut stderr = truncate_for_log(stderr.as_ref(), MAX_STDERR_CHARS);
        if stderr_truncated {
            stderr.push_str(" [truncated]");
        }
        anyhow::bail!(
            "hook exited with status {}{}",
            status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(", stderr: {stderr}")
            }
        )
    }
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
