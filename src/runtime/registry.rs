//! Process-wide registry of in-flight work, and the manual kill path.
//!
//! PRX is deliberately an *unbounded* agent runtime: no ceiling on concurrent
//! turns, sub-agents, tool calls or child processes, and — as timeouts are
//! removed — no wall clock that ends a task on the operator's behalf. A long
//! task is normal business here, so killing one on a timer is a worse failure
//! than letting it run.
//!
//! What that trades away is the accidental cleanup a timeout used to provide.
//! Once nothing expires by itself, **seeing** the work and **being able to end
//! it by hand** are the only remaining backstops. This module is both halves:
//!
//! * a registry of everything currently running — agent turns, tool calls,
//!   sub-agent runs, and child processes — each with a stable id, a
//!   human-readable name, a start time, and parent/child lineage
//!   ([`snapshot_all`]);
//! * a by-id kill path that terminates a process *group* (so forked
//!   grandchildren cannot escape), cancels a cooperative task, and optionally
//!   cascades down the lineage ([`kill`]);
//! * a ledger of children that were spawned but never reaped, classified
//!   against the OS at read time, so orphans and zombies are visible instead of
//!   inferred ([`WorkState::Orphaned`] / [`WorkState::Zombie`]).
//!
//! Deliberate non-goals, which future changes must not quietly reintroduce:
//!
//! * **No caps.** The registry never refuses a registration and never bounds
//!   its own size. It is an observation surface, not a gate. Growth is bounded
//!   only by real in-flight work, because entries are removed by RAII rather
//!   than by eviction.
//! * **No timeouts.** [`start_long_task_warner`] is the single time threshold in
//!   this module and it only ever logs; see its documentation.
//!
//! ## Leak-freedom
//!
//! Registration hands back a [`WorkGuard`]. Deregistration happens in `Drop`,
//! never at a call site, so an entry cannot survive a panic, a dropped future
//! (task cancellation), or an early `?` return. Child processes are the one
//! deliberate exception: a process guard dropped *without* [`WorkGuard::mark_reaped`]
//! keeps its ledger row precisely because the OS process may well have
//! outlived its Rust owner — that row is the orphan report, and it is retired
//! automatically once the pid is observed to be gone. Externally supervised
//! tasks can also require cleanup acknowledgement; dropping their owner without
//! confirmation retains an abandoned row instead of reporting a successful kill.
//!
//! ## Concurrency
//!
//! Every production tool call registers here, so a single global lock would
//! serialize the whole runtime. Entries are sharded by id across
//! [`SHARD_COUNT`] `parking_lot::RwLock` maps: registration and removal take one
//! shard's write lock for the length of a hash-map insert, while listing —
//! rare, operator-driven — read-locks each shard in turn.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Number of independently locked shards. Sized well above the core count so
/// unrelated tool calls almost never contend on the same lock.
const SHARD_COUNT: usize = 16;

/// Label prefix of the process row of a `sessions_spawn` session worker; the
/// killer also ends the process groups of that worker's descendants.
pub const WORKER_PROCESS_LABEL_PREFIX: &str = "session-worker ";

/// How long [`kill`] waits for a target to actually disappear before reporting
/// the request as merely *sent*.
///
/// This is a **verification** window, not a timeout: it never terminates
/// anything and never shortens the life of the work item. Its only effect is on
/// the wording of the report the operator reads back.
const KILL_VERIFY_WINDOW: Duration = Duration::from_secs(2);

/// Polling step used inside [`KILL_VERIFY_WINDOW`].
const KILL_VERIFY_STEP: Duration = Duration::from_millis(20);

/// Default long-task warning threshold when `[runtime] long_task_warn_secs` is
/// unset.
pub const DEFAULT_LONG_TASK_WARN_SECS: u64 = 900;

/// Smallest enabled long-task threshold. Below this the sweeper would warn
/// about ordinary tool calls and drown the signal it exists to produce.
pub const MIN_LONG_TASK_WARN_SECS: u64 = 10;

/// Class of work tracked by the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkKind {
    /// One agent turn driven by an inbound message — the lineage root that
    /// everything else in this enum hangs beneath. A turn is registered before
    /// it starts any work of its own, so an operator can see and end a turn
    /// that is stuck somewhere other than in a tool call.
    Turn,
    /// One tool invocation inside an agent turn.
    ToolCall,
    /// A sub-agent run or background session started by `sessions_spawn`.
    SubAgent,
    /// An OS child process.
    Process,
}

impl WorkKind {
    /// Stable lowercase tag used by the CLI and the HTTP API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::ToolCall => "tool_call",
            Self::SubAgent => "sub_agent",
            Self::Process => "process",
        }
    }
}

/// Reported lifecycle state of a work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    /// Live and doing its job.
    Running,
    /// A kill was issued; the item has not gone away yet.
    KillRequested,
    /// Child process whose Rust owner is gone; liveness not yet established.
    Abandoned,
    /// Child process still alive after its owning task finished — an orphan.
    Orphaned,
    /// Child process that exited but was never reaped — a zombie.
    Zombie,
}

impl WorkState {
    /// Stable lowercase tag used by the CLI and the HTTP API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::KillRequested => "kill_requested",
            Self::Abandoned => "abandoned",
            Self::Orphaned => "orphaned",
            Self::Zombie => "zombie",
        }
    }
}

/// Stored state discriminants. Derived states (`Orphaned` / `Zombie`) are not
/// stored: they are computed against the OS when a snapshot is taken, because
/// a process can become either one without anything in this process running.
const STATE_RUNNING: u8 = 0;
const STATE_KILL_REQUESTED: u8 = 1;
const STATE_ABANDONED: u8 = 2;

/// Stable identifier of one registered work item.
///
/// Ids are monotonic for the life of the process and are never reused, so an
/// id an operator read from `prx tasks list` can never later denote a different
/// item — at worst it denotes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkId(u64);

impl WorkId {
    /// Numeric form, for callers that need to index by id.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Parse the textual form produced by [`std::fmt::Display`], accepting both
    /// `w42` and a bare `42` so operators can paste either.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        let digits = trimmed.strip_prefix('w').unwrap_or(trimmed);
        digits.parse::<u64>().ok().map(Self)
    }
}

