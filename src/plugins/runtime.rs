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
use super::error::{PluginError, PluginResult};
use super::event_bus::EventBus;
use super::registry::PluginInfo;
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

        let tools = manager
            .create_tool_adapters_with_memory(memory, Some(Arc::clone(&event_bus)))
            .await
            .into_iter()
            .map(Arc::<dyn Tool>::from)
            .collect();
        let middleware = Arc::new(manager.create_middleware_chain(Some(Arc::clone(&event_bus))).await);
        let hooks = Arc::new(manager.create_hook_executor(Some(Arc::clone(&event_bus))).await);
        let cron = Arc::new(manager.create_cron_manager(Some(event_bus)).await);

        Ok(Self {
            id,
            manager,
            tools,
            middleware,
            hooks,
            cron,
        })
    }
}

/// Sole process-level owner of a workspace's plugin generation and event bus.
pub struct PluginRuntime {
    plugins_dir: PathBuf,
    memory: Option<Arc<dyn Memory>>,
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
            memory,
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

    /// List plugins from the current generation.
    pub async fn list_plugins(&self) -> Vec<PluginInfo> {
        let generation = self.generation.load_full();
        generation.manager.list_plugins().await
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
        let candidate = PluginGeneration::build(
            next_id,
            self.plugins_dir.clone(),
            self.memory.clone(),
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

    /// Snapshot the current middleware generation for one request pipeline.
    pub fn middleware(&self) -> Arc<MiddlewareChain> {
        Arc::clone(&self.generation.load().middleware)
    }

    /// Snapshot the current cron generation for scheduler integration.
    pub fn cron(&self) -> Arc<WasmCronManager> {
        Arc::clone(&self.generation.load().cron)
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
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&serde_json::json!({
                "generation": self.runtime.generation_id(),
                "count": plugins.len(),
                "plugins": plugins,
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
}
