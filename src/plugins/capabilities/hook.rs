//! Hook capability — bridges WASM hook plugins to the HookManager lifecycle.
//!
//! Hook plugins observe lifecycle events (agent_start, tool_call, etc.)
//! without modifying the data flow.

use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use wasmtime::AsContextMut;

use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::{HostState, apply_store_epoch_deadline, apply_store_resource_limits};
use crate::plugins::manifest::PluginManifest;

const GUEST_EVENT_QUEUE_CAPACITY: usize = 128;

/// A loaded hook plugin instance.
pub struct WasmHook {
    plugin_name: String,
    events: HashSet<String>,
    inner: Arc<Mutex<WasmHookInner>>,
    timeout_ms: u64,
}

struct WasmHookInner {
    store: wasmtime::Store<HostState>,
    instance: wasmtime::component::Instance,
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
        event_bus: Option<std::sync::Arc<crate::plugins::event_bus::EventBus>>,
    ) -> PluginResult<Self> {
        let timeout_ms = manifest.resources.max_execution_time_ms;

        let granted = granted_permissions;
        let optional = manifest.permissions.optional.iter().cloned().collect();
        let (event_sink, mut event_receiver) = tokio::sync::mpsc::channel(GUEST_EVENT_QUEUE_CAPACITY);
        let mut host_state = HostState::new(
            manifest.plugin.name.clone(),
            manifest.config.clone(),
            granted,
            optional,
            manifest.permissions.http_allowlist.clone(),
            timeout_ms,
        )
        .with_event_sink(event_sink);
        if let Some(bus) = event_bus {
            host_state = host_state.with_event_bus(bus);
        }

        let mut store = wasmtime::Store::new(engine, host_state);
        apply_store_resource_limits(&mut store, manifest.resources.max_memory_mb);
        store
            .set_fuel(manifest.resources.max_fuel)
            .map_err(|e| PluginError::Instantiation(format!("failed to set fuel: {e}")))?;

        let mut linker = wasmtime::component::Linker::<HostState>::new(engine);
        Self::register_host_functions(&mut linker)?;

        let instance = linker
            .instantiate_async(&mut store, component)
            .await
            .map_err(|e| PluginError::Instantiation(format!("failed to instantiate hook: {e}")))?;

        let plugin_name = manifest.plugin.name.clone();
        let inner = Arc::new(Mutex::new(WasmHookInner { store, instance }));
        let weak_inner = Arc::downgrade(&inner);
        let pump_plugin_name = plugin_name.clone();
        tokio::spawn(async move {
            while let Some(message) = event_receiver.recv().await {
                let Some(inner) = weak_inner.upgrade() else {
                    break;
                };
                if let Err(error) =
                    invoke_guest_event(&inner, &pump_plugin_name, &message.topic, &message.payload, timeout_ms).await
                {
                    tracing::warn!(
                        plugin = %pump_plugin_name,
                        topic = %message.topic,
                        error = %error,
                        "WASM subscriber pump delivery failed"
                    );
                }
            }
        });

        Ok(Self {
            plugin_name,
            events,
            inner,
            timeout_ms,
        })
    }

    /// Check if this hook listens for a specific event.
    pub fn handles_event(&self, event: &str) -> bool {
        self.events.contains(event) || self.events.contains("*")
    }

    /// Fire the hook for an event.
    pub async fn on_event(&self, event: &str, payload_json: &str) -> PluginResult<()> {
        if !self.handles_event(event) {
            return Ok(());
        }

        invoke_guest_event(&self.inner, &self.plugin_name, event, payload_json, self.timeout_ms).await
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// Register host functions for hook plugins.
    fn register_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
        super::common::register_common_host_functions(linker)
    }
}

async fn invoke_guest_event(
    inner: &Arc<Mutex<WasmHookInner>>,
    plugin_name: &str,
    event: &str,
    payload_json: &str,
    timeout_ms: u64,
) -> PluginResult<()> {
    let mut inner = inner.lock().await;
    let WasmHookInner { store, instance } = &mut *inner;

    // Navigate to the exported interface with compatibility fallback.
    let iface_name_candidates = ["prx:plugin/hook-exports@0.1.0", "prx:plugin/hook-exports"];
    let iface_idx = iface_name_candidates
        .iter()
        .find_map(|name| instance.get_export_index(store.as_context_mut(), None, name))
        .ok_or_else(|| {
            PluginError::Runtime(format!(
                "plugin does not export any supported hook interface: {}",
                iface_name_candidates.join(", ")
            ))
        })?;

    let func_idx = instance
        .get_export_index(store.as_context_mut(), Some(&iface_idx), "on-event")
        .ok_or_else(|| PluginError::Runtime("on-event not found in hook-exports".to_string()))?;

    let func = instance
        .get_func(store.as_context_mut(), &func_idx)
        .ok_or_else(|| PluginError::Runtime("on-event is not a function".to_string()))?;

    let params = [
        wasmtime::component::Val::String(event.into()),
        wasmtime::component::Val::String(payload_json.into()),
    ];
    let mut results = [wasmtime::component::Val::Bool(false)];

    apply_store_epoch_deadline(store, timeout_ms);
    let call_result = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        func.call_async(&mut *store, &params, &mut results),
    )
    .await;

    match call_result {
        Err(_) => Err(PluginError::Timeout(timeout_ms)),
        Ok(Err(e)) => Err(PluginError::Runtime(format!(
            "hook '{}' on-event error: {e}",
            plugin_name
        ))),
        Ok(Ok(())) => match &results[0] {
            wasmtime::component::Val::Result(r) => match r.as_ref() {
                Err(Some(b)) => match b.as_ref() {
                    wasmtime::component::Val::String(e) => Err(PluginError::Runtime(format!(
                        "hook '{}' returned error: {e}",
                        plugin_name
                    ))),
                    _ => Ok(()),
                },
                _ => Ok(()),
            },
            _ => Ok(()),
        },
    }
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
    pub async fn emit(&self, event: &str, payload_json: &str) {
        for hook in &self.hooks {
            if hook.handles_event(event) {
                if let Err(e) = hook.on_event(event, payload_json).await {
                    tracing::warn!(
                        plugin = %hook.plugin_name(),
                        event = %event,
                        error = %e,
                        "WASM hook execution failed"
                    );
                }
            }
        }
    }

    /// Returns true if no hooks are registered.
    pub const fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}
