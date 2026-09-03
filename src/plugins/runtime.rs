//! Process-level owner for one workspace's live WASM plugin generation.
//!
//! A generation contains the registry and every adapter derived from it. Reload
//! builds a complete candidate off to the side and publishes it with one ArcSwap;
//! callers therefore observe either the old generation or the new one, never a mix.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::PluginManager;
use super::capabilities::cron::WasmCronManager;
use super::capabilities::hook::WasmHookExecutor;
use super::capabilities::middleware::MiddlewareChain;
use super::capabilities::provider::WasmProvider;
use super::capabilities::storage::WasmStorage;
use super::error::{PluginError, PluginResult};
use super::event_bus::EventBus;
use super::manifest::PluginManifest;
use super::registry::{PluginInfo, PluginStatus};
use crate::memory::traits::Memory;
use crate::security::SecurityPolicy;
use crate::security::op_id;
use crate::security::policy::{ApprovalGrant, ResourceRiskLevel, SideEffectGate};
use crate::tools::{Tool, ToolCategory, ToolResult, ToolSpec, ToolTier};

const WASM_MAX_ALIAS_SPECS: usize = 32;

struct PluginGeneration {
    id: u64,
    manager: Arc<PluginManager>,
    tools: Vec<Arc<dyn Tool>>,
    middleware: Arc<MiddlewareChain>,
    hooks: Arc<WasmHookExecutor>,
    cron: Arc<WasmCronManager>,
    providers: HashMap<String, Arc<WasmProvider>>,
    storages: HashMap<String, Arc<WasmStorage>>,
    errors: Vec<String>,
}

impl PluginGeneration {
    async fn build(
        id: u64,
        plugins_dir: PathBuf,
        memory: Option<Arc<dyn Memory>>,
        event_bus: Arc<EventBus>,
    ) -> PluginResult<Self> {
        let manager = Arc::new(PluginManager::new(plugins_dir)?);
        manager.load_all().await?;

        let tools_build = manager
            .create_tool_adapters_with_memory(memory.clone(), Some(Arc::clone(&event_bus)))
            .await;
        let middleware_build = manager
            .create_middleware_chain_with_memory(memory.clone(), Some(Arc::clone(&event_bus)))
            .await;
        let hooks_build = manager
            .create_hook_executor_with_memory(memory.clone(), Some(Arc::clone(&event_bus)))
            .await;
        let cron_build = manager
            .create_cron_manager_with_memory(memory, Some(Arc::clone(&event_bus)))
            .await;
        let provider_build = manager.create_provider_adapters(Some(Arc::clone(&event_bus))).await;
        let storage_build = manager.create_storage_adapters(Some(event_bus)).await;
        let tools = tools_build.value.into_iter().map(Arc::<dyn Tool>::from).collect();
        let middleware = Arc::new(middleware_build.value);
        let hooks = Arc::new(hooks_build.value);
        let cron = Arc::new(cron_build.value);
        let mut errors = tools_build
            .errors
            .into_iter()
            .chain(middleware_build.errors)
            .chain(hooks_build.errors)
            .chain(cron_build.errors)
            .chain(provider_build.errors)
            .chain(storage_build.errors)
            .collect::<Vec<_>>();
        let mut providers = HashMap::new();
        for provider in provider_build.value {
            let name = provider.provider_name().to_string();
            if providers.insert(name.clone(), Arc::new(provider)).is_some() {
                errors.push(format!("duplicate WASM provider export '{name}'"));
            }
        }
        let mut storages = HashMap::new();
        for storage in storage_build.value {
            let name = storage.storage_name().to_string();
            if storages.insert(name.clone(), Arc::new(storage)).is_some() {
                errors.push(format!("duplicate WASM storage export '{name}'"));
            }
        }

        Ok(Self {
            id,
            manager,
            tools,
            middleware,
            hooks,
            cron,
            providers,
            storages,
            errors,
        })
    }
}

/// Sole process-level owner of a workspace's plugin generation and event bus.
pub struct PluginRuntime {
    plugins_dir: PathBuf,
    memory: parking_lot::RwLock<Option<Arc<dyn Memory>>>,
    event_bus: Arc<EventBus>,
    generation: ArcSwap<PluginGeneration>,
    reload_lock: Mutex<()>,
}

impl PluginRuntime {
    async fn new(workspace_dir: &Path, memory: Option<Arc<dyn Memory>>) -> PluginResult<Arc<Self>> {
        let plugins_dir = workspace_dir.join("plugins");
        std::fs::create_dir_all(&plugins_dir).map_err(PluginError::Io)?;
        let event_bus = Arc::new(EventBus::new());
        let generation =
            PluginGeneration::build(1, plugins_dir.clone(), memory.clone(), Arc::clone(&event_bus)).await?;
        let runtime = Arc::new(Self {
            plugins_dir,
            memory: parking_lot::RwLock::new(memory),
            event_bus,
            generation: ArcSwap::from_pointee(generation),
            reload_lock: Mutex::new(()),
        });
        Self::spawn_cron_scheduler(&runtime);
        Ok(runtime)
    }

    /// Current atomically published generation number.
    pub fn generation_id(&self) -> u64 {
        self.generation.load().id
    }

    /// Stable event bus shared by every generation for this workspace.
    pub fn event_bus(&self) -> Arc<EventBus> {
        Arc::clone(&self.event_bus)
    }

    /// Per-plugin adapter failures isolated in the current generation.
    pub fn adapter_errors(&self) -> Vec<String> {
        self.generation.load().errors.clone()
    }

    /// List plugins from the current generation.
    pub async fn list_plugins(&self) -> Vec<PluginInfo> {
        let generation = self.generation.load_full();
        let mut plugins = generation.manager.list_plugins().await;
        plugins.sort_by(|left, right| left.name.cmp(&right.name));
        plugins
    }

    /// Build a complete replacement generation, verify the requested plugin is
    /// still present, then publish all registries/adapters with one swap.
    pub async fn reload_plugin(&self, name: &str) -> PluginResult<u64> {
        let _reload_guard = self.reload_lock.lock().await;
        let old = self.generation.load_full();
        if old.manager.get_plugin(name).await.is_none() {
            return Err(PluginError::NotFound { name: name.to_string() });
        }

        let next_id = old.id.saturating_add(1);
        let memory = self.memory.read().clone();
        let candidate =
            PluginGeneration::build(next_id, self.plugins_dir.clone(), memory, Arc::clone(&self.event_bus)).await?;
        if candidate.manager.get_plugin(name).await.is_none() {
            return Err(PluginError::Runtime(format!(
                "reload candidate did not contain plugin '{name}'"
            )));
        }

        self.generation.store(Arc::new(candidate));
        tracing::info!(plugin = %name, generation = next_id, "plugin generation atomically reloaded");
        Ok(next_id)
    }