impl std::fmt::Display for WorkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "w{}", self.0)
    }
}

/// Point-in-time view of one registered work item.
#[derive(Debug, Clone)]
pub struct WorkSnapshot {
    pub id: WorkId,
    pub kind: WorkKind,
    /// Tool name, agent/run label, or command line.
    pub name: Arc<str>,
    /// The work item that started this one, when known.
    pub parent: Option<WorkId>,
    /// Correlating run id from `SPAWN_EXECUTION_CONTEXT`, when known.
    pub run_id: Option<Arc<str>>,
    /// Fan-out this item belongs to, when it was started as one member of a
    /// `sessions_spawn` batch. Members of one batch are otherwise
    /// indistinguishable from unrelated concurrent runs, because they are
    /// siblings rather than descendants of one another.
    pub batch_id: Option<Arc<str>>,
    /// Wall-clock start, milliseconds since the Unix epoch.
    pub started_at_unix_ms: u64,
    /// How long the item has been running.
    pub elapsed: Duration,
    pub state: WorkState,
    pub pid: Option<u32>,
    pub pgid: Option<i32>,
    /// Whether this item currently has a steering channel, i.e. whether
    /// `POST /api/runtime/tasks/{id}/message` can hand it anything.
    ///
    /// Reported because the *kind* does not answer the question and reading it
    /// as if it did is an easy, expensive mistake: a detached gateway job is
    /// registered as [`WorkKind::SubAgent`] and still has no channel, because
    /// only a run started through `sessions_spawn` ever calls
    /// [`attach_steer_sender`]. Without this field the only way to find out was
    /// to send a message and read the refusal.
    pub steerable: bool,
}

/// One registry row. Immutable metadata plus a small mutable state word.
struct Slot {
    id: u64,
    kind: WorkKind,
    name: Arc<str>,
    parent: Option<u64>,
    run_id: Option<Arc<str>>,
    /// Immutable batch membership; see [`WorkSnapshot::batch_id`].
    batch_id: Option<Arc<str>>,
    started: Instant,
    started_wall: SystemTime,
    state: AtomicU8,
    /// Cooperative cancellation for tool calls and sub-agent runs.
    cancel: Option<CancellationToken>,
    /// Set after `tokio::spawn` for sub-agent runs whose task may be aborted.
    /// Aborting drops the run future, which drops its guard — that removal is
    /// what confirms the kill.
    abort: RwLock<Option<tokio::task::AbortHandle>>,
    /// Operator-steering endpoint for sub-agent runs: a clone of the very
    /// `SubAgentRun::steer_tx` that `sessions_send` sends on. Holding it here
    /// adds an *address book*, not a second transport — the message still
    /// travels the one bounded channel the run already listens on, so
    /// backpressure and ordering are unchanged. See [`attach_steer_sender`].
    steer: RwLock<Option<tokio::sync::mpsc::Sender<String>>>,
    pid: Option<u32>,
    pgid: Option<i32>,
    /// How many long-task warnings this item has already produced, so the
    /// sweeper repeats at a fixed cadence instead of every sweep.
    warnings: AtomicU32,
}

impl Slot {
    fn stored_state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    /// Resolve the reported state, consulting the OS for abandoned children.
    ///
    /// Returns `None` when the row should be retired: an abandoned child whose
    /// pid is no longer present has been fully reclaimed and is no longer
    /// interesting.
    fn reported_state(&self) -> Option<WorkState> {
        match self.stored_state() {
            STATE_KILL_REQUESTED => Some(WorkState::KillRequested),
            STATE_ABANDONED => self
                .pid
                .map_or(Some(WorkState::Abandoned), |pid| match probe_process(pid) {
                    ProcessLiveness::Running => Some(WorkState::Orphaned),
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    ProcessLiveness::Zombie => Some(WorkState::Zombie),
                    ProcessLiveness::Gone => None,
                }),
            _ => Some(WorkState::Running),
        }
    }

    fn snapshot(&self, state: WorkState) -> WorkSnapshot {
        WorkSnapshot {
            id: WorkId(self.id),
            kind: self.kind,
            name: Arc::clone(&self.name),
            parent: self.parent.map(WorkId),
            run_id: self.run_id.clone(),
            batch_id: self.batch_id.clone(),
            started_at_unix_ms: self
                .started_wall
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX)),
            elapsed: self.started.elapsed(),
            state,
            pid: self.pid,
            pgid: self.pgid,
            steerable: self.steer.read().is_some(),
        }
    }
}

/// Sharded table of live work items.
struct Registry {
    next_id: AtomicU64,
    shards: [RwLock<HashMap<u64, Arc<Slot>>>; SHARD_COUNT],
}

impl Registry {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            shards: std::array::from_fn(|_| RwLock::new(HashMap::new())),
        }
    }

    fn shard(&self, id: u64) -> &RwLock<HashMap<u64, Arc<Slot>>> {
        // `id % SHARD_COUNT` on a monotonic counter spreads ids round-robin
        // across shards, which is exactly the distribution we want.
        let index = usize::try_from(id % SHARD_COUNT as u64).unwrap_or(0);
        self.shards.get(index).unwrap_or(&self.shards[0])
    }

    fn insert(&self, slot: Arc<Slot>) {
        self.shard(slot.id).write().insert(slot.id, slot);
    }

    fn remove(&self, id: u64) {
        self.shard(id).write().remove(&id);
    }

    fn get(&self, id: u64) -> Option<Arc<Slot>> {
        self.shard(id).read().get(&id).map(Arc::clone)
    }

    /// Collect every live slot. Locks one shard at a time and clones out `Arc`s,
    /// so no lock is held while the caller inspects the results.
    fn slots(&self) -> Vec<Arc<Slot>> {
        let mut collected = Vec::new();
        for shard in &self.shards {
            let guard = shard.read();
            collected.extend(guard.values().map(Arc::clone));
        }
        collected
    }
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

