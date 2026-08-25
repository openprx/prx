//! Idle (no-progress) hang detection for agent turns.
//!
//! # Why this exists, and what it deliberately is not
//!
//! PRX is an unbounded agent runtime: a turn has no wall-clock budget, because
//! a wall clock punishes *duration* and duration is not a fault. Long turns are
//! ordinary business here — a research run, a build, a large generation. Every
//! wall-clock turn timeout was removed for exactly that reason and must not
//! come back.
//!
//! What a wall clock did provide, badly, was recovery from a *hang*: a future
//! that will never return and will never error. That failure mode is real, it
//! is the documented motivation behind every "please add a timeout" report in
//! the wider ecosystem, and removing the wall clock removed the only thing that
//! ever recovered from it. This module puts the recovery back, keyed on the
//! right quantity:
//!
//! * a **wall-clock timeout** measures how long the turn has been *running*;
//! * this **idle detector** measures how long the turn has been *silent*.
//!
//! A turn that emits a token, starts or finishes a tool call, compacts its
//! history, or writes to a channel is alive by definition, and every one of
//! those events resets the window. A turn that has produced no observable event
//! at all for the whole window is not slow — it is wedged — and is terminated.
//!
//! The distinction is load-bearing, and there are two ways to destroy it:
//!
//! 1. **Stop resetting the window on progress.** That converts this detector
//!    into precisely the wall-clock turn timeout that was deleted. The test
//!    `long_running_turn_survives_while_it_keeps_making_progress` exists to go
//!    red the moment anyone does it.
//! 2. **Reinterpret [`IdleGuard::idle`] as a total budget.** It is not a
//!    ceiling on turn length and no default may ever be chosen as if it were.
//!    The only total ceiling is [`IdleGuard::max_total`], which is deliberately
//!    set a full day out — see its documentation.
//!
//! # Relationship to `[runtime] long_task_warn_secs`
//!
//! These two mechanisms look adjacent and are opposites. They must never be
//! merged, and neither may be re-expressed in terms of the other:
//!
//! | | `long_task_warn_secs` ([`crate::runtime::registry::warn_long_running`]) | `idle_hang_secs` (this module) |
//! |---|---|---|
//! | measures | elapsed run time | time since the last progress event |
//! | fires on | a task that is **long** | a task that is **silent** |
//! | refreshed by progress | no | **yes** |
//! | action | `warn!` only, **never terminates** | terminates the turn |
//! | a healthy 6-hour turn | is warned about, repeatedly | is never touched |
//!
//! So: `long_task_warn_secs` is an operator notification about duration and is
//! contractually forbidden from ending anything; `idle_hang_secs` is an
//! automatic recovery from a stall and is the only thing here that ends a turn.
//! The default thresholds are also deliberately different numbers (900 vs 1800)
//! so that a reader who sees one cannot mistake it for the other, and so that a
//! wedged turn is always *warned about* well before it is *terminated*.
//!
//! # What counts as progress
//!
//! Progress is *observable activity attributable to this turn*: provider stream
//! chunks, a completed provider round-trip, a tool call starting or finishing,
//! history compaction/paging, and output emitted to a channel or event sink.
//! Any single one of them resets the window; see [`ProgressKind`].
//!
//! Two things this definition intentionally does **not** do:
//!
//! * It does not attempt to distinguish *useful* progress from *futile*
//!   progress. A model that calls the same failing tool forever is emitting
//!   genuine events, so the runtime is not hung and this detector will not (and
//!   should not) fire. Non-convergence is a different fault with a different,
//!   already-present bound: `agent.max_tool_iterations`. Conflating the two
//!   would mean killing a healthy turn because we disliked its content, which
//!   is the wall-clock mistake wearing a different hat.
//! * It does not let one turn's activity keep another turn alive. The beat
//!   lives in a task-local ([`CURRENT_BEAT`]), so concurrent turns cannot
//!   refresh each other. The one deliberate exception is a *child* run linking
//!   to its spawner (see [`current_beat`] / [`child_beat`] / [`scope_beat`]), so
//!   a parent that is legitimately blocked on a working sub-agent is not
//!   mistaken for wedged. A member whose progress cannot cross a process
//!   boundary is covered by [`ProgressKind::SubtaskAlive`] instead; read that
//!   variant's documentation for why it is bounded.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::runtime::registry::{self, KillOutcome, WorkId};

/// Default idle window when `[runtime] idle_hang_secs` is unset.
///
/// 30 minutes of *complete silence*. Chosen against what this window actually
/// covers: not a single HTTP stream, but a whole turn including tool calls that
/// have no progress signal of their own (an opaque shell command, a blocking
/// MCP request). The comparable windows in the wider ecosystem for that same
/// "whole prompt round-trip, reset by any update" scope are 1800s (OpenHands
/// `acp_prompt_timeout`) and 1800s (Claude Code's stdio idle window); the 300s
/// figures elsewhere cover a narrower thing (an SSE stream between events).
///
/// It is also, deliberately, twice
/// [`crate::runtime::registry::DEFAULT_LONG_TASK_WARN_SECS`], so an operator
/// always sees the long-task warning long before anything is terminated.
///
/// This is **not** a bound on turn length. A turn that keeps producing events
/// runs forever regardless of this value.
pub const DEFAULT_IDLE_HANG_SECS: u64 = 1800;

/// Smallest enabled idle window.
///
/// Below this a single unremarkable tool call — a compile, a large download —
/// could pass without an intervening event and be mistaken for a hang. Anything
/// shorter is a foot-gun, not a tuning choice.
pub const MIN_IDLE_HANG_SECS: u64 = 30;

