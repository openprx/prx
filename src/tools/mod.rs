//! Tool subsystem for agent-callable capabilities.
//!
//! This module implements the tool execution surface exposed to the LLM during
//! agentic loops. Each tool implements the [`Tool`] trait defined in [`traits`],
//! which requires a name, description, JSON parameter schema, and an async
//! `execute` method returning a structured [`ToolResult`].
//!
//! Tools are assembled into registries by [`default_tools`] (shell, file read/write)
//! and [`all_tools`] (full set including memory, cron, HTTP, delegation,
//! and optional integrations). Security policy enforcement is injected via
//! [`SecurityPolicy`](crate::security::SecurityPolicy) at construction time.
//!
//! # Extension
//!
//! To add a new tool, implement [`Tool`] in a new submodule and register it in
//! [`all_tools_with_runtime`]. See `AGENTS.md` §7.3 for the full change playbook.

pub mod agents_list;
pub mod chat_profile_update;
pub mod composio;
pub mod config_reload;
pub mod cron;
pub mod delegate;
pub mod document_get_chunk;
pub mod document_search;
pub mod error_hints;
pub mod execution;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod gateway;
pub mod git_operations;
pub mod http_request;
pub mod image;
pub mod image_info;
pub mod intent;
pub mod mcp;
pub mod memory_forget;
pub mod memory_get;
pub mod memory_recall;
pub mod memory_reindex;
pub mod memory_search;
pub mod memory_store;
pub mod message_send;
pub mod nodes;
pub mod proxy_config;
pub mod pushover;
pub mod schema;
pub mod session_status;
pub mod sessions_history;
pub mod sessions_list;
pub(crate) mod sessions_read_model;
pub mod sessions_send;
pub mod sessions_spawn;
pub mod shell;
pub mod stay_silent;
pub mod subagents;
pub(crate) mod tool_diff;
pub mod traits;
pub mod web_fetch;
pub mod web_search_tool;
pub mod xin;

pub use agents_list::AgentsListTool;
pub use chat_profile_update::ChatProfileUpdateTool;
pub use composio::ComposioTool;
pub use config_reload::ConfigReloadTool;
pub use cron::CronTool;
pub use delegate::DelegateTool;
pub use document_get_chunk::DocumentGetChunkTool;
pub use document_search::DocumentSearchTool;
pub use execution::{
    AdapterOwnedPreparation, ApprovalStrategy, DenyApprovalStrategy, EffectPolicy, LegacyToolAdapter,
    SecurityEffectPolicy, ToolAdapterKind, ToolApprovalDecision, ToolApprovalRequest, ToolBackend, ToolCatalog,
    ToolDescriptor, ToolEffect, ToolExecutionAuditRecord, ToolExecutionAuditSink, ToolExecutionCommand,
    ToolExecutionContext, ToolExecutionDecision, ToolExecutionOutcome, ToolExecutionPermit, ToolExecutionPreparation,
    ToolExecutionService, ToolExecutionStatus, TracingToolExecutionAudit, decide_tool_execution,
};
pub use file_edit::FileEditTool;
pub use file_read::FileReadTool;
pub use file_write::FileWriteTool;
pub use gateway::GatewayTool;
pub use git_operations::GitOperationsTool;
pub use http_request::HttpRequestTool;
pub use image::ImageTool;
pub use image_info::ImageInfoTool;
pub use mcp::McpTool;
pub use memory_forget::MemoryForgetTool;
pub use memory_get::MemoryGetTool;
pub use memory_recall::MemoryRecallTool;
pub use memory_reindex::MemoryReindexTool;
pub use memory_search::MemorySearchTool;
pub use memory_store::MemoryStoreTool;
pub use message_send::MessageSendTool;
pub use nodes::NodesTool;
pub use proxy_config::ProxyConfigTool;
pub use pushover::PushoverTool;
pub use session_status::SessionStatusTool;
pub use sessions_history::SessionsHistoryTool;
pub use sessions_list::SessionsListTool;
pub use sessions_send::SessionsSendTool;
pub use sessions_spawn::SessionsSpawnTool;
pub use shell::ShellTool;
pub use stay_silent::{STAY_SILENT_TOOL_NAME, StaySilentTool};
pub use subagents::SubagentsTool;
pub use traits::Tool;
pub use traits::{ToolCategory, ToolResult, ToolSpec, ToolTier};
pub use web_fetch::WebFetchTool;
pub use web_search_tool::WebSearchTool;
pub use xin::XinTool;

