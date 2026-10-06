//! Shared trap recovery for WASM capability adapters.
//!
//! Every capability adapter owns one wasmtime `Store` and one component
//! instance. Wasmtime poisons the whole `Store` when anything unwinds out of
//! wasm — a trap, a failed `memory.grow`, fuel exhaustion, an epoch interrupt,
//! or a dropped `call_async` future — and a poisoned store refuses every later
//! entry with `cannot enter component instance`. Re-instantiating into that
//! store does not clear the flag.
//!
//! [`Supervisor`] therefore owns the store together with a small state machine:
//! a poisoned adapter is marked dead and the next call rebuilds it on a
//! brand-new `Store`; rebuilds are budgeted inside a rolling window; once the
//! budget is spent the adapter is quarantined and stays refused until the next
//! plugin generation replaces it.
//!
//! The state, the budget and every transition live inside the one
//! `tokio::sync::Mutex` that guards the store, so a caller queued behind a
//! trapping caller observes the transition the moment it acquires the lock and
//! is never admitted into a poisoned store. A caller whose own future is
//! dropped mid-call is covered by [`GuestCall`], whose `Drop` records the
//! poisoning while the store mutex is still held.
//!
//! What a failure *means* to the caller — fail open for middleware and hooks,
//! a typed error for tools, cron jobs, providers and storage — is decided by
//! each adapter, not here.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tokio::sync::Mutex;

use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::event_bus::EventBus;
use crate::plugins::host::{HostState, apply_store_epoch_deadline, apply_store_resource_limits};
use crate::plugins::manifest::PluginManifest;

/// Charged rebuild attempts allowed inside one restart window before the
/// adapter is quarantined. A failed attempt counts like a successful one; a
/// rebuild after a caller cancellation is not charged at all.
pub(crate) const MAX_RESTARTS_PER_WINDOW: usize = 3;

/// Shortest rolling window the restart budget is counted in.
pub(crate) const RESTART_WINDOW: Duration = Duration::from_mins(1);

/// The rolling window of one adapter: `max(RESTART_WINDOW, 4 x timeout)`.
///
/// A fixed minute cannot catch a guest that hangs: each hung call holds the
/// store for the whole host timeout before it is marked dead, so charged
/// rebuilds are at least one timeout apart, and at the 30 s default no three
/// of them ever share a minute. Four timeouts span the budget's three
/// rebuilds plus the call that started them, so a guest that hangs on every
/// call, or three times and then answers once, falls inside the window — but
/// only while it is called back to back: the fourth claim is refused only if
/// the idle time between calls stays under about a third of the timeout (for
/// timeouts of 15 s or more; a shorter timeout gets the 60 s floor, which
/// tolerates more idle than that). A
/// guest that hangs on every call at a slower pace is caught by the timeout
/// streak instead (`MAX_CONSECUTIVE_TIMEOUTS`). Faults further apart than the
/// window — an hourly job whose guest traps through an outage — never share
/// one and are retried indefinitely, as a transient fault should be.
fn restart_window(timeout_ms: u64) -> Duration {
    let per_call = Duration::from_millis(timeout_ms);
    let span = u32::try_from(MAX_RESTARTS_PER_WINDOW + 1).map_or(Duration::MAX, |calls| {
        per_call.checked_mul(calls).unwrap_or(Duration::MAX)
    });
    span.max(RESTART_WINDOW)
}

/// Timeouts in a row, with no guest call returning in between, after which
/// the adapter is quarantined at once, however far apart they are. Both forms
/// count: the host timer while the guest waits in a host call, and the epoch
/// deadline interrupting a guest that computes in wasm (see [`unwound`]).
///
/// Every guest call runs detached from its caller, so a caller can no longer
/// cancel one: a timeout is always the guest's own hang. The restart window
/// only catches a hanging guest called back to back; this catches it at any
/// pace. Only timeouts count. Any other trap is not a hang, so a trap outage spaced
/// beyond the window still recovers; a completed call, or a guest-returned
/// error (the guest ran and answered), ends the streak. The trade-off is
/// deliberate: an upstream that makes a guest hang on four calls in a row —
/// a black-holed endpoint through an outage — quarantines the adapter until
/// the next plugin generation (`refresh` or `reload`).
const MAX_CONSECUTIVE_TIMEOUTS: u64 = 4;

/// Failure warnings emitted per state before they are suppressed. The
/// suppressed total is reported on the next transition and in status.
const MAX_FAILURE_LOGS_PER_STATE: u64 = 3;

/// Characters of `last_error` kept in status. The reason can carry text the
/// guest itself authored, and status is read by the model through
/// `wasm_plugins_status`, so the field is bounded at the point it is recorded.
pub(crate) const MAX_LAST_ERROR_CHARS: usize = 512;

/// Appended to a `last_error` that was cut at `MAX_LAST_ERROR_CHARS`.
pub(crate) const LAST_ERROR_TRUNCATION_SUFFIX: &str = " ... (truncated)";

const STATE_LIVE: u8 = 0;
const STATE_RESTARTING: u8 = 1;
const STATE_QUARANTINED: u8 = 2;

/// The WASM capability an adapter serves. It names the adapter in logs, in
/// status and in the quarantine event topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilityKind {
    Middleware,
    Tool,
    Hook,
    Cron,
    Provider,
    Storage,
}

impl CapabilityKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Middleware => "middleware",
            Self::Tool => "tool",
            Self::Hook => "hook",
            Self::Cron => "cron",
            Self::Provider => "provider",
            Self::Storage => "storage",
        }
    }

    /// Published once when an adapter of this kind enters quarantine. The
    /// developer guide documents `prx.lifecycle.*` for agent lifecycle events;
    /// `prx.plugin.*` is used for adapter health, one topic per kind with the
    /// kind repeated in the payload, so both a per-kind and a wildcard
    /// subscription work.
    const fn quarantine_topic(self) -> &'static str {
        match self {
            Self::Middleware => "prx.plugin.middleware_quarantined",
            Self::Tool => "prx.plugin.tool_quarantined",
            Self::Hook => "prx.plugin.hook_quarantined",
            Self::Cron => "prx.plugin.cron_quarantined",
            Self::Provider => "prx.plugin.provider_quarantined",
            Self::Storage => "prx.plugin.storage_quarantined",
        }
    }

    /// Noun used in the instantiation error, kept identical to the message
    /// each adapter produced before recovery was shared.
    const fn instantiation_noun(self) -> &'static str {
        match self {
            Self::Middleware => "middleware",
            Self::Tool => "component",
            Self::Hook => "hook",
            Self::Cron => "cron",
            Self::Provider => "provider",
            Self::Storage => "storage plugin",
        }
    }
}

/// Published on every rebuild of an adapter that announces its rebuilds. A
/// storage guest that keeps its entries in its own linear memory comes back
/// empty from a rebuild, so its consumers must be able to learn of the reset
/// without polling status.
const STORAGE_RESTARTED_TOPIC: &str = "prx.plugin.storage_restarted";

/// Transitions made inside the critical section that are announced on the
/// event bus once the store lock is released.
#[derive(Default)]
struct Announcements {
    quarantined: Option<String>,
    restarted: Option<(String, u64)>,
    /// This call rebuilt the store before entering the guest.
    rebuilt: bool,
}

/// Registers the host functions a capability's guests import.
pub(crate) type LinkHostFunctions = fn(&mut wasmtime::component::Linker<HostState>) -> PluginResult<()>;

/// One store plus the component instance living in it.
pub(crate) struct LiveInstance {
    pub(crate) store: wasmtime::Store<HostState>,
    pub(crate) instance: wasmtime::component::Instance,
}

/// Everything an adapter's constructor received, kept so the adapter can
/// rebuild itself on a fresh store without going back to the plugin manager.
/// The engine and the precompiled component are both cheap handle clones, so a
/// rebuild is an instantiation, never a recompilation.
pub(crate) struct Respawn {
    kind: CapabilityKind,
    engine: wasmtime::Engine,
    component: wasmtime::component::Component,
    manifest: PluginManifest,
    granted_permissions: std::collections::HashSet<String>,
    memory: Option<Arc<dyn crate::memory::traits::Memory>>,
    event_bus: Option<Arc<EventBus>>,
    link: LinkHostFunctions,
    /// The adapter's KV namespace. It lives in host state, not in the guest,
    /// so every store the adapter builds shares it and data a guest wrote
    /// through the `kv` host functions survives a rebuild.
    kv_store: KvNamespace,
    /// Delivery path into a hook guest's event handler. Every store the
    /// adapter builds gets the same sink, so a subscription registered before
    /// a rebuild keeps delivering after it.
    event_sink: Option<tokio::sync::mpsc::Sender<crate::plugins::event_bus::EventMessage>>,
}

