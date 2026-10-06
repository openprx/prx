//! Hook capability — bridges WASM hook plugins to the HookManager lifecycle.
//!
//! Hook plugins observe lifecycle events (agent_start, tool_call, etc.)
//! without modifying the data flow.
//!
//! # Trap recovery
//!
//! The adapter's store is owned by a [`Supervisor`]: a trapped hook is rebuilt
//! on a brand-new `Store` by the next delivery, rebuilds are budgeted inside a
//! rolling window, and once the budget is spent the hook is quarantined until
//! the next plugin generation replaces it. Hooks are observe-only — no caller
//! consults their result — so every failure fails open: the event is simply not
//! observed by this hook, and the adapter's status records why.

use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use wasmtime::AsContextMut;

use super::recovery::{AdapterHealth, CallOutcome, CapabilityKind, GuestOp, Respawn, Supervisor};
use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::HostState;
use crate::plugins::manifest::PluginManifest;

const GUEST_EVENT_QUEUE_CAPACITY: usize = 128;

const HOOK_INTERFACES: [&str; 2] = ["prx:plugin/hook-exports@0.1.0", "prx:plugin/hook-exports"];

/// A loaded hook plugin instance.
pub struct WasmHook {
    plugin_name: String,
    events: HashSet<String>,
    supervisor: Arc<Supervisor>,
    invocation_state: Arc<HookInvocationState>,
}

#[derive(Default)]
struct HookInvocationState {
    count: AtomicU64,
    last_event: RwLock<Option<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WasmHookDiagnostics {
    pub plugin: String,
    pub event_patterns: Vec<String>,
    pub invocation_count: u64,
    pub last_event: Option<String>,
    /// Most recent failure, kept after later successful deliveries; see
    /// `last_error_at` for when it happened.
    pub last_error: Option<String>,
    #[serde(flatten)]
    pub health: AdapterHealth,
}

/// One delivery to the guest's `on-event` export.
struct OnEventCall {
    plugin_name: String,
    event: String,
    payload_json: String,
    /// Counted when the supervisor enters the guest, never for a delivery
    /// it refuses (quarantined, or dead awaiting a rebuild).
    invocation_state: Arc<HookInvocationState>,
}

impl GuestOp for OnEventCall {
    type Output = ();

    fn operation(&self) -> &'static str {
        "on-event"
    }

    fn invoke<'a>(
        self,
        store: &'a mut wasmtime::Store<HostState>,
        instance: &'a wasmtime::component::Instance,
    ) -> futures_util::future::BoxFuture<'a, CallOutcome<()>>
    where
        Self: 'a,
    {
        Box::pin(self.run(store, instance))
    }
}

impl OnEventCall {
    async fn run(
        self,
        store: &mut wasmtime::Store<HostState>,
        instance: &wasmtime::component::Instance,
    ) -> CallOutcome<()> {
        let Self {
            plugin_name,
            event,
            payload_json,
            invocation_state,
        } = self;
        invocation_state.count.fetch_add(1, Ordering::Relaxed);
        *invocation_state.last_event.write() = Some(event.clone());
        // Navigate to the exported interface with compatibility fallback.
        let Some(iface_idx) = HOOK_INTERFACES
            .iter()
            .find_map(|name| instance.get_export_index(store.as_context_mut(), None, name))
        else {
            return CallOutcome::GuestError(PluginError::Runtime(format!(
                "plugin does not export any supported hook interface: {}",
                HOOK_INTERFACES.join(", ")
            )));
        };

        let Some(func_idx) = instance.get_export_index(store.as_context_mut(), Some(&iface_idx), "on-event") else {
            return CallOutcome::GuestError(PluginError::Runtime("on-event not found in hook-exports".to_string()));
        };

        let Some(func) = instance.get_func(store.as_context_mut(), &func_idx) else {
            return CallOutcome::GuestError(PluginError::Runtime("on-event is not a function".to_string()));
        };

        let params = [
            wasmtime::component::Val::String(event.as_str().into()),
            wasmtime::component::Val::String(payload_json.as_str().into()),
        ];
        let mut results = [wasmtime::component::Val::Bool(false)];

        if let Err(e) = func.call_async(&mut *store, &params, &mut results).await {
            return super::recovery::unwound(store, &e, || format!("hook '{plugin_name}' on-event error: {e}"));
        }
        match results.first() {
            Some(wasmtime::component::Val::Result(r)) => match r.as_ref() {
                Err(Some(b)) => match b.as_ref() {
                    wasmtime::component::Val::String(e) => CallOutcome::GuestError(PluginError::Runtime(format!(
                        "hook '{plugin_name}' returned error: {e}"
                    ))),
                    _ => CallOutcome::Completed(()),
                },
                _ => CallOutcome::Completed(()),
            },
            _ => CallOutcome::Completed(()),
        }
    }
}

