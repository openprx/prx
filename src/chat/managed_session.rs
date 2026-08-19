use crate::chat::sessions::event::SessionEventSink;
use crate::chat::sessions::id::SessionId;
use crate::chat::sessions::model::{ManagedStatus, project_shell_status};
use crate::chat::sessions::runtime::ShellRegistry;
use crate::chat::sessions::shell::ShellOrigin;
use crate::tools::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;

/// How many *finished* shell sessions the shared registry keeps.
///
/// The registry is a `Vec` that this tool pushes into on every `action=shell`,
/// and a session handle is never removed by finishing — the reaper task updates
/// its status in place. Without a ceiling a long chat that keeps starting
/// background shells grows the vector for the life of the process.
///
/// Only *terminal* sessions are ever dropped, newest kept first, so the ceiling
/// can never take away output that is still being produced, and the entries the
/// model is most likely to read back with `logs` are the ones that survive. The
/// bound is deliberately far above `ReapPolicy::keep_last_terminal` (5), which
/// is what prunes this same registry from the chat main loop: in an interactive
/// chat that policy has always trimmed the vector long before this ceiling is
/// reached, so this is a backstop for bursts and for any path where the main
/// loop's reaper is not draining the registry — not a second retention policy
/// competing with it.
const MAX_RETAINED_TERMINAL_SESSIONS: usize = 32;

/// Model-callable access to PRX managed background sessions.
///
/// This intentionally shares the same shell registry and event bridge as chat
/// `/shell`, so model-spawned background shells show up in the TUI bottom list,
/// `/sessions`, `/logs`, `/kill`, and chat-exit cleanup.
pub struct ManagedSessionTool {
    workspace_dir: PathBuf,
    shells: ShellRegistry,
    event_sink: SessionEventSink,
}

impl ManagedSessionTool {
    #[must_use]
    pub const fn new(workspace_dir: PathBuf, shells: ShellRegistry, event_sink: SessionEventSink) -> Self {
        Self {
            workspace_dir,
            shells,
            event_sink,
        }
    }

    /// Drop finished sessions beyond [`MAX_RETAINED_TERMINAL_SESSIONS`],
    /// returning how many were removed.
    ///
    /// Running sessions are never touched: their handle is what `logs` and
    /// `kill` resolve through, and their output buffer is still being filled.
    /// Terminal sessions are ranked newest-first by the timestamp the reaper
    /// recorded, so reclamation always eats the coldest entries first.
    fn reclaim_finished(&self) -> usize {
        let mut shells = self.shells.lock();
        let terminal = shells.iter().filter(|shell| shell.is_terminal()).count();
        if terminal <= MAX_RETAINED_TERMINAL_SESSIONS {
            return 0;
        }

        let mut ranked: Vec<(DateTime<Utc>, &SessionId)> = shells
            .iter()
            .filter(|shell| shell.is_terminal())
            .map(|shell| (shell.finished_at().unwrap_or(shell.started_at), &shell.id))
            .collect();
        ranked.sort_unstable_by_key(|(finished_at, _)| std::cmp::Reverse(*finished_at));
        let drop_ids: HashSet<SessionId> = ranked
            .into_iter()
            .skip(MAX_RETAINED_TERMINAL_SESSIONS)
            .map(|(_, id)| id.clone())
            .collect();

        let removed = drop_ids.len();
        shells.retain(|shell| !drop_ids.contains(&shell.id));
        removed
    }

    fn execute_list(&self) -> ToolResult {
        self.reclaim_finished();
        let shells = self.shells.lock().clone();
        if shells.is_empty() {
            return ToolResult {
                success: true,
                output: "No managed shell sessions.".to_string(),
                error: None,
            };
        }

        let mut lines = Vec::with_capacity(shells.len() + 1);
        lines.push("Managed shell sessions:".to_string());
        for shell in &shells {
            let elapsed = Utc::now().signed_duration_since(shell.started_at).num_seconds().max(0);
            let status = status_label(project_shell_status(&shell.status()));
            let mut tail = shell.recent_output(1).pop().unwrap_or_default();
            tail.truncate(tail.floor_char_boundary(tail.len().min(120)));
            lines.push(format!(
                "- id={} status={} elapsed={}s truncated={} command={}{}",
                shell.id.as_str(),
                status,
                elapsed,
                shell.output_truncated(),
                shell.command,
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(" tail={tail}")
                }
            ));
        }