    /// Dispatch a lifecycle event through the current generation's hook adapters.
    pub async fn emit_hook(&self, event: &str, payload_json: &str) {
        let generation = self.generation.load_full();
        generation.hooks.emit(event, payload_json).await;
    }

    pub fn hook_diagnostics(&self) -> Vec<super::capabilities::hook::WasmHookDiagnostics> {
        self.generation.load().hooks.diagnostics()
    }

    pub fn middleware_diagnostics(&self) -> Vec<serde_json::Value> {
        self.generation.load().middleware.adapters()
    }

    pub fn cron_diagnostics(&self) -> Vec<serde_json::Value> {
        self.generation.load().cron.adapters()
    }

    /// Snapshot the current middleware generation for one request pipeline.
    pub fn middleware(&self) -> Arc<MiddlewareChain> {
        Arc::clone(&self.generation.load().middleware)
    }

    /// Snapshot the current cron generation for scheduler integration.
    pub fn cron(&self) -> Arc<WasmCronManager> {
        Arc::clone(&self.generation.load().cron)
    }

    /// Names exported by provider adapters in the current generation.
    pub fn provider_names(&self) -> Vec<String> {
        let mut names = self.generation.load().providers.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Names exported by storage adapters in the current generation.
    pub fn storage_names(&self) -> Vec<String> {
        let mut names = self.generation.load().storages.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Return a reload-safe provider proxy for an exported provider name.
    pub fn provider(self: &Arc<Self>, name: &str) -> Option<Box<dyn crate::providers::traits::Provider>> {
        let name = strip_wasm_prefix(name);
        self.generation.load().providers.contains_key(name).then(|| {
            Box::new(RuntimeWasmProvider {
                runtime: Arc::clone(self),
                name: name.to_string(),
            }) as Box<dyn crate::providers::traits::Provider>
        })
    }

    /// Return a reload-safe memory proxy for an exported storage name.
    pub fn storage(self: &Arc<Self>, name: &str) -> Option<Arc<dyn Memory>> {
        let name = strip_wasm_prefix(name);
        self.generation.load().storages.contains_key(name).then(|| {
            Arc::new(RuntimeWasmStorage {
                runtime: Arc::clone(self),
                name: name.to_string(),
            }) as Arc<dyn Memory>
        })
    }

    /// A stable multi-spec tool that resolves every call against one current generation.
    pub fn tool_router(self: &Arc<Self>) -> Box<dyn Tool> {
        Box::new(PluginToolRouter {
            runtime: Arc::clone(self),
        })
    }

    pub fn status_tool(self: &Arc<Self>) -> Box<dyn Tool> {
        Box::new(PluginStatusTool {
            runtime: Arc::clone(self),
        })
    }

    pub fn reload_tool(self: &Arc<Self>, security: Arc<SecurityPolicy>) -> Box<dyn Tool> {
        Box::new(PluginReloadTool {
            runtime: Arc::clone(self),
            security,
        })
    }

    pub fn manage_tool(self: &Arc<Self>, security: Arc<SecurityPolicy>) -> Box<dyn Tool> {
        Box::new(PluginManageTool {
            runtime: Arc::clone(self),
            security,
        })
    }

    /// Atomically rebuild every plugin adapter from the filesystem.
    pub async fn refresh_all(&self) -> PluginResult<u64> {
        let _reload_guard = self.reload_lock.lock().await;
        let old = self.generation.load_full();
        let next_id = old.id.saturating_add(1);
        let memory = self.memory.read().clone();
        let candidate =
            PluginGeneration::build(next_id, self.plugins_dir.clone(), memory, Arc::clone(&self.event_bus)).await?;
        self.generation.store(Arc::new(candidate));
        tracing::info!(generation = next_id, "plugin generation atomically refreshed");
        Ok(next_id)
    }

    /// Attach or replace the host memory backend and atomically rebuild every
    /// adapter. This supports the two-phase bootstrap required when the memory
    /// backend itself is supplied by a WASM storage plugin.
    pub async fn attach_memory(&self, memory: Arc<dyn Memory>) -> PluginResult<u64> {
        let _reload_guard = self.reload_lock.lock().await;
        let old = self.generation.load_full();
        let next_id = old.id.saturating_add(1);
        let candidate = PluginGeneration::build(
            next_id,
            self.plugins_dir.clone(),
            Some(Arc::clone(&memory)),
            Arc::clone(&self.event_bus),
        )
        .await?;
        *self.memory.write() = Some(memory);
        self.generation.store(Arc::new(candidate));
        tracing::info!(generation = next_id, "plugin generation attached live memory backend");
        Ok(next_id)
    }

    fn spawn_cron_scheduler(runtime: &Arc<Self>) {
        let runtime = Arc::downgrade(runtime);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_triggered = HashMap::new();
            loop {
                ticker.tick().await;
                let Some(runtime) = runtime.upgrade() else {
                    break;
                };
                let generation = runtime.generation.load_full();
                generation
                    .cron
                    .run_due_jobs(&mut last_triggered, chrono::Utc::now())
                    .await;
            }
        });
    }
}

fn strip_wasm_prefix(name: &str) -> &str {
    name.strip_prefix("wasm:").unwrap_or(name)
}

struct RuntimeWasmProvider {
    runtime: Arc<PluginRuntime>,
    name: String,
}

impl RuntimeWasmProvider {
    fn current(&self) -> anyhow::Result<Arc<WasmProvider>> {
        self.runtime
            .generation
            .load()
            .providers
            .get(&self.name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("WASM provider '{}' is unavailable in the current generation", self.name))
    }
}

