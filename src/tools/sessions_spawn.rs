//! Async sub-agent spawning tool — fire-and-forget with auto-announce on completion.
//!
//! Aligns with OpenClaw's `sessions_spawn` pattern:
//! - Accepts a task description and optional model/timeout
//! - Spawns a tokio task that runs an isolated agent loop
//! - Returns immediately with a run ID
//! - On completion, sends the result back through the channel automatically
//! - `history` action: view the conversation log of any sub-agent run
//! - `steer` action: inject a message into a running sub-agent's context

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::agent::loop_::{DocumentIngestRuntime, ScopeContext, SpawnEventSink, ToolConcurrencyGovernanceConfig};
use crate::channels::build_identity_prompt;
use crate::channels::traits::{Channel, SendMessage};
use crate::config::{AgentCompactionConfig, DelegateAgentConfig, MultimodalConfig, SessionsSpawnConfig};
use crate::hooks::HookManager;
use crate::memory::{Memory, MemoryEventRecording, MemoryFabric, MessageEventScope};
use crate::observability::NoopObserver;
use crate::providers::{self, ChatMessage, Provider};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
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
///
/// A value of `0` means "no timeout" (run until natural completion), matching
/// the session-worker semantics in `session_worker/runner.rs`.
const DEFAULT_SUB_AGENT_TIMEOUT_SECS: u64 = 600;
const DEFAULT_SUB_AGENT_SYSTEM_PROMPT: &str = "\
You are a sub-agent handling a specific delegated task. \
Complete the task thoroughly and report results concisely. \
Focus only on the assigned task; do not ask clarifying questions.";
const PROCESS_MEMORY_STRATEGY_SHARED: &str = "shared_fabric";
const PROCESS_MEMORY_STRATEGY_ISOLATED: &str = "isolated_private";
const PROCESS_MEMORY_STRATEGY_HYBRID: &str = "hybrid";

/// Status of a spawned sub-agent run.
#[derive(Debug, Clone)]
pub enum SubAgentStatus {
    Running,
    /// The run suspended on a tool call that requires an operator approval
    /// decision (NeedsInput). `prompt` is a short human-readable description of
    /// what is awaiting approval. This is a reversible, non-terminal state: once
    /// the operator decides (`/approve` / `/deny`) or the approval times out, the
    /// run returns to [`Running`](Self::Running) (or fails) and continues.
    AwaitingInput {
        prompt: String,
    },
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
    pub owner_id: Option<String>,
    pub topic_id: Option<String>,
    pub source_message_event_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub status: SubAgentStatus,
    pub recipient: Option<String>,
    /// Name of the channel this run must announce/kill-notify back on.
    ///
    /// Captured **per-turn** from the originating message's scope at spawn time
    /// (atomic with `recipient`), so announce/kill always route to the channel +
    /// recipient of the message that launched this run — never to a shared
    /// "active channel" that a concurrently-processed message may have
    /// overwritten. `None` falls back to the construction-time active channel.
    pub channel_name: Option<String>,
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
pub(crate) struct SpawnExecutionContext {
    pub(crate) run_id: String,
    pub(crate) session_scope_key: String,
    pub(crate) spawn_depth: usize,
    pub(crate) owner_id: Option<String>,
    pub(crate) topic_id: Option<String>,
    pub(crate) source_message_event_id: Option<String>,
    /// D8-4: distinguishes a *turn root* context (seeded at a top-level
    /// channel/chat/agent turn so its directly-spawned children inherit
    /// `parent_run_id`) from a *spawn run* context (a sub-agent run that may
    /// itself spawn). The turn root represents "the turn itself, before any
    /// spawn nesting": its first child must compute `spawn_depth` 0 — exactly as
    /// if no context were seeded — so seeding does not tighten the
    /// `max_spawn_depth` boundary. A spawn run's child computes `+1` as before.
    pub(crate) is_turn_root: bool,
}

impl SpawnExecutionContext {
    /// Seed a *turn root* context for a top-level channel/chat/agent turn. The
    /// per-turn `run_id` becomes the `parent_run_id` of any task this turn spawns
    /// directly, while `spawn_depth` starts at 0 and — because `is_turn_root` is
    /// true — the first child still computes depth 0 (no boundary tightening; see
    /// `spawn_depth` computation in `execute`).
    pub(crate) const fn seed_turn_context(run_id: String, session_scope_key: String) -> Self {
        Self {
            run_id,
            session_scope_key,
            spawn_depth: 0,
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            is_turn_root: true,
        }
    }
}

tokio::task_local! {
    pub(crate) static SPAWN_EXECUTION_CONTEXT: SpawnExecutionContext;
}

#[derive(Debug, Clone)]
struct SpawnScope {
    sender: String,
    channel: String,
    chat_type: String,
    chat_id: String,
    owner_id: Option<String>,
    topic_id: Option<String>,
    parent_task_id: Option<String>,
    source_message_event_id: Option<String>,
}

fn parse_spawn_scope(args: &serde_json::Value) -> Option<SpawnScope> {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return None;
    }