tokio::task_local! {
    /// Work item that owns the current task. Reading it is how a child process
    /// or a nested registration discovers its parent without any call site
    /// having to thread an id through.
    static CURRENT_WORK: WorkId;
}

/// The work item owning the current task, if this task is running inside one.
#[must_use]
pub fn current_work_id() -> Option<WorkId> {
    CURRENT_WORK.try_with(|id| *id).ok()
}

/// The nearest enclosing work item that is not a tool call.
///
/// A run that outlives the tool call that launched it must not hang off that
/// call: the row is deregistered when the call returns, which cuts the run out
/// of the lineage a cascading kill of the turn walks. Parenting it to the
/// enclosing turn (or sub-agent) keeps the run reachable for as long as that
/// owner is registered.
#[must_use]
pub fn current_owner_work_id() -> Option<WorkId> {
    let mut id = current_work_id()?;
    loop {
        let Some(slot) = REGISTRY.get(id.raw()) else {
            return Some(id);
        };
        if slot.kind != WorkKind::ToolCall {
            return Some(id);
        }
        match slot.parent {
            Some(parent) => id = WorkId(parent),
            None => return Some(id),
        }
    }
}

/// RAII registration. Dropping it deregisters the work item.
///
/// Deregistration lives in `Drop` rather than at a call site on purpose: a
/// panicking tool, a cancelled future, or an early `?` must not be able to
/// leave a ghost row behind. See the module documentation for the one
/// deliberate exception (unreaped child processes).
pub struct WorkGuard {
    slot: Arc<Slot>,
    /// Child processes only: whether the owner confirmed the process was reaped.
    /// An unconfirmed process keeps its row as an orphan report.
    reaped: bool,
    cleanup_pending: bool,
}

impl WorkGuard {
    /// Id of the registered item.
    #[must_use]
    pub fn id(&self) -> WorkId {
        WorkId(self.slot.id)
    }

    /// Record that the child process was waited on and its exit status
    /// collected, so the ledger row can be retired instead of being reported as
    /// an orphan.
    pub const fn mark_reaped(&mut self) {
        self.reaped = true;
    }

    /// Keep the row visible if its external cleanup owner disappears.
    pub(crate) const fn require_cleanup_acknowledgement(&mut self) {
        self.cleanup_pending = true;
    }

    /// Permit retirement after the external owner confirms cleanup.
    pub(crate) const fn mark_cleanup_confirmed(&mut self) {
        self.cleanup_pending = false;
    }

    /// Whether a kill has been requested for this item.
    #[must_use]
    pub fn kill_requested(&self) -> bool {
        self.slot.stored_state() == STATE_KILL_REQUESTED
    }
}

impl Drop for WorkGuard {
    fn drop(&mut self) {
        if self.cleanup_pending || (self.slot.kind == WorkKind::Process && !self.reaped) {
            // The Rust owner is gone but the OS process may not be. Keep the
            // row; `snapshot_all` classifies it against the OS and retires it
            // once the pid is genuinely gone.
            self.slot.state.store(STATE_ABANDONED, Ordering::Relaxed);
            return;
        }
        REGISTRY.remove(self.slot.id);
    }
}

fn register(
    kind: WorkKind,
    name: Arc<str>,
    parent: Option<WorkId>,
    run_id: Option<Arc<str>>,
    batch_id: Option<Arc<str>>,
    cancel: Option<CancellationToken>,
    pid: Option<u32>,
    pgid: Option<i32>,
) -> WorkGuard {
    let id = REGISTRY.next_id.fetch_add(1, Ordering::Relaxed);
    let slot = Arc::new(Slot {
        id,
        kind,
        name,
        parent: parent.map(WorkId::raw),
        run_id,
        batch_id,
        started: Instant::now(),
        started_wall: SystemTime::now(),
        state: AtomicU8::new(STATE_RUNNING),
        cancel,
        abort: RwLock::new(None),
        steer: RwLock::new(None),
        pid,
        pgid,
        warnings: AtomicU32::new(0),
    });
    REGISTRY.insert(Arc::clone(&slot));
    WorkGuard {
        slot,
        reaped: false,
        cleanup_pending: false,
    }
}

/// Register one agent turn.
///
/// A turn is a lineage *root*: it is registered before it starts any work of
/// its own, and running it inside [`scoped`] is what makes the tool calls,
/// sub-agents and child processes it goes on to start resolve it as their
/// parent — which is in turn what lets `kill --cascade` on the turn reach all
/// of them.
///
/// `cancel` must be the turn's own cooperative token, the one the turn itself
/// selects on. Cancelling is deliberately the only kill mechanism offered here:
/// a turn that is asked to stop can still finalize (flush its draft, commit its
/// terminal record, tell the user), which aborting the task would take away.
#[must_use]
pub fn register_turn(name: &str, run_id: &str, cancel: Option<CancellationToken>) -> WorkGuard {
    register(
        WorkKind::Turn,
        Arc::from(name),
        current_work_id(),
        Some(Arc::from(run_id)),
        None,
        cancel,
        None,
        None,
    )
}

/// Register one tool invocation.
///
/// `cancel` should be a token dedicated to this call (a child of the turn
/// token), so killing one tool does not have to take the whole turn with it.
#[must_use]
pub fn register_tool_call(name: &str, run_id: Option<&str>, cancel: Option<CancellationToken>) -> WorkGuard {
    register(
        WorkKind::ToolCall,
        Arc::from(name),
        current_work_id(),
        run_id.map(Arc::from),
        None,
        cancel,
        None,
        None,
    )
}

