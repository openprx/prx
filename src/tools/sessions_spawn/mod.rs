//! Async sub-agent spawning tool — fire-and-forget with auto-announce on completion.
//!
//! Aligns with OpenClaw's `sessions_spawn` pattern:
//! - Accepts a task description and optional model/timeout
//! - Spawns a tokio task that runs an isolated agent loop
//! - Returns immediately with a run ID
//! - On completion, sends the result back through the channel automatically
//! - `history` action: view the conversation log of any sub-agent run
//! - `steer` action: inject a message into a running sub-agent's context

mod batch_journal;
mod process_terminal;
mod task_control;
mod task_terminal;
pub(crate) use task_control::{TaskFinalization, TaskRunControl};
mod actions;
mod announce;
mod batch;
mod builders;
mod chat_assign;
mod child_lifecycle;
mod events;
mod failure;
mod memory_scope;
mod monitor;
mod owned_process;
mod process_control;
mod process_mode;
mod provider;
mod scope;
mod spawn;
mod task_mode;
mod terminal_commit;
#[cfg(test)]
mod test_worker;
mod tool;
mod tool_proxy;
mod worker_capability;
mod worker_manifest;

pub(crate) use self::actions::{STEER_CHANNEL_CAPACITY, steering_instruction};
pub(crate) use self::announce::{AnnounceOrigin, announce_is_authorized};
use self::announce::{OutboundNotice, deliver_or_withhold_notice, format_announce_message};
use self::batch::{BatchMember, BatchRoster, resolve_pinned_child_compaction};
#[cfg(test)]
use self::batch::{JoinedMember, JoinedVerdict, join_batch_members, join_summary, settled_verdict};
use self::child_lifecycle::{
    AbortOnDrop, cleanup_owned_child_after_panic, cleanup_process_worker_workspace, recover_operator_interruption,
    recover_ordinary_interruption, run_spawned_child_lifecycle,
};
#[cfg(test)]
use self::child_lifecycle::{drain_worker_stream, pump_worker_steer_frames};
use self::events::{record_spawn_request_event, record_spawn_result_event, spawn_event_scope};
pub(crate) use self::failure::KILLED_BY_USER_REASON;
use self::failure::{
    EXITED_WITHOUT_RESULT_PREFIX, PANICKED_BEFORE_RESULT_SUFFIX, PROCESS_OWNER_PANICKED_PREFIX,
    TERMINATED_BEFORE_RESULT_SUFFIX, TerminationCause, exited_without_result_reason, spawn_completion_note,
    sub_agent_reported_failure, task_failure_reason, termination_cause,
};
#[cfg(test)]
use self::failure::{failure_gave_no_conclusion, failure_is_kill};
use self::memory_scope::memory_key_prefix;
pub(crate) use self::memory_scope::{MemoryScope, parse_memory_scope};
#[cfg(test)]
pub(crate) use self::monitor::attach_test_task_mode_owner;
#[cfg(test)]
use self::monitor::vanished_monitor_reason;
use self::monitor::{watch_process_mode_monitor, watch_task_mode_monitor};
#[cfg(test)]
use self::owned_process::{
    OwnedChildExit, ProcessOutputDrain, drain_process_output_after_leader_exit, wait_for_owned_process,
    wait_for_reap_after_group_termination,
};
use self::owned_process::{
    OwnedProcessGroup, OwnedProcessPhase, ProcessWorkerOutcome, WorkerSteerPipe, describe_worker_exit,
    hold_unresolved_child_ownership, relinquish_process_group_after_leader_exit, run_owned_child_phase,
    start_kill_and_reap_direct_child, terminate_owned_child,
};
pub(crate) use self::process_control::{ProcessFinalization, ProcessRunControl, ProcessTerminationRequestResult};
use self::process_control::{ProcessModeRunSeed, new_process_mode_run};
#[cfg(test)]
use self::process_mode::classify_worker_output;
use self::process_mode::run_sub_agent_process;
use self::provider::resolve_child_provider_api_key;
pub(crate) use self::scope::{SPAWN_EXECUTION_CONTEXT, SpawnExecutionContext, ambient_channel_excluded_tools};
use self::scope::{
    SpawnLineage, SpawnScope, current_spawn_execution_context, parse_spawn_scope, spawn_lineage,
    spawn_session_scope_key,
};
#[cfg(test)]
use self::task_mode::restore_run_to_running;
use self::task_mode::run_sub_agent_task;
#[cfg(test)]
use self::terminal_commit::ProcessTerminalCommitHook;
use self::terminal_commit::{
    commit_process_owner_failure_if_unfinalized, commit_process_terminal_state, commit_run_termination_if_unfinished,
    register_killable_process_run,
};
#[cfg(test)]
use self::test_worker::{
    TestWorkerExecutable, TestWorkerProtocolDiagnostic, TestWorkerProtocolTerminal, TestWorkerRecoveryResult,
    TestWorkerRouteObservation, test_worker_cleanup_error_class, test_worker_process_identity,
};
#[cfg(test)]
pub(crate) use self::test_worker::{TestWorkerProtocolCase, TestWorkerProtocolControl, TestWorkerRouteCapture};
use self::tool_proxy::AllowedToolProxy;
pub(crate) use self::tool_proxy::{refresh_tools_for_explicit_allowlist, resolve_tools_for_agent};
#[cfg(test)]
use self::worker_capability::session_secret_path;
use self::worker_capability::{
    SESSION_WORKER_CAP_ENV, SESSION_WORKER_CAP_EXPIRY_ENV, SESSION_WORKER_CAP_TTL_SECS, capability_now_unix,
    compute_worker_capability_with_key, existing_session_worker_signing_key, manifest_signing_payload,
    seal_worker_capability, session_worker_signing_key,
};
use self::worker_manifest::{
    build_session_worker_cli_args, build_session_worker_manifest, copy_dir_recursive,
    normalize_process_memory_strategy, private_worker_memory_db_path, shared_worker_memory_db_path,
};
use crate::agent::turn_request::SpawnEventSink;
use crate::channels::traits::Channel;
use crate::config::DelegateAgentConfig;
use crate::memory::Memory;
use crate::memory::MemoryEventRecording;
use crate::memory::MemoryFabric;
use crate::providers;
#[cfg(test)]
use crate::providers::Provider;
use crate::security::SecurityPolicy;
use crate::tools::traits::Tool;
use chrono::DateTime;
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use tokio::sync::RwLock;