        ToolResult {
            success: true,
            output: lines.join("\n"),
            error: None,
        }
    }

    fn execute_shell(&self, command: &str) -> ToolResult {
        // Reclaim before growing, so the registry cannot creep upward one
        // finished session at a time.
        self.reclaim_finished();
        match crate::chat::sessions::shell::spawn_shell_with_origin(
            command,
            &self.workspace_dir,
            &self.event_sink,
            ShellOrigin::Model,
        ) {
            Ok(session) => {
                let id = session.id.as_str().to_string();
                self.shells.lock().push(session);
                ToolResult {
                    success: true,
                    output: format!(
                        "Started managed shell session id={id}. It is visible in the PRX chat bottom session list and can be inspected with managed_session logs."
                    ),
                    error: None,
                }
            }
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to start managed shell session: {error}")),
            },
        }
    }

    async fn execute_kill(&self, session_id: &str) -> ToolResult {
        let target = find_shell(&self.shells, session_id);
        let Some(shell) = target else {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No managed shell session with id `{session_id}`.")),
            };
        };

        match shell.kill().await {
            Ok(()) => ToolResult {
                success: true,
                output: format!("Killed managed shell session id={}.", shell.id.as_str()),
                error: None,
            },
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Failed to kill managed shell session id={}: {error}",
                    shell.id.as_str()
                )),
            },
        }
    }

    fn execute_logs(&self, session_id: &str, max_lines: usize) -> ToolResult {
        let target = find_shell(&self.shells, session_id);
        let Some(shell) = target else {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No managed shell session with id `{session_id}`.")),
            };
        };

        let lines = shell.recent_output(max_lines.clamp(1, 500));
        if lines.is_empty() {
            return ToolResult {
                success: true,
                output: format!(
                    "Managed shell session id={} has no buffered output yet.",
                    shell.id.as_str()
                ),
                error: None,
            };
        }

        let mut output = String::new();
        if shell.output_truncated() {
            output.push_str("[output truncated]\n");
        }
        output.push_str(&lines.join("\n"));
        ToolResult {
            success: true,
            output,
            error: None,
        }
    }
}

#[async_trait]
impl Tool for ManagedSessionTool {
    fn name(&self) -> &str {
        "managed_session"
    }

    fn description(&self) -> &str {
        "Create and manage PRX background sessions visible in the chat TUI. Use this for long-running shell observers instead of one-shot shell."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["shell", "list", "logs", "kill"],
                    "description": "shell starts a managed background shell; list shows sessions; logs reads recent output; kill terminates a shell session."
                },
                "command": {
                    "type": "string",
                    "description": "Shell command to run for action=shell."
                },
                "session_id": {
                    "type": "string",
                    "description": "Managed shell session id for logs or kill."
                },
                "max_lines": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 500,
                    "description": "Maximum recent log lines to return for logs."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("list");
        let result = match action {
            "shell" => {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                command.map_or_else(
                    || ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Missing non-empty `command` for action=shell.".to_string()),
                    },
                    |command| self.execute_shell(command),
                )
            }
            "list" => self.execute_list(),
            "logs" => {
                let session_id = args
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                session_id.map_or_else(
                    || ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Missing non-empty `session_id` for action=logs.".to_string()),
                    },
                    |session_id| {
                        let max_lines = args.get("max_lines").and_then(|v| v.as_u64()).unwrap_or(80) as usize;
                        self.execute_logs(session_id, max_lines)
                    },
                )
            }
            "kill" => {
                let session_id = args
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                match session_id {
                    Some(session_id) => self.execute_kill(session_id).await,
                    None => ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Missing non-empty `session_id` for action=kill.".to_string()),
                    },
                }
            }
            other => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Invalid managed_session action `{other}`.")),
            },
        };
        Ok(result)
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Core
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System, ToolCategory::DevOps]
    }
}

fn find_shell(shells: &ShellRegistry, session_id: &str) -> Option<crate::chat::sessions::shell::ShellSession> {
    shells
        .lock()
        .iter()
        .find(|shell| {
            let id = shell.id.as_str();
            id == session_id || id.starts_with(session_id)
        })
        .cloned()
}