    let scope = args.get("_zc_scope").and_then(serde_json::Value::as_object)?;
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
        owner_id: scope
            .get("owner_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        topic_id: scope
            .get("topic_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        parent_task_id: scope
            .get("task_id")
            .or_else(|| scope.get("parent_task_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        source_message_event_id: scope
            .get("message_event_id")
            .or_else(|| scope.get("source_message_event_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

fn current_spawn_execution_context() -> Option<SpawnExecutionContext> {
    SPAWN_EXECUTION_CONTEXT.try_with(|ctx| ctx.clone()).ok()
}

fn spawn_session_scope_key(parent_ctx: Option<&SpawnExecutionContext>, scope: Option<&SpawnScope>) -> String {
    if let Some(parent) = parent_ctx {
        return parent.session_scope_key.clone();
    }

    if let Some(scope) = scope {
        return format!("{}:{}:{}", scope.channel, scope.chat_id, scope.sender);
    }

    "sessions_spawn:global".to_string()
}

#[derive(Debug, Clone, Default)]
struct SpawnLineage {
    owner_id: Option<String>,
    topic_id: Option<String>,
    parent_task_id: Option<String>,
    source_message_event_id: Option<String>,
}

fn spawn_lineage(
    event_scope: &MessageEventScope,
    parent_ctx: Option<&SpawnExecutionContext>,
    scope: Option<&SpawnScope>,
) -> SpawnLineage {
    SpawnLineage {
        owner_id: scope
            .and_then(|scope| scope.owner_id.clone())
            .or_else(|| parent_ctx.and_then(|ctx| ctx.owner_id.clone()))
            .or_else(|| event_scope.owner_id.clone()),
        topic_id: scope
            .and_then(|scope| scope.topic_id.clone())
            .or_else(|| parent_ctx.and_then(|ctx| ctx.topic_id.clone())),
        parent_task_id: parent_ctx
            .map(|ctx| ctx.run_id.clone())
            .or_else(|| scope.and_then(|scope| scope.parent_task_id.clone())),
        source_message_event_id: scope
            .and_then(|scope| scope.source_message_event_id.clone())
            .or_else(|| parent_ctx.and_then(|ctx| ctx.source_message_event_id.clone())),
    }
}

fn running_run_count(runs: &[SubAgentRun]) -> usize {
    runs.iter()
        // A suspended (AwaitingInput) run is still live — it holds a concurrency
        // slot until it resumes or is killed/times out — so it counts here.
        .filter(|run| {
            matches!(
                run.status,
                SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. }
            )
        })
        .count()
}

const fn status_label(status: &SubAgentStatus) -> &'static str {
    match status {
        SubAgentStatus::Running => "running",
        SubAgentStatus::AwaitingInput { .. } => "awaiting-input",
        SubAgentStatus::Completed(_) => "completed",
        SubAgentStatus::Failed(_) => "failed",
    }
}

/// Tool that spawns an asynchronous sub-agent to handle a task in isolation.
/// Returns immediately with a run ID; results are announced via the active channel
/// when the sub-agent completes.
pub struct SessionsSpawnTool {
    /// Channel for announcing sub-agent results.
    ///
    /// Wrapped in an `RwLock` and updated per-message via
    /// [`Tool::set_active_channel`] (driven by the channel/gateway loop) — exactly
    /// like `MessageSendTool` — so that a sub-agent's result is announced
    /// back on the *originating* channel (e.g. wacli for a WhatsApp group message)
    /// rather than the single fixed channel this tool was constructed with (which,
    /// in a multi-channel deployment, was a default such as Signal). Each spawn
    /// snapshots the active channel at request time, so the announcement routes to
    /// whichever channel launched the run.
    channel: Arc<RwLock<Arc<dyn Channel>>>,
    /// Registry of every configured channel, keyed by [`Channel::name`].
    ///
    /// announce/kill resolve the *originating* channel object from here using the
    /// `channel_name` captured per-turn on the [`SubAgentRun`] — the channel and
    /// recipient then both come from the same launching message's scope, atomic
    /// and immune to the shared-`channel` overwrite race that concurrent message
    /// processing (the channels JoinSet) would otherwise cause. Empty in unit
    /// tests / single-channel paths, where the shared `channel` fallback applies.
    channels: Arc<HashMap<String, Arc<dyn Channel>>>,
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
    /// Shared memory backend for normalized spawn lifecycle events.
    memory: Option<Arc<dyn Memory>>,
    event_recording: MemoryEventRecording,
    /// Optional event bridge sink. When set (chat `/bg` path), a task-mode
    /// sub-agent streams its incremental output + tool calls through a
    /// per-session drainer (provisioned by the chat side) into the chat UI's
    /// ring buffers (v1.1a). When `None` (channels/gateway path), spawns stay
    /// silent — zero behaviour change for those callers.
    event_sink: Option<SpawnEventSink>,
    /// Optional approval resolver factory. When set (chat `/bg` path only), a
    /// task-mode sub-agent that hits the supervised approval gate **suspends**
    /// (NeedsInput) awaiting an operator `/approve` / `/deny` decision instead of
    /// auto-failing. When `None` (channels/gateway path, or chat without the
    /// factory) the historical auto-fail-on-gate semantics are preserved.
    approval_resolver_factory: Option<crate::agent::loop_::SpawnApprovalResolverFactory>,
}

impl SessionsSpawnTool {
    /// Create a new `SessionsSpawnTool` with the given channel and provider.
    ///
    /// Thin wrapper over [`Self::new_with_registry`] that mints a fresh, empty
    /// `active_runs` registry owned solely by this tool. Behaviour is identical to
    /// the previous inline construction (channels/gateway call sites are
    /// unaffected).
    #[allow(clippy::too_many_arguments)]
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
        Self::new_with_registry(
            channel,
            provider,
            provider_name,
            model,
            temperature,
            security,
            workspace_dir,
            multimodal_config,
            compaction_config,
            agents,
            fallback_api_key,
            provider_runtime_options,
            spawn_config,
            Arc::new(RwLock::new(Vec::new())),
        )
    }

    /// Create a new `SessionsSpawnTool` backed by a caller-provided `active_runs`
    /// registry.
    ///
    /// Identical to [`Self::new`] except the `active_runs` registry is injected
    /// rather than freshly minted, letting a single owner (e.g. chat) build one
    /// `Arc<RwLock<Vec<SubAgentRun>>>` and share it across `sessions_spawn`,
    /// `sessions_list`, `sessions_send`, `session_status`, and a side-channel
    /// handle — the single-source-of-truth registry for the chat session runtime.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_registry(
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
        active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    ) -> Self {
        Self {
            channel: Arc::new(RwLock::new(channel)),
            channels: Arc::new(HashMap::new()),
            provider,
            provider_name: provider_name.into(),
            model: model.into(),
            temperature,
            security,
            default_recipient: Arc::new(RwLock::new(None)),
            active_runs,
            tools: Arc::new(OnceLock::new()),
            workspace_dir,
            multimodal_config,
            compaction_config,
            agents: Arc::new(agents),
            fallback_api_key,
            provider_runtime_options,
            spawn_config,
            memory: None,
            event_recording: MemoryEventRecording::default(),
            event_sink: None,
            approval_resolver_factory: None,
        }
    }

    /// Attach a [`SpawnEventSink`] so task-mode sub-agents spawned by this tool
    /// stream their incremental output and tool-call notifications to the chat UI
    /// (live read-only attach, v1.1a).
    ///
    /// Only the chat `/bg` path sets this; channels/gateway leave it `None` and
    /// keep spawning silently (zero behaviour change for those callers).
    #[must_use]
    pub fn with_event_sink(mut self, sink: SpawnEventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Attach a [`SpawnApprovalResolverFactory`](crate::agent::loop_::SpawnApprovalResolverFactory)
    /// so a task-mode sub-agent that hits the supervised approval gate suspends
    /// (NeedsInput) awaiting an operator decision instead of auto-failing.
    ///
    /// Only the chat `/bg` path sets this; channels/gateway leave it `None` and
    /// keep the historical auto-fail-on-gate semantics (no human is present to
    /// approve, so suspending would only create a zombie).
    // Called only by the binary crate's `chat::run` (the sole NeedsInput
    // opt-in), which is not part of a `--lib` build.
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn with_approval_resolver_factory(
        mut self,
        factory: crate::agent::loop_::SpawnApprovalResolverFactory,
    ) -> Self {
        self.approval_resolver_factory = Some(factory);
        self
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

    /// Attach the full set of configured channels, keyed by [`Channel::name`].
    ///
    /// This is the per-turn routing registry: announce/kill resolve the launching
    /// message's channel object from here via the `channel_name` recorded on each
    /// [`SubAgentRun`], so routing is bound atomically to the originating message
    /// rather than to the shared "active channel" (which a concurrently-processed
    /// message can overwrite). Channels not found here fall back to the shared
    /// active channel.
    #[must_use]
    pub fn with_channels(mut self, channels: Arc<HashMap<String, Arc<dyn Channel>>>) -> Self {
        self.channels = channels;
        self
    }

    /// Attach shared memory so spawned runs are visible in the live fabric.
    pub fn with_shared_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub const fn with_event_recording(mut self, event_recording: MemoryEventRecording) -> Self {
        self.event_recording = event_recording;
        self
    }

    /// Resolve the channel a run must announce/kill-notify on.
    ///
    /// Prefers the run's per-turn `channel_name` (captured atomically from the
    /// launching message's scope) looked up in the channel registry. Falls back
    /// to the shared active channel when the name is absent (single-channel /
    /// unit-test paths) or not found in the registry (warns), so routing never
    /// panics — at worst it degrades to the previous shared-channel behaviour.
    async fn resolve_announce_channel(&self, channel_name: Option<&str>) -> Arc<dyn Channel> {
        if let Some(name) = channel_name {
            if let Some(channel) = self.channels.get(name) {
                return Arc::clone(channel);
            }
            if !self.channels.is_empty() {
                tracing::warn!(
                    channel = %name,
                    "sessions_spawn: originating channel not found in registry; \
                     falling back to active channel for announcement"
                );
            }
        }
        self.channel.read().await.clone()
    }
}

fn spawn_event_scope(
    workspace_id: &str,
    run_id: &str,
    session_scope_key: &str,
    parent_run_id: Option<&str>,
    agent_name: Option<&str>,
    scope: Option<&SpawnScope>,
) -> MessageEventScope {
    let mut envelope = RuntimeEnvelope::sessions_spawn(workspace_id, session_scope_key, run_id)
        .with_channel(scope.map_or("sessions_spawn", |scope| scope.channel.as_str()));
    if let Some(parent_run_id) = parent_run_id {
        envelope = envelope.with_parent_run_id(parent_run_id);
    }
    if let Some(agent_name) = agent_name {
        envelope = envelope.with_agent_id(agent_name);
    }
    if let Some(scope) = scope {
        envelope = envelope
            .with_sender(scope.sender.as_str())
            .with_recipient(scope.chat_id.as_str());
    }
    let mut event_scope = envelope.message_scope();
    if let Some(owner_id) = scope.and_then(|scope| scope.owner_id.as_deref()) {
        event_scope.owner_id = Some(owner_id.to_string());
    }
    event_scope
}

async fn record_spawn_request_event(
    fabric: Option<&MemoryFabric>,
    scope: MessageEventScope,
    task: &str,
    mode: &str,
    provider_name: &str,
    model: &str,
    max_iterations: usize,
    lineage: &SpawnLineage,
) {
    let Some(fabric) = fabric else {
        return;
    };
    let task_event_scope = scope.clone();
    let task_id = task_event_scope
        .run_id
        .clone()
        .unwrap_or_else(|| "sessions_spawn:unknown".to_string());
    if let Err(error) = fabric
        .record_inbound_user_message(
            scope,
            task,
            None,
            Some(
                json!({
                    "mode": mode,
                    "provider": provider_name,
                    "model": model,
                    "max_iterations": max_iterations,
                    "owner_id": lineage.owner_id,
                    "topic_id": lineage.topic_id,
                    "parent_task_id": lineage.parent_task_id,
                    "source_message_event_id": lineage.source_message_event_id
                })
                .to_string(),
            ),
        )
        .await
    {
        tracing::warn!("failed to record sessions_spawn request event: {error}");
    }
    if let Err(error) = fabric
        .record_task_event(
            task_event_scope,
            task_id,
            "task.spawned",
            Some(
                json!({
                    "task": task,
                    "mode": mode,
                    "provider": provider_name,
                    "model": model,
                    "max_iterations": max_iterations,
                    "owner_id": lineage.owner_id,
                    "topic_id": lineage.topic_id,
                    "parent_task_id": lineage.parent_task_id,
                    "source_message_event_id": lineage.source_message_event_id
                })
                .to_string(),
            ),
        )
        .await
    {
        tracing::warn!("failed to record sessions_spawn task.spawned event: {error}");
    }
}

async fn record_spawn_result_event(
    fabric: Option<&MemoryFabric>,
    scope: MessageEventScope,
    result_text: &str,
    status: &SubAgentStatus,
    lineage: &SpawnLineage,
) {
    let Some(fabric) = fabric else {
        return;
    };
    let task_event_scope = scope.clone();
    let task_id = task_event_scope
        .run_id
        .clone()
        .unwrap_or_else(|| "sessions_spawn:unknown".to_string());
    let (success, error) = match status {
        SubAgentStatus::Completed(_) => (true, None),
        SubAgentStatus::Running => (false, Some("still running".to_string())),
        SubAgentStatus::AwaitingInput { prompt } => (false, Some(format!("awaiting approval: {prompt}"))),
        SubAgentStatus::Failed(error) => (false, Some(error.clone())),
    };
    if let Err(error) = fabric
        .record_worker_result(
            scope,
            result_text,
            Some(
                json!({
                    "success": success,
                    "error": error,
                    "owner_id": lineage.owner_id,
                    "topic_id": lineage.topic_id,
                    "parent_task_id": lineage.parent_task_id,
                    "source_message_event_id": lineage.source_message_event_id
                })
                .to_string(),
            ),
        )
        .await
    {
        tracing::warn!("failed to record sessions_spawn result event: {error}");
    }
    let task_event_type = if success { "task.completed" } else { "task.failed" };
    if let Err(error) = fabric
        .record_task_event(
            task_event_scope,
            task_id,
            task_event_type,
            Some(
                json!({
                    "success": success,
                    "error": error,
                    "result_preview": result_text.chars().take(500).collect::<String>(),
                    "owner_id": lineage.owner_id,
                    "topic_id": lineage.topic_id,
                    "parent_task_id": lineage.parent_task_id,
                    "source_message_event_id": lineage.source_message_event_id
                })
                .to_string(),
            ),
        )
        .await
    {
        tracing::warn!("failed to record sessions_spawn {task_event_type} event: {error}");
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
                "provider": {
                    "type": "string",
                    "description": "Optional provider override for the sub-agent (e.g. 'openrouter', 'ollama'). Defaults to the agent config provider, then the gateway provider."
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

    /// Route sub-agent result announcements back on the channel the triggering
    /// message arrived on (wacli/Signal/Telegram/…). The channel/gateway loop
    /// calls this before each turn; each subsequent spawn snapshots the active
    /// channel, fixing the bug where results were always announced over the
    /// construction-time default channel (Signal) regardless of origin.
    async fn set_active_channel(&self, channel: Arc<dyn Channel>) {
        *self.channel.write().await = channel;
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("spawn");
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);

        match action {
            "list" => return self.execute_list().await,
            "kill" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for kill action"))?;
                return self.execute_kill(run_id, approval_grant.as_ref()).await;
            }
            "history" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for history action"))?;
                return self.execute_history(run_id).await;
            }
            "steer" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for steer action"))?;
                let message = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter for steer action"))?;
                return self.execute_steer(run_id, message, approval_grant.as_ref()).await;
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
                error: Some(format!("Invalid 'mode' value '{mode}'. Expected 'task' or 'process'.")),
            });
        }

        let model_override = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let provider_override = args
            .get("provider")
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
        // D8-4: a turn-root context represents the turn itself (zero spawn nesting
        // so far); its first child must compute depth 0 — identical to the
        // no-context case — so seeding a turn root never tightens the
        // max_spawn_depth boundary. A real spawn-run context's child computes +1.
        let spawn_depth = parent_exec_ctx.as_ref().map_or(0, |ctx| {
            if ctx.is_turn_root {
                ctx.spawn_depth
            } else {
                ctx.spawn_depth.saturating_add(1)
            }
        });
        let parent_run_id = parent_exec_ctx.as_ref().map(|ctx| ctx.run_id.clone());
        let session_scope_key = spawn_session_scope_key(parent_exec_ctx.as_ref(), spawn_scope.as_ref());

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
                    matches!(run.status, SubAgentStatus::Running) && run.session_scope_key == session_scope_key
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

        // FIX-P0-37: spawning a child session creates a new process that
        // consumes resources and carries a potential sandbox-escape surface,
        // so it is a Medium-risk side effect (requires an approval grant under
        // supervised autonomy; denied outright under read-only) rather than Low.
        if let Err(error) = SideEffectGate::new(self.security.as_ref()).authorize_resource_operation(
            self.name(),
            "sessions_spawn:spawn",
            ResourceRiskLevel::Medium,
            approval_grant.as_ref(),
        ) {
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

        // Resolve the announce recipient + channel atomically from this turn.
        //
        // Precedence: explicit `recipient` arg > per-turn scope `chat_id` >
        // shared `default_recipient`. The per-turn scope (parsed from the trusted
        // `_zc_scope` injected for *this* execution) is atomic — it travels with
        // the launching message — whereas `default_recipient`/`channel` are shared
        // and a concurrently-processed message can overwrite them between this
        // turn entering the LLM loop and the spawn actually executing. Binding
        // both `recipient` and `channel_name` from the same scope eliminates the
        // A-channel + B-recipient cross-wiring (cross-channel privacy leak).
        let recipient = match explicit_recipient {
            Some(r) => Some(r),
            None => match spawn_scope.as_ref().map(|scope| scope.chat_id.clone()) {
                Some(chat_id) => Some(chat_id),
                None => self.default_recipient.read().await.clone(),
            },
        };
        // Channel name bound to the *originating* message (atomic with recipient).
        // `None` (no trusted scope) falls back at announce time to the shared
        // active channel, preserving single-channel / legacy behaviour.
        let run_channel_name = spawn_scope.as_ref().map(|scope| scope.channel.clone());

        let resolved_provider_name = provider_override.unwrap_or_else(|| {
            selected_agent
                .as_ref()
                .map(|(_, cfg)| cfg.provider.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| self.provider_name.clone())
        });
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
        let resolved_max_iterations = args
            .get("max_iterations")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .map_or(configured_max, |dynamic_max| dynamic_max.max(1).min(configured_max));
        let memory_fabric = self.memory.as_ref().map(|memory| {
            MemoryFabric::new(memory.clone(), self.workspace_dir.to_string_lossy())
                .with_event_recording(self.event_recording)
        });
        let spawn_scope_for_event = spawn_event_scope(
            &self.workspace_dir.to_string_lossy(),
            &run_id,
            &session_scope_key,
            parent_run_id.as_deref(),
            agent_name.as_deref(),
            spawn_scope.as_ref(),
        );
        let run_lineage = spawn_lineage(&spawn_scope_for_event, parent_exec_ctx.as_ref(), spawn_scope.as_ref());
        record_spawn_request_event(
            memory_fabric.as_ref(),
            spawn_scope_for_event.clone(),
            task,
            &mode,
            &resolved_provider_name,
            &resolved_model,
            resolved_max_iterations,
            &run_lineage,
        )
        .await;

        if mode == "process" {
            let temperature = resolved_temperature;
            let history_arc: Arc<RwLock<Vec<HistoryEntry>>> = Arc::new(RwLock::new(Vec::new()));

            {
                let mut runs = self.active_runs.write().await;
                runs.push(SubAgentRun {
                    id: run_id.clone(),
                    task: task.to_string(),
                    owner_id: run_lineage.owner_id.clone(),
                    topic_id: run_lineage.topic_id.clone(),
                    source_message_event_id: run_lineage.source_message_event_id.clone(),
                    started_at: Utc::now(),
                    status: SubAgentStatus::Running,
                    recipient: recipient.clone(),
                    channel_name: run_channel_name.clone(),
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
            // Resolve the announce channel from the *per-turn* channel name bound
            // to this run (the launching message's scope), not the shared active
            // channel — so a concurrently-processed message cannot mis-route this
            // run's result. Falls back to the active channel when no scope name.
            let channel = self.resolve_announce_channel(run_channel_name.as_deref()).await;
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
            let process_agent_id = selected_agent.as_ref().map(|(name, _)| name.clone());
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
            let process_event_recording = self.event_recording;
            let process_memory_strategy =
                normalize_process_memory_strategy(&self.spawn_config.process_memory_strategy)?.to_string();
            let process_execution_ctx = SpawnExecutionContext {
                run_id: rid.clone(),
                session_scope_key: session_scope_key.clone(),
                spawn_depth,
                owner_id: run_lineage.owner_id.clone(),
                topic_id: run_lineage.topic_id.clone(),
                source_message_event_id: run_lineage.source_message_event_id.clone(),
                // A spawn-run context (this run may itself spawn): its children
                // compute spawn_depth + 1.
                is_turn_root: false,
            };
            let process_memory_fabric = memory_fabric.clone();
            let process_result_scope = spawn_scope_for_event.clone();
            let process_lineage = run_lineage.clone();

            let jh = tokio::spawn(SPAWN_EXECUTION_CONTEXT.scope(process_execution_ctx, async move {
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
                    process_agent_id.as_deref(),
                    &process_lineage,
                    &process_memory_strategy,
                    process_event_recording,
                    &process_compaction_config,
                )
                .await;

                let (status, result_text) = match worker_result {
                    Ok(result) if result.success => (SubAgentStatus::Completed(result.output.clone()), result.output),
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
                record_spawn_result_event(
                    process_memory_fabric.as_ref(),
                    process_result_scope,
                    &result_text,
                    &status,
                    &process_lineage,
                )
                .await;

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
            }));

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
                owner_id: run_lineage.owner_id.clone(),
                topic_id: run_lineage.topic_id.clone(),
                source_message_event_id: run_lineage.source_message_event_id.clone(),
                started_at: Utc::now(),
                status: SubAgentStatus::Running,
                recipient: recipient.clone(),
                channel_name: run_channel_name.clone(),
                abort_handle: None,
                history: history_arc.clone(),
                steer_tx: Some(steer_tx),
                parent_run_id: parent_run_id.clone(),
                session_scope_key: session_scope_key.clone(),
                spawn_depth,
            });
        }

        // Clone everything the spawned task needs.
        // Resolve the announce channel from the *per-turn* channel name bound to
        // this run (the launching message's scope), not the shared active channel
        // — so a concurrently-processed message cannot mis-route this run's
        // result. Falls back to the active channel when no scope name is present.
        let channel = self.resolve_announce_channel(run_channel_name.as_deref()).await;
        let provider_name = resolved_provider_name;
        let model = resolved_model;
        let temperature = resolved_temperature;
        let max_iterations = resolved_max_iterations;
        // Rebuild the provider object whenever the resolved provider differs
        // from the gateway provider. This covers a named agent provider AND an
        // inline `provider` override (BUG-12) even without a named agent.
        let provider = if provider_name != self.provider_name {
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
        // Event bridge (v1.1a): if a chat-side sink is attached, create this
        // session's middle channels + drainer up front (run_id is already
        // minted, so the drainer is tagged with the correct id — no race). The
        // background agent only ever `.send().await`s onto these; the drainer
        // continuously empties them, so the agent never back-pressures.
        let run_event_streams = self.event_sink.as_ref().map(|sink| sink.streams_for(&run_id));
        // NeedsInput (chat `/bg` only): mint this run's approval resolver so a
        // supervised gate hit suspends awaiting an operator decision instead of
        // auto-failing. `None` everywhere else (channels/gateway) preserves the
        // historical auto-fail-on-gate semantics.
        let run_approval_resolver = self
            .approval_resolver_factory
            .as_ref()
            .map(|factory| factory.resolver_for(&run_id));
        // NeedsInput: when (and only when) an approval resolver is attached, hand
        // the loop the run registry + id so it can deterministically restore
        // `AwaitingInput` -> `Running` on cancel-and-resume (steer). `None`
        // elsewhere — without a resolver no run can ever suspend.
        let (restore_active_runs, restore_run_id) = if run_approval_resolver.is_some() {
            (Some(self.active_runs.clone()), Some(run_id.clone()))
        } else {
            (None, None)
        };
        let task_owned = task.to_string();
        let tools = self.tools.get().cloned();
        let workspace_dir = self.workspace_dir.clone();
        let multimodal_config = self.multimodal_config.clone();
        let security = self.security.clone();
        let task_scope = spawn_scope.clone();
        let task_memory = self.memory.clone();
        let compaction_config = self.compaction_config.clone();
        let task_execution_ctx = SpawnExecutionContext {
            run_id: rid.clone(),
            session_scope_key: session_scope_key.clone(),
            spawn_depth,
            owner_id: run_lineage.owner_id.clone(),
            topic_id: run_lineage.topic_id.clone(),
            source_message_event_id: run_lineage.source_message_event_id.clone(),
            // A spawn-run context (this run may itself spawn): its children
            // compute spawn_depth + 1.
            is_turn_root: false,
        };
        let task_memory_fabric = memory_fabric.clone();
        let task_result_scope = spawn_scope_for_event.clone();
        let task_lineage = run_lineage.clone();
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
        let jh = tokio::spawn(SPAWN_EXECUTION_CONTEXT.scope(task_execution_ctx, async move {
            tracing::info!(run_id = %rid, "Sub-agent task starting");
            let (run_on_delta, run_on_tool) = match run_event_streams {
                Some((delta_tx, tool_tx)) => (Some(delta_tx), Some(tool_tx)),
                None => (None, None),
            };
            let run_future = run_sub_agent_task(
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
                task_memory,
                run_on_delta,
                run_on_tool,
                run_approval_resolver,
                restore_active_runs,
                restore_run_id,
            );
            // `timeout_secs == 0` means "no timeout" — run until natural
            // completion. This matches the session-worker semantics in
            // `session_worker/runner.rs`. A non-zero value wraps the run in a
            // `tokio::time::timeout`. `Ok(_)` => ran to completion (no timeout
            // or finished in time), `Err(_)` => elapsed.
            let result = if timeout_secs == 0 {
                Ok(run_future.await)
            } else {
                tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), run_future).await
            };
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
            record_spawn_result_event(
                task_memory_fabric.as_ref(),
                task_result_scope,
                &result_text,
                &status,
                &task_lineage,
            )
            .await;

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
        }));

        // Store the abort handle so kill action can cancel this run
        {
            let mut runs = self.active_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
                run.abort_handle = Some(jh.abort_handle());
            }
        }

        Ok(ToolResult {
            success: true,
            output: format!("Sub-agent spawned (run_id: {run_id}). Will announce result when complete."),
            error: None,
        })
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Automation]
    }
}

impl SessionsSpawnTool {
    fn memory_fabric(&self) -> Option<MemoryFabric> {
        self.memory.as_ref().map(|memory| {
            MemoryFabric::new(memory.clone(), self.workspace_dir.to_string_lossy())
                .with_event_recording(self.event_recording)
        })
    }

