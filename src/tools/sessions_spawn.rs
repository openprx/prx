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
use crate::agent::loop_::{run_tool_call_loop, ScopeContext};
use crate::channels::build_identity_prompt;
use crate::channels::traits::{Channel, SendMessage};
use crate::config::{
    AgentCompactionConfig, DelegateAgentConfig, MultimodalConfig, SessionsSpawnConfig,
};
use crate::hooks::HookManager;
use crate::observability::NoopObserver;
use crate::providers::{self, ChatMessage, Provider};
use crate::security::policy::ToolOperation;
use crate::security::SecurityPolicy;
use crate::session_worker::protocol::{WorkerManifest, WorkerResult};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Default timeout for sub-agent runs (10 minutes).
const DEFAULT_SUB_AGENT_TIMEOUT_SECS: u64 = 0;
const DEFAULT_SUB_AGENT_SYSTEM_PROMPT: &str = "\
You are a sub-agent handling a specific delegated task. \
Complete the task thoroughly and report results concisely. \
Focus only on the assigned task; do not ask clarifying questions.";

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
    pub recipient: Option<String>,
    /// Handle to abort the spawned tokio task (supports kill action).
    pub abort_handle: Option<tokio::task::AbortHandle>,
    /// Accumulated conversation history from the sub-agent's execution.
    pub history: Arc<RwLock<Vec<HistoryEntry>>>,
    /// Channel to inject steering messages into the running sub-agent.
    pub steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub parent_run_id: Option<String>,
    pub session_scope_key: String,
    pub spawn_depth: usize,
}

#[derive(Debug, Clone)]
struct SpawnExecutionContext {
    run_id: String,
    session_scope_key: String,
    spawn_depth: usize,
}

tokio::task_local! {
    static SPAWN_EXECUTION_CONTEXT: SpawnExecutionContext;
}

#[derive(Debug, Clone)]
struct SpawnScope {
    sender: String,
    channel: String,
    chat_type: String,
    chat_id: String,
}

fn parse_spawn_scope(args: &serde_json::Value) -> Option<SpawnScope> {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return None;
    }

    let scope = args
        .get("_zc_scope")
        .and_then(serde_json::Value::as_object)?;
    let sender = scope
        .get("sender")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let channel = scope
        .get("channel")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let chat_type = scope
        .get("chat_type")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let chat_id = scope
        .get("chat_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();

    Some(SpawnScope {
        sender,
        channel,
        chat_type,
        chat_id,
    })
}

fn current_spawn_execution_context() -> Option<SpawnExecutionContext> {
    SPAWN_EXECUTION_CONTEXT.try_with(|ctx| ctx.clone()).ok()
}

pub(crate) async fn with_spawn_execution_context<T, Fut>(
    run_id: String,
    session_scope_key: String,
    spawn_depth: usize,
    fut: Fut,
) -> T
where
    Fut: std::future::Future<Output = T>,
{
    SPAWN_EXECUTION_CONTEXT
        .scope(
            SpawnExecutionContext {
                run_id,
                session_scope_key,
                spawn_depth,
            },
            fut,
        )
        .await
}

#[cfg(test)]
pub(crate) fn spawn_execution_context_snapshot() -> Option<(String, String, usize)> {
    current_spawn_execution_context()
        .map(|ctx| (ctx.run_id, ctx.session_scope_key, ctx.spawn_depth))
}

fn spawn_session_scope_key(
    parent_ctx: Option<&SpawnExecutionContext>,
    scope: Option<&SpawnScope>,
) -> String {
    if let Some(parent) = parent_ctx {
        return parent.session_scope_key.clone();
    }

    if let Some(scope) = scope {
        return format!("{}:{}:{}", scope.channel, scope.chat_id, scope.sender);
    }

    "sessions_spawn:global".to_string()
}