use crate::config::{Config, DelegateAgentConfig};
use crate::memory::Memory;
use crate::runtime::{NativeRuntime, RuntimeAdapter};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

/// Single, authoritative gate for advertising the smart group-reply `stay_silent`
/// tool to the model. `stay_silent` is registered globally (so the tool loop can
/// always resolve a call), but its *spec* must only ever be advertised on smart
/// group-reply turns. Every tool-spec / prompt-instruction construction path
/// (native specs, prompt-guided text, skill-RAG rebuilds, the TUI/Redux
/// dispatcher, gateway sessions) funnels through this helper so that DMs and any
/// non-smart turn — on *any* provider type and *any* path — can never see, and
/// thus never call, `stay_silent`.
///
/// `expose_stay_silent` is `true` only for a smart group-reply turn; otherwise
/// the `stay_silent` spec is removed in place.
pub fn filter_tool_specs_for_exposure(specs: &mut Vec<ToolSpec>, expose_stay_silent: bool) {
    if !expose_stay_silent {
        specs.retain(|spec| spec.name != STAY_SILENT_TOOL_NAME);
    }
}

/// Whether a tool should be advertised to the model given the current exposure
/// context. Mirrors [`filter_tool_specs_for_exposure`] for callers that iterate
/// the registry directly (e.g. the prompt-guided `build_tool_instructions`),
/// keeping a single rule for which tools are exposure-gated.
#[must_use]
pub fn tool_name_is_exposed(name: &str, expose_stay_silent: bool) -> bool {
    expose_stay_silent || name != STAY_SILENT_TOOL_NAME
}

#[derive(Clone)]
struct ArcDelegatingTool {
    inner: Arc<dyn Tool>,
}

impl ArcDelegatingTool {
    fn boxed(inner: Arc<dyn Tool>) -> Box<dyn Tool> {
        Box::new(Self { inner })
    }
}

#[async_trait]
impl Tool for ArcDelegatingTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.inner.execute(args).await
    }

    async fn execute_with_cancellation(
        &self,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        self.inner.execute_with_cancellation(args, cancellation).await
    }

    async fn execute_named(&self, name: &str, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.inner.execute_named(name, args).await
    }

    async fn execute_named_with_cancellation(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<ToolResult> {
        self.inner
            .execute_named_with_cancellation(name, args, cancellation)
            .await
    }

    fn tier(&self) -> ToolTier {
        self.inner.tier()
    }

    fn categories(&self) -> &'static [ToolCategory] {
        self.inner.categories()
    }
}

fn boxed_registry_from_arcs(tools: Vec<Arc<dyn Tool>>) -> Vec<Box<dyn Tool>> {
    tools.into_iter().map(ArcDelegatingTool::boxed).collect()
}

/// Create the default tool registry
pub fn default_tools(security: Arc<SecurityPolicy>) -> Vec<Box<dyn Tool>> {
    default_tools_with_runtime(security, Arc::new(NativeRuntime::new()))
}

