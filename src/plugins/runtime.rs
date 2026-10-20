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
            wasm_tools,
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
        SideEffectGate::new(self.security.as_ref())
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
                    return Err(error.into());
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

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
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
mod tests {
    use super::*;
    use crate::plugins::trust::TRUST_FILE_NAME;
    use tempfile::TempDir;

    /// Runtime over a workspace whose current plugins the operator trusted.
    async fn init_plugin_runtime(workspace: &Path, memory: Option<Arc<dyn Memory>>) -> Option<Arc<PluginRuntime>> {
        init_test_plugin_runtime(workspace, memory).await
    }

    /// `wasm_plugins_manage` dispatches on `action` like `cron` and `xin`, so it
    /// owes the same contract: every parameter an action cannot be called
    /// without has to name that action, and no parameter may ship undescribed.
    #[test]
    fn manage_schema_names_every_action_in_the_parameters_it_requires() {
        crate::tools::schema::action_contract::assert_action_tool_contract(
            "wasm_plugins_manage",
            PLUGIN_MANAGE_DESCRIPTION,
            &plugin_manage_parameters_schema(),
        );
    }

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
        assert!(
            router.parameters_schema().pointer("/properties/tool/enum").is_none(),
            "an empty plugin generation must not publish an invalid empty enum"
        );

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

