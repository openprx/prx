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
use super::capabilities::tool::WasmToolAdapter;
use super::error::{PluginError, PluginResult};
use super::event_bus::EventBus;
use super::manifest::PluginManifest;
use super::registry::{PluginInfo, PluginStatus};
use super::trust::{PluginMaterial, PluginPolicy};
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
    /// The same adapters as `tools`, typed, so their health can be read.
    wasm_tools: Vec<Arc<WasmToolAdapter>>,
    middleware: Arc<MiddlewareChain>,
    hooks: Arc<WasmHookExecutor>,
    cron: Arc<WasmCronManager>,
    providers: HashMap<String, Arc<WasmProvider>>,
    storages: HashMap<String, Arc<WasmStorage>>,
    channels: HashMap<String, Arc<super::capabilities::channel::WasmChannel>>,
    errors: Vec<String>,
}

impl PluginGeneration {
    async fn build(
        id: u64,
        plugins_dir: PathBuf,
        policy: PluginPolicy,
        memory: Option<Arc<dyn Memory>>,
        event_bus: Arc<EventBus>,
    ) -> PluginResult<Self> {
        let manager = Arc::new(PluginManager::new(plugins_dir, policy)?);
        manager.load_all().await?;

        let tools_build = manager
            .create_wasm_tool_adapters(memory.clone(), Some(Arc::clone(&event_bus)))
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
        let channel_build = manager.create_channel_adapters(Some(Arc::clone(&event_bus))).await;
        let storage_build = manager.create_storage_adapters(Some(event_bus)).await;
        let wasm_tools: Vec<Arc<WasmToolAdapter>> = tools_build.value.into_iter().map(Arc::new).collect();
        let tools = wasm_tools
            .iter()
            .map(|tool| Arc::clone(tool) as Arc<dyn Tool>)
            .collect();
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
            .chain(channel_build.errors)
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

        let channels = channel_build
            .value
            .into_iter()
            .map(|channel| (channel.name().to_owned(), Arc::new(channel)))
            .collect();
        Ok(Self {
            id,
            manager,
            tools,
            wasm_tools,
            middleware,
            hooks,
            cron,
            providers,
            storages,
            channels,
            errors,
        })
    }
}

/// Sole process-level owner of a workspace's plugin generation and event bus.
pub struct PluginRuntime {
    plugins_dir: PathBuf,
    policy: PluginPolicy,
    memory: parking_lot::RwLock<Option<Arc<dyn Memory>>>,
    event_bus: Arc<EventBus>,
    generation: ArcSwap<PluginGeneration>,
    reload_lock: Mutex<()>,
}