fn running_run_count(runs: &[SubAgentRun]) -> usize {
    runs.iter()
        .filter(|run| matches!(run.status, SubAgentStatus::Running))
        .count()
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
    /// Compaction config for sub-agent tool call loops.
    compaction_config: AgentCompactionConfig,
    /// Configured named agents for identity/model/tool scoping in spawn.
    agents: Arc<HashMap<String, DelegateAgentConfig>>,
    /// Global credential fallback from root config.
    fallback_api_key: Option<String>,
    /// Provider runtime options (auth profile, state dir, etc.).
    provider_runtime_options: providers::ProviderRuntimeOptions,
    /// Process-mode controls for workspace lifecycle.
    spawn_config: SessionsSpawnConfig,
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
        compaction_config: AgentCompactionConfig,
        agents: HashMap<String, DelegateAgentConfig>,
        fallback_api_key: Option<String>,
        provider_runtime_options: providers::ProviderRuntimeOptions,
        spawn_config: SessionsSpawnConfig,
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
            compaction_config,
            agents: Arc::new(agents),
            fallback_api_key,
            provider_runtime_options,
            spawn_config,
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

    /// Return a shared Arc to the active runs registry.
    /// Used by sessions_list, sessions_send, and session_status to share state.
    pub fn active_runs_arc(&self) -> Arc<RwLock<Vec<SubAgentRun>>> {
        self.active_runs.clone()
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
        let available_agents = self
            .agents
            .iter()
            .filter(|(_, cfg)| cfg.spawn_enabled.unwrap_or(true))
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
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
                "agent": {
                    "type": "string",
                    "description": format!(
                        "Optional identity agent name. Available: {}",
                        if available_agents.is_empty() {
                            "(none configured)".to_string()
                        } else {
                            available_agents.join(", ")
                        }
                    )
                },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Maximum runtime in seconds. 0 or omitted = no timeout (sub-agent runs until completion). Set a value to enforce a deadline."
                },
                "max_iterations": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum tool call iterations for the sub-agent. Overrides agent config default. Omit to use agent/global config value."
                },
                "mode": {
                    "type": "string",
                    "enum": ["task", "process"],
                    "default": "task",
                    "description": "Execution mode. 'task' keeps current in-process behavior (default), 'process' launches an isolated OS process."
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
        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(self.spawn_config.default_mode.as_str())
            .to_ascii_lowercase();
        if mode != "task" && mode != "process" {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Invalid 'mode' value '{mode}'. Expected 'task' or 'process'."
                )),
            });
        }

        let model_override = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let agent_name = args
            .get("agent")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let explicit_recipient = args
            .get("recipient")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let spawn_scope = parse_spawn_scope(&args);
        let parent_exec_ctx = current_spawn_execution_context();
        let spawn_depth = parent_exec_ctx
            .as_ref()
            .map_or(0, |ctx| ctx.spawn_depth.saturating_add(1));
        let parent_run_id = parent_exec_ctx.as_ref().map(|ctx| ctx.run_id.clone());
        let session_scope_key =
            spawn_session_scope_key(parent_exec_ctx.as_ref(), spawn_scope.as_ref());

        {
            let runs = self.active_runs.read().await;
            let active_count = running_run_count(&runs);
            if active_count >= self.spawn_config.max_concurrent {
                tracing::warn!(
                    active_count,
                    max_concurrent = self.spawn_config.max_concurrent,
                    "sessions_spawn rejected: max concurrent runs reached"
                );
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "sessions_spawn rejected: max_concurrent={} reached",
                        self.spawn_config.max_concurrent
                    )),
                });
            }

            if spawn_depth > self.spawn_config.max_spawn_depth {
                tracing::warn!(
                    spawn_depth,
                    max_spawn_depth = self.spawn_config.max_spawn_depth,
                    "sessions_spawn rejected: max spawn depth exceeded"
                );
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "sessions_spawn rejected: spawn depth {} exceeds max_spawn_depth={}",
                        spawn_depth, self.spawn_config.max_spawn_depth
                    )),
                });
            }

            let same_session_children = runs
                .iter()
                .filter(|run| {
                    matches!(run.status, SubAgentStatus::Running)
                        && run.session_scope_key == session_scope_key
                })
                .count();
            if same_session_children >= self.spawn_config.max_children_per_agent {
                tracing::warn!(
                    same_session_children,
                    max_children_per_agent = self.spawn_config.max_children_per_agent,
                    session_scope_key = %session_scope_key,
                    "sessions_spawn rejected: max children per session reached"
                );
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "sessions_spawn rejected: max_children_per_agent={} reached",
                        self.spawn_config.max_children_per_agent
                    )),
                });
            }
        }

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

        let selected_agent = match agent_name.as_deref() {
            Some(name) => match self.agents.get(name) {
                Some(cfg) => {
                    if !cfg.spawn_enabled.unwrap_or(true) {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!(
                                "Agent '{name}' is not allowed for sessions_spawn (spawn_enabled=false)."
                            )),
                        });
                    }
                    Some((name.to_string(), cfg.clone()))
                }
                None => {
                    let mut available = self
                        .agents
                        .iter()
                        .filter(|(_, cfg)| cfg.spawn_enabled.unwrap_or(true))
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>();
                    available.sort_unstable();
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Unknown agent '{name}'. Available agents: {}",
                            if available.is_empty() {
                                "(none configured)".to_string()
                            } else {
                                available.join(", ")
                            }
                        )),
                    });
                }
            },
            None => None,
        };

        // Resolve the recipient: explicit arg > default_recipient
        let recipient = match explicit_recipient {
            Some(r) => Some(r),
            None => self.default_recipient.read().await.clone(),
        };

        let resolved_provider_name = selected_agent
            .as_ref()
            .map(|(_, cfg)| cfg.provider.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.provider_name.clone());
        let resolved_model = model_override.unwrap_or_else(|| {
            selected_agent
                .as_ref()
                .map(|(_, cfg)| cfg.model.trim().to_string())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| self.model.clone())
        });
        let resolved_temperature = selected_agent
            .as_ref()
            .and_then(|(_, cfg)| cfg.temperature)
            .unwrap_or(self.temperature);
        let resolved_api_key = selected_agent
            .as_ref()
            .and_then(|(_, cfg)| {
                cfg.api_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
            .or_else(|| self.fallback_api_key.clone());
        let configured_max = selected_agent
            .as_ref()
            .map(|(_, cfg)| cfg.max_iterations.max(1))
            .unwrap_or(SUB_AGENT_MAX_ITERATIONS);
        let resolved_max_iterations = if let Some(dynamic_max) = args
            .get("max_iterations")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
        {
            // Treat the configured limit as a hard cap — callers cannot exceed
            // the per-agent policy even if they explicitly request more.
            dynamic_max.max(1).min(configured_max)
        } else {
            configured_max
        };

        if mode == "process" {
            let temperature = resolved_temperature;
            let history_arc: Arc<RwLock<Vec<HistoryEntry>>> = Arc::new(RwLock::new(Vec::new()));

            {
                let mut runs = self.active_runs.write().await;
                runs.push(SubAgentRun {
                    id: run_id.clone(),
                    task: task.to_string(),
                    started_at: Utc::now(),
                    status: SubAgentStatus::Running,
                    recipient: recipient.clone(),
                    abort_handle: None,
                    history: history_arc,
                    steer_tx: None,
                    parent_run_id: parent_run_id.clone(),
                    session_scope_key: session_scope_key.clone(),
                    spawn_depth,
                });
            }

            let model = resolved_model;
            let provider_name = resolved_provider_name;
            let api_key = resolved_api_key;
            let max_iterations = resolved_max_iterations;
            let workspace_root = self.workspace_dir.clone();
            let worker_workspace_root = self
                .spawn_config
                .worker_workspace_root
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| {
                    let path = std::path::PathBuf::from(value);
                    if path.is_absolute() {
                        path
                    } else {
                        self.workspace_dir.join(path)
                    }
                })
                .unwrap_or_else(|| self.workspace_dir.join("workers"));
            let active_runs = self.active_runs.clone();
            let channel = self.channel.clone();
            let keep_workspace = !self.spawn_config.cleanup_on_complete;
            let allowed_tools = selected_agent
                .as_ref()
                .map(|(_, cfg)| {
                    cfg.allowed_tools
                        .iter()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let identity_dir = selected_agent.as_ref().and_then(|(_, cfg)| {
                cfg.identity_dir
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            });
            let task_owned = task.to_string();
            let rid = run_id.clone();
            let process_scope = spawn_scope.clone();
            let process_parent_run_id = parent_run_id.clone();
            let process_session_scope_key = session_scope_key.clone();
            let process_spawn_depth = spawn_depth;
            let process_compaction_config = self.compaction_config.clone();
            let process_execution_ctx = SpawnExecutionContext {
                run_id: rid.clone(),
                session_scope_key: session_scope_key.clone(),
                spawn_depth,
            };

            let jh = tokio::spawn(SPAWN_EXECUTION_CONTEXT.scope(
                process_execution_ctx,
                async move {
                    tracing::info!(run_id = %rid, "Sub-agent process starting");

                    let worker_result = run_sub_agent_process(
                        &rid,
                        &task_owned,
                        &provider_name,
                        &model,
                        api_key.as_deref(),
                        temperature,
                        timeout_secs,
                        max_iterations,
                        &workspace_root,
                        &worker_workspace_root,
                        identity_dir.as_deref(),
                        &allowed_tools,
                        keep_workspace,
                        process_scope.as_ref(),
                        process_spawn_depth,
                        &process_session_scope_key,
                        process_parent_run_id.as_deref(),
                        &process_compaction_config,
                    )
                    .await;

                    let (status, result_text) = match worker_result {
                        Ok(result) if result.success => (
                            SubAgentStatus::Completed(result.output.clone()),
                            result.output,
                        ),
                        Ok(result) => {
                            let error = result.error.unwrap_or_else(|| "worker failed".to_string());
                            let msg = format!("Sub-agent error: {error}");
                            (SubAgentStatus::Failed(error), msg)
                        }
                        Err(error) => {
                            let msg = format!("Sub-agent process error: {error}");
                            (SubAgentStatus::Failed(error.to_string()), msg)
                        }
                    };

                    let announce = format_announce_message(&rid, &status, &result_text);

                    {
                        let mut runs = active_runs.write().await;
                        if let Some(run) = runs.iter_mut().find(|r| r.id == rid) {
                            run.status = status;
                            run.steer_tx = None;
                        }
                    }

                    if let Some(target) = recipient {
                        let msg = SendMessage::new(&announce, &target);
                        if let Err(error) = channel.send(&msg).await {
                            tracing::error!(
                                run_id = %rid,
                                "Failed to announce sub-agent process result: {error}"
                            );
                        }
                    }

                    tracing::info!(run_id = %rid, "Sub-agent process finished");
                },
            ));

            {
                let mut runs = self.active_runs.write().await;
                if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
                    run.abort_handle = Some(jh.abort_handle());
                }
            }

            return Ok(ToolResult {
                success: true,
                output: format!(
                    "Sub-agent spawned in process mode (run_id: {run_id}). Will announce result when complete."
                ),
                error: None,
            });
        }

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
                recipient: recipient.clone(),
                abort_handle: None,
                history: history_arc.clone(),
                steer_tx: Some(steer_tx),
                parent_run_id: parent_run_id.clone(),
                session_scope_key: session_scope_key.clone(),
                spawn_depth,
            });
        }

        // Clone everything the spawned task needs
        let channel = self.channel.clone();
        let provider_name = resolved_provider_name;
        let model = resolved_model;
        let temperature = resolved_temperature;
        let max_iterations = resolved_max_iterations;
        let provider = if selected_agent.is_some() && provider_name != self.provider_name {
            match providers::create_provider_with_options(
                &provider_name,
                resolved_api_key.as_deref(),
                &self.provider_runtime_options,
            ) {
                Ok(provider) => Arc::<dyn Provider>::from(provider),
                Err(error) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Failed to create provider '{provider_name}' for sessions_spawn: {error}"
                        )),
                    });
                }
            }
        } else {
            self.provider.clone()
        };
        let active_runs = self.active_runs.clone();
        let rid = run_id.clone();
        let task_owned = task.to_string();
        let tools = self.tools.get().cloned();
        let workspace_dir = self.workspace_dir.clone();
        let multimodal_config = self.multimodal_config.clone();
        let security = self.security.clone();
        let task_scope = spawn_scope.clone();
        let compaction_config = self.compaction_config.clone();
        let task_execution_ctx = SpawnExecutionContext {
            run_id: rid.clone(),
            session_scope_key: session_scope_key.clone(),
            spawn_depth,
        };
        let (system_prompt, filtered_tools) = if let Some((agent, cfg)) = selected_agent {
            let identity_prompt = cfg
                .identity_dir
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|identity_dir| build_identity_prompt(&workspace_dir.join(identity_dir)))
                .unwrap_or_default();
            let prompt = if identity_prompt.trim().is_empty() {
                DEFAULT_SUB_AGENT_SYSTEM_PROMPT.to_string()
            } else {
                identity_prompt
            };
            let memory_scope = parse_memory_scope(cfg.memory_scope.as_deref())?;
            let tools = tools.map(|registry| {
                resolve_tools_for_agent(
                    registry,
                    &agent,
                    memory_scope,
                    if cfg.allowed_tools.is_empty() {
                        None
                    } else {
                        Some(&cfg.allowed_tools)
                    },
                )
            });
            (prompt, tools)
        } else {
            (DEFAULT_SUB_AGENT_SYSTEM_PROMPT.to_string(), tools)
        };

        // Spawn async task (fire-and-forget); capture handle to support kill
        let jh = tokio::spawn(
            SPAWN_EXECUTION_CONTEXT.scope(task_execution_ctx, async move {
                tracing::info!(run_id = %rid, "Sub-agent task starting");
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(timeout_secs),
                    run_sub_agent_task(
                        &task_owned,
                        provider,
                        &provider_name,
                        &model,
                        temperature,
                        filtered_tools,
                        &system_prompt,
                        &workspace_dir,
                        security,
                        &multimodal_config,
                        &compaction_config,
                        max_iterations,
                        steer_rx,
                        history_arc,
                        task_scope,
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

                let announce = format_announce_message(&rid, &status, &result_text);

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
            }),
        );

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
                let parent = r.parent_run_id.as_deref().unwrap_or("root");
                format!(
                    "• `{}` [{age}s ago] {status}\n  task: {}\n  depth: {} | parent: {}",
                    r.id, r.task, r.spawn_depth, parent
                )
            })
            .collect();

        Ok(ToolResult {
            success: true,
            output: format!(
                "Sub-agent runs ({} total):\n\n{}",
                runs.len(),
                lines.join("\n\n")
            ),
            error: None,
        })
    }

    /// Kill a running sub-agent by its run ID.
    async fn execute_kill(&self, run_id: &str) -> anyhow::Result<ToolResult> {
        let (recipient_opt, rid) = {
            let mut runs = self.active_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
                match &run.status {
                    SubAgentStatus::Running => {
                        if let Some(ah) = run.abort_handle.as_ref() {
                            ah.abort();
                        }
                        let recipient = run.recipient.clone();
                        let rid = run.id.clone();
                        run.status = SubAgentStatus::Failed("killed by user".into());
                        run.steer_tx = None;
                        (recipient, rid)
                    }
                    SubAgentStatus::Completed(_) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Run `{run_id}` already completed.")),
                        });
                    }
                    SubAgentStatus::Failed(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Run `{run_id}` already failed: {e}")),
                        });
                    }
                }
            } else {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("No run found with ID `{run_id}`.")),
                });
            }
        };

        if let Some(target) = recipient_opt {
            let msg_text = format!("🤖 Sub-agent `{rid}` was killed by user.");
            let msg = SendMessage::new(&msg_text, &target);
            if let Err(error) = self.channel.send(&msg).await {
                tracing::error!(run_id = %rid, "Failed to announce sub-agent kill: {error}");
            }
        } else {
            tracing::warn!(
                run_id = %rid,
                "Sub-agent was killed but no recipient configured for announcement"
            );
        }

        Ok(ToolResult {
            success: true,
            output: format!("Sub-agent `{run_id}` has been killed."),
            error: None,
        })
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
                error: Some(format!(
                    "Run `{run_id}` already failed ({e}); cannot steer."
                )),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryScope {
    Shared,
    Isolated,
}

