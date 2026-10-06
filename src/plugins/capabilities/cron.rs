//! Cron capability — bridges WASM cron plugins to the scheduler.
//!
//! Cron plugins declare a schedule in their manifest and export a `run`
//! function that is called by the scheduler when the schedule fires.
//!
//! # Trap recovery
//!
//! The adapter's store is owned by a [`Supervisor`]: a trapped job is rebuilt
//! on a brand-new `Store` by its next run, rebuilds are budgeted inside a
//! rolling window, and once the budget is spent the job is quarantined until
//! the next plugin generation replaces it. A run that cannot complete returns
//! an error and is not retried before its next occurrence; a job never reports
//! output it did not produce. A rebuild starts the guest with empty linear
//! memory, so state a job kept there (a run counter, say) starts over.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use wasmtime::AsContextMut;

use super::recovery::{CallOutcome, CapabilityKind, GuestOp, Respawn, Supervisor};
use crate::plugins::error::{PluginError, PluginResult};
use crate::plugins::host::HostState;
use crate::plugins::manifest::PluginManifest;

/// A loaded cron plugin instance.
pub struct WasmCronJob {
    plugin_name: String,
    schedule: String,
    supervisor: Arc<Supervisor>,
}

/// One call of the guest's `run` export.
struct RunCall {
    plugin_name: String,
}

impl GuestOp for RunCall {
    type Output = String;

    fn operation(&self) -> &'static str {
        "run"
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

impl RunCall {
    async fn run(
        self,
        store: &mut wasmtime::Store<HostState>,
        instance: &wasmtime::component::Instance,
    ) -> CallOutcome<String> {
        let plugin_name = self.plugin_name;
        // Navigate to the exported interface: prx:plugin/cron-exports@0.1.0
        let Some(iface_idx) = instance.get_export_index(store.as_context_mut(), None, "prx:plugin/cron-exports@0.1.0")
        else {
            return CallOutcome::GuestError(PluginError::Runtime(
                "plugin does not export prx:plugin/cron-exports@0.1.0".to_string(),
            ));
        };

        let Some(func_idx) = instance.get_export_index(store.as_context_mut(), Some(&iface_idx), "run") else {
            return CallOutcome::GuestError(PluginError::Runtime("run not found in cron-exports".to_string()));
        };

        let Some(func) = instance.get_func(store.as_context_mut(), &func_idx) else {
            return CallOutcome::GuestError(PluginError::Runtime("run is not a function".to_string()));
        };

        let mut results = [wasmtime::component::Val::Bool(false)];
        if let Err(e) = func.call_async(&mut *store, &[], &mut results).await {
            return super::recovery::unwound(store, &e, || format!("cron '{plugin_name}' run error: {e}"));
        }
        match results.first() {
            Some(wasmtime::component::Val::Result(r)) => match r.as_ref() {
                Ok(Some(b)) => match b.as_ref() {
                    wasmtime::component::Val::String(s) => CallOutcome::Completed(s.to_string()),
                    _ => CallOutcome::Completed(String::new()),
                },
                Err(Some(b)) => match b.as_ref() {
                    wasmtime::component::Val::String(e) => CallOutcome::GuestError(PluginError::Runtime(format!(
                        "cron '{plugin_name}' returned error: {e}"
                    ))),
                    _ => CallOutcome::Completed(String::new()),
                },
                _ => CallOutcome::Completed(String::new()),
            },
            _ => CallOutcome::Completed(String::new()),
        }
    }
}

impl WasmCronJob {
    /// Create a new cron adapter from a compiled WASM component.
    /// `granted_permissions` must come from `LoadedPlugin.granted_permissions`
    /// (policy-filtered), NOT directly from the manifest.
    pub async fn new(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        granted_permissions: std::collections::HashSet<String>,
        schedule: String,
        memory: Option<Arc<dyn crate::memory::traits::Memory>>,
        event_bus: Option<std::sync::Arc<crate::plugins::event_bus::EventBus>>,
    ) -> PluginResult<Self> {
        let respawn = Respawn::new(
            CapabilityKind::Cron,
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
            schedule,
            supervisor: Arc::new(Supervisor::new(respawn, live)),
        })
    }