impl PluginRuntime {
    async fn new(
        workspace_dir: &Path,
        policy: PluginPolicy,
        memory: Option<Arc<dyn Memory>>,
    ) -> PluginResult<Arc<Self>> {
        if !policy.enabled() {
            return Err(PluginError::Disabled);
        }
        let plugins_dir = workspace_dir.join("plugins");
        std::fs::create_dir_all(&plugins_dir).map_err(PluginError::Io)?;
        let event_bus = Arc::new(EventBus::new());
        let generation = PluginGeneration::build(
            1,
            plugins_dir.clone(),
            policy.clone(),
            memory.clone(),
            Arc::clone(&event_bus),
        )
        .await?;
        let runtime = Arc::new(Self {
            plugins_dir,
            policy,
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

    /// Canonical plugin source directory inside this runtime's workspace.
    ///
    /// Install and update review hashes on this path, then the manage tool
    /// resolves the operator's original source string with the same rules.
    pub fn resolve_install_source(&self, raw: &str) -> anyhow::Result<PathBuf> {
        let workspace = self
            .plugins_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("WASM plugins directory has no workspace parent"))?;
        let workspace = std::fs::canonicalize(workspace)?;
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

    /// `plugins/<name>` is on disk, but this generation did not load that name.
    /// Enable and reload cannot first-discover it.
    pub async fn absent_from_loaded_generation(&self, name: &str) -> anyhow::Result<bool> {
        validate_plugin_name(name)?;
        if !self.plugins_dir.join(name).is_dir() {
            return Ok(false);
        }
        let loaded = self.list_plugins().await;
        Ok(!loaded.iter().any(|plugin| plugin.name == name))
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
        let candidate = PluginGeneration::build(
            next_id,
            self.plugins_dir.clone(),
            self.policy.clone(),
            memory,
            Arc::clone(&self.event_bus),
        )
        .await?;
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

    /// Middleware adapters the current generation stopped calling after they
    /// spent their restart budget on repeated store-poisoning outcomes.
    pub fn quarantined_middleware(&self) -> Vec<String> {
        self.generation.load().middleware.quarantined_plugins()
    }

    /// Per-adapter health of the current generation's WASM tools.
    pub fn tool_diagnostics(&self) -> Vec<serde_json::Value> {
        self.generation
            .load()
            .wasm_tools
            .iter()
            .map(|tool| tool.diagnostics())
            .collect()
    }

    /// Every adapter of the current generation that spent its restart budget
    /// and is refused until the next generation, as `kind:plugin`.
    pub fn quarantined_adapters(&self) -> Vec<String> {
        let generation = self.generation.load();
        let middleware = generation
            .middleware
            .quarantined_plugins()
            .into_iter()
            .map(|plugin| format!("middleware:{plugin}"));
        let tools = generation
            .wasm_tools
            .iter()
            .filter(|tool| tool.is_quarantined())
            .map(|tool| format!("tool:{}", tool.plugin_name()));
        let hooks = generation
            .hooks
            .quarantined_plugins()
            .into_iter()
            .map(|plugin| format!("hook:{plugin}"));
        let cron = generation
            .cron
            .quarantined_plugins()
            .into_iter()
            .map(|plugin| format!("cron:{plugin}"));
        let mut providers = generation
            .providers
            .values()
            .filter(|provider| provider.is_quarantined())
            .map(|provider| format!("provider:{}", provider.plugin_name()))
            .collect::<Vec<_>>();
        providers.sort();
        let mut storages = generation
            .storages
            .values()
            .filter(|storage| storage.is_quarantined())
            .map(|storage| format!("storage:{}", storage.plugin_name()))
            .collect::<Vec<_>>();
        storages.sort();
        middleware
            .chain(tools)
            .chain(hooks)
            .chain(cron)
            .chain(providers)
            .chain(storages)
            .chain(
                generation
                    .channels
                    .values()
                    .filter(|channel| channel.is_quarantined())
                    .map(|channel| format!("channel:{}", channel.name())),
            )
            .collect()
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
    pub fn channel_diagnostics(&self) -> Vec<serde_json::Value> {
        let generation = self.generation.load();
        let mut entries = generation.channels.iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(right.0));
        entries.into_iter().map(|(_, channel)| channel.diagnostics()).collect()
    }

    /// Stable proxies resolve the active generation on every send and poll.
    pub fn channels(self: &Arc<Self>) -> Vec<Arc<dyn crate::channels::traits::Channel>> {
        let mut names = self.generation.load().channels.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                Arc::new(RuntimeWasmChannel {
                    runtime: Arc::clone(self),
                    name,
                }) as Arc<dyn crate::channels::traits::Channel>
            })
            .collect()
    }

    pub fn provider_names(&self) -> Vec<String> {
        let mut names = self.generation.load().providers.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Per-adapter health of the current generation's WASM providers, in
    /// provider-name order.
    pub fn provider_diagnostics(&self) -> Vec<serde_json::Value> {
        let generation = self.generation.load();
        let mut providers = generation.providers.iter().collect::<Vec<_>>();
        providers.sort_by(|left, right| left.0.cmp(right.0));
        providers
            .into_iter()
            .map(|(_, provider)| provider.diagnostics())
            .collect()
    }

    /// Per-adapter health of the current generation's WASM storage backends,
    /// in storage-name order.
    pub fn storage_diagnostics(&self) -> Vec<serde_json::Value> {
        let generation = self.generation.load();
        let mut storages = generation.storages.iter().collect::<Vec<_>>();
        storages.sort_by(|left, right| left.0.cmp(right.0));
        storages.into_iter().map(|(_, storage)| storage.diagnostics()).collect()
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

    /// Shared model-facing surface used when top-level and delegated turns must
    /// address the same reload-safe plugin generation.
    pub fn control_tool_arcs(self: &Arc<Self>, security: Arc<SecurityPolicy>) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(PluginToolRouter {
                runtime: Arc::clone(self),
            }),
            Arc::new(PluginStatusTool {
                runtime: Arc::clone(self),
            }),
            Arc::new(PluginReloadTool {
                runtime: Arc::clone(self),
                security: Arc::clone(&security),
            }),
            Arc::new(PluginManageTool {
                runtime: Arc::clone(self),
                security,
            }),
        ]
    }

    /// Atomically rebuild every plugin adapter from the filesystem.
    pub async fn refresh_all(&self) -> PluginResult<u64> {
        let _reload_guard = self.reload_lock.lock().await;
        let old = self.generation.load_full();
        let next_id = old.id.saturating_add(1);
        let memory = self.memory.read().clone();
        let candidate = PluginGeneration::build(
            next_id,
            self.plugins_dir.clone(),
            self.policy.clone(),
            memory,
            Arc::clone(&self.event_bus),
        )
        .await?;
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
            self.policy.clone(),
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

struct RuntimeWasmChannel {
    runtime: Arc<PluginRuntime>,
    name: String,
}
impl RuntimeWasmChannel {
    fn current(&self) -> anyhow::Result<Arc<super::capabilities::channel::WasmChannel>> {
        self.runtime
            .generation
            .load()
            .channels
            .get(&self.name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("WASM channel is no longer active"))
    }
}
#[async_trait]
impl crate::channels::traits::Channel for RuntimeWasmChannel {
    fn name(&self) -> &str {
        &self.name
    }
    async fn send(&self, message: &crate::channels::traits::SendMessage) -> anyhow::Result<()> {
        self.current()?.send(message).await
    }
    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
    ) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { () = tx.closed() => return Ok(()), _ = tick.tick() => {} }
            for message in self.current()?.poll().await? {
                if tx.send(message).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
    async fn health_check(&self) -> bool {
        self.current().is_ok_and(|channel| !channel.is_quarantined())
    }
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
                "tool_adapters": self.runtime.tool_diagnostics(),
                "middleware_adapters": self.runtime.middleware_diagnostics(),
                "cron_adapters": self.runtime.cron_diagnostics(),
                "provider_adapters": self.runtime.provider_names(),
                "channel_diagnostics": self.runtime.channel_diagnostics(),
                "provider_adapter_status": self.runtime.provider_diagnostics(),
                "storage_adapters": self.runtime.storage_names(),
                "storage_adapter_status": self.runtime.storage_diagnostics(),
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
        let generation = self.runtime.generation_id();
        let quarantined = self.runtime.quarantined_adapters();
        if quarantined.is_empty() {
            return crate::capability::CapabilityAvailability::healthy(format!(
                "WASM runtime generation {generation} is published"
            ));
        }
        // The generation is still published and every other adapter still runs,
        // so the capability is selectable but no longer probe-healthy.
        crate::capability::CapabilityAvailability::ready(format!(
            "WASM runtime generation {generation} is published; quarantined adapters: {}",
            quarantined.join(", ")
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

/// The exact build an install or update is approved to trust.
struct CandidateIdentity {
    source: PathBuf,
    record: super::trust::TrustRecord,
}

impl PluginManageTool {
    fn required_string<'a>(args: &'a serde_json::Value, key: &str) -> anyhow::Result<&'a str> {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty '{key}' parameter"))
    }

    /// Optional reviewed hash. Absent or JSON null means the caller did not
    /// bind one. A present value must be a non-empty string.
    fn reviewed_hash<'a>(args: &'a serde_json::Value, key: &str) -> anyhow::Result<Option<&'a str>> {
        match args.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => {
                let text = value
                    .as_str()
                    .map(str::trim)
                    .ok_or_else(|| anyhow::anyhow!("'{key}' must be a string"))?;
                if text.is_empty() {
                    anyhow::bail!("'{key}' must be a non-empty string");
                }
                Ok(Some(text))
            }
        }
    }

    fn authorize(&self, action: &str, args: &serde_json::Value) -> Result<(), ToolResult> {
        let risk = match action {
            "status" | "get" | "middleware_test" | "provider_chat" | "storage_health" | "storage_recall"
            | "storage_count" => return Ok(()),
            "refresh" | "enable" | "disable" => ResourceRiskLevel::Low,
            "cron_run" | "storage_store" => ResourceRiskLevel::Medium,
            // Install, update and remove change which code the operator trusts.
            "install" | "update" | "remove" | "storage_forget" => ResourceRiskLevel::High,
            _ => return Ok(()),
        };
        self.authorize_operation(&format!("wasm_plugins_manage:{action}"), risk, args)
    }

    fn authorize_operation(
        &self,
        operation: &str,
        risk: ResourceRiskLevel,
        args: &serde_json::Value,
    ) -> Result<(), ToolResult> {
        let grant = ApprovalGrant::from_runtime_args("wasm_plugins_manage", args);
        SideEffectGate::new(&crate::tools::effective_autonomy::call_policy(&self.security))
            .authorize_resource_operation("wasm_plugins_manage", operation, risk, grant.as_ref())
            .map(|_| ())
            .map_err(|error| ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            })
    }

    /// Identify, read-only, the exact build an install or update would trust,
    /// and authorize that grant at high risk. The operation id names the plugin
    /// name, the component hash, the manifest hash and the granted permissions,
    /// so an approval cannot be reused for different bytes or a wider grant.
    fn authorize_trust_grant(&self, action: &str, args: &serde_json::Value) -> Result<CandidateIdentity, ToolResult> {
        let refuse = |error: String| ToolResult {
            success: false,
            output: String::new(),
            error: Some(error),
        };
        let source = Self::required_string(args, "source").map_err(|error| refuse(error.to_string()))?;
        let identity = self
            .candidate_identity(source, args)
            .map_err(|error| refuse(error.to_string()))?;
        // Bind the sorted list by digest: a joined, normalised string would
        // collide for ["a", "b"] and ["a_b"]. The readable list is added as its
        // own segment so the approval text shows what is being granted.
        let encoded = serde_json::to_string(&identity.record.permissions)
            .map_err(|error| refuse(format!("cannot encode the permission grant: {error}")))?;
        let permissions = crate::plugins::trust::sha256_hex(encoded.as_bytes());
        let readable = if identity.record.permissions.is_empty() {
            "grants_none".to_string()
        } else {
            format!("grants_{}", identity.record.permissions.join("_"))
        };
        let operation = op_id::op_id(
            "wasm_plugins_manage",
            &format!("{action}-trust"),
            &[
                &identity.record.name,
                &identity.record.sha256,
                &identity.record.manifest_sha256,
                &readable,
                &permissions,
            ],
        );
        self.authorize_operation(&operation, ResourceRiskLevel::High, args)?;
        Ok(identity)
    }

    fn candidate_identity(&self, source: &str, args: &serde_json::Value) -> anyhow::Result<CandidateIdentity> {
        let source = self.source_path(source)?;
        let material = PluginMaterial::read(&source)?;
        validate_plugin_name(&material.manifest.plugin.name)?;
        let grant = match args.get("grant_permissions") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow::anyhow!("'grant_permissions' must be an array of strings"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
            Some(_) => anyhow::bail!("'grant_permissions' must be an array of strings"),
        };
        let record = material.record_for(&grant)?;
        // The same read that will be trusted. A hash supplied by the caller
        // has to match these bytes; a later swap fails closed before any record.
        if let Some(expected) = Self::reviewed_hash(args, "expected_sha256")?
            && expected != record.sha256
        {
            anyhow::bail!("plugin hashes differ from the reviewed artifacts");
        }
        if let Some(expected) = Self::reviewed_hash(args, "expected_manifest_sha256")?
            && expected != record.manifest_sha256
        {
            anyhow::bail!("plugin hashes differ from the reviewed artifacts");
        }
        Ok(CandidateIdentity { source, record })
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
        self.runtime.resolve_install_source(raw)
    }

    /// Copy the approved source into a staging directory, prove the staged
    /// bytes are the approved ones, and prove they compile. Nothing is trusted
    /// yet.
    fn copy_candidate(&self, identity: &CandidateIdentity) -> anyhow::Result<PathBuf> {
        let source = identity.source.as_path();
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
            Ok(())
        })();
        if let Err(error) = copy_result {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(error);
        }
        let verified = (|| -> anyhow::Result<()> {
            let material = PluginMaterial::read(&stage)?;
            let staged = material.record_for(&identity.record.permissions)?;
            if staged != identity.record {
                anyhow::bail!("the plugin source changed after it was approved; nothing was installed");
            }
            // Compile before the active directory or the trust file changes.
            self.runtime.generation.load().manager.compile_candidate(&material)?;
            Ok(())
        })();
        if let Err(error) = verified {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(error);
        }
        Ok(stage)
    }

    async fn install_or_update(&self, identity: &CandidateIdentity, update: bool) -> anyhow::Result<serde_json::Value> {
        let stage = self.copy_candidate(identity)?;
        let name = identity.record.name.clone();
        let destination = self.plugin_path(&name)?;
        if update != destination.exists() {
            let _ = std::fs::remove_dir_all(&stage);
            if update {
                anyhow::bail!("plugin '{name}' is not installed");
            }
            anyhow::bail!("plugin '{name}' is already installed; use update");
        }

        // The operator's grant is recorded only now, after approval and a
        // successful compile, and is undone with the installation on failure.
        let previous_record = self.runtime.policy.record_for_name(&name).unwrap_or_else(|error| {
            tracing::warn!(plugin = %name, error = %error, "plugin trust file unreadable before install");
            None
        });
        let record = identity.record.clone();
        if let Err(error) = self.blocking_trust(move |policy| policy.record(record)).await {
            let _ = std::fs::remove_dir_all(&stage);
            anyhow::bail!("could not record the operator's trust for '{name}': {error}");
        }

        let backup = if destination.exists() {
            let moved = (|| -> anyhow::Result<PathBuf> {
                let backup_root = self.workspace_dir()?.join(".plugin-backups");
                std::fs::create_dir_all(&backup_root)?;
                let path = backup_root.join(format!("{name}-{}", uuid::Uuid::new_v4()));
                std::fs::rename(&destination, &path)?;
                Ok(path)
            })();
            match moved {
                Ok(path) => Some(path),
                Err(error) => {
                    self.restore_trust(&name, previous_record.clone()).await;
                    let _ = std::fs::remove_dir_all(&stage);
                    return Err(error);
                }
            }
        } else {
            None
        };
        if let Err(error) = std::fs::rename(&stage, &destination) {
            if let Some(backup) = &backup {
                let _ = std::fs::rename(backup, &destination);
            }
            self.restore_trust(&name, previous_record.clone()).await;
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
                self.restore_trust(&name, previous_record.clone()).await;
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

    /// Run a trust-file write on the blocking pool: it waits for a file lock
    /// and must never stall a runtime worker.
    async fn blocking_trust<T: Send + 'static>(
        &self,
        work: impl FnOnce(PluginPolicy) -> PluginResult<T> + Send + 'static,
    ) -> PluginResult<T> {
        let policy = self.runtime.policy.clone();
        tokio::task::spawn_blocking(move || work(policy))
            .await
            .map_err(|error| PluginError::Runtime(format!("trust file task failed: {error}")))?
    }

    async fn restore_trust(&self, name: &str, previous: Option<super::trust::TrustRecord>) {
        let owned = name.to_string();
        if let Err(error) = self
            .blocking_trust(move |policy| policy.restore(&owned, previous))
            .await
        {
            tracing::error!(plugin = %name, error = %error, "could not restore the plugin trust record");
        }
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
                anyhow::bail!("plugin '{name}' refresh failed and disable was rolled back: {error}");
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
        let prepared = self.runtime.generation.load().manager.prepare_plugin(&source)?;
        if let PluginStatus::Error(reason) = &prepared.status {
            anyhow::bail!("plugin '{name}' cannot be enabled: {reason}");
        }
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
        // Trust is recorded under the manifest name, which need not equal the
        // folder name; read it before the folder moves.
        let manifest_name = PluginManifest::from_file(&source.join("plugin.toml"))
            .ok()
            .map(|manifest| manifest.plugin.name);
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
                    anyhow::bail!("plugin '{name}' refresh failed and remove was rolled back: {error}");
                }
            }
        } else {
            None
        };
        // A removed plugin keeps no standing trust: a copy restored later must
        // be installed (and approved) again.
        // Trust is keyed by the manifest name; the folder name is used only
        // when the manifest cannot be read. Revoking the folder name as well
        // would drop an unrelated plugin that happens to carry that name.
        let trusted_name = manifest_name.unwrap_or_else(|| name.to_string());
        self.restore_trust(&trusted_name, None).await;
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

