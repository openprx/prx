//! Middleware capability — bridges WASM middleware plugins to the message pipeline.
//!
//! Middleware plugins can intercept and transform data at four pipeline stages:
//! - `inbound`: after receiving a ChannelMessage
//! - `outbound`: before sending a response
//! - `llm_request`: before sending to the LLM (can modify messages/tools)
//! - `llm_response`: after receiving from the LLM (can post-process)
//!
//! # Trap recovery
//!
//! The adapter's store is owned by a [`Supervisor`]: a trapped adapter is
//! rebuilt on a brand-new `Store` by the next call, rebuilds are budgeted inside
//! a rolling window, and once the budget is spent the adapter is quarantined
//! until the next plugin generation replaces it. See `super::recovery` for the
//! state machine and its invariants. Every middleware stage fails open: a
//! failing, rebuilding or quarantined adapter leaves the data unchanged.

use std::sync::Arc;

use wasmtime::AsContextMut;

#[cfg(test)]
use super::recovery::{
    AdapterState, DeathCause, LAST_ERROR_TRUNCATION_SUFFIX, MAX_LAST_ERROR_CHARS, MAX_RESTARTS_PER_WINDOW,
    RESTART_WINDOW, SupervisorInner, bounded_reason,
};
use super::recovery::{CallOutcome, CapabilityKind, GuestOp, Respawn, Supervisor};
#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::time::{Duration, Instant};

use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::HostState;
use crate::plugins::manifest::PluginManifest;

/// The shared state machine's inner state, under the name the middleware
/// tests were written against.
#[cfg(test)]
type WasmMiddlewareInner = SupervisorInner;

/// The shared adapter state, under the name the middleware tests were written
/// against.
#[cfg(test)]
type MiddlewareState = AdapterState;

/// Pipeline stages where middleware can intercept data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiddlewareStage {
    Inbound,
    Outbound,
    LlmRequest,
    LlmResponse,
}

impl MiddlewareStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
            Self::LlmRequest => "llm_request",
            Self::LlmResponse => "llm_response",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "inbound" => Some(Self::Inbound),
            "outbound" => Some(Self::Outbound),
            "llm_request" => Some(Self::LlmRequest),
            "llm_response" => Some(Self::LlmResponse),
            _ => None,
        }
    }
}

/// Cancellation marker for one middleware call, armed against the adapter's
/// supervisor. Production calls arm the shared marker inside the supervisor;
/// this wrapper lets tests arm it on a held guard.
#[cfg(test)]
struct GuestCall<'a> {
    _marker: super::recovery::GuestCall<'a>,
}

#[cfg(test)]
impl<'a> GuestCall<'a> {
    fn arm(adapter: &'a WasmMiddleware, inner: &'a mut WasmMiddlewareInner, stage: MiddlewareStage) -> Self {
        Self {
            _marker: super::recovery::GuestCall::arm(&adapter.inner, inner, stage.as_str()),
        }
    }
}

/// One call of the guest's `process` export.
struct ProcessCall {
    plugin_name: String,
    stage: MiddlewareStage,
    data_json: String,
}

impl GuestOp for ProcessCall {
    type Output = String;

    fn operation(&self) -> &'static str {
        self.stage.as_str()
    }

    fn invoke<'a>(
        self,
        store: &'a mut wasmtime::Store<HostState>,
        instance: &'a wasmtime::component::Instance,
    ) -> futures_util::future::BoxFuture<'a, CallOutcome<String>>
    where
        Self: 'a,
    {
        Box::pin(self.run(store, instance))
    }
}