/// Default absolute ceiling on one turn when `[runtime] idle_hang_max_total_secs`
/// is unset: 24 hours.
///
/// This is the backstop for the one case the idle window cannot see: a turn that
/// keeps emitting faint, real events forever and so never goes silent. It is
/// **not** a turn-duration policy and must never be tuned as one. The value is
/// picked to be unreachable by legitimate work — the longest healthy agent turn
/// documented anywhere in the ecosystem survey this mechanism came from was
/// ~41 minutes, so a day leaves roughly 35x of headroom — and exists only so a
/// daemon that runs for months cannot accumulate immortal turns.
///
/// Set `0` to disable it entirely. Lowering it toward the idle window turns it
/// back into the wall-clock turn timeout this runtime does not have.
pub const DEFAULT_IDLE_HANG_MAX_TOTAL_SECS: u64 = 86_400;

/// Smallest enabled total ceiling: one hour.
///
/// Guards against the ceiling being quietly repurposed as a short turn budget.
pub const MIN_IDLE_HANG_MAX_TOTAL_SECS: u64 = 3_600;

/// Sentinel for "no configuration installed yet", so the built-in defaults
/// apply to code paths that run before (or without) `install`.
const NOT_INSTALLED: u64 = u64::MAX;

static INSTALLED_IDLE_SECS: AtomicU64 = AtomicU64::new(NOT_INSTALLED);
static INSTALLED_MAX_TOTAL_SECS: AtomicU64 = AtomicU64::new(NOT_INSTALLED);

/// Publish the configured thresholds process-wide.
///
/// Threading the values through every `run_tool_call_loop_outcome` call site
/// would touch six modules for a value that is uniform per process, so this
/// mirrors how `[runtime] long_task_warn_secs` is installed
/// ([`crate::runtime::registry::start_long_task_warner`]). Call it once during
/// dispatch; paths that never call it get the documented defaults.
pub fn install(idle_secs: Option<u64>, max_total_secs: Option<u64>) {
    INSTALLED_IDLE_SECS.store(idle_secs.unwrap_or(NOT_INSTALLED), Ordering::Relaxed);
    INSTALLED_MAX_TOTAL_SECS.store(max_total_secs.unwrap_or(NOT_INSTALLED), Ordering::Relaxed);
}

/// Thresholds a guarded turn runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleGuard {
    /// How long the turn may produce **no observable progress at all** before it
    /// is judged hung. `None` disables idle detection.
    ///
    /// Reset by every progress event — this is not a budget for the turn.
    pub idle: Option<Duration>,
    /// Absolute ceiling on the turn regardless of progress. `None` disables it.
    ///
    /// See [`DEFAULT_IDLE_HANG_MAX_TOTAL_SECS`] before touching this.
    pub max_total: Option<Duration>,
}

impl IdleGuard {
    /// Build from raw configuration values, applying defaults for `None` and
    /// treating `Some(0)` as "disabled".
    #[must_use]
    pub const fn resolve(idle_secs: Option<u64>, max_total_secs: Option<u64>) -> Self {
        Self {
            idle: match idle_secs {
                Some(0) => None,
                Some(secs) => Some(Duration::from_secs(secs)),
                None => Some(Duration::from_secs(DEFAULT_IDLE_HANG_SECS)),
            },
            max_total: match max_total_secs {
                Some(0) => None,
                Some(secs) => Some(Duration::from_secs(secs)),
                None => Some(Duration::from_secs(DEFAULT_IDLE_HANG_MAX_TOTAL_SECS)),
            },
        }
    }

    /// Both checks off — the guard degenerates to running the future directly.
    #[must_use]
    pub const fn is_disabled(self) -> bool {
        self.idle.is_none() && self.max_total.is_none()
    }
}

tokio::task_local! {
    /// Per-task override of the process-wide thresholds.
    static GUARD_OVERRIDE: IdleGuard;
}

/// Run `fut` with `guard` in force for every turn started inside it, in
/// preference to the process-wide configuration.
///
/// Scoped to the task rather than the process, so one caller tightening or
/// relaxing the window for a particular run cannot change the thresholds any
/// other concurrent run sees.
pub async fn with_guard<F>(guard: IdleGuard, fut: F) -> F::Output
where
    F: Future,
{
    GUARD_OVERRIDE.scope(guard, fut).await
}

/// The thresholds in force here: a task-scoped override if one is installed,
/// otherwise the process-wide configuration (or the defaults).
#[must_use]
pub fn configured() -> IdleGuard {
    if let Ok(scoped) = GUARD_OVERRIDE.try_with(|guard| *guard) {
        return scoped;
    }
    let read = |cell: &AtomicU64| match cell.load(Ordering::Relaxed) {
        NOT_INSTALLED => None,
        value => Some(value),
    };
    IdleGuard::resolve(read(&INSTALLED_IDLE_SECS), read(&INSTALLED_MAX_TOTAL_SECS))
}

