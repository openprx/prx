//! WASM Plugin System.
//!
//! Provides `PluginManager` for loading, unloading, and managing WASM plugins
//! using wasmtime with Component Model support.
//!
//! # Architecture
//!
//! - **Engine** (global, shared) — compiles WASM components, caches compilation
//! - **PrecompileCache** — disk-based cache of native-compiled components
//! - **PluginRegistry** — thread-safe map of loaded plugin instances
//! - **HostState** — per-instance state (config, KV, permissions)
//! - **PluginManifest** — parsed `plugin.toml` metadata
//! - **WasmToolAdapter** — bridges WASM tool plugins to PRX `Tool` trait
//!
//! # Performance
//!
//! Components are compiled once and stored in the registry. Adapter-creation
//! methods reuse the stored (already-compiled) `Component` instead of
//! re-reading and re-compiling the WASM file. Between restarts the
//! `PrecompileCache` persists the native artifact so Cranelift is skipped
//! entirely.
//!
//! # Feature Gate
//!
//! This entire module is behind `#[cfg(feature = "wasm-plugins")]`.
//! Default builds do not include wasmtime.

pub mod capabilities;
pub mod error;
pub mod event_bus;
pub mod host;
pub mod manifest;
pub mod precompile;
pub mod registry;
pub mod runtime;

pub use runtime::{PluginRuntime, init_plugin_runtime};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use error::{PluginError, PluginResult};
use manifest::PluginManifest;
use precompile::PrecompileCache;
use registry::{LoadedPlugin, PluginInfo, PluginRegistry};

use crate::tools::Tool;

const MAX_PLUGIN_WASM_BYTES: u64 = 128 * 1024 * 1024;
const MAX_PLUGIN_COUNT: usize = 256;

/// Aggregated performance metrics for the plugin system.
#[derive(Debug, Default)]
pub struct PluginMetrics {
    /// Total WASM compilation events (cache misses).
    pub compilations: AtomicU64,
    /// Total WASM precompile cache hits.
    pub cache_hits: AtomicU64,
    /// Total WASM precompile cache misses.
    pub cache_misses: AtomicU64,
    /// Cumulative compilation time in milliseconds.
    pub total_compile_ms: AtomicU64,
    /// Total adapter instantiation calls.
    pub total_instantiations: AtomicU64,
}

impl PluginMetrics {
    #[allow(clippy::indexing_slicing)]
    #[cfg(test)]
    fn record_compilation(&self, compile_ms: u64) {
        self.compilations.fetch_add(1, Ordering::Relaxed);
        self.total_compile_ms.fetch_add(compile_ms, Ordering::Relaxed);
    }

    #[allow(clippy::indexing_slicing)]
    #[cfg(test)]
    fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(clippy::indexing_slicing)]
    #[cfg(test)]
    fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    fn record_instantiation(&self) {
        self.total_instantiations.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot current counts as plain values.
    pub fn snapshot(&self) -> PluginMetricsSnapshot {
        PluginMetricsSnapshot {
            compilations: self.compilations.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            total_compile_ms: self.total_compile_ms.load(Ordering::Relaxed),
            total_instantiations: self.total_instantiations.load(Ordering::Relaxed),
        }
    }
}

/// A non-atomic snapshot of `PluginMetrics` suitable for logging / reporting.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginMetricsSnapshot {
    pub compilations: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub total_compile_ms: u64,
    pub total_instantiations: u64,
}

/// Central manager for the WASM plugin system.
///
/// Owns the wasmtime `Engine` (shared across all plugins), the disk-based
/// `PrecompileCache`, and the `PluginRegistry` that tracks loaded instances.
pub struct PluginManager {
    /// Shared wasmtime engine with async + component model support.
    engine: wasmtime::Engine,
    /// Registry of all loaded plugins.
    registry: PluginRegistry,
    /// Base directory where plugin subdirectories live.
    plugins_dir: PathBuf,
    /// Disk-based precompile cache for native WASM artifacts.
    precompile_cache: PrecompileCache,
    /// Runtime performance metrics.
    pub metrics: Arc<PluginMetrics>,
}

