//! Async sub-agent spawning tool — fire-and-forget with auto-announce on completion.
//!
//! Aligns with OpenClaw's `sessions_spawn` pattern:
//! - Accepts a task description and optional model/timeout
//! - Spawns a tokio task that runs an isolated agent loop
//! - Returns immediately with a run ID
//! - On completion, sends the result back through the channel automatically

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

/// Metadata for a spawned sub-agent run.
#[derive(Debug, Clone)]
pub struct SubAgentRun {
    pub id: String,
    pub task: String,
    pub started_at: DateTime<Utc>,
    pub status: SubAgentStatus,
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
        "Spawn an async sub-agent to handle a task in isolation. Returns immediately with a run ID. \
         The sub-agent will announce its result back to the current conversation when complete. \
         Use for long-running or parallel tasks that should not block the main conversation."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Task description for the sub-agent to complete"
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
            "required": ["task"]
        })
    }

    async fn set_active_recipient(&self, recipient: &str) {
        *self.default_recipient.write().await = Some(recipient.to_string());
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
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

        // Register the run
        {
            let mut runs = self.active_runs.write().await;
            runs.push(SubAgentRun {
                id: run_id.clone(),
                task: task.to_string(),
                started_at: Utc::now(),
                status: SubAgentStatus::Running,
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

        // Spawn async task (fire-and-forget)
        tokio::spawn(async move {
            tracing::info!(run_id = %rid, "Sub-agent task starting");
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                run_sub_agent(
                    &task_owned,
                    provider.as_ref(),
                    &provider_name,
                    &model,
                    temperature,
                    tools.as_deref(),
                    &workspace_dir,
                    &multimodal_config,
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
                    let msg = format!(
                        "Sub-agent timed out after {timeout_secs}s"
                    );
                    (SubAgentStatus::Failed("timeout".into()), msg)
                }
            };

            // Update run status
            {
                let mut runs = active_runs.write().await;
                if let Some(run) = runs.iter_mut().find(|r| r.id == rid) {
                    run.status = status;
                }
            }

            // Announce result back to channel if we have a recipient
            if let Some(target) = recipient {
                let announce = format!(
                    "🤖 Sub-agent `{rid}` completed:\n\n{result_text}"
                );
                let msg = SendMessage::new(&announce, &target);
                if let Err(e) = channel.send(&msg).await {
                    tracing::error!(
                        run_id = %rid,
                        "Failed to announce sub-agent result: {e}"
                    );
                }
            } else {
                // No channel to announce to; log as warning
                tracing::warn!(
                    run_id = %rid,
                    "Sub-agent completed but no recipient configured for announcement"
                );
            }
        });

        Ok(ToolResult {
            success: true,
            output: format!(
                "Sub-agent spawned (run_id: {run_id}). Will announce result when complete."
            ),
            error: None,
        })
    }
}

/// Maximum tool-call iterations for a sub-agent run.
const SUB_AGENT_MAX_ITERATIONS: usize = 15;

/// Run an isolated sub-agent loop for the given task.
///
/// When a tool registry is provided (via the OnceLock), runs a full agentic
/// tool-call loop (up to `SUB_AGENT_MAX_ITERATIONS`). Sessions_spawn itself
/// is included in the tool set — recursion is bounded by the per-run timeout.
///
/// Falls back to a single-turn completion when no tools are registered.
async fn run_sub_agent(
    task: &str,
    provider: &dyn Provider,
    provider_name: &str,
    model: &str,
    temperature: f64,
    tools: Option<&Vec<Box<dyn Tool>>>,
    workspace_dir: &std::path::Path,
    multimodal_config: &MultimodalConfig,
) -> anyhow::Result<String> {
    const SYSTEM_PROMPT: &str = "\
You are a sub-agent handling a specific delegated task. \
Complete the task thoroughly and report results concisely. \
Focus only on the assigned task; do not ask clarifying questions.";

    if let Some(tools_registry) = tools {
        // Agentic loop with tool support
        let mut history = vec![
            ChatMessage::system(SYSTEM_PROMPT),
            ChatMessage::user(task),
        ];
        let observer = NoopObserver;
        let hooks = HookManager::new(workspace_dir.to_path_buf());

        let response = run_tool_call_loop(
            provider,
            &mut history,
            tools_registry.as_slice(),
            &observer,
            &hooks,
            provider_name,
            model,
            temperature,
            true,  // silent — no streaming output
            None,  // no approval manager
            "sessions_spawn",
            multimodal_config,
            SUB_AGENT_MAX_ITERATIONS,
            None, // no cancellation token
            None, // no streaming sender
        )
        .await?;

        if response.trim().is_empty() {
            return Ok("[Sub-agent produced no output]".to_string());
        }
        Ok(response)
    } else {
        // Fallback: single-turn completion (no tools registered yet)
        let response = provider
            .chat_with_system(Some(SYSTEM_PROMPT), task, model, temperature)
            .await?;

        if response.trim().is_empty() {
            return Ok("[Sub-agent produced no output]".to_string());
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
    use crate::security::SecurityPolicy;
    use anyhow::anyhow;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    }

    #[test]
    fn schema_has_required_task() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
        );
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("task")));
        assert!(schema["properties"]["task"].is_object());
        assert!(schema["properties"]["model"].is_object());
        assert!(schema["properties"]["timeout_seconds"].is_object());
        assert!(schema["properties"]["recipient"].is_object());
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
}
