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

use super::chat_assignment::{self, ReturnTrip};
use super::sessions_list::format_run_usage;
use super::sessions_read_model;
use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_contracts::ScopeContext;
use crate::agent::turn_request::SpawnEventSink;
use crate::channels::build_identity_prompt;
use crate::channels::traits::{Channel, SendMessage};
use crate::config::{AgentCompactionConfig, DelegateAgentConfig, MultimodalConfig, SessionsSpawnConfig};
use crate::hooks::HookManager;
use crate::memory::{Memory, MemoryEventRecording, MemoryFabric, MessageEventScope};
use crate::observability::NoopObserver;
use crate::providers::{self, ChatMessage, Provider};
use crate::router::CompactionResolver;
use crate::runtime::chat_sessions::{AssignPrincipal, Disposition};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel};
use crate::security::{SecurityPolicy, SideEffectGate};
use crate::session_worker::protocol::{WorkerControlFrame, WorkerManifest, WorkerResult, config_source_generation};
use anyhow::Context as _;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};
use tokio::sync::{RwLock, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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

/// Everything the registry row for a process-mode run is built from.
struct ProcessModeRunSeed<'a> {
    id: String,
    task: String,
    lineage: &'a SpawnLineage,
    recipient: Option<String>,
    channel_name: Option<String>,
    process_control: Arc<ProcessRunControl>,
    history: Arc<RwLock<Vec<HistoryEntry>>>,
    steer_tx: tokio::sync::mpsc::Sender<String>,
    parent_run_id: Option<String>,
    session_scope_key: String,
    spawn_depth: usize,
    progress: Arc<crate::agent::idle::ProgressBeat>,
    batch_id: Option<String>,
}

