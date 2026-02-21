//! Async sub-agent spawning tool — fire-and-forget with auto-announce on completion.
//!
//! Aligns with OpenClaw's `sessions_spawn` pattern:
//! - Accepts a task description and optional model/timeout
//! - Spawns a tokio task that runs an isolated agent loop
//! - Returns immediately with a run ID
//! - On completion, sends the result back through the channel automatically
//! - `history` action: view the conversation log of any sub-agent run
//! - `steer` action: inject a message into a running sub-agent's context

use super::traits::{Tool, ToolResult};
use crate::agent::loop_::run_tool_call_loop;
use crate::channels::traits::{Channel, SendMessage};
use crate::config::MultimodalConfig;
use crate::hooks::HookManager;
use crate::observability::NoopObserver;
use crate::providers::{ChatMessage, Provider};
use crate::security::SecurityPolicy;
use crate::security::policy::ToolOperation;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Default timeout for sub-agent runs (10 minutes).
const DEFAULT_SUB_AGENT_TIMEOUT_SECS: u64 = 600;

/// Status of a spawned sub-agent run.
#[derive(Debug, Clone)]
pub enum SubAgentStatus {
    Running,
    Completed(String),
    Failed(String),
}

/// A single entry in the sub-agent's conversation history.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub role: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

/// Metadata for a spawned sub-agent run.
#[derive(Debug, Clone)]
pub struct SubAgentRun {
    pub id: String,
    pub task: String,
    pub started_at: DateTime<Utc>,
    pub status: SubAgentStatus,
    /// Handle to abort the spawned tokio task (supports kill action).
    pub abort_handle: Option<tokio::task::AbortHandle>,
    /// Accumulated conversation history from the sub-agent's execution.
    pub history: Arc<RwLock<Vec<HistoryEntry>>>,
    /// Channel to inject steering messages into the running sub-agent.
    pub steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

/// Tool that spawns an asynchronous sub-agent to handle a task in isolation.
/// Returns immediately with a run ID; results are announced via the active channel
/// when the sub-agent completes.
pub struct SessionsSpawnTool {
    /// Channel for announcing sub-agent results.
    channel: Arc<dyn Channel>,
    /// Provider for sub-agent LLM calls.
    provider: Arc<dyn Provider>,
    /// Provider name (for logging/display).
    provider_name: String,
    /// Model to use for sub-agent calls.
    model: String,
    /// Temperature for sub-agent LLM calls.
    temperature: f64,
    /// Security policy (for operation enforcement).
    security: Arc<SecurityPolicy>,
    /// Default recipient for result announcements.
    /// Updated per-message by the channel handler (similar to MessageSendTool).
    default_recipient: Arc<RwLock<Option<String>>>,
    /// Registry of active sub-agent runs.
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    /// Shared tool registry for sub-agent tool call loops.
    /// Set post-construction via `tools_handle().set(...)` to resolve the chicken-and-egg
    /// problem (sessions_spawn is part of tools_registry, but needs it to run sub-agents).
    tools: Arc<OnceLock<Arc<Vec<Box<dyn Tool>>>>>,
    /// Workspace dir for HookManager inside sub-agent loops.
    workspace_dir: PathBuf,
    /// Multimodal config for sub-agent tool call loops.
    multimodal_config: MultimodalConfig,
}

impl SessionsSpawnTool {
    /// Create a new `SessionsSpawnTool` with the given channel and provider.
    pub fn new(
        channel: Arc<dyn Channel>,
        provider: Arc<dyn Provider>,
        provider_name: impl Into<String>,
        model: impl Into<String>,
        temperature: f64,
        security: Arc<SecurityPolicy>,
        workspace_dir: PathBuf,
        multimodal_config: MultimodalConfig,
    ) -> Self {
        Self {
            channel,
            provider,
            provider_name: provider_name.into(),
            model: model.into(),
            temperature,
            security,
            default_recipient: Arc::new(RwLock::new(None)),
            active_runs: Arc::new(RwLock::new(Vec::new())),
            tools: Arc::new(OnceLock::new()),
            workspace_dir,
            multimodal_config,
        }
    }