/// Register work aimed *at* an already-registered item, as that item's child.
///
/// The one case is a cross-entry-point steering delivery: the gateway resolves
/// a run by address and then parks on that run's bounded queue, and the parked
/// send is registered so it can be seen and ended. `parent` is passed
/// explicitly because such a delivery runs on a request task that inherits no
/// task-locals from the target, so [`current_work_id`] would make it a lineage
/// *root*.
///
/// Being a root is what broke addressing. The row carries the target's
/// `run_id`, but [`resolve_address`] answers a `run_id` with the lineage root —
/// the target — so an operator killing by the only portable address reached the
/// run and never the delivery hanging off it, while the delivery kept parking.
/// Worse, that kill destroyed live work the operator had not aimed at. Making
/// the delivery a child restores the invariant every other row already keeps:
/// **everything sharing a `run_id` is inside that run's lineage**, so the
/// cascade a `run_id` kill already performs reaches it.
///
/// The direct [`WorkId`] address keeps working unchanged, and a delivery that
/// never parks is dropped before anyone can see it either way.
#[must_use]
pub fn register_delivery(
    name: &str,
    target: WorkId,
    run_id: Option<&str>,
    cancel: Option<CancellationToken>,
) -> WorkGuard {
    register(
        WorkKind::ToolCall,
        Arc::from(name),
        Some(target),
        run_id.map(Arc::from),
        None,
        cancel,
        None,
        None,
    )
}

/// Register a sub-agent run or background session.
///
/// `parent` is passed explicitly because the run executes in a freshly spawned
/// task, which does not inherit the spawner's task-locals; callers capture
/// [`current_work_id`] before `tokio::spawn`.
///
/// `batch_id` is `Some` only for a member of a `sessions_spawn` fan-out. It is
/// recorded here rather than derived from the lineage because a batch's members
/// are *siblings*: the tool call that launched them is their common parent, and
/// that row is retired the moment the launching call returns, which is long
/// before the members finish. Without this field the fan-out is unrecoverable
/// from the registry alone.
#[must_use]
pub fn register_sub_agent(
    name: &str,
    run_id: &str,
    parent: Option<WorkId>,
    batch_id: Option<&str>,
    cancel: Option<CancellationToken>,
) -> WorkGuard {
    register(
        WorkKind::SubAgent,
        Arc::from(name),
        parent,
        Some(Arc::from(run_id)),
        batch_id.map(Arc::from),
        cancel,
        None,
        None,
    )
}

/// Register an OS child process.
///
/// `pgid` must be the child's own process group (children spawned with
/// `process_group(0)` lead one), because that is what makes killing the whole
/// tree possible; a child without its own group can only be killed directly and
/// its forked grandchildren would survive.
#[must_use]
pub fn register_process(command: &str, pid: Option<u32>, pgid: Option<i32>) -> WorkGuard {
    register(
        WorkKind::Process,
        Arc::from(command),
        current_work_id(),
        None,
        None,
        None,
        pid,
        pgid,
    )
}

/// Attach the abort handle of an already-spawned task to a registered item.
///
/// Separate from registration because the handle only exists after
/// `tokio::spawn`, while the id has to be known before it (the future takes
/// ownership of the guard). A no-op for ids that already finished.
pub fn attach_abort_handle(id: WorkId, handle: tokio::task::AbortHandle) {
    if let Some(slot) = REGISTRY.get(id.raw()) {
        *slot.abort.write() = Some(handle);
    }
}

/// Attach a running sub-agent's steering channel to its registry row.
///
/// This is deliberately *not* a new delivery mechanism. `sender` is a clone of
/// the same `SubAgentRun::steer_tx` that `sessions_send` resolves out of the
/// spawning tool's `active_runs` and sends on, so a message routed through here
/// lands in the same bounded queue, in the same order, with the same
/// backpressure and no wall clock anywhere.
///
/// It exists because `active_runs` is *per tool instance*: the gateway and the
/// channel dispatch loop each build their own registry inside one daemon
/// process, so a control-plane request arriving at the gateway cannot see a run
/// that a channel turn started. The work registry is the only process-wide
/// index of in-flight work, which makes it the only place a cross-entry-point
/// address can be resolved.
///
/// A no-op for ids that already finished; the row and the sender it holds are
/// released together by [`WorkGuard`]'s `Drop`, so a completed run stops being
/// addressable without any explicit teardown.
pub fn attach_steer_sender(id: WorkId, sender: tokio::sync::mpsc::Sender<String>) {
    if let Some(slot) = REGISTRY.get(id.raw()) {
        *slot.steer.write() = Some(sender);
    }
}

/// Resolve an operator-supplied address to a live work item.
///
/// Two address spaces are accepted, and the order matters:
///
/// 1. **`run_id`** — the correlating id a run carries across processes. This is
///    the only address a caller outside this process should use, because it is
///    stable and meaningful everywhere the run is known.
/// 2. **[`WorkId`]** (`w42` or a bare `42`) — a *process-local, monotonic*
///    counter. It is convenient for an operator reading `prx tasks list` on the
///    same box and it means nothing anywhere else.
///
/// `run_id` is tried first so that address space always wins: a `WorkId` is all
/// digits (optionally `w`-prefixed) and a run id is a uuid, so in practice they
/// cannot collide, but resolving the portable address first keeps that true by
/// construction rather than by luck.
///
/// Several rows can share one `run_id` — a sub-agent run, every tool call made
/// inside it, and any cross-entry delivery parked on its steering queue. The
/// lineage root is returned: ids are handed out monotonically and the run is
/// registered before anything it starts, so the lowest matching id is the
/// ancestor of the rest. That answer is only usable because every one of those
/// rows really is in the root's lineage — which is why a delivery, whose task
/// inherits no task-locals from the target, is parented explicitly by
/// [`register_delivery`] rather than left to become a second root under the
/// same address. Preferring [`WorkKind::Turn`]
/// and [`WorkKind::SubAgent`] rows first keeps that intent explicit rather than
/// relying on the ordering alone.
#[must_use]
pub fn resolve_address(address: &str) -> Option<WorkId> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return None;
    }
    let slots = REGISTRY.slots();
    let root = slots
        .iter()
        .filter(|slot| slot.run_id.as_deref() == Some(trimmed))
        .min_by_key(|slot| {
            let rank = match slot.kind {
                WorkKind::Turn | WorkKind::SubAgent => 0u8,
                WorkKind::ToolCall | WorkKind::Process => 1,
            };
            (rank, slot.id)
        });
    if let Some(slot) = root {
        return Some(WorkId(slot.id));
    }
    let candidate = WorkId::parse(trimmed)?;
    slots.iter().any(|slot| slot.id == candidate.raw()).then_some(candidate)
}