/// Build the registry row for a process-mode run.
///
/// Split out from the spawn path so the steering wiring is testable: the row a
/// process-mode spawn publishes must always carry a live `steer_tx`, otherwise
/// `sessions_send` rejects the run as a "legacy run without steer support".
fn new_process_mode_run(seed: ProcessModeRunSeed<'_>) -> SubAgentRun {
    SubAgentRun {
        id: seed.id,
        task: seed.task,
        owner_id: seed.lineage.owner_id.clone(),
        topic_id: seed.lineage.topic_id.clone(),
        source_message_event_id: seed.lineage.source_message_event_id.clone(),
        started_at: Utc::now(),
        finished_at: None,
        status: SubAgentStatus::Running,
        recipient: seed.recipient,
        channel_name: seed.channel_name,
        abort_handle: None,
        process_control: Some(seed.process_control),
        task_control: None,
        history: seed.history,
        steer_tx: Some(seed.steer_tx),
        parent_run_id: seed.parent_run_id,
        session_scope_key: seed.session_scope_key,
        spawn_depth: seed.spawn_depth,
        token_usage_records: Vec::new(),
        progress: seed.progress,
        batch_id: seed.batch_id,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessFinalization {
    Natural,
    Terminated,
    TerminationFailed,
    Unconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessTerminationRequestResult {
    Finalized(ProcessFinalization),
    Pending,
}

/// Per-run process lifecycle control. The first termination reason wins and
/// every requester waits for the sole process owner to publish finalization.
#[derive(Debug)]
pub(crate) struct ProcessRunControl {
    /// A recovery projection has no live process owner and cannot signal it.
    recovery_only: bool,
    termination_tx: watch::Sender<Option<String>>,
    finalized_tx: watch::Sender<Option<ProcessFinalization>>,
    request_timeout: std::time::Duration,
    #[cfg(test)]
    timeout_barrier: Option<Arc<tokio::sync::Barrier>>,
}

impl ProcessRunControl {
    fn new() -> Arc<Self> {
        Self::new_with_request_timeout(std::time::Duration::from_secs(PROCESS_TERMINATION_REQUEST_TIMEOUT_SECS))
    }

    fn new_with_request_timeout(request_timeout: std::time::Duration) -> Arc<Self> {
        let (termination_tx, _) = watch::channel(None);
        let (finalized_tx, _) = watch::channel(None);
        Arc::new(Self {
            recovery_only: false,
            termination_tx,
            finalized_tx,
            request_timeout,
            #[cfg(test)]
            timeout_barrier: None,
        })
    }

    fn recovered_unconfirmed() -> Arc<Self> {
        let (termination_tx, _) = watch::channel(None);
        let (finalized_tx, _) = watch::channel(Some(ProcessFinalization::Unconfirmed));
        Arc::new(Self {
            recovery_only: true,
            termination_tx,
            finalized_tx,
            request_timeout: std::time::Duration::from_secs(PROCESS_TERMINATION_REQUEST_TIMEOUT_SECS),
            #[cfg(test)]
            timeout_barrier: None,
        })
    }

    async fn termination_requested(&self) -> String {
        let mut receiver = self.termination_tx.subscribe();
        loop {
            let reason = receiver.borrow_and_update().clone();
            if let Some(reason) = reason {
                return reason;
            }
            if receiver.changed().await.is_err() {
                return "process owner stopped".to_string();
            }
        }
    }

    pub(crate) async fn request_termination(&self, reason: &str) -> ProcessTerminationRequestResult {
        self.request_termination_with_timeout(reason, self.request_timeout)
            .await
    }

    async fn request_termination_with_timeout(
        &self,
        reason: &str,
        wait_timeout: std::time::Duration,
    ) -> ProcessTerminationRequestResult {
        let reason = reason.to_string();
        self.termination_tx.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(reason);
                true
            }
        });

        let wait_for_owner = async {
            let mut receiver = self.finalized_tx.subscribe();
            loop {
                let finalization = *receiver.borrow_and_update();
                if let Some(finalization) = finalization {
                    return ProcessTerminationRequestResult::Finalized(finalization);
                }
                if receiver.changed().await.is_err() {
                    return ProcessTerminationRequestResult::Pending;
                }
            }
        };
        // The test-only timeout barrier must be awaited before the final state
        // read, so this branch cannot use Clippy's synchronous map_or_else form.
        #[allow(clippy::option_if_let_else)]
        match tokio::time::timeout(wait_timeout, wait_for_owner).await {
            Ok(result) => result,
            Err(_) => {
                #[cfg(test)]
                if let Some(barrier) = &self.timeout_barrier {
                    barrier.wait().await;
                    barrier.wait().await;
                }
                self.finalization().map_or_else(
                    || ProcessTerminationRequestResult::Pending,
                    ProcessTerminationRequestResult::Finalized,
                )
            }
        }
    }

    fn finalize(&self, finalization: ProcessFinalization) {
        self.finalized_tx.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(finalization);
                true
            }
        });
    }

    pub(crate) fn finalization(&self) -> Option<ProcessFinalization> {
        *self.finalized_tx.borrow()
    }

    /// Resolve once the sole owner has published a finalization — or once it is
    /// gone without publishing one, which is the same thing for a waiter that
    /// only needs to know the run is over.
    async fn finalized(&self) {
        let mut receiver = self.finalized_tx.subscribe();
        while receiver.borrow_and_update().is_none() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    fn termination_reason(&self) -> Option<String> {
        self.termination_tx.borrow().clone()
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Arc<Self> {
        Self::new()
    }

    #[cfg(test)]
    pub(crate) fn new_pending_for_test() -> Arc<Self> {
        Self::new_with_request_timeout(std::time::Duration::from_millis(20))
    }

    #[cfg(test)]
    fn new_timeout_boundary_for_test(barrier: Arc<tokio::sync::Barrier>) -> Arc<Self> {
        let (termination_tx, _) = watch::channel(None);
        let (finalized_tx, _) = watch::channel(None);
        Arc::new(Self {
            recovery_only: false,
            termination_tx,
            finalized_tx,
            request_timeout: std::time::Duration::from_millis(20),
            timeout_barrier: Some(barrier),
        })
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_termination_for_test(&self) -> String {
        self.termination_requested().await
    }

    #[cfg(test)]
    pub(crate) fn finalize_for_test(&self, finalization: ProcessFinalization) {
        self.finalize(finalization);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SpawnExecutionContext {
    pub(crate) operator_authority: Option<Arc<crate::runtime::operator_spawn::task::OperatorTaskAuthority>>,
    pub(crate) ordinary_authority: Option<Arc<crate::runtime::child_source::ordinary_task::OrdinaryTaskAuthority>>,
    /// Incoming authority for this child, never an outgoing grant for its tools.
    pub(crate) incoming_grant: Option<Arc<crate::runtime::child_source::IssuedChildGrant>>,
    pub(crate) provider_binding: Option<Arc<crate::runtime::child_source::ChildProviderBinding>>,
    pub(crate) terminal_usage: Option<Arc<crate::runtime::turn_terminal::TerminalUsageCollector>>,
    pub(crate) input_receipt: Option<Arc<crate::runtime::child_source::ChildInputReceipt>>,
    pub(crate) run_id: String,
    pub(crate) session_scope_key: String,
    pub(crate) spawn_depth: usize,
    pub(crate) owner_id: Option<String>,
    pub(crate) owner_isolated: bool,
    pub(crate) topic_id: Option<String>,
    pub(crate) source_message_event_id: Option<String>,
    /// D8-4: distinguishes a *turn root* context (seeded at a top-level
    /// channel/chat/agent turn so its directly-spawned children inherit
    /// `parent_run_id`) from a *spawn run* context (a sub-agent run that may
    /// itself spawn). The turn root represents "the turn itself, before any
    /// spawn nesting": its first child reports `spawn_depth` 0 — exactly as if
    /// no context were seeded — so seeding a turn does not inflate the reported
    /// nesting of its children. A spawn run's child reports `+1`.
    pub(crate) is_turn_root: bool,
    /// Tool names the turn's own surface hides, carried down the spawn tree.
    ///
    /// An IM channel turn runs under `tool_tiering.for_channel_surface()`, which
    /// folds `channel_exclude` into `always_exclude` so a conversation partner
    /// cannot reach the operations surface. A sub-agent used to be started with
    /// no tiering at all, so `spawn` was a one-sentence way around that boundary
    /// from the same conversation. Names the operator listed in
    /// `always_include` are never in here: `for_channel_surface` keeps them
    /// exposed, and this list is derived from its output.
    pub(crate) channel_excluded_tools: Vec<String>,
}

impl SpawnExecutionContext {
    /// Seed a *turn root* context for a top-level channel/chat/agent turn. The
    /// per-turn `run_id` becomes the `parent_run_id` of any task this turn spawns
    /// directly, while `spawn_depth` starts at 0 and — because `is_turn_root` is
    /// true — the first child still reports depth 0 (see the `spawn_depth`
    /// computation in `execute`).
    pub(crate) const fn seed_turn_context(run_id: String, session_scope_key: String) -> Self {
        Self {
            operator_authority: None,
            ordinary_authority: None,
            incoming_grant: None,
            provider_binding: None,
            terminal_usage: None,
            input_receipt: None,
            run_id,
            session_scope_key,
            spawn_depth: 0,
            owner_id: None,
            owner_isolated: false,
            topic_id: None,
            source_message_event_id: None,
            is_turn_root: true,
            channel_excluded_tools: Vec::new(),
        }
    }

    /// Carry the turn surface's hidden tool names into everything it spawns.
    #[must_use]
    pub(crate) fn with_channel_excluded_tools(mut self, channel_excluded_tools: Vec<String>) -> Self {
        self.channel_excluded_tools = channel_excluded_tools;
        self
    }
}

/// Tool names the ambient turn/run context hides from this loop.
///
/// Empty for the operator entry points (terminal chat, gateway, console), which
/// seed no exclusions, and for any code running outside a spawn context.
pub(crate) fn ambient_channel_excluded_tools() -> Vec<String> {
    SPAWN_EXECUTION_CONTEXT
        .try_with(|ctx| ctx.channel_excluded_tools.clone())
        .unwrap_or_default()
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
    owner_isolated: bool,
    topic_id: Option<String>,
    parent_task_id: Option<String>,
    source_message_event_id: Option<String>,
    config_generation_id: Option<u64>,
    config_source_revision: Option<String>,
}

fn parse_spawn_scope(args: &serde_json::Value) -> Option<SpawnScope> {
    if let Some(context) = crate::tools::execution::current_child_call_context() {
        let context = context.context();
        return Some(SpawnScope {
            sender: context.sender().to_owned(),
            channel: context.channel().to_owned(),
            chat_type: context.chat_type.clone(),
            chat_id: context.chat_id.clone(),
            owner_id: sessions_read_model::effective_run_owner_from_context(context),
            owner_isolated: context.envelope.owner_isolated,
            topic_id: context.envelope.topic_id.clone(),
            parent_task_id: context.envelope.task_id.clone(),
            source_message_event_id: context.envelope.source_message_event_id.clone(),
            config_generation_id: context.envelope.config_generation_id,
            config_source_revision: context.envelope.config_source_revision.clone(),
        });
    }
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
        // OWNER-VIS: the run is tagged with the owner the reader side confines
        // to (`RunOwnerScope::from_args`), computed by the one shared function,
        // so a caller whose scope carries no explicit owner (a gateway webhook
        // turn) still finds the runs it started.
        owner_id: sessions_read_model::effective_run_owner(args),
        // Legacy trusted arguments do not establish an authenticated owner
        // isolation boundary. Real calls take this bit from their admitted
        // execution envelope above.
        owner_isolated: false,
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
        config_generation_id: scope.get("config_generation_id").and_then(serde_json::Value::as_u64),
        config_source_revision: scope
            .get("config_source_revision")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

/// Trusted provenance of the turn that launched a run, kept for the moment that
/// run finally announces.
///
/// A sub-agent announcement is a real outbound message whose `recipient` comes
/// straight from the model's tool call, but it leaves the process long after the
/// launching turn ended — there is no turn context left to authorize it against.
/// The only trustworthy identity is the one captured atomically from the
/// launching message's `_zc_scope`, in the very same snapshot `recipient` and
/// `channel_name` come from. Nothing may stand in for it: a synthesised
/// "system" sender would make the outbound ACL *look* enforced while letting
/// every announcement through, which is worse than not gating the path at all.
#[derive(Debug, Clone)]
pub(crate) struct AnnounceOrigin {
    sender: String,
    chat_type: String,
}

impl AnnounceOrigin {
    /// Build an origin from an identity another module already established as
    /// trusted.
    ///
    /// The one caller is [`crate::tools::chat_assignment`], which relays a chat
    /// session's result to the correspondent who asked for it and takes that
    /// correspondent from the origin the mailbox *recorded*, not from anything
    /// the model or the result payload said. Same rule as `from_scope`: an
    /// identity that cannot be established is `None` and degrades to `"unknown"`,
    /// never to a privileged one.
    pub(crate) fn from_parts(sender: &str, chat_type: &str) -> Self {
        Self {
            sender: sender.to_string(),
            chat_type: chat_type.to_string(),
        }
    }

    /// Capture a turn's identity from its trusted scope.
    ///
    /// Two callers with the same shape and different timing: a spawn snapshots
    /// the *launching* turn here because its announcement fires after that turn
    /// is gone, while `action='kill'` reads the *killing* turn's scope off its
    /// own arguments, since that turn is still running when its receipt goes out.
    ///
    /// `None` — an absent or untrusted `_zc_scope`, i.e. the CLI and
    /// single-channel paths — degrades to the same `"unknown"` identity
    /// `message_send` falls back to in exactly that situation (see
    /// `trusted_outbound_identity` there), never to a privileged one.
    fn from_scope(scope: Option<&SpawnScope>) -> Option<Self> {
        scope.map(|scope| Self {
            sender: scope.sender.clone(),
            chat_type: scope.chat_type.clone(),
        })
    }
}

/// Outbound recipient authorization for one message this tool emits by itself —
/// a run's result announcement or the receipt for a kill.
///
/// The counterpart of `MessageSendTool::authorize_outbound`: both funnel into
/// [`SecurityPolicy::is_outbound_allowed`], so a `send_allow` / `send_deny` rule
/// an operator writes governs the announcement path as well as the tool path.
/// Without this the model-supplied `recipient` on a `sessions_spawn` call was a
/// complete bypass of that ACL — spawn a throwaway task, name any recipient, and
/// both its completion notice *and* the receipt for killing it are delivered
/// with zero authorization.
///
/// Source and destination channel are deliberately the same value, and that is
/// load-bearing rather than lazy. Neither notice has a channel argument: both
/// always leave on the channel the run resolved for itself, so that channel *is*
/// both the anchor and the destination — there is no cross-channel capability on
/// this path to defend. Passing the captured scope channel instead
/// would diverge exactly when [`SessionsSpawnTool::resolve_announce_channel`]
/// falls back to the shared active channel (a name absent from the registry —
/// every single-channel deployment), source and destination would then differ,
/// the channel default would flip to deny, and every announcement in a
/// deployment with no rules configured at all would vanish.
///
/// MUTATION GUARD: pass the origin's own channel as `src_channel` here and
/// `announce_is_not_denied_by_a_channel_registry_fallback` goes red. That test is
/// the zero-regression tripwire for this decision, not a style preference.
pub(crate) fn announce_is_authorized(
    security: &SecurityPolicy,
    origin: Option<&AnnounceOrigin>,
    dst_channel: &str,
    recipient: &str,
) -> bool {
    let (sender, chat_type) = origin.map_or(("unknown", "unknown"), |origin| {
        (origin.sender.as_str(), origin.chat_type.as_str())
    });
    security.is_outbound_allowed(sender, dst_channel, chat_type, dst_channel, recipient)
}

/// Which of this tool's self-initiated messages is being delivered.
///
/// Three call sites — the task-mode result, the process-mode result and the
/// receipt for `action='kill'` — share one delivery path, so the only thing that
/// legitimately differs between them (operator-facing wording) lives here. The
/// authorization itself deliberately has no per-site variant: that is what makes
/// it impossible to fix the gate on one notice and leave another ungated, which
/// is precisely how the kill receipt stayed a bypass after the announcement was
/// closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboundNotice {
    /// A finished task-mode run reporting its own result.
    RunResult,
    /// A finished process-mode run reporting its own result.
    ProcessRunResult,
    /// The receipt for an operator's kill of a run.
    KillNotice,
}

impl OutboundNotice {
    /// How the withholding record names this notice to the operator.
    const fn withheld_label(self) -> &'static str {
        match self {
            Self::RunResult | Self::ProcessRunResult => "Announcement",
            Self::KillNotice => "Kill notice",
        }
    }

    /// Log context when the channel itself rejects the send (transport failure,
    /// not authorization).
    const fn send_failure_context(self) -> &'static str {
        match self {
            Self::RunResult => "Failed to announce sub-agent result",
            Self::ProcessRunResult => "Failed to announce sub-agent process result",
            Self::KillNotice => "Failed to announce sub-agent kill",
        }
    }
}

/// Record a notice the outbound ACL refused.
///
/// A refusal must never be a silent drop. It lands in the log *and* on the run's
/// own history, where `sessions_spawn action='history'` and `session_status`
/// surface it to the operator wondering why no result ever arrived. Both carry
/// only the destination's stable audit fingerprint, never the plaintext
/// recipient — the same rule `message_send`'s rejection text follows.
async fn record_withheld_notice(
    history: &Arc<RwLock<Vec<HistoryEntry>>>,
    run_id: &str,
    dst_channel: &str,
    recipient: &str,
    notice: OutboundNotice,
) {
    let recipient_ref = crate::security::op_id::ref_for_channel_recipient(dst_channel, recipient);
    let label = notice.withheld_label();
    tracing::warn!(
        run_id = %run_id,
        channel = %dst_channel,
        recipient = %recipient_ref,
        notice = %label,
        "Sub-agent outbound notice withheld: the configured scope rules do not permit this recipient"
    );
    history.write().await.push(HistoryEntry {
        role: "system".to_string(),
        content: crate::security::redaction::redact_secrets(&format!(
            "{label} withheld: outbound messaging to recipient {recipient_ref} on channel \
             '{dst_channel}' is not permitted by the configured scope rules"
        )),
        timestamp: Utc::now(),
    });
}

/// The one path by which this tool puts a message on a channel by its own
/// initiative — result announcement or kill receipt alike.
///
/// Folding the three call sites into a single function is the substance of this
/// gate, not tidiness. While each site inlined its own copy, "the process-mode
/// block is a verbatim mirror of the task-mode one" was an argument, not
/// evidence, and the kill receipt — carrying the *same* model-chosen
/// `recipient` — simply never grew a copy at all. With one implementation there
/// is one thing to test and nothing left to forget.
///
/// MUTATION GUARD: delete the [`announce_is_authorized`] branch here and four
/// tests go red at once —
/// `announce_to_a_denied_recipient_is_withheld_and_recorded_on_the_run`,
/// `a_model_supplied_recipient_cannot_bypass_send_deny`,
/// `a_kill_notice_to_a_denied_recipient_is_withheld_and_recorded_on_the_run` and
/// `a_model_supplied_recipient_cannot_bypass_send_deny_through_the_kill_notice`.
async fn deliver_or_withhold_notice(
    security: &SecurityPolicy,
    origin: Option<&AnnounceOrigin>,
    channel: &dyn Channel,
    history: &Arc<RwLock<Vec<HistoryEntry>>>,
    run_id: &str,
    recipient: &str,
    text: &str,
    notice: OutboundNotice,
) {
    if !announce_is_authorized(security, origin, channel.name(), recipient) {
        record_withheld_notice(history, run_id, channel.name(), recipient, notice).await;
        return;
    }
    let message = SendMessage::new(text, recipient);
    if let Err(error) = channel.send(&message).await {
        tracing::error!(run_id = %run_id, "{}: {error}", notice.send_failure_context());
    }
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
    owner_isolated: bool,
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
        owner_isolated: scope.is_some_and(|scope| scope.owner_isolated)
            || parent_ctx.is_some_and(|ctx| ctx.owner_isolated),
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

#[cfg(test)]
pub(crate) struct TestWorkerRecoveryResult {
    pub(crate) launch_event_id: i64,
    pub(crate) result: anyhow::Result<Option<bool>>,
}

/// Read-only fixture observation of the actual built worker manifest. No key,
/// transport seal or grant bytes leave the launch producer through this seam.
#[cfg(test)]
pub(crate) struct TestWorkerRouteObservation {
    pub(crate) run_id: String,
    pub(crate) parent_run_id: Option<String>,
    pub(crate) owner_id: Option<String>,
    pub(crate) source_event_id: Option<String>,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) route_hint: Option<String>,
    pub(crate) route_source_provider: Option<String>,
    pub(crate) route_credential: Option<providers::credentials::CredentialRef>,
    pub(crate) key_sha256: Option<[u8; 32]>,
    pub(crate) route_uses_root_connection: bool,
    pub(crate) direct_uses_root_connection: bool,
    pub(crate) child_source_present: bool,
    pub(crate) child_allowed_count: Option<usize>,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestWorkerRouteCapture {
    pub(crate) total: usize,
    pub(crate) first: Option<TestWorkerRouteObservation>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TestWorkerProtocolCase {
    HoldAfterThreeSteers,
    CloseAfterManifest,
}

#[cfg(test)]
pub(crate) struct TestWorkerProtocolControl {
    case: TestWorkerProtocolCase,
    claimed: std::sync::atomic::AtomicBool,
    termination_calls: Arc<std::sync::atomic::AtomicUsize>,
    terminal: parking_lot::Mutex<Option<TestWorkerProtocolTerminal>>,
    diagnostic: Arc<parking_lot::Mutex<TestWorkerProtocolDiagnostic>>,
}

#[cfg(test)]
#[derive(Default, serde::Serialize, Debug)]
pub(crate) struct TestWorkerProtocolDiagnostic {
    manifest_bytes: Option<usize>,
    component_bytes: std::collections::BTreeMap<String, usize>,
    precondition_passed: Option<bool>,
    spawned: bool,
    pid: Option<u32>,
    actual_pgid: Option<i32>,
    start_identity: Option<(u64, u64)>,
    identity_class: Option<&'static str>,
    registry_id: Option<u64>,
    launch_persisted: bool,
    reap_confirmed: bool,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    registration_marked_reaped: bool,
    lifecycle_class: Option<&'static str>,
    panic_cleanup_class: Option<&'static str>,
    finalization: Option<&'static str>,
    fixture_cleanup_class: Option<&'static str>,
    process_cleanup_class: Option<&'static str>,
    owner_cleanup_class: Option<&'static str>,
}

#[cfg(test)]
fn test_worker_cleanup_error_class(error: &anyhow::Error) -> &'static str {
    // Inspect locally; retain only a closed class, never rendered error text.
    let text = error.to_string();
    if text.contains("failed to kill session-worker process group") {
        "group_signal_failed"
    } else if text.contains("direct-child") || text.contains("direct child") {
        "direct_child_cleanup_failed"
    } else if text.contains("wait") || text.contains("reap") {
        "wait_or_reap_failed"
    } else {
        "other_cleanup_error"
    }
}

#[cfg(all(test, unix))]
#[allow(unsafe_code)]
fn test_worker_process_identity(pid: u32) -> (Option<i32>, Option<(u64, u64)>, &'static str) {
    let Ok(pid) = i32::try_from(pid) else {
        return (None, None, "invalid_pid");
    };
    // SAFETY: getpgid observes one actual positive child PID without signalling.
    let pgid = unsafe { libc::getpgid(pid) };
    let pgid = (pgid > 0).then_some(pgid);
    #[cfg(target_os = "macos")]
    {
        // SAFETY: plain C output buffer has the exact PROC_PIDTBSDINFO size.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: pid is the positive spawned child; info is writable for exactly
        // size bytes of PROC_PIDTBSDINFO. libproc validates pid and writes the buffer.
        let received = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        if received == size {
            return (
                pgid,
                Some((info.pbi_start_tvsec, info.pbi_start_tvusec)),
                "macos_start_time",
            );
        }
        (pgid, None, "start_unavailable")
    }
    #[cfg(target_os = "linux")]
    {
        let start = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|text| {
                let tail = text.get(text.rfind(')')? + 1..)?;
                tail.split_whitespace().nth(19)?.parse::<u64>().ok()
            });
        (
            pgid,
            start.map(|ticks| (ticks, 0)),
            if start.is_some() {
                "linux_start_ticks"
            } else {
                "start_unavailable"
            },
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        (pgid, None, "start_unsupported")
    }
}

#[cfg(test)]
pub(crate) struct TestWorkerProtocolTerminal {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stdin_held_through_reap: bool,
}

#[cfg(test)]
impl TestWorkerProtocolControl {
    pub(crate) fn new(case: TestWorkerProtocolCase) -> Self {
        Self {
            case,
            claimed: std::sync::atomic::AtomicBool::new(false),
            termination_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            terminal: parking_lot::Mutex::new(None),
            diagnostic: Arc::new(parking_lot::Mutex::new(TestWorkerProtocolDiagnostic::default())),
        }
    }

    fn claim(&self) -> anyhow::Result<()> {
        self.claimed
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .map_err(|_| anyhow::anyhow!("worker protocol control already claimed"))?;
        Ok(())
    }

    fn check_serialized_precondition(&self, payload: &str) -> anyhow::Result<()> {
        let value: serde_json::Value = serde_json::from_str(payload)?;
        let fields = value.as_object().context("serialized worker manifest object missing")?;
        anyhow::ensure!(fields.len() <= 128, "manifest diagnostic field bound");
        let mut lengths = std::collections::BTreeMap::new();
        for (name, value) in fields {
            anyhow::ensure!(name.len() <= 128, "manifest diagnostic name bound");
            lengths.insert(name.clone(), serde_json::to_vec(value)?.len());
        }
        if let Some(source) = fields.get("operator_source").and_then(serde_json::Value::as_object) {
            anyhow::ensure!(source.len() <= 32, "operator source diagnostic field bound");
            for (name, value) in source {
                anyhow::ensure!(name.len() <= 128, "operator diagnostic name bound");
                lengths.insert(format!("operator_source.{name}"), serde_json::to_vec(value)?.len());
            }
        }
        let mut diagnostic = self.diagnostic.lock();
        diagnostic.manifest_bytes = Some(payload.len());
        diagnostic.component_bytes = lengths;
        diagnostic.precondition_passed = Some(payload.len() < 3072);
        anyhow::ensure!(
            payload.len() < 3072,
            "worker stdin fixture precondition failed before spawn: manifest_bytes={}",
            payload.len()
        );
        Ok(())
    }

    pub(crate) fn serialized_precondition_failed(&self) -> bool {
        self.diagnostic.lock().precondition_passed == Some(false)
    }

    pub(crate) fn diagnostic_json(&self) -> anyhow::Result<Vec<u8>> {
        let mut value = serde_json::to_value(&*self.diagnostic.lock())?;
        let spawned = matches!(value.get("spawned").and_then(serde_json::Value::as_bool), Some(true));
        let object = value.as_object_mut().context("diagnostic object missing")?;
        let mut child = serde_json::Map::new();
        for name in [
            "pid",
            "actual_pgid",
            "start_identity",
            "identity_class",
            "registry_id",
            "launch_persisted",
            "reap_confirmed",
            "exit_code",
            "exit_signal",
            "registration_marked_reaped",
            "lifecycle_class",
            "panic_cleanup_class",
            "finalization",
        ] {
            child.insert(
                name.to_owned(),
                object.remove(name).context("diagnostic field missing")?,
            );
        }
        child.insert(
            "signal_attempts".to_owned(),
            serde_json::json!(self.termination_calls()),
        );
        object.insert(
            "child".to_owned(),
            if spawned {
                serde_json::Value::Object(child)
            } else {
                serde_json::Value::Null
            },
        );
        serde_json::to_vec_pretty(&value).map_err(Into::into)
    }

    fn observe_reap_status(&self, status: std::process::ExitStatus) {
        let mut diagnostic = self.diagnostic.lock();
        diagnostic.reap_confirmed = true;
        diagnostic.exit_code = status.code();
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            diagnostic.exit_signal = status.signal();
        }
    }

    pub(crate) fn observe_cleanup_parts(&self, process: &'static str, owner: &'static str) {
        let mut diagnostic = self.diagnostic.lock();
        diagnostic.process_cleanup_class = Some(process);
        diagnostic.owner_cleanup_class = Some(owner);
    }

    pub(crate) fn observe_fixture_cleanup(&self, finalization: Option<ProcessFinalization>, class: &'static str) {
        let mut diagnostic = self.diagnostic.lock();
        diagnostic.finalization = finalization.map(|value| match value {
            ProcessFinalization::Natural => "natural",
            ProcessFinalization::Terminated => "terminated",
            ProcessFinalization::TerminationFailed => "termination_failed",
            ProcessFinalization::Unconfirmed => "unconfirmed",
        });
        diagnostic.fixture_cleanup_class = Some(class);
    }

    pub(crate) fn termination_calls(&self) -> usize {
        self.termination_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn take_terminal(&self) -> anyhow::Result<TestWorkerProtocolTerminal> {
        self.terminal
            .lock()
            .take()
            .context("actual worker terminal observation missing")
    }
}

#[cfg(test)]
#[derive(Clone)]
struct TestWorkerExecutable {
    path: PathBuf,
    sha256: String,
    private_home: PathBuf,
    route_observer: Option<Arc<parking_lot::Mutex<TestWorkerRouteCapture>>>,
    protocol: Option<Arc<TestWorkerProtocolControl>>,
}

#[cfg(test)]
impl TestWorkerExecutable {
    fn verify(&self) -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};
        use std::io::Read as _;
        anyhow::ensure!(self.path.is_absolute(), "worker artifact path must be absolute");
        anyhow::ensure!(
            std::fs::symlink_metadata(&self.path)?.file_type().is_file(),
            "worker artifact must be a regular file, not a symlink"
        );
        let mut file = std::fs::File::open(&self.path)?;
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(buffer.get(..count).context("worker artifact read exceeded buffer")?);
        }
        anyhow::ensure!(
            hex::encode(hash.finalize()) == self.sha256,
            "worker artifact hash changed"
        );
        anyhow::ensure!(
            self.private_home.is_absolute() && std::fs::symlink_metadata(&self.private_home)?.file_type().is_dir(),
            "worker HOME must be an existing private directory, not a symlink"
        );
        Ok(())
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
    /// Provider for sub-agent LLM calls.
    provider: Arc<dyn Provider>,
    pinned_provider_resolver: Option<Arc<dyn providers::generation::PinnedProviderResolver>>,
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
    /// Model-aware compaction resolver for sub-agent tool call loops.
    compaction_resolver: CompactionResolver,
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
    /// Reliability settings applied when a sub-agent overrides the provider.
    ///
    /// Without this the override path built a bare provider with no retry,
    /// backoff or rate-limit handling at all — see [`Self::build_override_provider`].
    reliability: crate::config::ReliabilityConfig,
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

/// Select a credential for a child provider.
///
/// Each credential in scope goes only to the provider identity it was issued
/// for: a named agent's key to that agent's own provider, the gateway fallback
/// to the gateway provider. A child on any other provider gets none and
/// resolves its own auth profile, or runs keyless.
fn resolve_child_provider_api_key(
    selected_agent: Option<&DelegateAgentConfig>,
    fallback_api_key: Option<&str>,
    child_provider_name: &str,
    gateway_provider_name: &str,
) -> Option<String> {
    providers::credential_for_provider_identity(
        child_provider_name,
        &[
            (
                selected_agent.map(|agent| agent.provider.as_str()),
                selected_agent.and_then(|agent| agent.api_key.as_deref()),
            ),
            (Some(gateway_provider_name), fallback_api_key),
        ],
    )
    .map(str::to_string)
}

#[cfg(test)]
mod tests;

/// How a task-mode child gets its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildProvider {
    /// The spawning tool's own chain, built with the connection it was
    /// configured with (credential, base URL and rotation keys together).
    Gateway,
    /// The parent is a routed hint, so its chain is not the configured root
    /// connection even when a direct child names the root provider.
    Root,
    /// A fresh chain with this credential and no base URL.
    Build { api_key: Option<String> },
}

/// One member of a fan-out, as it was launched.
#[derive(Debug, Clone)]
struct BatchMember {
    run_id: String,
    /// What the member was asked to do, kept here so a member whose registry
    /// row is gone is still reported as something more useful than an id.
    task: String,
}

/// The membership of one fan-out.
#[derive(Debug, Clone)]
struct BatchRoster {
    batch_id: String,
    /// The owner the batch was launched for (OWNER-VIS): `join` answers a
    /// batch only to a caller whose run scope admits this owner.
    owner_id: Option<String>,
    members: Vec<BatchMember>,
}

/// How many fan-out rosters one tool instance remembers.
///
/// A roster is small and a batch is normally joined within the turn that
/// launched it, so this is a leak guard rather than a policy. Eviction is
/// oldest-first, and a join on an evicted batch falls back to the registry scan
/// this field replaced — the pre-existing behaviour, not a new failure.
const MAX_TRACKED_BATCH_ROSTERS: usize = 256;

impl SessionsSpawnTool {
    fn resolved_process_config_source(
        options: &providers::ProviderRuntimeOptions,
        pinned_config: Option<&crate::config::Config>,
    ) -> anyhow::Result<(PathBuf, String)> {
        let configured_dir = options
            .openprx_dir
            .as_ref()
            .context("sessions_spawn process mode requires the resolved parent config directory")?;
        let config_dir = std::fs::canonicalize(configured_dir)
            .with_context(|| format!("failed to resolve parent config directory {}", configured_dir.display()))?;
        if let Some(config) = pinned_config {
            let pinned_path = std::fs::canonicalize(&config.config_path)
                .context("failed to resolve pinned worker configuration source")?;
            let worker_path = std::fs::canonicalize(config_dir.join("config.toml"))
                .context("failed to resolve worker configuration source")?;
            anyhow::ensure!(
                pinned_path == worker_path,
                "worker configuration differs from pinned parent source"
            );
        }
        let generation = config_source_generation(&config_dir)?;
        Ok((config_dir, generation))
    }

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
        _temperature: f64,
        security: Arc<SecurityPolicy>,
        workspace_dir: PathBuf,
        _multimodal_config: MultimodalConfig,
        compaction_config: AgentCompactionConfig,
        agents: HashMap<String, DelegateAgentConfig>,
        fallback_api_key: Option<String>,
        _provider_runtime_options: providers::ProviderRuntimeOptions,
        _spawn_config: SessionsSpawnConfig,
        active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    ) -> Self {
        let compaction_resolver = CompactionResolver::from_base(compaction_config);
        tracing::debug!("sessions_spawn initialized child compaction resolver");
        let provider_name: String = provider_name.into();
        let configured_providers = providers::ConfiguredProviders::default()
            .with_providers(std::iter::once(provider_name.as_str()))
            .with_providers(agents.values().map(|agent| agent.provider.as_str()));
        Self {
            channel: Arc::new(RwLock::new(channel)),
            #[cfg(test)]
            test_worker_executable: None,
            channels: Arc::new(HashMap::new()),
            provider,
            pinned_provider_resolver: None,
            fallback_api_key_owner: provider_name.clone(),
            fallback_uses_root_connection: true,
            provider_name,
            model: model.into(),
            security,
            default_recipient: Arc::new(RwLock::new(None)),
            active_runs,
            tools: Arc::new(ParentToolRegistry::default()),
            workspace_dir,
            compaction_resolver,
            configured_providers,
            model_catalog: Arc::default(),
            hint_config: None,
            default_route_source: None,
            agents: Arc::new(agents),
            fallback_api_key,
            reliability: crate::config::ReliabilityConfig::default(),
            cost_config: crate::config::schema::CostConfig::default(),
            memory: None,
            source_fabric: None,
            event_recording: MemoryEventRecording::default(),
            event_sink: None,
            approval_resolver_factory: None,
            batch_rosters: Arc::new(RwLock::new(Vec::new())),
            restart_recovery: None,
        }
    }

    /// One actual bounded recovery page, retaining every per-launch error.
    /// This exposes no grant, signing key, or alternate verification path.
    #[cfg(test)]
    pub(crate) async fn recover_process_page_for_test(
        &self,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<TestWorkerRecoveryResult>> {
        let fabric = self
            .memory_fabric()
            .context("recovery fixture requires source fabric")?;
        let workspace = PathBuf::from(fabric.workspace_id());
        let page = process_terminal::recover_restart_page(&fabric, &workspace, after_id, limit).await?;
        let mut outcomes = Vec::with_capacity(page.outcomes.len());
        for item in page.outcomes {
            let result = match item.result {
                Ok(Some(recovered)) => recovered.restore(&self.active_runs).await.map(Some),
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            };
            outcomes.push(TestWorkerRecoveryResult {
                launch_event_id: item.launch_event_id,
                result,
            });
        }
        Ok(outcomes)
    }

    /// Isolated AUT only: the caller validates a root-frozen build/profile manifest.
    /// Production launch always resolves current_exe and has no override.
    #[cfg(test)]
    pub(crate) fn with_test_worker_executable(
        mut self,
        path: PathBuf,
        sha256: String,
        private_home: PathBuf,
    ) -> anyhow::Result<Self> {
        let executable = TestWorkerExecutable {
            path,
            sha256,
            private_home,
            route_observer: None,
            protocol: None,
        };
        executable.verify()?;
        self.test_worker_executable = Some(executable);
        Ok(self)
    }

    #[must_use]
    pub(crate) fn with_pinned_provider_resolver(
        mut self,
        resolver: Arc<dyn providers::generation::PinnedProviderResolver>,
    ) -> Self {
        self.pinned_provider_resolver = Some(resolver);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_test_worker_protocol(mut self, control: Arc<TestWorkerProtocolControl>) -> anyhow::Result<Self> {
        self.test_worker_executable
            .as_mut()
            .context("protocol control requires an attested worker artifact")?
            .protocol = Some(control);
        Ok(self)
    }

    /// Optional per-instance readback only; does not construct or mutate authority.
    #[cfg(test)]
    pub(crate) fn with_test_route_observer(
        mut self,
        observer: Arc<parking_lot::Mutex<TestWorkerRouteCapture>>,
    ) -> anyhow::Result<Self> {
        self.test_worker_executable
            .as_mut()
            .context("route observation requires an attested worker artifact")?
            .route_observer = Some(observer);
        Ok(self)
    }

    pub fn with_compaction_resolver(mut self, resolver: CompactionResolver) -> Self {
        self.compaction_resolver = resolver;
        self
    }

    #[must_use]
    pub fn with_cost_config(mut self, cost_config: crate::config::schema::CostConfig) -> Self {
        self.cost_config = cost_config;
        self
    }

    /// Widen the providers a per-call `provider` argument may name to every
    /// provider the operator configured (see
    /// [`providers::ConfiguredProviders::from_config`]). Without it only the
    /// gateway provider and the named agents' providers are selectable.
    #[must_use]
    pub fn with_configured_providers(mut self, configured: &providers::ConfiguredProviders) -> Self {
        self.configured_providers = self.configured_providers.union(configured);
        self
    }

    /// Supply the model evidence a spawn is resolved against
    /// (`providers::selection::catalog_for`).
    #[must_use]
    pub fn with_model_catalog(mut self, catalog: providers::catalog::CatalogView) -> Self {
        self.model_catalog = Arc::new(catalog);
        self
    }

    #[must_use]
    pub fn with_hint_config(mut self, config: &crate::config::Config) -> Self {
        self.hint_config = Some(Arc::new(config.clone()));
        self
    }

    /// Preserve a sealed route selection independently of agent execution
    /// identity when a worker spawns another child.
    pub fn with_default_route_source(
        mut self,
        provider: &str,
        credential: providers::credentials::CredentialRef,
    ) -> Self {
        self.default_route_source = Some((provider.to_string(), credential));
        self
    }

    pub fn with_fallback_api_key_owner(mut self, provider: &str) -> Self {
        self.fallback_api_key_owner = provider.to_string();
        self
    }

    pub const fn with_fallback_root_connection(mut self, uses_root: bool) -> Self {
        self.fallback_uses_root_connection = uses_root;
        self
    }

    /// Supply the deployment's reliability settings.
    ///
    /// Only consumed when a sub-agent overrides the provider and this tool has
    /// to build a fresh chain; without it the retry budget falls back to
    /// [`ReliabilityConfig::default`](crate::config::ReliabilityConfig::default)
    /// rather than to "no retries at all".
    #[must_use]
    pub fn with_reliability(mut self, reliability: crate::config::ReliabilityConfig) -> Self {
        self.reliability = reliability;
        self
    }

    /// Reliability settings for a sub-agent that pinned its own provider.
    ///
    /// Retry budget, backoff and per-model fallbacks carry over unchanged; the
    /// two chain-widening knobs are deliberately dropped:
    ///
    /// * `fallback_providers` — the caller named a provider on purpose (the
    ///   canonical use is "audit this with a second, different model"), so
    ///   silently serving the request from another vendor would defeat the
    ///   request while looking like success.
    /// * `api_keys` — those rotation credentials belong to the *gateway's*
    ///   provider. Replaying them against a different vendor only buys a run of
    ///   guaranteed 401s inside the retry budget.
    fn override_reliability(base: &crate::config::ReliabilityConfig) -> crate::config::ReliabilityConfig {
        crate::config::ReliabilityConfig {
            fallback_providers: Vec::new(),
            api_keys: Vec::new(),
            ..base.clone()
        }
    }

    /// Build the provider for a sub-agent that overrides the gateway provider.
    /// The exact model and owner-bound credential have already been released by
    /// the resolver; the typed factory adds the retry and fallback policy
    /// without reopening any credential source.
    fn build_override_provider(
        resolved: &providers::catalog::ResolvedModel,
        catalog: &providers::catalog::CatalogView,
        reliability: &crate::config::ReliabilityConfig,
        cost: &crate::config::schema::CostConfig,
        options: &providers::ProviderRuntimeOptions,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        let reliability = Self::override_reliability(reliability);
        let provider = providers::create_provider_for(resolved, catalog, &reliability, cost, options)?;
        Ok(Arc::from(provider))
    }

    /// How a task-mode child on `child_provider` gets its provider.
    ///
    /// Any spelling of the gateway's own provider identity reuses the gateway
    /// chain, which was built with the credential, base URL and rotation keys
    /// that belong together. A rebuilt chain never receives the gateway key:
    /// only a named agent's own key for the agent's own provider, or nothing.
    fn child_provider_plan(&self, selected_agent: Option<&DelegateAgentConfig>, child_provider: &str) -> ChildProvider {
        if let Some(agent) = selected_agent.filter(|agent| {
            providers::ProviderId::of(&agent.provider) == providers::ProviderId::of(child_provider)
                && (agent.credential_ref.is_some()
                    || agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()))
        }) {
            return ChildProvider::Build {
                api_key: agent.api_key.clone(),
            };
        }
        if providers::canonical_provider_key(child_provider) == providers::canonical_provider_key(&self.provider_name) {
            if self.model.trim().starts_with("hint:") {
                return ChildProvider::Root;
            }
            return ChildProvider::Gateway;
        }
        ChildProvider::Build {
            api_key: resolve_child_provider_api_key(selected_agent, None, child_provider, &self.provider_name),
        }
    }

    #[cfg(test)]
    fn resolve_child_compaction(
        &self,
        provider: &str,
        model: &str,
    ) -> crate::router::context::EffectiveCompactionConfig {
        self.compaction_resolver.resolve(provider, model)
    }

    /// Attach a `SpawnEventSink` so task-mode sub-agents spawned by this tool
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

    /// Attach a [`SpawnApprovalResolverFactory`](crate::agent::turn_request::SpawnApprovalResolverFactory)
    /// so a task-mode sub-agent that hits the supervised approval gate suspends
    /// (NeedsInput) awaiting an operator decision instead of auto-failing.
    ///
    /// Only the chat `/bg` path sets this; channels/gateway leave it `None` and
    /// keep the historical auto-fail-on-gate semantics (no human is present to
    /// approve, so suspending would only create a zombie).
    // Called only by `chat::run`, the sole NeedsInput opt-in.
    #[must_use]
    pub(crate) fn with_approval_resolver_factory(
        mut self,
        factory: crate::agent::turn_request::SpawnApprovalResolverFactory,
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
    pub fn tools_handle(&self) -> Arc<ParentToolRegistry> {
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
    /// Logical lifecycle storage is independent of execution-memory isolation.
    pub(crate) fn with_source_fabric(mut self, fabric: MemoryFabric) -> Self {
        self.source_fabric = Some(fabric);
        self
    }

    pub fn with_shared_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// The owning tool generation bounds recovery lifetime. Disabled decision
    /// mode starts no task and performs no storage or process probes.
    pub(crate) fn with_process_recovery(mut self, enabled: bool) -> anyhow::Result<Self> {
        if !enabled {
            return Ok(self);
        }
        anyhow::ensure!(self.restart_recovery.is_none(), "worker recovery is already attached");
        let fabric = self.memory_fabric().context("worker recovery requires shared memory")?;
        let workspace = std::path::PathBuf::from(fabric.workspace_id());
        let runs = Arc::clone(&self.active_runs);
        let handle = tokio::runtime::Handle::try_current()?;
        self.restart_recovery = Some(AbortOnDrop(handle.spawn(async move {
            process_terminal::run_restart_recovery(fabric, workspace, runs).await;
        })));
        Ok(self)
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
        envelope.config_generation_id = scope.config_generation_id;
        envelope.config_source_revision = scope.config_source_revision.clone();
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
    lineage: &SpawnLineage,
) -> anyhow::Result<Option<Arc<crate::runtime::child_source::ChildInputReceipt>>> {
    let Some(fabric) = fabric else {
        return Ok(None);
    };
    // Durable records keep the endpoint, never a URL's `user:password@`.
    let provider_name = crate::providers::redact_endpoint_userinfo(provider_name);
    let task_event_scope = scope.clone();
    let task_id = task_event_scope
        .run_id
        .clone()
        .unwrap_or_else(|| "sessions_spawn:unknown".to_string());
    let grant = crate::runtime::child_source::outgoing_grant();
    let receipt = crate::runtime::child_source::record_child_input(
        fabric,
        scope,
        task,
        None,
        Some(
            json!({
                "mode": mode,
                "provider": provider_name,
                "model": model,
                "owner_id": lineage.owner_id,
                "topic_id": lineage.topic_id,
                "parent_task_id": lineage.parent_task_id,
                "source_message_event_id": lineage.source_message_event_id
            })
            .to_string(),
        ),
        grant.as_deref(),
    )
    .await?;
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
    Ok(receipt)
}

async fn record_spawn_result_event(
    fabric: Option<&MemoryFabric>,
    scope: MessageEventScope,
    result_text: &str,
    status: &SubAgentStatus,
    lineage: &SpawnLineage,
    terminal_event_type: Option<&str>,
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
    // The result and task records carry the sub-agent's reply or error text:
    // redacted like the terminal commit (D-053).
    if let Err(error) = fabric
        .record_worker_result(
            scope,
            crate::agent::terminal::redact_persisted_text(result_text),
            Some(
                crate::agent::terminal::redact_persisted_json(json!({
                    "success": success,
                    "error": error,
                    "owner_id": lineage.owner_id,
                    "topic_id": lineage.topic_id,
                    "parent_task_id": lineage.parent_task_id,
                    "source_message_event_id": lineage.source_message_event_id
                }))
                .to_string(),
            ),
        )
        .await
    {
        tracing::warn!("failed to record sessions_spawn result event: {error}");
    }
    let task_event_type = terminal_event_type.unwrap_or(if success { "task.completed" } else { "task.failed" });
    if let Err(error) = fabric
        .record_task_event(
            task_event_scope,
            task_id,
            task_event_type,
            Some(
                crate::agent::terminal::redact_persisted_json(json!({
                    "success": success,
                    "error": error,
                    "result_preview": result_text.chars().take(500).collect::<String>(),
                    "owner_id": lineage.owner_id,
                    "topic_id": lineage.topic_id,
                    "parent_task_id": lineage.parent_task_id,
                    "source_message_event_id": lineage.source_message_event_id
                }))
                .to_string(),
            ),
        )
        .await
    {
        tracing::warn!("failed to record sessions_spawn {task_event_type} event: {error}");
    }
}

#[cfg(test)]
struct ProcessTerminalCommitHook {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

async fn commit_process_terminal_state(
    active_runs: &Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: &str,
    status: SubAgentStatus,
    token_usage_records: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    process_control: &ProcessRunControl,
    finalization: ProcessFinalization,
    repair: Option<&process_terminal::ConfirmedProcessRepair>,
    #[cfg(test)] commit_hook: Option<ProcessTerminalCommitHook>,
) -> anyhow::Result<()> {
    let mut runs = active_runs.write().await;
    let run = runs
        .iter_mut()
        .find(|run| run.id == run_id)
        .context("process terminal registry row missing")?;
    let prior_finalization = process_control.finalization();
    if let Some(repair) = repair {
        anyhow::ensure!(
            finalization == ProcessFinalization::Natural
                && repair.matches(run_id, run.owner_id.as_deref(), &status, &token_usage_records),
            "process repair proposal differs from verified completion"
        );
    }
    let repairing = repair.is_some()
        && matches!(
            prior_finalization,
            Some(ProcessFinalization::Unconfirmed | ProcessFinalization::TerminationFailed)
        );
    if let Some(previous) = prior_finalization.filter(|_| !repairing) {
        let same_status = match (&run.status, &status) {
            (SubAgentStatus::Completed(left), SubAgentStatus::Completed(right))
            | (SubAgentStatus::Failed(left), SubAgentStatus::Failed(right)) => left == right,
            _ => false,
        };
        anyhow::ensure!(
            previous == finalization && same_status,
            "process terminal conflicts with confirmed finalization"
        );
    }
    // Validate the entire merge before changing status or releasing the slot.
    let mut known = std::collections::BTreeMap::new();
    for usage in &run.token_usage_records {
        anyhow::ensure!(
            repair.is_none() || usage.settlement_id.is_some(),
            "verified process repair cannot reconcile unidentified prior usage"
        );
        if let Some(id) = usage.settlement_id.as_deref() {
            anyhow::ensure!(!id.is_empty(), "process settlement identity is empty");
            if let Some(previous) = known.insert(id, usage) {
                anyhow::ensure!(previous == usage, "process registry contains conflicting settlements");
            }
        }
    }
    let mut additions = Vec::new();
    for usage in &token_usage_records {
        if let Some(id) = usage.settlement_id.as_deref() {
            anyhow::ensure!(!id.is_empty(), "process settlement identity is empty");
            if let Some(previous) = known.get(id) {
                anyhow::ensure!(*previous == usage, "process settlement conflicts with prior usage");
                continue;
            }
            known.insert(id, usage);
        } else {
            anyhow::ensure!(
                prior_finalization.is_none(),
                "legacy process usage cannot be replayed without settlement identity"
            );
        }
        additions.push(usage.clone());
    }
    run.finished_at.get_or_insert_with(Utc::now);
    run.status = status;
    run.token_usage_records.extend(additions);
    run.steer_tx = None;

    #[cfg(test)]
    if let Some(hook) = commit_hook {
        let _ = hook.entered.send(());
        let _ = hook.release.await;
    }

    // Registry readers remain excluded by the write lock until both terminal
    // status and finalization are committed, so no caller can observe an early
    // slot release or terminal status with an unfinalized process control.
    if repairing {
        // The external observer has joined the sole monitor and still holds
        // the registry lock; the verified proposal and usage merge agree.
        process_control.finalized_tx.send_replace(Some(finalization));
    } else {
        process_control.finalize(finalization);
    }
    Ok(())
}

async fn commit_process_owner_failure_if_unfinalized(
    active_runs: &Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: &str,
    process_control: &ProcessRunControl,
    error: String,
) {
    if process_control.finalization().is_some() {
        return;
    }
    if let Err(commit_error) = commit_process_terminal_state(
        active_runs,
        run_id,
        SubAgentStatus::Failed(error),
        Vec::new(),
        process_control,
        ProcessFinalization::TerminationFailed,
        None,
        #[cfg(test)]
        None,
    )
    .await
    {
        tracing::error!(run_id, error = %commit_error, "process failure terminal remains unconfirmed");
    }
}

/// Make a process-mode run answer a kill aimed at its **own** registry row.
///
/// A task-mode run publishes an abort handle on its row, so
/// `registry::kill(row, cascade = false)` ends it. A process-mode run
/// deliberately publishes none: this monitor is the sole owner allowed to
/// signal and reap the OS child, and aborting it would strand the very process
/// the kill is meant to end. The consequence was that the row had *no*
/// termination mechanism at all — `prx tasks kill <batch-id> --no-cascade`
/// answered `not_killable` for every process-mode member, and only the default
/// cascade reached them, by signalling the worker's process group through the
/// child row [`run_sub_agent_process`] registers underneath.
///
/// The row now carries a cancellation token, and this bridge turns cancelling
/// it into exactly the termination the tool-plane `kill` action asks for: the
/// owner is told to stop, terminates and reaps its child, and commits
/// [`KILLED_BY_USER_REASON`]. No second owner, no new signalling path, no
/// clock — the registry simply reaches the mechanism that was already there.
///
/// The bridge ends with the run whichever way the run ends: it waits on the
/// token and on the control's finalization together, so a run that finishes on
/// its own does not leave a task behind.
///
/// MUTATION GUARD: drop the `token.cancelled()` arm and
/// `a_process_mode_member_answers_a_kill_of_its_own_row` reports `Requested`
/// instead of `Killed` and never sees the owner asked to stop.
fn bridge_registry_kill_to_process_control(control: Arc<ProcessRunControl>, token: CancellationToken) {
    tokio::spawn(async move {
        tokio::select! {
            () = token.cancelled() => {
                let outcome = control.request_termination(KILLED_BY_USER_REASON).await;
                tracing::info!(
                    ?outcome,
                    "work registry kill reached a process-mode sub-agent run; its owner was asked to terminate"
                );
            }
            () = control.finalized() => {}
        }
    });
}

/// Publish the registry row for a process-mode run, already wired to the only
/// mechanism that can end it.
///
/// One entry point rather than two statements at the call site, because the two
/// halves are not independently correct: a row published without the token is a
/// run the work registry cannot end at all (that was the bug), and a token
/// bridged onto a control whose row nobody registered is a kill nothing can
/// address. Keeping them together is also what lets a test exercise the wiring
/// production uses instead of a hand-built copy of it.
///
/// MUTATION GUARD: pass `None` for the token here and
/// `a_process_mode_member_answers_a_kill_of_its_own_row` reports
/// `NotKillable`.
fn register_killable_process_run(
    label: &str,
    run_id: &str,
    parent: Option<crate::runtime::registry::WorkId>,
    batch_id: Option<&str>,
    control: &Arc<ProcessRunControl>,
) -> crate::runtime::registry::WorkGuard {
    let token = CancellationToken::new();
    let guard = crate::runtime::registry::register_sub_agent(label, run_id, parent, batch_id, Some(token.clone()));
    bridge_registry_kill_to_process_control(Arc::clone(control), token);
    guard
}

/// Publish a terminal status for a run whose owning task vanished without
/// publishing one itself.
///
/// This is the discoverability half of the runtime's only automatic
/// termination. The idle (no-progress) detector ends a wedged turn by killing
/// everything the turn owns (`crate::agent::idle::kill_turn_subtree` ->
/// `crate::runtime::registry::kill`), and for a sub-agent that means *aborting
/// the monitor task* through the abort handle published on its registry row.
/// An aborted task runs no more code, so the row it was going to update is
/// left as it was: the registry's hang ledger records the termination while
/// `sessions_list` keeps reporting `running`, forever. Nothing polls it back
/// into line, so a join on that run would never return.
///
/// Only a non-terminal status is overwritten, so a terminal state committed by
/// the monitor microseconds earlier always wins.
async fn commit_run_termination_if_unfinished(
    active_runs: &Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: &str,
    reason: &str,
) -> bool {
    let mut runs = active_runs.write().await;
    let Some(run) = runs.iter_mut().find(|run| run.id == run_id) else {
        return false;
    };
    if matches!(run.status, SubAgentStatus::Completed(_) | SubAgentStatus::Failed(_)) {
        return false;
    }
    run.finished_at = Some(Utc::now());
    run.status = SubAgentStatus::Failed(reason.to_string());
    run.steer_tx = None;
    true
}

/// Why a monitor task stopped without reporting, when it stopped that way.
fn vanished_monitor_reason(error: &tokio::task::JoinError, kind: &str) -> String {
    if error.is_cancelled() {
        format!("{kind} {TERMINATED_BEFORE_RESULT_SUFFIX}")
    } else {
        format!("{kind} {PANICKED_BEFORE_RESULT_SUFFIX}")
    }
}

/// Watch a task-mode monitor from *outside* the run's registry subtree.
///
/// Awaiting the handle here neither aborts nor keeps the monitor alive; it only
/// observes how it ended. The watcher deliberately runs in a task that carries
/// no registry scope, so a cascade kill of the run cannot take the observer
/// down with the observed.
fn watch_task_mode_monitor(
    handle: tokio::task::JoinHandle<()>,
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: String,
    cleanup: Option<Arc<crate::runtime::turn_cleanup::CleanupTracker>>,
    task_control: Option<Arc<TaskRunControl>>,
    mut work_guard: Option<crate::runtime::registry::WorkGuard>,
    terminal_journal: Option<Arc<task_terminal::TaskTerminalJournal>>,
) {
    tokio::spawn(async move {
        let joined = handle.await;
        if let Some(control) = &task_control {
            control.revoke_authority();
        }
        // The observer outlives abort of the run, and owns its cleanup ticket
        // chain independently of the turn that originally spawned the run.
        let drained = match cleanup {
            Some(owner) => {
                owner
                    .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(15))
                    .await
            }
            None => Ok(()),
        };
        let recovered = if drained.is_ok() {
            if let Some(journal) = terminal_journal.as_ref() {
                let interruption = joined.as_ref().err().map(|error| {
                    let status = if error.is_cancelled() {
                        crate::agent::terminal::TurnTerminalStatus::Cancelled
                    } else {
                        crate::agent::terminal::TurnTerminalStatus::Failed
                    };
                    let reason = task_control
                        .as_ref()
                        .and_then(|control| control.reason())
                        .unwrap_or_else(|| vanished_monitor_reason(error, "sub-agent run"));
                    (status, reason)
                });
                Some(
                    tokio::time::timeout(std::time::Duration::from_secs(15), journal.recover(interruption))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|result| result),
                )
            } else {
                None
            }
        } else {
            None
        };
        let (reason, mut finalization) = if drained.is_err() {
            tracing::error!(run_id = %run_id, "Sub-agent child cleanup remains unconfirmed");
            (
                Some("sub-agent child cleanup remains unconfirmed".to_owned()),
                TaskFinalization::Unconfirmed,
            )
        } else if let Some(recovered) = recovered {
            let reconciled = match recovered {
                Ok(committed) => {
                    let mut runs = active_runs.write().await;
                    runs.iter_mut().find(|run| run.id == run_id).map_or_else(
                        || {
                            Err(anyhow::anyhow!(
                                "task registry disappeared before terminal reconciliation"
                            ))
                        },
                        |run| committed.reconcile_run(run),
                    )
                }
                Err(error) => Err(error),
            };
            match reconciled {
                Ok(status) => (
                    None,
                    if status == crate::agent::terminal::TurnTerminalStatus::Cancelled {
                        TaskFinalization::Terminated
                    } else {
                        TaskFinalization::Natural
                    },
                ),
                Err(error) => {
                    tracing::error!(run_id = %run_id, error = %error, "task terminal recovery remains unconfirmed");
                    (
                        Some("task terminal recovery remains unconfirmed".to_owned()),
                        TaskFinalization::Unconfirmed,
                    )
                }
            }
        } else if let Err(error) = joined {
            if error.is_cancelled() {
                (
                    Some(
                        task_control
                            .as_ref()
                            .and_then(|control| control.reason())
                            .map(|reason| {
                                if reason == KILLED_BY_USER_REASON {
                                    reason
                                } else {
                                    format!("sub-agent run ({reason}) {TERMINATED_BEFORE_RESULT_SUFFIX}")
                                }
                            })
                            .unwrap_or_else(|| vanished_monitor_reason(&error, "sub-agent run")),
                    ),
                    TaskFinalization::Terminated,
                )
            } else {
                (
                    Some(vanished_monitor_reason(&error, "sub-agent run")),
                    TaskFinalization::Unconfirmed,
                )
            }
        } else {
            (None, TaskFinalization::Natural)
        };
        if let Some(reason) = reason {
            if commit_run_termination_if_unfinished(&active_runs, &run_id, &reason).await {
                tracing::warn!(run_id = %run_id, "{reason}");
            } else if finalization == TaskFinalization::Terminated {
                // A previously committed terminal result wins the abort race.
                finalization = TaskFinalization::Natural;
            }
        }
        if finalization != TaskFinalization::Unconfirmed {
            if let Some(guard) = work_guard.as_mut() {
                guard.mark_cleanup_confirmed();
            }
        }
        if let Some(control) = task_control {
            control.publish(finalization);
        }
    });
}

/// Install the same externally-owned task monitor as a production task-mode
/// spawn on a test registry row.
///
/// Tests that exercise kill through a public tool must give a `Running` row
/// the owner production always installs. The watcher, rather than the caller,
/// remains responsible for joining the aborted task, committing its terminal
/// status, and publishing finalization to the kill request.
#[cfg(test)]
pub(crate) async fn attach_test_task_mode_owner(
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: &str,
) -> anyhow::Result<()> {
    let task_control = TaskRunControl::new();
    let handle = tokio::spawn(std::future::pending::<()>());
    let abort_handle = handle.abort_handle();
    task_control.attach(abort_handle.clone());
    {
        let mut runs = active_runs.write().await;
        let run = runs
            .iter_mut()
            .find(|run| run.id == run_id)
            .ok_or_else(|| anyhow::anyhow!("test task row `{run_id}` is unavailable"))?;
        anyhow::ensure!(
            run.task_control.is_none() && run.abort_handle.is_none(),
            "test task row `{run_id}` already has a termination owner"
        );
        run.task_control = Some(Arc::clone(&task_control));
        run.abort_handle = Some(abort_handle);
    }
    watch_task_mode_monitor(
        handle,
        active_runs,
        run_id.to_owned(),
        None,
        Some(task_control),
        None,
        None,
    );
    Ok(())
}

/// Process-mode counterpart of [`watch_task_mode_monitor`].
///
/// Routes through [`commit_process_owner_failure_if_unfinalized`] so the run's
/// `ProcessRunControl` is finalized together with the status — a caller parked
/// on finalization must not be stranded by the very cancellation that ended the
/// monitor.
fn watch_process_mode_monitor(
    handle: tokio::task::JoinHandle<()>,
    active_runs: Arc<RwLock<Vec<SubAgentRun>>>,
    run_id: String,
    process_control: Arc<ProcessRunControl>,
    mut work_guard: crate::runtime::registry::WorkGuard,
    verified_terminal: Arc<parking_lot::Mutex<Option<process_terminal::VerifiedWorkerCompletion>>>,
    recovery: Option<Arc<process_terminal::ProcessRecovery>>,
    ordinary_terminal: Arc<parking_lot::Mutex<Option<process_terminal::ordinary::VerifiedOrdinaryWorkerCompletion>>>,
    ordinary_recovery: Option<process_terminal::ordinary::OrdinaryProcessRecovery>,
    operator_terminal: Arc<parking_lot::Mutex<Option<process_terminal::operator::VerifiedOperatorWorkerCompletion>>>,
    operator_recovery: Option<process_terminal::operator::OperatorProcessRecovery>,
) {
    tokio::spawn(async move {
        let joined = handle.await;
        if !matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            let mut retained = verified_terminal.lock().clone();
            if retained.is_none() {
                if let Some(recovery) = recovery.as_ref() {
                    match recovery.recover().await {
                        Ok(completion) => retained = completion,
                        Err(error) => tracing::error!(run_id = %run_id, error = %error,
                            "process observer journal recovery remains unconfirmed"),
                    }
                }
            }
            if let Some(completion) = retained {
                if completion.run_id() == run_id {
                    let repair = recovery
                        .as_ref()
                        .map(|owner| owner.repair(completion.clone()))
                        .transpose();
                    let repair = match repair {
                        Ok(repair) => repair,
                        Err(error) => {
                            tracing::error!(run_id = %run_id, error = %error, "process repair authority unavailable");
                            None
                        }
                    };
                    let (result, usage) = completion.into_parts();
                    let status = if result.success {
                        SubAgentStatus::Completed(result.output)
                    } else {
                        SubAgentStatus::Failed(sub_agent_reported_failure(
                            result.error.as_deref().unwrap_or("worker failed"),
                        ))
                    };
                    if let Some(repair) = repair {
                        if let Err(error) = commit_process_terminal_state(
                            &active_runs,
                            &run_id,
                            status,
                            usage,
                            process_control.as_ref(),
                            ProcessFinalization::Natural,
                            Some(&repair),
                            #[cfg(test)]
                            None,
                        )
                        .await
                        {
                            tracing::error!(run_id = %run_id, error = %error,
                            "retained worker completion could not be reconciled");
                        }
                    }
                } else {
                    tracing::error!(run_id = %run_id, "retained worker completion belongs to another run");
                }
            }
        }
        if !matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            let mut completion = ordinary_terminal.lock().clone();
            if completion.is_none() {
                if let Some(recovery) = ordinary_recovery.as_ref() {
                    match recovery.recover().await {
                        Ok(recovered) => completion = recovered,
                        Err(error) => tracing::error!(run_id = %run_id, error = %error,
                            "ordinary monitor journal recovery remains unconfirmed"),
                    }
                }
            }
            if let Some(completion) = completion {
                let repair = completion.repair();
                match repair {
                    Ok(repair) => {
                        let status = if completion.result.success {
                            SubAgentStatus::Completed(completion.result.output)
                        } else {
                            SubAgentStatus::Failed(sub_agent_reported_failure(
                                completion.result.error.as_deref().unwrap_or("worker failed"),
                            ))
                        };
                        if let Err(error) = commit_process_terminal_state(
                            &active_runs,
                            &run_id,
                            status,
                            completion.usage,
                            process_control.as_ref(),
                            ProcessFinalization::Natural,
                            Some(&repair),
                            #[cfg(test)]
                            None,
                        )
                        .await
                        {
                            tracing::error!(run_id = %run_id, error = %error, "ordinary completion registry repair remains unconfirmed");
                        }
                    }
                    Err(error) => {
                        tracing::error!(run_id = %run_id, error = %error, "ordinary process exit remains unconfirmed")
                    }
                }
            }
        }
        if !matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            if let Some(recovery) = ordinary_recovery.as_ref() {
                if let Err(error) = recovery.retain_partial(&active_runs).await {
                    tracing::error!(run_id = %run_id, error = %error, "ordinary partial usage remains unconfirmed");
                }
            }
        }
        if !matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            let mut completion = operator_terminal.lock().clone();
            if completion.is_none() {
                if let Some(recovery) = operator_recovery.as_ref() {
                    match recovery.recover().await {
                        Ok(recovered) => completion = recovered,
                        Err(error) => tracing::error!(run_id = %run_id, error = %error,
                            "operator monitor journal recovery remains unconfirmed"),
                    }
                }
            }
            if let Some(completion) = completion {
                let repair = completion.repair();
                match repair {
                    Ok(repair) => {
                        let status = if completion.result.success {
                            SubAgentStatus::Completed(completion.result.output)
                        } else {
                            SubAgentStatus::Failed(sub_agent_reported_failure(
                                completion.result.error.as_deref().unwrap_or("worker failed"),
                            ))
                        };
                        if let Err(error) = commit_process_terminal_state(
                            &active_runs,
                            &run_id,
                            status,
                            completion.usage,
                            process_control.as_ref(),
                            ProcessFinalization::Natural,
                            Some(&repair),
                            #[cfg(test)]
                            None,
                        )
                        .await
                        {
                            tracing::error!(run_id = %run_id, error = %error, "operator completion registry repair remains unconfirmed");
                        }
                    }
                    Err(error) => {
                        tracing::error!(run_id = %run_id, error = %error, "operator process exit remains unconfirmed")
                    }
                }
            }
        }
        if !matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            if let Some(recovery) = operator_recovery.as_ref() {
                if let Err(error) = recovery.retain_partial(&active_runs).await {
                    tracing::error!(run_id = %run_id, error = %error, "operator partial usage remains unconfirmed");
                }
            }
        }
        if process_control.finalization().is_none() {
            let reason = joined.err().map_or_else(
                || "process monitor ended with unconfirmed settlement".to_owned(),
                |error| vanished_monitor_reason(&error, "sub-agent process monitor"),
            );
            let mut runs = active_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|run| run.id == run_id) {
                run.status = SubAgentStatus::Failed(format!("{reason}; cleanup and provider usage remain unconfirmed"));
                run.steer_tx = None;
            }
            process_control.finalize(ProcessFinalization::Unconfirmed);
            tracing::error!(run_id = %run_id, "process observer could not confirm cleanup and settlement");
        }
        if matches!(
            process_control.finalization(),
            Some(ProcessFinalization::Natural | ProcessFinalization::Terminated)
        ) {
            work_guard.mark_cleanup_confirmed();
        }
        // Unconfirmed and failed termination retain an abandoned registry row.
    });
}

/// Reason recorded when an operator ends a run through the tool/chat kill path.
///
/// A shared constant rather than a literal at each site because the *reader* of
/// the status — `join`'s outcome classification, chat's `Failed -> Cancelled`
/// projection — has to tell an operator kill from a task that failed on its own
/// terms, and the only carrier of that distinction is this wording.
pub(crate) const KILLED_BY_USER_REASON: &str = "killed by user";