#[async_trait]
impl crate::providers::traits::Provider for RuntimeWasmProvider {
    fn supports_native_tools(&self) -> bool {
        true
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.current()?
            .chat_with_system(system_prompt, message, model, temperature)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[crate::providers::traits::ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.current()?.chat_with_history(messages, model, temperature).await
    }

    async fn chat(
        &self,
        request: crate::providers::traits::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatResponse> {
        self.current()?.chat(request, model, temperature).await
    }
}

struct RuntimeWasmStorage {
    runtime: Arc<PluginRuntime>,
    name: String,
}

impl RuntimeWasmStorage {
    fn current(&self) -> anyhow::Result<Arc<WasmStorage>> {
        self.runtime
            .generation
            .load()
            .storages
            .get(&self.name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("WASM storage '{}' is unavailable in the current generation", self.name))
    }
}

#[async_trait]
impl Memory for RuntimeWasmStorage {
    fn name(&self) -> &str {
        &self.name
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: crate::memory::traits::MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.current()?.store(key, content, category, session_id).await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::traits::MemoryEntry>> {
        self.current()?.recall(query, limit, session_id).await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<crate::memory::traits::MemoryEntry>> {
        self.current()?.get(key).await
    }

    async fn list(
        &self,
        category: Option<&crate::memory::traits::MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::traits::MemoryEntry>> {
        self.current()?.list(category, session_id).await
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.current()?.forget(key).await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        self.current()?.count().await
    }

    async fn health_check(&self) -> bool {
        match self.current() {
            Ok(storage) => storage.health_check().await,
            Err(_) => false,
        }
    }

    fn supports_document_ingest(&self) -> bool {
        false
    }
}

struct PluginToolRouter {
    runtime: Arc<PluginRuntime>,
}

struct PluginStatusTool {
    runtime: Arc<PluginRuntime>,
}

impl PluginToolRouter {
    async fn execute_root(
        &self,
        args: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        let tool = args
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty 'tool' parameter"))?;
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| serde_json::json!({}));
        self.dispatch_alias(tool, arguments, cancellation).await
    }

    async fn dispatch_alias(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        let generation = self.runtime.generation.load_full();
        let Some(tool) = generation.tools.iter().find(|tool| tool.supports_name(name)) else {
            anyhow::bail!(
                "WASM plugin tool '{name}' is not available in generation {}",
                generation.id
            );
        };
        tool.execute_named_with_cancellation(name, args, cancellation).await
    }
}

#[async_trait]
impl Tool for PluginStatusTool {
    fn name(&self) -> &str {
        "wasm_plugins_status"
    }

    fn description(&self) -> &str {
        "List loaded WASM plugins and the atomically published runtime generation."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let plugins = self.runtime.list_plugins().await;
        let errors = self.runtime.adapter_errors();
        let hook_adapters = self.runtime.hook_diagnostics();
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&serde_json::json!({
                "generation": self.runtime.generation_id(),
                "count": plugins.len(),
                "plugins": plugins,
                "hook_adapters": hook_adapters,
                "middleware_adapters": self.runtime.middleware_diagnostics(),
                "cron_adapters": self.runtime.cron_diagnostics(),
                "provider_adapters": self.runtime.provider_names(),
                "storage_adapters": self.runtime.storage_names(),
                "errors": errors,
            }))?,
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System, ToolCategory::Automation]
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        crate::capability::CapabilityAvailability::healthy(format!(
            "WASM runtime generation {} is published",
            self.runtime.generation_id()
        ))
    }
}

struct PluginReloadTool {
    runtime: Arc<PluginRuntime>,
    security: Arc<SecurityPolicy>,
}

struct PluginManageTool {
    runtime: Arc<PluginRuntime>,
    security: Arc<SecurityPolicy>,
}

impl PluginManageTool {
    fn required_string<'a>(args: &'a serde_json::Value, key: &str) -> anyhow::Result<&'a str> {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty '{key}' parameter"))
    }

