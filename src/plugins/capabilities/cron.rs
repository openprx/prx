//! Cron capability — bridges WASM cron plugins to the scheduler.
//!
//! Cron plugins declare a schedule in their manifest and export a `run`
//! function that is called by the scheduler when the schedule fires.

use std::sync::Arc;
use tokio::sync::Mutex;
use wasmtime::AsContextMut;

use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::HostState;
use crate::plugins::manifest::PluginManifest;

/// A loaded cron plugin instance.
pub struct WasmCronJob {
    plugin_name: String,
    schedule: String,
    inner: Arc<Mutex<WasmCronInner>>,
    timeout_ms: u64,
}

struct WasmCronInner {
    store: wasmtime::Store<HostState>,
    instance: wasmtime::component::Instance,
}

impl WasmCronJob {
    /// Create a new cron adapter from a compiled WASM component.
    pub async fn new(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        schedule: String,
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
                PluginError::Instantiation(format!("failed to instantiate cron: {e}"))
            })?;

        Ok(Self {
            plugin_name: manifest.plugin.name.clone(),
            schedule,
            inner: Arc::new(Mutex::new(WasmCronInner { store, instance })),
            timeout_ms,
        })
    }

    /// Execute the cron job's `run` function.
    pub async fn run(&self) -> PluginResult<String> {
        let mut inner = self.inner.lock().await;
        let WasmCronInner { store, instance } = &mut *inner;

        // Navigate to the exported interface: prx:plugin/cron-exports@0.1.0
        let iface_idx = instance
            .get_export(
                store.as_context_mut(),
                None,
                "prx:plugin/cron-exports@0.1.0",
            )
            .ok_or_else(|| {
                PluginError::Runtime(
                    "plugin does not export prx:plugin/cron-exports@0.1.0".to_string(),
                )
            })?;

        let func_idx = instance
            .get_export(store.as_context_mut(), Some(&iface_idx), "run")
            .ok_or_else(|| {
                PluginError::Runtime("run not found in cron-exports".to_string())
            })?;

        let func = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| {
                PluginError::Runtime("run is not a function".to_string())
            })?;

        let params = [];
        let mut results = [wasmtime::component::Val::Bool(false)];

        let call_result = tokio::time::timeout(
            std::time::Duration::from_millis(self.timeout_ms),
            func.call_async(&mut *store, &params, &mut results),
        )
        .await;

        match call_result {
            Err(_) => Err(PluginError::Timeout(self.timeout_ms)),
            Ok(Err(e)) => Err(PluginError::Runtime(format!(
                "cron '{}' run error: {e}",
                self.plugin_name
            ))),
            Ok(Ok(())) => {
                func.post_return_async(&mut *store).await.ok();
                match &results[0] {
                    wasmtime::component::Val::Result(r) => match r.as_ref() {
                        Ok(Some(b)) => match b.as_ref() {
                            wasmtime::component::Val::String(s) => Ok(s.to_string()),
                            _ => Ok(String::new()),
                        },
                        Err(Some(b)) => match b.as_ref() {
                            wasmtime::component::Val::String(e) => {
                                Err(PluginError::Runtime(format!(
                                    "cron '{}' returned error: {e}",
                                    self.plugin_name
                                )))
                            }
                            _ => Ok(String::new()),
                        },
                        _ => Ok(String::new()),
                    },
                    _ => Ok(String::new()),
                }
            }
        }
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    pub fn schedule(&self) -> &str {
        &self.schedule
    }

    /// Register minimal host functions for cron plugins.
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

/// Manager for WASM cron plugins. Can be integrated with the existing
/// scheduler to add WASM-based scheduled tasks alongside native cron jobs.
pub struct WasmCronManager {
    jobs: Vec<WasmCronJob>,
}

impl WasmCronManager {
    pub fn new() -> Self {
        Self { jobs: Vec::new() }
    }

    /// Add a WASM cron job.
    pub fn add(&mut self, job: WasmCronJob) {
        self.jobs.push(job);
    }

    /// Get all registered cron jobs.
    pub fn jobs(&self) -> &[WasmCronJob] {
        &self.jobs
    }

    /// Returns true if no cron jobs are registered.
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Execute a specific cron job by plugin name.
    pub async fn run_job(&self, plugin_name: &str) -> PluginResult<String> {
        for job in &self.jobs {
            if job.plugin_name() == plugin_name {
                return job.run().await;
            }
        }
        Err(PluginError::NotFound {
            name: plugin_name.to_string(),
        })
    }
}