/// Tail of the reason [`vanished_monitor_reason`] records when a run's monitor
/// was cancelled out from under it — the shape of a cascade kill
/// (`prx tasks kill`, `kill_turn_subtree`), which reaches the run through the
/// registry and never gets to write a result of its own.
pub(crate) const TERMINATED_BEFORE_RESULT_SUFFIX: &str = "was terminated before it could report a result";

/// Whether a `Failed` reason describes a kill rather than a task-level failure.
///
/// A *sub-rule* of [`termination_cause`], which only reaches it for a reason
/// the runtime itself wrote — the suffix match below is deliberately never
/// applied to text a sub-agent could have chosen.
///
/// MUTATION GUARD: collapse this into `false` and a killed member of a joined
/// batch is reported as an ordinary failure, which is the one distinction the
/// caller of `join` needs in order to decide whether to retry it.
fn failure_is_kill(reason: &str) -> bool {
    reason == KILLED_BY_USER_REASON || reason.ends_with(TERMINATED_BEFORE_RESULT_SUFFIX)
}

/// Head of the reason recorded when a process-mode worker's pipe closed before
/// it ever wrote a result line — b1's classification of a SIGKILL, an OOM kill
/// or a segfault.
pub(crate) const EXITED_WITHOUT_RESULT_PREFIX: &str = "worker exited without result";

/// Tail of the reason recorded when a run's monitor task panicked.
///
/// The counterpart of [`TERMINATED_BEFORE_RESULT_SUFFIX`]: both describe a run
/// that stopped without a conclusion, but a cancellation was *asked for* by
/// somebody and a panic was not.
pub(crate) const PANICKED_BEFORE_RESULT_SUFFIX: &str = "panicked before it could report a result";

/// Head of the reason recorded when the process-mode owner task itself blew up
/// while holding the OS child.
pub(crate) const PROCESS_OWNER_PANICKED_PREFIX: &str = "process owner panicked";

/// Marker the runtime writes in front of a failure reason that came out of the
/// sub-agent itself.
///
/// Every other constant in this group is wording the *runtime* records about a
/// run. This one quarantines wording the run recorded about itself — a
/// process-mode worker's `WorkerResult.error`, an agent loop error carrying a
/// tool's or a provider's message — none of which the runtime authored and all
/// of which a sub-agent (or the untrusted input it is chewing on) can choose.
///
/// It is the load-bearing half of [`termination_cause`]: because the tag can
/// only ever sit at the *front* of a reason and only the runtime puts it there,
/// a sub-agent that names its own failure `killed by user`, `timeout`, or
/// anything ending in [`TERMINATED_BEFORE_RESULT_SUFFIX`] cannot relabel how
/// its own termination is reported to whoever joins the batch.
pub(crate) const SUB_AGENT_REPORTED_PREFIX: &str = "sub-agent reported:";

/// Wrap a sub-agent's own account of why it failed in the quarantine tag.
///
/// A function rather than a `format!` at each site so the tag and the space
/// after it cannot drift, and so a test can build the exact reason production
/// would from an adversarial worker payload.
fn sub_agent_reported_failure(detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        SUB_AGENT_REPORTED_PREFIX.to_string()
    } else {
        format!("{SUB_AGENT_REPORTED_PREFIX} {detail}")
    }
}

/// Which of the three ways a run can stop this reason describes.
///
/// `join`'s buckets are a claim about *who* ended a run, and until this
/// summer that claim was read out of the reason's prose — so a sub-agent that
/// wrote the right words into its own error could pick its own bucket, and a
/// generic one like `timeout` could collide with a runtime wording by
/// accident. The classification now follows the reason's *provenance*, which
/// the writer stamps and the reader only reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminationCause {
    /// Somebody ended the run: an operator kill, or a cascade that reached it.
    Killed,
    /// The run stopped without ever reaching a conclusion of its own.
    NoConclusion,
    /// The account came from the sub-agent, so it is a task-level verdict —
    /// whatever words it happens to contain.
    SelfReported,
}

/// Read a `Failed` reason's provenance.
///
/// The order of the checks is the whole design and is not interchangeable:
///
/// 1. The quarantine tag first. A reason the runtime marked as the sub-agent's
///    own account is a task verdict no matter what it says, which is what makes
///    the wordings below unforgeable rather than merely unlikely to collide.
/// 2. The runtime tags that are *anchored at the front* and carry an untrusted
///    tail — [`EXITED_WITHOUT_RESULT_PREFIX`] embeds a preview of the worker's
///    stderr, and [`PROCESS_OWNER_PANICKED_PREFIX`] embeds a cleanup error.
///    They are read before the suffix rule below because a worker whose last
///    stderr bytes are [`TERMINATED_BEFORE_RESULT_SUFFIX`] would otherwise
///    relabel its own silent death as an operator kill.
/// 3. Only then the remaining wordings, none of which an untrusted string can
///    reach any more.
///
/// An untagged reason from somewhere that never went through the writers here
/// falls to `SelfReported`: it is the one bucket that claims nothing about the
/// runtime having acted.
///
/// MUTATION GUARD: delete the first arm and
/// `a_sub_agent_cannot_forge_its_own_termination_verdict` fails — a worker
/// whose reported error ends in [`TERMINATED_BEFORE_RESULT_SUFFIX`] is filed
/// as an operator kill.
fn termination_cause(reason: &str) -> TerminationCause {
    if reason.starts_with(SUB_AGENT_REPORTED_PREFIX) {
        return TerminationCause::SelfReported;
    }
    if reason.starts_with(EXITED_WITHOUT_RESULT_PREFIX) || reason.starts_with(PROCESS_OWNER_PANICKED_PREFIX) {
        return TerminationCause::NoConclusion;
    }
    if failure_is_kill(reason) {
        return TerminationCause::Killed;
    }
    if failure_gave_no_conclusion(reason) {
        return TerminationCause::NoConclusion;
    }
    TerminationCause::SelfReported
}

/// Turn an agent-loop error into the reason its run's row will carry.
///
/// The decision is made from the error's *type*, on the task that owns the run,
/// at the moment it ends — the only place where the runtime still knows what
/// actually happened. Rendering to text is the last step, and once rendered the
/// text is only ever an explanation for a human; [`termination_cause`] reads the
/// tag this function chose, not the prose.
///
/// MUTATION GUARD: replace the body with `error.to_string()` and
/// `a_task_mode_failure_is_classified_by_the_error_type_not_its_words` fails —
/// a cancelled loop is filed as the task's own verdict, and a tool error that
/// merely quotes the hang detector is filed as `no_result`.
fn task_failure_reason(error: &anyhow::Error) -> String {
    if error
        .chain()
        .any(|source| source.is::<crate::agent::idle::IdleHangTerminated>())
    {
        // The runtime's own hang detector ended this turn: its rendering already
        // carries the markers `failure_gave_no_conclusion` recognises, and no
        // sub-agent can produce this type.
        error.to_string()
    } else if crate::agent::turn_contracts::is_tool_loop_cancelled(error) {
        // Somebody cancelled the loop — `prx tasks kill`, a parent cascade, a
        // shutdown. That is a kill, and it is the same shape
        // `vanished_monitor_reason` records for the cascade it can observe.
        format!("sub-agent run {TERMINATED_BEFORE_RESULT_SUFFIX}")
    } else {
        sub_agent_reported_failure(&error.to_string())
    }
}

/// The reason a run's row carries after its worker died without writing a
/// result.
///
/// A function rather than a `format!` at the single write site so a test can
/// build the exact wording production would, from a real killed child, instead
/// of re-typing it and drifting.
fn exited_without_result_reason(detail: &str) -> String {
    format!("{EXITED_WITHOUT_RESULT_PREFIX} ({detail})")
}

/// What a spawn confirmation tells the caller about where the result will show
/// up.
///
/// The two sentences are not decoration: a model that just launched a batch has
/// to know that nothing further will arrive on its own, or it will sit waiting
/// for announcements that were switched off and never call `join`.
const fn spawn_completion_note(announce_result: bool) -> &'static str {
    if announce_result {
        "Will announce result when complete."
    } else {
        "Result is not announced; collect it with the 'join' action on this batch_id."
    }
}

/// Whether a `Failed` reason describes a run that never reached a conclusion at
/// all, as opposed to a task that concluded it had failed.
///
/// This is the distinction `join` reports as `no_result` versus `failed`, and it
/// is not cosmetic: a caller reading `failed` learns that the sub-agent looked
/// at the problem and judged it undoable, while `no_result` says the sub-agent
/// was cut down before it could judge anything. Collapsing the two invites the
/// caller to trust a verdict that was never rendered.
///
/// Every arm matches a constant that the *writing* site also uses, so the two
/// halves cannot drift. A string is the only channel available because a run's
/// outcome reaches the registry as `SubAgentStatus::Failed(String)`, but the
/// string is no longer trusted on its own: this predicate is a *sub-rule* of
/// [`termination_cause`], which refuses to consult it at all for a reason the
/// sub-agent authored.
///
/// MUTATION GUARD: collapse this into `false` and every silent death is
/// reported as a considered failure.
fn failure_gave_no_conclusion(reason: &str) -> bool {
    reason.starts_with(EXITED_WITHOUT_RESULT_PREFIX)
        || reason.ends_with(PANICKED_BEFORE_RESULT_SUFFIX)
        || reason.starts_with(PROCESS_OWNER_PANICKED_PREFIX)
        || crate::agent::idle::message_describes_hang_termination(reason)
}

/// What one spawn attempt produced: the tool-visible result, plus the run id
/// when a run was actually registered.
pub(crate) struct SpawnOutcome {
    result: ToolResult,
    run_id: Option<String>,
}

impl SpawnOutcome {
    /// The request never became a run (bad argument, denied gate, unusable
    /// agent/provider). There is nothing to join.
    const fn rejected(error: String) -> Self {
        Self {
            result: ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            },
            run_id: None,
        }
    }

    /// A run is registered and its driver task is live.
    const fn started(run_id: String, output: String) -> Self {
        Self {
            result: ToolResult {
                success: true,
                output,
                error: None,
            },
            run_id: Some(run_id),
        }
    }
}

/// Terminal verdict for one member of a joined batch.
///
/// `Killed` is deliberately not folded into `Failed`: a task that concluded
/// "this cannot be done" and a task an operator stopped mid-flight mean opposite
/// things to whoever reads the summary.
enum JoinedVerdict {
    Completed(String),
    /// The task looked at the problem and concluded it could not be done. The
    /// string is the sub-agent's own account of why, which is exactly why it
    /// is tagged with [`SUB_AGENT_REPORTED_PREFIX`] on the way in: this is the
    /// one bucket whose contents the run itself gets to write.
    Failed(String),
    Killed(String),
    /// The run produced no conclusion at all: its worker died on a signal, its
    /// monitor panicked, its own hang detector ended it, its deadline expired,
    /// or its registry row is simply gone. Kept apart from `Failed` because a
    /// caller reading a failure reasonably believes a judgement was made, and
    /// here none ever was — see [`failure_gave_no_conclusion`].
    NoResult(String),
}

/// One settled member of a joined batch.
struct JoinedMember {
    run_id: String,
    task: String,
    verdict: JoinedVerdict,
}

/// How often the join wait re-reads the batch.
///
/// This is a *sampling* interval, not a deadline. `join_batch_members` has no
/// elapsed-time check, no iteration cap and no `tokio::time::timeout`: shorten
/// this and a finished batch surfaces sooner, lengthen it and the only cost is
/// latency. The tests in this module pin that difference — one of them runs a
/// batch across an hour of (virtual) time and still requires the real result.
const JOIN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Render the settled batch as the structured summary `join` returns.
///
/// Structured rather than concatenated prose because the four buckets mean
/// different things to whoever reads them, and a caller deciding what to retry
/// has to be able to tell them apart without parsing English.
///
/// Two things keep `total` honest: the exhaustive match below, which gives
/// every verdict exactly one bucket and makes a new variant a compile error
/// here, and the debug assertion, which catches a bucket that is filled but
/// never rendered. Neither is decoration — a member that quietly fits nowhere
/// would disappear from the caller's account of its own fan-out.
fn join_summary(batch_id: &str, settled: &[JoinedMember]) -> serde_json::Value {
    let mut completed = Vec::new();
    let mut failed = Vec::new();
    let mut killed = Vec::new();
    let mut no_result = Vec::new();
    for member in settled {
        match &member.verdict {
            JoinedVerdict::Completed(output) => {
                completed.push(json!({"run_id": member.run_id, "task": member.task, "output": output}));
            }
            JoinedVerdict::Failed(error) => {
                failed.push(json!({"run_id": member.run_id, "task": member.task, "error": error}));
            }
            JoinedVerdict::Killed(reason) => {
                killed.push(json!({"run_id": member.run_id, "task": member.task, "reason": reason}));
            }
            JoinedVerdict::NoResult(reason) => {
                no_result.push(json!({"run_id": member.run_id, "task": member.task, "reason": reason}));
            }
        }
    }
    let total = settled.len();
    debug_assert_eq!(
        total,
        completed.len() + failed.len() + killed.len() + no_result.len(),
        "every joined member must appear in exactly one bucket"
    );
    json!({
        "batch_id": batch_id,
        "total": total,
        "completed": completed,
        "failed": failed,
        "killed": killed,
        "no_result": no_result,
    })
}

/// Classify a run's current status, or `None` while it is still working.
fn settled_verdict(status: &SubAgentStatus) -> Option<JoinedVerdict> {
    match status {
        // `AwaitingInput` is reversible by design (the operator approves,
        // denies, or the request lapses), so it is *not* terminal and the join
        // keeps waiting through it.
        SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => None,
        SubAgentStatus::Completed(output) => Some(JoinedVerdict::Completed(output.clone())),
        // One dispatch on the reason's provenance rather than a chain of
        // guards, so the precedence between the three causes lives in exactly
        // one place — [`termination_cause`] — and cannot be reordered here by
        // accident.
        SubAgentStatus::Failed(reason) => Some(match termination_cause(reason) {
            TerminationCause::Killed => JoinedVerdict::Killed(reason.clone()),
            TerminationCause::NoConclusion => JoinedVerdict::NoResult(reason.clone()),
            TerminationCause::SelfReported => JoinedVerdict::Failed(reason.clone()),
        }),
    }
}

/// Block until every member of the batch has reached a terminal status.
///
/// # Why this has no deadline
///
/// A deadline here would be a wall-clock turn timeout wearing a different hat:
/// it would end a fan-out because it took long, and duration is not a fault in
/// this runtime. What actually bounds the wait is that *every member is bounded
/// on its own terms* — a task-mode member runs under
/// [`crate::agent::idle::run_guarded`], and a process-mode member's worker
/// installs the same thresholds in its own process. Every ending commits a
/// terminal status on the member's row rather than leaving it `Running`. When
/// the last member ends, this returns; there is no third outcome to time out
/// on.
///
/// That bound is real but conditional, and the condition is worth stating: a
/// member's watchdog only fires while the runtime it lives in still schedules.
/// A worker stopped by a signal, wedged in a blocking call, or hung before it
/// reached [`crate::agent::idle::install`] has no watchdog left to end it. What
/// covers *that* is not a deadline here — it is that the join stops vouching for
/// members it can no longer account for, so the caller's own detector reaches a
/// verdict. See [`member_vouches_for_the_waiter`] for which members those are,
/// and for the one class it cannot yet tell apart.
///
/// # Why it stamps the caller's beat, and why not on every poll
///
/// The caller is parked here, so it emits nothing of its own, and its own hang
/// detector would eventually judge it wedged. For task-mode members that never
/// happens while they work — their beats are parented on the caller's, so their
/// progress is the caller's progress — but that link only exists when the same
/// turn both spawned and joined, so the wait re-reads their beats rather than
/// relying on it. A process-mode member has no such link at all: the only signal
/// that crosses the process boundary is the bytes the worker writes, and a
/// healthy `session-worker` writes nothing on either pipe until the single
/// result line at the very end. Waiting on one is indistinguishable from
/// hanging.
///
/// So the wait itself supplies the evidence, as
/// [`crate::agent::idle::ProgressKind::SubtaskAlive`]. What it must *not* do is
/// supply it merely because some member's row has not been stamped terminal
/// yet: that is the absence of an ending, not the presence of work, and a turn
/// whose window is refreshed by it is immortal for as long as one member fails
/// to die — which in a runtime with no wall-clock turn budget means the idle
/// detector, the only automatic recovery there is, is switched off for the whole
/// join. [`member_vouches_for_the_waiter`] is the rule that keeps the evidence
/// honest; when no member satisfies it, this loop keeps polling but records
/// nothing, and the caller's own detector is free to reach its verdict.
async fn join_batch_members(active_runs: &Arc<RwLock<Vec<SubAgentRun>>>, members: &[BatchMember]) -> Vec<JoinedMember> {
    // Read once per join rather than per poll: this is the same source a
    // task-mode member's own `run_guarded` resolves its window from, so the two
    // cannot disagree about when that member was due to be judged.
    let window = crate::agent::idle::configured().idle;
    loop {
        // The guard is bound and dropped inside this block: the wait below must
        // never hold the registry lock, or a member trying to publish its own
        // terminal status would be blocked by the very join waiting for it.
        let vouched = {
            let runs = active_runs.read().await;
            if let Some(settled) = settled_batch(&runs, members) {
                return settled;
            }
            batch_vouches_for_the_waiter(&runs, members, window)
        };
        if vouched {
            // MUTATION GUARD: this line is the whole reason a turn joining a
            // process-mode fan-out is not killed as hung. Remove it and a batch
            // whose members produce no in-band signal takes the joining turn
            // down with it after `[runtime] idle_hang_secs`.
            crate::agent::idle::beat(crate::agent::idle::ProgressKind::SubtaskAlive);
        }
        tokio::time::sleep(JOIN_POLL_INTERVAL).await;
    }
}

/// Whether any member of the batch is still evidence that the waiting turn is
/// alive.
///
/// A member whose row has vanished contributes nothing: it is not something the
/// join is waiting on, it is something [`settled_batch`] already reports as
/// having no result.
fn batch_vouches_for_the_waiter(
    runs: &[SubAgentRun],
    members: &[BatchMember],
    window: Option<std::time::Duration>,
) -> bool {
    members.iter().any(|member| {
        runs.iter()
            .find(|run| run.id == member.run_id)
            .is_some_and(|run| member_vouches_for_the_waiter(run, window))
    })
}

/// Whether this member may be recorded as
/// [`crate::agent::idle::ProgressKind::SubtaskAlive`] on the waiting turn's
/// beat.
///
/// The question is deliberately *not* "is this member still running": every row
/// starts `Running` and stays that way until somebody commits an ending, so
/// answering yes to that is answering "nobody has told me it stopped", and a
/// turn kept alive by that is kept alive by silence. The question asked here is
/// "can I still account for this member", and it has three answers:
///
/// * **Blocked on a human.** `AwaitingInput` is a positive statement about
///   where the run is — an operator has been asked something and has not
///   answered — so it vouches. A join that gave up on a pending approval would
///   be terminating a turn for the operator's slowness.
/// * **Observable, and within its own window.** A task-mode member runs in this
///   process under [`crate::agent::idle::run_guarded`] with the same `window`,
///   and every provider chunk, tool call and compaction stamps the beat this
///   reads. While that beat is fresher than the window, the member is visibly
///   working. Once it is not, the member's own watchdog was due and did not
///   fire, so the premise that "every member is bounded on its own terms" is
///   false for this member and the join must stop underwriting it. `window` of
///   `None` means idle detection is switched off process-wide, so there is no
///   verdict to protect and nothing to withhold.
/// * **Not observable at all.** A process-mode member is byte-silent by
///   construction, so its beat's age says nothing and applying the rule above
///   would kill the parents of perfectly healthy fan-outs. It vouches on its
///   non-terminal status — the one place this function still trusts an absence.
///   That blind spot is bounded by the worker's own in-process watchdog and by
///   its in-process idle watchdog, and closing it properly needs a liveness frame
///   the worker's one-line stdout protocol does not carry today.
///
/// MUTATION GUARD: return `true` unconditionally and
/// `a_join_stops_vouching_for_an_observable_member_that_went_silent` goes green
/// on a turn that should have been terminated; return `false` for the
/// process-mode arm and `a_join_survives_a_real_child_process_that_never_writes_anything`
/// kills a turn whose child is working.
fn member_vouches_for_the_waiter(run: &SubAgentRun, window: Option<std::time::Duration>) -> bool {
    if matches!(run.status, SubAgentStatus::AwaitingInput { .. }) {
        return true;
    }
    if run.process_control.is_some() {
        return true;
    }
    window.is_none_or(|window| run.idle_for() < window)
}

/// The batch's verdicts, or `None` while any member is still working.
///
/// Deliberately two passes under one lock rather than one pass that builds as
/// it goes: a finished member's output can be large, and a join that polls for
/// an hour would otherwise clone all of them on every poll only to throw them
/// away. Building only once the answer is final also means the verdicts come
/// from a single consistent snapshot, so a member cannot be seen `Completed`
/// on the deciding pass and be gone by the reporting one.
fn settled_batch(runs: &[SubAgentRun], members: &[BatchMember]) -> Option<Vec<JoinedMember>> {
    let all_settled = members.iter().all(|member| {
        runs.iter()
            .find(|run| run.id == member.run_id)
            // A row that is gone is settled in the only sense that matters
            // here: nothing about it will ever change again.
            .is_none_or(|run| !matches!(run.status, SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. }))
    });
    if !all_settled {
        return None;
    }
    Some(
        members
            .iter()
            .map(|member| {
                runs.iter().find(|run| run.id == member.run_id).map_or_else(
                    // The roster names this member but the registry no longer
                    // holds it — retired by the chat session reaper, or dropped
                    // by a session shutdown. Reporting it as a member without a
                    // result is the honest verdict, and it is the whole reason
                    // the roster is the membership: derived from the registry,
                    // this member would not have been in the batch at all.
                    || JoinedMember {
                        run_id: member.run_id.clone(),
                        task: member.task.clone(),
                        verdict: JoinedVerdict::NoResult(
                            "no confirmed registry result is available for this recorded batch member".to_string(),
                        ),
                    },
                    |run| JoinedMember {
                        run_id: member.run_id.clone(),
                        task: run.task.clone(),
                        // Non-terminal statuses were ruled out above; a status
                        // that slipped through is reported as the absence it is
                        // rather than panicking inside a tool call.
                        verdict: settled_verdict(&run.status).unwrap_or_else(|| {
                            JoinedVerdict::NoResult("the run was still working when the batch settled".to_string())
                        }),
                    },
                )
            })
            .collect(),
    )
}

#[async_trait]
impl Tool for SessionsSpawnTool {
    fn name(&self) -> &str {
        "sessions_spawn"
    }

    fn description(&self) -> &str {
        "Hand work to an async sub-agent or to a live `prx chat` session, and manage what is running. \
         Actions and their parameters: 'spawn' (default) task -> run_id; 'spawn_batch' tasks -> batch_id; \
         'join' batch_id (waits for every member, returns all results); 'list'; 'kill' run_id; \
         'history' run_id; 'steer' run_id + message; 'chat_sessions'; 'chat_assign' session_id + task. \
         Work text always goes in `task`, never `message`."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let available_agents = self
            .agents
            .iter()
            .filter(|(_, cfg)| cfg.spawn_enabled.unwrap_or(true))
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        crate::tools::schema::with_default_action_requirements(
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["spawn", "spawn_batch", "join", "list", "kill", "history", "steer", "chat_sessions", "chat_assign"],
                        "default": "spawn",
                        "description": "Action to perform, with the parameter it reads: 'spawn' task; \
                         'spawn_batch' tasks; 'join' batch_id; 'list' none; 'kill' run_id; 'history' run_id; \
                         'steer' run_id + message; 'chat_sessions' none; 'chat_assign' session_id + task."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "For 'chat_assign': the live `prx chat` session that gets the task, as listed by 'chat_sessions'. A chat session id, not a sub-agent run_id."
                    },
                    "disposition": {
                        "type": "string",
                        "enum": ["queue", "steer", "interrupt"],
                        "default": "queue",
                        "description": "For 'chat_assign': how to handle a turn that session is already running. 'queue' (default) waits, 'steer' feeds the task to the running turn, 'interrupt' stops it first. Prefer 'queue'."
                    },
                    "tasks": {
                        "type": "array",
                        "minItems": 1,
                        "description": "For 'spawn_batch': the tasks to launch together, unlimited in number. Each entry is a task string or an object {task, agent?, model?, provider?, mode?, recipient?}; any other key is rejected. Top-level parameters are per-entry defaults that an entry key overrides. Members never announce their own results: call 'join' with the returned batch_id and report them yourself.",
                        "items": {
                            "anyOf": [
                                {"type": "string", "minLength": 1},
                                Self::batch_member_entry_schema()
                            ]
                        }
                    },
                    "batch_id": {
                        "type": "string",
                        "description": "For 'join': the batch identifier returned by 'spawn_batch'. 'join' blocks until every member has finished, however long that takes."
                    },
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The work to be done, as instructions. Required by 'spawn' and by 'chat_assign' \
                                        ('chat_assign' has no `message`, `prompt` or `text` parameter). Use `tasks` for \
                                        'spawn_batch' and `message` for 'steer'."
                    },
                    "run_id": {
                        "type": "string",
                        "description": "Sub-agent run id returned by 'spawn' and listed by 'list'. Required by 'kill', 'history' and 'steer'. Not a chat session id."
                    },
                    "message": {
                        "type": "string",
                        "description": "For 'steer': text injected into an already-running sub-agent to redirect it. \
                                        Not how work is handed out — 'spawn' and 'chat_assign' take their instructions in `task`."
                    },
                    "model": {
                        "type": "string",
                        "description": "For 'spawn' and 'spawn_batch': model override. Defaults to the gateway model."
                    },
                    "provider": {
                        "type": "string",
                        "description": "For 'spawn' and 'spawn_batch': provider override. Must be a provider the operator configured (the gateway provider, a named agent's provider, a fallback provider or a model route); any other provider, including an unconfigured 'custom:<url>', is refused. Defaults to the agent config provider, then the gateway provider."
                    },
                    "agent": {
                        "type": "string",
                        "description": format!(
                            "For 'spawn' and 'spawn_batch': identity agent name. Available: {}",
                            if available_agents.is_empty() {
                                "(none configured)".to_string()
                            } else {
                                available_agents.join(", ")
                            }
                        )
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["task", "process"],
                        "default": "task",
                        "description": "For 'spawn' and 'spawn_batch': 'task' runs in-process (default), 'process' launches an isolated OS process."
                    },
                    "recipient": {
                        "type": "string",
                        "description": "For 'spawn': recipient (phone number, group ID, …) of the result announcement and any \
                                        kill notice; defaults to the current sender. Under 'spawn_batch' it routes only the \
                                        kill notice. 'chat_assign' always replies into the assigning conversation."
                    },
                    "announce": {
                        "type": "boolean",
                        "description": "For 'spawn', default true: whether the sub-agent posts its own result to the channel. \
                                        A 'spawn_batch' member never announces; collect a batch with 'join' and report it once."
                    },
                    "last_n": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 200,
                        "description": "For 'history': return the last N entries (default 20, keeping final proof markers visible)."
                    },
                    "max_chars_per_entry": {
                        "type": "integer",
                        "minimum": 80,
                        "maximum": 4000,
                        "description": "For 'history': maximum characters per returned entry (default 800)."
                    }
                },
                "required": ["action"]
            }),
            "action",
            "spawn",
            &[
                crate::tools::schema::ActionRequirement {
                    action: "spawn",
                    required: &["task"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "spawn_batch",
                    required: &["tasks"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "kill",
                    required: &["run_id"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "history",
                    required: &["run_id"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "steer",
                    required: &["run_id", "message"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "chat_assign",
                    required: &["session_id", "task"],
                },
                crate::tools::schema::ActionRequirement {
                    action: "join",
                    required: &["batch_id"],
                },
            ],
        )
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
        crate::tools::execution::verify_current_child_call("sessions_spawn")?;
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("spawn");
        if action == "spawn_batch" {
            anyhow::ensure!(
                !self
                    .hint_config
                    .as_ref()
                    .is_some_and(|config| config.causal_tree.enabled)
                    || crate::runtime::child_source::outgoing_grant().is_some(),
                "decision-enabled spawning requires an issued child grant"
            );
        }
        // OWNER-VIS: every run lookup below is confined to the runs this
        // caller's owner started; the local-process operator sees them all. A
        // run outside the scope answers exactly like a missing one.
        let owner_scope = sessions_read_model::RunOwnerScope::from_args(&args);

        match action {
            "list" => return self.execute_list(&owner_scope).await,
            "kill" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for kill action"))?;
                let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                // Identity for the kill receipt's outbound gate, taken from the
                // killing turn's own trusted scope. Unlike a completion
                // announcement — which fires long after its launching turn ended
                // and therefore has to rely on an identity captured at spawn
                // time — a kill happens *inside* a live turn, so the authority
                // for the message it emits is present rather than remembered.
                // An absent or untrusted scope degrades to the same "unknown"
                // identity `AnnounceOrigin` documents, never a privileged one.
                let notice_origin = AnnounceOrigin::from_scope(parse_spawn_scope(&args).as_ref());
                return self
                    .execute_kill(
                        run_id,
                        approval_grant.as_ref(),
                        notice_origin.as_ref(),
                        &owner_scope,
                        &args,
                    )
                    .await;
            }
            "history" => {
                let run_id = args
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter for history action"))?;
                let last_n = args
                    .get("last_n")
                    .and_then(|v| v.as_u64())
                    .map(|value| value.clamp(1, 200) as usize);
                let max_chars_per_entry = args
                    .get("max_chars_per_entry")
                    .and_then(|v| v.as_u64())
                    .map(|value| value.clamp(80, 4000) as usize)
                    .unwrap_or(DEFAULT_HISTORY_ENTRY_MAX_CHARS);
                return self
                    .execute_history(run_id, last_n, max_chars_per_entry, &owner_scope, &args)
                    .await;
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
                let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
                return self
                    .execute_steer(run_id, message, approval_grant.as_ref(), &owner_scope)
                    .await;
            }
            "chat_sessions" => return self.execute_chat_sessions(&args),
            "chat_assign" => return self.execute_chat_assign(&args).await,
            "spawn_batch" => return self.execute_spawn_batch(&args).await,
            "join" => {
                let batch_id = args
                    .get("batch_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'batch_id' parameter for join action"))?;
                return self.execute_join(batch_id, &owner_scope).await;
            }
            _ => {} // fall through to spawn
        }

        Ok(self.execute_spawn(&args, None).await?.result)
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Automation]
    }
}