/// Kind of event that reset the idle window, recorded so the termination report
/// can say what the turn was last seen doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressKind {
    /// The guard was installed; no progress observed yet.
    TurnStart,
    /// A chunk arrived on the provider stream (text, reasoning, or tool-call
    /// delta). This is connection health, the quantity a stream should be
    /// judged on.
    ProviderStream,
    /// A non-streaming provider round-trip completed.
    ProviderResponse,
    /// A tool call began executing.
    ToolStart,
    /// A tool call produced its result.
    ToolEnd,
    /// History compaction, summarisation, or OS-paging ran.
    Compaction,
    /// Output was handed to a channel, TUI, or event sink.
    ChannelOutput,
    /// A retry/backoff step was taken after a recoverable provider error.
    ProviderRetry,
    /// The registry subtree owned by this turn changed shape — work items or
    /// child processes appeared or went away, which only happens when something
    /// is running.
    RuntimeSubtree,
    /// This turn is parked on sub-agent runs that are still executing.
    ///
    /// The one kind whose evidence is *out of band*: it is not something the
    /// turn emitted, it is something a supervisor confirmed about the runs the
    /// turn is waiting on. It exists because a fan-out member can be a separate
    /// OS process whose only in-band signal is the bytes it writes, and a
    /// healthy `session-worker` writes nothing until it is finished — so a
    /// parent blocked on one would go silent while its child works perfectly,
    /// which is precisely the misdiagnosis this module's header forbids.
    ///
    /// Recording it is bounded, not a licence to run forever:
    ///
    /// * it is recorded only while a member is *provably* non-terminal, and
    ///   every way a member can end now commits a terminal status (see
    ///   `crate::tools::sessions_spawn`), so "still running" is an observation
    ///   rather than a stale default;
    /// * each member carries its own idle detection — a task-mode member runs
    ///   under [`run_guarded`], a process-mode member's worker installs these
    ///   same thresholds in its own process — so a wedged member is ended by
    ///   its own watchdog and the wait then finishes;
    /// * [`IdleGuard::max_total`] is unaffected, so the joining turn still has
    ///   an absolute ceiling.
    SubtaskAlive,
}

impl ProgressKind {
    /// Stable lowercase tag for logs and error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnStart => "turn_start",
            Self::ProviderStream => "provider_stream",
            Self::ProviderResponse => "provider_response",
            Self::ToolStart => "tool_start",
            Self::ToolEnd => "tool_end",
            Self::Compaction => "compaction",
            Self::ChannelOutput => "channel_output",
            Self::ProviderRetry => "provider_retry",
            Self::RuntimeSubtree => "runtime_subtree",
            Self::SubtaskAlive => "subtask_alive",
        }
    }

    const fn as_u8(self) -> u8 {
        match self {
            Self::TurnStart => 0,
            Self::ProviderStream => 1,
            Self::ProviderResponse => 2,
            Self::ToolStart => 3,
            Self::ToolEnd => 4,
            Self::Compaction => 5,
            Self::ChannelOutput => 6,
            Self::ProviderRetry => 7,
            Self::RuntimeSubtree => 8,
            Self::SubtaskAlive => 9,
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::ProviderStream,
            2 => Self::ProviderResponse,
            3 => Self::ToolStart,
            4 => Self::ToolEnd,
            5 => Self::Compaction,
            6 => Self::ChannelOutput,
            7 => Self::ProviderRetry,
            8 => Self::RuntimeSubtree,
            9 => Self::SubtaskAlive,
            _ => Self::TurnStart,
        }
    }
}

/// Liveness evidence for one guarded turn.
///
/// Shared by `Arc` and mutated with relaxed atomics: recording progress happens
/// on the hot path (once per stream chunk) and must cost two stores, while the
/// reader is a watchdog that only looks once per idle window. Exact ordering
/// between a beat and a deadline check is uninteresting — a beat racing the
/// deadline by microseconds is not the difference between a live turn and a
/// wedged one.
#[derive(Debug)]
pub struct ProgressBeat {
    origin: Instant,
    /// Milliseconds since [`Self::origin`] at the last recorded event.
    last_ms: AtomicU64,
    /// Total events recorded, reported so an operator can tell "never started"
    /// from "stopped after working".
    events: AtomicU64,
    /// [`ProgressKind::as_u8`] of the last recorded event.
    kind: AtomicU8,
    /// Spawner's beat, when this run was started by another guarded run. A
    /// child's progress counts as its parent's progress, because a parent
    /// blocked on a sub-agent that is visibly working is not wedged.
    parent: Option<Arc<Self>>,
}

impl ProgressBeat {
    fn new(parent: Option<Arc<Self>>) -> Self {
        Self {
            origin: Instant::now(),
            last_ms: AtomicU64::new(0),
            events: AtomicU64::new(0),
            kind: AtomicU8::new(ProgressKind::TurnStart.as_u8()),
            parent,
        }
    }

    fn stamp(&self, kind: ProgressKind) {
        let elapsed_ms = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(elapsed_ms, Ordering::Relaxed);
        self.events.fetch_add(1, Ordering::Relaxed);
        self.kind.store(kind.as_u8(), Ordering::Relaxed);
    }

    /// Record one progress event on this beat and every ancestor beat.
    ///
    /// Iterative rather than recursive: the chain length is the spawn nesting
    /// depth, but a loop makes that structurally irrelevant.
    pub fn record(&self, kind: ProgressKind) {
        self.stamp(kind);
        let mut ancestor = self.parent.as_deref();
        while let Some(beat) = ancestor {
            beat.stamp(kind);
            ancestor = beat.parent.as_deref();
        }
    }

    /// How long since the last recorded event.
    ///
    /// Public because a beat is also the liveness evidence a *supervisor* reads
    /// — `sessions_list` reports how long a sub-agent run has been silent — and
    /// that reader is in another module. It is a read of a relaxed atomic, so
    /// polling it costs nothing and can never disturb the turn it observes.
    #[must_use]
    pub fn idle_for(&self) -> Duration {
        let last = Duration::from_millis(self.last_ms.load(Ordering::Relaxed));
        self.origin.elapsed().saturating_sub(last)
    }

    /// Total progress events recorded, so a reader can tell "never started"
    /// from "stopped after working".
    #[must_use]
    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// What this beat was last seen doing.
    #[must_use]
    pub fn last_kind(&self) -> ProgressKind {
        ProgressKind::from_u8(self.kind.load(Ordering::Relaxed))
    }
}

tokio::task_local! {
    /// Beat of the guarded turn owning the current task.
    ///
    /// Task-local rather than global so concurrent turns are isolated: one
    /// turn's activity can never keep another turn's watchdog quiet.
    static CURRENT_BEAT: Arc<ProgressBeat>;
}