/// Why a steering message could not be handed to a work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerRejection {
    /// The item is live but has no steering channel.
    ///
    /// A channel is opened by [`attach_steer_sender`], which only a sub-agent
    /// run started through `sessions_spawn` ever reaches. Everything else is
    /// in this bucket — agent turns, tool calls, detached gateway jobs, cron
    /// runs, and runs started before steering existed — *including* rows whose
    /// [`WorkKind`] is [`WorkKind::SubAgent`]: a gateway job is registered with
    /// that kind and still has nothing to receive a message. The kind is not
    /// the criterion; see [`WorkSnapshot::steerable`], which is.
    NotSteerable,
    /// The item's receiver is gone, or its row was retired while the message
    /// was being routed: the run ended.
    ChannelClosed,
}

impl SteerRejection {
    /// Stable lowercase tag used by the CLI and the HTTP API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotSteerable => "not_steerable",
            Self::ChannelClosed => "channel_closed",
        }
    }
}

/// Deliver an operator message to a registered run's steering channel.
///
/// The sender is cloned out and every lock released *before* the send, for the
/// same reason `sessions_send` drops its `active_runs` guard first: the queue is
/// bounded, so a send legitimately parks until the run drains it, and parking
/// while holding a registry lock would deadlock against the run's own
/// registrations.
///
/// Parking is the intended behaviour, not a fault. A busy target slows the
/// caller down; nothing here gives up on a clock, because a target that is deep
/// in a long tool call is doing exactly what this runtime exists to allow.
///
/// # Errors
///
/// Returns [`SteerRejection`] when the item has no steering channel, or when
/// its receiver has already gone away.
pub async fn steer(id: WorkId, message: String) -> Result<(), SteerRejection> {
    let sender = {
        // A row that has already been retired means the run finished between
        // the caller's resolution and this send — the same outcome as a dropped
        // receiver, and reported as such rather than as "cannot be steered".
        let Some(slot) = REGISTRY.get(id.raw()) else {
            return Err(SteerRejection::ChannelClosed);
        };
        let sender = slot.steer.read().clone();
        sender.ok_or(SteerRejection::NotSteerable)?
    };
    sender.send(message).await.map_err(|_| SteerRejection::ChannelClosed)
}

/// Run `fut` with `id` as the current work item, without transferring guard
/// ownership into the future.
///
/// Use this when the guard is already owned by the enclosing scope and only the
/// lineage needs to reach into the future — a child process spawned inside it
/// then resolves its parent through [`current_work_id`].
pub fn scope_current<F>(id: WorkId, fut: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    CURRENT_WORK.scope(id, fut)
}

/// Run `fut` as the given work item: the guard lives for exactly the future's
/// lifetime, and anything the future starts sees it as its parent.
///
/// Holding the guard *inside* the future is what makes cancellation safe — if
/// the future is dropped mid-flight, the guard is dropped with it.
pub async fn scoped<F>(guard: WorkGuard, fut: F) -> F::Output
where
    F: Future,
{
    let id = guard.id();
    let result = CURRENT_WORK.scope(id, fut).await;
    drop(guard);
    result
}

/// Every live work item, with abandoned children classified against the OS.
///
/// Rows for children that have been fully reclaimed are retired as a side
/// effect, so the ledger converges on "actually outstanding" without any
/// background reaper.
#[must_use]
pub fn snapshot_all() -> Vec<WorkSnapshot> {
    let mut snapshots = Vec::new();
    for slot in REGISTRY.slots() {
        match slot.reported_state() {
            Some(state) => snapshots.push(slot.snapshot(state)),
            None => REGISTRY.remove(slot.id),
        }
    }
    snapshots.sort_by_key(|snapshot| snapshot.id);
    snapshots
}

/// Snapshot of a single work item, or `None` when the id is unknown.
#[must_use]
pub fn snapshot(id: WorkId) -> Option<WorkSnapshot> {
    let slot = REGISTRY.get(id.raw())?;
    slot.reported_state().map(|state| slot.snapshot(state))
}

// ── Kill ────────────────────────────────────────────────────────────────────

/// Outcome of killing one work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// The item is confirmed gone.
    Killed,
    /// Termination was issued and the item has not gone away yet.
    Requested,
    /// The item had no way to be terminated (no process group, no token).
    NotKillable,
    /// No such id.
    Unknown,
}

impl KillOutcome {
    /// Stable lowercase tag used by the CLI and the HTTP API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Killed => "killed",
            Self::Requested => "requested",
            Self::NotKillable => "not_killable",
            Self::Unknown => "unknown",
        }
    }
}

/// Per-target result of a [`kill`].
#[derive(Debug, Clone)]
pub struct KillResult {
    pub id: WorkId,
    pub kind: WorkKind,
    pub name: Arc<str>,
    pub outcome: KillOutcome,
}