    /// Execute the cron job's `run` function.
    pub async fn run(&self) -> PluginResult<String> {
        // Detached: a cancelled `cron_run` call cannot drop a run it handed
        // over; the scheduler's own runs are never cancelled mid-run.
        self.supervisor
            .call_detached(RunCall {
                plugin_name: self.plugin_name.clone(),
            })
            .await
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    pub fn schedule(&self) -> &str {
        &self.schedule
    }

    pub fn is_quarantined(&self) -> bool {
        self.supervisor.is_quarantined()
    }

    /// Per-adapter health for `wasm_plugins_status`.
    pub fn diagnostics(&self) -> serde_json::Value {
        self.supervisor.diagnostics(serde_json::json!({
            "plugin": self.plugin_name,
            "schedule": self.schedule,
        }))
    }

    /// Register host functions for cron plugins.
    fn register_host_functions(linker: &mut wasmtime::component::Linker<HostState>) -> PluginResult<()> {
        super::common::register_common_host_functions(linker)
    }
}

/// Manager for WASM cron plugins. Can be integrated with the existing
/// scheduler to add WASM-based scheduled tasks alongside native cron jobs.
pub struct WasmCronManager {
    jobs: Vec<WasmCronJob>,
}

impl WasmCronManager {
    pub const fn new() -> Self {
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

    /// Adapter inventory used by runtime status and management surfaces.
    pub fn adapters(&self) -> Vec<serde_json::Value> {
        self.jobs.iter().map(WasmCronJob::diagnostics).collect()
    }

    /// Jobs that spent their restart budget and no longer run.
    pub fn quarantined_plugins(&self) -> Vec<String> {
        self.jobs
            .iter()
            .filter(|job| job.is_quarantined())
            .map(|job| job.plugin_name().to_string())
            .collect()
    }

    /// Returns true if no cron jobs are registered.
    pub const fn is_empty(&self) -> bool {
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

                manager.run_due_jobs(&mut last_triggered, now).await;
            }
        })
    }

    /// Execute jobs due at `now`, preserving trigger history outside the adapter
    /// generation so an atomic reload cannot double-fire schedules.
    pub(crate) async fn run_due_jobs(
        &self,
        last_triggered: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        for job in &self.jobs {
            let name = job.plugin_name().to_string();
            let schedule_str = job.schedule();
            if schedule_str.is_empty() || !is_cron_due(schedule_str, last_triggered.get(&name).copied(), now) {
                continue;
            }

            last_triggered.insert(name.clone(), now);
            tracing::debug!(plugin = %name, "firing WASM cron job");
            match job.run().await {
                Ok(output) => {
                    tracing::info!(plugin = %name, output = %output, "WASM cron job completed");
                }
                // The adapter logs state transitions, a bounded number of
                // failures and the first refusal while quarantined, and keeps
                // the totals in its status; a quarantined job due every 30
                // seconds must not add a warning per occurrence here.
                Err(error) => {
                    tracing::debug!(plugin = %name, error = %error, "WASM cron job did not complete");
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_MANIFEST: &str = r#"
[plugin]
name = "counter-cron"
version = "0.1.0"
wasm = "plugin.wasm"

[[capabilities]]
type = "cron"
name = "counter"
schedule = "*/30 * * * * *"

[permissions]
required = []
optional = []

[resources]
max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
"#;

    /// The committed `counter-cron` component. Its `run` takes no input and
    /// calls no host function, so no committed trigger can make it trap once
    /// fuel is metered per call; these tests drive the state machine with an
    /// injected fault instead. Its run counter lives in guest linear memory,
    /// which makes a rebuild on a fresh store directly observable.
    async fn counter_cron() -> WasmCronJob {
        let engine = crate::plugins::shared_wasm_engine().expect("test: shared wasmtime engine");
        let wasm = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/counter-cron/plugin.wasm");
        let bytes = std::fs::read(wasm).expect("test: the committed cron fixture is readable");
        let component =
            wasmtime::component::Component::from_binary(&engine, &bytes).expect("test: the fixture is a component");
        let manifest: PluginManifest = toml::from_str(FIXTURE_MANIFEST).expect("test: fixture manifest parses");
        WasmCronJob::new(
            &engine,
            &component,
            &manifest,
            std::collections::HashSet::new(),
            "*/30 * * * * *".to_string(),
            None,
            None,
        )
        .await
        .expect("test: the fixture instantiates")
    }

    fn counter(job: &WasmCronJob, field: &str) -> Option<u64> {
        job.diagnostics().get(field).and_then(serde_json::Value::as_u64)
    }

    #[tokio::test]
    async fn a_faulted_cron_job_reruns_on_a_fresh_store() {
        let job = counter_cron().await;
        assert_eq!(job.run().await.expect("test: first run"), "counter-cron-run-1");
        assert_eq!(job.run().await.expect("test: second run"), "counter-cron-run-2");

        job.supervisor.inject_fault("wasm trap: unreachable").await;
        assert_eq!(
            job.run().await.expect("test: the next run rebuilds"),
            "counter-cron-run-1",
            "a rebuilt job starts with empty guest memory, so its counter starts over"
        );
        assert_eq!(counter(&job, "restart_count"), Some(1));
        assert_eq!(counter(&job, "failure_count"), Some(1));
        assert_eq!(
            job.diagnostics().get("state").and_then(serde_json::Value::as_str),
            Some("live")
        );
    }

    #[tokio::test]
    async fn a_cron_job_that_keeps_faulting_is_quarantined_and_skipped() {
        let job = counter_cron().await;
        for fault in 1..=MAX_FAULTS_BEFORE_QUARANTINE {
            job.supervisor.inject_fault("wasm trap: unreachable").await;
            let rerun = job.run().await;
            if fault < MAX_FAULTS_BEFORE_QUARANTINE {
                assert!(rerun.is_ok(), "fault {fault} is inside the budget: {rerun:?}");
            } else {
                assert!(
                    matches!(rerun, Err(PluginError::Quarantined { .. })),
                    "the fault past the budget must quarantine: {rerun:?}"
                );
            }
        }
        assert!(job.is_quarantined());

        let mut manager = WasmCronManager::new();
        manager.add(job);
        assert_eq!(manager.quarantined_plugins(), vec!["counter-cron".to_string()]);
        let mut last_triggered = HashMap::new();
        manager.run_due_jobs(&mut last_triggered, chrono::Utc::now()).await;
        let skipped = manager
            .adapters()
            .first()
            .and_then(|adapter| adapter.get("skipped_quarantined").and_then(serde_json::Value::as_u64));
        assert_eq!(
            skipped,
            Some(2),
            "the refused manual run and the due occurrence are both counted"
        );
        assert!(
            matches!(
                manager.run_job("counter-cron").await,
                Err(PluginError::Quarantined { .. })
            ),
            "a quarantined job never reports output it did not produce"
        );
    }

    /// Three charged rebuilds, then the fourth fault quarantines.
    const MAX_FAULTS_BEFORE_QUARANTINE: usize = super::super::recovery::MAX_RESTARTS_PER_WINDOW + 1;

    /// D-023: a caller that gives up on a run — a cancelled `cron_run` tool
    /// call — no longer decides whether the run happens. The run was handed
    /// over, so it completes on its own task.
    #[tokio::test]
    async fn an_abandoned_cron_run_still_runs() {
        use std::future::Future;
        use std::task::{Context, Waker};

        let job = counter_cron().await;
        {
            let guard = job.supervisor.lock().await;
            {
                let mut run = std::pin::pin!(job.run());
                assert!(
                    run.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending(),
                    "the run must be waiting for the store"
                );
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
            }
            drop(guard);
        }
        assert_eq!(
            job.run().await.expect("test: the next run"),
            "counter-cron-run-2",
            "the abandoned run must have run first"
        );
    }
}