    fn authorize(&self, action: &str, args: &serde_json::Value) -> Result<(), ToolResult> {
        let risk = match action {
            "status" | "get" | "middleware_test" | "provider_chat" | "storage_health" | "storage_recall"
            | "storage_count" => return Ok(()),
            "refresh" | "enable" | "disable" => ResourceRiskLevel::Low,
            "install" | "update" | "cron_run" | "storage_store" => ResourceRiskLevel::Medium,
            "remove" | "storage_forget" => ResourceRiskLevel::High,
            _ => return Ok(()),
        };
        let operation = format!("wasm_plugins_manage:{action}");
        let grant = ApprovalGrant::from_runtime_args("wasm_plugins_manage", args);
        SideEffectGate::new(self.security.as_ref())
            .authorize_resource_operation("wasm_plugins_manage", &operation, risk, grant.as_ref())
            .map(|_| ())
            .map_err(|error| ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            })
    }

    fn workspace_dir(&self) -> anyhow::Result<&Path> {
        self.runtime
            .plugins_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("WASM plugins directory has no workspace parent"))
    }

    fn disabled_dir(&self) -> anyhow::Result<PathBuf> {
        Ok(self.workspace_dir()?.join(".plugins-disabled"))
    }

    fn trash_dir(&self) -> anyhow::Result<PathBuf> {
        Ok(self.workspace_dir()?.join(".plugins-trash"))
    }

    fn plugin_path(&self, name: &str) -> anyhow::Result<PathBuf> {
        validate_plugin_name(name)?;
        Ok(self.runtime.plugins_dir.join(name))
    }

    fn source_path(&self, raw: &str) -> anyhow::Result<PathBuf> {
        let workspace = std::fs::canonicalize(self.workspace_dir()?)?;
        let requested = Path::new(raw);
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            workspace.join(requested)
        };
        let source = std::fs::canonicalize(joined)?;
        if !source.starts_with(&workspace) || !source.is_dir() {
            anyhow::bail!("plugin source must be a directory inside the workspace");
        }
        Ok(source)
    }

    fn copy_candidate(&self, source: &Path) -> anyhow::Result<(PathBuf, PluginManifest)> {
        let manifest = PluginManifest::from_file(&source.join("plugin.toml"))?;
        validate_plugin_name(&manifest.plugin.name)?;
        let wasm_relative = Path::new(&manifest.plugin.wasm);
        if wasm_relative.is_absolute()
            || wasm_relative.components().count() != 1
            || wasm_relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            anyhow::bail!("plugin WASM path must name one file directly inside its source directory");
        }
        let staging_root = self.workspace_dir()?.join(".plugin-staging");
        std::fs::create_dir_all(&staging_root)?;
        let stage = staging_root.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&stage)?;
        let copy_result = (|| -> anyhow::Result<()> {
            std::fs::copy(source.join("plugin.toml"), stage.join("plugin.toml"))?;
            let wasm_source = source.join(wasm_relative);
            if !wasm_source.exists() {
                anyhow::bail!("plugin WASM file '{}' does not exist", wasm_source.display());
            }
            let metadata = std::fs::symlink_metadata(&wasm_source)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!("plugin WASM must be a regular non-symlink file");
            }
            std::fs::copy(&wasm_source, stage.join(wasm_relative))?;
            std::fs::write(stage.join(".prx-managed"), b"installed by wasm_plugins_manage\n")?;
            Ok(())
        })();
        if let Err(error) = copy_result {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(error);
        }
        // Compile and validate before the active directory is changed.
        if let Err(error) = self.runtime.generation.load().manager.prepare_plugin(&stage) {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(error.into());
        }
        Ok((stage, manifest))
    }

    async fn install_or_update(&self, source: &str, update: bool) -> anyhow::Result<serde_json::Value> {
        let source = self.source_path(source)?;
        let (stage, manifest) = self.copy_candidate(&source)?;
        let name = manifest.plugin.name.clone();
        let destination = self.plugin_path(&name)?;
        if update != destination.exists() {
            let _ = std::fs::remove_dir_all(&stage);
            if update {
                anyhow::bail!("plugin '{name}' is not installed");
            }
            anyhow::bail!("plugin '{name}' is already installed; use update");
        }

        let backup = if destination.exists() {
            let backup_root = self.workspace_dir()?.join(".plugin-backups");
            std::fs::create_dir_all(&backup_root)?;
            let path = backup_root.join(format!("{name}-{}", uuid::Uuid::new_v4()));
            std::fs::rename(&destination, &path)?;
            Some(path)
        } else {
            None
        };
        if let Err(error) = std::fs::rename(&stage, &destination) {
            if let Some(backup) = &backup {
                let _ = std::fs::rename(backup, &destination);
            }
            let _ = std::fs::remove_dir_all(&stage);
            return Err(error.into());
        }

        let refresh = self.runtime.refresh_all().await;
        let loaded = self
            .runtime
            .list_plugins()
            .await
            .into_iter()
            .find(|plugin| plugin.name == name);
        let adapter_error = self
            .runtime
            .adapter_errors()
            .into_iter()
            .find(|error| error.contains(&format!("plugin '{name}'")));
        let loaded_ready = loaded
            .as_ref()
            .is_some_and(|plugin| matches!(&plugin.status, PluginStatus::Active));
        let generation = match (refresh, loaded_ready, adapter_error) {
            (Ok(generation), true, None) => generation,
            (refresh, _, adapter_error) => {
                let refresh_error = refresh.err().map(|error| error.to_string());
                let _ = std::fs::remove_dir_all(&destination);
                if let Some(backup) = &backup {
                    let _ = std::fs::rename(backup, &destination);
                }
                let _ = self.runtime.refresh_all().await;
                if let Some(error) = refresh_error {
                    anyhow::bail!("plugin '{name}' refresh failed and installation was rolled back: {error}");
                }
                if let Some(error) = adapter_error {
                    anyhow::bail!(
                        "plugin '{name}' adapter validation failed and installation was rolled back: {error}"
                    );
                }
                anyhow::bail!("plugin '{name}' was not present after refresh; installation was rolled back");
            }
        };
        Ok(serde_json::json!({
            "action": if update {"update"} else {"install"},
            "generation": generation,
            "plugin": loaded,
            "backup": backup,
        }))
    }

    async fn disable(&self, name: &str) -> anyhow::Result<serde_json::Value> {
        let source = self.plugin_path(name)?;
        if !source.is_dir() {
            anyhow::bail!("plugin '{name}' is not installed");
        }
        let disabled_root = self.disabled_dir()?;
        std::fs::create_dir_all(&disabled_root)?;
        let destination = disabled_root.join(name);
        if destination.exists() {
            anyhow::bail!("disabled plugin '{name}' already exists");
        }
        std::fs::rename(&source, &destination)?;
        match self.runtime.refresh_all().await {
            Ok(generation) => Ok(serde_json::json!({"plugin": name, "enabled": false, "generation": generation})),
            Err(error) => {
                let _ = std::fs::rename(&destination, &source);
                let _ = self.runtime.refresh_all().await;
                Err(error.into())
            }
        }
    }

    async fn enable(&self, name: &str) -> anyhow::Result<serde_json::Value> {
        let destination = self.plugin_path(name)?;
        if destination.exists() {
            anyhow::bail!("plugin '{name}' is already enabled");
        }
        let source = self.disabled_dir()?.join(name);
        if !source.is_dir() {
            anyhow::bail!("disabled plugin '{name}' was not found");
        }
        self.runtime.generation.load().manager.prepare_plugin(&source)?;
        std::fs::rename(&source, &destination)?;
        let refresh = self.runtime.refresh_all().await;
        let loaded = self
            .runtime
            .list_plugins()
            .await
            .into_iter()
            .find(|plugin| plugin.name == name);
        let adapter_error = self
            .runtime
            .adapter_errors()
            .into_iter()
            .find(|error| error.contains(&format!("plugin '{name}'")));
        let loaded_ready = loaded
            .as_ref()
            .is_some_and(|plugin| matches!(&plugin.status, PluginStatus::Active));
        let generation = match (refresh, loaded_ready, adapter_error) {
            (Ok(generation), true, None) => generation,
            (refresh, _, adapter_error) => {
                let refresh_error = refresh.err().map(|error| error.to_string());
                let _ = std::fs::rename(&destination, &source);
                let _ = self.runtime.refresh_all().await;
                if let Some(error) = refresh_error {
                    anyhow::bail!("plugin '{name}' refresh failed and enable was rolled back: {error}");
                }
                if let Some(error) = adapter_error {
                    anyhow::bail!("plugin '{name}' adapter validation failed and enable was rolled back: {error}");
                }
                anyhow::bail!("plugin '{name}' did not load; enable was rolled back");
            }
        };
        Ok(serde_json::json!({"plugin": loaded, "enabled": true, "generation": generation}))
    }

    async fn remove(&self, name: &str) -> anyhow::Result<serde_json::Value> {
        let active = self.plugin_path(name)?;
        let disabled = self.disabled_dir()?.join(name);
        let source = if active.is_dir() {
            active.clone()
        } else if disabled.is_dir() {
            disabled
        } else {
            anyhow::bail!("plugin '{name}' is not installed or disabled");
        };
        let trash_root = self.trash_dir()?;
        std::fs::create_dir_all(&trash_root)?;
        let backup = trash_root.join(format!("{name}-{}", uuid::Uuid::new_v4()));
        std::fs::rename(&source, &backup)?;
        let generation = if source == active {
            match self.runtime.refresh_all().await {
                Ok(generation) => Some(generation),
                Err(error) => {
                    let _ = std::fs::rename(&backup, &source);
                    let _ = self.runtime.refresh_all().await;
                    return Err(error.into());
                }
            }
        } else {
            None
        };
        Ok(serde_json::json!({
            "plugin": name,
            "removed": true,
            "recoverable_backup": backup,
            "generation": generation,
        }))
    }

    fn disabled_plugins(&self) -> anyhow::Result<Vec<String>> {
        let root = self.disabled_dir()?;
        let mut names = if root.is_dir() {
            std::fs::read_dir(root)?
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_dir())
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        names.sort();
        Ok(names)
    }

    async fn active_inventory(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let loaded = self
            .runtime
            .list_plugins()
            .await
            .into_iter()
            .map(|plugin| (plugin.name.clone(), plugin))
            .collect::<HashMap<_, _>>();
        let mut paths = if self.runtime.plugins_dir.is_dir() {
            std::fs::read_dir(&self.runtime.plugins_dir)?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir() && path.file_name().is_some_and(|name| name != ".cwasm-cache"))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        paths.sort();
        let mut inventory = Vec::new();
        for path in paths {
            let directory = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            match PluginManifest::from_file(&path.join("plugin.toml")) {
                Ok(manifest) => {
                    let name = manifest.plugin.name;
                    if let Some(plugin) = loaded.get(&name) {
                        inventory.push(serde_json::json!({
                            "directory": directory,
                            "loaded": true,
                            "plugin": plugin,
                        }));
                    } else {
                        inventory.push(serde_json::json!({
                            "directory": directory,
                            "name": name,
                            "version": manifest.plugin.version,
                            "loaded": false,
                            "error": "plugin was skipped while building the current generation; inspect daemon logs for the load error",
                        }));
                    }
                }
                Err(error) => inventory.push(serde_json::json!({
                    "directory": directory,
                    "loaded": false,
                    "error": error.to_string(),
                })),
            }
        }
        Ok(inventory)
    }

    async fn test_middleware(&self, args: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let stage_name = Self::required_string(args, "stage")?;
        let stage = super::capabilities::middleware::MiddlewareStage::parse(stage_name)
            .ok_or_else(|| anyhow::anyhow!("unsupported middleware stage '{stage_name}'"))?;
        let input = args.get("data").cloned().unwrap_or_else(|| serde_json::json!({}));
        let output = self.runtime.middleware().process(stage, &input.to_string()).await;
        let output: serde_json::Value = serde_json::from_str(&output)
            .map_err(|error| anyhow::anyhow!("middleware output is not valid JSON: {error}"))?;
        Ok(serde_json::json!({"stage": stage_name, "input": input, "output": output}))
    }

    async fn run_cron(&self, args: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let name = Self::required_string(args, "name")?;
        let output = self.runtime.cron().run_job(name).await?;
        Ok(serde_json::json!({"plugin": name, "output": output}))
    }

    async fn chat_provider(&self, args: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let name = Self::required_string(args, "name")?;
        let model = args
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default");
        let temperature = args
            .get("temperature")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let messages = if let Some(messages) = args.get("messages") {
            serde_json::from_value::<Vec<crate::providers::traits::ChatMessage>>(messages.clone())?
        } else {
            vec![crate::providers::traits::ChatMessage::user(Self::required_string(
                args, "message",
            )?)]
        };
        let generation = self.runtime.generation.load_full();
        let provider = generation
            .providers
            .get(strip_wasm_prefix(name))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("WASM provider '{name}' is not available"))?;
        let response = crate::providers::traits::Provider::chat(
            provider.as_ref(),
            crate::providers::traits::ChatRequest {
                messages: &messages,
                tools: None,
            },
            model,
            temperature,
        )
        .await?;
        Ok(serde_json::json!({
            "provider": name,
            "model": model,
            "text": response.text,
            "tool_calls": response.tool_calls,
            "reasoning_content": response.reasoning_content,
        }))
    }

    async fn storage_operation(&self, action: &str, args: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let name = Self::required_string(args, "name")?;
        let generation = self.runtime.generation.load_full();
        let storage = generation
            .storages
            .get(strip_wasm_prefix(name))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("WASM storage '{name}' is not available"))?;
        match action {
            "storage_health" => Ok(serde_json::json!({"storage": name, "healthy": storage.health_check().await})),
            "storage_count" => Ok(serde_json::json!({"storage": name, "count": storage.count().await?})),
            "storage_store" => {
                let key = Self::required_string(args, "key")?;
                let content = Self::required_string(args, "content")?;
                let category = args
                    .get("category")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("custom");
                let category = match category {
                    "core" => crate::memory::traits::MemoryCategory::Core,
                    "daily" => crate::memory::traits::MemoryCategory::Daily,
                    "conversation" => crate::memory::traits::MemoryCategory::Conversation,
                    other => crate::memory::traits::MemoryCategory::Custom(other.to_string()),
                };
                let session_id = args.get("session_id").and_then(serde_json::Value::as_str);
                storage.store(key, content, category, session_id).await?;
                Ok(serde_json::json!({"storage": name, "stored": true, "key": key}))
            }
            "storage_recall" => {
                let query = args.get("query").and_then(serde_json::Value::as_str).unwrap_or("");
                let limit = args.get("limit").and_then(serde_json::Value::as_u64).unwrap_or(10) as usize;
                let session_id = args.get("session_id").and_then(serde_json::Value::as_str);
                let entries = storage.recall(query, limit, session_id).await?;
                Ok(serde_json::json!({"storage": name, "entries": entries}))
            }
            "storage_forget" => {
                let key = Self::required_string(args, "key")?;
                Ok(serde_json::json!({"storage": name, "key": key, "forgotten": storage.forget(key).await?}))
            }
            _ => anyhow::bail!("unsupported storage action '{action}'"),
        }
    }
}