    async fn record_active_run_task_event(&self, run: &SubAgentRun, event_type: &str, payload: serde_json::Value) {
        let Some(fabric) = self.memory_fabric() else {
            return;
        };
        let mut scope = MessageEventScope::new("sessions_spawn", crate::memory::MemoryVisibility::Workspace)
            .with_session_key(run.session_scope_key.clone())
            .with_run_id(run.id.clone());
        if let Some(owner_id) = run.owner_id.as_deref() {
            scope = scope.with_owner_id(owner_id);
        }
        if let Some(parent_run_id) = run.parent_run_id.as_deref() {
            scope = scope.with_parent_run_id(parent_run_id);
        }
        let payload = json!({
            "task": run.task,
            "status": status_label(&run.status),
            "owner_id": run.owner_id,
            "topic_id": run.topic_id,
            "parent_task_id": run.parent_run_id,
            "source_message_event_id": run.source_message_event_id,
            "detail": payload
        });
        if let Err(error) = fabric
            .record_task_event(scope, run.id.clone(), event_type.to_string(), Some(payload.to_string()))
            .await
        {
            tracing::warn!(run_id = %run.id, event_type, "failed to record sessions_spawn task event: {error}");
        }
    }

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
                    SubAgentStatus::AwaitingInput { prompt } => {
                        format!("❓ awaiting approval: {prompt}")
                    }
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
            output: format!("Sub-agent runs ({} total):\n\n{}", runs.len(), lines.join("\n\n")),
            error: None,
        })
    }

    /// Kill a running sub-agent by its run ID.
    async fn execute_kill(&self, run_id: &str, approval_grant: Option<&ApprovalGrant>) -> anyhow::Result<ToolResult> {
        let (recipient_opt, channel_name_opt, rid, killed_run) = {
            let mut runs = self.active_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
                match &run.status {
                    // A live run (executing) or one suspended awaiting approval
                    // (NeedsInput) is killable: aborting the task tears down the
                    // suspended approval resolver's pending await along with it.
                    SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => {
                        let operation_name = format!("sessions_spawn:kill:{run_id}");
                        if let Err(error) = SideEffectGate::new(self.security.as_ref()).authorize_resource_operation(
                            self.name(),
                            &operation_name,
                            ResourceRiskLevel::Medium,
                            approval_grant,
                        ) {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(error),
                            });
                        }
                        if let Some(ah) = run.abort_handle.as_ref() {
                            ah.abort();
                        }
                        let recipient = run.recipient.clone();
                        // Per-turn channel bound at spawn time — kill-notify routes
                        // to the same channel + recipient as the launching message,
                        // not the shared active channel (avoids cross-channel leak).
                        let channel_name = run.channel_name.clone();
                        let rid = run.id.clone();
                        run.status = SubAgentStatus::Failed("killed by user".into());
                        run.steer_tx = None;
                        (recipient, channel_name, rid, run.clone())
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

        self.record_active_run_task_event(&killed_run, "task.killed", json!({"reason": "killed by user"}))
            .await;

        if let Some(target) = recipient_opt {
            let msg_text = format!("🤖 Sub-agent `{rid}` was killed by user.");
            let msg = SendMessage::new(&msg_text, &target);
            let channel = self.resolve_announce_channel(channel_name_opt.as_deref()).await;
            if let Err(error) = channel.send(&msg).await {
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
                SubAgentStatus::AwaitingInput { .. } => "awaiting approval, no history captured yet",
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
    async fn execute_steer(
        &self,
        run_id: &str,
        message: &str,
        approval_grant: Option<&ApprovalGrant>,
    ) -> anyhow::Result<ToolResult> {
        let run_snapshot = {
            let runs = self.active_runs.read().await;
            let Some(run) = runs.iter().find(|r| r.id == run_id) else {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("No run found with ID `{run_id}`.")),
                });
            };

            match &run.status {
                // A suspended (AwaitingInput) run still owns a live steer channel;
                // a plain steer cancels the inner loop (tearing down the pending
                // approval await) and re-injects the operator's message as a new
                // turn, exactly as for a running session. Structured approval
                // decisions go through `/approve` / `/deny` instead.
                SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => {
                    let Some(ref tx) = run.steer_tx else {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!(
                                "Run `{run_id}` is running but has no steer channel (legacy run)."
                            )),
                        });
                    };
                    let operation_name = format!("sessions_spawn:steer:{run_id}");
                    if let Err(error) = SideEffectGate::new(self.security.as_ref()).authorize_resource_operation(
                        self.name(),
                        &operation_name,
                        ResourceRiskLevel::Low,
                        approval_grant,
                    ) {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(error),
                        });
                    }
                    tx.send(message.to_string())
                        .map_err(|_| anyhow::anyhow!("Sub-agent steer channel closed unexpectedly"))?;
                    run.clone()
                }
                SubAgentStatus::Completed(_) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Run `{run_id}` already completed; cannot steer.")),
                    });
                }
                SubAgentStatus::Failed(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Run `{run_id}` already failed ({e}); cannot steer.")),
                    });
                }
            }
        };

        self.record_active_run_task_event(
            &run_snapshot,
            "task.steered",
            json!({"message_preview": message.chars().take(500).collect::<String>()}),
        )
        .await;
        Ok(ToolResult {
            success: true,
            output: format!(
                "Steering message sent to sub-agent `{run_id}`. \
                 The agent will incorporate it at the next opportunity."
            ),
            error: None,
        })
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
                let new_key = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .map(|key| memory_key_prefix(prefix, key));
                if let Some(new_key) = new_key {
                    if let Some(m) = args.as_object_mut() {
                        m.insert("key".to_string(), serde_json::Value::String(new_key));
                    }
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

/// Environment variable carrying the sealed session-worker capability to the
/// child process.
const SESSION_WORKER_CAP_ENV: &str = "OPENPRX_SESSION_WORKER_CAPABILITY";
/// Environment variable carrying the capability's absolute expiry (unix secs),
/// which is bound into the capability HMAC.
const SESSION_WORKER_CAP_EXPIRY_ENV: &str = "OPENPRX_SESSION_WORKER_CAPABILITY_EXPIRY";
/// Capability time-to-live in seconds.
const SESSION_WORKER_CAP_TTL_SECS: u64 = 300;

/// Current unix time in seconds (saturating to 0 on clock errors).
fn capability_now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compute the sealed capability HMAC for a manifest and absolute expiry.
///
/// FIX-P0-36: the signed payload is
/// `run_id \0 expiry_unix \0 sha256_hex(manifest_json_with_empty_capability)`,
/// signed with `HMAC_SHA256(secret, payload)` and encoded as base64url (no
/// padding). The manifest is serialized via `serde_json::Value` (alphabetical
/// key order) so that the parent (this side) and the validating worker — which
/// reconstructs the payload from the transmitted JSON — produce byte-identical
/// inputs regardless of struct field declaration order.
///
/// NOTE: the equivalent recomputation lives in `session_worker::runner`
/// (`expected_worker_capability`). The two must stay in lockstep; they are kept
/// in separate modules deliberately (parent mints, child validates) and share
/// the same payload construction documented here.
fn seal_worker_capability(manifest: &WorkerManifest, expiry_unix: u64) -> anyhow::Result<String> {
    let payload = manifest_signing_payload(manifest)?;
    Ok(compute_worker_capability(&manifest.run_id, expiry_unix, &payload))
}

/// Serialize a manifest with an empty `parent_capability` field, returning the
/// canonical JSON payload (alphabetical key order via `serde_json::Value`).
fn manifest_signing_payload(manifest: &WorkerManifest) -> anyhow::Result<String> {
    let mut value =
        serde_json::to_value(manifest).map_err(|e| anyhow::anyhow!("serialize worker manifest for capability: {e}"))?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "parent_capability".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    serde_json::to_string(&value).map_err(|e| anyhow::anyhow!("reserialize worker manifest for capability: {e}"))
}

/// Compute `HMAC_SHA256(secret, run_id \0 expiry \0 sha256_hex(manifest))` as
/// base64url (no padding).
fn compute_worker_capability(run_id: &str, expiry_unix: u64, manifest_json: &str) -> String {
    use base64::Engine as _;
    use ring::hmac;
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(manifest_json.as_bytes());
    let manifest_hex = hmac_hex_encode(&hasher.finalize());

    let mut payload = Vec::with_capacity(run_id.len() + manifest_hex.len() + 32);
    payload.extend_from_slice(run_id.as_bytes());
    payload.push(0);
    payload.extend_from_slice(expiry_unix.to_string().as_bytes());
    payload.push(0);
    payload.extend_from_slice(manifest_hex.as_bytes());

    let tag = hmac::sign(&session_worker_signing_key(), &payload);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag.as_ref())
}