/// Terminate one work item, optionally taking its descendants with it.
///
/// Cascade defaults to *on* at every call site in PRX. The lineage exists
/// precisely because nothing expires on its own any more: killing an agent
/// without its tool calls and child processes leaves behind exactly the orphans
/// this registry was built to prevent, and the operator has no reason to expect
/// that a killed agent's `ffmpeg` keeps burning CPU. `cascade = false` remains
/// available for the narrow case of retiring a single stuck child while its
/// parent keeps working.
///
/// Termination is by process *group* for children — `killpg`, matching
/// `shell_process::ManagedShellChild` — so a child that forked its own workers
/// cannot leave them running. Tasks are cancelled cooperatively through their
/// [`CancellationToken`].
///
/// The returned outcomes distinguish "confirmed gone" from "termination issued,
/// still present", verified against the OS and against the registry rather than
/// against the return value of `kill(2)`.
pub async fn kill(id: WorkId, cascade: bool) -> Vec<KillResult> {
    let all = REGISTRY.slots();
    let Some(root) = all.iter().find(|slot| slot.id == id.raw()).map(Arc::clone) else {
        return vec![KillResult {
            id,
            kind: WorkKind::ToolCall,
            name: Arc::from("<unknown>"),
            outcome: KillOutcome::Unknown,
        }];
    };

    let targets = if cascade {
        collect_lineage(&all, root.id)
    } else {
        vec![root]
    };

    let mut issued = Vec::with_capacity(targets.len());
    for slot in &targets {
        let _ = slot.state.compare_exchange(
            STATE_RUNNING,
            STATE_KILL_REQUESTED,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        issued.push(request_termination(slot));
    }

    verify_terminated(&targets, &issued).await
}

/// Terminate every member of one `sessions_spawn` fan-out, each with the same
/// lineage cascade a single [`kill`] applies.
///
/// This is not a second termination mechanism: it resolves the batch to its
/// member rows and then reuses [`collect_lineage`] and [`request_termination`]
/// unchanged. It exists because a batch is a set of *sibling* roots, so there is
/// no single id whose lineage covers it — the launching tool call, which was
/// their common parent, is deregistered as soon as it returns, while the members
/// it started keep running.
///
/// Every target is signalled before any is verified, so ending a fan-out of any
/// width costs one verification window rather than one per member.
///
/// An empty result means no live work carries that batch id.
pub async fn kill_batch(batch_id: &str, cascade: bool) -> Vec<KillResult> {
    let all = REGISTRY.slots();
    let mut targets: Vec<Arc<Slot>> = Vec::new();
    for root in all.iter().filter(|slot| slot.batch_id.as_deref() == Some(batch_id)) {
        let lineage = if cascade {
            collect_lineage(&all, root.id)
        } else {
            vec![Arc::clone(root)]
        };
        for slot in lineage {
            if !targets.iter().any(|chosen| chosen.id == slot.id) {
                targets.push(slot);
            }
        }
    }
    if targets.is_empty() {
        return Vec::new();
    }

    let mut issued = Vec::with_capacity(targets.len());
    for slot in &targets {
        let _ = slot.state.compare_exchange(
            STATE_RUNNING,
            STATE_KILL_REQUESTED,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        issued.push(request_termination(slot));
    }

    verify_terminated(&targets, &issued).await
}

/// `root` plus every descendant of it, deepest ordering irrelevant because all
/// targets are signalled before any is verified.
fn collect_lineage(all: &[Arc<Slot>], root: u64) -> Vec<Arc<Slot>> {
    let mut selected: Vec<Arc<Slot>> = all.iter().filter(|slot| slot.id == root).map(Arc::clone).collect();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for slot in all {
            if slot.parent == Some(parent) && !selected.iter().any(|chosen| chosen.id == slot.id) {
                frontier.push(slot.id);
                selected.push(Arc::clone(slot));
            }
        }
    }
    selected
}

/// Whether any termination mechanism was available for this slot.
fn request_termination(slot: &Slot) -> bool {
    let cancelled = slot.cancel.as_ref().map_or(false, |token| {
        token.cancel();
        true
    });
    let signalled = kill_process_group(slot);
    let aborted = slot.abort.read().as_ref().map_or(false, |handle| {
        handle.abort();
        true
    });
    cancelled || signalled || aborted
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_process_group(slot: &Slot) -> bool {
    if let Some(pgid) = slot.pgid.filter(|pgid| *pgid > 0) {
        // A session worker's shell commands run in groups of their own that
        // `killpg(pgid)` cannot reach, so snapshot them before the worker dies.
        let foreign = match slot.pid.and_then(|pid| i32::try_from(pid).ok()) {
            Some(pid) if slot.name.starts_with(WORKER_PROCESS_LABEL_PREFIX) => {
                crate::runtime::descendant_groups::foreign_groups_of(pid, pgid)
            }
            _ => Vec::new(),
        };
        // SAFETY: `pgid` is the positive group id of a child spawned with
        // `process_group(0)`. `killpg` only delivers a signal; it dereferences
        // no pointer and the call cannot invalidate any Rust invariant.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
        crate::runtime::descendant_groups::kill_groups(&foreign);
        return true;
    }
    if let Some(pid) = slot.pid.and_then(|pid| i32::try_from(pid).ok()).filter(|pid| *pid > 0) {
        // SAFETY: as above; signalling a pid is a pure kernel-side operation.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        return true;
    }
    false
}

#[cfg(not(unix))]
fn kill_process_group(_slot: &Slot) -> bool {
    false
}

/// Poll each target until it is gone or the verification window closes.
///
/// A task target counts as gone when its row has left the registry (its guard
/// dropped); a process target counts as gone when its pid no longer resolves to
/// a live or zombie process.
async fn verify_terminated(targets: &[Arc<Slot>], issued: &[bool]) -> Vec<KillResult> {
    let deadline = Instant::now() + KILL_VERIFY_WINDOW;
    let mut done = vec![false; targets.len()];
    loop {
        let mut pending = false;
        for (index, slot) in targets.iter().enumerate() {
            if done.get(index).copied().unwrap_or(true) {
                continue;
            }
            if terminated(slot) {
                if let Some(entry) = done.get_mut(index) {
                    *entry = true;
                }
            } else {
                pending = true;
            }
        }
        if !pending || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(KILL_VERIFY_STEP).await;
    }

    targets
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let outcome = if done.get(index).copied().unwrap_or(false) {
                KillOutcome::Killed
            } else if issued.get(index).copied().unwrap_or(false) {
                KillOutcome::Requested
            } else {
                KillOutcome::NotKillable
            };
            KillResult {
                id: WorkId(slot.id),
                kind: slot.kind,
                name: Arc::clone(&slot.name),
                outcome,
            }
        })
        .collect()
}

fn terminated(slot: &Slot) -> bool {
    match slot.kind {
        // A zombie counts as terminated: the process is no longer running any
        // code. Collecting its exit status belongs to whoever owns the `Child`,
        // and the ledger keeps reporting it as a zombie until they do, so
        // nothing is hidden by calling the kill successful.
        WorkKind::Process => slot.pid.map_or_else(
            || REGISTRY.get(slot.id).is_none(),
            |pid| probe_process(pid) != ProcessLiveness::Running,
        ),
        WorkKind::Turn | WorkKind::ToolCall | WorkKind::SubAgent => REGISTRY.get(slot.id).is_none(),
    }
}