/// Tool-list summary for `wasm_plugins_manage`, kept beside the schema so the
/// contract test can hold both to the same standard without a live runtime.
pub(crate) const PLUGIN_MANAGE_DESCRIPTION: &str = "Inspect, manage and invoke workspace WASM plugins (tool, middleware, hook, cron, provider, storage). \
     Actions: 'status', 'get', 'refresh', 'install', 'update', 'enable', 'disable', 'remove', \
     'middleware_test', 'cron_run', 'provider_chat', 'storage_health', 'storage_store', \
     'storage_recall', 'storage_forget', 'storage_count'.";

/// Parameter schema for `wasm_plugins_manage`.
///
/// Kept as a free function so the schema contract can be asserted without
/// standing up a plugin runtime.
pub(crate) fn plugin_manage_parameters_schema() -> serde_json::Value {
    crate::tools::schema::publish_flat_schema(plugin_manage_execution_schema())
}

fn plugin_manage_execution_schema() -> &'static Arc<serde_json::Value> {
    static SCHEMA: std::sync::LazyLock<Arc<serde_json::Value>> = std::sync::LazyLock::new(|| {
        Arc::new({
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["status", "get", "refresh", "install", "update", "enable", "disable", "remove", "middleware_test", "cron_run", "provider_chat", "storage_health", "storage_store", "storage_recall", "storage_forget", "storage_count"],
                        "description": "Inspect with 'status'/'get'; reload the workspace with 'refresh'; manage a plugin \
                                        with 'install'/'update'/'enable'/'disable'/'remove'; exercise an exported adapter with \
                                        'middleware_test' (middleware stage), 'cron_run' (cron handler), 'provider_chat' (provider) \
                                        or the storage actions 'storage_health'/'storage_store'/'storage_recall'/'storage_forget'/'storage_count'."
                    },
                    "name": {"type": "string", "minLength": 1, "description": "Plugin name or exported adapter name; required by 'get'/'enable'/'disable'/'remove'/'cron_run'/'provider_chat' and by every storage action ('storage_health'/'storage_store'/'storage_recall'/'storage_forget'/'storage_count')."},
                    "source": {"type": "string", "description": "'install'/'update': plugin source directory in the workspace."},
                    "grant_permissions": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "'install'/'update': host permissions the operator grants (must include every required permission of the plugin and only ones it declares). The approval is bound to the plugin name, WASM hash, manifest hash and this list; omitted means none."
                    },
                    "expected_sha256": {
                        "type": "string",
                        "description": "'install'/'update': component hash the operator reviewed. When present, it must equal the component bytes read in the call that records trust."
                    },
                    "expected_manifest_sha256": {
                        "type": "string",
                        "description": "'install'/'update': manifest hash the operator reviewed. When present, it must equal the manifest bytes read in the call that records trust."
                    },
                    "stage": {"type": "string", "enum": ["inbound", "outbound", "llm_request", "llm_response"], "description": "'middleware_test': pipeline stage to exercise."},
                    "data": {"description": "'middleware_test': the envelope passed to the stage."},
                    "message": {"type": "string", "description": "'provider_chat': single user message."},
                    "messages": {
                        "type": "array",
                        "description": "'provider_chat': full message list; alternative to 'message'.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": {"type": "string", "enum": ["system", "user", "assistant", "tool"]},
                                "content": {"type": "string"}
                            },
                            "required": ["role", "content"],
                            "additionalProperties": false
                        }
                    },
                    "model": {"type": "string", "description": "'provider_chat': model override."},
                    "temperature": {"type": "number", "description": "'provider_chat': sampling temperature."},
                    "key": {"type": "string", "description": "Storage key; required by 'storage_store' and 'storage_forget'."},
                    "content": {"type": "string", "description": "Value to store; required by 'storage_store'."},
                    "category": {"type": "string", "description": "Optional storage category for 'storage_store'/'storage_recall'/'storage_count'."},
                    "session_id": {"type": "string", "description": "Optional storage session scope for 'storage_store'/'storage_recall'/'storage_count'."},
                    "query": {"type": "string", "description": "'storage_recall': search query."},
                    "limit": {"type": "integer", "minimum": 1, "description": "'storage_recall': maximum number of results."}
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
        })
    });
    &SCHEMA
}

