//! WASM Plugin System.
//!
//! Provides `PluginManager` for loading, unloading, and managing WASM plugins
//! using wasmtime with Component Model support.
//!
//! # Architecture
//!
//! - **Engine** (global, shared) — compiles WASM components, caches compilation
//! - **PluginRegistry** — thread-safe map of loaded plugin instances
//! - **HostState** — per-instance state (config, KV, permissions)
//! - **PluginManifest** — parsed `plugin.toml` metadata
//! - **WasmToolAdapter** — bridges WASM tool plugins to PRX `Tool` trait
//!
//! # Feature Gate
//!
//! This entire module is behind `#[cfg(feature = "wasm-plugins")]`.
//! Default builds do not include wasmtime.

pub mod capabilities;
pub mod error;
pub mod host;
pub mod manifest;
pub mod registry;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use error::{PluginError, PluginResult};
use manifest::PluginManifest;
use registry::{LoadedPlugin, PluginInfo, PluginRegistry};

use crate::tools::Tool;

/// Central manager for the WASM plugin system.
///
/// Owns the wasmtime `Engine` (shared across all plugins) and the
/// `PluginRegistry` that tracks loaded instances.
pub struct PluginManager {
    /// Shared wasmtime engine with async + component model support.
    engine: wasmtime::Engine,
    /// Registry of all loaded plugins.
    registry: PluginRegistry,
    /// Base directory where plugin subdirectories live.
    plugins_dir: PathBuf,
}

impl PluginManager {
    /// Create a new `PluginManager`.
    ///
    /// Initializes a wasmtime `Engine` with:
    /// - `async_support(true)` for tokio integration
    /// - `wasm_component_model(true)` for Component Model
    pub fn new(plugins_dir: PathBuf) -> PluginResult<Self> {
        let mut config = wasmtime::Config::new();
        config.async_support(true);
        config.wasm_component_model(true);

        let engine = wasmtime::Engine::new(&config).map_err(|e| {
            PluginError::Compilation(format!("failed to create wasmtime engine: {e}"))
        })?;

        tracing::info!(
            plugins_dir = %plugins_dir.display(),
            "WASM plugin manager initialized"
        );

        Ok(Self {
            engine,
            registry: PluginRegistry::new(),
            plugins_dir,
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

        for entry in entries {
            let entry = entry.map_err(PluginError::Io)?;
            let path = entry.path();

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
        let manifest_path = plugin_dir.join("plugin.toml");
        let manifest = PluginManifest::from_file(&manifest_path)?;
        let plugin_name = manifest.plugin.name.clone();

        // Check for duplicates
        if self.registry.contains(&plugin_name).await {
            return Err(PluginError::AlreadyLoaded {
                name: plugin_name,
            });
        }

        // Compile WASM if file exists
        let wasm_path = plugin_dir.join(&manifest.plugin.wasm);
        let component = if wasm_path.exists() {
            let wasm_bytes = std::fs::read(&wasm_path).map_err(PluginError::Io)?;
            let comp = wasmtime::component::Component::new(&self.engine, &wasm_bytes)
                .map_err(|e| {
                    PluginError::Compilation(format!(
                        "failed to compile '{}': {e}",
                        wasm_path.display()
                    ))
                })?;
            tracing::info!(
                plugin = %plugin_name,
                wasm = %wasm_path.display(),
                "WASM component compiled successfully"
            );
            Some(comp)
        } else {
            tracing::debug!(
                plugin = %plugin_name,
                wasm = %wasm_path.display(),
                "WASM file not found — manifest-only load"
            );
            None
        };

        // Register the plugin
        let loaded = LoadedPlugin::new(manifest, plugin_dir.to_path_buf(), component);
        self.registry
            .register(loaded)
            .await
            .map_err(|e| PluginError::AlreadyLoaded { name: e })?;

        tracing::info!(plugin = %plugin_name, "plugin loaded");
        Ok(())
    }

    /// Reload a plugin by name (unload + load from its original directory).
    pub async fn reload_plugin(&self, name: &str) -> PluginResult<()> {
        let source_dir = self
            .registry
            .get_source_dir(name)
            .await
            .ok_or_else(|| PluginError::NotFound {
                name: name.to_string(),
            })?;

        self.registry.unregister(name).await;
        self.load_plugin(&source_dir).await
    }

    /// Unload a plugin by name.
    pub async fn unload_plugin(&self, name: &str) -> PluginResult<()> {
        if self.registry.unregister(name).await {
            tracing::info!(plugin = %name, "plugin unloaded");
            Ok(())
        } else {
            Err(PluginError::NotFound {
                name: name.to_string(),
            })
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
        self.create_tool_adapters_with_memory(None).await
    }

    /// Create tool adapters for all plugins with tool capabilities,
    /// optionally injecting a memory backend into each adapter's host state.
    pub async fn create_tool_adapters_with_memory(
        &self,
        memory: Option<Arc<dyn crate::memory::traits::Memory>>,
    ) -> Vec<Box<dyn Tool>> {
        let plugins = self.registry.list().await;
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();

        for info in &plugins {
            // Check if this plugin has tool capabilities
            if !info.capabilities.iter().any(|c| c.starts_with("tool:")) {
                continue;
            }

            // Get the manifest to access component
            let manifest = match self.registry.get_manifest(&info.name).await {
                Some(m) => m,
                None => continue,
            };

            // We need the compiled component from the registry
            // For now, re-read and compile if needed
            let source_dir = match self.registry.get_source_dir(&info.name).await {
                Some(d) => d,
                None => continue,
            };

            let wasm_path = source_dir.join(&manifest.plugin.wasm);
            if !wasm_path.exists() {
                tracing::debug!(
                    plugin = %info.name,
                    "skipping tool adapter — no WASM file"
                );
                continue;
            }

            let wasm_bytes = match std::fs::read(&wasm_path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        plugin = %info.name,
                        error = %e,
                        "failed to read WASM file for tool adapter"
                    );
                    continue;
                }
            };

            let component = match wasmtime::component::Component::new(&self.engine, &wasm_bytes) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        plugin = %info.name,
                        error = %e,
                        "failed to compile WASM for tool adapter"
                    );
                    continue;
                }
            };

            match capabilities::tool::WasmToolAdapter::new_with_memory(
                &self.engine,
                &component,
                &manifest,
                memory.clone(),
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
    pub async fn create_middleware_chain(&self) -> capabilities::middleware::MiddlewareChain {
        let plugins = self.registry.list().await;
        let mut chain = capabilities::middleware::MiddlewareChain::new();

        for info in &plugins {
            let middleware_caps: Vec<_> = info
                .capabilities
                .iter()
                .filter(|c| c.starts_with("middleware"))
                .collect();
            if middleware_caps.is_empty() {
                continue;
            }

            let manifest = match self.registry.get_manifest(&info.name).await {
                Some(m) => m,
                None => continue,
            };

            let source_dir = match self.registry.get_source_dir(&info.name).await {
                Some(d) => d,
                None => continue,
            };

            let wasm_path = source_dir.join(&manifest.plugin.wasm);
            let wasm_bytes = match std::fs::read(&wasm_path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to read WASM for middleware");
                    continue;
                }
            };

            let component = match wasmtime::component::Component::new(&self.engine, &wasm_bytes) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to compile WASM for middleware");
                    continue;
                }
            };

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
                priority,
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
    pub async fn create_hook_executor(&self) -> capabilities::hook::WasmHookExecutor {
        let plugins = self.registry.list().await;
        let mut executor = capabilities::hook::WasmHookExecutor::new();

        for info in &plugins {
            let hook_caps: Vec<_> = info
                .capabilities
                .iter()
                .filter(|c| c.starts_with("hook"))
                .collect();
            if hook_caps.is_empty() {
                continue;
            }

            let manifest = match self.registry.get_manifest(&info.name).await {
                Some(m) => m,
                None => continue,
            };

            let source_dir = match self.registry.get_source_dir(&info.name).await {
                Some(d) => d,
                None => continue,
            };

            let wasm_path = source_dir.join(&manifest.plugin.wasm);
            let wasm_bytes = match std::fs::read(&wasm_path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to read WASM for hook");
                    continue;
                }
            };

            let component = match wasmtime::component::Component::new(&self.engine, &wasm_bytes) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to compile WASM for hook");
                    continue;
                }
            };