impl PluginManager {
    /// Create a new `PluginManager`.
    ///
    /// Initializes a wasmtime `Engine` with:
    /// - `async_support(true)` for tokio integration
    /// - `wasm_component_model(true)` for Component Model
    ///
    /// A `PrecompileCache` is created at `<plugins_dir>/.cwasm-cache/` to
    /// avoid recompiling unchanged plugins on every restart.
    pub fn new(plugins_dir: PathBuf) -> PluginResult<Self> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);
        #[cfg(target_has_atomic = "64")]
        config.epoch_interruption(true);

        let engine = wasmtime::Engine::new(&config)
            .map_err(|e| PluginError::Compilation(format!("failed to create wasmtime engine: {e}")))?;
        spawn_epoch_ticker(&engine);

        let cache_dir = plugins_dir.join(".cwasm-cache");
        let precompile_cache = PrecompileCache::new(cache_dir).map_err(PluginError::Io)?;

        tracing::info!(
            plugins_dir = %plugins_dir.display(),
            "WASM plugin manager initialized"
        );

        Ok(Self {
            engine,
            registry: PluginRegistry::new(),
            plugins_dir,
            precompile_cache,
            metrics: Arc::new(PluginMetrics::default()),
        })
    }

    /// Scan the plugins directory and load all valid plugins.
    ///
    /// Each subdirectory in `plugins_dir` that contains a `plugin.toml`
    /// is treated as a plugin. Errors in individual plugins are logged
    /// but do not prevent other plugins from loading.
    pub async fn load_all(&self) -> PluginResult<usize> {
        if !self.plugins_dir.exists() {
            tracing::debug!(
                path = %self.plugins_dir.display(),
                "plugins directory does not exist, skipping"
            );
            return Ok(0);
        }

        let mut loaded = 0;
        let entries = std::fs::read_dir(&self.plugins_dir).map_err(PluginError::Io)?;

        let mut paths = entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(PluginError::Io)?;
        paths.sort();
        if paths.len() > MAX_PLUGIN_COUNT {
            return Err(PluginError::ResourceLimit(format!(
                "plugins directory contains {} entries; limit is {MAX_PLUGIN_COUNT}",
                paths.len()
            )));
        }

        for path in paths {
            if !path.is_dir() {
                continue;
            }

            let manifest_path = path.join("plugin.toml");
            if !manifest_path.exists() {
                continue;
            }

            match self.load_plugin(&path).await {
                Ok(()) => {
                    loaded += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        plugin_dir = %path.display(),
                        error = %e,
                        "failed to load plugin, skipping"
                    );
                }
            }
        }

        tracing::info!(count = loaded, "plugins loaded");
        Ok(loaded)
    }

    /// Load a single plugin from its directory.
    ///
    /// Steps:
    /// 1. Parse `plugin.toml` manifest
    /// 2. Compile the WASM component (if present)
    /// 3. Register in the plugin registry
    pub async fn load_plugin(&self, plugin_dir: &Path) -> PluginResult<()> {
        let loaded = self.prepare_plugin(plugin_dir)?;
        let plugin_name = loaded.manifest.plugin.name.clone();

        // Check for duplicates
        if self.registry.contains(&plugin_name).await {
            return Err(PluginError::AlreadyLoaded { name: plugin_name });
        }

        // Register the plugin
        self.registry
            .register(loaded)
            .await
            .map_err(|e| PluginError::AlreadyLoaded { name: e })?;

        tracing::info!(plugin = %plugin_name, "plugin loaded");
        Ok(())
    }

    /// Reload a plugin by name. Preparation completes before one registry replace,
    /// so a failed candidate never creates an unload gap.
    pub async fn reload_plugin(&self, name: &str) -> PluginResult<()> {
        let source_dir = self
            .registry
            .get_source_dir(name)
            .await
            .ok_or_else(|| PluginError::NotFound { name: name.to_string() })?;

        let loaded = self.prepare_plugin(&source_dir)?;
        if loaded.manifest.plugin.name != name {
            return Err(PluginError::Manifest(format!(
                "reload manifest renamed plugin '{name}' to '{}'",
                loaded.manifest.plugin.name
            )));
        }
        self.registry.replace(loaded).await;
        Ok(())
    }

    fn prepare_plugin(&self, plugin_dir: &Path) -> PluginResult<LoadedPlugin> {
        let manifest_path = plugin_dir.join("plugin.toml");
        let manifest = PluginManifest::from_file(&manifest_path)?;
        let plugin_name = manifest.plugin.name.clone();
        let wasm_relative = Path::new(&manifest.plugin.wasm);
        if wasm_relative.is_absolute()
            || wasm_relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(PluginError::Manifest(format!(
                "plugin '{plugin_name}' wasm path must stay within its plugin directory"
            )));
        }

        let wasm_path = plugin_dir.join(wasm_relative);
        let component = if wasm_path.exists() {
            let metadata = std::fs::symlink_metadata(&wasm_path).map_err(PluginError::Io)?;
            if metadata.file_type().is_symlink() {
                return Err(PluginError::Manifest(format!(
                    "plugin '{plugin_name}' wasm file must not be a symlink"
                )));
            }
            if metadata.len() > MAX_PLUGIN_WASM_BYTES {
                return Err(PluginError::ResourceLimit(format!(
                    "plugin '{plugin_name}' WASM exceeds {MAX_PLUGIN_WASM_BYTES} bytes"
                )));
            }
            let wasm_bytes = std::fs::read(&wasm_path).map_err(PluginError::Io)?;
            if wasm_bytes.len() as u64 > MAX_PLUGIN_WASM_BYTES {
                return Err(PluginError::ResourceLimit(format!(
                    "plugin '{plugin_name}' WASM exceeds {MAX_PLUGIN_WASM_BYTES} bytes"
                )));
            }
            let compiled = self
                .precompile_cache
                .get_or_compile(&self.engine, &wasm_bytes)
                .map_err(|error| {
                    PluginError::Compilation(format!("failed to compile '{}': {error}", wasm_path.display()))
                })?;
            self.update_cache_metrics();
            tracing::info!(plugin = %plugin_name, wasm = %wasm_path.display(), "WASM component ready");
            Some(compiled)
        } else {
            tracing::debug!(plugin = %plugin_name, wasm = %wasm_path.display(), "WASM file not found — manifest-only load");
            None
        };

        Ok(LoadedPlugin::new(manifest, plugin_dir.to_path_buf(), component))
    }

    fn update_cache_metrics(&self) {
        let cache = &self.precompile_cache.metrics;
        let misses = cache.misses();
        self.metrics.cache_hits.store(cache.hits(), Ordering::Relaxed);
        self.metrics.cache_misses.store(misses, Ordering::Relaxed);
        self.metrics.compilations.store(misses, Ordering::Relaxed);
        self.metrics
            .total_compile_ms
            .store(cache.total_compile_ms(), Ordering::Relaxed);
    }

    /// Unload a plugin by name.
    pub async fn unload_plugin(&self, name: &str) -> PluginResult<()> {
        if self.registry.unregister(name).await {
            tracing::info!(plugin = %name, "plugin unloaded");
            Ok(())
        } else {
            Err(PluginError::NotFound { name: name.to_string() })
        }
    }

    /// List all loaded plugins.
    pub async fn list_plugins(&self) -> Vec<PluginInfo> {
        self.registry.list().await
    }

    /// Get info about a specific plugin.
    pub async fn get_plugin(&self, name: &str) -> Option<PluginInfo> {
        self.registry.get_info(name).await
    }

    /// Create WasmToolAdapter instances for all plugins that declare tool capabilities.
    ///
    /// Returns a list of boxed `Tool` trait objects ready for registration
    /// in the tools_registry.
    pub async fn create_tool_adapters(&self) -> Vec<Box<dyn Tool>> {
        self.create_tool_adapters_with_memory(None, None).await
    }

    /// Create tool adapters for all plugins with tool capabilities,
    /// optionally injecting a memory backend and/or event bus into each adapter's host state.
    pub async fn create_tool_adapters_with_memory(
        &self,
        memory: Option<Arc<dyn crate::memory::traits::Memory>>,
        event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> Vec<Box<dyn Tool>> {
        let plugins = self.registry.list().await;
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();

        for info in &plugins {
            if !matches!(info.status, registry::PluginStatus::Active) {
                continue;
            }
            // Check if this plugin has tool capabilities
            if !info.capabilities.iter().any(|c| c.starts_with("tool:")) {
                continue;
            }

            // Get the manifest, component, and policy-filtered permissions from the registry.
            let manifest = match self.registry.get_manifest(&info.name).await {
                Some(m) => m,
                None => continue,
            };

            let component = match self.registry.get_component(&info.name).await {
                Some(c) => c,
                None => {
                    tracing::debug!(plugin = %info.name, "skipping tool adapter — no WASM component");
                    continue;
                }
            };

            let granted_permissions: std::collections::HashSet<String> = self
                .registry
                .get_granted_permissions(&info.name)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();

            self.metrics.record_instantiation();

            match capabilities::tool::WasmToolAdapter::new_with_memory(
                &self.engine,
                &component,
                &manifest,
                granted_permissions,
                memory.clone(),
                event_bus.clone(),
            )
            .await
            {
                Ok(adapter) => {
                    tracing::info!(
                        plugin = %info.name,
                        tool = %adapter.name(),
                        "WASM tool adapter created"
                    );
                    tools.push(Box::new(adapter));
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = %info.name,
                        error = %e,
                        "failed to create tool adapter"
                    );
                }
            }
        }

        tools
    }

    /// Create middleware adapters for all plugins with middleware capabilities.
    pub async fn create_middleware_chain(
        &self,
        event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> capabilities::middleware::MiddlewareChain {
        let plugins = self.registry.list().await;
        let mut chain = capabilities::middleware::MiddlewareChain::new();

        for info in &plugins {
            if !matches!(info.status, registry::PluginStatus::Active) {
                continue;
            }
            if !info.capabilities.iter().any(|c| c.starts_with("middleware")) {
                continue;
            }

            let manifest = match self.registry.get_manifest(&info.name).await {
                Some(m) => m,
                None => continue,
            };

            let component = match self.registry.get_component(&info.name).await {
                Some(c) => c,
                None => {
                    tracing::debug!(plugin = %info.name, "skipping middleware adapter — no WASM component");
                    continue;
                }
            };

            let granted_permissions: std::collections::HashSet<String> = self
                .registry
                .get_granted_permissions(&info.name)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();

            self.metrics.record_instantiation();

            let priority = manifest
                .capabilities
                .iter()
                .find(|c| c.capability_type == "middleware")
                .map(|c| c.priority)
                .unwrap_or(100);

            match capabilities::middleware::WasmMiddleware::new(
                &self.engine,
                &component,
                &manifest,
                granted_permissions,
                priority,
                event_bus.clone(),
            )
            .await
            {
                Ok(mw) => {
                    tracing::info!(
                        plugin = %info.name,
                        priority,
                        "WASM middleware adapter created"
                    );
                    chain.add(mw);
                }
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to create middleware adapter");
                }
            }
        }

        chain
    }

    /// Create hook adapters for all plugins with hook capabilities.
    pub async fn create_hook_executor(
        &self,
        event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> capabilities::hook::WasmHookExecutor {
        let plugins = self.registry.list().await;
        let mut executor = capabilities::hook::WasmHookExecutor::new();

        for info in &plugins {
            if !matches!(info.status, registry::PluginStatus::Active) {
                continue;
            }
            if !info.capabilities.iter().any(|c| c.starts_with("hook")) {
                continue;
            }

            let manifest = match self.registry.get_manifest(&info.name).await {
                Some(m) => m,
                None => continue,
            };

            let component = match self.registry.get_component(&info.name).await {
                Some(c) => c,
                None => {
                    tracing::debug!(plugin = %info.name, "skipping hook adapter — no WASM component");
                    continue;
                }
            };

            let granted_permissions: std::collections::HashSet<String> = self
                .registry
                .get_granted_permissions(&info.name)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();

            self.metrics.record_instantiation();

            let events: std::collections::HashSet<String> = manifest
                .capabilities
                .iter()
                .filter(|c| c.capability_type == "hook")
                .flat_map(|c| c.events.iter().cloned())
                .collect();

            match capabilities::hook::WasmHook::new(
                &self.engine,
                &component,
                &manifest,
                granted_permissions,
                events,
                event_bus.clone(),
            )
            .await
            {
                Ok(hook) => {
                    tracing::info!(plugin = %info.name, "WASM hook adapter created");
                    executor.add(hook);
                }
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to create hook adapter");
                }
            }
        }

        executor
    }

    /// Create cron adapters for all plugins with cron capabilities.
    pub async fn create_cron_manager(
        &self,
        event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> capabilities::cron::WasmCronManager {
        let plugins = self.registry.list().await;
        let mut manager = capabilities::cron::WasmCronManager::new();

        for info in &plugins {
            if !matches!(info.status, registry::PluginStatus::Active) {
                continue;
            }
            if !info.capabilities.iter().any(|c| c.starts_with("cron")) {
                continue;
            }

            let manifest = match self.registry.get_manifest(&info.name).await {
                Some(m) => m,
                None => continue,
            };

            let component = match self.registry.get_component(&info.name).await {
                Some(c) => c,
                None => {
                    tracing::debug!(plugin = %info.name, "skipping cron adapter — no WASM component");
                    continue;
                }
            };

            let granted_permissions: std::collections::HashSet<String> = self
                .registry
                .get_granted_permissions(&info.name)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();

            self.metrics.record_instantiation();

            let schedule = manifest
                .capabilities
                .iter()
                .find(|c| c.capability_type == "cron")
                .and_then(|c| c.schedule.clone())
                .unwrap_or_default();

            if schedule.is_empty() {
                tracing::warn!(plugin = %info.name, "cron plugin has no schedule, skipping");
                continue;
            }

            match capabilities::cron::WasmCronJob::new(
                &self.engine,
                &component,
                &manifest,
                granted_permissions,
                schedule.clone(),
                event_bus.clone(),
            )
            .await
            {
                Ok(job) => {
                    tracing::info!(plugin = %info.name, schedule = %schedule, "WASM cron job created");
                    manager.add(job);
                }
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to create cron adapter");
                }
            }
        }

        manager
    }

    /// Create provider adapters for all plugins with provider capabilities.
    ///
    /// Returns a list of `WasmProvider` instances, each implementing the
    /// `Provider` trait and ready to handle LLM routing requests.
    pub async fn create_provider_adapters(
        &self,
        event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> Vec<capabilities::provider::WasmProvider> {
        let plugins = self.registry.list().await;
        let mut providers = Vec::new();

        for info in &plugins {
            if !matches!(info.status, registry::PluginStatus::Active) {
                continue;
            }
            if !info.capabilities.iter().any(|c| c.starts_with("provider")) {
                continue;
            }

            let manifest = match self.registry.get_manifest(&info.name).await {
                Some(m) => m,
                None => continue,
            };

            let component = match self.registry.get_component(&info.name).await {
                Some(c) => c,
                None => {
                    tracing::debug!(plugin = %info.name, "skipping provider adapter — no WASM component");
                    continue;
                }
            };

            let granted_permissions: std::collections::HashSet<String> = self
                .registry
                .get_granted_permissions(&info.name)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();

            self.metrics.record_instantiation();

            match capabilities::provider::WasmProvider::new(
                &self.engine,
                &component,
                &manifest,
                granted_permissions,
                event_bus.clone(),
            )
            .await
            {
                Ok(provider) => {
                    tracing::info!(
                        plugin = %info.name,
                        provider = %provider.provider_name(),
                        "WASM provider adapter created"
                    );
                    providers.push(provider);
                }
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to create provider adapter");
                }
            }
        }

        providers
    }

    /// Create storage adapters for all plugins with storage capabilities.
    ///
    /// Returns a list of `WasmStorage` instances, each implementing the
    /// `Memory` trait and ready to serve as a custom memory backend.
    pub async fn create_storage_adapters(
        &self,
        event_bus: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> Vec<capabilities::storage::WasmStorage> {
        let plugins = self.registry.list().await;
        let mut storages = Vec::new();

        for info in &plugins {
            if !matches!(info.status, registry::PluginStatus::Active) {
                continue;
            }
            if !info.capabilities.iter().any(|c| c.starts_with("storage")) {
                continue;
            }

            let manifest = match self.registry.get_manifest(&info.name).await {
                Some(m) => m,
                None => continue,
            };

            let component = match self.registry.get_component(&info.name).await {
                Some(c) => c,
                None => {
                    tracing::debug!(plugin = %info.name, "skipping storage adapter — no WASM component");
                    continue;
                }
            };

            let granted_permissions: std::collections::HashSet<String> = self
                .registry
                .get_granted_permissions(&info.name)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();

            self.metrics.record_instantiation();

            match capabilities::storage::WasmStorage::new(
                &self.engine,
                &component,
                &manifest,
                granted_permissions,
                event_bus.clone(),
            )
            .await
            {
                Ok(storage) => {
                    tracing::info!(
                        plugin = %info.name,
                        storage = %storage.storage_name(),
                        "WASM storage adapter created"
                    );
                    storages.push(storage);
                }
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to create storage adapter");
                }
            }
        }

        storages
    }

    /// Get a reference to the wasmtime engine.
    pub const fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }

    /// Get the plugins directory path.
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }

    /// Get a reference to the precompile cache.
    pub const fn precompile_cache(&self) -> &PrecompileCache {
        &self.precompile_cache
    }

    /// Snapshot of current performance metrics.
    pub fn metrics_snapshot(&self) -> PluginMetricsSnapshot {
        self.metrics.snapshot()
    }
}