impl ProcessCall {
    async fn run(
        self,
        store: &mut wasmtime::Store<HostState>,
        instance: &wasmtime::component::Instance,
    ) -> CallOutcome<String> {
        let Self {
            plugin_name,
            stage,
            data_json,
        } = self;
        // Navigate to the exported interface: prx:plugin/middleware-exports@0.1.0
        let Some(iface_idx) =
            instance.get_export_index(store.as_context_mut(), None, "prx:plugin/middleware-exports@0.1.0")
        else {
            return CallOutcome::GuestError(PluginError::Runtime(
                "plugin does not export prx:plugin/middleware-exports@0.1.0".to_string(),
            ));
        };

        let Some(func_idx) = instance.get_export_index(store.as_context_mut(), Some(&iface_idx), "process") else {
            return CallOutcome::GuestError(PluginError::Runtime(
                "process not found in middleware-exports".to_string(),
            ));
        };

        let Some(func) = instance.get_func(store.as_context_mut(), &func_idx) else {
            return CallOutcome::GuestError(PluginError::Runtime("process is not a function".to_string()));
        };

        // Call with (stage, data_json) -> result<string, string>
        let params = [
            wasmtime::component::Val::String(stage.as_str().into()),
            wasmtime::component::Val::String(data_json.as_str().into()),
        ];
        let mut results = [wasmtime::component::Val::Bool(false)];

        match func.call_async(&mut *store, &params, &mut results).await {
            Err(e) => super::recovery::unwound(store, &e, || format!("middleware '{plugin_name}' process error: {e}")),
            Ok(()) => match results.first() {
                Some(wasmtime::component::Val::Result(r)) => match r.as_ref() {
                    Ok(Some(b)) => match b.as_ref() {
                        wasmtime::component::Val::String(s) => CallOutcome::Completed(s.to_string()),
                        _ => CallOutcome::Completed(data_json.to_string()),
                    },
                    Err(Some(b)) => match b.as_ref() {
                        wasmtime::component::Val::String(e) => CallOutcome::GuestError(PluginError::Runtime(format!(
                            "middleware '{plugin_name}' returned error: {e}"
                        ))),
                        _ => CallOutcome::Completed(data_json.to_string()),
                    },
                    _ => CallOutcome::Completed(data_json.to_string()),
                },
                _ => CallOutcome::Completed(data_json.to_string()),
            },
        }
    }
}

/// A loaded middleware plugin instance.
pub struct WasmMiddleware {
    plugin_name: String,
    priority: i32,
    inner: Arc<Supervisor>,
}

impl WasmMiddleware {
    /// Create a new middleware adapter from a compiled WASM component.
    /// `granted_permissions` must come from `LoadedPlugin.granted_permissions`
    /// (policy-filtered), NOT directly from the manifest.
    pub async fn new(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        granted_permissions: std::collections::HashSet<String>,
        priority: i32,
        memory: Option<Arc<dyn crate::memory::traits::Memory>>,
        event_bus: Option<std::sync::Arc<crate::plugins::event_bus::EventBus>>,
    ) -> PluginResult<Self> {
        let respawn = Respawn::new(
            CapabilityKind::Middleware,
            engine,
            component,
            manifest,
            granted_permissions,
            memory,
            event_bus,
            Self::register_host_functions,
        );
        let live = respawn.instantiate().await?;

        Ok(Self {
            plugin_name: manifest.plugin.name.clone(),
            priority,
            inner: Arc::new(Supervisor::new(respawn, live)),
        })
    }

    /// Process data at a specific pipeline stage.
    pub async fn process(&self, stage: MiddlewareStage, data_json: &str) -> PluginResult<String> {
        // Host-side guard, taken before the lock so an oversized call never even
        // queues behind the store mutex. A payload the guest cannot hold makes
        // its `memory.grow` fail, and `trap_on_grow_failure` turns that into a
        // trap that poisons the store; because the next envelope is at least as
        // large, retrying would spend one rebuild per stage invocation until the
        // adapter quarantines. Skipping the call keeps the adapter live and
        // passes the data through unchanged, like every other fail-open outcome.
        if !self.inner.admit_payload(stage.as_str(), data_json.len()) {
            return Ok(data_json.to_string());
        }

        // Detached: an aborted turn cannot drop the stage mid-guest-call.
        self.inner
            .call_detached(ProcessCall {
                plugin_name: self.plugin_name.clone(),
                stage,
                data_json: data_json.to_string(),
            })
            .await
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    pub const fn priority(&self) -> i32 {
        self.priority
    }

    /// Per-adapter health for `wasm_plugins_status`.
    pub fn diagnostics(&self) -> serde_json::Value {
        self.inner.diagnostics(serde_json::json!({
            "plugin": self.plugin_name,
            "priority": self.priority,
        }))
    }

    pub fn is_quarantined(&self) -> bool {
        self.inner.is_quarantined()
    }

    /// Register host functions for middleware plugins.
    fn register_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
        super::common::register_common_host_functions(linker)
    }
}

/// Middleware chain that manages multiple middleware plugins sorted by priority.
pub struct MiddlewareChain {
    middlewares: Vec<WasmMiddleware>,
}