#[async_trait]
impl Tool for PluginManageTool {
    fn name(&self) -> &str {
        "wasm_plugins_manage"
    }

    fn description(&self) -> &str {
        PLUGIN_MANAGE_DESCRIPTION
    }

    fn parameters_schema(&self) -> serde_json::Value {
        plugin_manage_parameters_schema()
    }

    fn execution_parameters_schema(&self) -> Option<Arc<serde_json::Value>> {
        Some(Arc::clone(plugin_manage_execution_schema()))
    }

    async fn execute(&self, mut args: serde_json::Value) -> anyhow::Result<ToolResult> {
        crate::tools::schema::validate_execution_contract(self.name(), plugin_manage_execution_schema(), &mut args)?;
        let action = Self::required_string(&args, "action")?;
        let identity = if matches!(action, "install" | "update") {
            match self.authorize_trust_grant(action, &args) {
                Ok(identity) => Some(identity),
                Err(result) => return Ok(result),
            }
        } else {
            if let Err(result) = self.authorize(action, &args) {
                return Ok(result);
            }
            None
        };
        let result: anyhow::Result<serde_json::Value> = match action {
            "status" => Ok(serde_json::json!({
                "generation": self.runtime.generation_id(),
                "active": self.active_inventory().await?,
                "disabled": self.disabled_plugins()?,
                "hook_adapters": self.runtime.hook_diagnostics(),
                "tool_adapters": self.runtime.tool_diagnostics(),
                "middleware_adapters": self.runtime.middleware_diagnostics(),
                "cron_adapters": self.runtime.cron_diagnostics(),
                "provider_adapters": self.runtime.provider_names(),
                "channel_diagnostics": self.runtime.channel_diagnostics(),
                "provider_adapter_status": self.runtime.provider_diagnostics(),
                "storage_adapters": self.runtime.storage_names(),
                "storage_adapter_status": self.runtime.storage_diagnostics(),
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
            "install" | "update" => match &identity {
                Some(identity) => self.install_or_update(identity, action == "update").await,
                None => Err(anyhow::anyhow!("{action} was not authorized")),
            },
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
        if let Err(error) = SideEffectGate::new(&crate::tools::effective_autonomy::call_policy(&self.security))
            .authorize_resource_operation(
                self.name(),
                &operation_name,
                ResourceRiskLevel::Low,
                approval_grant.as_ref(),
            )
        {
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
        let mut tool_schema = serde_json::json!({
            "type": "string",
            "description": "Tool name exported by the active WASM plugin generation"
        });
        if !names.is_empty()
            && let Some(object) = tool_schema.as_object_mut()
        {
            object.insert("enum".to_string(), serde_json::json!(names));
        }
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool": tool_schema,
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
        let quarantined = generation
            .wasm_tools
            .iter()
            .filter(|tool| tool.is_quarantined())
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();
        if quarantined.is_empty() {
            return crate::capability::CapabilityAvailability::healthy(format!(
                "WASM generation {} exposes {} executable tool backend(s)",
                generation.id,
                generation.tools.len()
            ));
        }
        // Quarantined tools stay listed and answer with an explicit tool
        // error, so the router is still selectable but no longer healthy.
        crate::capability::CapabilityAvailability::ready(format!(
            "WASM generation {} exposes {} executable tool backend(s); quarantined: {}",
            generation.id,
            generation.tools.len(),
            quarantined.join(", ")
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

/// Return the one process-level runtime for the configured workspace, creating
/// it once. `None` when `[plugins] enabled` is false (nothing is scanned or
/// loaded then), or when the state directory cannot hold the trust file safely.
pub async fn init_plugin_runtime(
    config: &crate::config::Config,
    memory: Option<Arc<dyn Memory>>,
) -> Option<Arc<PluginRuntime>> {
    if !config.plugins.enabled {
        tracing::debug!("WASM plugins are disabled ([plugins] enabled = false); nothing is loaded");
        return None;
    }
    let policy = match PluginPolicy::from_config(config) {
        Ok(policy) => policy,
        Err(error) => {
            tracing::warn!(error = %error, "WASM plugins stay off: the plugin trust location is unsafe");
            return None;
        }
    };
    init_plugin_runtime_with_policy(&config.workspace_dir, policy, memory).await
}

/// [`init_plugin_runtime`] with an explicit policy.
pub async fn init_plugin_runtime_with_policy(
    workspace_dir: &Path,
    policy: PluginPolicy,
    memory: Option<Arc<dyn Memory>>,
) -> Option<Arc<PluginRuntime>> {
    if !policy.enabled() {
        return None;
    }
    let key = std::fs::canonicalize(workspace_dir).unwrap_or_else(|_| workspace_dir.to_path_buf());
    let runtimes = PROCESS_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut runtimes = runtimes.lock().await;
    if let Some(runtime) = runtimes.get(&key).and_then(Weak::upgrade) {
        return Some(runtime);
    }

    match PluginRuntime::new(&key, policy, memory).await {
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

/// Test policy: plugins on, trust file in `<workspace>/.prx-state` (outside
/// `<workspace>/plugins`).
#[cfg(test)]
pub(crate) fn test_policy(workspace: &Path) -> PluginPolicy {
    PluginPolicy::new(true, workspace.join(".prx-state").join(super::trust::TRUST_FILE_NAME))
}

/// Test stand-in for the operator: trust every plugin directory that is in the
/// workspace right now with exactly the permissions its manifest requires.
#[cfg(test)]
pub(crate) fn trust_workspace_plugins(workspace: &Path) {
    let policy = test_policy(workspace);
    let Ok(entries) = std::fs::read_dir(workspace.join("plugins")) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        if let Ok(material) = PluginMaterial::read(&entry.path()) {
            let granted = material.manifest.permissions.required.clone();
            if let Ok(record) = material.record_for(&granted) {
                policy.record(record).expect("test: write the trust file");
            }
        }
    }
}

/// Runtime over a test workspace whose current plugins the operator trusted.
#[cfg(test)]
pub(crate) async fn init_test_plugin_runtime(
    workspace: &Path,
    memory: Option<Arc<dyn Memory>>,
) -> Option<Arc<PluginRuntime>> {
    trust_workspace_plugins(workspace);
    init_plugin_runtime_with_policy(workspace, test_policy(workspace), memory).await
}

#[cfg(test)]
mod tests;