impl SessionsSpawnTool {
    /// What one `spawn_batch` member entry may carry — declared exactly once.
    ///
    /// This is both what the model is told (it is inlined into `tasks.items`
    /// by [`SessionsSpawnTool::parameters_schema`]) and what the merge in
    /// [`Self::batch_member_args`] enforces, via the key set derived from it
    /// in [`Self::batch_member_overridable_keys`]. One declaration, so the
    /// advertised contract and the enforced one cannot drift apart.
    ///
    /// `announce` is deliberately **absent**. A fan-out's account of itself is
    /// the single summary its caller sends after `join`; a member that could
    /// switch its own announcement on would turn that into N+1 messages, and
    /// because the whitelist is derived from this object, listing the key here
    /// is the same thing as letting the model set it. A run that should report
    /// for itself is a plain `spawn`.
    fn batch_member_entry_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task": {"type": "string", "minLength": 1},
                "agent": {"type": "string"},
                "model": {"type": "string"},
                "provider": {"type": "string"},
                "mode": {"type": "string", "enum": ["task", "process"]},
                "recipient": {"type": "string"},
            },
            "required": ["task"]
        })
    }

    /// The keys a member entry may set, read off [`Self::batch_member_entry_schema`].
    ///
    /// Derived rather than restated: a parameter added to the entry schema is
    /// accepted here the same day, and — the point of it — a *runtime* field
    /// added elsewhere is refused here without anyone remembering to extend a
    /// denylist. `_zc_scope`, `_zc_scope_trusted`, `_prx_scope_trusted` and
    /// the two approval-grant keys are not in the schema, so they are not in
    /// this set, so a member entry cannot set them.
    ///
    /// A schema that stopped exposing `properties` would yield the empty set
    /// and refuse every entry: the failure direction is refusal, not silent
    /// acceptance.
    fn batch_member_overridable_keys() -> &'static BTreeSet<String> {
        static KEYS: OnceLock<BTreeSet<String>> = OnceLock::new();
        KEYS.get_or_init(|| {
            Self::batch_member_entry_schema()
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .map(|properties| properties.keys().cloned().collect())
                .unwrap_or_default()
        })
    }

    /// Merge one batch entry over the batch-level arguments.
    ///
    /// The batch-level object carries the trusted per-turn scope and the
    /// approval grant for this tool call, so members inherit them by
    /// construction. Inheritance is only half the story, and the half that
    /// was reasoned about here before: the entry is written by the model, so
    /// any key it names *overwrites* what was inherited. `_zc_scope_trusted`
    /// is the single boolean [`parse_spawn_scope`] consults, and `_zc_scope`
    /// beside it names the sender, channel and chat id the sub-agent then
    /// speaks as — an entry allowed to set those picks its own identity and
    /// its own recipient, on a channel this turn never came from.
    ///
    /// So the merge is a whitelist, not a denylist: an entry may set only the
    /// per-member parameters the `spawn` schema declares, and any other key is
    /// an **error** rather than a silent drop. Dropping it would let a forged
    /// scope fail quietly and look like an ordinary spawn; refusing it puts
    /// the attempt in the caller's `rejected` list where it can be seen. The
    /// message names the offending key and never its value, which is
    /// model-authored content.
    ///
    /// `action`/`tasks` are dropped because they describe the batch, not a
    /// member, and every remaining batch-level key acts as a default that the
    /// member entry may override.
    fn batch_member_args(
        batch_args: &serde_json::Value,
        entry: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let mut member = batch_args
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("sessions_spawn arguments must be a JSON object"))?;
        member.remove("action");
        member.remove("tasks");
        match entry {
            // A bare string is the common case ("run these three tasks") and
            // means exactly `{"task": "..."}`.
            serde_json::Value::String(task) => {
                member.insert("task".to_string(), json!(task));
            }
            serde_json::Value::Object(fields) => {
                let overridable = Self::batch_member_overridable_keys();
                for (key, value) in fields {
                    // MUTATION GUARD: delete this refusal and a member entry
                    // carrying `_zc_scope_trusted`/`_zc_scope` overwrites the
                    // batch's real scope, choosing its own sender, channel and
                    // recipient.
                    if !overridable.contains(key) {
                        anyhow::bail!(
                            "entry key '{key}' is not a per-member parameter of 'spawn'; a batch entry may only set: {}",
                            overridable.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
                        );
                    }
                    member.insert(key.clone(), value.clone());
                }
            }
            other => anyhow::bail!("each entry of 'tasks' must be a string or an object, got: {other}"),
        }
        Ok(serde_json::Value::Object(member))
    }

    /// Fan out: start every task in `tasks` and label them with one batch id.
    ///
    /// Every member starts immediately — there is no concurrency cap, no
    /// staging, and no queue. Each member goes through [`Self::execute_spawn`],
    /// so it is registered and gated exactly as a standalone `spawn` would be;
    /// the only differences are the `batch_id` on its row and that it does not
    /// announce its own result (see `announce_result` there) — the fan-out's
    /// account of itself is the one summary its caller sends after `join`.
    ///
    /// A member that cannot be started is reported in `rejected` rather than
    /// aborting the fan-out: the caller asked for N independent tasks, and one
    /// unusable agent name is not a reason to withhold the other N-1.
    async fn execute_spawn_batch(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let Some(entries) = args.get("tasks").and_then(|value| value.as_array()) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Missing 'tasks' array for spawn_batch action".into()),
            });
        };
        if entries.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'tasks' must contain at least one task for spawn_batch action".into()),
            });
        }

        let batch_caller = crate::tools::execution::current_child_call_context();
        if let Some(caller) = &batch_caller {
            caller.verify_tool("sessions_spawn")?;
        }
        let batch_owner = batch_caller
            .as_ref()
            .map(|caller| caller.context().envelope.resolved_owner_id())
            .or_else(|| parse_spawn_scope(args).and_then(|scope| scope.owner_id));
        let batch_id = format!("batch-{}", Uuid::new_v4());
        let mut member_args = Vec::with_capacity(entries.len());
        let mut rejected = Vec::new();
        for entry in entries {
            match Self::batch_member_args(args, entry) {
                Ok(built) => member_args.push((Uuid::new_v4().to_string(), built)),
                Err(error) => rejected.push(json!({"task": entry, "error": error.to_string()})),
            }
        }

        let prepared = if let Some(grant) = crate::runtime::child_source::outgoing_grant() {
            let fabric = grant.source_fabric().clone();
            Some(batch_journal::PreparedBatch::prepare(fabric, &grant, &batch_id, &member_args).await?)
        } else if let Some(caller) = crate::tools::execution::current_child_call_context() {
            caller.verify_tool("sessions_spawn")?;
            if let Some(publication) = caller.context().ordinary_child_publication.as_ref() {
                batch_journal::PreparedBatch::prepare_ordinary(publication, caller.context(), &batch_id, &member_args)
                    .await?
            } else {
                None
            }
        } else {
            None
        };

        // Launched together rather than one after another: `execute_spawn`
        // awaits (memory events, config hashing for process mode) before it
        // returns, and a caller that asked for a fan-out should not pay for
        // those serially. All of these futures are polled on *this* task, which
        // is what keeps `child_beat`'s task-local parent link intact.
        let outcomes = futures_util::future::join_all(member_args.iter().map(|(run_id, member)| {
            boxed_spawn_field(|| self.execute_spawn_with_id(member, Some(&batch_id), Some(run_id)))
        }))
        .await;

        let mut dispatched = Vec::with_capacity(outcomes.len());
        for ((reserved, _), outcome) in member_args.iter().zip(&outcomes) {
            dispatched.push(match outcome {
                Ok(outcome) => match &outcome.run_id {
                    Some(actual) => {
                        anyhow::ensure!(actual == reserved, "batch member run differs from reserved identity");
                        batch_journal::DispatchState::Started
                    }
                    None => batch_journal::DispatchState::Rejected,
                },
                // `execute_spawn_with_id` registers the run immediately before
                // its infallible driver launch. Every returned error therefore
                // means no member was dispatched and can be recorded as a
                // visible rejection instead of an unresolvable unknown.
                Err(_) => batch_journal::DispatchState::Rejected,
            });
        }
        let journal_required = prepared.is_some();
        let journal_failed = if let Some(prepared) = prepared {
            prepared.finish(dispatched).await.is_err()
        } else {
            false
        };
        let mut spawned = Vec::new();
        let mut roster = Vec::new();
        for ((_, member), outcome) in member_args.iter().zip(outcomes) {
            let task = member.get("task").cloned().unwrap_or(serde_json::Value::Null);
            match outcome {
                Ok(outcome) => match outcome.run_id {
                    Some(run_id) => {
                        // The roster is written from the same value the caller
                        // is shown, before anything can retire the row: this is
                        // the batch's membership, fixed at launch.
                        roster.push(BatchMember {
                            run_id: run_id.clone(),
                            task: task.as_str().map_or_else(|| task.to_string(), str::to_string),
                        });
                        spawned.push(json!({"run_id": run_id, "task": task}));
                    }
                    None => rejected.push(json!({
                        "task": task,
                        "error": outcome.result.error.unwrap_or_else(|| "sub-agent was not started".to_string()),
                    })),
                },
                Err(error) => rejected.push(json!({"task": task, "error": error.to_string()})),
            }
        }
        if !roster.is_empty() {
            let mut rosters = self.batch_rosters.write().await;
            while rosters.len() >= MAX_TRACKED_BATCH_ROSTERS {
                rosters.remove(0);
            }
            rosters.push(BatchRoster {
                batch_id: batch_id.clone(),
                owner_id: batch_owner,
                members: roster,
            });
        }

        let started = spawned.len();
        let mut payload = json!({
            "batch_id": batch_id,
            "requested": entries.len(),
            "started": started,
            "spawned": spawned,
            "rejected": rejected,
        });
        if journal_required {
            payload
                .as_object_mut()
                .context("batch result must be an object")?
                .insert("recovery_journal_confirmed".to_owned(), json!(!journal_failed));
        }
        Ok(ToolResult {
            success: started > 0 && !journal_failed,
            output: serde_json::to_string_pretty(&payload)?,
            error: if journal_failed {
                Some(format!(
                    "batch {batch_id} dispatched, but durable dispatch confirmation is unavailable; use this batch ID to reconcile before retrying"
                ))
            } else {
                (started == 0).then(|| format!("no sub-agent in batch {batch_id} was confirmed started"))
            },
        })
    }

    /// Converge: block until every member of `batch_id` has finished, then
    /// report all of them.
    ///
    /// Partial success is data, not a tool failure — a batch where one member
    /// was killed still returns `success: true` with that member in `killed`,
    /// because the caller's next decision depends on seeing all four buckets at
    /// once. Nothing is hidden: [`join_summary`] accounts for every member
    /// exactly once and `total` is the sum of the buckets.
    async fn execute_join(
        &self,
        batch_id: &str,
        owner_scope: &sessions_read_model::RunOwnerScope,
    ) -> anyhow::Result<ToolResult> {
        let caller = crate::tools::execution::current_child_call_context();
        if let Some(caller) = &caller {
            caller.verify_tool("sessions_spawn")?;
        }
        let publication = caller
            .as_ref()
            .and_then(|caller| caller.context().ordinary_child_publication.as_ref());
        if let (Some(publication), Some(caller)) = (publication, caller.as_ref()) {
            publication.verify_caller(caller.context())?;
        }
        let recording_enabled = publication.map_or(self.event_recording.enabled, |publication| {
            publication
                .generation()
                .effective
                .memory
                .event_recording_config()
                .enabled
        });
        let source_fabric = publication
            .map(|publication| publication.source_fabric())
            .or_else(|| self.memory_fabric());
        let recorded = if recording_enabled {
            if let Some(fabric) = source_fabric {
                batch_journal::read(
                    &fabric,
                    std::path::Path::new(fabric.workspace_id()),
                    batch_id,
                    owner_scope,
                    None,
                )
                .await?
            } else {
                None
            }
        } else {
            None
        };
        let members = if let Some(roster) = recorded {
            roster.members
        } else {
            self.batch_members(batch_id, owner_scope).await
        };
        if members.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown batch '{batch_id}': no sub-agent run in this session carries that batch id."
                )),
            });
        }

        let settled = join_batch_members(&self.active_runs, &members).await;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&join_summary(batch_id, &settled))?,
            error: None,
        })
    }

    /// The identity behind a chat-assignment call, or the refusal that replaces
    /// it.
    ///
    /// The only source is the `_zc_*` block the runtime writes immediately
    /// before a tool executes, and an absent or untrusted one is a hard refusal
    /// rather than a fallback: an anonymous principal is something a later rule
    /// could match, and the operator plane is the *most* privileged principal
    /// there is. Degrading to either would turn "no identity" into a
    /// permission. The refusal names nobody, because there is nobody to name.
    fn assignment_principal(args: &serde_json::Value) -> Result<AssignPrincipal, ToolResult> {
        AssignPrincipal::from_trusted_scope(args).ok_or_else(|| ToolResult {
            success: false,
            output: String::new(),
            error: Some(
                "This call carries no trusted caller scope, so there is no identity to authorize a \
                 chat-session assignment against. Assignment is refused."
                    .to_string(),
            ),
        })
    }

    /// `action='chat_sessions'` — the live `prx chat` sessions *this caller*
    /// may hand work to.
    ///
    /// Filtered rather than complete on purpose. `chat_sessions::assign`
    /// authorizes before it reports a session unknown, precisely so a
    /// correspondent who may not assign anywhere cannot use it to learn which
    /// sessions exist; an unfiltered listing on the same principal's tool
    /// surface would hand back exactly that. A caller with no grants sees an
    /// empty list, which is the same thing it would learn by trying.
    fn execute_chat_sessions(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let principal = match Self::assignment_principal(args) {
            Ok(principal) => principal,
            Err(refusal) => return Ok(refusal),
        };
        let sessions = crate::runtime::chat_sessions::list()
            .into_iter()
            .filter(|session| {
                chat_assignment::may_assign(&self.security, &principal, &session.label, &session.session_id)
            })
            .map(|session| {
                json!({
                    "session_id": session.session_id,
                    "label": session.label,
                    "liveness": session.liveness.as_str(),
                    "last_poll_age_secs": session.last_poll_age_secs,
                    "queued": session.queued,
                    "delivered": session.delivered,
                    "accepted": session.accepted,
                    "registered_at_unix_ms": session.registered_at_unix_ms,
                })
            })
            .collect::<Vec<_>>();
        let note = if sessions.is_empty() {
            "No chat session is both registered and assignable by you. Assignment is default-deny: \
             an operator has to name the origin in `autonomy.scopes.assign_owners` or give it an \
             `assign_allow` entry."
        } else {
            "`liveness` is evidence, not a clock: 'silent' means the session has not pulled its \
             mailbox recently and may be busy or gone. Nothing is evicted for it."
        };
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({"sessions": sessions, "note": note}))?,
            error: None,
        })
    }

    /// A mailbox refusal, plus the one thing that keeps an unknown id from
    /// costing a round trip.
    ///
    /// Only [`ChatSessionError::UnknownSession`] is enriched. A model that gets
    /// "no session with that id" answers by guessing another id — the real-machine
    /// run produced `prx-agent-chat-6914` out of thin air — so the refusal that
    /// says the id is wrong is exactly where the right ones belong.
    ///
    /// **Every other refusal is passed through untouched, and that is a security
    /// decision, not an omission.** `NotAuthorized` is raised for a principal that
    /// may not assign here; attaching a listing to it would turn the one refusal
    /// that is supposed to reveal nothing into an enumeration oracle. The hint
    /// itself is filtered per session by the same [`chat_assignment::may_assign`]
    /// the `chat_sessions` listing uses, and is omitted entirely when the caller
    /// may assign nowhere.
    fn assignment_refusal(
        security: &SecurityPolicy,
        principal: &AssignPrincipal,
        error: &crate::runtime::chat_sessions::ChatSessionError,
    ) -> String {
        let hint = matches!(error, crate::runtime::chat_sessions::ChatSessionError::UnknownSession)
            .then(|| chat_assignment::assignable_sessions_hint(security, principal))
            .flatten();
        hint.map_or_else(
            || error.to_string(),
            |hint| format!("{error}. Sessions you may assign to right now: {hint}"),
        )
    }

    /// `action='chat_assign'` — hand a task to a live `prx chat` session and
    /// arrange for its answer to come back here.
    ///
    /// In-process rather than over the daemon's own HTTP API, and that is the
    /// substance of the action rather than an optimisation: an HTTP request
    /// carries no trustworthy caller identity, so the same call made over the
    /// loopback API would be authorized as the operator plane — the most
    /// privileged principal there is — no matter who typed the message. The
    /// only identity worth anything here is the one the runtime injected, and it
    /// exists only inside this process.
    ///
    /// The call returns as soon as the mailbox accepts the task. It does not
    /// wait: a chat session has no deadline, so waiting here would park the
    /// correspondent's turn for as long as the other end takes. The relay
    /// started below is what closes the loop, and it is a work-registry row of
    /// its own so an operator can end it.
    async fn execute_chat_assign(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let principal = match Self::assignment_principal(args) {
            Ok(principal) => principal,
            Err(refusal) => return Ok(refusal),
        };
        let session_id = args
            .get("session_id")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'session_id' parameter for chat_assign action"))?;
        let task = args
            .get("task")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'task' parameter for chat_assign action"))?;
        let disposition = match args.get("disposition").and_then(|value| value.as_str()) {
            // Unknown values are rejected rather than folded into the default:
            // a caller that asked to interrupt and silently got a queue would
            // have its intent evaporate with nothing said.
            Some(raw) => Disposition::parse(raw)
                .ok_or_else(|| anyhow::anyhow!("Unknown disposition '{raw}': use 'queue', 'steer' or 'interrupt'"))?,
            None => Disposition::Queue,
        };

        let receipt =
            match crate::runtime::chat_sessions::assign(&self.security, &principal, session_id, task, disposition) {
                Ok(receipt) => receipt,
                // Already redacted at the source: the mailbox's refusals name the
                // parties by `op_id` fingerprint, so this is safe to hand to a model
                // that will read it back out loud.
                Err(error) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(Self::assignment_refusal(&self.security, &principal, &error)),
                    });
                }
            };

        // Where the answer goes is settled here, from the assigning turn's own
        // trusted scope, and never from the tool arguments — unlike `spawn`,
        // which accepts an explicit `recipient` and gates it. There is no reason
        // for one correspondent to assign work whose result lands in someone
        // else's conversation, so the capability simply is not offered.
        let scope = parse_spawn_scope(args);
        let recipient = scope.as_ref().map_or_else(
            || match &principal {
                // No conversation on the scope: answer the correspondent
                // directly, which is the only address left that is still theirs.
                AssignPrincipal::Correspondent { sender, .. } => sender.clone(),
                AssignPrincipal::OperatorPlane => crate::runtime::chat_sessions::OPERATOR_PLANE_CHANNEL.to_string(),
            },
            |scope| scope.chat_id.clone(),
        );
        let channel = self.resolve_announce_channel(Some(principal.channel())).await;
        let relay_work_id = chat_assignment::spawn_return_trip(ReturnTrip {
            assignment_id: receipt.assignment_id.clone(),
            session_ref: receipt.session_ref.clone(),
            origin: principal,
            recipient,
            channel,
            security: Arc::clone(&self.security),
        });

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "assignment_id": receipt.assignment_id,
                "session_id": receipt.session_id,
                "session_ref": receipt.session_ref,
                "disposition": receipt.disposition.as_str(),
                "queued_ahead": receipt.queued_ahead,
                "work_id": receipt.work_id.to_string(),
                "relay_work_id": relay_work_id.to_string(),
                "delivery": "Accepted into the session's mailbox. Its result will be relayed to this \
                             conversation on its own when the session reports it — say that it was \
                             handed over and stop; do not wait for it here.",
            }))?,
            error: None,
        })
    }

    /// Who this batch launched.
    ///
    /// The roster is preferred over the registry because it is the only source
    /// that cannot lose a member: `active_runs` is pruned by the chat session
    /// reaper, so scanning it answers "which members are still on file", and a
    /// join that took that for the membership silently shrank its own `total`.
    ///
    /// The scan remains as the fallback for the two cases the roster cannot
    /// cover — a batch launched before this tool instance existed, and one
    /// evicted by [`MAX_TRACKED_BATCH_ROSTERS`] — where it is exactly as good,
    /// and exactly as lossy, as it was before.
    async fn batch_members(
        &self,
        batch_id: &str,
        owner_scope: &sessions_read_model::RunOwnerScope,
    ) -> Vec<BatchMember> {
        let recorded = {
            let rosters = self.batch_rosters.read().await;
            rosters
                .iter()
                .find(|roster| roster.batch_id == batch_id)
                .map(|roster| (owner_scope.admits(roster.owner_id.as_deref()), roster.members.clone()))
        };
        if let Some((admitted, members)) = recorded {
            // Another owner's batch is answered as an unknown one.
            return if admitted { members } else { Vec::new() };
        }
        let runs = self.active_runs.read().await;
        runs.iter()
            .filter(|run| run.batch_id.as_deref() == Some(batch_id) && owner_scope.admits(run.owner_id.as_deref()))
            .map(|run| BatchMember {
                run_id: run.id.clone(),
                task: run.task.clone(),
            })
            .collect()
    }

    /// Launch exactly one sub-agent run and return as soon as it is registered
    /// and its driver task is running.
    ///
    /// Split out of [`Tool::execute`] so `spawn_batch` starts its members
    /// through the *same* path a single `spawn` takes — same approval gate,
    /// same registry row, same announce routing — rather than a parallel
    /// implementation that would drift. `batch_id` is the only thing that
    /// changes behaviour, and only by flipping the default of the `announce`
    /// argument.
    ///
    /// Must be called from the task that owns the requesting turn:
    /// [`crate::agent::idle::child_beat`] reads a task-local, so a run started
    /// from a detached task would silently lose the parent link that keeps a
    /// joining caller alive.
    ///
    /// `batch_id` stamps the run's registry row so `join`, `sessions_list`, and
    /// the control plane can address the whole fan-out as one unit.
    async fn execute_spawn(&self, args: &serde_json::Value, batch_id: Option<&str>) -> anyhow::Result<SpawnOutcome> {
        boxed_spawn_field(|| self.execute_spawn_with_id(args, batch_id, None)).await
    }

    async fn execute_spawn_with_id(
        &self,
        args: &serde_json::Value,
        batch_id: Option<&str>,
        reserved_run_id: Option<&str>,
    ) -> anyhow::Result<SpawnOutcome> {
        let operator_source = crate::runtime::operator_spawn::claim(args)?;
        if let Some(source) = &operator_source {
            source.verify_live()?;
        }
        let caller = crate::tools::execution::current_child_call_context();
        if let Some(caller) = &caller {
            caller.verify_tool("sessions_spawn")?;
        }
        let ordinary_publication = caller
            .as_ref()
            .and_then(|caller| caller.context().ordinary_child_publication.as_ref());
        if let (Some(publication), Some(caller)) = (ordinary_publication, caller.as_ref()) {
            publication.verify_caller(caller.context())?;
        }
        let child_grant = crate::runtime::child_source::outgoing_grant();
        anyhow::ensure!(
            child_grant.is_some() || ordinary_publication.is_some() || operator_source.is_some(),
            "session spawning requires an admitted parent or operator source"
        );
        anyhow::ensure!(
            operator_source.is_none()
                || (caller.is_none() && child_grant.is_none() && current_spawn_execution_context().is_none()),
            "operator spawn cannot inherit model or child authority"
        );
        let child_model_config = child_grant
            .as_ref()
            .map(|grant| grant.generation().effective.as_ref())
            .or_else(|| ordinary_publication.map(|publication| publication.generation().effective.as_ref()))
            .or_else(|| {
                operator_source
                    .as_ref()
                    .map(|source| source.generation().effective.as_ref())
            })
            .context("spawn admitted configuration missing")?;
        let launch_security = child_grant
            .as_ref()
            .map(|grant| grant.security())
            .or_else(|| ordinary_publication.map(|publication| publication.security()))
            .or_else(|| operator_source.as_ref().map(|source| source.security()))
            .ok_or_else(|| anyhow::anyhow!("spawn admitted security policy missing"))?;
        let provider_options = providers::provider_runtime_options_from_config(child_model_config);
        let provider_options = &provider_options;
        let provider_reliability = &child_model_config.reliability;
        let spawn_config = &child_model_config.sessions_spawn;
        let agents = &child_model_config.agents;
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("Missing 'task' parameter"))?;

        if task.is_empty() {
            return Ok(SpawnOutcome::rejected("'task' parameter must not be empty".into()));
        }

        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(spawn_config.default_mode.as_str())
            .to_ascii_lowercase();
        if mode != "task" && mode != "process" {
            return Ok(SpawnOutcome::rejected(format!(
                "Invalid 'mode' value '{mode}'. Expected 'task' or 'process'."
            )));
        }
        let process_memory_strategy = if mode == "process" {
            normalize_process_memory_strategy(&spawn_config.process_memory_strategy)?.to_string()
        } else {
            String::new()
        };

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
        let spawn_scope = if let Some(source) = &operator_source {
            use crate::runtime::turn_source::TurnSourceProof;
            let proof = source.proof();
            Some(SpawnScope {
                sender: proof.policy_sender().to_owned(),
                channel: proof.policy_channel().to_owned(),
                chat_type: proof.policy_chat_type().to_owned(),
                chat_id: proof
                    .memory_scope()
                    .chat_id
                    .clone()
                    .context("operator spawn recipient missing")?,
                owner_id: Some(proof.owner_id().to_owned()),
                owner_isolated: proof.owner_isolated(),
                topic_id: None,
                parent_task_id: Some(proof.run_id().to_owned()),
                source_message_event_id: proof.source_event_id().map(str::to_owned),
                config_generation_id: Some(proof.config_generation_id()),
                config_source_revision: Some(proof.config_source_revision().to_owned()),
            })
        } else {
            parse_spawn_scope(args)
        };
        let parent_exec_ctx = current_spawn_execution_context();
        // D8-4: a turn-root context represents the turn itself (zero spawn
        // nesting so far), so its first child reports depth 0 — identical to the
        // no-context case. A real spawn-run context's child reports +1. The depth
        // is lineage only: nothing rejects a spawn for being deeply nested.
        let spawn_depth = parent_exec_ctx.as_ref().map_or(0, |ctx| {
            if ctx.is_turn_root {
                ctx.spawn_depth
            } else {
                ctx.spawn_depth.saturating_add(1)
            }
        });
        let parent_run_id = parent_exec_ctx.as_ref().map(|ctx| ctx.run_id.clone()).or_else(|| {
            use crate::runtime::turn_source::TurnSourceProof;
            operator_source
                .as_ref()
                .map(|source| source.proof().run_id().to_owned())
        });
        // Whatever the parent turn's surface hides stays hidden for everything
        // this spawn produces, however deeply it nests.
        let inherited_channel_exclusions = parent_exec_ctx
            .as_ref()
            .map(|ctx| ctx.channel_excluded_tools.clone())
            .unwrap_or_default();
        let session_scope_key = operator_source.as_ref().map_or_else(
            || spawn_session_scope_key(parent_exec_ctx.as_ref(), spawn_scope.as_ref()),
            |source| {
                use crate::runtime::turn_source::TurnSourceProof;
                source.proof().session_key().to_owned()
            },
        );

        // This runtime caps neither how many sub-agents run at once, how deeply
        // they nest, nor how many one session may own: an operator sees every
        // live run in the runtime registry and ends it with `prx tasks kill`,
        // which is what makes an uncapped fan-out answerable rather than
        // unstoppable. Counting the live runs on the way in keeps that fan-out
        // visible in the log while it grows, instead of only after it hurts.
        let live_runs = running_run_count(&self.active_runs.read().await);
        tracing::debug!(
            live_runs,
            spawn_depth,
            session_scope_key = %session_scope_key,
            "sessions_spawn accepted"
        );

        // FIX-P0-37: spawning a child session creates a new process that
        // consumes resources and carries a potential sandbox-escape surface,
        // so it is a Medium-risk side effect (requires an approval grant under
        // supervised autonomy; denied outright under read-only) rather than Low.
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), args);
        if let Err(error) = SideEffectGate::new(launch_security.as_ref()).authorize_resource_operation(
            self.name(),
            "sessions_spawn:spawn",
            ResourceRiskLevel::Medium,
            approval_grant.as_ref(),
        ) {
            return Ok(SpawnOutcome::rejected(error));
        }

        let run_id = reserved_run_id.map_or_else(|| Uuid::new_v4().to_string(), str::to_owned);
        // A native issued child owns a distinct session before its durable input
        // is issued. Ordinary and operator spawning retain their shared scope.
        let session_scope_key = child_grant.as_ref().map_or(session_scope_key, |grant| {
            format!("{}:child:{run_id}", grant.parent().session_key())
        });

        let selected_agent = match agent_name.as_deref() {
            Some(name) => match agents.get(name) {
                Some(cfg) => {
                    if !cfg.spawn_enabled.unwrap_or(true) {
                        return Ok(SpawnOutcome::rejected(format!(
                            "Agent '{name}' is not allowed for sessions_spawn (spawn_enabled=false)."
                        )));
                    }
                    let mut selected = cfg.clone();
                    {
                        let config = child_model_config;
                        selected.api_key = providers::credentials::configured_key(
                            &providers::credentials::ConfigCredentialSource::new(config),
                            &cfg.provider,
                            cfg.api_key.as_deref(),
                            cfg.credential_ref.as_deref(),
                        );
                    }
                    Some((name.to_string(), selected))
                }
                None => {
                    let mut available = agents
                        .iter()
                        .filter(|(_, cfg)| cfg.spawn_enabled.unwrap_or(true))
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>();
                    available.sort_unstable();
                    return Ok(SpawnOutcome::rejected(format!(
                        "Unknown agent '{name}'. Available agents: {}",
                        if available.is_empty() {
                            "(none configured)".to_string()
                        } else {
                            available.join(", ")
                        }
                    )));
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
            None => match spawn_scope
                .as_ref()
                .map(|scope| scope.chat_id.trim())
                .filter(|chat_id| !chat_id.is_empty())
                .map(str::to_owned)
            {
                Some(chat_id) => Some(chat_id),
                None => self.default_recipient.read().await.clone(),
            },
        };
        // Channel name bound to the *originating* message (atomic with recipient).
        // `None` (no trusted scope) falls back at announce time to the shared
        // active channel, preserving single-channel / legacy behaviour.
        let run_channel_name = spawn_scope.as_ref().map(|scope| scope.channel.clone());
        let operator_channel = if operator_source.is_some() {
            let channel = self.resolve_announce_channel(run_channel_name.as_deref()).await;
            anyhow::ensure!(
                Some(channel.name()) == run_channel_name.as_deref(),
                "operator spawn announcement channel differs from its source"
            );
            Some(channel)
        } else {
            None
        };

        // Whether this run posts its own result to the channel when it ends.
        //
        // Default by construction: a standalone `spawn` is fire-and-forget, so
        // its announcement is the only way its result is ever seen and it stays
        // on. A `spawn_batch` member's result is collected by `join`, so
        // announcing it as well would put the fan-out's raw intermediate output
        // on the channel *and* the parent's summary after it — N+1 messages for
        // one request. Which of those two a run is, is exactly `batch_id`.
        //
        // For a batch member this is not a default but a **property**: the
        // argument is not consulted at all. "One request, one message" is the
        // whole point of `spawn_batch` + `join`, and a default the model can
        // flip is not that promise — it is a suggestion. `announce` is
        // therefore no longer a per-member parameter of the entry schema
        // either, so an entry that names it is refused where the caller can
        // see it (see `batch_member_overridable_keys`), rather than silently
        // ignored here. A run that really should report for itself is a plain
        // `spawn`, which is unchanged.
        //
        // This deliberately changes nothing else about the announce wiring: the
        // run still captures `channel_name` + `recipient` atomically from the
        // launching message's scope (below), so a later kill notification still
        // routes to the right channel.
        //
        // MUTATION GUARD: drop the `batch_id.is_none() &&` and a batch launched
        // with `announce: true` at the top level delivers one message per member
        // on top of the parent's summary.
        let announce_result = batch_id.is_none()
            && args
                .get("announce")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);

        anyhow::ensure!(
            !child_model_config.causal_tree.enabled || child_grant.is_some() || operator_source.is_some(),
            "decision-enabled spawning requires a child grant or typed operator source"
        );
        let memory_fabric = child_grant
            .as_ref()
            .map(|grant| grant.source_fabric().clone())
            .or_else(|| ordinary_publication.map(|publication| publication.source_fabric()))
            .or_else(|| operator_source.as_ref().map(|source| source.fabric().clone()))
            .context("spawn admitted memory fabric missing")?;
        let process_memory_backend = memory_fabric.memory().name().to_owned();
        let child_model_catalog = child_grant
            .as_ref()
            .map(|grant| grant.provider_catalog())
            .or_else(|| ordinary_publication.map(|publication| publication.provider_catalog()))
            .or_else(|| operator_source.as_ref().map(|source| source.catalog()))
            .context("spawn admitted provider catalog missing")?;
        // A per-call provider is accepted only among the providers the operator
        // configured: a prompt must not be able to direct this run, or a
        // credential, to a host the operator never chose.
        if let Some(requested) = provider_override.as_deref() {
            let configured = providers::ConfiguredProviders::from_config(child_model_config);
            if let Err(error) = configured.ensure_permits(requested) {
                return Ok(SpawnOutcome::rejected(error.to_string()));
            }
        }
        let inherited_model = if let Some(grant) = &child_grant {
            grant.model_source()?
        } else if let Some(publication) = ordinary_publication {
            publication.model_source()
        } else {
            use crate::runtime::turn_source::TurnSourceProof;
            let source = operator_source.as_ref().context("spawn model source missing")?;
            (
                source.proof().provider_name(),
                source.proof().model_name(),
                source.resolved().credential().reference(),
            )
        };
        let inherits_default_route =
            provider_override.is_none() && model_override.is_none() && selected_agent.is_none();
        let mut resolved_provider_name = provider_override.unwrap_or_else(|| {
            selected_agent
                .as_ref()
                .map(|(_, cfg)| cfg.provider.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| inherited_model.0.to_owned())
        });
        let mut resolved_model = model_override.unwrap_or_else(|| {
            selected_agent
                .as_ref()
                .map(|(_, cfg)| cfg.model.trim().to_string())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| inherited_model.1.to_owned())
        });
        // Retain the request spelling: a resolved hint alone cannot prove which
        // route and credential source selected the actual child provider.
        let mut child_requested_model = (child_grant.is_some() || ordinary_publication.is_some())
            .then(|| (resolved_provider_name.clone(), resolved_model.clone()));
        let selected_route_hint = resolved_model.trim().strip_prefix("hint:").map(str::to_string);
        let route_source_provider = selected_route_hint.as_ref().map(|_| resolved_provider_name.clone());
        // Resolve the child's model for chat before any run or provider
        // exists: a model that cannot chat is refused and nothing is sent. The
        // gateway's own provider was resolved when it was built; a provider
        // this build cannot name fails construction with its own message.
        if providers::ProviderId::of(&resolved_provider_name).is_resolvable()
            && let Err(refusal) = providers::selection::check_model(
                child_model_catalog,
                &resolved_provider_name,
                &resolved_model,
                &providers::catalog::Requirement::chat(),
                providers::catalog::UnknownPolicy::Invocation,
            )
        {
            return Ok(SpawnOutcome::rejected(format!("Cannot spawn this run: {refusal}")));
        }
        let child_credential = operator_source.as_ref().map_or_else(
            || {
                let config = child_model_config;
                let credential = if inherits_default_route {
                    (providers::ProviderId::of(inherited_model.0) == providers::ProviderId::of(&resolved_provider_name))
                        .then(|| inherited_model.2.clone())
                } else {
                    None
                };
                Some(credential.unwrap_or_else(|| {
                    selected_agent
                        .as_ref()
                        .filter(|(_, agent)| {
                            providers::ProviderId::of(&agent.provider)
                                == providers::ProviderId::of(&resolved_provider_name)
                        })
                        .map_or_else(
                            || providers::selection::credential_for(config, &resolved_provider_name),
                            |(name, agent)| {
                                agent.credential_ref.as_deref().map_or_else(
                                    || {
                                        if agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
                                            providers::credentials::CredentialRef::LegacyInline(
                                                providers::credentials::LegacySlot::Agent(name.clone()),
                                            )
                                        } else {
                                            providers::selection::credential_for(config, &resolved_provider_name)
                                        }
                                    },
                                    |id| providers::credentials::CredentialRef::Profile { id: id.to_string() },
                                )
                            },
                        )
                }))
            },
            |source| Some(source.resolved().credential().reference().clone()),
        );
        // An admitted ordinary child also needs an exact PMM/credential
        // selection for direct model names, rather than reusing the gateway.
        let selected_hint = if let Some(source) = &operator_source {
            source.verify_live()?;
            let resolved = source.resolved().clone();
            resolved_provider_name = resolved.provider_label().to_owned();
            resolved_model = resolved.model().to_owned();
            Some(resolved)
        } else if ordinary_publication.is_some() || resolved_model.trim().starts_with("hint:") {
            let config = child_model_config;
            let credential = child_credential.clone().context("child credential selection missing")?;
            let resolved = match providers::selection::check_with(
                config,
                child_model_catalog,
                &resolved_provider_name,
                &resolved_model,
                credential,
                &providers::catalog::Requirement::chat(),
                providers::catalog::UnknownPolicy::Invocation,
            ) {
                Ok(resolved) => resolved,
                Err(refusal) => return Ok(SpawnOutcome::rejected(format!("Cannot spawn this run: {refusal}"))),
            };
            resolved_provider_name = resolved.provider_label().to_string();
            resolved_model = resolved.model().to_string();
            Some(resolved)
        } else {
            None
        };
        if selected_hint.is_none()
            && let Some((_, agent)) = selected_agent.as_ref()
            && let Some(id) = agent.credential_ref.as_deref()
        {
            let config = child_model_config;
            let checked = match providers::selection::check_with(
                config,
                child_model_catalog,
                &resolved_provider_name,
                &resolved_model,
                providers::credentials::CredentialRef::Profile { id: id.to_string() },
                &providers::catalog::Requirement::chat(),
                providers::catalog::UnknownPolicy::Invocation,
            ) {
                Ok(checked) => checked,
                Err(refusal) => return Ok(SpawnOutcome::rejected(format!("Cannot spawn this run: {refusal}"))),
            };
            if checked.credential().api_key() != agent.api_key.as_deref() {
                return Ok(SpawnOutcome::rejected(
                    "Cannot spawn this run: agent credential is unavailable".to_string(),
                ));
            }
        }
        let route_credential = selected_hint
            .as_ref()
            .filter(|_| operator_source.as_ref().is_none_or(|source| source.route().is_some()))
            .map(|resolved| resolved.credential().reference().clone());
        let route_uses_root_connection = operator_source.as_ref().is_none_or(|source| source.route().is_some())
            && selected_hint.as_ref().is_some_and(|resolved| {
                matches!(
                    resolved.credential().reference(),
                    providers::credentials::CredentialRef::LegacyInline(providers::credentials::LegacySlot::Root)
                )
            });
        let direct_uses_root_connection = operator_source.as_ref().map_or_else(
            || {
                selected_hint.is_none()
                    && self.fallback_uses_root_connection
                    && providers::ProviderId::of(&resolved_provider_name)
                        == providers::ProviderId::of(&self.fallback_api_key_owner)
                    && selected_agent.as_ref().is_none_or(|(_, agent)| {
                        agent.credential_ref.is_none()
                            && agent.api_key.as_deref().is_none_or(|key| key.trim().is_empty())
                    })
            },
            |source| {
                source.route().is_none()
                    && matches!(
                        source.resolved().credential().reference(),
                        providers::credentials::CredentialRef::LegacyInline(providers::credentials::LegacySlot::Root)
                    )
            },
        );
        let resolved_compaction = crate::router::context::resolve_effective_compaction_config(
            &child_model_config.agent.compaction,
            &resolved_provider_name,
            &resolved_model,
            &child_model_config.router,
            &child_model_config.model_routes,
        );
        tracing::debug!(
            mode = mode.as_str(),
            provider = %resolved_compaction.shown_provider(),
            model = resolved_compaction.selected_model.as_str(),
            max_context_tokens = resolved_compaction.config.max_context_tokens,
            source = ?resolved_compaction.max_context_source,
            kernel_capped = resolved_compaction.kernel_capped,
            "sessions_spawn resolved child compaction window"
        );
        let resolved_temperature = selected_agent
            .as_ref()
            .and_then(|(_, cfg)| cfg.temperature)
            .unwrap_or(child_model_config.default_temperature);
        // A gateway key is only meaningful for the gateway provider, and a named
        // agent's key only for that agent's own provider. Passing either to a
        // different provider hands one vendor's secret to another (and, for a
        // model-chosen provider, to a host the model picked).
        let resolved_api_key = selected_hint.as_ref().map_or_else(
            || {
                resolve_child_provider_api_key(
                    selected_agent.as_ref().map(|(_, cfg)| cfg),
                    self.fallback_api_key.as_deref(),
                    &resolved_provider_name,
                    &self.fallback_api_key_owner,
                )
            },
            |resolved| resolved.credential().api_key().map(str::to_string),
        );
        let task_provider_plan =
            self.child_provider_plan(selected_agent.as_ref().map(|(_, cfg)| cfg), &resolved_provider_name);
        // A spelling alias of the already-configured gateway is the same
        // connection, credential, and catalog identity. Issue its ordinary
        // child proof under the configured spelling so the decision service
        // sees the catalog row it actually published. Route hints retain their
        // requested source spelling because that spelling selects the route.
        if matches!(&task_provider_plan, ChildProvider::Gateway)
            && selected_route_hint.is_none()
            && let Some((provider, _)) = child_requested_model.as_mut()
        {
            provider.clone_from(&self.provider_name);
        }
        // Bind ordinary input to the pinned selection before recording.
        // Tasks retain this provider instance; processes export its verified
        // selection and construct their own instance from signed snapshots.
        let ordinary_binding = ordinary_publication
            .map(|publication| -> anyhow::Result<_> {
                let (provider, model) = child_requested_model
                    .as_ref()
                    .context("ordinary task model request missing")?;
                Ok(Arc::new(
                    crate::runtime::child_source::ChildProviderBinding::build_ordinary(
                        &publication.generation().effective,
                        publication.provider_catalog(),
                        provider,
                        model,
                        child_credential.clone().context("ordinary task credential missing")?,
                        Some(publication),
                    )?,
                ))
            })
            .transpose()?;
        let event_workspace = memory_fabric.workspace_id();
        let mut spawn_scope_for_event = spawn_event_scope(
            event_workspace,
            &run_id,
            &session_scope_key,
            parent_run_id.as_deref(),
            agent_name.as_deref(),
            spawn_scope.as_ref(),
        );
        if let Some(grant) = child_grant.as_ref() {
            grant.bind_event_scope(&mut spawn_scope_for_event)?;
        }
        if let Some(binding) = &ordinary_binding {
            binding.bind_ordinary_input_scope(
                &binding.provider(),
                event_workspace,
                &mut spawn_scope_for_event,
                caller.as_ref(),
            )?;
        }
        if let Some(source) = &operator_source {
            source.bind_input_scope(&mut spawn_scope_for_event)?;
        }
        let mut run_lineage = spawn_lineage(&spawn_scope_for_event, parent_exec_ctx.as_ref(), spawn_scope.as_ref());
        if let Some(source) = &operator_source {
            use crate::runtime::turn_source::TurnSourceProof;
            run_lineage.parent_task_id = Some(source.proof().run_id().to_owned());
            run_lineage.source_message_event_id = source.proof().source_event_id().map(str::to_owned);
        }
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
        let tools = self.tools.get()?;
        if let Some(registry) = &tools {
            refresh_tools_for_explicit_allowlist(registry, &allowed_tools).await?;
        } else if !allowed_tools.is_empty() {
            anyhow::bail!("sessions_spawn tool registry is not initialized");
        }
        let process_config_source = if mode == "process" {
            Some(Self::resolved_process_config_source(
                provider_options,
                Some(child_model_config),
            )?)
        } else {
            None
        };
        let source_receipt = record_spawn_request_event(
            Some(&memory_fabric),
            spawn_scope_for_event.clone(),
            task,
            &mode,
            &resolved_provider_name,
            &resolved_model,
            &run_lineage,
        )
        .await?;

        if mode == "process" {
            let (process_config_dir, process_config_generation) =
                process_config_source.ok_or_else(|| anyhow::anyhow!("process config source was not resolved"))?;
            let process_provider_selection = if child_grant.is_some() || ordinary_publication.is_some() {
                let (provider, model) = child_requested_model
                    .as_ref()
                    .context("process child selection missing")?;
                Some(crate::session_worker::protocol::WorkerProviderSelection {
                    provider: provider.clone(),
                    model: model.clone(),
                    credential: child_credential.clone().context("process child credential missing")?,
                })
            } else {
                None
            };
            let process_source_launch = ordinary_publication
                .map(|publication| {
                    publication.capture_process_launch(
                        caller.as_ref().context("ordinary process caller missing")?,
                        ordinary_binding
                            .as_deref()
                            .context("ordinary process binding missing")?,
                        &spawn_scope_for_event,
                        task,
                        source_receipt.clone(),
                        process_provider_selection
                            .clone()
                            .context("ordinary process selection missing")?,
                        process_config_generation.clone(),
                    )
                })
                .transpose()?
                .map(crate::runtime::operator_spawn::process::ProcessSourceLaunch::Ordinary);
            let process_source_launch = match &operator_source {
                Some(source) => Some(crate::runtime::operator_spawn::process::ProcessSourceLaunch::Operator(
                    source.capture_process(
                        &spawn_scope_for_event,
                        task,
                        source_receipt.clone(),
                        process_config_generation.clone(),
                    )?,
                )),
                None => process_source_launch,
            };
            let temperature = resolved_temperature;
            let history_arc: Arc<RwLock<Vec<HistoryEntry>>> = Arc::new(RwLock::new(Vec::new()));
            let process_control = ProcessRunControl::new();
            // Process-mode steering: the sender lives in the registry exactly as
            // it does for task mode, so `sessions_send` / `sessions_spawn:steer`
            // reach an OS-process sub-agent through the same code path. The
            // receiver is handed to the child owner, which forwards each message
            // as a line-delimited control frame on the worker's stdin.
            let (process_steer_tx, process_steer_rx) = tokio::sync::mpsc::channel::<String>(STEER_CHANNEL_CAPACITY);
            // Minted here, on the *spawning* task, so the run's beat is parented
            // on the caller's: a parent legitimately blocked on a working child
            // must not look silent to its own hang detector. A beat minted
            // inside the spawned monitor would have no parent at all, because
            // task-locals do not cross `tokio::spawn`.
            let process_progress = crate::agent::idle::child_beat();

            {
                let mut runs = self.active_runs.write().await;
                runs.push(new_process_mode_run(ProcessModeRunSeed {
                    id: run_id.clone(),
                    task: task.to_string(),
                    lineage: &run_lineage,
                    recipient: recipient.clone(),
                    channel_name: run_channel_name.clone(),
                    process_control: process_control.clone(),
                    history: history_arc.clone(),
                    steer_tx: process_steer_tx.clone(),
                    parent_run_id: parent_run_id.clone(),
                    session_scope_key: session_scope_key.clone(),
                    spawn_depth,
                    progress: Arc::clone(&process_progress),
                    batch_id: batch_id.map(str::to_string),
                }));
            }

            let model = resolved_model;
            let provider_name = resolved_provider_name;
            let api_key = resolved_api_key;
            let route_hint = selected_route_hint;
            let workspace_root = child_model_config.workspace_dir.clone();
            let worker_workspace_root = spawn_config
                .worker_workspace_root
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| {
                    let path = std::path::PathBuf::from(value);
                    if path.is_absolute() {
                        path
                    } else {
                        workspace_root.join(path)
                    }
                })
                .unwrap_or_else(|| workspace_root.join("workers"));
            let active_runs = self.active_runs.clone();
            // Resolve the announce channel from the *per-turn* channel name bound
            // to this run (the launching message's scope), not the shared active
            // channel — so a concurrently-processed message cannot mis-route this
            // run's result. Falls back to the active channel when no scope name.
            let channel = match &operator_channel {
                Some(channel) => Arc::clone(channel),
                None => self.resolve_announce_channel(run_channel_name.as_deref()).await,
            };
            let keep_workspace = !spawn_config.cleanup_on_complete;
            let process_agent_id = selected_agent.as_ref().map(|(name, _)| name.clone());
            let identity_dir = selected_agent.as_ref().and_then(|(_, cfg)| {
                cfg.identity_dir
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            });
            // Process workers need the same explicit agent contract as task-mode
            // children.  Without carrying this prompt into the signed manifest,
            // a named agent's tool-use and completion instructions silently vanish
            // at the OS-process boundary.
            let process_system_prompt = selected_agent.as_ref().and_then(|(_, cfg)| {
                cfg.system_prompt
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            });
            let task_owned = task.to_string();
            let rid = run_id.clone();
            let process_scope = spawn_scope.clone();
            // Announcement gate inputs for the process-mode monitor. Same three
            // values the task-mode branch below captures, handed to the same
            // `deliver_or_withhold_notice`.
            let process_announce_security = Arc::clone(launch_security);
            let process_announce_origin = AnnounceOrigin::from_scope(spawn_scope.as_ref());
            let process_announce_history = history_arc.clone();
            let process_parent_run_id = parent_run_id.clone();
            let process_session_scope_key = session_scope_key.clone();
            let process_spawn_depth = spawn_depth;
            let process_channel_exclusions = inherited_channel_exclusions.clone();
            let process_compaction_config = resolved_compaction.config.clone();
            let process_event_recording = child_model_config.memory.event_recording_config();
            let process_execution_ctx = SpawnExecutionContext {
                operator_authority: None,
                ordinary_authority: None,
                incoming_grant: child_grant.clone(),
                provider_binding: None,
                terminal_usage: None,
                input_receipt: source_receipt.clone(),
                run_id: rid.clone(),
                session_scope_key: session_scope_key.clone(),
                spawn_depth,
                owner_id: run_lineage.owner_id.clone(),
                owner_isolated: run_lineage.owner_isolated,
                topic_id: run_lineage.topic_id.clone(),
                source_message_event_id: run_lineage.source_message_event_id.clone(),
                // A spawn-run context (this run may itself spawn): its children
                // compute spawn_depth + 1.
                is_turn_root: false,
                channel_excluded_tools: inherited_channel_exclusions.clone(),
            };
            let process_memory_fabric = Some(memory_fabric.clone());
            let process_result_scope = spawn_scope_for_event.clone();
            let process_lineage = run_lineage.clone();
            let process_cost_config = child_model_config.cost.clone();
            let monitor_process_control = process_control.clone();

            // Registry row for this run. The parent is captured *here*, on the
            // spawning task, because a freshly spawned task inherits no
            // task-locals. No abort handle is attached: this monitor is the sole
            // owner responsible for signalling and reaping the OS child, so
            // aborting it would strand the very process a kill is meant to end.
            // The row's cooperative token is how a kill reaches it instead —
            // see `bridge_registry_kill_to_process_control`, which asks that
            // same owner to stop. A cascade additionally signals the worker's
            // process group through the child row `run_sub_agent_process`
            // registers underneath.
            let sub_agent_parent = crate::runtime::registry::current_work_id();
            let sub_agent_label = process_agent_id
                .clone()
                .unwrap_or_else(|| "sub-agent (process)".to_string());
            let mut sub_agent_work =
                register_killable_process_run(&sub_agent_label, &rid, sub_agent_parent, batch_id, &process_control);
            // Publish this run's steering channel on its registry row so the
            // control plane can address it by run id from another entry point.
            // Same sender, same bounded queue as `sessions_send` — only the
            // lookup is new; see `registry::attach_steer_sender`.
            crate::runtime::registry::attach_steer_sender(sub_agent_work.id(), process_steer_tx.clone());
            sub_agent_work.require_cleanup_acknowledgement();
            let process_work_id = sub_agent_work.id();
            let process_recovery = child_grant.as_ref().map(|grant| {
                Arc::new(process_terminal::ProcessRecovery::new(
                    process_memory_fabric.clone(),
                    Arc::clone(grant),
                    source_receipt.clone(),
                    batch_id.map(str::to_owned),
                ))
            });
            let is_ordinary_process = process_source_launch
                .as_ref()
                .is_some_and(|launch| launch.is_ordinary());
            let ordinary_recovery = if is_ordinary_process {
                Some(process_terminal::ordinary::OrdinaryProcessRecovery::new(
                    process_memory_fabric
                        .clone()
                        .context("ordinary recovery fabric missing")?,
                    workspace_root.clone(),
                    run_lineage
                        .owner_id
                        .clone()
                        .context("ordinary recovery owner missing")?,
                    run_id.clone(),
                )?)
            } else {
                None
            };
            let is_operator_process = process_source_launch
                .as_ref()
                .is_some_and(|launch| !launch.is_ordinary());
            let operator_recovery = if is_operator_process {
                Some(process_terminal::operator::OperatorProcessRecovery::new(
                    process_memory_fabric
                        .clone()
                        .context("operator recovery fabric missing")?,
                    workspace_root.clone(),
                    run_lineage
                        .owner_id
                        .clone()
                        .context("operator recovery owner missing")?,
                    run_id.clone(),
                )?)
            } else {
                None
            };
            let process_batch_id = batch_id.map(str::to_owned);
            #[cfg(test)]
            let test_worker_executable = self.test_worker_executable.clone();
            let monitor_recovery = process_recovery.clone();
            let verified_terminal = Arc::new(parking_lot::Mutex::new(None));
            let monitor_verified_terminal = Arc::clone(&verified_terminal);
            let ordinary_terminal = Arc::new(parking_lot::Mutex::new(None));
            let monitor_ordinary_terminal = Arc::clone(&ordinary_terminal);
            let operator_terminal = Arc::new(parking_lot::Mutex::new(None));
            let monitor_operator_terminal = Arc::clone(&operator_terminal);
            let monitor_active_runs = active_runs.clone();
            let monitor_progress = Arc::clone(&process_progress);
            let jh = tokio::spawn(crate::runtime::registry::scope_current(
                process_work_id,
                SPAWN_EXECUTION_CONTEXT.scope(
                    process_execution_ctx,
                    crate::agent::idle::scope_beat(Some(Arc::clone(&monitor_progress)), async move {
                        let failure_active_runs = active_runs.clone();
                        let failure_run_id = rid.clone();
                        let monitor_result = std::panic::AssertUnwindSafe(async {
                            tracing::info!(run_id = %rid, "Sub-agent process starting");

                            let mut worker_result = run_sub_agent_process(
                                &rid,
                                &task_owned,
                                &provider_name,
                                &model,
                                api_key.as_deref(),
                                route_hint.as_deref(),
                                route_source_provider.as_deref(),
                                route_credential.as_ref(),
                                route_uses_root_connection,
                                direct_uses_root_connection,
                                temperature,
                                &workspace_root,
                                &worker_workspace_root,
                                identity_dir.as_deref(),
                                process_system_prompt.as_deref(),
                                &allowed_tools,
                                keep_workspace,
                                process_scope.as_ref(),
                                process_spawn_depth,
                                &process_session_scope_key,
                                process_parent_run_id.as_deref(),
                                process_agent_id.as_deref(),
                                &process_lineage,
                                &process_memory_strategy,
                                &process_memory_backend,
                                &process_config_dir,
                                &process_config_generation,
                                process_event_recording,
                                &process_compaction_config,
                                &process_channel_exclusions,
                                monitor_process_control.as_ref(),
                                process_steer_rx,
                                history_arc,
                                Arc::clone(&monitor_progress),
                                process_provider_selection.as_ref(),
                                process_memory_fabric.as_ref(),
                                monitor_recovery.as_deref(),
                                process_source_launch,
                                process_batch_id.as_deref(),
                                #[cfg(test)]
                                test_worker_executable.as_ref(),
                            )
                            .await;

                            let recovered_usage = if matches!(
                                &worker_result,
                                Ok(ProcessWorkerOutcome::Terminated(_) | ProcessWorkerOutcome::ExitedWithoutResult(_))
                            ) {
                                let context = current_spawn_execution_context();
                                if let Some(grant) = context.as_ref().and_then(|value| value.incoming_grant.as_ref()) {
                                    let recovery = async {
                                        let fabric = process_memory_fabric.as_ref().context("worker recovery storage missing")?;
                                        let input = context.as_ref().and_then(|value| value.input_receipt.as_deref())
                                            .context("worker recovery input missing")?;
                                        if let Some(completion) = process_terminal::recover_completion(fabric, grant, input).await? {
                                            Ok::<_, anyhow::Error>((Some(completion), None))
                                        } else {
                                            let usage = process_terminal::recover_usage(fabric, grant, input).await?;
                                            Ok((None, Some(usage)))
                                        }
                                    }.await;
                                    match recovery {
                                        Ok((completion, usage)) => {
                                            if let Some(completion) = completion {
                                                worker_result = Ok(ProcessWorkerOutcome::VerifiedFinished(completion));
                                            }
                                            usage
                                        }
                                        Err(error) => {
                                            tracing::error!(run_id = %rid, error = %error,
                                                "worker interruption settlement remains unconfirmed");
                                            return;
                                        }
                                    }
                                } else { None }
                            } else { None };
                            let (status, mut result_text, mut token_usage_records, finalization) = match worker_result {
                                Ok(ProcessWorkerOutcome::VerifiedFinished(completion)) => {
                                    *monitor_verified_terminal.lock() = Some(completion.clone());
                                    let (result, usage) = completion.into_parts();
                                    let (status, text) = if result.success {
                                        (SubAgentStatus::Completed(result.output.clone()), result.output)
                                    } else {
                                        let error = result.error.unwrap_or_else(|| "worker failed".to_owned());
                                        (
                                            SubAgentStatus::Failed(sub_agent_reported_failure(&error)),
                                            format!("Sub-agent error: {error}"),
                                        )
                                    };
                                    (status, text, usage, ProcessFinalization::Natural)
                                }
                                Ok(ProcessWorkerOutcome::OrdinaryVerifiedFinished(completion)) => {
                                    *monitor_ordinary_terminal.lock() = Some(completion.clone());
                                    let result = completion.result;
                                    let (status, text) = if result.success {
                                        (SubAgentStatus::Completed(result.output.clone()), result.output)
                                    } else {
                                        let error = result.error.unwrap_or_else(|| "worker failed".to_owned());
                                        (SubAgentStatus::Failed(sub_agent_reported_failure(&error)), format!("Sub-agent error: {error}"))
                                    };
                                    (status, text, completion.usage, ProcessFinalization::Natural)
                                }
                                Ok(ProcessWorkerOutcome::OperatorVerifiedFinished(completion)) => {
                                    *monitor_operator_terminal.lock() = Some(completion.clone());
                                    let result = completion.result;
                                    let (status, text) = if result.success {
                                        (SubAgentStatus::Completed(result.output.clone()), result.output)
                                    } else {
                                        let error = result.error.unwrap_or_else(|| "worker failed".to_owned());
                                        (SubAgentStatus::Failed(sub_agent_reported_failure(&error)), format!("Sub-agent error: {error}"))
                                    };
                                    (status, text, completion.usage, ProcessFinalization::Natural)
                                }
                                Ok(ProcessWorkerOutcome::VerifiedStartupFailure(result)) => {
                                    let error = result.error.context("verified startup failure detail missing");
                                    let error = match error {
                                        Ok(error) => error,
                                        Err(error) => {
                                            tracing::error!(run_id = %rid, error = %error, "verified startup result invalid");
                                            return;
                                        }
                                    };
                                    (
                                        SubAgentStatus::Failed(sub_agent_reported_failure(&error)),
                                        format!("Sub-agent startup error: {error}"),
                                        Vec::new(),
                                        ProcessFinalization::Natural,
                                    )
                                }
                                Ok(ProcessWorkerOutcome::UnverifiedStartupFailure(_)) => {
                                    tracing::error!(run_id = %rid, "unverified startup failure escaped process owner");
                                    return;
                                }
                                Ok(ProcessWorkerOutcome::Finished(result)) if result.success => {
                                    let token_usage_records = result
                                        .tokens_used
                                        .as_ref()
                                        .and_then(|usage| {
                                            crate::llm::route_decision::MeteredTokenUsageRecord::from_parts(
                                                &provider_name,
                                                &model,
                                                usage,
                                                &process_cost_config,
                                            )
                                        })
                                        .into_iter()
                                        .collect::<Vec<_>>();
                                    (
                                        SubAgentStatus::Completed(result.output.clone()),
                                        result.output,
                                        token_usage_records,
                                        ProcessFinalization::Natural,
                                    )
                                }
                                Ok(ProcessWorkerOutcome::Finished(result)) => {
                                    let error = result.error.unwrap_or_else(|| "worker failed".to_string());
                                    let msg = format!("Sub-agent error: {error}");
                                    // `result.error` is a field of the JSON the
                                    // worker itself printed, so it is the run's
                                    // account of the run — quarantined here so
                                    // it cannot pass for a runtime wording in
                                    // `termination_cause`. The announcement
                                    // above still shows the raw account.
                                    (
                                        SubAgentStatus::Failed(sub_agent_reported_failure(&error)),
                                        msg,
                                        Vec::new(),
                                        ProcessFinalization::Natural,
                                    )
                                }
                                Ok(ProcessWorkerOutcome::Terminated(reason)) => {
                                    let msg = format!("Sub-agent process terminated: {reason}");
                                    (
                                        SubAgentStatus::Failed(reason),
                                        msg,
                                        Vec::new(),
                                        ProcessFinalization::Terminated,
                                    )
                                }
                                Ok(ProcessWorkerOutcome::OrdinaryInterrupted { detail, usage }) => {
                                    let error = format!("{}; cleanup and provider usage remain unconfirmed; {} visible segments lack a terminal",
                                        exited_without_result_reason(&detail), usage.unfinished_segments);
                                    (SubAgentStatus::Failed(error.clone()), format!("Sub-agent process error: {error}"),
                                        usage.records, ProcessFinalization::Unconfirmed)
                                }
                                Ok(ProcessWorkerOutcome::OperatorInterrupted { detail, usage }) => {
                                    let error = format!("{}; cleanup and provider usage remain unconfirmed; {} visible segments lack a terminal",
                                        exited_without_result_reason(&detail), usage.unfinished_segments);
                                    (SubAgentStatus::Failed(error.clone()), format!("Sub-agent process error: {error}"),
                                        usage.records, ProcessFinalization::Unconfirmed)
                                }
                                Ok(ProcessWorkerOutcome::ExitedWithoutResult(detail)) => {
                                    // MUTATION GUARD: folding this back into the
                                    // generic parse-error path loses the only
                                    // wording an operator can act on, and folding it
                                    // into "keep waiting" would leave the run
                                    // `Running` for as long as the process lives.
                                    let error = exited_without_result_reason(&detail);
                                    let msg = format!("Sub-agent process error: {error}");
                                    (
                                        SubAgentStatus::Failed(error),
                                        msg,
                                        Vec::new(),
                                        ProcessFinalization::Natural,
                                    )
                                }
                                Ok(ProcessWorkerOutcome::TerminationFailed(error)) => {
                                    let msg = format!("Sub-agent process termination failed: {error}");
                                    (
                                        SubAgentStatus::Failed(error),
                                        msg,
                                        Vec::new(),
                                        ProcessFinalization::TerminationFailed,
                                    )
                                }
                                Err(error) => {
                                    let msg = format!("Sub-agent process error: {error}");
                                    (
                                        SubAgentStatus::Failed(error.to_string()),
                                        msg,
                                        Vec::new(),
                                        if is_ordinary_process || is_operator_process || current_spawn_execution_context().as_ref()
                                            .is_some_and(|context| context.incoming_grant.is_some()) {
                                            ProcessFinalization::Unconfirmed
                                        } else { ProcessFinalization::Natural },
                                    )
                                }
                            };

                            if let Some(recovered) = recovered_usage {
                                token_usage_records.extend(recovered.records);
                                result_text.push_str(&format!(
                                    "; provider usage may be incomplete ({} recorded segments lack terminals)",
                                    recovered.unfinished_segments,
                                ));
                            }

                            let announce = format_announce_message(&rid, &status, &result_text);
                            if let Err(error) = commit_process_terminal_state(
                                &active_runs,
                                &rid,
                                status.clone(),
                                token_usage_records,
                                monitor_process_control.as_ref(),
                                finalization,
                                None,
                                #[cfg(test)]
                                None,
                            )
                            .await {
                                tracing::error!(run_id = %rid, error = %error, "process terminal registry commit remains unconfirmed");
                                return;
                            }
                            if finalization == ProcessFinalization::Unconfirmed {
                                return;
                            }
                            record_spawn_result_event(
                                process_memory_fabric.as_ref(),
                                process_result_scope,
                                &result_text,
                                &status,
                                &process_lineage,
                                (finalization == ProcessFinalization::Terminated).then_some("task.killed"),
                            )
                            .await;

                            if !announce_result {
                                tracing::debug!(
                                    run_id = %rid,
                                    "Sub-agent process result withheld from the channel (announce=false); \
                                     it is delivered to whoever joins this run's batch"
                                );
                            } else if let Some(target) = recipient {
                                // Not a mirror of the task-mode block any more:
                                // the same function, so the gate this path gets
                                // is literally the gate the task-mode tests
                                // exercise. Driving a real process-mode
                                // announcement from a unit test is impossible
                                // (the worker re-execs `current_exe`, which under
                                // `cargo test` is the test binary), so sharing the
                                // implementation is what covers it.
                                deliver_or_withhold_notice(
                                    &process_announce_security,
                                    process_announce_origin.as_ref(),
                                    channel.as_ref(),
                                    &process_announce_history,
                                    &rid,
                                    &target,
                                    &announce,
                                    OutboundNotice::ProcessRunResult,
                                )
                                .await;
                            }

                            tracing::info!(run_id = %rid, "Sub-agent process finished");
                        })
                        .catch_unwind()
                        .await;

                        if monitor_result.is_err() {
                            commit_process_owner_failure_if_unfinalized(
                                &failure_active_runs,
                                &failure_run_id,
                                monitor_process_control.as_ref(),
                                PROCESS_OWNER_PANICKED_PREFIX.to_string(),
                            )
                            .await;
                        }
                    }),
                ),
            ));

            // Process-mode callers must never abort this monitor: it is the
            // sole owner responsible for signalling and reaping the OS child.
            // Awaiting the handle from a *separate* task does not abort it, and
            // is the only way a cancellation of the monitor (runtime shutdown,
            // a future registry abort) can still reach the tool-plane row.
            watch_process_mode_monitor(
                jh,
                monitor_active_runs,
                run_id.clone(),
                process_control.clone(),
                sub_agent_work,
                verified_terminal,
                process_recovery,
                ordinary_terminal,
                ordinary_recovery,
                operator_terminal,
                operator_recovery,
            );

            return Ok(SpawnOutcome::started(
                run_id.clone(),
                format!(
                    "Sub-agent spawned in process mode (run_id: {run_id}). {}",
                    spawn_completion_note(announce_result)
                ),
            ));
        }

        // Create shared history and steer channel for this run
        let history_arc: Arc<RwLock<Vec<HistoryEntry>>> = Arc::new(RwLock::new(Vec::new()));
        // Class B — bounded. Steering messages are produced by operators and by
        // model tool calls, neither of which is rate-limited now that sub-agent
        // concurrency is uncapped, while the consumer only polls between tool
        // iterations. An unbounded queue therefore grows for the whole lifetime
        // of a long-running sub-agent.
        let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(STEER_CHANNEL_CAPACITY);
        // Minted on the spawning task so the run's beat is parented on the
        // caller's (see `crate::agent::idle::child_beat`). The sub-agent's own
        // guarded turn parents *its* beat on this one, so every ProgressKind
        // event it produces refreshes this row and the caller's window alike.
        let task_progress = crate::agent::idle::child_beat();

        // Clone everything the spawned task needs.
        // Resolve the announce channel from the *per-turn* channel name bound to
        // this run (the launching message's scope), not the shared active channel
        // — so a concurrently-processed message cannot mis-route this run's
        // result. Falls back to the active channel when no scope name is present.
        let channel = match &operator_channel {
            Some(channel) => Arc::clone(channel),
            None => self.resolve_announce_channel(run_channel_name.as_deref()).await,
        };
        let mut provider_name = resolved_provider_name;
        let mut model = resolved_model;
        let child_provider_binding = if let Some(grant) = child_grant.as_ref() {
            let credential = child_credential.clone().context("child credential selection missing")?;
            let (requested_child_provider, requested_child_model) =
                child_requested_model.as_ref().context("child model request missing")?;
            let binding = Arc::new(crate::runtime::child_source::ChildProviderBinding::build(
                Arc::clone(grant),
                requested_child_provider,
                requested_child_model,
                credential,
            )?);
            provider_name = binding.provider_name().to_owned();
            model = binding.model().to_owned();
            Some(binding)
        } else if let Some(source) = &operator_source {
            let binding = Arc::new(crate::runtime::child_source::ChildProviderBinding::build_operator(
                source,
            )?);
            provider_name = binding.provider_name().to_owned();
            model = binding.model().to_owned();
            Some(binding)
        } else if let Some(binding) = ordinary_binding {
            provider_name = binding.provider_name().to_owned();
            model = binding.model().to_owned();
            Some(binding)
        } else {
            None
        };
        let temperature = resolved_temperature;
        // An explicit hint always builds its exact selected chain, even when
        // it names the gateway provider: the gateway may hold another route's
        // credential. Direct models keep their existing provider plan.
        if let Some(source) = &operator_source {
            source.verify_live()?;
            let binding = child_provider_binding
                .as_ref()
                .context("operator child model binding missing")?;
            anyhow::ensure!(
                Arc::ptr_eq(&binding.provider(), source.provider()),
                "operator child provider differs from captured ingress"
            );
        }
        let pinned_generation_id = || {
            child_grant
                .as_ref()
                .map(|grant| grant.generation().id.0)
                .or_else(|| ordinary_publication.map(|publication| publication.generation().id.0))
                .or_else(|| operator_source.as_ref().map(|source| source.generation().id.0))
                .context("gateway child provider generation missing")
        };
        let provider_result = if matches!(&task_provider_plan, ChildProvider::Gateway)
            && operator_source.is_none()
            && let Some(resolver) = &self.pinned_provider_resolver
        {
            resolver.resolve(pinned_generation_id()?, &provider_name, &model).await
        } else if let Some(binding) = child_provider_binding.as_ref() {
            Ok(binding.provider())
        } else if let Some(resolved) = selected_hint.as_ref() {
            let reliability = crate::config::ReliabilityConfig {
                fallback_providers: Vec::new(),
                ..provider_reliability.clone()
            };
            providers::create_provider_for(
                resolved,
                child_model_catalog,
                &reliability,
                &child_model_config.cost,
                provider_options,
            )
            .map(Arc::from)
        } else {
            match task_provider_plan {
                ChildProvider::Root => {
                    let credential = child_credential.clone().context("child credential selection missing")?;
                    let resolved = match providers::selection::check_with(
                        child_model_config,
                        child_model_catalog,
                        &provider_name,
                        &model,
                        credential,
                        &providers::catalog::Requirement::chat(),
                        providers::catalog::UnknownPolicy::Invocation,
                    ) {
                        Ok(resolved) => resolved,
                        Err(refusal) => return Ok(SpawnOutcome::rejected(format!("Cannot spawn this run: {refusal}"))),
                    };
                    providers::create_provider_for(
                        &resolved,
                        child_model_catalog,
                        provider_reliability,
                        &child_model_config.cost,
                        provider_options,
                    )
                    .map(Arc::from)
                }
                ChildProvider::Build { api_key } => {
                    let credential = child_credential.clone().context("child credential selection missing")?;
                    let resolved = match providers::selection::check_with(
                        child_model_config,
                        child_model_catalog,
                        &provider_name,
                        &model,
                        credential,
                        &providers::catalog::Requirement::chat(),
                        providers::catalog::UnknownPolicy::Invocation,
                    ) {
                        Ok(resolved) => resolved,
                        Err(refusal) => {
                            return Ok(SpawnOutcome::rejected(format!("Cannot spawn this run: {refusal}")));
                        }
                    };
                    if let Some(planned_key) = api_key.as_deref() {
                        anyhow::ensure!(
                            resolved.credential().api_key() == Some(planned_key),
                            "resolved child credential differs from the provider plan"
                        );
                    }
                    Self::build_override_provider(
                        &resolved,
                        child_model_catalog,
                        provider_reliability,
                        &child_model_config.cost,
                        provider_options,
                    )
                }
                ChildProvider::Gateway => {
                    if let Some(resolver) = &self.pinned_provider_resolver {
                        resolver.resolve(pinned_generation_id()?, &provider_name, &model).await
                    } else {
                        Ok(self.provider.clone())
                    }
                }
            }
        };
        let provider = match provider_result {
            Ok(provider) => provider,
            Err(error) => {
                return Ok(SpawnOutcome::rejected(format!(
                    "Failed to create provider '{}' for sessions_spawn: {}",
                    providers::redact_endpoint_userinfo(&provider_name),
                    providers::sanitize_api_error(&providers::redact_endpoint_userinfo(&error.to_string()))
                )));
            }
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
        let identity_workspace = child_model_config.workspace_dir.clone();
        let security = Arc::clone(launch_security);
        // Second handle, for the announcement gate: `security` above is moved
        // into the sub-agent's own loop, and the announcement is authorized only
        // once that loop has returned.
        let announce_security = Arc::clone(launch_security);
        // The launching turn's trusted identity, captured atomically with
        // `recipient` / `run_channel_name` from the same scope snapshot.
        let announce_origin = AnnounceOrigin::from_scope(spawn_scope.as_ref());
        // Same handle the run writes its transcript through, so a refused
        // announcement is visible on the run itself and not only in the log.
        let announce_history = history_arc.clone();
        let mut terminal_usage = child_grant.as_ref().map(|grant| {
            Arc::new(crate::runtime::turn_terminal::TerminalUsageCollector::new(
                grant
                    .generation()
                    .effective
                    .workspace_dir
                    .to_string_lossy()
                    .into_owned(),
                grant.scope().gate_inputs().0.authority.owner.clone(),
                rid.clone(),
            ))
        });
        let mut task_execution_ctx = SpawnExecutionContext {
            operator_authority: None,
            ordinary_authority: None,
            incoming_grant: child_grant,
            provider_binding: child_provider_binding,
            terminal_usage: terminal_usage.clone(),
            input_receipt: source_receipt,
            run_id: rid.clone(),
            session_scope_key: session_scope_key.clone(),
            spawn_depth,
            owner_id: run_lineage.owner_id.clone(),
            owner_isolated: run_lineage.owner_isolated,
            topic_id: run_lineage.topic_id.clone(),
            source_message_event_id: run_lineage.source_message_event_id.clone(),
            // A spawn-run context (this run may itself spawn): its children
            // compute spawn_depth + 1.
            is_turn_root: false,
            channel_excluded_tools: inherited_channel_exclusions.clone(),
        };
        let task_memory_fabric = memory_fabric.clone();
        let task_result_scope = spawn_scope_for_event.clone();
        let task_lineage = run_lineage.clone();
        let agent_label_for_registry = selected_agent.as_ref().map(|(name, _)| name.clone());
        let (system_prompt, filtered_tools) = if let Some((agent, cfg)) = selected_agent {
            let identity_prompt = cfg
                .identity_dir
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|identity_dir| build_identity_prompt(&identity_workspace.join(identity_dir)))
                .unwrap_or_default();
            let prompt = if identity_prompt.trim().is_empty() {
                DEFAULT_SUB_AGENT_SYSTEM_PROMPT.to_string()
            } else {
                identity_prompt
            };
            let memory_scope = parse_memory_scope(cfg.memory_scope.as_deref())?;
            let tools = tools.map(|registry| {
                if ordinary_publication.is_some() {
                    // Keep the backend root identity and dynamic metadata for
                    // the sealed catalog. The child service enforces the exact
                    // manifest, agent allowance and inherited exclusions.
                    let memory_prefix = (memory_scope == MemoryScope::Isolated).then(|| agent.clone());
                    return Arc::new(
                        registry
                            .iter()
                            .map(|tool| {
                                Box::new(AllowedToolProxy {
                                    source: Arc::clone(&registry),
                                    public_name: tool.name().to_owned(),
                                    memory_prefix: memory_prefix.clone(),
                                }) as Box<dyn Tool>
                            })
                            .collect(),
                    );
                }
                resolve_tools_for_agent(
                    registry,
                    &agent,
                    memory_scope,
                    if cfg.allowed_tools.is_empty() {
                        None
                    } else {
                        Some(&cfg.allowed_tools)
                    },
                    &inherited_channel_exclusions,
                )
            });
            (prompt, tools)
        } else {
            (DEFAULT_SUB_AGENT_SYSTEM_PROMPT.to_string(), tools)
        };

        // Register the run.
        //
        // A background run outlives its spawning turn. Its independent observer
        // owns cleanup; parenting this to the caller would cancel valid work
        // when that caller returns. Register before publishing the active row.
        let task_control = TaskRunControl::new();
        if let Some(source) = &operator_source {
            let (authority, lifetime) = source.issue_task(
                Arc::clone(
                    task_execution_ctx
                        .provider_binding
                        .as_ref()
                        .context("operator model binding missing")?,
                ),
                &task_result_scope,
                task,
                task_execution_ctx.input_receipt.as_deref(),
                task_control.cancellation(),
            )?;
            task_control.retain_operator_lifetime(lifetime)?;
            use crate::runtime::turn_source::TurnSourceProof;
            let usage = Arc::new(crate::runtime::turn_terminal::TerminalUsageCollector::new_ordinary(
                authority.workspace_id().to_owned(),
                authority.owner_id().to_owned(),
                rid.clone(),
                authority.generation().effective.memory.event_recording_config(),
            ));
            terminal_usage = Some(Arc::clone(&usage));
            task_execution_ctx.terminal_usage = Some(usage);
            task_execution_ctx.operator_authority = Some(authority);
        }
        if ordinary_publication.is_some() {
            let binding = task_execution_ctx
                .provider_binding
                .as_ref()
                .context("ordinary task provider binding missing")?;
            let caller = caller.as_ref().context("ordinary task caller missing")?;
            let (authority, lifetime) = binding.issue_ordinary_task(
                caller,
                &task_result_scope,
                task,
                task_execution_ctx.input_receipt.as_deref(),
                task_control.cancellation(),
            )?;
            task_control.retain_ordinary_lifetime(lifetime)?;
            use crate::runtime::turn_source::TurnSourceProof;
            let (proof, _) = authority.source()?;
            let usage = Arc::new(crate::runtime::turn_terminal::TerminalUsageCollector::new_ordinary(
                proof.workspace_id().to_owned(),
                proof.owner_id().to_owned(),
                rid.clone(),
                authority
                    .publication()
                    .generation()
                    .effective
                    .memory
                    .event_recording_config(),
            ));
            terminal_usage = Some(Arc::clone(&usage));
            task_execution_ctx.terminal_usage = Some(usage);
            task_execution_ctx.ordinary_authority = Some(authority);
        }
        let logical_terminal = {
            use crate::agent::terminal::logical::LogicalTaskTerminalRef;
            match (
                task_execution_ctx.incoming_grant.as_deref(),
                task_execution_ctx.ordinary_authority.as_deref(),
                task_execution_ctx.operator_authority.as_deref(),
            ) {
                (Some(grant), None, None) => LogicalTaskTerminalRef::granted(
                    grant,
                    task_execution_ctx
                        .input_receipt
                        .as_deref()
                        .context("logical task input missing")?,
                    &task_result_scope,
                )?,
                (None, Some(authority), None) => LogicalTaskTerminalRef::ordinary(authority)?,
                (None, None, Some(authority)) => LogicalTaskTerminalRef::operator(authority)?,
                _ => anyhow::bail!("logical task completion requires one issued source authority"),
            }
        };
        let run_cleanup = Arc::new(crate::runtime::turn_cleanup::CleanupTracker::default());
        let run_cleanup_scope =
            crate::runtime::turn_cleanup::CleanupScope::new(Some(&run_cleanup), CancellationToken::new())?;

        // Deliberately the *last* fallible-free step before the driver task is
        // spawned, and never earlier: everything between this block and
        // `tokio::spawn` below is infallible, so the presence of a row in
        // `active_runs` implies a driver task that will publish a terminal status
        // for it. Registering before the fallible preparation above (agent prompt
        // and tool resolution) left a `Running` row behind on every early return —
        // no abort handle, no driver task, no observer — and `join` polls such a
        // row forever, with no wall-clock fallback in this runtime to break the
        // tie. Keep any new fallible step above this block.
        //
        // `abort_handle` is filled in right after `tokio::spawn` returns.
        {
            let mut runs = self.active_runs.write().await;
            runs.push(SubAgentRun {
                id: run_id.clone(),
                task: task.to_string(),
                owner_id: run_lineage.owner_id.clone(),
                topic_id: run_lineage.topic_id.clone(),
                source_message_event_id: run_lineage.source_message_event_id.clone(),
                started_at: Utc::now(),
                finished_at: None,
                status: SubAgentStatus::Running,
                recipient: recipient.clone(),
                channel_name: run_channel_name.clone(),
                abort_handle: None,
                process_control: None,
                task_control: Some(Arc::clone(&task_control)),
                history: history_arc.clone(),
                steer_tx: Some(steer_tx.clone()),
                parent_run_id: parent_run_id.clone(),
                session_scope_key: session_scope_key.clone(),
                spawn_depth,
                token_usage_records: Vec::new(),
                progress: Arc::clone(&task_progress),
                batch_id: batch_id.map(str::to_string),
            });
        }

        // Registry row for this run. Parent captured on the spawning task, which
        // is the tool call that requested the spawn; a spawned task inherits no
        // task-locals of its own.
        let sub_agent_parent = crate::runtime::registry::current_work_id();
        let sub_agent_label = agent_label_for_registry.unwrap_or_else(|| "sub-agent".to_string());
        let mut sub_agent_work =
            crate::runtime::registry::register_sub_agent(&sub_agent_label, &rid, sub_agent_parent, batch_id, None);
        let sub_agent_work_id = sub_agent_work.id();
        sub_agent_work.require_cleanup_acknowledgement();
        // As in the process branch: the control plane resolves `run_id` through
        // the process-wide work registry, so the run's existing steer sender is
        // published there. `active_runs` stays the tool-plane source of truth.
        crate::runtime::registry::attach_steer_sender(sub_agent_work_id, steer_tx.clone());

        let terminal_journal = Arc::new(task_terminal::TaskTerminalJournal::new(
            task_memory_fabric.clone(),
            task_result_scope.clone(),
            logical_terminal,
            true,
            terminal_usage,
        ));
        let driver_terminal_journal = Arc::clone(&terminal_journal);
        // Spawn async task (fire-and-forget); capture handle to support kill
        let jh = tokio::spawn(crate::runtime::registry::scope_current(
            sub_agent_work_id,
            SPAWN_EXECUTION_CONTEXT.scope(task_execution_ctx, async move {
                tracing::info!(run_id = %rid, "Sub-agent task starting");
                let provider_started_at = Utc::now();
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
                    security,
                    steer_rx,
                    history_arc,
                    run_on_delta,
                    run_on_tool,
                    run_approval_resolver,
                    restore_active_runs,
                    restore_run_id,
                    Arc::clone(&task_progress),
                );
                let result = run_cleanup_scope.run(run_future).await;
                tracing::info!(run_id = %rid, success = result.is_ok(), "Sub-agent task finished");

                let (status, result_text, terminal_status, history_projection, token_usage_records) = match result {
                    Ok(task_result) => (
                        SubAgentStatus::Completed(task_result.output.clone()),
                        task_result.output.clone(),
                        crate::agent::terminal::TurnTerminalStatus::Completed,
                        Some(crate::agent::terminal::TurnHistoryProjection {
                            assistant_content: task_result.output,
                            history_commit_len: task_result.history_commit_len,
                        }),
                        task_result.segment_usage,
                    ),
                    Err(error) => {
                        // Persist the same runtime-classified reason used by the
                        // live registry. Recovery after commit must not lose the
                        // quarantine tag on provider/tool-authored failure text.
                        let reason = task_failure_reason(&error);
                        let terminal_status = if matches!(termination_cause(&reason), TerminationCause::Killed) {
                            crate::agent::terminal::TurnTerminalStatus::Cancelled
                        } else {
                            crate::agent::terminal::TurnTerminalStatus::Failed
                        };
                        (
                            SubAgentStatus::Failed(reason.clone()),
                            reason,
                            terminal_status,
                            None,
                            Vec::new(),
                        )
                    }
                };

                let delivery_intent = recipient.as_ref().map_or_else(
                    || crate::agent::terminal::TurnDeliveryIntent::Deferred {
                        route: "sessions_registry".to_string(),
                    },
                    |target| crate::agent::terminal::TurnDeliveryIntent::Reply { target: target.clone() },
                );
                match driver_terminal_journal
                    .commit(crate::agent::terminal::TurnTerminalDraft {
                        decision: None,
                        terminal_id: rid.clone(),
                        scope: task_result_scope.clone(),
                        status: terminal_status,
                        history: history_projection,
                        history_scope: None,
                        provider_outcome: None,
                        telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                            summary: result_text.clone(),
                            started_at: provider_started_at,
                            finished_at: Utc::now(),
                        },
                        delivery_intent,
                    })
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(run_id = %rid, error = %error, "task terminal persistence remains unconfirmed");
                        // The external observer retries this exact proposal.
                        // Do not publish a result or a substitute usage receipt.
                        return;
                    }
                }
                let announce = format_announce_message(&rid, &status, &result_text);
                record_spawn_result_event(
                    Some(&task_memory_fabric),
                    task_result_scope,
                    &result_text,
                    &status,
                    &task_lineage,
                    None,
                )
                .await;

                // Update run status
                {
                    let mut runs = active_runs.write().await;
                    if let Some(run) = runs.iter_mut().find(|r| r.id == rid) {
                        run.finished_at = Some(Utc::now());
                        run.status = status;
                        run.token_usage_records.extend(token_usage_records);
                        run.steer_tx = None; // drop sender — no more steering possible
                    }
                }

                // Announce result back to channel if we have a recipient
                if !announce_result {
                    tracing::debug!(
                        run_id = %rid,
                        "Sub-agent result withheld from the channel (announce=false); \
                         it is delivered to whoever joins this run's batch"
                    );
                } else if let Some(target) = recipient {
                    // MUTATION GUARD: route this straight to `channel.send` and the
                    // model-supplied `recipient` on the spawn call reaches the
                    // channel with no authorization at all — the hole
                    // `message_send`'s own outbound gate closes on its side.
                    deliver_or_withhold_notice(
                        &announce_security,
                        announce_origin.as_ref(),
                        channel.as_ref(),
                        &announce_history,
                        &rid,
                        &target,
                        &announce,
                        OutboundNotice::RunResult,
                    )
                    .await;
                } else {
                    tracing::warn!(
                        run_id = %rid,
                        "Sub-agent completed but no recipient configured for announcement"
                    );
                }
            }),
        ));

        // Install the external owner before the first suspension after spawn.
        let abort_handle = jh.abort_handle();
        task_control.attach(abort_handle.clone());
        crate::runtime::registry::attach_abort_handle(sub_agent_work_id, abort_handle.clone());
        watch_task_mode_monitor(
            jh,
            self.active_runs.clone(),
            run_id.clone(),
            Some(run_cleanup),
            Some(task_control),
            Some(sub_agent_work),
            Some(terminal_journal),
        );
        {
            let mut runs = self.active_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
                run.abort_handle = Some(abort_handle);
            }
        }

        Ok(SpawnOutcome::started(
            run_id.clone(),
            format!(
                "Sub-agent spawned (run_id: {run_id}). {}",
                spawn_completion_note(announce_result)
            ),
        ))
    }
    fn memory_fabric(&self) -> Option<MemoryFabric> {
        self.source_fabric.clone().or_else(|| {
            self.memory.as_ref().map(|memory| {
                MemoryFabric::new(memory.clone(), self.workspace_dir.to_string_lossy())
                    .with_event_recording(self.event_recording)
            })
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
    async fn execute_list(&self, owner_scope: &sessions_read_model::RunOwnerScope) -> anyhow::Result<ToolResult> {
        let registry = self.active_runs.read().await;
        let runs = registry
            .iter()
            .filter(|run| owner_scope.admits(run.owner_id.as_deref()))
            .collect::<Vec<_>>();
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
                let usage = format_run_usage(&r.token_usage_records);
                format!(
                    "• `{}` [source=runtime, manageable=true, usage={usage}, {age}s ago] {status}\n  task: {}\n  depth: {} | parent: {}",
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
    async fn execute_kill(
        &self,
        run_id: &str,
        approval_grant: Option<&ApprovalGrant>,
        notice_origin: Option<&AnnounceOrigin>,
        owner_scope: &sessions_read_model::RunOwnerScope,
        args: &serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        let (process_control, task_kill_target) = {
            let mut runs = self.active_runs.write().await;
            match runs
                .iter_mut()
                .find(|r| r.id == run_id && owner_scope.admits(r.owner_id.as_deref()))
            {
                Some(run) => {
                    match &run.status {
                        // A live run (executing) or one suspended awaiting approval
                        // (NeedsInput) is killable: aborting the task tears down the
                        // suspended approval resolver's pending await along with it.
                        SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. } => {
                            let operation_name = format!("sessions_spawn:kill:{run_id}");
                            if let Err(error) = SideEffectGate::new(self.security.as_ref())
                                .authorize_resource_operation(
                                    self.name(),
                                    &operation_name,
                                    ResourceRiskLevel::Medium,
                                    approval_grant,
                                )
                            {
                                return Ok(ToolResult {
                                    success: false,
                                    output: String::new(),
                                    error: Some(error),
                                });
                            }
                            if let Some(control) = run.process_control.clone() {
                                (Some(control), None)
                            } else {
                                let recipient = run.recipient.clone();
                                // Per-turn channel bound at spawn time — kill-notify routes
                                // to the same channel + recipient as the launching message,
                                // not the shared active channel (avoids cross-channel leak).
                                let channel_name = run.channel_name.clone();
                                let rid = run.id.clone();
                                (None, Some((recipient, channel_name, rid, run.clone())))
                            }
                        }
                        SubAgentStatus::Completed(_) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Run `{run_id}` already completed.")),
                            });
                        }
                        SubAgentStatus::Failed(e) => {
                            if run
                                .task_control
                                .as_ref()
                                .is_some_and(|control| control.finalization() == Some(TaskFinalization::Terminated))
                                || run.process_control.as_ref().is_some_and(|control| {
                                    control.finalization() == Some(ProcessFinalization::Terminated)
                                })
                            {
                                return Ok(ToolResult {
                                    success: true,
                                    output: format!("Sub-agent `{run_id}` has been killed."),
                                    error: None,
                                });
                            }
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Run `{run_id}` already failed: {e}")),
                            });
                        }
                    }
                }
                None => (None, None),
            }
        };

        if let Some(control) = process_control {
            return match control.request_termination(KILLED_BY_USER_REASON).await {
                ProcessTerminationRequestResult::Finalized(ProcessFinalization::Terminated) => Ok(ToolResult {
                    success: true,
                    output: format!("Sub-agent `{run_id}` has been killed."),
                    error: None,
                }),
                ProcessTerminationRequestResult::Finalized(ProcessFinalization::Natural) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Run `{run_id}` finalized before the termination request won.")),
                }),
                ProcessTerminationRequestResult::Finalized(ProcessFinalization::TerminationFailed) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Run `{run_id}` could not be terminated by its process owner.")),
                }),
                ProcessTerminationRequestResult::Finalized(ProcessFinalization::Unconfirmed) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Run `{run_id}` cleanup and settlement remain unconfirmed.")),
                }),
                ProcessTerminationRequestResult::Pending => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Termination was requested for run `{run_id}`, but its process owner is still pending reap; the run remains active."
                    )),
                }),
            };
        }

        let Some((recipient_opt, channel_name_opt, rid, killed_run)) = task_kill_target else {
            return Ok(self.no_runtime_run_result(run_id, args).await);
        };

        let Some(control) = killed_run.task_control.as_ref() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Task termination owner is unavailable; completion is unconfirmed".to_owned()),
            });
        };
        match control.request_termination(KILLED_BY_USER_REASON).await {
            Some(TaskFinalization::Terminated) => {}
            Some(TaskFinalization::Natural) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Run `{run_id}` finalized before termination was confirmed.")),
                });
            }
            Some(TaskFinalization::Unconfirmed) | None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Termination for run `{run_id}` remains unconfirmed; cleanup has not been acknowledged."
                    )),
                });
            }
        }
        let killed_run = self
            .active_runs
            .read()
            .await
            .iter()
            .find(|run| run.id == rid)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("terminated task row is unavailable"))?;

        self.record_active_run_task_event(&killed_run, "task.killed", json!({"reason": "killed by user"}))
            .await;

        if let Some(target) = recipient_opt {
            let channel = self.resolve_announce_channel(channel_name_opt.as_deref()).await;
            // The receipt is addressed to the run's `recipient`, which the model
            // wrote on the *spawn* call — the identical bypass surface the
            // completion announcement had, and the one it kept after that was
            // closed: name any address, kill the run, and the notice went out
            // with no authorization at all.
            //
            // This does not re-open the deliberate call that a kill receipt
            // ignores `announce`. That switch answers "does a run report itself?"
            // and a kill is the operator's own action, not the run's; the ACL
            // answers "may this process address that recipient?". Two questions,
            // two mechanisms — folding them into one switch would mean either
            // silencing operator receipts or leaving them unauthorized.
            //
            // MUTATION GUARD: send directly on `channel` here instead and
            // `a_kill_notice_to_a_denied_recipient_is_withheld_and_recorded_on_the_run`
            // plus
            // `a_model_supplied_recipient_cannot_bypass_send_deny_through_the_kill_notice`
            // go red.
            deliver_or_withhold_notice(
                self.security.as_ref(),
                notice_origin,
                channel.as_ref(),
                &killed_run.history,
                &rid,
                &target,
                &format!("🤖 Sub-agent `{rid}` was killed by user."),
                OutboundNotice::KillNotice,
            )
            .await;
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
    async fn execute_history(
        &self,
        run_id: &str,
        last_n: Option<usize>,
        max_chars_per_entry: usize,
        owner_scope: &sessions_read_model::RunOwnerScope,
        args: &serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        let runs = self.active_runs.read().await;
        let Some(run) = runs
            .iter()
            .find(|r| r.id == run_id && owner_scope.admits(r.owner_id.as_deref()))
        else {
            return Ok(self.no_runtime_run_result(run_id, args).await);
        };

        let entries = run.history.read().await;
        let usage = format_run_usage(&run.token_usage_records);
        if entries.is_empty() {
            let status = match &run.status {
                SubAgentStatus::Running => "still running, no history captured yet",
                SubAgentStatus::AwaitingInput { .. } => "awaiting approval, no history captured yet",
                SubAgentStatus::Completed(_) => "completed but history is empty",
                SubAgentStatus::Failed(_) => "failed, history may be incomplete",
            };
            return Ok(ToolResult {
                success: true,
                output: format!(
                    "No history entries for run `{run_id}` ({status}).\nmetadata: source=runtime, manageable=true, usage={usage}"
                ),
                error: None,
            });
        }

        let retained = last_n.unwrap_or(DEFAULT_HISTORY_LAST_N).min(entries.len());
        let skipped = entries.len().saturating_sub(retained);
        let lines: Vec<String> = entries
            .iter()
            .skip(skipped)
            .map(|e| {
                let ts = e.timestamp.format("%H:%M:%S").to_string();
                let preview: String = e.content.chars().take(max_chars_per_entry).collect();
                let ellipsis = if e.content.chars().count() > max_chars_per_entry {
                    "\n[entry truncated]"
                } else {
                    ""
                };
                format!("[{ts}] **{}**: {}{}", e.role, preview, ellipsis)
            })
            .collect();
        let omitted = if skipped > 0 {
            format!("\n\n[history omitted: {skipped} older entries; use last_n/max_chars_per_entry to adjust]")
        } else {
            String::new()
        };

        Ok(ToolResult {
            success: true,
            output: format!(
                "Conversation history for sub-agent `{run_id}` ({} entries, showing last {retained}):\nmetadata: source=runtime, manageable=true, usage={usage}\n\n{}{}",
                entries.len(),
                lines.join("\n\n"),
                omitted
            ),
            error: None,
        })
    }

    async fn no_runtime_run_result(&self, run_id: &str, args: &serde_json::Value) -> ToolResult {
        if self.recovered_run_exists(run_id, args).await {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Run `{run_id}` exists only as a memory-backed projected session (source=memory, manageable=false). It is not present in the current runtime registry, so sessions_spawn cannot kill, steer, or read live history for it. Use sessions_list to inspect it; treat it as stale/interrupted unless a new runtime process reattaches it."
                )),
            };
        }
        ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("No runtime run found with ID `{run_id}`.")),
        }
    }

    /// Whether the caller could recover `run_id` from memory. Asked with the
    /// caller's own scope, so another owner's recorded run is not confirmed to
    /// exist.
    async fn recovered_run_exists(&self, run_id: &str, args: &serde_json::Value) -> bool {
        let Some(fabric) = self.memory_fabric() else {
            return false;
        };
        sessions_read_model::recover_task_runs(Some(&fabric.memory()), fabric.workspace_id(), args, 100)
            .await
            .is_ok_and(|runs| runs.iter().any(|run| run.run_id == run_id))
    }

    /// Inject a steering message into a running sub-agent.
    async fn execute_steer(
        &self,
        run_id: &str,
        message: &str,
        approval_grant: Option<&ApprovalGrant>,
        owner_scope: &sessions_read_model::RunOwnerScope,
    ) -> anyhow::Result<ToolResult> {
        // The steer sender is cloned out of the registry and the guard is
        // dropped *before* the send. The channel is bounded, so a send can now
        // park until the sub-agent drains it; parking while still holding a
        // read guard would deadlock, because the receiving loop takes a write
        // guard (`restore_running`) as its very next step after `recv`.
        let (steer_tx, run_snapshot) = {
            let runs = self.active_runs.read().await;
            let Some(run) = runs
                .iter()
                .find(|r| r.id == run_id && owner_scope.admits(r.owner_id.as_deref()))
            else {
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
                    (tx.clone(), run.clone())
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

        // Backpressure, not loss: if the sub-agent has not drained its queue,
        // this await slows the steering caller down instead of growing the
        // queue without bound. `send` only fails once the receiver is gone.
        steer_tx
            .send(message.to_string())
            .await
            .map_err(|_| anyhow::anyhow!("Sub-agent steer channel closed unexpectedly"))?;

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
pub(crate) enum MemoryScope {
    Shared,
    Isolated,
}

/// The single definition of what `agents.<name>.memory_scope` accepts.
///
/// `Config::validate` calls this too, so a typo is refused when the config is
/// loaded rather than surviving until the first spawn of that agent. Both
/// callers must keep using this one function: a second copy of the rule that
/// drifts would put the startup check and the spawn-time check out of step.
pub(crate) fn parse_memory_scope(scope: Option<&str>) -> anyhow::Result<MemoryScope> {
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
    let summary = compact_announce_summary(result_text);
    match status {
        SubAgentStatus::Completed(_) if summary.is_empty() => format!("🤖 Sub-agent `{rid}` completed"),
        SubAgentStatus::Completed(_) => format!("🤖 Sub-agent `{rid}` completed: {summary}"),
        _ if summary.is_empty() => format!("🤖 Sub-agent `{rid}` FAILED with an error"),
        _ => format!("🤖 Sub-agent `{rid}` FAILED with error: {summary}"),
    }
}

const ANNOUNCE_SUMMARY_MAX_CHARS: usize = 120;

fn compact_announce_summary(text: &str) -> String {
    let Some(line) = text.lines().map(str::trim).find(|line| is_useful_announce_line(line)) else {
        return String::new();
    };
    clamp_announce_chars(line, ANNOUNCE_SUMMARY_MAX_CHARS)
}

fn is_useful_announce_line(line: &str) -> bool {
    !line.is_empty() && !line.starts_with("```") && !line.starts_with("~~~")
}

fn clamp_announce_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out = String::new();
    for _ in 0..max_chars {
        match chars.next() {
            Some(ch) => out.push(ch),
            None => return out,
        }
    }
    if chars.next().is_some() && max_chars > 0 {
        out.pop();
        out.push('…');
    }
    out
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

    fn source_spec(&self) -> Option<crate::tools::ToolSpec> {
        self.find_source_tool()?.spec_for_name(&self.public_name)
    }

    fn rewrite_args(&self, mut args: serde_json::Value) -> serde_json::Value {
        if self.public_name == "memory_store"
            && let Some(prefix) = &self.memory_prefix
        {
            let new_key = args
                .get("key")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(|key| memory_key_prefix(prefix, key));
            if let Some(new_key) = new_key
                && let Some(map) = args.as_object_mut()
            {
                map.insert("key".to_string(), serde_json::Value::String(new_key));
            }
        }
        args
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
        self.source_spec().map(|spec| spec.parameters).unwrap_or_else(|| {
            json!({
                "type": "object",
                "description": "Unavailable proxied tool"
            })
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let Some(tool) = self.find_source_tool() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Tool '{}' is not registered.", self.public_name)),
            });
        };

        tool.execute_named(&self.public_name, self.rewrite_args(args)).await
    }

    async fn execute_with_cancellation(
        &self,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        self.execute_named_with_cancellation(&self.public_name, args, cancellation)
            .await
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        if let Some(tool) = self.find_source_tool() {
            tool.refresh().await?;
        }
        Ok(())
    }

    fn supports_name(&self, name: &str) -> bool {
        self.find_source_tool().is_some_and(|tool| {
            if self.public_name == tool.name() {
                tool.supports_name(name)
            } else {
                name == self.public_name
            }
        })
    }

    fn specs(&self) -> Vec<crate::tools::ToolSpec> {
        let Some(tool) = self.find_source_tool() else {
            return vec![self.spec()];
        };
        if self.public_name == tool.name() {
            tool.specs()
        } else {
            self.source_spec().into_iter().collect()
        }
    }

    fn spec_for_name(&self, name: &str) -> Option<crate::tools::ToolSpec> {
        self.supports_name(name)
            .then(|| self.find_source_tool()?.spec_for_name(name))
            .flatten()
    }

    fn catalog_specs(&self) -> Vec<crate::tools::ToolSpec> {
        let Some(tool) = self.find_source_tool() else {
            return Vec::new();
        };
        if self.public_name == tool.name() {
            tool.catalog_specs()
        } else {
            tool.spec_for_name(&self.public_name).into_iter().collect()
        }
    }

    fn immutable_catalog_specs(&self) -> Option<&[crate::tools::ToolSpec]> {
        let tool = self.find_source_tool()?;
        if self.public_name == tool.name() {
            tool.immutable_catalog_specs()
        } else {
            None
        }
    }

    fn shared_catalog_snapshot(&self) -> Option<crate::tools::SharedToolCatalogSnapshot> {
        let tool = self.find_source_tool()?;
        let snapshot = tool.shared_catalog_snapshot()?;
        Some(if self.public_name == tool.name() {
            snapshot
        } else {
            snapshot.restrict_to(&self.public_name, false)
        })
    }

    fn catalog_binding_snapshot(&self) -> std::collections::BTreeMap<String, [u8; 32]> {
        let Some(tool) = self.find_source_tool() else {
            return Default::default();
        };
        let mut bindings = tool.catalog_binding_snapshot();
        if self.public_name != tool.name() {
            bindings.retain(|name, _| name == &self.public_name);
        }
        bindings
    }

    fn catalog_availability_is_configured(&self) -> bool {
        self.find_source_tool()
            .is_some_and(Tool::catalog_availability_is_configured)
    }

    async fn resolve_approval_operation(
        &self,
        args: &serde_json::Value,
    ) -> anyhow::Result<Option<(String, crate::acl::approval_grant::RiskLevel)>> {
        let Some(tool) = self.find_source_tool() else {
            return Ok(None);
        };
        // The root resolver has no alias name parameter; do not apply it to
        // another public operation hosted by the same backend.
        if self.public_name != tool.name() {
            return Ok(None);
        }
        if self.public_name == "memory_store" && self.memory_prefix.is_some() {
            tool.resolve_approval_operation(&self.rewrite_args(args.clone())).await
        } else {
            tool.resolve_approval_operation(args).await
        }
    }

    fn tier(&self) -> crate::tools::ToolTier {
        self.find_source_tool()
            .map_or(crate::tools::ToolTier::Standard, Tool::tier)
    }

    fn categories(&self) -> &'static [crate::tools::ToolCategory] {
        self.find_source_tool().map_or(&[], Tool::categories)
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        self.find_source_tool().map_or_else(
            || crate::capability::CapabilityAvailability::declared("proxied tool is not registered"),
            Tool::availability,
        )
    }

    async fn execute_named(&self, name: &str, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        anyhow::ensure!(self.supports_name(name), "proxied tool does not expose requested name");
        let Some(tool) = self.find_source_tool() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Tool '{}' is not registered.", self.public_name)),
            });
        };
        tool.execute_named(name, self.rewrite_args(args)).await
    }

    async fn execute_named_with_cancellation(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        anyhow::ensure!(self.supports_name(name), "proxied tool does not expose requested name");
        let Some(tool) = self.find_source_tool() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Tool '{}' is not registered.", self.public_name)),
            });
        };
        tool.execute_named_with_cancellation(name, self.rewrite_args(args), cancellation)
            .await
    }
}