const PROCESS_OUTPUT_DRAIN_TIMEOUT_SECS: u64 = 1;
const PROCESS_REAP_TIMEOUT_SECS: u64 = 5;
const PROCESS_TERMINATION_REQUEST_TIMEOUT_SECS: u64 = 5;
const MAX_SUBPROCESS_OUTPUT: u64 = 10 * 1024 * 1024;
const DEFAULT_SUB_AGENT_SYSTEM_PROMPT: &str = "\
You are a sub-agent handling a specific delegated task. \
Complete the task thoroughly and report results concisely. \
Focus only on the assigned task; do not ask clarifying questions.";
const PROCESS_MEMORY_STRATEGY_SHARED: &str = "shared_fabric";
const PROCESS_MEMORY_STRATEGY_ISOLATED: &str = "isolated_private";
const PROCESS_MEMORY_STRATEGY_HYBRID: &str = "hybrid";
const DEFAULT_HISTORY_LAST_N: usize = 20;
const DEFAULT_HISTORY_ENTRY_MAX_CHARS: usize = 800;

// Keep the large spawn state machine off the execution service's poll frame.
// Construct it inside this synchronous boundary and erase its concrete type.
#[inline(never)]
fn boxed_spawn_field<'a, F>(
    make: impl FnOnce() -> F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + Send + 'a>>
