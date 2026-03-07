//! WASM Plugin System — P1 Framework.
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
//!
//! # Feature Gate
//!
//! This entire module is behind `#[cfg(feature = "wasm-plugins")]`.
//! Default builds do not include wasmtime.

pub mod error;
pub mod host;
pub mod manifest;
pub mod registry;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wasmtime;

use error::{PluginError, PluginResult};
use manifest::PluginManifest;
use registry::{LoadedPlugin, PluginInfo, PluginRegistry};

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
    ///
    /// `plugins_dir` is the directory containing plugin subdirectories,
    /// each with a `plugin.toml` and `.wasm` file.
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
    /// 2. Validate WASM file exists
    /// 3. Compile the WASM component (validates it's valid WASM)
    /// 4. Register in the plugin registry
    ///
    /// In P1, we parse the manifest and validate the WASM but don't
    /// instantiate (no Tool adapter yet — that's P2).
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

        // Validate WASM file exists
        let wasm_path = plugin_dir.join(&manifest.plugin.wasm);
        if wasm_path.exists() {
            // Compile the component to validate it's valid WASM
            let wasm_bytes = std::fs::read(&wasm_path).map_err(PluginError::Io)?;
            let _component = wasmtime::component::Component::new(&self.engine, &wasm_bytes)
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
        } else {
            tracing::debug!(
                plugin = %plugin_name,
                wasm = %wasm_path.display(),
                "WASM file not found — manifest-only load (no runtime execution)"
            );
        }

        // Register the plugin
        let loaded = LoadedPlugin::new(manifest, plugin_dir.to_path_buf());
        self.registry
            .register(loaded)
            .await
            .map_err(|e| PluginError::AlreadyLoaded { name: e })?;

        tracing::info!(plugin = %plugin_name, "plugin loaded");
        Ok(())
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

    /// Get a reference to the wasmtime engine.
    pub fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }

    /// Get the plugins directory path.
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }
}

// Allow PluginManager to be shared across threads.
// wasmtime::Engine is Send + Sync, PluginRegistry uses Arc<RwLock>.
unsafe impl Send for PluginManager {}
unsafe impl Sync for PluginManager {}

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