fn parse_memory_scope(scope: Option<&str>) -> anyhow::Result<MemoryScope> {
    match scope
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|s| !s.is_empty())
        .as_deref()
    {
        None | Some("shared") => Ok(MemoryScope::Shared),
        Some("isolated") => Ok(MemoryScope::Isolated),
        Some(other) => {
            anyhow::bail!("Invalid memory_scope '{other}'. Expected 'shared' or 'isolated'.")
        }
    }
}

fn memory_key_prefix(agent_name: &str, key: &str) -> String {
    if key.starts_with(&format!("{agent_name}:")) {
        key.to_string()
    } else {
        format!("{agent_name}:{key}")
    }
}

fn format_announce_message(rid: &str, status: &SubAgentStatus, result_text: &str) -> String {
    match status {
        SubAgentStatus::Completed(_) => {
            format!("🤖 Sub-agent `{rid}` completed:\n\n{result_text}")
        }
        _ => format!("🤖 Sub-agent `{rid}` FAILED:\n\n{result_text}"),
    }
}

#[derive(Clone)]
struct AllowedToolProxy {
    source: Arc<Vec<Box<dyn Tool>>>,
    public_name: String,
    memory_prefix: Option<String>,
}

impl AllowedToolProxy {
    fn find_source_tool(&self) -> Option<&dyn Tool> {
        self.source
            .iter()
            .find(|tool| tool.supports_name(&self.public_name))
            .map(|tool| tool.as_ref())
    }
}