/// Build the tool registry one sub-agent sees.
///
/// `channel_excluded` is the spawning channel surface's hidden set and is
/// applied last, after the agent's own allowlist: the boundary between a
/// conversation partner and the operations surface is not something an agent
/// configuration — or a `mode: "process"` manifest — may re-open. Names an
/// operator put back through `[tool_tiering] always_include` never reach this
/// list, because it is derived from `for_channel_surface()`, which keeps them.
pub(crate) fn resolve_tools_for_agent(
    source: Arc<Vec<Box<dyn Tool>>>,
    agent_name: &str,
    memory_scope: MemoryScope,
    allowed_tools: Option<&[String]>,
    channel_excluded: &[String],
) -> Arc<Vec<Box<dyn Tool>>> {
    let allowlist = allowed_tools.map(|items| {
        items
            .iter()
            .map(|item| item.trim())
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
    });
    let inherit_all = allowlist
        .as_ref()
        .is_none_or(|items| items.len() == 1 && items.first().is_some_and(|name| *name == "*"));

    let mut selected_names = Vec::new();
    if !inherit_all && let Some(list) = allowlist {
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
    if !selected_names.contains(&crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME)
        && source
            .iter()
            .any(|tool| tool.supports_name(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME))
    {
        selected_names.push(crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME);
    }
    if !channel_excluded.is_empty() {
        selected_names.retain(|name| !channel_excluded.iter().any(|hidden| hidden == name));
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

pub(crate) async fn refresh_tools_for_explicit_allowlist(
    source: &Arc<Vec<Box<dyn Tool>>>,
    allowed_tools: &[String],
) -> anyhow::Result<()> {
    let normalized = allowed_tools
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if normalized.is_empty() || normalized.as_slice() == ["*"] {
        return Ok(());
    }
    if normalized.contains(&"*") {
        anyhow::bail!("allowed_tools must use '*' exclusively");
    }
    if normalized
        .iter()
        .all(|name| source.iter().any(|tool| tool.supports_name(name)))
    {
        return Ok(());
    }

    for tool in source.iter() {
        tool.refresh().await?;
    }
    let unresolved = normalized
        .into_iter()
        .filter(|name| !source.iter().any(|tool| tool.supports_name(name)))
        .collect::<Vec<_>>();
    if !unresolved.is_empty() {
        anyhow::bail!(
            "Allowed tools are not registered after dynamic discovery: {}",
            unresolved.join(", ")
        );
    }
    Ok(())
}

/// Queue depth for a sub-agent's steering channel.
///
/// This is a backpressure valve, not a rate limit: it is sized so that no
/// realistic operator or model ever reaches it, because a steering message is
/// an explicit, deliberate redirection of a running agent — a session with a
/// thousand of them pending has a runaway producer, not a busy one. When the
/// queue does fill, `sessions_spawn:steer` parks on `send().await` so the
/// producer is slowed rather than having messages dropped or the process
/// growing without bound. Each queued entry is one operator string, so the
/// worst-case memory held here is negligible next to the run's own history.
pub(crate) const STEER_CHANNEL_CAPACITY: usize = 1024;

/// Render a steer message as the user turn that gets injected into a sub-agent's
/// conversation.
///
/// Shared by both modes on purpose: task mode injects it in-process and process
/// mode injects it inside the worker, so a steered run reads identically in
/// `sessions_history` whichever way it was spawned.
pub(crate) fn steering_instruction(message: &str) -> String {
    format!("[Steering instruction from operator] {message}")
}

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
    compute_worker_capability_with_key(&session_worker_signing_key(), run_id, expiry_unix, manifest_json)
}
fn compute_worker_capability_with_key(
    key: &ring::hmac::Key,
    run_id: &str,
    expiry_unix: u64,
    manifest_json: &str,
) -> String {
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

    let tag = hmac::sign(key, &payload);
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

/// Recovery must never create or rotate the transport key, or substitute a
/// process-local fallback when the original signing material is unavailable.
fn existing_session_worker_signing_key() -> anyhow::Result<ring::hmac::Key> {
    if let Ok(secret) = std::env::var("SESSION_WORKER_SECRET") {
        if !secret.is_empty() {
            return Ok(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes()));
        }
    }
    let path = session_secret_path().context("worker recovery key location unavailable")?;
    let bytes = std::fs::read(path).context("existing worker recovery key unavailable")?;
    anyhow::ensure!(bytes.len() == 32, "existing worker recovery key has invalid length");
    Ok(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &bytes))
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
/// Mirrors `WitnessKeyring`'s convention: an explicit override env, else the
/// resolved config directory, else `$HOME/.openprx` (no `dirs` dependency).
fn session_secret_path() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("OPENPRX_SESSION_WORKER_SECRET_PATH") {
        return Some(std::path::PathBuf::from(explicit));
    }
    // Follows the resolved config directory so the secret minted by a
    // `--config-dir` run is the one its workers read back, and no run writes
    // key material into a configuration it was told not to touch.
    if let Some(config_dir) = crate::config::process_config_dir() {
        return Some(config_dir.join("keys").join("session_worker.secret"));
    }
    crate::config::default_openprx_dir().map(|dir| dir.join("keys").join("session_worker.secret"))
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
/// Uses the admitted shared turn loop even when no tools are registered.
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

/// Dropping an outer steering future must not detach its active inner loop.
struct AbortLoopOnDrop(tokio::task::AbortHandle);

impl Drop for AbortLoopOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct SubAgentTaskResult {
    segment_usage: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    output: String,
    history_commit_len: usize,
}

fn settle_sub_agent_task_result(
    mut settled_segment_usage: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
    result: anyhow::Result<SubAgentTaskResult>,
) -> anyhow::Result<SubAgentTaskResult> {
    result.map(|mut result| {
        settled_segment_usage.append(&mut result.segment_usage);
        result.segment_usage = settled_segment_usage;
        if result.output.trim().is_empty() {
            result.output = "[Sub-agent produced no output]".to_string();
        }
        result
    })
}

async fn run_sub_agent_task(
    task: &str,
    provider: Arc<dyn Provider>,
    provider_name: &str,
    model: &str,
    temperature: f64,
    tools: Option<Arc<Vec<Box<dyn Tool>>>>,
    system_prompt: &str,
    security: Arc<SecurityPolicy>,
    mut steer_rx: tokio::sync::mpsc::Receiver<String>,
    history_out: Arc<RwLock<Vec<HistoryEntry>>>,
    on_delta: Option<tokio::sync::mpsc::Sender<String>>,
    on_tool_call: Option<tokio::sync::mpsc::Sender<crate::agent::turn_contracts::ToolCallNotification>>,
    approval_resolver: Option<Arc<dyn crate::agent::turn_contracts::ApprovalResolver>>,
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
    // Liveness beat for this run, minted by the spawner and already parented on
    // the spawner's own beat. Installed as the ambient beat of every loop
    // iteration below, so the run's registry row observes exactly the events
    // `crate::agent::idle` already defines as progress.
    progress: Arc<crate::agent::idle::ProgressBeat>,
) -> anyhow::Result<SubAgentTaskResult> {
    let operator_authority = SPAWN_EXECUTION_CONTEXT
        .try_with(|context| context.operator_authority.clone())
        .ok()
        .flatten();
    if let Some(authority) = &operator_authority {
        authority.verify_execution(run_id.as_deref(), task, &provider, provider_name, model)?;
    }
    let mut operator_segments = operator_authority
        .as_ref()
        .map(|authority| authority.segments())
        .transpose()?;
    let ordinary_authority = SPAWN_EXECUTION_CONTEXT
        .try_with(|context| context.ordinary_authority.clone())
        .ok()
        .flatten();
    let mut ordinary_segments = ordinary_authority
        .as_ref()
        .map(|authority| -> anyhow::Result<_> {
            anyhow::ensure!(
                Arc::ptr_eq(&authority.binding().provider(), &provider)
                    && authority.binding().provider_name() == provider_name
                    && authority.binding().model() == model,
                "ordinary task provider differs from issued source"
            );
            authority.segments()
        })
        .transpose()?;
    let mut child_segments = SPAWN_EXECUTION_CONTEXT
        .try_with(|context| -> anyhow::Result<_> {
            let Some(grant) = context.incoming_grant.as_ref() else {
                return Ok(None);
            };
            let binding = context
                .provider_binding
                .as_ref()
                .context("child provider binding missing")?;
            let receipt = context.input_receipt.as_ref().context("child input receipt missing")?;
            anyhow::ensure!(
                binding.provider_name() == provider_name && binding.model() == model && receipt.input() == task,
                "child execution inputs differ from their admitted source"
            );
            binding.source_proof(grant, receipt, &provider)?;
            Ok(Some((
                Arc::clone(grant),
                Arc::clone(binding),
                crate::runtime::child_source::ChildSegmentSequence::new(Arc::clone(grant), Arc::clone(receipt))?,
            )))
        })
        .unwrap_or(Ok(None))?;
    anyhow::ensure!(
        usize::from(child_segments.is_some())
            + usize::from(ordinary_segments.is_some())
            + usize::from(operator_segments.is_some())
            == 1,
        "task execution requires exactly one admitted source authority"
    );
    let child_usage = SPAWN_EXECUTION_CONTEXT
        .try_with(|context| context.terminal_usage.clone())
        .ok()
        .flatten();
    anyhow::ensure!(
        (child_segments.is_none() && ordinary_segments.is_none() && operator_segments.is_none())
            || child_usage.is_some(),
        "child segment usage owner missing"
    );
    let child_fabric = child_segments
        .as_ref()
        .map(|(grant, _, _)| grant.source_fabric().clone())
        .or_else(|| {
            ordinary_authority
                .as_ref()
                .map(|authority| authority.publication().source_fabric())
        })
        .or_else(|| operator_authority.as_ref().map(|authority| authority.fabric().clone()));
    let mut child_source = if let Some((grant, binding, segments)) = child_segments.as_mut() {
        let receipt = segments
            .record(child_fabric.as_ref().context("child segment fabric missing")?, task)
            .await?;
        let proof = Arc::new(binding.source_proof(grant, &receipt, &provider)?);
        let execution_context = receipt.execution_context(&proof)?;
        Some((proof, execution_context))
    } else if let Some(segments) = ordinary_segments.as_mut() {
        Some(segments.record(task).await?)
    } else if let Some(segments) = operator_segments.as_mut() {
        Some(segments.record(task).await?)
    } else {
        None
    };
    // Empty tool registries still enter the same source-bound decision and
    // terminal lifecycle. No direct provider path can bypass admission.
    let tools_registry = tools.unwrap_or_else(|| Arc::new(Vec::new()));

    let child_config = child_segments
        .as_ref()
        .map(|(grant, _, _)| Arc::clone(&grant.generation().effective))
        .or_else(|| {
            ordinary_authority
                .as_ref()
                .map(|authority| Arc::clone(&authority.publication().generation().effective))
        })
        .or_else(|| {
            operator_authority
                .as_ref()
                .map(|authority| Arc::clone(&authority.generation().effective))
        });
    let mut settled_segment_usage = Vec::new();
    let child_service = if let Some((grant, _, _)) = child_segments.as_ref() {
        Some(
            crate::runtime::turn_publication::ChildDecisionPublication {
                allowed_tools: None,
                grant: Arc::clone(grant),
                registry: Arc::clone(&tools_registry),
                observer: Arc::new(NoopObserver),
            }
            .publish()
            .await?,
        )
    } else if let Some(authority) = &ordinary_authority {
        let owner = crate::runtime::turn_cleanup::current_tracker().context("ordinary task cleanup owner missing")?;
        let excluded = SPAWN_EXECUTION_CONTEXT
            .try_with(|context| context.channel_excluded_tools.clone())
            .context("ordinary task execution scope missing")?;
        let service = authority.publication().task_service(
            Arc::clone(authority),
            Arc::clone(&tools_registry),
            authority.allowed_tools(),
            &excluded,
        )?;
        owner.retain_child_service(Arc::clone(&service))?;
        Some(service)
    } else if let Some(authority) = &operator_authority {
        let owner = crate::runtime::turn_cleanup::current_tracker().context("operator task cleanup owner missing")?;
        let service = authority
            .publication()
            .task_service(Arc::clone(authority), Arc::clone(&tools_registry))?;
        owner.retain_child_service(Arc::clone(&service))?;
        Some(service)
    } else {
        None
    };
    let ordinary_runtime = ordinary_authority
        .as_ref()
        .map(|authority| authority.publication().loop_runtime().cloned())
        .or_else(|| {
            operator_authority
                .as_ref()
                .map(|authority| authority.publication().loop_runtime().cloned())
        })
        .transpose()?;

    // --- Agentic loop with steering support ---
    let mut history: Vec<ChatMessage> = Vec::new();

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
        if let Some(authority) = &operator_authority {
            authority.verify_execution(run_id.as_deref(), task, &provider, provider_name, model)?;
        }
        let cancel_token = CancellationToken::new();

        // Clone everything needed for the inner spawned task.
        // We move `history` into the task and get it back after completion.
        let initial_system = (child_source.is_some() || history.first().is_none_or(|message| message.role != "system"))
            .then(|| system_prompt.to_owned());
        let initial_task = history.is_empty().then(|| task.to_owned());
        let mut task_history = history;
        let provider_instance = provider.clone();
        let child_source_owned = child_source.clone().context("task segment source missing")?;
        let ordinary_runtime_owned = ordinary_runtime.clone();
        let child_service_owned = child_service.clone().context("task segment service missing")?;
        let child_config_owned = child_config.clone().context("task segment configuration missing")?;
        let child_fabric_owned = child_fabric.clone().context("task segment fabric missing")?;
        let child_usage_owned = child_usage.clone();
        let provider_name_owned = provider_name.to_string();
        let model_name = model.to_string();
        let temperature_value = temperature;
        let tools_registry_owned = tools_registry.clone();
        let workspace_dir_owned = child_config_owned.workspace_dir.clone();
        let multimodal_config_owned = child_config_owned.multimodal.clone();
        let cancel_token_owned = cancel_token.clone();
        let security = security.clone();
        // Clone the event-bridge senders per iteration so they survive across
        // steer-driven loop restarts (the inner spawn consumes its clones; the
        // originals stay owned by this outer loop). When `None`, the loop runs
        // silently exactly as before (channels/gateway behaviour unchanged).
        //
        // NOTE: the agent stays `silent = true` regardless. `silent` only gates
        // the loop's *direct* `print!` to stdout (turn_engine.rs); a background
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
        // Task-locals do not cross `tokio::spawn`, so this run's beat is
        // re-installed inside the child task. It was minted on the *spawning*
        // task and is parented on the spawner's beat, so a parent legitimately
        // blocked on a visibly-working sub-agent is not mistaken for wedged —
        // and the same events are what make this run's own silence visible.
        //
        // MUTATION GUARD: reading `current_beat()` here instead reads the beat
        // of the spawned monitor task, which has none, and both properties are
        // silently lost.
        let run_beat = Arc::clone(&progress);
        let cleanup_parent = crate::runtime::turn_cleanup::current_tracker();
        let (cleanup_ack, cleanup_confirmed) = tokio::sync::oneshot::channel();
        let (segment_admitted, mut segment_admission) = tokio::sync::oneshot::channel();
        let loop_cleanup =
            crate::runtime::turn_cleanup::CleanupScope::new(cleanup_parent.as_ref(), CancellationToken::new())?
                .with_acknowledgement(Some(cleanup_ack));
        let mut loop_handle = tokio::spawn(crate::agent::idle::scope_beat(Some(run_beat), async move {
            // Carry the segment proof across the spawn boundary explicitly.
            let child_source = child_source_owned;
            let child_service = child_service_owned;
            let (observer, hooks): (Arc<dyn crate::observability::Observer>, Arc<HookManager>) =
                match ordinary_runtime_owned {
                    Some(runtime) => (runtime.observer, runtime.hooks),
                    None => (
                        Arc::new(NoopObserver),
                        Arc::new(HookManager::new(workspace_dir_owned.clone())),
                    ),
                };
            let context = &child_source.1;
            let envelope = &context.envelope;
            let scope_ctx = ScopeContext::from_execution_context(
                &security,
                context,
                envelope.recipient.as_deref().unwrap_or_default(),
            );
            let memory_owned = child_fabric_owned.memory();
            // Only build an `ApprovalManager` when a resolver is attached. Under
            // permission-model Phase 1 the unified `SecurityPolicy::decide`
            // (supervised: act-tools → Ask) is what routes a call into the
            // resolver suspend path; the manager is just the UI/grant layer. Built
            // from the live policy's autonomy level so `Full` / `ReadOnly` never
            // suspend.
            let approval_manager = approval_resolver_iter
                .as_ref()
                .map(|_| crate::approval::ApprovalManager::new());
            let result = async {
                let child_turn = crate::runtime::child_turn::ChildSegmentTurn::admit(
                    child_service,
                    child_source.clone(),
                    child_fabric_owned.clone(),
                    child_config_owned,
                    cleanup_parent.as_ref().context("child segment cleanup owner missing")?,
                    &cancel_token_owned,
                    child_usage_owned,
                )
                .await?
                .with_child_cleanup(cleanup_confirmed);
                let _ = segment_admitted.send(());
                if child_turn.is_close()? {
                    loop_cleanup.run(async { Ok(()) }).await?;
                    let reply = child_turn
                        .close(&cancel_token_owned)
                        .await?
                        .context("child segment close disposition changed")?;
                    task_history.push(ChatMessage::assistant(reply.clone()));
                    if let Some(sender) = on_delta_iter.as_ref() {
                        let _ = sender.try_send(reply.clone());
                    }
                    return Ok(SubAgentTaskResult {
                        segment_usage: Vec::new(),
                        output: reply,
                        history_commit_len: task_history.len(),
                    });
                }
                if let Some(system) = initial_system {
                    let system = child_turn.prompt_with_skills(&system)?;
                    if let Some(previous) = task_history.first_mut().filter(|message| message.role == "system") {
                        *previous = ChatMessage::system(system);
                    } else {
                        task_history.insert(0, ChatMessage::system(system));
                    }
                }
                if let Some(task) = initial_task {
                    task_history.push(crate::agent::turn_contracts::inbound_user_message(&task));
                }
                child_turn.enrich_input(&mut task_history)?;
                let loop_outcome = loop_cleanup
                    .run(crate::agent::turn_engine::run_turn(
                        &mut task_history,
                        crate::agent::turn_request::TurnRequest {
                            channel_name: "sessions_spawn",
                            // chat event bridge: incremental loop output (v1.1a)
                            scope: Some(&scope_ctx),
                            mode: crate::agent::turn_contracts::ChatMode::default(),
                            silent: true,
                            expose_stay_silent: false,
                            multimodal: &multimodal_config_owned,
                            schedule: crate::agent::turn_request::TurnSchedule {
                                read_only_tool_concurrency_window: 2,
                                priority_scheduling_enabled: false,
                                low_priority_tool_names: vec![
                                    "sessions_spawn".to_string(),
                                    "delegate".to_string(),
                                    "cron".to_string(),
                                ],
                            },
                            compaction: Some(child_turn.compaction()),
                            decision: child_turn.routing(),
                        },
                        crate::agent::turn_request::TurnServices {
                            provider: provider_instance.as_ref(),
                            provider_name: &provider_name_owned,
                            model: &model_name,
                            temperature: temperature_value,
                            tools_registry: tools_registry_owned,
                            observer: observer.as_ref(),
                            hooks: hooks.as_ref(),
                            // silent — never print to the chat terminal (see note above)
                            approval: approval_manager.map(Arc::new),
                            approval_resolver: approval_resolver_iter,
                            runtime_adapter: Some(child_turn.adapter()),
                            memory: crate::agent::turn_request::ToolLoopMemory::new(
                                &memory_owned,
                                &workspace_dir_owned,
                                Some(
                                    DocumentIngestRuntime::from_scope(memory_owned.clone(), &scope_ctx)
                                        .into_nested_turn(),
                                ),
                            )
                            .with_event_fabric(child_fabric_owned),
                        },
                        crate::agent::turn_request::TurnDelivery {
                            cancellation: Some(cancel_token_owned.clone()),
                            on_delta: on_delta_iter,
                            on_tool_call: on_tool_call_iter,
                        },
                    ))
                    .await;
                let (outcome, trace) = match loop_outcome {
                    Ok(result) => result,
                    Err(error) => {
                        child_turn
                            .fail(
                                &error,
                                cancel_token_owned.is_cancelled()
                                    || crate::agent::turn_contracts::is_tool_loop_cancelled(&error),
                            )
                            .await?;
                        return Err(error);
                    }
                };
                let segment_usage = child_turn
                    .complete(&outcome, &trace)
                    .await?
                    .usage_settlement
                    .into_iter()
                    .collect();
                Ok(SubAgentTaskResult {
                    segment_usage,
                    output: outcome.into_text(),
                    history_commit_len: task_history.len(),
                })
            }
            .await;
            (task_history, result)
        }));
        let _loop_abort = AbortLoopOnDrop(loop_handle.abort_handle());

        // A queued steer may arrive immediately after the run is published.
        // Do not let it cancel this segment until terminal ownership exists:
        // otherwise admission can be dropped before a terminal guard is built,
        // and the next segment correctly refuses the unconfirmed predecessor.
        tokio::select! {
            loop_result = &mut loop_handle => {
                let (returned_history, result) = loop_result?;
                history = returned_history;
                *history_out.write().await = chat_messages_to_history(&history);
                return settle_sub_agent_task_result(settled_segment_usage, result);
            }
            admitted = &mut segment_admission => {
                if admitted.is_err() {
                    let (returned_history, result) = loop_handle.await?;
                    history = returned_history;
                    *history_out.write().await = chat_messages_to_history(&history);
                    return settle_sub_agent_task_result(settled_segment_usage, result);
                }
            }
        }

        // Race: loop completion vs steering message
        tokio::select! {
            loop_result = &mut loop_handle => {
                // Inner loop finished (naturally or via error)
                let (returned_history, result) = loop_result?;
                history = returned_history;
                // Write final history to shared store
                *history_out.write().await = chat_messages_to_history(&history);
                return settle_sub_agent_task_result(settled_segment_usage, result);
            },
            steer_opt = steer_rx.recv() => {
                match steer_opt {
                    Some(steer_msg) => {
                        // Cancel the running inner loop
                        cancel_token.cancel();
                        // Wait for the task to acknowledge cancellation and return history
                        let (returned_history, cancelled_result) = loop_handle.await?;
                        history = returned_history;
                        match cancelled_result {
                            Err(error) if error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>() => {
                                *history_out.write().await = chat_messages_to_history(&history);
                                return Err(error);
                            }
                            Ok(mut completed) => settled_segment_usage.append(&mut completed.segment_usage),
                            Err(_) => {}
                        }
                        // NeedsInput: the cancelled inner loop may have been parked
                        // on a suspended approval (`resolve()` future dropped on
                        // cancel). Its registry status can be a zombie
                        // `AwaitingInput` if the resolver's `Drop` `try_write`
                        // restore was skipped under contention. We are about to
                        // re-run the loop, so deterministically restore `Running`
                        // here (async, authoritative) — never clobbering a terminal
                        // state set by a concurrent kill / timeout.
                        restore_running().await;
                        let segment_input = steering_instruction(&steer_msg);
                        if let Some((grant, binding, segments)) = child_segments.as_mut() {
                            let receipt = segments.record(
                                child_fabric.as_ref().context("child segment fabric missing")?,
                                &segment_input,
                            ).await?;
                            let proof = Arc::new(binding.source_proof(grant, &receipt, &provider)?);
                            let execution_context = receipt.execution_context(&proof)?;
                            child_source = Some((proof, execution_context));
                        } else if let Some(segments) = ordinary_segments.as_mut() {
                            child_source = Some(segments.record(&segment_input).await?);
                        } else if let Some(segments) = operator_segments.as_mut() {
                            child_source = Some(segments.record(&segment_input).await?);
                        }
                        // Inject the steering message as a user turn
                        tracing::info!("Sub-agent steering: injecting message");
                        history.push(ChatMessage::user(segment_input));
                        // Update shared history so callers can see the injected message
                        *history_out.write().await = chat_messages_to_history(&history);
                        // Loop continues — will re-enter with updated history
                    }
                    None => {
                        // Steer channel closed — no more steering; wait for natural completion
                        let (returned_history, result) = loop_handle.await?;
                        history = returned_history;
                        *history_out.write().await = chat_messages_to_history(&history);
                        return settle_sub_agent_task_result(settled_segment_usage, result);
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
    let config_dir = manifest
        .config_dir
        .to_str()
        .context("session-worker config directory must be valid UTF-8")?;
    Ok(vec![
        "--config-dir".to_string(),
        config_dir.to_string(),
        "session-worker".to_string(),
        "--task".to_string(),
        manifest.task.clone(),
        "--workspace".to_string(),
        manifest.workspace_dir.display().to_string(),
        "--memory-db".to_string(),
        manifest.memory_db_path.display().to_string(),
        "--tools".to_string(),
        tools_json,
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
        PROCESS_MEMORY_STRATEGY_HYBRID => anyhow::bail!(crate::config::HYBRID_PROCESS_MEMORY_UNAVAILABLE),
        other => anyhow::bail!(
            "Invalid sessions_spawn.process_memory_strategy '{other}'. Expected 'shared_fabric' or 'isolated_private'."
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_session_worker_manifest(
    run_id: &str,
    task: &str,
    provider_name: &str,
    model: &str,
    api_key: Option<&str>,
    route_hint: Option<&str>,
    route_source_provider: Option<&str>,
    route_credential: Option<&providers::credentials::CredentialRef>,
    route_uses_root_connection: bool,
    direct_uses_root_connection: bool,
    temperature: f64,
    worker_workspace: std::path::PathBuf,
    memory_db_path: std::path::PathBuf,
    memory_workspace_id: String,
    normalized_memory_strategy: &str,
    memory_backend: &str,
    shared_memory_db_path: std::path::PathBuf,
    worker_memory_db_path: std::path::PathBuf,
    config_dir: std::path::PathBuf,
    config_generation: &str,
    agent_id: Option<&str>,
    agent_system_prompt: Option<&str>,
    event_recording: MemoryEventRecording,
    allowed_tools: &[String],
    identity_dir: Option<String>,
    scope: Option<&SpawnScope>,
    lineage: &SpawnLineage,
    spawn_depth: usize,
    session_scope_key: &str,
    parent_run_id: Option<&str>,
    compaction_config: &AgentCompactionConfig,
    channel_excluded_tools: &[String],
    child_source: Option<(
        &crate::runtime::child_source::IssuedChildGrant,
        &crate::runtime::child_source::ChildInputReceipt,
        &crate::session_worker::protocol::WorkerProviderSelection,
    )>,
    source_launch: Option<(
        crate::runtime::operator_spawn::process::ProcessSourceLaunch,
        &ring::hmac::Key,
    )>,
) -> anyhow::Result<(WorkerManifest, String, u64)> {
    let mut manifest = WorkerManifest {
        parent_capability: None,
        capability_manifest: None,
        child_source: None,
        ordinary_source: None,
        operator_source: None,
        run_id: run_id.to_string(),
        task: task.to_string(),
        provider_name: provider_name.to_string(),
        model: model.to_string(),
        api_key: api_key.map(str::to_string),
        route_hint: route_hint.map(str::to_string),
        route_source_provider: route_source_provider.map(str::to_string),
        route_credential: route_credential.cloned(),
        route_uses_root_connection,
        direct_uses_root_connection,
        temperature,
        config_dir,
        config_generation: config_generation.to_string(),
        runtime_config_generation_id: scope.and_then(|ctx| ctx.config_generation_id),
        runtime_config_source_revision: scope.and_then(|ctx| ctx.config_source_revision.clone()),
        workspace_dir: worker_workspace,
        memory_db_path,
        memory_workspace_id: Some(memory_workspace_id),
        memory_strategy: Some(normalized_memory_strategy.to_string()),
        memory_backend: memory_backend.to_string(),
        shared_memory_db_path: Some(shared_memory_db_path),
        worker_memory_db_path: Some(worker_memory_db_path),
        agent_id: agent_id.map(str::to_string),
        persona_id: source_launch
            .as_ref()
            .and_then(|(launch, _)| launch.persona_id().map(str::to_owned)),
        memory_event_recording: event_recording,
        allowed_tools: allowed_tools.to_vec(),
        channel_excluded_tools: channel_excluded_tools.to_vec(),
        system_prompt: agent_system_prompt.map(str::to_string),
        identity_dir,
        scope_sender: scope.map(|ctx| ctx.sender.clone()),
        scope_channel: scope.map(|ctx| ctx.channel.clone()),
        scope_chat_type: scope.map(|ctx| ctx.chat_type.clone()),
        scope_chat_id: scope.map(|ctx| ctx.chat_id.clone()),
        owner_id: lineage.owner_id.clone(),
        owner_isolated: lineage.owner_isolated,
        topic_id: lineage.topic_id.clone(),
        parent_task_id: lineage.parent_task_id.clone(),
        source_message_event_id: lineage.source_message_event_id.clone(),
        spawn_depth,
        session_scope_key: session_scope_key.to_string(),
        parent_run_id: parent_run_id.map(str::to_string),
        compaction_config: Some(compaction_config.clone()),
    };

    if let Some((grant, receipt, selection)) = child_source {
        grant.bind_worker_manifest(&mut manifest, receipt, selection)?;
    }

    let capability_expiry = capability_now_unix().saturating_add(SESSION_WORKER_CAP_TTL_SECS);
    let sealed_capability = if let Some((launch, key)) = source_launch {
        launch.bind_worker_manifest(key, &mut manifest)?;
        compute_worker_capability_with_key(
            key,
            &manifest.run_id,
            capability_expiry,
            &manifest_signing_payload(&manifest)?,
        )
    } else {
        seal_worker_capability(&manifest, capability_expiry)?
    };
    manifest.parent_capability = Some(sealed_capability.clone());
    Ok((manifest, sealed_capability, capability_expiry))
}

#[derive(Debug)]
enum OwnedChildExit {
    Exited(std::process::ExitStatus),
    Terminated(String),
    TerminationFailed(String),
}

async fn hold_unresolved_child_ownership<T>(child: &tokio::process::Child, error: impl std::fmt::Display) -> T {
    tracing::error!(
        error = %error,
        "session-worker reap state is unresolved; retaining Child ownership and run slot"
    );
    let owned_child = child;
    loop {
        std::future::pending::<()>().await;
        let _ = owned_child.id();
    }
}

async fn start_kill_and_reap_direct_child(
    child: &mut tokio::process::Child,
) -> anyhow::Result<std::process::ExitStatus> {
    let start_kill_error = child.start_kill().err();
    let bounded_wait =
        tokio::time::timeout(std::time::Duration::from_secs(PROCESS_REAP_TIMEOUT_SECS), child.wait()).await;
    let mut bounded_wait_timed_out = false;
    let status = match bounded_wait {
        Ok(Ok(status)) => status,
        Ok(Err(wait_error)) => {
            let error = match start_kill_error {
                Some(start_error) => anyhow::anyhow!(
                    "failed to start direct-child kill: {start_error}; direct-child wait failed: {wait_error}"
                ),
                None => wait_error.into(),
            };
            return hold_unresolved_child_ownership(child, error).await;
        }
        Err(_) => {
            bounded_wait_timed_out = true;
            match child.wait().await {
                Ok(status) => status,
                Err(wait_error) => {
                    let error = start_kill_error.as_ref().map_or_else(
                        || {
                            anyhow::anyhow!(
                                "direct child was not reaped within {PROCESS_REAP_TIMEOUT_SECS}s and the continuing owner wait failed: {wait_error}"
                            )
                        },
                        |start_error| {
                            anyhow::anyhow!(
                                "failed to start direct-child kill: {start_error}; direct child was not reaped within {PROCESS_REAP_TIMEOUT_SECS}s and the continuing owner wait failed: {wait_error}"
                            )
                        },
                    );
                    return hold_unresolved_child_ownership(child, error).await;
                }
            }
        }
    };
    if let Some(start_error) = start_kill_error {
        if bounded_wait_timed_out {
            anyhow::bail!(
                "failed to start direct-child kill: {start_error}; direct child was not reaped within {PROCESS_REAP_TIMEOUT_SECS}s, but the continuing owner wait later confirmed reap"
            );
        }
        anyhow::bail!("failed to start direct-child kill before confirmed reap: {start_error}");
    }
    Ok(status)
}

async fn wait_for_reap_after_group_termination(
    child: &mut tokio::process::Child,
    #[cfg(test)] inject_wait_error: bool,
) -> anyhow::Result<std::process::ExitStatus> {
    #[cfg(test)]
    if inject_wait_error {
        return hold_unresolved_child_ownership(child, "injected post-termination child.wait error").await;
    }
    match child.wait().await {
        Ok(status) => Ok(status),
        Err(error) => hold_unresolved_child_ownership(child, error).await,
    }
}

async fn terminate_owned_child(
    child: &mut tokio::process::Child,
    process_group: &mut OwnedProcessGroup,
) -> anyhow::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        if let Err(group_error) = process_group.terminate() {
            process_group.disarm();
            let status = start_kill_and_reap_direct_child(child).await.map_err(|direct_error| {
                anyhow::anyhow!(
                    "failed to kill session-worker process group: {group_error}; direct-child fallback failed: {direct_error}"
                )
            })?;
            #[cfg(not(test))]
            let _ = status;
            #[cfg(test)]
            process_group.observe_reap(status);
            return Err(group_error);
        }
    }

    #[cfg(not(unix))]
    {
        process_group.disarm();
        return start_kill_and_reap_direct_child(child).await;
    }

    let status = wait_for_reap_after_group_termination(
        child,
        #[cfg(test)]
        false,
    )
    .await?;
    #[cfg(test)]
    process_group.observe_reap(status);
    Ok(status)
}

async fn wait_for_owned_process(
    child: &mut tokio::process::Child,
    process_group: &mut OwnedProcessGroup,
    process_control: &ProcessRunControl,
    #[cfg(test)] inject_wait_error: bool,
) -> anyhow::Result<OwnedChildExit> {
    #[cfg(test)]
    if inject_wait_error {
        relinquish_process_group_after_leader_exit(process_group);
        return hold_unresolved_child_ownership(child, "injected child.wait error").await;
    }
    tokio::select! {
        biased;
        status = child.wait() => match status {
            Ok(status) => {
                #[cfg(test)]
                process_group.observe_reap(status);
                Ok(OwnedChildExit::Exited(status))
            },
            Err(error) => {
                relinquish_process_group_after_leader_exit(process_group);
                hold_unresolved_child_ownership(child, error).await
            }
        },
        reason = process_control.termination_requested() => {
            match terminate_owned_child(child, process_group).await {
                Ok(_) => Ok(OwnedChildExit::Terminated(reason)),
                Err(error) => Ok(OwnedChildExit::TerminationFailed(error.to_string())),
            }
        }
    }
}

enum ProcessWorkerOutcome {
    UnverifiedStartupFailure(crate::session_worker::startup_failure::WorkerOutput),
    VerifiedStartupFailure(WorkerResult),
    OperatorVerifiedFinished(process_terminal::operator::VerifiedOperatorWorkerCompletion),
    OperatorInterrupted {
        detail: String,
        usage: process_terminal::RecoveredWorkerUsage,
    },
    Finished(WorkerResult),
    VerifiedFinished(process_terminal::VerifiedWorkerCompletion),
    OrdinaryVerifiedFinished(process_terminal::ordinary::VerifiedOrdinaryWorkerCompletion),
    OrdinaryInterrupted {
        detail: String,
        usage: process_terminal::RecoveredWorkerUsage,
    },
    Terminated(String),
    TerminationFailed(String),
    /// The worker's stdout reached EOF without a `WorkerResult` line.
    ///
    /// This is the process-mode half of "the run died where the parent cannot
    /// see it": an OOM kill, a `SIGKILL`, or a segfault leaves the parent with
    /// nothing but a closed pipe. It is a distinct variant rather than a
    /// generic decode failure so the terminal status names the actual fault,
    /// and so that a run in this state can never be left `Running`.
    ExitedWithoutResult(String),
}

/// Human description of how a worker process ended, for the terminal status of
/// a run that produced no result.
fn describe_worker_exit(status: std::process::ExitStatus, stderr: &str) -> String {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let how = match (status.code(), signal) {
        (_, Some(signal)) => format!("killed by signal {signal}"),
        (Some(code), None) => format!("exit status {code}"),
        (None, None) => "exit status unknown".to_string(),
    };
    if stderr.is_empty() {
        how
    } else {
        let preview = stderr.chars().rev().take(400).collect::<String>();
        let preview = preview.chars().rev().collect::<String>();
        format!("{how}; stderr: {preview}")
    }
}

#[derive(Debug)]
struct OwnedProcessGroup {
    #[cfg(unix)]
    pgid: i32,
    armed: bool,
    #[cfg(test)]
    termination_calls: Option<Arc<std::sync::atomic::AtomicUsize>>,
    #[cfg(test)]
    skip_signal: bool,
    #[cfg(test)]
    protocol_diagnostic: Option<Arc<parking_lot::Mutex<TestWorkerProtocolDiagnostic>>>,
}

impl OwnedProcessGroup {
    fn from_child(child: &tokio::process::Child) -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            let pgid = child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .filter(|pid| *pid > 0)
                .ok_or_else(|| anyhow::anyhow!("owned session-worker has no valid process group"))?;
            Ok(Self {
                pgid,
                armed: true,
                #[cfg(test)]
                termination_calls: None,
                #[cfg(test)]
                skip_signal: false,
                #[cfg(test)]
                protocol_diagnostic: None,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = child;
            Ok(Self {
                armed: true,
                #[cfg(test)]
                termination_calls: None,
                #[cfg(test)]
                skip_signal: false,
                #[cfg(test)]
                protocol_diagnostic: None,
            })
        }
    }

    #[allow(unsafe_code)]
    fn terminate(&mut self) -> anyhow::Result<()> {
        if !self.armed {
            return Ok(());
        }
        #[cfg(test)]
        if let Some(calls) = &self.termination_calls {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.skip_signal {
                self.armed = false;
                return Ok(());
            }
        }
        #[cfg(unix)]
        {
            // SAFETY: `pgid` is captured from a child spawned with
            // `process_group(0)` and is never exposed outside its owner.
            let rc = unsafe { libc::killpg(self.pgid, libc::SIGKILL) };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(anyhow::anyhow!(
                        "failed to kill session-worker process group {}: {error}",
                        self.pgid
                    ));
                }
            }
        }
        self.armed = false;
        Ok(())
    }

    #[cfg(test)]
    fn observe_reap(&self, status: std::process::ExitStatus) {
        if let Some(diagnostic) = &self.protocol_diagnostic {
            let mut diagnostic = diagnostic.lock();
            diagnostic.reap_confirmed = true;
            diagnostic.exit_code = status.code();
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt as _;
                diagnostic.exit_signal = status.signal();
            }
        }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }

    #[cfg(test)]
    const fn test_stub(termination_calls: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Self {
            #[cfg(unix)]
            pgid: 1,
            armed: true,
            termination_calls: Some(termination_calls),
            skip_signal: true,
            protocol_diagnostic: None,
        }
    }

    #[cfg(test)]
    fn observe_termination_calls(&mut self, termination_calls: Arc<std::sync::atomic::AtomicUsize>) {
        self.termination_calls = Some(termination_calls);
    }
}

/// Steering plumbing handed to the owner of a session-worker child process.
struct WorkerSteerPipe {
    steer_rx: tokio::sync::mpsc::Receiver<String>,
    history: Arc<RwLock<Vec<HistoryEntry>>>,
    run_id: String,
}

enum ProcessOutputDrain {
    Finished { stdout: Vec<u8>, stderr: Vec<u8> },
    TerminationFailed(String),
}

enum OwnedProcessPhase {
    Exited {
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Terminated(String),
    TerminationFailed(String),
}

async fn drain_process_output_after_leader_exit(
    stdout_task: tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>,
    stderr_task: tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>,
    process_control: &ProcessRunControl,
    drain_timeout: std::time::Duration,
) -> anyhow::Result<ProcessOutputDrain> {
    let stdout_abort = stdout_task.abort_handle();
    let stderr_abort = stderr_task.abort_handle();
    let drain = async move {
        let stdout = stdout_task.await??;
        let stderr = stderr_task.await??;
        Ok::<_, anyhow::Error>(ProcessOutputDrain::Finished { stdout, stderr })
    };
    tokio::pin!(drain);

    tokio::select! {
        biased;
        output = &mut drain => output,
        reason = process_control.termination_requested() => {
            stdout_abort.abort();
            stderr_abort.abort();
            Ok(ProcessOutputDrain::TerminationFailed(format!(
                "termination requested after session-worker leader exit ({reason}); process group ownership was already relinquished"
            )))
        }
        () = tokio::time::sleep(drain_timeout) => {
            stdout_abort.abort();
            stderr_abort.abort();
            anyhow::bail!(
                "session-worker descendants held output pipes open after leader exit for {}s",
                drain_timeout.as_secs_f64()
            )
        }
    }
}

const fn relinquish_process_group_after_leader_exit(process_group: &mut OwnedProcessGroup) {
    // Once wait() has reaped the leader, the numeric PGID is no longer a safe
    // capability: the group may be empty and the id may be reused immediately.
    process_group.disarm();
}

async fn run_owned_child_phase(
    child: &mut tokio::process::Child,
    process_group: &mut OwnedProcessGroup,
    stdout_task: tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>,
    stderr_task: tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>,
    process_control: &ProcessRunControl,
) -> anyhow::Result<OwnedProcessPhase> {
    let process_exit = match wait_for_owned_process(
        child,
        process_group,
        process_control,
        #[cfg(test)]
        false,
    )
    .await
    {
        Ok(exit) => exit,
        Err(error) => {
            stdout_task.abort();
            stderr_task.abort();
            return Err(error);
        }
    };

    match process_exit {
        OwnedChildExit::Exited(status) => {
            relinquish_process_group_after_leader_exit(process_group);
            match drain_process_output_after_leader_exit(
                stdout_task,
                stderr_task,
                process_control,
                std::time::Duration::from_secs(PROCESS_OUTPUT_DRAIN_TIMEOUT_SECS),
            )
            .await?
            {
                ProcessOutputDrain::Finished { stdout, stderr } => {
                    Ok(OwnedProcessPhase::Exited { status, stdout, stderr })
                }
                ProcessOutputDrain::TerminationFailed(error) => Ok(OwnedProcessPhase::TerminationFailed(error)),
            }
        }
        OwnedChildExit::Terminated(reason) => {
            stdout_task.abort();
            stderr_task.abort();
            Ok(OwnedProcessPhase::Terminated(reason))
        }
        OwnedChildExit::TerminationFailed(error) => {
            stdout_task.abort();
            stderr_task.abort();
            Ok(OwnedProcessPhase::TerminationFailed(error))
        }
    }
}

/// Aborts a background task when the owning scope unwinds.
///
/// The steer pump owns the child's `ChildStdin`; dropping this guard closes that
/// pipe, which is the worker's EOF signal that no further steering can arrive.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Forward steer messages to a running session-worker as line-delimited control
/// frames on its stdin, and mirror each delivered message into the run history.
///
/// Back-pressure, not loss, and deliberately without any wall-clock deadline: a
/// worker that is too busy to drain its stdin blocks this write, which fills the
/// bounded steer channel, which parks `sessions_send` — the same contract task
/// mode already has. A stuck target is the idle detector's problem, not a
/// timeout's.
async fn pump_worker_steer_frames(
    mut stdin: tokio::process::ChildStdin,
    mut steer_rx: tokio::sync::mpsc::Receiver<String>,
    history: Arc<RwLock<Vec<HistoryEntry>>>,
    run_id: String,
) {
    use tokio::io::AsyncWriteExt;

    while let Some(message) = steer_rx.recv().await {
        let frame = match serde_json::to_string(&WorkerControlFrame::Steer {
            message: message.clone(),
        }) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!(run_id = %run_id, "failed to encode sub-agent steer frame: {error}");
                continue;
            }
        };
        let write = async {
            stdin.write_all(frame.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(error) = write.await {
            // Broken pipe: the worker exited. Stop pumping; the run's terminal
            // commit drops the sender so no caller can park on it afterwards.
            tracing::debug!(run_id = %run_id, "sub-agent steer pipe closed: {error}");
            return;
        }
        // Recorded only after the frame is on the wire, so history reflects what
        // was actually delivered rather than what was merely queued.
        history.write().await.push(HistoryEntry {
            role: "user".to_string(),
            content: steering_instruction(&message),
            timestamp: Utc::now(),
        });
    }
}

/// Read one of the worker's output pipes to EOF, stamping the run's progress
/// beat on every arrival.
///
/// Bytes from the worker are the only liveness signal that crosses the process
/// boundary, which is why this replaced a plain `read_to_end`: the buffered
/// result is byte-identical, but the parent now learns *while* the worker is
/// working rather than only once it is gone. The event is recorded as
/// [`crate::agent::idle::ProgressKind::ChannelOutput`] — output handed to a
/// sink — rather than as a new kind, so there is exactly one definition of what
/// counts as progress in this runtime.
async fn drain_worker_stream<R>(
    stream: R,
    pipe: &'static str,
    progress: Option<Arc<crate::agent::idle::ProgressBeat>>,
) -> anyhow::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let mut limited = stream.take(MAX_SUBPROCESS_OUTPUT);
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut total: u64 = 0;
    loop {
        let read = limited.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if let Some(beat) = progress.as_ref() {
            beat.record(crate::agent::idle::ProgressKind::ChannelOutput);
        }
        match chunk.get(..read) {
            Some(bytes) => buffer.extend_from_slice(bytes),
            None => anyhow::bail!("session-worker {pipe} read reported more bytes than the buffer holds"),
        }
    }
    if total >= MAX_SUBPROCESS_OUTPUT {
        tracing::warn!(
            limit_bytes = MAX_SUBPROCESS_OUTPUT,
            pipe,
            "session-worker output reached size limit; output truncated"
        );
        buffer.extend_from_slice(b"\n[output truncated at 10MB]");
    }
    Ok(buffer)
}

async fn run_spawned_child_lifecycle(
    child: &mut tokio::process::Child,
    process_group: &mut OwnedProcessGroup,
    payload: &str,
    process_control: &ProcessRunControl,
    steer: Option<WorkerSteerPipe>,
    progress: Option<Arc<crate::agent::idle::ProgressBeat>>,
    #[cfg(test)] panic_before_wait: bool,
    #[cfg(test)] protocol: Option<&TestWorkerProtocolControl>,
) -> anyhow::Result<OwnedProcessPhase> {
    use tokio::io::AsyncWriteExt;

    // The manifest is line 1 of the worker's stdin. Unlike before, the pipe is
    // then kept open for the life of the run so steer frames can follow it.
    #[cfg(test)]
    let mut held_stdin = None;
    #[cfg(test)]
    let _steer_pump = if let Some(control) = protocol {
        assert!(payload.len() < 3072, "fixture manifest line exceeds stdin precondition");
        let mut pipe = child.stdin.take().context("worker protocol stdin missing")?;
        pipe.write_all(payload.as_bytes()).await?;
        pipe.write_all(b"\n").await?;
        pipe.flush().await?;
        match control.case {
            TestWorkerProtocolCase::HoldAfterThreeSteers => {
                for message in ["first steer", "second steer", "third steer"] {
                    let frame = serde_json::to_string(&WorkerControlFrame::Steer {
                        message: message.to_owned(),
                    })?;
                    assert!(frame.len() < 256, "fixture steer line exceeds stdin precondition");
                    pipe.write_all(frame.as_bytes()).await?;
                    pipe.write_all(b"\n").await?;
                }
                pipe.flush().await?;
                held_stdin = Some(pipe);
            }
            TestWorkerProtocolCase::CloseAfterManifest => drop(pipe),
        }
        None
    } else if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        steer.map(|steer| {
            AbortOnDrop(tokio::spawn(pump_worker_steer_frames(
                stdin,
                steer.steer_rx,
                steer.history,
                steer.run_id,
            )))
        })
    } else {
        None
    };
    #[cfg(not(test))]
    let _steer_pump = if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        steer.map(|steer| {
            AbortOnDrop(tokio::spawn(pump_worker_steer_frames(
                stdin,
                steer.steer_rx,
                steer.history,
                steer.run_id,
            )))
        })
    } else {
        None
    };

    let stdout_stream = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("session-worker stdout pipe was not configured"))?;
    let stderr_stream = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("session-worker stderr pipe was not configured"))?;

    let stdout_task = tokio::spawn(drain_worker_stream(
        stdout_stream,
        "stdout",
        progress.as_ref().map(Arc::clone),
    ));
    let stderr_task = tokio::spawn(drain_worker_stream(
        stderr_stream,
        "stderr",
        progress.as_ref().map(Arc::clone),
    ));

    #[cfg(test)]
    assert!(!panic_before_wait, "injected owned-child lifecycle panic");

    #[cfg(not(test))]
    {
        run_owned_child_phase(child, process_group, stdout_task, stderr_task, process_control).await
    }
    #[cfg(test)]
    {
        let phase = run_owned_child_phase(child, process_group, stdout_task, stderr_task, process_control).await?;
        #[cfg(test)]
        if let (Some(control), OwnedProcessPhase::Exited { status, stdout, stderr }) = (protocol, &phase) {
            let mut terminal = control.terminal.lock();
            anyhow::ensure!(terminal.is_none(), "duplicate worker terminal observation");
            *terminal = Some(TestWorkerProtocolTerminal {
                status: *status,
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                stdin_held_through_reap: held_stdin.is_some(),
            });
        }
        #[cfg(test)]
        drop(held_stdin);
        Ok(phase)
    }
}

async fn cleanup_owned_child_after_panic(
    child: &mut tokio::process::Child,
    process_group: &mut OwnedProcessGroup,
    #[cfg(test)] inject_try_wait_error: bool,
) -> anyhow::Result<()> {
    #[cfg(test)]
    if inject_try_wait_error {
        relinquish_process_group_after_leader_exit(process_group);
        return hold_unresolved_child_ownership(child, "injected panic-cleanup try_wait error").await;
    }
    match child.try_wait() {
        Ok(Some(status)) => {
            #[cfg(not(test))]
            let _ = status;
            #[cfg(test)]
            process_group.observe_reap(status);
            relinquish_process_group_after_leader_exit(process_group);
            Ok(())
        }
        Ok(None) => {
            terminate_owned_child(child, process_group).await?;
            Ok(())
        }
        Err(error) => {
            relinquish_process_group_after_leader_exit(process_group);
            hold_unresolved_child_ownership(child, error).await
        }
    }
}

fn cleanup_process_worker_workspace(run_id: &str, worker_workspace: &std::path::Path, keep_workspace: bool) {
    if !keep_workspace {
        if let Err(error) = std::fs::remove_dir_all(worker_workspace) {
            tracing::warn!(
                run_id,
                "Failed to cleanup worker workspace {}: {error}",
                worker_workspace.display()
            );
        }
    }
}

async fn recover_ordinary_interruption(
    fabric: &MemoryFabric,
    manifest: &crate::session_worker::protocol::WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    transport_key: &ring::hmac::Key,
    detail: String,
) -> anyhow::Result<ProcessWorkerOutcome> {
    if let Some(mut completion) =
        process_terminal::ordinary::recover_completion(fabric, manifest, generation, transport_key).await?
    {
        completion.confirm_reaped(&manifest.run_id)?;
        return Ok(ProcessWorkerOutcome::OrdinaryVerifiedFinished(completion));
    }
    let usage = process_terminal::ordinary::recover_partial_usage(fabric, manifest, generation, transport_key).await?;
    Ok(ProcessWorkerOutcome::OrdinaryInterrupted { detail, usage })
}

async fn recover_operator_interruption(
    fabric: &MemoryFabric,
    manifest: &crate::session_worker::protocol::WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    transport_key: &ring::hmac::Key,
    detail: String,
) -> anyhow::Result<ProcessWorkerOutcome> {
    if let Some(mut completion) =
        process_terminal::operator::recover_completion(fabric, manifest, generation, transport_key).await?
    {
        completion.confirm_reaped(&manifest.run_id)?;
        return Ok(ProcessWorkerOutcome::OperatorVerifiedFinished(completion));
    }
    let usage = process_terminal::operator::recover_partial_usage(fabric, manifest, generation, transport_key).await?;
    Ok(ProcessWorkerOutcome::OperatorInterrupted { detail, usage })
}

async fn run_sub_agent_process(
    run_id: &str,
    task: &str,
    provider_name: &str,
    model: &str,
    api_key: Option<&str>,
    route_hint: Option<&str>,
    route_source_provider: Option<&str>,
    route_credential: Option<&providers::credentials::CredentialRef>,
    route_uses_root_connection: bool,
    direct_uses_root_connection: bool,
    temperature: f64,
    workspace_root: &std::path::Path,
    worker_workspace_root: &std::path::Path,
    agent_identity_dir: Option<&str>,
    agent_system_prompt: Option<&str>,
    allowed_tools: &[String],
    keep_workspace: bool,
    scope: Option<&SpawnScope>,
    spawn_depth: usize,
    session_scope_key: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    lineage: &SpawnLineage,
    memory_strategy: &str,
    memory_backend: &str,
    config_dir: &std::path::Path,
    config_generation: &str,
    event_recording: MemoryEventRecording,
    compaction_config: &AgentCompactionConfig,
    channel_excluded_tools: &[String],
    process_control: &ProcessRunControl,
    steer_rx: tokio::sync::mpsc::Receiver<String>,
    history: Arc<RwLock<Vec<HistoryEntry>>>,
    progress: Arc<crate::agent::idle::ProgressBeat>,
    provider_selection: Option<&crate::session_worker::protocol::WorkerProviderSelection>,
    source_fabric: Option<&MemoryFabric>,
    recovery: Option<&process_terminal::ProcessRecovery>,
    source_launch: Option<crate::runtime::operator_spawn::process::ProcessSourceLaunch>,
    batch_id: Option<&str>,
    #[cfg(test)] test_worker_executable: Option<&TestWorkerExecutable>,
) -> anyhow::Result<ProcessWorkerOutcome> {
    let process_generation = source_launch.as_ref().map(|launch| Arc::clone(launch.generation()));
    let ordinary_generation = source_launch
        .as_ref()
        .filter(|launch| launch.is_ordinary())
        .map(|launch| Arc::clone(launch.generation()));
    let operator_generation = source_launch
        .as_ref()
        .filter(|launch| !launch.is_ordinary())
        .map(|launch| Arc::clone(launch.generation()));
    anyhow::ensure!(
        operator_generation.is_none() || batch_id.is_none(),
        "operator process cannot claim model batch provenance"
    );
    let source_transport_key = source_launch.as_ref().map(|_| session_worker_signing_key());
    if let Some(launch) = &source_launch {
        let fabric = source_fabric.context("process source fabric missing")?;
        let issued = launch.source_fabric();
        anyhow::ensure!(
            fabric.workspace_id() == issued.workspace_id() && Arc::ptr_eq(&fabric.memory(), &issued.memory()),
            "process source storage differs from captured launch"
        );
    }
    let execution = current_spawn_execution_context();
    let child_source = execution
        .as_ref()
        .and_then(|context| context.incoming_grant.as_deref())
        .map(|grant| {
            let receipt = execution
                .as_ref()
                .and_then(|context| context.input_receipt.as_deref())
                .context("process child input receipt is missing")?;
            grant.verify_receipt(receipt)?;
            let selection = provider_selection.context("process child provider selection missing")?;
            Ok::<_, anyhow::Error>((grant, receipt, selection))
        })
        .transpose()?;
    let logical_workspace_root = child_source.as_ref().map_or_else(
        || {
            process_generation.as_ref().map_or(workspace_root, |generation| {
                generation.effective.workspace_dir.as_path()
            })
        },
        |(grant, _, _)| grant.generation().effective.workspace_dir.as_path(),
    );
    if let Some((grant, _, _)) = child_source.as_ref() {
        let fabric = source_fabric.context("process child logical source fabric missing")?;
        anyhow::ensure!(
            fabric.workspace_id() == grant.source_fabric().workspace_id()
                && Arc::ptr_eq(&fabric.memory(), &grant.source_fabric().memory()),
            "process child source storage differs from issued grant"
        );
    }
    let worker_workspace = worker_workspace_root.join(run_id);
    #[cfg(all(test, unix))]
    {
        if let Some(artifact) = test_worker_executable.filter(|artifact| artifact.protocol.is_some()) {
            use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
            artifact.verify()?;
            let home = std::fs::canonicalize(&artifact.private_home)?;
            let root = std::fs::canonicalize(worker_workspace_root)?;
            anyhow::ensure!(
                root != home && root.starts_with(&home),
                "protocol workspace root must be inside owned private HOME"
            );
            for path in [&home, &root] {
                anyhow::ensure!(
                    std::fs::symlink_metadata(path)?.permissions().mode() & 0o777 == 0o700,
                    "protocol HOME and workspace root must be private"
                );
            }
            std::fs::DirBuilder::new().mode(0o700).create(&worker_workspace)?;
            anyhow::ensure!(
                std::fs::symlink_metadata(&worker_workspace)?.permissions().mode() & 0o777 == 0o700,
                "protocol worker workspace must be private at creation"
            );
        } else {
            std::fs::create_dir_all(&worker_workspace)?;
        }
    }
    #[cfg(any(not(test), not(unix)))]
    std::fs::create_dir_all(&worker_workspace)?;
    let shared_memory_db_path = shared_worker_memory_db_path(logical_workspace_root);
    let worker_memory_db_path = private_worker_memory_db_path(&worker_workspace);
    let normalized_memory_strategy = normalize_process_memory_strategy(memory_strategy)?;
    let (memory_db_path, memory_workspace_id) = match normalized_memory_strategy {
        PROCESS_MEMORY_STRATEGY_SHARED => (
            shared_memory_db_path.clone(),
            logical_workspace_root.to_string_lossy().to_string(),
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
    let (manifest, sealed_capability, capability_expiry) = build_session_worker_manifest(
        run_id,
        task,
        provider_name,
        model,
        api_key,
        route_hint,
        route_source_provider,
        route_credential,
        route_uses_root_connection,
        direct_uses_root_connection,
        temperature,
        worker_workspace.clone(),
        memory_db_path,
        memory_workspace_id,
        normalized_memory_strategy,
        memory_backend,
        shared_memory_db_path,
        worker_memory_db_path,
        config_dir.to_path_buf(),
        config_generation,
        agent_id,
        agent_system_prompt,
        event_recording,
        allowed_tools,
        identity_dir,
        scope,
        lineage,
        spawn_depth,
        session_scope_key,
        parent_run_id,
        compaction_config,
        channel_excluded_tools,
        child_source,
        source_launch.zip(source_transport_key.as_ref()),
    )?;

    if let Some(reason) = process_control.termination_reason() {
        cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
        return Ok(ProcessWorkerOutcome::Terminated(reason));
    }

    #[cfg(not(test))]
    let executable = std::env::current_exe()?;
    #[cfg(test)]
    let executable = if let Some(artifact) = test_worker_executable {
        artifact.verify()?;
        artifact.path.clone()
    } else {
        std::env::current_exe()?
    };
    #[cfg(test)]
    if let Some(observer) = test_worker_executable.and_then(|artifact| artifact.route_observer.as_ref()) {
        use sha2::{Digest as _, Sha256};
        let mut capture = observer.lock();
        capture.total = capture.total.saturating_add(1);
        if capture.first.is_none() {
            capture.first = Some(TestWorkerRouteObservation {
                run_id: manifest.run_id.clone(),
                parent_run_id: manifest.parent_run_id.clone(),
                owner_id: manifest.owner_id.clone(),
                source_event_id: manifest.source_message_event_id.clone(),
                provider: manifest.provider_name.clone(),
                model: manifest.model.clone(),
                route_hint: manifest.route_hint.clone(),
                route_source_provider: manifest.route_source_provider.clone(),
                route_credential: manifest.route_credential.clone(),
                key_sha256: manifest
                    .api_key
                    .as_ref()
                    .map(|key| Sha256::digest(key.as_bytes()).into()),
                route_uses_root_connection: manifest.route_uses_root_connection,
                direct_uses_root_connection: manifest.direct_uses_root_connection,
                child_source_present: manifest.child_source.is_some(),
                child_allowed_count: manifest.capability_manifest.as_ref().map(|scope| scope.allowed.len()),
            });
        }
    }
    let cli_args = build_session_worker_cli_args(&manifest)?;
    let mut command = tokio::process::Command::new(executable);
    #[cfg(test)]
    if let Some(artifact) = test_worker_executable {
        // The outer guarded runner remains responsible for OS filesystem/network
        // confinement. Do not launch this ignored AUT outside that guard.
        command
            .env_clear()
            .env("HOME", &artifact.private_home)
            .env("PATH", "/usr/bin:/bin");
    }
    command.env(SESSION_WORKER_CAP_ENV, &sealed_capability);
    command.env(SESSION_WORKER_CAP_EXPIRY_ENV, capability_expiry.to_string());
    command
        .args(cli_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    #[cfg(unix)]
    let mut prepared_manifest = crate::session_worker::manifest_reference::PreparedManifest::prepare(
        &manifest,
        &source_transport_key.clone().unwrap_or_else(session_worker_signing_key),
        capability_expiry,
    )?;
    #[cfg(unix)]
    let payload = prepared_manifest.line().to_owned();
    #[cfg(not(unix))]
    let payload = serde_json::to_string(&manifest)?;
    #[cfg(test)]
    let protocol = test_worker_executable.and_then(|artifact| artifact.protocol.as_deref());
    #[cfg(test)]
    if let Some(control) = protocol {
        control.claim()?;
        control.check_serialized_precondition(&payload)?;
    }
    let mut child = command.spawn()?;
    #[cfg(unix)]
    prepared_manifest.mark_spawned();
    #[cfg(test)]
    if let Some(control) = protocol {
        let mut diagnostic = control.diagnostic.lock();
        diagnostic.spawned = true;
        diagnostic.pid = child.id();
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let (pgid, start, class) = test_worker_process_identity(pid);
            diagnostic.actual_pgid = pgid;
            diagnostic.start_identity = start;
            diagnostic.identity_class = Some(class);
        }
    }
    // Ledger row for the worker process itself, carrying the process group so a
    // kill reaches anything the worker forks. Its parent resolves to the
    // sub-agent row, because this future runs inside that row's scope.
    #[cfg(unix)]
    let worker_pgid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0);
    #[cfg(not(unix))]
    let worker_pgid: Option<i32> = None;
    let mut worker_registration =
        crate::runtime::registry::register_process(&format!("session-worker {run_id}"), child.id(), worker_pgid);
    #[cfg(test)]
    if let Some(control) = protocol {
        control.diagnostic.lock().registry_id = Some(worker_registration.id().raw());
    }
    let mut process_group = match OwnedProcessGroup::from_child(&child) {
        Ok(group) => group,
        Err(error) => {
            let status = start_kill_and_reap_direct_child(&mut child)
                .await
                .map_err(|cleanup_error| anyhow::anyhow!("{error}; direct-child cleanup failed: {cleanup_error}"))?;
            #[cfg(not(test))]
            let _ = status;
            #[cfg(test)]
            if let Some(control) = protocol {
                control.observe_reap_status(status);
            }
            worker_registration.mark_reaped();
            #[cfg(test)]
            if let Some(control) = protocol {
                control.diagnostic.lock().registration_marked_reaped = true;
            }
            #[cfg(unix)]
            prepared_manifest.close().map_err(|cleanup_error| {
                anyhow::anyhow!(
                    "session-worker process group setup failed: {error}; snapshot cleanup failed: {cleanup_error}"
                )
            })?;
            if let Some(recovery) = recovery {
                recovery.confirm_exit(run_id)?;
            }
            cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
            return Err(error);
        }
    };
    #[cfg(test)]
    if let Some(control) = protocol {
        process_group.termination_calls = Some(Arc::clone(&control.termination_calls));
        process_group.protocol_diagnostic = Some(Arc::clone(&control.diagnostic));
    }
    let owned_phase = std::panic::AssertUnwindSafe(async {
        if let Some(recovery) = recovery {
            recovery
                .persist_launch(child.id().context("worker process id unavailable before launch")?)
                .await?;
        }
        if let Some(generation) = &ordinary_generation {
            process_terminal::ordinary::launch::persist(
                source_fabric.context("ordinary launch source storage missing")?,
                &manifest,
                generation,
                source_transport_key
                    .as_ref()
                    .context("ordinary launch transport key missing")?,
                child.id().context("ordinary worker PID unavailable before launch")?,
                batch_id,
            )
            .await?;
        }
        if let Some(generation) = &operator_generation {
            process_terminal::operator::launch::persist(
                source_fabric.context("operator launch source storage missing")?,
                &manifest,
                generation,
                source_transport_key
                    .as_ref()
                    .context("operator launch transport key missing")?,
                child.id().context("operator worker PID unavailable before launch")?,
            )
            .await?;
        }
        #[cfg(test)]
        if let Some(control) = protocol {
            if operator_generation.is_some() {
                control.diagnostic.lock().launch_persisted = true;
            }
            anyhow::ensure!(
                operator_generation.is_some()
                    && manifest.operator_source.is_some()
                    && manifest.memory_event_recording.enabled
                    && manifest.memory_event_recording.record_user_messages,
                "stdin protocol control requires a recorded operator launch"
            );
            let artifact = test_worker_executable.context("worker protocol artifact missing")?;
            let home = std::fs::canonicalize(&artifact.private_home)?;
            let config_dir = std::fs::canonicalize(&manifest.config_dir)?;
            anyhow::ensure!(
                config_dir != home && config_dir.starts_with(&home),
                "protocol config must be inside owned private HOME"
            );
            let config_path = config_dir.join("config.toml");
            anyhow::ensure!(
                std::fs::symlink_metadata(&config_path)?.file_type().is_file(),
                "protocol config must be a regular file"
            );
            std::fs::remove_file(config_path)?;
        }
        run_spawned_child_lifecycle(
            &mut child,
            &mut process_group,
            &payload,
            process_control,
            Some(WorkerSteerPipe {
                steer_rx,
                history,
                run_id: run_id.to_string(),
            }),
            Some(progress),
            #[cfg(test)]
            false,
            #[cfg(test)]
            protocol,
        )
        .await
    })
    .catch_unwind()
    .await;
    // Confirm the leader's reap only on branches with actual cleanup evidence.
    // Failed cleanup keeps the process ledger row and workspace available for
    // recovery; a returned lifecycle result alone is not acknowledgement.
    let owned_phase = match owned_phase {
        Ok(Ok(phase)) => phase,
        Ok(Err(error)) => {
            let cleanup = cleanup_owned_child_after_panic(
                &mut child,
                &mut process_group,
                #[cfg(test)]
                false,
            )
            .await;
            #[cfg(test)]
            if let Some(control) = protocol {
                let mut diagnostic = control.diagnostic.lock();
                diagnostic.lifecycle_class = Some("lifecycle_error");
                diagnostic.panic_cleanup_class = Some(match &cleanup {
                    Ok(()) => "cleanup_returned_ok",
                    Err(error) => test_worker_cleanup_error_class(error),
                });
            }
            cleanup.map_err(|cleanup_error| {
                anyhow::anyhow!("session-worker lifecycle failed: {error}; cleanup failed: {cleanup_error}")
            })?;
            worker_registration.mark_reaped();
            #[cfg(test)]
            if let Some(control) = protocol {
                control.diagnostic.lock().registration_marked_reaped = true;
            }
            #[cfg(unix)]
            prepared_manifest.close().map_err(|cleanup_error| {
                anyhow::anyhow!("session-worker lifecycle failed: {error}; snapshot cleanup failed: {cleanup_error}")
            })?;
            if let Some(recovery) = recovery {
                recovery.confirm_exit(run_id)?;
            }
            cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
            return Err(error);
        }
        Err(_) => {
            let cleanup = cleanup_owned_child_after_panic(
                &mut child,
                &mut process_group,
                #[cfg(test)]
                false,
            )
            .await;
            #[cfg(test)]
            if let Some(control) = protocol {
                let mut diagnostic = control.diagnostic.lock();
                diagnostic.lifecycle_class = Some("monitor_panic");
                diagnostic.panic_cleanup_class = Some(match &cleanup {
                    Ok(()) => "cleanup_returned_ok",
                    Err(error) => test_worker_cleanup_error_class(error),
                });
            }
            cleanup.map_err(|error| anyhow::anyhow!("{PROCESS_OWNER_PANICKED_PREFIX}; cleanup failed: {error}"))?;
            worker_registration.mark_reaped();
            #[cfg(test)]
            if let Some(control) = protocol {
                control.diagnostic.lock().registration_marked_reaped = true;
            }
            #[cfg(unix)]
            prepared_manifest.close().map_err(|cleanup_error| {
                anyhow::anyhow!("{PROCESS_OWNER_PANICKED_PREFIX}; snapshot cleanup failed: {cleanup_error}")
            })?;
            if let Some(recovery) = recovery {
                recovery.confirm_exit(run_id)?;
            }
            cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
            anyhow::bail!("{PROCESS_OWNER_PANICKED_PREFIX} while owning session-worker child");
        }
    };
    let (status, stdout, stderr) = match owned_phase {
        OwnedProcessPhase::Exited { status, stdout, stderr } => {
            worker_registration.mark_reaped();
            #[cfg(test)]
            if let Some(control) = protocol {
                control.diagnostic.lock().registration_marked_reaped = true;
            }
            #[cfg(unix)]
            prepared_manifest.close()?;
            if let Some(recovery) = recovery {
                recovery.confirm_exit(run_id)?;
            }
            (status, stdout, stderr)
        }
        OwnedProcessPhase::Terminated(reason) => {
            worker_registration.mark_reaped();
            #[cfg(test)]
            if let Some(control) = protocol {
                control.diagnostic.lock().registration_marked_reaped = true;
            }
            #[cfg(unix)]
            prepared_manifest.close()?;
            if let Some(recovery) = recovery {
                recovery.confirm_exit(run_id)?;
            }
            if let Some(generation) = &ordinary_generation {
                let recovered = recover_ordinary_interruption(
                    source_fabric.context("ordinary interrupted source storage missing")?,
                    &manifest,
                    generation,
                    source_transport_key
                        .as_ref()
                        .context("ordinary interruption transport key missing")?,
                    reason,
                )
                .await;
                cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
                return recovered;
            }
            if let Some(generation) = &operator_generation {
                let recovered = recover_operator_interruption(
                    source_fabric.context("operator interrupted source storage missing")?,
                    &manifest,
                    generation,
                    source_transport_key
                        .as_ref()
                        .context("operator interruption transport key missing")?,
                    reason,
                )
                .await;
                cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
                return recovered;
            }
            cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
            return Ok(ProcessWorkerOutcome::Terminated(reason));
        }
        OwnedProcessPhase::TerminationFailed(error) => {
            tracing::warn!(
                run_id,
                "worker cleanup unconfirmed; retaining process record and workspace"
            );
            return Ok(ProcessWorkerOutcome::TerminationFailed(error));
        }
    };
    let stdout_raw = String::from_utf8_lossy(&stdout).trim().to_string();
    let stderr_raw = String::from_utf8_lossy(&stderr).trim().to_string();
    let outcome = classify_worker_output(status, &stdout_raw, &stderr_raw);
    #[cfg(all(test, unix))]
    if let (Some(artifact), Ok(ProcessWorkerOutcome::Finished(result))) = (test_worker_executable, &outcome) {
        if !result.success && result.completion_evidence.is_none() {
            if let Some(error) = result.error.as_deref() {
                // Diagnostic only for the explicitly attested local AUT. Keep
                // unsigned failure evidence private; it grants no authority.
                let retained = (|| -> anyhow::Result<()> {
                    use std::io::Write as _;
                    use std::os::unix::fs::OpenOptionsExt as _;
                    anyhow::ensure!(
                        std::fs::symlink_metadata(&artifact.private_home)?.file_type().is_dir(),
                        "worker diagnostic home is not an owned directory"
                    );
                    let redacted = crate::agent::terminal::redact_persisted_text(error);
                    let bounded: String = redacted.chars().take(4096).collect();
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(artifact.private_home.join("unsigned-worker-error.txt"))?;
                    file.write_all(bounded.as_bytes())?;
                    file.sync_all()?;
                    Ok(())
                })();
                if retained.is_err() {
                    tracing::warn!("owned worker unsigned-error diagnostic could not be retained");
                }
            }
        }
    }
    let outcome = async {
        // A cleanup-confirmed worker can exit after journaling but before its
        // stdout result reaches the parent. Reuse its exact launch authority.
        if let Some(generation) = &ordinary_generation {
            let interruption = match &outcome {
                Ok(ProcessWorkerOutcome::ExitedWithoutResult(detail)) => Some(detail.clone()),
                Err(_) => Some("worker output was invalid or conflicted with its exit status".to_owned()),
                _ => None,
            };
            if let Some(detail) = interruption {
                return recover_ordinary_interruption(
                    source_fabric.context("ordinary interrupted source storage missing")?,
                    &manifest,
                    generation,
                    source_transport_key
                        .as_ref()
                        .context("ordinary interruption transport key missing")?,
                    detail,
                )
                .await;
            }
        }
        if let Some(generation) = &operator_generation {
            let interruption = match &outcome {
                Ok(ProcessWorkerOutcome::ExitedWithoutResult(detail)) => Some(detail.clone()),
                Err(_) => Some("worker output was invalid or conflicted with its exit status".to_owned()),
                _ => None,
            };
            if let Some(detail) = interruption {
                return recover_operator_interruption(
                    source_fabric.context("operator interrupted source storage missing")?,
                    &manifest,
                    generation,
                    source_transport_key
                        .as_ref()
                        .context("operator interruption transport key missing")?,
                    detail,
                )
                .await;
            }
        }
        match outcome? {
            ProcessWorkerOutcome::UnverifiedStartupFailure(envelope) => {
                anyhow::ensure!(
                    child_source.is_none() && ordinary_generation.is_none(),
                    "startup failure requires the original operator launch"
                );
                let generation = operator_generation
                    .as_ref()
                    .context("operator startup generation missing")?;
                let key = source_transport_key
                    .as_ref()
                    .context("operator startup transport key missing")?;
                // Authenticate the retained launch snapshot, not mutable post-launch disk state.
                process_terminal::operator::verify_startup_failure(
                    source_fabric.context("operator startup source storage missing")?,
                    &manifest,
                    generation,
                    key,
                    &envelope,
                )
                .await?;
                // This branch is reached only after OwnedProcessPhase::Exited and
                // mark_reaped above; terminated/unconfirmed owners return earlier.
                Ok(ProcessWorkerOutcome::VerifiedStartupFailure(envelope.result))
            }
            ProcessWorkerOutcome::Finished(result) => {
                if let Some((grant, input, _)) = child_source {
                    anyhow::ensure!(
                        input.event().run_id.as_deref() == Some(run_id),
                        "worker result belongs to another launched run"
                    );
                    let verified = process_terminal::verify(
                        source_fabric.context("worker logical source storage missing")?,
                        grant,
                        input,
                        result,
                    )
                    .await?;
                    Ok(ProcessWorkerOutcome::VerifiedFinished(verified))
                } else if let Some(generation) = &ordinary_generation {
                    let mut verified = process_terminal::ordinary::verify(
                        source_fabric.context("ordinary process source storage missing")?,
                        &manifest,
                        generation,
                        source_transport_key
                            .as_ref()
                            .context("ordinary process transport key missing")?,
                        result,
                    )
                    .await?;
                    verified.confirm_reaped(run_id)?;
                    Ok(ProcessWorkerOutcome::OrdinaryVerifiedFinished(verified))
                } else if let Some(generation) = &operator_generation {
                    let mut verified = process_terminal::operator::verify(
                        source_fabric.context("operator process source storage missing")?,
                        &manifest,
                        generation,
                        source_transport_key
                            .as_ref()
                            .context("operator process transport key missing")?,
                        result,
                    )
                    .await?;
                    verified.confirm_reaped(run_id)?;
                    Ok(ProcessWorkerOutcome::OperatorVerifiedFinished(verified))
                } else {
                    anyhow::ensure!(
                        result.completion_evidence.is_none()
                            && result.ordinary_completion.is_none()
                            && result.operator_completion.is_none(),
                        "ordinary worker completion requires its authenticated verifier"
                    );
                    Ok(ProcessWorkerOutcome::Finished(result))
                }
            }
            other => Ok(other),
        }
    }
    .await;
    // The leader is confirmed reaped here, including decode/verification
    // failures. Unconfirmed cleanup branches above retain their workspace.
    cleanup_process_worker_workspace(run_id, &worker_workspace, keep_workspace);
    outcome
}

/// Turn "the worker process is gone, here is what it left on its pipes" into a
/// run outcome.
///
/// Split out from [`run_sub_agent_process`] because this is the whole of the
/// process-mode liveness contract and it must be testable against a real
/// SIGKILLed child without standing up a manifest, a workspace and a config
/// directory first.
fn classify_worker_output(
    status: std::process::ExitStatus,
    stdout_raw: &str,
    stderr_raw: &str,
) -> anyhow::Result<ProcessWorkerOutcome> {
    // EOF without a result line. A closed pipe is the *only* thing the parent
    // observes when the worker is OOM-killed, SIGKILLed, or segfaults, so it has
    // to be mapped to a terminal state right here — no later event is coming,
    // and there is no wall clock in this runtime that would eventually give up.
    //
    // MUTATION GUARD: delete this branch and a killed worker falls through to
    // the decode error below, which is still terminal but no longer names the
    // fault; delete the whole classification and the run stays `Running`
    // forever, which is the bug this exists for.
    if stdout_raw.is_empty() {
        return Ok(ProcessWorkerOutcome::ExitedWithoutResult(describe_worker_exit(
            status, stderr_raw,
        )));
    }

    let envelope: crate::session_worker::startup_failure::WorkerOutput =
        serde_json::from_str(stdout_raw).map_err(|error| {
            anyhow::anyhow!(
                "Failed to parse session-worker output: {error}; status={:?}; stderr={stderr_raw}",
                status.code(),
            )
        })?;

    if envelope.authenticated_startup_failure.is_some() {
        anyhow::ensure!(status.success(), "startup failure envelope requires clean owned exit");
        return Ok(ProcessWorkerOutcome::UnverifiedStartupFailure(envelope));
    }
    let parsed = envelope.result;
    if !status.success() && parsed.success {
        return Err(anyhow::anyhow!(
            "session-worker exited with status {:?} despite success result",
            status.code()
        ));
    }

    Ok(ProcessWorkerOutcome::Finished(parsed))
}