/// A plugin's host-side KV namespace.
pub(crate) type KvNamespace = Arc<tokio::sync::RwLock<std::collections::HashMap<String, Vec<u8>>>>;

impl Respawn {
    /// Capture the construction inputs of one adapter. `granted_permissions`
    /// must come from `LoadedPlugin.granted_permissions` (policy-filtered),
    /// NOT directly from the manifest.
    pub(crate) fn new(
        kind: CapabilityKind,
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        granted_permissions: std::collections::HashSet<String>,
        memory: Option<Arc<dyn crate::memory::traits::Memory>>,
        event_bus: Option<Arc<EventBus>>,
        link: LinkHostFunctions,
    ) -> Self {
        Self {
            kind,
            engine: engine.clone(),
            component: component.clone(),
            manifest: manifest.clone(),
            granted_permissions,
            memory,
            event_bus,
            link,
            kv_store: Arc::default(),
            event_sink: None,
        }
    }

    /// Install the bounded sink a hook guest's subscriptions deliver into.
    pub(crate) fn with_event_sink(
        mut self,
        sink: tokio::sync::mpsc::Sender<crate::plugins::event_bus::EventMessage>,
    ) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Build a brand-new store, arm every manifest limit on it, and instantiate
    /// the component into it. Constructors and rebuilds both come through
    /// here, so a rebuilt store is armed exactly like the first one.
    pub(crate) async fn instantiate(&self) -> PluginResult<LiveInstance> {
        let timeout_ms = self.manifest.resources.max_execution_time_ms;
        let optional = self.manifest.permissions.optional.iter().cloned().collect();
        let mut host_state = HostState::new(
            self.manifest.plugin.name.clone(),
            self.manifest.config.clone(),
            self.granted_permissions.clone(),
            optional,
            self.manifest.permissions.http_allowlist.clone(),
            timeout_ms,
            self.manifest.resources.max_kv_storage_mb,
        )
        .with_kv_store(Arc::clone(&self.kv_store));
        if let Some(memory) = &self.memory {
            host_state = host_state.with_memory(Arc::clone(memory));
        }
        if let Some(bus) = &self.event_bus {
            host_state = host_state.with_event_bus(Arc::clone(bus));
        }
        if let Some(sink) = &self.event_sink {
            host_state = host_state.with_event_sink(sink.clone());
        }

        let mut store = wasmtime::Store::new(&self.engine, host_state);
        apply_store_resource_limits(&mut store, self.manifest.resources.max_memory_mb);
        store
            .set_fuel(self.manifest.resources.max_fuel)
            .map_err(|e| PluginError::Instantiation(format!("failed to set fuel: {e}")))?;
        apply_store_epoch_deadline(&mut store, timeout_ms);

        let mut linker = wasmtime::component::Linker::<HostState>::new(&self.engine);
        (self.link)(&mut linker)?;

        let instance = linker
            .instantiate_async(&mut store, &self.component)
            .await
            .map_err(|e| {
                PluginError::Instantiation(format!("failed to instantiate {}: {e}", self.kind.instantiation_noun()))
            })?;

        Ok(LiveInstance { store, instance })
    }
}

/// Health of the one store an adapter owns.
pub(crate) enum AdapterState {
    /// The store is enterable.
    Live(Box<LiveInstance>),
    /// Something unwound out of wasm: the store is poisoned and the next call
    /// rebuilds the adapter — charged to the restart budget unless the
    /// poisoning was a caller cancellation.
    Dead { reason: String, cause: DeathCause },
    /// The restart budget is spent. No further call enters the guest until the
    /// next plugin generation replaces this adapter.
    Quarantined { reason: String },
}

/// Why an adapter's store was poisoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeathCause {
    /// The plugin trapped, ran out of fuel or memory, or overran its timeout,
    /// or a rebuild failed to instantiate. The next rebuild is charged to the
    /// restart budget.
    Fault,
    /// The caller dropped its own future while the guest was running. The
    /// store is poisoned just the same, but the fault is the caller's, so the
    /// rebuild is not charged: aborted turns must not quarantine a healthy
    /// plugin. It is counted as `cancelled_count` instead.
    Cancelled,
}

/// Refunds a charged restart attempt when the rebuild it paid for is
/// abandoned — the caller's future dropped while `instantiate` was suspended.
/// Nothing was poisoned by that drop, the half-built store is simply
/// discarded, so the charge is returned. A rebuild that completes keeps the
/// charge whether it succeeded or failed.
pub(crate) struct RestartRefund<'a> {
    attempts: &'a mut VecDeque<Instant>,
    armed: bool,
}

impl<'a> RestartRefund<'a> {
    pub(crate) const fn arm(attempts: &'a mut VecDeque<Instant>) -> Self {
        Self { attempts, armed: true }
    }

    /// The rebuild completed; keep the charge.
    pub(crate) fn commit(mut self) {
        self.armed = false;
    }
}

impl Drop for RestartRefund<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.attempts.pop_back();
        }
    }
}

/// Authoritative adapter state. Every field is read and written while holding
/// the mutex that guards the store, so a transition is atomic with respect to
/// every queued caller.
pub(crate) struct SupervisorInner {
    pub(crate) state: AdapterState,
    pub(crate) restart_count: u64,
    /// When each rebuild attempt still inside the window was made. The window
    /// is rolling, not tumbling: the oldest entries are dropped as they age
    /// out, so no pair of window boundaries can admit twice the budget. It
    /// holds at most `MAX_RESTARTS_PER_WINDOW` instants, because an attempt is
    /// only pushed when the pruned deque is still under the budget.
    pub(crate) restart_attempts: VecDeque<Instant>,
    pub(crate) failure_count: u64,
    pub(crate) logs_since_transition: u64,
    pub(crate) suppressed_logs: u64,
    /// This adapter's rolling window; see `restart_window`.
    pub(crate) restart_window: Duration,
    /// Host timeouts since the guest last returned; see
    /// `MAX_CONSECUTIVE_TIMEOUTS`.
    pub(crate) consecutive_timeouts: u64,
}

impl SupervisorInner {
    fn live(live: LiveInstance, restart_window: Duration) -> Self {
        Self {
            state: AdapterState::Live(Box::new(live)),
            restart_count: 0,
            restart_attempts: VecDeque::with_capacity(MAX_RESTARTS_PER_WINDOW),
            failure_count: 0,
            logs_since_transition: 0,
            suppressed_logs: 0,
            restart_window,
            consecutive_timeouts: 0,
        }
    }

    /// Take one rebuild attempt from the rolling budget, or report that the
    /// budget is spent.
    ///
    /// The attempt is charged before it is made, so an instantiation that keeps
    /// failing consumes the budget exactly like a trap that keeps recurring,
    /// and neither can bypass the quarantine that ends both.
    pub(crate) fn claim_restart(&mut self, now: Instant) -> bool {
        while self
            .restart_attempts
            .front()
            .is_some_and(|attempted| now.duration_since(*attempted) > self.restart_window)
        {
            self.restart_attempts.pop_front();
        }
        if self.restart_attempts.len() >= MAX_RESTARTS_PER_WINDOW {
            return false;
        }
        self.restart_attempts.push_back(now);
        self.restart_count = self.restart_count.saturating_add(1);
        true
    }
}

/// Lock-free mirror of the adapter state, written inside the store mutex and
/// read by `wasm_plugins_status` without taking it.
#[derive(Default)]
struct AdapterStatus {
    state: AtomicU8,
    restart_count: AtomicU64,
    failure_count: AtomicU64,
    skipped_oversize: AtomicU64,
    skipped_quarantined: AtomicU64,
    cancelled_count: AtomicU64,
    /// Mirror of `SupervisorInner::consecutive_timeouts`.
    consecutive_timeouts: AtomicU64,
    oversize_logged: AtomicBool,
    quarantined_logged: AtomicBool,
    last_error: RwLock<Option<String>>,
    last_error_at: RwLock<Option<String>>,
    quarantined_since: RwLock<Option<Instant>>,
    /// Which rule quarantined the adapter, once one has.
    quarantine_rule: RwLock<Option<QuarantineRule>>,
}

/// The rule that quarantined an adapter, as named in status and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuarantineRule {
    /// The rolling restart budget was spent (`MAX_RESTARTS_PER_WINDOW`).
    RestartWindow,
    /// Timeouts in a row with no guest call returning in between
    /// (`MAX_CONSECUTIVE_TIMEOUTS`).
    TimeoutStreak,
}

impl QuarantineRule {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RestartWindow => "restart_window",
            Self::TimeoutStreak => "timeout_streak",
        }
    }
}