impl WasmHook {
    /// Create a new hook adapter from a compiled WASM component.
    /// `granted_permissions` must come from `LoadedPlugin.granted_permissions`
    /// (policy-filtered), NOT directly from the manifest.
    pub async fn new(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        granted_permissions: HashSet<String>,
        events: HashSet<String>,
        memory: Option<Arc<dyn crate::memory::traits::Memory>>,
        event_bus: Option<std::sync::Arc<crate::plugins::event_bus::EventBus>>,
    ) -> PluginResult<Self> {
        let (event_sink, mut event_receiver) = tokio::sync::mpsc::channel(GUEST_EVENT_QUEUE_CAPACITY);
        // Every store the adapter ever builds shares this sink, so a
        // subscription registered before a rebuild keeps delivering after it.
        let respawn = Respawn::new(
            CapabilityKind::Hook,
            engine,
            component,
            manifest,
            granted_permissions,
            memory,
            event_bus,
            Self::register_host_functions,
        )
        .with_event_sink(event_sink);
        let live = respawn.instantiate().await?;

        let plugin_name = manifest.plugin.name.clone();
        let supervisor = Arc::new(Supervisor::new(respawn, live));
        let invocation_state = Arc::new(HookInvocationState::default());
        let weak_supervisor = Arc::downgrade(&supervisor);
        let pump_plugin_name = plugin_name.clone();
        let pump_invocation_state = Arc::clone(&invocation_state);
        tokio::spawn(async move {
            while let Some(message) = event_receiver.recv().await {
                let Some(supervisor) = weak_supervisor.upgrade() else {
                    break;
                };
                // Hooks fail open; the supervisor records every failure in the
                // adapter's status and logs transitions plus a bounded number
                // of failures, so there is nothing left to report here.
                let _ = deliver(
                    &supervisor,
                    &pump_invocation_state,
                    &pump_plugin_name,
                    &message.topic,
                    &message.payload,
                )
                .await;
            }
        });

        Ok(Self {
            plugin_name,
            events,
            supervisor,
            invocation_state,
        })
    }

    /// Check if this hook listens for a specific event.
    pub fn handles_event(&self, event: &str) -> bool {
        self.events.iter().any(|pattern| event_pattern_matches(pattern, event))
    }

    /// Fire the hook for an event.
    pub async fn on_event(&self, event: &str, payload_json: &str) -> PluginResult<()> {
        if !self.handles_event(event) {
            return Ok(());
        }
        deliver(
            &self.supervisor,
            &self.invocation_state,
            &self.plugin_name,
            event,
            payload_json,
        )
        .await
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    pub fn is_quarantined(&self) -> bool {
        self.supervisor.is_quarantined()
    }

    pub fn diagnostics(&self) -> WasmHookDiagnostics {
        let mut event_patterns = self.events.iter().cloned().collect::<Vec<_>>();
        event_patterns.sort();
        WasmHookDiagnostics {
            plugin: self.plugin_name.clone(),
            event_patterns,
            invocation_count: self.invocation_state.count.load(Ordering::Relaxed),
            last_event: self.invocation_state.last_event.read().clone(),
            last_error: self.supervisor.last_error(),
            health: self.supervisor.health(),
        }
    }

    /// Register host functions for hook plugins.
    fn register_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
        super::common::register_common_host_functions(linker)
    }
}

/// Deliver one event to the guest, lifecycle and subscriber pump alike. A
/// payload the guest cannot hold is skipped rather than handed over: hooks are
/// observe-only, and skipping keeps the store from being poisoned by it.
async fn deliver(
    supervisor: &Arc<Supervisor>,
    invocation_state: &Arc<HookInvocationState>,
    plugin_name: &str,
    event: &str,
    payload_json: &str,
) -> PluginResult<()> {
    if !supervisor.admit_payload("on-event", event.len().saturating_add(payload_json.len())) {
        return Ok(());
    }
    // Detached: an aborted turn cannot drop a delivery mid-guest-call.
    supervisor
        .call_detached(OnEventCall {
            plugin_name: plugin_name.to_string(),
            event: event.to_string(),
            payload_json: payload_json.to_string(),
            invocation_state: Arc::clone(invocation_state),
        })
        .await
}