/// Lowercase hex-encode a byte slice.
fn hmac_hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        // `b >> 4` and `b & 0x0f` are both in `0..16`, always valid indices into
        // the 16-byte `HEX` table; `.get().copied()` keeps the lookup panic-free.
        out.push(HEX.get((b >> 4) as usize).copied().unwrap_or(b'0') as char);
        out.push(HEX.get((b & 0x0f) as usize).copied().unwrap_or(b'0') as char);
    }
    out
}

/// Process-level fallback secret, minted once if neither `SESSION_WORKER_SECRET`
/// nor a persisted secret file is available.
static SESSION_WORKER_FALLBACK_SECRET: OnceLock<[u8; 32]> = OnceLock::new();

/// Return the shared HMAC signing key for session-worker capabilities.
///
/// Resolution order (parent and child run the same binary on the same host, so
/// all three sources are deterministic across the process boundary):
/// 1. `SESSION_WORKER_SECRET` environment variable (explicit configuration).
/// 2. A 32-byte secret persisted under the OpenPRX state dir (auto-generated on
///    first use, mirroring `WitnessKeyring`), so parent and child derive the
///    same key without transporting the secret alongside the capability.
/// 3. A per-process random fallback (only consistent within a single process;
///    used in tests / when no filesystem state dir is available).
fn session_worker_signing_key() -> ring::hmac::Key {
    use ring::hmac;
    if let Ok(secret) = std::env::var("SESSION_WORKER_SECRET") {
        if !secret.is_empty() {
            return hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
        }
    }
    if let Some(bytes) = load_or_create_persisted_session_secret() {
        return hmac::Key::new(hmac::HMAC_SHA256, &bytes);
    }
    let bytes = SESSION_WORKER_FALLBACK_SECRET.get_or_init(generate_session_secret);
    hmac::Key::new(hmac::HMAC_SHA256, bytes)
}

/// Generate 32 random bytes, falling back to a time-derived seed (never panics)
/// when the system RNG is unavailable.
fn generate_session_secret() -> [u8; 32] {
    use ring::rand::SecureRandom as _;
    let rng = ring::rand::SystemRandom::new();
    let mut buf = [0u8; 32];
    if rng.fill(&mut buf).is_err() {
        let now = capability_now_unix().to_le_bytes();
        for (i, b) in buf.iter_mut().enumerate() {
            // `i % now.len()` is always within `now` (non-empty fixed-size array);
            // `.get().copied()` keeps the seed fill panic-free.
            *b = now.get(i % now.len()).copied().unwrap_or(0);
        }
    }
    buf
}

/// Path to the persisted session-worker secret under the OpenPRX state dir.
///
/// Mirrors `WitnessKeyring`'s convention: an explicit override env, else
/// `$HOME/.openprx`. Uses `HOME` directly (no `dirs` dependency).
fn session_secret_path() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("OPENPRX_SESSION_WORKER_SECRET_PATH") {
        return Some(std::path::PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    Some(home.join(".openprx").join("keys").join("session_worker.secret"))
}

/// Load the persisted 32-byte secret, creating it on first use. Returns `None`
/// if no state dir is resolvable or any filesystem operation fails (callers then
/// fall back to the per-process random secret).
fn load_or_create_persisted_session_secret() -> Option<[u8; 32]> {
    let path = session_secret_path()?;
    if let Ok(existing) = std::fs::read(&path) {
        if existing.len() == 32 {
            let mut buf = [0u8; 32];
            buf.copy_from_slice(&existing);
            return Some(buf);
        }
        // Wrong length → fall through and regenerate.
    }
    let secret = generate_session_secret();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return None;
        }
    }
    match std::fs::write(&path, secret) {
        Ok(()) => Some(secret),
        Err(error) => {
            tracing::warn!("failed to persist session-worker secret: {error}");
            None
        }
    }
}

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
///
/// Deterministically restore a single run from a suspended approval back to
/// [`SubAgentStatus::Running`].
///
/// Downgrades **only** `AwaitingInput` -> `Running`; any terminal state
/// (`Completed` / `Failed` / `Cancelled`) — e.g. one set by a concurrent kill or
/// timeout — is left untouched, so a killed run that already moved to `Failed` is
/// never resurrected to `Running`. A no-op if the run id is absent. Idempotent.
fn restore_run_to_running(runs: &mut [SubAgentRun], run_id: &str) {
    if let Some(run) = runs.iter_mut().find(|r| r.id == run_id)
        && matches!(run.status, SubAgentStatus::AwaitingInput { .. })
    {
        run.status = SubAgentStatus::Running;
        tracing::debug!(run_id = %run_id, "restored sub-agent to Running after approval suspension ended");
    }
}

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
    memory: Option<Arc<dyn Memory>>,
    on_delta: Option<tokio::sync::mpsc::Sender<String>>,
    on_tool_call: Option<tokio::sync::mpsc::Sender<crate::agent::loop_::ToolCallNotification>>,
    approval_resolver: Option<Arc<dyn crate::agent::loop_::ApprovalResolver>>,
    // NeedsInput: the shared run registry + this run's id, used to
    // deterministically restore `AwaitingInput` -> `Running` whenever the loop
    // leaves a suspended approval to continue running (steer / cancel-and-resume).
    // The resolver's own `Drop` only does a best-effort `try_write` restore that
    // is skipped under lock contention, so this async path is the authoritative
    // guarantee that no run is left as a zombie `AwaitingInput` while it is in
    // fact running again. `None` when there is no approval resolver attached
    // (channels / gateway), where suspension can never happen.
    active_runs: Option<Arc<RwLock<Vec<SubAgentRun>>>>,
    run_id: Option<String>,
) -> anyhow::Result<String> {
    // --- No-tools fallback: single-turn completion ---
    let Some(tools_registry) = tools else {
        let response = provider
            .chat_with_system(Some(system_prompt), task, model, temperature)
            .await?;
        // No incremental loop output exists on this path (single completion);
        // surface the final response as one delta so an attached follower sees
        // it (best-effort; dropped on a full/closed channel).
        if let Some(ref tx) = on_delta
            && !response.trim().is_empty()
        {
            let _ = tx.try_send(response.clone());
        }
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
    let mut history: Vec<ChatMessage> = vec![ChatMessage::system(system_prompt), ChatMessage::user(task)];

    // NeedsInput: deterministically restore a suspended run to `Running` before
    // re-entering the loop. Called on every path that *continues* running after a
    // possible approval suspension (steer-driven cancel-and-resume). Downgrades
    // `AwaitingInput` -> `Running` only; it never clobbers a terminal state
    // (Completed / Failed / Cancelled) set concurrently by a kill / timeout, so
    // a killed run that already moved to `Failed` is left untouched. This runs in
    // a proper async context (`.write().await`), so unlike the resolver's `Drop`
    // best-effort `try_write` it can never be skipped under lock contention.
    let restore_running = || async {
        let (Some(runs_arc), Some(rid)) = (active_runs.as_ref(), run_id.as_ref()) else {
            return;
        };
        let mut runs = runs_arc.write().await;
        restore_run_to_running(&mut runs, rid);
    };

    loop {
        let cancel_token = CancellationToken::new();

        // Clone everything needed for the inner spawned task.
        // We move `history` into the task and get it back after completion.
        let mut task_history = history;
        let provider_instance = provider.clone();
        let provider_name_owned = provider_name.to_string();
        let model_name = model.to_string();
        let temperature_value = temperature;
        let tools_registry_owned = tools_registry.clone();
        let workspace_dir_owned = workspace_dir.to_path_buf();
        let multimodal_config_owned = multimodal_config.clone();
        let compaction_config_owned = compaction_config.clone();
        let cancel_token_owned = cancel_token.clone();
        let security = security.clone();
        let scope_owned = scope.clone();
        let memory_owned = memory.clone();
        // Clone the event-bridge senders per iteration so they survive across
        // steer-driven loop restarts (the inner spawn consumes its clones; the
        // originals stay owned by this outer loop). When `None`, the loop runs
        // silently exactly as before (channels/gateway behaviour unchanged).
        //
        // NOTE: the agent stays `silent = true` regardless. `silent` only gates
        // the loop's *direct* `print!` to stdout (loop_.rs); a background
        // sub-agent must never print to the chat's terminal (it would corrupt
        // the TUI). The `on_delta` / `on_tool_call` channel sends are NOT gated
        // by `silent`, so the event bridge streams to the drainer either way.
        let on_delta_iter = on_delta.clone();
        let on_tool_call_iter = on_tool_call.clone();
        // NeedsInput: clone the per-run resolver for this iteration. When present
        // (chat `/bg` only), a supervised `ApprovalManager` is built so the loop
        // consults the resolver at the approval decision point (suspend-on-gate).
        // When absent (channels/gateway), no manager + no resolver = the
        // historical auto-fail-on-gate path (zero behaviour change).
        let approval_resolver_iter = approval_resolver.clone();
        let mut loop_handle = tokio::spawn(async move {
            let observer = NoopObserver;
            let hooks = HookManager::new(workspace_dir_owned);
            let scope_ctx = scope_owned.as_ref().map(|scope| ScopeContext {
                policy: &security,
                sender: scope.sender.as_str(),
                channel: scope.channel.as_str(),
                chat_type: scope.chat_type.as_str(),
                chat_id: scope.chat_id.as_str(),
                owner_id: scope.owner_id.as_deref(),
                topic_id: scope.topic_id.as_deref(),
                task_id: scope.parent_task_id.as_deref(),
                source_message_event_id: scope.source_message_event_id.as_deref(),
            });
            // Only build an `ApprovalManager` when a resolver is attached. Under
            // permission-model Phase 1 the unified `SecurityPolicy::decide`
            // (supervised: act-tools → Ask) is what routes a call into the
            // resolver suspend path; the manager is just the UI/grant layer. Built
            // from the live policy's autonomy level so `Full` / `ReadOnly` never
            // suspend.
            let approval_manager = approval_resolver_iter
                .as_ref()
                .map(|_| crate::approval::ApprovalManager::from_autonomy_level(security.autonomy));
            let loop_outcome = crate::agent::loop_::run_tool_call_loop_outcome(
                provider_instance.as_ref(),
                &mut task_history,
                tools_registry_owned.as_slice(),
                &observer,
                &hooks,
                &provider_name_owned,
                &model_name,
                temperature_value,
                true, // silent — never print to the chat terminal (see note above)
                approval_manager.as_ref(),
                "sessions_spawn",
                &multimodal_config_owned,
                max_iterations,
                true,
                2,
                30,
                false,
                vec!["sessions_spawn".to_string(), "delegate".to_string(), "cron".to_string()],
                ToolConcurrencyGovernanceConfig {
                    rollout_stage: "full".to_string(),
                    ..ToolConcurrencyGovernanceConfig::default()
                },
                Some(&compaction_config_owned),
                Some(cancel_token_owned),
                on_delta_iter, // chat event bridge: incremental loop output (v1.1a)
                scope_ctx.as_ref(),
                on_tool_call_iter, // chat event bridge: tool-call notifications (v1.1a)
                None,              // spawned sessions do not use tool tiering
                scope_ctx.as_ref().and_then(|ctx| {
                    memory_owned
                        .as_ref()
                        .map(|memory| DocumentIngestRuntime::from_scope(memory.clone(), ctx))
                }),
                crate::agent::loop_::ChatMode::default(),
                approval_resolver_iter,
                false,
            )
            .await;
            let result = loop_outcome.map(|(outcome, _trace)| outcome.into_text());
            (task_history, result)
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
                    Err(error) => Err(error),
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
                        // NeedsInput: the cancelled inner loop may have been parked
                        // on a suspended approval (`resolve()` future dropped on
                        // cancel). Its registry status can be a zombie
                        // `AwaitingInput` if the resolver's `Drop` `try_write`
                        // restore was skipped under contention. We are about to
                        // re-run the loop, so deterministically restore `Running`
                        // here (async, authoritative) — never clobbering a terminal
                        // state set by a concurrent kill / timeout.
                        restore_running().await;
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
                            Err(error) => Err(error),
                        };
                    }
                }
            }
        }
    }
}

