//! Cron capability — bridges WASM cron plugins to the scheduler.
//!
//! Cron plugins declare a schedule in their manifest and export a `run`
//! function that is called by the scheduler when the schedule fires.

use std::collections::HashMap;
use std::str::FromStr;
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
        event_bus: Option<std::sync::Arc<crate::plugins::event_bus::EventBus>>,
    ) -> PluginResult<Self> {
        let timeout_ms = manifest.resources.max_execution_time_ms;

        let granted = manifest.permissions.required.iter().cloned().collect();
        let optional = manifest.permissions.optional.iter().cloned().collect();
        let mut host_state = HostState::new(
            manifest.plugin.name.clone(),
            manifest.config.clone(),
            granted,
            optional,
            manifest.permissions.http_allowlist.clone(),
            timeout_ms,
        );
        if let Some(bus) = event_bus {
            host_state = host_state.with_event_bus(bus);
        }

        let mut store = wasmtime::Store::new(engine, host_state);
        store
            .set_fuel(manifest.resources.max_fuel)
            .map_err(|e| PluginError::Instantiation(format!("failed to set fuel: {e}")))?;

        let mut linker = wasmtime::component::Linker::<HostState>::new(engine);
        Self::register_host_functions(&mut linker)?;

        let instance = linker
            .instantiate_async(&mut store, component)
            .await
            .map_err(|e| PluginError::Instantiation(format!("failed to instantiate cron: {e}")))?;

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
            .get_export_index(
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
            .get_export_index(store.as_context_mut(), Some(&iface_idx), "run")
            .ok_or_else(|| PluginError::Runtime("run not found in cron-exports".to_string()))?;

        let func = instance
            .get_func(store.as_context_mut(), &func_idx)
            .ok_or_else(|| PluginError::Runtime("run is not a function".to_string()))?;

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
            Ok(Ok(())) => match &results[0] {
                wasmtime::component::Val::Result(r) => match r.as_ref() {
                    Ok(Some(b)) => match b.as_ref() {
                        wasmtime::component::Val::String(s) => Ok(s.to_string()),
                        _ => Ok(String::new()),
                    },
                    Err(Some(b)) => match b.as_ref() {
                        wasmtime::component::Val::String(e) => Err(PluginError::Runtime(format!(
                            "cron '{}' returned error: {e}",
                            self.plugin_name
                        ))),
                        _ => Ok(String::new()),
                    },
                    _ => Ok(String::new()),
                },
                _ => Ok(String::new()),
            },
        }
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    pub fn schedule(&self) -> &str {
        &self.schedule
    }

    /// Register host functions for cron plugins.
    fn register_host_functions(
        linker: &mut wasmtime::component::Linker<HostState>,
    ) -> PluginResult<()> {
        super::common::register_common_host_functions(linker)
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

    /// Start the WASM cron scheduler loop.
    ///
    /// Spawns a background tokio task that polls every 30 seconds and fires
    /// any jobs whose cron expression has a due occurrence since the last run.
    /// The task runs until the returned `JoinHandle` is aborted or the process
    /// exits.
    ///
    /// Cron expressions follow the same 5-field (`min hour dom mon dow`) or
    /// 6-field (`sec min hour dom mon dow`) format used by the rest of PRX.
    /// Expressions are normalized via [`crate::cron::normalize_expression`]
    /// before being parsed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let manager = Arc::new(plugin_manager.create_cron_manager().await);
    /// if !manager.is_empty() {
    ///     let handle = WasmCronManager::start_scheduler(Arc::clone(&manager));
    ///     // keep handle alive for the duration of the process
    /// }
    /// ```
    pub fn start_scheduler(manager: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            const POLL_SECS: u64 = 30;
            let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(POLL_SECS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            // Track the last time each job was triggered so we don't double-fire.
            let mut last_triggered: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();

            loop {
                ticker.tick().await;
                let now = chrono::Utc::now();

                for job in &manager.jobs {
                    let name = job.plugin_name().to_string();
                    let schedule_str = job.schedule();

                    if schedule_str.is_empty() {
                        continue;
                    }

                    let due = is_cron_due(schedule_str, last_triggered.get(&name).copied(), now);
                    if !due {
                        continue;
                    }

                    last_triggered.insert(name.clone(), now);
                    tracing::debug!(plugin = %name, "firing WASM cron job");

                    match job.run().await {
                        Ok(output) => {
                            tracing::info!(
                                plugin = %name,
                                output = %output,
                                "WASM cron job completed"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                plugin = %name,
                                error = %e,
                                "WASM cron job failed"
                            );
                        }
                    }
                }
            }
        })
    }
}

/// Return `true` if the cron expression has a due occurrence between
/// `last_run` (exclusive) and `now` (inclusive).
///
/// On the very first run (`last_run` is `None`) we treat the window as
/// the previous 60 seconds, meaning a job that would have fired in the
/// last minute is considered due immediately on startup.
fn is_cron_due(
    schedule: &str,
    last_run: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let normalized = match crate::cron::normalize_expression(schedule) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("WASM cron: invalid expression '{}': {e}", schedule);
            return false;
        }
    };

    let cron_sched = match cron::Schedule::from_str(&normalized) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("WASM cron: failed to parse '{}': {e}", schedule);
            return false;
        }
    };

    let check_from = last_run.unwrap_or_else(|| now - chrono::Duration::seconds(60));
    matches!(cron_sched.after(&check_from).next(), Some(next) if next <= now)
}