fn event_pattern_matches(pattern: &str, event: &str) -> bool {
    let pattern = pattern.trim();
    let event = event.trim();
    if pattern == "*" || pattern == event {
        return true;
    }

    let raw_event = event.strip_prefix("prx.lifecycle.").unwrap_or(event);
    if pattern == raw_event {
        return true;
    }

    pattern.strip_suffix('*').is_some_and(|prefix| {
        event.starts_with(prefix)
            || raw_event.starts_with(prefix)
            || format!("prx.lifecycle.{raw_event}").starts_with(prefix)
    })
}

/// Manager for WASM hook plugins. Integrates with the existing HookManager
/// by providing an additional WASM execution path for lifecycle events.
pub struct WasmHookExecutor {
    hooks: Vec<WasmHook>,
}

impl WasmHookExecutor {
    pub const fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Add a WASM hook to the executor.
    pub fn add(&mut self, hook: WasmHook) {
        self.hooks.push(hook);
    }

    /// Fire all WASM hooks that listen for the given event.
    ///
    /// Hooks fail open: a failing, rebuilding or quarantined hook simply does
    /// not observe the event. The adapter owns the reporting — it logs state
    /// transitions and a bounded number of failures, and publishes
    /// `failure_count`/`last_error` in its diagnostics — so this loop only
    /// traces at debug level instead of warning once per hook per event.
    pub async fn emit(&self, event: &str, payload_json: &str) {
        for hook in &self.hooks {
            if hook.handles_event(event)
                && let Err(error) = hook.on_event(event, payload_json).await
            {
                tracing::debug!(
                    plugin = %hook.plugin_name(),
                    event = %event,
                    error = %error,
                    "WASM hook delivery failed"
                );
            }
        }
    }

    /// Returns true if no hooks are registered.
    pub const fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn diagnostics(&self) -> Vec<WasmHookDiagnostics> {
        self.hooks.iter().map(WasmHook::diagnostics).collect()
    }