/// Record a progress event for the guarded turn owning this task.
///
/// A no-op outside a guarded turn, so instrumenting a shared code path costs a
/// task-local probe and nothing else.
pub fn beat(kind: ProgressKind) {
    let _ = CURRENT_BEAT.try_with(|current| current.record(kind));
}

/// The current task's beat, for handing to a task that will be spawned.
///
/// Task-locals do not cross `tokio::spawn`, so a child run started with
/// `tokio::spawn` must be given the parent's beat explicitly (see
/// [`scope_beat`]) or the parent — legitimately blocked on that child — would
/// look silent.
#[must_use]
pub fn current_beat() -> Option<Arc<ProgressBeat>> {
    CURRENT_BEAT.try_with(Arc::clone).ok()
}

/// Mint a beat for a run that will be driven from a task other than this one.
///
/// The new beat is parented on the caller's current beat, so the child's
/// progress still refreshes its spawner's window — the one deliberate exception
/// documented in this module's header. Call it on the *spawning* task: task
/// locals do not cross `tokio::spawn`, so a beat minted inside the spawned task
/// would silently have no parent.
///
/// Handing the returned handle to a supervisor (rather than only installing it
/// with [`scope_beat`]) is what lets a *third* party — `sessions_list`, a join
/// — read how long a run has been silent without participating in it.
#[must_use]
pub fn child_beat() -> Arc<ProgressBeat> {
    Arc::new(ProgressBeat::new(current_beat()))
}

/// Run `fut` with `beat` installed as the ambient beat, or unchanged when
/// `beat` is `None`.
pub async fn scope_beat<F>(beat: Option<Arc<ProgressBeat>>, fut: F) -> F::Output
where
    F: Future,
{
    match beat {
        Some(beat) => CURRENT_BEAT.scope(beat, fut).await,
        None => fut.await,
    }
}

/// Why a turn was terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HangReason {
    /// No observable progress for the whole idle window.
    NoProgress,
    /// Still making progress, but past the absolute ceiling.
    TotalRuntimeCap,
}

impl HangReason {
    /// Stable lowercase tag for logs, errors, and the registry ledger.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoProgress => "idle_no_progress",
            Self::TotalRuntimeCap => "total_runtime_cap",
        }
    }
}

/// Wording every no-progress termination message carries.
///
/// A shared constant because the *reader* of a terminated sub-agent's status is
/// on the far side of a `to_string()`: `sessions_spawn`'s join classification
/// only ever sees the rendered text, and it has to tell "the runtime ended a
/// stalled turn" (no conclusion was ever reached) from "the task concluded that
/// it failed". Keeping the phrase in one place is what stops those two from
/// drifting apart silently.
pub const HANG_TERMINATION_MARKER: &str = "was terminated as hung";

/// Wording every absolute-ceiling termination message carries.
///
/// Same contract as [`HANG_TERMINATION_MARKER`], for the other threshold: a
/// turn stopped at the ceiling also never reached a conclusion of its own.
pub const RUNTIME_CEILING_TERMINATION_MARKER: &str = "was terminated at the absolute runtime ceiling";

/// Whether a rendered error message describes a turn ended by this detector.
///
/// Text matching is not a shortcut here — it is the only channel available.
/// A sub-agent's outcome crosses into the run registry as
/// `SubAgentStatus::Failed(error.to_string())`, so the typed
/// [`IdleHangTerminated`] is already gone by the time anything downstream can
/// ask. The two markers are the constants the message is built from, so writer
/// and reader cannot disagree.
#[must_use]
pub fn message_describes_hang_termination(message: &str) -> bool {
    message.contains(HANG_TERMINATION_MARKER) || message.contains(RUNTIME_CEILING_TERMINATION_MARKER)
}

/// Error returned when a turn is terminated by this detector.
///
/// Deliberately its own type, so the three ways a turn can stop stay
/// distinguishable at the call site and in the log:
///
/// * this type — the turn **hung** and the runtime ended it;
/// * `ToolLoopCancelled` — an operator or a parent cancelled it (`prx tasks
///   kill`, shutdown, `/stop`);
/// * anything else — the task **failed** on its own terms.
#[derive(Debug, Clone)]
pub struct IdleHangTerminated {
    /// Which threshold was crossed.
    pub reason: HangReason,
    /// Human label of the guarded turn.
    pub label: Arc<str>,
    /// Silence at the moment of the verdict.
    pub idle: Duration,
    /// Threshold that was crossed.
    pub threshold: Duration,
    /// Total run time at the moment of the verdict.
    pub elapsed: Duration,
    /// How many progress events the turn produced before going quiet.
    pub progress_events: u64,
    /// What the turn was last seen doing.
    pub last_progress: ProgressKind,
    /// Registry work items that were signalled as part of the termination.
    pub killed: usize,
}

impl std::fmt::Display for IdleHangTerminated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            HangReason::NoProgress => write!(
                f,
                "agent turn '{}' {HANG_TERMINATION_MARKER}: no observable progress for {}s \
                 (idle threshold {}s, `[runtime] idle_hang_secs`). Last progress: {} after \
                 {} event(s); total run time {}s; {} runtime work item(s) signalled. \
                 This is a stall, not a failure and not an operator kill — the turn's \
                 duration alone would never have ended it.",
                self.label,
                self.idle.as_secs(),
                self.threshold.as_secs(),
                self.last_progress.as_str(),
                self.progress_events,
                self.elapsed.as_secs(),
                self.killed,
            ),
            HangReason::TotalRuntimeCap => write!(
                f,
                "agent turn '{}' {RUNTIME_CEILING_TERMINATION_MARKER} of {}s \
                 (`[runtime] idle_hang_max_total_secs`) after {}s and {} progress event(s); \
                 {} runtime work item(s) signalled. The turn was still emitting events, so \
                 this is a backstop against a run that never converges, not a turn-duration \
                 policy.",
                self.label,
                self.threshold.as_secs(),
                self.elapsed.as_secs(),
                self.progress_events,
                self.killed,
            ),
        }
    }
}