fn copy_dir_recursive(source: &std::path::Path, destination: &std::path::Path) -> anyhow::Result<()> {
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

fn shared_worker_memory_db_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root.join("memory").join("brain.db")
}

fn private_worker_memory_db_path(worker_workspace: &std::path::Path) -> std::path::PathBuf {
    worker_workspace.join("brain.db")
}

fn normalize_process_memory_strategy(strategy: &str) -> anyhow::Result<&'static str> {
    match strategy.trim() {
        "" | PROCESS_MEMORY_STRATEGY_SHARED => Ok(PROCESS_MEMORY_STRATEGY_SHARED),
        PROCESS_MEMORY_STRATEGY_ISOLATED => Ok(PROCESS_MEMORY_STRATEGY_ISOLATED),
        PROCESS_MEMORY_STRATEGY_HYBRID => Ok(PROCESS_MEMORY_STRATEGY_HYBRID),
        other => anyhow::bail!(
            "Invalid sessions_spawn.process_memory_strategy '{other}'. Expected 'shared_fabric', 'isolated_private', or 'hybrid'."
        ),
    }
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
    agent_id: Option<&str>,
    lineage: &SpawnLineage,
    memory_strategy: &str,
    event_recording: MemoryEventRecording,
    compaction_config: &AgentCompactionConfig,
) -> anyhow::Result<WorkerResult> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let worker_workspace = worker_workspace_root.join(run_id);
    std::fs::create_dir_all(&worker_workspace)?;
    let shared_memory_db_path = shared_worker_memory_db_path(workspace_root);
    let worker_memory_db_path = private_worker_memory_db_path(&worker_workspace);
    let normalized_memory_strategy = normalize_process_memory_strategy(memory_strategy)?;
    let (memory_db_path, memory_workspace_id) = match normalized_memory_strategy {
        PROCESS_MEMORY_STRATEGY_SHARED => (
            shared_memory_db_path.clone(),
            workspace_root.to_string_lossy().to_string(),
        ),
        PROCESS_MEMORY_STRATEGY_ISOLATED | PROCESS_MEMORY_STRATEGY_HYBRID => (
            worker_memory_db_path.clone(),
            worker_workspace.to_string_lossy().to_string(),
        ),
        other => anyhow::bail!("invalid normalized process memory strategy '{other}'"),
    };

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

    // FIX-P0-36: build the manifest with an empty capability first, then seal
    // it with an HMAC bound to the run id, an absolute expiry, and a digest of
    // the manifest contents. A leaked capability token therefore cannot be
    // replayed for a different run, after expiry, or with a tampered manifest.
    let mut manifest = WorkerManifest {
        parent_capability: None,
        run_id: run_id.to_string(),
        task: task.to_string(),
        provider_name: provider_name.to_string(),
        model: model.to_string(),
        api_key: api_key.map(str::to_string),
        temperature,
        workspace_dir: worker_workspace.clone(),
        memory_db_path,
        memory_workspace_id: Some(memory_workspace_id),
        memory_strategy: Some(normalized_memory_strategy.to_string()),
        shared_memory_db_path: Some(shared_memory_db_path),
        worker_memory_db_path: Some(worker_memory_db_path),
        agent_id: agent_id.map(str::to_string),
        persona_id: None,
        memory_event_recording: event_recording,
        allowed_tools: allowed_tools.to_vec(),
        timeout_seconds: timeout_secs,
        max_iterations,
        system_prompt: None,
        identity_dir,
        scope_sender: scope.map(|ctx| ctx.sender.clone()),
        scope_channel: scope.map(|ctx| ctx.channel.clone()),
        scope_chat_type: scope.map(|ctx| ctx.chat_type.clone()),
        scope_chat_id: scope.map(|ctx| ctx.chat_id.clone()),
        owner_id: lineage.owner_id.clone(),
        topic_id: lineage.topic_id.clone(),
        parent_task_id: lineage.parent_task_id.clone(),
        source_message_event_id: lineage.source_message_event_id.clone(),
        spawn_depth,
        session_scope_key: session_scope_key.to_string(),
        parent_run_id: parent_run_id.map(str::to_string),
        compaction_config: Some(compaction_config.clone()),
    };

    let capability_expiry = capability_now_unix().saturating_add(SESSION_WORKER_CAP_TTL_SECS);
    let sealed_capability = seal_worker_capability(&manifest, capability_expiry)?;
    manifest.parent_capability = Some(sealed_capability.clone());

    let executable = std::env::current_exe()?;
    let cli_args = build_session_worker_cli_args(&manifest)?;
    let mut command = tokio::process::Command::new(executable);
    command.env(SESSION_WORKER_CAP_ENV, &sealed_capability);
    command.env(SESSION_WORKER_CAP_EXPIRY_ENV, capability_expiry.to_string());
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

    // `timeout_secs == 0` means "no timeout" — the child (see
    // `session_worker/runner.rs`) runs until natural completion, so the parent
    // must not kill it prematurely. We use a far-future cap (30 days) as an
    // effectively-unbounded parent timeout, which keeps the existing
    // `tokio::time::timeout` wrapping intact while avoiding timer overflow.
    const NO_TIMEOUT_PARENT_CAP_SECS: u64 = 30 * 24 * 60 * 60;
    let parent_timeout = if timeout_secs == 0 {
        std::time::Duration::from_secs(NO_TIMEOUT_PARENT_CAP_SECS)
    } else {
        std::time::Duration::from_secs(timeout_secs)
    };
    let stdout_stream = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("session-worker stdout pipe was not configured"))?;
    let stderr_stream = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("session-worker stderr pipe was not configured"))?;

    const MAX_SUBPROCESS_OUTPUT: u64 = 10 * 1024 * 1024; // 10 MB per stream

    let process_outcome = tokio::time::timeout(parent_timeout, async {
        let stdout_future = async {
            let mut stdout_buf = Vec::new();
            let bytes_read = stdout_stream
                .take(MAX_SUBPROCESS_OUTPUT)
                .read_to_end(&mut stdout_buf)
                .await?;
            if bytes_read as u64 >= MAX_SUBPROCESS_OUTPUT {
                tracing::warn!(
                    limit_bytes = MAX_SUBPROCESS_OUTPUT,
                    "session-worker stdout reached size limit; output truncated"
                );
                stdout_buf.extend_from_slice(b"\n[output truncated at 10MB]");
            }
            Ok::<Vec<u8>, anyhow::Error>(stdout_buf)
        };
        let stderr_future = async {
            let mut stderr_buf = Vec::new();
            let bytes_read = stderr_stream
                .take(MAX_SUBPROCESS_OUTPUT)
                .read_to_end(&mut stderr_buf)
                .await?;
            if bytes_read as u64 >= MAX_SUBPROCESS_OUTPUT {
                tracing::warn!(
                    limit_bytes = MAX_SUBPROCESS_OUTPUT,
                    "session-worker stderr reached size limit; output truncated"
                );
                stderr_buf.extend_from_slice(b"\n[output truncated at 10MB]");
            }
            Ok::<Vec<u8>, anyhow::Error>(stderr_buf)
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
    let output = std::process::Output { status, stdout, stderr };
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
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
    use super::*;
    use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
    use crate::memory::{Memory, MemoryPrincipal, SqliteMemory};
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

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
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
                reasoning_content: None,
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

    /// A provider that sleeps before responding, so a spawned run stays in the
    /// `Running` state long enough for a kill test to act on it deterministically.
    struct SleepyProvider {
        delay_ms: u64,
        response: String,
    }

    #[async_trait::async_trait]
    impl crate::providers::Provider for SleepyProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            Ok(self.response.clone())
        }

        async fn chat(
            &self,
            _request: crate::providers::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            Ok(crate::providers::ChatResponse {
                text: Some(self.response.clone()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            })
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
                reasoning_content: None,
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

    fn make_tool(channel: Arc<dyn Channel>, provider: Arc<dyn crate::providers::Provider>) -> SessionsSpawnTool {
        make_tool_with_spawn_config(channel, provider, crate::config::SessionsSpawnConfig::default())
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

    /// FIX-P0-37: spawning is now a Medium-risk side effect, which requires an
    /// approval grant under the default (supervised) autonomy. Tests that drive
    /// the real `spawn` path must inject a matching grant — mirroring how the
    /// production agent loop issues one after operator approval. The operation
    /// name MUST equal the one the gate authorizes (`sessions_spawn:spawn`).
    fn spawn_grant_value() -> serde_json::Value {
        serde_json::to_value(ApprovalGrant::for_resource_operation(
            "sessions_spawn",
            "sessions_spawn:spawn",
            "test",
            None,
        ))
        .unwrap()
    }

    /// Merge a valid spawn approval grant into the given `spawn` arguments so the
    /// Medium-risk gate authorizes the call.
    fn with_spawn_grant(mut args: serde_json::Value) -> serde_json::Value {
        if let Some(obj) = args.as_object_mut() {
            obj.insert(
                crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.to_string(),
                spawn_grant_value(),
            );
        }
        args
    }

    #[test]
    fn name_and_description() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
        assert_eq!(tool.name(), "sessions_spawn");
        assert!(!tool.description().is_empty());
        assert!(tool.description().contains("history"));
        assert!(tool.description().contains("steer"));
    }

    #[test]
    fn default_sub_agent_timeout_is_ten_minutes() {
        // Regression: the constant was 0 (instant timeout in task mode) while
        // its doc claimed "10 minutes". It must now be 600s.
        assert_eq!(DEFAULT_SUB_AGENT_TIMEOUT_SECS, 600);
    }

    #[tokio::test]
    async fn task_mode_zero_timeout_does_not_elapse_immediately() {
        // Mirrors the task-mode timeout-wrapping logic at the spawn site:
        // `timeout_secs == 0` must run the future to completion (no timeout),
        // rather than wrapping it in `tokio::time::timeout(ZERO, ..)` which
        // would elapse on the first poll. Use a future with a real (small)
        // delay so a ZERO-duration timeout would observably fail.
        async fn wrap_like_task_mode(timeout_secs: u64) -> Result<&'static str, tokio::time::error::Elapsed> {
            let run_future = async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                "done"
            };
            if timeout_secs == 0 {
                Ok(run_future.await)
            } else {
                tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), run_future).await
            }
        }

        // 0 => no timeout => runs to completion.
        assert_eq!(wrap_like_task_mode(0).await, Ok("done"));
        // Non-zero generous timeout also completes.
        assert_eq!(wrap_like_task_mode(60).await, Ok("done"));
    }

    #[test]
    fn schema_has_required_fields() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
        let schema = tool.parameters_schema();
        // All params are optional at schema level; runtime validates per action
        let required = schema["required"].as_array().unwrap();
        assert!(required.is_empty(), "Required should be empty (validated at runtime)");
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["task"].is_object());
        assert!(schema["properties"]["run_id"].is_object());
        assert!(schema["properties"]["message"].is_object());
        assert!(schema["properties"]["model"].is_object());
        assert!(schema["properties"]["provider"].is_object());
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

    /// BUG-12: an inline `provider` override (no named agent) must drive a
    /// provider rebuild. Using an invalid provider name proves the override is
    /// consumed: provider creation fails naming the inline override, not the
    /// gateway provider ("test-provider").
    #[tokio::test]
    async fn inline_provider_override_drives_provider_rebuild() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));

        let result = tool
            .execute(with_spawn_grant(json!({
                "task": "do work",
                "provider": "totally-invalid-provider"
            })))
            .await
            .unwrap();

        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            err.contains("totally-invalid-provider"),
            "error should name the inline provider override: {err}"
        );
    }

    #[tokio::test]
    async fn missing_task_returns_error() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn empty_task_returns_failure() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
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
        tool.set_default_recipient(Some("test-recipient".to_string())).await;

        let result = tool
            .execute(with_spawn_grant(json!({"task": "Tell me a joke"})))
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

    /// A channel that records both its name and the messages it sent, so a test
    /// can assert *which* channel a sub-agent result was announced on.
    struct NamedRecordingChannel {
        name: &'static str,
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl NamedRecordingChannel {
        fn new(name: &'static str) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
            let sent = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    name,
                    sent: sent.clone(),
                }),
                sent,
            )
        }
    }

    #[async_trait::async_trait]
    impl Channel for NamedRecordingChannel {
        fn name(&self) -> &str {
            self.name
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.lock().await.push(message.content.clone());
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Regression for the cross-channel mis-routing bug: a sub-agent spawned from
    /// wacli (a `@g.us` group recipient) must announce its result back on the
    /// wacli channel — not on the construction-time default channel (which, in a
    /// multi-channel deployment, was Signal). The channel/gateway loop calls
    /// `set_active_channel` per message; this test simulates that switch and
    /// asserts the announcement lands only on the active (wacli) channel.
    #[tokio::test]
    async fn announce_routes_to_active_channel_not_construction_default() {
        // Tool is built with a "signal" default channel (mirrors the deployment
        // default that caused the bug).
        let (signal_ch, signal_sent) = NamedRecordingChannel::new("signal");
        let tool = make_tool(
            signal_ch,
            Arc::new(EchoProvider {
                response: "sub-agent done".into(),
            }),
        );

        // A wacli group message arrives: the gateway switches the active channel
        // and recipient before the spawn turn (exactly as channels/mod.rs does).
        let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
        tool.set_active_channel(wacli_ch as Arc<dyn Channel>).await;
        tool.set_active_recipient("120363000000000000@g.us").await;

        let result = tool
            .execute(with_spawn_grant(json!({"task": "do the thing"})))
            .await
            .unwrap();
        assert!(result.success, "spawn should succeed: {:?}", result.error);

        // Wait for the fire-and-forget sub-agent to finish and announce.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let on_wacli = wacli_sent.lock().await;
        let on_signal = signal_sent.lock().await;
        assert_eq!(
            on_wacli.len(),
            1,
            "result must be announced on the originating (wacli) channel"
        );
        assert!(on_wacli[0].contains("sub-agent done"));
        assert!(
            on_signal.is_empty(),
            "result must NOT be announced on the construction-time default (signal) channel"
        );
    }

    /// Build a tool whose announce/kill routing registry knows several named
    /// channels, so a test can assert a run resolves the *originating* channel by
    /// name from its per-turn scope rather than from shared "active" state.
    fn make_tool_with_channels(
        default_channel: Arc<dyn Channel>,
        provider: Arc<dyn crate::providers::Provider>,
        registry: Vec<Arc<dyn Channel>>,
    ) -> SessionsSpawnTool {
        let channels: HashMap<String, Arc<dyn Channel>> =
            registry.into_iter().map(|ch| (ch.name().to_string(), ch)).collect();
        make_tool(default_channel, provider).with_channels(Arc::new(channels))
    }

    /// Build a trusted per-turn spawn scope arg pinning the originating channel
    /// and chat_id (recipient) — mirrors the `_zc_scope` the agent loop injects
    /// for the message currently being processed.
    fn with_scope(mut args: serde_json::Value, channel: &str, chat_id: &str) -> serde_json::Value {
        if let Some(obj) = args.as_object_mut() {
            obj.insert("_zc_scope_trusted".to_string(), json!(true));
            obj.insert(
                "_zc_scope".to_string(),
                json!({
                    "sender": "alice",
                    "channel": channel,
                    "chat_type": "group",
                    "chat_id": chat_id,
                }),
            );
        }
        args
    }

    /// P0 concurrency race: announce must route by the run's *per-turn* channel +
    /// recipient (captured atomically from the launching message's scope), NOT by
    /// the shared "active" channel/recipient that a concurrently-processed message
    /// can overwrite between the spawning turn entering the LLM loop and the spawn
    /// actually executing.
    ///
    /// Scenario: message A arrives on `wacli` and begins a turn; before A's spawn
    /// executes, message B (on `signal`) overwrites the shared active
    /// channel/recipient (the gateway loop calls `set_active_*` per message). With
    /// the old shared-state model A's result would leak onto signal+B's recipient
    /// (cross-channel privacy leak). The fix binds A's announce to A's own scope.
    #[tokio::test]
    async fn announce_uses_per_turn_channel_not_shared_state() {
        let (signal_ch, signal_sent) = NamedRecordingChannel::new("signal");
        let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
        let tool = make_tool_with_channels(
            signal_ch.clone(),
            Arc::new(EchoProvider {
                response: "A's private result".into(),
            }),
            vec![signal_ch, wacli_ch],
        );

        // Message B (signal) has already overwritten the shared active state — this
        // is the racing message whose values would corrupt A under the old model.
        tool.set_active_channel({
            let (b_ch, _) = NamedRecordingChannel::new("signal");
            b_ch as Arc<dyn Channel>
        })
        .await;
        tool.set_active_recipient("B-signal-recipient").await;

        // A's spawn now executes, carrying A's own per-turn scope (wacli + A's
        // recipient). No explicit `recipient` arg, so it must come from the scope.
        let result = tool
            .execute(with_spawn_grant(with_scope(
                json!({"task": "do A's work"}),
                "wacli",
                "A-wacli-recipient",
            )))
            .await
            .unwrap();
        assert!(result.success, "spawn should succeed: {:?}", result.error);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let on_wacli = wacli_sent.lock().await;
        let on_signal = signal_sent.lock().await;
        assert_eq!(
            on_wacli.len(),
            1,
            "A's result must announce on its own (wacli) channel, not the shared active (signal) one"
        );
        assert!(on_wacli[0].contains("A's private result"));
        assert!(
            on_signal.is_empty(),
            "A's result must NOT leak onto signal (the racing message B's channel)"
        );

        // And the recipient must be A's scope chat_id, not B's shared recipient.
        let runs = tool.active_runs_snapshot().await;
        let a_run = runs.first().expect("one run registered");
        assert_eq!(a_run.recipient.as_deref(), Some("A-wacli-recipient"));
        assert_eq!(a_run.channel_name.as_deref(), Some("wacli"));
    }

    /// P0 concurrency race (kill variant): killing a run must notify on the run's
    /// per-turn channel + recipient bound at spawn time — never the shared active
    /// channel that a later, concurrently-processed message may have overwritten.
    #[tokio::test]
    async fn kill_uses_per_turn_channel_not_shared_state() {
        let (signal_ch, signal_sent) = NamedRecordingChannel::new("signal");
        let (wacli_ch, wacli_sent) = NamedRecordingChannel::new("wacli");
        let tool = make_tool_with_channels(
            signal_ch.clone(),
            // A long-lived run so it is still Running when we kill it.
            Arc::new(SleepyProvider {
                delay_ms: 5_000,
                response: "never reached".into(),
            }),
            vec![signal_ch, wacli_ch],
        );

        // Spawn A on wacli (its scope), in task mode so the abort handle exists.
        let result = tool
            .execute(with_spawn_grant(with_scope(
                json!({"task": "long A work", "mode": "task"}),
                "wacli",
                "A-wacli-recipient",
            )))
            .await
            .unwrap();
        assert!(result.success, "spawn should succeed: {:?}", result.error);
        let run_id = {
            let runs = tool.active_runs_snapshot().await;
            runs.first().expect("one run registered").id.clone()
        };

        // A concurrent message B (signal) overwrites the shared active channel
        // *before* the kill — exactly the race the fix must defeat.
        tool.set_active_channel({
            let (b_ch, _) = NamedRecordingChannel::new("signal");
            b_ch as Arc<dyn Channel>
        })
        .await;
        tool.set_active_recipient("B-signal-recipient").await;

        let kill_grant = serde_json::to_value(ApprovalGrant::for_resource_operation(
            "sessions_spawn",
            &format!("sessions_spawn:kill:{run_id}"),
            "test",
            None,
        ))
        .unwrap();
        let kill = tool
            .execute(json!({
                "action": "kill",
                "run_id": run_id,
                crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG: kill_grant,
            }))
            .await
            .unwrap();
        assert!(kill.success, "kill should succeed: {:?}", kill.error);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let on_wacli = wacli_sent.lock().await;
        let on_signal = signal_sent.lock().await;
        assert_eq!(on_wacli.len(), 1, "kill notice must route to A's own (wacli) channel");
        assert!(on_wacli[0].contains("killed"));
        assert!(
            on_signal.is_empty(),
            "kill notice must NOT leak onto signal (the racing message B's channel)"
        );
    }

    /// FIX-P0-37: spawning is a Medium-risk side effect. Under the default
    /// (supervised) autonomy and with NO approval grant supplied, the gate must
    /// deny the spawn outright — no run is registered and no announcement fires.
    #[tokio::test]
    async fn spawn_denied_without_grant_under_supervised() {
        let (ch, sent) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "should never run".into(),
            }),
        );
        tool.set_default_recipient(Some("test-recipient".to_string())).await;

        // No grant injected → Medium-risk gate denies under supervised autonomy.
        let denied = tool.execute(json!({"task": "Tell me a joke"})).await.unwrap();
        assert!(!denied.success, "spawn must be denied without an approval grant");
        assert!(
            denied.error.unwrap_or_default().contains("runtime approval grant"),
            "denial reason should reference the missing approval grant"
        );

        // No run should have been registered.
        assert!(tool.active_runs_snapshot().await.is_empty());

        // Give any (non-existent) async work a chance; nothing should be sent.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(sent.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn spawn_records_request_and_result_message_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "fabric result".into(),
            }),
        )
        .with_shared_memory(memory.clone());

        let result = tool
            .execute(with_spawn_grant(json!({
                "task": "write through fabric",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "sender": "alice",
                    "channel": "telegram",
                    "chat_type": "direct",
                    "chat_id": "chat-1",
                    "topic_id": "topic-1",
                    "source_message_event_id": "msg-1"
                }
            })))
            .await
            .unwrap();
        assert!(result.success);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let events = memory
            .list_message_events_since(
                &MemoryPrincipal {
                    workspace_id: "/tmp".to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("telegram:chat-1:alice".to_string()),
                    channel: Some("telegram".to_string()),
                    sender: Some("alice".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                10,
            )
            .await
            .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].source, "sessions_spawn");
        assert_eq!(events[0].role, "user");
        assert_eq!(events[0].owner_id.as_deref(), Some("owner:/tmp:telegram:alice"));
        assert_eq!(events[0].content, "write through fabric");
        assert_eq!(events[1].source, "sessions_spawn");
        assert_eq!(events[1].role, "event");
        assert_eq!(events[1].owner_id.as_deref(), Some("owner:/tmp:telegram:alice"));
        assert!(events[1].content.contains("fabric result"));
        let request_payload: serde_json::Value = serde_json::from_str(events[0].raw_payload_json.as_deref().unwrap())
            .expect("request payload should be json");
        assert_eq!(request_payload["topic_id"].as_str(), Some("topic-1"));
        assert_eq!(request_payload["source_message_event_id"].as_str(), Some("msg-1"));

        let memory_events = memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    workspace_id: "/tmp".to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("telegram:chat-1:alice".to_string()),
                    channel: Some("telegram".to_string()),
                    sender: Some("alice".to_string()),
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                20,
            )
            .await
            .unwrap();
        let task_events = memory_events
            .iter()
            .filter(|event| event.subject_table == "tasks")
            .collect::<Vec<_>>();
        assert_eq!(
            task_events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec!["task.spawned", "task.completed"]
        );
        assert!(
            task_events
                .iter()
                .all(|event| event.subject_id == events[0].run_id.as_deref().unwrap())
        );
        let task_payload: serde_json::Value =
            serde_json::from_str(task_events[0].payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(task_payload["topic_id"].as_str(), Some("topic-1"));
        assert_eq!(task_payload["source_message_event_id"].as_str(), Some("msg-1"));
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
            .execute(with_spawn_grant(json!({"task": "Do something"})))
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
        tool.set_default_recipient(Some("default-recipient".to_string())).await;

        let result = tool
            .execute(with_spawn_grant(json!({
                "task": "Test task",
                "recipient": "explicit-recipient"
            })))
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
            Arc::new(EchoProvider { response: "ok".into() }),
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
            Arc::new(EchoProvider { response: "ok".into() }),
            spawn_cfg,
        );

        let result = SPAWN_EXECUTION_CONTEXT
            .scope(
                SpawnExecutionContext {
                    run_id: "parent-run".to_string(),
                    session_scope_key: "signal:group:test".to_string(),
                    spawn_depth: 0,
                    owner_id: Some("owner-a".to_string()),
                    topic_id: Some("topic-a".to_string()),
                    source_message_event_id: Some("msg-a".to_string()),
                    // A real spawn-run parent (not a turn root): the next hop is
                    // depth 1, which exceeds max_spawn_depth=0 and is rejected.
                    is_turn_root: false,
                },
                async { tool.execute(json!({"task": "nested"})).await },
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("max_spawn_depth"));
    }

    /// D8-4: a turn-root context (is_turn_root = true, spawn_depth = 0) must NOT
    /// tighten the max_spawn_depth boundary. With max_spawn_depth = 0, a spawn
    /// directly from a turn root computes the child's depth as 0 (identical to the
    /// no-context case), so it is allowed — unlike a real spawn-run parent at
    /// depth 0, which would compute depth 1 and be rejected (see
    /// spawn_rejected_when_depth_exceeded above).
    #[tokio::test]
    async fn turn_root_seed_does_not_tighten_max_spawn_depth() {
        let (ch, _) = RecordingChannel::new();
        let mut spawn_cfg = crate::config::SessionsSpawnConfig::default();
        spawn_cfg.max_spawn_depth = 0;
        let tool = make_tool_with_spawn_config(
            Arc::new(ch),
            Arc::new(EchoProvider { response: "ok".into() }),
            spawn_cfg,
        );
        tool.set_default_recipient(Some("test-recipient".to_string())).await;

        let result = SPAWN_EXECUTION_CONTEXT
            .scope(
                SpawnExecutionContext::seed_turn_context(
                    "turn-root-run".to_string(),
                    "signal:+15551234567:openprx_user".to_string(),
                ),
                async { tool.execute(with_spawn_grant(json!({"task": "child of a turn"}))).await },
            )
            .await
            .unwrap();

        assert!(
            result.success,
            "a turn-root seed must not tighten max_spawn_depth=0 (first child depth is 0)"
        );

        let runs = tool.active_runs_snapshot().await;
        let child = runs.first().expect("the spawned child run must be registered");
        assert_eq!(child.spawn_depth, 0, "turn-root first child must compute spawn_depth 0");
        assert_eq!(
            child.parent_run_id.as_deref(),
            Some("turn-root-run"),
            "child must inherit parent_run_id = the per-turn run_id"
        );
    }

    /// D8-4: a real spawn-run parent at depth 0 (is_turn_root = false) is the
    /// boundary the turn-root case must NOT mimic: its first child computes depth
    /// 1, which exceeds max_spawn_depth = 0 and is rejected. This is the
    /// complement of turn_root_seed_does_not_tighten_max_spawn_depth (same
    /// spawn_depth = 0 seed, opposite is_turn_root, opposite outcome).
    #[tokio::test]
    async fn spawn_run_parent_at_depth_zero_still_rejects_next_hop() {
        let (ch, _) = RecordingChannel::new();
        let mut spawn_cfg = crate::config::SessionsSpawnConfig::default();
        spawn_cfg.max_spawn_depth = 0;
        let tool = make_tool_with_spawn_config(
            Arc::new(ch),
            Arc::new(EchoProvider { response: "ok".into() }),
            spawn_cfg,
        );
        tool.set_default_recipient(Some("test-recipient".to_string())).await;

        let result = SPAWN_EXECUTION_CONTEXT
            .scope(
                SpawnExecutionContext {
                    run_id: "spawn-run".to_string(),
                    session_scope_key: "signal:+15551234567:openprx_user".to_string(),
                    spawn_depth: 0,
                    owner_id: None,
                    topic_id: None,
                    source_message_event_id: None,
                    is_turn_root: false,
                },
                async { tool.execute(with_spawn_grant(json!({"task": "nested"}))).await },
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
            Arc::new(EchoProvider { response: "ok".into() }),
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
        assert!(result.error.unwrap_or_default().contains("max_children_per_agent"));
    }

    #[tokio::test]
    async fn failed_provider_announces_error() {
        let (ch, sent) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(FailingProvider));
        tool.set_default_recipient(Some("user".to_string())).await;

        let result = tool
            .execute(with_spawn_grant(json!({"task": "This will fail"})))
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
            .execute(with_spawn_grant(json!({"task": "Some task"})))
            .await
            .unwrap();

        let runs = tool.active_runs_snapshot().await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].task, "Some task");
    }

    /// Bug-V5-1 regression: a `sessions_spawn` call made *inside* a turn-root
    /// `SPAWN_EXECUTION_CONTEXT` scope (i.e. the model invoking the tool mid-turn)
    /// must capture the per-turn run id as the child's `parent_run_id`. The
    /// capture is synchronous — read at the top of `execute`, **before** any
    /// `tokio::spawn` — so the task-local is always present when read and never
    /// lost across the spawn boundary. The chat `/sessions` projection reads this
    /// `Some(parent)` as model-origin (see `SessionOrigin::from_parent_run_id`,
    /// asserted in the chat-side `model.rs` tests, which can reach that module).
    #[tokio::test]
    async fn model_spawn_within_turn_scope_captures_parent_run_id() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
        tool.set_default_recipient(Some("test-recipient".to_string())).await;

        let result = SPAWN_EXECUTION_CONTEXT
            .scope(
                SpawnExecutionContext::seed_turn_context("turn-run-xyz".to_string(), "chat:session-1".to_string()),
                async { tool.execute(with_spawn_grant(json!({"task": "model child"}))).await },
            )
            .await
            .unwrap();
        assert!(result.success, "spawn inside a turn scope must succeed");

        let runs = tool.active_runs_snapshot().await;
        let child = runs.first().expect("the spawned child run must be registered");
        assert_eq!(
            child.parent_run_id.as_deref(),
            Some("turn-run-xyz"),
            "child captured the per-turn run id as parent before the spawn boundary (=> model origin)"
        );
    }

    /// Bug-V5-1 complement: a `sessions_spawn` call made with **no**
    /// `SPAWN_EXECUTION_CONTEXT` in scope (the operator `/bg` slash-command path,
    /// dispatched outside the turn tool-loop scope) carries no `parent_run_id`,
    /// which the chat projection reads as user-origin.
    #[tokio::test]
    async fn user_spawn_without_scope_has_no_parent_run_id() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
        tool.set_default_recipient(Some("test-recipient".to_string())).await;

        // No `.scope(...)` wrapper: the task-local is absent, exactly as on the
        // operator slash-command path.
        let result = tool
            .execute(with_spawn_grant(json!({"task": "operator child"})))
            .await
            .unwrap();
        assert!(result.success);

        let runs = tool.active_runs_snapshot().await;
        let child = runs.first().expect("the spawned child run must be registered");
        assert_eq!(
            child.parent_run_id, None,
            "no spawn-execution context means no parent_run_id (=> user origin)"
        );
    }

    #[tokio::test]
    async fn spawn_action_obeys_readonly_resource_gate() {
        let (ch, _) = RecordingChannel::new();
        let readonly_security = Arc::new(SecurityPolicy {
            autonomy: crate::security::policy::AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = SessionsSpawnTool::new(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "done".into(),
            }),
            "test-provider",
            "test-model",
            0.7,
            readonly_security,
            std::path::PathBuf::from("/tmp"),
            crate::config::MultimodalConfig::default(),
            crate::config::AgentCompactionConfig::default(),
            HashMap::new(),
            None,
            crate::providers::ProviderRuntimeOptions::default(),
            crate::config::SessionsSpawnConfig::default(),
        );

        let result = tool.execute(json!({"task": "blocked"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("read-only mode"));
    }

    #[tokio::test]
    async fn kill_action_requires_resource_grant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "done".into(),
            }),
        )
        .with_shared_memory(memory.clone());
        {
            let mut runs = tool.active_runs.write().await;
            runs.push(SubAgentRun {
                id: "run-1".to_string(),
                task: "task".to_string(),
                owner_id: Some("owner-a".to_string()),
                topic_id: Some("topic-a".to_string()),
                source_message_event_id: Some("msg-a".to_string()),
                started_at: Utc::now(),
                status: SubAgentStatus::Running,
                recipient: None,
                channel_name: None,
                abort_handle: None,
                history: Arc::new(RwLock::new(Vec::new())),
                steer_tx: None,
                parent_run_id: None,
                session_scope_key: "test-session".to_string(),
                spawn_depth: 0,
            });
        }

        let denied = tool
            .execute(json!({"action": "kill", "run_id": "run-1"}))
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(denied.error.unwrap_or_default().contains("runtime approval grant"));

        let allowed = tool
            .execute(json!({
                "action": "kill",
                "run_id": "run-1",
                crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG:
                    serde_json::to_value(ApprovalGrant::for_resource_operation(
                        "sessions_spawn",
                        "sessions_spawn:kill:run-1",
                        "test",
                        None
                    )).unwrap()
            }))
            .await
            .unwrap();
        assert!(allowed.success);

        let events = memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    workspace_id: "/tmp".to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("test-session".to_string()),
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        let killed = events
            .iter()
            .find(|event| event.event_type == "task.killed")
            .expect("task.killed event should be persisted");
        assert_eq!(killed.subject_table, "tasks");
        assert_eq!(killed.subject_id, "run-1");
        let payload: serde_json::Value = serde_json::from_str(killed.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(payload["owner_id"].as_str(), Some("owner-a"));
        assert_eq!(payload["topic_id"].as_str(), Some("topic-a"));
        assert_eq!(payload["source_message_event_id"].as_str(), Some("msg-a"));
    }

    #[tokio::test]
    async fn active_runs_store_owner_topic_lineage() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(
            Arc::new(ch),
            Arc::new(EchoProvider {
                response: "done".into(),
            }),
        );

        let result = tool
            .execute(with_spawn_grant(json!({
                "task": "lineage task",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "sender": "alice",
                    "channel": "telegram",
                    "chat_type": "direct",
                    "chat_id": "chat-1",
                    "topic_id": "topic-a",
                    "task_id": "parent-task",
                    "message_event_id": "msg-a"
                }
            })))
            .await
            .unwrap();
        assert!(result.success);

        let runs = tool.active_runs_snapshot().await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].owner_id.as_deref(), Some("owner:/tmp:telegram:alice"));
        assert_eq!(runs[0].topic_id.as_deref(), Some("topic-a"));
        assert_eq!(runs[0].parent_run_id.as_deref(), None);
        assert_eq!(runs[0].source_message_event_id.as_deref(), Some("msg-a"));
    }

    #[tokio::test]
    async fn default_recipient_handle_shared() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
        let handle = tool.default_recipient_handle();
        *handle.write().await = Some("via-handle".to_string());

        let val = tool.default_recipient.read().await.clone();
        assert_eq!(val.as_deref(), Some("via-handle"));
    }

    #[tokio::test]
    async fn history_action_returns_no_run_error() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
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
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
        let result = tool.execute(json!({"action": "history"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn steer_action_requires_message() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
        let result = tool.execute(json!({"action": "steer", "run_id": "xxx"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn steer_action_returns_no_run_error() {
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
        let result = tool
            .execute(json!({"action": "steer", "run_id": "nonexistent", "message": "pivot!"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("No run found"));
    }

    #[tokio::test]
    async fn steer_action_persists_task_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let (ch, _) = RecordingChannel::new();
        let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }))
            .with_shared_memory(memory.clone());
        let (steer_tx, mut steer_rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut runs = tool.active_runs.write().await;
            runs.push(SubAgentRun {
                id: "run-steer".to_string(),
                task: "task".to_string(),
                owner_id: Some("owner-a".to_string()),
                topic_id: Some("topic-a".to_string()),
                source_message_event_id: Some("msg-a".to_string()),
                started_at: Utc::now(),
                status: SubAgentStatus::Running,
                recipient: None,
                channel_name: None,
                abort_handle: None,
                history: Arc::new(RwLock::new(Vec::new())),
                steer_tx: Some(steer_tx),
                parent_run_id: None,
                session_scope_key: "test-session".to_string(),
                spawn_depth: 0,
            });
        }

        let result = tool
            .execute(json!({"action": "steer", "run_id": "run-steer", "message": "pivot now"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(steer_rx.recv().await.as_deref(), Some("pivot now"));

        let events = memory
            .list_memory_events_since(
                &MemoryPrincipal {
                    workspace_id: "/tmp".to_string(),
                    agent_id: None,
                    persona_id: None,
                    session_key: Some("test-session".to_string()),
                    channel: None,
                    sender: None,
                    owner_id: None,
                    legacy_session_key: None,
                },
                0,
                10,
            )
            .await
            .unwrap();
        let steered = events
            .iter()
            .find(|event| event.event_type == "task.steered")
            .expect("task.steered event should be persisted");
        assert_eq!(steered.subject_table, "tasks");
        assert_eq!(steered.subject_id, "run-steer");
        let payload: serde_json::Value = serde_json::from_str(steered.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(payload["owner_id"].as_str(), Some("owner-a"));
        assert_eq!(payload["detail"]["message_preview"].as_str(), Some("pivot now"));
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
            .execute(with_spawn_grant(json!({"task": "Do a thing"})))
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

    #[tokio::test]
    async fn spawn_rejects_unknown_agent() {
        let (ch, _) = RecordingChannel::new();
        let mut agents = HashMap::new();
        agents.insert("alpha".to_string(), make_agent_config(None));
        let tool = SessionsSpawnTool::new(
            Arc::new(ch),
            Arc::new(EchoProvider { response: "ok".into() }),
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
            .execute(with_spawn_grant(json!({"task": "hello", "agent": "missing"})))
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
            Arc::new(EchoProvider { response: "ok".into() }),
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
            .execute(with_spawn_grant(json!({"task": "hello", "agent": "alpha"})))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("spawn_enabled=false"));
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
        tool.set_default_recipient(Some("test-recipient".to_string())).await;

        let result = tool
            .execute(with_spawn_grant(json!({"task": "t", "agent": "alpha"})))
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
            parent_capability: Some("capability".to_string()),
            run_id: "run".to_string(),
            task: "say \"hello\"".to_string(),
            provider_name: "provider".to_string(),
            model: "model".to_string(),
            api_key: None,
            temperature: 0.7,
            workspace_dir: std::path::PathBuf::from("/tmp/ws"),
            memory_db_path: std::path::PathBuf::from("/tmp/ws/brain.db"),
            memory_workspace_id: Some("/tmp/ws".to_string()),
            memory_strategy: Some("shared_fabric".to_string()),
            shared_memory_db_path: Some(std::path::PathBuf::from("/tmp/ws/memory/brain.db")),
            worker_memory_db_path: Some(std::path::PathBuf::from("/tmp/worker/brain.db")),
            agent_id: Some("agent-a".to_string()),
            persona_id: None,
            memory_event_recording: MemoryEventRecording::default(),
            allowed_tools: vec!["shell".to_string()],
            timeout_seconds: 30,
            max_iterations: 20,
            system_prompt: None,
            identity_dir: None,
            scope_sender: None,
            scope_channel: None,
            scope_chat_type: None,
            scope_chat_id: None,
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            spawn_depth: 0,
            session_scope_key: "sessions_spawn:global".to_string(),
            parent_run_id: None,
            compaction_config: None,
        };

        let args = build_session_worker_cli_args(&manifest).unwrap();
        let task_index = args.iter().position(|arg| arg == "--task").unwrap();
        assert_eq!(args[task_index + 1], manifest.task);
    }

    #[test]
    fn process_mode_manifest_uses_parent_workspace_memory_db() {
        let workspace = std::path::Path::new("/tmp/openprx-workspace");
        assert_eq!(
            shared_worker_memory_db_path(workspace),
            workspace.join("memory").join("brain.db")
        );
        assert_eq!(
            private_worker_memory_db_path(std::path::Path::new("/tmp/openprx-worker")),
            std::path::Path::new("/tmp/openprx-worker").join("brain.db")
        );
    }

    #[test]
    fn process_memory_strategy_is_explicitly_validated() {
        assert_eq!(normalize_process_memory_strategy("").unwrap(), "shared_fabric");
        assert_eq!(
            normalize_process_memory_strategy("shared_fabric").unwrap(),
            "shared_fabric"
        );
        assert_eq!(
            normalize_process_memory_strategy("isolated_private").unwrap(),
            "isolated_private"
        );
        assert_eq!(normalize_process_memory_strategy("hybrid").unwrap(), "hybrid");
        assert!(normalize_process_memory_strategy("worker-only").is_err());
    }

    #[test]
    fn spawn_event_scope_is_derived_from_runtime_envelope() {
        let scope = SpawnScope {
            sender: "alice".to_string(),
            channel: "telegram".to_string(),
            chat_type: "direct".to_string(),
            chat_id: "chat-1".to_string(),
            owner_id: None,
            topic_id: Some("topic-a".to_string()),
            parent_task_id: Some("parent-task".to_string()),
            source_message_event_id: Some("msg-a".to_string()),
        };
        let event_scope = spawn_event_scope(
            "/tmp/ws",
            "run-child",
            "telegram:chat-1:alice",
            Some("run-parent"),
            Some("agent-a"),
            Some(&scope),
        );

        assert_eq!(event_scope.source, "sessions_spawn");
        assert_eq!(event_scope.channel.as_deref(), Some("telegram"));
        assert_eq!(event_scope.session_key.as_deref(), Some("telegram:chat-1:alice"));
        assert_eq!(event_scope.run_id.as_deref(), Some("run-child"));
        assert_eq!(event_scope.parent_run_id.as_deref(), Some("run-parent"));
        assert_eq!(event_scope.agent_id.as_deref(), Some("agent-a"));
        assert_eq!(event_scope.sender.as_deref(), Some("alice"));
        assert_eq!(event_scope.recipient.as_deref(), Some("chat-1"));
        assert_eq!(event_scope.owner_id.as_deref(), Some("owner:/tmp/ws:telegram:alice"));
        let lineage = spawn_lineage(&event_scope, None, Some(&scope));
        assert_eq!(lineage.owner_id.as_deref(), Some("owner:/tmp/ws:telegram:alice"));
        assert_eq!(lineage.topic_id.as_deref(), Some("topic-a"));
        assert_eq!(lineage.parent_task_id.as_deref(), Some("parent-task"));
        assert_eq!(lineage.source_message_event_id.as_deref(), Some("msg-a"));
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

        let result = wait_with_parent_timeout(&mut child, std::time::Duration::from_millis(50)).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("session-worker exceeded parent timeout")
        );
    }

    #[test]
    fn isolated_memory_prefixes_key() {
        assert_eq!(memory_key_prefix("alpha", "plan"), "alpha:plan");
        assert_eq!(memory_key_prefix("alpha", "alpha:plan"), "alpha:plan");
    }

    /// Build a bare `SubAgentRun` with the given id and status for unit-testing
    /// the deterministic approval-suspension restore logic.
    fn restore_test_run(id: &str, status: SubAgentStatus) -> SubAgentRun {
        SubAgentRun {
            id: id.to_string(),
            task: "t".to_string(),
            owner_id: None,
            topic_id: None,
            source_message_event_id: None,
            started_at: Utc::now(),
            status,
            recipient: None,
            channel_name: None,
            abort_handle: None,
            history: Arc::new(RwLock::new(Vec::new())),
            steer_tx: None,
            parent_run_id: None,
            session_scope_key: "s".to_string(),
            spawn_depth: 0,
        }
    }

    /// NeedsInput: after a cancel/steer ends a suspended approval, the run must be
    /// deterministically restored from `AwaitingInput` back to `Running` (no
    /// zombie `AwaitingInput` left behind once it is running again).
    #[test]
    fn restore_run_downgrades_awaiting_input_to_running() {
        let mut runs = vec![restore_test_run(
            "r1",
            SubAgentStatus::AwaitingInput {
                prompt: "shell(rm -rf /tmp/x)".to_string(),
            },
        )];
        restore_run_to_running(&mut runs, "r1");
        assert!(
            matches!(runs[0].status, SubAgentStatus::Running),
            "AwaitingInput must be restored to Running on resume"
        );
    }

    /// NeedsInput: a kill that already moved the run to a terminal `Failed` state
    /// must NOT be resurrected to `Running` by the resume restore (terminal wins).
    #[test]
    fn restore_run_does_not_resurrect_terminal_failed() {
        let mut runs = vec![restore_test_run("r2", SubAgentStatus::Failed("killed".to_string()))];
        restore_run_to_running(&mut runs, "r2");
        assert!(
            matches!(runs[0].status, SubAgentStatus::Failed(ref m) if m == "killed"),
            "a terminal Failed (kill) state must never be overwritten by Running"
        );
    }

    /// NeedsInput: a completed run must likewise stay terminal.
    #[test]
    fn restore_run_does_not_resurrect_terminal_completed() {
        let mut runs = vec![restore_test_run("r3", SubAgentStatus::Completed("ok".to_string()))];
        restore_run_to_running(&mut runs, "r3");
        assert!(
            matches!(runs[0].status, SubAgentStatus::Completed(ref m) if m == "ok"),
            "a terminal Completed state must never be overwritten by Running"
        );
    }

    /// NeedsInput: an already-Running run is left as-is (idempotent), and an
    /// unknown run id is a harmless no-op.
    #[test]
    fn restore_run_is_idempotent_and_ignores_unknown_id() {
        let mut runs = vec![restore_test_run("r4", SubAgentStatus::Running)];
        restore_run_to_running(&mut runs, "r4");
        assert!(matches!(runs[0].status, SubAgentStatus::Running));
        // Unknown id: no panic, no change.
        restore_run_to_running(&mut runs, "does-not-exist");
        assert!(matches!(runs[0].status, SubAgentStatus::Running));
    }
}