#[async_trait]
impl Tool for AllowedToolProxy {
    fn name(&self) -> &str {
        &self.public_name
    }

    fn description(&self) -> &str {
        self.find_source_tool()
            .map(|tool| tool.description())
            .unwrap_or("Unavailable proxied tool")
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.find_source_tool()
            .map(|tool| tool.parameters_schema())
            .unwrap_or_else(|| {
                json!({
                    "type": "object",
                    "description": "Unavailable proxied tool"
                })
            })
    }

    async fn execute(&self, mut args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if self.public_name == "memory_store" {
            if let Some(prefix) = &self.memory_prefix {
                if let Some(key) = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                {
                    args["key"] = serde_json::Value::String(memory_key_prefix(prefix, key));
                }
            }
        }

        let Some(tool) = self.find_source_tool() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Tool '{}' is not registered.", self.public_name)),
            });
        };

        tool.execute_named(&self.public_name, args).await
    }
}

fn resolve_tools_for_agent(
    source: Arc<Vec<Box<dyn Tool>>>,
    agent_name: &str,
    memory_scope: MemoryScope,
    allowed_tools: Option<&[String]>,
) -> Arc<Vec<Box<dyn Tool>>> {
    let allowlist = allowed_tools.map(|items| {
        items
            .iter()
            .map(|item| item.trim())
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
    });

    let mut selected_names = Vec::new();
    if let Some(list) = allowlist {
        for name in list {
            if !selected_names.contains(&name) {
                selected_names.push(name);
            }
        }
    } else {
        for tool in source.iter() {
            let name = tool.name();
            if !selected_names.contains(&name) {
                selected_names.push(name);
            }
        }
    }

    let memory_prefix = if memory_scope == MemoryScope::Isolated {
        Some(agent_name.to_string())
    } else {
        None
    };

    let resolved = selected_names
        .into_iter()
        .map(|name| {
            Box::new(AllowedToolProxy {
                source: source.clone(),
                public_name: name.to_string(),
                memory_prefix: memory_prefix.clone(),
            }) as Box<dyn Tool>
        })
        .collect::<Vec<_>>();

    Arc::new(resolved)
}