/// Cut a reason to `MAX_LAST_ERROR_CHARS` on a character boundary.
pub(crate) fn bounded_reason(reason: &str) -> String {
    let mut bounded: String = reason.chars().take(MAX_LAST_ERROR_CHARS).collect();
    if bounded.len() < reason.len() {
        bounded.push_str(LAST_ERROR_TRUNCATION_SUFFIX);
    }
    bounded
}

impl AdapterStatus {
    fn label(&self) -> &'static str {
        match self.state.load(Ordering::Relaxed) {
            STATE_QUARANTINED => "quarantined",
            STATE_RESTARTING => "restarting",
            _ => "live",
        }
    }

    fn is_quarantined(&self) -> bool {
        self.state.load(Ordering::Relaxed) == STATE_QUARANTINED
    }

    fn record_error(&self, reason: &str) {
        *self.last_error.write() = Some(bounded_reason(reason));
        *self.last_error_at.write() = Some(chrono::Utc::now().to_rfc3339());
    }

    /// Re-open the once-per-state skip logs. Called on every state transition so
    /// each state reports its own first skipped call.
    fn reopen_skip_logs(&self) {
        self.oversize_logged.store(false, Ordering::Relaxed);
        self.quarantined_logged.store(false, Ordering::Relaxed);
    }

    fn health(&self) -> AdapterHealth {
        AdapterHealth {
            state: self.label(),
            restart_count: self.restart_count.load(Ordering::Relaxed),
            failure_count: self.failure_count.load(Ordering::Relaxed),
            cancelled_count: self.cancelled_count.load(Ordering::Relaxed),
            skipped_oversize: self.skipped_oversize.load(Ordering::Relaxed),
            skipped_quarantined: self.skipped_quarantined.load(Ordering::Relaxed),
            consecutive_timeouts: self.consecutive_timeouts.load(Ordering::Relaxed),
            quarantine_rule: self.quarantine_rule.read().map(QuarantineRule::as_str),
            last_error_at: self.last_error_at.read().clone(),
            quarantined_for_ms: self
                .quarantined_since
                .read()
                .map(|since| u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)),
        }
    }

    fn write_into(&self, map: &mut serde_json::Map<String, serde_json::Value>) {
        map.insert("state".to_string(), serde_json::json!(self.label()));
        map.insert(
            "restart_count".to_string(),
            serde_json::json!(self.restart_count.load(Ordering::Relaxed)),
        );
        map.insert(
            "failure_count".to_string(),
            serde_json::json!(self.failure_count.load(Ordering::Relaxed)),
        );
        map.insert(
            "skipped_oversize".to_string(),
            serde_json::json!(self.skipped_oversize.load(Ordering::Relaxed)),
        );
        map.insert(
            "skipped_quarantined".to_string(),
            serde_json::json!(self.skipped_quarantined.load(Ordering::Relaxed)),
        );
        map.insert(
            "cancelled_count".to_string(),
            serde_json::json!(self.cancelled_count.load(Ordering::Relaxed)),
        );
        map.insert(
            "consecutive_timeouts".to_string(),
            serde_json::json!(self.consecutive_timeouts.load(Ordering::Relaxed)),
        );
        let quarantine_rule = *self.quarantine_rule.read();
        if let Some(rule) = quarantine_rule {
            map.insert("quarantine_rule".to_string(), serde_json::json!(rule.as_str()));
        }
        map.insert("last_error".to_string(), serde_json::json!(*self.last_error.read()));
        map.insert(
            "last_error_at".to_string(),
            serde_json::json!(*self.last_error_at.read()),
        );
        let quarantined_since = *self.quarantined_since.read();
        if let Some(since) = quarantined_since {
            let elapsed = u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX);
            map.insert("quarantined_for_ms".to_string(), serde_json::json!(elapsed));
        }
    }
}

/// Recovery health of one adapter, as published in `wasm_plugins_status`.
/// `last_error` is published beside it by each adapter.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdapterHealth {
    /// `live`, `restarting` or `quarantined`.
    pub state: &'static str,
    /// Rebuild attempts, successful or not, charged or not.
    pub restart_count: u64,
    /// Calls that failed inside the guest, including poisoning outcomes.
    pub failure_count: u64,
    /// Calls whose caller was dropped while the guest was running.
    pub cancelled_count: u64,
    /// Calls refused before the guest because the payload exceeded the cap.
    pub skipped_oversize: u64,
    /// Calls refused because the adapter is quarantined.
    pub skipped_quarantined: u64,
    /// Timeouts since the guest last returned; the fourth quarantines.
    pub consecutive_timeouts: u64,
    /// `restart_window` or `timeout_streak`: the rule that quarantined the
    /// adapter, once one has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantine_rule: Option<&'static str>,
    /// When `last_error` was recorded (RFC 3339).
    pub last_error_at: Option<String>,
    /// How long the adapter has been quarantined, while it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantined_for_ms: Option<u64>,
}

/// What one attempted guest call left behind.
pub(crate) enum CallOutcome<T> {
    /// The guest call completed and produced a value.
    Completed(T),
    /// Something unwound out of wasm, so the store is now poisoned.
    Poisoning(PluginError),
    /// The call failed without unwinding out of wasm; the store stays usable.
    GuestError(PluginError),
    /// No guest call was attempted.
    Refused(PluginError),
}

/// Classify an error that unwound out of a guest export call.
///
/// Every such error poisons the store. An epoch-deadline interrupt is the
/// guest running past `max_execution_time_ms` while it computes in wasm: the
/// epoch deadline is armed from the same timeout as the host timer in
/// `GuestCall::run`, which can only fire while the guest waits in an async
/// host call, because a guest spinning in wasm never yields to it. Both are
/// the same hang, so the interrupt is reported as the same
/// [`PluginError::Timeout`] and feeds the same timeout streak
/// (`MAX_CONSECUTIVE_TIMEOUTS`). Anything else — `unreachable`, exhausted
/// fuel, a failed `memory.grow` — is a trap, described by `describe`.
pub(crate) fn unwound<T>(
    store: &wasmtime::Store<HostState>,
    error: &wasmtime::Error,
    describe: impl FnOnce() -> String,
) -> CallOutcome<T> {
    if is_epoch_interrupt(error) {
        CallOutcome::Poisoning(PluginError::Timeout(store.data().timeout_ms))
    } else {
        CallOutcome::Poisoning(PluginError::Runtime(describe()))
    }
}

/// Whether a guest call was stopped by its store's epoch deadline.
fn is_epoch_interrupt(error: &wasmtime::Error) -> bool {
    error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt)
}

/// One call into one guest export.
///
/// The supervisor arms the epoch deadline and the fuel budget and bounds the
/// call with the host timeout; `invoke` only looks the export up, calls it and
/// classifies the result. A `call_async` error must be classified by
/// [`unwound`]: it unwound out of wasm.
///
/// The call future is boxed: an unboxed generic future borrowing the store
/// defeats the `Send` inference of every async-trait caller up the stack. One
/// allocation per guest call is noise next to entering wasm.
pub(crate) trait GuestOp: Send {
    type Output: Send;

    /// Label for logs, e.g. the middleware stage or the storage operation.
    fn operation(&self) -> &'static str;

    fn invoke<'a>(
        self,
        store: &'a mut wasmtime::Store<HostState>,
        instance: &'a wasmtime::component::Instance,
    ) -> futures_util::future::BoxFuture<'a, CallOutcome<Self::Output>>
    where
        Self: 'a;
}

/// Cancellation marker for one guest call.
///
/// It is armed before the call and disarmed only once the call has returned. If
/// the caller's own future is dropped while the call is still pending — an
/// aborted turn task, a client that disconnected from a gateway handler, a
/// cancelled tool call — the `call_async` future is dropped too, and dropping
/// it resumes the guest fiber with an error that unwinds out of wasm and
/// poisons the whole store. Nothing resumes the supervisor in that case, so
/// without this marker the mutex would be released with the state still `Live`
/// over a store that no later caller can enter.
///
/// `Drop` runs while the store mutex is still held, because the call future is
/// created after the guard inside [`Supervisor::call`] and is therefore dropped
/// before it. The refusal arms of `run` contain no `await` point, so the
/// marker can only fire from inside the guest call itself.
pub(crate) struct GuestCall<'a> {
    supervisor: &'a Supervisor,
    inner: &'a mut SupervisorInner,
    operation: &'static str,
    armed: bool,
}

impl<'a> GuestCall<'a> {
    pub(crate) const fn arm(
        supervisor: &'a Supervisor,
        inner: &'a mut SupervisorInner,
        operation: &'static str,
    ) -> Self {
        Self {
            supervisor,
            inner,
            operation,
            armed: true,
        }
    }