    /// Return a shareable handle to the default-recipient slot so callers can
    /// update it before each agent turn without replacing the tool registration.
    pub fn default_recipient_handle(&self) -> Arc<RwLock<Option<String>>> {
        self.default_recipient.clone()
    }

    /// Return a handle to the tools OnceLock so callers can set the registry
    /// post-construction (resolves the chicken-and-egg registration problem).
    pub fn tools_handle(&self) -> Arc<OnceLock<Arc<Vec<Box<dyn Tool>>>>> {
        self.tools.clone()
    }

    /// Convenience: update the default recipient from the current message's reply_target.
    pub async fn set_default_recipient(&self, recipient: Option<String>) {
        *self.default_recipient.write().await = recipient;
    }

    /// Return a snapshot of active sub-agent runs (for status queries).
    pub async fn active_runs_snapshot(&self) -> Vec<SubAgentRun> {
        self.active_runs.read().await.clone()
    }
}

#[async_trait]
impl Tool for SessionsSpawnTool {
    fn name(&self) -> &str {
        "sessions_spawn"
    }

    fn description(&self) -> &str {
        "Manage async sub-agents. Actions: \
         'spawn' (default) — launch a sub-agent for a task and return a run_id; \
         'list' — show all active/completed sub-agent runs; \
         'kill' — abort a running sub-agent by run_id; \
         'history' — view the conversation log of a sub-agent run; \
         'steer' — inject a message into a running sub-agent to redirect it."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["spawn", "list", "kill", "history", "steer"],
                    "default": "spawn",
                    "description": "Action to perform: spawn a new sub-agent, list all runs, kill a run, view history, or steer a running sub-agent."
                },
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Task description for the sub-agent to complete. Required for 'spawn' action."
                },
                "run_id": {
                    "type": "string",
                    "description": "Run ID for kill/history/steer actions."
                },
                "message": {
                    "type": "string",
                    "description": "Message to inject into the running sub-agent. Required for 'steer' action."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for the sub-agent. Defaults to the gateway model."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": 10,
                    "maximum": 3600,
                    "description": "Maximum runtime in seconds (default 600). Sub-agent is cancelled if exceeded."
                },
                "recipient": {
                    "type": "string",
                    "description": "Optional recipient for result announcement (phone number, group ID, etc.). \
                                    Defaults to the current conversation sender."
                }
            },
            "required": []
        })
    }

    async fn set_active_recipient(&self, recipient: &str) {
        *self.default_recipient.write().await = Some(recipient.to_string());
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("spawn");

        match action {
            "list" => return self.execute_list().await,
            "kill" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for kill action"))?;
                return self.execute_kill(run_id).await;
            }
            "history" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Missing 'run_id' parameter for history action")
                    })?;
                return self.execute_history(run_id).await;
            }
            "steer" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Missing 'run_id' parameter for steer action")
                    })?;
                let message = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Missing 'message' parameter for steer action")
                    })?;
                return self.execute_steer(run_id, message).await;
            }
            _ => {} // fall through to spawn
        }

        // --- spawn action ---
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("Missing 'task' parameter"))?;

        if task.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'task' parameter must not be empty".into()),
            });
        }

        let timeout_secs = args
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_SUB_AGENT_TIMEOUT_SECS);

        let model_override = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let explicit_recipient = args
            .get("recipient")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Security check
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "sessions_spawn")
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let run_id = Uuid::new_v4().to_string();

        // Resolve the recipient: explicit arg > default_recipient
        let recipient = match explicit_recipient {
            Some(r) => Some(r),
            None => self.default_recipient.read().await.clone(),
        };

        // Create shared history and steer channel for this run
        let history_arc: Arc<RwLock<Vec<HistoryEntry>>> = Arc::new(RwLock::new(Vec::new()));
        let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        // Register the run (abort_handle set after spawn)
        {
            let mut runs = self.active_runs.write().await;
            runs.push(SubAgentRun {
                id: run_id.clone(),
                task: task.to_string(),
                started_at: Utc::now(),
                status: SubAgentStatus::Running,
                abort_handle: None,
                history: history_arc.clone(),
                steer_tx: Some(steer_tx),
            });
        }

        // Clone everything the spawned task needs
        let channel = self.channel.clone();
        let provider = self.provider.clone();
        let provider_name = self.provider_name.clone();
        let model = model_override.unwrap_or_else(|| self.model.clone());
        let temperature = self.temperature;
        let active_runs = self.active_runs.clone();
        let rid = run_id.clone();
        let task_owned = task.to_string();
        let tools = self.tools.get().cloned();
        let workspace_dir = self.workspace_dir.clone();
        let multimodal_config = self.multimodal_config.clone();

        // Spawn async task (fire-and-forget); capture handle to support kill
        let jh = tokio::spawn(async move {
            tracing::info!(run_id = %rid, "Sub-agent task starting");
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                run_sub_agent_task(
                    &task_owned,
                    provider,
                    &provider_name,
                    &model,
                    temperature,
                    tools,
                    &workspace_dir,
                    &multimodal_config,
                    steer_rx,
                    history_arc,
                ),
            )
            .await;
            tracing::info!(run_id = %rid, success = result.is_ok(), "Sub-agent task finished");

            let (status, result_text) = match result {
                Ok(Ok(text)) => (SubAgentStatus::Completed(text.clone()), text),
                Ok(Err(e)) => {
                    let msg = format!("Sub-agent error: {e}");
                    (SubAgentStatus::Failed(e.to_string()), msg)
                }
                Err(_) => {
                    let msg = format!("Sub-agent timed out after {timeout_secs}s");
                    (SubAgentStatus::Failed("timeout".into()), msg)
                }
            };

            // Update run status
            {
                let mut runs = active_runs.write().await;
                if let Some(run) = runs.iter_mut().find(|r| r.id == rid) {
                    run.status = status;
                    run.steer_tx = None; // drop sender — no more steering possible
                }
            }

            // Announce result back to channel if we have a recipient
            if let Some(target) = recipient {
                let announce = format!("🤖 Sub-agent `{rid}` completed:\n\n{result_text}");
                let msg = SendMessage::new(&announce, &target);
                if let Err(e) = channel.send(&msg).await {
                    tracing::error!(run_id = %rid, "Failed to announce sub-agent result: {e}");
                }
            } else {
                tracing::warn!(
                    run_id = %rid,
                    "Sub-agent completed but no recipient configured for announcement"
                );
            }
        });

        // Store the abort handle so kill action can cancel this run
        {
            let mut runs = self.active_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
                run.abort_handle = Some(jh.abort_handle());
            }
        }

        Ok(ToolResult {
            success: true,
            output: format!(
                "Sub-agent spawned (run_id: {run_id}). Will announce result when complete."
            ),
            error: None,
        })
    }
}