impl std::error::Error for IdleHangTerminated {}

/// Whether `error` is a turn terminated by this detector, as opposed to a task
/// failure or an operator kill.
#[must_use]
pub fn is_hang_termination(error: &anyhow::Error) -> bool {
    error.chain().any(|source| source.is::<IdleHangTerminated>())
}

/// Minimum re-arm delay, so a verdict that turns out not to be due cannot spin
/// the watchdog.
const WATCHDOG_MIN_STEP: Duration = Duration::from_millis(5);

/// Run `fut` as a guarded turn.
///
/// Returns `fut`'s own result untouched whenever it completes — success,
/// failure, or cancellation. Only when the turn goes silent past
/// [`IdleGuard::idle`] (or runs past [`IdleGuard::max_total`]) does this replace
/// the outcome with an [`IdleHangTerminated`] error.
///
/// The watchdog runs inline in the caller's task rather than in a spawned one,
/// which is what makes the cleanup total: returning from here drops `fut`, and
/// dropping `fut` drops every future nested inside it, which drops the RAII
/// guards that own child processes (`ManagedShellChild` kills its whole process
/// group on drop). That path works even against a tool that ignores
/// cancellation entirely, which is exactly the population this detector exists
/// to catch.
///
/// **Box a large `fut` before passing it in.** This function's own future holds
/// `fut` inline, so handing it the agent loop's future unboxed places a second
/// copy of one of the biggest futures in the process into the caller's frame —
/// which overflowed the `prx chat` worker stack when this was first written.
/// The agent-loop call site boxes for exactly that reason.
pub async fn run_guarded<F, T>(
    guard: IdleGuard,
    label: &str,
    cancel: Option<&CancellationToken>,
    fut: F,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    if guard.is_disabled() {
        return fut.await;
    }

    let beat = Arc::new(ProgressBeat::new(current_beat()));
    let started = Instant::now();
    let root = registry::current_work_id();
    let mut subtree = subtree_fingerprint(root);

    let guarded = CURRENT_BEAT.scope(Arc::clone(&beat), fut);
    tokio::pin!(guarded);

    loop {
        let step = next_check_delay(guard, &beat, started);
        tokio::select! {
            // Progress first: if the turn is ready, it wins the race outright,
            // even on a tick where the deadline has also come due.
            biased;
            outcome = &mut guarded => return outcome,
            () = tokio::time::sleep(step) => {}
        }

        // The registry subtree changing shape means tool calls or child
        // processes came and went, which only a live turn can do. Counted as
        // progress so a turn blocked on a long tool that is itself spawning
        // work is never mistaken for silent.
        let observed = subtree_fingerprint(root);
        if observed != subtree {
            subtree = observed;
            beat.record(ProgressKind::RuntimeSubtree);
        }

        let Some((reason, threshold)) = verdict(guard, &beat, started) else {
            continue;
        };
        return Err(terminate(reason, threshold, label, cancel, &beat, started).await);
    }
}

/// How long the watchdog may sleep before the earliest threshold could be due.
fn next_check_delay(guard: IdleGuard, beat: &ProgressBeat, started: Instant) -> Duration {
    let until_idle = guard.idle.map(|window| window.saturating_sub(beat.idle_for()));
    let until_total = guard.max_total.map(|cap| cap.saturating_sub(started.elapsed()));
    let step = match (until_idle, until_total) {
        (Some(idle), Some(total)) => idle.min(total),
        (Some(only), None) | (None, Some(only)) => only,
        // `is_disabled` already returned; this arm is unreachable in practice
        // and resolves to a harmless long nap rather than a busy loop.
        (None, None) => Duration::from_secs(DEFAULT_IDLE_HANG_SECS),
    };
    step.max(WATCHDOG_MIN_STEP)
}

/// Whether a threshold is actually due right now, and which one.
fn verdict(guard: IdleGuard, beat: &ProgressBeat, started: Instant) -> Option<(HangReason, Duration)> {
    // The idle check reads `beat.idle_for()`, never `started.elapsed()`.
    //
    // MUTATION GUARD: swapping it for `started.elapsed()` turns this detector
    // into the wall-clock turn timeout this runtime deliberately does not have,
    // and `long_running_turn_survives_while_it_keeps_making_progress` goes red.
    if let Some(window) = guard.idle
        && beat.idle_for() >= window
    {
        return Some((HangReason::NoProgress, window));
    }
    if let Some(cap) = guard.max_total
        && started.elapsed() >= cap
    {
        return Some((HangReason::TotalRuntimeCap, cap));
    }
    None
}