/// Maximum tool-call iterations for a sub-agent run (per steering segment).
const SUB_AGENT_MAX_ITERATIONS: usize = 200;

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
    system_prompt: &str,
    workspace_dir: &std::path::Path,
    security: Arc<SecurityPolicy>,
    multimodal_config: &MultimodalConfig,
    compaction_config: &AgentCompactionConfig,
    max_iterations: usize,
    mut steer_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    history_out: Arc<RwLock<Vec<HistoryEntry>>>,
    scope: Option<SpawnScope>,
) -> anyhow::Result<String> {
    // --- No-tools fallback: single-turn completion ---
    let Some(tools_registry) = tools else {
        let response = provider
            .chat_with_system(Some(system_prompt), task, model, temperature)
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
    let mut history: Vec<ChatMessage> =
        vec![ChatMessage::system(system_prompt), ChatMessage::user(task)];

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
        let cc = compaction_config.clone();
        let ct = cancel_token.clone();
        let security = security.clone();
        let scope_owned = scope.clone();

        let mut loop_handle = tokio::spawn(async move {
            let observer = NoopObserver;
            let hooks = HookManager::new(wd);
            let scope_ctx = scope_owned.as_ref().map(|scope| ScopeContext {
                policy: &security,
                sender: scope.sender.as_str(),
                channel: scope.channel.as_str(),
                chat_type: scope.chat_type.as_str(),
                chat_id: scope.chat_id.as_str(),
                policy_pipeline: None,
            });
            let result = run_tool_call_loop(
                p.as_ref(),
                &mut h,
                tr.as_slice(),
                &observer,
                &hooks,
                &pn,
                &m,
                t,
                true, // silent — no streaming output
                None, // no approval manager
                "sessions_spawn",
                &mc,
                max_iterations,
                Some(&cc),
                Some(ct),
                None, // no streaming sender
                scope_ctx.as_ref(),
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

fn copy_dir_recursive(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = destination.join(entry.file_name());
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if metadata.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn build_session_worker_cli_args(manifest: &WorkerManifest) -> anyhow::Result<Vec<String>> {
    let tools_json = serde_json::to_string(&manifest.allowed_tools)?;
    Ok(vec![
        "session-worker".to_string(),
        "--task".to_string(),
        manifest.task.clone(),
        "--workspace".to_string(),
        manifest.workspace_dir.display().to_string(),
        "--memory-db".to_string(),
        manifest.memory_db_path.display().to_string(),
        "--tools".to_string(),
        tools_json,
        "--timeout".to_string(),
        manifest.timeout_seconds.to_string(),
    ])
}

async fn wait_with_parent_timeout(
    child: &mut tokio::process::Child,
    parent_timeout: std::time::Duration,
) -> anyhow::Result<std::process::ExitStatus> {
    match tokio::time::timeout(parent_timeout, child.wait()).await {
        Ok(result) => Ok(result?),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!(
                "session-worker exceeded parent timeout of {}s and was killed",
                parent_timeout.as_secs()
            );
        }
    }
}

async fn run_sub_agent_process(
    run_id: &str,
    task: &str,
    provider_name: &str,
    model: &str,
    api_key: Option<&str>,
    temperature: f64,
    timeout_secs: u64,
    max_iterations: usize,
    workspace_root: &std::path::Path,
    worker_workspace_root: &std::path::Path,
    agent_identity_dir: Option<&str>,
    allowed_tools: &[String],
    keep_workspace: bool,
    scope: Option<&SpawnScope>,
    spawn_depth: usize,
    session_scope_key: &str,
    parent_run_id: Option<&str>,
    compaction_config: &AgentCompactionConfig,
) -> anyhow::Result<WorkerResult> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let worker_workspace = worker_workspace_root.join(run_id);
    std::fs::create_dir_all(&worker_workspace)?;

    let identity_dir = if let Some(identity_dir) = agent_identity_dir {
        let source_identity = workspace_root.join(identity_dir);
        if source_identity.exists() {
            let copied_identity_dir = worker_workspace.join("identity");
            if source_identity.is_dir() {
                copy_dir_recursive(&source_identity, &copied_identity_dir)?;
            } else {
                std::fs::create_dir_all(&copied_identity_dir)?;
                let target = copied_identity_dir.join(
                    source_identity
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new("identity.txt")),
                );
                std::fs::copy(source_identity, target)?;
            }
            Some("identity".to_string())
        } else {
            None
        }
    } else {
        None
    };

    let manifest = WorkerManifest {
        run_id: run_id.to_string(),
        task: task.to_string(),
        provider_name: provider_name.to_string(),
        model: model.to_string(),
        api_key: api_key.map(str::to_string),
        temperature,
        workspace_dir: worker_workspace.clone(),
        memory_db_path: worker_workspace.join("brain.db"),
        allowed_tools: allowed_tools.to_vec(),
        timeout_seconds: timeout_secs,
        max_iterations,
        system_prompt: None,
        identity_dir,
        scope_sender: scope.map(|ctx| ctx.sender.clone()),
        scope_channel: scope.map(|ctx| ctx.channel.clone()),
        scope_chat_type: scope.map(|ctx| ctx.chat_type.clone()),
        scope_chat_id: scope.map(|ctx| ctx.chat_id.clone()),
        spawn_depth,
        session_scope_key: session_scope_key.to_string(),
        parent_run_id: parent_run_id.map(str::to_string),
        compaction_config: Some(compaction_config.clone()),
    };

    let executable = std::env::current_exe()?;
    let cli_args = build_session_worker_cli_args(&manifest)?;
    let mut command = tokio::process::Command::new(executable);
    command
        .args(cli_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let payload = serde_json::to_string(&manifest)?;
        stdin.write_all(payload.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
    }

    let parent_timeout = std::time::Duration::from_secs(timeout_secs.max(1));
    let mut stdout_stream = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("session-worker stdout pipe was not configured"))?;
    let mut stderr_stream = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("session-worker stderr pipe was not configured"))?;

    let process_outcome = tokio::time::timeout(parent_timeout, async {
        let stdout_future = async {
            let mut stdout = Vec::new();
            stdout_stream.read_to_end(&mut stdout).await?;
            Ok::<Vec<u8>, anyhow::Error>(stdout)
        };
        let stderr_future = async {
            let mut stderr = Vec::new();
            stderr_stream.read_to_end(&mut stderr).await?;
            Ok::<Vec<u8>, anyhow::Error>(stderr)
        };

        let (status_result, stdout_result, stderr_result) = tokio::join!(
            wait_with_parent_timeout(&mut child, parent_timeout),
            stdout_future,
            stderr_future
        );

        let status = status_result?;
        let stdout = stdout_result?;
        let stderr = stderr_result?;
        Ok::<(std::process::ExitStatus, Vec<u8>, Vec<u8>), anyhow::Error>((status, stdout, stderr))
    })
    .await;

    let (status, stdout, stderr) = match process_outcome {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!(
                "session-worker process pipeline exceeded timeout of {}s",
                parent_timeout.as_secs()
            );
        }
    };
    let output = std::process::Output {
        status,
        stdout,
        stderr,
    };
    let stdout_raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr_raw = String::from_utf8_lossy(&output.stderr).trim().to_string();

    let parsed: WorkerResult = serde_json::from_str(&stdout_raw).map_err(|error| {
        anyhow::anyhow!(
            "Failed to parse session-worker output: {error}; status={:?}; stderr={}",
            output.status.code(),
            stderr_raw
        )
    })?;

    if !keep_workspace {
        if let Err(error) = std::fs::remove_dir_all(&worker_workspace) {
            tracing::warn!(
                run_id = run_id,
                "Failed to cleanup worker workspace {}: {error}",
                worker_workspace.display()
            );
        }
    }

    if !output.status.success() && parsed.success {
        return Err(anyhow::anyhow!(
            "session-worker exited with status {:?} despite success result",
            output.status.code()
        ));
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
    use crate::security::SecurityPolicy;
    use anyhow::anyhow;
    use std::collections::HashMap;
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

    struct EchoSystemProvider;

    #[async_trait::async_trait]
    impl crate::providers::Provider for EchoSystemProvider {
        async fn chat_with_system(
            &self,
            system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(system.unwrap_or_default().to_string())
        }

        async fn chat(
            &self,
            _request: crate::providers::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            Ok(crate::providers::ChatResponse {
                text: Some(String::new()),
                tool_calls: Vec::new(),
            })
        }
    }

    fn make_agent_config(identity_dir: Option<String>) -> DelegateAgentConfig {
        DelegateAgentConfig {
            provider: "test-provider".to_string(),
            model: "agent-model".to_string(),
            system_prompt: None,
            api_key: None,
            temperature: None,
            max_depth: 3,
            agentic: false,
            allowed_tools: Vec::new(),
            max_iterations: 10,
            identity_dir,
            memory_scope: None,
            spawn_enabled: None,
        }
    }

    fn make_tool(
        channel: Arc<dyn Channel>,
        provider: Arc<dyn crate::providers::Provider>,
    ) -> SessionsSpawnTool {
        make_tool_with_spawn_config(
            channel,
            provider,
            crate::config::SessionsSpawnConfig::default(),
        )
    }

    fn make_tool_with_spawn_config(
        channel: Arc<dyn Channel>,
        provider: Arc<dyn crate::providers::Provider>,
        spawn_config: crate::config::SessionsSpawnConfig,
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
            crate::config::AgentCompactionConfig::default(),
            HashMap::new(),
            None,
            crate::providers::ProviderRuntimeOptions::default(),
            spawn_config,
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
        assert!(
            required.is_empty(),
            "Required should be empty (validated at runtime)"
        );
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["task"].is_object());
        assert!(schema["properties"]["run_id"].is_object());
        assert!(schema["properties"]["message"].is_object());
        assert!(schema["properties"]["model"].is_object());
        assert!(schema["properties"]["agent"].is_object());
        assert!(schema["properties"]["timeout_seconds"].is_object());
        assert!(schema["properties"]["mode"].is_object());
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
        let result = tool.execute(json!({"task": "   "})).await.unwrap();
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

        let result = tool.execute(json!({"task": "Do something"})).await.unwrap();
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
    async fn spawn_rejected_when_max_concurrent_reached() {
        let (ch, _) = RecordingChannel::new();
        let mut spawn_cfg = crate::config::SessionsSpawnConfig::default();
        spawn_cfg.max_concurrent = 0;
        let tool = make_tool_with_spawn_config(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
            spawn_cfg,
        );

        let result = tool.execute(json!({"task": "blocked"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("max_concurrent"));
    }

    #[tokio::test]
    async fn spawn_rejected_when_depth_exceeded() {
        let (ch, _) = RecordingChannel::new();
        let mut spawn_cfg = crate::config::SessionsSpawnConfig::default();
        spawn_cfg.max_spawn_depth = 0;
        let tool = make_tool_with_spawn_config(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
            spawn_cfg,
        );

        let result = SPAWN_EXECUTION_CONTEXT
            .scope(
                SpawnExecutionContext {
                    run_id: "parent-run".to_string(),
                    session_scope_key: "signal:group:test".to_string(),
                    spawn_depth: 0,
                },
                async { tool.execute(json!({"task": "nested"})).await },
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("max_spawn_depth"));
    }

    #[tokio::test]
    async fn spawn_rejected_when_max_children_per_session_reached() {
        let (ch, _) = RecordingChannel::new();
        let mut spawn_cfg = crate::config::SessionsSpawnConfig::default();
        spawn_cfg.max_children_per_agent = 0;
        let tool = make_tool_with_spawn_config(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
            spawn_cfg,
        );

        let result = tool
            .execute(json!({
                "task": "child",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "sender": "openprx_user",
                    "channel": "signal",
                    "chat_type": "direct",
                    "chat_id": "+15551234567"
                }
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("max_children_per_agent"));
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
        let _ = tool.execute(json!({"task": "Some task"})).await.unwrap();

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
        let result = tool.execute(json!({"action": "history"})).await;
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
        let spawn_result = tool.execute(json!({"task": "Do a thing"})).await.unwrap();
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

    #[tokio::test]
    async fn spawn_rejects_unknown_agent() {
        let (ch, _) = RecordingChannel::new();
        let mut agents = HashMap::new();
        agents.insert("alpha".to_string(), make_agent_config(None));
        let tool = SessionsSpawnTool::new(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
            "test-provider",
            "test-model",
            0.7,
            test_security(),
            std::path::PathBuf::from("/tmp"),
            crate::config::MultimodalConfig::default(),
            crate::config::AgentCompactionConfig::default(),
            agents,
            None,
            crate::providers::ProviderRuntimeOptions::default(),
            crate::config::SessionsSpawnConfig::default(),
        );
        let result = tool
            .execute(json!({"task": "hello", "agent": "missing"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Unknown agent"));
    }

    #[tokio::test]
    async fn spawn_rejects_spawn_disabled_agent() {
        let (ch, _) = RecordingChannel::new();
        let mut agents = HashMap::new();
        let mut cfg = make_agent_config(None);
        cfg.spawn_enabled = Some(false);
        agents.insert("alpha".to_string(), cfg);
        let tool = SessionsSpawnTool::new(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "ok".into(),
            }),
            "test-provider",
            "test-model",
            0.7,
            test_security(),
            std::path::PathBuf::from("/tmp"),
            crate::config::MultimodalConfig::default(),
            crate::config::AgentCompactionConfig::default(),
            agents,
            None,
            crate::providers::ProviderRuntimeOptions::default(),
            crate::config::SessionsSpawnConfig::default(),
        );
        let result = tool
            .execute(json!({"task": "hello", "agent": "alpha"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("spawn_enabled=false"));
    }

    #[tokio::test]
    async fn spawn_agent_uses_identity_prompt() {
        let ws = tempfile::TempDir::new().unwrap();
        let identity_dir = ws.path().join("identities/alpha");
        std::fs::create_dir_all(&identity_dir).unwrap();
        std::fs::write(identity_dir.join("SOUL.md"), "Identity Soul").unwrap();

        let mut agents = HashMap::new();
        agents.insert(
            "alpha".to_string(),
            make_agent_config(Some("identities/alpha".to_string())),
        );

        let (ch, sent) = RecordingChannel::new();
        let tool = SessionsSpawnTool::new(
            Arc::new(ch),
            Arc::new(EchoSystemProvider),
            "test-provider",
            "test-model",
            0.7,
            test_security(),
            ws.path().to_path_buf(),
            crate::config::MultimodalConfig::default(),
            crate::config::AgentCompactionConfig::default(),
            agents,
            None,
            crate::providers::ProviderRuntimeOptions::default(),
            crate::config::SessionsSpawnConfig::default(),
        );
        tool.set_default_recipient(Some("test-recipient".to_string()))
            .await;

        let result = tool
            .execute(json!({"task": "t", "agent": "alpha"}))
            .await
            .unwrap();
        assert!(result.success);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let messages = sent.lock().await;
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("### SOUL.md"));
        assert!(messages[0].contains("Identity Soul"));
    }

    #[test]
    fn process_mode_task_arg_is_not_json_encoded() {
        let manifest = WorkerManifest {
            run_id: "run".to_string(),
            task: "say \"hello\"".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            temperature: 0.7,
            workspace_dir: std::path::PathBuf::from("/tmp/ws"),
            memory_db_path: std::path::PathBuf::from("/tmp/ws/brain.db"),
            allowed_tools: vec!["shell".to_string()],
            timeout_seconds: 30,
            max_iterations: 20,
            system_prompt: None,
            identity_dir: None,
            scope_sender: None,
            scope_channel: None,
            scope_chat_type: None,
            scope_chat_id: None,
            spawn_depth: 0,
            session_scope_key: "sessions_spawn:global".to_string(),
            parent_run_id: None,
            compaction_config: None,
        };

        let args = build_session_worker_cli_args(&manifest).unwrap();
        let task_index = args.iter().position(|arg| arg == "--task").unwrap();
        assert_eq!(args[task_index + 1], manifest.task);
    }

    #[tokio::test]
    async fn process_mode_parent_timeout_kills_stuck_process() {
        let mut command = tokio::process::Command::new("sleep");
        command.arg("5");
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();

        let result =
            wait_with_parent_timeout(&mut child, std::time::Duration::from_millis(50)).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("session-worker exceeded parent timeout"));
    }

    #[test]
    fn isolated_memory_prefixes_key() {
        assert_eq!(memory_key_prefix("alpha", "plan"), "alpha:plan");
        assert_eq!(memory_key_prefix("alpha", "alpha:plan"), "alpha:plan");
    }
}