impl MiddlewareChain {
    /// Create a new empty middleware chain.
    pub const fn new() -> Self {
        Self {
            middlewares: Vec::new(),
        }
    }

    /// Add a middleware, maintaining priority order.
    pub fn add(&mut self, mw: WasmMiddleware) {
        self.middlewares.push(mw);
        self.middlewares.sort_by_key(|m| m.priority());
    }

    /// Process data through all middlewares for a given stage.
    ///
    /// Every stage fails open: a plugin that errors, is being rebuilt, or is
    /// quarantined leaves the data unchanged. The adapter owns the reporting —
    /// it logs state transitions and a bounded number of failures, and
    /// publishes `failure_count`/`last_error` through `adapters()` — so this
    /// loop stays silent instead of emitting one line per plugin per call.
    pub async fn process(&self, stage: MiddlewareStage, data_json: &str) -> String {
        let mut data = data_json.to_string();
        for mw in &self.middlewares {
            if let Ok(transformed) = mw.process(stage, &data).await {
                data = transformed;
            }
        }
        data
    }

    /// Returns true if the chain has no middlewares.
    pub const fn is_empty(&self) -> bool {
        self.middlewares.is_empty()
    }

    /// Number of middlewares in the chain.
    pub const fn len(&self) -> usize {
        self.middlewares.len()
    }

    /// Ordered adapter inventory used by runtime diagnostics.
    pub fn adapters(&self) -> Vec<serde_json::Value> {
        self.middlewares.iter().map(WasmMiddleware::diagnostics).collect()
    }