fn validate_plugin_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        anyhow::bail!("plugin name must be 1-128 ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

#[async_trait]
impl Tool for PluginManageTool {
    fn name(&self) -> &str {
        "wasm_plugins_manage"
    }

    fn description(&self) -> &str {
        "Inspect, manage, and invoke workspace WASM plugins across tool, middleware, hook, cron, provider, and storage capabilities."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["status", "get", "refresh", "install", "update", "enable", "disable", "remove", "middleware_test", "cron_run", "provider_chat", "storage_health", "storage_store", "storage_recall", "storage_forget", "storage_count"]},
                "name": {"type": "string", "minLength": 1, "description": "Required for get, enable, disable, remove, cron_run, provider_chat, and every storage_* action; use a plugin name or exported adapter name"},
                "source": {"type": "string", "description": "Plugin source directory inside the workspace for install or update"},
                "stage": {"type": "string", "enum": ["inbound", "outbound", "llm_request", "llm_response"]},
                "data": {"description": "JSON envelope for middleware_test"},
                "message": {"type": "string", "description": "Single user message for provider_chat"},
                "messages": {"type": "array", "items": {"type": "object", "required": ["role", "content"]}},
                "model": {"type": "string"},
                "temperature": {"type": "number"},
                "key": {"type": "string"},
                "content": {"type": "string"},
                "category": {"type": "string"},
                "session_id": {"type": "string"},
                "query": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1}
            },
            "required": ["action"],
            "allOf": [
                {
                    "if": {"properties": {"action": {"enum": ["get", "enable", "disable", "remove", "cron_run", "provider_chat", "storage_health", "storage_store", "storage_recall", "storage_forget", "storage_count"]}}},
                    "then": {"required": ["name"]}
                },
                {
                    "if": {"properties": {"action": {"enum": ["install", "update"]}}},
                    "then": {"required": ["source"]}
                },
                {
                    "if": {"properties": {"action": {"const": "middleware_test"}}},
                    "then": {"required": ["stage"]}
                },
                {
                    "if": {"properties": {"action": {"const": "storage_store"}}},
                    "then": {"required": ["key", "content"]}
                },
                {
                    "if": {"properties": {"action": {"const": "storage_forget"}}},
                    "then": {"required": ["key"]}
                }
            ],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = Self::required_string(&args, "action")?;
        if let Err(result) = self.authorize(action, &args) {
            return Ok(result);
        }
        let result: anyhow::Result<serde_json::Value> = match action {
            "status" => Ok(serde_json::json!({
                "generation": self.runtime.generation_id(),
                "active": self.active_inventory().await?,
                "disabled": self.disabled_plugins()?,
                "hook_adapters": self.runtime.hook_diagnostics(),
                "middleware_adapters": self.runtime.middleware_diagnostics(),
                "cron_adapters": self.runtime.cron_diagnostics(),
                "provider_adapters": self.runtime.provider_names(),
                "storage_adapters": self.runtime.storage_names(),
                "errors": self.runtime.adapter_errors(),
            })),
            "get" => {
                let name = Self::required_string(&args, "name")?;
                validate_plugin_name(name)?;
                let active = self.active_inventory().await?;
                let item = active.into_iter().find(|item| {
                    item.pointer("/plugin/name").and_then(serde_json::Value::as_str) == Some(name)
                        || item.get("name").and_then(serde_json::Value::as_str) == Some(name)
                        || item.get("directory").and_then(serde_json::Value::as_str) == Some(name)
                });
                if let Some(item) = item {
                    Ok(item)
                } else if self.disabled_plugins()?.iter().any(|disabled| disabled == name) {
                    Ok(serde_json::json!({"name": name, "enabled": false}))
                } else {
                    anyhow::bail!("plugin '{name}' was not found")
                }
            }
            "refresh" => self
                .runtime
                .refresh_all()
                .await
                .map(|generation| serde_json::json!({"refreshed": true, "generation": generation}))
                .map_err(Into::into),
            "install" | "update" => {
                let source = Self::required_string(&args, "source")?;
                self.install_or_update(source, action == "update").await
            }
            "enable" => self.enable(Self::required_string(&args, "name")?).await,
            "disable" => self.disable(Self::required_string(&args, "name")?).await,
            "remove" => self.remove(Self::required_string(&args, "name")?).await,
            "middleware_test" => self.test_middleware(&args).await,
            "cron_run" => self.run_cron(&args).await,
            "provider_chat" => self.chat_provider(&args).await,
            action @ ("storage_health" | "storage_store" | "storage_recall" | "storage_forget" | "storage_count") => {
                self.storage_operation(action, &args).await
            }
            _ => anyhow::bail!("Unsupported wasm_plugins_manage action: {action}"),
        };
        Ok(match result {
            Ok(output) => ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&output)?,
                error: None,
            },
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(error.to_string()),
            },
        })
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System, ToolCategory::Automation]
    }
}