// ── Process liveness ────────────────────────────────────────────────────────

/// What the OS says about a pid.
///
/// Note the inherent pid-reuse caveat: a pid we believe belongs to an abandoned
/// child may have been recycled by the kernel. These states are diagnostics,
/// never a basis for signalling — [`kill`] only ever signals rows whose guard is
/// still alive or whose group id we created ourselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessLiveness {
    Running,
    /// Only the Linux and macOS probes can tell a zombie from a live process.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Zombie,
    Gone,
}

#[cfg(target_os = "linux")]
fn probe_process(pid: u32) -> ProcessLiveness {
    // `/proc/<pid>/stat` is the only place that distinguishes a live child from
    // one that exited and is waiting to be reaped; `kill(pid, 0)` succeeds for
    // both. The state character is the field immediately after the `)` that
    // closes the (possibly space-containing) comm field.
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return ProcessLiveness::Gone;
    };
    let Some(after_comm) = stat.rfind(')').and_then(|index| stat.get(index.saturating_add(1)..)) else {
        return ProcessLiveness::Running;
    };
    match after_comm.split_whitespace().next() {
        Some("Z") => ProcessLiveness::Zombie,
        Some("X" | "x") => ProcessLiveness::Gone,
        _ => ProcessLiveness::Running,
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn probe_process(pid: u32) -> ProcessLiveness {
    let Ok(pid) = i32::try_from(pid) else {
        return ProcessLiveness::Gone;
    };

    // Darwin has no procfs, but libproc exposes the BSD process state without
    // reaping the child.  `kill(pid, 0)` alone reports success for a zombie,
    // which made a completed child appear to be an indefinitely running one.
    // SAFETY: `proc_bsdinfo` is a C plain-data output structure whose all-zero
    // representation is valid; `proc_pidinfo` initializes the returned fields.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let expected = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).unwrap_or(i32::MAX);
    // SAFETY: `info` is writable for exactly `expected` bytes, the pointer and
    // size match `PROC_PIDTBSDINFO`, and Darwin validates the integer PID.
    let received = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), expected) };
    if received == expected {
        return if info.pbi_status == libc::SZOMB {
            ProcessLiveness::Zombie
        } else {
            ProcessLiveness::Running
        };
    }

    // Darwin's libproc returns ESRCH for a zombie, while `kill(pid, 0)` still
    // succeeds until the owner reaps it. That pair is the non-procfs equivalent
    // of Linux's `/proc/<pid>/stat` `Z` state.
    let proc_error = std::io::Error::last_os_error();
    // SAFETY: signal zero performs an OS liveness/permission probe only; it
    // neither delivers a signal nor dereferences memory in this process.
    if unsafe { libc::kill(pid, 0) } == 0 {
        if proc_error.raw_os_error() == Some(libc::ESRCH) {
            ProcessLiveness::Zombie
        } else {
            ProcessLiveness::Running
        }
    } else {
        ProcessLiveness::Gone
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
#[allow(unsafe_code)]
fn probe_process(pid: u32) -> ProcessLiveness {
    let Ok(pid) = i32::try_from(pid) else {
        return ProcessLiveness::Gone;
    };
    // SAFETY: signal 0 performs an existence/permission check and delivers
    // nothing. No pointer is dereferenced.
    if unsafe { libc::kill(pid, 0) } == 0 {
        // Without procfs a zombie is indistinguishable from a live process; the
        // conservative answer is "still there", which keeps the row visible.
        ProcessLiveness::Running
    } else {
        ProcessLiveness::Gone
    }
}

#[cfg(not(unix))]
const fn probe_process(_pid: u32) -> ProcessLiveness {
    ProcessLiveness::Gone
}

// ── Long-task warning ───────────────────────────────────────────────────────

/// Resolve the configured long-task warning threshold.
///
/// `None` in config means the default; `Some(0)` disables the warning entirely.
#[must_use]
pub const fn long_task_warn_threshold(configured: Option<u64>) -> Option<Duration> {
    match configured {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(Duration::from_secs(DEFAULT_LONG_TASK_WARN_SECS)),
    }
}

#[cfg(test)]
static WARNING_TICK: parking_lot::ReentrantMutex<()> = parking_lot::ReentrantMutex::new(());

/// Emit a `warn!` for every work item that has been running longer than
/// `threshold`, and report how many were warned about.
///
/// **This function must never terminate anything, and no caller may make it
/// do so.** It is the only time threshold in the runtime registry and it exists
/// solely so a long task is *visible*. PRX treats long tasks as normal
/// business: a research run or a build can legitimately take hours, and killing
/// one on a timer is a worse outcome than letting it finish. If a future change
/// needs work to end at a deadline, that is a product decision requiring an
/// explicit, separately named mechanism — turning this warning into a timeout is
/// the specific regression this comment exists to prevent.
pub fn warn_long_running(threshold: Duration) -> usize {
    #[cfg(test)]
    let _tick = WARNING_TICK.lock();
    let mut warned: usize = 0;
    for slot in REGISTRY.slots() {
        let elapsed = slot.started.elapsed();
        if elapsed < threshold {
            continue;
        }
        // Re-warn once per elapsed multiple of the threshold rather than on
        // every sweep, so a genuinely long task stays visible without flooding.
        // Floored at one so crossing the threshold always warns once, including
        // for sub-second thresholds where the integer ratio would round to zero.
        let due = u32::try_from(elapsed.as_secs() / threshold.as_secs().max(1))
            .unwrap_or(u32::MAX)
            .max(1);
        let already = slot.warnings.load(Ordering::Relaxed);
        if already >= due {
            continue;
        }
        slot.warnings.store(due, Ordering::Relaxed);
        warned = warned.saturating_add(1);
        tracing::warn!(
            work_id = %WorkId(slot.id),
            kind = slot.kind.as_str(),
            name = %slot.name,
            elapsed_secs = elapsed.as_secs(),
            threshold_secs = threshold.as_secs(),
            "runtime work item has been running for a long time; this is a notification only \
             and nothing was terminated. Inspect with `prx tasks list` and, if it is genuinely \
             stuck, end it with `prx tasks kill <id>`."
        );
    }
    warned
}

/// Start the background sweeper that emits long-task warnings.
///
/// Idempotent: repeated calls after the first are ignored, so wiring it from
/// several entry points is safe. Does nothing when the threshold is disabled or
/// when no tokio runtime is available.
///
/// The sweeper **only logs** — see [`warn_long_running`].
pub fn start_long_task_warner(threshold: Option<Duration>) {
    use std::sync::atomic::AtomicBool;
    static STARTED: AtomicBool = AtomicBool::new(false);

    let Some(threshold) = threshold else { return };
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // Sweep several times per threshold so a crossing is reported promptly
    // without the sweep itself becoming a busy loop.
    let interval = threshold / 4;
    let interval = interval.max(Duration::from_secs(1));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            warn_long_running(threshold);
        }
    });
}