where
    F: std::future::Future + Send + 'a,
{
    Box::pin(make())
}

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
    pub finished_at: Option<DateTime<Utc>>,
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
    /// Owner-mediated lifecycle control for process-mode runs. This deliberately
    /// exposes no PID/PGID: only the monitor that owns `Child` may signal and
    /// reap it. Task-mode runs leave this as `None` and retain abort semantics.
    pub(crate) process_control: Option<Arc<ProcessRunControl>>,
    /// Observer-owned task cleanup acknowledgement; never inferred from abort().
    pub(crate) task_control: Option<Arc<TaskRunControl>>,
    /// Accumulated conversation history from the sub-agent's execution.
    pub history: Arc<RwLock<Vec<HistoryEntry>>>,
    /// Channel to inject steering messages into the running sub-agent.
    ///
    /// Bounded at [`STEER_CHANNEL_CAPACITY`]; see that constant for why.
    pub steer_tx: Option<tokio::sync::mpsc::Sender<String>>,
    pub parent_run_id: Option<String>,
    pub session_scope_key: String,
    pub spawn_depth: usize,
    pub token_usage_records: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    /// Liveness evidence for this run: the beat every progress event inside it
    /// stamps.
    ///
    /// Deliberately *not* a second definition of "what counts as progress".
    /// The sub-agent's own turn already runs under
    /// [`crate::agent::idle::run_guarded`], whose beat is parented on this one,
    /// so every [`crate::agent::idle::ProgressKind`] event — provider chunk,
    /// tool start/end, compaction, channel output — lands here for free. A
    /// process-mode run has no in-process turn to borrow, so it stamps this
    /// beat from the worker's output stream, the only progress signal that
    /// crosses the process boundary.
    ///
    /// Read through [`SubAgentRun::last_progress_at`] /
    /// [`SubAgentRun::idle_for`]; both are relaxed atomic loads, so a
    /// supervisor may poll them without disturbing the run.
    pub progress: Arc<crate::agent::idle::ProgressBeat>,
    /// Fan-out batch this run belongs to, when it was started by
    /// `action: "spawn_batch"`.
    ///
    /// A label carried by each member rather than a lineage relationship,
    /// because the lineage does not survive: a batch's members are *siblings*
    /// whose common parent is the `sessions_spawn` tool call that launched
    /// them, and `ToolExecutionService` retires that registry row the moment
    /// the call returns — long before the members finish.
    /// [`crate::runtime::registry::register_sub_agent`] records this field for
    /// exactly that reason.
    ///
    /// So `prx tasks kill <parent-turn>` is **not** a batch kill, and neither is
    /// the idle detector's `kill_turn_subtree`: both walk parent links down from
    /// the turn and stop at the retired tool-call row, so the members are not
    /// among the targets. That is the intended shape rather than a gap — a
    /// sub-agent outlives the turn that launched it on purpose, which is what
    /// lets a spawn announce its result after that turn has ended. Killing a
    /// whole fan-out is `prx tasks kill <batch-id>`
    /// ([`crate::runtime::registry::kill_batch`]), which resolves this label to
    /// the member rows and applies the same per-member cascade `kill` would.
    ///
    /// It is what `action: "join"` selects on and what `sessions_list` groups
    /// by. `join` takes the batch's *membership* from the roster `spawn_batch`
    /// recorded rather than from this label, because a member's row can be
    /// retired out of `active_runs` before the join reads it — see
    /// [`SessionsSpawnTool::batch_members`].
    pub batch_id: Option<String>,
}

impl SubAgentRun {
    /// How long this run has been silent.
    #[must_use]
    pub fn idle_for(&self) -> std::time::Duration {
        self.progress.idle_for()
    }

    /// Wall-clock instant of the last observed progress event.
    ///
    /// Derived from the beat's monotonic clock rather than stored as a
    /// wall-clock stamp, so a system clock step cannot make a live run look
    /// stalled (or a stalled one look live). Never earlier than
    /// [`Self::started_at`], and equal to it while the run has produced nothing
    /// yet.
    #[must_use]
    pub fn last_progress_at(&self) -> DateTime<Utc> {
        chrono::Duration::from_std(self.idle_for())
            .ok()
            .and_then(|idle| Utc::now().checked_sub_signed(idle))
            .map_or(self.started_at, |at| at.max(self.started_at))
    }
}