/// Signal everything the turn owns, publish the event, and build the error.
async fn terminate(
    reason: HangReason,
    threshold: Duration,
    label: &str,
    cancel: Option<&CancellationToken>,
    beat: &ProgressBeat,
    started: Instant,
) -> anyhow::Error {
    // Cooperative cancellation first: tool-call tokens are children of the turn
    // token, so well-behaved work unwinds itself and reaches its own cleanup.
    if let Some(token) = cancel {
        token.cancel();
    }
    // Then the registry's own kill path — the same one `prx tasks kill` uses —
    // for anything that will not unwind on its own. Killing by process *group*
    // is what keeps forked grandchildren from escaping, and routing through the
    // registry is what makes the termination visible in `prx tasks list`.
    let killed = kill_turn_subtree().await;

    let terminated = IdleHangTerminated {
        reason,
        label: Arc::from(label),
        idle: beat.idle_for(),
        threshold,
        elapsed: started.elapsed(),
        progress_events: beat.events(),
        last_progress: beat.last_kind(),
        killed,
    };

    tracing::error!(
        reason = reason.as_str(),
        label = label,
        idle_secs = terminated.idle.as_secs(),
        threshold_secs = threshold.as_secs(),
        elapsed_secs = terminated.elapsed.as_secs(),
        progress_events = terminated.progress_events,
        last_progress = terminated.last_progress.as_str(),
        killed = killed,
        "agent turn terminated by the idle (no-progress) detector"
    );
    registry::record_hang_termination(
        label,
        reason.as_str(),
        terminated.idle.as_secs(),
        terminated.elapsed.as_secs(),
        killed,
    );

    anyhow::Error::new(terminated)
}

/// Kill every registry item the turn owns, without killing the turn's own row.
///
/// The turn's own row is skipped on purpose: [`registry::kill`] aborts the task
/// behind an item, and aborting the task that is running this function would
/// destroy it before it can report *why* the turn ended — the one thing that
/// distinguishes a hang from a plain cancellation.
///
/// Returns how many items were confirmed killed or had termination issued.
async fn kill_turn_subtree() -> usize {
    let Some(root) = registry::current_work_id() else {
        return 0;
    };
    let children: Vec<WorkId> = registry::snapshot_all()
        .into_iter()
        .filter(|item| item.parent == Some(root))
        .map(|item| item.id)
        .collect();
    let mut killed = 0_usize;
    for child in children {
        for result in registry::kill(child, true).await {
            if matches!(result.outcome, KillOutcome::Killed | KillOutcome::Requested) {
                killed = killed.saturating_add(1);
            }
        }
    }
    killed
}