impl SessionsSpawnTool {
    /// List all tracked sub-agent runs.
    async fn execute_list(&self) -> anyhow::Result<ToolResult> {
        let runs = self.active_runs.read().await;
        if runs.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No sub-agent runs tracked.".into(),
                error: None,
            });
        }

        let lines: Vec<String> = runs
            .iter()
            .map(|r| {
                let status = match &r.status {
                    SubAgentStatus::Running => "🔄 running".to_string(),
                    SubAgentStatus::Completed(msg) => {
                        let preview = msg.chars().take(60).collect::<String>();
                        let ellipsis = if msg.len() > 60 { "…" } else { "" };
                        format!("✅ completed: {preview}{ellipsis}")
                    }
                    SubAgentStatus::Failed(e) => format!("❌ failed: {e}"),
                };
                let age = (Utc::now() - r.started_at).num_seconds();
                format!("• `{}` [{age}s ago] {status}\n  task: {}", r.id, r.task)
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: format!("Sub-agent runs ({} total):\n\n{}", runs.len(), lines.join("\n\n")),
            error: None,
        })
    }

    /// Kill a running sub-agent by its run ID.
    async fn execute_kill(&self, run_id: &str) -> anyhow::Result<ToolResult> {
        let mut runs = self.active_runs.write().await;
        if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
            match &run.status {
                SubAgentStatus::Running => {
                    if let Some(ref ah) = run.abort_handle {
                        ah.abort();
                    }
                    run.status = SubAgentStatus::Failed("killed by user".into());
                    Ok(ToolResult {
                        success: true,
                        output: format!("Sub-agent `{run_id}` has been killed."),
                        error: None,
                    })
                }
                SubAgentStatus::Completed(_) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Run `{run_id}` already completed.")),
                }),
                SubAgentStatus::Failed(e) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Run `{run_id}` already failed: {e}")),
                }),
            }
        } else {
            Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No run found with ID `{run_id}`.")),
            })
        }
    }

    /// Return the conversation history of a sub-agent run.
    async fn execute_history(&self, run_id: &str) -> anyhow::Result<ToolResult> {
        let runs = self.active_runs.read().await;
        let Some(run) = runs.iter().find(|r| r.id == run_id) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No run found with ID `{run_id}`.")),
            });
        };

        let entries = run.history.read().await;
        if entries.is_empty() {
            let status = match &run.status {
                SubAgentStatus::Running => "still running, no history captured yet",
                SubAgentStatus::Completed(_) => "completed but history is empty",
                SubAgentStatus::Failed(_) => "failed, history may be incomplete",
            };
            return Ok(ToolResult {
                success: true,
                output: format!("No history entries for run `{run_id}` ({status})."),
                error: None,
            });
        }

        let lines: Vec<String> = entries
            .iter()
            .map(|e| {
                let ts = e.timestamp.format("%H:%M:%S").to_string();
                let preview: String = e.content.chars().take(200).collect();
                let ellipsis = if e.content.len() > 200 { "…" } else { "" };
                format!("[{ts}] **{}**: {}{}", e.role, preview, ellipsis)
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: format!(
                "Conversation history for sub-agent `{run_id}` ({} entries):\n\n{}",
                entries.len(),
                lines.join("\n\n")
            ),
            error: None,
        })
    }

    /// Inject a steering message into a running sub-agent.
    async fn execute_steer(&self, run_id: &str, message: &str) -> anyhow::Result<ToolResult> {
        let runs = self.active_runs.read().await;
        let Some(run) = runs.iter().find(|r| r.id == run_id) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No run found with ID `{run_id}`.")),
            });
        };

        match &run.status {
            SubAgentStatus::Running => {
                if let Some(ref tx) = run.steer_tx {
                    tx.send(message.to_string()).map_err(|_| {
                        anyhow::anyhow!("Sub-agent steer channel closed unexpectedly")
                    })?;
                    Ok(ToolResult {
                        success: true,
                        output: format!(
                            "Steering message sent to sub-agent `{run_id}`. \
                             The agent will incorporate it at the next opportunity."
                        ),
                        error: None,
                    })
                } else {
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Run `{run_id}` is running but has no steer channel (legacy run)."
                        )),
                    })
                }
            }
            SubAgentStatus::Completed(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Run `{run_id}` already completed; cannot steer.")),
            }),
            SubAgentStatus::Failed(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Run `{run_id}` already failed ({e}); cannot steer.")),
            }),
        }
    }
}