    /// Enter the guest, or refuse without entering it.
    async fn run<O: GuestOp>(&mut self, op: O) -> CallOutcome<O::Output> {
        let supervisor = self.supervisor;
        match &mut self.inner.state {
            AdapterState::Live(live) => {
                let LiveInstance { store, instance } = live.as_mut();
                apply_store_epoch_deadline(store, supervisor.timeout_ms);
                // Fuel is a store-level budget. Arming it only at construction
                // made `max_fuel` a process-lifetime allowance that every call
                // drained, so a long-lived adapter was guaranteed to trap
                // eventually and to stay poisoned. Re-arm it beside the epoch
                // deadline so `max_fuel` means "per invocation", which is what
                // every manifest already reads as.
                if let Err(error) = store.set_fuel(supervisor.max_fuel) {
                    return CallOutcome::GuestError(PluginError::Runtime(format!(
                        "{} '{}' could not meter fuel: {error}",
                        supervisor.kind.as_str(),
                        supervisor.plugin_name
                    )));
                }
                // Dropping the call future on timeout resumes the guest fiber
                // with an error, which unwinds out of wasm and poisons the
                // store exactly like a trap.
                tokio::time::timeout(Duration::from_millis(supervisor.timeout_ms), op.invoke(store, instance))
                    .await
                    .unwrap_or_else(|_| CallOutcome::Poisoning(PluginError::Timeout(supervisor.timeout_ms)))
            }
            // Reachable when the budget paid for a rebuild that then failed to
            // instantiate: the adapter stays dead and this call is refused.
            AdapterState::Dead { reason, .. } => CallOutcome::Refused(PluginError::Runtime(format!(
                "{} '{}' is awaiting a rebuild: {reason}",
                supervisor.kind.as_str(),
                supervisor.plugin_name
            ))),
            AdapterState::Quarantined { reason } => {
                supervisor.record_quarantined_skip(self.operation);
                CallOutcome::Refused(PluginError::Quarantined {
                    plugin: supervisor.plugin_name.clone(),
                    reason: reason.clone(),
                })
            }
        }
    }

    /// The call returned, so its outcome — and not cancellation — decides the
    /// next state.
    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GuestCall<'_> {
    fn drop(&mut self) {
        // Only a live store can have been poisoned by the drop. The refusal
        // arms of `run` never await, but if one ever did, a cancellation there
        // must not move a quarantined adapter back to a rebuildable state.
        if !self.armed || !matches!(self.inner.state, AdapterState::Live(_)) {
            return;
        }
        let reason = format!(
            "{} '{}' call cancelled while the guest was running",
            self.supervisor.kind.as_str(),
            self.supervisor.plugin_name
        );
        self.supervisor.status.cancelled_count.fetch_add(1, Ordering::Relaxed);
        self.supervisor
            .mark_dead(self.inner, self.operation, reason, DeathCause::Cancelled);
    }
}

/// Owner of one adapter's store: admission, rebuild, quarantine and status.
pub(crate) struct Supervisor {
    plugin_name: String,
    kind: CapabilityKind,
    inner: Mutex<SupervisorInner>,
    timeout_ms: u64,
    max_fuel: u64,
    max_payload_bytes: usize,
    /// Bound on waiting for the store mutex, for the kinds whose callers were
    /// promised one: an uncharged timeout, not a plugin fault.
    lock_wait: Option<Duration>,
    /// Whether every rebuild is published on `prx.plugin.storage_restarted`.
    announce_restarts: bool,
    respawn: Respawn,
    status: AdapterStatus,
}

impl Supervisor {
    /// Take ownership of an adapter's first live instance.
    pub(crate) fn new(respawn: Respawn, live: LiveInstance) -> Self {
        let resources = &respawn.manifest.resources;
        Self {
            plugin_name: respawn.manifest.plugin.name.clone(),
            kind: respawn.kind,
            inner: Mutex::new(SupervisorInner::live(
                live,
                restart_window(resources.max_execution_time_ms),
            )),
            timeout_ms: resources.max_execution_time_ms,
            max_fuel: resources.max_fuel,
            max_payload_bytes: resources.payload_cap_bytes(),
            lock_wait: None,
            announce_restarts: false,
            respawn,
            status: AdapterStatus::default(),
        }
    }

    /// Bound the wait for the store mutex by the manifest's execution timeout.
    ///
    /// Tool, provider and storage callers used to get a single timeout that
    /// covered both waiting for the store and the guest call. The guest call
    /// is now timed inside the critical section, where a timeout is recorded
    /// as the plugin's fault; waiting behind other callers is bounded here
    /// separately and fails with `PluginError::Timeout` without touching the
    /// adapter's state, because a slow queue is not a plugin fault.
    pub(crate) const fn with_bounded_lock_wait(mut self) -> Self {
        self.lock_wait = Some(Duration::from_millis(self.timeout_ms));
        self
    }

    /// Publish every successful rebuild on `prx.plugin.storage_restarted`.
    pub(crate) const fn with_restart_announcements(mut self) -> Self {
        self.announce_restarts = true;
        self
    }

    pub(crate) fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    pub(crate) const fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    /// The adapter's host KV namespace, shared by every store it builds.
    #[cfg(test)]
    pub(crate) fn kv_namespace(&self) -> KvNamespace {
        Arc::clone(&self.respawn.kv_store)
    }

    /// Record a plugin fault as if a guest call had poisoned the store, for
    /// contract tests of adapters whose committed fixture cannot be made to
    /// trap once its fuel is metered per call.
    #[cfg(test)]
    pub(crate) async fn inject_fault(&self, reason: &str) {
        let mut guard = self.inner.lock().await;
        self.mark_dead(&mut guard, "test", reason.to_string(), DeathCause::Fault);
    }