    /// Adapters that have spent their restart budget and are no longer called.
    pub fn quarantined_plugins(&self) -> Vec<String> {
        self.middlewares
            .iter()
            .filter(|middleware| middleware.is_quarantined())
            .map(|middleware| middleware.plugin_name().to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed `pipeline-middleware` component with resources that let it
    /// run normally: these tests drive the cancellation and budget paths
    /// directly, not through a trap.
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

    fn dead_inner(reason: &str) -> WasmMiddlewareInner {
        WasmMiddlewareInner {
            state: MiddlewareState::Dead {
                reason: reason.to_string(),
                cause: DeathCause::Fault,
            },
            restart_count: 0,
            restart_attempts: VecDeque::with_capacity(MAX_RESTARTS_PER_WINDOW),
            failure_count: 0,
            logs_since_transition: 0,
            suppressed_logs: 0,
            restart_window: RESTART_WINDOW,
            consecutive_timeouts: 0,
        }
    }

    async fn fixture_middleware() -> WasmMiddleware {
        let engine = crate::plugins::shared_wasm_engine().expect("test: shared wasmtime engine");
        let wasm =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/pipeline-middleware/plugin.wasm");
        let bytes = std::fs::read(wasm).expect("test: the committed middleware fixture is readable");
        let component =
            wasmtime::component::Component::from_binary(&engine, &bytes).expect("test: the fixture is a component");
        let manifest: PluginManifest = toml::from_str(FIXTURE_MANIFEST).expect("test: fixture manifest parses");
        WasmMiddleware::new(
            &engine,
            &component,
            &manifest,
            std::collections::HashSet::new(),
            10,
            None,
            None,
        )
        .await
        .expect("test: the fixture instantiates")
    }

    /// What this proves: when `GuestCall`'s `Drop` runs while the store mutex is
    /// still held — which is what a caller's future being dropped mid-call does
    /// — the adapter transitions to dead with a reason naming cancellation, and
    /// the next caller rebuilds on a fresh store instead of entering a store
    /// wasmtime has already poisoned.
    ///
    /// What it does not prove: that a real dropped caller reaches that `Drop`.
    /// No committed middleware guest can be suspended mid-call —
    /// `pipeline-middleware` calls no host function, so `call_async` runs the
    /// whole guest call inside a single poll and the caller's future is never
    /// pending inside the guard. Reaching the window from outside needs a guest
    /// that awaits an async host function, which cannot be built here.
    #[tokio::test]
    async fn a_cancelled_guest_call_leaves_the_adapter_dead_not_live() {
        let middleware = fixture_middleware().await;
        {
            let mut guard = middleware.inner.lock().await;
            drop(GuestCall::arm(&middleware, &mut guard, MiddlewareStage::Inbound));
            assert!(
                matches!(guard.state, MiddlewareState::Dead { .. }),
                "an armed call that never returned must poison the state before the guard is released"
            );
        }

        let cancelled = middleware.diagnostics();
        assert_eq!(
            cancelled.get("state").and_then(serde_json::Value::as_str),
            Some("restarting"),
            "{cancelled}"
        );
        let reason = cancelled
            .get("last_error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(reason.contains("cancelled"), "{cancelled}");

        let recovered = middleware
            .process(MiddlewareStage::Inbound, r#"{"text":"hello"}"#)
            .await
            .expect("test: the next call rebuilds on a fresh store");
        assert!(recovered.contains("middleware_probe"), "{recovered}");

        let after = middleware.diagnostics();
        assert_eq!(
            after.get("restart_count").and_then(serde_json::Value::as_u64),
            Some(1),
            "{after}"
        );
        assert_eq!(
            after.get("state").and_then(serde_json::Value::as_str),
            Some("live"),
            "{after}"
        );
        assert!(
            !after
                .get("last_error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .contains("cannot enter component instance"),
            "the recorded reason must be the cancellation, not the next call's symptom: {after}"
        );
    }

    #[test]
    fn every_rebuild_attempt_is_charged_to_the_rolling_budget() {
        let budget = u64::try_from(MAX_RESTARTS_PER_WINDOW).expect("test: the budget fits a u64");
        let mut inner = dead_inner("wasm trap: unreachable");
        let now = Instant::now();

        for attempt in 1..=MAX_RESTARTS_PER_WINDOW {
            assert!(inner.claim_restart(now), "attempt {attempt} is inside the budget");
        }
        // A rebuild that fails to instantiate has already been charged here, so
        // it cannot quarantine the adapter ahead of the budget or retry forever.
        assert!(!inner.claim_restart(now), "the budget is spent");
        assert_eq!(inner.restart_count, budget);

        assert!(
            !inner.claim_restart(now + RESTART_WINDOW / 2),
            "the window is rolling: nothing has aged out halfway through it"
        );

        let later = now + RESTART_WINDOW + Duration::from_secs(1);
        for attempt in 1..=MAX_RESTARTS_PER_WINDOW {
            assert!(inner.claim_restart(later), "attempt {attempt} of the next window");
        }
        assert!(
            !inner.claim_restart(later),
            "the next window has its own budget, not a doubled one"
        );
        assert_eq!(inner.restart_attempts.len(), MAX_RESTARTS_PER_WINDOW);
        assert_eq!(inner.restart_count, budget * 2);
    }

    #[test]
    fn a_guest_authored_reason_is_bounded_before_it_reaches_status() {
        let short = "middleware 'pipeline-middleware' returned error: unsupported stage";
        assert_eq!(bounded_reason(short), short);

        let long = "e".repeat(MAX_LAST_ERROR_CHARS * 4);
        let bounded = bounded_reason(&long);
        assert!(bounded.ends_with(LAST_ERROR_TRUNCATION_SUFFIX), "{bounded}");
        assert_eq!(
            bounded.chars().count(),
            MAX_LAST_ERROR_CHARS + LAST_ERROR_TRUNCATION_SUFFIX.chars().count()
        );

        // A multi-byte reason must be cut on a character boundary, not inside one.
        let multibyte = "\u{00fc}".repeat(MAX_LAST_ERROR_CHARS + 10);
        let bounded = bounded_reason(&multibyte);
        assert_eq!(
            bounded.chars().count(),
            MAX_LAST_ERROR_CHARS + LAST_ERROR_TRUNCATION_SUFFIX.chars().count()
        );
    }

    /// Like `FIXTURE_MANIFEST`, with the host-side payload cap raised to the
    /// guest's memory so a four-megabyte envelope reaches the guest and traps it.
    const TRAPPING_FIXTURE_MANIFEST: &str = r#"
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
max_payload_bytes = 8388608
"#;

    /// D-023: a turn aborted while a middleware stage runs no longer cancels
    /// the guest call. The stage runs to its own outcome on its own task, so a
    /// trapping guest is charged for its trap rather than excused as a
    /// cancellation.
    #[tokio::test]
    async fn an_abandoned_middleware_stage_is_still_charged_for_its_own_failure() {
        use std::future::Future;
        use std::task::{Context, Waker};

        let engine = crate::plugins::shared_wasm_engine().expect("test: shared wasmtime engine");
        let wasm =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/pipeline-middleware/plugin.wasm");
        let bytes = std::fs::read(wasm).expect("test: the committed middleware fixture is readable");
        let component =
            wasmtime::component::Component::from_binary(&engine, &bytes).expect("test: the fixture is a component");
        let manifest: PluginManifest = toml::from_str(TRAPPING_FIXTURE_MANIFEST).expect("test: manifest parses");
        let middleware = WasmMiddleware::new(
            &engine,
            &component,
            &manifest,
            std::collections::HashSet::new(),
            10,
            None,
            None,
        )
        .await
        .expect("test: the fixture instantiates");

        let trapping = serde_json::json!({"text": "hello", "filler": "x".repeat(4 * 1024 * 1024)}).to_string();
        {
            let guard = middleware.inner.lock().await;
            {
                let mut stage = std::pin::pin!(middleware.process(MiddlewareStage::Inbound, &trapping));
                assert!(
                    stage
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending(),
                    "the stage must be waiting for the store"
                );
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
            }
            drop(guard);
        }

        let next = middleware
            .process(MiddlewareStage::Inbound, r#"{"text":"hello"}"#)
            .await
            .expect("test: the next stage runs on a rebuilt store");
        assert!(next.contains("middleware_probe"), "{next}");
        let diagnostics = middleware.diagnostics();
        assert_eq!(
            diagnostics.get("failure_count").and_then(serde_json::Value::as_u64),
            Some(1),
            "the abandoned stage must have run and been charged for its trap: {diagnostics}"
        );
        assert_eq!(
            diagnostics.get("cancelled_count").and_then(serde_json::Value::as_u64),
            Some(0),
            "{diagnostics}"
        );
    }

    /// Runs the real `process` call against a store whose epoch deadline has
    /// already passed, so the committed guest is interrupted by the epoch
    /// deadline exactly as a guest spinning in wasm past
    /// `max_execution_time_ms` is — without spinning for real.
    #[cfg(target_has_atomic = "64")]
    struct PastEpochDeadline(ProcessCall);

    #[cfg(target_has_atomic = "64")]
    impl GuestOp for PastEpochDeadline {
        type Output = String;

        fn operation(&self) -> &'static str {
            self.0.operation()
        }

        fn invoke<'a>(
            self,
            store: &'a mut wasmtime::Store<HostState>,
            instance: &'a wasmtime::component::Instance,
        ) -> futures_util::future::BoxFuture<'a, CallOutcome<String>>
        where
            Self: 'a,
        {
            // Zero ticks beyond the current epoch: the deadline has been
            // reached, so the guest traps at its first epoch check.
            store.set_epoch_deadline(0);
            self.0.invoke(store, instance)
        }
    }

    /// R-C3Xh H1 / D-024: a guest the epoch deadline interrupts in wasm feeds
    /// the timeout streak like one the host timer stops in a host call, so
    /// four in a row quarantine it at any pace. Before the fix the interrupt
    /// was classed as a trap: never counted, and at a slow pace never caught.
    ///
    /// MUTATION GUARD: classify the `process` call's error without
    /// `recovery::unwound` (as a plain `Runtime` trap) and this goes red.
    #[cfg(target_has_atomic = "64")]
    #[tokio::test]
    async fn a_guest_interrupted_by_its_epoch_deadline_four_times_in_a_row_is_quarantined() {
        let middleware = fixture_middleware().await;
        for call in 1..=4 {
            let outcome = middleware
                .inner
                .call_detached(PastEpochDeadline(ProcessCall {
                    plugin_name: middleware.plugin_name.clone(),
                    stage: MiddlewareStage::Inbound,
                    data_json: r#"{"text":"spin"}"#.to_string(),
                }))
                .await;
            assert!(
                matches!(outcome, Err(PluginError::Timeout(30_000))),
                "call {call}: an epoch interrupt is the guest's own timeout: {outcome:?}"
            );
            // A slow pace: every earlier rebuild has aged out of the restart
            // window, so only the streak can quarantine.
            middleware.inner.lock().await.restart_attempts.clear();
        }
        assert!(
            middleware.is_quarantined(),
            "four epoch-deadline timeouts in a row must quarantine the adapter"
        );
    }
}