/// Maximum tool-call iterations for a sub-agent run (per steering segment).
const SUB_AGENT_MAX_ITERATIONS: usize = 15;

/// Convert a slice of `ChatMessage` to `HistoryEntry` values.
/// Each entry is timestamped with the current wall-clock time (approximate).
fn chat_messages_to_history(messages: &[ChatMessage]) -> Vec<HistoryEntry> {
    let now = Utc::now();
    messages
        .iter()
        .map(|m| HistoryEntry {
            role: m.role.clone(),
            content: m.content.clone(),
            timestamp: now,
        })
        .collect()
}

/// Run an isolated sub-agent loop with steering and history support.
///
/// Supports:
/// - Agentic tool-call loop (when a tool registry is available)
/// - Steering: injected messages are added to the conversation and the loop restarts
/// - History: `history_out` is updated after each significant state change
///
/// Falls back to a single-turn completion when no tools are registered.
async fn run_sub_agent_task(
    task: &str,
    provider: Arc<dyn Provider>,
    provider_name: &str,
    model: &str,
    temperature: f64,
    tools: Option<Arc<Vec<Box<dyn Tool>>>>,
    workspace_dir: &std::path::Path,
    multimodal_config: &MultimodalConfig,
    mut steer_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    history_out: Arc<RwLock<Vec<HistoryEntry>>>,
) -> anyhow::Result<String> {
    const SYSTEM_PROMPT: &str = "\
You are a sub-agent handling a specific delegated task. \
Complete the task thoroughly and report results concisely. \
Focus only on the assigned task; do not ask clarifying questions.";

    // --- No-tools fallback: single-turn completion ---
    let Some(tools_registry) = tools else {
        let response = provider
            .chat_with_system(Some(SYSTEM_PROMPT), task, model, temperature)
            .await?;
        let history = vec![
            HistoryEntry {
                role: "user".into(),
                content: task.to_string(),
                timestamp: Utc::now(),
            },
            HistoryEntry {
                role: "assistant".into(),
                content: response.clone(),
                timestamp: Utc::now(),
            },
        ];
        *history_out.write().await = history;
        return Ok(if response.trim().is_empty() {
            "[Sub-agent produced no output]".to_string()
        } else {
            response
        });
    };

    // --- Agentic loop with steering support ---
    let mut history: Vec<ChatMessage> = vec![
        ChatMessage::system(SYSTEM_PROMPT),
        ChatMessage::user(task),
    ];

    loop {
        let cancel_token = CancellationToken::new();

        // Clone everything needed for the inner spawned task.
        // We move `history` into the task and get it back after completion.
        let mut h = history;
        let p = provider.clone();
        let pn = provider_name.to_string();
        let m = model.to_string();
        let t = temperature;
        let tr = tools_registry.clone();
        let wd = workspace_dir.to_path_buf();
        let mc = multimodal_config.clone();
        let ct = cancel_token.clone();

        let mut loop_handle = tokio::spawn(async move {
            let observer = NoopObserver;
            let hooks = HookManager::new(wd);
            let result = run_tool_call_loop(
                p.as_ref(),
                &mut h,
                tr.as_slice(),
                &observer,
                &hooks,
                &pn,
                &m,
                t,
                true,  // silent — no streaming output
                None,  // no approval manager
                "sessions_spawn",
                &mc,
                SUB_AGENT_MAX_ITERATIONS,
                Some(ct),
                None, // no streaming sender
                None, // no scope context for spawned sessions
            )
            .await;
            (h, result)
        });

        // Race: loop completion vs steering message
        tokio::select! {
            loop_result = &mut loop_handle => {
                // Inner loop finished (naturally or via error)
                let (returned_history, result) = loop_result?;
                history = returned_history;
                // Write final history to shared store
                *history_out.write().await = chat_messages_to_history(&history);
                return match result {
                    Ok(text) => Ok(if text.trim().is_empty() {
                        "[Sub-agent produced no output]".to_string()
                    } else {
                        text
                    }),
                    Err(e) => Err(e),
                };
            },
            steer_opt = steer_rx.recv() => {
                match steer_opt {
                    Some(steer_msg) => {
                        // Cancel the running inner loop
                        cancel_token.cancel();
                        // Wait for the task to acknowledge cancellation and return history
                        let (returned_history, _cancelled_result) = loop_handle.await?;
                        history = returned_history;
                        // Inject the steering message as a user turn
                        tracing::info!("Sub-agent steering: injecting message");
                        history.push(ChatMessage::user(format!(
                            "[Steering instruction from operator] {steer_msg}"
                        )));
                        // Update shared history so callers can see the injected message
                        *history_out.write().await = chat_messages_to_history(&history);
                        // Loop continues — will re-enter with updated history
                    }
                    None => {
                        // Steer channel closed — no more steering; wait for natural completion
                        let (returned_history, result) = loop_handle.await?;
                        history = returned_history;
                        *history_out.write().await = chat_messages_to_history(&history);
                        return match result {
                            Ok(text) => Ok(if text.trim().is_empty() {
                                "[Sub-agent produced no output]".to_string()
                            } else {
                                text
                            }),
                            Err(e) => Err(e),
                        };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
    use crate::security::SecurityPolicy;
    use anyhow::anyhow;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    /// A channel that records sent messages.
    struct RecordingChannel {
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingChannel {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let sent = Arc::new(Mutex::new(Vec::new()));
            (Self { sent: sent.clone() }, sent)
        }
    }

    #[async_trait::async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "recording"
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().await.push(message.content.clone());
            Ok(())
        }

        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A provider that returns a canned response.
    struct EchoProvider {
        response: String,
    }

    #[async_trait::async_trait]
    impl crate::providers::Provider for EchoProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(self.response.clone())
        }

        async fn chat(
            &self,
            _request: crate::providers::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            Ok(crate::providers::ChatResponse {
                text: Some(self.response.clone()),
                tool_calls: Vec::new(),
            })
        }
    }

    /// A provider that always fails.
    struct FailingProvider;

    #[async_trait::async_trait]
    impl crate::providers::Provider for FailingProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Err(anyhow!("provider failure"))
        }

        async fn chat(
            &self,
            _request: crate::providers::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            Err(anyhow!("provider failure"))
        }
    }

    fn make_tool(
        channel: Arc<dyn Channel>,
        provider: Arc<dyn crate::providers::Provider>,
    ) -> SessionsSpawnTool {
        SessionsSpawnTool::new(
            channel,
            provider,
            "test-provider",
            "test-model",
            0.7,
            test_security(),
            std::path::PathBuf::from("/tmp"),
            crate::config::MultimodalConfig::default(),
        )
    }

    #[test]
    fn name_and_description() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
        );
        assert_eq!(tool.name(), "sessions_spawn");
        assert!(!tool.description().is_empty());
        assert!(tool.description().contains("history"));
        assert!(tool.description().contains("steer"));
    }

    #[test]
    fn schema_has_required_fields() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
        );
        let schema = tool.parameters_schema();
        // All params are optional at schema level; runtime validates per action
        let required = schema["required"].as_array().unwrap();
        assert!(required.is_empty(), "Required should be empty (validated at runtime)");
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["task"].is_object());
        assert!(schema["properties"]["run_id"].is_object());
        assert!(schema["properties"]["message"].is_object());
        assert!(schema["properties"]["model"].is_object());
        assert!(schema["properties"]["timeout_seconds"].is_object());
        assert!(schema["properties"]["recipient"].is_object());
        // Verify enum includes history and steer
        let enum_vals = schema["properties"]["action"]["enum"].as_array().unwrap();
        let enum_strs: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
        assert!(enum_strs.contains(&"history"));
        assert!(enum_strs.contains(&"steer"));
    }

    #[tokio::test]
    async fn missing_task_returns_error() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
        );
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn empty_task_returns_failure() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
        );
        let result = tool
            .execute(json!({"task": "   "}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn spawns_and_returns_run_id() {
        let (ch, sent) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "The joke: Why did the chicken cross the road?".into(),
            }),
        );
        tool.set_default_recipient(Some("test-recipient".to_string()))
            .await;

        let result = tool
            .execute(json!({"task": "Tell me a joke"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("run_id:"));
        assert!(result.output.contains("Will announce"));

        // Wait briefly for the spawned task to complete
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let messages = sent.lock().await;
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("Sub-agent"));
        assert!(messages[0].contains("chicken"));
    }

    #[tokio::test]
    async fn no_recipient_skips_announcement() {
        let (ch, sent) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "done".into(),
            }),
        );
        // No default_recipient set, no recipient in args

        let result = tool
            .execute(json!({"task": "Do something"}))
            .await
            .unwrap();
        assert!(result.success);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let messages = sent.lock().await;
        assert!(messages.is_empty(), "Should not announce without recipient");
    }

    #[tokio::test]
    async fn explicit_recipient_overrides_default() {
        let (ch, sent) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "result".into(),
            }),
        );
        tool.set_default_recipient(Some("default-recipient".to_string()))
            .await;

        let result = tool
            .execute(json!({
                "task": "Test task",
                "recipient": "explicit-recipient"
            }))
            .await
            .unwrap();
        assert!(result.success);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let messages = sent.lock().await;
        // Should have sent to explicit-recipient (check channel.sent has a message)
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn failed_provider_announces_error() {
        let (ch, sent) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(FailingProvider));
        tool.set_default_recipient(Some("user".to_string())).await;

        let result = tool
            .execute(json!({"task": "This will fail"}))
            .await
            .unwrap();
        assert!(result.success); // spawn succeeds; failure is in the sub-agent

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let messages = sent.lock().await;
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0].contains("error") || messages[0].contains("Error"),
            "Error message should be announced: {}",
            messages[0]
        );
    }

    #[tokio::test]
    async fn active_runs_tracked() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "done".into(),
            }),
        );

        // Spawn a run
        let _ = tool
            .execute(json!({"task": "Some task"}))
            .await
            .unwrap();

        let runs = tool.active_runs_snapshot().await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].task, "Some task");
    }

    #[tokio::test]
    async fn default_recipient_handle_shared() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
        );
        let handle = tool.default_recipient_handle();
        *handle.write().await = Some("via-handle".to_string());

        let val = tool.default_recipient.read().await.clone();
        assert_eq!(val.as_deref(), Some("via-handle"));
    }

    #[tokio::test]
    async fn history_action_returns_no_run_error() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
        );
        let result = tool
            .execute(json!({"action": "history", "run_id": "nonexistent"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("No run found"));
    }

    #[tokio::test]
    async fn history_action_requires_run_id() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
        );
        let result = tool
            .execute(json!({"action": "history"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn steer_action_requires_message() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
        );
        let result = tool
            .execute(json!({"action": "steer", "run_id": "xxx"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn steer_action_returns_no_run_error() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
        );
        let result = tool
            .execute(json!({"action": "steer", "run_id": "nonexistent", "message": "pivot!"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("No run found"));
    }

    #[tokio::test]
    async fn history_populated_after_no_tools_run() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "finished work".into(),
            }),
        );

        // Spawn without tool registry (no tools set)
        let spawn_result = tool
            .execute(json!({"task": "Do a thing"}))
            .await
            .unwrap();
        assert!(spawn_result.success);
        let run_id = spawn_result
            .output
            .split("run_id: ")
            .nth(1)
            .unwrap()
            .split(')')
            .next()
            .unwrap()
            .trim()
            .to_string();

        // Wait for task to complete
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Check history
        let hist_result = tool
            .execute(json!({"action": "history", "run_id": run_id}))
            .await
            .unwrap();
        assert!(hist_result.success);
        assert!(hist_result.output.contains("user"));
        assert!(hist_result.output.contains("assistant"));
    }
}