const fn status_label(status: ManagedStatus) -> &'static str {
    match status {
        ManagedStatus::Running => "running",
        ManagedStatus::Completed => "completed",
        ManagedStatus::Failed => "failed",
        ManagedStatus::Cancelled => "cancelled",
        ManagedStatus::NeedsInput => "needs-input",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn auto_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    #[tokio::test]
    async fn managed_session_shell_list_logs_and_kill() {
        let (sink, _rx) = SessionEventSink::channel();
        let shells = Arc::new(Mutex::new(Vec::new()));
        let tool = ManagedSessionTool::new(auto_security().workspace_dir.clone(), Arc::clone(&shells), sink);

        let started = tool
            .execute(json!({
                "action": "shell",
                "command": "echo managed-session-ok; sleep 5"
            }))
            .await
            .expect("tool call");
        assert!(started.success, "{started:?}");

        let id = shells
            .lock()
            .first()
            .expect("managed shell should be registered")
            .id
            .as_str()
            .to_string();
        for _ in 0..100 {
            let has_output = shells
                .lock()
                .first()
                .expect("managed shell should remain registered")
                .recent_output(10)
                .iter()
                .any(|line| line.contains("managed-session-ok"));
            if has_output {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let listed = tool.execute(json!({"action": "list"})).await.expect("list");
        assert!(listed.output.contains(&id));

        let logs = tool
            .execute(json!({"action": "logs", "session_id": id, "max_lines": 20}))
            .await
            .expect("logs");
        assert!(logs.output.contains("managed-session-ok"), "logs: {logs:?}");

        let killed = tool
            .execute(json!({"action": "kill", "session_id": id}))
            .await
            .expect("kill");
        assert!(killed.success, "{killed:?}");
    }

    /// The registry must actually *shrink* once finished sessions pile up past
    /// the ceiling — a status flag alone would leave the vector growing for the
    /// life of the process — while a still-running session and the output an
    /// operator has not read yet are left completely alone.
    #[tokio::test]
    async fn finished_sessions_are_reclaimed_and_running_ones_keep_their_output() {
        let (sink, _rx) = SessionEventSink::channel();
        let shells: crate::chat::sessions::runtime::ShellRegistry = Arc::new(Mutex::new(Vec::new()));
        let workspace = auto_security().workspace_dir.clone();
        let tool = ManagedSessionTool::new(workspace.clone(), Arc::clone(&shells), sink.clone());

        // A long-lived session that must survive reclamation with its buffered
        // output still readable through `logs`.
        let started = tool
            .execute(json!({
                "action": "shell",
                "command": "echo reclaim-keep-me-alive; sleep 300"
            }))
            .await
            .expect("test: spawn long-running session");
        assert!(started.success, "{started:?}");
        let runner_id = shells
            .lock()
            .first()
            .expect("test: long-running session registered")
            .id
            .as_str()
            .to_string();
        for _ in 0..200 {
            let ready = shells.lock().first().is_some_and(|shell| {
                shell
                    .recent_output(10)
                    .iter()
                    .any(|line| line.contains("reclaim-keep-me-alive"))
            });
            if ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // Fill past the ceiling. Pushed directly rather than through the tool so
        // reclamation cannot fire while the fixture is still being built, and
        // serialised so `finished_at` ordering is deterministic.
        let overflow = MAX_RETAINED_TERMINAL_SESSIONS + 4;
        for index in 0..overflow {
            let session = crate::chat::sessions::shell::spawn_shell_with_origin(
                &format!("echo reclaim-finished-{index}"),
                &workspace,
                &sink,
                crate::chat::sessions::shell::ShellOrigin::Model,
            )
            .expect("test: spawn short-lived session");
            for _ in 0..200 {
                if session.is_terminal() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            assert!(session.is_terminal(), "test: short-lived session must finish");
            shells.lock().push(session);
        }

        let before = shells.lock().len();
        assert_eq!(before, overflow + 1, "test: fixture must exceed the ceiling");

        let listed = tool.execute(json!({"action": "list"})).await.expect("list");
        assert!(listed.success, "{listed:?}");

        let after = shells.lock().len();
        assert!(
            after < before,
            "the registry must actually shrink, not merely mark entries: {before} -> {after}"
        );
        assert_eq!(
            after,
            MAX_RETAINED_TERMINAL_SESSIONS + 1,
            "reclamation keeps the ceiling of finished sessions plus the running one"
        );

        let surviving: Vec<String> = shells.lock().iter().map(|shell| shell.command.clone()).collect();
        assert!(
            surviving
                .iter()
                .any(|command| command.contains("reclaim-keep-me-alive")),
            "the running session must never be reclaimed: {surviving:?}"
        );
        assert!(
            surviving
                .iter()
                .any(|command| command.ends_with(&format!("reclaim-finished-{}", overflow - 1))),
            "the newest finished session must survive: {surviving:?}"
        );
        assert!(
            !surviving.iter().any(|command| command.ends_with("reclaim-finished-0")),
            "the coldest finished session must be the one reclaimed: {surviving:?}"
        );

        // The running session's output is intact after reclamation.
        let logs = tool
            .execute(json!({"action": "logs", "session_id": runner_id.clone(), "max_lines": 20}))
            .await
            .expect("logs");
        assert!(
            logs.output.contains("reclaim-keep-me-alive"),
            "reclamation must not drop output the operator has not read: {logs:?}"
        );

        let killed = tool
            .execute(json!({"action": "kill", "session_id": runner_id}))
            .await
            .expect("kill");
        assert!(killed.success, "{killed:?}");
    }
}