#[async_trait]
impl Tool for PluginReloadTool {
    fn name(&self) -> &str {
        "wasm_plugin_reload"
    }

    fn description(&self) -> &str {
        "Atomically rebuild and reload one already-known WASM plugin by name."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "Loaded plugin name"}},
            "required": ["name"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty 'name' parameter"))?;
        let operation_name = op_id::op_id(self.name(), "reload", &[name]);
        let approval_grant = ApprovalGrant::from_runtime_args(self.name(), &args);
        if let Err(error) = SideEffectGate::new(&self.security).authorize_resource_operation(
            self.name(),
            &operation_name,
            ResourceRiskLevel::Low,
            approval_grant.as_ref(),
        ) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }
        match self.runtime.reload_plugin(name).await {
            Ok(generation) => Ok(ToolResult {
                success: true,
                output: serde_json::json!({"plugin": name, "generation": generation}).to_string(),
                error: None,
            }),
            Err(error) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error.to_string()),
            }),
        }
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System, ToolCategory::Automation]
    }
}

#[async_trait]
impl Tool for PluginToolRouter {
    fn name(&self) -> &str {
        "wasm_plugin_call"
    }

    fn description(&self) -> &str {
        "Call a tool exposed by the current atomically published WASM plugin generation."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let mut names = self
            .runtime
            .generation
            .load()
            .tools
            .iter()
            .flat_map(|tool| tool.specs().into_iter().map(|spec| spec.name))
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool": {"type": "string", "enum": names},
                "arguments": {"type": "object", "default": {}}
            },
            "required": ["tool"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_root(args, None).await
    }

    fn specs(&self) -> Vec<ToolSpec> {
        let root = self.spec();
        let mut aliases = self
            .runtime
            .generation
            .load()
            .tools
            .iter()
            .flat_map(|tool| tool.specs())
            .collect::<Vec<_>>();
        aliases.sort_by(|left, right| left.name.cmp(&right.name));
        aliases.dedup_by(|left, right| left.name == right.name);
        aliases.truncate(WASM_MAX_ALIAS_SPECS);
        let mut specs = Vec::with_capacity(aliases.len().saturating_add(1));
        specs.push(root);
        specs.extend(aliases);
        specs
    }

    fn supports_name(&self, name: &str) -> bool {
        name == self.name()
            || self
                .runtime
                .generation
                .load()
                .tools
                .iter()
                .any(|tool| tool.supports_name(name))
    }

    async fn execute_named(&self, name: &str, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.execute_named_with_cancellation(name, args, None).await
    }

    async fn execute_named_with_cancellation(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation: Option<CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        if name == self.name() {
            return self.execute_root(args, cancellation).await;
        }
        self.dispatch_alias(name, args, cancellation).await
    }

    fn availability(&self) -> crate::capability::CapabilityAvailability {
        let generation = self.runtime.generation.load();
        crate::capability::CapabilityAvailability::healthy(format!(
            "WASM generation {} exposes {} executable tool backend(s)",
            generation.id,
            generation.tools.len()
        ))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Automation]
    }
}

type RuntimeMap = HashMap<PathBuf, Weak<PluginRuntime>>;
static PROCESS_RUNTIMES: OnceLock<Mutex<RuntimeMap>> = OnceLock::new();
static PROCESS_RUNTIME_INDEX: OnceLock<parking_lot::RwLock<Vec<Weak<PluginRuntime>>>> = OnceLock::new();

fn register_process_runtime(runtime: &Arc<PluginRuntime>) {
    let index = PROCESS_RUNTIME_INDEX.get_or_init(|| parking_lot::RwLock::new(Vec::new()));
    let mut runtimes = index.write();
    runtimes.retain(|candidate| candidate.strong_count() > 0);
    if !runtimes
        .iter()
        .filter_map(Weak::upgrade)
        .any(|candidate| Arc::ptr_eq(&candidate, runtime))
    {
        runtimes.push(Arc::downgrade(runtime));
    }
}