    /// Hooks that spent their restart budget and no longer observe events.
    pub fn quarantined_plugins(&self) -> Vec<String> {
        self.hooks
            .iter()
            .filter(|hook| hook.is_quarantined())
            .map(|hook| hook.plugin_name().to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::event_pattern_matches;

    #[test]
    fn lifecycle_patterns_match_canonical_and_legacy_names() {
        assert!(event_pattern_matches("prx.lifecycle.*", "prx.lifecycle.turn_complete"));
        assert!(event_pattern_matches(
            "prx.lifecycle.turn_complete",
            "prx.lifecycle.turn_complete"
        ));
        assert!(event_pattern_matches("turn_complete", "prx.lifecycle.turn_complete"));
        assert!(event_pattern_matches("prx.lifecycle.*", "turn_complete"));
        assert!(!event_pattern_matches("prx.lifecycle.tool_call", "prx.lifecycle.error"));
    }

    mod recovery {
        use super::super::*;
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll, Waker};

        const EVENT: &str = "prx.lifecycle.turn_complete";

        /// Short enough to keep the timeout regressions fast, long enough that
        /// an unblocked delivery never comes near it.
        const SHORT_TIMEOUT_MS: u64 = 200;

        fn audit_manifest(timeout_ms: u64) -> PluginManifest {
            toml::from_str(&format!(
                r#"
[plugin]
name = "audit-hook"
version = "0.1.0"
wasm = "plugin.wasm"

[[capabilities]]
type = "hook"
name = "audit"

[[capabilities.events]]
pattern = "prx.lifecycle.*"

[permissions]
required = ["log", "kv", "events"]
optional = []

[resources]
max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = {timeout_ms}
"#
            ))
            .expect("test: audit hook manifest parses")
        }

        /// The committed `audit-hook` component. `on-event` starts by
        /// incrementing a counter through the async `kv` host functions, so a
        /// test that holds the adapter's KV write lock suspends the guest
        /// inside a real delivery.
        async fn audit_hook(timeout_ms: u64) -> WasmHook {
            let engine = crate::plugins::shared_wasm_engine().expect("test: shared wasmtime engine");
            let wasm =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/audit-hook/plugin.wasm");
            let bytes = std::fs::read(wasm).expect("test: the committed hook fixture is readable");
            let component =
                wasmtime::component::Component::from_binary(&engine, &bytes).expect("test: the fixture is a component");
            let granted = ["log", "kv", "events"].into_iter().map(str::to_string).collect();
            let events = std::iter::once("prx.lifecycle.*".to_string()).collect();
            WasmHook::new(
                &engine,
                &component,
                &audit_manifest(timeout_ms),
                granted,
                events,
                None,
                None,
            )
            .await
            .expect("test: the fixture instantiates")
        }

        /// The KV namespace the guest's `kv` host functions lock.
        fn kv_namespace(hook: &WasmHook) -> Arc<tokio::sync::RwLock<std::collections::HashMap<String, Vec<u8>>>> {
            hook.supervisor.kv_namespace()
        }

        fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
            future.poll(&mut Context::from_waker(Waker::noop()))
        }

        /// D-023: a caller that gives up on a hook delivery — a turn aborted
        /// while lifecycle hooks run — no longer cancels the guest call. The
        /// delivery finishes on its own task, so nothing is poisoned or
        /// charged, and every abandoned delivery is still observed.
        #[tokio::test]
        async fn an_abandoned_hook_delivery_finishes_on_its_own_and_poisons_nothing() {
            let hook = audit_hook(30_000).await;
            let kv = kv_namespace(&hook);
            for round in 1..=4 {
                let hold = kv.write().await;
                {
                    let mut delivery = std::pin::pin!(hook.on_event(EVENT, "{}"));
                    assert!(
                        poll_once(delivery.as_mut()).is_pending(),
                        "round {round}: the delivery must be pending"
                    );
                    for _ in 0..8 {
                        tokio::task::yield_now().await;
                    }
                }
                drop(hold);
                hook.on_event(EVENT, "{}")
                    .await
                    .unwrap_or_else(|error| panic!("round {round}: the delivery after an abandoned one: {error}"));
            }
            let total = kv
                .read()
                .await
                .get("count:total")
                .and_then(|bytes| serde_json::from_slice::<i64>(bytes).ok());
            assert_eq!(total, Some(8), "every abandoned delivery must still have completed");
            let diagnostics = hook.diagnostics();
            assert_eq!(diagnostics.health.restart_count, 0);
            assert_eq!(diagnostics.health.cancelled_count, 0);
            assert_eq!(diagnostics.health.failure_count, 0);
        }

        /// A guest that stays suspended past `max_execution_time_ms` is
        /// charged like a trap: the next delivery rebuilds, and a hook that
        /// keeps timing out is quarantined.
        #[tokio::test]
        async fn a_timed_out_hook_is_rebuilt_then_quarantined() {
            let hook = audit_hook(SHORT_TIMEOUT_MS).await;
            let kv = kv_namespace(&hook);

            let hold = kv.write().await;
            let timed_out = hook.on_event(EVENT, "{}").await;
            drop(hold);
            assert!(matches!(timed_out, Err(PluginError::Timeout(_))), "{timed_out:?}");
            hook.on_event(EVENT, "{}")
                .await
                .unwrap_or_else(|error| panic!("the delivery after a timeout: {error}"));

            let hold = kv.write().await;
            for attempt in 1..=4 {
                assert!(
                    hook.on_event(EVENT, "{}").await.is_err(),
                    "attempt {attempt} must fail while the KV lock is held"
                );
            }
            drop(hold);
            let refused = hook.on_event(EVENT, "{}").await;
            assert!(matches!(refused, Err(PluginError::Quarantined { .. })), "{refused:?}");
        }

        /// R-C3X F9: `invocation_count` and `last_event` report deliveries
        /// that reached the guest. A delivery refused because the hook is
        /// quarantined never entered it and must not move either.
        #[tokio::test]
        async fn a_delivery_refused_by_a_quarantined_hook_is_not_counted() {
            let hook = audit_hook(SHORT_TIMEOUT_MS).await;
            let kv = kv_namespace(&hook);
            let hold = kv.write().await;
            for attempt in 1..=4 {
                assert!(
                    matches!(hook.on_event(EVENT, "{}").await, Err(PluginError::Timeout(_))),
                    "attempt {attempt} must time out while the KV lock is held"
                );
            }
            drop(hold);
            assert!(hook.is_quarantined(), "four timeouts in a row quarantine the hook");
            let delivered = hook.diagnostics().invocation_count;
            assert_eq!(delivered, 4, "every delivery that entered the guest is counted");

            let refused = hook.on_event("prx.lifecycle.refused_event", "{}").await;
            assert!(matches!(refused, Err(PluginError::Quarantined { .. })), "{refused:?}");
            let diagnostics = hook.diagnostics();
            assert_eq!(
                diagnostics.invocation_count, delivered,
                "a refused delivery is not an invocation"
            );
            assert_eq!(diagnostics.last_event.as_deref(), Some(EVENT));
        }
    }
}