        let message_items = schema
            .pointer("/properties/messages/items")
            .and_then(serde_json::Value::as_object)
            .expect("messages items should be an object schema");
        let message_properties = message_items
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("messages items should define its required properties");
        for required in ["role", "content"] {
            assert!(
                message_properties.contains_key(required),
                "required message property '{required}' must be defined"
            );
        }
        assert_eq!(
            message_properties.get("role").and_then(|value| value.get("type")),
            Some(&serde_json::json!("string"))
        );
        assert_eq!(
            message_properties.get("content").and_then(|value| value.get("type")),
            Some(&serde_json::json!("string"))
        );
    }

    #[tokio::test]
    async fn plugin_instantiation_arms_epoch_deadline_after_engine_has_advanced() {
        let temp = TempDir::new().unwrap();
        install_example_fixture(temp.path(), "pipeline-middleware");

        // A Store created with epoch interruption starts with deadline zero.
        // Advancing the shared engine makes a missing pre-instantiation deadline
        // fail deterministically with `wasm trap: interrupt`.
        crate::plugins::shared_wasm_engine()
            .expect("test: shared wasm engine")
            .increment_epoch();

        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let output = runtime
            .middleware()
            .process(
                super::super::capabilities::middleware::MiddlewareStage::Outbound,
                r#"{"text":"epoch-ready"}"#,
            )
            .await;
        assert!(output.contains("[wasm-middleware] epoch-ready"), "{output}");
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
                "source": "plugin-sources/voice-talk-realtime",
                "grant_permissions": ["log", "config", "kv"]
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

        // The committed fixture is scheduled every 30 s and is due at start-up,
        // so the runtime's own scheduler may run it between these manual runs
        // (every run is its own task and yields); the counter still has to
        // advance strictly from one manual run to the next.
        let run_number = |output: String| -> u64 {
            output
                .strip_prefix("counter-cron-run-")
                .and_then(|number| number.parse().ok())
                .unwrap_or_else(|| panic!("unexpected cron output: {output}"))
        };
        let first = run_number(runtime.cron().run_job("counter-cron").await.unwrap());
        let second = run_number(runtime.cron().run_job("counter-cron").await.unwrap());
        assert!(second > first, "run {second} must follow run {first}");

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

    /// The committed `pipeline-middleware` component is reused verbatim; only
    /// its `[resources]` table changes, so no WASM guest has to be rebuilt to
    /// drive a store-poisoning trap.
    const MIDDLEWARE_RECOVERY_STAGE: super::super::capabilities::middleware::MiddlewareStage =
        super::super::capabilities::middleware::MiddlewareStage::Outbound;

    /// Keeps the fixture healthy for ordinary envelopes while leaving one
    /// deterministic trap reachable: the guest DOM-parses the whole envelope, so
    /// a multi-megabyte envelope cannot fit an 8 MB linear memory and
    /// `trap_on_grow_failure` turns the failed `memory.grow` into a trap.
    /// `max_payload_bytes` is the largest value `validate()` accepts for an 8 MB
    /// guest, which keeps this trap reachable past the host-side oversize guard
    /// so the trap, not the guard, stays the subject of these tests.
    const TRAPPING_MIDDLEWARE_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608";

    /// Same guest memory and timeout, host-side oversize guard left at its
    /// derived default (`max_memory_mb` / 8 = 1 MiB here).
    const GUARDED_MIDDLEWARE_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000";

    /// Fuel the `middleware_fuel_is_metered_per_invocation` test grants: the
    /// measured cost of instantiating the fixture and serving one call, plus
    /// half again. Fuel cost differs per platform and wasmtime version
    /// (instantiation is tens of thousands of units on one target and nearly
    /// free on another), so no figure is hard-coded; see
    /// `measure_one_call_fuel`.
    fn funded_middleware_fuel(measured_one_call_fuel: u64) -> u64 {
        measured_one_call_fuel.saturating_add(measured_one_call_fuel / 2)
    }

    /// Outcome of loading the middleware fixture under `max_fuel` and sending
    /// it one envelope.
    #[derive(Debug, PartialEq, Eq)]
    enum FuelProbe {
        /// The adapter loaded and the call returned the transformed text.
        Funded,
        /// The adapter never loaded because the budget ran out.
        StarvedAtLoad,
        /// The adapter loaded but the call did not return the transformed text.
        StarvedAtCall,
    }

    async fn probe_middleware_fuel(max_fuel: u64) -> FuelProbe {
        let temp = TempDir::new().unwrap();
        install_middleware_fixture_with_resources(
            temp.path(),
            &format!("max_fuel = {max_fuel}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
        );
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        if !runtime.adapter_errors().is_empty() {
            assert!(
                runtime
                    .adapter_errors()
                    .iter()
                    .any(|error| error.contains("all fuel consumed")),
                "the adapter must be missing because the budget ran out, not for another reason \
                 (max_fuel {max_fuel}): {:?}",
                runtime.adapter_errors()
            );
            return FuelProbe::StarvedAtLoad;
        }
        let output = runtime
            .middleware()
            .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
            .await;
        if transformed_middleware_text(&output).as_deref() == Some("[wasm-middleware] hello") {
            FuelProbe::Funded
        } else {
            FuelProbe::StarvedAtCall
        }
    }

    /// Measures, on the current platform and engine, the smallest `max_fuel`
    /// that instantiates the fixture and serves one `process` call, as the
    /// boundary between a starved and a funded probe. Returns
    /// `(largest_starved, smallest_funded)`, at most `PRECISION` apart. Fuel
    /// cost is deterministic for a given engine and guest, so the probe is
    /// monotonic and the search needs no retries.
    async fn measure_one_call_fuel() -> (u64, u64) {
        const PRECISION: u64 = 64;
        const CEILING: u64 = 1_000_000_000;
        let mut starved = 0u64;
        let mut funded = 1_024u64;
        while probe_middleware_fuel(funded).await != FuelProbe::Funded {
            assert!(
                funded < CEILING,
                "one middleware call is not funded even by {CEILING} fuel"
            );
            starved = funded;
            funded = funded.saturating_mul(2);
        }
        while funded - starved > PRECISION {
            let middle = starved + (funded - starved) / 2;
            if probe_middleware_fuel(middle).await == FuelProbe::Funded {
                funded = middle;
            } else {
                starved = middle;
            }
        }
        (starved, funded)
    }

    /// The per-invocation fuel limit is real and discriminating, on every
    /// platform. Fuel costs differ per target and wasmtime version, so the
    /// oracle is measured here rather than hard-coded:
    ///
    /// 1. `measure_one_call_fuel` finds the boundary between a budget that
    ///    cannot instantiate the fixture and serve one call and a budget that
    ///    can.
    /// 2. A budget just below the boundary must fail, either because the
    ///    adapter runs out of fuel while loading (`all fuel consumed`) or, when
    ///    instantiation is nearly free, because the call itself runs out. In
    ///    both cases the call must not return the transformed text.
    /// 3. A budget half again above the boundary must serve several calls in
    ///    a row; each call that succeeds after the first proves fuel is reset
    ///    per invocation, since the sum of the calls exceeds that budget.
    ///
    /// The old oracle asserted that instantiation alone costs over 48 000
    /// fuel, which holds on aarch64 but not on x86_64.
    #[tokio::test]
    async fn the_per_invocation_fuel_bound_stays_discriminating() {
        let (starved_fuel, funded_fuel) = measure_one_call_fuel().await;
        assert!(starved_fuel > 0, "a zero-fuel budget must not fund a call");

        let temp = TempDir::new().unwrap();
        install_middleware_fixture_with_resources(
            temp.path(),
            &format!("max_fuel = {starved_fuel}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
        );
        let starved = init_plugin_runtime(temp.path(), None).await.unwrap();
        if starved.adapter_errors().is_empty() {
            let output = starved
                .middleware()
                .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
                .await;
            assert_ne!(
                transformed_middleware_text(&output).as_deref(),
                Some("[wasm-middleware] hello"),
                "a budget below the measured boundary ({starved_fuel}..{funded_fuel}) must not fund a call: {output}"
            );
        } else {
            assert!(
                starved.middleware_diagnostics().is_empty(),
                "a starved load must publish no middleware: {:?}",
                starved.middleware_diagnostics()
            );
            assert!(
                starved
                    .adapter_errors()
                    .iter()
                    .any(|error| error.contains("all fuel consumed")),
                "the adapter must be missing because the budget ran out, not for another reason: {:?}",
                starved.adapter_errors()
            );
        }

        let budget = funded_middleware_fuel(funded_fuel);
        let temp = TempDir::new().unwrap();
        install_middleware_fixture_with_resources(
            temp.path(),
            &format!("max_fuel = {budget}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
        );
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        for call in 1..=8 {
            let output = runtime
                .middleware()
                .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
                .await;
            assert_eq!(
                transformed_middleware_text(&output).as_deref(),
                Some("[wasm-middleware] hello"),
                "call {call} under {budget} fuel (boundary {funded_fuel}) must succeed: {output}"
            );
        }
    }

    fn install_middleware_fixture_with_resources(workspace: &Path, resources: &str) {
        let plugin_dir = workspace.join("plugins").join("pipeline-middleware");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/pipeline-middleware");
        std::fs::copy(fixture.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.toml"),
            format!(
                r#"[plugin]
name = "pipeline-middleware"
version = "0.1.0"
description = "middleware recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "middleware"
name = "pipeline-marker"
priority = 10

[permissions]
required = []
optional = []

[resources]
{resources}
"#
            ),
        )
        .unwrap();
    }

    /// Four megabytes of envelope: three live copies inside the guest (argument,
    /// parsed value, serialized result) cannot fit an 8 MB linear memory.
    fn oversized_middleware_envelope() -> String {
        serde_json::json!({"text": "hello", "filler": "x".repeat(4 * 1024 * 1024)}).to_string()
    }

    fn middleware_adapter_status(runtime: &PluginRuntime) -> serde_json::Value {
        let mut adapters = runtime.middleware_diagnostics();
        assert_eq!(adapters.len(), 1, "{adapters:?}");
        adapters.remove(0)
    }

    fn adapter_counter(adapter: &serde_json::Value, field: &str) -> u64 {
        adapter
            .get(field)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| panic!("adapter status must publish '{field}': {adapter}"))
    }

    fn adapter_text(adapter: &serde_json::Value, field: &str) -> String {
        adapter
            .get(field)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("adapter status must publish '{field}': {adapter}"))
            .to_string()
    }

    fn transformed_middleware_text(output: &str) -> Option<String> {
        serde_json::from_str::<serde_json::Value>(output)
            .ok()?
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
    }

    #[tokio::test]
    async fn middleware_recovers_on_a_fresh_store_after_a_guest_trap() {
        let temp = TempDir::new().unwrap();
        install_middleware_fixture_with_resources(temp.path(), TRAPPING_MIDDLEWARE_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

        let trapping = oversized_middleware_envelope();
        let trapped = runtime.middleware().process(MIDDLEWARE_RECOVERY_STAGE, &trapping).await;
        assert!(
            !trapped.contains("middleware_probe"),
            "the oversized envelope must trap the guest"
        );

        let recovered = runtime
            .middleware()
            .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
            .await;
        assert_eq!(
            transformed_middleware_text(&recovered).as_deref(),
            Some("[wasm-middleware] hello"),
            "{recovered}"
        );

        let adapter = middleware_adapter_status(&runtime);
        assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 1, "{adapter}");
        assert_eq!(
            adapter_counter(&adapter, "failure_count"),
            1,
            "the healthy call must not have entered the poisoned store: {adapter}"
        );
        let last_error = adapter_text(&adapter, "last_error");
        assert!(
            !last_error.contains("cannot enter component instance"),
            "the recorded reason must be the first trap, not a re-entry refusal: {last_error}"
        );
        assert!(
            adapter
                .get("last_error_at")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "{adapter}"
        );
    }

    #[tokio::test]
    async fn middleware_quarantines_after_the_restart_budget_is_spent() {
        let temp = TempDir::new().unwrap();
        install_middleware_fixture_with_resources(temp.path(), TRAPPING_MIDDLEWARE_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

        let trapping = oversized_middleware_envelope();
        for _ in 0..5 {
            let output = runtime.middleware().process(MIDDLEWARE_RECOVERY_STAGE, &trapping).await;
            assert_eq!(output.len(), trapping.len(), "every trapping call must fail open");
        }

        let adapter = middleware_adapter_status(&runtime);
        assert_eq!(adapter_text(&adapter, "state"), "quarantined", "{adapter}");
        assert!(!adapter_text(&adapter, "last_error").is_empty(), "{adapter}");
        assert_eq!(
            adapter_counter(&adapter, "restart_count"),
            3,
            "quarantine must follow a spent budget, not a shortcut around it: {adapter}"
        );
        let failures = adapter_counter(&adapter, "failure_count");
        let skipped = adapter_counter(&adapter, "skipped_quarantined");

        let refused = runtime
            .middleware()
            .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
            .await;
        assert_eq!(refused, r#"{"text":"hello"}"#, "a quarantined adapter still fails open");
        let after = middleware_adapter_status(&runtime);
        assert_eq!(adapter_text(&after, "state"), "quarantined", "{after}");
        assert_eq!(
            adapter_counter(&after, "failure_count"),
            failures,
            "a refused call must not be counted as a new guest failure: {after}"
        );
        assert_eq!(
            adapter_counter(&after, "skipped_quarantined"),
            skipped + 1,
            "a refused call must be visible as a quarantined skip: {after}"
        );

        let status = runtime.status_tool();
        assert_ne!(
            status.availability().level,
            crate::capability::CapabilityAvailabilityLevel::Healthy,
            "availability must degrade while an adapter is quarantined"
        );
        let reported = status.execute(serde_json::json!({})).await.unwrap();
        assert!(reported.output.contains("\"quarantined\""), "{}", reported.output);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_middleware_waiter_never_enters_a_poisoned_store() {
        let temp = TempDir::new().unwrap();
        install_middleware_fixture_with_resources(temp.path(), TRAPPING_MIDDLEWARE_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

        let chain = runtime.middleware();
        let trapping = oversized_middleware_envelope();
        // The waiter has to queue behind the trapping caller for this test to
        // exercise anything, so the trapping task announces that it is running
        // before it takes the store lock.
        let (entered, has_entered) = tokio::sync::oneshot::channel();
        let trapping_call = tokio::spawn({
            let chain = Arc::clone(&chain);
            async move {
                let _ = entered.send(());
                chain.process(MIDDLEWARE_RECOVERY_STAGE, &trapping).await
            }
        });
        has_entered.await.unwrap();
        // `MiddlewareChain::process` copies the envelope before it takes the
        // store lock, so the trapping caller carries a four-megabyte copy (well
        // under a millisecond) that the healthy caller does not. The delay only
        // has to cover that copy: landing late is harmless, because a waiter
        // that arrives after the trap has already been recorded still finds the
        // adapter dead and still performs exactly one rebuild.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let healthy_call = tokio::spawn({
            let chain = Arc::clone(&chain);
            async move { chain.process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#).await }
        });

        let trapped = trapping_call.await.unwrap();
        let healthy = healthy_call.await.unwrap();
        assert!(
            !trapped.contains("middleware_probe"),
            "the oversized envelope must trap"
        );
        assert_eq!(
            transformed_middleware_text(&healthy).as_deref(),
            Some("[wasm-middleware] hello"),
            "the queued waiter must run against a live store: {healthy}"
        );

        let adapter = middleware_adapter_status(&runtime);
        assert_eq!(
            adapter_counter(&adapter, "failure_count"),
            1,
            "only the trapping caller may record a failure: {adapter}"
        );
        // Exactly one: the waiter found the store dead and rebuilt it. Zero
        // would mean the healthy caller won the lock and nothing ever queued,
        // which is the ordering this test exists to rule out.
        assert_eq!(
            adapter_counter(&adapter, "restart_count"),
            1,
            "the waiter must have queued behind the trapping caller and rebuilt once: {adapter}"
        );
        assert!(
            !adapter_text(&adapter, "last_error").contains("cannot enter component instance"),
            "{adapter}"
        );
    }

    #[tokio::test]
    async fn middleware_skips_a_payload_that_cannot_fit_the_guest() {
        let temp = TempDir::new().unwrap();
        install_middleware_fixture_with_resources(temp.path(), GUARDED_MIDDLEWARE_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

        let oversize = oversized_middleware_envelope();
        let passed_through = runtime.middleware().process(MIDDLEWARE_RECOVERY_STAGE, &oversize).await;
        assert_eq!(passed_through, oversize, "an over-cap payload passes through unchanged");

        let adapter = middleware_adapter_status(&runtime);
        assert_eq!(adapter_counter(&adapter, "skipped_oversize"), 1, "{adapter}");
        assert_eq!(
            adapter_counter(&adapter, "skipped_quarantined"),
            0,
            "an oversize skip is not a quarantine: {adapter}"
        );
        assert_eq!(
            adapter_counter(&adapter, "failure_count"),
            0,
            "an over-cap payload must never reach the guest: {adapter}"
        );
        assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
        assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
        assert_eq!(adapter.get("last_error"), Some(&serde_json::Value::Null), "{adapter}");

        let next = runtime
            .middleware()
            .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
            .await;
        assert_eq!(
            transformed_middleware_text(&next).as_deref(),
            Some("[wasm-middleware] hello"),
            "{next}"
        );
    }

    #[tokio::test]
    async fn middleware_fuel_is_metered_per_invocation() {
        let (_, funded_fuel) = measure_one_call_fuel().await;
        let one_call_fuel = funded_middleware_fuel(funded_fuel);
        let temp = TempDir::new().unwrap();
        install_middleware_fixture_with_resources(
            temp.path(),
            &format!("max_fuel = {one_call_fuel}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
        );
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

        for call in 1..=3 {
            let output = runtime
                .middleware()
                .process(MIDDLEWARE_RECOVERY_STAGE, r#"{"text":"hello"}"#)
                .await;
            assert_eq!(
                transformed_middleware_text(&output).as_deref(),
                Some("[wasm-middleware] hello"),
                "call {call} exhausted a store-lifetime fuel budget: {output}"
            );
        }

        let adapter = middleware_adapter_status(&runtime);
        assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
        assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
    }

    /// Install `fixture/plugin.wasm` under `plugins/<name>` with a test-written
    /// manifest, so a committed component can be driven into a trap by its
    /// `[resources]` table alone.
    fn install_fixture_with_manifest(workspace: &Path, name: &str, fixture: &str, manifest: &str) {
        let plugin_dir = workspace.join("plugins").join(name);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture);
        std::fs::copy(source.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).unwrap();
        std::fs::write(plugin_dir.join("plugin.toml"), manifest).unwrap();
    }

    /// `adapter_key` of `wasm_plugins_status`, which is where operators and the
    /// model read adapter health.
    async fn status_adapters(runtime: &Arc<PluginRuntime>, adapter_key: &str) -> Vec<serde_json::Value> {
        let status = runtime.status_tool().execute(serde_json::json!({})).await.unwrap();
        let status: serde_json::Value = serde_json::from_str(&status.output).unwrap();
        status
            .get(adapter_key)
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_else(|| panic!("wasm_plugins_status must publish '{adapter_key}': {status}"))
    }

    async fn single_status_adapter(runtime: &Arc<PluginRuntime>, adapter_key: &str) -> serde_json::Value {
        let mut adapters = status_adapters(runtime, adapter_key).await;
        assert_eq!(adapters.len(), 1, "{adapters:?}");
        adapters.remove(0)
    }

    fn voice_tool_manifest(resources: &str) -> String {
        format!(
            r#"[plugin]
name = "voice-talk-realtime"
version = "0.1.0"
description = "tool recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "tool"
name = "voice_session"

[permissions]
required = ["log", "config", "kv"]
optional = []

[resources]
{resources}
"#
        )
    }

    fn install_voice_tool(workspace: &Path, resources: &str) {
        install_fixture_with_manifest(
            workspace,
            "voice-talk-realtime",
            "plugins/voice-talk-realtime",
            &voice_tool_manifest(resources),
        );
    }

    /// The guest parses its arguments into a DOM and echoes `instructions`
    /// into its result, so four megabytes of instructions cannot fit an 8 MB
    /// linear memory. `max_payload_bytes` keeps the call past the host-side
    /// guard so the trap stays the subject of the test.
    const TRAPPING_TOOL_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608";

    /// Same guest memory, host-side guard at its derived default (1 MiB here).
    const GUARDED_TOOL_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000";

    fn trapping_tool_args() -> serde_json::Value {
        serde_json::json!({"provider": "openai", "instructions": "x".repeat(4 * 1024 * 1024)})
    }

    async fn call_voice_tool(runtime: &Arc<PluginRuntime>, args: serde_json::Value) -> ToolResult {
        runtime
            .tool_router()
            .execute_named("voice_session", args)
            .await
            .unwrap()
    }

    async fn assert_voice_tool_healthy(runtime: &Arc<PluginRuntime>, context: &str) {
        let result = call_voice_tool(runtime, serde_json::json!({"provider": "openai"})).await;
        assert!(result.success, "{context}: {:?}", result.error);
        assert!(
            result.output.contains("wss://api.openai.com"),
            "{context}: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn tool_recovers_on_a_fresh_store_after_a_guest_trap() {
        let temp = TempDir::new().unwrap();
        install_voice_tool(temp.path(), TRAPPING_TOOL_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        assert_voice_tool_healthy(&runtime, "before the trap").await;

        let trapped = call_voice_tool(&runtime, trapping_tool_args()).await;
        assert!(!trapped.success, "four megabytes of instructions must trap the guest");

        assert_voice_tool_healthy(&runtime, "the call after the trap").await;

        let adapter = single_status_adapter(&runtime, "tool_adapters").await;
        assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
        assert_eq!(adapter_text(&adapter, "tool"), "voice_session", "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 1, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "failure_count"), 1, "{adapter}");
        assert!(
            !adapter_text(&adapter, "last_error").contains("cannot enter component instance"),
            "the recorded reason must be the trap, not a re-entry refusal: {adapter}"
        );
    }

    #[tokio::test]
    async fn tool_quarantines_after_the_restart_budget_is_spent() {
        let temp = TempDir::new().unwrap();
        install_voice_tool(temp.path(), TRAPPING_TOOL_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let (_, mut quarantined_events) = runtime
            .event_bus()
            .subscribe("tool-recovery-test", "prx.plugin.tool_quarantined")
            .await
            .unwrap();

        for call in 1..=5 {
            let result = call_voice_tool(&runtime, trapping_tool_args()).await;
            assert!(!result.success, "trapping call {call} must fail, never report success");
        }

        let adapter = single_status_adapter(&runtime, "tool_adapters").await;
        assert_eq!(adapter_text(&adapter, "state"), "quarantined", "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 3, "{adapter}");
        let skipped = adapter_counter(&adapter, "skipped_quarantined");

        let refused = call_voice_tool(&runtime, serde_json::json!({"provider": "openai"})).await;
        assert!(!refused.success, "a quarantined tool must return a tool error");
        assert!(
            refused.error.as_deref().unwrap_or_default().contains("quarantined"),
            "{refused:?}"
        );
        let after = single_status_adapter(&runtime, "tool_adapters").await;
        assert_eq!(adapter_counter(&after, "skipped_quarantined"), skipped + 1, "{after}");

        let event = quarantined_events
            .try_recv()
            .expect("test: one quarantine event must have been published");
        let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
        assert_eq!(
            payload.get("capability").and_then(serde_json::Value::as_str),
            Some("tool")
        );
        assert_eq!(
            payload.get("plugin").and_then(serde_json::Value::as_str),
            Some("voice-talk-realtime")
        );

        assert!(
            runtime.tool_router().supports_name("voice_session"),
            "a quarantined tool stays listed so the model is told why it fails"
        );
        assert_ne!(
            runtime.tool_router().availability().level,
            crate::capability::CapabilityAvailabilityLevel::Healthy
        );
        assert_ne!(
            runtime.status_tool().availability().level,
            crate::capability::CapabilityAvailabilityLevel::Healthy
        );
    }

    #[tokio::test]
    async fn tool_refuses_arguments_that_cannot_fit_the_guest() {
        let temp = TempDir::new().unwrap();
        install_voice_tool(temp.path(), GUARDED_TOOL_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

        let refused = call_voice_tool(&runtime, trapping_tool_args()).await;
        assert!(!refused.success, "an over-cap call is a tool error, not a success");
        assert!(
            refused
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("max_payload_bytes"),
            "{refused:?}"
        );

        let adapter = single_status_adapter(&runtime, "tool_adapters").await;
        assert_eq!(adapter_counter(&adapter, "skipped_oversize"), 1, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
        assert_voice_tool_healthy(&runtime, "the call after an oversize refusal").await;
    }

    /// Measured against the committed `voice-talk-realtime` component with a
    /// store-lifetime budget: instantiation plus `get-spec` fits in 100 000
    /// fuel units with no call left, 150 000 and 200 000 fund exactly one
    /// `execute`, and each further call costs roughly 50 000. 175 000 sits in
    /// the middle of the one-call band.
    const ONE_TOOL_CALL_FUEL: u64 = 175_000;

    #[tokio::test]
    async fn tool_fuel_is_metered_per_invocation() {
        let temp = TempDir::new().unwrap();
        install_voice_tool(
            temp.path(),
            &format!("max_fuel = {ONE_TOOL_CALL_FUEL}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
        );
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        for call in 1..=3 {
            assert_voice_tool_healthy(&runtime, &format!("call {call} under a per-call fuel budget")).await;
        }
    }

    fn audit_hook_manifest(resources: &str) -> String {
        format!(
            r#"[plugin]
name = "audit-hook"
version = "0.1.0"
description = "hook recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "hook"
name = "audit"

[[capabilities.events]]
pattern = "prx.lifecycle.*"

[permissions]
required = ["log", "kv", "events"]
optional = []

[resources]
{resources}
"#
        )
    }

    /// `audit-hook` needs `events`, which is not auto-granted, so the fixture
    /// is installed as a managed plugin like the documented-hook test does.
    fn install_audit_hook(workspace: &Path, resources: &str) {
        install_fixture_with_manifest(
            workspace,
            "audit-hook",
            "pdk/rust/examples/audit-hook",
            &audit_hook_manifest(resources),
        );
    }

    /// The guest JSON-parses the payload of `tool_call` events into a DOM, so
    /// four megabytes of payload cannot fit an 8 MB linear memory.
    /// `max_payload_bytes` keeps the delivery past the host-side guard.
    const TRAPPING_HOOK_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608";

    const GUARDED_HOOK_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000";

    const TRAPPING_HOOK_EVENT: &str = "prx.lifecycle.tool_call";
    const HEALTHY_HOOK_EVENT: &str = "prx.lifecycle.turn_complete";

    fn trapping_hook_payload() -> String {
        serde_json::json!({"tool": "shell", "filler": "x".repeat(4 * 1024 * 1024)}).to_string()
    }

    #[tokio::test]
    async fn hook_recovers_on_a_fresh_store_after_a_guest_trap() {
        let temp = TempDir::new().unwrap();
        install_audit_hook(temp.path(), TRAPPING_HOOK_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

        runtime.emit_hook(TRAPPING_HOOK_EVENT, &trapping_hook_payload()).await;
        runtime
            .emit_hook(HEALTHY_HOOK_EVENT, r#"{"source":"recovery-test"}"#)
            .await;

        let adapter = single_status_adapter(&runtime, "hook_adapters").await;
        assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
        assert_eq!(adapter_counter(&adapter, "invocation_count"), 2, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 1, "{adapter}");
        assert_eq!(
            adapter_counter(&adapter, "failure_count"),
            1,
            "only the trapping delivery may fail: {adapter}"
        );
        assert!(
            !adapter_text(&adapter, "last_error").contains("cannot enter component instance"),
            "the recorded reason must be the trap, not a re-entry refusal: {adapter}"
        );
    }

    #[tokio::test]
    async fn hook_quarantines_after_the_restart_budget_is_spent() {
        let temp = TempDir::new().unwrap();
        install_audit_hook(temp.path(), TRAPPING_HOOK_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let (_, mut quarantined_events) = runtime
            .event_bus()
            .subscribe("hook-recovery-test", "prx.plugin.hook_quarantined")
            .await
            .unwrap();

        let payload = trapping_hook_payload();
        for _ in 0..5 {
            runtime.emit_hook(TRAPPING_HOOK_EVENT, &payload).await;
        }

        let adapter = single_status_adapter(&runtime, "hook_adapters").await;
        assert_eq!(adapter_text(&adapter, "state"), "quarantined", "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 3, "{adapter}");
        let skipped = adapter_counter(&adapter, "skipped_quarantined");

        runtime.emit_hook(HEALTHY_HOOK_EVENT, "{}").await;
        let after = single_status_adapter(&runtime, "hook_adapters").await;
        assert_eq!(adapter_counter(&after, "skipped_quarantined"), skipped + 1, "{after}");

        let event = quarantined_events
            .try_recv()
            .expect("test: one quarantine event must have been published");
        let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
        assert_eq!(
            payload.get("capability").and_then(serde_json::Value::as_str),
            Some("hook")
        );
        assert_ne!(
            runtime.status_tool().availability().level,
            crate::capability::CapabilityAvailabilityLevel::Healthy
        );
    }

    #[tokio::test]
    async fn hook_skips_a_payload_that_cannot_fit_the_guest() {
        let temp = TempDir::new().unwrap();
        install_audit_hook(temp.path(), GUARDED_HOOK_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

        runtime.emit_hook(TRAPPING_HOOK_EVENT, &trapping_hook_payload()).await;
        let adapter = single_status_adapter(&runtime, "hook_adapters").await;
        assert_eq!(adapter_counter(&adapter, "skipped_oversize"), 1, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");

        runtime.emit_hook(HEALTHY_HOOK_EVENT, "{}").await;
        let after = single_status_adapter(&runtime, "hook_adapters").await;
        assert_eq!(adapter_text(&after, "state"), "live", "{after}");
        assert_eq!(adapter_counter(&after, "failure_count"), 0, "{after}");
        assert_eq!(
            after.get("last_event").and_then(serde_json::Value::as_str),
            Some(HEALTHY_HOOK_EVENT),
            "{after}"
        );
    }

    /// Measured against the committed `audit-hook` component with a
    /// store-lifetime budget: instantiation costs roughly 40 000 fuel units and
    /// one delivery roughly 5 000, so 50 000 funds exactly two deliveries.
    const TWO_HOOK_DELIVERIES_FUEL: u64 = 50_000;

    #[tokio::test]
    async fn hook_fuel_is_metered_per_invocation() {
        let temp = TempDir::new().unwrap();
        install_audit_hook(
            temp.path(),
            &format!("max_fuel = {TWO_HOOK_DELIVERIES_FUEL}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
        );
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        for delivery in 1..=4 {
            runtime.emit_hook(HEALTHY_HOOK_EVENT, "{}").await;
            let hook = runtime.hook_diagnostics().remove(0);
            assert!(
                hook.last_error.is_none(),
                "delivery {delivery} exhausted a store-lifetime fuel budget: {:?}",
                hook.last_error
            );
        }
    }

    /// Once a year, at midnight on 1 January: never due while a test runs, so
    /// the runtime's own scheduler cannot take a run from under the test.
    const QUIET_CRON_SCHEDULE: &str = "0 0 0 1 1 *";

    fn counter_cron_manifest(resources: &str) -> String {
        let schedule = QUIET_CRON_SCHEDULE;
        format!(
            r#"[plugin]
name = "counter-cron"
version = "0.1.0"
description = "cron recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "cron"
name = "counter"
schedule = "{schedule}"

[permissions]
required = []
optional = []

[resources]
{resources}
"#
        )
    }

    fn install_counter_cron(workspace: &Path, resources: &str) {
        install_fixture_with_manifest(
            workspace,
            "counter-cron",
            "pdk/rust/examples/counter-cron",
            &counter_cron_manifest(resources),
        );
    }

    #[tokio::test]
    async fn cron_status_publishes_recovery_health() {
        let temp = TempDir::new().unwrap();
        install_counter_cron(
            temp.path(),
            "max_fuel = 1000000000\nmax_memory_mb = 8\nmax_execution_time_ms = 30000",
        );
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        assert_eq!(
            runtime.cron().run_job("counter-cron").await.unwrap(),
            "counter-cron-run-1"
        );

        let adapter = single_status_adapter(&runtime, "cron_adapters").await;
        assert_eq!(adapter_text(&adapter, "schedule"), QUIET_CRON_SCHEDULE, "{adapter}");
        assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
    }

    /// Measured against the committed `counter-cron` component with a
    /// store-lifetime budget: 16 000 fuel units cannot instantiate it, 17 000
    /// instantiates it with no run left, and each run costs roughly 1 000, so
    /// 18 500 funds at most two runs.
    const FEW_CRON_RUNS_FUEL: u64 = 18_500;

    #[tokio::test]
    async fn cron_fuel_is_metered_per_invocation() {
        let temp = TempDir::new().unwrap();
        install_counter_cron(
            temp.path(),
            &format!("max_fuel = {FEW_CRON_RUNS_FUEL}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
        );
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        for run in 1..=4 {
            let output = runtime.cron().run_job("counter-cron").await;
            assert_eq!(
                output.as_deref().ok(),
                Some(format!("counter-cron-run-{run}").as_str()),
                "run {run} exhausted a store-lifetime fuel budget: {output:?}"
            );
        }
    }

    fn echo_provider_manifest(resources: &str) -> String {
        format!(
            r#"[plugin]
name = "echo-provider"
version = "0.1.0"
description = "provider recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "provider"
name = "wasm-echo"

[permissions]
required = []
optional = []

[resources]
{resources}
"#
        )
    }

    fn install_echo_provider(workspace: &Path, resources: &str) {
        install_fixture_with_manifest(
            workspace,
            "echo-provider",
            "pdk/rust/examples/echo-provider",
            &echo_provider_manifest(resources),
        );
    }

    /// The guest copies the last user message into its reply, so four
    /// megabytes of message (argument copy plus formatted reply) cannot fit
    /// an 8 MB linear memory. `max_payload_bytes` keeps the request past the
    /// host-side guard so the trap stays the subject of the test.
    const TRAPPING_PROVIDER_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608";

    const GUARDED_PROVIDER_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000";

    fn trapping_provider_message() -> String {
        "x".repeat(4 * 1024 * 1024)
    }

    async fn echo_chat(
        provider: &dyn crate::providers::traits::Provider,
        message: &str,
    ) -> anyhow::Result<crate::providers::traits::ChatResponse> {
        provider
            .chat(
                crate::providers::traits::ChatRequest {
                    messages: &[crate::providers::traits::ChatMessage::user(message)],
                    tools: None,
                },
                "fixture-model",
                0.0,
            )
            .await
    }

    async fn assert_echo_healthy(provider: &dyn crate::providers::traits::Provider, context: &str) {
        let response = echo_chat(provider, "probe")
            .await
            .unwrap_or_else(|error| panic!("{context}: {error}"));
        assert_eq!(
            response.text.as_deref(),
            Some("wasm-echo/fixture-model: probe"),
            "{context}"
        );
    }

    #[tokio::test]
    async fn provider_recovers_on_a_fresh_store_after_a_guest_trap() {
        let temp = TempDir::new().unwrap();
        install_echo_provider(temp.path(), TRAPPING_PROVIDER_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let provider = runtime.provider("wasm-echo").unwrap();

        let trapped = echo_chat(provider.as_ref(), &trapping_provider_message()).await;
        assert!(trapped.is_err(), "the oversized request must trap and surface an error");
        assert_echo_healthy(provider.as_ref(), "the request after the trap").await;

        let adapter = single_status_adapter(&runtime, "provider_adapter_status").await;
        assert_eq!(adapter_text(&adapter, "provider"), "wasm-echo", "{adapter}");
        assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 1, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "failure_count"), 1, "{adapter}");
        assert!(
            !adapter_text(&adapter, "last_error").contains("cannot enter component instance"),
            "{adapter}"
        );
        assert_eq!(
            runtime.provider_names(),
            vec!["wasm-echo"],
            "the name list is unchanged"
        );
    }

    #[tokio::test]
    async fn provider_quarantines_after_the_restart_budget_is_spent() {
        let temp = TempDir::new().unwrap();
        install_echo_provider(temp.path(), TRAPPING_PROVIDER_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let (_, mut quarantined_events) = runtime
            .event_bus()
            .subscribe("provider-recovery-test", "prx.plugin.provider_quarantined")
            .await
            .unwrap();
        let provider = runtime.provider("wasm-echo").unwrap();

        let message = trapping_provider_message();
        for request in 1..=5 {
            assert!(
                echo_chat(provider.as_ref(), &message).await.is_err(),
                "trapping request {request} must surface an error, never a response"
            );
        }

        let adapter = single_status_adapter(&runtime, "provider_adapter_status").await;
        assert_eq!(adapter_text(&adapter, "state"), "quarantined", "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 3, "{adapter}");

        let refused = echo_chat(provider.as_ref(), "probe").await;
        let error = refused.expect_err("a quarantined provider must return an error");
        assert!(error.to_string().contains("quarantined"), "{error}");

        let event = quarantined_events
            .try_recv()
            .expect("test: one quarantine event must have been published");
        let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
        assert_eq!(
            payload.get("capability").and_then(serde_json::Value::as_str),
            Some("provider")
        );
        assert_ne!(
            runtime.status_tool().availability().level,
            crate::capability::CapabilityAvailabilityLevel::Healthy
        );
    }

    #[tokio::test]
    async fn provider_refuses_a_request_that_cannot_fit_the_guest() {
        let temp = TempDir::new().unwrap();
        install_echo_provider(temp.path(), GUARDED_PROVIDER_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let provider = runtime.provider("wasm-echo").unwrap();

        let error = echo_chat(provider.as_ref(), &trapping_provider_message())
            .await
            .expect_err("an over-cap request is an error, never a truncated response");
        assert!(error.to_string().contains("max_payload_bytes"), "{error}");

        let adapter = single_status_adapter(&runtime, "provider_adapter_status").await;
        assert_eq!(adapter_counter(&adapter, "skipped_oversize"), 1, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
        assert_echo_healthy(provider.as_ref(), "the request after an oversize refusal").await;
    }

    /// Measured against the committed `echo-provider` component with a
    /// store-lifetime budget: 20 000 fuel units instantiate it and answer
    /// `name()` with no chat left, 30 000 fund four chats and 40 000 six, so
    /// 25 000 cannot fund six chats unless fuel is re-armed per call.
    const FEW_PROVIDER_CHATS_FUEL: u64 = 25_000;

    #[tokio::test]
    async fn provider_fuel_is_metered_per_invocation() {
        let temp = TempDir::new().unwrap();
        install_echo_provider(
            temp.path(),
            &format!("max_fuel = {FEW_PROVIDER_CHATS_FUEL}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
        );
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let provider = runtime.provider("wasm-echo").unwrap();
        for chat in 1..=6 {
            assert_echo_healthy(provider.as_ref(), &format!("chat {chat} under a per-call fuel budget")).await;
        }
    }

    fn map_storage_manifest(resources: &str) -> String {
        format!(
            r#"[plugin]
name = "map-storage"
version = "0.1.0"
description = "storage recovery regression fixture"
wasm = "plugin.wasm"

[[capabilities]]
type = "storage"
name = "wasm-map"

[permissions]
required = []
optional = []

[resources]
{resources}
"#
        )
    }

    fn install_map_storage(workspace: &Path, resources: &str) {
        install_fixture_with_manifest(
            workspace,
            "map-storage",
            "pdk/rust/examples/map-storage",
            &map_storage_manifest(resources),
        );
    }

    /// The guest keeps every entry in its own linear memory, so a few
    /// four-megabyte entries cannot fit an 8 MB guest. `max_payload_bytes`
    /// keeps the writes past the host-side guard.
    const TRAPPING_STORAGE_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000
max_payload_bytes = 8388608";

    const GUARDED_STORAGE_RESOURCES: &str = "max_fuel = 1000000000
max_memory_mb = 8
max_execution_time_ms = 30000";

    fn large_memory_content() -> String {
        "x".repeat(4 * 1024 * 1024)
    }

    async fn store_entry(storage: &Arc<dyn Memory>, key: &str, content: &str) -> anyhow::Result<()> {
        storage
            .store(key, content, crate::memory::traits::MemoryCategory::Core, None)
            .await
    }

    /// Write large entries until the guest traps; returns the key whose write
    /// failed.
    async fn store_until_trap(storage: &Arc<dyn Memory>, prefix: &str) -> String {
        let content = large_memory_content();
        for attempt in 1..=4 {
            let key = format!("{prefix}-{attempt}");
            if store_entry(storage, &key, &content).await.is_err() {
                return key;
            }
        }
        panic!("four-megabyte entries must exhaust an 8 MB guest within four writes");
    }

    #[tokio::test]
    async fn storage_recovers_on_a_fresh_store_and_announces_the_reset() {
        let temp = TempDir::new().unwrap();
        install_map_storage(temp.path(), TRAPPING_STORAGE_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let (_, mut restarted_events) = runtime
            .event_bus()
            .subscribe("storage-recovery-test", "prx.plugin.storage_restarted")
            .await
            .unwrap();
        let storage = runtime.storage("wasm-map").unwrap();

        store_entry(&storage, "before", "kept in guest memory").await.unwrap();
        let failed_key = store_until_trap(&storage, "large").await;

        store_entry(&storage, "after", "written after the rebuild")
            .await
            .expect("test: the write after the trap runs on a fresh store");
        assert!(
            storage.get(&failed_key).await.unwrap().is_none(),
            "a write that failed must not be reported present"
        );
        assert_eq!(
            storage.count().await.unwrap(),
            1,
            "the rebuilt guest starts empty: only the post-rebuild entry exists"
        );
        assert!(storage.health_check().await);

        let adapter = single_status_adapter(&runtime, "storage_adapter_status").await;
        assert_eq!(adapter_text(&adapter, "storage"), "wasm-map", "{adapter}");
        assert_eq!(adapter_text(&adapter, "state"), "live", "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 1, "{adapter}");
        assert!(
            !adapter_text(&adapter, "last_error").contains("cannot enter component instance"),
            "{adapter}"
        );
        assert_eq!(runtime.storage_names(), vec!["wasm-map"], "the name list is unchanged");

        let event = restarted_events
            .try_recv()
            .expect("test: a storage rebuild must be announced");
        let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
        assert_eq!(
            payload.get("capability").and_then(serde_json::Value::as_str),
            Some("storage")
        );
        assert_eq!(
            payload.get("restart_count").and_then(serde_json::Value::as_u64),
            Some(1)
        );
    }

    #[tokio::test]
    async fn storage_quarantines_after_the_restart_budget_is_spent() {
        let temp = TempDir::new().unwrap();
        install_map_storage(temp.path(), TRAPPING_STORAGE_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let (_, mut quarantined_events) = runtime
            .event_bus()
            .subscribe("storage-recovery-test", "prx.plugin.storage_quarantined")
            .await
            .unwrap();
        let storage = runtime.storage("wasm-map").unwrap();

        for round in 1..=4 {
            store_until_trap(&storage, &format!("round-{round}")).await;
        }

        // The first call after the fourth trap finds the budget spent and
        // quarantines the backend; every operation is refused from then on.
        for (operation, failed) in [
            ("store", store_entry(&storage, "refused", "x").await.is_err()),
            ("recall", storage.recall("", 10, None).await.is_err()),
            ("count", storage.count().await.is_err()),
            ("forget", storage.forget("refused").await.is_err()),
        ] {
            assert!(failed, "a quarantined backend must fail '{operation}', never answer it");
        }
        assert!(!storage.health_check().await, "a quarantined backend is not healthy");

        let adapter = single_status_adapter(&runtime, "storage_adapter_status").await;
        assert_eq!(adapter_text(&adapter, "state"), "quarantined", "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 3, "{adapter}");

        let event = quarantined_events
            .try_recv()
            .expect("test: one quarantine event must have been published");
        let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
        assert_eq!(
            payload.get("capability").and_then(serde_json::Value::as_str),
            Some("storage")
        );
        assert_ne!(
            runtime.status_tool().availability().level,
            crate::capability::CapabilityAvailabilityLevel::Healthy
        );
    }

    #[tokio::test]
    async fn storage_refuses_a_write_that_cannot_fit_the_guest() {
        let temp = TempDir::new().unwrap();
        install_map_storage(temp.path(), GUARDED_STORAGE_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let storage = runtime.storage("wasm-map").unwrap();

        store_entry(&storage, "small", "fits").await.unwrap();
        let error = store_entry(&storage, "large", &large_memory_content())
            .await
            .expect_err("an over-cap write is an error, never a silent drop");
        assert!(error.to_string().contains("max_payload_bytes"), "{error}");

        let adapter = single_status_adapter(&runtime, "storage_adapter_status").await;
        assert_eq!(adapter_counter(&adapter, "skipped_oversize"), 1, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "failure_count"), 0, "{adapter}");
        assert_eq!(adapter_counter(&adapter, "restart_count"), 0, "{adapter}");
        assert_eq!(storage.count().await.unwrap(), 1, "the earlier entry is intact");
    }

    /// Measured against the committed `map-storage` component with a
    /// store-lifetime budget: 10 000 fuel units cannot instantiate it and
    /// answer `name()`, 20 000 fund three writes and 30 000 six, so 20 000
    /// cannot fund six writes unless fuel is re-armed per call.
    const FEW_STORAGE_WRITES_FUEL: u64 = 20_000;

    #[tokio::test]
    async fn storage_fuel_is_metered_per_invocation() {
        let temp = TempDir::new().unwrap();
        install_map_storage(
            temp.path(),
            &format!("max_fuel = {FEW_STORAGE_WRITES_FUEL}\nmax_memory_mb = 8\nmax_execution_time_ms = 30000"),
        );
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let storage = runtime.storage("wasm-map").unwrap();
        for write in 1..=6 {
            store_entry(&storage, &format!("entry-{write}"), "value")
                .await
                .unwrap_or_else(|error| panic!("write {write} exhausted a store-lifetime fuel budget: {error}"));
        }
        assert_eq!(storage.count().await.unwrap(), 6);
    }

    /// R-C3X F4: the operation that rebuilds a storage backend runs against a
    /// fresh, empty guest. A read answering from it would look like "no
    /// memories" to the memory layer, so the first read after a rebuild is an
    /// error naming the reset; the next read answers from the rebuilt store.
    #[tokio::test]
    async fn storage_reports_its_reset_on_the_first_read_after_a_rebuild() {
        let temp = TempDir::new().unwrap();
        install_map_storage(temp.path(), TRAPPING_STORAGE_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());
        let storage = runtime.storage("wasm-map").unwrap();

        store_entry(&storage, "before", "kept in guest memory").await.unwrap();
        store_until_trap(&storage, "large").await;

        let first = storage.count().await;
        let error = first.expect_err("the read that rebuilt the backend must not answer from the empty store");
        assert!(error.to_string().contains("rebuilt"), "{error}");
        assert_eq!(
            storage.count().await.unwrap(),
            0,
            "the next read answers from the rebuilt, empty store"
        );
    }

    /// R-C3X F8: provider and storage failures reach callers as `anyhow`
    /// errors. They must keep their `PluginError` so the reliability layer can
    /// tell a quarantine, a timeout or a refused payload apart by type, not by
    /// matching message text.
    #[tokio::test]
    async fn provider_and_storage_errors_keep_their_plugin_error_type() {
        let temp = TempDir::new().unwrap();
        install_echo_provider(temp.path(), GUARDED_PROVIDER_RESOURCES);
        install_map_storage(temp.path(), GUARDED_STORAGE_RESOURCES);
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(runtime.adapter_errors().is_empty(), "{:?}", runtime.adapter_errors());

        let provider = runtime.provider("wasm-echo").unwrap();
        let error = echo_chat(provider.as_ref(), &trapping_provider_message())
            .await
            .expect_err("test: an over-cap request is refused");
        assert!(
            matches!(error.downcast_ref::<PluginError>(), Some(PluginError::ResourceLimit(_))),
            "{error:?}"
        );

        let storage = runtime.storage("wasm-map").unwrap();
        let error = store_entry(&storage, "large", &large_memory_content())
            .await
            .expect_err("test: an over-cap write is refused");
        assert!(
            matches!(error.downcast_ref::<PluginError>(), Some(PluginError::ResourceLimit(_))),
            "{error:?}"
        );
    }

    // ── Finding 95: trust, switch, install binding ──────────────────────

    /// Copy a committed example into `plugins/<example>` without trusting it.
    fn plant_untrusted(workspace: &Path, example: &str) -> PathBuf {
        install_example_fixture(workspace, example);
        workspace.join("plugins").join(example)
    }

    async fn plugin_status(runtime: &PluginRuntime, name: &str) -> Option<PluginInfo> {
        runtime
            .list_plugins()
            .await
            .into_iter()
            .find(|plugin| plugin.name == name)
    }

    #[tokio::test]
    async fn the_managed_marker_no_longer_grants_trust() {
        let temp = TempDir::new().unwrap();
        let plugin_dir = plant_untrusted(temp.path(), "audit-hook");
        // What the old code treated as full trust, and what an agent could
        // create with one medium-risk file_write.
        std::fs::write(plugin_dir.join(".prx-managed"), "installed by wasm_plugins_manage\n").unwrap();

        let runtime = init_plugin_runtime_with_policy(temp.path(), test_policy(temp.path()), None)
            .await
            .unwrap();
        let info = plugin_status(&runtime, "audit-hook")
            .await
            .expect("refused plugins stay listed");
        assert!(matches!(info.status, PluginStatus::Error(_)), "{:?}", info.status);
        assert!(info.permissions_granted.is_empty(), "{:?}", info.permissions_granted);
        assert!(runtime.hook_diagnostics().is_empty(), "a refused hook must not run");
        assert_eq!(runtime.adapter_errors().len(), 0);
    }

    #[tokio::test]
    async fn an_untrusted_plugin_that_asks_for_nothing_is_still_refused() {
        let temp = TempDir::new().unwrap();
        plant_untrusted(temp.path(), "pipeline-middleware");
        let runtime = init_plugin_runtime_with_policy(temp.path(), test_policy(temp.path()), None)
            .await
            .unwrap();
        let info = plugin_status(&runtime, "pipeline-middleware").await.unwrap();
        assert!(matches!(&info.status, PluginStatus::Error(reason) if reason.contains("not trusted")));
        assert_eq!(runtime.middleware_diagnostics().len(), 0);
        let output = runtime
            .middleware()
            .process(
                super::super::capabilities::middleware::MiddlewareStage::Outbound,
                r#"{"text":"hello"}"#,
            )
            .await;
        assert!(!output.contains("[wasm-middleware]"), "{output}");
    }

    #[tokio::test]
    async fn a_trusted_plugin_whose_component_changed_is_refused() {
        let temp = TempDir::new().unwrap();
        let plugin_dir = plant_untrusted(temp.path(), "pipeline-middleware");
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert!(matches!(
            plugin_status(&runtime, "pipeline-middleware").await.unwrap().status,
            PluginStatus::Active
        ));
        assert_eq!(runtime.middleware_diagnostics().len(), 1);

        // The agent swaps the component after the operator trusted it.
        let mut bytes = std::fs::read(plugin_dir.join("plugin.wasm")).unwrap();
        bytes.extend_from_slice(b"\0tampered");
        std::fs::write(plugin_dir.join("plugin.wasm"), bytes).unwrap();
        runtime.refresh_all().await.unwrap();

        let info = plugin_status(&runtime, "pipeline-middleware").await.unwrap();
        assert!(
            matches!(&info.status, PluginStatus::Error(reason) if reason.contains("hash")),
            "{:?}",
            info.status
        );
        assert!(runtime.middleware_diagnostics().is_empty(), "the swapped component ran");
    }

    #[tokio::test]
    async fn a_trusted_plugin_whose_manifest_widened_is_refused() {
        let temp = TempDir::new().unwrap();
        let plugin_dir = plant_untrusted(temp.path(), "pipeline-middleware");
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        assert_eq!(runtime.middleware_diagnostics().len(), 1);

        let mut manifest = std::fs::read_to_string(plugin_dir.join("plugin.toml")).unwrap();
        manifest = manifest.replace(
            "required = []",
            "required = []\nhttp_allowlist = [\"https://evil.example/*\"]",
        );
        std::fs::write(plugin_dir.join("plugin.toml"), manifest).unwrap();
        runtime.refresh_all().await.ok();
        let info = plugin_status(&runtime, "pipeline-middleware").await;
        assert!(
            info.is_none_or(|info| matches!(info.status, PluginStatus::Error(_))),
            "an edited manifest kept its trust"
        );
        assert_eq!(runtime.middleware_diagnostics().len(), 0);
    }

    #[tokio::test]
    async fn a_missing_or_unreadable_trust_file_trusts_nothing() {
        let temp = TempDir::new().unwrap();
        plant_untrusted(temp.path(), "pipeline-middleware");
        let policy = test_policy(temp.path());
        std::fs::create_dir_all(policy.trust_file().parent().unwrap()).unwrap();
        std::fs::write(policy.trust_file(), "{ garbage").unwrap();
        let runtime = init_plugin_runtime_with_policy(temp.path(), policy, None)
            .await
            .unwrap();
        let info = plugin_status(&runtime, "pipeline-middleware").await.unwrap();
        assert!(matches!(info.status, PluginStatus::Error(_)));
        assert_eq!(runtime.middleware_diagnostics().len(), 0);
    }

    #[tokio::test]
    async fn with_the_switch_off_nothing_is_scanned_or_loaded() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        install_example_fixture(&workspace, "pipeline-middleware");
        // Even a plugin the operator trusted must not load while the switch is off.
        trust_workspace_plugins(&workspace);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.clone();
        config.config_path = temp.path().join("config.toml");
        assert!(!config.plugins.enabled, "the switch defaults to off");
        assert!(crate::plugins::init_plugin_runtime(&config, None).await.is_none());
        assert!(
            !workspace.join(".prx-state/plugin-cache").exists(),
            "a disabled runtime must not even prepare its cache"
        );
        // The policy form refuses too.
        let off = PluginPolicy::new(false, workspace.join(".prx-state").join(TRUST_FILE_NAME));
        assert!(
            init_plugin_runtime_with_policy(&workspace, off.clone(), None)
                .await
                .is_none()
        );
        assert!(matches!(
            PluginManager::new(workspace.join("plugins"), off).err(),
            Some(PluginError::Disabled)
        ));
    }

    #[tokio::test]
    async fn the_switch_on_with_an_unsafe_state_directory_stays_off() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.clone();
        config.config_path = workspace.join("config.toml");
        config.plugins.enabled = true;
        assert!(crate::plugins::init_plugin_runtime(&config, None).await.is_none());
    }

    fn manage_for(
        runtime: &Arc<PluginRuntime>,
        workspace: &Path,
        autonomy: crate::security::AutonomyLevel,
    ) -> Box<dyn Tool> {
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.autonomy.level = autonomy;
        runtime.manage_tool(crate::runtime::bootstrap::build_security_policy(&config))
    }

    fn source_for_install(workspace: &Path, example: &str) -> String {
        let source = workspace.join("plugin-sources").join(example);
        std::fs::create_dir_all(&source).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("pdk/rust/examples")
            .join(example);
        std::fs::copy(fixture.join("plugin.toml"), source.join("plugin.toml")).unwrap();
        std::fs::copy(fixture.join("plugin.wasm"), source.join("plugin.wasm")).unwrap();
        format!("plugin-sources/{example}")
    }

    #[tokio::test]
    async fn install_records_trust_bound_to_name_hash_and_the_granted_permissions() {
        let temp = TempDir::new().unwrap();
        let source = source_for_install(temp.path(), "audit-hook");
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        let manage = manage_for(&runtime, temp.path(), crate::security::AutonomyLevel::Full);

        // A grant that leaves out a required permission is refused up front.
        let short = manage
            .execute(serde_json::json!({"action": "install", "source": source, "grant_permissions": ["log"]}))
            .await
            .unwrap();
        assert!(!short.success);
        assert!(
            short.error.unwrap_or_default().contains("requires"),
            "required permission check"
        );
        // A grant of an undeclared permission is refused.
        let wide = manage
            .execute(serde_json::json!({
                "action": "install",
                "source": source,
                "grant_permissions": ["log", "kv", "events", "shell"]
            }))
            .await
            .unwrap();
        assert!(!wide.success);
        // Nothing was trusted by the failed attempts.
        assert!(
            test_policy(temp.path())
                .records()
                .unwrap()
                .iter()
                .all(|r| r.name != "audit-hook")
        );

        let installed = manage
            .execute(serde_json::json!({
                "action": "install",
                "source": source,
                "grant_permissions": ["events", "kv", "log"]
            }))
            .await
            .unwrap();
        assert!(installed.success, "{:?}", installed.error);
        let records = test_policy(temp.path()).records().unwrap();
        let record = records.iter().find(|r| r.name == "audit-hook").expect("trust recorded");
        let wasm = std::fs::read(temp.path().join("plugins/audit-hook/plugin.wasm")).unwrap();
        assert_eq!(record.sha256, crate::plugins::trust::sha256_hex(&wasm));
        assert_eq!(record.permissions, vec!["events", "kv", "log"]);
        assert!(!temp.path().join("plugins/audit-hook/.prx-managed").exists());
        assert!(
            !test_policy(temp.path())
                .trust_file()
                .starts_with(temp.path().join("plugins")),
            "the trust file must live outside the agent-writable plugins directory"
        );
        assert_eq!(runtime.hook_diagnostics().len(), 1);

        let removed = manage
            .execute(serde_json::json!({"action": "remove", "name": "audit-hook"}))
            .await
            .unwrap();
        assert!(removed.success, "{:?}", removed.error);
        assert!(
            test_policy(temp.path())
                .records()
                .unwrap()
                .iter()
                .all(|r| r.name != "audit-hook")
        );
    }

    #[tokio::test]
    async fn install_approval_is_bound_to_the_exact_build_and_grant() {
        let temp = TempDir::new().unwrap();
        let source = source_for_install(temp.path(), "audit-hook");
        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        let manage = manage_for(&runtime, temp.path(), crate::security::AutonomyLevel::Supervised);

        let denied = manage
            .execute(serde_json::json!({
                "action": "install",
                "source": source,
                "grant_permissions": ["log", "kv", "events"]
            }))
            .await
            .unwrap();
        assert!(!denied.success);
        let error = denied.error.unwrap_or_default();
        assert!(error.contains("runtime approval grant"), "{error}");
        let wasm = std::fs::read(temp.path().join("plugin-sources/audit-hook/plugin.wasm")).unwrap();
        assert!(
            error.contains(&crate::plugins::trust::sha256_hex(&wasm)),
            "the approval operation must name the component hash: {error}"
        );
        assert!(error.contains("audit-hook"), "{error}");
        assert!(
            error.contains("grants_events_kv_log"),
            "the approval text must show the sorted grant: {error}"
        );
        let grant_digest = crate::plugins::trust::sha256_hex(br#"["events","kv","log"]"#);
        assert!(
            error.contains(&grant_digest),
            "the operation must carry the digest of the sorted grant: {error}"
        );
        assert!(!temp.path().join("plugins/audit-hook").exists());
        assert_eq!(test_policy(temp.path()).records().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn optional_permissions_are_granted_only_through_the_trust_record() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("plugins/optional-tool");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            "[plugin]\nname = \"optional-tool\"\nversion = \"0.1.0\"\nwasm = \"missing.wasm\"\n\n\
             [permissions]\nrequired = [\"log\"]\noptional = [\"memory\"]\n",
        )
        .unwrap();
        let policy = test_policy(temp.path());
        let material = PluginMaterial::read(&dir).unwrap();
        policy
            .record(material.record_for(&["log".to_string()]).unwrap())
            .unwrap();
        let runtime = init_plugin_runtime_with_policy(temp.path(), policy.clone(), None)
            .await
            .unwrap();
        let info = plugin_status(&runtime, "optional-tool").await.unwrap();
        assert_eq!(info.permissions_granted, vec!["log"]);

        policy
            .record(material.record_for(&["log".to_string(), "memory".to_string()]).unwrap())
            .unwrap();
        runtime.refresh_all().await.unwrap();
        let info = plugin_status(&runtime, "optional-tool").await.unwrap();
        assert_eq!(info.permissions_granted, vec!["log", "memory"]);
    }

    /// Trust is keyed by manifest name. Removing folder `foo` (manifest `bar`)
    /// must revoke `bar` and leave the unrelated trusted plugin named `foo`.
    #[tokio::test]
    async fn remove_revokes_the_manifest_name_not_the_folder_name() {
        let temp = TempDir::new().unwrap();
        install_example_fixture(temp.path(), "pipeline-middleware");
        install_example_fixture(temp.path(), "echo-provider");
        let plugins = temp.path().join("plugins");
        std::fs::rename(plugins.join("pipeline-middleware"), plugins.join("foo")).unwrap();
        std::fs::rename(plugins.join("echo-provider"), plugins.join("other")).unwrap();
        let rewrite = |dir: &str, from: &str, to: &str| {
            let path = plugins.join(dir).join("plugin.toml");
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains(&format!("name = \"{from}\"")), "fixture name line");
            std::fs::write(
                &path,
                text.replacen(&format!("name = \"{from}\""), &format!("name = \"{to}\""), 1),
            )
            .unwrap();
        };
        rewrite("foo", "pipeline-middleware", "bar");
        rewrite("other", "echo-provider", "foo");

        let runtime = init_plugin_runtime(temp.path(), None).await.unwrap();
        let names = |records: Vec<crate::plugins::trust::TrustRecord>| {
            let mut names: Vec<String> = records.into_iter().map(|record| record.name).collect();
            names.sort();
            names
        };
        assert_eq!(names(test_policy(temp.path()).records().unwrap()), vec!["bar", "foo"]);

        let manage = manage_for(&runtime, temp.path(), crate::security::AutonomyLevel::Full);
        let removed = manage
            .execute(serde_json::json!({"action": "remove", "name": "foo"}))
            .await
            .unwrap();
        assert!(removed.success, "{:?}", removed.error);
        assert_eq!(
            names(test_policy(temp.path()).records().unwrap()),
            vec!["foo"],
            "the folder-name collision revoked an unrelated plugin's trust"
        );
        assert!(plugins.join("other").is_dir());
    }
}