// ── Hang terminations ───────────────────────────────────────────────────────

/// One agent turn ended by the idle (no-progress) detector.
///
/// Recorded here, and nowhere near [`warn_long_running`], because the two are
/// opposites: the long-task warning reports that work is *long* and is
/// forbidden from ending it, while this ledger records the only automatic
/// termination in the runtime — work that produced no observable progress at
/// all for a full window and was judged hung. Keeping the ledger in the
/// registry is what makes such a termination visible to an operator alongside
/// the work items it killed.
///
/// The detector itself lives in [`crate::agent::idle`]; the registry only
/// stores what it reports.
#[derive(Debug, Clone)]
pub struct HangTermination {
    /// Label of the terminated turn.
    pub label: Arc<str>,
    /// Which threshold was crossed ([`crate::agent::idle::HangReason::as_str`]).
    pub reason: &'static str,
    /// Silence at the moment of the verdict.
    pub idle_secs: u64,
    /// Total run time at the moment of the verdict.
    pub elapsed_secs: u64,
    /// Work items signalled as part of the termination.
    pub killed: usize,
    /// When it happened, milliseconds since the Unix epoch.
    pub at_unix_ms: u64,
}

/// How many terminations are retained. A bound is required here for the same
/// reason the live table is unbounded: this is a log of rare events, not a
/// mirror of in-flight work, so it must not grow with process uptime.
const HANG_TERMINATION_HISTORY: usize = 64;

static HANG_TERMINATIONS: LazyLock<RwLock<Vec<HangTermination>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// Publish a hang termination to the registry's ledger.
///
/// Called only by the idle detector. Nothing in this module may call it: no
/// registry-owned sweeper terminates anything.
pub fn record_hang_termination(label: &str, reason: &'static str, idle_secs: u64, elapsed_secs: u64, killed: usize) {
    let at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
    let mut ledger = HANG_TERMINATIONS.write();
    if ledger.len() >= HANG_TERMINATION_HISTORY {
        ledger.remove(0);
    }
    ledger.push(HangTermination {
        label: Arc::from(label),
        reason,
        idle_secs,
        elapsed_secs,
        killed,
        at_unix_ms,
    });
}

/// Recent hang terminations, oldest first.
#[must_use]
pub fn hang_terminations() -> Vec<HangTermination> {
    HANG_TERMINATIONS.read().clone()
}

// ── Connection-pool metrics ─────────────────────────────────────────────────

/// A pool that can report its own saturation counters.
///
/// Implemented by the SQLite and PostgreSQL connection pools. Pools register a
/// `Weak` reference at construction, so a pool that is dropped disappears from
/// the report without anyone having to deregister it.
pub trait PoolStatsSource: Send + Sync {
    /// Pool family (`sqlite` / `postgres`).
    fn pool_kind(&self) -> &'static str;
    /// Which database this pool serves.
    fn pool_name(&self) -> String;
    /// Counters, already flattened into JSON.
    fn pool_metrics(&self) -> serde_json::Value;
}

/// One pool's reported metrics.
#[derive(Debug, Clone)]
pub struct PoolSnapshot {
    pub kind: &'static str,
    pub name: String,
    pub metrics: serde_json::Value,
}

static POOLS: LazyLock<RwLock<Vec<std::sync::Weak<dyn PoolStatsSource>>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// Publish a pool's counters to `prx tasks pools` and `GET /api/runtime/pools`.
///
/// Only a `Weak` is retained: registration never keeps a pool alive and never
/// needs a matching deregistration.
pub fn register_pool(pool: &Arc<dyn PoolStatsSource>) {
    let mut pools = POOLS.write();
    pools.retain(|entry| entry.strong_count() > 0);
    pools.push(Arc::downgrade(pool));
}

/// Metrics for every pool still alive, plus the process-wide blocking pool.
#[must_use]
pub fn pool_snapshots() -> Vec<PoolSnapshot> {
    let blocking = super::blocking::snapshot();
    let mut snapshots = vec![PoolSnapshot {
        kind: "blocking",
        name: "tokio-blocking".to_string(),
        metrics: serde_json::json!({
            "max_threads": blocking.max_threads,
            "in_flight": blocking.in_flight,
            "peak_in_flight": blocking.peak_in_flight,
            "saturation_events": blocking.saturation_events,
            "spawned_total": blocking.spawned_total,
            "utilization_percent": blocking.utilization_percent(),
            "saturated": blocking.is_saturated(),
        }),
    }];

    let live = {
        let mut pools = POOLS.write();
        pools.retain(|entry| entry.strong_count() > 0);
        pools.iter().filter_map(std::sync::Weak::upgrade).collect::<Vec<_>>()
    };
    for pool in live {
        snapshots.push(PoolSnapshot {
            kind: pool.pool_kind(),
            name: pool.pool_name(),
            metrics: pool.pool_metrics(),
        });
    }
    snapshots
}

#[cfg(test)]
mod tests;
