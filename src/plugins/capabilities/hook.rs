//! Hook capability — bridges WASM hook plugins to the HookManager lifecycle.
//!
//! Hook plugins observe lifecycle events (agent_start, tool_call, etc.)
//! without modifying the data flow.

use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use wasmtime::AsContextMut;

use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::HostState;
use crate::plugins::manifest::PluginManifest;

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
    pub async fn new(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        events: HashSet<String>,
    ) -> PluginResult<Self> {
        let timeout_ms = manifest.resources.max_execution_time_ms;

        let granted = manifest.permissions.required.iter().cloned().collect();
        let optional = manifest.permissions.optional.iter().cloned().collect();
        let host_state = HostState::new(
            manifest.plugin.name.clone(),
            manifest.config.clone(),
            granted,
            optional,
            manifest.permissions.http_allowlist.clone(),
            timeout_ms,
        );

        let mut store = wasmtime::Store::new(engine, host_state);
        store.set_fuel(manifest.resources.max_fuel).map_err(|e| {
            PluginError::Instantiation(format!("failed to set fuel: {e}"))
        })?;

        let mut linker = wasmtime::component::Linker::<HostState>::new(engine);
        Self::register_host_functions(&mut linker)?;

        let instance = linker
            .instantiate_async(&mut store, component)
            .await
            .map_err(|e| {
                PluginError::Instantiation(format!("failed to instantiate hook: {e}"))
            })?;

        Ok(Self {
            plugin_name: manifest.plugin.name.clone(),
            events,
            inner: Arc::new(Mutex::new(WasmHookInner { store, instance })),
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

        let mut inner = self.inner.lock().await;
        let WasmHookInner { store, instance } = &mut *inner;

        // Navigate to the exported interface: prx:plugin/hook-exports@0.1.0
        let iface_idx = instance
            .get_export(
                store.as_context_mut(),
                None,
                "prx:plugin/hook-exports@0.1.0",
            )
            .ok_or_else(|| {
                PluginError::Runtime(
                    "plugin does not export prx:plugin/hook-exports@0.1.0".to_string(),
                )
            })?;

        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "on-event")
            .ok_or_else(|| {
                PluginError::Runtime("on-event not found in hook-exports".to_string())
            })?;

        let func = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| {
                PluginError::Runtime("on-event is not a function".to_string())
            })?;

        let params = [
            wasmtime::component::Val::String(event.into()),
            wasmtime::component::Val::String(payload_json.into()),
        ];
        let mut results = [wasmtime::component::Val::Bool(false)];

        let call_result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            func.call_async(&mut *store, &params, &mut results),
        )
        .await;

        match call_result {
            Err(_) => Err(PluginError::Timeout(self.timeout_ms)),
            Ok(Err(e)) => Err(PluginError::Runtime(format!(
                "hook '{}' on-event error: {e}",
                self.plugin_name
            ))),
            Ok(Ok(())) => {
                func.post_return_async(&mut *store).await.ok();
                // Check for error result
                match &results[0] {
                    wasmtime::component::Val::Result(r) => match r.as_ref() {
                        Err(Some(b)) => match b.as_ref() {
                            wasmtime::component::Val::String(e) => {
                                Err(PluginError::Runtime(format!(
                                    "hook '{}' returned error: {e}",
                                    self.plugin_name
                                )))
                            }
                            _ => Ok(()),
                        },
                        _ => Ok(()),
                    },
                    _ => Ok(()),
                }
            }
        }
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// Register minimal host functions for hook plugins.
    fn register_host_functions(
        linker: &mut wasmtime::component::Linker<HostState>,
    ) -> PluginResult<()> {
        // prx:host/log@0.1.0
        let mut log_inst = linker
            .instance("prx:host/log@0.1.0")
            .map_err(|e| PluginError::Instantiation(format!("linker error (log): {e}")))?;
        log_inst
            .func_wrap(
                "log",
                |store: wasmtime::StoreContextMut<'_, HostState>,
                 (level, message): (String, String)| {
                    let name = store.data().plugin_name.clone();
                    match level.as_str() {
                        "trace" => tracing::trace!(plugin = %name, "{message}"),
                        "debug" => tracing::debug!(plugin = %name, "{message}"),
                        "info" => tracing::info!(plugin = %name, "{message}"),
                        "warn" => tracing::warn!(plugin = %name, "{message}"),
                        "error" => tracing::error!(plugin = %name, "{message}"),
                        _ => tracing::info!(plugin = %name, level = %level, "{message}"),
                    }
                    Ok(())
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link log.log: {e}")))?;

        // prx:host/config@0.1.0
        let mut config_inst = linker
            .instance("prx:host/config@0.1.0")
            .map_err(|e| PluginError::Instantiation(format!("linker error (config): {e}")))?;
        config_inst
            .func_wrap(
                "get",
                |store: wasmtime::StoreContextMut<'_, HostState>, (key,): (String,)| {
                    let value = store.data().config.get(&key).cloned();
                    Ok((value,))
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link config.get: {e}")))?;
        config_inst
            .func_wrap(
                "get-all",
                |store: wasmtime::StoreContextMut<'_, HostState>, (): ()| {
                    let pairs: Vec<(String, String)> = store
                        .data()
                        .config
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    Ok((pairs,))
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link config.get-all: {e}")))?;

        // prx:host/kv@0.1.0
        let mut kv_inst = linker
            .instance("prx:host/kv@0.1.0")
            .map_err(|e| PluginError::Instantiation(format!("linker error (kv): {e}")))?;
        kv_inst
            .func_wrap_async(
                "get",
                |store: wasmtime::StoreContextMut<'_, HostState>, (key,): (String,)| {
                    Box::new(async move {
                        let kv = store.data().kv_store.clone();
                        let guard = kv.read().await;
                        let value = guard.get(&key).cloned();
                        Ok((value,))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link kv.get: {e}")))?;
        kv_inst
            .func_wrap_async(
                "set",
                |store: wasmtime::StoreContextMut<'_, HostState>,
                 (key, value): (String, Vec<u8>)| {
                    Box::new(async move {
                        let kv = store.data().kv_store.clone();
                        let mut guard = kv.write().await;
                        guard.insert(key, value);
                        Ok(())
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link kv.set: {e}")))?;
        kv_inst
            .func_wrap_async(
                "delete",
                |store: wasmtime::StoreContextMut<'_, HostState>, (key,): (String,)| {
                    Box::new(async move {
                        let kv = store.data().kv_store.clone();
                        let mut guard = kv.write().await;
                        let existed = guard.remove(&key).is_some();
                        Ok((existed,))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link kv.delete: {e}")))?;
        kv_inst
            .func_wrap_async(
                "list-keys",
                |store: wasmtime::StoreContextMut<'_, HostState>, (prefix,): (String,)| {
                    Box::new(async move {
                        let kv = store.data().kv_store.clone();
                        let guard = kv.read().await;
                        let keys: Vec<String> = guard
                            .keys()
                            .filter(|k| k.starts_with(&prefix))
                            .cloned()
                            .collect();
                        Ok((keys,))
                    })
                },
            )
            .map_err(|e| PluginError::Instantiation(format!("link kv.list-keys: {e}")))?;

        Ok(())
    }
}

/// Manager for WASM hook plugins. Integrates with the existing HookManager
/// by providing an additional WASM execution path for lifecycle events.
pub struct WasmHookExecutor {
    hooks: Vec<WasmHook>,
}

impl WasmHookExecutor {
    pub fn new() -> Self {
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
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}