/// Count the runs that are still live, for the spawn-time visibility log.
///
/// Nothing rejects a spawn on this number; it exists so an operator reading the
/// log can watch fan-out grow, the same way the runtime registry lets them see
/// and kill it.
fn running_run_count(runs: &[SubAgentRun]) -> usize {
    runs.iter()
        // A suspended (AwaitingInput) run is still live — it has not released
        // its process or its history — so it counts here.
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

/// The tool registry a `sessions_spawn` tool runs its sub-agents with,
/// bound once after the registry is built.
///
/// It holds the registry weakly. The tool is itself an entry of that
/// registry, so a strong reference was a cycle that kept every superseded
/// Gateway runtime generation alive, and with it the MCP tool and one stdio
/// child per used server for each configuration change (D-097 M-3). Whoever
/// runs a turn holds the registry, so it is alive while the tool executes;
/// a spawned run takes its own strong reference for as long as it runs.
#[derive(Default)]
pub struct ParentToolRegistry(OnceLock<Weak<Vec<Box<dyn Tool>>>>);

impl ParentToolRegistry {
    /// Bind `registry`; `false` when a registry was already bound.
    pub fn set(&self, registry: &Arc<Vec<Box<dyn Tool>>>) -> bool {
        self.0.set(Arc::downgrade(registry)).is_ok()
    }

    /// The bound registry: `Ok(None)` when none was bound (the tool then
    /// runs a sub-agent without tools, as before any registry existed).
    ///
    /// # Errors
    ///
    /// When a registry was bound and is gone: the configuration the tool was
    /// built for was replaced and its registry released. The tool then
    /// refuses to spawn rather than run a sub-agent silently without its
    /// tools (D-098).
    pub fn get(&self) -> anyhow::Result<Option<Arc<Vec<Box<dyn Tool>>>>> {
        self.0.get().map_or(Ok(None), |registry| {
            registry.upgrade().map(Some).ok_or_else(|| {
                anyhow::anyhow!(
                    "sessions_spawn's tool registry is gone: the configuration it was built for was replaced; \
                     nothing was spawned, retry the turn"
                )
            })
        })
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
    #[cfg(test)]
    test_worker_executable: Option<TestWorkerExecutable>,
    /// Registry of every configured channel, keyed by [`Channel::name`].
    ///
    /// announce/kill resolve the *originating* channel object from here using the
    /// `channel_name` captured per-turn on the [`SubAgentRun`] — the channel and
    /// recipient then both come from the same launching message's scope, atomic
    /// and immune to the shared-`channel` overwrite race that concurrent message
    /// processing (the channels JoinSet) would otherwise cause. Empty in unit
    /// tests / single-channel paths, where the shared `channel` fallback applies.
    channels: Arc<HashMap<String, Arc<dyn Channel>>>,
    /// Test-only parent provider injected at the admitted-source seam. Child
    /// providers always come from the production admitted binding.
    #[cfg(test)]
    test_parent_provider: Arc<dyn Provider>,
    /// Provider name (for logging/display).
    provider_name: String,
    /// Model to use for sub-agent calls.
    model: String,
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
    tools: Arc<ParentToolRegistry>,
    /// Workspace dir for HookManager inside sub-agent loops.
    workspace_dir: PathBuf,
    /// Configured named agents for identity/model/tool scoping in spawn.
    agents: Arc<HashMap<String, DelegateAgentConfig>>,
    /// Credential issued for `provider_name` (the gateway provider): released
    /// to a child only when the child runs on that same provider identity.
    fallback_api_key: Option<String>,
    /// The providers the operator configured: the only ones a per-call
    /// `provider` argument may name.
    configured_providers: providers::ConfiguredProviders,
    /// What the configured models can do: operator declarations and cached
    /// catalog metadata. Empty means the adapter bounds only.
    model_catalog: Arc<providers::catalog::CatalogView>,
    /// Pinned configuration for a selected hint's owner-bound credential.
    hint_config: Option<Arc<crate::config::Config>>,
    default_route_source: Option<(String, providers::credentials::CredentialRef)>,
    fallback_api_key_owner: String,
    fallback_uses_root_connection: bool,
    /// Pricing table used only for recording/displaying child-agent token cost.
    cost_config: crate::config::schema::CostConfig,
    /// Shared memory backend for normalized spawn lifecycle events.
    memory: Option<Arc<dyn Memory>>,
    source_fabric: Option<MemoryFabric>,
    restart_recovery: Option<AbortOnDrop>,
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
    approval_resolver_factory: Option<crate::agent::turn_request::SpawnApprovalResolverFactory>,
    /// Membership of each fan-out, as `spawn_batch` recorded it the moment it
    /// launched.
    ///
    /// `join` used to rebuild this by scanning `active_runs` for rows carrying
    /// the batch label. That reads the *survivors*, not the members: the chat
    /// session reaper retires finished rows out of `active_runs`
    /// (`crate::chat::sessions::runtime`), so a member that completed and was
    /// retired before the join simply stopped existing — `total` shrank, its
    /// output was never reported, and nothing in the summary said a member had
    /// gone missing, because the only check on that identity is a
    /// `debug_assert` that release builds do not run.
    ///
    /// The roster is the answer to "who was launched", which is a fact fixed at
    /// launch and cannot be revised by a reaper. `active_runs` remains the
    /// answer to "what happened to them"; a member the roster names and the
    /// registry no longer holds is reported as a member with no result rather
    /// than subtracted from the batch.
    batch_rosters: Arc<RwLock<Vec<BatchRoster>>>,
}

#[cfg(test)]
mod tests;