    /// Acquire the store mutex. Exposed for tests that drive the cancellation
    /// marker directly.
    #[cfg(test)]
    pub(crate) async fn lock(&self) -> tokio::sync::MutexGuard<'_, SupervisorInner> {
        self.inner.lock().await
    }

    /// Call one guest export through the state machine, in line. Production
    /// code goes through [`Supervisor::call_detached`]; tests drive the state
    /// machine directly.
    #[cfg(test)]
    pub(crate) async fn call<O: GuestOp>(&self, op: O) -> PluginResult<O::Output> {
        self.call_noting_rebuild(op).await.0
    }

    /// Call one guest export on a task of its own and wait for it.
    ///
    /// A caller that goes away — a user stopping a turn, a caller-side
    /// deadline shorter than the plugin's, another plugin's host call timed
    /// out while it waited on this one — drops only its wait. The guest call
    /// is never dropped mid-run by a caller, so a caller can neither poison
    /// the store nor be charged for it: the call runs to its own outcome, and
    /// a guest that hangs is charged by its own host timeout. The task takes
    /// the same store lock through the same state machine, so the
    /// single-critical-section design is unchanged. The cost is one task and
    /// one owned copy of the call's arguments per call. The task carries the
    /// caller's `tracing` span but not its task-locals.
    pub(crate) async fn call_detached<O>(self: &Arc<Self>, op: O) -> PluginResult<O::Output>
    where
        O: GuestOp + 'static,
        O::Output: 'static,
    {
        self.call_detached_noting_rebuild(op).await.0
    }

    /// [`Supervisor::call_detached`], also reporting whether the call rebuilt
    /// the store before entering the guest.
    pub(crate) async fn call_detached_noting_rebuild<O>(self: &Arc<Self>, op: O) -> (PluginResult<O::Output>, bool)
    where
        O: GuestOp + 'static,
        O::Output: 'static,
    {
        let supervisor = Arc::clone(self);
        let operation = op.operation();
        // The caller's span follows the call onto its task, so the guest's
        // log lines and the adapter's transitions stay attributed to the turn
        // or delivery that made the call. Task-locals do not follow: a guest
        // call must not depend on any.
        let call = tracing::Instrument::instrument(
            async move { supervisor.call_noting_rebuild(op).await },
            tracing::Span::current(),
        );
        match tokio::spawn(call).await {
            Ok(outcome) => outcome,
            // A task only ends this way if it panicked or the runtime is
            // shutting down; either way nothing was produced.
            Err(join_error) => (
                Err(PluginError::Runtime(format!(
                    "{} '{}' {operation} did not complete: {join_error}",
                    self.kind.as_str(),
                    self.plugin_name
                ))),
                false,
            ),
        }
    }

    /// Like [`Supervisor::call`], and also report whether this call rebuilt
    /// the store before entering the guest, for adapters whose callers must
    /// learn that guest state was reset.
    pub(crate) async fn call_noting_rebuild<O: GuestOp>(&self, op: O) -> (PluginResult<O::Output>, bool) {
        let mut announcements = Announcements::default();
        let result = {
            let mut guard = match self.lock_wait {
                Some(wait) => match tokio::time::timeout(wait, self.inner.lock()).await {
                    Ok(guard) => guard,
                    Err(_) => return (Err(PluginError::Timeout(self.timeout_ms)), false),
                },
                None => self.inner.lock().await,
            };
            self.call_guarded(&mut guard, op, &mut announcements).await
        };
        // Publishing outside the critical section keeps an external subscriber
        // from holding the store lock; only the caller that performed the
        // transition carries it, so each event is emitted exactly once.
        if let Some((reason, restart_count)) = announcements.restarted {
            self.publish_restart(&reason, restart_count).await;
        }
        if let Some(reason) = announcements.quarantined {
            self.publish_quarantine(&reason).await;
        }
        (result, announcements.rebuilt)
    }

    /// One critical section: liveness check, rebuild, call, transition.
    async fn call_guarded<O: GuestOp>(
        &self,
        inner: &mut SupervisorInner,
        op: O,
        announcements: &mut Announcements,
    ) -> PluginResult<O::Output> {
        let operation = op.operation();
        self.ensure_live(inner, operation, announcements).await;

        let outcome = {
            let mut call = GuestCall::arm(self, inner, operation);
            let outcome = call.run(op).await;
            call.disarm();
            outcome
        };

        match outcome {
            CallOutcome::Completed(value) => {
                self.set_consecutive_timeouts(inner, 0);
                Ok(value)
            }
            CallOutcome::Poisoning(error) => {
                let timed_out = matches!(error, PluginError::Timeout(_));
                if timed_out {
                    self.set_consecutive_timeouts(inner, inner.consecutive_timeouts.saturating_add(1));
                }
                let reason = error.to_string();
                // Decided before the store is marked dead: a call that
                // quarantines is followed by no rebuild, so the dead-store
                // line ("the next call rebuilds it") would be false for it.
                if timed_out && inner.consecutive_timeouts >= MAX_CONSECUTIVE_TIMEOUTS {
                    self.count_failure(inner);
                    tracing::error!(
                        plugin = %self.plugin_name,
                        capability = self.kind.as_str(),
                        operation,
                        consecutive_timeouts = inner.consecutive_timeouts,
                        "WASM {} timed out on every call since it last returned",
                        self.kind.as_str()
                    );
                    announcements.quarantined =
                        Some(self.quarantine(inner, operation, reason, QuarantineRule::TimeoutStreak));
                } else {
                    self.mark_dead(inner, operation, reason, DeathCause::Fault);
                }
                Err(error)
            }
            CallOutcome::GuestError(error) => {
                // The guest ran and answered: it is not hanging.
                self.set_consecutive_timeouts(inner, 0);
                self.record_failure(inner, operation, &error);
                Err(error)
            }
            // A refusal is the quarantine decision replaying, not a new failure.
            CallOutcome::Refused(error) => Err(error),
        }
    }

    /// Rebuild a dead adapter on a fresh store, or quarantine it when the
    /// rolling restart budget is spent.
    async fn ensure_live(
        &self,
        inner: &mut SupervisorInner,
        operation: &'static str,
        announcements: &mut Announcements,
    ) {
        let AdapterState::Dead { reason, cause } = &inner.state else {
            return;
        };
        let dead_reason = reason.clone();
        let charged = *cause == DeathCause::Fault;
        if charged {
            // The attempt is charged to the budget before it is made, so a
            // rebuild that cannot instantiate consumes a restart exactly like
            // one that can. Otherwise a store that fails to instantiate — a
            // transient allocation failure, say — would quarantine the adapter
            // for the whole generation on its first occurrence, without ever
            // touching the budget the documented contract is written in terms of.
            if !inner.claim_restart(Instant::now()) {
                announcements.quarantined =
                    Some(self.quarantine(inner, operation, dead_reason, QuarantineRule::RestartWindow));
                return;
            }
        } else {
            // A cancellation poisoned the store but was not the plugin's fault:
            // the rebuild is counted, not charged.
            inner.restart_count = inner.restart_count.saturating_add(1);
        }
        self.status.restart_count.store(inner.restart_count, Ordering::Relaxed);

        let rebuilt = {
            let refund = charged.then(|| RestartRefund::arm(&mut inner.restart_attempts));
            let rebuilt = self.respawn.instantiate().await;
            if let Some(refund) = refund {
                refund.commit();
            }
            rebuilt
        };

        match rebuilt {
            Ok(live) => {
                inner.state = AdapterState::Live(Box::new(live));
                inner.logs_since_transition = 0;
                self.status.state.store(STATE_LIVE, Ordering::Relaxed);
                self.status.reopen_skip_logs();
                tracing::warn!(
                    plugin = %self.plugin_name,
                    capability = self.kind.as_str(),
                    operation,
                    reason = %dead_reason,
                    restart_count = inner.restart_count,
                    "WASM {} rebuilt on a fresh store after a poisoning outcome",
                    self.kind.as_str()
                );
                announcements.rebuilt = true;
                if self.announce_restarts {
                    announcements.restarted = Some((dead_reason, inner.restart_count));
                }
            }
            Err(error) => {
                // The adapter stays dead: this call is refused and the next one
                // retries, charged even after a cancellation, until the budget
                // is spent and quarantines it.
                let reason = format!("rebuild after '{dead_reason}' failed: {error}");
                inner.state = AdapterState::Dead {
                    reason: reason.clone(),
                    cause: DeathCause::Fault,
                };
                self.status.record_error(&reason);
                tracing::warn!(
                    plugin = %self.plugin_name,
                    capability = self.kind.as_str(),
                    operation,
                    reason = %reason,
                    restart_count = inner.restart_count,
                    "WASM {} rebuild failed; it stays dead until the restart budget is spent",
                    self.kind.as_str()
                );
            }
        }
    }

    /// Count a call skipped because its payload cannot fit the guest, logging
    /// once per adapter state. Returns `true` when the payload may be handed to
    /// the guest.
    pub(crate) fn admit_payload(&self, operation: &'static str, payload_bytes: usize) -> bool {
        if payload_bytes <= self.max_payload_bytes {
            return true;
        }
        self.status.skipped_oversize.fetch_add(1, Ordering::Relaxed);
        if !self.status.oversize_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                plugin = %self.plugin_name,
                capability = self.kind.as_str(),
                operation,
                payload_bytes,
                max_payload_bytes = self.max_payload_bytes,
                "WASM {} call skipped: the payload cannot fit the guest",
                self.kind.as_str()
            );
        }
        false
    }

    /// Count a call refused because the adapter is quarantined, logging once per
    /// adapter state so an operator can tell a dormant quarantine from one that
    /// is refusing every call.
    fn record_quarantined_skip(&self, operation: &'static str) {
        let skipped = self
            .status
            .skipped_quarantined
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if !self.status.quarantined_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                plugin = %self.plugin_name,
                capability = self.kind.as_str(),
                operation,
                skipped_quarantined = skipped,
                "WASM {} is quarantined; calls are refused until the next plugin generation",
                self.kind.as_str()
            );
        }
    }

    /// Set the timeout streak and its status mirror together.
    fn set_consecutive_timeouts(&self, inner: &mut SupervisorInner, count: u64) {
        inner.consecutive_timeouts = count;
        self.status.consecutive_timeouts.store(count, Ordering::Relaxed);
    }

    /// Count one failed guest call in state and status.
    fn count_failure(&self, inner: &mut SupervisorInner) {
        inner.failure_count = inner.failure_count.saturating_add(1);
        self.status.failure_count.store(inner.failure_count, Ordering::Relaxed);
    }

    /// Record a poisoning outcome and schedule a rebuild.
    fn mark_dead(&self, inner: &mut SupervisorInner, operation: &'static str, reason: String, cause: DeathCause) {
        self.count_failure(inner);
        inner.state = AdapterState::Dead {
            reason: reason.clone(),
            cause,
        };
        let suppressed = std::mem::take(&mut inner.suppressed_logs);
        inner.logs_since_transition = 0;
        self.status.state.store(STATE_RESTARTING, Ordering::Relaxed);
        self.status.reopen_skip_logs();
        self.status.record_error(&reason);
        tracing::error!(
            plugin = %self.plugin_name,
            capability = self.kind.as_str(),
            operation,
            reason = %reason,
            restart_count = inner.restart_count,
            suppressed_failure_logs = suppressed,
            "WASM {} store poisoned; the next call rebuilds it on a fresh store",
            self.kind.as_str()
        );
    }

    /// Record a failure that left the store usable, logging at most
    /// `MAX_FAILURE_LOGS_PER_STATE` times between transitions.
    fn record_failure(&self, inner: &mut SupervisorInner, operation: &'static str, error: &PluginError) {
        let reason = error.to_string();
        inner.failure_count = inner.failure_count.saturating_add(1);
        self.status.failure_count.store(inner.failure_count, Ordering::Relaxed);
        self.status.record_error(&reason);
        if inner.logs_since_transition < MAX_FAILURE_LOGS_PER_STATE {
            inner.logs_since_transition = inner.logs_since_transition.saturating_add(1);
            tracing::warn!(
                plugin = %self.plugin_name,
                capability = self.kind.as_str(),
                operation,
                reason = %reason,
                failure_count = inner.failure_count,
                "WASM {} call failed",
                self.kind.as_str()
            );
        } else {
            inner.suppressed_logs = inner.suppressed_logs.saturating_add(1);
        }
    }

    /// Stop calling the guest until the next generation swap replaces this
    /// adapter. Returns the reason to announce once the lock is released.
    fn quarantine(
        &self,
        inner: &mut SupervisorInner,
        operation: &'static str,
        reason: String,
        rule: QuarantineRule,
    ) -> String {
        let suppressed = std::mem::take(&mut inner.suppressed_logs);
        inner.logs_since_transition = 0;
        inner.state = AdapterState::Quarantined { reason: reason.clone() };
        self.status.state.store(STATE_QUARANTINED, Ordering::Relaxed);
        self.status.reopen_skip_logs();
        *self.status.quarantined_since.write() = Some(Instant::now());
        *self.status.quarantine_rule.write() = Some(rule);
        self.status.record_error(&reason);
        tracing::error!(
            plugin = %self.plugin_name,
            capability = self.kind.as_str(),
            operation,
            rule = rule.as_str(),
            reason = %reason,
            restart_count = inner.restart_count,
            suppressed_failure_logs = suppressed,
            "WASM {} quarantined; it stays refused until the next plugin generation",
            self.kind.as_str()
        );
        reason
    }

    async fn publish_restart(&self, reason: &str, restart_count: u64) {
        let Some(bus) = &self.respawn.event_bus else {
            return;
        };
        let payload = serde_json::json!({
            "plugin": self.plugin_name,
            "capability": self.kind.as_str(),
            "reason": reason,
            "restart_count": restart_count,
        })
        .to_string();
        if let Err(error) = bus.publish(STORAGE_RESTARTED_TOPIC, &payload).await {
            tracing::warn!(
                plugin = %self.plugin_name,
                capability = self.kind.as_str(),
                error = %error,
                "failed to publish the WASM adapter restart event"
            );
        }
    }

    async fn publish_quarantine(&self, reason: &str) {
        let Some(bus) = &self.respawn.event_bus else {
            return;
        };
        let payload = serde_json::json!({
            "plugin": self.plugin_name,
            "capability": self.kind.as_str(),
            "reason": reason,
        })
        .to_string();
        if let Err(error) = bus.publish(self.kind.quarantine_topic(), &payload).await {
            tracing::warn!(
                plugin = %self.plugin_name,
                capability = self.kind.as_str(),
                error = %error,
                "failed to publish the WASM adapter quarantine event"
            );
        }
    }

    /// Per-adapter health, merged into the identity fields the adapter supplies.
    pub(crate) fn diagnostics(&self, identity: serde_json::Value) -> serde_json::Value {
        let mut map = match identity {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        self.status.write_into(&mut map);
        serde_json::Value::Object(map)
    }

    pub(crate) fn is_quarantined(&self) -> bool {
        self.status.is_quarantined()
    }

    /// Typed recovery health, for adapters with a typed diagnostics record.
    pub(crate) fn health(&self) -> AdapterHealth {
        self.status.health()
    }

    /// The most recent failure, kept until a later one replaces it.
    pub(crate) fn last_error(&self) -> Option<String> {
        self.status.last_error.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed `pipeline-middleware` component with resources that let
    /// it run normally; these tests drive the state machine directly.
    const FIXTURE_MANIFEST: &str = r#"
[plugin]
name = "pipeline-middleware"
version = "0.1.0"
wasm = "plugin.wasm"

[[capabilities]]
type = "middleware"
name = "pipeline-marker"
priority = 10

[permissions]
required = []
optional = []

[resources]
max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
"#;

    async fn fixture_supervisor() -> Supervisor {
        let engine = crate::plugins::shared_wasm_engine().expect("test: shared wasmtime engine");
        let wasm =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/pipeline-middleware/plugin.wasm");
        let bytes = std::fs::read(wasm).expect("test: the committed middleware fixture is readable");
        let component =
            wasmtime::component::Component::from_binary(&engine, &bytes).expect("test: the fixture is a component");
        let manifest: PluginManifest = toml::from_str(FIXTURE_MANIFEST).expect("test: fixture manifest parses");
        let respawn = Respawn::new(
            CapabilityKind::Middleware,
            &engine,
            &component,
            &manifest,
            std::collections::HashSet::new(),
            None,
            None,
            super::super::common::register_common_host_functions,
        );
        let live = respawn.instantiate().await.expect("test: the fixture instantiates");
        Supervisor::new(respawn, live)
    }

    /// Hands back the KV namespace of whichever store the call ran against,
    /// without entering the guest.
    struct ReadKv;

    impl GuestOp for ReadKv {
        type Output = KvNamespace;

        fn operation(&self) -> &'static str {
            "read_kv"
        }

        fn invoke<'a>(
            self,
            store: &'a mut wasmtime::Store<HostState>,
            _instance: &'a wasmtime::component::Instance,
        ) -> futures_util::future::BoxFuture<'a, CallOutcome<KvNamespace>>
        where
            Self: 'a,
        {
            let kv = Arc::clone(&store.data().kv_store);
            Box::pin(async move { CallOutcome::Completed(kv) })
        }
    }

    /// Drop an armed marker while holding the store mutex, which is what a
    /// caller whose future is dropped mid-call does.
    async fn cancel_one_call(supervisor: &Supervisor) {
        let mut guard = supervisor.lock().await;
        drop(GuestCall::arm(supervisor, &mut guard, "test"));
    }

    fn counter(supervisor: &Supervisor, field: &str) -> Option<u64> {
        supervisor
            .diagnostics(serde_json::json!({}))
            .get(field)
            .and_then(serde_json::Value::as_u64)
    }

    /// Data a guest wrote through the `kv` host functions lives in host state,
    /// not in the guest's linear memory, so a rebuild on a fresh store must
    /// carry it over — as the plugin documentation has promised since the
    /// middleware recovery landed.
    #[tokio::test]
    async fn host_kv_survives_a_rebuild_on_a_fresh_store() {
        let supervisor = fixture_supervisor().await;
        let before = supervisor.call(ReadKv).await.expect("test: the live store answers");
        before.write().await.insert("durable".to_string(), b"kept".to_vec());

        cancel_one_call(&supervisor).await;
        let after = supervisor
            .call(ReadKv)
            .await
            .expect("test: the next call rebuilds on a fresh store");
        assert_eq!(
            counter(&supervisor, "restart_count"),
            Some(1),
            "a rebuild must have happened"
        );
        assert_eq!(
            after.read().await.get("durable").map(Vec::as_slice),
            Some(b"kept".as_slice()),
            "host KV must survive a rebuild"
        );
    }

    /// Reports a poisoning outcome without entering the guest, so the state
    /// machine can be driven through plugin faults deterministically.
    struct Fault;

    impl GuestOp for Fault {
        type Output = ();

        fn operation(&self) -> &'static str {
            "fault"
        }

        fn invoke<'a>(
            self,
            _store: &'a mut wasmtime::Store<HostState>,
            _instance: &'a wasmtime::component::Instance,
        ) -> futures_util::future::BoxFuture<'a, CallOutcome<()>>
        where
            Self: 'a,
        {
            Box::pin(async { CallOutcome::Poisoning(PluginError::Runtime("wasm trap: unreachable".to_string())) })
        }
    }

    /// R-C3f I1: a caller cancellation poisons the store, so the adapter must
    /// be rebuilt, but the fault is the caller's, not the plugin's. More
    /// cancellations than the whole restart budget inside one window must
    /// leave the adapter live and visible as `cancelled_count`.
    #[tokio::test]
    async fn a_caller_cancellation_is_not_charged_to_the_restart_budget() {
        let supervisor = fixture_supervisor().await;
        let rounds = MAX_RESTARTS_PER_WINDOW + 2;
        for round in 1..=rounds {
            cancel_one_call(&supervisor).await;
            supervisor
                .call(ReadKv)
                .await
                .unwrap_or_else(|error| panic!("round {round}: a cancelled call must not quarantine: {error}"));
        }
        let rounds = u64::try_from(rounds).expect("test: fits a u64");
        assert_eq!(counter(&supervisor, "cancelled_count"), Some(rounds));
        assert_eq!(counter(&supervisor, "restart_count"), Some(rounds));
        assert!(!supervisor.is_quarantined());
    }

    /// Cancellations must not launder plugin faults: after uncharged
    /// cancellations the plugin still has exactly its budget of charged
    /// rebuilds, and the fault that exceeds it quarantines the adapter.
    #[tokio::test]
    async fn plugin_faults_are_still_charged_after_cancellations() {
        let supervisor = fixture_supervisor().await;
        for _ in 0..MAX_RESTARTS_PER_WINDOW {
            cancel_one_call(&supervisor).await;
            supervisor.call(ReadKv).await.expect("test: a cancellation is rebuilt");
        }
        for fault in 1..=MAX_RESTARTS_PER_WINDOW {
            assert!(supervisor.call(Fault).await.is_err());
            supervisor
                .call(ReadKv)
                .await
                .unwrap_or_else(|error| panic!("fault {fault} is inside the budget: {error}"));
        }
        assert!(supervisor.call(Fault).await.is_err());
        let refused = supervisor.call(ReadKv).await;
        assert!(
            matches!(refused, Err(PluginError::Quarantined { .. })),
            "the fault past the budget must quarantine"
        );
        assert!(supervisor.is_quarantined());
    }

    /// R-C3f I2: a rebuild abandoned while `instantiate` is suspended returns
    /// its charge. Not constructible end to end — no committed component
    /// suspends during instantiation — so the refund guard is driven directly.
    #[test]
    fn an_abandoned_rebuild_refunds_its_charge() {
        let supervisor_inner = || SupervisorInner {
            state: AdapterState::Dead {
                reason: "wasm trap: unreachable".to_string(),
                cause: DeathCause::Fault,
            },
            restart_count: 0,
            restart_attempts: VecDeque::with_capacity(MAX_RESTARTS_PER_WINDOW),
            failure_count: 0,
            logs_since_transition: 0,
            suppressed_logs: 0,
            restart_window: RESTART_WINDOW,
            consecutive_timeouts: 0,
        };
        let now = Instant::now();

        let mut abandoned = supervisor_inner();
        assert!(abandoned.claim_restart(now));
        drop(RestartRefund::arm(&mut abandoned.restart_attempts));
        assert!(
            abandoned.restart_attempts.is_empty(),
            "the abandoned attempt is refunded"
        );

        let mut completed = supervisor_inner();
        assert!(completed.claim_restart(now));
        RestartRefund::arm(&mut completed.restart_attempts).commit();
        assert_eq!(
            completed.restart_attempts.len(),
            1,
            "a completed attempt keeps its charge"
        );
    }

    /// The injected clock. Moves every charged restart attempt `elapsed` into
    /// the past, which is what a call that held the store for `elapsed`
    /// before it was marked dead does to the spacing of the next claim. An
    /// attempt that cannot be moved that far back on a freshly booted host is
    /// dropped instead, which `claim_restart` would do anyway once it is older
    /// than any window in use.
    async fn advance_clock(supervisor: &Supervisor, elapsed: Duration) {
        let mut guard = supervisor.lock().await;
        let mut aged = VecDeque::with_capacity(guard.restart_attempts.len());
        for attempted in guard.restart_attempts.drain(..) {
            if let Some(earlier) = attempted.checked_sub(elapsed) {
                aged.push_back(earlier);
            }
        }
        guard.restart_attempts = aged;
    }

    /// The fixture's `max_execution_time_ms`, i.e. the default timeout.
    const FIXTURE_TIMEOUT: Duration = Duration::from_secs(30);

    /// Reports the host timeout's poisoning outcome without waiting for it.
    struct TimedOut;

    impl GuestOp for TimedOut {
        type Output = ();

        fn operation(&self) -> &'static str {
            "timed_out"
        }

        fn invoke<'a>(
            self,
            _store: &'a mut wasmtime::Store<HostState>,
            _instance: &'a wasmtime::component::Instance,
        ) -> futures_util::future::BoxFuture<'a, CallOutcome<()>>
        where
            Self: 'a,
        {
            Box::pin(async { CallOutcome::Poisoning(PluginError::Timeout(30_000)) })
        }
    }

    /// D-023 invariant: a guest that hangs on every call holds the store for
    /// the whole timeout each time, so its charged rebuilds are one timeout
    /// apart. At the 30 s default it must still be quarantined, at the same
    /// call as a fast trap loop.
    #[tokio::test]
    async fn a_guest_that_times_out_on_every_call_is_quarantined_at_the_default_timeout() {
        let supervisor = fixture_supervisor().await;
        for call in 1..=MAX_RESTARTS_PER_WINDOW + 3 {
            let outcome = supervisor.call(TimedOut).await;
            if matches!(outcome, Err(PluginError::Quarantined { .. })) {
                assert_eq!(
                    call,
                    MAX_RESTARTS_PER_WINDOW + 2,
                    "quarantine must follow the same number of timeouts as a fast trap loop"
                );
                return;
            }
            advance_clock(&supervisor, FIXTURE_TIMEOUT).await;
        }
        panic!("a guest that times out on every call, one 30 s timeout apart, was never quarantined");
    }

    /// D-023 invariant (R-C3Xf N1): hang, hang, hang, one fast success,
    /// repeating. Three of every four calls cost the full timeout; the
    /// success in between must not let the guest escape quarantine.
    #[tokio::test]
    async fn an_intermittent_hang_with_occasional_successes_is_quarantined() {
        let supervisor = fixture_supervisor().await;
        for cycle in 1..=4 {
            for _ in 0..3 {
                if matches!(supervisor.call(TimedOut).await, Err(PluginError::Quarantined { .. })) {
                    return;
                }
                advance_clock(&supervisor, FIXTURE_TIMEOUT).await;
            }
            if matches!(supervisor.call(ReadKv).await, Err(PluginError::Quarantined { .. })) {
                return;
            }
            assert!(cycle < 4, "the pattern must be caught within a few cycles");
        }
        panic!("hang, hang, hang, success was never quarantined");
    }

    /// D-023 invariant (R-C3Xf N2a): a caller cancellation is not the
    /// plugin's fault. A user who stops four turns in a row while a healthy
    /// plugin is working — with no other call completing in between — must
    /// not quarantine it.
    #[tokio::test]
    async fn four_user_cancellations_of_a_healthy_plugin_do_not_quarantine_it() {
        let supervisor = fixture_supervisor().await;
        for _ in 0..4 {
            cancel_one_call(&supervisor).await;
            let mut guard = supervisor.lock().await;
            let mut announcements = Announcements::default();
            supervisor.ensure_live(&mut guard, "test", &mut announcements).await;
            assert!(
                matches!(guard.state, AdapterState::Live(_)),
                "a cancelled call is rebuilt, never quarantined"
            );
        }
        assert_eq!(counter(&supervisor, "cancelled_count"), Some(4));
        supervisor
            .call(ReadKv)
            .await
            .expect("test: the plugin still answers after four cancellations");
        assert!(!supervisor.is_quarantined());
    }

    /// D-023 invariant (R-C3Xf N2b): an hourly job whose guest traps on every
    /// run through a four-hour upstream outage must be running again once the
    /// outage ends. Faults an hour apart never share a restart window.
    #[tokio::test]
    async fn an_hourly_job_failing_through_a_four_hour_outage_recovers_after_it() {
        let supervisor = fixture_supervisor().await;
        let hour = Duration::from_hours(1);
        for run in 1..=5 {
            let outcome = supervisor.call(Fault).await;
            assert!(
                !matches!(outcome, Err(PluginError::Quarantined { .. })),
                "run {run} of the outage must be retried, not quarantined"
            );
            advance_clock(&supervisor, hour).await;
        }
        supervisor
            .call(ReadKv)
            .await
            .expect("test: the first run after the outage succeeds on a rebuilt store");
        assert!(!supervisor.is_quarantined());
    }

    /// D-024: a guest that hangs on every call, called at a human pace — an
    /// agent that sees "timed out" and calls again after an LLM round trip —
    /// leaves more than T/3 idle between calls, so its charged rebuilds never
    /// share the restart window. Four timeouts in a row must quarantine it,
    /// at the fourth.
    #[tokio::test]
    async fn a_guest_hanging_on_every_call_at_a_slow_pace_is_quarantined_on_the_fourth_timeout() {
        let supervisor = fixture_supervisor().await;
        let idle = FIXTURE_TIMEOUT / 2;
        for call in 1..=4 {
            let outcome = supervisor.call(TimedOut).await;
            assert!(
                matches!(outcome, Err(PluginError::Timeout(_))),
                "call {call} must time out, not be refused: {outcome:?}"
            );
            advance_clock(&supervisor, FIXTURE_TIMEOUT + idle).await;
        }
        assert!(
            supervisor.is_quarantined(),
            "four timeouts in a row must quarantine however slowly they come"
        );
        assert!(matches!(
            supervisor.call(TimedOut).await,
            Err(PluginError::Quarantined { .. })
        ));
    }

    /// D-024: the timeout streak counts only unbroken runs. hang, hang, hang,
    /// success at a slow pace is not quarantined by it — any completed call
    /// resets the streak — and is too slow for the restart window, which still
    /// catches the same pattern back to back
    /// (`an_intermittent_hang_with_occasional_successes_is_quarantined`).
    #[tokio::test]
    async fn an_intermittent_hang_at_a_slow_pace_is_not_quarantined_by_the_streak() {
        let supervisor = fixture_supervisor().await;
        let idle = FIXTURE_TIMEOUT / 2;
        for _ in 0..4 {
            for _ in 0..3 {
                assert!(
                    matches!(supervisor.call(TimedOut).await, Err(PluginError::Timeout(_))),
                    "a hang in a pattern broken by successes must stay a timeout"
                );
                advance_clock(&supervisor, FIXTURE_TIMEOUT + idle).await;
            }
            supervisor
                .call(ReadKv)
                .await
                .expect("test: the fourth call of each cycle completes");
            advance_clock(&supervisor, idle).await;
        }
        assert!(!supervisor.is_quarantined());
    }

    /// D-024: traps do not feed the timeout streak, so a trap outage spaced
    /// beyond the restart window — faults that are not hangs — still recovers
    /// (R-C3Xf N2b stays fixed) even with a timeout in the middle of it.
    #[tokio::test]
    async fn traps_between_timeouts_do_not_complete_a_timeout_streak() {
        let supervisor = fixture_supervisor().await;
        let hour = Duration::from_hours(1);
        for outcome in 1..=7 {
            let result = if outcome % 2 == 0 {
                supervisor.call(TimedOut).await
            } else {
                supervisor.call(Fault).await
            };
            assert!(
                !matches!(result, Err(PluginError::Quarantined { .. })),
                "outcome {outcome}: three timeouts among traps are not four in a row"
            );
            advance_clock(&supervisor, hour).await;
        }
        supervisor
            .call(ReadKv)
            .await
            .expect("test: the adapter is still running after the outage");
    }

    /// R-C3Xh H1: an epoch-deadline interrupt is the in-wasm form of a
    /// timeout and is classified as one, carrying the manifest's timeout;
    /// every other trap stays a trap. Wasmtime attaches context (a backtrace)
    /// to a trap, so the classification must see through it.
    #[tokio::test]
    async fn an_epoch_interrupt_is_classified_as_a_timeout_and_other_traps_are_not() {
        let supervisor = fixture_supervisor().await;
        let guard = supervisor.lock().await;
        let AdapterState::Live(live) = &guard.state else {
            panic!("test: the fixture starts live");
        };
        let interrupt = wasmtime::Error::new(wasmtime::Trap::Interrupt).context("wasm backtrace:\n  0: guest");
        let unreachable = wasmtime::Error::new(wasmtime::Trap::UnreachableCodeReached);
        let out_of_fuel = wasmtime::Error::new(wasmtime::Trap::OutOfFuel);

        let timeout_ms = u64::try_from(FIXTURE_TIMEOUT.as_millis()).unwrap_or(u64::MAX);
        assert!(matches!(
            unwound::<()>(&live.store, &interrupt, || "interrupted".to_string()),
            CallOutcome::Poisoning(PluginError::Timeout(ms)) if ms == timeout_ms
        ));
        for (trap, label) in [(&unreachable, "unreachable"), (&out_of_fuel, "out of fuel")] {
            assert!(
                matches!(
                    unwound::<()>(&live.store, trap, || label.to_string()),
                    CallOutcome::Poisoning(PluginError::Runtime(ref reason)) if reason == label
                ),
                "{label} must stay a trap"
            );
        }
    }

    /// A guest call that answers with an error: the guest ran and returned.
    struct GuestErr;

    impl GuestOp for GuestErr {
        type Output = ();

        fn operation(&self) -> &'static str {
            "guest_err"
        }

        fn invoke<'a>(
            self,
            _store: &'a mut wasmtime::Store<HostState>,
            _instance: &'a wasmtime::component::Instance,
        ) -> futures_util::future::BoxFuture<'a, CallOutcome<()>>
        where
            Self: 'a,
        {
            Box::pin(async { CallOutcome::GuestError(PluginError::Runtime("guest said no".to_string())) })
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Step {
        /// A call the host timer stops.
        Timeout,
        /// A call that traps.
        Trap,
        /// A call the guest answers with an error.
        GuestErr,
        /// A caller cancellation while the guest runs.
        Cancel,
    }

    /// R-C3Xh H4: the three streak rules D-024 documents but no test pinned.
    /// A trap neither counts nor resets; a guest-returned error resets; a
    /// caller cancellation neither counts nor resets. Every step is spaced
    /// beyond the restart window's reach, so only the streak can quarantine.
    ///
    /// MUTATION GUARD: drop the reset in the `GuestError` arm and the second
    /// row goes red; reset the streak on a trap and the first row does.
    #[tokio::test]
    async fn the_timeout_streak_survives_traps_and_cancellations_and_ends_at_a_guest_error() {
        use Step::{Cancel, GuestErr as Answered, Timeout, Trap};
        let idle = FIXTURE_TIMEOUT / 2;
        let cases: [(&[Step], bool); 3] = [
            (&[Timeout, Trap, Timeout, Timeout, Timeout], true),
            (&[Timeout, Timeout, Timeout, Answered, Timeout], false),
            (&[Timeout, Timeout, Timeout, Cancel, Timeout], true),
        ];
        for (steps, quarantined) in cases {
            let supervisor = fixture_supervisor().await;
            for step in steps {
                match step {
                    Timeout => drop(supervisor.call(TimedOut).await),
                    Trap => drop(supervisor.call(Fault).await),
                    Answered => drop(supervisor.call(GuestErr).await),
                    Cancel => cancel_one_call(&supervisor).await,
                }
                advance_clock(&supervisor, FIXTURE_TIMEOUT + idle).await;
            }
            assert_eq!(
                supervisor.is_quarantined(),
                quarantined,
                "{steps:?}: quarantined must be {quarantined}"
            );
        }
    }

    /// Collects formatted `tracing` output for the one call a test watches.
    #[derive(Clone, Default)]
    struct Logs(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// R-C3Xh H5: the call that quarantines on the timeout streak does not
    /// also claim that the next call rebuilds the store, and status names the
    /// rule that fired and the streak behind it; the restart window is named
    /// as such when it is the one that fires.
    #[tokio::test]
    async fn status_names_the_quarantine_rule_and_the_quarantining_timeout_logs_no_rebuild() {
        let supervisor = fixture_supervisor().await;
        let idle = FIXTURE_TIMEOUT / 2;
        for _ in 0..3 {
            drop(supervisor.call(TimedOut).await);
            advance_clock(&supervisor, FIXTURE_TIMEOUT + idle).await;
        }
        let status = supervisor.diagnostics(serde_json::json!({}));
        assert_eq!(
            status.get("consecutive_timeouts"),
            Some(&serde_json::json!(3)),
            "{status}"
        );
        assert!(status.get("quarantine_rule").is_none(), "{status}");

        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish();
        let fourth = {
            let _guard = tracing::subscriber::set_default(subscriber);
            supervisor.call(TimedOut).await
        };
        let text = String::from_utf8_lossy(&logs.0.lock()).into_owned();
        assert!(matches!(fourth, Err(PluginError::Timeout(_))), "{fourth:?}");
        assert!(text.contains("quarantined"), "the quarantine is logged: {text}");
        assert!(
            !text.contains("the next call rebuilds it"),
            "no rebuild follows a quarantining call: {text}"
        );
        let status = supervisor.diagnostics(serde_json::json!({}));
        assert_eq!(
            status.get("quarantine_rule"),
            Some(&serde_json::json!("timeout_streak")),
            "{status}"
        );
        assert_eq!(
            status.get("consecutive_timeouts"),
            Some(&serde_json::json!(4)),
            "{status}"
        );

        let trapping = fixture_supervisor().await;
        for _ in 0..=MAX_RESTARTS_PER_WINDOW + 1 {
            drop(trapping.call(Fault).await);
        }
        assert!(trapping.is_quarantined());
        let status = trapping.diagnostics(serde_json::json!({}));
        assert_eq!(
            status.get("quarantine_rule"),
            Some(&serde_json::json!("restart_window")),
            "{status}"
        );
        assert_eq!(
            status.get("consecutive_timeouts"),
            Some(&serde_json::json!(0)),
            "{status}"
        );
    }
}