#[cfg(target_has_atomic = "64")]
fn spawn_epoch_ticker(engine: &wasmtime::Engine) {
    let weak = engine.weak();
    let tick = Duration::from_millis(host::WASM_EPOCH_TICK_MS);
    let _ = std::thread::Builder::new()
        .name("prx-wasm-epoch-ticker".to_string())
        .spawn(move || {
            while let Some(engine) = weak.upgrade() {
                std::thread::sleep(tick);
                engine.increment_epoch();
            }
        });
}

#[cfg(not(target_has_atomic = "64"))]
fn spawn_epoch_ticker(_engine: &wasmtime::Engine) {}

/// Initialize the plugin manager if configured.
///
/// Called during gateway startup. Returns `None` if the plugins directory
/// doesn't exist or no plugins are found (non-fatal).
pub async fn init_plugin_manager(workspace_dir: &Path) -> Option<Arc<PluginManager>> {
    let plugins_dir = workspace_dir.join("plugins");

    // Create the plugins directory if it doesn't exist
    if !plugins_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&plugins_dir) {
            tracing::warn!(error = %e, "failed to create plugins directory");
            return None;
        }
    }

    match PluginManager::new(plugins_dir) {
        Ok(manager) => {
            let manager = Arc::new(manager);
            match manager.load_all().await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(count, "WASM plugin system ready");
                    } else {
                        tracing::debug!("WASM plugin system ready (no plugins found)");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load plugins");
                }
            }
            Some(manager)
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to initialize WASM plugin manager");
            None
        }
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    // ── PluginMetrics ───────────────────────────────────────────

    #[test]
    fn metrics_default_all_zero() {
        let m = PluginMetrics::default();
        assert_eq!(m.compilations.load(Ordering::Relaxed), 0);
        assert_eq!(m.cache_hits.load(Ordering::Relaxed), 0);
        assert_eq!(m.cache_misses.load(Ordering::Relaxed), 0);
        assert_eq!(m.total_compile_ms.load(Ordering::Relaxed), 0);
        assert_eq!(m.total_instantiations.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn metrics_record_compilation() {
        let m = PluginMetrics::default();
        m.record_compilation(42);
        assert_eq!(m.compilations.load(Ordering::Relaxed), 1);
        assert_eq!(m.total_compile_ms.load(Ordering::Relaxed), 42);
        m.record_compilation(8);
        assert_eq!(m.compilations.load(Ordering::Relaxed), 2);
        assert_eq!(m.total_compile_ms.load(Ordering::Relaxed), 50);
    }

    #[test]
    fn metrics_record_cache_hit_miss() {
        let m = PluginMetrics::default();
        m.record_cache_hit();
        m.record_cache_hit();
        m.record_cache_miss();
        assert_eq!(m.cache_hits.load(Ordering::Relaxed), 2);
        assert_eq!(m.cache_misses.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn metrics_record_instantiation() {
        let m = PluginMetrics::default();
        m.record_instantiation();
        m.record_instantiation();
        m.record_instantiation();
        assert_eq!(m.total_instantiations.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn metrics_snapshot_captures_current_state() {
        let m = PluginMetrics::default();
        m.record_compilation(100);
        m.record_cache_hit();
        m.record_cache_miss();
        m.record_instantiation();

        let snap = m.snapshot();
        assert_eq!(snap.compilations, 1);
        assert_eq!(snap.cache_hits, 1);
        assert_eq!(snap.cache_misses, 1);
        assert_eq!(snap.total_compile_ms, 100);
        assert_eq!(snap.total_instantiations, 1);
    }

    #[test]
    fn metrics_snapshot_serializes_to_json() {
        let m = PluginMetrics::default();
        m.record_compilation(55);
        let snap = m.snapshot();
        let json = serde_json::to_value(&snap).expect("test: serialize snapshot");
        assert_eq!(json["compilations"], 1);
        assert_eq!(json["total_compile_ms"], 55);
    }

    // ── PluginManager construction ──────────────────────────────

    #[test]
    fn manager_new_creates_engine() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf());
        assert!(manager.is_ok());
    }

    #[test]
    fn manager_plugins_dir_matches() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let path = tmp.path().to_path_buf();
        let manager = PluginManager::new(path.clone()).expect("test: new");
        assert_eq!(manager.plugins_dir(), path);
    }

    #[test]
    fn manager_initial_metrics_are_zero() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let snap = manager.metrics_snapshot();
        assert_eq!(snap.compilations, 0);
        assert_eq!(snap.cache_hits, 0);
        assert_eq!(snap.total_instantiations, 0);
    }

    // ── PluginManager async operations ──────────────────────────

    #[tokio::test]
    async fn load_all_empty_dir_returns_zero() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let count = manager.load_all().await.expect("test: load_all");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn load_all_missing_dir_returns_zero() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let nonexistent = tmp.path().join("does_not_exist");
        let manager = PluginManager::new(nonexistent).expect("test: new");
        let count = manager.load_all().await.expect("test: load_all");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn load_all_skips_files_not_dirs() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        // Create a plain file (not a plugin dir)
        std::fs::write(tmp.path().join("not_a_plugin.txt"), "hello").expect("test: write");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let count = manager.load_all().await.expect("test: load_all");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn load_all_skips_dirs_without_manifest() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        std::fs::create_dir(tmp.path().join("my_plugin")).expect("test: mkdir");
        // No plugin.toml → skipped
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let count = manager.load_all().await.expect("test: load_all");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn list_plugins_initially_empty() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let plugins = manager.list_plugins().await;
        assert!(plugins.is_empty());
    }

    #[tokio::test]
    async fn get_plugin_nonexistent_returns_none() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        assert!(manager.get_plugin("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn unload_nonexistent_returns_error() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let result = manager.unload_plugin("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reload_nonexistent_returns_error() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let result = manager.reload_plugin("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reload_prepares_before_atomic_registry_replace() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let plugin_dir = tmp.path().join("atomic");
        std::fs::create_dir(&plugin_dir).unwrap();
        let manifest =
            |version: &str| format!("[plugin]\nname = \"atomic\"\nversion = \"{version}\"\nwasm = \"missing.wasm\"\n");
        std::fs::write(plugin_dir.join("plugin.toml"), manifest("1.0.0")).unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).unwrap();
        manager.load_plugin(&plugin_dir).await.unwrap();

        std::fs::write(plugin_dir.join("plugin.toml"), manifest("2.0.0")).unwrap();
        manager.reload_plugin("atomic").await.unwrap();
        assert_eq!(manager.get_plugin("atomic").await.unwrap().version, "2.0.0");

        std::fs::write(plugin_dir.join("plugin.toml"), "invalid = [").unwrap();
        assert!(manager.reload_plugin("atomic").await.is_err());
        assert_eq!(manager.get_plugin("atomic").await.unwrap().version, "2.0.0");
    }

    #[tokio::test]
    async fn load_rejects_wasm_path_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("escape");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.toml"),
            "[plugin]\nname = \"escape\"\nversion = \"1.0.0\"\nwasm = \"../outside.wasm\"\n",
        )
        .unwrap();
        let manager = PluginManager::new(tmp.path().to_path_buf()).unwrap();
        let error = manager.load_plugin(&plugin_dir).await.unwrap_err();
        assert!(error.to_string().contains("stay within"));
    }

    // ── init_plugin_manager ─────────────────────────────────────

    #[tokio::test]
    async fn init_plugin_manager_creates_plugins_dir() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let workspace = tmp.path();
        let plugins_dir = workspace.join("plugins");
        assert!(!plugins_dir.exists());

        let manager = init_plugin_manager(workspace).await;
        assert!(manager.is_some());
        assert!(plugins_dir.exists());
    }

    #[tokio::test]
    async fn init_plugin_manager_empty_workspace_succeeds() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = init_plugin_manager(tmp.path()).await;
        assert!(manager.is_some());
        let plugins = manager.as_ref().expect("test: manager").list_plugins().await;
        assert!(plugins.is_empty());
    }

    // ── create_tool_adapters on empty registry ──────────────────

    #[tokio::test]
    async fn create_tool_adapters_empty_returns_empty() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let tools = manager.create_tool_adapters().await;
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn create_middleware_chain_empty_returns_empty_chain() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let chain = manager.create_middleware_chain(None).await;
        assert!(chain.is_empty());
    }

    #[tokio::test]
    async fn create_provider_adapters_empty() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let providers = manager.create_provider_adapters(None).await;
        assert!(providers.is_empty());
    }

    #[tokio::test]
    async fn create_storage_adapters_empty() {
        let tmp = tempfile::tempdir().expect("test: tempdir");
        let manager = PluginManager::new(tmp.path().to_path_buf()).expect("test: new");
        let storages = manager.create_storage_adapters(None).await;
        assert!(storages.is_empty());
    }
}