            let events: std::collections::HashSet<String> = manifest
                .capabilities
                .iter()
                .filter(|c| c.capability_type == "hook")
                .flat_map(|c| c.events.iter().cloned())
                .collect();

            match capabilities::hook::WasmHook::new(&self.engine, &component, &manifest, events)
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
    pub async fn create_cron_manager(&self) -> capabilities::cron::WasmCronManager {
        let plugins = self.registry.list().await;
        let mut manager = capabilities::cron::WasmCronManager::new();

        for info in &plugins {
            let cron_caps: Vec<_> = info
                .capabilities
                .iter()
                .filter(|c| c.starts_with("cron"))
                .collect();
            if cron_caps.is_empty() {
                continue;
            }

            let manifest = match self.registry.get_manifest(&info.name).await {
                Some(m) => m,
                None => continue,
            };

            let source_dir = match self.registry.get_source_dir(&info.name).await {
                Some(d) => d,
                None => continue,
            };

            let wasm_path = source_dir.join(&manifest.plugin.wasm);
            let wasm_bytes = match std::fs::read(&wasm_path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to read WASM for cron");
                    continue;
                }
            };

            let component = match wasmtime::component::Component::new(&self.engine, &wasm_bytes) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(plugin = %info.name, error = %e, "failed to compile WASM for cron");
                    continue;
                }
            };

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
                schedule.clone(),
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

    /// Get a reference to the wasmtime engine.
    pub fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }

    /// Get the plugins directory path.
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }
}

/// Initialize the plugin manager if configured.
///
/// Called during gateway startup. Returns `None` if the plugins directory
/// doesn't exist or no plugins are found (non-fatal).
pub async fn init_plugin_manager(
    workspace_dir: &Path,
) -> Option<Arc<PluginManager>> {
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