/// Resolve `wasm:<exported-name>` through the process runtime index.
///
/// Provider factories call this synchronously after the workspace runtime has
/// been initialized. Restricting implicit lookup to the explicit `wasm:` prefix
/// prevents a plugin from shadowing a built-in provider name.
pub fn resolve_process_provider(name: &str) -> Option<Box<dyn crate::providers::traits::Provider>> {
    let exported = name.strip_prefix("wasm:")?;
    let runtimes = PROCESS_RUNTIME_INDEX.get()?.read();
    runtimes
        .iter()
        .rev()
        .filter_map(Weak::upgrade)
        .find_map(|runtime| runtime.provider(exported))
}

/// Resolve `wasm:<exported-name>` as a reload-safe Memory backend.
pub fn resolve_process_storage(name: &str) -> Option<Box<dyn Memory>> {
    let exported = name.strip_prefix("wasm:")?;
    let runtimes = PROCESS_RUNTIME_INDEX.get()?.read();
    runtimes.iter().rev().filter_map(Weak::upgrade).find_map(|runtime| {
        runtime.generation.load().storages.contains_key(exported).then(|| {
            Box::new(RuntimeWasmStorage {
                runtime,
                name: exported.to_string(),
            }) as Box<dyn Memory>
        })
    })
}

/// Return the one process-level runtime for `workspace_dir`, creating it once.
pub async fn init_plugin_runtime(workspace_dir: &Path, memory: Option<Arc<dyn Memory>>) -> Option<Arc<PluginRuntime>> {
    let key = std::fs::canonicalize(workspace_dir).unwrap_or_else(|_| workspace_dir.to_path_buf());
    let runtimes = PROCESS_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut runtimes = runtimes.lock().await;
    if let Some(runtime) = runtimes.get(&key).and_then(Weak::upgrade) {
        return Some(runtime);
    }

    match PluginRuntime::new(&key, memory).await {
        Ok(runtime) => {
            register_process_runtime(&runtime);
            runtimes.insert(key, Arc::downgrade(&runtime));
            Some(runtime)
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to initialize WASM plugin runtime");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, version: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            format!(
                r#"[plugin]
name = "atomic-test"
version = "{version}"
description = "atomic reload test"
wasm = "missing.wasm"

[permissions]
required = []
optional = []
"#
            ),
        )
        .unwrap();
    }

    fn install_example_fixture(workspace: &Path, example: &str) {
        let plugin_dir = workspace.join("plugins").join(example);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("pdk/rust/examples")
            .join(example);
        std::fs::copy(fixture.join("plugin.toml"), plugin_dir.join("plugin.toml")).unwrap();
        std::fs::copy(fixture.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).unwrap();
    }

    #[tokio::test]
    async fn workspace_has_one_process_runtime_owner() {
        let temp = TempDir::new().unwrap();
        let first = init_plugin_runtime(temp.path(), None).await.unwrap();
        let second = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&first.event_bus(), &second.event_bus()));
    }

    #[tokio::test]
    async fn reload_swaps_complete_generation_and_failed_candidate_preserves_old() {
        let temp = TempDir::new().unwrap();
        let plugin_dir = temp.path().join("plugins/atomic-test");
        write_manifest(&plugin_dir, "1.0.0");
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert_eq!(runtime.generation_id(), 1);

        write_manifest(&plugin_dir, "2.0.0");
        assert_eq!(runtime.reload_plugin("atomic-test").await.unwrap(), 2);
        let plugins = runtime.list_plugins().await;
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins.first().unwrap().version, "2.0.0");

        std::fs::write(plugin_dir.join("plugin.toml"), "invalid = [").unwrap();
        assert!(runtime.reload_plugin("atomic-test").await.is_err());
        assert_eq!(runtime.generation_id(), 2);
        assert_eq!(runtime.list_plugins().await.first().unwrap().version, "2.0.0");
    }

    #[tokio::test]
    async fn agent_control_tools_expose_generation_and_root_call_fallback() {
        let temp = TempDir::new().unwrap();
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        let router = runtime.tool_router();
        let status = runtime.status_tool();

        assert_eq!(router.name(), "wasm_plugin_call");
        assert_eq!(
            router.specs().first().map(|spec| spec.name.as_str()),
            Some("wasm_plugin_call")
        );
        assert!(router.supports_name("wasm_plugin_call"));

        let result = status.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("\"generation\": 1"));
    }

    #[tokio::test]
    async fn manage_schema_declares_action_specific_required_fields() {
        let temp = TempDir::new().unwrap();
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        let config = crate::config::Config {
            workspace_dir: temp.path().to_path_buf(),
            ..crate::config::Config::default()
        };
        let manage = runtime.manage_tool(crate::runtime::bootstrap::build_security_policy(&config));
        let schema = manage.parameters_schema();
        let conditions = schema
            .get("allOf")
            .and_then(serde_json::Value::as_array)
            .expect("manage schema should publish action-specific requirements");

        assert!(conditions.iter().any(|condition| {
            condition.pointer("/if/properties/action/const") == Some(&serde_json::json!("storage_forget"))
                && condition.pointer("/then/required") == Some(&serde_json::json!(["key"]))
        }));
        assert!(conditions.iter().any(|condition| {
            condition
                .pointer("/if/properties/action/enum")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|actions| actions.contains(&serde_json::json!("storage_forget")))
                && condition.pointer("/then/required") == Some(&serde_json::json!(["name"]))
        }));
    }

    #[tokio::test]
    async fn manage_tool_runs_install_call_disable_enable_remove_lifecycle() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("plugin-sources/voice-talk-realtime");
        std::fs::create_dir_all(&source).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/voice-talk-realtime");
        std::fs::copy(fixture.join("plugin.toml"), source.join("plugin.toml")).unwrap();
        std::fs::copy(fixture.join("plugin.wasm"), source.join("plugin.wasm")).unwrap();

        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        let manage = runtime.manage_tool(crate::runtime::bootstrap::build_security_policy(&config));

        let installed = manage
            .execute(serde_json::json!({
                "action": "install",
                "source": "plugin-sources/voice-talk-realtime"
            }))
            .await
            .unwrap();
        assert!(installed.success, "install failed: {:?}", installed.error);
        assert!(
            runtime
                .list_plugins()
                .await
                .iter()
                .any(|plugin| plugin.name == "voice-talk-realtime")
        );

        let called = runtime
            .tool_router()
            .execute_named("voice_session", serde_json::json!({"provider": "openai"}))
            .await
            .unwrap();
        assert!(called.success, "WASM call failed: {:?}", called.error);
        assert!(called.output.contains("wss://api.openai.com"));

        let disabled = manage
            .execute(serde_json::json!({"action": "disable", "name": "voice-talk-realtime"}))
            .await
            .unwrap();
        assert!(disabled.success, "disable failed: {:?}", disabled.error);
        assert!(runtime.list_plugins().await.is_empty());

        let enabled = manage
            .execute(serde_json::json!({"action": "enable", "name": "voice-talk-realtime"}))
            .await
            .unwrap();
        assert!(enabled.success, "enable failed: {:?}", enabled.error);
        assert_eq!(runtime.list_plugins().await.len(), 1);

        let removed = manage
            .execute(serde_json::json!({"action": "remove", "name": "voice-talk-realtime"}))
            .await
            .unwrap();
        assert!(removed.success, "remove failed: {:?}", removed.error);
        assert!(runtime.list_plugins().await.is_empty());
        assert!(temp.path().join(".plugins-trash").is_dir());
    }

    #[tokio::test]
    async fn documented_audit_hook_instantiates_and_records_real_delivery() {
        let temp = TempDir::new().unwrap();
        let plugin_dir = temp.path().join("plugins/audit-hook");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/audit-hook");
        std::fs::copy(fixture.join("plugin.toml"), plugin_dir.join("plugin.toml")).unwrap();
        std::fs::copy(fixture.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).unwrap();
        std::fs::write(plugin_dir.join(".prx-managed"), "test fixture\n").unwrap();

        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let before = runtime.hook_diagnostics();
        assert_eq!(before.len(), 1);
        assert_eq!(before.first().unwrap().invocation_count, 0);

        runtime
            .emit_hook("prx.lifecycle.turn_complete", r#"{"source":"regression-test"}"#)
            .await;

        let after = runtime.hook_diagnostics();
        let hook = after.first().unwrap();
        assert_eq!(hook.invocation_count, 1);
        assert_eq!(hook.last_event.as_deref(), Some("prx.lifecycle.turn_complete"));
        assert!(hook.last_error.is_none(), "{:?}", hook.last_error);
    }

    #[tokio::test]
    async fn every_wasm_capability_executes_through_the_live_generation() {
        let temp = TempDir::new().unwrap();
        for example in ["pipeline-middleware", "counter-cron", "echo-provider", "map-storage"] {
            install_example_fixture(temp.path(), example);
        }

        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        assert_eq!(runtime.middleware_diagnostics().len(), 1);
        assert_eq!(runtime.cron_diagnostics().len(), 1);
        assert_eq!(runtime.provider_names(), vec!["wasm-echo"]);
        assert_eq!(runtime.storage_names(), vec!["wasm-map"]);

        let middleware_output = runtime
            .middleware()
            .process(
                super::super::capabilities::middleware::MiddlewareStage::Outbound,
                r#"{"text":"hello"}"#,
            )
            .await;
        let middleware_output: serde_json::Value = serde_json::from_str(&middleware_output).unwrap();
        assert_eq!(
            middleware_output.get("text").and_then(serde_json::Value::as_str),
            Some("[wasm-middleware] hello")
        );
        assert_eq!(
            middleware_output
                .pointer("/middleware_probe/stage")
                .and_then(serde_json::Value::as_str),
            Some("outbound")
        );

        assert_eq!(
            runtime.cron().run_job("counter-cron").await.unwrap(),
            "counter-cron-run-1"
        );
        assert_eq!(
            runtime.cron().run_job("counter-cron").await.unwrap(),
            "counter-cron-run-2"
        );

        let provider = runtime.provider("wasm-echo").unwrap();
        let response = provider
            .chat(
                crate::providers::traits::ChatRequest {
                    messages: &[crate::providers::traits::ChatMessage::user("provider probe")],
                    tools: None,
                },
                "fixture-model",
                0.0,
            )
            .await
            .unwrap();
        assert_eq!(
            response.text.as_deref(),
            Some("wasm-echo/fixture-model: provider probe")
        );
        assert!(crate::providers::create_provider("wasm:wasm-echo", None).is_ok());

        let storage = runtime.storage("wasm-map").unwrap();
        storage
            .store(
                "fixture-key",
                "fixture-content",
                crate::memory::traits::MemoryCategory::Core,
                Some("fixture-session"),
            )
            .await
            .unwrap();
        assert!(storage.health_check().await);
        assert_eq!(storage.count().await.unwrap(), 1);
        let recalled = storage.recall("fixture", 10, Some("fixture-session")).await.unwrap();
        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled.first().map(|entry| entry.key.as_str()), Some("fixture-key"));
        assert!(storage.forget("fixture-key").await.unwrap());
        assert_eq!(storage.count().await.unwrap(), 0);

        runtime.refresh_all().await.unwrap();
        let refreshed = provider
            .simple_chat("after refresh", "fixture-model", 0.0)
            .await
            .unwrap();
        assert_eq!(refreshed, "wasm-echo/fixture-model: after refresh");
        assert!(
            storage.health_check().await,
            "storage proxy must resolve the refreshed generation"
        );
    }

    #[tokio::test]
    async fn mixed_capability_stress_executes_one_thousand_component_calls() {
        let temp = TempDir::new().unwrap();
        for example in ["pipeline-middleware", "counter-cron", "echo-provider", "map-storage"] {
            install_example_fixture(temp.path(), example);
        }
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        let provider = runtime.provider("wasm-echo").unwrap();
        let storage = runtime.storage("wasm-map").unwrap();

        let mut calls = 0usize;
        for index in 0..200usize {
            let middleware_output = runtime
                .middleware()
                .process(
                    super::super::capabilities::middleware::MiddlewareStage::LlmRequest,
                    &serde_json::json!({"index": index}).to_string(),
                )
                .await;
            assert!(middleware_output.contains("middleware_probe"));
            calls += 1;

            let cron_output = runtime.cron().run_job("counter-cron").await.unwrap();
            assert!(cron_output.starts_with("counter-cron-run-"));
            calls += 1;

            let response = provider
                .simple_chat(&format!("probe-{index}"), "stress-model", 0.0)
                .await
                .unwrap();
            assert!(response.ends_with(&format!("probe-{index}")));
            calls += 1;

            let key = format!("stress-{index}");
            storage
                .store(
                    &key,
                    "stress-content",
                    crate::memory::traits::MemoryCategory::Custom("stress".to_string()),
                    None,
                )
                .await
                .unwrap();
            calls += 1;

            let recalled = storage.recall(&key, 1, None).await.unwrap();
            assert_eq!(recalled.first().map(|entry| entry.key.as_str()), Some(key.as_str()));
            calls += 1;
        }

        assert_eq!(calls, 1_000);
        assert_eq!(storage.count().await.unwrap(), 200);
    }
}