/// Create the default tool registry with explicit runtime adapter.
pub fn default_tools_with_runtime(
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Vec<Box<dyn Tool>> {
    let workspace_dir = security.workspace_dir.clone();
    vec![
        Box::new(ShellTool::new(runtime, workspace_dir)),
        Box::new(FileReadTool::new(security.clone(), false)),
        Box::new(FileWriteTool::new(security.clone())),
        Box::new(FileEditTool::new(security)),
    ]
}

/// Create full tool registry including memory tools and optional Composio
#[allow(clippy::implicit_hasher, clippy::too_many_arguments)]
pub fn all_tools(
    config: Arc<Config>,
    security: &Arc<SecurityPolicy>,
    memory: Arc<dyn Memory>,
    composio_key: Option<&str>,
    composio_entity_id: Option<&str>,
    browser_config: &crate::config::BrowserConfig,
    http_config: &crate::config::HttpRequestConfig,
    workspace_dir: &std::path::Path,
    agents: &HashMap<String, DelegateAgentConfig>,
    fallback_api_key: Option<&str>,
    root_config: &crate::config::Config,
) -> Vec<Box<dyn Tool>> {
    let shared_config = crate::config::new_shared(config.as_ref().clone());
    all_tools_with_runtime(
        config,
        shared_config,
        security,
        Arc::new(NativeRuntime::new()),
        memory,
        composio_key,
        composio_entity_id,
        browser_config,
        http_config,
        workspace_dir,
        agents,
        fallback_api_key,
        root_config,
    )
}

/// Result of building the full tool registry, including optional side-channel
/// references to specific tool instances that the gateway needs direct access to.
pub struct ToolsRegistryResult {
    pub tools: Vec<Box<dyn Tool>>,
    /// If MCP is enabled, holds a shared reference to the `McpTool` so the
    /// gateway can query runtime-discovered tools without downcasting.
    pub mcp_tool: Option<Arc<McpTool>>,
}

/// Create full tool registry including memory tools and optional Composio.
#[allow(clippy::implicit_hasher, clippy::too_many_arguments)]
pub fn all_tools_with_runtime(
    config: Arc<Config>,
    shared_config: crate::config::SharedConfig,
    security: &Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
    memory: Arc<dyn Memory>,
    composio_key: Option<&str>,
    composio_entity_id: Option<&str>,
    browser_config: &crate::config::BrowserConfig,
    http_config: &crate::config::HttpRequestConfig,
    workspace_dir: &std::path::Path,
    agents: &HashMap<String, DelegateAgentConfig>,
    fallback_api_key: Option<&str>,
    root_config: &crate::config::Config,
) -> Vec<Box<dyn Tool>> {
    let result = all_tools_with_runtime_ext(
        config,
        shared_config,
        security,
        runtime,
        memory,
        composio_key,
        composio_entity_id,
        browser_config,
        http_config,
        workspace_dir,
        agents,
        fallback_api_key,
        root_config,
    );
    result.tools
}

/// Like [`all_tools_with_runtime`] but also returns side-channel references
/// (e.g. `Arc<McpTool>`) for gateway introspection.
#[allow(clippy::implicit_hasher, clippy::too_many_arguments)]
pub fn all_tools_with_runtime_ext(
    config: Arc<Config>,
    shared_config: crate::config::SharedConfig,
    security: &Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
    memory: Arc<dyn Memory>,
    composio_key: Option<&str>,
    composio_entity_id: Option<&str>,
    browser_config: &crate::config::BrowserConfig,
    http_config: &crate::config::HttpRequestConfig,
    workspace_dir: &std::path::Path,
    agents: &HashMap<String, DelegateAgentConfig>,
    fallback_api_key: Option<&str>,
    root_config: &crate::config::Config,
) -> ToolsRegistryResult {
    // Core tools — always registered regardless of module flags.
    let mut tool_arcs: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ShellTool::new(runtime, workspace_dir.to_path_buf())),
        Arc::new(FileReadTool::new(security.clone(), config.memory.acl_enabled)),
        Arc::new(FileWriteTool::new(security.clone())),
        Arc::new(FileEditTool::new(security.clone())),
        Arc::new(ProxyConfigTool::new(shared_config.clone(), security.clone())),
        Arc::new(GitOperationsTool::new(security.clone(), workspace_dir.to_path_buf())),
        Arc::new(PushoverTool::new(security.clone(), workspace_dir.to_path_buf())),
        // stay_silent lets the model decline to reply in smart group-reply mode.
        // Always registered so the loop can resolve the call; its spec is only
        // advertised to the model on smart group turns (loop-side `expose_stay_silent`
        // gate). A defensive `execute` makes any out-of-loop call a no-op.
        Arc::new(StaySilentTool::new()),
    ];

    // Vision tools are always available
    tool_arcs.push(Arc::new(ImageInfoTool::new(security.clone())));

    // ── Scheduler tools ──
    // The unified `cron` tool is ALWAYS registered (matching the legacy `schedule`
    // tool's always-on availability). The background scheduler is also always
    // started; concrete jobs determine whether any work fires.
    tool_arcs.push(Arc::new(CronTool::new(shared_config.clone(), security.clone())));
    tool_arcs.push(Arc::new(XinTool::new(shared_config.clone(), security.clone())));

    // ── Memory tools ──
    {
        tool_arcs.push(Arc::new(ChatProfileUpdateTool::new(memory.clone(), security.clone())));
        tool_arcs.push(Arc::new(MemoryStoreTool::new(memory.clone(), security.clone())));
        tool_arcs.push(Arc::new(MemoryForgetTool::new(memory.clone(), security.clone())));
        tool_arcs.push(Arc::new(MemorySearchTool::new(
            workspace_dir.to_path_buf(),
            memory.clone(),
            config.memory.acl_enabled,
        )));
        tool_arcs.push(Arc::new(MemoryGetTool::new(
            workspace_dir.to_path_buf(),
            memory.clone(),
            config.memory.acl_enabled,
        )));
        tool_arcs.push(Arc::new(DocumentSearchTool::new(
            workspace_dir.to_path_buf(),
            memory.clone(),
        )));
        tool_arcs.push(Arc::new(DocumentGetChunkTool::new(
            workspace_dir.to_path_buf(),
            memory.clone(),
        )));
        tool_arcs.push(Arc::new(MemoryReindexTool::new(memory.clone(), security.clone())));

        if config.memory.acl_enabled {
            tracing::warn!("memory_recall disabled because memory ACL is enabled; skipping tool registration");
        } else {
            tool_arcs.push(Arc::new(MemoryRecallTool::new(memory.clone(), false)));
        }
    }

    tool_arcs.push(Arc::new(
        NodesTool::new(shared_config, security.clone())
            .with_shared_memory(memory.clone())
            .with_event_recording(root_config.memory.event_recording_config()),
    ));

    // MCP is always registered. An empty server catalog is a configured-state
    // fact exposed by the tool, not a module-disable switch.
    let mcp = Arc::new(McpTool::new(
        security.clone(),
        config.mcp.clone(),
        workspace_dir.to_path_buf(),
    ));
    tool_arcs.push(mcp.clone());
    let mcp_tool_ref = Some(mcp);

    if let Some(key) = composio_key {
        if !key.is_empty() {
            tool_arcs.push(Arc::new(ComposioTool::new(key, composio_entity_id, security.clone())));
        }
    }

    // Network tool surfaces are always registered. Provider credentials and
    // domain allowlists are execution-time readiness, never registration gates.
    tool_arcs.push(Arc::new(HttpRequestTool::new(
        security.clone(),
        http_config.allowed_domains.clone(),
        http_config.max_response_size,
        http_config.timeout_secs,
    )));

    tool_arcs.push(Arc::new(WebSearchTool::new(
        root_config.web_search.provider.clone(),
        root_config.web_search.brave_api_key.clone(),
        root_config.web_search.max_results,
        root_config.web_search.timeout_secs,
    )));
    tool_arcs.push(Arc::new(WebFetchTool::new(
        security.clone(),
        browser_config.allowed_domains.clone(),
        root_config.web_search.fetch_max_chars,
        root_config.web_search.timeout_secs,
    )));

    // Always register agents_list (shows what agents are available for delegation)
    if !agents.is_empty() {
        tool_arcs.push(Arc::new(AgentsListTool::new(
            agents.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        )));
    }

    // Add delegation tool when agents are configured
    if !agents.is_empty() {
        let delegate_agents: HashMap<String, DelegateAgentConfig> =
            agents.iter().map(|(name, cfg)| (name.clone(), cfg.clone())).collect();
        let delegate_fallback_credential = fallback_api_key.and_then(|value| {
            let trimmed_value = value.trim();
            (!trimmed_value.is_empty()).then(|| trimmed_value.to_owned())
        });
        let parent_tools = Arc::new(tool_arcs.clone());
        let delegate_tool = DelegateTool::new_with_options(
            delegate_agents,
            delegate_fallback_credential,
            security.clone(),
            crate::providers::provider_runtime_options_from_config(root_config),
        )
        .with_parent_tools(parent_tools)
        .with_multimodal_config(root_config.multimodal.clone())
        .with_compaction_resolver(crate::router::CompactionResolver::new(
            root_config.agent.compaction.clone(),
            root_config.router.clone(),
            root_config.model_routes.clone(),
        ))
        .with_shared_memory(Arc::clone(&memory))
        .with_event_recording(root_config.memory.event_recording_config());
        tool_arcs.push(Arc::new(delegate_tool));
    }

    ToolsRegistryResult {
        tools: boxed_registry_from_arcs(tool_arcs),
        mcp_tool: mcp_tool_ref,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
    use super::*;
    use crate::config::{BrowserConfig, Config, MemoryConfig};
    use tempfile::TempDir;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    #[test]
    fn filter_tool_specs_for_exposure_removes_stay_silent_when_not_exposed() {
        let mut specs = vec![spec("shell"), spec(STAY_SILENT_TOOL_NAME), spec("file_read")];
        filter_tool_specs_for_exposure(&mut specs, false);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["shell", "file_read"]);
        assert!(!names.contains(&STAY_SILENT_TOOL_NAME));
    }

    #[test]
    fn filter_tool_specs_for_exposure_keeps_stay_silent_when_exposed() {
        let mut specs = vec![spec("shell"), spec(STAY_SILENT_TOOL_NAME)];
        filter_tool_specs_for_exposure(&mut specs, true);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&STAY_SILENT_TOOL_NAME));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn tool_name_is_exposed_gates_only_stay_silent() {
        // Non-stay_silent tools are always exposed regardless of the flag.
        assert!(tool_name_is_exposed("shell", false));
        assert!(tool_name_is_exposed("shell", true));
        // stay_silent is gated on the flag.
        assert!(!tool_name_is_exposed(STAY_SILENT_TOOL_NAME, false));
        assert!(tool_name_is_exposed(STAY_SILENT_TOOL_NAME, true));
    }

    fn test_config(tmp: &TempDir) -> Config {
        Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        }
    }

    #[test]
    fn default_tools_has_expected_count() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        assert_eq!(tools.len(), 4);
    }

    #[test]
    fn all_tools_excludes_browser_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(crate::memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig {
            allowed_domains: vec!["example.com".into()],
            ..BrowserConfig::default()
        };
        let http = crate::config::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            mem,
            None,
            None,
            &browser,
            &http,
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"browser_open"));
        assert!(names.contains(&"cron"));
        // The seven legacy single-purpose scheduler tools are fully consolidated
        // into `cron` and must no longer be registered.
        for removed in [
            "schedule",
            "cron_add",
            "cron_list",
            "cron_remove",
            "cron_run",
            "cron_runs",
            "cron_update",
        ] {
            assert!(
                !names.contains(&removed),
                "removed tool `{removed}` is still registered"
            );
        }
        assert!(names.contains(&"pushover"));
        assert!(names.contains(&"proxy_config"));
        assert!(
            names.contains(&"mcp_call"),
            "MCP must be registered even with zero servers"
        );
        assert!(names.contains(&"http_request"));
        assert!(names.contains(&"web_search_tool"));
        assert!(
            names.contains(&"web_fetch"),
            "web_fetch registration must not depend on its allowlist"
        );
    }

    #[test]
    fn all_tools_never_registers_removed_shell_shim_tools() {
        // The five thin-shim tools (canvas/screenshot/browser/tts/browser_open)
        // were removed in favour of the agent driving shell directly. They must
        // never be registered, even with browser support enabled in config.
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(crate::memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig {
            allowed_domains: vec!["example.com".into()],
            ..BrowserConfig::default()
        };
        let http = crate::config::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            mem,
            None,
            None,
            &browser,
            &http,
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        for removed in ["canvas", "screenshot", "browser", "tts", "browser_open"] {
            assert!(
                !names.contains(&removed),
                "removed shell-shim tool `{removed}` is still registered"
            );
        }
        // The surviving native tools are unaffected by the removal.
        assert!(names.contains(&"image_info"));
        assert!(names.contains(&"pushover"));
        assert!(names.contains(&"proxy_config"));
    }

    #[test]
    fn all_tools_skips_memory_recall_when_acl_enabled() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            acl_enabled: true,
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(crate::memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig::default();
        let http = crate::config::HttpRequestConfig::default();
        let cfg = Config {
            memory: mem_cfg.clone(),
            ..test_config(&tmp)
        };

        let tools = all_tools(
            Arc::new(cfg.clone()),
            &security,
            mem,
            None,
            None,
            &browser,
            &http,
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"memory_recall"));
        assert!(names.contains(&"document_search"));
        assert!(names.contains(&"document_get_chunk"));
        assert!(names.contains(&"memory_reindex"));
    }

    #[test]
    fn default_tools_names() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"file_read"));
        assert!(names.contains(&"file_write"));
        assert!(names.contains(&"file_edit"));
    }

    #[test]
    fn default_tools_all_have_descriptions() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        for tool in &tools {
            assert!(
                !tool.description().is_empty(),
                "Tool {} has empty description",
                tool.name()
            );
        }
    }

    #[test]
    fn default_tools_all_have_schemas() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        for tool in &tools {
            let schema = tool.parameters_schema();
            assert!(schema.is_object(), "Tool {} schema is not an object", tool.name());
            assert!(
                schema["properties"].is_object(),
                "Tool {} schema has no properties",
                tool.name()
            );
        }
    }

    #[test]
    fn tool_spec_generation() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        for tool in &tools {
            let spec = tool.spec();
            assert_eq!(spec.name, tool.name());
            assert_eq!(spec.description, tool.description());
            assert!(spec.parameters.is_object());
        }
    }

    #[test]
    fn tool_result_serde() {
        let result = ToolResult {
            success: true,
            output: "hello".into(),
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.output, "hello");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn tool_result_with_error_serde() {
        let result = ToolResult {
            success: false,
            output: String::new(),
            error: Some("boom".into()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json).unwrap();
        assert!(!parsed.success);
        assert_eq!(parsed.error.as_deref(), Some("boom"));
    }

    #[test]
    fn tool_spec_serde() {
        let spec = ToolSpec {
            name: "test".into(),
            description: "A test tool".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: ToolSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test");
        assert_eq!(parsed.description, "A test tool");
    }

    #[test]
    fn all_tools_includes_delegate_when_agents_configured() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(crate::memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig::default();
        let http = crate::config::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let mut agents = HashMap::new();
        agents.insert(
            "researcher".to_string(),
            DelegateAgentConfig {
                provider: "ollama".to_string(),
                model: "llama3".to_string(),
                system_prompt: None,
                api_key: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                max_iterations: 10,
                identity_dir: None,
                memory_scope: None,
                spawn_enabled: None,
            },
        );

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            mem,
            None,
            None,
            &browser,
            &http,
            tmp.path(),
            &agents,
            Some("delegate-test-credential"),
            &cfg,
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"delegate"));
    }

    #[test]
    fn all_tools_excludes_delegate_when_no_agents() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(crate::memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig::default();
        let http = crate::config::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            mem,
            None,
            None,
            &browser,
            &http,
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"delegate"));
    }
}