/// Cheap shape signature of the registry subtree below `root`.
///
/// `None` when the turn is not itself a registered work item, in which case
/// only explicit progress events are available. Work ids are monotonic and
/// never reused, so a changed signature means real churn rather than a
/// coincidence.
fn subtree_fingerprint(root: Option<WorkId>) -> Option<u64> {
    let root = root?;
    let all = registry::snapshot_all();
    let mut selected: Vec<u64> = vec![root.raw()];
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for item in &all {
            if item.parent == Some(parent) && !selected.contains(&item.id.raw()) {
                selected.push(item.id.raw());
                frontier.push(item.id);
            }
        }
    }
    Some(
        selected
            .iter()
            .fold(selected.len() as u64, |acc, id| acc.rotate_left(7) ^ id),
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn guard_ms(idle_ms: u64, total_ms: Option<u64>) -> IdleGuard {
        IdleGuard {
            idle: Some(Duration::from_millis(idle_ms)),
            max_total: total_ms.map(Duration::from_millis),
        }
    }

    /// Core evidence 1 — a genuinely hung turn is terminated, the error says so,
    /// and the child process it left behind is cleaned up through the registry's
    /// `killpg` path.
    ///
    /// The child is deliberately leaked out of Rust's ownership
    /// (`std::mem::forget`), which disables `kill_on_drop` and the RAII
    /// cleanup. After that, the *only* thing that can reap it is
    /// `registry::kill` signalling its process group — so the assertion below
    /// tests the kill path itself and not the drop path that happens to also
    /// exist.
    #[tokio::test]
    async fn hung_turn_is_terminated_and_its_child_process_group_is_killed() {
        let turn_token = CancellationToken::new();
        let turn =
            registry::register_sub_agent("idle-test-turn", "idle-test-run", None, None, Some(turn_token.clone()));

        let observed_pid = Arc::new(AtomicU64::new(0));
        let pid_sink = Arc::clone(&observed_pid);
        let token_for_turn = turn_token.clone();

        let tool_token = turn_token.child_token();
        let result: anyhow::Result<()> = registry::scoped(turn, async move {
            run_guarded(guard_ms(200, None), "hung-turn", Some(&token_for_turn), async move {
                let tool = registry::register_tool_call("hang_tool", None, Some(tool_token));
                let tool_id = tool.id();
                registry::scope_current(tool_id, async move {
                    let mut command = tokio::process::Command::new("sleep");
                    command
                        .arg("30")
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null());
                    #[cfg(unix)]
                    command.process_group(0);
                    let child = command.spawn().unwrap();
                    let pid = child.id().unwrap_or(0);
                    let _process =
                        registry::register_process("sleep 30 (idle-test)", Some(pid), i32::try_from(pid).ok());
                    pid_sink.store(u64::from(pid), Ordering::SeqCst);
                    // `kill_on_drop` was never set and the handle is leaked, so
                    // no RAII path can reclaim this child: only the registry's
                    // process-group kill can.
                    std::mem::forget(child);
                    // The hang itself: a future that never returns and never
                    // errors, producing no progress of any kind.
                    let () = std::future::pending().await;
                    // Held for the whole (never-ending) call.
                    drop(tool);
                    Ok(())
                })
                .await
            })
            .await
        })
        .await;

        let error = result.expect_err("a turn that never progresses must be terminated");
        assert!(
            is_hang_termination(&error),
            "the error must be identifiable as a hang termination, got: {error}"
        );
        let detail = error
            .downcast_ref::<IdleHangTerminated>()
            .expect("hang terminations carry their own error type");
        assert_eq!(detail.reason, HangReason::NoProgress);
        // The turn emitted no event of its own. The single recorded beat is the
        // registry subtree changing shape when the tool call and its child
        // process registered, which correctly bought the turn one more window
        // before the verdict — and then the silence was real.
        assert!(
            matches!(
                detail.last_progress,
                ProgressKind::TurnStart | ProgressKind::RuntimeSubtree
            ),
            "a hung turn must never have been seen streaming, calling tools, or emitting output; got {}",
            detail.last_progress.as_str()
        );
        assert!(detail.killed >= 1, "the in-flight tool call must have been killed");
        let rendered = error.to_string();
        assert!(
            rendered.contains("terminated as hung") && rendered.contains("no observable progress"),
            "the message must distinguish a hang from a failure or a kill: {rendered}"
        );
        assert!(
            turn_token.is_cancelled(),
            "termination must go through the existing cancellation mechanism"
        );

        // The termination is visible in the runtime registry, not only in the
        // error returned to one caller.
        let ledger = registry::hang_terminations();
        let recorded = ledger
            .iter()
            .rfind(|entry| entry.label.as_ref() == "hung-turn")
            .expect("the termination must be recorded in the registry ledger");
        assert_eq!(recorded.reason, HangReason::NoProgress.as_str());
        assert_eq!(recorded.killed, detail.killed);

        let pid = u32::try_from(observed_pid.load(Ordering::SeqCst)).unwrap_or(0);
        assert!(pid > 0, "the test child must have started");
        let mut gone = false;
        for _ in 0..100 {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() || process_is_zombie(pid) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "the leaked child process ({pid}) must have been killed by killpg");
    }

    fn process_is_zombie(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rfind(')')
                .and_then(|index| stat.get(index.saturating_add(1)..))
                .and_then(|rest| rest.split_whitespace().next())
                .is_some_and(|state| state == "Z")
        })
    }

    /// Core evidence 2 — the whole point of the mechanism.
    ///
    /// A turn that runs an order of magnitude longer than the idle window, but
    /// keeps producing progress, must finish untouched. This is what separates
    /// an idle detector from the wall-clock timeout that was removed.
    ///
    /// MUTATION: make the idle check read total elapsed time instead of time
    /// since the last beat (see [`verdict`]) and this test fails.
    #[tokio::test]
    async fn long_running_turn_survives_while_it_keeps_making_progress() {
        let idle = Duration::from_millis(300);
        // 15 beats one third of the window apart => ~1.5s of run time, five
        // times the idle window, and never more than 100ms of silence.
        let outcome: anyhow::Result<u32> = run_guarded(
            IdleGuard {
                idle: Some(idle),
                max_total: None,
            },
            "long-progressing-turn",
            None,
            async {
                let mut ticks = 0_u32;
                for _ in 0..15 {
                    tokio::time::sleep(idle / 3).await;
                    beat(ProgressKind::ProviderStream);
                    ticks += 1;
                }
                Ok(ticks)
            },
        )
        .await;

        let ticks = outcome.expect("a turn that keeps making progress must never be terminated");
        assert_eq!(ticks, 15, "the turn must run to its natural end");
    }

    /// The same shape as the test above, but with the progress removed: this is
    /// the control proving the previous test's duration really is past the
    /// threshold, so its green is not just "it finished too fast to notice".
    #[tokio::test]
    async fn the_same_duration_without_progress_is_terminated() {
        let idle = Duration::from_millis(300);
        let outcome: anyhow::Result<u32> = run_guarded(
            IdleGuard {
                idle: Some(idle),
                max_total: None,
            },
            "long-silent-turn",
            None,
            async {
                for _ in 0..15 {
                    tokio::time::sleep(idle / 3).await;
                }
                Ok(15)
            },
        )
        .await;

        let error = outcome.expect_err("silence for the whole window is a hang");
        assert!(is_hang_termination(&error), "got: {error}");
    }

    /// Core evidence 3 — the long-task warning and the idle detector do not
    /// interfere: the warning fires on a live item, terminates nothing, and the
    /// guarded turn running alongside it completes normally.
    #[tokio::test]
    async fn long_task_warning_warns_without_terminating_anything() {
        let token = CancellationToken::new();
        let work = registry::register_sub_agent("warn-only-item", "warn-run", None, None, Some(token.clone()));
        let id = work.id();

        // A zero threshold makes every live item "long", which is the strongest
        // form of the question: even then, nothing may be terminated.
        let warned = registry::warn_long_running(Duration::ZERO);
        assert!(warned >= 1, "the sweeper must have reported at least this item");

        let snapshot = registry::snapshot(id).expect("the warned item must still be registered");
        assert_eq!(
            snapshot.state,
            registry::WorkState::Running,
            "the long-task warning must never move an item out of Running"
        );
        assert!(
            !token.is_cancelled(),
            "the long-task warning must never cancel anything"
        );

        // And a guarded turn is unaffected by the warning sweeper.
        let outcome: anyhow::Result<()> =
            run_guarded(guard_ms(400, None), "warned-but-progressing", Some(&token), async {
                for _ in 0..6 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    registry::warn_long_running(Duration::ZERO);
                    beat(ProgressKind::ToolEnd);
                }
                Ok(())
            })
            .await;
        assert!(
            outcome.is_ok(),
            "the long-task warning must not push a progressing turn into a hang verdict"
        );
        assert!(!token.is_cancelled(), "nothing may have been cancelled");
        drop(work);
    }

    /// The absolute ceiling fires even while progress keeps arriving, and says
    /// so — it is a different verdict from a stall.
    #[tokio::test]
    async fn total_ceiling_stops_a_turn_that_never_converges() {
        let outcome: anyhow::Result<()> = run_guarded(guard_ms(10_000, Some(300)), "never-converges", None, async {
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                beat(ProgressKind::ProviderStream);
            }
        })
        .await;

        let error = outcome.expect_err("the ceiling must stop a run that never ends");
        let detail = error
            .downcast_ref::<IdleHangTerminated>()
            .expect("the ceiling produces a hang termination error");
        assert_eq!(detail.reason, HangReason::TotalRuntimeCap);
        assert!(detail.progress_events > 0, "the turn was progressing the whole time");
        assert!(error.to_string().contains("absolute runtime ceiling"), "{error}");
    }

    /// A turn that fails or is cancelled keeps its own error: the guard never
    /// relabels an ordinary outcome as a hang.
    #[tokio::test]
    async fn ordinary_outcomes_are_passed_through_untouched() {
        let failed: anyhow::Result<()> = run_guarded(guard_ms(5_000, None), "failing-turn", None, async {
            Err(anyhow::anyhow!("tool exploded"))
        })
        .await;
        let error = failed.expect_err("the inner failure must survive");
        assert!(!is_hang_termination(&error), "a task failure is not a hang");
        assert_eq!(error.to_string(), "tool exploded");

        let ok: anyhow::Result<u8> = run_guarded(guard_ms(5_000, None), "fast-turn", None, async { Ok(7) }).await;
        assert_eq!(ok.expect("a fast turn is untouched"), 7);
    }

    /// Progress in one turn cannot keep another turn's watchdog quiet.
    #[tokio::test]
    async fn one_turns_progress_does_not_refresh_another_turns_window() {
        let noisy = async {
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                beat(ProgressKind::ProviderStream);
            }
            Ok(())
        };
        let silent = async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            Ok(())
        };

        let (noisy, silent): (anyhow::Result<()>, anyhow::Result<()>) = tokio::join!(
            run_guarded(guard_ms(300, None), "noisy", None, noisy),
            run_guarded(guard_ms(300, None), "silent", None, silent),
        );
        assert!(noisy.is_ok(), "the progressing turn survives");
        let error = silent.expect_err("the silent turn is terminated regardless of its neighbour");
        assert!(is_hang_termination(&error), "got: {error}");
    }

    /// A child run's progress counts for its spawner, so a parent blocked on a
    /// working sub-agent is not judged silent.
    #[tokio::test]
    async fn child_run_progress_keeps_its_spawner_alive() {
        let outcome: anyhow::Result<()> = run_guarded(guard_ms(300, None), "parent-turn", None, async {
            let inherited = current_beat();
            let child = tokio::spawn(scope_beat(inherited, async {
                for _ in 0..15 {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    beat(ProgressKind::ToolEnd);
                }
            }));
            child.await.map_err(anyhow::Error::new)?;
            Ok(())
        })
        .await;
        assert!(
            outcome.is_ok(),
            "a parent blocked on a visibly working child must not be terminated"
        );
    }

    /// b1's foundation: a supervisor mints a beat, hands it to a run, and then
    /// reads it from outside. That only works because the run's own guarded turn
    /// parents *its* beat on the handed one — so the handed beat is refreshed by
    /// the shared `ProgressKind` vocabulary and by nothing else.
    ///
    /// MUTATION GUARD: stop parenting the guard's beat on the ambient beat and
    /// this goes red, taking every "is the sub-agent still alive?" answer with
    /// it.
    #[tokio::test]
    async fn a_handed_beat_is_refreshed_by_the_guarded_turn_installed_under_it() {
        let handed = child_beat();
        assert_eq!(handed.events(), 0, "a fresh beat has seen nothing");

        let outcome: anyhow::Result<()> = scope_beat(
            Some(Arc::clone(&handed)),
            run_guarded(guard_ms(10_000, None), "child-turn", None, async {
                beat(ProgressKind::ToolStart);
                tokio::time::sleep(Duration::from_millis(30)).await;
                beat(ProgressKind::ToolEnd);
                Ok(())
            }),
        )
        .await;

        assert!(outcome.is_ok(), "the turn itself must be unaffected");
        assert!(
            handed.events() >= 2,
            "every progress event must reach the handed beat, got {}",
            handed.events()
        );
        assert_eq!(
            handed.last_kind(),
            ProgressKind::ToolEnd,
            "the handed beat must report what the run was last seen doing"
        );
        assert!(
            handed.idle_for() < Duration::from_millis(500),
            "the handed beat must show the run as recently active"
        );
    }

    #[test]
    fn thresholds_resolve_defaults_and_honour_the_disable_switch() {
        let defaults = IdleGuard::resolve(None, None);
        assert_eq!(defaults.idle, Some(Duration::from_secs(DEFAULT_IDLE_HANG_SECS)));
        assert_eq!(
            defaults.max_total,
            Some(Duration::from_secs(DEFAULT_IDLE_HANG_MAX_TOTAL_SECS))
        );

        assert_eq!(
            IdleGuard::resolve(Some(0), Some(0)),
            IdleGuard {
                idle: None,
                max_total: None
            }
        );
        assert!(IdleGuard::resolve(Some(0), Some(0)).is_disabled());
        assert_eq!(
            IdleGuard::resolve(Some(90), Some(7_200)).idle,
            Some(Duration::from_secs(90))
        );

        // The idle default must stay clear of the long-task warning default, so
        // an operator always gets a warning well before anything is terminated.
        assert!(
            DEFAULT_IDLE_HANG_SECS > crate::runtime::registry::DEFAULT_LONG_TASK_WARN_SECS,
            "the idle kill threshold must sit above the warn-only threshold"
        );
    }

    #[tokio::test]
    async fn a_disabled_guard_runs_the_future_directly() {
        let outcome: anyhow::Result<u8> = run_guarded(
            IdleGuard {
                idle: None,
                max_total: None,
            },
            "unguarded",
            None,
            async {
                tokio::time::sleep(Duration::from_millis(30)).await;
                Ok(3)
            },
        )
        .await;
        assert_eq!(outcome.expect("a disabled guard never terminates"), 3);
    }
}
